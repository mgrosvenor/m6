#!/usr/bin/env bash
# conformance.sh — HTTP/1.1, HTTP/2 and HTTP/3 conformance, as a ratchet.
#
# HTTP/2 and HTTP/3 have had third-party conformance testing since 2026-09;
# HTTP/1.1 had none until 2026-09-11, and the divergence showed. Four HTTP/1.1
# parsers in this workspace, three conventions for header-name case, none of
# them checking Transfer-Encoding, and m6-file sending a body on a HEAD that
# 404s -- the same defect fixed in m6-http months earlier and never applied
# here, because nothing tested it.
#
# Every tester used here is an INDEPENDENT implementation. That is the whole
# point: a conformance test written by the same hand as the implementation
# encodes the same misreading of the RFC twice and passes.
#
#   h2spec   github.com/summerwind/h2spec        RFC 9113
#   h3spec   github.com/kazu-yamamoto/h3spec     RFC 9114 + QUIC
#   h1spec   github.com/dropseed/h1spec          RFC 9112 / 9110
#
# ── The ratchet ───────────────────────────────────────────────────────────────
#
# Each target has a floor in scores.txt. A run below its floor fails. A run
# above it prints the new number and tells you to raise the floor, which is a
# deliberate manual step: raising a floor is a claim that the improvement is
# real and permanent, and it belongs in a commit next to the change that earned
# it.
#
# Usage:
#   tools/conformance.sh              # everything
#   tools/conformance.sh h1           # one protocol
#   tools/conformance.sh --update     # rewrite floors to the measured scores
set -uo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
SCORES="$HERE/conformance-scores.txt"
WORK="${CONFORMANCE_WORK:-/tmp/m6-conformance}"
TLS_PORT=10443
H2C_PORT=18080
BRIDGE_BASE=18090

UPDATE=false
ONLY=""
for arg in "$@"; do
  case "$arg" in
    --update) UPDATE=true ;;
    h1|h2|h3) ONLY="$arg" ;;
    *) echo "usage: $0 [h1|h2|h3] [--update]"; exit 2 ;;
  esac
done

GREEN='\033[0;32m'; RED='\033[0;31m'; YELLOW='\033[1;33m'; RESET='\033[0m'
pass() { echo -e "${GREEN}PASS${RESET} $*"; }
fail() { echo -e "${RED}FAIL${RESET} $*"; }
info() { echo -e "${YELLOW}----${RESET} $*"; }

PIDS=()
cleanup() {
  # Kill by recorded PID. NEVER `pkill -f` with a pattern naming the port or
  # the config path: over ssh the command line contains that string too, so
  # pkill matches its own session and kills the connection. That has happened
  # three times in this project.
  for p in ${PIDS[@]:-}; do kill "$p" 2>/dev/null || true; done
  sleep 0.3
  for p in ${PIDS[@]:-}; do kill -9 "$p" 2>/dev/null || true; done
}
trap cleanup EXIT

have() { command -v "$1" >/dev/null 2>&1; }

floor_for() { awk -v k="$1" '$1==k {print $2}' "$SCORES" 2>/dev/null; }
total_for() { awk -v k="$1" '$1==k {print $3}' "$SCORES" 2>/dev/null; }

# Measured scores go in a file, not an associative array: macOS ships bash 3.2,
# which has neither `declare -A` nor `setsid`, and this has to run on a
# developer laptop as well as the Linux build box.
MEASURED="$WORK/measured.txt"
: > "$MEASURED"
RESULT=0

# setsid keeps a child alive past this script on Linux and does not exist on
# macOS. Neither matters here -- the trap kills everything on exit -- so use it
# when present and fall through when not.
SETSID=""
have setsid && SETSID="setsid"


check() {  # check <key> <passed> <total>
  local key="$1" got="$2" tot="$3"
  local floor; floor="$(floor_for "$key")"
  printf '%s %s %s\n' "$key" "$got" "$tot" >> "$MEASURED"
  if [[ -z "$floor" ]]; then
    info "$key: $got/$tot (no floor recorded yet)"
    return
  fi
  if (( got < floor )); then
    fail "$key: $got/$tot — floor is $floor. Conformance went BACKWARDS."
    RESULT=1
  elif (( got > floor )); then
    pass "$key: $got/$tot — above the floor of $floor. Raise it with --update, in the commit that earned it."
  else
    pass "$key: $got/$tot"
  fi
}

# ── Loopback instance ─────────────────────────────────────────────────────────
# A loopback instance, not the staging service: staging was once found running
# a stale binary, and measuring it reported pre-fix numbers as current.

start_edge() {
  local site="$WORK/site-origin"
  mkdir -p "$site"
  if [[ ! -f "$ROOT/target/release/m6-http" ]]; then
    fail "no release build; run: cargo build --workspace --release"
    exit 1
  fi
  if [[ ! -f "$WORK/conf.toml" ]]; then
    info "no $WORK/conf.toml — skipping the edge; see 'Third-party conformance' in the site HANDOVER for the one-time setup"
    return 1
  fi
  # The rate limit must be raised for the run or the limiter is what gets
  # measured rather than the protocol.
  if [[ -f "$site/site.toml" ]]; then
    sed -i.bak 's/^requests_per_min = .*/requests_per_min = 100000/' "$site/site.toml" 2>/dev/null || true
  fi
  $SETSID nohup "$ROOT/target/release/m6-http" "$site" "$WORK/conf.toml" \
    > "$WORK/edge.log" 2>&1 &
  PIDS+=($!)
  for _ in $(seq 1 100); do
    if (exec 3<>/dev/tcp/127.0.0.1/$TLS_PORT) 2>/dev/null; then exec 3>&- 3<&-; return 0; fi
    sleep 0.1
  done
  fail "loopback m6-http never listened on $TLS_PORT; see $WORK/edge.log"
  RESULT=1
  return 1
}

start_backend() {  # start_backend <name> <bridge-port> <site-dir> <config>
  local name="$1" port="$2" site="$3" conf="$4"
  local sock="$WORK/$name.sock"
  rm -f "$sock"
  M6_SOCKET_OVERRIDE="$sock" $SETSID nohup "$ROOT/target/release/$name" "$site" "$conf" \
    > "$WORK/$name.log" 2>&1 &
  PIDS+=($!)
  for _ in $(seq 1 200); do [[ -S "$sock" ]] && break; sleep 0.05; done
  [[ -S "$sock" ]] || { fail "$name never created $sock; see $WORK/$name.log"; RESULT=1; return 1; }
  $SETSID nohup python3 "$HERE/unix_bridge.py" "$port" "$sock" > "$WORK/$name-bridge.log" 2>&1 &
  PIDS+=($!)
  sleep 0.5
}

# ── h1 ────────────────────────────────────────────────────────────────────────

run_h1() {
  if ! have uvx; then
    info "h1: skipped, uvx not installed (https://docs.astral.sh/uv/)"
    return
  fi
  info "HTTP/1.1 — h1spec (RFC 9112/9110)"
  mkdir -p "$WORK"

  # The backends speak HTTP/1.1 over unix sockets, which is the wire contract
  # every m6 app implements, so they are the targets that matter most.
  start_backend m6-file "$BRIDGE_BASE" \
    "$ROOT/m6-file/tests/fixtures" "$ROOT/m6-file/tests/fixtures/m6-file-test.conf" || return 1
  local out
  out="$(uvx --from git+https://github.com/dropseed/h1spec h1spec 127.0.0.1:$BRIDGE_BASE 2>&1 \
        | sed 's/\x1b\[[0-9;]*m//g')"
  echo "$out" | grep -E '^\s+[✓✗]' || true
  local line got tot
  line="$(echo "$out" | grep -oE '[0-9]+/[0-9]+ passed' | tail -1)"
  got="${line%%/*}"; tot="${line#*/}"; tot="${tot%% *}"
  [[ -n "$got" ]] && check "h1:m6-file" "$got" "$tot"
}

# ── h2 and h3 ─────────────────────────────────────────────────────────────────

run_h2() {
  if ! have h2spec; then info "h2: skipped, h2spec not installed"; return; fi
  info "HTTP/2 — h2spec (RFC 9113)"
  start_edge || return
  local out; out="$(h2spec -h 127.0.0.1 -p $TLS_PORT -t -k --timeout 5 2>&1)"
  local got tot
  got="$(echo "$out" | grep -oE '[0-9]+ passed' | tail -1 | grep -oE '[0-9]+')"
  local failed; failed="$(echo "$out" | grep -oE '[0-9]+ failed' | tail -1 | grep -oE '[0-9]+')"
  tot=$(( ${got:-0} + ${failed:-0} ))
  [[ -n "$got" ]] && check "h2:m6-http" "$got" "$tot"
}

run_h3() {
  if ! have h3spec; then info "h3: skipped, h3spec not installed"; return; fi
  info "HTTP/3 — h3spec (RFC 9114 + QUIC)"
  # -n or every test fails on certificate name mismatch and reports a false
  # disaster.
  local out; out="$(h3spec -n 127.0.0.1 $TLS_PORT 2>&1)"
  local got failed
  got="$(echo "$out" | grep -coE '^\s*\+' || true)"
  failed="$(echo "$out" | grep -coE '^\s*-' || true)"
  [[ "${got:-0}" -gt 0 ]] && check "h3:m6-http" "$got" $(( got + failed ))
}

# ── Run ───────────────────────────────────────────────────────────────────────

mkdir -p "$WORK"
case "$ONLY" in
  h1) run_h1 ;;
  h2) run_h2 ;;
  h3) run_h3 ;;
  *)  run_h1; run_h2; run_h3 ;;
esac

if [[ "$UPDATE" == "true" ]]; then
  : > "$SCORES.new"
  echo "# target            floor  total   (raised by tools/conformance.sh --update)" >> "$SCORES.new"
  while read -r k g t; do
    [[ -z "$k" ]] && continue
    printf '%-20s %-6s %s\n' "$k" "$g" "$t" >> "$SCORES.new"
  done < "$MEASURED"
  sort -o "$SCORES.new" "$SCORES.new"
  mv "$SCORES.new" "$SCORES"
  info "floors updated in $SCORES — commit this alongside the change that earned it"
  exit 0
fi

echo
if (( RESULT == 0 )); then
  pass "conformance: nothing went backwards"
else
  fail "conformance regressed — see above"
fi
exit $RESULT
