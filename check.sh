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
#   2. Unit + integration tests  ← HTTP/1.1 and HTTP/3 covered here
#   3. Benchmarks (informational — prints criterion output; never blocks the push)
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
  -p m6-render --bench critical_path
)

# ── Colours ───────────────────────────────────────────────────────────────────
GREEN='\033[0;32m'; RED='\033[0;31m'; YELLOW='\033[1;33m'; RESET='\033[0m'
pass() { echo -e "${GREEN}PASS${RESET} $1"; }
fail() { echo -e "${RED}FAIL${RESET} $1"; exit 1; }
info() { echo -e "${YELLOW}----${RESET} $1"; }
warn() { echo -e "${YELLOW}WARN${RESET} $1"; }

# ── 1. Build ──────────────────────────────────────────────────────────────────
info "Building (release)..."
cargo build --workspace --release --quiet
pass "Build"

# ── 2. Correctness: unit + integration tests ──────────────────────────────────
info "Running correctness tests..."
if cargo test --workspace --quiet 2>&1; then
  pass "Unit + integration tests (HTTP/1.1 ✓  HTTP/3 ✓)"
else
  fail "Test suite failed — fix correctness issues before performance check"
fi

# ── 3. Performance (informational) ────────────────────────────────────────────
if [[ "$RUN_BENCH" == "false" ]]; then
  info "Skipping benchmarks (--no-bench)"
  echo ""
  echo -e "${GREEN}All checks passed.${RESET}"
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

BASELINE_DIR="target/criterion/$BASELINE_NAME"
if [[ -d "$BASELINE_DIR" ]]; then
  BENCH_ARGS="-- --baseline $BASELINE_NAME"
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

echo ""
echo -e "${GREEN}All checks passed.${RESET}"
