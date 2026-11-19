#!/bin/bash
# dev.sh — start all services for example 11-admin-dashboard.
# M6_PORT=8444
set -e

SITE="$(cd "$(dirname "$0")" && pwd)"
EXAMPLES="$(cd "$SITE/../.." && pwd)"
M6="${M6:-$(cd "$SITE/../../../m6" && pwd)}"
export PATH="$M6/target/release:$EXAMPLES/target/release:$PATH"

# Development keys and state: a private per-user directory, see dev-home.sh.
. "$SITE/../dev-home.sh"
"$SITE/../dev-keys.sh"

# ── Setup check ───────────────────────────────────────────────────────────────
if [ ! -f $M6_DEV_HOME/11-admin-dashboard/auth.db ]; then
    echo "First-time setup required. Running setup.sh..."
    "$SITE/setup.sh"
fi

# ── Publisher token ───────────────────────────────────────────────────────────
# The renderer sends POST /genid with it (see [genid] in its config).
"$SITE/../dev-token.sh" "$(m6_conf "$SITE/configs/m6-auth.conf")" publisher genid-write $M6_DEV_HOME/11-admin-dashboard/genid-write.token

# ── Clean up stale state ──────────────────────────────────────────────────────
mkdir -p /tmp/m6
lsof -ti :8444 2>/dev/null | xargs kill -9 2>/dev/null || true
sleep 0.3
rm -f /tmp/m6/m6-auth-admin.sock /tmp/m6/check-m6-auth-admin.sock \
      /tmp/m6/render-admin.sock \
      /tmp/m6/m6-file-admin.sock

echo "=== 11-admin-dashboard ==="

# ── m6-auth ───────────────────────────────────────────────────────────────────
M6_SOCKET_OVERRIDE=/tmp/m6/m6-auth-admin.sock \
    m6-auth-server "$SITE" "$(m6_conf "$SITE/configs/m6-auth.conf")" &
AUTH_PID=$!

# m6-auth-server also binds the check socket beside this one (check-<name>),
# which answers POST /check for m6-http: every `require` and the source
# node's /genid ask it who the caller is.

# ── render-admin ──────────────────────────────────────────────────────────────
M6_SOCKET_OVERRIDE=/tmp/m6/render-admin.sock \
    render-admin "$SITE" "$(m6_conf "$SITE/configs/render-admin.conf")" &
ADMIN_PID=$!

# ── m6-file (static dashboard) ────────────────────────────────────────────────
M6_SOCKET_OVERRIDE=/tmp/m6/m6-file-admin.sock \
    m6-file "$SITE" "$SITE/configs/m6-file.conf" &
FILE_PID=$!

trap 'kill $AUTH_PID $ADMIN_PID $FILE_PID $HTTP_PID 2>/dev/null; wait 2>/dev/null' EXIT

# ── Wait for backend sockets ──────────────────────────────────────────────────
printf "waiting for backends"
for i in $(seq 1 50); do
    [ -S /tmp/m6/m6-auth-admin.sock ] && \
    [ -S /tmp/m6/check-m6-auth-admin.sock ] && \
    [ -S /tmp/m6/render-admin.sock  ] && \
    [ -S /tmp/m6/m6-file-admin.sock ] && { echo " ready."; break; }
    printf "."; sleep 0.2
done

# ── m6-http ───────────────────────────────────────────────────────────────────
# Truncate the log file so render-admin always reads current-session data.
mkdir -p $M6_DEV_HOME/11-admin-dashboard/state
: > $M6_DEV_HOME/11-admin-dashboard/m6-http-admin.log
STATE_DIRECTORY=$M6_DEV_HOME/11-admin-dashboard/state \
    m6-http "$SITE" "$(m6_conf "$SITE/configs/system-dev.toml")" >> $M6_DEV_HOME/11-admin-dashboard/m6-http-admin.log 2>&1 &
HTTP_PID=$!

echo ""
echo "Admin dashboard: https://localhost:8444/"
echo "Sign in:  admin / admin"
echo ""
echo "Press Ctrl+C to stop."
wait
