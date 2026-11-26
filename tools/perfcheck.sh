#!/usr/bin/env bash
# perfcheck.sh: an interleaved, pinned A/B of a candidate build against a base build.
#
# A recorded number in a file stopped meaning anything on 2026-09-27: the
# content it was measured against changed, and the machine it was measured on
# moved. This script therefore builds and compares two binaries, the candidate
# and a base, so both arms render the same bytes on the same machine in the
# same minute. The verdict is made by the tested Rust module behind the
# m6-perf-ab binary, never by this script.
#
# WHY INTERLEAVED. The two arms alternate each round, base then candidate on odd
# rounds and candidate then base on even rounds, so drift that grows through the
# run lands on both arms equally and cancels in the paired difference.
#
# WHY PINNED. The server runs on one CPU and the C client on another, so the
# client's own work never competes with the server for a CPU, and every thread
# of the server shares one CPU and its cache, the layout a single-threaded
# event-loop server will run under.
#
# WHY A C CLIENT. The previous client was Python, and its own interpreter time
# sat inside every reading. It also never checked the status line, so a 404 or a
# 500 was timed as a fast render. The C client checks every status line and
# times only the socket work.
#
# EVERY RUN APPENDS ONE LINE TO tools/perf-history.jsonl, in git. A number
# without the conditions it was taken under cannot be compared with anything
# later, so the record carries the commit, the toolchain, the CPU model and core
# count, the OS, total memory, and the load average BEFORE and AFTER the run. It
# keeps every raw reading, not only the medians, so the spread is visible.
#
# It carries no hostname, address or domain. What class of machine, and how busy,
# is the question. Which machine is not.
#
# WHERE IT RUNS. The build host, and not the laptop's pre-push hook. A wall-clock
# measurement needs a quiet machine. This laptop sits at load 20-30 with dev
# servers and preview instances running, and readings there moved by half
# between consecutive runs. Same reasoning as the conformance check, which also
# only became real once it ran on the box that gates a deploy.

set -uo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
# Per-account. A bare /tmp/m6-perfcheck is one directory shared by every
# account, and a root-owned one is unwritable to the account that runs the gate
# on a hardened build host.
#
# The check compiles perf_client into this directory and runs it, so it must be
# on a mount that allows exec. A hardened host mounts /tmp noexec, and every
# client there exits 126, so the first candidate a small script can run from is
# used: /tmp, then /var/tmp, then the account's cache directory. This is the
# rule m6_core::platform::scratch_dir applies.
exec_dir() {
  local base dir probe
  for base in /tmp /var/tmp "${XDG_CACHE_HOME:-$HOME/.cache}"; do
    dir="$base/m6-perfcheck-$(id -un)"
    mkdir -p "$dir" 2>/dev/null || continue
    probe="$dir/.exec-probe-$$"
    if printf '#!/bin/sh\nexit 0\n' > "$probe" 2>/dev/null &&
       chmod 700 "$probe" 2>/dev/null && "$probe" 2>/dev/null; then
      rm -f "$probe"
      echo "$dir"
      return 0
    fi
    rm -f "$probe"
  done
  return 1
}
WORK="${PERFCHECK_WORK:-$(exec_dir)}" ||
  { echo "perfcheck: no work directory allows exec; set PERFCHECK_WORK" >&2; exit 2; }

# The examples directory of this repository holds the committed bytes both arms
# render. PERFCHECK_EXAMPLES overrides its location.
# PERFCHECK_SITE still points the whole check at a deployment instead, for a
# deployment that wants to measure its own content.
EXAMPLES="${PERFCHECK_EXAMPLES:-$ROOT/examples}"
SITE="${PERFCHECK_SITE:-}"
HISTORY="${PERFCHECK_HISTORY:-$HERE/perf-history.jsonl}"

BASE_BIN=""
CAND_BIN="$ROOT/target/release/m6-html"
BASE_EXAMPLES=""
# 100 rounds, not 30: each round starts a fresh m6-html and each settles at its
# own speed, so the round medians span a wide range and more requests per round
# cannot narrow it. More rounds average that out; 100 rounds of 500 requests
# keeps every CI99 width inside the 10% cap on an A/A.
ROUNDS=100
REQUESTS=500
THRESHOLD=3
MAX_CI_WIDTH=10

# shellcheck source=tools/perfcheck-stray.sh
. "$HERE/perfcheck-stray.sh"

usage() {
  cat <<'EOF'
usage: tools/perfcheck.sh --base-bin PATH [options]

  --base-bin PATH       the base m6-html (required)
  --cand-bin PATH       the candidate m6-html (default target/release/m6-html)
  --base-examples DIR   the examples the base arm renders (default: the candidate's)
  --rounds N            rounds per target (default 100)
  --requests N          requests per round (default 500)
  --threshold PCT       slower verdict threshold, percent (default 3)
  --max-ci-width PCT    interval width that marks a run not measured (default 10)

Environment:
  PERFCHECK_EXAMPLES      examples directory (default: examples/ in this repository)
  PERFCHECK_SITE          measure a deployment's render:capabilities instead
  PERFCHECK_WORK          work directory
  PERFCHECK_HISTORY       history file (default tools/perf-history.jsonl)
  PERFCHECK_CPUS          client,server CPUs to pin
  PERFCHECK_UNPINNED      set to run without taskset; the result is not measured
  PERFCHECK_ACCEPT_SLOWER a reason that accepts a deliberate regression
EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --base-bin) BASE_BIN="$2"; shift 2 ;;
    --cand-bin) CAND_BIN="$2"; shift 2 ;;
    --base-examples) BASE_EXAMPLES="$2"; shift 2 ;;
    --rounds) ROUNDS="$2"; shift 2 ;;
    --requests) REQUESTS="$2"; shift 2 ;;
    --threshold) THRESHOLD="$2"; shift 2 ;;
    --max-ci-width) MAX_CI_WIDTH="$2"; shift 2 ;;
    *) usage; exit 2 ;;
  esac
done

if [[ -z "$BASE_BIN" ]]; then
  echo "perfcheck: --base-bin is required" >&2
  usage
  exit 2
fi
if [[ ! -x "$BASE_BIN" ]]; then
  echo "perfcheck: $BASE_BIN is not executable" >&2
  usage
  exit 2
fi

# Both binaries are resolved to absolute paths and used as-is everywhere, so the
# leftover-server check after the loop matches exactly the processes this run
# started, not another checkout's servers that share a relative path.
BASE_BIN="$(resolve_abs "$BASE_BIN")"
CAND_BIN="$(resolve_abs "$CAND_BIN")"

if ! [[ "$ROUNDS" =~ ^[0-9]+$ ]] || (( ROUNDS < 1 )); then
  echo "perfcheck: --rounds takes a positive integer, got '$ROUNDS'" >&2
  usage; exit 2
fi
if ! [[ "$REQUESTS" =~ ^[0-9]+$ ]] || (( REQUESTS < 1 )); then
  echo "perfcheck: --requests takes a positive integer, got '$REQUESTS'" >&2
  usage; exit 2
fi
if ! [[ "$THRESHOLD" =~ ^[0-9]+(\.[0-9]+)?$ ]]; then
  echo "perfcheck: --threshold takes a non-negative number, got '$THRESHOLD'" >&2
  usage; exit 2
fi
if ! [[ "$MAX_CI_WIDTH" =~ ^[0-9]+(\.[0-9]+)?$ ]]; then
  echo "perfcheck: --max-ci-width takes a non-negative number, got '$MAX_CI_WIDTH'" >&2
  usage; exit 2
fi

# A wall-clock number is only worth anything on an idle host, so this takes the
# build host to itself through tools/host-lock.sh, or reports NOT MEASURED and
# fails. A caller already holding the host (build-host-tests.sh) has exported
# M6_HOST_LOCK_HELD=exclusive, and the call returns at once.
# shellcheck source=tools/host-lock.sh
. "$HERE/host-lock.sh"
host_alone || exit 2

mkdir -p "$WORK"
CLIENT="$WORK/perf_client"
ROUNDS_FILE="$WORK/rounds.txt"
: > "$ROUNDS_FILE"

# The C client, standard library and POSIX only, built with the same flags CI
# uses so a warning here is a warning there.
if ! cc -O2 -std=c11 -Wall -Wextra -Werror -o "$CLIENT" "$HERE/perf_client.c" \
    2> "$WORK/cc.log"; then
  echo "perfcheck: building the C client failed" >&2
  cat "$WORK/cc.log" >&2
  exit 2
fi

# The verdict binary. If it is missing, build it; a failure to build it is a
# check that cannot measure, and fails.
AB_BIN="$ROOT/target/release/m6-perf-ab"
if [[ ! -x "$AB_BIN" ]]; then
  if ! (cd "$ROOT" && cargo build --release -p m6-http --bin m6-perf-ab) \
      > "$WORK/build-ab.log" 2>&1; then
    echo "perfcheck: building m6-perf-ab failed" >&2
    cat "$WORK/build-ab.log" >&2
    exit 2
  fi
fi

# ── The machine facts the record needs ────────────────────────────────────────
#
# Each is empty when its command fails, so a fact this machine cannot report is
# a null in the record rather than a number it did not take.
machine_cpu() {
  if [[ -r /proc/cpuinfo ]]; then
    awk -F: '/^model name/ { sub(/^[ \t]+/, "", $2); print $2; exit }' /proc/cpuinfo 2>/dev/null
  else
    sysctl -n machdep.cpu.brand_string 2>/dev/null
  fi
}

machine_cores() {
  if command -v lscpu >/dev/null 2>&1; then
    local n
    n="$(lscpu -p=Core,Socket 2>/dev/null | grep -v '^#' | sort -u | grep -c .)"
    if [[ "$n" =~ ^[0-9]+$ ]] && (( n > 0 )); then echo "$n"; return; fi
  fi
  sysctl -n hw.physicalcpu 2>/dev/null
}

machine_mem_kb() {
  if [[ -r /proc/meminfo ]]; then
    awk '/^MemTotal:/ { print $2; exit }' /proc/meminfo 2>/dev/null
  else
    local b
    b="$(sysctl -n hw.memsize 2>/dev/null)"
    [[ "$b" =~ ^[0-9]+$ ]] && echo $(( b / 1024 ))
  fi
}

M6_PERF_CPU="$(machine_cpu)"
M6_PERF_CORES="$(machine_cores)"
M6_PERF_MEM_KB="$(machine_mem_kb)"
M6_PERF_OS="$(uname -sr 2>/dev/null)"
M6_PERF_ARCH="$(uname -m 2>/dev/null)"
M6_PERF_TOOLCHAIN="$(rustc --version 2>/dev/null)"
M6_PERF_TS="$(date -u +%Y-%m-%dT%H:%M:%SZ 2>/dev/null)"
export M6_PERF_CPU M6_PERF_CORES M6_PERF_MEM_KB M6_PERF_OS M6_PERF_ARCH M6_PERF_TOOLCHAIN M6_PERF_TS
# The commit and dirtiness come from the checkout. build-host-tests.sh sets them
# first, because the tree it synced to the host has no .git to ask.
if [[ -z "${M6_PERF_COMMIT:-}" ]]; then
  M6_PERF_COMMIT="$(git -C "$ROOT" rev-parse HEAD 2>/dev/null)"
  export M6_PERF_COMMIT
fi
if [[ -z "${M6_PERF_DIRTY:-}" ]]; then
  if [[ -z "$(git -C "$ROOT" status --porcelain 2>/dev/null)" ]]; then
    M6_PERF_DIRTY=0
  else
    M6_PERF_DIRTY=1
  fi
  export M6_PERF_DIRTY
fi

# ── The CPUs ──────────────────────────────────────────────────────────────────
#
# The client CPU is the lowest CPU on the lowest core, the server CPU the lowest
# CPU on the highest core. Two cores, so the client's own work never competes
# with the server, and every thread of the server shares one CPU and its cache.
# PERFCHECK_CPUS replaces the choice, and PERFCHECK_UNPINNED runs without
# taskset, which makes the result always not measured.
PINNED=1
CLIENT_CPU=""
SERVER_CPU=""
if [[ "${PERFCHECK_UNPINNED:-0}" == "1" ]]; then
  PINNED=0
elif [[ -n "${PERFCHECK_CPUS:-}" ]]; then
  CLIENT_CPU="${PERFCHECK_CPUS%%,*}"
  SERVER_CPU="${PERFCHECK_CPUS##*,}"
else
  if ! command -v taskset >/dev/null 2>&1 || ! command -v lscpu >/dev/null 2>&1; then
    echo "NOT MEASURED: taskset/lscpu missing" >&2
    exit 2
  fi
  map="$(lscpu -p=CPU,CORE 2>/dev/null \
    | awk -F, '$1 ~ /^[0-9]+$/ && $2 ~ /^[0-9]+$/ {print $1","$2}')"
  [[ -n "$map" ]] || { echo "NOT MEASURED: lscpu gave no CPU map" >&2; exit 2; }
  low_core="$(printf '%s\n' "$map" | awk -F, '{print $2}' | sort -n | head -1)"
  high_core="$(printf '%s\n' "$map" | awk -F, '{print $2}' | sort -n | tail -1)"
  CLIENT_CPU="$(printf '%s\n' "$map" | awk -F, -v c="$low_core" '$2==c {print $1}' | sort -n | head -1)"
  SERVER_CPU="$(printf '%s\n' "$map" | awk -F, -v c="$high_core" '$2==c {print $1}' | sort -n | head -1)"
  if [[ "$low_core" == "$high_core" ]]; then
    echo "NOT MEASURED: one core, the client and server would share it" >&2
    exit 2
  fi
fi

# Run a command pinned to one CPU, or unpinned when the run asked for that.
# `exec` replaces the subshell that `&` and `$()` fork, so `$!` is the command
# itself and `kill` stops it rather than a now-empty wrapper shell.
run_pinned() {
  local cpu="$1"; shift
  if (( PINNED == 1 )); then
    exec taskset -c "$cpu" "$@"
  else
    exec "$@"
  fi
}

load_now() {
  if [[ -r /proc/loadavg ]]; then
    cut -d' ' -f1 /proc/loadavg
  else
    uptime | sed -E 's/.*load averages?: *//; s/[ ,].*//'
  fi
}
LOAD_BEFORE="$(load_now)"

SERVER_PID=""
cleanup() { [[ -n "$SERVER_PID" ]] && kill "$SERVER_PID" 2>/dev/null; }
trap cleanup EXIT INT TERM

# ── Targets ───────────────────────────────────────────────────────────────────
#
# A rendered HTML page, end to end over the socket: routing, the request
# dictionary, the template engine, compression and the response writer. Three
# of them, because a page has more than one cost and one target cannot separate
# them.
#
#   render:minimal     example 01, `/`. Almost no content, so what is left is the
#                      FIXED per-request cost, undiluted.
#   render:blog-index  example 05, `/blog`. The post index rendered from the
#                      committed posts.json, the per-item cost.
#   render:no-store    example 05, `/login`. A cache = "no-store" route, the page
#                      every visitor pays for because no cache keeps it.
#
# The base arm can render another examples checkout (--base-examples), which is
# what P2 uses to pair an old build with old content.
BASE_EXAMPLES_DIR="${BASE_EXAMPLES:-$EXAMPLES}"

target_rows() {
  if [[ -n "$SITE" ]]; then
    printf '%s\n' "render:capabilities /capabilities __site__"
  else
    printf '%s\n' "render:minimal / 01-static"
    printf '%s\n' "render:blog-index /blog 05-cms"
    printf '%s\n' "render:no-store /login 05-cms"
  fi
}

arm_site() {
  local arm="$1" subdir="$2" ex
  if [[ "$arm" == "base" ]]; then ex="$BASE_EXAMPLES_DIR"; else ex="$EXAMPLES"; fi
  echo "$ex/$subdir"
}

run_round() {
  local key="$1" r="$2" arm="$3" site="$4" conf="$5" path="$6" bin="$7"
  local sock="$WORK/render.sock" log="$WORK/render-$key-$arm.log"
  rm -f "$sock"
  # A fresh socket per round, and the closed descriptors keep the server from
  # inheriting the host lock.
  M6_SOCKET_OVERRIDE="$sock" \
    run_pinned "$SERVER_CPU" "$bin" "$site" "$conf" --log-level error \
    > "$log" 2>&1 6<&- 8<&- &
  local pid=$! i
  SERVER_PID="$pid"
  for ((i = 0; i < 100; i++)); do
    [[ -S "$sock" ]] && break
    sleep 0.05
  done
  if [[ ! -S "$sock" ]]; then
    echo "perfcheck: $key ($arm) never bound $sock; see $log" >&2
    kill "$pid" 2>/dev/null; wait "$pid" 2>/dev/null
    SERVER_PID=""
    return 1
  fi
  local out rc
  out="$(run_pinned "$CLIENT_CPU" "$CLIENT" "$sock" "$path" "$REQUESTS")"
  rc=$?
  kill "$pid" 2>/dev/null; wait "$pid" 2>/dev/null
  SERVER_PID=""
  if (( rc != 0 )); then
    echo "perfcheck: $key ($arm) client failed (exit $rc); see $log" >&2
    return 1
  fi
  printf '%s %s %s %s\n' "$key" "$r" "$arm" "$out" >> "$ROUNDS_FILE"
}

# ── The run ───────────────────────────────────────────────────────────────────
for ((r = 1; r <= ROUNDS; r++)); do
  if (( r % 2 == 1 )); then
    arms=(base cand)
  else
    arms=(cand base)
  fi
  while read -r key path subdir; do
    for arm in "${arms[@]}"; do
      bin="$BASE_BIN"; site=""; conf=""
      if [[ "$arm" == "base" ]]; then bin="$BASE_BIN"; else bin="$CAND_BIN"; fi
      if [[ "$subdir" == "__site__" ]]; then
        site="$SITE"
        conf="$SITE/deploy/rendered/prod/configs/m6-html.conf"
      else
        site="$(arm_site "$arm" "$subdir")"
        conf="$site/configs/m6-html.conf"
      fi
      run_round "$key" "$r" "$arm" "$site" "$conf" "$path" "$bin"
    done
  done < <(target_rows)
done

# ── No strays ─────────────────────────────────────────────────────────────────
#
# Each round kills its server and waits for it, so no m6-html from this run
# should still be alive when the loop finishes. A stray server pins the measured
# CPU and corrupts every reading, so a run that leaves one behind fails rather
# than reporting a number. Both the base and candidate binaries are checked, at
# the absolute paths the run started, so a stray in either arm is caught.
deadline=$(( SECONDS + 5 ))
while stray_alive "$BASE_BIN" "$CAND_BIN"; do
  if (( SECONDS >= deadline )); then
    echo "NOT MEASURED: m6-html from this run is still alive after the run" >&2
    exit 2
  fi
  sleep 0.2
done

# ── The verdict ───────────────────────────────────────────────────────────────
#
# m6-perf-ab pairs the arms, decides, prints the table and appends the history
# record, then this script exits with its status. M6_PERF_COMMIT, M6_PERF_DIRTY
# and M6_PERF_BASE_COMMIT pass through unchanged. A check that measured nothing
# fails here rather than reporting a pass: an empty rounds file means every
# round failed, and m6-perf-ab must not read that as "no slowdown".
if [[ ! -s "$ROUNDS_FILE" ]]; then
  echo "NOT MEASURED: no rounds were measured, the rounds file is empty" >&2
  exit 2
fi
LOAD_AFTER="$(load_now)"
"$AB_BIN" "$ROUNDS_FILE" "$HISTORY" "$THRESHOLD" "$MAX_CI_WIDTH" "$ROUNDS" "$REQUESTS" \
  "$LOAD_BEFORE" "$LOAD_AFTER" "$PINNED" "$CLIENT_CPU" "$SERVER_CPU"
rc=$?
exit $rc
