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
#   tools/perfcheck.sh --runs 9   # rounds per target, default 5, minimum wins
#
# EVERY RUN APPENDS ONE LINE TO tools/perf-history.jsonl, in git. A number
# without the conditions it was taken under cannot be compared with anything
# later, so the record carries the commit, the toolchain, the CPU model and core
# count, the OS, total memory, and the load average BEFORE and AFTER the run. It
# keeps every raw reading, not only the minimum, so the spread is visible.
#
# It carries no hostname, address or domain. What class of machine, and how busy,
# is the question. Which machine is not.
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
# This paragraph said median while the code took the MINIMUM, from the day it was
# written until 2026-09-27. Owner's decision that day settled it in favour of what
# the comment already said: work with medians. A minimum is the best reading the
# machine ever managed, which is not what anyone's request costs. The median is
# the typical cost, and typical is what a latency number is for.
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
# Per-account. See the same change in tools/conformance.sh: a bare
# /tmp/m6-perfcheck is one directory shared by every account, and a root-owned
# one is unwritable to the account that runs the gate on a hardened build host.
WORK="${PERFCHECK_WORK:-/tmp/m6-perfcheck-$(id -un)}"

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
RUNS=5
HISTORY="$HERE/perf-history.jsonl"

while [[ $# -gt 0 ]]; do
  case "$1" in
    --update) UPDATE=true; shift ;;
    --margin) MARGIN="$2"; shift 2 ;;
    --runs)   RUNS="$2"; shift 2 ;;
    *) echo "usage: $0 [--update] [--margin PERCENT] [--runs N]"; exit 2 ;;
  esac
done

if ! [[ "$RUNS" =~ ^[0-9]+$ ]] || (( RUNS < 1 )); then
  echo "--runs takes a positive integer, got '$RUNS'" >&2
  exit 2
fi

GREEN='\033[0;32m'; RED='\033[0;31m'; YELLOW='\033[1;33m'; RESET='\033[0m'
pass() { echo -e "${GREEN}PASS${RESET} $*"; }
fail() { echo -e "${RED}FAIL${RESET} $*"; }
info() { echo -e "${YELLOW}----${RESET} $*"; }

mkdir -p "$WORK"
MEASURED="$WORK/measured.txt"
RAW="$WORK/raw.txt"
: > "$MEASURED"
: > "$RAW"
RESULT=0

# The load average BEFORE anything starts, because the measurement raises it
# itself. A reader needs what the box was already doing, not what this script
# did to it.
load_now() { uptime | sed 's/.*load average[s]*: *//; s/,.*//' | tr -d ' '; }
LOAD_BEFORE="$(load_now)"

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

  local sock="$WORK/render.sock"
  local -a readings=()
  # $RUNS rounds, take the MEDIAN. Owner's decision, 2026-09-27.
  #
  # A minimum is the best reading the machine ever managed and no request costs
  # that. A maximum is whatever else the box was doing. The median is the typical
  # cost, which is the number a latency floor should be made of.
  #
  # Every reading is kept, not only the median. A single figure cannot separate a
  # run that read 157us, 158us, 159us from one that read 157us, 340us, 890us, and
  # those two say very different things about the machine.
  for _ in $(seq 1 "$RUNS"); do
    rm -f "$sock"
    M6_SOCKET_OVERRIDE="$sock" "$bin" "$site" "$conf" --log-level error \
      > "$WORK/render.log" 2>&1 &
    local pid=$!
    PIDS+=("$pid")
    for _ in $(seq 1 100); do [[ -S "$sock" ]] && break; sleep 0.05; done
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
      readings+=("$got")
    fi
  done

  local all="${readings[*]:-}"
  printf '%s %s\n' "$key" "$all" >> "$RAW"
  info "$key: $RUNS rounds, ns: $all"

  # The median. For an even count, the mean of the two middle readings, which is
  # why this is integer arithmetic on nanoseconds rather than picking a side.
  local best=""
  if (( ${#readings[@]} > 0 )); then
    best="$(printf '%s\n' "${readings[@]}" | sort -n | awk '
      { a[NR] = $1 }
      END {
        if (NR % 2) { print a[(NR + 1) / 2] }
        else        { print int((a[NR / 2] + a[NR / 2 + 1]) / 2) }
      }')"
  fi

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

# ── The history record ────────────────────────────────────────────────────────
#
# Written on every run, pass or fail, before the verdict is printed. A run that
# failed is exactly the run a later reader wants the conditions for.
#
# It never blocks the check. A gate that fails because its bookkeeping failed is
# a gate that gets skipped, so a problem here is reported and the exit code is
# still the measurement's.
LOAD_AFTER="$(load_now)"
if have python3; then
  if python3 "$HERE/perf_history.py" "$HISTORY" "$RAW" "$BASELINE" \
       "$MARGIN" "$RUNS" "$LOAD_BEFORE" "$LOAD_AFTER"; then
    info "recorded in $(basename "$HISTORY"), load ${LOAD_BEFORE} before, ${LOAD_AFTER} after"
  else
    info "the history record failed to write. The measurement above still stands."
  fi
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
