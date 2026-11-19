#!/bin/bash
# test.sh — functional tests for example 11-admin-dashboard.
# Requires the server to be running (./dev.sh in another terminal).
set -e

SITE="$(cd "$(dirname "$0")" && pwd)"
M6="$(cd "$SITE/../../../m6" && pwd)"
export PATH="$M6/target/release:$PATH"

BASE="https://localhost:8444"
. "$SITE/../dev-home.sh"
CONF="$(m6_conf "$SITE/configs/m6-auth.conf")"
PASS=0
FAIL=0

check() {
    local desc="$1" expected="$2" actual="$3"
    if [ "$actual" = "$expected" ]; then
        echo "  PASS: $desc"
        PASS=$((PASS + 1))
    else
        echo "  FAIL: $desc (expected '$expected', got '$actual')"
        FAIL=$((FAIL + 1))
    fi
}

check_contains() {
    local desc="$1" needle="$2" haystack="$3"
    if echo "$haystack" | grep -q "$needle"; then
        echo "  PASS: $desc"
        PASS=$((PASS + 1))
    else
        echo "  FAIL: $desc (expected to contain '$needle')"
        echo "         got: ${haystack:0:200}"
        FAIL=$((FAIL + 1))
    fi
}

echo "=== 11-admin-dashboard functional tests ==="
echo ""

# Get an admin bearer token
TOKEN=$(m6-auth-cli "$CONF" token create admin --name test --ttl-days 1)
if [ -z "$TOKEN" ]; then
    echo "FATAL: could not obtain admin token"
    exit 1
fi

AUTH="-H \"Authorization: Bearer $TOKEN\""

# ── Unauthenticated access ────────────────────────────────────────────────────
echo "-- Unauthenticated --"
CODE=$(curl -sk -o /dev/null -w "%{http_code}" "$BASE/api/admin/perf")
check "/api/admin/perf without token returns 401" "401" "$CODE"

# ── /api/admin/perf ───────────────────────────────────────────────────────────
echo "-- Perf --"
BODY=$(curl -sk -H "Authorization: Bearer $TOKEN" "$BASE/api/admin/perf")
check_contains "/api/admin/perf returns history key" '"history"' "$BODY"

# ── /api/admin/system ─────────────────────────────────────────────────────────
echo "-- System --"
BODY=$(curl -sk -H "Authorization: Bearer $TOKEN" "$BASE/api/admin/system")
check_contains "/api/admin/system returns uptime" '"uptime_secs"' "$BODY"
check_contains "/api/admin/system returns cpu" '"cpu_usage_pct"' "$BODY"
check_contains "/api/admin/system returns memory" '"memory"' "$BODY"
check_contains "/api/admin/system returns disks" '"disks"' "$BODY"

# ── /api/admin/bench (targets) ────────────────────────────────────────────────
echo "-- Bench targets --"
BODY=$(curl -sk -H "Authorization: Bearer $TOKEN" "$BASE/api/admin/bench")
check_contains "/api/admin/bench returns targets" '"targets"' "$BODY"
check_contains "/api/admin/bench lists the system config's port" '127.0.0.1:8444' "$BODY"

BODY=$(curl -sk -H "Authorization: Bearer $TOKEN" "$BASE/api/admin/backends")
check_contains "/api/admin/backends reports the system config's bind" '"bind":"127.0.0.1:8444"' "$BODY"

# ── /api/admin/logs ───────────────────────────────────────────────────────────
echo "-- Logs --"
BODY=$(curl -sk -H "Authorization: Bearer $TOKEN" "$BASE/api/admin/logs")
check_contains "/api/admin/logs returns lines" '"lines"' "$BODY"
check_contains "/api/admin/logs returns total_lines" '"total_lines"' "$BODY"

# ── /api/admin/routes ─────────────────────────────────────────────────────────
echo "-- Routes --"
BODY=$(curl -sk -H "Authorization: Bearer $TOKEN" "$BASE/api/admin/routes")
check_contains "/api/admin/routes returns routes"          '"routes"'          "$BODY"
check_contains "/api/admin/routes returns sample_requests" '"sample_requests"' "$BODY"

# ── /api/admin/config ─────────────────────────────────────────────────────────
echo "-- Config --"
BODY=$(curl -sk -H "Authorization: Bearer $TOKEN" "$BASE/api/admin/config")
check_contains "/api/admin/config returns config" '"config"' "$BODY"

# ── /api/admin/publish ────────────────────────────────────────────────────────
# One POST /genid to m6-http: it reloads every backend and flushes its cache.
echo "-- Publish --"
CODE=$(curl -sk -o /dev/null -w "%{http_code}" -X POST \
       -H "Authorization: Bearer $TOKEN" "$BASE/api/admin/publish")
check "POST /api/admin/publish returns 200" "200" "$CODE"

# ── /api/admin/cache/flush ────────────────────────────────────────────────────
echo "-- Cache flush --"
CODE=$(curl -sk -o /dev/null -w "%{http_code}" -X POST \
       -H "Authorization: Bearer $TOKEN" "$BASE/api/admin/cache/flush")
check "POST /api/admin/cache/flush returns 200" "200" "$CODE"

# ── /api/admin/services unknown ───────────────────────────────────────────────
echo "-- Service restart --"
CODE=$(curl -sk -o /dev/null -w "%{http_code}" -X POST \
       -H "Authorization: Bearer $TOKEN" "$BASE/api/admin/services/no-such/restart")
check "POST restart unknown service returns 404" "404" "$CODE"

echo ""
echo "=== Results: $PASS passed, $FAIL failed ==="
[ "$FAIL" -eq 0 ]
