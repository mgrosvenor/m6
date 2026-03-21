# m6 — User Guide

Ten examples, each building on the last. Clone the examples repo to follow along:

```bash
git clone https://github.com/m6/m6-examples
cd m6-examples
```

Prerequisites:

```bash
# Install m6 binaries
cargo install m6-http m6-html m6-file m6-auth

# TLS for development (run once)
mkcert -install && mkcert localhost 127.0.0.1
# Outputs localhost.pem and localhost-key.pem
```

---

## Example 01 — Static Site

`examples/01-static/`

The base case. Templates, assets, and hand-authored JSON. No build step, no custom renderers. Three processes, one config each.

```bash
cd examples/01-static
./dev.sh
# open https://localhost:8443
```

### Site directory

```
01-static/
├── site.toml
├── configs/
│   ├── m6-html.conf
│   └── m6-file.conf
├── templates/
│   ├── base.html
│   ├── home.html
│   ├── page.html
│   └── error.html
├── assets/
│   └── style.css
└── data/
    └── site.json
```

### `site.toml`

```toml
[site]
name   = "My Site"
domain = "localhost"

[server]
bind     = "127.0.0.1:8443"
tls_cert = "../../localhost.pem"
tls_key  = "../../localhost-key.pem"

[errors]
mode = "internal"

[log]
level  = "info"
format = "text"

[[backend]]
name    = "m6-html"
sockets = "/run/m6/m6-html-*.sock"

[[backend]]
name    = "m6-file"
sockets = "/run/m6/m6-file-*.sock"

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

### `configs/m6-html.conf`

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

### `configs/m6-file.conf`

```toml
[[route]]
path = "/assets/{relpath}"
root = "assets/"
```

### `data/site.json`

```json
{
  "nav": [
    { "label": "Home",  "path": "/" },
    { "label": "About", "path": "/about" }
  ]
}
```

### `templates/base.html`

```html
<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <title>{% block title %}{{ site_name }}{% endblock %}</title>
  <link rel="stylesheet" href="/assets/style.css">
</head>
<body>
  <nav>
    {% for item in nav %}
      <a href="{{ item.path }}">{{ item.label }}</a>
    {% endfor %}
  </nav>
  <main>{% block content %}{% endblock %}</main>
</body>
</html>
```

### `templates/error.html`

```html
{% extends "base.html" %}
{% block title %}{{ query.status }} · {{ site_name }}{% endblock %}
{% block content %}
  <h1>{{ query.status }} — {{ query.reason }}</h1>
  <p>The page <code>{{ query.path }}</code> could not be found.</p>
  <a href="/">Return home</a>
{% endblock %}
```

### `dev.sh`

```bash
#!/bin/bash
set -e
SITE="$(cd "$(dirname "$0")" && pwd)"
trap 'kill $(jobs -p) 2>/dev/null' EXIT

m6-html "$SITE" "$SITE/configs/m6-html.conf" &
m6-file "$SITE" "$SITE/configs/m6-file.conf" &
m6-http "$SITE" &

echo "Running at https://localhost:8443"
wait
```

---

## Example 02 — Blog

`examples/02-blog/`

Adds a blog. Markdown source files in `content/posts/` are processed by `m6-md` into a single JSON file at `data/posts.json`. m6-html serves the blog from that file. No per-post JSON files, no `[[route_group]]`.

```bash
cd examples/02-blog
cargo install m6-md          # separate project, one-time install

# Generate posts.json from Markdown source, then start the site
m6-md content/posts/ --output data/posts.json
./dev.sh
```

To update content, re-run `m6-md content/posts/ --output data/posts.json` then restart or let m6-http pick up the changed file via inotify.

### What's new

`content/posts/` contains Markdown source files with TOML frontmatter. m6-md produces `data/posts.json` — a single JSON object with a `documents` array containing every post's metadata and rendered HTML body. m6 has no knowledge of Markdown.

### Source file format

```
+++
title   = "Hello World"
date    = "2024-01-15"
summary = "A brief introduction to the blog."
tags    = ["rust", "web"]
+++

Post body in **Markdown**. Tables, footnotes, and strikethrough supported.
```

Any frontmatter key beyond `title` and `date` passes through to the JSON as-is.

### `data/posts.json` (produced by m6-md)

```json
{
  "documents": [
    {
      "stem":    "hello-world",
      "path":    "/hello-world",
      "title":   "Hello World",
      "date":    "2024-01-15",
      "body":    "<p>Post body in <strong>Markdown</strong>...</p>",
      "summary": "A brief introduction to the blog.",
      "tags":    ["rust", "web"]
    }
  ]
}
```

Sorted by `date` descending. `body` is fully rendered HTML.

### New routes in `site.toml`

```toml
[[route]]
path    = "/blog"
backend = "m6-html"

[[route]]
path    = "/blog/{stem}"
backend = "m6-html"
```

No `[[route_group]]`. The routes are fixed patterns — m6-html handles any `/blog/{stem}` and looks up the matching post in the params.

### New routes in `configs/m6-html.conf`

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

Both routes load the same file. The index template iterates `documents`. The post template filters by `stem`.

### `templates/post-index.html`

```html
{% extends "base.html" %}
{% block title %}Blog · {{ site_name }}{% endblock %}
{% block content %}
  <h1>Blog</h1>
  {% for doc in documents %}
    <article>
      <h2><a href="/blog/{{ doc.stem }}">{{ doc.title }}</a></h2>
      <time>{{ doc.date }}</time>
      {% if doc.summary %}<p>{{ doc.summary }}</p>{% endif %}
    </article>
  {% endfor %}
{% endblock %}
```

### `templates/post.html`

```html
{% extends "base.html" %}
{% set post = documents | filter(attribute="stem", value=stem) | first %}
{% block title %}{{ post.title }} · {{ site_name }}{% endblock %}
{% block content %}
  <article>
    <h1>{{ post.title }}</h1>
    <time>{{ post.date }}</time>
    {{ post.body | safe }}
  </article>
{% endblock %}
```

`stem` is a built-in key provided by m6-html — the `{stem}` capture from the matched URL.

## Example 03 — Contact Form

`examples/03-contact/`

Adds `render-contact` — a custom renderer that handles GET (renders the form) and POST (sends email via SMTP).

```bash
cd examples/03-contact
cargo build --release -p render-contact
./dev.sh
```

### `render-contact/Cargo.toml`

```toml
[package]
name    = "render-contact"
version = "0.1.0"
edition = "2021"

[dependencies]
m6-render = { git = "https://github.com/m6/m6", tag = "v0.1.0", features = ["smtp"] }
serde_json = "1"
```

### `render-contact/src/main.rs`

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

### `configs/render-contact.conf`

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

### `templates/contact.html`

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

### New in `site.toml`

```toml
[[backend]]
name    = "render-contact"
sockets = "/run/m6/render-contact-*.sock"

[[route]]
path    = "/contact"
backend = "render-contact"
```

### Updated `dev.sh`

```bash
./target/release/render-contact "$SITE" "$SITE/configs/render-contact.conf" &
```

---

## Example 04 — Login and Protected Pages

`examples/04-auth/`

Adds m6-auth. Members-only pages behind login. The JWT is stored in an HttpOnly cookie — the browser sends it automatically on every request, JavaScript cannot read it.

```bash
cd examples/04-auth
./setup.sh   # generates keys, creates first admin user
./dev.sh
# login at https://localhost:8443/login
```

### Key generation and bootstrap (`setup.sh`)

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

### `configs/m6-auth.conf`

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

### New in `site.toml`

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

### `templates/login.html`

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

### `templates/members.html`

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

## Example 05 — CMS Blog

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

### Site directory

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

### `site.toml`

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

### `render-cms/Cargo.toml`

```toml
[package]
name    = "render-cms"
version = "0.1.0"
edition = "2021"

[dependencies]
m6-render  = { git = "https://github.com/m6/m6", tag = "v0.1.0", features = ["auth", "disk"] }
serde_json = "1"
```

### `render-cms/src/main.rs`

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

### The publish flow

1. Editor saves draft → `POST /api/drafts` or `PATCH /api/drafts/{stem}` writes `content/drafts/{stem}.json`
2. Editor clicks **Publish** → `POST /api/publish/{stem}`:
   - render-cms writes `content/posts/{stem}.json` — inotify fires, m6-http evicts `/blog/{stem}` from cache
   - render-cms writes updated `content/posts/_index.json` — inotify fires, m6-http evicts `/blog`
   - render-cms touches `site.toml` — m6-http reloads route table, re-expands `[[route_group]]` glob, `/blog/{stem}` route becomes live
   - render-cms deletes `content/drafts/{stem}.json`
3. Browser redirects to `/blog/{stem}`
4. m6-http: route now live, cache miss → m6-html reads `content/posts/{stem}.json`, renders → m6-http caches
5. Every subsequent request to `/blog/{stem}`: cache hit — RAM only

### Performance profile

| Request type | Latency |
|---|---|
| Public post (cached) | < 1ms — RAM only |
| Public post (cache miss) | ~5ms — m6-html reads `content/posts/{stem}.json`, renders, caches |
| CMS dashboard | ~10ms — JWT verified locally, render-cms reads posts directory + drafts directory |
| Publish | ~5ms — one atomic file write, one inotify event |

### `dev.sh`

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

## Example 06 — Production with systemd

`examples/06-systemd/`

Takes the CMS blog from example 05 and runs it properly under systemd. Every process is a systemd unit. Ordering, restart policy, log capture, socket directory permissions, deploy workflow, and horizontal scaling are all covered.

The unit files live in `examples/06-systemd/systemd/` and are copied into place during setup.

---

### Server preparation

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

---

### Deploy site directory

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

### Unit files

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

### Install and start

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

### Verify

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

### Logs

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

### Deploy updates

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

# TLS cert renewal (certbot handles automatically, m6-http reloads via inotify)
certbot renew
# m6-http detects cert file change and reloads TLS context — no restart needed
```

---

### Horizontal scaling

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

### Crash recovery

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

## Example 07 — Dev to Production

`examples/07-dev-to-production/`

The complete workflow for taking a site from local development to a production server. The same site directory runs in both environments — the system config provides the three keys that differ.

---

### The split

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

### Dev script

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

### First-time server setup

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

### Systemd unit — m6-http

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

### Deploy

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

### Dev vs production at a glance

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

### Checking what's in effect

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

### Common mistakes

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
rsync -av ./ user@server:/var/www/my-blog/

# Right — always exclude server-managed state
rsync -av --exclude 'content/drafts/' --exclude 'data/posts.json' --exclude 'data/auth.db' \
  ./ user@server:/var/www/my-blog/
```

**Editing `site.toml` on the server directly:**
```bash
# Wrong — next deploy overwrites it
ssh server vim /var/www/my-blog/site.toml

# Right — edit locally and deploy
# Server-specific overrides belong in /etc/m6/my-blog.toml
```

---

## Example 08 — Log Viewer

`examples/08-logviewer/`

A real-time log viewer built into the site. m6-http writes JSON-format logs to disk. A browser UI polls a tail endpoint served by m6-file and renders incoming lines with filtering and search. Demonstrates `format = "json"` logging and m6-file's byte-range tail mode.

```bash
cd examples/08-logviewer
./dev.sh
# open https://localhost:8443/logs
```

---

## Example 09 — Global Deployment

`examples/09-global-deployment/`

A multi-region Vultr deployment: an origin node runs the full m6 stack, and multiple cache nodes around the world run m6-http in pure proxy mode. A WireGuard mesh connects all nodes. Cache invalidation propagates from origin to all cache nodes on publish. Demonstrates horizontal scale-out across regions without a CDN.

```bash
cd examples/09-global-deployment
# See setup-origin.sh and setup-cache-node.sh
```

---

## Example 10 — API Tokens

`examples/10-api-tokens/`

Adds long-lived API tokens for scripts and services. Unlike session cookies (15-minute TTL, browser only), an API token is a JWT passed as `Authorization: Bearer <token>` and can be valid for days or months. m6-http verifies Bearer tokens the same way it verifies session cookies — same EC key, same `require` rules. No special server configuration is needed.

```bash
cd examples/10-api-tokens
./dev.sh
```

### When to use API tokens

| | Session cookie | API token |
|---|---|---|
| Caller | Browser | Script / service / CI |
| Lifetime | 15 min (refreshes to 30 days) | Configurable (default 30 days) |
| Transport | `Cookie: session=...` | `Authorization: Bearer ...` |
| Issuance | `POST /auth/login` | `m6-auth-cli token create` |
| Revocation | `POST /auth/logout` | `m6-auth-cli token revoke` |

### Setup

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

### `site.toml` — protecting an API endpoint

```toml
# API endpoint — requires a valid Bearer token with role 'api'
[[route]]
path    = "/api/data"
backend = "m6-html"
require = "role:api"
```

`require = "role:api"` works for both session cookies and Bearer tokens. A browser user logged in with the `api` role can also access the route. For endpoints that should be machine-only, use a dedicated role name like `api` and never assign it to regular user accounts.

### Token management

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

### Template variables in protected routes

When a request carries a valid Bearer token, m6-html templates can access the caller's identity via the `auth` object — the same object available after browser login:

```html
<p>Caller: {{ auth.username }}</p>
<p>Roles:  {{ auth.roles | join(sep=", ") }}</p>
```

### Functional tests

`test.sh` in the example directory runs against a live server:

```bash
# In one terminal
./dev.sh

# In another
./test.sh
```

Tests cover: public access, 401 on unauthenticated access, token create/list/revoke, role enforcement, and custom error pages.

