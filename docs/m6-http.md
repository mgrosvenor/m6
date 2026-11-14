# m6-http

Reverse proxy, cache, and router. The only process in an m6 deployment that listens on a public port. Does not manage other processes — that is systemd's job.

---

## CLI

```
m6-http <site-dir> <system-config>
```

Both arguments required. Development uses a minimal local system config (e.g. `configs/system.toml`). Production uses `/etc/m6/my-blog.toml`. The system config holds the node's own state: the `[server]` node keys (bind address and TLS paths), `[node]`, `[auth]` and the `[health]` access keys. Everything else comes from `site.toml` unchanged.

Optional flags:

```
--log-level debug    # override log level
--dump-config        # print effective merged config to stdout, then exit 0
```

Logs to stdout as structured JSON.

---

## Startup Sequence

1. Load `site.toml` from site directory
2. Load `<system-config>`. It carries `[node]`, the `[server]` node keys, `[auth]` and the `[health]` access keys, and nothing else. A key in the wrong file is exit 2
3. Validate merged config — exit 2 on error, before binding
4. If `[auth]` is declared in the system config, load the public key for local JWT verification. `backend` is optional
5. Expand `[[route_group]]` globs against site directory — builds route table entries and invalidation map
6. Read renderer configs to build params-file → URL invalidation map entries
7. Bind the TLS listener. If `h2c_bind` is set, bind H2C (plain-TCP HTTP/2) listener on that address, which is internal when `internal_binds` names it and then accepts only the peers in `internal_peers`
8. Start epoll event loop

---

## Backend Pools

Each `[[backend]]` with `sockets` is a pool. m6-http finds pool members once at start by expanding the socket glob.

**Pool management:**
- Members are the sockets the glob matches when m6-http starts
- A pool whose glob matches no socket at start exits 2
- Failed connection → member backed off, retried after 1s, 2s, 4s, max 30s
- A backend that restarts on the same socket path serves again after its backoff

**Load balancing:** least-connections across active pool members.

**URL backends** (`http://`, `https://`) are not pooled — one HTTP/1.1 connection per request, run on a dedicated I/O thread.

**H2C backends** use the `h2c://` scheme (e.g. `url = "h2c://origin.example.com:8080"`). The connection is maintained persistently by an event-loop-driven `H2cClientPool` — no thread is spawned. Intended for inter-node requests over a private backbone, where TLS at the application layer is redundant.

**H2S backends** use the `h2s://` scheme (e.g. `url = "h2s://api.example.com:443"`). Like H2C but over TLS — ALPN negotiates `h2` during the handshake; startup fails if the server does not support it. The connection is maintained persistently by an event-loop-driven `H2sTlsClientPool`. Intended for HTTP/2 forwarding to external TLS-terminating upstreams.

---

## Routing

1. **Cache hit** — serve from memory, no backend contact
2. **Auth check** — if route has `require`, verify JWT locally; 401 if absent, 403 if insufficient
3. **Route match** — forward to backend pool
4. **No match** — 404 per `[errors] mode`

Route matching: a fixed route (every segment a literal) beats a pattern route (any `{param}` or `{*rest}`). Two fixed routes on one path, and two patterns that can both match some path, are a configuration error at load, and `--dump-config` fails the same way (see `m6-site-toml.md`, Route Matching).

---

## Request Forwarding

Complete request forwarded — method, path, query string, all headers, body.

**Added:** `X-Forwarded-For`, `X-Forwarded-Proto: https`, `X-Forwarded-Host`

**Removed:** `Connection`, `Upgrade`, `Keep-Alive`, `Transfer-Encoding`

---

## Request Header Limits

A request head is bounded before routing, the cache lookup and every backend, so an over-limit request is answered by the edge and no backend sees it.
Two limits apply, and both follow the numbers the mainstream servers use: nginx's `large_client_header_buffers 4 8k`, Varnish's `http_req_hdr_len 8k` and `http_req_size 32k`, and Apache's `LimitRequestFieldSize 8190`.
`MAX_HEADER_LINE` is 8 KiB for one field, counted as its name plus its value.
`MAX_REQUEST_HEAD` is 32 KiB for the request line plus every field.
HTTP/2 and HTTP/3 count a decoded head as RFC 9113 6.5.2 defines it for `SETTINGS_MAX_HEADER_LIST_SIZE`: name plus value plus 32 bytes per field.

A field over `MAX_HEADER_LINE`, or a head over `MAX_REQUEST_HEAD`, is answered 400 with the error page titled "Request Header Or Cookie Too Large", which is nginx's own text for this refusal.
A request line over `MAX_HEADER_LINE` is answered 414 Request-URI Too Large, counting the method, target and version on HTTP/1.1 and `:method` plus `:path` on HTTP/2 and HTTP/3.
Both limits are defined once in m6-core as `m6_core::headers::MAX_HEADER_LINE` and `m6_core::headers::MAX_REQUEST_HEAD`, and the HTTP/1.1 parser, both backends and the edge read those definitions.

HTTP/2 advertises `MAX_REQUEST_HEAD` in `SETTINGS_MAX_HEADER_LIST_SIZE` and enforces it while decoding.
The 256 KiB `MAX_HEADER_BLOCK` bound on the compressed header block stays, because it is the defence against HPACK expansion.
HTTP/3 advertises `MAX_REQUEST_HEAD` in `SETTINGS_MAX_FIELD_SECTION_SIZE`, and the pinned quiche enforces it by closing the connection with H3_EXCESSIVE_LOAD.
A single field over `MAX_HEADER_LINE` and a request line over it are answered on the HTTP/3 stream, as they are on the other two protocols.
The HTTP/1.1 head limit is separate from the 20 MiB cap on a whole request, and that cap stays.

---

## Caching

m6-http caches a backend response when that response says a shared cache may store it.
`Cache-Control: public` is stored under the `(path, query, content-encoding)` key, while `no-store` and `private` are not stored.
Each encoding variant is cached independently on demand, with no eager pre-fetching.

### The browser-caching pattern

A page and an unversioned file are read through two caches with different lifetimes, so one `Cache-Control` field names both.
This is the split web.dev's HTTP caching guide and MDN's `Cache-Control` reference document: the browser revalidates, the shared cache keeps its copy.
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

### Invalidation

m6-http maintains a map from data files to the URL paths that depend on them. The map is built at startup from two sources:

1. **`[[route_group]]` globs** map each matched file to its corresponding URL path. `content/posts/hello-world.json` → `/blog/hello-world`, which expands the glob against current files.
2. **Route `params` files** — each params file declared in renderer configs maps to all routes that reference it. `data/posts.json` → `/blog` and `/blog/{stem}` (all cached stems).

The params-file-to-route mapping requires m6-http to read renderer configs at startup to build the full map. This is the only time m6-http reads renderer configs.

### 4xx and 5xx responses are never cached

m6-http never caches responses with status 4xx or 5xx, regardless of `Cache-Control` headers.

---

## Error Handling

### Error flow

When a backend returns a non-2xx response, or no backend instance is available:

1. m6-http checks `[errors] mode`
2. If `mode = "custom"`, m6-http makes a GET request to `<[errors] path>?status=<N>&from=<original-path>`
3. The backend renders an error page using the `status` and `from` query params, returns 200
4. m6-http returns that body to the client with the **original** status code
5. If the fetch fails, or if the failing request was already to the error path (no recursion), m6-http falls back to `"internal"` mode

| Mode | Response |
|---|---|
| `"status"` | Status code, empty body |
| `"internal"` | Status code, m6-http-generated minimal HTML |
| `"custom"` | Status code, error page fetched from `[errors] path` |

`[errors] path` is required when `mode = "custom"`. Startup validation exits 2 if mode is `"custom"` and path is absent.

Error responses are never cached. The error path is declared as a route in `site.toml` like any other.

No static fallback file.

---

## Auth Enforcement

If a route declares `require`, m6-http enforces before forwarding:

1. Extract JWT from `Authorization: Bearer <token>` header, or if absent from the `session` cookie. Header takes precedence — API clients use it; browsers use the cookie.
2. Verify signature locally using m6-auth's public key — no network call
3. Check token expiry
4. Check claims against `require` declaration (`group:<n>` or `role:<n>`)
5. Reject with 401 (no token / invalid token) or 403 (insufficient claims)

One function, `auth::authorise`, does steps 1 to 4 for route `require` and for the monitoring endpoints below, so both accept the same tokens and refuse the same ways.
A 401 from m6-http itself carries `WWW-Authenticate: Bearer` (RFC 9110 section 11.6.1).
A `WWW-Authenticate` header a backend already supplied is kept.

Valid requests are forwarded with the verified token intact. Renderers may perform additional fine-grained checks using the auth claims available in the request dictionary.

**401 handling:** When a browser request (`Accept: text/html`) to a protected route has no token or an expired token, m6-http checks whether a `refresh` cookie is present. If so, it redirects to `POST /auth/refresh` — if the refresh token is still valid, new cookies are set and the browser is redirected back to the original path transparently. If the refresh token is also expired or absent, m6-http redirects to `/login?next=<original-path>`. API clients (non-`text/html` Accept) always receive 401 directly.

---


## Monitoring Endpoints

m6-http answers `/health`, `/perf` and `/traffic` itself, ahead of routing, the cache and every backend.
A refusal never reaches routing or the cache, so a catch-all route can never answer `/perf` and a built-in path never enters the cache.
`/health` is public.
`/perf` and `/traffic` are gated by the `[health]` keys in Table 1.

| Key | Where | Meaning |
|---|---|---|
| `[auth] public_key` | `site.toml` or system config, not both | Key that verifies API tokens. An absolute path |
| `[health] metrics_require` | either file, system config wins | `role:<name>` or `group:<name>`. A token with the claim is required, from the Authorization header or the session cookie, as route `require` reads it |
| `[health] metrics_peers` | either file, system config wins | IP addresses in a local range. Any other peer gets 404 |
| `[health] metrics_token_file` | either file, system config wins | Legacy bearer string. An absolute path. Ignored when `metrics_require` is set |

*Table 1: the keys that decide who reads `/perf` and `/traffic`. With none of them set, only the legacy file gate applies.*

The checks run in this order, as Table 2 shows.

| Step | Check | Refusal |
|---|---|---|
| 1 | `metrics_peers` set: not on an internal h2c listener, and the peer address is listed | 404, the body of a disabled endpoint, before any token is read |
| 2 | `metrics_require` set: the token (Authorization header or session cookie) verifies and holds the claim | 401 for no or a bad token, 403 for the wrong claim |
| 2 | `metrics_require` unset: the legacy bearer string matches | 404 when no string is configured, 401 when it does not match |

*Table 2: the metrics gate, in order. A request reaches the report only when every step that applies passes.*

Step 1 reads the peer address a completed TCP or QUIC handshake proved.
It is never read from a header, so a forged `X-Forwarded-For` does not open the endpoints.
An internal h2c listener believes `X-Forwarded-For`, so it never serves them.
The cost is one address parse and a scan of a short list, and only on these two paths.
The cache-hit path is unchanged.

---

## Signal Handling

| Signal | First receipt | Second receipt |
|---|---|---|
| `SIGTERM` or `SIGINT` | Stop accepting, remove the socket if there is one, exit 0, without waiting for requests in progress | Immediate exit |
| `SIGUSR1` | Reopen the analytics log file | Same again, and never a step toward exit |

Rotate the analytics file by renaming it and sending `SIGUSR1`.
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

## Exit Codes

| Code | Meaning |
|---|---|
| `0` | Clean shutdown |
| `1` | Runtime error |
| `2` | Config or usage error — before binding |

---

## H2C (HTTP/2 Cleartext)

m6-http supports HTTP/2 without TLS on a separate port, intended for use over a private backbone where network-layer encryption makes application-layer TLS redundant.

**Inbound H2C** — configure `h2c_bind` in `[server]`:

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

The H2C listener accepts plain TCP connections and speaks HTTP/2 framing directly (no TLS record layer). The request is then routed and handled identically to a TLS request.

The listener is internal, and trusts the `X-Forwarded-For` a cache node sends, only when `[server] internal_binds` names it.
m6-http refuses to start with `h2c_bind` set and `internal_binds` absent.
`internal_binds = []` declares the listener public.
An internal listener closes a connection from any address not in `[server] internal_peers`, and m6-http refuses to start with `internal_binds` set and `internal_peers` absent.

**Outbound H2C** — use the `h2c://` scheme in a backend URL:

```toml
[[backend]]
name = "global"
url  = "h2c://origin.example.com:8080"
```

Outbound H2C connections are maintained persistently by `H2cClientPool`, driven by the event loop — no thread is spawned per request. Multiple requests to the same upstream are multiplexed over the single persistent H2 connection.

---

## H2S (HTTP/2 over TLS)

**Outbound H2S** — use the `h2s://` scheme in a backend URL:

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

m6-http performs TLS negotiation with ALPN `h2` at startup of the first request. If the server does not accept `h2` in ALPN, the connection attempt fails with a 502. The connection is maintained persistently by `H2sTlsClientPool`, event-loop driven — identical to `H2cClientPool` but with a TLS record layer. Multiple requests to the same upstream are multiplexed over the single persistent H2 connection.

---

## HTTP/2 Denial of Service

Three HTTP/2 behaviours in m6-http bound specific work: a per-connection cap on streams, send-side flow control on the backbone clients, and the stream state machine.
The cap limits how many streams a peer may start, both clients spend only the flow-control credit the peer grants, and the state machine decides which frames a peer may send.
The sections below state each behaviour, the code that enforces it, and the tests that pin it.
Connection count and the rate of PING and SETTINGS frames are outside these three behaviours.

### Rapid Reset and the concurrency cap

*Rapid Reset* (CVE-2023-44487) is a denial-of-service attack that opens HTTP/2 streams and cancels each one at once.
m6-http dispatches a request inline from its HEADERS frame, so the request work happens before the cancel arrives.
The attack costs the server one request dispatch per stream and costs the peer one RST_STREAM frame.
m6-http answers it with a cap on counted streams, a decay window that holds a cancelled stream's slot, and a ceiling on consecutive refusals, as Table 3 lists.

| Constant | Value | What it bounds |
|---|---|---|
| `MAX_CONCURRENT` | 100 | Streams one connection may count at once |
| `RESET_DECAY` | 1 second | How long a stream ended by RST_STREAM keeps its slot |
| `RESET_MEMORY` | 128 | Reset stream ids remembered for the slot count and the closed-stream rule |
| `MAX_REFUSED_STREAK` | 50 | Consecutive REFUSED_STREAM answers before the connection ends |

*Table 3: the four constants that bound a Rapid Reset flood, all in `m6-http/src/http2.rs`. Take from it that a peer may have at most `MAX_CONCURRENT` streams counted at once whether it finishes them or cancels them, and that a peer that ignores `MAX_REFUSED_STREAK` refusals loses the connection.*

The cap counts started streams, because `active_streams` adds the live `streams` map to the client-initiated ids in `recently_reset` that are younger than `RESET_DECAY`.
`note_reset` records each reset id with its instant and drops the oldest entry at `RESET_MEMORY`.
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

### Send-side flow control on the backbone clients

Both backbone clients send a request body only within the credit the peer granted.
*H2C* (HTTP/2 cleartext) and *H2S* (HTTP/2 over TLS) keep two windows per connection, and a DATA frame needs credit in both, as Table 4 shows.
The checks live in `pump_stream` in `m6-http/src/h2c_client.rs` and `m6-http/src/h2s_client.rs`.

| Window | Starts at | Debit | Credit |
|---|---|---|---|
| `conn_send_window` | 65535 | Every DATA frame | Connection WINDOW_UPDATE |
| `send_window` | The peer's SETTINGS_INITIAL_WINDOW_SIZE | Every DATA frame on the stream | Stream WINDOW_UPDATE |

*Table 4: the two send windows each backbone client keeps. Take from it that a DATA frame needs credit in both windows, so a stream-level grant alone cannot open the connection window.*

`pump_stream` takes `conn_send_window.min(send_window)` as its credit and sends frames no larger than the peer's maximum frame size until the credit or the body runs out.
A stream with no credit keeps its unsent bytes in `pending_body` and returns.
A WINDOW_UPDATE on stream 0 adds to the connection window and calls `pump_all`.
A WINDOW_UPDATE on a stream adds to that stream's window and calls `pump_stream` for it.

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
The H2S client has an equivalent `pump_stream` and no separate integration test.
h2spec tests m6-http as a server, so it exercises neither backbone client.

### The stream state machine

The server tracks each HTTP/2 stream in one of the five live states of RFC 9113 section 5.1.
`frame_verdict` in `m6-http/src/http2.rs` is one explicit table over `StreamState`, with the rule for each arm beside it.
Table 5 gives the inbound frames each state allows.

| State | How it is entered | Frames the peer may send |
|---|---|---|
| Idle | The id is above every stream opened so far | HEADERS. PRIORITY is allowed in every state |
| Open | HEADERS opened the stream | Every frame type |
| HalfClosedLocal | This server sent END_STREAM | Every frame type |
| HalfClosedRemote | The peer sent END_STREAM | WINDOW_UPDATE, RST_STREAM and PRIORITY |
| Closed | Both sides finished, or a reset ended the stream | WINDOW_UPDATE and RST_STREAM are tolerated. PRIORITY is always allowed. Other frames are STREAM_CLOSED |

*Table 5: the five live HTTP/2 stream states and the inbound frames each allows. Take from it that any frame other than HEADERS or PRIORITY on an idle stream is a connection error, that a closed stream follows RFC 7540 section 5.1's split, where a frame after a reset is a stream error and a frame after END_STREAM is a connection error, and that WINDOW_UPDATE, RST_STREAM and PRIORITY are tolerated in a closed stream either way.*

Two RFC states are absent on purpose.
`ReservedLocal` and `ReservedRemote` are commented out in `StreamState`, with the RFC rule kept beside each row.
m6-http does not push, so nothing can enter reserved (local), as m6 issue #93 records.
A PUSH_PROMISE from a peer is a connection error under RFC 9113 section 8.4, answered at the frame level before any stream object exists, so nothing can enter reserved (remote).

`Closed` is derived without storing every stream id.
`stream_state` returns `Idle` for an id above `last_stream_id` and `Closed` for one at or below it.
That split tells a frame on a never-opened stream from a frame on a finished stream without an unbounded set of closed ids.

`recently_reset` holds the reset ids so `frame_verdict` can choose the error class Table 5 describes, and it is bounded at `RESET_MEMORY`.
The state table runs before dispatch, so no handler sees a frame in a state that forbids it.
Known gaps below lists the one tracked implementation gap.

### The header list size limit

`send_server_settings` advertises `SETTINGS_MAX_HEADER_LIST_SIZE` (0x6) as 64 KiB, and the server enforces the same value, `MAX_HEADER_LIST_SIZE` in `m6-http/src/http2.rs`.
The size is counted as RFC 9113 6.5.2 defines it: the length of each name plus the length of each value plus 32 bytes per field, over the decoded fields of one header block.

`MAX_HEADER_BLOCK` bounds only the compressed block, at 256 KiB.
HPACK lets one byte refer to a dynamic table entry of up to 4096 bytes, so a block under that cap can decode to a header list hundreds of times larger.
The decoded limit closes that expansion.
It is a quarter of the compressed cap, and a browser request with its cookies is a few KiB, so a legitimate peer does not reach it.

A block whose decoded list is over the limit is decoded in full, so the connection's HPACK table stays in step with the peer's, and is then answered with a 431 that has no body, followed by RST_STREAM with NO_ERROR (RFC 9113 4.3.1 and 8.1).
It is a stream error and not a connection error.
The fault is in one request, and a COMPRESSION_ERROR would end every other stream on the connection.

### Known gaps

One HTTP/2 limit is absent from the server today, and Table 6 records it with the m6 issue that tracks it.
The row states what the code does and leaves the fix to the issue.

| Gap | What the code does today | Issue |
|---|---|---|
| HTTP/2 error codes | The server sends NO_ERROR, PROTOCOL_ERROR, FLOW_CONTROL_ERROR, STREAM_CLOSED, FRAME_SIZE_ERROR, REFUSED_STREAM, COMPRESSION_ERROR and ENHANCE_YOUR_CALM. It has no path that sends INTERNAL_ERROR, SETTINGS_TIMEOUT, CANCEL, CONNECT_ERROR, INADEQUATE_SECURITY or HTTP_1_1_REQUIRED. The two backbone clients do send CANCEL. | #163 |

*Table 6: the one HTTP/2 limit m6-http does not implement, and the issue that tracks it. Take from it that the server sends no CANCEL or INTERNAL_ERROR, and that the issue number is the record of the gap.*

The gap is engineering work tracked by the issue.
Event Loop below describes the single-threaded epoll loop that drives these connections.

---

## Event Loop

Single-threaded epoll. No Tokio.

- No heap allocation on hot path
- No `unwrap()` or `expect()`
- No blocking calls in event loop
- Cache behind `Arc`, shared read-only once built
- H2C and H2S client pools driven in the event loop alongside TLS/QUIC listeners

---

## Logging

Structured JSON to stdout. journald captures via systemd.

Emitted events: startup, request complete (path, status, backend, latency_us, cache_hit), auth failure (warn), pool change (info), cache eviction (debug).

Periodic stats: `requests`, `rps_avg`, `rps_peak`, `latency_p50_us`, `latency_p99_us`, `cache_hits`, `cache_misses`, `cache_hit_rate`, `backend_errors`, `pool_members`.

---

## Dependencies

`quinn` (HTTP/3), `rustls` (TLS), `matchit` (routing), `bytes` (zero-copy), `serde`+`toml` (config), `jsonwebtoken` (JWT verification).
