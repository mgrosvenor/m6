# m6

A family of composable Unix processes for serving websites. Each process has one job. They communicate over Unix sockets and are wired together via `site.toml`.

---

## System Architecture

```
                         ┌─────────────────────────────────────────────┐
  Browser / API client   │                  m6-http                    │
─────────────────────►   │  - TLS termination (always-on)              │
  HTTPS :443             │  - Routing (site.toml)                      │
  HTTP/3 + HTTP/1.1      │  - Response cache (path × encoding)         │
                         │  - JWT auth enforcement (local verify)       │
                         │  - Backend pool management (least-conn)      │
                         │  - inotify hot-reload (config + certs + keys)│
                         └──────────┬───────────────────────────────────┘
                                    │  Unix sockets  /run/m6/*.sock
               ┌────────────────────┼──────────────────────────┐
               ▼                    ▼                          ▼
         ┌──────────┐        ┌──────────────┐          ┌─────────────┐
         │ m6-html  │        │   m6-file    │          │  m6-auth    │
         │          │        │              │          │  -server    │
         │ Tera     │        │ Static files │          │             │
         │ template │        │ Compression  │          │ JWT issue   │
         │ renderer │        │ (br/gzip)    │          │ JWT refresh │
         │          │        │ Route match  │          │ Login/logout│
         │ Hot      │        │ Symlink guard│          │ Rate limit  │
         │ reload   │        │              │          │ Key watch   │
         └──────────┘        └──────────────┘          └──────┬──────┘
               │                                              │
               │  m6-render library (shared)                  │
               └──────────────────────────────────────────────┘
                                                              │
                                                       ┌──────▼──────┐
                                                       │  m6-auth    │
                                                       │  (SQLite)   │
                                                       │  WAL mode   │
                                                       │  bcrypt pw  │
                                                       └─────────────┘

  User-supplied renderers (any language, declared as [[backend]] in site.toml)
  are managed by systemd alongside m6-html, m6-file, m6-auth-server.
```

### Request Flow (cache miss)

```
Client → m6-http (epoll, single-thread)
           │
           ├─ JWT check (local RSA/EC verify, no network hop)
           │
           ├─ Route → backend pool (least-connections over Unix sockets)
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
| 3 | Tier 2 + user renderers | Dynamic sites: forms, APIs, CMSes. Any language. |

---

## Components

### m6-http

Reverse proxy, cache, and router. The only process that listens on a public port.

**Architecture:**
- Single-threaded `epoll` event loop (no Tokio, no async runtime)
- HTTP/3 over QUIC (`quiche`) and HTTP/1.1 on the same port
- Response cache keyed by `(path, content-encoding)` — each encoding variant cached independently
- JWT verified locally with m6-auth's public key — no per-request network hop
- Backend pools declared as Unix socket globs; inotify watches `/run/m6/` for pool membership changes
- `site.toml` and TLS cert/key watched via inotify — hot-reload with no restart
- `Cache-Control: public` responses cached; `no-store` / `private` skipped
- Error pages fetched from configured `[errors] path` backend on 4xx/5xx

**Performance design:**
- Zero heap allocation on the hot path (cached responses)
- Stack-allocated header buffer (8 KiB) for request parsing
- `write_decimal` uses a 20-byte stack buffer instead of `format!`
- `Arc` cache swapped atomically — never mutated in place

**Benchmark results (release, Apple M4, criterion + percentile reporter):**

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
- Fixed thread pool + bounded request queue (via m6-render framework)
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

Shared library crate used by `m6-auth-server`, `m6-auth-cli`, and custom renderers.

- SQLite with WAL mode — multiple processes may open simultaneously
- Automatic schema migration on `Db::open`
- bcrypt password hashing
- User, group, role, and refresh-token management
- Sync API (matches `rusqlite`)

---

### m6-auth-cli

Bootstrap and management CLI. Operates directly on SQLite — works whether the server is running or not.

```
m6-auth-cli user add <username>
m6-auth-cli user del <username>
m6-auth-cli user passwd <username>
m6-auth-cli user ls [--json]
m6-auth-cli group add <group>
m6-auth-cli group del <group>
m6-auth-cli group member add <group> <user>
m6-auth-cli group member del <group> <user>
m6-auth-cli group ls [--json]
```

---

## Site Directory Structure

```
my-site/
├── site.toml           ← routes, backends, cache, auth, error config
├── configs/
│   ├── m6-html.conf    ← template routes, global params, compression
│   ├── m6-file.conf    ← file routes, compression levels
│   └── system-dev.toml ← bind address and TLS paths (not version-controlled)
├── templates/          ← Tera templates
├── assets/             ← static files (CSS, JS, images)
├── content/            ← pre-built JSON (populated by tool or renderer)
└── data/               ← auxiliary data files
```

No binaries, no `logs/` directory. All processes log structured JSON to stdout — systemd captures via journald.

---

## Configuration (site.toml)

```toml
[site]
name   = "My Site"
domain = "example.com"

[server]
# In system config (not here) — bind address and TLS paths

[[backend]]
name    = "m6-html"
sockets = "/run/m6/m6-html-*.sock"

[[backend]]
name    = "m6-file"
sockets = "/run/m6/m6-file-*.sock"

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

All processes are managed by systemd. m6-http does not start or monitor anything.

```
# Development (shell script)
m6-auth-server  $SITE_DIR $AUTH_CONF &
m6-html         $SITE_DIR $HTML_CONF &
m6-file         $SITE_DIR $FILE_CONF &
m6-http         $SITE_DIR $SYSTEM_CONF &
wait
```

Scaling: start additional systemd instances (e.g. `m6-html-2.service`). The socket `/run/m6/m6-html-2.sock` appears; m6-http detects it via inotify and adds it to the pool automatically. No config change needed.

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
| m6-http | TLS cert/key changed | TLS context |
| m6-auth-server | Key file changed (inotify) | JWT signing key |
| m6-html / m6-file | Config file written (inotify, Linux); mtime poll fallback on non-Linux | FrameworkState (routes, templates, params) |

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

- TLS always required; m6-http terminates; Unix sockets between processes (no internal TLS)
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

## Known Limitations (v1)

These are tracked gaps between the current implementation and the full spec:

**m6-http:**
- URL backends (single-upstream HTTP/HTTPS with ALPN) are not yet implemented; only Unix socket backends are supported
- TLS cert/key changes are detected via inotify but do not trigger a quiche TLS context reload at runtime
- The cache invalidation map is built only from `[[route_group]]` globs; the second source (renderer config `params` declarations → affected route paths) is not yet parsed
- No explicit timeout on backend Unix socket calls; a stalled backend blocks one event loop iteration

**m6-html / m6-file:**
- On non-Linux platforms, hot-reload falls back to mtime polling (~1 s); Linux uses inotify

**m6-auth-server:**
- Rate-limit state is in-memory per process; resets on restart and is not shared across multiple instances
- When m6-auth sits behind m6-http over a Unix socket, rate limiting falls back to a single "unix" bucket if `X-Forwarded-For` / `X-Real-IP` headers are absent

**Out of scope for v1 (by design):**
- Windows
- Rate limiting outside of m6-auth login
- Built-in OAuth2 / OIDC provider
- MFA / WebAuthn
- Horizontal scaling of m6-http itself (scales vertically via caching)

---

## Building

```sh
cargo build --release --workspace
```

Requires Rust 1.75+. Linux only (epoll, inotify, Unix sockets).

For development TLS: [mkcert](https://github.com/FiloSottile/mkcert).
