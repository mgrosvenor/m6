#!/usr/bin/env bash
# Test that perfcheck.sh's leftover-server check matches only the base and
# candidate binaries the run started, at the absolute paths it started them, and
# not a path that appears merely as an argument.
set -uo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=tools/perfcheck-stray.sh
. "$HERE/perfcheck-stray.sh"

T="$(mktemp -d)" || { echo "cannot make a temp dir" >&2; exit 1; }
trap 'rm -rf -- "$T"' EXIT

# Two distinct absolute "binary" paths. A real binary's argv[0] is its own
# path, so the test runs `exec -a <path> sleep 60`, which gives sleep the path
# as its argv[0], the exact shape m6-html has.
BASE="$(resolve_abs "$T/base-bin/m6-html")"
CAND="$(resolve_abs "$T/cand-bin/m6-html")"

fail() { echo "FAIL: $1" >&2; exit 1; }

# Neither binary running: the check reports no stray.
if stray_alive "$BASE" "$CAND"; then fail "reports a stray with nothing running"; fi

# A stray base binary (a different absolute path from the candidate) is caught.
exec -a "$BASE" sleep 60 &
pid=$!
for _ in $(seq 1 50); do pgrep -f "$BASE" >/dev/null 2>&1 && break; sleep 0.05; done
stray_alive "$BASE" "$CAND" || fail "does not catch a stray base binary"
kill "$pid" 2>/dev/null; wait "$pid" 2>/dev/null

# A stray candidate binary is caught too.
exec -a "$CAND" sleep 60 &
pid=$!
for _ in $(seq 1 50); do pgrep -f "$CAND" >/dev/null 2>&1 && break; sleep 0.05; done
stray_alive "$BASE" "$CAND" || fail "does not catch a stray candidate binary"
kill "$pid" 2>/dev/null; wait "$pid" 2>/dev/null

# A process whose argv carries the base path only as an argument, the shape of
# perfcheck.sh's own `--base-bin`, must NOT match: only argv[0] counts.
yes "$BASE" >/dev/null &
pid=$!
for _ in $(seq 1 50); do pgrep -f "$BASE" >/dev/null 2>&1 && break; sleep 0.05; done
if ! pgrep -f "$BASE" >/dev/null 2>&1; then fail "test setup: the argument-carrying process did not start"; fi
stray_alive "$BASE" "$CAND" && fail "matches a path that appears only as an argument"
kill "$pid" 2>/dev/null; wait "$pid" 2>/dev/null

echo "PASS: the leftover-server check matches both binaries, and no argument-only path"
