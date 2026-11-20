# m6-http

Reverse proxy, cache, and router.
The only process in an m6 deployment that listens on a public port, unless `[server] reuse_port` asks several of them to share it.

The sections run from the process's surface to its protocols.
§1 to §13 cover the CLI, startup, pools, routing, forwarding, limits, caching, the generation ID, errors, auth, monitoring, signals and exit codes.
§14 to §16 cover H2C, H2S and the HTTP/2 denial-of-service bounds.
§17 to §19 cover the event loop, logging and dependencies, and §20 closes with where to read next.

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
11. [Monitoring](#11-monitoring)
12. [Signal Handling](#12-signal-handling)
13. [Exit Codes](#13-exit-codes)
14. [H2C (HTTP/2 Cleartext)](#14-h2c-http2-cleartext)
15. [H2S (HTTP/2 over TLS)](#15-h2s-http2-over-tls)
16. [HTTP/2 Denial of Service](#16-http2-denial-of-service)
17. [Event Loop](#17-event-loop)
18. [Logging](#18-logging)
19. [Dependencies](#19-dependencies)
20. [Summary](#20-summary)

---

## 1. CLI

```
m6-http <site-dir> <system-config>
```

Both arguments are required to serve.
The system config holds the node's own state: the `[server]` node keys (bind address and TLS paths), `[node]`, `[auth]`, `[genid]`, `[cache]` and `[limits]`.
Everything else comes from `site.toml`, including `[health]` and `[stats]`, which describe the site every node serves.
In one-file mode the same file serves as both, and every key is read from it.

Optional flags:

```
--log-level debug    # override log level
--dump-config        # print effective merged config to stdout, then exit 0
--version, -V        # print the tag form, v1.16.0-rc.1, and exit 0
--version-full       # print one line per m6 crate in the binary, then exit 0
```

`--version` prints the tag, so a deploy can compare it with the tag it installed.
`--version-full` gives each crate's full SemVer string with its own commit and dirty flag, which is what shows a mixed build.

m6-http logs one structured JSON line per event to stdout, or journald's native socket under systemd.

---

## 2. Startup Sequence

m6-http starts in this order, before it serves a request.

1. Load `site.toml` from the site directory.
2. Load `<system-config>`.
   It carries `[node]`, the `[server]` node keys, `[auth]`, `[genid]`, `[cache]` and `[limits]`, and nothing else.
   A key in the wrong file is exit 2.
3. Validate the merged config, exiting 2 on error before binding.
4. Open the log and write the start-up record, the `type=startup` lines that [`m6-log.md`](m6-log.md) §9 gives.
   `--dump-config` prints the merged config and exits 0 before this step, so a config check writes no record.
   A refusal in a later step exits after the record, so the record shows the settings that were refused.
5. If `[auth]` is declared, take the check backend it names.
   `backend` is required and names the socket backend that answers `/check` (`AuthConfig` in `m6-http/src/config.rs`).
   Step 9 probes it.
6. Read the generation state.
   A source with `[genid]` reads `$STATE_DIRECTORY/genid`, and a cache node holds no file.
7. Build the route table: insert each `[[route]]`, then expand each `[[route_group]]` glob against the site directory and add one route per matched file.
8. Resolve every URL backend host once.
   A name that does not resolve is exit 2.
9. Probe every member of the `[auth] backend` pool, when `[auth]` is declared.
   The probe sends one `POST /check` with no credential.
   A 401 or 503 passes, because a checker that refuses or is busy is there.
   A 404 is exit 2, naming the setting, the backend and the socket, because that address is not an auth server.
   A connect failure or a timeout warns and skips that member, so a node can start while its checker is replaced.
   If no member answers 401 or 503 and at least one answered another status, the start is exit 2 and names the backend.
10. Bind the TLS and QUIC sockets on `[server].bind`.
   If `h2c_bind` is set, bind the H2C (plain-TCP HTTP/2) listener on that address.
   The listener is internal when `internal_binds` names it, and then accepts only the peers in `internal_peers`.
11. Start the event loop.

With `[server] reuse_port`, only the public 443 port is shared: its TLS listener and its QUIC UDP socket are bound together through `m6_core::platform::net::listen_shared`, before the TLS configuration is built.
With `reuse_port_processes` set, Linux also attaches a socket filter to both that picks the process from the client's source address, so one client always reaches one process.
The redirect listener on port 80, the internal h2c listener and every backend socket are not shared.
Section 11 gives what `/health` and the metrics line report under `reuse_port`.

---

## 3. Backend Pools

Each `[[backend]]` with `sockets` is a pool.
m6-http finds pool members once at start by expanding the socket glob.

**Pool management:**
- A pool whose glob matches no socket at start exits 2.
- A request that fails to connect tries the next member, and the client receives 502 only when every member has failed.
- A backend that stops and restarts on the same socket path serves again on its next turn.

**Load balancing:** plain round robin.
Each request takes the next member in turn, so `N * k` requests put exactly `k` on each of `N` members.

**URL backends** (`http://`, `https://`) take one HTTP/1.1 connection per request, kept open for later requests in a per-backend idle set of up to eight.
The host name is resolved once at start, so an address change needs a restart.

**H2C backends** (`h2c://`, section 14) and **H2S backends** (`h2s://`, section 15) keep one persistent HTTP/2 connection each.

A backend that answers before it has read the whole request and then refuses the write has that answer forwarded to the client, and its connection is not reused.
A request written in full and answered early keeps its connection, which rejoins the idle set when the reply's framing allows another request.

---

## 4. Routing

m6-http decides a request's path through a fixed order, and each step can answer before the next one runs.

1. **Rate limit:** a request over the per-IP limit is answered 429 before the cache, the redirect, routing or any backend.
2. **www redirect:** with `[site] redirect_www` set (the default), a `www.` host is answered 301 on the apex host.
3. **Unmatched-host redirect:** with `[server] redirect_unmatched_to` set, a Host outside `[server] served_hosts`, including no Host at all, is answered 301 to `https://<redirect_unmatched_to><path>`. The internal h2c listener is excluded.
4. **Method check:** a route that names `methods` refuses another method with 405 and `Allow`, before the cache lookup.
5. **Cache lookup:** a route with no `require` and a method that may read the cache is served from memory on a fresh hit, with no backend contact.
6. **Route lookup:** a path that matches no route is answered 404 per `[errors] mode`.
7. **Auth check:** when the route declares `require`, the credential is sent to `POST /check` on `[auth] backend` under a 50 ms deadline. An API request with no credential is 401 locally with no call, a browser is redirected as section 10 describes, a 401 from the authority is 401, an identity without the requirement is 403, and a check that cannot be made is 503 with `Retry-After: 1`.
8. **Forward:** the request is forwarded to the backend pool.

The two host redirects sit before the cache because the cache key is `(path, query, encoding)` with no host component, so a warm entry for a served name would otherwise answer a request for a name the node does not own.
A redirector on `[server] redirect_bind` makes the same unmatched-host decision: a served name keeps its 301 to the HTTPS form of itself, and every other name goes to the canonical target.

Route matching: a fixed route (every segment a literal) beats a pattern route (any `{param}` or `{*rest}`).
Two fixed routes on one path, and two patterns that can both match some path, are a configuration error at load, and `--dump-config` fails the same way (see `m6-site-toml.md` section 5, Route matching).

### 4.1 The per-IP limit

`[rate_limit]` in `site.toml` sets `enabled` (default true) and `requests_per_min` (default 300), and the limit uses a fixed 60-second window.
A request over the limit is answered 429 with `Retry-After: 60` and the body "Too Many Requests".
The counter is a map in one process's memory, keyed by the client address, and a process restart clears it.
The map tracks up to `[limits.rate_limit_addresses] max` addresses, 50,000 by default.
When it is full and every entry is inside its window, a new address is answered 429 and the error log names the limit.
The limit applies on HTTP/1.1, HTTP/2 and HTTP/3, and `m6-site-toml.md` §4.13 owns the keys.

One process on the public port enforces `requests_per_min` for the node.
With `[server] reuse_port` and `reuse_port_processes = N` on Linux, the kernel sends every connection and every QUIC datagram from one client address to the same process, so the limit is exact per client: 300 requests per minute per client across the port, whatever N is.
On macOS, and on Linux with `reuse_port_processes` unset, each of N processes keeps its own count, so one client can reach up to N times `requests_per_min` across the port: three processes at the default allow 900 requests in a minute.
An operator who needs one number there sets `requests_per_min` to the node total divided by the process count, or enforces the limit at the front door.
A process that stops or starts moves some clients to another process until the group is whole again, and a client that moves starts a fresh count there.
While the group is short of N, a client whose place has no socket is chosen by the kernel's own hash, so a stopped process drops no traffic.

### 4.2 Route methods

A `[[route]]` or `[[route_group]]` may name the methods it answers in `methods`.
A request with another method is refused 405 with `Allow` naming the methods the route answers, before the body is read (RFC 9110 15.5.6 and 10.2.1).
`GET` also answers `HEAD` (RFC 9110 9.3.2).
`OPTIONS` is answered 405 with the route's `Allow` at the edge and never forwarded (RFC 9110 9.3.7).
An unknown or empty `methods` list stops the start with exit 2.
`m6-site-toml.md` §4.8 and §4.9 own the key.

### 4.3 Route cache override

A `[[route]]` or `[[route_group]]` may name `cache`, a `Cache-Control` value that replaces the backend's own on every reply the route gets.
It is applied before the reply is stored, so `no-store` keeps it out of the cache, and the stored copy and the client's reply carry the same one field (RFC 9111 5.2).
The override never weakens a directive that forbids storage: a reply that already carries `no-store` or `private` keeps the field the backend sent, and the route value is not applied (RFC 9111 3.5).

---

## 5. Request Forwarding

m6-http forwards the complete request to the chosen backend: method, path, query string, all headers and body.
A URL backend receives its own host in `Host`, and a socket backend receives the client's.

**Added:** `X-Forwarded-For`, `X-Forwarded-Proto: https`, `X-Forwarded-Host` and `Via`, which appends this hop to a value already present.

**Removed:** every hop-by-hop field (`Connection`, `Upgrade`, `Keep-Alive`, `Transfer-Encoding`, `TE`, `Trailer`, `Proxy-Authorization`, `Proxy-Connection`), every field the client's `Connection` header names (RFC 9110 7.6.1), and each proxy-owned header (`Content-Length`, `X-Forwarded-For`, `X-Forwarded-Proto`, `X-Forwarded-Host`, `X-Real-IP`), which m6-http writes itself.
A client's `X-Auth-Claims` is also dropped on arrival, on every protocol, so only the authority's identity reaches a backend (section 10).

**Response fields:** HTTP/2 and HTTP/3 write every response field name in lower case (RFC 9113 8.2.1, RFC 9114 4.2).
Both drop the connection-specific set `Connection`, `Keep-Alive`, `Proxy-Connection`, `Transfer-Encoding` and `Upgrade`, and HTTP/2 writes its own `Content-Length`.
HTTP/1.1 keeps the fields that describe its own connection, and it writes `Connection` itself.

---

## 6. Request Header Limits

A request head is bounded before routing, the cache lookup and every backend, so the edge answers an over-limit request.
Two limits apply.
`MAX_HEADER_LINE` is 8 KiB for one field, counted as its name plus its value.
`MAX_REQUEST_HEAD` is 32 KiB for the request line plus every field.
HTTP/2 and HTTP/3 count a decoded head as RFC 9113 6.5.2 defines it for `SETTINGS_MAX_HEADER_LIST_SIZE`: name plus value plus 32 bytes per field.

A field over `MAX_HEADER_LINE`, or a head over `MAX_REQUEST_HEAD`, is answered 400 with the error page titled "Request Header Or Cookie Too Large".
A request line over `MAX_HEADER_LINE` is answered 414 Request-URI Too Large, counting the method, target and version on HTTP/1.1 and `:method` plus `:path` on HTTP/2 and HTTP/3.
Both limits are defined once in m6-core as `m6_core::headers::MAX_HEADER_LINE` and `m6_core::headers::MAX_REQUEST_HEAD`, and the HTTP/1.1 parser, both backends and the edge read those definitions.

HTTP/2 advertises `MAX_REQUEST_HEAD` in `SETTINGS_MAX_HEADER_LIST_SIZE` and enforces it while decoding.
`[limits.h2_header_block] max` bounds the compressed header block, 256 KiB by default (section 16.4).
HTTP/3 advertises `MAX_REQUEST_HEAD` in `SETTINGS_MAX_FIELD_SECTION_SIZE`, and the pinned quiche enforces it by closing the connection with H3_EXCESSIVE_LOAD.
A field or request line over `MAX_HEADER_LINE` is answered on the HTTP/3 stream.
The cap on a whole request is `[limits.edge_request] max`, 32 MiB by default.

---

## 7. Caching

m6-http stores a backend response when its status is in Table 1 and its `Cache-Control` says `public`.
The key is `(path, query, content-encoding)`, and each encoding variant is cached on demand.
A response with `no-store` or `private` is never stored.
A response whose `Vary` names any field other than `Accept-Encoding` is not stored, because the key cannot express it.
A shared cache also refuses a response to a request that carried a credential: an `Authorization` field or a `session` cookie forbids the store (RFC 9111 3.5 and 5.2.2.5).
The rule holds whatever the backend's header says and whether or not the route names `cache`.

### 7.1 The browser-caching pattern

A page and an unversioned file are read through two caches with different lifetimes, so one `Cache-Control` field names both.
The browser revalidates, and the shared cache keeps its copy.
A versioned asset is one exact version, so it is pinned.
m6 emits three headers.

- A page, from m6-core: `Cache-Control: public, max-age=0, must-revalidate, s-maxage=86400`, with an `ETag`.
- An unversioned file, from m6-file: the same header, with an `ETag`.
- A versioned asset, `?v=<hash>`: `Cache-Control: public, max-age=31536000, immutable`, with an `ETag`.

`max-age=0` and `must-revalidate` make the browser's copy stale at once and forbid serving it without validation (RFC 9111 sections 5.2.2.1 and 5.2.2.2).
The browser therefore sends `If-None-Match` on every use, and an unchanged response costs one 304.
`s-maxage=86400` is honoured by shared caches only and overrides `max-age` for them (RFC 9111 section 5.2.2.10), so m6-http keeps the response for a day and answers that conditional request itself (RFC 9111 section 4.3.2) without contacting the backend.
`public` makes the response storable by a shared cache.

An upstream that sends `stale-while-revalidate` is honoured: the cache serves stale inside the window the directive names and refreshes behind the request (RFC 9111 section 4.2.4).
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

### 7.3 Which statuses are stored

Storability starts from the statuses RFC 9111 3 lists, and Table 1 gives the set this cache accepts.
A status outside Table 1 is answered to the client and not stored, whatever its `Cache-Control` says.

| Status | Stored when public |
|---|---|
| `200`, `203`, `204` | Yes |
| `300`, `301`, `308` | Yes |
| `404`, `405`, `410`, `414` | Yes |
| `501` | Yes |
| `206` | Never, because the key has no `Range` component and a partial representation would replay as a whole one |
| Every other 4xx, and every 5xx | Never |

**Table 1: the statuses the cache stores, from RFC 9111 3.**
A public 404 is stored, a 206 never is, and no 5xx is.

Table 1 also decides what reaches the disk cache, because only a stored response is written there.

---

## 8. Site generation ID

A source node holds a site generation ID.
The ID is a `u64` counter that names the content the backends are serving at one moment.
A node is a source because its system config carries a `[genid]` table.
A node with no `[genid]` has no endpoint at `/genid` and answers that path as it answers any other unknown path.

The endpoint answers three methods, as Table 2 lists.

| Method | Claim | Answer |
|---|---|---|
| `GET` | `[genid] read_require` | `200` with `{"genid":"<decimal>"}` and `M6-Genid: <decimal>` |
| `HEAD` | `[genid] read_require` | The `GET` status and headers, with no body |
| `POST` | `[genid] write_require` | Runs the change sequence, then `200` with the same shape for the new ID |

**Table 2: the methods `/genid` answers on a source.**
`GET` and `HEAD` need the read claim, `POST` needs the write claim, and each 200 answer carries the ID in `M6-Genid`.

All three methods in Table 2 require a loopback peer or an internal listener, checked before any token is read.
Anyone else gets the 404 an unknown path gets.
Both claims absent is also a 404, so the endpoint exists only when it can answer.
The `[genid]` keys are system config only.
A source needs the unit's `StateDirectory=`.

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
4. Answer every covered POST with the new ID.
   A state write failure answers those POSTs 500 and is logged with the path.

The ID changes only after every backend has the new content, because a cache could otherwise refill from a backend still serving the old bytes under the new number.
A POST that arrives while a sequence runs is parked, and the sequence that answers it started after it arrived.

The ID advances only in step 3 of a POST, never at start.
It lives in `$STATE_DIRECTORY/genid` as the decimal ID and a newline, written to `genid.tmp` and renamed over the file before the POST is answered.
A restart reads that file, so a generation survives a restart.
A source with `[genid]` and no `STATE_DIRECTORY`, or an existing state file that cannot be read or does not hold a `u64`, stops the start with exit 2.
A missing state file starts the ID at 0, so a first start needs no file.
`--dump-config` prints and exits 0 without one.

A publisher that is itself a backend of the source drives this POST from its own event loop, never as a blocking call, so its loop stays free to answer the source's refresh.

### 8.2 The ID on content

Every response the source receives from a backend carries the ID that was current when the request was dispatched, written as `M6-Genid: <decimal>` when the response head is serialised.
HTTP/2 and HTTP/3 write the name in lower case, which RFC 9113 8.2.1 and RFC 9114 4.2 require.
A cached response carries the ID stored with it.
A response whose dispatch ID is no longer current is not stored, so the cache cannot refill from a backend that is one generation behind.
A locally built answer, such as a health check or an error page, carries no ID.

The generation is also reported on the metrics line, under a `genid` object with `role`, `id`, `sequence_running` and the peers that read it.
An operator watching a publish sees one `genid advanced` line at info with `genid`, `members`, `failed` and `elapsed_ms`, and one `genid: member refresh failed` line at error naming the socket path of each member that did not refresh.
The POST still answers 200 when a member failed, and the `failed` count is what says the generation is not uniform.

### 8.3 A cache node follows the source

A node whose `[genid]` sets `upstream` is a cache node.
It names a `[[backend]]` with a `url` and holds no state file.
It asks that upstream for the ID and follows it.

1. **Heartbeat.**
   At start, and then once `heartbeat_secs` (default 10) have passed since the last one finished, the node sends `GET /genid` to the upstream with `Authorization: Bearer <token from token_file>`.
   The heartbeat takes the transport the upstream's URL scheme names: an h2 stream for `h2c://` and `h2s://`, and an HTTP/1.1 upstream slot for `http://` and `https://`.
   One heartbeat is in flight at most, on the accounting tick, under the backend's `backend_timeout_secs` deadline.
   The address is the one resolved at start, so an address change needs a restart.
2. **A new ID is a full flush.**
   The node rebuilds the route table from the config loaded at start, swaps the cache and the held error pages for empty ones in one assignment each, and logs `genid changed` at info with `from` and `to`.
   No entry survives into the new generation, and the cache refills on demand.
   A first answer sets the ID with no flush, because the node held nothing the new ID makes old.
3. **The store rule.**
   A response from any URL backend enters the cache only when its `M6-Genid` parses and equals the ID the node holds, and the entry carries that ID.
   A response with no header, no current ID, or another ID is sent to the client unchanged and not stored.
   Another ID also makes the next heartbeat due at once, so a node that learns of a publish from a response follows it without waiting out the interval.
   On a cache node the ID reaches a client from the upstream's own header on a miss, and from the cached entry on a hit.
4. **Serving while the upstream is down.**
   While the upstream does not answer, an entry past its freshness and stale windows is served as `STALE` with its `Age`.
   A forward that ends in 502, 503 or 504 answers with the held entry for its key when there is one, at any age.
   The operator sees one `genid: upstream down` line at warn on the transition, and the metrics line reports `upstream_down: true`.
   The first answer after an outage logs `genid: upstream up` at info and clears the flag.
5. **Background fetches wait.**
   Prefetch, warm-on-start and stale-while-revalidate fetches are not drained while the node has no ID or the upstream is down.
   The queue holds up to `[limits.refresh_queue] max`, 4096 by default, and drains once the upstream answers. A refresh past the maximum is refused with an error that names the key.
6. **`/genid` is forwarded.**
   `GET`, `HEAD` and `POST /genid` reach the upstream backend after the loopback or internal-listener address check, with the request's own headers, bypassing routing and the cache.
   The cache node checks no token: the source authorises the caller.
   A forwarded POST waits under `backend_timeout_secs` and is answered 504 if the deadline passes while the source is still completing the publish.
   `GET /genid` then shows whether the advance happened.

A cache node reports its own `genid` object on the metrics line: `role` is `cache`, `id` is the ID it holds or `null`, and `upstream`, `upstream_down`, `upstream_rtt_ms` and `last_ok_s` describe the node it follows.
A source restart keeps its ID, so a cache node sees no change when it comes back.

### 8.4 Disk cache (cache nodes)

A cache node with `[cache] dir` in its system config writes the responses it caches to disk, and the request path never touches a file.
A source keeps its cache in memory and is refused at start when it sets `dir`, with `config error: [cache].dir is for a cache node ([genid] upstream)`.

The layout is `<dir>/<genid decimal>/<64 hex characters>`, the file name being the SHA-256 of the cache key's bytes.
A file holds the generation ID, the key, the status and headers as JSON, the body, and a SHA-256 check value over every byte before it.
The writer writes `<name>.tmp` with one write and renames it into place, with no `fsync`.
A crash can leave a short or torn file, and the check value catches it.

**Writing.**
A stored entry is flagged `queued` in the same insert that stores it, and the insert writes nothing.
The writer works in slices on idle turns: while an entry is flagged or an old directory waits for removal, the loop polls with a zero timeout and runs one slice per turn.
Table 3 gives the bounds on one slice.
Removals run first, so their bytes are available to the write bound.
Whichever limit is reached first ends the slice, and an entry the slice could not reach keeps its flag for the next one.
The writer reads each entry again at the slice, so an entry replaced since it was queued is written as it is now, and an entry evicted since is skipped.

| Bound | Value |
|---|---|
| Loop time per slice, checked between files | 1 ms |
| Bytes written per slice | 4 MiB, and a single larger entry is written on its own |
| Files removed per slice | 512 |
| Total size of the directory | `[limits.cache_bytes] max`, 128 MiB by default |

**Table 3: the bounds on the disk writer.**
A full queue costs the loop about 1 ms per turn, and a write that would pass the size bound is skipped.

The disk copy is bounded by the memory cache's size, as the last row of Table 3 says.
A skipped write is logged once per generation at info.
A write failure is logged once per episode with the path and the error, and the entry is dropped without a retry.
While the node has no current ID nothing is written, because nothing is stored in memory then either.

**Generation change.**
The queue is cleared, the new subdirectory is used from the next write, and every other subdirectory is removed over the slices that follow, then the empty directory.

**Restart.**
The node reads the disk copy at start, before the loop serves, and never on the request path.
It reads the subdirectory with the largest decimal name and decodes that generation's files one at a time in file-name order.
Each entry is inserted into a fresh cache with the live bound, so a disk copy larger than the memory bound fills the cache with the entries that fit first and leaves the rest on disk.
A file that is short, fails its check value or carries another generation is deleted and counted, with one log line naming the path and the error.
A `.tmp` file is an interrupted write and is deleted without being counted.
A damaged file never stops the start and is never served.

The decoded entries are held aside until the first heartbeat answers, and the ID decides by the store rule's equality.
A matching ID installs the copy as the live cache, so the node starts warm.
Another ID drops it, because the site changed while the node was down.
When the upstream does not answer, the copy is installed marked unverified, so the node serves the pages it held before the restart.
The first later success clears the mark, keeps the copy when the ID is unchanged and runs the usual flush when it has moved.
The start cost is one directory listing, one read and one SHA-256 per file, up to `[limits.cache_bytes] max`.
The `disk cache loaded` line reports `elapsed_ms` beside the ID, the entry count (`entries`) and the damaged-file count (`bad`).

**Metrics.**
A node with a cache directory adds a `disk_cache` object to the metrics line, with the fields in Table 4.

| Field | Meaning |
|---|---|
| `dir_bytes` | Bytes of the files in the current generation's directory |
| `queued` | Entries whose flag is set |
| `written_last_tick`, `written_bytes_last_tick` | Files and encoded bytes of the last slice, including a single entry over 4 MiB |
| `skipped_full` | Writes skipped for the size bound |
| `ticks`, `tick_last_ns` | Slices taken since start, and the loop time the last one took |
| `loaded_entries`, `loaded_bad` | Entries the start-time load inserted, and files it deleted |
| `state` | `kept`, `discarded`, `unverified` or `none`, and `none` until the first heartbeat answers |

**Table 4: the `disk_cache` fields on the metrics line.**

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

Table 5 gives the three modes.

| Mode | Response |
|---|---|
| `"status"` | Status code, empty body |
| `"internal"` | Status code, m6-http-generated minimal HTML |
| `"custom"` | Status code, error page fetched from `[errors] path` |

**Table 5: the three `[errors] mode` values and the response each produces.**
`custom` fetches a page and `internal` generates one.

`[errors] path` is required when `mode = "custom"`.
Startup validation exits 2 if mode is `"custom"` and path is absent.
m6-http holds each fetched error page by status, apart from the response cache, and a generation change empties them.
`[errors] mode` defaults to `"internal"`.
The error path is declared as a route in `site.toml` like any other.

---

## 10. Auth Enforcement

If a route declares `require`, m6-http asks the authority before forwarding:

1. Extract the credential from the `Authorization: Bearer <token>` header, or if absent from the `session` cookie.
   The header takes precedence over the cookie.
2. Send one `POST /check` over the `[auth] backend` socket, carrying that credential, under a 50 ms deadline (`CHECK_DEADLINE` in `m6-http/src/auth.rs`).
   The edge forwards the token unchanged and decodes nothing.
3. A 200 answers `sub`, `username`, `roles` and `groups`. Check the route's `require` (`group:<n>` or `role:<n>`) against those lists (`Answer::decide` in `m6-core/src/auth.rs`).
4. A 401 is the authority refusing the credential, and the client gets 401.
   A 200 whose identity lacks the requirement is 403.
5. A timeout, a connect failure, or an answer that is neither a 200 identity nor a 401 is 503 with `Retry-After: 1` (`parse_check` in `m6-core/src/auth.rs`).
   The 503 carries no login redirect.

An API request with no credential is answered 401 locally, and no check is sent.
No decision is cached: every protected request makes its own call, so a removed role or a deleted user counts from the next request.

A source node's `GET`, `HEAD` and `POST /genid` take the same path, under `[genid] read_require` and `write_require`.
A refusal is the authority's 401 or 403, and a checker that does not answer is 503 with `Retry-After: 1`.

A route's `require` and a source's `/genid` claims are the two credential consumers of `[auth] backend`.

`m6_core::auth` owns the shapes: `Identity`, `Answer`, `Decision`, the `require` matcher, the answer parser and the one `X-Auth-Claims` encoder.
`m6_core::http::token_refusal` is the one definition of the 401 and 403 bytes, and every route answers it unchanged for an API client: 401 `{"error":"unauthorised"}` with `WWW-Authenticate: Bearer` (RFC 9110 section 11.6.1), 403 `{"error":"forbidden"}` without it.
`[auth]` needs `backend`. Config validation refuses a `require` route or a genid claim without `[auth]`, and refuses `[auth]` without a `backend`, so a running node cannot reach a gated path with no authority.

`[auth] backend` names the `[[backend]]` whose members answer `POST /check`.
Its `sockets` glob must match at least one socket, and a `url` backend is refused by name (`m6-http/src/config.rs`, the `[auth].backend ... is a URL backend` error).
The members are probed once at start (section 2 step 8).

Valid requests are forwarded with the identity the authority answered, base64 JSON in `X-Auth-Claims`.
That header carries `sub`, `username`, `roles` and `groups` and no token claim.

**401 handling:** A browser (`Accept: text/html`) sent to a protected route with no credential, an invalid one or an expired one is redirected: to `/auth/refresh` when a `refresh` cookie is present, otherwise to `/login?next=<original-path>`. A browser whose credential verifies but lacks the role gets the site's own 403 error page, keeping the check's status. API clients (non-`text/html` Accept) receive the JSON refusal directly.

---

## 11. Monitoring

m6-http reports monitoring through one public endpoint and two log lines.
The endpoint is `[health].path`, and it answers liveness.
The two lines carry the numbers an operator reads, and each comes from the process that wrote it.
This section gives the endpoint's answers, the two lines and the traffic window behind the summary.

When `[health].enabled` is true, m6-http answers `[health].path` ahead of routing, the cache and every backend, so a catch-all route cannot answer it and the path never enters the cache.
`[health]` belongs in `site.toml` and has two keys, `enabled` (default true) and `path` (default `/health`), in `HealthConfig` in `m6-http/src/config.rs`.
`/health` is public and has two answers, and the status code carries the answer.
`200` with `{"status":"ok","node":"<name>"}` says the process is alive and every dependency it reads is answering, and the body gains `process_index` when the node sets one.
`503` with `{"status":"degraded","node":"<name>"}` says the process is alive and serving its public and cached pages while a dependency is failing.
On a cache node the dependency is its origin, and a warm cache keeps serving while the origin is unreachable.
A source node reads no dependency here, so it answers `ok` while the process runs.
So `/health` has two answers plus an absence: a process that is gone, or whose listener refuses, gives no answer at all, and m6-monitor reads that as `unreachable`.
These two answers fit the monitor's four states: `ok`, `degraded`, `unknown` and `unreachable`.
The degraded status is 503 so a reader that only looks at the code sees it, and m6-monitor reads a non-200 as `degraded`.

`/health` reads flags and probes nothing.
The origin flag is the cache node's own follower state, set where a heartbeat or a forwarded client request already observes the origin failing and cleared by the next successful heartbeat.
A successful client request clears nothing: the heartbeat observes the recovery, and it is due immediately after a failure, so a recovered origin is reported within one heartbeat interval.
A cache whose last success is an hour old and whose origin has no observed failure still answers `ok`, and a node that has just started answers `ok` until the first failure.

The degraded body carries the node name and, when the node sets one, the process index.
`/health` is public, so the dependency and the reason go in the process log: one warn line when the origin goes bad and one info line when it is back, on the change only and never per request.
m6-monitor's degraded line says a dependency is not answering and points at that log for which one.

With `[server] reuse_port`, `/health` is answered on the shared port, so the process the kernel picks answers it.
`[server] process_index` and `[server] status_bind` give every process a status listener of its own, at the `status_bind` port plus the index.
That listener answers `[health].path` and nothing else, and every other path gets the 404 of a disabled endpoint (`is_status_path` and `status_refusal` in `m6-http/src/main.rs`).

`/health` is the only built-in monitoring endpoint, and a request for `/perf` or `/traffic` is routed like any other path.
`/genid` is the other path m6-http answers itself, on a node whose system config carries `[genid]` (section 8).
The metrics document and the traffic document are log lines, as Table 6 gives, so a monitor reads them from the journal.
Table 6 also gives the interval key that writes each line.

| line | `type` | target | `fields.message` | interval key |
|---|---|---|---|---|
| the metrics document | `perf` | `m6_http::stats` | `stats` | `[stats].interval_s` |
| the traffic document | `traffic` | `m6_http::traffic` | `traffic` | `[stats].traffic_interval_s` |

**Table 6: the two monitoring lines and the interval key that writes each.**

Under systemd each line is one datagram with `M6_TYPE` set to `perf` or `traffic`, so `journalctl M6_TYPE=perf` reads the metrics lines without parsing the document.
The JSON line carries the same word in `type`.
With `[server] reuse_port`, each process writes its own metrics line to the journal under the same node name, and a monitor adds them.

Each line is one JSON object with `type`, `target` and `fields`.
`fields.perf` holds the whole metrics document from `perf_document` in `m6-http/src/health.rs`, and `fields.traffic` holds the traffic document from `build_report` in the same file.
The metrics document holds the request, cache and latency figures, the pool and TLS state, the build as `build.version` (the full SemVer string with its commit), and the host's figures.
The traffic document summarises the last 60 minutes of requests, described at the end of this section.
The metrics document is the largest document m6 writes, and `m6-log.md` section 2 gives its recorded size and the log buffer limits.
The `host` section reports that machine's load, CPU count, memory, disks, interfaces and thermal zones, so a node with more of them writes a longer line.

`metrics.channels` reports each channel under the name `http/<version>/<iface>` (`Stats::record` in `m6-http/src/stats.rs`).
Table 7 lists the key each monitoring reader selects on.

| reader | selection |
|---|---|
| a metrics reader | `type` `perf` in the JSON line, or `M6_TYPE=perf` in the journal |
| a traffic reader | `type` `traffic` in the JSON line, or `M6_TYPE=traffic` in the journal |
| the access reader | `type` `access`, one line per request or monitor poll |
| a liveness reader | `GET /health`, the public endpoint |

**Table 7: the key each monitoring reader selects on.**
The two documents are found by `type` and the endpoint by its path.

The traffic line carries the last 60 minutes this process served, from a window it keeps in memory and reads no log file for.
The window holds at most 32,768 events and is allocated at start.
It starts empty after a restart, and `window_minutes` says how much of the hour it holds: the whole minutes since start, or less when the ring has wrapped.
When the ring wraps inside the hour, `events_dropped` counts the events it overwrote while they were still inside the window.
A path is stored up to 512 bytes and a user agent up to 256, and a value the cap cut ends in `...`, while the access line keeps them whole.
The notable-client sample is then cut at 160 characters, marked the same way.
In the traffic report, a request whose path or user agent was cut counts as probe-shaped, so it appears in `probe_paths` and can make an address read as scanning.
Probe and injection detection reads at most the first 512 bytes of a path, so a marker past that cap is not matched.
The 256-byte user-agent cap bounds the agent text the report shows and the text `claims_to_be_bot` reads.

Section 12 covers the signals the process handles.

---

## 12. Signal Handling

Table 8 gives what each signal does to m6-http.

| Signal | First receipt | Second receipt |
|---|---|---|
| `SIGTERM` or `SIGINT` | Stop accepting and exit 0, without waiting for requests in progress | Immediate exit 0 |
| `SIGUSR1` | Discarded | Discarded |

**Table 8: the signals m6-http handles.**
A second termination signal exits at once.

A clean exit writes one `type=shutdown` line with the run's `run_id`, its uptime and reason `signal`, and a forced exit on the second signal writes reason `forced` ([`m6-log.md`](m6-log.md) §9).

---

## 13. Exit Codes

The process exits with one of three codes, and Table 9 gives each with the failures behind it.

| Code | Meaning |
|---|---|
| `0` | Clean shutdown |
| `1` | Runtime error, including a failed TLS, status or H2C listener bind |
| `2` | Config or usage error, or a failed QUIC or shared-port bind |

**Table 9: the exit codes m6-http uses.**
A config error and a QUIC or shared-port bind failure exit 2, and a TLS, status or H2C listener bind failure exits 1.

The bind arms in `m6-http/src/main.rs` carry the split: the UDP socket and `listen_shared` return 2, and the TLS, status and H2C listeners return 1.
Config validation happens before any listener binds, and `[cache].dir` and `lock_memory` are checked after the listeners are open, so a config error also exits 2 from a node whose listeners are bound.
`m6-site-toml.md` §8 states the same split from the config side.

---

## 14. H2C (HTTP/2 Cleartext)

m6-http serves HTTP/2 without TLS on a separate port, for a private backbone.

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

The H2C listener speaks HTTP/2 framing over plain TCP, and routes and handles each request as it does a TLS request.

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

`H2cClientPool` keeps one persistent connection per upstream in the event loop.
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
`H2sTlsClientPool` keeps one persistent connection per upstream, as `H2cClientPool` does, with a TLS record layer.

---

## 16. HTTP/2 Denial of Service

Three HTTP/2 behaviours in m6-http bound specific work: a per-connection cap on streams, send-side flow control on the backbone clients, and the stream state machine.
The cap limits how many streams a peer may start, both clients spend only the flow-control credit the peer grants, and the state machine decides which frames a peer may send.
Each section names the code that enforces the behaviour and the tests that pin it.

### 16.1 Rapid Reset and the concurrency cap

*Rapid Reset* (CVE-2023-44487) is a denial-of-service attack that opens HTTP/2 streams and cancels each one at once.
m6-http dispatches a request inline from its HEADERS frame, so the request work happens before the cancel arrives.
The attack costs the server one request dispatch per stream and costs the peer one RST_STREAM frame.
m6-http answers it with a cap on counted streams, a decay window that holds a cancelled stream's slot, and a ceiling on consecutive refusals, as Table 10 lists.

| Constant | Value | What it bounds |
|---|---|---|
| `MAX_CONCURRENT` | 100 | Streams one connection may count at once |
| `RESET_DECAY` | 1 second | How long a stream ended by RST_STREAM keeps its slot |
| `RESET_MEMORY` | 200, twice `MAX_CONCURRENT` | Reset stream ids remembered for the slot count and the closed-stream rule |
| `MAX_REFUSED_STREAK` | 50 | Consecutive REFUSED_STREAM answers before the connection ends |

**Table 10: the four constants that bound a Rapid Reset flood, all in `m6-http/src/http2.rs`.**
A peer has at most `MAX_CONCURRENT` streams counted at once whether it finishes or cancels them, and loses the connection after `MAX_REFUSED_STREAK` refusals.

The cap counts started streams, because `active_streams` adds the live `streams` map to the client-initiated ids in `recently_reset` that are younger than `RESET_DECAY`.
`note_reset` records each reset id with its instant and drops the oldest entry at `RESET_MEMORY` (twice `MAX_CONCURRENT`, so 200 at the cap of 100: derived from the cap so it stays above it whatever the cap is, and so it has no config key of its own).
`active_streams` counts only the client-initiated odd ids in that record.
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
The engine keeps two windows per connection, and a DATA frame needs credit in both, as Table 11 shows.
The checks live in `pump_stream` in `m6-http/src/h2_client.rs`.

| Window | Starts at | Debit | Credit |
|---|---|---|---|
| `conn_send_window` | 65535 | Every DATA frame | Connection WINDOW_UPDATE |
| `send_window` | The peer's SETTINGS_INITIAL_WINDOW_SIZE | Every DATA frame on the stream | Stream WINDOW_UPDATE |

**Table 11: the two send windows each backbone client keeps.**
A DATA frame needs credit in both, so a stream-level grant alone cannot open the connection window.

`pump_stream` takes `conn_send_window.min(send_window)` as its credit and sends frames no larger than the peer's maximum frame size until the credit or the body runs out.
A stream with no credit keeps its unsent bytes in `pending_body` and returns.
A WINDOW_UPDATE on stream 0 adds to the connection window and calls `pump_all`.
A WINDOW_UPDATE on a stream adds to that stream's window and calls `pump_stream` for it.

A strict peer answers FLOW_CONTROL_ERROR or closes the connection when a client overruns its credit, so both clients read the connection window before sending.

END_STREAM rides on the frame that drains the body, so the bytes sent always match the advertised `content-length`.
Skipping an uncredited write would send fewer bytes than promised, and blocking in the loop would stall every other connection the poller drives.

A WINDOW_UPDATE that would push the connection window above 2^31-1 is a flow-control error, so both clients use `checked_add` and mark the connection dead on overflow.
A WINDOW_UPDATE that would push a stream window above 2^31-1 sends RST_STREAM(FLOW_CONTROL_ERROR) and removes that stream, and the connection stays up.
A zero increment on stream 0 is a connection error, and the client marks the connection dead.
A zero increment on a stream is answered with RST_STREAM(PROTOCOL_ERROR), because RFC 9113 section 6.9 makes it a stream error.

`a_backbone_body_respects_the_peers_send_window` in `m6-http/tests/backbone_flow_control.rs` drives the H2C client against a stub origin that grants a 4 KiB window.
The stub records an overrun for every DATA byte beyond the credit it granted, and the test checks that the body arrives whole and in order.
The H2S transport shares that engine and has no separate integration test.

### 16.3 The stream state machine

The server tracks each HTTP/2 stream in one of the five live states of RFC 9113 section 5.1.
`frame_verdict` in `m6-http/src/http2.rs` is one explicit table over `StreamState`, with the rule for each arm beside it.
Table 12 gives the inbound frames each state allows.

| State | How it is entered | Frames the peer may send |
|---|---|---|
| Idle | The id is above every stream opened so far | HEADERS. PRIORITY is allowed in every state |
| Open | HEADERS opened the stream | Every frame type |
| HalfClosedLocal | This server sent END_STREAM | Every frame type |
| HalfClosedRemote | The peer sent END_STREAM | WINDOW_UPDATE, RST_STREAM and PRIORITY |
| Closed | Both sides finished, or a reset ended the stream | WINDOW_UPDATE and RST_STREAM are tolerated. PRIORITY is always allowed. Other frames are STREAM_CLOSED |

**Table 12: the five live HTTP/2 stream states and the inbound frames each allows.**
On a closed stream, a frame after a reset is a stream error and a frame after END_STREAM is a connection error.

Two RFC states have no arm in `StreamState`.
`ReservedLocal` and `ReservedRemote` are commented out in `StreamState`, with the RFC rule kept beside each row.
m6-http does not push, so nothing can enter reserved (local), as m6 issue #93 records.
A PUSH_PROMISE from a peer is a connection error under RFC 9113 section 8.4, answered at the frame level before any stream object exists, so nothing can enter reserved (remote).

`Closed` is derived without storing every stream id.
`stream_state` returns `Idle` for an id above `last_stream_id` and `Closed` for one at or below it.
That split tells a frame on a never-opened stream from a frame on a finished stream without an unbounded set of closed ids.
The closed row in Table 12 is the one that needs `recently_reset`, because the error class depends on how the stream closed.

`recently_reset` holds the reset ids so `frame_verdict` can choose the error class Table 12 describes, and it is bounded at `RESET_MEMORY`.
The state table runs before dispatch, so no handler sees a frame in a state that forbids it.

### 16.4 The header list size limit

The server advertises `SETTINGS_MAX_HEADER_LIST_SIZE` (0x6) as `MAX_REQUEST_HEAD`, 32 KiB, and enforces that same value while decoding (RFC 9113 6.5.2).
The bound covers one request head: the request line plus every field, counted as name plus value plus 32 bytes per field.
A single field over `MAX_HEADER_LINE` (8 KiB) is refused as well, so a head under the total can still fail on one long field.
Section 6 defines both constants.

`[limits.h2_header_block] max` bounds the compressed block, at 256 KiB by default.
HPACK lets one byte refer to a dynamic table entry of up to 4096 bytes, so a block under that cap can decode to a far larger header list, and the decoded limit `MAX_REQUEST_HEAD` closes that expansion.

`decode_hpack` counts each field as it decodes and keeps a field only while the list is within `MAX_REQUEST_HEAD`, and it still decodes the rest of an oversize block so the connection's HPACK table stays in step with the peer's.
An over-limit head is then answered 400 with the error page, or 414 when the request line alone is over `MAX_HEADER_LINE`, followed by RST_STREAM with NO_ERROR (RFC 9113 4.3.1 and 8.1).
It is a stream error, so the other streams on the connection continue, where a COMPRESSION_ERROR would end them.

### 16.5 Known gaps

One HTTP/2 behaviour is absent from the server today, and Table 13 records it.

| Gap | What the code does today | Issue |
|---|---|---|
| HTTP/2 error codes | The server sends NO_ERROR, PROTOCOL_ERROR, FLOW_CONTROL_ERROR, STREAM_CLOSED, FRAME_SIZE_ERROR, REFUSED_STREAM, COMPRESSION_ERROR and ENHANCE_YOUR_CALM. It has no path that sends INTERNAL_ERROR, SETTINGS_TIMEOUT, CANCEL, CONNECT_ERROR, INADEQUATE_SECURITY or HTTP_1_1_REQUIRED. The two backbone clients do send CANCEL. | none, recorded here |

**Table 13: the one HTTP/2 behaviour m6-http does not implement.**
The server sends no CANCEL or INTERNAL_ERROR, and no issue tracks the gap.

---

## 17. Event Loop

One thread, one edge-triggered `m6_core::platform::Poll` loop, no async runtime.
`m6-architecture.md` §1.10 states the rules it follows, and `m6-core/examples/echo.rs` is the reference loop.
The H2C and H2S client pools run in the loop beside the TLS and QUIC listeners.

An HTTP/2 connection sends in the same way: `send_until_blocked` frames and writes until the socket would block or no stream has credit left, and the writable edge the loop then waits for runs it again.
The send queue starts at 64 KiB (`[limits.h2_send] start`; the same value is the kernel's unsent-bytes mark on Linux), so a body of any size is sent in many passes, and a socket that took every byte never reports a new edge.
`rfc9113_6_9_h2_serves_a_37_kib_body`, `_300_kib_body` and `_2_mib_body` in `m6-http/tests/rfc_conformance.rs` fetch each size with a client that announces a browser's windows.

The fixed tokens are `TOKEN_TLS_LISTENER`, `TOKEN_STATUS_LISTENER`, `TOKEN_H2C_LISTENER`, `TOKEN_UDP`, `TOKEN_SIGNAL` and `TOKEN_LOG`.
Every connection and every upstream gets its own `Token(k)` in the `Slots` table.
A turn matches the token and runs one arm: accept on a listener until it would block, drain the UDP socket for QUIC, read the signal descriptor and hand it to `m6_core::signal::Lifecycle`, flush the log, or drive one connection or upstream.
The wait is bounded by a 1 s accounting tick, every deadline table and quiche's own timeout, and is zero while the disk writer has work: each idle turn then runs one slice.

---

## 18. Logging

m6-http writes one structured JSON line per event to its one log stream: stdout, or journald's native socket under systemd (`m6-log/src/sink.rs`).
`m6-log.md` owns the line format and the buffer limits.

Emitted events: `m6-http listeners bound` at info on start, `request complete` at debug with `path`, `status`, `version`, `backend`, `latency_ns` and `cache_hit`, `genid advanced` at info, `auth: no token`, `auth: insufficient claims`, `auth: refused` and `auth check returned no decision` at warn, `pool: socket added` and `pool: socket found` at info, and `cache: evicted` at debug.

Three periodic lines follow, each reporting the window since the last line.
The operational line carries `requests`, `rps_avg`, `rps_peak`, `cache_hits`, `cache_misses`, `cache_hit_rate`, `backend_errors`, `pool_members`, `hit_samples`, `hit_p0_ns`, `hit_p50_ns`, `hit_p99_ns`, `hit_max_ns`, `miss_samples`, `miss_p0_ns`, `miss_p50_ns`, `miss_p99_ns`, `miss_max_ns`, `monitor_requests`, `monitor_p50_ns` and `monitor_p99_ns`.
Each percentile carries its sample count beside it, so a p50 over one sample cannot be read as a p50 over a thousand.
`pool_members` is the total member count read once at start.
The `type` `perf` line carries the metrics document under `fields.perf`.
`[stats].interval_s` sets how often these two lines are written, ten seconds by default, and a debug build reads `M6_TEST_STATS_INTERVAL_SECS` to override both intervals.
The `type` `traffic` line carries the traffic document under `fields.traffic`, at `[stats].traffic_interval_s`, one minute by default.
Section 11 gives the targets, messages and document sources of the last two.

---

## 19. Dependencies

m6-http's main dependencies are `quiche` (HTTP/3 and QUIC), `rustls` (TLS), `matchit` (routing), `hpack` (HTTP/2 header decoding), `httparse` (HTTP/1.1), `bytes` (buffers), `serde` and `toml` (config), and `sha2` (the disk cache's file names and check values).
The edge decodes no token, so m6-http has no JWT dependency, and `[auth] backend` answers `POST /check` (section 10).
`quiche` is a fork pinned by revision, and `tools/conformance-scores.txt` records why.
`m6-architecture.md` gives the reasons behind these mechanisms.

---

## 20. Summary

m6-http is the one process on the public port: it terminates TLS on all three HTTP versions, routes to backend pools, caches by path, query and encoding, and asks `POST /check` at every credential gate.
It reads its two config files once, writes its start-up record, and refuses to start on any setting it cannot honour.
While it runs, `/health` and `/genid` are the only paths it answers itself, and the metrics and traffic documents leave as log lines.
[`m6-site-toml.md`](m6-site-toml.md) gives every key, [`m6-log.md`](m6-log.md) the log stream, and [`m6-architecture.md`](m6-architecture.md) the reasons.

