#!/usr/bin/env bash
# memcalls.sh: count the kernel memory calls and minor page faults a warm
# service makes while it serves steady load, for one build or two side by side.
#
# WHY. The allocator's cost that matters on a warm service is the call into the
# kernel (brk, mmap, munmap, madvise, mremap, mprotect), not the time in the
# allocator. A warm service should make none. This attaches strace to the
# RUNNING service after a warm-up, so start-up calls are not counted, and reads
# the table.
#
# The window, for each target:
#   1. start the service(s) and wait until they answer,
#   2. warm up with --warmup requests,
#   3. attach `strace -c -f` to every thread of the service,
#   4. send --requests more,
#   5. detach (SIGTERM to strace, which prints its table; SIGINT is ignored in a script's background job) and read the table.
# The minor page faults come from /proc/PID/stat over the same window, and four
# figures are read after it: the resident set (`VmRSS`) and the address space it
# sits in (`VmSize`), the memory the pin holds (`VmLck`), and the part on
# transparent huge pages (`AnonHugePages`). A process that is resident to its
# whole address space shows `rss` at `mapped`, which is what a pin that populates
# every reserved mapping looks like.
#
# Targets:
#   m6-http cache hit    m6-http over TLS HTTP/1.1, one cacheable page, every
#                        request after the first answered from its cache
#   m6-html render       m6-html on its socket, a page rendered every request
#
# USAGE
#   tools/memcalls.sh base=DIR [cand=DIR] [--requests N] [--warmup N]
#
# DIR holds the built m6-http and m6-html (target/release of a checkout).
# Run it for the base and the candidate in one invocation so both arms see the
# same host in the same minute. Nothing in it is a test: it is a measurement.
#
# PRIVILEGE. strace must attach to a process it did not start. Run as root
# (MEMCALLS_SUDO=sudo) or set kernel.yama.ptrace_scope to 0. The script says so
# and stops if the attach is refused.
#
# PASS MARK. Zero memory calls in the window, or a stated handful with each one
# named. The table names every call.

set -uo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
WORK="${MEMCALLS_WORK:-/tmp/m6-memcalls-$(id -un)}"
STRACE="${MEMCALLS_STRACE:-strace}"
SUDO="${MEMCALLS_SUDO:-}"
CALLS="brk,mmap,munmap,madvise,mremap,mprotect"
PORT="${MEMCALLS_PORT:-18443}"
REQUESTS=10000
WARMUP=2000
ARMS=()

usage() {
  sed -n '/^# USAGE/,/^# Run it/p' "$0" | sed 's/^# \{0,1\}//' >&2
  exit 2
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --requests) REQUESTS="$2"; shift 2 ;;
    --warmup) WARMUP="$2"; shift 2 ;;
    base=* | cand=*) ARMS+=("$1"); shift ;;
    *) usage ;;
  esac
done
((${#ARMS[@]} > 0)) || usage
for n in "$REQUESTS" "$WARMUP"; do
  [[ "$n" =~ ^[0-9]+$ ]] && ((n > 0)) || { echo "memcalls: counts are positive integers" >&2; usage; }
done
for tool in openssl curl "$STRACE" cc head; do
  command -v "$tool" >/dev/null 2>&1 || { echo "memcalls: $tool not found" >&2; exit 2; }
done

# shellcheck source=tools/memcalls-site.sh
. "$HERE/memcalls-site.sh"
trap stop_all EXIT INT TERM

# Every thread id of a process, as -p arguments for strace.
tid_args() {
  local t
  for t in /proc/"$1"/task/*; do printf -- '-p\n%s\n' "${t##*/}"; done
}

# Attach strace to the process, run the load command, detach, and print one
# result line per memory call plus the total. Arguments: label pid load...
measure() {
  local label="$1" pid="$2"; shift 2
  local out="$WORK/strace-$label.txt" faults_before faults_after sp tids=()
  mapfile -t tids < <(tid_args "$pid")
  : > "$out"
  $SUDO "$STRACE" -c -f -e trace="$CALLS" -o "$out" "${tids[@]}" &
  sp=$!
  sleep 1
  if ! kill -0 "$sp" 2>/dev/null; then
    echo "memcalls: strace could not attach to $pid; run as root (MEMCALLS_SUDO=sudo) or set kernel.yama.ptrace_scope=0" >&2
    return 1
  fi
  faults_before="$(minflt "$pid")"
  "$@" > "$WORK/load-$label.log" 2>&1 || echo "memcalls: the load for $label reported a failure, see $WORK/load-$label.log" >&2
  faults_after="$(minflt "$pid")"
  $SUDO kill -TERM "$sp" 2>/dev/null
  wait "$sp" 2>/dev/null
  local total
  total="$(awk 'NF >= 5 && $NF ~ /^[a-z_0-9]+$/ && $4 ~ /^[0-9]+$/ && $NF != "total" { s += $4 } END { print s + 0 }' "$out")"
  echo "---- $label: memory calls in the window ($REQUESTS requests after $WARMUP warm-up)"
  awk 'NF >= 5 && $NF ~ /^[a-z_0-9]+$/ && $4 ~ /^[0-9]+$/ && $NF != "total" { printf "  %-10s %s\n", $NF, $4 }' "$out"
  printf '  %-10s %s\n' "TOTAL" "$total"
  printf '  %-10s %s\n' "minor-flt" "$((faults_after - faults_before))"
  printf '  %-10s %s KiB\n' "rss" "$(rss_kb "$pid")"
  printf '  %-10s %s KiB\n' "mapped" "$(mapped_kb "$pid")"
  printf '  %-10s %s KiB\n' "locked" "$(locked_kb "$pid")"
  printf '  %-10s %s KiB\n' "anon-huge" "$(huge_kb "$pid")"
}

run_arm() {
  local label="${1%%=*}" dir="${1#*=}" file
  for file in m6-http m6-html m6-file; do
    [[ -x "$dir/$file" ]] || { echo "memcalls: $dir/$file is not executable" >&2; return 1; }
  done
  echo "==== $label ($dir)"

  # m6-http cache hit.
  start_stack "$dir" "$label" || return 1
  local url="https://127.0.0.1:$PORT/"
  url_file "$WARMUP" "$url" "$WORK/warm.curl"
  url_file "$REQUESTS" "$url" "$WORK/load.curl"
  curl -sk --http1.1 --fail -K "$WORK/warm.curl" || echo "memcalls: warm-up failed" >&2
  measure "$label-http-cache-hit" "$HTTP_PID" curl -sk --http1.1 --fail -K "$WORK/load.curl"
  stop_all

  # m6-html render.
  start_stack "$dir" "$label" || return 1
  "$CLIENT" "$HTML_SOCK" /render "$WARMUP" > /dev/null || echo "memcalls: warm-up failed" >&2
  measure "$label-html-render" "$HTML_PID" "$CLIENT" "$HTML_SOCK" /render "$REQUESTS"
  stop_all
}

make_site 0 || exit 2
for arm in "${ARMS[@]}"; do run_arm "$arm" || exit 2; done
