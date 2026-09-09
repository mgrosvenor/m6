# Completing the HTTP/2 implementation

**Decision taken 2026-09-09.** m6 keeps one concurrency model — the synchronous
epoll loop — and HTTP/2 is a hard requirement. That rules out the `h2` crate
(async, tokio-coupled) and rules out dropping H2. The remaining honest options
were a sans-io codec or finishing the hand-written implementation properly.

**Options rejected, with reasons:**

- **`h2` (hyperium)** — async only. Integrating it means a tokio runtime in the
  request path, i.e. two concurrency models sharing the cache lock. Rejected on
  the standing architectural constraint, not on quality.
- **`h2-sans-io`** — 2 stars, 16 commits, targets RFC 7540 (obsoleted by 9113),
  and explicitly excludes "connection/stream lifecycle management", which is
  exactly where most findings are. Replacing audited code with this would trade
  a known liability for an unknown one.
- **nghttp2 via `libnghttp2-sys`** — genuinely battle-tested and genuinely
  sans-io (`nghttp2_session_mem_recv`/`mem_send` + callbacks), so it *would* fit
  the epoll loop the way quiche already does for H3. Kept on the table as the
  fallback if measurement says the gap is too large. Cost: raw bindgen FFI (the
  high-level wrapper is "very early phase"), several hundred lines of `unsafe`,
  callback lifetime management, a C build dependency, and losing Rust memory
  safety in the H2 path.
- **Dropping H2** — rejected by the owner as a hard requirement.

## What is actually missing

Read out of `m6-http/src/http2.rs`, not inferred:

### 1. The stream state machine has 3 of the 7 required states

```rust
enum StreamState { Open, HalfClosedRemote, Closed }
```

RFC 9113 §5.1 requires **idle, reserved (local), reserved (remote), open,
half-closed (local), half-closed (remote), closed**.

Missing: `Idle`, `ReservedLocal`, `ReservedRemote`, `HalfClosedLocal`.

This is the root of most of the open findings. Without `Idle` you cannot detect
a frame arriving on a stream that was never opened; without `HalfClosedLocal`
you cannot reject a client that keeps sending after we have finished. Nearly
every "frame in wrong state" conformance test depends on states that do not
exist here.

### 2. Five of the fourteen error codes exist

Present: `NO_ERROR`, `PROTOCOL_ERROR`, `STREAM_CLOSED`, `FRAME_SIZE_ERROR`,
`REFUSED_STREAM`.

Missing: `INTERNAL_ERROR` (0x2), **`FLOW_CONTROL_ERROR` (0x3)**,
`SETTINGS_TIMEOUT` (0x4), `CANCEL` (0x8), **`COMPRESSION_ERROR` (0x9)**,
`CONNECT_ERROR` (0xa), `ENHANCE_YOUR_CALM` (0xb), `INADEQUATE_SECURITY` (0xc),
`HTTP_1_1_REQUIRED` (0xd).

The two in bold are not cosmetic. **`COMPRESSION_ERROR` is what a server MUST
send when HPACK decoding fails, and it MUST be a connection error** — the
dynamic table is shared across the connection, so a decode failure means every
subsequent header block on that connection is untrustworthy. Sending a stream
error instead leaves a poisoned connection alive. `FLOW_CONTROL_ERROR` is
required when a peer overruns a window.

### 3. `SETTINGS_MAX_HEADER_LIST_SIZE` (0x6) is not implemented

Five of six settings are handled. The missing one is the *defence* setting: it
is how a server advertises the largest header list it will accept, and it is the
documented mitigation for HPACK bombs and CONTINUATION floods. Without it there
is no advertised bound, and a client has no way to know one exists.

## Phases

Ordered so that each phase is independently shippable and independently
verifiable. Phase 0 comes first because it converts the argument into a number.

## Measured: h2spec, 2026-09-09

**Latest: 132 passed, 14 failed** (2026-09-09, after phases 1-2, the
CONTINUATION fix and F-005). Sequence: 97 baseline, 114 pseudo-headers, 126
state machine, 130 CONTINUATION unit fix, 132 F-005.

Independent verification now runs alongside h2spec: nghttp2's `nghttp` client
and `h2load` (200/200 succeeded, 0 errored), and `h3spec` for HTTP/3 at 33/49
— of which 10 are in quiche rather than m6. h2spec alone proved insufficient
in one direction too: it caught a CONTINUATION regression that every unit
test missed.

Original baseline: **146 tests, 97 passed, 49 failed** (h2spec v2.6.0 against staging, TLS,
`-h 127.0.0.1 -p 443 -t -k`, 240s).

Two-thirds pass with no conformance work ever done. The framing layer, HPACK
codec, SETTINGS negotiation and stream multiplexing foundations are sound — the
failures are concentrated, not diffuse.

**49 failures, five root causes:**

| root cause | failures | RFC sections |
|---|---:|---|
| Pseudo-header validation | ~21 | 8.1, 8.1.2, 8.1.2.1/.2/.3/.6 |
| Stream state machine | ~16 | 5.1, 5.1.1, 5.4.1, 6.4 |
| Frame validation table | ~11 | 4.2, 6.1, 6.3, 6.5, 6.7, 6.10 |
| HPACK error handling | ~2 | HPACK 4.2, 6.3 |
| Flow control | ~2 | 6.9.1 |

**This settles the build-vs-replace question in favour of building.** An earlier
draft of this plan said "40+ failures means nghttp2 becomes the honest answer".
That threshold was wrong: it treated *test count* as *bug count*. 49 failing
tests over 5 well-specified defects is a different proposition from 49
independent ones, and every one of the five is in a part of the RFC that
prescribes the behaviour exactly.

**nghttp2 is now off the table** unless something below proves far harder than
it looks.

### Re-ordered by measured payoff

The original phase order put the state machine first because it looked like the
root of everything. The measurement says otherwise: **pseudo-header validation
is the single largest cluster AND the easiest work** — it is input validation on
an already-decoded header list, with no state machine involvement at all.

Representative failures, all currently answered with a 200 where the RFC
requires PROTOCOL_ERROR:

- "Sends a HEADERS frame that contains the header field name in uppercase letters"
- "Sends a HEADERS frame that contains a unknown pseudo-header field"
- "Sends a HEADERS frame with empty \":path\" pseudo-header field"

That is one validation function over the decoded list, and it clears ~21 of 49.

**Order: 1. pseudo-headers (~21) → 2. state machine (~16) → 3. frame validation
table (~11) → 4. HPACK errors (~2) → 5. flow control (~2).**

### Reproducing the measurement

    ssh -p 4022 root@<build-host> \
      "h2spec -h 127.0.0.1 -p 443 -t -k --timeout 5"

**Against staging, never production.** h2spec deliberately sends malformed
frames and this code has already had one remotely triggerable panic (F076).

Raise `requests_per_min` on staging for the run and restore it afterwards —
h2spec opens ~146 connections and would otherwise measure the rate limiter. The
same trap invalidated the first benchmark run.

Note when reading raw h2spec output: each failure line is emitted twice (the
tool redraws it), so a naive `grep -c` reports exactly double. 98 counted means
49 real.

## External scan: F-005, 2026-09-09 — FIXED

An external audit (an LLM-driven scan, run by a third party with the owner's
knowledge) reported unbounded HTTP/2 header accumulation as High severity.
Four defects, verified here against the real parser before touching anything.

| # | defect | status when reported | now |
|---|---|---|---|
| 1 | No inbound `SETTINGS_MAX_FRAME_SIZE` check (RFC 9113 4.2) | live | fixed |
| 2 | No cap on accumulated header block (CVE-2024-27316 class) | live | fixed |
| 3 | Stray CONTINUATION accepted (6.10) | **already fixed** | fixed |
| 4 | Flow-control window overflow (6.9.1) | live, worse than reported | fixed |

**Verify before believing, in both directions.** The report's four PoCs were
run verbatim first, each asserting the *buggy* behaviour. A and B passed,
confirming both live. C failed — defect 3 had been closed hours earlier by
the 5.1 state-machine work, and the scan was against a snapshot that
predated it. D did not merely fail: it *panicked*, because the unchecked
`+=` on an `i32` overflows in a debug build. That is a remotely reachable
abort, which is worse than the "wraps and stalls" the report described.

Neither accepting nor dismissing the report would have been right. One
finding was stale, one was understated, two were exactly as described.

**Why the flood mattered more than its class suggests.** Every other defence
misses it. The per-IP rate limiter runs inside the request handler, and the
handler is only reached once a request *completes*; a CONTINUATION sequence
that never sets END_HEADERS never dispatches, so the limiter never counts it.
The idle timeout is reset by each frame received, so a trickle holds the
connection open while memory grows. One connection, unauthenticated, until
the single-threaded server is OOM-killed. The cap is therefore checked on
every CONTINUATION rather than at END_HEADERS — a check that only runs at the
end of a block that never ends never runs at all.

Seven regression guards, the PoCs inverted, plus two that a frame at exactly
the limit and an ordinary WINDOW_UPDATE still work: a cap that breaks
legitimate traffic is not a fix.

**Still open from the same report:** Rapid Reset (CVE-2023-44487). Reset
streams are removed from `streams`, and `MAX_CONCURRENT` is measured by map
size, so the concurrency cap is bypassable. Partially mitigated — the
`recently_reset` deque added with the state machine bounds reset *memory* at
128 entries — but reset *rate* is still uncounted. Phase 6.

### Phase 0 — Measure (h2spec) — DONE, see above

Run the conformance suite against **staging**, never production: h2spec
deliberately sends malformed frames, and m6 has already had one remotely
triggerable panic in this exact code (F076 — a HEADERS frame with PRIORITY and
a payload under five bytes sliced out of bounds). Running it against prod risks
taking the site down.

Raise staging's rate limit for the run and restore it afterwards — the same trap
that invalidated the first benchmark run, where the harness measured
`requests_per_min = 1200` rather than the server.

Output: a ranked list of named failures mapped to RFC sections. **This decides
whether the rest of this plan is a few days or a rewrite**, and therefore whether
nghttp2 comes back on the table.

### Phase 1 — The state machine

Replace the 3-state enum with all 7. Implement §5.1's transition table
explicitly — a `match (state, frame_type, flags)` that names every legal
transition and rejects the rest — rather than as scattered `if` checks.

The table is the deliverable. It is what makes the remaining phases verifiable
and what a reviewer can check against the RFC line by line.

Also §5.1.1: stream identifiers must increase monotonically; a client-initiated
stream must be odd; reusing or going backwards is a connection error.

### Phase 2 — Error discipline

Add the nine missing codes, then make the **connection-error vs stream-error**
distinction explicit at every rejection site. Getting this wrong is worse than
missing the code entirely: a connection error sent as a stream error leaves a
connection alive in a state both peers disagree about.

Specifically: HPACK failure → connection error, `COMPRESSION_ERROR`. Flow
control overrun → `FLOW_CONTROL_ERROR`, connection or stream depending on which
window was exceeded.

### Phase 3 — Flow control ledgers

Per-stream `recv_window` exists. What is missing is a single accounting point
that both windows go through, so connection and stream credit cannot drift
apart. Include the §6.9.1 rule that a window must not exceed 2^31-1, and that a
`WINDOW_UPDATE` of 0 is a protocol error.

### Phase 4 — HPACK hardening and `MAX_HEADER_LIST_SIZE`

Implement setting 0x6, advertise it, and enforce it during decode — not after,
which is what makes a bomb work. Bound the dynamic table, handle table-size
updates, and reject Huffman padding violations.

This is the phase where using a maintained HPACK crate is worth evaluating
separately from the rest: HPACK is a pure codec with no I/O, so a crate here
carries none of the runtime-coupling objections that ruled out `h2`.

### Phase 5 — Frame validation table

One table covering every frame type × every state, including the rules that are
easy to miss: frames on stream 0 that require a stream and vice versa,
`CONTINUATION` that must immediately follow `HEADERS`/`PUSH_PROMISE` with no
interleaving, `PRIORITY` accepted-and-ignored (deprecated in 9113 but still must
parse), and unknown frame types ignored rather than rejected.

### Phase 6 — Denial of service

Named classes, each with a test:

- **Rapid Reset (CVE-2023-44487)** — a client opens and immediately resets
  streams, so concurrency limits never bind while the server keeps doing work.
  Needs accounting of reset streams, not just open ones.
- **CONTINUATION flood** — unbounded header fragments before END_HEADERS.
- **HPACK bomb** — small compressed input expanding to a large header list.
- **Settings flood** — unacknowledged SETTINGS forcing unbounded state.

### Phase 7 — Green and keep it green

h2spec passing in CI, plus fuzzing the frame parser. The parser is the right
fuzz target: it is pure `&[u8]` in, and it is where the one known panic was.

## What "done" means

h2spec green, the frame parser fuzzed, and the README's RFC compliance section
updated with the real numbers. **Not** "the audit findings are closed" — the
findings are a proxy for conformance, and h2spec is the actual measure.

## Standing constraint

Every phase ships behind the existing discipline: tests that are verified to
fail against the defect first, zero warnings on Linux, a full `run-tests.sh`
before deploy, and staging before production.
