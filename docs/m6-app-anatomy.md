# Anatomy of an m6 app

**Every m6 app has the same shape. This is that shape, written so it can be
followed from a repository that is not this one.**

Companion documents: `m6-core-reference.md` for what each component does, and
`m6-core.md` for why the boundary is where it is.

---

## 1. The whole app

This is `m6-html`, in production on three nodes, complete and unabridged:

```rust
use m6_core::prelude::*;

fn main() -> anyhow::Result<()> {
    App::new().run()?;
    Ok(())
}
```

Four lines. Everything else an app needs comes from `m6-core`: the socket, the
accept loop, HTTP/1.1 parsing and framing, routing, the request dictionary,
config loading, hot reload, compression, minification, logging, and a clean
shutdown. **Linking m6-core is the only thing a new app has to do.**

The manifest is the other half of the claim, and the interesting line is the
dependency count:

```toml
[package]
name    = "my-app"
version = "0.1.0"
edition = "2021"

[[bin]]
name = "my-app"

[dependencies]
m6-core = { path = "../m6-core" }   # see §7 for outside this repo
anyhow  = "1"

[dev-dependencies]
m6-core  = { path = "../m6-core", features = ["testkit"] }
tempfile = "3"
```

Two dependencies. `anyhow` is there only because `main` returns its `Result`.

## 2. The contracts

These are the same for every app, and an app that honours them needs no
bespoke deployment knowledge.

### Invocation

```
my-app  <site_dir>  <config_path>
```

`argv[1]` is the site directory, the root that `Request::site_path` resolves
against and the only tree the app reads. `argv[2]` is its TOML config. Nothing
else is positional, and there are no flags in the service path.

### Environment

| variable | purpose |
|---|---|
| `M6_SOCKET_OVERRIDE` | Bind this unix socket instead of the one derived from the config path. **Tests only.** It is what makes the lifecycle test in §4 work without root or a fixed path. |

### Config

The keys m6-core reads are fixed; everything else in the file is yours and
arrives in the request dictionary. A minimal config:

```toml
[thread_pool]
size       = 2
queue_size = 8

[server]
# Read deadline on an accepted connection. Default 30, `0` disables.
# A silent peer holds a pool worker for exactly this long and no longer.
read_timeout_s = 30

[log]
level  = "info"
format = "json"
```

Framework sections are `[thread_pool]`, `[params_cache]`, `[server]`,
`[compression]`, `[minification]`, `[log]`, plus `[[route]]` entries. Any
other top-level key lands in `RendererConfig::user_config` and is readable
from the request dictionary, which is how an app gets its own settings
without inventing a second config file. `m6-monitor` puts its whole fleet
under `[monitor]` this way.

### Logging

Every app logs three lines under its own name, and core emits them, not the
app:

```
<name> started
<name> shutdown signal received
<name> shutdown complete
```

These are load-bearing rather than decorative. They used to be uneven: two of
five services said "starting" and never "started", `m6-md` said nothing, and
all three render apps logged `m6-render` instead of their own name, so the
journal could not tell you which process had died.

### Shutdown

`App::run()` calls `m6_core::signal::block()` before anything else and installs
the `ShutdownHandle`. An app never does this itself, and the ordering is the
reason: blocking a signal is per-thread, threads inherit the mask at creation,
and `tracing_appender` spawns a writer thread. An app that initialised logging
before blocking would have a thread with SIGTERM unblocked, the kernel would
deliver the process-directed signal there, and the default disposition would
kill the process. That defect ran for thirty days on syd, invisibly, because
systemd counts death by the signal it sent as a clean stop.

On SIGTERM the app drains in-flight requests, removes its socket, and exits 0.
**A socket left behind keeps a dead member in m6-http's backend pool until the
next rescan**, which is why §4 asserts it is gone.

### systemd

The unit is the same shape for every app. `m6-html`'s, with the hardening
fragment omitted:

```ini
[Unit]
Description=my-app
After=network.target

[Service]
Type=simple
User=m6
Group=m6
ExecStart=/usr/local/bin/my-app \
    /var/www/my-site \
    /var/www/my-site/configs/my-app.conf
Restart=on-failure
RestartSec=2
StandardOutput=journal
StandardError=journal
SyslogIdentifier=my-app
RuntimeDirectory=m6
RuntimeDirectoryMode=0750
RuntimeDirectoryPreserve=yes
ReadWritePaths=/run/m6

[Install]
WantedBy=multi-user.target
```

**`ReadWritePaths` must name only what this app's role actually has.** An
absent path fails mount-namespace setup outright with `226/NAMESPACE` and the
unit never starts. Claiming `/run/m6` on a node that has no socket backends
took London off the air for about ninety seconds on 2026-09-11. Use
`RuntimeDirectory` to guarantee what must exist; never a `-` prefix to excuse
an absent path, because tolerance turns a misconfigured node into one that
starts anyway with weaker isolation than intended.

## 3. Adding routes and state

Routes are registered in code, not config:

```rust
App::new()
    .route_get("/", page)
    .route_get("/digest", digest_json)
    .run()?;
```

A handler is `Fn(&Request) -> Result<Response>`. `Response::render(template,
req)` renders; `Response::json`, `::text`, `::html`, `::redirect`,
`::not_found` cover the rest. An app that renders no templates says so and
stops linking Tera:

```rust
App::new().renderer(NoTemplates).route_get("/healthz", |_| Ok(Response::text("ok")))
```

Four entry points, by what state the app needs. Each returns a builder with the
same routing surface:

| entry point | for |
|---|---|
| `App::new()` | no state |
| `App::with_global(init)` | one `G` shared across all workers |
| `App::with_thread_state(init)` | a `T` per worker thread |
| `App::with_state(init_g, init_t)` | both |

The `init` closures receive the config map and return `Result`, so an app that
cannot build its state fails at startup rather than on the first request.

## 4. Testing it

The lifecycle contract is one call. This is the whole of
`m6-monitor/tests/lifecycle.rs`, minus its config constant:

```rust
use std::process::Command;
use m6_core::testkit::{assert_app_lifecycle, binary};

#[test]
fn lifecycle_is_clean() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("app.conf");
    std::fs::write(&config, CONFIG).unwrap();
    let sock = dir.path().join("app.sock");   // keep it short; ~104 byte cap on macOS

    assert_app_lifecycle(
        "my-app",
        Command::new(binary("my-app"))
            .arg(dir.path())
            .arg(&config)
            .env("M6_SOCKET_OVERRIDE", &sock),
        &sock,
    );
}
```

It spawns the binary, waits for the socket, sends SIGTERM, and asserts three
things: the exit status is success rather than a signal, the socket is gone,
and the three lifecycle lines were logged under the app's own name.

**Write this test for every app.** It was opt-in and hand-written in five
separate integration suites, and the sixth app, `m6-monitor`, never got one,
which is how a service reached "tested, ready to deploy" with seventeen unit
tests and nothing that had ever started the binary.

## 5. What you get without asking

Listed because apps have re-implemented several of these before:

- HTTP/1.1 parsing and framing at 32/32 on h1spec, including HEAD with no body
  and correct `Connection` handling.
- Content-coding negotiation with real q-values, so `br;q=0` is honoured.
- Conditional requests: `If-None-Match`, `If-Match`, `If-Unmodified-Since`.
- brotli and gzip, HTML/JS/CSS/JSON minification, both configurable per MIME
  type.
- Config hot reload on write, without a restart.
- Structured logging, JSON or text, reloadable.
- A request dictionary assembled from config, params files, path params, query,
  form body, cookies and built-ins, in a fixed order.

## 6. The apps that are not shaped like this

> **This section describes today, not the target.** The target shape is **one
> thread, one event loop, a switch over connection state, everything
> non-blocking, and anything that must block on its own sync thread signalling
> the loop through an fd.** m6-http is already that shape; the `App` services
> are not. `m6-app-shape-plan.md` is the route. Read this section as a snapshot
> of a system mid-migration.

**There is one app family and one edge. Everything else is unfinished.**

| service | uses `App`? | is the difference real? |
|---|---|---|
| `m6-html`, `m6-monitor`, `render-analytics`, `render-contact`, `render-cms` | yes | This is the shape. |
| `m6-http` | no | **Yes.** The edge. |
| `m6-file` | no | **No.** Owed work. |
| `m6-auth-server` | no | **No.** Owed work. |

**`m6-http` is genuinely one of a kind.** It binds public TCP and UDP,
terminates TLS, HTTP/2 and HTTP/3, proxies to backends over unix sockets and
h2c, and owns the response cache. `App` is a unix-socket backend loop; m6-http
is what those backends sit behind. It will not become an `App` service and
should not.

**`m6-file` and `m6-auth-server` are not a second family**, and this document
said they were until it was measured:

- `m6-file`'s stated reason is that it needs its own `poll(2)` loop to wait on
  the config-watcher fd and the listener together. `App` already does that, at
  `app.rs:1759`. Diffed against it, m6-file's loop is **22 of 33 lines
  byte-identical**, and the rest are the same expressions with renamed locals.
  It also already calls `m6_core::server::serve_connection` for every
  connection. It is core's loop with a hand-copied wrapper.
- `m6-auth-server` binds through `m6_core::server::UnixServer` directly. The
  only capability it needs that `App` does not offer is `chmod 0666` on the
  socket, which is a config key rather than an architecture.

Closing both is owed work. **`m6-app-shape-plan.md` is what it takes**: five
core enhancements, two of which fix live defects in services that are already
the right shape, and a sequence that keeps the hot path untouched. Until then,
note that the divergence is historical: neither is a divergent
*implementation*, both assemble the same m6-core parts in a different order, so
the risk is drift rather than disagreement today.

One thing that plan establishes and this document should not understate:
**migrating those two onto `App` as it stands today would delete the only two
read timeouts in the fleet.** They each set 30 seconds by hand; `App` sets
none. One shape has to mean the shape absorbs what they knew.

`m6-md` and `m6-auth-cli` are CLI tools, not services, and are outside this
document.

**If you are writing a new app, you are writing an `App` service.** There is no
second shape to choose from.

## 7. Using m6-core from another repository

**This is the part that is not yet trivial, and saying so is the point of this
section.**

Today the site's renderers depend on core by filesystem path:

```toml
m6-core = { path = "../../m6/m6-core" }
```

That is a hard-coded directory layout across a repository boundary with no
version constraint. It works only if a checkout of `m6` sits next to your
checkout at exactly that relative position, it pins nothing, and there is no
way to say "the version I tested against".

Phase 7 of the migration replaces it with a git dependency pinned to a
revision:

```toml
m6-core = { git = "https://github.com/mgrosvenor/m6", rev = "<sha>" }
```

**Phase 7 is not done.** Until it is, an app outside this repository needs the
sibling checkout, and this document cannot honestly promise otherwise. The gate
for calling it done is that the site builds with no `m6` checkout beside it and
`deploy.sh` stops syncing the tree.
