# m6-render (library crate)

Framework for building m6 renderers. Handles the Unix socket server, HTTP parsing, config loading, params file loading, template rendering, thread pool management, and all other infrastructure. Renderer authors write lifecycle functions and handlers.

m6-html is the degenerate case — the trivial renderer with no user state and a one-line main. Every other renderer is a more complex use of the same library.

---

## Lifecycle

Four functions — two for global state, two for per-thread state. All optional except `init_global` when using state.

**`init_global(config)`** — called once at startup and on every reload. Receives the merged config dictionary. Returns global state shared across all threads. Initialise things designed for concurrent access: SMTP transports, HTTP clients, connection pools, read-only data.

**`init_thread(config, &Global)`** — called once per thread when the pool starts, and again on reload. Receives config and a reference to the global state. Returns per-thread state. Initialise things that must not be shared: database connections, prepared statement caches, scratch buffers.

**`handle(req, &Global, &mut ThreadLocal)`** — called per request on whichever thread picks it up. `&mut ThreadLocal` gives exclusive mutable access with no locking — the thread owns it entirely.

**`destroy_thread(ThreadLocal)`** — called on each thread before it exits (shutdown or reload). Takes ownership. Finalise prepared statements, flush write buffers, checkpoint WAL.

**`destroy_global(Global)`** — called after all threads have finished their `destroy_thread`. Takes ownership. Disconnect pools, flush shared caches.

### Full lifecycle sequence

```
startup:
  init_global(config)                    → Global
  init_thread(config, &Global) × N       → ThreadLocal  (one per thread)

per request:
  handle(req, &Global, &mut ThreadLocal) → Result<Response>

reload:
  stop accepting requests
  in-flight requests finish
  destroy_thread(local) × N
  destroy_global(global)
  config reloaded
  init_global(new_config)                → new Global
  init_thread(new_config, &Global) × N   → new ThreadLocals
  resume

shutdown (SIGTERM):
  stop accepting requests
  in-flight requests finish
  destroy_thread(local) × N
  destroy_global(global)
  exit 0
```

`destroy_thread` and `destroy_global` are optional — register via builder, fall back to `Drop` if absent. `init_thread` is optional — threads get `()` as local state if not registered. `init_global` is required when calling `App::with_state`.

### Reload trigger

The framework watches two files via inotify:

- **`site.toml`** — touched by any process that modifies site content (the system-wide sync point)
- **The renderer's own config file** — e.g. `configs/render-cms.conf`

Either file changing triggers the full reload sequence. This means the renderer picks up its own config changes (new routes, updated secrets) independently, and also reloads when site content changes via the site.toml sync point.

Template compilation errors during reload cause the reload to abort — the old state continues serving requests and an error is logged. The renderer does not crash on a bad reload.

---

## Concurrency model

The framework runs a fixed thread pool. Each incoming request is dispatched to an available thread. Multiple requests are handled concurrently — handlers may block on disk I/O, database queries, or network calls without stalling other requests.

**Two tiers of state — each optimised for its access pattern:**

`Global` is shared across all threads. The framework wraps it in `Arc` and clones the Arc once per request — one atomic increment, not a copy of the data. Requires `Global: Send + Sync`. Put things here that are designed for concurrent access or are inherently read-only.

`ThreadLocal` is owned exclusively by one thread. No wrapping, no locking, no atomic operations. Requires only `ThreadLocal: Send` (crosses a thread boundary once at creation). Put things here that must not be shared — database connections, prepared statement caches, scratch buffers.

```rust
struct Global {
    mailer: SmtpTransport,                  // Send+Sync — concurrent use designed in
    client: ureq::Agent,                    // Send+Sync — same
}

struct ThreadLocal {
    db:  rusqlite::Connection,              // one per thread — zero contention
    buf: Vec<u8>,                           // scratch buffer — no allocation per request
}
```

The framework imposes no locks of its own. Synchronisation cost is exactly what the user's design requires — and with per-thread database connections, the common case of a read or write query has no synchronisation at all.

### Thread pool configuration

```toml
# configs/my-renderer.conf  — renderer-specific
[thread_pool]
size       = 8    # threads — default: number of logical CPUs
queue_size = 64   # pending request queue depth — default: size × 8
```

These are framework config keys — consumed by the framework, not merged into the request dictionary. Each renderer process has its own pool; instances of the same renderer are independent.

When all threads are busy and the queue is full, the framework returns 503 to m6-http immediately. m6-http can retry another pool member if available, or return 503 to the client.

---

## Renderers without state

When no user state is needed — m6-html being the degenerate case:

```rust
use m6_render::prelude::*;

fn main() -> Result<()> {
    App::new()
        .route("/blog/{stem}", handle_post)
        .route("/blog", handle_index)
        .run()
}

fn handle_post(req: &Request) -> Result<Response> {
    Response::render("templates/post.html", req)
}

fn handle_index(req: &Request) -> Result<Response> {
    Response::render("templates/post-index.html", req)
}
```

Routes declared in config alone require no code at all — this is how m6-html works:

```rust
fn main() -> Result<()> {
    App::new().run()
}
```

---

## Renderers with state

```rust
use m6_render::prelude::*;
use m6_auth::Db;

// Shared across all threads — Send + Sync required
struct Global {
    mailer: SmtpTransport,
}

// One per thread — Send required, Sync not required
struct ThreadLocal {
    db:  Db,
    buf: Vec<u8>,
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

fn init_thread(config: &Map<String, Value>, _global: &Global) -> Result<ThreadLocal> {
    Ok(ThreadLocal {
        db:  Db::open(config["db_path"].as_str()?)?,
        buf: Vec::with_capacity(4096),
    })
}

fn destroy_thread(local: ThreadLocal) {
    local.db.close().ok();
}

fn handle_signup(req: &Request, global: &Global, local: &mut ThreadLocal) -> Result<Response> {
    let username = req.field("username")?;
    let password = req.field("password")?;
    local.db.user_create(&username, &password, &["user"])?;
    Ok(Response::redirect("/login?registered=1"))
}

fn main() -> Result<()> {
    App::with_state(init_global, init_thread)
        .on_destroy_thread(destroy_thread)
        .route_get("/signup",  |req, _g, _l| Response::render("templates/signup.html", req))
        .route_post("/signup", handle_signup)
        .run()
}
```

`on_destroy_thread` and `on_destroy` are optional — `Drop` handles cleanup for resources that don't need explicit teardown.

The `config` dictionary passed to `init_global` and `init_thread` is the same merged config + secrets dictionary available in every request. The framework makes no distinction between config keys consumed by init and those used by handlers.

---

## Config

The renderer config file (`configs/my-renderer.conf`) is TOML. Framework keys (`global_params`, `[[route]]`, `secrets_file`) are consumed by the framework. All other keys are merged into the config dictionary, passed to `init`, and available in every request dictionary.

```toml
# Framework keys
global_params = ["data/site.json"]
secrets_file  = "/etc/m6/my-renderer.toml"

[[route]]
path     = "/blog"
template = "templates/post-index.html"
params   = ["data/posts.json"]

[[route]]
path     = "/blog/{stem}"
template = "templates/post.html"
params   = ["data/posts.json", "content/posts/{stem}.json"]

# Renderer-specific keys — available in init() and every request dictionary
[smtp]
host     = "localhost"
port     = 1025
from     = "noreply@example.com"
to       = "owner@example.com"
```

TOML sections become nested objects: `config["smtp"]["host"]` in `init`, `req["smtp"]["host"]` in handlers, `{{ smtp.from }}` in templates.

### Secrets file

Points to a TOML file outside the site directory. Merged after the main config — secrets file wins on conflict. All keys land in the same config dictionary. Silently ignored if absent (development fallback values live in the main config). Exit 2 if present but malformed.

```toml
# /etc/m6/my-renderer.toml  (not in version control)
[smtp]
password = "live-smtp-password"

db_path = "/var/www/my-site/data/app.db"
```

The main config has `password = "dev-password"` for local development; the secrets file overrides it in production. `init` reads `config["smtp"]["password"]` without knowing which source it came from.

---

## Request dictionary

`Map<String, Value>` — the same type as the config dictionary. Values are `serde_json::Value`.

Keys are merged in this order, later sources winning on conflict:

1. **Config keys** — all non-framework keys from merged config + secrets
2. **Global params files** — declared in `global_params`, loaded at startup, cached in memory
3. **Route params files** — declared per-route `params`, static files cached, parameterised files via LRU
4. **Path params** — `{stem}`, `{page}` etc. captured from the URL
5. **Query params** — `?page=2&sort=date`
6. **POST form fields** — for POST routes with `application/x-www-form-urlencoded` body
7. **Cookies** — parsed from the `Cookie` header, available under the `cookies` key
8. **Built-in keys** — `site_name`, `site_domain`, `request_path`, `datetime`, `year`
9. **Auth keys** — `auth_username`, `auth_groups`, `auth_roles`, `auth_sub` (from `X-Auth-Claims` if present)
10. **Error keys** — `error_status`, `error_from` (on requests to `[errors] path` only)

### Example request dictionary

For `GET /blog/hello-world?sort=date` from an authenticated user, with `data/site.json` as a global param and `content/posts/hello-world.json` as a route param:

```json
{
  "site_name":    "My Blog",
  "site_domain":  "example.com",
  "smtp": {
    "host": "localhost",
    "port": 1025,
    "from": "noreply@example.com"
  },

  "posts": [...],

  "stem":  "hello-world",
  "title": "Hello World",
  "date":  "2024-01-15",
  "body":  "<p>...</p>",
  "tags":  ["rust", "web"],

  "sort": "date",

  "cookies": {
    "session": "<jwt>",
    "theme":   "dark"
  },

  "request_path": "/blog/hello-world",
  "datetime":     "2024-01-15T10:30:00Z",
  "year":         "2024",

  "auth_username": "alice",
  "auth_groups":   ["editors", "users"],
  "auth_roles":    ["user"],
  "auth_sub":      "user-id-123"
}
```

Keys from later sources override earlier ones on conflict. Config keys are always available as the base layer — `req["smtp"]["host"]` works in any handler regardless of which route matched.

```rust
let title    = req["title"].as_str().unwrap_or("Untitled");
let smtp_to  = req["smtp"]["to"].as_str()?;
let stem     = req["stem"].as_str()?;
let sort     = req["sort"].as_str().unwrap_or("date");
let theme    = req["cookies"]["theme"].as_str().unwrap_or("light");
let username = req["auth_username"].as_str().unwrap_or("anonymous");
```

In templates: `{{ cookies.theme }}`, `{{ auth_username }}`, `{{ smtp.from }}`.

### Params file caching

Static params files (no path params) are loaded at startup and cached in memory. Refreshed on reload.

Parameterised params files (`content/posts/{stem}.json`) are loaded on first access and cached in an LRU. LRU is cleared on reload.

```toml
[params_cache]
size = 256   # default: 256 entries
```

### Raw request metadata

```rust
req.method()           // "GET", "POST", etc.
req.path()             // "/blog/hello-world"
req.header("X-Foo")    // Option<&str>
req.content_type()     // Option<&str>
req.body_raw()         // &[u8]
req.body_json()?       // Value — parses application/json body
req.field("name")?     // single form field, Err(BadRequest) if absent
```

---

## Response

Constructed and returned from the handler. Never written to directly.

```rust
// Template rendering
Response::render("templates/post.html", req)?
Response::render_with("templates/post.html", req, json!({"extra": "value"}))?
Response::render_status("templates/error.html", req, 404)?

// Other
Response::redirect("/login")                        // 302
Response::redirect_permanent("/new-path")           // 301
Response::json(json!({"ok": true}))                 // 200 application/json
Response::json_status(json!({"error": "..."}), 400)
Response::text("hello")                             // 200 text/plain
Response::status(204)                               // no body
Response::not_found()                               // 404
Response::forbidden()                               // 403
Response::bad_request()                             // 400

// Chained modifiers
Response::render("templates/page.html", req)?
    .header("Cache-Control", "no-store")
    .cookie("flash", "", 0)
```

Template paths are relative to the site directory. Tera is the template engine.

---

## App builder

```rust
// No state — handlers receive only &Request
App::new()
    .route("/path",        handler)
    .route_get("/path",    get_handler)
    .route_post("/path",   post_handler)
    .route_patch("/path",  patch_handler)
    .route_delete("/path", delete_handler)
    .run()

// Global state only — handlers receive &Request and &Global
App::with_global(init_global)
    .on_destroy(destroy_global)
    .route_post("/path", handler)
    .run()

// Per-thread state only — handlers receive &Request, &(), &mut ThreadLocal
// Use when thread state is needed but nothing needs sharing across threads
App::with_thread_state(init_thread)
    .on_destroy_thread(destroy_thread)
    .route_post("/path", handler)
    .run()

// Global + per-thread state — handlers receive &Request, &Global, &mut ThreadLocal
App::with_state(init_global, init_thread)
    .on_destroy_thread(destroy_thread)
    .on_destroy(destroy_global)
    .route_post("/path", handler)
    .run()
```

`App::new()`, `App::with_global()`, `App::with_thread_state()`, and `App::with_state()` are the four entry points. The type system enforces the correct handler signature for each — mismatched signatures are compile errors, not runtime failures.

Routes registered in code take precedence over config routes for the same path. A method-specific registration returns 405 for other methods.

### Route matching

Specificity wins: exact paths beat parameterised, longer beats shorter. First declaration breaks ties among equal-specificity patterns (warning logged). Unmatched path → 404.

Path param values are validated before use: alphanumeric, hyphens, and underscores only (plus `.` and `/` for `{relpath}`). No `..` sequences. Invalid values → 400.

---

## Compression

The framework compresses responses per `Accept-Encoding`. Brotli and gzip supported. Configurable per MIME type — same format as m6-html:

```toml
[compression]
"text/html"              = { brotli = 6, gzip = 6 }
"application/javascript" = { brotli = 6, gzip = 6 }
"image/jpeg"             = { brotli = 0, gzip = 0 }
```

Defaults: text types compressed at brotli 6 / gzip 6; images, fonts, PDF not compressed. Custom renderers inherit the same compression behaviour as m6-html with no extra code.

---

## Error handling

Handlers return `Result<Response>`. The framework catches all errors:

| Error | Response |
|---|---|
| `Error::NotFound` | 404 |
| `Error::Forbidden` | 403 |
| `Error::BadRequest(msg)` | 400 |
| Any other `Err` | 500, full error logged |

All error responses pass through `[errors]` processing in m6-http if configured.

---

## File I/O helpers

Always available — no feature flag.

```rust
// All paths relative to site directory
req.read_json("data/posts.json")?          // Value
req.write_json("data/posts.json", &data)?  // overwrites
req.write_json_atomic("data/posts.json", &data)?  // temp + rename
req.list_json("content/drafts/")?          // Vec<Value>
req.site_path("content/posts/hello.json")  // PathBuf — absolute path
req.touch("site.toml")?                    // update mtime
```

---

## Built-in template keys and filters

Available in all Tera templates without any handler code.

**Keys:**
```
{{ site_name }}       {{ site_domain }}    {{ request_path }}
{{ datetime }}        {{ year }}           {{ stem }}
{{ error_status }}    {{ error_from }}
{{ auth_username }}   {{ auth_groups }}    {{ auth_roles }}
```

**Filters:**
```
{{ "hello world" | slugify }}
{{ post.date | date_format(fmt="%B %d, %Y") }}
{{ content | markdown }}
{{ content | truncate_words(n=50) }}
```

---

## Optional features

All disabled by default.

### `email`

SMTP via `lettre`. Initialise the transport in `init`, store in state, use in handlers.

```rust
// In init:
let mailer = SmtpTransport::builder(config["smtp"]["host"].as_str()?)
    .port(config["smtp"]["port"].as_u64()? as u16)
    .credentials(config["smtp"]["username"].as_str()?,
                 config["smtp"]["password"].as_str()?)
    .build()?;

// In handler:
state.mailer.send(
    Message::builder()
        .from(req["smtp"]["from"].as_str()?.parse()?)
        .to(req["smtp"]["to"].as_str()?.parse()?)
        .subject(format!("Contact from {}", req.field("name")?))
        .body(req.field("message")?)?
)?;
```

### `http-client`

Outbound HTTP via `ureq` (sync). Initialise in `init`, store in state.

```rust
// In init:
let client = ureq::Agent::new();

// In handler:
let data: Value = state.client
    .get("https://api.example.com/data")
    .set("Authorization", &format!("Bearer {}", req["api_key"].as_str()?))
    .call()?
    .into_json()?;
```

### `multipart`

File upload parsing.

```rust
let upload = req.file("avatar")?;
// Upload { filename: String, content_type: String, data: Vec<u8> }

// Write to a caller-controlled path — never use upload.filename directly
let dest = format!("assets/avatars/{}.jpg", req["auth_sub"].as_str()?);
req.write_bytes(&dest, &upload.data)?;
```

`upload.filename` is user-supplied and must not be used as a filesystem path — it may contain path traversal sequences or overwrite existing files. The caller always constructs the destination path explicitly.

`req.write_bytes(path, data)` is the low-level write primitive — atomic (temp + rename), path validated within site directory.

Max upload size in config:
```toml
[multipart]
max_size_mb = 10
```

### `flash`

Post-redirect-get flash messages. Stored in a short-lived cookie signed with HMAC-SHA256. The signing key is a dedicated 32-byte secret in the renderer's `secrets_file` — not the auth key.

```toml
# /etc/m6/my-renderer.toml
flash_secret = "base64-encoded-32-random-bytes"   # generate: openssl rand -base64 32
```

```rust
// POST handler — set message, redirect
Ok(Response::redirect("/dashboard").flash("Post published."))

// GET handler — {{ flash }} in template, cleared after first read
```

Flash messages survive exactly one redirect. The framework verifies the HMAC before trusting the cookie — a tampered or forged cookie is silently ignored (no flash shown). Exit 2 at startup if `flash` feature is enabled but `flash_secret` is absent from config.

### `csrf`

Synchroniser token. Not needed for auth forms (`SameSite=Strict` covers those).

```rust
// POST handler
req.verify_csrf()?;
```

Framework injects `{{ csrf_token }}` into every template automatically:

```html
<input type="hidden" name="csrf_token" value="{{ csrf_token }}">
```

---

## Logging

Structured JSON to stdout. `tracing` throughout. Log level from `[log] level` in `site.toml` or `--log-level` flag.

Framework-emitted events:

| Event | Level | Fields |
|---|---|---|
| Startup | info | routes loaded, templates compiled, thread pool size |
| Request complete | info | path, method, matched route, status, latency_us, thread_id |
| Unmatched path | warn | path |
| Params file missing | error | path |
| Handler error (500) | error | path, error message, backtrace |
| Reload triggered | info | — |
| Reload complete | info | elapsed_ms |
| Shutdown | info | — |

User code logs via `tracing::info!`, `tracing::warn!` etc. — captured by the same subscriber.

---

## Signal handling

| Signal | First receipt | Second receipt |
|---|---|---|
| `SIGTERM` or `SIGINT` | Drain queue, finish in-flight requests, `destroy_thread` × N then `destroy_global`, exit 0 | Immediate exit |

On first signal, the framework stops accepting new connections, waits for in-flight requests to complete, calls `destroy_thread` on each thread then `destroy_global`, then exits 0.

---

## Exit codes

| Code | Meaning |
|---|---|
| `0` | Clean shutdown |
| `1` | Runtime error |
| `2` | Config or startup error — bad config, `init` returned `Err`, socket bind failed, template compilation error |

---

## Prelude

`use m6_render::prelude::*` brings into scope:

```rust
// Core types
pub use crate::{App, Request, Response, Error, Result};

// Config and state
pub use serde_json::{json, Map, Value};

// Utility functions available in handlers
pub use crate::util::{slugify, today_iso8601, now_iso8601};

// Optional feature types (only available when feature is enabled)
#[cfg(feature = "email")]
pub use lettre::{Message, SmtpTransport, Transport};

#[cfg(feature = "http-client")]
pub use ureq;

#[cfg(feature = "multipart")]
pub use crate::multipart::Upload;
```

User code rarely needs to import anything beyond the prelude.

---

## Cargo.toml

```toml
[package]
name    = "m6-render"
version = "0.1.0"
edition = "2021"

[lib]
name = "m6_render"

[features]
default     = []
email       = ["dep:lettre"]
http-client = ["dep:ureq"]
multipart   = ["dep:multer"]
flash       = []
csrf        = []

[dependencies]
tera       = "1"
serde      = { version = "1", features = ["derive"] }
serde_json = "1"
toml       = "0.8"
comrak     = { version = "0.21", default-features = false }
slug       = "0.1"
anyhow     = "1"
thiserror  = "1"
tracing    = "0.1"

lettre = { version = "0.11", optional = true, features = ["smtp-transport", "tls-native"] }
ureq   = { version = "2",    optional = true, features = ["json"] }
multer = { version = "3",    optional = true }
```

---

## Workspace

```
m6/
├── Cargo.toml
├── m6-render/          ← this crate (library)
├── m6-html/            ← zero-code binary (5 lines)
├── m6-file/            ← static file server (no m6-render dependency)
├── m6-auth/            ← auth library
├── m6-auth-server/     ← auth server binary
└── m6-auth-cli/        ← auth CLI binary
```

m6-file does not use m6-render — it serves files directly with no template rendering or params loading. It shares socket and HTTP infrastructure, which may be extracted into a lower-level `m6-core` crate if warranted.
