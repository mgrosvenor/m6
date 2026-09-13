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

**Production, origin, same binary, same counter, minutes apart:**

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
three nodes are `kvm-clock` at **20.9 ns/call**), steal time (**0.02%** on origin,
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

## 6. What is still unmeasured

Stated plainly, because a performance document that implies more coverage than
it has is how the next person gets misled.

- **Nothing here has run on the production nodes** except §4. The render figure
  is a laptop figure; origin is a 1-core VM and will differ in magnitude, though
  the direction is a property of the code.
- **No throughput or concurrency measurement** of the two migrated services.
  `m6-file` moved from an unbounded channel and a fixed worker set to `App`'s
  bounded pool with 503 backpressure, and `m6-auth-server` from a thread per
  connection to the same pool. Both are better shapes under overload and
  neither has a number. The gallery page, which fires dozens of concurrent
  image requests and is why m6-file's pool was widened to 32, is the case to
  measure.
- **No h2 or h3 conformance-side performance data.** h2spec and h3spec are
  correctness gates, not timing.
- **The 6 ms per page** quoted in the handover predates all of this and was
  measured on origin under unknown conditions. It should be re-measured after a
  deploy rather than carried forward.
