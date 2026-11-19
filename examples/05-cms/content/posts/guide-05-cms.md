+++
title   = "Guide 05: CMS Blog"
date    = "2025-01-03"
summary = "The full system: a public blog cached in RAM, an auth-protected CMS renderer, and a publish flow that advances the site's generation to go live."
tags    = ["guide", "tutorial", "cms", "custom-renderer"]
cover   = "https://picsum.photos/seed/guide05/1200/500"
+++

**Guide series:**
[Prerequisites](/blog/guide-00-prerequisites) →
[Example 01](/blog/guide-01-static-site) →
[Example 02](/blog/guide-02-blog) →
[Example 03](/blog/guide-03-contact-form) →
[Example 04](/blog/guide-04-auth) →
**Example 05 — CMS Blog** →
[Example 06](/blog/guide-06-systemd) →
[Example 07](/blog/guide-07-dev-to-production)

---

`examples/05-cms/` is the full system. A public blog served at maximum speed from the m6-http RAM cache. An auth-protected CMS renderer for creating, editing, and publishing posts. No database — posts are JSON files on disk.

```bash
cd examples/05-cms
./setup.sh           # keys, first admin user
cargo build --release -p render-cms
./dev.sh
# CMS at https://localhost:8443/cms (login required)
# Blog at https://localhost:8443/blog (public, cached)
```

## Site directory

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
│   ├── posts/     ← published JSON (one file per post)
│   └── drafts/    ← draft JSON (never publicly routable)
├── keys/
└── render-cms/
    ├── Cargo.toml
    └── src/main.rs
```

## `site.toml`

```toml
[site]
name   = "My Blog"
domain = "localhost"

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

# ── Public routes — no auth, fully cached ────────────────────────

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

# ── Auth endpoints — public ──────────────────────────────────────

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

# ── CMS — all protected, never cached ───────────────────────────

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

[[route]]
path    = "/api/posts/{stem}"
backend = "render-cms"
require = "group:editors"
```

`[[route_group]]` with `content/posts/*.json` handles routing for individual posts. m6-http expands the glob when it builds its route table at start. A post file a publish adds is routed because the publish ends with `POST /genid`, and `site.toml` itself is read once, at start: an edit to it takes a restart.

`site.toml` has no `[auth]` because the backend it names belongs to one node. It goes in the system config.

## `configs/system-dev.toml`

```toml
[server]
bind     = "127.0.0.1:8443"
tls_cert = "@M6_DEV_HOME@/dev/dev.pem"
tls_key  = "@M6_DEV_HOME@/dev/dev-key.pem"

[auth]
backend = "m6-auth-check"

[genid]
read_require  = "role:genid-read"
write_require = "role:genid-write"

[node]
name = "dev"
```

`[auth]` names the backend m6-http asks who a caller is: the check socket of m6-auth, declared as the `m6-auth-check` backend in `site.toml` (`sockets = "/tmp/m6/check-m6-auth*.sock"`). `[genid]` makes this node the source of the site's generation ID and names the claims that gate `/genid`: a token with the `genid-write` role may publish. Declare each table in one file only. A key in the file that does not own it stops m6-http at start with exit 2.

## `render-cms/Cargo.toml`

```toml
[package]
name    = "render-cms"
version = "0.1.0"
edition = "2021"

[dependencies]
m6-core        = { path = "../../../../m6/m6-core" }
serde_json     = "1"
comrak         = { version = "0.21", default-features = false }
base64         = "0.22"
rustls-pemfile = "2"
```

`m6-core` is the one m6 crate a service links. It supplies routing, the request dictionary, templates and the pieces of an event loop. The service writes its own `main`, which is the loop, and its handlers.

## `render-cms/src/main.rs`

The routes are a constant, in the order the `match` below reads them. `Site::load` compiles them at start, and each request that one of them answers comes back as `Handled::Code(index, req)`:

```rust
const ROUTES: &[CodeRoute] = &[
    CodeRoute { pattern: "/contact",              method: RouteMethod::Get },
    CodeRoute { pattern: "/contact",              method: RouteMethod::Post },
    CodeRoute { pattern: "/cms",                  method: RouteMethod::Get },
    CodeRoute { pattern: "/cms/new",              method: RouteMethod::Get },
    CodeRoute { pattern: "/cms/edit/{stem}",      method: RouteMethod::Get },
    CodeRoute { pattern: "/api/drafts",           method: RouteMethod::Post },
    CodeRoute { pattern: "/api/drafts/{stem}",    method: RouteMethod::Patch },
    CodeRoute { pattern: "/api/publish/{stem}",   method: RouteMethod::Post },
    CodeRoute { pattern: "/api/unpublish/{stem}", method: RouteMethod::Post },
    CodeRoute { pattern: "/api/drafts/{stem}",    method: RouteMethod::Delete },
    CodeRoute { pattern: "/api/posts/{stem}",     method: RouteMethod::Delete },
];
```

`main` is the service's own event loop: start, run, stop. It loads the site, binds the socket, opens the signal descriptor, and then waits on one poll for the listener, the signals and every connection. Nothing in the run phase blocks. `docs/m6-app-anatomy.md` in the m6 repository walks through the loop line by line, and `main.rs` in this example is that loop with one addition: a publish waits on a `POST /genid`, which the same loop drives.

A handler answers `Answer::Now(response)`, or `Answer::AfterPublish(response)` when it changed the site's files. The loop holds an `AfterPublish` answer, sends `POST /genid` to m6-http on the connection slot's own client, and writes the answer when m6-http has replied:

```rust
fn handle_publish(req: &Request) -> Result<Answer> {
    let stem         = req["stem"].as_str().ok_or(Error::NotFound)?;
    let draft_path   = format!("content/drafts/{}.json", stem);
    let publish_path = format!("content/posts/{}.json", stem);

    let mut post = req.read_json(&draft_path)?;
    post["stem"]         = json!(stem);
    post["path"]         = json!(format!("/blog/{}", stem));
    post["published_at"] = json!(now_iso8601());

    req.write_json_atomic(&publish_path, &post)?;
    update_index(req)?;
    let _ = fs::remove_file(req.site_path(&draft_path));

    Ok(Answer::AfterPublish(Response::json(json!({"published": true, "path": post["path"]}))))
}

fn handle_unpublish(req: &Request) -> Result<Answer> {
    let stem         = req["stem"].as_str().ok_or(Error::NotFound)?;
    let publish_path = format!("content/posts/{}.json", stem);
    let draft_path   = format!("content/drafts/{}.json", stem);

    let mut post = req.read_json(&publish_path)?;
    post["draft"] = json!(true);
    req.write_json(&draft_path, &post)?;
    let _ = fs::remove_file(req.site_path(&publish_path));
    update_index(req)?;

    Ok(Answer::AfterPublish(Response::json(json!({"unpublished": true}))))
}
```

The handlers do not touch `site.toml`: m6 services read their files once, at start, and a touch reloads nothing. The `POST /genid` is what makes every backend read the new content.

## `configs/render-cms.conf`

```toml
global_params = ["data/site.json"]

[[route]]
path  = "/cms"
cache = "no-store"
# ... one entry per route above

[genid]
url        = "https://localhost:8443/genid"
connect    = "127.0.0.1:8443"
token_file = "@M6_DEV_HOME@/05-cms/genid-write.token"
ca_file    = "@M6_DEV_HOME@/dev/ca.pem"
timeout_ms = 10000
```

`[genid]` names the publish URL, the address to connect to, the file holding a token with the `genid-write` role (`dev.sh` mints it with `m6-auth-cli ... token create`), the certificate to trust (a development node signs its own; in production leave `ca_file` out and the system's certificate store is used) and how long a publish may take. A missing or empty token file stops the renderer at start.

## CMS API

| Method | Path | Action |
|---|---|---|
| `POST` | `/api/drafts` | Create draft → returns `{"stem":"..."}` |
| `PATCH` | `/api/drafts/{stem}` | Update draft fields |
| `DELETE` | `/api/drafts/{stem}` | Delete draft permanently |
| `POST` | `/api/publish/{stem}` | Publish draft → moves to posts, then `POST /genid` |
| `POST` | `/api/unpublish/{stem}` | Move published post back to drafts |
| `DELETE` | `/api/posts/{stem}` | Delete published post permanently |

The dashboard shows **Edit / Delete** for drafts and **View / Edit / Unpublish / Delete** for published posts. The editor defaults the date field to today for new posts.

## The publish flow

1. Editor saves draft → `POST /api/drafts` or `PATCH /api/drafts/{stem}` writes `content/drafts/{stem}.json`
2. Editor clicks **Publish** → `POST /api/publish/{stem}`:
   - render-cms renders markdown → HTML, writes `content/posts/{stem}.json`
   - render-cms rebuilds `data/posts.json` index
   - render-cms sends `POST /genid` to m6-http, which reloads every backend, flushes its cache and advances the generation — `/blog/{stem}` becomes live
   - the answer to the editor waits for that request, so a post reported as published is live
   - render-cms deletes `content/drafts/{stem}.json`
3. Browser goes to `/blog/{stem}`
4. m6-http: cache empty after the flush, cache miss → m6-html renders → m6-http caches
5. Every subsequent request to `/blog/{stem}`: cache hit — RAM only

<pre class="mermaid">
sequenceDiagram
    participant Editor
    participant rcms as render-cms
    participant Disk
    participant mhttp as m6-http
    participant mhtml as m6-html
    participant Browser

    Editor->>rcms: PATCH /api/drafts/my-post (save draft)
    Editor->>rcms: POST /api/publish/my-post
    rcms->>Disk: write content/posts/my-post.json
    rcms->>mhttp: POST /genid (write token)
    mhttp->>mhtml: reload
    mhttp-->>rcms: 200, new generation (cache flushed)
    rcms->>Editor: {"published": true}
    Editor->>Browser: open /blog/my-post
    Browser->>mhttp: GET /blog/my-post
    mhttp->>mhtml: cache miss, render
    mhtml->>mhttp: response
    mhttp->>Browser: page (now cached)
</pre>

## Performance profile

| Request type | Latency |
|---|---|
| Public post (cached) | < 1 ms — RAM only |
| Public post (cache miss) | ~5 ms — m6-html reads JSON, renders, caches |
| CMS dashboard | one check of the token by m6-auth, then render-cms reads the posts and drafts directories |
| Publish | one atomic file write, then one `POST /genid` whose answer the editor waits for |

The public blog path never touches disk after the first request.

## `dev.sh`

```bash
#!/bin/bash
set -e
SITE="$(cd "$(dirname "$0")" && pwd)"
trap 'kill $(jobs -p) 2>/dev/null' EXIT

# keys, auth database and the publisher token (examples/dev-token.sh)
m6-html        "$SITE" "$SITE/configs/m6-html.conf" &
m6-file        "$SITE" "$SITE/configs/m6-file.conf" &
m6-auth-server "$SITE" "$(m6_conf "$SITE/configs/m6-auth.conf")" &   # login and check sockets
"$EXAMPLES/target/release/render-cms" "$SITE" "$(m6_conf "$SITE/configs/render-cms.conf")" &
# the source node writes its generation ID under STATE_DIRECTORY
STATE_DIRECTORY="$M6_DEV_HOME/05-cms/state" m6-http "$SITE" "$(m6_conf "$SITE/configs/system-dev.toml")" &

echo "Running at https://localhost:8443"
echo "CMS at    https://localhost:8443/cms"
wait
```

---

**← [Example 04 — Login and Protected Pages](/blog/guide-04-auth)** | **[Example 06 — Production with systemd](/blog/guide-06-systemd) →**
