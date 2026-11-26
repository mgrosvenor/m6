#!/usr/bin/env bash
# Test that dev-token.sh leaves its user holding the role it was asked for,
# including when the auth database already holds that user without the role,
# as a database kept from before genid-write became purge-write does. Needs
# m6-auth-cli on PATH and openssl.
set -uo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
T="$(mktemp -d)" || { echo "cannot make a temp dir" >&2; exit 1; }
trap 'rm -rf -- "$T"' EXIT
fail() { echo "FAIL: $1" >&2; exit 1; }

openssl ecparam -name prime256v1 -genkey -noout 2>/dev/null |
    openssl pkcs8 -topk8 -nocrypt -out "$T/auth.pem" 2>/dev/null ||
    fail "cannot make a signing key"
openssl ec -in "$T/auth.pem" -pubout -out "$T/auth.pub" 2>/dev/null ||
    fail "cannot write the public key"
cat > "$T/m6-auth.conf" <<CONF
[storage]
path = "$T/auth.db"

[tokens]
access_ttl  = 900
refresh_ttl = 2592000
issuer      = "localhost"

[keys]
private_key = "$T/auth.pem"
public_key  = "$T/auth.pub"
CONF

roles() { m6-auth-cli "$T/m6-auth.conf" user roles publisher; }

# A fresh database: the user is made with the role and a token is written.
"$HERE/dev-token.sh" "$T/m6-auth.conf" publisher purge-write "$T/fresh.token" ||
    fail "dev-token.sh failed on a fresh database"
[ -s "$T/fresh.token" ] || fail "no token written on a fresh database"
[ "$(roles)" = "purge-write" ] || fail "a fresh user holds '$(roles)', not purge-write"

# A kept database whose user holds only the old role.
m6-auth-cli "$T/m6-auth.conf" user roles publisher --set genid-write --unset purge-write \
    >/dev/null 2>&1 || fail "cannot set up the old role"
[ "$(roles)" = "genid-write" ] || fail "setup: the user holds '$(roles)'"
"$HERE/dev-token.sh" "$T/m6-auth.conf" publisher purge-write "$T/kept.token" ||
    fail "dev-token.sh failed on a kept database"
case ",$(roles)," in
    *,purge-write,*) ;;
    *) fail "a kept user holds '$(roles)' after dev-token.sh, without purge-write" ;;
esac

echo "dev-token.sh: ok"
