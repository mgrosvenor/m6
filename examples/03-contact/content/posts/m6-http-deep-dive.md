+++
title   = "m6-http: Cache, Router, and Reverse Proxy"
date    = "2025-03-05"
summary = "m6-http's primary job is its in-memory response cache. On a cache hit it writes pre-rendered, pre-compressed bytes directly to the network socket — no backend contact, no work. The router only runs on cache misses."
tags    = ["m6-http", "performance", "internals"]
cover   = "https://picsum.photos/seed/m6http/1200/500"
+++

m6-http is the only process in an m6 deployment that listens on a public port. Its primary job is its **in-memory response cache**. Most requests in production never reach a backend. They're answered entirely from RAM.

Everything else — TLS termination, routing, auth enforcement, error page fetching — only runs when the cache can't answer the request.

## The cache

<pre class="mermaid">
flowchart TD
    Req["Request"] --> CacheHit{"Cache hit?"}
    CacheHit --"yes (common case)"--> ServeRAM["Write bytes to socket\n(no backend, no work)"]
    CacheHit --"no"--> AuthReq{"Auth required?"}
    AuthReq --"check refuses"--> Reject["401 / 403"]
    AuthReq --"check passes or no auth"--> RouteMatch{"Route match?"}
    RouteMatch --"no match"--> NotFound["404"]
    RouteMatch --"yes"--> Backend["Forward to backend"]
    Backend --> Cacheable{"Cacheable?"}
    Cacheable --"yes"--> CacheStore["Store in cache + return"]
    Cacheable --"no"--> Return["Return to client"]
</pre>

A cache entry is a complete HTTP response, ready to write to the network:

- Rendered HTML (or file bytes), already compressed to **brotli or gzip**
- All response headers, pre-formed as a byte string
- The status line

On a cache hit, m6-http does nothing except call `write()` on the socket. No template lookup, no file read, no compression, no backend contact, no allocation. For a typical page request, the server-side latency is a few microseconds.

**Cache key:** `(path, query, content-encoding)`. Each encoding variant is cached independently — if a brotli client is first, the brotli variant is stored; the next gzip client triggers a fresh render which is cached as a separate entry.

**What gets cached:** Responses with `Cache-Control: public`. `no-store` and `private` are not cached. A 5xx and most 4xx responses are never cached, whatever their headers say.

**The cache is stored behind an `Arc`**, swapped for an empty one when the generation advances. The swap is non-blocking. In-flight requests that hold a reference to the old cache complete normally — they're reading from an immutable snapshot. The new snapshot is built and swapped in; the old one is freed when the last reader drops it.

### Invalidation is one number

<pre class="mermaid">
flowchart LR
    Publisher["publisher\n(CMS, deploy script)"] --"POST /genid"--> mhttp["m6-http\n(source node)"]
    mhttp --"POST /genid"--> Backends["every backend\non a Unix socket"]
    Backends --"204: reloaded"--> mhttp
    mhttp --> Swap["swap in an empty cache,\nadvance the generation ID"]
</pre>

A cached entry leaves the cache in one of four ways: a successful state-changing request evicts the entry for its own path, its freshness lifetime ends, the site's generation changes and empties the cache, or the process restarts. A change to a file behind a page therefore does not reach the cache until one of those happens.

The generation is a counter the source node owns, written to `$STATE_DIRECTORY/genid`. A publish is one `POST /genid`, which needs a token with the claim `[genid] write_require` names, from a caller on loopback or an internal listener. m6-http runs four steps in its loop:

1. Send `POST /genid` to every member of every socket pool. Each backend reads its config, templates and params again through its start path, keeps the old site if the new one does not load, and answers 204.
2. Wait for every member, each under the backend timeout. A member that fails is logged at error with its socket path, and does not stop the sequence.
3. In one step, rebuild the route table from the config loaded at start, swap the cache and the held error pages for empty ones, advance the ID and write it.
4. Answer the POST with the new ID.

The ID changes only after every backend has the new content, so the cache cannot refill from a backend that still serves the old bytes under the new number. There is no per-path map and no sweep, and a request already holding an entry finishes on the content it took.

## The event loop

m6-http runs a single-threaded event loop on one poll (epoll on Linux, kqueue on macOS). No Tokio. No async runtime. Just the poll, file descriptors, and explicit state machines.

The constraints are self-imposed and deliberate:

- No heap allocation on the hot path
- No `unwrap()` or `expect()` — every error is handled
- No blocking calls in the event loop
- The cache `Arc` is swapped atomically; never mutated in place

A single-threaded event loop is easier to reason about, easier to profile, and has predictable latency. There's no lock contention because there are no locks on the hot path.

## HTTP/3 and QUIC

m6-http speaks HTTP/3 via [quinn](https://github.com/quinn-rs/quinn) and HTTP/1.1 and HTTP/2 via rustls. QUIC connections share the same poll as TCP connections — the QUIC UDP socket is just another fd.

QUIC matters on real network paths with packet loss. On loopback, TCP wins the Happy Eyeballs race because the crypto handshake is slower. That's expected.

## Routing (cache miss path only)

Route matching uses [matchit](https://github.com/ibraheemdev/matchit), a radix tree router. Specificity rules: exact paths beat parameterised paths, longer paths beat shorter ones.

The routing decision runs only on cache misses:

1. **Auth required?** Ask the auth check socket. 401 or 403 before any backend contact.
2. **Route match?** Forward to backend pool.
3. **No match?** 404 per configured error mode.
4. **Response cacheable?** Store it. Next request is a cache hit.

## Backend pools

Each `[[backend]]` is a pool of Unix sockets. m6-http expands the socket glob once, at start:

- A glob that matches no socket at start stops m6-http with exit 2, so the backends start first (systemd `After=`)
- A request whose connect fails tries the next member, and the client gets a 502 only when every member has failed
- A backend that stops and restarts on the same socket path serves again on its next turn
- Adding capacity means starting another instance on a new socket and restarting m6-http

Load balancing is plain round robin: each request takes the next member in turn, so every member takes the same share.

## Auth enforcement

Routes with `require` are checked before forwarding:

```toml
[[route]]
path    = "/dashboard"
backend = "m6-html"
require = "group:staff"
```

The token is taken from `Authorization: Bearer` or the `session` cookie (header takes precedence) and sent to the check backend named by `[auth] backend` in the system config, which is m6-auth's check socket. m6-auth verifies the signature, kind, issuer, audience and expiry and answers with the caller's current roles and groups, and m6-http checks them against the `require` declaration. A role change therefore takes effect without a new token. The refusal is `{"error":"unauthorised"}` (401) or `{"error":"forbidden"}` (403) for an API client.

For browser clients with an expired token but a valid refresh cookie, m6-http transparently redirects through `POST /auth/refresh` and back to the original path. API clients always receive 401 directly.

## Error handling

Three modes:

| Mode | Response |
|---|---|
| `"status"` | Status code, empty body |
| `"internal"` | Status code, m6-http-generated minimal HTML |
| `"custom"` | Status code, error page fetched from `[errors] path` |

In custom mode, m6-http makes a `GET <errors-path>?status=N&from=<original-path>` request to the error backend (typically m6-html), gets back a rendered HTML page, and returns it to the client with the original status code. If that fetch also fails, it falls back to internal mode. No recursion.

## Reading files once

m6-http reads its configuration, `site.toml`, keys and certificates once, at start. A change to any of them takes a restart, and a certificate renewal takes a restart too, which the certbot deploy hook does. Content is different: a `POST /genid` rebuilds the route table from the config loaded at start (so a publish that adds a file a `[[route_group]]` glob matches gains its route), empties the cache and reloads every backend, with no restart. In-flight requests finish on the generation they started on.

## Observability

Structured JSON to stdout. Every request logs: path, status, backend, latency in microseconds, cache hit/miss. Auth failures log at warn.

Periodic stats every 10 seconds: `rps_avg`, `rps_peak`, `latency_p50_us`, `latency_p99_us`, `cache_hit_rate`, `backend_errors`, `pool_members`.

journald captures everything. `journalctl -u m6-http -f` is all you need.

## Running it

```
m6-http <site-dir> <system-config>
```

The system config holds the state of one node: its identity (`[node]`), its bind addresses and TLS paths (`[server]`), its check backend (`[auth]`) and the claims that gate `/genid` (`[genid]`). `site.toml` holds the settings every node shares, such as routes, backends, site identity and log level, and it lives alongside the content. m6-http logs a warning at start for each node key it finds in `site.toml`, naming the key and the file it belongs in.

```bash
# Development
m6-http . configs/system-dev.toml

# Production
m6-http /srv/my-blog /etc/m6/my-blog.toml
```

Exit codes: `0` clean shutdown, `1` runtime error, `2` config or usage error (before binding).

---

**← [m6 Architecture](/blog/architecture)** | **[m6-html and m6-core](/blog/m6-render-deep-dive) →**
