+++
title   = "m6-html and m6-core: Templates, Params, and Custom Renderers"
date    = "2025-02-28"
summary = "The parts that make m6-html a small service — and how to use the same library, m6-core, to write custom backends in Rust."
tags    = ["m6-html", "m6-core", "templates", "internals"]
cover   = "https://picsum.photos/seed/m6render/1200/500"
+++

m6-html's application logic is one call and a loop:

```rust
let mut site = Site::load("m6-html", &[], &[], &TeraFactory);
// ... bind the socket, then the event loop: read a request,
// site.handle(&head), write the answer
```

It passes no code routes and no handler names, so every route it serves is a template from its config. Everything else — route matching, params loading, key merging, template rendering, compression, error handling, logging — comes from `m6-core`, the one crate every m6 service links. Understanding `m6-core` tells you everything about how m6-html works, and also how to write your own custom renderer for anything m6-html can't do on its own.

## How m6-html renders a request

<pre class="mermaid">
flowchart LR
    Req["Request"] --> PathMatch["Path\nmatching"]
    PathMatch --> ParamsLoad["Load params\nfiles"]
    ParamsLoad --> Merge["Merge global\nparams"]
    Merge --> Render["Render Tera\ntemplate"]
    Render --> Compress["Compress\n(br/gzip)"]
    Compress --> Resp["Response"]
</pre>

1. **Path matching** — the incoming path is matched against the routes declared in `m6-html.conf`. The first match wins. Parameterised paths extract named values (e.g. `{stem}` from `/blog/hello-world`).

2. **Params loading** — each matched route lists `params` files. Path param values substitute into file paths: `/blog/{stem}` with `stem=hello-world` loads `content/posts/hello-world.json`. All params files are loaded and merged into a single JSON object.

3. **Global params merge** — the `global_params` list at the top of the config (typically `data/site.json`) is merged in first. Route params overlay global params. Path params (from the URL) are added as top-level keys.

4. **Template render** — the merged JSON object is passed to Tera as the context. The template specified in the route config is rendered.

5. **Compression** — the rendered response is compressed per `Accept-Encoding` and per-MIME compression config. Brotli and gzip are supported.

6. **Response** — status, headers, body sent back over the Unix socket.

## Config

```toml
# configs/m6-html.conf

global_params = ["data/site.json"]

[[route]]
path     = "/"
template = "templates/home.html"
params   = ["data/home.json"]

[[route]]
path     = "/blog/{stem}"
template = "templates/post.html"
params   = ["data/posts.json"]

[[route]]
path     = "/_errors"
template = "templates/error.html"
params   = []
cache    = "no-store"
```

The `cache` key on a route sets the `Cache-Control` header on responses. Default is `public` for rendered routes. Use `no-store` for the error route — you don't want a cached 404 page to be served for a valid path.

## Tera templates

m6-html uses [Tera](https://keats.github.io/tera/), a template engine inspired by Jinja2. Template inheritance via `{% extends %}` and `{% block %}` works exactly as you'd expect.

One Tera behaviour worth knowing: `{% set %}` in a child template is silently discarded unless it's inside a `{% block %}`. If you need a derived variable in multiple blocks, set it independently in each block. This differs from Jinja2.

```html
{% block title %}
  {%- set post = documents | filter(attribute="stem", value=stem) | first -%}
  {{ post.title }} · {{ site_name }}
{% endblock %}

{% block content %}
  {% set post = documents | filter(attribute="stem", value=stem) | first %}
  <h1>{{ post.title }}</h1>
  {{ post.body | safe }}
{% endblock %}
```

The `safe` filter marks the value as trusted HTML — necessary for rendered Markdown bodies.

## Custom renderers with m6-core

Any route that requires logic beyond "render this template with this data" needs a custom renderer. `m6-core` supplies the parts of a service: routing, the request dictionary, templates, and the pieces of an event loop. The service writes its handlers and its own loop, so nothing is hidden in a framework.

A custom renderer declares its code routes as a constant and loads the site at start:

```rust
use m6_core::prelude::*;

const ROUTES: &[CodeRoute] = &[CodeRoute {
    pattern: "/hello/{name}",
    method: RouteMethod::Get,
}];

fn handle(req: &Request) -> Result<Response> {
    let name = req["name"].as_str().unwrap_or("world");
    Ok(Response::html(format!("<h1>Hello, {name}!</h1>")))
}

// In main: let mut site = Site::load("my-renderer", ROUTES, &[], &TeraFactory);
// then the loop, which answers each request with
//     Handled::Code(0, req) => site.finish(&req, handle(&req)?),
```

`Site::load` reads the config, `site.toml` and the templates once. `site.handle` matches a request and hands a code route back as `Handled::Code(index, req)`, and `site.finish` adds the site-wide parts to the answer: a rendered template, `Cache-Control`, compression. `docs/m6-app-anatomy.md` in the m6 repository gives the whole loop for a one-route service, and every example's `main.rs` is that loop with its own handlers.

The `Request` type gives you the path params (`req["name"]`), the query, the form fields, the headers, the body and the caller's identity. The `Response` type covers the common cases: HTML, JSON, redirect, a rendered template. State, such as a mail relay or a database, is a plain value in `main`, owned by the one thread that runs the loop.

The socket naming convention is the same as m6-html: the config filename determines the socket path. `configs/my-renderer.conf` → `/run/m6/my-renderer.sock`. Declare it as a backend in `site.toml`:

```toml
[[backend]]
name    = "my-renderer"
sockets = "/run/m6/my-renderer*.sock"
```

And route to it:

```toml
[[route]]
path    = "/api/greet/{name}"
backend = "my-renderer"
```

## m6-file

m6-file serves files from the filesystem. It shares the socket naming convention and CLI interface with m6-html, but has no templates and no params. Its one handler is registered in code under the name `files`, and configuration binds it to a path:

```toml
[[route]]
path    = "/assets/{*relpath}"
handler = "files"
root    = "assets/"
```

`GET /assets/css/main.css` → resolves to `assets/css/main.css` relative to the site directory. `{*relpath}` captures the rest of the path, slashes included. Path traversal is blocked: resolved paths must stay within `root`, `..` sequences return 404, and symlinks outside root are not followed.

## Compression

Both m6-html and m6-file compress responses per `Accept-Encoding` and per-MIME config. The defaults are sensible — text, HTML, CSS, JS compressed; images and fonts not. Override per MIME type:

```toml
[compression]
"text/html"  = { brotli = 6, gzip = 6 }
"image/jpeg" = { brotli = 0, gzip = 0 }
```

Compression level 0 means disabled for that MIME type. The compression decision is made once; subsequent requests for the same path and encoding are served from the m6-http cache.

---

**← [m6-http Deep Dive](/blog/m6-http-deep-dive)** | **[Guide: Prerequisites](/blog/guide-00-prerequisites) →**
