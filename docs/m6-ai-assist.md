# m6 — AI Code Assistance Guide

Read all documents before writing any code. Start here, then read the relevant spec document.

---

## Document Map

| Document | Read for |
|---|---|
| `m6-overview.md` | Architecture, tiers, process relationships |
| `m6-site-toml.md` | Complete config schema — primary reference |
| `m6-render.md` | m6-html and m6-file full specs |
| `m6-http.md` | Routing, caching, auth enforcement, error handling |
| `m6-auth.md` | Auth service full spec |
| `m6-decisions.md` | Every decision, flat |
| `m6-design-discussion.md` | Rationale |
| `m6-user-guide.md` | Concrete examples |
| `m6-testing.md` | Full test plan |

---

## Repository Layout

```
Cargo.toml              ← workspace manifest

m6-render/              ← library crate: App, Request, Response, lifecycle, thread pool
  src/lib.rs

m6-html/                ← zero-code binary (5 lines)
  src/main.rs

m6-file/                ← static file server (no m6-render dependency)
  src/main.rs

m6-auth/                ← auth library: Db, schema, migrations, password hashing
  src/lib.rs

m6-auth-server/         ← auth HTTP service
  src/main.rs

m6-auth-cli/            ← admin CLI
  src/main.rs

m6-http/                ← reverse proxy, cache, router
  src/main.rs
```

---

## Invariants — Never Violate These

**1. m6-http does not spawn or manage processes.** systemd does. m6-http connects to sockets.

**2. Auth is structurally absent from the hot path.** Routes compile into `Public` or `Protected` enum variants. A `Public` route executes zero auth code. Not a conditional skip — a structural absence.

**3. No `require` in site.toml → no auth code runs anywhere in m6-http.** No public key loaded, no JWT library called, no branch taken.

**4. JWT verification is local.** m6-http verifies using the public key in memory. No network call to m6-auth per request.

**5. One m6-html process per pool instance.** All HTML routes handled by one config. No per-route-type processes.

**6. Backend pools are socket globs.** Discovered via inotify on `/run/m6/`. Pool membership updates without config reload.

**6a. `site.toml` is the sole inotify watch target for site content.** m6-http does not watch data files or content directories. Any process that modifies site content signals m6-http by touching `site.toml`. This triggers a full reload: routing, pools, auth config, and cache invalidation map (re-expands `[[route_group]]` globs). TLS cert/key files and the system config are watched separately for their own reload triggers.

**7. Renderers are HTTP/1.1+ servers.** No output mode. No special protocol beyond HTTP.

**8. m6-http reads renderer configs only for invalidation map construction.** It reads `params` file declarations at startup to map data files to URL paths. It does not use renderer configs for routing, auth, or any other purpose.

**9. `site.toml` is the single source of truth.**

**10. No env vars.** Secrets in config files, out of version control.

**11. All renderers: two positional args + optional `--log-level`.**

**12. Socket path derived from config filename.** `configs/m6-html.conf` → `/run/m6/m6-html.sock`. No other convention.

**13. No plain HTTP. TLS always required.**

**14. Fixed site directory structure. No path configuration.**

**15. JSON merge order: global_params → route params → built-ins (last, non-overridable).**

**16. m6-file always returns `Cache-Control: public`.** Level 0 compression for pre-compressed formats.

**17. No static error fallback file.** `[errors] mode` controls m6-http behaviour on unreachable backends.

**18. Cache key is `(path, content-encoding)`.** Never path alone. Each encoding variant independent.

**19. m6-http owns the invalidation map.** Derived from `[[route_group]]` at startup. No backend protocol.

**20. All processes log to stdout.** Never to files. journald captures via systemd.

**21. Cache behind `Arc`, swapped atomically.** Never mutated in place.

**22. Auth enforcement: m6-http coarse-grained (route), renderers fine-grained (resource).**

**23. Verified JWT claims forwarded in `X-Auth-Claims` header.** Renderers read claims without re-verifying.

**24. Login rate limited.** 5 attempts / 15 minutes / IP. m6-auth only.

**25. Signal handling identical across all processes.** First: clean. Second: immediate.

**26. Exit codes: 0 success, 1 runtime error, 2 config/usage error.**

**27. Route matching by specificity everywhere.** Exact before parameterised. Longer before shorter.

**28. Path param values validated before filesystem access.** Invalid chars → 400.

**29. m6-html unmatched path → 404 empty body.**

**30. No build step in m6.** Content JSON is pre-existing. m6 is runtime only.

---

## Key Data Structures

### Route (m6-http)

```rust
enum Requirement {
    Group(String),
    Role(String),
}

enum Route {
    // Public: zero auth code on this path
    Public {
        backend: BackendId,
    },
    // Protected: m6-http verifies JWT before forwarding
    Protected {
        backend:     BackendId,
        requirement: Requirement,
    },
}
```

### Backend Pool (m6-http)

```rust
struct Pool {
    glob:    String,           // "/run/m6/m6-html-*.sock"
    active:  Vec<SocketPath>,  // currently connected sockets
    backoff: HashMap<SocketPath, Instant>,
}

// Load balancing: least-connections
fn select(pool: &Pool) -> Option<&SocketPath> {
    pool.active.iter()
        .filter(|s| !pool.backoff.contains_key(s))
        .min_by_key(|s| s.in_flight_count())
}
```

### Cache (m6-http)

```rust
#[derive(Clone, PartialEq, Eq, Hash)]
enum ContentEncoding { Br, Gzip, Identity }

struct CachedResponse {
    body:         Bytes,
    status:       u16,
    content_type: String,
    encoding:     ContentEncoding,
}

// Key: (path, content-encoding). Swapped atomically — never mutated in place.
type Cache = Arc<HashMap<(String, ContentEncoding), CachedResponse>>;
```

---

## Key Algorithms

### m6-http request handling

```rust
fn handle(req: &Request, state: &State) -> Response {
    // 1. Cache lookup — entirely before route table, before auth
    let encoding = best_encoding(req.accept_encoding(), &state.cache);
    if let Some(cached) = state.cache.get(&(req.path(), encoding)) {
        return Response::from_cached(cached);  // hot path ends here for public routes
    }

    // 2. Route lookup
    let route = match state.routes.match_path(req.path()) {
        None => return state.error_response(404, "Not Found", req.path()),
        Some(r) => r,
    };

    // 3. Auth — only if Protected variant
    if let Route::Protected { requirement, backend } = &route {
        if let Err(e) = verify_auth(req, requirement, &state.auth_key) {
            return e.into_response();  // 401 or 403
        }
    }

    // 4. Forward to backend pool
    let backend_id = route.backend();
    let pool = &state.pools[backend_id];
    let socket = match pool.select() {
        None => return state.error_response(503, "Service Unavailable", req.path()),
        Some(s) => s,
    };

    let resp = forward(req, socket)?;

    // 5. Error page fetch for 4xx/5xx
    let resp = if resp.status() >= 400 {
        fetch_error_page(resp.status(), req.path(), &state) 
            .unwrap_or(resp)
    } else {
        resp
    };

    // 6. Cache if public
    if resp.cache_control() == CacheControl::Public {
        state.cache_insert(req.path(), encoding, &resp);
    }

    resp
}

fn verify_auth(req: &Request, req: &Requirement, key: &DecodingKey) -> Result<Claims> {
    let token = extract_token(req)?;          // 401 if absent
    let claims = decode_jwt(token, key)?;     // 401 if invalid/expired
    check_requirement(&claims, requirement)?; // 403 if insufficient
    Ok(claims)
}
```

### m6-html request handling

```rust
// m6-html is the degenerate renderer — its handler is the framework default.
// The framework builds the request dictionary in this order:
//
//   1. Config keys (all non-framework keys from merged config + secrets)
//   2. global_params files, left-to-right
//   3. Route params files, left-to-right (path params expanded first)
//   4. Path params ({stem}, {page} etc.)
//   5. Query params
//   6. POST form fields (for POST routes)
//   7. Cookies (under "cookies" key)
//   8. Built-ins: site_name, site_domain, request_path, datetime, year
//   9. Auth keys: auth_username, auth_groups, auth_roles, auth_sub
//  10. Error keys: error_status, error_from (error route only)
//
// Later sources win on conflict. Built-ins cannot be overridden.
// The template receives the full merged dictionary.

fn handle(req: &Request) -> Result<Response> {
    Response::render(req.template(), req)  // template from matched route config
}
```

### Signal handling (all processes)

```rust
static SHUTDOWN: AtomicUsize = AtomicUsize::new(0);

extern "C" fn on_signal(_: c_int) {
    if SHUTDOWN.fetch_add(1, SeqCst) >= 1 {
        std::process::exit(0);  // second signal — immediate
    }
    // first signal — main loop polls SHUTDOWN
}
```

---

## Error Messages

```rust
// Good
.with_context(|| format!("failed to open params file '{}'", path.display()))
anyhow!("backend '{}': no sockets found matching '{}'", name, glob)
anyhow!("route '{}': require = {:?} but no [auth] declared", path, require)

// Bad
.context("open failed")
anyhow!("not found")
```

Never panic in the m6-http event loop:

```rust
if let Err(e) = handle_event(ev) {
    tracing::error!("event handling failed: {e:#}");
}
```

---

## What Not To Do

- Spawn renderer processes from m6-http
- Add conditional auth checks — use enum variants
- Load the public key if no `require` routes exist
- Call m6-auth per request for token verification
- Add a static fallback error file
- Create a `logs/` or `renderers/` directory in the site
- Use env vars for secrets
- Add Tokio to m6-http
- Mutate the cache in place
- Use suffix stripping for binary lookup (no binary lookup — systemd manages processes)
- Add a build step to m6
- Add any `auth_token` field to renderer configs

---

## Suggested Implementation Order

See `m6-implementation-plan.md` for the full plan with milestones and test gates.

1. **m6-auth library** — schema, migrations, `Db::open`, user/group ops, password hashing
2. **m6-auth-cli** — bootstrap workflow: create first admin before server starts
3. **m6-auth-server** — login, refresh, logout, public-key endpoints; rate limiting; key rotation
4. **m6-render library core** — `App::new/with_global/with_state`, lifecycle (init_global, init_thread, destroy_thread, destroy_global), thread pool, Unix socket server, HTTP/1.1 parsing, config + secrets loading, inotify reload
5. **m6-render request dictionary** — config keys, global_params, route params, path params, query params, form fields, cookies, built-ins, auth keys, error keys; LRU for parameterised params
6. **m6-render response and templates** — Tera compilation at startup (exit 2 on error), render, render_with, compression, all response constructors
7. **m6-html** — App::new().run() — the degenerate case proves the framework
8. **m6-file** — socket server, route matching, path resolution, traversal protection, compression
9. **End-to-end smoke test** — all processes started, curl all routes, verify logs are JSON
10. **m6-http Phase 1** — config parsing, route table (Public/Protected enum), pool management (inotify), request forwarding, error page fetch
11. **m6-http Phase 2** — caching: (path, content-encoding) key, invalidation map, inotify-driven eviction
12. **m6-http Phase 3** — auth enforcement: JWT verification, claim checking, X-Auth-Claims header, silent refresh redirect
13. **m6-http Phase 4** — hot reload: site.toml, TLS cert/key, system config
14. **m6-render optional features** — email, http-client, multipart, flash, csrf
15. **Hardening** — sanitiser pass, signal edge cases, load testing, exit code coverage
