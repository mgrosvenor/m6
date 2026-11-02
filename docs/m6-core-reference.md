# m6-core, component reference

**Status: reference.**
This document describes m6-core at 38 modules, four of them behind features, and 25,584 lines of Rust.
One of the 38 is the prelude, a module of re-exports described in section 1, and the other 37 are the components in section 3.
Where it and the code disagree, the code is right and this document is a defect.

m6-core is the crate every m6 service links.
It supplies the server loop, routing, configuration, the request dictionary and the response helpers, so a service that wants one of those does not write it again.

This document is the reference for that crate: every module and the interface it exposes.
It answers what is here and how to call it, and `m6-architecture.md` answers why it is here and where its boundary falls.

Table 1 gives the two documents and the job each one does.
Take from it that this file is the lookup and the architecture document is the reason.

| file | job |
|---|---|
| `m6-architecture.md` | **Design.** What the crate is for, where its boundary is, and the rule that decides what goes in. |
| this file | **Reference.** Every component and its interface. |

**Table 1: the two m6-core documents and the job each one does.**
Take from it that a question about why a module exists belongs in the design document, and a question about a function's name or argument belongs here.

`m6-render-lib.md` is superseded.
It describes a crate that has been deleted, and most of what it described now lives here.

The document follows the shape of a service.
Sections 1 to 3 cover the ways into the crate, its feature flags and its module list.
Sections 4 and 5 cover the modules that build a service and the modules that read a request and write a response.
Sections 6 to 11 cover HTTP semantics, content, observability, errors, the test kit and the gaps this crate still has.

**Contents**

1. [Using it](#1-using-it)
2. [Feature flags](#2-feature-flags)
3. [Component index](#3-component-index)
4. [Building a service](#4-building-a-service)
5. [Request and response](#5-request-and-response)
6. [HTTP semantics](#6-http-semantics)
7. [Content](#7-content)
8. [Observability](#8-observability)
9. [Errors](#9-errors)
10. [Test kit](#10-test-kit)
11. [Known gaps in this crate](#11-known-gaps-in-this-crate)

---

## 1. Using it

This section shows the two shapes a service takes, the prelude and the hand-built loop, and what the prelude gives by default.

m6-core is the only crate an m6 service links, and the intended first line is the prelude.

```rust
use m6_core::prelude::*;

fn main() -> Result<()> {
    App::new()
        .route("/blog/{stem}", |req| Response::render("templates/post.html", req))
        .run()
}
```

The prelude is deliberately small.
It exports `App` and `AppContext`, `Dict`, `Error` and `Result`, `Request` and `Response`, the three helpers `now_iso8601`, `slugify` and `today_iso8601` from `util`, and `serde_json`'s `json!`, `Map` and `Value`.
Feature-gated additions appear in it only when the feature is on: `Upload`, `TeraFactory` and `TeraRenderer`, lettre's `Message`, `SmtpTransport` and `Transport`, `ureq`, and `rusqlite`.

A service that renders no templates says so, and stops linking Tera.

```rust
use m6_core::prelude::*;
use m6_core::render::NoTemplates;

fn main() -> Result<()> {
    App::new()
        .renderer(NoTemplates)
        .route("/healthz", |_req| Ok(Response::text("ok")))
        .run()
}
```

There is a second shape.
`m6-http` is a proxy with its own event loop and datagram sockets, and a tool can use the parsers in `telemetry` and `ndjson` without a server at all.
Every module is public, and most of the crate can be used without the `App` loop, so m6-core is a library first and a framework second.
One part is not reachable that way: `FrameworkState::build_dict` is private, which section 4 records as an open item.

Both shapes link one crate, and the difference is where the service loop comes from.
Section 2 gives the features that decide which parts compile.

## 2. Feature flags

Features and modules are different axes, and `flash` and `csrf` have no module at all.
They gate code inside `App` and one method each, `Response::flash` and `Request::verify_csrf`.
A reader who greps for a `csrf` module finds nothing and wrongly concludes the feature is missing.
Table 2 gives the eight features.

| feature | default | what it adds |
|---|---|---|
| `templates` | **on** | `template` module, `TeraFactory`, `TeraRenderer`. Pulls tera and comrak. |
| `testkit` | off | The `testkit` module. Enable from `[dev-dependencies]` only. |
| `multipart` | off | `multipart` module, `Request::file`. Pulls multer, tokio, bytes, futures-util. |
| `flash` | off | `Response::flash` and *HMAC* (hash-based message authentication code) signed one-shot messages. Pulls hmac, sha2. |
| `csrf` | off | *CSRF* (cross-site request forgery) token generation in the request dictionary, and `Request::verify_csrf`. No new dependencies. |
| `email` | off | `lettre` in the prelude. |
| `http-client` | off | `ureq` in the prelude. |
| `sqlite` | off | `sqlite` module: `open`, `open_reader`, `migrate`, `append_only`, `used_bytes`, `prune_oldest`, and `rusqlite` in the prelude. Pulls rusqlite 0.31 with bundled SQLite. |

**Table 2: the eight features, their defaults and what each one compiles.**
Take from it that only `templates` is on by default, six features pull a dependency and two add code only.

`templates` is on by default because a feature that hides code from `cargo test --workspace` is how two `csrf` tests stayed broken from Phase 4 to Phase 5.
`chrono` and `lru` are unconditional dependencies for the same reason.
A binary that wants none of it sets `default-features = false`.

Every off-by-default feature is invisible to `cargo test --workspace`: `testkit`, `multipart`, `flash`, `csrf`, `email`, `http-client` and `sqlite`.
A *CI* (continuous integration) step builds and tests m6-core with `--all-features` to cover them.

The two defaults are the whole of the choice: templating arrives without asking, and every other block is opt-in.
Section 3 lists the modules those features compile.

## 3. Component index

Thirty-seven component modules, grouped by the task that sends a reader to one.
The prelude in section 1 is the thirty-eighth `pub mod` in `lib.rs`, and it re-exports the items the other modules implement.
Table 3 is the index, and it uses six abbreviations: *HTTP* (Hypertext Transfer Protocol), *RFC* (Request for Comments), *TCP* (Transmission Control Protocol), *JSON* (JavaScript Object Notation), *UTC* (Coordinated Universal Time) and *CSPRNG* (cryptographically secure pseudorandom number generator).

| group | module | what it is |
|---|---|---|
| **Building a service** | `app` | The `App` builder, routing, the thread pool, the request dictionary and the service lifecycle. The largest module at 4,905 lines. |
| | `config` | `RendererConfig` and the configuration file that produces it. |
| | `server` | `UnixServer` and `serve_connection`, the socket-backend loop. |
| | `signal` | `block()`, `Service` and `ShutdownHandle`. Signal-safe shutdown, and SIGUSR1 to reopen the log file. |
| | `render` | The `Renderer` and `RendererFactory` traits, plus `NoTemplates`. The only thing the loop needs from a template engine. |
| | `template` | The Tera implementation of those traits, and the site filters. Feature `templates`. |
| | `watcher` | `ConfigWatcher`, inotify on Linux and kqueue on macOS, FreeBSD and OpenBSD, and `ConfigReloader`, which watches a service's two files. |
| **Request and response** | `dict` | The request dictionary: a shared base plus a per-request overlay. |
| | `request` | `Request`, the handler's view: the dictionary, body access, file I/O helpers, query and form parsing, and percent-encoding. |
| | `response` | `Response`, every constructor a handler needs, and `send`. |
| | `http` | `RawRequest`, `RawResponse`, `Method`, the `HeaderSource` trait and `is_same_origin_path`. |
| | `headers` | Reading and writing a header list, once. `get`, `get_all`, `combine`, `set`, `append`, `remove`. |
| | `cookie` | `Cookie` and `SameSite`. One place that says what a cookie can be. |
| | `multipart` | `Upload` and `parse_upload`. Feature `multipart`. |
| | `time_range` | `TimeRange`, the `from` and `to` of a query read as a half-open UTC range, and `rfc3339_utc_ms`. |
| **HTTP semantics** | `h1` | The HTTP/1.1 parser, plus `Responder`, `keep_alive`, `status_reason` and `Expectation`. |
| | `parse` | The stream adapter over `h1`. Reads bytes until `h1` says the message is complete. |
| | `conditional` | RFC 9110 13.2 preconditions. `evaluate_preconditions` and `is_not_modified`. |
| | `negotiate` | Content-coding negotiation, RFC 9110 12.5.3, with real q-values. |
| | `mime` | `mime_from_path` and `should_compress_default`. |
| | `path` | `validate_path_param`. A security boundary. |
| **Content** | `compress` | brotli and gzip, compress and decompress. |
| | `minify` | The four minifiers: markup, JavaScript, stylesheets and JSON. |
| | `util` | `slugify`, `now_iso8601`, `today_iso8601`, `iso8601_minutes_ago` and `html_escape`. |
| | `random` | `random_hex_token::<N>()`. One CSPRNG token generator, so there is never a third. |
| **Observability** | `log` | `init`, `init_with_analytics`, `LogHandle` and `LogPulse`. Tracing setup and the liveness pulse. |
| | `telemetry` | m6's own formats read back: `AnalyticsRecord`, `PeriodicStats`, `TrafficSummary`, and the probe and crawler heuristics. |
| | `monitoring` | `/health` and `/perf`: `HealthReport`, `PerfReport`, `TrafficReport` and `LoggingHealth`. |
| | `host` | What the machine is doing. Load, memory, disk, thermal, net, diskstats, pressure, *TCP* and file descriptors. |
| | `tls` | How long the loaded certificate has left: `not_after_unix`, `record_loaded`, `loaded` and `TlsCertificate`. |
| | `firewall` | `FirewallState` from nftables' own JSON. |
| | `wireguard` | `Handshakes` from `wg show <if> latest-handshakes` output. |
| | `ndjson` | Newline-delimited JSON, read and written, tolerant of torn lines, and served as bounded pages with a cursor. |
| **Storage** | `sqlite` | One SQLite file with the house settings, versioned migrations and append-only tables. Feature `sqlite`. |
| **Everything else** | `net` | `is_private_ip` and `bind_ip`. The one definition of a private address. |
| | `error` | `Error` and `Result`. 28 lines. |
| | `testkit` | The shared integration harness. Feature `testkit`. |

**Table 3: the 37 modules, their group and one line on what each holds.**
Take from it that the group names the task, and the module name is the string to search for in the section that follows.

The index is the map for the rest of the document.
Sections 4 and 5 take the two largest groups in turn, and sections 6 to 10 take the rest.

## 4. Building a service

This section covers the modules that build a service: the `App` builder and its state, the configuration it reads, the socket loop, signal handling, the renderer seam and the config watcher.
It uses *HTTP* (Hypertext Transfer Protocol) for the wire protocol and *TOML* (Tom's Obvious, Minimal Language) for the configuration format.

### 4.1. `app`

The builder is `App::new()`, then `.route(...)` or a method-specific variant (`route_get`, `route_post`, `route_put`, `route_patch`, `route_delete`), then `.run()`.
`.renderer(...)` supplies the template implementation.

Four entry points choose what state the service needs, and each returns a different builder type with the same routing surface, so a service declares its state shape once and the type carries it.
Table 4 gives the four.
Take from it that the entry point is the state declaration, and every later call is the same on all four types.

| entry point | builder | for |
|---|---|---|
| `App::new()` | `App` | no state |
| `App::with_global(init)` | `AppWithGlobal<G>` | one `G` shared across all workers |
| `App::with_thread_state(init)` | `AppWithThreadState<T>` | a `T` per worker thread |
| `App::with_state(init_global, init_thread)` | `AppWithState<G, T>` | both |

**Table 4: the four `App` entry points and the state each one carries.**
Take from it that the global value is shared and the thread value is per worker.

The global initialiser receives an `AppContext`, which carries the config map, the site directory, the config path and the resolved `site.toml`.
A thread initialiser receives the config map and the global state, or `()` when there is none.
Both return `Result`, so a service that cannot build its state fails at startup, before the first request.

`.on_destroy(...)` gives the global a teardown hook and `.on_destroy_thread(...)` gives the thread value one.
`.on_reload(...)` runs after a reload has built and checked the new state, before that state and log level are installed, so a hook that returns `Err` keeps the previous configuration serving.

Routing: `compile_pattern` turns `/blog/{stem}` into `Vec<Segment>`, `route_specificity` scores a compiled route so a literal beats a parameter, and `match_route` returns `PathParams` as a `Vec<(String, String)>` because order is meaningful and duplicates are possible.
`find_route` picks the winner.
Route matching existed three times before this, twice hand-rolled with different specificity rules, so the same route table could resolve differently in two services.

The thread pool: `ThreadPool::new(size, queue_size)`, or `new_with_exit(size, queue_size, on_thread_exit)` to run a callback when each worker thread ends.
`submit` returns `false` when the queue is full, which is how the loop sheds load with a 503 instead of growing a backlog.
`try_submit(resource, f)` hands a resource to the work and returns it back when the queue is full.
`in_flight` reports depth and `drain` waits.

The request dictionary: `FrameworkState::build_dict` is where the real knowledge lives, and it is **private**, which is the outstanding item on this module, because a service not using `App` cannot reuse any of it.
Three layers no longer run per request: config keys, global params files and a route's static params files are merged once per reload into `route.base_dict`.
The per-request order is:

- Params files whose path holds a `{placeholder}`.
- Path params, each validated.
  A `{*name}` capture may span segments and a `{name}` capture may not.
- Query params, inserted at top level and as a nested `query` map.
- POST form fields, from `application/x-www-form-urlencoded` only.
  A non-empty body of any other type logs a warning.
- Cookies, as a nested `cookies` map.
- Built-ins `request_path`, `datetime` and `year`, after the params files so a params file cannot override them.
- Auth claims from `x-auth-claims`.
- Flash message, when the feature is on: verify the HMAC, insert `flash`, and the response path adds the removal cookie.
- CSRF token, when the feature is on and the dictionary holds none: reuse the `_csrf` cookie or generate one, and insert it.
  A submitted form token stays in place for `verify_csrf` to compare.

Error keys need no step, because they arrive as query parameters at the query step.

`is_shutdown()` is a free function for handlers that run long enough to care.

### 4.2. `config`

`config::load(config_path, site_dir) -> anyhow::Result<RendererConfig>`.
The struct is:

```rust
pub struct RendererConfig {
    pub user_config:   Map<String, Value>,   // arbitrary service keys
    pub global_params: Vec<String>,
    pub routes:        Vec<RouteConfig>,
    pub thread_pool:   ThreadPoolConfig,     // size, queue_size
    pub params_cache:  ParamsCacheConfig,    // size
    pub server:        ServerConfig,         // read_timeout, socket_mode
    pub compression:   HashMap<String, CompressionLevel>,  // per-mime brotli/gzip
    pub minification:  MinificationConfig,   // per-mime enabled, inline_js
    pub log:           LogConfig,            // level, format
    pub site_toml:     Option<PathBuf>,      // the reserved key site_toml
}
```

`site_toml` names the `site.toml` this service reads.
`RendererConfig::resolve_site_toml(site_dir)` returns the path: `<site_dir>/site.toml` when the key is absent, the value as given when it is absolute, and `site_dir` joined with it when it is relative.
A non-string or empty value fails `config::load`.
`AppContext::site_toml` carries the resolved path to the global initialiser, set whether or not the file exists.
A service that names a `site_toml` that does not exist, cannot be read or is not valid TOML exits 2 at start.

A `RouteConfig` carries nine fields: `path`, `template`, `params`, `status`, `cache`, `methods`, `headers`, `handler` and `settings`.
`handler` names a code handler registered with `App::handler`, and `settings` keeps the per-route keys core does not define.
`MinificationConfig::is_enabled(mime)` answers the per-type question, and `toml_to_json` is the conversion used throughout.

`[server] read_timeout_s` is the read deadline applied to every accepted connection, defaulting to `server::DEFAULT_READ_TIMEOUT_SECS` (30).
`0` means no timeout, which is what every `App` service did before the key existed.
It is read once at startup, like the pool dimensions: it is set on a socket at accept time, so a reload cannot retune connections already being served.

`[server] socket_mode` is the mode applied to the unix socket after bind, defaulting to `server::DEFAULT_SOCKET_MODE` (`0o660`).
It is an **octal string**, as systemd writes it, because TOML has no octal literal and `660` as a decimal integer is `0o1224`.

Neither key silently falls back.
A value that cannot be understood, a negative timeout or a mode above `0777` fails `config::load` and the service does not start.
A service that will not start says so on the first line of its journal, and a service running at a socket mode nobody chose looks exactly like one running at the right mode.
An App service answers `--dump-config` too, so a deploy can validate its config before starting it, and it exits 2 when the config is refused.

### 4.3. `server`

`UnixServer::bind(path)` claims a unix socket and unlinks it on `Drop`.
`accept_one(handler)` takes one connection, and `listener()` exposes the raw `UnixListener` for a service running its own `poll(2)` loop.

`serve_connection(stream, handler)` is the loop itself.
It hands the handler a `RawRequest` and a `&mut h1::Responder`, so a handler writes through the response writer and cannot frame one itself.
`MAX_REQUESTS_PER_CONN` is 100.

`socket_path_from_config(config_path)` derives the socket path so that services and deploy scripts agree without a second convention.

### 4.4. `signal`

**`signal::block()` must be the first statement of every `main`, before logging.**
Blocking a signal is per-thread and threads inherit the mask at creation, and `tracing_appender::non_blocking` spawns a writer thread, so a `main` that initialises logging first has already created a thread with SIGTERM unblocked.
The kernel delivers process-directed SIGTERM there, the default disposition kills the process, and the `sigwait` thread never receives a signal in its life.
A service that started that way could lose its shutdown line invisibly, because systemd counts death by the signal it sent as a clean stop.

`ShutdownHandle::install(Service::new("name").socket(path).wake_fd(fd))` sets up the handler, and it asserts that the mask is set and refuses to start otherwise.
`complete()`, `is_shutdown()` and `wait()` are the lifecycle, and `is_shutdown()` is also re-exported from `app`.

`REOPEN_SIGNAL` is SIGUSR1.
`block()` blocks it with SIGTERM and SIGINT, and the signal thread handles it before it counts a shutdown signal, so a reopen is never a step toward a forced exit.
The thread calls `log::reopen_log_file()` and logs what happened.
The reopen opens the new file while holding the lock the log writer takes for every line, so once the path exists again no further line can reach the renamed file.
If the open fails, the old handle stays in place, the process keeps writing to the previous file and an error is logged.
SIGHUP stays free for a future configuration reload.

### 4.5. `render` and `template`

`render` is the seam, and it is 87 lines.
A route that names a template produces a `Response` carrying `template_name` and `template_dict`, a `Renderer` turns that into bytes, and a `RendererFactory` builds one per reload, shared by every worker.
`NoTemplates` implements both traits and errors if anything asks it to render, which is how the site renderers that render nothing stopped linking Tera, comrak and pest in order to get a server loop.

`template` is the Tera implementation: `build_tera`, `build_tera_from_paths`, `TeraRenderer`, `TeraFactory` and `NOT_FOUND_SENTINEL`, which is how a template signals a 404 from inside rendering.

### 4.6. `watcher`

`ConfigWatcher::new(paths)`, `raw_fd()` to fold into an existing `poll(2)` set, and `read_events(filenames)` to drain and report whether anything relevant changed.

`ConfigReloader::new(site_toml, process, seen)` owns a `ConfigWatcher` on a service's two files and reports which of them changed.
The loop calls `on_readable(now)` when `raw_fd()` is readable and `tick(now)` on every iteration, and `tick` returns `Some(Changed { site, process })` when a report is due.
A `Fingerprint` of device, inode, length, mtime and ctime decides which file changed.
On Linux an event that names a file forces a report even when the fingerprint is equal, which catches a same-length rewrite and two renames that reuse an inode.
On kqueue the watcher reports the directory, so an unrelated write beside the config costs one spurious report and the fingerprints decide the result.

The first change after quiet is reported at once.
A change within `RELOAD_DEBOUNCE` (20 ms) of a report is held and reported once more when the burst settles, capped at `RELOAD_POLL` (one second), so a burst is at most two reports.
The fingerprints are also compared every `RELOAD_POLL` whatever the watcher says, which finds a lost watch.
Take a `Seen` from `Seen::read` before the first read of either file so a change in between is reported on the first tick.
With no watcher the poll alone does the job, there is no thread, and `tick` does not allocate.

Three implementations sit behind `cfg`: **inotify on Linux**, kqueue on macOS, FreeBSD and OpenBSD, and a stub elsewhere that never reports a change.
The Linux path uses `nix::sys::inotify`, which copies each event header into an aligned `MaybeUninit`, so the unaligned read the old hand-written arm risked cannot happen.

Those six modules are a service's own parts: the builder, its configuration, its socket, its signals, its renderer and its reload.
Section 5 turns to the objects a handler receives and returns.

---

## 5. Request and response

This section covers the objects a handler touches: the request, the response, the *HTTP* (Hypertext Transfer Protocol) types behind them, the header and cookie helpers, and the time range a query can ask for.
It ends with the rules each helper enforces, and section 6 turns to the parsers those objects read from.

### 5.1. `request`

`Request` is what a handler receives: `method()`, `path()`, `header(name)`, `content_type()`, `body_raw()` and `body_json()`.

`field(name)` reads a form or query field.
`get(key)` and `Index<&str>` read the dictionary, and `dict()` borrows the whole thing.

File I/O is on `Request` on purpose, scoped to the site directory: `site_path(rel)` resolves a path, and `read_json`, `write_json`, `write_json_atomic`, `list_json` and `touch` read and write there.
`write_bytes` does the same and needs the `multipart` feature.
The readers and writers refuse a `..` component, and `write_json_atomic` is the one to reach for when a reader may be running.

`verify_csrf()` (feature `csrf`) and `file(name) -> Upload` (feature `multipart`).

Free functions, all used by the framework and all worth knowing: `parse_query_string`, `parse_form_body`, `url_decode`, `url_encode`, `url_encode_path`, `cookie`, `parse_cookies`, `parse_auth_claims`, `validate_path_param` and `single_query_value`.

`single_query_value(query, key)` returns the raw value of a key that may appear once: `None` when absent, `RepeatedKey` when repeated.
`time_range`, `ndjson::Cursor` and a service's own filter parameters all read through it, so one query string follows one rule.

The *URL* (Uniform Resource Locator) coders `url_encode` and `url_decode` exist because core could percent-decode and not encode, so callers wrote their own encoder.
Encoding and decoding are two halves of one block.

### 5.2. `response`

Constructors: `render`, `render_with`, `render_status`, `render_dict`, `redirect`, `redirect_permanent`, `json`, `json_status`, `html`, `text`, `status`, `not_found`, `forbidden`, `bad_request` and `refusal`.

`refusal(&message)` is the 400 for a refused request parameter: the message and a newline as plain text, with `no-store` and `nosniff`.
The message must not echo client input.

Builders: `body`, `with_status`, `header`, `cookie` and `flash` (feature `flash`).

`send(&mut h1::Responder)` writes it, and `error_to_response(&Error)` is the default mapping from an error to a response.

### 5.3. `http`

`RawRequest` is the parsed wire request: `method()`, `path()`, `query()`, `header()`, `content_type()` and `accept()`.
`RawResponse::new(status)` with `.header()`, `.body()`, `.content_type()`, then `send` or `to_bytes`.

`Method` is a newtype over `&'static str` with the six constants.

`HeaderSource` is the trait that lets header lookup work over both `[(String, String)]` and `Vec<(String, String)>`, and `header(headers, name)` is the case-insensitive first-match lookup.

`is_same_origin_path(candidate)` is the redirect guard: it answers whether a path is safe to redirect to without leaving the origin.

### 5.4. `headers`

Use this module when the question is harder than first-match.
Eight or more sites across the workspace wrote `.find(|(k, _)| k.eq_ignore_ascii_case(name))` inline, several of them inside core, and that idiom hides two real questions.

`get`, `get_all`, `contains`, `set`, `append`, `set_if_absent` and `remove`.

`combine(headers, name)` folds repeated field lines into one comma-separated value and **refuses when the field must not be folded**.
`NEVER_COMBINED` is the list and `set-cookie` is on it: *RFC* (Request for Comments) 6265 3 is explicit that cookie values may contain commas, so joining two `Set-Cookie` lines produces one malformed cookie and silently loses the other.
`may_combine(name)` asks in advance.

### 5.5. `cookie`

`Cookie::new(name, value)` then `.max_age()`, `.path()`, `.domain()`, `.secure()`, `.http_only()`, `.same_site(SameSite::...)`, then `to_header_value()` or `to_header()`.
`Cookie::removal(name)` builds an expiry.

This module builds and parses cookies.
Before it, four `format!` calls with different attribute sets were the implementation, and none of them could answer "which of our cookies are HttpOnly?".

### 5.6. `time_range`

`TimeRange::from_query(query, max_span)` and `TimeRange::from_request(req, max_span)` read `from` and `to`.
`DEFAULT_MAX_SPAN` is 31 days.

- Both keys are required and each may appear once.
  Other keys are ignored, so a cursor or a filter passes through.
- A value is *RFC* 3339 in *UTC* (Coordinated Universal Time) with an uppercase `T` and a final `Z`, such as `2026-01-01T00:00:00Z`.
  Fractions of a second are accepted, and an offset such as `+10:00` is refused.
- The range is half open, `[from, to)`.
  `contains(t)` is `from <= t < to`, so two adjacent ranges never share a row.
- `from` is not before 1970, `to` is later than `from`, and the span is at most `max_span`.
- `from_unix_ms`, `to_unix_ms`, `from_unix_s` and `to_unix_s` round up, so for integer timestamps `from_ms <= t < to_ms` is exactly `[from, to)`.

`TimeRangeError::to_response()` is a 400 in plain text with `no-store` and `nosniff`.
It says which parameter to fix and never repeats the value the client sent.

`rfc3339_utc_ms(unix_ms)` formats a millisecond timestamp as `2026-01-01T00:00:00.123Z`.
It returns a value that formats when displayed, so a row can carry it without a `String` per line, and a negative or out-of-range input gives the epoch.

Those six modules are the handler's whole surface: one request, one response, the wire types they are built from, and the three helpers for headers, cookies and time ranges.
Section 6 turns to the parsers and the HTTP rules those objects implement.

---

## 6. HTTP semantics

### 6.1. `h1`

`parse_request(buf) -> ParseResult` is the one HTTP/1.1 parser, and it is
**pure**: it does not strip proxy-owned headers, because that is ingress policy
rather than parsing, and a backend has no proxy headers to strip.
`ParseResult` is `Complete(RawRequest)`, `Incomplete` or `Error`.

There were four parsers. Measured against h1spec, the independent RFC 9112
tester, this one scored 27/32 and the others 15, 15 and 14. The plan named the
14 as the consolidation target. Measuring the candidate before moving anything
onto it is what caught that. All four targets now score 32/32 and
`tools/conformance.sh` holds them there.

`Expectation` and `expectation(buf)` handle `Expect: 100-continue`;
`CONTINUE_RESPONSE` is the reply. `keep_alive(&req)` decides connection reuse.
`status_reason(status)` maps a code to its phrase.

`Responder::new(w, method, keep_alive)` is the write side: `send`,
`send_with_length`, `error`, with `keeps_alive()` and `body_bytes()`. It omits
the body on a HEAD, which the three hand-written writers it replaced did not
reliably do, so every 404, 405, 400 and 412 answering a HEAD went out with one.

### 6.2. `parse`

`parse_request(stream) -> Result<RawRequest, ParseError>`.

**This is a stream adapter, not a second parser.** It reads into a buffer and
asks `h1::parse_request` whether the message is complete, so it has no framing
logic of its own. It takes `Write` as well as `Read` because a client that sent
`Expect: 100-continue` will not send its body until told to, and answering that
has to happen before the read that would otherwise deadlock.

Two caps, guarding different things: 8 KB of head, then 16 MB of body. It used
to read one byte per `read()` call, measured at 377 syscalls for an ordinary
browser's 377-byte request head, because `server` hands it a raw `UnixStream`
with nothing buffering.

`ParseError::status()` and `.reason()` give the response for a parse failure.

### 6.3. `conditional`

`evaluate_preconditions(...) -> Precondition`, `is_not_modified(...)`,
`not_modified_headers(cached)`.

RFC 9110 13.2, implemented once. It was implemented twice, and the second copy
was five inline lines that did strong comparison for `If-None-Match` and
implemented neither `If-Match` nor `If-Unmodified-Since`. The symptom came and
went with cache state, because a hit was answered correctly by the good copy
and only a miss reached the bad one. A defect that correlates with cache state
reads as noise.

### 6.4. `negotiate`

`coding_quality(accept_encoding, coding) -> Option<f32>`,
`preferred_coding(accept_encoding, candidates)`,
`canonical_coding(accept_encoding) -> &'static str`.

Real q-values. `q=0` means "not acceptable" (RFC 9110 12.4.2), which a
substring match gets wrong in the direction that matters: brotli sent to a
client that explicitly refused it. `canonical_coding` is what makes a cache key
out of a header, so that `gzip` and `gzip, deflate, br, zstd` stop being two
entries for one byte-identical response.

### 6.5. `mime` and `path`

`mime_from_path(path)`, `should_compress_default(mime)`.

`validate_path_param(value, allow_slash) -> Result<&str, PathParamError>`. This
is a security boundary and it existed three times, with three behaviours. There
is one now.

### 6.6. `net`

`is_private_ip(ip)` is the one answer to "is this on a private network".
`bind_ip(bind)` pulls the address out of `ip:port`, `[v6]:port` or a bare
address, and gives `None` for a host name. Names are never resolved.

| family | private |
|---|---|
| IPv4 | `127.0.0.0/8`, `10.0.0.0/8`, `172.16.0.0/12`, `192.168.0.0/16`, `169.254.0.0/16`, `100.64.0.0/10` |
| IPv6 | `::1`, `fc00::/7`, `fe80::/10` |

An IPv4-mapped IPv6 address is classified by its IPv4 form. `0.0.0.0` and `::`
are not private. A check on a peer address only means something on a socket
that faces the client directly; behind a proxy the peer is the proxy.

---

## 7. Content

`compress`: `brotli_compress(data, quality)`, `brotli_decompress`,
`gzip_compress(data, level)`, `gzip_decompress`.

`minify`: `minify_html(data, minify_inline_js)`, `minify_js`, `minify_css`,
`minify_json`.

`util`: `slugify`, `today_iso8601`, `now_iso8601`, `iso8601_minutes_ago`, `html_escape`.

`random`: `random_hex_token::<N>()`. There were two byte-identical copies at
two lengths. Both were correct; the hazard is the third one, written by someone
who reaches for `rand::random()` and produces something that looks identical at
a glance.

### 7.1. `sqlite`

Feature `sqlite`. Re-exports `rusqlite`. `open(path)` opens one database file
with the settings in Table 7.1. `migrate(&mut conn, &[Migration])` applies a
versioned list kept in `PRAGMA user_version`, and `schema_version` reads it.
`append_only(&conn, table)` adds triggers that refuse UPDATE and DELETE, and
`is_append_only` reports them. `used_bytes` and `prune_oldest` keep a table
under a size cap. The cap counts the whole file, and `prune_oldest` returns
`Pruned { rows_deleted, cap_met }`. `OpenOptions` changes the busy timeout, the journal mode
(`Journal::Rollback` or `Journal::Wal`) and foreign keys.
`open_reader(path)` opens an existing file read-only with the busy timeout and nothing else, for a reader beside the writer.

| setting | value | reason |
|---|---|---|
| `journal_mode` | `DELETE` | The database is one file at rest. WAL leaves `-wal` and `-shm` beside it, and a copy of the main file alone can miss committed work. |
| `busy_timeout` | 5 s | A second connection waits for a lock instead of failing with `SQLITE_BUSY`. Set explicitly, because rusqlite documents its own default as subject to change. |
| `foreign_keys` | on | SQLite has them off in every new connection. The value is read back. |
| `recursive_triggers` | on | `INSERT OR REPLACE` fires DELETE triggers only with this on, so the append-only rule needs it. |

*Table 7.1: the settings `open` applies, and why. Each is read back, and a
setting that does not take effect is an error.*

**Migrations** number 1, 2, 3 in order, and the list is checked before anything
runs. Each runs in its own `BEGIN IMMEDIATE` transaction with the version
re-read inside it, so connections that open at once apply each migration once.
A failing migration rolls back. A file newer than the list is refused with
`NewerThanCode`, before the first migration and again inside each transaction.
`migrate` switches `foreign_keys` off before each migration, so a migration can
rebuild a table that others point at. It runs `PRAGMA foreign_key_check` before
the commit, rolls back with `ForeignKeyCheck` on any dangling row, and restores
the setting afterwards. A released migration is never edited. Use one journal
mode for every connection to a file, because SQLite cannot leave WAL while
another connection is using it.

**Append-only** triggers are named `<table>_append_only_update` and
`<table>_append_only_delete`. Call `append_only` after `migrate` on every open,
and it restores triggers that were dropped. The triggers guard against mistakes
in code. Anyone who can write the file can drop them. A table that is pruned
must not be append-only.

---

## 8. Observability

### 8.1. `log`

`init(format, level)` and `init_with_analytics(format, level, analytics_path)`
return a `LogHandle`, whose `reload(format, level)` re-reads on config change.
`read_site_log_config(site_toml)` and `parse_level(s)` are the config side. `read_site_log_config` takes the path of the file, which `RendererConfig::resolve_site_toml` gives.

`pulse() -> &'static LogPulse` is the liveness signal: `events()` counts, and
`seconds_since_last()` answers "has anything been logged recently", which is how
a silent log becomes a fault rather than a zero.

`reopen_log_file() -> Reopened` reopens the analytics file at the absolute path it was opened with.
`reopens() -> &'static ReopenRecord` answers how many reopens succeeded and failed, and how long ago the last of each was.
`writes_log_file()` says whether this process has a file to reopen.
`newest_rotated(live)` finds the newest rotated sibling of a log file, plain or gzip.
`read_tail(path, max_bytes)` reads the end of one file.
`read_recent(live, max_bytes, since)` reads the live file and, when it adds history, the newest rotated sibling before it.
`read_recent_counted` does the same and also returns what the read cost.

### 8.2. `telemetry`

The write side had no struct: `m6-http` emits analytics with
`tracing::info!(target: "analytics", ...)` and the JSON comes out of the
subscriber's formatter, so every consumer re-derived the format by looking at a
sample. That is how it came to be read as `ts` and `user_agent` rather than
`timestamp` and `fields.user_agent`, which yields an empty result that looks
exactly like a quiet hour.

`AnalyticsRecord` and `AnalyticsFields` are that missing definition, `Serialize`
as well as `Deserialize` so the write side can adopt them.
`parse_analytics(...)` reads a stream. `PeriodicStats::parse(line)` reads the
`periodic stats` log line, `strip_ansi` first because the journal carries
colour. `aggregate_windows(...) -> StatsAggregate` sums them.

The heuristics, and their thresholds as named constants rather than magic
numbers: `claims_to_be_bot`, `looks_like_probe`, `looks_like_injection`,
`BOT_MARKERS`, `PROBE_MARKERS`, `INJECTION_MARKERS`,
`PROBE_PATHS_FOR_CONCERN` (3), `REFUSED_REQUESTS_FOR_CONCERN` (10),
`UA_ROTATION_THRESHOLD` (10), `UA_ROTATION_MIN_REQUESTS` (20).

`UA_ROTATION_THRESHOLD` exists because one IP rotating 526 user agents would
otherwise be reported as a dozen AI crawlers politely visiting. Read the full
user-agent list; a keyword list invents crawlers.

`ClientSummary::is_notable()` and `error_ratio()`, `CrawlerSighting`, and
`TrafficSummary::from_records(...)` assemble the picture. Volume alone is not a
fault: the same rule once fired on 185 requests from the operator's own address
and on a single 404 to `/.git/config`. A fault list only works if everything on
it is a fault.

### 8.3. `monitoring`

`/health` answers up or down and is constant-cost by construction. `/perf`
answers counters and percentiles and is token-gated, **and not out of modesty
about traffic volume**: live latency and error counters tell an attacker
which requests cost 2.8us and which cost 7.8ms, about 2800 to 1 on this
deployment, which turns a blind probe into a tuning loop. With no token
configured `/perf` is a 404, not a 401, so forgetting to configure it fails
closed and does not advertise a door.

`HealthReport::build(node, pools)` returns `(u16, HealthReport)` and
`into_response(code)`. Health means "this node can serve", not "the fleet is
fine": a cache node has no socket pools at all, so a naive "0 members means
unhealthy" check reports both edges as permanently down, and a cache node with
a warm cache keeps serving correctly while the origin is unreachable, which is
the entire point of having edges.

`PerfReport::build(...)`, `PerfOutcome`, `metrics_authorised(...)`,
`is_monitoring_endpoint(backend)`, `HEALTH_BACKEND`, `PERF_BACKEND`.

A caller with its own gate uses two more pieces.
`PerfAccess::refusal()` returns the legacy bearer gate's verdict: `Some(Disabled)` with no token configured, `Some(Unauthorised)` for a wrong one, `None` to pass.
`PerfReport::build_authorised(subject, snapshot)` builds the report with no access check, for a caller that has already authorised the request.
`PerfOutcome::Forbidden` answers 403 `{"error":"forbidden"}` with no `WWW-Authenticate`, for a valid token that lacks the required claim.
m6-http uses these for `[health] metrics_require` and `metrics_peers`, described in `docs/m6-http.md`.

`PerfReport.tls` carries the served certificate chain and how long each
certificate has left, from `tls::loaded()`. It is `serde(default)`, so a node
older than the field stays parseable to an aggregator rather than making the
whole payload unreadable, which is the treatment `build` already gets.

`PerfReport.logging` carries `LoggingHealth`, which is also on `/traffic`.
Its `reopen` field is a `LogReopenHealth`: reopens and failures since start, the age of the last reopen, its unix time and the age of the last failure.
`reopen` is `None` for a process that writes no log file, and `seconds_since_last_reopen` is `None` until the first reopen.
Both are `serde(default)`, so a node older than the fields still parses.

`TrafficReport::build(node, ndjson, since, window_minutes)` is the `/traffic`
endpoint: the node does the analysis and the log stays put. `LoggingHealth::read()`
with `QUIET_SECONDS` (40) and `is_blind()` is the silent-log fault.
`NotableClient`, `HeavyHitter`, `CrawlerReport` are its parts.
`CrawlerReport` carries `first_seen` and `last_seen`, both `serde(default)`.

### `resolve`

Reverse DNS with a deadline, for checking a claim against its source.
It runs in a monitor and never in a serving path, because a lookup is a network round trip.

`reverse_confirmed(addr, timeout)` and `reverse_confirmed_default(addr)` resolve the PTR name of an address, then resolve that name forward, and return a `Ptr`:

| `Ptr` | meaning |
|---|---|
| `Name(n)` | the PTR name `n` resolves back to the address |
| `Unconfirmed(n)` | the PTR name `n` exists and its forward lookup completed without the address, a false claim |
| `None` | the address has no PTR record, unverified |
| `Failed` | a lookup failed, timed out or found no free thread slot, unverified |

Table 2: the outcomes of a confirmed reverse lookup.

`Ptr::verifies(domain)` matches a name against a domain on a dot boundary, so `example.com.attacker.net` does not match `example.com`.
`DEFAULT_TIMEOUT` is two seconds and covers both lookups together.
At most `MAX_LOOKUP_THREADS` (8) lookup threads are alive at once, counting those abandoned at their deadline, and a lookup with no free slot reads `Failed` at once.
`telemetry::verify_crawler(user_agent, &ptr)` turns a `Ptr` into a `CrawlerVerdict` using the operator's documented method.

### 8.4. `host`

Parsing is separated from reading. The `parse_*` functions are pure over the
text these files contain and are tested on any platform; the readers are thin
and platform-gated.

**`snapshot(disk_path) -> HostSnapshot`** is the one most callers want: it
assembles every reading below into the struct `/perf` publishes.

Readers, each platform-gated with a degraded fallback that returns `None` or an
empty `Vec` rather than failing: `load_average`, `memory`, `disk`, `cpu_count`,
`thermal_zones`, `uptime`, `net_devices`, `disk_io`, `pressure(resource)`,
`tcp_health`, `file_descriptors`.

Pure parsers, tested on any platform: `parse_loadavg`, `parse_meminfo`,
`parse_cgroup_memory`, `parse_thermal_millidegrees`, `parse_uptime`,
`parse_net_dev`, `parse_diskstats`, `parse_pressure`, `parse_netstat`,
`parse_sockstat_into`.

Types: `HostSnapshot`, `LoadAverage`, `Memory`, `MemorySource`, `Disk`,
`ThermalZone`, `NetDevice`, `DiskIo`, `Pressure`, `TcpHealth`,
`FileDescriptors`.

A VPS frequently has no thermal zone at all, so "no reading" is an ordinary
answer and is `None`, not an error.

Two readings are easy to get wrong and invisible when wrong:

- **Memory inside a container is not the host's memory.** `/proc/meminfo`
  reports the machine, not the cgroup limit. `memory()` prefers the cgroup limit
  when one applies and says which it used in `Memory::source`.
- **Free disk is not available disk.** The difference between `f_bfree` and
  `f_bavail` is the blocks reserved for root, typically 5%. `disk()` reports
  `f_bavail`, the space a service can actually use.

There is no threshold anywhere in this module. Core says what the number is; a
monitoring service decides what 80% means.

### 8.5. `tls`

**`not_after_unix(der) -> Result<i64, CertificateError>`** reads `notAfter` out
of a DER certificate as seconds since the Unix epoch.
**`record_loaded(chain)`** stores a loaded chain and returns the certificates it
could not read, by depth and reason.
**`loaded()`** and **`loaded_at(now_unix)`** return the stored chain as
`Vec<TlsCertificate>`, leaf first, each carrying `depth`, `not_after_unix` and
`expires_in_seconds`.

This module exists because certificate renewal fails silently.
Renewal is done by an *ACME* (Automatic Certificate Management Environment)
client on a timer outside m6, and a timer that fails keeps firing and keeps
failing.
Nothing looks, so the first signal is the server refusing connections on the day
the certificate expires.
A server already holds the answer, which makes it the cheapest place to ask.

**It reports the loaded material, never a file.**
A path on disk and the certificate a running process is serving are different
facts, and a renewal that wrote a new file without reloading the server is
exactly the case where they disagree.
`record_loaded` is therefore called by whatever loads the certificate, and
`loaded()` opens nothing, so polling a monitoring endpoint cannot be made to do
disk work.

| behaviour | why |
|---|---|
| a chain, leaf first | `depth` 0 is what an *ACME* client renews, and intermediates expire too. A chain is good until its soonest expiry |
| `expires_in_seconds` goes negative | how long ago separates a renewal that failed last night from one that stopped running in March |
| an unreadable certificate is returned, not recorded | rustls accepted it or there would be no server, so refusing to serve is the worse failure. A reported absence lets a monitor say "cannot say" |
| a later `record_loaded` replaces the earlier one | so a reload corrects it. `m6-http`'s `handle_tls_reload` records after the new material is installed on both the rustls listener and the quiche configuration, so the registry holds what every protocol serves and a failed build leaves the previous chain recorded. Until m6 #210 a reload rebuilt the quiche configuration alone and recorded nothing, so the number was correct for h1 and h2 and stale for h3 |

The last two rows are the ones to keep: an absence and a measured number must
not look alike, and a stale number is worse than either.

There is no threshold here, for the reason `host` gives.
Core reports how many seconds are left and a monitoring service decides what is
too few.

**The parse is written out rather than imported.** `rustls` hands over
`CertificateDer` and parses no validity from it, and `rustls-webpki` keeps its
own parse private, so one integer field meant either eight crates of general
ASN.1 or a walk over a known shape. `notAfter` sits at a fixed structural
position in every X.509 certificate, so the walk steps over six elements by
length and reads the seventh. Every length is checked against the buffer and a
test runs every truncation of a real certificate through it. The dates go
through `chrono`, because civil-from-days arithmetic is the mistake
`util::iso_date_from` records.

### 8.6. `firewall`

`FirewallState::from_nft_json(text)`, `from_file(path)`,
`blocked_addresses()`, `active()`, and `Block`.

nftables emits JSON natively, so there is no format to invent and no log to
scrape. It reads a file rather than shelling out because `nft list ruleset`
needs `CAP_NET_ADMIN` and the process answering public requests must not have
it: a systemd timer writes `/var/lib/m6/firewall.json` and m6 reads it as
ordinary data.

It reports the deliberate blocks and whether they are still being hit. It does
not report "180 packets from 10 sources", which never changed and never prompted
an action.

### 8.7. `wireguard`

`Handshakes::parse_latest_handshakes(text)`, `from_file(path)`, `find(key)`, and `PeerHandshake`.

It reads a file because WireGuard state needs `CAP_NET_ADMIN`.
A systemd timer writes the output of `wg show <if> latest-handshakes`, and m6 reads it as ordinary data.
A time of 0 means the peer never completed a handshake.
A missing file is `Ok(None)`, and a malformed line is an error naming the line.

Do not use `wg show <if> dump`, because it carries the interface private key.

The time is the last successful handshake.
After a key changes under a tunnel it stops moving, and it still looks recent for minutes.
An age is therefore the slow signal, and a probe over the tunnel is the fast one.

### 8.8. `ndjson`

`read_str(text)`, `Reader::new(r)` with `skipped()` and `read_count()`,
`write_one`, `write_all`, `to_string`.

**Torn lines are ordinary, not exceptional.** An NDJSON file is usually being
appended to while it is read, so the last line can be half-written and a reader
that fails the whole stream on one bad line fails whenever it reads during a
write. Readers skip what they cannot parse; `skipped()` is how you find out.

**Pages for responses.** A service that answers with rows builds a `Page` from
`PageLimits` and calls `push(&row)` for each row. A row must serialise to a JSON
object. `push` returns `Pushed::Full`, without writing, when the row would pass
the byte or row limit. The service then makes a `Cursor` from the key of that
row and calls `page.into_response(req.query(), next.as_ref())`.

- The defaults are 4 MiB and 10,000 rows (`DEFAULT_PAGE_BYTES`,
  `DEFAULT_PAGE_ROWS`). `PageLimits::new` clamps the bytes to at most
  `MAX_PAGE_BYTES`, 64 MiB.
- That maximum is half of the 128 MiB that m6-http will buffer from a backend.
  m6-http holds every backend body in memory, so a page is one complete response
  body that is sent whole. A test in m6-http keeps the maximum under that cap.
- The first row of a page is kept even when it is bigger than `max_bytes`, so a
  client can always make progress. A row bigger than `MAX_PAGE_BYTES` is an
  `InvalidData` error.
- The response is 200 with `Content-Type: application/x-ndjson`, `no-store` and
  `nosniff`. It is framed with `Content-Length`, has no ETag and is not
  compressed.
- With a next cursor, the response has `Link: <?QUERY>; rel="next"`. QUERY is the
  request's query with any `cursor` removed and the new one added, and bytes
  outside the URI query set are percent-encoded. A client follows the link until
  a response has none.
- `Cursor::new(s)` takes 1 to 128 bytes of `A-Za-z0-9-._~`, so it needs no
  encoding. `Cursor::from_query(query)` gives `None` when the key is absent, and
  a `CursorError` when it is repeated, empty or malformed. `to_response()` is a
  400 that does not repeat the value.

---

## 9. Errors

`error` is eighteen lines: an `Error` enum and
`Result<T> = std::result::Result<T, Error>`. `response::error_to_response` maps
one to a response.

## 10. Test kit

Feature `testkit`, from `[dev-dependencies]` only, so no production binary
links it.

- `testkit::process::Service`: `spawn`, `kill`, `terminate(timeout)`,
  `output()`, `assert_alive`, `wait_for_tcp`, `wait_for_path`.
  `assert_lifecycle_logged(name, output)` checks a service logged its start and
  stop.
- `testkit::port::claim_port() -> PortClaim`, which holds the port until dropped
  so two tests cannot race for it.
- `testkit::wait`: `for_path`, `for_tcp`, `for_unix`, `until`. 25 ms poll.
- `testkit::paths::binary(name)` builds the release binary from the current source, once per name per test process, and returns its path.
- `testkit::response::read_one(r, method)` reads exactly one response, HEAD
  included.

---

## 11. Known gaps in this crate

Recorded here because a reference that only describes the good parts is a
brochure.

- **`FrameworkState::build_dict` is private.** Twelve ordered steps of real
  knowledge that a service not using `App` cannot reach. This is the open
  "header to dict" item.
- **`h1`'s module doc calls `parse.rs` "(deleted)".** The file exists and is on
  the production path of every socket backend. What was deleted is the parser
  that used to be in it; it is now the stream adapter described in §6.
- **The inotify path in `watcher` had never been compiled** as of 2026-09-11.
  It is `#[cfg(target_os = "linux")]` and the development machine is a Mac.
- **A small number of ad-hoc case-insensitive header lookups remain outside
  core**, in `m6-http`: `cache.rs`, `http2.rs`, `redirect.rs`, `security.rs`,
  `main.rs`.
