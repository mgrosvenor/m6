# Handover

State of play for the next session. Written 2026-09-11, updated 2026-09-12.

> ## Read first, 2026-09-12 (latest session)
>
> **`App` has config-driven routes now, which was the owner's
> *"dynamicly reload the file list"*.** A handler is registered once by name
> (`App::handler("files", f)`), a route names it in config
> (`handler = "files"`), and config routes are rebuilt on every reload, so
> adding an asset tree is a config edit rather than a restart. Keys core does
> not define (`root`, `tail`) are kept for the handler to read. An unregistered
> handler name is fatal: exit 2 at startup, and a reload refused with the
> previous routes still serving. Detail in `CONSOLIDATION-TODO.md` §6.
> **m6-file is not migrated yet**, and the reason changed during the session;
> see below.
>
> **Wildcard routing was marked DONE and did not work.** Found doing the above.
> Any `{*name}` capture spanning more than one segment was answered **400**,
> which is every use it exists for: `build_dict` validated it with
> `allow_slash = false`, on a premise that was true when written and that
> `Segment::Wildcard` made false. All six of its tests stopped at the matcher.
> Fixed, with the two halves separated as `validate_wildcard_param`, and
> traversal still refused. **Lesson 30 again: the matcher is not the wire.**
>
> **Code routes were emitting a `Last-Modified` they had no claim to**, the
> newest template's mtime for an answer computed per request. The comment at
> the emit site already said code routes were skipped; the loop did not skip
> them. Fixed.
>
> **`Response` can stream now**, and `Response::verbatim()` stops core
> re-encoding a representation a handler already negotiated. `Body` is a sum
> type, so a stream structurally has no bytes for the minifier, the compressor
> or the default ETag hash to touch. The ledger's "streaming never blocked
> m6-file" was written at 14:32 on 2026-09-12 and `send_stream` reached
> m6-file at 14:55 the same day; the two were never reconciled.
>
> **m6-file is still not migrated, and measuring `App` first found something
> bigger: `App` deep-copies its immutable state on every request.** Tracked as
> `CONSOLIDATION-TODO.md` §7.
>
> `build_dict` starts from an empty map and copies the whole static config in,
> per request. With the site's real `data/content.json` (68KB, 1,364 nodes),
> which production declares as **both** the global params and the route's
> params, and which `render_response` then clones a third time:
> **~323us of pure copying per HTML page before Tera is called**, on a laptop.
> syd is 1 core and renders every page at ~6ms. Nothing in it varies between
> requests.
>
> The first number I reported was 3.08us from a synthetic 20-key config, and
> the owner's response was that 3us sounded wrong for building a small map.
> It was: the parser and the map are noise (41 to 583ns), the copy is
> everything, and against the real config it is seventy times larger.
>
> **The owner then asked for a full copy audit of the system, with the target
> stated as zero and config to be a read-only reference throughout. It is
> `CONSOLIDATION-TODO.md` §7a.** Result: **`m6-http` and `m6-file` are already
> right** (`Arc<Vec<_>>` headers and `bytes::Bytes` bodies at the edge, `Arc`
> config and routes in m6-file), and **`App`, the framework both are meant to
> migrate onto, is the only offender**, at **~0.63ms of pure copying per HTML
> page**. `content.json` is deep-copied **six times per request**. The route to
> zero is listed there in the order that pays; the floor is Tera's own context
> build (189us), which core cannot remove without changing the renderer seam.
>
> **This is not §3a.** That is m6-http's cache-hit p50 and never touches
> m6-html. Do not conflate them.
>

> ## Read first, 2026-09-12 (late session)
>
> **The scope note on §3b was wrong and the owner corrected it twice. Read the
> corrected one before planning anything.** Deferred is **the event loop and
> the handler contract, only**; the IO layer is in scope as "arguably low touch
> consolidation work"; wildcard routing, streaming bodies and **both
> migrations** are live work and were never deferred. Verbatim in
> `docs/CONSOLIDATION-TODO.md` §3b. Do not widen it again.
>
> **§3b-now is finished, and so is most of what sat behind it.** Read timeout,
> socket mode, HEAD fast path, poll block, header sweep, wildcard routing,
> streaming bodies, ConfigWatcher tests. See §6b.
>
> **The cache-hit p50 has doubled in six days and it is not a bad baseline.**
> 1.7us on 2026-09-06 to 3.9-4.0us now, same counter, same node, same method,
> monotonic across four deploys. Three earlier sessions dismissed it by
> re-baselining the prompt; that verdict is withdrawn. Tracked as §3a with the
> evidence and the A/B that would name the commit. **Report the deviation, do
> not adjust the baseline.** Still not diagnosed.
>
> **A failed TCP bind is a warning, so m6-http can run with nothing listening
> on 443.** Found 2026-09-12, tracked as §3d, deliberately **not** changed
> because it is a production behaviour change under the freeze. `SO_REUSEADDR`
> removed the most likely cause; the response is still fail-open.
>
> **A hit rate near 0.3 does not mean what the hourly prompt says it means.**
> The prompt reads a fall toward 0.3 as the edge lifetime having regressed.
> On 2026-09-12 syd read 0.3466 over 24h and 0.3052 lifetime, and the edge was
> fine: **55.2% of its requests since restart were 404s** (2,796 of 5,068) and
> a 404 is uncacheable, so every one counts as a miss. Real traffic in the same
> hour was 93 HIT / 8 MISS = **0.92**.
>
> So separate the two before reporting a regression, because the number is the
> same either way:
>
> 1. **Last hour from analytics** (`cache_state` HIT vs MISS). Near 1.0 means
>    the edge lifetime is working.
> 2. **404 share from `/perf` `status_counts`.** High means the aggregate is
>    scan volume, not lifetime.
>
> This is the case `deploy/BLOCKLIST.md` predicts in its closing section, and
> the fix it names is **negative caching**, still not implemented. Until it
> exists, every junk 404 on a cache node is a Pacific round trip.

> **The hourly health check is not scheduled.** The old cron was session-only
> and died with that session. `CronCreate` jobs are in-memory and expire after
> 7 days regardless. If the owner wants it scheduled, say plainly that it will
> not survive the session either; a durable version needs launchd or a real
> crontab.
>
> **Six addresses are blocked**, ledger and nodes in sync at 32 rules.
> `13.220.90.211` was added 2026-09-12 on instruction. Two earlier notes still
> apply: `15.177.23.18` was **not an attack** (the eighth orphaned Route53
> health check, reported as the day's strongest malicious candidate across
> three checks before its user agent was read), and `45.142.193.161` is
> deliberately **not** blocked despite topping every ufw drop list, because it
> only scans closed ports and never reaches the application.

**Read this, then `docs/CONSOLIDATION-TODO.md`.** This file is what is true;
that one is the ledger of what is done and what is owed, audited against the
commit log rather than written from memory.

---

## 1. Where the work is

**Branch `main`, clean, 103 commits ahead of the deployed `22ee3a4` here and
20 ahead of `d6ebfa5` in the site repo. No migration code is deployed and the
freeze holds until it is finished.**

> **Both repos are pushed and level with `origin/main` as of 2026-09-12**:
> `m6` at `6c1f74a`, the site repo at `5eb16d8`. Ahead of the *fleet* is the
> deliberate freeze and is fine; ahead of *origin* is unbacked work on a laptop
> and is not. If you find them ahead again, push.

> The deployed commit is whatever the newest entry in
> `~/dr-grosvenor-site/docs/RELEASES.md` names, and nothing else. **Recompute,
> do not edit in place:**
>
> ```sh
> git -C ~/m6 log --oneline <newest m6 sha in RELEASES.md>..HEAD | wc -l
> ```
>
> This line was wrong twice, naming `b32e837` and 48 when `b32e837` had already
> been superseded by the 2026-09-10 18:47 deploy (its fleet md5 `aced7223` is
> now the `.prev` rollback target). Of the 15 site commits, four record the
> hardening, block-ledger and 2026-09-12 block changes that *were* applied on
> instruction.

Three production changes WERE applied on 2026-09-11, on instruction, as
deliberate exceptions. They change how services are confined and what the
firewall denies, not what code runs:

- **systemd hardening on every node**, 1.7 OK from `systemd-analyze`.
- **The block ledger reconciled**, 26 identical rules on all three nodes.
- **`80.94.95.211` blocked** after an 843-path credential sweep.

All three verified at 11:57 UTC by the hourly check, not just by the runs that
applied them:

- Contact form delivery is in the journal, `contact form: message sent` at
  11:07:33, which is the last path the hardening left unproven and the one
  staging cannot test.
- `80.94.95.211` last reached the application at 10:08:22, before its rule
  went in, and not since. Worth knowing it had been probing since
  **2026-09-05**, six days rather than one.
- The orphaned Route53 checks are still arriving and are now dropped: about
  225 packets on each of six addresses on chi, ~1,350 in total. That traffic
  was previously refused on one node and served on two, which is exactly what
  the reconciliation was for.

- **1018 workspace tests pass at default features, zero warnings**, release
  and test builds, clippy at its 157 Darwin ceiling, 2026-09-12 (latest
  session). Verified on Linux via `deploy/run-tests.sh m6`. The sixteen over
  the previous 1002 are the wildcard, config-route-handler and streaming-body
  tests. The `app::dict_cost` module is `#[ignore]`d on purpose: those are
  measurements, and a timing assertion is the wall-clock trap.
- **The port race recurred once on 2026-09-12 (latest session)**, in
  `security_e2e::forged_x_auth_claims_does_not_survive_ingress_e2e`:
  `HTTP/1.1 TCP listener bind failed ... Address already in use (os error 48)`,
  then "m6-http never served a backend request". Clean on the next full run and
  on three runs in isolation. `SO_REUSEADDR` (`07f11d8`) was believed to have
  closed this; it has not, or not entirely. It is also §3d demonstrated: the
  bind failure is a `warn!`, so the process came up with nothing on the port
  and the test reported the symptom rather than the cause.
- **Clippy is a gate now**, on the owner's instruction: `tools/clippy.sh`,
  wired into `check.sh` (step 2, so the pre-push hook covers it) and into the
  Linux gate before prod. It is a **ratchet**, not `-D warnings`: the count may
  fall and may never rise. Ceilings are **per platform** because clippy
  versions disagree, at `tools/clippy-ceiling-Darwin.txt` (157) and
  `tools/clippy-ceiling-Linux.txt` (145). A clippy *error* fails regardless.
  **Driving the ceiling to zero is outstanding and not yet approved as work.**
- h1spec **32/32 on all four HTTP/1.1 targets**, with a CI ratchet
  (`tools/conformance.sh`, floors in `tools/conformance-scores.txt`) wired into
  `check.sh` as a blocking gate.
- h2spec and h3spec are **not installed on the laptop** and skip on every local
  run. They must run on the build box before deploy.

### Migration status (`docs/m6-core-implementation-plan.md`)

| phase | what | status |
|---|---|---|
| 0 | Prerequisites | done |
| 1 | Small consolidations | done |
| 2 | `m6_core::testkit` | done |
| 3 | Semantics | done, 3.3 dropped |
| 4 | HTTP/1.1 | done, 32/32 |
| 5 | Service loop | done |
| 6 | Consumer apps link `m6-core` only | done |
| 7 | Decouple the repositories | **next**, not started |
| 8 | Backend examples | not started |

**`m6-render` no longer exists.** m6-core is the only crate a service links.

---

## 2. Standing constraints that do not lapse

- **Never use em dashes** in prose written for the owner.
- **Zero compiler warnings**, pre-existing included, checked on Linux.
- **Test locally, commit, then deploy. Never deploy from an uncommitted tree.**
- **Secrets never enter git.** Only `.example` files, paths, documented shape.
- Any change to **layout, copy, or rendering** needs individual approval before
  it ships.
- Image resizing is the owner's job.
- Firewall blocks are **per-IP only**, no CIDR rules.
- Crawler sightings are reported **explicitly, every run**, even a quiet one.
- The build box is **not backed up**. Everything done to a node is in git.
- Keep dynamic allocations to an absolute minimum. **Latency is the key
  metric.**
- "Clean and consistent is the only way forwards. Apps should deviate only
  where functionality demands it."
- "Clean simple code with lots of reuse out of core. This is not the place to
  get clever or inventive."

### The architectural rule, restated 2026-09-11

**m6-core is the PHP of m6: a box of blocks a service is assembled from.**
Anything we can reasonably expect to generalise to other sites and instances
belongs in core, and core should be **the only thing a service needs to link**
to build its own m6-compatible service. mgrosvenor.com is the first instance of
a general system, not the thing the system is for.

A default service being nearly a no-op on top of core is the result we want,
not a smell. `m6-html` is the whole renderer:

```rust
use m6_core::prelude::*;
fn main() -> anyhow::Result<()> { App::new().run()?; Ok(()) }
```

---

## 3. What m6-core is now

15,191 lines across 31 modules, recounted 2026-09-12. **The reference is
written:
`docs/m6-core-reference.md`**, every module and its interface. The table below
is the index; that file is the detail. What remains is *inside* the code:
eighteen of the thirty-one modules still have no module-level doc comment
(recounted 2026-09-12; it was seventeen of thirty before `server.rs` grew and
the count moved).

| module | what |
|---|---|
| `app` | the service loop: thread pool, bounded queue, 503 backpressure, routing, config reload |
| `h1`, `parse`, `http` | HTTP/1.1 parsing, framing, the one response writer |
| `server` | unix socket server, `serve_connection` |
| `request`, `response` | the handler-facing types, form/query/cookie parsing, percent coding |
| `cookie` | the one `Set-Cookie` formatter |
| `headers` | case-insensitive access, repeated fields, RFC-correct combining |
| `config` | TOML config loading |
| `ndjson` | newline-delimited JSON, read and written |
| `telemetry` | analytics records, `periodic stats`, traffic classification |
| `host` | load, memory, disk, thermal, uptime. Reading only |
| `monitoring` | `/health` and `/perf`, shared by producer and consumer |
| `render`, `template` | the renderer seam, and Tera behind it |
| `conditional`, `negotiate`, `mime`, `compress`, `minify` | caching semantics, content negotiation |
| `signal`, `watcher`, `log`, `random`, `path`, `util`, `error` | the rest of the runtime |
| `testkit` | the shared harness, behind a feature |

Services: `m6-http` (edge/proxy), `m6-file`, `m6-html`, `m6-auth-server`,
`m6-md`, `m6-monitor`, plus `m6-auth`/`m6-auth-cli`.

---

## 4. Immediate next steps, in order

0. **Deploy what is already written but on no node**: `m6-monitor` (runbook
   `deploy/FLEET-MONITOR.md`) and the firewall stats collector
   (`deploy/FIREWALL-STATS.md`). Both are tested, neither is installed, and
   until they are the hourly check still needs the Python script.
1. ~~**Document m6-core in full.**~~ **DONE 2026-09-11:
   `docs/m6-core-reference.md`.** All 30 modules and their interfaces, written
   against the source. `m6-render-lib.md` is marked superseded (deleted crate)
   and `m6-core.md` §9 is marked historical (pre-migration gap analysis that
   read as current state). What remains is *inside* the code: seventeen of the
   thirty modules still have no module-level doc comment, listed in
   `CONSOLIDATION-TODO.md`.
2. ~~**One app shape, minimal set only.**~~ **§3b-now COMPLETE 2026-09-12**,
   all four: read timeout, socket mode, `send_with_length`, poll block. Details
   in §6b and in `CONSOLIDATION-TODO.md` §3b.

   **The scope note was wrong and the owner corrected it. Read §3b before
   planning.** Deferred is the **event loop and the handler contract, only**.
   The IO layer is in scope as low-touch. Wildcard routing and streaming
   bodies were never deferred and are now **done**. Both migrations were never
   deferred either.

2a. **The two migrations, which are the live consolidation work.**
   - **`m6-auth-server` onto `App`: ready now, nothing blocking.** Four routes,
     all literal (`/auth/login`, `/auth/refresh`, `/auth/logout`,
     `/auth/public-key`), no wildcards needed. Its stated blocker was `chmod`
     on the socket and that is now `[server] socket_mode`. It is **not running
     in production** (the origin's `site.toml` has its backend commented out),
     so the risk is low.
   - **`m6-file` onto `App`: unblocked, and the blocker is now built.**
     Wildcard routing and streaming both landed, and streaming was never a
     blocker anyway because m6-file buffers everything. The last real
     difference was that **`App` registered code routes once at startup, so a
     config reload could not add or change one**, where m6-file's
     `handle_reload` rebuilds its table. The owner's instruction on 2026-09-12
     was *"And dynamicly reload the file list."* **Core side DONE** the same
     day: `App::handler(name, f)` plus `handler = "..."` on `[[route]]`, with
     per-route settings for `root` and `tail`, in `CONSOLIDATION-TODO.md` §6.
     **The migration itself is the next piece of code to write**, and it is
     what will carry the end-to-end reload test the core change does not have:
     write the config, let the watcher fire, get 200 on a path that did not
     exist a moment ago.

2b. **Finish header to dict.** `FrameworkState::build_dict` is private and is
   where the real knowledge lives: twelve ordered steps, and the ordering is
   load-bearing (built-ins go in *after* params files so a params file cannot
   override them). The dict-to-header half landed in `3e7a7d8`.
3. ~~**Compile the watcher fix on the build box.**~~ **DONE 2026-09-11**, via
   `deploy/run-tests.sh`: 962 passed, 0 failed, **0 warnings** on Linux for both
   the release and test builds, and the compile-time assertion
   `align_of::<libc::inotify_event>() <= 8` was evaluated for the first time and
   holds.
   ~~**Half the item remains and it is a different half.**~~ **THAT HALF IS
   NOW DONE TOO, 2026-09-12, `e6ba278`.** `ConfigWatcher` has four tests,
   waiting on its own fd with `poll(2)` the way `App` does rather than
   sleeping. Writing them found two defects in the macOS implementation, both
   fixed in the same commit: `new` returned before its watcher threads had
   registered their kevents, so an edge-triggered change in that window was
   lost silently, and those threads never exited. Both went with the threads,
   which should never have existed: a kqueue descriptor is pollable, so it goes
   straight onto the service's own poll loop as Linux already did with inotify.
   What is still owed there is §3c, the rewrite onto `nix`.
4. **Deploy `m6-monitor` on the build host and prove it.** **Verified
   2026-09-12: it is installed on no machine at all** — not syd, lon or chi,
   and not the build host. This file used to say "has never polled a real
   node" while `CONSOLIDATION-TODO` said "has now been run against the real
   fleet from the laptop and works"; the two disagreed and neither was checked.
   What is certain is that nothing is installed anywhere.

   **It also cannot replace `tools/health-check.py` yet, whatever gets
   installed.** `--check` reads `/traffic`, which **404s** on the deployed
   binary, and `/perf`, whose deployed shape is `{node, uptime_s, metrics}`
   with no `pools` field. Both were measured on syd on 2026-09-12. So the
   build-host install is worth doing on its own (it is off-fleet and breaks no
   freeze), but retiring the script waits on a post-freeze binary reaching the
   nodes **and** on the firewall stats collector, because ufw counts are the
   firewall's data and m6 does not expose them. `deploy/FLEET-MONITOR.md` is the runbook. It runs
   **off-fleet**, not on the centre: a monitor on syd cannot report that syd is
   down. The build host already reaches all three nodes and already holds the
   `/perf` token.
5. ~~**The m6-http header sweep**~~ **DONE 2026-09-12.** It was 25 sites, not
   "about a dozen", and m6-http was using none of `m6_core::headers`. Zero
   remain in non-test code. Two of them were bugs rather than duplication: the
   Set-Cookie scan and the `Connection` token check each read only the first
   line of a field that may legitimately repeat.
5b. **Rewrite `watcher.rs` on `nix`'s safe wrappers** (`CONSOLIDATION-TODO` §3c),
   owner's instruction 2026-09-12. The threads are already gone (`e6ba278`,
   single pollable kqueue on the main poll); what remains is ~390 lines of raw
   unsafe libc across three cfg arms, including the manual `inotify_event`
   pointer walk that produced the alignment UB. `nix` is already a dependency.
   **Do not reach for `notify`**: it spawns its own thread and delivers over a
   channel, which puts back what `e6ba278` removed. The watcher must keep
   exposing a pollable fd for the service's own poll loop.
5c. **Decide what a failed bind should do** (`CONSOLIDATION-TODO` §3d). A
   failed TCP bind is a `warn!` and m6-http continues with the listener set to
   `None`, so it can run with nothing on 443 while systemd sees it healthy.
   `SO_REUSEADDR` removed the likely cause; the response is untouched because
   it is a production behaviour change under the freeze. Check whether any node
   role legitimately runs without a listener before making it fatal.
6. **HTTP Garden** (arxiv 2405.17737), the differential fuzzer. Needs Docker,
   so the build box.
7. **h2spec and h3spec on the build box** before any deploy.
8. **Benchmark Phases 5 and 6, and find the p50 regression with the same run.**
   The plan requires a delta per phase and neither has one; the whole request
   path moved between crates. **Now also the way to settle §3a**, the cache-hit
   p50 that has gone 1.7 -> 3.9us across four deploys in six days. One paired,
   interleaved A/B across `084f89e`, `438bdb3`, `b32e837`, `22ee3a4` and HEAD
   answers both questions, so do it once.
9. **Phase 7**, then **Phase 8**.

---

## 5. The hourly health check

**`m6-monitor --check <config>` is the health check** (`d524334`). It prints
the hourly report and exits 1 on faults, over the same code path as the page,
so the two cannot disagree. Every part the Python script sshed for is now a
field the node computes: log targets and analytics from `/traffic`, periodic
stats and host state from `/perf`.

It also removes two of the three self-measurement defects by construction: it
generates no load and runs no scans, so it can neither flag its own traffic as
an incident nor time itself against its own journal read. The third, that it
measures from wherever it runs, is unavoidable and is labelled in the output.

**`tools/health-check.py` is kept, deliberately, and is still what to run
today.** It reads ufw block counts, which are the firewall's data rather than
m6's, and it is the only thing that has produced a full report against the
binaries actually deployed. Under the freeze `--check` degrades honestly
rather than usefully: `/traffic` 404s because the endpoint is not deployed and
`/perf` fails to parse with `missing field pools`, both reported as named
warnings rather than blanks. The script goes when the nodes run a binary from
this side of the freeze.

`docs/health-check.md` is still worth reading, but as *why*, not *how*. Every
trap it describes is encoded in both.

Three nodes, always all three:

| node | role | service | WireGuard | analytics file |
|---|---|---|---|---|
| syd | origin | `m6-http-origin` | 10.0.0.1 | `/var/www/dr-grosvenor-site/logs/analytics.ndjson` |
| lon | cache | `m6-http-cache` | 10.0.0.4 | `/var/www/m6-cache/logs/analytics.ndjson` |
| chi | cache | `m6-http-cache` | 10.0.0.5 | `/var/www/m6-cache/logs/analytics.ndjson` |

Most of this is now reachable over HTTP instead of ssh, because `/perf` carries
the host's load, memory, disk and temperature. What still needs a shell: the
**log-target histogram** (Part A) and **ufw block counts**. A log-target count
on `/perf` would remove Part A outright and is worth doing.

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
  `check.sh` step 2 and in the Linux gate. A ratchet, per platform, and a
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

`deploy/run-tests.sh` rsyncs both trees to `root@45.63.29.146` **port 4022** and
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
  `mkdir -p "$WORK"` runs ~330 lines later, so on a fresh box the ratchet's
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
  UA-rotation heuristic flags the backbone addresses `10.0.0.4` (lon) and
  `10.0.0.5` (chi) as forging bot UAs, because a cache node relays real
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
`10.0.0.1:80` is h2c-only and does not answer HTTP/1.1 at all, and the cache
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
  a recorded production measurement in `~/dr-grosvenor-site/docs/RELEASES.md`,
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

## 8. Lessons that cost something to learn

The first twelve are from Phase 4 and are in the plan too. These are the ones
worth carrying.

1. **Measure the candidate before consolidating onto it.** The plan named
   `m6-core/src/parse.rs` as the consolidation target for HTTP/1.1; measured,
   it was the *worst* of the four at 14/32.
2. **A test can pin wrong behaviour as firmly as right behaviour.** Both
   `/health`'s allowlist test and `empty_body_is_unchanged` passed for as long
   as the defect existed.
3. **A number measured from traffic you generated is not a production number.**
4. **An absent file at the path you expected is not evidence the feature is
   off.** Check where the process actually writes.
5. **Conformance harnesses produce fake scores.**
6. **Kill by PID. Never `pkill -f <pattern>` naming a port or config path.**
7. **Benchmark paired and interleaved against a fixed baseline.**
8. **Do not hand-roll a conformance tester.**
9. **Derive a security boundary structurally, not from config.**
10. **Safe by default, opt in explicitly.**
11. **Blocking a signal without installing a handler makes a process
    unkillable.**
12. **Silently swallowing a parse error looks like health.**

New this session:

13. **A feature gate that hides code from the default test run is how tests
    rot.** Two `csrf` tests stayed broken from Phase 4 to Phase 5 because
    `cargo test --workspace` never compiled them. This is why chrono and lru
    are unconditional dependencies of core rather than gated: the owner chose
    the dependency over the blind spot.
14. **A monitoring tool will measure its own effect, and it will keep doing
    it.** Three times in one day, three different mechanisms: the health check
    flagged its own load generator as a security incident; it read a 50%
    latency regression off a single sample taken during that load; and it timed
    a 4ms loopback call immediately after a 24-hour `journalctl` scan on a
    two-core VM, reporting 266ms maxima that did not reproduce in twelve clean
    samples a minute later. The fixes, in order: require failure as well as
    volume, take the median of five, measure before the scan rather than after.
    Assume there is a fourth.
15. **A fault list only works if everything on it is a fault.** The same rule
    fired on 185 requests from the operator's own address and on one 404 to
    `/.git/config`. Both are noise and both teach the reader to skim. What
    counts on its own is what is deliberate: injection, user-agent rotation,
    scanning across several distinct paths. Volume counts only together with
    failure, and a single refused probe is recorded without being escalated.
16. **A claim can be literally true and support a false conclusion.** "Zero
    template files" was true of the three renderers and did not mean they
    needed no template engine.
17. **Read the full user-agent list; a keyword list invents crawlers.** One IP
    rotating 526 user agents would have been reported as a dozen AI crawlers
    visiting. `UA_ROTATION_THRESHOLD` makes that a property of the data.
18. **Encoding and decoding are two halves of one block.** Core could
    percent-decode and not encode, so callers wrote their own encoder. The same
    shape as having four cookie formatters and no cookie type.
19. **Check the transport before writing the runbook.** The monitor was
    designed against the WireGuard mesh because that is the obvious answer;
    origin's backbone listener is h2c-only and the cache nodes have none.
20. **A monitor inside the thing it monitors cannot report the failure that
    matters.** It was specified for the central node until the owner asked
    where it should run. Run it on syd and the fleet digest dies with syd.
    Related: a client that builds a fresh connection per request pays a cold
    TLS handshake each time, which over a long link is most of the measurement
    (828ms to lon, versus 27ms to syd, and the difference is the handshake).
21. **Confinement must claim only what the role actually has.** Putting
    `ReadWritePaths=/run/m6` in the shared hardening fragment took London off
    the air for about ninety seconds on 2026-09-11 with `226/NAMESPACE`: a
    cache node proxies to origin over h2c, runs no socket backends, and so has
    no `/run/m6` at all, and an absent `ReadWritePaths` target fails mount
    namespace setup outright. The fix is per-role fragments plus
    `RuntimeDirectory` to guarantee what must exist, never a `-` prefix to
    excuse an absent path: tolerance turns a misconfigured node into one that
    starts anyway with weaker isolation than intended. Written up in the site
    repo at `deploy/systemd/hardening/_common.conf` and
    `deploy/systemd/hardening/m6-http-cache.conf`.

    The second half of this lesson is that the warning was *already* in
    `deploy/systemd/m6-html.service`, read earlier the same session, and the
    mistake was made anyway. A caution that lives only next to the code it
    guards will be read and not retained. That is why it is here.

New 2026-09-12:

22. **Counts rank a source; identity decides what it is.** `15.177.23.18` sent
    371 requests to chi, 100% refused, for three hours without adapting, and
    was reported across three consecutive hourly checks as the strongest
    malicious block candidate of the day. Reading one field settled it:
    `UA: Amazon-Route53-Health-Check-Service`, path `/route53-health/index.php`,
    every 30 seconds to the second. It is the eighth orphaned health check, and
    seven siblings were already in the ledger. **Every trait that made it look
    like a determined attacker is what a dead health check looks like**: high
    volume, total failure, infinite persistence, no adaptation. An attacker
    varies paths when refused; nothing was reading these results to vary them.
    Rank by counts, then read the user agent and the path before naming it.
23. **A gate that cannot measure must fail, not pass.** `run_h3` in
    `tools/conformance.sh` never starts the server it tests, and records a
    score only `if got > 0`, so a run that measured nothing printed **PASS**.
    That is lesson 12 living inside the thing built to enforce lesson 5. Any
    check whose failure mode is silence needs an explicit "did I actually
    measure anything" assertion.
24. **The same defect wears different clothes at every layer.** Four copies of
    `socket_path_from_config`, five hand-written copies of the lifecycle
    assertion, two accept loops 22/33 identical, four I/O idioms, two
    concurrency models. Each was found by asking "how many implementations of
    this are there?" rather than by reading any one of them. That question is
    the most productive one available in this codebase and it has not stopped
    paying yet.
25. **Check which mechanism is running before explaining why it cannot work.**
    Asked to reschedule the hourly health check, I explained at length why a
    cloud routine could not reach production over ssh. The check was a local
    session cron using my own shell and keys, and the answer was one field in
    a cron expression. `CronList` first, theory second.

26. **A safety net added at one layer becomes a defect at the next.** Giving
    `App` a read timeout was scoped as three lines and a config key. Setting
    the option was indeed three lines. What the scoping missed is that nothing
    downstream had ever seen a read time out: the timeout surfaced as
    `ParseError::Io(WouldBlock)`, whose status is 400, and `serve_connection`
    dutifully wrote that 400 to a peer that had sent nothing. m6-http pools
    backend connections, so a response written into an idle socket is read as
    the answer to the *next* request on it, which is manufactured response
    smuggling on a code path added to improve safety. The fix is the split the
    parser already made for `Ok(0)`, on whether any byte arrived: nothing means
    an idle peer leaving, so close silently; a stalled part-request gets 408.
    **When adding a deadline, follow the new error all the way to the wire**,
    and ask what the peer does with whatever gets written.
27. **`testkit::binary()` prefers `target/release`, and will happily hand a
    test a binary from yesterday.** `cargo test` rebuilds the lib and the test
    binary from current source, then spawns a service binary that may be hours
    old. On 2026-09-12 a correct fix measured as broken twice, and the wire
    said the opposite of the test: a manual run closed the connection at
    exactly 1.00s while the suite insisted nothing happened. The release-first
    rule is deliberate and should not be flipped (it was itself a fix, see the
    doc comment), and there is no sound mtime check to bolt on, because in the
    intended order cargo relinks the test binary *after* the release binaries.
    So it is procedural: **`cargo build --workspace --release` before
    `cargo test`**, and when an end-to-end test contradicts what the source
    plainly says, `ls -la` the binary before debugging the code.
    `deploy/run-tests.sh` builds release itself and is not exposed.
28. **Skipping a read means skipping what the read was rejecting.** m6-file's
    HEAD fast path answers from `std::fs::metadata`, which succeeds on a
    directory and reports its size, so `HEAD /assets/css` returned `200` with
    `Content-Length: 128` while the GET beside it returned 404. The `fs::read`
    the fast path removed was doing two jobs: producing the bytes, and failing
    on anything that was not a file. Only the first was obvious. **Before
    skipping work, list what that work was implicitly validating.** Same family
    as the conditional-request defect: a correct function in one crate and a
    wrong inline copy in another, invisible until the cache state changed.
29. **A control assertion is what makes a test mean anything, and it earns its
    place on the machine you did not think about.** The HEAD test chmods a file
    to `000` and requires the HEAD to answer anyway. The Linux build host runs
    the suite as **root**, root ignores permission bits, the file stayed
    readable, and what fired was the control: "the file must really be
    unreadable or this test proves nothing". Without that line the test would
    have gone green on a box where it demonstrates nothing, which is the
    "never been red" failure wearing a uid. It now skips explicitly when it can
    read a `0000` file. **A green tick that depends on who ran it is worse than
    an absent one.**
30. **A method-equivalence test only covers the inputs it is given.** The HEAD
    against GET comparison walks identity, minified and brotli and asserts
    identical headers, and it stayed green through the directory defect above,
    because every path it asks for is a file. Equivalence is not coverage.
31. **A test helper that cannot fail is not a probe.** Four readiness helpers
    in the e2e suites `.unwrap()`ed a transport error while being called from
    inside a 30-second retry loop written to tolerate exactly that. Two of them
    panicked on a refused connect *after* asking whether the service was alive,
    and printed the answer in the panic: `failed with m6-http alive`. **When a
    failure message contains its own refutation, believe the message.**
32. **"It does not reproduce" is a statement about the harness, not the bug.**
    Five separate intermittent failures were each reproduced deliberately once
    the mechanism was guessed: a listener that accepts and drops for the RST, a
    50 ms window on each side of a global static for the race. The one that
    took longest, `a_fresh_flag_is_clear`, ran clean 300 times in isolation and
    300 times under artificial CPU load, and was still a hard race. **Narrow is
    not the same as rare, and neither is the same as acceptable.**
33. **A gate must run where production runs, and the difference will find you.**
    Three things only showed up on the Linux build box: clippy was not
    installed, the clippy count differed by 13 from the laptop's (different
    version, plus cfg-gated code that only compiles there), and the
    unreadable-file test could not work because the box runs as **root**, which
    ignores permission bits. None of it was visible locally.


34. **A doc comment that justifies a decision by naming a premise becomes a lie
    the day the premise changes, and nothing anywhere checks it.**
    `validate_path_param` said, correctly and at length, that every parameter
    is validated with `allow_slash = false` *because* "this crate's router has
    no catch-all support ... a parameter here captures exactly one path segment
    and can never contain a slash". Adding `Segment::Wildcard` falsified that
    sentence and left the code it was explaining in place, so the one capture
    defined to hold slashes was answered 400 by the validator. The comment was
    the best possible warning and it was in the one file the change did not
    touch. **When adding a capability, grep for the assumption it invalidates**,
    not just for the code it calls: `allow_slash`, `exact segment count` and
    `can never` were each one search away. Same family as lesson 21, where the
    caution lived next to the code it guarded and was read and not retained.
35. **Two of this session's three findings were in work already marked DONE.**
    Wildcard routing shipped with six green tests that all stopped at the
    matcher, and the `Last-Modified` loop contradicted the comment at its own
    emit site. Neither was found by reading the ledger, which said both were
    finished; both were found by using the feature for the next thing. **The
    cheapest audit of a completed item is the first real consumer**, and until
    there is one, "done" means "written", which is what §6's closing note now
    says out loud about the reload chain itself.


36. **Two ledger entries written 23 minutes apart contradicted each other, and
    the later work inherited the earlier claim.** `a979390` at 14:32 recorded
    "streaming never blocked m6-file, checked against the source"; `8c79ee7`
    at 14:55 gave m6-file `send_stream`. Both were accurate when written. The
    migration row kept pointing at the first one for the rest of the day, and
    a session later it was still being quoted, by me, to the owner. **A
    "checked against the source" note is a measurement with a timestamp, not a
    fact**, and it expires the moment the source changes. When a row cites a
    check, cite the commit it was checked at, so the next reader can see
    whether anything has landed since.
37. **Measure the cost of the thing you are migrating onto, not just its
    capabilities.** The gating question for m6-file looked like a list of
    features `App` lacked, and all of them got built. What actually stops the
    migration is that `App` deep-copies its whole static config into a fresh
    map on every request. No capability list would have surfaced it; one
    `#[ignore]`d measurement did. Lesson 1 said measure the candidate before
    consolidating onto it, and that was about *correctness* scores; this is
    the same rule about cost.
38. **A synthetic benchmark measures the shape you imagined, not the one that
    runs.** The first figure was 3.08us, from a config with twenty short
    string keys, and it was reported as the finding. The owner's reply was
    that 3us sounded wrong for building a small map, which was the right
    instinct twice over: the map and the parser are 41 to 583ns, so the number
    was all copy; and the real config loads a 68KB JSON file **twice**, making
    the true figure ~323us, seventy times larger. **Take the input from
    production before quoting a number**, and when a measurement looks too big
    for what it claims to measure, that gap is the finding.
