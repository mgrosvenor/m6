#!/bin/bash
# dev.sh — start the 07-dev-to-production stack for local development.
set -e

SITE="$(cd "$(dirname "$0")" && pwd)"
M6="${M6:-$(cd "$SITE/../.." && pwd)}"
EXAMPLES="${EXAMPLES:-$(cd "$SITE/.." && pwd)}"
export PATH="$M6/target/release:$PATH"

# Development keys and state: a private per-user directory, see dev-home.sh.
. "$SITE/../dev-home.sh"
"$SITE/../dev-keys.sh"
. "$SITE/../own.sh"

# ── Auth database ─────────────────────────────────────────────────────────────
if [ ! -f $M6_DEV_HOME/07-dev-to-production/auth.db ]; then
    echo "Creating admin user (password: admin)..."
    m6-auth-cli "$(m6_conf "$SITE/configs/m6-auth.conf")" user add admin --password admin
    m6-auth-cli "$(m6_conf "$SITE/configs/m6-auth.conf")" group add editors
    m6-auth-cli "$(m6_conf "$SITE/configs/m6-auth.conf")" group member add editors admin
fi

# ── Publisher token ───────────────────────────────────────────────────────────
# The renderer sends POST /genid with it (see [genid] in its config).
"$SITE/../dev-token.sh" "$(m6_conf "$SITE/configs/m6-auth.conf")" publisher purge-write $M6_DEV_HOME/07-dev-to-production/genid-write.token

# ── Generate posts.json from markdown ─────────────────────────────────────────
m6-md "$SITE/content/posts/" --output "$SITE/data/posts.json"

# ── Clean up stale state ──────────────────────────────────────────────────────
mkdir -p /tmp/m6
own_init "$SITE" stack
own_reap
own_refuse_busy 8443 "example 07" || exit 1
rm -f /tmp/m6/m6-html*.sock /tmp/m6/m6-file*.sock /tmp/m6/m6-auth*.sock /tmp/m6/check-m6-auth*.sock /tmp/m6/render-cms-prod*.sock

# ── Start services ────────────────────────────────────────────────────────────
M6_SOCKET_OVERRIDE=/tmp/m6/m6-html.sock \
    m6-html "$SITE" "$SITE/configs/m6-html.conf" &
HTML_PID=$!
own_track "$HTML_PID"

M6_SOCKET_OVERRIDE=/tmp/m6/m6-file.sock \
    m6-file "$SITE" "$SITE/configs/m6-file.conf" &
FILE_PID=$!
own_track "$FILE_PID"

M6_SOCKET_OVERRIDE=/tmp/m6/m6-auth.sock \
    m6-auth-server "$SITE" "$(m6_conf "$SITE/configs/m6-auth.conf")" &
AUTH_PID=$!
own_track "$AUTH_PID"

# m6-auth-server also binds the check socket beside this one (check-<name>),
# which answers POST /check for m6-http: every `require` and the source
# node's /genid ask it who the caller is.

M6_SOCKET_OVERRIDE=/tmp/m6/render-cms-prod.sock \
    "$EXAMPLES/target/release/render-cms-prod" "$SITE" "$(m6_conf "$SITE/configs/render-cms.conf")" &
RENDER_PID=$!
own_track "$RENDER_PID"

trap 'own_stop_records; own_forget; wait 2>/dev/null' EXIT

# Wait for backend sockets. A backend that exits stops the stack here, before
# m6-http starts on a half-built one.
echo "Waiting for backend sockets..."
own_wait_sockets "example 07" \
    /tmp/m6/m6-html.sock /tmp/m6/m6-file.sock \
    /tmp/m6/m6-auth.sock /tmp/m6/check-m6-auth.sock \
    /tmp/m6/render-cms-prod.sock || exit 1
echo "Backend sockets ready."

mkdir -p $M6_DEV_HOME/07-dev-to-production/state
STATE_DIRECTORY=$M6_DEV_HOME/07-dev-to-production/state \
    m6-http "$SITE" "$(m6_conf "$SITE/configs/system-dev.toml")" &
HTTP_PID=$!
own_track "$HTTP_PID"

echo ""
echo "CMS:   https://localhost:8443/cms  (login: admin / admin)"
echo "Blog:  https://localhost:8443/blog"
echo ""
wait
