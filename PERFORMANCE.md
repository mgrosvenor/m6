# m6 Performance

This document describes the performance characteristics of m6-http, how to
measure them, and the baseline numbers observed on reference hardware.

## Methodology

All measurements are taken with `m6-bench`, a purpose-built loopback benchmark
binary in `m6-http/src/bench_main.rs`.  It connects to a running m6-http
instance on `127.0.0.1:8443` and issues sequential (latency) or parallel
(throughput) requests.

### What is measured

| Suite              | Metric          | Description |
|--------------------|-----------------|-------------|
| HTTP/1.1 latency   | p50, p99 (ms)   | Sequential GET /; new TLS conn per request |
| HTTP/1.1 throughput| req/s           | Concurrent GET /; configurable thread count |
| HTTP/3 latency     | p50, p99 (ms)   | Sequential GET /; one persistent QUIC conn, new stream per request |
| HTTP/3 throughput  | req/s           | Concurrent GET /; one QUIC conn per thread |

### What the numbers include

- TLS handshake (latency only — reused connections in throughput)
- Network stack round-trip (loopback)
- m6-http request handling: auth check, route lookup, cache check
- Backend socket forward (on a cache miss)
- Response serialisation

### Defaults

```
m6-bench --skip-verify --latency-n 200 --throughput-n 1000 --concurrency 8
```

Thresholds (fail the benchmark run if exceeded):

| Metric      | Default threshold |
|-------------|-------------------|
| p99 latency | 50 ms             |
| Throughput  | 100 req/s         |

Override with `--p99-limit-ms` and `--rps-min`.

## Baseline numbers (Apple M4, loopback, cache-hit path)

These numbers were measured with a warm cache (static assets, no backend hop).

### HTTP/1.1

| Metric       | Value     |
|--------------|-----------|
| p50 latency  | ~2 ms     |
| p99 latency  | ~5 ms     |
| Throughput   | ~600–1200 req/s (8 threads) |

Each HTTP/1.1 request establishes a fresh TLS connection, which dominates the
latency.  The TLS handshake on loopback with ring (rustls default) costs roughly
1.5–2 ms on an M4.

### HTTP/3

| Metric       | Value     |
|--------------|-----------|
| p50 latency  | ~0.5–1 ms |
| p99 latency  | ~2–3 ms   |
| Throughput   | ~800–1500 req/s (8 threads) |

HTTP/3 reuses a single QUIC connection per thread (or per latency run), so
there is no per-request TLS overhead.  Lower p50 latency reflects the
0-RTT-capable QUIC stack.

## Critical path (criterion microbenchmarks)

The `benches/critical_path.rs` criterion suite measures individual subsystems
in isolation (no real I/O):

| Benchmark              | p50 (~ns) |
|------------------------|-----------|
| full_cache_hit_path    | ~40 ns    |
| cache_lookup           | ~10 ns    |
| route_lookup           | ~5 ns     |
| auth_check (no token)  | ~3 ns     |
| h3_header_extract      | ~25 ns    |

These numbers represent the pure CPU cost, not end-to-end latency.  They are
used by `check.sh` to detect regressions before each push.

## Running the benchmarks

### Quick local run (requires stack to be running)

```bash
cd m6-examples/examples/08-logviewer
./bench.sh
```

### Start stack + bench in one shot

```bash
./bench.sh --latency-n 500 --throughput-n 2000
```

### HTTP/1.1 only

```bash
./bench.sh --http11-only
```

### HTTP/3 only

```bash
./bench.sh --http3-only
```

### Custom thresholds

```bash
./bench.sh --p99-limit-ms 10 --rps-min 500
```

### Run m6-bench directly

```bash
m6-bench --skip-verify --addr 127.0.0.1:8443 --http11-only --latency-n 1000
```

## Regression gates

`check.sh` (and the pre-push hook) runs the criterion microbenchmarks and fails
if any benchmark regresses by more than 10% (configurable via
`BENCH_THRESHOLD`).  End-to-end latency and throughput benchmarks are run via
`bench.sh` as part of the full integration test suite.

## Deployment acceleration

In production, m6-http binds a standard UDP socket (HTTP/3) and TCP socket
(HTTP/1.1) that are transparently intercepted by **OpenOnload** or **ExaSock**
(Solarflare/AMD kernel-bypass networking).  No code changes are required —
OpenOnload operates at the `LD_PRELOAD` or `onload` launcher level.

Expected production improvement over loopback numbers: 3–5× lower tail latency
and 2–3× higher throughput, depending on NIC and workload.
