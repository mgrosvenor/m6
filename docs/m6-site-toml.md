# site.toml reference

`site.toml` is the config file every m6 process of a site reads.
It holds the site identity, the routes, the backends and the log and error settings.
The page states which key belongs in `site.toml` and which belongs in the process config.

## Contents

1. [Site directory structure](#1-site-directory-structure)
2. [Complete annotated example](#2-complete-annotated-example)
3. [Config layering](#3-config-layering)
4. [Key reference](#4-key-reference)
5. [Route matching](#5-route-matching)
6. [Request forwarding](#6-request-forwarding)
7. [Signal handling](#7-signal-handling)
8. [Exit codes](#8-exit-codes)
9. [Startup validation](#9-startup-validation)
10. [Minimal development config](#10-minimal-development-config)
11. [Summary](#11-summary)

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
level = "info"     # "debug" | "info" | "warn" | "error"

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

The system config accepts `[server]`, `[node]`, `[auth]` and two `[health]` keys, `metrics_require` and `metrics_peers`.
Everything else comes from `site.toml` unchanged.

Section 2 shows the two files.
In that example, development and production differ in the bind address and the certificate paths.

The merge rules, in order:

1. Each key has one file, Table 1 in §3.3.
2. A key in the other file, an unknown key or section in either file, or a system config with no `[node]` stops m6-http at start with exit 2, and the error names the key and the file it belongs in.
3. `tls_cert`, `tls_key` and `[auth] public_key` are absolute paths, and a relative one is a startup error, exit 2.
4. An absolute path is used as given.
5. Validation runs after both files are read.
6. When both arguments name one file, every key is read from it, `[node]` included.

The `[auth]`, `[health]` and `[server]` sections after section 3 state the one-file rules.

### 3.2. Renderer secrets

A renderer config may declare `secrets_file`, the absolute path of a TOML file outside the site directory.
The secrets file holds the values the repo must not carry, such as mail credentials.

The merge rule is one owner per key.
A key set in both the config and the secrets file is refused at start, and the error names both files.
A key set only in the secrets file is added to the merged config.
A key set only in the config stays as written.
`secrets_file` is optional, and an absent secrets file is ignored.
A relative `secrets_file` stops the renderer at startup with exit 2.

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
Take from it that the service `[log]` is the one key with two homes, and every other key has one.

| Setting | File it belongs in | Reason |
|---|---|---|
| `[site]` name, domain, redirect_www, describedby | `site.toml` | The identity every node shares |
| `[node]` name | System config | Names this node |
| `[server]` bind, tls_cert, tls_key, h2c_bind, internal_binds, internal_peers, redirect_bind, warm_on_start, max_requests_per_connection, reuse_port | System config | Differs per node |
| `[server]` backend_timeout_secs, allowed_methods | `site.toml` | How the site's routes are served, the same on every node |
| `[auth]` | System config | A verify key path is a credential path |
| `[limits.<name>]` | System config | The start and maximum of this node's buffers and queues, memory a node owns |
| `[health]` metrics_require, metrics_peers | System config | Which peers may read a node's metrics is a property of the node |
| `[health]` enabled, path, perf_path, traffic_path, traffic_cache_s | `site.toml` | The endpoint layout every node and the monitor share |
| `[log]`, `[analytics]`, `[rate_limit]`, `[errors]`, `[security]` | `site.toml` | Identical on every node |
| `[[backend]]`, `[[route]]`, `[[route_group]]` | `site.toml` | Identical in development and production |
| Service `[params_cache]`, `[server]`, `[compression]`, `[minification]`, `[[route]]`, `global_params` | Service config | The service's own state |
| Service `[limits.<name>]` | Service config | The start and maximum of the service's own buffers, `m6-app-anatomy.md` §4.2 |
| Service `[log]` | Both | `site.toml` sets the site default and the service config overrides it |
| Service `site_toml` | Service config | Names where `site.toml` is |
| Mail credentials | `secrets_file` | Secret, held outside the repo |

**Table 1: which file owns each key.**

m6-http reads each key from its file only.
A key in the other file stops the start with exit 2, and the error names the key and the file it belongs in.
The check does not run when both arguments name one file.
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

A file that does not exist, cannot be read or is not valid TOML stops the app at start with exit 2, whether the key names it or it is the default `<site_dir>/site.toml`.
`--dump-config` prints the resolved path on a `site.toml` line.

### 3.5. `site.toml` is clean

`site.toml` is safe to publish.
`m6-architecture.md` §8.1 states the split that keeps it clean.

## 4. Key reference

The key reference states a key's type, its default and what the loader does with a bad value.
The tables group the keys by the section that declares them, and the sections after this one state the behaviour those keys turn on.
Take from the tables that each key has one owner file and one type, and that an unknown key or section stops the start with exit 2.

### 4.1. `[site]`

Table 2 gives the two required `[site]` keys.
Take from it that both are required, and that both describe the site every node serves.

| Key | Type | Required | Notes |
|---|---|---|---|
| `name` | string | yes | The site's display name, written to the startup log line |
| `domain` | string | yes | The site's apex domain, compared with a request's `Host` and used for the `www.` redirect |

**Table 2: the `[site]` keys.**
The domain is the apex, so a value that names one node belongs in `[node]`.

### 4.2. `[server]`

Table 3 gives the `[server]` keys.
Take from it that the bind, the certificate and the key are required, and every other key has a default or a condition.

| Key | Type | Required | Notes |
|---|---|---|---|
| `bind` | string | yes | `"address:port"`. The public listener |
| `tls_cert` | string | yes | Absolute path to a *PEM* (Privacy-Enhanced Mail) chain |
| `tls_key` | string | yes | Absolute path to a PEM key |
| `h2c_bind` | string | no | `"ip:port"`. A plain-TCP HTTP/2 listener for a private backbone, which needs `internal_binds` |
| `internal_binds` | list of strings | when `h2c_bind` is set | Set to `h2c_bind`, an `"ip:port"` in a local range, to make the h2c listener internal. `[]` means none |
| `internal_peers` | list of strings | when `internal_binds` is not empty | IP addresses in a local range. The only peers the internal h2c listener accepts, and a connection from any other address is closed |
| `max_requests_per_connection` | integer | no | Default `100`, and `0` is a config error. The most requests one HTTP/1.1 connection serves before m6-http closes it, so a client that makes more than this many HTTP/1.1 requests reconnects, with a new TLS handshake. System config only, read at start |
| `reuse_port` | boolean | no | Default `false`. When `true`, only the public 443 port is shared: its TLS listener and QUIC UDP socket on `bind` are bound with `SO_REUSEPORT` in one call, so several m6-http processes serve that port. The redirect port 80, the internal h2c socket and every backend socket are not shared. Linux spreads a new connection by a hash and macOS does not, so the split is not even. System config only, read at start |

**Table 3: the `[server]` keys.**
The bind addresses and the internal lists choose where the process listens and whom it trusts, and the certificate and key choose what it presents.

`reuse_port` is off by default, so an accidental second m6-http on a node's public port fails at bind and an existing config binds exactly as before.
Set it to `true` on every process that should serve the public 443 port: only its TLS listener and its QUIC UDP socket on `bind` are bound in one call, so a request is answered by one of the processes.
That is the whole scope. No other socket is shared: the redirect listener on `redirect_bind` (port 80), the internal h2c listener on `h2c_bind` and every backend socket keep the binding they had.
The health endpoints are served on the shared listener, so a probe reaches one process.
A redirector binds one plain-HTTP listener on `redirect_bind` and has no TLS or QUIC listener to share.
Linux spreads a new TCP connection by a hash of its 4-tuple and macOS does not, so the split is not even and no process is guaranteed traffic.
A QUIC client that changes address may land on another process.

The per-IP limit in `[rate_limit]` is counted in each process's memory, and §4.13 states it.
With the default `requests_per_min = 300`, one process on the public port allows one client 300 requests in a 60-second window.
With `reuse_port`, three processes on that port allow one client up to 900 requests in the same window, because each process keeps its own count.
An operator who needs the node to enforce one number sets `requests_per_min` to the node total divided by the process count, or enforces the limit at the front door.

The certificate and key files are read once at start.
A renewal writes both and restarts m6-http, which then serves the new pair on HTTP/1.1, HTTP/2 and HTTP/3.
Until the restart, m6-http keeps serving the previous certificate.

An internal listener is one that `internal_binds` names, and m6-http never infers it from an address.
On an internal listener the statistics channel is labelled `internal`, the custom error path is served to the caller, and the `X-Forwarded-For` a cache node sends is believed, so each visitor behind that node keeps their own rate limit.

Every entry must be in a local range: loopback, private (RFC 1918), link-local, shared (100.64.0.0/10) or IPv6 unique-local.
Host names are never resolved, and a wildcard address such as `0.0.0.0` is refused.
Only the h2c listener can be internal, so an entry that equals `bind` is refused.

An internal listener accepts a connection only from an address in `internal_peers`, and closes any other before reading a request.
Forwarded addresses are believed only from the peers the config names, whether or not a firewall is in place.
Each entry is an IP address in a local range, without a port.
An address that is listed must be one the peer connects from, and the list needs at least one address of the internal listener's IP family.
The internal h2c listener is for relaying nodes, and a monitor polls a node over its TLS listener.
So `internal_peers` both admits a peer and trusts the client addresses that peer forwards.
A request on the TLS or QUIC listener from a private network address other than loopback is counted as monitor traffic, so a monitor on the private network does not move the visitor counters.
A real visitor reaches that listener from a public address.
A request from a public address is monitor only on `/health`, `/perf` or `/traffic`, and external on every other path.

Both lists are read at startup from the system config only, so a change needs a restart.
A redirect-mode config ignores the keys.

The log writes a line for the first refused connection from each address, then at most one line per address every 60 seconds with the count since the last line.
It remembers 256 addresses.
A new address past that takes the place of the remembered address whose last line is oldest, when that line is more than 60 seconds old and no count is owed.
When no remembered address qualifies, refusals from the new address are counted.
Across all addresses it writes at most 32 of those lines in 60 seconds, and refusals past that limit are counted.
The first refusal after the 60 seconds ends writes one summary line with the counts, the number of distinct counted addresses (up to 1024) and the most recent counted address.

The example below makes the h2c listener internal and admits two peers.

```toml
[server]
bind           = "0.0.0.0:443"
h2c_bind       = "10.0.0.1:8080"
internal_binds = ["10.0.0.1:8080"]
internal_peers = ["10.0.0.2", "10.0.0.3"]
```

### 4.3. `[log]`

Table 4 gives the one `[log]` key.
Take from it that the key has a default, so an absent section leaves the process at `info`.

| Key | Type | Default | Notes |
|---|---|---|---|
| `level` | string | `"info"` | `"trace"`, `"debug"`, `"info"`, `"warn"` or `"error"` |

**Table 4: the `[log]` key.**
The level names are case-insensitive.

All processes log to stdout as JSON lines.
A `format` key is an unknown key, so the start is refused with exit 2 and the message names it.

### 4.4. `[errors]`

The `[errors]` table chooses what m6-http sends for a backend failure or a non-2xx response from a backend.
Table 5 gives the keys and Table 6 gives the three modes.
Take from them that `mode` decides the body and `path` names the route that renders it.

| Key | Type | Default | Notes |
|---|---|---|---|
| `mode` | string | `"internal"` | `"status"`, `"internal"` or `"custom"` |
| `path` | string | none | Required when `mode = "custom"`. The route m6-http fetches for the error page |

**Table 5: the `[errors]` keys.**
The default mode already sends a page, so `mode` is only needed to choose a different one.

| Mode | Behaviour |
|---|---|
| `"status"` | The status code with an empty body |
| `"internal"` | The status code with a minimal *HTML* (HyperText Markup Language) page m6-http generates |
| `"custom"` | The status code with a page fetched from `path?status=N&from=/original-path`, and `"internal"` when the fetch fails |

**Table 6: the three error modes.**
`"custom"` needs its route to exist, because m6-http reaches the page through the route table like any other request.

A `mode` outside the three is read as `"internal"`.
[`m6-http.md`](m6-http.md) owns the custom flow, from the internal fetch and its query parameters to the fallback.
A page fetched in custom mode is held for reuse for 60 seconds per status, so a sustained run of one error costs one fetch a minute and a redeployed page appears within a minute.

The example below runs custom mode and declares the route the error page comes from.

```toml
[errors]
mode = "custom"
path = "/_errors"

[[route]]
path    = "/_errors"
backend = "m6-html"
```

### 4.5. `[auth]`

The `[auth]` table gives m6-http the key that verifies a *JWT* (JSON Web Token).
Table 7 gives its two keys.
Take from it that a site that only verifies tokens needs `public_key` alone.

| Key | Type | Required | Notes |
|---|---|---|---|
| `backend` | string | no | The name of a declared `[[backend]]`, checked when given. Verification does not use it |
| `public_key` | string | yes | Absolute path to m6-auth's public key for local JWT verification |

**Table 7: the `[auth]` keys.**
The name in `backend` is checked against the declared backends, and the key in `public_key` verifies every token.

`[auth]` sits in the system config only.

### 4.6. `[health]`

The `[health]` table configures the monitoring endpoints, which m6-http answers inside itself ahead of routing, the cache and every backend.
Table 8 gives the keys.
Take from it that the endpoint layout is site-wide and the two access keys belong to one node.

| Key | Type | Default | Notes |
|---|---|---|---|
| `enabled` | bool | `true` | Turns all three endpoints off |
| `path` | string | `"/health"` | Always public |
| `perf_path` | string | `"/perf"` | Counters, latency percentiles and host facts |
| `traffic_path` | string | `"/traffic"` | A summary of the last hour this process served, from memory. See [`m6-http.md`](m6-http.md) |
| `traffic_cache_s` | integer | `60` | Seconds a `/traffic` summary is reused |
| `metrics_require` | string | none | `"role:<name>"` or `"group:<name>"`. A token with this claim opens `/perf` and `/traffic`, and it comes from the Authorization header or the session cookie as a route `require` reads it. The one token check decides; without this key the endpoints answer 404 |
| `metrics_peers` | list of strings | none | The IP addresses allowed to read `/perf` and `/traffic`. Anyone else gets 404 |

**Table 8: the `[health]` keys.**
The two `metrics_*` keys are read from the system config only.

A request to `/perf` or `/traffic` is served only when both rules that are set pass, and the checks run in this order:

1. `metrics_peers`, when set: the request did not arrive on an internal h2c listener, and its peer address is one of the listed addresses.
   Otherwise the answer is 404 with the body of an endpoint that is not enabled, before any token is read.
2. `metrics_require`, when set: the request carries a token, from the Authorization header or the session cookie as a route `require` reads it, that `[auth] public_key` verifies and that holds the claim.
   No token or a bad token is 401 with `WWW-Authenticate: Bearer`.
   A valid token without the claim is 403.
   Without `metrics_require` the endpoints answer 404, so there is no second gate and no legacy bearer file.

The peer address is the one a completed TCP or QUIC handshake proved, never a header.
That is why the endpoints are refused on an internal h2c listener, where the client address is the `X-Forwarded-For` value the listener believes.

An entry in `metrics_peers` is an IP address in a local range (`m6_core::net::is_private_ip`).
Names are never resolved, ranges are not accepted, and a public or wildcard address is a startup error.
An IPv4-mapped IPv6 peer matches the IPv4 entry.

`metrics_peers` composes with the token check.
`/health` ignores both keys.
On a host where a public interface could receive packets with a listed source address, filter them with reverse-path filtering, because a peer address that arrives on the wrong interface is still a completed handshake.
When `metrics_require` and `metrics_peers` are unset, the endpoints answer 404.

The example below gates `/perf` and `/traffic` on a role and one peer address.

```toml
[auth]
public_key = "/etc/m6/api-auth.pub"

[health]
metrics_require = "role:monitor"
metrics_peers   = ["10.0.0.5"]
```

### 4.7. `[[backend]]`

The `[[backend]]` table declares one upstream m6-http can send a request to.
Table 9 gives the keys.
Take from it that a backend has one name and one upstream.

| Key | Type | Required | Notes |
|---|---|---|---|
| `name` | string | yes | The name a `[[route]]` or `[[route_group]]` `backend` field uses |
| `sockets` | string | one of | A glob, for example `/run/m6/m6-html-*.sock` |
| `url` | string | one of | A remote server. No socket pool |

**Table 9: the `[[backend]]` keys.**
A backend uses `sockets` or `url` for its upstream, and a route names the backend by `name`.

**Socket backend.**
m6-http expands the glob once at start and adds every matching socket to the backend's pool.
A glob that matches no socket stops the start with exit 2, before any listener binds.
A new or removed socket needs a restart of m6-http.
The pool sends each request to the next member in turn, so `N * k` requests put exactly `k` on each of `N` members.
A request whose connect fails tries the next member, and a client receives 502 only when every member has failed.
A backend that stops and restarts on the same path serves again on its next turn.

**URL backend.**
A URL backend names one remote upstream, so it has no socket pool and no load balancing.
The scheme chooses the protocol: `http://` and `https://` speak HTTP/1.1, and `h2c://` and `h2s://` speak HTTP/2.
[`m6-http.md`](m6-http.md) owns the four schemes and their connections.

### 4.8. `[[route]]`

The `[[route]]` table maps one path to one backend.
Table 10 gives the keys.
Take from it that a route is a path and a backend, with an optional claim.

| Key | Type | Required | Notes |
|---|---|---|---|
| `path` | string | yes | A literal path, or a pattern with `{param}` or `{*rest}` |
| `backend` | string | yes | The name of a declared `[[backend]]` |
| `require` | string | no | `"group:<name>"` or `"role:<name>"`. A token must satisfy it |
| `methods` | array of strings | no | The methods the route answers, such as `["POST"]`. Absent adds no restriction |
| `cache` | string | no | A `Cache-Control` value, such as `"no-store"`, that replaces the one the backend set on every reply this route gets. Absent leaves the backend's header alone |

**Table 10: the `[[route]]` keys.**
A `{param}` matches one segment, and a `{*rest}` in the last segment matches the rest of the path.
A route that names `cache` has that value sent in place of the `Cache-Control` the backend set, so one path can be `no-store` whatever its handler decides.
The edge applies it before it decides whether to store the reply, so `no-store` keeps the reply out of the cache, and a hit replays the stored value.
A route that lists `methods` answers exactly those: `GET` also answers `HEAD`, and a request with another method gets 405 with `Allow` before the cache or a backend is touched.
The names m6 accepts are `GET`, `POST`, `PUT`, `PATCH` and `DELETE`.
A name outside that set, or an empty list, stops the start with exit 2, and `--dump-config` fails the same way.
`[server] allowed_methods` remains the server-wide ceiling: a route narrows it and does not widen it.

### 4.9. `[[route_group]]`

The `[[route_group]]` table turns a file glob into one route per matched file.
Table 11 gives the keys.
Take from it that the glob is a path on disk and `path` is the URL the file is served at.

| Key | Type | Required | Notes |
|---|---|---|---|
| `glob` | string | yes | A file glob, relative to the site directory |
| `path` | string | yes | The URL pattern, with `{stem}`, `{filename}`, `{relpath}` or `{dir}` |
| `backend` | string | yes | The name of a declared `[[backend]]` |
| `require` | string | no | As `[[route]]` |
| `methods` | array of strings | no | As `[[route]]` |
| `cache` | string | no | As `[[route]]` |

**Table 11: the `[[route_group]]` keys.**
The glob is expanded once at start, so the routes a group adds are fixed until a restart.

The glob is relative to m6-http's site directory, and the backend serves the URL path from its own root.
Those are different directories in a site that gives each app its own root, so the glob accounts for it: `apps/file/assets/**/*` producing `/assets/{relpath}` against an m6-file route whose own `root` is `assets/`.
A wrong glob leaves the routes out of the table, and a wrong backend root leaves the routes in place while every request 404s.
[`m6-site-layout.md`](m6-site-layout.md) and [`m6-file.md`](m6-file.md) give the layout and the root.

Cache warming reads the explicit `[[route]]` entries only.
It skips a path that contains `{`, and it does not visit the paths a `[[route_group]]` expands to, so neither a wildcard route nor a route_group is pre-warmed.
A file that should be pre-warmed needs a concrete `[[route]]`.

### 4.10. Glob variables

Table 12 gives the four variables a `[[route_group]]` path can use.
Take from it that each variable is filled from the file the glob matched.

| Variable | Expands to |
|---|---|
| `{stem}` | The file name without its extension, for example `hello-world` |
| `{filename}` | The file name with its extension, for example `hello-world.json` |
| `{relpath}` | The path relative to the glob's non-wildcard prefix |
| `{dir}` | The directory of the matched file, relative to the glob's non-wildcard prefix |

**Table 12: the four `[[route_group]]` path variables.**
Every relative path is measured from the part of the glob before its first wildcard.

### 4.11. `[genid]`

The `[genid]` table gives a node one of two roles.
With the two claims it is the source of its site's generation ID, and it opens the endpoint that reads and advances that ID.
With `upstream` it is a cache node, and it follows the source's ID.
Table 13 gives its keys.
Take from it that the two shapes do not mix and that the table belongs to the system config.

| Key | Type | Default | Notes |
|---|---|---|---|
| `read_require` | string | none | `"role:<name>"` or `"group:<name>"`. A `GET /genid` needs a token with this claim. Source only |
| `write_require` | string | none | The same, for `POST /genid`, which advances the generation. Source only |
| `upstream` | string | none | A `[[backend]]` name whose entry has a `url`. Present makes this node a cache node. Cache node only |
| `token_file` | string | none | Absolute path to a token with the source's read claim. The heartbeat and a forwarded `/genid` send it. Cache node only |
| `heartbeat_secs` | integer | 10 | Seconds between heartbeats. Cache node only |

**Table 13: the `[genid]` keys.**
A source sets both claims or neither, and needs `[auth] public_key` for either.
A cache node sets `upstream` and `token_file`, and needs `[auth]` for nothing, because the source authorises the caller.

The start is refused with exit 2 for any of these.
A table with only one claim, a claim that names no role or group, or a claim without `[auth] public_key`.
`upstream` that names no `[[backend]]` with a `url`, or that is set without `token_file`.
`token_file` that is relative, unreadable or empty.
`heartbeat_secs = 0`.
`read_require` or `write_require` beside `upstream`.
`token_file` or `heartbeat_secs` without `upstream`.

`[genid]` belongs to the system config, because who may publish a site, and which node follows which, is one node's state and not something every node shares.
A source also needs `StateDirectory=` in its unit, because the ID is written to `$STATE_DIRECTORY/genid`.
A source with `[genid]` and no `StateDirectory=` exits 2, and `--dump-config` still prints and exits 0.
A cache node holds no file and needs no `StateDirectory=`.

```toml
[auth]
public_key = "/etc/m6/api-auth.pub"

[genid]
read_require  = "role:monitor"
write_require = "role:editor"
```

A cache node's table names the origin and the token it reads with.
The token file is a credential, delivered like every other one, for example `/run/credentials/<unit>/genid-read` from `LoadCredential=`.

```toml
[genid]
upstream       = "origin"
token_file     = "/run/credentials/m6-http.service/genid-read"
heartbeat_secs = 10
```

The generation endpoint is one node's own authority.
Section 4.12 adds the cache node's disk directory.
Section 5 states which route answers a request.

### 4.12. `[cache] dir`

A cache node may write the responses it caches to a directory it owns.
The table is system config only, because the directory is one node's state.
Table 14 gives its key.

| Key | Type | Default | Notes |
|---|---|---|---|
| `dir` | string | none | Absolute path to a directory the process can write. Cache node only |

**Table 14: the `[cache] dir` key.**

The directory must exist and be writable by the process.
The start creates and removes a probe file, `.m6-probe`, to prove both, and refuses the start with the io error and the path when it cannot.
A relative path is refused.
A source node is refused, because its cache sits beside its backends and a cold cache costs one local render per page.
A node without the table keeps its cache in memory only.

A cache node's layout is `<dir>/<genid decimal>/<64 hex characters>`, the file name being the SHA-256 of the cache key's bytes.
The writer removes the other generation directories under `dir` over its ticks, so the directory holds files for the generation the node follows.
The files are a cache: deleting the whole directory is safe, and costs a refill.

```toml
[cache]
dir = "/var/cache/m6-http"
```

The unit's `CacheDirectory=m6-http` creates `/var/cache/m6-http` and owns it.
Section 4.11 states how the generation directory is chosen.

### 4.13. `[rate_limit]`

m6-http counts requests from each client address in a fixed 60-second window, and it answers 429 with `Retry-After: 60` when the count passes the limit.
The table lives in `site.toml`, because every process of a site enforces the same limit.
Table 15 gives the keys.

| Key | Type | Default | Notes |
|---|---|---|---|
| `enabled` | boolean | `true` | `false` turns the limiter off |
| `requests_per_min` | integer | `300` | Requests allowed from one client address in a 60-second window. `0` refuses every request |

**Table 15: the `[rate_limit]` keys.**
Take from it that the limiter is on by default at 300 requests per client in 60 seconds.

The count is keyed by client address and kept in one process's memory.
One process on the public port enforces 300 requests per client per minute for the node.
With `[server] reuse_port`, N processes on one port keep N counts, so one client can reach up to N times `requests_per_min` across the port: three processes at the default allow 900 requests in a minute.
An operator who needs the node to enforce one number sets `requests_per_min` to the node total divided by N, or enforces the limit at the front door.

```toml
[rate_limit]
enabled = true
requests_per_min = 300
```

An idea for a later batch on Linux is a *reuseport socket filter* (*BPF*, Berkeley Packet Filter) attached to the shared listener, which hashes each client address to one process.
Every connection from one address then lands on the same process, and the per-process count equals the per-client count.
The filter is a new mechanism on the connection path, and this batch's rule is latency first with no new mechanisms, so the idea waits for a later batch.

### 4.14. `[limits]`

m6-http sizes its buffers and queues from named limits, each with a `start` and a `max`.
The table lives in the system config, because memory is a property of the node.
A `[limits]` table in `site.toml` stops the start with exit 2 in a two-file config, and the message names the system config.
The keys m6-http reads are `edge_request`, `edge_response`, `h2_recv`, `h2_header_block`, `h2_stream_body`, `refresh_queue`, `rate_limit_addresses`, `cache_bytes`, `connections`, `log_buffer` and `log_field`.
[`m6-app-anatomy.md`](m6-app-anatomy.md) §4.2 gives every limit with its defaults in Table 6, the growth rule, the rounding to the page size and the error a value past its maximum gets.

```toml
[limits.edge_request]
start = "4KiB"
max   = "32MiB"
```

Take from the example that a limit is two sizes, and that a request past `max` is refused with an error that names `[limits.edge_request] max` and the file to raise it in.

## 5. Route matching

m6-http answers a request from the cache first, and a cache hit is answered without selecting a route or contacting a backend.
A miss matches the request path against the routes from `site.toml`, and two rules decide which route answers.
A fixed route, whose every segment is a literal, beats a pattern route, which has a `{param}` or `{*rest}` segment.
A table with two routes that the rules cannot separate stops the start with exit 2, and `--dump-config` fails the same way.

Two routes conflict when both can match one path.
The cases are two fixed routes with the same path, and two pattern routes that can both match some path, such as `/blog/{slug}` with `/blog/{name}`, `/blog/{slug}` with `/blog/{*rest}`, or `/{lang}/about` with `/blog/{slug}`.
The edge applies the same rule and the same test as the services, so a `site.toml` route table and a backend's own route table are refused by one rule.

A `[[route]]` with no `methods` adds no method restriction of its own; with a list it answers exactly the ones it lists.
`GET` in the list answers `HEAD` too, so `methods = ["GET"]` answers both and a 405 from that route carries `Allow: GET, HEAD`.
The method check runs before routing and before the cache lookup, so a method the site or the route does not serve never reaches a backend: a known method gets 405 with `Allow`, and an invented one gets 501.
`OPTIONS` is decided at the edge and never forwarded: it is answered 405 with the route's `Allow`, because m6 offers no options of its own and a route cannot name it. A site that names `OPTIONS` in `[server] allowed_methods` still gets that refusal, with `OPTIONS` left out of `Allow`.
The status is 405, which says the method is the problem and the target resource exists (RFC 9110 15.5.6).

A request that reaches the route table therefore matches at most one route, and the method check has already accepted its method.
Section 6 follows that request to the backend.

## 6. Request forwarding

m6-http forwards the request to the backend a route names, with the proxy's own headers attached and the hop-by-hop fields removed.
[`m6-backend-protocol.md`](m6-backend-protocol.md) §2.2 and §2.5 own the fields the proxy sends and the fields it drops.
Section 7 states how the process stops when a signal arrives.

## 7. Signal handling

A process stops on `SIGTERM` or `SIGINT`, with a second signal forcing the exit, and reopens its log file on `SIGUSR1`.
Both receipts exit 0, so a supervisor reads a stop as a clean stop.
[`m6-http.md`](m6-http.md) owns the signal table and the shutdown sequence.
Section 8 gives the codes a failure returns.

## 8. Exit codes

m6-http exits 0 on a clean stop, 2 on a configuration or usage error, and 1 on a runtime failure.
A bind failure splits by listener: the TLS listener on `[server].bind` and the h2c listener on `h2c_bind` exit 1, and the HTTP/3 UDP socket on `[server].bind` exits 2.
With `[server] reuse_port` set, the TLS listener and the QUIC socket are bound in one call before the TLS configuration is built, so a failure to bind either exits 2.
[`m6-http.md`](m6-http.md) owns the exit-code table.
Exit 2 repeats on every start until the operator fixes the file, and section 9 lists the checks that produce it.

## 9. Startup validation

m6-http reads both files and validates the result before it serves a request.
Each check below stops the start with exit 2 and names what is wrong.
A mistake therefore costs a restart before the node serves.

Exit 2 on:

- `[site]` or `[server]` required keys absent
- `tls_cert` or `tls_key` absent, file not found, or one without the other
- `max_requests_per_connection = 0`
- A key in the file it does not belong to, in a two-file config
- A key or section m6-http does not know, in either file
- `[auth]` in `site.toml`, in a two-file config
- A system config with no `[node]`
- `[log] level` unrecognised
- `[limits]` with an unknown limit, an unknown key, a value that is not a size, a zero, or a `max` below its `start`
- Any route missing `path` or `backend`
- Any route `backend` value not declared in `[[backend]]`
- A `[[backend]]` `sockets` glob that matches no socket
- Duplicate `path` values across `[[route]]`
- `cache` on a `[[route]]` or a `[[route_group]]` that is empty or holds a control character
- A `methods` list naming a method other than `GET`, `POST`, `PUT`, `PATCH` or `DELETE`, or an empty list
- `[errors] mode = "custom"` without `path`
- `[auth]` declared but `public_key` file not found
- `[auth]` declared with a `backend` that is not declared in `[[backend]]`
- `require` on a route but no `[auth]` declared
- `tls_cert`, `tls_key` or `[auth] public_key` given as a relative path
- `[health] metrics_peers` empty, or an entry that is not an IP address (names are never resolved, ranges and ports are refused)
- `[health] metrics_peers` entry that is not in a local range
- `[health] metrics_require` without `[auth]`, or not `role:<name>` or `group:<name>`
- `--log-level` unrecognised
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

Every check above runs before the process serves a request, so a running service has passed every check and bound its listeners.
Section 10 gives a development pair that a developer can run.

## 10. Minimal development config

A development setup is the two files at their smallest.
The site directory holds a `site.toml` with one HTML backend, one file backend, one route and one route group, and `configs/system-dev.toml` holds the node name and the loopback listener.

```toml
# site.toml
[site]
name   = "My Site"
domain = "localhost"

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

```toml
# configs/system-dev.toml
[node]
name = "dev"

[server]
bind     = "127.0.0.1:8443"
tls_cert = "/tmp/m6/dev/dev.pem"
tls_key  = "/tmp/m6/dev/dev-key.pem"
```

The certificate and key are the development pair under `/tmp/m6/dev`, and m6-http reads both files at start.
`--dump-config` prints the merged result without binding a listener, which is how a deploy checks the pair before the node serves it.

The pair is the whole configuration a development node needs, and every key in it is in the tables above.
Section 11 closes with where the rest is documented.

## 11. Summary

The page gives the two files m6-http reads.
`site.toml` holds the state every node shares: the site identity, the routes, the backends, the log and the error settings.
The system config holds one node's state: the node name, the bind address, the certificate and the credential paths.
A key in the other file stops the start with exit 2, so a key has one owner.

Table 14 gives the pages that own the behaviour behind these keys.
Take from it that the server's internals, the renderer's own keys and the site layout each have a page of their own.

| document | covers |
|---|---|
| [`m6-architecture.md`](m6-architecture.md) §8 | Why the configuration splits into two files |
| [`m6-user-guide.md`](m6-user-guide.md) | The worked examples, including a development site and a production site |
| [`m6-http.md`](m6-http.md) | The server's pools, cache, error modes and signal handling |
| [`m6-app-anatomy.md`](m6-app-anatomy.md) | The renderer config, which is the process config of a service |
| [`m6-site-layout.md`](m6-site-layout.md) | One root per app, which a `[[route_group]]` glob has to account for |

**Table 14: further reading, and what each document covers.**

A site that runs with these two files and the keys in the tables is a site whose configuration m6-http has read and accepted.
