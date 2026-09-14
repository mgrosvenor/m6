# C++: stateful, in-memory, performance-critical

An m6 backend example. `docs/m6-backend-examples.md` §4 is where the set is
explained; this file says whether you are in the right place.

Pick C++ for backends that hold **substantial state in memory** and answer from
it: a Redis-shaped service, an in-memory index, a cache, a graph, a time series
buffer, queried over HTTP.

The reason is the standard library plus RAII: real containers, deterministic
destruction, and no GC pause between a request arriving and being answered. When
the work is "look it up in a large structure and serialise the answer", the
language is not fighting you.

**Read it beside `../c/main.c`.** Same syscalls, same contract, but the fd and
the payload are owned by objects whose destructors do the cleanup, so there is no
`close` or `unlink` to forget on an error path. That contrast is why both exist.

Built size: about 38KB on Linux.

## Run it

```sh
c++ -std=c++17 -O2 -pthread -o /tmp/ex main.cpp
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
