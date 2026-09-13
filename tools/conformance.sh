#!/usr/bin/env bash
# conformance.sh — HTTP/1.1, HTTP/2 and HTTP/3 conformance, as a minimum-score check.
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
# ── The minimum-score check ───────────────────────────────────────────────────────────────
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
#   tools/conformance.sh --allow-missing-tools
#                                     # let an absent h2spec/h3spec SKIP rather
#                                     # than fail. For a developer laptop only.
#                                     # The pre-prod gate must never pass it.
#
# A GATE THAT CANNOT MEASURE MUST FAIL, NOT PASS.
#
# This script used to break that rule three ways, and reported PASS through all
# of them:
#
#   - `run_h3` never started the server it tested, so h3spec talked to a port
#     with nothing on it.
#   - Both h2 and h3 recorded a score only if the parse produced one, so a run
#     that measured nothing skipped the floor check entirely and fell through
#     to "nothing went backwards".
#   - An absent h2spec or h3spec printed "skipped" and returned success, so the
#     laptop's missing tools looked exactly like a pass.
#
# Every one of those is now a failure. A target listed in the scores file and
# not measured in a full run fails the run.
set -uo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
SCORES="$HERE/conformance-scores.txt"
WORK="${CONFORMANCE_WORK:-/tmp/m6-conformance}"
TLS_PORT=10443
H2C_PORT=18080
BRIDGE_BASE=18090
AUTH_BRIDGE_PORT=18096
REDIRECT_PORT=18081
EDGE_BRIDGE_PORT=18095

UPDATE=false
ALLOW_MISSING=false
ONLY=""
for arg in "$@"; do
  case "$arg" in
    --update) UPDATE=true ;;
    --allow-missing-tools) ALLOW_MISSING=true ;;
    h1|h2|h3) ONLY="$arg" ;;
    *) echo "usage: $0 [h1|h2|h3] [--update] [--allow-missing-tools]"; exit 2 ;;
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
# `mkdir` first. This truncation used to run ~330 lines before `mkdir -p
# "$WORK"`, so on a fresh box it failed and every later append went nowhere:
# the score file's own bookkeeping was silently not written.
mkdir -p "$WORK"
MEASURED="$WORK/measured.txt"
: > "$MEASURED"
RESULT=0
SKIPPED=""


# setsid keeps a child alive past this script on Linux and does not exist on
# macOS. Neither matters here -- the trap kills everything on exit -- so use it
# when present and fall through when not.
SETSID=""
have setsid && SETSID="setsid"


# Name the tests that failed, from whichever tester's output this is.
#
# WITHOUT THIS, A REGRESSION IS UNDIAGNOSABLE FROM CI. On 2026-09-13 the h2
# target came back 145/146 on a GitHub runner while the build host said 146/146,
# and the only thing in the CI log was "145/146 — floor is 146. Conformance went
# BACKWARDS." The log file naming the test lives in $WORK on a runner that is
# destroyed when the job ends, so there was nothing left to look at and the
# temptation was to call it a flake and move on. A count is not a diagnosis.
#
# Each tester marks failures differently: h2spec uses ×, h3spec [✘], h1spec its
# own text. All three are matched rather than guessing which produced this log,
# and -a because h2spec's output carries control bytes that make GNU grep treat
# the file as binary and print nothing useful.
report_failures() {  # report_failures <log-path>
  local log="$1"
  [[ -f "$log" ]] || { info "  no output file at $log"; return; }
  local found
  found="$(grep -a -nE '✘|✖|×|✕|^\s*Error|FAILED' "$log" | head -40 || true)"
  if [[ -n "$found" ]]; then
    info "  the failing tests, from $log:"
    printf '%s\n' "$found" | sed 's/^/      /' >&2
  else
    info "  no failure markers matched; the tail of $log:"
    tail -30 "$log" | sed 's/^/      /' >&2
  fi
}

check() {  # check <key> <passed> <total> [log-path]
  local key="$1" got="$2" tot="$3" log="${4:-}"
  local floor; floor="$(floor_for "$key")"
  printf '%s %s %s\n' "$key" "$got" "$tot" >> "$MEASURED"
  if [[ -z "$floor" ]]; then
    info "$key: $got/$tot (no floor recorded yet)"
    return
  fi
  if (( got < floor )); then
    fail "$key: $got/$tot — floor is $floor. Conformance went BACKWARDS."
    # Print the names, not just the count, so a CI failure can be read after
    # the runner is gone.
    [[ -n "$log" ]] && report_failures "$log"
    RESULT=1
  elif (( got > floor )); then
    pass "$key: $got/$tot — above the floor of $floor. Raise it with --update, in the commit that earned it."
  else
    pass "$key: $got/$tot"
  fi
}

# Record a score, or fail because there was nothing to record.
#
# The difference between this and calling `check` directly is the whole point
# of the file: `check` compares a number against a floor, and says nothing at
# all when handed no number.
measured_or_fail() {  # measured_or_fail <key> <got> <tot> <log-path>
  local key="$1" got="$2" tot="$3" log="$4"
  if [[ -z "$got" || -z "$tot" || "$tot" -eq 0 ]]; then
    fail "$key: measured NOTHING — no score could be parsed from the output."
    info "  a gate that cannot measure must fail, not pass. Output: $log"
    RESULT=1
    return 1
  fi
  check "$key" "$got" "$tot" "$log"
}

# A tool that is not installed is a run that did not test anything.
require_tool() {  # require_tool <binary> <label>
  if have "$1"; then return 0; fi
  if [[ "$ALLOW_MISSING" == "true" ]]; then
    # Explicitly permitted, so it does not fail the run -- but it is reported
    # as a skip in red and the summary refuses to say "pass". The property
    # that matters is that the path to a deploy cannot take this branch: the
    # Linux gate runs without the flag, on a box where both testers exist.
    fail "$2: SKIPPED — $1 is not installed. THIS RUN DID NOT TEST $2."
    SKIPPED="$SKIPPED $2"
    return 1
  fi
  fail "$2: $1 is not installed. A gate that cannot measure must fail, not pass."
  info "  install it, or pass --allow-missing-tools on a laptop (never before a deploy)."
  RESULT=1
  return 1
}

# ── Loopback instance ─────────────────────────────────────────────────────────
# A loopback instance, not the staging service: staging was once found running
# a stale binary, and measuring it reported pre-fix numbers as current.

EDGE_UP=false
start_edge() {
  # Idempotent. h2 and h3 both need the edge and both call this; the second
  # call used to collide with the first on port 10443 and be reported as "port
  # already in use", which read as an environment problem and was really this
  # function being called twice.
  if [[ "$EDGE_UP" == "true" ]]; then
    return 0
  fi
  # Self-contained: builds its own site, cert and backend. The alternative is
  # the manual one-time setup in the site HANDOVER, which is fine for a person
  # and useless in CI, and which was once measured while pointing at a stale
  # staging binary.
  local site="$WORK/edge-site"
  mkdir -p "$site/public" "$site/configs"

  if [[ ! -x "$ROOT/target/release/m6-http" || ! -x "$ROOT/target/release/m6-file" \
        || ! -x "$ROOT/target/release/m6-html" ]]; then
    fail "no release build; run: cargo build --workspace --release"
    RESULT=1
    return 1
  fi

  # rustls rejects an X.509 v1 certificate (UnsupportedCertVersion). `-addext`
  # is what forces v3, and openssl gives no warning if you leave it out.
  if [[ ! -f "$WORK/cert.pem" ]]; then
    openssl req -x509 -newkey rsa:2048 -keyout "$WORK/key.pem" -out "$WORK/cert.pem" \
      -days 2 -nodes -subj "/CN=localhost" \
      -addext "subjectAltName=DNS:localhost,IP:127.0.0.1" 2>/dev/null
  fi

  printf 'PUBLIC CONTENT\n' > "$site/public/open.txt"
  # m6-file is an `App` service: the handler is named in config, and the
  # wildcard is explicit. The old spelling now fails at startup, which is the
  # point of that check -- but it failed HERE first, unnoticed, because
  # `start_edge` only ran under h2 and h2spec is absent on the laptop.
  printf '[[route]]\npath = "/public/{*relpath}"\nhandler = "files"\nroot = "public/"\n' \
    > "$site/configs/m6-file.conf"
  cat > "$site/site.toml" <<TOML
[site]
name   = "conformance"
domain = "localhost"

[log]
level  = "warn"
format = "text"

[analytics]
enabled = false

# The rate limit must be off, or the limiter is what gets measured rather than
# the protocol.
[rate_limit]
enabled = false

[[backend]]
name    = "m6-file"
sockets = "$WORK/edge-m6-file-*.sock"

[[backend]]
name    = "m6-html"
sockets = "$WORK/edge-m6-html-*.sock"

[[route]]
path    = "/public/{relpath}"
backend = "m6-file"

[[route]]
path    = "/"
backend = "m6-html"
TOML
  cat > "$WORK/conf.toml" <<TOML
[server]
bind     = "127.0.0.1:$TLS_PORT"
tls_cert = "$WORK/cert.pem"
tls_key  = "$WORK/key.pem"

[node]
name = "conformance"
TOML

  # Backend first: m6-http discovers sockets by rescan, so a backend that
  # appears late means an empty pool and 502 on every request.
  local sock="$WORK/edge-m6-file-1.sock"
  rm -f "$sock"
  M6_SOCKET_OVERRIDE="$sock" $SETSID nohup "$ROOT/target/release/m6-file" \
    "$site" "$site/configs/m6-file.conf" > "$WORK/edge-file.log" 2>&1 &
  local fpid=$!
  PIDS+=($fpid)
  for _ in $(seq 1 300); do [[ -S "$sock" ]] && break; sleep 0.02; done
  [[ -S "$sock" ]] || { fail "edge backend never created $sock"; RESULT=1; return 1; }

  # A second backend, so the edge under test routes the way production does:
  # m6-http in front of a file service AND a renderer, not in front of one
  # thing. h2spec and h3spec exercise the edge's own framing either way, but a
  # single-backend edge is not the service that runs, and the point of running
  # these against a live stack is that it is the live stack.
  mkdir -p "$site/templates"
  printf '<!doctype html><html><body><h1>conformance</h1></body></html>\n' \
    > "$site/templates/index.html"
  printf '[[route]]\npath = "/"\ntemplate = "templates/index.html"\n' \
    > "$site/configs/m6-html.conf"
  local hsock="$WORK/edge-m6-html-1.sock"
  rm -f "$hsock"
  M6_SOCKET_OVERRIDE="$hsock" $SETSID nohup "$ROOT/target/release/m6-html" \
    "$site" "$site/configs/m6-html.conf" > "$WORK/edge-html.log" 2>&1 &
  local hpid=$!
  PIDS+=($hpid)
  for _ in $(seq 1 300); do [[ -S "$hsock" ]] && break; sleep 0.02; done
  [[ -S "$hsock" ]] || { fail "edge renderer never created $hsock"; RESULT=1; return 1; }

  require_free_port "$TLS_PORT" "the loopback edge" || return 1
  $SETSID nohup "$ROOT/target/release/m6-http" "$site" "$WORK/conf.toml" \
    > "$WORK/edge.log" 2>&1 &
  local pid=$!
  PIDS+=($pid)
  wait_port_owned_by "$pid" "$TLS_PORT" "the loopback edge" || return 1
  # The backend pool is filled by a periodic rescan, so listening is not the
  # same as being able to serve.
  sleep 2.5
  EDGE_UP=true
  return 0
}

start_auth() {  # start_auth <bridge-port>
  local port="$1" site="$WORK/auth-site"
  mkdir -p "$site/data"
  if [[ ! -f "$site/auth.pem" ]]; then
    openssl genrsa -out "$site/auth.pem" 2048 2>/dev/null
    openssl rsa -in "$site/auth.pem" -pubout -out "$site/auth.pub" 2>/dev/null
  fi
  cat > "$site/m6-auth.conf" <<TOML
[storage]
path = "data/auth.db"

[tokens]
access_ttl  = 900
refresh_ttl = 2592000
issuer      = "conformance"

[keys]
private_key = "$site/auth.pem"
public_key  = "$site/auth.pub"
TOML
  local sock="$WORK/auth.sock"
  rm -f "$sock"
  M6_SOCKET_OVERRIDE="$sock" $SETSID nohup "$ROOT/target/release/m6-auth-server" \
    "$site" "$site/m6-auth.conf" > "$WORK/auth.log" 2>&1 &
  local pid=$!
  PIDS+=($pid)
  for _ in $(seq 1 300); do [[ -S "$sock" ]] && break; sleep 0.02; done
  [[ -S "$sock" ]] || { fail "m6-auth-server never created $sock"; RESULT=1; return 1; }
  require_free_port "$port" "the m6-auth-server bridge" || return 1
  $SETSID nohup python3 "$HERE/unix_bridge.py" "$port" "$sock" > "$WORK/auth-bridge.log" 2>&1 &
  local bpid=$!
  PIDS+=($bpid)
  wait_port_owned_by "$bpid" "$port" "the m6-auth-server bridge"
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
    require_tool uvx "h1 (all targets)" || true
    info "h1: h1spec is delivered through uvx (https://docs.astral.sh/uv/)"
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

  # m6-auth-server, the other consumer of the one parser in m6_core::h1.
  if start_auth "$AUTH_BRIDGE_PORT"; then
    h1_against "h1:m6-auth-server" "$AUTH_BRIDGE_PORT"
  fi

  # The plaintext :80 redirect listener, which is public-facing in production.
  if start_redirect; then
    h1_against "h1:m6-http-redirect" "$REDIRECT_PORT"
  fi

  # NOT MEASURED: the :443 engine behind a TLS bridge.
  #
  # h1spec speaks cleartext, so reaching :443 needs a TLS-terminating proxy in
  # front, and the score through a Python threading proxy came back 30, 27, 27,
  # 24, 24 on five consecutive runs of identical code. A gate built on a number
  # that moves is a false-failure generator.
  #
  # It is also unnecessary. Since the redirect listener was rewritten onto
  # Http11Listener, :80 and :443 run the SAME HTTP/1.1 engine -- the only
  # difference is H1Io::Plain versus H1Io::Tls. The redirect target above is
  # plain TCP, needs no bridge, and is stable run to run, so it measures the
  # engine directly. The TLS layer itself is covered by h2spec and h3spec.
  #
  # tools/tls_bridge.py is kept for ad-hoc investigation; it is not a gate.
}

# Prove the bridge carries a known-good request UNDER THE TESTER'S OWN
# CONDITIONS before believing any score through it.
#
# Three conformance scores in this project were harness artefacts, not
# measurements: a unix bridge that closed both directions on EOF (11/32), a TLS
# bridge that broke the session on half-close (6/32), and a run against a
# leftover process still holding the port (5/32, then 8/32). Each looked exactly
# like a catastrophic server. h1spec half-closes its write side after sending,
# so that is the condition the sanity check has to use.
bridge_sanity() {
  local port="$1"
  python3 - "$port" <<'PYEOF'
import socket, sys
port = int(sys.argv[1])
def probe(half_close):
    s = socket.create_connection(("127.0.0.1", port), timeout=8)
    s.sendall(b"GET /public/open.txt HTTP/1.1\r\nHost: localhost\r\n\r\n")
    if half_close:
        s.shutdown(socket.SHUT_WR)
    out = b""
    try:
        while True:
            b = s.recv(4096)
            if not b:
                break
            out += b
    except OSError:
        pass
    s.close()
    return out
open_ok = probe(False).startswith(b"HTTP/")
half_ok = probe(True).startswith(b"HTTP/")
if open_ok and half_ok:
    sys.exit(0)
print(f"  bridge sanity FAILED: write-open={open_ok} half-closed={half_ok}")
sys.exit(1)
PYEOF
  if [[ $? -ne 0 ]]; then
    fail "the bridge on $port does not carry a known-good request; any score through it is meaningless"
    RESULT=1
    return 1
  fi
  return 0
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
  require_tool h2spec "h2:m6-http" || return 1
  info "HTTP/2 — h2spec (RFC 9113)"
  start_edge || { fail "h2:m6-http: the edge never came up, so nothing was measured"; RESULT=1; return 1; }
  local log="$WORK/h2spec.out"
  h2spec -h 127.0.0.1 -p $TLS_PORT -t -k --timeout 5 > "$log" 2>&1
  local got failed tot
  # -a, because h2spec's output carries control bytes and GNU grep then treats
  # the file as binary: it prints "binary file matches" instead of the match,
  # the parse yields nothing, and the run is scored as having measured nothing.
  # That happened on the first CI run and the "measured NOTHING" check caught
  # it, which is the check doing its job.
  got="$(grep -a -oE '[0-9]+ passed' "$log" | tail -1 | grep -oE '[0-9]+')"
  failed="$(grep -a -oE '[0-9]+ failed' "$log" | tail -1 | grep -oE '[0-9]+')"
  tot=$(( ${got:-0} + ${failed:-0} ))
  measured_or_fail "h2:m6-http" "${got:-}" "$tot" "$log"
}

run_h3() {
  require_tool h3spec "h3:m6-http" || return 1
  info "HTTP/3 — h3spec (RFC 9114 + QUIC)"
  # THE EDGE HAS TO BE RUNNING. This line was missing, which is why h3 could
  # report a pass having talked to a closed UDP port. m6-http binds QUIC on the
  # same address as `server.bind`, so the same loopback instance serves it.
  start_edge || { fail "h3:m6-http: the edge never came up, so nothing was measured"; RESULT=1; return 1; }
  # -n or every test fails on certificate name mismatch and reports a false
  # disaster.
  local log="$WORK/h3spec.out"
  h3spec -n 127.0.0.1 $TLS_PORT > "$log" 2>&1
  # Parse h3spec's own summary line -- "49 examples, 12 failures" -- rather
  # than counting result markers.
  #
  # The previous parser counted lines starting with `+` or `-`, which this
  # tester has never emitted: it marks each result with a trailing [✔] or [✘].
  # So the count was always zero, and the zero was then used to SKIP the floor
  # check rather than to fail. Two bugs compounding: a parser that matched
  # nothing, and a gate that treated "nothing" as "fine".
  local total failed got
  total="$(grep -a -oE '[0-9]+ examples' "$log" | tail -1 | grep -oE '[0-9]+')"
  failed="$(grep -a -oE '[0-9]+ failures?' "$log" | tail -1 | grep -oE '[0-9]+')"
  if [[ -n "$total" && -n "$failed" ]]; then
    got=$(( total - failed ))
  else
    got=""
  fi
  measured_or_fail "h3:m6-http" "$got" "${total:-0}" "$log"
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
  # MERGE, never rewrite. `--update h1` measures only the h1 targets, and a
  # wholesale rewrite would silently delete the h2 and h3 floors -- turning the
  # one file that says a protocol may not go backwards into the thing that lets
  # it. The header comment explains the whole mechanism and is not regenerable,
  # so it is preserved too.
  cp "$SCORES" "$SCORES.new"
  while read -r k g t; do
    [[ -z "$k" ]] && continue
    if grep -qE "^${k}[[:space:]]" "$SCORES.new"; then
      # Replace this target's line in place, leaving every other line alone.
      awk -v key="$k" -v floor="$g" -v total="$t" \
        '$1 == key { printf "%-22s %-6s %s\n", key, floor, total; next } { print }' \
        "$SCORES.new" > "$SCORES.tmp" && mv "$SCORES.tmp" "$SCORES.new"
    else
      printf '%-22s %-6s %s\n' "$k" "$g" "$t" >> "$SCORES.new"
    fi
  done < "$MEASURED"
  mv "$SCORES.new" "$SCORES"
  info "floors updated in $SCORES — commit this alongside the change that earned it"
  exit 0
fi

# ── Did this run actually measure what it claims? ────────────────────────────
#
# The per-target checks above catch a target that measured nothing. This
# catches a target that was never attempted at all, which is the failure the
# old script had no way to see: `run_h3` returning early left no trace, and the
# summary line said "nothing went backwards" because, strictly, nothing had.
#
# Every target with a floor must appear in this run's measurements. Only a
# full run can assert that, since `conformance.sh h1` is deliberately partial.
if [[ -z "$ONLY" && "$UPDATE" != "true" ]]; then
  echo
  missing=""
  while read -r key _floor _total; do
    [[ -z "$key" || "$key" == \#* ]] && continue
    case "$SKIPPED" in *" $key"*) continue ;; esac
    grep -qE "^${key} " "$MEASURED" || missing="$missing $key"
  done < "$SCORES"
  if [[ -n "$missing" ]]; then
    fail "these targets have a floor and were NOT MEASURED:$missing"
    info "  a target that is not measured is not gated. That is the whole point."
    RESULT=1
  fi
fi

echo
if [[ -n "$SKIPPED" ]]; then
  fail "conformance INCOMPLETE — NOT TESTED:$SKIPPED"
  info "  this run proves nothing about those protocols. Do not read it as a pass."
  info "  the Linux gate runs without --allow-missing-tools and will test them."
  exit $RESULT
fi
if (( RESULT == 0 )); then
  pass "conformance: every target measured, nothing went backwards"
else
  fail "conformance regressed or could not measure — see above"
fi
exit $RESULT
