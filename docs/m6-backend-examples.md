# m6 backend examples

The six examples under `m6-http/tests/backends/` are reference implementations of the backend wire contract in `m6-backend-protocol.md`, one in C, C++, Go, Python and two forms of Rust.
They exist so the multi-language promise of the platform is exercised in the test gate.
This document explains what each example is for, how they are benchmarked and tested, and where they disagree with the contract.

Status: implemented, 2026-09-13.
All six conform, and the shared assertion set of section 7 runs them in the gate.
Section 11 lists what is still owed, and section 10 records the disagreements explicitly.

## Contents

1. [Why](#1-why)
2. [The contract lives elsewhere](#2-the-contract-lives-elsewhere)
3. [What each example does](#3-what-each-example-does)
4. [What each language is for](#4-what-each-language-is-for)
5. [Benchmarking](#5-benchmarking)
6. [Where they live](#6-where-they-live)
7. [How they are tested](#7-how-they-are-tested)
8. [Missing runtimes](#8-missing-runtimes)
9. [Deliberately out of scope](#9-deliberately-out-of-scope)
10. [Where this document and the code disagree](#10-where-this-document-and-the-code-disagree)
11. [Still owed against this document](#11-still-owed-against-this-document)

## 1. Why

`m6-architecture.md` promises that a renderer can be "any HTTP/1.1+ server, written in any language".
Before these examples, every backend in the tree was Rust, so the promise was written down and never exercised.
Nothing would fail if the multi-language contract quietly stopped being true.

`m6-backend-protocol.md` states the contract normatively.
These examples make it executable.
Each is a reference implementation of the contract in a different language, and they run in the gate.
A change to `m6-http` that breaks a plain C backend turns a test red, and executable examples cannot rot.

They serve three audiences at once:

- **Someone writing a backend** gets a complete, working, minimal starting point in their language, and a worked reading of the specification.
- **The platform** gets a regression guard on the contract.
- **This documentation** gets examples that are executed, so they cannot go stale.

## 2. The contract lives elsewhere

`m6-backend-protocol.md` is the normative specification, and this document describes the examples.
Every example is written from that specification.
When an example and the specification disagree, the specification is right and the example is a bug.

That ordering matters.
An example that drifts from the specification is worse than no example, because it is the thing people copy.
The tests in section 7 assert the behaviour the specification requires, so an example that stops conforming fails the gate.

The specification's section 9 is a checklist of everything a backend MUST do.
Each example is a direct realisation of that checklist, readable side by side with it.

## 3. What each example does

Every example serves the five routes in Table 1, deliberately trivial so the contract is the only thing on display.
Take from it that the only per-language difference is the name of the language in the pages.

| Path | Response |
|---|---|
| `/` | 200, a small static HTML page naming the language |
| `/status` | 200, `application/json`, the benchmark payload of section 5.1 |
| `/health` | 200, `text/plain`, `ok` |
| `/boom` | 500, a small error page, exercising the backend error path |
| anything else | 404, a small not-found page |

**Table 1: the five routes every example serves, and the response to each.**

Each response carries `Content-Type` with a charset and a correct `Content-Length`.
There is no templating, no filesystem access and no configuration file parsing.
Someone building on an example deletes the routing table and adds their own.

## 4. What each language is for

Each example is a starting point for a different kind of backend, and each README says so, so a reader can tell whether they are in the right place.
This section is guidance about fit, and every one satisfies the contract completely.

### 4.1. C: constrained and close to the metal

Pick C when the runtime itself is the problem.
You get no allocator you did not choose, no garbage collector and no interpreter, and a static binary measured in tens of kilobytes.

The case that motivates it is *IoT* (Internet of Things) with `m6-http` as the front door.
A sensor or controller on an ESP32 or a small Linux board serves a handful of endpoints.
`m6-http` sits in front holding *TLS* (Transport Layer Security), HTTP/2, caching and rate limiting, none of which the device could reasonably implement.
The device speaks the simplest possible HTTP/1.1 over a socket and nothing else.
The contract has the shape it does partly so this is possible.

C is also the right choice for direct kernel interface work: `io_uring`, raw sockets, `netlink`, device files, anything where a language runtime sits between you and the syscall you want.

### 4.2. C++: stateful, in-memory, performance-critical

Pick C++ for backends that hold substantial state in memory and answer from it.
A Redis-shaped service: an in-memory index, a cache, a graph, a time series buffer, queried over HTTP.

The reason is the standard library plus *RAII* (Resource Acquisition Is Initialisation): real containers, deterministic destruction, and no *GC* (garbage collection) pause between a request arriving and being answered.
When the work is "look it up in a large structure and serialise the answer", the language is not fighting you.

### 4.3. Go: concurrent I/O and operational simplicity

Pick Go for backends whose work is waiting on other things: fanning out to upstream APIs, calling cloud services, handling webhooks, aggregating results from databases.

Goroutines make a thousand concurrent outbound calls unremarkable, and the standard library ships a good HTTP client and JSON codec.
An integration backend is the standard library plus the calls it fans out.
Operationally it is one static binary with no runtime to install, which matters when the backend is deployed somewhere `m6-http` is not.

Go is also the example that tests the contract hardest, because `net.Listen("unix", ...)` with `http.Serve` puts a mature, strict HTTP implementation behind the socket.
If `m6-http` ever emits something subtly wrong, this is where it shows first.

### 4.4. Python: native compute behind a thin dispatch layer

The usual framing of "Python is slow but has libraries" is wrong for this use case.

For numerical and scientific backends, the work does not happen in Python.
`numpy` dispatches into *BLAS* (Basic Linear Algebra Subprograms) and *LAPACK* (Linear Algebra Package), `scipy` into Fortran kernels, `torch` and `onnxruntime` into optimised native code or a *GPU* (graphics processing unit).
Python is the orchestration layer.
The array operation runs at native speed with *SIMD* (single instruction, multiple data), faster than a straightforward hand-written C loop doing the same thing, because those kernels have had decades of attention.

That changes what the Python overhead is.
The overhead is per request: parse the request, dispatch into native code, serialise the result.
A backend computing over a large array pays Python cost once and native cost for the real work.

So the fit is: the request is small, the computation is large, and the computation has a native implementation someone else has already optimised.
Model inference, a statistical summary over a dataset, an image transform, a similarity search over embeddings.
`m6-http` in front handles TLS, HTTP/2 and caching, and the backend does one `numpy` call and returns.

The same argument covers the wider ecosystem: if the good client library for the thing you need exists only in Python, that is a real engineering reason.

Where Python is a poor fit is the opposite shape: high request rates doing trivial per-request work, where the interpreter overhead is the whole cost and there is nothing native to dispatch into.
Section 5 shows that clearly, and the `/status` benchmark is deliberately that shape, so read Python's number as the floor of what dispatch costs.

### 4.5. Rust without `m6-core`: predictable latency, hostile input

Pick this shape for backends that must be both fast and safe, with no GC pause between arrival and answer, and where the input is untrusted or the parsing is intricate.

It has no dependencies at all, and that is deliberate.
It is the control in the measurement that says what `m6-core` costs, so anything linked here that its pair does not link would show up in the delta and be read as core's overhead.

It is also the honest control for the platform.
If writing this were much harder than the Go one, the contract would have drifted toward Rust and the problem would be the platform.

### 4.6. Rust with `m6-core`: the default inside the ecosystem

This is the same example with socket lifecycle, thread pool, signals, config, logging and content handling supplied.
For a new backend by someone already in the m6 codebase, it is the starting point.
Its value is measured by its diff against the previous one.

Pick the language whose section describes the work, then read that example's README, which says how to build and run it.

## 5. Benchmarking

The examples share a `/status` route so they can be compared, and that route returns a byte-identical JSON payload in every language.
Section 5.1 gives the payload, and section 5.2 gives the two measurement layers.

### 5.1. The payload

The payload is static, identical bytes, checked into the repository once and used verbatim by every example.
Nothing is generated and nothing varies, so `Content-Length`, compressibility and the work to serialise are identical, and the only variable is the implementation.

It is 660 bytes of realistic API-shaped JSON, small enough that the measurement is dominated by accept, parse and write.
Copying a payload is the part the contract governs, and 660 bytes is too small for that to dominate.

Dynamic status fields, such as uptime and counters, are deliberately excluded.
They would vary per response, defeat byte-identity, and measure clock and formatting code.

### 5.2. What is measured

Two layers are measured, because they answer different questions.
Table 2 gives them.
Take from it that the direct layer is a property of the backend, and the second is what a client experiences.

| Layer | Question |
|---|---|
| Direct to the Unix socket | How expensive is this implementation of the contract? |
| Through `m6-http` | What does a client actually experience? |

**Table 2: the two benchmark layers, and the question each answers.**

Each layer reports requests per second, latency p50, p99 and max, resident memory under load, binary or image size, and cold start to first successful response.
Cold start matters more than it looks.
It is what a `systemd` restart costs during a deploy, and it is the number that decides whether a backend can be scaled by adding instances.

### 5.3. What this does and does not tell you

The numbers measure an implementation.
Every example is the straightforward standard-library version, written for clarity.
A tuned Python service with a different I/O model would land somewhere else, so reading these numbers as a language ranking would be wrong.

Behind the cache, backend throughput is not the number that decides how fast a site feels.
A cache hit is served without reaching a backend, so only cache misses, uncacheable routes and cold starts depend on backend throughput.
Backend throughput is a real number, and it is not the whole story.

The comparison that is informative is Rust without `m6-core` against Rust with `m6-core`.
Same language, same compiler, same payload, so the delta is exactly the overhead the library adds.
If that number is not close to zero, `m6-core` has a problem worth knowing about.

### 5.4. Run from RAM, or the measurement is about the disk

The binaries, the payload and the sockets all live on tmpfs for a benchmark run.
Otherwise the numbers are partly a story about the filesystem, and which part varies with what else the box has touched recently.

It matters for two figures measured at startup:

- **Cold start to first successful response** reads the binary off disk, and on a cold page cache it measures the disk.
  The gap is largest for the largest binaries, so it would land as a size penalty on `rust-m6core` that has nothing to do with `m6-core`.
- **Reading the payload at startup**, which every example does, for the same reason.

On Linux, `/dev/shm` is tmpfs and is what the benchmark uses.
`/run` is also tmpfs and is where systemd-managed sockets live, so a socket there is already in RAM.
macOS has no tmpfs by default, so a run on a laptop is labelled NOT-RAM and is not comparable to a build-host run.

### 5.5. Honesty requirements

Per the traps this project has already hit:

- Numbers are re-measured, and never quoted from this document, because anything written down is a claim about the past.
- Published figures carry commit, hardware, payload size, concurrency and the exact command line, per `BENCHMARKS.md`.
- Client-side timing across a network measures the network, so benchmarks run on the same host as the backend.
- The load generator's own cost is measured and stated.

These rules keep a benchmark number from looking authoritative without being reproducible, and section 11 records the one that ran.

## 6. Where they live

The six examples live under `m6-http/tests/backends/`, one directory per language.
Figure 1 gives the layout.
Take from it that each language is one directory.

```
m6-http/tests/backends/
├── c/
├── cpp/
├── python/
├── go/
├── rust-plain/
└── rust-m6core/
```

**Figure 1: the six backend example directories under `m6-http/tests/backends/`.**

They sit under `tests/` so they are part of the gate, and the documentation links to them.
The code is never duplicated into prose.

## 7. How they are tested

One shared assertion set, parameterised by language, runs every example.
`backends_contract.rs` asserts what a socket alone shows, and `backends_through_proxy.rs` stands each example up behind a real `m6-http` and drives it over TLS.

On the socket, every example must:

- answer the five routes of section 3 as specified
- send an accurate `Content-Length` on every response
- return byte-identical `/status` output in every language, asserted, so a benchmark comparing payloads that have quietly diverged compares nothing
- send the `Content-Length` but no body for a HEAD
- drain a request body
- shut down cleanly on SIGTERM, by draining, removing the socket and exiting 0

Through the proxy, every example must:

- return its body unchanged
- keep its `Content-Length` framing intact across the hop
- return its own 404 status for an unknown path
- relay `/boom` as a 500 the proxy counts as a backend error
- arrive uncompressed, because the proxy has no compressor and the examples declare `compresses = false`
- get no `Vary: Accept-Encoding`, because a non-compressing backend has one representation
- serve two identical requests with the same status and body when it sends no `Cache-Control`, because an uncacheable backend must not break the edge

`X-Forwarded-For` is asserted in `security_regressions.rs`, which checks the forwarded request itself.
`X-Forwarded-Host` and `Via` are unit-tested in `m6-http/src/forward.rs`.

Because the assertions are shared, adding a language is adding a directory.

## 8. Missing runtimes

A missing toolchain must not silently pass.
That is the "regression test that has never failed" trap, and this suite is exposed to it, because the six examples need five toolchains: C, C++, Go, Python and Rust.

- **On a development machine**, a missing runtime skips that language with a visible warning.
- **On the build host**, `tools/build-host-tests.sh` sets `M6_BACKENDS_REQUIRE_ALL=1`, and the run fails if any of the four external runtimes is absent.

The four external runtimes are `cc`, `c++`, `go` and `python3`.
The two Rust examples build with Cargo and need no separate toolchain.

## 9. Deliberately out of scope

The examples deliberately leave out the following, so the contract stays the only thing on display:

- **Frameworks.**
  No Flask, no Gin, no Actix.
  Their conventions would obscure the contract, and someone using one can map from the standard-library version.
- **The libraries that motivate a choice.**
  The Python example does not import `numpy`, even though section 4 argues that is the reason to pick Python.
  The example demonstrates the contract, and the reader supplies the payload.
  Adding a numerical dependency would make the example about `numpy` and put a wheel build in the gate.
- **Databases, templating, sessions.**
  These are application concerns, and `m6-render` is where that story lives for Rust.
- **Performance.**
  These are correctness references, and `m6-file` remains the example of a fast backend.
- **Node, Ruby, PHP, Java.**
  The set already spans systems, scripting and a mature HTTP stack, and each addition is a runtime the build host must carry.
  Add one when there is a reason.

## 10. Where this document and the code disagree

Writing the examples found contradictions between normative documents.
They are recorded here, and resolved by the owner's decision where one has been made.

### 10.1. Socket mode

The `0666` socket and `0755` directory in the examples are a test convention.
Each example runs as its own user, and `0666` lets the proxy's user connect without a shared group.
Under that pair any local user can connect, so a deployment does not copy it.
Protocol section 1.2 step 3 sets the rule: `0660` with a trusted group, or `0666` only behind a directory that admits the same trusted users.

The shared test writes `socket_mode = "0666"` into the `rust-m6core` example's config to match the others, so one value can be asserted.
Core keeps its `0660` default.

### 10.2. A bare 404 has no body

Core answers an unmatched path with a 404 and no body.
That satisfies the protocol, which only asks for an honest status, but it does not match section 3, where every example serves a small not-found page.
The `rust-m6core` example registers a catch-all `/{*any}` route last, so its 404 matches the other five.

This is worth knowing before reading the examples side by side and wondering why one needed an extra route.

### 10.3. Core compresses, and the examples must not

Core's pipeline minifies and compresses what a handler returns.
That is correct for a service returning a document, and wrong for these examples.

Protocol section 3.6 makes compression the backend's job, and a backend MAY compress.
These examples choose not to, and declare `compresses = false`.
`rust-m6core` takes core with `default-features = false`, so built on its own (`cargo build -p m6-example-rust-m6core`) it links no minifier.
The test suite builds it with `--workspace`, where cargo unifies features and `m6-html` turns `minify` on, and then a minified body would make its `/boom` 167 bytes where the other five send 178.

Both HTML routes and `/status` in that example are therefore `.verbatim()`, which stops compression, and minification in a build that has the `minify` feature.
`/status` would need it regardless, because identical bytes across six languages is the whole point of that route, and anything that re-encodes could re-space them.

### 10.4. A backend's 404 body never reaches the client

The status of an unknown path's 404 is the backend's, and the body is the edge's.

The status is genuinely the backend's: the proxy routes the request, forwards it and relays what comes back.
`/boom` proves the relaying, because a 500 is a status the proxy would not invent for a route that resolved.

The body belongs to the edge, under `[errors] mode`.
Table 3 gives the modes.
Take from it that no mode relays the backend's own error page.

| mode | what the client gets for a backend 404 |
|---|---|
| `internal`, the default | m6-http's own error page |
| `status` | the status and an empty body |
| `custom` | the document named in config |

**Table 3: the `[errors]` modes, and what each sends for a backend 404.**

So an example's carefully written 404 page is never seen through a proxy, only on its socket.
Section 7 records the status, and whether the proxy should relay the body is the owner's call.

The value `passthrough` is not a valid mode, and `m6-http/src/error.rs` treats any unknown mode as `internal`.

### 10.5. The proxy does not compress: resolved 2026-09-13

This is the largest of the disagreements, and the one with a consequence for anyone writing a backend.

Two texts promised the proxy compresses:

- Protocol section 3.6 said a backend SHOULD NOT compress, because the proxy performs content negotiation and compression itself.
- Section 7 of this document said m6-http applies compression and caching on top of an uncompressed, uncached backend response.

m6-http has no compressor.
`brotli` and `flate2` appear only in `m6-core/Cargo.toml`, `m6-core/src/compress.rs` is the only implementation, and nothing under `m6-http/src` calls it.
What the proxy does is cache and select per-encoding variants of whatever a backend produced, keyed on `Content-Encoding`: negotiation over what already exists.

The consequence is that a backend that declines to compress has its bytes delivered uncompressed, always.
For the Rust services this is invisible, because `m6-core` compresses on the backend side.
For a C, Go or Python backend written from the old specification, those bytes are delivered uncompressed.

Resolved, owner's decision 2026-09-13: m6-http is explicitly a cache.
The documents were wrong, and the code was not.
Compression is the backend's job, `m6-core` is how a Rust backend gets it, and the proxy negotiates between and caches the representations a backend produced.

What changed:

- Protocol section 3.6 was rewritten to say so, with section 3.6.1 for the config key.
- `[[backend]] compresses = <bool>` in `site.toml`, read by both sides, default `true` because an `m6-core` backend compresses unless told not to.
  The edge uses it to decide whether to add `Vary: Accept-Encoding`.
  A backend with one representation would otherwise promise variants that cannot exist and fragment every shared cache downstream for nothing.
- The backend refuses to start if it disagrees with the edge.
  `m6_core::compress::check_declared_support` runs before binding and exits 2, which is what protocol section 8.3 reserves for a configuration error.
  The `declared false while compressing` direction is the dangerous one: the edge stops varying while the backend still returns brotli, so a compressed body can be cached and replayed to a client that asked for identity.
- All six examples declare `compresses = false`, which is the truth about them, and `backends_through_proxy.rs` asserts the edge then sends no `Vary: Accept-Encoding`.
  That test was checked by flipping the key to `true` and confirming it fails.

The gap is now explicit and visible.
A non-Rust backend still serves uncompressed bytes if it chooses not to compress, but it chooses that knowingly, the config records it, and nothing promises otherwise.

## 11. Still owed against this document

Three things are done:

- **The direct-to-socket benchmark of section 5**, done 2026-09-13 in `tools/backend-bench.py`, with results in `docs/BENCHMARKS.md`.
  The delta between the two Rust examples is not close to zero, which section 5.3 says is a problem worth knowing about.
  The route is deliberately the shape that maximises framework overhead, and a cache hit never reaches a backend, so the number measures that shape.
- **The through-the-proxy half of section 7**, done 2026-09-13 in `m6-http/tests/backends_through_proxy.rs`: seven tests, each example behind a real m6-http over TLS.
- **A README per example**, done 2026-09-13.
  Each directory carries one, saying what that language is for, how to build and run it, and what its measured numbers are.
  The module-level comment in each source file carries the same argument where someone is reading the code.

Two things remain:

- The through-m6-http benchmark, the second layer of section 5.2, because `tools/backend-bench.py` measures the direct layer only.
- The decision in section 10.4, which is whether the proxy should relay a backend's error body.

Sections 10.1 and 10.5 are settled: the protocol states the socket requirement, and the proxy is a cache.
