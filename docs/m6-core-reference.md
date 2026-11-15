# m6-core, component reference

**Status: reference.**
This document describes every module in m6-core.
Where it and the code disagree, the code is right and this document is a defect.

m6-core is the crate every m6 service links.
It supplies the loop parts, routing, configuration, the request dictionary and the response helpers, so a service that wants one of those does not write it again.

This document is the reference for that crate: every module and the interface it exposes.
Table 1 gives the two m6-core documents and the job each one does.

| file | job |
|---|---|
| `m6-architecture.md` | **Design.** What the crate is for, where its boundary is, and the rule that decides what goes in. |
| this file | **Reference.** Every component and its interface. |

**Table 1: the two m6-core documents and the job each one does.**
Take from it that a question about why a module exists belongs in the design document, and a question about a function's name or argument belongs here.

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

This section shows the shape a service takes, the prelude, and what the prelude gives.

m6-core is the only crate an m6 service links, and the intended first line is the prelude.

```rust
use m6_core::prelude::*;

fn main() -> anyhow::Result<()> {
    let site = Site::load("my-service", ROUTES, &[], &NoTemplates);
    // The loop: a poll, a listener, the signal descriptor, the connection
    // slots, the deadlines, and one `match` on the event token.
}
```

`docs/m6-app-anatomy.md` §2 has that loop in full.

The prelude is deliberately small.
It exports `Site`, `Handled`, `CodeRoute` and `RouteMethod`, `Dict`, `Error` and `Result`, `Request` and `Response`, `ServerConn`, `Read`, `Flush` and `head_slots`, the event module's `Timeouts`, `Slots` and `LoopTrace`, the platform layer's `Signals`, `Signal`, `Poll`, `Events`, `Interest` and `Token`, `listen_unix`, the three helpers `now_iso8601`, `slugify` and `today_iso8601` from `util`, `NoTemplates`, and `serde_json`'s `json!`, `Map` and `Value`.
Feature-gated additions appear in it only when the feature is on: `Upload`, `TeraFactory` and `TeraRenderer`, `Message`, `Relay`, `RelayConfig`, `Smtp` and `Endpoint`, and `rusqlite`.

A service that renders no templates says so, and takes core with `default-features = false` to stop linking Tera.

```rust
use m6_core::prelude::*;

let site = Site::load("my-service", ROUTES, &[], &NoTemplates);
```

Every module is public, and a tool can use the parsers in `telemetry` and `ndjson` without a server at all, so m6-core is a library of parts.
The loop belongs to the service, and `docs/m6-app-anatomy.md` is the shape it follows.

## 2. Feature flags

Features and modules are different axes, and `flash` and `csrf` have no module at all.
They gate code inside `site` and one method each, `Response::flash` and `Request::verify_csrf`.
A reader who greps for a `csrf` module finds nothing and wrongly concludes the feature is missing.
Table 2 gives the nine features.

| feature | default | what it adds |
|---|---|---|
| `templates` | **on** | `template` module, `TeraFactory`, `TeraRenderer`. Pulls tera and comrak. Without it a service passes `NoTemplates`. |
| `minify` | **on** | `minify` module, and the minification step in `site`. Pulls minify-html, which brings a CSS engine, and minify-js. Without it a config that turns minification on fails to load and names this feature. |
| `testkit` | off | The `testkit` module. Enable from `[dev-dependencies]` only. |
| `multipart` | off | `multipart` module, `Request::file`. Pulls multer, tokio, bytes, futures-util. |
| `flash` | off | `Response::flash` and *HMAC* (hash-based message authentication code) signed one-shot messages. Pulls hmac, sha2. |
| `csrf` | off | *CSRF* (cross-site request forgery) token generation in the request dictionary, and `Request::verify_csrf`. No new dependencies. |
| `email` | off | `email` module: an SMTP client a service's loop drives (`Relay`, `Smtp`), and lettre for building messages. Pulls lettre without its transport. |
| `http-client` | off | `client::Endpoint`: a form POST to a URL resolved at start, driven by the service's loop on `client::Upstream`. Pulls url. |
| `sqlite` | off | `sqlite` module: `open`, `open_reader`, `migrate`, `append_only`, `used_bytes`, `prune_oldest`, and `rusqlite` in the prelude. Pulls rusqlite with bundled SQLite. |

**Table 2: the nine features, their defaults and what each one compiles.**
Take from it that `templates` and `minify` are on by default, seven features pull a dependency and two add code only.

`templates` and `minify` are on by default because a feature that hides code from `cargo test --workspace` is how two `csrf` tests stayed broken from Phase 4 to Phase 5.
`chrono` and `lru` are unconditional dependencies for the same reason.
A service that uses neither takes core with `default-features = false` and names only the features it uses, and then, built on its own (`cargo build -p`, `cargo install`), links no template engine, Markdown engine or CSS engine.
Cargo unifies features across one build, so in a `cargo build --workspace` `m6-html` turns both features on for every crate.
`m6-http`, `m6-monitor`, `m6-auth` and `m6-auth-server` do this, and `m6-file` names `minify` alone.

Every off-by-default feature is invisible to `cargo test --workspace`: `testkit`, `multipart`, `flash`, `csrf`, `email`, `http-client` and `sqlite`.
A *CI* (continuous integration) step builds and tests m6-core with `--all-features` to cover them.

The two defaults are the whole of the choice: templating arrives without asking, and every other block is opt-in.
Section 3 lists the modules those features compile.

## 3. Component index

The component modules, five of them behind features, are grouped by the task that sends a reader to one.
The prelude in section 1 is a `pub mod` in `lib.rs` that re-exports the items the other modules implement.
Table 3 is the index, and it uses six abbreviations: *HTTP* (Hypertext Transfer Protocol), *RFC* (Request for Comments), *TCP* (Transmission Control Protocol), *JSON* (JavaScript Object Notation), *UTC* (Coordinated Universal Time) and *CSPRNG* (cryptographically secure pseudorandom number generator).

| group | module | what it is |
|---|---|---|
| **Building a service** | `site` | `Site`, `CodeRoute`, `Handled` and `RouteMethod`, the route table, the request dictionary and the site-wide finish. |
| | `config` | `RendererConfig` and the configuration file that produces it. |
| | `server` | `socket_path_from_config` and the blocking request and response adapters the benchmarks use. |
| | `signal` | `block()` and `Lifecycle`. Signal-safe shutdown, and SIGUSR1 to reopen the log file. |
| | `render` | The `Renderer` and `RendererFactory` traits, plus `NoTemplates`. The only thing `Site` needs from a template engine. |
| | `template` | The Tera implementation of those traits, and the site filters. Feature `templates`. |
| | `client` | `Upstream`, `Addr`, `TlsClient`, `Reply` and `system_roots`: one outgoing HTTP request a loop drives. `Endpoint` is behind `http-client`. |
| | `email` | `Relay`, `RelayConfig`, `Smtp` and `SmtpError`: an SMTP client a loop drives. Feature `email`. |
| **Request and response** | `dict` | The request dictionary: a shared base plus a per-request overlay. |
| | `request` | `Request`, the handler's view: the dictionary, body access, file I/O helpers, query and form parsing, and percent-encoding. |
| | `response` | `Response`, every constructor a handler needs, and `send`. |
| | `http` | `RawRequest`, `RawResponse`, `Method`, the `HeaderSource` trait and `is_same_origin_path`. |
| | `headers` | Reading and writing a header list, once. `get`, `get_all`, `combine`, `set`, `append`, `remove`. |
| | `cookie` | `Cookie` and `SameSite`. One place that says what a cookie can be. |
| | `multipart` | `Upload` and `parse_upload`. Feature `multipart`. |
| | `time_range` | `TimeRange`, the `from` and `to` of a query read as a half-open UTC range, and `rfc3339_utc_ms`. |
| **HTTP semantics** | `h1` | The HTTP/1.1 parser, plus `ServerConn`, `Responder`, `keep_alive`, `status_reason` and `Expectation`. |
| | `parse` | The stream adapter over `h1`. Reads bytes until `h1` says the message is complete. |
| | `conditional` | RFC 9110 13.2 preconditions. `evaluate_preconditions` and `is_not_modified`. |
| | `negotiate` | Content-coding negotiation, RFC 9110 12.5.3, with real q-values. |
| | `mime` | `mime_from_path` and `should_compress_default`. |
| | `path` | `validate_path_param`. A security boundary. |
| **Content** | `compress` | brotli and gzip, compress and decompress. |
| | `minify` | The four minifiers: markup, JavaScript, stylesheets and JSON. Feature `minify`. |
| | `util` | `slugify`, `now_iso8601`, `today_iso8601`, `iso8601_minutes_ago` and `html_escape`. |
| | `random` | `random_hex_token::<N>()`. One CSPRNG token generator, so there is never a third. |
| **Observability** | `log` | `init`, `tick`, `watch`, `flush`, `reopen`, `stop`, `Line`, the five macros and `LogPulse`. The buffered JSONL logger every service loop drives, re-exported from `m6-log`. |
| | `telemetry` | m6's own formats read back: `AnalyticsRecord`, `PeriodicStats`, `TrafficSummary`, and the probe and crawler heuristics. |
| | `monitoring` | `/health` and `/perf`: `HealthReport`, `PerfReport`, `TrafficReport` and `LoggingHealth`. |
| | `resolve` | Reverse DNS with forward confirmation, `Ptr` re-exported. The lookup is `dns::Confirm`, driven by a service's loop. |
| | `platform` | The platform layer, the one crate that knows the operating system: readiness, the clock, signals, a child's exit, sockets, limits, directories, host and process facts. `docs/m6-platform.md`. Its `host` module is what the machine is doing: load, memory, disk, thermal, net, diskstats, pressure, *TCP* and file descriptors. |
| | `tls` | How long the loaded certificate has left: `not_after_unix`, `record_loaded`, `loaded` and `TlsCertificate`. |
| | `firewall` | `FirewallState` from nftables' own JSON. |
| | `wireguard` | `Handshakes` from `wg show <if> latest-handshakes` output. |
| | `ndjson` | Newline-delimited JSON, read and written, tolerant of torn lines, and served as bounded pages with a cursor. |
| **Storage** | `sqlite` | One SQLite file with the house settings, versioned migrations and append-only tables. Feature `sqlite`. |
| **Everything else** | `net` | `is_private_ip`, `bind_ip`, `listen_unix` and `listen_tcp`. The one definition of a private address, and the two listeners. |
| | `error` | `Error` and `Result`. |
| | `testkit` | The shared integration harness. Feature `testkit`. |

**Table 3: the component modules, their group and one line on what each holds.**
Take from it that the group names the task, and the module name is the string to search for in the section that follows.

The index is the map for the rest of the document.
Sections 4 and 5 take the two largest groups in turn, and sections 6 to 10 take the rest.

## 4. Building a service

This section covers the modules a service's `main` builds from: the site's request path and routing, the configuration it reads, the socket helpers, signal handling and the renderer seam.
It uses *HTTP* (Hypertext Transfer Protocol) for the wire protocol and *TOML* (Tom's Obvious, Minimal Language) for the configuration format.

### 4.1. The parts a loop uses

**A service's loop is written in its own `main` from four groups of parts.**
Table 4 names them, and `docs/m6-app-anatomy.md` §2 is the loop in full.

| part | module | what it supplies |
|---|---|---|
| the reactor | `platform::Poll` | `Poll`, `Events`, `Token`, `Interest` and the non-blocking sockets in `platform::net` |
| the signal descriptor | `platform::Signals` | a shutdown or reload request as a readable descriptor, and `Signal` values |
| the deadlines | `event::Timeouts` | wake me at a time for a token, and the timeout to pass to the poll |
| the connection table | `event::Slots` | slots made at start, their growth to a ceiling, and the free list |
| the accounting | `event::LoopTrace` | the wait against the work, per trigger |
| one connection | `h1::ServerConn` | read, parse, buffer and write through the slot's own buffers |
| the listener | `net::listen_unix` | a non-blocking unix listener with its mode set |
| the site | `site::Site` | routes, the request dictionary, templates and the finish |

**Table 4: the parts a service's loop builds from.**
Take from it that every part is a plain type with methods, and that none of them calls the service back.

`m6-http` is a proxy with its own loop and datagram sockets, and a tool can use the parsers in `telemetry` and `ndjson` without a server at all.
Every module is public, so the parts are usable one at a time.

### 4.2. `config`

`config::load(config_path, site_dir) -> anyhow::Result<RendererConfig>`.
The struct is:

```rust
pub struct RendererConfig {
    pub user_config:   Map<String, Value>,   // arbitrary service keys
    pub global_params: Vec<String>,
    pub routes:        Vec<RouteConfig>,
    pub params_cache:  ParamsCacheConfig,    // size
    pub server:        ServerConfig,         // read_timeout, socket_mode
    pub compression:   HashMap<String, CompressionLevel>,  // per-mime brotli/gzip
    pub minification:  MinificationConfig,   // per-mime enabled, inline_js
    pub log:           LogConfig,            // level
    pub site_toml:     Option<PathBuf>,      // the reserved key site_toml
}
```

`site_toml` names the `site.toml` this service reads.
`RendererConfig::resolve_site_toml(site_dir)` returns the path: `<site_dir>/site.toml` when the key is absent, the value as given when it is absolute, and `site_dir` joined with it when it is relative.
A non-string or empty value fails `config::load`.
`Site::site_toml` carries the resolved path, set whether or not the file exists.
A service that names a `site_toml` that does not exist, cannot be read or is not valid TOML exits 2 at start.

A `RouteConfig` carries nine fields: `path`, `template`, `params`, `status`, `cache`, `methods`, `headers`, `handler` and `settings`.
`handler` names a handler the service passes to `Site::load`, and `settings` keeps the per-route keys core does not define.
`MinificationConfig::is_enabled(mime)` answers the per-type question, and `toml_to_json` is the conversion used throughout.

`[server] read_timeout_s` is the read deadline applied to every accepted connection, defaulting to `server::DEFAULT_READ_TIMEOUT_SECS` (30).
`0` means no timeout.
It is read once at startup and applied at accept time.

`[server] socket_mode` is the mode applied to the unix socket after bind, defaulting to `server::DEFAULT_SOCKET_MODE` (`0o660`).
It is an **octal string**, as systemd writes it, because TOML has no octal literal and `660` as a decimal integer is `0o1224`.
A chmod that fails stops the service at start with exit 2.

Neither key silently falls back.
A value that cannot be understood, a negative timeout or a mode above `0777` fails `config::load` and the service does not start.
A service that will not start says so on the first line of its journal, and a service running at a socket mode nobody chose looks exactly like one running at the right mode.
A service answers `--dump-config` too, so a deploy can validate its config before starting it, and it exits 2 when the config is refused.

### 4.3. `server`

`socket_path_from_config(config_path)` derives the socket path so that services and deploy scripts agree without a second convention, and `M6_SOCKET_OVERRIDE` replaces it.

`apply_socket_mode(path, mode)`, `bind_tcp_reuseaddr(addr)` and `DEFAULT_SOCKET_MODE`, `DEFAULT_READ_TIMEOUT_SECS` and `MAX_REQUESTS_PER_CONN` are the start-time helpers and limits shared by every service and `m6-http`.

`serve_connection(stream, handler)` is the blocking per-connection loop, and `parse_request` and `write_response` are its companions.
They remain for the benchmarks and tests that drive a socket directly.
A service's loop uses `h1::ServerConn` instead.

### 4.4. `signal`

**`signal::block()` must be the first statement of every `main`.**
Blocking a signal is per-thread and threads inherit the mask at creation, so a `main` that starts a thread before blocking has a thread with SIGTERM unblocked.
The kernel delivers process-directed SIGTERM there, and the default disposition kills the process instead of the loop reading the shutdown request.
A service that started that way could lose its shutdown line invisibly, because systemd counts death by the signal it sent as a clean stop.

`Signals::new()` asserts that the mask is set and refuses to start otherwise.
`Lifecycle::on_signal` decides what each signal means and `complete()` ends the lifecycle.

The reload signal is SIGUSR1 (`platform::Signal::ReopenLogs`).
`block()` blocks it with SIGTERM and SIGINT, and a service's loop reads it from the same descriptor.
The loop calls `log::reopen()` and logs what happened. The call flushes the buffered lines first, then opens the path again, so once the path exists every new line reaches the new file and none reaches the renamed one.
If the open fails, the previous file stays in place, the process keeps writing to it and an error is logged.
A reopen never counts toward a forced exit.
SIGHUP is not handled.

### 4.5. `render` and `template`

`render` is the seam.
A route that names a template produces a `Response` carrying `template_name` and `template_dict`, a `Renderer` turns that into bytes, and a `RendererFactory` builds one at start.
`NoTemplates` implements both traits and errors if anything asks it to render, which is how a service that renders nothing links no template engine.

`template` is the Tera implementation: `build_tera`, `build_tera_from_paths`, `TeraRenderer`, `TeraFactory` and `NOT_FOUND_SENTINEL`, which is how a template signals a 404 from inside rendering.

`register_filters` adds the site filters and functions in Table 5 to every Tera instance, beside Tera's own.

| name | kind | what it does |
|---|---|---|
| `slugify` | filter | turns `Hello World` into `hello-world` |
| `date_format(fmt="%B %d, %Y")` | filter | formats a date string, parses RFC 3339 and `%Y-%m-%d`, and returns anything else unchanged |
| `markdown` | filter | renders Markdown to HTML with comrak |
| `truncate_words(n=50)` | filter | keeps the first `n` words, default 50, and appends `…` past that |
| `asset` | filter | returns a content-addressed URL for a file under `assets/`, from a manifest built at start |
| `img_dims(path=x)` | function | returns the ` width="W" height="H"` attribute pair, nothing when the image has no readable size, and needs `\| safe` because it returns markup |
| `not_found()` | function | fails the render with the 404 sentinel, so a missing lookup becomes a 404 |

**Table 5: the site template filters and functions.**
Take from it that `asset` and `img_dims` are computed from tables built at start, and that `not_found()` turns a missing lookup into a 404.

### 4.6. `site`

`Site` is the site's request handling as plain functions on a value the service owns.
A service's loop calls `Site::load` once at start, then per request `handle`, its own handler for the index, and `finish`.
`Site` never calls back into the service: a route the service answers itself comes back as `Handled::Code` or `Handled::Named` with the index of its route.

**Start.** `Site::load` reads the command line (`--version`, `--dump-config`), the config, `site.toml`, the logging settings, the route table, templates and params.
It exits 2 on a configuration error and 1 when logging cannot start, and it refuses a route that names a handler nothing provides.
`name` is the service's name for the lifecycle lines.
`read_timeout`, `socket_mode` and `socket_path` are what the loop binds and applies, and `user_config`, `site_dir`, `config_path` and `site_toml` are the roots a service resolves its own files against.

```rust
pub const GENID_PATH: &str;
pub fn load(name: &str, code: &[CodeRoute], named: &[&str], renderer: &dyn RendererFactory) -> Site;
pub fn reload(&self, code: &[CodeRoute], named: &[&str], renderer: &dyn RendererFactory) -> Result<Site, String>;
pub fn socket_path(&self) -> &Path;
pub fn read_timeout(&self) -> Option<Duration>;
pub fn socket_mode(&self) -> u32;
pub fn user_config(&self) -> &Map<String, Value>;
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
A method other than POST on `/genid` is a 405 and a route on `/genid` is refused at start.
The service's dispatch is a plain `match` on that index.

```rust
let resp = match site.handle(&head) {
    Handled::Done(resp) => resp,
    Handled::Code(FILES, req) => {
        let r = handler::serve(&req).unwrap_or_else(|e| error_response(&e));
        site.finish(&req, r)
    }
    Handled::Named(FILES, req) => {
        let r = handler::serve(&req).unwrap_or_else(|e| error_response(&e));
        site.finish(&req, r)
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
    Handled::Named(..) | Handled::Code(..) => unreachable!("load checked every route and name"),
};
```

`finish` renders a template the response names, then applies the route's `Cache-Control`, `Last-Modified` and headers, the CSRF and flash cookies, minification and compression, in the order the request path requires.

**Routing.** `compile_pattern` turns `/blog/{stem}` into `Vec<Segment>`, and `match_route` returns `PathParams` as a `Vec<(String, String)>` because order is meaningful and duplicates are possible.
`find_route` picks the winner.
`RouteTable` indexes the compiled routes: a fixed route is one hash lookup on its path, and `find_route` tries the pattern routes only when no fixed route answers.
`check_route_conflicts` runs once when a config loads and refuses what would need a tie-break: two fixed routes on one path, or two patterns that can both match some path, where the same request method reaches both.
`Methods` is the set of methods a route answers, so a conflict needs the sets to meet and `find_route` tests one bit.
m6-http builds `RouteDecl`s from `site.toml` and calls the same check.

**The request dictionary.** `FrameworkState::build_dict` is where the real knowledge lives, and `Site` is the one way a service reaches it.
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
- CSRF token, when the feature is on and the dictionary holds none: reuse the `_csrf` cookie or generate one, and insert it.
  A submitted form token stays in place for `verify_csrf` to compare.

Error keys need no step, because they arrive as query parameters at the query step.

**Allocation.** `Site` allocates on the start and stop paths freely.
The page path allocates on every request today: the owned `Request` and its dictionary, Tera and compression.
EL-6a measures that count in `site_allocs` and ALLOC-3 reduces it.

### 4.7. `email` and `client::Endpoint`

A service that sends mail or posts a form does both as work its own loop drives.
The `email` feature compiles the `email` module: `Relay`, `RelayConfig`, `Smtp` and `SmtpError`, plus lettre for building a `Message`.
The `http-client` feature compiles `client::Endpoint` on the `client::Upstream` a loop already drives.
Neither blocks, and neither starts a thread.

**email.** `Relay::new(&RelayConfig { .. })` runs at start.
It refuses plain SMTP to any host but `127.0.0.1`, `::1` or `localhost`, resolves the host once with `net::resolve`, and reads the EHLO name.
A host that does not resolve is an error, so the service stops at start, and a relay whose address changes needs a restart.
`Smtp::new(read_cap, write_cap)` makes one slot per outgoing table entry, with its buffers.
`Smtp::start` keeps the bytes from `formatted()` and begins a non-blocking connect.
The service registers `fd()` with `interest()`, keeps the deadline from `deadline()` in its outgoing `Timeouts`, and calls `drive()` when the token fires.
`Ok(None)` reregisters and returns to the loop, `Ok(Some(()))` means the relay accepted the message, and `Err` names the step, the relay's code and its first line.
The service answers 504 when its deadline passes first, and `close()` drops the connection and frees the slot.
The connection's state is one `enum` of steps inside the slot, so the service's loop has one arm per outgoing token.
Plain SMTP is accepted only to a relay on the same host.

**client::Endpoint.** `Endpoint::new(url, tls)` runs at start: it parses an `http` or `https` URL, resolves the host once, and keeps the `Host` header, path and query.
`post_form(&mut up, fields, deadline)` encodes `application/x-www-form-urlencoded` straight into the `Upstream`'s request buffer and starts it, and the service drives the `Upstream` as it drives an `Smtp`.
One connection per call, with `Connection: close`, so there is no idle list.
`m6_core::rustls` re-exports the TLS library, so a service names `m6_core::rustls::ClientConfig` and takes no `rustls` dependency of its own.

These two modules sit beside the loop, not in it.
Section 5 turns to the objects a handler receives and returns.

---

## 5. Request and response

This section covers the objects a handler touches: the request, the response, the *HTTP* (Hypertext Transfer Protocol) types behind them, the header and cookie helpers, and the time range a query can ask for.
It ends with the rules each helper enforces, and section 6 turns to the parsers those objects read from.

### 5.1. `request`

`Request` is what a handler receives: `method()`, `path()`, `header(name)`, `content_type()`, `body_raw()` and `body_json()`.

`field(name)` reads a form or query field.
`get(key)` and `Index<&str>` read the dictionary, and `dict()` borrows the whole thing.

File I/O is on `Request` on purpose, relative to the site directory: `site_path(rel)` joins a path to it, and `read_json`, `write_json`, `write_json_atomic` and `list_json` read and write there.
`write_bytes` does the same and needs the `multipart` feature.
The readers and writers refuse a `..` component but not an absolute path, which replaces the site directory, and `site_path` checks nothing, so a handler that builds a path from request input validates it first with `path::validate_path_param`.
`write_json_atomic` is the one to reach for when a reader may be running.

`verify_csrf()` (feature `csrf`) and `file(name) -> Upload` (feature `multipart`).

Free functions, all used by `site` and all worth knowing: `parse_query_string`, `parse_form_body`, `url_decode`, `url_encode`, `url_encode_path`, `cookie`, `parse_cookies`, `parse_auth_claims`, `validate_path_param` and `single_query_value`.

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

The module also defines the two request-head limits every protocol and service shares.
`MAX_HEADER_LINE` is 8 KiB for one field, and `MAX_REQUEST_HEAD` is 32 KiB for the request line plus every field.
Both are the numbers nginx, Varnish and Apache use, and the module doc names each setting.
`request_line_len(method, target)` counts a request line, and `field_over_limit(name, value)` answers the field question.

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
`write_head_to` is the same head written straight into a caller's buffer, for a
caller whose writer is already a buffer.

**`ServerConn`** is one HTTP/1.1 connection slot for a service's own loop.
`new(read_cap, write_cap)` makes it at start, `attach` takes an accepted
`m6_core::platform::net::UnixStream` and `detach` gives it back, and `stream()` is what the
reactor registers.
`read` reads until the socket would block and frames what is buffered. A whole
request is then parsed with `parse_head_uninit` into the caller's fixed header
slots and returned as `Read::Request(head)`, or `read` reports `Read::More` or
`Read::Closed`. It keeps the request's method, the keep-alive decision and the
served count on the slot, so `respond` needs no request argument.
`respond(status, headers, body)` queues a short answer and
`respond_response(Response)` queues a handler's. `flush` writes until the socket
would block and reports `Done` (everything written, connection keeping alive),
`More` (the socket would block with output still queued, so write again when it
is writable) or `Close` (everything written and the connection closes).
A `Body::Stream` body is pumped 64 KiB per writable turn, one `read(2)` of the
file per turn.
A request or a response larger than the slot's buffers grows that buffer and it
is cut back to the configured size afterwards.
The parser is edge-triggered, so one turn reads and writes until the socket
would block.

The header slots are `MaybeUninit` and made with `head_slots()`.
The head borrows them for one call, which is why the array is made where it is
used and costs no writes.

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
            reregister(&mut conn, interest, token);
            break;
        }
        // The socket would block with output still queued: register with
        // `conn.interest()` (WRITABLE here) and flush again when it is.
        Flush::More => {
            let interest = conn.interest();
            reregister(&mut conn, interest, token);
            break;
        }
        // Everything is written and this connection closes.
        Flush::Close => break,
    }
}
```

The service's own loop registers the token with `conn.interest()` after every turn, so a turn that ends waiting for the socket to be readable leaves a READABLE registration and a turn that owes output leaves a WRITABLE one.
When a WRITABLE event arrives, the loop calls `conn.flush()` and re-registers with `conn.interest()` again, which is what continues a `Flush::More`.

**Allocation.** `ServerConn` allocates nothing per request: its buffers are made
at start and a request or response larger than them is the one case that grows
one.

### 6.2. `parse`

`parse_request(stream) -> Result<RawRequest, ParseError>`.

**This is a stream adapter, not a second parser.** It reads into a buffer and
asks `h1::parse_request` whether the message is complete, so it has no framing
logic of its own. It takes `Write` as well as `Read` because a client that sent
`Expect: 100-continue` will not send its body until told to, and answering that
has to happen before the read that would otherwise deadlock.

Two bounds, guarding different things: `MAX_REQUEST_HEAD` of head through the
shared parser, then 16 MB of body. It reads in chunks rather than one byte per
`read()` call, because `server` hands it a raw `UnixStream` with nothing
buffering.

The benchmarks and `m6-file`'s `query_of` test drive it. A service's loop frames
requests with `h1::ServerConn`.

`ParseError::status()` and `.reason()` give the response for a parse failure,
including 414 for a request line over `MAX_HEADER_LINE` and 400 for a field or
a head over its limit.

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

`listen_unix(path, mode)` removes a stale socket file, creates the parent
directory, binds non-blocking and sets the mode. `listen_tcp(addr)` binds
non-blocking with `SO_REUSEADDR`, so a restart can rebind a port whose
connections are in `TIME_WAIT` and a live listener on it is still refused.
Both are start only and return `Err` rather than recovering, because a socket
with a mode nobody chose or a port someone else holds is a start failure.

---

## 7. Content

`compress`: `brotli_compress(data, quality)`, `brotli_decompress`,
`gzip_compress(data, level)`, `gzip_decompress`.

`minify` (feature `minify`): `minify_html(data, minify_inline_js)`, `minify_js`, `minify_css`,
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

`m6_core::log` re-exports the `m6-log` crate and adds `watch`.
`init(level, analytics)` sets the level and opens the two Sinks at start: the operational Sink on stdout and, when `analytics` is given, the analytics Sink on that path.
It returns nothing to hold. The Sinks and the level are thread-local, and a process has one loop thread.
`read_site_log_config(site_toml)` reads `[log] level` from `site.toml`, and `parse_level(s)` turns a level name into a `Level`.
`Site` applies the precedence: the command line, then the service's `[log]`, then `site.toml`'s `[log]`.

A service's loop calls the logger in plain sight.
`tick(now)` flushes both Sinks on the one-second accounting tick.
`watch(registry, token)` registers stdout for writability while the operational Sink holds bytes a full destination would not take, and removes it when they drain.
The `LOG` arm calls `flush()`.
A `Signal::ReopenLogs` calls `reopen()`.
`stop()` runs after the loop: it restores stdout's flags, writes everything buffered, then the dropped-count line.
`Line` and the `error!`, `warn!`, `info!`, `debug!` and `trace!` macros build one line into a Sink without allocating.

`pulse() -> &'static LogPulse` is the liveness signal: `events()` counts the lines the level let through, and
`seconds_since_last()` answers "has anything been logged recently", which is how
a silent log becomes a fault rather than a zero.

`reopen()` flushes the analytics file and opens its path afresh.
`reopens() -> &'static ReopenRecord` answers how many reopens succeeded and failed, and how long ago the last of each was.
`writes_log_file()` says whether this process has a file to reopen.
`read_tail(path, max_bytes)` reads the end of one file.

### 8.2. `telemetry`

The write side had no struct: `m6-http` emits analytics through
`m6_core::log`'s `Line` and the JSON comes out of the line builder, so every
consumer re-derived the format by looking at a sample. That is how it came to be read as `ts` and `user_agent` rather than
`timestamp` and `fields.user_agent`, which yields an empty result that looks
exactly like a quiet hour.

`AnalyticsRecord` and `AnalyticsFields` are that missing definition, `Serialize`
as well as `Deserialize` so the write side can adopt them.
`AnalyticsRecord::from_parts(at_us, message, status, path, client_ip, user_agent, truncated)`
builds the record the analytics line with those values parses to, which is how
`/traffic` turns its in-memory window back into records.
`truncated` is the window's own fact that a cap cut the path or user agent, and
it is not part of the line.
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

`HealthReport::build(node)` returns `(u16, HealthReport)` and
`into_response(code)`, always 200 `ok`. The answer covers the m6-http process.
Backend liveness is outside it: a socket pool keeps no per-member health state,
so a dead member is found when a request connects to it and that request takes
the next member.

`PerfReport::build_authorised(subject, snapshot)`, `PerfOutcome`,
`is_monitoring_endpoint(backend)`, `HEALTH_BACKEND`, `PERF_BACKEND`.

`PerfReport::build_authorised` builds the report with no access check, for a
caller that has already passed the one token check.
`PerfOutcome::Unauthorised` and `PerfOutcome::Forbidden` answer with
`m6_core::http::token_refusal`, the one definition of a token check's refusal:
401 `{"error":"unauthorised"}` with `WWW-Authenticate: Bearer` for no or a bad
credential, 403 `{"error":"forbidden"}` for the wrong role or group.
m6-http uses these for `[health] metrics_require` and `metrics_peers`, described
in `docs/m6-http.md`.

`PerfReport.tls` carries the served certificate chain and how long each
certificate has left, from `tls::loaded()`. It is `serde(default)`, so a node
older than the field stays parseable to an aggregator rather than making the
whole payload unreadable, which is the treatment `build` already gets.

`PerfReport.logging` carries `LoggingHealth`, which is also on `/traffic`.
Its `reopen` field is a `LogReopenHealth`: reopens and failures since start, the age of the last reopen, its unix time and the age of the last failure.
`reopen` is `None` for a process that writes no log file, and `seconds_since_last_reopen` is `None` until the first reopen.
Both are `serde(default)`, so a node older than the fields still parses.

`TrafficReport::from_records(node, records, window_minutes)` summarises records
that are already parsed, which is how `m6-http` answers `/traffic` from its
in-memory window. `events_dropped` says how many events that window overwrote
before they left it, so a short window is not read as a quiet hour. `LoggingHealth::read()`
with `QUIET_SECONDS` (40) and `is_blind()` is the silent-log fault.
`NotableClient`, `HeavyHitter`, `CrawlerReport` are its parts.
`CrawlerReport` carries `first_seen` and `last_seen`, both `serde(default)`.

### 8.4. `resolve`

Reverse DNS, for checking a claim against its source. A lookup is a network round trip, so it runs in a monitor's own loop and never in a serving path.

The rule lives in [`crate::dns`]. `Confirm::start(ip, ns)` asks the PTR question for an address on one UDP socket, then asks the name it gets forward, and answers `Ptr` only after that second answer. The caller owns the deadline and reads [`Ptr`], whose four outcomes are Table 8.4.

| `Ptr` | meaning |
|---|---|
| `Name(n)` | the PTR name `n` resolves back to the address |
| `Unconfirmed(n)` | the PTR name `n` exists and its forward lookup completed without the address, a false claim |
| `None` | the address has no PTR record, unverified |
| `Failed` | the lookup did not complete: a socket error, a truncated reply or the deadline, unverified |

*Table 8.4: the outcomes of a confirmed reverse lookup.*

`Confirm` never waits. `fd()` gives the descriptor to register, `drive()` moves the current question as far as it goes without waiting and changes `fd()` when the PTR step ends, and the outcome arrives once. A query dropped at its deadline closes its socket, which reads as `Failed`. A truncated reply is a failure, because an A, AAAA or PTR answer fits in UDP and a TCP fallback would be a second mechanism. `dns::nameserver()` reads the first nameserver in `/etc/resolv.conf` once, at start.

`Ptr::verifies(domain)` matches a name against a domain on a dot boundary, so `example.com.attacker.net` does not match `example.com`. `telemetry::verify_crawler(user_agent, &ptr)` turns a `Ptr` into a `CrawlerVerdict` using the operator's documented method.

This module re-exports `Ptr`, because callers name `resolve::Ptr` and this section states the rule. The one caller is `m6-monitor`'s fleet poll, which drives `Confirm` on its own loop and keeps the answers.

### 8.5. `platform::host`

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

### 8.6. `tls`

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
facts, and a renewal that wrote a new file without restarting the server is
exactly the case where they disagree.
`record_loaded` is therefore called by whatever loads the certificate, and
`loaded()` opens nothing, so polling a monitoring endpoint cannot be made to do
disk work.

| behaviour | why |
|---|---|
| a chain, leaf first | `depth` 0 is what an *ACME* client renews, and intermediates expire too. A chain is good until its soonest expiry |
| `expires_in_seconds` goes negative | how long ago separates a renewal that failed last night from one that stopped running in March |
| an unreadable certificate is returned, not recorded | rustls accepted it or there would be no server, so refusing to serve is the worse failure. A reported absence lets a monitor say "cannot say" |
| a later `record_loaded` replaces the earlier one | so a load at start corrects it, and the registry holds what every protocol serves |

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

### 8.7. `firewall`

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

### 8.8. `wireguard`

`Handshakes::parse_latest_handshakes(text)`, `from_file(path)`, `find(key)`, and `PeerHandshake`.

It reads a file because WireGuard state needs `CAP_NET_ADMIN`.
A systemd timer writes the output of `wg show <if> latest-handshakes`, and m6 reads it as ordinary data.
A time of 0 means the peer never completed a handshake.
A missing file is `Ok(None)`, and a malformed line is an error naming the line.

Do not use `wg show <if> dump`, because it carries the interface private key.

The time is the last successful handshake.
After a key changes under a tunnel it stops moving, and it still looks recent for minutes.
An age is therefore the slow signal, and a probe over the tunnel is the fast one.

### 8.9. `ndjson`

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

- **`h1`'s module doc calls `parse.rs` "(deleted)".** The file exists and is on
  the production path of every socket backend. What was deleted is the parser
  that used to be in it; it is now the stream adapter described in §6.
- **A small number of ad-hoc case-insensitive header lookups remain outside
  core**, in `m6-http`: `cache.rs`, `http2.rs`, `redirect.rs`, `security.rs`,
  `main.rs`.
