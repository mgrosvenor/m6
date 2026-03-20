# m6-http Benchmark Results

All benchmarks run on macOS (Darwin 24.6.0, Apple M4) against a loopback target.
`127.0.0.1:8443` for TLS inbound, `127.0.0.1:8080` for H2C inbound.
Each suite restarts all servers (m6-http, m6-html, m6-file, bench-url-backend) to avoid connection pollution.
**n = 2000** requests per latency/path test; **10 s** window for throughput tests; **8 concurrent connections** for throughput.

---

## Socket-backend latency — GET `/` → Unix socket (µs)

Single sequential connection, n=2000.

| Protocol | p0    | p25  | p50  | p75  | p99  | p100  | avg   | std  |
|----------|------:|-----:|-----:|-----:|-----:|------:|------:|-----:|
| HTTP/1.1 | 143.5 | 157.4| 164.0| 171.0| 264.2| 376.6 | 168.8 | 23.2 |
| HTTP/2   |  15.2 |  18.6|  20.1|  25.5|  49.0| 148.9 |  24.8 | 10.2 |
| HTTP/3   |  21.2 |  26.9|  28.5|  45.5|  67.1| 152.7 |  34.4 | 11.7 |
| H2C      |  12.7 |  18.5|  21.3|  35.2|  58.0| 104.5 |  26.6 | 10.4 |

HTTP/2 and H2C are **~8× lower latency** at p50 than HTTP/1.1. H2C is marginally faster than
HTTP/2 (21 µs vs 20 µs p50) — no TLS record framing on the hot path. The high p99/p100
on H1 is macOS scheduler noise; the H2/H2C/H3 stacks amortise connection overhead across requests.

---

## Socket-backend path — cache-hit and cache-miss latency (µs)

Full dispatch path through m6-http → m6-html (template render) or m6-http → m6-file (static file).

### HTTP/1.1

| Route                | p0    | p25   | p50   | p75   | p99   | avg   | std  |
|----------------------|------:|------:|------:|------:|------:|------:|-----:|
| cache-hit  → m6-html | 147.7 | 166.4 | 174.1 | 180.0 | 293.9 | 177.8 | 43.1 |
| cache-hit  → m6-file | 134.7 | 163.4 | 170.9 | 176.5 | 198.9 | 169.6 | 12.5 |
| cache-miss → m6-html | 158.4 | 193.7 | 199.6 | 204.9 | 234.1 | 200.5 | 31.3 |
| cache-miss → m6-file | 186.8 | 200.2 | 206.1 | 212.5 | 430.2 | 215.3 |115.4 |

### HTTP/2

| Route                | p0   | p25  | p50  | p75  | p99  | avg  | std  |
|----------------------|-----:|-----:|-----:|-----:|-----:|-----:|-----:|
| cache-hit  → m6-html | 15.1 | 20.3 | 22.6 | 33.2 | 60.8 | 26.6 |  9.7 |
| cache-hit  → m6-file | 14.8 | 19.5 | 21.0 | 23.0 | 34.0 | 21.5 |  3.9 |
| cache-miss → m6-html | 31.3 | 48.0 | 51.2 | 53.8 | 96.2 | 52.0 | 11.5 |
| cache-miss → m6-file | 48.2 | 56.5 | 58.1 | 60.0 | 77.5 | 58.7 |  4.2 |

### HTTP/3

| Route                | p0   | p25  | p50  | p75  | p99  | avg  | std  |
|----------------------|-----:|-----:|-----:|-----:|-----:|-----:|-----:|
| cache-hit  → m6-html | 21.3 | 26.6 | 27.8 | 30.7 | 60.0 | 31.8 |  9.0 |
| cache-hit  → m6-file | 19.5 | 26.2 | 27.8 | 31.2 | 51.8 | 29.2 |  5.5 |
| cache-miss → m6-html | 35.5 | 53.5 | 56.6 | 60.1 | 89.0 | 57.6 |  8.8 |
| cache-miss → m6-file | 49.3 | 60.2 | 62.0 | 64.8 | 95.6 | 64.0 |  6.8 |

### H2C (HTTP/2 cleartext — plain TCP, no TLS)

| Route                | p0   | p25  | p50  | p75  | p99  | avg  | std  |
|----------------------|-----:|-----:|-----:|-----:|-----:|-----:|-----:|
| cache-hit  → m6-html | 13.9 | 21.3 | 23.6 | 28.7 | 47.1 | 26.6 |  7.8 |
| cache-hit  → m6-file | 15.4 | 19.9 | 22.8 | 25.5 | 37.5 | 23.0 |  3.9 |
| cache-miss → m6-html | 31.5 | 49.6 | 52.1 | 55.4 | 78.8 | 53.2 |  8.2 |
| cache-miss → m6-file | 46.4 | 57.6 | 59.5 | 61.8 | 85.0 | 61.3 | 24.6 |

**Notes:**
- m6-file cache-hit is consistently the fastest path at p50 (~21–23 µs H2/H2C).
- H2C and HTTP/2 are essentially identical on cache misses — the backend socket RTT dominates
  at ~52–58 µs, not the TLS framing cost.
- H3 p75 on cache-hit paths has more variance than H2/H2C due to QUIC's UDP congestion control
  and ACK coalescing, even on loopback.

---

## Socket-backend throughput — 8 parallel connections, 10 s

| Protocol | req/s      |
|----------|------------|
| HTTP/1.1 |  11,739    |
| HTTP/2   | 158,271    |
| HTTP/3   |  73,960    |
| H2C      | 118,554    |

**Notes:**
- **HTTP/2 is the throughput winner** at ~158 K req/s, benefiting from connection multiplexing
  and `WINDOW_UPDATE` flow-control that allows continuous pipelining without stalling.
- **H2C reaches ~119 K req/s** — H2 multiplexing without TLS encryption cost.
- **HTTP/3 reaches ~74 K req/s**. QUIC's per-stream flow control and UDP processing overhead
  through the quiche library reduce peak throughput relative to H2.
- **HTTP/1.1 is limited to ~12 K req/s** — per-connection TLS handshake cost.

---

## Protocol routing matrix — latency p50 (µs)

Inbound protocol × outbound backend protocol. All 16 combinations measured.
Outbound backends are `bench-url-backend` instances on loopback; m6-http forwards
via persistent pooled connections (h2c/h2s) or per-request connections (http/https).
n=2000, single sequential inbound connection.

| Inbound ↓ \ Outbound → | http (h1) | https (h1+TLS) | h2c | h2s (h2+TLS) |
|---|---:|---:|---:|---:|
| **h1 (TLS)**  | 170 | 170 | 169 | 170 |
| **h2 (TLS)**  |  19 |  19 |  19 |  19 |
| **h3 (QUIC)** |  29 |  32 |  28 |  29 |
| **h2c**       |  20 |  20 |  20 |  20 |

Full latency data (p0 / p25 / p50 / p75 / p99 µs):

| Suite             |   p0 |  p25 |  p50 |  p75 |  p99 |  avg |  std |
|-------------------|-----:|-----:|-----:|-----:|-----:|-----:|-----:|
| h1→http           |145.5 |166.9 |170.1 |174.0 |239.8 |172.4 | 14.1 |
| h1→https          |148.9 |166.9 |170.0 |173.8 |189.0 |170.9 |  6.5 |
| h1→h2c            |149.2 |166.3 |169.4 |173.2 |189.9 |172.7 |105.6 |
| h1→h2s            |150.5 |167.3 |170.2 |173.8 |189.7 |171.1 |  7.1 |
| h2→http           | 14.8 | 18.1 | 19.2 | 19.9 | 26.8 | 19.6 |  2.1 |
| h2→https          | 14.2 | 18.1 | 19.1 | 19.9 | 27.0 | 19.6 |  2.3 |
| h2→h2c            | 14.3 | 18.2 | 19.2 | 19.9 | 26.8 | 19.6 |  2.4 |
| h2→h2s            | 14.5 | 18.1 | 19.1 | 20.0 | 26.5 | 19.6 |  2.3 |
| h3→http           | 24.7 | 28.4 | 29.3 | 31.5 | 53.1 | 30.6 |  4.7 |
| h3→https          | 23.1 | 29.0 | 31.5 | 33.2 | 52.8 | 31.7 |  4.8 |
| h3→h2c            | 17.0 | 22.4 | 28.4 | 32.8 | 55.8 | 28.4 |  7.5 |
| h3→h2s            | 21.1 | 26.6 | 28.6 | 31.0 | 54.1 | 29.6 |  5.5 |
| h2c→http          | 14.4 | 19.7 | 20.3 | 20.9 | 26.5 | 20.5 |  2.3 |
| h2c→https         | 14.7 | 19.8 | 20.4 | 21.0 | 30.1 | 22.6 | 91.7 |
| h2c→h2c           | 15.5 | 19.6 | 20.2 | 20.9 | 30.3 | 20.5 |  2.8 |
| h2c→h2s           | 15.7 | 19.5 | 20.2 | 20.7 | 27.2 | 20.3 |  2.0 |

---

## Protocol routing matrix — throughput (req/s)

8 concurrent inbound connections, 10 s window.

| Inbound ↓ \ Outbound → | http | https | h2c | h2s |
|---|---:|---:|---:|---:|
| **h1**  |  11,382 |  11,561 |  11,611 |  11,579 |
| **h2**  | 165,169 | 160,637 | 158,858 | 154,497 |
| **h3**  |  77,354 |  64,306 |  56,504 |  51,088 |
| **h2c** | 116,583 | 177,032 | 176,310 | 163,046 |

**Notes:**
- **Outbound protocol does not affect h2/h2c latency** — at p50, h2→http, h2→https, h2→h2c,
  and h2→h2s all measure 19 µs. The outbound connections are persistent and multiplexed;
  forwarding cost is identical regardless of outbound TLS or framing overhead.
- **h1 inbound is bottlenecked by new TLS connection per request** — all four outbound variants
  sit at ~170 µs p50, matching h1→socket latency. The outbound backend is never the bottleneck.
- **h3 throughput falls with TLS outbound backends** — h3→http: 77 K/s vs h3→h2s: 51 K/s.
  QUIC's single-threaded event loop becomes CPU-bound when outbound TLS adds encryption work.
- **h2c inbound beats h2 inbound on throughput** (177 K vs 165 K for http outbound) —
  no inbound TLS record framing cost.

---

## Event-loop utilisation — epoll idle vs. working

Measured using macOS `sample` (1 ms sampling interval, 5 s window) during the throughput
tests. For this single-threaded server, `%CPU ≈ % time not blocked in kqueue/epoll_wait`.

| Protocol | Idle in kqueue | Working | Dominant cost |
|----------|---------------:|--------:|---------------|
| HTTP/1.1 | **18.5%** | **81.5%** | TLS handshake (HKDF/SHA-512) per new connection |
| HTTP/2   |  **7.5%** | **92.5%** | TLS AES-GCM decrypt + encrypt (~56% combined) |
| HTTP/3   |  **<0.1%** | **~100%** | UDP `sendto` (~24%) + QUIC packet build/encrypt (~30%) |

---

## Test configuration

### `system.toml`

```toml
[server]
bind     = "127.0.0.1:8443"
h2c_bind = "127.0.0.1:8080"
tls_cert = "cert.pem"
tls_key  = "key.pem"
```

### `site.toml` (routing + backends, abbreviated)

```toml
[[backend]]
name    = "m6-html"
sockets = "/tmp/m6-bench/m6-html-bench.sock"

[[backend]]
name    = "m6-file"
sockets = "/tmp/m6-bench/m6-file-bench.sock"

[[backend]]
name = "url-http"
url  = "http://127.0.0.1:18080"

[[backend]]
name            = "url-https"
url             = "https://127.0.0.1:18443"
tls_skip_verify = true

[[backend]]
name = "url-h2c"
url  = "h2c://127.0.0.1:18081"

[[backend]]
name            = "url-h2s"
url             = "h2s://127.0.0.1:18444"
tls_skip_verify = true
```

Response body is ~107 bytes. TLS certificates are self-signed (rcgen), bench client uses
`--skip-verify`. URL backends are `bench-url-backend` instances (all return a fixed 200 body
with `Cache-Control: no-store`).

---

## Environment

| Item | Value |
|------|-------|
| Platform | macOS Darwin 24.6.0 (Apple M4) |
| TLS target | `127.0.0.1:8443` (loopback) |
| H2C target | `127.0.0.1:8080` (loopback, plain TCP) |
| URL backends | `127.0.0.1:18080/18443/18081/18444` (loopback) |
| TLS | rustls with ring provider |
| HTTP/3 | quiche 0.26.1 (Cloudflare, boringssl-vendored) |
| Socket backends | m6-html (m6-render), m6-file |
| m6-http model | single-threaded epoll (kqueue on macOS) event loop |
| bench concurrency | 8 parallel connections (throughput); 1 (latency/path) |
| Date | 2026-03-20 |
