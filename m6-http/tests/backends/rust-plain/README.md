# Rust without m6-core: predictable latency, hostile input

An m6 backend example. `docs/m6-backend-examples.md` §4 is where the set is
explained; this file says whether you are in the right place.

Pick this shape for backends that must be **both fast and safe**, with no GC
pause between arrival and answer, and where the input is untrusted or the parsing
intricate.

It has **no dependencies at all**, and that is deliberate. This is the control in
the measurement that says what `m6-core` costs, so anything linked here that its
pair does not link would show up in the delta and be read as core's overhead.
Even the three libc calls it needs are declared in a small `extern` block rather
than pulled in.

It is also the honest control for the platform. If writing this were much harder
than `../go/main.go`, the contract would have drifted toward Rust and the problem
would be the platform, not the example. It is about the same length.

**It uses protocol §7's reference model**: a fixed thread pool with a bounded
queue, 503 when the queue is full. The first version spawned a thread per
connection, which the protocol permits, and it made the comparison with its pair
meaningless: the measured difference was pooling against thread-per-connection
rather than library against no library. Fixing it took this example from 17,608
to 46,826 rps and inverted the sign of the answer. See `docs/BENCHMARKS.md`.

Built size: about 555KB. On the build host: 28,954 rps, p50 63.7us.

## Run it

```sh
cargo build --release -p m6-example-rust-plain
../../../../target/release/m6-example-rust-plain /tmp/ex.sock ../status.json
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
