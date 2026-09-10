#!/usr/bin/env bash
# Paired, interleaved A/B. One bench, alternating trees, many rounds.
#
# The first attempt ran A,A,A,A then B,B,B,B and the same tree measured 21.96 ns
# in one pass and 20.41 ns in the next: ~7% drift with no code change. Any delta
# smaller than that is invisible to a design that measures all of A before any
# of B. Alternating puts the drift into both arms equally, so it cancels in the
# paired difference instead of landing entirely on one side.
export PATH=$HOME/.cargo/bin:$PATH
BENCH="${1:-cache_hit}"
ROUNDS="${2:-10}"
ARGS="--warm-up-time 1 --measurement-time 3 --sample-size 30"

echo "== building =="
for t in m6-base m6-cand; do
  cd "$HOME/build/$t" && cargo build --release --quiet --benches -p m6-http 2>&1 | tail -1
done

echo "== waiting for idle =="
while true; do
  L=$(cut -d' ' -f1 /proc/loadavg)
  awk -v l="$L" 'BEGIN{exit !(l < 0.35)}' && break
  sleep 20
done

echo "== $BENCH, $ROUNDS interleaved rounds =="
for r in $(seq 1 "$ROUNDS"); do
  for t in m6-base m6-cand; do
    v=$(cd "$HOME/build/$t" && cargo bench -p m6-http --bench critical_path --quiet -- $ARGS "$BENCH" 2>&1 \
        | grep -oE 'time: +\[[^]]*\]' | head -1 | grep -oE '[0-9.]+ [num]s' | sed -n 2p)
    echo "r$r $t $v"
  done
done
echo "===== DONE ====="
