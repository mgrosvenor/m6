# Performance: every number, and how it was taken

This document records what each component of m6 costs, which change made it cost that, and the conditions each figure was taken under.
Section 1 gives the rules every figure follows, sections 2 to 7 give the figures, section 8 gives the commands that re-take them, and section 10 lists what has no figure.

## Contents

1. [What this document records](#1-what-this-document-records)
2. [Per-request cost in m6-core](#2-per-request-cost-in-m6-core)
3. [Allocation](#3-allocation)
4. [Cache-hit latency, and why it is load dependent](#4-cache-hit-latency-and-why-it-is-load-dependent)
5. [Connection setup, per channel](#5-connection-setup-per-channel)
6. [Where a TLS handshake goes](#6-where-a-tls-handshake-goes)
7. [Certificate compression and the QUIC amplification limit](#7-certificate-compression-and-the-quic-amplification-limit)
8. [Reproducing these measurements](#8-reproducing-these-measurements)
9. [Event-loop accounting and queue costs](#9-event-loop-accounting-and-queue-costs)
10. [What is not measured](#10-what-is-not-measured)

---

## 1. What this document records

m6 keeps performance numbers in three places and each answers a different question.
Table 1 names the three places and what each one answers.

| file | what it answers | authority |
|---|---|---|
| [`BENCHMARKS.md`](BENCHMARKS.md) | what a client observes end to end | authoritative for any end-to-end figure |
| `tools/perfcheck.sh` | the merge gate: no consistent slowdown against the base build | authoritative for the gated render targets |
| this document | what an individual component costs, and which change changed it | authoritative for per-component cost |

**Table 1: where each kind of performance number lives.**
Take from it that an end-to-end claim belongs in `BENCHMARKS.md` and a gate verdict in `tools/perfcheck.sh`, an interleaved A/B whose run records are in `tools/perf-history.jsonl`.

Four rules govern every figure below, and Table 2 states them.

| rule | what it requires |
|---|---|
| conditions with every number | the machine and the method. CPU, core count, memory, load and the config that produced the figure all belong here. A laptop figure and a build-host figure never share a column |
| an unmeasured change says so | "structural" means the reasoning is sound and nobody has put a number on it |
| comparisons are paired and interleaved | run A, B, A, B against a fixed baseline, so drift hits both sides equally |
| reproducible sources | a figure comes from a loopback instance, `m6-examples` or the build host, described by its hardware and with no host, domain or node name |

**Table 2: the four rules every number in this document follows.**
Take from it that a figure carries its hardware, its config and its method.

---

## 2. Per-request cost in m6-core

`App` built every request's dictionary by copying the whole of a service's static configuration into an empty map.
Removing that copying took a rendered page from 2.79 ms to 1.64 ms and cut core's own copying by a factor of 280.
This section gives the end figure, the per-copy audit behind it, the changes, and the one cost left in place.

### 2.1. The rendered page

Two builds were run against the same site directory, the same renderer config and the same content file, over a unix socket, five interleaved rounds of 300 requests each.
The figure is the median of each round's p50, and Table 3 gives the two medians.

| build | p50 per rendered page |
|---|---:|
| before the copy removal | 2.79 ms |
| after it, the branch tip of 2026-09-13 | 1.64 ms |

**Table 3: cost of one rendered page, before and after the copy removal, a 41% reduction.**
Taken on a laptop, release build, with a fresh connection per request, whose cost is identical on both sides and cancels in the comparison.

Both builds answer `Content-Length: 16660` with ETag `"c97d83c5076770c"`, a content hash, so the pages match byte for byte.
The newer build emits `Connection: close` when the client asked for it, which is 19 bytes.

### 2.2. Where the 1.15 ms went

Each request assembled its dictionary from the config file's keys plus the site's content file and deep-copied it six times, producing the same result for every request.
`m6-core`'s `copy_audit` measures each copy separately, in release, against a real content file of 68 KB and 1,364 nodes.
Table 4 lists the six copies with the change that removed each and its cost before and after.

| copy | removed by | before | after |
|---|---|---:|---:|
| config and `site_dir` out of the read lock | `Arc<RendererConfig>` | 458 ns | 42 ns |
| the matched route | routing returns a borrow | 167 ns | 42 ns |
| `build_dict`, three copies inside | a per-route base dictionary built once per reload | 222,708 ns | 1,125 ns |
| `raw.clone()` into `Request` | `serve_connection` hands the request to its handler | 458 ns | 0 |
| `dict.clone()` into `Request` | the layered `Dict` | 108,583 ns | 375 ns |
| `dict.clone()` in `render_response` | the layered `Dict` | 109,500 ns | 0 |
| `tera::Context`, inside the engine | left in place, see 2.5 | 189,000 ns | 143,417 ns |
| **total per request** | | **630,874 ns** | **145,001 ns** |

**Table 4: the six per-request copies, each measured separately.**
Take from it that core's own copying fell from about 442 µs to about 1.58 µs, a factor of 280, and that the remaining 143 µs is inside the template engine.

### 2.3. The change

The fix has three parts:

- `Arc<RendererConfig>` replaces the copy of the renderer config.
- A per-route base dictionary is built once per reload.
- `m6-core/src/dict.rs` holds a layered `Dict`, a shared base plus a per-request overlay.

The route table is shared, so routing returns a borrow, and `serve_connection` hands the request to its handler.

### 2.4. The merge order the change preserves

The dictionary is assembled from a fixed order of sources, and the order decides which value wins.
Built-in keys are merged after params files, so a params file cannot override `year`, `datetime` or `request_path`.

The layering keeps that order because built-ins live in the overlay and params files in the base.
The first three steps of the original order produced the same map for every request on a route, so they moved into `route.base_dict` behind an `Arc`.
`build_dict` still merges what varies per request: per-request params files, path params, query params, form fields, cookies, built-ins and auth keys.
The test `dict::tests::a_base_entry_can_never_override_an_overlay_one` in `m6-core/src/dict.rs` holds that order.

### 2.5. The template engine floor

`tera::Context::insert` calls `to_value`, which deep-copies every entry into the engine's own `BTreeMap`, and that accounts for 143 µs of the remaining 145 µs.
Against a minimal template, building the context is 99.6% of the work, at 178,875 ns to build against 750 ns to render.
The cost scales with the size of the context, so a page touching three keys still pays to copy all 1,364 nodes.

The engine's cost is accepted.
A site can lower it: a content file named as both `global_params` and the route's `params` enters every page's context twice.

### 2.6. The borrowed response head

A rendered page's header names and values are set up once and borrowed, and the page's own bytes are copied out of the page area once, as the minified body.
Table 5 lists the allocation counts, which the `site_allocs` and `request_allocs` tests in `m6-core/tests` measure.

| path | before | after the borrowed head | after the reusable context (2.7) |
|---|---:|---:|---:|
| `site_allocs`, page path | 50 | 41 | 36 |
| `request_allocs`, template request | 36 | 27 | 22 |
| `request_allocs`, handler request | 23 | 21 | 16 |

**Table 5: allocations per request on the page and handler paths, after each of the two changes.**
Take from it that the current budgets are 36, 22 and 16, and that 2.7 records what those allocations are.

What changed:

- `Response.headers` holds `HeaderText` pairs. A `HeaderText` borrows a `&'static str`, shares an `Arc<str>`, holds up to 23 bytes inline, or owns a `String`, and it derefs to `str`.
- A route's `Cache-Control`, `Last-Modified` and extra headers are made once, when the route loads, as `Arc<str>` texts that the route owns, and each response clones the `Arc`, which increments a count. A reload that replaces the site frees the old texts once the responses still sending them are done, so nothing grows across reloads. The header names `Content-Type`, `Content-Encoding`, `ETag` and `Cache-Control` are literals.
- The `ETag` is at most 18 bytes (16 hex digits and two quotes), so it is held inline in the header. It is computed from the final bytes, so the page carries its validator to the caller and to the edge's cache, as the cache-header tests require, and `a_page_defaults_to_revalidation_and_carries_an_etag` reads it from `Response.headers`.
- A rendered page stays in the page area until the minifier has read it, so the copy out of the area is the minified body. A page that is not minified is copied once.

Only the minifier reads the page area directly, because `Body::Bytes` holds an owned `Vec<u8>` that the compression stage and the caller take.

### 2.7. The reusable page context

A rendered page makes no allocation of its own.
A traced `site_allocs` run shows all 36 page-path allocations are minify-html's 21 and Tera's 15.
The 16 handler-request allocations are the owned request (10), its cookies (5) and the route handle (1), which stay because a handler is handed a `Request` that owns them.

What changed:

- The dictionary's per-request layer is a list, taken from a spare one that the last request left on the thread. The service runs one loop on one thread, so the spare is the service's own reusable dictionary. It is cleared and never shrinks.
- The three built-in values, `request_path`, `datetime` and `year`, are written into the `String` buffers the last request left for those keys. A spare overlay keeps only those three, each at most 8 KiB.
- `datetime` and `year` are formatted into stack buffers, and no format string is parsed.
- A response's header list goes back to a spare list on the thread when the response is dropped, with sixteen headers of room, and the next response takes it.
- The overlay's size follows the `request_dict` limit, 32 entries at start and 4096 at most. A request with more fields is refused with a 400 that names the key.

### 2.8. mimalloc as the global allocator, and whether it stays

Every m6 service (m6-http, m6-file, m6-html, m6-md, m6-auth-server and m6-monitor) installs `m6_core::alloc::M6Alloc`, which is mimalloc, and every allocation-counting test installs the same type, so the counts are taken against the allocator production runs.
The A/B in section 8.2 tests it against the base build: cache-hit and render latency no worse, and the memory-call and page-fault counts at zero or a named handful.

A warm service should make no `brk`, `mmap`, `munmap`, `madvise`, `mremap` or `mprotect` call.
That takes four settings, made at start and listed in Table 6:

| setting | what it does | where |
|---|---|---|
| purge delay -1 | mimalloc never hands freed memory back, so the heap grows and does not shrink | `m6_core::alloc::never_return`, first line of `main` |
| `[limits.heap_reserve]` | one committed reservation taken from the OS at start, on explicit huge pages when mimalloc finds a pool and otherwise advised `MADV_HUGEPAGE` for transparent ones (the common `madvise` mode only backs an advised range). m6-http defaults it to the `cache_bytes` maximum plus 64 MiB, rounded to 2 MiB, so the cache lives in it | `m6_core::alloc::reserve` |
| `[server] lock_memory` (default `true`) | `mlockall(MCL_CURRENT \| MCL_FUTURE \| MCL_ONFAULT)` on Linux: a page is pinned as the load first touches it, so a hot page is never a page fault and memory that is reserved and unused is not resident | m6-http, after the reservation and before serving |
| growth past the reservation | allowed, never a failure, logged once | `m6_core::alloc::note_growth`, called from each service's accounting tick |

**Table 6: the settings that keep a warm service out of the kernel.**
Take from it that the reservation is sized from the cache, so the hot data sits in one pinned region.

The measured window took zero memory calls and zero minor faults on both the base and the candidate build.
Table 7 gives the resident memory measured on the build host, a Linux machine:

| state | resident, pinned build (KiB) | resident, base build (KiB) | pinned build, on transparent huge pages (KiB) |
|---|---:|---:|---:|
| one cached page | 129,028 | 95,712 | 104,448 |
| 128 MiB cache full | 277,632 | 228,788 | 235,520 |

**Table 7: resident memory of m6-http with the on-fault pin, against the base build.**
Take from it that the pin costs 33 to 49 MiB over the base, and that most of the pinned build's memory sits on transparent huge pages.

`m6-http/tests/heap_resident.rs` is Linux only, so the run behind these figures is the build host's.
`tools/memcalls.sh` reports the resident set after the window, `tools/hwcounters.sh` reads `AnonHugePages` and `Hugetlb` from `/proc/<pid>/smaps_rollup`, and `cargo nextest run -p m6-http --test heap_resident` asserts the bound.

---

## 3. Allocation

Two changes stopped `m6-file` reading bytes it never sent, and `m6-http` counts every allocation per request against a budget.
Neither file-path change carries a latency figure, so both are recorded as structural.
Table 8 describes each change and its measurement.

| change | what it does | measurement |
|---|---|---|
| HEAD from metadata | A HEAD is answered from `metadata.len()` without opening the file, when the representation is the file, and a directory is handled the same way | Structural. A HEAD on a large image previously paid a whole `fs::read`, then minification, then brotli at level 6, to produce bytes nobody receives |
| streaming response bodies | `Response.body` is a `Body`, either `Bytes(Vec<u8>)` or `Stream { len, reader }`, so a file reaches the wire without being materialised | Structural. A 3.6 MB asset was read into a `Vec` on every cache miss to be copied straight out again |

**Table 8: the two allocation changes on the static file path.**
Take from it that both are structural, and that the second removed a 3.6 MB allocation per cache miss on the largest asset.

In `m6-core/src/response.rs`, `as_bytes()` returns `None` for a stream, so the minifier, the compressor and the default content-hash ETag skip a body core has not read.

### 3.1. Allocations per request

`m6-http` counts its own heap allocations behind the `count-allocations` feature, which is off by default.
A plain build compiles the counting and the arm and stop calls out, and `m6_core::alloc::M6Alloc`, the allocator every binary and every counting test installs, is plain mimalloc without the feature.
With the feature on, the run loop arms `m6_core::testkit::alloc_count` when it starts and logs `calls` and `bytes` once at stop.
The count covers every `alloc`, `alloc_zeroed` and `realloc` the loop thread made while serving, from m6-http, m6-core, rustls, quiche and every other Rust library in the process.

Run the test with the feature, serially:

```text
cargo test -p m6-http --test allocations --features count-allocations -- --test-threads=1 --nocapture
```

`tests/allocations.rs` starts the binary cargo built with the feature, sends one warm-up request, then 1,000 requests on one connection and 2,000 on another, sends SIGTERM and reads each run's mark and stop lines.
The server reads the counter at the first turn after the warm-up response is on the wire (`M6_ALLOC_MARK_AFTER=1`) and again at stop, and the per-request figure is `((2N stop - 2N mark) - (N stop - N mark)) / N`.
The one TLS handshake and the one backend-connecting first request in a run sit before that run's mark, so they cancel inside the run.
Table 9 gives the four rows, measured on macOS on 2026-11-15, and the test asserts the budgets.

| row | what it runs | calls per 1,000 requests | budget | what the calls are |
|---|---|---:|---:|---|
| `h1_tls_hit` | a cached page on one keep-alive connection over TLS | 2,000 | 2,001 | two rustls record buffers per request |
| `h2_tls_hit` | the same over HTTP/2 | 3,000 | 3,001 | three rustls record buffers per request |
| `h1_tls_miss_insert` | a new query string each time, so each request reaches the backend and is stored | 47,001 | 47,004 | the cache entry keeps its header names and values in one byte arena, so a miss builds the arena once where it cloned twelve strings |
| `h1_tls_new_connection_per_connection` | a cached page with a full TLS handshake per request | 117,000 per 1,000 connections | none | reported only |

**Table 9: the allocation rows of `m6-http/tests/allocations.rs`, in calls.**
Take from it that our code allocates nothing on either hit path, and that a miss is a slow path whose budget is a ceiling.

The budgets are the top of each row's observed spread with nothing added, and they only go down.
Each change lowers the row it attacks, and a row above its budget fails the test.
The new-connection row varies with the server's read sizes (122.859 to 123.849 calls per connection across the runs that fixed its budget), so a per-connection ceiling would fail on that variation and the row is reported only.

**The h1 allowance.**
The h1 hit path makes 2,000 calls per 1,000 requests, all rustls's record buffers (read 2.000 over fourteen runs), and the budget adds one call for a run whose received-record count is one higher than its neighbour's.
The counter is read at a mark after the warm-up, so the run's one TLS handshake is in neither reading.
Its calls move with the number of reads its flight arrives in (139 or 140 calls for an unsplit handshake, 141 or 142 when the ClientHello arrives in two reads), which is why a per-request figure must exclude it.

**The h2 allowance.**
The h2 hit path makes 3,000 calls per 1,000 requests, read 2,999 to 3,001 over fourteen runs.
The extra call is one more TLS record received in a run, one `Payload::into_vec` in `CommonState::take_received_plaintext` that can grow rustls's received-plaintext chunk queue.
The allowance is one call per run, so it does not grow with the request count.

**The miss row.**
The miss row's one call above 47,000 is the single cache-table growth its 1,000 inserts trigger, and its observed spread across runs was 46.998 to 47.004.

**The close path.**
The h1 and h2 TLS and plain close paths build the `io::Error` from its kind, so closing a connection allocates nothing of ours.

---

## 4. Cache-hit latency, and why it is load dependent

`hit_p50_ns` is a load-dependent measurement.
Read it with the hit count of the window it came from, because it tracks request density and the cache lookup is flat.
This section records the cache lookup cost, what the metrics line reports, and the rule for reading the number.

**Three source comments cite this section by number**, in `m6-http/src/stats.rs` and `m6-monitor/src/digest.rs`.
Renumbering it breaks them.

### 4.1. The cache lookup is flat

A paired, interleaved A/B across five commits spanning 2026-09-09 to 2026-09-13 measured the cache lookup on a single-core VM with a `kvm-clock` clocksource.
The A/B used a portable bench written against the API common to all five commits.
Table 10 gives the lookup cost across the five commits and across cache sizes.

| variable | cache lookup |
|---|---:|
| five commits, 2026-09-09 to 2026-09-13 | 125 ns each |
| 1 cache entry to 20,000 | 125 ns to 166 ns |

**Table 10: cache-lookup cost across five commits and across cache sizes.**
Take from it that the lookup is flat across every commit and stays within 125 ns to 166 ns from 1 entry to 20,000, so a rising `hit_p50_ns` tracks load.

The clocksource costs 20.9 ns per call, steal time read 0.02% at load 0.16, and the accounting change did not move the number.

### 4.2. What the timer spans

On the HTTP/1.1 and HTTP/2 path the timer (`start` in `m6-http/src/main.rs`, read as `start.elapsed()`) starts before the route's auth check and the lookup key, and stops once the lookup's follow-up work is done, before preconditions are evaluated and the response is written.
It covers `requires_auth`, `make_lookup_key`, `RequestDirectives::parse`, `Cache::lookup_with`, queueing a refresh for a stale entry and finding the peer's interface, all in memory.

### 4.3. What the metrics line reports

The metrics line (`snapshot()` in `m6-http/src/stats.rs`) reports the percentiles of one reservoir of the newest samples, up to `RESERVOIR`, which is 4096.
The reservoir is a count of samples.
On a quiet node it reaches back hours and blends idle and busy traffic, which is why `hit_samples` is reported beside it, on the metrics line and in the monitor digest.
`m6-monitor` reads zero samples as "not measured" and publishes `null`.

The reservoir is a ring that is never cleared, and `percentiles_ring` reads its newest *n* entries.
The `*_window_added` counters give the periodic log its own ten-second window.
`stats::perf_reservoir_tests` holds six tests, including `an_emit_does_not_erase_what_the_document_reports`, which fails if an emit clears the ring.

End to end, the 05-cms example stack took 40 requests, then read the metrics line three times across two emit boundaries with silence between them, and Table 11 gives each scrape:

| scrape | hit_samples | hit_p50_ns | hit_p99_ns |
|---|---:|---:|---:|
| straight after traffic | 81 | 2,250 | 5,625 |
| after 14 s of silence | 81 | 2,250 | 5,625 |
| after 14 s more | 81 | 2,250 | 5,625 |

**Table 11: the metrics line holding its samples across two emit boundaries.**
Take from it that the reported figure is the same whenever the scrape lands, and that the periodic log reports its own window, reading `cache_hits=39 hit_p50_ns=2208` in the busy window and zeros either side.

### 4.4. Reading the number correctly

`hit_p50_ns` is comparable only between windows with similar hit counts, so report the window's hit count beside the number.
To measure it, warm one cacheable page, issue a few thousand rapid requests over loopback, and read the `periodic stats` line whose window contains them.
The cache lookup costs 125 ns (Table 10).

---

## 5. Connection setup, per channel

Three single-purpose binaries measure connection setup from the client side: `m6-probe-h1`, `m6-probe-h2` and `m6-probe-h3`.
Each performs one handshake per connection, strictly sequentially, and sends no request except in the h3 `--0rtt` mode.
Their shared source is `m6-http/src/probe.rs`, and they check the server's own figure against an independent measurement.

*RTT* is round-trip time.
*QUIC* is the transport underneath HTTP/3.

### 5.1. The figures

Taken on the build host's loopback, 200 sequential handshakes per channel, against the server's own report of the same connections.
Table 12 compares the two figures per channel.

| channel | probe p50 | metrics p50 | metrics samples |
|---|---|---|---|
| http/1.1 | 0.381 ms | 0.425 ms | 200 of 200 |
| http/2 | 0.353 ms | 0.423 ms | 270 (200 probe plus 70 warmer) |
| http/3 | 1.132 ms | 1.075 ms | 200 of 200 |

**Table 12: client-measured against server-measured connection setup, loopback, 2026-09-15.**
Take from it that the two agree within 0.07 ms, and that the client figure sits above the server's for h1 and h3 because the client times from its own first send and the server from receiving that packet.

**The h1 and h2 figures exclude the TCP round trip**, because rustls is handed the socket after the three-way handshake completes.
**The h3 figure includes the equivalent**, because QUIC folds transport and crypto together and there is no earlier point to start from.
The two spans differ, so a handshake p50 is reported per channel.

### 5.2. Loopback prices only CPU

On loopback h3 reads 1.1 ms against h2's 0.42 ms, because loopback has no round trip.
Over a real path the ordering inverts.
Table 13 was measured from a client 5.1 ms away by ping.

| measurement | real path, 5.1 ms RTT |
|---|---|
| h1 rustls handshake, excluding TCP connect | 7.79 ms |
| h2 rustls handshake, excluding TCP connect | 6.14 ms |
| h3 cold QUIC handshake | 12.2 ms |
| h3 0-RTT, first packet to response headers | 6.0-6.7 ms |

**Table 13: connection setup over a 5.1 ms path, where h3 0-RTT wins.**
Take from it that h1 and h2 need a TCP round trip before any of this, so a returning visitor over h2 pays TCP, then TLS, then a request round trip, reaching about 16 ms to a response where h3 with 0-RTT answers in 6.4 ms.

A real-path number decides protocol choice.

### 5.3. Resumption

The probes offer a session ticket from their second handshake on, so a full handshake and a resumed one are measured separately and are comparable on the metrics line.
A resumed handshake saves the certificate and the signature.
TLS 1.3 completes in one round trip either way, so the saving is crypto and it is sub-millisecond on a fast path.
Table 14 gives the full and the resumed p50 from a distant client.

| handshake | p50 | n |
|---|---|---|
| h2 full | 7.153 ms | 1 |
| h2 resumed | 6.634 ms | 7 |

**Table 14: full against resumed handshake, measured by the probe from a distant client over a real path.**
Take from it that the saving is 0.52 ms, which is the cost of the certificate and the signature.

A browser opens one h2 connection per visit and multiplexes it, so h2 resumption needs a return visit inside the ticket's lifetime and a single-visit h2 channel reads 0%.
`tools/conformance.sh resume` proves the capability on h1, h2 and h3, and CI's conformance job runs it.

Section 6 splits the handshake's cost, and section 7 covers the size of the server's first flight over QUIC.

---

## 6. Where a TLS handshake goes

This section splits a TLS 1.3 handshake into crypto, kernel and m6's own code, prices the loopback method that produced the figure, compares the cipher suites and the two providers, follows the hit path under handshake load, and prices a system call.
Every figure was taken on 2026-11-16 on a 4-vCPU x86-64 cloud VM (Skylake) whose hypervisor hides ADX and SHA-NI, running Linux 7.0.0-43-generic at load 0.36 to 0.49, with an ECDSA P-256 certificate, TLS 1.3, a release build and the client on the same host.
The finding is that m6's own code is 9 us of a 372 us handshake, and the rest needs a CPU exposing ADX and SHA-NI before it can fall much further.

*ADX* is an x86 instruction set for integer arithmetic, and *SHA-NI* is the x86 extension that accelerates SHA-256 and SHA-1.
The handshake figures of Tables 15 and 16 name two crypto providers, *ring* and *aws-lc-rs*, and aws-lc-rs has the faster X25519 on this hardware.
`m6-http/Cargo.toml` builds aws-lc-rs only, so Table 15 and the ring rows of Table 16 describe a provider this tree no longer builds.
The handshake times come from the probe clients in `m6-http`, the split from `perf record` on the server, the system-call count from `strace`, and the bare call costs from a loop of the system call itself.
Section 8.1 gives each command, and every table names its method and constants.

### 6.1. The split

A full handshake with ring read 0.372 ms at p50 from the client over 3,000 connections, and the server's own CPU per handshake was 400.0 us.
A profile of 1,000 of those handshakes attributes the server's CPU by leaf frame.
The profiled run read 0.490 ms p50 and 580.0 us of server CPU per handshake, so the shares below are the profile's proportions applied to the 372 us.

| share | us | of 372 us | what it is |
|---|---:|---:|---|
| crypto, of which: | 158 | 42.5% | key exchange, signature, hash, record encryption |
| X25519 key exchange | 83 | 22.3% | the server's key share and shared secret |
| P-256 signature | 32 | 8.6% | the ECDSA signature over the handshake transcript |
| SHA-384 compression | 29 | 7.8% | the transcript hash |
| other crypto | 14 | 3.8% | record encryption, HKDF and ticket encryption |
| kernel | 144 | 38.7% | system calls, TCP and the socket layer |
| m6's own code | 9 | 2.4% | accept, the event loop and TLS glue |
| libc, rustls, allocator and other user code | 61 | 16.4% | wrappers, `memcpy`, heap and std |
| total | 372 | 100% | the client's p50 |

**Table 15: where a 372 us TLS 1.3 handshake with ring goes on the VM, by server CPU. The p50 is `m6-probe-h1 --n 3000`'s full row, the server's CPU per handshake is its process CPU time across that run, the shares are a `perf record -F 20000 -e cpu-clock -g --call-graph dwarf` of 1,000 handshakes partitioned by leaf frame, and the X25519 and P-256 rows are a 2,000-iteration in-memory bench of each primitive.**
Take from it that crypto and the kernel are 81% of the handshake and m6 is 2.4%, and that the single largest item is the 83 us X25519 key exchange.
The crypto sub-rows sum to 158, and the percentages are those microseconds over 372.
The profile's SHA-512 compression frames are 7.58% of its self time, 28 us, and the parts bench's finish phase, which includes that hash, reads 29.9 us, so the table row is 29.

### 6.2. The loopback price

The same handshake reads 0.372 ms in the first run and a 0.410 ms median in the repeat, 10% apart, on a host at load 0.36 to 0.49.
Both ends share the four vCPUs on loopback, so each end's work sits inside the other's wall time.
A loopback handshake is an upper bound on CPU cost.
A real path moves the client's CPU to another machine and adds the round trips that Table 13 prices.

### 6.3. The suites and the two providers

Seven rounds of each combination, 3,000 handshakes a round, compare the two cipher suites the client offers and the two providers.

| client suite | provider | full p50, median (range) | resumed p50, median |
|---|---|---:|---:|
| AES-256-GCM | ring | 0.410 ms (0.373 to 0.441) | 0.360 ms |
| AES-256-GCM | aws-lc-rs | 0.422 ms (0.337 to 0.447) | 0.403 ms |
| AES-128-GCM | ring | 0.387 ms (0.360 to 0.419) | 0.345 ms |
| AES-128-GCM | aws-lc-rs | 0.380 ms (0.360 to 0.441) | 0.337 ms |

**Table 16: whole-handshake p50 by client suite and provider, seven rounds each, 3,000 handshakes a round on the VM (`m6-probe-h1 --addr HOST:PORT --n 3000`, with the client's cipher-suite list fixed to one suite per round, against a server release build of each provider).**
Take from it that the ranges overlap in every row and the ordering flips between the two suites, so the whole handshake does not measurably change.

The in-memory parts bench reads X25519 at 83.4 to 83.7 us with ring and 46.7 to 62.9 us with aws-lc-rs, and the P-256 signature at 26.0 to 33.6 us across both (a 2,000-iteration in-memory bench of each primitive, p50).
A saving of 20 to 36 us on the key exchange is 5% to 10% of the 372 us handshake, and the run-to-run spread in Table 16 is 10%, so the whole-handshake difference is below what this method resolves.

### 6.4. Hits under handshake load

A cache hit does not pay the handshake, and it can wait behind one.
One kept-alive connection served hits while a second generator opened new full handshakes at 0, 50, 100, 200 and 500 a second, in 10 second windows.
Table 17 gives the hit and handshake latency at each rate.

| new handshakes/s | achieved/s | hits/s | hit p50 us | hit p99 us | handshake p50 ms |
|---:|---:|---:|---:|---:|---:|
| 0 | 0.0 | 15,659.9 | 57.4 | 126.9 | - |
| 50 | 50.0 | 15,471.0 | 58.0 | 138.1 | 0.826 |
| 100 | 100.0 | 14,491.1 | 60.5 | 175.5 | 0.867 |
| 200 | 200.0 | 13,403.0 | 58.3 | 327.8 | 0.777 |
| 500 | 500.0 | 13,658.8 | 58.2 | 300.6 | 0.645 |

**Table 17: cache hits while new full handshakes arrive, one run per rate, 10 second windows on the VM (`m6-bench`'s cache-hit client on one kept-alive connection while `m6-probe-h1` connections open at each rate).**
Take from it that the hit p50 stays flat at 57.4 to 60.5 us, the hit p99 rises to 327.8 us at 200 handshakes a second and holds at 300.6 us at 500, and hit throughput falls 15% from 15.7k to 13.4k a second.

The p99 plateau is 300 us, which matches the 158 us of crypto plus 144 us of kernel from Table 15, 302 us, so the hit tail is one handshake deep.
The handshake p50 in this run is 0.65 to 0.87 ms, above the 0.372 ms of section 6.1, because the same four vCPUs now run the hit client, the handshake generator and the server together.

### 6.5. The system call

Table 18 gives the cost of two bare system calls on the VM, the unit in which the kernel share of Table 15 is spent.

| operation | per call |
|---|---:|
| `getppid`, 5,000,000 calls | 1.20 us |
| pipe write plus read, 2,000,000 calls | 1.54 us |

**Table 18: the two bare system call costs on the VM (a `getppid` loop of 5,000,000 calls and a pipe write and read loop of 1,000,000 pairs, section 8.1).**
Take from it that most of the kernel share in Table 15 is the TCP work behind the calls.

A single per-call figure for the handshake's syscall mix is `NOT MEASURED`.
The handshake makes 26,184 calls per 1,000 handshakes, of `writev`, `recvfrom`, `close`, `accept4`, `epoll_wait`, `epoll_ctl`, `setsockopt` and `ioctl` (`strace -c -f` attached to the server over the same 1,000 handshakes), and `strace`'s average per call carries its own overhead.
Pricing it needs a bench on the same VM, with the profiler off, that times the mix the `strace` shows.
ring made nine `getrandom` calls per handshake in that record and aws-lc-rs made two calls in 1,000 handshakes, and their cost is `NOT MEASURED` for the same reason.

### 6.6. Conclusion

Of 372 us, 9 us is m6, 144 us is the kernel and 158 us is crypto, and the largest single item is the 83 us X25519 key exchange.
The rest of the crypto needs a CPU that exposes ADX and SHA-NI, which this hypervisor hides.
Crypto and kernel together are 302 us, which sets the floor on this hardware.
The provider comparison on the development Mac is `NOT MEASURED`, because the shared host's run-to-run spread was larger than the difference.

---

## 7. Certificate compression and the QUIC amplification limit

*QUIC* is the transport underneath HTTP/3.
A QUIC server may send only `factor x bytes received` before it has validated the client's address, under RFC 9000 section 8.1, so that it cannot be used as a reflection amplifier.
m6's uncompressed handshake flight exceeded that budget, stalled, and waited a full round trip.
Certificate compression removed the excess and the server runs at quiche's conforming default factor of 3.

### 7.1. The stall

A client's opening Initial is padded to 1200 bytes, so at a factor of 3 the budget is 3600 bytes.
The uncompressed handshake flight was 4082 bytes, of which the certificate chain is 3429.
`m6-probe-h3` prints a per-packet timeline with the live ratio, against a 4.85 ms RTT.
Figure 1 shows that timeline for one uncompressed handshake:

```
+0.741ms  client 1200B
+7.018ms  server 1200B   1.00x
+7.223ms  server 2400B   2.00x
+7.240ms  server 3600B   3.00x   <- stops, 482 bytes still owed
+12.221ms server 4082B           <- one full round trip later
```

**Figure 1: the server exhausting its amplification budget mid-flight.**
Take from it that the final datagram arrives one full round trip after the previous one, and that the ratio at the moment the server stopped was 3.00x.
The ratio over the whole handshake reads 1.62x, because the client's ACKs have arrived by the end, so only the per-packet timeline shows the stall.
Disabling pacing, which quiche enables by default, left the stall unchanged.

### 7.2. The ordinary case

A published [study](https://www.fastly.com/blog/quic-handshake-tls-compression-certificates-extension-study) measured 40-44% of uncompressed chains exceeding the budget, and certificate compression taking that to 1-9%.
[Other work](https://blog.apnic.net/2023/01/16/on-the-interplay-between-tls-certificates-and-quic-performance/) puts it at 61% for a Firefox-sized 1352-byte Initial.
Table 19 gives the chain sizes measured for m6 and six public sites, for scale.

| chain | certs | chain bytes |
|---|---:|---:|
| m6's 4-certificate ECDSA chain from a public CA | 4 | 3429 |
| public site A | 4 | 3393 |
| public site B | 4 | 3576 |
| public site C | 3 | 2552 |
| public site D | 3 | 2718 |
| public site E | 3 | 3755 |
| public site F | 3 | 4051 |

**Table 19: measured certificate chain sizes, for scale.**
Take from it that a 3429-byte chain is typical and that two of the seven are larger, so chain size alone does not decide which servers stall.
The factor each server runs at decides it, and Table 20 gives the factor three servers run at:

| edge | bytes sent before address validation | implied factor |
|---|---:|---|
| m6 at quiche's default | 3600 then stops | 3.00x |
| public edge A | 4198 in one flight | 3.50x |
| public edge B, serving site F | 5360 in one flight | 4.47x |

**Table 20: what other edges appear to run at, measured with `m6-probe-h3` on a 1200-byte client Initial.**
Take this as our own measurement: the byte accounting counts QUIC payload, and no public source corroborates servers exceeding the limit.

### 7.3. The fix that shipped

m6-http sets no amplification factor override, so quiche's conforming default of 3 applies.
Certificate compression, RFC 8879, keeps the flight under that budget.
The compression runs in m6's own quiche fork, pinned by revision in `m6-http/Cargo.toml`, and is enabled by `cfg.enable_cert_compression()` in `m6-http/src/main.rs`.
Table 21 gives the chain size under each compression algorithm.

| algorithm | chain bytes | of uncompressed |
|---|---:|---:|
| uncompressed | 3429 | 100% |
| brotli | 2258 | 66% |
| zstd | 2308 | 67% |
| zlib | 2359 | 69% |

**Table 21: the certificate chain under each compression algorithm.**
Take from it that any of the three fits the budget, and that brotli is the one m6 offers.

**m6 offers brotli and accepts zlib without offering it.**
`h3spec` rebuilds the handshake transcript by re-compressing, and a peer that does that cannot verify our CertificateVerify.
zlib also compresses this chain less well than brotli.
BoringSSL negotiates from the intersection with the client's `compress_certificate` extension, so a client that advertises nothing m6 compresses with receives the chain uncompressed and handshakes normally.

**Measured end to end.**
The whole server flight went from 4081 bytes to 2859 with brotli, a saving of 1222 bytes, which leaves 741 bytes of headroom under the 3600 budget for a chain that grows.
The handshake completes in one round trip: four datagrams with a 5.07 ms stall became three datagrams with none.
Public edge A's flight went from 4198 to 3388 bytes for the same probe, which shows our client advertises compression, and public edge B did not change.

### 7.4. 0-RTT

0-RTT is enabled, and h3spec reads 47/49 with early data on, which is the recorded floor in `tools/conformance-scores.txt`.
The test "MUST send PROTOCOL_VIOLATION if CRYPTO in 0-RTT is received" passes because the close is keyed on whether the server has ever sent a Handshake packet, which is the fact that determines whether the peer can read it.

**0-RTT data is replayable, so `handle_h3_request` is stricter than RFC 8470's advice on idempotent methods.**
In early data it serves only a fresh cache hit and answers 425 Too Early to everything else.
A replayed cache read re-sends bytes and does nothing more.
A site may serve a fire-and-forget GET beacon whose replay inflates a counter, and a stale hit queues a background refresh, which is a write.

With 0-RTT a returning visitor reaches response headers in 6.0 to 6.7 ms (Table 13).

---

## 8. Reproducing these measurements

The measurements that can be re-run are described below, with the command where the tree carries one.
A re-run measures the tree it runs on, so the figures here stay the record of the builds they name.
The commands are grouped by the section they support.

### 8.1. Sections 2 to 7

**Per-request copy cost, sections 2.2 and 2.5:**

```sh
M6_REAL_CONTENT_JSON=/path/to/data/content.json \
  cargo test --release -p m6-core copy_audit -- --ignored --nocapture
M6_REAL_CONTENT_JSON=/path/to/data/content.json \
  cargo test --release -p m6-core dict_cost  -- --ignored --nocapture
```

Both are `#[ignore]`d and print `SKIP` and measure nothing when `M6_REAL_CONTENT_JSON` is unset, so the file is the measurement's input.

**Page render A/B, section 2.1.**
Build the renderer at the two commits into separate target directories, then alternate: start one on a unix socket against a real site directory and its renderer config, warm it, time 300 requests, kill it, repeat with the other, five rounds.
Interleaving keeps the afternoon's drift on both sides.

**Cache-lookup A/B across commits, section 4.1.**
Use `git worktree add --detach` per commit, and write a bench against the API common to all of them, which is `Cache::new`, `CacheKey::new`, `insert`, `get` and `make_lookup_key` in `m6-http/src/cache.rs`.
Build each with its own `CARGO_TARGET_DIR`, then run interleaved rounds.

**Cache-hit p50 under load, section 4.4.**
Warm one cacheable page, issue a few thousand rapid requests over loopback, and read the `periodic stats` line whose window contains them.
Report the hit count with the percentile.

**Connection setup and 0-RTT, sections 5 and 7:**

```sh
cargo build --release --bin m6-probe-h1 --bin m6-probe-h2 --bin m6-probe-h3
./target/release/m6-probe-h1 --addr HOST:443 --n 200
./target/release/m6-probe-h2 --addr HOST:443 --n 200
./target/release/m6-probe-h3 --addr HOST:443 --n 200
./target/release/m6-probe-h3 --addr HOST:443 --0rtt /
./target/release/m6-probe-h3 --addr HOST:443 --0rtt /some-path --method POST
```

The probes take any host, which is how Tables 19 and 20 were measured against third-party edges.

**The TLS handshake figures, section 6.**
Start a release m6-http on a loopback port with an ECDSA P-256 certificate, then drive it with `m6-probe-h1`:

```sh
cargo build --release -p m6-http --bins
./target/release/m6-probe-h1 --addr 127.0.0.1:8443 --n 3000
```

The probe reports full and resumed handshakes separately with their percentiles, and a shared client config measures resumption while `--no-resume` measures full handshakes alone.
The split in Table 15 comes from a `perf record -F 20000 -e cpu-clock -g --call-graph dwarf -p <server pid>` of 1,000 handshakes, read with `perf report --stdio --no-children -g none --sort dso,sym` and partitioned by leaf frame.
The system-call count is `strace -c -f -p <server pid>` over the same run.
The bare call costs are a C loop of 200,000 warm-up `getppid` calls, then 5,000,000 timed with `clock_gettime(CLOCK_MONOTONIC)`, and 1,000,000 pipe write and read pairs.
The parts bench is an in-memory loop of 2,000 full handshakes per primitive, reading the key exchange and the signature alone and reporting p50.
The hit-under-load run holds `m6-bench`'s cache hit on one kept-alive connection while `m6-probe-h1` opens full handshakes at 0, 50, 100, 200 and 500 a second, one 10 second window per rate.
This tree builds aws-lc-rs only, so the ring rows of Table 16 need a build that selects ring.

### 8.2. The mimalloc A/B on the build host

Run on the idle build host, holding the host lock, the base (`ce6c29c`) against the branch (`perf/mimalloc`).
Build both with the release profile.

```sh
git worktree add ~/build/base ce6c29c
git worktree add ~/build/cand perf/mimalloc
for t in base cand; do
  (cd ~/build/$t && cargo build --release -p m6-http -p m6-html -p m6-file --bins)
done
```

**The pass mark, the kernel memory calls.** Attach `strace` to the running service after a warm-up:

```sh
cd ~/build/cand
MEMCALLS_SUDO=sudo tools/memcalls.sh base=$HOME/build/base/target/release \
  cand=$HOME/build/cand/target/release --requests 10000 --warmup 2000
```

It prints, for m6-http over hot cache hits and for m6-html over renders, one row per call (`brk`, `mmap`, `munmap`, `madvise`, `mremap`, `mprotect`), the total, the minor page faults and the RSS after the window.
The candidate's total should be 0, or a handful with every call named in the table.

**Page faults, TLB and cache misses per request, with the cache full:**

```sh
tools/hwcounters.sh base=$HOME/build/base/target/release cand=$HOME/build/cand/target/release \
  --requests 10000 --warmup 2000 --fill-mib 128
```

The script says "hardware counters not available on this host" and reports the software events alone when the VM exposes no counters.
`dTLB-load-misses` per request is the number huge pages should bring close to zero.
The script also prints `AnonHugePages` and `Hugetlb` from `/proc/<pid>/smaps_rollup` for m6-http, so huge pages are measured.

**Render latency**, interleaved and pinned, from the candidate checkout:

```sh
tools/perfcheck.sh --base-bin $HOME/build/base/target/release/m6-html \
  --cand-bin $HOME/build/cand/target/release/m6-html
```

**Cache-hit latency end to end**: `./bench.sh --latency-n 20000` in each checkout (it makes its own certificate and builds the release binaries), alternating base and candidate for five rounds, and compare the HTTP/1.1 and HTTP/2 cache-hit rows.
`tools/paired_bench.sh cache_hit` runs in-process with no allocator of its own, so it cannot measure the allocator.

**RSS after warm-up** is the `rss` row of both scripts above.
Record the binary sizes (`ls -l target/release/m6-http`) and the release build time beside it.

**The resident-set bound on Linux** is `cargo nextest run -p m6-http --test heap_resident`.
The test returns on macOS before its assertions, so the build host is where it runs, and that is the run Table 7 comes from.

---

## 9. Event-loop accounting and queue costs

Three costs inside the process, none of them a page cost, are recorded here and in `CHANGELOG.md`.
Each was measured on a release build on an Apple M4.
Table 22 gives each cost before and after the change.

| what | before | after | what changed |
|---|---|---|---|
| the traffic line, per minute of event loop | 78.6 ms | **13.4 ms** | a full window of 32,768 events across 2,000 clients. Each marker list is one case-insensitive aho-corasick automaton, and the summary reads the window's slots in place instead of building a record per event |
| the metrics turn, per ten seconds | 438 us | **80 us** | no host read in the turn. `HostReader` reads one host source per one-second tick, 1.5 us or less each on macOS except the interface table at 466 us, and the document is serialised straight into the log buffer |
| `Bounded` pop, 50,000 entries | 79.4 ms | **0.18 ms** | pop moved every remaining entry down one place. It is now a ring index (`VecDeque`), so pop is constant time |

**Table 22: costs of the event loop's accounting lines and of the `Bounded` queue, before and after.**
Take from it that the traffic line fell from 0.13% of the loop (78.6 ms of 60 s) to 0.022% (13.4 ms), 5.9 times, and that a `Bounded` queue of 50,000 entries drains 441 times faster.
The traffic line is the one cost here that scales with traffic.

---

## 10. What is not measured

Five gaps remain, listed in Table 23.

| gap | what is missing |
|---|---|
| throughput and concurrency | `m6-file` and `m6-auth-server` have no throughput or concurrency figure |
| conformance-side timing | h2spec and h3spec are correctness gates. h3spec deliberately opens stalled and malformed connections, so it cannot generate load: it once reported an h3 handshake p50 of 113 ms on loopback, where the probe reads 1.1 ms (Table 12) |
| the render figures after the release profile | Table 3 predates the `[profile.release]` change of 2026-09-27, which cut the gated render targets by 13.7% and 18.5%, and the gate now compares the candidate against a base build |
| the ACK share of the QUIC gap | the pinned quiche has no `draft-ietf-quic-ack-frequency` support. `grep -rn "ACK_FREQUENCY\|ack_frequency" quiche/src` finds nothing, the `Frame` enum carries RFC 9000's `ACK` only (`quiche/src/frame.rs:58-72`), and `TransportParams` carries the two RFC 9000 ack parameters and no other (`quiche/src/transport_params.rs:176-179`). `Config` exposes `set_ack_delay_exponent` (`quiche/src/lib.rs:1084`) and `set_max_ack_delay` (`quiche/src/lib.rs:1089-1093`), and both write the parameters this endpoint advertises to the peer. This endpoint's own ack is decided at `quiche/src/lib.rs:4620-4621` by the peer's packets, and its ack delay comes from the peer's `max_ack_delay` at `quiche/src/lib.rs:8058`. rc.1 therefore adds no ack-frequency control and claims no figure for that share |
| other machines | Table 3 is one laptop, and no other machine has a copy-removal figure |

**Table 23: what this document does not cover.**
Take from it that throughput has no number at all, and that `tools/perfcheck.sh` supersedes Table 3 for any gating purpose.

The measured position is that core's per-request copying fell by a factor of 280, the static file path allocates nothing for bytes it does not send, the cache lookup costs 125 ns and is flat, and the QUIC handshake completes in one round trip at a conforming amplification factor.
The template engine's 143 µs context build is measured and accepted, and throughput has no measurement.
The ACK share of the QUIC gap has no figure: the pinned quiche offers no ack-frequency control, so rc.1 adds none.
