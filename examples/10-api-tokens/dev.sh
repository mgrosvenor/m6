#!/bin/bash
# dev.sh — start the 10-api-tokens stack for local development.
set -e

SITE="$(cd "$(dirname "$0")" && pwd)"
M6="${M6:-$(cd "$SITE/../../../m6" && pwd)}"
export PATH="$M6/target/release:$PATH"

# Development keys and state: a private per-user directory, see dev-home.sh.
. "$SITE/../dev-home.sh"
"$SITE/../dev-keys.sh"

# ── Auth database ─────────────────────────────────────────────────────────────
# Always recreate to start with clean state (removes any API tokens from prior runs).
rm -f $M6_DEV_HOME/10-api-tokens/auth.db
echo "Creating 'api' user (password: secret)..."
m6-auth-cli "$(m6_conf "$SITE/configs/m6-auth.conf")" user add api --role api --role user --password secret

# ── Clean up stale state ──────────────────────────────────────────────────────
mkdir -p /tmp/m6
lsof -ti :8443 2>/dev/null | xargs kill -9 2>/dev/null || true
sleep 0.3
rm -f /tmp/m6/m6-html-api*.sock /tmp/m6/m6-file-api*.sock /tmp/m6/m6-auth-api*.sock /tmp/m6/check-m6-auth-api*.sock

# ── Start services ────────────────────────────────────────────────────────────
M6_SOCKET_OVERRIDE=/tmp/m6/m6-html-api.sock \
    m6-html "$SITE" "$SITE/configs/m6-html.conf" &
HTML_PID=$!

M6_SOCKET_OVERRIDE=/tmp/m6/m6-file-api.sock \
    m6-file "$SITE" "$SITE/configs/m6-file.conf" &
FILE_PID=$!

M6_SOCKET_OVERRIDE=/tmp/m6/m6-auth-api.sock \
    m6-auth-server "$SITE" "$(m6_conf "$SITE/configs/m6-auth.conf")" &
AUTH_PID=$!

# m6-auth-server also binds the check socket beside this one (check-<name>),
# which answers POST /check for m6-http: every `require` and the source
# node's /genid ask it who the caller is.

trap 'kill $HTML_PID $FILE_PID $AUTH_PID $HTTP_PID 2>/dev/null; wait 2>/dev/null' EXIT

# Wait for backend sockets
for i in $(seq 1 30); do
    [ -S /tmp/m6/m6-html-api.sock ] && \
    [ -S /tmp/m6/m6-file-api.sock ] && \
    [ -S /tmp/m6/m6-auth-api.sock ] && [ -S /tmp/m6/check-m6-auth-api.sock ] && break
    sleep 0.2
done

m6-http "$SITE" "$(m6_conf "$SITE/configs/system-dev.toml")" &
HTTP_PID=$!

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
