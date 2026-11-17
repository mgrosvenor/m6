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

The directory holds data and config, and the binaries live outside it.
No `logs/` directory belongs here: every process logs to stdout, which the journal holds and rotates.

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
level = "info"     # "trace" | "debug" | "info" | "warn" | "error"

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
sockets = "/run/m6/m6-auth.sock"

[[backend]]
name    = "m6-auth-check"
sockets = "/run/m6/check-m6-auth.sock"

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

# A protected route: the checker's identity must satisfy the group claim
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

The socket path comes from the config file's stem, so `configs/m6-auth.conf` gives the primary socket `/run/m6/m6-auth.sock`, and the check socket adds `check-` in front of that file name to give `/run/m6/check-m6-auth.sock`.

The system config:

```toml
[node]
name = "edge-a"

[server]
bind     = "0.0.0.0:443"
tls_cert = "/etc/letsencrypt/live/example.com/fullchain.pem"
tls_key  = "/etc/letsencrypt/live/example.com/privkey.pem"

[auth]
backend = "m6-auth-check"  # the backend whose members answer POST /check
```

The example keeps the node name, the bind address, the certificate and key, and the `[auth]` section in the system config.
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

The system config accepts `[server]`, `[node]`, `[auth]`, the `[genid]` claims, the cache directory and the named limits.
Everything else comes from `site.toml` unchanged.

Section 2 shows the two files.
In that example, development and production differ in the bind address and the certificate paths.

The merge rules, in order:

1. Each key has one file, Table 1 in §3.3.
2. A key in the other file, an unknown key or section in either file, or a system config with no `[node]` stops m6-http at start with exit 2, and the error names the key and the file it belongs in.
3. `tls_cert` and `tls_key` are absolute paths, and a relative one is a startup error, exit 2.
4. An absolute path is used as given.
5. Validation runs after both files are read.
6. When both arguments name one file, every key is read from it, `[node]` included.

### 3.2. Renderer secrets

A renderer config may declare `secrets_file`, the absolute path of a TOML file outside the site directory.
The secrets file holds the values the repo must not carry, such as mail credentials.

The merge rule is one owner per key.
A key set in both the config and the secrets file is refused at start, and the error names both files.
A key set only in the secrets file is added to the merged config.
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
`site.toml` has no secrets, so it is safe to publish, and `m6-architecture.md` §8.1 states the split that keeps it so.

### 3.3. What belongs where

The rule is that `site.toml` holds the state every node shares, and the process config holds the state of one node.
Table 1 maps each key to its file and names the reason.
Take from it that the service `[log]` is the one key with two homes, and every other key has one.

| Setting | File it belongs in | Reason |
|---|---|---|
| `[site]` name, domain, redirect_www, describedby | `site.toml` | The identity every node shares |
| `[node]` name | System config | Names this node |
| `[server]` bind, tls_cert, tls_key, h2c_bind, internal_binds, internal_peers, redirect_bind, warm_on_start, lock_memory, max_requests_per_connection, reuse_port, reuse_port_processes, process_index, status_bind, served_hosts, redirect_unmatched_to | System config | Differs per node |
| `[server]` backend_timeout_secs, allowed_methods | `site.toml` | How the site's routes are served, the same on every node |
| `[auth]` | System config | The checker it names runs on this node |
| `[genid]` | System config | Who may publish the site's generation, and which node follows which |
| `[cache] dir` | System config | The disk cache directory is one node's state |
| `[limits.<name>]` | System config | The start and maximum of this node's buffers and queues, memory a node owns |
| `[health]` enabled, path | `site.toml` | The endpoint layout every node shares |
| `[log]`, `[stats]`, `[analytics]`, `[rate_limit]`, `[errors]`, `[security]` | `site.toml` | Identical on every node |
| `[[backend]]`, `[[route]]`, `[[route_group]]` | `site.toml` | Identical in development and production |
| Service `[params_cache]`, `[server]`, `[compression]`, `[minification]`, `[[route]]`, `global_params` | Service config | The service's own state |
| Service `[limits.<name>]` | Service config | The start and maximum of the service's own buffers, `m6-app-anatomy.md` §4.2 |
| Service `[log]` | Both | `site.toml` sets the site default and the service config overrides it |
| Service `site_toml` | Service config | Names where `site.toml` is |
| Mail credentials | `secrets_file` | Secret, held outside the repo |

**Table 1: which file owns each key.**

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

## 4. Key reference

The tables in this section give each key's type and default, grouped by the section that declares it.
Take from them that each key has one owner file and one type, and that an unknown key or section stops the start with exit 2.

### 4.1. `[site]`

Table 2 gives the `[site]` keys.
Take from it that `name` and `domain` are required, and that every key here describes the site every node serves.

| Key | Type | Required | Notes |
|---|---|---|---|
| `name` | string | yes | The site's display name, written to the startup log line |
| `domain` | string | yes | The site's apex domain, compared with a request's `Host` and used for the `www.` redirect |
| `redirect_www` | boolean | no | Default `true`. A request for `www.<domain>` is answered 301 to `https://<domain><path>`, with the query kept. Only that alias redirects |
| `describedby` | string | no | Unset by default. The path of a machine-readable description of the site, conventionally `/llms.txt`. When set, an HTML response carries a `Link` header with `rel="describedby"` for the path |

**Table 2: the `[site]` keys.**
The domain is the apex, so a value that names one node belongs in `[node]`.

### 4.2. `[server]`

Table 3 gives the `[server]` keys.
Take from it that `bind`, the certificate and the key are required on a serving node, and every other key has a default or a condition.

| Key | Type | Required | Notes |
|---|---|---|---|
| `bind` | string | yes, or `redirect_bind` | `"address:port"`. The public listener |
| `tls_cert` | string | yes, except in redirect mode | Absolute path to a *PEM* (Privacy-Enhanced Mail) chain |
| `tls_key` | string | yes, except in redirect mode | Absolute path to a PEM key |
| `backend_timeout_secs` | integer | no | Default `30`. The seconds one backend request may take, connect, write and read included, before m6-http gives up on it |
| `h2c_bind` | string | no | `"ip:port"`. A plain-TCP HTTP/2 listener for a private backbone, which needs `internal_binds` |
| `internal_binds` | list of strings | when `h2c_bind` is set | Set to `h2c_bind`, an `"ip:port"` in a local range, to make the h2c listener internal. `[]` means none |
| `internal_peers` | list of strings | when `internal_binds` is not empty | IP addresses in a local range. The only peers the internal h2c listener accepts, and a connection from any other address is closed |
| `max_requests_per_connection` | integer | no | Default `100`, and `0` is a config error. The most requests one HTTP/1.1 connection serves before m6-http closes it, so a client that makes more than this many HTTP/1.1 requests reconnects, with a new TLS handshake. System config only, read at start |
| `reuse_port` | boolean | no | Default `false`. When `true`, only the public 443 port is shared: its TLS listener and QUIC UDP socket on `bind` are bound with `SO_REUSEPORT` in one call, so several m6-http processes serve that port. The redirect port 80, the internal h2c socket and every backend socket are not shared. Linux spreads a new connection by a hash and macOS does not, so the split is not even unless `reuse_port_processes` is set. System config only, read at start |
| `reuse_port_processes` | integer, at least 1 | no | Unset by default. With `reuse_port`, the number of m6-http processes that share the port. On Linux a socket filter attached to the TLS listener and the QUIC socket sends (a hash of the client's source address) mod this number to one socket, so one client address always reaches the same process, for HTTP/1.1, HTTP/2 and HTTP/3, over IPv4 and IPv6. Every process on the port must set the same number. macOS has no such filter and ignores it. System config only, read at start |
| `lock_memory` | boolean | no | Default `true`. Pins every page of the process in RAM at start, after the heap reservation and before the first request (`mlockall` on Linux, nothing on macOS), so a hot page is never a page fault and the cache is never swapped out. If the kernel refuses because `RLIMIT_MEMLOCK` is too low, m6-http stops at start with exit 2 and an error that names the limit; it never carries on unpinned when pinning was asked for. Under systemd set `LimitMEMLOCK=infinity` in the unit. Set `false` to run unpinned. System config only, read at start |
| `process_index` | integer | no | Unset by default. This process's number among the m6-http processes of one node, 0 to N-1, below `reuse_port_processes` when that is set. `/health` reports it as `process_index`, and with `status_bind` it picks this process's status port. System config only, read at start |
| `status_bind` | string, `"<address>:<base port>"` | no | Unset by default. With `reuse_port`, each process also answers `/health` on a TLS listener of its own at the base port plus its `process_index`, so a monitor reads the liveness of every process and not whichever one the kernel picked on 443. Every other path on it is 404. Needs `process_index`; a port past 65535 is exit 2. It is not in the `reuse_port` group, so nothing steers to it. Bind it to the node's tunnel address, never a public one. System config only, read at start |
| `redirect_bind` | string, `"address:port"` | no | Unset by default. Run this process as a plain-HTTP redirector on this address: every request is answered 301 to `https://<Host><path>`, or to `redirect_unmatched_to` when the Host is not in `served_hosts`. No TLS, no QUIC, no backends, no routes and no cache are built. System config only, read at start |
| `served_hosts` | list of strings | no | `[]` by default. The host names this node serves, matched against `Host` or `:authority` with any port stripped and ASCII case ignored. The list alone changes nothing, because the policy is off until `redirect_unmatched_to` is set. System config only, read at start |
| `redirect_unmatched_to` | string | no | Unset by default, which turns the policy off. A bare host name, for example `example.com`. A request outside `served_hosts`, including one with no Host, is answered 301 to `https://<redirect_unmatched_to><path>` with the query kept, before the cache and before routing. Refused when `served_hosts` is empty, when it carries a scheme, port, path or wildcard, and when it is an IP address. System config only, read at start |

**Table 3: the `[server]` keys.**
Every key except `backend_timeout_secs` and `allowed_methods` is system config only and read at start, and those two are in `site.toml`.
The bind addresses and the internal lists choose where the process listens and whom it trusts, and the certificate and key choose what it presents.

The unmatched-host policy sends a request for a name this node does not serve to the site's canonical name.
`served_hosts` names the served names, and `redirect_unmatched_to` names the destination.
Both are off by default.
A target with no served names is a config error, because every request would redirect, including the node's own name, and the deploy's checks and the monitor could not reach it.
The policy runs in the same position as the `www.` redirect, before the cache lookup and before routing, so a warm entry for a served name cannot answer a request for a name the node does not own.
The internal h2c listener is excluded, because its `:authority` is the backbone address, which no visitor sent.

`reuse_port` is off by default, so a second m6-http on a node's public port fails at bind.
Set it to `true` on every process that serves the public port.
Only the TLS listener and the QUIC UDP socket on `bind` are bound in one call, so one process answers each request.
The redirect listener on `redirect_bind`, the h2c listener on `h2c_bind` and every backend socket are not shared.
The health endpoints are served on the shared listener, so a probe reaches one process.
Without `reuse_port_processes`, Linux spreads a new TCP connection by a hash of its 4-tuple, which is uneven, and on macOS the process bound last gets every client.
With `reuse_port_processes = N` on Linux, a socket filter (classic BPF, `SO_ATTACH_REUSEPORT_CBPF`) hashes the client's source address and takes it mod N, so every connection and every QUIC datagram from one address reaches one process whatever its source port.
A QUIC client that changes address may land on another process.
The filter picks a place in the kernel's group of sockets.
While a process is stopped the group is short of N, and a client whose place is empty is chosen by the kernel's own hash.
A process that stops or starts also moves some clients, so a connection already open keeps its process and a QUIC client that moves must handshake again.
The mapping settles when all N processes are up.
Section 4.13 states how `[rate_limit]` counts across these processes.

The certificate and key files are read once at start (`m6-http/src/http11.rs:2687`).
A renewal writes both and restarts m6-http, which then serves the new pair on HTTP/1.1, HTTP/2 and HTTP/3.
Until that restart, m6-http keeps serving the previous certificate.

An internal listener is one that `internal_binds` names.
On an internal listener the statistics channel is labelled `internal`, the custom error path is served to the caller, and the `X-Forwarded-For` a cache node sends is believed, so each visitor behind that node keeps their own rate limit.

Every entry must be in a local range: loopback, private (RFC 1918), link-local, shared (100.64.0.0/10) or IPv6 unique-local.
Host names are never resolved, and a wildcard address such as `0.0.0.0` is refused.
Only the h2c listener can be internal, so an entry that equals `bind` is refused.

An internal listener accepts a connection only from an address in `internal_peers`, and closes any other before reading a request.
Each entry is an IP address in a local range, without a port.
An address that is listed must be one the peer connects from, and the list needs at least one address of the internal listener's IP family.
The internal h2c listener is for relaying nodes, and a monitor polls a node over its TLS listener.
So `internal_peers` both admits a peer and trusts the client addresses that peer forwards.
A request on the TLS or QUIC listener from a private network address other than loopback is counted as monitor traffic, so a monitor on the private network does not move the visitor counters.
A real visitor reaches that listener from a public address.
A request from a public address is monitor only on `/health`, and external on every other path.

A redirect-mode config ignores both lists.

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
A `format` key is an unknown key, so the start is refused with exit 2 and the message names it (`m6-core/src/log.rs:318`).

### 4.4. `[errors]`

The `[errors]` table chooses what m6-http sends for a backend failure or a non-2xx response from a backend.
Table 5 gives the keys and Table 6 gives the three modes.
Take from them that `mode` decides the body, `path` names the route that renders it, and `verbose_fallback` adds the request detail to the internal page.

| Key | Type | Default | Notes |
|---|---|---|---|
| `mode` | string | `"internal"` | `"status"`, `"internal"` or `"custom"` |
| `path` | string | none | Required when `mode = "custom"`. The route m6-http fetches for the error page |
| `verbose_fallback` | boolean | `false` | The internal error page carries the request path, the matched route, the backend name, the error detail and a hint. Set it for development, and leave it off in production |

**Table 5: the `[errors]` keys.**

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

The `[auth]` table names the authority m6-http asks about a credential.
Table 7 gives its one key.
Take from it that every gated path needs a running checker: a route `require` and a source node's `/genid`.

| Key | Type | Required | Notes |
|---|---|---|---|
| `backend` | string | yes | The name of a declared `[[backend]]` with `sockets`. Its members answer `POST /check`, and each is probed once at start. A `url` backend is refused by name (`m6-http/src/config.rs:1543`) |

**Table 7: the `[auth]` key.**
The members of `backend` are probed at start: 401 or 503 passes, 404 stops the start, and a refused connection warns (`m6-http/src/main.rs:7041`).
Every credential gate sends one `POST /check` under a 50 ms deadline: a route `require` and a source node's `read_require` and `write_require` (`m6-http/src/main.rs:1769`, `m6-http/src/auth.rs:11`).
A request with no credential is refused before the call.
A 401 from the checker is a refusal.
A 200 identity without the claim is 403.
A checker that does not answer, or answers something the edge cannot use, is 503 with `Retry-After: 1` (`m6-core/src/auth.rs:100`, `:112`).
`[auth] public_key` is removed, and a config that still sets it fails at TOML load (`m6-http/src/config.rs:833`).
A route with `require`, or a source with `read_require` or `write_require`, needs `[auth]`.
`[auth]` needs `backend` (`m6-http/src/config.rs:1556`, `:1587`, `:1702`).

### 4.6. `[health]`

The `[health]` table configures the one monitoring endpoint, which m6-http answers inside itself ahead of routing, the cache and every backend.
Table 8 gives the keys.
The metrics and traffic documents are log lines, not endpoints, so no key here opens a read of them and no credential is involved.

| Key | Type | Default | Notes |
|---|---|---|---|
| `enabled` | bool | `true` | Turns `/health` off |
| `path` | string | `"/health"` | Always public |

**Table 8: the `[health]` keys.**

`/health` has two answers, 200 `{"status":"ok"}` and 503 `{"status":"degraded"}`, and the status code carries the answer.
It answers from flags and probes nothing, so its cost is one JSON serialisation.
Section 11 of [`m6-http.md`](m6-http.md) states both bodies.

`[health] metrics_require`, `metrics_peers`, `perf_path`, `traffic_path` and `traffic_cache_s` are removed.
A config that still names one stops the start with exit 2, naming the unknown key, and the metrics and traffic documents are read from the journal instead.

### 4.7. `[[backend]]`

The `[[backend]]` table declares one upstream m6-http can send a request to.
Table 9 gives the keys.
Take from it that a backend has one name, one upstream and a declaration of whether it compresses.

| Key | Type | Required | Notes |
|---|---|---|---|
| `name` | string | yes | The name a `[[route]]` or `[[route_group]]` `backend` field uses |
| `sockets` | string | one of | A glob, for example `/run/m6/m6-html-*.sock` |
| `url` | string | one of | A remote server. No socket pool |
| `tls_skip_verify` | boolean | no | Default `false`. Skip TLS certificate verification for a `url` backend. For a test against a self-signed certificate |
| `compresses` | boolean | no | Default `true`. Whether this backend compresses its own responses. The edge uses it to decide whether to vary on `Accept-Encoding`, and a backend built on m6-core exits 2 when the declaration disagrees with its `[compression]`. [`m6-app-anatomy.md`](m6-app-anatomy.md) §4 owns the service side |

**Table 9: the `[[backend]]` keys.**
A backend sets one of `sockets` and `url`.

**Socket backend.**
m6-http expands the glob once at start and adds every matching socket to the backend's pool.
A glob that matches no socket stops the start with exit 2, before any listener binds.
A new or removed socket needs a restart of m6-http.
The pool sends each request to the next member in turn, so `N * k` requests put exactly `k` on each of `N` members.
A request whose connect fails tries the next member, and a client receives 502 only when every member has failed.
A backend that stops and restarts on the same path serves again on its next turn.

**URL backend.**
A URL backend names one remote upstream, with no load balancing.
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
| `require` | string | no | `"group:<name>"` or `"role:<name>"`. The checker's identity must satisfy it |
| `methods` | array of strings | no | The methods the route answers, such as `["POST"]`. Absent adds no restriction |
| `cache` | string | no | A `Cache-Control` value, such as `"no-store"`, that replaces the one the backend set on every reply this route gets. Absent leaves the backend's header alone. A reply that already carries `no-store` or `private` keeps that field |

**Table 10: the `[[route]]` keys.**
A `{param}` matches one segment, and a `{*rest}` in the last segment matches the rest of the path.
A route that names `cache` has that value sent in place of the `Cache-Control` the backend set, so one path can be `no-store` whatever its handler decides.
The edge applies it before it decides whether to store the reply, so `no-store` keeps the reply out of the cache, and a hit replays the stored value.
The override never removes a `no-store` or `private` the backend sent: that reply keeps the backend's field and the route value is not applied.
A response to a request that carried an `Authorization` field or a `session` cookie is never stored, whatever the route or the backend says.
A route that lists `methods` answers exactly those: `GET` also answers `HEAD`, and a request with another method gets 405 with `Allow` before the cache or a backend is touched.
The names m6 accepts are `GET`, `POST`, `PUT`, `PATCH` and `DELETE`.
A name outside that set, or an empty list, stops the start with exit 2, and `--dump-config` fails the same way.

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

`warm_on_start` visits concrete `[[route]]` paths only.
It skips a path that contains `{` and every path a `[[route_group]]` expands to, so a file to pre-warm needs a concrete `[[route]]`.

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
| `read_require` | string | none | `"role:<name>"` or `"group:<name>"`. A `GET /genid` needs an identity with this claim. Source only |
| `write_require` | string | none | The same, for `POST /genid`, which advances the generation. Source only |
| `upstream` | string | none | A `[[backend]]` name whose entry has a `url`. Present makes this node a cache node. Cache node only |
| `token_file` | string | none | Absolute path to a token with the source's read claim. The heartbeat and a forwarded `/genid` send it. Cache node only |
| `heartbeat_secs` | integer | 10 | Seconds between heartbeats. Cache node only |

**Table 13: the `[genid]` keys.**
A source sets both claims or neither, and needs `[auth] backend` for either (`m6-http/src/config.rs:1702`).
A source's `GET` and `POST /genid` send their credential to that checker too (`m6-http/src/main.rs:5377`).
A cache node sets `upstream` and `token_file`, and needs `[auth]` for nothing, because the source authorises the caller.

The start is refused with exit 2 for any of these.
A table with only one claim, a claim that names no role or group, or a claim without `[auth] backend`.
`upstream` that names no `[[backend]]` with a `url`, or that is set without `token_file`.
`token_file` that is relative, unreadable or empty.
`heartbeat_secs = 0`.
`read_require` or `write_require` beside `upstream`.
`token_file` or `heartbeat_secs` without `upstream`.

A source needs `StateDirectory=` in its unit, because the ID is written to `$STATE_DIRECTORY/genid`.
A source with `[genid]` and no `StateDirectory=` exits 2, and `--dump-config` still prints and exits 0.
A cache node holds no file and needs no `StateDirectory=`.

```toml
[auth]
backend = "m6-auth-check"

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

Section 4.12 adds the cache node's disk directory.
Section 5 states which route answers a request.

### 4.12. `[cache] dir`

A cache node may write the responses it caches to a directory it owns.
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

### 4.13. `[rate_limit]`

m6-http counts requests from each client address in a fixed 60-second window, and it answers 429 with `Retry-After: 60` when the count passes the limit.
Table 15 gives the keys.

| Key | Type | Default | Notes |
|---|---|---|---|
| `enabled` | boolean | `true` | `false` turns the limiter off |
| `requests_per_min` | integer | `300` | Requests allowed from one client address in a 60-second window. `0` refuses every request |

**Table 15: the `[rate_limit]` keys.**
Take from it that the limiter is on by default at 300 requests per client in 60 seconds.

The count is keyed by client address and kept in one process's memory.
One process on the public port enforces 300 requests per client per minute for the node.
On Linux with `[server] reuse_port` and `reuse_port_processes = N`, one client address always reaches the same process, so the limit is 300 requests per minute per client across the port, whatever N is.
On macOS, and on Linux with `reuse_port_processes` unset, N processes on one port keep N counts, so one client can reach up to N times `requests_per_min` across the port: three processes at the default allow 900 requests in a minute.
An operator who needs the node to enforce one number there sets `requests_per_min` to the node total divided by N, or enforces the limit at the front door.
While a process is stopped or starting, some clients move to another process and start a fresh count there, so the limit can briefly be looser until all N are up.

```toml
[rate_limit]
enabled = true
requests_per_min = 300
```

### 4.14. `[limits]`

m6-http sizes its buffers and queues from named limits, each with a `start` and a `max`.
The table lives in the system config, because memory is a property of the node.
A `[limits]` table in `site.toml` stops the start with exit 2 in a two-file config, and the message names the system config.
The keys m6-http reads are `edge_request`, `edge_response`, `h2_recv`, `h2_header_block`, `h2_stream_body`, `h2_send`, `refresh_queue`, `rate_limit_addresses`, `cache_bytes`, `connections`, `log_buffer`, `log_field` and `heap_reserve`.
`heap_reserve` is the heap m6-http reserves from the OS at start, on huge pages where the kernel has them: by default `cache_bytes` max plus 64 MiB, rounded to 2 MiB, so the cache is carved from it. A `start` below `cache_bytes` max is warned at start, naming both keys.
[`m6-app-anatomy.md`](m6-app-anatomy.md) §4.2 gives every limit with its defaults in Table 6, the growth rule, the rounding to the page size and the error a value past its maximum gets.

```toml
[limits.edge_request]
start = "4KiB"
max   = "32MiB"
```

Take from the example that a limit is two sizes, and that a request past `max` is refused with an error that names `[limits.edge_request] max` and the file to raise it in.

### 4.15. `[stats]`

The `[stats]` table sets how often m6-http writes its periodic log lines.
The table lives in `site.toml`, because the cadence is the same on every node.
Table 16 gives the keys.
Take from it that both intervals are whole seconds and that zero stops the start.

| Key | Type | Default | Notes |
|---|---|---|---|
| `interval_s` | integer | `10` | Seconds between the accounting tick, which writes the operational `periodic stats` line and the `type` `perf` line. `0` stops the start with exit 2 |
| `traffic_interval_s` | integer | `60` | Seconds between the `type` `traffic` lines. `0` stops the start with exit 2 |

**Table 16: the `[stats]` keys.**
The `type` `perf` line and the `type` `traffic` line go to stdout, or to journald's native socket under systemd, where `M6_TYPE=perf` and `M6_TYPE=traffic` select them.

The `type` `perf` line carries the whole metrics document under `fields.perf`, from `perf_document` in `m6-http/src/health.rs`.
The document holds the request, cache and latency figures, the pool and TLS state, the build identity and the host facts.
The `type` `traffic` line carries the traffic summary under `fields.traffic`, from `build_report` in the same file, over a window of up to the last 60 minutes.

A zero for either key is refused at start with exit 2, because the line would then be written on every pass of the event loop.

### 4.16. `[node]`

The `[node]` table names this process's node.
Table 17 gives its key.
Take from it that the name is required, and that it comes from the config of one node.

| Key | Type | Required | Notes |
|---|---|---|---|
| `name` | string | yes | This node's identity, for example `edge-a`. It appears in `/health`, the metrics line and the analytics record. System config only |

**Table 17: the `[node]` key.**

### 4.17. `[security]`

The `[security]` table sets the response headers m6-http adds to what it serves.
Table 18 gives the keys.
Take from it that `csp_mode` chooses which `Content-Security-Policy` header is sent, and every other key is a header value.
An empty value omits that header, and a backend's own value wins.

| Key | Type | Default | Notes |
|---|---|---|---|
| `hsts` | string | one year, with `includeSubDomains` and `preload` | `Strict-Transport-Security` |
| `x_content_type_options` | string | `nosniff` | `X-Content-Type-Options` |
| `x_frame_options` | string | `DENY` | `X-Frame-Options` |
| `referrer_policy` | string | `strict-origin-when-cross-origin` | `Referrer-Policy` |
| `content_security_policy` | string | `default-src 'self'`, with inline styles and `data:` images allowed | `Content-Security-Policy` |
| `csp_mode` | string | `"enforce"` | `"enforce"`, `"report-only"` or `"off"`. `report-only` sends `Content-Security-Policy-Report-Only`, and `off` sends neither |
| `permissions_policy` | string | `camera=(), microphone=(), geolocation=(), payment=(), usb=(), interest-cohort=()` | `Permissions-Policy` |
| `cross_origin_opener_policy` | string | `same-origin` | `Cross-Origin-Opener-Policy` |
| `cross_origin_resource_policy` | string | `same-origin` | `Cross-Origin-Resource-Policy` |

**Table 18: the `[security]` keys.**
The headers reach cache hits, error pages and rate-limit refusals as well as backend responses.

### 4.18. `[analytics]`

The `[analytics]` table turns the per-request access lines on and off.
Table 19 gives the one key.
Take from it that the lines are on by default, and that they go to the same log stream as every other line with `type` `access`.

| Key | Type | Default | Notes |
|---|---|---|---|
| `enabled` | boolean | `true` | Turns the per-request access lines off |

**Table 19: the `[analytics]` key.**
`[log]` controls the level and destination of the stream these lines share, and `[stats]` sets how often the metrics line is written.

Section 5 follows one request through the route table.

## 5. Route matching

m6-http answers a request from the cache first, and a cache hit is answered without selecting a route or contacting a backend.
A miss matches the request path against the routes from `site.toml`, and two rules decide which route answers.
A fixed route, whose every segment is a literal, beats a pattern route, which has a `{param}` or `{*rest}` segment.
A table with two routes that the rules cannot separate stops the start with exit 2, and `--dump-config` fails the same way.

Two routes conflict when both can match one path.
The cases are two fixed routes with the same path, and two pattern routes that can both match some path, such as `/blog/{slug}` with `/blog/{name}`, `/blog/{slug}` with `/blog/{*rest}`, or `/{lang}/about` with `/blog/{slug}`.
The edge applies the same rule and the same test as the services, so a `site.toml` route table and a backend's own route table are refused by one rule.

A `[[route]]` with no `methods` adds no method restriction of its own.
With a list it answers exactly the ones it lists.
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

A process stops on `SIGTERM` or `SIGINT`, with a second signal forcing the exit.
Both receipts exit 0, so a supervisor reads a stop as a clean stop.
[`m6-http.md`](m6-http.md) owns the signal table and the shutdown sequence.
Section 8 gives the codes a failure returns.

## 8. Exit codes

m6-http exits 0 on a clean stop, 2 on a configuration or usage error, and 1 on a runtime failure.
A bind failure splits by listener: the TLS listener on `[server].bind` and the h2c listener on `h2c_bind` exit 1, and the HTTP/3 UDP socket on `[server].bind` exits 2.
With `[server] reuse_port` set, the TLS listener and the QUIC socket are bound in one call before the TLS configuration is built, so a failure to bind either exits 2.
[`m6-http.md`](m6-http.md) owns the exit-code table.
Exit 2 repeats on every start until the operator fixes the file. Section 9 lists the checks that produce it.

## 9. Startup validation

m6-http reads both files and validates the result before it serves a request.
Each check below stops the start with exit 2 and names what is wrong.

Exit 2 on:

- `[site]` or `[server]` required keys absent
- `tls_cert` or `tls_key` absent, file not found, or one without the other
- `max_requests_per_connection = 0`
- A key in the file it does not belong to, in a two-file config
- A key or section m6-http does not know, in either file
- `[auth]` in `site.toml`, in a two-file config
- A system config with no `[node]`
- `[log] level` unrecognised
- `[stats].interval_s = 0` or `[stats].traffic_interval_s = 0`
- `[server] reuse_port_processes = 0`, a `process_index` that is not below `reuse_port_processes`, a `status_bind` with no `process_index`, or a `status_bind` port plus `process_index` past 65535
- `[server] served_hosts` entry that is not a bare host name
- `[server] redirect_unmatched_to` empty, carrying a scheme, port, path or wildcard, or naming an IP address
- `[server] redirect_unmatched_to` set with `served_hosts` empty
- `[limits]` with an unknown limit, an unknown key, a value that is not a size, a zero, or a `max` below its `start`
- Any route missing `path` or `backend`
- Any route `backend` value not declared in `[[backend]]`
- A `[[backend]]` `sockets` glob that matches no socket
- Duplicate `path` values across `[[route]]`
- `cache` on a `[[route]]` or a `[[route_group]]` that is empty or holds a control character
- A `methods` list naming a method other than `GET`, `POST`, `PUT`, `PATCH` or `DELETE`, or an empty list
- `[errors] mode = "custom"` without `path`
- `[auth]` with no `backend`, or one that is not declared in `[[backend]]`, or that names a `url` backend
- `require` on a route but no `[auth]` declared
- A removed `[auth] public_key` key, which fails at TOML load
- `tls_cert` or `tls_key` given as a relative path
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

Section 11 closes with where the rest is documented.

## 11. Summary

The page gives the two files m6-http reads.
`site.toml` holds the state every node shares: the site identity, the routes, the backends, the log and the error settings.
The system config holds one node's state: the node name, the bind address, the certificate and the credential paths.
A key in the other file stops the start with exit 2, so a key has one owner.

Table 20 gives the pages that own the behaviour behind these keys.
Take from it that the server's internals, the renderer's own keys and the site layout each have a page of their own.

| document | covers |
|---|---|
| [`m6-architecture.md`](m6-architecture.md) §8 | Why the configuration splits into two files |
| [`m6-user-guide.md`](m6-user-guide.md) | The worked examples, including a development site and a production site |
| [`m6-http.md`](m6-http.md) | The server's pools, cache, error modes and signal handling |
| [`m6-app-anatomy.md`](m6-app-anatomy.md) | The renderer config, which is the process config of a service |
| [`m6-site-layout.md`](m6-site-layout.md) | One root per app, which a `[[route_group]]` glob has to account for |

**Table 20: further reading, and what each document covers.**

A site that runs with these two files and the keys in the tables is a site whose configuration m6-http has read and accepted.
