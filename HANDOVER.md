# Handover

**Written for someone taking over with no prior context.** Read this file top
to bottom before touching anything. It is what is true right now.

Then: `CLAUDE.md` for the rules, `docs/CONSOLIDATION-TODO.md` for what is owed.

Last rewritten 2026-09-13, and **every factual claim in it was checked against
the repository, the fleet and the GitHub API that day** rather than written
from memory. Two verification passes have now run over it. The first found two
wrong claims and a real bug in the test harness; the second found three more
wrong numbers. All of them are corrected below, and the pattern is worth
naming: **every single error was a number that was true when it was measured
and had quietly stopped being true by the time it was read.** Counts, commit
hashes and scores rot. Where this file can give you the command instead of the
number, it does.

---

## 0. If you read nothing else

- **Do not deploy.** 135 m6 commits are undeployed behind a deliberate freeze,
  counted 2026-09-13; recompute with the command in §3, never trust the
  number. Lifting the freeze has a known hazard: §6.
- **`main` is not what is running.** It holds all of that undeployed work. The
  deployed commit is whatever the newest entry in
  `~/dr-grosvenor-site/docs/RELEASES.md` names.
- **m6-http depends on a FORK of quiche**, `mgrosvenor/quiche` branch
  `m6-h3-conformance`, pinned by revision. It is quiche master plus two open
  upstream PRs, and it takes h3 conformance from 37/49 to **47/49**. Drop it and
  return to a tag as soon as upstream releases those fixes. §4.
- **49/49 is not being chased.** The last two are QPACK, they are upstream's
  choice rather than a bug, and the owner has accepted them: not a 1.0 blocker.
- **There are two repositories** and they must deploy together. §1, §6.
- **Blocking an IP address is a write.** Propose, never apply unasked. §7.

---

## 1. What this is

`m6` is an HTTP stack in Rust: an edge (`m6-http`: TLS, HTTP/1.1, HTTP/2,
HTTP/3, a cache, proxying) and services behind it built on a shared library
(`m6-core`). It serves **mgrosvenor.com** from three nodes.

**The site is a separate repository**, `~/dr-grosvenor-site`, holding content,
production configs, deploy scripts, and three renderer crates of its own
(`render-cms`, `render-analytics`, `render-contact`). Most changes touch both
repositories, they deploy separately, and they can disagree. That is the
largest release risk; see §6.

### The fleet

| node | role | service | WireGuard | analytics file |
|---|---|---|---|---|
| syd | origin | `m6-http-origin` | 10.0.0.1 | `/var/www/dr-grosvenor-site/logs/analytics.ndjson` |
| lon | cache | `m6-http-cache` | 10.0.0.4 | `/var/www/m6-cache/logs/analytics.ndjson` |
| chi | cache | `m6-http-cache` | 10.0.0.5 | `/var/www/m6-cache/logs/analytics.ndjson` |

All three are **1-core VMs**; syd has 950MB. Access is `ssh root@<node>.mgrosvenor.com`.

The **build host** is a separate 4-core Linux box, `root@45.63.29.146` **port
4022**. It runs every gate that needs a quiet machine or a real Linux. It is
**not backed up**: everything done to a node must be in git.

### The services

| binary | what it does | shape |
|---|---|---|
| `m6-http` | the edge: TLS, h1/h2/h3, cache, proxy | its own event loop, not an `App` |
| `m6-file` | static files | `App` service, one named handler |
| `m6-html` | renders pages from templates | `App` service, no code routes |
| `m6-auth-server` | login, tokens, keys | `App` service with global state. **Not running in production** |
| `m6-md`, `m6-monitor` | markdown, fleet digest | `App` services |

### The architectural rule

**m6-core is the PHP of m6: a box of blocks a service is assembled from.**
Anything that generalises belongs in core, and core should be the only thing a
service links. A service being nearly a no-op on top of core is the goal:

```rust
use m6_core::prelude::*;
fn main() -> anyhow::Result<()> { App::new().run()?; Ok(()) }
```

---

## 2. How to work here

### The branch model, new as of 2026-09-13

| branch | what it is |
|---|---|
| `main` | releases only. Advances **only** via `tools/release.sh`. |
| `develop` | where work is integrated |
| `<type>/<issue>-<slug>` | one branch per issue, off `develop` |

Types: `feat` `fix` `perf` `docs` `refactor` `test` `chore`.

`.githooks/pre-push` (version-controlled, via `core.hooksPath`) refuses a push
to `main` that `release.sh` did not make, a merge on `develop` without the
record that the checks ran, and a branch name with no issue number.

```sh
./tools/branch.sh 42 some-slug --type fix   # off develop; checks the issue exists
git push origin fix/42-some-slug            # fast local checks
./tools/merge.sh fix/42-some-slug           # everything on the build host, then merges
./tools/release.sh 1.0.0                    # develop into main, changelog, tag
```

**`merge.sh` takes more than ten minutes.** It runs a release build, the whole
suite, clippy, h1/h2/h3 conformance and the performance check on the build
host. Run it in the background and let it finish. It refuses on a dirty working
tree, which it has already caught me doing.

### The tools

| tool | what it does |
|---|---|
| `tools/branch.sh` | start work; confirms the issue exists with `gh` |
| `tools/merge.sh` | full checks, then merge into develop, recording what ran |
| `tools/release.sh` | develop into main; refuses without a CHANGELOG entry |
| `tools/clippy.sh` | clippy, `-D warnings`. No ceiling, no `--update` |
| `tools/conformance.sh` | h1/h2/h3 against recorded minimum scores |
| `tools/perfcheck.sh` | page render against a recorded number, 20% margin |
| `tools/health-check.py` | the hourly production check; §7 |
| `check.sh` | the laptop pre-push set |
| `~/dr-grosvenor-site/deploy/run-tests.sh` | everything, on the build host |

### How to write for the owner

- **No em dashes.**
- **No agile or consultant vocabulary.** He is an old-school Unix engineer and
  said so directly. Say "a recorded minimum", not the other word. A latency
  spike is fine; an investigation is not a "spike".
- Commit messages: what changed, why it mattered, **how it was verified**.
  Verified means measured against something running.
- He reads carefully and pushes back hard on hand-waving, and he is usually
  right. Give him the number, and say plainly when you were wrong.

---

## 3. Where the work is, exactly

### Branches

| repo | branch | state |
|---|---|---|
| m6 | `main` | `ead669b`, **12 commits behind `develop`**. Not what is running: it also holds work that has never deployed. |
| m6 | `develop` | `bc1e9b1`, pushed |
| site | `main` | `e9f11c2` |
| site | `develop` | `0b04e67`, 2 ahead of `main`, pushed |

**Two different numbers get confused here, so keep them apart.** `main` is 12
commits behind `develop`: that is unreleased work. 135 commits are undeployed:
that is measured from the deployed commit, which is far behind `main`. An
earlier version of this table printed 135 in the `main` row and was wrong.

**CI runs on `develop` now**, from 2026-09-13, and before that push it had
never run there. **Two runs have gone green end to end**, all five jobs: build,
tests, clippy, fmt, h1/h2/h3 conformance, cargo-deny and MSRV.

Expect **roughly 10 minutes warm and 40 cold.** The first run took 41m49s and
the second 9m58s, same workflow, and the difference is the cargo cache. Forty
minutes with no output looks exactly like a hang, so it is worth knowing it is
not one: conformance has to build the whole stack and drive three protocol
testers. Watch with `gh run list --branch develop`.

### What is deployed

**Nothing since 2026-09-10**, m6 `22ee3a4`. Recompute, never edit in place:

```sh
git -C ~/m6 log --oneline 22ee3a4..develop | wc -l
```

**The site side of that cannot be recomputed at all.** Every entry in
`RELEASES.md` names the m6 commit it shipped and none of them names the site
commit, checked across the whole file. So "which content and configs are live"
has no recorded answer, and any figure for undeployed site commits is an
estimate. **Record both hashes at the next release**, and treat this as part of
lifting the freeze rather than a documentation chore.

Three production changes were applied during the freeze on explicit
instruction: systemd hardening fleet-wide, the firewall block ledger
reconciled to 32 identical rules, and one address blocked. Those changed
confinement and firewall rules, not what code runs.

---

## 4. Health of the checks

Run everything: `cd ~/dr-grosvenor-site && ./deploy/run-tests.sh m6`.

| check | state | where it runs |
|---|---|---|
| tests | **1022 passing**, 0 failures, verified over three consecutive runs | everywhere |
| compiler warnings | **0**, release and test builds | Linux, enforced |
| clippy | **0**, both toolchains, enforced with `-D warnings` | `tools/clippy.sh` |
| `cargo fmt` | clean | CI, `check.sh` |
| `cargo deny` | clean, 5 advisories as recorded exceptions | CI |
| h1 conformance | **32/32** on four targets | CI, build host |
| h2 conformance | **146/146** | CI, build host |
| h3 conformance | **47/49**, on a fork of quiche. Floor 47 | CI, build host |
| performance | `render:capabilities` 2,170,589 ns | build host only |

### h3 is 47/49, on a fork of quiche, and 49 is not being chased

**m6-http does not depend on released quiche any more.** It pins
`github.com/mgrosvenor/quiche` by revision, branch `m6-h3-conformance`, which is
quiche master plus two open upstream pull requests. That is a real decision with
a real cost, taken on 2026-09-13, and §4 is where it is written down.

The short history, because every step of it was wrong before it was right:

1. h3 was 37/49 with the floor at 37, and the 1.0 list said the fix was to bump
   quiche from 0.26.1, since all twelve failures sit below the layer m6-http
   works at. **The bump was done, 0.26.1 to 0.29.3, and the score did not move
   by one test.** 0.29.3 is also the newest plain release: the higher numbers in
   that repo are `tokio-quiche`, a different crate.
2. Reading quiche's source then produced a confident claim that the twelve were
   deliberate anti-DoS design and effectively unfixable. **That was wrong, and it
   got caught only because the owner did not believe it.** A source comment
   explaining a behaviour reads exactly like one endorsing it. The issue tracker
   is where intent lives.
3. Ten of the twelve are open upstream bugs with open fix PRs. Applied and
   measured, they take h3 to 47/49.

| failures | what quiche does | upstream |
|---|---|---|
| **8** TRANSPORT_PARAMETER_ERROR | detects them (the edge log shows exactly 8 `InvalidTransportParam`) and calls `close()`, queueing the right code, then calls `mark_closed()` because `recv_count` is still 0: it increments at the end of `recv_single`, after frames are parsed. `send()` then returns `Done`, so a correctly built close can never go out | issue **#2515**, open since 2026-06-22, naming the same mechanism and the same `recv_count == 0`. Fix PR **#2521** |
| **2** PROTOCOL_VIOLATION, reserved bits | **does not detect them.** There is no reserved-bit validation anywhere in `packet.rs`; the packets are accepted | issues **#2526** and **#2652**. Fix PR **#2575**, last touched 2026-09-10. #2596 closed the Initial case; h3spec tests Handshake and Short |
| **2** QPACK stream errors | reads the peer's QPACK streams and **discards every byte**, counting only totals. Static table only, so no dynamic table capacity to exceed | nothing open, and #90 "don't error on QPACK instruction" is closed. Upstream's choice, not its mistake |

Measured on the build host, every run with the linked source confirmed in
`Cargo.lock`:

| quiche | h3 |
|---|---|
| 0.29.3 as released | **37/49** |
| tag 0.29.3 + cherry-picked #2521 + #2575 | **47/49** |
| master + both PRs | **47/49**, adopted |

Both bases scored the same, so the choice was never about the number. Master was
taken because it is where both PRs are based, which is what keeps the fork
rebasable. Pinned by **revision, not branch**, so the dependency cannot move
under a build that claims to be reproducible. quiche's own suite passes on it:
1123 tests, zero failures.

**What it costs, and this is not a formality.** Both PRs are unmerged and both
come from third-party forks, not Cloudflare, so the QUIC transport path of a
production edge now carries community changes upstream has not reviewed, plus 25
unreleased master commits. #2575's own commit message records that the ideal
test, crafting a real packet with the AEAD-protected reserved bit set, was **not**
written. And pinning a fork cuts against the reason Phase 7 chose a tag over
crates.io: not owning someone else's release surface.

**So drop the fork when upstream releases these.** That is the point of it. Watch
#2521 and #2575, then go back to a tag and re-measure.

**49/49 is not being chased.** Owner's decision, 2026-09-13: *"47/49 is good
enough. It's not going to block 1.0.0"*. The two QPACK failures are accepted.
Closing them would mean writing new protocol validation into the fork ourselves,
which is small in lines and not small in kind: new parsing on the connection path
with no upstream review, needing per-stream buffering that is exactly where a
careless version becomes unbounded memory on a stream a peer controls.
`tools/conformance-scores.txt` has the full detail, the rebuild recipe and the
one merge conflict to expect.

### clippy is zero, and there is no ceiling any more

Done 2026-09-13, issue #5. `tools/clippy.sh` is now `-D warnings`: one finding
fails the run, the same as a rustc warning. `--update` is gone and the two
`tools/clippy-ceiling-*.txt` files are deleted.

**Zero was verified on both toolchains**, which is the only reason a single
absolute rule is safe. That distinction turned out to matter more than the
ceiling did:

| | clippy | findings before |
|---|---|---|
| this laptop, Homebrew | 0.1.95 | 45 |
| build host and CI, stable | 0.1.98 | 123 |

**The per-platform ceiling was really per-clippy-version.** Identical source,
78 findings apart. New lints arrive with new versions, and cfg-gated code is
only linted where it compiles, so the macOS kqueue block in `watcher.rs` is
invisible to clippy on Linux and the Linux-only lints are invisible here. The
practical consequence: **`--fix` for the Linux findings had to run on the build
host**, with the changed sources pulled back and read under git. Fixing from a
1.95 laptop is fixing the wrong list.

The handover's old breakdown here, 30 auto-fixable and 18 collapsible `if` and
15 complex types, was taken at 135 and had rotted like every other number in
this file. What it actually took:

- **`--fix` cleared 123 to 49**, all of it behaviour-preserving: `map_or` to
  `is_some_and`/`is_none_or`, `write!` with a trailing newline to `writeln!`,
  `Error::new(Other, e)` to `Error::other`, redundant borrows, and `Default`
  for 12 types that had `new()` without one.
- **The other 49 wanted decisions**, and the owner's call was to reshape
  everything and allow nothing. Type aliases for the `Arc<dyn Fn(..)>` builder
  shapes in `app.rs`; `is_empty` beside `len`; boxing four oversized enum
  variants; `&mut Vec<u8>` to `&mut [u8]`; and two argument lists grouped into
  `EventLoopIo` and `H2Response`, which took an 8- and a 9-argument function to
  4 and 6.
- **Three findings were real documentation bugs**, not style. The clearest:
  `health.rs` carried the doc comment for a timing-safe credential comparison
  directly above `#[cfg(test)] mod token_file_tests`, so it documented the test
  module. The function it described had moved to `m6-core/src/monitoring.rs`
  during the consolidation and the comment stayed behind. Two module-level
  prose blocks in `http11.rs` and `hints.rs` were `///` rather than `//!`, so
  they documented the next `use` statement.
- **One was arguably a real bug**: a `drop(req)` in `m6-core/src/h1.rs`
  commented "release borrow of `headers`". It compiles without it. NLL had
  ended that borrow for years and the line was pre-NLL residue.

**If a toolchain upgrade brings a new lint, the gate fails.** That is intended.
Fix it, or add a targeted `#[allow]` naming the lint with the reason argued in
the commit. Do not put the ceiling back.

### m6-core costs 36% of a backend's throughput, measured

Phase 8's reason for existing, settled 2026-09-13. `rust-plain` and
`rust-m6core` in `m6-http/tests/backends/` are the same language, compiler,
payload and concurrency model, so the difference between them is the library.

| | rust-plain | rust-m6core | delta |
|---|---:|---:|---|
| throughput | 28,954 rps | 18,297 rps | **-36.8%** |
| p50 | 63.7 us | 102.2 us | **+38.5 us** |
| RSS | 2,612 KB | 22,992 KB | 8.8x |
| artifact | 555 KB | 31.5 MB | 56.7x |
| cold start | 2.9 ms | 19.0 ms | 6.6x |

Reproduced within 3%. Conditions in `docs/BENCHMARKS.md`: build host, tmpfs,
concurrency 2, 660-byte payload.

**Both of these are true and neither cancels the other.** It is a real cost, and
`docs/m6-backend-examples.md` §5.3 says a delta this far from zero means core has
a problem worth knowing about. It is also measured on the shape that maximises
it: a route whose own work is copying 660 bytes, so the framework is nearly the
whole cost, while behind the edge cache the hit rate is 0.87 to 0.90 and most
requests never reach a backend. It is not "the site is 36% slower".

The first run of this said core was 72% **faster**. The control was wrong, not
the subject: it spawned a thread per connection while core answered from a pool.
A control that differs from its subject in two ways measures neither.

### Five places the documents and the code disagree

Writing the examples found them. All five are in
`docs/m6-backend-examples.md` §10, and **two are decisions rather than tasks**:

- **§10.1, socket mode.** Protocol §1.2 says a backend MUST `chmod 0666`. Core
  defaults to 0660 and production runs 0660, because the proxy shares the group.
  The fleet contradicts a MUST and works. Either the spec says 0660, or core
  loosens and undoes a deliberate hardening.
- **§10.5, compression.** Protocol §3.6 tells backends not to compress because
  "the proxy performs content negotiation and compression itself". **m6-http has
  no compressor.** brotli and flate2 are only in m6-core; the proxy caches and
  selects per-encoding variants of what a backend produced. Measured: 660 bytes
  through the edge with `Accept-Encoding: br, gzip` come back uncompressed. So a
  C, Go or Python backend written from the spec serves uncompressed bytes
  forever, and it is invisible for the Rust services only because m6-core
  compresses on the backend side, which is what §3.6 tells backends not to do.

The other three are smaller: a bare 404 has no body, core minifies what the
examples must not, and no error mode relays a backend's own error page.

### Where checks run, and why

- **CI** (GitHub Actions, every push and PR): build, tests, warnings, clippy,
  fmt, h1/h2/h3, cargo-deny, MSRV.
- **Build host** (`run-tests.sh`): all of that **plus the performance check**,
  which is not on CI because a wall-clock measurement on a shared runner
  measures the runner.
- **Laptop** (`check.sh`): the fast subset. It passes `--allow-missing-tools`
  because h2spec and h3spec are not installed there, and the output says
  loudly what it did **not** test.

**A check that cannot measure must fail, not pass.** `tools/conformance.sh`
broke this four separate ways and reported success through all of them; h2 and
h3 were untested for months and nothing said so. Its header lists the four.

---

## 5. The road to 1.0

**1.0 is not cut until the consolidation work is done.** Owner's decision,
recorded beside the version in `Cargo.toml`. All nine crates are at 0.2.0,
matching the newest tag; `release.sh` bumps them at a release.

| # | item | notes |
|---|---|---|
| 1 | ~~**clippy to zero**~~ | **done 2026-09-13, issue #5.** 0 on both toolchains, and `clippy.sh` is `-D warnings` with the ceiling files deleted. §4 |
| 2 | ~~**quiche 0.26.1 → 0.29.3, re-measure h3**~~ | **done 2026-09-13, issue #4.** The bump alone moved nothing. h3 is now **47/49** on a fork of quiche master carrying PRs #2521 and #2575, floor raised to 47. The last two are QPACK and are accepted, not chased. §4 |
| 3 | **Phase 7: renderers onto a git tag** | below |
| 4 | ~~**Phase 8: six `/status` implementations**~~ | **done 2026-09-13, issue #6.** All six conform, 13 tests in the gate, Go installed. The measurement: **linking m6-core costs 36% of throughput and +37us p50**, 8.8x RSS, 56.7x binary. §4 below |
| 5 | **Deploy, lifting the freeze** | §6. Not a code task. |

### Phase 7 in detail

The site's three renderers carry `m6-core = { path = "../../m6/m6-core" }`: a
filesystem layout hard-coded across a repository boundary with no version
constraint. Replace with:

```toml
m6-core = { git = "https://github.com/mgrosvenor/m6", tag = "v1.0.0" }
```

**Not crates.io.** The owner's call: publishing means owning a public API, a
name, and maintenance for other people. A tag gives the versioning without any
of it, and m6-http already takes quiche exactly this way.

**Verified:** a crate depending on m6-core by git revision resolves and
compiles with no `../m6` checkout present, which is the gate Phase 7 states.

**Sequence it at the release.** It needs a tag to pin to, and pinning to a bare
revision now means pinning to a commit that items 1, 2 and 4 immediately
supersede. `deploy.sh` must stop syncing the m6 tree at the same time.

---

## 6. The deploy, and the thing that will bite

**m6-file's config and binary must land together.** The `App` migration changed
its config format: every route needs `handler = "files"`, and
`/assets/{relpath}` became `/assets/{*relpath}`. Both orderings break:

- **New config, old binary**: the old matcher reads `{*relpath}` as a
  single-segment parameter, so every nested asset 404s.
- **Old config, new binary**: routes name no handler, so everything under
  `/assets` 404s.

`deploy.sh` ships configs; `deploy-platform.sh` ships binaries. Two separate
runs. **`--dump-config` exists on every service now** and `deploy-platform.sh`
validates with it before installing, so the second case is a refused deploy
rather than a silent outage. The first is not covered. **Write a combined step
before touching production.**

### Verify after deploying: deliberate behaviour changes

- socket modes are **0660** (were 0755 from umask, or 0666 by hand)
- traversal answers **404** where a single-segment parameter gave 400
- m6-file **sheds with 503** past a 256-deep queue instead of queueing without
  bound. Production sets `size = 32`, so the queue is 256. **Watch the gallery
  page**, which fires dozens of concurrent image requests and is why the pool
  was widened to 32 in the first place.
- a route's `Cache-Control` is a **default, not an override**
- a **failed bind is fatal** where it used to warn and continue
- `UMask=0027` and `LimitNOFILE=65535` on every service
- rendered bytes are **unchanged**: verified byte-identical before and after
  the migration, same content-hash ETag, same Content-Length

### Known gaps in the deploy path

- **Staging cannot exercise the cache role.** Single origin, no cache nodes, no
  WireGuard. The 90-second London outage on 2026-09-11 was a cache-role fault
  and staging would have called that change safe.
- **`m6-monitor` is installed on no machine**, not even the build host, and
  cannot replace `health-check.py` yet: `--check` reads `/traffic`, which 404s
  on the deployed binary, and `/perf`, whose deployed shape has no `pools`
  field. Measured on syd 2026-09-12.
- **The firewall stats collector** is written, tested, and on no node. Until it
  is deployed `/traffic` reports `firewall: null`.

---

## 7. The hourly health check

**`tools/health-check.py --load` is the standing order.** It covers all three
nodes, which the syd-only commands in the prompt do not, and it encodes the
traps: per-role analytics paths, the nested record shape, ANSI stripping,
generated-versus-observed labelling, forged-bot detection.

**It is not scheduled.** A cron created from a session dies with it; a durable
version needs launchd or a real crontab.

### Three baselines in the prompt that are now wrong

1. **`hit_p50_ns` is load-dependent and not comparable across days.** On a
   near-idle single-core VM the cache-hit path goes cold between requests, so
   the number tracks request density. Same binary, same node, minutes apart:
   50-70 hits per window reads **3,900ns**; 1,200 hits reads **1,064ns**, below
   the 1.7-2.2us band treated as the baseline. **Report the window's hit count
   beside the number.** The prompt still states 1.7-2.2us flat; it is the
   owner's file to change.
2. **A hit rate near 0.3 usually means scan volume, not a regression.** A 404
   is uncacheable and counts as a miss. Separate last-hour HIT/MISS from the
   404 share. chi has read 72% 404s in an hour with real traffic fine.
3. **Crawler totals over windows longer than ~60 minutes are understated.** The
   user-agent rotation heuristic flags backbone addresses 10.0.0.4 and 10.0.0.5
   as forging bot agents, because a cache node relays real clients' agents,
   then excludes those requests. Known tool bug, not an incident.

### Standing security rules

- Firewall blocks are **per-IP only**, never CIDR.
- **Blocking is a write.** Propose candidates; never apply without being asked.
- Crawler sightings are reported **explicitly, every run**, even a quiet one.

**Four block candidates are outstanding and unblocked**: `136.69.139.253`
(1,350 requests in under two minutes, 741 rotating user agents, SSRF and cloud
credential probes), `95.173.161.147` (encoded traversal aimed at `/bin/sh`),
`94.26.106.175` (phpinfo sweep, seen on two separate days), `34.20.194.202`.

---

## 8. The dozen lessons that come up most

Full list, 40 of them, in `docs/LESSONS.md`.

0. **A number in a document is a measurement with a timestamp, not a fact.**
   Every error found in two verification passes over this file was a number
   that was true when taken and stale when read.
1. **A check that cannot measure must fail, not pass.**
2. **A gate that runs only where it is convenient is not a gate.** The only
   thing running conformance was a laptop hook on a machine with neither h2spec
   nor h3spec installed. h1 was not running on the build host either, because
   `uvx` was installed but not on the gate's PATH.
3. **Recording today's failure as the standard is the same error in a new
   costume.** h3 37/49 was the example here, and it turned out to be the
   exception that sharpens the rule: 37 is a dependency's measured ceiling, so
   the floor was legitimate and only the wording around it was wrong. §4. The
   rule stands for every floor that records something you could fix.
4. **Correct attribution is not a diagnosis.** The twelve h3 failures were
   rightly placed inside quiche, and the remedy inferred from that, "bump the
   version", was carried on the 1.0 list unquestioned until it was tried and
   moved nothing. Lesson 40 in full.
5. **Measure the candidate before consolidating onto it, and measure its cost,
   not only its features.**
6. **A synthetic benchmark measures the shape you imagined.** The first copy
   figure was 3.08us from a 20-key config; the real config loads a 68KB JSON
   file twice, making it ~323us.
7. **`testkit::binary()` prefers `target/release`** and will hand a test a
   binary from yesterday. `cargo build --workspace --release` first.
8. **A doc comment that justifies a decision by naming a premise becomes a lie
   the day the premise changes.**
9. **The matcher is not the wire.** Six wildcard tests all stopped at
   `match_route`, so a feature marked done had never worked end to end.
10. **Confinement must claim only what the role actually has.**
   `ReadWritePaths=/run/m6` in a shared systemd fragment took London off the
   air: a cache node has no `/run/m6`.
11. **Kill by PID.** Never `pkill -f` naming a port or config path. This laptop
    runs the owner's own dev and preview servers and sits at load 20-30.
12. **Counts rank a source; identity decides what it is.** 371 refused requests
    over three hours was reported as the day's strongest attacker three times.
    One field settled it: `UA: Amazon-Route53-Health-Check-Service`.
13. **Run the suite to a file and grep the file, never the pipe.**
14. **Making a warning fatal does not create the bug it reveals.** The e2e port
    race was survivable while a failed bind only warned: the service came up
    with no listener and the test failed later with "never served a backend
    request", naming the symptom and not the cause. Making the bind fatal
    turned it into an immediate honest failure, which is the only reason it was
    found. Lesson 39 has the whole thing, including the two different races
    that were being treated as one.

---

## 9. Open questions, honestly unresolved

- **`m6-auth-cli`'s `test_token_create_prints_jwt`** fails intermittently and
  has never been explained. Did not recur on 2026-09-12 or -13.
- **Four `cargo deny` advisories** listed as exceptions in `deny.toml`, issue
  #3. **Their reachability has never been established**; that issue was written
  before checking, which is the same mistake made with a fifth. That fifth,
  hpack's decoder panic, **was** checked and is **not** reachable:
  `validate_hpack_block` rejects malformed blocks before the decoder sees them,
  pinned by `http2::hpack_robustness`. Do the same for the other four rather
  than trusting the issue text.
- **GitHub issues**: #1 (CI, done, close it) and #3 (above).
- **GitHub branch protection on `main` is not set**, confirmed against the
  API: `Branch not protected`. The hooks protect one laptop. Setting it needs
  the owner's go-ahead because it changes how the repository behaves for
  everyone.
- **`FrameworkState::build_dict` is private**, so the twelve ordered steps of
  dictionary building are not reusable by a service not using `App`.
- **The IO layer, the event loop and the handler contract are deferred**,
  explicitly, by the owner. Not 1.0 work. See `docs/CONSOLIDATION-TODO.md` §3b
  and do not widen that scope.
