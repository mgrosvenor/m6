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
REDIRECT_PORT=18081

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

# Wait for a port, but only accept it if OUR child is still the one alive.
#
# A bare connect check is not proof that the process just started is the one
# listening. A leftover instance from an earlier run held the port, the new
# child died with "Address already in use", the connect succeeded against the
# stale process, and h1spec scored an abandoned binary -- twice, at 5/32 and
# then 8/32, varying with whatever was there. That is the same failure as
# measuring a stale staging binary, and as benchmarking against an orphaned
# run: the harness reported a number for something other than what it built.
wait_port_owned_by() {  # wait_port_owned_by <pid> <port> <what>
  local pid="$1" port="$2" what="$3"
  for _ in $(seq 1 100); do
    if ! kill -0 "$pid" 2>/dev/null; then
      fail "$what exited during startup; see the log in $WORK"
      RESULT=1
      return 1
    fi
    if (exec 3<>/dev/tcp/127.0.0.1/"$port") 2>/dev/null; then exec 3>&- 3<&-; return 0; fi
    sleep 0.1
  done
  fail "$what never listened on $port"
  RESULT=1
  return 1
}

# Refuse to run at all if a target port is already taken: whatever is there is
# not ours, and measuring it is worse than not measuring.
require_free_port() {  # require_free_port <port> <what>
  if (exec 3<>/dev/tcp/127.0.0.1/"$1") 2>/dev/null; then
    exec 3>&- 3<&-
    fail "port $1 is already in use; $2 would measure whatever is there, not this build"
    RESULT=1
    return 1
  fi
  return 0
}

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
  require_free_port "$TLS_PORT" "the loopback edge" || return 1
  $SETSID nohup "$ROOT/target/release/m6-http" "$site" "$WORK/conf.toml" \
    > "$WORK/edge.log" 2>&1 &
  local pid=$!
  PIDS+=($pid)
  wait_port_owned_by "$pid" "$TLS_PORT" "the loopback edge"
}

start_redirect() {
  # m6-http in redirect mode is a plaintext HTTP/1.1 server on :80, a separate
  # process from the :443 instance, running on every production node. It is the
  # only m6 HTTP/1.1 implementation that is public-facing *and* unencrypted, so
  # a tester reaches it with no bridge and no TLS termination, and so does
  # anyone else.
  local site="$WORK/redir-site"
  mkdir -p "$site"
  cat > "$site/site.toml" <<TOML
[site]
name   = "conformance"
domain = "localhost"
TOML
  cat > "$WORK/redirect.toml" <<TOML
[server]
bind          = "127.0.0.1:1"
redirect_bind = "127.0.0.1:$REDIRECT_PORT"

[node]
name = "conformance"
TOML
  require_free_port "$REDIRECT_PORT" "the redirect listener" || return 1
  $SETSID nohup "$ROOT/target/release/m6-http" "$site" "$WORK/redirect.toml" \
    > "$WORK/redirect.log" 2>&1 &
  local pid=$!
  PIDS+=($pid)
  wait_port_owned_by "$pid" "$REDIRECT_PORT" "the redirect listener"
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
  require_free_port "$port" "the $name bridge" || return 1
  $SETSID nohup python3 "$HERE/unix_bridge.py" "$port" "$sock" > "$WORK/$name-bridge.log" 2>&1 &
  local bpid=$!
  PIDS+=($bpid)
  wait_port_owned_by "$bpid" "$port" "the $name bridge"
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
  h1_against "h1:m6-file" "$BRIDGE_BASE"

  # m6-html is a second backend with its own HTTP/1.1 parser.
  if start_backend m6-html $((BRIDGE_BASE + 1)) \
       "$ROOT/m6-html/tests/fixtures" "$ROOT/m6-html/tests/fixtures/configs/m6-html.conf"; then
    h1_against "h1:m6-html" $((BRIDGE_BASE + 1))
  fi

  # The plaintext :80 redirect listener, which is public-facing in production.
  if start_redirect; then
    h1_against "h1:m6-http-redirect" "$REDIRECT_PORT"
  fi
}

h1_against() {  # h1_against <score-key> <port>
  local key="$1" port="$2"
  local out
  out="$(uvx --from git+https://github.com/dropseed/h1spec h1spec 127.0.0.1:$port 2>&1 \
        | sed 's/\x1b\[[0-9;]*m//g')"
  echo "$out" | grep -E '^\s+✗' || true
  local line got tot
  line="$(echo "$out" | grep -oE '[0-9]+/[0-9]+ passed' | tail -1)"
  got="${line%%/*}"; tot="${line#*/}"; tot="${tot%% *}"
  if [[ -n "$got" ]]; then
    check "$key" "$got" "$tot"
  else
    fail "$key: h1spec produced no score"
    RESULT=1
  fi
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
