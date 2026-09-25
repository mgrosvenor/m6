# Working on m6

House rules. They are enforced by git hooks and by the checks in `tools/`, not
by anyone remembering them.

---

## Branches

| branch | what it is |
|---|---|
| `main` | Releases only. What the world sees on GitHub. It advances **only** by a pull request from `develop`, with a CHANGELOG entry, then a tag. |
| `develop` | Where work is integrated. Everything branches from here and comes back **by pull request**. |
| `<type>/<issue>-<slug>` | One branch per issue. Created by `tools/branch.sh`. |

**A pull request is the only way into `develop`, and the only way from `develop`
to `main`.** Owner's decision, 2026-09-14. `tools/merge.sh` and
`tools/release.sh` merged locally and are deleted.

Types: `feat` `fix` `perf` `docs` `refactor` `test` `chore`.

**Nothing is pushed to `main` by hand.** The pre-push hook refuses any direct
push to it. `main` moves only when a pull request is merged on GitHub; the tag
that follows is pushed by `./tag.sh`, which sets the one variable the hook accepts.

> **The transition is over.** This note said `main` held ~120 undeployed commits
> from the model being adopted mid-project, and that `main` could not be read as
> what is running. That stopped being true at 1.0 on 2026-09-14: `main` advances
> only at a release, and it is the release line.
>
> **`main` is still not the same fact as what is deployed, and it never will
> be.** Releasing and deploying are separate decisions and the deployment
> repository owns the second one: it pins a tag, and a release with nothing in it
> for a node to run is deliberately not pinned. 1.11.1 and 1.11.2 are both
> documentation releases, so `main` is `v1.11.2` while production runs 1.11.0,
> and that is correct rather than a lag. What is deployed is
> `deploy/estate/prod.json` in the deployment repository, and the newest entry in
> its `docs/RELEASES.md` says how it got there.

### The cycle

```sh
./tools/branch.sh 42 h3-gate-measures-nothing --type fix   # from develop
# ... work, commit ...
git push origin fix/42-h3-gate-measures-nothing
gh pr create --base develop --fill                         # CI runs the full set
# review, then merge on GitHub
```

and when it is time to ship:

```sh
# bump the version in Cargo.toml and write the CHANGELOG entry first
gh pr create --base main --head develop --title "Release 1.1.0"
# CI additionally checks the changelog entry and that the version is untagged
# merge on GitHub, then:
git checkout main && git pull && ./tag.sh v1.1.0
```

The build host is still worth running by hand before opening a pull request, because
it tests more than a runner can:

```sh
M6_BUILD_HOST=root@<box> ./tools/build-host-tests.sh
```

---

## What has to pass

**Before a branch merges into `develop`**, on the build host, all of it:

| check | what it means | where |
|---|---|---|
| unit + integration tests | the whole workspace, zero failures | `cargo test --workspace` |
| compiler warnings | **zero**, release and test builds, pre-existing included | Linux |
| clippy | **silent. `-D warnings`, no ceiling**, from 2026-09-13 | `tools/clippy.sh` |
| h1, h2, h3 conformance | at or above the recorded scores | `tools/conformance.sh` |
| performance | within margin of the recorded number | `tools/perfcheck.sh` |
| cargo-deny | advisories, licences and sources | `cargo deny check` |
| the examples build | zero warnings, clippy silent, their tests pass, every config parses | the `m6-examples` repository |
| the examples work | example 05's end-to-end suite over the whole running stack | `examples/05-cms/test.sh` |

**CI runs all of it on the pull request.** `tools/build-host-tests.sh` runs the
same ground plus the two things a shared runner cannot do: the performance check,
which needs a quiet machine, and conformance against a real h2spec/h3spec install.
Run it before opening the pull request when the change could touch either.

**The examples are not a courtesy.** `m6-examples` is the only code in the
checks that uses m6's interfaces, and until 2026-09-14 nothing built it: it had
stopped compiling entirely, and five more defects were sitting underneath that
where nobody could see them. A change to m6 lands with the examples building, or
it does not land. See `docs/LESSONS.md` lesson 41.

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
- **Zero clippy findings too**, from 2026-09-13. No ceiling to raise. A new lint
  from a toolchain upgrade gets fixed, or gets a targeted `#[allow]` naming the
  lint with the reason argued in the commit. Never a global allowance.
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
