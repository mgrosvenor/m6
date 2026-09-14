#!/usr/bin/env bash
# build-host-tests.sh — run m6's own checks on a Linux build host.
#
#   M6_BUILD_HOST=root@198.51.100.7 ./tools/build-host-tests.sh
#
# ── Why m6 has its own ───────────────────────────────────────────────────────
#
# These checks used to live in a deployment repository, and `tools/merge.sh` and
# `tools/release.sh` reached into it to run them. That is backwards: m6 is a
# generic web system and its own correctness is its own business, so a bare
# checkout with no site anywhere near it could not check itself.
#
# A deployment keeps its own runner for the things only it can check — its
# renderers, its content, its rendered configs. Both exist, and a deployment's
# runner can call this one for the m6 half. See docs/m6-user-guide.md.
#
# ── Why a remote Linux box at all ────────────────────────────────────────────
#
# Two of the rules can only be checked there, and one of them cannot be checked
# on a busy machine at all:
#
#   - ZERO COMPILER WARNINGS on the target platform. Some are cfg-gated and
#     appear only on Linux; an `unused_mut` in the inotify watcher was invisible
#     on macOS for exactly that reason.
#   - CONFORMANCE needs h1spec, h2spec and h3spec installed. A laptop without
#     them reported "skipped" and returned success for months, so h2 and h3 went
#     unchecked on every run and nothing said so.
#
# ── Configuration ────────────────────────────────────────────────────────────
#
#   M6_BUILD_HOST      required. user@host for ssh and rsync.
#   M6_BUILD_SSH_OPTS  optional ssh options, e.g. "-p 4022". Default none.
#   M6_BUILD_ROOT      optional remote directory. Default /root/build.
#   M6_ALLOW_DIRTY     optional. Skip the clean-tree guard, for local iteration.
#   M6_EXAMPLES        optional path to the m6-examples checkout. Default: a
#                      sibling directory named m6-examples. Set M6_SKIP_EXAMPLES=1
#                      to run without it, knowing what that leaves unchecked.
#
# THE BUILD HOST IS ASSUMED NOT TO BE BACKED UP. Nothing may exist only there, so
# this refuses to run against a dirty tree: whatever it tested is then always
# recoverable from git. Pass M6_ALLOW_DIRTY=1 to override, knowingly.
#
# ── Why the examples are built here ──────────────────────────────────────────
#
# m6-examples is the first place an m6 API change shows up, and for weeks nothing
# built it. By 2026-09-14 it did not compile at all: every renderer crate still
# pointed at `m6-render`, a crate m6 had deleted, and the binaries left in
# `target/release` from before the deletion meant running an example still looked
# fine. Underneath that were five more defects, each invisible for the same
# reason -- every asset in every example 502'd because m6-file's config schema had
# changed, PATCH was refused at the edge, unpublishing a post silently did
# nothing.
#
# m6's own checks were passing throughout. They could not have caught any of it,
# because m6 does not contain a site and the examples are where m6's interfaces
# are actually used. So they are built here, and their end-to-end test runs
# against the binaries this script just built.
#
# It DOES run the performance check, as of 2026-09-14.
#
# It used to skip it, on the stated grounds that `tools/perfcheck.sh` measures
# "rendering real content against a rendered config" and therefore belonged to a
# deployment. That stopped being true the same day: perfcheck was repointed at the
# examples, whose content is committed, so it measures m6 against bytes that are
# the same on every machine.
#
# Until this was wired up, the two baselines recorded in perf-baseline.txt had
# nothing checking them — a recorded number nobody compares against is a number
# nobody will notice moving. And this is the only place it can run: a wall-clock
# measurement needs a quiet machine, which a shared CI runner is not.

set -uo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"

GREEN=$'\033[0;32m'; RED=$'\033[0;31m'; YELLOW=$'\033[1;33m'; RESET=$'\033[0m'
info() { echo "${YELLOW}==>${RESET} $1"; }
ok()   { echo "    ${GREEN}ok${RESET} $1"; }
die()  { echo "${RED}ERROR:${RESET} $1" >&2; exit 1; }

# Extract `KEY=<number>` from the log.
#
# NOT `grep -oP`: that is a GNU extension, and this script is driven from
# whatever machine the developer has. BSD grep answers "invalid option -- P" and
# the count silently becomes empty, which then reports phantom warnings on a
# clean run. sed is in POSIX and behaves the same on both.
num_after() { sed -n "s/.*$1=\([0-9][0-9]*\).*/\1/p" "$2" | tail -1; }

BUILD_HOST="${M6_BUILD_HOST:-}"
[[ -n "$BUILD_HOST" ]] || die "M6_BUILD_HOST is not set.

  These checks need a Linux box with h1spec, h2spec, h3spec, cargo-deny and the
  backend-example runtimes (cc, c++, python3, go) on it. m6 does not know which
  machine is yours.

      export M6_BUILD_HOST=root@198.51.100.7
      export M6_BUILD_SSH_OPTS='-p 4022'      # if it is not on 22"

SSH_OPTS="${M6_BUILD_SSH_OPTS:-} -o StrictHostKeyChecking=accept-new -o ConnectTimeout=15"
BUILD_ROOT="${M6_BUILD_ROOT:-/root/build}"
LOG=/tmp/m6-build-host-tests.log
EXAMPLES_LOG=/tmp/m6-build-host-examples.log

# ── Where the examples are ───────────────────────────────────────────────────
#
# The renderer crates reach m6 by relative path (`../../../../m6/m6-core`), which
# resolves to a sibling directory. They are synced as siblings on the build host
# for the same reason, so the path holds there without editing any manifest.
EXAMPLES="${M6_EXAMPLES:-$(cd "$ROOT/.." 2>/dev/null && pwd)/m6-examples}"
if [[ -n "${M6_SKIP_EXAMPLES:-}" ]]; then
    EXAMPLES=""
elif [[ ! -f "$EXAMPLES/Cargo.toml" ]]; then
    die "cannot find the examples repository at '$EXAMPLES'.

  m6-examples is where m6's own interfaces are actually used, and it is the first
  place an API change breaks. It went unbuilt long enough to stop compiling
  entirely, so it is checked here rather than trusted.

      export M6_EXAMPLES=/path/to/m6-examples

  To run without it, knowing that leaves every example unchecked:

      export M6_SKIP_EXAMPLES=1"
fi

# ── The git guard ────────────────────────────────────────────────────────────
if [[ -z "${M6_ALLOW_DIRTY:-}" ]]; then
    [[ -z "$(git -C "$ROOT" status --porcelain)" ]] || {
        echo "${RED}ERROR:${RESET} $ROOT has uncommitted changes." >&2
        git -C "$ROOT" status --short | head -10 >&2
        echo "  The build host is assumed not to be backed up. Commit first, or" >&2
        echo "  set M6_ALLOW_DIRTY=1." >&2
        exit 1; }
    ok "tree is clean (nothing will exist only on the build host)"

    # The examples get the same guard: whatever ran there has to be recoverable
    # from git too, and the logs and generated data files these examples write
    # while running are noise rather than work, so they are excluded.
    if [[ -n "$EXAMPLES" ]]; then
        dirty=$(git -C "$EXAMPLES" status --porcelain 2>/dev/null \
            | grep -vE '(logs/|\.db(-shm|-wal)?$|/data/posts\.json$|/content/drafts/)' || true)
        [[ -z "$dirty" ]] || {
            echo "${RED}ERROR:${RESET} $EXAMPLES has uncommitted changes." >&2
            printf '%s\n' "$dirty" | head -10 >&2
            echo "  Commit them, or set M6_ALLOW_DIRTY=1." >&2
            exit 1; }
        ok "examples tree is clean"
    fi
fi

# ── Sync ─────────────────────────────────────────────────────────────────────
# --no-times, then restamp: rsync -a preserves source mtimes, so a file edited
# before the last build lands looking older than its own artefacts and cargo
# rebuilds NOTHING while reporting success. That shipped a stale binary once.
info "syncing to $BUILD_HOST:$BUILD_ROOT/m6"
# shellcheck disable=SC2086
ssh $SSH_OPTS "$BUILD_HOST" "mkdir -p $BUILD_ROOT" || die "cannot reach $BUILD_HOST"
# shellcheck disable=SC2086
rsync -az -e "ssh $SSH_OPTS" --delete --exclude 'target/' --exclude '.git/' \
    "$ROOT/" "$BUILD_HOST:$BUILD_ROOT/m6/" || die "rsync failed"
# shellcheck disable=SC2086
ssh $SSH_OPTS "$BUILD_HOST" \
    "find $BUILD_ROOT/m6 \\( -name '*.rs' -o -name 'Cargo.toml' -o -name 'Cargo.lock' \\) -exec touch {} +"
ok "source synced and restamped"

if [[ -n "$EXAMPLES" ]]; then
    info "syncing examples to $BUILD_HOST:$BUILD_ROOT/m6-examples"
    # shellcheck disable=SC2086
    rsync -az -e "ssh $SSH_OPTS" --delete --exclude 'target/' --exclude '.git/' \
        --exclude 'logs/' "$EXAMPLES/" "$BUILD_HOST:$BUILD_ROOT/m6-examples/" \
        || die "rsync of examples failed"
    # shellcheck disable=SC2086
    ssh $SSH_OPTS "$BUILD_HOST" \
        "find $BUILD_ROOT/m6-examples \\( -name '*.rs' -o -name 'Cargo.toml' -o -name 'Cargo.lock' \\) -exec touch {} +"
    ok "examples synced and restamped"
fi

# ── Run ──────────────────────────────────────────────────────────────────────
info "m6: release build, warnings, clippy, cargo-deny, tests, conformance"
# shellcheck disable=SC2086
ssh $SSH_OPTS "$BUILD_HOST" "M6_DIR=$BUILD_ROOT/m6 bash -s" > "$LOG" 2>&1 <<'REMOTE' || true
set -uo pipefail
# `/root/.local/bin` is where `uv` installs, and h1spec is delivered through
# `uvx`. It was missing from PATH once, so every run reported "h1: skipped, uvx
# not installed" while the binary sat on disk, and the skip was silent.
export PATH=$PATH:$HOME/.cargo/bin:$HOME/.local/bin:/root/.cargo/bin:/root/.local/bin
cd "$M6_DIR" || exit 1

echo "### release build"
cargo build --workspace --release 2>&1 | tail -3
echo "RELEASE_WARNINGS=$(cargo build --workspace --release 2>&1 | grep -c '^warning' || true)"

echo "### clippy"
# `-D warnings`, no ceiling. Prints its own line; CLIPPY_STATUS is what decides.
./tools/clippy.sh > /tmp/clippy.out 2>&1
echo "CLIPPY_STATUS=$?"
tail -5 /tmp/clippy.out

echo "### cargo-deny"
# It used to run only in GitHub Actions, and that cost four hours once: a git
# source changed, deny.toml was not updated to allow it, CI went red on every
# push, and the build-host checks kept reporting that everything passed. A check
# that lives in one place only is not a check.
if command -v cargo-deny >/dev/null 2>&1; then
    cargo deny check > /tmp/deny.out 2>&1
    echo "DENY_STATUS=$?"
    tail -4 /tmp/deny.out
else
    echo "DENY_STATUS=127"
    echo "cargo-deny is NOT INSTALLED here: cargo install cargo-deny --locked"
fi

echo "### backend runtimes"
# docs/m6-backend-examples.md §8: a missing runtime must not silently pass. On a
# laptop an absent toolchain skips that language with a warning; HERE every one
# must be present, because a language that never ran is not a language that
# passed.
BACKEND_RUNTIMES_MISSING=""
for tool in cc c++ python3 go; do
    command -v "$tool" >/dev/null 2>&1 || BACKEND_RUNTIMES_MISSING="$BACKEND_RUNTIMES_MISSING $tool"
done
echo "BACKEND_RUNTIMES_MISSING=$BACKEND_RUNTIMES_MISSING"
for tool in cc c++ python3 go; do
    printf '  %-8s %s\n' "$tool" "$(command -v "$tool" 2>/dev/null || echo MISSING)"
done

echo "### test build"
echo "TESTBUILD_WARNINGS=$(cargo test --workspace --no-run 2>&1 | grep -c '^warning' || true)"

echo "### test run"
# Strict mode for the backend examples: a missing runtime fails rather than skips.
export M6_BACKENDS_REQUIRE_ALL=1
# Captured in full, then reported. Grepping the pipe drops the assertion message,
# leaving a bare `thread '<name>' panicked at <file>:<line>` with the text that
# says WHY thrown away.
cargo test --workspace > /tmp/m6-full.log 2>&1 || true
grep -E '^test result:' /tmp/m6-full.log || true
echo "### failure detail"
sed -n '/^failures:/,$p' /tmp/m6-full.log | head -60

echo "### conformance"
# WITHOUT --allow-missing-tools, deliberately. This box has all three testers,
# and an absent one here is a failure rather than a skip. A laptop's pre-push
# hook may pass that flag and say loudly what it did not test; this has no excuse
# available to it.
./tools/conformance.sh > /tmp/conformance.out 2>&1
echo "CONFORMANCE_STATUS=$?"
tail -14 /tmp/conformance.out

echo "### performance"
# Against the recorded numbers in tools/perf-baseline.txt, taken on this machine.
# A run slower than its number by more than the margin fails; a faster one prints
# the reading and asks for it to be recorded deliberately.
./tools/perfcheck.sh > /tmp/perf.out 2>&1
echo "PERF_STATUS=$?"
grep -E 'render:|PASS|FAIL' /tmp/perf.out | tail -6
REMOTE

# ── The examples ─────────────────────────────────────────────────────────────
#
# A separate ssh invocation and a separate log, so a failure here is legible
# rather than buried at the bottom of m6's own output. It builds against the m6
# tree just synced, so this is the check that an m6 change did not break the
# code that uses m6.
if [[ -n "$EXAMPLES" ]]; then
    info "m6-examples: build, warnings, clippy, tests, and the CMS end-to-end suite"
    # shellcheck disable=SC2086
    ssh $SSH_OPTS "$BUILD_HOST" "EG_DIR=$BUILD_ROOT/m6-examples M6_DIR=$BUILD_ROOT/m6 bash -s" \
        > "$EXAMPLES_LOG" 2>&1 <<'REMOTE_EG' || true
set -uo pipefail
export PATH=$PATH:$HOME/.cargo/bin:$HOME/.local/bin:/root/.cargo/bin:/root/.local/bin
cd "$EG_DIR" || exit 1

echo "### examples release build"
cargo build --release 2>&1 | tail -3
echo "EG_RELEASE_WARNINGS=$(cargo build --release 2>&1 | grep -c '^warning' || true)"

echo "### examples clippy"
# The examples meet the same standard as m6: a reader copies from them.
#
# Into its OWN target directory. clippy and cargo build share fingerprints when
# they share a target dir, so each one invalidates the other's artifacts: running
# clippy here left the release build stale, and the 05-cms stage below then spent
# three minutes rebuilding from scratch inside its startup timeout and was
# reported as a stack that never answered. The wasted disk is worth more than a
# failure that points at the wrong thing.
CARGO_TARGET_DIR=target/clippy cargo clippy --workspace --all-targets --release 2>&1 \
    | grep -c '^warning' > /tmp/eg-clippy.count
echo "EG_CLIPPY_WARNINGS=$(cat /tmp/eg-clippy.count)"
CARGO_TARGET_DIR=target/clippy cargo clippy --workspace --all-targets --release 2>&1 \
    | grep '^warning' | head -5

echo "### examples unit tests"
cargo test --workspace > /tmp/eg-tests.log 2>&1
echo "EG_TEST_STATUS=$?"
grep -E '^test result:' /tmp/eg-tests.log || true
sed -n '/^failures:/,$p' /tmp/eg-tests.log | head -40

echo "### development certificates"
# A fresh checkout has no keys/: they are gitignored, and each example's dev.sh
# generates them. The config parse below validates that tls_cert exists, so
# without this every example is rejected for a missing file. This passed here only
# because rsync carried the developer's own untracked keys.
for d in examples/*/; do
    [ -f "$d/dev.sh" ] || continue
    mkdir -p "$d/keys"
    if [ ! -f "$d/keys/dev.pem" ]; then
        openssl req -x509 -newkey rsa:2048 -sha256 -days 365 -nodes \
            -keyout "$d/keys/dev-key.pem" -out "$d/keys/dev.pem" \
            -subj "/CN=localhost" -addext "subjectAltName=DNS:localhost,IP:127.0.0.1" 2>/dev/null
    fi
    # Five examples need an auth signing keypair too; their setup.sh makes it.
    if [ ! -f "$d/keys/auth.pub" ]; then
        openssl ecparam -name prime256v1 -genkey -noout -out "$d/keys/auth.pem" 2>/dev/null
        openssl ec -in "$d/keys/auth.pem" -pubout -out "$d/keys/auth.pub" 2>/dev/null
        chmod 600 "$d/keys/auth.pem" 2>/dev/null || true
    fi
done
mkdir -p examples/09-global-deployment/certs
echo "certificates present"

echo "### every example's config parses"
# m6-file's config schema changed and ten example configs were left naming no
# handler, so the service exited 2 before binding and every asset 502'd. Parsing
# each config with the real binary is cheap and catches that class outright.
#
# EVERY example is accounted for, and an example that is not parsed must say why.
# A loop that quietly stepped over the ones it could not find a config for would
# be the silent skip this project has been bitten by four times -- it would report
# nine of eleven and read exactly like eleven of eleven.
EG_CONFIG_BAD=0
EG_CONFIG_UNEXPLAINED=0
for d in examples/*/; do
    name=$(basename "$d")
    case "$name" in
        data) continue ;;   # shared fixtures, not an example
        06-systemd)
            printf '  %-24s not parsed: systemd units only, it has no site config\n' "$name"
            continue ;;
        09-global-deployment)
            # Its configs name /etc/letsencrypt paths for five real cache nodes,
            # so m6-http rejects them here for a missing certificate rather than
            # for anything wrong with the config. Its own integration test in
            # examples/09-global-deployment/integration-test covers it instead,
            # and that test ran above.
            printf '  %-24s not parsed: production configs name letsencrypt paths; its integration test covers it\n' "$name"
            continue ;;
    esac
    sys="$d/configs/system-dev.toml"
    [ -f "$sys" ] || sys="$d/site.toml"
    if [ ! -f "$sys" ]; then
        printf '  %-24s NO CONFIG FOUND and no reason recorded for that\n' "$name"
        EG_CONFIG_UNEXPLAINED=$((EG_CONFIG_UNEXPLAINED + 1))
        continue
    fi
    out=$("$M6_DIR/target/release/m6-http" "$(cd "$d" && pwd)" "$(cd "$(dirname "$sys")" && pwd)/$(basename "$sys")" --dump-config 2>&1 >/dev/null)
    if [ -n "$out" ]; then
        printf '  %-24s %s\n' "$name" "$(echo "$out" | head -1)"
        EG_CONFIG_BAD=$((EG_CONFIG_BAD + 1))
    else
        printf '  %-24s ok\n' "$name"
    fi
done
echo "EG_CONFIG_BAD=$EG_CONFIG_BAD"
echo "EG_CONFIG_UNEXPLAINED=$EG_CONFIG_UNEXPLAINED"

echo "### 05-cms end-to-end"
# The one example that runs the whole stack: edge, templates, files, markdown,
# auth and a custom renderer. If this passes, the parts work together.
# Everything built BEFORE dev.sh, so the readiness wait below measures startup
# and nothing else. dev.sh builds what it needs itself, which is right for a
# person running it by hand and wrong here: a cold build inside a startup timeout
# is reported as a stack that never came up.
cargo build --release -p render-cms 2>&1 | tail -1
(cd "$M6_DIR" && cargo build --workspace --release 2>&1 | tail -1)

cd examples/05-cms || exit 1
M6="$M6_DIR" M6_NO_BROWSER=1 ./dev.sh --no-open > /tmp/eg-cms-dev.log 2>&1 &
DEV_PID=$!
up=0
# 180 seconds. Six services have to bind, and m6-md regenerates posts.json from
# twelve markdown files first.
for _ in $(seq 1 180); do
    code=$(curl -sk --http1.1 -o /dev/null -w '%{http_code}' https://127.0.0.1:8443/ 2>/dev/null || true)
    if [ "$code" = "200" ]; then up=1; break; fi
    sleep 1
done
if [ "$up" != "1" ]; then
    echo "EG_CMS_STATUS=1"
    echo "the 05-cms stack never answered in 180s; dev.sh output:"
    tail -30 /tmp/eg-cms-dev.log
    echo "--- service logs ---"
    tail -20 logs/*.log 2>/dev/null
else
    ./test.sh > /tmp/eg-cms-test.log 2>&1
    echo "EG_CMS_STATUS=$?"
    grep -E 'passed|FAIL' /tmp/eg-cms-test.log | sed 's/\x1b\[[0-9;]*m//g' | tail -25
fi
kill $DEV_PID 2>/dev/null
# The stack's own children: dev.sh's trap handles them, but it is not given long
# here, so anything still holding the site directory is cleaned up by name of
# path rather than by process name. Killing by name would reach other m6
# instances on this box.
for pid in $(pgrep -f "$(pwd)" 2>/dev/null || true); do
    [ "$pid" = "$$" ] && continue
    kill "$pid" 2>/dev/null || true
done
REMOTE_EG
fi

# ── Report ───────────────────────────────────────────────────────────────────
FAILED=0
rw=$(num_after RELEASE_WARNINGS "$LOG");  rw="${rw:-?}"
tw=$(num_after TESTBUILD_WARNINGS "$LOG"); tw="${tw:-?}"
cl=$(num_after CLIPPY_STATUS "$LOG");      cl="${cl:-?}"
dn=$(num_after DENY_STATUS "$LOG");        dn="${dn:-?}"
cf=$(num_after CONFORMANCE_STATUS "$LOG"); cf="${cf:-?}"
read -r passed failed < <(awk '/^test result:/{p+=$4; f+=$6} END{print p+0, f+0}' "$LOG")

[[ "$rw" == "0" ]] || { echo "${RED}   $rw release warning(s) on Linux${RESET}" >&2; FAILED=1; }
[[ "$tw" == "0" ]] || { echo "${RED}   $tw test-build warning(s) on Linux${RESET}" >&2; FAILED=1; }
[[ "$cl" == "0" ]] || { echo "${RED}   clippy is not silent on Linux${RESET}" >&2
                        grep -A6 '^### clippy' "$LOG" >&2; FAILED=1; }
[[ "$dn" == "0" ]] || { echo "${RED}   cargo-deny failed (advisories/licences/sources)${RESET}" >&2
                        sed -n '/^### cargo-deny/,/^### backend runtimes/p' "$LOG" >&2; FAILED=1; }
missing=$(sed -n 's/^BACKEND_RUNTIMES_MISSING=//p' "$LOG" | tail -1)
[[ -z "${missing// /}" ]] || {
    echo "${RED}   backend example runtimes missing:$missing${RESET}" >&2
    echo "${RED}   A language that never ran is not a language that passed.${RESET}" >&2
    FAILED=1; }
[[ "$cf" == "0" ]] || { echo "${RED}   conformance failed (h1/h2/h3)${RESET}" >&2
                        sed -n '/^### conformance/,/^### performance/p' "$LOG" >&2; FAILED=1; }
pf=$(num_after PERF_STATUS "$LOG"); pf="${pf:-?}"
[[ "$pf" == "0" ]] || { echo "${RED}   performance check failed${RESET}" >&2
                        sed -n '/^### performance/,$p' "$LOG" >&2; FAILED=1; }
[[ "$failed" == "0" && "$passed" -gt 0 ]] || {
    echo "${RED}   m6: $passed passed, $failed failed${RESET}" >&2
    sed -n '/^### failure detail/,$p' "$LOG" >&2; FAILED=1; }

if [[ $FAILED -eq 0 ]]; then
    ok "m6: $passed passed, 0 failed, 0 warnings (release + test), clippy silent, cargo-deny ok, h1+h2+h3 ok, performance ok"
else
    echo
    echo "${RED}m6's own checks FAILED on $BUILD_HOST. Full log: $LOG${RESET}" >&2
fi

# ── Report: the examples ─────────────────────────────────────────────────────
if [[ -n "$EXAMPLES" ]]; then
    egw=$(num_after EG_RELEASE_WARNINGS "$EXAMPLES_LOG"); egw="${egw:-?}"
    egc=$(num_after EG_CLIPPY_WARNINGS  "$EXAMPLES_LOG"); egc="${egc:-?}"
    egt=$(num_after EG_TEST_STATUS      "$EXAMPLES_LOG"); egt="${egt:-?}"
    egk=$(num_after EG_CONFIG_BAD       "$EXAMPLES_LOG"); egk="${egk:-?}"
    ege=$(num_after EG_CMS_STATUS       "$EXAMPLES_LOG"); ege="${ege:-?}"
    read -r egpassed egfailed < \
        <(awk '/^test result:/{p+=$4; f+=$6} END{print p+0, f+0}' "$EXAMPLES_LOG")
    # NOT a `.*\([0-9][0-9]*\) passed` capture. The leading `.*` is greedy, so on
    # "98 passed" it swallowed the 9 and the group matched "8": the run reported
    # the CMS suite as 8 checks when it had made 98. A summary that gets its own
    # numbers wrong is worse than no summary, because it is the line a reader
    # trusts instead of opening the log.
    cms_pass=$(grep -oE '[0-9]+ passed' "$EXAMPLES_LOG" | tail -1 | awk '{print $1}')
    cms_fail=$(grep -oE '[0-9]+ failed' "$EXAMPLES_LOG" | tail -1 | awk '{print $1}')
    cms="${cms_pass:-?} passed, ${cms_fail:-?} failed"

    EG_FAILED=0
    [[ "$egw" == "0" ]] || { echo "${RED}   examples: $egw release warning(s)${RESET}" >&2; EG_FAILED=1; }
    [[ "$egc" == "0" ]] || { echo "${RED}   examples: $egc clippy finding(s)${RESET}" >&2
                             sed -n '/^### examples clippy/,/^### examples unit/p' "$EXAMPLES_LOG" >&2
                             EG_FAILED=1; }
    [[ "$egt" == "0" ]] || { echo "${RED}   examples: unit tests failed${RESET}" >&2
                             sed -n '/^### examples unit tests/,/^### every example/p' "$EXAMPLES_LOG" >&2
                             EG_FAILED=1; }
    [[ "$egk" == "0" ]] || { echo "${RED}   examples: $egk config(s) rejected by m6-http${RESET}" >&2
                             sed -n '/^### every example/,/^### 05-cms/p' "$EXAMPLES_LOG" >&2
                             EG_FAILED=1; }
    egu=$(num_after EG_CONFIG_UNEXPLAINED "$EXAMPLES_LOG"); egu="${egu:-?}"
    [[ "$egu" == "0" ]] || {
        echo "${RED}   examples: $egu example(s) had no config and no recorded reason.${RESET}" >&2
        echo "${RED}   Either give it one, or record in build-host-tests.sh why it has none.${RESET}" >&2
        sed -n '/^### every example/,/^### 05-cms/p' "$EXAMPLES_LOG" >&2
        EG_FAILED=1; }
    [[ "$ege" == "0" ]] || { echo "${RED}   examples: the 05-cms end-to-end suite failed${RESET}" >&2
                             sed -n '/^### 05-cms end-to-end/,$p' "$EXAMPLES_LOG" >&2
                             EG_FAILED=1; }

    if [[ $EG_FAILED -eq 0 ]]; then
        ok "examples: $egpassed passed, 0 failed, 0 warnings, clippy silent, every config parses, 05-cms end-to-end ${cms:-ok}"
    else
        echo "${RED}   examples FAILED. Full log: $EXAMPLES_LOG${RESET}" >&2
        FAILED=1
    fi
else
    echo "${YELLOW}   examples SKIPPED (M6_SKIP_EXAMPLES set): every example is unchecked,${RESET}"
    echo "${YELLOW}   including whether this m6 still builds the code that uses it.${RESET}"
fi

if [[ $FAILED -eq 0 ]]; then
    echo
    echo "${GREEN}m6 and its examples passed on $BUILD_HOST.${RESET}"
else
    echo
    echo "${RED}Checks FAILED on $BUILD_HOST.${RESET}" >&2
    echo "${RED}  m6:       $LOG${RESET}" >&2
    [[ -n "$EXAMPLES" ]] && echo "${RED}  examples: $EXAMPLES_LOG${RESET}" >&2
fi
exit $FAILED
