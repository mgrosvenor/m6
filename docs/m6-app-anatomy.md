# Anatomy of an m6 service

An m6 service is a `main` with its own event loop, plus the parts `m6-core` supplies.
This document shows that shape and the four contracts a service honours: the command line, the configuration, the routes and the lifecycle.
It is written so it can be followed from another repository.

§2 shows the two files in full.
§3 to §6 give the four contracts.
§7 is the test that proves the lifecycle in one call, §8 lists what the parts give a service, §9 covers building against `m6-core` from another repository, and §10 summarises.

## Contents

1. [What a service is](#1-what-a-service-is)
2. [The two files](#2-the-two-files)
3. [The command line](#3-the-command-line)
4. [The configuration](#4-the-configuration)
5. [Routes, handlers and state](#5-routes-handlers-and-state)
6. [The lifecycle](#6-the-lifecycle)
7. [Proving the lifecycle in one call](#7-proving-the-lifecycle-in-one-call)
8. [What the parts give a service](#8-what-the-parts-give-a-service)
9. [Building against m6-core from another repository](#9-building-against-m6-core-from-another-repository)
10. [Summary](#10-summary)

---

## 1. What a service is

**An m6 service is a Unix socket server whose `main` runs one event loop.**
`m6-core` supplies the parts: the reactor, the signal descriptor, the deadlines, the connection slot, routing, the request dictionary, configuration, compression, minification, logging and the shutdown lines.
The service supplies its routes, their bodies, and the loop that ties the parts together.

`m6-html`, `m6-file`, `m6-auth-server` and `m6-monitor` have this shape.
`m6-http` faces the internet, terminates TLS, HTTP/2 and HTTP/3, owns the response cache and proxies to these services over their sockets.
`m6-md` and `m6-auth-cli` are command line tools with no socket.

A process passes through three phases, and Table 1 says what each may do.

| phase | when | blocking |
|---|---|---|
| start | from `main` until the loop begins | allowed: read config, load templates, open the database, resolve host names |
| run | serving requests | forbidden: every wait goes through the loop |
| stop | after the loop exits | allowed: flush, unlink the socket, exit |

**Table 1: the three phases of a process, and what each may block on.**
Take from it that a blocking call belongs in start or stop, and that the run phase never blocks, because one blocked loop stalls every connection in the process.

Table 2 says which document owns each part of the subject.

| document | what it owns |
|---|---|
| `m6-architecture.md` §7.1, "Why each service writes its own loop" | why the shape exists, and why start, stop and the loop belong where they do |
| `m6-core-reference.md` | every module, every feature flag, and the full interface of `Request` and `Response` |
| this document | the contract a service honours, and the code that honours it |
| `m6-backend-protocol.md` | the wire contract the edge requires of any backend, in any language |

**Table 2: where each part of the subject is written down.**
Take from it that this document holds the shape and the contract, and that a question of the form "why is it like this" is answered in `m6-architecture.md` §7.1.

§2 starts the contract with a complete one-route service.

---

## 2. The two files

**A service is a `Cargo.toml` and a `main.rs`.**
The manifest holds one m6 dependency, and the `main` holds the three phases.
Both are shown here in full, and §3 adds a one-line `build.rs`.

The manifest is the first half.

```toml
[package]
name    = "my-service"
version = "0.1.0"
edition = "2021"

[[bin]]
name = "my-service"

[dependencies]
m6-core = { path = "../m6-core" }   # see §9 for outside this repository
anyhow  = "1"

[dev-dependencies]
m6-core  = { path = "../m6-core", features = ["testkit"] }
tempfile = "3"
```

`m6-core` is the only m6 dependency, and `anyhow` is there because the `main` below returns `anyhow::Result`.
`m6-http/tests/backends/rust-m6core` is a five-route service whose `main` returns `std::io::Result` and whose only dependency is `m6-core`.
`templates` and `minify` are on by default and pull tera, comrak, minify-html and minify-js.
A service that renders no templates and minifies nothing sets `default-features = false` and links none of them, and a configuration that then turns minification on is refused at start.
`testkit` goes in `[dev-dependencies]` only, which keeps the test harness out of every release binary.
`m6-core-reference.md` §2, "Feature flags", is the full feature table.

The `main` is the second half.
Below is a one-route service with the shape every service follows: start, run, stop.

```rust
use std::io;
use std::time::Duration;

use m6_core::prelude::*;
use m6_core::signal::{Action, Lifecycle};
use m6_core::site::finish_reload;
use m6_core::{genid_failed, genid_ok};

const ROUTES: &[CodeRoute] = &[CodeRoute { pattern: "/healthz", method: RouteMethod::Get }];
const LISTENER: Token = Token(usize::MAX);
const SIGNALS: Token = Token(usize::MAX - 1);
const LOG: Token = Token(usize::MAX - 2);
const TICK: Duration = Duration::from_secs(1);
const READ_CAP: usize = 16 * 1024;
const WRITE_CAP: usize = 64 * 1024;

fn main() -> anyhow::Result<()> {
    // Start: blocking and allocation are fine.
    m6_core::signal::block();
    let mut site = Site::load("my-service", ROUTES, &[], &NoTemplates);
    let read_timeout = site.read_timeout();
    let mut poll = Poll::new()?;
    let mut signals = Signals::new()?;
    let mut listener = listen_unix(site.socket_path(), site.socket_mode())?;
    poll.registry()
        .register(&mut listener, LISTENER, Interest::READABLE)?;
    poll.registry()
        .register_fd(signals.raw_fd(), SIGNALS, Interest::READABLE)?;
    let mut lifecycle = Lifecycle::new(site.name(), Some(site.socket_path().to_path_buf()));
    lifecycle.started();
    let mut conns = Slots::new(1024, 1024, || ServerConn::new(READ_CAP, WRITE_CAP));
    let mut deadlines = Timeouts::new(1024, read_timeout.unwrap_or(Duration::ZERO));
    let mut events = Events::with_capacity(256);
    let mut last_tick = m6_core::platform::now();

    // Run: nothing blocks.
    'run: loop {
        poll.poll(&mut events, Some(deadlines.timeout(m6_core::platform::now(), TICK)))?;
        for event in &events {
            match event.token() {
                LISTENER => {
                    while let Some(taken) = conns.take() {
                        let mut stream = match listener.accept() {
                            Ok((stream, _)) => stream,
                            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                                conns.free(taken.key);
                                break;
                            }
                            Err(e) => {
                                conns.free(taken.key);
                                eprintln!("my-service: accept error: {e}");
                                break;
                            }
                        };
                        poll.registry()
                            .register(&mut stream, Token(taken.key), Interest::READABLE)?;
                        if read_timeout.is_some() {
                            deadlines.touch(Token(taken.key), m6_core::platform::now());
                        }
                        conns.get_mut(taken.key).attach(stream);
                    }
                }
                SIGNALS => {
                    let mut stop = false;
                    while let Some(signal) = signals.read() {
                        if lifecycle.on_signal(signal) == Action::Stop {
                            stop = true;
                        }
                    }
                    if stop {
                        break 'run;
                    }
                }
                LOG => m6_core::log::flush(),
                Token(k) => {
                    let conn = conns.get_mut(k);
                    let was = conn.interest();
                    match turn(conn, &mut site) {
                        Ok(true) => {
                            if conn.interest() != was {
                                let interest = conn.interest();
                                let stream = conn
                                    .stream()
                                    .expect("every registered token has a connection");
                                poll.registry().reregister(stream, Token(k), interest)?;
                            }
                            if read_timeout.is_some() {
                                deadlines.touch(Token(k), m6_core::platform::now());
                            }
                        }
                        _ => m6_core::h1::close_conn(&poll, &mut conns, &mut deadlines, k)?,
                    }
                }
            }
        }
        // A full stdout holds its bytes in the Sink. Watch it for writability
        // only while a write has returned EAGAIN, so the loop never waits on
        // the log. A failed watch is logged and the service serves on.
        if let Err(e) = m6_core::log::watch(poll.registry(), LOG) {
            m6_core::log::error!(error = %e, "log descriptor registration failed");
        }
        // The accounting tick: flush the log buffers once a second.
        if m6_core::platform::now() - last_tick >= TICK {
            m6_core::log::tick(m6_core::platform::wall());
            last_tick = m6_core::platform::now();
        }
        while let Some(Token(k)) = deadlines.expired(m6_core::platform::now()) {
            m6_core::h1::close_conn(&poll, &mut conns, &mut deadlines, k)?;
        }
    }

    // Stop: blocking and allocation are fine.
    lifecycle.complete();
    m6_core::log::stop()?;
    Ok(())
}

/// One connection's turn: write what it owes, read, dispatch, write, and
/// report whether it stays open.
fn turn(conn: &mut ServerConn, site: &mut Site) -> io::Result<bool> {
    loop {
        if conn.interest() == Interest::WRITABLE {
            match conn.flush()? {
                Flush::More => return Ok(true),
                Flush::Close => return Ok(false),
                Flush::Done => {}
            }
        }
        let mut heads = head_slots();
        let read = conn.read(&mut heads)?;
        let (response, idle) = match read {
            Read::Request(head) => {
                let response = match site.handle(&head) {
                    Handled::Done(response) => response,
                    Handled::Code(0, req) => site.finish(&req, Response::text("ok")),
                    // `POST /genid`: the start path again, then one assignment.
                    Handled::Reload(reload) => {
                        let response = match site.reload(&[], &[], &NoTemplates) {
                            Ok(next) => {
                                *site = next;
                                genid_ok()
                            }
                            Err(e) => genid_failed(&e),
                        };
                        finish_reload(&reload, response)
                    }
                    Handled::Code(..) | Handled::Named(..) => unreachable!(),
                };
                (Some(response), false)
            }
            Read::More => (None, true),
            Read::Closed => return Ok(false),
        };
        if let Some(response) = response {
            conn.respond_response(response);
        }
        match conn.flush()? {
            Flush::Done if !idle => {}
            Flush::Done | Flush::More => return Ok(true),
            Flush::Close => return Ok(false),
        }
    }
}
```

The loop has one `match` on the event's token.
`LISTENER` takes free slots and accepts until the kernel has no more.
`SIGNALS` drains the signal descriptor through `Lifecycle`, which answers whether to stop.
`LOG` writes what a full stdout held.
Every other token is one connection, and `turn` reads, dispatches and writes until its socket would block.
`watch` registers stdout only while a write has returned `EAGAIN`, and `tick` flushes the log buffer once a second.
`deadlines.expired` closes a peer that sent nothing before its read deadline.

Two details keep the shape correct.
`close_conn` clears the deadline, detaches the stream, deregisters it and frees the slot, in that order, so every exit from a connection goes through it.
`turn` reads again after a flush that kept the connection alive, because a pipelined request may already be buffered and an edge-triggered reactor reports a descriptor once per change.

`m6-file/src/main.rs` is the same loop with the buffer sizes read from `[limits]`, the connection table lowered by the open-file limit, and a `LoopTrace` for accounting.
`m6-core/examples/echo.rs` is a reference loop of the same shape.
Everything from §3 on is what the resulting binary promises.

---

## 3. The command line

**A service is configured by files and started with two paths.**
This section gives both, the flags a service answers, and the exit codes a supervisor reads.

```
my-service  <site_dir>  <config_path>  [--log-level LEVEL] [--dump-config] [--version] [--version-full]
```

`argv[1]` is the site directory, the root that `Request::site_path` resolves against.
`argv[2]` is the TOML configuration.
A call with fewer than two positional arguments prints the usage line and exits 2.

Table 3 gives the flags.
`--version` and `--version-full` are answered at the top of `main`, before any configuration is read.
`Site::load` handles the other two before any socket is bound.

| flag | what it does |
|---|---|
| `--log-level LEVEL` | overrides both configuration sources for the level. §4 gives the precedence |
| `--dump-config` | loads the configuration exactly as the service would, prints what it resolved to with every route and what would answer it, and exits |
| `--version`, `-V` | prints the tag form of the build's version, `v1.16.0-rc.1`, and exits 0 |
| `--version-full` | prints one `<crate> - <full SemVer>` line per m6 crate in the binary, the binary's own name first, and exits 0 |

**Table 3: the flags a service answers, and what each one is for.**
Take from it that the two version flags need no site directory and no configuration, and that `--dump-config` and `--log-level` are handled by `Site::load`.
[`CONTRIBUTING.md`](../CONTRIBUTING.md) §4.2 gives both version forms with examples.

The version flags come from two lines in the service.
Its `build.rs` calls the one build helper, and its `main` calls the one macro that answers both flags before it parses anything else:

```rust
// build.rs
fn main() { m6_core::build_info::emit(); }

// main
m6_core::version_flags!(m6_core::build_info::linked());
```

The helper emits the crate's `CARGO_PKG_VERSION`, the twelve-character commit from `git rev-parse --short=12 HEAD`, and whether `git status --porcelain` was non-empty.
A build with no git, which is a source tarball, reports `unknown` as the commit and still builds.

**`--dump-config` exits 0 when this binary can serve this configuration and 2 when it cannot.**
A deploy can run it against a new binary before installing it.
It prints every route with what would answer it, and names every route that nothing can serve.
A service that also reads a table of its own takes the dump after those reads, through `Site::load_deferring_dump`, so a key that table refuses fails the dump the way it fails the start.

Table 4 gives the exit codes, which are `m6-backend-protocol.md` §8.3, "Exit codes", for every backend in any language.

| code | meaning |
|---|---|
| 0 | clean shutdown |
| 1 | runtime error, including a failure to initialise logging |
| 2 | configuration or usage error, detected before the socket is bound |

**Table 4: the three exit codes, from `m6-backend-protocol.md` §8.3, "Exit codes".**
Take from it that 2 means "failed before binding", which lets a supervisor tell a misconfiguration from a crash.

Exit 2 covers these refusals:

- fewer than two positional arguments
- a configuration that does not load: a file that does not parse, a key set in both the configuration and its secrets file, a relative `secrets_file`, an unknown key in a core table, a `[thread_pool]` table, an invalid `[limits]` entry, minification turned on in a build without the `minify` feature, or an unknown log level
- a `site.toml` that does not exist, cannot be read or is not valid TOML
- a `[compression]` setting that contradicts what `site.toml` declares about this backend
- a startup error building the site
- a `[[route]]` that names a handler this binary does not register, or that names neither a template nor a handler and has no code route on the same pattern
- the `flash` feature enabled with no `flash_secret`
- a socket that cannot be bound, in a service that maps the error to 2 as `m6-file` does

The `main` in §2 returns the bind error instead, which exits 1.

Those refusals are decided before the socket exists, and the configuration is what every one of them reads, so §4 comes next.

---

## 4. The configuration

**Core reserves thirteen top-level keys and hands the service everything else.**
This section gives the reserved keys and their defaults, the name that ties the configuration file to the socket and to `site.toml`, and the limits every buffer follows.

Table 5 is the reserved set, and a key in it never reaches a template.

| key | read by | what it sets |
|---|---|---|
| `[params_cache]` | core | `size`, default 256 entries |
| `[server]` | core | `read_timeout_s`, default 30, `0` disables. `socket_mode`, an octal string as systemd writes it, default `"0660"` |
| `[compression]` | core | brotli and gzip levels per MIME type |
| `[minification]` | core | on or off per MIME type, plus `inline_js`. A build without the `minify` feature refuses a config that turns it on. The `no-store` exception is in Table 9 of `m6-architecture.md` |
| `[log]` | core | `level`, default `info` |
| `[limits.<name>]` | core | `start` and `max` of a named buffer or queue, §4.2 and Table 6 |
| `[[route]]` | core | the configuration route table, §5 |
| `global_params` | core | JSON files merged into every request dictionary |
| `secrets_file` | core | a second TOML file merged with this one, so a secret stays out of the tracked configuration. One key, one owner: a key set in both files is refused with an error naming every clash. The path is absolute, and a relative one is exit 2. A file that does not exist is skipped |
| `flash_secret` | core, feature `flash` | the HMAC key for flash cookies. Absent with the feature on is exit 2 |
| `site_toml` | core | the path of `site.toml`. Absolute as given, relative to `site_dir`, default `<site_dir>/site.toml`. A file that does not exist, cannot be read or is not valid TOML is exit 2 |
| `site_size_budget_mb` | core, read by `m6-file` | the document root's size budget in MiB, a whole number of at least 1, default 128. Other services accept the key and ignore it |
| `[errors]` | `m6-http`, in `site.toml` | reserved, so it does not reach the request dictionary. The section itself belongs to `site.toml` |

**Table 5: the thirteen reserved keys, who reads each, and its default.**
Take from it that `[server]` refuses a value it cannot parse, because a service that started with a deadline nobody chose looks healthy.
An unknown key inside `[params_cache]`, `[server]`, `[log]`, a `[compression."<mime>"]` table or `[limits]` stops the service at start with exit 2.

A `[thread_pool]` table stops the start with exit 2 and the message `[thread_pool] is removed: a service runs one event loop on one core; run more instances of the service to use more cores`.

Every other top-level key lands in `RendererConfig::user_config` and is readable from the request dictionary.
A service takes its own settings this way, and `m6-monitor` puts its whole fleet under `[monitor]`.

The logging level has three sources and the last one wins: `site.toml`'s `[log]`, then this file's `[log]`, then `--log-level`.
An unknown level in any of the three stops the service at start with exit 2.
A `format` key is refused as an unknown key in either file, because every m6 log is a JSON line.

### 4.1 One name in three places

**The configuration file's stem is the socket's name and the service's `[[backend]]` name in `site.toml`.**
The socket is `/run/m6/<stem>.sock`, so a renamed configuration file moves the socket and the edge stops finding it.
The name in the lifecycle lines (§6.2) is the literal the service passes to `Site::load`.

The service reads `<site_dir>/site.toml`, or the path in the reserved key `site_toml`.
A service with its own root under `apps/` sets `site_toml = "../../site.toml"`, because the file sits at the top of the tree and the service's root sits below it.

Figure 1 is the chain.

```
   <any-directory>/my-service.toml      argv[2], the configuration file
        |
        |  file stem
        v
     "my-service"
        |
        +---------------> /run/m6/my-service.sock       the socket this service binds
        |
        +---------------> site.toml
                             [[backend]]
                             name    = "my-service"
                             sockets = "/run/m6/my-service*.sock"
```

**Figure 1: the configuration file's stem determines the socket path and the backend name.**
Take from it that the three names are one string, and that core reads only the stem, so the directory the configuration lives in is the operator's choice.

The glob in `site.toml` lets a pool have more than one member, `my-service-1.sock` and `my-service-2.sock`, one per systemd instance.
`m6-site-toml.md` §4.7, "`[[backend]]`", gives the rest of that section.
`M6_SOCKET_OVERRIDE` replaces the derived path outright, which lets a test bind a socket in a temporary directory with no privileges and no fixed path.
Every release binary reads it from the environment, and the tooling in `tools/` sets it as well as the test suites.

The name also carries one agreement that core checks at startup.
`site.toml` declares `[[backend]] compresses` for this name, default `true`, and a service whose `[compression]` disagrees with that declaration exits 2 with a message saying which way.
A service compresses when any MIME type has a non-zero brotli or gzip level, and the defaults give the text types level 6.
A service with no `[[backend]]` entry under its name passes unchecked.
A service that compresses where `site.toml` says it does not lets the edge cache a compressed body and replay it to a client that asked for identity.
A service that declares `true` and compresses nothing fragments every downstream cache on a header that cannot change the body.

The route table is the part of that configuration a service writes code against, and §5 is how the two meet.

### 4.2 Limits

**Every bounded buffer and queue has a named limit with a `start` and a `max`, and both are config keys.**
Table 6 lists every limit with the defaults a config that says nothing gets, the process that reads it, and what happens past the maximum.
A key under `[limits]` that is not in the table stops the process at start with exit 2.
`m6-http` reads its keys from the `[limits]` table of its system config, and every other service reads them from its own configuration file.

| limit | unit | start | max | read by | what it bounds | past `max` |
|---|---|---|---|---|---|---|
| `render_page` | bytes | 512 KiB | 4 MiB | m6-html and any `Site` | the area one rendered page is written into | the page is refused with a 500 |
| `request_body` | bytes | 16 KiB | 16 MiB | services | one connection's read buffer, so the largest request body | 413 |
| `response_buffer` | bytes | 64 KiB | 64 MiB | services | one connection's write buffer, so the largest response held in memory | 500 |
| `upstream_read` | bytes | 64 KiB | 128 MiB | services, m6-monitor | the buffer one backend response is read into | the fetch fails |
| `upstream_request` | bytes | 16 KiB | 32 MiB | services, m6-monitor | the buffer one backend request is built in | the fetch fails |
| `connections` | slots | 1024 | 1048576 | services, m6-http | the connection table, lowered by the open-file limit | the kernel backlog holds the connection |
| `compress_output` | bytes | 64 KiB | 128 MiB | services | the response a compressor reads | 500 |
| `minify_output` | bytes | 64 KiB | 128 MiB | services | the response a minifier reads | 500 |
| `smtp_reply` | bytes | 512 B | 128 KiB | services with mail | one relay reply | the send fails |
| `ndjson_page` | bytes | 4 MiB | 64 MiB | m6-monitor | the default page and the largest row of an ndjson stream | the row is refused |
| `tail_read` | bytes | 64 KiB | 512 KiB | m6-file | one page of a tail response, and the window a last-N-lines request scans | a client further behind reads the next page from `X-Log-End`, and N lines that do not fit in a page are 413 |
| `log_buffer` | bytes | 16 KiB | 1 MiB | every process | one log destination's buffer | the line is refused |
| `log_field` | bytes | 8 KiB | 8 KiB | every process | one string field of a log line | the line is refused |
| `history_queue` | polls | 8 | 1024 | m6-monitor | polls held while the history file is locked | the new poll is refused |
| `journal_line` | bytes | 64 KiB | 4 MiB | m6-monitor | one line read from the journal, such as a whole metrics document with its JSON wrapper | the line is dropped and counted |
| `edge_request` | bytes | 4 KiB | 32 MiB | m6-http | one HTTP/1.1 connection's read buffer, so the largest request | 413 |
| `edge_response` | bytes | 512 B | 128 MiB | m6-http | one HTTP/1.1 connection's write buffer, so the largest response held | 500 |
| `h2_recv` | bytes | 16 KiB | 4 MiB | m6-http | one HTTP/2 connection's read buffer | the connection closes |
| `h2_header_block` | bytes | 16 KiB | 256 KiB | m6-http | one HTTP/2 header block before decoding | GOAWAY, ENHANCE_YOUR_CALM |
| `h2_stream_body` | bytes | 4 KiB | 32 MiB | m6-http | one HTTP/2 request body | the stream is reset |
| `h2_send` | bytes | 16 KiB | 1 MiB | m6-http | one HTTP/2 connection's unsent response bytes, where `start` is the pacing mark | never reached: the flusher stops at `start` |
| `request_dict` | entries | 32 | 4096 | services | the fields of one request: query, form, path parameters, cookies and built-ins | the request is refused with a 400 |
| `refresh_queue` | entries | 16 | 4096 | m6-http | stale cache entries waiting for a background fetch | the refresh is refused |
| `rate_limit_addresses` | entries | 1024 | 50000 | m6-http | client addresses the request counter tracks | a new address is answered 429 |
| `cache_bytes` | bytes | 128 MiB | 128 MiB | m6-http | the response cache, which evicts the least recently read entry | the cache evicts |
| `heap_reserve` | bytes | 32 MiB (m6-http: `cache_bytes` max plus 64 MiB) | 4 GiB | services | the heap reserved from the OS at start, on huge pages where the kernel has them | the heap still grows from the OS, logged once, and is never a failure |

**Table 6: the named limits, their defaults, the process that reads each, and what happens past the maximum.**
Take from it that every limit is one `start` and one `max`, and that the last column says what happens past `max`.
The start-up record's platform line lists every limit a process runs with, its `start`, its `max` and whether each came from the file or the default ([`m6-log.md`](m6-log.md) §9).

The HTTP/1.1 head limits of 8 KiB per field and 32 KiB per head are protocol limits answered 400 (414 for a request line).
The HTTP/2 reset memory is twice the concurrency cap (200 streams at the cap of 100), so it stays above the cap and has no key of its own.

```toml
[limits.render_page]
start = "512KiB"
max   = "4MiB"
```

A byte size is a whole number of bytes, or a number followed by `B`, `KiB`, `MiB` or `GiB`.
A count is a whole number.
Either key may be left out, and the default for that name applies.

The same rule holds for every limit:

1. The area is allocated at `start` when the service starts.
2. It grows only when a write does not fit, by doubling, and the last step is clamped to `max`.
3. It keeps the grown size and never shrinks.
4. Each growth writes one log line that names the limit, the page or request that grew it, the old and new sizes, the maximum and the exact key and file to raise, so an operator can raise `start` until growth stops: `limit render_page grew from 512 KiB to 1 MiB for page /blog/x (max 4 MiB, [limits.render_page] in /etc/m6/m6-html.conf)`.
5. A write that needs more than `max` is refused outright with one error line, for example: `m6-html: page /blog/x needs at least 5 MiB, over [limits.render_page] max = 4 MiB in /etc/m6/m6-html.conf; raise max to fix this`.
   The request gets its error status, or the queue item is refused, and nothing is cut, dropped without an error or sent in part.

A large area is a whole multiple of the operating system's page, `page * 2^k`, where the page comes from `m6-platform`: 4 KiB on Linux x86_64 and 16 KiB on macOS arm64.
A small buffer is a power of two, and a count is used as written.
A value of another size is rounded up to the next one, and the service writes one warning to stderr per value, naming the key, the file, the value set and the value used:
`m6-html: [limits.render_page] start = 500KiB in /etc/m6/m6-html.conf is not a power-of-two multiple of the 16 KiB page; using 512 KiB. Set it to 512KiB to silence this.`
A value that cannot work stops the start with exit 2: a `max` below its `start`, a zero, a unit that is not a size, an unknown limit or an unknown key.

---

## 5. Routes, handlers and state

**A route is a `CodeRoute` in the service's code or a `[[route]]` in configuration, and a handler is always code.**
This section gives both bindings, the one `match` on `Handled`, and where state lives.

### 5.1 Routes bound in code

A service declares its code routes as one constant, in the order its `match` arms read.

```rust
const ROUTES: &[CodeRoute] = &[
    CodeRoute { pattern: "/", method: RouteMethod::Get },
    CodeRoute { pattern: "/digest", method: RouteMethod::Get },
    CodeRoute { pattern: "/items/{id}", method: RouteMethod::Any },
];
```

`RouteMethod` names `Any`, `Get`, `Post`, `Put`, `Patch` or `Delete`, and `GET` also answers `HEAD`.
`Site::load` compiles the routes at start, refuses two that could answer one request, and indexes each by pattern and method.

A request that one of them answers comes back as `Handled::Code(index, req)`, and the service's one `match` calls the body for that index and then `site.finish`.

`POST /genid` is reserved.
`Site::handle` answers it before routing, and the loop runs the start path again with `Site::reload` and assigns the new `Site` when that is `Ok`.

```rust
let response = match site.handle(&head) {
    Handled::Done(response) => response,
    Handled::Code(0, req) => page(&req),
    Handled::Code(1, req) => digest_json(&req),
    Handled::Code(2, req) => item(&req),
    Handled::Code(index, _) => unreachable!("route {index} is not in this match"),
    Handled::Named(index, req) => named(index, &req),
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
```

`site.finish` renders a template the response names, then adds the site-wide parts: `Cache-Control` from the route, `Last-Modified`, CSRF and flash housekeeping, minification and compression.
`Response::render(template, req)` renders a template, and `json`, `text`, `html`, `status`, `redirect`, `not_found`, `forbidden` and `bad_request` cover the rest, with `body`, `header`, `cookie` and `verbatim` as builders over any of them.
`m6-core-reference.md` §5, "Request and response", has the full set.

A service that renders no templates passes `NoTemplates` and takes core with `default-features = false`, so that built on its own (`cargo build -p`, `cargo install`) it links no template engine.

```rust
let site = Site::load("my-service", ROUTES, &[], &NoTemplates);
```

The set of code routes is fixed for the life of the process.

### 5.2 Routes declared in configuration

**A service whose routes are part of its deployment registers a handler by name and lets configuration bind it to a path.**
`m6-file` registers one handler, `files`, and every route it answers comes from its configuration.

```rust
let site = Site::load("m6-file", &[], &["files"], &NoTemplates);
```

```toml
[[route]]
path    = "/assets/{*relpath}"
handler = "files"
root    = "assets/"
```

Configuration routes are compiled at start.
A route naming a handler that was never registered exits 2 at startup.
A request for a route that names a handler comes back as `Handled::Named(index, req)`, where `index` is the handler's position in the list given to `Site::load`, and the service's `match` calls it.

Table 7 gives the keys a `[[route]]` may carry.

| key | what it does |
|---|---|
| `path` | the pattern. `{name}` captures one segment, `{*name}` captures a segment and every one after it |
| `handler` | the name of a handler the service passes to `Site::load` |
| `template` | a template to render, which needs no code at all |
| `params` | JSON files merged into this route's request dictionary. A path holding `{placeholder}` resolves per request |
| `status` | the status a template route answers with, default 200 |
| `cache` | a `Cache-Control` value. Named, it replaces any `Cache-Control` the handler set. Not named, a handler route defaults to `no-store` and a template route to `public, max-age=0, must-revalidate, s-maxage=86400` (`m6_core::PAGE_CACHE_CONTROL`), and a handler's own value wins over the default |
| `methods` | exactly the methods this route answers, `GET` also answering `HEAD`. Omitted, it answers every method. An unknown name fails the load |
| `headers` | extra response headers, as `[[key, value]]` pairs |
| anything else | kept verbatim and read by the handler through `Request::route_setting` |

**Table 7: the eight keys core defines on a `[[route]]`, and what happens to a ninth.**
Take from it that the defaults for `cache` depend on the kind of route, because a handler route that inherited the page default would put computed output in a shared cache by omission, and that core does not interpret `root` or any other key it does not define.

A route carrying both a template and a handler goes to the handler.
A route naming neither is valid only when a code route has the same pattern, and it then carries `cache` and `methods` for that code route.
Two routes that could answer one request are refused at load, so no request needs a tie-break.

### 5.3 State

**State is a plain value in `main`, beside the loop.**
One thread owns it, so it needs no lock.

```rust
fn main() -> anyhow::Result<()> {
    let site = Site::load("my-service", ROUTES, &[], &NoTemplates);
    let config = MyConfig::read(site.user_config(), site.site_dir())?;
    let db = m6_core::sqlite::open(&config.database)?;   // feature `sqlite`
    let state = State { db, config };
    // The loop's arms call `handle(&state, &mut conn, &head)`.
}
```

`Site` carries the parsed configuration, the site directory, the configuration file's path and the resolved path of `site.toml`, which a service reads through `site.user_config()`, `site.site_dir()`, `site.config_path()` and `site.site_toml()`.
A value that the loop mutates across requests is a plain `mut` local, passed to the arm as `&mut`.

A start that cannot build its state prints what failed and exits 2.
The state outlives the loop, and stop may use it before the process exits.

Routes and state are what a service writes.
The lifecycle in §6 is what it builds.

---

## 6. The lifecycle

**The service owns startup and shutdown, and the parts supply the ordering rule, the three log lines and the socket cleanup.**
This section gives the one ordering rule that cannot be relaxed, the three lines every service logs, and the systemd unit.

### 6.1 Signals

`m6_core::signal::block()` masks SIGTERM, SIGINT and SIGUSR1.
It runs before anything that can start a thread, logging included, and `Signals::new()` then opens the descriptor the loop waits on.
A thread inherits the signal mask as it stands when the thread is created.
A thread started before `block()` has SIGTERM unblocked, the kernel delivers the process-directed signal there, and the default disposition kills the process.
`Signals::new()` refuses to open without the mask and says so.

The descriptor opens before the socket exists.
The socket is how a caller knows the service is ready, so a caller that waits for it and then sends SIGTERM must find the descriptor open.
A stop that arrives while the mask is blocked and no descriptor is open is discarded on macOS, and the service would then ignore the stop that followed its own bind.

The first signal stops accepting, removes the socket and exits 0.
It does not wait for a request in progress, so a request cut off by a restart is lost, and a service that needs state to survive a restart cannot depend on a shutdown hook.
A second signal removes the socket, logs `<name> shutdown forced` and exits 0 at once.
A socket left behind keeps a dead member in the edge's backend pool.

`Lifecycle::on_signal` decides what a signal means: `Action::Stop` breaks the loop and `Action::Continue` carries on.
`Lifecycle::complete()` removes the socket and writes the completion line.

### 6.2 The three lines

Every service logs the same three lines under its own name, and `Lifecycle` emits them.

```
<name> started
<name> shutdown signal received
<name> shutdown complete
```

The name comes from `site.name()`, the literal the service passes to `Site::load`, and the started line carries the socket.
One emitter makes a search of a journal for `shutdown complete` mean the same thing for every unit.
`on_signal` writes the signal line before the loop breaks, so it precedes the completion line in the log.

Two structured records frame the three lines.
`Site::prepare` writes the start-up record, several `type=startup` lines, before the socket is bound, and `Lifecycle::complete` writes one `type=shutdown` line, with the run's `run_id`, its uptime and its reason, before the completion line.
[`m6-log.md`](m6-log.md) §9 gives both, and [`m6-core-reference.md`](m6-core-reference.md) §4.9 the two calls an application with its own config makes instead.

### 6.3 Under load and under a supervisor

A connection is served until the peer closes it or 100 requests have gone by, and the read deadline from `[server] read_timeout_s` closes a silent peer when it passes.
The connection table starts at the `connections` limit, its maximum is lowered by the process's open-file limit, and a client arriving at the ceiling waits in the kernel's listen backlog.
More cores means more instances of the service.

The systemd unit has to supply four things: the two positional arguments, the user and group the socket directory is owned by, `RuntimeDirectory=m6` with `RuntimeDirectoryMode=0750`, and `ReadWritePaths` naming what this service writes.
It also supplies `LoadCredential` for each secret and `StateDirectory` for a database, as `m6-user-guide.md` §7.2, "Secrets and state", sets out.
`m6-user-guide.md` §7.4, "Unit files", carries a complete unit per service.

**`ReadWritePaths` must name only paths that exist.**
An absent path fails mount namespace setup with `226/NAMESPACE` and the unit never starts, so a service claiming `/run/m6` on a node with no socket backends does not come up.
`RuntimeDirectory` guarantees what must exist.
A `-` prefix on a path tolerates its absence, and the unit then starts on a misconfigured node with weaker isolation than intended.

A service can prove it holds all of this with one call, and §7 is that call.

---

## 7. Proving the lifecycle in one call

**The lifecycle contract is one assertion, and every serving binary in the repository makes it.**
This section is the whole test and what it checks.

Below is a lifecycle test in the shape of `m6-monitor/tests/lifecycle.rs`.

```rust
use std::process::Command;

use m6_core::testkit::{assert_app_lifecycle, binary};

#[test]
fn lifecycle_is_clean() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("app.conf");
    std::fs::write(&config, "").unwrap();
    // site.toml must exist (§4.1), and an empty one declares no backend.
    std::fs::write(dir.path().join("site.toml"), "").unwrap();
    // Short: a unix socket path caps near 104 bytes on macOS.
    let sock = dir.path().join("app.sock");

    assert_app_lifecycle(
        "my-service",
        Command::new(binary("my-service"))
            .arg(dir.path())
            .arg(&config)
            .env("M6_SOCKET_OVERRIDE", &sock),
        &sock,
    );
}
```

It spawns the binary, waits for the socket, sends SIGTERM, and asserts three things.
The exit status is success, because a signal status means the process died at the default disposition.
The socket is gone.
The three lines of §6.2 were logged under the service's own name.

**Write this test for every service.**
`m6-file`, `m6-auth-server` and `m6-monitor` make the assertion through `assert_app_lifecycle`, and `m6-html` and `m6-http` through `assert_lifecycle_logged` inside a wider suite that already has the process running.

The test is short because the lifecycle belongs to the parts, and §8 is the rest of what they give a service.

---

## 8. What the parts give a service

**A service that links `m6-core` gets the whole HTTP layer and the whole event-loop layer.**
This section lists it.

- HTTP/1.1 parsing and framing at 32/32 on h1spec, measured on `m6-html`, `m6-file`, `m6-auth-server` and the redirect listener, including HEAD with no body and correct `Connection` handling.
- Persistent connections to 100 requests, and a read deadline.
- `ServerConn` and `head_slots`, which read, parse and write through buffers made at start.
- `Poll`, `Token`, `Interest` and `Events` over epoll and kqueue, from `m6_core::platform`, the one crate that knows the operating system (`docs/m6-platform.md`). `m6-core/tests/echo_example.rs` tests the reference loop in `m6-core/examples/echo.rs`.
- `Signals`, `Signal`, `Timeouts`, `Slots` and `LoopTrace`: the signal descriptor, the deadline set, the connection table with its growth and free list, and the wait against work accounting.
- `listen_unix`, which binds a non-blocking socket with the mode set.
- Content-coding negotiation with real q-values, so `br;q=0` is honoured, `*` supplies the quality for anything unnamed, and an explicitly named coding beats `*` in either direction.
- Conditional requests: `If-Match`, `If-None-Match`, `If-Modified-Since` and `If-Unmodified-Since`, with weak comparison where the specification requires it.
- brotli and gzip, and HTML, CSS, JSON and JavaScript minification, both configurable per MIME type, with minification before compression.
- Configuration, templates and params files read at start and again on `POST /genid` (§5.1). The log level is fixed for the life of the process.
- Structured logging as JSON lines, flushed by the loop that writes them.
- A request dictionary layered over a shared base, assembled from twelve sources in a fixed order.
  The sources are the service's own configuration keys, `global_params`, the route's params files, path parameters, the query string, a form body, cookies, built-ins, forwarded authentication claims, and the flash and *CSRF* (*cross-site request forgery*) values when those features are on.
- Path parameter validation, which allows alphanumerics, `-`, `_`, `.`, and `/` only where a route's parameter spans segments, and refuses `..` anywhere as a substring.

Two of those carry a condition.
The built-in dictionary keys go in after every params file, so a content file cannot redefine the request path and make a page lie about which URL it is.
A completed request is logged at `info` with its path, method, status and `latency_us`, so a service at the default level writes one access line per request.

Everything in this section arrives by linking one crate, and §9 is how to link it from outside this repository.

---

## 9. Building against m6-core from another repository

**A service outside this repository reaches core by path or by git revision.**
This section gives both and says what each one costs.

A relative path is what `m6-examples` uses.

```toml
m6-core = { path = "../../../../m6/m6-core" }
```

That is a directory layout across a repository boundary.
It works only if a checkout of `m6` sits at exactly that relative position, it pins nothing, and nothing records which version the service was tested against.

A git dependency pinned to a revision needs no layout and names a version.

```toml
m6-core = { git = "https://github.com/mgrosvenor/m6", rev = "<sha>" }
```

The revision is the version tested against, recorded in the manifest, so a build cannot move underneath the service that declares it.
The pin advances only when the service's author advances it.

That is the last of the contract, and §10 collects it.

---

## 10. Summary

**Everything above is one claim: a new m6 service is its routes, its state and its loop, and the parts come from core.**
Table 8 is the whole contract in one place.

| the service supplies | `m6-core` supplies |
|---|---|
| a `main` with the loop, its token `match` and its `Handled` `match` | the reactor, the signal descriptor, the deadlines, the connection slot and `listen_unix` |
| routes as `CodeRoute` values, handlers registered by name, or both | route matching, the configuration route table and the dispatch |
| plain state in `main`, passed to the arms | the request dictionary, rendering, minification, compression and logging |
| a configuration file whose stem is the service's name | the socket path, the backend name and the reserved keys of Table 5 |
| a systemd unit naming the two paths and what it writes | `Lifecycle`: the start line, the signal line, the socket unlink and the exit |
| one call to `assert_app_lifecycle` | the assertion behind it |

**Table 8: the division of labour between a service and `m6-core`.**
Take from it that the left column is the whole of a new service, and that `m6-architecture.md` §6.2, "Why each thing in core exists exactly once", gives the reason for every row on the right.

A change to any row on the right reaches every service at once.
A service that follows this shape is read by anyone who knows the shape, and `m6-file` is a complete static file server written to it.

**A new service is a `main` and one `match` per descriptor.**
`m6-architecture.md` §7.1 gives the reasons for that shape, and this document is how to write it.
