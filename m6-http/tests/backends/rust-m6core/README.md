# Rust on m6-core: the default inside the ecosystem

An m6 backend example. `docs/m6-backend-examples.md` §4 is where the set is
explained; this file says whether you are in the right place.

The same as `../rust-plain`, with the socket lifecycle, thread pool, signal
handling, config, logging and content handling **supplied rather than written**.
For a new backend by someone already in the m6 codebase, this is the starting
point.

**Note the invocation.** It is `<site-dir> <config-path>`, not the
`<socket-path> <status-json-path>` of the other five: a core service is
configured rather than argument-driven, and the socket comes from config or
`M6_SOCKET_OVERRIDE`. That difference is part of what the example shows.

**Read it beside `../rust-plain/src/main.rs`.** What that file writes by hand and
this one does not write at all: the binding sequence including the `chmod` that
is the single most common cause of a backend that starts and is never contacted,
signal handling and the unlink on the way out, the accept loop and its pool,
parsing a request line and header section, draining exactly `Content-Length`
bytes, writing a status line and an accurate `Content-Length`, and suppressing
the body on HEAD. What is left is four routes and their bodies.

**Its value is measured, not asserted**, and the measurement is not flattering.
Against its pair on the build host: **-36.8% throughput, +38.5us p50, 8.8x
resident memory, 56.7x binary size.** `docs/m6-backend-examples.md` §5.3 says
that if the delta is not close to zero then core has a problem worth knowing
about, and it is not close to zero. It is also measured on the shape that
maximises it, a route whose own work is copying 660 bytes, and behind the edge
cache most requests never reach a backend at all. Both halves of that are true.
`docs/BENCHMARKS.md` has the conditions.

Three routes are `.verbatim()`: without it core minifies the HTML, which is core
doing its job and wrong for a set of examples meant to be read side by side. See
§10.3.

## Run it

```sh
cargo build --release -p m6-example-rust-m6core
M6_SOCKET_OVERRIDE=/tmp/ex.sock \
  ../../../../target/release/m6-example-rust-m6core <site-dir> <config.toml>
```

Then, from another shell:

```sh
curl --unix-socket /tmp/ex.sock http://x/status
```

`/`, `/status`, `/health`, `/boom` and anything else (404) are the five routes.
The `/status` payload is read from `../status.json`, the one copy every example
serves, so all six return byte-identical bytes.

## The contract

`docs/m6-backend-protocol.md` is normative and this example was written from it;
§9 there is the checklist. If the two disagree, the specification is right and
this is a bug. `docs/m6-backend-examples.md` §10 lists the places where the
documents and the code are known to disagree.

Conformance is asserted by `m6-http/tests/backends_contract.rs` and
`m6-http/tests/backends_through_proxy.rs`, which run this example in the gate.
