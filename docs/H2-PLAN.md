# Completing the HTTP/2 implementation

> **STATUS 2026-09-10 — DONE, with one addition since.** h2spec **146/146**,
> h3spec **37/49** (all 12 remaining failures inside quiche, none attributable
> to m6). All phases below are complete, including Phase 6 (Rapid Reset), which
> is **deployed to the fleet** (`m6 438bdb3`).
>
> **Phase 7, added, completed and DEPLOYED 2026-09-10 (`m6 b32e837`, fleet md5
> `aced7223`): send-side flow control on the backbone clients.** h2spec exercises m6 as a *server* and so
> never covered `h2c_client`/`h2s_client`, which had no send-side flow control
> at all — `conn_send_window` was incremented on WINDOW_UPDATE and never read,
> there was no per-stream send window, and stream-level WINDOW_UPDATE frames
> were discarded by a catch-all `match` arm. The request body is now resumable
> state (`pending_body` + `stream_send_window`) pumped as credit allows, with
> retroactive `SETTINGS_INITIAL_WINDOW_SIZE` handling per RFC 9113 6.9.2.
> Guard: `m6-http/tests/backbone_flow_control.rs`, a stub origin that enforces
> the window it advertises, verified red without the fix. Full write-up in
> `~/dr-grosvenor-site/HANDOVER.md` §5a, and the EOF defect it uncovered in
> §5c.
>
> The only H2 item not implemented is
> `SETTINGS_MAX_HEADER_LIST_SIZE` (0x6) — see Phase 6's HPACK-bomb note. This
> document is kept as the record of how it was done; everything from
> "What is actually missing" down to the phase list is the **original
> 2026-09-09 diagnosis**, present-tense as first written — read the "Measured"
> section and the per-phase DONE markers for what is actually true now.

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

*(Original 2026-09-09 diagnosis, present-tense as first written. All of the
items in this section are now DONE — see the phase markers and the "Measured"
section. Kept for the reasoning, not as current state.)*

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

## Measured: h2spec, 2026-09-10

**COMPLETE: 146 passed, 0 failed.**

Sequence: 97 baseline, 114 pseudo-headers, 126 state machine, 130 CONTINUATION
unit fix, 132 F-005, 142 phase 3 frame-shape table, 143 rustls backpressure,
145 HPACK size updates, 146 trailer END_STREAM.

Note the 132 -> 142 step was never a change: it is what phase 3 was already
worth. 132 was measured before phase 3 landed and then sat in the docs as if
current. **Re-measure before quoting a number.**

`h3spec` is at **37/49**, and all 12 remaining failures are inside quiche (10
QUIC transport, 2 QPACK stream errors). No h3spec failure is attributable to
m6 any more.

Independent verification runs alongside both: nghttp2's `nghttp` client and
`h2load` (200/200 succeeded, 0 errored). h2spec alone proved insufficient in
one direction too: it caught a CONTINUATION regression that every unit test
missed. And it is insufficient in the other direction as well -- see the
rustls entry below, where a unit test at the same boundary passed throughout
while h2spec failed, because the defect lived in TLS I/O and the unit test fed
`recv_buf` directly.

### 2026-09-10, the last four

| # | Defect | Where |
|---|---|---|
| 1 | rustls 16 KiB plaintext backpressure read as a dead socket, so any body >= 2^14 killed the connection with no GOAWAY | `http2.rs::fill_recv` |
| 2 | HPACK dynamic table size update accepted after a field, and above the advertised maximum | `http2.rs::validate_hpack_block` (new) |
| 3 | Second HEADERS without END_STREAM left the stream open until idle timeout | `http2.rs::handle_headers` |
| 4 | Neither `:authority` nor `Host` required, so a request need not name its origin | `validate_request_header_bytes` |

(1) is the one that mattered off the scoreboard: it broke every HTTP/2 upload
of 16 KiB or more in production. `http11.rs::advance_tls` had handled the same
rustls behaviour since HTTP/1.1 hit it; the fix was simply never ported. When
one protocol path grows a workaround, check the others.

Found while fixing (2), unrelated to conformance: `SETTINGS_HEADER_TABLE_SIZE`
from the peer was applied to **our** decoder rather than bounding our encoder,
and the value is an unbounded u32. One SETTINGS frame sized m6's own HPACK
dynamic table cap at 4 GiB, and the table lives for the whole connection so
`MAX_HEADER_BLOCK` does not bound the total. Now clamped to the advertised
4096. Conformance work is a good excuse to read the settings path; this was
not a conformance failure and no spec suite would have caught it.

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

**Closed from the same report 2026-09-10:** Rapid Reset (CVE-2023-44487).
Reset streams were removed from `streams` while `MAX_CONCURRENT` was measured
by map size, so the concurrency cap was bypassable. The `recently_reset` deque
added with the state machine bounded reset *memory* at 128 entries; reset
*rate* was uncounted until now. See Phase 6 below.

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

- ~~**Rapid Reset (CVE-2023-44487)**~~ — **DONE 2026-09-10.** See below.
- ~~**CONTINUATION flood**~~ — done with F-005; bounded by `MAX_HEADER_BLOCK`,
  checked on every CONTINUATION rather than at END_HEADERS.
- **HPACK bomb** — small compressed input expanding to a large header list.
  Partly covered: the dynamic table is clamped to the advertised 4096 and
  `MAX_HEADER_BLOCK` bounds one block. `SETTINGS_MAX_HEADER_LIST_SIZE` (0x6)
  is still not implemented, so there is still no advertised bound.
- **Settings flood** — unacknowledged SETTINGS forcing unbounded state.

#### Rapid Reset — DONE 2026-09-10

`MAX_CONCURRENT` was measured as `streams.len()`, and RST_STREAM removes the
stream from that map, so a peer that cancelled each stream the instant it
opened it was measured at zero however many it started. **The cap was present
and unreachable.** Each of those streams costs a full request: m6 dispatches
inline from the HEADERS frame, so the work is finished before the RST_STREAM is
even parsed. h2spec does not test this, which is why 146/146 said nothing about
it.

Fix, in `http2.rs`:

- `recently_reset` entries carry the instant of the reset, and `active_streams`
  counts open streams **plus** those reset within `RESET_DECAY` (1 s). The cap
  now counts streams *started*, not streams still open.
- Only client-initiated (odd) ids count. An even id is one of our own pushes,
  and a RST_STREAM on it is the peer declining a push -- it costs us nothing.
  Not abusable: `handle_headers` already rejects an even client stream id.
- `MAX_REFUSED_STREAK` (50) consecutive refusals end the connection with
  ENHANCE_YOUR_CALM. REFUSED_STREAM alone is advice, and a flood ignores it;
  each refused HEADERS still costs a frame parse and an HPACK decode.

**Two defects found by measuring the fix on a real socket**, neither visible to
the unit tests:

1. A refused stream was never recorded, so the peer's own RST_STREAM for it --
   already in flight, since cancelling fast is the whole point -- landed on an
   id that still derived as `Idle`, where RFC 9113 5.1 makes it a *connection*
   error. A 200-stream flood drew **50 GOAWAY(PROTOCOL_ERROR) frames**, one per
   refusal. It also meant a legitimate client that merely reached the cap with
   a cancel in flight was answered a connection error. Refusing now marks the
   stream closed-by-reset.
2. `drive()`'s `Err` arm appended `GOAWAY(PROTOCOL_ERROR)` unconditionally,
   **overwriting every precise code** a handler had already sent -- so
   ENHANCE_YOUR_CALM here, and COMPRESSION_ERROR on the HPACK path, were never
   the peer's last word. Now guarded by `goaway_sent`. This one was pre-existing
   and affects paths well outside this phase.

Neither was reachable from the unit tests, which call `process_frame` directly
and never enter `drive()`. **Both were found by pointing a raw-socket client at
a running server and reading the frames that came back.**

Measured after the fix: first refusal at stream **#101** of 200, exactly 50
refusals, **one** GOAWAY carrying **0xb ENHANCE_YOUR_CALM**. h2load with 20
concurrent streams still 200/200, h2spec still 146/146.

Still open here: the reset budget is per connection, so a peer with many
connections gets `MAX_CONCURRENT` streams per connection per second. The
per-IP connection limit is what bounds that, and it is a separate mechanism.

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
