# m6, design and architecture

This document answers why.
It gives the reason behind each m6 design decision: the problem m6 solves, the shape that solves it, and what the shape gives up.
Mechanism appears only as far as a reason needs it.

`README.md` says what m6 is and how to run one, `m6-core-reference.md` names every component and interface, `m6-site-toml.md` gives every configuration key, and `m6-backend-protocol.md` specifies the wire contract.
`BENCHMARKS.md` is authoritative for every measured number, and `PERFORMANCE.md` carries the per-component costs.

We begin with the question m6 was built to answer, what reality added to it, and what it gives up (§1), then the shape that follows (§2).
§3 to §8 give the reasoning for each part of that shape: the process family, the edge, authentication, the library, the service shape, and configuration.
§9 holds the remaining decisions with a reason each, §10 lists what is absent, and §11 summarises.
Where this document and the code disagree, the code is right and this document is a defect.

## Contents

1. [The question m6 answers](#1-the-question-m6-answers)
2. [The shape that follows](#2-the-shape-that-follows)
3. [Why a family of processes](#3-why-a-family-of-processes)
4. [Why one process faces the internet](#4-why-one-process-faces-the-internet)
5. [Why authentication sits where it does](#5-why-authentication-sits-where-it-does)
6. [Why m6-core exists, and where its boundary falls](#6-why-m6-core-exists-and-where-its-boundary-falls)
7. [Why every service has one shape](#7-why-every-service-has-one-shape)
8. [Why configuration is split the way it is](#8-why-configuration-is-split-the-way-it-is)
9. [The remaining decisions, and the reason for each](#9-the-remaining-decisions-and-the-reason-for-each)
10. [What is absent](#10-what-is-absent)
11. [Summary](#11-summary)

---

## 1. The question m6 answers

**m6 began as a question: what does it look like to treat an HTTP server like a high performance trading engine?**
Everything in this document is the answer to that, plus what reality added.

This section gives the premise, what reality adds, why microseconds are not the target on their own, why a site runs its own edge, what an integrated stack buys, what m6 gives up, and where m6 is the wrong choice.
It ends with the event loop every service runs.

### 1.1 The premise: a path is a symbol

**A trading engine measures tick to trade, and its job in that window is a symbol lookup and a write.**
On bare metal with a kernel-bypass network stack that window is just under a microsecond.
The reference class is the ExaNIC family with its `exasock` sockets library, now sold as the Cisco Nexus SmartNIC.
Table 1 gives its published latencies.

| path | measured | how |
|---|---|---|
| native API, small Ethernet frame | 780 ns application to application | direct access to the card |
| `exasock`, TCP | 930 ns | `sockperf`, through the socket acceleration library |
| `exasock`, UDP | 880 ns | `sockperf`, through the socket acceleration library |

**Table 1: published ExaNIC latencies, the floor m6's design was aimed at.**
Take from it that a purpose-built card with kernel bypass only just breaks one microsecond, so a microsecond is the state of the art.

`exasock` reaches those figures by intercepting the Linux socket calls and sending directly to the card.
Sources: the [ExaNIC sockets guide](https://exablaze.com/docs/exanic/user-guide/sockets/), the [ExaNIC benchmarking guide](https://exablaze.com/docs/exanic/user-guide/benchmarking/) and [cisco/exanic-software](https://github.com/cisco/exanic-software).
m6 is a userspace server on a general-purpose stack with TLS and three HTTP versions in front of the lookup, so 1 µs is an upper bound on ambition, and §1.2 gives the distance from it.

**HTTP has the same shape, and most assets on most sites are static.**
A path is a symbol and a response is the trade, so serving a page is a hash map lookup and one write of a buffer and a length.
Four rules follow, and they are the whole of m6's hot path:

- the page is **already in memory**, so nothing is read from disk to answer
- the page is **already compressed**, so nothing is encoded per request
- **nothing allocates** on the critical path
- **no compute happens** that the answer does not require

§4.2 and §4.3 are those four rules as implementation, and §1.3 defends them against crossing a process boundary.

### 1.2 What reality adds, and where m6 is

**TLS (*Transport Layer Security*) adds round trips before the first byte, and supporting HTTP/1.1, HTTP/2 and HTTP/3 adds three wire formats with three sets of conformance rules.**
A public site needs all of it, and all of it sits between the symbol lookup and the client.

**Every figure in this section is an order of magnitude with its conditions attached, and none is a guarantee.**
Table 2 states what each number is, because a target, a component cost and an end-to-end measurement are three different things.
`BENCHMARKS.md` is authoritative for every measured figure, and a number that appears without its conditions is wrong.

| figure | what it is | status |
|---|---|---|
| 930 ns | the best published kernel-bypass TCP round trip, on a purpose-built card (§1.1) | the upper bound on ambition, never measured in m6 |
| ~2.2 µs | a cache hit inside m6, on m6's own timer (`hit_p50_ns`) | measured, component cost, excludes TLS and the network |
| ~13 µs | a Unix socket round trip | the reason for §1.3 |
| ~226 µs | an HTTP/2 cache hit end to end on the benchmark host | measured, includes TLS, loopback and the client |
| 5 ms | request to response for a real user | the target that governs every latency decision (§1.4) |

**Table 2: the figures this section uses, what each one measures, and which is a target.**
Take from it that the microsecond figures are component costs and the millisecond figure is the goal.

The 2.2 µs cache hit is the trading-engine window, within roughly two to three times the 930 ns that dedicated hardware reaches.
The 226 µs is what a client on the same machine sees, and almost all of the distance between the two is TLS and the protocol stack.

### 1.3 Why a cache hit must not leave the process

**A Unix socket round trip costs about 13 microseconds at the median, against the 2.2 microsecond cache hit from §1.2.**
A design that crosses a process boundary to reach its cache therefore spends roughly six times as long on the crossing as on the answer.
So the response cache lives in the same address space as the TLS stack, and the rest of the edge is built around that decision.

A separate cache tier is a process boundary by construction.
A cache such as Varnish behind a TLS terminator such as nginx or hitch crosses that boundary on every hit before TLS can send.
Shipping two existing servers together cannot close the gap, because the process boundary is the cost, and §6.1 applies the same argument to code.

### 1.4 Why microseconds are not the target on their own

**A host network stack is measured in milliseconds, and a page reaching a person takes several more.**
Ten microseconds of saved work is invisible against that.

**So the target is 5 milliseconds from request to response, for a real user.**
The microsecond work leaves that budget to the network.
Every latency decision in this document answers to the 5 millisecond figure.

### 1.5 Why a site runs its own edge

**A cache-only node near the user gives proximity and one source of truth.**

**Proximity.** Most of the 5 millisecond budget is distance, so a node close to the reader is most of the answer.
No measurement against a commercial network has been made.

**One source of truth.** A cache-only node holds no content of its own, so content has exactly one origin and every other node is a copy with no authority.
A commercial content delivery network is a second system with its own view of the site, and that view can diverge from the origin's.
This half is structural and needs no measurement.

A cache node is `m6-http` with no content behind it, so §4 has one edge to reason about.

### 1.6 The second question: what does an integrated stack look like?

**A conventional stack for the same site is six systems: a content delivery network, a cache, a web server, a language runtime, a database, and an authoring framework.**
That is six things to learn and configure, with code and configuration written in all of them.

**m6 is one stack from Markdown to the edge, and the parts know about each other.**
Table 3 gives three things that awareness buys, each in the code today.

| what a mutually aware stack can do | how m6 does it |
|---|---|
| answer a monitor without rendering a page | `/health` is answered before routing, the cache and any backend, so a check costs one JSON serialisation (`m6-http/src/main.rs`, `m6-http/src/health.rs`) |
| measure the site without changing it | a request carrying `no-cache` reaches the backend and the stored entry survives. A monitor poll moves no cache counter, and a warming fetch mints no session and logs no miss |
| warm itself | the edge holds its parsed route table, so it queues one fetch per concrete route per encoding (identity, gzip, br). It skips patterns, the error path and routes marked `no-store`, and drains one fetch per loop iteration, so warming cannot delay startup or stampede the origin |

**Table 3: three things a mutually aware stack can do, and how m6 does each.**
Take from it that every row needs one part of the stack to act on something another part owns.

### 1.7 The third question: HTTP as the only dynamic protocol

**The only dynamic communication between m6 processes is HTTP.**
The only shared state is static: one site-wide configuration file that sets the routes.
That gives one protocol to debug, secure, log and measure, and any process can be replaced by anything that speaks it, in any language (§3.1).

### 1.8 What this gives up

Table 4 gives what the single-threaded, in-process-cache model surrenders.

| given up | why it follows from the design |
|---|---|
| multi-core use within one process | one thread occupies one core. m6 scales by running more instances, and §3.4 makes pool membership automatic |
| HTTP/1.1 throughput against a mature server | a new connection per request pays a TLS handshake each time, and one slow answer holds its own process until it finishes |
| zero-copy file serving | content is copied through userspace buffers, so a server using `sendfile()` pulls ahead as responses grow past a few kilobytes |
| kernel TLS | Linux can offload record encryption to the kernel, and m6 encrypts in userspace (`docs/POSITIONING.md` records it as unmeasured) |

**Table 4: what m6's design surrenders, and why each follows from it.**
Take from it that every entry follows from one thread owning the hot path and the cache living in its address space.

### 1.9 When m6 is the wrong choice

**A site dominated by HTTP/1.1 traffic or by large file responses is better served by a mature conventional server.**
So is a deployment that must saturate many cores from one process.
m6 fits a site where most assets are static, where the common request is a cache hit over HTTP/2 or HTTP/3, and where one stack that understands itself is worth more than the last increment of raw throughput.

### 1.10 The event loop

**Every m6 service runs one single-threaded loop in its own `main`, and the loop is the only place runtime work waits.**
The loop is the edge-triggered `epoll(7)` contract plus `listen(2)`'s backlog.
`docs/m6-app-anatomy.md` §2 shows the shape in full, and §7.1 gives the reason each service writes its own.

Twelve rules hold for every loop.

1. Start and stop may block. The loop never does.
2. If something can be done at start or at stop, do it there.
3. One thread per process. More cores means more processes.
4. No thread per request and no hand-off to another thread.
5. No timer that checks for something a descriptor could signal. The one timer is the accounting tick.
6. Each service owns its loop. Nothing in m6-core takes it over.
7. Failures at start are errors (exit 2).
8. No heap allocation on the hot path, explicit or hidden. Everything the loop uses is allocated at start, so growing a `Vec`, building a `String`, `format!`, `Box::new` and inserting into a `HashMap` are all forbidden in the loop. The connection table's growth is the one allowed exception.
9. No buffers on the stack in the run loop. Read and write through the slot's buffers, because `[0u8; N]` writes N zeros every time it runs and a frame over 4 KiB probes the stack page by page.
10. No `unsafe` in a service's loop. The crate root says `#![deny(unsafe_code)]`.
11. The poll is edge-triggered (`m6_core::platform::Poll`, over `mio`, with `m6-core/examples/echo.rs` as the reference loop). A socket is reported once per change, so each turn reads and writes until one side returns `WouldBlock`.
12. Readiness is re-read after the handler. Drive first, then compare `interest()` with what is registered and reregister only on a change. Reregister once when a test seam fakes a `WouldBlock`, because the kernel never returned `EAGAIN` and only `EPOLL_CTL_MOD` re-reports readiness. A loop change that is green on macOS is unproven until the Linux run.

**A process has three phases, and the loop is the middle one.**
Start, from `main` until the loop begins, may block freely.
It reads configuration, keys, certificates, templates and site content, opens and migrates databases, and resolves host names with the ordinary blocking lookup.
Run never blocks, and every wait goes through the loop.
Stop, after the loop exits on a signal, may block freely: it flushes logs, closes files and removes the socket.
A failure at start exits 2, and a change to anything read at start needs a restart.

**m6-core supplies parts, and the service writes the loop.**
The service's `main` owns the `Poll`, the token `match` and every descriptor.
It calls plain types with methods: `platform::Poll`, `Events`, `Interest` and `Token`, `platform::Signals`, `event::Timeouts`, `event::Slots`, `event::LoopTrace`, `h1::ServerConn`, `client::Upstream` and `net::listen_unix`.
`m6-core/examples/echo.rs` is the smallest complete loop, and `docs/m6-core-reference.md` §4.1 names every part.

**`m6_core::platform` is the only place an OS call lives.**
It owns readiness, the clock, signals, non-blocking sockets, the open-file limit and a child's exit, and no other crate names `mio`, `nix`, `libc` or a `cfg(target_os)` (`docs/m6-platform.md`).

**Every handler in a turn sees one clock reading.**
`m6_core::platform::now` is the monotonic clock, and the loop reads it after the wait and passes that time to every handler in the turn.
The wait's own bound comes from `event::Timeouts` on the same clock.

**Waiting on another service is an `enum` of steps.**
The connection's state names the step, the outgoing work gets its own token and deadline, and the loop calls its `drive()` when the reactor reports that token ready.
A step that is unfinished returns to the loop.
A deadline that passes first answers 504 and drops the outgoing work, which closes its socket.
A multi-step protocol such as SMTP is the same enum with a `match` inside `drive()`.

**A busy SQLite statement gets a deadline.**
A statement that answers `SQLITE_BUSY` while the loop is running sets a short deadline on that connection, and the loop tries again when it passes.
`m6-core/src/sqlite.rs` sets `busy_timeout` to zero after the start-time open, because a wait in the loop holds every request behind it.

**A regular file is always ready to every poll.**
A read or write of one in the loop is kept small, and the code that adds it names the reason.
`docs/m6-log.md` states the same rule for a log sink: a regular file never returns `EAGAIN`, so it is never registered with a loop.

**The queue is the kernel's listen backlog, and the connection table is the process's own limit.**
The listener accepts until the kernel has no more, and connections beyond the table wait in the backlog, so the process queues no work it cannot finish.
The table starts with 1024 slots (`m6_core::limits::CONNECTIONS`), each with its buffers.
It doubles when full, never shrinks, and stops at the process's open-file limit, raised to the hard limit at start.
At that ceiling the loop stops accepting.

**Signals arrive as a descriptor, and `Lifecycle` says what each one means.**
`m6_core::platform::Signals` is signalfd on Linux and a kqueue filter elsewhere, so a shutdown is readable in the same `match` as every socket.
`m6_core::signal::Lifecycle` turns each signal into the lifecycle line to log and the decision to stop, and `m6_core::signal::block()` is the first statement of `main`, before anything that could start a thread.

**Logging in the loop is `m6_core::log`.**
A line is buffered and written on the accounting tick, and a full stdout is watched for writability under its own token, so the loop never waits on the log.
`docs/m6-log.md` has the interface.

**The check is the clippy ledger and the counting allocator.**
`clippy.toml` refuses threads, sleeps, blocking connects and socket timeouts in m6 code, and every non-test use of a refused call carries an `#[expect(clippy::disallowed_methods, reason = "...")]` that names why it is allowed or which job removes it.
`m6_core::testkit::alloc_count` counts every heap allocation, so a hot path's claim of zero is a test result.
What remains on the TLS path is rustls's buffer for each encrypted record.

---

## 2. The shape that follows

**The process split follows from §1.**
Table 5 gives the three constraints that decide it and what each one forces.

| constraint from §1 | what it forces |
|---|---|
| a cache hit must not cross a process boundary (§1.3) | TLS termination and the response cache live in one process, and that process holds the public port |
| a backend may be written in any language (§1.7) | the boundary below the edge is a protocol, so everything else is a separate process reached over HTTP/1.1 on a Unix socket |
| one stack that knows about itself (§1.6) | one shared configuration describes the whole set, so no part has to be told about another by hand |

**Table 5: the three constraints from §1 and what each forces about the shape.**
Take from it that the first constraint puts TLS and the cache together, the second puts everything else outside, and the third makes the result one system.

Table 6 names what that produces.

| binary | one job | where it sits |
|---|---|---|
| `m6-http` | terminate TLS, rate limit, cache, route, ask the check service for identity and enforce route authentication, proxy to backends | the public port. A second process of the same binary answers `:80` with a redirect to HTTPS |
| `m6-html` | render HTML from templates and JSON data | behind the edge |
| `m6-file` | serve files from the filesystem | behind the edge |
| `m6-auth-server` | verify credentials, sign *JWTs* (*JSON Web Tokens*), and answer the identity check | behind the edge |
| `m6-monitor` | poll each node's `/health`, read every node's metrics and traffic documents from the monitor host's journal, measure the tunnel round trip and WireGuard handshake age where configured, keep a size-capped history of every poll and of crawler sightings in one SQLite file, and serve one report and the history | behind the edge |

**Table 6: the five serving binaries, the one job each has, and where each sits.**
Take from it that `m6-http` is the one binary reachable from the internet, it runs in two modes, and everything else answers HTTP/1.1 on a Unix socket behind it.

Two command line tools serve nothing: `m6-md` converts a directory of Markdown into one JSON file, and `m6-auth-cli` manages users and groups against the auth database.

**A site is data and configuration only**: `site.toml`, one config file per backend process, templates, assets, and content as JSON.
The m6 binaries are found through the system path, so a site holds none of them.
Every process writes its log to journald's native socket under systemd and to stdout elsewhere (§3.3), so no process keeps a log directory.

---

## 3. Why a family of processes

A single process with the cache and TLS (§1.3) is the minimum m6 could have been.
The other processes buy a backend in any language, a contained crash, an existing process manager, and capacity added without editing configuration.

### 3.1 Why the boundary is a wire contract

**A backend is reached over HTTP/1.1 on a Unix socket, so it can be written in any language.**
`m6-backend-protocol.md` specifies the contract, and six reference backends in C, C++, Go, Python and two forms of Rust under `m6-http/tests/backends/` implement it (`docs/m6-backend-examples.md`).

This constrains §6: `m6-core` is a convenience for Rust and is never the only readable definition of any part of the contract.
Behaviour a backend depends on that exists only as Rust is a hole in the specification, and the fix is to specify it.

### 3.2 Why one job each

**A process with one job can be restarted without taking anything else down.**
A template that fails to compile stops HTML rendering and leaves static files and TLS serving.
The `:80` redirect listener is its own process for the same reason: a slow client there cannot stall the process holding TLS connections, and it builds no QUIC stack, cache or route table.

**One job also means one config, one log stream and one unit per instance.**
`m6-html` takes a route table in its config and serves every HTML route in it, so that config is a complete picture of what it does.
Splitting it per route type would multiply units and log streams and leave each config describing a fragment.

### 3.3 Why systemd owns the lifecycle

**systemd is already on the machine and manages processes better than anything m6 could implement.**
Restart policy, resource limits, dependency ordering, log capture and service isolation are its job.
So `m6-http` spawns nothing, monitors nothing and restarts nothing, and expects its backends to be running.

Two consequences follow.
Every process writes structured JSON lines to journald, one datagram per line, and the journal rotates them (`docs/m6-log.md` §5).
A crashed backend restarts in seconds, which makes §4.5 affordable.

### 3.4 Why pool membership is discovered

**Adding capacity to a backend pool should take no configuration edit**, because §1.8 makes scaling the answer to load.
A pool is declared as a socket glob, and `m6-http` expands that glob once at start (`m6-http/src/pool.rs`).
Backends therefore start before m6-http (systemd `After=` on the backend units), and a new instance joins its pool when m6-http restarts.
An explicit list would need a config edit to add capacity, in the path of the one operation an operator performs under pressure.

Requests go to the members in turn, so each member takes the same share of the load.
A request whose connect fails tries the next member, so a stopped backend costs one refused connect and the request is still served.
A backend that restarts on the same path serves again on its next turn.

---

## 4. Why one process faces the internet

The edge is the only process on a public port when `[server] reuse_port` is off, its default.
Turning it on lets several m6-http processes share the public port.
This section gives the reason the protocol burden is concentrated at the edge, why it runs one thread, how the cache keys and bounds, why it distrusts its clients, how errors are served, how invalidation works, and what `/health` answers.

### 4.1 Why the protocol burden is concentrated

**One process terminating TLS and three HTTP versions means one place to get them right.**
Table 7 gives the recorded conformance floors.

| suite | target | score |
|---|---|---|
| h1spec | `m6-auth-server`, `m6-file`, `m6-html`, `m6-http` in redirect mode | 32/32 each |
| h2spec | `m6-http` | 146/146 |
| h3spec | `m6-http` | 47/49 |

**Table 7: the recorded conformance floors, from `tools/conformance-scores.txt`.**
Take from it that HTTP/1.1 is measured on four targets because everything behind the edge speaks it, and that HTTP/3 is the one suite short of full marks (§6.4).

TLS, the frame layers and *HPACK* (*HTTP/2 header compression*) live in `m6-http` alone, so the protocol code has one home and one test surface.
A rule applied once at the edge holds for every backend behind it, in whatever language, which is what makes §4.4 possible.

### 4.2 Why one thread and no async runtime

**A cache hit is a hash map lookup and a write, and an async runtime adds overhead to that.**
This is §1.1's four rules as an implementation.
m6-http runs one `m6_core::platform::Poll` loop, and one thread owns every connection's state, so the request path has no cross-thread synchronisation and the header scan on the hit path allocates nothing.
Network descriptors and backend descriptors sit in one readiness set, so there is nothing to coordinate between.

The cost is a state machine over one loop, which is harder to write than a thread per request.
The benefit is that no interleaving exists to reason about under load.

### 4.3 Why the cache keys and bounds as it does

**Two responses that differ in bytes are two entries, and the rest of the key follows.**
The key is the path, the query string and the content coding.
Each coding is a different body, so it is a different entry, stored when a client asks for it.
The query string identifies the resource, so it is part of the key.

**The cache has a ceiling because memory does.**
It holds 128MB of response footprint by default and evicts to 87.5% of that when passed, so a large site degrades to a lower hit rate.

**Two rules decide what may be stored, and both stop one client's response reaching another.**
A response varying on anything except `Accept-Encoding` is never stored, because the key cannot express another dimension.
Whether a backend compresses is declared once in `site.toml`, and the edge reads it to decide whether to vary on `Accept-Encoding`.
The backend reads the same declaration and refuses to start when it disagrees with what the backend does (`m6_core::compress::check_declared_support`).
The dangerous direction is a backend that compresses while the declaration says it does not: the edge stops varying on `Accept-Encoding`, and a compressed body can be replayed to a client that asked for identity.

### 4.4 Why the edge distrusts its clients

**A header the proxy generates is a statement only the proxy can make truthfully, so a client's copy is removed on the way in.**
`x-auth-claims` and `x-forwarded-for` are stripped from every inbound request, on every protocol, before routing (`UNTRUSTED_INBOUND` in `m6-http/src/forward.rs`).

Backends resolve a repeated header by first match, and a proxy appends its own value after the client's, so a forged copy would be the one that wins.
A forged `x-auth-claims` is an authentication bypass, since any client could assert membership of any group.
A forged `x-forwarded-for` is a rate-limit bypass, since rotating the value defeats a per-address counter.
Every per-address decision in m6, including the login throttle, rests on that strip.

**Two more rules hold because the edge is the one place they can be applied once.**
The security response header set is filled in at serialisation, so cache hits, backend responses, generated error pages and refusals all carry it, and a backend setting its own value for one of them wins.
A request whose framing does not parse ends the connection, because after a framing error there is no way to know where the next request starts, and guessing is how a smuggled request gets through.

### 4.5 Why errors go through one route

**One route means one template, and a new piece of error context needs no configuration change.**
The edge fetches the configured error path with the status and the original path as query parameters, and returns the rendered HTML under the original status code.
A request already on the error path is answered directly, which stops recursion without tracking depth.

**systemd covers the window in which no pool member can render an error page.**
A crashed backend restarts in seconds (§3.3), so the window is short and a plain status code covers it.
`[errors] mode` selects the answer: `status` gives a status code with an empty body, `internal` gives minimal generated HTML, and `custom` gives the configured page, falling back to `internal` when no path is set.
A fallback HTML file would need maintaining, validating at startup, holding in memory and serving correctly for every content type.

### 4.6 Why invalidation is one generation ID

**A publish must never let the cache serve old bytes under a new name, so the unit of invalidation is one number the source owns.**
A generation is the content a set of backends is serving at one moment, and the source node holds the `u64` that names it (`m6-http/src/genid.rs`).
A backend's only part in a publish is answering `POST /genid` with 204 after it has rebuilt its content.

A `POST /genid` on the source runs one change sequence.
It sends `POST /genid` with an empty body to every member of every socket pool, on that member's own socket, and waits for each answer.
It then rebuilds the route table from the config loaded at start, flushes the cache and the held error pages in one assignment each, advances the ID, and writes it to `$STATE_DIRECTORY/genid`.
A member that fails is logged with its socket path and counted, and the POST still answers 200, so the operator sees a non-uniform generation and the deploy still succeeds.
The ID advances before any POST is answered, so a publisher that sees 200 knows the new generation survives a restart.

**The store rule keeps a stale render out of the cache.**
Every response a backend returns is stamped with the ID that was current when the request was dispatched.
The source stores a response only while that ID is still current, so a response rendered before the publish cannot enter the cache after it.
A cache node stores only a response whose stamp equals the ID it holds, so a node cannot fill from an upstream one generation behind.
A local answer such as a health check carries no stamp and is never stored.

**A cache node learns the ID from its upstream.**
Its `[genid] upstream` names a `[[backend]]` with a `url`, and its `token_file` holds a read token.
It sends `GET /genid` over the transport its URL backend's scheme names, at start and once `heartbeat_secs` (default 10) has passed since the last answer, and a new ID flushes its cache in one assignment.
A node with no ID stores nothing, so a failed first heartbeat is due again after one second (`NO_ID_RETRY` in `m6-http/src/genid.rs`), which matters when `heartbeat_secs` is large.
That is the whole invalidation: one number, one flush, no per-path map.

The disk cache follows the same rule.
Its layout is `<dir>/<genid>/<key hash>`, and the other generation directories are removed over the slices that follow (`m6-http/src/disk_cache.rs`).
A restart holds the disk copy aside until the first heartbeat names its ID: a match installs it, another ID discards it, and no answer installs it unverified so the node serves from disk while the source is down.
The next answer settles the copy.

### 4.7 Why `/health` answers three states and names no dependency

**The endpoint a monitor hits most often must cost one JSON serialisation and tell a stranger nothing.**
`/health` is answered ahead of routing, the cache and every backend, and it reads flags the loop already keeps (`m6-http/src/main.rs`).
It probes nothing, so its cost is one serialisation (`m6-http/src/health.rs`).

`200` with `{"status":"ok","node":"<name>"}` says the process is alive and every dependency it reads is answering.
`503` with `{"status":"degraded","node":"<name>"}` says the process is alive and a dependency is failing.
A process that is gone, or whose listener refuses, gives no answer, and m6-monitor reads that as unreachable.
The monitor reports `ok` for a 200 whose body says ok, `degraded` for any non-200, `unreachable` for no answer, and `unknown` for a 200 with no body (`m6-monitor/src/poll.rs`).

**The body names the node and no dependency, because `/health` is public and unsecured.**
A named dependency would tell a stranger which part of the node to wait out.
The dependency appears in the warn line the process logs when it fails and the info line when it recovers, on the change only.
On a cache node the dependency is its origin.
The flag is the follower's own `down` bit, set where a heartbeat or a forwarded request observes the failure and cleared by the next heartbeat answer.
A successful client request clears nothing, because the heartbeat observes the recovery and is due immediately after a failure.

---

## 5. Why authentication sits where it does

Authentication is enforced at the edge, which asks the auth service for the identity on each credential gate, and a public route pays one branch for it.

### 5.1 Why a public route costs one branch

**A static site is the base case, so authentication costs as close to nothing as possible on a route that has none.**
Three things make it cheap.

**A site with no protected route configures no checker.**
It needs no `[auth]` section, no check backend and no auth process.
With no `[auth]`, no credential gate can be configured, so there is no authentication configuration to get wrong.

**The caching decision short-circuits on one flag.**
Whether a response may enter the shared cache turns on whether its route requires authentication, and the route table carries one boolean saying whether any route does.
A site with none answers that question without a route lookup, which matters because it is asked before every cache lookup on every protocol.

**A protected route is one check on the route already matched.**
`require` is an optional field on the route entry, so a public route pays a single branch.
That is the price of one route table, at one branch against a path measured in microseconds.

### 5.2 Why the check is one network call

**m6-http decodes no token and holds no key, and every credential gate asks m6-auth-server (`m6-http/src/auth.rs`).**
A gate makes one `POST /check` on a Unix socket under a 50 ms deadline (`CHECK_DEADLINE`), and the answer is used once.
The gates are a route `require` and a source node's `GET` and `POST /genid`.
A node with `[auth] backend` therefore needs a running checker for its protected routes and for `/genid`.
The measured parts of one check add to about 111 µs, and a request with no credential makes no call at all.

Nothing caches the answer, so a role change or a deleted user counts from the next request.
The service is the only holder of the key, so key rotation needs no coordination at the edge.
The check socket is derived beside the primary socket, so a deployment can run the login side and the check side as two processes and keep a login's bcrypt off the check loop.

### 5.3 Why enforcement happens twice

**Route-level enforcement at the edge is a boundary that holds whatever each backend implements.**
An unauthenticated request to a protected route never reaches a backend, and the requirement is declared once in `site.toml`.

**Resource-level checks stay in the backend because a route cannot express them.**
Whether this user may read this document depends on the document.
The identity the check answered is forwarded so the backend can answer that without its own check, which is why `x-auth-claims` is the header §4.4 protects.

---

## 6. Why m6-core exists, and where its boundary falls

`m6-core` is the PHP of m6: a generic library of components a service assembles a web application from.
Breadth is the intent and singularity is the rule.

### 6.1 Why breadth is the intent

**A service should be able to build what it needs from one dependency**, which is §1.6 applied to the code.
Core holds templating, compression, minification, cookies, multipart bodies, an SMTP client, an outbound HTTP client, host metrics and firewall reporting beside the HTTP layers, and a component a website might want belongs in it.
A new service is small as a result: `m6-html` is one `main` and a route table, and renders every HTML page a site serves.

Feature gates pay for the breadth.
A service names what it wants, so a binary carries what it uses, and a command line tool such as `m6-md` links no QUIC stack or TLS library.

Table 8 gives the sorts of component core holds.

| sort of component | what it covers |
|---|---|
| HTTP semantics | the version-independent rules: conditional requests, content negotiation, repeated header fields, content types with charset |
| HTTP/1.1 | the one request parser and response writer, and the blocking read adapter over it |
| Service scaffolding | the reactor, the signal descriptor, the deadlines, the connection table, routing, the socket helper and the shutdown sequence |
| Configuration | parsing and validation, and secrets merging |
| Request and response | what a handler receives and returns, the layered request dictionary, cookie construction, multipart bodies |
| Content | gzip and brotli, HTML, CSS, JSON and JavaScript minification, and the renderer seam with its template implementation behind it |
| Safety | path parameter validation and traversal refusal, request size caps, cryptographic token generation |
| Observability | logging setup, the analytics record format, the health and performance endpoints, host load, memory, disk and temperature, firewall counters, newline-delimited JSON |
| Test kit | standing a service up, claiming a socket without a race, driving it and tearing it down |
| Errors and helpers | the one error type and the status each variant becomes, dates and slugs |

**Table 8: the sorts of component `m6-core` holds.**
Take from it that core spans the HTTP specification, the service lifecycle and the operational surface, and that a service links only the sorts it names.
`m6-core-reference.md` gives each module and its interface.

### 6.2 Why each thing in core exists exactly once

**A security boundary with two implementations has two behaviours, so every concept has one implementation.**
Path validation is the example: one validator allows alphanumerics, `-`, `_`, `.`, and `/` only where a route's parameter spans segments, and refuses `..` anywhere as a substring.
The same holds for the HTTP/1.1 parser, precondition evaluation, content-coding negotiation, cookie formatting, signal handling and route matching.

**The rule: code moves into core when it has more than one consumer, and single-consumer code stays with its consumer.**
A library with one consumer is that consumer's code in another directory, and moving it there adds an abstraction boundary nobody crosses.

### 6.3 Why HTTP/2 and HTTP/3 stay at the edge

**They have one consumer permanently, because `m6-http` is the only process that terminates a public connection.**
The consumer count follows from the architecture and will not change.

The HTTP/3 path in `m6-http/src/main.rs` imports only `H2Request` and `RequestRef` from the HTTP/2 module (line 37), plus one header helper.
It shares no frame layer, no HPACK and no stream state machine with HTTP/2.
HPACK and *QPACK* (*HTTP/3 header compression*) are different algorithms, and HTTP/2 frames and QUIC streams are different transports.
The one thing the two versions share is version-independent semantics, which is in core.

**The cost:** the edge keeps the largest and most defect-dense body of code in the project, and conformance to the HTTP/2 and HTTP/3 specifications stays its property.
If a second consumer appears, such as a backend terminating HTTP/2 itself, this is the decision to revisit.

The same reasoning keeps the caching rules at the edge: only `m6-http` evaluates `Cache-Control`, because the edge is the cache and the backends sit behind it.

### 6.4 Why the QUIC stack is a fork

**The HTTP/3 conformance gap was upstream, and measurement established that.**
Released quiche scores 37/49 on h3spec.
Twelve failures sit below the layer m6 works at: eight where a first-flight transport error leaves a correctly built connection close unsendable, two where reserved packet bits are accepted without validation, and two in QPACK.
None is reachable through quiche's public interface (`tools/conformance-scores.txt`).

The fork is quiche master plus two open upstream pull requests that close ten of the twelve, which gives 47/49.
It is pinned by revision (`m6-http/Cargo.toml`) so the dependency cannot move under a reproducible build.
The price is that the transport of a public edge carries changes upstream has not reviewed, and the exit condition is upstream releasing the fixes.

The remaining two failures are QPACK.
quiche reads the peer's QPACK instruction streams and discards them, running a static table only.
Closing the gap needs new validation on the connection path with per-stream buffering for instructions split across reads, which risks unbounded memory on a stream a peer controls.
That trade is declined for two tests about how a hostile peer is refused.

### 6.5 Why the allocator is mimalloc and the heap is pinned as it is faulted

**Latency is the metric, so the kernel calls a heap makes are the cost to remove, and every service uses one allocator, mimalloc.**
`m6_core::alloc::M6Alloc` wraps mimalloc, and every service binary installs it as its global allocator (`m6-core/src/alloc.rs`).
The allocator never returns freed memory to the OS, and `alloc::reserve` takes `[limits.heap_reserve]` from the OS once at start.
m6-http reserves `[limits.cache_bytes] max` plus 64 MiB, rounded up to the huge page size, so the cache is carved from one region and a warm service makes no `brk`, `mmap`, `munmap` or `madvise` call on the request path (`m6-http/src/limits.rs`, `heap_reserve`).

**Pinning is a separate promise, kept page by page.**
`[server] lock_memory` defaults true and calls `mlockall(MCL_CURRENT | MCL_FUTURE | MCL_ONFAULT)` after the reservation and before the first request (`m6-platform/src/linux.rs`, `lock_memory`).
`MCL_ONFAULT` locks each page when the program first touches it, so memory that is reserved and unused stays unresident, and a page a cache hit reads was faulted during warm-up and stays locked.

**A finite limit is refused at start.**
The kernel enforces the soft `RLIMIT_MEMLOCK`, so m6-http reads that limit first and refuses any value but infinity.
The error names the remedy, and `[server] lock_memory = false` runs unpinned (`docs/m6-user-guide.md` §7.4).
A kernel older than 4.4 fails the call with `EINVAL`, which is answered with an error naming the version and the same key.

The start logs the resident set and the address space on either side of the pin, and `m6_platform::process::huge_page_bytes` reports how much sits on huge pages.
`docs/m6-platform.md` and `docs/PERFORMANCE.md` own the interface and the numbers.

---

## 7. Why every service has one shape

Every m6 service except the edge is a `main` with its own event loop, and the loop is the same shape in each.
One shape means one loop, one state model and one shutdown sequence serve all of them.

### 7.1 Why each service writes its own loop

§1.10 states the loop and its rules.
This section gives the reason each service writes its own.

**The service's `main` owns the poll, the token `match` and every descriptor, so the loop is visible where it runs and a service reads top to bottom.**
`docs/m6-app-anatomy.md` is that shape in full.

**A second thread would need its own state and a hand-off, and a hand-off costs a context switch and a cold cache.**
A `[thread_pool]` table is refused at start with exit 2, so an operator cannot ask for threads by accident (`m6-core/src/config.rs`).

**A user-space queue would accept work the process cannot finish, which turns overload into growing latency.**
The kernel's listen backlog is the queue, and the edge's own deadline is the backpressure.

**A silent peer is the one wait no descriptor reports.**
Its read deadline is a `Timeouts` entry that the loop checks each turn.

### 7.2 Why the request dictionary is layered

**Most of what a template reads is identical for every request, so it is built once and shared.**
Configuration keys, global parameter files and a route's static parameter files are merged once at start and shared behind a reference count, and only what varies is allocated per request.

**The order within it is a security property.**
Built-in keys such as the request path and the current date go in after every parameter file, so a parameter file cannot override them.
A content file that could redefine the request path could make a page misreport which URL it is.

### 7.3 Why startup and shutdown belong to core

**A service stops on SIGTERM by refusing new connections, removing its socket and exiting.**
It does not wait for requests in progress: the supervisor restarts it on a deploy, and a request cut off by a restart is acceptable.
The first signal exits 0 and the second exits immediately.
The sequence is the service's `main`, with m6-core supplying the parts it calls: `signal::block()` first, `Lifecycle` for the lines and the signal meanings, and the socket path that `Lifecycle::complete` unlinks.
A service supplies two pieces of data: the name in its lifecycle lines and the socket to unlink on every exit path.
Core owning the lifecycle lines means a journal search for a shutdown means the same thing for every unit.

**Blocking signals is the first statement of `main`, the one ordering rule in m6 that cannot be relaxed.**
A thread inherits the signal mask as it stands when the thread is created, and a library a service links can start one.
A thread started before the mask is set has SIGTERM unblocked, the kernel delivers a process-directed signal there, and the default disposition kills the process.
A supervisor counts death by the signal it sent as a clean stop, so the only symptoms are a missing log line, a wrong exit status, and a socket file left behind for the edge to keep in a pool.
The install asserts the mask is already set and refuses to start otherwise, because nothing else about that failure is visible.

---

## 8. Why configuration is split the way it is

Every process reads two files.
`site.toml` holds the state every process of the site shares, and the process config holds that process's own state.
For m6-http the process config is the system configuration, which holds what differs between nodes and environments.
For a backend it is the backend's own config, and the reserved key `site_toml` says where `site.toml` is.
An operator and a developer own different things, and the split follows that ownership.

### 8.1 Why the system configuration holds node state and wins

**The bind address and the TLS certificate paths differ between environments**, so the system configuration holds them.
It holds the rest of a node's own state too: `[node]`, `[auth]`, `[genid]`, `[cache]`, `[limits.<name>]` and the node-specific `[server]` keys.
`m6-site-toml.md` lists every setting and the file it belongs in.
`site.toml` holds the state every node shares.
m6-http refuses the start with exit 2 when a key sits in the other file (`misplaced_keys` in `m6-http/src/config.rs`).
The system configuration is a required argument, because one code path is simpler to specify and test than a branch for whether it was supplied.

**It wins on conflict so that a deploy cannot change where a server listens or which certificate it presents.**
An operator needs that to hold whatever is deployed over the top, and everything else comes from `site.toml` unchanged.
As a result `site.toml` carries no password and no certificate path, which lets a site's repository be published.

### 8.2 Why a key may have only one owner

**A key set in both a configuration and its secrets file is refused at startup, because a file that is overridden looks the same as a file that is correct to a reader of that one file.**
An absent required value fails loudly by itself and names the missing key, so the overlap adds no safety.

### 8.3 Why TLS is always on, and only at the edge

**HTTP/3 requires TLS, so supporting plain HTTP would mean two code paths for one saving.**
Development uses a locally trusted certificate, which makes it a two-command setup and keeps development and production identical in the TLS layer.

**Traffic between the edge and a backend crosses a Unix socket with no TLS**, because a Unix socket is local, faster than loopback TCP, and has no network exposure.
The trust boundary is the edge, the concentration §4.4 relies on.

---

## 9. The remaining decisions, and the reason for each

The sections above shape the system, and these decisions shape working with it.
Table 9 gives them by area.

| area | decision | why |
|---|---|---|
| routing | a fixed route beats a pattern, and two routes a request could both reach are a config error at load | the answer cannot depend on the order of blocks in a file somebody edits later, and a request needs no tie-break |
| routing | a route lists the methods it answers, and no list means every method | a code POST handler and a config GET page can share a path |
| routing | `{*name}` spans segments and is legal only last | a wildcard in the middle has no single correct split |
| routing | a route naming a handler no code registered is fatal at startup | a route that 404s while the config says it serves is an outage that looks like a missing page |
| routing | the same parameter syntax in `site.toml` and in a backend's config | one syntax to learn, and one matcher to be correct |
| request data | path parameters are validated before use, and a traversal answers 404 while a malformed value answers 400 | a 400 to a traversal confirms the router recognised it, which tells the sender the payload is worth varying |
| request data | only `application/x-www-form-urlencoded` bodies are decoded, and any other body on a POST is logged loudly | a skipped body otherwise looks downstream like empty fields, with nothing in any log to say it was skipped |
| request data | query parameters appear at the top level and as a nested map, except a key named like one of the route's path parameters, which is only in the nested map | a template wants one form and a handler iterating wants the other, and a query value that replaced a validated path parameter would bypass its validation |
| caching | a route backed by code defaults to `no-store`, and one backed by a template to `public` | a handler computes an answer per request, and inheriting `public` by omission would show one user another user's page |
| caching | invalidation is derived from the site's own declarations at startup | the site already says which files a route reads, and a backend that has to announce a change can forget to |
| caching | a rendered page carries a validator derived from its inputs | a rendered page has no file of its own to date, so a conditional request would otherwise return the whole page |
| processes | a socket path is derived from the config filename | one convention, so a unit file and a pool glob cannot disagree about where a service listens |
| processes | two positional arguments, and one environment variable, `M6_SOCKET_OVERRIDE` | the positional arguments carry the service's configuration, and the variable relocates only the listening socket, is set by a template unit that runs several instances, and is absent otherwise (`m6-app-anatomy.md` §4.1, `socket_path_from_config` in `m6-core/src/server.rs`) |
| processes | a connection serves at most 100 requests before closing (`MAX_REQUESTS_PER_CONN`, and `[server] max_requests_per_connection` on m6-http) | one peer cannot hold a slot indefinitely by pipelining |
| processes | an accepted connection has a read deadline, 30 seconds by default (`DEFAULT_READ_TIMEOUT_SECS`) | a peer that connects and says nothing otherwise holds its slot until it disconnects, and enough of those fill the table while the service looks healthy |
| processes | exit 0 clean, 1 runtime error including a configured TLS or h2c bind failure, 2 configuration or usage error including a QUIC or shared-port bind failure | a supervisor can tell a refused config from a runtime failure, which lets a deploy validate a new binary with `--dump-config` before installing it |
| content | minification runs before compression, a `no-store` response is not minified, and each minifier returns its input unchanged when it cannot parse | ratios are better in that order, a response no cache keeps would pay the cost on every request, and a minifier that does not understand a file passes it through |
| content | inline script minification is off by default | the engine parses scripts as modules, which have different scoping, and it can silently rewrite a valid classic script into a broken one |
| content | text types declare a charset and JSON does not | without a charset clients fall back to a legacy encoding and render UTF-8 as mojibake, and the parameter is undefined for JSON |
| auth | access tokens are short-lived (`access_ttl` 900 seconds) with a longer refresh (`refresh_ttl` 30 days) on its own path | a stolen access token expires on its own, and the refresh path is the one thing that needs to be revocable |
| auth | login is throttled per address | it is the one endpoint where guessing is the attack |
| auth | a requirement is spelled as a group or a role, and an unknown form denies | a typo in a requirement fails closed |
| auth | every credential gate asks the one check service, and the edge holds no key | a role change or a deleted user counts from the next request, and key rotation needs no coordination at the edge |

**Table 9: the remaining decisions by area, each with the reason behind it.**
Take from it that the pattern across them is failing closed and failing loudly.

---

## 10. What is absent

Table 10 lists what m6 omits and the reason for each.

| absent | why |
|---|---|
| a build step | content authoring and web serving are separate jobs, and a separate tool that understands the site directory needs no m6 internals |
| Windows | the deployment model is systemd units and Unix sockets |
| a built-in OAuth2 or OIDC provider, MFA, WebAuthn | no m6 component implements them |
| horizontal scaling of the edge itself | the edge is one process per node by default and scales through caching and more nodes. `[server] reuse_port` lets several processes share the public port |
| HTTP/2 server push | the specification deprecated it, and the server treats a received push promise as an error |

**Table 10: what m6 omits, and why.**
Take from it that each omission is a decision with a reason.

---

## 11. Summary

m6 began as a question: what does it look like to treat an HTTP server like a high performance trading engine?
A trading engine's job in its tick-to-trade window is a symbol lookup and a write, and purpose-built hardware with kernel bypass gets that to 930 ns.
HTTP has the same shape: a path is a symbol and a response is the trade.
Most assets on most sites are static, so a page is one hash map lookup and one write of a buffer already in memory and already compressed.

TLS and three wire formats are what reality adds, and m6 is a userspace server on a general-purpose stack, so a microsecond is an upper bound on ambition.
The figure that governs is 5 ms request to response for a real user, because a host network stack is milliseconds and saved microseconds are invisible against it.
Microsecond work leaves that budget to the network, and a site runs its own edge to spend the budget on distance and to keep one source of truth for its content.

**One measurement decides most of the architecture.**
A process boundary costs about 13 µs against a 2.2 µs cache lookup, so a cache hit must not leave the process.
That puts TLS termination and the cache in one process on the public port, which puts the protocol burden and the trust boundary there too, so one strip of client-supplied headers protects every backend behind it.
Everything else sits behind a wire contract so it can be written in any language, and one shared configuration describes the set so the parts can answer a health check cheaply, measure the site without evicting it, and warm the cache from the route table the edge holds.

m6 gives up one core per process, HTTP/1.1 throughput against a mature server, zero-copy file serving, and kernel TLS.
A site dominated by HTTP/1.1 or by large files is better served elsewhere.

Authentication costs a public route one branch, and a site that protects nothing configures no `[auth]` and asks no checker.
Every gate that exists asks the one checker, so the edge holds no key.
A publish is one `POST /genid`: the source reloads every backend on a Unix socket, flushes the cache and advances one number, and a cache node follows that number on its heartbeat.
`m6-core` is wide by intent and singular by rule, so a security boundary has one implementation.
The heap is mimalloc, reserved once at start and pinned as it is faulted, and a finite memlock limit stops the start.
HTTP/2, HTTP/3 and the caching rules stay at the edge because they have one consumer permanently.

Changing any of this means answering the reason before editing the rule.
