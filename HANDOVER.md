# Handover

**Written for someone with no prior context.** Read this, then
`docs/CONSOLIDATION-TODO.md`. This file is what is true now; that one is the
ledger of what is done and what is owed.

Last rewritten 2026-09-13. Previous versions of this file accreted session
notes; those are now in `docs/SESSION-NOTES.md` and the lessons in
`docs/LESSONS.md`.

---

## 1. What this is

`m6` is an HTTP stack in Rust: an edge (`m6-http`: TLS, HTTP/1.1, HTTP/2,
HTTP/3, a cache, proxying) and services behind it built on a shared library
(`m6-core`). It serves **mgrosvenor.com** from three nodes.

The site lives in a **separate repository**, `~/dr-grosvenor-site`, holding the
content, the production configs, the deploy scripts, and three small renderer
crates of its own. Both repositories are part of most changes, they deploy
separately, and they can disagree. That is currently the largest release risk;
see §6.

### The fleet

| node | role | service | WireGuard | analytics file |
|---|---|---|---|---|
| syd | origin | `m6-http-origin` | 10.0.0.1 | `/var/www/dr-grosvenor-site/logs/analytics.ndjson` |
| lon | cache | `m6-http-cache` | 10.0.0.4 | `/var/www/m6-cache/logs/analytics.ndjson` |
| chi | cache | `m6-http-cache` | 10.0.0.5 | `/var/www/m6-cache/logs/analytics.ndjson` |

All three are **1-core VMs**; syd has 950MB. The build host is a separate
4-core Linux box at `root@45.63.29.146` **port 4022**. It is **not backed up**,
so everything done to a node must be in git.

### The architectural rule

**m6-core is the PHP of m6: a box of blocks a service is assembled from.**
Anything that generalises belongs in core, and core should be the only thing a
service links. A service being nearly a no-op on top of core is the goal, not a
smell:

```rust
use m6_core::prelude::*;
fn main() -> anyhow::Result<()> { App::new().run()?; Ok(()) }
```

---

## 2. Read these before doing anything

1. **`CLAUDE.md`** — the branch model, what has to pass, the standing rules.
   The git hooks enforce most of it.
2. **`docs/PERFORMANCE.md`** — every performance number, how it was measured,
   on what. Do not quote a performance number from anywhere else.
3. **`docs/LESSONS.md`** — each one cost something. §8 here has the dozen that
   come up most.

### How to write for the owner

- **No em dashes.**
- **No agile or consultant vocabulary.** He is an old-school Unix engineer and
  said so plainly. Say "a recorded minimum", not the other word. A latency
  spike is fine; an investigation is not a "spike".
- Commit messages say what changed, why it mattered, and **how it was
  verified**. Verified means measured against something running, not inferred
  from the source.
- He reads carefully and pushes back on hand-waving. Give him the number.

---

## 3. Where the work is, exactly

### Branches

**The branch model is new, adopted 2026-09-13.** `main` is releases only;
`develop` is where work is integrated; work happens on `<type>/<issue>-<slug>`
branched from `develop`. `.githooks/pre-push` refuses anything else and is
version-controlled via `core.hooksPath`.

| repo | branch | state |
|---|---|---|
| m6 | `main` | `ead669b`. **Holds ~130 undeployed commits** because the model arrived mid-project. Do **not** read `main` as "what is running". |
| m6 | `develop` | `697d7f8`, pushed |
| m6 | `chore/1-ci-on-github-actions` | `7b7076d`, pushed, **CI green, NOT MERGED** |
| site | `develop` | `0b04e67`, **2 commits ahead of origin, unpushed** |
| site | `main` | `e9f11c2` |

**Do these two things first:**

```sh
git -C ~/dr-grosvenor-site push                        # develop is unpushed
cd ~/m6 && ./tools/merge.sh chore/1-ci-on-github-actions
```

The merge runs everything on the build host and takes **more than ten
minutes**. It was started once and timed out. Nothing was lost and nothing was
half-merged; just run it again and let it finish.

### The cycle, once that is done

```sh
./tools/branch.sh <issue> <slug> --type fix   # branches off develop
# work, commit
git push origin fix/<issue>-<slug>            # fast local checks
./tools/merge.sh fix/<issue>-<slug>           # everything, then merge
./tools/release.sh 1.0.0                      # develop into main, changelog, tag
```

### What is deployed

**Nothing since 2026-09-10.** The deployed commit is whatever the newest entry
in `~/dr-grosvenor-site/docs/RELEASES.md` names, currently m6 `22ee3a4`.
Recompute, never edit in place:

```sh
git -C ~/m6 log --oneline 22ee3a4..develop | wc -l
```

**130 m6 commits and 24 site commits are undeployed.** The freeze holds until
the consolidation work is finished. Three production changes were applied
during it on explicit instruction (systemd hardening, the firewall block ledger
reconciled, one address blocked); those changed confinement and firewall rules,
not what code runs.

---

## 4. Health of the checks

Run the whole set: `cd ~/dr-grosvenor-site && ./deploy/run-tests.sh m6`
(build host, several minutes).

| check | state | where |
|---|---|---|
| tests | **1022 passing**, 0 failures | everywhere |
| compiler warnings | **0**, release and test builds | Linux, enforced |
| clippy | **135 macOS / 145 Linux** | `tools/clippy.sh` |
| `cargo fmt` | clean | CI, `check.sh` |
| `cargo deny` | clean, 5 advisories listed as exceptions | CI |
| h1 conformance | **32/32** on all four targets | CI, build host |
| h2 conformance | **146/146** | CI, build host |
| h3 conformance | **37/49 — RED, see below** | CI, build host |
| performance | `render:capabilities` 2,170,589 ns | build host only |

### Two of those are not what they look like

**h3 is 37/49, which is a failing protocol implementation reported as PASS**
because 37 is what is written in `tools/conformance-scores.txt`. That is
recording today's failure as the standard. All twelve failures are QUIC
transport parameter validation, packet reserved bits, and QPACK — **every one
inside `quiche`**, which m6-http uses for HTTP/3 and which is pinned to **tag
0.26.1** while upstream is **0.29.3**. m6's own code sits above that layer.
Upgrading quiche is the first thing to try and costs one CI run.

**clippy at 135 should be zero.** The arrangement is "the count may fall and
may never rise", which is living with the number rather than removing it. Most
are mechanical and `cargo clippy --fix` handles a large share: 18 collapsible
`if` inside a `match`, 15 complex types, 9 `write!` ending in a newline, 6
`while let`, 6 `Error::other`, then a long tail.

### Where the checks run, and why it matters

- **CI** (GitHub Actions, every push and PR): build, tests, warnings, clippy,
  fmt, h1/h2/h3 conformance, cargo-deny, MSRV.
- **Build host** (`deploy/run-tests.sh`): all of that plus the **performance
  check**, which is not on CI because a wall-clock measurement on a shared
  runner measures the runner.
- **Laptop** (`check.sh`, pre-push): the fast subset. It passes
  `--allow-missing-tools` to the conformance script because h2spec and h3spec
  are not installed there, and the output says loudly what it did **not** test.

---

## 5. The road to 1.0

**1.0 is not cut until the consolidation work is done.** Owner's decision,
recorded beside the version in `Cargo.toml`. All nine crates are at 0.2.0,
matching the newest tag; `tools/release.sh` bumps them at a release.

In the agreed order:

1. **Drive clippy to zero.** Cheapest, asked for directly.
2. **Upgrade quiche 0.26.1 → 0.29.3, re-measure h3.** If the twelve clear, h3
   is genuinely green. If not, they are upstream bugs worth reporting to
   cloudflare/quiche with the h3spec output.
3. **Point the site's renderers at m6 as a git dependency pinned to a tag**,
   instead of `path = "../../m6/m6-core"`. This is Phase 7. **Not crates.io**:
   the owner's call, 2026-09-13. Publishing would mean committing to a public
   API, a name, and maintenance for other people, none of which this project
   wants. A tag gives the versioning without any of that, and m6-http already
   depends on quiche exactly this way.

   ```toml
   m6-core = { git = "https://github.com/mgrosvenor/m6", tag = "v1.0.0" }
   ```

   **Gate:** the site builds with no `m6` checkout beside it, and `deploy.sh`
   stops syncing the tree to the build host.
4. **Phase 8: six implementations of the same `/status` payload.** Needs Go on
   the build host, which is `apt install golang`. Its purpose is the
   measurement that says whether linking core costs or saves, which 1.0 should
   be able to answer.
5. **Deploy, lifting the freeze.** §6. Not a code task.

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
runs. **`--dump-config` now exists on every service** and `deploy-platform.sh`
validates with it before installing, so the second case is a refused deploy
rather than a silent outage. The first is not covered. **Write a combined step
before touching production.**

### Verify after deploying: deliberate behaviour changes

- socket modes are **0660** (were 0755 from umask, or 0666 by hand)
- path traversal answers **404** where a single-segment parameter gave 400
- m6-file **sheds with 503** past a 256-deep queue instead of queueing without
  bound. Production sets `size = 32`, so the queue is 256. **Watch the gallery
  page**, which fires dozens of concurrent image requests and is why the pool
  was widened to 32 in the first place.
- a route's `Cache-Control` is now a **default, not an override**
- a **failed bind is fatal** where it used to warn and continue
- `UMask=0027` and `LimitNOFILE=65535` on every service

### Known gaps in the deploy path

- **Staging cannot exercise the cache role.** Single origin, no cache nodes, no
  WireGuard. The 90-second London outage on 2026-09-11 was a cache-role fault
  and staging would have called that change safe.
- **`m6-monitor` is installed on no machine**, not even the build host, and
  cannot replace `tools/health-check.py` yet: `--check` reads `/traffic`, which
  404s on the deployed binary, and `/perf`, whose deployed shape has no `pools`
  field. Measured on syd 2026-09-12.
- **The firewall stats collector** is written, unit-tested, and on no node.
  Until it is deployed `/traffic` reports `firewall: null`.

---

## 7. The hourly health check

**`tools/health-check.py --load` is the standing order.** It covers all three
nodes, which the syd-only commands in the prompt do not, and it encodes the
traps: per-role analytics paths, the nested record shape, ANSI stripping,
generated-versus-observed labelling, forged-bot detection.

**It is not scheduled.** A cron created from a session dies with it; a durable
version needs launchd or a real crontab.

### Three things the prompt's baselines get wrong

1. **`hit_p50_ns` is load-dependent and not comparable across days.** On a
   near-idle single-core VM the cache-hit path goes cold between requests, so
   the number tracks request density. Same binary, same node, minutes apart:
   50-70 hits per window reads **3,900ns**; 1,200 hits reads **1,064ns**, below
   the 1.7-2.2us band everyone treated as the baseline. **Report the window's
   hit count beside the number.** The prompt still states 1.7-2.2us flat; it is
   the owner's to change.
2. **A hit rate near 0.3 usually means scan volume, not a regression.** A 404
   is uncacheable and counts as a miss. Separate last-hour HIT/MISS from the
   404 share. chi has read 72% 404s in an hour with real traffic fine.
3. **Crawler totals over windows longer than ~60 minutes are understated.** The
   user-agent rotation heuristic flags the backbone addresses 10.0.0.4 and
   10.0.0.5 as forging bot agents, because a cache node relays real clients'
   agents, then excludes those requests. Known tool bug, not an incident.

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

Full list in `docs/LESSONS.md`.

1. **A check that cannot measure must fail, not pass.**
   `tools/conformance.sh` broke this four ways and reported success through all
   of them. h2 and h3 were untested for months and nothing said so.
2. **A gate that runs only where it is convenient is not a gate.** The only
   thing running conformance was a laptop hook, on a machine where neither
   h2spec nor h3spec is installed. h1 was not running on the build host either,
   because `uvx` was installed but not on the gate's PATH.
3. **Recording today's failure as the standard is the same error in a new
   costume.** h3 37/49 reports PASS because 37 is written in a file.
4. **Measure the candidate before consolidating onto it, and measure its cost,
   not only its features.** What blocked m6-file's migration was not a missing
   capability but `App` spending 0.63ms per page copying its own config.
5. **A synthetic benchmark measures the shape you imagined.** The first copy
   figure was 3.08us from a 20-key config; the real config loads a 68KB JSON
   file twice, making it ~323us. When a number looks too big for what it claims
   to measure, that gap is the finding.
6. **`testkit::binary()` prefers `target/release`** and will hand a test a
   binary from yesterday. Run `cargo build --workspace --release` before
   `cargo test`.
7. **A doc comment that justifies a decision by naming a premise becomes a lie
   the day the premise changes.** `validate_path_param` explained why slashes
   were impossible; `Segment::Wildcard` falsified it, and the one capture
   defined to hold slashes was answered 400.
8. **The matcher is not the wire.** Six wildcard tests all stopped at
   `match_route`, so a feature marked done had never worked end to end.
9. **Confinement must claim only what the role actually has.**
   `ReadWritePaths=/run/m6` in a shared systemd fragment took London off the
   air: a cache node has no `/run/m6`, and an absent target fails mount
   namespace setup outright. Never use a `-` prefix to excuse it.
10. **Kill by PID.** Never `pkill -f` naming a port or config path. This laptop
    runs the owner's own dev and preview servers, and it sits at load 20-30.
11. **Counts rank a source; identity decides what it is.** An address sending
    371 refused requests over three hours was reported as the day's strongest
    attacker three times. One field settled it:
    `UA: Amazon-Route53-Health-Check-Service`.
12. **Run the suite to a file and grep the file, never the pipe.**
    `cargo test --workspace > /tmp/run.txt 2>&1`.

---

## 9. Open questions, honestly unresolved

- **`m6-auth-cli`'s `test_token_create_prints_jwt`** fails intermittently and
  has never been explained. Did not recur on 2026-09-12 or -13.
- **A port race in the e2e suites.** `Address already in use` on m6-http's TCP
  listener, seen after `SO_REUSEADDR` was believed to have closed it. Clean on
  re-runs.
- **Four `cargo deny` advisories** listed as exceptions in `deny.toml`, tracked
  in issue #3. **The reachability of each has not been established**; that
  issue was written before checking, which is the same mistake made with the
  hpack one. A fifth, hpack's decoder panic, **was** checked and is not
  reachable: `validate_hpack_block` rejects malformed blocks before the decoder
  sees them, pinned by `http2::hpack_robustness`.
- **GitHub issues**: #1 (CI, effectively done, close when the branch merges)
  and #3 (the advisories above).
- **GitHub branch protection on `main` is not set.** The hooks protect this
  laptop only. Setting it needs the owner's go-ahead because it changes how the
  repository behaves for everyone.
