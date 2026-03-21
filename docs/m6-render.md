# m6-render

Two built-in renderer binaries: m6-html and m6-file. Both use the m6-render library. Custom renderers use the same library — see `m6-render-lib.md`.

---

## m6-html

The degenerate renderer. Loads params files, merges built-in keys, renders a Tera template. No custom handler logic. Its entire implementation:

```rust
fn main() -> Result<()> {
    App::new().run()
}
```

Everything else — route matching, params loading, key merging, template rendering, compression, error handling, logging — is the framework.

### CLI

```
m6-html <site-dir> <config-path>
```

Socket path derived from config filename: `configs/m6-html.conf` → `/run/m6/m6-html.sock`. Named instances: `configs/m6-html-2.conf` → `/run/m6/m6-html-2.sock`. Rule: basename, strip final extension.

`--log-level debug` optional. Logs structured JSON to stdout.

### Config

```toml
# configs/m6-html.conf

global_params = ["data/site.json"]

[[route]]
path     = "/"
template = "templates/home.html"
params   = ["content/pages/index.json"]

[[route]]
path     = "/blog"
template = "templates/post-index.html"
params   = ["content/posts/_index.json"]

[[route]]
path     = "/blog/{stem}"
template = "templates/post.html"
params   = ["content/posts/{stem}.json"]

[[route]]
path     = "/_errors"
template = "templates/error.html"
params   = []
cache    = "no-store"
```

Full config key reference: see `m6-render-lib.md`.

### Compression

Compresses per `Accept-Encoding`. Brotli and gzip. Configurable per MIME type:

```toml
[compression]
"text/html"              = { brotli = 6, gzip = 6 }
"application/javascript" = { brotli = 6, gzip = 6 }
"image/jpeg"             = { brotli = 0, gzip = 0 }
```

Defaults — compressed: `text/html`, `text/css`, `application/javascript`, `image/svg+xml`, `text/plain`. Not compressed: fonts, images, PDF, everything else.

### Logging

Structured JSON to stdout. Events: startup (routes loaded, templates compiled), request complete (path, matched route, status, latency_us), missing params file (error), unmatched path (warn).

---

## m6-file

Serves files from the filesystem. Does not use the m6-render library — no templates, no params, no thread pool. Uses a dedicated minimal socket/HTTP server sharing the same Unix socket conventions and CLI interface as m6-html. Implemented directly in its own crate (`m6-file/`).

### CLI

```
m6-file <site-dir> <config-path>
```

Socket path derived by the same rule as m6-html.

### Config

```toml
# configs/m6-file.conf

[compression]
"text/css"               = { brotli = 6, gzip = 6 }
"application/javascript" = { brotli = 6, gzip = 6 }
"font/woff2"             = { brotli = 0, gzip = 0 }
"image/jpeg"             = { brotli = 0, gzip = 0 }

[[route]]
path = "/assets/{relpath}"
root = "assets/"

[[route]]
path = "/content/posts/{stem}/{filename}"
root = "content/posts/{stem}/"
```

**Per `[[route]]`:**

| Key | Type | Required | Notes |
|---|---|---|---|
| `path` | string | yes | URL pattern |
| `root` | string | yes | Filesystem root, relative to site directory. Path params may be used. |

### Path resolution

URL suffix after matched pattern appended to `root` after expanding path params.

`GET /assets/css/main.css` → `root = "assets/"`, `relpath = "css/main.css"` → `assets/css/main.css`.

### Path traversal protection

Resolved path must stay within `root`. `..` sequences return 404. Path param values: alphanumeric, hyphens, underscores, `.`, `/` only. Symlinks not followed outside root.

### Cache-Control

Always `Cache-Control: public`.

### Logging

Structured JSON to stdout. Events: startup, request complete (path, status, bytes, latency_us), traversal attempt (warn), file not found (debug).

---

## Signal handling (both binaries)

| Signal | First receipt | Second receipt |
|---|---|---|
| `SIGTERM` or `SIGINT` | Finish current request, close socket, exit 0 | Immediate exit |

## Exit codes (both binaries)

| Code | Meaning |
|---|---|
| `0` | Clean shutdown |
| `1` | Runtime error |
| `2` | Config or usage error — before binding |
