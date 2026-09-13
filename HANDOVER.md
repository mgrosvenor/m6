# Handover

**Written for someone taking over with no prior context.** Read this file top
to bottom before touching anything. It is what is true right now.

Then: `CLAUDE.md` for the rules, `docs/CONSOLIDATION-TODO.md` for what is owed.

Last rewritten 2026-09-13.

---

## 0. If you read nothing else

- **Do not deploy.** 133 m6 commits and 24 site commits are undeployed behind a
  deliberate freeze. Lifting it has a known hazard: §6.
- **`main` is not what is running.** It holds all of that undeployed work. The
  deployed commit is whatever the newest entry in
  `~/dr-grosvenor-site/docs/RELEASES.md` names.
- **h3 conformance is 37/49 and reports PASS.** That is a failing protocol
  implementation with today's failure recorded as the standard. §4.
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
| `tools/clippy.sh` | clippy against a recorded count, per platform |
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
| m6 | `main` | `ead669b`, **133 commits behind develop**, none of it deployed. Not what is running. |
| m6 | `develop` | `9abfebe`, the CI branch merged in with everything passing |
| site | `main` | `e9f11c2` |
| site | `develop` | `0b04e67`, pushed |

**CI has never run on `develop`.** It was added on a branch. The first push to
`develop` will be its first run there.

### What is deployed

**Nothing since 2026-09-10**, m6 `22ee3a4`. Recompute, never edit in place:

```sh
git -C ~/m6 log --oneline 22ee3a4..develop | wc -l
```

Three production changes were applied during the freeze on explicit
instruction: systemd hardening fleet-wide, the firewall block ledger
reconciled to 32 identical rules, and one address blocked. Those changed
confinement and firewall rules, not what code runs.

---

## 4. Health of the checks

Run everything: `cd ~/dr-grosvenor-site && ./deploy/run-tests.sh m6`.

| check | state | where it runs |
|---|---|---|
| tests | **1022 passing**, 0 failures | everywhere |
| compiler warnings | **0**, release and test builds | Linux, enforced |
| clippy | **135 macOS / 145 Linux** | `tools/clippy.sh` |
| `cargo fmt` | clean | CI, `check.sh` |
| `cargo deny` | clean, 5 advisories as recorded exceptions | CI |
| h1 conformance | **32/32** on four targets | CI, build host |
| h2 conformance | **146/146** | CI, build host |
| h3 conformance | **37/49 — RED** | CI, build host |
| performance | `render:capabilities` 2,170,589 ns | build host only |

### h3 is red and reports PASS

`tools/conformance-scores.txt` records 37, so 37/49 passes. That is recording
today's failure as the standard, and it should not stand.

All twelve failures are QUIC transport parameter validation, packet reserved
bits, and QPACK. **Every one is inside `quiche`**, which m6-http uses for
HTTP/3 and which is pinned to **tag 0.26.1** while upstream is **0.29.3**. m6's
own code sits above that layer. Upgrading is one dependency bump and one CI run
to find out whether it clears them.

### clippy at 135 should be zero

The arrangement is "the count may fall and may never rise", which is living
with the number. Investigated, not assumed:

- `cargo clippy --fix` applies about **30 of the 135**.
- **18** collapsible `if` inside a `match`: mechanical, but each touches real
  logic and wants reading.
- **15** "very complex type": these are the
  `Arc<dyn Fn(&Request, &G, &mut T) -> Result<Response> + Send + Sync>` shapes
  in `App`'s stateful builders. Fixing them means type aliases, which is a real
  readability gain rather than silencing a lint.
- The rest is a long tail: 9 `write!` ending in a newline, 6 `while let`, 6
  `Error::other`, and singles.

Reachable, but it is a real branch with diffs across most crates, not one
`--fix` run.

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
| 1 | **clippy to zero** | §4 has the breakdown |
| 2 | **quiche 0.26.1 → 0.29.3, re-measure h3** | one bump, one CI run |
| 3 | **Phase 7: renderers onto a git tag** | below |
| 4 | **Phase 8: six `/status` implementations** | `apt install golang` on the build host, nothing more. Its purpose is the measurement that says whether linking core costs or saves. |
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

Full list, all 49, in `docs/LESSONS.md`.

1. **A check that cannot measure must fail, not pass.**
2. **A gate that runs only where it is convenient is not a gate.** The only
   thing running conformance was a laptop hook on a machine with neither h2spec
   nor h3spec installed. h1 was not running on the build host either, because
   `uvx` was installed but not on the gate's PATH.
3. **Recording today's failure as the standard is the same error in a new
   costume.** h3 37/49 reports PASS because 37 is written in a file.
4. **Measure the candidate before consolidating onto it, and measure its cost,
   not only its features.**
5. **A synthetic benchmark measures the shape you imagined.** The first copy
   figure was 3.08us from a 20-key config; the real config loads a 68KB JSON
   file twice, making it ~323us.
6. **`testkit::binary()` prefers `target/release`** and will hand a test a
   binary from yesterday. `cargo build --workspace --release` first.
7. **A doc comment that justifies a decision by naming a premise becomes a lie
   the day the premise changes.**
8. **The matcher is not the wire.** Six wildcard tests all stopped at
   `match_route`, so a feature marked done had never worked end to end.
9. **Confinement must claim only what the role actually has.**
   `ReadWritePaths=/run/m6` in a shared systemd fragment took London off the
   air: a cache node has no `/run/m6`.
10. **Kill by PID.** Never `pkill -f` naming a port or config path. This laptop
    runs the owner's own dev and preview servers and sits at load 20-30.
11. **Counts rank a source; identity decides what it is.** 371 refused requests
    over three hours was reported as the day's strongest attacker three times.
    One field settled it: `UA: Amazon-Route53-Health-Check-Service`.
12. **Run the suite to a file and grep the file, never the pipe.**

---

## 9. Open questions, honestly unresolved

- **`m6-auth-cli`'s `test_token_create_prints_jwt`** fails intermittently and
  has never been explained. Did not recur on 2026-09-12 or -13.
- **A port race in the e2e suites.** `Address already in use` on m6-http's TCP
  listener, seen after `SO_REUSEADDR` was believed to have closed it. Clean on
  re-runs.
- **Four `cargo deny` advisories** listed as exceptions in `deny.toml`, issue
  #3. **Their reachability has never been established**; that issue was written
  before checking, which is the same mistake made with a fifth. That fifth,
  hpack's decoder panic, **was** checked and is **not** reachable:
  `validate_hpack_block` rejects malformed blocks before the decoder sees them,
  pinned by `http2::hpack_robustness`. Do the same for the other four rather
  than trusting the issue text.
- **GitHub issues**: #1 (CI, done, close it) and #3 (above).
- **GitHub branch protection on `main` is not set.** The hooks protect one
  laptop. Setting it needs the owner's go-ahead because it changes how the
  repository behaves for everyone.
- **`FrameworkState::build_dict` is private**, so the twelve ordered steps of
  dictionary building are not reusable by a service not using `App`.
- **The IO layer, the event loop and the handler contract are deferred**,
  explicitly, by the owner. Not 1.0 work. See `docs/CONSOLIDATION-TODO.md` §3b
  and do not widen that scope.
