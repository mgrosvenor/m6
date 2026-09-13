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
  number. Lifting the freeze has a known hazard, recorded in the deployment
  repository's `docs/OPERATIONS.md`.
- **`main` is not what is running.** It holds all of that undeployed work. The
  deployed commit is whatever the newest entry in
  the deployment repository's `docs/RELEASES.md` names.
- **m6-http depends on a FORK of quiche**, `mgrosvenor/quiche` branch
  `m6-h3-conformance`, pinned by revision. It is quiche master plus two open
  upstream PRs, and it takes h3 conformance from 37/49 to **47/49**. Drop it and
  return to a tag as soon as upstream releases those fixes. §4.
- **49/49 is not being chased.** The last two are QPACK, they are upstream's
  choice rather than a bug, and the owner has accepted them: not a 1.0 blocker.
- **m6 and a site are two repositories** and they must deploy together. §1, §6.
- **Operations lives with the deployment**, not here. §6.

---

## 1. What this is

`m6` is an HTTP stack in Rust: an edge (`m6-http`: TLS, HTTP/1.1, HTTP/2,
HTTP/3, a cache, proxying) and services behind it built on a shared library
(`m6-core`). It is generic: a site built on it is a separate repository.

**A site is a separate repository**, holding content,
production configs, deploy scripts, and three renderer crates of its own
(`render-cms`, `render-analytics`, `render-contact`). Most changes touch both
repositories, they deploy separately, and they can disagree. That is the
largest release risk; see §6.

### Where it runs

**m6 does not know.** A fleet is configuration, not code: which nodes exist,
what they are called and how they reach each other belong to a deployment, and
that is a separate repository with its own changelog and its own
`docs/OPERATIONS.md`.

That used to be a table in this file, with hostnames and WireGuard addresses in
it, which is how a generic system ends up describing one site.

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
| — | the hourly production check moved to the deployment repository; §6 |
| `check.sh` | the laptop pre-push set |
| the deployment repo's `deploy/run-tests.sh` | everything, on the build host. Found by `tools/find-deployment.sh` |

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

Run everything: `./tools/merge.sh <branch>`, which finds the deployment
repository and runs its `deploy/run-tests.sh`.

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

**ACCEPTED, owner's decision 2026-09-13.** Not an open question. What it costs
is recorded so it can be re-read when upstream releases the fixes and the fork
can be dropped for a tag. Both PRs are unmerged and both
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
- ~~**§10.5, compression.**~~ **SETTLED 2026-09-13: m6-http is a cache, not a
  transformer.** The documents were wrong, not the code: brotli and flate2 live
  in m6-core, the proxy has no compressor, and it negotiates between and caches
  the representations a backend produced. Protocol §3.6 now says so and §3.6.1
  adds `[[backend]] compresses = <bool>` to site.toml, **read by both sides**.
  The edge uses it to decide whether to add `Vary: Accept-Encoding`; the backend
  refuses to start if it disagrees, exiting 2 before binding. Default true,
  because every backend here is built on core. The quiche fork is accepted too,
  so neither of these is an open question any more.

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
| 5 | **Deploy, lifting the freeze** | the deployment repository's business; §6. Not a code task. |

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

## 6. Operations: the fleet, the deploy, the hourly check

**Not here. m6 is a generic web system and does not know whose fleet it is
running on.** Those sections used to live in this file, which meant a generic
system's handover carried one particular deployment's node names, WireGuard
addresses and analytics paths.

They are now in the deployment repository, as `docs/OPERATIONS.md`: the fleet
table, the m6-file config-and-binary ordering hazard, and the hourly health
check with its traps. `deploy/health-check.py` moved there with them.

What stays m6's business is in §4: whether the checks are honest, and what they
measure.

## 7. The dozen lessons that come up most

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

## 8. Open questions, honestly unresolved

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
