#!/usr/bin/env bash
# hwcounters.sh: page faults, TLB and cache misses per request on a warmed-up
# m6-http answering hot cache hits, for a base build and a candidate side by side.
#
# WHY. With the cache hit path allocating nothing, memory pinned and the cache
# on huge pages, a hot hit should cost almost no page faults and almost no TLB
# misses. This reads the counters that say whether it does, per request.
#
# The window, per arm:
#   1. start m6-html, m6-file and m6-http and wait until they answer,
#   2. fill the cache with --fill-mib of static files (the cache is then full,
#      which is when the TLB reach matters), then warm up with --warmup hits,
#   3. `perf stat` attached to m6-http while --requests hot hits go through one
#      keep-alive connection,
#   4. divide each counter by the requests and print one row per event.
#
# THE HOST. The build host is a VM, and a VM may expose no hardware counters.
# The script runs `perf stat -e cycles true` first. When that gives no count it
# prints "hardware counters not available on this host" and reports only the
# software events (page-faults, minor-faults, major-faults), which the kernel
# counts anywhere. It never prints a number it did not read.
#
# USAGE
#   tools/hwcounters.sh base=DIR [cand=DIR] [--requests N] [--warmup N] [--fill-mib N]
#
# DIR holds the built m6-http, m6-html and m6-file (target/release of a checkout).
# Defaults: 10000 requests, 2000 warm-up, 128 MiB of fill (the default
# cache_bytes). `--fill-mib 0` skips the fill.
#
# PRIVILEGE. perf must attach to a process it did not start: run as root
# (MEMCALLS_SUDO=sudo) or lower kernel.perf_event_paranoid and
# kernel.yama.ptrace_scope.

set -uo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
WORK="${MEMCALLS_WORK:-/tmp/m6-hwcounters-$(id -un)}"
SUDO="${MEMCALLS_SUDO:-}"
PERF="${MEMCALLS_PERF:-perf}"
PORT="${MEMCALLS_PORT:-18444}"
REQUESTS=10000
WARMUP=2000
FILL_MIB=128
ARMS=()

SOFT="page-faults,minor-faults,major-faults"
HARD="dTLB-load-misses,iTLB-load-misses,cache-misses,LLC-load-misses,instructions,cycles"

usage() {
  sed -n '/^# USAGE/,/^# cache_bytes/p' "$0" | sed 's/^# \{0,1\}//' >&2
  exit 2
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --requests) REQUESTS="$2"; shift 2 ;;
    --warmup) WARMUP="$2"; shift 2 ;;
    --fill-mib) FILL_MIB="$2"; shift 2 ;;
    base=* | cand=*) ARMS+=("$1"); shift ;;
    *) usage ;;
  esac
done
((${#ARMS[@]} > 0)) || usage
[[ "$REQUESTS" =~ ^[0-9]+$ && "$WARMUP" =~ ^[0-9]+$ && "$FILL_MIB" =~ ^[0-9]+$ ]] \
  && ((REQUESTS > 0 && WARMUP > 0)) || { echo "hwcounters: counts are whole numbers, requests and warmup above zero" >&2; usage; }
for tool in openssl curl "$PERF" cc head; do
  command -v "$tool" >/dev/null 2>&1 || { echo "hwcounters: $tool not found" >&2; exit 2; }
done

# shellcheck source=tools/memcalls-site.sh
. "$HERE/memcalls-site.sh"
trap stop_all EXIT INT TERM

# Whether this host counts hardware events. A VM without a virtual PMU prints
# "<not supported>" for cycles, and a count is a number.
EVENTS="$SOFT"
if $SUDO "$PERF" stat -x, -e cycles true 2>&1 | grep -Eq '^[0-9]+,'; then
  EVENTS="$SOFT,$HARD"
else
  echo "hardware counters not available on this host: reporting the software events only"
fi

# report <label> <perf-csv> <requests>: one row per event, the count and the
# count per request. An event perf could not count is "not counted".
report() {
  echo "---- $1: $3 hot cache hits"
  printf '  %-22s %16s %14s\n' event count per-request
  awk -F, -v n="$3" '
    NF >= 3 && $3 != "" {
      if ($1 ~ /^[0-9]+$/) printf "  %-22s %16d %14.3f\n", $3, $1, $1 / n
      else printf "  %-22s %16s %14s\n", $3, "not counted", "-"
    }' "$2"
}

run_arm() {
  local label="${1%%=*}" dir="${1#*=}" file
  for file in m6-http m6-html m6-file; do
    [[ -x "$dir/$file" ]] || { echo "hwcounters: $dir/$file is not executable" >&2; return 1; }
  done
  echo "==== $label ($dir)"
  start_stack "$dir" "$label" || return 1
  local url="https://127.0.0.1:$PORT/" i
  if ((FILL_MIB > 0)); then
    : > "$WORK/fill.curl"
    for ((i = 0; i < FILL_MIB * 4; i++)); do
      printf 'url = "https://127.0.0.1:%s/assets/blob-%s.bin"\noutput = "/dev/null"\n' "$PORT" "$i" >> "$WORK/fill.curl"
    done
    curl -sk --http1.1 -K "$WORK/fill.curl" || echo "hwcounters: the fill failed" >&2
  fi
  url_file "$WARMUP" "$url" "$WORK/warm.curl"
  url_file "$REQUESTS" "$url" "$WORK/load.curl"
  curl -sk --http1.1 --fail -K "$WORK/warm.curl" || echo "hwcounters: warm-up failed" >&2
  grep -h "heap:" "$WORK/http-$label.log" | head -1
  local out="$WORK/perf-$label.csv"
  $SUDO "$PERF" stat -x, -e "$EVENTS" -p "$HTTP_PID" -o "$out" -- \
    curl -sk --http1.1 --fail -K "$WORK/load.curl" > "$WORK/load-$label.log" 2>&1 \
    || { echo "hwcounters: perf stat failed, see $WORK/load-$label.log" >&2; return 1; }
  report "$label" "$out" "$REQUESTS"
  printf '  %-22s %s KiB\n' "rss" "$(rss_kb "$HTTP_PID")"
  stop_all
}

make_site "$FILL_MIB" || exit 2
for arm in "${ARMS[@]}"; do run_arm "$arm" || exit 2; done
