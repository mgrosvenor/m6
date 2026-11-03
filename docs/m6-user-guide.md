# m6 user guide

Eleven examples build one website, from a static page to a global fleet.
Each example lives in the `m6-examples` repository and adds one capability to the last.
This guide explains what each example adds and which file does the work.
Sections 2 to 12 walk Examples 01 to 11 in order.

**Contents**

1. [Before you start](#1-before-you-start)
2. [Example 01, static site](#2-example-01-static-site)
3. [Example 02, blog](#3-example-02-blog)
4. [Example 03, contact form](#4-example-03-contact-form)
5. [Example 04, login and protected pages](#5-example-04-login-and-protected-pages)
6. [Example 05, CMS blog](#6-example-05-cms-blog)
7. [Example 06, production with systemd](#7-example-06-production-with-systemd)
8. [Example 07, dev to production](#8-example-07-dev-to-production)
9. [Example 08, log viewer](#9-example-08-log-viewer)
10. [Example 09, global deployment](#10-example-09-global-deployment)
11. [Example 10, API tokens](#11-example-10-api-tokens)
12. [Example 11, admin dashboard API](#12-example-11-admin-dashboard-api)

## 1. Before you start

The examples need two checkouts, one certificate tool and a release build of m6.
This section installs them and points at the full-stack check, so a later failure is a real answer about an example.
The release build puts every m6 binary on the path, including `m6-md`, which Example 02 uses.

Rust 1.88 or newer provides the compiler and Cargo.
`mkcert` issues a development certificate that a browser trusts.
`openssl` is the fallback when `mkcert` is absent.
The two checkouts must sit side by side, because the renderer crates reach m6 through the relative path `../../../../m6/m6-core`.

Build m6 and the examples with these commands:

```sh
git clone https://github.com/mgrosvenor/m6
cd m6
cargo build --release --workspace
export PATH="$PWD/target/release:$PATH"

cd ..
git clone https://github.com/mgrosvenor/m6-examples
cd m6-examples
cargo build --release
```

m6-http serves the public site over *TLS* (Transport Layer Security), so every site needs a certificate.
Install `mkcert` once, and the local `dev.sh` scripts write a development certificate on first run into the path their system config names.
When `mkcert` is absent, `dev.sh` uses `openssl` and the browser warns about the self-signed certificate.

Check the install with Example 05, which runs the whole stack.
[`README.md`](../README.md) step 4 gives the commands and explains what the check covers.
A green run there means every binary and every example dependency works.
A failure there is about the install, before the guide asks you to write anything.

The next section starts from the smallest site: three processes, four config files, templates and data.

## 2. Example 01, static site

Example 01 is the base case: a static site with *HTML* (HyperText Markup Language) templates, one stylesheet and hand-authored *JSON* (JavaScript Object Notation) data, served over *TLS* (Transport Layer Security).
It runs three processes.
The example runs directly from the templates, data and configs in its directory.
The released files are in `examples/01-static/`.

m6-http terminates TLS, routes each request and caches the response.
m6-html renders templates from the JSON data.
m6-file serves files from `assets/`.
m6-http is the only process with a network listener, and it reaches the other two over Unix sockets in `/tmp/m6/`.

### 2.1. Site directory

Eight entries make the example:
- `site.toml`
- `configs/m6-html.conf`
- `configs/m6-file.conf`
- `configs/system-dev.toml`
- `templates/`
- `assets/style.css`
- `data/`
- `dev.sh`

Sections 2.2 to 2.7 explain the config files, the data, the templates, the stylesheet and the runner.

`site.toml` holds what every node shares.
`configs/system-dev.toml` holds what belongs to this node, and m6-http takes both files with the site directory first.
That split is the subject of section 8.

### 2.2. `site.toml`

`site.toml` is the *TOML* (Tom's Obvious, Minimal Language) file that declares the site name, the error mode, the log format, two backends and the routes.
The `sockets` globs point at `/tmp/m6/`, where the development stack writes its sockets.

```toml
[site]
name   = "My Site"
domain = "localhost"

[errors]
mode             = "internal"
verbose_fallback = true

[log]
level  = "info"
format = "text"

[[backend]]
name    = "m6-html"
sockets = "/tmp/m6/m6-html*.sock"

[[backend]]
name    = "m6-file"
sockets = "/tmp/m6/m6-file*.sock"

[[route]]
path    = "/"
backend = "m6-html"

[[route]]
path    = "/about"
backend = "m6-html"

[[route]]
path    = "/_errors"
backend = "m6-html"

[[route_group]]
glob    = "assets/**/*"
path    = "/assets/{relpath}"
backend = "m6-file"
```

The `[errors]` block sets `mode = "internal"`, so m6-http generates a small error page itself.
The released example sets `verbose_fallback = true` for development, which adds the request path, the matched route, the backend name and the error detail to that page.
Set `verbose_fallback = false` in a production `site.toml`, because every internal error page would carry those details to any client.
Internal mode uses m6-http's own page, and custom mode uses the `/_errors` route.

A `[[route]]` matches one path pattern, exact or with `{param}` captures.
A `[[route_group]]` expands `assets/**/*` into one route per asset.
`{relpath}` in the route group is the path below `assets/`.

### 2.3. `configs/m6-html.conf`

`configs/m6-html.conf` loads `data/site.json` into every template and gives each route a template and a params list.
The `/` route renders `templates/home.html` with the global params.
The `/about` route adds `data/about.json`.
The `/_errors` route renders `templates/error.html` and sets `cache = "no-store"`, so m6-http never caches the error page.

```toml
global_params = ["data/site.json"]

[[route]]
path     = "/"
template = "templates/home.html"
params   = []

[[route]]
path     = "/about"
template = "templates/page.html"
params   = ["data/about.json"]

[[route]]
path     = "/_errors"
template = "templates/error.html"
params   = []
cache    = "no-store"
```

### 2.4. `configs/m6-file.conf`

`configs/m6-file.conf` has one route, and m6-file serves every asset through its `files` handler.
The route uses `{*relpath}` for a nested asset path such as `css/style.css`.

```toml
[[route]]
path = "/assets/{*relpath}"
handler = "files"
root = "assets/"
```

`handler = "files"` is required.
A route that names an unknown handler stops m6-file at startup with exit code 2.

### 2.5. `data/site.json`

`data/site.json` supplies the three global values that the layout uses: the site name, the tagline and the navigation.
`base.html` reads `site_name` and `nav`, and `home.html` reads `site_tagline`.

```json
{
  "site_name": "My Site",
  "site_tagline": "A minimal static site built with m6.",
  "nav": [
    { "label": "Home",  "path": "/" },
    { "label": "About", "path": "/about" }
  ]
}
```

### 2.6. Templates

`templates/base.html` is the layout for every page.
It sets the title block, loads `/assets/style.css`, renders `nav` from `data/site.json`, and leaves a `content` block for the page.
`templates/home.html` fills that block with the tagline and a link to `/about`.
`templates/page.html` renders `title` and the HTML `body` from `data/about.json`.

m6-html builds one params map per request.
The map holds the global params, the route's params files, the path captures and the query parameters.
Each query parameter is available under `query` and at the top level.

### 2.7. `dev.sh`

`dev.sh` starts the three processes and stops them together with a trap.
It sets `M6_SOCKET_OVERRIDE` so each backend writes its socket to `/tmp/m6/`, which matches the `sockets` globs in `site.toml`.
It also writes the development certificate on first run, waits for both backend sockets, then starts m6-http with `site.toml` and `configs/system-dev.toml`.

Run it from `examples/01-static` and open `https://localhost:8443`:

```sh
cd examples/01-static
./dev.sh
```

The script looks for the m6 checkout beside this repository, at `../../../m6`.
Set `M6` to a different path when the two checkouts are not siblings.

[`m6-site-toml.md`](m6-site-toml.md) owns every config key.
[`m6-file.md`](m6-file.md) owns the handler and wildcard rules.
[`m6-render.md`](m6-render.md) owns template rendering.

Example 01 shows the smallest useful site: three processes and four config files.
Example 02 keeps that stack and adds a Markdown build step, one generated params file and two routes.

## 3. Example 02, blog

Example 02 adds a blog to the static site.
`m6-md` converts Markdown files into one *JSON* (JavaScript Object Notation) file, and m6-html renders two pages from it with *HTML* (HyperText Markup Language) templates.
The example keeps the same three processes and the same site layout.
It adds a source directory, a generated data file, two routes and two templates.

### 3.1. What is new

The blog keeps its posts in Markdown with *TOML* (Tom's Obvious, Minimal Language) frontmatter under `content/posts/`.
`m6-md` reads that directory and writes `data/posts.json`, a single JSON object with a `documents` array.
m6-html renders the blog pages from that JSON.
The Markdown conversion happens before the server starts.

A new or changed post needs a rebuild and a cache invalidation.
`dev.sh` builds the m6 binaries, then runs `m6-md` in watch mode and touches `site.toml` after each rebuild, which evicts the affected URLs.

### 3.2. Source file format

Each post starts with a TOML frontmatter block between `+++` lines, followed by the Markdown body.

```
+++
title   = "Hello World"
date    = "2024-01-15"
summary = "A brief introduction to the blog."
tags    = ["rust", "web"]
+++

Post body in **Markdown**. Tables, footnotes, and strikethrough supported.
```

`title` and `date` are the two keys that m6-md reads for itself.
Other keys, such as `summary` and `tags`, pass through to the JSON unchanged.
A key named `stem`, `path` or `body` is ignored with a warning, because m6-md sets those three fields.
When `title` is absent, m6-md uses the file stem with hyphens replaced by spaces.
When `date` is absent, m6-md uses the file's modification time as a `YYYY-MM-DD` date.
Files whose names start with `_` are skipped, and the directory is read one level deep.

### 3.3. `data/posts.json`

`data/posts.json` holds one entry per post, and m6-md sorts the entries by `date` descending.
The entry below is one post, with the fields that the example discusses.

```json
{
  "documents": [
    {
      "stem":    "hello-world",
      "path":    "/hello-world",
      "title":   "Hello World",
      "date":    "2024-01-15",
      "body":    "<p>Post body in <strong>Markdown</strong>. Tables, footnotes, and strikethrough supported.</p>\n",
      "summary": "A brief introduction to the blog.",
      "tags":    ["rust", "web"]
    }
  ]
}
```

`stem` is the filename without `.md`, and `body` is the rendered HTML.
`path` is `/{stem}`.
The public address comes from the route, `/blog/{stem}`, and the templates use `stem`.

### 3.4. New routes in `site.toml`

The blog adds two routes to `site.toml`, one exact and one parameterised.
`/blog` renders the index, and `/blog/{stem}` renders one post.
Both routes point at m6-html, and the asset route group is the only route group in the file.

```toml
[[route]]
path    = "/blog"
backend = "m6-html"

[[route]]
path    = "/blog/{stem}"
backend = "m6-html"
```

m6-http matches `/blog/{stem}` and forwards the `stem` capture to m6-html.

### 3.5. New routes in `configs/m6-html.conf`

m6-html gains two routes, and all three routes load `data/posts.json`, including the existing `/` route.
The index template loops over `documents`.
The post template filters the same list by `stem`.
The `/` route uses `documents` for the three most recent posts on the home page.

```toml
[[route]]
path     = "/blog"
template = "templates/post-index.html"
params   = ["data/posts.json"]

[[route]]
path     = "/blog/{stem}"
template = "templates/post.html"
params   = ["data/posts.json"]
```

### 3.6. `templates/post-index.html`

`templates/post-index.html` renders the list, newest first, and links each title to `/blog/{{ doc.stem }}`.
It repeats the date descending sort that m6-md already applied, so the page keeps that order when a JSON file is edited by hand.

```html
{% for doc in documents | sort(attribute="date") | reverse %}
```

### 3.7. `templates/post.html`

`templates/post.html` filters `documents` by `stem`, renders the first match, and calls `not_found()` when no post matches.
`stem` is the `{stem}` capture from the request path, which m6-html adds to the params map.

```html
{% set post_list = documents | filter(attribute="stem", value=stem) %}
{% if post_list | length == 0 %}{{ not_found() }}{% endif %}
{% set post = post_list | first %}
```

[`m6-md.md`](m6-md.md) owns the frontmatter rules, the watch flags, the output format and `not_found()`.
[`m6-render-lib.md`](m6-render-lib.md) owns the template engine and its built-in keys and m6 filters.
The `sort`, `reverse`, `filter`, `length` and `first` filters are Tera's standard filters.

Example 02 adds a content pipeline in front of the same three processes.
Example 03 adds a fourth process, a custom renderer that handles a contact form and sends email.

## 4. Example 03, contact form

`examples/03-contact/`

Adds `render-contact` — a custom renderer that handles GET (renders the form) and POST (sends email via SMTP).

```bash
cd examples/03-contact
cargo build --release -p render-contact
./dev.sh
```

### 4.1. `render-contact/Cargo.toml`

```toml
[package]
name    = "render-contact"
version = "0.1.0"
edition = "2021"

[dependencies]
m6-render = { git = "https://github.com/m6/m6", tag = "v0.1.0", features = ["smtp"] }
serde_json = "1"
```

### 4.2. `render-contact/src/main.rs`

```rust
use m6_render::prelude::*;

// Shared — SmtpTransport is Send+Sync, no locking needed
struct Global {
    mailer: SmtpTransport,
}

fn init_global(config: &Map<String, Value>) -> Result<Global> {
    Ok(Global {
        mailer: SmtpTransport::builder(config["smtp"]["host"].as_str()?)
            .port(config["smtp"]["port"].as_u64()? as u16)
            .credentials(
                config["smtp"]["username"].as_str()?,
                config["smtp"]["password"].as_str()?,
            )
            .build()?,
    })
}

fn handle_post(req: &Request, global: &Global, _local: &mut ()) -> Result<Response> {
    let name    = req.field("name")?;
    let email   = req.field("email")?;
    let message = req.field("message")?;

    global.mailer.send(
        Message::builder()
            .from(req["smtp"]["from"].as_str()?.parse()?)
            .to(req["smtp"]["to"].as_str()?.parse()?)
            .subject(format!("Contact from {}", name))
            .body(format!("From: {} <{}>\n\n{}", name, email, message))?,
    )?;

    Response::render_with("templates/contact.html", req, json!({"sent": true, "name": name}))
}

fn main() -> Result<()> {
    App::with_global(init_global)
        .route_post("/contact", handle_post)
        // GET /contact served by framework default (template render from config)
        .run()
}
```

### 4.3. `configs/render-contact.conf`

```toml
global_params = ["data/site.json"]
secrets_file  = "/etc/m6/render-contact.toml"

[[route]]
path     = "/contact"
template = "templates/contact.html"
params   = []
cache    = "no-store"

# Development defaults — overridden by secrets_file in production
[smtp]
host     = "localhost"
port     = 1025
username = ""
password = ""
from     = "noreply@example.com"
to       = "owner@example.com"
```

```toml
# /etc/m6/render-contact.toml  (production secrets, not in version control)
[smtp]
host     = "smtp.example.com"
port     = 587
username = "user@example.com"
password = "live-smtp-password"
```

### 4.4. `templates/contact.html`

```html
{% extends "base.html" %}
{% block title %}Contact · {{ site_name }}{% endblock %}
{% block content %}
  <h1>Contact</h1>
  {% if sent %}
    <p>Message sent. Thanks, {{ name }}!</p>
  {% else %}
    <form method="post" action="/contact">
      <label>Name <input type="text" name="name" required></label>
      <label>Email <input type="email" name="email" required></label>
      <label>Message <textarea name="message" required></textarea></label>
      <button type="submit">Send</button>
    </form>
  {% endif %}
{% endblock %}
```

### 4.5. New in `site.toml`

```toml
[[backend]]
name    = "render-contact"
sockets = "/run/m6/render-contact-*.sock"

[[route]]
path    = "/contact"
backend = "render-contact"
```

### 4.6. Updated `dev.sh`

```bash
./target/release/render-contact "$SITE" "$SITE/configs/render-contact.conf" &
```

---

## 5. Example 04, login and protected pages

`examples/04-auth/`

Adds m6-auth. Members-only pages behind login. The JWT is stored in an HttpOnly cookie — the browser sends it automatically on every request, JavaScript cannot read it.

```bash
cd examples/04-auth
./setup.sh   # generates keys, creates first admin user
./dev.sh
# login at https://localhost:8443/login
```

### 5.1. Key generation and bootstrap (`setup.sh`)

```bash
#!/bin/bash
set -e
SITE="$(cd "$(dirname "$0")" && pwd)"

# Generate signing keys
mkdir -p "$SITE/keys"
openssl ecparam -name prime256v1 -genkey -noout -out "$SITE/keys/auth.pem"
openssl ec -in "$SITE/keys/auth.pem" -pubout -out "$SITE/keys/auth.pub"
chmod 600 "$SITE/keys/auth.pem"
echo "Keys generated."

# Create first admin user — database created automatically if absent
m6-auth-cli "$SITE/configs/m6-auth.conf" user add admin --role admin

echo "Setup complete. Start the server with ./dev.sh"
```

### 5.2. `configs/m6-auth.conf`

```toml
[storage]
path = "data/auth.db"

[tokens]
access_ttl  = 900
refresh_ttl = 2592000
issuer      = "localhost"

[keys]
private_key = "keys/auth.pem"
public_key  = "keys/auth.pub"
```

### 5.3. New in `site.toml`

```toml
[auth]
backend    = "m6-auth"
public_key = "keys/auth.pub"

[[backend]]
name    = "m6-auth"
sockets = "/run/m6/m6-auth-*.sock"

[[route]]
path    = "/login"
backend = "m6-html"

[[route]]
path    = "/auth/login"
backend = "m6-auth"

[[route]]
path    = "/auth/logout"
backend = "m6-auth"

[[route]]
path    = "/auth/refresh"
backend = "m6-auth"

[[route]]
path    = "/members"
backend = "m6-html"
require = "group:members"

[[route]]
path    = "/members/{page}"
backend = "m6-html"
require = "group:members"
```

### 5.4. `templates/login.html`

```html
{% extends "base.html" %}
{% block title %}Login · {{ site_name }}{% endblock %}
{% block content %}
  <h1>Login</h1>
  {% if query.error %}
    <p class="error">Invalid username or password.</p>
  {% endif %}
  <form method="post" action="/auth/login">
    <input type="hidden" name="next" value="{{ query.next }}">
    <label>Username <input type="text" name="username" required autofocus></label>
    <label>Password <input type="password" name="password" required></label>
    <button type="submit">Login</button>
  </form>
{% endblock %}
```

The hidden `next` field carries the original destination through the form POST. m6-auth validates that `next` is a relative path before redirecting — no client-side validation needed.

m6-auth handles `POST /auth/login` (form-encoded), verifies credentials, and either:
- **Success** → sets two HttpOnly cookies and `302` to `next`
- **Failure** → `302` back to `/login?error=invalid&next=<next>`

```
Set-Cookie: session=<access_token>;  HttpOnly; Secure; SameSite=Strict; Path=/;             Max-Age=900
Set-Cookie: refresh=<refresh_token>; HttpOnly; Secure; SameSite=Strict; Path=/auth/refresh; Max-Age=2592000
```

The `session` cookie is sent on every request — m6-http verifies it locally. The `refresh` cookie is sent only to `POST /auth/refresh` due to its restricted `Path`. No JavaScript involved anywhere in the auth flow.

**Session expiry:** When the `session` cookie expires, m6-http checks for a `refresh` cookie on the next protected request. If present and valid, new cookies are issued and the user is returned to where they were without seeing a login page. If the refresh token has also expired, the user is redirected to `/login?next=<original-path>`.

### 5.5. `templates/members.html`

```html
{% extends "base.html" %}
{% block title %}Members · {{ site_name }}{% endblock %}
{% block content %}
  <h1>Members Area</h1>
  <p>Welcome. This page is only visible after login.</p>
  <form method="post" action="/auth/logout">
    <button type="submit">Logout</button>
  </form>
{% endblock %}
```

Logout is a plain form POST — no JavaScript. m6-auth clears both cookies and redirects to `/`.

---

## 6. Example 05, CMS blog

`examples/05-cms/`

The full system. A public blog served at maximum speed from RAM cache. An authorised CMS renderer for creating, editing, and publishing posts.

Published posts are individual JSON files in `content/posts/` — one per post. Drafts live in `content/drafts/` and are never publicly routable. On publish, render-cms writes the post file, updates the index, and touches `site.toml` to trigger a route table reload in m6-http.

```bash
cd examples/05-cms
./setup.sh           # keys, first admin user
cargo build --release -p render-cms
./dev.sh
# CMS at https://localhost:8443/cms (login required)
# Blog at https://localhost:8443/blog (public, cached)
```

### 6.1. Site directory

```
05-cms/
├── site.toml
├── configs/
│   ├── system-dev.toml
│   ├── m6-html.conf
│   ├── m6-file.conf
│   ├── m6-auth.conf
│   └── render-cms.conf
├── templates/
│   ├── base.html
│   ├── home.html
│   ├── post-index.html
│   ├── post.html
│   ├── login.html
│   ├── error.html
│   └── cms/
│       ├── dashboard.html
│       └── editor.html
├── assets/
├── content/
├── data/
│   ├── posts/       ← published JSON (one file per post, written by CMS on publish)
│   └── drafts/      ← draft JSON (never publicly routable)
├── keys/
└── render-cms/
    ├── Cargo.toml
    └── src/main.rs
```

### 6.2. `site.toml`

```toml
[site]
name   = "My Blog"
domain = "localhost"

[auth]
backend    = "m6-auth"
public_key = "keys/auth.pub"

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
name    = "render-cms"
sockets = "/run/m6/render-cms-*.sock"

# ── Public routes — no auth, fully cached ─────────────────────

[[route]]
path    = "/"
backend = "m6-html"

[[route]]
path    = "/blog"
backend = "m6-html"

[[route_group]]
glob    = "content/posts/*.json"
path    = "/blog/{stem}"
backend = "m6-html"

[[route_group]]
glob    = "assets/**/*"
path    = "/assets/{relpath}"
backend = "m6-file"

[[route]]
path    = "/_errors"
backend = "m6-html"

# ── Auth endpoints — public ────────────────────────────────────

[[route]]
path    = "/login"
backend = "m6-html"

[[route]]
path    = "/auth/login"
backend = "m6-auth"

[[route]]
path    = "/auth/logout"
backend = "m6-auth"

[[route]]
path    = "/auth/refresh"
backend = "m6-auth"

# ── CMS — all protected, never cached ─────────────────────────

[[route]]
path    = "/cms"
backend = "render-cms"
require = "group:editors"

[[route]]
path    = "/cms/edit/{stem}"
backend = "render-cms"
require = "group:editors"

[[route]]
path    = "/cms/new"
backend = "render-cms"
require = "group:editors"

[[route]]
path    = "/api/drafts"
backend = "render-cms"
require = "group:editors"

[[route]]
path    = "/api/drafts/{stem}"
backend = "render-cms"
require = "group:editors"

[[route]]
path    = "/api/publish/{stem}"
backend = "render-cms"
require = "group:editors"

[[route]]
path    = "/api/unpublish/{stem}"
backend = "render-cms"
require = "group:editors"
```

`[[route_group]]` with `content/posts/*.json` handles routing for individual posts. New post files become routable when m6-http reloads after `site.toml` is touched on publish.

### 6.3. `render-cms/Cargo.toml`

```toml
[package]
name    = "render-cms"
version = "0.1.0"
edition = "2021"

[dependencies]
m6-render  = { git = "https://github.com/m6/m6", tag = "v0.1.0", features = ["auth", "disk"] }
serde_json = "1"
```

### 6.4. `render-cms/src/main.rs`

```rust
use m6_render::prelude::*;
use serde_json::{json, Value};
use std::fs;

fn main() -> Result<()> {
    // render-cms uses JSON files directly — no state needed.
    // A real CMS would use App::with_state(init_global, init_thread)
    // to hold a database connection per thread.
    App::new()
        .route_get("/cms",             handle_dashboard)
        .route_get("/cms/new",         handle_new)
        .route_get("/cms/edit/{stem}", handle_edit)
        .route_post("/api/drafts",           handle_create_draft)
        .route_patch("/api/drafts/{stem}",   handle_update_draft)
        .route_post("/api/publish/{stem}",   handle_publish)
        .route_post("/api/unpublish/{stem}", handle_unpublish)
        .run()
}

fn handle_dashboard(req: &Request) -> Result<Response> {
    let author    = req["auth_username"].as_str().unwrap_or("");
    let drafts    = req.list_json("content/drafts/")?;
    let published = req.list_json("content/posts/")?;
    Response::render_with("templates/cms/dashboard.html", req, json!({
        "drafts":    drafts,
        "published": published,
        "author":    author,
    }))
}

fn handle_new(req: &Request) -> Result<Response> {
    Response::render_with("templates/cms/editor.html", req, json!({"new": true}))
}

fn handle_edit(req: &Request) -> Result<Response> {
    let stem = req["stem"].as_str().ok_or(Error::NotFound)?;
    let post = req.read_json(&format!("content/drafts/{}.json", stem))
        .or_else(|_| req.read_json(&format!("content/posts/{}.json", stem)))?;
    Response::render_with("templates/cms/editor.html", req, post)
}

fn handle_create_draft(req: &Request) -> Result<Response> {
    let body: Value = req.body_json()?;
    let stem  = req["title"].as_str().unwrap_or("untitled").to_slug();
    let draft = json!({
        "stem":   stem,
        "title":  body["title"],
        "body":   body["body"],
        "author": req["auth_username"],
        "date":   today_iso8601(),
    });
    req.write_json(&format!("content/drafts/{}.json", stem), &draft)?;
    Response::json_status(json!({"stem": stem}), 201)
}

fn handle_update_draft(req: &Request) -> Result<Response> {
    let stem = req["stem"].as_str().ok_or(Error::NotFound)?;
    let body: Value = req.body_json()?;
    let path = format!("content/drafts/{}.json", stem);
    let mut draft = req.read_json(&path)?;
    if let Some(t) = body.get("title") { draft["title"] = t.clone(); }
    if let Some(b) = body.get("body")  { draft["body"]  = b.clone(); }
    req.write_json(&path, &draft)?;
    Response::json(json!({"ok": true}))
}

fn handle_publish(req: &Request) -> Result<Response> {
    let stem         = req["stem"].as_str().ok_or(Error::NotFound)?;
    let draft_path   = format!("content/drafts/{}.json", stem);
    let publish_path = format!("content/posts/{}.json", stem);

    let mut post = req.read_json(&draft_path)?;
    post["stem"]         = json!(stem);
    post["path"]         = json!(format!("/blog/{}", stem));
    post["published_at"] = json!(now_iso8601());

    req.write_json_atomic(&publish_path, &post)?;
    update_index(req)?;
    req.touch_site_toml()?;
    let _ = fs::remove_file(req.site_path(&draft_path));

    Response::json(json!({"published": true, "path": post["path"]}))
}

fn handle_unpublish(req: &Request) -> Result<Response> {
    let stem         = req["stem"].as_str().ok_or(Error::NotFound)?;
    let publish_path = format!("content/posts/{}.json", stem);
    let draft_path   = format!("content/drafts/{}.json", stem);

    let mut post = req.read_json(&publish_path)?;
    post["draft"] = json!(true);
    req.write_json(&draft_path, &post)?;
    fs::remove_file(req.site_path(&publish_path))?;
    update_index(req)?;
    req.touch_site_toml()?;

    Response::json(json!({"unpublished": true}))
}

```

### 6.5. The publish flow

1. Editor saves draft → `POST /api/drafts` or `PATCH /api/drafts/{stem}` writes `content/drafts/{stem}.json`
2. Editor clicks **Publish** → `POST /api/publish/{stem}`:
   - render-cms writes `content/posts/{stem}.json` — inotify fires, m6-http evicts `/blog/{stem}` from cache
   - render-cms writes updated `content/posts/_index.json` — inotify fires, m6-http evicts `/blog`
   - render-cms touches `site.toml` — m6-http reloads route table, re-expands `[[route_group]]` glob, `/blog/{stem}` route becomes live
   - render-cms deletes `content/drafts/{stem}.json`
3. Browser redirects to `/blog/{stem}`
4. m6-http: route now live, cache miss → m6-html reads `content/posts/{stem}.json`, renders → m6-http caches
5. Every subsequent request to `/blog/{stem}`: cache hit — RAM only

### 6.6. Performance profile

| Request type | Latency |
|---|---|
| Public post (cached) | < 1ms — RAM only |
| Public post (cache miss) | ~5ms — m6-html reads `content/posts/{stem}.json`, renders, caches |
| CMS dashboard | ~10ms — JWT verified locally, render-cms reads posts directory + drafts directory |
| Publish | ~5ms — one atomic file write, one inotify event |

### 6.7. `dev.sh`

```bash
#!/bin/bash
set -e
SITE="$(cd "$(dirname "$0")" && pwd)"
trap 'kill $(jobs -p) 2>/dev/null' EXIT

m6-html    "$SITE" "$SITE/configs/m6-html.conf" &
m6-file    "$SITE" "$SITE/configs/m6-file.conf" &
m6-auth    "$SITE" "$SITE/configs/m6-auth.conf" &
"$SITE/target/release/render-cms" "$SITE" "$SITE/configs/render-cms.conf" &
m6-http    "$SITE" "$SITE/configs/system-dev.toml" &

echo "Running at https://localhost:8443"
echo "CMS at    https://localhost:8443/cms"
wait
```

## 7. Example 06, production with systemd

`examples/06-systemd/`

Takes the CMS blog from example 05 and runs it properly under systemd. Every process is a systemd unit. Ordering, restart policy, log capture, socket directory permissions, deploy workflow, and horizontal scaling are all covered.

The unit files live in `examples/06-systemd/systemd/` and are copied into place during setup.

---

### 7.1. Server preparation

```bash
# Create a dedicated user — no login shell, no home directory
useradd --system --no-create-home --shell /usr/sbin/nologin m6

# Site directory — owned by m6 user
mkdir -p /var/www/my-blog
chown m6:m6 /var/www/my-blog

# Runtime directory for Unix sockets
# systemd will create /run/m6 and set ownership automatically
# via RuntimeDirectory= in each unit (see below)

# Auth keys — readable by m6 user only
mkdir -p /etc/m6
openssl ecparam -name prime256v1 -genkey -noout -out /etc/m6/auth.pem
openssl ec -in /etc/m6/auth.pem -pubout -out /etc/m6/auth.pub
chown m6:m6 /etc/m6/auth.pem /etc/m6/auth.pub
chmod 600   /etc/m6/auth.pem

# TLS certificate (example uses certbot/Let's Encrypt)
certbot certonly --standalone -d example.com
# Outputs /etc/letsencrypt/live/example.com/fullchain.pem
#         /etc/letsencrypt/live/example.com/privkey.pem
# m6-http reads these directly — add m6 to the ssl-cert group or adjust permissions
```

m6-http does not watch the certificate files, so a renewal has to touch the system config after it writes them.
This hook does it, and certbot runs it after each renewal:

```sh
# /etc/letsencrypt/renewal-hooks/deploy/m6-touch.sh, mode 0755
#!/bin/sh
touch /etc/m6/my-blog.toml
```

The touch is what makes m6-http read the new files, and until it runs m6-http keeps serving the previous certificate.

---

### 7.2. Deploy site directory

```bash
# From your development machine
rsync -av --delete \
  --exclude 'content/drafts/' \
  --exclude 'data/auth.db' \
  --exclude 'keys/' \
  examples/05-cms/ \
  user@example.com:/var/www/my-blog/

# content/posts/ excluded — managed by the CMS renderer on the server
# auth.db excluded — persistent server-side state
# keys/ excluded — already in /etc/m6/ on the server

# On the server, fix ownership after rsync
chown -R m6:m6 /var/www/my-blog
```

Production `site.toml` differs from development in three places: `bind`, `tls_cert`, `tls_key`.

`/var/www/my-blog/site.toml`:

```toml
[site]
name   = "My Blog"
domain = "example.com"

[server]
bind     = "0.0.0.0:443"
tls_cert = "/etc/letsencrypt/live/example.com/fullchain.pem"
tls_key  = "/etc/letsencrypt/live/example.com/privkey.pem"

[errors]
mode = "internal"

[auth]
backend    = "m6-auth"
public_key = "/etc/m6/auth.pub"

# backends and routes identical to example 05
```

---

### 7.3. Unit files

One unit per process. The units for renderers all follow the same pattern — only `ExecStart` and `Description` differ.

**`/etc/systemd/system/m6-html.service`**

```ini
[Unit]
Description=m6-html renderer
Documentation=https://github.com/m6/m6
# Start after network is up but before m6-http
After=network.target

[Service]
Type=simple
User=m6
Group=m6

# Creates /run/m6/ with correct ownership on start, removes on stop
RuntimeDirectory=m6
RuntimeDirectoryMode=0750

ExecStart=/usr/local/bin/m6-html \
    /var/www/my-blog \
    /var/www/my-blog/configs/m6-html.conf

# Restart on any non-clean exit — covers crashes and OOM kills
Restart=on-failure
RestartSec=2

# All output goes to journald
StandardOutput=journal
StandardError=journal
SyslogIdentifier=m6-html

# Hardening
NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=true
ReadWritePaths=/run/m6
ReadOnlyPaths=/var/www/my-blog /etc/m6

[Install]
WantedBy=multi-user.target
```

**`/etc/systemd/system/m6-file.service`**

```ini
[Unit]
Description=m6-file renderer
After=network.target

[Service]
Type=simple
User=m6
Group=m6
RuntimeDirectory=m6
RuntimeDirectoryMode=0750

ExecStart=/usr/local/bin/m6-file \
    /var/www/my-blog \
    /var/www/my-blog/configs/m6-file.conf

Restart=on-failure
RestartSec=2
StandardOutput=journal
StandardError=journal
SyslogIdentifier=m6-file
NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=true
ReadWritePaths=/run/m6
ReadOnlyPaths=/var/www/my-blog

[Install]
WantedBy=multi-user.target
```

**`/etc/systemd/system/m6-auth.service`**

```ini
[Unit]
Description=m6-auth service
After=network.target

[Service]
Type=simple
User=m6
Group=m6
RuntimeDirectory=m6
RuntimeDirectoryMode=0750

ExecStart=/usr/local/bin/m6-auth \
    /var/www/my-blog \
    /var/www/my-blog/configs/m6-auth.conf

Restart=on-failure
RestartSec=2
StandardOutput=journal
StandardError=journal
SyslogIdentifier=m6-auth
NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=true
ReadWritePaths=/run/m6 /var/www/my-blog/data
ReadOnlyPaths=/var/www/my-blog /etc/m6

[Install]
WantedBy=multi-user.target
```

Note `ReadWritePaths` includes `/var/www/my-blog/data` — m6-auth writes the SQLite database there.

**`/etc/systemd/system/render-cms.service`**

```ini
[Unit]
Description=render-cms custom renderer
After=network.target

[Service]
Type=simple
User=m6
Group=m6
RuntimeDirectory=m6
RuntimeDirectoryMode=0750

ExecStart=/var/www/my-blog/bin/render-cms \
    /var/www/my-blog \
    /var/www/my-blog/configs/render-cms.conf

Restart=on-failure
RestartSec=2
StandardOutput=journal
StandardError=journal
SyslogIdentifier=render-cms
NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=true
ReadWritePaths=/run/m6 /var/www/my-blog/content
ReadOnlyPaths=/var/www/my-blog /etc/m6

[Install]
WantedBy=multi-user.target
```

The `render-cms` binary lives in `/var/www/my-blog/bin/` — deployed alongside the site.

**`/etc/systemd/system/m6-http.service`**

```ini
[Unit]
Description=m6-http reverse proxy and cache
Documentation=https://github.com/m6/m6
After=network.target m6-html.service m6-file.service m6-auth.service render-cms.service

[Service]
Type=simple
User=m6
Group=m6
RuntimeDirectory=m6
RuntimeDirectoryMode=0750

ExecStart=/usr/local/bin/m6-http /var/www/my-blog

Restart=on-failure
RestartSec=2
StandardOutput=journal
StandardError=journal
SyslogIdentifier=m6-http

# Bind to port 443 — requires capability or use authbind/setcap
AmbientCapabilities=CAP_NET_BIND_SERVICE
NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=true
ReadWritePaths=/run/m6
ReadOnlyPaths=/var/www/my-blog /etc/letsencrypt /etc/m6

[Install]
WantedBy=multi-user.target
```

`After=` expresses preference, not a hard dependency — m6-http starts after renderers but will handle unreachable backends gracefully if one hasn't started yet. The socket pool's backoff mechanism handles the brief window between m6-http binding and all renderers being ready.

---

### 7.4. Install and start

```bash
# Copy unit files
cp examples/06-systemd/systemd/*.service /etc/systemd/system/

# Reload systemd
systemctl daemon-reload

# Enable all units (start on boot)
systemctl enable m6-html m6-file m6-auth render-cms m6-http

# Start
systemctl start m6-html m6-file m6-auth render-cms m6-http

# Verify all running
systemctl status m6-html m6-file m6-auth render-cms m6-http
```

Expected output for each unit:

```
● m6-html.service - m6-html renderer
     Loaded: loaded (/etc/systemd/system/m6-html.service; enabled)
     Active: active (running) since ...
   Main PID: 12345 (m6-html)
```

---

### 7.5. Verify

```bash
# Check m6-http is listening
ss -tlnp | grep :443

# Check all sockets appeared in /run/m6/
ls -la /run/m6/
# Expected:
# /run/m6/m6-html.sock
# /run/m6/m6-file.sock
# /run/m6/m6-auth.sock
# /run/m6/render-cms.sock

# Smoke test
curl -s -o /dev/null -w "%{http_code}" https://example.com/
# 200

curl -s -o /dev/null -w "%{http_code}" https://example.com/blog
# 200

curl -s -o /dev/null -w "%{http_code}" https://example.com/cms
# 302 (redirect to /login)
```

---

### 7.6. Logs

```bash
# Follow all m6 processes together
journalctl -f -u m6-http -u m6-html -u m6-file -u m6-auth -u render-cms

# One process
journalctl -u m6-http -f

# JSON output for structured log processing
journalctl -u m6-http -o cat | jq .

# Last 100 lines
journalctl -u m6-http -n 100

# Since last boot
journalctl -u m6-http -b

# Errors only
journalctl -u m6-http -p err

# Between timestamps
journalctl -u m6-http --since "2024-01-15 09:00" --until "2024-01-15 10:00"
```

---

### 7.7. Rotating the analytics log

`m6-http` writes the analytics log to one file and reopens it when it receives SIGUSR1.
Rotation renames the file and sends the signal, so no line is lost, and `copytruncate` is not needed.
This logrotate stanza rotates the file every hour and keeps the last 48 files, two days.

```
/var/www/example/logs/analytics.ndjson {
    hourly
    dateext
    dateformat -%Y%m%d%H
    rotate 48
    compress
    missingok
    notifempty
    postrotate
        systemctl kill --kill-whom=main --signal=SIGUSR1 m6-http.service
        timeout 10 sh -c 'until test -e "$1"; do sleep 0.1; done' sh "$1"
    endscript
}
```

`rotate` is a finite count because every kept file uses disk, and `/traffic` lists the directory to find the newest rotated file.
`rotate -1` keeps files forever and is not suitable.
Choose a count that fits the disk at your traffic.

`hourly` only takes effect if logrotate itself runs every hour, and distributions run it once a day.
Run it hourly with a systemd timer override.

```
# systemctl edit logrotate.timer
[Timer]
OnCalendar=
OnCalendar=hourly
AccuracySec=1m
```

On a system that uses cron instead, add `/etc/cron.hourly/logrotate` containing `#!/bin/sh` and `exec /usr/sbin/logrotate /etc/logrotate.conf`, and make it executable.
Keep the daily run too if other stanzas use `daily`, which is harmless because logrotate skips a file that is not due.

There is no `create` line, because `m6-http` creates the file itself.
The wait in `postrotate` is a proof.
The log writer cannot write to the renamed file once the new file exists, so the old file is finished before logrotate compresses it.
If the reopen fails, the wait times out and `postrotate` fails.
`--kill-whom=` needs systemd 252 or later, and older systemd spells it `--kill-who=`.

`/perf` reports the last reopen as `logging.reopen`, with the seconds since the last reopen, how many reopens succeeded and failed, and the age of the last failure.
`m6-monitor` shows it for each node in section A of `/check` and in `/digest`.
Setting `[monitor] log_reopen_max_age_s` makes it a fault when a node's last reopen is older than that many seconds.
A limit of twice the rotation interval suits an hourly rotation, so 7200 seconds with the schedule above.
The limit is only as true as the schedule, so set it after logrotate runs hourly.
`/traffic` reads the newest rotated file next to the log when the live file does not cover its window, so its sixty minute window does not shrink just after a rotation.
It skips a rotated file last written before the window, and a `.gz` file larger than the 24 MB read budget on disk or once decoded.

---

### 7.8. Monitor history

`m6-monitor` keeps a history of every poll when `[monitor] history_file` is set, and serves it as JSON Lines.
We cover the keys, the two endpoints, paging, how to protect them and what the history leaves out.
Table 1 gives the two keys, which are read once at start.

| key (`[monitor]`) | default | rule |
|---|---|---|
| `history_file` | absent, so no history | a non-empty path, relative to the site directory |
| `history_max_mb` | 256 | 1 to 1048576 |

**Table 1: the two `[monitor]` keys that switch the history on and cap it, with their defaults and rules.**
A change to either needs a restart.

The file holds one row per node per poll, one row per channel and interface, the text of every finding, and once per `/traffic` window the response status counts and crawler sightings of each node.
When the file passes `history_max_mb`, the oldest polls are deleted until it is 5% under the cap, and nothing expires by age.
One window of one node stores at most 100 crawler sightings, the ones with the most requests, and cuts each user agent to 256 bytes.
The `/crawlers` key `sightings_dropped` says how many were left out.
On Unix the monitor creates the directory with mode 0700 and the file with mode 0600, because the file holds client addresses.
A write that fails is a warning on node `monitor` and never stops polling.
A prune that fails, or a cap that deleting every earlier poll cannot meet, is its own warning, and the poll is still stored.
To start fresh, stop the service, delete the file and start it again.

`GET /history` and `GET /crawlers` answer on the service's unix socket and nowhere else.
Both take `from` and `to` as RFC 3339 UTC times, the range `[from, to)` is at most 31 days, and `node` limits the answer to one node.
A page holds at most 4 MiB and 10,000 rows, and a `Link: <...>; rel="next"` header names the next page until the last one.

```
curl --unix-socket /run/m6/m6-monitor.sock \
  'http://localhost/history?from=2026-01-01T00:00:00Z&to=2026-01-02T00:00:00Z&node=edge-a'
```

The monitor has no listener and no token code.
Put an m6-http in front of the socket and protect the routes with `require`, the way Example 11 protects its API:

```toml
[[backend]]
name    = "monitor"
sockets = "/run/m6/m6-monitor.sock"

[[route]]
path    = "/history"
backend = "monitor"
require = "role:admin"

[[route]]
path    = "/crawlers"
backend = "monitor"
require = "role:admin"
```

The monitor sends `Cache-Control: no-store` on every page itself, so the routes need no `cache` key.

The node's own `periodic stats` log is a 10 second series that sits in the node's journal, which is capped.
It has no channels, reachability, host figures or tunnel, and it disappears while a node is down.
Only the history keeps those, and it keeps them through an outage.
Channel counters include the monitor's own polls and header checks (issue #107), and `monitor_requests` on each node row says how many.

---

### 7.9. Deploy updates

```bash
# Code or template changes — rsync and done
# m6-http detects site.toml change via inotify and reloads routing
rsync -av --delete \
  --exclude 'content/drafts/' \
  --exclude 'data/auth.db' \
  --exclude 'keys/' \
  examples/05-cms/ \
  user@example.com:/var/www/my-blog/

# If render-cms binary changed — rebuild, deploy binary, restart just that unit
cargo build --release -p render-cms
rsync target/release/render-cms user@example.com:/var/www/my-blog/bin/
ssh user@example.com systemctl restart render-cms
# Other units unaffected — m6-http detects render-cms socket reappear and resumes routing

# TLS cert renewal (certbot renews, and its deploy hook touches the system config)
certbot renew
# m6-http reads the new certificate when the system config is touched, with no restart
```

---

### 7.10. Horizontal scaling

Add a second m6-html instance to handle more traffic. No config change to m6-http — it detects the new socket automatically.

```bash
# Create a second config (content identical to m6-html.conf)
cp /var/www/my-blog/configs/m6-html.conf \
   /var/www/my-blog/configs/m6-html-2.conf

# Create the unit by copying and editing ExecStart
cp /etc/systemd/system/m6-html.service \
   /etc/systemd/system/m6-html-2.service

# Edit the new unit
sed -i \
  -e 's/Description=m6-html renderer/Description=m6-html renderer (instance 2)/' \
  -e 's/m6-html.conf/m6-html-2.conf/' \
  -e 's/SyslogIdentifier=m6-html/SyslogIdentifier=m6-html-2/' \
  /etc/systemd/system/m6-html-2.service

systemctl daemon-reload
systemctl enable m6-html-2
systemctl start m6-html-2

# /run/m6/m6-html-2.sock appears
# m6-http detects it via inotify — adds to pool within milliseconds
# Traffic now load-balanced across both instances

# Verify
ls /run/m6/
# m6-html.sock  m6-html-2.sock  m6-file.sock  m6-auth.sock  render-cms.sock

journalctl -f -u m6-html -u m6-html-2
```

To scale back down:

```bash
systemctl stop m6-html-2
# Socket disappears — m6-http removes from pool immediately
# No config change, no m6-http restart
```

---

### 7.11. Crash recovery

systemd restarts failed units automatically with `Restart=on-failure` and `RestartSec=2`. During the brief restart window, m6-http's pool backoff handles missing sockets — requests to affected routes return 503 until the socket reappears.

```bash
# Simulate a crash
systemctl kill -s KILL m6-html

# Watch systemd restart it
journalctl -u m6-html -f
# ... process exited with status 9 (SIGKILL)
# ... Starting m6-html renderer...
# ... m6-html started, listening on /run/m6/m6-html.sock

# m6-http detects socket reappearance via inotify — normal service resumes
```

---

## 8. Example 07, dev to production

`examples/07-dev-to-production/`

The complete workflow for taking a site from local development to a production server. The same site directory runs in both environments — the system config provides the three keys that differ.

---

### 8.1. The split

**`site.toml` — version controlled, deployed with the site, contains no secrets:**

```toml
[site]
name   = "My Blog"
domain = "example.com"

[server]
# Development values — system config overrides these in production
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

# All backends and routes — identical in dev and production
[[backend]]
name    = "m6-html"
sockets = "/run/m6/m6-html-*.sock"
# ... (full backend and route table as in example 05)
```

**`configs/system-dev.toml` — development system config, version controlled:**

```toml
# The three keys that differ in development.
# Committed to the repo — contains no secrets.
[server]
bind     = "127.0.0.1:8443"
tls_cert = "../../localhost.pem"
tls_key  = "../../localhost-key.pem"
```

**`/etc/m6/my-blog.toml` — production system config, on the server only:**

```toml
[server]
bind     = "0.0.0.0:443"
tls_cert = "/etc/letsencrypt/live/example.com/fullchain.pem"
tls_key  = "/etc/letsencrypt/live/example.com/privkey.pem"
```

Only `[server]`. Nothing else.

**`configs/render-contact.conf` — version controlled, uses local mock values:**

```toml
secrets_file = "/etc/m6/contact-secrets.toml"
# On dev machines /etc/m6/contact-secrets.toml doesn't exist
# — silently ignored, [smtp] values below are used directly.

global_params = ["data/site.json"]

[[route]]
path     = "/contact"
template = "templates/contact.html"
params   = []
cache    = "no-store"

[smtp]
host     = "localhost"
port     = 1025
username = ""
password = ""
from     = "dev@localhost"
to       = "dev@localhost"
```

**`/etc/m6/contact-secrets.toml` — on the server only:**

```toml
[smtp]
host     = "smtp.postmarkapp.com"
port     = 587
username = "live-api-token"
password = "live-api-token"
from     = "noreply@example.com"
to       = "owner@example.com"
```

When `secrets_file` points to a file that exists, it is merged in — secrets file wins on conflict. When the file is absent (on any dev machine), it is silently ignored and the `[smtp]` block provides values directly. The path itself is not secret and is safe to commit.

---

### 8.2. Dev script

```bash
#!/bin/bash
set -e
SITE="$(cd "$(dirname "$0")" && pwd)"
trap 'kill $(jobs -p) 2>/dev/null' EXIT

m6-html "$SITE" "$SITE/configs/m6-html.conf" &
m6-file "$SITE" "$SITE/configs/m6-file.conf" &
m6-auth "$SITE" "$SITE/configs/m6-auth.conf" &
"$SITE/target/release/render-cms" "$SITE" "$SITE/configs/render-cms.conf" &

# Second argument always required — use checked-in dev system config
m6-http "$SITE" "$SITE/configs/system-dev.toml" &

echo "Running at https://localhost:8443"
wait
```

---

### 8.3. First-time server setup

```bash
# On the server

# Create system user
useradd --system --no-create-home --shell /usr/sbin/nologin m6

# Create site directory
mkdir -p /var/www/my-blog
chown m6:m6 /var/www/my-blog

# Generate production auth keys
mkdir -p /etc/m6
openssl ecparam -name prime256v1 -genkey -noout -out /etc/m6/auth.pem
openssl ec -in /etc/m6/auth.pem -pubout -out /etc/m6/auth.pub
chown m6:m6 /etc/m6/auth.pem /etc/m6/auth.pub
chmod 600 /etc/m6/auth.pem

# Create production system config
cat > /etc/m6/my-blog.toml << 'TOML'
[server]
bind     = "0.0.0.0:443"
tls_cert = "/etc/letsencrypt/live/example.com/fullchain.pem"
tls_key  = "/etc/letsencrypt/live/example.com/privkey.pem"
TOML
chown root:m6 /etc/m6/my-blog.toml
chmod 640 /etc/m6/my-blog.toml

# Create SMTP secrets
cat > /etc/m6/contact-secrets.toml << 'TOML'
[smtp]
host     = "smtp.postmarkapp.com"
port     = 587
username = "live-api-token"
password = "live-api-token"
from     = "noreply@example.com"
to       = "owner@example.com"
TOML
chown root:m6 /etc/m6/contact-secrets.toml
chmod 640 /etc/m6/contact-secrets.toml

# Obtain TLS certificate
certbot certonly --standalone -d example.com

# Install systemd units
cp examples/07-dev-to-production/systemd/*.service /etc/systemd/system/
systemctl daemon-reload

# Deploy site (see deploy.sh below), then:
systemctl enable m6-html m6-file m6-auth render-cms m6-http
systemctl start  m6-html m6-file m6-auth render-cms m6-http
```

---

### 8.4. Systemd unit, m6-http

The only difference from example 06 is `ExecStart` includes the system config path:

```ini
[Unit]
Description=m6-http reverse proxy and cache
After=network.target m6-html.service m6-file.service m6-auth.service render-cms.service

[Service]
Type=simple
User=m6
Group=m6
RuntimeDirectory=m6
RuntimeDirectoryMode=0750

ExecStart=/usr/local/bin/m6-http \
    /var/www/my-blog \
    /etc/m6/my-blog.toml

Restart=on-failure
RestartSec=2
StandardOutput=journal
StandardError=journal
SyslogIdentifier=m6-http
AmbientCapabilities=CAP_NET_BIND_SERVICE
NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=true
ReadWritePaths=/run/m6
ReadOnlyPaths=/var/www/my-blog /etc/letsencrypt /etc/m6

[Install]
WantedBy=multi-user.target
```

Renderer units are identical to example 06 — they take only two positional args and use `secrets_file` for credentials rather than a system config argument.

---

### 8.5. Deploy

```bash
#!/bin/bash
# deploy.sh — run from development machine
set -e

SERVER="user@example.com"
REMOTE_SITE="/var/www/my-blog"

rsync -av --delete \
  --exclude 'content/drafts/' \
  --exclude 'data/auth.db' \
  --exclude 'keys/' \
  --exclude '*.pem' \
  --exclude '*.pub' \
  --exclude 'target/' \
  ./ "$SERVER:$REMOTE_SITE/"

ssh "$SERVER" chown -R m6:m6 "$REMOTE_SITE"

if [[ "$1" == "--binary" ]]; then
  cargo build --release -p render-cms
  rsync target/release/render-cms "$SERVER:$REMOTE_SITE/bin/"
  ssh "$SERVER" systemctl restart render-cms
fi

echo "Deployed."
# m6-http detects site.toml change via inotify and reloads routing
```

---

### 8.6. Dev vs production at a glance

| | Development | Production |
|---|---|---|
| `m6-http` invocation | `m6-http $SITE $SITE/configs/system-dev.toml` | `m6-http /var/www/my-blog /etc/m6/my-blog.toml` |
| `[server] bind` | `127.0.0.1:8443` | `0.0.0.0:443` |
| `[server] tls_cert` | `localhost.pem` (mkcert) | `/etc/letsencrypt/...` |
| SMTP | `localhost:1025` (mock) | `smtp.postmarkapp.com:587` |
| SMTP credentials | inline in renderer config | `/etc/m6/contact-secrets.toml` via `secrets_file` |
| System config | `configs/system-dev.toml` (in repo) | `/etc/m6/my-blog.toml` (on server) |
| Process manager | `dev.sh` | systemd |
| Logs | terminal stdout | journald |

Routes, templates, backends, auth config, and log settings are identical. The same `site.toml` runs in both environments.

---

### 8.7. Checking what's in effect

```bash
# See the effective merged config — useful when debugging prod behaviour
m6-http /var/www/my-blog /etc/m6/my-blog.toml --dump-config

# Output shows [server] values from /etc/m6/my-blog.toml,
# everything else from site.toml:
#
# [server]
# bind     = "0.0.0.0:443"
# tls_cert = "/etc/letsencrypt/live/example.com/fullchain.pem"
# tls_key  = "/etc/letsencrypt/live/example.com/privkey.pem"
#
# [site]
# name   = "My Blog"
# domain = "example.com"
# ...
```

---

### 8.8. Common mistakes

**Forgetting the system config argument:**
```bash
m6-http /var/www/my-blog
# Exit 2 — second argument required
```

**Putting secrets in renderer config instead of secrets_file:**
```toml
# Wrong — committed to repo, visible to anyone who can clone
[smtp]
password = "live-api-token"   # in configs/render-contact.conf

# Right — in /etc/m6/contact-secrets.toml, referenced by secrets_file
```

**Deploying over `data/posts.json` — wipes published posts:**
```bash
# Wrong
rsync -av ./ user@example.com:/var/www/my-blog/

# Right — always exclude server-managed state
rsync -av --exclude 'content/drafts/' --exclude 'data/posts.json' --exclude 'data/auth.db' \
  ./ user@example.com:/var/www/my-blog/
```

**Editing `site.toml` on the server directly:**
```bash
# Wrong — next deploy overwrites it
ssh user@example.com vim /var/www/my-blog/site.toml

# Right — edit locally and deploy
# Server-specific overrides belong in /etc/m6/my-blog.toml
```

---

## 9. Example 08, log viewer

`examples/08-logviewer/`

A real-time log viewer built into the site. m6-http writes JSON-format logs to disk. A browser UI polls a tail endpoint served by m6-file and renders incoming lines with filtering and search. Demonstrates `format = "json"` logging and m6-file's byte-range tail mode.

```bash
cd examples/08-logviewer
./dev.sh
# open https://localhost:8443/logs
```

---

## 10. Example 09, global deployment

`examples/09-global-deployment/`

A multi-region deployment: an origin node runs the full m6 stack, and multiple cache nodes around the world run m6-http in pure proxy mode. A private mesh network connects all nodes. Cache invalidation propagates from origin to all cache nodes on publish. Demonstrates horizontal scale-out across regions without a CDN.

```bash
cd examples/09-global-deployment
# See setup-origin.sh and setup-cache-node.sh
```

---

## 11. Example 10, API tokens

`examples/10-api-tokens/`

Adds long-lived API tokens for scripts and services. Unlike session cookies (15-minute TTL, browser only), an API token is a JWT passed as `Authorization: Bearer <token>` and can be valid for days or months. m6-http verifies Bearer tokens the same way it verifies session cookies — same EC key, same `require` rules. No special server configuration is needed.

```bash
cd examples/10-api-tokens
./dev.sh
```

### 11.1. When to use API tokens

| | Session cookie | API token |
|---|---|---|
| Caller | Browser | Script / service / CI |
| Lifetime | 15 min (refreshes to 30 days) | Configurable (default 30 days) |
| Transport | `Cookie: session=...` | `Authorization: Bearer ...` |
| Issuance | `POST /auth/login` | `m6-auth-cli token create` |
| Revocation | `POST /auth/logout` | `m6-auth-cli token revoke` |

### 11.2. Setup

```bash
# 1. Create a user with the roles your API needs
m6-auth-cli configs/m6-auth.conf user add api --role api --role user --password secret

# 2. Issue a token
TOKEN=$(m6-auth-cli configs/m6-auth.conf token create api --name ci-pipeline --ttl-days 30)
echo "$TOKEN"
# eyJhbGciOiJFUzI1NiIsInR5cCI6IkpXVCJ9...

# 3. Use it
curl -sk https://localhost:8443/api/data \
     -H "Authorization: Bearer $TOKEN"
```

### 11.3. `site.toml`, protecting an API endpoint

```toml
# API endpoint — requires a valid Bearer token with role 'api'
[[route]]
path    = "/api/data"
backend = "m6-html"
require = "role:api"
```

`require = "role:api"` works for both session cookies and Bearer tokens. A browser user logged in with the `api` role can also access the route. For endpoints that should be machine-only, use a dedicated role name like `api` and never assign it to regular user accounts.

### 11.4. Token management

```bash
# List active tokens for a user
m6-auth-cli configs/m6-auth.conf token ls api
# NAME            ID                                    CREATED     EXPIRES
# ci-pipeline     550e8400-e29b-41d4-a716-446655440000  2026-03-21  2026-04-20

# List as JSON
m6-auth-cli configs/m6-auth.conf token ls api --json

# Revoke by ID
m6-auth-cli configs/m6-auth.conf token revoke 550e8400-e29b-41d4-a716-446655440000
```

**Revocation note:** `token revoke` removes the token from the database and the listing. Because JWTs are stateless, a revoked token remains cryptographically valid until its `exp` claim. For immediate revocation, use `--ttl-days 1` so tokens expire quickly on their own.

### 11.5. Template variables in protected routes

When a request carries a valid Bearer token, m6-html templates can access the caller's identity via the `auth` object — the same object available after browser login:

```html
<p>Caller: {{ auth.username }}</p>
<p>Roles:  {{ auth.roles | join(sep=", ") }}</p>
```

### 11.6. Functional tests

`test.sh` in the example directory runs against a live server:

```bash
# In one terminal
./dev.sh

# In another
./test.sh
```

Tests cover: public access, 401 on unauthenticated access, token create/list/revoke, role enforcement, and custom error pages.


---

## 12. Example 11, admin dashboard API

`examples/11-admin-dashboard/`

A self-hosting management API and AJAJ dashboard for m6 sites. All endpoints return a single JSON blob — one blob per "tab". Built as a custom renderer (`render-admin`) using `App::with_global`.

All `/api/admin/*` routes are protected by `require = "role:admin"` in `site.toml`. Only requests carrying a valid Bearer token (or session cookie) with the `admin` role are accepted.

The static AJAJ dashboard (`public/index.html`) is served by `m6-file` at `https://localhost:8444/`. It uses skeleton-loading (shimmer placeholders animate immediately; AJAX fills in real data) and auto-refreshes every 10 seconds. Six tabs: Perf, System, Bench, Logs, Routes, Config.

```bash
cd examples/11-admin-dashboard
./setup.sh
./dev.sh
# open https://localhost:8444/
```

### 12.1. API endpoints

| Method | Path | Returns |
|--------|------|---------|
| `GET` | `/api/admin/perf` | Last N "periodic stats" entries from m6-http log |
| `GET` | `/api/admin/routes` | Per-route request counts, cache rates, avg latency |
| `GET` | `/api/admin/system` | CPU%, RAM, disk usage, uptime |
| `GET` | `/api/admin/bench` | Bench targets auto-discovered from `site.toml` |
| `POST` | `/api/admin/bench` | Start a bench job → `{ "job_id": "..." }` |
| `GET` | `/api/admin/bench/{id}` | Poll job status and output |
| `GET` | `/api/admin/logs` | Last N log lines as JSON array |
| `GET` | `/api/admin/config` | site.toml parsed as a JSON object |
| `PUT` | `/api/admin/config` | Write site.toml from a JSON object |
| `POST` | `/api/admin/config/touch` | Touch site.toml → triggers m6-http hot reload |
| `POST` | `/api/admin/cache/flush` | Alias for config/touch |
| `POST` | `/api/admin/services/{name}/restart` | Send SIGTERM to a service via its PID file |

### 12.2. JSON blob shapes

**`GET /api/admin/perf?n=60`**
```json
{
  "history": [
    {
      "ts": "2026-03-22T10:00:00Z",
      "rps_avg": 120, "rps_peak": 340,
      "latency_p50_us": 480, "latency_p99_us": 1900,
      "cache_hits": 85, "cache_misses": 15, "cache_hit_rate": 0.85,
      "backend_errors": 0, "pool_members": 4
    }
  ]
}
```
Each entry is one "periodic stats" log event (emitted every 10 s by m6-http). Pass `?n=N` to limit history length.

**`GET /api/admin/system`**
```json
{
  "uptime_secs": 86400,
  "cpu_usage_pct": 14.2,
  "memory": { "used_bytes": 512000000, "total_bytes": 8000000000, "used_pct": 6.4 },
  "disks": [
    { "mount": "/", "used_bytes": 10000000000, "total_bytes": 50000000000, "used_pct": 20.0 }
  ]
}
```

**`GET /api/admin/bench`**
```json
{
  "targets": [
    { "id": "h1",  "label": "HTTP/1.1 127.0.0.1:8443", "addr": "127.0.0.1:8443", "proto": "h1" },
    { "id": "h2",  "label": "HTTP/2   127.0.0.1:8443", "addr": "127.0.0.1:8443", "proto": "h2" },
    { "id": "h3",  "label": "HTTP/3   127.0.0.1:8443", "addr": "127.0.0.1:8443", "proto": "h3" },
    { "id": "all", "label": "All      127.0.0.1:8443", "addr": "127.0.0.1:8443", "proto": "all" }
  ]
}
```
Targets are auto-discovered from `site.toml`'s `[server].bind` address — no manual configuration.

**`POST /api/admin/bench`**
```json
// Request body:
{ "addr": "127.0.0.1:8443", "proto": "h1", "duration_secs": 10, "concurrency": 50 }

// Response:
{ "job_id": "550e8400-e29b-41d4-a716-446655440000" }
```

**`GET /api/admin/bench/{id}`**
```json
{
  "id": "550e8400-e29b-41d4-a716-446655440000",
  "target": "127.0.0.1:8443 (h1)",
  "status": "done",
  "output": "Requests/sec: 42000\nLatency p50: 0.4ms ...",
  "started_at": 1742641200,
  "finished_at": 1742641210
}
```
`status` is `"running"`, `"done"`, or `"failed"`.

**`GET /api/admin/routes?n=10000`**
```json
{
  "sample_requests": 8234,
  "sample_window_secs": 600,
  "routes": [
    { "path": "/", "requests": 4100, "cache_hits": 3900, "cache_misses": 200, "hit_rate": 0.951, "avg_latency_us": 120 },
    { "path": "/blog", "requests": 2800, "cache_hits": 2650, "cache_misses": 150, "hit_rate": 0.946, "avg_latency_us": 180 }
  ]
}
```
Routes are sorted by request count descending. Pass `?n=N` to limit the sample window (number of "request complete" log entries examined). `sample_window_secs` is the approximate wall-clock span covered; `null` if timestamps are absent in the log.

**`GET /api/admin/logs?n=100`**
```json
{ "lines": ["log line 1", "log line 2", ...], "total_lines": 5432 }
```

**`GET /api/admin/config`**
```json
{
  "path": "/path/to/site.toml",
  "config": {
    "server": { "bind": "127.0.0.1:8444", "tls_cert": "...", "tls_key": "..." },
    "backend": [{ "name": "render-admin", "socket": "/tmp/m6/render-admin.sock" }],
    "route": [{ "path": "/api/admin/*", "backend": "render-admin", "require": "role:admin" }]
  }
}
```
The full site.toml is parsed and returned as a JSON object. All TOML types (strings, integers, arrays, nested tables) are preserved.

**`PUT /api/admin/config`**
```json
// Request body: the "config" object from GET, with your modifications
{
  "server": { "bind": "127.0.0.1:8444", ... },
  "route": [...]
}
```
The JSON object is converted back to TOML, validated, and written atomically. Returns `{ "ok": true }` on success or `{ "error": "..." }` on validation failure. The original file is never modified if validation fails.

**`POST /api/admin/services/{name}/restart`**
```json
{ "ok": true, "pid": 12345 }
```
Sends SIGTERM to the process whose PID is in the service's configured PID file.

### 12.3. Config: `render-admin.conf`

The renderer reads its settings from the m6-render user config dict:

```toml
socket           = "/tmp/m6/render-admin.sock"
log_file         = "/tmp/m6/m6-http-admin.log"
site_toml        = "site.toml"
perf_history     = 60
routes_sample    = 10000
log_tail_default = 100
bench_bin        = "m6-bench"
bench_duration_secs = 10
bench_concurrency   = 50

[[services]]
name     = "m6-http"
pid_file = "/tmp/m6/m6-http-admin.pid"

[[services]]
name     = "render-admin"
pid_file = "/tmp/m6/render-admin.pid"
```

### 12.4. Tests

44 tests covering all six modules run on every `cargo test`:

```bash
cargo test -p render-admin

# Output:
# running 44 tests
# test bench_tests::targets_discovered_from_site_toml ... ok
# test routes_tests::aggregates_per_path ... ok
# test ops_tests::config_write_json_blob_round_trips ... ok
# ... (44 total)
# test result: ok. 44 passed; 0 failed
```

Tests cover: perf log parsing (field completeness, N-limiting), per-route aggregation (hit rate, latency, sample window, sort order), system metrics shape, bench target discovery, async job lifecycle, log tail content, config read/write/touch round-trip, null rejection, and service restart.

### 12.5. Functional tests

`test.sh` tests against a live server:

```bash
# Terminal 1
./dev.sh

# Terminal 2
./test.sh
```

Tests: unauthenticated 401, all GET endpoints return correct JSON keys, config touch/flush returns 200, unknown service returns 404.
