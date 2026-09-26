**What changed, and why it mattered**

**How it was verified**

Measured against something running, not inferred from the source.

---

Before this merges into `develop`, all of it has to pass on the build host:

- [ ] whole test suite, zero failures
- [ ] zero compiler warnings, release and test builds, pre-existing included
- [ ] clippy silent: `-D warnings`, no ceiling
- [ ] h1, h2, h3 conformance at or above their recorded scores
- [ ] performance check within its margin

CI runs all of it on this pull request. `M6_BUILD_HOST=root@<box>
./tools/build-host-tests.sh` runs the same ground plus the two things a shared
runner cannot do: the performance check, which needs a quiet machine, and
conformance against a real h2spec/h3spec install. Run it here first when the
change could touch either.

If something got faster or slower, say so and update the recorded number in the
same commit.

---

**If this pull request is into `main`, it IS the release.** Docs being up to date
is a fundamental part of one, not a follow-up to it, so tick these having
actually opened the files:

- [ ] `CHANGELOG.md` has a section for this version: what changed, why it
      mattered, how it was verified
- [ ] every document that makes a CURRENT-STATE claim still holds. The ones that
      do: `README.md` (what m6 is and does), `CONTRIBUTING.md` (the gates),
      `docs/H2-PLAN.md` (the conformance status line),
      `docs/PERFORMANCE.md` (the numbers), `docs/LESSONS.md`
- [ ] no document tells a reader to run something that no longer exists

CI checks the changelog entry and that the version is untagged. It cannot check
freshness, which is why these are here: a checkbox read while you know the answer
is the only mechanism that has ever worked on this.

Why it is asked at all: on 2026-09-26 a sweep found `docs/H2-PLAN.md` leading
with h3spec 37/49 thirteen days after it became 47/49, `CONTRIBUTING.md`
describing a clippy ceiling removed on 2026-09-13, and `README.md` saying a
decision "gates a 1.0 release" twelve days after 1.0 was cut.

Closes #
