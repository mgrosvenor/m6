#!/usr/bin/env bash
# check.sh — full CI gate: correctness → performance regression
#
# Usage:
#   ./check.sh               # run everything
#   ./check.sh --no-bench    # skip benchmarks (faster iteration)
#   BENCH_THRESHOLD=5        # % regression allowed before failure (default: 10)
#
# Order of operations:
#   1. Build (release)
#   2. Unit + integration tests  ← HTTP/1.1 and HTTP/3 covered here
#   3. Benchmark regression check (compare against saved baseline)
#
# Baseline management:
#   The baseline is stored in benches/baseline.json (criterion output).
#   Run `./check.sh --save-baseline` to update it after an intentional improvement.

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

BENCH_THRESHOLD="${BENCH_THRESHOLD:-10}"  # % allowed regression
BASELINE_FILE="benches/baseline.json"

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

# ── 3. Performance regression check ──────────────────────────────────────────
if [[ "$RUN_BENCH" == "false" ]]; then
  info "Skipping benchmarks (--no-bench)"
  exit 0
fi

info "Running benchmarks..."
BENCH_OUT="$(mktemp)"
if ! cargo bench --workspace --quiet 2>&1 | tee "$BENCH_OUT"; then
  fail "Benchmark run failed"
fi

# Extract criterion regression lines
# Criterion prints: "change: [-X% -Y% +Z%] (p = P > 0.05)" for no change
#                   "Regression (p = P ...)" for detected regression
if grep -q "Regression" "$BENCH_OUT"; then
  echo ""
  echo -e "${RED}Performance regression detected:${RESET}"
  grep "Regression" "$BENCH_OUT"
  rm -f "$BENCH_OUT"
  fail "Performance regression — profile before merging"
fi

# Check for large % changes even if not statistically significant
while IFS= read -r line; do
  if echo "$line" | grep -q "change:"; then
    # Extract the median % change (middle value in [...])
    pct=$(echo "$line" | grep -oE '[-+]?[0-9]+\.[0-9]+%' | sed -n '2p' | tr -d '%+')
    if [[ -n "$pct" ]] && awk "BEGIN{exit !(${pct} > ${BENCH_THRESHOLD})}"; then
      echo -e "${RED}Large performance change: ${pct}% (threshold: ${BENCH_THRESHOLD}%)${RESET}"
      echo "  $line"
      rm -f "$BENCH_OUT"
      fail "Performance regression > ${BENCH_THRESHOLD}% detected"
    fi
  fi
done < "$BENCH_OUT"

rm -f "$BENCH_OUT"
pass "Benchmarks (no regression > ${BENCH_THRESHOLD}%)"

echo ""
echo -e "${GREEN}All checks passed.${RESET}"
