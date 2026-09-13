# m6 backend examples — Design

**Status: implemented, 2026-09-13.** All six exist under
`m6-http/tests/backends/`, all six conform, and the shared assertion set of §7
runs them in the gate. What is still owed against this document is listed in
§11.

Where this document and the implementation disagree, §10 records the
disagreement rather than either side quietly winning.

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

### 5.4 Run from RAM, or the measurement is about the disk

**The binaries, the payload and the sockets all live on tmpfs for a benchmark
run.** Otherwise the numbers are partly a story about the filesystem, and which
part varies with what else the box has touched recently.

It matters most for the two figures this document asks for that are not
steady-state throughput:

- **Cold start to first successful response** reads the binary off disk. That is
  the number which decides whether a backend can be scaled by adding instances,
  and on a cold page cache it measures the disk rather than the runtime. The gap
  is largest for exactly the examples whose binaries are largest, so it would
  land as a size penalty on `rust-m6core` that has nothing to do with
  `m6-core`.
- **Reading the payload at startup**, which every example does, for the same
  reason.

On Linux the build host, `/dev/shm` is tmpfs and is what the benchmark uses.
`/run` is also tmpfs and is where production sockets live, so a socket there is
already in RAM. macOS has no tmpfs by default, so a run on a laptop states that
it is not comparable to a build-host run rather than pretending otherwise.

### 5.5 Honesty requirements

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

**Go is installed**, 2026-09-13: `golang-go` 1.26.0 on the build host, and
1.27.1 on the laptop via Homebrew. The build host now carries all four external
runtimes, and `deploy/run-tests.sh` asserts each one and fails the run if any is
absent, rather than letting a language go untested.

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

---

## 10. Where this document and the code disagree

Writing the examples found contradictions between normative documents. They are
recorded here rather than resolved by whichever file was edited last.

### 10.1 Socket mode: 0666 in the spec, 0660 in core and in production

`m6-backend-protocol.md` §1.2 step 3 says the backend MUST `chmod 0666`, and
explains why: the proxy runs as a different user and cannot connect otherwise.
It calls this the single most common cause of a backend that starts cleanly and
is never contacted.

**`m6-core` defaults to `0660`, and production runs `0660`.** The m6 handover
records the move to 0660 as a deliberate hardening, away from a 0755 that came
from the umask by accident. So the fleet contradicts the MUST and works, because
the proxy is in the socket's group.

The four hand-written examples implement 0666 as the spec states. The
`rust-m6core` example sets `socket_mode = "0666"` in its own config to match
them, so the shared test can assert one value. Nothing here changes core's
default.

**This wants a decision.** Either the spec should say 0660 with a note that the
proxy must share the group, which is what actually runs and is tighter, or core
should default to 0666, which is looser and would undo a deliberate hardening.
The first looks right, and it is the owner's call, not a documentation tidy-up.

### 10.2 A bare 404 has no body

Core answers an unmatched path with a 404 and no body. That satisfies the
protocol, which only asks for an honest status, but not §3 of this document,
where every example serves "a small not-found page". The `rust-m6core` example
registers a catch-all `/{*any}` route last so its 404 matches the other five.

Not a defect in either document, but worth knowing before reading the examples
side by side and wondering why one needed an extra route.

### 10.3 Core minifies, and the examples must not

Core's pipeline minifies and compresses what a handler returns. That is correct
for a service returning a document, and wrong for these examples: protocol §3.6
says a backend SHOULD NOT compress because the proxy negotiates and caches each
representation itself, and a minified body would also make `rust-m6core`'s
`/boom` 167 bytes where the other five send 178.

Both HTML routes and `/status` in that example are therefore `.verbatim()`.
`/status` would need it regardless: identical bytes across six languages is the
whole point of that route, and anything that re-encodes could re-space them.

### 10.4 A backend's 404 body never reaches the client

§7 of this document says an unknown path "produces the backend's 404, not
m6-http's". That is true of the **status** and false of the **body**.

The status is genuinely the backend's: the proxy routes the request, forwards it
and relays what comes back. `/boom` proves the relaying, because a 500 is not a
status the proxy would invent for a route that resolved.

The body belongs to the edge, under `[errors] mode`:

| mode | what the client gets for a backend 404 |
|---|---|
| `internal`, the default | m6-http's own error page |
| `status` | the status and an empty body |
| `custom` | the document named in config |

**No mode relays the backend's own error page.** So an example's carefully
written 404 page is never seen through a proxy, only on its socket. Either §7
should say "status" or the proxy should gain a passthrough mode. Owner's call.

Worth knowing separately: `"passthrough"` is not a valid mode and silently
becomes `internal` (`m6-http/src/error.rs:22`). The first version of the
through-proxy test wrote exactly that and spent a while looking like a proxy
bug.

### 10.5 The proxy does not compress, and two documents say it does

This is the largest of the disagreements and the one with a consequence for
anyone writing a backend.

- protocol §3.6: "The backend SHOULD NOT compress its response... **The proxy
  performs content negotiation and compression itself**, caches each
  representation, and reuses it across clients."
- §7 of this document: "m6-http applies compression and caching **on top of** an
  uncompressed, uncached backend response, proving that the backend need not
  participate."

**m6-http has no compressor.** `brotli` and `flate2` appear only in
`m6-core/Cargo.toml`, the only implementation is `m6-core/src/compress.rs`, and
nothing under `m6-http/src` calls it. What the proxy does is cache and select
per-encoding *variants* of whatever a backend produced, keyed on
content-encoding: negotiation over what already exists, not compression.

Confirmed by measurement, not just by reading: 660 bytes of JSON requested
through the edge with `Accept-Encoding: br, gzip` come back with no
`Content-Encoding` and the identity length, for all six examples.

**The consequence.** A backend that follows §3.6 and declines to compress has
its bytes delivered uncompressed, always. For the Rust services this is
invisible, because `m6-core` compresses on the backend side, which is precisely
what §3.6 tells backends not to do. For a C, Go or Python backend written from
the specification as written, it is not invisible at all: the site simply
serves them uncompressed.

`backends_through_proxy.rs` asserts the current behaviour and names this
section, rather than carrying a permanently failing test. Resolving it is either
teaching the proxy to compress, which is the behaviour both documents already
promise, or correcting both documents to say that compression is the backend's
job and `m6-core` is how a Rust backend gets it. The first matches what a reader
of the protocol expects; the second matches the fleet. Owner's call.

---

---

## 11. Still owed against this document

- ~~The benchmark of §5~~ **done 2026-09-13**, `tools/backend-bench.py`, results
  in `docs/BENCHMARKS.md`. The answer to §5.3's question is that linking
  `m6-core` costs **36% of throughput and +37us p50** on this route, reproduced
  within 3% across two runs, plus 8.8x resident memory and 56.7x binary size.
  §5.3 says that if the number is not close to zero then core has a problem
  worth knowing about, and it is not close to zero. What it is NOT is "the site
  is 36% slower": the route is deliberately the shape that maximises framework
  overhead, and behind the edge cache most requests never reach a backend.

  Getting there required fixing the control rather than the subject. The first
  run reported core as 72% FASTER, because `rust-plain` spawned a thread per
  connection while core answered from a fixed pool, so the comparison was
  pooling against thread-per-connection. `rust-plain` now uses protocol §7's
  reference model, which is what it should always have been, and that alone took
  it from 17,608 to 46,826 rps.
- ~~The through-the-proxy half of §7~~ **done 2026-09-13**,
  `m6-http/tests/backends_through_proxy.rs`: six tests, each example behind a
  real m6-http over TLS. Two of the behaviours §7 promised turned out not to
  happen, and are recorded in §10.4 and §10.5 rather than asserted. The
  `X-Forwarded-For` and `Via` checks are deliberately not duplicated here:
  `security_regressions.rs` already asserts them against the forwarded request
  itself, which is a better layer than inferring them from a backend's reply,
  and re-asserting them would have meant growing a sixth route on all six
  examples that §3 does not have.
- **A README per example**, which §4 asks for so a reader can tell whether they
  are in the right place. The module-level comment in each file carries that
  text today.
