# Go: concurrent I/O and operational simplicity

An m6 backend example. `docs/m6-backend-examples.md` §4 is where the set is
explained; this file says whether you are in the right place.

Pick Go for backends whose work is mostly **waiting on other things**: fanning
out to several upstream APIs, calling cloud services, handling webhooks,
aggregating results from multiple databases.

Goroutines make a thousand concurrent outbound calls unremarkable, and the
standard library ships a good HTTP client and JSON codec, so a backend that is
mostly integration is mostly stdlib. Operationally it is one static binary with
no runtime to install, which matters when the backend is deployed somewhere
`m6-http` is not.

It is also the example that **tests the contract hardest**, because
`net.Listen("unix", ...)` with `http.Serve` puts a mature, strict HTTP
implementation behind the socket rather than a parser written for the occasion.
If m6-http ever emits something subtly wrong, this is where it shows first.

Built size: about 8.5MB, which is Go's static runtime and is not a defect.

## Run it

```sh
go build -o /tmp/ex .
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
