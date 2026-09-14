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
# WHERE IT RUNS. The build host, and not the
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

# ── What is measured, and where it comes from ─────────────────────────────────
#
# The examples repository, checked out beside this one. It used to be a
# deployment's `deploy/rendered/prod/configs/m6-html.conf`, which meant m6 could
# not measure itself: a bare checkout with no site beside it failed the check
# rather than running it, and the recorded number belonged to one particular
# site's content.
#
# The examples are m6's own, they are version-controlled with it in mind, and
# their content is committed, so the same bytes are rendered on every machine
# and the number means the same thing twice. PERFCHECK_EXAMPLES overrides the
# location; PERFCHECK_SITE still points the whole check at a deployment instead,
# for a deployment that wants to measure its own content.
EXAMPLES="${PERFCHECK_EXAMPLES:-$(cd "$ROOT/.." 2>/dev/null && pwd)/m6-examples}"
SITE="${PERFCHECK_SITE:-}"
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

# ── Targets: a rendered HTML page, end to end over the socket ─────────────────
#
# The one kind of number that covers the most code: routing, the request
# dictionary, the template engine, compression and the response writer. It is
# also where the copy work landed, so it is the number that would have caught it.
#
# Two of them, because a page has two costs and one target cannot separate them:
#
#   render:minimal     example 01, `/`. A 10-line template over 185 bytes of
#                      params. Almost no content, which is the point: what is
#                      left is the FIXED per-request cost, undiluted. The defect
#                      this file exists for was exactly that shape -- `App` spent
#                      0.63 ms per page copying its own config, regardless of how
#                      big the page was -- and it shows up most clearly here.
#
#   render:blog-index  example 05, `/blog`. The post index rendered from 113 KB
#                      of committed posts.json, iterating every post. This is the
#                      per-item cost, which a minimal page cannot see at all.
#
# A regression in one and not the other says where to look, which one number
# never did.
measure_one() {
  local key="$1" site="$2" conf="$3" path="$4"

  if [[ ! -f "$conf" ]]; then
    fail "$key — no config at $conf, so nothing was measured."
    info "  The examples are expected beside this checkout:"
    info "    git clone https://github.com/mgrosvenor/m6-examples"
    info "  Or set PERFCHECK_EXAMPLES, or PERFCHECK_SITE for a deployment."
    RESULT=1
    return 1
  fi
  local bin="$ROOT/target/release/m6-html"
  if [[ ! -x "$bin" ]]; then
    fail "$key — $bin is missing. Build release first."
    RESULT=1
    return 1
  fi

  local sock="$WORK/render.sock" best=""
  # Five rounds, take the minimum. The minimum is the least contaminated by
  # whatever else the machine was doing, which is the question being asked:
  # what the code costs, not what the box was busy with.
  for _ in 1 2 3 4 5; do
    rm -f "$sock"
    M6_SOCKET_OVERRIDE="$sock" "$bin" "$site" "$conf" --log-level error \
      > "$WORK/render.log" 2>&1 &
    local pid=$!
    PIDS+=("$pid")
    local i
    for i in $(seq 1 100); do [[ -S "$sock" ]] && break; sleep 0.05; done
    if [[ ! -S "$sock" ]]; then
      fail "$key — m6-html never bound $sock. See $WORK/render.log"
      RESULT=1
      kill "$pid" 2>/dev/null
      return 1
    fi
    local got
    got="$(python3 "$HERE/perfcheck_client.py" "$sock" "$path" 200 2>/dev/null)"
    kill "$pid" 2>/dev/null
    wait "$pid" 2>/dev/null
    if [[ -n "$got" ]]; then
      [[ -z "$best" || "$got" -lt "$best" ]] && best="$got"
    fi
  done

  if [[ -z "$best" ]]; then
    fail "$key — measured nothing. A check that cannot measure must fail."
    RESULT=1
    return 1
  fi
  compare "$key" "$best"
}

info "Performance check (margin ${MARGIN}%)"

if [[ -n "$SITE" ]]; then
  # A deployment measuring its own content. Its layout, its target name.
  measure_one "render:capabilities" "$SITE" \
    "$SITE/deploy/rendered/prod/configs/m6-html.conf" /capabilities
else
  measure_one "render:minimal" "$EXAMPLES/examples/01-static" \
    "$EXAMPLES/examples/01-static/configs/m6-html.conf" /
  measure_one "render:blog-index" "$EXAMPLES/examples/05-cms" \
    "$EXAMPLES/examples/05-cms/configs/m6-html.conf" /blog
fi

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
