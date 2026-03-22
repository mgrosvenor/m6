# m6

m6 is a platform for building and deploying fast websites — covering the full stack from auth to rendering to delivery. Start with a single `site.toml` and get TLS, routing, in-process caching, and JWT authentication out of the box. Add Tera templates, static files, or custom renderers in any language as your site grows. Deploy to a single machine, scale to multiple processes, or fan out to a global edge fleet. Either way, pages load in sub-millisecond time.

---

## System Architecture

```
                         ┌──────────────────────────────────────────────┐
  Browser / API client   │                  m6-http                     │
─────────────────────►   │  - TLS termination (HTTP/3 + HTTP/2 + H1.1) │
  HTTPS (any port)       │  - Routing (site.toml)                       │
                         │  - Response cache (path × encoding)          │
                         │  - JWT auth enforcement (local verify)        │
                         │  - Backend pool management (least-conn)       │
                         │  - Hot-reload (config + certs + pools)        │
                         └────┬──────────────┬──────────────┬───────────┘
                              │              │              │
                    Unix socket         Unix socket     URL backend
                    (same host)         (same host)     http:// https://
                    /run/m6/*.sock      /run/m6/*.sock  h2c:// h2s://
                              │              │         (remote / edge node)
          ┌───────────────────┼──────────────┘
          ▼                   ▼                   ▼
    ┌──────────┐       ┌────────────┐     ┌──────────────┐   ┌─────────────┐
    │ m6-html  │       │  m6-file   │     │  m6-auth     │   │   custom    │
    │          │       │            │     │  -server     │   │  renderer   │
    │ Tera     │       │ Static     │     │              │   │  (any lang) │
    │ template │       │ files      │     │ JWT issue    │   │             │
    │ renderer │       │ Brotli/    │     │ JWT refresh  │   │ Rust:       │
    │          │       │ gzip       │     │ Login/logout │   │ m6-render   │
    │ Hot      │       │ Symlink    │     │ Rate limit   │   │ library     │
    │ reload   │       │ guard      │     │ Key watch    │   │             │
    └────┬─────┘       └────────────┘     └──────┬───────┘   └─────────────┘
         │  m6-render library (Rust)             │  m6-auth library (Rust)
         └───────────────────────────────────────┘
                                                 │
                                          ┌──────▼──────┐
                                          │  SQLite DB  │
                                          │  WAL mode   │
                                          │  bcrypt pw  │
                                          └─────────────┘
```

### Request Flow (cache miss)

```
Client → m6-http (epoll, single-thread)
           │
           ├─ JWT check (local RSA/EC verify, no network hop)
           │
           ├─ Route → backend pool (least-conn; Unix socket or URL)
           │
           ├─ Renderer handles request (thread pool, blocking I/O)
           │
           └─ m6-http: inspect Cache-Control, store entry, return response
```

### Deployment Tiers

| Tier | Components | Use case |
|------|-----------|---------|
| 1 | m6-http + m6-html + m6-file | Static sites. No build step. Copy files to deploy. |
| 2 | Tier 1 + external content tool (e.g. `m6-md`) | Generated static sites. m6 has no build step. |
| 3 | Tier 2 + custom renderers | Dynamic sites: forms, APIs, CMSes. Any language. |
| 4 | Tier 3 × N nodes + URL backends | Global edge fleet. Edge nodes proxy to origin via h2s://. |

---

## Components

### m6-http

Reverse proxy, cache, and router. The only process that listens on a public port.

**Architecture:**
- Single-threaded `epoll` event loop (no Tokio, no async runtime)
- HTTP/3 over QUIC (`quiche`) and HTTP/2 + HTTP/1.1 on the same port
- Response cache keyed by `(path, content-encoding)` — each encoding variant cached independently
- JWT verified locally with m6-auth's public key — no per-request network hop
- Backend pools: Unix socket globs (local) or URL backends (`http://`, `https://`, `h2c://`, `h2s://`)
- inotify watches `/run/m6/` for pool membership changes (new sockets added without config reload)
- `site.toml` and TLS cert/key watched via inotify — hot-reload with no restart
- `Cache-Control: public` responses cached; `no-store` / `private` skipped
- Error pages fetched from configured `[errors] path` backend on 4xx/5xx

**Performance design:**
- Zero heap allocation on the hot path (cached responses)
- Stack-allocated header buffer (8 KiB) for request parsing
- `write_decimal` uses a 20-byte stack buffer instead of `format!`
- `Arc` cache swapped atomically — never mutated in place

**Benchmark results (release, Apple M4, loopback, n=2000):**

End-to-end latency (sequential requests, persistent connection where applicable):

| Protocol | p50 | p99 | Notes |
|----------|-----|-----|-------|
| HTTP/1.1 | 0.198 ms | 0.444 ms | New TLS conn per request |
| HTTP/2   | 0.048 ms | 0.131 ms | Persistent TLS conn |
| HTTP/3   | 0.031 ms | 0.092 ms | Persistent QUIC conn |

HTTP/3 cache effect (p50, path suite):

| Path | p50 | Notes |
|------|-----|-------|
| cache-hit→m6-file  | 0.029 ms | Served from m6-http memory cache |
| cache-miss→m6-file | 0.064 ms | Forwarded to m6-file backend |
| cache-hit→m6-html  | 0.048 ms | Served from m6-http memory cache |
| cache-miss→m6-html | 0.059 ms | Forwarded to m6-html renderer |

Throughput (8 concurrent threads, 10 s, GET `/`):

| Protocol | req/s |
|----------|-------|
| HTTP/1.1 | 8,840 |
| HTTP/2   | 28,797 |
| HTTP/3   | 61,748 |

Criterion microbenchmarks (CPU cost, no I/O):

| Operation | Median | Notes |
|-----------|--------|-------|
| `make_lookup_key` | 10.4 ns | URL + encoding → cache key |
| `cache_hit` | 21.5 ns | LRU lookup, entry present |
| `cache_miss` | 14.7 ns | LRU lookup, entry absent |
| `stats_record` | **0.9 ns** | Per-request counter update |
| `h3_header_extract` | 6.4 ns | Scan 6 HTTP/3 headers for path/method/encoding |
| `full_cache_hit_path` | 22.6 ns | Key + hit combined |

---

### m6-html

Tera template renderer. Handles all HTML routes.

**Architecture (via m6-render library):**
- Fixed thread pool (default: CPU count) + bounded request queue
- `FrameworkState` in `Arc<RwLock<...>>` — hot-reloaded atomically on config change (~1 s mtime poll)
- Two-tier state: `Global` (shared, `Arc` clone per request) and `ThreadLocal` (zero sync overhead)
- JSON params loaded from disk at startup; static params stored as `Arc<Map>` — pointer clone per request, no Map copy
- Dynamic `{stem}`-keyed params resolved and LRU-cached
- Accept-Encoding checked with zero-alloc byte windowing (`ae_contains`)
- Minification pipeline (applied before compression): HTML (`minify-html`), CSS (in-house), JSON (`serde_json`), JS (`minify-js` / parse-js engine, falls back to original bytes on parse failure)
- Brotli and gzip compression with configurable levels per MIME type

**Performance design:**
- `Arc<Map>` params cache: pointer clone per request instead of full Map allocation
- Route matching: Vec linear scan, ~42–83 ns (faster than HashMap for 3–8 routes)
- Template render: ~200 ns (Tera cached compiled template)
- Minification runs once per asset, result is then compressed and cached by m6-http
- Socket round-trip: ~13 µs

**Benchmark results (release, Apple M4, criterion + percentile reporter):**

Core path:

| Operation | p50 | p99 | avg | stddev |
|-----------|-----|-----|-----|--------|
| compile_pattern | 42 ns | 84 ns | 61 ns | 21 ns |
| find_route (exact) | 42 ns | 83 ns | 40 ns | 10 ns |
| find_route (param) | 83 ns | 125 ns | 87 ns | 13 ns |
| match_route | 42 ns | 84 ns | 47 ns | 14 ns |
| parse_query | 208 ns | 250 ns | 209 ns | 20 ns |
| parse_cookies | 208 ns | 250 ns | 212 ns | 14 ns |
| template_render | 208 ns | 250 ns | 217 ns | 20 ns |
| response_write | 83 ns | 125 ns | 90 ns | 16 ns |
| socket_round_trip | 13.0 µs | 21.1 µs | 13.7 µs | 3.2 µs |

Compression (criterion median, per-request cost before caching):

| Compressor | Input | Level | Median |
|------------|-------|-------|--------|
| brotli | HTML 2 KB | 1 (fast) | 9.9 µs |
| brotli | HTML 2 KB | 6 (default) | 110 µs |
| brotli | HTML 2 KB | 11 (max) | 1.50 ms |
| brotli | CSS 8 KB | 1 | 13.3 µs |
| brotli | CSS 8 KB | 6 | 119 µs |
| brotli | CSS 8 KB | 11 | 2.56 ms |
| gzip | HTML 2 KB | 1 | 26 µs |
| gzip | HTML 2 KB | 6–9 | 26–33 µs |
| gzip | CSS 8 KB | 1 | 13 µs |
| gzip | CSS 8 KB | 6–9 | 31–34 µs |

Minification (criterion median):

| Minifier | Input | Median |
|----------|-------|--------|
| HTML (`minify-html`) | 2 KB | 10.8 µs |
| CSS (in-house) | 8 KB | 6.1 µs |
| JSON (`serde_json`) | 1 KB | 1.2 µs |
| JS (`minify-js`) | 3 KB | 51.7 µs |

Full minify → brotli-6 pipeline (one-time cost, result cached by m6-http):

| Asset | Median |
|-------|--------|
| HTML 2 KB | 122 µs |
| CSS 8 KB | 124 µs |
| JS 3 KB | 180 µs |

---

### m6-file

Static file server. Serves assets, downloads, and any file from the site directory.

**Architecture:**
- Fixed thread pool + bounded request queue
- Route matching with `{relpath}` (catchall) and `{stem}` (single segment) params
- Param validation: alphanumeric + `-_.` for stem; subdirs allowed for relpath; `..` → 404
- Fast symlink guard: `symlink_metadata` check only; `canonicalize` only called when a symlink is found (avoids ~15 µs syscall on every request for normal files)
- Brotli and gzip compression applied per-request; m6-http caches the compressed response
- No cache in m6-file — all caching is m6-http's responsibility
- Always returns `Cache-Control: public`

**Benchmark results (release, Apple M4, criterion + percentile reporter):**

| Operation | p50 | p99 | avg | stddev |
|-----------|-----|-----|-----|--------|
| route_match | 63 ns | 84 ns | 73 ns | 24 ns |
| handle_request (disk read + compress) | 10.6 µs | 13.9 µs | 10.9 µs | 1.0 µs |
| socket_round_trip | 27.9 µs | 34.7 µs | 27.6 µs | 3.2 µs |

---

### m6-auth-server

Auth service. Issues and verifies JWTs, manages sessions.

**Architecture:**
- Four HTTP endpoints: `POST /auth/login`, `POST /auth/refresh`, `POST /auth/logout`, `GET /auth/public-key`
- RS256 or ES256 JWT signing (EC recommended); access token TTL 15 min, refresh token TTL 30 days
- Key pair watched via inotify — key rotation without restart
- Rate limiting: 5 login attempts / 15 min / IP
- Delegates credential and ACL storage to m6-auth library (SQLite, WAL mode)
- Never logs passwords, tokens, or key material

---

### m6-auth (library)

Shared library crate used by `m6-auth-server` and `m6-auth-cli`.

- SQLite with WAL mode — multiple processes may open simultaneously
- Automatic schema migration on `Db::open`
- bcrypt password hashing
- User, group, role, and refresh-token management
- Sync API (matches `rusqlite`)

---

### m6-render (library)

Framework library for writing custom renderers in Rust. Used by `m6-html` and any Rust custom renderer crate.

- `App::new()` / `App::with_global()` for stateful renderers
- Request routing with path parameters (`{id}`, `{relpath}`)
- `Request` / `Response` / `Error` types
- Thread pool + bounded queue managed by the framework
- Hot-reload triggered by config file mtime changes

```rust
fn main() {
    App::with_global(init_global)
        .route_get("/api/items",       handle_list)
        .route_post("/api/items",      handle_create)
        .route_get("/api/items/{id}",  handle_get)
        .run().unwrap();
}
```

---

### m6-auth-cli

Bootstrap and management CLI. Operates directly on SQLite — works whether the server is running or not.

```
m6-auth-cli <config> user add <username> [--role <role>]... [--password <pw>]
m6-auth-cli <config> user del <username>
m6-auth-cli <config> user passwd <username>
m6-auth-cli <config> user ls [--json]
m6-auth-cli <config> group add <group>
m6-auth-cli <config> group del <group>
m6-auth-cli <config> group member add <group> <user>
m6-auth-cli <config> group member del <group> <user>
m6-auth-cli <config> group ls [--json]
m6-auth-cli <config> token create <username> [--name <n>] [--ttl-days <d>]
m6-auth-cli <config> token ls <username> [--json]
m6-auth-cli <config> token revoke <token-id>
```

---

## Site Directory Structure

```
my-site/
├── site.toml           ← routes, backends, cache, auth, error config
├── configs/
│   ├── m6-html.conf    ← template routes, global params, compression
│   ├── m6-file.conf    ← file routes, compression levels
│   └── m6-auth.conf    ← storage, token TTLs, key paths
├── templates/          ← Tera templates
├── assets/             ← static files (CSS, JS, images)
├── content/            ← pre-built JSON (populated by tool or renderer)
└── data/               ← auxiliary data files (auth DB, etc.)
```

All processes log structured JSON to stdout — systemd captures via journald.

---

## Configuration (site.toml)

```toml
[site]
name   = "My Site"
domain = "example.com"

[server]
bind     = "0.0.0.0:443"
tls_cert = "/etc/m6/cert.pem"
tls_key  = "/etc/m6/key.pem"

[[backend]]
name    = "m6-html"
sockets = "/run/m6/m6-html-*.sock"

[[backend]]
name    = "m6-file"
sockets = "/run/m6/m6-file-*.sock"

# URL backend: forward to a remote origin or edge node
[[backend]]
name = "origin"
url  = "h2s://origin.example.com:443"

[[route]]
path    = "/"
backend = "m6-html"

[[route]]
path    = "/assets/{relpath}"
backend = "m6-file"

[[route]]
path    = "/admin/{page}"
backend = "m6-html"
require = "group:admin"

[errors]
mode = "custom"
path = "/_errors"

[auth]
public_key = "/run/m6/auth.pub"
```

---

## Process Management

All processes are independent. m6-http does not start or monitor anything.

```bash
# Development (shell script, see m6-examples/m6-run-eg)
m6-auth-server  $SITE_DIR $AUTH_CONF &
m6-html         $SITE_DIR $HTML_CONF &
m6-file         $SITE_DIR $FILE_CONF &
m6-http         $SITE_DIR $SITE_TOML &
wait
```

**Scaling:** start additional instances (e.g. `m6-html-2.service`). The socket `/run/m6/m6-html-2.sock` appears; m6-http detects it via inotify and adds it to the pool. No config change needed.

**Global deployment:** run m6-http at each edge location. Configure it with a `h2s://` URL backend pointing to the origin. Each edge node caches independently. See [Example 09](docs/m6-user-guide.md#example-09--global-deployment) and the Vultr multi-region deployment walkthrough.

---

## Auth Flow

```
Browser                  m6-http                  m6-auth-server
   │                        │                            │
   │  GET /admin/dashboard  │                            │
   │───────────────────────►│                            │
   │                        │ verify JWT (local, no RPC) │
   │                        │  – expired session cookie  │
   │                        │  – valid refresh cookie    │
   │                        │                            │
   │  302 → POST /auth/refresh                           │
   │◄───────────────────────│                            │
   │                        │                            │
   │  POST /auth/refresh ───────────────────────────────►│
   │                        │                   new JWT  │
   │                        │◄───────────────────────────│
   │  302 → /admin/dashboard│ set session+refresh cookies│
   │◄───────────────────────│                            │
   │                        │                            │
   │  GET /admin/dashboard  │                            │
   │───────────────────────►│                            │
   │                        │ verify JWT (local) ✓       │
   │                        │ forward X-Auth-Claims      │
   │                        │──────────────────────────► m6-html
```

---

## Hot Reload

| Component | Trigger | What reloads |
|-----------|---------|-------------|
| m6-http | `site.toml` changed (inotify) | Routes, backends, cache invalidation map |
| m6-http | Socket appears/disappears in `/run/m6/` | Backend pool membership |
| m6-http | TLS cert/key changed (inotify) | TLS context (including QUIC) |
| m6-auth-server | Key file changed (inotify) | JWT signing key |
| m6-html | Config file written (inotify on Linux, mtime poll on macOS) | Templates, routes, params |

---

## Error Handling

```
Backend 4xx/5xx  →  m6-http fetches GET /_errors?status=N&from=<path>
                 →  returns rendered HTML with original status code

Pool empty       →  status per [errors] mode:
                      "status"   — status code, empty body
                      "internal" — status code, minimal HTML
                      "custom"   — status code, error page from [errors] path
```

---

## Security

- TLS always required; m6-http terminates; internal communication over Unix sockets or TLS URL backends
- JWT verified locally on every request — no per-request network hop to m6-auth
- Path traversal: `..` in any URL path → 404; `..` in a route param → 400
- Symlink guard: resolves symlinks at request time; symlinks escaping `site_dir` → 404
- Rate limiting on login: 5 attempts / 15 min / IP
- `site.toml` contains no secrets; secrets live in system config or `secrets_file`

---

## Exit Codes

| Code | Meaning |
|------|---------|
| 0 | Clean shutdown (SIGTERM / SIGINT) |
| 1 | Runtime error |
| 2 | Config or usage error (before bind) |

---

## Known Limitations

**m6-http:**
- The cache invalidation map is built only from `[[route_group]]` globs; the second source (renderer config `params` declarations → affected route paths) is not yet parsed
- No explicit timeout on backend calls; a stalled backend blocks one event loop iteration

**m6-auth-server:**
- Rate-limit state is in-memory per process; resets on restart and is not shared across multiple instances
- When m6-auth sits behind m6-http, rate limiting falls back to a single "unix" bucket if `X-Forwarded-For` / `X-Real-IP` headers are absent

**Out of scope (by design):**
- Windows
- Rate limiting outside of m6-auth login
- Built-in OAuth2 / OIDC provider
- MFA / WebAuthn
- Horizontal scaling of m6-http itself (scales vertically via caching; global scale via edge nodes)

---

## Comparative positioning

m6-http is a **single-threaded, in-process-cached, TLS-terminating reverse proxy**.
The response cache is an `Arc<Bytes>` LRU in the same heap as the TLS stack — a
cache hit is a hash lookup, a reference-count increment, and an AES-GCM seal. No
IPC, no lock, no copy.

**Measured single-core throughput (macOS loopback, TLS, warm cache, 8 concurrent connections):**

| Protocol | req/s   | vs nginx (1 worker) |
|----------|--------:|---------------------|
| HTTP/1.1 |  11,857 | ~0.5–0.9× (nginx better; H1 not the primary path) |
| HTTP/2   | 158,323 | ~3–9× faster |
| HTTP/3   |  77,672 | no published baseline |

**HTTP/2 context:** nginx single-worker H2 reaches ~17–59K req/s; LiteSpeed ~84K.
m6-http at 158K is the result of correct H2 flow-control (`WINDOW_UPDATE`), stream
multiplexing, and the in-process cache. H2O on Linux with kernel TLS reaches ~325K —
the gap is `TCP_ULP` / KTLS offloading AES-GCM into the kernel, a Linux-only feature
not yet integrated here. The profiler shows ~56% of H2 working time is in userspace
AES-GCM; KTLS would eliminate most of it.

**Where m6 wins:** H2/H3 single-core throughput; zero-IPC cache hits; deterministic
tail latency (no lock convoys, no cross-core coherence); no external cache tier needed.

**Where m6 doesn't win:** H1 throughput (nginx is more mature); multi-core scale
within a single instance (one process = one core); large-file serving (no `sendfile`).

See [`docs/POSITIONING.md`](docs/POSITIONING.md) for the full analysis and [`docs/BENCHMARKS.md`](docs/BENCHMARKS.md) for raw numbers.

---

## Documentation

All detailed documentation lives in [`docs/`](docs/):

| Document | Contents |
|---|---|
| [`docs/m6-user-guide.md`](docs/m6-user-guide.md) | Eleven worked examples, start here |
| [`docs/m6-site-toml.md`](docs/m6-site-toml.md) | `site.toml` reference |
| [`docs/m6-http.md`](docs/m6-http.md) | m6-http internals |
| [`docs/m6-auth.md`](docs/m6-auth.md) | m6-auth-server reference |
| [`docs/m6-auth-cli.md`](docs/m6-auth-cli.md) | m6-auth-cli reference |
| [`docs/m6-auth-lib.md`](docs/m6-auth-lib.md) | m6-auth library API |
| [`docs/m6-render.md`](docs/m6-render.md) | m6-render custom renderer guide |
| [`docs/m6-render-lib.md`](docs/m6-render-lib.md) | m6-render library API |
| [`docs/m6-md.md`](docs/m6-md.md) | m6-md Markdown processor |
| [`docs/BENCHMARKS.md`](docs/BENCHMARKS.md) | Raw benchmark numbers |
| [`docs/POSITIONING.md`](docs/POSITIONING.md) | Competitive analysis |

---

## Building

```sh
cargo build --release --workspace
```

Requires Rust 1.75+. Production target: Linux (epoll, inotify). macOS supported for development.

For development TLS: [mkcert](https://github.com/FiloSottile/mkcert).
