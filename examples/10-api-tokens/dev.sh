#!/bin/bash
# dev.sh — start the 10-api-tokens stack for local development.
set -e

SITE="$(cd "$(dirname "$0")" && pwd)"
M6="${M6:-$(cd "$SITE/../.." && pwd)}"
export PATH="$M6/target/release:$PATH"

# Development keys and state: a private per-user directory, see dev-home.sh.
. "$SITE/../dev-home.sh"
"$SITE/../dev-keys.sh"
. "$SITE/../own.sh"

# ── Auth database ─────────────────────────────────────────────────────────────
# Always recreate to start with clean state (removes any API tokens from prior runs).
rm -f $M6_DEV_HOME/10-api-tokens/auth.db
echo "Creating 'api' user (password: secret)..."
m6-auth-cli "$(m6_conf "$SITE/configs/m6-auth.conf")" user add api --role api --role user --password secret

# ── Clean up stale state ──────────────────────────────────────────────────────
mkdir -p /tmp/m6
own_init "$SITE" stack
own_reap
own_refuse_busy 8443 "example 10" || exit 1
rm -f /tmp/m6/m6-html-api*.sock /tmp/m6/m6-file-api*.sock /tmp/m6/m6-auth-api*.sock /tmp/m6/check-m6-auth-api*.sock

# ── Start services ────────────────────────────────────────────────────────────
M6_SOCKET_OVERRIDE=/tmp/m6/m6-html-api.sock \
    m6-html "$SITE" "$SITE/configs/m6-html.conf" &
HTML_PID=$!
own_track "$HTML_PID"

M6_SOCKET_OVERRIDE=/tmp/m6/m6-file-api.sock \
    m6-file "$SITE" "$SITE/configs/m6-file.conf" &
FILE_PID=$!
own_track "$FILE_PID"

M6_SOCKET_OVERRIDE=/tmp/m6/m6-auth-api.sock \
    m6-auth-server "$SITE" "$(m6_conf "$SITE/configs/m6-auth.conf")" &
AUTH_PID=$!
own_track "$AUTH_PID"

# m6-auth-server also binds the check socket beside this one (check-<name>),
# which answers POST /check for m6-http: every `require` and the source
# node's /genid ask it who the caller is.

trap 'own_stop_records; own_forget; wait 2>/dev/null' EXIT

# Wait for backend sockets. A backend that exits stops the stack here, before
# m6-http starts on a half-built one.
echo "Waiting for backend sockets..."
own_wait_sockets "example 10" \
    /tmp/m6/m6-html-api.sock /tmp/m6/m6-file-api.sock \
    /tmp/m6/m6-auth-api.sock /tmp/m6/check-m6-auth-api.sock || exit 1
echo "Backend sockets ready."

m6-http "$SITE" "$(m6_conf "$SITE/configs/system-dev.toml")" &
HTTP_PID=$!
own_track "$HTTP_PID"

echo "Running at https://localhost:8443"
echo ""
echo "Login at  https://localhost:8443/login  (api / secret)"
echo ""
echo "Create an API token:"
echo "  m6-auth-cli configs/m6-auth.conf token create api --name demo"
echo ""
echo "Call the protected endpoint:"
echo "  curl -sk https://localhost:8443/api/data -H 'Authorization: Bearer <token>'"
wait
