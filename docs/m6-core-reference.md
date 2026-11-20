# m6-core, component reference

**Status: reference.**
Where it and the code disagree, the code is right and this document is a defect.

m6-core is the crate every m6 service links.
It supplies the loop parts, routing, configuration, the request dictionary and the response helpers.

This document describes every module of that crate and the interface it exposes.
`m6-architecture.md` gives the design: what the crate is for, where its boundary is, and the rule that decides what goes in.

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

This section shows the shape a service takes, the command line it answers and what the prelude gives.

m6-core is the only crate an m6 service links, and the first line of a service is the prelude.

```rust
use m6_core::prelude::*;

fn main() -> anyhow::Result<()> {
    let site = Site::load("my-service", ROUTES, &[], &NoTemplates);
    // The loop: a poll, a listener, the signal descriptor, the connection
    // slots, the deadlines, and one `match` on the event token.
}
```

`docs/m6-app-anatomy.md` §2 has that loop in full.

Every service started through `Site::load` takes `<site-dir> <config-path> [--log-level LEVEL] [--dump-config] [--version] [--version-full]`.
With fewer than two positional arguments it prints that usage and exits 2.

The prelude exports `Site`, `Handled`, `CodeRoute` and `RouteMethod`, `Dict`, `Error` and `Result`, `Request` and `Response`, `ServerConn`, `Read`, `Flush` and `head_slots`, the event module's `Timeouts`, `Slots` and `LoopTrace`, the platform layer's `Signals`, `Signal`, `Poll`, `Events`, `Interest` and `Token`, `listen_unix`, the three helpers `now_iso8601`, `slugify` and `today_iso8601` from `util`, `NoTemplates`, and `serde_json`'s `json!`, `Map` and `Value`.
Feature-gated additions, present when the feature is on: `Upload`, `TeraFactory` and `TeraRenderer`, `Message`, `Relay`, `RelayConfig`, `Smtp` and `Endpoint`, and `rusqlite`.

A service that renders no templates passes `NoTemplates` and takes core with `default-features = false`, which links no Tera.

Every module is public, so a tool can use the parsers in `telemetry` and `ndjson` without a server.

## 2. Feature flags

`flash` and `csrf` have no module of their own.
They gate code inside `site` and one method each, `Response::flash` and `Request::verify_csrf`.
Table 1 gives the eleven features.

| feature | default | what it adds |
|---|---|---|
| `templates` | **on** | `template` module, `TeraFactory`, `TeraRenderer`. Pulls tera and comrak. Without it a service passes `NoTemplates`. |
| `minify` | **on** | `minify` module, and the minification step in `site`. Pulls minify-html, which brings a CSS engine. Without it a config that turns minification on fails to load and names this feature. |
| `testkit` | off | The `testkit` module, and `alloc-count`. Enable from `[dev-dependencies]` only. |
| `alloc-count` | off | The counting allocator on its own, so a service binary can count its loop's allocations. |
| `alloc-trace` | off | A backtrace per allocation, for the `alloc_sites` diagnostic target. |
| `multipart` | off | `multipart` module, `Request::file`, `Request::write_bytes`. Pulls multer, bytes, futures-util, futures-executor. |
| `flash` | off | `Response::flash` and *HMAC* (hash-based message authentication code) signed one-shot messages. Pulls hmac. |
| `csrf` | off | *CSRF* (cross-site request forgery) token generation in the request dictionary, and `Request::verify_csrf`. No new dependencies. |
| `email` | off | `email` module: an SMTP client a service's loop drives (`Relay`, `Smtp`), and lettre for building messages. Pulls lettre. |
| `http-client` | off | `client::Endpoint`: a form POST to a URL resolved at start, driven by the service's loop on `client::Upstream`. Pulls url. |
| `sqlite` | off | `sqlite` module: `open`, `open_reader`, `migrate`, `append_only`, `used_bytes`, `prune_oldest`, and `rusqlite` in the prelude. Pulls rusqlite with bundled SQLite. |

**Table 1: the eleven features, their defaults and what each one compiles.**
Take from it that `templates` and `minify` are on by default, and that `csrf`, `alloc-count` and `alloc-trace` add no dependency.

`templates` and `minify` are on by default so that a service that renders or minifies gets them by depending on core.
`chrono` and `lru` are unconditional dependencies.
A service that needs neither names `default-features = false` and lists only the features it uses.
Built on its own (`cargo build -p`, `cargo install`), it then links no template engine, Markdown engine or CSS engine.
Cargo unifies features across one build, so in a `cargo build --workspace` `m6-html` turns both on for every crate.
`m6-http`, `m6-monitor`, `m6-auth`, `m6-auth-cli` and `m6-auth-server` take `default-features = false`, and `m6-file` names `minify` alone.

No workspace crate enables `multipart`, `flash`, `csrf`, `email` or `http-client`, so `cargo test --workspace` skips them.
A *CI* (continuous integration) step builds and tests m6-core with `--all-features` to cover them.

Section 3 lists the modules those features compile.

## 3. Component index

The component modules, six of them behind features, are grouped by the task that sends a reader to one.
Table 2 is the index, and it uses six abbreviations: *HTTP* (Hypertext Transfer Protocol), *RFC* (Request for Comments), *TCP* (Transmission Control Protocol), *JSON* (JavaScript Object Notation), *UTC* (Coordinated Universal Time) and *CSPRNG* (cryptographically secure pseudorandom number generator).

| group | module | what it is |
|---|---|---|
| **Building a service** | `site` | `Site`, `CodeRoute`, `Handled` and `RouteMethod`, the route table, the request dictionary and the site-wide finish. |
| | `config` | `RendererConfig`, `Secret` and the configuration file that produces it. |
| | `server` | `socket_path_from_config` and the blocking request and response adapters the benchmarks use. |
| | `event` | `Timeouts`, `Slots` and `LoopTrace`, the deadlines, connection table and accounting of a service's loop. |
| | `alloc` | `M6Alloc`, the allocator every m6 binary installs, with `never_return`, `reserve` and `note_growth`. |
| | `signal` | `block()` and `Lifecycle`. Signal-safe shutdown. |
| | `render` | The `Renderer` and `RendererFactory` traits, plus `NoTemplates`. The only thing `Site` needs from a template engine. A `Renderer` writes the page into the service's page area. |
| | `limits` | Named limits: `Limits`, `Limit`, `LimitSpec` and `Bounded<T>`. One `start` and `max` per buffer or queue, read from `[limits.<name>]`, with the one growth rule and the over-max error. `m6-app-anatomy.md` §4.2 lists the limits. |
| | `build_info` | `BuildIdentity`, the `build_identity!` macro, `emit` for a service's `build.rs`, `linked` and `handle_version_flags` for `--version` and `--version-full`. The constants are fixed at build time and the type comes from `m6-platform`. |
| | `startup` | `Record`, `RunId`, `Section`, `Setting`, `Source` and `ConfigTrace`: the start-up record and its shutdown line. One call at start and one at a clean exit. `docs/m6-log.md` section 9 gives the line shapes. |
| | `template` | The Tera implementation of those traits, and the site filters. Feature `templates`. |
| | `client` | `Upstream`, `Addr`, `TlsClient`, `Reply` and `system_roots`: one outgoing HTTP request a loop drives. `Endpoint` is behind `http-client`. |
| | `email` | `Relay`, `RelayConfig`, `Smtp` and `SmtpError`: an SMTP client a loop drives. Feature `email`. |
| **Request and response** | `dict` | The request dictionary: a shared base plus a per-request overlay. |
| | `request` | `Request`, the handler's view: the dictionary, body access, file I/O helpers, query and form parsing, and percent-encoding. |
| | `response` | `Response`, every constructor a handler needs, and `send`. |
| | `http` | `RawRequest`, `RawResponse`, `Method`, the `HeaderSource` trait and `is_same_origin_path`. |
| | `auth` | `Identity`, `Answer`, `Decision` and `Require`: the identity a `/check` answers, the `role:<name>` or `group:<name>` matcher, the answer parser and the head decision. No I/O. |
| | `headers` | Reading and writing a header list, once. `get`, `get_all`, `combine`, `set`, `append`, `remove`. |
| | `cookie` | `Cookie` and `SameSite`. One place that says what a cookie can be. |
| | `multipart` | `Upload` and `parse_upload`. Feature `multipart`. |
| | `time_range` | `TimeRange`, the `from` and `to` of a query read as a half-open UTC range, and `rfc3339_utc_ms`. |
| **HTTP semantics** | `h1` | The HTTP/1.1 parser, plus `ServerConn`, `Responder`, `keep_alive`, `status_reason` and `Expectation`. |
| | `parse` | The stream adapter over `h1`. Reads bytes until `h1` says the message is complete. |
| | `conditional` | RFC 9110 13.2 preconditions. `evaluate_preconditions` and `is_not_modified`. |
| | `negotiate` | Content-coding negotiation, RFC 9110 12.5.3, with real q-values. |
| | `mime` | `mime_from_path` and `should_compress_default`. |
| | `path` | `validate_path_param` and `resolve_within`. A security boundary. |
| **Content** | `compress` | brotli and gzip, compress and decompress. |
| | `minify` | The four minifiers: markup, JavaScript, stylesheets and JSON. Feature `minify`. |
| | `util` | `slugify`, `now_iso8601`, `today_iso8601`, `iso8601_minutes_ago` and `html_escape`. |
| | `random` | `random_hex_token::<N>()`. One CSPRNG token generator. |
| **Observability** | `log` | `init`, `tick`, `watch`, `flush`, `stop`, `Line`, the five macros and `LogPulse`. The buffered JSONL logger every service loop drives, re-exported from `m6-log`. |
| | `telemetry` | m6's own formats read back: `AnalyticsRecord`, `PeriodicStats`, `TrafficSummary`, and the probe and crawler heuristics. |
| | `monitoring` | `/health`, and the metrics and traffic documents: `HealthReport`, `PerfReport`, `TrafficReport` and `LoggingHealth`. |
| | `resolve` | Reverse DNS with forward confirmation, `Ptr` re-exported. The lookup is `dns::Confirm`, driven by a service's loop. |
| | `dns` | `Confirm`, `Query`, `Ptr` and `nameserver`: one DNS question on its own non-blocking UDP socket. |
| | `platform` | The platform layer, the one crate that knows the operating system: readiness, the clock, signals, a child's exit, sockets, limits, directories, host and process facts. `docs/m6-platform.md`. Its `host` module is what the machine is doing: load, memory, disk, thermal, net, diskstats, pressure, *TCP* and file descriptors. Its `facts` module is what the host says it is, read once at start: `host_facts()`. |
| | `tls` | How long the loaded certificate has left and its fingerprint: `not_after_unix`, `Fingerprint`, `record_loaded`, `loaded` and `TlsCertificate`. |
| | `firewall` | `FirewallState` from nftables' own JSON. |
| | `wireguard` | `Handshakes` from `wg show <if> latest-handshakes` output. |
| | `ndjson` | Newline-delimited JSON, read and written, tolerant of torn lines, and served as bounded pages with a cursor. |
| **Storage** | `sqlite` | One SQLite file with the house settings, versioned migrations and append-only tables. Feature `sqlite`. |
| **Everything else** | `net` | `is_private_ip`, `bind_ip`, `resolve`, `listen_unix` and `listen_tcp`. The one definition of a private address, the start-time resolver and the two listeners. |
| | `error` | `Error` and `Result`. |
| | `testkit` | The shared integration harness. Feature `testkit`. |

**Table 2: the component modules, their group and one line on what each holds.**
Take from it that the group names the task, and the module name is the string to search for in the section that follows.

Sections 4 and 5 take the two largest groups in turn, and sections 6 to 10 take the rest.

## 4. Building a service

This section covers the modules a service's `main` builds from: the site's request path and routing, the configuration it reads, the socket helpers, signal handling and the renderer interface.
It uses *HTTP* (Hypertext Transfer Protocol) for the wire protocol and *TOML* (Tom's Obvious, Minimal Language) for the configuration format.

### 4.1. The parts a loop uses

**A service's loop is written in its own `main` from eight parts.**
Table 3 names them, and `docs/m6-app-anatomy.md` §2 is the loop in full.

| part | module | what it supplies |
|---|---|---|
| the reactor | `platform::Poll` | `Poll`, `Events`, `Token`, `Interest` and the non-blocking sockets in `platform::net` |
| the signal descriptor | `platform::Signals` | a shutdown request as a readable descriptor, and `Signal` values |
| the deadlines | `event::Timeouts` | wake me at a time for a token, and the timeout to pass to the poll |
| the connection table | `event::Slots` | slots made at start, their growth to a ceiling, and the free list |
| the accounting | `event::LoopTrace` | the wait against the work, per trigger |
| one connection | `h1::ServerConn` | read, parse, buffer and write through the slot's own buffers |
| the listener | `net::listen_unix` | a non-blocking unix listener with its mode set |
| the site | `site::Site` | routes, the request dictionary, templates and the finish |

**Table 3: the parts a service's loop builds from.**
Take from it that the loop is assembled from these eight parts, one per concern.

### 4.2. `config`

`config::load(config_path, site_dir) -> anyhow::Result<RendererConfig>`.
`config::load_traced(config_path, site_dir) -> anyhow::Result<(RendererConfig, ConfigTrace)>` returns the same config and the trace that says which file, flag or environment variable set each key.
`config::Secret<T>` is a value that serialises as `<redacted>`, so a config field declared with it cannot reach a log line.
The struct is:

```rust
pub struct RendererConfig {
    pub user_config:   Map<String, Value>,   // arbitrary service keys
    pub global_params: Vec<String>,
    pub routes:        Vec<RouteConfig>,
    pub params_cache:  ParamsCacheConfig,    // size
    pub server:        ServerConfig,         // read_timeout, socket_mode
    pub compression:   HashMap<String, CompressionLevel>,  // per-mime brotli/gzip
    pub minification:  MinificationConfig,   // per-mime enabled
    pub log:           LogConfig,            // level
    pub limits:        Limits,               // [limits.<name>], start and max per buffer
    pub site_toml:     Option<PathBuf>,      // the reserved key site_toml
    pub site_size_budget_mb: Option<u64>,    // top-level key, read by m6-file
}
```

`site_toml` names the `site.toml` this service reads.
`RendererConfig::resolve_site_toml(site_dir)` returns the path: `<site_dir>/site.toml` when the key is absent, the value as given when it is absolute, and `site_dir` joined with it when it is relative.
A non-string or empty value fails `config::load`.
`Site::site_toml` carries the resolved path.
A `site.toml` that does not exist, cannot be read or is not valid TOML exits 2 at start, whether the path is named or the default.

A `RouteConfig` carries ten fields: `path`, `template`, `params`, `status`, `cache`, `cache_explicit`, `methods`, `headers`, `handler` and `settings`.
`handler` names a handler the service passes to `Site::load`, and `settings` keeps the per-route keys core does not define.
`MinificationConfig::is_enabled(mime)` answers the per-type question, and `toml_to_json` converts a TOML value to JSON.

`RendererConfig::effective_json(site_dir)` returns the full effective configuration, defaults included, as the start-up record carries it.
`RendererConfig::startup_sections(site_dir, trace)` splits that value into one section per table, with each setting's `source`.

`[server] read_timeout_s` is the read deadline applied to every accepted connection, defaulting to `server::DEFAULT_READ_TIMEOUT_SECS` (30).
`0` means no timeout.
It is read once at startup and applied at accept time.

`[server] socket_mode` is the mode applied to the unix socket after bind, defaulting to `server::DEFAULT_SOCKET_MODE` (`0o660`).
It is an **octal string** such as `"0660"`, as systemd writes it, because TOML has no octal literal and `660` as a decimal integer is `0o1224`.
`listen_unix` sets the mode, and a failure there stops the service at start.

A value that cannot be parsed, a negative timeout or a mode above `0777` fails `config::load` and the service does not start.
Every service answers `--dump-config`, so a deploy can validate a config before starting it, and it exits 2 when the config is refused.

### 4.3. `server`

`socket_path_from_config(config_path)` derives the socket path from the config path, and `M6_SOCKET_OVERRIDE` replaces it.

`apply_socket_mode(path, mode)`, `bind_tcp_reuseaddr(addr)` and the constants `DEFAULT_SOCKET_MODE`, `DEFAULT_READ_TIMEOUT_SECS` and `MAX_REQUESTS_PER_CONN` (100) are the start-time helpers and limits shared by every service and `m6-http`.

`serve_connection(stream, handler)` is the blocking per-connection loop, and `parse_request` and `write_response` are its companions.
Benchmarks and tests that drive a socket directly use them.
A service's loop uses `h1::ServerConn`.

### 4.4. `signal`

**`signal::block()` must be the first statement of every `main`.**
It blocks SIGTERM, SIGINT and SIGUSR1 in the calling thread.
Threads inherit the signal mask at creation, so a `main` that starts a thread before blocking has a thread with SIGTERM unblocked.
The kernel delivers a process-directed SIGTERM there, and the default disposition kills the process before the loop reads the shutdown request.
The shutdown line is then lost.

`Signals::new()` asserts that the mask is set and panics otherwise.
`Lifecycle::on_signal` decides what each signal means and `complete()` ends the lifecycle.

The only signal a service acts on is a termination: SIGTERM or SIGINT, read from one descriptor.
The first stops the loop, and a second before the loop has ended forces an exit.
SIGUSR1 is read and discarded.
Every log line goes to stdout or to journald's native socket, so no signal reopens a log file.

### 4.5. `render` and `template`

`render` holds the interface between `Site` and a template engine.
A route that names a template produces a `Response` carrying `template_name` and `template_dict`, a `Renderer` turns that into bytes, and a `RendererFactory` builds one at start.
`Renderer::render` writes the page into a `Bounded<u8>`, the one page area a service makes at the `render_page` limit's start and reuses for every page.
A page past the limit's maximum is `RenderError::TooLarge`, and the client gets 500.
`NoTemplates` implements both traits and errors if anything asks it to render.

`template` is the Tera implementation: `build_tera`, `build_tera_from_paths`, `TeraRenderer`, `TeraFactory` and `NOT_FOUND_SENTINEL`, which is how a template signals a 404 from inside rendering.

Both builders add the site filters and functions in Table 4 to the Tera instance, beside Tera's own.

| name | kind | what it does |
|---|---|---|
| `slugify` | filter | turns `Hello World` into `hello-world` |
| `date_format(fmt="%B %d, %Y")` | filter | formats a date string, parses RFC 3339 and `%Y-%m-%d`, and returns anything else unchanged |
| `markdown` | filter | renders Markdown to HTML with comrak |
| `truncate_words(n=50)` | filter | keeps the first `n` words, default 50, and appends `…` past that |
| `asset` | filter | returns a content-addressed URL for a file under `assets/`, from a manifest built at start |
| `img_dims(path=x)` | function | returns the ` width="W" height="H"` attribute pair, nothing when the image has no readable size, and needs `\| safe` because it returns markup |
| `not_found()` | function | fails the render with the 404 sentinel, so a missing lookup becomes a 404 |

**Table 4: the site template filters and functions.**
Take from it that `asset` and `img_dims` are computed from tables built at start, and that `not_found()` turns a missing lookup into a 404.

### 4.6. `site`

`Site` is the site's request handling as plain functions on a value the service owns.
A service's loop calls `Site::load` once at start, then per request `handle`, its own handler for the index, and `finish`.
A route the service answers itself comes back as `Handled::Code` or `Handled::Named` with the index of its route.

**Start.** `Site::load` reads the command line, the config, `site.toml`, the logging settings, the route table, templates and params.
It exits 2 on a configuration error and 1 when logging cannot start, and it refuses a route that names a handler nothing provides.
`name` is the service's name for the lifecycle lines.
`read_timeout`, `socket_mode` and `socket_path` are what the loop binds and applies, and `user_config`, `config`, `site_dir`, `config_path` and `site_toml` are the roots a service resolves its own files against. `config` is the whole parsed renderer config, for a start check a service runs itself.

**A service's own config and `--dump-config`.** `Site::load` answers `--dump-config` before a `Site` exists, so that answer cannot see a table the service reads itself.
A service with such a table calls `load_deferring_dump` instead, reads the table with the statements the start uses, and calls `dump_requested` and then `dump` before it resolves a name, opens a file or binds a socket.
The dump then refuses what the start refuses, and it prints the same bytes the shared parser prints.

```rust
pub const GENID_PATH: &str;
pub fn load(name: &str, code: &[CodeRoute], named: &[&str], renderer: &dyn RendererFactory) -> Site;
pub fn load_deferring_dump(name: &str, code: &[CodeRoute], named: &[&str], renderer: &dyn RendererFactory) -> Site;
pub fn dump_requested(&self) -> bool;
pub fn dump(&self, code: &[CodeRoute], named: &[&str]) -> i32;
pub fn reload(&self, code: &[CodeRoute], named: &[&str], renderer: &dyn RendererFactory) -> Result<Site, String>;
pub fn socket_path(&self) -> &Path;
pub fn read_timeout(&self) -> Option<Duration>;
pub fn socket_mode(&self) -> u32;
pub fn user_config(&self) -> &Map<String, Value>;
pub fn config(&self) -> &crate::config::RendererConfig;
pub fn site_dir(&self) -> &Path;
pub fn config_path(&self) -> &Path;
pub fn site_toml(&self) -> &Path;
pub fn name(&self) -> &str;
pub fn handle(&self, head: &RequestHead<'_>) -> Handled;
pub fn finish(&self, req: &Request, resp: Response) -> Response;
pub fn genid_ok() -> Response;
pub fn genid_failed(cause: &str) -> Response;
```

**Per request.** `handle` routes, builds the dictionary and the owned `Request`, and either answers a template route itself or returns the service's index.
`Done` is already finished.
`Code` and `Named` carry the request, and the service calls `finish` on the response its handler returned.
`Reload` is `POST /genid`: the loop runs the start path again with `reload`, assigns the new `Site` when that is `Ok`, and answers `genid_ok()` or `genid_failed(&e)`.
A method other than POST on `/genid` is a 405 with `Allow: POST`, and a route on `/genid` is refused at start.
The service's dispatch is a plain `match` on that index, written out in the turn loop in section 6.1.

`finish` renders a template the response names, then applies the route's `Cache-Control`, `Last-Modified` and headers, the CSRF and flash cookies, minification and compression.

**Routing.** `compile_pattern` turns `/blog/{stem}` into `Vec<Segment>`, and `match_route` returns `PathParams` as a `Vec<(String, String)>` because order is meaningful and duplicates are possible.
`find_route` picks the winner.
`RouteTable` indexes the compiled routes: a fixed route is one hash lookup on its path, and `find_route` tries the pattern routes only when no fixed route answers.
`check_route_conflicts` runs once when a config loads and refuses what would need a tie-break: two fixed routes on one path, or two patterns that can both match some path, where the same request method reaches both.
`Methods` is the set of methods a route answers, so a conflict needs the sets to meet and `find_route` tests one bit.
m6-http builds `RouteDecl`s from `site.toml` and calls the same check.

**The request dictionary.** `FrameworkState::build_dict` builds the dictionary, and a service reaches it through `Site`.
Three layers do not run per request: config keys, global params files and a route's static params files are merged once at start into `route.base_dict`.
The per-request order is:

- Params files whose path holds a `{placeholder}`.
- Path params, each validated.
  A `{*name}` capture may span segments and a `{name}` capture may not.
- Query params, inserted at top level and as a nested `query` map.
  A query key with the name of one of the route's path params is only in `query`, so it cannot replace the validated path value.
- POST form fields, from `application/x-www-form-urlencoded` only.
  A field with the name of one of the route's path params is dropped, for the same reason.
  A non-empty body of any other type logs a warning.
- Cookies, as a nested `cookies` map.
- Built-ins `request_path`, `datetime` and `year`, after the params files so a params file cannot override them.
- Auth claims from `x-auth-claims`, each present only when its claim is: `auth_username` (claim `username`, else `sub`), `auth_sub`, `auth_groups` and `auth_roles`.
- Flash message, when the feature is on: verify the HMAC, insert `flash`, and the response path adds the removal cookie.
- CSRF token, when the feature is on and the dictionary holds no `csrf_token`: reuse the `_csrf` cookie or generate one, and insert it as `csrf_token`.
  A submitted form token stays in place for `verify_csrf` to compare.

A request with more entries than the `request_dict` limit's maximum is a 400.
Error keys need no step, because they arrive as query parameters at the query step.

### 4.7. `email` and `client::Endpoint`

A service that sends mail or posts a form does both as work its own loop drives.
The `email` feature compiles the `email` module: `Relay`, `RelayConfig`, `Smtp` and `SmtpError`, plus lettre for building a `Message`.
The `http-client` feature compiles `client::Endpoint` on the `client::Upstream` a loop already drives.
Neither blocks, and neither starts a thread.

**email.** `Relay::new(&RelayConfig { .. })` runs at start.
`RelayConfig` carries `host`, `port`, `starttls`, `credentials` and `timeout`.
`Relay::new` refuses plain SMTP (no `starttls`) to any host but `127.0.0.1`, `::1` or `localhost`, resolves the host once with `net::resolve`, and reads the EHLO name.
A host that does not resolve is an error, so the service stops at start, and a relay whose address changes needs a restart.
`Smtp::new(read_cap, write_cap)` makes one slot per outgoing table entry, with its buffers.
`Smtp::start` keeps the bytes from `Message::formatted()` and begins a non-blocking connect.
`fd()` is `None` while the slot has no connection.
The service registers `fd()` with `interest()`, keeps the deadline from `deadline()` in its outgoing `Timeouts`, and calls `drive()` when the token fires.
An outgoing event for a slot that is no longer sending is ignored, because the client can leave and free the slot earlier in the same poll batch.
`Ok(None)` reregisters and returns to the loop, `Ok(Some(()))` means the relay accepted the message, and `Err` names the step, the relay's code and its first line.
The service answers 504 when its deadline passes first, and `close()` drops the connection and frees the slot.
The connection's state is one `enum` of steps inside the slot, so the service's loop has one arm per outgoing token.

**client::Endpoint.** `Endpoint::new(url, tls)` runs at start: it parses an `http` or `https` URL, resolves the host once, and keeps the `Host` header, path and query.
`post_form(&mut up, fields, deadline)` encodes `application/x-www-form-urlencoded` straight into the `Upstream`'s request buffer and starts it, and the service drives the `Upstream` as it drives an `Smtp`.
Each call opens one connection and sends `Connection: close`.
`m6_core::rustls` re-exports the TLS library, so a service names `m6_core::rustls::ClientConfig` and takes no `rustls` dependency of its own.

### 4.8. `build_info`

`build_info` answers the two version flags from constants fixed at build time.
`build_identity!()` builds a `BuildIdentity` from the `M6_BUILD_*` variables the service's own `build.rs` emitted, so each crate reports its own version, commit and dirty flag.
`emit()` is the one line in that build script. It runs `git rev-parse --short=12 HEAD` and `git status --porcelain --untracked-files=no`, so only a modified or staged tracked file marks the build dirty, and writes `unknown` and a clean tree when git is absent.
`linked()` returns the calling crate's identity followed by its m6 dependencies' identities, depth first. Each of those crates emitted its own constants, so `--version-full` shows a mixed build.
`handle_version_flags(own, linked)` prints the tag form for `--version` and the per-crate lines for `--version-full`, then exits. `m6_core::version_flags!` is the one-call form a service uses at the top of `main`, and it also registers the binary's identity for the start-up record. It names the first line for the binary with `CARGO_BIN_NAME` through `for_binary`, so the seven extra binaries of the m6-http package each name themselves and list `m6-http` second.
`BuildIdentity`, `emit` and `full_lines` come from `m6-platform`, which sits below m6-core.

### 4.9. `startup`

**The start-up record is several `type=startup` lines, and a clean exit adds one `type=shutdown` line.**
`m6_core::startup::Record` builds it, `Record::emit` writes it through the operational log, and `startup::shutdown(reason)` writes the exit line.
A service built on `Site` gets both with no call of its own: `Site::prepare` writes the record, and `Lifecycle::complete` writes the shutdown line.

An application with its own config and loop makes two calls.
The identity line names the binary when the binary registers itself, and `version_flags!` at the top of `main` does that as it answers the version flags:

```rust
m6_core::version_flags!(m6_core::build_info::linked());
```

`Record::new(app, argv)` starts the record with the identity `version_flags!` registered, the host from `HostReport::decided`, and the default limits.
`Record::node_name(name)` states the application's configured node name on every line, which is `[node] name` for m6-http, so two instances on one host stay two nodes.
The record takes the effective config as sections and a `ConfigTrace` that labels each setting `file`, `default`, `env` or `flag`.
`ConfigTrace::add_config_file` records a file's path, its role and its sha256 as read.
`Invocation::read` reads the declared flags and environment variables and records the settings they replaced.
`LimitsReport::read` records every named limit's start, max and source, and `Record::limits` replaces the default with a service's own.
`record_log_level` writes the level the process runs at into the `log` section, because `site.toml` and `--log-level` come after the config file.
`HostReport::decided` is the platform line's host block, from `m6_core::platform::host_facts()` (section 8.5.1).
A config field that holds a credential is declared `m6_core::Secret<T>`, which serialises as `<redacted>`, and `startup::names_a_secret` is the name rule for a value with no type.
[`m6-log.md`](m6-log.md) §9 gives what the record writes: every line's keys, the `node` rule and the three redaction rules.

Section 5 turns to the objects a handler receives and returns.

## 5. Request and response

This section covers the objects a handler touches: the request, the response, the *HTTP* (Hypertext Transfer Protocol) types behind them, the header and cookie helpers, and the time range a query can ask for.
It ends with the credential decision that runs before a handler, and section 6 turns to the parsers those objects read from.

### 5.1. `request`

`Request` is what a handler receives: `method()`, `path()`, `header(name)`, `content_type()`, `body_raw()` and `body_json()`.

`field(name)` reads a form or query field.
`get(key)` and `Index<&str>` read the dictionary, and `dict()` borrows the whole thing.

File I/O on `Request` is relative to the site directory: `read_json`, `write_json`, `write_json_atomic` and `list_json` read and write there.
`write_bytes` does the same and needs the `multipart` feature.
Each one resolves the path through `path::resolve_within`, which follows symlinks and refuses a `..` component, a leading `/` and any path that resolves outside the site directory, with a 400.
`write_json_atomic` writes a temp file and renames it, so it is the one to use when a reader may be running.
`site_path(rel)` joins a path to the site directory and checks nothing.
A handler that builds a path from request input resolves it with `path::resolve_within` first.

`verify_csrf()` needs feature `csrf`, and `file(name) -> Result<Upload>` needs feature `multipart`.

Free functions used by `site`: `parse_query_string`, `parse_form_body`, `url_decode`, `url_encode`, `url_encode_path`, `cookie`, `parse_cookies`, `parse_auth_claims`, `validate_path_param` and `single_query_value`.

`single_query_value(query, key)` returns the raw value of a key that may appear once: `None` when absent, `RepeatedKey` when repeated.
`time_range`, `ndjson::Cursor` and a service's own filter parameters all read through it, so one query string follows one rule.

`url_encode` and `url_decode` are the percent coders for a *URL* (Uniform Resource Locator).

### 5.2. `response`

Constructors: `render`, `render_with`, `render_status`, `render_dict`, `redirect`, `redirect_permanent`, `json`, `json_status`, `html`, `text`, `stream`, `status`, `not_found`, `forbidden`, `bad_request` and `refusal`.

`refusal(&message)` is the 400 for a refused request parameter: the message and a newline as plain text, with `no-store` and `nosniff`.
The message must not echo client input.

`stream(status, len, reader)` sends a body of `len` bytes read from `reader`.

Builders: `body`, `with_status`, `header`, `cookie` and `flash` (feature `flash`).
`cookie(name, value, max_age)` sets `Path=/` and `HttpOnly`, and a `max_age` of 0 deletes the cookie.
A cookie that needs `Secure` or `SameSite` is built with `Cookie` and pushed with `header`.

`send(&mut h1::Responder)` writes it, and `error_to_response(&Error)` is the default mapping from an error to a response.

### 5.3. `http`

`RawRequest` is the parsed wire request: `method()`, `path()`, `query()`, `header()`, `content_type()` and `accept()`.
`RawResponse::new(status)` with `.header()`, `.body()`, `.content_type()`, then `send` or `to_bytes`.

`Method` is a newtype over `&'static str` with the constants `GET`, `POST`, `PUT`, `DELETE`, `PATCH` and `HEAD`.

`HeaderSource` lets header lookup work over `[(String, String)]`, `Vec<(String, String)>` and a `Response`'s `Vec<Header>`.
`HeaderText` borrows a `&'static str`, shares an `Arc<str>`, holds up to 23 bytes inline or owns a `String`.
`header(headers, name)` is the case-insensitive first-match lookup.

`is_same_origin_path(candidate)` is the redirect guard: it answers whether a path is safe to redirect to without leaving the origin.

### 5.4. `headers`

This module reads and writes a header list once: `get`, `get_all`, `contains`, `set`, `append`, `set_if_absent` and `remove`.

`combine(headers, name)` folds repeated field lines into one comma-separated value and **refuses when the field must not be folded**.
`NEVER_COMBINED` is the list and `set-cookie` is on it: *RFC* (Request for Comments) 6265 3 allows commas in cookie values, so joining two `Set-Cookie` lines produces one malformed cookie and loses the other.
`may_combine(name)` asks in advance.

The module also defines the two request-head limits every protocol and service shares.
`MAX_HEADER_LINE` is 8 KiB for one field, and `MAX_REQUEST_HEAD` is 32 KiB for the request line plus every field.
They follow the limits nginx, Varnish and Apache document, and the module doc names each setting.
`request_line_len(method, target)` counts a request line, and `field_over_limit(name, value)` answers the field question.

### 5.5. `cookie`

`Cookie::new(name, value)` then `.max_age()`, `.path()`, `.domain()`, `.secure()`, `.http_only()`, `.same_site(SameSite::...)`, then `to_header_value()` or `to_header()`.
`Cookie::removal(name)` builds an expiry.

### 5.6. `time_range`

`TimeRange::from_query(query, max_span)` and `TimeRange::from_request(req, max_span)` read `from` and `to`.
`DEFAULT_MAX_SPAN` is 31 days.

- Both keys are required and each may appear once.
  Other keys are ignored, so a cursor or a filter passes through.
- A value is *RFC* 3339 in *UTC* (Coordinated Universal Time) with an uppercase `T` and a final `Z`, such as `2026-01-01T00:00:00Z`.
  Fractions of a second are accepted, and an offset such as `+10:00` is refused.
- The range is half open, `[from, to)`.
  `contains(t)` is `from <= t < to`, so two adjacent ranges never share a row.
- `from` is at or after 1970-01-01T00:00:00Z, `to` is later than `from`, and the span is at most `max_span`.
- `from_unix_ms`, `to_unix_ms`, `from_unix_s` and `to_unix_s` round up, so for integer timestamps `from_ms <= t < to_ms` is exactly `[from, to)`.

`TimeRangeError::to_response()` is a 400 in plain text with `no-store` and `nosniff`.
It says which parameter to fix and never repeats the value the client sent.

`rfc3339_utc_ms(unix_ms)` formats a millisecond timestamp as `2026-01-01T00:00:00.123Z`.
It returns a value that formats when displayed, so a row can carry it without a `String` per line, and a negative or out-of-range input gives the epoch.

### 5.7. `auth`

`m6_core::auth` is where the credential decision lives.
It does no I/O and starts no thread.

- `Identity` is what a `/check` 200 answers: `sub`, `username`, `roles` and `groups`.
- `Require::parse("role:<name>")` and `Require::parse("group:<name>")` are the one definition of a route's requirement, and `check_require(identity, require)` matches it.
  Any other form denies.
- `parse_check(status, body)` is total: a 200 with all four fields is `Answer::Identified`, a 401 is `Answer::Refused`, and every other status or a 200 that does not parse is `Answer::NoDecision`.
  A 403 is `NoDecision`.
- `Answer::decide(require)` maps to `Decision`: `Allow(identity)`, `Deny`, `NoCredential` or `NoDecision`.
  `Decision::refusal()` returns the 401 or 403 bytes from `http::token_refusal`.
  `NoDecision` returns `None` and is answered 503, so a caller must match it before reading `None` as permission.
- `encode_claims_header(identity)` writes the four keys as base64 JSON for `X-Auth-Claims`, and no token claim survives.

A 401 is the authority's decision, and a 503 is its absence.
`NoDecision` maps to a 503 because a 401 tells a user with a valid credential to log in again.

---

## 6. HTTP semantics

Section 6 covers the modules on the wire: the *HTTP* (Hypertext Transfer Protocol) parser and connection slot, the blocking stream adapter, conditional requests, content-coding negotiation, MIME types, path parameters and the network helpers.
It uses *RFC* (Request for Comments) for the specifications it names.

### 6.1. `h1`

**`parse_request(buf) -> ParseResult` is the one HTTP/1.1 parser, and it is pure.**
It leaves proxy-owned headers in place, because stripping them is ingress policy.
`ParseResult` is `Complete(RawRequest)`, `Incomplete` or `Error(HeadError)`.

The four h1 conformance targets score 32/32 on h1spec, the independent RFC 9112 tester, and `tools/conformance.sh` holds them there (`tools/conformance-scores.txt`).

`Expectation` and `expectation(buf)` handle `Expect: 100-continue`, and `CONTINUE_RESPONSE` is the reply.
`keep_alive(&req)` decides connection reuse, and `status_reason(status)` maps a code to its phrase.

`Responder::new(w, method, keep_alive)` is the write side.
It writes with `send`, `send_with_length`, `send_stream` and `error`, and `keeps_alive()` and `body_bytes()` answer the connection decision and the bytes written.
It omits the body on a HEAD at every status.
`write_head_to` is the same head written straight into a caller's buffer, for a caller whose writer is already a buffer.

**`ServerConn`** is one HTTP/1.1 connection slot for a service's own loop.
`new(read_cap, write_cap)` makes it at start, `attach` takes an accepted `m6_core::platform::net::UnixStream`, `detach` gives it back, and `stream()` is what the reactor registers.
`read` reads until the socket would block and frames what is buffered.
A whole request is then parsed with `parse_head_uninit` into the caller's fixed header slots and returned as `Read::Request(head)`, or `read` reports `Read::More` or `Read::Closed`.
It keeps the request's method, the keep-alive decision and the served count on the slot, so `respond` needs no request argument.

`respond(status, headers, body)` queues a short answer, and `respond_response(Response)` queues a handler's.
`flush` writes until the socket would block and reports `Done` (everything written, connection keeping alive), `More` (the socket would block with output still queued, so write again when it is writable) or `Close` (everything written and the connection closes).
A `Body::Stream` body is pumped 64 KiB per writable turn, one `read(2)` of the file per turn.
A request past the input buffer grows it by doubling, clamped at the `request_body` maximum plus `MAX_REQUEST_HEAD`, and the buffer keeps the grown size.

The header slots are `MaybeUninit` and made with `head_slots()`.
The head borrows them for one call, so the array is made where it is used and costs no writes.

The turn loop:

```rust
loop {
    let mut heads = h1::head_slots();
    let read = conn.read(&mut heads)?;
    // The head borrows the slot, so its answer is built here and written
    // after the match, when the borrow has ended.
    let (response, idle) = match read {
        Read::Request(head) => {
            let response = match site.handle(&head) {
                Handled::Done(response) => response,
                Handled::Code(index, req) => {
                    let response = service_route(index, &req);
                    site.finish(&req, response)
                }
                Handled::Named(index, req) => {
                    let response = service_route(index, &req);
                    site.finish(&req, response)
                }
                Handled::Reload(reload) => {
                    let response = match site.reload(ROUTES, &[], &NoTemplates) {
                        Ok(next) => {
                            *site = next;
                            genid_ok()
                        }
                        Err(e) => genid_failed(&e),
                    };
                    finish_reload(&reload, response)
                }
            };
            (Some(response), false)
        }
        Read::More => (None, true),
        Read::Closed => break,
    };
    if let Some(response) = response {
        conn.respond_response(response);
    }
    match conn.flush()? {
        // Everything is written and the connection keeps alive. A pipelined
        // request may already be buffered, so read again before waiting.
        Flush::Done if !idle => {}
        // Everything is written and this turn has nothing more to read.
        Flush::Done => {
            let interest = conn.interest();
            reregister(conn, interest, token);
            break;
        }
        // The socket would block with output still queued: register with
        // `conn.interest()` (WRITABLE here) and flush again when it is.
        Flush::More => {
            let interest = conn.interest();
            reregister(conn, interest, token);
            break;
        }
        // Everything is written and this connection closes.
        Flush::Close => break,
    }
}
```

The service's own loop registers the token with `conn.interest()` after every turn, so a turn that ends waiting for the socket to be readable leaves a READABLE registration, and a turn that owes output leaves a WRITABLE one.
When a WRITABLE event arrives, the loop calls `conn.flush()` and re-registers with `conn.interest()` again, which is what continues a `Flush::More`.

**Allocation.** `ServerConn` allocates nothing per request, and a request past the input buffer or a streamed body past the output buffer is the one case that grows a buffer.

### 6.2. `parse`

`parse_request(stream) -> Result<RawRequest, ParseError>`.

**This is a stream adapter over `h1`.**
It reads into a buffer and asks `h1::parse_request` whether the message is complete, so it has no framing logic of its own.
It takes `Write` as well as `Read`, because a client that sent `Expect: 100-continue` will not send its body until told to, and answering that has to happen before the read that would otherwise deadlock.

The head is capped at `MAX_REQUEST_HEAD` by the shared parser, and the body at `[limits.request_body] max`, 16 MiB by default, answered 413 past it.
It reads 1024-byte chunks.

Benchmarks and tests drive it.

`ParseError::status()` and `.reason()` give the response for a parse failure, including 414 for a request line over `MAX_HEADER_LINE` and 400 for a field or a head over its limit.

### 6.3. `conditional`

`evaluate_preconditions(...) -> Precondition`, `is_not_modified(...)`, `not_modified_headers(cached)`.

RFC 9110 13.2 preconditions, including `If-Match` and `If-Unmodified-Since`, evaluated in one place.

### 6.4. `negotiate`

`coding_quality(accept_encoding, coding) -> Option<f32>`, `preferred_coding(accept_encoding, candidates)`, `canonical_coding(accept_encoding) -> &'static str`.

The `Accept-Encoding` q-values are parsed, and `q=0` means "not acceptable" (RFC 9110 12.4.2).
`canonical_coding` narrows a header to the one coding this server will serve, and it is the intended cache-key form.
No caller uses it yet: `m6-http` keys its response cache on the raw `Accept-Encoding` text, so `gzip` and `gzip, deflate, br, zstd` are two entries for one byte-identical response.

### 6.5. `mime` and `path`

`mime_from_path(path)`, `should_compress_default(mime)`.

`validate_path_param(value, allow_slash) -> Result<&str, PathParamError>` is a security boundary.
It allows alphanumerics, `-`, `_` and `.` (and `/` when `allow_slash` is set), and it refuses `..` anywhere in the value.
`resolve_within(root, rel) -> Result<PathBuf, ResolvePathError>` follows symlinks and refuses `..`, a leading `/` and any result outside `root`.

### 6.6. `net`

`is_private_ip(ip)` is the one answer to "is this on a private network".
`bind_ip(bind)` pulls the address out of `ip:port`, `[v6]:port` or a bare address, and gives `None` for a host name.
`resolve(host, port)` returns an IP literal at once, and otherwise the first IPv4 address the system resolver gives, or the first address when there is no IPv4 one.
It blocks, so it runs at start only.

Table 5 lists the ranges `is_private_ip` treats as private.

| family | private |
|---|---|
| IPv4 | `127.0.0.0/8`, `10.0.0.0/8`, `172.16.0.0/12`, `192.168.0.0/16`, `169.254.0.0/16`, `100.64.0.0/10` |
| IPv6 | `::1`, `fc00::/7`, `fe80::/10` |

**Table 5: the address ranges `is_private_ip` treats as private.**
Take from it that loopback, link-local, unique-local and the carrier-grade NAT range all count, and that the unspecified addresses do not.

An IPv4-mapped IPv6 address is classified by its IPv4 form.
A check on a peer address only means something on a socket that faces the client directly, and behind a proxy the peer is the proxy.

`listen_unix(path, mode)` removes a stale socket file, creates the parent directory, binds non-blocking and sets the mode.
`listen_tcp(addr)` binds non-blocking with `SO_REUSEADDR`, so a restart can rebind a port whose connections are in `TIME_WAIT` while a live listener on it is still refused.
Both are for start only and return `Err` on failure.

Section 6 covers the parser through the network helpers, and section 7 turns to content: compression, minification, the utility helpers and SQLite.

---

## 7. Content

Section 7 covers the content helpers a response path uses: compression, minification, the utility and random helpers, and the one SQLite file a service keeps.
It uses *WAL* (write-ahead log) and *CSPRNG* (cryptographically secure pseudorandom number generator).
The four small modules come first, and SQLite has the rest of the section.

`compress` provides `brotli_compress(data, quality)`, `brotli_decompress(data)`, `gzip_compress(data, level)` and `gzip_decompress(data)`.

`minify` (feature `minify`) provides `minify_html(data)`, `minify_css(data)` and `minify_json(data)`.
There is no JavaScript minifier, and `minify_html` leaves an inline `<script>` as written.

`util` provides `slugify`, `today_iso8601`, `now_iso8601`, `iso8601_minutes_ago` and `html_escape`.

`random` provides `random_hex_token::<N>()` and `random_hex_token_into`, the one CSPRNG token generator.

### 7.1. `sqlite`

Feature `sqlite`.
Re-exports `rusqlite`.
`open(path)` opens one database file with the settings in Table 6.
`migrate(&mut conn, &[Migration])` applies a versioned list kept in `PRAGMA user_version`, and `schema_version` reads it.
`append_only(&conn, table)` adds triggers that refuse UPDATE and DELETE, and `is_append_only` reports them.
`used_bytes` and `prune_oldest` keep a table under a size cap.
The cap counts the whole file, and `prune_oldest` returns `Pruned { rows_deleted, cap_met }`.
`OpenOptions` changes the busy timeout, the journal mode (`Journal::Rollback` or `Journal::Wal`) and foreign keys.
`open_reader(path)` opens an existing file read-only with the busy timeout and nothing else, for a reader beside the writer.
`open_read_only(path)` opens an existing WAL database for reading beside a writer, sets `query_only`, and leaves the journal mode and schema alone.

| setting | value | reason |
|---|---|---|
| `journal_mode` | `DELETE` | The database is one file at rest. WAL leaves `-wal` and `-shm` beside it, and a copy of the main file alone can miss committed work. |
| `busy_timeout` | 5 s | A second connection waits for a lock. |
| `foreign_keys` | on | SQLite has them off in every new connection. The value is read back. |
| `recursive_triggers` | on | `INSERT OR REPLACE` fires DELETE triggers only with this on, so the append-only rule needs it. |

**Table 6: the settings `open` applies, and why.**
Take from it that the file is one file at rest.
Each setting is read back, and a setting that does not take effect is an error.

**Migrations** number 1, 2, 3 in order, and the list is checked before anything runs.
Each runs in its own `BEGIN IMMEDIATE` transaction with the version re-read inside it, so connections that open at once apply each migration once.
A failing migration rolls back.
A file newer than the list is refused with `NewerThanCode`, before the first migration and again inside each transaction.
`migrate` switches `foreign_keys` off before each migration, so a migration can rebuild a table that others point at.
It runs `PRAGMA foreign_key_check` before and after the migration's SQL inside the transaction, compares the dangling references, and rolls back with `ForeignKeyCheck` when the migration adds one.
A released migration is never edited.
Use one journal mode for every connection to a file, because SQLite cannot leave WAL while another connection is using it.

**Append-only** triggers are named `<table>_append_only_update` and `<table>_append_only_delete`.
Call `append_only` after `migrate` on every open, and it restores triggers that were dropped.
The triggers guard against mistakes in code.
Anyone who can write the file can drop them.
A table that is pruned must not be append-only.

Section 7 covers the content helpers and the one database module, and section 8 turns to observability.

---

## 8. Observability

Section 8 covers observability: logging, telemetry, the monitoring documents, reverse DNS, host readings, TLS, firewall, WireGuard and NDJSON.
It uses *TLS* (Transport Layer Security), *DNS* (Domain Name System), *NDJSON* (newline-delimited JSON) and *ACME* (Automatic Certificate Management Environment).
The section runs from the loop's own log line to the certificate a client is served.

### 8.1. `log`

`m6_core::log` re-exports the `m6-log` crate and adds `watch`.
`init(level)` sets the level and opens the Sink on stdout at start.
The Sinks and the level are thread-local, and a process has one loop thread.
`read_site_log_config(site_toml)` reads `[log] level` from `site.toml`, and `parse_level(s)` turns a level name into a `Level`.
`Site` applies the precedence: the command line, then the service's `[log]`, then `site.toml`'s `[log]`.

A service's loop calls the logger in plain sight.
`tick(now)` flushes the Sink on the one-second accounting tick.
`watch(registry, token)` registers stdout for writability while the operational Sink holds bytes a full destination would not take, and removes it when they drain.
The `LOG` arm calls `flush()`.
`stop()` runs after the loop: it restores stdout's flags, writes everything buffered, then the dropped-count line.
`Line` and the `error!`, `warn!`, `info!`, `debug!` and `trace!` macros build one line into a Sink without allocating.

`pulse() -> &'static LogPulse` is the liveness signal: `events()` counts the lines the level let through, and `seconds_since_last()` answers "has anything been logged recently", so a silent log reads as a fault.

`read_tail(path, max_bytes)` reads the end of one plain or gzip file.

### 8.2. `telemetry`

`m6-http` emits analytics through `m6_core::log`'s `Line`, and the line has the keys `timestamp` and `fields.user_agent`.
`AnalyticsRecord` and `AnalyticsFields` define that line, `Serialize` as well as `Deserialize` so the write side can adopt them.
`RequestView` is one request as the traffic document reads it, borrowed from a parsed record (`AnalyticsRecord::view`) or from the traffic window's slot, with its time as a `Stamp`, the envelope's text or Unix microseconds.
`RequestView.truncated` is the window's own fact that a cap cut the path or user agent, and a parsed record never sets it.
`parse_analytics(...)` reads a stream.
`PeriodicStats::parse(line)` reads the `periodic stats` log line, `strip_ansi` first because the journal carries colour.
`aggregate_windows(...) -> StatsAggregate` sums them.

The heuristics and thresholds are named constants: `claims_to_be_bot`, `looks_like_probe`, `looks_like_injection`, `BOT_MARKERS`, `PROBE_MARKERS`, `INJECTION_MARKERS`, `PROBE_PATHS_FOR_CONCERN` (3), `REFUSED_REQUESTS_FOR_CONCERN` (10), `UA_ROTATION_THRESHOLD` (10) and `UA_ROTATION_MIN_REQUESTS` (20).

`ClientSummary::is_notable()` and `error_ratio()`, `CrawlerSighting`, and `TrafficSummary::from_records(...)` or `from_views(...)` assemble the picture.
A client is notable on an injection path, a rotating user agent, `PROBE_PATHS_FOR_CONCERN` probe paths, or `REFUSED_REQUESTS_FOR_CONCERN` requests with at least half refused.
Volume alone earns no fault.

### 8.3. `monitoring`

`/health` answers `ok` or `degraded`.
m6-http answers it before routing, the cache and any backend, so it costs a JSON serialisation and nothing else.
The metrics and traffic documents are `type` `perf` and `type` `traffic` log lines, which a monitor reads from the journal, so no HTTP endpoint serves them and no credential opens them.

`HealthReport::build(node)` and `HealthReport::degraded(node)` return `(u16, HealthReport)`, and `into_response(code)` serialises it with `no-store`.
`build` is 200 `ok`, and `degraded` is 503 with the status `degraded`, whose body names the node and no dependency.
The answer covers the m6-http process and the dependency flags the process already holds: an edge's origin is the follower's own `down` bit, so the answer reads a flag and makes no probe.
A socket pool keeps no per-member health state, so a dead member is found when a request connects to it and that request takes the next member.

`PerfReport::build(subject, metrics)` builds the metrics document with no access check, for the periodic metrics line.
`monitoring::not_found()` is the 404 a disabled or unknown monitoring path answers, with the metrics headers and no credential challenge.
m6-http keeps a `/health` poll out of the site counters with `is_monitoring_endpoint`, described in `docs/m6-http.md`.

`PerfReport.tls` carries the served certificate chain, how long each certificate has left, and each certificate's SHA-256 fingerprint, from `tls::loaded()`.
It is `serde(default)`, so an aggregator can parse a payload from an older node, and `TlsCertificate.fingerprint` is `serde(default)` for the same reason.

`PerfReport.logging` carries `LoggingHealth`, which is also on the traffic document.
It is the line count the level let through and the age of the last line, and it is `serde(default)`, so a node older than the field still parses.
`LoggingHealth::read()` builds it, `QUIET_SECONDS` is 40, and `is_blind()` is the silent-log fault.

`TrafficReport::from_records(node, records, window_minutes)` summarises records that are already parsed, and `TrafficReport::from_views(node, views, window_minutes)` summarises borrowed views, which is how `m6-http` builds its traffic document from the in-memory window without a record per request.
`events_dropped` says how many events that window overwrote before they left it, so a short window is not read as a quiet hour.
`NotableClient`, `HeavyHitter` and `CrawlerReport` are its parts, and `CrawlerReport` carries `first_seen` and `last_seen`, both `serde(default)`.

### 8.4. `resolve`

`resolve` checks a claim against its source with reverse DNS.
A lookup is a network round trip, so it runs in a monitor's own loop, outside the serving path.

The rule lives in `dns`.
`Confirm::start(ip, ns)` asks the *PTR* (pointer) question for an address on one UDP socket, then asks the name it gets forward, and answers `Ptr` only after that second answer.
The caller owns the deadline and reads `Ptr`, whose four outcomes are Table 7.

| `Ptr` | meaning |
|---|---|
| `Name(n)` | the PTR name `n` resolves back to the address |
| `Unconfirmed(n)` | the PTR name `n` exists and its forward lookup completed without the address, a false claim |
| `None` | the address has no PTR record, unverified |
| `Failed` | the lookup did not complete: a socket error, a truncated reply or the deadline, unverified |

**Table 7: the four outcomes of a confirmed reverse lookup.**
Take from it that a missing record and a failed lookup are different answers, and that only `Name` is a confirmed claim.

`Confirm` never waits.
`fd()` gives the descriptor to register, `drive()` moves the current question as far as it goes without waiting and changes `fd()` when the PTR step ends, and the outcome arrives once.
A query dropped at its deadline closes its socket, and the caller records `Failed`.
A truncated reply is a failure, because an A, AAAA or PTR answer fits in UDP.
`dns::nameserver()` reads the first nameserver in `/etc/resolv.conf` once, at start.

`Ptr::verifies(domain)` matches a name against a domain on a dot boundary, so `example.com.attacker.net` does not match `example.com`.
`telemetry::verify_crawler(user_agent, &ptr)` turns a `Ptr` into a `CrawlerVerdict` using the operator's documented method.

`resolve` re-exports `Ptr`.
`m6-monitor`'s fleet poll drives `Confirm` on its own loop and keeps the answers.

### 8.5. `platform::host`

**`snapshot(disk_path) -> HostSnapshot`** assembles every reading below into the struct the metrics document publishes.

**`HostReader`** keeps a `HostSnapshot` current inside an event loop. `HostReader::new(disk_path)` reads the facts fixed for the process once (CPU count, descriptor limits, the cgroup memory limit, the thermal zones) and one full pass. Each `step()` then reads one `Source`, one file or one call, and returns which. `snapshot()` borrows the block.

Readers are platform-gated and return `None`, an empty `Vec` or a core count of 1 when the host gives nothing, so a missing reading leaves the rest of the snapshot intact: `load_average`, `memory`, `disk`, `cpu_count`, `thermal_zones`, `uptime`, `net_devices`, `disk_io`, `pressure(resource)`, `tcp_health` and `file_descriptors`.

Pure parsers over the text these files contain, tested on any platform: `parse_loadavg`, `parse_meminfo`, `parse_cgroup_memory`, `parse_thermal_millidegrees`, `parse_uptime`, `parse_net_dev`, `parse_diskstats`, `parse_pressure`, `parse_netstat` and `parse_sockstat_into`.

Types: `HostSnapshot`, `LoadAverage`, `Memory`, `MemorySource`, `Disk`, `ThermalZone`, `NetDevice`, `DiskIo`, `Pressure`, `TcpHealth` and `FileDescriptors`.

Two readings carry a rule:

- **A container's memory limit is the one that applies.** `/proc/meminfo` reports the machine, and `memory()` prefers the cgroup limit when one applies and says which it used in `Memory::source`.
- **The space a service can use is `f_bavail`.** `f_bfree` counts the blocks reserved for root, typically 5%, and `disk()` reports `f_bavail`.

The module sets no threshold: a monitoring service decides what 80% means.

### 8.5.1. `platform::facts`

**`host_facts() -> HostFacts`** reads what the host says it is, once, at start.
It never fails and never panics.
It is reached as `m6_core::platform::host_facts()`, and `docs/m6-platform.md` lists what it reads on each operating system, including the cgroup rule.

**`Fact<T>` is either `Known(T)` or `Unknown(&'static str)`.**
The reason is a short static phrase naming what was tried, such as `cannot read /proc/meminfo`, so a monitor can print why a fact is unknown.
`HostFacts` carries the host name, the *FQDN* (fully qualified domain name), every interface address with its name and scope, the OS, the kernel, the CPU, the memory, the limits, the clock and the virtualisation type.
A group read from more than one source keeps each member a `Fact`, so a host that answers the CPU count and not the model loses neither.

The serialised form is `{"known": value}` or `{"unknown": "reason"}`, so every key is present in a start-up record.
A `clock.boot_time` that is known is RFC 3339 in UTC, written by `write_rfc3339_utc`, the same date text the log line writes.

The reads that are the same on both families are in `facts.rs`.
The reads that differ are one function each in `linux.rs` and `macos.rs`: `host_os_facts`, `host_cpu`, `host_memory`, `host_limits`, `host_clock` and `host_virtualization`.
The parsers are public and pure, so a missing file, malformed text and a rolling release with no `VERSION_ID` are held on any target.

For the cgroup limits, `max` answers `Known(None)`, a number answers `Known(Some(bytes))`, and only a file that cannot be read or parsed answers `Unknown`.
`read_cgroup_memory` and `read_cgroup_cpu` take the mount point as a parameter, so a test builds a fixture hierarchy in a temporary directory.

### 8.6. `tls`

**`not_after_unix(der) -> Result<i64, CertificateError>`** reads `notAfter` out of a DER certificate as seconds since the Unix epoch.
**`record_loaded(chain)`** stores a loaded chain and returns the certificates it could not read, by depth and reason.
**`loaded()`** and **`loaded_at(now_unix)`** return the stored chain as `Vec<TlsCertificate>`, leaf first, each carrying `depth`, `not_after_unix`, `expires_in_seconds` and `fingerprint`.
**`unreadable()`** counts the certificates the last `record_loaded` could not read.
**`Fingerprint::of(der)`** is SHA-256 over a certificate's DER, printed as 64 lower-case hex characters.

**The fingerprint tells apart two certificates that expire on the same day.**
SHA-256 covers the whole certificate, so a renewal that reissues with the same key pair changes the serial number, the validity and the signature, and the fingerprint moves with all three.
The digest is the one `openssl x509 -noout -fingerprint -sha256` reports, printed here in lower-case hex with no colons.
`fingerprint` is `None` on a certificate that arrived from a node older than the field, and a reader must treat that as "cannot say".

**The module reports the certificate the process loaded.**
A renewal that wrote a new file without restarting the server leaves the file and the served certificate different.
`record_loaded` is called by whatever loads the certificate, and `loaded()` opens no file, so polling a monitoring endpoint does no disk work.

Table 8 lists the four behaviours of a loaded chain.

| behaviour | effect |
|---|---|
| a chain, leaf first | `depth` 0 is what an *ACME* client renews, and intermediates expire too. A chain is good until its soonest expiry |
| `expires_in_seconds` goes negative | how long ago a certificate expired is a number a reader can act on |
| an unreadable certificate comes back in the return value | the server keeps serving it and a monitor can say "cannot say" |
| a later `record_loaded` replaces the earlier one | the registry holds what every protocol serves |

**Table 8: what the loaded chain reports.**
Take from it that an unreadable certificate is reported as absent, and that `expires_in_seconds` goes negative after expiry.

The module sets no threshold, as `host` sets none.

**`not_after_unix` parses the DER by hand.**
`notAfter` sits at a fixed structural position in every X.509 certificate, so the walk steps over six elements by length and reads the seventh.
Every length is checked against the buffer, and a test runs every truncation of a generated certificate through it.
The dates go through `chrono`.

### 8.7. `firewall`

`FirewallState::from_nft_json(text)`, `from_file(path)`, `blocked_addresses()`, `active()` and `Block`.

nftables emits JSON natively.
`nft list ruleset` needs `CAP_NET_ADMIN`, which the process answering public requests must not hold, so a privileged timer writes `nft -j list ruleset` output to a file and m6 reads the file.
It reports the per-address blocks and whether they are still being hit.

### 8.8. `wireguard`

`Handshakes::parse_latest_handshakes(text)`, `from_file(path)`, `find(key)` and `PeerHandshake`.

It reads a file because WireGuard state needs `CAP_NET_ADMIN`.
A privileged timer writes the output of `wg show <if> latest-handshakes`, and m6 reads it as ordinary data.
A time of 0 means the peer never completed a handshake, and `latest_unix` is `None`.
A missing file is `Ok(None)`, and a malformed line is an error naming the line.

The file must not hold `wg show <if> dump` output, because it carries the interface private key.

The time is the last successful handshake.
After a key changes under a tunnel it stops moving, and it still looks recent for minutes.
An age is therefore the slow signal, and a probe over the tunnel is the fast one.

### 8.9. `ndjson`

`read_str(text)`, `Reader::new(r)` with `skipped()` and `read_count()`, `write_one`, `write_all` and `to_string`.

**Torn lines.**
An NDJSON file is usually being appended to while it is read, so its last line can be half-written.
Readers skip lines they cannot parse, and `skipped()` counts them.

**Pages for responses.**
A service that answers with rows builds a `Page` from `PageLimits` and calls `push(&row)` for each row.
A row must serialise to a JSON object.
`push` returns `Pushed::Full` and leaves the page as it was when the row would pass the byte or row limit: the row is serialised into the buffer and truncated again, so peak memory is the byte limit plus the largest single row.
The service then makes a `Cursor` from the key of that row and calls `page.into_response(req.query(), next.as_ref())`.

- The defaults are 4 MiB and 10,000 rows (`DEFAULT_PAGE_BYTES`, `DEFAULT_PAGE_ROWS`).
  They follow the `ndjson_page` limit once a service has called `ndjson::set_page_limit`, whose `start` is the page size and whose `max` is the largest row.
  `PageLimits::new` clamps the bytes to at most `MAX_PAGE_BYTES`, 64 MiB.
- m6-http holds every backend body in memory, up to 128 MiB, so a page is one complete response body that is sent whole.
- The first row of a page is kept even when it is bigger than `max_bytes`, so a client can always make progress.
  A row bigger than `MAX_PAGE_BYTES` is an `InvalidData` error.
- The response is 200 with `Content-Type: application/x-ndjson`, `no-store` and `nosniff`.
  It is framed with `Content-Length`.
- With a next cursor, the response has `Link: <?QUERY>; rel="next"`.
  QUERY is the request's query with any `cursor` removed and the new one added, and bytes outside the URI query set are percent-encoded.
  A client follows the link until a response has none.
- `Cursor::new(s)` takes 1 to 128 bytes of `A-Za-z0-9-._~`, so it needs no encoding.
  `Cursor::from_query(query)` gives `None` when the key is absent, and a `CursorError` when it is repeated, empty or malformed.
  `to_response()` is a 400 that carries no copy of the value.

Section 8 covers what the process and the machine report, and section 9 turns to the error type a handler returns.

---

## 9. Errors

Section 9 covers the error type a handler returns and the one place it becomes a response.

`error` holds an `Error` enum and `Result<T> = std::result::Result<T, Error>`, and `response::error_to_response` maps a variant to a status: `NotFound` is 404, `Forbidden` is 403, `BadRequest` is 400, and `Other` is 500 with the detail logged.

A path parameter containing `..` is a `NotFound`, so a refusal does not confirm that the traversal was recognised.

Section 10 turns to the test kit that drives these answers.

---

## 10. Test kit

Section 10 covers the shared integration harness a test uses to drive a real service: the process, port, wait, path, response and check helpers.

Feature `testkit`, from `[dev-dependencies]` only, so no production binary links it.

- `testkit::process::Service`: `spawn`, `kill`, `terminate(timeout)`, `output()`, `assert_alive`, `wait_for_started`, `wait_for_tcp` and `wait_for_path`.
  `assert_lifecycle_logged(name, output)` checks a service logged its start and stop.
- `testkit::port::claim_port() -> PortClaim`, which holds the port until dropped so two tests cannot race for it.
- `testkit::wait`: `for_path`, `for_tcp`, `for_unix` and `until`, on a 25 ms poll.
- `testkit::paths::binary(name)` returns the path of a workspace binary this test run built, without running cargo, and panics with the command to run when the binary is absent.
- `testkit::response::read_one(r, method)` reads exactly one response, HEAD included.
- `testkit::check::CheckServer` is a fake `/check` authority on a Unix socket.
  `answer(token, Reply)` sets the table, `serve_one()` accepts and answers one queued connection with no thread, and `calls()` counts check requests.
  `Reply` can answer a 200 identity, a 401, a 403, a 503, any other status, a slow answer, a hold, or a close in the middle of an answer.
  `close()` stops listening and leaves the socket file with no listener, so the next connect is refused.

`STARTUP_BOUND` and `RESPONSE_BOUND` are 60-second ceilings on a start and a response.
The harness waits on the event or the process exit, and section 11 records what the crate still lacks.

---

## 11. Known gaps in this crate

This section records what the crate and its migration still leave undone.

- **Ad-hoc case-insensitive header lookups remain outside core.** They are in `m6-http`: `cache.rs`, `http2.rs`, `redirect.rs`, `security.rs` and `main.rs`, and each is a local lookup that the one `headers` module can replace.

---

## Summary

m6-core is the one crate a service links, and it supplies the parts of a service.
Sections 4 and 5 cover the modules that build a service and the ones that read a request and write a response.
Sections 6 to 8 cover HTTP semantics, content and observability, section 9 the errors every module returns, and section 10 the test kit.
Section 11 lists what the crate still lacks.
`m6-architecture.md` explains why each module exists, and `CHANGELOG.md` lists what changed in each release.
