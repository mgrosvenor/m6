# m6, architecture

m6 serves a website with a family of Unix processes, one job each, wired together by `site.toml` over Unix sockets.
One process faces the internet and terminates *TLS* (*Transport Layer Security*), HTTP/1.1, HTTP/2 and HTTP/3.
Every other process answers HTTP/1.1 on a Unix socket behind it, and is assembled from one library.

This document is the architecture as built, with the reason for each decision beside it.
We begin with the processes and the shape of a site (§1).
We then follow a request through the edge (§2) and state which protocol specifications the edge implements and how far (§3).
§4 draws the boundary between the library and the applications that link it, §5 is the shape every application has, and §6 is configuration.
§7 summarises.

Four things live elsewhere.
Component interfaces are in `m6-core-reference.md`, configuration keys in `m6-site-toml.md`, the backend wire contract in `m6-backend-protocol.md`, and measured performance in `PERFORMANCE.md` and `BENCHMARKS.md`.
Where this document and the code disagree, the code is right and this document is a defect.

## Contents

1. [The processes](#1-the-processes)
2. [The edge](#2-the-edge)
3. [Protocol coverage](#3-protocol-coverage)
4. [The m6-core boundary](#4-the-m6-core-boundary)
5. [The service shape](#5-the-service-shape)
6. [Configuration](#6-configuration)
7. [Summary](#7-summary)

---

## 1. The processes

m6 is six serving binaries and two command line tools, and one of the six is reachable from the internet.
Each serving process takes two positional arguments (`<site-dir> <config>`), logs structured JSON to stdout, and is started and restarted by systemd.
This section names them, gives the three combinations a site uses, and shows the directory they all read.

### 1.1 What runs

Table 1 names every binary and the one job it has.

| binary | one job | reachable from the internet |
|---|---|---|
| `m6-http` | Terminate TLS, rate limit, cache, route, enforce route auth, proxy to backends. | yes |
| `m6-http` in redirect mode | Answer `:80` with a 301 to HTTPS. Its own process, with no TLS, QUIC, cache or route table built. | yes |
| `m6-html` | Render HTML from Tera templates and JSON data. | no |
| `m6-file` | Serve files from the filesystem. | no |
| `m6-auth-server` | Verify credentials and sign *JWTs* (*JSON Web Tokens*). Four routes. | no |
| `m6-monitor` | Poll every node's `/health` and `/perf` and serve one page saying how the fleet is. | no |
| `m6-md` | Convert a directory of Markdown into one JSON file. A command line tool. | no |
| `m6-auth-cli` | Manage users and groups directly against the auth database. A command line tool. | no |

**Table 1: every m6 binary and the one job it has.**
Take from it that two processes listen on public ports and both are `m6-http`, and that everything else answers HTTP/1.1 on a Unix socket or runs from a shell.

`m6-html`, `m6-file`, `m6-auth-server` and `m6-monitor` are default apps, which means they ship with m6 and are assembled from `m6-core`.
`m6-html` is six lines because everything it does is core's.
`m6-file` adds a request handler, `m6-auth-server` adds four handlers and a state builder, and `m6-monitor` adds a poller, each over the same loop.
A site adds backends of its own beside them, in any language, and `m6-http` treats all of them the same way, because the only thing it knows about a backend is the wire contract.

### 1.2 Tiers

A site uses as much of m6 as it needs, and Table 2 gives the three combinations.

| tier | processes | content comes from | build step |
|---|---|---|---|
| 1, static | `m6-http`, `m6-html`, `m6-file` | JSON and templates in the site directory | none |
| 2, generated static | tier 1 plus a tool such as `m6-md` | the tool writes the JSON | outside m6 |
| 3, dynamic | tier 1 or 2 plus backends of the site's own | handler code | outside m6 |

**Table 2: the three tiers of m6 site, by which processes run and where content comes from.**
Take from it that m6 has no build step at any tier, and that moving up a tier adds processes without changing the ones below.

### 1.3 The site directory

The layout is fixed, and Figure 1 gives it in full.

```
my-site/
├── site.toml          routing, backend pools, auth, logging
├── configs/           one config per backend process
│   ├── m6-html.conf
│   └── m6-file.conf
├── templates/
├── assets/
├── content/           JSON, written by hand or by a tier 2 tool
└── data/
```

**Figure 1: the m6 site directory.**
Take from it that a site is data and configuration only, with no binaries and no log directory, because binaries are found through `PATH` or an absolute path in a systemd unit and every process logs to stdout.

`site.toml` holds routing, pools, auth and logging for every process in the tree, and §6.1 covers what it deliberately does not hold.
The process that reads it first is the one facing the internet.

---

## 2. The edge

`m6-http` is a single-threaded event loop that terminates *TLS*, three HTTP versions and *QUIC* (*Quick UDP Internet Connections*), answers from a bounded cache, and proxies what it cannot answer to a pool of backends.
One thread runs the loop and owns every connection's state, so nothing on the request path waits on another thread.
This section follows a request through it, then covers the cache, the backend pools and reload.

### 2.1 The request path

A request crosses the edge in a fixed order, and the cheapest answers come earliest.
Table 3 gives the order and what each step costs.

| step | what happens | outcome |
|---|---|---|
| 1 | Accept a TCP connection, terminate TLS, and pick HTTP/1.1 or HTTP/2 by *ALPN* (*Application-Layer Protocol Negotiation*). HTTP/3 arrives instead as QUIC on UDP. | a parsed request |
| 2 | Check the per-IP rate limit, ahead of cache lookup and all backend work. | 429, or continue |
| 3 | Validate the method against `allowed_methods`. | 405 with `Allow` for a known method, 501 for an unrecognised one |
| 4 | Answer `/health` and `/perf` from local state. `/perf`'s metrics block needs a bearer token, and the token defaults to absent, so forgetting to configure it serves no metrics. | a JSON verdict, touching no route, cache or backend |
| 5 | Look up the cache, keyed on path, query and content coding. | a stored response, or a miss |
| 6 | Match the route, most specific first. | a backend name, or 404 |
| 7 | Verify the JWT locally and check the route's `require`. | 401, 403, or continue |
| 8 | Proxy to the least loaded pool member over HTTP/1.1 on a Unix socket. | a backend response |
| 9 | Store the response when it may be cached, add the security headers and `Alt-Svc`, record analytics. | the answer |

**Table 3: the order a request crosses `m6-http`, and what each step can answer.**
Take from it that a cache hit returns at step 5 having touched no route table and no backend, and that a health check returns at step 4 having touched even less.

Steps 3 and 4 are ordered deliberately.
Method validation runs first so that a health path refuses `PUT` like every other path.
`/health` then answers before routing, the cache and any backend, so that what a monitor measures is whether the node is up.

### 2.2 What the edge refuses to take from a client

Three rules hold on every protocol, and the first is the one that carries the most weight.

**Headers the proxy generates are stripped from every inbound request.**
`x-auth-claims` and `x-forwarded-for` are statements about a request that only the edge can make truthfully.
Backends resolve a repeated header by first match, and the proxy appends its own value after the client's, so a client that sent its own copy would win.
Dropping them at ingress is what makes `x-auth-claims` an authentication decision and `x-forwarded-for` a rate-limiting key.
Hop-by-hop headers are removed in the same pass.

**Security response headers are applied at serialisation.**
They are filled in for every response, including cache hits, backend responses, generated error pages and rate-limit refusals, and a backend that sets its own value for one of them wins.

**A malformed request ends the connection.**
After a framing error there is no way to know where the next request starts.

### 2.3 The cache

The cache is in memory, bounded, and invalidated from the site's own declarations.
Table 4 gives its shape.

| property | value |
|---|---|
| key | path, query string and content coding, in one allocation |
| stored | a response the RFC 9111 rules say may be stored |
| bound | 128MB of total response footprint by default |
| eviction | when the bound is passed, down to 87.5% of it |
| invalidation | `[[route_group]]` globs map a file to its URLs, and a backend config's `params` declarations map a data file to every route that reads it |
| swap | behind an `Arc`, replaced atomically |

**Table 4: what the response cache keys on, what bounds it, and how it is invalidated.**
Take from it that the query string is part of the key, so two query strings are two entries, and that the cache has a ceiling of 128MB.

Two rules decide what may be stored at all.
**A response that varies on anything except `Accept-Encoding` is never stored**, because the key cannot express another dimension and storing it would replay one client's variant to everyone.
**Whether a backend compresses is declared once in `site.toml` and checked on both sides.**
`[[backend]] compresses` tells the edge whether to advertise an encoding dimension, and the backend reads the same key and refuses to start when it disagrees with what it actually does.
The dangerous direction is a backend that compresses while the declaration says it does not, because the edge then stops varying on `Accept-Encoding` and a compressed body can be stored and replayed to a client that asked for identity.

Invalidation is derived from the site's own declarations.
`m6-http` reads each backend config at startup to build the map, and never for routing.
A data file changing evicts the paths that read it, and every entry for a path is evicted together.
The map is rebuilt when `site.toml` reloads, which is also when a `[[route_group]]` glob is expanded again, so a backend that writes a new content file touches `site.toml` to make it routable.

### 2.4 Backend pools

A backend is a pool of Unix sockets declared as a glob.

```toml
[[backend]]
name    = "m6-html"
sockets = "/run/m6/m6-html-*.sock"
```

**Figure 2: a backend pool, declared as a socket glob in `site.toml`.**
Take from it that a pool names a glob, which is what lets a pool change size without a config edit.

Membership comes from rescanning the glob every 2 seconds.
Starting another systemd instance therefore adds a member within that window, and stopping one removes it, with no config change and no reload.
Requests go to the member holding the fewest connections.
A member that fails is retried after 1, 2, 4, 8, 16 and then 30 seconds, and an empty pool answers per `[errors] mode`.

A site may also declare a URL backend, which is a single upstream reached over TLS with ALPN.

### 2.5 Errors and reload

One route renders every error, and `[errors] mode` decides what happens when it cannot.

| mode | answer when no error page can be fetched |
|---|---|
| `status` | the status code with an empty body |
| `internal` | the status code with minimal HTML that `m6-http` generates |
| `custom` | the error page fetched from `[errors] path`, falling back to `internal` when no path is configured |

**Table 5: the three `[errors] mode` values and what each returns.**
Take from it that a site always gets a status code, and that the richest mode degrades to the simplest one.

For a backend 4xx or 5xx, `m6-http` fetches `[errors] path` with `status` and `from` as query parameters and returns the rendered HTML under the original status code.
A request already on the error path is answered directly, which is how recursion is refused.

`site.toml` is watched, and a change reloads routing, pools, auth configuration, the invalidation map and the security headers with no restart.
The TLS certificate and key are watched as well.
That covers what the edge does, and the next section states how much of each protocol specification it implements.

---

## 3. Protocol coverage

m6 implements HTTP/1.1 and HTTP/2 itself and reaches HTTP/3 through *QUIC* (*Quick UDP Internet Connections*) provided by quiche.
Coverage is measured by three independent conformance testers.
This section gives the specifications, the scores, and the gaps.

### 3.1 What implements which specification

Table 6 maps each specification to the code that implements it.

| specification | what it covers | where |
|---|---|---|
| RFC 9110, semantics | methods, status, field validation, conditional requests, content negotiation, HEAD, hop-by-hop | `m6-core`: `conditional`, `negotiate`, `headers`, `h1`, `http`, `mime`. `m6-http`: `cache`, `forward`, `http11`, `http2`, `error` |
| RFC 9111, caching | storability, freshness, age, directives, revalidation | `m6-http/cache.rs` |
| RFC 9112, HTTP/1.1 | parse, serialise, chunked coding, framing | `m6-core/h1.rs`, the one parser, used by every backend and by the edge |
| RFC 9113, HTTP/2 | frame layer, stream state machine, flow control, error taxonomy, SETTINGS, CONTINUATION, GOAWAY, field validation | `m6-http/http2.rs`, `m6-http/fields.rs` |
| RFC 7541, *HPACK* (*HTTP/2 header compression*) | prefixed integers, string literals, dynamic table size updates, indexed fields | `m6-http/http2.rs`, with the `hpack` crate for the table |
| RFC 9114, HTTP/3 | request and response mapping, field validation | `m6-http/fields.rs` shared with HTTP/2, and quiche |
| RFC 9204, *QPACK* (*HTTP/3 header compression*) | encoder and decoder instruction streams | quiche, static table only |
| RFC 9000, 9001, 9002, QUIC | transport, TLS, loss recovery | quiche |
| RFC 6265, cookies | `Set-Cookie` construction, and the rule against folding it | `m6-core/cookie.rs`, `m6-core/headers.rs` |
| RFC 7838, Alt-Svc | HTTP/3 discovery | `m6-http/main.rs`, advertised on every response |

**Table 6: each protocol specification and the module that implements it.**
Take from it that field validation is shared between HTTP/2 and HTTP/3 because RFC 9114 4.3 restates RFC 9113 8.3, and that everything below HTTP/3's field layer belongs to quiche.

Most of the edge is protocol code: `m6-http` is 33,124 lines, and the frame layers, the stream state machine and the parsers are the bulk of it.
Across the serving crates, RFC 9110 is cited 94 times, RFC 9113 78 times, RFC 9112 37 times and RFC 9111 27 times.

### 3.2 Measured conformance

Table 7 gives the recorded floors, which a run must meet.

| suite | target | score |
|---|---|---|
| h1spec | `m6-auth-server`, `m6-file`, `m6-html`, `m6-http` in redirect mode | 32/32 each |
| h2spec | `m6-http` | 146/146 |
| h3spec | `m6-http` | 47/49 |

**Table 7: the recorded conformance floors, from `tools/conformance-scores.txt`.**
Take from it that HTTP/1.1 is measured on four separate binaries because four of them speak it, and that HTTP/3 is the only suite not at full marks.

A run below a floor fails the gate.
A run above one asks for the floor to be raised in the commit that earned it.

### 3.3 The gaps

Three gaps remain, and Table 8 states each with its standing.

| gap | standing |
|---|---|
| QPACK 4.1.3 and 4.4.3, the last two h3spec tests | Accepted. quiche reads the peer's QPACK instruction streams and discards them, running a static table only. Closing it means new validation on the connection path, with per-stream buffering for instructions split across reads, which is where a careless version becomes unbounded memory on a stream a peer controls |
| The QUIC stack is a fork of quiche, pinned by revision | Accepted, with an exit condition. Released quiche scores 37/49, and two open upstream pull requests take it to 47/49. The fork is quiche master plus those two. It is pinned by revision so the dependency cannot move under a build that claims to be reproducible, and it is dropped when upstream releases the fixes |
| RFC 9218, extensible priorities | Not implemented. HTTP/2 priority handling is limited to refusing a stream that depends on itself |

**Table 8: the three protocol gaps and where each one stands.**
Take from it that two gaps are decided and recorded with their reasoning, and that RFC 9218 is the one gap with no decision behind it.

Protocol work is the largest body of code in m6, and it is also the part that depends least on the rest.
The library every other process is built from is the subject of the next section.

---

## 4. The m6-core boundary

`m6-core` is the box of blocks a service is assembled from, and the only crate a service links.
It is 32 modules and 19,937 lines, linked by the edge, by the default apps, and optionally by a site's own Rust backends.
This section states the rule that decides what goes in, what is in, what stays out, and what linking it costs.

### 4.1 What core is for, and the rule for what goes in

**`m6-core` is the PHP of m6: a generic library of components for building web applications.**
Breadth is the intent.
A component a website might want belongs in core, which is why it holds templating, compression, minification, cookies, multipart bodies, an SMTP client, an outbound HTTP client, host metrics and firewall reporting beside the HTTP layers.
A service assembles what it needs from one dependency, and the feature gates in §4.5 decide what it pays for.

**What decides whether a given piece of code belongs there is consumer count: more than one consumer moves it in, and single-consumer code stays with its consumer.**
Breadth and singularity are separate questions.
Core is wide on purpose, and each thing in it exists exactly once.

A library with one consumer is that consumer's code in another directory.
The rule is what keeps HTTP/2, HTTP/3 and the RFC 9111 caching rules in `m6-http`: the edge is the only process that terminates a public connection and the only process that is a cache, and by the architecture nothing else ever will be.
It is also what puts the shutdown sequence, logging, HTTP/1.1, path validation, content coding negotiation and conditional requests in core, which have five, four, four, two, two and two consumers.

### 4.2 Where core sits

Figure 3 shows core beside both sides of the wire contract.

```
                        ┌──────────────────────────────┐
   public traffic  ───► │  m6-http                     │
   TLS, h1, h2, h3      │  listener, rate limit, cache │
                        │  routing, auth, proxy policy │
                        └───────────────┬──────────────┘
                                        │  HTTP/1.1 over a Unix socket
                                        │  (the wire contract)
                        ┌───────────────▼──────────────┐
                        │  application                 │
                        │  m6-html, m6-file, m6-auth,  │
                        │  m6-monitor, or any language │
                        └──────────────────────────────┘

   m6-core is linked by m6-http and by the applications. It is a library,
   never a process, and it never appears in the request path by itself.
```

**Figure 3: where `m6-core` sits relative to the wire contract.**
Take from it that core is linked by the processes on both sides of the contract and is not a hop between them.

The wire contract outranks the library.
`m6-backend-protocol.md` is small enough to implement from scratch in any language in well under a hundred lines, and six reference backends in C, C++, Go, Python and Rust are built and tested from it.
Core is a convenience for Rust, so behaviour a backend depends on must be readable in the specification, and behaviour that exists only as Rust is a hole in the specification.

### 4.3 What is in core

Table 9 groups them by what they answer.

| group | modules | what they own |
|---|---|---|
| Semantics | `conditional`, `negotiate`, `headers`, `http`, `mime` | RFC 9110 rules: preconditions, coding negotiation, repeated fields, content types with charset |
| HTTP/1.1 | `h1`, `parse` | the one parser and response writer, and the blocking read adapter over it |
| Service | `app`, `server`, `signal` | the socket server, the accept loop, routing, the thread pool, the shutdown sequence |
| Configuration | `config`, `watcher` | TOML parsing, secrets merging, and a pollable file-change descriptor |
| Content | `compress`, `minify`, `template`, `render` | gzip and brotli, HTML, CSS, JSON and JavaScript minification, and the renderer seam |
| Request and response | `request`, `response`, `dict`, `cookie`, `multipart` | what a handler receives and returns, the layered request dictionary, `Set-Cookie` construction |
| Safety | `path`, `random`, `parse` | path parameter validation and traversal refusal, cryptographic token generation, and the request caps: 8KB of headers and a 16MB body, answered with 413 |
| Observability | `log`, `telemetry`, `monitoring`, `host`, `firewall`, `ndjson` | logging setup, the analytics record format, `/health` and `/perf`, host load, memory, disk and temperature, nftables block counters |
| Test kit | `testkit` | standing a service up, claiming a socket without a race, driving it, tearing it down |

**Table 9: the nine groups of module in `m6-core`.**
Take from it that core owns the questions an external specification answers and the questions every service asks, and that observability is one of them because every deployment of m6 publishes the same endpoints.

**Safety is grouped alone because a security boundary with two implementations has two behaviours.**
Path validation allows alphanumerics, `-`, `_`, `.`, and `/` when the route's parameter spans segments, and it refuses `..` anywhere as a substring, a leading or trailing slash, and every other byte including space and NUL.
A rejected traversal answers 404 and a merely malformed value answers 400, because naming a traversal as a traversal tells the sender their payload reached the router.

**The test kit is in core because core owns conformance.**
The crate that proves a rule is the crate that owns the harness proving it.

### 4.4 What stays outside

Table 10 gives what looks like core and is a deployment decision.

| stays in `m6-http` | why |
|---|---|
| the TLS listener and certificates | a deployment decision, and an application never terminates TLS |
| the event loop | the edge's concurrency model. Applications use the thread pool (§5.2) |
| cache storage and eviction | RFC 9111 defines freshness, and not how many megabytes to keep |
| the route table and matching policy | which paths exist is site configuration |
| rate limiting | a policy choice about whom to refuse |
| proxy and pool logic | specific to being the front door |
| HTTP/2 and HTTP/3 | one consumer, permanently (§4.1) |

**Table 10: what stays in `m6-http`, and the reason for each.**
Take from it that the split is what a specification requires against what one deployment does.

### 4.5 What linking core costs

**Core must stay linkable by a command line tool.**
`m6-md` is a Markdown converter and must not acquire a QUIC stack or a TLS library by depending on core.
With HTTP/2 and HTTP/3 outside, that holds by construction, because nothing in core's scope needs `quiche`, `rustls` or `ring`.

Table 11 gives the feature gates.

| feature | default | adds |
|---|---|---|
| `templates` | on | Tera and comrak, behind the renderer seam |
| `testkit` | off | the integration harness and raw socket clients |
| `multipart` | off | `multipart/form-data` body parsing |
| `flash` | off | one-shot messages signed with an *HMAC* (*hash-based message authentication code*) |
| `csrf` | off | *CSRF* (*cross-site request forgery*) double-submit token generation and checking |
| `email` | off | an SMTP client |
| `http-client` | off | an outbound HTTP client |

**Table 11: `m6-core`'s seven feature gates.**
Take from it that templating is the only one on by default, so a service that renders nothing sets `default-features = false` to avoid linking a template engine.

---

## 5. The service shape

Every m6 service except the edge is an `App`: a Unix socket server with a fixed thread pool, a bounded queue, and routes that come from config.
The shape is the same for a six-line default app and for a site's own backend.
This section gives the whole of a minimal app, the four ways to hold state, the concurrency model, the request dictionary and the lifecycle.

### 5.1 The whole app

Figure 4 is `m6-html`, complete.

```rust
use m6_core::prelude::*;

fn main() -> anyhow::Result<()> {
    App::new().run()?;
    Ok(())
}
```

**Figure 4: `m6-html` in full, which renders every HTML page a site serves.**
Take from it that a default app supplies no code of its own, because routing, templating, the request dictionary, compression and the lifecycle all come from core.

A service that needs code registers a handler by name, and config binds a route to it.

```toml
[[route]]
path    = "/assets/{*relpath}"
handler = "files"
root    = "assets/"
```

**Figure 5: a config route bound to a handler registered in code.**
Take from it that the handler is code and the route is configuration, so an asset tree is added by editing config and reloading.

A route naming a handler that no code registered is refused: startup exits 2, and a reload keeps the previous routes serving.

### 5.2 State and concurrency

Table 12 gives the four entry points.

| entry point | for |
|---|---|
| `App::new()` | no state |
| `App::with_global(init)` | one value shared by every worker, behind an `Arc` |
| `App::with_thread_state(init)` | one value per worker thread, with no synchronisation |
| `App::with_state(g, t)` | both |

**Table 12: the four `App` entry points, by what state a service holds.**
Take from it that a service declares its state shape once and the builder's type carries it, so a handler receives its state with the real type.

The concurrency model is a fixed thread pool over a bounded queue.
The pool defaults to the CPU count and the queue to eight times the pool.
A full queue answers 503 immediately, which is how backpressure reaches the edge.
Scaling is by starting another systemd instance, which the edge picks up within 2 seconds (§2.4).

Routing is by specificity, decided when a route is compiled.
A literal segment beats a parameter, a parameter beats a wildcard, and `{*name}` captures the rest of the path and is legal only in the last position.

### 5.3 The request dictionary

A request becomes a map that a template or handler reads, assembled in a fixed order from a shared base and a per-request overlay.
Table 13 gives the order.

| layer | source |
|---|---|
| base, built once per route per reload | config keys, then global params files, then the route's static params files |
| overlay, per request | params files whose path holds a placeholder, path params, query, form fields, cookies, **the built-ins**, auth claims, flash, CSRF token |

**Table 13: the two layers of the request dictionary and what goes in each.**
Take from it that the base is shared behind an `Arc` and never copied per request, and that the overlay always wins over the base.

The built-ins go in after every params file.
`request_path`, `datetime` and `year` describe the request, and a content file that could redefine them could make a page lie about which URL it is.
Only `application/x-www-form-urlencoded` bodies are decoded, and a POST carrying any other body type is logged loudly with its content type.

### 5.4 Lifecycle

**`m6_core::signal::block()` is the first statement of `main`.**
A thread inherits the signal mask as it stands when it is created, and a service's logging writer is a thread.
A writer thread created before the mask is set takes SIGTERM at its default disposition, which kills the process.
`ShutdownHandle::install` asserts the mask is already set and refuses to start otherwise.

There is one shutdown sequence for every service, and what a service supplies is data: the name used in every lifecycle line, the socket to self-connect and unlink on every exit path, and a wake descriptor for a loop parked in epoll or kqueue.
Core owns the lifecycle log lines, so `journalctl -u <unit> | grep shutdown` means the same thing for every unit.

SIGTERM and SIGINT are identical.
The first drains in-flight work and exits 0, and the second exits immediately.

| code | meaning |
|---|---|
| 0 | clean shutdown |
| 1 | runtime error |
| 2 | configuration or usage error, before binding |

**Table 14: the three exit codes every m6 binary uses.**
Take from it that a supervisor can tell a bad config from a crash, because exit 2 happens before anything binds.

---

## 6. Configuration

A site is configured by `site.toml`, one config file per backend process, and one system config holding what differs between environments.
Secrets live outside the site directory and each key has exactly one owner.
This section covers the layering, the secrets rule and the reload semantics.

### 6.1 Layering and secrets

`site.toml` travels with the site under version control and contains no password and no certificate path.
The system config is a required second positional argument and holds `[server]` alone, which is the bind address and the TLS paths.
`[server]` is the only section that differs between environments, so restricting the file keeps its purpose obvious.
The system config wins on conflict, so a deploy that overwrites the site tree cannot change the bind address or the certificate path.

A backend's secrets come from `secrets_file`, a path outside the site directory.

**A key set in both a config and its secrets file is refused at startup.**
A file that is overridden is indistinguishable from a file that is correct when you are reading one file.
The way to make a missing required value fail loudly is for the value to be absent, which names the key.

### 6.2 Reload

Every service watches its own config file and `site.toml` through one pollable descriptor folded into the loop it already runs, using inotify on Linux and kqueue on the BSDs.
A reload rebuilds routes, templates and the request dictionary base, and swaps them atomically.
A reload that fails to parse, fails to compile a template, or names an unregistered handler is refused with the previous state left serving.

Connection settings are read once at startup, because they are applied to a socket at accept time.
The read timeout defaults to 30 seconds and the socket mode to `0660`.

`--dump-config` loads the configuration exactly as the service would, prints how every route would be served, and exits 0 when the binary can serve the config and 2 when it cannot.
That is what validates a new binary against a config before it is installed.

---

## 7. Summary

m6 is six serving binaries with one job each, wired by `site.toml` over Unix sockets, with `m6-http` the only one on a public port in either of its two modes, and every other process answering HTTP/1.1 behind it.
The edge terminates three HTTP versions, answers from a 128MB cache keyed on path, query and coding, and refuses to accept from a client any header it generates itself.
Conformance is measured: 32/32 on HTTP/1.1 across four binaries, 146/146 on HTTP/2, and 47/49 on HTTP/3.

`m6-core` holds what an external specification answers and what every service needs, and the rule that decides is consumer count.
More than one consumer moves code into core, and one consumer keeps it with its consumer, which is why HTTP/2, HTTP/3 and the caching rules stay at the edge, permanently.
Every service except the edge is the same `App`, and a default app supplies no code of its own.

The decisions here are in force with their reasons attached, so changing one means answering its reason.
