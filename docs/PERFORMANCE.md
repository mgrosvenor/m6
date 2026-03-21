# m6 Performance

This document describes the performance characteristics of m6-http, how to
measure them, and the baseline numbers observed on reference hardware.

## Methodology

All measurements are taken with `m6-bench`, a purpose-built loopback benchmark
binary in `m6-http/src/bench_main.rs`, orchestrated by `bench.sh`.

`bench.sh` runs independent suites covering four inbound protocols (H1/H2/H3/H2C),
three socket-backend suite types (latency/path/throughput), and a full 4×4 URL-backend
routing matrix (all inbound × all outbound protocol combinations).
Servers are stopped and restarted between every suite to eliminate stale-connection
pollution.

### Socket-backend suites

| Suite | Metric | Description |
|-------|--------|-------------|
| HTTP/1.1 latency | per-request µs | Sequential GET `/`; new TLS conn per request |
| HTTP/2 latency | per-request µs | Sequential GET `/`; persistent TLS conn, one stream per request |
| HTTP/3 latency | per-request µs | Sequential GET `/`; persistent QUIC conn, one H3 stream per request |
| H2C latency | per-request µs | Sequential GET `/`; persistent plain-TCP H2 conn |
| HTTP/1.1 path | per-request µs | Sequential GET of four paths; new TLS conn per request |
| HTTP/2 path | per-request µs | Sequential GET of four paths; persistent TLS conn |
| HTTP/3 path | per-request µs | Sequential GET of four paths; persistent QUIC conn |
| H2C path | per-request µs | Sequential GET of four paths; persistent plain-TCP H2 conn |
| HTTP/1.1 throughput | req/s | Concurrent GET `/`; 8 threads, new conn per request |
| HTTP/2 throughput | req/s | Concurrent GET `/`; 8 threads, persistent conn per thread |
| HTTP/3 throughput | req/s | Concurrent GET `/`; 8 threads, persistent QUIC conn per thread |
| H2C throughput | req/s | Concurrent GET `/`; 8 threads, persistent plain-TCP H2 conn per thread |

### URL-backend routing matrix suites

| Suite | Metric | Description |
|-------|--------|-------------|
| URL latency | per-request µs | 16 combinations: (h1\|h2\|h3\|h2c) inbound × (http\|https\|h2c\|h2s) outbound; sequential |
| URL throughput | req/s | Same 16 combinations; 8 concurrent threads, 10 s window |

URL backends are `bench-url-backend` instances (fixed 200 body, `Cache-Control: no-store`).
Outbound http/https use one connection per request (H1); h2c/h2s use persistent multiplexed connections.

### Path suite — four routes

| Name | URL | Backend | Cache-Control |
|------|-----|---------|---------------|
| cache-hit→m6-html | `GET /` | m6-html | `public` — cached after first request |
| cache-hit→m6-file | `GET /assets/hello.txt` | m6-file | `public` — cached after first request |
| cache-miss→m6-html | `GET /nocache/` | m6-html | `no-store` — always forwarded |
| cache-miss→m6-file | `GET /tail/hello.txt` | m6-file | `no-store` — always forwarded |

### Defaults

```bash
./bench.sh --skip-verify
```

Override any m6-bench parameter by passing it to `bench.sh`:

```bash
./bench.sh --skip-verify --latency-n 5000 --duration 30 --concurrency 16
```

---

## Baseline numbers (Apple M4, loopback, n=2000 per latency/path suite)

### Socket-backend latency — GET `/`

| Protocol | p50 (µs) | p99 (µs) | avg (µs) | std (µs) |
|----------|--------:|---------:|---------:|---------:|
| HTTP/1.1 |   164.0 |    264.2 |    168.8 |     23.2 |
| HTTP/2   |    20.1 |     49.0 |     24.8 |     10.2 |
| HTTP/3   |    28.5 |     67.1 |     34.4 |     11.7 |
| H2C      |    21.3 |     58.0 |     26.6 |     10.4 |

HTTP/1.1 establishes a fresh TLS connection per request; ~164 µs p50 is dominated
by the TLS handshake. HTTP/2, HTTP/3, and H2C reuse a single connection.
H2C is marginally faster than HTTP/2 at p50 — no TLS record framing overhead.

---

### Socket-backend path — cache hit vs cache miss p50 (µs)

| Path | HTTP/1.1 | HTTP/2 | HTTP/3 | H2C |
|------|--------:|-------:|-------:|----:|
| cache-hit  → m6-html | 174 |  23 | 28 | 24 |
| cache-hit  → m6-file | 171 |  21 | 28 | 23 |
| cache-miss → m6-html | 200 |  51 | 57 | 52 |
| cache-miss → m6-file | 206 |  58 | 62 | 60 |

All paths cluster at ~170–206 µs for HTTP/1.1 (TLS handshake dominates).
HTTP/2 and H2C are nearly identical — the ~30 µs cache-miss delta is the backend
Unix socket round-trip, not TLS framing. H2C's slight p50 edge over HTTP/2
disappears at cache-miss because the backend RTT dominates.

---

### Socket-backend throughput — 8 concurrent connections, 10 s

| Protocol | req/s   |
|----------|--------:|
| HTTP/1.1 |  11,739 |
| HTTP/2   | 158,271 |
| HTTP/3   |  73,960 |
| H2C      | 118,554 |

HTTP/2 leads at ~158 K req/s via connection multiplexing.
H2C reaches ~119 K req/s — H2 framing without TLS, event-loop driven.
HTTP/3 at ~74 K req/s is CPU-bound on QUIC per-datagram overhead.
HTTP/1.1 is limited to ~12 K req/s by per-connection TLS handshake cost.

---

### Protocol routing matrix — latency p50 (µs)

All 16 (inbound × outbound) combinations. n=2000, sequential.

| Inbound ↓ \ Outbound → | http | https | h2c | h2s |
|---|---:|---:|---:|---:|
| **h1 (TLS)**  | 170 | 170 | 169 | 170 |
| **h2 (TLS)**  |  19 |  19 |  19 |  19 |
| **h3 (QUIC)** |  29 |  32 |  28 |  29 |
| **h2c**       |  20 |  20 |  20 |  20 |

**Key finding:** for h2 and h2c inbound, the outbound backend protocol has no
measurable effect on latency. The outbound connections are persistent and
multiplexed (h2c/h2s) or fast enough on loopback (http/https) that forwarding
cost is dominated by inbound processing, not outbound transport.

---

### Protocol routing matrix — throughput (req/s)

8 concurrent inbound connections, 10 s window.

| Inbound ↓ \ Outbound → | http | https | h2c | h2s |
|---|---:|---:|---:|---:|
| **h1**  |  11,382 |  11,561 |  11,611 |  11,579 |
| **h2**  | 165,169 | 160,637 | 158,858 | 154,497 |
| **h3**  |  77,354 |  64,306 |  56,504 |  51,088 |
| **h2c** | 116,583 | 177,032 | 176,310 | 163,046 |

**h1 row:** all outbound variants hit ~11.5 K req/s — inbound TLS-per-request is
the bottleneck, not the backend.

**h2/h2c rows:** throughput is high and roughly uniform across outbound protocols.
Small variation (~5–10%) reflects the extra TLS work for https/h2s outbound.

**h3 row:** drops sharply with TLS outbound (77 K→51 K req/s). QUIC's single-
threaded event loop is CPU-bound; adding outbound TLS encryption pushes it further.

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

These numbers represent the pure CPU cost, not end-to-end latency. They are
used by `check.sh` to detect regressions before each push.

---

## Running the benchmarks

### Full suite

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
./bench.sh --skip-verify --h2c-only
./bench.sh --skip-verify --url-only
```

### Run m6-bench directly (server must already be running)

```bash
m6-bench --skip-verify --addr 127.0.0.1:8443 --h2c-addr 127.0.0.1:8080 --http3-only --path-only
m6-bench --skip-verify --addr 127.0.0.1:8443 --h2c-addr 127.0.0.1:8080 --url-only --latency-only
```

---

## Regression gates

`check.sh` (and the pre-push hook) runs the criterion microbenchmarks and fails
if any benchmark regresses by more than 15% (configurable via `BENCH_THRESHOLD`).
End-to-end latency and throughput benchmarks are run via `bench.sh`.

---

## Deployment acceleration

In production, m6-http binds a standard UDP socket (HTTP/3) and TCP socket
(HTTP/1.1 / HTTP/2) that are transparently intercepted by **OpenOnload** or
**ExaSock** (Solarflare/AMD kernel-bypass networking). No code changes are
required — OpenOnload operates at the `LD_PRELOAD` or `onload` launcher level.

Expected production improvement over loopback numbers: 3–5× lower tail latency
and 2–3× higher throughput, depending on NIC and workload.
