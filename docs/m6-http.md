# m6-http

Reverse proxy, cache, and router.
The only process in an m6 deployment that listens on a public port, unless `[server] reuse_port` asks several of them to share it.
It does not manage other processes, which is systemd's job.

The sections run from the process's surface to its protocols.
§1 to §13 cover the CLI, startup, pools, routing, forwarding, limits, caching, the generation ID, errors, auth, monitoring, signals and exit codes.
§14 to §16 cover H2C, H2S and the HTTP/2 denial-of-service bounds.
§17 to §19 cover the event loop, logging and dependencies.

## Contents

1. [CLI](#1-cli)
2. [Startup Sequence](#2-startup-sequence)
3. [Backend Pools](#3-backend-pools)
4. [Routing](#4-routing)
5. [Request Forwarding](#5-request-forwarding)
6. [Request Header Limits](#6-request-header-limits)
7. [Caching](#7-caching)
8. [Site generation ID](#8-site-generation-id)
9. [Error Handling](#9-error-handling)
10. [Auth Enforcement](#10-auth-enforcement)
11. [Monitoring Endpoints](#11-monitoring-endpoints)
12. [Signal Handling](#12-signal-handling)
13. [Exit Codes](#13-exit-codes)
14. [H2C (HTTP/2 Cleartext)](#14-h2c-http2-cleartext)
15. [H2S (HTTP/2 over TLS)](#15-h2s-http2-over-tls)
16. [HTTP/2 Denial of Service](#16-http2-denial-of-service)
17. [Event Loop](#17-event-loop)
18. [Logging](#18-logging)
19. [Dependencies](#19-dependencies)

---

## 1. CLI

```
m6-http <site-dir> <system-config>
```

Both arguments are required.
Development uses a minimal local system config (e.g. `configs/system.toml`).
Production uses `/etc/m6/my-blog.toml`.
The system config holds the node's own state: the `[server]` node keys (bind address and TLS paths), `[node]`, `[auth]`, the `[health]` access keys, `[genid]` and `[cache]`.
Everything else comes from `site.toml`.
In one-file mode the same file serves as both, and every key is read from it.

Optional flags:

```
--log-level debug    # override log level
--dump-config        # print effective merged config to stdout, then exit 0
```

m6-http logs to stdout as structured JSON.

---

## 2. Startup Sequence

m6-http starts in this order, before it serves a request.

1. Load `site.toml` from the site directory.
2. Load `<system-config>`.
   It carries `[node]`, the `[server]` node keys, `[auth]`, the `[health]` access keys, `[genid]` and `[cache]`, and nothing else.
   A key in the wrong file is exit 2.
3. Validate the merged config, exiting 2 on error before binding.
4. If `[auth]` is declared, load the public key for local JWT verification.
   `backend` is optional.
5. Read the generation state.
   A source with `[genid]` reads `$STATE_DIRECTORY/genid`, and a cache node holds no file.
6. Build the route table: insert each `[[route]]`, then expand each `[[route_group]]` glob against the site directory and add one route per matched file.
7. Resolve every URL backend host once.
   A name that does not resolve is exit 2.
8. Bind the TLS and QUIC sockets on `[server].bind`.
   If `h2c_bind` is set, bind the H2C (plain-TCP HTTP/2) listener on that address.
   The listener is internal when `internal_binds` names it, and then accepts only the peers in `internal_peers`.
9. Start the event loop.

With `[server] reuse_port`, only the public 443 port is shared: its TLS listener and its QUIC UDP socket are bound together through `m6_core::platform::net::listen_shared`, before the TLS configuration is built.
The redirect listener on port 80, the internal h2c listener and every backend socket are not shared.
`/health`, `/perf` and `/traffic` are answered on the shared port, so the process the kernel picks answers them.

---

## 3. Backend Pools

Each `[[backend]]` with `sockets` is a pool.
m6-http finds pool members once at start by expanding the socket glob.

**Pool management:**
- Members are the sockets the glob matches when m6-http starts.
- A pool whose glob matches no socket at start exits 2.
- A request that fails to connect tries the next member, and the client receives 502 only when every member has failed.
- A backend that stops and restarts on the same socket path serves again on its next turn.

**Load balancing:** plain round robin.
Each request takes the next member in turn, so `N * k` requests put exactly `k` on each of `N` members.

**URL backends** (`http://`, `https://`) are not a member pool.
Each request takes one HTTP/1.1 connection, kept open for later requests in a per-backend idle set of up to eight, driven by the event loop through a `client::Upstream` slot with no dedicated I/O thread.
The host name is resolved once at start, so an address change needs a restart.

**H2C backends** use the `h2c://` scheme (e.g. `url = "h2c://origin.example.com:8080"`).
The connection is maintained persistently by an event-loop-driven `H2cClientPool`, and no thread is spawned.
It is intended for inter-node requests over a private backbone, where TLS at the application layer is redundant.

**H2S backends** use the `h2s://` scheme (e.g. `url = "h2s://api.example.com:443"`).
It is H2C over TLS, and ALPN negotiates `h2` during the handshake.
The connection is maintained persistently by an event-loop-driven `H2sTlsClientPool`.
It is intended for HTTP/2 forwarding to external TLS-terminating upstreams.

A backend that answers before it has read the whole request and then refuses the write has that answer forwarded to the client, and its connection is never reused.
A request that was written in full and answered early is a different case: the connection can still carry another request if the reply's framing says it can, so it rejoins the idle set like any other.

---

## 4. Routing

m6-http decides a request's path through a fixed order, and each step can answer before the next one runs.

1. **Rate limit:** a request over the per-IP limit is answered 429 before the cache, the redirect, routing or any backend.
2. **Method check:** a route that names `methods` answers only those, and another method is refused 405 with `Allow` before the cache is looked up or a backend is contacted.
3. **www redirect:** with `[site] redirect_www` set (the default), a `www.` host is answered 301 on the apex host.
4. **Cache lookup:** a route with no `require` and a method that may read the cache is served from memory on a fresh hit, with no backend contact.
5. **Route lookup:** a path that matches no route is answered 404 per `[errors] mode`.
6. **Auth check:** when the route declares `require`, the JWT is verified locally, with 401 when it is absent and 403 when the claims are insufficient.
7. **Forward:** the request is forwarded to the backend pool.

Route matching: a fixed route (every segment a literal) beats a pattern route (any `{param}` or `{*rest}`).
Two fixed routes on one path, and two patterns that can both match some path, are a configuration error at load, and `--dump-config` fails the same way (see `m6-site-toml.md`, Route Matching).

### 4.1 The per-IP limit

`[rate_limit]` in `site.toml` sets `enabled` (default true) and `requests_per_min` (default 300), and the limit uses a fixed 60-second window.
A request over the limit is answered 429 with `Retry-After: 60` and the body "Too Many Requests", before the cache lookup and any backend work.
The counter is a map in one process's memory, keyed by the client address.
A process restart clears the count.
The limit applies on HTTP/1.1, HTTP/2 and HTTP/3 alike, and `m6-site-toml.md` §4.13 owns the keys.

One process on the public port enforces `requests_per_min` for the node.
With `[server] reuse_port`, N processes on one port each keep a count, so one client can reach up to N times `requests_per_min` across the port: three processes at the default allow 900 requests in a minute.
An operator who needs the node to enforce one number sets `requests_per_min` to the node total divided by the process count, or enforces the limit at the front door.
`m6-site-toml.md` §4.13 records a Linux socket-filter design that would make the per-process count exact, as an idea for a later batch.

The steps after the limit decide which route and backend serve a request, and the next section states exactly what that backend receives.

A `[[route]]` or `[[route_group]]` may name the methods it answers in `methods`.
A request with another method is refused at the edge with 405 and `Allow` naming the ones the route does answer, before the body, the cache or a backend (RFC 9110 15.5.6 and 10.2.1).
`GET` also answers `HEAD` (RFC 9110 9.3.2).
`OPTIONS` is answered at the edge the same way and never forwarded (RFC 9110 9.3.7).
An unknown or empty `methods` list stops the start with exit 2.
`m6-site-toml.md` §4.8 and §4.9 own the key.

A `[[route]]` or `[[route_group]]` may name `cache`, a `Cache-Control` value that replaces the backend's own on every reply the route gets.
It is applied before the reply is stored, so `no-store` keeps it out of the cache, and the stored copy and the client's reply carry the same one field (RFC 9111 5.2).

---

## 5. Request Forwarding

m6-http forwards the complete request to the chosen backend: method, path, query string, all headers and body.
A URL backend receives its own host in `Host`, and a socket backend receives the client's.

**Added:** `X-Forwarded-For`, `X-Forwarded-Proto: https`, `X-Forwarded-Host` and `Via`, which appends this hop to a value already present.

**Removed:** every hop-by-hop field (`Connection`, `Upgrade`, `Keep-Alive`, `Transfer-Encoding`, `TE`, `Trailer`, `Proxy-Authorization`, `Proxy-Connection`), every field the client's `Connection` header names (RFC 9110 7.6.1), and each proxy-owned header (`Content-Length`, `X-Forwarded-For`, `X-Forwarded-Proto`, `X-Forwarded-Host`, `X-Real-IP`), which m6-http writes itself.

**Response fields:** HTTP/2 and HTTP/3 write every response field name in lower case (RFC 9113 8.2.1, RFC 9114 4.2).
Both drop the connection-specific set `Connection`, `Keep-Alive`, `Proxy-Connection`, `Transfer-Encoding` and `Upgrade`, and HTTP/2 writes its own `Content-Length`.
HTTP/1.1 keeps the fields that describe its own connection, and it writes `Connection` itself.
m6-html closes its connection after each page and sends `Connection: close`, and a client enforcing RFC 9114 4.2 reset an HTTP/3 stream that carried the field and read no body.

---

## 6. Request Header Limits

A request head is bounded before routing, the cache lookup and every backend, so an over-limit request is answered by the edge and no backend sees it.
Two limits apply, and both follow the numbers the mainstream servers use: nginx's `large_client_header_buffers 4 8k`, Varnish's `http_req_hdr_len 8k` and `http_req_size 32k`, and Apache's `LimitRequestFieldSize 8190`.
`MAX_HEADER_LINE` is 8 KiB for one field, counted as its name plus its value.
`MAX_REQUEST_HEAD` is 32 KiB for the request line plus every field.
HTTP/2 and HTTP/3 count a decoded head as RFC 9113 6.5.2 defines it for `SETTINGS_MAX_HEADER_LIST_SIZE`: name plus value plus 32 bytes per field.

A field over `MAX_HEADER_LINE`, or a head over `MAX_REQUEST_HEAD`, is answered 400 with the error page titled "Request Header Or Cookie Too Large", which is nginx's own text for this refusal.
A request line over `MAX_HEADER_LINE` is answered 414 Request-URI Too Large, counting the method, target and version on HTTP/1.1 and `:method` plus `:path` on HTTP/2 and HTTP/3.
Both limits are defined once in m6-core as `m6_core::headers::MAX_HEADER_LINE` and `m6_core::headers::MAX_REQUEST_HEAD`, and the HTTP/1.1 parser, both backends and the edge read those definitions.

HTTP/2 advertises `MAX_REQUEST_HEAD` in `SETTINGS_MAX_HEADER_LIST_SIZE` and enforces it while decoding.
The 256 KiB bound on the compressed header block stays as the default of `[limits.h2_header_block] max`, because it is the defence against HPACK expansion.
HTTP/3 advertises `MAX_REQUEST_HEAD` in `SETTINGS_MAX_FIELD_SECTION_SIZE`, and the pinned quiche enforces it by closing the connection with H3_EXCESSIVE_LOAD.
A single field over `MAX_HEADER_LINE` and a request line over it are answered on the HTTP/3 stream, as they are on the other two protocols.
The HTTP/1.1 head limit is separate from the cap on a whole request, which is `[limits.edge_request] max` and defaults to 32 MiB.

---

## 7. Caching

m6-http caches a backend response when the response permits a shared cache to store it and its status is one RFC 9111 3 makes storable.
`Cache-Control: public` is stored under the `(path, query, content-encoding)` key, while `no-store` and `private` are not stored.
Each encoding variant is cached independently on demand, with no eager pre-fetching.
A response whose `Vary` names any field other than `Accept-Encoding` is not stored, because the key cannot express it.

### 7.1 The browser-caching pattern

A page and an unversioned file are read through two caches with different lifetimes, so one `Cache-Control` field names both.
web.dev's HTTP caching guide and MDN's `Cache-Control` reference describe the same split: the browser revalidates, and the shared cache keeps its copy.
A versioned asset is one exact version, so it is pinned instead.
These are the three headers m6 emits, and the directives that decide each behaviour.

- A page, from m6-core: `Cache-Control: public, max-age=0, must-revalidate, s-maxage=86400`, with an `ETag`.
- An unversioned file, from m6-file: the same header, with an `ETag`.
- A versioned asset, `?v=<hash>`: `Cache-Control: public, max-age=31536000, immutable`, with an `ETag`.

`max-age=0` and `must-revalidate` make the browser's copy stale at once and forbid serving it without validation (RFC 9111 sections 5.2.2.1 and 5.2.2.2).
The browser therefore sends `If-None-Match` on every use, and an unchanged response costs one 304.
`s-maxage=86400` is honoured by shared caches only and overrides `max-age` for them (RFC 9111 section 5.2.2.10), so m6-http keeps the response for a day and answers that conditional request itself (RFC 9111 section 4.3.2) without contacting the backend.
`public` makes the response storable by a shared cache.

`no-cache` is absent because it would also force the shared cache to revalidate, the round trip `s-maxage` removes (RFC 9111 section 5.2.2.4).
m6 does not emit `stale-while-revalidate` in these headers, because it would let the browser serve a stale copy while it revalidates.
An upstream that sends `stale-while-revalidate` is still honoured: the cache serves stale inside the window the directive names and refreshes behind the request (RFC 9111 section 4.2.4).
`immutable` appears only on the versioned asset, where changed bytes mean a different URL, and it stops the browser revalidating that URL on reload.

### 7.2 Invalidation

An entry leaves the cache in one of four ways.

1. A successful state-changing request evicts the entry for its path, and for a same-origin `Location` and `Content-Location` (RFC 9111 4.4).
2. The entry's freshness lifetime ends.
   A response with no explicit `max-age` lives at most 24 hours, the heuristic ceiling.
3. A generation change empties the cache.
   The source swaps it in step 3 of a `POST /genid`, and a cache node flushes it when the upstream's ID changes.
4. The process restarts, because the cache is in memory.

A file change behind a page therefore does not reach the cache until one of those happens, and a publish through `/genid` makes it happen at once.
m6-http keeps no map from data files to the URL paths that depend on them, and it reads no renderer config at startup.

### 7.3 Which statuses are stored

Storability starts from the statuses RFC 9111 3 lists, and Table 1 gives the set this cache accepts.
A status outside Table 1 is answered to the client and not stored, whatever its `Cache-Control` says.
m6-http never stores a 206, because the key carries no `Range` component and a partial representation would be replayed as a whole one.

| Status | Stored when public |
|---|---|
| `200`, `203`, `204` | Yes |
| `300`, `301`, `308` | Yes |
| `404`, `405`, `410`, `414` | Yes |
| `501` | Yes |
| `206` | Never |
| Every other 4xx, and every 5xx | Never |

*Table 1: the statuses the cache may store, from RFC 9111 3. Take from it that a public 404 is stored, a 206 is never stored, and no 5xx is stored whatever its headers say.*

Table 1 also decides what reaches the disk cache, because only a stored response is written there.

---

## 8. Site generation ID

A source node holds a site generation ID.
The ID is a `u64` counter that names the content the backends are serving at one moment.
m6-http owns it, backend services never see it, and only the source's `[genid]` table names it.
A node is a source because its system config carries a `[genid]` table.
A node with no `[genid]` has no endpoint at `/genid` and answers that path as it answers any other unknown path.

The endpoint answers three methods, as Table 2 lists.

| Method | Claim | Answer |
|---|---|---|
| `GET` | `[genid] read_require` | `200` with `{"genid":"<decimal>"}` and `M6-Genid: <decimal>` |
| `HEAD` | `[genid] read_require` | The `GET` status and headers, with no body |
| `POST` | `[genid] write_require` | Runs the change sequence, then `200` with the same shape for the new ID |

*Table 2: the methods `/genid` answers on a source. Take from it that `GET` and `HEAD` need the read claim, `POST` needs the write claim, and each 200 answer carries the ID in `M6-Genid`.*

All three methods in Table 2 require a loopback peer or an internal listener, checked before any token is read.
Anyone else gets the 404 an unknown path gets.
Both claims absent is also a 404, so the endpoint exists only when it can answer.
The `[genid]` keys are system config only.
A source needs the unit's `StateDirectory=`, and a cache node does not.

### 8.1 The change sequence

A `POST /genid` runs four steps in the source's loop.

1. Send `POST /genid` with an empty body to every member of every socket pool, on that member's own socket, outside the pool's member choice.
   Each member gets its own upstream slot and the `backend_timeout_secs` deadline.
2. Wait for every member.
   A 204 is done.
   A 404 or 405 is a backend with nothing to refresh and is logged at debug.
   Any other status, a connect failure or the deadline is logged at error with the member's socket path and the reason.
   A failed member does not stop the sequence, and it does not count as refreshed.
3. In one step, rebuild the route table from the config loaded at start, swap the cache and the held error pages for empty ones, advance the ID and write it.
   Each swap is one assignment, so a request sees the old generation or the new one, and a request already holding an entry finishes on the content it took.
   A route-table build error keeps the old table and is logged.
   The route table is built from `site.toml` as loaded at start, so a publish that adds a file a `[[route_group]]` glob matches gains its route, and a `site.toml` edit needs a restart.
   m6-http keeps no data-file-to-path invalidation map, so the cache swap is the whole invalidation.
4. Answer every covered POST with the new ID.
   A state write failure answers those POSTs 500 and is logged with the path.

The order matters.
The ID must not change before every backend has the new content, because a cache could otherwise refill from a backend still serving the old bytes under the new number.
A POST that arrives while a sequence runs is parked, and the sequence that answers it started after it arrived.

The ID advances only in step 3 of a POST, never at start.
It lives in `$STATE_DIRECTORY/genid` as the decimal ID and a newline, written to `genid.tmp` and renamed over the file before the POST is answered.
A restart reads that file, so a generation survives a restart.
A source with `[genid]` and no `STATE_DIRECTORY`, or an existing state file that cannot be read or does not hold a `u64`, stops the start with exit 2.
A missing state file starts the ID at 0, so a first start needs no file.
`--dump-config` prints and exits 0 without one, because a dump is not about to serve.

A publisher that is itself a backend of the source drives this POST from its own event loop, never as a blocking call, so its loop stays free to answer the source's refresh.

### 8.2 The ID on content

Every response the source receives from a backend carries the ID that was current when the request was dispatched, written as `M6-Genid: <decimal>` when the response head is serialised.
HTTP/2 and HTTP/3 write the name in lower case, which RFC 9113 8.2.1 and RFC 9114 4.2 require.
A cached response carries the ID stored with it.
A response whose dispatch ID is no longer current is not stored, so the cache cannot refill from a backend that is one generation behind.
A locally built answer, such as a health check or an error page, carries no ID.

The generation is also reported by an authorised `/perf`, under a `genid` object with `role`, `id`, `sequence_running` and the peers that read it.
An operator watching a publish sees one `genid advanced` line at info with `genid`, `members`, `failed` and `elapsed_ms`, and one `genid: member refresh failed` line at error naming the socket path of each member that did not refresh.
The POST still answers 200 when a member failed, and the `failed` count is what says the generation is not uniform.

### 8.3 A cache node follows the source

A node whose `[genid]` sets `upstream` is a cache node.
It names a `[[backend]]` with a `url`, holds no state file, and needs no `StateDirectory=`.
It asks that upstream for the ID and follows it.

1. **Heartbeat.**
   At start, and then once `heartbeat_secs` (default 10) have passed since the last one finished, the node sends `GET /genid` to the upstream with `Authorization: Bearer <token from token_file>`.
   One heartbeat is in flight at most, on the accounting tick, under the backend's `backend_timeout_secs` deadline.
   The address is the one resolved at start, so an address change needs a restart.
2. **A new ID is a full flush.**
   The node rebuilds the route table from the config loaded at start, swaps the cache and the held error pages for empty ones in one assignment each, and logs `genid changed` at info with `from` and `to`.
   No entry survives into the new generation.
   One assignment publishes each replacement, so a request sees the old generation or the new one and never a half-emptied cache, and a request already holding an entry finishes on the content it took.
   The cache refills on demand.
   A first answer sets the ID with no flush, because the node held nothing the new ID makes old.
   That is the whole invalidation, and it keeps the cache and the ID in step without a per-path map.
3. **The store rule.**
   A response from any URL backend enters the cache only when its `M6-Genid` parses and equals the ID the node holds, and the entry carries that ID.
   A response with no header, no current ID, or another ID is sent to the client unchanged and not stored.
   Another ID also makes the next heartbeat due at once, so a node that learns of a publish from a response follows it without waiting out the interval.
   On a cache node the ID reaches a client from the upstream's own header on a miss, and from the cached entry on a hit.
4. **Serving while the upstream is down.**
   While the upstream does not answer, an entry past its freshness and stale windows is served as `STALE` with its `Age`.
   A forward that ends in 502, 503 or 504 answers with the held entry for its key when there is one, at any age.
   The operator sees one `genid: upstream down` line at warn on the transition, and `/perf` reports `upstream_down: true`.
   The first answer after an outage logs `genid: upstream up` at info and clears the flag.
   Old pages keep working while the source is unreachable, and that is better than an error page.
5. **Background fetches wait.**
   Prefetch, warm-on-start and stale-while-revalidate fetches are not drained while the node has no ID or the upstream is down.
   The queue holds up to `[limits.refresh_queue] max`, 4096 by default, and drains once the upstream answers. A refresh past the maximum is refused with an error that names the key.
6. **`/genid` is forwarded.**
   `GET`, `HEAD` and `POST /genid` reach the upstream backend after the loopback or internal-listener address check, with the request's own headers, bypassing routing and the cache.
   The cache node checks no token: the source authorises the caller.
   A forwarded POST waits under `backend_timeout_secs` and is answered 504 if the deadline passes while the source is still completing the publish.
   `GET /genid` then shows whether the advance happened.

A cache node reports its own `genid` object from an authorised `/perf`: `role` is `cache`, `id` is the ID it holds or `null`, and `upstream`, `upstream_down`, `upstream_rtt_ms` and `last_ok_s` describe the node it follows.
A source restart keeps its ID, so a cache node sees no change when it comes back.

### 8.4 Disk cache (cache nodes)

A cache node with `[cache] dir` in its system config writes the responses it caches to disk, and the request path never touches a file.
The table is node state and belongs in the system config.
A source keeps its cache in memory, because it sits beside its backends and a cold cache costs one local render per page.
A source that sets it is refused at start with `config error: [cache].dir is for a cache node ([genid] upstream)`.

The layout is `<dir>/<genid decimal>/<64 hex characters>`, the file name being the SHA-256 of the cache key's bytes.
One subdirectory per generation means a change needs no per-file rewrite, and every other subdirectory is removed over the slices that follow.

A stored entry is marked with a `queued` flag in the same insert that stores it, and the insert writes nothing to disk.
The flag is the dedup, so a second store before the slice adds nothing.
No key is copied and no queue entry is pushed, so the store allocates nothing for the writer.
On a slice the writer scans the cache for flagged entries, and an entry the budget cannot reach keeps its flag for the next slice.

The work rides idle turns: while the writer has a flagged entry or an old directory to remove, the loop polls with a zero timeout and runs one slice per turn, and it goes back to its normal wait when the queue is empty.
A request that arrives on a descriptor waits for at most one slice, and a slice stops after 1 ms of loop time, checked between files, so a full queue costs the loop about a millisecond and the rest waits for later turns.
The 4 MiB write bound and the 512-file removal bound remain upper bounds, and whichever limit is reached first ends the slice.
Removals run first, so their bytes are available to the write bound.
A slice that runs out of budget with more to write leaves the remaining keys flagged, and the next slice continues with them.
A single entry larger than 4 MiB is written on its own, because a file is never split.
The writer reads each entry again at the slice, so an entry replaced since it was queued is written as it is now, and an entry evicted since is skipped.

Only public cacheable responses enter the cache, so only public cacheable responses reach the disk.
A file holds the generation ID, the key, the status and headers as JSON, the body, and a SHA-256 check value over every byte before it.
The writer encodes into one buffer reserved at start, writes `<name>.tmp` with one write, and renames it into place.
There is no `fsync`.
A crash can leave a short or torn file, and the check value catches it.

The disk copy is bounded by the memory cache's size, `[limits.cache_bytes] max`, 128 MiB by default.
A write that would pass the bound is skipped and logged once per generation at info.
A write failure is logged once per episode with the path and the error, and the entry is dropped without a retry.
A failed write never fails a request, because no request waits for one.

On a generation change the queue and the size map are cleared, the new subdirectory is used from the next write, and every other subdirectory is queued for removal.
Removal takes up to 512 files per slice, or fewer when the 1 ms time budget runs out first, then the empty directory.
While the node has no current ID nothing is written, because nothing is stored in memory then either.

A restart reads the disk copy at start, before the loop serves, and never on the request path.
The node lists `<dir>` and reads the subdirectory with the largest decimal name, because the source's IDs only grow.
That generation's files are decoded one at a time in file-name order.
Each entry is inserted into a fresh cache bounded like the live one, so a disk copy larger than the memory bound fills the cache with the entries that fit first and leaves the rest on disk under their keys.
The writer counts the files that were left, so its byte bound covers the whole directory a restart inherits.
Every other generation's directory is queued for removal over the slices that follow.

A file that is short, fails its check value or carries another generation is deleted and counted, with one log line naming the path and the error.
A `.tmp` file is an interrupted write and is deleted without being counted.
A damaged file never stops the start and is never served.

The decoded entries are held aside until the first heartbeat answers.
The ID decides, by the same equality the store rule uses.
A matching ID installs the copy as the live cache, so the node starts warm.
Another ID drops it, because the site changed while the node was down, and the old directory is removed over the slices that follow.
When the upstream does not answer, the copy is installed marked unverified, because a site with pages from before the restart serves visitors better than a site with none.
The first later success clears the mark, keeps the copy when the ID is unchanged and runs the usual flush when it has moved.

The start cost is one directory listing, one read and one SHA-256 per file, up to `max_bytes`.
The `disk cache loaded` line reports `elapsed_ms` beside the ID, the entry count (`entries`) and the damaged-file count (`bad`).

An authorised `/perf` on a node with a directory adds a `disk_cache` object: `dir_bytes`, `queued`, `written_last_tick`, `written_bytes_last_tick`, `skipped_full`, `ticks`, `tick_last_ns`, `loaded_entries`, `loaded_bad` and `state`.
`dir_bytes` counts the files in the current generation's directory.
`queued` counts the entries whose flag is set.
`written_bytes_last_tick` is the encoded size of the last slice's writes, including a single entry larger than the 4 MiB budget.
`ticks` counts the slices taken since the node started.
`tick_last_ns` is the loop time the last slice took, measured from the platform clock.
`loaded_entries` counts the entries the start-time load inserted and `loaded_bad` the files it deleted.
`state` is `kept`, `discarded`, `unverified` or `none`, and it is `none` until the first heartbeat answers.

---

## 9. Error Handling

m6-http answers an error in one of three modes, and the mode decides the body while the status stays the one the failure earned.

### 9.1 Error flow

When a backend returns a non-2xx response, or no backend instance is available:

1. m6-http checks `[errors] mode`.
2. If `mode = "custom"`, m6-http makes a GET request to `<[errors] path>?status=<N>&from=<original-path>`.
3. The backend renders an error page using the `status` and `from` query params, and returns 200.
4. m6-http returns that body to the client with the **original** status code.
5. If the fetch fails, or if the failing request was already to the error path (no recursion), m6-http falls back to `"internal"` mode.

Table 3 gives the three modes.

| Mode | Response |
|---|---|
| `"status"` | Status code, empty body |
| `"internal"` | Status code, m6-http-generated minimal HTML |
| `"custom"` | Status code, error page fetched from `[errors] path` |

*Table 3: the three `[errors] mode` values and the response each produces. Take from it that `custom` fetches a page and `internal` generates one.*

`[errors] path` is required when `mode = "custom"`.
Startup validation exits 2 if mode is `"custom"` and path is absent.
The fallback in Table 3 keeps a failed custom fetch from answering with no body.

Error responses are never cached.
The error path is declared as a route in `site.toml` like any other.
m6-http has no static fallback file.

---

## 10. Auth Enforcement

If a route declares `require`, m6-http enforces it before forwarding:

1. Extract the JWT from the `Authorization: Bearer <token>` header, or if absent from the `session` cookie.
   The header takes precedence: API clients use it, and browsers use the cookie.
2. Verify the signature locally using m6-auth's public key, with no network call.
3. Check token expiry.
4. Check claims against the `require` declaration (`group:<n>` or `role:<n>`).
5. Reject with 401 (no token or invalid token) or 403 (insufficient claims).

One function, `auth::check`, does steps 1 to 4 for route `require`, `/perf`, `/traffic` and `/genid`, once per request, so all four accept the same tokens and refuse the same ways.
It decides `Allow(claims)`, `NoCredential` (401) or `Deny` (403). `m6_core::http::token_refusal` is the one definition of the refusal, and every route answers it unchanged for an API client: 401 `{"error":"unauthorised"}` with `WWW-Authenticate: Bearer` (RFC 9110 section 11.6.1), 403 `{"error":"forbidden"}` without it.
A missing `[auth] public_key` fails closed as 401. Config validation refuses `require` without `[auth]`, so a running node does not reach that state.

Valid requests are forwarded with the verified token intact.
Renderers may perform additional fine-grained checks using the auth claims available in the request dictionary.

**401 handling:** A browser (`Accept: text/html`) sent to a protected route with no credential, an invalid one or an expired one is redirected: to `/auth/refresh` when a `refresh` cookie is present, otherwise to `/login?next=<original-path>`. A browser whose credential verifies but lacks the role gets the site's own 403 error page, keeping the check's status. API clients (non-`text/html` Accept) receive the JSON refusal directly.

---

## 11. Monitoring Endpoints

m6-http answers `/health`, `/perf` and `/traffic` itself, ahead of routing, the cache and every backend.
A refusal never reaches routing or the cache, so a catch-all route can never answer `/perf` and a built-in path never enters the cache.
`/health` is public and always answers 200 with `{"status":"ok","node":"<name>"}`.
`/health` answers for the m6-http process alone: it reads no socket pool, makes no network round trip and says nothing about a URL backend.
On an edge that means `ok` says the process is alive and answering, and says nothing about the origin.
A warm cache keeps serving while the origin is unreachable.

`/perf` and `/traffic` are gated by the `[health]` keys in Table 4.
In two-file mode all four are node state and live in the system config.
In one-file mode they live in that one file.

Each of the three endpoints reports the process that accepted the request.
With `[server] reuse_port`, the kernel chooses that process, so a monitor polling one address reads one front end of N per poll and the report holds that process's share of the node.
Section 4.1 states the same split for the request counters.

`/perf` reports three traffic populations and never pools them.
`http/<version>/external` is a real visitor from a public address on the TLS or QUIC listener, or a loopback client on either.
`http/<version>/internal` is a cache node's forward on the listener `[server] internal_binds` names.
`http/<version>/monitor` is a poll of `/health`, `/perf` or `/traffic`, or any request on the TLS or QUIC listener from a private network address other than loopback.
The backend name identifies the three endpoints and the peer address identifies the rest, so no header and no configuration key is involved.
Every monitor request also moves `monitor_requests_total`, and none of them moves `requests_total` or the hit and miss counters.

| Key | Where | Meaning |
|---|---|---|
| `[auth] public_key` | `site.toml` or system config, not both | Key that verifies API tokens. An absolute path |
| `[health] metrics_require` | either file, system config wins | `role:<name>` or `group:<name>`. A token with the claim is required, from the Authorization header or the session cookie, as route `require` reads it |
| `[health] metrics_peers` | either file, system config wins | IP addresses in a local range. Any other peer gets 404 |

*Table 4: the keys that decide who reads `/perf` and `/traffic`. Take from it that with none of them set the endpoints do not exist, and that a config that still names the removed `metrics_token_file` stops the start with exit 2.*

With none of the keys set, both endpoints answer the 404 an unconfigured path gives.
A config that still names `[health] metrics_token_file` stops the start with exit 2, naming the key, because the legacy gate is removed.
The checks run in this order, as Table 5 shows.

| Step | Check | Refusal |
|---|---|---|
| 1 | `metrics_peers` set: not on an internal h2c listener, and the peer address is listed | 404, the body of a disabled endpoint, before any token is read |
| 2 | `metrics_require` set: the one token check verifies the token (Authorization header or session cookie) and holds the claim | 401 for no or a bad token, 403 for the wrong claim. Same status, headers and body as every other token-gated route |
| 2 | `metrics_require` unset: the endpoint does not exist | 404, the body of a disabled endpoint, before any token is read |

*Table 5: the metrics gate, in order. Take from it that a peer outside `metrics_peers` learns nothing, and that a token is read only when `metrics_require` names the claim it must hold.*

Step 1 in Table 5 reads the peer address a completed TCP or QUIC handshake proved.
It is never read from a header, so a forged `X-Forwarded-For` does not open the endpoints.
An internal h2c listener believes `X-Forwarded-For`, so it never serves them.
The cost is one address parse and a scan of a short list, and only on these two paths.
The cache-hit path is unchanged.

---

## 12. Signal Handling

m6-http handles two signals, and Table 6 gives what each receipt does.
A second termination signal exits at once, and a repeated `SIGUSR1` only reopens the file again.

| Signal | First receipt | Second receipt |
|---|---|---|
| `SIGTERM` or `SIGINT` | Stop accepting, remove the socket if there is one, exit 0, without waiting for requests in progress | Immediate exit |
| `SIGUSR1` | Reopen the analytics log file | Same again, and never a step toward exit |

*Table 6: the signals m6-http handles. Take from it that `SIGUSR1` never moves the process toward exit, and that a second termination signal exits at once.*

Rotate the analytics file by renaming it and sending `SIGUSR1`.
The `SIGUSR1` row of Table 6 exists for this rotation.
The process creates the file at its path again, and the path reappearing means the renamed file is finished.
If the reopen fails, the process keeps writing to the previous file and logs an error.
`/perf` reports the last reopen under `logging.reopen`.
`/traffic` reports the last 60 minutes this process served, from a window it keeps in memory.
The window holds at most 32,768 events and is allocated at start, so a scrape reads no log file.
It starts empty after a restart, and `window_minutes` says how much of the hour it holds: the whole minutes since start, or less when the ring has wrapped.
When the ring wraps inside the hour, `events_dropped` counts the events it overwrote while they were still inside the window.
A path is stored up to 512 bytes and a user agent up to 256, and a value the cap cut ends in `...`, while the analytics log keeps them whole.
The notable-client sample is then cut at 160 characters, marked the same way.
In the traffic report, a request whose path or user agent was cut counts as probe-shaped, so it appears in `probe_paths` and can make an address read as scanning.
Probe and injection detection reads at most the first 512 bytes of a path, so a marker past that cap is not matched.
The cut request is still probe-shaped under the rule above, and an injection marker past the cap is not reported.
The 256-byte user-agent cap bounds the agent text the report shows and the text `claims_to_be_bot` reads.

---

## 13. Exit Codes

The process exits with one of three codes, as Table 7 gives.

| Code | Meaning |
|---|---|
| `0` | Clean shutdown |
| `1` | Runtime error |
| `2` | Config or usage error, before binding |

*Table 7: the exit codes m6-http uses. Take from it that a config error ends the process before it binds, and a clean shutdown is 0.*

A non-zero code from Table 7 is what a unit's status reports, and code 2 is the one the startup sequence returns for a config it refuses.

---

## 14. H2C (HTTP/2 Cleartext)

m6-http supports HTTP/2 without TLS on a separate port, intended for use over a private backbone where network-layer encryption makes application-layer TLS redundant.

**Inbound H2C:** configure `h2c_bind` in `[server]`:

```toml
[server]
bind     = "0.0.0.0:443"
tls_cert = "/run/credentials/m6-http.service/tls-cert.pem"
tls_key  = "/run/credentials/m6-http.service/tls-key.pem"
h2c_bind = "127.0.0.1:8080"   # plain-TCP HTTP/2 on this address
internal_binds = ["127.0.0.1:8080"]
internal_peers = ["127.0.0.1"]   # the only peers it accepts
```

The unit's `LoadCredential` lines deliver the two files, as `m6-user-guide.md` section 7.2 sets out.

The H2C listener accepts plain TCP connections and speaks HTTP/2 framing directly (no TLS record layer).
The request is then routed and handled identically to a TLS request.

The listener is internal, and trusts the `X-Forwarded-For` a cache node sends, only when `[server] internal_binds` names it.
m6-http refuses to start with `h2c_bind` set and `internal_binds` absent.
`internal_binds = []` declares the listener public.
An internal listener closes a connection from any address not in `[server] internal_peers`, and m6-http refuses to start with `internal_binds` set and `internal_peers` absent.

**Outbound H2C:** use the `h2c://` scheme in a backend URL:

```toml
[[backend]]
name = "global"
url  = "h2c://origin.example.com:8080"
```

Outbound H2C connections are maintained persistently by `H2cClientPool`, driven by the event loop, and no thread is spawned per request.
Multiple requests to the same upstream are multiplexed over the single persistent H2 connection.

---

## 15. H2S (HTTP/2 over TLS)

An H2S backend is an HTTP/2 connection over TLS.

**Outbound H2S:** use the `h2s://` scheme in a backend URL:

```toml
[[backend]]
name = "api"
url  = "h2s://api.example.com:443"

[[backend]]
name = "api-dev"
url              = "h2s://dev.example.com:443"
tls_skip_verify  = true   # dev/test only
```

An `https://` or `h2s://` backend is verified against the host's certificate authorities.
A host with none stops m6-http at start with exit 2, naming the backend.

m6-http builds the TLS client with ALPN `h2` at start and connects on the first request.
If the server does not accept `h2` in ALPN, the connection attempt fails and the client receives 502.
The connection is maintained persistently by `H2sTlsClientPool`, event-loop driven, identical to `H2cClientPool` but with a TLS record layer.
Multiple requests to the same upstream are multiplexed over the single persistent H2 connection.

---

## 16. HTTP/2 Denial of Service

Three HTTP/2 behaviours in m6-http bound specific work: a per-connection cap on streams, send-side flow control on the backbone clients, and the stream state machine.
The cap limits how many streams a peer may start, both clients spend only the flow-control credit the peer grants, and the state machine decides which frames a peer may send.
The sections below state each behaviour, the code that enforces it, and the tests that pin it.
Connection count and the rate of PING and SETTINGS frames are outside these three behaviours.

### 16.1 Rapid Reset and the concurrency cap

*Rapid Reset* (CVE-2023-44487) is a denial-of-service attack that opens HTTP/2 streams and cancels each one at once.
m6-http dispatches a request inline from its HEADERS frame, so the request work happens before the cancel arrives.
The attack costs the server one request dispatch per stream and costs the peer one RST_STREAM frame.
m6-http answers it with a cap on counted streams, a decay window that holds a cancelled stream's slot, and a ceiling on consecutive refusals, as Table 8 lists.

| Constant | Value | What it bounds |
|---|---|---|
| `MAX_CONCURRENT` | 100 | Streams one connection may count at once |
| `RESET_DECAY` | 1 second | How long a stream ended by RST_STREAM keeps its slot |
| `RESET_MEMORY` | 200, twice `MAX_CONCURRENT` | Reset stream ids remembered for the slot count and the closed-stream rule |
| `MAX_REFUSED_STREAK` | 50 | Consecutive REFUSED_STREAM answers before the connection ends |

*Table 8: the four constants that bound a Rapid Reset flood, all in `m6-http/src/http2.rs`. Take from it that a peer may have at most `MAX_CONCURRENT` streams counted at once whether it finishes them or cancels them, and that a peer that ignores `MAX_REFUSED_STREAK` refusals loses the connection.*

The cap counts started streams, because `active_streams` adds the live `streams` map to the client-initiated ids in `recently_reset` that are younger than `RESET_DECAY`.
`note_reset` records each reset id with its instant and drops the oldest entry at `RESET_MEMORY` (twice `MAX_CONCURRENT`, so 200 at the cap of 100: derived from the cap so it stays above it whatever the cap is, and so it has no config key of its own).
`active_streams` counts only the client-initiated odd ids in that record.
The decay window in Table 8 is what makes the cap count cancelled streams, as `active_streams` does.
`resetting_streams_does_not_free_concurrency_slots` opens `MAX_CONCURRENT` streams, cancels each one, and checks that the count stays at `MAX_CONCURRENT` and that stream `1 + MAX_CONCURRENT * 2` is refused.

A refusal is recorded as a reset, because the peer's own RST_STREAM is already in flight.
`handle_headers` sends RST_STREAM with REFUSED_STREAM, then `note_reset` records the id so that the crossing cancel reads as a closed stream, which RFC 9113 section 5.1 tolerates.
Without that record, RFC 9113 section 5.1 makes RST_STREAM on an idle stream a connection error, so one refusal could end the connection.
`a_cancel_crossing_a_refusal_is_not_a_connection_error` pins the connection staying up with no GOAWAY.

A flood that ignores refusals ends the connection, because every refused HEADERS still costs a frame parse and an *HPACK* (HTTP/2 header compression) decode.
`refused_streak` counts refusals in a row and resets when a stream is accepted.
At `MAX_REFUSED_STREAK` the server sends one GOAWAY with ENHANCE_YOUR_CALM and ends the connection.
`a_reset_flood_that_ignores_refusals_ends_the_connection` checks exactly one GOAWAY and the phase change.

Slots come back after the decay window, because a permanent hold would block every new stream on that connection after an ordinary burst of cancellations.
`reset_slots_are_released_after_the_decay_window` advances a synthetic clock by twice `RESET_DECAY` and checks that the count falls to zero.
`RESET_DECAY` stays inside legitimate browser behaviour, because a browser may cancel every stream it has open and it cannot have more than `MAX_CONCURRENT` open to begin with.

`tools/rapidreset.py` is the raw-socket reproducer that sends the frames a well-behaved client library will not.
`python3 tools/rapidreset.py [attempts] [host] [h2c-port]` defaults to 200 attempts against an *H2C* (HTTP/2 cleartext) listener on loopback.
Its docstring records the expected result against a fixed server: the first refusal at the 101st attempt (stream 201), 50 REFUSED_STREAM replies, and one GOAWAY carrying 0xb (ENHANCE_YOUR_CALM).
The four tests above are the evidence that runs on a laptop without a listener.

### 16.2 Send-side flow control on the backbone clients

The h2c and h2s backbone clients are one HTTP/2 client, generic over its transport, so one `pump_stream` enforces this for both.
The engine keeps two windows per connection, and a DATA frame needs credit in both, as Table 9 shows.
The checks live in `pump_stream` in `m6-http/src/h2_client.rs`.

| Window | Starts at | Debit | Credit |
|---|---|---|---|
| `conn_send_window` | 65535 | Every DATA frame | Connection WINDOW_UPDATE |
| `send_window` | The peer's SETTINGS_INITIAL_WINDOW_SIZE | Every DATA frame on the stream | Stream WINDOW_UPDATE |

*Table 9: the two send windows each backbone client keeps. Take from it that a DATA frame needs credit in both windows, so a stream-level grant alone cannot open the connection window.*

`pump_stream` takes `conn_send_window.min(send_window)` as its credit and sends frames no larger than the peer's maximum frame size until the credit or the body runs out.
A stream with no credit keeps its unsent bytes in `pending_body` and returns.
A WINDOW_UPDATE on stream 0 adds to the connection window and calls `pump_all`.
A WINDOW_UPDATE on a stream adds to that stream's window and calls `pump_stream` for it.
The connection window in Table 9 starts at 65535 and moves only on a connection WINDOW_UPDATE.

A strict peer answers FLOW_CONTROL_ERROR or closes the connection when a client overruns its credit, so both clients read the connection window before sending.

END_STREAM rides on the frame that drains the body, so the bytes sent always match the advertised `content-length`.
Skipping an uncredited write would send fewer bytes than promised, and blocking in the loop would stall every other connection the poller drives.
The remainder is stream state that resumes when credit arrives.

A WINDOW_UPDATE that would push the connection window above 2^31-1 is a flow-control error, so both clients use `checked_add` and mark the connection dead on overflow.
A WINDOW_UPDATE that would push a stream window above 2^31-1 sends RST_STREAM(FLOW_CONTROL_ERROR) and removes that stream, and the connection stays up.
A zero increment on stream 0 is a connection error, and the client marks the connection dead.
A zero increment on a stream is answered with RST_STREAM(PROTOCOL_ERROR), because RFC 9113 section 6.9 makes it a stream error.

`a_backbone_body_respects_the_peers_send_window` in `m6-http/tests/backbone_flow_control.rs` drives the H2C client against a stub origin that grants a 4 KiB window.
The stub records an overrun for every DATA byte beyond the credit it granted, and the test checks that the body arrives whole and in order.
The H2S transport shares that engine and has no separate integration test.
h2spec tests m6-http as a server, so it exercises neither backbone client.

### 16.3 The stream state machine

The server tracks each HTTP/2 stream in one of the five live states of RFC 9113 section 5.1.
`frame_verdict` in `m6-http/src/http2.rs` is one explicit table over `StreamState`, with the rule for each arm beside it.
Table 10 gives the inbound frames each state allows.

| State | How it is entered | Frames the peer may send |
|---|---|---|
| Idle | The id is above every stream opened so far | HEADERS. PRIORITY is allowed in every state |
| Open | HEADERS opened the stream | Every frame type |
| HalfClosedLocal | This server sent END_STREAM | Every frame type |
| HalfClosedRemote | The peer sent END_STREAM | WINDOW_UPDATE, RST_STREAM and PRIORITY |
| Closed | Both sides finished, or a reset ended the stream | WINDOW_UPDATE and RST_STREAM are tolerated. PRIORITY is always allowed. Other frames are STREAM_CLOSED |

*Table 10: the five live HTTP/2 stream states and the inbound frames each allows. Take from it that any frame other than HEADERS or PRIORITY on an idle stream is a connection error, that a closed stream follows RFC 9113 section 5.1's split, where a frame after a reset is a stream error and a frame after END_STREAM is a connection error, and that WINDOW_UPDATE, RST_STREAM and PRIORITY are tolerated in a closed stream either way.*

Two RFC states are absent on purpose.
`ReservedLocal` and `ReservedRemote` are commented out in `StreamState`, with the RFC rule kept beside each row.
m6-http does not push, so nothing can enter reserved (local), as m6 issue #93 records.
A PUSH_PROMISE from a peer is a connection error under RFC 9113 section 8.4, answered at the frame level before any stream object exists, so nothing can enter reserved (remote).

`Closed` is derived without storing every stream id.
`stream_state` returns `Idle` for an id above `last_stream_id` and `Closed` for one at or below it.
That split tells a frame on a never-opened stream from a frame on a finished stream without an unbounded set of closed ids.
The closed row in Table 10 is the one that needs `recently_reset`, because the error class depends on how the stream closed.

`recently_reset` holds the reset ids so `frame_verdict` can choose the error class Table 10 describes, and it is bounded at `RESET_MEMORY`.
The state table runs before dispatch, so no handler sees a frame in a state that forbids it.
Known gaps below lists the one tracked implementation gap.

### 16.4 The header list size limit

The server advertises `SETTINGS_MAX_HEADER_LIST_SIZE` (0x6) as `MAX_REQUEST_HEAD`, 32 KiB, and enforces that same value while decoding (RFC 9113 6.5.2).
The bound covers one request head: the request line plus every field, counted as name plus value plus 32 bytes per field.
A single field over `MAX_HEADER_LINE` (8 KiB) is refused as well, so a head under the total can still fail on one long field.
Both constants are defined once in `m6_core::headers` and shared with the HTTP/1.1 parser and both backends.

`MAX_HEADER_BLOCK` bounds only the compressed block, at 256 KiB.
HPACK lets one byte refer to a dynamic table entry of up to 4096 bytes, so a block under that cap can decode to a header list hundreds of times larger.
The decoded limit closes that expansion.
It is one eighth of the compressed cap, and a browser request with its cookies is a few KiB, so a legitimate peer does not reach it.

`decode_hpack` counts each field as it decodes and keeps a field only while the list is within `MAX_REQUEST_HEAD`, and it still decodes the rest of an oversize block so the connection's HPACK table stays in step with the peer's.
An over-limit head is then answered 400 with the error page, or 414 when the request line alone is over `MAX_HEADER_LINE`, followed by RST_STREAM with NO_ERROR (RFC 9113 4.3.1 and 8.1).
It is a stream error, so the other streams on the connection continue.
The fault is in one request, and a COMPRESSION_ERROR would end every other stream on the connection.

### 16.5 Known gaps

One HTTP/2 behaviour is absent from the server today, and Table 11 records it with the m6 issue that tracks it.
The row states what the code does and leaves the fix to the issue.

| Gap | What the code does today | Issue |
|---|---|---|
| HTTP/2 error codes | The server sends NO_ERROR, PROTOCOL_ERROR, FLOW_CONTROL_ERROR, STREAM_CLOSED, FRAME_SIZE_ERROR, REFUSED_STREAM, COMPRESSION_ERROR and ENHANCE_YOUR_CALM. It has no path that sends INTERNAL_ERROR, SETTINGS_TIMEOUT, CANCEL, CONNECT_ERROR, INADEQUATE_SECURITY or HTTP_1_1_REQUIRED. The two backbone clients do send CANCEL. | #163 |

*Table 11: the one HTTP/2 behaviour m6-http does not implement, and the issue that tracks it. Take from it that the server sends no CANCEL or INTERNAL_ERROR, and that the issue number is the record of the gap.*

The issue in Table 11 is the record of the gap, and the fix is engineering work tracked there.
Event Loop below describes the one loop that drives these connections, and `m6-architecture.md` §1.10 states the rules it follows.

---

## 17. Event Loop

One thread, one edge-triggered `m6_core::platform::Poll` loop, no async runtime.
`m6-architecture.md` §1.10 states the rules it follows, and `m6-core/examples/echo.rs` is the reference loop.

- No heap allocation on hot path
- No `unwrap()` or `expect()`
- No blocking calls in event loop
- H2C and H2S client pools driven in the event loop alongside TLS/QUIC listeners

The fixed tokens are `TOKEN_TLS_LISTENER`, `TOKEN_H2C_LISTENER`, `TOKEN_UDP`, `TOKEN_SIGNAL` and `TOKEN_LOG`.
Every connection and every upstream gets its own `Token(k)` in the `Slots` table.
A turn matches the token and runs one arm: accept on a listener until it would block, drain the UDP socket for QUIC, read the signal descriptor and hand it to `m6_core::signal::Lifecycle`, flush the log, or drive one connection or upstream.
The wait is bounded by a 1 s accounting tick, every deadline table and quiche's own timeout, and is zero while the disk writer has work: each idle turn then runs one slice.
That single loop is also where every log line is written, and the next section lists the events it writes.

---

## 18. Logging

m6-http writes one structured JSON line per event to stdout, and journald captures it through systemd.

Emitted events: `m6-http listeners bound` at info on start, `request complete` at debug with `path`, `status`, `version`, `backend`, `latency_ns` and `cache_hit`, `genid advanced` at info, `auth: no token` and `auth: token verification failed` at warn, `pool: socket added` and `pool: socket found` at info, and `cache: evicted` at debug.

Periodic stats: `requests`, `rps_avg`, `rps_peak`, `cache_hits`, `cache_misses`, `cache_hit_rate`, `backend_errors`, `pool_members`, `hit_samples`, `hit_p0_ns`, `hit_p50_ns`, `hit_p99_ns`, `hit_max_ns`, `miss_samples`, `miss_p0_ns`, `miss_p50_ns`, `miss_p99_ns`, `miss_max_ns`, `monitor_requests`, `monitor_p50_ns` and `monitor_p99_ns`.
`pool_members` is the total member count read once at start, so it stays fixed for the life of the process.
Each line reports the window since the last line, and each percentile carries its sample count beside it so a p50 over one sample cannot be read as a p50 over a thousand.

---

## 19. Dependencies

m6-http links these crates directly: `quiche` (HTTP/3 and QUIC), `rustls` (TLS), `matchit` (routing), `bytes` (zero-copy buffers), `serde` and `toml` (config), and `jsonwebtoken` (JWT verification).
`quiche` is a fork pinned by revision, and `tools/conformance-scores.txt` records why.

Where this document and the code disagree, the code is right and this document is a defect.
The reasons behind these mechanisms are in [`m6-architecture.md`](m6-architecture.md).
