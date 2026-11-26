#!/bin/bash
# dev-token.sh <m6-auth.conf> <user> <role> <token-file>
#
# Writes a token for <user>, who holds <role>, to <token-file>. m6-http's
# `PURGE` endpoint is gated by roles (`[registration] write_require` and
# `read_require` in the system config), so the token is what lets a renderer
# purge (`purge-write`) or a cache node read `GET /m6/status` (`purge-read`).
# Needs the development keys (dev-keys.sh) and an auth
# database. Writes a token when none exists or the file is over 25 days old; the
# token lives 30 days (--ttl-days), and an older one stays valid until then.
set -euo pipefail

CONF="$1"
USER_NAME="$2"
ROLE="$3"
OUT="$4"

# A second run finds the user already there, which is fine. The role is granted
# on every run, because `user add` leaves an existing user's roles alone: an
# auth database kept from before a role was renamed (genid-write became
# purge-write) would otherwise hand out a token whose user lacks the role, and
# every PURGE with it is refused. m6-auth reads roles from the database at each
# check, so the grant counts at once and an existing token needs no reissue.
m6-auth-cli "$CONF" user add "$USER_NAME" --role "$ROLE" \
    --password "$(openssl rand -hex 16)" >/dev/null 2>&1 || true
m6-auth-cli "$CONF" user roles "$USER_NAME" --set "$ROLE" >/dev/null 2>&1

if [ ! -s "$OUT" ] || [ -n "$(find "$OUT" -mtime +25 2>/dev/null)" ]; then
    mkdir -p "$(dirname "$OUT")"
    umask 077
    m6-auth-cli "$CONF" token create "$USER_NAME" --name "$USER_NAME" --ttl-days 30 > "$OUT"
fi
