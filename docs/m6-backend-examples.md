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
example, because it is the thing people copy. The tests in §6 assert the
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
| `/health` | 200, `text/plain`, `ok` |
| `/boom` | 500, a small error page, to exercise the backend error path |
| anything else | 404, a small not-found page |

Each response carries `Content-Type` with a charset and a correct
`Content-Length`. No templating, no filesystem access, no configuration file
parsing. Someone building on an example should be deleting the routing table
and adding their own, not unpicking a framework.

## 4. The examples

| Example | Demonstrates |
|---|---|
| **C** | The contract with nothing underneath it. Sockets, `chmod`, `accept`, a hand written parser. The floor: if this is comfortable, the contract is genuinely small. |
| **C++** | The same, with the standard library. The comparison with C is the point: almost no difference, because the contract is not the hard part. |
| **Python** | The scripting case, standard library only. Likely the most copied example. |
| **Go** | The important one. `net.Listen("unix", …)` with `http.Serve` puts a **mature HTTP stack** behind the contract instead of a hand written parser, so it tests strictness the toy parsers do not: response framing, header canonicalisation, connection handling. |
| **Rust, without `m6-core`** | Proves that Rust is not privileged. Standard library only, no m6 dependency at all. This is the honest control: if it is much harder than the Go one, the contract has drifted toward Rust. |
| **Rust, with `m6-core`** | The convenience path. The same behaviour with the scaffolding supplied. The interesting number is how much shorter it is than the previous row, because that is the value `m6-core` actually adds. |

The last two are a matched pair on purpose. Together they answer "what does
`m6-core` buy" with a diff rather than an assertion.

## 5. Where they live

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

## 6. How they are tested

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
- SIGTERM shuts the backend down cleanly, the socket is removed, and `m6-http`
  reports a backend error rather than hanging.

Because the assertions are shared, adding a language is adding a directory.

## 7. Missing runtimes

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

## 8. Deliberately out of scope

- **Frameworks.** No Flask, no Gin, no Actix. Their conventions would obscure
  the contract, and someone using one can map from the standard library
  version.
- **Databases, templating, sessions.** Application concerns, not contract
  concerns. `m6-render` is where that story lives for Rust.
- **Performance.** These are correctness references. Nothing here should be
  read as a benchmark, and `m6-file` remains the example of a fast backend.
- **Node, Ruby, PHP, Java.** Nothing against them. The set above already spans
  systems, scripting and a mature HTTP stack, and each addition is a runtime
  the build host must carry. Add one when there is a reason.
