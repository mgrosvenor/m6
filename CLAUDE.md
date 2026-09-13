# Working on m6

House rules. They are enforced by git hooks and by the checks in `tools/`, not
by anyone remembering them.

---

## Branches

| branch | what it is |
|---|---|
| `main` | Releases only. What the world sees on GitHub. It advances **only** by a merge from `develop`, made by `tools/release.sh`, with a CHANGELOG entry and a tag. |
| `develop` | Where work is integrated. Everything branches from here and merges back here. |
| `<type>/<issue>-<slug>` | One branch per issue. Created by `tools/branch.sh`. |

Types: `feat` `fix` `perf` `docs` `refactor` `test` `chore`.

**Nothing is pushed to `main` by hand.** The pre-push hook refuses it unless
`tools/release.sh` is doing it.

> **Note on the transition, 2026-09-13.** `main` currently holds ~120 commits
> that have never been deployed, because this model was adopted mid-project.
> From the next release onwards `main` only advances at a release. Until then,
> do not read `main` as "what is running": the deployed commit is whatever the
> newest entry in `~/example-site/docs/RELEASES.md` names.

### The cycle

```sh
./tools/branch.sh 42 h3-gate-measures-nothing --type fix   # from develop
# ... work, commit ...
git push origin fix/42-h3-gate-measures-nothing            # runs the local checks
./tools/merge.sh fix/42-h3-gate-measures-nothing           # full checks, then merge
git push origin develop
```

and when it is time to ship:

```sh
# write the CHANGELOG entry first
./tools/release.sh 0.3.0
```

---

## What has to pass

**Before a branch merges into `develop`**, on the build host, all of it:

| check | what it means | where |
|---|---|---|
| unit + integration tests | the whole workspace, zero failures | `cargo test --workspace` |
| compiler warnings | **zero**, release and test builds, pre-existing included | Linux |
| clippy | at or under the recorded count; the count may fall and may never rise | `tools/clippy.sh` |
| h1, h2, h3 conformance | at or above the recorded scores | `tools/conformance.sh` |
| performance | within margin of the recorded number | `tools/perfcheck.sh` |

`tools/merge.sh` runs all of it through `deploy/run-tests.sh` on the build
host, and records what it ran in the merge commit. **The pre-push hook refuses
a merge commit on `develop` that carries no such record**, so this is not a
convention that can be quietly skipped.

### Why the build host and not the laptop

Both the conformance and performance checks measure things a busy machine
distorts. This laptop runs at load 20-30 with dev servers and preview
instances on it, and the performance check failed its own second run because a
release build was going at the same time: readings moved by half. The
conformance testers (`h2spec`, `h3spec`) are not installed on the laptop at
all, and for months that showed up as "skipped" and was read as a pass.

So: the laptop's pre-push hook runs the fast checks and says plainly what it
did **not** test. The build host runs everything, and it is what gates a merge.

### A check that cannot measure must fail

This is the rule the conformance script broke four different ways, reporting
success each time. If a check cannot run its measurement, it fails. It does
not skip, and it does not print a number it did not take. See
`tools/conformance.sh`'s header for the four cases and what each one cost.

---

## Standing rules that do not lapse

- **Never use em dashes** in prose written for the owner.
- **No agile or consultant vocabulary.** Plain technical English. Say "a
  recorded minimum", not the other word.
- **Zero compiler warnings**, pre-existing included, checked on Linux.
- **Test locally, commit, then deploy.** Never deploy from an uncommitted tree.
- **Secrets never enter git.** Only `.example` files, paths, documented shape.
- Any change to **layout, copy, or rendering** needs individual approval before
  it ships.
- Image resizing is the owner's job.
- Firewall blocks are **per-IP only**, no CIDR rules.
- **Blocking an address is a write.** Propose candidates; never apply without
  being asked.
- The build host is **not backed up**. Everything done to a node is in git.
- **Keep allocations to a minimum. Latency is the metric that matters.**
- "Clean and consistent is the only way forwards. Apps should deviate only
  where functionality demands it."
- "Clean simple code with lots of reuse out of core. This is not the place to
  get clever or inventive."

### The architectural rule

**m6-core is the PHP of m6: a box of blocks a service is assembled from.**
Anything that can reasonably be expected to generalise to other sites belongs
in core, and core should be **the only thing a service links**. A default
service being nearly a no-op on top of core is the result we want, not a smell:

```rust
use m6_core::prelude::*;
fn main() -> anyhow::Result<()> { App::new().run()?; Ok(()) }
```

---

## Commit messages

Say what changed, why it mattered, and how it was verified. "Verified" means
measured against something running, not inferred from the source: a good number
of the defects in `CHANGELOG.md` were invisible in the code and only showed up
against a deployed artefact.

A commit that fixes a defect should say what the defect actually did, in terms
of what a user or an operator would have seen. A commit that changes a recorded
minimum or a performance number must argue for it.

Every commit ends with:

```
Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
```

---

## Where to read next

| file | what it holds |
|---|---|
| `HANDOVER.md` | **start here.** What is true right now, written for someone with no prior context |
| `docs/CONSOLIDATION-TODO.md` | what is done and what is owed, with the 1.0 list at the top |
| `docs/PERFORMANCE.md` | every performance number, how it was measured, on what |
| `docs/LESSONS.md` | the things that cost something to learn |
| `docs/SESSION-NOTES.md` | point-in-time records. Not maintained; the handover wins |
| `docs/m6-core-reference.md` | every core module and its interface |
| `CHANGELOG.md` | what changed in each release |

`HANDOVER.md` is what is true. Where it and the ledger disagree, it wins.
