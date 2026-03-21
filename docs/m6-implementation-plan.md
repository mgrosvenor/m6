# m6 — Implementation Plan

Each phase has a clear deliverable and a test gate. Nothing moves to the next phase until the gate passes. All tests run with `cargo nextest run`. Sanitiser runs (`-Z sanitizer=address,leak`) gate the hardening phase.

---

## Phase 1 — m6-auth library

**Deliverable:** `m6-auth` crate. Schema, migrations, all user/group/token operations.

**Work:**
- `Db::open` — creates SQLite file, sets WAL mode, runs migrations
- Schema: `users`, `groups`, `memberships`, `refresh_tokens`
- `user_create`, `user_get`, `user_list`, `user_delete`, `user_set_password`, `user_set_roles`, `user_verify_password`
- `group_create`, `group_get`, `group_list`, `group_delete`, `group_member_add`, `group_member_remove`, `group_members`, `user_groups`
- `refresh_token_store`, `refresh_token_verify`, `refresh_token_revoke`, `refresh_tokens_revoke_all`
- `AuthError` enum
- Bcrypt for password hashing (cost factor 12)

**Test gate:**
- Unit tests for all operations against a temp database
- `user_verify_password` returns `None` for wrong password and non-existent user (same response — no user enumeration)
- `user_delete` cascades: group memberships removed, refresh tokens revoked
- Concurrent open: two `Db::open` calls on the same file, interleaved writes, no corruption
- WAL mode verified after open

---

## Phase 2 — m6-auth-cli

**Deliverable:** `m6-auth-cli` binary. Full bootstrap workflow working end-to-end.

**Work:**
- CLI argument parsing: `<config> <entity> <command> [args] [flags]`
- `user ls|add|del|passwd|roles`
- `group ls|add|del|member ls|add|del`
- `--password` flag (non-interactive), `--json` flag on ls commands
- Interactive password prompt via `rpassword`
- Reads `db.path` from config file — no other config keys used
- Exit codes: 0 success, 1 runtime error, 2 usage error

**Test gate:**
- Full bootstrap sequence: key generation → `user add admin --role admin` → `group add editors` → `group member add editors admin`
- `user ls --json` output is valid JSON
- `user add` duplicate username → exit 1, clear error message
- `user del` unknown user → exit 1
- Config file not found → exit 2
- No arguments → exit 2, usage message

---

## Phase 3 — m6-auth-server

**Deliverable:** `m6-auth-server` binary. All four HTTP endpoints functional.

**Work:**
- Unix socket server (HTTP/1.1)
- Config loading: `[storage]`, `[tokens]`, `[keys]`
- Key loading: EC (prime256v1) and RSA (2048+), PEM format
- inotify on key files — reload without restart
- `POST /auth/login` — form and JSON content types, sets two cookies for browser, returns JSON for API
- `POST /auth/refresh` — cookie and JSON, issues new access token
- `POST /auth/logout` — revokes refresh token, clears cookies for browser
- `GET /auth/public-key` — returns PEM public key
- Login rate limiting: 5 attempts / 15 minutes / IP (in-memory, resets on restart)
- JWT issuance: RS256 or ES256, claims: `iss`, `sub`, `exp`, `iat`, `username`, `groups`, `roles`
- `next` param validation: relative path starting with `/`, falls back to `/`
- Never logs passwords, tokens, or key material
- Exit 2 if private key not found or unreadable

**Test gate:**
- Login form POST: valid credentials → 302, two cookies (session Path=/, refresh Path=/auth/refresh), both HttpOnly Secure SameSite=Strict
- Login JSON: valid credentials → 200 JSON, no cookies
- Login: wrong password → 401/302, no cookie
- Login: 6th attempt within 15 minutes → 429 with Retry-After
- Refresh: valid cookie → 302, new session cookie
- Refresh: expired token → 302 /login
- Logout: clears both cookies (Max-Age=0)
- Public key endpoint returns valid PEM
- JWT signature verifiable with returned public key
- Key rotation: issue token with key A, rotate files, token from A still valid until expiry, token from B valid immediately
- SIGTERM: in-flight request completes, then exit 0
- SIGTERM twice: immediate exit

---

## Phase 4 — m6-render library core

**Deliverable:** `m6-render` crate. Framework skeleton: socket server, config loading, thread pool, lifecycle, inotify reload.

**Work:**
- Unix socket HTTP/1.1 server
- Config + secrets file loading and merge
- `App::new()`, `App::with_global()`, `App::with_thread_state()`, `App::with_state()` entry points
- Lifecycle: `init_global`, `init_thread` × N threads, `destroy_thread` × N, `destroy_global`
- Fixed thread pool: size from `[thread_pool] size` (default: CPU count), bounded queue (default: size × 8)
- Queue full → 503 immediately
- inotify on `site.toml` and renderer config file → reload sequence
- Template errors at startup → exit 2
- Template errors at runtime (render) → 500
- Signal handling: SIGTERM/SIGINT first → drain queue, finish in-flight, destroy lifecycle, exit 0; second → immediate exit
- Exit codes: 0 clean, 1 runtime, 2 config/startup

**Test gate:**
- `App::new().run()` with empty config: starts, accepts requests, returns 404 for unknown paths
- Thread pool: N concurrent blocking handlers run truly in parallel (assert via timing)
- Queue full: N+queue_size+1 simultaneous slow requests → some receive 503
- `init_global` returning `Err` → exit 2 before accepting any request
- `init_thread` returning `Err` → exit 2
- Template syntax error in config → exit 2
- inotify: touch config file → destroy_thread × N, destroy_global called, then init sequence reruns
- SIGTERM mid-request: in-flight completes, destroy called, exit 0
- SIGTERM twice: immediate exit (in-flight abandoned)
- Secrets file: absent → silently ignored; present but malformed → exit 2; present → keys override config

---

## Phase 5 — m6-render request dictionary

**Deliverable:** Full request dictionary assembly — all ten merge layers.

**Work:**
- Config keys merged as layer 1
- Global params files: loaded at startup into memory, refreshed on reload
- Route params files: static files cached, parameterised files via LRU (`[params_cache] size`, default 256), LRU cleared on reload
- Path param extraction and validation (alphanumeric, hyphens, underscores; `.` `/` for `{relpath}`; no `..` → 400)
- Query param parsing → merged into dict
- POST form field parsing (`application/x-www-form-urlencoded`) → merged into dict
- Cookie header parsing → merged under `cookies` key
- Built-in keys: `site_name`, `site_domain`, `request_path`, `datetime`, `year`
- Auth keys: `auth_username`, `auth_groups`, `auth_roles`, `auth_sub` from `X-Auth-Claims` header
- Error keys: `error_status`, `error_from` from query params on error route

**Test gate:**
- Full merge order: conflicting keys at each layer resolved correctly (later source wins)
- Built-in keys cannot be overridden by params files
- `{stem}` containing `..` → 400
- `{relpath}` with subdirectory → allowed
- Missing static params file → 500, error logged
- Missing parameterised params file → 500, error logged
- LRU: 257th unique stem evicts least-recently-used; LRU cleared on reload
- Cookies parsed correctly, nested under `cookies` key
- Auth keys absent when `X-Auth-Claims` header absent
- Error keys present only on error route

---

## Phase 6 — m6-render response and templates

**Deliverable:** Full response construction, Tera template rendering, compression.

**Work:**
- Tera templates compiled at startup (all templates in config), exit 2 on syntax error
- `Response::render`, `render_with`, `render_status`
- All response constructors: redirect, json, json_status, text, status, not_found, forbidden, bad_request
- `.header()` and `.cookie()` chained modifiers
- Compression: brotli and gzip per `Accept-Encoding`, configurable per MIME type, defaults
- Custom Tera filters: `slugify`, `date_format`, `markdown` (comrak), `truncate_words`
- Built-in template keys available without handler code
- File I/O helpers: `read_json`, `write_json`, `write_json_atomic`, `list_json`, `site_path`, `touch`
- Error mapping: `Error::NotFound` → 404, `Error::Forbidden` → 403, `Error::BadRequest` → 400, other → 500

**Test gate:**
- Template with undefined variable → 500, logged
- `{{ content | markdown }}` renders correctly
- `{{ "hello world" | slugify }}` → `"hello-world"`
- brotli response decompresses to correct HTML
- gzip response decompresses to correct HTML
- No `Accept-Encoding` → identity
- `write_json_atomic`: write fails partway → old file intact (temp+rename pattern)
- `Error::NotFound` → 404 response
- All response constructors produce correct status and Content-Type

---

## Phase 7 — m6-html

**Deliverable:** `m6-html` binary. Five lines of code. All behaviour from the framework.

**Work:**
```rust
fn main() -> Result<()> {
    App::new().run()
}
```

- Compression configured in `[compression]` block
- Routes and params declared in config
- Logging: startup (routes loaded, templates compiled), request complete, unmatched path (warn), params file missing (error)

**Test gate:** Full m6-html test suite from testing plan (L1–L8):
- Start/stop, secrets_file handling
- Route matching (exact before parameterised, longer before shorter, tie warning)
- Params merge order
- Path parameter expansion and validation
- All built-in keys in templates
- Status and Cache-Control per route
- Compression (br, gzip, identity)
- 100 concurrent requests, no errors
- SIGTERM: clean exit, sanitiser: no leaks

---

## Phase 8 — m6-file

**Deliverable:** `m6-file` binary. Static file server with route matching, path resolution, traversal protection, compression.

**Work:**
- Minimal Unix socket HTTP/1.1 server (not m6-render — separate implementation)
- CLI: `<site-dir> <config-path>`, same socket path derivation
- Config: `[[route]]` with `path` and `root`, `[compression]`
- Route matching: same specificity rules
- Path resolution: URL suffix appended to expanded `root`
- Traversal protection: resolved path within root, no `..`, symlinks not followed outside root
- MIME type detection
- `Cache-Control: public` always
- Compression per MIME type and Accept-Encoding

**Test gate:** Full m6-file test suite (L1–L5):
- Existing file → correct bytes and Content-Type
- Nonexistent file → 404
- `../` in URL → 404
- Symlink outside root → 404
- `{relpath}` with subdirectory → correct file
- Compression: text/css compressed, image/jpeg not
- Cache-Control always public
- Sanitiser: no leaks

---

## Phase 9 — End-to-end smoke test

**Deliverable:** All processes running together, all routes responding correctly.

**Work:**
- Fixture site with all routes (/, /blog, /blog/{stem}, /_errors, /auth/*, /assets/*, protected routes)
- Dev shell script starts all processes
- Curl all public routes → 200
- Protected route without JWT → redirect to login
- Login → get JWT → access protected route → 200
- Asset → correct bytes, Cache-Control: public
- Unknown path → 404 with error page (if custom errors configured)
- All log lines valid JSON

**Test gate:** Script exits 0. All assertions pass.

---

## Phase 10 — m6-http Phase 1: Routing and Forwarding

**Deliverable:** m6-http routes requests to backends over Unix sockets.

**Work:**
- `site.toml` parsing and validation, system config merge
- Route table: `Public`/`Protected` enum variants — structural, not conditional
- `[[route_group]]` glob expansion at startup
- Pool management: socket glob watching via inotify, least-connections load balancing, backoff on failure
- Request forwarding: complete request, hop-by-hop headers stripped, X-Forwarded-* added
- `[errors] mode`: status, internal, custom (fetch from error path, no recursion)
- Exit 2 on all validation failures
- `--dump-config` flag

**Test gate:** m6-http L1 (start/stop), L2 (routing), L5 (pool management), L7 (error handling), L8 (errors mode):
- All validation failures exit 2 before binding
- Exact route beats parameterised
- Socket appears → added to pool immediately
- Socket disappears → removed from pool
- All sockets gone → 503 per mode
- Failed socket retried after backoff
- Error page fetched from error path, original status returned
- Error page fetch fails → falls back to internal mode
- No recursion on error path requests

---

## Phase 11 — m6-http Phase 2: Caching

**Deliverable:** In-memory cache with inotify-driven invalidation.

**Work:**
- Cache key: `(path, content-encoding)`
- `Cache-Control: public` → cached; `no-store`/`private` → not cached
- 4xx/5xx responses never cached
- Query strings stripped before cache lookup
- Invalidation map: built from `[[route_group]]` globs (file → URL) and renderer config params declarations (params file → routes)
- m6-http reads renderer configs at startup for invalidation map only
- inotify on data files → evict affected cache entries
- Map rebuilt on `site.toml` reload
- `Arc` swap — cache never mutated in place

**Test gate:** m6-http L6 (caching), L9 (hot reload):
- `Cache-Control: public` response cached, second request not forwarded to backend
- `Cache-Control: no-store` not cached
- Cache key is (path, encoding): br and gzip cached independently
- Query string stripped: `?a=1` and `?a=2` same cache entry
- Data file modified → inotify fires → affected cache entry evicted → next request goes to backend
- `site.toml` modified → glob re-expanded → new files become routable → old invalidation map rebuilt

---

## Phase 12 — m6-http Phase 3: Auth Enforcement

**Deliverable:** Route-level JWT enforcement, X-Auth-Claims forwarding, silent refresh.

**Work:**
- Load public key at startup only if `[auth]` declared and routes have `require`
- JWT extracted from `Authorization: Bearer` header first, `session` cookie second
- Local verification: signature (RS256/ES256), expiry, issuer
- Claim checking: `group:<n>` or `role:<n>` against JWT claims
- 401 (no/invalid token) or 403 (insufficient claims)
- Browser 401 handling: check for `refresh` cookie → redirect to `POST /auth/refresh` → if valid, new cookies + redirect to original path; if expired → redirect to `/login?next=<path>`
- API clients (non-`text/html` Accept): 401 directly, no redirect
- Verified claims forwarded in `X-Auth-Claims` header (base64 JSON)
- Public routes: zero auth code executed (assert via instrumentation)
- No `[auth]` declared: no public key loaded, no JWT code path reachable

**Test gate:** m6-http L3 (public routes, zero auth), L4 (protected routes, login, refresh, logout):
- Public route with no JWT: forwarded, auth function call count = 0
- Protected route, no token, API → 401
- Protected route, no token, browser → 302 to `/login?next=...`
- Protected route, expired session + valid refresh, browser → transparent renewal
- Protected route, both expired, browser → 302 to login
- Invalid JWT signature → 401
- Expired JWT → 401
- Wrong group → 403
- Correct group → forwarded with X-Auth-Claims
- X-Auth-Claims base64-decoded JSON matches original claims

---

## Phase 13 — m6-http Phase 4: Hot Reload

**Deliverable:** All inotify-driven reloads working without restart.

**Work:**
- `site.toml` change → full reload: routes, pools, auth config, invalidation map
- System config change → re-merge `[server]`, reload TLS context
- TLS cert/key change → reload TLS context
- Socket appears in `/run/m6/` matching pool glob → added to pool
- Socket disappears → removed from pool
- All reload logic runs in epoll loop (no blocking calls)

**Test gate:** m6-http L9 (hot reload):
- `site.toml` modified → route table updated, new routes routable
- TLS cert replaced → new connections use new cert, old connections unaffected
- Data file modified → cache eviction (covered in Phase 11 but reconfirmed here)
- New socket appears → requests routed to it within one inotify cycle

---

## Phase 14 — m6-render optional features

**Deliverable:** email, http-client, multipart, flash, csrf features functional.

**Work:**
- `email` (lettre): `SmtpTransport` in Global state, `Message` builder in handler
- `http-client` (ureq): `Agent` in Global or ThreadLocal state, sync HTTP calls
- `multipart` (multer): `req.file("field")`, `Upload` struct, `req.write_bytes()`, max size config
- `flash`: HMAC-SHA256 signed cookie, `flash_secret` from secrets_file, exit 2 if absent, one-redirect lifetime
- `csrf`: synchroniser token per session, `req.verify_csrf()`, `{{ csrf_token }}` injected into all templates

**Test gate (per feature):**

*email:* send via SMTP mock (mailhog or similar), message received with correct headers and body.

*http-client:* call mock HTTP server, response body parsed correctly.

*multipart:* upload file, `upload.filename` is user-supplied string (not used as path), `write_bytes` writes correct bytes atomically.

*flash:* set on POST, read on next GET, absent on subsequent GET; tampered cookie silently ignored (no flash shown); no `flash_secret` → exit 2.

*csrf:* valid token → request proceeds; missing token → 403; tampered token → 403; `{{ csrf_token }}` in template.

---

## Phase 15 — Examples and Demo Site

**Deliverable:** Three working example renderers and a complete demo site that exercises the full stack. These are the primary proof that the library API is correct and usable — if they are awkward to write, something in the framework needs fixing.

### render-contact

Contact form with SMTP. Exercises `App::with_global`, `init_global`, `Global` state, form field parsing, the `email` feature, flash messages on success.

```
render-contact/
  src/main.rs
  Cargo.toml
```

**Gate:** form POST sends email via SMTP mock; success flash message appears on redirect; missing field returns 400; `flash_secret` absent → exit 2.

### render-signup

User registration using the m6-auth library. Exercises `App::with_thread_state`, `init_thread`, `ThreadLocal` state with a `Db` connection, `AuthError::UserExists` handling.

```
render-signup/
  src/main.rs
  Cargo.toml
```

**Gate:** new user created → redirect to `/login?registered=1`; duplicate username → error in template; registered user can log in via m6-auth.

### render-cms

Content management: drafts, publish, unpublish. Exercises `App::new` (no state), file I/O helpers (`read_json`, `write_json_atomic`, `list_json`, `touch`), auth claims via `req["auth_username"]`, and the full publish flow (write file → touch `site.toml` → m6-http evicts cache → route becomes live).

```
render-cms/
  src/main.rs
  Cargo.toml
```

**Gate:** create draft → appears in dashboard; publish → post accessible at `/blog/{stem}`, route live, cache evicted; unpublish → route gone; unauthenticated request → 403.

### Demo site

A complete m6 site exercising all tiers and all example renderers together.

```
demo/
  site.toml
  configs/
    m6-html.conf
    m6-file.conf
    m6-auth.conf
    render-contact.conf
    render-signup.conf
    render-cms.conf
  templates/
  assets/
  content/
  data/
  setup.sh        ← key generation + first admin user
  dev.sh          ← starts all processes
```

**Gate:**
- `./setup.sh` runs clean on a fresh checkout
- `./dev.sh` starts all processes; all exit cleanly on SIGTERM
- All public routes return 200
- Contact form sends email (SMTP mock)
- Signup creates user; subsequent login succeeds
- CMS publish flow: post appears at correct URL, cache evicted, route live
- All log output is valid JSON: `cat logs | jq . > /dev/null`

---

## Phase 16 — Hardening

**Deliverable:** All processes pass sanitiser, load tests, signal edge cases.

**Work:**
- Address and leak sanitiser: `RUSTFLAGS="-Z sanitizer=address,leak" cargo +nightly nextest run`
- Load: `wrk -t4 -c100 -d30s` on cached public routes, p99 < 10ms
- Signal edge cases: SIGTERM during `init_global`, during `init_thread`, during `destroy_thread`
- Exit code coverage: every exit 2 path reachable by test
- All log lines valid JSON: `journalctl -o cat | jq . > /dev/null`
- `cargo clippy -- -D warnings` clean
- `cargo fmt --check` clean

**Test gate:** All above pass in CI. Load test p99 meets target.

---

## CI Pipeline

```yaml
jobs:
  test:
    runs-on: ubuntu-latest
    steps:
      - cargo nextest run
      - cargo clippy -- -D warnings
      - cargo fmt --check

  sanitiser:
    runs-on: ubuntu-latest
    steps:
      - RUSTFLAGS="-Z sanitizer=address,leak"
        cargo +nightly nextest run --target x86_64-unknown-linux-gnu

  load:
    runs-on: ubuntu-latest
    steps:
      - start all processes against fixture site
      - wrk -t4 -c100 -d30s https://localhost:8443/
      - assert p99 < 10ms for cached public routes
      - assert p99 < 50ms for uncached template routes
```

---

## Milestone Summary

| Phase | Deliverable | Gate |
|---|---|---|
| 1 | m6-auth library | Unit tests, concurrent access |
| 2 | m6-auth-cli | Bootstrap workflow, error handling |
| 3 | m6-auth-server | All endpoints, rate limiting, key rotation |
| 4 | m6-render core | Socket server, thread pool, lifecycle, reload |
| 5 | Request dictionary | All 10 merge layers, LRU, validation |
| 6 | Response and templates | Tera, compression, file I/O |
| 7 | m6-html | Full m6-html test suite |
| 8 | m6-file | Full m6-file test suite |
| 9 | End-to-end smoke | All processes, all routes |
| 10 | m6-http routing | Route table, pool management, error modes |
| 11 | m6-http caching | Cache key, invalidation, Arc swap |
| 12 | m6-http auth | JWT enforcement, zero auth on public routes |
| 13 | m6-http hot reload | All inotify triggers |
| 14 | Optional features | email, http-client, multipart, flash, csrf |
| 15 | Examples and demo site | All three renderers working, demo site setup.sh + dev.sh clean |
| 16 | Hardening | Sanitiser, load test, clippy, fmt |
