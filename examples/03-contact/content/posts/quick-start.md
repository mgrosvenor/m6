+++
title   = "Quick Start: Your First m6 Site in Five Minutes"
date    = "2025-03-15"
summary = "Install the binaries, copy an example, and have a TLS-served site running locally before your coffee brews."
tags    = ["tutorial", "getting-started"]
cover   = "https://picsum.photos/seed/quickstart/1200/500"
+++

m6 is an integrated system for building and serving websites. It covers the whole stack — from authoring content in Markdown to writing bytes to a network socket — under one roof, with a single config format and a unified set of concepts.

Under the hood it's a family of small Unix processes, each with one job, communicating over Unix sockets. What makes it more than a collection of parts is that the pieces are designed to work together: the renderer produces output the cache can serve directly, and one request, `POST /genid`, makes every backend read new content and empties the cache. The result is a system that's simpler to reason about than a stack assembled from separate tools, and fast because none of the layers work against each other.

There's no framework to install, no build pipeline to configure, and no daemon to babysit — systemd handles process management in production, and a shell script handles it in development.

This guide gets you from zero to a running local site.

## Prerequisites

You'll need Rust (stable) and [mkcert](https://github.com/FiloSottile/mkcert) for local TLS:

```bash
cargo install m6-http m6-html m6-file
mkcert -install && mkcert localhost 127.0.0.1
```

The `mkcert` command writes `localhost+1.pem` and `localhost+1-key.pem` in the current directory. The examples do not read those two files. Each example's `dev.sh` runs `examples/dev-keys.sh`, which writes `dev.pem` and `dev-key.pem` to `dev/` in a private per-user directory (`@M6_DEV_HOME@` in the configs), and the system config refers to those.

## Clone the examples

```bash
git clone https://github.com/m6/m6-examples
cd m6-examples/examples/01-static
```

## Start the site

```bash
./dev.sh
```

Open `https://localhost:8443` in your browser. That's it.

## What just happened?

<pre class="mermaid">
flowchart LR
    Browser --"HTTPS :8443"--> mhttp["m6-http"]
    subgraph "m6 stack"
        mhtml["m6-html"]
        mfile["m6-file"]
    end
    mhttp --"Unix socket"--> mhtml
    mhttp --"Unix socket"--> mfile
</pre>

The dev script started three processes:

- **m6-html** — renders HTML from Tera templates and JSON data files. Listening on a Unix socket.
- **m6-file** — serves static assets from the filesystem. Listening on a Unix socket.
- **m6-http** — the only public-facing process. Its primary job is its **in-memory response cache**. On a cache hit, it writes pre-rendered, pre-compressed bytes directly to the network socket — m6-html and m6-file are not contacted at all. On a cache miss, it routes the request to the right backend, stores the response, and serves all future requests for that URL from RAM.

All three read from the same site directory. m6-http routes based on `site.toml`. m6-html routes based on `configs/m6-html.conf`. After the first request to any page, subsequent requests never touch m6-html or m6-file.

## The site directory

```
01-static/
├── site.toml           ← routing and backend declarations
├── configs/
│   ├── m6-html.conf    ← which templates render which paths
│   └── m6-file.conf    ← which URL prefixes map to which filesystem roots
├── templates/
│   ├── base.html
│   ├── home.html
│   └── page.html
├── assets/
│   └── style.css
└── data/
    └── site.json       ← global template variables (site name, nav, etc.)
```

No binary blobs. No generated files. Everything you see is what you deploy.

## Editing content

Open `data/site.json` and change `site_name`. Then stop the stack with `Ctrl-C` and run `./dev.sh` again. m6 reads its files once, at start, so a change shows after a restart, which also empties m6-http's cache. Reload your browser tab.

## Add a page

Add a route to `configs/m6-html.conf`:

```toml
[[route]]
path     = "/contact"
template = "templates/page.html"
params   = ["data/contact.json"]
```

Create `data/contact.json`:

```json
{ "title": "Contact", "body": "<p>hello@example.com</p>" }
```

Restart the stack, and `https://localhost:8443/contact` is live.

## Next steps

- [**Guide 00 — Prerequisites**](/blog/guide-00-prerequisites) — install binaries, set up local TLS
- [**Guide 01 — Static Site**](/blog/guide-01-static-site) — templates, assets, hand-authored JSON
- [**Guide 02 — Blog with m6-md**](/blog/guide-02-blog) — Markdown source files rendered to JSON
- [**Guide 03 — Contact Form**](/blog/guide-03-contact-form) — custom renderer, SMTP
- [**Guide 04 — Login and Protected Pages**](/blog/guide-04-auth) — JWT auth, HttpOnly cookies
- [**Guide 05 — CMS Blog**](/blog/guide-05-cms) — full CMS with draft/publish workflow
- [**Guide 06 — Production with systemd**](/blog/guide-06-systemd) — unit files, scaling, crash recovery
- [**Guide 07 — Dev to Production**](/blog/guide-07-dev-to-production) — same config, two environments

The [architecture overview](/blog/architecture) explains how the pieces fit together.

---

**Next: [m6 Architecture](/blog/architecture) →**
