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

Closes #
