# m6-http — Comparative Positioning

This document places m6-http's measured performance in context against nginx, HAProxy,
Envoy, H2O, and OpenResty. It covers the architectural reasons for the differences,
where m6 wins, where it doesn't, and what the headroom looks like on Linux.

All m6 numbers are from `bench.sh` on macOS Darwin 24.6.0 (Apple Silicon ARM64),
loopback `127.0.0.1:8443`, TLS (rustls + ring), n=2000 requests, 8 concurrent
connections for throughput. See `BENCHMARKS.md` for full tables.

---

## What makes m6-http architecturally different

Most HTTP servers in this category are multi-process or multi-threaded. They need
shared caches (shared memory, memcached, Redis, disk) because each worker has its own
address space. That shared cache requires serialisation, locking, and at minimum one
extra IPC round-trip per cache lookup.

m6-http is single-threaded. Every request — TLS decrypt, route lookup, cache lookup,
optional backend call, TLS encrypt — runs on one thread in one process. The cache is
an `Arc<Bytes>` LRU in the same heap. A cache hit is:

```
recv() → TLS decrypt → route hash → LRU lookup → Arc clone → TLS encrypt → send()
```

There is no copy, no lock, no IPC. The `Arc` reference is incremented once; the
encrypted TLS record is written from the same allocation that was stored on the first
request. The critical-path criterion benchmarks measure the route+cache work at ~22 ns.

The trade-off is that a single thread cannot use multiple CPU cores simultaneously.
m6 scales horizontally (multiple instances behind a load balancer, or multiple backend
workers auto-discovered via socket globs) but each m6-http process is inherently
single-core.

---

## Benchmark numbers

### Latency — single sequential connection (µs)

| Protocol | p50   | p99   |
|----------|------:|------:|
| HTTP/1.1 | 163.8 | 472.7 |
| HTTP/2   |  22.0 |  70.1 |
| HTTP/3   |  28.0 |  94.3 |

H2 and H3 are ~7× lower than H1 at p50. The H1 cost is dominated by the TLS
handshake on each new connection; H2/H3 reuse a single connection.

### Throughput — 8 concurrent connections, 10 s, warm cache (req/s)

| Protocol | req/s   |
|----------|--------:|
| HTTP/1.1 |  11,857 |
| HTTP/2   | 158,323 |
| HTTP/3   |  77,672 |

### Event-loop utilisation during throughput (macOS `sample`, 1 ms interval)

| Protocol | Idle in kqueue | Working | Dominant CPU cost |
|----------|---------------:|--------:|-------------------|
| HTTP/1.1 |         18.5%  |  81.5%  | TLS handshake (HKDF/SHA-512) |
| HTTP/2   |          7.5%  |  92.5%  | TLS AES-GCM decrypt+encrypt (~56%) |
| HTTP/3   |         <0.1%  |  ~100%  | UDP sendto (~24%) + QUIC packet build/seal (~30%) |

H3 is fully CPU-saturated at 77K req/s. H2 at 158K req/s is dominated by
userspace AES-GCM; the cache and routing logic accounts for only ~6% of working time.

---

## Comparison to other servers

### HTTP/1.1 throughput (single core, TLS, small response)

| Server | req/s | Notes |
|--------|------:|-------|
| nginx (1 worker, wrk, real network) | ~13,400 | http2benchmark.org |
| nginx (1 worker, loopback) | ~23,600 | Cloudflare blog |
| HAProxy (1 worker, loopback, keep-alive) | ~39,000 | ProxyBenchmarks |
| HAProxy (i7, in-memory redirect, TLS 1.2) | ~180,000 | HAProxy blog |
| Envoy (1 thread) | ~26,000 | ProxyBenchmarks |
| **m6-http** | **11,857** | this repo, macOS loopback |

**m6 is behind nginx and HAProxy for H1.** The gap is mainly:

1. The bench client reconnects periodically — TLS handshakes (HKDF/SHA-512 visible
   in profiler) consume ~18% of server CPU between idle gaps.
2. nginx benefits from `sendfile()` for static content and mature H1 connection
   keep-alive management tuned over many years.
3. `forward_to_backend()` in m6-http is a synchronous blocking Unix socket call.
   During a cache miss it stalls the entire event loop. For the throughput test the
   cache is warm after the first request, but the reconnect overhead remains.

H1 is not m6's primary serving path; H2 and H3 are.

### HTTP/2 throughput (single core, TLS, small response)

| Server | req/s | Notes |
|--------|------:|-------|
| nginx (h2load, real network) | ~17,500 | http2benchmark.org |
| LiteSpeed (h2load, real network) | ~83,800 | http2benchmark.org |
| nginx (h2load, high concurrency) | ~58,700 | nghttp2 docs |
| H2O (single process, loopback, Linux + KTLS) | ~325,000–338,000 | h2o.examp1e.net |
| **m6-http** | **158,323** | this repo, macOS loopback |

**m6 significantly outperforms nginx and LiteSpeed for H2.** The efficient multiplexing
(multiple streams per connection, `WINDOW_UPDATE` flow control maintained correctly) and
the in-process `Arc<Bytes>` cache combine to serve responses with minimal overhead.

H2O is faster. Its advantage is kernel TLS (`TCP_ULP` / `SOL_TLS` on Linux) which
offloads AES-GCM encryption/decryption to the kernel's sendmsg path — eliminating the
~56% of H2 CPU time that m6-http currently spends in rustls AES-GCM. H2O also uses
`sendfile()` for file content. Both of these are Linux-only.

### HTTP/3 throughput (single core, QUIC/UDP)

No meaningful published baseline exists for H3 at single-core scale. Nginx added H3
in 1.25.x but hasn't been widely benchmarked in single-worker configurations.
m6-http's 77K req/s is the reference point; note that H3 is fully CPU-saturated at
this rate (kqueue idle <0.1%), so the number reflects the limit of a single core
running the quiche QUIC stack with userspace AES-GCM, not any I/O ceiling.

### Proxy caches with in-process response caches

There is no widely-deployed proxy that combines in-process in-memory caching with TLS
termination in a single-threaded event loop in the way m6-http does.

| Proxy | In-process cache | TLS | H2 | Notes |
|-------|:----------------:|:---:|:--:|-------|
| HAProxy | Yes (in-memory, RFC 7234) | Yes | Yes | Multi-process; cache shared via shm (locked); no H2 cache benchmarks published |
| Envoy | Yes (SimpleHttpCache, experimental) | Yes | Yes | Much heavier per-request overhead; ~26K H1 baseline |
| OpenResty | Partial (lua_shared_dict, string-based) | Yes | Yes | Requires Lua scripting; shm across workers needs lock |
| Pingora (Cloudflare) | Yes (pingora-memory-cache, experimental) | Yes | Yes | Library, not a binary; caching APIs unstable |
| Varnish | Yes (in-process, excellent) | No — needs separate TLS terminator | No | Best-in-class HTTP cache but no TLS |
| H2O | No general-purpose in-memory cache | Yes | Yes | Fastest TLS (KTLS), no cache |
| **m6-http** | **Yes — Arc<Bytes>, zero-copy, no lock** | **Yes** | **Yes** | **Single-threaded, no IPC for cache hits** |

The nearest production equivalent is Cloudflare's internal Pingora deployment, not the
open-source version. The open-source Pingora caching APIs remain experimental.

Varnish + a TLS terminator (hitch or nginx) is the most mature combination for
high-throughput HTTP caching, but the TLS terminator and cache are separate processes:
every cache hit still crosses a process boundary (Unix socket round-trip, ~20–50 µs)
before the TLS layer can send the response. For m6-http's ~22 µs H2 round-trip, that
IPC hop would more than double end-to-end latency.

---

## Where m6 wins

- **H2 single-core throughput**: ~158K req/s against nginx's ~17–59K. The
  `Arc<Bytes>` in-process cache, correct H2 flow-control, and stream multiplexing
  combine to maximise the throughput of one core.
- **H2 cache-hit latency**: p50 ~19–22 µs (path suite). The only work is a hash
  lookup, an `Arc` clone, HPACK encoding, and AES-GCM seal. No IPC, no syscall
  (other than the TLS send), no allocation on the hot path.
- **No external dependencies**: no Redis, no memcached, no Varnish, no separate
  cache tier. The entire hot path — TLS + routing + cache + auth verify + proxy —
  runs in one process.
- **H3 correctness and performance**: 77K req/s on a fully CPU-saturated single
  core via the quiche QUIC stack with proper H3 stream multiplexing.
- **Deterministic tail latency**: single-threaded means no lock-convoy spikes,
  no cross-core cache coherence traffic, no NUMA effects. The p99/p50 ratio for
  H2 cache hits is ~3× (70 µs / 22 µs); for a multi-threaded cache this ratio
  is typically 5–10×.

## Where m6 doesn't win

- **H1 throughput**: nginx is ~2× faster for HTTP/1.1. nginx has a 20-year head
  start on H1 connection management, `sendfile()` integration, and kernel-bypass
  tuning. m6-http's H1 path also has a synchronous blocking backend call that
  stalls the event loop during cache misses.
- **Multi-core utilisation**: one m6-http process = one core. nginx, HAProxy,
  Envoy, and H2O all use multiple cores within a single server. m6 relies on
  running multiple instances (or backends) for horizontal scale.
- **H2O on Linux with kernel TLS**: H2O + KTLS reaches ~325–338K H2 req/s by
  pushing AES-GCM into the kernel. m6-http's ~158K is the ceiling on macOS
  (no KTLS). On Linux with `rustls-ktls`, m6-http could reach a similar range.
- **Large responses**: m6-http copies all content through userspace buffers.
  nginx and H2O use `sendfile()` / zero-copy for file content; for responses
  larger than a few KB their throughput advantage grows.

---

## Performance ceiling and headroom

### On macOS (current)

macOS has no kernel TLS, no `SO_BUSY_POLL`, and no `io_uring`. The numbers in
`BENCHMARKS.md` are close to the macOS ceiling for a single-threaded userspace
TLS server. The ring crate already uses ARM Crypto Extensions (2,829 `aese`
instructions confirmed in the release binary) so AES-GCM is hardware-accelerated.

### On Linux (projected)

| Change | Expected impact |
|--------|----------------|
| Kernel TLS (`rustls-ktls`, `TCP_ULP`) | Eliminate ~56% of H2 CPU → ~350K H2 req/s |
| `io_uring` batched sends | Reduce H3 UDP `sendto` overhead (~24% of H3 CPU) → ~100K H3 req/s |
| Async backend calls (non-blocking Unix socket) | Eliminate event-loop stall on cache miss → better H1 throughput and lower tail latency |
| `SO_BUSY_POLL` | Reduce kqueue/epoll latency → lower H2/H3 p99 |
| OpenOnload / ExaSock (kernel bypass, production) | 3–5× lower tail latency, 2–3× higher throughput |

The single most impactful change is kernel TLS, which would bring H2 throughput
into H2O territory (~325K) while keeping the in-process zero-copy cache architecture
intact.

### Tokio

Tokio is already a dependency (used by the `h2` crate and `tokio-rustls`). Switching
the main event loop from the hand-written kqueue loop to a Tokio `current_thread`
executor would change the abstraction layer but not the syscalls or the throughput.
The bottleneck is AES-GCM, not the event loop. The one useful Tokio path would be
`tokio-uring` on Linux for batched H3 UDP sends.

---

## Summary

m6-http occupies a niche that few production servers fill: a **single-threaded,
in-process-cached, TLS-terminating reverse proxy** with H1 + H2 + H3 on the same
port. The design trades multi-core utilisation for zero-lock, zero-IPC, zero-copy
cache hits. On a single core, that trades out well for H2 and H3: 158K and 77K req/s
respectively, against nginx's ~17–59K H2.

The gap to H2O (the fastest single-core H2 server) is kernel TLS, which is a
Linux-only feature not yet integrated into the rustls stack used here. That gap is
bridgeable without changing the single-threaded architecture.

For H1, nginx remains the mature choice. For H2/H3 at single-core scale with an
integrated in-memory cache, m6-http has no direct production equivalent.
