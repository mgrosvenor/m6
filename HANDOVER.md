# Handover

State of play for the next session. Written 2026-09-11, updated 2026-09-12.

> ## Read first, 2026-09-12
>
> **The hourly health check was a session-only cron and it died with that
> session.** Job `b54e3408`, 6:37/9:37/12:37/15:37/18:37/21:37 Sydney, six a
> day, nothing between 21:37 and 06:37. `CronCreate` jobs are in-memory and
> auto-expire after 7 days anyway. **If the owner still wants scheduled checks,
> recreate it, and say plainly that it will not survive this session either.**
> A durable version needs launchd or a real crontab on the Mac.
>
> **The app-shape architecture is agreed and deliberately not scheduled.** See
> `docs/m6-app-shape-plan.md` and `docs/CONSOLIDATION-TODO.md` §3b. The split
> is §3b-now (a read timeout, a socket-permissions key, optionally lifting a
> duplicated accept loop, plus a free `send_with_length` fix) and §3b-later
> (wildcard routing, streaming bodies, the IO layer, the event loop, the
> handler contract). **Do not re-derive the argument.** It is written up.
>
> **Five addresses were blocked on 2026-09-12**, ledger and nodes at 31 rules
> and in sync. One of them, `15.177.23.18`, was **not an attack**: it is the
> eighth orphaned Route53 health check, and it was reported across three hourly
> checks as the strongest malicious candidate of the day before its user agent
> was read. See `deploy/BLOCKLIST.md`.

**Read this, then `docs/CONSOLIDATION-TODO.md`.** This file is what is true;
that one is the ledger of what is done and what is owed, audited against the
commit log rather than written from memory.

---

## 1. Where the work is

**Branch `main`, clean, 80 commits ahead of the deployed `22ee3a4` here and 15
ahead of `d6ebfa5` in the site repo. No migration code is deployed and the
freeze holds until it is finished.**

> **Both repos are ahead of `origin/main` and that is not fine.** 61 commits on
> `m6`, 13 on the site repo, as of 2026-09-12. Ahead of the *fleet* is the
> deliberate freeze; ahead of *origin* is just unbacked work on a laptop, and
> the site handover's §5 says to push when you find this. Not pushed here
> because `m6`'s `pre-push` hook runs the full suite and the owner has not
> asked for it this session. **Push both.**

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

- **969 workspace tests pass at default features, verified on Linux via
  `deploy/run-tests.sh m6` on 2026-09-12. Zero warnings**, release and test
  builds, same count as macOS.
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

13,412 lines across 30 modules. **The reference is written:
`docs/m6-core-reference.md`**, every module and its interface. The table below
is the index; that file is the detail. What remains is *inside* the code:
seventeen modules still have no module-level doc comment.

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
2. **One app shape, minimal set only.** Scoped 2026-09-12; the architecture is
   agreed and deferred (`CONSOLIDATION-TODO.md` §3b). Do these and stop:
   - ~~**`App` sets a read timeout after accept.**~~ **DONE 2026-09-12**,
     `d52a51b`. `[server] read_timeout_s`, default 30, `0` disables. The two
     services that had hand-written the same 30 seconds now call the same core
     function. **Production needs no config change; the default applies.** It
     was not the three lines it looked like: see lesson 26.
   - ~~**Socket permissions as a config key.**~~ **DONE 2026-09-12.**
     `[server] socket_mode`, octal string, default `0660`. It was m6-file *and*
     m6-auth-server setting `0666` by hand, not just m6-auth-server, and `App`
     set nothing at all so its five services took `0755` from the umask. All
     seven are now `0660`. **This changes a file permission in production**,
     which nothing else on this list does: check the modes after the deploy.
     Safe because every unit is `User=m6` and `/run/m6` is `0750` owned by
     `m6`, so the world bits were never load-bearing.
   - **Still open, optional (Tier 2): lift the accept/poll block** m6-file
     duplicates 22 of 33 lines of.
   - **Still open, free and unrelated to shape: `send_with_length` has zero
     callers** while m6-file's HEAD does a full `fs::read`, minify and brotli-6
     before discarding the body at `h1.rs:700`.

   Anything touching performance still wants **benchmarking Phases 5 and 6**
   first, which is owed anyway. The two that remain are a code move and
   strictly less work, so neither needs it.
2b. **Finish header to dict.** `FrameworkState::build_dict` is private and is
   where the real knowledge lives: twelve ordered steps, and the ordering is
   load-bearing (built-ins go in *after* params files so a params file cannot
   override them). The dict-to-header half landed in `3e7a7d8`.
3. ~~**Compile the watcher fix on the build box.**~~ **DONE 2026-09-11**, via
   `deploy/run-tests.sh`: 962 passed, 0 failed, **0 warnings** on Linux for both
   the release and test builds, and the compile-time assertion
   `align_of::<libc::inotify_event>() <= 8` was evaluated for the first time and
   holds.
   **Half the item remains and it is a different half.** `ConfigWatcher` has no
   tests on any platform, so config hot reload on Linux still has no behavioural
   coverage. "Never compiled" is closed; "never exercised" is not.
4. **Deploy `m6-monitor` on the build host and prove it.** It is tested and has
   never polled a real node. `deploy/FLEET-MONITOR.md` is the runbook. It runs
   **off-fleet**, not on the centre: a monitor on syd cannot report that syd is
   down. The build host already reaches all three nodes and already holds the
   `/perf` token.
5. **The m6-http header sweep**, about a dozen ad-hoc lookups left.
6. **HTTP Garden** (arxiv 2405.17737), the differential fuzzer. Needs Docker,
   so the build box.
7. **h2spec and h3spec on the build box** before any deploy.
8. **Benchmark Phases 5 and 6.** The plan requires a delta per phase and
   neither has one. The whole request path moved between crates.
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

## 6a. Session of 2026-09-12

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

- **The cache-hit p50 baseline is unverified.** Recorded as 1.7-2.2us; four
  separate readings today put every node at 2.9-3.4us, flat across hundreds of
  windows. The baseline is the thing in doubt, not the measurement. Re-derive
  before treating a miss as an incident. `tools/health-check.py` deliberately
  asserts no p50 threshold for this reason.
- **24h hit rates**: syd ~0.58, lon ~0.16-0.19, chi ~0.19-0.21. Stable across
  the day. The earlier reconciliation that looked wrong was my own error:
  origin never sees what a cache node answers from its own cache.
- **Two tests are flaky, and they share a shape.** Both spawn external
  processes, both have failed exactly once inside a loaded full-workspace run,
  and neither reproduces in isolation. In both cases the assertion text was
  lost, which is the thing to fix first next time: capture the full output of
  a failing full-suite run before re-running anything.

  - `redirect_lifecycle::sigterm_shuts_down_rather_than_being_ignored`. Did
    not reproduce in 21 further runs. Ruled out: the obvious startup race,
    because `redirect::run` installs `ShutdownHandle` *before* `bind_plain`.
    It guards a bug that shipped.
  - `m6-auth-cli` `test_token_create_prints_jwt`. Did not reproduce in three
    isolated runs or a subsequent full gate. It shells out to `openssl` three
    times per `setup_keys`, and several tests in that file do the same, so
    process spawning under load is the first place to look.

  Neither is understood. A test that fails only when the machine is busy is
  either a real race or a test that is too tight, and both are worth knowing
  which.
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