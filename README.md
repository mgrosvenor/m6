# m6

m6 is a platform for building and deploying fast websites.
It covers the full stack from auth to rendering to delivery.
Start with a single `site.toml` and get *TLS* (Transport Layer Security), routing, in-process caching, and *JWT* (JSON Web Token) authentication through one checker.
Add Tera templates, static files, or custom renderers in any language as your site grows.
Publish a change with one `POST /genid`, and read `/health` from a monitor.
Deploy to a single machine, scale to multiple processes, or fan out to a global edge fleet.
Either way, a cache hit costs microseconds on the server, and the round trip decides what a visitor sees.
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
The last runs the whole stack and checks it, so you finish knowing the install works.

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

Any other result is a defect in your install.

Then read [`docs/m6-user-guide.md`](docs/m6-user-guide.md), which walks the eleven examples from a static site up to a global fleet.

### 1.1 If you are changing m6 itself

Build the examples too and run that suite, because they use m6's interfaces and are the first to break on a change.
m6's own checks run it:

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
                         │  - Response cache (path × query × encoding)  │
                         │  - Credential gates ask POST /check          │
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
    │          │       │ gzip       │     │ Login/logout │   │ m6-core     │
    │          │       │ Symlink    │     │ POST /check  │   │ library     │
    │          │       │ guard      │     │ Rate limit   │   │             │
    └────┬─────┘       └────────────┘     └──────┬───────┘   └─────────────┘
         │  m6-core   library (Rust)             │  m6-auth library (Rust)
         └───────────────────────────────────────┘
                                                 │
                                          ┌──────▼──────┐
                                          │  SQLite DB  │
                                          │  WAL mode   │
                                          │  bcrypt pw  │
                                          └─────────────┘
```

**Figure 1: the m6 process family and how a request reaches it.**
m6-http terminates TLS and routes. m6-html, m6-file and a custom renderer answer it over Unix sockets, m6-auth-server issues tokens and answers the identity check, and a URL backend reaches a remote node.

### 2.1 Request Flow (cache miss)

Figure 2 follows one cache-miss request through m6-http.
Take from it that the credential decision, the route choice and the cache store all happen in the one process that terminates TLS.

```
Client → m6-http (one event loop, single thread)
           │
           ├─ Credential present: POST /check on the auth socket
           │
           ├─ Route → backend pool (round robin, Unix socket or URL)
           │
           ├─ Renderer handles request (one event loop per process)
           │
           └─ m6-http: inspect Cache-Control, store entry, return response
```

**Figure 2: a cache-miss request through m6-http.**
A protected cache-miss request makes two network hops, one to the checker and one to the backend, and a public one makes one.
The credential decision, the route choice and the cache store run in the same event loop.

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

m6 ships seven binaries and four libraries.
Sections 3.1 to 3.7 take the components a request meets: the edge, the HTML and file backends, the auth service, two of the libraries, and the command that manages the auth database.
The other binaries are `m6-monitor` and `m6-md`, and the other libraries are `m6-log` and `m6-platform`.
Each entry names what the component owns and what it costs.

### 3.1 m6-http

Reverse proxy, cache, and router.
The only process that listens on a public port.

**Architecture:**
- One thread and one edge-triggered event loop over `m6_core::platform`
- HTTP/3 over QUIC (`quiche`) and HTTP/2 + HTTP/1.1 on the same port
- Response cache keyed by `(path, query, content-encoding)`, with each encoding variant cached independently
- Every credential gate asks m6-auth-server's `POST /check` under a 50 ms deadline: a route `require`, `/perf`, `/traffic` and a source node's `/genid`.
  m6-http decodes no token and holds no key
- Backend pools: Unix socket globs (local) or URL backends (`http://`, `https://`, `h2c://`, `h2s://`)
- Backend sockets are found once at start from each pool's glob, and a change takes a restart
- `site.toml` and the system config are read once at start.
  Restart m6-http after renewing a certificate
- `Cache-Control: public` responses cached.
  `no-store` and `private` responses skipped
- Error pages fetched from configured `[errors] path` backend on 4xx/5xx
- `/health`, `/perf`, `/traffic` and `/genid` answered ahead of routing, the cache and every backend

**Performance design:**
- Zero heap allocation on the hot path (cached responses)
- Every buffer and queue starts small, grows by doubling and never shrinks, and a request past its `[limits]` maximum is refused with an error that names the key (`docs/m6-site-toml.md` §4.14)
- A request head is bounded at 8 KiB per field and 32 KiB in all, answered 400 past either, or 414 for a request line over 8 KiB
- `write_decimal` formats a length into a 20-byte stack buffer
- The response cache is an `AHashMap` behind `Arc<RwLock<..>>`, and a generation change replaces the whole map in one assignment
- The service installs `m6_core::alloc::M6Alloc`, which is mimalloc, and takes its `[limits.heap_reserve]` from the OS once at start (section 6.3)

**Benchmark results:**

Table 2 and Table 3 give the end-to-end results, client-observed, from one recorded run.
Table 2 gives latency for a cache hit and a cache miss through m6-html over each protocol.

| Protocol | Cache hit p50 | Cache hit p99 | Cache miss p50 |
|----------|--------------:|--------------:|---------------:|
| HTTP/1.1 | 679.1–687.8 | 951.4–1,011.0 | 4,797.7–5,063.1 |
| HTTP/2   | 225.7–259.2 | 318.2–369.9 | 4,096.5–4,159.8 |
| HTTP/3   | 953.6–983.3 | 1,339.5–1,344.5 | 4,329.3–4,617.0 |

**Table 2: end-to-end latency in microseconds, client-observed, for a cache hit and a cache miss through m6-html.**
Two consecutive runs on 2026-09-06 on a *VPS* (virtual private server) with 4 vCPU Intel Xeon (Skylake), 7 GB of *RAM* (random access memory) and Ubuntu 26.04.1.
The measured tree is commit `3204e0f`, built with the release profile.
TLS is 1.3 through rustls, the client is co-located with the server over loopback at concurrency 2, and each suite took 2,000 latency samples.
Each cell spans the two runs.
[`docs/BENCHMARKS.md`](docs/BENCHMARKS.md) §5 records all four figures per suite, and §4 the full conditions and the command line.

The measurement runs the client and the server on one host over loopback, so it prices CPU and system calls.
A visitor's latency includes the internet round trip, in the tens to hundreds of milliseconds.

The shape is a cache hit at 225.7 to 983.3 µs p50 and a cache miss at 4,096.5 to 5,063.1 µs, with HTTP/2 the fastest hit.

Table 3 gives throughput from the same machine, date and method.

| Protocol | req/s |
|----------|-------|
| HTTP/1.1 | 1,889–2,085 |
| HTTP/2   | 6,711–7,154 |
| HTTP/3   | 1,030–1,047 |

**Table 3: throughput by protocol at concurrency 2, over two runs on the same 4 vCPU VPS on 2026-09-06.**
The range spans the two runs, and HTTP/2 carries 3.4 to 3.6 times the requests per second of HTTP/1.1.

At concurrency 8 on a 4-core host the load generator starves the server, and two identical runs disagreed by 50x.
[`docs/BENCHMARKS.md`](docs/BENCHMARKS.md) §3 gives that diagnosis, §4 the conditions and §5 the per-percentile figures, and [`docs/PERFORMANCE.md`](docs/PERFORMANCE.md) records what each component costs.

Table 4 gives four Criterion microbenchmarks, which measure CPU cost with no I/O.

| Operation | Median | Notes |
|-----------|--------|-------|
| `make_lookup_key` | 10.4 ns | Path, query and encoding into a cache key |
| `cache_hit` | 21.5 ns | Hash lookup, entry present |
| `cache_miss` | 14.7 ns | Hash lookup, entry absent |
| `stats_record` | **0.9 ns** | Per-request counter update |

**Table 4: four m6-http critical-path microbenchmarks.**
A cache lookup costs 14.7 to 21.5 ns and a stats update 0.9 ns.
[`docs/BENCHMARKS.md`](docs/BENCHMARKS.md) §8 records these four as carried over from an earlier run with no machine and no date, so re-run them on the build host before quoting them as current.

---

### 3.2 m6-html

Tera template renderer.
Handles all HTML routes.

**Architecture (via m6-core library):**
- One process and one event loop, accepting on listener readiness
- `Site` built once at start, holding the config, the route table and the renderer
- Plain state in `main`, passed to each request arm
- JSON params loaded from disk at startup
- Static params are stored as `Arc<Map>`, so a request clones a pointer and copies no Map
- Dynamic `{stem}`-keyed params resolved and LRU-cached
- `Accept-Encoding` negotiated by `m6_core::negotiate`, which parses the q-values, refuses `q=0` and allocates nothing
- Minification pipeline (applied before compression): HTML (`minify-html`), CSS (in-house), JSON (`serde_json`), JS (`minify-js` / parse-js engine, falls back to original bytes on parse failure)
- Brotli and gzip compression with configurable levels per MIME type

**Performance design:**
- Minification runs once per cached response, and m6-http then serves the result compressed.
  A `no-store` response is not minified at all, as [`docs/m6-architecture.md`](docs/m6-architecture.md) records

---

### 3.3 m6-file

Static file server.
Serves assets, downloads, and any file from the site directory.

**Architecture:**
- One event loop and a connection table sized from the open-file limit
- Route matching with `{relpath}` (catchall) and `{stem}` (single segment) params
- Param validation: alphanumeric plus `-_.` for a stem, subdirectories allowed for a relpath, and `..` answered 404
- Symlink guard: every request path goes through `m6_core::path::resolve_within`, which canonicalises the root and the deepest existing prefix.
  A symlink escaping `site_dir` is answered 404
- Brotli and gzip compression applied per request, and m6-http caches the compressed response
- No cache in m6-file: all caching is m6-http's responsibility
- `Cache-Control` from the query string alone: `public, max-age=0, must-revalidate, s-maxage=86400` unversioned, and `public, max-age=31536000, immutable` for `?v=<hash>`

---

### 3.4 m6-auth-server

Auth service.
Issues tokens, answers the identity check, and manages sessions.

**Architecture:**
- Five routes: `POST /auth/login`, `POST /auth/refresh`, `POST /auth/logout`, `GET /auth/public-key`, and `POST /check`
- The other four routes answer on the primary socket.
  `POST /check` answers on the check socket, derived beside it as `<parent>/check-<file name>`, so `/run/m6/m6-auth.sock` gives `/run/m6/check-m6-auth.sock`
- `POST /check` verifies the access token's signature, issuer and expiry, then reads `sub`, `username`, `roles` and `groups` from the database for that request.
  A role change or a deleted user counts from the next request
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

### 3.6 m6-core (library)

The crate every m6 service links.
It supplies the loop parts, routing, configuration, the request dictionary and the response helpers, so a service writes its routes, their handlers and its loop.

- `Site` for routing, the request dictionary, templates and the site-wide finish
- Code routes as `CodeRoute` values, answered from one `match` on `Handled`
- `ServerConn`, `Signals`, `Timeouts`, `Slots` and the reactor as the parts a service's own loop uses
- `Request` / `Response` / `Error` types
- `alloc::M6Alloc`, the mimalloc global allocator every service binary installs, and `alloc::never_return`, which stops freed memory going back to the OS
- Configuration read once at start, and a change takes a restart

`docs/m6-app-anatomy.md` walks through the loop a service writes.

---

### 3.7 m6-auth-cli

Bootstrap and management CLI.
Operates directly on SQLite, with the server running or not.

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
The system config holds this node's own state: its name, the listener, the certificate, its private key, and the name of the backend that answers `POST /check`.
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

[[backend]]
name    = "m6-auth-check"
sockets = "/run/m6/check-m6-auth*.sock"

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
backend = "m6-auth-check"
```

`[auth] backend` names a `[[backend]]` with `sockets`, and `[auth]` does not load without that key.
`[auth]` is also needed by a route with `require`, by `[health] metrics_require`, and by either `[genid]` claim, because every one of those gates asks `POST /check`.
A `[auth] public_key` key from an earlier release is refused at load.
The login routes keep their own backend: `/auth/login`, `/auth/logout` and `/auth/refresh` reach the login socket, and `[auth] backend` names the check socket.

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
Each cache node follows the origin's generation ID, so it flushes its own cache when a publish advances the ID.
See [Example 09](docs/m6-user-guide.md#10-example-09-global-deployment).

### 6.1 Health and metrics

`/health` has three answers.
`200` carries a JSON body whose `status` is `ok` and whose `node` is this node's name, and a process with `[server] process_index` set adds `process_index`.
`503` carries the same body with `status` set to `degraded`.
A process that is gone, or whose listener refuses, gives no answer at all.
The degraded body names the node and no dependency, because `/health` is public and unsecured.
The dependency is in the warn line the process logs when it fails and the info line when it recovers.
`/perf` and `/traffic` answer behind `[health] metrics_require`, and answer 404 without it.

### 6.2 Publishing a generation

A publish sends `POST /genid` to the source node's m6-http.
The source reloads every backend on a Unix socket, flushes its cache, advances the generation ID and answers with the new ID.
A member that fails the reload is logged and counted, and the POST still answers 200.
A cache node follows the ID: its `[genid] upstream` names a URL backend, its `token_file` holds the bearer token, and it asks with `GET /genid` at start and every `heartbeat_secs` (10 by default).
A cache node with no ID retries a failed heartbeat after one second.
[`docs/m6-site-toml.md`](docs/m6-site-toml.md) §4.11 gives the keys and [`docs/m6-http.md`](docs/m6-http.md) §8 gives the change sequence.

### 6.3 Memory locking

On Linux, m6-http pins its pages as they are faulted.
`[server] lock_memory` defaults true, and production sets `LimitMEMLOCK=infinity` in the systemd unit.
A finite soft `RLIMIT_MEMLOCK` stops the start with exit 2, and the error names `LimitMEMLOCK=infinity`, `ulimit -l unlimited` and `[server] lock_memory = false`.
`lock_memory = false` runs unpinned.
The reservation and the pin are recorded in [`docs/PERFORMANCE.md`](docs/PERFORMANCE.md), and [`docs/m6-site-toml.md`](docs/m6-site-toml.md) gives the two keys (§4.2 for `lock_memory`, §4.14 for `heap_reserve`).

---

## 7. Auth Flow

Figure 4 follows a browser login and token refresh.
Take from it that login and refresh reach m6-auth-server, and that a protected route asks it for the identity before forwarding.

```
Browser                  m6-http                  m6-auth-server
   │                        │                            │
   │  GET /admin/dashboard  │                            │
   │───────────────────────►│                            │
   │                        │ ask /check (auth server)   │
   │                        │  - expired session cookie  │
   │                        │  - valid refresh cookie    │
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
   │                        │ ask /check (auth) ✓        │
   │                        │ forward X-Auth-Claims      │
   │                        │──────────────────────────► m6-html
```

**Figure 4: a browser login and token refresh.**
An expired session cookie is answered 302 to the refresh endpoint, the new token becomes a session cookie, and m6-http forwards the identity the authority answered to the backend.

Every credential gate sends one `POST /check`: a route `require`, `/perf`, `/traffic` and a source node's `GET` and `POST /genid`.
A node with `[auth] backend` therefore needs a running checker for those endpoints as well as for a protected route.
A request with no credential sends no check and is refused 401, except a protected browser route, which is answered 302 to `/login` or `/auth/refresh`.
A checker that does not answer within the 50 ms deadline turns the request into 503 with `Retry-After: 1`.

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

- TLS on the public listener, terminated by m6-http
- Every credential gate asks m6-auth-server's `POST /check` under a 50 ms deadline, and the answer is used once: a route `require`, `/perf`, `/traffic` and a source node's `/genid`.
  Login, refresh, logout and check reach m6-auth-server, and nothing caches a decision
- m6-http holds no key and decodes no token, so a revoked credential or a role change counts from the next request
- `/health` is public and unsecured.
  Its degraded body names the node and no dependency, and the dependency appears in the process log
- Path traversal: `..` in any URL path is answered 404, and `..` in a route param is answered 404 too.
  A param with invalid characters is answered 400
- Symlink guard: m6-file resolves every request path through `m6_core::path::resolve_within`, which canonicalises the root and the deepest existing prefix, and a symlink escaping `site_dir` is answered 404
- Rate limiting on login: 5 attempts / 15 min / IP
- `site.toml` contains no secrets.
  Secrets live in the system config or a `secrets_file`

---

## 10. Exit Codes

Table 5 lists the exit codes.
A supervisor tells a bad config from a crash by the code alone, and the bind split tells a refused config from a configured listener that failed.

| Code | Meaning |
|------|---------|
| 0 | Clean shutdown (SIGTERM / SIGINT) |
| 1 | Runtime error, including a configured TLS, status or h2c listener that fails to bind |
| 2 | Config or usage error, including a QUIC or shared-port bind failure |

**Table 5: the exit codes every m6 process uses.**
A failed QUIC or shared-port bind is exit 2 (`m6-http/src/main.rs:7469`), and a failed TLS, status or h2c bind is exit 1 (`m6-http/src/main.rs:7561`, `:7587`, `:7610`).

---

## 11. RFC compliance

m6 was audited clause by clause against RFC 9110, 9111, 9112, 9113 and 9114 in September 2026.
Every defect the audit found in message framing and smuggling, RFC 9110 semantics, RFC 9111 caching and denial of service has since been fixed, with tests, and verified against a running server.
`CHANGELOG.md` records each fix and how it was verified.

### 11.1 Still open

- Most of the H2 frame/stream/flow-control cluster (F064–F098 beyond the panics and flow-control fixes already made), H2 client behaviour (F099–F109), H3 integration (F110–F114), auth extensions (F115–F119)
- The cache key omits scheme and authority (F044), and it is not fixed.
  It is safe where no response varies by Host, and unsafe for multi-tenant use.
  `should_cache` refuses to store anything whose `Vary` names a field other than `Accept-Encoding`

### 11.2 The structural question

The audit offered two options: replace the hand-written HTTP/2 machinery with a maintained codec, or build it out properly with a frame-validation table, a stream state machine, flow-control ledgers, HPACK directionality, fuzzing and h2spec.

The second option was taken: h2spec is **146/146** and h3spec **47/49**, both with recorded floors that CI enforces on every pull request.
1.0.0 was released on 2026-09-14.
The two remaining h3spec failures are QPACK stream errors in quiche, accepted deliberately.
`tools/conformance-scores.txt` argues that one in full.

---

## 12. Known Limitations

**m6-http:**
- The server sends eight of the fourteen HTTP/2 error codes.
  It has no path that sends `INTERNAL_ERROR`, `SETTINGS_TIMEOUT`, `CANCEL`, `CONNECT_ERROR`, `INADEQUATE_SECURITY` or `HTTP_1_1_REQUIRED`.
  [`docs/m6-http.md`](docs/m6-http.md) §16.5 records each one

**m6-auth-server:**
- m6-auth-server runs as one process per site, and its login failure counts live in memory.
  A restart clears them.
- The client address is the value of `X-Forwarded-For`, then `X-Real-IP`.
  m6-http always sends `X-Forwarded-For` with one address and strips any copy a client sent.
  A caller that sends neither header, which can only be something other than m6-http, counts in one shared bucket named `unix`, so its failures block every other such caller.

**Out of scope:**
- Windows
- Rate limiting beyond m6-http's per-address request limit and m6-auth's login limit
- A built-in OAuth2 / OIDC provider
- MFA / WebAuthn
- Several cores in one m6-http process: each process runs one event loop and its own cache, so a node adds processes and each one caches separately.
  `[server] reuse_port` lets several processes share the public port, and `docs/m6-site-toml.md` §4.2 gives its limits
- HTTP/2 server push and 103 Early Hints, both removed on 2026-09-18 (section 12.1)

### 12.1 Why there is no server push or 103 Early Hints

Both existed in m6 through 1.9.0 and were removed in 1.10.0.

**HTTP/2 push is a dead feature.**
The server cannot see the client's cache, so it sends bytes to a returning visitor who already has them.
Cache digests were proposed to close that gap and never shipped.
Chrome removed push in 106, and Firefox disabled it.
A `PUSH_PROMISE` received from a client is still a connection error, per RFC 9113 8.4.

**103 Early Hints has no gap to fill.**
Its value is the time between a request arriving and the response being ready, so a browser can fetch subresources during backend think-time.
m6 caches server side, so on a cache hit the response is ready immediately.
A hit costs about 2.2 µs inside m6 (`docs/BENCHMARKS.md` section 6), so the 103 and the final response go out microseconds apart.
A `<link>` in `<head>` is reached by the browser's preload scanner in the first kilobyte, so a 103 gains nothing there either.

**If you need this, relay the page's own declarations.**
The author declares what the page needs with `<link rel=preload>` and `rel=modulepreload`, and the server promotes those declarations into `Link` response headers or a 103, as Cloudflare and Fastly do.
That is worth building only where backend think-time exists.

---

## 13. Comparative positioning

m6-http is a **single-threaded, in-process-cached, TLS-terminating reverse proxy**.
The response cache is an `AHashMap` behind `Arc<RwLock<..>>` in the same heap as the TLS stack.
A cache hit is a hash lookup, a reference-count increment, and an AES-GCM seal.
The cached body reaches the TLS writer without an intermediate copy.

**Throughput and latency:** [`docs/BENCHMARKS.md`](docs/BENCHMARKS.md) gives the measured figures, the hardware and the exact commands.
On that hardware HTTP/2 answers a cache hit 2.6 to 3 times faster than HTTP/1.1, and carries 3.4 to 3.6 times the throughput.

**Two quantities:** a cache hit costs **~2.2 µs** inside m6 (its own timer, confirmed against a running server), and an end-to-end HTTP/2 cache hit on the benchmark host costs **225.7 to 259.2 µs**, TLS, loopback and the client included.

**Where m6 wins:** HTTP/2 over HTTP/1.1, cache hits with no IPC, and a deterministic hot path.

**Where m6 loses:** multi-core scale within one instance, and large-file serving (no `sendfile`).

[`docs/POSITIONING.md`](docs/POSITIONING.md) compares m6 with nginx, HAProxy, Envoy, H2O, OpenResty, Varnish and Pingora, with capability columns from each project's own documentation and nothing run like-for-like.

---

## 14. Documentation

Table 6 lists every document in [`docs/`](docs/) and what it holds.

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

**Table 6: every document in `docs/` and what it holds.**
The user guide runs a site, and the architecture document gives the reasons behind the design.

---

## 15. Building

```sh
cargo build --release --workspace
```

Requirements are in section 1.
A new reader runs the install steps first, then reads [`docs/m6-user-guide.md`](docs/m6-user-guide.md) and [`docs/m6-architecture.md`](docs/m6-architecture.md).
