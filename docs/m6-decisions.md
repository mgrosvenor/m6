# m6 — Decisions Reference

Every design decision, flat. For rationale see `m6-design-discussion.md`.

---

## Architecture

- Five processes: m6-http, m6-html, m6-file, m6-auth, user-supplied renderers
- m6-http is the only public-facing process
- systemd manages all processes — m6-http does not spawn or monitor anything
- All processes log to stdout — systemd captures via journald
- `site.toml` is the single source of truth

## Tiers

- **Tier 1** — m6-http + m6-html + m6-file. Static sites. No build step. No custom renderers.
- **Tier 2** — Tier 1 + external content tools (e.g. `m6-md`). Generated static sites. m6 has no build step.
- **Tier 3** — User-supplied renderers. Any language, any HTTP/1.1+ server. Dynamic sites.

## Site Directory

- Fixed structure: `site.toml`, `configs/`, `templates/`, `assets/`, `content/`, `data/`
- No `renderers/` directory — binaries found via PATH or absolute paths in systemd units
- No `logs/` directory — all processes log to stdout
- No build artifacts in site directory

## Process Management

- systemd manages all processes — restart, logging, resource limits, dependency ordering
- m6-http does not start, monitor, or restart any renderer
- Development: shell script starts all processes, `wait` on exit
- m6-http connects to backends over Unix sockets — if unreachable, returns per `[errors] mode`

## Backend Pools

- Backends declared as socket globs: `sockets = "/run/m6/m6-html-*.sock"`
- m6-http watches the socket directory via inotify — pool membership updates automatically
- Socket appears → added to pool. Socket disappears → removed from pool
- Load balancing: least-connections
- Failed connection → socket temporarily removed, retried with backoff (1s, 2s, 4s, max 30s)
- Empty pool → return status per `[errors] mode`
- URL backends: single upstream, ALPN negotiation, not pooled

## Renderer CLI

- Two positional args: `<site-dir>` `<config-path>`
- Socket path derived from config filename: `configs/m6-html.conf` → `/run/m6/m6-html.sock`
- Multi-instance: `configs/m6-html-2.conf` → `/run/m6/m6-html-2.sock`
- Optional `--log-level` flag only
- No env vars — secrets in config files

## m6-html Config

- `global_params` — merged before every route's params
- `[[route]]` blocks: `path`, `template`, `params`, `status`, `cache`
- `status` — fixed integer per route, default 200
- `cache` — `"public"` or `"no-store"`, default `"public"`
- No `auth_token` in renderer config — auth is m6-http's responsibility

## m6-file Config

- `[compression]` table: MIME type → `{brotli, gzip}` levels
- `[[route]]` blocks: `path`, `root`
- Always returns `Cache-Control: public`
- No `auth_token` in renderer config

## Route Matching (all components)

- Specificity wins: exact before parameterised, longer before shorter
- First declaration breaks ties (warning logged)
- Same `{param}` syntax in `site.toml` and renderer configs

## Path Parameter Expansion

- `{stem}`, `{relpath}` etc. in params paths expanded from request URL
- Validated before filesystem access: alphanumeric, hyphens, underscores (+ `.` `/` for `{relpath}`)
- No `..` — returns 400

## JSON Merging (m6-html)

- `global_params` first, then route `params`, left-to-right, later wins
- Built-in keys injected last — cannot be overridden
- Built-ins: `site_name`, `site_domain`, `request_path`, `datetime`, `year`, `query`

## Caching

- Cache key: `(path, content-encoding)` — each encoding variant independent
- `Cache-Control: public` → cached. `no-store` / `private` → not cached
- No eager pre-fetching — only requested encodings are cached
- All entries for a path evicted together on invalidation
- Invalidation map built from two sources: `[[route_group]]` globs (file → URL) and renderer config `params` declarations (params file → all routes referencing it)
- m6-http reads renderer configs at startup solely to build the params invalidation map — not for routing
- `[[route_group]]` glob expanded at startup and on `site.toml` reload — new files require a reload to become routable
- Renderers writing new content files should touch `site.toml` to trigger glob re-expansion and route table update
- Map rebuilt on `site.toml` reload

## Error Handling

- Backend 4xx/5xx → m6-http fetches `GET <[errors] path>?status=N&from=<original-path>` from the configured error backend
- If no `/_errors` route declared → original backend response forwarded as-is
- If `/error` fetch fails or request was already to `/error` → return per `[errors] mode`
- Backend unreachable (pool empty) → return per `[errors] mode`
- No static fallback file

## `[errors] mode`

- `"status"` — HTTP status code, empty body (default)
- `"internal"` — HTTP status code, m6-http-generated minimal HTML
- `"custom"` — HTTP status code, error page fetched from `[errors] path` backend

## Config Layering

- `site.toml` travels with the site — version controlled, deployed via rsync, contains no secrets
- System config is a required second positional argument: `m6-http <site-dir> <system-config>`
- Development uses a minimal checked-in system config (e.g. `configs/system-dev.toml`)
- System config contains only `[server]` — bind address and TLS paths that differ per environment
- System config wins on `[server]` key conflict. All other keys come from `site.toml` unchanged.
- Validation runs after merge — `[server]` may come from either file
- System config absent → `site.toml` is authoritative (normal for development)
- System config watched via inotify — reload on change, no restart needed
- `secrets_file` in renderer configs — optional path to a TOML file outside the site directory
- `secrets_file` merged into renderer config at startup — secrets file wins on conflict
- `secrets_file` absent → silently ignored (development uses values directly in renderer config)
- `site.toml` is clean: no passwords, no cert paths, safe to publish



- Optional — completely absent from hot path if no `require` in `site.toml`
- `[auth]` section required if any route uses `require` — startup validation error if absent
- m6-http verifies JWT locally using public key — no network call per request
- Enforcement: signature, expiry, issuer, then group/role claim
- Login is a plain HTML form POST (`application/x-www-form-urlencoded`); `POST /auth/login` also accepts `application/json` for API clients
- Browser login: `302` on success/failure; sets `session` cookie (`Path=/`, 15min) and `refresh` cookie (`Path=/auth/refresh`, 30 days) — both HttpOnly, Secure, SameSite=Strict
- `next` param as hidden form field, validated server-side as relative path
- Silent renewal: expired session + valid refresh cookie → m6-http redirects to `POST /auth/refresh` → new cookies → back to original path
- Logout: form POST to `/auth/logout` — clears both cookies, redirects to `/`
- 401 handling: browser requests attempt refresh first; redirect to `/login?next=<path>` only if refresh also fails; API clients receive 401 directly
- JWT extracted from `Authorization: Bearer` header first, `session` cookie second
- Verified claims forwarded to renderers in `X-Auth-Claims` header
- Renderers can inspect claims for fine-grained checks via m6-render auth extension
- `require` syntax: `"group:<n>"` or `"role:<n>"`

## m6-auth

**Library crate (`m6-auth`):**
- Owns database schema, migrations, password hashing, user/group operations
- Shared by `m6-auth-server`, `m6-auth-cli`, and any custom renderer (e.g. render-signup)
- Sync API — `rusqlite` is sync, renderers call it from sync handlers
- `Db::open` creates database and runs migrations automatically
- SQLite WAL mode — multiple processes may open simultaneously

**Server binary (`m6-auth-server`):**
- Pure runtime service: login, token refresh, logout only
- No HTTP management endpoints — four endpoints total
- RS256 or ES256 JWT signing (EC recommended)
- Access token TTL: 15 minutes. Refresh token TTL: 30 days.
- Rate limiting on login: 5 attempts / 15 minutes / IP
- Keys watched via inotify — rotated without restart
- Never logs passwords, tokens, or key material

**CLI binary (`m6-auth-cli`):**
- User and group management via direct SQLite
- Bootstrap: creates first admin user before server starts
- Works whether server is running or not

## Logging

- All processes log structured JSON to stdout
- systemd captures via journald — `journalctl -u m6-html`
- `[log] level` and `format` in `site.toml` apply to all processes

## TLS

- Always required — m6-http terminates TLS
- Unix socket communication between m6-http and renderers — no internal TLS
- Cert/key watched via inotify — reloaded without restart
- Development: `mkcert`

## m6-http Event Loop

- Single-threaded epoll — no Tokio
- inotify fd in same epoll set as network fds
- No heap allocation on hot path
- Cache behind `Arc`, swapped atomically — never mutated in place
- No blocking calls in event loop

## Renderer Concurrency Model

- Fixed thread pool per renderer process — default size: CPU count
- Bounded request queue — default depth: pool size × 8
- Queue full → 503 immediately, back-pressure to m6-http
- Two tiers of state: `Global` (shared, `Send + Sync`) and `ThreadLocal` (per-thread, `Send` only)
- Framework wraps `Global` in `Arc` — one atomic increment per request, no data copy
- `ThreadLocal` owned exclusively by its thread — zero synchronisation overhead
- Per-thread database connections give true parallelism with no locking
- Three entry points: `App::new()` (no state), `App::with_global()`, `App::with_thread_state()`, `App::with_state()`
- Handler signature enforced at compile time to match entry point used
- Scaling via multiple systemd instances, not larger thread pools
- `[thread_pool] size` and `queue_size` configurable per renderer in renderer config

## Signal Handling

- SIGTERM and SIGINT identical across all tools
- First: clean shutdown. Second: immediate exit
- m6-http: drain in-flight, exit 0
- Renderers: finish current request, close socket, exit 0

## Exit Codes

- 0: success / clean shutdown
- 1: runtime error
- 2: config or usage error (before binding)

## No Build Step in m6

- m6 has no build step
- Content JSON produced by external tools (tier 2) or user renderers (tier 3)
- m6-html and m6-file serve whatever is in the site directory

## User-Supplied Renderers

- Any HTTP/1.1+ server, any language
- Managed by systemd, declared as `[[backend]]` in `site.toml`
- m6-render Rust library available but not required
- No binary convention in site directory

## Not In Scope (v1)

- Windows
- Rate limiting (except m6-auth login)
- Built-in OAuth2 / OIDC provider
- MFA / WebAuthn
- Horizontal scaling of m6-http itself (single instance, scales via caching)
