# Contributing to m6

This page holds the rules a contributor needs.
m6 is a generic web hosting engine: an origin, an edge cache, a renderer and a monitor, meant to serve a website from your own servers.
Contributions and bug reports are welcome.
The full model is in [`CLAUDE.md`](CLAUDE.md).

1. [The short version](#1-the-short-version)
2. [Branches](#2-branches)
3. [What has to pass](#3-what-has-to-pass)
4. [Releases](#4-releases)
5. [A check that cannot measure must fail](#5-a-check-that-cannot-measure-must-fail)
6. [Style](#6-style)
7. [Commit messages](#7-commit-messages)
8. [Reporting a bug](#8-reporting-a-bug)
9. [Summary](#9-summary)

## 1. The short version

The three commands that make a change:

```sh
./tools/branch.sh <issue> <slug> --type fix   # branches off develop
# work, commit
git push origin fix/<issue>-<slug>            # runs the fast checks
gh pr create --base develop --fill            # CI runs everything, then merge on GitHub
```

File the issue first.
A branch named after an issue nobody filed is a branch nobody can find later.

## 2. Branches

Table 1 gives the three branches.

| branch | what it is |
|---|---|
| `main` | releases only. Nothing is pushed to it by hand |
| `develop` | where work is integrated |
| `<type>/<issue>-<slug>` | one branch per issue, cut from `develop` |

**Table 1: the three branches.**
Take from it that work starts on a work branch cut from develop, and lands back by pull request.

Types are `feat`, `fix`, `perf`, `docs`, `refactor`, `test` and `chore`.

## 3. What has to pass

A change merges into `develop` only after every check below passes, on Linux.
Table 2 lists the checks by the *CI* (continuous integration) job that runs each one.

| check | CI job |
|---|---|
| formatting (`cargo fmt --all --check`) | build, tests, clippy |
| zero compiler warnings, release and test builds, pre-existing included | build, tests, clippy |
| the test suite (`cargo test --workspace`) | build, tests, clippy |
| m6-core with every feature on (`cargo test -p m6-core --lib --all-features` and `--doc --all-features`) | build, tests, clippy |
| clippy silent (`tools/clippy.sh`) | build, tests, clippy |
| h1, h2 and h3 conformance at or above the recorded floors (`tools/conformance.sh`) | h1, h2, h3 conformance |
| dependencies (`cargo deny check`) | advisories, licences, duplicates |
| shellcheck clean at warning level | shell scripts |
| the examples build and work | the examples build and work |
| the minimum supported Rust version in `Cargo.toml` | minimum supported Rust version |

**Table 2: the checks a merge must pass, and the CI job that runs each one.**
Take from it that five checks run in one CI job, and the other five each have their own.

The workspace test run does not compile m6-core's off-by-default features, so CI builds and tests m6-core with every feature on.
The performance check runs on the build host, because it needs a quiet machine.
The address and leak sanitisers run in their own workflow, `.github/workflows/sanitizer.yml`, on pushes to `develop` and on pull requests to it.
That workflow is advisory: a finding in code older than the pull request leaves the merge clear.

CI runs every check on the pull request, again on the push to `main` or `develop` that merges it, and on a release tag.
`tools/build-host-tests.sh` runs the same ground plus the performance check and conformance against a real h2spec and h3spec install.
Run it before opening the pull request when the change could touch either.

[`docs/m6-testing.md`](docs/m6-testing.md) covers what each check proves and how to run it.

## 4. Releases

A pull request into `main` is the release, and four checks apply to it.
CI enforces three of them.
The `main takes develop only` job refuses a work branch aimed straight at `main`.
The `changelog` job checks that `CHANGELOG.md` has a `## <version>` section and that the version is not already tagged.
The fourth check is judgement, which CI cannot decide.
m6's own documents must still hold, which is a fundamental part of a release and which the release checklist in the pull request template asks for.

### 4.1. Versions

A version is `X.Y.Z`, and what changed decides which number moves.
Table 3 gives the three numbers.

| number | moves when | example |
|---|---|---|
| X | a major architectural change | a new process model |
| Y | a breaking change to an API, configuration, library or behaviour, or a major new feature | a config key renamed, or a new protocol |
| Z | bug fixes, minor updates or minor new features | a corrected cache clear |

**Table 3: the three version numbers and what moves each.**
Take from it that a release doing two things takes the larger move.

A change to configuration or behaviour that needs a migration note is a Y change.
It carries a Migration section in its `CHANGELOG.md` entry, ahead of the list of changes.

### 4.2. Recording a speed change

A change that makes m6 faster records the new number, in the commit that earned it:

```sh
./tools/perfcheck.sh --update
```

A change that makes m6 slower says why in the commit message.
The performance check refuses the merge until the number is updated deliberately, so accepting a regression is a deliberate decision.

The numbers are taken on the build host and mean nothing on another machine.
[`docs/PERFORMANCE.md`](docs/PERFORMANCE.md) explains why and holds the measurements.

## 5. A check that cannot measure must fail

A check you add fails when it cannot run.
It does not print "skipped" and return success.
[`tools/conformance.sh`](tools/conformance.sh)'s header lists the three ways the conformance script broke this rule.

## 6. Style

- Run `cargo fmt --all`.
  CI runs `cargo fmt --all --check` and fails on a difference.
- Comments explain why, especially why something is not the obvious thing.
- Prose follows the standing rules and the documentation ruleset R0 to R10, both in [`CLAUDE.md`](CLAUDE.md).

## 7. Commit messages

Commit messages follow the rule in [`CLAUDE.md`](CLAUDE.md).
A commit says what changed, why it mattered, and how it was verified.

## 8. Reporting a bug

Say what you did, what you expected, and what happened.
A packet capture, a log line with its timestamp, or a `curl -v` is worth more than a description.
If it involves the edge, say which role: origin, edge cache or monitor.

Reproduce it against something a reader can run, never against your own live server.
`m6-examples`, example 05's end-to-end suite, and h2spec or h3spec against a loopback instance all work.
Evidence from a private deployment cannot be re-run by anyone else, which makes it an assertion.

Security issues go to [`SECURITY.md`](SECURITY.md), not to the issue tracker.

## 9. Summary

Branch from develop, open a pull request into develop, and pass every check in Table 2.
File the issue first, say in the commit what changed and how it was verified, and report defects against something a reader can run.

Release by merging develop into main with a changelog entry and the docs still true.
