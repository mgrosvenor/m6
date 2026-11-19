#!/usr/bin/env bash
# test.sh — the auth example end to end, for example 04.
#
# Run the same way as 05-cms/test.sh: the driver starts the example, runs this
# file and stops it. Nothing here starts or stops the site, and nothing here
# starts or stops any other process either.
#
#   ../m6-test-eg 4            the driver (start, this file, stop)
#   ./dev.sh                      one terminal
#   ./test.sh                     another
#
# Usage:
#   ./test.sh                     against https://127.0.0.1:8443
#   ./test.sh --addr HOST:PORT    against a different address
#
# ── What this example actually serves ────────────────────────────────────────
#
# Read from the code, not assumed. `site.toml` routes three paths to the
# `m6-auth` backend, and `m6-auth-server/src/main.rs` registers exactly four
# routes on it:
#
#   POST /auth/login    form body  -> 302 /next + Set-Cookie session, refresh
#                       JSON body  -> 200 {"access_token","refresh_token","expires_in"}
#   POST /auth/refresh  refresh cookie -> 302 + a new session cookie
#                       JSON body  -> 200 {"access_token","expires_in"}
#   POST /auth/logout   browser (no bearer) -> 302 / + clears both cookies,
#                       and revokes the refresh token in the database
#                       Authorization: Bearer -> 204
#   GET  /auth/public-key -> the public key PEM. NOT routed in site.toml, so
#                       through the front it is a 404, and this file does not
#                       pretend otherwise.
#
# `/members` is not the auth server. It is m6-html behind `require =
# "group:members"`, and m6-http sends the `session` cookie to the `[auth]`
# backend as one `POST /check` under a 50 ms deadline before it forwards. The
# token is not verified locally. Its refusals are m6-http's: no token with a
# browser Accept header is 302 /login?next=..., no token without one is 401 +
# WWW-Authenticate: Bearer, a token that does not verify is treated as no
# credential at all — 401 for an API client and the same /login redirect for a
# browser — and a verified token without the group is 403.
#
# The login throttle is real (`rate_limit.rs`: five failures per window per
# forwarded address, in memory, 900 s by default). It is checked last because
# it deliberately locks this address out.
#
# ── What this file does not assert, and why ─────────────────────────────────
#
# The 403 (a valid token with the wrong group) is not exercised: the example
# has one user, `admin`, and `dev.sh` puts it in `members`. Testing 403 needs a
# second user, which means writing to the example's database from a test that
# is supposed to observe it.
#
# The expired-credential path is not exercised either. `access_ttl` is 900 s;
# waiting it out is a fifteen-minute test, and minting an expired token needs
# the private key and an ES256 signer that bash does not have. A credential
# that does not verify at all is checked instead (a nonsense `session` value,
# refused with 401) — the same `Refusal::Invalid` branch, but not the same fact.
#
# Logout does not end the session: it clears the cookies and revokes the
# refresh token, and the already-issued access token stays valid for up to its
# remaining 15 minutes because it is stateless. This file does not assert that
# either way. It is a property of stateless access tokens rather than something
# `m6-auth` promises, and locking it in with a check would make a security
# improvement (revoking on logout) look like a regression.
#
# ── Why there is no SMTP relay here ─────────────────────────────────────────
#
# This example's stack also runs example 03's `render-contact` binary on its own
# socket and routes `/contact`,
# which is why the old copy of this file could pass 14 of 14 while the auth
# server was not running: it was testing the contact form. The contact form is
# example 03's subject and 03's suite covers it; 04's subject is the auth
# server, so the relay and its fixture are gone. That is also why nothing in
# this file starts a process or kills one: there is no fixture to own.
#
# ── Determinism ─────────────────────────────────────────────────────────────
#
# No `sleep`. Readiness is a bounded retry on the socket answering, and every
# check has one expected value. If the socket never answers, the run stops with
# the DEAD sentence below instead of scoring 0 of N, because a stack that is
# not there must read as a failure and not as a partial pass.
set -uo pipefail

SITE="$(cd "$(dirname "$0")" && pwd)"
ADDR="127.0.0.1:8443"
# `own.sh` is read-only here (own_port_holder) — it is what names the process
# holding the port when the stack is dead. Nothing in this file kills anything.
. "$SITE/../own.sh"

while [[ $# -gt 0 ]]; do
    case "$1" in
        --addr) ADDR="$2"; shift 2 ;;
        *) echo "Unknown flag: $1" >&2; exit 1 ;;
    esac
done

BASE="https://${ADDR}"
PORT="${ADDR##*:}"
CURL=(curl -sk --http1.1 -m 30)

PASS=0; FAIL=0
RED=$'\033[0;31m'; GREEN=$'\033[0;32m'; YELLOW=$'\033[1;33m'; RESET=$'\033[0m'

# ── Helpers ──────────────────────────────────────────────────────────────────

pass() { echo "  ${GREEN}PASS${RESET}  $1"; PASS=$(( PASS + 1 )); }
fail() { echo "  ${RED}FAIL${RESET}  $1"; FAIL=$(( FAIL + 1 )); }

# check <label> <got> <want>
check() {
    if [[ "$2" == "$3" ]]; then pass "$1 ($2)"; else fail "$1 — expected $3, got $2"; fi
}

section() { echo; echo "${YELLOW}── $1 ${RESET}"; }

# fetch <prefix> <curl args...> — leaves the status in $CODE, the header file
# in $HDRS and the body file in $BODY (a path, so read it, do not echo it).
fetch() {
    local p="$1"; shift
    CODE=$("${CURL[@]}" -D "$p.h" -o "$p.b" -w '%{http_code}' "$@" 2>/dev/null) || CODE="000"
    HDRS="$p.h"; BODY="$p.b"
}

# hdr <Name> — the first value of that response header, or empty.
hdr() {
    grep -i "^$1:" "$HDRS" 2>/dev/null | head -1 \
        | sed 's/^[^:]*:[[:space:]]*//' | tr -d '\r'
}

# set_cookie_line <name> — the Set-Cookie line for that cookie, or empty.
set_cookie_line() {
    grep -i '^set-cookie:' "$HDRS" 2>/dev/null | tr -d '\r' \
        | grep -i "^set-cookie: $1=" | head -1
}

# cookie_value <name> — the cookie's value (these JWTs carry no '='), or empty.
cookie_value() { set_cookie_line "$1" | cut -d= -f2 | cut -d';' -f1; }

# cookie_attrs <name> — the Set-Cookie line with the value removed, so the
# attributes can be compared exactly without the token in the diff.
cookie_attrs() {
    set_cookie_line "$1" | sed 's/^[^:]*: [^;=]*=[^;]*;//; s/^[[:space:]]*//'
}

# cookie_scope <name> — just the lifetime and the path, so the scope checks do
# not also fail when the HttpOnly check is the one that should fail.
cookie_scope() { cookie_attrs "$1" | grep -oE 'Path=[^;]*; Max-Age=[0-9]+'; }

login_form() {  # login_form <user> <pass> [next]
    local args=(--data-urlencode "username=$1" --data-urlencode "password=$2")
    [[ -n "${3-}" ]] && args+=(--data-urlencode "next=$3")
    fetch "$WORK/login-f" -X POST "$BASE/auth/login" \
        -H 'Content-Type: application/x-www-form-urlencoded' "${args[@]}"
}

WORK="$(mktemp -d)"
# The temp directory is the only thing this run creates. own_forget removes a
# pid file an earlier version of this suite left behind when it was killed.
own_init "$SITE" test
trap 'rm -rf "$WORK"; own_forget' EXIT
own_reap

# ── The stack answers ────────────────────────────────────────────────────────
#
# Bounded, not timed: curl retries the connection twenty times, a second apart,
# and only a connection that never establishes ends as 000. The pacing is
# curl's `--retry-delay`, so there is no sleep in this file, and twenty attempts
# cover the ten to thirteen seconds a debug `dev.sh` takes to bind the port. A
# stack that is merely still booting is waited for; a stack that is not there is
# declared dead after the bound rather than waited on indefinitely.

code=$("${CURL[@]}" --retry 20 --retry-delay 1 --retry-connrefused --retry-max-time 30 \
    -o /dev/null -w '%{http_code}' "$BASE/login" 2>/dev/null) || code="000"

if [[ "$code" == "000" ]]; then
    echo
    echo "${RED}FATAL: the stack is DEAD — every request returned 000.${RESET}"
    echo "  Nothing answered at $BASE/login after 20 attempts a second apart, so every check in"
    echo "  this file would have measured a missing server. That is a failure,"
    echo "  not a partial pass."
    holder=$(own_port_holder "$PORT")
    if [[ -n "$holder" ]]; then
        echo "  Port $PORT is held by pid $holder, which this run did not start."
    else
        echo "  No process is listening on port $PORT."
    fi
    echo "  Start it with ./dev.sh, or run the driver: ../m6-test-eg 4"
    exit 1
fi

section "1. The login page is served"

check "the stack answers GET /login" "$code" "200"
fetch "$WORK/login-page" "$BASE/login"
if grep -Eq 'action="?/auth/login"?' "$BODY" 2>/dev/null; then
    pass "GET /login renders the form posting to /auth/login"
else
    fail "GET /login renders the form posting to /auth/login — no such action in the page"
fi

# ── Nothing is served without a credential ───────────────────────────────────

section "2. A member page without a credential"

fetch "$WORK/m-browser" -H 'Accept: text/html' "$BASE/members"
check "GET /members with no cookie, from a browser" "$CODE" "302"
check "the browser is sent to the login page" "$(hdr Location)" "/login?next=/members"

fetch "$WORK/m-api" "$BASE/members"
check "GET /members with no cookie, from an API client" "$CODE" "401"
check "the refusal tells an API client to authenticate" "$(hdr WWW-Authenticate)" "Bearer"

# A token that does not verify is not a credential that happens to be wrong: at
# this tip `auth::check` maps a forged or expired token to `NoCredential`
# (`m6-http/src/auth.rs`, "An invalid, expired or forged token is no credential
# at all"), so a browser gets the same redirect as one that sent nothing. The
# security fact is the one below — an API client is refused rather than served
# — and that is what is asserted. The redirect for a browser is representation,
# not a separate decision, so it is not scored twice.
fetch "$WORK/m-bogus" -H 'Cookie: session=not-a-token' "$BASE/members"
check "GET /members with a session cookie that does not verify" "$CODE" "401"

# ── A session begins ─────────────────────────────────────────────────────────

section "3. A correct credential starts a session"

login_form admin admin /members
check "POST /auth/login with admin/admin" "$CODE" "302"
check "the login sends the browser on to the page it asked for" "$(hdr Location)" "/members"

LOGIN_SESSION=$(cookie_value session)
LOGIN_REFRESH=$(cookie_value refresh)

check "the session cookie is scoped to the whole site for 900 s" \
    "$(cookie_scope session)" "Path=/; Max-Age=900"
check "the refresh cookie is scoped to /auth/refresh for 30 days" \
    "$(cookie_scope refresh)" "Path=/auth/refresh; Max-Age=2592000"

# ── The one secret that must not leak ────────────────────────────────────────
#
# The access and refresh tokens are credentials. They travel in HttpOnly
# cookies so page script cannot read them, and they must not also appear in the
# response body or in the Location header, where they would land in browser
# history, referrers and access logs.
if [[ "$(cookie_attrs session)" == *HttpOnly* && "$(cookie_attrs refresh)" == *HttpOnly* \
   && "$(cookie_attrs session)" == *Secure* && "$(cookie_attrs refresh)" == *Secure* ]]; then
    pass "both token cookies are HttpOnly and Secure, so script cannot read them"
else
    fail "both token cookies are HttpOnly and Secure, so script cannot read them — script can read a token"
fi

if [[ -n "$LOGIN_SESSION" && -n "$LOGIN_REFRESH" ]] \
   && ! grep -q 'eyJ' "$BODY" 2>/dev/null \
   && [[ "$(hdr Location)" != *session=* && "$(hdr Location)" != *refresh=* ]]; then
    pass "the tokens are in the cookies only — not in the body or the Location"
else
    fail "the tokens are in the cookies only — not in the body or the Location"
fi

fetch "$WORK/m-session" -H 'Accept: text/html' -H "Cookie: session=$LOGIN_SESSION" "$BASE/members"
check "GET /members with the session cookie" "$CODE" "200"
if grep -qF 'Members Area' "$BODY" 2>/dev/null; then
    pass "the members page is the one that was rendered"
else
    fail "the members page is the one that was rendered — no 'Members Area' in the body"
fi

# ── A wrong credential ───────────────────────────────────────────────────────

section "4. A wrong credential is refused and says nothing useful"

login_form admin wrong-password /members
check "POST /auth/login with the wrong password" "$CODE" "302"
check "the failed login returns to the login page" "$(hdr Location)" \
    "/login?error=invalid&next=/members"
check "the failed login sets no cookie" "$(set_cookie_line session)$(set_cookie_line refresh)" ""

WRONG_LOCATION=$(hdr Location)
login_form nobody wrong-password /members
check "POST /auth/login with a user that does not exist" "$CODE" "302"
# Same status, same Location, same absence of a cookie: the response cannot be
# used to learn which usernames exist.
check "an unknown user is indistinguishable from a wrong password" \
    "$(hdr Location)|$(set_cookie_line session)" "$WRONG_LOCATION|"

# ── A refresh ────────────────────────────────────────────────────────────────

section "5. The refresh cookie mints a new session"

fetch "$WORK/refresh" -X POST "$BASE/auth/refresh" \
    -H "Cookie: refresh=$LOGIN_REFRESH" -H 'Referer: /members'
check "POST /auth/refresh with the refresh cookie" "$CODE" "302"
check "the refresh returns the browser to the page it came from" "$(hdr Location)" "/members"
REFRESHED_SESSION=$(cookie_value session)
check "the refresh sets a session cookie" \
    "$([[ -n "$REFRESHED_SESSION" ]] && echo yes || echo no)" "yes"

fetch "$WORK/m-refreshed" -H 'Accept: text/html' -H "Cookie: session=$REFRESHED_SESSION" "$BASE/members"
check "the refreshed session opens /members" "$CODE" "200"

fetch "$WORK/refresh-none" -X POST "$BASE/auth/refresh" -H 'Accept: text/html'
check "POST /auth/refresh with no refresh cookie" "$CODE" "302"
check "a refresh with no cookie is sent to the login page" "$(hdr Location)" "/login"

# ── Logout ───────────────────────────────────────────────────────────────────

section "6. Logout revokes the refresh token"

fetch "$WORK/logout" -X POST "$BASE/auth/logout" \
    -H "Cookie: session=$LOGIN_SESSION; refresh=$LOGIN_REFRESH"
check "POST /auth/logout" "$CODE" "302"
check "logout sends the browser home" "$(hdr Location)" "/"
check "logout clears the session cookie" "$(cookie_scope session)" "Path=/; Max-Age=0"
check "logout clears the refresh cookie" "$(cookie_scope refresh)" \
    "Path=/auth/refresh; Max-Age=0"

# The cookie is gone from the browser, but the token is still in the database's
# revocation table — a copy taken before logout must stop working. This is the
# check the old contact suite could never make: it is a fact about the auth
# server's database, not about the front's cache.
fetch "$WORK/refresh-after-logout" -X POST "$BASE/auth/refresh" \
    -H "Cookie: refresh=$LOGIN_REFRESH" -H 'Accept: text/html'
check "POST /auth/refresh with the logged-out refresh token" "$CODE" "302"
check "the revoked refresh token is sent to the login page" "$(hdr Location)" "/login"

# ── The JSON API ─────────────────────────────────────────────────────────────

section "7. The same routes answer JSON for an API client"

fetch "$WORK/login-json" -X POST "$BASE/auth/login" \
    -H 'Content-Type: application/json' -d '{"username":"admin","password":"admin"}'
check "POST /auth/login with a JSON body" "$CODE" "200"
if python3 -c 'import json,sys; d=json.load(open(sys.argv[1])); sys.exit(0 if sorted(d)==["access_token","expires_in","refresh_token"] and d["expires_in"]==900 else 1)' "$BODY" 2>/dev/null; then
    pass "the JSON login returns the two tokens and expires_in 900"
else
    fail "the JSON login returns the two tokens and expires_in 900"
fi

API_REFRESH=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["refresh_token"])' "$BODY" 2>/dev/null || true)
fetch "$WORK/refresh-json" -X POST "$BASE/auth/refresh" \
    -H 'Content-Type: application/json' -d "{\"refresh_token\":\"$API_REFRESH\"}"
check "POST /auth/refresh with a JSON body" "$CODE" "200"
if python3 -c 'import json,sys; d=json.load(open(sys.argv[1])); sys.exit(0 if d.get("access_token") and d.get("expires_in")==900 else 1)' "$BODY" 2>/dev/null; then
    pass "the JSON refresh returns a new access token"
else
    fail "the JSON refresh returns a new access token"
fi

fetch "$WORK/refresh-json-bogus" -X POST "$BASE/auth/refresh" \
    -H 'Content-Type: application/json' -d '{"refresh_token":"not-a-token"}'
check "POST /auth/refresh with a JSON body that does not verify" "$CODE" "401"

# ── The login throttle ───────────────────────────────────────────────────────
#
# Five failures per window per address, in memory. Every earlier
# section logged in successfully, and a correct password clears the count, so
# this section starts from zero — and it must, or the counts below would be
# measuring the sections above instead of the throttle.
# It is last because it locks this address out for the rest of the run.

section "8. The login throttle"

# The window `Retry-After` counts down. `configs/m6-auth.conf` sets no
# `[rate_limit]`, so this is the server's default of 900 s.
window=$(sed -n 's/^[[:space:]]*window_secs[[:space:]]*=[[:space:]]*\([0-9]*\).*/\1/p' \
    "$SITE/configs/m6-auth.conf" | tail -1)
[[ -n "$window" ]] || window=900

login_form admin admin
check "a correct password succeeds and clears the failure count" "$CODE" "302"

throttle_started=$SECONDS
throttled=0
for _ in 1 2 3 4 5; do
    login_form admin wrong-password
    [[ "$CODE" == "302" ]] && throttled=$(( throttled + 1 ))
done
check "five wrong passwords are refused one at a time (302 each)" "$throttled" "5"

login_form admin wrong-password
check "the sixth wrong password is throttled" "$CODE" "429"

# `Retry-After` is the whole seconds left in the window, rounded up, so it is
# never below the true wait and never above the window. The window opened at
# the first failure, which happened after `throttle_started`, so the remaining
# time is at least `window - elapsed`. The old hard-coded 60 is below that
# bound on any host whose burst fits inside the window, which is the point.
elapsed=$(( SECONDS - throttle_started ))
low=$(( window - elapsed ))
[[ "$low" -lt 1 ]] && low=1
retry=$(hdr Retry-After)
if [[ "$retry" =~ ^[0-9]+$ ]] && (( retry >= low && retry <= window )); then
    pass "the throttle says how long to wait ($retry s of a ${window} s window)"
else
    fail "the throttle says how long to wait — expected $low..$window, got '$retry'"
fi

# ── Summary ──────────────────────────────────────────────────────────────────

echo
echo "═══════════════════════════════════════════════════════"
printf '  %s%s passed%s  %s%s failed%s  (%d checks)\n' \
    "$GREEN" "$PASS" "$RESET" "$RED" "$FAIL" "$RESET" "$(( PASS + FAIL ))"
echo "═══════════════════════════════════════════════════════"

[[ $FAIL -eq 0 ]] || exit 1
