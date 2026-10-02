# Anatomy of an m6 app

This document answers how.
How an m6 app is built, what it promises, and what it gets for promising it.

`m6-architecture.md` §7, "why every service has one shape", answers why there is one shape at all, and `m6-core-reference.md` names every component and its interface.
This document is the shape itself, written so it can be followed from a repository that is not this one.

An app is two files and one dependency, and §2 shows both in full.
§3 to §6 give the four contracts every service honours: the command line, the configuration, the routes and the lifecycle.
§7 is the test that proves the lifecycle contract in one call, §8 lists what a service gets without asking, and §9 covers building against `m6-core` from another repository.
§10 summarises.

Where this document and the code disagree, the code is right and this document is a defect.

## Contents

1. [What an app is](#1-what-an-app-is)
2. [The two files](#2-the-two-files)
3. [The command line](#3-the-command-line)
4. [The configuration](#4-the-configuration)
5. [Routes, handlers and state](#5-routes-handlers-and-state)
6. [The lifecycle](#6-the-lifecycle)
7. [Proving the lifecycle in one call](#7-proving-the-lifecycle-in-one-call)
8. [What a service gets without asking](#8-what-a-service-gets-without-asking)
9. [Building against m6-core from another repository](#9-building-against-m6-core-from-another-repository)
10. [Summary](#10-summary)

---

## 1. What an app is

**An m6 app is a Unix socket server that links `m6-core` and nothing else.**
Core owns the socket, the accept loop, HTTP/1.1, routing, the request dictionary, configuration, hot reload, compression, minification, logging and the shutdown sequence.
What is left for a service to write is its routes and their bodies.

Every m6 service except the edge is an app in this sense.
`m6-http` faces the internet, terminates TLS, HTTP/2 and HTTP/3, owns the response cache and proxies to these services over their sockets, which is a different job and a different shape.
`m6-md` and `m6-auth-cli` are command line tools and hold no socket at all.

Four documents divide this subject, and Table 1 says which one to open.

| document | what it owns |
|---|---|
| `m6-architecture.md` §7, "why every service has one shape" | why there is one shape, and why startup, shutdown and the bounded queue belong to core |
| `m6-core-reference.md` | every module, every feature flag, and the full interface of `Request` and `Response` |
| this document | the contract a service honours, and the code that honours it |
| `m6-backend-protocol.md` | the wire contract the edge requires of any backend, in any language |

**Table 1: where each part of the subject is written down.**
Take from it that this document holds the shape and the contract, and that a question of the form "why is it like this" is answered in `m6-architecture.md` §7, "why every service has one shape", rather than here.

The rest of this document is that contract in four parts, and §2 starts with the smallest complete app in the repository.

---

## 2. The two files

**An app is a `main.rs` and a `Cargo.toml`.**
Six lines of source and one dependency, both shown here in full.

`m6-html` is complete and unabridged below, and it is what a real deployment runs.

```rust
use m6_core::prelude::*;

fn main() -> anyhow::Result<()> {
    App::new().run()?;
    Ok(())
}
```

That serves every HTML page a site has, at every route its configuration declares, with templates, compression, minification and hot reload.
Linking `m6-core` is the only thing a new app has to do.

The manifest is the other half of the claim.

```toml
[package]
name    = "my-app"
version = "0.1.0"
edition = "2021"

[[bin]]
name = "my-app"

[dependencies]
m6-core = { path = "../m6-core" }   # see §9 for outside this repository

[dev-dependencies]
m6-core  = { path = "../m6-core", features = ["testkit"] }
tempfile = "3"
```

One dependency.
`m6-html` also carries `anyhow` because its `main` returns `anyhow::Result`.
A `main` returning `m6_core::prelude::Result` needs nothing beyond core, and `m6-http/tests/backends/rust-m6core` is a five-route service whose whole manifest is that one `m6-core` line.

Two feature decisions are made in the manifest.
`templates` is on by default and pulls tera and comrak, so a service that renders no templates sets `default-features = false` and links neither.
`testkit` goes in `[dev-dependencies]` only, which is what keeps the test harness out of every release binary.
`m6-core-reference.md` §2, "feature flags", is the full feature table and this document does not repeat it.

Those two files and one dependency are the whole of the build.
Everything from §3 on is what the resulting binary promises.

---

## 3. The command line

**A service is configured by files and started with two paths.**
This section gives both, the four flags core parses before it starts, and the exit code a supervisor reads.

```
my-app  <site_dir>  <config_path>  [--log-level LEVEL] [--dump-config] [--version]
```

`argv[1]` is the site directory, which is the root `Request::site_path` resolves against and the only tree the service reads.
`argv[2]` is its TOML configuration.
A call with fewer than two positional arguments prints the usage line and exits 2.

Table 2 gives the flags, all four handled in `parse_invocation` before any socket is bound.

| flag | what it does |
|---|---|
| `--log-level LEVEL` | overrides both configuration sources for the level. §4 gives the precedence |
| `--dump-config` | loads the configuration exactly as the service would, prints what it resolved to with every route and what would answer it, and exits |
| `--version`, `-V` | prints the program's own name from `argv[0]` and the version, and exits 0. Takes no site directory and no configuration, so a freshly installed binary can be asked what it is before any configuration is in place |

**Table 2: the flags core parses, and what each one is for.**
Take from it that `--dump-config` is the only one that reads the configuration, and that `--version` is the only one that runs without it.

**`--dump-config` exits 0 when this binary can serve this configuration and 2 when it cannot**, which makes it a validation step a deploy can run against a new binary before installing it.
It names every route that nothing can serve, and a route that nothing can serve is invisible at runtime because it answers 404 like any other unmatched path.
Every service loads its configuration through the same function, so the check covers all of them.

Table 3 gives the exit codes, which are `m6-backend-protocol.md` §8.3, "exit codes", and the same for every backend in any language.

| code | meaning |
|---|---|
| 0 | clean shutdown |
| 1 | runtime error, including a failure to initialise logging |
| 2 | configuration or usage error, detected before the socket is bound |

**Table 3: the three exit codes, from `m6-backend-protocol.md` §8.3, "exit codes".**
Take from it that 2 means "failed before binding", which is what lets a supervisor tell a misconfiguration from a crash.

Six conditions exit 2, and all six are configuration the service refuses to start on.
A configuration that does not load, a startup error building the framework state, a socket that cannot be bound, a `[[route]]` naming a handler this binary does not register, a `[compression]` setting that contradicts what `site.toml` declares about this backend, and the `flash` feature enabled with no `flash_secret`.
"Does not load" covers a file that does not parse and a key set both in the configuration and in its secrets file.

Those paths are decided before the socket exists, which is why §4 comes next: the configuration is what every one of them is reading.

---

## 4. The configuration

**Core reserves twelve top-level keys and hands the service everything else.**
This section gives the reserved keys and their defaults, the name that ties the configuration file to the socket and to `site.toml`, and what a reload changes.

Table 4 is the reserved set, and a key in it never reaches a template.

| key | read by | what it sets |
|---|---|---|
| `[thread_pool]` | core | `size`, default the CPU count. `queue_size`, default eight times the size |
| `[params_cache]` | core | `size`, default 256 entries |
| `[server]` | core | `read_timeout_s`, default 30, `0` disables. `socket_mode`, an octal string as systemd writes it, default `"0660"` |
| `[compression]` | core | brotli and gzip levels per MIME type |
| `[minification]` | core | on or off per MIME type, plus `inline_js` |
| `[log]` | core | `level`, default `info`. `format`, default `json` |
| `[[route]]` | core | the configuration route table, §5 |
| `global_params` | core | JSON files merged into every request dictionary |
| `secrets_file` | core | a second TOML file merged with this one, so a secret stays out of the tracked configuration. One key, one owner: a key set in both files is refused with an error naming every clash |
| `flash_secret` | core, feature `flash` | the HMAC key for flash cookies. Absent with the feature on is exit 2 |
| `[errors]` | `m6-http`, in `site.toml` | reserved, so it does not reach the request dictionary. The section itself belongs to `site.toml` |
| `[multipart]` | nothing | reserved and read by no code in this repository. `m6-render-lib.md` documents `max_size_mb` under it and nothing enforces it |

**Table 4: the twelve reserved keys, who reads each, and its default.**
Take from it that every value core reads has a working default, and that `[server]` refuses a value it cannot parse, because a service that started with a deadline nobody chose looks healthy.

Every other top-level key lands in `RendererConfig::user_config` and is readable from the request dictionary.
That is how a service gets its own settings without inventing a second configuration file, and `m6-monitor` puts its whole fleet under `[monitor]` this way.

The logging level has three sources and the last one wins: `site.toml`'s `[log]`, then this file's `[log]`, then `--log-level`.
The format has the first two.

### 4.1 One name in three places

**The configuration file's stem is the service's name, its socket's name, and its name in `site.toml`.**
Nothing derives that from the binary or from a configuration key, so a renamed configuration file moves the socket and the edge stops finding it.

Figure 1 is the chain.

```
   <any-directory>/my-app.toml          argv[2], the configuration file
        |
        |  file stem
        v
     "my-app"
        |
        +---------------> /run/m6/my-app.sock       the socket this service binds
        |
        +---------------> site.toml
                             [[backend]]
                             name    = "my-app"
                             sockets = "/run/m6/my-app*.sock"
```

**Figure 1: the configuration file's stem determines the socket path and the backend name.**
Take from it that the three names are one string, and that core reads only the stem, so the directory the configuration lives in is the operator's choice.

The glob in `site.toml` exists so a pool can have more than one member, `my-app-1.sock` and `my-app-2.sock`, one per systemd instance.
`m6-site-toml.md` §`[[backend]]` gives the rest of that section.
`M6_SOCKET_OVERRIDE` replaces the derived path outright, which is what lets a test bind a socket in a temporary directory with no privileges and no fixed path.
It is read from the environment in every release binary, and the tooling in `tools/` sets it as well as the test suites.

The name also carries one agreement that core checks at startup.
`site.toml` declares `[[backend]] compresses` for this name, and a service whose `[compression]` disagrees with that declaration exits 2 with a message saying which way.
A service that compresses where `site.toml` says it does not lets the edge cache a compressed body and replay it to a client that asked for identity, and the reverse fragments every downstream cache on a header that cannot change the body.

### 4.2 What a reload changes

Core watches the directories holding the configuration file and `site.toml`, and reloads within milliseconds of a write.
Where no change descriptor is available it falls back to polling the modification times at roughly one second.
Table 5 says what that reload reaches.

| reloaded on a write | fixed until a restart |
|---|---|
| routes, both the code-bound set and the configuration-declared one | `[thread_pool]` size and queue, read once at startup |
| templates | `[server] read_timeout_s`, applied to a socket at accept time |
| the base request dictionary: `global_params`, static params files and the service's own keys | `[server] socket_mode`, applied to the socket once after binding |
| compression and minification levels | `[log]` level and format |

**Table 5: what a configuration write changes, and what needs a restart.**
Take from it that everything about a request is reloadable and everything about the process is not, with one exception: `[log]` is a process setting that core can reload and an app never does.

`m6_core::log::init` returns a `LogHandle` whose `reload` changes the level, and `m6-http` calls it on every configuration change.
An `App` service binds that handle to `_log_guard` and never calls it, so editing `[log] level` in an app's configuration and waiting for the reload changes nothing.

A reload that fails is refused whole.
A configuration that does not parse, a template that does not build, and a route naming an unregistered handler each leave the previous state serving and log the reason, which is what makes a typo in a live configuration recoverable.

The route table is the part of that configuration a service writes code against, and §5 is how the two meet.

---

## 5. Routes, handlers and state

**A route is bound in code or declared in configuration, and a handler is always code.**
This section gives both bindings, the four builders that differ only in what state a handler receives, and the one asymmetry between them.

### 5.1 Routes bound in code

```rust
App::new()
    .route_get("/", page)
    .route_get("/digest", digest_json)
    .run()?;
```

`route_get`, `route_post`, `route_put`, `route_patch` and `route_delete` bind one method, and `route` binds any.
A handler is `Fn(&Request) -> Result<Response>`.
`Response::render(template, req)` renders, and `json`, `text`, `html`, `status`, `redirect`, `not_found`, `forbidden` and `bad_request` cover the rest, with `body`, `header`, `cookie` and `verbatim` as builders over any of them.
`m6-core-reference.md` §5, "request and response", has the full set.

A service that renders no templates says so, and then links no template engine.

```rust
use m6_core::render::NoTemplates;   // at the crate root too, and not in the prelude

App::new().renderer(NoTemplates).route_get("/healthz", |_| Ok(Response::text("ok")))
```

The set of code routes is fixed for the life of the process, so a reload cannot add one.

### 5.2 Routes declared in configuration

**A service whose routes are part of its deployment registers a handler by name and lets configuration bind it to a path.**
A static file service gains an asset tree by being told about a directory.

```rust
App::new().handler("files", handler::serve).run()?;
```

```toml
[[route]]
path    = "/assets/{*relpath}"
handler = "files"
root    = "assets/"
```

That is `m6-file` in full, and its `main.rs` is 34 lines of which 24 are a comment.
Configuration routes are recompiled on every reload, so adding, changing or removing one takes effect without a restart.
A route naming a handler that was never registered exits 2 at startup and is refused at reload.

Table 6 gives the keys a `[[route]]` may carry.

| key | what it does |
|---|---|
| `path` | the pattern. `{name}` captures one segment, `{*name}` captures a segment and every one after it |
| `handler` | the name of a handler registered with `App::handler` |
| `template` | a template to render, which needs no code at all |
| `params` | JSON files merged into this route's request dictionary. A path holding `{placeholder}` resolves per request |
| `status` | the status a template route answers with, default 200 |
| `cache` | `public` or `no-store`. A handler route defaults to `no-store` and a template route to `public` |
| `methods` | the methods this route answers |
| `headers` | extra response headers, as `[[key, value]]` pairs |
| anything else | kept verbatim and read by the handler through `Request::route_setting` |

**Table 6: the eight keys core defines on a `[[route]]`, and what happens to a ninth.**
Take from it that the defaults for `cache` depend on the kind of route, because letting a handler route inherit `public` would put computed output in a shared cache by omission, and that core deliberately does not know what `root` means.

Where a route has both a template and a code handler on the same pattern, the template wins.
That is what stops a POST handler registered in code from shadowing a GET route the configuration declares at the same path.

### 5.3 State

Four builders differ only in what state a handler receives, and Table 7 gives all four.

| builder | state | initialiser | handler |
|---|---|---|---|
| `App::new()` | none | none | `Fn(&Request) -> Result<Response>` |
| `App::with_global(init)` | one `G` shared by every worker | `Fn(&AppContext) -> Result<G>` | `Fn(&Request, &G) -> Result<Response>` |
| `App::with_thread_state(init)` | a `T` owned by each worker | `Fn(&Map<String, Value>, &()) -> Result<T>` | `Fn(&Request, &mut T) -> Result<Response>` |
| `App::with_state(init_g, init_t)` | both | `Fn(&AppContext) -> Result<G>` and `Fn(&Map<String, Value>, &G) -> Result<T>` | `Fn(&Request, &G, &mut T) -> Result<Response>` |

**Table 7: the four builders, with the initialiser and handler signature of each.**
Take from it that per-thread state is handed to a handler as `&mut` and shared state as `&`, which is the whole difference: a value owned by one worker needs no synchronisation, and a value shared across workers costs one atomic increment per request and no copy.

An initialiser returns `Result`, so a service that cannot build its state exits 2 at startup.
`on_destroy` and `on_destroy_thread` register the reverse, for state that owns something worth closing.

**Only the global initialiser receives an `AppContext`.**
`AppContext` carries the parsed configuration, the site directory and the configuration file's path, because a configuration that names files has to resolve them against the same roots the rest of the service uses.
A per-thread initialiser receives the configuration map and the global value, so per-thread state whose configuration names a file has no site directory to resolve it against.

`m6-auth-server` is the worked example: 122 lines, one global holding the database, the signing keys, the token lifetimes and a rate limiter, and four routes on literal paths.
Its handlers return `RawResponse`, which lifts into `Response` verbatim, so a migration onto `App` left the security-carrying code alone.

Routes and state are what a service writes.
The lifecycle in §6 is what it inherits.

---

## 6. The lifecycle

**Core owns startup and shutdown end to end, and what a service supplies is data: the name in its lifecycle lines, the socket to unlink on every exit path, and a descriptor to wake a parked loop.**
This section gives the one ordering rule that cannot be relaxed, the three lines every service logs, and what the systemd unit has to provide.

### 6.1 Signals

`App::run()` calls `m6_core::signal::block()` before anything else, logging included, and then installs the shutdown handle once the socket is bound and ready.
The ordering is the rule.
A thread inherits the signal mask as it stands when the thread is created, and `tracing_appender` spawns a writer thread.
A service that initialised logging before blocking would have a thread with SIGTERM unblocked, the kernel would deliver the process-directed signal there, and the default disposition would kill the process.
The install asserts the mask is already set and refuses to start otherwise, because a supervisor counts death by the signal it sent as a clean stop and nothing else about that failure is visible.
`m6-architecture.md` §7.3, "why startup and shutdown belong to core", is the full argument.

The first signal drains and exits 0.
The second exits immediately.
Both unlink the socket, and a socket left behind keeps a dead member in the edge's backend pool until its next rescan.

### 6.2 The three lines

Every service logs the same three lines under its own name, and core emits them.

```
<name> started
<name> shutdown signal received
<name> shutdown complete
```

The name is the program's own, from `argv[0]`, and the started line carries the socket.
Core owning these is what makes searching a journal for `shutdown complete` mean the same thing for every unit.
The signal line is written before the shutdown flag is published, because the flag is what releases the main thread to drain and exit, and a fast service used to exit with the completion line written and the signal line still queued in the writer.

### 6.3 Under load and under a supervisor

A connection is served until the peer closes it or 100 requests have gone by, and the read deadline from `[server] read_timeout_s` is applied at accept time, so a silent peer holds a worker for exactly that long.
A full queue answers 503 immediately, which is how backpressure reaches the edge.

**Two of those defaults differ from what `m6-backend-protocol.md` requires, and the protocol is the older document.**
It requires a backend to chmod its socket `0666` and to close the connection after one request, and an `App` service defaults to `0660` and serves up to 100.
Both work against this edge: the edge and the service run as the same user, and `m6-http/src/forward.rs` sends `Connection: close` on every request it forwards, so persistence is never exercised.
For an `App` service the values above are what runs, and a backend written in another language follows the protocol.

The systemd unit has to supply four things: the two positional arguments, the user and group the socket directory is owned by, `RuntimeDirectory=m6` with `RuntimeDirectoryMode=0750`, and `ReadWritePaths` naming what this service actually writes.
`m6-user-guide.md`, "Example 06, Production with systemd", carries a complete unit per service and this document does not repeat them.

**`ReadWritePaths` must name only paths that exist.**
An absent path fails mount namespace setup outright with `226/NAMESPACE` and the unit never starts, so a service claiming `/run/m6` on a node with no socket backends does not come up.
Use `RuntimeDirectory` to guarantee what must exist, and never a `-` prefix to excuse an absent path, because tolerating it turns a misconfigured node into one that starts anyway with weaker isolation than intended.

All of that is core's, which means a service can prove it holds with one call, and §7 is that call.

---

## 7. Proving the lifecycle in one call

**The lifecycle contract is one assertion, and every service in the repository makes it.**
This section is the whole test and what it checks.

Below is `m6-monitor/tests/lifecycle.rs` without its configuration constant.

```rust
use std::process::Command;
use m6_core::testkit::{assert_app_lifecycle, binary};

#[test]
fn lifecycle_is_clean() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("app.conf");
    std::fs::write(&config, CONFIG).unwrap();
    let sock = dir.path().join("app.sock");   // short: a unix socket path caps near 104 bytes on macOS

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

It spawns the binary, waits for the socket, sends SIGTERM, and asserts three things.
The exit status is success, because a signal status means the process died at the default disposition instead of shutting down.
The socket is gone.
The three lines of §6.2 were logged under the service's own name, which also catches a service logging under someone else's.

**Write this test for every app.**
The assertion was opt-in and hand-written in five separate integration suites, and the sixth service reached "tested, ready to deploy" with seventeen unit tests and nothing that had ever started its binary.
All five serving binaries assert it now: `m6-file`, `m6-auth-server` and `m6-monitor` through `assert_app_lifecycle`, and `m6-html` and `m6-http` through `assert_lifecycle_logged` inside a wider suite that already has the process running.

The test is short because the contract is core's, and §8 is the rest of what comes with it.

---

## 8. What a service gets without asking

**A service that registers one route gets the whole HTTP layer.**
This section lists it, because apps have re-implemented parts of it before.

- HTTP/1.1 parsing and framing at 32/32 on h1spec, measured on `m6-html`, `m6-file`, `m6-auth-server` and the redirect listener, including HEAD with no body and correct `Connection` handling.
- Persistent connections to 100 requests, and a read deadline applied at accept.
- Content-coding negotiation with real q-values, so `br;q=0` is honoured, `*` supplies the quality for anything unnamed, and an explicitly named coding beats `*` in either direction.
- Conditional requests: `If-Match`, `If-None-Match`, `If-Modified-Since` and `If-Unmodified-Since`, with weak comparison where the specification requires it.
- brotli and gzip, and HTML, CSS, JSON and JavaScript minification, both configurable per MIME type, with minification before compression.
- Configuration hot reload on write, without a restart, as §4.2 bounds it.
- Structured logging, JSON or text, at a level and format fixed for the life of the process.
- A request dictionary layered over a shared base, assembled in twelve ordered steps.
  The sources are the service's own configuration keys, `global_params`, the route's params files, path parameters, the query string, a form body, cookies, built-ins, forwarded authentication claims, and the flash and *CSRF* (*cross-site request forgery*) values when those features are on.
- Path parameter validation, which allows alphanumerics, `-`, `_`, `.`, and `/` only where a route's parameter spans segments, and refuses `..` anywhere as a substring.

Two of those carry a condition.
The built-in dictionary keys go in after every params file, so a content file cannot redefine the request path and make a page lie about which URL it is.
A completed request is logged at `debug` with its path, method, status and latency, so a service at the default `info` level records nothing per request.

Everything in this section arrives by linking one crate, and §9 is how to link it from outside this repository.

---

## 9. Building against m6-core from another repository

**m6 is not published to a registry, so an app outside this repository reaches core by path or by revision.**
This section gives both and says what each one costs.

A relative path is what `m6-examples` uses.

```toml
m6-core = { path = "../../../../m6/m6-core" }
```

That is a directory layout across a repository boundary.
It works only if a checkout of `m6` sits at exactly that relative position, it pins nothing, and there is no way to say which version the app was tested against.

A git dependency pinned to a revision needs no layout and names a version.

```toml
m6-core = { git = "https://github.com/mgrosvenor/m6", rev = "<sha>" }
```

The revision is the version tested against, recorded in the manifest, and a build cannot move underneath the app that declares it.
The cost is that the pin is advanced deliberately, and an app wanting a fix in core says so in a commit.

**The relative path is still the shape in use.**
An app on that form needs the sibling checkout, and the test of having left it behind is that the app builds with no `m6` checkout beside it.

That is the last of the contract, and §10 collects it.

---

## 10. Summary

**Everything above is one claim: a new m6 service is its routes and nothing else.**
Table 8 is the whole contract in one place.

| the app supplies | core supplies |
|---|---|
| a `main` that builds an `App` and calls `run` | the socket, the accept loop, HTTP/1.1, the thread pool and the bounded queue |
| routes bound in code, handlers registered by name, or neither | route matching, the configuration route table, and hot reload of both |
| a state initialiser, when it has state | the two state tiers, and the destructor hooks |
| a configuration file whose stem is the service's name | the socket path, the backend name and the reserved keys of Table 4 |
| a systemd unit naming the two paths and what it writes | signal blocking, the drain, the three log lines and the unlink |
| one call to `assert_app_lifecycle` | the assertion behind it |

**Table 8: the division of labour between an app and `m6-core`.**
Take from it that the left column is the whole of a new service, and that every row on the right was once written once per service, which `m6-architecture.md` §6.2, "why each thing in core exists exactly once", counts.

Two consequences follow, and both are reasons to stay inside the shape.
A change to any row on the right reaches every service at once, which is why four HTTP/1.1 parsers scoring between 14/32 and 27/32 against the same suite became one scoring 32/32.
And a service written to this shape is read by anyone who knows the shape, which is what makes 34 lines of `m6-file` a complete static file server.

**If you are writing a new app, you are writing an `App` service.**
There is one shape, `m6-architecture.md` §7, "why every service has one shape", argues why, and this document is how.
