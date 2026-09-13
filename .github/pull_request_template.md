**What changed, and why it mattered**

**How it was verified**

Measured against something running, not inferred from the source.

---

Before this merges into `develop`, all of it has to pass on the build host:

- [ ] whole test suite, zero failures
- [ ] zero compiler warnings, release and test builds, pre-existing included
- [ ] clippy at or under its recorded count
- [ ] h1, h2, h3 conformance at or above their recorded scores
- [ ] performance check within its margin

`./tools/merge.sh <branch>` runs them and records what it ran in the merge
commit. If something got faster or slower, say so and update the recorded
number in the same commit.

Closes #
