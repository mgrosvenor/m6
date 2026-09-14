# Contributing to m6

m6 is a small HTTP stack written for one site, and built so that other sites
could use it. Contributions are welcome; so are bug reports that simply say
what you saw.

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
- clippy, at or under its recorded count
- h1, h2 and h3 conformance, at or above their recorded scores
- the performance check, within its margin

CI runs them on the pull request. `tools/build-host-tests.sh` runs the same ground plus the performance check, which a shared runner cannot measure.

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

- The code aligns struct fields and match arms by hand. `cargo fmt` is **not**
  run over this tree; it would undo that throughout. Match the file you are in.
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
either. If it involves the edge, say which node.

Security issues go to `SECURITY.md`, not to the issue tracker.
