#!/usr/bin/env bash
# Test that perfcheck.sh's leftover-server check matches both the base and the
# candidate binary, at the absolute paths the run started. A stray in either arm
# pins the measured CPU, so the check must catch both.
set -uo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=tools/perfcheck-stray.sh
. "$HERE/perfcheck-stray.sh"

T="$(mktemp -d)" || { echo "cannot make a temp dir" >&2; exit 1; }
trap 'rm -rf -- "$T"' EXIT

# Two dummy servers: shell scripts that sleep, used only for their argv[0], so
# pgrep has a process whose full command line carries the path.
for name in base cand; do
  printf '#!/bin/sh\nsleep 60\n' > "$T/$name"
  chmod +x "$T/$name"
done
BASE="$(resolve_abs "$T/base")"
CAND="$(resolve_abs "$T/cand")"

fail() { echo "FAIL: $1" >&2; exit 1; }

# Neither binary running: the check reports no stray.
if stray_alive "$BASE" "$CAND"; then fail "reports a stray with nothing running"; fi

# A stray base binary (a different absolute path from the candidate) is caught.
"$BASE" &
pid=$!
for _ in $(seq 1 50); do pgrep -f "$BASE" >/dev/null 2>&1 && break; sleep 0.05; done
stray_alive "$BASE" "$CAND" || fail "does not catch a stray base binary"
kill "$pid" 2>/dev/null; wait "$pid" 2>/dev/null

# A stray candidate binary is caught too.
"$CAND" &
pid=$!
for _ in $(seq 1 50); do pgrep -f "$CAND" >/dev/null 2>&1 && break; sleep 0.05; done
stray_alive "$BASE" "$CAND" || fail "does not catch a stray candidate binary"
kill "$pid" 2>/dev/null; wait "$pid" 2>/dev/null

echo "PASS: the leftover-server check matches both base and candidate binaries"
