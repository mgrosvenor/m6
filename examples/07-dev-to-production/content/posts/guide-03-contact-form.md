+++
title   = "Guide 03: Contact Form with a Custom Renderer"
date    = "2025-01-05"
summary = "Write your first custom renderer with m6-core. Handle a GET/POST contact form and send email via SMTP from the renderer's own event loop."
tags    = ["guide", "tutorial", "m6-core", "custom-renderer"]
cover   = "https://picsum.photos/seed/guide03/1200/500"
+++

**Guide series:**
[Prerequisites](/blog/guide-00-prerequisites) →
[Example 01](/blog/guide-01-static-site) →
[Example 02](/blog/guide-02-blog) →
**Example 03 — Contact Form** →
[Example 04](/blog/guide-04-auth) →
[Example 05](/blog/guide-05-cms) →
[Example 06](/blog/guide-06-systemd) →
[Example 07](/blog/guide-07-dev-to-production)

---

`examples/03-contact/` introduces custom renderers. `render-contact` handles GET (renders the form) and POST (sends email via SMTP). Everything else — routing, caching, TLS — stays in the m6 layer.

```bash
cd examples/03-contact
cargo build --release -p render-contact
./dev.sh
```

## What's a custom renderer?

<pre class="mermaid">
flowchart LR
    Browser --"HTTPS"--> mhttp["m6-http"]
    mhttp --"/contact GET"--> mhtml["m6-html\n(template render)"]
    mhttp --"/contact POST"--> rc["render-contact\n(SMTP send)"]
    rc --"email"--> SMTP[("SMTP server")]
</pre>

Any HTTP/1.1 server that listens on a Unix socket. m6-http finds it when it expands the pool's socket glob at start, and routes requests to it. A Rust renderer links `m6-core`, which supplies the parts of a service. The service writes its own event loop and its handlers.

## `render-contact/Cargo.toml`

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

The `email` feature compiles `m6_core::email`: an SMTP client the service's own loop drives without blocking.

## `render-contact/src/main.rs`

State is a plain value built once at start. Here it is the mail relay, resolved and checked, and the two addresses:

```rust
use m6_core::email::{lettre, Relay, RelayConfig, Smtp};
use m6_core::prelude::*;

struct Global {
    relay: Relay,
    from:  String,
    to:    String,
}

fn load_state(site: &Site) -> Result<Global> {
    let config = site.user_config();
    let host = config["smtp"]["host"].as_str()
        .ok_or_else(|| Error::Other(anyhow::anyhow!("smtp.host missing")))?;
    // ... port, username, password, from and to are read the same way
    let relay = Relay::new(&RelayConfig {
        host: host.to_string(),
        port,
        starttls: tls,          // None only for localhost, 127.0.0.1 and ::1
        credentials,
        timeout: Duration::from_secs(20),
    }).map_err(|e| Error::Other(anyhow::anyhow!("{e}")))?;
    Ok(Global { relay, from, to })
}
```

The POST handler builds the message and starts the send on its connection slot's `Smtp`. It returns the page to show once the relay has accepted the message:

```rust
fn handle_post(req: &Request, global: &Global, smtp: &mut Smtp) -> Result<Response> {
    let name    = req.field("name")?;
    let email   = req.field("email")?;
    let text    = req.field("message")?;

    let msg = message(global, &name, &email, &text)?;
    smtp.start(&global.relay, &msg).map_err(|e| Error::Other(e.into()))?;

    Response::render_with("templates/contact.html", req, json!({"sent": true, "name": name}))
}
```

The routes are a constant, and `main` is the service's own loop:

```rust
const ROUTES: &[CodeRoute] = &[CodeRoute {
    pattern: "/contact",
    method: RouteMethod::Post,
}];
```

`GET /contact` is a template route in the configuration, so only the POST is code.

## `configs/render-contact.conf`

```toml
global_params = ["data/site.json"]
secrets_file  = "/etc/m6/render-contact.toml"

[[route]]
path     = "/contact"
methods  = ["GET"]
template = "templates/contact.html"
params   = []
cache    = "no-store"

# Development defaults. In production these move to secrets_file and are
# deleted from here: a key set in both files is refused at start.
[smtp]
host     = "localhost"
port     = 1025
# The development relay speaks plain SMTP, which m6-core allows only to
# localhost. Production relays take STARTTLS: delete this line.
starttls = false
username = ""
password = ""
from     = "noreply@example.com"
to       = "owner@example.com"
```

`secrets_file` is merged in if the file exists. If absent (on dev machines), it's silently ignored and the `[smtp]` block above provides defaults. The path is absolute and safe to commit — it contains no secrets itself. `methods = ["GET"]` limits the template route to GET, because the POST is the code route in `main.rs`.

Production secrets file (on the server, not in version control):

```toml
# /etc/m6/render-contact.toml
[smtp]
host     = "smtp.example.com"
port     = 587
username = "user@example.com"
password = "live-smtp-password"
```

## `templates/contact.html`

```html
{% extends "base.html" %}
{% block title %}Contact · {{ site_name }}{% endblock %}
{% block content %}
  <h1>Contact</h1>
  {% if sent %}
    <p>Message sent. Thanks, {{ name }}!</p>
  {% else %}
    <form method="post" action="/contact">
      <label>Name    <input type="text"  name="name"    required></label>
      <label>Email   <input type="email" name="email"   required></label>
      <label>Message <textarea           name="message" required></textarea></label>
      <button type="submit">Send</button>
    </form>
  {% endif %}
{% endblock %}
```

Same template handles both states — `sent` is absent on GET, `true` after a successful POST.

## New entries in `site.toml`

```toml
[[backend]]
name    = "render-contact"
sockets = "/run/m6/render-contact*.sock"

[[route]]
path    = "/contact"
backend = "render-contact"
```

The socket name is derived from the config filename: `configs/render-contact.conf` → `/run/m6/render-contact.sock`.

## Updated `dev.sh`

```bash
./target/release/render-contact "$SITE" "$SITE/configs/render-contact.conf" &
```

Added alongside the existing m6-html, m6-file, and m6-http lines. The renderer starts and creates its socket, and m6-http finds it when it expands the pool's glob at its own start, so the backends start before m6-http.

## How the loop sends mail

`main` loads the site, builds its state with `load_state` and runs the loop with one `Smtp` slot per connection slot:

1. `Site::load` reads the config (the renderer conf and the secrets overlay), `site.toml` and the templates, once. A start that cannot build its state prints why and exits 2.
2. The loop waits on one poll for the listener, the signals, every client and every mail connection.
3. A `POST /contact` arrives as `Handled::Code(0, req)`. The handler starts the send, and the loop holds the "sent" page and registers the mail socket under the slot's odd token.
4. Each time the mail socket fires, `smtp.drive()` advances the exchange without blocking: `Ok(None)` means keep waiting, `Ok(Some(()))` means the relay accepted the message and the loop sends the held page, and `Err` answers 500 with the cause.
5. A relay that takes longer than its deadline answers 504, and a client that leaves while its message is in flight drops the exchange.

State is a plain value in `main`, owned by the one thread that runs the loop, so it needs no lock. A slow relay holds one client's answer and never the loop.

---

**← [Example 02 — Blog with m6-md](/blog/guide-02-blog)** | **[Example 04 — Login and Protected Pages](/blog/guide-04-auth) →**
