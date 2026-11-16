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
13. [Summary](#13-summary)

## 1. Before you start

The examples need two checkouts, one certificate tool and a release build of m6.
This section installs them and points at the full-stack check, so a later failure is a real answer about an example.
The release build puts every m6 binary on the path, including `m6-md`, which Example 02 uses.

Rust 1.89 or newer provides the compiler and Cargo.
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
Install `mkcert` once, and the local `dev.sh` scripts run `examples/dev-keys.sh`, which writes the development certificate to `/tmp/m6/dev` on first run.
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
level = "info"

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
Each query parameter is available under `query`, and at the top level unless a path capture has the same name.

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
[`m6-core-reference.md`](m6-core-reference.md) §4.5 owns template rendering.

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
`dev.sh` builds the m6 binaries and runs m6-md once to write `data/posts.json`.

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

[`m6-md.md`](m6-md.md) owns the frontmatter rules, the command line, the output format and `not_found()`.
[`m6-core-reference.md`](m6-core-reference.md) owns the template engine and its m6 filters at §4.5, and the built-in keys at §4.6.
The `sort`, `reverse`, `filter`, `length` and `first` filters are Tera's standard filters.

Example 02 adds a content pipeline in front of the same three processes.
Example 03 adds a fourth process, a custom renderer that handles a contact form and sends email.

## 4. Example 03, contact form

Example 03 adds a contact form to the blog.
A fourth process, `render-contact`, renders the form and sends the submitted message as *SMTP* (Simple Mail Transfer Protocol) email.
The example shows the shape of a custom renderer: a small Rust crate that owns one route end to end.

Run the example from `examples/03-contact`.
The workspace build in section 1 already built `render-contact`, so `dev.sh` starts it directly.

```sh
cd examples/03-contact
./dev.sh
```

### 4.1. `render-contact/Cargo.toml`

`render-contact` is a Rust crate in the m6-examples workspace.
Its manifest depends on m6-core by the relative path `../../../../m6/m6-core`, with the `email` feature that supplies the *SMTP* client a service's loop drives.
`anyhow` and `serde_json` handle errors and JSON.

```toml
[package]
name    = "render-contact"
version = "0.1.0"
edition = "2021"

[dependencies]
m6-core    = { path = "../../../../m6/m6-core", features = ["email"] }
anyhow     = "1"
serde_json = "1"
```

The relative path is why the m6 and m6-examples checkouts must sit side by side.

### 4.2. `render-contact/src/main.rs`

`main.rs` has two functions plus `main`.
`load_state` builds the relay and its certificate store from the config.
`message` builds one `Message`.
`main` loads the site, calls `load_state` at start, and runs the loop with one `Smtp` slot per connection slot.

```rust
use std::sync::Arc;
use std::time::Duration;

use m6_core::email::{lettre, Relay, RelayConfig, Smtp};
use m6_core::prelude::*;
use m6_core::rustls::ClientConfig;

struct Global {
    relay: Relay,
    from:  String,
    to:    String,
}

fn load_state(site: &Site) -> Result<Global> {
    let config = site.user_config();
    let host = config["smtp"]["host"].as_str()
        .ok_or_else(|| Error::Other(anyhow::anyhow!("smtp.host missing")))?;
    let port = config["smtp"]["port"].as_u64()
        .ok_or_else(|| Error::Other(anyhow::anyhow!("smtp.port missing")))? as u16;
    let username = config["smtp"]["username"].as_str()
        .ok_or_else(|| Error::Other(anyhow::anyhow!("smtp.username missing")))?;
    let password = config["smtp"]["password"].as_str()
        .ok_or_else(|| Error::Other(anyhow::anyhow!("smtp.password missing")))?;
    let starttls = config["smtp"]["starttls"].as_bool().unwrap_or(true);
    let from = config["smtp"]["from"].as_str()
        .ok_or_else(|| Error::Other(anyhow::anyhow!("smtp.from missing")))?.to_string();
    let to = config["smtp"]["to"].as_str()
        .ok_or_else(|| Error::Other(anyhow::anyhow!("smtp.to missing")))?.to_string();

    let tls = if starttls {
        Some(Arc::new(
            ClientConfig::builder()
                .with_root_certificates(client::system_roots()
                    .map_err(|e| Error::Other(anyhow::anyhow!("{e}")))?)
                .with_no_client_auth(),
        ))
    } else {
        None
    };
    let credentials = (!username.is_empty())
        .then(|| (username.to_string(), password.to_string()));
    let relay = Relay::new(&RelayConfig {
        host: host.to_string(),
        port,
        starttls: tls,
        credentials,
        timeout: Duration::from_secs(20),
    }).map_err(|e| Error::Other(anyhow::anyhow!("{e}")))?;

    Ok(Global { relay, from, to })
}

fn message(global: &Global, name: &str, email: &str, text: &str) -> Result<Message> {
    let from_addr: lettre::Address = global.from.parse()
        .map_err(|e: lettre::address::AddressError| Error::Other(anyhow::anyhow!("{e}")))?;
    let to_addr: lettre::Address = global.to.parse()
        .map_err(|e: lettre::address::AddressError| Error::Other(anyhow::anyhow!("{e}")))?;

    Message::builder()
        .from(lettre::message::Mailbox::new(None, from_addr))
        .to(lettre::message::Mailbox::new(None, to_addr))
        .subject(format!("Contact from {}", name))
        .body(format!("From: {} <{}>\n\n{}", name, email, text))
        .map_err(|e| Error::Other(anyhow::anyhow!("{e}")))
}
```

`load_state` runs once at startup, and the loop passes its value to each request arm.
The POST arm builds one message and calls `smtp.start(&state.relay, &msg)` on its connection's slot.
The connection's state becomes `Sending`, and the loop registers `smtp.fd()` with `smtp.interest()` under the outgoing token and puts `smtp.deadline()` in the outgoing `Timeouts`.
The client's own token stays `READABLE` while it waits.
An outgoing event for a slot whose state is no longer `Sending` is ignored, because the client can leave in the same poll batch.
When the outgoing token fires, `smtp.drive()` returns `Ok(None)` and the loop reregisters, or `Ok(Some(()))` and the handler renders `templates/contact.html` with `sent: true`, or `Err` and the handler answers with the cause.
The outgoing deadline passing first answers 504.
A client that closes while `Sending` frees its slot, which closes the relay connection.

### 4.3. `configs/render-contact.conf`

`render-contact.conf` loads `data/site.json` into every template, names the secrets file and declares the one route.
The route renders `templates/contact.html` with `cache = "no-store"`, so m6-http never caches the form or its response.

```toml
global_params = ["data/site.json"]
secrets_file  = "/etc/m6/render-contact.toml"

[[route]]
path     = "/contact"
template = "templates/contact.html"
params   = []
cache    = "no-store"

[smtp]
host     = "localhost"
port     = 1025
# The local development relay on localhost speaks plain SMTP. Production relays take STARTTLS on port 587: delete this line.
starttls = false
username = ""
password = ""
from     = "noreply@example.com"
to       = "owner@example.com"
```

The `[smtp]` values are development defaults.
`starttls = false` is what lets the example talk to the local plain relay: m6-core refuses plain SMTP to any host but `localhost`, `127.0.0.1` or `::1`, so delete the line for a relay elsewhere.
In production the values move to the file named by `secrets_file`, and the `[smtp]` section is removed from this file.

A key set in both files is refused at startup, and a missing secrets file is ignored, so the example runs with no setup.
In production, deliver the secrets file with `LoadCredential=secrets.toml:/etc/m6/render-contact.toml` and set `secrets_file = "/run/credentials/render-contact.service/secrets.toml"`.

### 4.4. `templates/contact.html`

`templates/contact.html` shows one of two states.
When `sent` is true it shows a thank-you message.
Otherwise it shows the form, which posts the three fields back to `/contact`.

```html
{% extends "templates/base.html" %}
{% block title %}Contact · {{ site_name }}{% endblock %}
{% block content %}
  <div class="form-card">
    <h1>Contact</h1>
    {% if sent %}
      <p>Message sent. Thanks, <strong>{{ name }}</strong>!</p>
    {% else %}
      <form method="post" action="/contact">
        <label>Name
          <input type="text" name="name" required autofocus>
        </label>
        <label>Email
          <input type="email" name="email" required>
        </label>
        <label>Message
          <textarea name="message" required></textarea>
        </label>
        <button type="submit">Send message</button>
      </form>
    {% endif %}
  </div>
{% endblock %}
```

The form's field names match the ones `handle_post` reads.
`extends` names `templates/base.html`, the layout every page shares.

### 4.5. New in `site.toml`

`site.toml` gains one backend and one route.
The backend names the `render-contact` process and its socket, and the route sends `/contact` to it.

```toml
[[backend]]
name    = "render-contact"
sockets = "/tmp/m6/render-contact.sock"

[[route]]
path    = "/contact"
backend = "render-contact"
```

The `sockets` value is one exact path, because the example runs one copy of the renderer.

### 4.6. Updated `dev.sh`

`dev.sh` starts the renderer beside the other processes.
`M6_SOCKET_OVERRIDE` points the renderer's socket at the path `site.toml` names, and the command uses the binary from the workspace build.

```sh
M6_SOCKET_OVERRIDE=/tmp/m6/render-contact.sock \
    "$EXAMPLES/target/release/render-contact" "$SITE" "$SITE/configs/render-contact.conf" &
```

`$EXAMPLES` is the m6-examples checkout root.

[`m6-app-anatomy.md`](m6-app-anatomy.md) owns the custom renderer guide.
[`m6-app-anatomy.md`](m6-app-anatomy.md) §4 owns the renderer config keys, §5.3 owns where state lives (a plain value in `main`), and [`m6-site-toml.md`](m6-site-toml.md) §3.2 owns `secrets_file`.
[`m6-site-toml.md`](m6-site-toml.md) owns the `[[backend]]` and `[[route]]` keys.

Example 03 adds one custom renderer and one form route.
Example 04 adds login and protects a members page behind it.

## 5. Example 04, login and protected pages

Example 04 adds authentication to the contact-form site.
The `m6-auth` service verifies logins and issues *JWT* (JSON Web Token) tokens, and `m6-http` enforces access to protected routes.
The example shows the division of work: one service issues the tokens, and `m6-http` enforces the routes.

Run the example from `examples/04-auth`.
`setup.sh` generates the signing keys and creates the admin user, then `dev.sh` starts the stack.

```sh
cd examples/04-auth
./setup.sh
./dev.sh
```

Open `https://localhost:8443/login` and log in as `admin` with password `admin`.

### 5.1. Key generation and bootstrap (`setup.sh`)

`setup.sh` generates the signing key pair and creates the first admin user.
The keys are *PKCS#8* (Public-Key Cryptography Standards #8), the format `m6-auth-server` accepts.

```bash
#!/bin/bash
set -e
SITE="$(cd "$(dirname "$0")" && pwd)"

# Development keys, written by examples/dev-keys.sh into /tmp/m6/dev.
"$SITE/../dev-keys.sh"

m6-auth-cli "$SITE/configs/m6-auth.conf" user add admin --role admin --password admin
```

`examples/dev-keys.sh` writes the signing key pair to `/tmp/m6/dev`.
`user add admin --role admin --password admin` creates the admin user with the role `admin` and the password `admin`, and it creates the auth database on first run.

### 5.2. `configs/m6-auth.conf`

`m6-auth.conf` configures the auth service: the database path, the two token lifetimes, the issuer and the key paths.

```toml
[storage]
path = "/tmp/m6/04-auth/auth.db"

[tokens]
access_ttl  = 900
refresh_ttl = 2592000
issuer      = "localhost"

[keys]
private_key = "/tmp/m6/dev/auth.pem"
public_key  = "/tmp/m6/dev/auth.pub"
```

`access_ttl` sets the access token lifetime to 900 seconds, and `refresh_ttl` sets the refresh token lifetime to 2592000 seconds.
`issuer` sets the `iss` claim the service writes into tokens, and the two key paths name the files `examples/dev-keys.sh` wrote.

### 5.3. New config

Example 04 changes two config files.
The system config gains the `[auth]` block that points `m6-http` at the auth service and its public key.
`site.toml` gains the `m6-auth` backend and the login, logout, refresh and members routes.

The `[auth]` block lives in `configs/system-dev.toml`, the system config `m6-http` reads:

```toml
[auth]
backend    = "m6-auth-check"
public_key = "/tmp/m6/dev/auth.pub"
```

The backends and routes live in `site.toml`:

```toml
[[backend]]
name    = "m6-auth"
sockets = "/tmp/m6/m6-auth*.sock"

[[backend]]
name    = "m6-auth-check"
sockets = "/tmp/m6/check-m6-auth*.sock"

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

`/login` renders the login form from `m6-html`, the three `/auth/` routes reach the auth service, and the two `/members` routes are the protected pages.

### 5.4. `templates/login.html`

`templates/login.html` renders the login form and the failure message.
The hidden `next` field carries the page the user came from.

```html
{% extends "templates/base.html" %}
{% block title %}Login · {{ site_name }}{% endblock %}
{% block content %}
  <div class="form-card">
    <h1>Login</h1>
    {% if query.error is defined and query.error %}
      <p class="error">Invalid username or password.</p>
    {% endif %}
    <form method="post" action="/auth/login">
      <input type="hidden" name="next" value="{{ query.next | default(value='') }}">
      <label>Username
        <input type="text" name="username" required autofocus>
      </label>
      <label>Password
        <input type="password" name="password" required>
      </label>
      <button type="submit">Login</button>
    </form>
  </div>
{% endblock %}
```

The form posts to `/auth/login`, where `m6-auth` verifies the credentials and sets the session and refresh cookies.

### 5.5. `templates/members.html`

`templates/members.html` is the protected page, and its only action is the logout form.

```html
{% extends "templates/base.html" %}
{% block title %}Members · {{ site_name }}{% endblock %}
{% block content %}
  <h1>Members Area</h1>
  <p>Welcome. This page is only visible after login.</p>
  <form method="post" action="/auth/logout">
    <button type="submit">Logout</button>
  </form>
{% endblock %}
```

The form posts to `/auth/logout`.

[`m6-auth.md`](m6-auth.md) owns the login, refresh and logout responses and the cookie attributes.
[`m6-auth-cli.md`](m6-auth-cli.md) owns the `user add` and group commands.
[`m6-site-toml.md`](m6-site-toml.md) owns the `[auth]` and `require` keys.

Example 04 adds login and one protected page.
Example 05 builds a CMS blog with a publish flow on top of the same auth.

## 6. Example 05, CMS blog

Example 05 builds a *CMS* (Content Management System) blog on the auth from Example 04.
It adds a custom renderer that owns the CMS, and it is the first example to run the whole stack end to end.
The public blog is cached at the edge, and the CMS routes are protected and never cached.

Run the example from `examples/05-cms`.
`setup.sh` generates the signing keys and creates the admin user, then `dev.sh` builds and starts the stack.

```sh
cd examples/05-cms
./setup.sh
./dev.sh
```

The CMS is at `https://localhost:8443/cms`, and the public blog is at `https://localhost:8443/blog`.
`./test.sh` then checks the running stack over the wire.

### 6.1. Site directory

Example 05 adds the CMS renderer, a drafts directory and a test script to the Example 04 layout.

- `render-cms/` is the CMS crate
- `content/drafts/` holds the JSON drafts
- `configs/render-cms.conf` marks the renderer's routes uncacheable
- `test.sh` checks the running stack end to end

Everything else carries over: `site.toml`, the other four config files, `templates/`, `assets/`, `content/posts/`, `data/`, `setup.sh` and `dev.sh`.

`content/posts/` holds the Markdown sources and the published *JSON* (JavaScript Object Notation) posts, and `content/drafts/` holds the JSON drafts.
`data/posts.json` is the merged index both sources write into.

### 6.2. `site.toml`

`site.toml` declares the four backends and the routes, and `configs/system-dev.toml` holds the node's bind address, *TLS* (Transport Layer Security) paths and auth key.
Example 05 adds the `render-cms` backend, the CMS routes and the method list.

The `render-cms` backend and the routes that reach it:

```toml
[[backend]]
name    = "render-cms"
sockets = "/tmp/m6/render-cms*.sock"

[[route]]
path    = "/contact"
backend = "render-cms"

[[route]]
path    = "/cms"
backend = "render-cms"
require = "group:editors"

[[route]]
path    = "/api/publish/{stem}"
backend = "render-cms"
require = "group:editors"
```

The full CMS route set is `/cms`, `/cms/new`, `/cms/edit/{stem}`, `/api/drafts`, `/api/drafts/{stem}`, `/api/publish/{stem}`, `/api/unpublish/{stem}` and `/api/posts/{stem}`, all behind the `editors` group.

The public blog and asset routes:

```toml
[[route]]
path    = "/blog/{stem}"
backend = "m6-html"

[[route]]
path    = "/assets/{*relpath}"
backend = "m6-file"
```

render-cms registers PATCH and DELETE routes, so `allowed_methods` names them:

```toml
[server]
allowed_methods = ["GET", "HEAD", "POST", "PATCH", "DELETE"]
```

`configs/system-dev.toml` holds the node's own settings:

```toml
[server]
bind     = "127.0.0.1:8443"
tls_cert = "/tmp/m6/dev/dev.pem"
tls_key  = "/tmp/m6/dev/dev-key.pem"

[auth]
backend    = "m6-auth-check"
public_key = "/tmp/m6/dev/auth.pub"

[node]
name = "dev"
```

### 6.3. `render-cms/Cargo.toml`

`render-cms` is a Rust crate in the m6-examples workspace.
Its manifest depends on m6-core by the relative path `../../../../m6/m6-core`, on `serde_json` for JSON, on `comrak` to render Markdown to *HTML* (HyperText Markup Language), and on `base64` for the editor transport.

```toml
[package]
name    = "render-cms"
version = "0.1.0"
edition = "2021"

[dependencies]
m6-core    = { path = "../../../../m6/m6-core" }
serde_json = "1"
comrak     = { version = "0.21", default-features = false }
base64     = "0.22"
```

### 6.4. `render-cms/src/main.rs`

The routes are `CodeRoute` values, and `configs/render-cms.conf` marks the routes it lists `cache = "no-store"`.
The renderer also owns the `/contact` form.

```rust
const ROUTES: &[CodeRoute] = &[
    CodeRoute { pattern: "/contact",               method: RouteMethod::Get },
    CodeRoute { pattern: "/contact",               method: RouteMethod::Post },
    CodeRoute { pattern: "/cms",                   method: RouteMethod::Get },
    CodeRoute { pattern: "/cms/new",               method: RouteMethod::Get },
    CodeRoute { pattern: "/cms/edit/{stem}",       method: RouteMethod::Get },
    CodeRoute { pattern: "/api/drafts",            method: RouteMethod::Post },
    CodeRoute { pattern: "/api/drafts/{stem}",     method: RouteMethod::Patch },
    CodeRoute { pattern: "/api/publish/{stem}",    method: RouteMethod::Post },
    CodeRoute { pattern: "/api/unpublish/{stem}",  method: RouteMethod::Post },
    CodeRoute { pattern: "/api/drafts/{stem}",     method: RouteMethod::Delete },
    CodeRoute { pattern: "/api/posts/{stem}",      method: RouteMethod::Delete },
];

// Part E's `main` runs the loop from `m6-app-anatomy.md` section 2, with one
// `Handled::Code` arm per route above, in the same order. Its start:
let site = Site::load("render-cms", ROUTES, &[], &TeraFactory);
```

The snippet stops at start: part E adds the loop and its `match` arms.
The handlers use the file helpers m6-core puts on `Request`.
`handle_create_draft` writes `content/drafts/{stem}.json`.
`handle_publish` reads that draft, renders its Markdown body to HTML, writes `content/posts/{stem}.json`, rebuilds the index and deletes the draft.
`handle_unpublish` moves the post back to `content/drafts/{stem}.json` and rebuilds the index.
`handle_edit` opens a draft first, then a published CMS post, then a Markdown post from the index, so an editor can edit any post.

### 6.5. The publish flow

A publish moves a draft to the public blog in five steps.

1. The editor saves a draft, and `POST /api/drafts` or `PATCH /api/drafts/{stem}` writes `content/drafts/{stem}.json`.
2. The editor publishes, and `POST /api/publish/{stem}` reads that draft.
3. render-cms renders the Markdown body to HTML, writes `content/posts/{stem}.json` and merges it into `data/posts.json`.
4. render-cms restarts, or m6-http restarts, so the change is read at start.
5. The next request for `/blog/{stem}` is a cache miss, m6-html renders the post from the updated index, and m6-http caches it.

Unpublishing reverses the flow.
`POST /api/unpublish/{stem}` moves the post back to `content/drafts/{stem}.json` and rebuilds the index.
A post that came from Markdown survives every index rebuild, because `update_index` keeps the entries the CMS does not own.

### 6.6. Caching

The edge caches only the public, unauthenticated routes.
The CMS routes sit behind `require = "group:editors"`, so m6-http never caches them.
The contact, `/login` and `/_errors` routes are marked `cache = "no-store"`.

### 6.7. `dev.sh` and `test.sh`

`dev.sh` builds m6 and render-cms, writes the TLS and auth keys, creates the admin user, generates `data/posts.json` and starts the stack.
The stack is m6-html, m6-file, m6-auth-server, render-cms and m6-http.

`test.sh` checks the running stack over the wire.
It has 15 sections, numbered 0 to 14, and every check has one expected answer.
The summary line prints the pass count, the fail count and the total, and the script deletes its own draft before it ends.

[`m6-core-reference.md`](m6-core-reference.md) owns the `Request` file helpers and the `CodeRoute` values a service passes to `Site::load`.
[`m6-md.md`](m6-md.md) owns the Markdown conversion.
[`m6-site-toml.md`](m6-site-toml.md) owns the `[[route]]`, `require` and `allowed_methods` keys.
[`m6-http.md`](m6-http.md) owns the cache and its clearing rules.
[`m6-auth.md`](m6-auth.md) owns the login, refresh and logout responses.

Example 05 completes the full stack and adds the end-to-end suite that section 1 points to.
Example 06 runs the same site under systemd for production.

## 7. Example 06, production with systemd

Example 06 runs the Example 05 CMS blog under systemd.
The example holds only the unit files in `examples/06-systemd/systemd/`.
It reuses the Example 05 site directory, with a production `site.toml` and a production system config written on the server.
This section prepares the server, deploys the site directory and writes the units.
Sections 7.5 to 7.12 install them, verify the stack and cover logs, scaling and recovery.

### 7.1. Server preparation

The server needs a dedicated user, a site directory, the auth signing keys and a *TLS* (Transport Layer Security) certificate.
Create the user and the two directories first:

```bash
useradd --system --no-create-home --shell /usr/sbin/nologin m6

mkdir -p /var/www/my-blog
chown m6:m6 /var/www/my-blog

mkdir -p /etc/m6
```

Generate the auth keys with `openssl genpkey`.
It writes the *PKCS#8* (Public-Key Cryptography Standards #8) form that m6-auth-server accepts.
`openssl ecparam -genkey` writes the *SEC1* form, which m6-auth-server refuses, so do not use it.
`ec_param_enc:named_curve` keeps the named curve form the token signer accepts.

```bash
openssl genpkey -algorithm EC -pkeyopt ec_paramgen_curve:P-256 -pkeyopt ec_param_enc:named_curve -out /etc/m6/auth.pem
openssl pkey -in /etc/m6/auth.pem -pubout -out /etc/m6/auth.pub
chmod 600   /etc/m6/auth.pem
chmod 600   /etc/m6/auth.pub
```

systemd reads a credential as root, so both key files stay root's.

Write `/etc/m6/m6-auth.conf` for the login unit and `/etc/m6/check/m6-auth.conf` for the check unit.
Both are owned by root and readable by the m6 group, and they are identical except for the unit name in the two `[keys]` paths.
Both files have the stem `m6-auth`, so both derive `/run/m6/m6-auth.sock` as the primary path.

```bash
cat > /etc/m6/m6-auth.conf << 'CONF'
[storage]
path = "/var/lib/m6-auth/auth.db"

[tokens]
issuer = "example.com"

[keys]
private_key = "/run/credentials/m6-auth-login.service/auth.pem"
public_key  = "/run/credentials/m6-auth-login.service/auth.pub"
CONF
mkdir -p /etc/m6/check
sed 's/m6-auth-login\.service/m6-auth-check.service/g' \
  /etc/m6/m6-auth.conf > /etc/m6/check/m6-auth.conf
chown root:m6 /etc/m6/m6-auth.conf /etc/m6/check/m6-auth.conf
chmod 640 /etc/m6/m6-auth.conf /etc/m6/check/m6-auth.conf
```

The login unit binds `/run/m6/m6-auth.sock`.
The check unit derives `/run/m6/check-m6-auth.sock` beside that primary path and binds only the check socket.

Obtain the certificate with certbot.
It writes `fullchain.pem` and `privkey.pem` under `/etc/letsencrypt/live/example.com/`, the paths the m6-http unit's `LoadCredential` lines deliver.

```bash
certbot certonly --standalone -d example.com
```

The deploy hook is `/etc/letsencrypt/renewal-hooks/deploy/m6-http-restart.sh`:

```sh
#!/bin/sh
# /etc/letsencrypt/renewal-hooks/deploy/m6-http-restart.sh, mode 0755
systemctl restart m6-http
```

systemd copies the certificate and key into m6-http's credential directory when the unit starts, so the hook restarts m6-http after each renewal, and until then m6-http serves the previous certificate.

`/perf` reports each loaded certificate with its expiry and its SHA-256 fingerprint, and the fingerprint is the identity two certificates with one expiry do not share.
It is the same value `openssl x509 -noout -fingerprint -sha256 -in fullchain.pem` prints, in lower-case hex with no colons, taken over the certificate's DER.
Every client already receives those bytes in the handshake, so the value discloses nothing a visitor does not have.

A fleet whose nodes share one certificate sets `[monitor] one_certificate = true`, and then a node whose leaf fingerprint differs is a fault in `/check` and in `/digest`.
Without the key the monitor warns only when some nodes agree and one does not, so it stays silent for a two-node fleet even when the two nodes differ.
Leave the key unset when each node renews its own certificate.

### 7.2. Secrets and state

Every m6 setting that names a secret, key or certificate takes an absolute path, and a relative one stops the service at startup with exit 2, naming the setting.
Table 1 lists the settings, the file each sits in and the program that reads it.

| Setting | File | Reader |
|---|---|---|
| `secrets_file` | any renderer config | m6-core |
| `[server] tls_cert`, `tls_key` | system config | m6-http |
| `[auth] public_key` | system config | m6-http |
| `[storage] path` | m6-auth config | m6-auth-server and m6-auth-cli |
| `[keys] private_key`, `public_key` | m6-auth config | m6-auth-server and m6-auth-cli |
| `perf_token_file` | `[monitor]` | m6-monitor |

**Table 1: the settings that name a secret, key or certificate, the file each sits in and the program that reads it. Each takes an absolute path.**

The documented layout:

- Configuration in `/etc/m6/`.
- A secret in `/etc/m6/<file>`, owned by root, mode 0600, delivered by the unit's `LoadCredential=<name>:/etc/m6/<file>` and named in the config as `/run/credentials/<unit>/<name>`.
- A service's database in `/var/lib/<service>/`, which `StateDirectory=<service>` creates, owned by the unit's user and writable under `ProtectSystem=strict`.

systemd copies a credential when the unit starts, so a changed secret reaches a service at its next restart.
A certificate renewal is one of those: certbot's deploy hook restarts m6-http (section 7.1).

m6-monitor's `history_file` may be relative, and `/var/lib/m6-monitor/` with `StateDirectory=m6-monitor` is the place for it.

The examples keep their development keys in `/tmp/m6/dev`, which `examples/dev-keys.sh` writes, outside every site directory.

Section 7.4 gives the unit files that carry the `LoadCredential` and `StateDirectory` lines.

### 7.3. Deploy site directory

Deploy the Example 05 site directory with rsync, then fix ownership.
Three paths stay on the server and are excluded from the sync:

- `content/drafts/`, managed by the CMS renderer
- `keys/`, so a key file never leaves the development machine

```bash
rsync -av --delete \
  --exclude 'content/drafts/' \
  --exclude 'keys/' \
  examples/05-cms/ \
  user@example.com:/var/www/my-blog/

chown -R m6:m6 /var/www/my-blog
```

The production `site.toml` differs from Example 05 in two places.
`domain` names the public site, and the backend `sockets` globs move from `/tmp/m6/` to `/run/m6/`, where a renderer writes its socket when no `M6_SOCKET_OVERRIDE` is set.
The method list stays in `site.toml`.
The `[server]` bind and certificate paths, and `[auth]`, live in the system config `/etc/m6/my-blog.toml`, which the m6-http unit in section 7.4 names.

```toml
[site]
name   = "My Blog"
domain = "example.com"

[server]
allowed_methods = ["GET", "HEAD", "POST", "PATCH", "DELETE"]

[errors]
mode = "internal"

[[backend]]
name    = "m6-html"
sockets = "/run/m6/m6-html*.sock"

# ... the remaining backends and routes as in Example 05, with sockets under /run/m6/
```

[`m6-site-toml.md`](m6-site-toml.md) section 3 owns the split between `site.toml` and the system config.

### 7.4. Unit files

One unit per process, five in total.
The four backend units (m6-html, m6-file, m6-auth, render-cms) follow one pattern and differ in `Description`, `ExecStart`, `ReadWritePaths`, `ReadOnlyPaths`, for m6-http `LoadCredential`, and for m6-auth `LoadCredential` and `StateDirectory`.
m6-http's unit names the system config as its second argument and adds `AmbientCapabilities` to bind port 443.

`/etc/systemd/system/m6-html.service`:

```ini
[Unit]
Description=m6-html renderer
After=network.target

[Service]
Type=simple
User=m6
Group=m6

RuntimeDirectory=m6
RuntimeDirectoryMode=0750
RuntimeDirectoryPreserve=yes

ExecStart=/usr/local/bin/m6-html \
    /var/www/my-blog \
    /var/www/my-blog/configs/m6-html.conf

Restart=on-failure
RestartSec=2

StandardOutput=journal
StandardError=journal
SyslogIdentifier=m6-html

NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=true
ReadWritePaths=/run/m6
ReadOnlyPaths=/var/www/my-blog /etc/m6

[Install]
WantedBy=multi-user.target
```

For more than one m6-html instance, use a template unit instead of one copy per instance.
The instance number becomes `%i`, and `M6_SOCKET_OVERRIDE` names a socket the pool glob matches.

`/etc/systemd/system/m6-html@.service`:

```ini
[Unit]
Description=m6-html renderer (instance %i)
After=network.target

[Service]
Type=simple
User=m6
Group=m6

RuntimeDirectory=m6
RuntimeDirectoryMode=0750
RuntimeDirectoryPreserve=yes

Environment=M6_SOCKET_OVERRIDE=/run/m6/m6-html-%i.sock

ExecStart=/usr/local/bin/m6-html \
    /var/www/my-blog \
    /var/www/my-blog/configs/m6-html.conf

Restart=on-failure
RestartSec=2

StandardOutput=journal
StandardError=journal
SyslogIdentifier=m6-html-%i

NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=true
ReadWritePaths=/run/m6
ReadOnlyPaths=/var/www/my-blog /etc/m6

[Install]
WantedBy=multi-user.target
```

The `[[backend]]` entry in the production `site.toml` (§7.3) names `sockets = "/run/m6/m6-html*.sock"`, which matches the instance sockets too, because `m6-html@1` writes `/run/m6/m6-html-1.sock`.
A deployment that runs only the template unit can narrow the glob to `/run/m6/m6-html-*.sock`.

`/etc/systemd/system/m6-file.service`:

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
RuntimeDirectoryPreserve=yes

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

`/etc/systemd/system/m6-auth-login.service`, the login side:

```ini
[Unit]
Description=m6-auth login service
After=network.target

[Service]
Type=simple
User=m6
Group=m6
RuntimeDirectory=m6
RuntimeDirectoryMode=0750
RuntimeDirectoryPreserve=yes

ExecStart=/usr/local/bin/m6-auth-server \
    /var/www/my-blog \
    /etc/m6/m6-auth.conf \
    --login-only

LoadCredential=auth.pem:/etc/m6/auth.pem
LoadCredential=auth.pub:/etc/m6/auth.pub
StateDirectory=m6-auth
StateDirectoryMode=0700

Restart=on-failure
RestartSec=2
StandardOutput=journal
StandardError=journal
SyslogIdentifier=m6-auth-login
NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=true
ReadWritePaths=/run/m6
ReadOnlyPaths=/var/www/my-blog /etc/m6

[Install]
WantedBy=multi-user.target
```

`/etc/systemd/system/m6-auth-check.service`, the check side:

```ini
[Unit]
Description=m6-auth check service
After=network.target

[Service]
Type=simple
User=m6
Group=m6
RuntimeDirectory=m6
RuntimeDirectoryMode=0750
RuntimeDirectoryPreserve=yes

ExecStart=/usr/local/bin/m6-auth-server \
    /var/www/my-blog \
    /etc/m6/check/m6-auth.conf \
    --check-only

LoadCredential=auth.pem:/etc/m6/auth.pem
LoadCredential=auth.pub:/etc/m6/auth.pub
StateDirectory=m6-auth
StateDirectoryMode=0700

Restart=on-failure
RestartSec=2
StandardOutput=journal
StandardError=journal
SyslogIdentifier=m6-auth-check
NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=true
ReadWritePaths=/run/m6
ReadOnlyPaths=/var/www/my-blog /etc/m6

[Install]
WantedBy=multi-user.target
```

m6-auth's keys come from the two `LoadCredential` lines, and its database lives in `/var/lib/m6-auth`, which `StateDirectory=m6-auth` creates.
Section 7.2 gives the layout.

The two units run the same binary, the same database and the same keys.
They take one config each because `LoadCredential` places a unit's keys under `/run/credentials/<unit>/`, and the `[keys]` paths in the config are absolute.
`/etc/m6/m6-auth.conf` names `/run/credentials/m6-auth-login.service/auth.pem`, and `/etc/m6/check/m6-auth.conf` names `/run/credentials/m6-auth-check.service/auth.pem`.
The two files are identical apart from those two paths, and both sit at a path with the stem `m6-auth` so the socket derivation agrees.

The login unit binds `/run/m6/m6-auth.sock` and the four login routes.
The check unit binds `/run/m6/check-m6-auth.sock` and `/check`.
The `check-` prefix keeps the check socket out of the login backend's `/run/m6/m6-auth*.sock` glob, so the existing m6-http backend and its routes keep dialling the login socket.
The two processes are what keep a login's bcrypt verify off the check loop. A single unit with no flags binds both sockets in one process, and then a login still delays every `/check` for the length of its verify.

`/etc/systemd/system/render-cms.service`:

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
RuntimeDirectoryPreserve=yes

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

The render-cms binary lives in `/var/www/my-blog/bin/`, deployed beside the site.

`/etc/systemd/system/m6-http.service`:

```ini
[Unit]
Description=m6-http reverse proxy and cache
Documentation=https://github.com/mgrosvenor/m6
After=network.target m6-html.service m6-file.service m6-auth-login.service m6-auth-check.service render-cms.service

[Service]
Type=simple
User=m6
Group=m6
RuntimeDirectory=m6
RuntimeDirectoryMode=0750
RuntimeDirectoryPreserve=yes

WorkingDirectory=/var/www/my-blog

ExecStart=/usr/local/bin/m6-http /var/www/my-blog /etc/m6/my-blog.toml

LoadCredential=tls-cert.pem:/etc/letsencrypt/live/example.com/fullchain.pem
LoadCredential=tls-key.pem:/etc/letsencrypt/live/example.com/privkey.pem
LoadCredential=auth.pub:/etc/m6/auth.pub

Restart=on-failure
RestartSec=2
StandardOutput=journal
StandardError=journal
SyslogIdentifier=m6-http

AmbientCapabilities=CAP_NET_BIND_SERVICE
NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=true
ReadWritePaths=/run/m6 /var/www/my-blog/logs
ReadOnlyPaths=/var/www/my-blog /etc/m6

[Install]
WantedBy=multi-user.target
```

`AmbientCapabilities=CAP_NET_BIND_SERVICE` lets the unprivileged `m6` user bind port 443.
`After=` is an ordering hint only.
m6-http reads each pool's sockets once at start, so the backends' sockets must exist before it starts, and a pool whose glob matches no socket exits 2.

Section 7.5 installs these units and starts the stack.

### 7.5. Install and start

Copy the five units into place, reload systemd, then enable and start them together:

```bash
cp examples/06-systemd/systemd/*.service /etc/systemd/system/
systemctl daemon-reload
systemctl enable m6-html m6-file m6-auth render-cms m6-http
systemctl start m6-html m6-file m6-auth render-cms m6-http
systemctl status m6-html m6-file m6-auth render-cms m6-http
```

Each unit reports `active (running)`:

```
● m6-html.service - m6-html renderer
     Loaded: loaded (/etc/systemd/system/m6-html.service; enabled)
     Active: active (running) since ...
   Main PID: 12345 (m6-html)
```

### 7.6. Verify

Check that m6-http is listening and that every backend wrote its socket into `/run/m6/`:

```bash
ss -tlnp | grep :443

ls -la /run/m6/
```

The socket directory lists four sockets, one per backend:

```
/run/m6/m6-html.sock
/run/m6/m6-file.sock
/run/m6/m6-auth.sock
/run/m6/render-cms.sock
```

Then smoke-test the public routes, with `$DOMAIN` set to the domain from `site.toml`:

```bash
DOMAIN=example.com

curl -s -o /dev/null -w "%{http_code}" "https://$DOMAIN/"
# 200

curl -s -o /dev/null -w "%{http_code}" "https://$DOMAIN/blog"
# 200

curl -s -o /dev/null -w "%{http_code}" "https://$DOMAIN/cms"
# 401
```

Without `-k`, `curl` verifies the certificate chain, hostname and expiry, so these codes also prove the TLS boundary.

The `/cms` route is protected, so a plain `curl` gets a 401.
A browser, which sends `Accept: text/html`, gets a 302 to `/login` instead.
That split is m6-http's own rule for a missing token.

### 7.7. Logs

Every unit logs to journald, so `journalctl` reads them all:

```bash
journalctl -f -u m6-http -u m6-html -u m6-file -u m6-auth -u render-cms
journalctl -u m6-http -f
journalctl -u m6-http -o cat | jq .
journalctl -u m6-http -n 100
journalctl -u m6-http -b
journalctl -u m6-http -p err
journalctl -u m6-http --since "2024-01-15 09:00" --until "2024-01-15 10:00"
```

Every m6 log line is JSON, so `journalctl -u m6-http -o cat | jq .` parses every one.

### 7.8. Rotating the analytics log

m6-http writes the analytics log to one file and reopens it when it receives SIGUSR1.
Rotation renames the file and sends that signal, so no line is lost and `copytruncate` is not needed.
This logrotate stanza rotates the file every hour and keeps the last 48 files, two days:

```
/var/www/my-blog/logs/analytics.ndjson {
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

`rotate` is a finite count because every kept file uses disk.
`rotate -1` keeps files forever and is not suitable.
Choose a count that fits the disk at your traffic.

`hourly` takes effect only when logrotate itself runs every hour, and logrotate's default schedule is once a day.
Run it hourly with a systemd timer override:

```
# systemctl edit logrotate.timer
[Timer]
OnCalendar=
OnCalendar=hourly
AccuracySec=1m
```

On a system that uses cron instead, add `/etc/cron.hourly/logrotate` containing `#!/bin/sh` and `exec /usr/sbin/logrotate /etc/logrotate.conf`, and make it executable.
Keep the daily run too when other stanzas use `daily`.
logrotate skips a file that is not due, so the double run is harmless.

There is no `create` line, because m6-http creates the file itself.
The wait in `postrotate` proves that reopen happened.
The log writer cannot write to the renamed file once the new file exists, so the old file is finished before logrotate compresses it.
When the reopen fails, the wait times out and `postrotate` fails.
`--kill-whom=` needs systemd 252 or later, and older systemd spells it `--kill-who=`.

`/perf` reports the last reopen as `logging.reopen`, with the seconds since the last reopen, how many reopens succeeded and failed, and the age of the last failure.
`m6-monitor` shows it for each node in section A of `/check` and in `/digest`.
Setting `[monitor] log_reopen_max_age_s` makes it a fault when a node's last reopen is older than that many seconds.
A limit of twice the rotation interval suits an hourly rotation, so 7200 seconds with the schedule above.
The limit is only as true as the schedule, so set it after logrotate runs hourly.
`/traffic` answers from a window in memory and reads no log file, so rotation leaves its figures alone.
The window covers the last 60 minutes this process served, starts empty after a restart, and `window_minutes` says how much of the hour it holds.
The `/traffic` paragraph in [`m6-http.md`](m6-http.md) gives the field caps and the count of events a full window dropped.

### 7.9. Monitor history

m6-monitor keeps a history of every poll when `[monitor] history_file` is set, and serves it as *JSON Lines* (one JSON object per line).
Table 2 gives the two keys, which are read once at start.

| key (`[monitor]`) | default | rule |
|---|---|---|
| `history_file` | absent, so no history | a non-empty path, relative to the site directory |
| `history_max_mb` | 256 | 1 to 1048576, one MB is 2^20 bytes |

**Table 2: the two `[monitor]` keys that switch the history on and cap it, with their defaults and rules.**
A change to either needs a restart.
Put `history_file` in `/var/lib/m6-monitor/` and give the unit `StateDirectory=m6-monitor`.

The file holds one row per node per poll, one row per channel and interface, the text of every finding, and once per `/traffic` window the response status counts and crawler sightings of each node.
When the file passes `history_max_mb`, the oldest polls are deleted, 256 at a time, until it is 5% under the cap.
Nothing expires by age.
One window of one node stores at most 100 crawler sightings, the ones with the most requests, and cuts each user agent to 256 bytes.
The `/crawlers` key `sightings_dropped` says how many were left out.
On Unix the monitor creates the directory with mode 0700 and the file with mode 0600, because the file holds client addresses.
A write that fails is a warning on node `monitor` and never stops polling.
While the history file is locked the monitor holds the polls in memory, up to `[limits.history_queue] max` (1024 polls, `m6-app-anatomy.md` §4.2), and a poll past that is refused and counted instead of dropping the oldest.
A prune that fails, or a cap that deleting every earlier poll cannot meet, is its own warning, and the poll is still stored.
To start fresh, stop the service, delete the file and start it again.

The `/check` report starts with a HEALTH section, one line per node, showing this poll's `/health` answer and the age of the node's last 200 from this history.
`[monitor] health_last_ok_max_age_s` sets the age above which the line is flagged, and it defaults to three `poll_interval_s` values.
A node is ok only when `/health` answers 200 and the body status is `ok`. Every other node is a fault on its line and in the verdict: a 503 whatever its body says, a 200 with any other status, and no answer at all. The last 200 is read from an index that holds only the 200 rows, so reading it does not slow as the history grows.

`GET /history` and `GET /crawlers` answer on the service's Unix socket and nowhere else.
Both take `from` and `to` as *RFC 3339* UTC times, the range `[from, to)` is at most 31 days, and `node` limits the answer to one node.
A page holds at most 4 MiB and 10,000 rows.
When more rows match, the response carries a `Link` header with `rel="next"`, and the client follows it until a page has none.

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

The node's own `periodic stats` log is a 10 second series in the node's journal, which is capped.
It has no channels, reachability, host figures or tunnel, and it disappears while a node is down.
Only the history keeps those, and it keeps them through an outage.
Channel counters separate the monitor's own polls and header checks (issue #107) into `http/<version>/monitor`, and `monitor_requests` on each node row says how many.

### 7.10. Deploy updates

Deploy code and template changes with rsync, then restart m6-http so it reads the new `site.toml`:

```bash
rsync -av --delete \
  --exclude 'content/drafts/' \
  --exclude 'keys/' \
  examples/05-cms/ \
  user@example.com:/var/www/my-blog/
```

When the render-cms binary changes, rebuild it, deploy it and restart only that unit:

```bash
cargo build --release -p render-cms
rsync target/release/render-cms user@example.com:/var/www/my-blog/bin/
ssh user@example.com systemctl restart render-cms
```

The other units keep running, and m6-http resumes routing to render-cms when its socket reappears.
For certificate renewal, certbot renews and its deploy hook restarts m6-http, and m6-http reads the new certificate:

```bash
certbot renew
```

### 7.11. Horizontal scaling

The template unit replaces the plain one: the plain unit's `/run/m6/m6-html.sock` matches the same `/run/m6/m6-html*.sock` pool glob, so leaving it running beside `m6-html@1` and `m6-html@2` puts three processes behind one pool.
Add two m6-html instances with it, then restart m6-http so it reads the pool again:

```bash
systemctl disable --now m6-html.service
systemctl enable --now m6-html@1 m6-html@2
systemctl restart m6-http
```

The instance number sets the socket, so `m6-html@2` writes `/run/m6/m6-html-2.sock` and the pool glob in `site.toml` matches both.
m6-http reads the pool once at start, so restart it to add the instance, then it load-balances traffic across both.

To scale back down:

```bash
systemctl disable --now m6-html@2
```

The socket disappears and m6-http keeps the member in its rotation. Each request that lands on it tries the next member, and the instance serves again on its next turn once a listener is bound there.

### 7.12. Crash recovery

systemd restarts a failed unit with `Restart=on-failure` and `RestartSec=2`.
During the brief restart window, a request that lands on the missing socket tries the next member and is served. If every member of the pool is down, the affected routes answer 502 until one is back.

```bash
systemctl kill -s KILL m6-html
journalctl -u m6-html -f
# ... process exited with status 9 (SIGKILL)
# ... Starting m6-html renderer...
# ... m6-html started, listening on /run/m6/m6-html.sock
```

m6-http serves from the pool again on the next turn once a listener is bound there.

Example 06 shows the same site running as five supervised units.
Example 07 makes the development and production split explicit, with one `site.toml` and a system config per environment.

## 8. Example 07, dev to production

Example 07 is the Example 05 CMS blog with the development and production split made explicit.
One `site.toml` holds what every node shares, and a system config holds what each node owns.
The same site directory runs in both environments.
This section walks the split, the dev script, the first-time server setup, the deploy script and the common mistakes.

### 8.1. The split

`site.toml` holds the site identity, the method list, the log and error settings, the four backends and every route.
It carries no node address, no certificate path and no credential:

```toml
[site]
name   = "My Site"
domain = "localhost"

[server]
allowed_methods = ["GET", "HEAD", "POST", "PATCH"]

[errors]
mode             = "internal"
verbose_fallback = true

[log]
level = "info"

[[backend]]
name    = "m6-html"
sockets = "/tmp/m6/m6-html*.sock"

[[backend]]
name    = "m6-file"
sockets = "/tmp/m6/m6-file*.sock"

[[backend]]
name    = "m6-auth"
sockets = "/tmp/m6/m6-auth*.sock"

[[backend]]
name    = "m6-auth-check"
sockets = "/tmp/m6/check-m6-auth*.sock"

[[backend]]
name    = "render-cms"
sockets = "/tmp/m6/render-cms-prod*.sock"

# ... the public, auth and CMS routes as in Example 05
```

The development system config `configs/system-dev.toml` holds the node name, bind address, certificate paths and auth key:

```toml
[server]
bind     = "127.0.0.1:8443"
tls_cert = "/tmp/m6/dev/dev.pem"
tls_key  = "/tmp/m6/dev/dev-key.pem"

[auth]
backend    = "m6-auth-check"
public_key = "/tmp/m6/dev/auth.pub"

[node]
name = "dev"
```

The production system config `/etc/m6/my-blog.toml` holds the same three sections, with the node name `origin`, the public bind `0.0.0.0:443`, the two credential paths under `/run/credentials/m6-http.service/`, and the auth public key `/run/credentials/m6-http.service/auth.pub`.
Section 8.3 writes it as a heredoc.

The split is one rule: `site.toml` holds the state every node shares, and the system config holds the state of one node.
On a conflict, the system config's `[server]` keys win.
Declaring `[auth]` in both files is a startup error, exit 2.
[`m6-site-toml.md`](m6-site-toml.md) section 3 owns the full split and its merge rules.

### 8.2. Dev script

Build the renderer first with `cargo build --release -p render-cms-prod`.
Then `dev.sh` generates the development certificate and auth keys, creates the admin user and the editors group, runs `m6-md` over `content/posts/`, and starts the pre-built stack.
It sets `M6_SOCKET_OVERRIDE` for each backend so the sockets land in `/tmp/m6/`, where `site.toml` names them.
It starts m6-http last with the dev system config:

```sh
M6_SOCKET_OVERRIDE=/tmp/m6/m6-html.sock \
    m6-html "$SITE" "$SITE/configs/m6-html.conf" &

M6_SOCKET_OVERRIDE=/tmp/m6/m6-auth.sock \
    m6-auth-server "$SITE" "$SITE/configs/m6-auth.conf" &

M6_SOCKET_OVERRIDE=/tmp/m6/render-cms-prod.sock \
    "$EXAMPLES/target/release/render-cms-prod" "$SITE" "$SITE/configs/render-cms.conf" &

m6-http "$SITE" "$SITE/configs/system-dev.toml" &
```

The custom renderer crate is named `render-cms-prod`, so its binary and socket carry that name.
Run it from `examples/07-dev-to-production` and open `https://localhost:8443`.

### 8.3. First-time server setup

On the server, create the user, the site directory and the auth keys, then write the production system config:

```bash
useradd --system --no-create-home --shell /usr/sbin/nologin m6

mkdir -p /var/www/my-blog
chown m6:m6 /var/www/my-blog

mkdir -p /etc/m6
openssl genpkey -algorithm EC -pkeyopt ec_paramgen_curve:P-256 -pkeyopt ec_param_enc:named_curve -out /etc/m6/auth.pem
openssl pkey -in /etc/m6/auth.pem -pubout -out /etc/m6/auth.pub
chmod 600   /etc/m6/auth.pem
chmod 600   /etc/m6/auth.pub

Write `/etc/m6/m6-auth.conf` as section 7.1 shows it.

cat > /etc/m6/my-blog.toml << 'TOML'
[server]
bind     = "0.0.0.0:443"
tls_cert = "/run/credentials/m6-http.service/tls-cert.pem"
tls_key  = "/run/credentials/m6-http.service/tls-key.pem"

[auth]
backend    = "m6-auth-check"
public_key = "/run/credentials/m6-http.service/auth.pub"

[node]
name = "origin"
TOML
chown root:m6 /etc/m6/my-blog.toml
chmod 640 /etc/m6/my-blog.toml

certbot certonly --standalone -d example.com

cp examples/07-dev-to-production/systemd/*.service /etc/systemd/system/
systemctl daemon-reload
```

Deploy the site with `deploy.sh`, then enable and start the units:

```bash
systemctl enable m6-html m6-file m6-auth render-cms m6-http
systemctl start  m6-html m6-file m6-auth render-cms m6-http
```

### 8.4. Systemd unit, m6-http

The m6-http unit is the same as Example 06's in section 7.4, and the renderer units are identical to Example 06.
Each takes two positional arguments, the site directory and its config.

### 8.5. Deploy

`deploy.sh` runs from the development machine and syncs the site directory to the server.
The `--binary` flag rebuilds render-cms, deploys the binary and restarts only that unit.
The rsync excludes the server-managed state so `--delete` does not wipe it.

```bash
#!/bin/bash
# deploy.sh, run from the development machine
set -e

SERVER="user@example.com"
REMOTE_SITE="/var/www/my-blog"

rsync -av --delete \
  --exclude 'content/drafts/' \
  --exclude 'content/posts/' \
  --exclude 'data/posts.json' \
  --exclude 'keys/' \
  --exclude '*.pem' \
  --exclude '*.pub' \
  --exclude 'target/' \
  ./ "$SERVER:$REMOTE_SITE/"

ssh "$SERVER" chown -R m6:m6 "$REMOTE_SITE"

if [[ "$1" == "--binary" ]]; then
  cargo build --release -p render-cms-prod
  rsync target/release/render-cms-prod "$SERVER:$REMOTE_SITE/bin/render-cms"
  ssh "$SERVER" systemctl restart render-cms
fi

echo "Deployed."
# restart m6-http so it reads the new site.toml
```

### 8.6. Dev vs production at a glance

Table 3 maps each setting to its two values.
Take from it that the two environments differ only in the system config and the process manager.

| Setting | Development | Production |
|---|---|---|
| m6-http invocation | `m6-http $SITE configs/system-dev.toml` | `m6-http /var/www/my-blog /etc/m6/my-blog.toml` |
| `[server] bind` | `127.0.0.1:8443` | `0.0.0.0:443` |
| `[server] tls_cert` | `/tmp/m6/dev/dev.pem` | `/run/credentials/m6-http.service/tls-cert.pem` |
| `[auth] public_key` | `/tmp/m6/dev/auth.pub` | `/run/credentials/m6-http.service/auth.pub` |
| System config | `configs/system-dev.toml`, in the repo | `/etc/m6/my-blog.toml`, on the server |
| Process manager | `dev.sh` | systemd |
| Logs | terminal stdout | journald |

**Table 3: development and production, key by key.**
The site directory, the routes and the backends are identical in both environments.

### 8.7. Checking what is in effect

`--dump-config` prints the merged config, which is the fastest way to see what a production run actually uses:

```bash
m6-http /var/www/my-blog /etc/m6/my-blog.toml --dump-config
```

The output shows the `[server]` values from the system config and everything else from `site.toml`.
The merge rule is the one in section 8.1.

### 8.8. Common mistakes

Four mistakes come up, each with its fix:

- **Forgetting the system config argument.**
  `m6-http /var/www/my-blog` exits 2, because the second argument is required.
- **Putting node values in `site.toml`.**
  `bind`, the certificate paths and `[auth]` belong in the system config.
  The merge rule is the one in section 8.1.
- **Deploying over server-managed state.**
  Sync with the exclusions from section 8.5, so `--delete` does not remove `data/posts.json`, `content/posts/` or the auth database.
- **Editing `site.toml` on the server.**
  The next deploy overwrites it.
  Edit locally, deploy, and keep server-specific overrides in `/etc/m6/my-blog.toml`.

Example 07 makes the shared and node-owned state explicit.
Example 08 adds a log viewer that tails the JSON logs in the browser.

## 9. Example 08, log viewer

Example 08 adds a real-time log viewer to the site.
m6-http writes JSON logs, m6-file serves them from a byte offset, and a browser page polls and renders the new lines.
The example demonstrates JSON logging and m6-file's tail mode.

```bash
cd examples/08-logviewer
./dev.sh
# open https://localhost:8443/logs
```

The stack is three processes: m6-http, m6-html and m6-file.
`dev.sh` redirects each process's output to its own file under `logs/`, so the viewer has one file per process.

`site.toml` sets `level = "debug"`, and routes `/logs/tail/{relpath}` straight to m6-file.
The route is direct, so m6-http forwards the request without requiring the log files to exist at startup.
The example leaves the tail route unauthenticated, and `level = "debug"` logs can carry request detail.
A production deployment protects the route with `require = "role:..."`, the way section 7.9 protects the monitor history.

`configs/m6-file.conf` turns on tail mode for the log files:

```toml
[[route]]
path = "/logs/tail/{*relpath}"
handler = "files"
root = "logs/"
tail = true
```

The browser polls `GET /logs/tail/<name>.log?offset=N`.
m6-file returns the bytes from that offset and the `X-Log-End` header, which the client reads for the next offset.
The route is marked uncacheable.

[`m6-file.md`](m6-file.md) owns the tail handler and its headers.
[`m6-site-toml.md`](m6-site-toml.md) owns `format` and `level`.

## 10. Example 09, global deployment

Example 09 is a multi-node deployment: one origin runs the full stack, and cache nodes around it run m6-http in pure proxy mode.
A private *WireGuard* mesh connects the nodes, and cache invalidation propagates from the origin to every cache node on publish.

```bash
cd examples/09-global-deployment
# See setup-origin.sh and setup-cache-node.sh
```

The origin serves the Example 07 site and answers the public domain.
Each cache node runs only m6-http, pointing at the origin over the mesh, and caches responses independently.
`setup-origin.sh` provisions the origin and `setup-cache-node.sh` provisions a cache node, both writing a system config, the WireGuard interface and the systemd unit.
`deploy.sh` syncs content to the origin and, with `--all --invalidate`, to the cache nodes, then touches `site.toml` on each node. The touch no longer reloads m6-http, so an operator restarts m6-http to apply the change.

The example uses its own node names and addresses, held in `deploy.toml`, which is a local file never committed.
The configs under `configs/` and the `wireguard/` files are the node-specific settings.

## 11. Example 10, API tokens

Example 10 adds long-lived API tokens for scripts and services.
A token is a *JWT* (JSON Web Token) passed as `Authorization: Bearer <token>`, and m6-http verifies it with the same key and the same `require` rules as a session cookie.

```bash
cd examples/10-api-tokens
./setup.sh
./dev.sh
```

`setup.sh` creates the `api` user with the `api` and `user` roles.

### 11.1. When to use API tokens

Table 4 compares a session cookie with an API token.
Take from it that the token is the machine-friendly form of the same credential.

| | Session cookie | API token |
|---|---|---|
| Caller | Browser | Script, service or *CI* (continuous integration) |
| Lifetime | 15 minutes, refreshes to 30 days | Configurable, default 365 days |
| Transport | `Cookie: session=...` | `Authorization: Bearer ...` |
| Issuance | `POST /auth/login` | `m6-auth-cli token create` |
| Revocation | `POST /auth/logout` | `m6-auth-cli token revoke` |

**Table 4: session cookie versus API token.**
Both are signed with the same *EC* (Elliptic Curve) key, and `require` accepts either.

### 11.2. Setup

Create a user, issue a token, then use it:

```bash
m6-auth-cli configs/m6-auth.conf user add api --role api --role user --password secret

TOKEN=$(m6-auth-cli configs/m6-auth.conf token create api --name ci-pipeline --ttl-days 30)
echo "$TOKEN"
# eyJhbGciOiJFUzI1NiIsInR5cCI6IkpXVCJ9...

curl -sk https://localhost:8443/api/data \
     -H "Authorization: Bearer $TOKEN"
```

The default token lifetime is 365 days, and `--ttl-days` shortens it.
`--name` defaults to `api-token`.

### 11.3. `site.toml`, protecting an API endpoint

One `require` key protects an endpoint for both session cookies and Bearer tokens:

```toml
[[route]]
path    = "/api/data"
backend = "m6-html"
require = "role:api"
```

A browser user with the `api` role can also reach the route.
For a machine-only endpoint, use a dedicated role and never assign it to a login account.

### 11.4. Token management

```bash
m6-auth-cli configs/m6-auth.conf token ls api
# ID                                    NAME          CREATED     EXPIRES
# 550e8400-e29b-41d4-a716-446655440000  ci-pipeline   2026-03-21  2026-04-20

m6-auth-cli configs/m6-auth.conf token ls api --json

m6-auth-cli configs/m6-auth.conf token revoke 550e8400-e29b-41d4-a716-446655440000
```

`token revoke` removes the token from the database and the listing.
The *JWT* itself stays cryptographically valid until its `exp` claim, because verification is stateless.
For immediate revocation, use a short `--ttl-days`.

### 11.5. Template variables in protected routes

A request with a valid Bearer token gives m6-html templates the same `auth_username` and `auth_roles` variables as a browser login:

```html
<p>Caller: {{ auth_username }}</p>
<p>Roles:  {{ auth_roles | join(sep=", ") }}</p>
```

### 11.6. Functional tests

`test.sh` runs against a started stack:

```bash
./dev.sh    # one terminal
./test.sh   # another
```

The checks cover public access, a 401 without a token, token create, list and revoke, role enforcement and the error page.

## 12. Example 11, admin dashboard API

Example 11 is a self-hosting management API and dashboard for an m6 site.
It is a custom renderer, `render-admin`, with its own loop and plain state in `main`.
Every `/api/admin/*` route is protected by `require = "role:admin"`.

```bash
cd examples/11-admin-dashboard
./setup.sh
./dev.sh
# open https://localhost:8444/
```

Example 11 runs on Linux.
On macOS, `render-admin` exits 2 at start because the shipped config sets `bench_bin` and the bench child is reaped from a pidfd, which only Linux provides.

The dashboard is served by m6-file at `https://localhost:8444/` and refreshes every 10 seconds.
It has six tabs: Perf, System, Bench, Logs, Routes and Config.

### 12.1. API endpoints

Table 5 lists the endpoints the renderer registers.
Take from it that the first twelve manage the dashboard tabs, and the last five report backend and m6-http statistics.

| Method | Path | Returns |
|---|---|---|
| `GET` | `/api/admin/perf` | Last N periodic stats entries |
| `GET` | `/api/admin/routes` | Per-route request counts, cache rates and latency |
| `GET` | `/api/admin/system` | CPU, memory, disks and uptime |
| `GET` | `/api/admin/bench` | Bench targets from the system config bind |
| `POST` | `/api/admin/bench` | Start a bench job, returning a job id |
| `GET` | `/api/admin/bench/{id}` | Poll a bench job |
| `GET` | `/api/admin/logs` | Last N log lines |
| `GET` | `/api/admin/config` | site.toml as a JSON object |
| `PUT` | `/api/admin/config` | Write site.toml from a JSON object |
| `POST` | `/api/admin/config/touch` | Touch site.toml (a restart of m6-http applies the change) |
| `POST` | `/api/admin/cache/flush` | Alias for config touch (a restart of m6-http applies the change) |
| `POST` | `/api/admin/services/{name}/restart` | Send SIGTERM to a service via its PID file |
| `GET` | `/api/admin/backends` | Backend topology and the system config bind |
| `GET` | `/api/admin/backends/{name}/sample` | One backend's stats over the sample window |
| `GET` | `/api/admin/backends/{name}/alltime` | One backend's stats over all time |
| `GET` | `/api/admin/m6http/sample` | m6-http's own stats over the sample window |
| `GET` | `/api/admin/m6http/alltime` | m6-http's own stats over all time |

**Table 5: the seventeen `/api/admin` endpoints.**
`site.toml` must declare `allowed_methods` with `PUT`, or the edge answers `PUT /api/admin/config` with a 405.

### 12.2. JSON blob shapes

The endpoints return a single JSON blob each.
`GET /api/admin/perf?n=60` returns the recent periodic stats entries, and `GET /api/admin/system` returns the host figures.
`GET /api/admin/bench` lists the targets taken from the `[server] bind` in the system config, one per protocol.
`POST /api/admin/bench` starts a job and `GET /api/admin/bench/{id}` polls it.
`GET /api/admin/routes?n=10000` samples the request-complete log and returns per-route counts and hit rates, sorted by request count.
`GET /api/admin/logs?n=100` returns the last log lines.
`GET /api/admin/config` returns site.toml parsed as JSON, and `PUT` writes it back after validation, atomically.
`POST /api/admin/services/{name}/restart` sends SIGTERM to the process whose PID is in the configured PID file.

### 12.3. Config: `render-admin.conf`

The renderer reads its settings from its config:

```toml
socket           = "/tmp/m6/render-admin.sock"
log_file         = "/tmp/m6/m6-http-admin.log"
site_toml        = "site.toml"
system_toml      = "configs/system-dev.toml"
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

`system_toml` names the system config whose `[server] bind` supplies the bench targets.
The `[[services]]` list names the processes the restart endpoint can signal.

### 12.4. Tests

`cargo test -p render-admin` runs the unit and integration suites for the five modules, `bench`, `logs`, `ops`, `perf` and `system`.
They cover perf log parsing, per-route aggregation, system metrics shape, bench target discovery, the job lifecycle, log tail content, config read and write round-trips, and service restart.

### 12.5. Functional tests

`test.sh` runs against a started stack:

```bash
./dev.sh    # one terminal
./test.sh   # another
```

The checks cover a 401 without a token, the JSON keys of each endpoint, a config restart and a cache flush, and an unknown service returning 404.

## 13. Summary

The eleven examples layer one m6 capability onto the last.
By Example 11 a reader has seen serving, auth, a CMS, a systemd deployment, a multi-node fleet, API tokens and an admin dashboard.
A reader can now copy the example that matches the next step, and follow the owning document each example points to for the details it leaves out.
