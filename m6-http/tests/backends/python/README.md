# Python: native compute behind a thin dispatch layer

An m6 backend example. `docs/m6-backend-examples.md` §4 is where the set is
explained; this file says whether you are in the right place.

The usual framing of "Python is slow but has libraries" is the wrong way to
think about this one.

For numerical and scientific backends **the work does not happen in Python**.
`numpy` dispatches into BLAS and LAPACK, `scipy` into Fortran kernels, `torch`
and `onnxruntime` into optimised native code or a GPU. Python is the
orchestration layer, and its cost is **per request, not per element**.

So the fit is: **the request is small, the computation is large, and the
computation has a native implementation someone else has already optimised.**
Model inference, a statistical summary over a dataset, an image transform, a
similarity search over embeddings.

**Read this example's benchmark number as the floor of what dispatch costs, not
as a verdict on the language.** `/status` is deliberately the shape Python is
worst at: trivial per-request work at a high rate with nothing native to
dispatch into. On the build host it measures about 3,500 rps against 29,000 for
plain Rust. That is the honest number for this shape and it says nothing about a
backend doing one `numpy` call.

No `numpy` here on purpose: the example demonstrates the contract, and a
numerical dependency would make it about `numpy` and put a wheel build in the
gate.

## Run it

```sh
python3 main.py /tmp/ex.sock ../status.json
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
