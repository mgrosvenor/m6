# m6-http

Reverse proxy, cache, and router. The only process in an m6 deployment that listens on a public port. Does not manage other processes — that is systemd's job.

---

## CLI

```
m6-http <site-dir> <system-config>
```

Both arguments required. Development uses a minimal local system config (e.g. `configs/system.toml`). Production uses `/etc/m6/my-blog.toml`. The system config contains only `[server]` — bind address and TLS paths that differ per environment. Everything else comes from `site.toml` unchanged.

Optional flags:

```
--log-level debug    # override log level
--dump-config        # print effective merged config to stdout, then exit 0
```

Logs to stdout as structured JSON.

---

## Startup Sequence

1. Load `site.toml` from site directory
2. Load `<system-config>` and merge — system config `[server]` keys win
3. Validate merged config — exit 2 on error, before binding
4. If `[auth]` declared, load public key for local JWT verification
5. Expand `[[route_group]]` globs against site directory — builds route table entries and invalidation map
6. Read renderer configs to build params-file → URL invalidation map entries
7. Register inotify on site directory (`site.toml`), system config, and `/run/m6/` (socket discovery)
8. Bind TLS listener; if `h2c_bind` is set, bind H2C (plain-TCP HTTP/2) listener on that address
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

**URL backends** are not pooled — single upstream. Currently only HTTP/1.1 is supported over `http://` and `https://` URLs; HTTP/2 ALPN negotiation is not yet implemented.

**H2C backends** use the `h2c://` scheme (e.g. `url = "h2c://10.0.0.2:8080"`). The connection is maintained persistently by an event-loop-driven `H2cClientPool` — no thread is spawned. Intended for inter-node requests over WireGuard tunnels, where TLS at the application layer is redundant.

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

Valid requests are forwarded with the verified token intact. Renderers may perform additional fine-grained checks using the auth claims available in the request dictionary.

**401 handling:** When a browser request (`Accept: text/html`) to a protected route has no token or an expired token, m6-http checks whether a `refresh` cookie is present. If so, it redirects to `POST /auth/refresh` — if the refresh token is still valid, new cookies are set and the browser is redirected back to the original path transparently. If the refresh token is also expired or absent, m6-http redirects to `/login?next=<original-path>`. API clients (non-`text/html` Accept) always receive 401 directly.

---


## Hot Reload

`site.toml` is the sole inotify watch target for site content. It is the sync point — touching it triggers a full reload of routing, backend pools, auth config, and the cache invalidation map (which re-expands `[[route_group]]` globs against current files). Renderers and other processes that modify site content signal m6-http by touching `site.toml`.

| Change | Response |
|---|---|
| `site.toml` touched or modified | Reload routing, pools, auth config, invalidation map |
| System config modified | Re-merge `[server]` with `site.toml`, reload TLS context |
| TLS cert/key file modified | Reload TLS context |
| Socket appears in `/run/m6/` matching a pool glob | Add to pool |
| Socket disappears from `/run/m6/` | Remove from pool |

Cache eviction is driven by the invalidation map, which is rebuilt on every `site.toml` reload. m6-http does not watch individual data files.

No restart needed for any of the above.

---

## Signal Handling

| Signal | First receipt | Second receipt |
|---|---|---|
| `SIGTERM` or `SIGINT` | Stop accepting, drain in-flight, exit 0 | Immediate exit |

---

## Exit Codes

| Code | Meaning |
|---|---|
| `0` | Clean shutdown |
| `1` | Runtime error |
| `2` | Config or usage error — before binding |

---

## H2C (HTTP/2 Cleartext)

m6-http supports HTTP/2 without TLS on a separate port, intended for use over WireGuard tunnels where network-layer encryption makes application-layer TLS redundant.

**Inbound H2C** — configure `h2c_bind` in `[server]`:

```toml
[server]
bind     = "0.0.0.0:443"
tls_cert = "cert.pem"
tls_key  = "key.pem"
h2c_bind = "127.0.0.1:8080"   # plain-TCP HTTP/2 on this address
```

The H2C listener accepts plain TCP connections and speaks HTTP/2 framing directly (no TLS record layer). The request is then routed and handled identically to a TLS request.

**Outbound H2C** — use the `h2c://` scheme in a backend URL:

```toml
[[backend]]
name = "global"
url  = "h2c://10.0.0.2:8080"
```

Outbound H2C connections are maintained persistently by `H2cClientPool`, driven by the event loop — no thread is spawned per request. Multiple requests to the same upstream are multiplexed over the single persistent H2 connection.

---

## Event Loop

Single-threaded epoll. No Tokio.

- No heap allocation on hot path
- No `unwrap()` or `expect()`
- No blocking calls in event loop
- Cache behind `Arc`, swapped atomically — never mutated in place
- inotify fd in same epoll set as network fds
- H2C client pool driven in the event loop alongside TLS/QUIC listeners

---

## Logging

Structured JSON to stdout. journald captures via systemd.

Emitted events: startup, request complete (path, status, backend, latency_us, cache_hit), auth failure (warn), pool change (info), config reload (info), cache eviction (debug).

Periodic stats: `requests`, `rps_avg`, `rps_peak`, `latency_p50_us`, `latency_p99_us`, `cache_hits`, `cache_misses`, `cache_hit_rate`, `backend_errors`, `pool_members`.

---

## Dependencies

`quinn` (HTTP/3), `rustls` (TLS), `matchit` (routing), `bytes` (zero-copy), `serde`+`toml` (config), `inotify`, `jsonwebtoken` (JWT verification).
