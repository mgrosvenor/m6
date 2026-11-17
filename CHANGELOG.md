# Changelog

This file records what changed in each m6 release, newest first.
Each entry says what changed, why it mattered, and how it was verified.
"Verified" means measured against something running.

Read an entry by its subsections, which repeat the same names across releases: Migration, Added, Changed, Fixed, Removed and Security.
Version numbers follow the policy under Versions in `CONTRIBUTING.md`.
`main` holds releases only, and `develop` is where work happens.

A version heading carries no number.
The heading is the version, and a number would break the pattern `tag.sh` and CI use to match a release's section.

The released versions, newest first:

- [1.15.0, 2026-10-29](#1150-2026-10-29)
- [1.14.0, 2026-10-27](#1140-2026-10-27)
- [1.13.0, 2026-10-02](#1130-2026-10-02)
- [1.12.1, 2026-10-02](#1121-2026-10-02)
- [1.12.0, 2026-09-27](#1120-2026-09-27)
- [1.11.3, 2026-09-26](#1113-2026-09-26)
- [1.11.2, 2026-09-26](#1112-2026-09-26)
- [1.11.1, 2026-09-25](#1111-2026-09-25)
- [1.11.0, 2026-09-25](#1110-2026-09-25)
- [1.10.0, 2026-09-19](#1100-2026-09-19)
- [1.9.0, 2026-09-18](#190-2026-09-18)
- [1.8.1, 2026-09-18](#181-2026-09-18)
- [1.8.0, 2026-09-17](#180-2026-09-17)
- [1.7.0, 2026-09-16](#170-2026-09-16)
- [1.6.0, 2026-09-16](#160-2026-09-16)
- [1.5.0, 2026-09-16](#150-2026-09-16)
- [1.4.0, 2026-09-16](#140-2026-09-16)
- [1.3.0, 2026-09-16](#130-2026-09-16)
- [1.2.0, 2026-09-15](#120-2026-09-15)
- [1.1.0, 2026-09-15](#110-2026-09-15)
- [1.0.0, 2026-09-14](#100-2026-09-14)
- [0.2.0 and earlier, 2026-03-15 and before](#020-and-earlier-2026-03-15-and-before)

---

## Unreleased

**m6-http checks a token in one place, and every route that gates on one answers the same 401 or 403 from one definition.** Route `require`, `/perf`, `/traffic` and `/genid` each call `m6_http_lib::auth::check` exactly once per request; it decides `Allow(claims)`, `NoCredential` (401) or `Deny` (403), and `m6_core::http::token_refusal` is the only definition of the refusal's status, headers and body. The legacy `[health] metrics_token_file` bearer-string gate and its second checker, `m6_core::monitoring::metrics_authorised`, are removed: `/perf` and `/traffic` are gated by `metrics_require`, and without it they answer the 404 of an endpoint that does not exist. A missing `[auth] public_key` now fails closed on a `require` route instead of skipping the check.

### Added

**A public `GET /ready` answers whether a node can serve a request that may need its origin (#175).** It answers 200 while a cache node's last successful origin answer is no more than two heartbeats old, and 503 past that with the follower's failure reason, for example `{"status":"not_ready","node":"edge-a","origin":"down","reason":"connect failed"}`. A source answers 200 `{"status":"ready","node":"origin","origin":"not_applicable"}`: `[genid] upstream` is what makes a node a cache, so the role comes from config rather than from a missing timestamp. A cache that has never had a successful answer is 503 because it holds no ID. The window is `2 * heartbeat_secs`, 20 s at the default 10 s, and there is no new config key. `/ready` is public and answered ahead of routing beside `/health`, and `/health` is unchanged: it still answers 200 `{"status":"ok","node":"<name>"}` for the process alone with no origin state, so a load balancer reads readiness from a status code without pulling a node whose process and warm cache still serve. No migration: the endpoint is new and no existing answer changes. Verified on 2026-11-16 on a macOS sandbox: an end-to-end test stops a real origin, waits until `last_ok_s` is past the window and reads 503 with `connect failed` from `/ready` while `/health` still answers 200, and three further tests cover a live origin, a source node and the unchanged `/health` body in all three states.

### Migration

**`Response.headers` is `Vec<Header>`, a pair of `HeaderText`, where it was `Vec<(String, String)>`.**
A `HeaderText` borrows a `&'static str`, shares an `Arc<str>`, holds up to 23 bytes inline or owns a `String`, derefs to `str` and compares equal to a `&str` and a `String`, so a reader of `resp.headers` changes nothing, and `Response::header(name, value)` still takes two `&str`.
Code that pushes a pair into `resp.headers` writes `(name.into(), value.into())`, or `HeaderText::Static("Name")` to borrow a literal.
`CompiledRoute` gains a `head` field, `Default::default()` in a literal.

**Past a limit's maximum, work is now refused with an error that names the key, where four places used to cut, drop or carry on.**
A log line with a string field over `[limits.log_field] max` (8 KiB) is refused whole, where the field was cut at 1024 bytes and ended in `...`.
A last-N-lines tail request whose N lines do not fit in one `[limits.tail_read] max` page (512 KiB) is answered 413, where `m6-file` returned fewer lines than asked. A client behind a log by more than one page still reads it in pages from `X-Log-End`, as before.
A monitor poll that finds the history hold past `[limits.history_queue] max` (1024 polls) is refused and counted, where the oldest poll was dropped.
A background refresh past `[limits.refresh_queue] max` (4096) is refused with an error line, where it was dropped without one, and a new client address past `[limits.rate_limit_addresses] max` (50,000) is answered 429, where the table grew without a bound.
m6-http's largest HTTP/1.1 request is now 32 MiB (`[limits.edge_request] max`), the next power of two above the 20 MiB it was, and its largest HTTP/2 body is 32 MiB (`[limits.h2_stream_body] max`), above the 20 MiB it was.
A size that is not a power of two is rounded up with one warning on stderr, so 20 MiB cannot be set and `"16MiB"` is the way to the old HTTP/1.1 behaviour.

**The `Renderer` trait writes into a page area instead of returning a `String`.**
`Renderer::render(&self, template, ctx, out: &mut BoundedBuf)` replaces `render(&self, template, ctx) -> Result<String, RenderError>`, and `RenderError` gains `TooLarge(usize)`.
A service that links `m6-core` and uses `TeraFactory` or `NoTemplates` changes nothing.
A service that implements its own `Renderer` writes the page into `out` with `std::io::Write` and returns `RenderError::TooLarge(out.overflow()?)` when a write fails.

**A `cache` on a route now replaces the `Cache-Control` the handler set, where it used to lose to it (#126).** An m6-core service gave a handler's own `Cache-Control` precedence over a route's `cache`, so `cache = "no-store"` on an m6-file route was ignored. A route that names `cache` now wins, with one field left in the reply. A route with no `cache` key is unchanged: `cache` stays a default the handler may override. A deployment that set `cache` on a handler route and relied on the handler's value should delete the key.

**The per-IP limit is counted per process, so a node that shares its public port with `[server] reuse_port` allows one client up to N times `requests_per_min` across N processes, unless Linux is told the group size (see Added: `reuse_port_processes`).** The default is 300 requests per client in 60 seconds, so three processes on the port allow 900. A config that sets `[server] reuse_port = true` and leaves `[rate_limit]` enabled starts without an error. `docs/m6-http.md` and `docs/m6-site-toml.md` §4.13 state the number and the consequence. No configuration change is required. A node that must enforce one number sets `requests_per_min` to the node total divided by the process count, or enforces the limit at the front door.

**`[health] metrics_token_file` is removed, and a config that still names it stops m6-http at start with exit 2.** Switch `/perf` and `/traffic` to `[health] metrics_require = "role:<name>"` or `"group:<name>"` with `[auth] public_key`, and give the monitor a signed API token. A node that used only the legacy bearer file serves neither endpoint until it does. The refusal body on every token-gated route is now `{"error":"unauthorised"}` (401, with `WWW-Authenticate: Bearer`) or `{"error":"forbidden"}` (403); an API client that parsed a route's HTML 401 or 403 page gets JSON. A browser with no credential is still redirected to `/login` or `/auth/refresh`, and a browser with a valid token for the wrong role still gets the site's 403 page; a browser with an invalid or expired token is now redirected to login instead of shown a 401 page.

**An m6-http route can declare the methods it answers, and a request with another method is answered 405 with `Allow` naming the methods it does answer (#174).**
A `[[route]]` or `[[route_group]]` takes `methods = ["GET", "POST"]` in `site.toml`.
A route with no `methods` adds no method restriction of its own, so no existing route changes behaviour.
The refusal is decided at the edge, before the body is read, the cache is looked up or a backend is contacted, and it is 405 with `Allow` (RFC 9110 15.5.6 and 10.2.1).
`GET` also answers `HEAD`, so a GET-only route names both in `Allow` (RFC 9110 9.3.2).
`OPTIONS` is answered at the edge with 405 and `Allow` and never forwarded (RFC 9110 9.3.7), and a route cannot name it.
An unknown or empty `methods` list stops the start with exit 2, and `--dump-config` fails the same way.
Verified by seven end-to-end tests against a real m6-http and backend socket, an RFC 9110 conformance test over HTTP/1.1, HTTP/2 and HTTP/3, a startup test of the exit 2, and unit tests of the fixed method set.

**The JSON digest carries the traffic and security report the text check prints (#220).**
`/digest` gains `nodes[].traffic`, with the facts `/check` sections D and E print from the node's `/traffic`: `window_minutes`, `events_dropped`, `total_requests`, `status`, `logging` (`state`, `events_total`, `seconds_since_last`), `heavy_hitters`, `notable` with the agents and paths it presented, `crawlers`, `forged_bot_requests` and `forgers`, `probe_noise`, and `firewall` (`blocks`, `total_rules`, `hit`, `hit_total`).
`nodes[].traffic_error` carries the reason when a node has no report.
The section carries client addresses, user agents and firewall block addresses, and no token, key or fingerprint.
Every list that a node's traffic can grow without bound is cut to a named size, with its true count beside it, so one request cannot make the digest unbounded: a three-node digest with one of everything is 6,214 bytes against 2,821 before, and 56,689 bytes with every list filled to its cap.
`traffic` is additive and no existing field changes, so no migration is needed.
Verified: 212 m6-monitor tests pass, the check-versus-JSON agreement test fails when the section is removed, and the size test measures both figures.

**Each crawler sighting on `/traffic` and in the monitor report now carries the time it was first and last seen, and the monitor checks each address behind a sighting against the method the crawler's operator documents. A reverse DNS name counts only when resolving it forward returns the address (#205).**

The new JSON fields are additive.
`CrawlerReport` gains `first_seen` and `last_seen` (RFC 3339 UTC), both `serde(default)`, so a node older than these fields still parses and an aggregator newer than a node reads an empty string as "not reported".
No existing field changes, so no migration is needed.

### Migration

**`/perf` reports traffic in three populations, and the traffic counters no longer include the monitor's polls outside the three monitoring endpoints (#107).** `http/<version>/external` and `http/<version>/internal` keep their meaning, and `http/<version>/monitor` is new. A request is monitor when it is answered by `/health`, `/perf` or `/traffic`, or when it arrives on the TLS or QUIC listener from a private network address. It is internal when it arrives on the listener `[server] internal_binds` names, and external otherwise. `monitor_requests_total` now counts every monitor request, including a poll on another path, and the three endpoint polls now appear in the channel table instead of only in that total. A request from a private address, such as an operator on the same private network, is now counted as monitor and is not in `requests_total`. Loopback stays external. No configuration change is required.

**A site whose `[server] allowed_methods` names `OPTIONS` no longer forwards that method.** m6-http answers `OPTIONS` at the edge with 405 and `Allow`, as the default configuration already did, so a client that used `OPTIONS` to discover a backend's methods now reads the edge's list instead of the backend's answer. Remove `OPTIONS` from the list: keeping it changes no answer, and no route can be configured to forward it.

**m6-file now refuses at start, with exit 2, a served root that carries `.git`, `target` or `node_modules`, and warns on a served root over `site_size_budget_mb`.** The check follows each route's `root`, so a renderer's `target/` beside the app root is not examined when no route reaches it. A deployment whose routes reach a checkout or a build tree must move that tree outside every route's `root` before upgrading. A `logs/` directory is named in the warning and served, because a route rooted at it is a legitimate log viewer. The budget defaults to 128 MiB and counts the regular-file bytes the route roots reach. A site that serves more raises the key. Run `m6-file <site-dir> <config>` once on a development copy to see the warning before the restart.

**`cache_misses_total`, `requests_total`, the response-code table and `backend_errors_total` now include responses m6-http fetched from a backend (#213).** Before this change the backend completion path recorded nothing. `cache_misses_total` moved only for a miss m6-http answered itself, so a node's reported hit rate stayed near 1.0000 however much it fetched, `requests_total` was short by every fetched response, and a backend's own 5xx never reached `backend_errors_total`. The same fields now read a real hit rate. Nodes older than this change report the four new cache counts as 0, because they do not have them. No configuration change is required.

**Pages and unversioned files now make browsers revalidate on every request, and m6 no longer emits `stale-while-revalidate` in those headers (#99).** m6-core page routes default to `Cache-Control: public, max-age=0, must-revalidate, s-maxage=86400`, and m6-file sends the same header on an unversioned file, both with an `ETag`. A browser now sends `If-None-Match` on every page and file load: an unchanged response is a 304, and the edge answers it from its own copy for 86400 seconds. An upstream that sends `stale-while-revalidate` is still honoured inside its window (RFC 9111 section 4.2.4). A deployment that relied on `public` alone, or on the `max-age=60, stale-while-revalidate=60` file header, should expect one conditional request per view. Versioned `?v=<hash>` assets keep `public, max-age=31536000, immutable`. No configuration change is required.

**Remove `format` from `[log]`.** Every service now writes JSON lines through m6-log, so there is no format to choose, and a config that still sets `[log] format` is refused at start with "unknown key [log] format". m6-core's `LogHandle`, `init`, `init_with_analytics`, `reopen_log_file` and `Reopened` are gone; a service sets up logging through m6-log.

**Every setting that names a secret, key or certificate takes an absolute path, and a relative one stops the service at startup with exit 2, naming the setting: `secrets_file` (every service built on m6-core), `[server] tls_cert`, `tls_key`, `[auth] public_key` and `[health] metrics_token_file` (m6-http), `[storage] path`, `[keys] private_key` and `[keys] public_key` (m6-auth-server and m6-auth-cli), and `perf_token_file` (m6-monitor).** Before upgrading, replace each relative value with the absolute path it resolved to. m6-http resolved them against the site directory. m6-auth-server and m6-auth-cli resolved `[keys]` against the site directory and `[storage] path` against the config file's directory, so `configs/m6-auth.conf` with `path = "data/auth.db"` used `<site>/configs/data/auth.db`. m6-core and m6-monitor resolved them against the working directory. `m6-auth-cli token create` no longer takes `--site-dir`. The documented production layout delivers m6-http's certificate, key and auth public key through systemd `LoadCredential` as well, and a certificate renewal restarts m6-http: a deployment that follows it changes its certbot deploy hook from a touch of the system config to `systemctl restart` of the m6-http unit.

**A log or database directory that does not exist now stops the service at start with exit 2, naming the setting and the path, in place of being created.** Create the directory each setting names, owned by the user the service runs as, or point the setting at a directory that exists: the parent of `[analytics] log_path` for m6-http and the parent of `[storage] path` for m6-auth-server. A deployment's converge that creates `<site>/logs` and the config directory, owned by `m6`, already covers m6-http's default analytics path. It must also create m6-auth-server's database directory where auth is enabled. That unit is enabled on no node today, and the shipped `configs/m6-auth.conf` still names the relative `data/auth.db` this release refuses, so an operator who enables it must create the directory and make the path absolute.

**m6-http refuses at start, with exit 2, a config it used to warn about.** That covers a key or section it does not know, a key in the file it does not belong to (docs/m6-site-toml.md §3.3, Table 1), a system config with no `[node]`, and a removed or misfiled `[health]` key.
Before upgrading, run `m6-http <site> <system config> --dump-config` on every node and fix what it names: add `[node] name = "<node>"` to each system config, and move every node key out of `site.toml`.
Every m6 service refuses an unknown log level (in `site.toml`, its own config or `--log-level`), a `site.toml` that is missing or is not valid TOML (the default path included), an unknown key in one of m6-core's own tables (`[thread_pool]`, `[params_cache]`, `[server]`, `[log]`, `[compression]`), and a socket whose mode it cannot set.

**A backend host name that does not resolve when m6-http starts now stops it with exit 2.** Host names in `url` and HTTP/2 backends are resolved once at start, so a backend whose address changes needs m6-http restarted.

**A database path m6-auth-server cannot open now stops it at start with exit 2**, instead of answering 500 to each request.

**m6-http verifies an https:// or h2s:// backend against the host's certificate authorities only, and a host with none stops it at start with exit 2.** A backend that relied on the bundled Mozilla list on a host with no CA certificates needs the host's CA package installed.

**A service config that still has `[thread_pool]` is refused at start.** Delete it from every service config. To use more cores, run more instances (a systemd template unit with `M6_SOCKET_OVERRIDE` per instance and a glob in `site.toml`'s `[[backend]]` sockets). Code built on `App`: replace `App::new()` and its `route_*` calls with a main that loads `site::Site`, opens its listener with `net::listen_unix` and runs the loop in docs/m6-app-anatomy.md, with your routes as `CodeRoute` values and one match on `Handled`.

**m6-monitor reads the fleet list and every node's host name at start.** A change to the fleet, or to a node's address, needs a restart. A node whose host name does not resolve at start stops m6-monitor with exit 2, naming the node.

**m6-monitor's history file gains the index `health_ok_node_at` at start**, with no action needed. `[monitor] health_last_ok_max_age_s` is optional; its default is three poll intervals.

**One request-header limit, as nginx sets it.** A header field over 8 KiB or a head over 32 KiB now gets 400 ("Request Header Or Cookie Too Large") from m6-http, and a request line over 8 KiB gets 414, on HTTP/1.1, HTTP/2 and HTTP/3, before any backend is contacted. The limits are `m6_core::headers::MAX_HEADER_LINE` and `MAX_REQUEST_HEAD`, and m6-core backends answer the same codes. HTTP/1.1 heads up to 20 MiB and HTTP/2 heads up to 64 KiB used to be forwarded and then refused by an m6 backend, which the client saw as 502. Each connection slot's head buffer is 32 KiB. On HTTP/3, quiche refuses a field section over 32 KiB by closing the connection instead of answering 400.

`m6_core::mio` is gone: use `m6_core::platform::{Poll, Events, Interest, Token}`, `platform::net`'s sockets and `Registry::register_fd` in place of `SourceFd`. `m6_core::event::{Signals, Signal}` are `m6_core::platform::{Signals, Signal}` and `Signal::Terminate` is `Signal::Shutdown`. `event::raise_open_file_limit` is `platform::process::raise_fd_limit`. `m6_core::host` is `m6_core::platform::host`. `m6_core::signal::ShutdownHandle` and `Service` are removed: use `platform::Signals` in the service's own loop. A service that spawns a child uses `platform::process::spawn`, and an accept loop matches `platform::net::accept_error`. Shipping code reads the clock through `platform::now()` and `platform::wall()`.

**A cache node needs `[genid] upstream = "<backend>"` and `token_file`, and the source must run this version first.** The token file is an absolute path to a token with the source's read claim, delivered like every other credential. A cache node stores nothing from an upstream that sends no `M6-Genid`, so a source older than this release cannot fill it.

**`[monitor] one_certificate` is new and optional, default false.** A deployment whose fleet is meant to carry one certificate sets it to true to turn a differing node into a fault in `/check` and `/digest`. Leave it unset where each node renews its own certificate. `m6-monitor` ignores a `[monitor]` key it does not know, so an older monitor starts against a config that has this key and simply does not check it. No other setting changes.

### Added

**`m6-auth-server` gains a fifth route, `POST /check`, which answers the current identity for an access token.**
It takes `Authorization: Bearer <access token>` and answers 200 with `sub`, `username`, `roles` and `groups`, all read from the database at that request.
The token proves which user is asking, and the database supplies what that user may do, so a role change is visible to the next request without a new token.
The answer is the decision for this request only, and it carries no cache hint, because nothing caches a decision and a revocation counts from the next request.
A missing, invalid or expired token is 401 `{"error":"unauthorised"}`.
A user who is not in the database, including a deleted user, is the same 401, and the route never answers 200 with an empty role list.
A locked database is 503 `{"error":"unavailable"}` at once, because the caller's deadline is shorter than this service's one-second deferral.
The route is additive: no m6-http route calls it yet, so no deployment's request path changes and local token verification still decides a request.
Verified on 2026-11-17 on a macOS sandbox: four tests in `m6-auth-server/tests/check.rs` spawn the real service over its Unix socket and prove the database's role change, the 401s for an unknown subject, a deleted user and a missing credential, and a handler unit test proves a busy database is 503.

**`[limits.heap_reserve]`, and for m6-http `[server] lock_memory`, which pins the heap in RAM.**
`heap_reserve` is the heap reserved from the OS at start: 32 MiB by default and at most 4 GiB, rounded to a power of two with one warning. m6-http defaults it to `cache_bytes` max plus 64 MiB rounded to 2 MiB, so the cache is carved from the huge-page region, and warns at start, naming `[limits.heap_reserve] start` and `[limits.cache_bytes] max`, when a configured value leaves the cache outside it.
`lock_memory` defaults to true, so it needs `RLIMIT_MEMLOCK` to cover the process: production sets `LimitMEMLOCK=infinity` in the unit, a Linux developer raises the limit for the account (`docs/m6-testing.md` §2), and CI raises it with `sudo prlimit` in each Linux step that starts m6-http. The reservation is also advised `MADV_HUGEPAGE` (`m6_platform::process::advise_huge_pages`), because transparent huge pages in `madvise` mode back only an advised range and mimalloc asks for explicit huge pages alone. After the reservation and before the first request m6-http calls `m6_platform::process::lock_memory` (`mlockall(MCL_CURRENT | MCL_FUTURE)` on Linux, a documented no-op on macOS). If the kernel refuses because `RLIMIT_MEMLOCK` is too low, m6-http stops with exit 2 and an error that names the limit and the fix (`LimitMEMLOCK=infinity` under systemd, or `lock_memory = false`); it never carries on unpinned when pinning was asked for. `m6_platform::process::huge_page_bytes` reports how much of the process sits on huge pages, and m6-http logs it once at start.
`tools/memcalls.sh` attaches `strace -c -f` to a warmed-up m6-http (cache hits) and m6-html (renders) and counts `brk`, `mmap`, `munmap`, `madvise`, `mremap` and `mprotect` over 10,000 requests, base against branch. `tools/hwcounters.sh` runs `perf stat` over 10,000 hot cache hits with the cache full and prints page faults, dTLB and iTLB misses, cache misses, instructions and cycles per request, or says "hardware counters not available on this host" and prints only the software events.
Verified on macOS: unit tests for the limit and its m6-http default, `lock_memory`, the `lock_memory` key, and a platform test that lowers `RLIMIT_MEMLOCK` to zero in a child (a Linux refusal naming the limit and the fix; `Ok` on macOS), with an m6-http start-up test that does the same through `ulimit -l 0` on Linux; both scripts pass shellcheck and ran their server and request paths against dev builds with a stand-in for `strace`. The Linux pins, the counters and the strace table have not run yet.

**Every bounded buffer has a named limit with a `start` and a `max` read from one config table, and the rendered page is the first buffer on it.**
`[limits.render_page]` takes `start` and `max`, as bytes or as `"512KiB"` and `"4MiB"`, and an absent table gives the defaults: 512 KiB at start, 4 MiB at most (`docs/m6-app-anatomy.md` §4.2).
The new `m6_core::limits` module owns one rule: allocate `start` at start, grow only when a write does not fit, by doubling and clamped at `max`, keep the size and never shrink, and log one line naming the limit and the new size on each growth.
A page past `max` is refused with a 500 and one log line that names the limit, the size needed against the maximum, the page and the exact key and file to raise, for example `page /blog/x needs at least 5 MiB, over [limits.render_page] max = 4 MiB in /etc/m6/m6-html.toml; raise max to fix this`.
Sizes are `page * 2^k` with the page read from `m6-platform` (`process::page_size`, 4 KiB on Linux x86_64 and 16 KiB on macOS arm64).
A value of another size is rounded up with one stderr warning that names the key, the file, the value set and the value used, and a `max` below its `start`, a zero, an unknown limit or an unknown key stops the start with exit 2.
Before, `Tera` returned a fresh `String` for every page, which re-grew several times as the page was written.
Now every page is written into one area the service makes at start, then copied out once at its exact size, so the area allocates nothing once it has reached the largest page seen.
A reload builds a new service state, and the area starts again at `start`.
Verified by `m6-core/tests/limit_page_allocs.rs` under the counting allocator: 100 pages under the start allocate nothing for the area, one 600 KiB page grows it once to 1 MiB, and the next pages of any size up to 1 MiB allocate nothing for it.
With `BoundedBuf::clear` made to drop its capacity, the three tests fail with 100, 2 and 5 allocations against 0, 1 and 0.
Unit tests cover the rounding and its one warning, the refusals, the doubling and clamping, and the over-max message naming its key and file, and two site tests cover a page that grows the area and a page past the maximum.
The per-request allocation budgets in `site_allocs` and `request_allocs` are unchanged, because the small test pages never re-grew a `String`.

**m6-monitor reads every m6-http process of a node, not whichever one the kernel picks on the shared port.** Each process takes `[server] process_index` and `status_bind = "<address>:<base port>"` and also answers `/health`, `/perf` and `/traffic` on its own TLS listener at the base port plus its index; every other path there is 404, under the same `[health]` gate. The listener joins the TLS listener's connection table in the existing loop, so nothing on the shared port's request path changes. In m6-monitor, `processes = N` on a node (with `tunnel.address` as the base) polls process `i` at that address with its port raised by `i` and reports it as `<node>/<i>`. Its own line in `/check` and its own node in `/digest` show each process, a new `PROCESSES` section and a `process_groups` digest key add a node's processes together, and a process that is down or answers as another index is a fault naming `<node>/<i>`. Header checks run once, from process 0, against the public address. `processes = 1` or absent polls the node as one address, so existing configs and their output are unchanged: no `PROCESSES` section and no `process_groups` key appear. Why it mattered: with `reuse_port` the monitor saw one process of N and could not see a dead one. Verified by a three-process test in `scale_reuse_port.rs` (each status port answers its own index, `/` is 404, stopping one refuses only its port), monitor tests for the expansion, the status-port polling, the index mismatch, a refused process, the sums and the unchanged output without `processes`, and the zero-allocation `allocations` test, unchanged, for the request path.

**On Linux, `[server] reuse_port_processes = N` makes each client address always reach the same m6-http process, so the per-client rate limit is exact.** `m6_platform::net::listen_shared(addr, group_size)` attaches a classic BPF program (`SO_ATTACH_REUSEPORT_CBPF`) to the TLS listener and the QUIC socket. It returns (a hash of the client's source address) mod N and reads the address with the `SKF_NET_OFF` offsets, for TCP (HTTP/1.1, HTTP/2) and UDP (HTTP/3), IPv4 and IPv6, so every connection and datagram from one address reaches one process whatever its source port. The limit is then 300 requests per minute per client across the port. Why it mattered: the count is per process, so N processes allowed N times the limit. Unset, nothing is attached and behaviour is as before. macOS keeps today's behaviour and ignores the key. The program is on the stack and the accept path allocates nothing new. When a process stops or starts, the group is short of N or its members move, so some clients change process until all N are up; open connections stay, a moved QUIC client handshakes again, and its rate count restarts. Every process must set the same N. `docs/m6-http.md`, `docs/m6-site-toml.md` and `docs/m6-platform.md` say so. Verified on macOS only so far: the workspace builds and the config tests pass here. Four Linux tests in `m6-platform/src/linux.rs` (TCP, UDP, IPv6 and the unsteered contrast) are written but not yet run, because they need Linux; they are to be proved on the Linux host.

**A route's `cache` overrides the `Cache-Control` the backend set, on an app route and on a site.toml `[[route]]` or `[[route_group]]` (#126).**
m6-file builds its own `Cache-Control` from the query string, so no route could say `no-store` for one path. That mattered for anything short-lived served from disk: an ACME HTTP-01 token answered with `s-maxage=86400` let an edge hold a spent token, or a 404 from before the token existed, for a day.
This is the nginx rule per location: the route's value replaces the handler's, and the reply carries exactly one `Cache-Control`.
In an app's config (m6-core), an explicit `cache` replaces what the handler set, and a route without the key keeps the handler's value.
In `site.toml`, m6-http applies `cache` to every reply the route gets before storing it, so `no-store` stops the store and the client and the edge copy agree. The cache-hit path is untouched. A value that is empty or holds a control character stops the start with exit 2, naming the route.
m6-http used to refuse the key at start, so no accepted config changes meaning.
Verified: RFC 9111 section 5.2 conformance tests over HTTP/1.1, HTTP/2 and HTTP/3 against a real m6-http and a backend that sends `public, max-age=60`, each failing with the override removed; a m6-core test that fails with the override removed; unit tests of the config check.

**CI fails when a document names a commit that no longer exists.** `tools/check-commit-hashes.sh` reads every tracked Markdown file and runs `git cat-file` on each lowercase hex token of 7 to 40 characters that names a commit.
A max-age, a byte size, an allocation count, a colour, a UUID, an ETag and a versioned-asset hash are not commits, so they are not flagged, which is what keeps the check from being switched off.
It needs no network and no build, and it runs in under a second on this tree.
Before this entry the tree had 29 distinct commit references: 2 live, 22 dead in `docs/PERFORMANCE.md` after a rewrite killed the hashes 83e4142 had repointed to, 1 dead in `README.md` that 83e4142 missed, and 4 in the quiche fork that `CHANGELOG.md` names as another repository.
The 23 dead ones are recorded as dead history in one list, each allowed only in the document that already carries it, and the check fails if an entry starts resolving again.
Verified on 2026-11-15 on a macOS sandbox: the check passes at `024ec86`, and a deliberately dead hash added to `docs/m6-http.md` in a scratch copy fails it with the file, the line and the hash, then passes again when reverted.

**m6-file reports what each route's `root` carries, once at start and never per request.** The walk names `.git`, `target` and `node_modules` and exits 2, warns about `logs/`, editor backups and `.DS_Store`, and compares the regular-file bytes the roots reach with `site_size_budget_mb`. The budget defaults to 128 MiB and the warning names the total, the file count and the setting. A root is cut at a `{param}` placeholder, an absolute root and a root that climbs out are skipped, and overlapping roots are counted once. Verified on 2026-11-15 on a macOS sandbox: five tests spawn the real binary over a `.git` root (exit 2), a `logs/` root (warned and serving), an over-budget root (warned with the total and setting), a clean root, and a process root holding `render-cms/target` outside the route roots (not refused).

**`/perf` reports the four cache outcomes a request can have, in place of one hit/miss pair that could not tell them apart (#213).** The new counts are `cache_hits_valid_total`, for a hit on an entry still inside its freshness lifetime, `cache_hits_invalid_total`, for a hit on one past it and served under `stale-while-revalidate` or `max-stale`, `cache_misses_fetched_total`, for a miss answered by fetching a backend, and `cache_misses_answered_locally_total`, for a miss m6-http answered itself with no backend contacted, such as a path matching no route. `cache_hits_total` is the sum of the two hit fields and `cache_misses_total` the sum of the two miss fields, so both keep their place in the report. A hit is recorded where the lookup returns its entry, so validity comes from the variant the cache chose rather than from the entry's age recalculated in the reporter. A fetched miss is recorded where the backend's answer arrives, the one point every protocol passes and the only one where the final status and the miss latency are both known. A background `stale-while-revalidate` refresh is not a request and moves no counter. Verified on 2026-11-15 on a macOS sandbox: `m6-http/tests/cache_accounting.rs` drives a cold fetch, a fresh hit, a stale hit and a local 404 against a real m6-http, asserts the exact delta of all six figures after each step, and asserts no movement once the stale serve's background refresh has been answered by the origin. Removing any one of the four increments fails the suite, and counting the background refresh fails the stale-hit assertion.

**A cache node with `[cache] dir` reads its disk cache at start and holds it until its first `/genid` heartbeat.** The same ID installs it, so the node starts warm, and another ID discards it. If the upstream does not answer, the node serves the disk copy until a heartbeat succeeds, then flushes it if the site changed meanwhile. A damaged file is deleted and counted. `/perf` adds `loaded_entries`, `loaded_bad` and `state`. Verified on 2026-11-15 on a macOS sandbox: a full 4 MiB cache of 1,024 files loads 987 entries in 122 to 145 ms in a debug build, inside the 452 ms startup window the writer's measurement reported, and three integration tests start warm, discard a load after a publish, and serve a disk copy with the source stopped.

**The conformance gate now runs m6's own RFC tests, including the HTTP behaviour changes in this batch that no external tester can see.** `tools/conformance.sh` gains an `rfc` stage that runs `m6-http`'s `tests/rfc_conformance.rs`, `tests/backend_h2_split_headers.rs` and `tests/early_response.rs`, and the `m6-file` cache-control and `m6-core` conditional unit tests. Every test names its RFC section: `Content-Length` as `1*DIGIT` on h1 and h2 (RFC 9110 8.6, RFC 9113 8.1.1), a split body-less h2 reply (RFC 9113 6.2, 6.10, 8.1), lower-case h2 and h3 field names (RFC 9113 8.2.1, RFC 9114 4.2), `If-None-Match` answered 304 on h1, h2 and h3 (RFC 9110 13.1.2, 15.4.5), and a backend's early answer forwarded (RFC 9110, RFC 9112 9.6). The stage fails when a test fails, so CI's `h1, h2, h3 conformance` job fails with it. Verified on 2026-11-15 on a macOS sandbox: 9 of 9 `rfc_conformance` tests pass on this tip, and the content-length, lower-case and split-header tests fail when their fix is reverted.

**A cache node with `[cache] dir` writes the responses it caches to disk in bounded slices on idle loop turns, never on the request path.** Each file carries the site generation ID and a SHA-256 check value. The disk copy is bounded by the memory cache's size and moves to a new directory when the ID changes. A write failure is logged once with the path and the error, and the entry is dropped without failing a request. `/perf` reports the directory's bytes, the queued keys and the write counters.

Migration: none for a node without `[cache] dir`. The key is refused on a source node.

**A cache miss on a node with a disk cache allocates nothing for the writer.** The store sets the entry's `queued` flag in the same map write that stores it, so no key is copied and nothing is pushed. The tick scans for flagged entries, and an entry the budget cannot reach keeps its flag for the next tick. `/perf` adds `written_bytes_last_tick`, and a load test queues 500 entries of 10,000 bytes, drives the slice directly, and reports the writer's slice time and the loop's `work_max_ns` while the queue drains.

**A disk-cache slice now costs the loop about a millisecond, on an idle turn.** While the writer has a flagged entry or an old directory to remove, the loop polls with a zero timeout and runs one slice per idle turn, and returns to its normal wait when the queue is empty; a request on a descriptor waits for at most one slice. The slice spends at most 1 ms of loop time, checked between files on the platform clock, where one full queue used to hold the loop for the half second it took to write 4 MiB. The 4 MiB write and 512-file removal bounds remain upper bounds and whichever is reached first ends the slice. A file is never split, and the remaining flagged entries wait for the next slice. `/perf` adds `ticks` and `tick_last_ns`. Verified on 2026-11-15 on a macOS sandbox: 500 responses of 10,000 bytes behind 1,000 old-generation files drain in 0.42 s over 308 to 319 idle turns (251 write slices of one or two files, the rest removals), about 1,200 files per second against the production tick's two files per second. The writer's slice time was median 1.43 ms, p99 1.84 ms and max 2.00 ms, and while the writer drained under 500 live requests the loop's `work_max_ns` was 3.61 ms (the upstream trigger); the old accounting-tick path held the same queue for 170.2 ms on this sandbox and 437.7 to 524.6 ms on the recording machine. The load harness drives the slice directly, so the gate does not wait on real seconds, and the live write test asserts `/perf` reports `work_max_ns`.

**`[server] reuse_port` lets two or three m6-http processes serve the public 443 port.**
It defaults to false, so an accidental second instance still fails at bind and an existing config binds exactly as before.
When set, only the TLS listener and its QUIC UDP socket on `[server].bind` are bound through `m6_platform::net::listen_shared` with `SO_REUSEPORT`, in one call before the TLS configuration is built.
No other socket is shared: the redirect listener on port 80, the internal h2c listener, the health endpoints and every backend socket are unchanged.
Linux spreads a new TCP connection across the sockets that share a port by a hash of its 4-tuple and macOS does not, so the split is not even and no process is guaranteed traffic.
A UDP datagram goes to one socket, so a QUIC client that changes address may land on another process.
Verified on the sandbox with three m6-http processes on one port and one unix-socket backend set: every request was answered 200 before and after one process was stopped.

**A count of heap allocations per request, as a test.** Building m6-http with the `count-allocations` feature counts every allocation the loop makes while serving, and logs the calls and bytes once at stop. `tests/allocations.rs` holds the count per cached request over HTTP/1.1 and HTTP/2 with TLS, per cache miss and per new connection, to a budget that only goes down. A normal build is unchanged. Verified on 2026-11-15 on a macOS sandbox with the difference of two runs on one warm connection, N = 1,000 and 2N = 2,000: 11.000 and 91.000 calls per request, 91.001 for a miss that inserts and 123.000 calls per connection. The three request budgets are the top of the observed spread, 11,003, 91,004 and 91,004 calls per 1,000 requests, with nothing added on top, and the test passed three runs in a row. The new-connection row is measured per connection and reported rather than budgeted, because a full handshake per connection varies with the server's read sizes. Adding one `String::from("x")` to the request path failed the three budgeted rows by one call per request and raised the new-connection figure by the same.

**The source node holds the site's generation ID, and `/genid` there reloads every backend.** m6-http on the source node owns the ID; a `POST /genid` to it drives the reload of every member of every pool, and a member that fails is named in the log with its socket and the status it answered. Only a response carrying the current ID is stored in the cache, so a response rendered before a reload cannot be cached after it. A reload on a backend now writes an access line like any other request.

**`POST /genid` on a backend reloads its site from disk, the same way it starts.** Config, `site.toml`, templates and params are read again through the start path, and the new site replaces the old one only when the whole load succeeds: the answer is 204, or 500 with the cause while the old site keeps serving. A request already answered or parked when the reload lands finishes with the generation it started under. A service route on `/genid` is refused at start.

**User guide section 7.2, Secrets and state: configuration in /etc/m6, secrets by systemd's `LoadCredential` (m6-http's certificate and keys included), databases by `StateDirectory`. The m6-auth unit uses both, and the m6-http unit uses `LoadCredential`.**

**Crawler sightings carry a time and a verdict (#205).**
The monitor report's crawler section prints `seen: <first> .. <last>` for each sighting, and one line per address (at most three) with one of these outcomes:

| outcome | meaning |
|---|---|
| VERIFIED | the address has a PTR name under a domain the operator documents, and that name resolves back to the address |
| MISMATCH | the PTR name is under no domain the operator documents, or it does not resolve back to the address |
| UNVERIFIED | no PTR record, the lookup failed or timed out, or the operator publishes address ranges instead of PTR records |
| no published method | the user agent is in no table, so there is nothing to judge it against |

Table 1: the outcomes of a crawler address check.
Only MISMATCH calls a claim false. A timeout or a missing record reads UNVERIFIED and never MISMATCH.

**`m6_core::resolve` confirms a PTR name forward and bounds its threads.**
`reverse_confirmed(addr, timeout)` resolves the PTR name with `getnameinfo`, then resolves that name with `getaddrinfo`, and returns `Ptr::Name` only when the address is among the results.
A completed forward lookup without the address returns the new `Ptr::Unconfirmed(name)`, and a forward lookup that fails or times out returns `Ptr::Failed`.
One deadline covers both lookups.
At most `MAX_LOOKUP_THREADS` (8) lookup threads are alive at once, counting threads abandoned at their deadline.
A lookup that finds no free slot returns `Ptr::Failed` at once without starting a thread.
A PTR name with any character other than letters, digits, `-`, `_` and `.` returns `Ptr::Failed` and is never looked up forward or printed.
Lookups run in the monitor only, never on the serving path, and add no dependency.

**The monitor's poller makes the lookups, and `/check` only reads them.**
After each fleet poll the poller looks up at most `MAX_LOOKUPS_PER_POLL` (8) addresses it has no stored result for, before it takes the snapshot's write lock.
A completed result is kept for one hour and a failed one for five minutes, and at most 512 addresses are kept.
An address past the cap, or not yet reached, reads `not looked up yet  UNVERIFIED` until a later poll covers it.
`check::render` takes the stored results as data and resolves nothing, so a resolver that never answers delays the next poll by at most 16 seconds and never a request to `/check`.
`--check` has no later poll, so it makes its own lookups under the wider `MAX_LOOKUPS_PER_CHECK` (64).
Googlebot verifies only under `googlebot.com` and `google.com`. A name under `googleusercontent.com` does not verify it, because every cloud customer's virtual machine gets a reverse name there that also resolves forward.
`telemetry::CrawlerVerdict` gains `ForwardMismatch`, so a `match` over it needs a new arm.

**CI runs the workspace under the address and leak sanitisers on pushes to `develop`, in a workflow of its own, and fails when the sanitiser is unavailable (#164).**
A probe compiles and runs a trivial program with the same flags before the suite, so a missing runtime or a rejected flag fails the job in seconds instead of passing while measuring nothing.

**`[server] max_requests_per_connection` sets how many requests one HTTP/1.1 connection serves before m6-http closes it.**
The default is 100, as before, and 0 is a config error.
The key is read at start, so a change needs a restart.
A client that makes more requests than this on one connection reconnects, with a new TLS handshake.

**m6_core::client::Upstream, a non-blocking HTTP/1.1 client for an event loop.** It connects to a unix socket or a TCP address, optionally over TLS, writes one request and parses the response as bytes arrive, and can carry the next request on the same connection. Its buffers are allocated once, so a request over a unix socket or plain TCP allocates nothing. m6-http's unix-socket backends use it.

**m6_core::dns, a DNS question on its own non-blocking UDP socket.** dns::Query asks one PTR, or one A and AAAA pair, of the first nameserver in /etc/resolv.conf, and dns::Confirm makes the reverse-then-forward check a crawler claim needs, each driven by the caller's event loop. Messages are built and parsed by hickory-proto. Nothing else in m6 resolves names while serving: host names are resolved once at start.

**m6-core supplies the parts a service's own event loop is built from.** h1::ServerConn serves one HTTP/1.1 connection from buffers made at start, reading and writing until the socket would block, with no allocation per request. net::listen_unix and net::listen_tcp open a non-blocking listener. site::Site loads a site's config, templates and params at start, and handle and finish answer a request with the routing, rendering, minification and compression App used, returning the service's own routes to the service to answer.

**m6-log, a buffered JSONL logger for an event loop.** Lines are written into a 16 KiB buffer allocated at start and flushed when full, every second and at stop, as nginx's buffered access log does. Warnings and errors are written at once. No thread, no lock, no allocation per line. A destination that cannot take more is never waited on: lines are kept, then dropped and counted, as HAProxy does.

**m6-monitor's hourly check reports each node's `/health` and the age of its last 200.** A node is ok only when `/health` answers 200 with status ok, and every other node is a fault on its line and in the verdict.

**A cache node follows the site generation ID.**
A cache node is an m6-http whose `[genid] upstream` names the source.
It asks the source for the ID at start and every `heartbeat_secs` (default 10).
A new ID flushes the cache, which then refills on demand.
It stores a response only when its `M6-Genid` equals the ID it holds.
While the upstream does not answer, it serves every page it holds, at any age, rather than an error, and `/perf` reports `upstream_down`.
`/genid` on a cache node is forwarded to the source with the caller's own token.
`/perf` reports the node's ID, its upstream's round trip and whether the upstream is down.
Verified by `genid_cache_node.rs`, which starts a real source and a real cache node and covers the start-time ID, a publish through the node, a wake from a newer reply, serving held content with the source stopped, recovery, the forwarded `/genid` token check and warm fetches waiting for the ID, and by `genid_cache_allocs.rs`, which measures 0 allocations in 2000 store checks.

**A generation change on a cache node swaps the cache in one step.**
The rule is a full flush: no entry survives into the new generation, and the cache refills on demand.
Each replacement is one assignment, so a request sees the old generation or the new one and never a half-emptied cache.
A request already holding an entry finishes on the content it took, and the old generation stays whole for every holder until it drops.
The swap keeps the cache's byte bound and clears the serve-held policy, because it runs exactly when the upstream has answered with the new ID.
Verified by the `cache.rs` generation swap tests, which pin the full flush, the held response and the old generation staying whole, the bound and the serve-held reset, and by `genid_cache_allocs.rs`, which measures 2 allocations for a 4096-entry swap and 0 in 2000 store checks and 1000 hits.

**`/perf` reports a SHA-256 fingerprint of every certificate it loaded, so two certificates that expire on the same day are distinguishable, and `[monitor] one_certificate = true` declares that a fleet shares one certificate (#275).**
`TlsCertificate.fingerprint` is 64 lower-case hex characters over the certificate's DER, the same value `openssl x509 -noout -fingerprint -sha256` prints.
The whole certificate is hashed and not its key, because a renewal with the same key pair still issues a different certificate: the serial, the validity and the signature all change, and the fingerprint moves with them.
The value is computed once per certificate when the chain is loaded, in `tls::record_loaded`, and never per request, and a node older than the field reports `null`, which the monitor reads as "cannot say".
`m6-monitor` groups the fleet by the leaf fingerprint instead of by `notAfter`, so two certificates with one expiry no longer look like one certificate, and the check line gains `fp <12 hex>`.
With `[monitor] one_certificate` set, a node whose leaf fingerprint differs is a fault, which is how a two-node fleet and a same-expiry reissue can be read, and a node that reports an expiry but no fingerprint is warned about instead of counted as agreement.
Verified on 2026-11-16 on a macOS sandbox: `tls_reload` and the new unit tests pass, the fingerprint of two certificates issued into one second differs while their `notAfter` is equal, and each of the five new tests fails when its half is removed (no fingerprint computed at load, no fingerprint in the digest, the declaration ignored).

### Security

**rustix 1.1.4 to 1.1.6 (RUSTSEC-2026-0486).** rustix's xattr functions marked unwritten bytes as initialised when given a zero-length buffer. Found by `cargo deny check` on the build host; the update is the advisory's fix. `deny.toml` no longer allows the OpenSSL licence, since nothing in the tree uses OpenSSL now.

**A login throttle blocks five wrong passwords even when the series takes longer than `window_secs`.**
The count was anchored at the first failure, so the fifth failure could reset it to one and the sixth was answered 302 instead of 429.
The example's development config sets a five-second window, and an unoptimised password check takes about a second, so five wrong passwords span more than five seconds on a loaded host.
The window is now measured from the most recent failure, and a block lifts after `window_secs` with no further failure.
Verified with `failures_spread_across_more_than_one_window_exhaust_the_budget`, which fails with the first-failure window restored, and with example 05's 98 checks on the running stack.

**A `Content-Length` field value is now `1*DIGIT` only, so a `+5` is refused instead of read as 5 (RFC 9110 8.6).**
Rust's integer parser accepts a leading `+`, so a client could send `Content-Length: +5` and make the server wait for five bytes that never came, once per request, on a connection it kept open.
A value that is not all digits is now a malformed message, answered 400 with the connection closed: `+5` is refused, and `-5`, `5x` and an empty value stay refused as they already were.
Verified with `malformed_content_length_values_are_rejected`, which sends `+5`, `-5`, `5x` and an empty value with `Connection: close`, and asserts each is answered 400 with the connection closed. On the old parser the `+5` case is answered with nothing inside the read bound, so that case fails.

**SQLite dependency updated for RUSTSEC-2026-0459.** `rusqlite` moves to 0.41 and `libsqlite3-sys` to 0.39, which bundles SQLite 3.54.1, the release the advisory names as fixed. The bundled SQLite in m6-auth, m6-auth-server, m6-auth-cli and m6-core's `sqlite` feature is affected; no configuration changes.

**HTTP/2 now advertises SETTINGS_MAX_HEADER_LIST_SIZE and refuses a request whose decoded header list is over 64 KiB with a 431 (#162).**
Only the compressed header block was bounded, at 256 KiB, and HPACK lets one byte stand for a table entry of up to 4096 bytes, so a block under that cap could decode to a far larger header list.
The list is now counted as RFC 9113 6.5.2 defines it while it is decoded, and a field is kept only while the list is within the limit, so a request keeps at most 64 KiB of header fields, as RFC 9113 6.5.2 counts them, however far its block would decode. It is answered 431 and RST_STREAM on its own stream after it is decoded in full, so the HPACK table stays in step and other streams are unaffected.
Verified with `a_header_list_over_the_limit_is_answered_431_on_its_stream`, `the_server_settings_advertise_the_header_list_limit` and `an_amplified_block_never_holds_more_than_the_limit_in_memory`, which fail without the change and pass with it (a 6,009-byte block made the old decoder keep 8 MB of fields), and with a request exactly at the limit still served.

**A query key or form field can no longer replace a path parameter, and m6-file serves only the file its route's pattern addresses (#279).**
Core checked every path parameter for `..` and stray characters, then added each query parameter to the same request dictionary under the same name.
A request such as `GET /assets/a.css?relpath=../config` therefore reached m6-file with the unchecked query value as `relpath`, and m6-file served the file it named, outside the site directory.
Core now leaves a path parameter in place when a query key or form field has its name.
**Behaviour change:** a template that read a query value under a path parameter's name now gets the path value, and reads the query value as `query.<name>`.
m6-file now reads `relpath`, `filename` and each `root` placeholder only when the route's pattern captures that name, and checks each with core's path-parameter rule (a slash only in a `{*name}` capture). A name the pattern does not capture is never taken from the query, so `?relpath=` on a `{filename}` route cannot pick a file in a subdirectory, and a `root` placeholder the pattern does not capture is a 404.
Verified with `l2_query_relpath_cannot_replace_the_path_parameter` and `l2_query_relpath_cannot_choose_a_file_on_a_filename_route`, which send those requests over HTTP. The first fails on the previous code with `200 OK` and the service's config in the body. `a_dotdot_relpath_in_the_dictionary_is_404` covers m6-file's own check.

**A site-relative file helper follows no symlink out of the site directory, and a refused write creates nothing outside it (#346).**
`Request::validated_path` rejected `..` in the string and never resolved the path, so `read_json`, `write_json`, `write_json_atomic`, `list_json` and the multipart `write_bytes` reached `data/linked/file.json` through a symlinked directory to a file outside the site.
`write_json` then called `create_dir_all` on that path, so a refused write created the missing directories outside the site before writing.
The helpers now call one shared `m6_core::path::resolve_within`, which canonicalises the site root, canonicalises the deepest existing ancestor of the requested path, appends the components that do not exist yet, and refuses the result unless it starts with the resolved root.
A dangling symlink counts as existing and is refused, so a write cannot create its target outside the site.
A symlink that resolves back inside is accepted, which keeps a site that links `data` to `storage/data` working.
`..`, a leading `/` and a Windows prefix are still refused.
Verified with `read_json_refuses_a_symlinked_directory_that_leaves_the_site`, `write_json_refuses_a_symlinked_directory_before_creating_anything` (which asserts the outside directory is still empty), `in_site_symlinked_directory_still_reads_and_writes` and `read_json_still_refuses_dotdot`. The first two return the outside file and `Ok` with the previous lexical check restored.

**m6-file refuses a path whose symlinked directory leaves the site, on plain and tail routes (#339).**
`escapes_site_dir` called `symlink_metadata` on the last component only, so `GET /assets/linked/secret.css` with `assets/linked` pointing outside the site was served: the final component was a regular file and the check stopped there.
`serve` now resolves the path through core's `resolve_within`, checks the resolved path against the resolved site root, and reads the file it resolved.
A directory symlink that stays inside the site still resolves inside and is served.
Verified with `a_symlinked_directory_inside_the_site_is_refused`, which returns 200 with the outside bytes when the old last-component check is restored, and `a_symlinked_directory_inside_the_site_is_served`, which serves a site that links `assets` to `static/assets`. `a_dotdot_relpath_in_the_dictionary_is_404` still refuses `..`.

### Fixed

**A cache node with no generation ID retries a failed heartbeat on the next tick, and a confirmed disk copy writes the `disk cache kept` line.**
A heartbeat is a request with the backend deadline, so a loaded host or a stalled guest can miss one answer. The node then waited the whole configured interval before asking again. At `heartbeat_secs = 3600` one timeout left a node with no ID for an hour, and a node with no ID cannot serve a request because the store rule needs the ID a reply was dispatched under. A failure now schedules the next heartbeat one loop tick away while the node holds no ID, and the configured interval once it does. A disk copy installed unverified after a timeout is confirmed by the next answer that names the same ID, and that transition now logs `disk cache kept`, the same line the settle path already wrote, so `/perf` and the log agree. No config key and no route changes. Verified: the new `a_node_with_no_id_retries_a_failed_heartbeat` fails at `STARTUP_BOUND` (60 s) without the retry and passes in 1.8 s with it, `a_restart_with_the_source_down_serves_the_disk_copy` asserts the kept line on the confirmation, and three consecutive full workspace runs under 16 CPU burners passed with zero failures.

**`h2_tls_hit` read 3.002 on Linux and failed a budget of 3,001 because the HTTP/2 close path allocated, and it reads 3,000 now.** `Http2Conn::fill_recv` built `io::Error::new(io::ErrorKind::UnexpectedEof, "closed")`, and that constructor boxes its message, so every connection torn down inside the counted window added a call to `tests/allocations.rs`'s stop count. How many teardowns land in a window moves with when the shutdown arrives: dhat between two 1,100-request runs of the same binary differed by 3 calls at that site and by 1 at rustls's received-plaintext copy, which is how a row that makes 3,000 calls per 1,000 requests read 3.002 and failed on a host where the same row had read 2.999 to 3.001. Both close paths, `H2Io::Tls` and `H2Io::Plain`, now build the error from its kind, which allocates nothing, and the line a trace log prints for it reads std's "unexpected end of file". The request path is unchanged and still allocates none of m6's own. Verified on macOS against a dev build with `--features count-allocations`: fourteen runs of the row read 3,000 twelve times, 3,001 once and 2,999 once, and the fixed part of each run fell by exactly 3 calls (N = 1,000 read 3,222 before and 3,219 after, 2N = 2,000 read 6,222 before and 6,219 after). The call the 3,001 budget allows is one more TLS record received in a run than that run's baseline, which is one `Payload::into_vec` in `CommonState::take_received_plaintext` and can grow rustls's received-plaintext chunk queue. That corrects the 1.15.0 entry's account of the same call as a chunk-queue growth seen only in the 2N run: the growth is one call in the 100-request and the 1,100-request profiles of both runs, so it cancels in the difference, and the call above 3,000 is a record boundary.

**`[server] lock_memory` pinned every mapping the allocator had reserved, so m6-http held 1,229 MiB where the base build held 96 MiB, and the kernel's out-of-memory killer killed it.** `mlockall(MCL_CURRENT | MCL_FUTURE)` populates every current mapping and marks every later one to be populated, and mimalloc reserves 1 GiB of address space by default (`MI_DEFAULT_ARENA_RESERVE`, 1024*1024 KiB) on top of the `[limits.heap_reserve]` reservation, so the lock made 1.2 GiB resident that the load never touched. The build host measured 1,259,140 KiB resident with 1,062,912 KiB of it on transparent huge pages, against 95,856 KiB for the base build, while mimalloc's own committed figure was 328,269,824 bytes and the process's mapped size was 1,271,664 KiB. m6-http logged `heap: 1216 MiB on huge pages, memory pinned`, and 13 m6-http processes and 7 more of the build account's login session were killed. The pin is now `mlockall(MCL_CURRENT | MCL_FUTURE | MCL_ONFAULT)`, so a page is locked when the load first faults it and memory that is reserved and unused stays unresident. A page a cache hit reads was faulted during the warm-up and stays locked, so the hit path still takes zero minor page faults. `MCL_ONFAULT` is Linux 4.4 or newer, and availability is checked by asking the kernel: a kernel that does not know the flag fails the call with `EINVAL`, which is answered with an error naming the version and `lock_memory = false` rather than a fallback that pins every mapping. `LimitMEMLOCK=infinity` is what the unit sets, and a finite limit is refused at start with the error that names it. The refusal is owner policy, so the requirement is one error at start for every limit and every process, including a privileged process the kernel would have allowed. The kernel enforces the limit itself: `mlockall` with `MCL_CURRENT` refuses the call when `mm->total_vm` is over the soft `RLIMIT_MEMLOCK` (`mm/mlock.c`), and under `MCL_FUTURE` every later `mmap` and `brk` calls `mlock_future_ok`, which refuses a mapping that would take `mm->locked_vm` past the same soft limit (`mm/mmap.c`). The kernel cannot half-pin the process. The guard now reads the soft limit, the one the kernel enforces, so `LimitMEMLOCK=8M:infinity` is refused at start too, where the hard-limit check this replaces let it through. m6-http now logs its resident set and address space on either side of the pin (`memory: N MiB resident of M MiB mapped`), and `m6-platform`'s `process::resident_bytes` and `process::virtual_bytes` report them. `a_warm_server_is_resident_within_its_reservation` in `m6-http/tests/heap_resident.rs` warms a real m6-http with cache hits against a counting origin and holds the resident set to the process's own pre-pin size plus the reservation its config asks for plus a tenth, checks `VmLck` is above zero so a run with no pin at all cannot pass, and requires zero minor faults over 1,000 hits after the warm-up. The bound also has an absolute ceiling of 192 MiB from the small-cache run, so a regression that raises the pre-pin resident set as well as the formula still fails. The same test can be run against an eager pin by dropping `MCL_ONFAULT` in a throwaway worktree, and it then fails near 1.2 GiB. Verified on macOS: the workspace builds with zero warnings, clippy is silent, `cargo nextest run --workspace --no-fail-fast` passes 2,308 tests, and the doc tests and the platform gate pass. The Linux resident set, the minor-fault count and the strace table have not run yet.

**Two refusal tests could not refuse, and the measurement tools' site answered the load with 429.** `lock_memory_under_a_lowered_limit_refuses_naming_the_limit_and_the_fix` and `lock_memory_with_too_low_a_limit_exits_2_naming_the_limit_and_the_fix` lowered `RLIMIT_MEMLOCK` with `sh -c "ulimit -S -H -l 0"`. On Ubuntu `sh` is dash, which rejects that call (`ulimit: error setting limit (Invalid argument)`), exits 2 and leaves the limit unchanged, so the first test's child ran under the account's own limit and `mlockall` succeeded where the test expected a refusal, and the second saw the shell's error instead of the service's. Both now use `ulimit -l 0`, which sets both limits, and both check that the limit reads 0 before they rely on it, so a shell that accepts the command and changes nothing fails the test with that reason. `tools/memcalls-site.sh` generated a site with m6-http's default rate limit of 300 requests a minute, so a 10,000 request load was answered 429 after the first 300 and the counters measured the refusal path. The generated site now sets `[rate_limit] enabled = false`. Verified on macOS against a dev build: the load of 400 requests to `/` was answered 299 `200`s and 101 `429`s with the old site and 400 `200`s with the new one, `m6-http --dump-config` shows `"enabled": true` against `false`, the dash failure is reproduced with `/bin/dash` (exit 2, `ulimit: error setting limit (Invalid argument)`) while the corrected command prints 0, and the platform test fails under dash before the change ("the shell did not lower RLIMIT_MEMLOCK") and passes after it. `tools/memcalls.sh` now also prints `mapped`, `locked` and `anon-huge` beside `rss`, so a pin that makes reserved memory resident shows `rss` at `mapped` in its own output.

**`bench.sh` measured an HTTP/1.1 handshake and called it a cache hit.** Its HTTP/1.1 latency client opened a new TCP and TLS connection for every request, so the row read about 1.3 ms against 0.09 ms for HTTP/2, a tenfold gap on the same cached page. Nothing was slow in m6-http: its accepted sockets already set `TCP_NODELAY`, and a hit is written as one TLS flush. `m6-bench` now sends HTTP/1.1 over one kept-alive connection like a browser, reconnecting outside the timed region before the server's per-connection request cap, and labels the old row "new connection per request, includes TLS handshake". The path and URL-backend HTTP/1.1 rows use the kept-alive client too. A new suite, `--h1-vs-h2`, runs both in one process and fails if the kept-alive HTTP/1.1 p50 exceeds twice HTTP/2's. Verified on macOS against a dev build, 2000 requests: HTTP/1.1 keep-alive p50 58 us, p99 115 to 175 us; HTTP/1.1 new connection p50 1309 to 1359 us, p99 1467 to 1643 us; HTTP/2 p50 90 us, p99 130 to 135 us; HTTP/3 p50 137 us; h2c p50 73 us. The ratio is 0.64 against a limit of 2; setting the limit to 0.5 makes the suite print FAIL and exit non-zero. Removing `set_nodelay` from the accept path does not trip it on loopback, because a hit is a single write.

**`bench.sh` runs again.** It copied a certificate fixture that no longer exists, wrote a `[log] format` key the config now refuses, built m6-html and m6-file in a call that `--bin` restricted away, left the analytics log path pointing at a missing directory, and let the per-address rate limit answer the load generator 429. It now makes its own throwaway certificate, drops the key, builds in two calls, sets `[analytics] log_path` and turns `[rate_limit]` off. `BENCH_PROFILE=debug` runs the dev build to check the script; numbers come only from the default release build. Verified on macOS against a dev build: all suites pass and print their latency tables, with no 429, and `m6-http --dump-config` accepts the generated config.

**Lifecycle tests wait for the service's own `<name> started` line before they send a signal, not for its socket file.** `m6-monitor`'s `lifecycle_is_clean_with_history_on` failed "did not exit within 5s of SIGTERM" 4 times in 480 runs under a parallel `cargo build --workspace`: the socket is created before the service sets up its signals, and on macOS a SIGTERM that arrives in that window is discarded. The services are unchanged. `Service::wait_for_started` waits for the line the service logs after its signal handling is in place, and `assert_app_lifecycle` and the service test helpers that spawn `m6-monitor`, `m6-auth-server`, `m6-html` and `m6-file` call it after `wait_for_path`. Verified: 0 failures in 720 runs of that test under the same load.

**An HTTP/2 response longer than the send queue no longer stops after 32,768 bytes.** `flush_pending_streams` frames one 16 KiB queue per pass, and the connection wrote it and returned. A socket that took every byte never stopped being writable, so it never reported a new writable edge, and the edge-triggered loop did not run the flush again. A client that announces large windows, as a browser does, sends no WINDOW_UPDATE while a body is in flight, so nothing else woke the connection. The status and the content-length arrived, then 32,768 bytes, then nothing, for any response over 32 KiB. `send_until_blocked` now frames and writes until the socket would block or nothing is left that the windows allow, which is what an edge-triggered loop needs, and the writable edge it waits for arrives when the kernel drains below its mark. The tests `rfc9113_6_9_h2_serves_a_37_kib_body`, `_300_kib_body` and `_2_mib_body` fetch each size through m6-http with a client that announces browser-sized windows, and the h3 tests of the same sizes pin that HTTP/3 was never affected. On Linux the three h2 tests fail at the base and stop at ten seconds, and all six pass with the fix.

**`Smtp::fd()` returns `Option<RawFd>` and no longer panics, and a loop ignores an outgoing event for a slot that is no longer sending.** A client that left freed its slot and closed the slot's `Smtp`, and the slot's outgoing-token event, already in the same poll batch, then called `drive()`, saw the interest change and reregistered `fd()`, which panicked with "no connection" and stopped the service. `fd()` is `None` before `start` and after `close` or a finished delivery. `Option` was chosen over a deregister-and-reregister helper on `Smtp` because the loop already owns the registry and the slot state, so the type needs no second way to say "no socket", and `if let Some(fd)` is the form a caller already writes. The loops in `m6-core/tests/email_in_the_loop.rs` and `email_allocs.rs` now return early on an outgoing event when the slot is not `Sending`, and `docs/m6-core-reference.md` and `docs/m6-user-guide.md` state the rule. This is an API change to `fd()` only for callers that used it on a closed slot. Verified: `an_outgoing_event_after_the_client_left_is_ignored` frees the slot and then handles its outgoing event in fixed order, and it panics with "no connection" when the guard is removed and passes with it.

**Four tests that failed on a loaded host now wait on the event they need, and pass under load (a defect in each, none a retry or a longer timeout).** `m6-auth-server`'s three rate-limit tests and `t04_login_rate_limited_429` logged in as a user that does not exist, which makes the server run a cost-12 bcrypt verify per attempt, CPU work that a loaded host stretched past the 10 s read deadline ("no response within 10s"). They now log in as a seeded user with a cost-4 hash and a wrong password, the same failure path, a sixty-fourth of the work. `m6-core`'s `half_close_drains_then_closes` set its read timeout after half-closing, when the server may already have closed and macOS refuses `setsockopt` on a closed peer (`EINVAL`, panic at `recv_exact`); it sets the timeout first. `m6-monitor`'s `the_monitor_runs_one_thread` accepted two thread counts 100 ms apart that agreed, and short-lived start-up threads made that reading 6 or a history 2 higher; it now waits until the count reaches the settled range (loop plus the platform's certificate-store workers), and a thread that never leaves, such as one per node, still fails it. Verified under load (load average 50 to 130, with parallel `cargo build --workspace`): before, 2 of 3 auth tests failed in 3 of 6 runs, `half_close` failed 2 of 20 runs and the monitor test 3 of 48 runs; after, 4 of 4, 20 of 20 and 80 of 80 runs pass.

**Every buffer and queue the audit found unbounded, shrinking, or silently dropping is on a named limit with a `start`, a `max` and one rule.**
The rule is: allocate `start`, grow by doubling only when a write does not fit, clamp at `max`, keep the size, log one line on each growth, and refuse past `max` with an error that names the limit, the size needed, what was refused and the key and file to raise.
Twenty-two limits now exist (`docs/m6-app-anatomy.md` §4.2, Table 6), read from `[limits.<name>]` in a service's config and in m6-http's system config.
- m6-core: the HTTP/1.1 slot's read buffer (`request_body`) and write buffer (`response_buffer`) no longer shrink after a large request, a request past the maximum is 413 and a response past it is 500.
  The upstream client's read buffer (`upstream_read`) and request buffer (`upstream_request`, which had no bound) refuse past their maximums.
  The compressor's and minifier's input (`compress_output`, `minify_output`) and the SMTP reply buffer (`smtp_reply`, a fixed buffer that failed when full) follow limits, and so do the connection table (`connections`) and the ndjson page (`ndjson_page`).
- m6-http: `edge_request`, `edge_response` (which had no bound), `h2_recv` (no bound), `h2_header_block`, `h2_stream_body`, `refresh_queue`, `rate_limit_addresses`, `cache_bytes` and `connections`.
- m6-log: `log_buffer` grows from 16 KiB to 1 MiB where it was fixed and dropped lines, and `log_field` replaces the 1024-byte cut.
- m6-monitor: `history_queue`, and its node requests follow `upstream_read` and `upstream_request`.
- m6-file: `tail_read`, the page size of a tail response and the window a last-N-lines request scans.
Left without a key on purpose: the HTTP/1.1 head limits (431), the HTTP/2 reset memory, which is now twice the concurrency cap (200 where it was 128) so it stays above the cap whatever the cap is, and the idle URL connection pool, which closes a spare connection and loses no work.
Verified by a test per limit that fills it past its maximum and reads the refusal: `m6-core/tests/server_conn.rs`, `m6-core/src/limits.rs`, `m6-core/src/client.rs`, `m6-core/src/email.rs`, `m6-core/src/site.rs`, `m6-log/tests/limits_refuse.rs`, `m6-log/tests/escaping.rs`, `m6-http/src/http11.rs`, `m6-http/src/http2.rs`, `m6-http/src/rate_limit.rs`, `m6-monitor/src/history.rs` and `m6-file/src/handler.rs`.
The hot cache hit still allocates nothing (`m6-http --features count-allocations --test allocations`), and the page-path budgets in `site_allocs` and `request_allocs` are unchanged.
m6-http conformance (h1spec, h2spec, h3spec) was not run on macOS and must be run on Linux.

**A monitor poll on any path is counted as monitor traffic, not as a visitor (#107).** The channel table had two interfaces, so a header check on `/capabilities`, a `/health` poll, or any other monitoring request arrived in `http/*/external` and moved the visitor hit rate while no visitor had moved. `/perf` now reports `http/*/monitor` beside `http/*/external` and `http/*/internal`. The three monitoring endpoints are identified by their backend name, and every other path by the peer's address class. A monitor runs on the private network, so its poll arrives from a private address, while a real visitor's address is public after NAT. The class comes from the listener socket and the peer address the connection proved, so it reads no header and needs no configuration key. Three tests pin the three populations. `a_monitor_poll_on_another_path_is_not_external_traffic` fails as external traffic when the private-peer class is removed, `a_real_external_request_stays_external` fails as monitor when the class is over-broad, and `an_internal_backbone_request_stays_internal` fails as external when the internal socket is classified by its forwarded peer address instead.

**The HTTP conformance gate runs again.** `tools/conformance.sh` still wrote `[log] format`, which is now refused, and started the redirect listener with analytics on and no log directory, which is now refused too, so the gate measured nothing for h1's redirect listener, h2 or h3. It now writes neither, and runs the dev-profile build (`target/debug`, or `M6_BIN_DIR`) like every other gate; CI builds `--bins` for it instead of `--release`. `tools/backend-bench.py` no longer writes `[log] format` either. Verified on Linux: h1spec 32/32 on all four targets, h2spec 146/146, h3spec 47/49 (the recorded floor), resumption and early data as before.

**A source node's `M6-Genid` header no longer makes an HTTP/2 response unreadable.**
The header was serialised with its title-case name in the HPACK block, and RFC 9113 8.2.1 requires lower-case field names.
A strict client rejected the HEADERS frame as a protocol error and read no body, so a page that HTTP/1.1 served as 2671 bytes arrived as 0 over HTTP/2.
The HTTP/2 and HTTP/3 serialisers now write the lower-case spelling.
quiche already lower-cases the names it encodes, so HTTP/3 was correct on the wire, and the change there removes the dependence on that normalisation.
Verified with `h2_serves_the_same_body_as_h1` and `h3_serves_the_same_body_as_h1`, which fetch the same cached page through a real m6-http process and compare it with HTTP/1.1.
The h2 test fails on the previous code with a protocol error from a strict h2 client and passes after the fix.

**An HTTP/3 response no longer carries the backend's connection-specific fields, which cost the whole body.**
m6-http copied every field of a backend's HTTP/1.1 reply into the h3 field section, including `Connection: close`, which m6-html sends because it closes its connection after each page.
RFC 9114 4.2 makes a response carrying a connection-specific field malformed, and a client MUST treat it as such: nghttp3 (curl) reset the stream and read no body, so example 08's `/logs`, 1454 bytes over HTTP/1.1, arrived as 200 with zero bytes over HTTP/3.
HTTP/1.1 must carry the field and HTTP/2's serialiser already dropped it; both now take the connection-specific set from `m6_http_lib::fields::is_connection_specific`, which covers `Connection`, `Keep-Alive`, `Proxy-Connection`, `Transfer-Encoding` and `Upgrade` (RFC 9113 8.2.2, RFC 9114 4.2).
Verified with `rfc9114_4_2_h3_serves_the_same_body_as_h1_when_the_backend_closes`, which serves a no-store page from a backend that closes its connection, fetches it over HTTP/1.1, HTTP/2 and HTTP/3 and compares the three bodies: 2671 bytes on h1 and h2 with a reset stream on h3 before the fix, 2671 on all three after it.
On the examples' 08 stack with curl, `/logs` went from `200, 0 bytes` to `200, 1454 bytes`.

**A service writes its access line at the default log level (#179).**
`Site::handle` logged "request complete" at debug, below the default level of info, so a service that left its level alone had no access line.
It now logs at info, one line per request as a web server's access log does, and `[log] level` decides whether the destination keeps it.
Verified with `an_access_line_is_logged_at_the_default_level`, which finds no line on the previous code and an INFO line after it.

**A split body-less HTTP/2 backend response is forwarded instead of a 502 (#315).**
A backend that put END_STREAM on a HEADERS frame and finished the header block in CONTINUATION frames had its stream completed before the block ended, so the reply was failed as "stream ended without headers" and the edge answered 502.
The client now completes a stream only when the header block ends and END_STREAM has been seen, as RFC 9113 sections 4.3 and 6.2 require, in the shared h2c and h2s path.
Verified with `a_split_bodyless_response_is_forwarded`, which sees a 502 on the previous code and the backend's 200 after it.

**A backend's early response is forwarded instead of a 502.**
When a backend answered before it had read the whole request body (a 413 for an oversized upload, for example) and then closed, m6 treated the failed write as the result and answered 502, and on Linux the reply was lost.
The client now reads a reply the backend has already sent before it reports a write error, over plain and TLS connections, and the backend's own status reaches the client.
Verified: the socket and m6-http early-response tests on Linux and macOS (on Linux, 0 of 10 passed before the fix and 10 of 10 after).

**The test kit's scratch directory and port claims work on any host, so the suite has no list of known failures.**
`m6_core::testkit` chose `/var/tmp` for scratch files whether or not the process could write there, and kept port claims per build, so two builds on one host could claim the same port and one test reached the other's server.
Scratch files now go to the first of `TMPDIR`, `/var/tmp` and the system temp directory that can create, write and run a file, and port claims live in one directory per user that every build shares.
Verified: the full workspace suite on Linux and macOS with no failure.

**m6-http refuses an HTTP/2 or HTTP/3 request whose `:path` contains a space or a control byte, as HTTP/1.1 already does, and a request it will not forward no longer counts against a backend.**
Such a request passed ingress and was refused only when forwarded, after a pool member had been chosen, and the refusal marked that member failed.
It is now malformed at ingress and its stream is reset, as RFC 9113 8.1.1 and RFC 9114 4.1.2 require.
A request refused when forwarded, for example one with a CR or LF in a header value, no longer counts against the backend.
A member's failure count now counts transport failures in a row: a completed exchange clears it, and the backoff is capped before it is computed.

**On SIGTERM or SIGINT a service stops accepting, removes its socket and exits, without waiting for requests in progress (#338).**
A restart on a deploy cuts off a request that is still running, and that is accepted: the supervisor starts the service again at once.
Waiting for accepted requests took a request counter, a condition variable, a deadline setting and a shutdown hook, and gained nothing a restart does not already do.
The m6-html shutdown test now holds a request open at SIGTERM and again at SIGINT, and asserts only that the service exits 0 within five seconds with its socket removed.
The test harness's `Service::wait_for_exit` and `terminate` now also wait for the output threads to copy a child's last lines, which a test reading `output()` straight after the exit could miss.
Migration: `ThreadPool::drain`, `ThreadPool::in_flight`, `ServerConfig::drain_timeout`, `server::DEFAULT_DRAIN_TIMEOUT_SECS`, `[server] drain_timeout_s` and the `on_destroy` registration on `AppWithGlobal` and `AppWithState` are removed, along with the shutdown hook they fed. A caller does nothing: a `drain_timeout_s` key left in a config is ignored, and a service that needs durable state must not depend on a shutdown hook, because none runs.

**The config reloader reports a rename of a service's config once, and decides what changed from the file's bytes, mode, owner and modification time (#300).**
A rename over the config showed to `stat` a moment before its inotify event was queued, so a poll in that gap reported the change and the event reported it again. The old file's `ctime` also changes just before the name moves, which a second `stat` read as another change. The fingerprint is now a hash of the bytes with the mode, owner and modification time, and leaves out the inode and `ctime`, so a rename's transient state, which has the old bytes and times, is no change and the finished rename is one. Each check opens a file once. A check runs on every event from the watcher, named or not, and once a second. The first change after quiet is still reported at once and bursts are held as before. A `touch`, a `chmod` and a same-length rewrite in one timestamp tick are each a change. A `touch` forces a reload only when it moves the modification time, so on a filesystem with one-second timestamps a second touch in the same second, with unchanged bytes, is not seen.
Verified on the build host: a test that renames over a watched config 300 times under CPU load failed in 19 of 20 runs on the old code and passed in 20 of 20 on this release's code, and deterministic tests cover each kind of change, including a config reached through a symlink whose target directory is swapped.

**`a_burst_of_writes_is_at_most_two_reloads` no longer fails on a busy machine (#299).**
It wrote the config three times 10 ms apart and expected at most two reloads, but the reloader only merges a write that lands within 20 ms of the last report, so on a loaded runner a late write was rightly reported a third time. The test now spaces its writes 5 ms apart (longer than the service loop takes to wake and reload, so it is unlikely to merge them by itself), times each burst, and judges only one that fitted inside the window, retrying up to five times. Verified on the build host, 20 runs each: 20 passes with four CPU-bound processes running, and 20 failures reporting three reloads with the reloader's hold removed.

**JavaScript labelled `text/javascript` is now compressed by default (#186).**
The default tables disagreed. `text/javascript` was minified but missing from the default compression types, and the `.js` and `.mjs` extensions map to that label, so every script was minified and sent uncompressed. It now gets the same levels as `application/javascript` (brotli 6, gzip 6), and a test keeps the two default tables in step.
A site that switched compression off by setting brotli and gzip to 0 on each default type, and declares `compresses = false` for its backend, must now set `text/javascript` to 0 as well. Until it does, the service refuses to start or reload with "compresses = false, but this service DOES compress".
Verified by a request-level test that asks for `text/javascript` with `Accept-Encoding: br` and expects a brotli response, and by a test that the two default tables list the same types. Both fail on the old tables.

**The edge now skips an informational (1xx) response from a backend and returns the final one (#100).**
A backend that sent `103 Early Hints` ahead of its answer had the 103 taken for the response, so the fetch failed. The HTTP/1.1 reader and both HTTP/2 backbone clients now drop any 1xx other than 101 (RFC 9110 15.2) and fail the fetch as a bad gateway after eight of them, so a backend cannot hold a worker by sending them without end. Before this, an HTTP/2 client carried a 103's fields into the final response, which the edge could then cache.
Verified by new loopback backend tests for each reader, which fail before the change and pass after it, covering a 103 ahead of the final response, a head that arrives one byte at a time, and the bound at eight and nine.

**CI runs example 05's integration test after its stack is up (#277).**
The examples' unit-test step ran `cargo test --workspace` in m6-examples before example 05's `dev.sh` had created the dev certificate and auth database. Since m6-examples #11 that test fails on missing prerequisites instead of skipping, so every pull request failed. The step now skips that test, and the example 05 step runs it once the stack answers, as m6-examples' own CI does.
Verified by this pull request's own CI run.

**The signal-mask guard fails closed (#178).**
When the thread's signal mask cannot be read, a service now refuses to start instead of assuming SIGTERM is blocked.

**URL backends and HTTP/2 upstreams no longer block or wait on a timer.** An http:// or https:// backend ran each request on its own thread with a new TCP and TLS connection, and its reply was noticed on a 10 ms poll. An h2c or h2s upstream connected, and for h2s completed its TLS handshake, inside the event loop, so an unreachable upstream stalled every request on the node. All of them now connect, write and read in the loop, http(s) connections are reused, and backend_timeout_secs applies to HTTP/2 upstream streams too. An idle m6-http wakes about once a second instead of 100 times.

**A backend that refuses a request no longer takes its pool member out of rotation.** When a backend closed the connection while m6-http was still sending the request (m6-file answering an oversized header, then closing without draining it), m6-http marked the member failed and backed it off for a second, so the next request to that backend answered 502. A member that never received the whole request is now released. A member that took the whole request and then failed, a silent member and a refused connect still back off.

### Docs

**The docs now state the four cache counts, the HTTP/2 send loop and m6-file's tail mode, and drop five claims the code no longer meets.**
The README's 8 KiB stack header buffer and its two out-of-scope items (per-address rate limiting, shared-port scaling), the m6-file page's "no route can set `Cache-Control`" and "tracked as #126", and the head-limit refusal named 431 where the code sends 400 and 414.
Verified by loading each configuration example with `--dump-config` of the matching binary.

**The README, `docs/m6-http.md`, `docs/m6-site-toml.md`, `docs/m6-testing.md`, `docs/m6-user-guide.md` and one changelog count now match the code.**
The renderer library is `m6-core`, because `m6-render` was deleted.
The monitor population excludes loopback, and a public address is monitor only on `/health`, `/perf` and `/traffic`.
The `[health]` table has two access keys.
The certificate comparison groups on the fingerprint.
`m6-file` computes `Cache-Control` from the query string.
The route-method change names seven end-to-end tests.
Verified by counting the tests, reading the classifying code and running `tools/check-commit-hashes.sh`.

**`README.md` follows the documentation ruleset, and its example check count, its backend pool description and its configuration example now match the code (#128).**
Fifteen numbered sections, a contents list, and every table and figure numbered, captioned and referenced.
The install section says 98 checks, which is what example 05's suite reports and what this file's earlier entries record.
Backend pools say round robin, because `m6-http/src/pool.rs` chooses the next member in turn.
The configuration example is split into `site.toml` and the system config, because `[server] tls_cert`, `tls_key` and `[auth] public_key` belong to the system config, a key in the wrong file stops the start with exit 2, and the system config needs `[node]`.
The `m6-auth-cli` synopsis gains `user roles`, `group member ls` and `passwd --password`.

**The event loop is described once, in `docs/m6-architecture.md` §1.10,** with m6-http's own loop in `docs/m6-http.md` pointing at it. Two false claims went: that backend calls have no timeout (`backend_timeout_secs` exists, default 30) and that m6-http is "single-threaded epoll" by name. `CONTRIBUTING.md` says which loop changes need approval: a new disallowed-method exception, a thread, or an allocation on the loop's path.

**`docs/m6-core-reference.md` sections 1 to 5 follow the documentation ruleset, and the module and line counts, the `RouteConfig` fields and the watcher description now match the code (#144).**

**`docs/m6-http.md` documents m6-http's HTTP/2 denial-of-service behaviour: the Rapid Reset concurrency cap, send-side flow control on the backbone clients, the five-state stream machine and the known gaps (#160).**
Documentation only. Each claim names the function or test that enforces it.

**The m6-auth-cli reference follows the documentation ruleset, and its token defaults, config keys, key format and exit codes now match the binary (#138).**

**The m6-md reference follows the documentation ruleset, and its params snapshot and watch-mode first pass now match the binary (#150).**

**`docs/m6-auth-lib.md` follows the documentation ruleset, and its storage description now matches the code (#139).**

**`docs/m6-backend-protocol.md` no longer states two MUSTs that the reference `App` does not meet (#200).**
The proxy's user must be able to connect to the backend socket, with `0660` and a shared group preferred and `0666` allowed only inside a restricted directory. A backend may support persistent connections for clients that ask for them, and must close after responding to a request that carries `Connection: close`, which the proxy sends. `docs/m6-backend-examples.md` and `docs/m6-app-anatomy.md` now say the same.

**Source comments in m6-http, m6-monitor and `tools/conformance.sh` no longer carry one deployment's hosts or measurements, and the HTTP/1.1 module header states the keep-alive the code implements (#190, #189 in part).**
Comments only.

**The build-host gate runs example 05's end-to-end test after its stack is up (#273).**
The examples unit-test step skips `blog_site_end_to_end`, which needs certificates and the running stack, and the gate runs `cargo test -p render-cms --test integration` once that stack answers, counting a failure into the examples result.

**Source comments in m6-core and m6-file no longer carry a deployment's names, per-node sizes or deployed versions, and no longer use "load-bearing" (#189, #190).**
Comments only.

**`docs/m6-backend-protocol.md` now follows the documentation ruleset, and its claims match the code (#142).**

**The user guide's install section, Example 01 and Example 02 now match the released examples and follow the documentation ruleset (#156).**

**The user guide's Examples 03 and 04 now match the released examples and follow the documentation ruleset (#156).**

**docs/POSITIONING.md no longer carries the withdrawn 22.0 µs and 158,323 req/s figures, and its cross-server tables are replaced by a caveated capability comparison (#192, #158).**

**`docs/m6-backend-examples.md` follows the documentation ruleset, and it now agrees with the backend protocol that compression is the backend's job and matches `docs/BENCHMARKS.md` and `tools/build-host-tests.sh` (#141).**

**`docs/m6-architecture.md` now names `M6_SOCKET_OVERRIDE` as the one environment variable a service reads, and the `socket_path_from_config` doc comment points at that page (#187).**

**The user guide's Example 05 (CMS blog) now matches the released example and follows the documentation ruleset (#156).**

**`docs/m6-site-toml.md` part 1 follows the documentation ruleset, and it now says that a secrets file refuses a key set in both files and that `[server]` and `[auth]` belong in the system config (#154).**

**CHANGELOG.md's preamble and its 1.15.0, 1.14.0, 1.13.0 and 1.12.1 entries now follow the documentation ruleset (#129).**

**The user guide's admin dashboard section says the bench targets come from the system config's `[server].bind`, as example 11 now has it (#305).**

### Changed

**Every m6 service installs mimalloc as its global allocator, and a warm service stays out of the kernel's memory calls.**
`m6_core::alloc::M6Alloc` wraps mimalloc and is the one allocator type: m6-http, m6-file, m6-html, m6-md, m6-auth-server and m6-monitor declare it in `main.rs`, and every allocation-counting test and example declares the same type, so counts are taken against the allocator production runs. Under `alloc-count` (the `testkit` and `count-allocations` features) it counts exactly as `testkit::alloc_count::Counting` did, and without them the counting is not compiled. `Counting` is removed. A library crate declares no allocator. m6-auth-cli is a one-shot command and keeps the system allocator.
Each service sets mimalloc's options in code at the top of `main`: freed memory is never returned to the OS (purge delay -1; the option's number is counted from the bundled `mimalloc.h` by `m6-core/build.rs`, because the binding does not export it), so the heap grows and does not shrink. It then reserves `[limits.heap_reserve]` bytes from the OS once, committed and on huge pages where the kernel offers them, and logs once when the heap later grows past it; growth is never a failure.
This is on trial. `docs/PERFORMANCE.md` §2.8 says the A/B in §7.2 decides whether it stays, and a worse latency, or memory calls that do not fall to zero, reverts it.
Verified on macOS: 2301 tests pass; every counting test (`alloc_count_armed`, `event_allocs`, `limit_page_allocs`, `server_conn`, `site_allocs`, `upstream_no_alloc`, `email_allocs`, `allocations`, `client_allocs`) builds and passes with its budget unchanged.
Not yet verified on Linux: the strace, `perf stat`, latency and RSS comparison on the idle build host (`tools/memcalls.sh`, `tools/hwcounters.sh`, `tools/perfcheck.sh`).

**A rendered page makes no allocation of its own: the dictionary overlay, its built-in values and the response's header list are reused from one request to the next (template request 27 to 22 allocations, page path 41 to 36, handler request 21 to 16).**
The overlay is taken from a spare one on the thread, cleared and not freed, and it keeps only the buffers of `request_path`, `datetime` and `year`; `datetime` and `year` are formatted into stack buffers; a response's header list returns to a spare list when the response is dropped. The 22 that remain on a page are Tera's and minify-html's, and a traced run of `site_allocs` shows all 36 on its path are theirs.
A new limit, `[limits.request_dict]` (32 entries at start, 4096 at most), bounds the fields of one request, and a request past it is refused with a 400 that names the key.
**Rollback:** revert the merge commit of `perf/reusable-page-context` (`git log --first-parent --merges --grep reusable-page-context`). The overlay returns to a `BTreeMap`, the built-ins to a `String` each request and the header list to a `Vec` made per response, five allocations on a page, and the budgets go back: `site_allocs` 36 to 41, `request_allocs` template 22 to 27 and handler 16 to 21. The `request_dict` key goes with it. `docs/PERFORMANCE.md` §2.7 records the same.
Verified by the three budgets, which fail when an allocation comes back, by `a_dictionary_reuses_the_last_ones_built_in_buffers_and_shows_nothing_else` (the built-in's buffer is the last request's and nothing else of it is visible), `a_header_list_is_reused_by_the_next_response`, `an_insert_past_the_maximum_is_refused_and_reported` and `a_request_with_too_many_fields_is_refused_with_a_400`.

**A rendered page's header names and values are borrowed, and the page's copy out of the page area is the minified body (page path 50 to 41 allocations, template request 36 to 27, handler request 23 to 21).**
A route's `Cache-Control`, `Last-Modified` and extra headers are made once at load as `Arc<str>` texts the route owns, and each response clones the `Arc`, the header names are literals, and the page's `ETag`, 18 bytes, is held inline in its header.
Of the 27 allocations a rendered page still makes, 22 are Tera's and minify-html's and 5 are ours: the built-in `year` and `datetime` values (2), the dictionary overlay's first node and key insert (2) and the response's header vector (1).
The wire is unchanged: CACHE-HDR's tests, the cache-hit 304 tests over HTTP/1.1, HTTP/2 and HTTP/3 and `rfc9111_5_2` pass without an edit.
The `ETag` is still computed in `finish`, not at serialisation, because CACHE-HDR's contract is that the caller and the edge's cache see it in `Response.headers`; held inline it costs no allocation.
**Rollback:** revert the merge commit of `perf/borrowed-response-head` (`git log --first-parent --merges --grep borrowed-response-head`). `Response.headers` returns to `Vec<(String, String)>`, each header a page sets costs a `String` pair again, and the budgets go back: `site_allocs` 41 to 50, `request_allocs` template 27 to 36 and handler 21 to 23. `docs/PERFORMANCE.md` §2.6 records the same.
Verified by the three budgets, which fail when a header comes back as a `String`, by `a_rendered_pages_route_headers_are_borrowed_not_made_per_request`, which reads the borrowed text and the inline `ETag` off two requests, by `a_reload_frees_the_old_sites_route_header_texts`, which reloads a route 1,000 times with changing values and finds every old text freed once its response finishes, and by the unit tests of `HeaderText` and `InlineBuf`.

**`docs/m6-http.md` follows the documentation ruleset, and the claims the code no longer supported are corrected (#148).**
The document had 20 em dashes, no contents list, unnumbered headings, three semicolons and captioned tables out of order. It now has none of the dashes or semicolons, one contents list, numbered headings, sequential table captions and one sentence to a line.
The rewrite corrected statements that were false against the code: m6-http stores the RFC 9111 section 3 statuses (404, 405, 410, 414 and 501 among them) and never a 206 or a 5xx, and it keeps no data-file-to-URL invalidation map. It removes the eight hop-by-hop fields, adds `Via` and drops connection-specific response fields on HTTP/2 and HTTP/3. The decoded head limit is 32 KiB, and the h2c and h2s clients are one `H2Client`. The health keys live in the system config, `/health` answers for the process alone, and logging and the HTTP/3 stack are named as the code has them.
The rate-limit text from the per-process change is kept and sits in section 4.1, with the 900-request example, and `/health`, `/perf` and `/traffic` are stated to ride the shared port.
Verified by reading each changed claim against the tree. Documentation only, no build run.

**`[log]` has one type and one parse, owned by m6-core (#98).** The `[log]` table is read by `m6_core::config::LogConfig::from_toml`, the same function for `site.toml` and a service's own config, and m6-http re-exports that type instead of declaring a second one. A level set in `site.toml` still reaches the logger and still means `info` when absent, and the set of files accepted and refused is unchanged. The one visible difference is that m6-http's `[log]` error now names the table and the key the way every other service's does.

**A rendered page is answered from the borrowed request head, the dictionary's built-in keys are borrowed, and the page path fell from 67 allocations to 50; a handler request fell from 29 to 23.** `Site::handle` used to copy the method, path, version and every header into a `RawRequest`, and then a `Request`, for every request, including a template route that never hands one to a handler. A config template route now builds its dictionary and runs the whole finish path from the borrowed `RequestHead` through a `RequestMeta` view; a handler, a code route, `/genid` and a 404 still get the owned request, and a `Transfer-Encoding` head falls back to the owned path because its body is decoded into an owned buffer. The compression candidate list is a fixed two-slot array instead of a `Vec`, the route's `Last-Modified` is formatted once when the route is compiled, the `ETag`'s value is written into a String sized once, `Dict`'s overlay keys are `Cow<'static, str>` so `query`, `cookies`, `request_path`, `datetime` and `year` borrow instead of allocating, and a route's template name is an `Arc<str>` shared with the response. Verified with `site_allocs`, which asserts a budget of 50 so it can only fall, and `request_allocs`: 23 allocations for a handler request, 36 for a page, and an `identity` response no longer costs more than one whose compression block is skipped (difference 0). What is left on the page path is Tera's and minify-html's own allocations, plus 13 of ours that need a borrowed request context and a borrowed response head; each is named with its line and cost in `work-queue/out/ALLOC-RENDER-ZERO.md`.

**A cached HTTP/2 hit builds no owned request and allocates nothing of m6-http's own: `h2_tls_hit` fell from 15.000 to 3.000 calls per request, and its budget is 3,000 on macOS and 3,001 on Linux, where the 2N run counts one growth of rustls's received-plaintext chunk queue (`CommonState::take_received_plaintext`).** The 3 that remain are rustls's per-record buffers, two `Payload::into_vec` on the decrypted request and one `PrefixedPayload::with_capacity` on the encrypted response, which I-174 accepts. The request line and header list are `(offset, length)` views of the connection's read buffer: method, path and query are `&str`, the fields are `H2Field` ranges, and `Host` is `:authority` presented at lookup rather than copied. The owned `HttpRequest` is built only when a miss hands the request to a backend, and a request parked across loop turns keeps its ranges into the buffer. Frames are parsed at an offset in one receive buffer rather than split into a new `Vec`, so the frame payload is never copied and the buffer keeps its capacity for the next read. The decoded field list is returned to a per-connection spare and the stream state to a retired pool, so a stream reuses both instead of allocating a header list per request. Verified on macOS with the `count-allocations` guard and `alloc_attribution` at 3.000 calls and 267.977 bytes per request, with the h1 and miss rows unchanged, and with four breaks that each restore one copy (the owned request, the frame `split_off`, the exact-capacity remainder, and the per-stream header list) and put the row over 3,000.

**A `no-store` response is no longer minified, because no cache repays the work.** A cache keeps what it stores, so minification is paid once per fill and its saving collected on every hit. A response carrying `Cache-Control: no-store` has no second hit, so the cost landed on every request to save a few bytes after compression. `Site::finish` now skips the minifier for those responses through the same `cache_control_no_store` rule the default ETag already uses, so the two agree. Verified with `a_no_store_page_is_sent_unminified`, `a_cacheable_page_is_still_minified` and `a_no_store_json_answer_is_sent_unminified` in `m6-core/src/site.rs`. The two `no-store` tests fail on the previous code and the cacheable one holds the other direction.

**HTTP/2 and HTTP/3 send responses in the order the client's `priority` header asks for, and the kernel's send queue is bounded so that order survives the socket.** m6 ignored the field and let the kernel choose what left first, so a page's stylesheet waited behind a large image on the same connection. `priority::from_headers` parses the RFC 9218 Dictionary itself, in place and without allocating, because quiche's parser for the same field needs its `sfv` feature and would add a dependency for two members. On HTTP/2 a stream records the priority from its request's HEADERS, and the flusher takes its next frame from the lowest urgency that has a response the windows allow, in stream-id order. A non-incremental response is sent to its end before the next one starts, and an incremental one takes a frame each in turn (RFC 9218 section 10). `TCP_NOTSENT_LOWAT` bounds the send queue on Linux, so m6 rather than the kernel decides the order. Verified with the parser's tests against the RFC's examples and its out-of-range and garbage rows, and with `incremental_streams_interleave` and `one_end_stream_when_the_peer_ends_after_ours`. The m6-http lib tests pass.

**Every m6 service and m6-core log through m6-log, and `tracing` is gone.** The log lines are written into m6-log's buffer inside the event loop and flushed by it, so no service has a logging thread or a `tracing` subscriber any more.

**m6-http's per-request analytics line allocates nothing.** The line is built from borrowed request fields straight into m6-log's buffer. Verified with `analytics_allocs`: 0 allocations in 1000 returning-visitor requests and 0 in 1000 new-session requests, with all 2002 lines written. The one allocation left is the `Set-Cookie` value for a new session, once per visitor rather than once per request.

**Choosing a backend socket is plain round robin, and a request whose connect fails tries the next member.** The least-connections choice and the 1 to 30 second per-member backoff are gone, with the failure count and the retry deadline they read. Each request takes the next member in turn, so `N * k` requests put exactly `k` on each of `N` members, and a client receives 502 only when every member has failed. A backend that stops and restarts on the same path serves again on its next turn. The `/perf` pool counts report every member as in rotation. Verified on the sandbox with a real `m6-http` in front of three and four unix-socket backends, and a member killed and then restarted on the same path.

**Every test binary answers the standard test-list protocol, so `cargo nextest` runs the whole workspace.**
Nine binaries that run without libtest's harness (the allocation counters and the loop tests that need one thread or a blocked signal mask) now answer `--list --format terse` and accept a name filter, so a test runner can list and run them like any other. Each still forces one thread after parsing its arguments. Verified on Linux with `cargo nextest run --workspace`: 1989 tests in 97 binaries listed and passed.

**Every clock read outside `m6-platform` is refused by its gate, including reads through `chrono`, `time` and `filetime`.**
Five shipping reads that asked the operating system directly now take the time from `m6_core::platform::now()` or `wall()`, and `chrono` only formats a value it is given.
Verified: the platform gate on Linux and macOS.

**The minimum supported Rust version is 1.89** (was 1.88), the version the updated SQLite binding requires.

**m6-http drives only the connections that have events, accepts only when a client is waiting, and resumes a blocked write when the socket can take more.** Every HTTP/1.1 connection shared the listener's token, so any event drove every connection and tried a failing accept, and a response that filled the socket buffer waited for the next unrelated wake-up. Each descriptor now has its own token, each turn reads and writes until the socket would block, a connection with unsent bytes asks for write readiness, and idle and read timeouts are deadlines rather than checks on every pass. On the build host this removed one failing accept4 per loop pass and one or two recvfrom calls per request.

**A backend reply reaches the client in the same loop turn it arrives.** Replies from socket backends and from the h2c and h2s upstream pools went over a channel that the client's connection checked only on its next wake-up. They are now written straight into the waiting HTTP/1.1, HTTP/2 or HTTP/3 stream, with no channel per request. A custom error page from a socket backend is fetched the same way.

**A request's own code path went from 41 allocations to 29, and a rendered page from 52 to 51, against a target of zero.** Both counts are measured by the counting allocator, not estimated, and they only ever fall: a dependency update that moves one records the new count and the reason in its commit. What remains is rustls's buffer per encrypted record, the response's own buffers and what Tera does when it renders.

**A cached HTTP/1.1 response is served without heap allocation in m6-http's own code.** The request was copied into owned strings, and the cached headers and body were copied and every header line formatted into a new buffer for each hit. The request is now parsed into slices of the connection's buffer, the cached entry is shared rather than copied, and the response head is written straight into the connection's output buffer, which is allocated once per connection slot. TLS, logging and HTTP/2 and HTTP/3 hits still allocate and are handled separately.

**An HTTP/1.1 connection reuses one request buffer and one response buffer, and a response head is written from slices rather than formatted into new strings.** Each request allocated its own read buffer, and each response started from an empty `Vec` that grew eight times to fit a cached hit's head. dhat put 8,000 of a cached hit's 11,000 allocations per 1,000 requests on that growth and 1,000 on the read buffer. Both buffers now belong to the connection, cleared at the end of a request rather than dropped, so the capacity a request grows is the capacity the next one uses. The response writer appends the status line, each header name and value, and the length from the borrowed slices they already are, so no header line is built in a `String`, and a `Cache-Control` field is parsed into borrowed slices instead of a `String` per directive. Measured with `alloc_attribution` on macOS, in calls per request: a cached hit 11.003 to 2.000 (two rustls records, none of m6-http's own), a cache miss 91.004 to 58.004, and a new connection's request part, that row less the handshake-only row, 10.000 to 4.000. The allocations guard's budgets are lowered to those counts in the same change: 11,200 to 2,200, 91,204 to 58,207 and 123,200 to 117,200. Verified with `alloc_attribution` and `allocations`, with the whole workspace suite (2,064 tests), and with a break that restores the per-response buffer and the per-header `format!`, which reads 11.000 and 73.001 and fails both rows.

**A cache entry stores its response headers in one byte arena instead of a `String` per name and value.** Inserting a backend reply into the cache cloned every header pair into a fresh `Vec` so the entry could own it, which dhat measured at twelve allocations per inserted response. The entry now owns one tape holding the names and values with each field a pair of `u32` ranges, shared through `bytes::Bytes`, so a hit is a refcount bump and the hit writer borrows `&str` from the arena. `Set-Cookie` and `M6-Genid` are marked in the tape as not storable and are filtered when the entry is stored, which keeps one visitor's cookie out of the shared cache without a second copy in the ordinary case. Measured with `alloc_attribution` on macOS: the h1 cache-miss row falls from 58.004 to 47.001 calls per request, and the h1 hit and new-connection rows are unchanged at 2.000 and 117.000, with h2 at the trunk fix's 3.000. The allocations guard's miss budget is lowered to 47,004 in the same change. The cost is one arena per entry, sized to its fields plus four spare slots and 128 bytes of text, so a small entry holds about 200 bytes of slack; the capacity bound still counts every byte the arena allocates, because `HeaderBlock::footprint` reports the reservation rather than the written length.

**m6-core: `config::secret_path` is the one absolute-path check every crate uses, and m6-auth-server's two path bases are gone (m6 #16). m6-monitor reads its fleet once at start, so a fleet it cannot read stops the start with exit 2.**

**m6-http: `tls_cert`, `tls_key`, `[auth] public_key` and `metrics_token_file` take absolute paths, from either file.**

**m6-core: an unknown log level, a missing or invalid `site.toml` and a failed socket chmod stop a service at start with exit 2.**

**m6-http: each setting is read from one file, and an unknown or misplaced key, a missing `[node]` or an unusable metrics token file stops the start with exit 2.**

**`check.sh` and `tools/build-host-tests.sh` share a build host safely: builds and lints run side by side, the steps that bind fixed ports take turns, and a measurement runs only when the host is otherwise idle (#344).**
The protocol is written once, in `tools/host-lock.sh`, which `check.sh`, `tools/perfcheck.sh` and both build-host halves of `tools/build-host-tests.sh` source. A run holds `/var/tmp/m6-host.lock` shared, takes `/var/tmp/m6-test.lock` around the test suite, conformance and the examples' tests and stack, and builds its tests before taking it. A measurement drops its shared hold, queues through an entry lock that stops new runs from starting while it waits, and takes the host lock exclusive. If the host is not idle within `M6_HOST_IDLE_WAIT` seconds (default 7200), the measurement reports NOT MEASURED and the run fails, and `check.sh` says the run is incomplete. Lock files are created readable by every account and opened read only, and processes a run leaves behind do not keep its locks. The remote half of `build-host-tests.sh` keeps its logs in a directory of its own. Where `flock` is missing, as on macOS, nothing is locked.

**CI runs every independent check in the build-and-test and examples jobs even after an earlier one fails, and clippy is a job of its own, named `clippy`, so one push reports every failure at once and a notification names the check that failed (#340).**
A failing test used to skip clippy and the opt-in feature tests, and their problems surfaced only on the next push. A check that needs an earlier step's output (the toolchain, compiled code, the development certificates) runs only if that output exists, and a compiler warning fails its own step without skipping the tests. The test steps also fail on cargo's own exit status, so a test build that does not compile or a test binary that aborts can no longer show green. The `main takes develop only` check now reads the pull request's branch name from an environment variable instead of pasting it into its script, where a crafted name could run as a command on the runner.

**m6-core now has a `minify` feature beside `templates`, and the services that use neither take core with `default-features = false`, so `m6-http`, `m6-monitor`, `m6-auth` and `m6-auth-server`, built on their own, no longer link a template engine, a Markdown engine or a CSS engine (#181).**
Both features stay on by default, so a crate that depends on core as before is unaffected and needs no change. `m6-file` names `minify` alone, and `m6-html` and `m6-md` keep the defaults. Cargo unifies features across one build, so this holds when a service is built on its own (`cargo build -p`, `cargo install`); in a `cargo build --workspace`, `m6-html` turns `minify` on for every crate. Built on its own, a service without `minify` stops minifying its own responses: `m6-monitor` (its dashboard HTML) and `m6-auth-server` (its JSON) send them unminified, and `m6-http`, `m6-auth` and `m6-auth-cli` are built the same way. That costs them nothing and saves CPU per response. A build without `minify` also refuses a config that turns minification on, and the error names the feature. Measured on the build host on one clean tree: `cargo tree -e normal` for `m6-http` fell from 464 lines to 263 and shows none of lightningcss, minify-html, minify-js, tera or comrak, the `m6-monitor` release binary fell from 28.0 MB to 7.1 MB, and the `m6-file` one from 23.5 MB to 17.8 MB. Whole-workspace builds barely move, because `m6-html` and `m6-md` still ask for the defaults and cargo links one core for the whole build. The numbers and the latency check are in the pull request.

**A fixed route beats a pattern route, and two routes that a request could both reach are now a configuration error, in the services and at the edge (#289).**
A route is fixed when every segment is a literal and a pattern when it has a `{param}` or a `{*rest}`. Two fixed routes on one path, and two patterns that can both match some path (`/blog/{slug}` with `/blog/{name}`, with `/blog/{*rest}`, or `/{lang}/about` with `/blog/{slug}`), fail the load when a request method reaches both. A service refuses to start with exit 2, a reload is refused and the running routes keep serving, and `--dump-config` fails the same way. The edge applies the same rule to `site.toml` routes. Before, the first declared route answered a tie and the other could never be reached.
A fixed route is now found by one hash lookup and the patterns are tried only when no fixed route answers, so a request has no tie-break to resolve.
Migration: the error reads `routes "/blog/{slug}" ([[route]] number 1) and "/blog/{name}" ([[route]] number 2) can both answer GET /blog/x; remove one, give one explicit methods, or make one a fixed path`. Remove one of the two routes, give one a `methods` list the other does not share, or replace a parameter with the literal it was standing in for. The layout of a code `route_post("/contact")` beside a config `[[route]]` for `/contact` now needs `methods = ["GET"]` on the config route, because a route with no `methods` also answers `POST`.
Verified by unit tests of the rule (each fails when its part of the check is removed or made too eager), by m6-html and m6-file refusing to start, `--dump-config` refusing, an m6-file reload into a conflicting config being refused while the old routes keep answering, and an m6-http load refusal.

**A route's `methods` list is exactly the methods it answers, and an unknown method name fails the load (#336).**
A list of two or more methods, or one unrecognised name, used to make the route answer every method. `methods = ["GET", "POST"]` now answers `GET`, `HEAD` and `POST` only, and a name other than `GET`, `POST`, `PUT`, `PATCH` or `DELETE`, or an empty list, is refused naming the route and the value.
Migration: a route that relied on the widening needs its list to name every method it should answer, or no `methods` key.

**m6-http `--dump-config` builds the route table as start does, so a route table start refuses fails the dump with exit 2 (#337).**
A test fails when the dump path stops building the table.

**A refresh token survives a signing-key rotation, and the login form and the JSON login share one sequence (#332).**
Refresh tokens were checked against the current signing key, so rotating the key logged every user out at their next refresh. A refresh token is now verified by the hash stored with it, and a revoked or unknown token is still refused.

**m6-auth: a corrupt roles column is an error, and an unknown user costs the same password check as a known one.**
An unreadable `roles` value used to read as no roles. It is now `AuthError::Roles` with the user named. A login for a user that does not exist runs one bcrypt check against a fixed hash, so the response time no longer tells an attacker which accounts exist. The three hand-written clock reads use `jwt::now_secs()`.

**CSS minification leaves quoted strings and `url(...)` contents unchanged.**
The minifier removed `/* ... */` and collapsed spaces with no notion of a string, so a `/*` or a run of spaces inside `content: "..."` or `url(...)` was changed. Both are now copied as they are.

**m6-monitor header-check failures reach the digest.**
A failing header check showed on the check page but never in the digest, its page or its history. It now does, one uptime formatter serves the page and the check, and a header-check URL with a trailing slash is trimmed.

**A `no-store` response is sent without hashing its body for an ETag.**
`Response::send` skipped nothing: every response paid a full-body SipHash for an ETag, including those whose `Cache-Control` carries `no-store`, which no cache ever revalidates. The check matches the `no-store` token exactly across every `Cache-Control` line and ignores it inside a quoted string, so a cacheable response keeps its ETag and its 304.

**A multipart upload no longer builds a tokio runtime.**
`parse_upload` built a new single-threaded runtime for every upload. It now drives the parser with `futures_executor::block_on`, and m6-core's `multipart` feature no longer depends on tokio.

**The HTTP/2 paths copy less per request and per frame, and the TLS HTTP/2 path reads the client address once per connection.**
The dispatched request is moved instead of cloned, frame payloads are borrowed instead of copied, and DATA is sent from a slice. The client address on the TLS HTTP/2 path is taken at accept, not looked up and formatted again on every wakeup. The h2c and h2s backbone clients are now one HTTP/2 client generic over its transport.

**Telemetry reads a field by its exact key.**
Looking up `requests` matched inside `total_requests=` and read the wrong number. A field now matches only at the start of a token.

**`m6-bench` and `m6-bench-detail` compute percentiles with the rule `/perf` uses, and the four QUIC clients share one configuration.**

**Tests in m6-http and m6-core wait on a signal with a deadline instead of fixed sleeps.**
Two m6-http negatives keep a short fixed wait, because m6-http emits nothing to wait on for them. m6-core's test suites share one `until` helper.

**Debug and test builds carry line tables only, and no debug information for dependencies, so a full test build is 3.7 GB instead of 12 GB (#303).**
Every file under a crate's `tests/` is its own binary, and each linked every dependency with full debug information. A full check filled about 16 GB, more than a hosted CI runner's free disk, and CI jobs died mid-build. Measured on the build host on one commit with `cargo test --workspace --no-run`: `target/debug` went from 12 GB to 3.7 GB and the build from 411 s to 282 s. Backtraces still show file and line.

**The m6-auth-cli integration tests assert that each setup command succeeds, and print its stderr when one does not (#258).**
A failed setup command used to surface later as an unrelated count assertion.

**m6-auth-server opens one database connection per worker, so login, refresh and logout no longer queue behind a single connection lock (#182).**
A login held that lock while bcrypt verified the password. A worker opens its connection on first use, and a failed open answers 500 and is tried again on the next request.
`m6_core::sqlite::OpenOptions::open` now retries the switch to WAL until the busy timeout passes, because SQLite answers busy for that pragma without honouring the timeout when several processes or threads open a new database at once. `m6_auth::Db::open` uses that opener, so it now refuses a read-only file and turns on `recursive_triggers`.
`m6_auth::AuthError` gains `Open`, so a `match` over it needs a new arm.

**m6-http's benchmark dependencies are no longer production dependencies (#188).**
The cold-start benchmark is now a `[[bench]]` (`cargo bench -p m6-http --bench bench_coldstart -- --addr <host:port> <flags>`, and without `--addr` it prints a note and exits 0), and `tokio`, `tokio-rustls`, `h2` and `http` moved to dev-dependencies. The `m6-bench-coldstart` binary is gone.

**`--dump-config` runs the compresses agreement check that start runs (#255).**
A config that disagrees with `site.toml` about compression now makes `--dump-config` exit 2 with `Config error: <msg>`, as start does, instead of passing validation and failing at start.

**The m6-auth-server reference follows the documentation ruleset, and its `next` check, browser logout, key rotation and `--dump-config` descriptions now match the service (#140).**

**A request takes the framework read lock once and releases it before the handler runs or the template renders, so a hot reload no longer queues requests behind a render (#183).**
The lock used to be taken twice and held through the handler and the render. std's `RwLock` stops granting new readers once a writer is waiting, so a reload made every new request wait for the longest render in flight, and the reload itself dropped the old renderer while holding the write lock. Now the lock covers only copying out the shared state, routing and building the dictionary, and the renderer is an `Arc` the request carries out with it.
A test blocks a request inside the renderer, swaps in a state with another renderer, and asserts the swap completes while the request is blocked and that the request is answered by the renderer it started with. It fails on the old locking and passes on the new.

**Tests now cover that a woff2 response is never compressed and is served byte-exact (#293).**
They fail if `font/woff2` is added to the default compress list, which would re-encode a file that is already compressed.

**Tests now run the built m6-http binary through its startup argument and system-config errors (#294).**
They fail if a change to argument parsing or the config merge turns a clear exit 2 into a start with the wrong config.

**Tests now cover the shutdown signals every service shares (#288).**
They fail if SIGTERM or SIGINT leaves the service running, exits non-zero or leaves the socket behind while a request is in flight.

**Tests now cover params precedence, the query and path-parameter clash, and a route status reaching the response (#290, #291, #292).**
They fail if a global params key beats a route key, if a changed params file is missed after a restart, if a query key replaces a path parameter, or if a route status is dropped.

**Tests now cover the auth path (#295, #296).**
They fail if a public route runs auth code with or without a token or from the cache, or if a browser stops being redirected to `/login?next=<path>` or `/auth/refresh` while an API client gets 401.

**Tests now cover backend pool failover and rejoin after backoff (#297).**
They fail if a failed member is picked again before its backoff ends or is never picked again after it.

**Tests now cover custom error pages from a socket backend (#298).**
They fail if a backend 404 or 500 loses the fetched page or its status, if a failed page fetch is retried, or if the error path fetches itself.

**The build-host sync now keeps `tools/perf-history.jsonl`, and the script brings it back after perfcheck so the committed performance history records every run (#193).**
The sync used to delete the file the build host wrote, so the history never entered git. The sync now sends the committed history to the host, perfcheck appends its record there, and the script replaces the local file with the result only if the copy is complete and no shorter. The clean-tree guard ignores the file, a copy-back failure fails the run without stopping the examples, and `.gitattributes` merges the file by union so two branches that each add a record do not conflict.

**m6-html and m6-file each run one event loop in one thread.** They accepted on one thread and handed each connection to a worker over a locked queue, about 15 µs per request, and a worker held its connection for its whole keep-alive life. Now each service's main accepts, reads, answers and writes with every socket non-blocking: an idle keep-alive connection costs a buffer, not a thread, and a large file streams as the client reads it. Connections beyond what the process can serve wait in the kernel's listen queue: there is no 503 for a full queue. Run several instances to use several cores.

**m6-auth-server runs one event loop with one database connection.** It handed each connection to a worker thread, and each worker opened its own database connection on its first request, answering 500 when that open failed. Now one loop serves every connection, the database is opened once at start, and a database that cannot be opened stops the service with exit 2. The login rate limit is per process.

**m6-auth-server never waits on a database lock.** It waited up to 5 seconds for SQLite's lock, which in a single-threaded service holds every request. It now sets SQLite's busy timeout to zero once started, retries a request that found the database locked every 5 ms from its event loop while serving others, and answers 503 with Retry-After after one second. A retried login does not verify the password twice. m6-auth-cli keeps its 5-second wait.

**m6-monitor runs one thread with its own event loop.** It ran a background poll thread, one thread per node per poll with blocking HTTP requests, a thread per tunnel probe, a history writer thread and a thread per reverse DNS lookup. Each poll is now a set of non-blocking requests, connects and DNS queries driven by its loop, the page, digest and check are built once per poll and served from memory, and history is written in one short transaction per poll, never waiting on a locked database. The reports are unchanged. The monitor trusts the host's certificate store instead of a bundled one.

**/traffic no longer reads the analytics log.** m6-http read up to 24 MiB of its log file inside its event loop to answer /traffic. It now keeps the last 32,768 events it logs in memory, in space allocated at start, and builds the report from those in the last hour. A report after a restart covers only the time since the restart, and window_minutes says how long that is. A node that wraps the ring inside the hour reports the shorter span it holds and counts the events it dropped. A path is stored up to 512 bytes and a user agent up to 256, and a cut value ends in `...`: the log keeps them whole. A request whose path or user agent was cut counts as probe-shaped, so a marker cannot be hidden past the cut.

**A node answering `/health` with 200 and an unrecognised status, or with no usable answer, is now a fault in m6-monitor's digest.**

**One platform layer.** m6 named the operating system, or an OS crate, in every service loop, three thread counters, a Linux-only test, the host readings and the bench tools. They now live in one crate, `m6-platform`, re-exported as `m6_core::platform` and selected by OS family: readiness over mio, the clock, a shutdown or reload request as a descriptor, a child's exit as a descriptor (pidfd on Linux, kqueue on macOS), sockets with the right options, the open-file and socket-path limits, host and process facts. A gate test refuses platform conditionals, OS crates, direct clock reads and service-manager variables anywhere else. One shared contract suite and the echo reference app run unchanged on macOS arm64, Linux x86_64 and Linux arm64, and CI builds and tests all three. A child a service spawned inherited its blocked signals (and on macOS the ignored disposition) and could not be stopped: fixed. The thread-based `ShutdownHandle` is removed. Known limitation: when a process runs out of file descriptors, connections already queued wait until the next new connection arrives.

### Removed

**`ureq`, `native-tls`, `openssl` and their dependents left m6-core, and email is a client the service's own loop drives.** A form POST is `client::Endpoint` over the loop's `Upstream`, and an SMTP send is `email::Smtp` driven by the loop's step, with lettre supplying the message bytes, the envelope, the command sequence and the reply parse. The socket, the descriptor and the deadline are ours, as they are for every other client. A send that a backend refuses answers 500 naming the reply code, and one that never answers answers 504 on the loop's deadline rather than blocking it.

Nine crates left the lockfile: ureq, webpki-roots, native-tls, openssl, openssl-sys, foreign-types 0.3.2, foreign-types-shared 0.1.1 and futures-io. Verified: `cargo tree -i` reports no match for ureq, native-tls or webpki-roots, no source names the removed transports, the allocation test measures 0 in our code around the send with lettre's own allocations recorded as the accepted exception, and the three loop tests fail when the loop is replaced by a blocking call.

m6-http's bundled webpki-roots store, a second store beside the host's.

**App, its builders, AppContext, with_thread_state and `[thread_pool]` are gone: m6-core is not a framework.** Every service writes its own main with its own event loop, and m6-core supplies the parts: h1::ServerConn, site::Site, net::listen_unix, and the event module's Signals, Timeouts, Slots and LoopTrace (see docs/m6-app-anatomy.md). The params cache takes no lock.

### Verified

- Unit tests with stub resolvers cover a confirmed name, a PTR whose forward lookup returns other addresses, a name with no forward answer, a failed forward lookup, no PTR, a name with no address (EAI_NONAME and EAI_NODATA), a slot free by the time the caller has the result, the per-poll lookup cap, expiry and cache bound, a report that reads stored results and resolves nothing, a resolver that never answers, and the thread bound (eight blocked lookups, then fifty more that return at once and start no thread, then slots free again).
- No test uses a live public name.

---

## 1.15.0, 2026-10-29

**m6-http reloads its system config, and TLS on HTTP/1.1, HTTP/2 and HTTP/3, when the process config is touched (#254).**
It keeps its cache while it reloads.
Certificate files are no longer watched, so a renewal must touch the process config after it writes them.
m6-auth-server re-reads its keys on reload and refuses a private key that is not the pair of the public key.
A `site.toml` change clears the cache again once the backends' settle window has passed, and once more after the poll interval.
That clears the stale cached page that 1.14.0 introduced in #244.

Migration is required for anyone who renews certificates or rotates auth keys.
The steps are under Migration.

### Migration

m6-http no longer watches its certificate and key files, and m6-auth-server no longer watches its signing keys.
Anyone renewing certificates must touch the process config after writing them.
A certbot deploy hook that runs `touch /etc/m6/system.toml` is one way to do that.
Until the touch, m6-http keeps serving the previous certificate.
Rotate an m6-auth-server key by writing both files and then touching its config.
After an auth key rotation, touch m6-http's system config as well.
m6-http takes the new public key when either of its files is reloaded, and until it reloads new tokens get 401.
A renewal hook that touches `site.toml` does not reload the certificate and only empties the cache, so touch the process config instead.
`[analytics] enabled`, `[analytics] log_path` and the `[server]` binds now apply only on a restart.
In 1.14.0, turning `[analytics] enabled` off took effect on a `site.toml` reload, and now m6-http keeps the running value and warns.

### Changed

**m6-http watches its system config as well as `site.toml`, on one `ConfigReloader` (#252).**
Before this change m6-http watched `site.toml` and its certificate files and did not watch its system config.
A change there applied at the next `site.toml` reload, which also emptied the response cache.
m6-http now runs the same `ConfigReloader` as every `App`, over both files.

A change to `site.toml` rebuilds routes, pools, the invalidation map, the error mode and the security headers, and clears the cache, as before.
A change to the system config alone takes only the keys that file owns.
It keeps the cache and re-reads TLS on HTTP/1.1, HTTP/2 and HTTP/3, the `[auth]` key, the `[health]` access keys, the node name, `allowed_methods` and `backend_timeout_secs`.
The system config alone never replaces routes.
A refused `site.toml` reload therefore cannot leave the config describing one site while the route table serves another.
A change to both files runs both.

`config reload: complete` now carries `site`, `process`, `cache_cleared` and `elapsed_us`.

The keys m6-http reads only at start keep their running value on a reload and are named.
They are `[server]` `bind`, `h2c_bind`, `internal_binds`, `internal_peers`, `redirect_bind` and `warm_on_start`, and `[analytics]` `enabled` and `log_path`.
The log says `config reload: [server].bind changed; restart m6-http to apply it`, and `/perf` and `--dump-config` describe what is running.

**TLS is read when the system config is touched, and the certificate files are not watched (#252).**
A renewal writes the certificate and the key as two files.
The first event could read a certificate beside the previous key, fail the pair check and wait for another event.
The second watcher, a thread with a self-pipe off Linux, is gone.
`FsWatcher` now watches only the backend socket directory, on Linux.
A renewal writes both files, then touches the system config, and m6-http reads both on the touch.
A touch that arrives before the key is written fails the pair check, keeps the running certificate on every protocol, and is retried after 1 s, doubling to 60 s.
A system config change reloads TLS even when `site.toml` fails to load, so a `site.toml` that is mid-edit does not hold a renewal back.

**A `site.toml` reload empties the cache twice more, 100 ms and about 1 s after it ends (#252).**
A backend that reloads after m6-http can answer a request in the gap with the old page, and m6-http caches it.
m6-http now clears the cache again 100 ms after the reload ends, for a backend that held the change for the 20 ms window, and once more 1.08 s after it ends, for a backend that found the change only on its once-a-second poll.
The delays are `RELOAD_DEBOUNCE` and `RELOAD_POLL` from m6-core plus 80 ms for the backend's own reload.
The window starts when the reload ends, so a slow reload does not use it up.
A second change re-arms both.
It costs one extra cache miss per page about a second after a content change, and nothing on the cache-hit path.
The three cache-hit blocks in `m6-http/src/main.rs` are byte-identical to the previous release.

**m6-auth-server re-reads its signing keys on reload, and m6-monitor names its restart-only keys (#252).**
The key-watching thread in m6-auth-server is gone, and with it the `libc` dependency it alone used.
The service's `on_reload` hook loads the private and public key before it swaps either.
A key that fails to load, or a private key that is not the pair of the public key, leaves the running keys serving and is retried.
A changed `[storage] path`, token lifetime, issuer or rate limit is named in a warning that says to restart.
m6-monitor warns when `poll_interval_s`, `[monitor] history_file` or `history_max_mb` changes, because it reads them once at start.

### Fixed

**A `site.toml` change could leave a stale page in the cache when a backend reloaded after m6-http had cleared it (#254, introduced in 1.14.0 by #244).**
m6-http clears its cache as soon as it sees the change.
A backend under `ConfigReloader` can hold a second report for 20 ms, and on Linux one `Request::touch` produces two reports, so the backend reloaded after the clear.
A request in between cached the old page, and a `Cache-Control: public` page stays cached for up to 24 hours.
The cache is now cleared twice more, as described above.
m6-auth-server also refuses a private key that is not the pair of the public key, so a rotation caught between its two writes keeps the running keys and is retried.

### 1. Verified

- `tls_reload`: the certificate moves on HTTP/1.1, HTTP/2 and HTTP/3 after the touch, flat and through a certbot-style symlink.
  Writing the certificate without a touch changes nothing for 3 s.
  A broken `site.toml` does not hold a certificate back.
  A touch before the key is written lands through the retry.
- `config_reload`: a system config touch keeps the cache and logs `cache_cleared=false`.
  A `site.toml` change clears it twice.
  A bind change is named and not applied.
  A system config change after a refused `site.toml` reload keeps the running routes.
- `metrics_access`: `[health] metrics_peers` in the system config applies on HTTP/1.1, HTTP/2 and HTTP/3 without a restart, and a rotated `[auth]` key verifies after a touch and not before.
- `config_reload` with a real `m6-html` backend that holds the change: the new page is served within 500 ms.
  The second clear comes a full window after a slow reload ends, and a third clear removes a page cached after the second.
- m6-auth-server: a mismatched key pair is refused and the matching pair lands on the retry.
  A rotated signing key is served after the config is touched and not before, and a garbage key leaves the old one serving.
- m6-monitor: a changed `poll_interval_s` is named and the service keeps running.

1.15.0 changes how m6-http and m6-auth-server reload, adds one migration for certificate and auth-key renewal, and clears the stale cache that 1.14.0 could leave.

---

## 1.14.0, 2026-10-27

**1.14.0 adds log reopen on SIGUSR1, declared internal listeners that accept only the peers a config names, token-gated metrics, a monitor history and tunnel health, and new m6-core blocks.**
Those blocks include an opt-in SQLite feature and a config reloader.
It also adds a fleet certificate comparison, the user agents of notable clients, each config key in the file it belongs to, and test binaries built from the source under test.
The TLS reload now replaces the certificate every listener serves, which fixes the limitation 1.13.0 recorded.

### Changed

**m6 services reopen their log file on SIGUSR1 (#234).**
Before this change SIGUSR1 killed every m6 process at the default disposition, and `m6-http` opened its analytics file once and never reopened it, so rotation had to use `copytruncate`, which can lose the lines written between the copy and the truncate.
SIGUSR1 now joins SIGTERM and SIGINT in the signals `m6_core::signal::block()` blocks.
The signal thread handles it before it counts toward a forced exit, so any number of reopen signals never shut a process down.
A process that writes no log file logs one line and carries on.

Rotation renames the file and sends SIGUSR1.
The reopen opens the new file while holding the lock the log writer takes for every line, and the writer takes that lock once per event.
No line is lost or split, no request thread waits, and the path reappearing proves the renamed file is finished.
If the open fails, the process keeps writing to the previous file, logs an error and records the failure.
The path is made absolute once at start, so a later change of working directory cannot redirect a reopen.

`/traffic` also reads the newest rotated file next to the live log, named `<name>-YYYYmmddHH` with or without `.gz`, so its window does not shrink just after an hourly rotation.
`m6_core::log::read_recent` does the read, and `/traffic` runs it on the event loop, so the read is capped at 24 MB.
A plain rotated file is read from a seek, so the cap holds however large the file is.
The rotated file is skipped when the live tail already filled that budget, and when it was last written before the start of the `/traffic` window.
A `.gz` file is decoded only when it is no larger than the budget on disk and the uncompressed size in its trailer is no larger than the budget, and the decoder stops at the budget whatever the trailer says.
A skipped rotated file means the report covers the live file.

**`/perf` and `/traffic` report the last log reopen, and m6-monitor faults when it is too old (#234).**
`logging.reopen` gives `reopens_total`, `failures_total`, `seconds_since_last_reopen`, `last_reopen_unix` and `seconds_since_last_failure`.
`seconds_since_last_reopen` is `null` until the first reopen, which is not the same as zero.
`reopen` is `null` for a process that writes no log file.
`/perf` gains a `logging` block, and a node older than the field still parses.

m6-monitor shows the age for each node in `/check` section A and in `/digest` as `nodes[].log_reopen` (`state`, `age_s`, `last_unix`, `reopens`, `failures`, `last_failure_age_s`, `detail`).
The new `[monitor] log_reopen_max_age_s` is optional and has no default, because analytics logging is on by default and a deployment that does not rotate would otherwise fault two hours after every start.
`/digest` carries it as `thresholds.log_reopen_age_s`.
With it set, these are Faults:

- a last reopen older than the limit
- a node that has not reopened in more than the limit since it started
- a last reopen that failed and has no success after it
- a node too old to report the age

Without it the state is shown and never judged.

A main log that has gone quiet, which section A already printed as a FAULT, is now also a digest finding, so the verdict counts it and `ALL CLEAR` no longer appears above it.

**`/traffic` reports the user agents a notable client presented, and renames the count in m6-core (#221, part of #219).**
`ClientSummary` counted the user agents an address presented and threw the strings away, so a report read `1 UA` and not which one.
Three rotating agents and three that are all `curl/8.4.0` are different findings, and only the strings tell them apart.
`notable[].user_agents` in the `/traffic` payload is new, and it is an array of strings.
It is `serde(default)`, so a payload from a node that predates it still parses and reads as empty.
`notable[].distinct_user_agents` stays beside it as the true count.

Payload change for consumers: a reader of `notable[]` now finds an array under `user_agents`.
On `heavy_hitters[]`, `user_agents` is still the count under that wire name, because an aggregator reads `/traffic` from nodes on other versions.
Only the Rust field was renamed, from `HeavyHitter.user_agents` to `HeavyHitter.distinct_user_agents`, with `serde(rename = "user_agents")`.
A test parses an older payload and checks that the count still goes out under the old name.
A heavy hitter carries the count and no sample, because its only signal is the count, and the loudest source on a node is often the monitor's own polling.

The sample is capped by `m6_core::telemetry::USER_AGENTS_KEPT` (4 agents) and `USER_AGENT_MAX_CHARS` (160 characters).
A longer agent ends in `USER_AGENT_TRUNCATED` (`...`).
The cap exists because one address presented 526 distinct agents, and a client chooses the length of the header.
The sample is sorted before it is cut, so two runs over the same data give the same report.
The cut is by character, because a byte slice can land inside a multi-byte sequence and panic.
m6-monitor prints `agents (4 of 526)` when the sample is partial and `agents:` when it is the whole set.
An empty agent prints as `<none sent>`.
Which protocol a client spoke is not in this change and stays open in #219.

Verified: three new tests in m6-core cover the cap, the determinism and a multi-byte agent, and three in m6-monitor cover the printed block, a partial sample and a missing header.
The multi-byte test panics with byte slicing.
The PR records `cargo test --workspace` at 1172 passed with 0 failed, `tools/clippy.sh` silent and `cargo fmt --check` clean, on a laptop.
It records that nothing in the tree read `heavy_hitters[].user_agents` as an array.

**A listener is internal only when the config says so (#232).**
m6-http used to decide from a listener's bind address whether it was internal.
Three things followed from that guess: the statistics label, whether the h2c listener believed the client address a cache node forwards, and whether the custom error path was served.
`[server] internal_binds` now names the internal listener by address.
m6-http refuses an entry that is not an IP address and port, is not in a local range (`m6_core::net::is_private_ip`), or is not its `h2c_bind`.
Only the h2c listener can be internal.
An entry that equals `bind` is refused, because the TLS and QUIC listener faces clients and stays external.

**Migration.**
A config with `h2c_bind` must add `internal_binds`, or m6-http exits 2 and says what to add.
A listener declared internal must also list its peers in `internal_peers` (#246), or m6-http exits 2 again.
For an h2c listener on a local IP address, write both keys: `internal_binds = ["<h2c_bind>"]` and `internal_peers = ["<ip of each peer allowed to connect>"]`.
`internal_binds = []` treats the listener as public, and then `internal_peers` is not needed.
An h2c listener bound to a host name or a public address can only be declared public.
Add the keys before upgrading, because older versions ignore them.
A `bind` on a private address is now labelled `external` in `/perf`.

Verified: `cargo test --workspace` gives 1297 passed and 0 failed, clippy is silent with `-D warnings`, and the release and test builds have no warnings.
Thirteen new config tests, two rewritten h2c trust tests and four tests that run the real binary cover every refusal, the migration messages and a valid start.
Two of the binary tests send requests over h2c under a rate limit and show that a declared listener believes the forwarded address and a public one does not.
A diff of the three cache-hit blocks in `m6-http/src/main.rs` against the base is empty, so the request path for a cache hit is untouched.
Example 09's integration test in m6-examples ran to "All assertions passed" with no `SKIP` against an earlier build of this change, with its h2c listener declared the same way.

**Each key goes in the file it belongs to, and an app can say where `site.toml` is (#242).**
Every m6 process reads two files.
`site.toml` holds the state every process of the site shares.
The process config holds that process's own state, which means node identity, binds, certificates, credential paths and per-node security.
Three things stopped that rule from holding.

An `App` looked for `site.toml` only at `<site_dir>/site.toml`.
`docs/m6-site-layout.md` gives each app its own root under `apps/`, with `site.toml` at the top of the tree.
In that layout an app read no `site.toml`.
The `[[backend]] compresses` agreement passed without checking, `[log]` fell back to its defaults, and the watch was on a directory that never held the file.
A new reserved config key, `site_toml`, names the file.
A relative value resolves against `site_dir`, an absolute value is used as given, and no key means `<site_dir>/site.toml` as before.
A named file that does not exist, cannot be read or is not valid TOML is exit 2 at start.
A file at the default path keeps its tolerance.
`--dump-config` prints the resolved path.
`Request::touch("site.toml")` touches that file, so the documented edge invalidation reloads the file the app reads.
`AppContext` gains a `site_toml` field, and m6-auth-server reads its issuer fallback from that file.

The system config accepts `[health] metrics_token_file`, a credential path, and it wins over `site.toml` like the other `[health]` keys.
A relative path there resolves against the site directory, as `tls_cert` does.
The same rule applies to `[auth] public_key` in the system config.
A relative `metrics_token_file` in `site.toml` still resolves against the working directory.
The token is read after the two files are merged.

m6-http accepted node keys in `site.toml` and site-wide keys in the system config without a word, because the system config wins and the result worked.
It now logs a warning at start for each key in the wrong file.
The node keys are `[server]` `bind`, `tls_cert`, `tls_key`, `h2c_bind`, `internal_binds`, `internal_peers`, `redirect_bind` and `warm_on_start`, `[auth]`, `[node]`, and `[health]` `metrics_token_file`, `metrics_require` and `metrics_peers`.
They belong in the system config.
The site-wide keys are `[server]` `backend_timeout_secs` and `allowed_methods`.
They belong in `site.toml`.
The warning is silent when both arguments name one file.
The docs list which file each key belongs in.

**Two signatures changed.**
`m6_core::compress::check_declared_support` and `m6_core::log::read_site_log_config` take the path of `site.toml` where they took the site directory.
`AuthConfig::load` in m6-auth-server takes it as a third argument.
The public struct `AppContext` gained a `site_toml` field, so a struct literal of it outside m6-core no longer compiles.
A service reads the context a handler or initialiser is given and does not build one, so this reaches only code that builds an `AppContext` by hand.

Migration: a caller of the two functions or of `AuthConfig::load` passes the `site.toml` path.
Code that builds an `AppContext` by hand adds the field.
A service that does not set `site_toml` behaves as before, and a misplaced key keeps working and is named in a warning.
The system config's relative paths resolve against the site directory, as described above.

**Verified** by unit tests for the key, its resolution and its refusals, by m6-file starting on a per-app root and exiting 2 on the compression disagreement it could only see through `site_toml`, by m6-auth-server taking its issuer from the named file, by m6-core tests for a named `site.toml` that is a directory, unreadable or invalid and for `touch("site.toml")` with a file outside the site directory, and by m6-http tests for the relative key paths, the `[node]` warning, and one table-driven test that every misplaced key warns in the wrong file and not in the right one.

**An `App` reloads when either of its two files changes, and a lost watch no longer stops it (#244, closes #185 and #198).**
An `App` service watched both its config and `site.toml` but rebuilt only from its own config.
`site.toml`'s `[log]` level and its `[[backend]] compresses` declaration were read once at start, so editing the log level there changed nothing (#198).
A watcher that stopped delivering events left a service that never reloaded, because the one-second modification-time check ran only when there was no watcher at all (#185).
State that `init_global` built from the config had no way to reload.

`m6_core::ConfigReloader` now watches both files.
It takes the change from the platform watcher and, whatever the watcher says, compares the two files' identity, size and times once a second.
The first change after quiet is reported at once.
A change that arrives within 20 ms of a report is held and reported once more when the burst has settled, no later than a second after the window opened.
A write that replaces the file in one step is one reload with no delay and a burst of writes is at most two.
A write that truncates the file first can report twice.

A file seen at length 0 is skipped with a warning, and the write that fills it reloads.
Reporting at once matters when an edge clears its cache on the same change, because a backend that reloads late would otherwise leave a stale page cached.
A touch alone reloads.
On Linux an event that names one of the files is a change even when its size and times are equal, which coarse timestamps can cause.
A file that failed to load is not tried again until it is written again.

An App reload now re-reads `site.toml` every time.
It applies the log level with the same precedence as start: the command line, then the service's `[log]`, then `site.toml`'s `[log]`.
It checks the `compresses` agreement again and refuses the reload on a disagreement.
It refuses a `site_toml` that is gone and a `site.toml` that is not valid TOML, whether or not `site_toml` names it.
The previous state keeps serving after any refusal.
It logs `config reload: <key> changed; restart this service to apply it` for `thread_pool.size`, `thread_pool.queue_size`, `server.read_timeout_s`, `server.socket_mode` and `site_toml`, which keep their running values.
`Reload complete` gains `site` and `process` fields saying which file changed.

`AppWithGlobal::on_reload` and `AppWithState::on_reload` register `Fn(&G, &AppContext) -> Result<()>`.
Core calls it with the new config before installing the new state, so a service can re-read the state it built in `init_global`.
If the hook returns `Err`, core logs it with the delay before the next attempt, keeps the previous routes and log level, and tries the reload again after 1 s, then 2 s, 4 s and so on up to 60 s.
A real write to either file reloads at once and starts the delay again at one second.
The hook must be safe to run twice on one config.
A panic in the hook is not caught, because the release profile aborts on panic.
It ends the process and the service manager restarts it, so return `Err` for anything recoverable.

m6-http is not changed by this.
It keeps its own watcher until it moves onto `ConfigReloader`.

**An internal listener accepts only the peers its config names (#246).**
An h2c listener named in `[server] internal_binds` believed the `X-Forwarded-For` of any peer that reached it, so a firewall was the only thing between a forged client address and the rate limiter.
`[server] internal_peers` now lists the IP addresses it accepts, each in a local range.
A connection from any other address is closed at accept.
The log writes a line for the first refused connection from each address, then at most one line per address every 60 seconds with the number of refusals since the last line.
It remembers 256 addresses.
A new address past that takes the place of the remembered address whose last line is oldest, when that line is more than 60 seconds old and no count is owed.
When no remembered address qualifies, refusals from the new address are counted.
Across all addresses it writes at most 32 of those lines in 60 seconds, and refusals past that limit are counted.
The first refusal after the 60 seconds ends writes one summary line with the counts, the number of distinct counted addresses (up to 1024) and the most recent counted address.
One call to accept handles at most 64 connections, so a peer that connects in a loop cannot hold the event loop.
The check runs once per connection, and every request on an admitted connection is served as before.
A peer list with no address of the internal bind's IP family is refused at load.

**Migration.**
A config with a non-empty `internal_binds` must add `internal_peers`, or m6-http exits 2 and says what to add.
The migration note for `internal_binds` under #232 gives both keys together.

**Verified** by unit tests for each refusal and for canonical addresses, and by real-binary tests over h2c: a listed peer is served and believed, an unlisted peer's connection is closed with no response, and a config without the key refuses to start.

**Tests run the binary built from the source they test (#245).**
`m6_core::testkit::binary` returned `target/release/<name>` whatever its age, so a `cargo test` after an edit could spawn a binary from before the edit and report a correct change as broken.
It now asks cargo to build `<name>` in release with workspace features, once per binary name per test process, and returns that binary.
A build that is current costs one cargo call, and a stale one is rebuilt first.
It no longer falls back to a debug binary.
m6-auth-cli's tests use it instead of their own lookup.

**Verified** by a unit test that changes a scratch binary's source between two calls and sees the new output, and by the whole suite.

### Fixed

**A TLS reload now replaces the certificate every listener serves, and the limitation 1.13.0 recorded is fixed (#217, closes #210).**
`handle_tls_reload` used to rebuild the quiche configuration alone.
The rustls configuration behind the HTTP/1.1 and HTTP/2 listener was built once at start and never replaced.
After a renewal wrote new material, HTTP/3 served the new certificate and HTTP/1.1 and HTTP/2 served the old one until the process restarted.
The log said `TLS config reloaded`, which was true of one protocol.
`/perf` also kept reporting the certificate loaded at start, so a renewal was invisible to a monitor.
This is the known limitation the 1.13.0 notes record, and it is fixed.
h1 and h2 now serve the renewed certificate, and all three protocols agree.

`Http11Listener::replace_tls_config` swaps the configuration that future connections are served under, and `terminates_tls` says whether there is one to swap.
Connections already established keep the certificate they handshook with.
A plaintext listener is left alone, because turning a redirect listener into a TLS one on a certificate change would take the redirect off the air.
`load_tls_material` loads without recording, and `TlsMaterial::record` records once the material is installed.
Before, a reload that built the rustls configuration and then failed to build the quiche one reported the new expiry while every protocol still served the old certificate.
`handle_tls_reload` now builds both configurations before it installs either, and on any failure every protocol keeps the material it had.

The macOS, FreeBSD and OpenBSD file watcher emitted `SiteTomlChanged` alone, so a certificate change reached the server on none of those platforms.
The kqueue thread now watches the certificate and key the way it already watched `site.toml`.
The wake carries no filename, so the thread tells a certificate write from a content write by comparing modification times, and a content deploy does not rebuild TLS on every file it writes.
A certificate outside the site directory is still not seen there, and a path replaced by a rename still loses its watch.
Both were already true of `site.toml`.

Verified: `m6-http/tests/tls_reload.rs` starts m6-http on one certificate, replaces it on disk, and reads the leaf back over HTTP/1.1, HTTP/2 and HTTP/3.
It asserts that all three present the new certificate, that they agree on one `notAfter`, and that `/perf` reports the material in use.
With the listener swap and the recording removed, the HTTP/1.1 poll times out after 30 seconds while HTTP/3 has already moved, which is the defect.
The PR records `cargo test --workspace` at 1168 tests with 0 failures, `tools/clippy.sh` silent and `cargo fmt --check` clean, all on a laptop.
Conformance and performance were left to CI and the build host.

### Security

**Four dependencies moved up a patch version to clear seven RustSec advisories (#230).**
The moves are rustls 0.23.45 to 0.23.46, rustls-webpki 0.103.15 to 0.103.16, tokio 1.53.1 to 1.53.3, and webpki-roots 1.0.9 to 1.0.10.
This clears RUSTSEC-2026-0346, -0347, -0348, -0349, -0381, -0382 and -0386, which had turned CI's `cargo deny` job red on every open pull request.
Only `Cargo.lock` changes, and no `Cargo.toml` requirement moved.

Verified: `cargo deny check` passes, and the release build has no warnings.
The workspace tests pass with 0 failures, h3 and QUIC included.

### Added

**Small APIs can now return the rows for a time range as JSON Lines, a page at a time (#225).**
Three additions to m6-core.
`m6_core::net` has `is_private_ip` and `bind_ip`, one definition of a private address.
`m6_core::time_range` reads required `from` and `to` query keys as RFC 3339 UTC times for a half-open range `[from, to)`, with a longest span (31 days unless the service asks for another) and a 400 that says what to fix without repeating what the client sent.
`m6_core::ndjson` gains `Page`, which a service fills with rows until it says `Full`.
The service then sends the key of the next row as a `Cursor`, and the response carries it in a `Link: <...>; rel="next"` header.
`PageLimits` default to 4 MiB and 10,000 rows and cannot exceed 64 MiB, half of the 128 MiB that m6-http will buffer from a backend.
A test in m6-http keeps that true.

Why: a website that serves a log or a report for a period needs the same decisions each time.
Those decisions are the time format, whether the end is included, how long a range may be, how big a response may be, and how a client asks for the rest.
They are now made once, in m6-core.

Verified: `cargo fmt --check` and `cargo clippy --workspace --all-targets -- -D warnings` pass with no warnings, and `cargo test --workspace` passes with 0 failures.
This was run against unit tests only, and no page was served from a running m6-http.

**m6-core has an opt-in `sqlite` feature (#227).**
A service that wants one SQLite file can enable `sqlite` on m6-core and link nothing else, because the feature re-exports `rusqlite` in the prelude.
`m6_core::sqlite::open(path)` opens the file with `journal_mode=DELETE`, a 5 second `busy_timeout`, `foreign_keys` on and `recursive_triggers` on, reads each setting back, and returns an error if one does not take effect.
`OpenOptions` changes the busy timeout, the journal mode and foreign keys.
SQLite cannot leave WAL mode while another connection is using the file, so every connection to a file should use the same journal mode.
Opening a WAL file with the default rollback journal then fails with `SQLITE_BUSY`.
`migrate` applies a numbered list of migrations and keeps the version in `PRAGMA user_version`.
Each migration runs in its own `BEGIN IMMEDIATE` transaction, so connections that open at once apply each one once, and a failing migration rolls back.
A file newer than the migration list is refused with `NewerThanCode`, both before the first migration and again inside each transaction, so a newer process that migrates the file meanwhile is also refused.
`migrate` switches `foreign_keys` off before each migration, because SQLite ignores that pragma inside a transaction, and so a migration can rebuild a table that others point at without losing their rows.
It runs `PRAGMA foreign_key_check` before and after each migration's SQL, inside its transaction, and fails with `ForeignKeyCheck` naming the child and parent table if any (child table, parent table) pair has more dangling references after than before.
A dangling reference already in the file does not block a migration that keeps both table names.
Renaming a table that holds one is refused, so delete the orphan rows in an earlier migration first.
A migration that removes one dangling reference and adds another between the same two tables is not caught.
`foreign_keys` is restored to its earlier setting when `migrate` returns.
`append_only(&conn, table)` adds triggers that refuse UPDATE and DELETE on a named table, and `is_append_only` reports them.
`used_bytes` and `prune_oldest` keep a table that is not append-only under a size cap.
The cap counts the whole file, so `prune_oldest` returns the rows deleted and `cap_met`, which is false when the table was emptied and the file is still over the cap.

A service can now use this one way to open, migrate and guard a file.
m6-auth still opens SQLite itself and moves to it later.
The feature is off by default.
`cargo tree -p m6-http` shows no rusqlite, and `Cargo.lock` gains one line, `"rusqlite"` under m6-core.
Nothing in the workspace turns the feature on, so `cargo test --workspace` never builds it.
CI therefore has a step that builds and tests m6-core with `--all-features`, and `tools/clippy.sh` has a second pass that lints it the same way.
That also covers `email` and `multipart`, which nothing in the workspace turns on either.

Verified: `cargo test -p m6-core --lib --features sqlite` passes 417 tests with 0 failures, including 34 in the `sqlite` module that run against real files.
The doctests pass with the feature on.
`cargo test -p m6-core --features sqlite,testkit` builds with no warnings and passes.
`cargo build --workspace --release`, `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `./tools/clippy.sh` and `cargo test --workspace` all pass with no warnings.
This was run on unit and file-backed tests only, and no request-path code changed.

**m6-monitor reports each node's tunnel round trip and WireGuard handshake age (#226).**
For each node that has a `tunnel` configured, the monitor host times up to three TCP connects to the node's tunnel address and reads the age of that peer's latest WireGuard handshake.
Both appear in `/check` section H, in `/digest` as `nodes[].tunnel` (`rtt_ms`, `rtt_samples_ms`, `probe`, `probe_error`, `handshake`, `handshake_age_s`, `handshake_detail`) and as a tunnel column on the HTML page.
`/digest` also carries `handshake_file_age_s` once for the monitor, and `thresholds.tunnel_handshake_age_s`.
Section H prints the `wg` interface drops from a node's `/perf` `host.net` when that node reports them, and only when at least one node has a tunnel.

The round trip is a TCP connect because ICMP needs `CAP_NET_RAW`, which the monitor's unit does not hold.
A reset from a closed port still measures the path.
One connect that gets through in a poll means the tunnel answered.
The outcome is then `connected`, the round trip is the minimum of the connects that got through, and the sample count shows a partial loss.
Only a poll where no connect gets through is `no_answer`.
The probe of a node has one deadline of `timeout_ms`.
Up to three connects run one after another, each given what is left of that time, and a connect that times out ends the probe.
A dead tunnel therefore costs one `timeout_ms`.
A poll runs the HTTPS requests first and the probe after them, all on `timeout_ms`, so a node that is down can cost up to four `timeout_ms` in one poll.
A reading goes stale when one poll takes longer than twice `poll_interval_s`, so keep `timeout_ms` under half of `poll_interval_s`.

The handshake is read from a file because `wg show` needs `CAP_NET_ADMIN`.
A privileged timer writes the output of `wg show <interface> latest-handshakes` to the file and m6-monitor reads it as data.
That timer must run for the handshake age to be judged, including under `--check` from cron.
`wg show <interface> dump` must not be used because it carries the interface private key.
A handshake time is the last successful one, so the age is the slow signal and the probe is the fast one.

Every tunnel finding is a Fault:

- a probe with no answer
- a peer that never handshook
- a peer absent from the interface
- a handshake older than the limit
- a handshakes file that is missing or unreadable
- a handshakes file older than the limit, which is one monitor-level fault naming the file and saying its collector has probably stopped, and the node handshakes then read as unknown
- a node with a `tunnel` and no `public_key`
- tunnels configured with no `wg_handshakes_file`, which is one monitor-level fault

Findings are computed before the unreachable-node case, so a node whose HTTP poll failed still reports its tunnel.
Probing runs on the background poller, and in `--check` in its own process.
No request handler changed.

Configuration is optional except where a tunnel needs it.
`[monitor]` takes `wg_handshakes_file`, which any node with a `tunnel` needs, and `tunnel_handshake_max_age_s` (default 600).
With a tunnel configured, loading the config fails when `tunnel_handshake_max_age_s` is below the top-level `poll_interval_s` plus `timeout_ms` rounded up to whole seconds plus 180.
The error names all three inputs and the minimum.
The poller and this check read the same top-level `poll_interval_s` through one function, and a `poll_interval_s` under `[monitor]` is not read.
WireGuard rekeys every 120 seconds only while traffic flows, and the probe each poll is that traffic.
The 180 seconds also has to hold the period of the timer that writes the handshakes file.
The monitor does not know that period, so it must fit inside the 180 seconds.

Each `[[monitor.nodes]]` takes `tunnel = { address = "<ip>:<port>", public_key = "<peer public key>" }`, where the address must be an IP literal and a port, and a name or a bad value is a config error.
A node with no tunnel is reported as not configured.

The HTML page escapes node name, role, status, findings and the tunnel column with `m6_core::util::html_escape`.
That function escapes `&`, `<`, `>` and `"`, and the m6-http error page now calls it in place of a private copy.

**`m6-core::wireguard`**, a new module.
`Handshakes::parse_latest_handshakes`, `from_file`, `find` and `PeerHandshake` read the `latest-handshakes` output.
A time of 0 means the peer never completed a handshake, a missing file is `Ok(None)`, and a malformed line is an error naming the line.
`docs/m6-core-reference.md` (component index and module section) and the `m6-monitor` row of Table 7 in `docs/m6-architecture.md` describe both.

Verified: `cargo test -p m6-monitor -p m6-core` passes for the wireguard tests (7), the m6-monitor unit tests (87) and the m6-monitor integration tests that include the loopback tunnel probe, with 0 failures and no warnings.
Not run locally: conformance and the MSRV build, which CI covers.

**m6-monitor warns when the nodes of a fleet serve different certificates (#218, closes #216).**
The monitor judged each node's certificate against a threshold and never compared the nodes with each other.
A node that missed a renewal keeps serving the superseded certificate, which is valid, trusted by every client and expiring sooner than the one installed.
The per-node expiry, the `cert_expiry_days` threshold and the digest all passed on every node, because each node alone was healthy.
The first signal was the stale node refusing connections.

`NodeDigest.cert_not_after_unix` is the leaf's `notAfter`, beside the countdown that was already there.
The countdown is measured from the moment each node was polled, so two nodes serving one certificate differ by the seconds between their polls.
`notAfter` belongs to the certificate, so comparing it across nodes is exact.
Both figures come from one reading, so the fleet comparison and the per-node threshold cannot describe different certificates.
When the nodes that reported a certificate disagree, the monitor warns and names every node and what it serves.
It is checked and reported the way version and build drift already are.
Only nodes that reported a certificate are compared, because a node that is silent about its chain already warns on its own.
A renewal moves `notAfter` forward, so a node that missed one disagrees on the day of the renewal, which is the day there is time to fix it.

The `tls_reading` test helper left `not_after_unix` at 0 on every entry while it set `expires_in_seconds` from its argument.
It now derives both from one base.

Verified: five new tests.
With the comparison disabled, the two that assert the finding fail and the three negative ones stay green.
The PR records `cargo test -p m6-monitor` at 48 tests with 0 failures, `tools/clippy.sh` silent and `cargo fmt --check` clean.
The change is contained to m6-monitor, so the full workspace suite was left to CI.

**`/perf` and `/traffic` for listed peers only, with a token (#237).**
`[health] metrics_peers` lists the IP addresses allowed to read `/perf` and `/traffic`.
Each entry must be in a local range (`m6_core::net::is_private_ip`), and names and ranges are refused.
The peer address is the one the TCP or QUIC handshake proved, so the endpoints are refused on an internal h2c listener, which believes `X-Forwarded-For`.
Any other peer gets 404.
`[health] metrics_require` takes `role:<name>` or `group:<name>` and checks a token with `[auth] public_key`.
The token comes from the Authorization header or the session cookie, as route `require` reads it.
No or a bad token gets 401 and the wrong claim gets 403.
`metrics_token_file` stays as the legacy gate and is ignored when `metrics_require` is set.
`[auth] backend` is optional, and `[auth]` and the two new keys may be set in the system config.
The proxy's own 401 from route `require` now carries `WWW-Authenticate: Bearer`.
m6-monitor's `metrics_over_tunnel` fetches `/perf` and `/traffic` through a node's `tunnel.address` while `/health` stays on the public URL.

**Migration.**
A system config that holds only `[server]` and `[node]` is unaffected.
A system config that carries `[auth]` or `[health]` used to have them ignored with a warning.
Now `[auth]` is read, and `[health]` there accepts `metrics_require`, `metrics_peers` and `metrics_token_file`.
A config that breaks the rule below stops m6-http starting with exit 2.
On a running node a reload fails the same way and keeps the old config until a restart.
Check each system config for `[auth]` declared in both the system config and `site.toml` before the upgrade.
Keep it in one file and delete it from the other.
A `[health]` key in the system config other than `metrics_require`, `metrics_peers` and `metrics_token_file` stops m6-http starting with exit 2.
Move any other key (`enabled`, `path`, `perf_path`, `traffic_path`, `traffic_cache_s`) to `site.toml`.

When both arguments name the same file, its `[auth]` and `[health]` are the site config's and the system-side copy is ignored.
Older versions ignore the new keys, so a config can carry them before the upgrade, but the endpoints stay on the legacy gate until it.

Verified: `cargo test --workspace -- --test-threads=1` passes 1388 tests with 0 failures and 0 warnings on the release and test builds.
`cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `tools/clippy.sh` and `cargo deny check` are silent.
`m6-http/tests/metrics_access.rs` runs the real binary and covers h1, h2 and h3.
The three `// end cache hit` blocks in `m6-http/src/main.rs` are byte-identical to `develop` (735 lines, empty diff), and the only functions touched in that file are `handle_request_inner` and a new helper beside the error path.
`cache_hit` measures 47.0 ns and `full_cache_hit_path` 45.6 ns on a loaded laptop (informational).

**m6-monitor keeps a size-capped history of every poll and of crawler sightings, and serves it as paged JSON Lines (#238).**
A website that runs a fleet monitor can now ask what a node looked like an hour ago or last week.
Setting `[monitor] history_file` writes every poll to one SQLite file, and `history_max_mb` (default 256, range 1 to 1048576) caps it.
Without `history_file` nothing is written, `/history` and `/crawlers` answer 503 and section I of `/check` says "off".
A bad key, or a file written by a newer release, stops the start with exit status 2.

Each poll is one transaction holding the poll's verdict and the text of every finding, one row per node and one row per channel and interface.
A node row holds reachability, the node's own verdict, the build, uptime, request, cache and backend error counters, hit and miss percentiles with their sample counts, the host figures, the tunnel figures and the log reopen state.
Once per node per `/traffic` window the history also keeps the response status counts and the crawler sightings.
A window stores at most 100 sightings, the ones with the most requests, and cuts each user agent to 256 bytes.
Agents that are equal after the cut share one sighting, and `sightings_dropped` on each `/crawlers` line says how many were left out.
A node's window is stored again only after `window_minutes` have passed, so the stored windows tile time without repeating a sighting about 120 times.
Counters are stored as the node reports them, and the hit rate is computed on output.
An absent figure is `null`, never 0.

When the file's used bytes pass the cap, the oldest polls are deleted 256 at a time until the file is 5% under it, and every row of a poll goes with it.
Each batch also deletes the user agents that no remaining sighting names, because a visitor chooses that text and could otherwise grow the file without bound.
The names of nodes, channels, interfaces and builds come from the fleet, stay in append-only tables and are never pruned.
There is no time expiry and no vacuum, so the file stops growing near the cap.
This uses `m6_core::sqlite::used_bytes`.
The poller hands each poll to a writer thread through a queue of 8 and never waits.
A full queue drops the poll and counts it, and a failed write adds one Warn on node `monitor`.
A prune that fails is the state `prune_failed`, and a cap that deleting every earlier poll cannot meet is `over_cap`.
Each has its own Warn and a line in section I of `/check`, and the poll is still stored and counted as written.
On Unix the directory is created with mode 0700 and the file with mode 0600, and an existing file is set to 0600.
Measured on 6 channels and one interface, a node row costs 639 bytes with its channels and interface rows.

`GET /history` and `GET /crawlers` take `from` and `to` (RFC 3339 UTC, half open, at most 31 days) and an optional `node`.
They answer `application/x-ndjson` with `Cache-Control: no-store`, one page of at most 4 MiB and 10,000 rows, and a `Link: rel="next"` header while more rows match.
Both answer on the unix socket only.
The monitor has no listener and no token code, so a website puts an m6-http in front of the socket and protects the routes with `require`.
The reader opens its own read-only connection and reads a whole page inside one read transaction, so a prune cannot leave a row without its channels, interfaces or findings.
It reads 256 rows at a time with their children, found by the exact ids of the chunk, and stops when the page is full, so a 4 MiB page reads about the 1,500 to 4,000 rows it holds.
The file uses a rollback journal, so the writer waits for the transaction to end, up to its 5 second busy timeout.
m6-http links no SQLite.

Three m6-core blocks became public for this, so the monitor reads its `node` parameter and refuses it exactly as core reads `from`, `to` and `cursor`.
`m6_core::request::single_query_value` returns the one raw value of a query key, or `RepeatedKey`.
`Response::refusal` is the plain-text 400 with `no-store` and `nosniff` that `TimeRangeError` and `CursorError` already return.
`m6_core::sqlite::open_reader` opens an existing file read-only with the 5 second busy timeout, read back.

Why not the periodic stats log: it lives in each node's journal, which is capped, it has no channels, reachability, host figures or tunnel, and it disappears while a node is down, which is when a history is wanted.
Channel counters include the monitor's own polls and header checks (#107), and `monitor_requests` says how many.

Verified: unit tests cover the schema, the transaction, the cap, the crawler tiling, the cursor and the exact keys of both line shapes.
They also cover unique long user agents across windows staying under the cap, the per-window caps, one snapshot per page with a prune tried between statements, a page that fills in the middle of a chunk, the plans of the child queries, the two prune states and the file modes.
Integration tests run the real binary with an unreachable node.
They check both endpoints on the socket, the 400 and 503 answers, a failed open, a file newer than the binary, a clean shutdown and that the process listens on no TCP port.

### 1. Docs

**README: five passages that explained why the README says what it says were cut (#215, closes #214).**
Each passage explained its own reasoning or assessed the README, or stated a point twice by contrasting it with its opposite.
The paragraph on withdrawn benchmarks keeps what a reader needs, which is that figures published before September 2026 are withdrawn and why they could not be trusted.
F044 now reads "Not fixed. Safe for the audited configuration", which also stops a generic server's README referring to a deployment.
No figure, score or claim changed, and the audit result and the conformance counts are as they were.

Verified by reading the merged diff, which changes only the README.
No code changed and no tests were run.

**Two contributor rules in CLAUDE.md corrected (#223).**
The rule that a change to layout, copy or rendering needs individual approval now applies only to changes a visitor can reach.
A page no visitor can reach, such as m6-monitor's own page on its private socket, does not need that approval.
The rule that every commit ends with a fixed `Co-Authored-By` line is removed, because the line named one model version and went stale each time the model changed.

The first rule, read literally, asked for sign-off on every page m6 renders, including operator pages that only the operator's own tools can open.
The second told contributors to write a trailer that was wrong as soon as it was written.

Verified by reading the diff: it touches only `CLAUDE.md`, replacing one bullet and deleting one paragraph with its code block.
No code and no other document changes, and no tests were run.

1.14.0 fixes the TLS reload limitation that 1.13.0 recorded, and it clears seven RustSec advisories by moving four dependencies up a patch version.

---

## 1.13.0, 2026-10-02

**A server that terminates TLS now says how long its certificate has left.**

Certificate renewal is done by an *ACME* (Automatic Certificate Management Environment) client outside m6, on a timer.
The documented failure mode of that arrangement is silence.
The renewal fails, the timer keeps firing and keeps failing, nothing looks, and the first signal is the server refusing connections on the day the certificate expires.
A server already holds the answer, so it is the cheapest place to ask.

### Added

**`m6-core::tls`**, a new module.
`not_after_unix(der)` reads `notAfter` out of a DER certificate.
`record_loaded(chain)` stores what a server loaded and returns what it could not read.
`loaded()` reports the chain as `TlsCertificate`, leaf first, each carrying `depth`, `not_after_unix` and `expires_in_seconds`.
`unreadable()` counts what failed.

**`PerfReport.tls` and `PerfReport.tls_unreadable`** on `/perf`, both `serde(default)`, so an older node stays parseable to an aggregator and the whole payload stays readable.

**m6-monitor prints days remaining** on the per-node line beside the version and build, and warns below `Thresholds::cert_expiry_days`, default 21.
That default tracks renewal attempts.
A client on a 90-day certificate starts renewing at 30 days and retries daily, so 21 means renewal has failed about nine times before anyone is told.
A threshold inside the renewal window fires on the first transient failure and gets ignored, and an ignored warning is the same as no warning.

### 1. It reports the loaded material

A path on disk and the certificate a running process is serving are different facts.
A renewal that wrote a new file without reloading the server is exactly where they disagree.
`loaded()` opens nothing, so polling a monitoring endpoint cannot be made to do disk work.

### 2. The parse is written out

`rustls` parses no validity from `CertificateDer` and `rustls-webpki` keeps its own parse private, so one integer field meant either eight crates of general ASN.1 or a walk over a known shape.
`notAfter` sits at a fixed structural position in every X.509 certificate, so the walk steps over six elements by length and reads the seventh.
Every length is checked against the buffer, and a test runs every truncation of a real certificate through it.
Dates go through `chrono`, because civil-from-days arithmetic is the mistake `util::iso_date_from` already records.

### 3. Absence and a measurement must not look alike

Three defects found in review, all in the direction of reading silence as health.

| defect | consequence |
|---|---|
| `depth` derived from position after dropping failures | a chain whose leaf could not be read reported its intermediate as `depth: 0`, which every document calls the certificate an ACME client renews |
| `min()` over a partially-read chain | an unreadable leaf plus a multi-year intermediate gave a long number, every threshold passed, and the digest said ALL CLEAR while nothing knew when the served leaf expired |
| gating the check on an `https://` poll URL | a node polled over the backbone in plaintext serves TLS publicly and reports its chain, and every arm was skipped |

**Table 1: what review caught before release, and what each would have cost.**
Take from it that a check written to notice an absence can itself read absence as health.

### 4. Known limitation

`handle_tls_reload` rebuilds the quiche configuration alone, so after a renewal HTTP/3 serves the new certificate while rustls keeps the old one, and nothing re-records.
The number reported is therefore correct for HTTP/1.1 and HTTP/2 and stale for HTTP/3.
That is a server defect and is **m6 #210**.
Earlier drafts of this module's documentation claimed a reload re-records.
No caller does, and the documents now say so.

### 5. Verified

`cargo test --workspace` with the new tests, clippy silent, zero compiler warnings, `cargo fmt` clean, the examples workspace building with clippy silent, and CI green on h1, h2 and h3 conformance, cargo-deny and the MSRV check.
The parse is tested against rcgen-generated certificates.
The tests cover a future expiry read back exactly, an expired one reading negative, a v1 certificate with no version field, an impossible date, DER's forbidden indefinite length, and every truncation of a real certificate.

1.13.0 adds TLS certificate expiry reporting, and it records one known limitation that 1.14.0 later fixes.

---

## 1.12.1, 2026-10-02

**A documentation and comment release.**
No behaviour changes to any shipped binary.

RULE ZERO says nothing about one particular deployment belongs in this repository, in code, tests, comments, docs or the changelog.
This release removes what was still there.
It also corrects three comments that a previous pass made wrong while removing other things.

### Removed

One deployment's node names, in both source and prose.
A credentials file path and the DNS provider it belonged to, used as a worked example.
A hosting provider, named in the benchmark host table with the machine's specification, in a deployment walkthrough and in a README pointer.
A visitor's address and session identifier, carried in a test fixture captured from a live log.
A firewall rule copied verbatim from a running ruleset, counters included.
An ssh port, which appeared only as a worked example and therefore only ever recorded that its author deviates from the default.
Twenty-odd comments asserting measurements taken against one private instance, which no reader can reproduce, so they are assertions.

Addresses that had to stay addresses, such as a client address in a log fixture, are now RFC 5737 documentation ranges.

### Changed

**One host, everywhere.**
`user@example.com` is the only ssh and rsync target, replacing five competing conventions.
`example.com` and its subdomains are the only http, https, h2c and h2s hosts.

**"WireGuard" is now "private backbone",** in 16 places across 9 files.
The transport between nodes is the operator's choice.
m6 requires only that the listener is on a private address.

**`M6_BUILD_ROOT` defaults to `m6-build`,** relative to the login directory, so a normal login can write it.

### Fixed

Three comments that an earlier pass rewrote into something incorrect.
An incorrect comment is worse than the unverifiable claim it replaced.

- The 0-RTT comment credited QUIC early data for the cache-to-origin hop.
  That hop is h2c over TCP.
  The setting configures the listener that serves visitors.
- Two comments claimed a table was reproducible with one `curl -H`.
  The column described is what the replaced implementation produced, so following the instruction contradicts the table.
- A dated narrative kept its date and swapped only its address, leaving a comment asserting that a documentation address made a real request on a real day.

### 1. Verified

`cargo test --workspace` 1150 passed and 0 failed, clippy silent, zero compiler warnings, `cargo fmt` clean, `shellcheck -S warning` clean, and the examples workspace builds with clippy silent against this tree.
CI passed h1, h2 and h3 conformance, the examples suite, cargo-deny and the MSRV check.

### 2. What this release does not do

**The history is untouched.**
Every string removed here remains reachable in any clone, and removing it from the working tree does not retract it.
That is a separate decision about a published repository.

**The default is still wrong.**
The full checks still want a remote Linux host, which is #207, and is why a contributor's connection settings were in this repository to be removed in the first place.

1.12.1 removes deployment-specific details and corrects three comments, and it changes no shipped behaviour.

---

## 1.12.0, 2026-09-27

**m6 gained a release profile.**
Every release before this one used cargo's defaults.
That default is a real optimised build, opt-level 3 with debug assertions off, and it left *LTO* (link-time optimisation) off, `codegen-units` at 16, symbols unstripped and overflow checks disabled.

Turning overflow checks and LTO on made the binaries smaller and the hot paths faster at the same time.

### Changed

**`[profile.release]` in `Cargo.toml` now sets `lto = "fat"`, `codegen-units = 1`, `strip = "symbols"`, `panic = "abort"` and `overflow-checks = true`.**

`cargo install --git` uses the installed crate's own profile, so a deployment cannot supply these settings from outside.
That is why the missing profile went unnoticed.
A deployment repository carried the full profile for its own binaries, and every m6 binary it installed carried none.

| binary | before | after |
|---|---|---|
| m6-http | 11.14 MB | **7.08 MB, -36.5%** |
| m6-md | 6.59 MB | 3.85 MB, -41.6% |
| m6-monitor | 33.11 MB | 25.17 MB, -24.0% |
| m6-file | 30.08 MB | 23.20 MB, -22.9% |
| m6-html | 30.00 MB | 23.16 MB, -22.8% |
| m6-auth-server | 33.46 MB | 25.94 MB, -22.5% |

**Table 1: release binary sizes before and after the release profile.** Take from it that every service shrank, and m6-http shrank the most.

Binary size is a latency property here.
A node runs five m6 services at once, and 30 MB of text per service is resident memory and instruction cache the request path competes with.
m6-http, which is in every request path, lost 36.5%.

| recorded path | before | after |
|---|---|---|
| `render:minimal` | 201827 ns | **174101 ns, -13.7%** |
| `render:blog-index` | 1784823 ns | **1454253 ns, -18.5%** |

**Table 2: the two recorded paths, before and after the profile.** Take from it that the profile shortened both.
Both figures are medians over six runs on a quiet 2-core Linux build host, and both recorded minimums in `tools/perf-baseline.txt` moved down with the argument beside them.

**Overflow checks are on in release**, at a few percent that the LTO gain more than covers.
The F-005 entry in this file is the reason.
An external report described the HTTP/2 flow-control arithmetic as "wraps and stalls", and its proof of concept panicked instead, because an unchecked `+=` on an `i32` is checked in a debug build and unchecked in a release build.
Every integer a peer can influence in this workspace sits in a hand-written protocol parser: window sizes, frame lengths, header list sizes, stream identifiers.
A wrap there is a security defect that presents as a stall.
An abort is one visible connection failure.

### Fixed

**`check.sh` said "All checks passed." for a run that tested neither HTTP/2 nor HTTP/3.**
`tools/conformance.sh` prints "INCOMPLETE, NOT TESTED" and "do not read it as a pass", then exits 0, because with `--allow-missing-tools` nothing regressed: it never ran.
`check.sh` tested only the exit code.
Its own comment claimed the summary "refuses to say pass", which it did not.
The run now ends on "Checks INCOMPLETE: conformance did not run in full. This run is not a gate."

**The benchmark comparison aged silently.**
`target/criterion` is untracked and `cargo clean` erases it.
One baseline was six months old while `check.sh`'s header told a reader to investigate any change over 30%, and a documentation-only change printed +562% against it.
The baseline's age is printed on every run, and past 14 days the comparison is dropped.

**`tools/perfcheck.sh` took the minimum while its own header said median.**
That paragraph had claimed "WHY MEDIAN OF SEVERAL ROUNDS" since it was written.
The minimum is the best reading a machine ever managed, and no request costs that.
It takes the median now, `--runs N` sets the round count, and every reading is kept.

**`tools/build-host-tests.sh` reported the wrong disk.**
Its debug-cache check looked at a hardcoded path, so a run with `M6_BUILD_ROOT` set elsewhere reported "0 tree(s)" while gigabytes sat in the tree it had just built.
Its arithmetic summed `du -sh` strings like "9.9G" and then discarded the total.

### Added

**`tools/perf-history.jsonl`**, one JSON record per performance run, in git.
A record holds the commit, the toolchain, the CPU model, physical core count, OS, architecture, total memory, the load average before and after, and per target the median, every raw reading, the minimum, the maximum and the spread.
A number without the conditions it was taken under cannot be compared with anything later.
One record showed a 40.9% spread on an idle machine, which is why a single reading settles nothing.

The file carries no hostname, address or domain.

**`docs/m6-testing.md`**, rewritten: what checks m6, the commands, the shared test harness, and the h2spec and h3spec procedure with its five traps.
Four of those traps report a conformance failure that is not there, and two of them were not written down anywhere before.

**A documentation ruleset, R0 to R10**, stated in `CLAUDE.md` so a contributor who cannot read a private skill still has it.
`docs/m6-site-layout.md` and `docs/m6-file.md` are written to it, the latter having been missing entirely.

### 1. Verified

`tools/build-host-tests.sh` on a Linux build host, with the profile in place.
1146 tests passed and 0 failed, zero warnings in release and test builds, clippy silent, cargo-deny clean, h1spec 32/32 on all four targets, h2spec 146/146, h3spec 47/49, TLS resumption green on all three protocols, performance within margin, the examples 58 passed with every config parsing, and example 05's end-to-end suite 98 passed over the running stack.

---

## 1.11.3, 2026-09-26

**m6 is a generic web hosting engine, and this release is what makes the repository say so.**
Rule Zero now sits first in `CLAUDE.md`, above the branch model.
m6 must not in any way be tied to one particular site.

This repository is public.
One instance of m6 is its author's own site, in a separate private repository, and that instance is a user of this software and nothing more.

### Removed

**Four working documents that belonged to one deployment.**
`HANDOVER.md` and `docs/CONSOLIDATION-TODO.md` stated which of that deployment's machines ran which version at which artefact md5, the order they were deployed in, and what was deployed as against merely released.
Two session-record files went with them, one carrying a server's public IP address in older revisions.
They are preserved in full, outside this repository, by the deployment that owns them.

**`m6-core/tests/generic_system.rs`, and the reason is the interesting part.**
That test existed to keep one deployment's identity out of this repository, and it could not do the job.
A banned list has to spell what it bans, so the file published two domains and a private repository's name, split into string fragments purely so the test would not match its own source.
Splitting the strings hides them from a grep.
A reader still sees them plainly.
It also only ever read files, so it had nothing to say about the issue tracker, where the same material had accumulated while the test passed on every run.

The rule it was standing in for is care and judgement, and it is written down instead.

### Changed

**Node names became role names** throughout the source, the tests and the docs: `origin`, `edge-a`, `edge-b`.
113 of them, in test fixtures and in provenance comments of the form "real output from <node>".

**Artefact hashes and deployment state are out of the changelog, the lessons and the HTTP/2 plan.**
A release note says what changed in a version of m6.
It does not say whose machines are running it.

**`SECURITY.md` takes GitHub private vulnerability reporting only.**
It also linked a contact form, which was a link to one particular site, so it published which site the maintainer runs.

**`CONTRIBUTING.md` describes m6 by what it is.**
It no longer calls m6 "a small HTTP stack written for one site".
It asks that a bug report be reproducible against something the reader can run.
A measurement against a private production instance cannot be re-run by anyone else, so it is an assertion.
`m6-examples`, example 05's end-to-end suite, and h2spec or h3spec against a loopback instance all serve as reproducible evidence.

### Fixed

**The release gate checked a document that is no longer here, so it now checks the ones that are.**
It required `HANDOVER.md` to mention the version being released.
That file has left, and a workflow cannot read a private repository, but the rule did not leave with it.
A release is a claim about what is now true, so the documents that state what is true are part of it.
m6's own docs go stale in the same way.
A sweep for this release found `docs/H2-PLAN.md` leading with h3spec 37/49 thirteen days after it became 47/49, `CONTRIBUTING.md` describing a clippy ceiling removed on 2026-09-13, `README.md` saying a decision "gates a 1.0 release" twelve days after 1.0 was cut, and `CONTRIBUTING.md` telling contributors that `cargo fmt` is not run over this tree while CI has run `cargo fmt --all --check` since the day it was formatted.

Freshness is judgement, so it is a release checklist in the pull request template, at the moment the author knows the answer.
That is all the previous check ever did: it never verified the document was true, only that someone opened it.

A mechanical "no document names a script that does not exist" check was written for CI and discarded after being measured.
It found 87 findings, roughly 80 of them legitimate, because m6's docs reference a deployment's scripts, the examples repository's, and scripts a user writes in their own project.
A gate at that signal-to-noise gets muted, which is how the two real errors in this repository's shell survived for months.

### 1. Verified

Build host: 1146 passed, 0 failed, 0 warnings on release and test builds, clippy ok, conformance h1+h2+h3 ok, performance ok.
1148 before, less the two tests in the deleted file.
CI green on all six jobs.

Nothing in this release changes behaviour.
There is nothing in it for a server to run.

---

## 1.11.2, 2026-09-26

**Documentation, one CI gate and two tests.**
No library or binary change, so there is nothing in it for a node to run, and a deployment has no reason to move to it.
Whether to pin a release is the deployment's decision, and a documentation release is normally not worth pinning.

It is a release, and not a commit on `develop`, for the reason 1.11.1 exists.
`main` takes releases only, and `main` is what a fresh clone gets.
Everything below is text a person reads while doing something, and the reader who most needs it is the one who has just cloned the repository.

### Fixed

**Both of the pre-push hook's tag refusals told you to run a script deleted twelve days earlier.**
`./tools/release.sh` went on 2026-09-14 with `tools/merge.sh`, when a pull request became the only way into `develop` and the only way from `develop` to `main`.
The hook's own header records that deletion ten lines above the first refusal, and its `main` guidance correctly says `./tag.sh`.
One short file disagreed with itself.

The logic was right throughout.
What was wrong is the only part anyone reads.
Nobody reads a hook, they read its refusal, and these two fire while somebody is cutting a release and is least inclined to stop and check whether the remedy exists.
Following it gets `no such file or directory`, from the tool whose whole job is to be believed.

The second refusal was worse than merely wrong.
It named `tools/release.sh` as the thing that sets `M6_RELEASE`, when `./tag.sh` is what sets it.
It misdirected on the single fact it exists to convey.
Both now give the real sequence, and the second says what `./tag.sh` checks, which is the argument for using it.

**The same deleted script was in two more places, and all four are read at the same moment.**
`.github/pull_request_template.md` put it in front of the author of every pull request.
`Cargo.toml`'s own comment, beside the version line, told you to run it after bumping, which is the first step of a release.
That is the shape: every one of these sites is read while cutting a release and at no other time, so being wrong costs nothing until it costs something.
Issue #118.

**`.githooks/pre-push` had never been checked by anything.**
CI's shellcheck step found `*.sh`, and a git hook does not get an extension, so the one shell file in this repository that runs on every developer's machine was the one file excluded.
It had an unused-variable warning sitting in it.
A gate that selects its inputs by file extension misses exactly the files that never get one.
The `find` now includes `.githooks`: 11 scripts, clean at warning level.

**Four documents stated things that had stopped being true.**

- `CONTRIBUTING.md` said clippy is held "at or under its recorded count".
  Clippy has been `-D warnings` with no ceiling since 2026-09-13, and the ceiling was removed precisely because a number in a file reads as an allowance.
- The pull request template said the same thing, to every author.
- `README.md` closed its structural-question section with "this decision gates a 1.0 release", twelve days after 1.0 was cut.
  The question was answered by taking the second option and building it out: h2spec 146/146, h3spec 47/49, both with floors CI enforces.
- `docs/H2-PLAN.md` led with h3spec **37/49** and "all 12 remaining failures inside quiche".
  It has been 47/49 since 2026-09-13, and the two that remain are QPACK stream errors that are a deliberate upstream decision.
  That is the status line of the document named for any HTTP/2 or HTTP/3 work.

### Added

**The hook's refusals are tested, because text a program prints is program behaviour.**

- `every_script_offered_as_a_remedy_exists` walks the hook's non-comment lines and the whole pull request template and fails if a path named there is not on disk.
  Comments are exempt, deliberately, so the header can go on recording what was deleted and why.
- `a_tag_refusal_names_tag_sh` asserts that the refusal names `./tag.sh`.
  Ruling out deleted scripts falls short of naming the right one.

Both were watched red against the old text before being trusted, each half separately.

### 1. Verified

Build host, against the tree: **1148 passed, 0 failed**, 0 warnings on release and test builds, clippy ok, conformance h1+h2+h3 ok, performance ok.
1146 before, so the two new tests are the difference.
CI on the pull request: build/tests/clippy, conformance, examples, MSRV, cargo-deny and shell scripts all pass.

Recorded as lesson 53.

---

## 1.11.1, 2026-09-25

A documentation release, and the reason it had to be a release is the lesson.

### Fixed

**A working document on `main` stated the previous release as current, for hours.**
It named a version that had been superseded earlier the same day, in its first bullet, on the default branch, in the file a cold reader opens first.

The correction existed on `develop` immediately and could not reach `main`.
A docs-only `develop` to `main` pull request fails the "version is not already tagged" check, because `v1.11.0` existed.
So the stale text was stuck on the default branch until some later release happened to carry it, which is why this release exists.

### Changed

**A release is now gated on the documentation, and on the changelog.**
Owner's rule: docs being up to date is a fundamental part of a release.
A pull request into `main` carries a second requirement beside the changelog entry, and the job says both halves out loud: the release says what changed and the docs say what is true.
`CONTRIBUTING.md` states it beside the other gates.

1.11.2 repointed this at m6's own documents.
The file it originally checked was a working document belonging to one deployment, and it is no longer in this repository.

The check is mechanical on purpose.
Nothing inside this repository can know what any deployment is running.
What it can require is that the document is opened at the moment the author knows what is true.
A cleverer check would be one that cannot fail honestly, and this repository's own trap list is mostly those.

### 1. Nothing for a node to run

No library or binary change, so a deployment has no reason to move to it.

This release is the first thing the new gate ran against, and it passed only because the document it checked named the version.

---

## 1.11.0, 2026-09-25

**`/perf` reports what is running, and TLS session resumption is measured and gated.**

### Changed

**`/perf` reports what is running: name, version and build hash.**
Issue #105.
`PerfReport`'s `version: String` becomes `build: BuildId { name, version, hash }`.
The version is unchanged in meaning and moved inside `BuildId`, and it is not duplicated beside it.
`serde(default)`, so a node older than this parses to an empty `BuildId` and the monitor reads it as unknown, and not as agreement.

A version string cannot tell two builds of one tag apart.
Rust is not byte-reproducible, so `v1.10.0` built twice gives different binaries both reporting `1.10.0`.
On 2026-09-20 staging ran one build of `v1.10.0` and production another, and every reading an operator had said the fleet agreed, and it took `md5sum` on four machines to find.
Each part of the identity answers a different question.
`name` says which binary, because a node runs several and "the node is on 1.10.0" has never been one fact.
`version` says which release it claims to be.
`hash` says which build it actually is.

`/health` is unchanged and still publishes exactly `status` and `node`.
A bare hash was proposed there first and rejected.
It is defensible on disclosure grounds, since a hash names one build and not a release and cannot be turned back into one when the build is not reproducible and no binaries are published.
An opaque number on its own could be a hash of anything.
An identity is a hash keyed to a name and a version, and those already live on `/perf`.

The field is `hash` and not `md5` because the algorithm is how the value is produced, and not what it means.
Naming the field after the algorithm would make changing it a wire break.
It is md5 today, because a deployment's own tooling already compares that number.

**No existing tool breaks.**
`serde(default)` on both sides means a new monitor against an old node reads "unknown", and an old monitor against a new node reads "unknown" too, and neither fails to parse.
Nothing else consumed the field.
The deployment's `ops.sh` takes the version from `m6-http --version`, and `perf-report.py` never read it.
The only cost of a mixed fleet is a few minutes of the monitor saying "unknown" for nodes it has not caught up with.

This was verified three ways.
The build hash equals `md5sum` of the running executable, asserted against a hash taken in the test and not against a constant.
The binary name comes from the executable and not from the library's `CARGO_PKG_NAME`, which would label every service `m6-core`.
A payload with the field removed still parses.
The value is read once at first use and cached, because a deploy replaces the file while the process keeps serving the bytes it started with.

**`m6-monitor` reports build drift.**
Section C reads `m6-http 1.2.3 build 0123456789ab` per node, and a new fleet finding fires when the versions agree and the hashes do not, which is exactly the case a version comparison cannot see.
It is reported separately from version drift and only when versions match.
Different releases have different binaries by construction, so naming that as a second fault sends an operator to two places for one problem.
Both new guards were watched go red against the unfixed code.

### Added

**TLS session resumption is measured, and gated on h1, h2 and h3.**
`tools/conformance.sh` gains a `resume` stage.
CI runs the suite with no flags, so it gates every pull request, and `tools/build-host-tests.sh` and the laptop's pre-push hook run it too.
It is the only conformance stage that needs no external tester, which makes it the first real protocol check a laptop run performs.
h2spec and h3spec are not installed there, and every other stage skips.

1.9.0 installed a session ticketer so browser sessions could resume, and nothing anywhere could tell whether it worked.
A monitor read 0% resumed on `http/2/external` across a whole fleet for a day after the 1.10.0 deploy, which looks identical whether the ticketer is broken or the traffic has no returning connections.
Issue #101, closed: the server resumes on every protocol, the counter is accurate, and the 0% was the traffic, because a browser opens one h2 connection per visit and resumption needs a return visit inside the ticket's lifetime.

Verified against a running server: 7 of 8 h2 handshakes resumed and 7 of 8 h1, with the server's own `resumed` counter incrementing by exactly the number the client observed.

### Fixed

**`probe::tls_handshake` never collected the session ticket.**
It stopped reading the instant `is_handshaking()` went false, and TLS 1.3 sends `NewSessionTicket` after Finished as application-phase data, so rustls never stored one and the next connection had nothing to offer.
The probe therefore reported `RESUMPTION: none` against a fleet that resumes correctly, which reads as a server defect and was a defect in the measuring tool.
It now does one bounded `read_tls` after the handshake, outside the timed region so the figure stays comparable with the server's own.
`docs/LESSONS.md` lesson 50.

### Changed

**`m6-probe-h1` and `m6-probe-h2` report full and resumed separately**, and exit non-zero when a ticket was offered and never accepted.
Both had always shared one `ClientConfig` across their N connections, so every run was one full handshake plus N-1 attempted resumptions reported as a single distribution, while the header claimed the figure was comparable to a `/perf` channel that separates them.
`--no-resume` reproduces the old behaviour.

`probe::resumption_failed` holds the gate's decision, with five unit tests.
The failing branch cannot be produced against a server that resumes, and a gate whose failure path has never run is a claim.

---

## 1.10.0, 2026-09-19

**HTTP/2 server push and 103 Early Hints are removed, with the hint extractor.**

### Removed

**HTTP/2 server push and 103 Early Hints, and with them the hint extractor.**
Owner's decision.
The reasoning is in `README.md` under "Why there is no server push or 103 Early Hints", and in issue #93, because the absence is a decision and the next person to notice it should find the argument instead of rebuilding the feature.

In short, push cannot see the client's cache, so it sends bytes to visitors who already have them, and it is gone from Chrome, Firefox and RFC 9113.
A 103 fills the gap between a request arriving and the response being ready, and a cache-first server does not have one.
Measured on a production fleet, the origin answered in about 7ms and an edge hit from memory, so the 103 and the final response went out microseconds apart.

The implementation compounded it.
Hints were derived by scanning the response body for `href=` and `src=` and filtering on file extension, which cannot tell an `<img src>` from an `<a href>`.
It preloaded images the page had marked `loading=lazy`, a cache node emitted every hint twice (89 `Link` headers for 45 distinct values), and none of it could be measured because there was no way to turn it off.

Gone: `hints.rs`, `CachedResponse.hints`, `H2Response.hints`, the fifth element of `RequestOutcome::Ready` and `FinalizedResponse`, the 103 emission on h1, h2 and h3, the `PUSH_PROMISE` send path, and the one prefetch site that queued hinted assets.

Kept, deliberately:

- **Refusing a client `PUSH_PROMISE`.** RFC 9113 8.4 makes it a connection error whether or not this server pushes.
- **Validating `SETTINGS_ENABLE_PUSH`.** RFC 9113 6.5.2 makes a value other than 0 or 1 a connection error.
  The value is no longer stored, because the answer to "may I push?" is now no regardless of what the client permits.
- **The prefetch queue.** Four of its five call sites are stale-refresh and route warming, which have nothing to do with hints.
- **Selective `Link` replacement** in `set_describedby_link`. m6 no longer emits preload links, but a backend or an author may, and discarding someone else's headers would be wrong.

Two HTTP/2 tests that measured outbound DATA framing and the per-stream send window through a pushed stream were converted, and not deleted.
Both rules (RFC 9113 4.2 and 6.9) govern every response this server writes, so they now exercise an ordinary one.
Two that tested only server-initiated streams were removed, because nothing can create one.

---

## 1.9.0, 2026-09-18

Early hints stop overriding the page, and TLS connections stop paying for a full handshake every time.
Both were found by measuring a live fleet and not by reading the source, and both had been true for as long as the features existed.

### Fixed

**Every TLS connection did a full handshake, because the server issued session tickets and then had nowhere to match them.**
`make_tls_server_config` took rustls' defaults, and for a public server they are the wrong ones:

    session_storage: ServerSessionMemoryCache::new(256)   server/builder.rs:113
    ticketer:        NeverProducesTickets                 server/builder.rs:116

With no ticketer, TLS 1.3 resumption is **stateful**: the session is held server-side and the client gets a handle back, so every returning client needs its own entry still to be in a 256-entry cache.
What the fleet's own monitor measured, in one hour:

| node | channel | handshakes | resumed |
|---|---|---:|---:|
| origin | http/1.1/external | 451 | 90% |
| origin | http/2/external | 197 | **0%** |
| edge-a | http/2/external | 157 | **0%** |
| edge-b | http/2/external | 165 | **0%** |

**Table 1: handshakes and resumptions per node and channel, measured over one hour.**
Take from it that only the channel with few returning clients resumed.

Those figures look contradictory, and they agree.
HTTP/1.1 there is a handful of clients polling on repeat, so their few entries stay resident.
HTTP/2 is browsers: many distinct clients against 256 slots, evicted long before any of them returns.
The origin took 648 handshakes in that hour.
The channel that resumed is the one that did not need to, and the channel carrying visitors resumed never, and paid two extra round trips on every connection.
A resumed handshake on that fleet ran **p50 0.78ms against 207ms** for a full one.

A `Ticketer` makes resumption stateless, so there is nothing per-client to evict and the cache size stops governing whether a browser resumes.
`session_storage` is deliberately left at 256.
TLS 1.3 never reads it once the ticketer exists, and `new(n)` allocates `HashMap::with_capacity(n)` plus `VecDeque::with_capacity(n)` up front, which is memory bought for a TLS 1.2 session-id path that carries scanners.

**This was verified with rustls on both ends** (`tests/tls_resumption.rs`), where `handshake_kind()` is the protocol's own answer.
It was not verified with `openssl s_client`, which was tried first and is useless here.
macOS ships **LibreSSL 3.3.6** as `/usr/bin/openssl`, whose TLS 1.3 client resumption is incomplete, and it reports a full handshake even against a server that does resume.
That was established by running it against a local build that resumes, and not assumed.

**Early hints preloaded images the page had marked `loading=lazy`.**
On one live site, 17 of 17 lazy images were preloaded anyway: the page said "do not fetch this until it is needed", the server said "fetch it now" in the same response, and the hint won because it arrived first.
Lazy loading on such a page was inert.
`extract_hints` scanned for `href=`/`src=` and never looked at the rest of the enclosing tag.
`fetchpriority=low` is honoured too, for the same reason.

A server overriding an explicit, standardised author instruction is wrong whether or not it is faster, so this is not gated on showing a latency win.

Only the enclosing tag is examined, over a bounded window.
A document-wide search for `loading=lazy` would drop every hint on any page that lazy-loads anything, and there is a test for exactly that.

### Changed

**Hints are ordered by what the resource is, and not by its directory name.**
`hints.sort()` was lexicographic over the URL, so on one site `/assets/company-logos/` sorted ahead of `/assets/css/` and the render-blocking stylesheet was announced 18th, behind 17 logos.
Ranking on `preload_as` needs nothing about a site's layout, since `as=` is the standard's own statement of a resource's role.

Stated honestly: browsers assign preload priority from `as=` and not from header order, so this is unlikely to be worth much on its own.
It is here because ordering by accident is not a decision anyone made.

### 1. Internal

**`backends_contract` leaked a 95MB scratch directory into `/tmp` on every run.**
Named by process id and never removed, holding the compiled C, C++ and Go backends plus `GOCACHE`.
`/tmp` on the build host is a 3.7GB **tmpfs**, which that fills in roughly 39 runs, and it did.
It was found at 82%, where it failed the merge gate twice without either failure naming a disk.
One was a Go link dying on ENOSPC, and the other was `render:minimal` reading 31% over its ceiling, which looks exactly like a real regression and is not.
Stale directories are now swept by age before a new one is made, tested against a tempdir and not the real `/tmp`.

---

## 1.8.1, 2026-09-18

**A prefetched or pushed hint is now stored under the key the request uses.**

### Fixed

**A prefetched or pushed hint was stored under the unversioned cache key, so an edge answered `/a.css` with the immutable `/a.css?v=1` response.**
Found on a live fleet within minutes of the 1.8.0 rollout, by the deploy's own check that an unversioned asset must not be immutable.

| asset, requested with no query | edge | origin |
|---|---|---|
| `/assets/css/style.css` | `max-age=31536000, immutable` | `max-age=60, s-maxage=86400` |
| `/assets/js/nav.js` | `max-age=31536000, immutable` | `max-age=60, s-maxage=86400` |
| `/assets/icons/logo.svg` | `max-age=31536000, immutable` | `max-age=60, s-maxage=86400` |

**Table 1: what an unversioned request got at the edge and at the origin for three assets.**
Take from it that the edge cached an immutable answer the origin never sent for that request.

`extract_hints` returns URLs as the page wrote them, so a hint carries the page's cache-busting query.
Three callers passed that whole string as a **path** with **no query**: both prefetch queue sites and the HTTP/2 push request.
Both key builders strip the path at `?` and read the query only from their own argument, so `make_lookup_key("/a.css?v=1", None, "")` is byte-identical to `make_lookup_key("/a.css", None, "")`.
The request that went out still carried `?v=1`, so the backend returned the versioned, immutable response, and it was stored where an unversioned request finds it.

The stripping is deliberate and stays.
It stops a stray `?` smuggling one request's query into another's key.
What was missing is that callers must split first, which `hints::split_url` now does in one place for all three.

**Why it appeared only now.**
The prefetch loop is not new.
It could not run, because `extract_hints` returned an empty list on every page until 1.8.0 fixed it.
A latent defect became reachable the moment the feature it serves started working.

Verified by a test that pins the trap and not just describes it.
The two lookup keys are asserted equal, so the next caller meets the requirement to split as a failing assumption, and not as a comment.

---

## 1.8.0, 2026-09-17

**m6-http warms its own cache at startup, and this release fixes server push and the hint extractor.**

### Added

**`[server] warm_on_start`: m6-http seeds its own cache at startup.**
Off by default.
When on, the server puts one entry per warmable route per encoding into its background-fetch queue as it starts.

This replaces a shell script.
The script was copied onto every node, run there by a systemd unit, and found the warmable routes by running a regular expression over the node's own `site.toml`.
The server has that file already parsed.
It knows its route table, its error path and which routes are no-store, so it is better placed to make the decision than anything outside it, and moving the decision here retires a script, a unit and a regex TOML parser together.

Selection matches what the script did, on better evidence than a regex:

- concrete paths only, because `/assets/{*relpath}` is a pattern and not a URL
- never the configured error path.
  A public listener refuses that request by design, so warming it would put a 404 body in the cache
- nothing marked no-store, because the answer is not cacheable and the fetch is pure cost

One entry per encoding, identity plus gzip plus br, because the cache keys on encoding.
Warming identity alone leaves a gzip visitor paying for the miss anyway.
Identity is the empty string, which is how the cache key spells "no content-encoding".

**It queues, and it does not fetch.**
The queue drains one entry per event-loop iteration and is the same mechanism that refreshes stale entries, so warming cannot delay startup, cannot block serving, and cannot stampede the origin.
A fleet restarting together does not all reach across the backbone at once.

Off by default, because a server should not do network I/O at boot unless it is asked to.
The system config wins over the site config: warming is a property of the node, and a cache node wants it where a laptop serving the site locally does not.

Verified by four tests over the decision, which is a pure function so it can be driven without a `ServerState`: a plain route is warmed, a pattern is not, the error page is not (and a site with no error page configured has nothing to exclude), and no-store is not, including with surrounding whitespace.

### Fixed

**HTTP/2 server push sent oversized DATA frames and ignored per-stream flow control.**
A hinted asset between 16 KB and the connection window went out as a single DATA frame, which RFC 9113 4.2 requires the peer to treat as a **connection** error of type `FRAME_SIZE_ERROR`.
One stylesheet took down the connection, for any client that had not disabled push.

m6 pushes early-hinted assets whenever a client has not sent `SETTINGS_ENABLE_PUSH=0`.
103 Early Hints is the fallback for clients that have.
The push path wrote `PUSH_PROMISE`, `HEADERS` and the whole body directly, debiting only the connection window, because it never registered the pushed stream at all.
So it obeyed neither the peer's `SETTINGS_MAX_FRAME_SIZE` nor the per-stream window that RFC 9113 6.9 requires, and a client's `RST_STREAM` declining a push was ignored and not seen.

The promised stream is now registered in `reserved (local)` before the promise goes out, moves to `half-closed (remote)` when its HEADERS are sent, and its body is delivered by the same flow-controlled sender as every ordinary response.
Framing, both windows and stream reaping are that one path now.

**Who this affected.**
Chrome 106 and later disable push, so the overwhelming majority of real traffic takes the 103 branch and never reached this.
That is also why it survived 146/146 HTTP/2 conformance: h2spec does not drive server push with an oversized body.

**A second defect, found by fixing the first.**
Routing pushes through the reaping path meant `last_stream_id` could advance to an even, server-initiated id.
It is the **peer's** high-water mark, used to tell an unopened stream (idle) from a finished one (closed), so a client's next legal odd stream would derive `Closed` and be answered `STREAM_CLOSED`.
It is bumped for client-initiated streams only.

Verified by four tests: DATA is split to the peer's maximum frame size, the per-stream window bounds a push, a push does not move the peer's high-water mark, and a client may decline a push with `RST_STREAM`.

**Early hints, and therefore server push, did nothing at all on any site m6 minifies.**
`hints::extract_hints` matched only quoted HTML attributes: `href="`, `src="`, `href='` and `src='`.
m6's own minifier strips attribute quotes, so the extractor found nothing in the output of the server it ships with.
Measured against a live homepage, fetched as identity so the body is the real thing: **0 quoted `href="`, 0 quoted `src="`, 182 unquoted `href=`, 108 unquoted `src=`**.

Everything downstream was inert: no `Link: rel=preload`, no `103 Early Hints` on h2 or h3, no prefetch of hinted assets into the cache, and no server push, whose branch is guarded on the hint list being non-empty.
A performance feature with tests, a config surface and two protocol paths, doing nothing.

HTML5 allows an unquoted attribute value, ending at whitespace or `>`.
Both forms are read now, and an attribute name must be preceded by whitespace so that `data-href` and `xlink:href` are not mistaken for `href`.

**A hint now names what the page will actually request.**
The URL was emitted with its query string removed, so a hint read `/assets/css/style.css` while the page asked for `/assets/css/style.css?v=ae331def`.
m6 keys the cache on the full path including the query, so the preload warmed an entry nothing would ask for and the visitor paid the miss anyway.
The query is ignored where it should be, which is reading the extension to decide the `as=` value, and kept everywhere else.

**The existing tests could not have caught this.**
Every one of them wrote its own fixture, and every one wrote it with quotes, so they passed against input the server never produces.
The new test does not write its input: it runs the same `minify_html` production runs and asserts on what comes out, with a guard that fails if the minifier ever stops stripping quotes and makes the test vacuous.

**Two real defects in the conformance gate, found by running shellcheck for the first time.**
The repository has 10 shell scripts, several carrying shellcheck disable directives, and nothing had ever run shellcheck: not CI, not `tools/build-host-tests.sh`.
The directives were decoration, and they made the repository look linted.
First run: 2 errors and 14 warnings.

Both errors were in the path that kills the harness's own child processes:

```sh
for p in ${PIDS[@]:-}; do kill "$p" 2>/dev/null || true; done
```

An unquoted array expansion, so an element containing whitespace re-splits and the loop kills the wrong thing or nothing at all.
`${PIDS[@]+"${PIDS[@]}"}` is what `:-` was reaching for and does not achieve under `set -u`.
Seven more findings were `PIDS+=($pid)`, the same class one step milder.

`bench.sh` set `SKIP_VERIFY` in two places and read it nowhere, because `BENCH_PASS` already carries `--skip-verify` unconditionally.
A flag that looks like it does something and does not, now removed.

Two SC2034s in `tools/conformance.sh` are annotated and not deleted.
`H2C_PORT` and `EDGE_BRIDGE_PORT` are unused as variables, but they are the only record of which ports the harness reserves, and deleting them would lose the note that stops something else picking a colliding one.

**Gated in CI at warning level, and not style.**
Style is 100+ findings of preference in this repository, and a gate that reports preference gets ignored, which is exactly how two real errors survived in a repository whose own rule is that a check which cannot measure must fail.

Verified against the running harness and not the source, because the conformance gate is what was edited: h1 32/32 four times, h2 146/146, h3 47/49, every target measured.

### Changed

**Every `#[allow]` in the workspace is gone, and clippy's zero is now real.**
`tools/clippy.sh` runs `-D warnings` and reported zero.
An `#[allow]` in source overrides a lint level given on the command line, so that zero was measured over code that had been told to stay quiet: 13 attributes hiding 22 findings, two of them blanket attributes over a whole binary crate and a whole module.

The push defect above is what one of them was hiding.
Its stated reason, that this server never pushes, was false, and it explained away the symptom: the `ReservedLocal` state was never constructed because the push path bypassed the state machine entirely.

The rest were a dead subprocess test harness whose helpers returned success when the binary was not built, four `too_many_arguments` on functions whose six shared parameters became one `analytics::Event`, a `PerfReport::build` that now takes what to report on and who is asking, dead fields, unused imports, and one stale attribute.

No public API of `m6-core` changes.
`m6-http`'s internal analytics helpers take an `Event` and not six positional arguments.

**The build host refuses to check the examples when it cannot measure them.**
The 05-cms end-to-end suite binds 127.0.0.1:8443 and waited only for a 200 on that port.
A staging node on the same box bound to `0.0.0.0:8443` satisfies that probe just as well, so the suite could measure a server nobody had changed and report its failures as the example's.
It now refuses to start when the port is held, naming the process, and confirms the answering server is the example before believing 98 assertions about it.

### 1. Note for deployments running a warm script

Setting `warm_on_start` needs this release on the node first.
A 1.7.0 binary does not know the key, and on m6's config rules an unknown key is a config error, so setting it ahead of the rollout buys a failure on every node at once.
Roll 1.8.0 out, then turn it on, then retire the script and its timer.

---

## 1.7.0, 2026-09-16

**Custom error pages now work across a proxy hop.**
1.6.0 recorded the gap, and this release closes it.

### Fixed

**m6-http refused every external request to its configured `errors.path`, for a sound reason, and it refused its own cache nodes on the same rule.**
The page is fetched with the status and the original path appended to its URL, so a public caller allowed to request it directly could supply its own and have a deployment's template render them.

It refused a deployment's own cache nodes on the same rule.
A cache node holds no content, so when a path matches nothing it fetches the error page from the origin over h2c on the private backbone.
The origin refused it exactly like a stranger, the node fell back to m6-http's built-in page, and that is what visitors saw at the edge.

**Every layer of config was correct.**
Both site configs set `mode = "custom"`, both routed the error path to the HTML backend, and that backend returned 200 with the branded page when asked directly over its socket.
The mechanism was configured, deployed and dead, which is worse than unsupported because it looks like it works.
The origin looked healthy for an unrelated reason: a missing path there matches a route to the backend and never reaches the error machinery at all.

The origin always had the information needed.
`Iface::for_bind` classifies a private bind as internal, and the h2c listener is granted `ForwardedTrust::Backbone` on exactly that basis, its own comment reading "granted by the listener, never by a header".
That fact never reached the decision.
It does now, threaded to the handler and read only by this guard, so a public client still cannot claim it.

The decision is a pure function and not an inline condition, because no test drives an h2c listener end to end and this needed to be testable.
Three tests cover it: a public request refused and an internal one allowed, ordinary paths untouched on either listener, and no firing at all when no custom page is configured.

A deployment that worked around this by serving the same template at a second routable path no longer needs to.

A public caller still cannot claim the internal trust.

---

## 1.6.0, 2026-09-16

**The production error page no longer names the software serving it.**

### Security

**`internal_error_html` in non-verbose mode, which is what production uses, rendered the server's name.**

```html
<body><h1>404 Not Found</h1><p>m6-http</p></body>
```

Verbose mode puts a description of the status in that paragraph, which is useful to a developer reading it.
Production put the name of the server software there, which tells a visitor nothing and tells a scanner what is serving.

Found on a live fleet, and only on part of it.
An origin never shows this page, because a missing path there is routed to a backend that renders the site's own.
A cache node has no route matching an arbitrary path, so this page answered every probe for `/.env`, `/wp-admin` and the rest, naming the software that refused them.

**The existing test on this body passed throughout.**
It asserted only what the body should contain and never what it should not, and a page can be right about the status and still say too much.
Both halves are asserted now, across four statuses, plus a second test holding verbose mode to still carrying its detail so the fix cannot be made by gutting that instead.

This is the generic default.
A deployment that wants a branded page configures `[errors] mode = "custom"`, and this is what answers when that is absent, or when its fetch fails, which is exactly the case where an origin is unreachable and an edge has nothing else to serve.

### 1. Note for anyone using custom error pages behind a cache

m6-http refuses every external request to its own `errors.path`, correctly: a caller could otherwise supply its own `status` and `from` and have them rendered.
It cannot, however, distinguish a spoofing stranger from a downstream cache node of the same deployment fetching the page it was told to fetch.
So `mode = "custom"` does not work across a proxy hop: the edge asks, the origin refuses it like any stranger, and the edge falls back to this page.

Nothing in m6 changes here, because the workaround is a deployment's to make.
Serve the same error template at a second, normally-routable path and point the edge's `errors.path` at that.
Whether m6 should grant the backbone listener an exception is open.
The trust concept already exists as `ForwardedTrust::Backbone`, but it is not threaded to where the refusal happens.

1.6.0 records the custom-page limitation that 1.7.0 then fixes.

---

## 1.5.0, 2026-09-16

**1.4.0 made the HTTP/3 handshake slower on short paths than no certificate compression at all.**
This release caches the compressed chain, and it makes `/perf` report the running release so the monitor can flag drift.

### Fixed

**BoringSSL calls a certificate compressor on every full handshake.**
A certificate chain does not change while the process runs, so the same bytes were recompressed per connection, and brotli at quality 11 is expensive:

| | cost on a 3.4 KB chain |
|---|---|
| brotli q11 | **8.848 ms** |
| brotli q9 | 2.104 ms |
| brotli q4 | 1.113 ms |
| zlib L9 | 0.137 ms |

**Table 1: the cost of compressing a 3.4 KB chain at four settings.**
Take from it that brotli q11 costs many times the zlib alternative.

Quality 11 was chosen for the best ratio without anyone measuring what it cost.
End to end, the h3 handshake p50 against a 4.9 ms path:

| | p50 | flight |
|---|---|---|
| no compression | 12.5 ms (2 RTT, the flight stalls on the 3600-byte budget) | 4081 B |
| zlib | 7.0 ms | 2962 B |
| brotli q11, per handshake (1.4.0) | **14.6 ms** | 2859 B |
| **brotli q11, cached (1.5.0)** | **6.8 ms** | 2858 B |

**Table 2: the h3 handshake p50 and flight size under four schemes.**
Take from it that caching the compressed chain turns the slowest option into the fastest.

A kilobyte saved is worth nothing against ten milliseconds of CPU.

On a long path this was invisible.
At 211 ms and 308 ms of round-trip time the handshake measured 1.00 and 1.05 round trips after the 1.4.0 deploy, because a round trip hides ten milliseconds completely.
On a 5 ms path from a single-core VM it was the whole cost: 9.5 ms under 1.3.0, 14.6 ms under 1.4.0.
It was found by measuring a running server after the rollout, and not by any gate.
Neither CI nor conformance measures handshake CPU on a short path.

The compressed chain is now cached for the life of the process, keyed by the uncompressed bytes so a collision cannot serve one host's chain in place of another's, bounded at 8 entries, and a failure is not cached so a transient one does not persist.
The compressor runs outside the lock: holding a lock across an 8 ms compression would serialise every concurrent handshake behind it.
Quality 11 is kept, because the cache makes it a one-off per chain and the ratio is the reason to prefer brotli at all.

### Changed

**`/perf` reports the running release, and m6-monitor flags fleet drift.**

`/perf` published node, uptime, pools, backends, metrics and host, and no version, so the only way to learn what a node was running was to ssh in and ask the binary.
"Every node runs the release we think it does" was an invariant nothing could check remotely.

On 2026-09-16 four written records disagreed about the deployment fleet and none of them matched it: a deploy repository pinning one tag, its captured config naming that tag with an artefact hash matching nothing running, a release log naming an older tag as the newest deploy, and every node serving a fourth.
No node has to be faulty for that.
Nothing can observe the truth, so the records rot quietly.

The monitor now prints the version per node and raises a warning when the reporting nodes disagree, naming each.
A node that cannot report its version counts as drift, and not as agreement: two nodes agreeing while a third is silent is not a uniform fleet, it is an unknown one, and calling it uniform is the failure this exists to remove.

The field is `serde(default)`, so a node older than this change deserialises to an empty string instead of making the payload unparseable, and renders as "unknown (node too old to report it)".
The fleet is upgraded one node at a time, and an aggregator that cannot read an older node learns nothing about the node it most needs to ask about.

**This reverses a documented decision.**
`monitoring.rs` said "No version or build string is exposed by either endpoint, at any tier: that only tells a scanner which vulnerabilities are worth trying."
That argument is about an anonymous reader, and `/perf` has none: with no token configured it is a 404, and with one it is a 401 without the credential.
Anyone holding the perf token already reads live latency percentiles and error counters, which is a far better attack signal than a version string.
`/health` is unauthenticated and still exposes nothing, so the public surface is unchanged.
The reasoning is recorded in the module docs, and not only in a commit.

### 1. Note

The quiche fork is at `efff6235` on branch `m6-cert-compression`.
Its own suite passes at 1126 + 45 tests, zero warnings and clippy silent in both the default and `cert-compression-zstd` configurations.

---

## 1.4.0, 2026-09-16

**The certificate chain no longer costs a round trip, and the spec deviation that hid that is gone.**

### 1. Performance

A QUIC server may send only three times what it has received before it has validated the client's address (RFC 9000 8.1).
A 1200-byte client Initial gives a 3600-byte budget, and m6's uncompressed handshake flight was 4081 bytes: the server sent 3600, stopped, and waited a full round trip for an ACK before it could finish.
1.2.0 and 1.3.0 covered that with `set_max_amplification_factor(4)`, a deliberate deviation marked temporary from the day it shipped.

Certificate compression (RFC 8879) removes the need for it.
Measured against the staging origin, same probe and path, only compression changing:

| | server flight | datagrams | handshake |
|---|---|---|---|
| no compression | 4081 B | 4 | stalls at 3600, +5.07 ms |
| brotli | **2859 B** | 3 | no stall, 1.16x |

**Table 1: the server flight and handshake with and without brotli compression.**
Take from it that compression brings the flight under the 3600-byte budget.

2859 against a 3600 budget, so the flight fits at the **conforming factor of 3** with headroom for a chain that grows.
On an intercontinental path, where a round trip is 270 to 300 ms, that round trip was the single largest avoidable cost in setting up a new connection.

### 2. zlib is accepted, never offered

m6 compresses with brotli, and with zstd where that feature is built.
It accepts all three as a client and offers **zlib to nobody**, which needs explaining because it looks like missing support and is not.

Offering zlib broke h3spec: 38 of 49 tests failed with `TLSDecryptError "cannot verify CertificateVerify"`.
It is not a codec defect.
m6's zlib output is ordinary RFC 1950 and libz round-trips it byte-identically.

hs-tls, which h3spec is built on, decodes a CompressedCertificate into a certificate chain and then reconstructs the wire bytes by **re-compressing** at zlib's default level (`tls/Network/TLS/Packet13.hs`):

```haskell
putOpaque24 $ BL.toStrict $ compress $ BL.fromStrict bs
```

A TLS 1.3 transcript is the handshake messages as transmitted, so a reconstruction only agrees when both ends run a byte-identical compressor at identical settings.
m6 compresses at level 9 with miniz_oxide, so the transcript hashes differ and the peer rejects m6's CertificateVerify.

zlib is also the only algorithm hs-tls implements, so offering it is precisely what selects it for the clients that cannot handle it.
Declining it costs them nothing: they advertise nothing m6 compresses with, so they receive a plain chain and handshake normally.
Brotli compresses m6's own chain better than zlib did anyway, 2859 bytes against 2962.

The failure is independent of certificate size.
Three self-signed certificates of 615 B, 1151 B and 1846 B on disk (EC P-256, RSA-2048, RSA-4096) all fail identically, so it is not a fragmentation or flight-size effect.

### Fixed

**The conformance harness reused an expired certificate, and reported it as a protocol regression.**
`start_edge` generated its self-signed certificate only when the file was absent, never when it had expired, and `$WORK` is a fixed path under `/tmp` that survives between runs.
A `-days 2` certificate created on Sep 11 was still in use on Sep 15: every handshake failed with `TlsFail`, and h3 scored 11/49 against a floor of 47.

That is indistinguishable from a broken server if you read only the summary line.
Several certificate-compression and amplification measurements were taken against it and misread before the certificate itself was checked, and five consecutive source changes produced an identical score without that being recognised as the signal it was.

The certificate is now regenerated when it is missing **or** within an hour of expiry.
Note the direction of this defect: an expired fixture can only cause a false FAIL, never a false PASS, so no previously recorded score was inflated by it.

### Changed

The quiche fork is at `bdf00278`, on branch `m6-cert-compression`.
It adds RFC 8879 compression and decompression for zlib, brotli and zstd, with zlib registered for decompression only.
Its own suite passes unchanged at 1123 + 45 tests, zero warnings and clippy silent in both the default and `cert-compression-zstd` configurations.

`zstd` remains behind a feature flag: it is the only one of the three with no pure Rust compressor, and it saves about 50 bytes less than brotli on a real chain.
It is not a trade worth inheriting silently.

---

## 1.3.0, 2026-09-16

**HTTP/3 0-RTT is enabled, backend errors name their backend, and the quiche pin is reachable.**

### 1. Performance

**A returning visitor's request rides in the first flight**, so a complete response costs one round trip instead of a handshake plus a round trip.

Measured from a laptop against a 4.92 ms path:

| scenario | h1 | h2 | h3 | h3 + 0-RTT |
|---|---|---|---|---|
| cold, first packet to first response | 23.1 ms | 15.1 ms | 13.0 ms | **6.4 ms** |
| warm, time to first byte | 10.24 ms | 5.00 ms | 5.02 ms | 5.00 ms |

**Table 1: cold and warm response times per protocol, with and without 0-RTT.**
Take from it that 0-RTT halves the cold h3 time.

6.4 ms against a 4.92 ms round trip is **1.3 RTT**, within 0.3 RTT of the floor: no HTTP response can beat one round trip, because the request has to arrive and the answer has to come back.
Against h2 a returning visitor is **2.4x faster** to a first response.

The h1 and h3 cold figures are `m6-bench-detail` phase measurements, n=30 for per-request phases and n=1 for connection setup, which that tool records only from its first post-warmup connection.
The 0-RTT figure is n=4 from `m6-probe-h3 --0rtt` (6.38, 6.41, 6.49, 6.53 ms).

**The replay gates are unchanged and still strict.**
In early data m6 serves only a FRESH cache hit and answers 425 Too Early to everything else, and not merely RFC 8470's "idempotent methods".
This site's analytics beacon is a fire-and-forget GET, so "GET is safe" would not have been enough, and a stale hit queues a background refresh, which is a write.

### Fixed

**Enabling 0-RTT used to cost a conformance test.**
It no longer does, and the failure was in three different places before it was found.

h3spec's `MUST send PROTOCOL_VIOLATION if CRYPTO in 0-RTT is received [TLS 8.3]` took h3 from 47/49 to 46/49 whenever early data was enabled.
Three fixes in the quiche fork, of which the first two were in the wrong place:

1. **A guard on the CRYPTO frame** in `process_frame`.
   Correct per RFC 9001 §8.3, and it changed the score not at all.
   h3spec sends its 0-RTT packet during a FRESH handshake with no resumption, so there is no 0-RTT read key, so the packet is buffered as undecryptable and its frames are never parsed.
   Its own qlog:

   ```
   1. initial: [crypto, padding]
   2. initial: [crypto]
   3. 0RTT:    [crypto, padding]   <- the violation
   4. initial: [ack, crypto, padding]
   ```

   A check on frame CONTENTS cannot fire for that, by anyone.
   Kept regardless: it is correct for a genuinely resumed connection, where the frame is readable.

2. **Reject on the packet TYPE**, before decryption.
   A server holding handshake keys but no 0-RTT key knows no PSK was accepted, so the peer sent 0-RTT it was never entitled to send.
   This detected the violation, m6-http logged `conn.recv error: InvalidPacket`, and h3spec still failed.

3. **The CONNECTION_CLOSE was going where the client could not read it.**
   quiche put it in a Handshake packet, and a client derives its handshake keys from the server's flight, which had not been sent.
   The client's qlog showed it received only an Initial carrying an ACK: no close, no server CRYPTO at all.
   `write_pkt_type` preferred an Initial close only when `recv_count == 0`, a first-flight case.
   Here it was non-zero.
   It is now keyed on whether the server has ever sent a Handshake packet, which is the fact that decides whether the peer could hold the keys.

   A first version preferred Initial whenever the handshake was unconfirmed, and quiche's own `app_close_by_server_during_handshake_not_established` caught it: there the client IS established and has discarded its Initial keys, so Initial is the unreadable one.

**Backend errors name the backend.**
The monitor reported a single total, "3 backend errors since start", leaving the operator to guess the service.
The guess that matters is render-contact: a contact submission whose SMTP send fails returns 500 and is counted nowhere else, so an anonymous total is the difference between seeing silent mail loss and not seeing it.

m6-http already knew which backend errored, `Stats::record` takes the name and uses it to decide whether an error counts at all, and then discarded it.
It is now kept per backend, on `/perf`, rendered worst-first:

```
3 backend errors since start: render-contact 5, m6-html 2
```

The new field is `serde(default)`, so a node older than this reports "node too old to attribute them" and not nothing, which is the same rule this codebase uses for absent sample counts.

### Security

**The pinned quiche revision was unreferenced.**
1.2.0 pinned `916a5a25`, which sat on no branch and no tag in the fork and was reachable only by raw SHA.
GitHub garbage-collects unreferenced objects.
When that happened every build would have failed, CI included, and the commit would have been unrecoverable.
It is now referenced by both a branch and a tag, `m6-pinned-1.2.0`.

The fork is now at `4bbba71e`.
Its own suite passes unchanged at 1123 + 45 tests.

### Changed

`tools/build-host-tests.sh` reports build-host disk usage and, above 80%, prints the command that reclaims it.
`cargo test --workspace` fills `target/debug` and cargo never garbage-collects it: the build host sat at 73% with **65G** of debug artefacts across 5023 dependency files and 2311 fingerprint directories, against 6G of release trees that deploys actually use.
Clearing it took 104G used down to 37G.

It is reported and not deleted automatically.
A gate that quietly removes 65G of build cache makes the next run slow for reasons the operator did not choose.

---

## 1.2.0, 2026-09-15

**m6-monitor polls on a schedule and serves the report over HTTP**, and `GET /check` returns the whole health report as text.

### Changed

**Every request used to poll the whole fleet itself.**
`page` and `digest_json` each called `collect_from` directly, with no background loop and no cached state.
So load on the production nodes scaled with how often anyone LOOKED at the monitor, two requests moments apart could disagree because they came from different polls, and every request paid the full poll latency.
Measured over an ssh-forwarded unix socket:

| endpoint | before | after |
|---|---|---|
| `/digest` | 3.56 s | **0.15 s** |
| `/` | 2.86 s | **0.013 s** |
| `/check` | **404** | **200, 0.011 s** |

**Table 1: monitor endpoint latency before and after the change.**
Take from it that reading a cached snapshot is orders of magnitude faster than polling.

A thread spawned at startup now refreshes a shared snapshot every `poll_interval_s` (default 30).
All three routes read it, and none of them polls.

### Added

**`GET /check` returns the whole A-to-G health report as text.**
`check::render` produced it already and was reachable only through the one-shot `--check` CLI, which is the only reason that flag existed.
The health check is now one fetch over a forwarded socket, with no ssh to any node:

```sh
ssh -fN -L /tmp/m6mon.sock:/run/m6/m6-monitor.sock user@example.com
curl --unix-socket /tmp/m6mon.sock http://localhost/check
```

**Every response states the age of its reading.**
`/check` prints `polled 25.3s ago (...)` as its FIRST line so it cannot be read past, and a snapshot older than three intervals prints a STALE banner saying the poller is failing and the figures are not current.
`/digest` carries `snapshot_age_s`, `snapshot_at`, `poll_interval_s` and `stale`.

A monitor serving an old reading without saying so is worse than one that is plainly down, because it looks like current information.
Before the first poll completes both return 503 and say so, and not serving zeroes: "not measured yet" and "measured as zero" are different claims, which is the same distinction this codebase makes for handshake sample counts.
A failed poll KEEPS the previous snapshot and lets its age grow, and it does not discard the last known good reading or present it as current.

### Fixed

**`m6-probe-h3` made the server it measured log errors.**
`quic_handshake_shape` set a unidirectional stream limit but granted no `initial_max_stream_data_uni` credit, which defaults to 0.
The server could open its h3 control and QPACK streams but not write SETTINGS on them, so `h3::Connection::with_transport` failed and m6-http logged `h3 init error: TransportError(StreamLimit)` for every probe connection.

Probing production wrote 18 such warnings into origin's journal and 4 each into edge-a's and edge-b's, which then surfaced in a health check as an unexplained new error on two nodes.
Seven-day totals confirmed no real client had ever triggered it.

The handshake figures were unaffected, because QUIC establishment completes before h3 is set up.
But a measuring tool that makes the thing it measures log errors is not read-only, and nobody reading those journals could have known the cause was the probe.
Verified: 4 errors before the fix, 0 after.
Setting the stream limit alone changed nothing, which is how the real cause was found.

**The config path comes from `AppContext.config_path`**, and not from `std::env::args().nth(2)`, which quietly assumed an argument position.

### 1. Note

`m6-monitor --check <config>` still works and does the same render, but re-polls the fleet to do it.
It should go once `/check` has been the normal route for a release.
`--help` does not list it: the usage string is generated by core's `App`, and a service cannot declare its own flags, recorded in issue #32.

---

## 1.1.0, 2026-09-15

**`/perf` gains per-channel handshake timing, three probe clients and real latency.**

### 1. Performance

**The HTTP/3 handshake took two round trips and now takes one.**
Staging went from ~12.5 ms to ~6.8 ms on a 5 ms path.

A QUIC server may send only `factor x bytes received` before it has validated the client's address, to stop it being used as a reflection amplifier (RFC 9000 §8.1).
A client's opening Initial is padded to 1200 bytes, so at quiche's default factor of 3 the budget is 3600.
m6-http's handshake flight is 4082 bytes, nearly all certificate chain, so quiche sent 3600, stopped with 482 bytes owed, and waited a full round trip for the client's ACK.
Measured against a 4.85 ms RTT:

```
+7.240ms  server 3600B   3.00x   <- stops dead, 482 bytes still owed
+12.221ms server 4082B           <- one full round trip later
```

`cfg.set_max_amplification_factor(4)` raises the budget to 4800 and the flight goes out in one piece.
**Conformance is unchanged at 47/49**, measured either side of the change and not assumed: h3spec does not test this limit.

**This is deliberately temporary and marked as such in the source.**
RFC 9000 §8.1 says MUST NOT exceed 3.
It is accepted because 3x and 4x are the same practical outcome for a reflection amplifier: DNS gives around 50x, NTP monlist 500x, memcached 50,000x, and at either factor the attacker burns a third or a quarter of the attack on their own upstream.
The line is deleted when certificate compression lands (issue #28), which measurement shows is sufficient: zlib takes the chain from 3400 to 2345 bytes, saving 1055 where 482 is needed.

This is the ordinary case, and not anything unusual about one deployment.
Fastly's study measured 40-44% of uncompressed chains exceeding the budget, and compression taking that to 1-9%.

### Added

**Per-channel handshake timing on `/perf`, for h1, h2 and h3, split by resumption.**
It is never summed on either axis, because neither axis is the same measurement:

- h1 and h2 are the rustls handshake and EXCLUDE the TCP round trip, which finished before rustls saw the socket.
  h3 is the QUIC handshake and INCLUDES its equivalent, because QUIC folds transport and crypto together.
- a resumed handshake skips the certificate and the signature.
  rustls with the `std` feature defaults to a 256-session store, so resumption is already happening without anything being configured for it, and blending the two gives a figure that moves with the returning-visitor mix while neither cost changes.

Each of the four groups carries windowed percentiles plus a lifetime count, mean, min and max that the reservoir cannot discard.
The per-channel reservoir is now 1024, up from 512.

**Three single-purpose probe clients**: `m6-probe-h1`, `m6-probe-h2`, `m6-probe-h3`.
One protocol each, one handshake per connection, strictly sequential, no requests and no charts.
They exist because a figure the server reports about itself has to be checkable against something.
`m6-probe-h3` also reports handshake shape, client flights, server datagrams, and a per-packet timeline with the live amplification ratio, which is what identified the round trip above.

**m6-monitor renders three sections it previously could not**: firewall blocks, cache-header assertions taken from the deployment's own declaration and not duplicated in the monitor, and connection setup per channel.
`header_faults` folds into the verdict, so a wrong cache policy can no longer print ALL CLEAR.

### Fixed

**A completed TLS handshake was discarded if the client closed immediately.**
`advance_tls` returns `Err` on EOF, and a client that finishes its handshake and closes at once sends the final flight and the FIN together, so one call completed the handshake, hit EOF, took the error path, replaced `conn.kind` and returned about forty lines above where the handshake was recorded.

Measured with 200 sequential handshakes per protocol against a client figure of 0.34 ms: **h1 recorded 0 of 200**, because nothing happens after an h1 handshake so the close won every time, and the channel did not appear on `/perf` at all.
**h2 recorded 70 of 200 at a p50 of 6.14 ms**, because the only survivors were connections the server reached before the close, the ones that had already waited an extra event-loop turn.
The surviving population was selected for slowness, so the figure read 18x the truth and looked plausible.
h3 was unaffected, recording 200 of 200 correctly, because the QUIC path stamps inside the packet handler.

**`m6-bench-detail` panicked before measuring anything.**
m6-http builds rustls with `default-features = false`, so no process-level `CryptoProvider` is installed automatically and the first `ClientConfig::builder()` panicked.
Every other binary in the crate installs it in `main`, and this one built a provider inside a verifier method and dropped the result.
Its h1 config also offered no ALPN, so it measured whichever channel the server fell back to, and its chart directory was never created, making every SVG write fail while the run still exited 0.

### Security

**rustls 0.23.44 → 0.23.45, for RUSTSEC-2026-0285.**

rustls accepted TLS 1.3 handshake messages sent at the wrong encryption level when they followed a key-changing message in the same record: a plaintext `EncryptedExtensions` packed into the same record as the `ServerHello`, for example.
RFC 8446 §5.1 requires that handshake messages do not span key changes and that the connection is terminated with `unexpected_message` if they do.
Functionally the same bug as Go's GO-2026-4340 (CVE-2025-61730).

`CVSS:3.1/AV:N/AC:L/PR:N/UI:N/S:U/C:L/I:N/A:N`, category crypto-failure.
The advisory is explicit that the handshake transcript is still authenticated, so a network-position attacker cannot use this to alter or complete a handshake, and the practical effect is that a peer could send handshake messages that should have been encrypted in plaintext without rustls rejecting the connection.
A protocol-strictness failure and not a break in authentication.
m6-http terminates TLS, so it is fixed and not deferred to the next release.

`Cargo.lock` only: the declared constraint was already `"0.23"`.
Three edges move and not one, and `cargo update --dry-run` reporting "Locking 1 package" undersells it: `cssparser-macros → syn` goes 3.0.5 to 2.0.119 and `tempfile → getrandom` goes 0.4.3 to 0.3.4.
Both are cargo re-resolving onto versions already present in the lockfile and not new dependencies, both are build-time graphs, and pointing `cssparser-macros` at the syn 2 the rest of the tree already uses reduces duplication.

Verified against a real handshake and not a successful build, since the change is a TLS library: TLS 1.3 negotiating `AEAD-CHACHA20-POLY1305-SHA256` with verify 0, TLS 1.2 negotiating `ECDHE-RSA-CHACHA20-POLY1305`, ALPN h2 and h1 both answering 200.

### Changed

**A secrets file supplies values, and it no longer overrides them.**
A key set in both a config and its `secrets_file` is now an error at load, naming every clashing key and both paths.
It used to merge with "src wins on conflict", silently, so a config file could state a value that was not the one in use.

That is not hypothetical.
A deployed renderer config read `host = "localhost"`, `port = 1025` and a `from` address, and all of them were inert because the secrets file set them too: the service was relaying through a real provider and sending as a different domain.
An audit read the deployed config and drew the wrong conclusion.
A file that is overridden looks exactly like a file that is correct when you are holding one file.

Tables are still descended into, so two files contributing DIFFERENT keys to one section is unaffected, and that is what a secrets file is for.
A clash is one value claimed twice.
An empty string counts as a value, because `username = ""` beside a real username is exactly how a config comes to look as though it holds credentials.

**This is a breaking change for any deployment whose two files overlap.**
The service will refuse to start and say which keys.
The fix is to give each key one owner: secrets in the secrets file, everything else in the config.
If the config held a deliberately broken placeholder so that a missing secrets file would fail loudly, delete it, because an absent required key already fails loudly and names itself, which a present-but-wrong value does not.

### Fixed

**No email address is published in `SECURITY.md`.**
GitHub renders that file on the repository front page, so an address in it is an address scrapers read.
It now points at GitHub private vulnerability reporting, which is the better channel anyway because it threads and takes attachments, and at a contact form.

`m6-core`'s "this crate names no particular deployment" test grew the second domain it had been missing, which is how a personal address survived in that file.
Its own self-test was checking one entry of the banned list and not all of them, which is why a missing entry was never going to be noticed.

**`/perf` reported no latency at all, on every node, and never had.**

`m6-http`'s aggregate latency reservoirs were cleared by `maybe_emit` every ten seconds, and `Stats::snapshot()`, which is what `/perf` serves, read those same fields.
So `/perf` returned the percentiles of whatever fraction of a ten-second window happened to be open when it was scraped, and a node taking a couple of requests a minute has almost no cache hits in any given ten seconds.

Observed on a production origin with ten hours of uptime: `cache_hits_total: 338` beside `hit_samples: 0, hit_p50_ns: 0, hit_p99_ns: 0`.
m6-monitor correctly reads zero samples as "not measured" and publishes `null`, so the fleet digest carried **no latency for any node**, while the `periodic stats` log line was printing `hit_p50_ns=3878` for the same counter in the same minute.

The per-channel reservoirs were never cleared and held real data throughout, which is why this survived: anyone scrolling past the aggregate in `/perf` saw plausible per-channel numbers.
The aggregate is the only figure m6-monitor reads.

The reservoirs now run as rings and are never cleared.
New `*_window_added` counters give the periodic log its own ten-second window, so the operational logging is unchanged, which is the property `snapshot()`'s comment was protecting when it declined to reset.
Declining to reset was right, and also reading the window the emitter reset was the defect.
One reservoir, no extra memory, no extra work on the request path.

`percentiles_ring` replaces `percentiles`: it reads the newest *n* entries and not `samples[..n]` from the front, which was correct only while the index was reset every window.
Against a ring that genuinely wraps, reading from the front reports the **oldest** samples as current, so the two changes had to land together.

`snapshot()`'s percentiles now span the most recent up to 4096 samples, **a count and not a period of time**.
On a quiet node that reaches back hours and blends idle and busy traffic, which matters because this number is load dependent (`docs/PERFORMANCE.md` §4).
`hit_samples` and `miss_samples` are therefore reported beside the percentiles in the periodic log as well, and m6-monitor's digest carries `hit_samples` per node so the figure can be read at all.

Verified: six tests in `stats::perf_reservoir_tests`, three of which fail against the old reset, checked by reinstating it.
End to end against a running 05-cms stack, scraped across two emit boundaries with silence between them: 81 samples, p50 2,250ns, p99 5,625ns, stable across all three scrapes, where the old code returned zeros.
The periodic log still reports per-window figures: 39 hits at p50 2,208ns in the busy window, zeros in the idle ones either side.

1.1.0 fixes TLS handshake recording and latency reporting on `/perf`.

---

## 1.0.0, 2026-09-14

The first release.
Everything under this heading was on `develop` unreleased and undeployed, some of it for months.

`docs/PERFORMANCE.md` has the measured performance story by commit.

### 1. What this repository is

**m6 is a generic web system: an HTTP edge and a library for writing services behind it.**
It is not a website.
A site built on m6 is a separate repository with its own changelog and its own version numbers, and nothing about any particular site's content, fleet or deployment schedule belongs in this file.

The boundary is the wire contract in `docs/m6-backend-protocol.md`, which is version 1 and language agnostic, plus `m6-core` for services that want the Rust conveniences.
Anything that generalises belongs here, and anything true only of one deployment does not.

### 2. What 1.0.0 means here

It does not mean finished.
It means the consolidation work is done, the checks are real, and the numbers in this repository are measurements and not claims.
The API is stable enough to tag and pin against, which is what `m6-backend-protocol.md` version 1 already says about the wire contract.

### 3. The body of work

- **m6-core is the PHP of m6**, phases 0 to 6: a box of blocks a service is assembled from, and the only crate a service links.
  `m6-file` and `m6-auth-server` are `App` services now, and m6-file lost 969 lines including a second router and a second config parser.
- **Config-driven routes.** `App::handler(name, f)` plus `handler = "..."` on a route, rebuilt on every reload, so a static file server gains an asset tree by being told about a directory and not by being recompiled.
- **Streaming response bodies.** A stream structurally has no bytes for the minifier, compressor or default ETag to touch.
- **Copy elimination.** Core's per-request copying went from ~442us to ~1.58us.
- **Zero `unsafe` in production code**, down from 390 lines of raw libc in the filesystem watcher.

### 4. The checks became real

This is the half that mattered most, because most of it had never run.

- **h1, h2 and h3 conformance are measured against recorded minimums.**
  h2 and h3 had no minimum at all and were silently skipped on every run: the only thing running them was a laptop hook on a machine with neither `h2spec` nor `h3spec` installed, and the Linux checks did not run conformance at all.
  h1 32/32 on four targets, h2 146/146, h3 47/49.
- **A check that cannot measure now fails and does not pass.**
  `tools/conformance.sh` broke that rule four separate ways and reported success through all of them.
- **clippy is silent**, `-D warnings`, on both toolchains, and the per-platform ceiling files are deleted.
  That ceiling turned out to be per-clippy-version: 45 findings at 0.1.95 against 123 at 0.1.98 on identical source.
- **Zero compiler warnings**, release and test builds, checked on Linux.
- **CI on GitHub Actions** on every push and PR, and `cargo-deny` now runs on the build host too, because a check that lives in one place only is not a check.
- **Six reference backends** in C, C++, Python, Go, and Rust with and without m6-core, all conforming to the wire contract, with 13 shared tests running them in the build checks.
  The multi-language promise was written down and never exercised until now.
- **The examples repository is built by m6's own checks**, and its CMS example's end-to-end suite runs against the binaries those checks just built.
  Nothing had ever built it.
  By the time anyone looked it did not compile at all: every renderer crate still pointed at `m6-render`, a crate m6 had deleted, and the binaries left in `target/release` from before the deletion meant running an example still appeared to work.

  Underneath that were five more, each invisible for the same reason.
  Every asset in every example returned 502, because m6-file became an `App` service and ten configs across the repository were left naming no handler, so the service exited 2 before binding.
  PATCH and DELETE were refused at the edge by `allowed_methods`, which looks exactly like a missing route.
  Unpublishing a post in the CMS example answered `{"unpublished": true}` and left the post listed and readable.
  Starting an example ran `pkill -x m6-http` and killed every other m6 on the machine.

  m6's own checks were passing throughout, and could not have caught any of it: m6 contains no site, and the examples are where its interfaces are actually used.
  `tools/perfcheck.sh` also measured a deployment's content through a deployment's rendered config, so m6 could not measure itself either, and it now renders two pages from the examples, whose content is committed, so the number means the same thing on every machine.
- **The CMS example's end-to-end suite has no skipped checks**, and 96, up from 46.
  The previous version had five that accepted a range of answers, and each one was hiding one of the defects above: the unpublish check read the API's own reply and not the public site, and the auth checks passed on "302 or 401 or 403".
  It also pinned `--http1.1` for everything, so the example that runs the whole stack never exercised HTTP/2 once.

### 5. Protocol fixes in this release

- **HTTP/2: a received GOAWAY no longer closes the connection**, so the frames behind it are answered and do not meet a TCP reset.
  A WINDOW_UPDATE that overflows a stream we have just answered is still a FLOW_CONTROL_ERROR and is not dropped.
  Both were intermittent, both came from discarding state the instant we finished with it, and h2 went from 2 failures in 6 runs to 20 out of 20 clean.
- **h3 37/49 to 47/49.**
  All twelve failures were upstream in quiche, and the recorded remedy, bump the version, moved the score by zero tests.
  Ten are cleared by two open upstream pull requests, applied on a fork pinned by revision.
  The last two are QPACK and are accepted.
- **Compression is the backend's job, and m6-http is a cache and not a transformer.**
  The protocol told backends not to compress on the grounds that the proxy would, and the proxy has no compressor: a C, Go or Python backend written from that advice served uncompressed bytes forever.
  `[[backend]] compresses` is now read by both sides, and a backend refuses to start if it disagrees with what it can actually do.
- **`touch site.toml` never reloaded anything on Linux.**
  m6-core's `Request::touch` is the documented way for a renderer to invalidate the edge after writing content, and it used `utimensat(2)`, which reports `IN_ATTRIB`.
  The inotify mask asked for `IN_CLOSE_WRITE | IN_CREATE | IN_MOVED_TO`, so the event was read and discarded, and the mtime fallback runs only when there is no watcher fd.
  macOS was fine, because kqueue reports the attribute change, so the defect was invisible where the code was written and live where it runs.
  Blog publishing was spared only because `m6-md --touch` has its own implementation that opens the file.
  The watcher now accepts `IN_ATTRIB` and `Request::touch` opens and closes the file as well, and either alone is sufficient.
  Found within the hour after the checks started building the examples on Linux.
- **A backend's error headers survive the error page.**
  m6-http replaces a backend's 4xx or 5xx with its own page and threw the backend's whole header block away with it.
  So a throttled login reached the client as a generic "An unexpected error occurred" with no `Retry-After`, even though m6-auth-server had set one: the throttle worked and was unusable, because the one header saying when to come back was the one discarded.
  The same substitution dropped `WWW-Authenticate` from a backend 401 and `Allow` from a backend 405, which RFC 9110 11.6.1 and 10.2.1 require.
  Those four headers now carry over and nothing else does, because the rest describe a body that is no longer being sent.
- **The login throttle counts failures, and not logins.**
  m6-auth-server incremented on every login request before looking at the credentials, and never cleared the count, so six logins in fifteen minutes locked the IP out whether or not any password was wrong.
  Counting successes stops no attack, since an attacker with the password does not need six attempts, and what it stopped was a person using three devices, and any test suite that logs in repeatedly.
  The limits move into `[rate_limit] max_attempts` and `window_secs`, defaulting to the previous 5 and 900.
- **Supply chain:** five `cargo-deny` exceptions down to three, each remaining one stating whether it is reachable and how that was checked.
  `rustls-pemfile` is gone from the tree entirely.

### 6. Known, recorded, and not fixed in 1.0.0

Stated here and not discovered later:

- **h3 is 47/49.**
  The two remaining are QPACK, quiche models neither code, and closing them means writing protocol validation into a forked dependency.
- **m6-http depends on a fork of quiche**, pinned by revision, carrying two unmerged upstream pull requests.
  Deliberate, and to be dropped for a tag when upstream releases them.
- **Linking m6-core costs about 36% of a backend's throughput** and 8.8x its resident memory on a trivial route.
  Measured, reproduced within 3%, and recorded in `docs/BENCHMARKS.md`.
  It is measured on the shape that maximises it, and behind the edge cache most requests never reach a backend.
- **One intermittent test-harness failure**, issue #9: a claimed TCP port occasionally turns out to be in use.
  Two failures in five full runs, the mechanism identified, the fix not yet written.
  It affects the test harness, and not m6.
- **`m6-monitor`'s `/traffic` and `/perf` shapes have moved** since anything was built against them, so a consumer written to the older shape will not find `pools` or a `firewall` field.
  Whether any given deployment runs it is that deployment's business, and not this repository's.

1.0.0 is the first release, and it records the work that made the checks real and the gaps that remain.

---

## 0.2.0 and earlier, 2026-03-15 and before

**The early releases are not split by version.**
This project kept one flat list of changes until 2026-09-13, when `main` became release-only and versions started to mean something.
Splitting the entries below across the versions they shipped in would mean guessing.
A changelog that guesses is worse than one that says it does not know.
Everything here is accurate about *what* changed.
The *which release* is the part that is not recorded.

From the next release onwards, each version gets its own heading.

---

## Content-Length on bodyless statuses (RFC 9110 8.6)

m6 emitted `content-length: 0` on every 304, on **all three protocols**.
Zero is the one value that actively misinforms.
The 200 for that resource is 16 KB, so the response told a client the representation was empty.
A cache updating its stored entry from that 304 could conclude the body it holds is the wrong length.

RFC 9110 8.6 requires a 1xx or 204 to carry no `Content-Length` at all, and a 304 to carry it only when the value equals what the 200 would have sent.
The header is now omitted for all of them, through one shared `status_may_have_content_length` used by the H1, H2 and H3 serialisers so they cannot drift apart.

Omitting is safe because a 304 has no body by definition and each protocol signals end-of-message its own way.
H1 signals by connection close or the next request boundary, and H2/H3 by END_STREAM.
Emitting the *correct* non-zero length would also be legal.
It would mean carrying the stored body's length through the 304 path for no benefit to any client.

**A test was pinning the bug.**
`empty_body_is_unchanged` asserted `content-length: 0` on a **204**.
It passed for as long as the violation existed and failed the moment it was fixed.
It is now split so each status asserts its own rule.
A 200 carries the header, and a 204 must not.
A test can pin wrong behaviour just as firmly as right behaviour, and this one did.

Found by reading a 304 off a raw socket against a running server.

694 workspace tests pass.
Zero warnings.


## Corrected initial age (RFC 9111 4.2.3)

Age was taken from the `Age` header alone.
That trusts an upstream to have set it.
An upstream cache that stores a response *without* emitting Age makes an hour-old response look brand new to us, and then to everyone downstream of us.
Each hop that does this restarts the clock.

The spec cross-checks against `Date`:

    apparent_age          = max(0, now - Date)
    corrected_initial_age = max(apparent_age, age_value)

Taking the max is conservative in both directions.
A missing or under-reported Age is corrected upward by Date.
A clock skewed so Date is in the future yields zero apparent age, leaving Age to stand.

`response_delay`, the third term in 4.2.3, is deliberately omitted as an approximation.
Including it means threading each request's start time through all 31 insert sites.
Measured, the backend round trip here is ~2ms against freshness lifetimes of 60s and 86400s.
That is 0.003% of the shorter one, and below the one-second resolution `Age` can express.
It would matter for a cache fronting a slow or distant origin.
It does not matter for this one, and the code says so and says when to revisit.

Verified alongside `set_date`, which only adds a Date when absent.
A stored response keeps its original generation time and the age chain survives across hops.
The fix composes with what was already there.

Six tests, including the two directions of the max, clock skew, and a malformed Date.
A malformed Date is ignored.
Reading it as the epoch would make every such response appear ~56 years old and instantly stale.

691 workspace tests pass.
Zero warnings.


## Request cache directives (F053-F056)

RFC 9111 5.2.1 directives sent by the CLIENT were not implemented at all.
The most visible consequence is a browser reload.
A browser reload sends `Cache-Control: no-cache` for a hard reload, or `max-age=0` for an ordinary reload, and m6 served the cached copy regardless.
A visitor had no way to force a refresh, whatever they pressed.

Implemented: `no-cache`, `max-age`, `min-fresh`, `max-stale` (bare and bounded), `only-if-cached`, and `Pragma: no-cache` as the HTTP/1.0 fallback.
`Pragma: no-cache` is honoured only when Cache-Control is absent, because Cache-Control is authoritative when both are present (RFC 9111 5.4).

`max-stale` is `Option<Option<u64>>`.
Bare `max-stale` means unlimited staleness, and `max-stale=60` bounds it, and a plain `Option<u64>` cannot express the difference.

`only-if-cached` is deliberately NOT encoded in the `Lookup` enum.
The lookup returns `Miss` as usual and each server path turns that into a 504.
Putting an HTTP status into a structure that otherwise knows nothing about HTTP would be the wrong seam.

**Consequence worth stating plainly:** honouring these makes reloads slower, by design.
A reload that previously replayed from cache in ~2us now reaches the backend.
On the origin that is a couple of milliseconds.
From a cache node it is a round trip to the origin.
That is what the client asked for, and it is the only way "force refresh" can work at all.
It is a real change in behaviour for reloads.

Twelve tests, covering the parse edge cases (quoted values containing commas, malformed numbers, Pragma precedence) and the behaviour against a live cache.

685 workspace tests pass.
Zero warnings.


## Accept-Encoding q-values, Via, and Connection-nominated fields (F012-F014, F017-F019)

**F012-F014, content negotiation ignored q-values.**
`choose_encoding` tested `accept_encoding.contains("br")`.
Substring matching, so `Accept-Encoding: gzip, br;q=0` still matched, and the response went out brotli-encoded to something that had said it could not accept it.
`q=0` means *not acceptable* (RFC 9110 12.4.2).
Preference was ignored too.
`gzip;q=1.0, br;q=0.1` chose brotli because `br` was tested first.

Replaced with a real parser.
Bare tokens default to q=1, `q=0` excludes, `*` supplies the quality for anything unnamed, an explicit mention overrides the wildcard in both directions, and the highest acceptable quality wins with ties falling to our own br-then-gzip preference.
Identity remains the fallback even against `identity;q=0`, which RFC 9110 12.5.3 permits.
Serving uncompressed beats a 406.

**F017, no `Via` on forwarded messages.**
m6 forwarded everything anonymously, so a request that looped back was indistinguishable from a fresh one.
The header is now appended, reporting the protocol the request was RECEIVED on.
h2/h3 say `2`/`3` even though they leave as HTTP/1.1, because Via describes the hop received.
An upstream proxy's Via is preserved and extended, never replaced.

The pseudonym is the fixed token `m6`.
RFC 9110 7.6.3 allows a pseudonym precisely so an intermediary need not disclose its internal naming.
`origin`/`edge-a`/`edge-b` would hand every client a map of the topology for nothing.
A test asserts Via leaks none of those names.

**F018/F019, `Connection`-nominated fields were forwarded.**
`Connection` lists fields that apply to this hop only.
The static hop-by-hop list covers the well-known ones, but a sender may nominate any field.
Forwarding one on lets a client smuggle a header past an intermediary that believed it had consumed it.
That is the same shape of confusion that makes request smuggling work.
The fields are now stripped in both serialisers.
`close`/`keep-alive`/`upgrade` are recognised as connection options.

Thirteen tests.
673 workspace tests pass, zero warnings.


## m6-http: RFC 9110 13.2.2 preconditions (F003, F006, F008, F010)

Only `If-None-Match` and `If-Modified-Since` were implemented, steps 3 and 4 of the evaluation order.
Steps 1 and 2 did not exist at all.

- **F006 `If-Match` and `If-Unmodified-Since` were ignored.**
  That is not a missing nicety.
  `If-Match` is precisely how a client avoids the lost-update problem, so silently ignoring it turned every conditional write into an unconditional one.
  The old return type was `bool`, which could express "304" and "carry on" but had no way to say **412**, so the two headers could not have been honoured without changing it.
- **F003 `If-None-Match` used strong comparison.**
  RFC 9110 8.8.3.2 specifies the *weak* function there, so `W/"abc"` must match `"abc"`.
  A client returning the weak validator it had just been given never matched, and was sent the entire body again.
- **F008/F010 evaluation order** is now the specified one: If-Match, then If-Unmodified-Since only when If-Match is absent, then If-None-Match, then If-Modified-Since only when If-None-Match is absent and the method is GET/HEAD.
  A matching `If-None-Match` on an unsafe method is 412.
  304 is meaningless as the answer to a state change.

`If-Match` correctly uses *strong* comparison, so a weak validator does not authorise a write even though it names the same entity.
There is a test for that asymmetry specifically.
An unparseable date is ignored (RFC 9110 13.1.4), or a malformed header would turn every request into a 412.

Wired into all four server paths, with the HTTP/3 site taking the method from its `:method` pseudo-header.
`is_not_modified` is kept as a wrapper for compatibility but documented as unable to express 412.

Eleven tests.
Note two of them initially failed for a reason worth recording.
The fixtures used weekday names that did not match their dates.
"Wed, 03 Sep 2026" is a Thursday, `httpdate` rejected them, and the date-based tests were silently exercising the unparseable-date path.

654 workspace tests pass.
Zero warnings on Linux and macOS.


## m6-http: strict chunked decoding (F032-F034), and a missing body cap

Every relaxation here was a way for this decoder and the next hop to disagree about where a body ends.
On a reused connection that disagreement means the remainder is read as the head of the following response.

- **F032** the CRLF after each chunk's data is now required.
  It was skipped only `if` it happened to be present, so a chunk whose data was followed by anything else silently resynchronised onto the wrong offset and the rest of the body was parsed as chunk headers.
- **F033/F034** the trailer section is parsed.
  The decoder returned at the zero-size chunk without confirming the message terminated, so a body truncated mid-trailer was indistinguishable from a complete one.
  Trailer field names are validated as tokens, so a malformed line is a parse error.

Two further defects found while implementing, neither in the audit:

- **Chunk sizes were not required to be plain hex.**
  `usize::from_str_radix` accepts a leading `+`, so a chunk declared `+A` parsed as 10.
  That is the same trap that had let `Content-Length: +5` through.
  The digits are now checked one by one.
- **The chunked path had no body cap.**
  `read_to_end` was unbounded, so while the `Content-Length` path refused to allocate above `MAX_BACKEND_BODY`, a chunked response could allocate without limit.
  That is the memory exhaustion the cap exists to prevent, reachable by choosing chunked framing.
  It is now bounded by the same limit.

Six tests.
The first asserts that well-formed bodies still decode, including chunk-ext, uppercase hex, an empty body and a trailer section.
The stricter parsing therefore did not reject valid messages.

643 workspace tests pass.
Zero warnings on Linux and macOS.


## m6-http: stop emitting two Content-Length headers

Found by reading a live HEAD off a raw socket after deploying the response framing work.

`build_response` wrote every upstream header verbatim and then appended its own `content-length`.
On an asset that produced:

    Content-Length: 16580     <- from the backend
    content-length: 0         <- from the serialiser

Two Content-Length fields with **different values** is exactly the framing ambiguity this proxy had just been taught to refuse *from* a backend (F028), being emitted *by* the proxy.
A downstream cache or browser picking the second would treat the body as empty.
HTML responses carried the duplicate too, but with matching values, so it looked merely cosmetic there.
Only the asset exposed the conflict.

Now any upstream `content-length` or `connection` is dropped before the serialiser writes its own, and the value is chosen properly.
`body.len()` is used normally, because the body may have been compressed after the backend set its length.
For a HEAD whose body is already empty the upstream length is used, because RFC 9110 9.3.2 requires a HEAD to report the length a GET *would* have returned.
Reporting 0 there tells a crawler the resource is empty.

Five tests, including that a GET's real body length still wins over a contradicting upstream header, and that ordinary headers are untouched.


## m6-http: strict framing for backend HTTP/1.1 responses (F028-F031, F035)

Message framing is the security boundary between this proxy and its backend.
If the two disagree about where a response ends, the leftover bytes become the head of the next response on a reused connection.
Each of these was a way to disagree, so each is now a hard error.

- **F028** conflicting `Content-Length`.
  The parser kept the last value seen, so a backend emitting two different lengths framed the response by whichever came last.
  Now every value present must agree, across repeated fields and within a comma-separated list, or the response is refused.
  Repeated but identical values still work.
  That is pinned by its own test so the fix cannot reject legitimate traffic.
- **F029** `Transfer-Encoding` together with `Content-Length` is refused.
  RFC 9112 6.1 forbids sending both, and a recipient that picks one is the classic smuggling primitive because the next hop may pick the other.
- **F030** `Transfer-Encoding` is a list and only the final coding frames the message.
  Only a value that was literally `chunked` was recognised, so a valid `gzip, chunked` was treated as unframed and the chunk envelope was returned as though it were the body.
  A `Transfer-Encoding` not ending in `chunked` now errors.
- **F031** an unparseable `Content-Length` is refused.
  It became `None` via `.ok()` and fell through to read-to-EOF, silently switching framing mode on malformed input.
  Digits are now validated explicitly (`1*DIGIT`, RFC 9110 8.6).
  `str::parse` accepts a leading `+`, so `Content-Length: +5` had been read as 5.
  That was caught by the test.
- **F035** responses to HEAD, and 1xx/204/304, are bodyless whatever their headers claim (RFC 9112 6.3).
  The reader did not know the request method and blocked waiting for `Content-Length` bytes a correct backend never sends, so every HEAD to a backend stalled until the read timeout.
  `read_response_for` takes the method, and `read_response` remains for callers that cannot know it.

Ten tests.
The bodyless rule is paired with a test that a GET with identical headers still reads its body, so it could not have been implemented by ignoring bodies generally.

632 workspace tests pass.
Zero warnings on Linux and macOS.


## m6-http: refuse to forward requests that would smuggle HTTP/1.1 framing (F036/F094)

Decoded HTTP/2 and HTTP/3 header fields were written into HTTP/1.1 request syntax verbatim.
H2 and H3 header fields are length-delimited, so a value may contain any byte.
HTTP/1.1 is CRLF-delimited, so a CR or LF in that value stops being data and becomes framing.
A client could therefore append arbitrary headers, or a whole second request, to what the backend received.

This is the egress half of the bare-LF class fixed earlier on ingress, and the worse half.
On ingress a malformed request is the client's problem.
Here the proxy generates the malformed bytes itself and does so with the backend's trust behind it.

`check_forwardable` now gates both forwarding paths (unix socket and URL upstream) before a connection is opened, and covers everything written into the request line and header block, not just the header loop:

- field values: no CR, LF or NUL
- field names: RFC 9110 5.6.2 tokens, so a colon or space cannot split a header
- method: no space (a space forges the request line without any CR)
- path and query: no CR, LF, NUL or space
- `X-Forwarded-For` / `X-Forwarded-Host`, both derived from client-controlled input and written by the proxy itself

Refused.
RFC 9113 8.2.1 makes such a message malformed, and silently rewriting a request before handing it to a backend hides an attack.
Hop-by-hop headers are exempt because they are dropped before serialisation and cannot reach the backend.
That exemption is pinned by a test so the skip list and the check cannot drift into a hole.

Six tests, verified failing with the check disabled.
Five of the six fail, and the sixth (legitimate traffic still passes) correctly does not, which is what makes it worth keeping.
obs-text is explicitly allowed, so the fix cannot regress into "reject anything non-ASCII".

622 workspace tests pass.
Zero warnings on Linux and macOS.


## m6-core: a config reload no longer silences logging

`LogHandle::reload` rebuilt the whole layer, a new writer, new `fmt` layer, new `.with_filter(..)`, and handed it to `reload::Handle::modify`.
That is unsupported.
`.with_filter(..)` produces a `Filtered` layer, per-layer filter ids are assigned when the subscriber is *constructed*, and a `Filtered` layer swapped in afterwards has no id and panics on the first event through it.

In production the symptom was not a crash.
m6-http kept serving traffic and reporting itself healthy while every log target except `analytics` went silent.
Stats, pool events, warnings and errors all went.
`analytics` survived only because it is a separate layer with its own writer that reload never touches.
Measured on one node across the same PID: stats 1788 -> 0, m6_http 124 -> 0, pool 74 -> 0, rustls 60 -> 0, analytics 954 -> 42.
A deploy touches site.toml and that triggers a reload, so every deploy blinded the server to its own errors.

Now only the FILTER is reloaded.
The `Filtered` wrapper is built once at registration and keeps its id for the process lifetime.
Only the filter value inside it swaps.
The stdout writer is likewise created once.
Format cannot change this way (json and text are distinct layer types), so a reload requesting a different format keeps the current one and warns.
The level, which is the knob that gets used, still applies.
A failed reload now logs at ERROR.

Test: `m6-core/tests/log_reload.rs`.
The subscriber is process-global and cannot be torn down, so it re-executes the test binary as a child and inspects real stdout.
It asserts a line before the reload, so a pass cannot be vacuous, then after one reload, then after a second.
Asserting that `reload()` returned would prove nothing.
It returned cleanly the whole time it was writing into a dead layer.
Verified failing against the old code with the exact production panic, then passing.


## m6-file: lengthen only the shared-cache lifetime for unversioned assets

Unversioned assets (the webfont, `manifest.json`, any asset requested without its `?v=` hash) carried `max-age=60, stale-while-revalidate=60`.
On a low-traffic origin the edge entry expired faster than requests arrived, so the cache re-fetched them from the backend roughly once a minute.
Measured at a 45% asset hit rate, with the webfont at 21 misses to 14 hits.

Added `s-maxage=86400`.
Deliberately **not** a longer `max-age` or `stale-while-revalidate`.
Both are honoured by browsers, and a browser cache cannot be invalidated, so raising either strands visitors on the previous file for a day.
That was tried before and rolled back.
`s-maxage` is defined for shared caches only (RFC 9110/9111 5.2.2.10) and is ignored by browsers, so it lengthens exactly the copy an operator can evict.

Versioned `?v=<hash>` assets keep `max-age=31536000, immutable` and are unchanged.

The rule moved into `cache_control_for()`.
It had been an inline expression, and the tests asserted a *copy* of the condition pasted into the test module, so the emitted directives were never covered.
The browser-facing window could have been lengthened with every test still green.
Tests now call the real function and assert the exact strings, plus the invariant that the long window appears only on `s-maxage`.
Both were verified to fail against the previous directive and against the rolled-back mistake.
608 workspace tests pass.
Zero warnings on Linux and macOS.


## 2026-09-06, `Date` on every response, and complete 304 metadata

### 1. `Date` (F001)

RFC 9110 6.6.1 MUST.
m6 has a clock and generated no `Date` on anything.
A recipient cannot compute a response's age without it, which is why the `Age` header added earlier was not sufficient on its own.
The two are only useful together.

**Stamped before the cache insert.**
This is the part worth recording.
`Vary` is deliberately applied *after* the insert so `should_cache` sees the backend's own value.
Copying that pattern for `Date` produced a cache hit whose headers had no `Date` at all.
Adding a fresh one on the hit path would have been worse.
It would claim the response was generated just now while the `Age` beside it said sixty seconds.
`Date` means generation time, so it has to be captured at generation and stored.

Verified end to end: MISS carries `Date` and no `Age`, and a hit three seconds later carries the **same** `Date` and `Age: 3`.

### 2. 304 responses were missing their metadata (F004, F011, F062)

RFC 9110 15.4.5 requires a 304 to carry the metadata a 200 would have, so a client can update its stored response from it.
m6 sent only `ETag`, `Last-Modified` and `Cache-Control`.

`Vary` was the damaging omission.
A client or shared cache updating its stored entry from such a 304 loses the knowledge that the response varies by `Accept-Encoding`, and can then reuse a brotli body for a gzip-only request.
`Date` was missing too, leaving the recipient nothing to compute age from.

The response now carries `Vary`, `Date`, `Age`, `Expires` and `Content-Location` alongside the validators.
`Content-Length` is deliberately **not** carried.
RFC 9110 8.6 permits it on a 304 only when it equals the 200's length, and getting that wrong is worse than omitting it.

A second bug surfaced while fixing this and is worth naming.
The 304 path reads from the *stored* headers, so it inherited the same gap.
`Vary` and `Date` were absent there for the same reason.
Adding them at the three 304 construction sites was required as well as fixing the filter.
A filter that copies fields correctly is no use when the source never had them.

604 workspace tests pass across two consecutive runs, zero warnings.

---

## 2026-09-06, `Expires`-based freshness (F047)

`Expires` was not consulted at all.
A response using the older header, still perfectly valid and what a great deal of software emits, fell straight through to "no freshness given" and was treated as fresh indefinitely.
That is exactly backwards: it carried an explicit expiry and the cache ignored it.

Now implemented in the precedence RFC 9111 4.2.1 requires: `s-maxage`, then `max-age`, then `Expires - Date`, then the bounded heuristic.

Measured against the **response's own `Date`**.
Using local time would silently lengthen or shorten the lifetime by however much the two servers' clocks disagree.
That is a real effect on a cache sitting between two machines.
A missing `Date` falls back to now.
An `Expires` at or before `Date` means already stale, and yields zero.

Five tests, including clock skew (Date and Expires both shifted an hour, and the interval is still honoured), `max-age` outranking `Expires`, a past `Expires`, and unparseable garbage falling back to the heuristic.

601 workspace tests pass across two consecutive runs, zero warnings.

---

## 2026-09-06, Bounded heuristic freshness, and unsafe-method invalidation

### 1. `Cache-Control: public` no longer means fresh forever (F048)

A response with no explicit lifetime stored with `expires_at = None` and stayed fresh indefinitely.
That was the deliberate CDN-style model the deploy pipeline relies on.
Content lives until `invalidate-cache.sh` clears it.
"Forever" is not a defensible reading of RFC 9111 4.2.2, which permits a *heuristic* freshness lifetime.
An entry whose invalidation was missed for any reason would be served indefinitely.

Bounded at 24 hours.
The model is intact: deploys invalidate far more often than that, so in normal operation nothing expires that the pipeline was not going to clear anyway.
What changes is that a missed invalidation is now a bounded fault.
Nothing this site serves depends on it.
Every route and asset carries an explicit `max-age`.

### 2. A successful POST/PUT/DELETE now invalidates the cached URI (F058)

RFC 9111 4.4 MUST.
Nothing invalidated anything.
After a successful state-changing request the cache kept serving the previous representation until it expired on its own.
That is a live concern the moment the CMS returns.
Edit a page, and the edge keeps serving the old one.

Two deliberate restrictions:

- **Only on a non-error status.**
  A 4xx/5xx means the state change did not happen, so the cached copy is still correct.
  Invalidating on failure would also hand anyone a trivial way to flush the cache by spamming failing POSTs.
- **`Location`/`Content-Location` only when same-origin.**
  An off-site redirect target is not ours to evict, and following one blindly would let a backend clear arbitrary entries.

Safe methods (GET, HEAD, OPTIONS, TRACE, per RFC 9110 9.2.1) change nothing and are skipped.
Everything else counts as state-changing, including methods this server does not itself implement.

### 3. A third flaky test, same shape as the others

`m6-html`'s `spawn_server` waited for the socket *file* to appear, which is not the same as the server accepting on it.
Under a full run the connect raced the listen backlog, and `http_request` returns an empty string on I/O error.
Its comment says "so callers can retry rather than panic", but no caller retries, so an assertion on `"200 OK"` failed against `""`.
The test now polls with a real request until the server answers.

That is the third instance of the same mistake in this codebase's tests.
They waited for a proxy for readiness.

596 workspace tests pass across three consecutive runs, zero warnings.

---

## 2026-09-06, `Age` on cache-served responses (F045, F046)

RFC 9111 5.1 requires a shared cache to send `Age`.
Nothing emitted it at all, so a downstream cache had no way to know how old what we handed it already was and treated a minute-old response as newly generated.

Two halves, and the second is the one that matters in a chain:

- **`Age` is now emitted** on every cache-served response, at all three cache-hit sites.
  Verified live: absent on MISS, 3 after three seconds, 8 after eight, exactly one header.
- **Upstream age carries forward** (F046).
  RFC 9111 4.2.3 defines age as time since the response was *generated*.
  An entry that arrived already one hop old had its clock reset to zero, so each hop in a chain made the content look fresher than it was.
  The `Age` the origin declared is now folded into the stored entry and added to the time held locally.

The age travels on the `Lookup` result.
The value changes every second, the stored headers are shared behind an `Arc`, and the callers already build an owned header vector to add `Vary`/`alt-svc`.
This costs nothing on the cache-hit path.

Four tests, including a malformed upstream `Age` being ignored.

Still open in this area: `Expires`/`Date`-based freshness (F047), bare `public` being fresh forever (F048), and unsafe-method invalidation (F058).

594 workspace tests pass, zero warnings.

---

## 2026-09-06, 501 vs 405, case-sensitive methods, and two more allocation bounds

### 1. An unrecognised method is 501, not 405 (F016)

RFC 9110 15.5.6 vs 15.6.2: 405 means the method is *known* and the resource will not do it, and MUST carry `Allow`.
501 means the server does not implement the method at all.
Everything got 405, which claims knowledge of an invented verb and points the client at the resource when the method is the problem.

This one was mine, introduced with the method gate earlier the same day.

### 2. The method gate could be bypassed by changing case

Found while testing the above, and the more serious of the two.

`method_may_read_cache` compared with `eq_ignore_ascii_case`, while the configured allow-list compares exactly.
The cache lookup runs **before** the method gate, so a request with method `get` matched the cache predicate, was served from cache, and never reached the check that would have refused it.

RFC 9110 9.1 makes the method token case-sensitive, so `get` is simply not GET.
Both cache predicates are now exact matches.
The general lesson is the one worth keeping.
Two comparisons of the same thing, in different places, that disagree about case is a bypass waiting to be found.

Verified on a raw socket: `get`, `Get` and `head` now return 501 where they previously returned 200 from cache.

### 3. `HTTP/1.1 501 Unknown`

501 was missing from the reason-phrase table and fell through to the default, so the server emitted a real status with a phrase describing nothing.
Added, along with 504.

### 4. Two more unbounded allocations (H003, H004)

- **H003, the sharper one.**
  `vec![0u8; len]` allocated the backend's declared `Content-Length` *before reading a byte*, so a backend declaring 4 GB allocated 4 GB immediately.
  It is now refused above 128 MiB, before allocating, since the allocation is the damage.
  The bodyless read-to-EOF path was unbounded the same way and is now capped too.
- **H004.**
  H3 request bodies accumulated with no ceiling, the same defect fixed for h2 earlier.
  The same 20 MiB limit applies so the two protocols cannot disagree about what is acceptable.
  Over-limit streams get a 413 and are reset.

590 workspace tests pass across two consecutive runs, zero warnings.

---

## 2026-09-06, HTTP/2 frame validation: a remote panic, and flow-control accounting

From the RFC audit, revision 1.
The first item is the reason this jumped the queue.

### 1. A three-byte frame could kill the process (F076)

```rust
if flags & FLAG_PRIORITY != 0 { pos += 5; }
let header_block = &payload[pos..];        // panics when payload.len() < pos
```

A HEADERS frame declaring PRIORITY but carrying fewer than the five bytes the priority fields require sliced out of range and aborted.
Reachable immediately after the connection preface, with no other setup and no valid request.

Verified: restoring the original line makes the new test fail with `range start index 5 out of range for slice of length 0`.
A first attempt at that verification was itself wrong.
A partial revert left the new padding bounds-check in place, which also guards the slice, so the test passed and looked vacuous.
Reverting to the exact original line showed the panic.

Now a FRAME_SIZE_ERROR, per RFC 9113 6.2.

### 2. Padding was fed to the HPACK decoder (same six lines)

`&payload[pos..]` ran to the end of the payload, so trailing pad bytes were passed to HPACK as though they were field data.
The header block is now `payload[pos .. len - pad]`.

### 3. Flow-control accounting (F078-F081)

- **RFC 9113 6.9.1: the whole payload counts against flow control**, padding and pad-length byte included.
  Only `data.len()` was charged, so a peer padding heavily reclaimed credit it never spent and the two ends' views of the window drifted apart.
- **No per-stream receive window existed at all** (F080).
  Only the connection window existed, so a single stream could consume the entire connection's credit.
  `H2Stream` now carries one.
- **An empty DATA frame emitted `WINDOW_UPDATE` with increment 0**, which is itself a PROTOCOL_ERROR (F081).
  Never emitted now.
- **`PADDED` with an empty payload** was silently treated as unpadded (F078).

### 4. Request bodies were unbounded (H001)

Not an RFC clause.
A robustness finding the audit listed separately, and one of the two it rated Critical.
Bodies accumulated with no ceiling while flow-control credit was returned, so a peer could stream indefinitely and grow the process until it was killed.
Capped at 20 MiB, above the renderers' own 16 MiB multipart limit, so it never trips before their check does.

### 5. Tests

Seven, including a sweep of every frame type against a spread of flag combinations, payload lengths and stream IDs, asserting only that nothing panics.
That property is the one that matters here.
A remote peer controls every byte, so any panic is a remote kill.
It is also what the audit asked for under "fuzz targets for every frame parser".

587 workspace tests pass, zero warnings.

---

## 2026-09-06, Reject a bare LF in the header block, and a raw-socket robustness suite

### 1. The defect

`httparse` accepts a lone LF as a line terminator, so a header **value** containing a raw `\n` was silently split into two headers:

```
X-Test: a\nX-Injected: yes   ->   X-Test: a   +   X-Injected: yes
```

and the second was forwarded to the backend.
No HTTP client will send this, which is why it survived.
It is only reachable from a raw socket.

**Honest scope.**
This is not a demonstrated exploit against the current topology, and the CHANGELOG should not imply otherwise.
Ingress stripping runs *after* the split, so proxy-owned headers (`X-Forwarded-For`, `X-Auth-Claims`) are still removed.
m6-http re-serialises with CRLF, so a cache node forwarding to the origin cannot desync with itself.
The hazard is the classic one from RFC 9112 11.2, two hops disagreeing where a header ends.
It goes live the moment anything is placed in front of m6.
Current hardened servers reject it, so m6 now does too.
One scan of the already-parsed header block rejects any LF not preceded by CR.
The body is excluded, so legitimate LF bytes in a payload are unaffected.

### 2. The suite that found it

`m6-http/tests/robustness.rs`, 17 tests over raw TLS sockets against a real `m6-http`, covering the gaps left by `security_regressions.rs` (function level) and `redirect.rs` (the `:80` listener):

- **Framing/smuggling**: conflicting `Content-Length`, `CL` + `Transfer-Encoding`, malformed lengths (negative, non-numeric, overflowing), space before the header colon.
- **Injection**: CR/LF/NUL in the request target, in header values, and in `Host`, illegal header names, response-splitting assertions on every case.
- **Bounds**: a 128 KB header value, 5,000 headers, a 64 KB request target.
- **Incomplete input**: connect-and-send-nothing, a dribbled and abandoned request, a declared body that never arrives, and eight concurrent half-open connections which must not delay a normal request (the slowloris class the old Python `:80` shim was vulnerable to).
- **Malformed request lines**: ten shapes including absolute-form, CONNECT and raw control bytes.
- **Application injection**: SQLi/XSS/template/JNDI-shaped queries, overlong UTF-8, double-encoding, and six path-traversal encodings.

The tests assert properties.
They assert no smuggling, no injected header in the response head, no hang and no crash, with a known-good request after every case to prove the server is still healthy.
A deliberate diagnostic test prints what the server actually returns, so a suite of vacuous passes (every reply empty) cannot masquerade as coverage.
It shows 400 for every framing abuse, 404 for traversal, and 405 for CONNECT.

### 3. Also observed, not yet changed

An empty `Host:` header is answered 200.
RFC 9112 3.2 requires 400 for a missing or invalid `Host` in HTTP/1.1, and an empty value is invalid.
Left alone for now because the redirect path validates the host separately and nothing reflects it.
Recorded.

544 workspace tests pass, zero warnings.
The robustness suite is 17/17 serially.

---

## 2026-09-06, `Last-Modified` on rendered HTML

The last of the three caching defects from the owner's audit.
m6-html emitted an `ETag`, so `If-None-Match` validated correctly, but there was no `Last-Modified` at all.
A client or tool validating by date got the entire page back every time.
`/` is 52 KB, which is exactly the figure the audit reported.

**The hard part was deciding what value is truthful.**
A rendered page has no file of its own to stat.
It does have inputs, and their mtimes are an honest answer:

- **Templates, pooled.**
  They include each other, and `_head.html`, `_banner.html` and `_footer.html` are on every page, so the newest template dates every route.
  Resolving the transitive include set per route would be a lot of machinery to make one date slightly tighter.
  Pooling errs toward revalidating, which costs a request.
- **Params, per route.**
  This is where precision pays: editing a publication should not make `/capabilities` look modified.
  Visible in the live output: `/` reads its own `timeline.json` mtime while the other pages read the template floor.

Computed once when the framework state is built and recomputed on reload, so it costs nothing per request.
Emitted only on a success: a validator on a 404 or a 500 invites a client to revalidate an error as though it were content.

**Omitted where no honest value exists.**
A code route, whose handler computes its answer at request time, and a route whose params path carries a `{placeholder}`, which resolves per request.
RFC 9110 permits omitting the header, and a wrong date is far worse than an absent one.

No change was needed in m6-http.
`cache::is_not_modified` already checked `If-Modified-Since` against a cached entry's `Last-Modified`.
It simply never had one to compare against for rendered HTML.

The directory walk is depth-bounded (8) so a symlink loop under the site directory cannot hang startup, and that bound is tested.

544 workspace tests pass, zero warnings.
Verified locally: header present on every page, `If-Modified-Since` with the exact value returns 304, with an epoch date returns 200 and the full 16,678 bytes.

**Deploy note:** m6-render is a path dependency of **four** binaries, m6-html, render-contact, render-analytics and render-cms.
No deploy script covered `m6-html`: neither `deploy-platform.sh` nor `deploy.sh --binary`, whose loop is only the three site-repo renderers.
That is the same multi-binary trap that took `/contact` down previously.
`deploy-platform.sh` now builds, installs and restarts m6-html alongside m6-http and m6-file.

---

## 2026-09-06, Method validation and HEAD framing

Two coupled defects from the owner's audit, fixed together because fixing either alone is worse than fixing neither.

### 1. Every method was served the cached page

Verified live before the fix, warm and cold, on both protocols:

```
GET/HEAD/POST/PUT/DELETE/PATCH/OPTIONS/TRACE/FOO  ->  200, 54,361 bytes
```

Including TRACE, and including an entirely invented `FOO`.
`cacheable` was derived from the route's auth requirement alone.
`make_lookup_key` had no method component, and nothing downstream inspected the method either, so m6-html rendered whatever it was handed.
`m6-file` returned 405 for non-GET/HEAD of its own accord, which is why only HTML routes were affected and why spot-checking an asset always looked fine.

Now `[server].allowed_methods` (default GET, HEAD, POST) is checked before routing, cache lookup or backend dispatch, and anything else gets 405 with an `Allow` header.
POST is admitted because the contact form needs it, and is never cached.
Configurable because the disabled CMS routes need PUT and DELETE when they return.

**Deviation from the audit, stated deliberately.**
It recommended including the method in the cache key.
That would touch 30+ call sites and would stop HEAD sharing GET's entry.
HEAD sharing GET's entry is what makes HEAD cheap, and is the shape the audit itself preferred.
The same property is obtained by construction instead.
`method_may_read_cache` admits GET and HEAD, `method_may_write_cache` admits GET alone, so the key namespace can only ever hold GET representations and no unsafe method can read or write one.
Pinned by tests.

### 2. HEAD responses were malformed, not merely bodied

```
HEAD /capabilities -> 200, Content-Length: 16738, and 16,738 body bytes
```

Not just oversized.
The response advertised the GET representation's length and then mis-terminated, so curl reported *"transfer closed with N bytes remaining"* (exit 18) and HTTP/2 aborted the stream with `INTERNAL_ERROR`.
Health checks, link validators, crawlers and uptime monitors all use HEAD, so every one of them was either transferring the whole page or erroring.

`curl -I` hid all of it.
It parses the response and discards the body, so every hand check looked clean.
Only a raw socket read shows it.
That is why the new tests assert on raw bytes.

Fixed at all four serialisation points (h1 `build_response`, h2 sync and async dispatch, h3 `send_h3_response`).
`Content-Length` still describes what a GET would have returned, and no body is sent.

### 3. Why the order mattered

While HEAD wrongly returned a full body, the entry it stored was byte-identical to a GET entry, so the method-less key was harmless.
Fixing the HEAD body alone would have stored a *bodyless* response under the key a later GET reads.
That turns a protocol violation into silent content loss.
The write gate lands in the same commit.

Verified against a local stack, on a raw socket, in the sequence that would expose it: cold GET full body, warm HEAD zero body with the correct Content-Length, warm GET still the full body, and separately a **cold HEAD followed by a GET**, which returns the full page.

539 workspace tests pass, zero warnings.
`curl -I` now exits 0 on both protocols.
POST still reaches the contact backend, and all pages 200.

---

## 2026-09-06, `Link: rel="describedby"` on HTML responses

New `[site].describedby` setting.
When set (the site uses `/llms.txt`), every **HTML** response carries `Link: </llms.txt>; rel="describedby"`.

The site already emits the equivalent `<link rel="describedby">` in `<head>`.
The header is the better of the two and there is no reason to have only one.
It arrives before the page is parsed, so anything inspecting response headers on its first request discovers the site's machine-facing layer without reading any markup.
The markup link covers readers that only see the document.

HTML only.
`llms.txt` describes the *site*, and a stylesheet or a PNG claiming to be described by it says nothing useful and would add a header to the majority of requests, which are assets.

Applied at the same five places as `Vary`: the two miss-path wrappers and the three cache-hit replay sites.
Empty by default, so a deployment with no such file does not advertise one.

Two hazards, both covered by tests.
A cache node's backend is the origin, which already added this header before the response was forwarded and cached, so the node would emit two without a dedupe.
`Link` is also carrying the preload hints.
The dedupe matches on `rel="describedby"` specifically, so the preloads survive.

529 workspace tests pass, zero warnings on both platforms.
Verified locally on HTML for cache MISS and HIT, and verified absent on CSS and on llms.txt itself.

---

## 2026-09-06, HTTP caching correctness

Three defects in the revalidation and caching headers, all raised from a live audit of a production deployment, then reproduced and root-caused here.
None is cosmetic.
Each costs bandwidth or risks a downstream cache serving the wrong bytes.

### 1. `Vary: Accept-Encoding` was emitted only when replaying a cache hit

Same URL, back to back:

```
req1 (MISS) -> vary: <absent>
req2 (HIT)  -> vary: Accept-Encoding
```

This is the worse of the two orderings.
The first client to request any URL is the one that gets the wrong headers, and that is every fresh visitor, plus every downstream shared cache at the moment it populates itself.

The header was never being stripped on the forward path, as first assumed.
It was simply only ever added on the hit path.
Both backends were affected, at both tiers.
m6-file emits no `Vary` of its own, and neither does m6-render.

Fixed by applying it once in a thin `handle_request` wrapper covering all four server paths and all eleven of the inner function's `Ready` returns, plus a matching wrapper on `finalize_url_response` for the async URL-backend completion path, which bypasses the first one entirely.

**`set_vary_accept_encoding` now merges, and that matters.**
It used to drop every existing `Vary` and write `Accept-Encoding` in its place.
That was harmless only while it ran solely on the cache-hit path, where `should_cache` had already refused anything varying on more than encoding.
Called on the miss path, an overwriting version would rewrite a backend's `Vary: Cookie` to `Vary: Accept-Encoding`, `should_cache` would then see a cacheable response, and one client's private variant would be stored and replayed to everyone.
Preserving the other field names is what keeps such a response uncacheable.
`Vary: *` is passed through untouched.

Seven tests, including the merge case asserted end to end against `should_cache`.

### 2. All content codings shared one strong ETag

```
br        etag="6a9bdf1d-140ca"   7,246 bytes
gzip      etag="6a9bdf1d-140ca"   7,576 bytes
identity  etag="6a9bdf1d-140ca"  45,355 bytes
```

Three representations, three different byte strings, one validator asserting they are the same thing.
RFC 9110 requires a strong validator to identify the representation actually sent.
Combined with the missing `Vary` above, a downstream shared cache holding the brotli entry could legitimately match that tag against a gzip-only client and hand back a body it cannot decode.

The coding is now folded into the tag (`-br` / `-gz`), and identity keeps the historical unsuffixed form so already-cached URLs do not spuriously miss.
Content negotiation moved above the conditional check to make this possible.
It reads the path and request headers only, never the file, so a 304 still skips the read, minify and compress entirely.

Four tests, including the pairing that matters: a brotli ETag validates a brotli request (304) and does **not** validate a gzip one (200).

### 3. A failed compression was served mislabelled

Found while fixing the above, in the same lines.
`compress_brotli(&data, lvl).unwrap_or(data)` discarded the error and fell back to the uncompressed bytes **while still setting `Content-Encoding: br`**.
That is a body no client could decode, announced as one it could.
Silent, with no log line.

Another instance of the pattern worth naming across this codebase.
m6 fails open in several places (minification, compression negotiation, cache refresh).
Each is the right behaviour and each is invisible when it triggers.
The fallback now drops the label, strips the coding suffix from the ETag, and warns.

### 4. Verified

524 workspace tests pass, zero warnings.
Both fixes measured end to end against a local dev stack before deploying.
`Vary` present on MISS and HIT for both backends.
ETags distinct per coding.
Negotiation sizes unchanged (br 7,372 / gzip 7,694 / identity 45,792).
A conditional GET returns 304 for the matching coding and 200 for a mismatched one, and all pages still 200.

### 5. Still open from the same audit

- Rendered HTML carries no `Last-Modified`, so `If-Modified-Since` never validates and returns the full page.
  Strictly RFC-legal with no modification date available, but it costs a real transfer.
  Needs a truthful value derived from the render inputs.
- Method confusion and HEAD framing.
  Must be fixed together: today a HEAD wrongly returns a full body, so the entry it stores is byte-identical to a GET entry and the method-less cache key is harmless.
  Fixing the HEAD body alone would store a bodyless response under the shared key and serve the next GET an empty page.

---

**This file records each m6 release, newest first, and each entry says what changed, why it mattered, and how it was verified.**
Entries before 1.0.0 sit in one section, `## 0.2.0 and earlier`, for the reason its framing states.
Later entries carry their version in their heading and repeat the same subsection names, so a reader finds the same facts in the same place across releases.
