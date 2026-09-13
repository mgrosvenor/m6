#!/usr/bin/env bash
# perfcheck.sh — measure the hot paths and refuse a merge that made them slower.
#
# The same shape as tools/conformance.sh: a recorded number per target in
# tools/perf-baseline.txt, and a run that comes in worse than its number by
# more than the allowed margin fails. A run that comes in better prints the new
# number and asks you to record it deliberately, in the commit that earned it.
#
# Usage:
#   tools/perfcheck.sh            # measure and compare
#   tools/perfcheck.sh --update   # record the measured numbers as the new ones
#   tools/perfcheck.sh --margin 15
#
# WHY A MARGIN. These are wall-clock numbers on a shared machine, so they move
# a few percent between runs for reasons that have nothing to do with the code.
# The default margin is 20%, which is wide enough that an idle laptop does not
# fail a correct change and narrow enough to catch the kind of regression that
# matters: this file exists because `App` spent 0.63 ms per page copying its own
# config for months and nothing noticed.
#
# WHY MEDIAN OF SEVERAL ROUNDS, INTERLEAVED WHERE THERE ARE TWO SIDES. One
# reading of one process is a reading of the afternoon. See the m6 handover's
# lesson 7 and docs/PERFORMANCE.md.
#
# WHERE IT RUNS. The build host, through deploy/run-tests.sh, and not the
# laptop's pre-push hook. A wall-clock measurement needs a quiet machine: this
# laptop sits at load 20-30 with the owner's own dev servers and preview
# instances running, and readings there moved by half between consecutive runs.
# The recorded numbers in perf-baseline.txt are therefore build-host numbers
# and mean nothing anywhere else. Same reasoning as the conformance check,
# which also only became real once it ran on the box that gates a deploy.
#
# WHAT IT DOES NOT DO. It does not compare against another commit. Doing that
# honestly means building both, and a pre-push check that builds twice is a
# check people learn to skip. The recorded numbers are the previous side of the
# comparison, which is what makes this cheap enough to actually run.

set -uo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
BASELINE="$HERE/perf-baseline.txt"
WORK="${PERFCHECK_WORK:-/tmp/m6-perfcheck}"
# The deployment repository, discovered rather than named: m6 is generic and does
# not know whose site this is. PERFCHECK_SITE still wins if set.
# shellcheck source=tools/find-deployment.sh
. "$HERE/find-deployment.sh"
SITE="${PERFCHECK_SITE:-$(_m6_find_deployment "$ROOT" || true)}"
UPDATE=false
MARGIN=20

while [[ $# -gt 0 ]]; do
  case "$1" in
    --update) UPDATE=true; shift ;;
    --margin) MARGIN="$2"; shift 2 ;;
    *) echo "usage: $0 [--update] [--margin PERCENT]"; exit 2 ;;
  esac
done

GREEN='\033[0;32m'; RED='\033[0;31m'; YELLOW='\033[1;33m'; RESET='\033[0m'
pass() { echo -e "${GREEN}PASS${RESET} $*"; }
fail() { echo -e "${RED}FAIL${RESET} $*"; }
info() { echo -e "${YELLOW}----${RESET} $*"; }

mkdir -p "$WORK"
MEASURED="$WORK/measured.txt"
: > "$MEASURED"
RESULT=0

PIDS=()
cleanup() { for p in "${PIDS[@]:-}"; do kill "$p" 2>/dev/null; done; }
trap cleanup EXIT INT TERM

baseline_for() { awk -v k="$1" '$1==k {print $2}' "$BASELINE" 2>/dev/null; }

# Compare a measurement against its recorded number.
#
# Lower is better for every target here; they are all nanoseconds. If that ever
# stops being true the unit column in the baseline file has to say so.
compare() {  # compare <key> <measured-ns>
  local key="$1" got="$2"
  local base; base="$(baseline_for "$key")"
  printf '%s %s\n' "$key" "$got" >> "$MEASURED"

  if [[ -z "$base" ]]; then
    info "$key: ${got}ns (no number recorded yet — record it with --update)"
    return
  fi

  local ceiling=$(( base + base * MARGIN / 100 ))
  if (( got > ceiling )); then
    fail "$key: ${got}ns against ${base}ns, which is over the ${MARGIN}% margin (ceiling ${ceiling}ns)."
    info "  this change made it slower, OR the machine was busy while measuring."
    info "  This check is meant to run on the build host, which is quiet. On a"
    info "  loaded laptop it will fail on correct code: measured readings there"
    info "  moved 50% between runs while a release build was going."
    RESULT=1
  elif (( got * 100 < base * 90 )); then
    pass "$key: ${got}ns against ${base}ns — faster. Record it with --update, in the commit that earned it."
  else
    pass "$key: ${got}ns against ${base}ns"
  fi
}

have() { command -v "$1" >/dev/null 2>&1; }

# ── Target: a rendered HTML page, end to end over the socket ──────────────────
#
# The one number that covers the most code: routing, the request dictionary,
# the template engine, compression and the response writer. It is also where
# the copy work landed, so it is the number that would have caught it.
measure_render() {
  local conf="$SITE/deploy/rendered/prod/configs/m6-html.conf"
  if [[ ! -f "$conf" ]]; then
    fail "render:capabilities — no site at $SITE, so nothing was measured."
    info "  set PERFCHECK_SITE, or check out the site repo beside this one."
    RESULT=1
    return 1
  fi
  local bin="$ROOT/target/release/m6-html"
  if [[ ! -x "$bin" ]]; then
    fail "render:capabilities — $bin is missing. Build release first."
    RESULT=1
    return 1
  fi

  local sock="$WORK/render.sock" best=""
  # Five rounds, take the minimum. The minimum is the least contaminated by
  # whatever else the machine was doing, which is the question being asked:
  # what the code costs, not what the box was busy with.
  for _ in 1 2 3 4 5; do
    rm -f "$sock"
    M6_SOCKET_OVERRIDE="$sock" "$bin" "$SITE" "$conf" --log-level error \
      > "$WORK/render.log" 2>&1 &
    local pid=$!
    PIDS+=("$pid")
    local i
    for i in $(seq 1 100); do [[ -S "$sock" ]] && break; sleep 0.05; done
    if [[ ! -S "$sock" ]]; then
      fail "render:capabilities — m6-html never bound $sock. See $WORK/render.log"
      RESULT=1
      kill "$pid" 2>/dev/null
      return 1
    fi
    local got
    got="$(python3 "$HERE/perfcheck_client.py" "$sock" /capabilities 200 2>/dev/null)"
    kill "$pid" 2>/dev/null
    wait "$pid" 2>/dev/null
    if [[ -n "$got" ]]; then
      [[ -z "$best" || "$got" -lt "$best" ]] && best="$got"
    fi
  done

  if [[ -z "$best" ]]; then
    fail "render:capabilities — measured nothing. A check that cannot measure must fail."
    RESULT=1
    return 1
  fi
  compare "render:capabilities" "$best"
}

# WANTED, NOT PRESENT: a second target covering m6-http's own request path.
# The obvious one is the cache lookup, and a first attempt at it parsed
# criterion's output and could not, so it printed "skipping" and returned
# success. That is precisely the failure just removed from the conformance
# check, and shipping it here would have been the same mistake in a new file.
# One target that always runs beats two where one quietly does not.

info "Performance check (margin ${MARGIN}%)"
measure_render

if [[ "$UPDATE" == "true" ]]; then
  cp "$BASELINE" "$BASELINE.new" 2>/dev/null || : > "$BASELINE.new"
  while read -r k v; do
    [[ -z "$k" ]] && continue
    if grep -qE "^${k}[[:space:]]" "$BASELINE.new"; then
      awk -v key="$k" -v val="$v" '$1 == key { printf "%-26s %s\n", key, val; next } { print }' \
        "$BASELINE.new" > "$BASELINE.tmp" && mv "$BASELINE.tmp" "$BASELINE.new"
    else
      printf '%-26s %s\n' "$k" "$v" >> "$BASELINE.new"
    fi
  done < "$MEASURED"
  mv "$BASELINE.new" "$BASELINE"
  info "numbers recorded in $BASELINE — commit this with the change that earned it"
  exit 0
fi

echo
if (( RESULT == 0 )); then
  pass "performance: nothing got slower"
else
  fail "performance: see above"
fi
exit $RESULT
