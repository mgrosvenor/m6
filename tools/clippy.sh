#!/usr/bin/env bash
# clippy.sh — clippy must be silent.
#
# Usage:
#   ./tools/clippy.sh            # fails on any clippy finding
#
# There is no ceiling any more, and no --update. From 2026-09-13 this is
# `-D warnings`: one finding fails the run, the same way a rustc warning does.
#
# ── Why it used to carry a number, and why it no longer does ──────────────────
#
# The workspace had 161 findings the day this check was added, none of them a
# correctness bug: empty lines after doc comments, `if` blocks that could
# collapse into a `match`, complex types worth a `type` alias. Denying all
# warnings then would have meant either fixing 161 things in one unreviewed
# sweep or turning the gate off, and the second is what actually happens. So it
# became a ceiling that could only fall.
#
# That was a holding position and it held for as long as it was useful. The
# count went to zero on 2026-09-13 under issue #5, so the ceiling now gates
# nothing and only creates work: a number in a file that has to be lowered by
# hand every time someone improves something, and which reads as an allowance
# rather than as a standard. `-D warnings` needs no maintenance and cannot rot.
#
# **The rustc zero-warnings rule is unchanged and absolute.** This is clippy,
# which is a different and much larger set of opinions, and it is now held to
# the same standard.
#
# ── A finding here is a finding *for the clippy you are running* ──────────────
#
# The two ceiling files this replaced were per-platform, and that was really
# per-clippy-version: the laptop ran 0.1.95 and the build host 0.1.98, and on
# identical source they disagreed by 78 findings. New lints arrive with new
# versions, and cfg-gated code is only linted where it compiles, so a
# macOS-only kqueue block is invisible to clippy on Linux and vice versa.
#
# Zero was reached and verified on BOTH, which is the only reason a single
# absolute rule is safe here. If a toolchain upgrade introduces a new lint, this
# fails, and that is the intended behaviour: fix it, or argue in the commit for
# a targeted `#[allow]` naming the lint and the reason. Do not reintroduce a
# global allowance.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
cd "$SCRIPT_DIR/.."

if [[ "${1:-}" == "--update" ]]; then
  echo "--update is gone: there is no ceiling to update. clippy must be silent." >&2
  exit 2
fi

if ! cargo clippy --version >/dev/null 2>&1; then
  echo "clippy is not installed (rustup component add clippy)" >&2
  exit 1
fi

# --all-targets so tests and benches are linted too: that is where the last two
# defects in this codebase were found, and lint coverage that stops at src/ is
# how a test helper gets to panic inside a retry loop for a year.
#
# -D warnings on the command line rather than a crate attribute, so the same
# source can still be linted leniently by hand while the gate stays absolute.
OUT="$(mktemp)"
trap 'rm -f "$OUT"' EXIT

if cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tee "$OUT" >/dev/null; then
  echo "clippy: silent on $(uname -s) ($(cargo clippy --version))"
  exit 0
fi

echo "clippy is not silent on $(uname -s) ($(cargo clippy --version)):" >&2
grep -E '^(error|warning)' "$OUT" | grep -vE 'generated [0-9]+ warning' | head -40 >&2
echo >&2
echo "Fix them. There is no ceiling to raise." >&2
exit 1
