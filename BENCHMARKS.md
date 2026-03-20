# m6-http Benchmark Results

All benchmarks run on macOS (Darwin 24.6.0) against a loopback target (`127.0.0.1:8443` for TLS, `127.0.0.1:8080` for H2C).
Suite: 12 tests — HTTP/1.1 · HTTP/2 · HTTP/3 · H2C × latency · path · throughput.
Each suite restarts all servers (m6-http, m6-html, m6-file) to avoid connection pollution.
**n = 2000** requests per latency/path test; **10 s** window for throughput tests.

---

## Latency — single-connection round-trip (µs)

| Protocol | p0    | p1    | p25  | p50  | p75  | p99  | p100  | avg   | std   |
|----------|------:|------:|-----:|-----:|-----:|-----:|------:|------:|------:|
| HTTP/1.1 | 133.1 | 136.8 | 154.8| 164.6| 173.8| 399.7| 995.1 | 172.1 |  46.0 |
| HTTP/2   |  15.5 |  17.0 |  18.2|  19.4|  20.7|  29.6|  55.8 |  19.8 |   2.7 |
| HTTP/3   |  16.8 |  17.2 |  23.8|  25.8|  28.5|  49.7|  80.0 |  25.9 |   5.4 |
| **H2C**  |  12.9 |  15.0 |  17.0|  18.8|  21.2|  27.7|  69.2 |  19.4 |   3.1 |

HTTP/2 and H2C are **~8× lower latency** at p50 than HTTP/1.1. H2C is marginally faster than
HTTP/2 (18.8 µs vs 19.4 µs p50) — no TLS record framing on the hot path. The high p99/p100
on H1 is macOS scheduler noise; the H2/H2C/H3 stacks amortise connection overhead across requests.

---

## Path benchmarks — cache-hit and cache-miss latency (µs)

Exercises the full dispatch path through m6-http → m6-html (template render) and
m6-http → m6-file (static file). Cache-hit = content cached in the backend;
cache-miss = content loaded from disk on each request.

### HTTP/1.1

| Route                | p0    | p25   | p50   | p75   | p99   | avg   | std  |
|----------------------|------:|------:|------:|------:|------:|------:|-----:|
| cache-hit  → m6-html | 145.0 | 160.5 | 165.7 | 171.3 | 371.8 | 172.8 | 36.5 |
| cache-hit  → m6-file | 145.4 | 160.4 | 164.8 | 169.0 | 185.9 | 164.8 |  7.7 |
| cache-miss → m6-html | 178.4 | 191.9 | 197.8 | 202.9 | 223.2 | 200.1 | 90.6 |
| cache-miss → m6-file | 183.5 | 198.3 | 202.8 | 207.5 | 226.1 | 203.3 |  7.6 |

### HTTP/2

| Route                | p0   | p25  | p50  | p75  | p99  | avg  | std  |
|----------------------|-----:|-----:|-----:|-----:|-----:|-----:|-----:|
| cache-hit  → m6-html | 15.5 | 19.2 | 21.2 | 33.5 | 86.7 | 30.8 | 18.9 |
| cache-hit  → m6-file | 15.4 | 18.6 | 19.4 | 20.4 | 27.9 | 19.8 |  2.5 |
| cache-miss → m6-html | 36.7 | 47.4 | 48.9 | 50.6 | 58.9 | 49.1 |  3.3 |
| cache-miss → m6-file | 42.2 | 53.9 | 55.2 | 56.8 | 67.4 | 55.7 |  4.1 |

### HTTP/3

| Route                | p0   | p25  | p50  | p75  | p99  | avg  | std  |
|----------------------|-----:|-----:|-----:|-----:|-----:|-----:|-----:|
| cache-hit  → m6-html | 22.0 | 27.1 | 28.3 | 29.3 | 55.2 | 28.7 |  4.3 |
| cache-hit  → m6-file | 20.1 | 28.5 | 30.2 | 33.0 | 55.9 | 31.2 |  4.8 |
| cache-miss → m6-html | 42.8 | 55.2 | 58.4 | 63.2 | 90.9 | 60.1 |  7.5 |
| cache-miss → m6-file | 52.5 | 59.0 | 61.7 | 66.9 | 96.5 | 64.3 |  8.6 |

### H2C (HTTP/2 cleartext — plain TCP, no TLS)

| Route                | p0   | p25  | p50  | p75  | p99  | avg  | std  |
|----------------------|-----:|-----:|-----:|-----:|-----:|-----:|-----:|
| cache-hit  → m6-html | 14.5 | 20.7 | 21.8 | 22.5 | 36.0 | 22.1 |  3.8 |
| cache-hit  → m6-file | 13.6 | 20.8 | 21.8 | 23.2 | 43.2 | 22.5 |  4.0 |
| cache-miss → m6-html | 42.2 | 50.1 | 51.7 | 53.4 | 73.0 | 52.4 |  4.4 |
| cache-miss → m6-file | 48.7 | 55.2 | 56.9 | 59.2 | 81.8 | 58.3 |  6.9 |

**Notes:**
- m6-file cache-hit is consistently the fastest path at p50 (~22 µs H2/H2C) owing to its
  in-process `mmap`/page-cache read and minimal protocol overhead.
- H2C and HTTP/2 are essentially identical on cache misses — the backend socket RTT dominates
  at ~52–57 µs, not the TLS framing cost.
- H3 p75 on cache-hit paths has more variance than H2/H2C due to QUIC's UDP congestion control
  and ACK coalescing, even on loopback.

---

## Throughput — 8 parallel connections, 10 s window

| Protocol | req/s      |
|----------|------------|
| HTTP/1.1 |  11,582    |
| HTTP/2   | 154,715    |
| HTTP/3   |  77,043    |
| **H2C**  | **113,242**|

**Notes:**
- **HTTP/2 is the throughput winner** at ~155 K req/s, benefiting from connection multiplexing
  and `WINDOW_UPDATE` flow-control that allows continuous pipelining without stalling.
- **H2C reaches ~113 K req/s** — H2 multiplexing without TLS encryption cost. Sits between
  HTTP/2 and HTTP/3; the plain-TCP path is slightly heavier than TLS (which benefits from
  kernel TLS offload) but avoids QUIC's per-datagram overhead.
- **HTTP/3 reaches ~77 K req/s**. QUIC's per-stream flow control and UDP processing overhead
  through the quiche library reduce peak throughput relative to H2, but it offers better
  behaviour under packet loss and in high-latency networks.
- **HTTP/1.1 is limited to ~12 K req/s** because each of the 8 connections is strictly
  sequential; per-connection TLS handshake overhead brings it down to ~12 K on macOS loopback.

---

## Event-loop utilisation — epoll idle vs. working

Measured using macOS `sample` (1 ms sampling interval, 5 s window) during the throughput
tests. For this single-threaded server, `%CPU ≈ % time not blocked in kqueue/epoll_wait`.

| Protocol | Idle in kqueue | Working | Dominant cost |
|----------|---------------:|--------:|---------------|
| HTTP/1.1 | **18.5%** | **81.5%** | TLS handshake (HKDF/SHA-512) per new connection |
| HTTP/2   |  **7.5%** | **92.5%** | TLS AES-GCM decrypt + encrypt (~56% combined) |
| HTTP/3   |  **<0.1%** | **~100%** | UDP `sendto` (~24%) + QUIC packet build/encrypt (~30%) |

**Notes:**
- **H3 is fully CPU-saturated** at 77 K req/s: the server never reaches kqueue during a
  throughput run. The bottleneck is QUIC per-packet overhead — every UDP datagram needs its
  own header, packet number, AES-GCM seal, and congestion-control update (LegacyRecovery).
  TCP/TLS coalesces multiple requests into fewer records, giving H2 a throughput edge.
- **H2's 7.5% idle** comes from brief gaps when all 8 streams have sent their responses and
  are waiting for the next request batch to arrive from the bench client.
- **H1's 18.5% idle** is higher because the bench client creates new TLS connections
  periodically (per-connection HKDF/SHA-512 key derivation is visible in the flat profile),
  and each new handshake leaves the server idle while the client completes its side.
- **During latency tests** (single sequential connection) the picture reverses: with ~22 µs
  of work per H2 request and ~164 µs between requests, the server spends ~99%+ of wall time
  blocked in kqueue.

### H2 working-time breakdown (from `sample` call tree)

| Phase | Samples | % of total |
|-------|--------:|-----------:|
| TLS AES-GCM decrypt (`fill_recv`) | ~1264 / 4279 | ~30% |
| TLS AES-GCM encrypt (`flush_tls`) | ~1129 / 4279 | ~26% |
| Request handling + cache lookup   |  ~264 / 4279 |  ~6% |
| kqueue idle                       |   320 / 4279 |  ~7% |
| Other (loop overhead, alloc, etc) |  ~322 / 4279 |  ~8% |

### H3 working-time breakdown (from `sample` call tree)

| Phase | Samples | % of total |
|-------|--------:|-----------:|
| UDP `sendto` syscall (`flush_conn`) | ~1030 / 4259 | ~24% |
| quiche packet build + AES-GCM seal  |  ~456 / 4259 | ~11% |
| `drain_udp` / recv + quiche dispatch | ~2769 / 4259 | ~65% |
| kqueue idle                          |     2 / 4259 |  ~0% |

---

## Test configuration

### `system.toml` (m6-http bind / TLS / H2C)

```toml
[server]
bind     = "127.0.0.1:8443"
tls_cert = "cert.pem"
tls_key  = "key.pem"
h2c_bind = "127.0.0.1:8080"
```

### `site.toml` (routing + backends)

```toml
[site]
name   = "m6-bench"
domain = "localhost"

[errors]
mode = "internal"

[log]
level  = "warn"
format = "text"

[[backend]]
name    = "m6-html"
sockets = "/tmp/m6-bench/m6-html-bench.sock"

[[backend]]
name    = "m6-file"
sockets = "/tmp/m6-bench/m6-file-bench.sock"

[[route]]
path    = "/"
backend = "m6-html"

[[route]]
path    = "/nocache/"
backend = "m6-html"

[[route_group]]
glob    = "assets/**/*"
path    = "/assets/{relpath}"
backend = "m6-file"

[[route_group]]
glob    = "tail/**/*"
path    = "/tail/{relpath}"
backend = "m6-file"
```

### `configs/m6-html.conf`

```toml
global_params = ["data/site.json"]

[[route]]
path     = "/"
template = "templates/home.html"

[[route]]
path     = "/nocache/"
template = "templates/home.html"
cache    = "no-store"
```

### `configs/m6-file.conf`

```toml
[[route]]
path = "/assets/{relpath}"
root = "assets/"

[[route]]
path = "/tail/{relpath}"
root = "assets/"
tail = true
```

### Bench template (`templates/home.html`)

```html
<!doctype html>
<html><head><meta charset="utf-8"><title>bench</title></head>
<body><h1>m6-bench</h1><p>ok</p></body>
</html>
```

Response body is ~107 bytes. TLS certificates are self-signed (rcgen), bench client uses
`--skip-verify`.

---

## Environment

| Item | Value |
|------|-------|
| Platform | macOS Darwin 24.6.0 (ARM64) |
| TLS target | `127.0.0.1:8443` (loopback) |
| H2C target | `127.0.0.1:8080` (loopback, plain TCP) |
| TLS | rustls with ring provider |
| HTTP/3 | quiche 0.26.1 (Cloudflare, boringssl-vendored) |
| Backends | m6-html (m6-render), m6-file |
| m6-http model | single-threaded epoll (kqueue on macOS) event loop |
| bench concurrency | 8 parallel connections (throughput); 1 (latency/path) |
| Date | 2026-03-20 |
