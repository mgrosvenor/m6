# m6-core — Implementation Plan

Step by step migration to the design in `m6-core.md`. Each phase is
independently shippable, independently verifiable, and ordered so that nothing
before Phase 4 changes a byte on the wire.

**This is a re-verification exercise against a live system, not a refactor.**
The conformance suites, the full test suite and the production fleet are the
gate at every step. A phase is not done because the code moved; it is done when
the gate is green and the fleet is verified.

---

## The target, in one picture

```
   m6-core        the library. Service loop, HTTP types, HTTP semantics,
                  HTTP/1.1, config, signals, logging, content, safety,
                  testkit. Linked OPTIONALLY by consumer apps.

   m6-http        the edge. TLS, HTTP/2, HTTP/3, cache, routing, rate
                  limiting, proxy policy. Links m6-core.

   m6-html        a DEFAULT APP. Templating (Tera) over m6-core.
   m6-file        a DEFAULT APP. Static files over m6-core.
   m6-auth-server a DEFAULT APP. Auth over m6-core.

   m6-render      DELETED. Scaffolding to m6-core, templating to m6-html.

   consumer apps  link m6-core, or nothing at all. Never a default app.
```

Three decisions this plan implements, all already taken:

1. **HTTP/2 and HTTP/3 stay in `m6-http`.** They have exactly one consumer,
   permanently. See `m6-core.md` §4.1.
2. **`m6-render` is dissolved.** Scaffolding to `m6-core`, templating to
   `m6-html`, crate deleted. Consumer apps link `m6-core` only.
3. **Duplicate dependencies are acceptable; coupling is not.** If
   `render-analytics` wants Tera it declares Tera. It does not acquire a
   template engine as a side effect of wanting a server loop.

---

## Why this ordering

Two constraints drive it.

**Risk descends.** Phases 1 to 3 cannot change observable behaviour. Phase 4
touches the wire but only HTTP/1.1. Phase 5 is the largest single move. Phase 6
changes what ships and how. Nothing in the plan touches the HTTP/2 or HTTP/3
implementations, which are the highest-defect-density code in the project.

**The harness comes early.** Moving code without a shared test harness means
writing the tests twice, once where the code is and once where it lands.

---

## Phase 0 — Prerequisites

No code. Settle two things that later phases assume.

**0.1 `m6-html`'s fate — DECIDED 2026-09-10.** `m6-render` is **dissolved**,
not repackaged. Its service scaffolding moves into `m6-core` and its templating
moves into `m6-html`. The crate is deleted.

This is simpler than either option originally offered. There is no framework
crate left to name, no library that only one binary links, and no cross-repo
path dependency to pin, because nothing outside `m6-html` will link templating
at all.

| `m6-render` module | Lines | Destination |
|---|---|---|
| `app.rs` (service loop, thread pool, routing, signals, compression) | 2,617 | `m6-core`, minus the render step |
| `request.rs` | 643 | `m6-core` |
| `response.rs` | 252 | `m6-core` |
| `config.rs` | 452 | split: app config to `m6-core`, template config to `m6-html` |
| `server.rs` | 152 | `m6-core` |
| `multipart.rs` | 111 | `m6-core`, optional feature. It is request body parsing, not templating |
| `util.rs` | 33 | `m6-core` |
| `error.rs` | 18 | `m6-core` |
| `template.rs` | 695 | **`m6-html`** |

`m6-html` stops being six lines and becomes a real app: the Tera integration,
template discovery and watching, and the render step, over `m6-core`. That is
the right size for the one process whose job is rendering HTML.

**0.2 Benchmark suite — DONE.** All three `critical_path` benches had stopped
compiling and nothing noticed, because `run-tests.sh` passes `--no-bench`.
Fixed in `5de80c7`. Latency is the metric this plan is judged on and it could
not be measured. `run-tests.sh` should additionally *build* the benches so this
cannot recur silently.

**0.3 Baseline recorded**, `m6-http` critical path, 2026-09-10:

| Bench | Baseline |
|---|---|
| `make_lookup_key` | 13.3 ns |
| `cache_hit` | 56.2 ns |
| `cache_miss` | 21.7 ns |
| `stats_record` | 3.72 ns |
| `h3_header_extract` | 7.39 ns |
| `full_cache_hit_path` | 56.5 ns |

Criterion's own comparison against its stored baseline is **not** usable: that
baseline predates whenever the suite stopped compiling, so its age is unknown.

**These numbers are laptop figures and are not comparable to build-host
figures.** They were also taken as a single run per bench, which 2026-09-11
measurement showed is not a comparative method on either machine: the same
commit measured twice on the build host moved 14.2% on `full_cache_hit_path`.
Use `tools/paired_bench.sh`, and see "Where to run them" in `BENCHMARKS.md`.
Treat the table above as a record of scale, not as a regression gate.

**Every phase re-runs this and reports the delta**, paired and interleaved. A
phase that moves any of these materially without an explanation is not done —
and a delta smaller than the host's own run-to-run drift is not a measurement,
it is a coin toss with extra steps.

**Gate:** recorded in `m6-decisions.md` under Crate Boundaries. Done.

---

## Phase 1 — Small consolidations

Mechanical. No behaviour change except where a duplicate was wrong.

| Move | From | Detail |
|---|---|---|
| Path parameter validation | 3 implementations | `m6-core::path` is the survivor. `m6-render::request::validate_path_param` and `m6-file::route::is_safe_param`/`is_safe_catchall` are deleted |
| Signal handling | 4 implementations | `m6-core::signal::ShutdownHandle` is the survivor. `m6-file`, `m6-http`, `m6-md`, `m6-render` call it |
| Case-insensitive header lookup | `m6-http::analytics::header` | Moves to `m6-core::http` |
| Random token generation | 2 implementations | `m6-core::random_hex_token(len)` |
| Dead code | `m6-core::config`, `m6-core::path::safe_resolve` | Delete or find the caller. ~360 lines with no consumers |

**Do path validation first and separately.** It is a security boundary with
three different behaviours today: `m6-render`'s `relpath` branch performs no
character validation at all, while `m6-core` rejects leading slashes and
restricts the character set.

**Gate:** full suite green. A test asserting the divergence table from
`CORE-DUPLICATION-AUDIT.md` §1 resolves one way. Fleet unchanged.

**Risk:** low. **Rollback:** revert; nothing else depends on it yet.

---

## Phase 2 — `m6_core::testkit` — **done**, commits `3b9b895`..`939831e`

Everything after this needs it.

Provides: port claiming without a time-of-check-to-time-of-use race, binary
location, service spawn and teardown, readiness waits, and a byte-level HTTP
client for framing tests. Behind the `testkit` feature, so no production binary
links it.

Migrated: all four `m6-http` suites that carried their own port logic, plus
`m6-file` and `m6-html`, which each had their own copy of everything.
`m6-http/tests/common/` is deleted.

**What it found.** Two things worth more than the deduplication.

*The known flake was never diagnosable.* All four suites piped a child's stderr
and then never read it, which is two bugs at once: a child logging more than one
pipe buffer blocks in `write` and stops serving, and a child that dies has its
last words thrown away, so the test reports `ConnectionRefused` and nothing
else. `Service` drains stderr continuously and prints it on any failed assertion
about the service.

*Every m6 service was dying on SIGTERM instead of shutting down.* Blocking a
signal is per-thread and threads inherit the mask at creation; logging was
initialised first, and `tracing_appender::non_blocking` spawns a writer thread,
so by the time `main` blocked SIGTERM that thread had had it unblocked for a
hundred lines. The kernel delivered every signal there and the default
disposition killed the process. m6-file had not logged a shutdown in thirty days
of production. See "Signal Handling" in `m6-decisions.md`; the fix is
`m6_core::signal::block()` first in `main`, now asserted rather than documented.

*Then the same argument applied to shutdown itself*, commit `939831e`. Phase 2
unified the signal *mechanism* and left the *sequence* as five variants: three
install overloads, three wake mechanisms, one of five services unlinking its
socket, three of five logging a startup line, two of five logging a shutdown
line, and all three render apps logging `m6-render` rather than their own name.
None of that was demanded by anything the services do. There is now one entry
point, `ShutdownHandle::install(Service)`, and the differences are data:
`name`, `socket` and `wake_fd`. Core owns all four lifecycle log lines,
asserted by `testkit::assert_lifecycle_logged` in four services.

That work found one more testkit defect: `Service::spawn` piped stderr and
nulled stdout, and m6 services log to **stdout**, so the harness had been
discarding every log line it existed to capture. It drains both streams now.

Two suites also had a shared-socket-path hazard: `m6-file` and `m6-html` put
their sockets at a fixed `$TMPDIR/<id>.sock` and deleted whatever was there
before spawning, so one run could unlink a socket a live server was using.

**Gate: MET.** Full suite green **10/10 consecutive runs** on `094965b`, plus
`check.sh` on the Linux build box (build and full suite). Ten runs rather than
one because this phase exists partly to fix an intermittent failure and a
single green run proves nothing.

The Linux box earned its place in the gate twice. It caught a Phase 1b
leftover, `poller.rs` still referencing the removed `sigmask` in its
`cfg(linux)` arm, so the tree did not compile on Linux at all. Then it caught
two readiness defects that macOS could not: see the follow-up commit below.

**Do not run the gate against a tree you are still editing.** A first attempt
scored 4/10 because runs 5 through 10 compiled a working tree that was being
changed underneath them. Those failures were the harness, not the code, and it
took a moment to be sure of that. Finish, then gate.

**Risk:** low for production, moderate for the suite. **Rollback:** the old
helpers stay until the last file is migrated.

**Bonus:** this is the prerequisite for `m6-backend-examples.md`, whose shared
assertion set needs exactly this harness.

---

## Phase 3 — Semantics

Version-independent rules. Moves without touching any wire format.

**3.0 `HeaderSource` and `header()` → `m6-core::http`. Do this first.** Both
3.1 and 3.2 depend on it, and `Phase 1` deferred it. It lives in
`m6-http/src/analytics.rs` today, which is the wrong home for a general HTTP
abstraction.

**There is an orphan-rule cost, and it is worth knowing before starting.**
`HeaderSource` has impls for `[quiche::h3::Header]` and
`Vec<quiche::h3::Header>`. Once the trait is in `m6-core`, those become
`impl ForeignTrait for ForeignType` in `m6-http`, which Rust forbids, and
`quiche` must not become an `m6-core` dependency because that would undo the
decision that h2 and h3 stay in `m6-http`. The fix is one newtype in
`m6-http` wrapping the quiche slice, and it touches six production call sites
(`main.rs` 1318, 1342, 1385, 1933 and two more) plus four in tests. Small,
mechanical, but it is not zero and it is invisible until the compiler says so.

The alternative considered and rejected: have core's precondition function take
a lookup closure instead of a trait, dodging the orphan rule with less code.
Rejected because it leaves every consumer writing its own lookup, and `m6-file`
— the crate whose defect this phase exists to fix — would get nothing shared.

**There are four header parsers and four conventions**, which is the real
duplication behind Phase 1's one-line "case-insensitive header lookup" entry:

| crate | parser | name case at rest | lookup |
|---|---|---|---|
| `m6-core` | `parse.rs:104` | as sent | `RawRequest::header`, lowercases both sides, allocates per header scanned |
| `m6-file` | `http.rs:45` | lowercased | `k == "if-none-match"` |
| `m6-render` | `server.rs:47` | lowercased | `k == name` |
| `m6-http` | h1/h2/h3 | as sent | `eq_ignore_ascii_case`, no allocation |

`m6-file` and `m6-render` are correct at runtime because they normalise once at
parse. The hazard is that `k == "literal"` is sound only under an invariant
established in a different file and invisible at the call site: hand either one
a `RawRequest` from `m6-core`'s parser and every conditional-request header
silently stops matching. The allocating lookup is **not** a live latency
problem — `m6_core::http::RawRequest` reaches production only through
`m6_core::server::UnixServer`, whose sole consumer is `m6-auth-server`, which
is not in the fleet.

**3.1 `validate_request_header_bytes` → `m6-http/src/fields.rs`, NOT
`m6-core`. The plan was wrong here.**

It was listed as a move to core on the strength of a claim in
`m6-decisions.md` that the symbol is "RFC 9110 semantics, not h2 wire format".
Checked against the code, every rule in it is RFC 9113 8.2.1, 8.2.2, 8.3 or
8.3.1, restated by RFC 9114 4.3. HTTP/1.1 has no pseudo-headers, allows any
field-name case, and requires `Connection` rather than banning it, so none of
these rules mean anything outside h2 and h3.

Moving it to core would put protocol-version-specific rules in the crate whose
whole premise is version independence, and would have quietly undermined the
boundary decision it was cited to support.

The real problem was a name: the H3 path imported from a file called
`http2.rs`, which looks like a layering violation and is not one. Both
protocols now import from `fields.rs`. No crate boundary is crossed and nothing
about the decision changes.

**3.2 Conditional requests and preconditions → `m6-core`.** The full RFC 9110
13.2.2 precedence with weak comparison, from `m6-http::cache`. Then **fix
`m6-file`**, which has its own inline version missing weak comparison and two
of the four precedence steps.

This one has a live defect to prove the move worked, and its shape matters.
Verified 2026-09-11 against the release binary in isolation, no production
involved:

| RFC 9110 13.2.2 step | `m6-file` | correct? |
|---|---|---|
| 1. `If-Match: "nope"` | 200 | ✗ 412 — not implemented |
| 2. `If-Unmodified-Since: <past>` | 200 | ✗ 412 — not implemented |
| 3. `If-None-Match: "<etag>"` | 304 | ✓ |
| 3. `If-None-Match: W/"<etag>"` | **200** | ✗ 304 — strong comparison |
| 3. `If-None-Match: *` | 304 | ✓ |
| 4. `If-Modified-Since` | 304/200 correctly | ✓ |

One line is responsible, `m6-file/src/handler.rs:183`:
`inm == "*" || inm.split(',').any(|tag| tag.trim() == etag)`. Byte equality is
*strong* comparison; `If-None-Match` requires weak (RFC 9110 8.8.3.2).

**On the wire the symptom is intermittent, and that is the trap.** `m6-http`'s
own implementation is correct and already deployed (`134c50c`, with
`etag_weak_eq` and the full four-step precedence), so:

| path | who answers | weak `If-None-Match` |
|---|---|---|
| cache **hit** | `m6-http::evaluate_preconditions` | 304, correct |
| cache **miss** | `m6-file`, strict comparison | 200, wrong |

Both were observed on production within minutes of each other, 200 then 304
once the entry warmed (`age: 66`). A symptom that comes and goes with cache
state is exactly what gets written off as noise.

**So the gate must be measured on a cold cache key**, or it passes against
unfixed code. One trap in doing that: forcing a miss by varying
`Accept-Encoding` changes which *variant* is served, and `m6-file` appends
`-br`/`-gz` to the ETag per variant, so the client's identity ETag then
legitimately does not match and 200 is correct. Compare against the ETag for
the variant the request will actually receive.

**3.3 Caching semantics → `m6-core`. DROPPED 2026-09-11.**

Counted rather than assumed: **nothing outside `m6-http` parses or evaluates
`Cache-Control`.** `m6-file` and `m6-render` only emit fixed strings. By the
architecture nothing ever will, because m6-http *is* the cache and the backends
exist behind it.

So this would move ~2,000 lines of the highest-consequence code in the project
across a crate boundary to serve no consumer, and the plan itself rates it
"moderate risk... `cache.rs` carries the RFC 9111 behaviour the whole site
depends on".

It also contradicted a decision already taken. `m6-decisions.md` keeps h2 and h3
in `m6-http` because "they have exactly one consumer, permanently". RFC 9111
caching has exactly one consumer, permanently, for the same structural reason.

**The rule this settles, which now covers 3.1 and 3.3 together:**

> Code moves to `m6-core` when it has more than one consumer. Single-consumer
> code stays with its consumer, however core-ish it looks.

It reproduces every decision already taken. Counted 2026-09-11:

| component | consumers | verdict |
|---|---|---|
| `signal` | 5 | core |
| `log` | 4 | core |
| `compress`, `validate_path_param` | 2 each | core |
| preconditions | 2 | core, and the second was broken |
| HTTP/1.1 | 4 parsers in 4 crates | **core — Phase 4** |
| RFC 9111 caching | 1 | m6-http |
| h2/h3 field validation | 1 | m6-http |
| HTTP/2, HTTP/3 | 1 | m6-http |

**Gate:** full suite green; h2spec 146/146; h3spec 37/49; the weak-ETag case
returns 304 on the wire after deploy.

**Risk:** moderate. `cache.rs` is 2,489 lines and carries the RFC 9111
behaviour the whole site depends on. **Do 3.2 before 3.3**: it is smaller, has
a proven defect, and validates the approach.

---

## Phase 4 — HTTP/1.1

The first phase that changes wire-handling code, and the last protocol move.

- Request and response parse and serialise → `m6-core` (partly there already:
  `m6-core::parse` exists and `m6-auth-server` uses it).
- Framing validation from `forward.rs`: `Transfer-Encoding` with
  `Content-Length`, conflicting lengths, unparseable lengths, the size
  ceilings.

This is the piece that makes a Rust backend short, because HTTP/1.1 is the
backend wire contract.

**Gate:** full suite; h2spec and h3spec unchanged; a body sweep of 16,000 /
16,384 / 65,535 / 262,144 / 1,048,576 bytes through every node returning 200,
which is the regression test that caught the last framing defect.

**Risk:** moderate to high. Framing bugs are smuggling bugs. Every rule in
`m6-backend-protocol.md` §3.2 exists because it was got wrong once.

---

## Phase 5 — The service loop

The largest single move, and the one consumer apps are waiting for.

**Partly done already, as a side effect of Phase 4.** `serve_connection` and
`Responder` moved into `m6-core` (commit `4680cf7`) to get HEAD and keep-alive
right in one place, and `m6-render`'s `handle_connection` and
`Response::write_to` were deleted onto them. So the connection lifecycle half
of `app.rs` has landed. What remains below is the rest: the thread pool,
bounded queue, 503 backpressure, routing, compression and config reload.

From `m6-render` into `m6-core`:

| Component | Lines | Note |
|---|---|---|
| `App` and the service loop | ~2,617 (`app.rs`) | Socket lifecycle, thread pool, bounded queue, 503 backpressure, routing, compression, config reload |
| `Request` | 643 | |
| `Response` | 252 | |
| `Error` / `Result` | 18 | |
| `config` | 452 | App config loading |
| `server` | 152 | |
| `util` | 33 | |

`m6-render` retains `template.rs` (695 lines) plus `multipart.rs`, and becomes
what its name says: templating.

This is the phase that answers the measurement behind the whole plan.
`m6-render` is **14% templating and 86% service scaffolding**, and three of its
four consumers have **zero template files** yet link Tera, comrak, pest,
chrono-tz, globset, slug and eleven other crates to get a server loop.

**Sub-steps, in order:** `Error`/`Result` first (18 lines, everything depends
on it), then `Request`/`Response`, then `config`, then `App` last.

**Gate:** full suite; `m6-html` and all three site renderers build and pass;
the site deploys and serves; `/contact` accepts a real submission end to end.

**Risk:** high, by size. **Mitigation:** `m6-render` re-exports the moved
symbols during the transition, so no consumer changes until Phase 6. Nothing
downstream breaks while the move is in flight.

---

## Phase 6 — Consumer apps link `m6-core` only

The payoff.

**6.1** `render-contact`, `render-analytics` and `render-cms` switch their
dependency from `m6-render` to `m6-core`, and remove the re-export shim.

**6.2** Any that genuinely want Tera declare it themselves. Per the decision:
duplicate dependencies are fine, coupling is not.

**6.3** `m6-render` (or `m6-html`, per Phase 0.1) keeps both.

Expected result: those three lose Tera, comrak, pest and the rest, and the site
repo's build stops pulling a template engine for services that render no
templates.

**Gate:** each renderer builds, tests, deploys, serves. Contact form submits
end to end. Analytics beacon records. Dependency-tree diff recorded in the
release notes, because the shrink is the point and should be a number.

**Risk:** low. By this phase the code has already moved.

---

## Phase 7 — Decouple the repositories

The cross-repo path dependency becomes removable once Phase 6 lands.

Today `dr-grosvenor-site/render-*/Cargo.toml` carries
`m6-render = { path = "../../m6/m6-render" }`. That hard-codes a filesystem
layout across a repo boundary with **no version constraint**, so the site links
whatever is on disk. `deploy.sh` rsyncs the whole `m6` tree to the builder and
`touch`es every source file to defeat mtime staleness. Your own comment records
the outage: "A stale m6 copy compiles cleanly against the wrong library and
yields a binary missing whatever the templates just started using".

Replace with a **git dependency pinned to a revision**. No registry needed, an
explicit version boundary, and `Cargo.lock` records exactly which `m6` commit
the site was built against. Upgrading becomes a deliberate commit.

**Gate:** the site repo builds with no `m6` checkout beside it. `deploy.sh`
stops syncing the `m6` tree and the `touch` workaround is deleted.

**Risk:** low technically, but it changes the release relationship between the
two repos and should be a recorded decision.

---

## Phase 8 — Backend examples

Now buildable, because Phase 2 provides the harness and Phase 6 provides
`m6-core` as a standalone app dependency.

Implement `m6-backend-examples.md`: C, C++, Python, Go, Rust without
`m6-core`, Rust with it, all written from `m6-backend-protocol.md`, all serving
the byte-identical `/status` payload, under `m6-http/tests/backends/`.

**Gate:** every example passes the shared assertion set; `/status` output is
byte-identical across all six; `run-tests.sh` fails on the build host if a
runtime is missing.

**Prerequisite:** Go installed on the build host.

**This phase is also the measurement.** The Rust-without-core against
Rust-with-core diff is what tells you whether Phase 5 produced a library worth
linking. If the difference is small, `m6-core` did not buy much and that is
worth knowing.

---

## Summary

| Phase | Changes the wire | Risk | Blocks | Status |
|---|---|---|---|---|
| 0 Prerequisites | no | none | everything | done |
| 1 Small consolidations | no | low | — | done, `3780834` |
| 2 Testkit | no | low | 3, 4, 5, 8 | done, `3b9b895` |
| 3 Semantics | no (fixes one bug) | moderate | 4 | done, `1ba5dfa`; 3.3 dropped |
| 4 HTTP/1.1 | yes | moderate-high | 5 | **done**, `20f6a8d`..`4680cf7`; 32/32 h1spec on all four targets |
| 5 Service loop | no | high (size) | 6 | **next** |
| 6 Consumer apps | no | low | 7, 8 | |
| 7 Decouple repos | no | low | — | |
| 8 Backend examples | no | low | — | |

Phases 1 and 2 were worth doing regardless of whether the rest proceeds, and
that held: Phase 1 closed a security divergence, and Phase 2 closed the open
test flake and found a production shutdown defect that had been live for a
month. Nothing is deployed yet; the whole sequence ships at the end.

**Stopping early is a valid outcome.** Phases 1 through 3 leave the codebase
better with no architectural commitment. The commitment starts at Phase 5.
