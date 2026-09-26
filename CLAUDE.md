# Working on m6

House rules. They are enforced by git hooks and by the checks in `tools/`, not
by anyone remembering them.

---

## RULE ZERO: m6 IS A GENERIC WEB HOSTING ENGINE

**It must not in any way be tied to one particular site.** Owner's rule, stated
plainly on 2026-09-26 and placed first because it was broken repeatedly while
being written down elsewhere.

This repository is **public**. One instance of m6 happens to be the owner's own
site, in its own private repository. That instance is a USER of this software and
nothing more. Nothing about it belongs here.

**Never in this repository. Not in code, not in tests, not in comments, not in
docs, not in the changelog, not in issues, not in commit messages.**

| forbidden | use instead |
|---|---|
| any real domain or hostname | `example.com`, `www.example.com` |
| any real IP address or ssh port | `localhost`, `127.0.0.1`, or `<host>` |
| real node names | `origin`, `edge-a`, `edge-b`, `monitor` |
| an email address | nothing. `SECURITY.md` uses GitHub private reporting |
| a repository name of a deployment | nothing |
| an artefact md5, or which version is deployed where | nothing. That is the deployment's record, not m6's |
| per-node measurements taken off someone's live fleet | numbers from a loopback instance or an example |
| "my site needs X" | "a website might want to X" |

**Issues and documents cover generic features only.** Write a feature request
abstractly: *a website might want to ...*. A defect report gets the finding plus
a reproduction any reader can run.

**And the reason that last part is not a style preference.** Evidence measured
against a private production instance is **unreproducible by anyone else**, so it
is not evidence, only an assertion that looks authoritative. "Verified means
measured against something running" is satisfied without touching anyone's fleet:
`m6-examples`, example 05's end-to-end suite, h2spec and h3spec against a
loopback instance, and `tools/build-host-tests.sh`. Use those.

**How this was broken, so it is not repeated.** By 2026-09-26 this public
repository had accumulated: a working handover and a ledger stating which of one
deployment's boxes ran which version at which md5, that deployment's node names
in 19 source and doc files, a server's public IP in older revisions, the owner's
personal email in `SECURITY.md`'s history, and 19 public issues carrying node
names, artefact hashes, live-fleet measurements and two server IPs with their
roles. A test existed to prevent exactly this and could not: it had to spell the
identity it banned, so it published what it guarded, and it only ever checked
files, never the issue tracker.

**So this rule is care and judgement, not machinery.** There is no check that
will catch it for you. Read what you are about to write and ask whether a
stranger running m6 on their own server would find it relevant.

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
| `CHANGELOG.md` | **start here.** What changed in each release, newest first, with how it was verified |
| `README.md` | what m6 is, what it does, and how to run one |
| `docs/m6-core-reference.md` | every core module and its interface |
| `docs/PERFORMANCE.md` | every performance number, how it was measured, on what |
| `docs/LESSONS.md` | the things that cost something to learn |
| `docs/H2-PLAN.md` | HTTP/2 and HTTP/3: the conformance status, then the phase history |
| `tools/conformance-scores.txt` | the floors, and the whole argument behind the h3 number |

### The working documents are not here, deliberately

**`HANDOVER.md`, `docs/CONSOLIDATION-TODO.md` and the session notes were removed
on 2026-09-26.** Owner's rule: handover and todo are not public documents, only
release notes are.

They stated the operational state of one particular deployment, which is
private: which version was in production and since when, artefact md5s, node
names, deploy sequencing, and in older revisions a server's public IP address.
This repository is a generic web system; that material belongs to the deployment
that owns it, and it now lives in the private deployment repository under
`docs/m6-internal/`, byte-for-byte, with its provenance recorded.

**The rule this leaves you with.** m6's docs, issues and changelog entries cover
GENERIC features only. Write a feature issue abstractly, "a website might want
to ...", never "my site needs". Never put a domain, hostname, IP address, email
address, node name, artefact hash or deployment state in this repository.

And note what that rules out, because it is the trap this repository walked into
19 issues deep: **evidence measured against a private production instance is
unreproducible by anyone else**, so it is not evidence. "Verified means measured
against something running" is satisfied by `m6-examples`, example 05's
end-to-end suite, h2spec and h3spec against a loopback instance, and
`tools/build-host-tests.sh`. Use those.
