#!/bin/bash
# setup.sh — first-time setup for example 10-api-tokens.
set -e
SITE="$(cd "$(dirname "$0")" && pwd)"

# Development keys and state: a private per-user directory, see dev-home.sh.
. "$SITE/../dev-home.sh"
"$SITE/../dev-keys.sh"

# Create the 'api' user with the 'api' and 'user' roles
m6-auth-cli "$(m6_conf "$SITE/configs/m6-auth.conf")" user add api --role api --role user --password secret
echo "User 'api' created (password: secret)."

echo ""
echo "Setup complete. Run ./dev.sh to start the server."
echo ""
echo "Then create an API token:"
echo "  m6-auth-cli configs/m6-auth.conf token create api --name demo"
