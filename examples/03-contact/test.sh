#!/usr/bin/env bash
# test.sh — the contact form end to end, for example 03.
#
# Run the same way as 05-cms/test.sh: the driver starts the example, runs this
# file and stops it. Nothing here starts or stops the site.
#
#   ../m6-test-eg 3            the driver (start, this file, stop)
#   ./dev.sh                      one terminal
#   ./test.sh                     another
#
# Usage:
#   ./test.sh                     against https://127.0.0.1:8443
#   ./test.sh --addr HOST:PORT    against a different address
#
# ── The relay is part of the test ────────────────────────────────────────────
#
# The form hands the message to an SMTP relay. "Run a mail catcher on port 2525
# first" measures nothing on a machine where nobody did: the accepted send would
# fail, and a check that needs a step the test does not take is a check that
# silently disappears the moment the step is forgotten. So this file runs its
# own relay: a Python socket in a scratch directory that answers the protocol in
# the three modes the checks need, and is killed for the fourth.
#
# Python is not a new dependency: 05-cms/test.sh already parses JSON with
# python3. Bash can open a socket for the accepted send, but an SMTP
# conversation is a greeting, multiline replies, four commands and a
# dot-terminated body; the smallest thing that does that cleanly is the script
# below, and the alternative (nc) is not on every machine.
#
# ── The port ─────────────────────────────────────────────────────────────────
#
# The port is not written here. It is read from configs/render-contact.conf, the
# one place the example states where its relay is, so this test cannot pass
# against a relay the example is not talking to. The config names 2525; port
# 1025 is refused outright, because it is mailpit's default and is already held
# on some machines (macOS's `bridge` on the one this was written on), and binding
# a second program's port is not a test of this example.
#
# ── The 504 ──────────────────────────────────────────────────────────────────
#
# The silent relay is ended by the service's own 20 s delivery deadline, so the
# 504 takes 20 s of wall clock. That duration is not asserted: a stopwatch
# around a deadline is a race, and the reason is not reported to the client.
# render-contact answers the deadline with a body of "timeout", and m6-http
# replaces that body with its own 504 page, so nothing the client can see says
# which timeout fired. The test asserts the status, and that the relay's
# delivery connection is closed rather than still waiting: the deadline does
# that, an early answer does not.
#
# There are no skips. Every check has one expected answer, and the file exits
# non-zero if any of them is not it.
set -uo pipefail

SITE="$(cd "$(dirname "$0")" && pwd)"
ADDR="127.0.0.1:8443"
# The relay this test starts is the only relay it may stop. See below.
. "$SITE/../dev-home.sh"
. "$SITE/../own.sh"

while [[ $# -gt 0 ]]; do
    case "$1" in
        --addr) ADDR="$2"; shift 2 ;;
        *) echo "Unknown flag: $1" >&2; exit 1 ;;
    esac
done

BASE="https://${ADDR}"
CURL=(curl -sk --http1.1 -m 60)

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

section() { echo; echo "${YELLOW}── $1 ${RESET}"; }

code() {
    local c
    c=$("${CURL[@]}" -o /dev/null -w '%{http_code}' "$@" 2>/dev/null) || c="000"
    printf '%s' "${c:-000}"
}
body() { "${CURL[@]}" "$@"; }

# ── The scripted relay ───────────────────────────────────────────────────────
#
# Kept in the private development directory (dev-home.sh), outside every site
# tree, so a stray file cannot be served as content. The path is fixed for this checkout rather than a mktemp one so a run
# that was killed can find and clear its own relay before binding. The key is
# the cksum of SITE because SITE ends in the example name in every worktree, and
# the name alone gave every worktree one shared directory.

RELAY_KEY=$(printf '%s' "$SITE" | cksum | cut -d' ' -f1)
RELAY_DIR="$M6_DEV_HOME/eg-contact-relay-$(basename "$SITE")-$RELAY_KEY"
RELAY_PORT=$(sed -n 's/^[[:space:]]*port[[:space:]]*=[[:space:]]*\([0-9][0-9]*\).*/\1/p' \
    "$SITE/configs/render-contact.conf" | tail -1)

if [[ -z "$RELAY_PORT" ]]; then
    echo "FATAL: no [smtp] port in $SITE/configs/render-contact.conf" >&2
    exit 1
fi
if [[ "$RELAY_PORT" -eq 1025 ]]; then
    echo "FATAL: configs/render-contact.conf names port 1025, which this test will not use." >&2
    echo "It is mailpit's default and is already held on some machines. The committed" >&2
    echo "example uses 2525; a different free port works too, in the config and here." >&2
    exit 1
fi

mkdir -p "$RELAY_DIR"
cat > "$RELAY_DIR/relay.py" <<'PY'
#!/usr/bin/env python3
"""A scripted SMTP relay, the fixture test.sh drives.

Not a mail server: it answers the protocol so the example's send path can be
checked from the outside. Three modes:

  ok      greet, accept EHLO/MAIL/RCPT/DATA, answer 250 after the body
  refuse  greet, accept EHLO, answer 554 to MAIL FROM
  silent  accept the connection and say nothing, so the service's own delivery
          deadline is what ends the send

Every command and the message body go to the log, which is where the relay-side
checks read them from.
"""
import socket
import sys
import threading

port, mode, logpath = int(sys.argv[1]), sys.argv[2], sys.argv[3]
log = open(logpath, "a", buffering=1)


def note(line):
    log.write(line + "\n")


def handle(conn):
    f = conn.makefile("rwb")
    note("CONNECT")
    if mode == "silent":
        note("SILENT")
        try:
            while conn.recv(4096):
                pass
        except OSError:
            pass
        return
    f.write(b"220 test relay\r\n")
    f.flush()
    in_data = False
    while True:
        line = f.readline()
        if not line:
            note("EOF")
            return
        text = line.rstrip(b"\r\n").decode("utf-8", "replace")
        upper = text.upper()
        if in_data:
            if text == ".":
                in_data = False
                note("DATA-END")
                f.write(b"250 ok\r\n")
            else:
                note("BODY " + text)
        elif upper.startswith("DATA"):
            in_data = True
            note("DATA:")
            f.write(b"354 go ahead\r\n")
        elif upper.startswith("MAIL FROM") and mode == "refuse":
            note(text)
            note("REFUSED")
            f.write(b"554 no\r\n")
            f.flush()
            return
        elif upper.startswith("EHLO") or upper.startswith("HELO"):
            note(text)
            f.write(b"250 test relay\r\n")
        elif upper.startswith("MAIL FROM") or upper.startswith("RCPT TO"):
            note(text)
            f.write(b"250 ok\r\n")
        elif upper.startswith("QUIT"):
            note(text)
            f.write(b"221 bye\r\n")
            f.flush()
            return
        else:
            note(text)
            f.write(b"250 ok\r\n")
        f.flush()


srv = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
srv.bind(("127.0.0.1", port))
srv.listen(8)
note("LISTEN %d %s" % (port, mode))
while True:
    conn, _ = srv.accept()
    threading.Thread(target=handle, args=(conn,), daemon=True).start()
PY

RELAY_PID=""
relay_stop() {
    if [[ -n "$RELAY_PID" ]] && kill -0 "$RELAY_PID" 2>/dev/null; then
        kill "$RELAY_PID" 2>/dev/null || true
        wait "$RELAY_PID" 2>/dev/null || true
    fi
    RELAY_PID=""
    # Every recorded pid is dead now; a run killed mid-relay leaves its file for
    # the next run's own_reap to find.
    own_forget
}

# relay_start <ok|refuse|silent>
relay_start() {
    local mode="$1" line deadline
    local log="$RELAY_DIR/relay-$mode.log"
    relay_stop
    : > "$log"
    python3 "$RELAY_DIR/relay.py" "$RELAY_PORT" "$mode" "$log" &
    RELAY_PID=$!
    own_track "$RELAY_PID"
    # Ready means the socket answers, and in the modes that greet that the
    # greeting arrived. The condition, under one generous bound: the previous
    # version proved it with a 500-iteration busy loop over the log, one connect
    # and a one-second deadline on the greeting. That is a stopwatch, not a
    # readiness test, and on a loaded host a relay that was merely slow was
    # reported as one that never started. Nothing here waits a fixed duration:
    # each attempt is made as soon as the previous one fails, the relay exiting
    # ends the wait at once, and the bound only stops a relay that is truly dead.
    deadline=$((SECONDS + 60))
    while :; do
        if ! kill -0 "$RELAY_PID" 2>/dev/null; then
            break
        fi
        if { exec 9<>"/dev/tcp/127.0.0.1/$RELAY_PORT"; } 2>/dev/null; then
            if [[ "$mode" == "silent" ]]; then
                # Silent never greets by design, so a successful connect is all
                # the answer there is.
                exec 9<&-
                return 0
            fi
            if IFS= read -r -t 1 -u 9 line && [[ "$line" == 220* ]]; then
                exec 9<&-
                return 0
            fi
            exec 9<&-
        fi
        if [[ "$SECONDS" -ge "$deadline" ]]; then
            break
        fi
        sleep 0.1
    done
    echo "FATAL: the scripted relay did not answer on 127.0.0.1:$RELAY_PORT ($mode) within 60s" >&2
    sed 's/^/  /' "$log" >&2 2>/dev/null || true
    exit 1
}

# Claim the port the way every example's dev.sh claims its HTTPS port: reap the
# relay a previous run of THIS example recorded, by pid, in
# <RELAY_DIR>/logs/.own-relay.pids, then bind. A listener this test cannot prove
# it started is reported with its pid and the run stops. Killing a stranger's
# relay to make the checks pass would make them pass on a machine state the test
# itself created.
#
# RELAY_DIR is keyed to this checkout, so the pid file names only relays this
# worktree's runs started. Another worktree running this example records its
# relay under a different key, and own_reap here never reads that key.
own_init "$RELAY_DIR" relay
own_reap
if ! own_refuse_busy "$RELAY_PORT" "example 03's test"; then
    echo "       Or point configs/render-contact.conf at a free port." >&2
    exit 1
fi

WORK="$(mktemp -d)"
trap 'relay_stop; rm -rf "$WORK"' EXIT

# post_contact — leaves the status in $CODE and the page in $BODY.
post_contact() {
    CODE=$("${CURL[@]}" -o "$WORK/body.html" -w '%{http_code}' \
        -X POST "$BASE/contact" \
        -H 'Content-Type: application/x-www-form-urlencoded' \
        -d 'name=Test+Sender&email=sender%40example.com&message=Sent+by+test.sh' \
        2>/dev/null) || CODE="000"
    BODY=$(cat "$WORK/body.html" 2>/dev/null || true)
}

# ── The stack answers ────────────────────────────────────────────────────────

section "1. The form is reachable"

c=$(code "$BASE/contact")
check "the stack answers GET /contact" "$c" "200"
if [[ "$c" != "200" ]]; then
    echo
    echo "  ${RED}The example is not answering at $BASE/contact.${RESET}" >&2
    echo "  Start it with ./dev.sh, or run the driver: ../m6-test-eg 3" >&2
    echo "  Logs: $SITE/logs/  and  /tmp/m6-eg-03.log" >&2
fi
# m6-html renders attributes unquoted (`action=/contact`), so match either form.
if body "$BASE/contact" | grep -Eq 'action="?/contact"?'; then
    pass "GET /contact renders the form"
else
    fail "GET /contact renders the form — no form posting to /contact in the page"
fi

# ── A relay that accepts ─────────────────────────────────────────────────────

section "2. A relay that accepts the message"

relay_start ok
post_contact
check "POST /contact with a live relay" "$CODE" "200"
check_contains "the accepted send renders sent: true" "$BODY" "Message sent"

# The relay-side facts that say the message was actually handed over, not
# merely that the page said so. DATA-END is the dot that ends the body, so the
# whole message arrived, not just the command. The body and RCPT lines are the
# text that was typed and the recipient the example configured; a send that
# delivered an empty body or a different recipient fails here.
check "the relay received DATA" \
    "$(grep -c '^DATA-END$' "$RELAY_DIR/relay-ok.log" 2>/dev/null || true)" "1"
check "the relay received the message body" \
    "$(grep -c '^BODY Sent by test\.sh$' "$RELAY_DIR/relay-ok.log" 2>/dev/null || true)" "1"
check "the relay saw MAIL FROM:<noreply@example.com>" \
    "$(grep -cF 'MAIL FROM:<noreply@example.com>' "$RELAY_DIR/relay-ok.log" 2>/dev/null || true)" "1"
check "the relay saw RCPT TO:<owner@example.com>" \
    "$(grep -cF 'RCPT TO:<owner@example.com>' "$RELAY_DIR/relay-ok.log" 2>/dev/null || true)" "1"

# ── A relay that refuses ─────────────────────────────────────────────────────

section "3. A relay that refuses the message"

relay_start refuse
post_contact
check "POST /contact with a refusing relay" "$CODE" "500"

# No second check on the body. m6-http's internal error page is built from the
# status it is about to return (error.rs internal_error_html), so a body
# containing "500" is exactly the status checked above and cannot fail unless
# that check already did. The renderer's own body never reaches the client.

# ── A relay that says nothing ────────────────────────────────────────────────

section "4. A relay that never answers"

relay_start silent
post_contact
check "POST /contact with a silent relay" "$CODE" "504"

# What ended the send is not visible to the client: render-contact's "timeout"
# body is replaced by m6-http's 504 page, and neither process logs the reason.
# A stopwatch around the 20 s deadline is a race, so assert the strongest fact
# that is not one: the relay's delivery connection is closed rather than still
# waiting. That is what the deadline does and an early answer does not.
for _ in $(seq 1 100); do
    [[ -z "$(lsof -nP -a -p "$RELAY_PID" -iTCP -sTCP:ESTABLISHED 2>/dev/null)" ]] && break
done
check "the silent relay's delivery connection was closed" \
    "$(lsof -nP -a -p "$RELAY_PID" -iTCP -sTCP:ESTABLISHED 2>/dev/null | wc -l | tr -d ' ')" "0"

# The 504 frees the delivery slot and leaves the client connection open for its
# next request, so the step after it must show that the slot came back. It used
# to start the replacement relay and post at once, on the assumption that a
# relay which greets means the front can serve this route again. That is a
# different fact, and the check failed under load with "expected 200, got 504":
# the first request after the timeout was also the first request to test whether
# the front and this backend were serving again.
#
# So wait for the condition the check needs, through the front: a GET of the
# same route, with a nonce in the query so the answer cannot be a cache hit and
# must come from the backend. It ends the moment the answer is 200, and the
# bound only stops a stack that is truly not serving. Nothing here sleeps for a
# duration and the POST is not retried.
relay_start ok
NONCE="probe-$$-$SECONDS"
probe_deadline=$((SECONDS + 60))
while :; do
    pc=$(code "$BASE/contact?$NONCE")
    [[ "$pc" == "200" ]] && break
    if [[ "$SECONDS" -ge "$probe_deadline" ]]; then
        echo "  note: /contact did not answer 200 within 60s (last ${pc:-000}); the POST below still runs." >&2
        break
    fi
done
post_contact
check "POST /contact after the 504" "$CODE" "200"

# ── No relay at all ──────────────────────────────────────────────────────────

section "5. Nothing listening"

relay_stop
post_contact
check "POST /contact with no relay listening" "$CODE" "500"

# ── Summary ──────────────────────────────────────────────────────────────────

echo
echo "═══════════════════════════════════════════════════════"
printf '  %s%s passed%s  %s%s failed%s  (%d checks)\n' \
    "$GREEN" "$PASS" "$RESET" "$RED" "$FAIL" "$RESET" "$(( PASS + FAIL ))"
echo "═══════════════════════════════════════════════════════"

[[ $FAIL -eq 0 ]] || exit 1
