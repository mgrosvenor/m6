#!/usr/bin/env bash
# clippy.sh — the clippy ratchet.
#
# Usage:
#   ./tools/clippy.sh            # check; fails if the count went up
#   ./tools/clippy.sh --update   # record the current count as the new ceiling
#
# Why a ratchet rather than `-D warnings`:
#
# The workspace had 161 clippy findings the day this was added, and none of them
# is a correctness bug: empty lines after doc comments, `if` blocks that could
# collapse into a `match`, complex types worth a `type` alias. Denying all
# warnings would have meant either fixing 161 things in one unreviewed sweep or
# turning the gate off, and the second is what actually happens.
#
# So the ceiling only ever comes down. New code cannot add a finding, and every
# one that gets fixed lowers the bar behind it. This is the same shape as
# tools/conformance.sh and its floors, and for the same reason: a gate nobody
# can pass is a gate that gets removed.
#
# **The rustc zero-warnings rule is unchanged and absolute.** This is clippy,
# which is a different and much larger set of opinions.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
cd "$SCRIPT_DIR/.."

CEILING_FILE="tools/clippy-ceiling.txt"
UPDATE=false
[[ "${1:-}" == "--update" ]] && UPDATE=true

if ! cargo clippy --version >/dev/null 2>&1; then
  echo "clippy is not installed (rustup component add clippy)" >&2
  exit 1
fi

# --all-targets so tests and benches are linted too: that is where the last two
# defects in this codebase were found, and lint coverage that stops at src/ is
# how a test helper gets to panic inside a retry loop for a year.
OUT="$(mktemp)"
trap 'rm -f "$OUT"' EXIT
cargo clippy --workspace --all-targets 2>&1 | tee "$OUT" >/dev/null || true

# Per-lint warnings only. The per-crate "generated N warnings" lines are
# summaries of these and would double-count.
COUNT="$(grep -E '^warning: ' "$OUT" | grep -vcE 'generated [0-9]+ warning' || true)"
COUNT="${COUNT:-0}"

# A clippy *error* is never acceptable, whatever the ceiling says.
if grep -qE '^error' "$OUT"; then
  echo "clippy reported errors:" >&2
  grep -E '^error' "$OUT" | head -20 >&2
  exit 1
fi

if $UPDATE; then
  echo "$COUNT" > "$CEILING_FILE"
  echo "clippy ceiling set to $COUNT"
  exit 0
fi

if [[ ! -f "$CEILING_FILE" ]]; then
  echo "no $CEILING_FILE; run ./tools/clippy.sh --update to create it" >&2
  exit 1
fi

CEILING="$(tr -d '[:space:]' < "$CEILING_FILE")"

if (( COUNT > CEILING )); then
  echo "clippy findings rose from $CEILING to $COUNT" >&2
  echo "the new ones are in the output below; fix them rather than raising the ceiling" >&2
  grep -E '^warning: ' "$OUT" | grep -vE 'generated [0-9]+ warning' | sort | uniq -c | sort -rn | head -20 >&2
  exit 1
fi

if (( COUNT < CEILING )); then
  echo "clippy findings fell from $CEILING to $COUNT — lower the ceiling:"
  echo "    ./tools/clippy.sh --update"
  # Not a failure. Refusing a push because someone improved things is how a
  # ratchet teaches people to stop improving things.
fi

echo "clippy: $COUNT findings, ceiling $CEILING"
