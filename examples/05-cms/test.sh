#!/usr/bin/env bash
# test.sh — end-to-end test for example 05, the CMS blog.
#
# This is the example that runs the whole stack: m6-http at the edge, m6-html for
# templates, m6-file for assets, m6-md for markdown, m6-auth-server for logins,
# and render-cms as a custom renderer. So this is the suite that checks the
# system rather than one service, and it is the one to run before believing that
# a change to m6 did no harm.
#
# Start the stack first:  ./dev.sh
#
# Usage:
#   ./test.sh                      against https://127.0.0.1:8443
#   ./test.sh --addr HOST:PORT     against a different address
#   ./test.sh --user U --pass P    different credentials (default admin/admin)
#
# ── There are no skips ───────────────────────────────────────────────────────
#
# Every check has exactly one expected answer. A check that accepts a range
# ("302 or 401 or 403") reports a state it never established, and a check that
# cannot fail is not a check. If something here cannot be established, it fails
# and says so. Nothing relies on GNU-only options such as `grep -P`, so the
# suite runs the same on macOS and Linux.
#
# ── What is deliberately not covered ─────────────────────────────────────────
#
#   - HTTP/3, because the system curl has no h3. m6's own conformance suite
#     covers h3 against h3spec.
#   - Real SMTP. The contact form's POST path is exercised; delivery is not.
#   - TLS certificate validation: -k throughout, because these are mkcert certs.
set -uo pipefail

SITE="$(cd "$(dirname "$0")" && pwd)"

ADDR="127.0.0.1:8443"
CMS_USER="admin"
CMS_PASS="admin"

while [[ $# -gt 0 ]]; do
    case "$1" in
        --addr) ADDR="$2"; shift 2 ;;
        --user) CMS_USER="$2"; shift 2 ;;
        --pass) CMS_PASS="$2"; shift 2 ;;
        *) echo "Unknown flag: $1" >&2; exit 1 ;;
    esac
done

BASE="https://${ADDR}"
CURL=(curl -sk --http1.1)

# Kept out of the site tree so a stray file cannot be served as content.
WORK="$(mktemp -d)"
JAR="$WORK/cookies.txt"
trap 'rm -rf "$WORK"' EXIT

PASS=0; FAIL=0
RED=$'\033[0;31m'; GREEN=$'\033[0;32m'; YELLOW=$'\033[1;33m'; RESET=$'\033[0m'

# ── Helpers ──────────────────────────────────────────────────────────────────

pass() { echo "  ${GREEN}PASS${RESET}  $1"; PASS=$(( PASS + 1 )); }
fail() { echo "  ${RED}FAIL${RESET}  $1"; FAIL=$(( FAIL + 1 )); }

# check <label> <got> <want>
check() {
    if [[ "$2" == "$3" ]]; then pass "$1 ($2)"; else fail "$1 — expected $3, got $2"; fi
}

# check_contains <label> <haystack> <needle>
check_contains() {
    if printf '%s' "$2" | grep -qF -- "$3"; then
        pass "$1"
    else
        fail "$1 — no '$3' in the response"
    fi
}

# check_absent <label> <haystack> <needle>
check_absent() {
    if printf '%s' "$2" | grep -qF -- "$3"; then
        fail "$1 — '$3' is present and should not be"
    else
        pass "$1"
    fi
}

# check_file_contains <label> <path> <needle>
#
# Separate from check_contains because that one holds the haystack in a shell
# variable and pipes it to grep. For a file the size of data/posts.json that
# wastes the copy and, worse, grep exits on the first match while printf is still
# writing, so printf reports a broken pipe and the check reads as a failure on a
# passing system.
check_file_contains() {
    if grep -qF -- "$3" "$2" 2>/dev/null; then
        pass "$1"
    else
        fail "$1 — no '$3' in $2"
    fi
}

# check_file_absent <label> <path> <needle>
check_file_absent() {
    if grep -qF -- "$3" "$2" 2>/dev/null; then
        fail "$1 — '$3' is present in $2 and should not be"
    else
        pass "$1"
    fi
}

# check_nonempty <label> <value>
check_nonempty() {
    if [[ -n "$2" ]]; then pass "$1 ($2)"; else fail "$1 — absent"; fi
}

# wait_for <label> <max-seconds> <snippet>
#
# Polls until the snippet exits 0, then passes and says how long it took; fails
# when the budget is gone. Replaces `sleep 2`.
#
# Publishing writes a JSON file, rebuilds the index and sends `POST /genid`, and
# m6-http and m6-html then reload. How long that takes depends on the machine.
# A fixed sleep encodes one machine's answer: 2 seconds was enough on a laptop
# and not on m6's build host under load, where this suite reported that a
# published post was neither listed nor served -- a real-looking defect that was
# nothing but the test not waiting. Polling states the actual requirement, which
# is that it happens, and keeps a generous ceiling so a genuine failure to
# publish still fails.
wait_for() {
    local label="$1" max="$2" snippet="$3"
    local tries=$(( max * 2 )) i=0
    while [ "$i" -lt "$tries" ]; do
        if eval "$snippet" >/dev/null 2>&1; then
            pass "$label (after $(( i * 5 / 10 )).$(( i * 5 % 10 ))s)"
            return 0
        fi
        i=$(( i + 1 ))
        sleep 0.5
    done
    fail "$label — still not true after ${max}s"
    return 1
}

code()  { "${CURL[@]}" -o /dev/null -w '%{http_code}' "$@"; }
body()  { "${CURL[@]}" "$@"; }
# Response headers with the header NAME lowercased and CRs stripped, because
# names are case-insensitive (RFC 9110 5.1) and h1 and h2 do not agree on the
# case they send. Values are left exactly as they arrived: lowercasing those too
# turned `Allow: GET, HEAD, POST` into lowercase and quietly broke every
# assertion made against a header's value.
heads() {
    "${CURL[@]}" -D - -o /dev/null "$@" | tr -d '\r' | awk '
        { i = index($0, ":")
          if (i > 0) print tolower(substr($0, 1, i - 1)) substr($0, i)
          else       print }'
}
# header_value <name> <headers>
header_value() { printf '%s\n' "$2" | grep "^$1:" | head -1 | sed "s/^$1:[[:space:]]*//"; }

authed()      { "${CURL[@]}" -b "$JAR" "$@"; }
authed_code() { "${CURL[@]}" -b "$JAR" -o /dev/null -w '%{http_code}' "$@"; }

section() { echo; echo "${YELLOW}── $1 ${RESET}"; }

json_field() {  # json_field <field> <<< json   (on stdin)
    python3 -c "import sys,json
try: print(json.load(sys.stdin).get('$1',''))
except Exception: print('')"
}

# ── 0. The stack is up ───────────────────────────────────────────────────────

section "0. The stack is up"

c=$(code "$BASE/")
check "GET / reachable" "$c" "200"
if [[ "$c" != "200" ]]; then
    echo
    echo "${RED}The stack is not answering. Start it with ./dev.sh and try again.${RESET}" >&2
    echo "  Logs: $SITE/logs/" >&2
    exit 1
fi

# ── 1. Both wire protocols ───────────────────────────────────────────────────
#
# The rest of the suite pins --http1.1; this section exercises both protocols.

section "1. HTTP/1.1 and HTTP/2"

read -r h1code h1ver < <(curl -sk --http1.1 -o /dev/null -w '%{http_code} %{http_version}' "$BASE/")
check "HTTP/1.1 GET / status"  "$h1code" "200"
check "HTTP/1.1 negotiated"    "$h1ver"  "1.1"

read -r h2code h2ver < <(curl -sk --http2 -o /dev/null -w '%{http_code} %{http_version}' "$BASE/")
check "HTTP/2 GET / status"    "$h2code" "200"
check "HTTP/2 negotiated"      "$h2ver"  "2"

# Same bytes over both, or one of them is serving something else.
h1len=$(curl -sk --http1.1 -o /dev/null -w '%{size_download}' "$BASE/")
h2len=$(curl -sk --http2   -o /dev/null -w '%{size_download}' "$BASE/")
check "HTTP/2 serves the same body length as HTTP/1.1" "$h2len" "$h1len"

# ── 2. Public pages ──────────────────────────────────────────────────────────

section "2. Public pages"

for p in / /about /blog /blog/quick-start /blog/architecture; do
    check "GET $p" "$(code "$BASE$p")" "200"
done

b=$(body "$BASE/")
check_contains "GET / names the site"        "$b" "m6"
check_contains "GET / lists recent posts"    "$b" "Recent Posts"

check_contains "GET /blog lists a post"      "$(body "$BASE/blog")" "quick-start"
check_contains "GET /blog/quick-start renders its title" \
    "$(body "$BASE/blog/quick-start")" "Quick Start"

# An unknown post is m6-html's not-found page: 200 with the site's chrome, not a
# bare 404, because the template renders it. An unknown *path* is a real 404.
nf=$(body "$BASE/blog/no-such-post-here")
check "GET /blog/<unknown> renders the not-found page" \
    "$(code "$BASE/blog/no-such-post-here")" "200"
check_contains "GET /blog/<unknown> says not found" "$nf" "not found"
check "GET an unrouted path is a real 404" \
    "$(code "$BASE/this-path-does-not-exist")" "404"

# ── 3. Static assets, through m6-file ────────────────────────────────────────
#
# Each asset route has to name m6-file's `files` handler. A config that does not
# is refused at start, so the service never binds and every asset answers 502.
# The checks below tell 502 from 200.

section "3. Static assets"

check "GET /assets/style.css" "$(code "$BASE/assets/style.css")" "200"
ct=$(header_value 'content-type' "$(heads "$BASE/assets/style.css")")
check_contains "style.css is served as CSS" "$ct" "text/css"
check "GET /assets/easymde.min.js" "$(code "$BASE/assets/easymde.min.js")" "200"
check "a missing asset is 404" "$(code "$BASE/assets/no-such-file.css")" "404"

# Nested paths: the route is `/assets/{*relpath}`, the explicit wildcard. A bare
# `{relpath}` matches one segment only, which serves the top of the tree and
# 404s everything under it -- the failure is invisible on a flat assets dir.
check "a nested asset, two levels down"  "$(code "$BASE/assets/vditor/dist/index.css")" "200"
check "a nested asset, four levels down" "$(code "$BASE/assets/vditor/dist/js/i18n/en_US.js")" "200"

# Conditional requests: the cache is useless if revalidation does not work.
etag=$(header_value 'etag' "$(heads "$BASE/assets/style.css")")
if [[ -n "$etag" ]]; then
    pass "style.css carries an ETag ($etag)"
    check "If-None-Match on the same ETag is 304" \
        "$(code -H "If-None-Match: $etag" "$BASE/assets/style.css")" "304"
    check "If-None-Match on a stale ETag is 200" \
        "$(code -H 'If-None-Match: "0000000-0000"' "$BASE/assets/style.css")" "200"
else
    fail "style.css carries an ETag — none present, so revalidation cannot work"
fi

# ── 4. Compression ───────────────────────────────────────────────────────────
#
# m6-http is a cache, not a transformer: compression is the backend's job and the
# edge only promises what the backend can deliver. `Vary: Accept-Encoding` is the
# part that matters for a shared cache -- without it one client's gzip response
# is served to a client that cannot read it.

section "4. Compression"

hz=$(heads -H 'Accept-Encoding: gzip' "$BASE/")
check_contains "gzip requested, gzip returned"   "$(header_value 'content-encoding' "$hz")" "gzip"
check_contains "the response varies on Accept-Encoding" "$(header_value 'vary' "$hz")" "Accept-Encoding"

hi=$(heads -H 'Accept-Encoding: identity' "$BASE/")
check_absent "identity requested, nothing encoded" "$(header_value 'content-encoding' "$hi")" "gzip"

# ── 5. The cache actually caches ─────────────────────────────────────────────
#
# m6-http exposes no x-cache header, so this reads `Age`, which RFC 9111 7.1
# defines as the time since the response was generated. A second request that
# comes back with a non-zero Age and the same ETag was served from the store and
# not fetched again.

section "5. The cache"

e1=$(header_value 'etag' "$(heads "$BASE/blog")")
sleep 2
h2h=$(heads "$BASE/blog")
e2=$(header_value 'etag' "$h2h")
age=$(header_value 'age' "$h2h")
check "the ETag is stable across requests" "$e2" "$e1"
if [[ -n "$age" ]] && [[ "$age" -ge 1 ]]; then
    pass "the second request was served from the cache (Age: $age)"
else
    fail "the second request has Age '${age:-<absent>}', so nothing was cached"
fi

# ── 6. Security headers at the edge ──────────────────────────────────────────

section "6. Security headers"

hh=$(heads "$BASE/")
check_contains "HSTS is set"                  "$(header_value 'strict-transport-security' "$hh")" "max-age="
check_contains "a Content-Security-Policy is set" "$(header_value 'content-security-policy' "$hh")" "default-src"
check_contains "sniffing is disabled"         "$(header_value 'x-content-type-options' "$hh")" "nosniff"

# ── 7. Request methods ───────────────────────────────────────────────────────
#
# m6-http validates the method before routing, from `[server] allowed_methods`.
# This example declares GET, HEAD, POST, PATCH and DELETE because render-cms
# registers PATCH and DELETE routes. Until that line existed, every PATCH was
# 405 at the edge while the handler behind it was registered and working.
#
# 405 and 501 are not interchangeable (RFC 9110 15.5.6 and 15.6.2): 405 says the
# server knows the method, 501 says it does not. Answering 405 to an invented
# verb claims knowledge the server does not have.

section "7. Request methods"

allow=$(header_value 'allow' "$(heads -X PUT "$BASE/")")
check "an undeclared method is 405" "$(code -X PUT "$BASE/")" "405"
check_contains "405 carries Allow, listing GET"    "$allow" "GET"
check_contains "405 carries Allow, listing PATCH"  "$allow" "PATCH"
check_contains "405 carries Allow, listing DELETE" "$allow" "DELETE"
check_absent   "Allow does not list the refused method" "$allow" "PUT"
check "an invented verb is 501, not 405" "$(code -X FOO "$BASE/")" "501"

# ── 8. Contact form ──────────────────────────────────────────────────────────

section "8. Contact form"

check "GET /contact" "$(code "$BASE/contact")" "200"
cb=$(body "$BASE/contact")
check_contains "the contact page has a form"  "$cb" "<form"
check_contains "the form has a message field" "$cb" "message"

pb=$(body "$BASE/contact" -X POST \
    -H 'Content-Type: application/x-www-form-urlencoded' \
    -d 'name=Test+Sender&email=sender%40example.com&message=Sent+by+test.sh')
check "POST /contact" "$(code "$BASE/contact" -X POST \
    -H 'Content-Type: application/x-www-form-urlencoded' \
    -d 'name=Test+Sender&email=sender%40example.com&message=Sent+by+test.sh')" "200"
check_contains "the confirmation names the sender" "$pb" "Test Sender"

# ── 9. Login page ────────────────────────────────────────────────────────────

section "9. Login page"

check "GET /login" "$(code "$BASE/login")" "200"
lb=$(body "$BASE/login")
check_contains "the login page has a form"           "$lb" "<form"
check_contains "the login page has a password field" "$lb" "password"

# ── 10. Authentication ───────────────────────────────────────────────────────

section "10. Authentication"

# A wrong password is refused. The form path redirects back to /login with an
# error rather than returning 401, so that redirect is the assertion.
wrong=$(heads -X POST "$BASE/auth/login" \
    -H 'Content-Type: application/x-www-form-urlencoded' \
    -d "username=${CMS_USER}&password=definitely-not-the-password")
check_contains "a wrong password redirects to /login with an error" \
    "$(header_value 'location' "$wrong")" "/login?error=invalid"
check_absent "a wrong password sets no session cookie" \
    "$(printf '%s\n' "$wrong" | grep '^set-cookie:' | grep 'session=' || true)" "session="

# The real login. -c writes the jar, so the cookies are kept for what follows.
rm -f "$JAR"
lh=$(curl -sk --http1.1 -c "$JAR" -D - -o /dev/null -X POST "$BASE/auth/login" \
    -H 'Content-Type: application/x-www-form-urlencoded' \
    -d "username=${CMS_USER}&password=${CMS_PASS}" | tr -d '\r')
check "POST /auth/login redirects on success" \
    "$(printf '%s\n' "$lh" | grep -c '^HTTP/.* 302')" "1"

sc=$(printf '%s\n' "$lh" | grep -i '^set-cookie:' | grep 'session=')
check_contains "login sets a session cookie"   "$sc" "session="
check_contains "the session cookie is HttpOnly" "$sc" "HttpOnly"
check_contains "the session cookie is Secure"   "$sc" "Secure"
check_contains "the session cookie is SameSite" "$sc" "SameSite"

rc=$(printf '%s\n' "$lh" | grep -i '^set-cookie:' | grep 'refresh=')
check_contains "login sets a refresh cookie"    "$rc" "refresh="
check_contains "the refresh cookie is scoped to /auth/refresh" "$rc" "Path=/auth/refresh"

check "POST /auth/refresh with the cookie" \
    "$(authed_code -X POST "$BASE/auth/refresh")" "302"

# ── 11. Protected pages ──────────────────────────────────────────────────────

section "11. Protected pages"

check "GET /cms with no credentials is refused" "$(code "$BASE/cms")" "401"
check "GET /cms/new with no credentials is refused" "$(code "$BASE/cms/new")" "401"
check "POST /api/drafts with no credentials is refused" \
    "$(code -X POST "$BASE/api/drafts" -H 'Content-Type: application/json' -d '{}')" "401"

check "GET /cms as an editor" "$(authed_code "$BASE/cms")" "200"
check_contains "the dashboard mentions drafts" "$(authed "$BASE/cms")" "draft"
check "GET /cms/new as an editor" "$(authed_code "$BASE/cms/new")" "200"
check_contains "the editor has a title field" "$(authed "$BASE/cms/new")" "title"

# The CMS must never be cached at the edge: it is per-user and it is behind auth.
cmsh=$(authed -D - -o /dev/null "$BASE/cms" | tr -d '\r' | tr 'A-Z' 'a-z')
cc=$(header_value 'cache-control' "$cmsh")
if printf '%s' "$cc" | grep -qE 'no-store|private|no-cache'; then
    pass "/cms is not cacheable by a shared cache ($cc)"
else
    fail "/cms Cache-Control is '${cc:-<absent>}', which lets a shared cache keep it"
fi

# ── 12. The publish lifecycle ────────────────────────────────────────────────
#
# Every step is checked against what a visitor sees, not against what the API
# says it did: the reply "unpublished" proves nothing about the public site.
# The post must leave /blog and stop serving its body.

section "12. Draft, publish, unpublish"

MARKER="e2e-marker-$$"
created=$(authed -X POST "$BASE/api/drafts" -H 'Content-Type: application/json' \
    -d "{\"title\":\"E2E Lifecycle $$\",\"body\":\"$MARKER\",\"summary\":\"created by test.sh\",\"tags\":[\"test\"],\"date\":\"2026-09-14\"}" \
    -w '\n%{http_code}')
ccode="${created##*$'\n'}"
cbody="${created%$'\n'${ccode}}"
check "POST /api/drafts creates a draft" "$ccode" "201"

STEM=$(printf '%s' "$cbody" | json_field stem)
if [[ -z "$STEM" ]]; then
    fail "the create response carries a stem — got: $cbody"
    STEM="e2e-lifecycle-$$"      # keep going so the rest still reports
else
    pass "the create response carries a stem ($STEM)"
fi

check "GET /cms/edit/<stem>" "$(authed_code "$BASE/cms/edit/$STEM")" "200"

# A draft is not public.
check_absent "an unpublished draft is not readable at its blog URL" \
    "$(body "$BASE/blog/$STEM")" "$MARKER"
check_absent "an unpublished draft is not listed on /blog" \
    "$(body "$BASE/blog")" "$STEM"

check "PATCH /api/drafts/<stem> updates it" \
    "$(authed_code -X PATCH "$BASE/api/drafts/$STEM" \
        -H 'Content-Type: application/json' -d '{"summary":"updated by test.sh"}')" "200"

pubresp=$(authed -X POST "$BASE/api/publish/$STEM" -w '\n%{http_code}')
pubcode="${pubresp##*$'\n'}"
check "POST /api/publish/<stem>" "$pubcode" "200"

# Publishing sends `POST /genid`, which reloads the backends and empties the
# cache. The answer waits for it, and the poll below still states the
# requirement rather than a duration.
wait_for "the published post reaches /blog" 30 \
    "$(printf '%s' "${CURL[*]}") '$BASE/blog' | grep -qF '$STEM'"

check "the published post is readable" "$(code "$BASE/blog/$STEM")" "200"
check_contains "the published post serves its body" "$(body "$BASE/blog/$STEM")" "$MARKER"
check_contains "the published post is listed on /blog" "$(body "$BASE/blog")" "$STEM"

unpubresp=$(authed -X POST "$BASE/api/unpublish/$STEM" -w '\n%{http_code}')
unpubcode="${unpubresp##*$'\n'}"
check "POST /api/unpublish/<stem>" "$unpubcode" "200"
wait_for "the unpublished post leaves /blog" 30 \
    "! $(printf '%s' "${CURL[*]}") '$BASE/blog' | grep -qF '$STEM'"

# The checks that matter: the public site, not the API reply.
check_absent "an unpublished post is no longer listed on /blog" \
    "$(body "$BASE/blog")" "$STEM"
check_absent "an unpublished post no longer serves its body" \
    "$(body "$BASE/blog/$STEM")" "$MARKER"
check_file_absent "an unpublished post is out of the index" \
    "$SITE/data/posts.json" "\"$STEM\""

# Unpublishing returns the post to the drafts folder rather than destroying it.
if [[ -f "$SITE/content/drafts/$STEM.json" ]]; then
    pass "unpublishing keeps the draft for re-editing"
else
    fail "unpublishing lost the draft: no content/drafts/$STEM.json"
fi

# Posts that came from markdown must survive every rebuild of the index.
check_file_contains "markdown-sourced posts survive the rebuild" \
    "$SITE/data/posts.json" "quick-start"

check "DELETE /api/drafts/<stem>" "$(authed_code -X DELETE "$BASE/api/drafts/$STEM")" "200"

# Nothing of the test's own may be left in the site tree.
left=0
[[ -f "$SITE/content/drafts/$STEM.json" ]] && left=1
[[ -f "$SITE/content/posts/$STEM.json"  ]] && left=1
check "the test leaves no content behind" "$left" "0"

# ── 13. Logout ───────────────────────────────────────────────────────────────

section "13. Logout"

# -b and -c together, so the jar is updated by the response: the cookie-clearing
# Set-Cookie headers must replace the stale token, or it keeps being sent.
loh=$(curl -sk --http1.1 -b "$JAR" -c "$JAR" -D - -o /dev/null \
    -X POST "$BASE/auth/logout" | tr -d '\r')
check "POST /auth/logout redirects" "$(printf '%s\n' "$loh" | grep -c '^HTTP/.* 302')" "1"

cleared=$(printf '%s\n' "$loh" | grep -i '^set-cookie:' | grep 'session=')
check_contains "logout clears the session cookie" "$cleared" "Max-Age=0"
clearedr=$(printf '%s\n' "$loh" | grep -i '^set-cookie:' | grep 'refresh=')
check_contains "logout clears the refresh cookie" "$clearedr" "Max-Age=0"

check "after logout the CMS is refused" "$(authed_code "$BASE/cms")" "401"

# ── 14. The login throttle ───────────────────────────────────────────────────
#
# m6-auth-server allows a number of FAILED logins per IP per window. Only
# failures count, and a correct password clears the count, so this suite can log
# in many times without locking itself out. The limits come from the config.
#
# This example's m6-auth.conf sets a short window on purpose, so the recovery can
# be measured rather than waited out. Production leaves the section out.

section "14. The login throttle"

max=$(sed -n 's/^[[:space:]]*max_attempts[[:space:]]*=[[:space:]]*\([0-9]*\).*/\1/p' \
    "$SITE/configs/m6-auth.conf" | tail -1)
window=$(sed -n 's/^[[:space:]]*window_secs[[:space:]]*=[[:space:]]*\([0-9]*\).*/\1/p' \
    "$SITE/configs/m6-auth.conf" | tail -1)

if [[ -z "$max" || -z "$window" ]]; then
    fail "configs/m6-auth.conf sets [rate_limit] max_attempts and window_secs — one or both missing, so the throttle cannot be measured"
elif [[ "$window" -gt 30 ]]; then
    fail "configs/m6-auth.conf has window_secs=$window; this is the development config and needs a short window so recovery can be measured"
else
    pass "the throttle is configured for testing (max_attempts=$max, window_secs=${window}s)"

    # Successes must not consume the budget. More of them than the whole budget,
    # so this fails outright if counting ever comes back.
    ok=0
    for _ in $(seq 1 $(( max + 3 ))); do
        [[ "$(code -X POST "$BASE/auth/login" \
            -H 'Content-Type: application/x-www-form-urlencoded' \
            -d "username=${CMS_USER}&password=${CMS_PASS}")" == "302" ]] && ok=$(( ok + 1 ))
    done
    check "$(( max + 3 )) correct logins are never throttled" "$ok" "$(( max + 3 ))"

    # Wrong passwords do consume it.
    refused=0
    for _ in $(seq 1 "$max"); do
        [[ "$(code -X POST "$BASE/auth/login" \
            -H 'Content-Type: application/x-www-form-urlencoded' \
            -d "username=${CMS_USER}&password=wrong-on-purpose")" == "302" ]] && refused=$(( refused + 1 ))
    done
    check "the first $max wrong passwords are refused, not throttled" "$refused" "$max"

    blocked=$(heads -X POST "$BASE/auth/login" \
        -H 'Content-Type: application/x-www-form-urlencoded' \
        -d "username=${CMS_USER}&password=wrong-on-purpose")
    check "wrong password number $(( max + 1 )) is throttled" \
        "$(printf '%s\n' "$blocked" | grep -c '^HTTP/.* 429')" "1"
    check_nonempty "the 429 carries Retry-After" "$(header_value 'retry-after' "$blocked")"

    # While blocked, even the right password is refused. That is the point of it.
    check "the right password is refused while the IP is blocked" \
        "$(code -X POST "$BASE/auth/login" \
            -H 'Content-Type: application/x-www-form-urlencoded' \
            -d "username=${CMS_USER}&password=${CMS_PASS}")" "429"

    # And the block lifts, leaving nothing behind for the next run.
    sleep $(( window + 2 ))
    check "the throttle lifts after its window" \
        "$(code -X POST "$BASE/auth/login" \
            -H 'Content-Type: application/x-www-form-urlencoded' \
            -d "username=${CMS_USER}&password=${CMS_PASS}")" "302"
fi

# ── Summary ──────────────────────────────────────────────────────────────────

echo
echo "═══════════════════════════════════════════════════════"
printf '  %s%d passed%s  %s%d failed%s  (%d checks)\n' \
    "$GREEN" "$PASS" "$RESET" "$RED" "$FAIL" "$RESET" "$(( PASS + FAIL ))"
echo "═══════════════════════════════════════════════════════"

[[ $FAIL -eq 0 ]] || exit 1
