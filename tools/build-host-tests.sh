#!/usr/bin/env bash
# build-host-tests.sh — run m6's own checks on a Linux build host.
#
#   M6_BUILD_HOST=user@example.com ./tools/build-host-tests.sh
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
#   M6_BUILD_HOST      required. The ssh target, as user@example.com.
#   M6_BUILD_SSH_OPTS  optional ssh options, e.g. "-p 2222". Default none.
#   M6_BUILD_ROOT      optional remote directory, relative to the login
#                      directory unless absolute. Default m6-build.
#   M6_ALLOW_DIRTY     optional. Skip the clean-tree guard, for local iteration.
#   M6_SKIP_EXAMPLES   optional. Set to 1 to skip the examples (examples/ in this
#                      repository), knowing what that leaves unchecked.
#
# THE BUILD HOST IS ASSUMED NOT TO BE BACKED UP. Nothing may exist only there, so
# this refuses to run against a dirty tree: whatever it tested is then always
# recoverable from git. Pass M6_ALLOW_DIRTY=1 to override, knowingly.
#
# ── Why the examples are built here ──────────────────────────────────────────
#
# The examples are the first place an m6 API change shows up, and for weeks nothing
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
# It DOES run the performance check, which is an A/B of the candidate against a
# base build made on this same host in the same run. The base is M6_PERF_BASE
# when set, otherwise the merge base of HEAD and origin/develop, and on develop
# itself the commit before HEAD. Both arms render the same committed example
# bytes in alternating order, each pinned to its own CPU, and the gate fails on
# a consistent paired slowdown.
#
# A wall-clock measurement needs a quiet machine, which a shared CI runner is
# not, so this stays here rather than in CI.

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

      export M6_BUILD_HOST=user@example.com"

SSH_OPTS="${M6_BUILD_SSH_OPTS:-} -o StrictHostKeyChecking=accept-new -o ConnectTimeout=15"
# Relative, so the remote login directory decides where it lands. NOT `~/...`:
# the value is used inside `cd "$M6_DIR"` on the far side, and a quoted tilde
# is not expanded, so it would resolve to a literal directory named `~`.
BUILD_ROOT="${M6_BUILD_ROOT:-m6-build}"
LOG=/tmp/m6-build-host-tests.log
EXAMPLES_LOG=/tmp/m6-build-host-examples.log

# Copy tools/perf-history.jsonl back from the build host, so the record perfcheck
# appended there becomes part of the next commit. The sync sends the checkout's
# history to the host, so the host's file is the checkout's file plus this run's
# record and replaces it whole. The new file is written beside the old one and
# renamed only if the local file is a byte prefix of it: perfcheck only appends,
# so anything else means the local history changed during the run (a pull, a
# branch switch) and replacing it would drop records. A failure never leaves a
# partial history behind.
copy_back() {
    local src="$1"
    local dst="$2"
    local tmp
    tmp="$(mktemp "$dst.XXXXXX")" || { echo "cannot make a temp file beside $dst" >&2; return 1; }
    # shellcheck disable=SC2086
    if rsync -az -e "ssh $SSH_OPTS" "$src" "$tmp" && [[ -s "$tmp" ]] \
        && { [[ ! -s "$dst" ]] || cmp -s "$dst" <(head -c "$(( $(wc -c < "$dst") ))" "$tmp"); }; then
        mv "$tmp" "$dst"
    else
        rm -f "$tmp"
        echo "cannot copy $src back to $dst: the fetch failed, or the history here is not a prefix of the host's (it changed during the run)" >&2
        return 1
    fi
}

# ── Where the examples are ───────────────────────────────────────────────────
#
# examples/ in this repository, its own Cargo workspace. It is synced with the
# rest of the tree, so the renderer crates reach m6-core by the same relative
# path on the build host as here.
EXAMPLES="$ROOT/examples"
if [[ -n "${M6_SKIP_EXAMPLES:-}" ]]; then
    EXAMPLES=""
fi

# ── The git guard ────────────────────────────────────────────────────────────
# The logs and generated data files the examples write while running are noise
# rather than work, so they do not count as uncommitted changes.
EXAMPLES_NOISE='^.. examples/.*(logs/|\.db(-shm|-wal)?$|/data/posts\.json$|/content/drafts/)'
if [[ -z "${M6_ALLOW_DIRTY:-}" ]]; then
    # The history file is exempt: the host's copy is regenerated from the checkout
    # on every sync, so the gate can be rerun before the new record is committed.
    [[ -z "$(git -C "$ROOT" status --porcelain -- . ':!tools/perf-history.jsonl' | grep -vE "$EXAMPLES_NOISE" || true)" ]] || {
        echo "${RED}ERROR:${RESET} $ROOT has uncommitted changes." >&2
        git -C "$ROOT" status --short -- . ':!tools/perf-history.jsonl' | grep -vE "$EXAMPLES_NOISE" | head -10 >&2
        echo "  The build host is assumed not to be backed up. Commit first, or" >&2
        echo "  set M6_ALLOW_DIRTY=1." >&2
        exit 1; }
    ok "tree is clean (nothing will exist only on the build host)"
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

# ── The performance base ─────────────────────────────────────────────────────
#
# The candidate is compared against a build of its base commit made on this same
# host in the same run, so both arms share the machine, the toolchain and the
# content. M6_PERF_BASE picks a revision; otherwise the base is the merge base of
# HEAD and origin/develop, and on develop itself the commit before HEAD.
if [[ -n "${M6_PERF_BASE:-}" ]]; then
  PERF_BASE_REF="$M6_PERF_BASE"
else
  [[ -n "$(git -C "$ROOT" rev-parse --verify origin/develop 2>/dev/null)" ]] \
    || die "origin/develop is missing. Fetch it, or set M6_PERF_BASE."
  PERF_BASE_REF="$(git -C "$ROOT" merge-base HEAD origin/develop)"
  if [[ "$PERF_BASE_REF" == "$(git -C "$ROOT" rev-parse HEAD)" ]]; then
    PERF_BASE_REF="HEAD^1"
  fi
fi
PERF_BASE_COMMIT="$(git -C "$ROOT" rev-parse "$PERF_BASE_REF^{commit}")" \
  || die "cannot resolve the performance base '$PERF_BASE_REF'"
info "perf base: $PERF_BASE_REF $(git -C "$ROOT" rev-parse --short "$PERF_BASE_COMMIT")"

# Ship the base tree beside the candidate so it can be built on the host.
# shellcheck disable=SC2086
git -C "$ROOT" archive --format=tar "$PERF_BASE_COMMIT" \
  | ssh $SSH_OPTS "$BUILD_HOST" \
      "rm -rf $BUILD_ROOT/m6-base && mkdir -p $BUILD_ROOT/m6-base && tar -x -C $BUILD_ROOT/m6-base" \
  || die "cannot ship the performance base"

# ── Run ──────────────────────────────────────────────────────────────────────
info "m6: release build, warnings, clippy, cargo-deny, tests, conformance"
# shellcheck disable=SC2086
# PERF_COMMIT is read here, from the checkout being synced, because the remote
# tree has no `.git`. PERF_DIRTY is 1 when the checkout differs from that commit
# (possible only under M6_ALLOW_DIRTY), not counting the history file itself.
PERF_COMMIT="$(git -C "$ROOT" rev-parse HEAD 2>/dev/null || echo unknown)"
PERF_DIRTY=0
[[ -z "$(git -C "$ROOT" status --porcelain -- . ':!tools/perf-history.jsonl' 2>/dev/null)" ]] || PERF_DIRTY=1
ssh $SSH_OPTS "$BUILD_HOST" \
    "M6_DIR=$BUILD_ROOT/m6 M6_PERF_COMMIT=$PERF_COMMIT M6_PERF_DIRTY=$PERF_DIRTY M6_PERF_BASE_COMMIT=$PERF_BASE_COMMIT bash -s" \
    > "$LOG" 2>&1 <<'REMOTE' || true
set -uo pipefail
# `/root/.local/bin` is where `uv` installs, and h1spec is delivered through
# `uvx`. It was missing from PATH once, so every run reported "h1: skipped, uvx
# not installed" while the binary sat on disk, and the skip was silent.
export PATH=$PATH:$HOME/.cargo/bin:$HOME/.local/bin:/root/.cargo/bin:/root/.local/bin
cd "$M6_DIR" || exit 1
# This run's own files: another gate may be running on this host at the same
# time, and fixed /tmp paths would let each read the other's logs. They sit
# beside the checkout, not in /tmp, and are the run's TMPDIR: a hardened host
# mounts /tmp noexec, and rustdoc runs each doctest as a binary built in
# TMPDIR, so every doctest failed with "Permission denied" there.
T="$(mktemp -d "$PWD/../tmp.XXXXXX")" || exit 1
trap 'rm -rf -- "$T"' EXIT
export TMPDIR="$T"
# Several gates share this host (#344). tools/host-lock.sh holds the protocol:
# this run holds the host shared, the test suite and conformance take the port
# lock, and perfcheck gets the host to itself.
# shellcheck source=tools/host-lock.sh
. ./tools/host-lock.sh || exit 1
host_enter_shared

echo "### release build"
cargo build --workspace --release 2>&1 | tail -3
echo "RELEASE_WARNINGS=$(cargo build --workspace --release 2>&1 | grep -c '^warning' || true)"

echo "### performance base build"
# The base is built into its own target directory beside the candidate, and
# skipped when the stamp matches. The stamp, not mtimes: git archive stamps
# every file with the commit time, so cargo's mtime check would reuse a build
# of another commit (the same trap the restamp above describes).
if [ "$(cat ../target-base/BASE_COMMIT 2>/dev/null)" = "$M6_PERF_BASE_COMMIT" ]; then
    BASE_BUILD_STATUS=0
    echo "BASE_BUILD_STATUS=$BASE_BUILD_STATUS"
else
    (cd ../m6-base && CARGO_TARGET_DIR="$PWD/../target-base" cargo build --release -p m6-html)
    BASE_BUILD_STATUS=$?
    echo "BASE_BUILD_STATUS=$BASE_BUILD_STATUS"
    if [ "$BASE_BUILD_STATUS" = "0" ]; then
        mkdir -p ../target-base
        echo "$M6_PERF_BASE_COMMIT" > ../target-base/BASE_COMMIT
    fi
fi

echo "### clippy"
# `-D warnings`, no ceiling. Prints its own line; CLIPPY_STATUS is what decides.
./tools/clippy.sh > $T/clippy.out 2>&1
echo "CLIPPY_STATUS=$?"
tail -5 $T/clippy.out

echo "### rustdoc"
# Broken intra-doc links are rustdoc warnings, and the zero-warnings rule counts
# pre-existing warnings. `--all-features` as well, because `testkit` and the
# opt-in core features are off by default and nothing else builds their docs.
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps > $T/rustdoc.out 2>&1
echo "RUSTDOC_STATUS=$?"
tail -5 $T/rustdoc.out
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features > $T/rustdoc-all.out 2>&1
echo "RUSTDOC_ALL_STATUS=$?"
tail -5 $T/rustdoc-all.out

echo "### cargo-deny"
# It used to run only in GitHub Actions, and that cost four hours once: a git
# source changed, deny.toml was not updated to allow it, CI went red on every
# push, and the build-host checks kept reporting that everything passed. A check
# that lives in one place only is not a check.
if command -v cargo-deny >/dev/null 2>&1; then
    cargo deny check > $T/deny.out 2>&1
    echo "DENY_STATUS=$?"
    tail -4 $T/deny.out
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
# CI builds `--workspace --bins` before `cargo test --workspace --no-run`, and the
# gate must run the same steps (AGENTS rule 5). It matters because `cargo test`
# builds a package's plain binary only when that package has a tests/ directory,
# and m6-md has none, so its binary was absent from target/debug and m6-core's
# version_flag test failed with "no m6-md binary". A clean target showed it; a
# stale target hid it. One log so a compile error is visible: the warning count
# alone swallowed the failure.
{ cargo build --workspace --bins && cargo test --workspace --no-run; } > "$T/testbuild.log" 2>&1
TESTBUILD_STATUS=$?
echo "TESTBUILD_WARNINGS=$(grep -c '^warning' "$T/testbuild.log" || true)"
echo "TESTBUILD_STATUS=$TESTBUILD_STATUS"
[ "$TESTBUILD_STATUS" -eq 0 ] || tail -30 "$T/testbuild.log"

echo "### test run"
# Strict mode for the backend examples: a missing runtime fails rather than skips.
export M6_BACKENDS_REQUIRE_ALL=1
# Captured in full, then reported. Grepping the pipe drops the assertion message,
# leaving a bare `thread '<name>' panicked at <file>:<line>` with the text that
# says WHY thrown away.
with_port_lock cargo test --workspace > "$T/m6-full.log" 2>&1 || true
grep -E '^test result:' "$T/m6-full.log" || true
echo "### failure detail"
sed -n '/^failures:/,$p' "$T/m6-full.log" | head -60

echo "### feature-gated tests"
# The targets `cargo test --workspace` does not build, because their
# `required-features` are off by default. The same script CI and check.sh call,
# so this host cannot pass a suite the runner fails. `h1_tls_miss_insert` was
# the one that exposed three separate lists.
with_port_lock ./tools/feature-gated-tests.sh > "$T/feature-gated.log" 2>&1
echo "FEATURE_GATED_STATUS=$?"
grep -E '^test result:' "$T/feature-gated.log" || true
sed -n '/^failures:/,$p' "$T/feature-gated.log" | head -60

echo "### conformance"
# WITHOUT --allow-missing-tools, deliberately. This box has all three testers,
# and an absent one here is a failure rather than a skip. A laptop's pre-push
# hook may pass that flag and say loudly what it did not test; this has no excuse
# available to it.
with_port_lock ./tools/conformance.sh > "$T/conformance.out" 2>&1
echo "CONFORMANCE_STATUS=$?"
tail -14 "$T/conformance.out"

echo "### performance"
# An A/B of the candidate against the base built above, both made on this host
# in this run. M6_PERF_COMMIT, M6_PERF_DIRTY and M6_PERF_BASE_COMMIT arrive in
# the environment from the ssh invocation, set from the checkout that synced.
# The tree up here has no .git, so `git rev-parse` finds nothing, and a
# performance record that cannot name its commit cannot be compared with
# anything later.
if [ "$BASE_BUILD_STATUS" = "0" ]; then
    if host_alone; then
        ./tools/perfcheck.sh --base-bin "$PWD/../target-base/release/m6-html" > "$T/perf.out" 2>&1
        echo "PERF_STATUS=$?"
        grep -E 'render:|PASS|FAIL|NOT MEASURED|ACCEPTED' "$T/perf.out" | tail -8
    else
        echo "PERF_STATUS=2"
    fi
else
    echo "PERF_STATUS=2"
    echo "the performance base failed to build, so nothing was measured"
fi
REMOTE

# ── Bring the history back ───────────────────────────────────────────────────
#
# perfcheck appended one record to tools/perf-history.jsonl on the build host,
# the only place it runs. Bring it back now, pass or fail, because a run that
# failed is the one a later reader most wants the conditions for. Bookkeeping
# never blocks the check: a failure is reported and fails the run at the end, but
# the examples still run.
COPY_BACK_FAILED=0
if copy_back "$BUILD_HOST:$BUILD_ROOT/m6/tools/perf-history.jsonl" "$ROOT/tools/perf-history.jsonl"; then
    info "commit tools/perf-history.jsonl to keep this run's record"
else
    COPY_BACK_FAILED=1
fi

# ── The examples ─────────────────────────────────────────────────────────────
#
# A separate ssh invocation and a separate log, so a failure here is legible
# rather than buried at the bottom of m6's own output. It builds against the m6
# tree just synced, so this is the check that an m6 change did not break the
# code that uses m6.
if [[ -n "$EXAMPLES" ]]; then
    info "examples: build, warnings, clippy, tests, and the CMS end-to-end suite"
    # shellcheck disable=SC2086
    ssh $SSH_OPTS "$BUILD_HOST" "EG_DIR=$BUILD_ROOT/m6/examples M6_DIR=$BUILD_ROOT/m6 bash -s" \
        > "$EXAMPLES_LOG" 2>&1 <<'REMOTE_EG' || true
set -uo pipefail
export PATH=$PATH:$HOME/.cargo/bin:$HOME/.local/bin:/root/.cargo/bin:/root/.local/bin
# M6_DIR arrives relative to the login directory (BUILD_ROOT defaults to a
# relative path), and this half changes directory before it uses it, so it is
# made absolute first. Without this every example step, and the lock helper
# below, looked for m6 in a directory that does not exist.
M6_DIR="$(cd "$M6_DIR" && pwd)" || exit 1
cd "$EG_DIR" || exit 1
# This run's own files (see the m6 half above).
T="$(mktemp -d)" || exit 1
trap 'rm -rf -- "$T"' EXIT
# The host lock shared for the whole run (#344, tools/host-lock.sh). The builds
# and clippy below bind nothing; from the unit tests on, every step binds fixed
# loopback ports (8443 among them), so the port lock is held from there.
# shellcheck source=tools/host-lock.sh
. "$M6_DIR/tools/host-lock.sh" || exit 1
host_enter_shared

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
    | grep -c '^warning' > $T/eg-clippy.count
echo "EG_CLIPPY_WARNINGS=$(cat $T/eg-clippy.count)"
CARGO_TARGET_DIR=target/clippy cargo clippy --workspace --all-targets --release 2>&1 \
    | grep '^warning' | head -5

# Everything the remaining steps run is built first, outside the port lock, so
# the lock covers running and not compiling. The later builds are then no-ops.
cargo test --workspace --no-run > "$T/eg-testbuild.log" 2>&1 || true
cargo build --release -p render-cms > /dev/null 2>&1 || true
(cd "$M6_DIR" && cargo build --workspace --release > /dev/null 2>&1) || true
port_hold
echo "### examples unit tests"
# Not blog_site_end_to_end: it needs certificates and example 05's stack, and
# fails without them. It runs below, after that stack is up.
cargo test --workspace -- --skip blog_site_end_to_end > $T/eg-tests.log 2>&1
echo "EG_TEST_STATUS=$?"
grep -E '^test result:' $T/eg-tests.log || true
sed -n '/^failures:/,$p' $T/eg-tests.log | head -40

echo "### development certificates"
# A fresh checkout has no keys: dev-keys.sh writes them to /tmp/m6/dev,
# which the config parse below needs.
./dev-keys.sh
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
for d in */; do
    name=$(basename "$d")
    case "$name" in
        # The same skips as CI's "Every example's config parses" step in
        # .github/workflows/ci.yml: shared fixtures, the shared publisher
        # crate and the build output are not examples.
        data|common|target) continue ;;
        06-systemd)
            printf '  %-24s not parsed: systemd units only, it has no site config\n' "$name"
            continue ;;
        09-global-deployment)
            # It holds one system config per node and one site config per role,
            # so it has no single configs/system-dev.toml for this loop to parse.
            # Its own integration test in
            # 09-global-deployment/integration-test covers it instead,
            # and that test ran above.
            printf '  %-24s not parsed: per-node configs, no single system-dev.toml; its integration test covers it\n' "$name"
            continue ;;
    esac
    sys="$d/configs/system-dev.toml"
    [ -f "$sys" ] || sys="$d/site.toml"
    if [ ! -f "$sys" ]; then
        printf '  %-24s NO CONFIG FOUND and no reason recorded for that\n' "$name"
        EG_CONFIG_UNEXPLAINED=$((EG_CONFIG_UNEXPLAINED + 1))
        continue
    fi
    # The checked-in config names the key directory as @M6_DEV_HOME@; m6 reads
    # the copy with the real path filled in, exactly as CI does.
    sys=$(. ./dev-home.sh && m6_conf "$sys")
    if ! out=$("$M6_DIR/target/release/m6-http" "$(cd "$d" && pwd)" "$sys" --dump-config 2>&1 >/dev/null) || [ -n "$out" ]; then
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

cd 05-cms || exit 1

# ── The port has to be ours ───────────────────────────────────────────────────
#
# This example binds 127.0.0.1:8443. Anything else already holding that port, or
# holding 0.0.0.0:8443, which covers loopback, answers the readiness probe below
# exactly as well as the example would, and the suite then measures a server
# nobody has changed. A build host that doubles as a staging box has something on
# 8443 as a matter of course.
#
# Refused rather than detected later, because the symptom is unreadable: whether
# the example wins its bind is a race, so the same collision reports a different
# number of assertion failures each run, none of which mention a port.
if ss -ltnH 'sport = :8443' 2>/dev/null | grep -q .; then
    echo "EG_CMS_STATUS=1"
    echo "port 8443 is already bound on this host, so 05-cms cannot be measured:"
    ss -ltnpH 'sport = :8443' 2>/dev/null | sed 's/^/    /'
    echo "Stop whatever holds it, or move it, and run again."
    exit 0
fi

# Its processes do not inherit the locks, so one that outlives this run cannot
# hold the host or the ports. While this run lasts it holds the port lock itself,
# so no other run's tests bind ports beside the stack; a stack left over after a
# failed run is caught by the 8443 check above on the next examples run, which a
# wedged lock would not be.
M6="$M6_DIR" M6_NO_BROWSER=1 ./dev.sh --no-open > $T/eg-cms-dev.log 2>&1 6<&- 7<&- 8<&- &
DEV_PID=$!
DEV_START=$(ps -o lstart= -p "$DEV_PID" 2>/dev/null | sed 's/^[[:space:]]*//;s/[[:space:]]*$//' || true)
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
    tail -30 $T/eg-cms-dev.log
    echo "--- service logs ---"
    tail -20 logs/*.log 2>/dev/null
# A 200 says something is serving, not that it is the thing under test. Prove it
# is this example before believing 98 assertions about it: the home page lists
# recent posts, and nothing else on this host renders that.
elif ! curl -sk --http1.1 https://127.0.0.1:8443/ 2>/dev/null | grep -q 'Recent Posts'; then
    echo "EG_CMS_STATUS=1"
    echo "something answered 200 on 8443 but it is not 05-cms, so nothing here was measured:"
    ss -ltnpH 'sport = :8443' 2>/dev/null | sed 's/^/    /'
else
    ./test.sh > $T/eg-cms-test.log 2>&1
    cms_rc=$?
    grep -E 'passed|FAIL' $T/eg-cms-test.log | sed 's/\x1b\[[0-9;]*m//g' | tail -25
    # The Rust end-to-end test needs this stack, so it runs here and its
    # failure counts into the same status.
    echo "### 05-cms rust end-to-end"
    cargo test -p render-cms --test integration > $T/eg-cms-rust.log 2>&1
    rust_rc=$?
    # A test that cannot start its stack prints SKIP and passes; count it as a failure.
    if grep -q '^SKIP' $T/eg-cms-rust.log; then
        echo "rust end-to-end skipped:"
        grep '^SKIP' $T/eg-cms-rust.log
        rust_rc=1
    fi
    # No "test result:" line is echoed: the report below sums those lines for the
    # unit-test count and takes the last "N passed" as the suite's count.
    echo "rust end-to-end exit status $rust_rc"
    grep -E '^test .* FAILED|panicked' $T/eg-cms-rust.log | grep -v '^test result:' | tail -25
    if [ "$cms_rc" != "0" ] || [ "$rust_rc" != "0" ]; then
        echo "EG_CMS_STATUS=1"
    else
        echo "EG_CMS_STATUS=0"
    fi
fi
kill $DEV_PID 2>/dev/null
# dev.sh stops the services it recorded when its EXIT trap runs. Reap those
# recorded pids too, because a SIGKILL or a crash would leave the trap unrun.
# This used to sweep `pgrep -f "$(pwd)"`, which matched anything whose command
# line mentioned the directory, and that is another run's stack as easily as
# this one.
if [ -f ../own.sh ]; then
    . ../own.sh
    own_init "$(pwd)" stack
    own_stop_records
    own_forget
fi
i=0
while [ "$i" -lt 50 ]; do
    kill -0 "$DEV_PID" 2>/dev/null || break
    # A child this shell has not reaped is a zombie: kill -0 still succeeds.
    st=$(ps -o stat= -p "$DEV_PID" 2>/dev/null | tr -d ' ' || true)
    case "$st" in Z*) break ;; esac
    sleep 0.1
    i=$((i + 1))
done
if kill -0 "$DEV_PID" 2>/dev/null; then
    now=$(ps -o lstart= -p "$DEV_PID" 2>/dev/null | sed 's/^[[:space:]]*//;s/[[:space:]]*$//' || true)
    if [ "${DEV_START:--}" != "-" ] && [ -n "$now" ] && [ "$now" != "$DEV_START" ]; then
        echo "dev.sh pid $DEV_PID is a different process now; leaving it alone"
    else
        kill -9 "$DEV_PID" 2>/dev/null || true
    fi
fi
REMOTE_EG
fi

# ── Report ───────────────────────────────────────────────────────────────────
FAILED=$COPY_BACK_FAILED
rw=$(num_after RELEASE_WARNINGS "$LOG");  rw="${rw:-?}"
tw=$(num_after TESTBUILD_WARNINGS "$LOG"); tw="${tw:-?}"
cl=$(num_after CLIPPY_STATUS "$LOG");      cl="${cl:-?}"
rd=$(num_after RUSTDOC_STATUS "$LOG");     rd="${rd:-?}"
ra=$(num_after RUSTDOC_ALL_STATUS "$LOG"); ra="${ra:-?}"
dn=$(num_after DENY_STATUS "$LOG");        dn="${dn:-?}"
cf=$(num_after CONFORMANCE_STATUS "$LOG"); cf="${cf:-?}"
read -r passed failed < <(awk '/^test result:/{p+=$4; f+=$6} END{print p+0, f+0}' "$LOG")

[[ "$rw" == "0" ]] || { echo "${RED}   $rw release warning(s) on Linux${RESET}" >&2; FAILED=1; }
[[ "$tw" == "0" ]] || { echo "${RED}   $tw test-build warning(s) on Linux${RESET}" >&2; FAILED=1; }
tbs=$(num_after TESTBUILD_STATUS "$LOG"); tbs="${tbs:-?}"
[[ "$tbs" == "0" ]] || { echo "${RED}   the test build failed on Linux (see the build log)${RESET}" >&2; FAILED=1; }
fg=$(num_after FEATURE_GATED_STATUS "$LOG"); fg="${fg:-?}"
[[ "$fg" == "0" ]] || { echo "${RED}   feature-gated tests failed (the targets --workspace omits)${RESET}" >&2
                        sed -n '/^### feature-gated tests/,/^### conformance/p' "$LOG" >&2; FAILED=1; }
[[ "$cl" == "0" ]] || { echo "${RED}   clippy is not silent on Linux${RESET}" >&2
                        grep -A6 '^### clippy' "$LOG" >&2; FAILED=1; }
[[ "$rd" == "0" ]] || { echo "${RED}   rustdoc: a broken intra-doc link (default features)${RESET}" >&2
                        sed -n '/^### rustdoc/,/^### cargo-deny/p' "$LOG" >&2; FAILED=1; }
[[ "$ra" == "0" ]] || { echo "${RED}   rustdoc: a broken intra-doc link (--all-features)${RESET}" >&2
                        sed -n '/^### rustdoc/,/^### cargo-deny/p' "$LOG" >&2; FAILED=1; }
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
    ok "m6: $passed passed, 0 failed, 0 warnings (release + test), clippy silent, rustdoc silent, cargo-deny ok, h1+h2+h3 ok, performance ok"
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
    read -r egpassed _ < \
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

# ── Disk: report it, and say what would reclaim it ───────────────────────────
#
# `cargo test --workspace` above is what fills target/debug, and cargo NEVER
# garbage-collects it: every dependency bump and toolchain change leaves its
# artefacts behind for good. On 2026-09-15 the build host was at 73% with **65G**
# of target/debug across 5023 files in deps and 2311 fingerprint directories, while
# the release trees that deploys actually use were 6G. Clearing it took the box from
# 104G used to 37G.
#
# Reported rather than deleted. This script is a GATE, and a gate that quietly
# removes 65G of build cache makes the next run slow for reasons the operator did
# not choose. The number and the command are enough.
# TWO BUGS FIXED HERE ON 2026-09-27, both found by reading the output against the
# box. This block looked at a HARDCODED /root/build, so a run with M6_BUILD_ROOT
# set elsewhere reported "debug caches in 0 tree(s)" while 12 GB sat in the tree
# it had just built. And its awk summed `du -sh` output, which is strings like
# "9.9G", then printed only the count and never used the total.
#
# It reports $BUILD_ROOT now, and it reports the SIZE in megabytes, which is a
# number that can be compared with the next run's.
# shellcheck disable=SC2086
DISK="$(ssh $SSH_OPTS "$BUILD_HOST" "df -h / | awk 'NR==2 {print \$5\" of \"\$2\" used\"}'" 2>/dev/null)"
# shellcheck disable=SC2086
DEBUG_SZ="$(ssh $SSH_OPTS "$BUILD_HOST" \
    "du -sm $BUILD_ROOT/*/target/debug 2>/dev/null | awk '{s+=\$1; n++} END {if (n) printf \"%d tree(s), %.1f GB\", n, s/1024}'" \
    2>/dev/null)"
echo
echo "   build host disk: ${DISK:-unknown}${DEBUG_SZ:+, debug caches: $DEBUG_SZ}"
case "${DISK%% *}" in
    8[0-9]%|9[0-9]%|100%)
        echo "${YELLOW}   above 80%. Reclaim the debug caches, which no deploy reads:${RESET}"
        echo "${YELLOW}     ssh $SSH_OPTS $BUILD_HOST 'rm -rf $BUILD_ROOT/*/target/debug'${RESET}" ;;
esac

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
