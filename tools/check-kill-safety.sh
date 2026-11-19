#!/usr/bin/env bash
# check-kill-safety.sh: a teardown may kill only a pid this run recorded.
#
# Usage:
#   ./tools/check-kill-safety.sh              # check this repository
#   ./tools/check-kill-safety.sh --tree <dir> # check another checkout
#   ./tools/check-kill-safety.sh --self-test  # prove the scan catches a plant
#
# `--tree` checks another checkout. The path is resolved from the caller's
# directory, before the scan changes into the tree. This repository's own
# scan covers examples/, which is tracked here.
#
# Why this exists. On 2026-11-15 four example scripts tore down with
# `lsof -ti :8443 | xargs kill -9`, three `dev.sh` scripts with
# `pkill -x m6-http`, and 08's `kill_own` with `pgrep -f "$SITE"`. Each reaches
# a process by name, port or command-line text, so each kills another session's
# stack on a machine where two sessions run the same example. The examples
# now use `examples/own.sh`: record a pid
# when the process starts, store its start time beside it, and compare the
# start time before signalling, so a reused pid is left alone.
#
# The scan skips comment lines, so prose that describes an old shape does not
# fail this check.
#
# What is scanned: tracked `*.sh`, `*.yml` and `*.yaml` files and the git
# hooks. That is where a teardown is written. Rust code that runs `lsof` is a
# read-only report (m6-platform, m6-core testkit) and is not a kill site.
#
# What is forbidden:
#   pkill ...                      kills whatever matches a name or pattern
#   killall ...                    the same, by name
#   ... | xargs kill               kills whatever a selector printed
#   lsof -t ... | ... kill         kills the holder of a port
#   pgrep ... -> kill              kills whatever a command line matched
#
# A `pgrep` whose result is only compared or printed (tools/perfcheck-stray.sh,
# m6-run-eg waiting for a port) is not a kill and is not reported. The rule is
# about what is signalled, not about the words on the line.
#
# Exit 0 when no offender is found, 1 with `file:line: text` for every
# offender. No network, no cargo, and one awk process for the whole tree.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"

# scan_files <path>...: print one "path:line: text" per offending line.
# awk is used because the 03/04 CI teardown holds the `pgrep` and the `kill`
# on different lines of the same `for ... done`, so a line grep cannot see it.
# The lookahead window is four lines: the shape is always
#   for pid in $(pgrep ...); do
#       [ ... ] && continue
#       kill "$pid"
#   done
# The line reported is the selector, because the selector is the defect.
scan_files() {
  awk '
    function report(n, text) { printf "%s:%d: %s\n", FILENAME, n, text; found++ }
    FNR == 1 { pending = 0 }
    {
      line = $0
      s = line
      sub(/^[[:space:]]+/, "", s)
      if (s ~ /^#/) next

      # `kill -0` asks whether a pid exists. It signals nothing.
      if (line ~ /(^|[^[:alnum:]_])kill[[:space:]]+-0([[:space:]]|$)/) next

      if (line ~ /(^|[^[:alnum:]_])pkill([[:space:]]|$)/) { report(FNR, line); next }
      if (line ~ /(^|[^[:alnum:]_])killall([[:space:]]|$)/) { report(FNR, line); next }
      if (line ~ /xargs[^|]*[[:space:]]kill([[:space:]]|$)/) { report(FNR, line); next }
      if (line ~ /lsof[^|]*-[^|[:space:]]*t/ && line ~ /\|[^|]*kill([[:space:]]|$)/) { report(FNR, line); next }

      if (line ~ /(^|[^[:alnum:]_])pgrep([[:space:]]|$)/) {
        if (line ~ /(^|[^[:alnum:]_])kill([[:space:]]|$)/) { report(FNR, line); next }
        if (line ~ /\$\(pgrep/ || line ~ /`pgrep/) { pending = FNR + 3; sel_line = FNR; sel_text = line }
      }
      if (pending > 0 && FNR <= pending && line ~ /(^|[^[:alnum:]_])kill([[:space:]]|$)/) {
        report(sel_line, sel_text)
        pending = 0
      }
    }
    END { exit (found ? 1 : 0) }
  ' "$@" || true
}

# The scan list: tracked shell, workflow and hook files. Exclude this file,
# whose regexes and self-test text necessarily name the forbidden shapes.
scan_list() {
  git ls-files -z -- '*.sh' '*.yml' '*.yaml' '.githooks/*'
}

self_test() {
  local dir got want
  dir="$(mktemp -d)"
  trap 'rm -rf "$dir"' RETURN

  cat > "$dir/planted.sh" <<'PLANT'
pkill -f m6-http
for pid in $(pgrep -f "$PWD"); do
  [ "$pid" = "$$" ] && continue
  kill "$pid"
done
lsof -ti :8443 | xargs kill -9
ps -eo pid,args | awk '/m6-http/ {print $1}' | xargs kill
PLANT
  cat > "$dir/clean.sh" <<'CLEAN'
# pkill -f m6-http was here. It is prose now.
pgrep -f "$PWD" >/dev/null 2>&1 && echo alive
kill -0 "$$" 2>/dev/null || true
CLEAN

  got="$(scan_files "$dir/planted.sh")"
  want=4
  if [ "$(printf '%s\n' "$got" | grep -c . || true)" -ne "$want" ]; then
    echo "self-test: planted file: expected $want offenders, got:" >&2
    printf '%s\n' "$got" >&2
    return 1
  fi
  if [ -n "$(scan_files "$dir/clean.sh")" ]; then
    echo "self-test: a comment, a read-only pgrep and kill -0 must not be offenders:" >&2
    scan_files "$dir/clean.sh" >&2
    return 1
  fi
  echo "self-test OK: 4 planted kill-by-match lines found. Comment, read-only pgrep and kill -0 ignored."
}

if [ "${1:-}" = "--self-test" ]; then
  self_test
  exit 0
fi

case "${1:-}" in
  "") ROOT="$(cd "$SCRIPT_DIR/.." && pwd)" ;;
  --tree)
    [ $# -eq 2 ] && [ -n "$2" ] || { echo "usage: $0 [--tree <dir>|--self-test]" >&2; exit 2; }
    ROOT="$(cd "$2" 2>/dev/null && pwd)" || { echo "check-kill-safety: no such tree: $2" >&2; exit 2; }
    ;;
  *) echo "usage: $0 [--tree <dir>|--self-test]" >&2; exit 2 ;;
esac
cd "$ROOT"

# Do not report this script's own regexes and self-test text when it scans the
# repository it lives in. An empty SELF means the tree is another checkout.
SELF=""
case "$SCRIPT_DIR" in
  "$ROOT"/*) SELF="${SCRIPT_DIR#"$ROOT"/}/$(basename "$0")" ;;
esac

files=()
while IFS= read -r -d '' f; do
  [ -n "$SELF" ] && [ "$f" = "$SELF" ] && continue
  files+=("$f")
done < <(scan_list)

tip="$(git rev-parse --short=7 HEAD 2>/dev/null || echo unknown)"
offenders="$(scan_files "${files[@]}")"
if [ -n "$offenders" ]; then
  echo "FAIL: a teardown kills a process it selected by name, port or pattern:" >&2
  printf '%s\n' "$offenders" >&2
  echo >&2
  echo "Record each pid when the process starts and keep its start time beside it" >&2
  echo "(examples/own.sh: own_init, own_track, own_stop_records). Compare the start" >&2
  echo "time before signalling, so a reused pid is left alone. Do not select the" >&2
  echo "target with pgrep/pkill/lsof/xargs." >&2
  exit 1
fi

echo "kill safety at $tip: ${#files[@]} shell and workflow files, no process signalled by name, port or pattern. OK"
