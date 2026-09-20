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

- **THE FREEZE IS OVER. m6 1.10.0 is deployed to production**, 2026-09-19, on
  all three nodes (syd, lon, chi), md5 `17ef4e5bef36e36205230cf21db5faa6`, 19/19
  checks each. It carries 1.9.0 (the TLS session ticketer) and 1.10.0 (push and
  103 Early Hints removed). The lines above this one said "do not deploy, 135
  commits are undeployed" until that day; that is history now, and the next
  release is an ordinary release.
- **#105 is written, green and NOT merged.** PR
  [#106](https://github.com/mgrosvenor/m6/pull/106) into `develop`, branch
  `feat/105-health-reports-binary-hash`, four commits, CI green and the full
  build-host gate passed (1145 tests, clippy silent, cargo-deny ok including the
  new `md-5`, h1/h2/h3 and performance ok, examples and the CMS end-to-end
  suite green).

  `PerfReport`'s `version: String` becomes `build: BuildId { name, version, hash }`,
  so `/perf` says which BUILD is running and not only which release it claims to
  be. `/health` is untouched and still publishes exactly `status` and `node`: a
  bare hash there was proposed, and rejected because an opaque number on its own
  tells a reader nothing. `m6-monitor` reports build drift when versions agree
  and hashes differ, which is the 2026-09-20 case a version comparison cannot
  see. Nothing breaks: `serde(default)` on both sides, and no other consumer read
  the field.

  It is held because the deployment repository is mid-refactor. Rolling it needs
  a staging monitor, and staging has never had one (site #89). Merging and
  releasing 1.11.0 is safe whenever that is ready.
- **`main` IS what is running**, as of this release, which is the point of the
  branch model. Confirm it the same way as always: the newest entry in the
  deployment repository's `docs/RELEASES.md` names the commit, and
  `deploy/estate/prod.json` there records the md5 running on each node.
- **A ROLLOUT THAT REMOVES A WIRE FEATURE MUST DEPLOY THE ORIGIN FIRST.** Issue
  #100, found the hard way on 2026-09-19 and still open. The deploy takes edges
  before the origin, which is right for risk and wrong for this: a 1.10.0 edge
  fetching from a 1.8.1 origin answered **502 on about one miss in five**,
  because the origin still emits `103 Early Hints` and `forward.rs:1670` lists
  204, 304, 100 and 101 as bodyless but not 103. Nine 502s reached European
  visitors. Staging cannot catch it: it converges all three instances in one run
  and never holds a mixed-version fleet.
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
| `main` | releases only. Advances **only** by a pull request from `develop`, then a tag. |
| `develop` | where work is integrated, **by pull request** |
| `<type>/<issue>-<slug>` | one branch per issue, off `develop` |

Types: `feat` `fix` `perf` `docs` `refactor` `test` `chore`.

`.githooks/pre-push` (version-controlled, via `core.hooksPath`) refuses a direct
push to `main`, a release tag that is not a version or is pushed without
`M6_RELEASE=1`, and a work branch whose name carries no issue number. It is
milliseconds: the content checks are CI's job, and a hook that took ten minutes
killed every push with SIGPIPE by idling out git's connection. Do not put a suite
back into it.

```sh
./tools/branch.sh 42 some-slug --type fix   # off develop; checks the issue exists
git push origin fix/42-some-slug
gh pr create --base develop --fill          # CI runs the full set
# merge on GitHub

# and to release:
# bump Cargo.toml, write the CHANGELOG entry
gh pr create --base main --head develop --title "Release 1.1.0"
# merge on GitHub, then
git checkout main && git pull && ./tag.sh v1.1.0
```

**Run the build host before opening a pull request** when the change could touch
performance or conformance, because a shared runner cannot measure wall-clock and
CI's h3spec is not the same as a quiet Linux box:

```sh
M6_BUILD_HOST=root@<box> M6_BUILD_SSH_OPTS='-p 4022' ./tools/build-host-tests.sh
```

It takes ten to fifteen minutes and runs everything: m6's build, warnings, clippy,
cargo-deny, the whole suite, h1/h2/h3 conformance, the performance check, then the
examples repository and its CMS end-to-end suite.

### The tools

| tool | what it does |
|---|---|
| `tools/branch.sh` | start work; confirms the issue exists with `gh` |
| `tools/clippy.sh` | clippy, `-D warnings`. No ceiling, no `--update` |
| `tools/conformance.sh` | h1/h2/h3 against recorded minimum scores |
| `tools/perfcheck.sh` | two page renders from the examples, against recorded numbers, 20% margin |
| `tools/build-host-tests.sh` | **everything, on the build host.** Build, warnings, clippy, cargo-deny, tests, h1/h2/h3 conformance, the performance check, then the examples repository and its CMS end-to-end suite. Run it before opening a pull request |
| `tag.sh` | tag a release already merged into main, and push it. Checks the branch, the tree, the version against Cargo.toml, the changelog entry, and that the tag is free. Runs no suite: the pull request already did |
| — | the hourly production check moved to the deployment repository; §6 |
| `check.sh` | the laptop pre-push set |
| the deployment repo's `deploy/run-tests.sh` | the deployment's own half: its renderers, its content, its rendered configs. It no longer runs m6's |

**Set the build host in your shell.** `tools/build-host-tests.sh` refuses without
it and says so rather than quietly checking nothing:

```sh
export M6_BUILD_HOST=root@<your-linux-box>
export M6_BUILD_SSH_OPTS='-p 4022'        # if it is not on 22
```

The address is deliberately **not** written down in m6: which machine builds this
is a property of whoever is working here, not of a generic web system. It is in
the deployment repository's `docs/OPERATIONS.md`, which is where the rest of the
infrastructure lives. A merge attempted without it fails before touching anything,
which is the right outcome and was confirmed by doing it.

**`tools/find-deployment.sh` is gone**, along with `merge.sh` and `release.sh`.
All three used it to locate a deployment repository and run its
`deploy/run-tests.sh`, which had it backwards: a release of m6 cannot depend on
somebody's site being checked out beside it, and a bare checkout could not check
itself. All three now use m6's own runner and the examples.

### The examples repository is part of the checks

`m6-examples`, checked out **beside** this one (the renderer crates reach m6 by
relative path, so they have to be siblings). `tools/build-host-tests.sh` builds it
against the m6 tree it just built, parses every example's config with the real
m6-http, and runs example 05's end-to-end suite over the whole running stack.

This is not optional courtesy to the examples. It is the only code in the checks
that **uses** m6's interfaces, and nothing had ever built it: by 2026-09-14 it did
not compile at all, and underneath that were five more defects nobody could see.
Read lesson 41. `M6_SKIP_EXAMPLES=1` runs without it and says what that leaves
unchecked.

**It earned itself within the hour.** The first Linux run found that
`Request::touch` -- m6's documented way for a renderer to invalidate the edge --
had never worked on Linux, because `utimensat` reports `IN_ATTRIB` and the
inotify mask did not ask for it. Four watcher tests passed throughout, because
every one of them wrote bytes. Lesson 44.

**GitHub Actions runs the examples too**, as the `examples` job in
`.github/workflows/ci.yml`, and `m6-examples` has its own workflow building the
other direction against m6's `develop`. Before 2026-09-14 neither existed, so a
push could break every example and CI stayed green.

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

**m6 1.10.0, on all three production nodes, since 2026-09-19.** The tag is
`v1.10.0`; the artefact is md5 `17ef4e5bef36e36205230cf21db5faa6`, and
production promoted the binary staging had run rather than rebuilding it,
because Rust is not byte-reproducible and a rebuild would make the staging pass
prove nothing.

Recompute what is unreleased, never trust a number written here:

```sh
git -C ~/m6 log --oneline v1.10.0..develop | wc -l
```

**The site side is now recorded too**, which it was not when this section said
it could not be. `deploy/estate/prod.json` in the deployment repository holds,
per node, the m6 version, the running md5 and the rollback md5, and its
`global.build` block holds the site commit and the m6 tag. It is generated by
`ops.sh capture prod` and committed, so "which content and configs are live" has
an answer that is diffable rather than an estimate.

Three production changes were applied during the freeze on explicit
instruction: systemd hardening fleet-wide, the firewall block ledger
reconciled to 32 identical rules, and one address blocked. Those changed
confinement and firewall rules, not what code runs.

---

## 4. Health of the checks

Run everything: `./tools/build-host-tests.sh`, which finds the deployment
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
| TLS resumption, h1 / h2 / h3 | **gated**, added 2026-09-19 | CI, build host, laptop |
| performance | `render:capabilities` 2,170,589 ns | build host only |

### Resumption is gated now, and the reason is worth reading

`tools/conformance.sh resume` checks that a client offering a session ticket
gets a resumed handshake, on **all three protocols**: h1 and h2 through rustls'
TLS 1.3 tickets, h3 through QUIC's own, because a ticketer proven on TCP says
nothing about QUIC.

It exists because 1.9.0 installed a ticketer and **nothing could tell whether it
worked**. The production monitor read 0% resumed on `http/2/external` across all
three nodes for a day after the release, which looks identical whether the
ticketer is broken or the traffic simply has no returning connections. Issue
#101, now closed: the server resumes fine on every protocol, the counter is
accurate, and the 0% was the traffic. A browser opens ONE h2 connection per
visit and multiplexes it, so resumption needs a return visit inside the ticket's
lifetime; the `http/1.1` channel reads 88-96% only because its repeat client is
a monitor polling on a loop.

**Expect 0% on `http/2/external` in production and do not read it as a fault.**

This is also the only conformance stage that needs no external tester, so it is
the first real protocol check a laptop run has ever performed: h2spec and h3spec
are not installed there and every other stage skips.

And the tool lied first, which is the part to remember. `probe::tls_handshake`
stopped reading the instant the handshake completed, and TLS 1.3 sends
`NewSessionTicket` AFTER Finished, so rustls never stored one and the probe
reported `RESUMPTION: none` against every node. That reads as a server defect.
It was the measuring tool reporting a property of itself. See `docs/LESSONS.md`.

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
matching the newest tag. Bump them in `Cargo.toml` in the release pull request.

| # | item | notes |
|---|---|---|
| 1 | ~~**clippy to zero**~~ | **done 2026-09-13, issue #5.** 0 on both toolchains, and `clippy.sh` is `-D warnings` with the ceiling files deleted. §4 |
| 2 | ~~**quiche 0.26.1 → 0.29.3, re-measure h3**~~ | **done 2026-09-13, issue #4.** The bump alone moved nothing. h3 is now **47/49** on a fork of quiche master carrying PRs #2521 and #2575, floor raised to 47. The last two are QPACK and are accepted, not chased. §4 |
| 3 | **Phase 7: renderers onto a git tag** | below |
| 4 | ~~**Phase 8: six `/status` implementations**~~ | **done 2026-09-13, issue #6.** All six conform, 13 tests in the gate, Go installed. The measurement: **linking m6-core costs 36% of throughput and +37us p50**, 8.8x RSS, 56.7x binary. §4 below |
| 5 | ~~**The examples are built by the checks**~~ | **done 2026-09-14, issue #11.** They did not compile at all, and five more defects were underneath that. `build-host-tests.sh` now builds them and runs example 05's end-to-end suite; `find-deployment.sh` is deleted. See §2 and lesson 41 |
| 6 | ~~**Deploy, lifting the freeze**~~ | **done 2026-09-19.** m6 1.10.0 on all three production nodes, md5 `17ef4e5bef36`, 19/19 checks each, promoted rather than rebuilt. The rollout found one platform defect, #100, still open: a 1.10.0 edge answers 502 when a backend sends a 103, which cost nine 502s to European visitors because the deploy takes edges before the origin. §0 |

**The two performance numbers are recorded.** `render:capabilities` measured a
deployment's content and is removed; `render:minimal` (201827ns) and
`render:blog-index` (1784823ns) replace it, first measured on the build host on
2026-09-14 at load 0.20. Three rounds each, median recorded rather than best,
with all readings and the spread in `tools/perf-baseline.txt`. Two further rounds
pass against them. That file also explains why the old number was not translated
across: neither target renders the same page from the same bytes.

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
- **`FrameworkState::build_dict` is private**, and that is now a decision
  rather than a debt: nothing outside `App` builds a request dictionary, and the
  ordering is documented in `app`'s module doc and in
  `docs/m6-core-reference.md`. See `docs/CONSOLIDATION-TODO.md` §1.
- **The IO layer, the event loop and the handler contract are deferred**,
  explicitly, by the owner. Not 1.0 work. See `docs/CONSOLIDATION-TODO.md` §3b
  and do not widen that scope.
