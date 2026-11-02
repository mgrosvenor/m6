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
2. Load `<system-config>` and merge. It may carry `[server]`, `[node]`, `[auth]` and the `[health]` keys `metrics_require` and `metrics_peers`. Its `[server]` and `[health]` keys win
3. Validate merged config — exit 2 on error, before binding
4. If `[auth]` is declared (in either file, never both), load the public key for local JWT verification. `backend` is optional
5. Expand `[[route_group]]` globs against site directory — builds route table entries and invalidation map
6. Read renderer configs to build params-file → URL invalidation map entries
7. Watch `site.toml` and the system config for reload, and `/run/m6/` for socket discovery
8. Bind TLS listener; if `h2c_bind` is set, bind H2C (plain-TCP HTTP/2) listener on that address, which is internal when `internal_binds` names it and then accepts only the peers in `internal_peers`
9. Start epoll event loop

---

## Backend Pools

Each `[[backend]]` with `sockets` is a pool. m6-http discovers pool members by watching the socket glob's directory via inotify.

**Pool management:**
- Socket appears matching glob → added to pool
- Socket disappears → removed from pool
- Failed connection → socket removed from active pool temporarily, retried after backoff: 1s, 2s, 4s, max 30s
- Empty pool → return appropriate status per `[errors] mode`

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

Route matching: specificity wins. Exact before parameterised. Longer before shorter.

---

## Request Forwarding

Complete request forwarded — method, path, query string, all headers, body.

**Added:** `X-Forwarded-For`, `X-Forwarded-Proto: https`, `X-Forwarded-Host`

**Removed:** `Connection`, `Upgrade`, `Keep-Alive`, `Transfer-Encoding`

---

## Caching

| Backend returns | Behaviour |
|---|---|
| `Cache-Control: public` | Cached under `(path, content-encoding)` key |
| `Cache-Control: no-store` | Not cached |
| `Cache-Control: private` | Not cached |

Cache key: `(path, content-encoding)`. Query strings stripped before lookup. Each encoding variant cached independently on demand — no eager pre-fetching.

### Invalidation

m6-http maintains a map from data files to the URL paths that depend on them. The map is built at startup from two sources:

1. **`[[route_group]]` globs** — each matched file maps to its corresponding URL path. `content/posts/hello-world.json` → `/blog/hello-world`. Map rebuilt on every `site.toml` reload, which re-expands the glob against current files.
2. **Route `params` files** — each params file declared in renderer configs maps to all routes that reference it. `data/posts.json` → `/blog` and `/blog/{stem}` (all cached stems).

When inotify fires on a file change, m6-http looks up affected URL paths in the map and evicts those cache entries.

The params-file-to-route mapping requires m6-http to read renderer configs at startup to build the full map. This is the only time m6-http reads renderer configs.

Map rebuilt on `site.toml` reload.

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
| `[auth] public_key` | `site.toml` or system config, not both | Key that verifies API tokens. A relative path in the system config resolves against the site directory |
| `[health] metrics_require` | either file, system config wins | `role:<name>` or `group:<name>`. A token with the claim is required, from the Authorization header or the session cookie, as route `require` reads it |
| `[health] metrics_peers` | either file, system config wins | IP addresses in a local range. Any other peer gets 404 |
| `[health] metrics_token_file` | either file, system config wins | Legacy bearer string. A relative path in the system config resolves against the site directory. Ignored when `metrics_require` is set |

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

## Hot Reload

m6-http reloads when `site.toml` or the system config changes, and the two changes cost different amounts.
One `ConfigReloader` from m6-core watches both files.
It takes the change from the platform's watcher and, whatever the watcher says, compares the two files' size, times and identity once a second, so a lost watch delays a reload by at most a second.
Renderers and other processes that change site content signal m6-http by touching `site.toml`.
Table 1 says what each change reloads.

| Change | Response |
|---|---|
| `site.toml` touched or modified | Reload routing, pools, the invalidation map, the error mode, the security headers, the log level and the auth key. The cache is cleared, and cleared again 100 ms and 1.08 s after the reload ends |
| System config touched or modified | Reload TLS on HTTP/1.1, HTTP/2 and HTTP/3 from the configured paths, the auth key, the `[health]` access keys, the node name, `allowed_methods` and `backend_timeout_secs`. The cache is kept |
| Both files in one burst | The union of the two rows |
| Certificate or key file written | Nothing, until the system config is touched |
| Socket appears in `/run/m6/` matching a pool glob | Add to pool |
| Socket disappears from `/run/m6/` | Remove from pool |

**Table 1: what a change to each file or directory reloads in m6-http.**
Take from it that only a `site.toml` change empties the cache, and that new key material needs a touch of the system config.

The later cache clears cover a backend that reloads after m6-http.
A request in that gap can fill the cache with the old page.
The clear 100 ms after the reload ends covers a backend that held the change for its 20 ms window, and the clear 1.08 s after covers a backend that found the change only on its once-a-second poll.
Each delay is the m6-core constant (`RELOAD_DEBOUNCE`, `RELOAD_POLL`) plus 80 ms for the backend's own reload.
The cost is one extra cache miss per page about a second after a content change.
The cache-hit path is unchanged.

A reload logs `config reload: complete` with the fields `site`, `process`, `cache_cleared` and `elapsed_us`.
A reload that fails keeps the running config and logs `config reload: failed, keeping current config`.
A system config change still reloads TLS when `site.toml` is mid-edit and fails to load, so a broken `site.toml` does not hold a renewal back.

Some keys are read once, when the listeners bind or the analytics log opens.
They are `[server]` `bind`, `h2c_bind`, `internal_binds`, `internal_peers`, `redirect_bind` and `warm_on_start`, and `[analytics]` `enabled` and `log_path`.
A change to one keeps the running value and logs `config reload: [server].bind changed; restart m6-http to apply it`, naming the key.

### Renewing a certificate

A renewal writes the certificate and the key, then touches the system config.
m6-http does not watch the certificate and key files, so a renewal that omits the touch changes nothing and the previous certificate keeps serving.
The touch comes after both files are written, so m6-http never reads a certificate beside the previous key.
If the touch arrives first, the pair fails its check, the running certificate stays, and the reload is tried again after 1 s, then 2 s, 4 s and so on up to 60 s, and at once on the next touch.
`/perf` reports the `notAfter` of the certificate being served, so a renewal that forgot the touch shows there.

---

## Signal Handling

| Signal | First receipt | Second receipt |
|---|---|---|
| `SIGTERM` or `SIGINT` | Stop accepting, drain in-flight, exit 0 | Immediate exit |
| `SIGUSR1` | Reopen the analytics log file | Same again, and never a step toward exit |

Rotate the analytics file by renaming it and sending `SIGUSR1`.
The process creates the file at its path again, and the path reappearing means the renamed file is finished.
If the reopen fails, the process keeps writing to the previous file and logs an error.
`/perf` reports the last reopen under `logging.reopen`.
`/traffic` also reads the newest rotated file next to the log (`<name>-YYYYmmddHH`, plain or `.gz`), so its window spans a rotation.

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
tls_cert = "cert.pem"
tls_key  = "key.pem"
h2c_bind = "127.0.0.1:8080"   # plain-TCP HTTP/2 on this address
internal_binds = ["127.0.0.1:8080"]
internal_peers = ["127.0.0.1"]   # the only peers it accepts
```

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

m6-http performs TLS negotiation with ALPN `h2` at startup of the first request. If the server does not accept `h2` in ALPN, the connection attempt fails with a 502. The connection is maintained persistently by `H2sTlsClientPool`, event-loop driven — identical to `H2cClientPool` but with a TLS record layer. Multiple requests to the same upstream are multiplexed over the single persistent H2 connection.

---

## HTTP/2 Denial of Service

m6-http bounds the HTTP/2 work a peer can cause in three places.
A per-connection cap limits how many streams a peer may start, both backbone clients spend only the flow-control credit the peer grants, and the stream state machine decides which frames a peer may send.
The sections below state each bound, the code that enforces it, and the test that pins it.
Together they are the denial-of-service posture a reviewer checks before m6-http faces a public port.

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

*Table 3: the four constants that bound a Rapid Reset flood, all in `m6-http/src/http2.rs`. Take from it that a peer may have at most 100 streams counted at once whether it finishes them or cancels them, and that a peer that ignores 50 refusals loses the connection.*

The cap counts started streams, because `active_streams` adds the live `streams` map to the client-initiated ids in `recently_reset` that are younger than `RESET_DECAY`.
`note_reset` records each reset id with its instant and drops the oldest entry at `RESET_MEMORY`.
`RESET_MEMORY` is 128, above the cap of 100, so a peer cannot evict its own reset records and win back slots.
Only client-initiated odd stream ids count.
`resetting_streams_does_not_free_concurrency_slots` opens 100 streams, cancels each one, and checks that the count stays at 100 and that stream 201 is refused.

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
One second stays inside legitimate browser behaviour, because a browser may cancel every stream it has open and it cannot have more than 100 open to begin with.

`tools/rapidreset.py` is the raw-socket reproducer that sends the frames a well-behaved client library will not.
`python3 tools/rapidreset.py [attempts] [host] [h2c-port]` defaults to 200 attempts against an *H2C* (HTTP/2 cleartext) listener on loopback.
Its docstring records the expected result against a fixed server: the first refusal at stream 101, 50 REFUSED_STREAM replies, and one GOAWAY carrying 0xb (ENHANCE_YOUR_CALM).
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

A WINDOW_UPDATE that would push a window above 2^31-1 is a flow-control error, so both clients use `checked_add`. An overflow on stream 0 marks the connection dead. An overflow on a stream sends RST_STREAM(FLOW_CONTROL_ERROR) and drops that stream.
A zero increment on stream 0 is a connection error, and the client marks the connection dead.
A zero increment on a stream is answered with RST_STREAM(PROTOCOL_ERROR), because RFC 9113 section 6.9 makes it a stream error.

`a_backbone_body_respects_the_peers_send_window` in `m6-http/tests/backbone_flow_control.rs` drives the H2C client against a stub origin that grants a 4 KiB window.
The stub records an overrun for every DATA byte beyond the credit it granted, and the test checks that the body arrives whole and in order.
The H2S client has an equivalent `pump_stream` and no separate integration test.
h2spec tests m6-http as a server, so it exercises neither backbone client.

### The stream state machine

The server tracks each HTTP/2 stream in one of the five live states of RFC 9113 section 5.1.
`frame_verdict` in `m6-http/src/http2.rs` is one explicit table over `StreamState`, and every arm cites the rule it enforces.
Table 5 gives the inbound frames each state allows.

| State | How it is entered | Frames the peer may send |
|---|---|---|
| Idle | The id is above every stream opened so far | HEADERS. PRIORITY is allowed in every state |
| Open | HEADERS opened the stream | Every frame type |
| HalfClosedLocal | This server sent END_STREAM | Every frame type |
| HalfClosedRemote | The peer sent END_STREAM | WINDOW_UPDATE, RST_STREAM and PRIORITY |
| Closed | Both sides finished, or a reset ended the stream | WINDOW_UPDATE and RST_STREAM are tolerated. PRIORITY is always allowed. Other frames are STREAM_CLOSED |

*Table 5: the five live HTTP/2 stream states and the inbound frames each allows. Take from it that any frame other than HEADERS or PRIORITY on an idle stream is a connection error, that a frame after a reset is a stream error and a frame after END_STREAM is a connection error, and that WINDOW_UPDATE, RST_STREAM and PRIORITY are tolerated in a closed stream either way.*

Two RFC states are absent on purpose.
`ReservedLocal` and `ReservedRemote` are commented out in `StreamState`, with the RFC rule kept beside each row, so the code still reads against RFC 9113 section 5.1 line by line.
m6-http does not push, so nothing can enter reserved (local), as m6 issue #93 records.
A PUSH_PROMISE from a peer is a connection error under RFC 9113 section 8.4, answered at the frame level before any stream object exists, so nothing can enter reserved (remote).

`Closed` is derived without storing every stream id.
`stream_state` returns `Idle` for an id above `last_stream_id` and `Closed` for one at or below it.
That split tells a frame on a never-opened stream from a frame on a finished stream without an unbounded set of closed ids.

The error class for a closed stream depends on how it closed.
RFC 9113 section 5.1 makes a frame after RST_STREAM a stream error of type STREAM_CLOSED, and a frame after END_STREAM a connection error of the same type.
`recently_reset` holds the reset ids so `frame_verdict` can choose the right class, and it is bounded at `RESET_MEMORY`.
The state table runs before dispatch, so no handler sees a frame in a state that forbids it.
Known gaps below lists the two HTTP/2 limits the server still lacks.

### Known gaps

Two HTTP/2 limits are absent from the server today, and Table 6 records each with the m6 issue that tracks it.

| Gap | What the code does today | Issue |
|---|---|---|
| Maximum header list size | `send_server_settings` advertises `MAX_CONCURRENT_STREAMS`, `INITIAL_WINDOW_SIZE` and `MAX_FRAME_SIZE`. It does not advertise `SETTINGS_MAX_HEADER_LIST_SIZE`. `handle_settings` accepts the peer's value for that setting and ignores it. `MAX_HEADER_BLOCK` still caps one accumulated header block at 256 KiB. | #162 |
| HTTP/2 error codes | The server sends NO_ERROR, PROTOCOL_ERROR, FLOW_CONTROL_ERROR, STREAM_CLOSED, FRAME_SIZE_ERROR, REFUSED_STREAM, COMPRESSION_ERROR and ENHANCE_YOUR_CALM. It has no path that sends INTERNAL_ERROR, SETTINGS_TIMEOUT, CANCEL, CONNECT_ERROR, INADEQUATE_SECURITY or HTTP_1_1_REQUIRED. The two backbone clients do send CANCEL. | #163 |

*Table 6: two HTTP/2 limits m6-http does not implement, and the issues that track them. Take from it that a peer gets no advertised header-list bound and no CANCEL or INTERNAL_ERROR from the server, and that the issue numbers are the record of each gap.*

The bounds described above plus these two open issues are the HTTP/2 denial-of-service posture of m6-http.

---

## Event Loop

Single-threaded epoll. No Tokio.

- No heap allocation on hot path
- No `unwrap()` or `expect()`
- No blocking calls in event loop
- Cache behind `Arc`, swapped atomically — never mutated in place
- inotify fd in same epoll set as network fds
- H2C and H2S client pools driven in the event loop alongside TLS/QUIC listeners

---

## Logging

Structured JSON to stdout. journald captures via systemd.

Emitted events: startup, request complete (path, status, backend, latency_us, cache_hit), auth failure (warn), pool change (info), config reload (info), cache eviction (debug).

Periodic stats: `requests`, `rps_avg`, `rps_peak`, `latency_p50_us`, `latency_p99_us`, `cache_hits`, `cache_misses`, `cache_hit_rate`, `backend_errors`, `pool_members`.

---

## Dependencies

`quinn` (HTTP/3), `rustls` (TLS), `matchit` (routing), `bytes` (zero-copy), `serde`+`toml` (config), `inotify`, `jsonwebtoken` (JWT verification).
