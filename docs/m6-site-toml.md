# site.toml reference

`site.toml` is the config file every m6 process of a site reads.
It holds the site identity, the routes, the backends and the log and error settings.
The page states which key belongs in `site.toml` and which belongs in the process config.

## Contents

1. [Site directory structure](#1-site-directory-structure)
2. [Complete annotated example](#2-complete-annotated-example)
3. [Config layering](#3-config-layering)
4. [Key reference](#4-key-reference)
5. [Route Matching](#5-route-matching)
6. [Request Forwarding](#6-request-forwarding)
7. [Signal Handling](#7-signal-handling)
8. [Exit Codes](#8-exit-codes)
9. [Startup Validation](#9-startup-validation)
10. [Minimal Development Config](#10-minimal-development-config)

## 1. Site directory structure

The site directory holds one `site.toml` and the files each process needs.
Figure 1 gives the shape.

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

**Figure 1: the site directory.**
`site.toml` sits at the top, and `configs/` holds each renderer's own config.

No binaries and no `logs/` directory belong here.
Every process logs to stdout, so the directory holds data and config only.

## 2. Complete annotated example

A complete site is two files.
The first is `site.toml`, which every process reads for the state the whole site shares.
The second is the system config, which m6-http reads for this node's own state.

`site.toml`:

```toml
[site]
name   = "My Blog"
domain = "example.com"

[log]
level  = "info"     # "debug" | "info" | "warn" | "error"
format = "json"     # "json" | "text"

[errors]
mode = "status"     # "status" | "custom" | "internal"

# Backend pools

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

# A remote backend over HTTP or HTTPS
[[backend]]
name = "remote-api"
url  = "https://api.example.com"

# Routes

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

# A protected route: the token must satisfy the group claim
[[route]]
path    = "/admin/{page}"
backend = "m6-html"
require = "group:editors"

# Auth endpoints, served by m6-auth itself
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

The system config:

```toml
[server]
bind     = "0.0.0.0:443"
tls_cert = "/etc/letsencrypt/live/example.com/fullchain.pem"
tls_key  = "/etc/letsencrypt/live/example.com/privkey.pem"

[auth]
backend    = "m6-auth"           # the backend declared above
public_key = "/etc/m6/auth.pub"  # m6-auth's public key, used to verify tokens locally
```

The example keeps the bind address, the certificate and key, and the `[auth]` section in the system config.
Section 3 states how the two files merge at start.

## 3. Config layering

Each m6 process reads `site.toml` and its own process config.
For m6-http the process config is the system config, and for a renderer it is the app config.
`m6-architecture.md` §8 gives the reason for the split.

### 3.1. m6-http system config

m6-http takes two arguments, the site directory and the system config.

```
m6-http <site-dir> <system-config>
```

The second argument is required.
In development it names a checked-in dev system config.
In production the service unit names the server copy.

```sh
# Development script
m6-http "$SITE" "$SITE/configs/m6-http-dev.toml"

# Production service unit
ExecStart=/usr/local/bin/m6-http /var/www/my-blog /etc/m6/my-blog.toml
```

The system config accepts `[server]`, `[node]`, `[auth]` and three `[health]` keys, `metrics_require`, `metrics_peers` and `metrics_token_file`.
Everything else comes from `site.toml` unchanged.

Section 2 shows the two files.
In that example, development and production differ in the bind address and the certificate paths.

The merge rules, in order:

1. The system config loads after `site.toml`.
2. Its `[server]` keys win on conflict, and so do its three `[health]` keys.
3. A relative `[auth] public_key` in the system config resolves against the site directory, as `tls_cert` does.
4. An absolute path is used as given.
5. Any other top-level key in the system config is ignored with a warning, and any other `[health]` key there fails to parse.
6. Validation runs after the merge.
7. When both arguments name one file, its `[auth]` and `[health]` belong to the site config alone.
8. A key read only at start keeps its running value, and the log names it and says to restart.

The `[auth]`, `[health]` and `[server]` sections after section 3 state the one-file, `metrics_token_file` and touch-reload rules.

### 3.2. Renderer secrets

A renderer config may declare `secrets_file`, a path to a TOML file that sits outside the site directory.
The secrets file holds the values the repo must not carry, such as mail credentials.

The merge rule is one owner per key.
A key set in both the config and the secrets file is refused at start, and the error names both files.
A key set only in the secrets file is added to the merged config.
A key set only in the config stays as written.
`secrets_file` is optional, and an absent secrets file is ignored.

The config, in version control:

```toml
secrets_file = "/etc/m6/contact-secrets.toml"

global_params = ["data/site.json"]

[[route]]
path     = "/contact"
template = "templates/contact.html"
params   = []
cache    = "no-store"

[smtp]
host = "smtp.provider.example"
port = 587
```

The secrets file, on the server and never in the repo:

```toml
[smtp]
username = "mail-user"
password = "mail-password"
```

`secrets_file` applies to renderer configs only.
`site.toml` has no secrets.

### 3.3. What belongs where

The rule is that `site.toml` holds the state every node shares, and the process config holds the state of one node.
Table 1 maps each key to its file and names the reason.
Take from it that App `[log]` is the one key with two homes, and every other key has one.

| Setting | File it belongs in | Reason |
|---|---|---|
| `[site]` name, domain, redirect_www, describedby | `site.toml` | The identity every node shares |
| `[node]` name | System config | Names this node |
| `[server]` bind, tls_cert, tls_key, h2c_bind, internal_binds, internal_peers, redirect_bind, warm_on_start | System config | Differs per node |
| `[server]` backend_timeout_secs, allowed_methods | `site.toml` | How the site's routes are served, the same on every node |
| `[auth]` | System config | A verify key path is a credential path |
| `[health]` metrics_require, metrics_peers | System config | Which peers may read a node's metrics is a property of the node |
| `[health]` metrics_token_file | System config | A credential path. A relative path resolves against the site directory |
| `[health]` enabled, path, perf_path, traffic_path, traffic_cache_s | `site.toml` | The endpoint layout every node and the monitor share |
| `[log]`, `[analytics]`, `[rate_limit]`, `[errors]`, `[security]` | `site.toml` | Identical on every node |
| `[[backend]]`, `[[route]]`, `[[route_group]]` | `site.toml` | Identical in development and production |
| App `[thread_pool]`, `[params_cache]`, `[server]`, `[compression]`, `[minification]`, `[[route]]`, `global_params` | App config | The app's own state |
| App `[log]` | Both | `site.toml` sets the site default and the app config overrides it |
| App `site_toml` | App config | Names where `site.toml` is |
| Mail credentials | `secrets_file` | Secret, held outside the repo |

**Table 1: which file owns each key.**

m6-http accepts most of these keys in either file, and the system config wins.
A misplaced key works, so m6-http logs a warning at start naming the key and the file it belongs in.
The warning is silent when both arguments name one file.
A `[node]` in `site.toml` is never read, so it is warned about too.
A system config with no `[node]` logs a warning that the node name falls back to `[site].name`.
A secrets file is part of its process config.

### 3.4. Where an app finds `site.toml`

An app reads `<site_dir>/site.toml` unless its config sets the reserved key `site_toml`.
A relative value resolves against `site_dir`.
An absolute value is used as given.
An app with its own root under `apps/` sets `site_toml` to reach the file at the top of the tree.

```toml
# apps/file/m6-file.conf
site_toml = "../../site.toml"
```

A named file that does not exist, cannot be read or is not valid TOML stops the app at start with exit 2.
A file at the default path may be absent or invalid, as before.
`--dump-config` prints the resolved path on a `site.toml` line.

### 3.5. `site.toml` is clean

`site.toml` is safe to publish.
`m6-architecture.md` §8.1 states the split that keeps it clean.

## 4. Key reference

### 4.1. `[site]`

| Key | Type | Required | Notes |
|---|---|---|---|
| `name` | string | yes | Available in templates as `site_name` |
| `domain` | string | yes | Available in templates as `site_domain` |

### 4.2. `[server]`

| Key | Type | Required | Notes |
|---|---|---|---|
| `bind` | string | yes | `"address:port"` |
| `tls_cert` | string | yes | Absolute path, PEM chain |
| `tls_key` | string | yes | Absolute path, PEM key |
| `h2c_bind` | string | no | `"ip:port"`. Plain-TCP HTTP/2 listener for a private backbone, which needs `internal_binds` |
| `internal_binds` | list of strings | when `h2c_bind` is set | Set to `h2c_bind`, an `"ip:port"` in a local range, to make the h2c listener internal. `[]` means none |
| `internal_peers` | list of strings | when `internal_binds` is not empty | IP addresses in a local range. The only peers the internal h2c listener accepts, and a connection from any other address is closed |

The certificate and key files are not watched.
A renewal writes both, then touches the system config, and m6-http reads them on the touch, on HTTP/1.1, HTTP/2 and HTTP/3.
Until the touch, m6-http keeps serving the previous certificate.

An internal listener is one that `internal_binds` names.
m6-http never infers it from an address.
On an internal listener the statistics channel is labelled `internal`, and the custom error path is served to the caller.
On an internal `h2c_bind` listener m6-http also believes the client address in the `X-Forwarded-For` that a cache node sends, so each visitor behind a cache node keeps their own rate limit.
Only the h2c listener can be internal, so an entry that equals `bind` is refused.
Every entry must be in a local range, which means loopback, private (RFC 1918), link-local, shared (100.64.0.0/10) or IPv6 unique-local.
Host names are never resolved and a wildcard address such as `0.0.0.0` is refused.
The list is read at startup, so a change needs a restart.
The system config wins, and its list replaces the one in `site.toml` as a whole.
A redirect-mode config ignores the key.

An internal listener accepts a connection only from an address in `internal_peers`, and closes any other before reading a request.
So forwarded addresses are believed only from the peers the config names, whether or not a firewall is in place.
Each entry is an IP address in a local range, without a port.
The internal h2c listener is for relaying nodes.
A monitor polls a node over its TLS listener.
So `internal_peers` both admits a peer and trusts the client addresses that peer forwards.
An address that is listed must be one the peer connects from, and the list needs at least one address of the bind's IP family.
The log writes a line for the first refused connection from each address, then at most one line per address every 60 seconds with the count since the last line.
It remembers 256 addresses.
A new address past that takes the place of the remembered address whose last line is oldest, when that line is more than 60 seconds old and no count is owed.
When no remembered address qualifies, refusals from the new address are counted.
Across all addresses it writes at most 32 of those lines in 60 seconds, and refusals past that limit are counted.
The first refusal after the 60 seconds ends writes one summary line with the counts, the number of distinct counted addresses (up to 1024) and the most recent counted address.
The list is read at startup, and the system config's list replaces the one in `site.toml` as a whole.

```toml
[server]
bind           = "0.0.0.0:443"
h2c_bind       = "10.0.0.1:8080"
internal_binds = ["10.0.0.1:8080"]
internal_peers = ["10.0.0.2", "10.0.0.3"]
```

### 4.3. `[log]`

| Key | Type | Default | Notes |
|---|---|---|---|
| `level` | string | `"info"` | `"debug"` `"info"` `"warn"` `"error"` |
| `format` | string | `"json"` | `"json"` or `"text"` |

All processes log to stdout. `format` applies to all processes that read this config.

### 4.4. `[errors]`

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

### 4.5. `[auth]`

| Key | Type | Required | Notes |
|---|---|---|---|
| `backend` | string | no | Name of a declared `[[backend]]`. Checked when given. Verification does not use it |
| `public_key` | string | yes | Path to m6-auth's public key for local JWT verification |

`[auth]` may sit in `site.toml` or in the system config, never both.
A config that only verifies tokens needs `public_key` alone.

### 4.6. `[health]`

The monitoring endpoints, answered inside m6-http ahead of routing, the cache and every backend.

| Key | Type | Default | Notes |
|---|---|---|---|
| `enabled` | bool | `true` | Turns all three endpoints off |
| `path` | string | `"/health"` | Always public |
| `perf_path` | string | `"/perf"` | Counters, latency percentiles and host facts |
| `traffic_path` | string | `"/traffic"` | Summary of the last hour of the analytics log |
| `traffic_cache_s` | integer | `60` | Seconds a `/traffic` summary is reused |
| `metrics_token_file` | string | none | The legacy gate. Path to a file holding a bearer string. May also sit in the system config, where it wins and a relative path resolves against the site directory. In `site.toml` a relative path resolves against the working directory, so use an absolute path. Ignored when `metrics_require` is set |
| `metrics_require` | string | none | `"role:<name>"` or `"group:<name>"`. A token with this claim opens `/perf` and `/traffic`. It comes from the Authorization header or the session cookie, as route `require` reads it |
| `metrics_peers` | list of strings | none | IP addresses allowed to read `/perf` and `/traffic`. Anyone else gets 404 |

The system config accepts `metrics_require` and `metrics_peers` only.

**Who may read `/perf` and `/traffic`.**
A request is served only when both rules pass, checked in this order.

1. `metrics_peers`, when set: the request did not arrive on an internal h2c listener, and its peer address is one of the listed addresses.
   Otherwise the answer is 404 with the body of an endpoint that is not enabled, before any token is read.
2. `metrics_require`, when set: the request carries a token, from the Authorization header or the session cookie as route `require` reads it, that `[auth] public_key` verifies and that holds the claim.
   No token or a bad token is 401 with `WWW-Authenticate: Bearer`.
   A valid token without the claim is 403.
   When `metrics_require` is unset, the legacy `metrics_token_file` gate applies.
   Without either, the endpoints answer 404.

The peer address is the one a completed TCP or QUIC handshake proved, never a header.
That is why the endpoints are refused on an internal h2c listener, where the client address is the `X-Forwarded-For` value the listener believes.
An entry in `metrics_peers` is an IP address in a local range (`m6_core::net::is_private_ip`).
Names are never resolved, ranges are not accepted, and a public or wildcard address is a startup error.
An IPv4-mapped IPv6 peer matches the IPv4 entry.

`metrics_peers` composes with either gate.
`/health` ignores both keys.
On a host where a public interface could receive packets with a listed source address, filter them (reverse-path filtering), because a peer address that arrives on the wrong interface is still a completed handshake.
With none of the new keys set, behaviour is the legacy gate alone.

```toml
[auth]
public_key = "/etc/m6/api-auth.pub"

[health]
metrics_require = "role:monitor"
metrics_peers   = ["10.0.0.5"]
```

### 4.7. `[[backend]]`

| Key | Type | Required | Notes |
|---|---|---|---|
| `name` | string | yes | Referenced by `[[route]]` `backend` field |
| `sockets` | string | one of | Glob — `/run/m6/m6-html-*.sock`. Discovered via inotify. |
| `url` | string | one of | Remote HTTP/S server. Not pool-managed. |

`sockets` and `url` are mutually exclusive.

**Socket pool behaviour:** m6-http watches the directory containing the glob for socket appearance and disappearance. New sockets matching the glob are added to the pool. Disappeared sockets are removed. Load balancing uses least-connections. A failed connection attempt removes the socket from the active pool temporarily; retried after backoff.

**URL backend:** HTTP version negotiated via ALPN (`https://`). HTTP/1.1 for `http://`. Not load-balanced — single upstream.

### 4.8. `[[route]]`

| Key | Type | Required | Notes |
|---|---|---|---|
| `path` | string | yes | Exact or parameterised — `{stem}`, `{relpath}`, etc. |
| `backend` | string | yes | Name of a declared `[[backend]]` |
| `require` | string | no | `"group:<name>"` or `"role:<name>"` — JWT must satisfy |

### 4.9. `[[route_group]]`

| Key | Type | Required | Notes |
|---|---|---|---|
| `glob` | string | yes | Site-directory-relative file glob |
| `path` | string | yes | URL pattern with glob variables |
| `backend` | string | yes | Name of a declared `[[backend]]` |
| `require` | string | no | Same as `[[route]]` |

**The glob is relative to m6-http's site directory, and the file it names is resolved
by the backend relative to the BACKEND's root.** Those are different directories in a
site that gives each app its own root, so the glob has to account for it: a glob of
`apps/file/assets/**/*` producing the URL `/assets/{relpath}`, against an m6-file route
whose own `root` is `assets/`. Get the glob wrong and the routes never exist; get the
backend's `root` wrong and the routes exist while every request 404s. See
[`m6-site-layout.md`](m6-site-layout.md) and [`m6-file.md`](m6-file.md).

**Prefer a `[[route_group]]` to a wildcard route for static files.** A route_group
expands into one concrete route per file, and cache warming skips any route path
containing `{`, so a wildcard silently means those files are never pre-warmed.

Glob is expanded at startup and on `site.toml` reload. New files matching the glob require a `site.toml` reload to become routable — renderers that create new content files (e.g. a CMS publish action) should touch `site.toml` after writing to trigger the reload.
In an app, `req.touch("site.toml")` touches the file the app reads, which is the one its `site_toml` key names.

### 4.10. Glob Variables

| Variable | Expands to |
|---|---|
| `{stem}` | Filename without extension — `hello-world` |
| `{filename}` | Filename with extension — `hello-world.json` |
| `{relpath}` | Path relative to non-wildcard prefix |
| `{dir}` | Directory of matched file |

---

## 5. Route Matching

Specificity wins: exact paths beat parameterised, longer beats shorter. The edge builds its table with `matchit`, which refuses two patterns that match the same paths (`/blog/{slug}` and `/blog/{name}`), so such a `site.toml` fails to load.

---

## 6. Request Forwarding

Complete request forwarded — method, path, query string, all headers, body.

**Added:** `X-Forwarded-For`, `X-Forwarded-Proto: https`, `X-Forwarded-Host`

**Removed (hop-by-hop):** `Connection`, `Upgrade`, `Keep-Alive`, `Transfer-Encoding`

---

## 7. Signal Handling

| Signal | First receipt | Second receipt |
|---|---|---|
| `SIGTERM` or `SIGINT` | Clean shutdown | Immediate exit |

m6-http clean shutdown: stop accepting new connections, drain in-flight requests, close, exit 0.

Renderers clean shutdown: finish current request, close socket, exit 0.

---

## 8. Exit Codes

| Code | Meaning |
|---|---|
| `0` | Clean shutdown |
| `1` | Runtime error |
| `2` | Config or usage error — before binding |

---

## 9. Startup Validation

Exit 2 on:

- `[site]`, `[server]` required keys absent
- `tls_cert` or `tls_key` absent, file not found, or one without the other
- Any route missing `path` or `backend`
- Any route `backend` value not declared in `[[backend]]`
- Duplicate `path` values across `[[route]]`
- `[log] level` unrecognised
- `[auth]` declared but `public_key` file not found
- `[auth]` declared with a `backend` that is not declared in `[[backend]]`
- `[auth]` declared in both `site.toml` and the system config
- `[health] metrics_peers` empty, or an entry that is not an IP address (names are never resolved, ranges and ports are refused)
- `[health] metrics_peers` entry that is not in a local range
- `[health] metrics_require` without `[auth]`, or not `role:<name>` or `group:<name>`
- `require` on a route but no `[auth]` declared
- `internal_binds` entry that is not an IP address and port (names are never resolved)
- `internal_binds` entry that is not in a local range
- `internal_binds` entry that equals `bind` (only the h2c listener can be internal)
- `internal_binds` entry that is not equal to `h2c_bind`
- `h2c_bind` set and `internal_binds` absent (`internal_binds = []` is the explicit answer for a public listener)
- `internal_binds` not empty and `internal_peers` absent
- `internal_peers` empty
- `internal_peers` entry that is not an IP address (names are never resolved, ranges and ports are refused)
- `internal_peers` entry that is not in a local range
- `internal_peers` set with no internal listener
- `internal_peers` with no address of the internal bind's IP family (an IPv4-mapped bind is IPv4)

Warnings (not fatal):

- `[[route_group]]` glob matching no files at startup
- Local renderer config key overriding a global key

---

## 10. Minimal Development Config

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
