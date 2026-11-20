# m6

m6 is a platform for building and deploying fast websites.
It covers the full stack from auth to rendering to delivery.
Start with a single `site.toml` and get *TLS* (Transport Layer Security), routing, in-process caching, and *JWT* (JSON Web Token) authentication through one checker.
Add Tera templates, static files, or custom renderers in any language as your site grows.
Publish a change with one `POST /genid`, and read `/health` from a monitor.
Deploy to a single machine, scale to multiple processes, or fan out to a global edge fleet.
A cache hit costs about 2.2 µs inside m6 ([`docs/BENCHMARKS.md`](docs/BENCHMARKS.md) §6).

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

**2. A development certificate.** m6-http's public listener is TLS only, so a site needs a certificate.
[mkcert](https://github.com/FiloSottile/mkcert) issues one your browser already trusts:

```sh
brew install mkcert        # or your package manager's equivalent
mkcert -install
```

**3. The examples.** They live in `examples/` in this repository, and their renderer crates use the m6 crates beside them:

```sh
cd examples
cargo build --release      # the custom renderers
cd ..
```

**4. Run the whole stack and check it.**

```sh
cd examples/05-cms
./dev.sh                   # in one terminal: the stack comes up
./test.sh                  # in another
```

Example 05 exercises everything at once: m6-http at the edge, m6-html for templates, m6-file for assets, m6-md for markdown, m6-auth-server for logins, and a custom Rust renderer for the CMS.
`test.sh` checks it end to end: both wire protocols, assets, compression, the cache, the security headers, the draft to publish to unpublish cycle, login, logout and the login throttle.

Then read [`docs/m6-user-guide.md`](docs/m6-user-guide.md), which walks the eleven examples from a static site up to a global fleet.

### 1.1 If you are changing m6 itself

Build the examples too and run that suite, which m6's own checks run:

```sh
M6_BUILD_HOST=user@example.com ./tools/build-host-tests.sh
```

See [`CONTRIBUTING.md`](CONTRIBUTING.md).

---

## 2. System Architecture

Figure 1 gives the whole shape of a deployment.
Take from it that m6-http is the only process on a public port, and that every other process is reached over a Unix socket or a URL backend.

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
                              │              │         (remote / cache node)
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

### 2.2 Deployment Tiers

Table 1 lists the four deployment tiers and the components each one adds.

| Tier | Components | Use case |
|------|-----------|---------|
| 1 | m6-http + m6-html + m6-file | Static sites. Copy files to deploy. |
| 2 | Tier 1 + external content tool (e.g. `m6-md`) | Generated static sites. |
| 3 | Tier 2 + custom renderers | Dynamic sites: forms, APIs, CMSes. Any language. |
| 4 | Tier 3 × N nodes + URL backends | Global fleet. Cache nodes proxy to the origin via h2s://. |

**Table 1: the four deployment tiers and the components each one adds.**
Tier 4 repeats tier 3 across N nodes, and each cache node reaches the origin through an `h2s://` URL backend.

---

## 3. Components

m6 ships seven binaries (`m6-http`, `m6-html`, `m6-file`, `m6-auth-server`, `m6-auth-cli`, `m6-monitor`, `m6-md`) and four libraries (`m6-core`, `m6-auth`, `m6-log`, `m6-platform`).
Sections 3.1 to 3.7 cover the components a request meets.
[`docs/m6-monitor.md`](docs/m6-monitor.md), [`docs/m6-md.md`](docs/m6-md.md), [`docs/m6-log.md`](docs/m6-log.md) and [`docs/m6-platform.md`](docs/m6-platform.md) cover the other four.

### 3.1 m6-http

Reverse proxy, cache, and router.

**Architecture:**
- One thread and one edge-triggered event loop over `m6_core::platform`
- HTTP/3 over QUIC (`quiche`) and HTTP/2 + HTTP/1.1 on the same port
- Response cache keyed by `(path, query, content-encoding)`, with each encoding variant cached independently
- Every credential gate forwards the credential to m6-auth-server's `POST /check` (section 7)
- Backend pools: Unix socket globs (local) or URL backends (`http://`, `https://`, `h2c://`, `h2s://`)
- Backend sockets are found once at start from each pool's glob, and a change takes a restart
- `site.toml` and the system config are read once at start.
  Restart m6-http after renewing a certificate
- `Cache-Control: public` responses are cached, and `no-store` and `private` responses are skipped
- Error pages fetched from the `[errors] path` route on a backend 4xx or 5xx
- `[health] path` (default `/health`) and `/genid` answered ahead of routing, the cache and every backend

**Performance design:**
- No heap allocation on the hot path (cached responses)
- Every buffer and queue starts small, grows by doubling and never shrinks, and a request past its `[limits]` maximum is refused with an error that names the key (`docs/m6-site-toml.md` §4.14)
- A request head is limited to 8 KiB per field and 32 KiB in all, answered 400 past either, or 414 for a request line over 8 KiB
- The response cache is an `AHashMap` behind `Arc<RwLock<..>>`, and a generation change replaces the whole map in one assignment
- The service installs `m6_core::alloc::M6Alloc`, which is mimalloc, and takes its `[limits.heap_reserve]` from the OS once at start (section 6.3)

**Benchmark results:**
An end-to-end HTTP/2 cache hit measured 225.7 to 259.2 µs p50 on a 4 vCPU host over loopback, TLS and the client included, and a cache hit costs about 2.2 µs inside m6.
[`docs/BENCHMARKS.md`](docs/BENCHMARKS.md) §5 gives every protocol's latency and throughput with the conditions, §8 the Criterion microbenchmarks, and [`docs/PERFORMANCE.md`](docs/PERFORMANCE.md) what each component costs.

---

### 3.2 m6-html

Tera template renderer for HTML routes.

**Architecture (via m6-core library):**
- One process and one event loop, accepting on listener readiness
- `Site` built once at start, holding the config, the route table and the renderer
- Plain state in `main`, passed to each request arm
- JSON params loaded from disk at startup
- Static params are stored as `Arc<Map>`, so a request clones a pointer and copies no Map
- Dynamic `{stem}`-keyed params resolved and LRU-cached
- `Accept-Encoding` negotiated by `m6_core::negotiate`, which parses the q-values, refuses `q=0` and allocates nothing
- Minification pipeline (applied before compression): HTML (`minify-html`), CSS (in-house), JSON (`serde_json`). JavaScript is not minified, in a script file or inline in a page
- Brotli and gzip compression with configurable levels per MIME type

**Performance design:**
- Minification runs once per cached response, and m6-http then serves the result compressed.
  A `no-store` response is not minified ([`docs/m6-architecture.md`](docs/m6-architecture.md))

---

### 3.3 m6-file

Static file server for assets, downloads, and any file in the site directory.

**Architecture:**
- One event loop and a connection table sized from the open-file limit
- Route matching with `{relpath}` (catchall) and `{stem}` (single segment) params
- Param validation: alphanumeric plus `-_.` for a stem, subdirectories allowed for a relpath, and `..` answered 404
- Symlink guard: every request path goes through `m6_core::path::resolve_within`, which canonicalises the root and the deepest existing prefix.
  A symlink escaping `site_dir` is answered 404
- Brotli and gzip compression applied per request, and m6-http caches the compressed response
- `Cache-Control` from the query string alone: `public, max-age=0, must-revalidate, s-maxage=86400` unversioned, and `public, max-age=31536000, immutable` for `?v=<hash>`

---

### 3.4 m6-auth-server

Auth service that issues tokens, answers the identity check, and manages sessions.

**Architecture:**
- Five routes: `POST /auth/login`, `POST /auth/refresh`, `POST /auth/logout`, `GET /auth/public-key`, and `POST /check`
- The other four routes answer on the primary socket.
  `POST /check` answers on the check socket, derived beside it as `<parent>/check-<file name>`, so `/run/m6/m6-auth.sock` gives `/run/m6/check-m6-auth.sock`
- `POST /check` verifies the access token's signature, issuer and expiry, then reads `sub`, `username`, `roles` and `groups` from the database for that request.
  A role change or a deleted user counts from the next request
- RS256 or ES256 JWT signing (ES256 recommended).
  Access token TTL 15 min and refresh token TTL 30 days by default, set in `[tokens]`
- Key pair read at start, so a key rotation is a restart.
  A key that fails to load, or a private key that is not the pair of the public key, exits 2
- Rate limiting: 5 failed logins per 15 minutes per client address by default, set in `[rate_limit]`
- Credentials and access lists live in the m6-auth library

---

### 3.5 m6-auth (library)

Shared library crate used by `m6-auth-server` and `m6-auth-cli`.

- SQLite with WAL mode, which lets several processes open the database at once
- Automatic schema migration on `Db::open`
- bcrypt password hashing
- User, group, role, and refresh-token management

---

### 3.6 m6-core (library)

The crate every m6 service links.
It supplies the loop parts, routing, configuration, the request dictionary and the response helpers.

- `Site` for routing, the request dictionary, templates and the site-wide finish
- Code routes as `CodeRoute` values, answered from one `match` on `Handled`
- `ServerConn`, `Signals`, `Timeouts`, `Slots` and the reactor as the parts a service's own loop uses
- `Request` / `Response` / `Error` types
- `alloc::M6Alloc`, the mimalloc global allocator every service binary installs, and `alloc::never_return`, which stops freed memory going back to the OS

`docs/m6-app-anatomy.md` walks through the loop a service writes.

---

### 3.7 m6-auth-cli

Bootstrap and management CLI that operates on the SQLite database directly, with the server running or not.
It manages users, roles, groups, API tokens and sessions, and [`docs/m6-auth-cli.md`](docs/m6-auth-cli.md) gives every command.

---

## 4. Site Directory Structure

Figure 3 gives the layout of one site directory.

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

Every process writes structured JSON lines to stdout, or to journald's native socket under systemd ([`docs/m6-log.md`](docs/m6-log.md) §5), so a site directory holds no log files.

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

# URL backend: forward to a remote origin or cache node
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
`[auth]` is also needed by a route with `require` and by either `[genid]` claim, because both of those gates ask `POST /check`.
`/auth/login`, `/auth/logout` and `/auth/refresh` reach the login socket, and `[auth] backend` names the check socket.

Every path in the system config is absolute.
[`docs/m6-site-toml.md`](docs/m6-site-toml.md) gives every key and names the one file that owns it.

---

## 6. Process Management

Each process is started and supervised on its own by the service manager, systemd on the production target.
Backends must start before m6-http (in systemd, `After=` on the backend units in m6-http's unit), because m6-http reads each pool's sockets once at start and a pool whose glob matches no socket exits 2.
A backend that restarts on the same socket path serves again on its next connect.

```bash
# Development (see examples/m6-run-eg)
m6-auth-server  $SITE_DIR $AUTH_CONF &
m6-html         $SITE_DIR $HTML_CONF &
m6-file         $SITE_DIR $FILE_CONF &
m6-http         $SITE_DIR $SITE_TOML &
wait
```

**Scaling:** start additional instances (e.g. `m6-html-2.service`), and a new instance joins its pool when m6-http restarts.

**Global deployment:** run m6-http at each edge location.
Configure it with a `h2s://` URL backend pointing to the origin.
Each cache node follows the origin's generation ID, so it flushes its own cache when a publish advances the ID.
See [Example 09](docs/m6-user-guide.md#10-example-09-global-deployment).

### 6.1 Health, logs and the build

An operator reads a running node through four things, and each has one owning document.

- **`/health`** answers 200 `ok` or 503 `degraded`, and a process that is gone gives no answer. The degraded body names the node only, because `/health` is public. [`docs/m6-http.md`](docs/m6-http.md) §11 gives both bodies and what each one means.
- **The log** is one stream of JSON lines, and the `type` key on every line names what it carries: `event`, `perf` (the metrics document), `traffic`, `access`, `startup` and `shutdown`. Under systemd each line is one journal datagram, and `M6_TYPE` carries the same word, so `journalctl M6_TYPE=perf` selects the metrics lines. No endpoint serves the metrics or traffic documents. [`docs/m6-log.md`](docs/m6-log.md) §1 and §5 give the line and the datagram.
- **The start-up record** is written once at every start: the build, the invocation, the sha256 of every config file, every setting with its source, the host facts and the effective `[limits]`, then one `shutdown` line on a clean exit. [`docs/m6-log.md`](docs/m6-log.md) §9 gives its parts.
- **The version flags.** Every binary answers `--version` with the tag form, such as `v1.16.0-rc.1`, and `--version-full` with one line per m6 crate it links, commit and dirty flag included. [`CONTRIBUTING.md`](CONTRIBUTING.md) §4.2 gives both forms.

m6-monitor reads all four across a fleet: it polls `/health` and reads the journal each poll interval, and it checks each start-up record against the environment and the deploy's record ([`docs/m6-monitor.md`](docs/m6-monitor.md) §7.1).

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

Every credential gate sends one `POST /check`: a route `require` and a source node's `GET` and `POST /genid`.
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

Each item below is enforced by the process that owns that step.

- TLS on the public listener, terminated by m6-http
- Every credential gate asks m6-auth-server's `POST /check` and uses the answer once (section 7), so a revoked credential or a role change counts from the next request
- `/health` is public, and its degraded body names the node only (section 6.1)
- Path traversal: `..` in any URL path or route param is answered 404, and a param with invalid characters is answered 400
- Symlink guard in m6-file (section 3.3): a symlink escaping `site_dir` is answered 404
- Login rate limit: 5 failed attempts per 15 minutes per client address (section 3.4)
- `site.toml` contains no secrets.
  Secrets live in the system config or a `secrets_file`
- No secret reaches the log: the start-up record writes `<redacted>` for a `Secret<T>` field, for a value from the secrets file, and for an untyped key whose name holds `password`, `secret`, `token`, `credential` or `private_key` ([`docs/m6-log.md`](docs/m6-log.md) §9)

---

## 10. Exit Codes

Table 2 lists the exit codes.

| Code | Meaning |
|------|---------|
| 0 | Clean shutdown (SIGTERM / SIGINT) |
| 1 | Runtime error, including a configured TLS, status or h2c listener that fails to bind |
| 2 | Config or usage error, including a QUIC or shared-port bind failure |

**Table 2: the exit codes every m6 process uses.**
A failed QUIC or shared-port bind is exit 2, and a failed TLS, status or h2c bind is exit 1 ([`docs/m6-http.md`](docs/m6-http.md) §13).

---

## 11. RFC compliance

m6 was audited against the HTTP RFCs in September 2026, and `CHANGELOG.md` records each fix and how it was verified.
Table 3 gives the conformance scores.

| Tester | Score | Floor |
|--------|-------|-------|
| h2spec against m6-http | **146/146** | 146 |
| h3spec against m6-http | **47/49** | 47 |

**Table 3: h2spec and h3spec scores for m6-http, and the floors CI enforces on every pull request.**
The two h3spec failures are QPACK stream errors in quiche, argued in full in `tools/conformance-scores.txt`.

### 11.1 Still open

The cache key is `(path, query, content-encoding)` and omits scheme and authority.
It is safe where no response varies by `Host` and unsafe for multi-tenant use.
`should_cache` refuses to store a response whose `Vary` names a field other than `Accept-Encoding`.

---

## 12. Known Limitations

**m6-http:**
- The server sends eight of the fourteen HTTP/2 error codes.
  It has no path that sends `INTERNAL_ERROR`, `SETTINGS_TIMEOUT`, `CANCEL`, `CONNECT_ERROR`, `INADEQUATE_SECURITY` or `HTTP_1_1_REQUIRED`.
  [`docs/m6-http.md`](docs/m6-http.md) §16.5 records each one

**m6-auth-server:**
- m6-auth-server runs as one process per site, and its login failure counts live in memory, so a restart clears them.
- The client address is the value of `X-Forwarded-For`, then `X-Real-IP`.
  m6-http always sends `X-Forwarded-For` with one address and strips any copy a client sent.
  A caller that sends neither header counts in one shared bucket named `unix`, so its failures block every other such caller.

**Out of scope:**
- Windows
- A built-in OAuth2 / OIDC provider
- MFA / WebAuthn
- Several cores in one m6-http process: each process runs one event loop and its own cache, so a node adds processes that each cache separately.
  `[server] reuse_port` lets several processes share the public port, and `docs/m6-site-toml.md` §4.2 gives its limits
- Large-file serving: m6-file does not use `sendfile`
- HTTP/2 server push and 103 Early Hints (section 12.1)

### 12.1 Why there is no server push or 103 Early Hints

**HTTP/2 push.**
The server cannot see the client's cache, so it sends bytes to a returning visitor who already has them.
A `PUSH_PROMISE` received from a client is a connection error, per RFC 9113 8.4.

**103 Early Hints.**
Its value is the time between a request arriving and the response being ready, so a browser can fetch subresources during backend think-time.
m6 caches server side, so on a cache hit the response is ready immediately.
A hit costs about 2.2 µs inside m6 (`docs/BENCHMARKS.md` §6), so the 103 and the final response go out microseconds apart.

---

## 13. Comparative positioning

m6-http is a single-threaded, in-process-cached, TLS-terminating reverse proxy.
A cache hit is a hash lookup, a reference-count increment, and an AES-GCM seal.

[`docs/POSITIONING.md`](docs/POSITIONING.md) compares m6 with nginx, HAProxy, Envoy, H2O, OpenResty, Varnish and Pingora, with capability columns from each project's own documentation and nothing run like-for-like.

---

## 14. Documentation

Table 4 lists every document in the repository, grouped by the reader it serves, with the question each one answers.

| Document | What it answers |
|---|---|
| **Running a site** | |
| [`docs/m6-user-guide.md`](docs/m6-user-guide.md) | How to build and run a site, through eleven worked examples from a static site to a global fleet. Start here |
| [`docs/m6-site-toml.md`](docs/m6-site-toml.md) | Every key of `site.toml` and the system config, and which file owns it |
| [`docs/m6-site-layout.md`](docs/m6-site-layout.md) | How to lay out a site directory: one root per app |
| [`docs/m6-http.md`](docs/m6-http.md) | What m6-http does: start-up, pools, routing, the cache, the generation ID, `/health`, signals, exit codes and the HTTP/2 bounds |
| [`docs/m6-file.md`](docs/m6-file.md) | How m6-file resolves and serves a static file, and what it refuses |
| [`docs/m6-auth.md`](docs/m6-auth.md) | How m6-auth-server issues tokens and answers `POST /check`, with its config, API and limits |
| [`docs/m6-auth-cli.md`](docs/m6-auth-cli.md) | Every m6-auth-cli command for users, groups, API tokens and sessions |
| [`docs/m6-md.md`](docs/m6-md.md) | How m6-md turns a directory of Markdown into one JSON file |
| [`docs/m6-monitor.md`](docs/m6-monitor.md) | How m6-monitor reads a fleet from `/health` and the journal, and every check it reports |
| [`docs/m6-log.md`](docs/m6-log.md) | What one m6 process writes: the line, its `type`, the journal datagram and the start-up record |
| **Writing a backend or a service** | |
| [`docs/m6-backend-protocol.md`](docs/m6-backend-protocol.md) | The wire contract between m6-http and any backend, in any language |
| [`docs/m6-backend-examples.md`](docs/m6-backend-examples.md) | Six reference backends in C, C++, Go, Python and Rust, and where each one departs from the contract |
| [`docs/m6-app-anatomy.md`](docs/m6-app-anatomy.md) | The shape every m6 service has: its two files, command line, config, limits and lifecycle |
| [`docs/m6-core-reference.md`](docs/m6-core-reference.md) | Every module in m6-core and its interface |
| [`docs/m6-auth-lib.md`](docs/m6-auth-lib.md) | The m6-auth library's API, for a renderer that opens the auth database |
| [`docs/m6-platform.md`](docs/m6-platform.md) | The operating-system layer, its contracts on Linux and macOS, and how a port is added |
| **Design and measurement** | |
| [`docs/m6-architecture.md`](docs/m6-architecture.md) | Why m6 is built this way: the question it answers, what it gives up, and the reason behind each decision |
| [`docs/PERFORMANCE.md`](docs/PERFORMANCE.md) | What each component costs, which change moved it, and the conditions behind every figure |
| [`docs/BENCHMARKS.md`](docs/BENCHMARKS.md) | The end-to-end benchmark figures and the method behind them |
| [`docs/POSITIONING.md`](docs/POSITIONING.md) | How m6-http compares with nginx, HAProxy, Envoy, H2O, OpenResty, Varnish and Pingora |
| **Changing m6** | |
| [`CONTRIBUTING.md`](CONTRIBUTING.md) | The branch model, the checks a merge passes, the version policy and how to read a build's version |
| [`docs/m6-testing.md`](docs/m6-testing.md) | What checks m6, how to run each check, and the conformance procedure |
| [`CHANGELOG.md`](CHANGELOG.md) | What changed in each release, why, and how it was verified |
| [`SECURITY.md`](SECURITY.md) | How to report a vulnerability |

**Table 4: every m6 document, by the reader it serves.**
The user guide runs a site, the app anatomy writes a service, and the architecture document gives the reasons behind both.

---

## 15. Building

```sh
cargo build --release --workspace
```

Requirements are in section 1.
Next, read [`docs/m6-user-guide.md`](docs/m6-user-guide.md) and [`docs/m6-architecture.md`](docs/m6-architecture.md).
