#!/bin/bash
# dev.sh — run a 6-node global deployment topology on a single machine.
# M6_PORT=9001
#
# Topology (all on loopback):
#   Origin H2C (9000) ← cache nodes connect here (H2C, no TLS)
#   Origin public (9001, TLS) ← direct access
#   Cache edge-a (9002), edge-b (9003), edge-c (9004), edge-d (9005), edge-e (9006)
#
# The cache nodes proxy all traffic to the origin H2C instance.
# This mirrors production where cache nodes talk to the origin over WireGuard H2C.
#
# Reuses 07-dev-to-production's site content, auth setup, and backend configs.
# Prerequisites: m6-http, m6-html, m6-file, m6-auth-server, m6-md,
#                render-cms (from 07-dev-to-production), mkcert.
set -e

SITE09="$(cd "$(dirname "$0")" && pwd)"
SITE07="$(cd "$SITE09/../07-dev-to-production" && pwd)"
M6="${M6:-$(cd "$SITE09/../.." && pwd)}"
EXAMPLES="${EXAMPLES:-$(cd "$SITE09/.." && pwd)}"
export PATH="$M6/target/release:$PATH"

# Development keys and state: a private per-user directory, see dev-home.sh.
. "$SITE09/../dev-home.sh"
"$SITE09/../dev-keys.sh"
. "$SITE09/../own.sh"

# ── Require 07 to have been set up first ─────────────────────────────────────
if [ ! -f $M6_DEV_HOME/07-dev-to-production/auth.db ]; then
    echo "ERROR: $M6_DEV_HOME/07-dev-to-production/auth.db not found."
    echo "Run 07-dev-to-production/dev.sh first to set up the auth database." >&2
    exit 1
fi

# The origin's generation: the CMS publishes with a genid-write token, and each cache
# node follows the origin with a genid-read token.
STATE=$M6_DEV_HOME/09-global-deployment
mkdir -p "$STATE/state"
"$SITE09/../dev-token.sh" "$(m6_conf "$SITE07/configs/m6-auth.conf")" publisher genid-write "$STATE/genid-write.token"
for edge in a b c d e; do
    "$SITE09/../dev-token.sh" "$(m6_conf "$SITE07/configs/m6-auth.conf")" "reader-edge-$edge" genid-read "$STATE/genid-read-edge-$edge.token"
done
# 07's renderer config publishes to port 8443. Here the origin's TLS port is 9001.
sed -e 's#https://localhost:8443/genid#https://localhost:9001/genid#' \
    -e 's#127.0.0.1:8443#127.0.0.1:9001#' \
    -e "s#$M6_DEV_HOME/07-dev-to-production/genid-write.token#$STATE/genid-write.token#" \
    "$(m6_conf "$SITE07/configs/render-cms.conf")" > "$STATE/render-cms.conf"

# ── Generate posts.json ────────────────────────────────────────────────────────
m6-md "$SITE07/content/posts/" --output "$SITE07/data/posts.json"

# ── Stop this example's previous run; refuse a foreign port holder ────────────
own_init "$SITE09" stack
own_reap
for port in 9000 9001 9002 9003 9004 9005 9006; do
    own_refuse_busy "$port" "example 09" || exit 1
done
rm -f /tmp/m6/m6-html*.sock /tmp/m6/m6-file*.sock /tmp/m6/m6-auth*.sock \
       /tmp/m6/check-m6-auth*.sock /tmp/m6/render-cms*.sock
mkdir -p /tmp/m6

# ── Backend services (07's content and config) ────────────────────────────────
M6_SOCKET_OVERRIDE=/tmp/m6/m6-html.sock \
    m6-html "$SITE07" "$SITE07/configs/m6-html.conf" &
HTML_PID=$!
own_track "$HTML_PID"

M6_SOCKET_OVERRIDE=/tmp/m6/m6-file.sock \
    m6-file "$SITE07" "$SITE07/configs/m6-file.conf" &
FILE_PID=$!
own_track "$FILE_PID"

M6_SOCKET_OVERRIDE=/tmp/m6/m6-auth.sock \
    m6-auth-server "$SITE07" "$(m6_conf "$SITE07/configs/m6-auth.conf")" &
AUTH_PID=$!
own_track "$AUTH_PID"

M6_SOCKET_OVERRIDE=/tmp/m6/render-cms.sock \
    "$EXAMPLES/target/release/render-cms" "$SITE07" "$STATE/render-cms.conf" &
RENDER_PID=$!
own_track "$RENDER_PID"

# Stop what this run started, including when the wait below fails. The origin
# and the cache nodes are tracked after the wait, and this same trap covers them.
trap 'own_stop_records; own_forget; wait 2>/dev/null' EXIT

# Wait for backend sockets. A backend that exits stops the stack here, before
# the origin starts on a half-built one.
echo "Waiting for backend services..."
own_wait_sockets "example 09" \
    /tmp/m6/m6-html.sock /tmp/m6/m6-file.sock \
    /tmp/m6/m6-auth.sock /tmp/m6/check-m6-auth.sock \
    /tmp/m6/render-cms.sock || exit 1
echo "Backend services ready."

# ── Origin — TLS (port 9001) + H2C backbone (port 9000) ──────────────────────
# Single m6-http instance: bind = TLS for users, h2c_bind = cleartext for cache nodes
STATE_DIRECTORY="$STATE/state" \
    m6-http "$SITE09/site-origin" "$(m6_conf "$SITE09/configs/local/origin.toml")" &
ORIGIN_PID=$!
own_track "$ORIGIN_PID"

# ── Cache nodes (ports 9002–9006, all proxy to origin H2C on 9000) ────────────
m6-http "$SITE09/site-cache" "$(m6_conf "$SITE09/configs/local/cache-edge-a.toml")" &
own_track "$!"

m6-http "$SITE09/site-cache" "$(m6_conf "$SITE09/configs/local/cache-edge-b.toml")" &
own_track "$!"

m6-http "$SITE09/site-cache" "$(m6_conf "$SITE09/configs/local/cache-edge-c.toml")" &
own_track "$!"

m6-http "$SITE09/site-cache" "$(m6_conf "$SITE09/configs/local/cache-edge-d.toml")" &
own_track "$!"

m6-http "$SITE09/site-cache" "$(m6_conf "$SITE09/configs/local/cache-edge-e.toml")" &
own_track "$!"

echo ""
echo "┌─────────────────────────────────────────────────────────────┐"
echo "│  6-node global deployment — local demo                      │"
echo "├─────────────────────────────────────────────────────────────┤"
echo "│  Origin                                                     │"
echo "│    H2C backbone : http://127.0.0.1:9000  (cache nodes only) │"
echo "│    Public TLS   : https://127.0.0.1:9001                    │"
echo "├─────────────────────────────────────────────────────────────┤"
echo "│  Cache nodes                                                │"
echo "│    edge-a       : https://127.0.0.1:9002                    │"
echo "│    edge-b       : https://127.0.0.1:9003                    │"
echo "│    edge-c       : https://127.0.0.1:9004                    │"
echo "│    edge-d       : https://127.0.0.1:9005                    │"
echo "│    edge-e       : https://127.0.0.1:9006                    │"
echo "├─────────────────────────────────────────────────────────────┤"
echo "│  CMS: https://127.0.0.1:9001/cms  (login: admin / admin)    │"
echo "│  Each cache node proxies CMS to origin (no-store cache)     │"
echo "└─────────────────────────────────────────────────────────────┘"
echo ""

wait
