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
These numbers are the reference from here.

**Every phase re-runs this and reports the delta.** A phase that moves any of
these materially without an explanation is not done.

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

## Phase 2 — `m6_core::testkit`

Everything after this needs it.

Provides: socket and port claiming without a time-of-check-to-time-of-use race,
service spawn and teardown, request builders, and a raw-socket client for
framing-level tests (`tools/rapidreset.py` is the working prototype).

Then migrate the existing test files onto it. Four of eight in `m6-http/tests`
carry their own port logic today.

**Gate:** full suite green, run **at least ten consecutive times**, because
this phase exists partly to fix an intermittent failure and a single green run
proves nothing. The known flake is `ConnectionRefused` at
`robustness.rs:138` under full-suite parallelism.

**Risk:** low for production, moderate for the suite. **Rollback:** the old
helpers stay until the last file is migrated.

**Bonus:** this is the prerequisite for `m6-backend-examples.md`, whose shared
assertion set needs exactly this harness.

---

## Phase 3 — Semantics

Version-independent rules. Moves without touching any wire format.

**3.1 `validate_request_header_bytes` → `m6-core`.** Currently in `http2.rs`,
imported by the H3 path. This is the single symbol h2 and h3 share; once it is
in core they keep sharing it with neither wire format moving.

**3.2 Conditional requests and preconditions → `m6-core`.** The full RFC 9110
13.2.2 precedence with weak comparison, from `m6-http::cache`. Then **fix
`m6-file`**, which has its own inline version missing weak comparison and two
of the four precedence steps.

This one has a live defect to prove the move worked. Confirmed on production:
`If-None-Match: W/"6a9c8db4-155f1"` against `/assets/css/style.css?v=…`
returns **200 with 46,775 bytes** where RFC 9110 8.8.3.2 requires 304.

**3.3 Caching semantics → `m6-core`.** Storability, freshness, age, request and
response directives. The *rules*. Cache storage and eviction stay in `m6-http`.

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

| Phase | Changes the wire | Risk | Blocks |
|---|---|---|---|
| 0 Prerequisites | no | none | everything |
| 1 Small consolidations | no | low | — |
| 2 Testkit | no | low | 3, 4, 5, 8 |
| 3 Semantics | no (fixes one bug) | moderate | 4 |
| 4 HTTP/1.1 | yes | moderate-high | 5 |
| 5 Service loop | no | high (size) | 6 |
| 6 Consumer apps | no | low | 7, 8 |
| 7 Decouple repos | no | low | — |
| 8 Backend examples | no | low | — |

Phases 1 and 2 can start immediately and are worth doing regardless of whether
the rest proceeds: Phase 1 closes a security divergence, and Phase 2 probably
closes the open test flake.

**Stopping early is a valid outcome.** Phases 1 through 3 leave the codebase
better with no architectural commitment. The commitment starts at Phase 5.
