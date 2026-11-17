# Anatomy of an m6 service

An m6 service is a `main` with its own event loop, plus the parts `m6-core` supplies.
This document is that shape, written so it can be followed from a repository that is not this one.

`m6-architecture.md` §7, "why each service writes its own loop", argues why the shape exists, and `m6-core-reference.md` names every component and its interface.
This document shows the shape itself.

§2 shows the two files in full.
§3 to §6 give the four contracts every service honours: the command line, the configuration, the routes and the lifecycle.
§7 is the test that proves the lifecycle in one call, §8 lists what the parts give a service, §9 covers building against `m6-core` from another repository, and §10 summarises.

Where this document and the code disagree, the code is right and this document is a defect.

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

Every m6 service except the edge has this shape.
`m6-http` faces the internet, terminates TLS, HTTP/2 and HTTP/3, owns the response cache and proxies to these services over their sockets, which is a different job and a different shape.
`m6-md` and `m6-auth-cli` are command line tools and hold no socket at all.

A process passes through three phases, and Table 1 says what each may do.

| phase | when | blocking |
|---|---|---|
| start | from `main` until the loop begins | allowed: read config, load templates, open the database, resolve host names |
| run | serving requests | forbidden: every wait goes through the loop |
| stop | after the loop exits | allowed: flush, unlink the socket, exit |

**Table 1: the three phases of a process, and what each may block on.**
Take from it that a blocking call belongs in start or stop, and that the run phase never blocks.

The rule follows from the measure.
Latency is the number that matters, and one blocked loop stalls every connection in the process, while a hand-off to another thread costs a context switch and a cold cache.
The owner's decision is that start and stop may block freely, and the loop is for runtime only.

Four documents divide this subject, and Table 2 says which one to open.

| document | what it owns |
|---|---|
| `m6-architecture.md` §7, "why each service writes its own loop" | why the shape exists, and why start, stop and the loop belong where they do |
| `m6-core-reference.md` | every module, every feature flag, and the full interface of `Request` and `Response` |
| this document | the contract a service honours, and the code that honours it |
| `m6-backend-protocol.md` | the wire contract the edge requires of any backend, in any language |

**Table 2: where each part of the subject is written down.**
Take from it that this document holds the shape and the contract, and that a question of the form "why is it like this" is answered in `m6-architecture.md` §7.

The rest of this document is that contract in four parts, and §2 starts with the smallest complete service in the repository.

---

## 2. The two files

**A service is a `Cargo.toml` and a `main.rs`.**
The manifest holds one dependency, and the `main` holds the three phases, both shown here in full.

The manifest is the first half of the claim.

```toml
[package]
name    = "my-service"
version = "0.1.0"
edition = "2021"

[[bin]]
name = "my-service"

[dependencies]
m6-core = { path = "../m6-core" }   # see §9 for outside this repository

[dev-dependencies]
m6-core  = { path = "../m6-core", features = ["testkit"] }
tempfile = "3"
```

One dependency.
A `main` returning `anyhow::Result` carries `anyhow` as well, and `m6-http/tests/backends/rust-m6core` is a five-route service whose whole manifest is that one `m6-core` line.
`templates` is on by default and pulls tera and comrak, so a service that renders no templates sets `default-features = false` and links neither.
`testkit` goes in `[dev-dependencies]` only, which keeps the test harness out of every release binary.
`m6-core-reference.md` §2, "feature flags", is the full feature table and this document does not repeat it.

The `main` is the second half.
Below is a one-route service with the shape every service follows: start, run, stop.

```rust
use std::io;
use std::time::{Duration, Instant};

use m6_core::prelude::*;
use m6_core::signal::{Action, Lifecycle};

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
    let site = Site::load("my-service", ROUTES, &[], &NoTemplates);
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
                    match turn(conn, &site) {
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
`watch` registers stdout only while a write has returned `EAGAIN`, and `tick` flushes the buffer once a second.
`deadlines.expired` closes a peer that sent nothing before its read deadline.

Two details keep the shape correct.
`close_conn` is the one detach, deregister and free order, so every exit from a connection goes through it.
And `turn` reads again after a flush that kept the connection alive, because a pipelined request may already be buffered and an edge-triggered reactor reports a descriptor once per change.

Everything from §3 on is what the resulting binary promises.

---

## 3. The command line

**A service is configured by files and started with two paths.**
This section gives both, the four flags core parses before it starts, and the exit code a supervisor reads.

```
my-service  <site_dir>  <config_path>  [--log-level LEVEL] [--dump-config] [--version]
```

`argv[1]` is the site directory, which is the root `Request::site_path` resolves against and the only tree the service reads.
`argv[2]` is its TOML configuration.
A call with fewer than two positional arguments prints the usage line and exits 2.

Table 3 gives the flags, all of them handled in `parse_invocation` before any socket is bound.

| flag | what it does |
|---|---|
| `--log-level LEVEL` | overrides both configuration sources for the level. §4 gives the precedence |
| `--dump-config` | loads the configuration exactly as the service would, prints what it resolved to with every route and what would answer it, and exits |
| `--version`, `-V` | prints the program's own name from `argv[0]` and the version, and exits 0. Takes no site directory and no configuration, so a freshly installed binary can be asked what it is before any configuration is in place |

**Table 3: the flags core parses, and what each one is for.**
Take from it that `--dump-config` is the only one that reads the configuration, and that `--version` is the only one that runs without it.

**`--dump-config` exits 0 when this binary can serve this configuration and 2 when it cannot**, which makes it a validation step a deploy can run against a new binary before installing it.
It names every route that nothing can serve, and a route that nothing can serve is invisible at runtime because it answers 404 like any other unmatched path.
Every service loads its configuration through the same function, so the check covers all of them.
A service that also reads a table of its own takes the dump after those reads, through `Site::load_deferring_dump`, so a key that table refuses fails the dump the way it fails the start.

Table 4 gives the exit codes, which are `m6-backend-protocol.md` §8.3, "exit codes", and the same for every backend in any language.

| code | meaning |
|---|---|
| 0 | clean shutdown |
| 1 | runtime error, including a failure to initialise logging |
| 2 | configuration or usage error, detected before the socket is bound |

**Table 4: the three exit codes, from `m6-backend-protocol.md` §8.3, "exit codes".**
Take from it that 2 means "failed before binding", which is what lets a supervisor tell a misconfiguration from a crash.

Seven conditions exit 2, and all seven are configuration or environment the service refuses to start on.
A configuration that does not load, a `[thread_pool]` table, a startup error building the site, a socket that cannot be bound, a `[[route]]` naming a handler this binary does not register, a `[compression]` setting that contradicts what `site.toml` declares about this backend, and the `flash` feature enabled with no `flash_secret`.
"Does not load" covers a file that does not parse and a key set both in the configuration and in its secrets file.

Those paths are decided before the socket exists, which is why §4 comes next: the configuration is what every one of them is reading.

---

## 4. The configuration

**Core reserves twelve top-level keys and hands the service everything else.**
This section gives the reserved keys and their defaults, the name that ties the configuration file to the socket and to `site.toml`, and what is read at start.

Table 5 is the reserved set, and a key in it never reaches a template.

| key | read by | what it sets |
|---|---|---|
| `[params_cache]` | core | `size`, default 256 entries |
| `[server]` | core | `read_timeout_s`, default 30, `0` disables. `socket_mode`, an octal string as systemd writes it, default `"0660"` |
| `[compression]` | core | brotli and gzip levels per MIME type |
| `[minification]` | core | on or off per MIME type, plus `inline_js`. A build without the `minify` feature refuses a config that turns it on. The `no-store` exception is in Table 11 of `m6-architecture.md` |
| `[log]` | core | `level`, default `info` |
| `[limits.<name>]` | core | `start` and `max` of a named buffer or queue, §4.2 and Table 6 |
| `[[route]]` | core | the configuration route table, §5 |
| `global_params` | core | JSON files merged into every request dictionary |
| `secrets_file` | core | a second TOML file merged with this one, so a secret stays out of the tracked configuration. One key, one owner: a key set in both files is refused with an error naming every clash. An absolute path, and a relative one is exit 2 |
| `flash_secret` | core, feature `flash` | the HMAC key for flash cookies. Absent with the feature on is exit 2 |
| `site_toml` | core | the path of `site.toml`. Absolute as given, relative to `site_dir`, default `<site_dir>/site.toml`. A named file that does not exist, cannot be read or is not valid TOML is exit 2 |
| `[errors]` | `m6-http`, in `site.toml` | reserved, so it does not reach the request dictionary. The section itself belongs to `site.toml` |

**Table 5: the twelve reserved keys, who reads each, and its default.**
Take from it that every value core reads has a working default, and that `[server]` refuses a value it cannot parse, because a service that started with a deadline nobody chose looks healthy. An unknown key inside one of these tables stops the service at start with exit 2.

A `[thread_pool]` table is refused with its own message, because one process runs one loop on one core.
`[thread_pool] is removed: a service runs one event loop on one core; run more instances of the service to use more cores`.
The way to use more cores is more instances, and §6.3 shows the unit.

Every other top-level key lands in `RendererConfig::user_config` and is readable from the request dictionary.
That is how a service gets its own settings without inventing a second configuration file, and `m6-monitor` puts its whole fleet under `[monitor]` this way.

The logging level has three sources and the last one wins: `site.toml`'s `[log]`, then this file's `[log]`, then `--log-level`.
An unknown level in any of the three stops the service at start with exit 2.
A `format` key is refused as an unknown key in either file, because every m6 log is a JSON line.

### 4.1 One name in three places

**The configuration file's stem is the service's name, its socket's name, and its name in `site.toml`.**
Nothing derives that from the binary or from a configuration key, so a renamed configuration file moves the socket and the edge stops finding it.

The service finds `site.toml` through the reserved key `site_toml`.
Without the key it reads `<site_dir>/site.toml`.
A service that has its own root under `apps/` sets `site_toml = "../../site.toml"`, because the file sits at the top of the tree and the service's root sits below it.
Without the key the service reads no `site.toml`, so the compression agreement below passes unchecked and `[log]` falls back to its defaults.

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

The glob in `site.toml` exists so a pool can have more than one member, `my-service-1.sock` and `my-service-2.sock`, one per systemd instance.
`m6-site-toml.md` §`[[backend]]` gives the rest of that section.
`M6_SOCKET_OVERRIDE` replaces the derived path outright, which is what lets a test bind a socket in a temporary directory with no privileges and no fixed path.
It is read from the environment in every release binary, and the tooling in `tools/` sets it as well as the test suites.

The name also carries one agreement that core checks at startup.
`site.toml` declares `[[backend]] compresses` for this name, and a service whose `[compression]` disagrees with that declaration exits 2 with a message saying which way.
A service that compresses where `site.toml` says it does not lets the edge cache a compressed body and replay it to a client that asked for identity, and the reverse fragments every downstream cache on a header that cannot change the body.

The route table is the part of that configuration a service writes code against, and §5 is how the two meet.

### 4.2 Limits

**Every bounded buffer and queue has a named limit with a `start` and a `max`, and both are config keys.**
Table 6 lists every limit with the defaults a config that says nothing gets, the process that reads it, and what happens past the maximum.
A key under `[limits]` that is not in the table stops the process at start with exit 2.
m6-http reads its keys from the `[limits]` table of its system config, and every other service reads them from its own configuration file.

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
| `journal_line` | bytes | 64 KiB | 4 MiB | m6-monitor | one line read from the journal, a whole metrics or traffic document with its JSON wrapper | the line is dropped and counted |
| `edge_request` | bytes | 4 KiB | 32 MiB | m6-http | one HTTP/1.1 connection's read buffer, so the largest request | 413 |
| `edge_response` | bytes | 512 B | 128 MiB | m6-http | one HTTP/1.1 connection's write buffer, so the largest response held | 500 |
| `h2_recv` | bytes | 16 KiB | 4 MiB | m6-http | one HTTP/2 connection's read buffer | the connection closes |
| `h2_header_block` | bytes | 16 KiB | 256 KiB | m6-http | one HTTP/2 header block before decoding | GOAWAY, ENHANCE_YOUR_CALM |
| `h2_stream_body` | bytes | 4 KiB | 32 MiB | m6-http | one HTTP/2 request body | the stream is reset |
| `request_dict` | entries | 32 | 4096 | services | the fields of one request: query, form, path parameters, cookies and built-ins | the request is refused with a 400 |
| `refresh_queue` | entries | 16 | 4096 | m6-http | stale cache entries waiting for a background fetch | the refresh is refused |
| `rate_limit_addresses` | entries | 1024 | 50000 | m6-http | client addresses the request counter tracks | a new address is answered 429 |
| `cache_bytes` | bytes | 128 MiB | 128 MiB | m6-http | the response cache, which evicts the least recently read entry | the cache evicts |
| `heap_reserve` | bytes | 32 MiB (m6-http: `cache_bytes` max plus 64 MiB) | 4 GiB | services | the heap reserved from the OS at start, on huge pages where the kernel has them | the heap still grows from the OS, logged once, and is never a failure |

**Table 6: the named limits, their defaults, the process that reads each, and what happens past the maximum.**
Take from it that every limit is one `start` and one `max`, and that a limit past its maximum refuses the work with an error naming the key, except the cache, which evicts by design.

The HTTP/1.1 head limits of 8 KiB per field and 32 KiB per head are protocol limits answered 400 (414 for a request line), and the HTTP/2 reset memory is derived as twice the concurrency cap (200 streams at the cap of 100), so it stays above the cap whatever the cap is and has no key of its own.

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
4. Each growth writes one log line that names the limit and the new size, so an operator can raise `start` until growth stops.
5. A write that needs more than `max` is refused outright with one error line, for example: `m6-html: page /blog/x needs at least 5 MiB, over [limits.render_page] max = 4 MiB in /etc/m6/m6-html.toml; raise max to fix this`.
   The request gets its error status, or the queue item is refused. Nothing is cut, dropped without an error or sent in part.

A large area is a whole multiple of the operating system's page, `page * 2^k`, where the page comes from `m6-platform`: 4 KiB on Linux x86_64 and 16 KiB on macOS arm64.
A small buffer is a power of two, and a count is used as written.
A value of another size is rounded up to the next one, and the service writes one warning to stderr per value, naming the key, the file, the value set and the value used:
`m6-html: [limits.render_page] start = 500KiB in /etc/m6/m6-html.toml is not a power-of-two multiple of the 16 KiB page; using 512 KiB. Set it to 512KiB to silence this.`
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
`Site::load` compiles them at start, refuses two that could answer one request, and indexes each by pattern and method.

A request that one of them answers comes back as `Handled::Code(index, req)`, and the service's one `match` calls the body for that index and then `site.finish`.

`POST /genid` is reserved: `Site::handle` answers it before routing, and the loop runs the start path again with `Site::reload` and assigns the new `Site` when that is `Ok`.

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

`site.finish` adds the site-wide parts: `Cache-Control` from the route, `Last-Modified`, CSRF and flash housekeeping, minification and compression.
`Response::render(template, req)` renders a template, and `json`, `text`, `html`, `status`, `redirect`, `not_found`, `forbidden` and `bad_request` cover the rest, with `body`, `header`, `cookie` and `verbatim` as builders over any of them.
`m6-core-reference.md` §5, "request and response", has the full set.

A service that renders no templates says so, and takes core with `default-features = false`, so that built on its own (`cargo build -p`, `cargo install`) it links no template engine.

```rust
let site = Site::load("my-service", ROUTES, &[], &NoTemplates);
```

The set of code routes is fixed for the life of the process.

### 5.2 Routes declared in configuration

**A service whose routes are part of its deployment registers a handler by name and lets configuration bind it to a path.**
A static file service gains an asset tree by being told about a directory.

```rust
let site = Site::load("m6-file", &[], &["files"], &NoTemplates);
```

```toml
[[route]]
path    = "/assets/{*relpath}"
handler = "files"
root    = "assets/"
```

`m6-file` is this one route plus the loop from §2, with `files` as its one named handler.
Configuration routes are compiled at start.
A route naming a handler that was never registered exits 2 at startup, and the response comes back as `Handled::Named(index, req)` for the service's `match` to call.

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
Take from it that the defaults for `cache` depend on the kind of route, because letting a handler route inherit the page default would put computed output in a shared cache by omission, and that core deliberately does not know what `root` means.

A route carrying both a template and a handler goes to the handler.
Two routes that could answer one request are refused at load, so no request needs a tie-break.

### 5.3 State

**State is a plain value in `main`, beside the loop.**
There is one thread, so no lock, no `Arc` for sharing and no registry is involved.

```rust
fn main() -> anyhow::Result<()> {
    let site = Site::load("my-service", ROUTES, &[], &NoTemplates);
    let config = MyConfig::read(site.user_config(), site.site_dir())?;
    let db = m6_core::sqlite::open(&config.database)?;
    let state = State { db, config };
    // The loop's arms call `handle(&state, &mut conn, &head)`.
}
```

`Site` carries the parsed configuration, the site directory, the configuration file's path and the resolved path of `site.toml`, which a service reads through `site.user_config()`, `site.site_dir()`, `site.config_path()` and `site.site_toml()`.
A value that the loop mutates across requests is a plain `mut` local, passed to the arm as `&mut`.
A `RefCell` is the type for a field that a `&self` method must mutate, and `ParamsCache` uses one for exactly that reason.

A start that cannot build its state prints what failed and exits 2.
The state outlives the loop, and stop may use it before the process exits.

Routes and state are what a service writes.
The lifecycle in §6 is what it builds.

---

## 6. The lifecycle

**The service owns startup and shutdown, and the parts supply the ordering rule, the three log lines and the socket cleanup.**
This section gives the one ordering rule that cannot be relaxed, the three lines every service logs, and the systemd unit.

### 6.1 Signals

`m6_core::signal::block()` runs before anything else, logging included, and `Signals::new()` opens the descriptor the loop waits on.
The ordering is the rule.
A thread inherits the signal mask as it stands when the thread is created.
A service that started a thread before blocking would have a thread with SIGTERM unblocked, the kernel would deliver the process-directed signal there, and the default disposition would kill the process.
`Signals::new()` refuses to open without the mask and says so.

The descriptor opens before the socket exists.
The socket is how a caller knows the service is ready, so a caller that waits for it and then sends SIGTERM must find the descriptor open.
A stop that arrives while the mask is blocked and no descriptor is open is discarded on macOS, and the service would then ignore the stop that followed its own bind.

The first signal stops accepting, removes the socket and exits 0.
It does not wait for a request in progress, so a request cut off by a restart is lost.
A service that needs state to survive a restart must not depend on a shutdown hook.
A second signal exits at once.
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
One emitter is what makes searching a journal for `shutdown complete` mean the same thing for every unit.
The signal line is written before the loop breaks, because a fast service used to exit with the completion line written and the signal line still queued in the writer.

### 6.3 Under load and under a supervisor

A connection is served until the peer closes it or 100 requests have gone by, and the read deadline from `[server] read_timeout_s` closes a silent peer when it passes.
The connection table is sized at start from the process's open-file limit, and a client arriving at the ceiling waits in the kernel's listen backlog.
More cores means more instances, never more threads in one.

The systemd unit has to supply four things: the two positional arguments, the user and group the socket directory is owned by, `RuntimeDirectory=m6` with `RuntimeDirectoryMode=0750`, and `ReadWritePaths` naming what this service actually writes.
It also supplies `LoadCredential` for each secret and `StateDirectory` for a database, as `m6-user-guide.md` section 7.2 sets out.
`m6-user-guide.md`, "Example 06, production with systemd", carries a complete unit per service and this document does not repeat them.

**`ReadWritePaths` must name only paths that exist.**
An absent path fails mount namespace setup outright with `226/NAMESPACE` and the unit never starts, so a service claiming `/run/m6` on a node with no socket backends does not come up.
Use `RuntimeDirectory` to guarantee what must exist, and never a `-` prefix to excuse an absent path, because tolerating it turns a misconfigured node into one that starts anyway with weaker isolation than intended.

A service can prove it holds all of this with one call, and §7 is that call.

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
The exit status is success, because a signal status means the process died at the default disposition instead of shutting down.
The socket is gone.
The three lines of §6.2 were logged under the service's own name, which also catches a service logging under someone else's.

**Write this test for every service.**
The assertion was opt-in and hand-written in five separate integration suites, and the sixth service reached "tested, ready to deploy" with seventeen unit tests and nothing that had ever started its binary.
All five serving binaries assert it now: `m6-file`, `m6-auth-server` and `m6-monitor` through `assert_app_lifecycle`, and `m6-html` and `m6-http` through `assert_lifecycle_logged` inside a wider suite that already has the process running.

The test is short because the lifecycle belongs to the parts, and §8 is the rest of what they give a service.

---

## 8. What the parts give a service

**A service that links `m6-core` gets the whole HTTP layer and the whole event-loop layer.**
This section lists it, because services have re-implemented parts of it before.

- HTTP/1.1 parsing and framing at 32/32 on h1spec, measured on `m6-html`, `m6-file`, `m6-auth-server` and the redirect listener, including HEAD with no body and correct `Connection` handling.
- Persistent connections to 100 requests, and a read deadline.
- `ServerConn` and `head_slots`, which read, parse and write through buffers made at start, so serving a request allocates only where the page path does.
- `Poll`, `Token`, `Interest` and `Events` over epoll and kqueue, from `m6_core::platform`, the one crate that knows the operating system (`docs/m6-platform.md`). `m6-core/examples/echo.rs` is the reference loop every service follows, held by `m6-core/tests/echo_example.rs` on every supported target.
- `Signals`, `Signal`, `Timeouts`, `Slots` and `LoopTrace`: the signal descriptor, the deadline set, the connection table with its growth and free list, and the wait against work accounting.
- `listen_unix`, which binds a non-blocking socket with the mode set.
- Content-coding negotiation with real q-values, so `br;q=0` is honoured, `*` supplies the quality for anything unnamed, and an explicitly named coding beats `*` in either direction.
- Conditional requests: `If-Match`, `If-None-Match`, `If-Modified-Since` and `If-Unmodified-Since`, with weak comparison where the specification requires it.
- brotli and gzip, and HTML, CSS, JSON and JavaScript minification, both configurable per MIME type, with minification before compression.
- Configuration read once at start, and a change takes a restart.
- Structured logging as JSON lines, at a level fixed for the life of the process, flushed by the loop that writes them.
- A request dictionary layered over a shared base, assembled in twelve ordered steps.
  The sources are the service's own configuration keys, `global_params`, the route's params files, path parameters, the query string, a form body, cookies, built-ins, forwarded authentication claims, and the flash and *CSRF* (*cross-site request forgery*) values when those features are on.
- Path parameter validation, which allows alphanumerics, `-`, `_`, `.`, and `/` only where a route's parameter spans segments, and refuses `..` anywhere as a substring.

Two of those carry a condition.
The built-in dictionary keys go in after every params file, so a content file cannot redefine the request path and make a page lie about which URL it is.
A completed request is logged at `debug` with its path, method, status and latency, so a service at the default `info` level records nothing per request.

Everything in this section arrives by linking one crate, and §9 is how to link it from outside this repository.

---

## 9. Building against m6-core from another repository

**m6 is not published to a registry, so a service outside this repository reaches core by path or by revision.**
This section gives both and says what each one costs.

A relative path is what `m6-examples` uses.

```toml
m6-core = { path = "../../../../m6/m6-core" }
```

That is a directory layout across a repository boundary.
It works only if a checkout of `m6` sits at exactly that relative position, it pins nothing, and there is no way to say which version the service was tested against.

A git dependency pinned to a revision needs no layout and names a version.

```toml
m6-core = { git = "https://github.com/mgrosvenor/m6", rev = "<sha>" }
```

The revision is the version tested against, recorded in the manifest, and a build cannot move underneath the service that declares it.
The cost is that the pin is advanced deliberately, and a service wanting a fix in core says so in a commit.

**The relative path is still the shape in use.**
A service on that form needs the sibling checkout, and the test of having left it behind is that the service builds with no `m6` checkout beside it.

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
Take from it that the left column is the whole of a new service, and that every row on the right was once written once per service, which `m6-architecture.md` §6.2, "why each thing in core exists exactly once", counts.

Two consequences follow, and both are reasons to stay inside the shape.
A change to any row on the right reaches every service at once, which is why four HTTP/1.1 parsers scoring between 14/32 and 27/32 against the same suite became one scoring 32/32.
And a service written to this shape is read by anyone who knows the shape, which is what makes `m6-file` a complete static file server.

**If you are writing a new service, you are writing a `main` and one `match` per descriptor.**
There is one shape, `m6-architecture.md` §7, "why each service writes its own loop", argues why, and this document is how.
