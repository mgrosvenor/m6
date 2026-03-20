# m6 Performance

This document describes the performance characteristics of m6-http, how to
measure them, and the baseline numbers observed on reference hardware.

## Methodology

All measurements are taken with `m6-bench`, a purpose-built loopback benchmark
binary in `m6-http/src/bench_main.rs`, orchestrated by `bench.sh`.

`bench.sh` runs twelve independent suites — four protocols × three suite types.
Servers are stopped and restarted between every suite to eliminate stale-connection
pollution (particularly relevant for HTTP/3, whose QUIC connections linger for
their idle timeout after a throughput run).

### Suites

| Suite | Metric | Description |
|-------|--------|-------------|
| HTTP/1.1 latency | per-request µs | Sequential GET `/`; new TLS conn per request |
| HTTP/2 latency | per-request µs | Sequential GET `/`; persistent TLS conn, one stream per request |
| HTTP/3 latency | per-request µs | Sequential GET `/`; persistent QUIC conn, one H3 stream per request |
| H2C latency | per-request µs | Sequential GET `/`; persistent plain-TCP H2 conn, one stream per request |
| HTTP/1.1 path | per-request µs | Sequential GET of four paths (see below); new TLS conn per request |
| HTTP/2 path | per-request µs | Sequential GET of four paths; persistent TLS conn |
| HTTP/3 path | per-request µs | Sequential GET of four paths; persistent QUIC conn |
| H2C path | per-request µs | Sequential GET of four paths; persistent plain-TCP H2 conn |
| HTTP/1.1 throughput | req/s | Concurrent GET `/`; 8 threads, new conn per request |
| HTTP/2 throughput | req/s | Concurrent GET `/`; 8 threads, persistent conn per thread |
| HTTP/3 throughput | req/s | Concurrent GET `/`; 8 threads, persistent QUIC conn per thread |
| H2C throughput | req/s | Concurrent GET `/`; 8 threads, persistent plain-TCP H2 conn per thread |

### Path suite — four routes

The path suite exercises all four request paths through m6-http, probing the
cache behaviour for both the HTML renderer and the static file server:

| Name | URL | Backend | Cache-Control |
|------|-----|---------|---------------|
| cache-hit→m6-html | `GET /` | m6-html | `public` (default) — cached by m6-http after first request |
| cache-hit→m6-file | `GET /assets/hello.txt` | m6-file | `public` — cached by m6-http after first request |
| cache-miss→m6-html | `GET /nocache/` | m6-html | `no-store` — always forwarded to m6-html |
| cache-miss→m6-file | `GET /tail/hello.txt` | m6-file | `no-store` (tail route) — always forwarded to m6-file |

The m6-http in-memory cache is active **only on the HTTP/3 path**; HTTP/1.1,
HTTP/2, and H2C requests always forward to the backend.  The cache benefit is
therefore visible only in the HTTP/3 path results.

### What the numbers include

- TLS / QUIC handshake (latency suites only — connections are reused within a run)
- Loopback network round-trip
- m6-http: auth check, route lookup, cache lookup (H3 only), response serialisation
- Backend socket forward + render / file read (on a cache miss, or H1/H2)

### Defaults

```
./bench.sh --skip-verify
```

Override any m6-bench parameter by passing it to `bench.sh`:

```
./bench.sh --skip-verify --latency-n 5000 --duration 30 --concurrency 16
```

## Baseline numbers (Apple M4, loopback, n=2000 per latency/path suite)

All numbers from a single `./bench.sh --skip-verify --h2c --h2c-addr 127.0.0.1:8080` run.
Statistics are computed over 2000 sequential requests with 5 warmup requests discarded.

---

### Latency — GET `/` (single path, sequential)

| Protocol | p50 (µs) | p99 (µs) | avg (µs) | std (µs) |
|----------|--------:|---------:|---------:|---------:|
| HTTP/1.1 |   164.6 |    399.7 |    172.1 |     46.0 |
| HTTP/2   |    19.4 |     29.6 |     19.8 |      2.7 |
| HTTP/3   |    25.8 |     49.7 |     25.9 |      5.4 |
| H2C      |    18.8 |     27.7 |     19.4 |      3.1 |

HTTP/1.1 establishes a fresh TLS connection per request; ~165 µs p50 is dominated
by the TLS handshake.  HTTP/2, HTTP/3, and H2C reuse a single connection.
H2C is marginally faster than HTTP/2 at p50 — no TLS record framing overhead.

---

### Path benchmarks — cache hit vs cache miss, per protocol

#### Path p50 latency summary (µs)

| Path | HTTP/1.1 | HTTP/2 | HTTP/3 | H2C |
|------|--------:|-------:|-------:|----:|
| cache-hit  → m6-html | 175 | 20 | 28 | 22 |
| cache-hit  → m6-file | 169 | 20 | 30 | 22 |
| cache-miss → m6-html | 199 | 51 | 58 | 52 |
| cache-miss → m6-file | 206 | 56 | 62 | 57 |

All paths cluster at ~170–206 µs for HTTP/1.1 (TLS handshake dominates).
HTTP/2 and H2C are nearly identical — the ~30 µs cache-miss cost is the backend
Unix socket round-trip, not TLS framing. H2C's slight p50 edge over HTTP/2
disappears at cache-miss because the backend RTT dominates.
HTTP/3 cache hits are served from the in-process cache; cache-misses pay the
same backend forward cost as HTTP/2/H2C.

---

### Throughput — concurrent GET `/`, 8 threads, 10 s

| Protocol | req/s   |
|----------|--------:|
| HTTP/1.1 |  11,582 |
| HTTP/2   | 154,715 |
| HTTP/3   |  77,043 |
| H2C      | 113,242 |

HTTP/2 leads at ~155 K req/s via connection multiplexing and kernel TLS offload.
H2C reaches ~113 K req/s — H2 framing without TLS, no thread per request (event-loop
driven). HTTP/3 at ~77 K req/s is CPU-bound on QUIC per-datagram overhead.
HTTP/1.1 is limited to ~12 K req/s by per-connection TLS handshake cost.

---

## Critical path (criterion microbenchmarks)

The `benches/critical_path.rs` criterion suite measures individual subsystems
in isolation (no real I/O):

| Benchmark | p50 (~ns) |
|-----------|-----------|
| full_cache_hit_path | ~40 ns |
| cache_lookup | ~10 ns |
| route_lookup | ~5 ns |
| auth_check (no token) | ~3 ns |
| h3_header_extract | ~25 ns |

These numbers represent the pure CPU cost, not end-to-end latency.  They are
used by `check.sh` to detect regressions before each push.

## Running the benchmarks

### Full suite (all protocols, all suite types, fresh servers between each)

```bash
./bench.sh --skip-verify
```

### Custom parameters

```bash
./bench.sh --skip-verify --latency-n 5000 --duration 30 --concurrency 16
```

### Protocol filters

```bash
./bench.sh --skip-verify --http11-only
./bench.sh --skip-verify --http3-only
./bench.sh --skip-verify --h2c-only --h2c-addr 127.0.0.1:8080
```

### Run m6-bench directly (server must already be running)

```bash
m6-bench --skip-verify --addr 127.0.0.1:8443 --http3-only --path-only
m6-bench --skip-verify --h2c --h2c-addr 127.0.0.1:8080 --latency-only
```

## Regression gates

`check.sh` (and the pre-push hook) runs the criterion microbenchmarks and fails
if any benchmark regresses by more than 15% (configurable via `BENCH_THRESHOLD`).
End-to-end latency and throughput benchmarks are run via `bench.sh` as part of
the full integration test suite.

## Deployment acceleration

In production, m6-http binds a standard UDP socket (HTTP/3) and TCP socket
(HTTP/1.1 / HTTP/2) that are transparently intercepted by **OpenOnload** or
**ExaSock** (Solarflare/AMD kernel-bypass networking).  No code changes are
required — OpenOnload operates at the `LD_PRELOAD` or `onload` launcher level.

Expected production improvement over loopback numbers: 3–5× lower tail latency
and 2–3× higher throughput, depending on NIC and workload.
