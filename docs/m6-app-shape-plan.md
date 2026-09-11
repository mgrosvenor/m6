# One app shape: what is missing, and in what order

**Goal, stated by the owner: one good way to build every app.** Every service
the same general structure, powered by m6-core, documented well enough to pick
up from outside this repo.

**Constraint, stated with it: no performance regression.**

`m6-app-anatomy.md` is the shape as it exists today. This file is the gap
between that and every service actually having it.

---

## 1. Where each service stands

| service | shape | gap |
|---|---|---|
| `m6-html` | **App** | none |
| `m6-monitor` | **App** | none |
| `render-analytics` | **App** | none |
| `render-contact` | **App** | none |
| `render-cms` | **App** | none |
| `m6-file` | hand-rolled main | one router feature, one response feature |
| `m6-auth-server` | hand-rolled main | two config keys |
| `m6-http` | the edge | **not a gap. See §2.** |

Five of eight are already the shape. Two are close. One is a different thing.

## 2. Why m6-http cannot be an App service, and why that is fine

This question deserves a real answer rather than "it is the edge", because the
answer decides whether "one shape" means seven services or eight.

**m6-http already uses core for everything that generalises.** Ten modules:
`signal`, `h1`, `log`, `http`, `monitoring`, `telemetry`, `conditional`,
`util`, `random`, `firewall`. Nothing in that list is duplicated inside
m6-http. When something in m6-http turns out to generalise, it moves to core;
that is how `h1`, `conditional` and `negotiate` got there.

What it does not share is the **service loop**, for three reasons that are
structural rather than historical:

**Transport.** `App` binds a unix socket. m6-http binds public TCP (:80, :443)
and UDP (:443, QUIC).

**Concurrency model.** `main.rs` line 3 describes itself: *"HTTP/3 over
QUIC/UDP using quiche (sans-I/O) + single-threaded epoll."* `App` is a thread
pool doing a **blocking read per connection**. A QUIC connection is not a
socket you can hand to a blocking worker: every connection on the node is
multiplexed over one shared UDP socket, and connection state advances only
when the event loop feeds packets into quiche. These two models do not
compose.

**Work shape.** An App handler is `Fn(&Request) -> Result<Response>`. m6-http
does not produce a response from a request; it terminates three protocol
versions, consults a cache, proxies to a backend pool, and streams the answer
back.

**Making App able to host m6-http means replacing App's concurrency model with
epoll or async.** That is a rewrite of the loop five working services depend
on, for the benefit of one service that already shares every part of core that
can be shared. It is the change most likely to cost throughput, and the
constraint on this work is no performance regression.

**So "one shape" means: one shape for services, plus an edge that converges on
core for everything except its loop.** The measure of success for m6-http is
not that it becomes an `App`; it is that the count of things it implements
itself keeps falling. Ten core modules today.

**That is not the end of the answer, though.** Leaving it there leaves two
concurrency models in the project, and two of anything is what this migration
exists to remove. §2a is how they converge without rewriting m6-http and
without pretending backends can be single-threaded.

## 2a. The convergence: one I/O model, two execution models

§2 says m6-http cannot be an `App`. That is true and remains true. It is also
not the whole answer, because it leaves the project with **two concurrency
models**, and two of anything is what this migration exists to remove.

The resolution is not "make everything epoll". It is that **App's split is in
the wrong place**, and moving it converges the two.

### Why m6-http can be single-threaded epoll

Because it never blocks. Its cache is an in-memory `AHashMap<CacheKey,
CacheEntry>`, and there is **no disk I/O in its event loop or in `cache.rs`**.
It parses, looks up a hash map, and proxies.

### Why backends cannot be

Because they exist to block. Every `App` handler does blocking disk I/O:
`Request::read_json`, `write_json_atomic`, `list_json`, template loading,
m6-file's `fs::read`. One slow disk read on a single-threaded event loop stalls
every connection on the node. A naive "one model, make it epoll" would be
actively wrong, and would be the highest-risk change available under a
no-regression constraint.

### Where App's split actually is

```
app.rs:1872   listener.accept()            event loop
app.rs:1877   pool.try_submit(stream, ..)  hands the RAW SOCKET to a worker
app.rs:1920   serve_connection(stream, ..) worker does the BLOCKING READ
```

**The read is the part an untrusted peer controls the timing of, and it runs on
a worker.** §3.1's missing read timeout is a symptom of that, not the disease:
a 30 second timeout on a two-worker pool still surrenders half the node's
capacity for 30 seconds to one silent peer.

### Where it should be

**I/O on the loop, handler on a worker.**

- The loop accepts, reads non-blocking into a buffer, and asks
  `h1::parse_request(&buf)` whether the message is complete.
- Only a **complete, parsed `Request`** is dispatched to a worker.
- The worker runs the handler and may block on disk freely.

What that buys:

| | |
|---|---|
| Slowloris | Immune by construction. No worker is ever held by a slow peer. |
| Handler contract | **Unchanged.** `Fn(&Request) -> Result<Response>`, still free to block. |
| Concurrency models | One I/O model shared with m6-http; execution differs only in that backends have a worker pool behind the loop and m6-http does not need one. |
| Read timeout | Becomes belt-and-braces rather than the defence. |

### Core is already most of the way there

```rust
h1::parse_request(buf: &[u8]) -> ParseResult   // Complete | Incomplete | Error
```

That is an incremental, buffer-driven parser, and **m6-http already drives it
exactly this way**, from an `H1State::Reading { buf }` state machine.
`m6-core/src/parse.rs` exists only to adapt it back into a blocking stream for
`App`'s benefit, and has no framing logic of its own.

So this is not a new parser or a new protocol path. It is **deleting an
adapter** and moving the read to the side of the fence that already has an
event loop.

### Honest risks

- It is a real rewrite of `App`'s loop, which five production services depend
  on. It is the largest item in this document.
- The loop becomes the single thread doing all reads. At current traffic that
  is nothing; it is still a new bottleneck where there was none.
- It must be measured, and the baseline to measure against does not exist yet
  (§5).

### What this changes in the ordering

§3.1's read timeout stays first, because it is a one-line socket option that
closes a live exposure today and does not presuppose any of this. But it should
be understood as a stopgap, and this section as the actual destination.

---

## 3. What core is missing

Five items. Two are config keys, two are capabilities core does not have at
all, and one is a capability core has that nothing uses.

### 3.1 A read timeout on accepted connections `[defect, affects 5 services]`

Neither `App` nor `m6_core::server` sets one.

| service | read timeout |
|---|---|
| `m6-file` | 30s |
| `m6-auth-server` | 30s |
| `m6-html`, `m6-monitor`, `render-*` | **none** |
| `m6-core` `server.rs` / `app.rs` | **none** |

Both services that hand-rolled a main independently decided they needed one.
The shared runtime that five production services depend on does not have it. A
peer that connects and sends nothing parks a worker inside
`parse::parse_request`'s blocking `read()` indefinitely, and these pools are
configured at two workers.

Exposure is limited because backends sit behind m6-http on a unix socket and
m6-http is trusted. It is not zero: `m6-auth-server`'s socket is mode `0666`,
so any local user can open it, and `m6-monitor` is reached over an ssh tunnel.

**This is worth fixing whether or not anything is ever migrated.** It is also a
precondition: migrating m6-file and m6-auth-server onto `App` as it stands
today would **delete the only two read timeouts in the fleet**. One shape has
to mean the shape absorbs what the stragglers knew.

### 3.2 A wildcard route segment `[blocks m6-file]`

```rust
pub enum Segment { Literal(String), Param(String) }

// match_route:
if path_segs.len() != route.segments.len() { return None; }
```

Exact segment-count equality, and a `Param` matches exactly one segment. There
is no catch-all, so **`App` cannot express a static file server**. m6-file
instead uses a root-template model (`root: "assets/"`, remainder joined on) and
serves arbitrary depth like `/assets/gallery/thumb/photo-07.webp`.

This is the whole of m6-file's reason to be a different shape. Its accept loop
is 22 of 33 lines identical to `App`'s; it already calls
`m6_core::server::serve_connection` per connection.

### 3.3 A streaming response body `[capability core lacks]`

`Responder` has exactly three senders, all taking `&[u8]`:

```rust
pub fn send(&mut self, status, headers, body: &[u8])
pub fn error(&mut self, status)
pub fn send_with_length(&mut self, status, headers, body: &[u8], length: usize)
```

`Response.body` is a `Vec<u8>`. **There is no way in m6-core to serve a body
that has not been fully materialised in memory.**

m6-file is not choosing to buffer. `handler.rs:276` is
`std::fs::read(&fs_path)` because there is no alternative. Today the largest
asset on the site is 3.6M and only five files exceed 1M, so nothing is hurting.
But "the box of blocks a service is assembled from" cannot currently assemble a
service that serves a video, and that is a ceiling on the shape rather than a
quirk of one service.

Shape of the fix: `Responder::send_stream(status, headers, reader: impl Read,
length)`, and a `Response` variant carrying a reader instead of a `Vec` so
`App` can express it too.

### 3.4 A length-known, body-absent response `[core has it; zero callers]`

`send_with_length` exists, is documented for exactly one use case, *"a HEAD
answered without reading the file, say"*, and **has no callers anywhere in the
workspace**.

Meanwhile m6-file's HEAD path, traced:

```
line  84  HEAD passes the 405 gate       (only non-GET/HEAD is rejected)
line 276  std::fs::read(&fs_path)        full file into memory
line 288  minify_js / minify_html        full cost
line 308  brotli level 6                 full cost
line 343  resp.send(200, &hdrs, &body)
h1.rs:700 bodyless = method == HEAD      → the bytes are discarded
```

`HEAD /assets/vditor/dist/js/lute/lute.min.js` reads 3.6MB, minifies 3.6MB,
brotli-compresses it at level 6, and throws all of it away to return a header.
Nothing in that path checks the method.

That is an amplification asymmetry: trivial request, expensive server work,
reachable by anyone. It is bounded by the edge cache in front of m6-file, so it
is not urgent, but the fix is free because the capability already exists.

`Response` cannot express it either, so `App` absorbing m6-file requires
`Response` to gain it.

### 3.5 Socket permissions as a config key `[blocks m6-auth-server]`

`App` never calls `set_permissions`. `m6-auth-server` sets its socket to `0666`
by hand. This wants a config key, not a bespoke `main`.

## 4. What transfers, and to which apps

The point of putting these in core rather than in the two stragglers.

| enhancement | m6-file | m6-auth-server | the 5 App services | m6-http |
|---|---|---|---|---|
| 3.1 read timeout | keeps what it has | keeps what it has | **gains a protection they lack** | has its own |
| 3.2 wildcard route | **unblocks it** | no | available for any tree-serving app | no |
| 3.3 streaming body | **removes its memory ceiling** | no | available; needed by any app serving large files | could use it for large proxied bodies |
| 3.4 bodyless response | **removes a full read+minify+compress on HEAD** | yes | **every app's HEAD path** | yes |
| 3.5 socket perms | no | **unblocks it** | available | no |

Two of the five (3.1, 3.4) fix live defects in services that are already the
right shape. That is the argument for doing them first regardless of whether
any migration happens.

## 5. Not regressing performance

The constraint is explicit, so the plan is explicit about it.

**None of these need cost anything on the hot path.**

- **3.1 read timeout** is a socket option set once per accepted connection. No
  per-request cost.
- **3.2 wildcard** touches `match_route`, which is hot. **Keep the existing
  length-equality fast path for routes with no wildcard**, and only take the
  greedy path when a compiled route actually contains one. A route table with
  no wildcards must execute the same instructions it does today.
- **3.3 streaming** is a new variant. The existing `Vec<u8>` path must stay
  byte-identical; nothing that buffers today should acquire a branch it did not
  have.
- **3.4 bodyless** is strictly less work. It is the only item that is a
  performance *improvement*, and a large one on HEAD.
- **3.5 socket perms** is one `set_permissions` call at bind time.

**Measurement is owed anyway.** Phases 5 and 6 already owe benchmark deltas and
have none, and the whole request path moved between crates during them. So the
baseline this work is measured against does not exist yet.

Order of operations: **establish the Phase 5/6 baseline first**, then measure
each of these against it, paired and interleaved, per the standing rule. A
criterion run on a laptop has ±5-15% noise; `check.sh` deliberately does not
gate on it. Use the build host, which is the staging environment and the only
box with a stable comparison.

## 6. Order

Sequenced so each step ships alone and earns the next.

1. **Benchmark Phases 5 and 6.** Owed already. Nothing below can claim "no
   regression" without it.
2. **3.1 read timeout.** Fixes a live gap in five services. No migration
   needed, no design in it.
3. **3.4 bodyless response**, and fix m6-file's HEAD path to use it. A
   performance win, not a cost.
4. **3.2 wildcard route.** Unblocks m6-file.
5. **3.5 socket permissions key.** Unblocks m6-auth-server.
6. **Migrate `m6-file` to `App`.** Delete its hand-rolled main and its copy of
   the accept loop.
7. **Migrate `m6-auth-server` to `App`.** Same.
8. **3.3 streaming body.** The only item above with real design in it: it
   changes what a `Response` is. Nothing above is blocked on it.
9. **§2a: move the read onto the event loop.** The destination. Largest item
   here, and the one that leaves the project with a single I/O model instead of
   two. Deliberately last: every step above is independently useful, ships
   alone, and makes this one smaller. Doing it first would mean rewriting the
   loop of five working services before the baseline to measure it against
   exists.

After 7 the fleet is one shape plus the edge. Step 8 raises the ceiling of that
shape. **Step 9 is what makes it one good path rather than two that happen to
share a parser.**

## 7. What this does not touch

- **m6-http stays as it is**, converging on core module by module. See §2.
- **`m6-md` and `m6-auth-cli` are CLI tools**, not services, and are outside
  the shape entirely.
