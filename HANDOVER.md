# Handover

State of play for the next session. Written 2026-09-11, updated later the
same day after Phase 5 landed.

---

## 1. Where the work is

**Branch `main`, clean, 51 commits ahead of the deployed `b32e837`, 29 not yet
pushed. Nothing is deployed. The freeze holds until the whole migration is
finished.**

- 862 workspace tests pass at default features, 871 with `--all-features`.
  Zero warnings. The count went 863 -> 862 because a duplicate
  `socket_path_from_config` test went with the duplicate function.
- h1spec **32/32 on all four HTTP/1.1 targets**, with a CI ratchet
  (`tools/conformance.sh`, floors in `tools/conformance-scores.txt`) wired into
  `check.sh` as a blocking gate.
- h2spec and h3spec are **not installed on the laptop** and have skipped on
  every local run. They must run on the build box before deploy.

### Migration status (`docs/m6-core-implementation-plan.md`)

| phase | what | status |
|---|---|---|
| 0 | Prerequisites | done |
| 1 | Small consolidations | done |
| 2 | `m6_core::testkit` | done |
| 3 | Semantics | done, 3.3 dropped |
| 4 | HTTP/1.1 | done, 32/32 |
| **5** | **Service loop** | **done** |
| 6 | Consumer apps link `m6-core` only | **next** |
| 7 | Decouple the repositories | not started |
| 8 | Backend examples | not started |

---

## 2. Standing constraints — these do not lapse

- **Never use em dashes** in prose written for the owner.
- **Zero compiler warnings**, pre-existing included, checked on Linux.
- **Test locally, commit, then deploy. Never deploy from an uncommitted tree.**
- **Secrets never enter git.** Only `.example` files, paths, documented shape.
  Values live on the boxes.
- Any change to **layout, copy, or rendering** needs individual approval before
  it ships.
- Image resizing is the owner's job.
- Firewall blocks are **per-IP only**, no CIDR rules.
- Crawler sightings are reported **explicitly, every run**, even a quiet one.
- The build box is **not backed up**. Everything done to a node is captured in
  git.
- Keep dynamic allocations to an absolute minimum. **Latency is the key
  metric.**
- "Clean and consistent is the only way forwards. Apps should deviate only
  where functionality demands it."
- "Clean simple code with lots of reuse out of core. This is not the place to
  get clever or inventive."

---

## 3. What Phase 4 produced

**Five HTTP/1.1 implementations became one**, `m6-core/src/h1.rs`. All four
conformance targets (m6-file, m6-html, m6-auth-server, m6-http-redirect) went
27 → 30 → 32 out of 32.

New in `m6-core`:

| item | file | what |
|---|---|---|
| `h1::parse_request` | `h1.rs` | The one HTTP/1.1 parser. Pure. |
| `h1::Responder` | `h1.rs` | The one response writer. Owns HEAD suppression and `Connection`. |
| `h1::Expectation`, `expectation()` | `h1.rs` | `Expect: 100-continue` (RFC 9110 10.1.1) |
| `h1::keep_alive`, `status_reason` | `h1.rs` | Persistence decision, reason phrases |
| `server::serve_connection` | `server.rs` | The one backend connection loop |
| `testkit::read_one` | `testkit/response.rs` | Read exactly one response, method-aware |
| `ForwardedTrust` etc. | `m6-http/src/forward.rs` | Trusted client-address attribution |

**Deleted:** four serialisers (m6-file's three writers, m6-render's
`Response::write_to`, the one inside `RawResponse::to_bytes`), m6-render's
`handle_connection`, m6-http's second HTTP/1.1 implementation in `redirect.rs`.

### Bugs found and fixed along the way, all awaiting the same deploy

1. **SIGTERM could not stop the `:80` redirect listener.** `main` blocks the
   shutdown signals so only core's `sigwait` thread sees them; redirect mode
   returned before installing that thread. Only SIGKILL worked. Guarded by
   `m6-http/tests/redirect_lifecycle.rs`.
2. **The `:80` redirect dropped every query string.** `http://host/x?v=1` went
   to `https://host/x`.
3. **Backends answered a HEAD with a body** on every error path, and never kept
   a connection open, so every cache miss paid a fresh connect.
4. **`OPTIONS *` and `CONNECT` were answered 400** by the redirect listener.
5. **Origin attributed every relayed request to the WireGuard tunnel**, so all
   traffic through one cache node shared a single 300/min rate-limit bucket.
6. **`conformance.sh --update` deleted the floors it had not measured.**

---

## 4. Immediate next steps, in order

1. **Phase 6.** `render-contact`, `render-analytics` and `render-cms` switch
   from `m6-render` to `m6-core` and construct `App::new(..)` with the
   renderer they actually want, which for all three is
   `m6_core::render::NoTemplates` (none of them has a template file). Then
   `m6-render/src/{app,config,error,multipart,request,response,server}.rs`
   and its `util` shim are deleted; only `template.rs` is left, and Phase 0.1
   says that becomes `m6-html`.
2. **Compile the watcher fix on the build box.** `m6-core/src/watcher.rs` is
   inside `#[cfg(target_os = "linux")]` and no Linux target is installed on
   the laptop, so the inotify alignment fix in `8bcebac` has never been
   through a compiler. It is three lines and a const assertion, and it is
   still unverified.
3. **HTTP Garden** (arxiv 2405.17737), the coverage-guided differential fuzzer.
   Outstanding from "do them both". Needs Docker, which is not on the laptop, so
   it runs on the build box.
4. **h2spec and h3spec on the build box** before any deploy.
5. **Re-derive the cache-hit latency baseline.** See §6.
6. **Item 11**, normalise the cache key to `{identity, gzip, br}`. Deferred; has
   an encoding-agreement hazard.
7. **Benchmark Phase 5**, paired and interleaved, against the fixed baseline.
   The plan requires every phase to report a delta and Phase 5 has not. It
   moved the whole request path between crates and added one virtual call per
   templated response, so a number is owed even if it is a flat one.

---

## 5. The hourly health check

**Read `docs/health-check.md` before running it.** The pasted prompt is written
in syd terms and is not sufficient on its own.

Three nodes, all checked every run:

| node | role | service | WireGuard | analytics file |
|---|---|---|---|---|
| syd | origin | `m6-http-origin` | 10.0.0.1 | `/var/www/dr-grosvenor-site/logs/analytics.ndjson` |
| lon | cache | `m6-http-cache` | 10.0.0.4 | `/var/www/m6-cache/logs/analytics.ndjson` |
| chi | cache | `m6-http-cache` | 10.0.0.5 | `/var/www/m6-cache/logs/analytics.ndjson` |

Anything a cache node serves from its own cache never reaches origin, so a
syd-only reading is **biased, not partial**.

---

## 5a. What Phase 5 produced

`m6-render` went from ~4,900 lines to 904, of which 754 is `template.rs`.
`app.rs` (2,651 lines), `request.rs`, `response.rs`, `config.rs`,
`server.rs`, `util.rs`, `error.rs` and `multipart.rs` are all in `m6-core`.

The seam is `m6-core/src/render.rs`: `Renderer`, `RendererFactory`,
`RenderError` and `NoTemplates`. Core routes, builds the dictionary,
compresses and writes; it does not render and links no template engine.
`m6-html` supplies `TeraRenderer`/`TeraFactory`.

Three things worth knowing:

- **`{{ not_found() }}` is typed now.** It used to fail the render with the
  magic string `__M6_NOT_FOUND__` and the service loop recovered the intent
  with `msg.contains(..)`. Tera still needs the sentinel internally because
  its errors are strings, but it converts at the boundary.
- **chrono and lru are unconditional deps of `m6-core`.** Gating them behind
  an `app` feature kept them out of m6-file, m6-http and m6-auth-server but
  stopped a default `cargo test --workspace` from compiling the 97 tests that
  came with `app.rs`. Owner chose the dependency over the blind spot.
- **`m6_render::App` is a unit struct, not a re-export.** Core's
  `App::new()` takes a renderer; the shim's four constructors hand it
  `TeraFactory` so the site renderers did not have to change. Phase 6
  deletes it.

Also fixed along the way, and also awaiting the same deploy:

7. **`/health` was publishing the internal topology.** Every pool by name
   (`m6-html`, `m6-file`, `render-contact`, `render-analytics`) with worker
   counts, plus `uptime_s` and `url_backends`, on an unauthenticated public
   URL. It is `{"status","node"}` now; the detail moved to token-gated
   `/perf`. The test that should have caught it checked an allowlist that
   the leaked fields were on, so it passed for as long as the leak existed.
8. **The inotify read buffer was unaligned.** `read_events` cast offsets in a
   `[u8; 4096]` straight to `*const libc::inotify_event` and dereferenced
   them. That struct needs 4-byte alignment and a byte array has 1, so it was
   undefined behaviour that happened to work. Owner spotted it. Not compiled
   yet, see step 2 above.

---

## 6. Open questions, honestly unresolved

- **The cache-hit p50 baseline is unverified.** Recorded as 1.7-2.2us; syd has
  measured a flat ~3.3us across 299 windows in 24 hours on an idle box. Either
  the band was derived differently or the drift predates the visible window.
  Re-derive before treating a miss as an incident.
- **Per-node cache hit rates need re-measuring** now that all three analytics
  files are readable. The 24h journal figures were syd 0.57, lon 0.16, chi 0.21.
  An earlier reconciliation looked wrong (chi reporting ~300 misses/hour while
  origin logged ~10 requests from chi) but that was probably my own error:
  I was reconciling chi's counters against origin's log, and origin never sees
  what chi answers from cache. Confirm from chi's own file.
- **`redirect_lifecycle::sigterm_shuts_down_rather_than_being_ignored` is
  flaky.** Failed once inside a full `--all-features --test-threads=1`
  workspace run. Did not reproduce in eight isolated runs, twelve concurrent
  runs, or a second full-suite run, and I never captured the assertion text,
  so the cause is unknown. Ruled out: the obvious startup race, because
  `redirect::run` installs `ShutdownHandle` *before* `bind_plain`, so a
  successful `wait_for_tcp` already implies the signal thread exists. This
  guards a bug that shipped, so a flaky version of it is worth a real
  diagnosis rather than a retry.
- **`149.28.160.27`** has appeared for four consecutive hours, presenting its own
  IP as TLS SNI (rejected by rustls) and as a spoofed `Referer`. Low rate,
  getting nothing. Companions: `108.61.197.71`, `66.42.119.239`.

---

## 7. Lessons that cost something to learn

These are in the plan too, but they are the ones worth carrying.

1. **Measure the candidate before consolidating onto it.** The plan said to
   consolidate the HTTP/1.1 parsers onto `m6-core/src/parse.rs`. Measured, that
   was the *worst* of the four at 14/32. The survivor is the one the edge
   already used, because it is the only one that has been attacked.
2. **A test can pin wrong behaviour as firmly as right behaviour.**
   `empty_body_is_unchanged` asserted `content-length: 0` on a 204 and passed
   for as long as the violation existed. `finding_7b` asserted that a chunked
   body must be *rejected*. Both had to be rewritten to state the property
   rather than the current output.
3. **A number measured from traffic you generated is not a production number.**
   Warming seven pages and hammering them reported `cache_hit_rate=1.0000` every
   hour while the real edge ran at 0.16 to 0.21.
4. **An absent file at the path you expected is not evidence the feature is
   off.** I reported "the cache nodes record no analytics" as a finding. They
   had 12 MB and 45 MB of it, at a different path. Check where the process
   actually writes.
5. **Conformance harnesses produce fake scores.** Three scores in this work were
   artefacts: a bridge closing both directions on EOF (11/32), a bridge
   forwarding half-close onto a TLS socket (6/32), and measuring a leftover
   process that still held the port (5/32, 8/32). `bridge_sanity`,
   `require_free_port` and `wait_port_owned_by` exist because of those.
6. **Kill by PID. Never `pkill -f <pattern>` naming a port or config path** —
   over ssh the command line contains that string too, so it matches its own
   session. That has killed the connection three times.
7. **Benchmark paired and interleaved, against a fixed baseline, not the
   previous commit.** The build host runs a full m6 stack on 4 cores; the same
   commit measured 320.66 and 366.24 ns. Also: `--sample-size` on the CLI is
   silently overridden by `group.sample_size()` in the bench source.
8. **Do not hand-roll a conformance tester.** The implementation writing its own
   tester encodes the same misreading of the RFC twice and passes.
9. **Derive a security boundary structurally, not from config.** Forwarded-address
   trust comes from the bind address via `Iface::for_bind`, so there is no key to
   set wrong and no peer list to keep in step. A config reload is the thing that
   silences logging on this fleet; a trust flag that a reload could get wrong is
   a boundary that moves at deploy time.
10. **Safe by default, opt in explicitly.** `Http2Conn::new()` trusts nothing;
    the backbone listener calls `.trusting_forwarded_for()`. A connection built
    any other way is safe by construction rather than by remembering.
11. **Blocking a signal without installing a handler makes a process
    unkillable.** Found because a conformance run could not reclaim its port.
12. **Silently swallowing a parse error looks like health.** m6-file and
    m6-auth-server both did `parse_request(...)?` into a caller that only
    logged, so every malformed request got a silent close. Invisible while the
    parser was lenient; the moment it got stricter the conformance score went
    *down* while the code underneath got better.
