# m6

m6 is a platform for building and deploying fast websites.
It covers the full stack from auth to rendering to delivery.
Start with a single `site.toml` and get *TLS* (Transport Layer Security), routing, in-process caching, and *JWT* (JSON Web Token) authentication out of the box.
Add Tera templates, static files, or custom renderers in any language as your site grows.
Deploy to a single machine, scale to multiple processes, or fan out to a global edge fleet.
Either way, pages load in sub-millisecond time.
The install comes first, then the architecture and components, and the document closes with the reference map.

## Contents

1. [Install](#1-install)
2. [System Architecture](#2-system-architecture)
3. [Components](#3-components)
4. [Site Directory Structure](#4-site-directory-structure)
5. [Configuration (`site.toml` and the system config)](#5-configuration-sitetoml-and-the-system-config)
6. [Process Management](#6-process-management)
7. [Auth Flow](#7-auth-flow)
8. [Error Handling](#8-error-handling)
9. [Security](#9-security)
10. [Exit Codes](#10-exit-codes)
11. [RFC compliance](#11-rfc-compliance)
12. [Known Limitations](#12-known-limitations)
13. [Comparative positioning](#13-comparative-positioning)
14. [Documentation](#14-documentation)
15. [Building](#15-building)

---

## 1. Install

Four steps.
The last one is the point: it runs the whole stack and checks it, so you finish knowing the install works.

**1. Build the binaries.**

```sh
git clone https://github.com/mgrosvenor/m6
cd m6
cargo build --release --workspace
export PATH="$PWD/target/release:$PATH"
```

Rust 1.89 or newer.
Linux is the production target (epoll).
macOS works for development.
Example 11's admin dashboard needs Linux because its shipped config sets `bench_bin` and the bench child is reaped from a pidfd.

**2. A development certificate.** m6-http's public listener is TLS only, and its `h2c_bind` listener is for a private backbone, so a site needs a certificate.
[mkcert](https://github.com/FiloSottile/mkcert) issues one your browser already trusts:

```sh
brew install mkcert        # or your package manager's equivalent
mkcert -install
```

**3. The examples.** They live in their own repository, checked out beside this one, because the renderer crates reach m6 by relative path:

```sh
cd ..
git clone https://github.com/mgrosvenor/m6-examples
cd m6-examples
cargo build --release      # the custom renderers
```

**4. Run the whole stack and check it.**

```sh
cd examples/05-cms
./dev.sh                   # in one terminal: six services come up
./test.sh                  # in another
```

Example 05 is the one that exercises everything at once: m6-http at the edge, m6-html for templates, m6-file for assets, m6-md for markdown, m6-auth-server for logins, and a custom Rust renderer for the CMS.
`test.sh` makes 98 checks against it: both wire protocols, assets including nested paths, compression, the cache, the security headers, every request method, the full draft to publish to unpublish cycle checked against what a visitor actually sees, login, logout, and the login throttle including its recovery.

```
  98 passed  0 failed  (98 checks)
```

Anything other than that is a real answer about your install.
No check is skipped, and every check has exactly one passing result.

Then read [`docs/m6-user-guide.md`](docs/m6-user-guide.md), which walks the eleven examples from a static site up to a global fleet.

### 1.1 If you are changing m6 itself

Build the examples too, and run that suite.
They are where m6's interfaces are actually used, and they are the first place a change to them breaks.
m6's own checks do this for you:

```sh
M6_BUILD_HOST=user@example.com ./tools/build-host-tests.sh
```

See [`CONTRIBUTING.md`](CONTRIBUTING.md).

---

## 2. System Architecture

Figure 1 gives the whole shape of a deployment.
Take from it that only m6-http listens on a public port, and that every other process is reached over a Unix socket or a URL backend.

```
                         ┌──────────────────────────────────────────────┐
  Browser / API client   │                  m6-http                     │
─────────────────────►   │  - TLS termination (HTTP/3 + HTTP/2 + H1.1)  │
  HTTPS (any port)       │  - Routing (site.toml)                       │
                         │  - Response cache (path × encoding)          │
                         │  - JWT auth enforcement (local verify)       │
                         │  - Backend pool management (round robin)     │
                         │  - Config, certs and pools at start          │
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
    │          │       │ Symlink    │     │ Rate limit   │   │ library     │
    │          │       │ guard      │     │ Key rotation │   │             │
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

**Figure 1: the m6 process family and how a request reaches it.**
m6-http terminates TLS and routes, and m6-html, m6-file and a custom renderer answer it over Unix sockets while a URL backend reaches a remote node.

### 2.1 Request Flow (cache miss)

Figure 2 follows one cache-miss request through m6-http.
Take from it that the JWT check, the route choice and the cache store all happen in the one process that terminates TLS.

```
Client → m6-http (one event loop, single thread)
           │
           ├─ JWT check (local RSA/EC verify, no network hop)
           │
           ├─ Route → backend pool (round robin, Unix socket or URL)
           │
           ├─ Renderer handles request (one event loop per process)
           │
           └─ m6-http: inspect Cache-Control, store entry, return response
```

**Figure 2: a cache-miss request through m6-http.**
The JWT check, the route choice and the cache store run in the same event loop, so the backend call is the only network hop on the path.

### 2.2 Deployment Tiers

Table 1 lists the four deployment tiers and the components each one adds.
Take from it that a global edge fleet is tier 3 repeated, with URL backends in place of local ones.

| Tier | Components | Use case |
|------|-----------|---------|
| 1 | m6-http + m6-html + m6-file | Static sites. No build step. Copy files to deploy. |
| 2 | Tier 1 + external content tool (e.g. `m6-md`) | Generated static sites. m6 has no build step. |
| 3 | Tier 2 + custom renderers | Dynamic sites: forms, APIs, CMSes. Any language. |
| 4 | Tier 3 × N nodes + URL backends | Global edge fleet. Edge nodes proxy to origin via h2s://. |

**Table 1: the four deployment tiers and the components each one adds.**
Tier 4 repeats tier 3 across N nodes and reaches the origin through `h2s://` URL backends.

---

## 3. Components

m6 ships four services, two libraries and one command.
Sections 3.1 to 3.7 take them in the order a request meets them: the edge, the HTML and file backends, the auth service, the libraries those backends share, and the command that manages the auth database.
Each entry names what the process owns and what it costs.

### 3.1 m6-http

Reverse proxy, cache, and router.
The only process that listens on a public port.

**Architecture:**
- One thread and one edge-triggered event loop over `m6_core::platform`
- HTTP/3 over QUIC (`quiche`) and HTTP/2 + HTTP/1.1 on the same port
- Response cache keyed by `(path, content-encoding)`, with each encoding variant cached independently
- JWT verified locally with m6-auth's public key, so no request makes a network hop
- Backend pools: Unix socket globs (local) or URL backends (`http://`, `https://`, `h2c://`, `h2s://`)
- Backend sockets are found once at start from each pool's glob, and a change takes a restart
- `site.toml` and the system config are read once at start.
  Restart m6-http after renewing a certificate
- `Cache-Control: public` responses cached.
  `no-store` and `private` responses skipped
- Error pages fetched from configured `[errors] path` backend on 4xx/5xx

**Performance design:**
- Zero heap allocation on the hot path (cached responses)
- Stack-allocated header buffer (8 KiB) for request parsing
- `write_decimal` uses a 20-byte stack buffer instead of `format!`
- `Arc` cache swapped atomically, never mutated in place

**Benchmark results (release, Apple M4, loopback, n=2000):**

Table 2 gives end-to-end latency for sequential requests, with a persistent connection where the protocol allows one.

| Protocol | p50 | p99 | Notes |
|----------|-----|-----|-------|
| HTTP/1.1 | 0.198 ms | 0.444 ms | New TLS conn per request |
| HTTP/2   | 0.048 ms | 0.131 ms | Persistent TLS conn |
| HTTP/3   | 0.031 ms | 0.092 ms | Persistent QUIC conn |

**Table 2: end-to-end latency by protocol, at p50 and p99.**
HTTP/3 is fastest at both percentiles, and HTTP/1.1 is slowest because each request opens a new TLS connection.

Table 3 gives the HTTP/3 cache effect at p50 over the path suite.

| Path | p50 | Notes |
|------|-----|-------|
| cache-hit→m6-file  | 0.029 ms | Served from m6-http memory cache |
| cache-miss→m6-file | 0.064 ms | Forwarded to m6-file backend |
| cache-hit→m6-html  | 0.048 ms | Served from m6-http memory cache |
| cache-miss→m6-html | 0.059 ms | Forwarded to m6-html renderer |

**Table 3: the HTTP/3 cache effect at p50 for one path suite.**
A hit is served from m6-http's memory and a miss pays the backend round trip, for both backends.

Throughput, measured on a 4-vCPU Linux host with the load generator co-located over loopback, TLS 1.3, warm cache, **concurrency 2**, commit `0a68cda`, from two consecutive runs so the spread is visible.
Table 4 gives the result.

| Protocol | req/s |
|----------|-------|
| HTTP/1.1 | 1,889 – 2,085 |
| HTTP/2   | 6,711 – 7,154 |
| HTTP/3   | 1,030 – 1,047 |

**Table 4: throughput by protocol at concurrency 2, over two runs.**
HTTP/2 carries 3.2 to 3.8 times the requests per second of HTTP/1.1, and HTTP/3 is the lowest of the three.

Concurrency 2, not 8, is deliberate: on a 4-core box the load generator and the server share cores, and at 8 the client starves the server.
Two identical runs disagreed by 50×.
Full conditions, command lines and per-percentile latencies are in [`docs/BENCHMARKS.md`](docs/BENCHMARKS.md).
Numbers quoted without those conditions are not meaningful.

Table 5 gives the Criterion microbenchmarks, which measure CPU cost with no I/O.

| Operation | Median | Notes |
|-----------|--------|-------|
| `make_lookup_key` | 10.4 ns | URL + encoding → cache key |
| `cache_hit` | 21.5 ns | LRU lookup, entry present |
| `cache_miss` | 14.7 ns | LRU lookup, entry absent |
| `stats_record` | **0.9 ns** | Per-request counter update |
| `h3_header_extract` | 6.4 ns | Scan 6 HTTP/3 headers for path/method/encoding |
| `full_cache_hit_path` | 22.6 ns | Key + hit combined |

**Table 5: the m6-http critical-path microbenchmarks.**
No operation here costs more than 22.6 ns, so the hot path's own work is tens of nanoseconds.

---

### 3.2 m6-html

Tera template renderer.
Handles all HTML routes.

**Architecture (via m6-render library):**
- One process and one event loop, accepting on listener readiness
- `Site` built once at start, holding the config, the route table and the renderer
- Plain state in `main`, passed to each request arm
- JSON params loaded from disk at startup.
  Static params are stored as `Arc<Map>`, so a request clones a pointer and copies no Map
- Dynamic `{stem}`-keyed params resolved and LRU-cached
- Accept-Encoding checked with zero-alloc byte windowing (`ae_contains`)
- Minification pipeline (applied before compression): HTML (`minify-html`), CSS (in-house), JSON (`serde_json`), JS (`minify-js` / parse-js engine, falls back to original bytes on parse failure)
- Brotli and gzip compression with configurable levels per MIME type

**Performance design:**
- `Arc<Map>` params cache: pointer clone per request instead of full Map allocation
- Route matching: Vec linear scan, ~42–83 ns (faster than HashMap for 3–8 routes)
- Template render: ~200 ns (Tera cached compiled template)
- Minification runs once per cached response, and m6-http then serves the result compressed.
  A `no-store` response is the exception, as the `content` row of Table 11 in [`docs/m6-architecture.md`](docs/m6-architecture.md) records
- Socket round-trip: ~13 µs

**Benchmark results (release, Apple M4, criterion + percentile reporter):**

Table 6 gives the core path.

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

**Table 6: the m6-html core-path microbenchmarks.**
A route match costs 42 to 83 ns and a template render 208 ns at p50, while the socket round trip costs 13.0 µs.

Table 7 gives compression at the criterion median, as a per-request cost before caching.

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

**Table 7: the cost of one compression pass by compressor, input and level.**
Brotli at level 11 costs more than 100 times level 1 on the same input, and gzip stays between 13 and 34 µs.

Table 8 gives minification at the criterion median.

| Minifier | Input | Median |
|----------|-------|--------|
| HTML (`minify-html`) | 2 KB | 10.8 µs |
| CSS (in-house) | 8 KB | 6.1 µs |
| JSON (`serde_json`) | 1 KB | 1.2 µs |
| JS (`minify-js`) | 3 KB | 51.7 µs |

**Table 8: the cost of one minification pass by minifier and input.**
CSS is the cheapest of the four per kilobyte and JavaScript the most expensive.

Table 9 gives the full minify → brotli-6 pipeline, a one-time cost whose result m6-http caches.

| Asset | Median |
|-------|--------|
| HTML 2 KB | 122 µs |
| CSS 8 KB | 124 µs |
| JS 3 KB | 180 µs |

**Table 9: the full minify and brotli-6 pipeline for one asset.**
The pipeline costs 122 to 180 µs once per asset, and m6-http caches the result, so a later hit pays none of it.

---

### 3.3 m6-file

Static file server.
Serves assets, downloads, and any file from the site directory.

**Architecture:**
- One event loop and a connection table sized from the open-file limit
- Route matching with `{relpath}` (catchall) and `{stem}` (single segment) params
- Param validation: alphanumeric plus `-_.` for a stem, subdirectories allowed for a relpath, and `..` answered 404
- Fast symlink guard: a `symlink_metadata` check only.
  `canonicalize` is called only when a symlink is found, which avoids a ~15 µs syscall on every request for normal files
- Brotli and gzip compression applied per request.
  m6-http caches the compressed response
- No cache in m6-file: all caching is m6-http's responsibility
- Always returns `Cache-Control: public`

**Benchmark results (release, Apple M4, criterion + percentile reporter):**

Table 10 gives the m6-file measurements.

| Operation | p50 | p99 | avg | stddev |
|-----------|-----|-----|-----|--------|
| route_match | 63 ns | 84 ns | 73 ns | 24 ns |
| handle_request (disk read + compress) | 10.6 µs | 13.9 µs | 10.9 µs | 1.0 µs |
| socket_round_trip | 27.9 µs | 34.7 µs | 27.6 µs | 3.2 µs |

**Table 10: the m6-file microbenchmarks.**
A disk read and a compression pass cost 10.6 µs at p50, and the socket round trip costs 27.9 µs.

---

### 3.4 m6-auth-server

Auth service.
Issues and verifies JWTs, manages sessions.

**Architecture:**
- Four HTTP endpoints: `POST /auth/login`, `POST /auth/refresh`, `POST /auth/logout`, `GET /auth/public-key`
- RS256 or ES256 JWT signing (EC recommended).
  Access token TTL 15 min, refresh token TTL 30 days
- Key pair read at start, so a key rotation is a restart.
  A key that fails to load, or a private key that is not the pair of the public key, exits 2
- Rate limiting: 5 failed logins per 15 minutes per client address by default, set in `[rate_limit]`
- Delegates credential and ACL storage to m6-auth library (SQLite, WAL mode)
- Never logs passwords, tokens, or key material

---

### 3.5 m6-auth (library)

Shared library crate used by `m6-auth-server` and `m6-auth-cli`.

- SQLite with WAL mode, so multiple processes may open it at the same time
- Automatic schema migration on `Db::open`
- bcrypt password hashing
- User, group, role, and refresh-token management
- Sync API (matches `rusqlite`)

---

### 3.6 m6-render (library)

Library for writing custom renderers in Rust.
Used by `m6-html` and any Rust custom renderer crate.

- `Site` for routing, the request dictionary, templates and the site-wide finish
- Code routes as `CodeRoute` values, answered from one `match` on `Handled`
- `ServerConn`, `Signals`, `Timeouts`, `Slots` and the reactor as the parts a service's own loop uses
- `Request` / `Response` / `Error` types
- Configuration read once at start, and a change takes a restart

`docs/m6-app-anatomy.md` walks through the loop a renderer writes.

---

### 3.7 m6-auth-cli

Bootstrap and management CLI.
Operates directly on SQLite, so it works whether the server is running or not.

```
m6-auth-cli <config> user ls [--json]
m6-auth-cli <config> user add <username> [--role <role>]... [--password <pw>]
m6-auth-cli <config> user del <username>
m6-auth-cli <config> user passwd <username> [--password <pw>]
m6-auth-cli <config> user roles <username> [--set <role>]... [--unset <role>]...
m6-auth-cli <config> group ls [--json]
m6-auth-cli <config> group add <name>
m6-auth-cli <config> group del <name>
m6-auth-cli <config> group member ls <group> [--json]
m6-auth-cli <config> group member add <group> <username>
m6-auth-cli <config> group member del <group> <username>
m6-auth-cli <config> token create <username> [--name <name>] [--ttl-days <days>]
m6-auth-cli <config> token ls <username> [--json]
m6-auth-cli <config> token revoke <token-id>
```

---

## 4. Site Directory Structure

Figure 3 gives the layout of one site directory.
Take from it that configuration, templates, assets and data each have one place.

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

**Figure 3: the site directory layout.**
The top two entries are configuration, the middle three are the files a backend serves, and `data/` holds state such as the auth database.

All processes log structured JSON to stdout, which systemd captures via journald.

---

## 5. Configuration (`site.toml` and the system config)

A site is described by two files.
`site.toml` holds the state every node shares: the site's name and domain, the backends, the routes, the error mode and the log level.
The system config holds this node's own state: its name, the listener, the certificate, its private key, and the public key that verifies tokens.
m6-http reads both, and a key written in the wrong file stops the start with exit 2, as does a system config with no `[node]`.

`site.toml`:

```toml
[site]
name   = "My Site"
domain = "example.com"

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

[[route]]
path    = "/_errors"
backend = "m6-html"

[errors]
mode = "custom"
path = "/_errors"
```

The system config, which the examples keep at `configs/system-dev.toml`:

```toml
[node]
name = "edge-a"

[server]
bind     = "0.0.0.0:443"
tls_cert = "/etc/m6/cert.pem"
tls_key  = "/etc/m6/key.pem"

[auth]
public_key = "/run/m6/auth.pub"
```

Every path in the system config is absolute, and a relative one is a startup error.
[`docs/m6-site-toml.md`](docs/m6-site-toml.md) gives every key and names the one file that owns it.

---

## 6. Process Management

Each process is started and supervised on its own by the service manager, systemd on the production target.
Backends must start before m6-http (in systemd, `After=` on the backend units in m6-http's unit), because m6-http reads each pool's sockets once at start and a pool whose glob matches no socket exits 2.
A backend that restarts on the same socket path serves again on its next connect, and a new instance joins its pool when m6-http restarts.

```bash
# Development (shell script, see m6-examples/m6-run-eg)
m6-auth-server  $SITE_DIR $AUTH_CONF &
m6-html         $SITE_DIR $HTML_CONF &
m6-file         $SITE_DIR $FILE_CONF &
m6-http         $SITE_DIR $SITE_TOML &
wait
```

**Scaling:** start additional instances (e.g. `m6-html-2.service`).
m6-http finds each pool's sockets once at start, so a new instance joins its pool when m6-http restarts.

**Global deployment:** run m6-http at each edge location.
Configure it with a `h2s://` URL backend pointing to the origin.
Each edge node caches independently.
See [Example 09](docs/m6-user-guide.md#10-example-09-global-deployment) and the multi-region deployment walkthrough.

---

## 7. Auth Flow

Figure 4 follows a browser login and token refresh.
Take from it that only login and refresh reach m6-auth-server, and that every other request verifies the token locally.

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

**Figure 4: a browser login and token refresh.**
An expired session cookie is answered 302 to the refresh endpoint, the new token becomes a session cookie, and m6-http forwards the claims to the backend.

---

## 8. Error Handling

Figure 5 gives the error path.
Take from it that a custom error page is fetched from a backend, and that a failed fetch falls back to the built-in page.

```
Backend 4xx/5xx  →  m6-http fetches GET /_errors?status=N&from=<path>
                 →  returns rendered HTML with original status code

Pool empty       →  status per [errors] mode:
                      "status"   -> status code, empty body
                      "internal" -> status code, minimal HTML
                      "custom"   -> status code, error page from [errors] path
```

**Figure 5: how a backend failure becomes a client response.**
A backend 4xx or 5xx is rendered from the configured error route, and an empty pool follows `[errors] mode`.

---

## 9. Security

Each item below is enforced by the process that owns that step, so no route has to repeat a check.
The last is a rule about what `site.toml` may hold.

- TLS always required, and m6-http terminates it.
  Internal communication runs over Unix sockets or TLS URL backends
- JWT verified on every request against m6-auth's public key, which m6-http holds locally, so verification costs no network hop to m6-auth.
  Only login and refresh reach m6-auth-server
- Path traversal: `..` in any URL path answered 404, and `..` in a route param answered 400
- Symlink guard: resolves symlinks at request time, and a symlink escaping `site_dir` is answered 404
- Rate limiting on login: 5 attempts / 15 min / IP
- `site.toml` contains no secrets.
  Secrets live in the system config or a `secrets_file`

---

## 10. Exit Codes

Table 11 lists the exit codes.

| Code | Meaning |
|------|---------|
| 0 | Clean shutdown (SIGTERM / SIGINT) |
| 1 | Runtime error |
| 2 | Config or usage error (before bind) |

**Table 11: the exit codes every m6 process uses.**
A supervisor tells a bad config from a crash by the code alone.

---

## 11. RFC compliance

m6 was audited clause-by-clause against RFC 9110/9111/9112/9113/9114 in September 2026: **180 checks, 38 passing, 119 failing, 23 ambiguous.**

Everything below has since been fixed, with tests, and verified against a running server:

**Message framing and smuggling**
- Bare `LF` accepted as a line terminator on ingress (`httparse` permits it)
- **H2/H3 header values written verbatim into backend HTTP/1.1.** CR/LF in a decoded value could inject a whole request into a trusting backend (F036/F094)
- Backend responses: conflicting `Content-Length` resolved by position.
  `Transfer-Encoding` + `Content-Length` both accepted.
  Only a literal `chunked` recognised, so `gzip, chunked` was misframed.
  An invalid length silently became read-to-EOF (F028–F031)
- Chunked decoding: post-chunk CRLF optional.
  Trailer section never validated.
  Chunk sizes accepted a leading `+`.
  **No body cap on the chunked path** at all
- The proxy emitted **two conflicting `Content-Length` headers** of its own
- `Host` validation, absolute-form targets, oversized headers

**Semantics (RFC 9110)**
- Method never validated: `TRACE`, `POST` and an invented `FOO` were all served the cached homepage.
  405 vs 501 not distinguished
- HEAD returned a full body while advertising the GET length.
  curl reported error 18 and HTTP/2 aborted the stream
- Preconditions: `If-Match` and `If-Unmodified-Since` ignored entirely (so a conditional write was applied unconditionally), `If-None-Match` used strong instead of weak comparison, evaluation order not implemented (F003/F006/F008/F010)
- `Accept-Encoding` matched by substring, so `br;q=0`, an explicit refusal, still selected brotli.
  Q-values and preference ignored (F012–F014)
- No `Via` on forwarded messages (F017).
  `Connection`-nominated fields forwarded (F018/F019)

**Caching (RFC 9111)**
- No `Age` and no `Date`.
  Freshness unbounded for a bare `public`
- Age taken from the `Age` header alone, so an upstream that omits it restarted the clock at every hop (4.2.3 corrected age)
- Client directives entirely unimplemented, so a browser reload could not force a refresh (F053–F056)
- 206 stored.
  Unsafe methods did not invalidate.
  304 missing `Vary`/`Date`
- Quoted directive values mis-split.
  `public` on one line beat a later `no-store`

**Denial of service**
- Unbounded request bodies on H2 and H3.
  Unbounded backend response allocation driven by a backend-declared length.
  A HEADERS frame with PRIORITY and a payload under five bytes sliced out of bounds and could panic the process

### 11.1 Still open

- Most of the H2 frame/stream/flow-control cluster (F064–F098 beyond the panics and flow-control fixes already made), H2 client behaviour (F099–F109), H3 integration (F110–F114), auth extensions (F115–F119)
- The cache key omits scheme and authority (F044).
  Not fixed.
  Safe for the audited configuration, where no response varies by Host.
  `should_cache` refuses to store anything whose `Vary` names a field other than `Accept-Encoding`, in every configuration.
  Unsafe for multi-tenant use.

### 11.2 The structural question

The audit's own recommendation, and the honest position at the time: either replace the hand-written HTTP/2 machinery with a maintained codec and keep m6's routing and cache around it, or commit to hand-written H2 as a project goal and build it out properly: a frame-validation table, complete stream state machine, connection and stream flow-control ledgers, HPACK directionality, fragmentation handling, fuzzing, and h2spec.

**ANSWERED, and this paragraph said "this decision gates a 1.0 release" until 2026-09-26, twelve days after 1.0 was cut.**
The second option was taken and built out: h2spec is **146/146** and h3spec **47/49**, both with recorded floors that CI enforces on every pull request.
1.0.0 was released on 2026-09-14.
The two remaining h3spec failures are QPACK stream errors inside quiche, accepted deliberately.
`tools/conformance-scores.txt` argues that one in full.

---

## 12. Known Limitations

**m6-http:**
- The HTTP/2 error codes the server does not send, tracked as issue #163 in [`docs/m6-http.md`](docs/m6-http.md)

**m6-auth-server:**
- m6-auth-server runs as one process per site, and its login failure counts live in memory.
  A restart clears them.
- The client address is the value of `X-Forwarded-For`, then `X-Real-IP`.
  m6-http always sends `X-Forwarded-For` with one address and strips any copy a client sent.
  A caller that sends neither header, which can only be something other than m6-http, counts in one shared bucket named `unix`, so its failures block every other such caller.

**Out of scope (by design):**
- Windows
- Rate limiting outside of m6-auth login
- Built-in OAuth2 / OIDC provider
- MFA / WebAuthn
- Horizontal scaling of m6-http itself (it scales vertically via caching, and globally via edge nodes)
- **HTTP/2 server push** and **103 Early Hints** were both implemented and removed on 2026-09-18.
  See below.

### 12.1 Why there is no server push or 103 Early Hints

Both existed in m6 until 1.9.0 and were removed.

**HTTP/2 push is a dead feature.**
The server cannot see the client's cache, so it sends bytes to a returning visitor who already has them.
Cache digests were proposed to close that gap and never shipped.
Chrome removed push in 106, Firefox disabled it, and RFC 9113 dropped it from the specification.
Receipt of a `PUSH_PROMISE` from a client is still refused as a connection error, per RFC 9113 8.4.
That is required and stays.

**103 Early Hints has nothing to fill in a server shaped like this.**
Its value is the gap between a request arriving and the response being ready, so a browser can start fetching subresources during backend think-time.
m6 caches server side: on a cache hit the response is ready immediately, and the 103 and the final response go out microseconds apart.
Measured on a production fleet, the origin answered in about 7ms and an edge hit from memory.
There is no dead time to exploit.
The secondary argument, that a 103 beats the HTML parser to the subresources, does not hold either: `<link>` lives in `<head>`, so a browser's preload scanner reaches it in the first kilobyte.

**The implementation also had to guess.**
It derived the hint list by scanning the response body for `href=` and `src=` and filtering on file extension, which cannot distinguish an `<img src>` from an `<a href>`, or from text inside a `<script>`, `<template>` or comment.
It preloaded images the page had explicitly marked `loading=lazy`, overriding the author.
A cache node emitted every hint twice, 89 `Link` headers for 45 distinct values.
It ordered them by directory name, so a render-blocking stylesheet was announced behind seventeen logos.
None of that was measurable, because there was no switch to turn the feature off and compare.

**If you need this, build a relay, not an oracle.**
The standard answer is for the author to declare what the page needs, using `<link rel=preload>` and `rel=modulepreload` with their own `as=`, `type` and `crossorigin`, and for the server to promote those existing declarations into `Link` response headers or a 103.
That is what Cloudflare and Fastly do.
It requires no guessing, cannot contradict the page, and is a much smaller piece of code than the one removed.
It is worth building only where there is genuine backend think-time to fill, which a cache-first server usually does not have.

See issue #93 for the full reasoning and the measurements.

---

## 13. Comparative positioning

m6-http is a **single-threaded, in-process-cached, TLS-terminating reverse proxy**.
The response cache is an `Arc<Bytes>` LRU in the same heap as the TLS stack.
A cache hit is a hash lookup, a reference-count increment, and an AES-GCM seal.
A cache held in a separate process, or in shared memory, adds an IPC round trip or a lock to that path.
This one has neither, and the cached body reaches the TLS writer without an intermediate copy.

**Throughput:** see [`docs/BENCHMARKS.md`](docs/BENCHMARKS.md) for the measured figures, the hardware, and the exact commands.
Summary: HTTP/2 is about 3× HTTP/1.1 on this hardware, on both latency and throughput.

Figures published here before September 2026 are withdrawn.
Two throughput tables contradicted each other, 158,323 req/s for HTTP/2 against 28,797, both claiming identical conditions and neither recording a commit, hardware, payload or command line.
Both were deleted and re-measured, and the measured figure in [`docs/BENCHMARKS.md`](docs/BENCHMARKS.md) is well under either.

A caveated architectural comparison against nginx, HAProxy, Envoy, H2O, OpenResty, Varnish and Pingora remains in [`docs/POSITIONING.md`](docs/POSITIONING.md), with capability columns from each project's own documentation and nothing run like-for-like.
The numeric throughput figures published here until September 2026 took their numbers from elsewhere, on other hardware with other payloads, which does not compare anything.

**Two different quantities, both true, easily confused:** a cache hit costs **~2.2 µs** inside m6 (its own timer, confirmed against a running server), while an end-to-end HTTP/2 request on the benchmark host is **~230 µs**, TLS, loopback and the client included.
Quoting the first as if it were the second is how the old numbers drifted.

**Where m6 wins:** HTTP/2 over HTTP/1.1 on this hardware, zero-IPC cache hits, and a deterministic hot path.

**Where m6 doesn't win:** HTTP/1.1 throughput (nginx is more mature), multi-core scale within a single instance (one process runs on one core), and large-file serving (no `sendfile`).

See [`docs/POSITIONING.md`](docs/POSITIONING.md) for the full analysis and [`docs/BENCHMARKS.md`](docs/BENCHMARKS.md) for raw numbers.

---

## 14. Documentation

Table 12 lists every document in [`docs/`](docs/) and what it holds.

| Document | Contents |
|---|---|
| [`docs/m6-architecture.md`](docs/m6-architecture.md) | **Why m6 is built this way.** The question it answers, what it gives up, and the reason behind every design decision |
| [`docs/m6-user-guide.md`](docs/m6-user-guide.md) | Eleven worked examples, start here |
| [`docs/m6-app-anatomy.md`](docs/m6-app-anatomy.md) | **How to write one.** The shape every m6 service has, and the four contracts it honours |
| [`docs/m6-site-toml.md`](docs/m6-site-toml.md) | `site.toml` reference |
| [`docs/m6-site-layout.md`](docs/m6-site-layout.md) | Laying out a site: one root per app |
| [`docs/m6-file.md`](docs/m6-file.md) | m6-file static file service reference |
| [`docs/m6-backend-protocol.md`](docs/m6-backend-protocol.md) | The wire contract between m6-http and any backend, in any language |
| [`docs/m6-backend-examples.md`](docs/m6-backend-examples.md) | Six reference backends in C, C++, Go, Python and Rust, and where each one disagrees with the contract |
| [`docs/m6-core-reference.md`](docs/m6-core-reference.md) | Every module in m6-core and its interface |
| [`docs/m6-log.md`](docs/m6-log.md) | What one m6 process writes, and how a line is built without a writer thread |
| [`docs/m6-platform.md`](docs/m6-platform.md) | The operating-system layer that lets a service link m6-core alone |
| [`docs/m6-http.md`](docs/m6-http.md) | m6-http internals |
| [`docs/m6-auth.md`](docs/m6-auth.md) | m6-auth-server reference |
| [`docs/m6-auth-cli.md`](docs/m6-auth-cli.md) | m6-auth-cli reference |
| [`docs/m6-auth-lib.md`](docs/m6-auth-lib.md) | m6-auth library API |
| [`docs/m6-md.md`](docs/m6-md.md) | m6-md Markdown processor |
| [`docs/m6-testing.md`](docs/m6-testing.md) | What checks m6, how to run it, and the h2spec and h3spec procedure |
| [`docs/BENCHMARKS.md`](docs/BENCHMARKS.md) | Raw benchmark numbers, end to end |
| [`docs/PERFORMANCE.md`](docs/PERFORMANCE.md) | What each component costs, which change changed it, and the conditions every figure was taken under |
| [`docs/POSITIONING.md`](docs/POSITIONING.md) | Competitive analysis |

**Table 12: every document in `docs/` and what it holds.**
The user guide runs a site, and the architecture document explains the design decisions behind it.

---

## 15. Building

```sh
cargo build --release --workspace
```

Requires Rust 1.89+.
Production target: Linux (epoll).
macOS supported for development.

For development TLS: [mkcert](https://github.com/FiloSottile/mkcert).

A new reader should run the install steps first, then read [`docs/m6-user-guide.md`](docs/m6-user-guide.md).
The reason behind each design decision is in [`docs/m6-architecture.md`](docs/m6-architecture.md).
