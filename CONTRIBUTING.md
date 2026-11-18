# Contributing to m6

m6 is a web hosting engine made of an origin, an edge cache, a renderer and a monitor.
This page holds the rules a contributor needs to get a change merged: the branch model, the checks, the version policy and how to report a bug.
House rules, including the documentation ruleset R0 to R10, are in [`CLAUDE.md`](CLAUDE.md).

1. [The short version](#1-the-short-version)
2. [Branches](#2-branches)
3. [What has to pass](#3-what-has-to-pass)
4. [Releases](#4-releases)
5. [A check that cannot measure must fail](#5-a-check-that-cannot-measure-must-fail)
6. [Style](#6-style)
7. [Commit messages](#7-commit-messages)
8. [Reporting a bug](#8-reporting-a-bug)
9. [Changes the event-loop rule needs approved](#9-changes-the-event-loop-rule-needs-approved)
10. [Summary](#10-summary)

## 1. The short version

Three commands make a change:

```sh
./tools/branch.sh <issue> <slug> --type fix   # branches off develop
# work, commit
git push origin fix/<issue>-<slug>
gh pr create --base develop --fill            # CI runs every check, then merge on GitHub
```

File the issue first: `tools/branch.sh` stops when `gh` cannot find the issue.

## 2. Branches

Table 1 gives the three branches.

| branch | what it is |
|---|---|
| `main` | releases only. Nothing is pushed to it by hand |
| `develop` | where work is integrated |
| `<type>/<issue>-<slug>` | one branch per issue, cut from `develop` |

**Table 1: the three branches.**
Take from it that work starts on a work branch cut from `develop` and lands back by pull request.

The types are `feat`, `fix`, `perf`, `docs`, `refactor`, `test` and `chore`.

## 3. What has to pass

A change merges into `develop` after every check in Table 2 passes on Linux.
Table 2 lists each check with the *CI* (continuous integration) job that runs it.

| check | CI job |
|---|---|
| formatting (`cargo fmt --all --check`) | build and tests |
| zero compiler warnings in release and test builds, pre-existing included | build and tests |
| the test suite (`cargo test --workspace`) | build and tests |
| m6-core with every feature on (`cargo test -p m6-core --lib --all-features` and `--doc --all-features`) | build and tests |
| rustdoc silent (`RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps`, then with `--all-features`) | build and tests |
| clippy silent (`tools/clippy.sh`) | clippy |
| h1, h2 and h3 conformance at or above the recorded floors (`tools/conformance.sh`) | h1, h2, h3 conformance |
| dependencies (`cargo deny check`) | advisories, licences, duplicates |
| shellcheck clean at warning level | shell scripts |
| a teardown kills only a pid this run recorded (`tools/check-kill-safety.sh`) | shell scripts |
| documents name live commits (`tools/check-commit-hashes.sh`) | documents name live commits |
| the C client builds with no warnings | perf tools |
| the examples build and work | the examples build and work |
| the minimum supported Rust version in `Cargo.toml` builds the workspace | minimum supported Rust version |

**Table 2: the checks a merge must pass, and the CI job that runs each one.**
Take from it that five checks run in the build and tests job, and the other nine run in eight more jobs.

The workspace test run does not compile m6-core's off-by-default features, so CI builds and tests m6-core with every feature on.
The performance check runs on the build host, because it needs a quiet machine.
The address and leak sanitisers run in their own workflow, `.github/workflows/sanitizer.yml`, on pushes to `develop` and on pull requests to it.
That workflow is advisory: a finding in code older than the pull request leaves the merge clear.

CI runs every check except performance on the pull request, on the push to `main` or `develop` that merges it, and on a release tag.
`tools/build-host-tests.sh` runs the same checks plus the performance check, with conformance against a real h2spec and h3spec install.
Run it before opening the pull request when the change could touch either.

[`docs/m6-testing.md`](docs/m6-testing.md) covers what each check proves and how to run it.

## 4. Releases

A pull request into `main` is the release, and four checks apply to it.
CI enforces three of them.
The `main takes develop only` job refuses a work branch aimed at `main`.
The `changelog` job checks that `CHANGELOG.md` has a `## <version>` section, and that the version is not already tagged.
The fourth check is the author's judgement: the documents that state current facts must still hold, and the release checklist in the pull request template asks for it.

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

A release candidate carries the SemVer pre-release field.
On `develop` the workspace version is `X.Y.Z-rc.N`, and each candidate built from `develop` for staging is tagged `vX.Y.Z-rc.N`.
The release commit sets `X.Y.Z` and is tagged `vX.Y.Z`.
The release is the promoted candidate, and no second build is cut for it.

The version alone says which kind of build a binary is.
A `-rc.N` is a develop build for staging, a plain `X.Y.Z` is the release, and `.dirty` in the build metadata marks a tree with uncommitted changes.
A dirty build anywhere but the dev environment is a finding, and so is a pre-release build on prod.
`m6-monitor` reports both as faults.

### 4.2. Reading a build's version

A binary reports its version two ways.
`--version` prints the tag form, `vX.Y.Z-rc.N` or `vX.Y.Z`, with no crate name and no build metadata, on a dirty tree too.
`--version-full` prints one line per m6 crate in the binary, the binary's own name first, each as `<crate> - <full SemVer with build metadata>`.
A reader that needs the dirty marker uses `--version-full` or the `version` field of the start-up record.

The build metadata is the twelve-character commit, with `.dirty` appended on a tree with uncommitted changes.
A build with no git, such as a source tarball, reads `1.16.0-rc.1+unknown`.
Each line comes from that crate's own build-time constants, so a mixed build shows two commits.

Example 1 shows `m6-http`:

```text
$ m6-http --version
v1.16.0-rc.1
$ m6-http --version-full
m6-http - 1.16.0-rc.1+abc123def456
m6-core - 1.16.0-rc.1+abc123def456
m6-log - 1.16.0-rc.1+abc123def456
m6-platform - 1.16.0-rc.1+abc123def456
```

**Example 1: the two version forms of a clean m6-http build.**

The first line names the binary, from `CARGO_BIN_NAME`.
Eight binaries share the `m6-http` package, so each names itself first and lists `m6-http` second.
Example 2 shows `m6-bench`:

```text
$ m6-bench --version-full
m6-bench - 1.16.0-rc.1+abc123def456
m6-http - 1.16.0-rc.1+abc123def456
m6-core - 1.16.0-rc.1+abc123def456
m6-log - 1.16.0-rc.1+abc123def456
m6-platform - 1.16.0-rc.1+abc123def456
```

**Example 2: a binary whose name differs from its package's name.**

### 4.3. A speed change

The A/B gate on the build host (`tools/perfcheck.sh`) measures a change against its base.
Each target runs 100 rounds of 500 requests, because a fresh server per round settles at its own speed, and more rounds narrow the spread where more requests do not.
The gate prints the median candidate-to-base ratio and its 99% confidence interval per target.
A slowdown of 3% or more with the whole interval above 1 fails the merge.

`PERFCHECK_ACCEPT_SLOWER="<reason>"` accepts a slowdown.
The reason is recorded in `tools/perf-history.jsonl`, and the commit message says why.

The figures are valid on the build host only.
[`docs/PERFORMANCE.md`](docs/PERFORMANCE.md) explains why and holds the measurements.

## 5. A check that cannot measure must fail

A check you add fails when it cannot run.
[`tools/conformance.sh`](tools/conformance.sh) shows the cost: its header lists the three ways it printed "skipped" or a pass while measuring nothing.

## 6. Style

- Run `cargo fmt --all`.
  CI runs `cargo fmt --all --check` and fails on a difference.
- Comments explain why.
- Prose follows the documentation ruleset R0 to R10 in [`CLAUDE.md`](CLAUDE.md).

## 7. Commit messages

A commit says what changed, why it mattered, and how it was verified.
[`CLAUDE.md`](CLAUDE.md) has the full rule.

## 8. Reporting a bug

Say what you did, what you expected, and what happened.
A packet capture, a log line with its timestamp, or a `curl -v` output is worth more than a description.
If the bug involves the edge, say which role: origin, edge cache or monitor.

Reproduce the bug against something any reader can run: `m6-examples`, example 05's end-to-end suite, or h2spec and h3spec against a loopback instance.
Evidence from a private deployment cannot be re-run by anyone else, so it is an assertion.

Security issues go to [`SECURITY.md`](SECURITY.md).

## 9. Changes the event-loop rule needs approved

A change that adds an `#[expect(clippy::disallowed_methods)]`, a thread, a heap allocation on a loop's hot path or an `unsafe` block names the project owner's approval in its pull request description and quotes the reason given.

## 10. Summary

Branch from `develop`, open a pull request into `develop`, and pass every check in Table 2.
File the issue first, say in the commit what changed and how it was verified, and report defects against something a reader can run.
Release by merging `develop` into `main` with a changelog entry and the documents still true.
