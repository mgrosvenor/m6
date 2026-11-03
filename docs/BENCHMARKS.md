# m6 benchmarks

This page reports m6's benchmark figures with the conditions recorded for each set, and section 11 names the sets where that record is thin.
This page is the authority for what a client observes end to end.
`docs/PERFORMANCE.md` owns what one component costs, and `tools/perf-baseline.txt` owns the numbers a merge is gated on.
A figure quoted anywhere else without its conditions is wrong, and this page is where the figure is checked.

The page opens with why it was rewritten and with the methodology bug that explains the older claims.
It then gives the conditions and the end-to-end results, how to read the latency columns, and what the figures leave unclaimed.
The last four sections cover the criterion microbenchmarks, how to run a comparison that survives a busy machine, the six backend examples, and the summary.
Section 1 lists the sections.

## 1. Contents

The sections follow the order a reader needs them, from why the page exists to what it does not establish.

2. [Why this file was rewritten (2026-09-06)](#2-why-this-file-was-rewritten-2026-09-06)
3. [The methodology bug: concurrency and co-location](#3-the-methodology-bug-concurrency-and-co-location)
4. [Conditions behind the end-to-end figures](#4-conditions-behind-the-end-to-end-figures)
5. [End-to-end results](#5-end-to-end-results)
6. [Reading the latency figures](#6-reading-the-latency-figures)
7. [What these figures do not claim](#7-what-these-figures-do-not-claim)
8. [Criterion microbenchmarks](#8-criterion-microbenchmarks)
9. [Where to run a comparative benchmark](#9-where-to-run-a-comparative-benchmark)
10. [Backend examples, direct to socket](#10-backend-examples-direct-to-socket)
11. [Summary](#11-summary)

Section 2 begins with the 2026-09-06 rewrite, because every later figure depends on the methodology that rewrite established.

## 2. Why this file was rewritten (2026-09-06)

An external audit found the published throughput figures not credible, and it was right.
The README carried two tables that claimed the same conditions, 8 concurrent connections, TLS and a warm cache, with values that cannot both be true.
Table 1 gives the two tables and the ratio between them.

| protocol | table A | table B | ratio |
|---|---:|---:|---:|
| HTTP/1.1 | 8,840 req/s | 11,857 req/s | 1.3x |
| HTTP/2 | 28,797 req/s | 158,323 req/s | **5.5x** |
| HTTP/3 | 61,748 req/s | 77,672 req/s | 1.3x |

**Table 1: two published tables that claimed identical conditions, and the ratio between them.**
The HTTP/2 row differs by 5.5x, so one of the two tables was wrong and a reader could not tell which.

Both published tables are too high.
The figures in section 5 are at least 4x below table A on every row, and 58x below it on HTTP/3.
Neither table recorded the commit, the hardware, the payload or the command line, so neither could be checked or repaired.
Both are deleted.

The explanation is the methodology bug in section 3, which is a property of the harness.
Section 3 gives the bug, and section 5 gives the figures that replaced the deleted tables.

## 3. The methodology bug: concurrency and co-location

The harness default is the bug.
`m6-bench` defaults to `--concurrency 8`, and on a 4-core machine the load generator and the server share those cores, so at concurrency 8 the client starves the server it is measuring.

The symptom is instability.
Two back-to-back runs of the identical command disagreed by 50x on the same metric:

```text
cache-hit to m6-file over HTTP/2, p50    89 µs    (run 1)
                                       4199 µs    (run 2)
```

At `--concurrency 2`, which leaves cores for the server, the same metric reproduces within 15%.
Every figure in sections 4 and 5 is at concurrency 2 for that reason.

A benchmark that cannot be repeated is noise.
Co-locating the load generator with the server is the constraint that makes the low concurrency necessary.
A separate load-generator host would allow higher concurrency and would report higher throughput.

Two further traps corrupt a run without saying so.

- **The rate limiter counts.**
  The first run was full of 429s because the configuration carried a 1,200 requests-per-minute limit.
  That run measured the rate limiter.
  The limit was raised for the run and restored afterwards.
- **`/tail/hello.txt` answers 404** on a normal site configuration, so that suite measures an error path until the bench backend is running.

Both traps produce figures that look like results, and section 4 gives the conditions behind the figures that survived them.

## 4. Conditions behind the end-to-end figures

A figure means nothing without the machine, the build and the topology that produced it.
Table 2 gives the conditions behind every end-to-end figure in section 5.
The commit is the tree that was measured.

| condition | value |
|---|---|
| commit | `3204e0f` |
| host | *VPS* (virtual private server), 4 vCPU Intel Xeon (Skylake, IBRS), 7 GB *RAM* (random access memory) |
| operating system | Ubuntu 26.04.1, kernel 7.0.0-30-generic |
| toolchain | rustc 1.98.0, `cargo build --release` |
| release profile | cargo defaults at the cited commit: `opt-level=3`, `lto=false`, `codegen-units=16`, `panic=unwind` |
| *TLS* (Transport Layer Security) | rustls, TLS 1.3, self-signed certificate, `--skip-verify` on the client |
| topology | client and server on the same host, over loopback |
| concurrency | 2 (section 3) |
| latency samples | 2000 per suite |
| throughput window | 10 s |
| frequency governor | not exposed on this VM, so frequency scaling is not controlled |

**Table 2: the conditions behind the end-to-end figures.**
Every row constrains the reading, and a figure from another machine, another build or another concurrency is a different measurement.

The server under test was m6-http serving a site's real pages, so each payload was a page a site serves.
The two command lines below rebuild and rerun the measurement:

```sh
cargo build --release --bin m6-bench
./target/release/m6-bench --skip-verify --addr 127.0.0.1:443 \
    --concurrency 2 --duration 10
```

`m6-bench` also takes `--latency-n`, which defaults to 2000, and `--h2c-addr` for the cleartext listener.
[`bench.sh`](../bench.sh) builds the binaries it needs, writes a bench site under `/tmp/m6-bench`, and runs every latency, path and throughput suite for each protocol, restarting the servers between suites.

Section 5 reports the figures, and a reader quoting one of them should quote Table 2 with it.

## 5. End-to-end results

Two consecutive runs are reported in full, so the spread between them is visible.
Table 3 gives latency and Table 4 gives throughput.

Table 3 gives end-to-end latency in microseconds for six suites.
Take from it that the HTTP/2 cache hit is the only suite below 1 ms, and that every cache miss costs at least 4 ms.

| suite | p50 run 1 | p50 run 2 | p99 run 1 | p99 run 2 |
|---|---:|---:|---:|---:|
| HTTP/1.1 cache-hit → m6-html | 687.8 | 679.1 | 1011.0 | 951.4 |
| HTTP/1.1 cache-miss → m6-html | 4797.7 | 5063.1 | 6223.5 | 6526.4 |
| **HTTP/2 cache-hit → m6-html** | **225.7** | **259.2** | 318.2 | 369.9 |
| HTTP/2 cache-miss → m6-html | 4096.5 | 4159.8 | 5394.5 | 5321.6 |
| HTTP/3 cache-hit → m6-html | 953.6 | 983.3 | 1344.5 | 1339.5 |
| HTTP/3 cache-miss → m6-html | 4617.0 | 4329.3 | 5844.5 | 5525.9 |

**Table 3: end-to-end latency in microseconds, client-observed, two consecutive runs.**
The two runs differ by at most 15% on p50, and the p99 column is what a slow request costs.

Table 4 gives throughput in requests per second from the same two runs.
Take from it that HTTP/2 carries 3.4x the throughput of HTTP/1.1 and that HTTP/3 is the slowest protocol on a loopback.

| protocol | run 1 | run 2 |
|---|---:|---:|
| HTTP/1.1 | 2085.1 | 1889.3 |
| HTTP/2 | 7154.1 | 6711.2 |
| HTTP/3 | 1030.3 | 1047.0 |

**Table 4: throughput in requests per second, two consecutive runs.**
The HTTP/3 row is a property of this loopback setup, and section 6 says what it does not generalise to.

Section 6 separates the two cache-hit quantities in these two tables.

## 6. Reading the latency figures

Two cache-hit figures are quoted in m6's documentation.
They measure different things, and quoting one as the other is the error this page exists to prevent.

The 225.7 µs HTTP/2 cache hit in Table 3 is end-to-end and client-observed.
It includes TLS record processing on both sides, loopback syscalls, the client's own work and the server's.
The 2.2 µs figure is m6's internal timer around the cache lookup and response construction, reported by the running server as `hit_p50_ns`.

Both figures are true.
The first is what a client experiences on this hardware.
The second is what the cache costs.
Quoting the 2.2 µs figure as what a client sees understates the client experience by two orders of magnitude, and quoting the 225.7 µs figure as the cache's cost overstates the cache's share by the same factor.

Four further readings follow from Table 3 and Table 4.

- **A cache miss costs at least 4 ms**, and the backend render dominates it.
- **HTTP/1.1 cache misses are the slowest suite**, at 4.8 ms to 5.1 ms p50.
- **HTTP/2 is 2.6x to 3x faster than HTTP/1.1 on latency here**, and 3.4x to 3.6x faster on throughput.
- **HTTP/3 is the slowest on throughput**, at 1,030 req/s to 1,047 req/s.
  That is a real result on a loopback, co-located test.
  QUIC gains its advantage from loss and round-trip time, so the result does not generalise to a real network.

Section 7 lists what the page leaves unclaimed, which is the other half of reading a figure correctly.

## 7. What these figures do not claim

Four claims are absent on purpose, and each absence is a decision a reader can hold the page to.

- **No comparison against nginx, H2O or LiteSpeed.**
  The previous README quoted such comparisons without having run them like-for-like.
  Numbers taken from another project's blog post, on other hardware and with other payloads, are not a comparison.
- **No multi-core scaling figures.**
  None were measured.
- **No claim about a real network.**
  Every figure here is loopback.
- **No sub-millisecond visitor claim.**
  The internal cache hit is microseconds, and what a visitor experiences over the internet is dominated by round-trip time, in the tens to hundreds of milliseconds.
  Both facts belong together or neither should be quoted.

Section 8 turns to the CPU-only microbenchmarks, which are the one set here that does not involve a network stack.

## 8. Criterion microbenchmarks

Criterion measures *CPU* (central processing unit) cost with no I/O, which makes the medians below the stable figures on the page.
Table 5 gives four of them.

Table 5 gives the median cost of four operations in the cache and the stats recorder.
Take from it that a cache lookup costs tens of nanoseconds and a stats update costs under 1 ns.

| operation | median |
|---|---:|
| `make_lookup_key` | 10.4 ns |
| `cache_hit` | 21.5 ns |
| `cache_miss` | 14.7 ns |
| `stats_record` | 0.9 ns |

**Table 5: criterion medians for four cache and stats operations, in nanoseconds.**

The 2026-09-06 pass did not re-measure these.
They are carried over from an earlier run, and they carry no machine and no date, so re-run them on the build host before quoting them as current.

Section 9 is about comparing two builds, and the machine dominates that comparison.

## 9. Where to run a comparative benchmark

A comparison between two builds needs a design that survives the machine, and m6's first design did not.
Section 9.1 gives the diagnosis, section 9.2 the discarded rule, section 9.3 the rule that replaced it, and sections 9.4 and 9.5 the traps and the developer-machine conditions.

### 9.1. The diagnosis: core scheduling, not the benchmark

The `critical_path` benches produced a 19% to 26% spread between consecutive runs of the same commit, while criterion's confidence interval within each run was +/-0.08%.
A number internally precise and externally scattered is the signature of a per-process constant, so the variance came from the environment.
Two hypotheses were tested.

**The hash seed was the first, and it was wrong.**
`Cache` uses `AHashMap`, and `ahash`'s `RandomState` is seeded from the OS once per process, so the same key lands in a different bucket every run.
`Cache::with_fixed_seed_for_bench()` pins the seed, and the spread was unchanged at 49-62 ns over five runs.
The constructor was kept because determinism is worth having.
It fixed nothing.

**Core scheduling on a loaded heterogeneous machine was the second, and it was right.**
Table 6 gives the two configurations.

| configuration | result | spread |
|---|---|---|
| Forced to efficiency cores (`taskpolicy -c background`) | 107.1, 108.0 ns | **+/-1%** |
| Default quality of service, mixed performance and efficiency cores | 49-62 ns | +/-13% |

**Table 6: one bench under two core-scheduling configurations on the same machine, in nanoseconds.**
Pinning to the efficiency cores is reproducible to +/-1% at 2.1x the cost, and the default setting scatters by 13%.

The development machine is an Apple M4 with 4 performance cores and 6 efficiency cores.
Efficiency cores run this workload 2.1x slower, and pinning to them is reproducible to +/-1%.
The benchmark was correct and the scheduling was the variable.

The machine was also loaded when this was noticed.
Load average was 89 rising to 151 on ten cores.
Under that load macOS moves a process between core classes, and a run lands anywhere between the performance-core figure and the efficiency-core figure.

The general lesson closes the diagnosis.
A benchmark whose consecutive runs of unchanged code disagree by more than 2% is measuring something other than the code, and it should be diagnosed before it is quoted.
Here the clue was the gap between the within-run interval, +/-0.08%, and the between-run spread, 13%: whatever varied was fixed for the life of a process and different between processes.

### 9.2. The rule about the build host, and why it was wrong

The rule was first written as a claim about the build host, and the claim was false.
It said comparative benchmark numbers come from the build host, a dedicated Linux VM with homogeneous cores, nothing else running, and the same operating system as production.

"Nothing else running" was asserted and not checked.
Measured on 2026-09-11, the build host runs a full m6 stack, a log-scanning daemon and an audit daemon, on four cores.
It is the staging environment as well as the build box.

The cores are homogeneous, which removes the performance and efficiency problem, and the machine is busy, which replaces it with a different one.
Table 7 gives the consequence: the same tree and the same commit, measured across two passes.

| bench | pass 1 | pass 2 | swing |
|---|---|---:|---:|
| `h3_header_extract` | 21.961 ns | 20.408 ns | **7.1%** |
| `full_cache_hit_path` | 320.66 ns | 366.24 ns | **14.2%** |

**Table 7: two passes over one unchanged tree on the build host, in nanoseconds.**
A 14.2% swing on one bench is the size of the drift a comparison has to survive.

Criterion's interval within each run was under 1% on both passes, so each number looks authoritative alone.
Anything smaller than 14% measured as all of A and then all of B on this host is drift.

Section 9.3 gives the design that cancels the drift.

### 9.3. The rule: paired and interleaved

A comparative number comes from a paired, interleaved run.
Alternating the two trees round by round puts the drift into both arms equally, so it cancels in the paired difference instead of landing on one side.

The rule is one sentence: measure A, then B, many times, alternating, and report a confidence interval on the paired difference.
[`tools/paired_bench.sh`](../tools/paired_bench.sh) is the harness.
It builds both trees, waits for the load average to fall below 0.35, then runs the named bench for ten interleaved rounds by default and prints one line per round.

An absolute figure quoted on its own is meaningful only with the host and the date attached, and it is not comparable to one taken on a different machine.
A figure taken on the laptop must never be compared against a build-host figure, in this page or any other.

Section 9.4 gives two traps in the harness itself, both of which produced figures that looked fine.

### 9.4. Two traps in the harness itself

**`--sample-size` on the command line is ignored.**
The bench groups set `group.sample_size` in the source, and criterion applies the group setting after the command-line configuration, so the code wins.
A run requesting 30 samples and a three-second measurement still performs the count the group set, which is why a sixteen-measurement sweep took two hours instead of five minutes.
The flag is on the process command line and does nothing.

**Do not benchmark a tree you are still editing.**
Check that a previous run has died before starting another.
A first attempt at the A/B measured a tree being changed underneath it, and a second competed with an orphaned run that had survived being killed from the other end of an *SSH* (Secure Shell) session.
Both produced figures that looked fine.

Section 9.5 gives the conditions under which a developer machine can still be used.

### 9.5. A developer machine, with two conditions

A developer machine is fine for a quick look, with two conditions.

1. **Check the load first.**
   `uptime`.
   A load average above the core count means the figures are not comparable to anything.
2. **Pin the core class if you must compare on a heterogeneous machine.**
   `taskpolicy -c background` gives +/-1% reproducibility on an M-series Mac.
   The absolute figure is then 2.1x the real one and must never be quoted, while the run-to-run comparison is valid, which is what regression detection needs.

Both conditions are about the machine, because the benchmark itself is stable.
Section 10 applies the same discipline to six implementations on Unix sockets.

## 10. Backend examples, direct to socket

The six backend examples under `m6-http/tests/backends/` each implement the same `/status` route, and section 5 of [`m6-backend-examples.md`](m6-backend-examples.md) asks how expensive each implementation of the contract is.
The measurement runs each one on its Unix socket with no proxy in the path.

Conditions: commit `3ab8f37`, build host, 4-core Linux 7.0.0-31-generic x86_64, payload 660 bytes.
The payload is byte-identical across all six and asserted so by `m6-http/tests/backends_contract.rs`.
Each backend takes 5 seconds of load at concurrency 2, for the reason in section 3: the generator and the server share four cores, and higher concurrency starves the server it is measuring.

```sh
cargo build --workspace --release
python3 tools/backend-bench.py --duration 5 --workers 2
```

Everything ran from RAM, staged into `/dev/shm`: the binaries, the payload and the sockets.
Cold start reads the binary off disk otherwise, and the two Rust examples differ 56.7x in artefact size, so a disk-backed run would charge m6-core for a page cache miss and call it framework overhead.
A run on macOS is labelled NOT-RAM by the tool and is not comparable.
There the cold-start column was 400-650 ms for every compiled example and 52 ms for Python, which is first-exec code signing on Apple Silicon.

*RAM* is random access memory, and *NOT-RAM* is the tool's own label for a run staged on disk.
The client's own cost was measured.
The generator connected and closed a socket in its own process with no HTTP: 20,475 req/s, p50 44.7 µs.
Every row's p50 in Table 8 sits above that floor, so the differences between rows are resolvable.

Table 8 gives the six implementations, with *RSS* (resident set size) under load and the cold-start time to the first response.
Take from it that the two Rust rows are the comparison the section exists for, and that Python answers at 3,513 req/s.

| language | req/s | p50 µs | p99 µs | RSS KB | artifact bytes | cold ms |
|---|---:|---:|---:|---:|---:|---:|
| c | 23,353 | 155.9 | 401.0 | 2,160 | 21,968 | 3.1 |
| cpp | 23,088 | 158.2 | 398.2 | 4,224 | 37,856 | 3.3 |
| python | 3,513 | 1,122.3 | 1,605.8 | 12,912 | 10,510 | 34.0 |
| go | 20,828 | 171.2 | 516.9 | 14,436 | 8,485,293 | 5.8 |
| rust-plain | 28,954 | 63.7 | 138.8 | 2,612 | 554,888 | 2.9 |
| rust-m6core | 18,297 | 102.2 | 194.9 | 22,992 | 31,468,168 | 19.0 |

**Table 8: six implementations of the same `/status` route, one 5-second run each, direct to the Unix socket.**
The cold column is milliseconds to the first successful response.

Read Python's row as section 4 of [`m6-backend-examples.md`](m6-backend-examples.md) asks: this route is the shape Python is worst at, trivial per-request work at a high rate with nothing native to dispatch into.
The row is the floor of what dispatch costs, and section 5.3 of that page warns against reading it as a language ranking.

Section 10.1 gives the comparison the section exists for, Rust against Rust with and without m6-core.

### 10.1. What linking m6-core costs

Same language, same compiler, same payload and the same concurrency model, so the difference is the library.
Table 9 gives it.

| | rust-plain | rust-m6core | delta |
|---|---:|---:|---:|
| throughput | 28,954 req/s | 18,297 req/s | **-36.8%** |
| p50 | 63.7 µs | 102.2 µs | **+38.5 µs** |
| p99 | 138.8 µs | 194.9 µs | +56.1 µs |
| RSS | 2,612 KB | 22,992 KB | 8.8x |
| artifact | 554,888 B | 31,468,168 B | 56.7x |
| cold start | 2.9 ms | 19.0 ms | 6.6x |

**Table 9: the two Rust examples, same payload and same concurrency model, in one run.**
The delta column is the cost of linking m6-core.

A second identical run gave -35.6% and +36.0 µs, within 3% of the first.
Section 5.3 of [`m6-backend-examples.md`](m6-backend-examples.md) says that a delta that is not close to zero is a problem worth knowing about, and this delta is not close to zero.
It is 37 µs per request, which goes into building the request dictionary, matching the route, running the response pipeline and logging.

Two things are true at the same time.

- **The cost is real and it is the largest single finding of the backend example comparison.**
  A backend answering trivial requests at a high rate pays 36.8% of its throughput for the convenience, and carries 8.8x the resident memory.
- **The cost is measured on the shape that maximises it.**
  The route's own work is one 660-byte copy, so the framework carries the request.
  Behind the edge cache the hit rate is 0.87 to 0.90 and a hit is served in single-digit microseconds, so 87% to 90% of requests never reach a backend at all.
  Read the 36.8% as this route shape's cost.

Section 10.2 gives the control error that inverted the first version of this result.

### 10.2. The methodology bug this found in the examples

The first version of this measurement reported m6-core as 72% faster than plain Rust, and the cause was the control.
`rust-plain` spawned a thread per connection while m6-core answered from a fixed pool, so the comparison measured the concurrency model.

The concurrency section of [`m6-backend-protocol.md`](m6-backend-protocol.md) names a fixed thread pool with a bounded queue as the reference model, so `rust-plain` was not a faithful reference either.
Giving it one took it from 17,608 to 46,826 req/s at concurrency 4, a 2.7x improvement, and inverted the sign of the answer.

A control that differs from its subject in two ways measures neither.
The comparison in Table 9 comes from a pair that differs only in the library.

Section 11 closes the page with what a reader should carry away.

## 11. Summary

One figure is evidence only with its conditions, and this page is written so a reader can quote the conditions with the figure.

A figure from a busy machine, a mixed core class or a co-located generator at concurrency 8 describes that machine and that harness.
Section 3 gives the two traps that produce such figures, and section 9 gives the design that cancels machine drift.

The numbers a merge is gated on are different: [`tools/perfcheck.sh`](../tools/perfcheck.sh) measures two render targets against the values recorded in `tools/perf-baseline.txt`, on the build host, with a 20% margin.
`docs/PERFORMANCE.md` owns what one component costs.

Three sets of figures carry weaker conditions than the rest.
The criterion medians in section 8 were carried over from an earlier run, and the pair in section 3 is a demonstration with no commit or machine recorded.
The 2026-09-06 run in section 5 was taken on one 4-vCPU VPS against a site's pages.
Re-run the medians on the build host before quoting them as current, and read the other two as records of the runs that produced them.

The page's authority is its conditions, and a reader who quotes the conditions with the figure carries the evidence with it.
Section 2 records what this page replaced, and that record is the reason the rest of the page is precise about where each figure came from.
