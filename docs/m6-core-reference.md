# m6-core, component reference

**Status: reference. Describes the crate as it is, at 30 modules and ~13,400
lines.** Where this disagrees with the code, the code is right and this file is
a bug.

Three documents cover m6-core and they do different jobs:

| file | job |
|---|---|
| `m6-core.md` | **Design.** What the crate is for and where its boundary is. Written before the migration, so its §9 "current state" describes a crate that no longer exists. |
| `m6-core-implementation-plan.md` | **The migration.** Phases, gates, rollback. |
| this file | **Reference.** Every component and its interface. |

`m6-render-lib.md` is superseded. It documents a crate that has been deleted,
and most of what it described now lives here.

---

## 1. Using it

m6-core is the only crate an m6 service links. The intended first line is the
prelude:

```rust
use m6_core::prelude::*;

fn main() -> Result<()> {
    App::new()
        .route("/blog/{stem}", |req| Response::render("templates/post.html", req))
        .run()
}
```

The prelude is deliberately small: `App`, `Error`, `Result`, `Request`,
`Response`, the three time and slug helpers from `util`, and `serde_json`'s
`json!`, `Map` and `Value`. Feature-gated additions appear in it only when the
feature is on (`Upload`, `TeraFactory`, `TeraRenderer`, `lettre`'s mail types,
`ureq`).

A service that renders no templates says so, and stops linking Tera:

```rust
App::new()
    .renderer(NoTemplates)
    .route("/healthz", |_req| Ok(Response::text("ok")))
    .run()
```

There is a second shape. Not every consumer wants the `App` loop: `m6-http` is
a proxy, `m6-auth-server` binds its own socket, and the monitoring tools only
want the parsers. Every module is public and usable on its own, so m6-core is a
library first and a framework second.

## 2. Feature flags

**Features and modules are different axes, and two features have no module at
all.** `flash` and `csrf` gate methods on `App`, `Request` and `Response`; a
reader who greps for a `csrf` module finds nothing and wrongly concludes the
feature is missing.

| feature | default | what it adds |
|---|---|---|
| `templates` | **on** | `template` module, `TeraFactory`, `TeraRenderer`. Pulls tera and comrak. |
| `testkit` | off | The `testkit` module. Enable from `[dev-dependencies]` only. |
| `multipart` | off | `multipart` module, `Request::file`. Pulls multer, tokio, bytes, futures-util. |
| `flash` | off | `Response::flash`, HMAC-signed one-shot messages. Pulls hmac, sha2. |
| `csrf` | off | `Request::verify_csrf`, token generation in the request dictionary. No new dependencies. |
| `email` | off | `lettre` in the prelude. |
| `http-client` | off | `ureq` in the prelude. |

`templates` is on by default on purpose, and the reason is a scar: a feature
that hides code from `cargo test --workspace` is how two `csrf` tests stayed
broken from Phase 4 to Phase 5. chrono and lru are unconditional dependencies
for the same reason. A binary that wants none of it sets
`default-features = false`.

## 3. Component index

Thirty modules. Grouped by what you would be doing when you reach for one.

**Building a service**

| module | what it is |
|---|---|
| `app` | The `App` builder, routing, the thread pool, the request dictionary, the service lifecycle. The largest module at ~2,700 lines. |
| `config` | `RendererConfig` and the TOML that produces it. |
| `server` | `UnixServer` and `serve_connection`, the socket-backend loop. |
| `signal` | `block()`, `Service`, `ShutdownHandle`. Signal-safe shutdown. |
| `render` | The `Renderer` and `RendererFactory` traits, plus `NoTemplates`. The only thing the loop needs from a template engine. |
| `template` | The Tera implementation of those traits, and the site filters. Feature `templates`. |
| `watcher` | `ConfigWatcher`, inotify on Linux and kqueue on the BSDs. |

**Request and response**

| module | what it is |
|---|---|
| `request` | `Request`, the handler's view: the dictionary, body access, file I/O helpers, query and form parsing, URL coding. |
| `response` | `Response`, every constructor a handler needs, and `send`. |
| `http` | `RawRequest`, `RawResponse`, `Method`, the `HeaderSource` trait, `is_same_origin_path`. |
| `headers` | Reading and writing a header list, once. `get`, `get_all`, `combine`, `set`, `append`, `remove`. |
| `cookie` | `Cookie` and `SameSite`. One place that says what a cookie can be. |
| `multipart` | `Upload` and `parse_upload`. Feature `multipart`. |

**HTTP semantics**

| module | what it is |
|---|---|
| `h1` | **The** HTTP/1.1 parser, plus `Responder`, `keep_alive`, `status_reason`, `Expectation`. |
| `parse` | The stream adapter over `h1`. Reads bytes until `h1` says the message is complete. |
| `conditional` | RFC 9110 13.2 preconditions. `evaluate_preconditions`, `is_not_modified`. |
| `negotiate` | Content-coding negotiation, RFC 9110 12.5.3, with real q-values. |
| `mime` | `mime_from_path`, `should_compress_default`. |
| `path` | `validate_path_param`. A security boundary. |

**Content**

| module | what it is |
|---|---|
| `compress` | brotli and gzip, compress and decompress. |
| `minify` | HTML, JS, CSS, JSON. |
| `util` | `slugify`, `now_iso8601`, `today_iso8601`, `iso8601_minutes_ago`. |
| `random` | `random_hex_token::<N>()`. One CSPRNG token generator, so there is never a third. |

**Observability**

| module | what it is |
|---|---|
| `log` | `init`, `init_with_analytics`, `LogHandle`, `LogPulse`. Tracing setup and the liveness pulse. |
| `telemetry` | m6's own formats read back: `AnalyticsRecord`, `PeriodicStats`, `TrafficSummary`, and the probe and crawler heuristics. |
| `monitoring` | `/health` and `/perf`: `HealthReport`, `PerfReport`, `TrafficReport`, `LoggingHealth`. |
| `host` | What the machine is doing. Load, memory, disk, thermal, net, diskstats, pressure, TCP, fds. |
| `firewall` | `FirewallState` from nftables' own JSON. |
| `ndjson` | Newline-delimited JSON, read and written, tolerant of torn lines. |

**Everything else**

| module | what it is |
|---|---|
| `error` | `Error` and `Result`. Eighteen lines. |
| `testkit` | The shared integration harness. Feature `testkit`. |

---

## 4. Building a service

### `app`

The builder is `App::new()`, then `.route(...)` or a method-specific variant
(`route_get`, `route_post`, `route_put`, `route_patch`, `route_delete`), then
`.run()`. `.renderer(...)` supplies the template implementation.

**Four entry points, by what state the service needs.** Each returns a
different builder type with the same routing surface, so a service declares its
state shape once and the type carries it:

| entry point | builder | for |
|---|---|---|
| `App::new()` | `App` | no state |
| `App::with_global(init)` | `AppWithGlobal<G>` | one `G` shared across all workers |
| `App::with_thread_state(init)` | `AppWithThreadState<T>` | a `T` per worker thread |
| `App::with_state(init_global, init_thread)` | `AppWithState<G, T>` | both |

The `init` closures receive the config map (`&Map<String, Value>`) and return
`Result`, so a service that cannot build its state fails at startup rather than
on the first request. `.on_destroy(...)` gives the global a teardown hook.

**Routing.** `compile_pattern` turns `/blog/{stem}` into `Vec<Segment>`;
`route_specificity` scores a compiled route so that a literal beats a
parameter; `match_route` returns `PathParams` (a `Vec<(String, String)>`, not a
map, because order is meaningful and duplicates are possible);
`find_route` picks the winner. Route matching existed three times before this,
twice hand-rolled with different specificity rules, so the same route table
could resolve differently in two services.

**The thread pool.** `ThreadPool::new(size, queue_size)`, or `new_with_exit`
when the pool must be able to end the process. `submit` returns `false` rather
than blocking when the queue is full, which is how the loop sheds load with a
503 instead of growing a backlog. `try_submit` carries a result back,
`in_flight` reports depth, `drain` waits.

**The request dictionary.** `FrameworkState::build_dict` is where the real
knowledge lives, and it is **private**, which is the outstanding item on this
module: a service not using `App` cannot reuse any of it. Twelve ordered steps,
and the order is load-bearing:

1. Config keys.
2. Global params files.
3. Route params files.
4. Path params.
5. Query params, inserted both at top level and as a nested `query` map.
6. POST form fields.
7. Cookies, top level and as a nested `cookies` map.
8. **Built-ins: `request_path`, `datetime`, `year`.** After the params files, so
   a params file cannot override them. This is the load-bearing step.
9. Auth keys.
10. Error keys.
11. Flash message: verify the HMAC, insert `flash`, clear the cookie. Feature
    `flash`.
12. CSRF token: reuse from the cookie or generate, insert `csrf_token`. Feature
    `csrf`.

`is_shutdown()` is a free function for handlers that run long enough to care.

### `config`

`config::load(config_path, site_dir) -> RendererConfig`. The struct:

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
}
```

A `RouteConfig` carries `path`, `template`, `params`, `status`, `cache`,
`methods` and `headers`. `MinificationConfig::is_enabled(mime)` answers the
per-type question. `toml_to_json` is the conversion used throughout.

`[server] read_timeout_s` is the read deadline applied to every accepted
connection, defaulting to `server::DEFAULT_READ_TIMEOUT_SECS` (30). `0` means
no timeout, which is what every `App` service did before the key existed. Read
once at startup, like the pool dimensions: it is set on a socket at accept
time, so a reload cannot retune connections already being served.

`[server] socket_mode` is the mode applied to the unix socket after bind,
defaulting to `server::DEFAULT_SOCKET_MODE` (`0o660`). It is an **octal
string**, as systemd writes it, because TOML has no octal literal and `660`
as a decimal integer is `0o1224`.

Neither key silently falls back. A value that cannot be understood, a negative
timeout or a mode above `0777`, fails `config::load` and the service does not
start. A service that will not start says so on the first line of its journal;
a service running at a socket mode nobody chose looks exactly like one running
at the right mode. Note that `--dump-config` is m6-http only, so an App
service's config is **not** validated at deploy time by
`deploy-platform.sh`.

### `server`

`UnixServer::bind(path)` claims a unix socket and unlinks it on `Drop`.
`accept_one(handler)` takes one connection; `listener()` exposes the raw
`UnixListener` for a service running its own `poll(2)` loop, which is what
`m6-file` does.

`serve_connection(stream, handler)` is the loop itself. It hands the handler a
`&RawRequest` and a `&mut h1::Responder` rather than the stream, so a handler
cannot write an ill-framed response. `MAX_REQUESTS_PER_CONN` is 100.

`socket_path_from_config(config_path)` derives the socket path so that services
and deploy scripts agree without a second convention.

### `signal`

**`signal::block()` must be the first statement of every `main`, before
logging.** This is not a style preference. Blocking a signal is per-thread and
threads inherit the mask at creation; `tracing_appender::non_blocking` spawns a
writer thread, so a `main` that initialises logging first has already created a
thread with SIGTERM unblocked. The kernel delivers process-directed SIGTERM
there, the default disposition kills the process, and the `sigwait` thread never
receives a signal in its life. On syd this meant `m6-file` had not logged a
shutdown line in thirty days, invisibly, because systemd counts death by the
signal it sent as a clean stop.

`ShutdownHandle::install(Service::new("name").socket(path).wake_fd(fd))` sets up
the handler; it asserts the mask is set and refuses to start otherwise.
`complete()`, `is_shutdown()` and `wait()` are the lifecycle; `is_shutdown()` is
also re-exported from `app`.

### `render` and `template`

`render` is the seam, and it is 89 lines. A route that names a template
produces a `Response` carrying `template_name` and `template_dict`; a
`Renderer` turns that into bytes; a `RendererFactory` builds one per worker.
`NoTemplates` implements both and errors if anything asks it to render, which
is how the three site renderers stopped linking Tera, comrak and pest in order
to get a server loop.

`template` is the Tera implementation: `build_tera`, `build_tera_from_paths`,
`TeraRenderer`, `TeraFactory`, and `NOT_FOUND_SENTINEL`, which is how a
template signals a 404 from inside rendering.

### `watcher`

`ConfigWatcher::new(paths)`, `raw_fd()` to fold into an existing `poll(2)` set,
`read_events(filenames)` to drain and report whether anything relevant changed.

Three implementations behind `cfg`: **inotify on Linux**, kqueue on
macOS/FreeBSD/OpenBSD, and a stub elsewhere that never reports a change. The
Linux path reads into a `#[repr(align(8))]` buffer because `inotify_event`
contains a `c_int` and a bare `[u8; N]` has alignment 1; a stack array is
almost always well aligned in practice, which is the kind of bug that works
until it does not.

---

## 5. Request and response

### `request`

`Request` is what a handler receives. `method()`, `path()`, `header(name)`,
`content_type()`, `body_raw()`, `body_json()`.

`field(name)` reads a form or query field. `get(key)` and `Index<&str>` read the
dictionary; `dict()` borrows the whole thing.

**File I/O is on `Request` on purpose**, scoped to the site directory:
`site_path(rel)` resolves, `read_json`, `write_json`, `write_json_atomic`,
`list_json`, `write_bytes`, `touch`. `write_json_atomic` is the one to reach
for when a reader may be running.

`verify_csrf()` (feature `csrf`) and `file(name) -> Upload` (feature
`multipart`).

Free functions, all used by the framework and all worth knowing: `parse_query_string`,
`parse_form_body`, `url_decode`, `url_encode`, `url_encode_path`, `cookie`,
`parse_cookies`, `parse_auth_claims`, `validate_path_param`.

`url_encode` exists because core could percent-decode and not encode, so callers
wrote their own encoder. Encoding and decoding are two halves of one block.

### `response`

Constructors: `render`, `render_with`, `render_status`, `render_dict`,
`redirect`, `redirect_permanent`, `json`, `json_status`, `html`, `text`,
`status`, `not_found`, `forbidden`, `bad_request`.

Builders: `body`, `with_status`, `header`, `cookie`, `flash` (feature `flash`).

`send(w, ...)` writes it. `error_to_response(&Error)` is the default mapping
from an error to a response.

### `http`

`RawRequest` is the parsed wire request: `method()`, `path()`, `query()`,
`header()`, `content_type()`, `accept()`. `RawResponse::new(status)` with
`.header()`, `.body()`, `.content_type()`, then `send` or `to_bytes`.

`Method` is a newtype over `&'static str` with the six constants.

`HeaderSource` is the trait that lets header lookup work over both
`[(String, String)]` and `Vec<(String, String)>`, and `header(headers, name)` is
the case-insensitive first-match lookup.

`is_same_origin_path(candidate)` is the redirect guard: it answers whether a
path is safe to redirect to without leaving the origin.

### `headers`

Use this rather than `http::header` when the question is harder than
first-match. Eight or more sites across the workspace wrote
`.find(|(k, _)| k.eq_ignore_ascii_case(name))` inline, several of them inside
core, and that idiom hides two real questions.

`get`, `get_all`, `contains`, `set`, `append`, `set_if_absent`, `remove`.

`combine(headers, name)` folds repeated field lines into one comma-separated
value, and **refuses when the field must not be folded**. `NEVER_COMBINED` is
the list and `set-cookie` is on it: RFC 6265 3 is explicit, cookie values may
contain commas, so joining two `Set-Cookie` lines produces one malformed cookie
and silently loses the other. `may_combine(name)` asks in advance.

### `cookie`

`Cookie::new(name, value)` then `.max_age()`, `.path()`, `.domain()`,
`.secure()`, `.http_only()`, `.same_site(SameSite::...)`, then
`to_header_value()` or `to_header()`. `Cookie::removal(name)` for expiry.

This does not decide policy. It makes "which of our cookies are HttpOnly?" a
question with an answer, which it was not when four `format!` calls with
different attribute sets were the implementation.

---

## 6. HTTP semantics

### `h1`

`parse_request(buf) -> ParseResult` is the one HTTP/1.1 parser, and it is
**pure**: it does not strip proxy-owned headers, because that is ingress policy
rather than parsing, and a backend has no proxy headers to strip.
`ParseResult` is `Complete(RawRequest)`, `Incomplete` or `Error`.

There were four parsers. Measured against h1spec, the independent RFC 9112
tester, this one scored 27/32 and the others 15, 15 and 14. The plan named the
14 as the consolidation target. Measuring the candidate before moving anything
onto it is what caught that. All four targets now score 32/32 and
`tools/conformance.sh` ratchets it.

`Expectation` and `expectation(buf)` handle `Expect: 100-continue`;
`CONTINUE_RESPONSE` is the reply. `keep_alive(&req)` decides connection reuse.
`status_reason(status)` maps a code to its phrase.

`Responder::new(w, method, keep_alive)` is the write side: `send`,
`send_with_length`, `error`, with `keeps_alive()` and `body_bytes()`. It omits
the body on a HEAD, which the three hand-written writers it replaced did not
reliably do, so every 404, 405, 400 and 412 answering a HEAD went out with one.

### `parse`

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

### `conditional`

`evaluate_preconditions(...) -> Precondition`, `is_not_modified(...)`,
`not_modified_headers(cached)`.

RFC 9110 13.2, implemented once. It was implemented twice, and the second copy
was five inline lines that did strong comparison for `If-None-Match` and
implemented neither `If-Match` nor `If-Unmodified-Since`. The symptom came and
went with cache state, because a hit was answered correctly by the good copy
and only a miss reached the bad one. A defect that correlates with cache state
reads as noise.

### `negotiate`

`coding_quality(accept_encoding, coding) -> Option<f32>`,
`preferred_coding(accept_encoding, candidates)`,
`canonical_coding(accept_encoding) -> &'static str`.

Real q-values. `q=0` means "not acceptable" (RFC 9110 12.4.2), which a
substring match gets wrong in the direction that matters: brotli sent to a
client that explicitly refused it. `canonical_coding` is what makes a cache key
out of a header, so that `gzip` and `gzip, deflate, br, zstd` stop being two
entries for one byte-identical response.

### `mime` and `path`

`mime_from_path(path)`, `should_compress_default(mime)`.

`validate_path_param(value, allow_slash) -> Result<&str, PathParamError>`. This
is a security boundary and it existed three times, with three behaviours. There
is one now.

---

## 7. Content

`compress`: `brotli_compress(data, quality)`, `brotli_decompress`,
`gzip_compress(data, level)`, `gzip_decompress`.

`minify`: `minify_html(data, minify_inline_js)`, `minify_js`, `minify_css`,
`minify_json`.

`util`: `slugify`, `today_iso8601`, `now_iso8601`, `iso8601_minutes_ago`.

`random`: `random_hex_token::<N>()`. There were two byte-identical copies at
two lengths. Both were correct; the hazard is the third one, written by someone
who reaches for `rand::random()` and produces something that looks identical at
a glance.

---

## 8. Observability

### `log`

`init(format, level)` and `init_with_analytics(format, level, analytics_path)`
return a `LogHandle`, whose `reload(format, level)` re-reads on config change.
`read_site_log_config(site_dir)` and `parse_level(s)` are the config side.

`pulse() -> &'static LogPulse` is the liveness signal: `events()` counts, and
`seconds_since_last()` answers "has anything been logged recently", which is how
a silent log becomes a fault rather than a zero.

### `telemetry`

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

### `monitoring`

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

`TrafficReport::build(node, ndjson, since, window_minutes)` is the `/traffic`
endpoint: the node does the analysis and the log stays put. `LoggingHealth::read()`
with `QUIET_SECONDS` (40) and `is_blind()` is the silent-log fault.
`NotableClient`, `HeavyHitter`, `CrawlerReport` are its parts.

### `host`

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

### `firewall`

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

### `ndjson`

`read_str(text)`, `Reader::new(r)` with `skipped()` and `read_count()`,
`write_one`, `write_all`, `to_string`.

**Torn lines are ordinary, not exceptional.** An NDJSON file is usually being
appended to while it is read, so the last line can be half-written and a reader
that fails the whole stream on one bad line fails whenever it reads during a
write. Readers skip what they cannot parse; `skipped()` is how you find out.

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
- `testkit::paths::binary(name)` resolves a built binary.
- `testkit::response::read_one(r, method)` reads exactly one response, HEAD
  included.

---

## 11. Known gaps in this crate

Recorded here because a reference that only describes the good parts is a
brochure.

- **`FrameworkState::build_dict` is private.** Twelve ordered steps of real
  knowledge that a service not using `App` cannot reach. This is the open
  "header to dict" item.
- **Seventeen of thirty modules have no module-level doc comment**: `app` has a
  one-line stub, and `compress`, `config`, `error`, `http`, `log`, `mime`,
  `minify`, `multipart`, `parse`, `path`, `request`, `response`, `server`,
  `signal`, `template`, `util` and `watcher` have none. The thirteen that do are
  the best documentation in the repository, which makes the gap sharper rather
  than softer.
- **`h1`'s module doc calls `parse.rs` "(deleted)".** The file exists and is on
  the production path of every socket backend. What was deleted is the parser
  that used to be in it; it is now the stream adapter described in §6.
- **The inotify path in `watcher` had never been compiled** as of 2026-09-11.
  It is `#[cfg(target_os = "linux")]` and the development machine is a Mac.
- **A small number of ad-hoc case-insensitive header lookups remain outside
  core**, in `m6-http`: `cache.rs`, `http2.rs`, `redirect.rs`, `security.rs`,
  `main.rs`.
