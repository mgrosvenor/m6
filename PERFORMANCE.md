# m6 Performance

This document describes the performance characteristics of m6-http, how to
measure them, and the baseline numbers observed on reference hardware.

## Methodology

All measurements are taken with `m6-bench`, a purpose-built loopback benchmark
binary in `m6-http/src/bench_main.rs`, orchestrated by `bench.sh`.

`bench.sh` runs nine independent suites — three protocols × three suite types.
Servers are stopped and restarted between every suite to eliminate stale-connection
pollution (particularly relevant for HTTP/3, whose QUIC connections linger for
their idle timeout after a throughput run).

### Suites

| Suite | Metric | Description |
|-------|--------|-------------|
| HTTP/1.1 latency | per-request ms | Sequential GET `/`; new TLS conn per request |
| HTTP/2 latency | per-request ms | Sequential GET `/`; persistent TLS conn, one stream per request |
| HTTP/3 latency | per-request ms | Sequential GET `/`; persistent QUIC conn, one H3 stream per request |
| HTTP/1.1 path | per-request ms | Sequential GET of four paths (see below); new TLS conn per request |
| HTTP/2 path | per-request ms | Sequential GET of four paths; persistent TLS conn |
| HTTP/3 path | per-request ms | Sequential GET of four paths; persistent QUIC conn |
| HTTP/1.1 throughput | req/s | Concurrent GET `/`; 8 threads, new conn per request |
| HTTP/2 throughput | req/s | Concurrent GET `/`; 8 threads, persistent conn per thread |
| HTTP/3 throughput | req/s | Concurrent GET `/`; 8 threads, persistent QUIC conn per thread |

### Path suite — four routes

The path suite exercises all four request paths through m6-http, probing the
cache behaviour for both the HTML renderer and the static file server:

| Name | URL | Backend | Cache-Control |
|------|-----|---------|---------------|
| cache-hit→m6-html | `GET /` | m6-html | `public` (default) — cached by m6-http after first request |
| cache-hit→m6-file | `GET /assets/hello.txt` | m6-file | `public` — cached by m6-http after first request |
| cache-miss→m6-html | `GET /nocache/` | m6-html | `no-store` — always forwarded to m6-html |
| cache-miss→m6-file | `GET /tail/hello.txt` | m6-file | `no-store` (tail route) — always forwarded to m6-file |

The m6-http in-memory cache is active **only on the HTTP/3 path**; HTTP/1.1 and
HTTP/2 requests always forward to the backend.  The cache benefit is therefore
visible only in the HTTP/3 path results.

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

All numbers from a single `./bench.sh --skip-verify` run.  Statistics are
computed over 2000 sequential requests with 5 warmup requests discarded.

---

### Latency — GET `/` (single path, sequential)

| Protocol | p0 | p1 | p25 | p50 | p75 | p99 | p100 | IQR | range | avg | std | (ms) |
|----------|----|----|-----|-----|-----|-----|------|-----|-------|-----|-----|------|
| HTTP/1.1 | 0.174 | 0.181 | 0.192 | 0.198 | 0.206 | 0.444 | 0.719 | 0.015 | 0.545 | 0.208 | 0.047 | ms |
| HTTP/2   | 0.037 | 0.039 | 0.045 | 0.048 | 0.053 | 0.131 | 1.492 | 0.008 | 1.455 | 0.057 | 0.040 | ms |
| HTTP/3   | 0.020 | 0.022 | 0.027 | 0.031 | 0.057 | 0.092 | 0.185 | 0.030 | 0.165 | 0.042 | 0.019 | ms |

HTTP/1.1 establishes a fresh TLS connection per request; the ~0.2 ms p50 is
dominated by the TLS handshake cost on loopback.  HTTP/2 and HTTP/3 reuse a
single connection, reducing p50 to ~0.05 ms and ~0.03 ms respectively.

---

### Path benchmarks — cache hit vs cache miss, per protocol

#### HTTP/1.1 path (no cache — every request forwarded to backend)

| Path | p0 | p1 | p25 | p50 | p75 | p99 | p100 | IQR | range | avg | std | (ms) |
|------|----|----|-----|-----|-----|-----|------|-----|-------|-----|-----|------|
| cache-hit→m6-html  | 0.176 | 0.183 | 0.194 | 0.202 | 0.210 | 0.378 | 0.549 | 0.016 | 0.372 | 0.209 | 0.032 | ms |
| cache-hit→m6-file  | 0.182 | 0.189 | 0.199 | 0.204 | 0.209 | 0.238 | 2.423 | 0.010 | 2.241 | 0.206 | 0.051 | ms |
| cache-miss→m6-html | 0.179 | 0.184 | 0.193 | 0.200 | 0.207 | 0.248 | 0.341 | 0.014 | 0.163 | 0.201 | 0.013 | ms |
| cache-miss→m6-file | 0.178 | 0.189 | 0.199 | 0.204 | 0.210 | 0.249 | 4.659 | 0.011 | 4.481 | 0.208 | 0.100 | ms |

All four paths are similar (~0.2 ms p50) because H1 has no cache; every request
pays the full TLS handshake + backend forward cost.

#### HTTP/2 path (no cache — every request forwarded to backend)

| Path | p0 | p1 | p25 | p50 | p75 | p99 | p100 | IQR | range | avg | std | (ms) |
|------|----|----|-----|-----|-----|-----|------|-----|-------|-----|-----|------|
| cache-hit→m6-html  | 0.035 | 0.039 | 0.045 | 0.049 | 0.055 | 0.122 | 0.221 | 0.010 | 0.187 | 0.056 | 0.019 | ms |
| cache-hit→m6-file  | 0.044 | 0.048 | 0.053 | 0.055 | 0.057 | 0.090 | 0.241 | 0.005 | 0.196 | 0.056 | 0.009 | ms |
| cache-miss→m6-html | 0.032 | 0.039 | 0.045 | 0.047 | 0.049 | 0.070 | 0.182 | 0.004 | 0.150 | 0.048 | 0.007 | ms |
| cache-miss→m6-file | 0.043 | 0.047 | 0.051 | 0.053 | 0.054 | 0.067 | 0.120 | 0.003 | 0.077 | 0.053 | 0.004 | ms |

HTTP/2 also has no cache; all paths cluster around ~0.05 ms p50.

#### HTTP/3 path (cache active — hit paths skip the backend)

| Path | p0 | p1 | p25 | p50 | p75 | p99 | p100 | IQR | range | avg | std | (ms) |
|------|----|----|-----|-----|-----|-----|------|-----|-------|-----|-----|------|
| cache-hit→m6-html  | 0.022 | 0.024 | 0.031 | 0.048 | 0.054 | 0.080 | 0.114 | 0.023 | 0.092 | 0.046 | 0.014 | ms |
| cache-hit→m6-file  | 0.022 | 0.025 | 0.028 | 0.029 | 0.030 | 0.055 | 0.131 | 0.002 | 0.109 | 0.030 | 0.006 | ms |
| cache-miss→m6-html | 0.036 | 0.051 | 0.057 | 0.059 | 0.062 | 0.087 | 0.115 | 0.005 | 0.080 | 0.060 | 0.006 | ms |
| cache-miss→m6-file | 0.054 | 0.057 | 0.061 | 0.064 | 0.068 | 0.096 | 0.146 | 0.007 | 0.092 | 0.065 | 0.007 | ms |

The cache benefit is clear on the file path: cache-hit→m6-file p50 = **0.029 ms**
vs cache-miss→m6-file p50 = **0.064 ms** — a **2.2× speedup**.  The HTML path
shows a smaller but real benefit (0.048 ms vs 0.059 ms) because the Tera render
work is modest for the bench template.

---

### Throughput — concurrent GET `/`, 8 threads, 10 s

| Protocol | req/s |
|----------|-------|
| HTTP/1.1 | 8,840 |
| HTTP/2   | 28,797 |
| HTTP/3   | 61,748 |

HTTP/3 achieves ~7× the throughput of HTTP/1.1 and ~2× that of HTTP/2, driven
by connection reuse (no TLS handshake per request), QUIC multiplexing, and the
m6-http single-threaded event loop being able to drain many pending requests
from each connection in one pass.

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
```

### Run m6-bench directly (server must already be running)

```bash
m6-bench --skip-verify --addr 127.0.0.1:8443 --http3-only --path-only
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
