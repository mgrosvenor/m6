#!/bin/bash
set -e
SITE="$(cd "$(dirname "$0")" && pwd)"
M6="${M6:-$(cd "$SITE/../../../m6" && pwd)}"
EXAMPLES="${EXAMPLES:-$(cd "$SITE/../.." && pwd)}"
export PATH="$M6/target/release:$PATH"

NO_OPEN=0
for arg in "$@"; do
    [[ "$arg" == "--no-open" ]] && NO_OPEN=1
done
# Suppress browser launch when running non-interactively (CI, cargo test, etc.)
[[ -t 1 ]] || NO_OPEN=1
# Respect M6_NO_BROWSER exported by m6-run-eg.
[[ "${M6_NO_BROWSER:-0}" = "1" ]] && NO_OPEN=1

GREEN='\033[0;32m'; YELLOW='\033[1;33m'; RESET='\033[0m'
info() { echo -e "${YELLOW}----${RESET} $1"; }

# ── Build ──────────────────────────────────────────────────────────────────────
info "Building m6 binaries..."
(cd "$M6" && cargo build --release --quiet)

info "Building render-cms..."
(cd "$EXAMPLES" && cargo build --release -p render-cms --quiet)

# Development keys and state: a private per-user directory, see dev-home.sh.
. "$SITE/../dev-home.sh"
"$SITE/../dev-keys.sh"

# ── Auth database ─────────────────────────────────────────────────────────────
if [ ! -f $M6_DEV_HOME/05-cms/auth.db ]; then
    info "Creating admin user (password: admin)..."
    m6-auth-cli "$(m6_conf "$SITE/configs/m6-auth.conf")" user add admin --password admin
    m6-auth-cli "$(m6_conf "$SITE/configs/m6-auth.conf")" group add editors
    m6-auth-cli "$(m6_conf "$SITE/configs/m6-auth.conf")" group member add editors admin
fi

# ── Publisher token ───────────────────────────────────────────────────────────
# The renderer sends POST /genid with it (see [genid] in its config).
"$SITE/../dev-token.sh" "$(m6_conf "$SITE/configs/m6-auth.conf")" publisher genid-write $M6_DEV_HOME/05-cms/genid-write.token

# Run again after editing a post, then restart m6-html so it reads the new file.
# ── Generate posts.json from markdown ─────────────────────────────────────────
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

rm -f /tmp/m6/m6-html*.sock /tmp/m6/m6-file*.sock /tmp/m6/m6-auth*.sock /tmp/m6/check-m6-auth*.sock /tmp/m6/render-cms*.sock

# ── Start services ────────────────────────────────────────────────────────────
info "Starting m6-html..."
M6_SOCKET_OVERRIDE=/tmp/m6/m6-html.sock \
    m6-html "$SITE" "$SITE/configs/m6-html.conf" >> "$SITE/logs/m6-html.log" 2>&1 &
HTML_PID=$!

info "Starting m6-file..."
M6_SOCKET_OVERRIDE=/tmp/m6/m6-file.sock \
    m6-file "$SITE" "$SITE/configs/m6-file.conf" >> "$SITE/logs/m6-file.log" 2>&1 &
FILE_PID=$!

info "Starting m6-auth-server..."
M6_SOCKET_OVERRIDE=/tmp/m6/m6-auth.sock \
    m6-auth-server "$SITE" "$(m6_conf "$SITE/configs/m6-auth.conf")" >> "$SITE/logs/m6-auth.log" 2>&1 &
AUTH_PID=$!

# m6-auth-server also binds the check socket beside this one (check-<name>),
# which answers POST /check for m6-http: every `require` and the source
# node's /genid ask it who the caller is.

info "Starting render-cms..."
M6_SOCKET_OVERRIDE=/tmp/m6/render-cms.sock \
    "$EXAMPLES/target/release/render-cms" "$SITE" "$(m6_conf "$SITE/configs/render-cms.conf")" >> "$SITE/logs/render-cms.log" 2>&1 &
CMS_PID=$!

trap 'echo ""; info "Stopping..."; kill $HTML_PID $FILE_PID $AUTH_PID $CMS_PID $HTTP_PID 2>/dev/null; wait 2>/dev/null' EXIT

# Wait for backend sockets
echo -n "Waiting for backend sockets"
for i in $(seq 1 30); do
    if [[ -S /tmp/m6/m6-html.sock && -S /tmp/m6/m6-file.sock && -S /tmp/m6/m6-auth.sock && -S /tmp/m6/check-m6-auth.sock && -S /tmp/m6/render-cms.sock ]]; then
        echo " ready."
        break
    fi
    echo -n "."
    sleep 0.25
done

info "Starting m6-http..."
mkdir -p $M6_DEV_HOME/05-cms/state
STATE_DIRECTORY=$M6_DEV_HOME/05-cms/state \
    m6-http "$SITE" "$(m6_conf "$SITE/configs/system-dev.toml")" >> "$SITE/logs/m6-http.log" 2>&1 &
HTTP_PID=$!

# Wait for server readiness
CURL="$(command -v /opt/homebrew/opt/curl/bin/curl 2>/dev/null || echo curl)"
echo -n "Waiting for server"
READY=false
for i in $(seq 1 40); do
    code=$($CURL -sk --http1.1 -o /dev/null -w "%{http_code}" https://127.0.0.1:8443/ 2>/dev/null || true)
    if [[ "$code" == "200" || "$code" == "302" ]]; then
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

[[ $NO_OPEN -eq 0 ]] && open "https://localhost:8443/" 2>/dev/null || true

echo ""
echo -e "${GREEN}Stack is running.${RESET}  Press Ctrl-C to stop."
echo ""
echo "  CMS:    https://localhost:8443/cms  (login: admin / admin)"
echo "  Blog:   https://localhost:8443/blog"
echo ""
echo "  Logs:   $SITE/logs/m6-http.log"
echo "          $SITE/logs/m6-html.log"
echo "          $SITE/logs/m6-file.log"
echo "          $SITE/logs/m6-auth.log"
echo "          $SITE/logs/render-cms.log"
echo ""

wait
