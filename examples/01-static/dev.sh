#!/bin/bash
# dev.sh — start the 01-static stack for local development.
set -e

SITE="$(cd "$(dirname "$0")" && pwd)"
M6="${M6:-$(cd "$SITE/../.." && pwd)}"
export PATH="$M6/target/release:$PATH"

# Development keys and state: a private per-user directory, see dev-home.sh.
. "$SITE/../dev-home.sh"
"$SITE/../dev-keys.sh"
. "$SITE/../own.sh"

# ── Clean up stale state ──────────────────────────────────────────────────────
mkdir -p /tmp/m6
own_init "$SITE" stack
own_reap
own_refuse_busy 8443 "example 01" || exit 1
rm -f /tmp/m6/m6-html.sock /tmp/m6/m6-file.sock

# ── Start services ────────────────────────────────────────────────────────────
M6_SOCKET_OVERRIDE=/tmp/m6/m6-html.sock \
    m6-html "$SITE" "$SITE/configs/m6-html.conf" &
HTML_PID=$!
own_track "$HTML_PID"

M6_SOCKET_OVERRIDE=/tmp/m6/m6-file.sock \
    m6-file "$SITE" "$SITE/configs/m6-file.conf" &
FILE_PID=$!
own_track "$FILE_PID"

trap 'own_stop_records; own_forget; wait 2>/dev/null' EXIT

# Wait for backend sockets. A backend that exits stops the stack here, before
# m6-http starts on a half-built one.
echo "Waiting for backend sockets..."
own_wait_sockets "example 01" /tmp/m6/m6-html.sock /tmp/m6/m6-file.sock || exit 1
echo "Backend sockets ready."

m6-http "$SITE" "$(m6_conf "$SITE/configs/system-dev.toml")" &
HTTP_PID=$!
own_track "$HTTP_PID"

echo "Running at https://localhost:8443"
wait
