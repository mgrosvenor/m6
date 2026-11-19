#!/bin/bash
# dev.sh — start the 02-blog stack for local development.
set -e

SITE="$(cd "$(dirname "$0")" && pwd)"
M6="${M6:-$(cd "$SITE/../../.." && pwd)/m6}"

GREEN='\033[0;32m'; YELLOW='\033[1;33m'; RESET='\033[0m'
info() { echo -e "${YELLOW}----${RESET} $1"; }

# ── Build ──────────────────────────────────────────────────────────────────────
info "Building m6 binaries..."
(cd "$M6" && cargo build --release --quiet)
export PATH="$M6/target/release:$PATH"

# Development keys and state: a private per-user directory, see dev-home.sh.
. "$SITE/../dev-home.sh"
"$SITE/../dev-keys.sh"

# Run again after editing a post, then restart m6-html so it reads the new file.
# ── Generate posts.json from Markdown (initial pass) ──────────────────────────
if command -v m6-md &>/dev/null; then
    info "Generating posts.json..."
    m6-md "$SITE/content/posts/" --output "$SITE/data/posts.json"
fi

# ── Clean up stale state ──────────────────────────────────────────────────────
mkdir -p "$SITE/logs"
mkdir -p /tmp/m6

lsof -ti :8443 2>/dev/null | xargs kill -9 2>/dev/null || true
# Kill only this example's own processes: matching the site directory in the
# command line cannot reach another m6 on the machine. The port holder is killed
# above, to clear a stale run of this example.
kill_own() {
    local pid
    for pid in $(pgrep -f "$SITE" 2>/dev/null || true); do
        [ "$pid" = "$$" ] && continue
        kill "$pid" 2>/dev/null || true
    done
}
kill_own
sleep 0.3

rm -f /tmp/m6/m6-html*.sock /tmp/m6/m6-file*.sock

# ── Start services ────────────────────────────────────────────────────────────
info "Starting m6-html..."
M6_SOCKET_OVERRIDE=/tmp/m6/m6-html.sock \
    m6-html "$SITE" "$SITE/configs/m6-html.conf" >> "$SITE/logs/m6-html.log" 2>&1 &
HTML_PID=$!

info "Starting m6-file..."
M6_SOCKET_OVERRIDE=/tmp/m6/m6-file.sock \
    m6-file "$SITE" "$SITE/configs/m6-file.conf" >> "$SITE/logs/m6-file.log" 2>&1 &
FILE_PID=$!

trap 'echo ""; info "Stopping..."; kill $HTML_PID $FILE_PID $HTTP_PID 2>/dev/null; wait 2>/dev/null' EXIT

# Wait for backend sockets
echo -n "Waiting for backend sockets"
for i in $(seq 1 20); do
    if [[ -S /tmp/m6/m6-html.sock && -S /tmp/m6/m6-file.sock ]]; then
        echo " ready."
        break
    fi
    echo -n "."
    sleep 0.25
done

info "Starting m6-http..."
m6-http "$SITE" "$(m6_conf "$SITE/configs/system-dev.toml")" >> "$SITE/logs/m6-http.log" 2>&1 &
HTTP_PID=$!

# Wait for server readiness
CURL="$(command -v /opt/homebrew/opt/curl/bin/curl 2>/dev/null || echo curl)"
echo -n "Waiting for server"
READY=false
for i in $(seq 1 40); do
    code=$($CURL -sk --http1.1 -o /dev/null -w "%{http_code}" https://127.0.0.1:8443/ 2>/dev/null || true)
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
    echo "ERROR: server did not become ready. Check logs in $SITE/logs/" >&2
    exit 1
fi

[[ "${M6_NO_BROWSER:-0}" = "0" ]] && open "https://localhost:8443/" 2>/dev/null || true

echo ""
echo -e "${GREEN}Stack is running.${RESET}  Press Ctrl-C to stop."
echo ""
echo "  Home:   https://localhost:8443/"
echo "  Blog:   https://localhost:8443/blog"
echo "  About:  https://localhost:8443/about"
echo ""
echo "  Logs:   $SITE/logs/m6-http.log"
echo "          $SITE/logs/m6-html.log"
echo "          $SITE/logs/m6-file.log"
echo ""
echo "  PIDs:   m6-html=$HTML_PID  m6-file=$FILE_PID  m6-http=$HTTP_PID"
echo ""

wait
