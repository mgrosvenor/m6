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
| formatting (`cargo fmt --all --check`) | build and tests |
| zero compiler warnings, release and test builds, pre-existing included | build and tests |
| the test suite (`cargo test --workspace`) | build and tests |
| m6-core with every feature on (`cargo test -p m6-core --lib --all-features` and `--doc --all-features`) | build and tests |
| clippy silent (`tools/clippy.sh`) | clippy |
| rustdoc silent (`RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps`, then with `--all-features`) | build and tests |
| h1, h2 and h3 conformance at or above the recorded floors (`tools/conformance.sh`) | h1, h2, h3 conformance |
| dependencies (`cargo deny check`) | advisories, licences, duplicates |
| shellcheck clean at warning level | shell scripts |
| a teardown kills only a pid this run recorded (`tools/check-kill-safety.sh`) | shell scripts |
| documents name live commits (`tools/check-commit-hashes.sh`) | documents name live commits |
| the C client builds with no warnings | perf tools |
| the examples build and work | the examples build and work |
| the minimum supported Rust version in `Cargo.toml` | minimum supported Rust version |

**Table 2: the checks a merge must pass, and the CI job that runs each one.**
Take from it that five checks run in one CI job, and the other nine run in eight more, with shellcheck and kill safety both in the shell job.

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

**A release candidate carries the SemVer pre-release field.** On `develop` the workspace version is `X.Y.Z-rc.N`, and each candidate built from `develop` for staging is tagged `vX.Y.Z-rc.N`.
The release commit sets `X.Y.Z` and is tagged `vX.Y.Z`.
A plain version therefore means the release candidate became the release: `1.16.0` is the release that `1.16.0-rc.1` was promoted to, and no second build is cut for it.

**The version alone says which of the three a build is.** A `-rc.N` is a develop build for staging, a plain `X.Y.Z` is the release, and a `.dirty` in the build metadata is an uncommitted test build.
The dirty marker is build metadata, so `--version` never carries it. A reader that needs it asks `--version-full` or reads the metrics line's `version`.
A dirty build outside a development machine is a finding, and a pre-release build on production is a finding. `m6-monitor` reports both as faults.

**A binary reports the version two ways.** `--version` prints the tag form and nothing else, `vX.Y.Z-rc.N` or `vX.Y.Z`, with no crate name and no build metadata. A dirty tree still prints the tag form.
`--version-full` prints one line per m6 crate in the binary, the binary's own name first, each as `<crate> - <full SemVer with build metadata>`.
A line comes from that crate's own build-time constants, so a mixed build shows two commits instead of reporting one commit for the whole binary.
The build metadata is the twelve-character commit, with `.dirty` appended on a tree with uncommitted changes. A build with no git, which is a source tarball, reads `1.16.0-rc.1+unknown`.
The commit and the dirty flag are build metadata, which SemVer allows and ignores when comparing precedence.

On the release tree at `3a639b7e`, `m6-http` prints:

```text
$ m6-http --version
v1.16.0-rc.1
$ m6-http --version-full
m6-http - 1.16.0-rc.1+3a639b7e5f55
m6-core - 1.16.0-rc.1+3a639b7e5f55
m6-log - 1.16.0-rc.1+3a639b7e5f55
m6-platform - 1.16.0-rc.1+3a639b7e5f55
```

A binary whose name differs from its package's name prints its own name first and the package second, so `m6-bench --version-full` prints:

```text
m6-bench - 1.16.0-rc.1+3a639b7e5f55
m6-http - 1.16.0-rc.1+3a639b7e5f55
m6-core - 1.16.0-rc.1+3a639b7e5f55
m6-log - 1.16.0-rc.1+3a639b7e5f55
m6-platform - 1.16.0-rc.1+3a639b7e5f55
```

A mixed build shows two commits, for example:

```text
m6-http - 1.16.0-rc.1+3a639b7e5f55
m6-core - 1.16.0-rc.1+82a819001c85
```

The first line names the binary, from `CARGO_BIN_NAME`, which is not always its package's name. Eight binaries share the `m6-http` package, so each names itself first and lists `m6-http` second.

### 4.2. A speed change

A change is measured against its base by the A/B gate on the build host, which prints the paired difference and its 99% interval per target.
A consistent slowdown fails the merge.

Each target runs 100 rounds of 500 requests, because a fresh server per round settles at its own speed and more rounds, not more requests, narrow the spread.

Accepting one is `PERFCHECK_ACCEPT_SLOWER="<reason>"`, which is recorded in the history, and the commit message says why.

The figures are taken on the build host and mean nothing on another machine.
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

## 9. Changes the event-loop rule needs approved

A change that adds an `#[expect(clippy::disallowed_methods)]`, a thread, a heap allocation on a loop's hot path or an `unsafe` block names the owner's approval in its pull request description, and quotes the reason he gave.

## 10. Summary

Branch from develop, open a pull request into develop, and pass every check in Table 2.
File the issue first, say in the commit what changed and how it was verified, and report defects against something a reader can run.

Release by merging develop into main with a changelog entry and the docs still true.
