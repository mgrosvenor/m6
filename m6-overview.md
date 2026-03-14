# m6 — Architecture Overview

A small family of composable Unix processes for serving websites. Each process has one job. They are wired together via `site.toml` and communicate over Unix sockets.

---

## The Processes

**m6-http** — reverse proxy, cache, and router. The only process that listens on a public port. Routes requests to backend pools, caches `Cache-Control: public` responses, enforces route-level auth, fetches styled error pages from m6-html on backend errors. Does not manage other processes — that is systemd's job.

**m6-html** — renders HTML from Tera templates and JSON data. One instance per pool member. Handles all HTML routes: pages, error pages, log dashboards, anything template-based.

**m6-file** — serves files from the filesystem. One instance per pool member. Handles assets, downloads, log file access.

**m6-auth** — auth service. Issues and verifies JWTs, manages credentials and ACLs. m6-http verifies tokens locally using m6-auth's public key; no per-request network hop. m6-auth is called for credential operations: login, token refresh, logout, user and group management.

**User-supplied renderers** — any HTTP/1.1+ server. Written in any language. Declared as backends in `site.toml`. Managed by systemd. The m6-render Rust library makes writing them straightforward but is not required.

---

## Tiers

**Tier 1 — Static sites.** m6-http + m6-html + m6-file. Templates, configs, assets, and pre-built JSON in the site directory. No build step. No custom renderers. Deployed by copying files.

**Tier 2 — Generated static sites.** Same as tier 1 but content JSON is produced by an external tool (e.g. `m6-md`, a separate project) that understands the m6 site directory structure. m6 itself has no build step.

**Tier 3 — Dynamic sites.** User-supplied renderers handle routes requiring custom logic — forms, APIs, CMSes, anything. These are independent HTTP servers wired in as backends. m6 provides the m6-render library and examples; the renderer can be any language.

---

## Site Directory

```
my-site/
├── site.toml
├── configs/
│   ├── m6-html.conf
│   └── m6-file.conf
├── templates/
├── assets/
├── content/        ← pre-built JSON (populated by user or external tool)
└── data/
```

No binaries. No `renderers/` directory. No `logs/` directory. All processes log to stdout — systemd captures via journald.

---

## Backend Pools

Backends are pools of Unix sockets. m6-http load-balances across all sockets in a pool using least-connections. Sockets are declared as a glob — m6-http discovers matching sockets in `/run/m6/` via inotify. Adding an instance means starting a new systemd unit; the socket appears and m6-http picks it up automatically. No config change needed to scale.

```toml
[[backend]]
name    = "m6-html"
sockets = "/run/m6/m6-html-*.sock"
```

---

## Auth

Routes declare `require` — a group or role that the request's JWT must satisfy. m6-http verifies the token locally using m6-auth's public key (no network hop) and rejects the request before it reaches any renderer. Renderers can additionally perform fine-grained checks via the m6-render auth extension.

---

## Error Handling

When m6-http cannot reach any instance in a backend pool (all sockets unreachable), it returns the appropriate HTTP status code with an empty body. For backend 4xx/5xx responses, m6-http fetches a styled error page from the configured `[errors] path` at `GET <path>?status=N&from=<original-path>` and returns the rendered HTML with the original status code. No static fallback file.

---

## Process Management

systemd manages all processes. Each renderer instance is a systemd unit. m6-http has its own unit with `After=` dependencies on renderer units. For development, a shell script starts everything.

---

## Logging

All processes log structured JSON to stdout. systemd captures via journald. No log files, no log directory in the site.

---

## Hot Reload

m6-http watches `site.toml` via inotify. On change, routing and the invalidation map are reloaded with no restart. m6-http also watches `/run/m6/` for socket appearance and disappearance — pool membership updates automatically as instances start and stop.

---

## TLS

Always required. m6-http terminates TLS. Backends communicate over Unix sockets — no TLS between m6-http and renderers. Development: `mkcert`.

---

## Caching

Cache key is `(path, content-encoding)`. `Cache-Control: public` responses are cached; `no-store` and `private` are not. Each encoding variant (br, gzip, identity) is cached independently on demand — no eager pre-fetching. Invalidation is derived from `site.toml` source declarations at startup: when a data file changes (inotify), m6-http evicts affected paths directly.
