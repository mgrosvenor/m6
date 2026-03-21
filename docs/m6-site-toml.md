# m6 — `site.toml` Reference

Single config file read by m6-http and all renderers. All paths relative to the site directory unless noted.

---

## Site Directory Structure

```
my-site/
├── site.toml
├── configs/
│   ├── m6-html.conf
│   ├── m6-file.conf
│   └── <renderer>.conf
├── templates/
├── assets/
├── content/
└── data/
```

No binaries. No `logs/` directory. Processes log to stdout.

---

## Complete Annotated Example

```toml
[site]
name   = "Jane Smith"
domain = "example.com"

[server]
bind     = "0.0.0.0:443"
tls_cert = "/etc/letsencrypt/live/example.com/fullchain.pem"
tls_key  = "/etc/letsencrypt/live/example.com/privkey.pem"

[log]
level  = "info"    # "debug" | "info" | "warn" | "error"
format = "json"    # "json" | "text"

[errors]
mode = "status"    # "status" | "custom" | "internal"

# ── Backend pools ─────────────────────────────────────────────

[[backend]]
name    = "m6-html"
sockets = "/run/m6/m6-html-*.sock"

[[backend]]
name    = "m6-file"
sockets = "/run/m6/m6-file-*.sock"

[[backend]]
name    = "m6-auth"
sockets = "/run/m6/m6-auth-*.sock"

[[backend]]
name    = "render-contact"
sockets = "/run/m6/render-contact-*.sock"

# URL backend — remote HTTP/S server, not managed by systemd
[[backend]]
name = "remote-api"
url  = "https://api.example.com"

# ── Auth ──────────────────────────────────────────────────────

[auth]
backend    = "m6-auth"           # backend name declared above
public_key = "/etc/m6/auth.pub"  # m6-auth's public key for local JWT verification

# ── Routes ───────────────────────────────────────────────────

[[route]]
path    = "/"
backend = "m6-html"

[[route]]
path    = "/about"
backend = "m6-html"

[[route]]
path    = "/blog"
backend = "m6-html"

[[route_group]]
glob    = "content/posts/*.json"
path    = "/blog/{stem}"
backend = "m6-html"

[[route]]
path    = "/_errors"
backend = "m6-html"

[[route]]
path    = "/contact"
backend = "render-contact"

# Protected route — JWT must satisfy group membership
[[route]]
path    = "/admin/{page}"
backend = "m6-html"
require = "group:editors"

# Auth endpoints — handled by m6-auth directly
[[route]]
path    = "/auth/login"
backend = "m6-auth"

[[route]]
path    = "/auth/logout"
backend = "m6-auth"

[[route]]
path    = "/auth/refresh"
backend = "m6-auth"

# Log dashboard
[[route]]
path    = "/logs"
backend = "m6-html"
require = "group:admins"

[[route_group]]
glob    = "*.log"
path    = "/logs/{filename}"
backend = "m6-file"
require = "group:admins"

# Static assets
[[route_group]]
glob    = "assets/**/*"
path    = "/assets/{relpath}"
backend = "m6-file"

# Remote backend
[[route]]
path    = "/api/{rest}"
backend = "remote-api"
```

---

## Config Layering

`site.toml` travels with the site — version controlled, deployed via rsync. It contains no secrets and no environment-specific values. It can be public.

Server-specific values (`[server]` bind address, TLS cert paths) live in a system config — a required second argument to m6-http. Renderer secrets (SMTP credentials etc.) are referenced via a `secrets_file` key in the renderer config.

### m6-http system config

```
m6-http <site-dir> <system-config>
```

The second argument is required. In development it points to a checked-in dev system config. In production the systemd unit points to `/etc/m6/my-blog.toml`.

```ini
# Development dev.sh
m6-http "$SITE" "$SITE/configs/m6-http-dev.toml"

# Production systemd unit
ExecStart=/usr/local/bin/m6-http /var/www/my-blog /etc/m6/my-blog.toml
```

The system config contains only `[server]`. Everything else — routes, backends, site identity, log level, auth — comes from `site.toml` unchanged.

**Merge semantics:** system config is loaded after `site.toml`. Its `[server]` keys win on conflict. Any non-`[server]` keys in the system config are ignored with a warning. Validation runs after merge.

**`site.toml` — in version control:**

```toml
[site]
name   = "My Blog"
domain = "example.com"

[server]
# Development values — overridden by system config in production
bind     = "127.0.0.1:8443"
tls_cert = "../../localhost.pem"
tls_key  = "../../localhost-key.pem"

[auth]
backend    = "m6-auth"
public_key = "keys/auth.pub"

[log]
level  = "info"
format = "text"

[errors]
mode = "internal"

# backends, routes — identical in dev and production
```

**`/etc/m6/my-blog.toml` — on the server, never in the repo:**

```toml
[server]
bind     = "0.0.0.0:443"
tls_cert = "/etc/letsencrypt/live/example.com/fullchain.pem"
tls_key  = "/etc/letsencrypt/live/example.com/privkey.pem"
```

Only `[server]`. Nothing else.

### Renderer secrets

Renderer configs declare `secrets_file` pointing to a TOML file outside the site directory. The secrets file is merged into the renderer config — secrets file wins on conflict. The key is optional: absent in development, present in production.

```toml
# configs/render-contact.conf — in version control
secrets_file = "/etc/m6/contact-secrets.toml"   # silently ignored if file absent

global_params = ["data/site.json"]

[[route]]
path     = "/contact"
template = "templates/contact.html"
params   = []
cache    = "no-store"

[smtp]
# Non-secret values — safe in version control
host = "localhost"
port = 1025
# Development fallback values when secrets_file absent:
from     = "dev@localhost"
to       = "dev@localhost"
username = ""
password = ""
```

```toml
# /etc/m6/contact-secrets.toml — on the server, never in the repo
[smtp]
host     = "smtp.postmarkapp.com"
port     = 587
username = "live-api-token"
password = "live-api-token"
from     = "noreply@example.com"
to       = "owner@example.com"
```

`secrets_file` applies only to renderer configs. `site.toml` has no secrets.

### What belongs where

| Setting | Location | Reason |
|---|---|---|
| `[server]` bind, tls_cert, tls_key | System config `/etc/m6/*.toml` | Differs per environment |
| SMTP credentials | `secrets_file` → `/etc/m6/*.toml` | Secret, must not be in repo |
| All routes and backends | `site.toml` | Identical in dev and production |
| Log level and format | `site.toml` | Not secret, fine in repo |
| Templates, params | Renderer configs in `configs/` | Identical in dev and production |

### `site.toml` is clean

With `[server]` in the system config and credentials in secrets files, `site.toml` contains no passwords, no cert paths, no environment-specific values. It is safe to publish. The repo can be public.


### `[site]`

| Key | Type | Required | Notes |
|---|---|---|---|
| `name` | string | yes | Available in templates as `site_name` |
| `domain` | string | yes | Available in templates as `site_domain` |

### `[server]`

| Key | Type | Required | Notes |
|---|---|---|---|
| `bind` | string | yes | `"address:port"` |
| `tls_cert` | string | yes | Absolute path, PEM chain |
| `tls_key` | string | yes | Absolute path, PEM key |

TLS cert and key watched via inotify — reloaded on change without restart.

### `[log]`

| Key | Type | Default | Notes |
|---|---|---|---|
| `level` | string | `"info"` | `"debug"` `"info"` `"warn"` `"error"` |
| `format` | string | `"json"` | `"json"` or `"text"` |

All processes log to stdout. `format` applies to all processes that read this config.

### `[errors]`

Controls how m6-http handles error responses — both backend failures (no pool instance available) and non-2xx responses returned by backends.

| Key | Type | Default | Notes |
|---|---|---|---|
| `mode` | string | `"status"` | `"status"` `"internal"` `"custom"` |
| `path` | string | none | Required when `mode = "custom"`. Route m6-http fetches for error pages. |

| Mode | Behaviour |
|---|---|
| `"status"` | HTTP status code, empty body |
| `"internal"` | HTTP status code, m6-http generates a minimal styled HTML page |
| `"custom"` | m6-http fetches `path?status=N&from=/original-path`. Falls back to `"internal"` if the fetch fails. |

**Custom error flow:**

When a backend returns a non-2xx response, or no backend instance is available, and `mode = "custom"`:

1. m6-http makes an internal GET request to `<[errors] path>?status=404&from=/original-path`
2. The backend (typically m6-html) renders an error page using the `status` and `from` query params
3. m6-http returns that rendered page to the client with the original status code
4. If the error page fetch itself fails, m6-http falls back to `"internal"` mode

The error path must be declared as a route in `site.toml` pointing to the backend that handles it.

```toml
[errors]
mode = "custom"
path = "/_errors"    # required when mode = "custom"

[[route]]
path    = "/_errors"
backend = "m6-html"
```

Error responses are never cached regardless of mode.

### `[auth]`

| Key | Type | Required | Notes |
|---|---|---|---|
| `backend` | string | yes | Name of a declared `[[backend]]` |
| `public_key` | string | yes | Path to m6-auth's public key for local JWT verification |

### `[[backend]]`

| Key | Type | Required | Notes |
|---|---|---|---|
| `name` | string | yes | Referenced by `[[route]]` `backend` field |
| `sockets` | string | one of | Glob — `/run/m6/m6-html-*.sock`. Discovered via inotify. |
| `url` | string | one of | Remote HTTP/S server. Not pool-managed. |

`sockets` and `url` are mutually exclusive.

**Socket pool behaviour:** m6-http watches the directory containing the glob for socket appearance and disappearance. New sockets matching the glob are added to the pool. Disappeared sockets are removed. Load balancing uses least-connections. A failed connection attempt removes the socket from the active pool temporarily; retried after backoff.

**URL backend:** HTTP version negotiated via ALPN (`https://`). HTTP/1.1 for `http://`. Not load-balanced — single upstream.

### `[[route]]`

| Key | Type | Required | Notes |
|---|---|---|---|
| `path` | string | yes | Exact or parameterised — `{stem}`, `{relpath}`, etc. |
| `backend` | string | yes | Name of a declared `[[backend]]` |
| `require` | string | no | `"group:<name>"` or `"role:<name>"` — JWT must satisfy |

### `[[route_group]]`

| Key | Type | Required | Notes |
|---|---|---|---|
| `glob` | string | yes | Site-directory-relative file glob |
| `path` | string | yes | URL pattern with glob variables |
| `backend` | string | yes | Name of a declared `[[backend]]` |
| `require` | string | no | Same as `[[route]]` |

Glob is expanded at startup and on `site.toml` reload. New files matching the glob require a `site.toml` reload to become routable — renderers that create new content files (e.g. a CMS publish action) should touch `site.toml` after writing to trigger the reload.

### Glob Variables

| Variable | Expands to |
|---|---|
| `{stem}` | Filename without extension — `hello-world` |
| `{filename}` | Filename with extension — `hello-world.json` |
| `{relpath}` | Path relative to non-wildcard prefix |
| `{dir}` | Directory of matched file |

---

## Route Matching

Specificity wins: exact paths beat parameterised, longer beats shorter. First declaration breaks ties among equal-specificity patterns (with a warning logged).

---

## Request Forwarding

Complete request forwarded — method, path, query string, all headers, body.

**Added:** `X-Forwarded-For`, `X-Forwarded-Proto: https`, `X-Forwarded-Host`

**Removed (hop-by-hop):** `Connection`, `Upgrade`, `Keep-Alive`, `Transfer-Encoding`

---

## Signal Handling

| Signal | First receipt | Second receipt |
|---|---|---|
| `SIGTERM` or `SIGINT` | Clean shutdown | Immediate exit |

m6-http clean shutdown: stop accepting new connections, drain in-flight requests, close, exit 0.

Renderers clean shutdown: finish current request, close socket, exit 0.

---

## Exit Codes

| Code | Meaning |
|---|---|
| `0` | Clean shutdown |
| `1` | Runtime error |
| `2` | Config or usage error — before binding |

---

## Startup Validation

Exit 2 on:

- `[site]`, `[server]` required keys absent
- `tls_cert` or `tls_key` absent, file not found, or one without the other
- Any route missing `path` or `backend`
- Any route `backend` value not declared in `[[backend]]`
- Duplicate `path` values across `[[route]]`
- `[log] level` unrecognised
- `[auth]` declared but `public_key` file not found
- `[auth]` declared but `backend` not declared in `[[backend]]`
- `require` on a route but no `[auth]` declared

Warnings (not fatal):

- `[[route_group]]` glob matching no files at startup
- Local renderer config key overriding a global key

---

## Minimal Development Config

```toml
[site]
name   = "My Site"
domain = "localhost"

[server]
bind     = "127.0.0.1:8443"
tls_cert = "localhost.pem"
tls_key  = "localhost-key.pem"

[[backend]]
name    = "m6-html"
sockets = "/run/m6/m6-html-*.sock"

[[backend]]
name    = "m6-file"
sockets = "/run/m6/m6-file-*.sock"

[[route]]
path    = "/"
backend = "m6-html"

[[route_group]]
glob    = "assets/**/*"
path    = "/assets/{relpath}"
backend = "m6-file"
```
