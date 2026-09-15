# The performance story, by commit

What changed, which commit changed it, what was measured, and how. Written
2026-09-13 on the owner's request.

**Rules this file follows, because a performance document that breaks them is
worse than none:**

- Every number says **how it was measured and on what**. A laptop figure and a
  production figure are not the same kind of thing and are never mixed in one
  column.
- A change with **no measurement says so**. "Structural" means the reasoning is
  sound and nobody has put a number on it.
- Comparisons are **paired and interleaved** against a fixed baseline, per
  lesson 7, so drift hits both sides equally rather than whichever ran last.
- Nothing here is deployed. Every figure is from the laptop or the build host
  unless the row says production.

---

## 1. The headline: a rendered page costs 41% less

**Measured 2026-09-13.** The deployed commit against HEAD, both serving the
**real site directory, the real production `m6-html.conf`, and the real
`data/content.json`**, over a unix socket, five interleaved rounds of 300
requests each, median of each round's p50:

| | p50 per rendered page |
|---|---:|
| `22ee3a4`, deployed | **2.79 ms** |
| `HEAD` | **1.64 ms** |

Laptop, release build, fresh connection per request (its cost is identical on
both sides, so it cancels in the comparison). Harness in `/tmp/m6-ab`; the
method is described in §5 so it can be rebuilt.

**The rendered bytes are identical.** Both answer `Content-Length: 16660` with
ETag `"c97d83c5076770c"`, which is a content hash, so the pages are the same
byte for byte. The only difference on the wire is that HEAD emits
`Connection: close` when the client asked for it and the old binary did not,
which is 19 bytes and is the correct behaviour. **No layout or copy change**,
which matters because those need individual approval.

This is the Phase 5 and 6 delta the plan has owed since the migration, and it
also carries everything in §2.

---

## 2. Where the 1.15 ms went: App stopped copying its own config

`App` built every request's dictionary by starting from an empty map and
copying the whole of the service's static configuration into it. On this site
that is the config file's keys plus `data/content.json`, **68 KB and 1,364
nodes**, and it was deep-copied **six times per request** to produce something
identical for every request until the next config reload.

**Measured with `m6-core`'s `copy_audit`**, release, against that same real
`content.json`:

| copy | commit that removed it | before | after |
|---|---|---:|---:|
| config + `site_dir` out of the read lock | `13280a5` (J1) | 458 ns | **42 ns** |
| the matched route | `5eb9af3` (J6) | 167 ns | **42 ns** |
| `build_dict`, three copies inside | `13280a5` (J2, J3, J4) | 222,708 ns | **1,125 ns** |
| `raw.clone()` into `Request` | `5eb9af3` (J6) | 458 ns | **0** |
| `dict.clone()` into `Request` | `13280a5` (J3) | 108,583 ns | **375 ns** |
| `dict.clone()` in `render_response` | `13280a5` (J5) | 109,500 ns | **0** |
| `tera::Context`, inside the engine | not removed, see below | 189,000 ns | 143,417 ns |
| **total per request** | | **630,874 ns** | **145,001 ns** |

**Core's own copying: ~442 µs to ~1.58 µs, a factor of 280.**

Re-run it with:

```sh
cargo test --release -p m6-core copy_audit -- --ignored --nocapture
```

### The commits

| commit | change |
|---|---|
| `1b05a18` | Found it. The first figure was 3.08 µs from a synthetic 20-key config, and the owner's reply was that 3 µs sounded wrong for building a small map. It was: the parser and the map are 41-583 ns, the copy is everything, and against the real config it is seventy times larger. |
| `479f1df` | The audit the owner asked for, across the whole system. **`m6-http` and `m6-file` were already right** (`Arc<Vec<_>>` headers and `bytes::Bytes` bodies at the edge, `Arc` config and routes in m6-file). `App`, the framework both were meant to migrate onto, was the only offender. |
| `13280a5` | J1 to J5. `Arc<RendererConfig>`; a per-route base dictionary built once per reload; `m6-core/src/dict.rs`, a layered `Dict` of shared base plus per-request overlay; stop merging the same params file twice; stop cloning the dict to merge a context that is usually absent. |
| `5eb9af3` | J6. The last two copies were **borrows dressed as copies**: the route table is shared so routing returns a borrow, and `serve_connection` hands the request to its handler instead of lending it. The only thing blocking that move was `Responder` holding `method: &'a str` when its sole use was `eq_ignore_ascii_case("HEAD")`. |

### The precedence this had to preserve

`build_dict` merges twelve sources in a fixed order and the order is
load-bearing: built-ins go in **after** params files so a params file cannot
override `year`, `datetime` or `request_path`. The layering preserves it by
construction, because built-ins live in the overlay and params files in the
base. The test was written first and is
`dict::tests::a_base_entry_can_never_override_an_overlay_one`.

### The floor, and why it stays

`tera::Context::insert` calls `to_value`, which deep-copies every entry into
the engine's own `BTreeMap`: **143 µs of the remaining 145 µs.** Measured
against a minimal template, which is the case most favourable to it being
negligible, the context build is **99.6%** of the work (178,875 ns to build,
750 ns to render), and the cost scales with the size of the context rather than
with what the template reads, so a page touching three keys still pays to copy
all 1,364 nodes.

**Closed as accepted on the owner's call, 2026-09-13:** *"Don't bother with
tera. That's just how it is."* Recorded rather than struck out because the
number is larger than it looks. If it is ever reopened, the cheaper lever is on
the site side rather than in core: the whole content file is in every page's
context because the config names it as both `global_params` and the route's
`params`.

---

## 3. Allocation, not latency: the 3.6 MB that stopped being read

| commit | change | measurement |
|---|---|---|
| `a094851`, `7932e43` | m6-file answers a HEAD from `metadata.len()` without opening the file, when the representation *is* the file. `7932e43` is the directory case the first version got wrong. | **Structural.** A HEAD on a large image previously paid a whole `fs::read`, then minification, then brotli at level 6, to produce bytes nobody receives. |
| `8c79ee7`, `0759ad4` | Streaming response bodies. `Response.body` is a sum type of `Bytes` and `Stream { len, reader }`, so a file goes to the wire without being materialised. | **Structural.** `/assets/vditor/dist/js/lute/lute.min.js` is 3.6 MB that was read into a `Vec` on every cache miss to be copied straight out again. |

`0759ad4` made the sum type deliberate rather than a `Vec<u8>` beside an
optional reader: `as_bytes()` is `None` for a stream, so the minifier, the
compressor and the default content-hash ETag **structurally cannot** run on a
body core has not read.

---

## 4. §3a: the regression that was not one

**The longest-running open performance item, resolved 2026-09-13 in `9655ef7`,
and the answer is that there was never a code regression.**

`hit_p50_ns` is a **load-dependent measurement**. On a near-idle single-core VM
the cache-hit path goes cold between requests, so the number tracks request
density rather than code.

**Production, syd, same binary, same counter, minutes apart:**

| window | cache hits in it | hit p50 | hit p99 |
|---|---:|---:|---:|
| routine traffic | 50-70 | **3,900-4,000 ns** | 4,200-4,400 ns |
| a tight burst | **1,200** | **1,064 ns** | 3,782 ns |

1,064 ns is *below* the 1.7-2.2 µs band recorded on 2026-09-06 and treated as
the baseline ever since.

**What the timer actually spans.** It starts immediately before the cache
lookup and stops immediately after it, **before the response is written**, so
it covers exactly two in-memory operations: `make_lookup_key` and
`Cache::lookup_with`. The `ctx.start` timers elsewhere in `m6-http/src/main.rs`
belong to the **miss** path and do not feed this number.

**The paired, interleaved A/B the ledger asked for**, five rounds, one host,
across every commit §3a named:

| `084f89e` | `438bdb3` | `b32e837` | `22ee3a4` | `HEAD` |
|---:|---:|---:|---:|---:|
| 125 ns | 125 ns | 125 ns | 125 ns | 125 ns |

Flat. And flat across cache size, 125-166 ns from 1 entry to 20,000, which was
the other candidate.

**Also ruled out, each measured rather than argued:** a slow clocksource (all
three nodes are `kvm-clock` at **20.9 ns/call**), steal time (**0.02%** on syd,
load 0.16), the `22ee3a4` accounting change, and cache growth.

**Consequence for the hourly check.** `hit_p50_ns` is only comparable between
windows with similar hit counts, and the prompt's baseline should say so:
"1.7-2.2 µs at ~40-70 hits/window, ~1.0 µs under sustained load", with the
window's hit count reported beside the number. That prompt is the owner's, so
it is flagged and not changed here.

**Why nobody found it.** Running the A/B turned up the reason no benchmark data
exists across exactly that window: **m6-http's bench stopped compiling at
`438bdb3`**, when `stats.record` gained parameters and the bench was not
updated, and stayed broken until the clippy gate forced `--all-targets` on
2026-09-12. Benches are hidden from `cargo test` the same way the csrf tests
were hidden behind a feature flag, which is lesson 13 in another costume. The
A/B used a portable bench written against the API common to all five commits.

---

## 4a. §4's sequel: `/perf` never reported any latency at all

**Found and fixed 2026-09-15.** §4 established that `hit_p50_ns` is load
dependent and that the number should be read beside its sample count. While
checking the fleet against that, the endpoint that serves it turned out to
report nothing.

`/perf` on syd, with ten hours of uptime behind it:

```json
"cache_hits_total": 338,   "cache_misses_total": 1035,
"hit_samples": 0, "hit_p50_ns": 0, "hit_p99_ns": 0,
"miss_samples": 0, "monitor_samples": 0
```

338 hits counted, **zero latency samples**. m6-monitor correctly reads zero
samples as "not measured" and publishes `null`, so the fleet digest carried no
latency for any node and never had. The one number the monitor exists to trend
was structurally absent, while the `periodic stats` line was printing
`hit_p50_ns=3878` for the same counter in the same minute.

### Cause

`maybe_emit` reset `hit_idx` and `hit_count` to 0 every ten seconds, and
`snapshot()` -- what `/perf` serves -- read those same fields. So `/perf`
reported the percentiles of whatever fraction of a ten-second window happened to
be open when it was scraped. **syd takes about two requests a minute**, so
almost every ten-second window contains no cache hit at all.

`snapshot()`'s own comment reasoned carefully about not *resetting* from the
scrape path, so a polling monitor could not gut the operational log. That was
right, and it was incomplete: it read the window the emitter did reset.

### Not everything was broken, which is why it survived

The **per-channel** reservoirs were never cleared, so they held real data the
whole time. Same scrape, same second:

| channel | requests | hit samples | hit p50 |
|---|---:|---:|---:|
| http/1.1/external | 839 | 35 | 3,191 ns |
| http/2/external | 249 | 131 | 2,604 ns |
| http/2/internal | 569 | 174 | 2,789 ns |

The aggregate was the only broken figure, and it is the only one m6-monitor
reads. Anyone opening `/perf` and scrolling past the aggregate would have seen
plausible numbers.

### Fix

One reservoir, run as a ring and never cleared. `*_window_added` counters give
the periodic log its own ten-second window, so the operational logging is
unchanged. `percentiles_ring` reads the newest *n* entries rather than
`samples[..n]` from the front, which had been correct only because the index was
reset every window and would have reported the **oldest** samples as current
once the ring genuinely wrapped. No extra memory, no extra work per request.

`snapshot()` now spans the most recent up to `RESERVOIR` (4096) samples, **which
is a count, not a period of time.** On a quiet node it reaches back hours and
blends idle and busy traffic. That is exactly why `hit_samples` is reported
beside it, in the periodic log and in the monitor digest as well as in `/perf`:
per §4 the percentile means nothing without it.

### Verified

Unit: `stats::perf_reservoir_tests`, six tests. Three of them fail against the
old reset, including `an_emit_does_not_erase_what_perf_reports`; the other three
are properties that held either way. Checked by reinstating the old reset and
watching them fail, because a regression test that passes against the broken
version is worse than no test.

End to end: the 05-cms example stack with a perf token, 40 requests, then scrapes
across two emit boundaries with silence in between -- the ordering that used to
return zeros.

| scrape | hit_samples | hit_p50_ns | hit_p99_ns |
|---|---:|---:|---:|
| straight after traffic | 81 | 2,250 | 5,625 |
| after 14s of silence | 81 | 2,250 | 5,625 |
| after 14s more | 81 | 2,250 | 5,625 |

And the periodic log still means "this window": `cache_hits=39
hit_p50_ns=2208` in the busy window, zeros in the idle ones either side.

---

## 5. How to reproduce any of this

**Copy audit** (core's per-request copying, against the real content file):

```sh
cargo test --release -p m6-core copy_audit -- --ignored --nocapture
cargo test --release -p m6-core dict_cost  -- --ignored --nocapture
```

Both are `#[ignore]`d on purpose. **A timing assertion is the wall-clock trap**
that `test_static_file_cache_hit` already fell into once: it fires on a loaded
build box against correct code. These are measurements, not tests.

**Page render A/B.** Build `m6-html` at the two commits into separate target
directories, then alternate: start one on a unix socket against
`the deployment repository` and the production `m6-html.conf`, warm it, time 300
requests, kill it, repeat with the other, five rounds. Interleaving is the
point; running all of one then all of the other measures the afternoon.

**Cache-hit A/B across commits.** `git worktree add --detach` per commit, a
portable bench in `m6-http/examples/` using only the API common to all of them
(`Cache::new`, `CacheKey::new`, `insert`, `get`, `make_lookup_key`), built with
a per-commit `CARGO_TARGET_DIR`, then interleaved rounds.

**Production hit_p50 under load.** On the node, warm one cacheable page, then
issue a few thousand rapid requests over loopback and read the `periodic stats`
line whose window contains them. Report the hit count with the percentile or
the number means nothing.

---

## 5a. Connection setup, per channel, measured by our own clients

Added 2026-09-15. Three new single-purpose binaries, `m6-probe-h1`,
`m6-probe-h2` and `m6-probe-h3`, each doing one handshake per connection,
strictly sequentially, with no requests and no charts. Source in
`m6-http/src/probe.rs`. m6-http publishes the same measurement per channel on
`/perf`, and the point of the probes is that the server's figure about itself is
now checkable.

### The figures

On the build host's loopback, 200 sequential handshakes each, against the
server's own report of the same connections:

| channel | probe p50 | `/perf` p50 | `/perf` samples |
|---|---|---|---|
| http/1.1 | 0.381 ms | 0.425 ms | 200 of 200 |
| http/2 | 0.353 ms | 0.423 ms | 270 (200 probe + 70 warmer) |
| http/3 | 1.132 ms | 1.075 ms | 200 of 200 |

The client figure sits just above the server's for h1 and h3 because the client
times from its own first send and the server from receiving that packet.

**h1 and h2 are the rustls handshake and EXCLUDE the TCP round trip**, because
rustls is handed the socket after the three-way handshake finishes. **h3 is the
QUIC handshake and INCLUDES its equivalent**, because QUIC folds transport and
crypto together and there is no earlier point to start from. The two are not the
same span and must never be averaged. A single "handshake p50" across all three
would track the protocol mix, which is the error §4a fixed for the request
latency aggregate.

### Loopback makes h3 look slow, and that is an artefact

On loopback h3 reads 1.1 ms against h2's 0.42 ms, because loopback has no round
trip and so prices only CPU. Over a real path it inverts. From a laptop to the
build host, 5.1 ms RTT measured by ping:

| | real path, 5.1 ms RTT |
|---|---|
| h1 rustls handshake (excl. TCP connect) | 7.79 ms |
| h2 rustls handshake (excl. TCP connect) | 6.14 ms |
| h3 cold QUIC handshake | 12.2 ms |
| **h3 0-RTT, first packet to response headers** | **6.0 - 6.7 ms** |

h1 and h2 need a TCP round trip before any of that, so a returning visitor over
h2 pays TCP, then TLS, then a request round trip: roughly 16 ms to a response
where h3 with 0-RTT answers in 6.4 ms.

**Never conclude anything about protocol choice from a loopback number.**

### An extra round trip on every new h3 connection, and how it was misdiagnosed twice

**Fixed 2026-09-15** by `cfg.set_max_amplification_factor(4)`, which is temporary
and tracked as ledger item 0. Staging went from **12.5 ms to 6.8 ms**.

A QUIC server may send only `factor x bytes received` before it has validated the
client's address (RFC 9000 8.1, to stop it being used as a reflection amplifier).
A client's opening Initial is padded to 1200 bytes, so at quiche's default factor
of 3 the budget is 3600. Our handshake flight is 4082 bytes, nearly all
certificate chain. `m6-probe-h3`'s timeline, against a 4.85 ms RTT:

```
+0.741ms  client 1200B
+7.018ms  server 1200B   1.00x
+7.223ms  server 2400B   2.00x
+7.240ms  server 3600B   3.00x   <- stops dead, 482 bytes still owed
+12.221ms server 4082B           <- one full round trip later
```

At factor 4 the budget is 4800 and the whole flight goes out at once: the final
datagram arrives 0.10-0.33 ms after the previous one instead of 4.98-6.28 ms.
**Conformance is unchanged at 47/49** — h3spec does not test this limit, which was
measured rather than assumed.

#### This is the ordinary case, not something unusual about this deployment

Worth stating plainly because the first two explanations written here assumed the
opposite. [Fastly's study](https://www.fastly.com/blog/quic-handshake-tls-compression-certificates-extension-study)
measured **40-44% of uncompressed chains** exceeding the budget, and certificate
compression taking that to **1-9%**. [Other work](https://blog.apnic.net/2023/01/16/on-the-interplay-between-tls-certificates-and-quic-performance/)
puts it at **61%** for a Firefox-sized 1352-byte Initial. Roughly half the QUIC
internet pays this round trip.

Measured chains, for scale. Ours is unremarkable and two are larger:

| site | certs | chain bytes |
|---|---|---|
| this deployment | 4 | 3429 |
| news.ycombinator.com | 4 | 3393 |
| letsencrypt.org | 4 | 3576 |
| cloudflare.com | 3 | 2552 |
| github.com | 3 | 2718 |
| www.google.com | 3 | 3755 |
| www.mozilla.org | 3 | 4051 |

Hacker News and letsencrypt.org carry the identical 4-certificate Let's Encrypt
chain, byte for byte on the intermediates.

#### Two wrong diagnoses, both of which produced a confident explanation

Recorded because each was stated as fact and each had to be withdrawn.

1. **"The chain is unusually long because it is on a new hierarchy."** Wrong. The
   chain is a standard Let's Encrypt ECDSA chain that many sites use, and Google's
   and Mozilla's are bigger. Withdrawn after measuring seven sites instead of
   reasoning about one.

2. **"The amplification limit is not involved, the ratio is only 1.62x."** Wrong,
   and wrong in the more instructive way: 1.62x is the ratio at the END of the
   handshake, by which time the client has sent its ACKs. The ratio at the instant
   the server stopped was exactly 3.00x. A totals-only view cannot see this, which
   is why the probe now prints a per-packet timeline with the live ratio.

   This retraction was itself wrong, and diagnosis 1's replacement — "their chain
   is smaller so they fit" — did not survive its own arithmetic either, since
   Cloudflare sends 4198 bytes, which does not fit in 3600. What actually differs
   is the factor each server runs at.

A third hypothesis, **pacing**, was tested and killed rather than argued: quiche
enables pacing by default and `flush_conn` discards `send_info.at`, so it looked
like a strong candidate. Disabling pacing on staging changed the stall not at all.

#### What other edges appear to run at

Measured with `m6-probe-h3` on a 1200-byte client Initial. **Treat this as our
measurement, not established fact**: byte accounting here counts QUIC payload, and
no public source corroborates servers exceeding the limit.

| edge | bytes sent before establishment | implied factor |
|---|---|---|
| m6 at quiche's default | 3600 then stops | 3.00x |
| Cloudflare | 4198 in one flight | 3.50x |
| Fastly (serving www.mozilla.org) | 5360 in one flight | 4.47x |

#### The proper fix, measured

Certificate compression (RFC 8879), tracked as issue #28. On our own chain:

| | bytes |
|---|---|
| chain uncompressed | 3400 |
| chain, zlib (RFC 8879 algorithm 1) | **2345** |
| saving | **1055** |
| needed to fit at factor 3 | 482 |
| flight after compression | ~3027 |
| margin under the 3600 budget | **573** |

zlib alone is more than enough and is the weakest of the three algorithms. It has
no compatibility cost, unlike trimming the chain: a client that does not advertise
`compress_certificate` simply gets today's behaviour. Not reachable today — quiche
binds 12 `SSL_CTX_*` functions and `SSL_CTX_add_cert_compression_alg` is not among
them, though BoringSSL underneath implements it. When it lands, the factor
override is deleted.

rustls supports it for h1 and h2 behind its `brotli` and `zlib` features, neither
of which this build enables. No round-trip win there, since TCP has no
amplification limit, just fewer bytes.

### 0-RTT

Enabled 2026-09-15 (`cfg.enable_early_data()`), and it engages: verified on
staging with `m6-probe-h3 --0rtt /`, which reports whether the request was on
the wire before the handshake completed rather than inferring it from timing.

0-RTT data is replayable, so `handle_h3_request` is deliberately stricter than
RFC 8470's "idempotent methods" advice: in early data it serves **only a fresh
cache hit** and answers 425 Too Early to everything else. A replayed cache read
re-sends bytes and does nothing more. "GET is safe" would not have been enough,
because this site's analytics beacon is a fire-and-forget GET, and a stale hit
would queue a background refresh, which is a write. Both gates verified against
staging: a cached path answers 200 in early data, an uncached one answers 425.

### Reproducing

```sh
cargo build --release --bin m6-probe-h1 --bin m6-probe-h2 --bin m6-probe-h3
./target/release/m6-probe-h1 --addr HOST:443 --n 200
./target/release/m6-probe-h2 --addr HOST:443 --n 200
./target/release/m6-probe-h3 --addr HOST:443 --n 200
./target/release/m6-probe-h3 --addr HOST:443 --0rtt /
./target/release/m6-probe-h3 --addr HOST:443 --0rtt /some-path --method POST
```

### Two measuring-tool defects found on the way, both of which produced numbers

Recorded because in both cases the tool reported success and a plausible figure,
which is worse than a tool that fails.

1. **`m6-bench-detail` panicked before measuring anything.** m6-http builds
   rustls with `default-features = false`, so no process-level CryptoProvider is
   installed automatically, and the first `ClientConfig::builder()` panicked.
   Every other binary in the crate installs it; this one did not. With our own
   client broken, handshake timing was taken with `h3spec` instead -- a
   conformance tester that deliberately opens stalled connections -- which
   reported an **h3 handshake p50 of 113 ms on loopback** for an engine that
   answers requests in microseconds. That figure was believed long enough to be
   written down. The real figure is 1.1 ms.

2. **The first version of `m6-probe-h1/h2` never completed a handshake.**
   rustls' client reports `is_handshaking() == false` as soon as it has the
   traffic keys, one step before the client Finished is flushed. The loop exited
   on that condition and dropped the socket, so the server never received
   Finished, sat handshaking until EOF, and recorded nothing. The probe reported
   200 successes at a plausible 0.34 ms while the server completed zero. The 70
   h2 samples the server did report turned out to be the cache warmer's curl
   connections at startup, which very nearly got read as a server bug.

A third defect, this one in m6-http itself and fixed here: the handshake was
stamped about forty lines below the point where `advance_tls` returns an error,
so a client that completed its handshake and closed immediately had the
measurement thrown away. h1 recorded **0 of 200** such handshakes, and h2 kept
only the ones the server reached before the close -- that is, the slow ones --
giving a p50 of **6.14 ms against a true 0.42 ms**. A biased partial sample set
is worse than none, because 6.14 ms looked plausible.

---

## 6. What is still unmeasured

Stated plainly, because a performance document that implies more coverage than
it has is how the next person gets misled.

- **Nothing here has run on the production nodes** except §4. The render figure
  is a laptop figure; syd is a 1-core VM and will differ in magnitude, though
  the direction is a property of the code.
- **No throughput or concurrency measurement** of the two migrated services.
  `m6-file` moved from an unbounded channel and a fixed worker set to `App`'s
  bounded pool with 503 backpressure, and `m6-auth-server` from a thread per
  connection to the same pool. Both are better shapes under overload and
  neither has a number. The gallery page, which fires dozens of concurrent
  image requests and is why m6-file's pool was widened to 32, is the case to
  measure.
- **No h2 or h3 conformance-side performance data.** h2spec and h3spec are
  correctness gates, not timing. h3spec in particular must NEVER be used as a
  load generator for a timing measurement: it deliberately opens stalled and
  malformed connections, and doing this produced the 113 ms figure in §5a.
- **The 6 ms per page** quoted in the handover predates all of this and was
  measured on syd under unknown conditions. It should be re-measured after a
  deploy rather than carried forward.
