#!/usr/bin/env bash
# bench.sh — build, configure, and run the full m6 benchmark suite.
#
# Usage:
#   ./bench.sh [--skip-verify] [--latency-n N] [--duration S] [--concurrency C]
#              [--p99-limit-ms F] [--rps-min F] [--addr HOST:PORT]
#
# All nine suites (H1/H2/H3 × latency/path/throughput) are run in sequence.
# Servers are restarted between each suite to avoid stale-connection pollution.
set -euo pipefail

REPO="$(cd "$(dirname "$0")" && pwd)"
BENCH_PORT="${BENCH_PORT:-8443}"
BENCH_ADDR="${BENCH_ADDR:-127.0.0.1:${BENCH_PORT}}"
BENCH_DIR="/tmp/m6-bench"
SITE_DIR="$BENCH_DIR/site"
HTML_SOCK="$BENCH_DIR/m6-html-bench.sock"
FILE_SOCK="$BENCH_DIR/m6-file-bench.sock"

HTML_PID=""
FILE_PID=""
HTTP_PID=""

# ── Parse pass-through flags ──────────────────────────────────────────────────
# Extract flags we forward to m6-bench; discard protocol/suite filters
# (bench.sh controls those itself).
BENCH_PASS=()
SKIP_VERIFY=0
i=0
args=("$@")
while [[ $i -lt ${#args[@]} ]]; do
    arg="${args[$i]}"
    case "$arg" in
        --skip-verify)
            BENCH_PASS+=("--skip-verify")
            SKIP_VERIFY=1
            ;;
        --latency-n|--duration|--concurrency|--p99-limit-ms|--rps-min|--addr)
            BENCH_PASS+=("$arg" "${args[$((i+1))]}")
            i=$(( i + 1 ))
            ;;
        --http11-only|--http2-only|--http3-only|\
        --latency-only|--throughput-only|--path-only)
            # These are controlled by bench.sh; silently ignore if passed by user.
            ;;
        *)
            echo "Unknown flag: $arg" >&2
            exit 1
            ;;
    esac
    i=$(( i + 1 ))
done

# ── Build ─────────────────────────────────────────────────────────────────────
echo "==> Building binaries..."
cargo build --release \
    -p m6-http --bin m6-http \
    -p m6-http --bin m6-bench \
    -p m6-html \
    -p m6-file \
    2>&1 | grep -E "^(error|warning\[|Compiling|Finished)" || true

M6_HTTP="$REPO/target/release/m6-http"
M6_BENCH="$REPO/target/release/m6-bench"
M6_HTML="$REPO/target/release/m6-html"
M6_FILE="$REPO/target/release/m6-file"

for bin in "$M6_HTTP" "$M6_BENCH" "$M6_HTML" "$M6_FILE"; do
    if [[ ! -x "$bin" ]]; then
        echo "ERROR: $bin not found after build" >&2
        exit 1
    fi
done
echo "    all binaries ready."

# ── Set up site directory (once) ──────────────────────────────────────────────
echo "==> Setting up bench site at $SITE_DIR..."
rm -rf "$BENCH_DIR"
mkdir -p \
    "$SITE_DIR/templates" \
    "$SITE_DIR/data" \
    "$SITE_DIR/assets" \
    "$SITE_DIR/configs"

# TLS certs — reuse the test-fixture self-signed certs.
cp "$REPO/m6-http/tests/fixtures/site/cert.pem" "$SITE_DIR/cert.pem"
cp "$REPO/m6-http/tests/fixtures/site/key.pem"  "$SITE_DIR/key.pem"

# site.toml — four routes:
#   /              → m6-html (cacheable, Cache-Control: public)
#   /nocache/      → m6-html (not cacheable, cache = "no-store" in m6-html config)
#   /assets/{…}   → m6-file (cacheable, Cache-Control: public)
#   /tail/{…}     → m6-file (not cacheable, tail = true → no-store)
cat > "$SITE_DIR/site.toml" <<EOF
[site]
name   = "m6-bench"
domain = "localhost"

[errors]
mode = "internal"

[log]
level  = "warn"
format = "text"

[[backend]]
name    = "m6-html"
sockets = "$HTML_SOCK"

[[backend]]
name    = "m6-file"
sockets = "$FILE_SOCK"

[[route]]
path    = "/"
backend = "m6-html"

[[route]]
path    = "/nocache/"
backend = "m6-html"

[[route_group]]
glob    = "assets/**/*"
path    = "/assets/{relpath}"
backend = "m6-file"

[[route_group]]
glob    = "tail/**/*"
path    = "/tail/{relpath}"
backend = "m6-file"
EOF

# system.toml — bind address and TLS.
cat > "$BENCH_DIR/system.toml" <<EOF
[server]
bind     = "$BENCH_ADDR"
tls_cert = "cert.pem"
tls_key  = "key.pem"
EOF

# m6-html config:
#   /        → cache = "public" (default) — m6-http caches after first request
#   /nocache/ → cache = "no-store"        — never cached
cat > "$SITE_DIR/configs/m6-html.conf" <<EOF
global_params = ["data/site.json"]

[[route]]
path     = "/"
template = "templates/home.html"

[[route]]
path     = "/nocache/"
template = "templates/home.html"
cache    = "no-store"
EOF

# m6-file config:
#   /assets/{relpath} → normal serve; Cache-Control: public  — cached by m6-http
#   /tail/{relpath}   → tail=true;  Cache-Control: no-store  — never cached
cat > "$SITE_DIR/configs/m6-file.conf" <<EOF
[[route]]
path = "/assets/{relpath}"
root = "assets/"

[[route]]
path = "/tail/{relpath}"
root = "assets/"
tail = true
EOF

# Template (minimal — no nav loop, renders fast).
cat > "$SITE_DIR/templates/home.html" <<'EOF'
<!doctype html>
<html><head><meta charset="utf-8"><title>bench</title></head>
<body><h1>m6-bench</h1><p>ok</p></body>
</html>
EOF

# Data.
cat > "$SITE_DIR/data/site.json" <<'EOF'
{"site_name":"m6-bench"}
EOF

# Static file served by both /assets/ (cacheable) and /tail/ (no-store) routes.
# Both directories must exist so the route_group glob expansion finds the files.
mkdir -p "$SITE_DIR/tail"
echo "hello from m6-file" > "$SITE_DIR/assets/hello.txt"
echo "hello from m6-file" > "$SITE_DIR/tail/hello.txt"

echo "    site ready."

# ── Server lifecycle helpers ───────────────────────────────────────────────────

stop_all() {
    local pids=()
    [[ -n "$HTML_PID" ]] && pids+=("$HTML_PID")
    [[ -n "$FILE_PID" ]] && pids+=("$FILE_PID")
    [[ -n "$HTTP_PID" ]] && pids+=("$HTTP_PID")
    if (( ${#pids[@]} > 0 )); then
        kill "${pids[@]}" 2>/dev/null || true
        wait "${pids[@]}" 2>/dev/null || true
    fi
    rm -f "$HTML_SOCK" "$FILE_SOCK"
    HTML_PID=""
    FILE_PID=""
    HTTP_PID=""
}

start_backends() {
    echo "  -> Starting m6-html..."
    M6_SOCKET_OVERRIDE="$HTML_SOCK" \
        "$M6_HTML" "$SITE_DIR" "$SITE_DIR/configs/m6-html.conf" \
        --log-level warn \
        >"$BENCH_DIR/m6-html.log" 2>&1 &
    HTML_PID=$!

    echo "  -> Starting m6-file..."
    M6_SOCKET_OVERRIDE="$FILE_SOCK" \
        "$M6_FILE" "$SITE_DIR" "$SITE_DIR/configs/m6-file.conf" \
        --log-level warn \
        >"$BENCH_DIR/m6-file.log" 2>&1 &
    FILE_PID=$!

    # Wait for backend sockets (up to 10 s).
    local DEADLINE=$(( $(date +%s) + 10 ))
    while [[ ! -S "$HTML_SOCK" || ! -S "$FILE_SOCK" ]]; do
        if (( $(date +%s) > DEADLINE )); then
            echo "ERROR: backend sockets did not appear within 10 s." >&2
            echo "m6-html log:" >&2; cat "$BENCH_DIR/m6-html.log" >&2
            echo "m6-file log:" >&2; cat "$BENCH_DIR/m6-file.log" >&2
            exit 1
        fi
        sleep 0.2
    done
}

start_http() {
    echo "  -> Starting m6-http on $BENCH_ADDR..."
    "$M6_HTTP" "$SITE_DIR" "$BENCH_DIR/system.toml" \
        --log-level warn \
        >"$BENCH_DIR/m6-http.log" 2>&1 &
    HTTP_PID=$!

    # Wait for TCP port (up to 10 s).
    local DEADLINE=$(( $(date +%s) + 10 ))
    while ! nc -z 127.0.0.1 "$BENCH_PORT" 2>/dev/null; do
        if (( $(date +%s) > DEADLINE )); then
            echo "ERROR: m6-http port $BENCH_PORT did not open within 10 s." >&2
            cat "$BENCH_DIR/m6-http.log" >&2
            exit 1
        fi
        sleep 0.2
    done
}

# Clean up on exit (handles Ctrl-C etc.).
trap 'stop_all' EXIT

# ── Run one suite with a fresh server ────────────────────────────────────────
# Usage: run_suite <suite-label> <proto-flag> <suite-flag>
run_suite() {
    local label="$1"
    local proto_flag="$2"   # e.g. --http11-only
    local suite_flag="$3"   # e.g. --latency-only

    echo ""
    echo "======================================================================="
    echo "Suite: $label"
    echo "======================================================================="
    stop_all
    start_backends
    start_http

    "$M6_BENCH" \
        --addr "$BENCH_ADDR" \
        "$proto_flag" \
        "$suite_flag" \
        "${BENCH_PASS[@]}"
}

# ── Nine suites: H1/H2/H3 × latency/path/throughput ──────────────────────────

run_suite "HTTP/1.1 latency"    --http11-only --latency-only
run_suite "HTTP/2  latency"     --http2-only  --latency-only
run_suite "HTTP/3  latency"     --http3-only  --latency-only

run_suite "HTTP/1.1 path"       --http11-only --path-only
run_suite "HTTP/2  path"        --http2-only  --path-only
run_suite "HTTP/3  path"        --http3-only  --path-only

run_suite "HTTP/1.1 throughput" --http11-only --throughput-only
run_suite "HTTP/2  throughput"  --http2-only  --throughput-only
run_suite "HTTP/3  throughput"  --http3-only  --throughput-only

echo ""
echo "======================================================================="
echo "All suites complete."
echo "======================================================================="
