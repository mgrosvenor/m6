# Session records

Point-in-time notes from the sessions that built this. **Claims here were true
when written and are not maintained.** `HANDOVER.md` is what is true now; where
they disagree, it wins.

Kept because the reasoning in them is often the only record of why something is
shaped the way it is, and because several of the defects listed were invisible
in the code.

Moved out of `HANDOVER.md` on 2026-09-13.

---

## 6b. Session of 2026-09-12, later

> **Point-in-time record, not current state.** Claims here were true when
> written. §1 and §4 are what is true now; where they disagree, they win.

**Nothing deployed. Freeze intact.** 1002 tests, zero warnings, clippy at its
ceiling, verified on Linux.

### Consolidation: §3b-now finished, and most of what sat behind it

| item | commit | note |
|---|---|---|
| `App` read timeout | `d52a51b` | `[server] read_timeout_s`, default 30 |
| Socket mode | `4fc33da` | `[server] socket_mode`, default `0660` |
| HEAD without opening the file | `a094851`, `7932e43` | plus the directory regression it introduced |
| poll(2) block into core | `cf84546` | only the poll block; the two concurrency models stay apart |
| m6-http header sweep | | 25 sites, not "about a dozen"; none left |
| ConfigWatcher tests + macOS rewrite | `e6ba278` | single pollable kqueue, no threads |
| `{*name}` wildcard routes | | explicit, not implicit |
| Streaming response bodies | | `send_stream`, used by m6-file |
| `SO_REUSEADDR` | `07f11d8` | |
| m6-md date arithmetic | | a real bug, below |

### Three live bugs found, none of them the thing being worked on

- **m6-md reported a quarter of all dates wrong.** Hand-rolled
  civil-from-days, era anchored at 1970 instead of shifted to March. Measured:
  7,281 of 29,200 days over 1970-2050. The last day of every leap year became
  the first of the next, and the whole following year was a day late. Every
  date in 2025 was wrong; it is correct today, which is why nobody saw it, and
  it would have resumed on 2028-12-31. Now `m6_core::util::iso_date_from`.
- **A HEAD on a directory answered 200.** Introduced by the HEAD fast path in
  the same session and caught by the Linux gate: `std::fs::metadata` succeeds
  on a directory, and the `fs::read` the fast path skipped was also what had
  been rejecting non-files. `is_file()` now guards it.
- **m6-http can run with nothing listening on 443.** A failed bind is a
  warning. Tracked as §3d, not changed under the freeze.

### Five intermittent test failures, all diagnosed, none written off

The word "flake" was wrong. Each was reproduced deliberately before being
fixed. **One of the two long-standing named ones is solved**:
`redirect_lifecycle::sigterm_...` stored the shutdown flag *before* logging the
line explaining it, so the main thread could drain, log "shutdown complete" and
exit while that line sat in `tracing_appender`'s queue. The rest were four test
helpers that panicked on a transport error inside the retry loop written to
tolerate it, two unit tests racing over the global `SHUTDOWN_FLAG`, a
wall-clock cache-hit assertion, and the `TIME_WAIT` port race that
`SO_REUSEADDR` fixed.

**`m6-auth-cli`'s `test_token_create_prints_jwt` is the only one left** and did
not recur.

### The tooling changed under you

- **Clippy is a gate**, on the owner's instruction. `tools/clippy.sh`, in
  `check.sh` step 2 and in the Linux gate. Per platform, and the count may fall and may never rise, and a
  clippy *error* fails regardless of the ceiling.
- **`health-check.py` missed self-identifying bots four separate ways** and now
  has `--self-test`. `bot\b` does not match `OnlineOrNot.com_bot_1.0`; a
  `+https://...` in a UA is a bot convention and was not matched at all;
  probe and injection paths were matched **raw**, so `%2e%2e%2f` was never
  `../`; and a crawler that only polls `/health` was filtered out before
  detection because those rows are `message = "monitor"`. Scanners are now
  reported apart from crawlers. Fixing the `+URL` case alone surfaced three
  crawlers that had never been reported.
- **The Linux gate returns the whole `failures:` section** instead of a bare
  `panicked at <file>:<line>` with the reason stripped off.

### If you read one thing about method

Run the suite as `cargo test --workspace > /tmp/run.txt 2>&1` and grep the
file, never the pipe. Two of today's diagnoses came from captured output that
earlier sessions had lost to a re-run, and one of those had been unexplained
for weeks.

---

## 6a. Session of 2026-09-12

> **Point-in-time record, not current state.** Claims here were true when
> written. §1 and §4 are what is true now; where they disagree, they win.

Eight commits, `588daca` to `0a40584`. Docs and small core changes; **nothing
deployed, freeze intact**.

### The Linux build host works, and closed the oldest unverified item

`deploy/run-tests.sh` rsyncs both trees to `root@<build-host>` **port 4022** and
enforces zero warnings on Linux. **It needs no push, only a committed tree.**
Result: **m6 962 passed, renderers 9 passed, 0 failed, 0 warnings** on release
and test builds, same test count as macOS.

- **The inotify alignment fix compiled for the first time**, and
  `const _: () = assert!(align_of::<libc::inotify_event>() <= 8)` was
  *evaluated* and holds. That premise was untested until now.
- **New gap found doing it: `ConfigWatcher` has no tests on any platform.**
  `tests/log_reload.rs` covers `LogHandle::reload`, not the watcher. Config hot
  reload has never been exercised. "Never compiled" is closed; "never
  exercised" is not.
- Box is Linux, 4 cores, rustc 1.98.0, **h2spec and h3spec installed**, **no Go
  and no Docker** (so Phase 8 and HTTP Garden need a toolchain install).
- It is also **staging**, and staging is a **single origin**: it cannot
  exercise the cache role, which is the role the 226/NAMESPACE outage hit.

### Conformance: h2 verified, h3 gate is broken

**h2spec 146/146 on Linux**, matching baseline. h3spec produced **no score and
the script reported PASS**, which is two bugs in `tools/conformance.sh`:

- `run_h3` never calls `start_edge`; `run_h2` does. Standalone
  `conformance.sh h3` tests a port with nothing on it.
- It only records a score `if got > 0`, so measuring nothing reports PASS.
- Separately, `MEASURED="$WORK/measured.txt"` is truncated at script init but
  `mkdir -p "$WORK"` runs ~330 lines later, so on a fresh box the minimum-score check's
  bookkeeping silently fails to write.

There are **no h2 or h3 floors** in `conformance-scores.txt`, only the four h1
targets.

### Core changes

- **`testkit::assert_app_lifecycle`**: the whole lifecycle contract in one call
  (exit status is success not a signal, socket removed, three lines logged
  under the right name). It was opt-in and hand-copied into five suites, which
  is why the sixth service never got one.
- **`m6-monitor/tests/lifecycle.rs`**: it had no `tests/` directory at all. 17
  unit tests and nothing had ever started the binary. It passes, so it was
  always structurally right and merely unproven.
- **`socket_path_from_config` consolidated, four copies to one.** The
  derivation existed in `m6-core/src/server.rs` *and* `m6-file/src/config.rs`
  with a different fallback stem, and m6-file called its own; the
  `M6_SOCKET_OVERRIDE` wrapper was then duplicated in three mains. The override
  now lives inside core's function.

### New documents

- **`docs/m6-core-reference.md`**: all 30 modules and their interfaces.
- **`docs/m6-app-anatomy.md`**: the app shape, written to be followed from
  another repo.
- **`docs/m6-app-shape-plan.md`**: the single-threaded target, the evidence,
  and the deferral.
- `m6-render-lib.md` marked SUPERSEDED, `m6-core.md` §9 marked historical.

### Production facts learned

- **syd is 1 core and 950MB.** `m6-file` runs **32** workers on it; `m6-html`,
  which renders every HTML page at ~6ms, runs **1** (no `size` line). The 32
  was raised reactively after the gallery exhausted the default.
- **The health check tool has a bug**: over windows longer than ~60 minutes the
  UA-rotation heuristic flags the backbone addresses `192.0.2.4` (lon) and
  `192.0.2.5` (chi) as forging bot UAs, because a cache node relays real
  clients' agents. It then **excludes those requests from crawler counts**, so
  wide-window crawler totals are understated. Not yet fixed.

---

## 6. What this session produced

> **Point-in-time record, not current state.** Claims here were true when
> written. §1 and §4 are what is true now; where they disagree, they win.

### Phases 5 and 6: m6-render is gone

`app.rs` (2,651 lines), `request`, `response`, `config`, `server`, `util`,
`error`, `multipart` and `template` all moved into core. m6-render deleted.

The render seam is `m6_core::render`: `Renderer`, `RendererFactory`,
`RenderError`, `NoTemplates`. Core routes, builds the dictionary, compresses
and writes; it does not depend on Tera at the type level, only by default.
`App::new()` gives you Tera; `.renderer(NoTemplates)` opts out.

**`{{ not_found() }}` is typed now.** It used to fail the render with the magic
string `__M6_NOT_FOUND__` and the loop recovered the intent with
`msg.contains(..)`.

**The plan's justification for Phase 6 was wrong**, and it is worth not
repeating: "three of four consumers have zero template files yet link Tera" is
true about files and false about need. All three render templates out of the
*site* directory and use `| asset` and `img_dims()`. Giving them `NoTemplates`
would have compiled and 500'd every page.

### New in core

`ndjson`, `telemetry`, `host`, `monitoring`, `cookie`, `headers`. See §3.

`AnalyticsRecord` is the definition the analytics format never had: m6-http
emits rows through `tracing::info!` so the JSON shape was whatever the
subscriber produced, and every consumer re-derived it by squinting at a sample.

### `m6-monitor`, new service

Polls every node's `/health` and `/perf`, serves `/` as a page and `/digest` as
JSON. Runs on the **build host, outside the fleet**, because a monitor inside
the fleet cannot report that the fleet is down. `deploy/FLEET-MONITOR.md` in
the site repo is the runbook; `deploy/ORIGIN-NODE.md` is what is origin-only.

**The WireGuard mesh is not a path to those endpoints.** Measured: origin's
`192.0.2.1:80` is h2c-only and does not answer HTTP/1.1 at all, and the cache
nodes have *no* backbone listener. It polls the per-node public names instead,
which means it measures through each node's public edge and is **not**
comparable with the loopback TTFB.

### Bugs fixed, all awaiting the same deploy

Carried forward from Phase 4: the `:80` redirect SIGTERM bug, the dropped query
string, HEAD-with-a-body on error paths, `OPTIONS *`/`CONNECT` 400s, the
WireGuard client-IP attribution, `conformance.sh --update` deleting floors.

New this session:

7. **`/health` was publishing the internal topology.** Every backend pool by
   name with worker counts, plus `uptime_s` and `url_backends`, on an
   unauthenticated public URL. Now `{"status","node"}`; the detail moved to
   token-gated `/perf`. The test that should have caught it checked an
   allowlist that the leaked fields were *on*.
8. **The inotify read buffer was unaligned.** Cast a `[u8; 4096]` straight to
   `*const libc::inotify_event` and dereferenced it. Undefined behaviour that
   happened to work. **Still never compiled.**
9. **`looks_like_injection` matched the raw path**, so `UNION%20SELECT` never
   matched `union select`. SQLi in a URL is always percent-encoded, so the
   detector read clean against exactly the traffic it exists to catch.
10. **Two stale `csrf` tests** had not compiled since Phase 4. `run-tests.sh`
    runs default features, `csrf` is not one, so nothing ever built them.
11. **`check-templates.sh` gated on a log line that no longer exists**
    (`"m6-render started"`, now `"routes loaded"`), so the template gate
    reported FAILED on a clean start. Run by hand, which is why nobody noticed.

### Site repo

The container gutter asymmetry (`#curriculum-value` set `padding-left` and
never `padding-right`, 7px against 25px at ≤768px, experience page only), the
1144px max-width, `style.css` normalised to LF with a `.gitattributes`, and the
renderers switched to m6-core.

---

## 7. Open questions, honestly unresolved

- **THE CACHE-HIT p50 HAS DOUBLED IN SIX DAYS AND NOBODY HAS INVESTIGATED IT.**
  **Corrected 2026-09-12. This entry previously said the baseline was
  "unverified" and told the reader to stop flagging it. That was wrong, and the
  reasoning behind it was backwards.**

  The 1.7-2.2us band is not a guess and was not derived "some other way". It is
  a recorded production measurement in `the deployment repository/docs/RELEASES.md`,
  taken with **the same instrument, on the same node, by the same method** as
  every reading since: m6's own `hit_p50_ns`, over loaded windows on the origin.

  | date | deploy | method | p50 | p99 |
  |---|---|---|---|---|
  | 2026-09-06 | s-maxage verify | 2 loaded windows, 41 + 29 hits | **1.7us** | 2.0us |
  | 2026-09-10 | Rapid Reset `438bdb3` | loaded window | 2.5us | - |
  | 2026-09-10 | flow control `b32e837` | loaded window | 2.95us | 3.55us |
  | 2026-09-12 | `22ee3a4` | loaded window, 76 hits | **3.61us** | 4.52us |
  | 2026-09-12 | `22ee3a4` | 24h, 268 windows | 3.90us | 4.20us |

  **2.1x slower in six days, monotonic across four deploys**, in the metric the
  owner has named as the key one. The 24h figure over 268 windows corroborates
  the loaded reading, so it is not a single bad sample.

  Ruled out on 2026-09-12:
  - **An accounting change.** `22ee3a4` was "monitoring accounted separately",
    which is exactly the shape that fakes a regression. It is not: monitor
    polls were excluded from the stats counters before and are still absent
    from `hit_p50_ns` now. Same population either way.
  - **Machine pressure.** syd measured idle at the time: load 0.08 on one core,
    601MB available of 950, 109MB swap allocated with zero swap-in/out while
    sampling, m6-http RSS 47MB.
  - **Unbounded growth.** m6-http had been up 15 hours, not weeks.

  **Not yet known: which change did it, or whether it is code at all.** Two
  hypotheses, neither measured: Rapid Reset added per-stream accounting, and
  `22ee3a4` replaced a substring match with real q-value parsing on every
  request. Against that, the *same binary* read 2.95us on 2026-09-10 and
  3.6-3.9us today, which is a rise with no code change, so part of this may be
  environmental in a way an idle snapshot does not capture.

  **How to settle it:** a paired, interleaved A/B on the build box across
  `084f89e`, `438bdb3`, `b32e837`, `22ee3a4` and HEAD, one load, one host,
  medians of five. That is lesson 7, and it is the same work as the owed
  "benchmark Phases 5 and 6" in item 8.

  **How this got lost is the part to carry.** Three consecutive sessions saw
  the deviation and each concluded the *baseline* was wrong rather than the
  server, on the reasoning "we keep measuring ~3us, so 1.7-2.2 must be wrong".
  That is backwards: if a regression lands before your first reading, every
  reading after it agrees with every other one. Consistency is not correctness.
  It was then written into two handovers as settled, with an instruction to
  stop reporting it.
- **24h hit rates**: syd ~0.58, lon ~0.16-0.19, chi ~0.19-0.21. Stable across
  the day. The earlier reconciliation that looked wrong was my own error:
  origin never sees what a cache node answers from its own cache.
- ~~**Two tests are flaky, and they share a shape.**~~ **LARGELY RESOLVED
  2026-09-12, and "flaky" was the wrong word for all of it.** Five distinct
  intermittent failures were run to ground in one session. Every one had a
  specific mechanism, and every one was reproduced deliberately before being
  fixed. Nothing was written off.

  - **Four helpers panicked inside a readiness loop.** Every e2e suite waits
    with `wait::until(30s, || <request>.status == 200)` for m6-http to fill its
    backend pool, and four of the request helpers `.unwrap()`ed a transport
    error, so the first hiccup ended the run instead of being retried. Two
    unwrapped `write_all` (a server that accepts then closes sends RST, so the
    client learns on its next write, which for TLS is the handshake); two
    panicked on a refused connect on the stated premise that a refusal means
    the service died, while calling `assert_alive` first and printing
    `failed with m6-http alive` when it did not. Reproduced deterministically
    with a listener that accepts and drops, matching the original error exactly.
  - **Two unit tests raced over a global `static`.** `SHUTDOWN_FLAG` is one
    `AtomicBool` for the process; one test stores `true`, another asserts
    `false`, and `cargo test` runs them on different threads. It failed once in
    a full run and never in 300 runs of that module alone, because the window
    is two atomic stores wide. Widening each side by 50 ms reproduces it every
    time. Fixed with a mutex, verified under those same widened windows.
  - **A wall-clock assertion.** `test_static_file_cache_hit` closed with
    `hit_latency < 5ms`. It is the example the site handover's traps already
    used, and it duly fired on the loaded Linux build box against correct code.
    Now asserts `Age`, measured on the wire first: a miss carries no `age`
    header, a hit carries one.

  **One of the two originally named is now understood and fixed.**
  `redirect_lifecycle::sigterm_shuts_down_rather_than_being_ignored` was an
  ordering bug in `signal.rs`, caught on 2026-09-12 by finally capturing the
  output: the journal had `started`, the listener line and `shutdown complete`,
  and nothing in between. The signal thread did
  `SHUTDOWN_FLAG.store(true)` and *then* logged. The flag is what releases the
  main thread, which drains, logs "shutdown complete" and returns from `main`,
  and process exit discards whatever is still queued in `tracing_appender`'s
  non-blocking writer. So the line was racing the whole drain, and the redirect
  listener runs start-to-complete in ~20ms, which is why only a loaded machine
  lost it. Logging before the store makes the two lines ordered rather than
  concurrent. Fixed; 6/6 in isolation and clean in a full run.

  **The remaining intermittent failure is a port race, and it is not the
  m6-auth-cli one either.** Seen three times on 2026-09-12 across
  `robustness.rs` and `analytics_e2e.rs`, always as
  `HTTP/1.1 TCP listener bind failed ... Address already in use (os error 48)`
  followed by "m6-http never served a backend request". `claim_port` hands out
  a port after verifying it binds, then drops that listener; the service binds
  some milliseconds later, after cert generation and config writing. Something
  takes the port in between. **Not yet diagnosed**, and the candidates are
  TIME_WAIT from a previous test's client connections on the same port, and a
  `PortClaim` released while its service process is still exiting. Worth noting
  m6-http does not set `SO_REUSEADDR`.

  **`m6-auth-cli`'s `test_token_create_prints_jwt` is still not understood**
  and did not recur: `redirect_lifecycle::sigterm_shuts_down_rather_than_being_ignored`
  and `m6-auth-cli`'s `test_token_create_prints_jwt`. What has changed is that
  the next one will be readable rather than lost. **Run the suite as
  `cargo test --workspace > /tmp/run.txt 2>&1` and grep the file, never the
  pipe**, and the Linux gate now returns the whole `failures:` section instead
  of a bare `panicked at <file>:<line>` with the reason stripped off.

- ~~**`185.19.40.146` is a block candidate**~~ **BLOCKED 2026-09-12**, with
  four others. Ledger and all three nodes at 31 rules, in sync.
  **The open question it leaves is the campaign, not the address.** One
  `//xmlrpc.php` operator ran an identical signature from three addresses,
  rotating *and returning*: `185.19.40.146` (06:34, 10:24, 11:48, 12:32, 19:06,
  20:10), `34.24.203.92` (14:47), `35.237.17.157` (15:14), the latter two both
  GCP. Per-IP blocking slows it and will need topping up. **The structural
  answers are in `BLOCKLIST.md`'s own closing section and neither exists:
  negative caching, and a 404-rate throttle.** Negative caching is the bigger
  prize: every 404 is uncacheable today, so on a cache node each junk request
  crosses the Pacific and back at ~207ms.
- **`45.142.193.161` was deliberately NOT blocked.** Top firewall-drop source
  on all three nodes for eight hours, ~1,430 packets, but it never reaches the
  application: it scans closed ports and the default deny already drops it. An
  explicit rule would change nothing and imply the application had been
  touched.
- **A coordinated probe hit chi** at 05:47-05:50 UTC: `34.91.241.0` (GCP), 890
  requests in under three minutes rotating 526 user agents, targeting SSRF
  (`/fetch`, `/proxy`), cloud credentials (`.aws`, `.azure`, gcloud ADC) and
  LFI (`/@fs/...`). It got 795 404s, 85 429s and ten 200s, all of them `/`.
  Nothing sensitive served. Candidate for a per-IP block; not blocked, because
  the check is read-only.

---

