# m6-http comparative positioning

This page positions m6-http against the servers a reader would otherwise choose.
It quotes m6's measured figures from `docs/BENCHMARKS.md`, compares the architectures, and names where m6 wins and where it does not.
The conclusion is that m6 trades multi-core scale for a zero-*IPC* (inter-process communication) cache hit, which wins for *HTTP* (Hypertext Transfer Protocol) 2 cache serving and does not win for HTTP/1.1 throughput or large files.

`docs/BENCHMARKS.md` is the only authority for a number about m6.
Every m6 figure on this page is quoted from there with its conditions, and a figure quoted anywhere else without those conditions is wrong.

## 1. Contents

The sections run from m6's own figures to the architecture that produces them, to the comparison, to where m6 wins and loses, to headroom.

2. [m6's measured figures](#2-m6s-measured-figures)
3. [Why the architecture differs](#3-why-the-architecture-differs)
4. [How other servers compare](#4-how-other-servers-compare)
5. [Where m6 wins and where it does not](#5-where-m6-wins-and-where-it-does-not)
6. [Headroom](#6-headroom)
7. [Summary](#7-summary)

## 2. m6's measured figures

Two cache-hit figures matter for positioning, and they measure different things.
Table 1 states both.

| figure | what it measures | source |
|---|---|---|
| 2.2 µs | a cache hit inside m6, on its own timer | `BENCHMARKS.md` section 6 |
| 225.7 µs and 259.2 µs | an HTTP/2 cache-hit *p50* (median) end to end, two runs | `BENCHMARKS.md` Table 3 |

*Table 1: the two cache-hit figures, and what each measures.* Take from it that 2.2 µs is what the cache costs and 225.7 µs is what a client on the same host sees, so quoting one as the other is the error to avoid.

The two figures were measured on the 4 vCPU virtual private server in `BENCHMARKS.md` Table 2, over loopback, at concurrency 2, with *TLS* (Transport Layer Security) 1.3.
`BENCHMARKS.md` Table 3 records the latency for all six suites, and Table 4 records the throughput.
The throughput figures are 7154.1 req/s and 6711.2 req/s for HTTP/2, 2085.1 req/s and 1889.3 req/s for HTTP/1.1, and 1030.3 req/s and 1047.0 req/s for HTTP/3.

`BENCHMARKS.md` section 6 draws the two ratios this page relies on.
HTTP/2 is 2.6x to 3x faster than HTTP/1.1 on a cache hit, and 3.4x to 3.6x faster on throughput.
A cache miss costs at least 4 ms.

Section 3 explains the architecture that produces these figures.

## 3. Why the architecture differs

m6-http is single-threaded, and it holds its response cache in the same process as its TLS stack.
A multi-process or multi-threaded server keeps its cache in shared memory or another process, so every hit pays a lock or an IPC round trip.

The in-process cache is an *LRU* (least recently used) map of `Arc<Bytes>` values.
A cache hit is a hash lookup, a reference-count increment, and an *AES-GCM* (Advanced Encryption Standard in Galois/Counter Mode) seal.
There is no copy, no lock, and no IPC.

The cost of that design is one core per process.
A single thread cannot use more than one *CPU* (central processing unit) core at once, so m6 scales by running more instances behind a load balancer.

Section 4 compares that design against the servers a reader would otherwise pick.

## 4. How other servers compare

Read Table 2 as an architectural comparison.
The capability columns come from each project's own documentation.
Nothing here was run like-for-like on one machine against one payload.
The figures are indicative of scale.
`docs/BENCHMARKS.md` is the only authority for a number about m6.

| server | in-process response cache | terminates TLS | source |
|---|---|---|---|
| nginx | no | yes | nginx documentation |
| HAProxy | yes, shared behind a lock | yes | HAProxy documentation |
| Envoy | experimental | yes | Envoy documentation |
| H2O | no general-purpose cache | yes | H2O documentation |
| OpenResty | partial, a shared dictionary | yes | OpenResty documentation |
| Varnish | yes | needs a separate terminator | Varnish documentation |
| Pingora | experimental | yes | Pingora documentation |
| m6-http | yes, `Arc<Bytes>`, no lock | yes | this repository and `BENCHMARKS.md` |

*Table 2: the servers m6-http is compared against, by capability.* Take from it that m6-http is the only row that holds a general-purpose response cache and TLS termination in one process.

`docs/m6-architecture.md` section 1.3 gives the full argument for why a cache hit must not cross a process boundary, and what that boundary costs.
Section 5 turns the comparison into where m6 wins and where it does not.

## 5. Where m6 wins and where it does not

The wins and losses below follow from `BENCHMARKS.md`, and each names the figure it rests on.

**Where m6 wins.**

- **HTTP/2 over HTTP/1.1.**
  HTTP/2 is 2.6x to 3x faster on a cache-hit p50, and 3.4x to 3.6x faster on throughput (`BENCHMARKS.md` section 6).
- **A zero-IPC cache hit.**
  The 2.2 µs internal cache hit holds no copy, no lock, and no IPC (`BENCHMARKS.md` section 6).
- **A deterministic hot path.**
  One thread means no lock contention and no cross-core cache traffic, so the hot path has no convoy spikes.

**Where m6 does not win.**

- **HTTP/1.1 throughput.**
  HTTP/1.1 is the slowest of the three on throughput, at 2085.1 req/s and 1889.3 req/s (`BENCHMARKS.md` Table 4).
  The HTTP/1.1 path also has a synchronous backend call that blocks the event loop during a cache miss.
- **Multi-core scale within one instance.**
  One m6-http process uses one core, and nginx, HAProxy and Envoy use multiple cores in one instance.
- **Large files.**
  m6-http has no `sendfile()` path, so it cannot hand a file to the kernel for zero-copy transfer.

Section 6 states what would change these positions.

## 6. Headroom

The figures in section 2 are the current measurements, and they come from the Linux virtual private server in `BENCHMARKS.md` Table 2.
`BENCHMARKS.md` records no macOS measurement, so no macOS figure is claimed here.

Two Linux features would change the HTTP/2 result, and neither is measured.
Kernel TLS would offload the AES-GCM work that the TLS path performs in userspace.
`io_uring` would batch socket sends.
Both are projections, and neither figure appears in `BENCHMARKS.md`.

Section 7 closes the page.

## 7. Summary

m6-http is a single-threaded reverse proxy with an in-process cache and TLS termination in one process.
That design wins for HTTP/2 cache serving, where the internal cache hit is 2.2 µs and the end-to-end p50 is 225.7 µs to 259.2 µs.
It does not win for HTTP/1.1 throughput or for large files, and it scales by running more instances.

The comparison to other servers is architectural, and every m6 figure on this page is quoted from `docs/BENCHMARKS.md`.
