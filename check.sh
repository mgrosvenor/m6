#!/usr/bin/env bash
# check.sh — full CI gate: correctness → performance snapshot
#
# Usage:
#   ./check.sh               # run everything
#   ./check.sh --no-bench    # skip benchmarks (faster iteration)
#   ./check.sh --save-baseline  # run benches and save as the comparison baseline
#
# Order of operations:
#   1. Build (release)
#   2. Clippy minimum-score check — BLOCKS the push
#   3. Unit + integration tests
#   4. Conformance minimum-score check (h1spec / h2spec / h3spec) — BLOCKS the push
#   5. Benchmarks (informational — prints criterion output; never blocks the push)
#
# Why benches don't gate:
#   Sub-microsecond criterion benchmarks on a development machine have ±5-15%
#   run-to-run noise from CPU scheduling, thermal state, and background load.
#   Using them as a hard gate produces frequent false positives.  They are run
#   here so the output is visible in the pre-push log; inspect it manually if
#   you suspect a real regression.  A >30% change in a cache-hot benchmark
#   warrants investigation.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
cd "$SCRIPT_DIR"

# ── Args ──────────────────────────────────────────────────────────────────────
RUN_BENCH=true
SAVE_BASELINE=false
for arg in "$@"; do
  case "$arg" in
    --no-bench)       RUN_BENCH=false ;;
    --save-baseline)  SAVE_BASELINE=true ;;
    *) echo "Unknown arg: $arg"; exit 1 ;;
  esac
done

BASELINE_NAME="check"

# Criterion bench targets (harness=false; accept --baseline / --save-baseline).
# Listed individually to avoid running inline #[bench] items that use the
# standard harness and reject unknown flags.
CRITERION_BENCHES=(
  -p m6-file   --bench critical_path
  -p m6-http   --bench critical_path
  -p m6-core   --bench critical_path
)

# ── Colours ───────────────────────────────────────────────────────────────────
GREEN='\033[0;32m'; RED='\033[0;31m'; YELLOW='\033[1;33m'; RESET='\033[0m'
pass() { echo -e "${GREEN}PASS${RESET} $1"; }
# The last line a reader sees, and the one that used to lie. m6 #166.
summary() {
  echo ""
  if [[ -n "${INCOMPLETE:-}" ]]; then
    echo -e "${YELLOW}Checks INCOMPLETE: ${INCOMPLETE} did not run in full.${RESET}"
    echo -e "${YELLOW}This run is not a gate. tools/build-host-tests.sh is.${RESET}"
  else
    echo -e "${GREEN}All checks passed.${RESET}"
  fi
}
fail() { echo -e "${RED}FAIL${RESET} $1"; exit 1; }
info() { echo -e "${YELLOW}----${RESET} $1"; }
warn() { echo -e "${YELLOW}WARN${RESET} $1"; }

# ── 1. Build ──────────────────────────────────────────────────────────────────
info "Building (release)..."
cargo build --workspace --release --quiet
pass "Build"

# ── 2. Clippy ─────────────────────────────────────────────────────────────────
# A minimum-score check, not `-D warnings`: the count may fall and may never rise. See
# tools/clippy.sh for why, and tools/clippy-ceiling.txt for where it stands.
# The rustc zero-warnings rule is separate and absolute.
info "Running clippy..."
if ./tools/clippy.sh; then
  pass "Clippy (nothing new)"
else
  fail "Clippy regressed — see above"
fi

# ── 3. Correctness: unit + integration tests ──────────────────────────────────
# --test-threads=1: several integration suites (m6-http's edge_proxy.rs,
# security_e2e.rs, analytics_e2e.rs) spawn real m6-http/m6-html/m6-file
# processes bound to fixed loopback ports. Run concurrently with each other,
# their #[test] fns race for those ports and fail with spurious 502s that
# have nothing to do with the code under test.
info "Running correctness tests..."
if cargo test --workspace --quiet -- --test-threads=1 2>&1; then
  pass "Unit + integration tests (HTTP/1.1 ✓  HTTP/3 ✓)"
else
  fail "Test suite failed — fix correctness issues before performance check"
fi

# ── 4. Conformance: HTTP/1.1, HTTP/2, HTTP/3 ─────────────────────────────────
# A minimum score against independent testers (h1spec, h2spec, h3spec). Floors live
# in tools/conformance-scores.txt; a score below its floor fails the push.
#
# This gates because HTTP/1.1 went untested for the whole life of the project
# while h2 and h3 were held at 146/146 and 37/49, and the divergence was
# exactly what you would predict: four HTTP/1.1 parsers, three conventions for
# header-name case, none checking Transfer-Encoding, and m6-file returning a
# body on a HEAD that 404s -- a defect fixed in m6-http months earlier and
# never applied to the other implementation, because nothing measured it.
#
# `--allow-missing-tools` is for THIS gate only, and it is not a pass: an
# absent tester is reported as NOT TESTED, and the summary says "Checks
# INCOMPLETE" rather than "All checks passed". A laptop without h2spec still gets
# the h1 gate, and the run says plainly what it did not do.
#
# THAT SECOND SENTENCE WAS FALSE UNTIL 2026-09-27. It claimed the summary refused
# to say pass while the code printed PASS and then "All checks passed." on a run
# that tested neither h2 nor h3. m6 #166. A comment describing an intention next
# to code doing something else is read as the code, which is the third time this
# repository has paid for that shape.
#
# The full checks (`tools/build-host-tests.sh`, on the Linux build host where both
# testers are installed) runs WITHOUT the flag, so the path to a deploy cannot
# skip h2 or h3. That split is the whole design: skipping is a laptop
# convenience and never a way to ship.
info "Checking formatting..."
if cargo fmt --all --check >/dev/null 2>&1; then
  pass "Formatting (cargo fmt)"
else
  fail "Formatting: run 'cargo fmt --all'"
  cargo fmt --all --check 2>&1 | head -20
fi

# A SKIPPED TESTER MUST NOT BECOME "All checks passed." -- m6 #166.
#
# tools/conformance.sh prints "INCOMPLETE, NOT TESTED" and "do not read it as a
# pass", then exits 0, because with --allow-missing-tools nothing regressed: it
# just never ran. This block used to test only the exit code, print PASS, and let
# the script end on "All checks passed." having tested neither h2 nor h3. The
# comment above claimed the summary "refuses to say pass". It did not.
#
# Now the skip is carried to the end of the run in INCOMPLETE and the summary says
# so. The exit code stays 0 on a laptop, deliberately: a developer's run is still
# useful and still not a gate.
INCOMPLETE=""
info "Running conformance (h1spec / h2spec / h3spec)..."
CONF_OUT="$(mktemp)"
if ./tools/conformance.sh --allow-missing-tools 2>&1 | tee "$CONF_OUT"; then
  if grep -q 'NOT TESTED' "$CONF_OUT"; then
    NOT_TESTED="$(grep -o 'NOT TESTED:.*' "$CONF_OUT" | tail -1)"
    warn "Conformance INCOMPLETE — ${NOT_TESTED:-a tester was absent}"
    INCOMPLETE="conformance"
  else
    pass "Conformance (every target measured, nothing went backwards)"
  fi
else
  fail "Conformance regressed — see above, and tools/conformance-scores.txt"
fi
rm -f "$CONF_OUT"

# ── 5. Performance (informational) ────────────────────────────────────────────
if [[ "$RUN_BENCH" == "false" ]]; then
  info "Skipping benchmarks (--no-bench)"
  summary
  exit 0
fi

if [[ "$SAVE_BASELINE" == "true" ]]; then
  info "Saving benchmark baseline '$BASELINE_NAME'..."
  if cargo bench "${CRITERION_BENCHES[@]}" --quiet -- --save-baseline "$BASELINE_NAME" 2>&1; then
    pass "Baseline '$BASELINE_NAME' saved"
  else
    fail "Benchmark run failed during baseline save"
  fi
  echo ""
  echo -e "${GREEN}Baseline saved. Future runs will compare against it.${RESET}"
  exit 0
fi

info "Running benchmarks (informational — will not block push)..."

# THE BASELINE'S AGE IS PRINTED, OR THE BASELINE IS NOT USED -- m6 #167.
#
# target/criterion is untracked and `cargo clean` erases it, so this comparison
# silently ages. One was found six months old, and check.sh's header tells a
# reader to investigate any change over 30%: a documentation-only change printed
# +562% against it. A percentage against an undated artefact is worse than no
# percentage.
#
# Over CRITERION_BASELINE_MAX_DAYS the comparison is dropped rather than shown,
# because a stale number invites exactly the investigation it cannot support.
CRITERION_BASELINE_MAX_DAYS="${CRITERION_BASELINE_MAX_DAYS:-14}"
BASELINE_DIR="target/criterion/$BASELINE_NAME"
if [[ -d "$BASELINE_DIR" ]]; then
  # stat differs between GNU and BSD, so try both and fall back to using it.
  BASE_EPOCH="$(stat -c %Y "$BASELINE_DIR" 2>/dev/null || stat -f %m "$BASELINE_DIR" 2>/dev/null || echo 0)"
  BASE_DAYS=$(( ( $(date +%s) - BASE_EPOCH ) / 86400 ))
  if (( BASE_EPOCH > 0 && BASE_DAYS > CRITERION_BASELINE_MAX_DAYS )); then
    warn "criterion baseline '$BASELINE_NAME' is ${BASE_DAYS} days old — NOT comparing against it"
    info "  it lives in target/criterion, which is untracked. Re-take it with --save-baseline"
    BENCH_ARGS=""
  else
    info "comparing against criterion baseline '$BASELINE_NAME', ${BASE_DAYS} days old"
    BENCH_ARGS="-- --baseline $BASELINE_NAME"
  fi
else
  info "No baseline yet — run './check.sh --save-baseline' to create one"
  BENCH_ARGS=""
fi

BENCH_OUT="$(mktemp)"
# Run benchmarks; capture output but do not fail on non-zero exit.
cargo bench "${CRITERION_BENCHES[@]}" --quiet $BENCH_ARGS 2>&1 | tee "$BENCH_OUT" || true

# Surface any criterion-detected regressions as warnings (not failures).
REGRESSIONS=$(grep "Performance has regressed\." "$BENCH_OUT" | wc -l | tr -d ' ')
if [[ "$REGRESSIONS" -gt 0 ]]; then
  warn "$REGRESSIONS benchmark(s) flagged by criterion — inspect output above"
else
  pass "Benchmarks (no criterion regressions against baseline)"
fi

rm -f "$BENCH_OUT"

summary
