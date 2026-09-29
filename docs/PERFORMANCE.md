# Performance: every number, and how it was taken

This document records what m6 costs, which change made it cost that, and the conditions each figure was taken under.
It covers per-component costs inside the server.
End-to-end figures live in [`BENCHMARKS.md`](BENCHMARKS.md), and the numbers a merge is gated on live in `tools/perf-baseline.txt`.

A figure without its conditions is unusable, so every number below carries the machine it came from and the method that produced it.
Section 1 states the rules the rest of the document follows.

## Contents

1. [What this document records](#1-what-this-document-records)
2. [Per-request cost in m6-core](#2-per-request-cost-in-m6-core)
3. [Allocation on the static file path](#3-allocation-on-the-static-file-path)
4. [Cache-hit latency, and why it is load dependent](#4-cache-hit-latency-and-why-it-is-load-dependent)
5. [Connection setup, per channel](#5-connection-setup-per-channel)
6. [Certificate compression and the QUIC amplification limit](#6-certificate-compression-and-the-quic-amplification-limit)
7. [Reproducing these measurements](#7-reproducing-these-measurements)
8. [What is not measured](#8-what-is-not-measured)

---

## 1. What this document records

m6 keeps performance numbers in three places and each answers a different question.
Sending a reader to the wrong one produces a figure that is accurate and irrelevant, so the split is stated first.

| file | what it answers | authority |
|---|---|---|
| [`BENCHMARKS.md`](BENCHMARKS.md) | what a client observes end to end | authoritative for any end-to-end figure |
| `tools/perf-baseline.txt` | what the merge gate requires, in nanoseconds | authoritative for the gated render targets |
| this document | what an individual component costs, and which change changed it | authoritative for per-component cost |

**Table 1: where each kind of performance number lives.**
Take from it that an end-to-end claim belongs in `BENCHMARKS.md` and a gated number in `tools/perf-baseline.txt`, so quoting this document for either is a mistake.

Four rules govern every figure below.
They exist because a performance document that breaks them misleads more effectively than no document at all.

| rule | what it requires |
|---|---|
| conditions with every number | the machine and the method, always. CPU, core count, memory, load and the config that produced the figure all belong here. A laptop figure and a build-host figure never share a column |
| an unmeasured change says so | "structural" means the reasoning is sound and nobody has put a number on it |
| comparisons are paired and interleaved | run A, B, A, B against a fixed baseline, so drift hits both sides equally |
| no figure identifies where it was taken | a number from a running instance appears as an anonymous example: its conditions in full, and no host, domain or node name. A number that has to be reproducible comes from a loopback instance, `m6-examples`, or the build host |

**Table 2: the four rules every number in this document follows.**
Take from it that rules one and four pull in the same direction rather than against each other: hardware and config are the conditions a figure needs, and the identity of the box is not one of them.

**Commit hashes recorded before 2026-09-26 do not resolve.**
The history rewrite of that date changed every SHA in this repository, so hashes written into older records point at nothing.
The hashes in this document were recovered by matching commit subjects and diffs, and Table 5 records the mapping.

---

## 2. Per-request cost in m6-core

`App` built every request's dictionary by copying the whole of a service's static configuration into an empty map.
Removing that copying took a rendered page from 2.79 ms to 1.64 ms and cut core's own copying by a factor of 280.
This section gives the end figure, the per-copy audit behind it, the commits, and the one cost left in place.

### 2.1. The rendered page

Two builds were run against the same site directory, the same renderer config and the same content file, over a unix socket, five interleaved rounds of 300 requests each.
The figure is the median of each round's p50.

| build | p50 per rendered page |
|---|---:|
| `4df91be`, the earlier build | 2.79 ms |
| the branch tip, 2026-09-13 | 1.64 ms |

**Table 3: cost of one rendered page, before and after the copy removal, a 41% reduction.**
Taken on a laptop, release build, with a fresh connection per request, whose cost is identical on both sides and cancels in the comparison.

**The rendered bytes are identical.**
Both builds answer `Content-Length: 16660` with ETag `"c97d83c5076770c"`, which is a content hash, so the pages match byte for byte.
The only difference on the wire is that the newer build emits `Connection: close` when the client asked for it, which is 19 bytes and is the correct behaviour.

### 2.2. Where the 1.15 ms went

The dictionary was assembled from the config file's keys plus the site's content file.
It was deep-copied six times per request, to produce something identical for every request until the next config reload.
`m6-core`'s `copy_audit` measures each copy separately, in release, against a real content file of 68 KB and 1,364 nodes.

| copy | removed by | before | after |
|---|---|---:|---:|
| config and `site_dir` out of the read lock | `18d2efe` (J1) | 458 ns | 42 ns |
| the matched route | `bf17313` (J6) | 167 ns | 42 ns |
| `build_dict`, three copies inside | `18d2efe` (J2, J3, J4) | 222,708 ns | 1,125 ns |
| `raw.clone()` into `Request` | `bf17313` (J6) | 458 ns | 0 |
| `dict.clone()` into `Request` | `18d2efe` (J3) | 108,583 ns | 375 ns |
| `dict.clone()` in `render_response` | `18d2efe` (J5) | 109,500 ns | 0 |
| `tera::Context`, inside the engine | left in place, see 2.5 | 189,000 ns | 143,417 ns |
| **total per request** | | **630,874 ns** | **145,001 ns** |

**Table 4: the six per-request copies, each measured separately.**
Take from it that core's own copying fell from about 442 µs to about 1.58 µs, a factor of 280, and that the remaining 143 µs is inside the template engine rather than in core.

### 2.3. The commits

Five commits carried this work.
Their hashes changed in the 2026-09-26 history rewrite, so both the current hash and the hash used in older records are given.

| current | in older records | what it did |
|---|---|---|
| `fd77816` | `1b05a18` | Found it. The first figure was 3.08 µs from a synthetic 20-key config, which the owner judged too slow for building a small map. The parser and the map are 41 to 583 ns and the copy is the rest, seventy times larger against a real config |
| `d7020aa` | `479f1df` | The audit across the whole system. `m6-http` and `m6-file` were already correct, using `Arc<Vec<_>>` headers and `bytes::Bytes` bodies at the edge. `App` was the only offender |
| `18d2efe` | `13280a5` | J1 to J5. `Arc<RendererConfig>`, a per-route base dictionary built once per reload, and `m6-core/src/dict.rs`, a layered `Dict` of shared base plus per-request overlay |
| `bf17313` | `5eb9af3` | J6. The last two copies were borrows written as copies. The route table is shared, so routing returns a borrow, and `serve_connection` hands the request to its handler |
| `4df91be` | `22ee3a4` | The accounting change, and the earlier of the two builds in Table 3 |

**Table 5: the copy-removal commits, with the hash each carried before the history rewrite.**
Take from it that any hash in Table 4 or an older document resolves only through this mapping.

### 2.4. The merge order the change had to preserve

The dictionary is assembled from a fixed order of sources, and the order decides which value wins.
Built-in keys are merged after params files so that a params file cannot override `year`, `datetime` or `request_path`.

The layering preserves that order by construction, because built-ins live in the overlay and params files in the base.
Steps 1 to 3 of the original order produced the same map for every request on a route, so they moved into `route.base_dict` and are shared behind an `Arc`.
What `build_dict` still merges is what varies per request: per-request params files, path params, query params, form fields, cookies, built-ins and auth keys.
The test was written before the change and is `dict::tests::a_base_entry_can_never_override_an_overlay_one`.

### 2.5. The template engine floor

`tera::Context::insert` calls `to_value`, which deep-copies every entry into the engine's own `BTreeMap`, and that accounts for 143 µs of the remaining 145 µs.
Measured against a minimal template, the case most favourable to the cost being small, building the context is 99.6% of the work, at 178,875 ns to build against 750 ns to render.
The cost scales with the size of the context rather than with what the template reads, so a page touching three keys still pays to copy all 1,364 nodes.

**Accepted on the owner's call, 2026-09-13.**
It is recorded because the figure is larger than it looks.
The cheaper lever sits on the site side: a content file named as both `global_params` and the route's `params` enters every page's context twice over.

Removing core's copying left one measured cost inside the template engine and nothing measurable in core itself.
The file path had a separate problem, which was allocation rather than latency.

---

## 3. Allocation on the static file path

Two changes stopped `m6-file` reading bytes it never sent.
Neither carries a latency figure, and both are recorded as structural for that reason.

| commit | in older records | change | measurement |
|---|---|---|---|
| `e1ed6dd`, `1005519` | `a094851`, `7932e43` | A HEAD is answered from `metadata.len()` without opening the file, when the representation is the file. `1005519` is the directory case the first version got wrong | Structural. A HEAD on a large image previously paid a whole `fs::read`, then minification, then brotli at level 6, to produce bytes nobody receives |
| `1d40724`, `dc9be74` | `8c79ee7`, `0759ad4` | Streaming response bodies. `Response.body` is a sum type of `Bytes` and `Stream { len, reader }`, so a file reaches the wire without being materialised | Structural. A 3.6 MB asset was read into a `Vec` on every cache miss to be copied straight out again |

**Table 6: the two allocation changes on the static file path.**
Take from it that both are reasoned rather than measured, and that the second removed a 3.6 MB allocation per cache miss on the largest asset.

`dc9be74` made the sum type deliberate.
In `m6-core/src/response.rs`, `as_bytes()` returns `None` for a stream, so the minifier, the compressor and the default content-hash ETag structurally cannot run on a body core has not read.

Allocation on the file path is now bounded by what the response sends.
The cache path raised a different question: whether its latency had regressed at all.

---

## 4. Cache-hit latency, and why it is load dependent

`hit_p50_ns` is a load-dependent measurement.
On a near-idle single-core VM the cache-hit path goes cold between requests, so the number tracks request density rather than code.
This section records the measurement that looked like a regression and was not, the defect that stopped the figure being published at all, and the rule for reading it.

**Three source comments cite this section by number**, in `m6-http/src/stats.rs` and `m6-monitor/src/digest.rs`.
Renumbering it breaks them.

### 4.1. The measurement that looked like a regression

The longest-running open performance item was resolved on 2026-09-13 in `72e155d`, and the answer is that there was never a code regression.
One node, one binary, one counter, sampled minutes apart under different request densities:

| window | cache hits in it | hit p50 | hit p99 |
|---|---:|---:|---:|
| routine traffic | 50-70 | 3,900-4,000 ns | 4,200-4,400 ns |
| a tight burst | 1,200 | 1,064 ns | 3,782 ns |

**Table 7: the same code reading four times faster under load, on a single-core VM with a `kvm-clock` clocksource.**
Take from it that 1,064 ns sits below the 1.7-2.2 µs band recorded on 2026-09-06 and treated as the baseline afterwards, so the band described the traffic and not the code.
These are example figures from one instance under real traffic, given with their conditions and without a host, per rule four of Table 2.
They illustrate the effect rather than establishing a number for any particular deployment.

A paired, interleaved A/B across five commits spanning 2026-09-09 to 2026-09-13 confirms it.
Three of the five are recoverable after the history rewrite and two are not.

| position | commit | cache lookup |
|---|---|---:|
| 1 | not recovered | 125 ns |
| 2 | `3c3ea14`, 2026-09-09 | 125 ns |
| 3 | not recovered | 125 ns |
| 4 | `4df91be`, 2026-09-10 | 125 ns |
| 5 | the branch tip, 2026-09-13 | 125 ns |

**Table 8: cache-lookup cost at five points across the window in which the regression was suspected.**
Take from it that the figure is flat across every commit, and that it stays between 125 ns and 166 ns from 1 cache entry to 20,000, which was the other candidate explanation.

Each remaining candidate was measured rather than argued.
The clocksource is `kvm-clock` at 20.9 ns per call, steal time read 0.02% at load 0.16, and neither the accounting change in `4df91be` nor cache growth moved the number.

**Why nobody found it earlier.**
`m6-http`'s bench stopped compiling at `3c3ea14`, when `stats.record` gained parameters and the bench was not updated, and it stayed broken until the clippy gate forced `--all-targets` on 2026-09-12.
Benches are hidden from `cargo test` the same way the csrf tests were hidden behind a feature flag.
The A/B used a portable bench written against the API common to all five commits.

### 4.2. What the timer spans

The timer starts immediately before the cache lookup and stops immediately after it, before the response is written.
It therefore covers two in-memory operations, `make_lookup_key` and `Cache::lookup_with`.
The `ctx.start` timers elsewhere in `m6-http/src/main.rs` belong to the miss path and do not feed this number.

### 4.3. The aggregate that reported nothing

Found and fixed on 2026-09-15 in `4acf0e0`.
While checking nodes against the load-dependence finding, the endpoint that serves the figure turned out to report nothing at all:

```json
"cache_hits_total": 338,   "cache_misses_total": 1035,
"hit_samples": 0, "hit_p50_ns": 0, "hit_p99_ns": 0,
"miss_samples": 0, "monitor_samples": 0
```

338 hits counted and zero latency samples, after ten hours of uptime.
`m6-monitor` correctly reads zero samples as "not measured" and publishes `null`, so the fleet digest carried no latency for any node and never had.
The one number the monitor exists to trend was structurally absent, while the `periodic stats` line printed `hit_p50_ns=3878` for the same counter in the same minute.

**Cause.**
`maybe_emit` reset `hit_idx` and `hit_count` to zero every ten seconds, and `snapshot()`, which serves `/perf`, read those same fields.
`/perf` therefore reported the percentiles of whatever fraction of a ten-second window happened to be open when it was scraped.
A node taking two requests a minute averages one request per thirty-second window, so most ten-second windows contain no cache hit at all.
The comment on `snapshot()` reasoned carefully about not resetting from the scrape path, so that a polling monitor could not gut the operational log.
That reasoning was correct and incomplete: it read the window the emitter did reset.

**Why it survived.**
The per-channel reservoirs were never cleared, so they held real data throughout.

| channel | requests | hit samples | hit p50 |
|---|---:|---:|---:|
| http/1.1/external | 839 | 35 | 3,191 ns |
| http/2/external | 249 | 131 | 2,604 ns |
| http/2/internal | 569 | 174 | 2,789 ns |

**Table 9: the per-channel figures from the same scrape that reported a zeroed aggregate.**
Take from it that only the aggregate was broken, and that it is the only figure `m6-monitor` reads, so anyone scrolling past it saw plausible numbers.

**Fix.**
One reservoir, run as a ring and never cleared.
`*_window_added` counters give the periodic log its own ten-second window, so operational logging is unchanged.
`percentiles_ring` reads the newest *n* entries, which had been correct only because the index was reset every window and would have reported the oldest samples as current once the ring wrapped.
The change costs no extra memory and no extra work per request.

`snapshot()` now spans the most recent samples up to `RESERVOIR`, which is 4096 in `m6-http/src/stats.rs`, **and that is a count rather than a period of time**.
On a quiet node it reaches back hours and blends idle and busy traffic.
That is why `hit_samples` is reported beside it, in the periodic log, in the monitor digest and in `/perf`.

**Verified.**
`stats::perf_reservoir_tests` holds six tests.
Three fail against the old reset, including `an_emit_does_not_erase_what_perf_reports`, and the other three are properties that held either way.
This was checked by reinstating the old reset and watching them fail, because a regression test that passes against the broken version is worth nothing.

End to end, the 05-cms example stack with a perf token, 40 requests, then scrapes across two emit boundaries with silence between them, which is the ordering that used to return zeros:

| scrape | hit_samples | hit_p50_ns | hit_p99_ns |
|---|---:|---:|---:|
| straight after traffic | 81 | 2,250 | 5,625 |
| after 14 s of silence | 81 | 2,250 | 5,625 |
| after 14 s more | 81 | 2,250 | 5,625 |

**Table 10: `/perf` holding its samples across two emit boundaries.**
Take from it that the reported figure no longer depends on when the scrape lands, and that the periodic log still means "this window", reading `cache_hits=39 hit_p50_ns=2208` in the busy window and zeros either side.

### 4.4. Reading the number correctly

`hit_p50_ns` is comparable only between windows with similar hit counts.
A baseline for it reads "1.7-2.2 µs at 40-70 hits per window, about 1.0 µs under sustained load", with the window's hit count reported beside the number.

The cache path costs 125 ns and the figure that appeared to contradict that was measuring traffic density.
Connection setup is the cost a client notices, and it is measured separately per protocol.

---

## 5. Connection setup, per channel

Three single-purpose binaries measure connection setup from the client side: `m6-probe-h1`, `m6-probe-h2` and `m6-probe-h3`.
Each performs one handshake per connection, strictly sequentially, with no requests.
Their source is `m6-http/src/probe.rs`, and they exist so that the server's figure about itself can be checked against an independent measurement.

*RTT* is round-trip time.
*QUIC* is the transport underneath HTTP/3.

### 5.1. The figures

Taken on the build host's loopback, 200 sequential handshakes per channel, against the server's own report of the same connections.

| channel | probe p50 | `/perf` p50 | `/perf` samples |
|---|---|---|---|
| http/1.1 | 0.381 ms | 0.425 ms | 200 of 200 |
| http/2 | 0.353 ms | 0.423 ms | 270 (200 probe plus 70 warmer) |
| http/3 | 1.132 ms | 1.075 ms | 200 of 200 |

**Table 11: client-measured against server-measured connection setup, loopback, 2026-09-15.**
Take from it that the two agree within 0.07 ms, and that the client figure sits above the server's for h1 and h3 because the client times from its own first send and the server from receiving that packet.

**The h1 and h2 figures exclude the TCP round trip**, because rustls is handed the socket after the three-way handshake completes.
**The h3 figure includes the equivalent**, because QUIC folds transport and crypto together and there is no earlier point to start from.
The two spans differ and must never be averaged.
A single handshake p50 across all three would track the protocol mix, which is the error section 4.3 fixed for the request-latency aggregate.

### 5.2. Loopback prices only CPU

On loopback h3 reads 1.1 ms against h2's 0.42 ms, because loopback has no round trip.
Over a real path the ordering inverts.
Measured from a client 5.1 ms away by ping:

| measurement | real path, 5.1 ms RTT |
|---|---|
| h1 rustls handshake, excluding TCP connect | 7.79 ms |
| h2 rustls handshake, excluding TCP connect | 6.14 ms |
| h3 cold QUIC handshake | 12.2 ms |
| h3 0-RTT, first packet to response headers | 6.0-6.7 ms |

**Table 12: connection setup over a 5.1 ms path, where h3 wins.**
Take from it that h1 and h2 need a TCP round trip before any of this, so a returning visitor over h2 pays TCP, then TLS, then a request round trip, reaching about 16 ms to a response where h3 with 0-RTT answers in 6.4 ms.

**Never conclude anything about protocol choice from a loopback number.**

### 5.3. Resumption

The probes gained the ability to offer a session ticket on 2026-09-19 in `378c535`.
Before that they shared one `ClientConfig` across N connections, so they measured one full handshake and N-1 attempted resumptions under a single heading.
Separating the two is what makes these figures comparable with `/perf`.

A resumed handshake saves the certificate and the signature.
TLS 1.3 completes in one round trip either way, so the saving is crypto and it is sub-millisecond on a fast path.

| handshake | p50 | n |
|---|---|---|
| h2 full | 7.153 ms | 1 |
| h2 resumed | 6.634 ms | 7 |

**Table 13: full against resumed handshake, measured by the probe from a distant client over a real path.**
Take from it that the saving is 0.52 ms, which is the cost of the certificate and the signature.

On the server's side of the same channel the gap is far larger, because its full figure includes slow and hostile clients that a probe is not.
`/perf` read h1 full p50 at 161.65 ms against resumed at 0.76 ms, over 34 and 422 samples.
Those are different client populations, which is why the two are never blended into one handshake p50.

**What 1.9.0 delivered.**
The server's own figure for `http/2/external`, across the change from 1.8.1 to 1.10.0:

| figure | 1.8.1 | 1.10.0 |
|---|---|---|
| full p50 | 13.95 ms | 3.40 ms |
| full p99 | 1339.00 ms | 34.04 ms |
| full mean | 119.08 ms | 6.38 ms |

**Table 14: full-handshake cost before and after 1.9.0, measured by the server.**
Take from it that the p99 fell by a factor of 39, which is the figure that moved most.

**Resumption reads 0% on `http/2/external` and that is correct.**
A browser opens one h2 connection per visit and multiplexes it, so resumption needs a return visit inside the ticket's lifetime.
An `http/1.1` channel reading 88-96% has a repeat client polling on a loop.
`tools/conformance.sh resume` proves the capability on h1, h2 and h3 on every push, so the question cannot stay open again.

### 5.4. Two measuring-tool defects, both of which produced numbers

Both are recorded because the tool reported success and a plausible figure, which is worse than a tool that fails.

1. **`m6-bench-detail` panicked before measuring anything.**
   `m6-http` builds rustls with `default-features = false`, so no process-level CryptoProvider is installed automatically, and the first `ClientConfig::builder()` panicked.
   Every other binary in the crate installs it and this one did not.
   With the client broken, handshake timing was taken with `h3spec`, a conformance tester that deliberately opens stalled connections.
   It reported an h3 handshake p50 of 113 ms on loopback, for an engine that answers requests in microseconds.
   That figure was believed long enough to be written down.
   The real figure is 1.1 ms.

2. **The first version of `m6-probe-h1` and `m6-probe-h2` never completed a handshake.**
   rustls reports `is_handshaking() == false` as soon as it has the traffic keys, one step before the client Finished is flushed.
   The loop exited on that condition and dropped the socket, so the server never received Finished, sat handshaking until EOF, and recorded nothing.
   The probe reported 200 successes at a plausible 0.34 ms while the server completed zero.
   The 70 h2 samples the server did report were the cache warmer's curl connections at startup, which came close to being read as a server bug.

A third defect sat in `m6-http` itself.
The handshake was stamped about forty lines below the point where `advance_tls` returns an error, so a client that completed its handshake and closed immediately had the measurement discarded.
h1 recorded 0 of 200 such handshakes, and h2 kept only the ones the server reached before the close, which are the slow ones, giving a p50 of 6.14 ms against a true 0.42 ms.
A biased partial sample is worse than none, because 6.14 ms looked plausible.

Connection setup is now measured from both ends and the two agree.
One part of it, the size of the server's first flight, was large enough to cost a whole round trip and has its own section.

---

## 6. Certificate compression and the QUIC amplification limit

*QUIC* is the transport underneath HTTP/3.
A QUIC server may send only `factor x bytes received` before it has validated the client's address, under RFC 9000 section 8.1, so that it cannot be used as a reflection amplifier.
m6's uncompressed handshake flight exceeded that budget, stalled, and waited a full round trip.
Certificate compression removed the excess and the server now runs at quiche's conforming default factor of 3.

### 6.1. The stall

A client's opening Initial is padded to 1200 bytes, so at a factor of 3 the budget is 3600 bytes.
The uncompressed handshake flight was 4082 bytes, of which the certificate chain is 3429.
`m6-probe-h3` prints a per-packet timeline with the live ratio, against a 4.85 ms RTT:

```
+0.741ms  client 1200B
+7.018ms  server 1200B   1.00x
+7.223ms  server 2400B   2.00x
+7.240ms  server 3600B   3.00x   <- stops, 482 bytes still owed
+12.221ms server 4082B           <- one full round trip later
```

**Figure 1: the server exhausting its amplification budget mid-flight.**
Take from it that the final datagram arrives one full round trip after the previous one, and that the ratio at the moment the server stopped was 3.00x.

### 6.2. This is the ordinary case

The first two explanations written for this assumed the opposite, so the general position is stated plainly.
[Fastly's study](https://www.fastly.com/blog/quic-handshake-tls-compression-certificates-extension-study) measured 40-44% of uncompressed chains exceeding the budget, and certificate compression taking that to 1-9%.
[Other work](https://blog.apnic.net/2023/01/16/on-the-interplay-between-tls-certificates-and-quic-performance/) puts it at 61% for a Firefox-sized 1352-byte Initial.
About half of the QUIC internet pays this round trip.

| site | certs | chain bytes |
|---|---:|---:|
| a 4-certificate Let's Encrypt ECDSA chain | 4 | 3429 |
| news.ycombinator.com | 4 | 3393 |
| letsencrypt.org | 4 | 3576 |
| cloudflare.com | 3 | 2552 |
| github.com | 3 | 2718 |
| www.google.com | 3 | 3755 |
| www.mozilla.org | 3 | 4051 |

**Table 15: measured certificate chain sizes, for scale.**
Take from it that a 3429-byte chain is unremarkable and that two of the seven are larger, so chain size alone does not explain which servers stall.

**Two diagnoses were stated as fact and withdrawn.**

1. "The chain is unusually long because it is on a new hierarchy."
   The chain is a standard Let's Encrypt ECDSA chain, byte-identical on the intermediates to the one news.ycombinator.com and letsencrypt.org serve, and Google's and Mozilla's are larger.
   Withdrawn after measuring seven sites instead of reasoning about one.

2. "The amplification limit is not involved, the ratio is only 1.62x."
   1.62x is the ratio at the end of the handshake, by which time the client has sent its ACKs.
   The ratio at the instant the server stopped was 3.00x.
   A totals-only view cannot see this, which is why the probe now prints the per-packet timeline in Figure 1.

   The replacement for diagnosis 1, that other chains are smaller and therefore fit, failed its own arithmetic as well, since Cloudflare sends 4198 bytes and 4198 does not fit in 3600.
   What differs between servers is the factor each one runs at.

A third hypothesis, pacing, was tested and killed rather than argued.
quiche enables pacing by default and `flush_conn` discards `send_info.at`, which made it a strong candidate.
Disabling pacing changed the stall not at all.

| edge | bytes sent before address validation | implied factor |
|---|---:|---|
| m6 at quiche's default | 3600 then stops | 3.00x |
| Cloudflare | 4198 in one flight | 3.50x |
| Fastly, serving www.mozilla.org | 5360 in one flight | 4.47x |

**Table 16: what other edges appear to run at, measured with `m6-probe-h3` on a 1200-byte client Initial.**
Take this as our own measurement rather than established fact: the byte accounting counts QUIC payload, and no public source corroborates servers exceeding the limit.

### 6.3. The fix that shipped

Releases 1.2.0 and 1.3.0 carried `set_max_amplification_factor(4)`, which raised the budget to 4800 and let the whole flight go out at once.
It was marked temporary from the day it shipped, because it deviates from RFC 9000 rather than satisfying it.
**That override is deleted and the factor is back at the conforming 3.**

Certificate compression, RFC 8879, is what replaced it, and it closed issue #28 on 2026-09-17.
The compression runs in m6's own quiche fork, pinned by revision in `m6-http/Cargo.toml`, and is enabled by `cfg.enable_cert_compression()` in `m6-http/src/main.rs`.

| algorithm | chain bytes | of uncompressed |
|---|---:|---:|
| uncompressed | 3429 | 100% |
| brotli | 2258 | 66% |
| zstd | 2308 | 67% |
| zlib | 2359 | 69% |

**Table 17: the certificate chain under each compression algorithm.**
Take from it that any of the three fits the budget, and that brotli is the one m6 offers.

**m6 offers brotli and accepts zlib without offering it.**
`h3spec` rebuilds the handshake transcript by re-compressing, and a peer that does that cannot verify our CertificateVerify.
zlib also compresses this chain less well than brotli.
BoringSSL negotiates from the intersection with the client's `compress_certificate` extension, so a client that advertises nothing m6 compresses with receives the chain uncompressed and handshakes normally.

**Measured end to end.**
The whole server flight went from 4081 bytes to 2859 with brotli, a saving of 1222 bytes, which leaves 741 bytes of headroom under the 3600 budget for a chain that grows.
The handshake completes in one round trip: four datagrams with a 5.07 ms stall became three datagrams with none.
Third parties verified that our client advertises compression instead of merely accepting it: Cloudflare's flight went from 4198 to 3388 bytes for the same probe, and a Fastly edge did not change at all.

### 6.4. 0-RTT

0-RTT was held back on 2026-09-15 and **enabled on 2026-09-16**, once m6's quiche fork carried the three changes it needed.
Enabling it costs no conformance: h3spec reads 47/49 with early data on, which is the recorded floor in `tools/conformance-scores.txt`.

Accepting early data had cost one test, "MUST send PROTOCOL_VIOLATION if CRYPTO in 0-RTT is received", taking 47/49 to 46/49.
Three attempts were needed and the first two were in the wrong place.
A guard on the CRYPTO frame's contents cannot fire, because h3spec sends its 0-RTT packet during a fresh handshake with no resumption, so there is no 0-RTT read key and the packet is never parsed.
Rejecting on packet type detected the violation and h3spec still failed, because quiche put the CONNECTION_CLOSE in a Handshake packet whose keys the client did not yet have.
The close is now keyed on whether the server has ever sent a Handshake packet, which is the fact that determines whether the peer can read it.

**0-RTT data is replayable, so `handle_h3_request` is stricter than RFC 8470's advice on idempotent methods.**
In early data it serves only a fresh cache hit and answers 425 Too Early to everything else.
A replayed cache read re-sends bytes and does nothing more.
"GET is safe" would not have been enough, because a site may serve a fire-and-forget GET beacon whose replay inflates a counter, and a stale hit would queue a background refresh, which is a write.
Both gates were verified: a cached path answers 200 in early data and an uncached one answers 425.

The handshake now completes in one round trip and a returning visitor pays none.
Every figure above can be re-taken, and the commands are next.

---

## 7. Reproducing these measurements

Every measurement in this document can be re-run.
The commands below are grouped by the section they support.

**Per-request copy cost, sections 2.2 and 2.5:**

```sh
cargo test --release -p m6-core copy_audit -- --ignored --nocapture
cargo test --release -p m6-core dict_cost  -- --ignored --nocapture
```

Both are `#[ignore]`d deliberately.
A timing assertion fires on a loaded build box against correct code, which is the trap `test_static_file_cache_hit` already fell into once.
These are measurements and the gate for them is `tools/perfcheck.sh` against `tools/perf-baseline.txt`.

**Page render A/B, section 2.1.**
Build the renderer at the two commits into separate target directories, then alternate: start one on a unix socket against a real site directory and its renderer config, warm it, time 300 requests, kill it, repeat with the other, five rounds.
Interleaving is the point, because running all of one and then all of the other measures the afternoon.

**Cache-hit A/B across commits, section 4.1.**
Use `git worktree add --detach` per commit, and write a bench against the API common to all of them, which is `Cache::new`, `CacheKey::new`, `insert`, `get` and `make_lookup_key` in `m6-http/src/cache.rs`.
Build each with its own `CARGO_TARGET_DIR`, then run interleaved rounds.
The bench used in 2026-09-13 was written for that run and was not kept.

**Cache-hit p50 under load, section 4.4.**
Warm one cacheable page, issue a few thousand rapid requests over loopback, and read the `periodic stats` line whose window contains them.
Report the hit count with the percentile.

**Connection setup and 0-RTT, sections 5 and 6:**

```sh
cargo build --release --bin m6-probe-h1 --bin m6-probe-h2 --bin m6-probe-h3
./target/release/m6-probe-h1 --addr HOST:443 --n 200
./target/release/m6-probe-h2 --addr HOST:443 --n 200
./target/release/m6-probe-h3 --addr HOST:443 --n 200
./target/release/m6-probe-h3 --addr HOST:443 --0rtt /
./target/release/m6-probe-h3 --addr HOST:443 --0rtt /some-path --method POST
```

The probes take any host, which is how Tables 15 and 16 were measured against third-party edges.

---

## 8. What is not measured

A performance document that implies more coverage than it has is how the next reader is misled, so the gaps are listed plainly.

| gap | what is missing |
|---|---|
| throughput and concurrency | `m6-file` moved from an unbounded channel and a fixed worker set to `App`'s bounded pool with 503 backpressure, and `m6-auth-server` from a thread per connection to the same pool. Both are better shapes under overload and neither has a number |
| conformance-side timing | h2spec and h3spec are correctness gates. h3spec must never be used as a load generator, because it deliberately opens stalled and malformed connections, which is what produced the 113 ms figure in section 5.4 |
| the render figures after the release profile | Table 3 predates the `[profile.release]` change of 2026-09-27, which cut the gated render targets by 13.7% and 18.5%. The current gated numbers are in `tools/perf-baseline.txt`, written by `tools/perfcheck.sh` on the build host |
| magnitude on small hosts | Table 3 is a laptop figure. A single-core VM differs in magnitude, though the direction is a property of the code |

**Table 18: what this document does not cover.**
Take from it that throughput has no number at all, and that Table 3 is superseded for any gating purpose by `tools/perf-baseline.txt`.

The measured position is that core's per-request copying fell by a factor of 280 and the static file path no longer allocates what it does not send.
The cache lookup costs 125 ns and is flat, and the QUIC handshake completes in one round trip at a conforming amplification factor.
What remains is the template engine's 143 µs context build, which is measured and accepted, and throughput, which has never been measured at all.
Both are recorded here so that the next measurement starts from what is known rather than from what is assumed.
