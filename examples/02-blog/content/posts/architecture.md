+++
title   = "m6 Architecture: An Integrated System for Building and Serving Websites"
date    = "2025-03-10"
summary = "m6 thinks about the whole stack — from authoring content in Markdown to writing bytes to a network socket — and integrates it under one roof with a single set of concepts."
tags    = ["architecture", "design"]
cover   = "https://picsum.photos/seed/architecture/1200/500"
+++

You could build what m6 does from parts: a Markdown converter, a template engine, a reverse proxy, a caching layer, an auth service. nginx, Varnish, PHP, Pandoc — the parts exist. People have been assembling them for decades. Each piece is well-understood in isolation.

The problem is the seams. Every integration point is a configuration file in a different format, a different concept of what a "route" is, a different way to invalidate a cache, a different log format, a different security model. The tools don't share a mental model, so you carry several of them simultaneously. Debugging means knowing which layer ate the request. Optimising means understanding five different caching models.

m6 is a different approach. It's an integrated system that thinks about the whole stack — from authoring pages in Markdown to writing bytes to a network socket — and handles all of it under one roof, with a single set of concepts, a single config format, and a unified model of how content moves from source to browser.

The integration is what makes the developer experience simpler. It's also what makes the system fast: because the cache layer knows exactly which content files each URL depends on, it can drop every stale page in one step when content changes, with one number to track and nothing to match. Because the rendering layer speaks directly to the cache layer's expectations, responses arrive already compressed and ready to serve. No adapter overhead, no translation, no impedance mismatch.

This is still a collection of small Unix processes. Each has one job. They communicate over Unix sockets. You wire them together with a config file. But the point isn't the processes — the point is that the whole journey, from the Markdown file on disk to the bytes on the wire, is a single coherent system.

## The processes

<pre class="mermaid">
flowchart LR
    Browser --"HTTPS"--> mhttp["m6-http\n(cache + router)"]
    subgraph "Backends (Unix sockets)"
        mhtml["m6-html"]
        mfile["m6-file"]
        mauth["m6-auth"]
        custom["Custom Renderer"]
    end
    mhttp --"cache miss only"--> mhtml
    mhttp --"cache miss only"--> mfile
    mhttp --"credential ops"--> mauth
    mhttp --"cache miss only"--> custom
</pre>

**m6-http** is the only process that listens on a public port. Its primary job — the one that determines real-world performance — is its **in-memory response cache**. Most requests in production never reach a backend at all. m6-http serves them directly from RAM: a complete HTTP response, fully rendered, already compressed to brotli or gzip, with all headers pre-formed. On a cache hit, m6-http does nothing except write bytes to the network socket.

On a cache miss, m6-http routes the request to the appropriate backend pool, gets a rendered response back, stores it in the cache if the response is cacheable, and returns it to the client. Subsequent requests for the same resource hit the cache.

m6-http also terminates TLS, enforces route-level authentication, and fetches styled error pages from m6-html when backends fail. It does not start, stop, or monitor other processes — that's systemd's job.

**m6-html** renders HTML from Tera templates and JSON data files. Each instance listens on a Unix socket. Its entire application logic is: match path → load params files → merge global params → render template → compress → respond. No database, no sessions, no side effects.

**m6-file** serves files from the filesystem. Similar socket convention to m6-html, but no templates, no params. Just path resolution with traversal protection and content-type detection.

**m6-auth** issues and verifies JWTs, manages credentials and ACLs. It listens on two sockets: the login socket for login, token refresh and logout, and a check socket that answers `POST /check`. m6-http asks the check socket who a caller is whenever a route has a `require`, so a role or group change takes effect without a new token. The check is one local socket round trip and only protected routes pay it.

**User-supplied renderers** are ordinary HTTP/1.1+ servers. Declared as backends in `site.toml`. Managed by systemd. A Rust renderer links `m6-core`, which supplies the parts of a service (routing, the request dictionary, templates, the event-loop pieces) and leaves the loop and the handlers to the renderer, but any language works.

## Tiers

<pre class="mermaid">
flowchart TD
    subgraph "Tier 1 — Static"
        T1H["m6-http"] --- T1Html["m6-html"]
        T1H --- T1File["m6-file"]
    end
    subgraph "Tier 2 — Generated Static"
        T2H["m6-http"] --- T2Html["m6-html"]
        T2H --- T2File["m6-file"]
        T2Md["m6-md (external)"] --> T2Data[("data JSON")]
        T2Data --> T2Html
    end
    subgraph "Tier 3 — Dynamic"
        T3H["m6-http"] --- T3Html["m6-html"]
        T3H --- T3File["m6-file"]
        T3H --- T3R["custom renderer"]
    end
</pre>

m6 sites fall into three tiers based on what you actually need:

**Tier 1 — Static.** m6-http + m6-html + m6-file. Templates, configs, assets, and hand-authored JSON. No build step. Deployed by copying files. This covers a lot of ground: marketing sites, documentation, portfolios.

**Tier 2 — Generated static.** Same as tier 1, but content JSON is produced by an external tool. `m6-md` converts Markdown files with TOML frontmatter into the `{ "documents": [...] }` format m6-html expects. Other tools can produce the same format. m6 itself has no opinion about where the JSON came from.

**Tier 3 — Dynamic.** User-supplied renderers handle routes that require custom logic — form submissions, APIs, CMSes, anything with mutable state. These are independent HTTP servers wired in as backends. m6 provides routing, caching, auth enforcement, and error handling around them.

## The cache

This is the source of m6's performance. Every cacheable response is stored in RAM as a complete, ready-to-send unit:

- The rendered HTML (or file bytes), already compressed to brotli or gzip
- All HTTP response headers, pre-formed
- The status code

On a cache hit, m6-http writes this blob directly to the network socket. No rendering, no compression, no template lookup, no file read, no allocation. The backend processes aren't contacted at all. For a typical static page or blog post, the effective server latency is measured in microseconds.

```
Cache hit path:
  request → cache lookup → write bytes to socket

Cache miss path:
  request → route match → backend → render → compress → cache → write to socket
```

**Cache key:** `(path, query, content-encoding)`. Each encoding variant — brotli, gzip, identity — is cached independently. If a brotli-capable client is first, the brotli variant is cached. The next gzip-only client gets a fresh render, which is then cached as a separate entry.

**What gets cached:** Responses with `Cache-Control: public`. `no-store` and `private` are not cached. A 5xx and most 4xx responses are never cached, whatever their headers say.

**Invalidation is one number.** A cached entry leaves the cache in one of four ways: a successful state-changing request evicts the entry for its own path, its freshness lifetime ends (a page with no explicit `max-age` lives at most 24 hours), the site's generation changes and empties the whole cache, or the process restarts. A change to a file behind a page therefore does not reach readers until the generation changes.

The generation is a counter the source node owns. A publish is one `POST /genid` to m6-http: it reloads every backend on a Unix socket, flushes the cache and advances the counter, then answers with the new value. Tools that change content (a CMS renderer, a deploy script) send that request when they finish. There is no per-path map and nothing to keep in step with the files.

## Backend pools and socket discovery

The socket glob in `site.toml` is the only configuration m6-http needs for a backend:

```toml
[[backend]]
name    = "m6-html"
sockets = "/run/m6/m6-html-*.sock"
```

m6-http expands the glob once, at start, so the backends start first (systemd `After=` on their units). Requests go to the pool's members in turn. A request whose connect fails tries the next member, and a backend that restarts on the same socket path serves again on its next turn. Scaling up means starting another instance on a new socket and restarting m6-http, with no change to the config.

Backends are only contacted on cache misses. A well-configured site with stable content will see backends contacted rarely — once per content version, per URL.

## Auth

Routes declare `require`:

```toml
[[route]]
path    = "/admin"
backend = "m6-html"
require = "group:admins"
```

m6-http asks m6-auth's check socket about the caller's token before the request reaches any renderer, and only for routes that declare `require`. The check verifies the token's signature, kind, issuer, audience and expiry, and answers with the caller's current roles and groups. Renderers can read the caller's identity from the request dictionary (`auth_username`, `auth_roles`, `auth_groups`) for finer checks.

## Reading files once

Every m6 process reads its configuration, `site.toml`, templates, params files and keys once, at start. A change to a configuration file takes a restart. A change to content does not: a publish is one `POST /genid`, which re-runs the start path in every backend and swaps the new site in only if it loads, so the old one keeps serving when it does not. A certificate renewal takes a restart of m6-http, which a certbot deploy hook does after each renewal.

## Logging

Every process logs structured JSON to stdout. systemd captures it via journald. No log files in the site directory. No log rotation. No separate log daemon. `journalctl -u m6-http -f` is the interface.

## Why one integrated system?

You could build something equivalent from separate tools — nginx as the proxy, Varnish as the cache, a template engine for rendering, a Markdown converter for content, a separate auth library. The parts exist and are individually excellent.

The cost is the integration surface. Each tool has its own config format, its own concept of a route, its own caching model, its own log format. Getting them to work together means writing glue, and debugging means knowing which layer handled (or dropped) each request.

m6 trades that flexibility for coherence. Because all the pieces share a common design — the same socket convention, the same config format, the same model of what a route is — the integration just works. A publish can be one request because every backend answers to the same site generation, and a cache that follows one number needs no glue. None of this requires configuration or glue.

## Why Unix processes rather than a monolith?

The separate-process architecture isn't about flexibility for its own sake — it's about operational simplicity. Each process has one job, its own resource accounting, and its own restart boundary. A broken renderer doesn't take down the cache. Scaling a hot renderer means starting another instance on a new socket; m6-http finds it when it expands the pool's glob at its next start, with no config change.

The boundary between components is HTTP/1.1 over a Unix socket — utterly conventional. You can replace any component with anything that speaks that protocol. `m6-core` supplies the parts for writing a custom renderer in Rust, but any language works.

See the individual component posts for deeper dives: [m6-http](/blog/m6-http-deep-dive), [m6-html and m6-core](/blog/m6-render-deep-dive).

---

**← [Quick Start](/blog/quick-start)** | **[m6-http Deep Dive](/blog/m6-http-deep-dive) →**
