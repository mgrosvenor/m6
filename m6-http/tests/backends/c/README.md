# C: constrained and close to the metal

An m6 backend example. `docs/m6-backend-examples.md` §4 is where the set is
explained; this file says whether you are in the right place.

Pick C when **the runtime itself is the problem**: no allocator you did not
choose, no garbage collector, no interpreter, and a static binary measured in
tens of kilobytes.

The case that motivates it is **IoT with `m6-http` as the front door**. A sensor
or controller on an ESP32 or a small Linux board serves a handful of endpoints
while the proxy holds TLS, HTTP/2, caching and rate limiting, none of which the
device could reasonably implement. The contract is shaped the way it is partly so
this is possible.

Also the right choice for **direct kernel interface work**: `io_uring`, raw
sockets, `netlink`, device files, anything where a language runtime sits between
you and the syscall you actually want.

This is the example that shows how little the contract asks for: one file, the
standard library and POSIX sockets. A thread per connection, which §7 of the
protocol permits.

Built size: about 22KB on Linux, 52KB on macOS.

## Run it

```sh
cc -std=c11 -O2 -pthread -o /tmp/ex main.c
/tmp/ex /tmp/ex.sock ../status.json
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
