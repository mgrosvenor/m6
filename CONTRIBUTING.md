# Contributing to m6

m6 is a small, self-contained HTTP stack: an origin, an edge cache, a renderer
and a monitor, meant to serve a website from your own servers. Contributions are
welcome; so are bug reports that simply say what you saw.

## The short version

```sh
./tools/branch.sh <issue> <slug> --type fix   # branches off develop
# work, commit
git push origin fix/<issue>-<slug>            # runs the fast checks
gh pr create --base develop --fill           # CI runs everything, then merge on GitHub
```

`CLAUDE.md` has the full model. The rules below are the ones a contributor
needs.

## Branches

- `main` is releases only. Nothing is pushed to it by hand.
- `develop` is where work is integrated.
- Work happens on `<type>/<issue>-<slug>`, branched from `develop`.
- Types: `feat` `fix` `perf` `docs` `refactor` `test` `chore`.

Every branch names an issue, because a branch named after an issue nobody
filed is a branch nobody can find six months later. File the issue first; it
can be two sentences.

## What has to pass

All of it, on Linux, before anything merges into `develop`:

- the whole test suite
- **zero compiler warnings**, release and test builds, including ones that were
  already there
- clippy **silent**: `-D warnings`, no ceiling, since 2026-09-13. This said "at
  or under its recorded count" until 2026-09-26, which was the old model and had
  been replaced because a number in a file reads as an allowance and has to be
  maintained by hand. A new lint from a toolchain upgrade gets fixed, or gets a
  targeted `#[allow]` naming the lint with the reason argued in the commit.
- h1, h2 and h3 conformance, at or above their recorded scores
- the performance check, within its margin

CI runs them on the pull request. `tools/build-host-tests.sh` runs the same ground plus the performance check, which a shared runner cannot measure.

### A release also has to be documented, and that is enforced

A pull request into `main` **is** the release, and two more checks apply to it:

- `CHANGELOG.md` has a `## <version>` section: what changed, why it mattered, and how it
  was verified.
- m6's own documents still hold. The ones making current-state claims are
  `README.md`, this file, `docs/H2-PLAN.md`, `docs/PERFORMANCE.md` and
  `docs/LESSONS.md`. The release checklist in the pull request template asks for
  this, because freshness is judgement and CI cannot decide it.

  This was a check on `HANDOVER.md` until 2026-09-26, requiring it to mention the
  version. That file states which of one particular deployment's boxes runs what,
  so it is not a public document and now lives in the private deployment
  repository, where a workflow here cannot read it. **The rule did not move with
  it**: a release is a claim about what is now true, so the documents that say
  what is true are part of the release.

**Docs being up to date is a fundamental part of a release, not a follow-up to one.**
Owner's rule, 2026-09-25, and it was earned: 1.11.0 shipped while a working document
on `main` still named the previous release as current, in the first bullet of the first
section a cold reader opens. The correction existed on `develop` and could not reach
`main`, because a docs-only `develop` to `main` pull request fails the "version is not
already tagged" check, so the stale text sat on the default branch until a later
release happened to carry it. 1.11.1 had to be cut for documentation alone.

The checklist is deliberately judgement and not machinery. Nothing here can know what
any deployment is running, and a check that cannot perform its measurement must not
pretend to: this repository's trap list is mostly those. What the checklist guarantees
is that you open the files while cutting the release, which is when you know what is
true.

### If you make something faster

Record the new number, in the commit that earned it:

```sh
./tools/perfcheck.sh --update
```

Those numbers are taken on the build host and mean nothing on another machine.
`docs/PERFORMANCE.md` explains why and holds the real measurements.

### If you make something slower

Say why in the commit message. The check will refuse the merge until the
number is updated deliberately, which is the point: accepting a regression is
a decision, not an accident.

## A check that cannot measure must fail

If you add a check, it fails when it cannot run. It does not print "skipped"
and return success. The conformance script did that in four different places
and reported PASS through all of them, and h2 and h3 went untested for months
as a result. `tools/conformance.sh`'s header lists the four and what each cost.

## Style

- **Run `cargo fmt --all`.** CI runs `cargo fmt --all --check` and the build
  fails on a difference. This file said the opposite until 2026-09-26, that
  `cargo fmt` is "not run over this tree" because the code aligned struct fields
  by hand: that was true until 2026-09-13, when the tree was formatted and the
  owner's call was to run it and accept the result. Formatting has one answer
  now and nobody has to hold it in their head.
- Comments explain *why*, and especially why something is not the obvious
  thing. The best comments in this codebase name the defect that made the code
  look the way it does.
- No em dashes in prose.
- Plain technical English. No borrowed management vocabulary.

## Commit messages

What changed, why it mattered, and how it was verified. "Verified" means
measured against something running, not inferred from the source: a good number
of the defects in `CHANGELOG.md` were invisible in the code and only showed up
against a running server.

## Reporting a bug

Say what you did, what you expected, and what happened. A packet capture, a log
line with its timestamp, or a `curl -v` is worth more than a description of
either. If it involves the edge, say which role: origin, edge cache or monitor.

**Reproduce it against something a reader can run**, not against your own live
server. `m6-examples`, example 05's end-to-end suite, and h2spec or h3spec against
a loopback instance all work. Evidence from a private deployment cannot be re-run
by anyone else, which makes it an assertion rather than evidence.

Security issues go to `SECURITY.md`, not to the issue tracker.
