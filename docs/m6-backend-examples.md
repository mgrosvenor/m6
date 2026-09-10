# m6 backend examples — Design

**Status: design. The examples described here do not exist yet.**

---

## 1. Why

`m6-overview.md` promises that a renderer can be "any HTTP/1.1+ server, written
in any language". Today that promise is written down and never exercised. Every
backend in the tree is Rust, and most link `m6-render`, so nothing would fail
if the multi-language contract quietly stopped being true.

`m6-backend-protocol.md` now states the contract normatively. These examples
make it **executable**. They are reference
implementations of the backend wire contract in several languages, they live in
the test suite, and they run in the gate. If a change to `m6-http` breaks a
plain C backend, a test goes red rather than a document going stale.

They serve three audiences at once:

- **Someone writing a backend.** A complete, working, minimal starting point in
  their language, and a worked reading of the specification.
- **The platform.** A regression guard on the contract itself.
- **This documentation.** Executable examples cannot rot.

## 2. The contract lives elsewhere

**`m6-backend-protocol.md` is normative. This document is not.**

Every example is written *from* that specification, and nothing about the
contract is restated here. If an example and the specification disagree, the
specification is right and the example is a bug.

That ordering matters. An example that drifts from the spec is worse than no
example, because it is the thing people copy. The tests in §7 assert the
behaviour the specification requires, so an example that stops conforming fails
the gate rather than quietly teaching the wrong shape.

The specification's §9 is a checklist of everything a backend MUST do. Each
example is a direct realisation of that checklist and SHOULD be readable
side by side with it.

## 3. What each example does

Deliberately trivial, so the contract is the only thing on display. Identical
behaviour in every language:

| Path | Response |
|---|---|
| `/` | 200, a small static HTML page naming the language |
| `/status` | 200, `application/json`, **the benchmark payload (§5.1)** |
| `/health` | 200, `text/plain`, `ok` |
| `/boom` | 500, a small error page, exercising the backend error path |
| anything else | 404, a small not-found page |

Each response carries `Content-Type` with a charset and a correct
`Content-Length`. No templating, no filesystem access, no configuration file
parsing. Someone building on an example should be deleting the routing table
and adding their own, not unpicking a framework.

## 4. What each language is for

The examples are not six ways to do the same thing. Each is a starting point
for a **different kind of backend**, and the example should say so in its own
README so the reader can tell whether they are in the right place.

This section is guidance about fit, not a ranking. Every one of them satisfies
the contract completely.

### C — constrained and close to the metal

For backends where the runtime itself is the problem: no allocator you did not
choose, no garbage collector, no interpreter, a static binary measured in tens
of kilobytes.

The case that motivates it is **IoT with `m6-http` as the front door**. A
sensor or controller on an ESP32 or a small Linux board serves a handful of
endpoints; `m6-http` sits in front holding TLS, HTTP/2, caching and rate
limiting, none of which the device could reasonably implement. The device
speaks the simplest possible HTTP/1.1 over a socket and nothing else. The
contract exists in the shape it does partly so this is possible.

Also the right choice for **direct kernel interface work**: `io_uring`, raw
sockets, `netlink`, device files, anything where a language runtime sits
between you and the syscall you actually want.

### C++ — stateful, in-memory, performance-critical

For backends that hold **substantial state in memory** and answer from it. A
Redis-shaped service: an in-memory index, a cache, a graph, a time series
buffer, queried over HTTP.

The reason is the standard library plus RAII: real containers, deterministic
destruction, and no GC pause between a request arriving and being answered.
When the work is "look it up in a large structure and serialise the answer",
this is a natural fit and the language is not fighting you.

### Go — concurrent I/O and operational simplicity

For backends whose work is mostly **waiting on other things**: fanning out to
several upstream APIs, calling cloud services, handling webhooks, aggregating
results from multiple databases.

Goroutines make a thousand concurrent outbound calls unremarkable, and the
standard library ships a good HTTP client and JSON codec, so a backend that is
mostly integration is mostly stdlib. Operationally it is one static binary with
no runtime to install, which matters when the backend is deployed somewhere
`m6-http` is not.

It is also the example that **tests the contract hardest**, because
`net.Listen("unix", …)` with `http.Serve` puts a mature, strict HTTP
implementation behind it rather than a parser written for the occasion.

### Python — native compute behind a thin dispatch layer

The usual framing of "Python is slow but has libraries" is wrong for this use
case, and worth stating properly.

For numerical and scientific backends, **the work does not happen in Python.**
`numpy` dispatches into BLAS and LAPACK, `scipy` into Fortran kernels, `torch`
and `onnxruntime` into optimised native code or a GPU. Python is the
orchestration layer; the array operation runs at native speed with SIMD, often
faster than a straightforward hand-written C loop doing the same thing, because
those kernels have had decades of attention.

That changes what the Python overhead actually is. It is **per request**, not
per element: parse the request, dispatch into native code, serialise the
result. A backend computing something over a large array pays Python cost once
and native cost for the real work.

So the fit is: **the request is small, the computation is large, and the
computation has a native implementation someone else has already optimised.**
Model inference, a statistical summary over a dataset, an image transform, a
similarity search over embeddings. `m6-http` in front handles TLS, HTTP/2 and
caching; the backend does one `numpy` call and returns.

The same argument covers the wider ecosystem: if the good client library for
the thing you need exists only in Python, that is a real engineering reason and
not a compromise.

Where it is genuinely a poor fit is the opposite shape: high request rates
doing trivial per-request work, where the interpreter overhead is the whole
cost and there is nothing native to dispatch into. §5 will show that clearly,
and the `/status` benchmark is deliberately that shape, so read Python's number
as the floor of what dispatch costs rather than as a verdict on the language.

### Rust without `m6-core` — predictable latency, hostile input

For backends that must be **both fast and safe**, with no GC pause between
arrival and answer, and where the input is untrusted or the parsing is
intricate.

It is also the honest control for the project. If this example is much harder
than the Go one, the contract has drifted toward Rust and something is wrong
with the platform, not with the example.

### Rust with `m6-core` — the default inside the ecosystem

The same, with socket lifecycle, thread pool, signals, config, logging and
content handling supplied rather than written.

For a new backend by someone already in the m6 codebase, this is the starting
point. Its value is measured by its diff against the previous one, not
asserted.

## 5. Benchmarking

The examples share a route so they can be compared: **`/status` returns a
byte-identical JSON payload in every language.**

### 5.1 The payload

Static, identical bytes, checked into the repository once and used verbatim by
every example. Nothing generated, nothing varying: identical `Content-Length`,
identical compressibility, identical work to serialise, so the only variable is
the implementation.

A few hundred bytes of realistic API-shaped JSON. Small enough that the
measurement is dominated by accept, parse and write rather than by copying a
payload, which is the part the contract governs.

Dynamic status fields (uptime, counters) are deliberately **excluded**. They
would vary per response, defeat byte-identity, and measure clock and formatting
code instead of the thing being compared.

### 5.2 What is measured

Two layers, because they answer different questions:

| Layer | Question |
|---|---|
| **Direct to the Unix socket** | How expensive is this implementation of the contract? |
| **Through `m6-http`** | What does a client actually experience? |

Reported for each: requests per second, latency p50/p99/max, resident memory
under load, binary or image size, and cold start to first successful response.

Cold start matters more than it looks: it is what a `systemd` restart costs
during a deploy, and it is the number that decides whether a backend can be
scaled by adding instances.

### 5.3 What this does and does not tell you

**It measures an implementation, not a language.** Every example is the
straightforward standard-library version, written for clarity. A tuned Python
service with a different I/O model would land somewhere else entirely. Reading
these numbers as a language ranking would be wrong.

**Behind the cache, backend throughput is usually not the constraint.** In the
reference deployment the edge cache hit rate is around 0.87 to 0.90 and a cache
hit is served in single-digit microseconds, so most requests never reach a
backend at all. Backend throughput matters for cache misses, uncacheable routes
and cold starts. It is a real number, and it is not the number that decides how
fast the site feels.

**The comparison that is genuinely informative** is Rust-without-`m6-core`
against Rust-with-`m6-core`. Same language, same compiler, same payload, so the
delta is exactly the overhead the library adds. If that number is not close to
zero, `m6-core` has a problem worth knowing about.

### 5.4 Honesty requirements

Per the traps this project has already hit:

- Numbers are **re-measured, never quoted from this document.** Anything
  written down is a claim about the past.
- Published figures carry commit, hardware, payload size, concurrency and the
  exact command line, per `BENCHMARKS.md`.
- Client-side timing across a network measures the network. Benchmarks run on
  the same host as the backend.
- The load generator's own cost is measured and stated, not assumed to be zero.

## 6. Where they live

```
m6-http/tests/backends/
├── c/
├── cpp/
├── python/
├── go/
├── rust-plain/
└── rust-m6core/
```

Under `tests/` rather than `examples/` so they are part of the gate rather than
decoration. `docs/` links to them; the code is never duplicated into prose.

## 7. How they are tested

One shared assertion set, parameterised by language. Every example is stood up
behind a real `m6-http` and must satisfy the same checks:

- A request through `m6-http` returns the backend's body unchanged.
- `X-Forwarded-For` carries the real client address, not the socket peer.
- `X-Forwarded-Host` and `Via` arrive intact.
- The backend's `Content-Length` framing survives the hop.
- `m6-http` applies compression and caching **on top of** an uncompressed,
  uncached backend response, proving the backend need not participate.
- `/boom` produces a 500 that `m6-http` reports as a backend error and can
  replace with a styled error page.
- An unknown path produces the backend's 404, not `m6-http`'s.
- `/status` returns **byte-identical** output in every language. This is
  asserted, not assumed: a benchmark comparing payloads that have quietly
  diverged compares nothing.
- SIGTERM shuts the backend down cleanly, the socket is removed, and `m6-http`
  reports a backend error rather than hanging.

Because the assertions are shared, adding a language is adding a directory.

## 8. Missing runtimes

A missing toolchain must not silently pass. That is the "regression test that
has never failed" trap, and this suite is unusually exposed to it: most
developers will not have every runtime installed.

- **On a development machine**, a missing runtime skips that language with a
  visible warning.
- **On the build host**, `run-tests.sh` asserts every runtime is present and
  fails if one is not.

Same shape as the existing zero-warnings rule, which is enforced on Linux on
the build host rather than on whatever the developer happens to be running.

The build host currently has Python, C and C++. **Go must be installed**, and
it is the highest value addition of the set for the reason in §4.

## 9. Deliberately out of scope

- **Frameworks.** No Flask, no Gin, no Actix. Their conventions would obscure
  the contract, and someone using one can map from the standard library
  version.
- **The libraries that motivate a choice.** The Python example does not import
  `numpy`, even though §4 argues that is the reason to pick Python. The example
  demonstrates the *contract*; the reader supplies the payload. Adding a
  numerical dependency would make the example about `numpy` and would put a
  wheel build in the gate.
- **Databases, templating, sessions.** Application concerns, not contract
  concerns. `m6-render` is where that story lives for Rust.
- **Performance.** These are correctness references. Nothing here should be
  read as a benchmark, and `m6-file` remains the example of a fast backend.
- **Node, Ruby, PHP, Java.** Nothing against them. The set above already spans
  systems, scripting and a mature HTTP stack, and each addition is a runtime
  the build host must carry. Add one when there is a reason.
