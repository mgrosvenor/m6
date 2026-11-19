#!/bin/bash
set -e
SITE="$(cd "$(dirname "$0")" && pwd)"

# Development keys and state: a private per-user directory, see dev-home.sh.
. "$SITE/../dev-home.sh"
"$SITE/../dev-keys.sh"

# Create first admin user — database created automatically if absent
m6-auth-cli "$(m6_conf "$SITE/configs/m6-auth.conf")" user add admin --role admin --password admin

echo "Setup complete. Start the server with ./dev.sh"
