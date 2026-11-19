#!/bin/bash
# test.sh — functional tests for example 10-api-tokens.
# Requires the server to be running (./dev.sh in another terminal).
set -e

SITE="$(cd "$(dirname "$0")" && pwd)"
M6="${M6:-$(cd "$SITE/../../../m6" && pwd)}"
export PATH="$M6/target/release:$PATH"

BASE="https://localhost:8443"
. "$SITE/../dev-home.sh"
CONF="$(m6_conf "$SITE/configs/m6-auth.conf")"
PASS=0
FAIL=0

check() {
    local desc="$1"; local expected="$2"; local actual="$3"
    if [ "$actual" = "$expected" ]; then
        echo "  PASS: $desc"
        PASS=$((PASS + 1))
    else
        echo "  FAIL: $desc (expected '$expected', got '$actual')"
        FAIL=$((FAIL + 1))
    fi
}

echo "=== 10-api-tokens functional tests ==="
echo ""

# ── Home page is public ───────────────────────────────────────────────────────
echo "-- Public access --"
CODE=$(curl -sk -o /dev/null -w "%{http_code}" "$BASE/")
check "home page returns 200" "200" "$CODE"

# ── Protected endpoint requires auth ──────────────────────────────────────────
echo "-- Unauthenticated access --"
CODE=$(curl -sk -o /dev/null -w "%{http_code}" "$BASE/api/data")
check "/api/data without token returns 401" "401" "$CODE"

CODE=$(curl -sk -o /dev/null -w "%{http_code}" "$BASE/dashboard")
check "/dashboard without session returns 401" "401" "$CODE"

# ── Bearer token authentication ───────────────────────────────────────────────
echo "-- Bearer token --"
TOKEN=$(m6-auth-cli "$CONF" token create api --name test-token --ttl-days 1)
if [ -z "$TOKEN" ]; then
    echo "  FAIL: token create returned empty output"
    FAIL=$((FAIL + 1))
else
    echo "  PASS: token create returned a JWT"
    PASS=$((PASS + 1))
fi

CODE=$(curl -sk -o /dev/null -w "%{http_code}" "$BASE/api/data" \
     -H "Authorization: Bearer $TOKEN")
check "/api/data with valid token returns 200" "200" "$CODE"

# ── Wrong role is rejected ────────────────────────────────────────────────────
echo "-- Role enforcement --"
# The 'api' user has role 'api' and 'user' but dashboard requires 'user' — should still work
CODE=$(curl -sk -o /dev/null -w "%{http_code}" "$BASE/dashboard" \
     -H "Authorization: Bearer $TOKEN")
check "/dashboard with bearer token (role:user) returns 200" "200" "$CODE"

# ── Token listing and revocation ──────────────────────────────────────────────
echo "-- Token management --"
LIST=$(m6-auth-cli "$CONF" token ls api --json)
COUNT=$(echo "$LIST" | python3 -c "import sys,json; print(len(json.load(sys.stdin)))" 2>/dev/null || echo "0")
if [ "$COUNT" -ge 1 ]; then
    echo "  PASS: token ls returns at least 1 token"
    PASS=$((PASS + 1))
else
    echo "  FAIL: token ls returned: $LIST"
    FAIL=$((FAIL + 1))
fi

TOKEN_ID=$(echo "$LIST" | python3 -c "import sys,json; print(json.load(sys.stdin)[0]['id'])" 2>/dev/null || echo "")
if [ -n "$TOKEN_ID" ]; then
    m6-auth-cli "$CONF" token revoke "$TOKEN_ID" >/dev/null
    echo "  PASS: token revoke succeeded"
    PASS=$((PASS + 1))

    # Note: the JWT itself remains valid until expiry (stateless verification).
    # Revoke removes it from the listing only. For immediate invalidation, use short TTLs.
    NEW_COUNT=$(m6-auth-cli "$CONF" token ls api --json | \
        python3 -c "import sys,json; print(len(json.load(sys.stdin)))" 2>/dev/null || echo "-1")
    check "token ls after revoke returns 0 tokens" "0" "$NEW_COUNT"
fi

# ── Error pages ───────────────────────────────────────────────────────────────
echo "-- Error pages --"
BODY=$(curl -sk "$BASE/no-such-path")
if echo "$BODY" | grep -q "404"; then
    echo "  PASS: 404 page contains status code"
    PASS=$((PASS + 1))
else
    echo "  FAIL: 404 page missing status code, got: ${BODY:0:200}"
    FAIL=$((FAIL + 1))
fi

echo ""
echo "=== Results: $PASS passed, $FAIL failed ==="
[ "$FAIL" -eq 0 ]
