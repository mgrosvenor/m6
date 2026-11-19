#!/bin/bash
# bench.sh — latency + throughput benchmarks for example 08 (log viewer)
#
# Starts the m6 stack if not running, runs m6-bench against it, then
# shuts down any stack it started.
#
# Usage:
#   ./bench.sh [--http11-only] [--http3-only] [--latency-n N]
#              [--throughput-n N] [--concurrency C]
#              [--p99-limit-us F] [--rps-min F]
#
# Pass-through flags are forwarded to m6-bench unchanged.
set -e

SITE="$(cd "$(dirname "$0")" && pwd)"
M6="${M6:-$(cd "$SITE/../../.." && pwd)/m6}"

GREEN='\033[0;32m'; RED='\033[0;31m'; YELLOW='\033[1;33m'; RESET='\033[0m'

info() { echo -e "${YELLOW}----${RESET} $1"; }

# ── Build ──────────────────────────────────────────────────────────────────────
info "Building m6 binaries..."
(cd "$M6" && cargo build --release --quiet)
export PATH="$M6/target/release:$PATH"

# Development keys and state: a private per-user directory, see dev-home.sh.
. "$SITE/../dev-home.sh"
"$SITE/../dev-keys.sh"

# ── Start stack (if not already listening) ────────────────────────────────────
STACK_STARTED=false
if ! nc -z localhost 8443 2>/dev/null; then
    info "Starting m6 stack..."
    mkdir -p "$SITE/logs"
    mkdir -p /tmp/m6
    rm -f /tmp/m6/m6-html.sock /tmp/m6/m6-file.sock

    trap 'kill $(jobs -p) 2>/dev/null; wait 2>/dev/null' EXIT
    STACK_STARTED=true

    M6_SOCKET_OVERRIDE=/tmp/m6/m6-html.sock \
        m6-html "$SITE" "$SITE/configs/m6-html.conf" >> "$SITE/logs/m6-html.log" 2>&1 &
    M6_SOCKET_OVERRIDE=/tmp/m6/m6-file.sock \
        m6-file "$SITE" "$SITE/configs/m6-file.conf" >> "$SITE/logs/m6-file.log" 2>&1 &

    echo -n "Waiting for backend sockets"
    for i in $(seq 1 20); do
        if [[ -S /tmp/m6/m6-html.sock && -S /tmp/m6/m6-file.sock ]]; then echo " ready."; break; fi
        echo -n "."; sleep 0.25
    done

    m6-http "$SITE" "$(m6_conf "$SITE/configs/system-dev.toml")" >> "$SITE/logs/m6-http.log" 2>&1 &

    # Wait for readiness using HTTP/1.1
    CURL="$(command -v /opt/homebrew/opt/curl/bin/curl 2>/dev/null || echo curl)"
    echo -n "Waiting for server"
    for i in $(seq 1 40); do
        code=$($CURL -sk --http1.1 -o /dev/null -w "%{http_code}" https://localhost:8443/ 2>/dev/null || true)
        if [[ "$code" == "200" ]]; then echo " ready."; break; fi
        echo -n "."; sleep 0.5
    done
else
    info "Stack already running on :8443, using it."
fi

# ── Run benchmarks ────────────────────────────────────────────────────────────
echo ""
info "Running m6-bench --skip-verify --addr 127.0.0.1:8443 $*"
echo ""
if m6-bench --skip-verify --addr 127.0.0.1:8443 "$@"; then
    echo ""
    echo -e "${GREEN}bench.sh: all benchmarks passed.${RESET}"
else
    echo ""
    echo -e "${RED}bench.sh: benchmark(s) FAILED.${RESET}"
    exit 1
fi
