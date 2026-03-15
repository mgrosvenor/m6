#!/usr/bin/env bash
# check.sh — full CI gate: correctness → performance regression
#
# Usage:
#   ./check.sh               # run everything
#   ./check.sh --no-bench    # skip benchmarks (faster iteration)
#   ./check.sh --save-baseline  # run benches and save as the regression baseline
#
# Order of operations:
#   1. Build (release)
#   2. Unit + integration tests  ← HTTP/1.1 and HTTP/3 covered here
#   3. Benchmark regression check (compare against saved named baseline)
#
# Baseline management:
#   Criterion stores a named baseline called "check" in target/criterion/.
#   Run `./check.sh --save-baseline` after an intentional improvement to
#   update it.  Until a baseline exists, the bench step runs but does not
#   compare (safe for fresh clones).

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

# ── Colours ───────────────────────────────────────────────────────────────────
GREEN='\033[0;32m'; RED='\033[0;31m'; YELLOW='\033[1;33m'; RESET='\033[0m'
pass() { echo -e "${GREEN}PASS${RESET} $1"; }
fail() { echo -e "${RED}FAIL${RESET} $1"; exit 1; }
info() { echo -e "${YELLOW}----${RESET} $1"; }

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

# ── 3. Performance ────────────────────────────────────────────────────────────
if [[ "$RUN_BENCH" == "false" ]]; then
  info "Skipping benchmarks (--no-bench)"
  exit 0
fi

# Criterion bench targets (harness=false; accept --baseline / --save-baseline).
# Specified as --bench <name> to avoid running inline #[bench] items that use
# the standard harness and reject unknown flags.
CRITERION_BENCHES=(
  -p m6-file   --bench critical_path
  -p m6-http   --bench critical_path
  -p m6-render --bench critical_path
)

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

info "Running benchmarks (comparing against baseline '$BASELINE_NAME')..."
BENCH_OUT="$(mktemp)"

# Use named baseline if it exists; otherwise run without comparison (first use).
BASELINE_DIR="target/criterion/$BASELINE_NAME"
if [[ -d "$BASELINE_DIR" ]]; then
  BENCH_ARGS="-- --baseline $BASELINE_NAME"
else
  info "No baseline found — running without comparison (use --save-baseline to create one)"
  BENCH_ARGS=""
fi

if ! cargo bench "${CRITERION_BENCHES[@]}" --quiet $BENCH_ARGS 2>&1 | tee "$BENCH_OUT"; then
  rm -f "$BENCH_OUT"
  fail "Benchmark run failed"
fi

# Criterion prints "Performance has regressed." for statistically-significant
# slowdowns when comparing against a named baseline.
if grep -q "Performance has regressed\." "$BENCH_OUT"; then
  echo ""
  echo -e "${RED}Performance regression detected:${RESET}"
  grep -B2 "Performance has regressed\." "$BENCH_OUT" | grep -v "^--$" || true
  rm -f "$BENCH_OUT"
  fail "Performance regression — profile before merging"
fi

rm -f "$BENCH_OUT"
pass "Benchmarks (no regression)"

echo ""
echo -e "${GREEN}All checks passed.${RESET}"
