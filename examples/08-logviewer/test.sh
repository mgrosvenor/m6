#!/bin/bash
# test.sh — build, set up, run, and smoke-test example 08 (log viewer)
#
# Suite 0: unit tests
# Suite A: explicit HTTP/1.1 (curl --http1.1)
# Suite B: explicit HTTP/3  (curl --http3-only)
#
# This file starts and stops its own stack; the driver must not start dev.sh
# for it first.
# M6_SELF_STARTING=1
#
# Flags:
#   --no-suite-b   skip HTTP/3 suite (useful when HTTP/3 not available)
#   --keep         leave the stack running after the results (Ctrl-C stops it)
set -e

SITE="$(cd "$(dirname "$0")" && pwd)"
M6="${M6:-$(cd "$SITE/../.." && pwd)}"  # this repository; override with M6=/path
. "$SITE/../own.sh"

# ── Flags ──────────────────────────────────────────────────────────────────────
RUN_SUITE_B=true
KEEP=false
for arg in "$@"; do
  case "$arg" in
    --no-suite-b) RUN_SUITE_B=false ;;
    --keep)       KEEP=true ;;
    *) echo "Unknown arg: $arg"; exit 1 ;;
  esac
done

# Use brew curl if available (has HTTP/3 support); fall back to system curl.
CURL="$(command -v /opt/homebrew/opt/curl/bin/curl 2>/dev/null || echo curl)"
BASE_OPTS="-sk"

# ── Colours ────────────────────────────────────────────────────────────────────
GREEN='\033[0;32m'; RED='\033[0;31m'; RESET='\033[0m'

# ── 1. Unit tests ─────────────────────────────────────────────────────────────
echo "=== Running m6-file unit tests ==="
(cd "$M6" && cargo test -p m6-file handler::tests --quiet)

# ── 2. Build m6 binaries ──────────────────────────────────────────────────────
echo "=== Building m6 binaries ==="
(cd "$M6" && cargo build --release --quiet)
export PATH="$M6/target/release:$PATH"

# Development keys and state: a private per-user directory, see dev-home.sh.
. "$SITE/../dev-home.sh"
"$SITE/../dev-keys.sh"

# ── 4. Start stack ────────────────────────────────────────────────────────────
mkdir -p "$SITE/logs"
mkdir -p /tmp/m6

# Stop only what a previous run of THIS file recorded, by pid, in
# logs/.own-test.pids. A holder of port 8443 that this file did not start is
# reported, with its pid, and left alone.
own_init "$SITE" test
own_reap
own_refuse_busy 8443 "this test" || exit 1

rm -f /tmp/m6/m6-html.sock /tmp/m6/m6-file.sock
echo "=== Starting m6 stack ==="
# `wait` with no arguments waits for every child, and the three servers are
# children, so a bare `wait` at the end would block after the results. Stop the
# recorded pids and then wait, which reaps them.
stop_stack() {
    own_stop_records
    own_forget
    wait 2>/dev/null || true
}
trap 'echo "Stopping..."; stop_stack' EXIT

M6_SOCKET_OVERRIDE=/tmp/m6/m6-html.sock \
    m6-html "$SITE" "$SITE/configs/m6-html.conf" >> "$SITE/logs/m6-html.log" 2>&1 &
own_track "$!"
M6_SOCKET_OVERRIDE=/tmp/m6/m6-file.sock \
    m6-file "$SITE" "$SITE/configs/m6-file.conf" >> "$SITE/logs/m6-file.log" 2>&1 &
own_track "$!"

echo -n "Waiting for backend sockets"
for _ in $(seq 1 20); do
    if [[ -S /tmp/m6/m6-html.sock && -S /tmp/m6/m6-file.sock ]]; then
        echo " ready."
        break
    fi
    echo -n "."
    sleep 0.25
done

m6-http "$SITE" "$(m6_conf "$SITE/configs/system-dev.toml")" >> "$SITE/logs/m6-http.log" 2>&1 &
own_track "$!"

# Wait for server readiness via HTTP/1.1 (always available)
echo -n "Waiting for server"
READY=false
for _ in $(seq 1 40); do
    code=$($CURL $BASE_OPTS --http1.1 -o /dev/null -w "%{http_code}" https://localhost:8443/ 2>/dev/null || true)
    if [[ "$code" == "200" ]]; then
        echo " ready."
        READY=true
        break
    fi
    echo -n "."
    sleep 0.5
done

if [[ "$READY" != "true" ]]; then
    echo ""
    own_refuse_busy 8443 "this test" || exit 1
    echo "ERROR: server did not become ready. Check logs in $SITE/logs/" >&2
    exit 1
fi

# ── Suite A: HTTP/1.1 ─────────────────────────────────────────────────────────
echo ""
echo "=== Suite A: HTTP/1.1 ==="
PASS_A=0
FAIL_A=0

check_a() {
    local desc="$1" expected="$2"
    shift 2
    local actual
    actual=$("$@" 2>/dev/null || true)
    if echo "$actual" | grep -qi "$expected"; then
        echo -e "  ${GREEN}PASS${RESET} [HTTP/1.1] $desc"
        PASS_A=$((PASS_A + 1))
    else
        echo -e "  ${RED}FAIL${RESET} [HTTP/1.1] $desc"
        echo "        expected: $expected"
        echo "        got:      $actual" | head -5
        FAIL_A=$((FAIL_A + 1))
    fi
}

OPTS11="$BASE_OPTS --http1.1"

check_a "GET / returns 200" "200" \
    $CURL $OPTS11 -o /dev/null -w "%{http_code}" https://localhost:8443/

check_a "GET /logs contains lv-table" "lv-table" \
    $CURL $OPTS11 https://localhost:8443/logs

check_a "tail returns X-Log-End header" "x-log-end:" \
    $CURL $OPTS11 -D - "https://localhost:8443/logs/tail/m6-http.log?offset=0" -o /dev/null

check_a "tail returns Cache-Control: no-store" "no-store" \
    $CURL $OPTS11 -D - "https://localhost:8443/logs/tail/m6-http.log?offset=0" -o /dev/null

# tail-n mode: offset=0&n=200 is the exact URL the browser uses on first load
check_a "tail-n mode returns X-Log-End header" "x-log-end:" \
    $CURL $OPTS11 -D - "https://localhost:8443/logs/tail/m6-http.log?offset=0&n=200" -o /dev/null

check_a "tail-n mode returns 200" "200" \
    $CURL $OPTS11 -o /dev/null -w "%{http_code}" \
        "https://localhost:8443/logs/tail/m6-http.log?offset=0&n=200"

END11=$($CURL $OPTS11 -D - "https://localhost:8443/logs/tail/m6-http.log?offset=0" -o /dev/null \
      2>/dev/null | grep -i x-log-end | tr -d '[:space:]' | cut -d: -f2 || true)
check_a "tail from end offset returns empty body" "" \
    $CURL $OPTS11 "https://localhost:8443/logs/tail/m6-http.log?offset=$END11"

check_a "GET /assets/logs.js returns 200" "200" \
    $CURL $OPTS11 -o /dev/null -w "%{http_code}" https://localhost:8443/assets/logs.js

check_a "GET /assets/logs.css returns 200" "200" \
    $CURL $OPTS11 -o /dev/null -w "%{http_code}" https://localhost:8443/assets/logs.css

check_a "tail of missing file returns 404" "404" \
    $CURL $OPTS11 -o /dev/null -w "%{http_code}" \
        "https://localhost:8443/logs/tail/doesnotexist.log?offset=0"

echo "  Suite A: $PASS_A passed, $FAIL_A failed"

# ── Suite B: HTTP/3 ───────────────────────────────────────────────────────────
PASS_B=0
FAIL_B=0
SKIP_B=false

if [[ "$RUN_SUITE_B" == "false" ]]; then
    echo ""
    echo "=== Suite B: HTTP/3 (skipped via --no-suite-b) ==="
    SKIP_B=true
elif ! $CURL --version 2>/dev/null | grep -q "HTTP3"; then
    echo ""
    echo "=== Suite B: HTTP/3 (skipped — curl lacks HTTP/3 support) ==="
    SKIP_B=true
else
    echo ""
    echo "=== Suite B: HTTP/3 ==="

    check_b() {
        local desc="$1" expected="$2"
        shift 2
        local actual
        actual=$("$@" 2>/dev/null || true)
        if echo "$actual" | grep -qi "$expected"; then
            echo -e "  ${GREEN}PASS${RESET} [HTTP/3] $desc"
            PASS_B=$((PASS_B + 1))
        else
            echo -e "  ${RED}FAIL${RESET} [HTTP/3] $desc"
            echo "        expected: $expected"
            echo "        got:      $actual" | head -5
            FAIL_B=$((FAIL_B + 1))
        fi
    }

    OPTS3="$BASE_OPTS --http3-only -4"

    check_b "GET / returns 200" "200" \
        $CURL $OPTS3 -o /dev/null -w "%{http_code}" https://localhost:8443/

    check_b "GET /logs contains lv-table" "lv-table" \
        $CURL $OPTS3 https://localhost:8443/logs

    check_b "tail returns X-Log-End header" "x-log-end:" \
        $CURL $OPTS3 -D - "https://localhost:8443/logs/tail/m6-http.log?offset=0" -o /dev/null

    check_b "tail returns Cache-Control: no-store" "no-store" \
        $CURL $OPTS3 -D - "https://localhost:8443/logs/tail/m6-http.log?offset=0" -o /dev/null

    # tail-n mode: offset=0&n=200 is the exact URL the browser uses on first load
    check_b "tail-n mode returns X-Log-End header" "x-log-end:" \
        $CURL $OPTS3 -D - "https://localhost:8443/logs/tail/m6-http.log?offset=0&n=200" -o /dev/null

    check_b "tail-n mode returns 200" "200" \
        $CURL $OPTS3 -o /dev/null -w "%{http_code}" \
            "https://localhost:8443/logs/tail/m6-http.log?offset=0&n=200"

    END3=$($CURL $OPTS3 -D - "https://localhost:8443/logs/tail/m6-http.log?offset=0" -o /dev/null \
          2>/dev/null | grep -i x-log-end | tr -d '[:space:]' | cut -d: -f2 || true)
    check_b "tail from end offset returns empty body" "" \
        $CURL $OPTS3 "https://localhost:8443/logs/tail/m6-http.log?offset=$END3"

    check_b "GET /assets/logs.js returns 200" "200" \
        $CURL $OPTS3 -o /dev/null -w "%{http_code}" https://localhost:8443/assets/logs.js

    check_b "GET /assets/logs.css returns 200" "200" \
        $CURL $OPTS3 -o /dev/null -w "%{http_code}" https://localhost:8443/assets/logs.css

    check_b "tail of missing file returns 404" "404" \
        $CURL $OPTS3 -o /dev/null -w "%{http_code}" \
            "https://localhost:8443/logs/tail/doesnotexist.log?offset=0"

    echo "  Suite B: $PASS_B passed, $FAIL_B failed"
fi

# ── Summary ────────────────────────────────────────────────────────────────────
echo ""
TOTAL_PASS=$((PASS_A + PASS_B))
TOTAL_FAIL=$((FAIL_A + FAIL_B))
echo "=== Results: $TOTAL_PASS passed, $TOTAL_FAIL failed ==="
if [[ "$SKIP_B" == "true" ]]; then
    echo "  (Suite B skipped)"
fi
echo ""
echo "Logs dir:    $SITE/logs/"
echo ""

if [[ "$TOTAL_FAIL" -gt 0 ]]; then
    exit 1
fi

if [[ "$KEEP" == "true" ]]; then
    echo "Log viewer:  https://localhost:8443/logs"
    echo ""
    echo "Press Ctrl-C to stop the server."
    # Opt-in: without --keep the file returns after its results and the EXIT
    # trap stops the stack it started.
    wait
fi
