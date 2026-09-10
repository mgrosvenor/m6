# m6 benchmarks

Every number here is reproducible from the command lines given. If a figure
appears anywhere else in this repository without the conditions that produced
it, treat the figure as wrong and this file as authoritative.

## Why this file was rewritten (2026-09-06)

An external audit found the published throughput figures not credible, and it
was right. The README carried **two tables claiming the same conditions** ("8
concurrent connections, TLS, warm cache") with irreconcilable values:

| protocol | table A | table B | ratio |
|---|---:|---:|---:|
| HTTP/1.1 | 8,840 req/s | 11,857 req/s | 1.3× |
| HTTP/2 | 28,797 req/s | 158,323 req/s | **5.5×** |
| HTTP/3 | 61,748 req/s | 77,672 req/s | 1.3× |

At least one was wrong and a reader had no way to tell which. Neither recorded
the commit, the hardware, the payload, or the command line, so neither could be
checked. They have been deleted rather than reconciled.

**Measured on real hardware, they are both far too high** — see the results
below. The most likely explanation is the methodology bug found while
re-measuring, described next.

## The methodology bug worth knowing about

The harness defaults to `--concurrency 8`. On a 4-core machine the load
generator and the server **share those cores**, so at concurrency 8 the client
starves the server it is measuring. The symptom is not a plausible-looking
lower number; it is instability. Two back-to-back runs of the identical command
disagreed by **50×** on the same metric:

    cache-hit→m6-file (H2) p50:   89 µs   (run 1)
                                4199 µs   (run 2)

At `--concurrency 2`, leaving cores for the server, the same metric reproduces
within 15% and usually within 7%. **Every number below is at concurrency 2 for
that reason.** A benchmark that cannot be repeated is not a measurement, and
co-locating the load generator with the server is the constraint that makes
this necessary — a separate load-generator host would allow higher concurrency
and would report higher throughput.

Two other traps hit during the same session, both of which silently corrupt
results:

- **The rate limiter counts.** The first run was full of `429`s because staging
  inherits production's `requests_per_min = 1200`. It was measuring the rate
  limiter, not the server. Raised for the run and restored afterwards.
- **`/tail/hello.txt` 404s** on a normal site config; that suite is measuring an
  error path unless the bench backend is running.

## Conditions

| | |
|---|---|
| commit | `0a68cda` |
| host | Vultr VPS, 4 vCPU Intel Xeon (Skylake, IBRS), 7 GB RAM |
| OS | Ubuntu 26.04.1, kernel 7.0.0-30-generic |
| toolchain | rustc 1.98.0, `cargo build --release` |
| release profile | cargo defaults: `opt-level=3`, `lto=false`, `codegen-units=16`, `panic=unwind` |
| TLS | rustls, TLS 1.3, self-signed cert, `--skip-verify` on the client |
| topology | **client and server on the same host, over loopback** |
| concurrency | 2 (see above) |
| latency samples | 2000 per suite |
| throughput window | 10 s |
| CPU governor | not exposed on this VM — frequency scaling is not controlled |

The server under test was the staging m6-http, serving the real
mgrosvenor.com content, so the payloads are real pages rather than a synthetic
fixture.

    cargo build --release --bin m6-bench
    ./target/release/m6-bench --skip-verify --addr 127.0.0.1:443 \
        --concurrency 2 --duration 10

## Results

Two consecutive runs, both reported, so the spread is visible rather than
averaged away.

### Latency, µs (end-to-end, client-observed)

| suite | p50 run 1 | p50 run 2 | p99 run 1 | p99 run 2 |
|---|---:|---:|---:|---:|
| HTTP/1.1 cache-hit → m6-html | 687.8 | 679.1 | 1011.0 | 951.4 |
| HTTP/1.1 cache-miss → m6-html | 4797.7 | 5063.1 | 6223.5 | 6526.4 |
| **HTTP/2 cache-hit → m6-html** | **225.7** | **259.2** | 318.2 | 369.9 |
| HTTP/2 cache-miss → m6-html | 4096.5 | 4159.8 | 5394.5 | 5321.6 |
| HTTP/3 cache-hit → m6-html | 953.6 | 983.3 | 1344.5 | 1339.5 |
| HTTP/3 cache-miss → m6-html | 4617.0 | 4329.3 | 5844.5 | 5525.9 |

### Throughput, req/s

| protocol | run 1 | run 2 |
|---|---:|---:|
| HTTP/1.1 | 2085.1 | 1889.3 |
| HTTP/2 | 7154.1 | 6711.2 |
| HTTP/3 | 1030.3 | 1047.0 |

## Reading these correctly

**The cache-hit latency here is not the same quantity as the "2.2 µs cache hit"
measured in production, and conflating the two is probably how the original
figures drifted.**

- **~230 µs (H2 above)** is end-to-end, client-observed: TLS record processing
  on both sides, loopback syscalls, the client's own work, and the server's.
- **~2.2 µs** is m6's internal timer around the cache lookup and response
  construction — reported by the running server as `hit_p50_ns` and confirmed
  repeatedly on production.

Both are true. The first is what a client experiences on this hardware; the
second is what the cache costs. Quoting the second as though it were the first
would be dishonest, and quoting the first as though it were the cache's cost
understates it by two orders of magnitude.

**A cache miss costs ~4 ms**, dominated by the backend render, not by m6.

**HTTP/2 is roughly 3× faster than HTTP/1.1** here on both latency and
throughput, which is the one qualitative claim these numbers support well.
HTTP/3 is the slowest on throughput in this environment; that is a real result
on a loopback co-located test and should not be generalised to a real network,
where QUIC's advantages are about loss and RTT rather than raw local
throughput.

## What is deliberately not claimed

- **No comparison against nginx, H2O or LiteSpeed.** The previous README quoted
  such comparisons without having run them like-for-like. Numbers taken from
  other people's blog posts, on other hardware, with other payloads, are not a
  comparison.
- **No multi-core scaling figures.** Not measured.
- **No claim about a real network.** Everything here is loopback.
- **No "sub-millisecond" marketing claim.** The internal cache hit is
  microseconds; what a visitor experiences over the internet is dominated by
  RTT and is tens to hundreds of milliseconds. Both facts belong together or
  neither should be quoted.

## Criterion microbenchmarks

Pure CPU cost, no I/O, and therefore stable and worth keeping:

| operation | median |
|---|---:|
| `make_lookup_key` | 10.4 ns |
| `cache_hit` | 21.5 ns |
| `cache_miss` | 14.7 ns |
| `stats_record` | 0.9 ns |

These were not re-measured in the 2026-09-06 pass; they are carried over and
should be re-run on the build host before being quoted as current.

---

## Where to run them, and why it matters

**Diagnosed 2026-09-10.** The `critical_path` benches were producing a 19-26%
spread between consecutive runs of the same commit, while criterion's
confidence interval *within* each run was +/-0.08%. Internally precise,
externally scattered: the signature of a per-process constant, not of noise.

Two hypotheses were tested and one was wrong.

**Wrong: the hash seed.** `Cache` uses `AHashMap`, and `ahash`'s `RandomState`
is seeded from the OS once per process, so the same key lands in a different
bucket every run. Plausible, and false. `Cache::with_fixed_seed_for_bench()`
pins the seed; the spread was unchanged at 49-62 ns over five runs. The
constructor was kept anyway, because determinism is worth having, but it fixed
nothing.

**Right: core scheduling on a loaded heterogeneous machine.**

| Configuration | Result | Spread |
|---|---|---|
| Forced to efficiency cores (`taskpolicy -c background`) | 107.1, 108.0 ns | **+/-1%** |
| Default QoS, mixed P/E | 49-62 ns | +/-13% |

The development machine is an Apple M4: **4 performance cores and 6 efficiency
cores**. Efficiency cores run this workload **2.1x slower**, and pinning to
them is reproducible to +/-1%. The benchmark was never broken; the scheduling
was the variable.

The machine was also not idle when this was noticed. Load average was 89 rising
to 151 on ten cores, with `spotlightknowledged` at 88.8% CPU and 112 Firefox
`plugin-container` processes. Under that load macOS migrates a process across
core classes, and a run lands anywhere between the P-core figure and the E-core
figure depending on what share it happened to get.

### The rule

**Comparative benchmark numbers come from the build host**, which is a
dedicated Linux VM with homogeneous cores, nothing else running, and the same
OS as production. That is where a figure quoted as a regression check must come
from.

A developer machine is fine for a quick look, with two conditions:

1. **Check the load first.** `uptime`. A load average above the core count
   means the numbers are not comparable to anything.
2. **If you must compare on a heterogeneous machine, pin the core class.**
   `taskpolicy -c background` gives +/-1% reproducibility on an M-series Mac.
   The absolute figure is then 2.1x the real one and must never be quoted, but
   run-to-run comparison is valid, which is what regression detection needs.

### The general lesson

A benchmark whose consecutive runs of unchanged code disagree by more than a
percent or two is not measuring the code. Diagnose it before quoting it. The
tell here was the gap between the within-run interval (+/-0.08%) and the
between-run spread (+/-13%): whatever varied was fixed for the life of a
process and different between processes, which pointed at the environment
rather than at the benchmark or the code.
