/// Criterion benchmarks for the m6-http software critical path.
///
/// Target: cache-hit path under 100ns total application code.
/// Run with: cargo bench -p m6-http
use criterion::{black_box, criterion_group, Criterion};

use m6_http_lib::cache::{Cache, CacheKey, CachedResponse, make_lookup_key};
use m6_http_lib::stats::Stats;

// ── Fixtures ──────────────────────────────────────────────────────────────────

fn make_cache_with_entry() -> (Cache, &'static str, &'static str) {
    let cache = Cache::new();
    let path = "/blog/hello-world";
    let enc = "gzip";
    let key = CacheKey::new(path, enc);
    let resp = CachedResponse {
        status: 200,
        headers: std::sync::Arc::new(vec![
            ("content-type".to_string(), "text/html; charset=utf-8".to_string()),
            ("cache-control".to_string(), "public, max-age=3600".to_string()),
            ("vary".to_string(), "accept-encoding".to_string()),
        ]),
        body: bytes::Bytes::from_static(b"<html><body>hello world</body></html>"),
    };
    cache.insert(key, resp);
    (cache, path, enc)
}

// ── Percentile reporter ────────────────────────────────────────────────────────

/// Run `f` for `n` iterations (after 10% warmup), collect raw nanosecond
/// timings, then print p0/p1/p50/p99/p100/avg/stddev/count.
fn report_percentiles<F: FnMut()>(label: &str, n: usize, mut f: F) {
    let warmup = n / 10;
    for _ in 0..warmup { f(); }

    let mut samples: Vec<u64> = Vec::with_capacity(n);
    for _ in 0..n {
        let t0 = std::time::Instant::now();
        f();
        samples.push(t0.elapsed().as_nanos() as u64);
    }

    samples.sort_unstable();
    let count = samples.len() as f64;

    let p = |pct: f64| -> u64 {
        let idx = ((pct / 100.0) * count) as usize;
        samples[idx.min(samples.len() - 1)]
    };

    let avg = samples.iter().sum::<u64>() as f64 / count;
    let variance = samples.iter()
        .map(|&x| { let d = x as f64 - avg; d * d })
        .sum::<f64>() / count;
    let stddev = variance.sqrt();

    println!(
        "\n── {label} (n={n}) ─────────────────────────────────────────────\n\
         p0={p0}ns  p1={p1}ns  p50={p50}ns  p99={p99}ns  p100={p100}ns\n\
         avg={avg:.1}ns  stddev={stddev:.1}ns",
        p0   = p(0.0),
        p1   = p(1.0),
        p50  = p(50.0),
        p99  = p(99.0),
        p100 = p(100.0),
    );
}

// ── Benchmarks ────────────────────────────────────────────────────────────────

/// make_lookup_key: build the cache lookup key into a stack buffer.
/// Expected: ~5–10ns
fn bench_make_lookup_key(c: &mut Criterion) {
    let mut group = c.benchmark_group("make_lookup_key");
    group.sample_size(100_000);
    let mut buf = [0u8; 512];
    group.bench_function("make_lookup_key", |b| {
        b.iter(|| {
            let key = make_lookup_key(
                black_box("/blog/hello-world"),
                black_box("gzip"),
                &mut buf,
            );
            black_box(key.len())
        })
    });
    group.finish();
}

/// Full cache hit: make_lookup_key + AHashMap::get.
/// Expected: ~20–40ns (hot cache line in L1)
fn bench_cache_hit(c: &mut Criterion) {
    let (cache, path, enc) = make_cache_with_entry();
    let mut group = c.benchmark_group("cache_hit");
    group.sample_size(100_000);
    group.bench_function("cache_hit", |b| {
        b.iter(|| {
            let mut buf = [0u8; 512];
            let key = make_lookup_key(black_box(path), black_box(enc), &mut buf);
            black_box(cache.get(key))
        })
    });
    group.finish();
}

/// Cache miss (key not present).
/// Expected: ~15–25ns (hash + one probe, no value copy)
fn bench_cache_miss(c: &mut Criterion) {
    let (cache, _, _) = make_cache_with_entry();
    let mut group = c.benchmark_group("cache_miss");
    group.sample_size(100_000);
    group.bench_function("cache_miss", |b| {
        b.iter(|| {
            let mut buf = [0u8; 512];
            let key = make_lookup_key(black_box("/not/in/cache"), black_box("br"), &mut buf);
            black_box(cache.get(key))
        })
    });
    group.finish();
}

/// Stats::record — the per-request instrumentation call.
/// Expected: <5ns (3 integer increments + 1 array write).
/// Uses iter_custom to batch calls — operation is sub-ns and trips zero-time guards otherwise.
fn bench_stats_record(c: &mut Criterion) {
    let mut stats = Stats::new();
    let mut group = c.benchmark_group("stats_record");
    group.sample_size(10_000);
    group.bench_function("stats_record", |b| {
        b.iter_custom(|iters| {
            let start = std::time::Instant::now();
            for _ in 0..iters {
                stats.record(black_box(250), black_box(true), black_box(false));
            }
            start.elapsed()
        })
    });
    group.finish();
}

/// Header extraction loop: iterate over a typical quiche::h3::Header list,
/// extracting :path, :method, accept-encoding.
/// Expected: ~15–25ns for 6 headers
fn bench_h3_header_extract(c: &mut Criterion) {
    use quiche::h3::NameValue;
    let headers: Vec<quiche::h3::Header> = vec![
        quiche::h3::Header::new(b":method", b"GET"),
        quiche::h3::Header::new(b":path", b"/blog/hello-world"),
        quiche::h3::Header::new(b":scheme", b"https"),
        quiche::h3::Header::new(b":authority", b"example.com"),
        quiche::h3::Header::new(b"accept-encoding", b"br, gzip;q=0.9"),
        quiche::h3::Header::new(b"user-agent", b"Mozilla/5.0"),
    ];
    let mut group = c.benchmark_group("h3_header_extract");
    group.sample_size(100_000);
    group.bench_function("h3_header_extract", |b| {
        b.iter(|| {
            let mut path = b"/" as &[u8];
            let mut method = b"GET" as &[u8];
            let mut enc = b"" as &[u8];
            for h in black_box(&headers) {
                match h.name() {
                    b":path"           => path = h.value(),
                    b":method"         => method = h.value(),
                    b"accept-encoding" => enc = h.value(),
                    _ => {}
                }
            }
            black_box((path, method, enc))
        })
    });
    group.finish();
}

/// Full application critical path: header extract + lookup_key + cache get.
/// This is the combined software budget — target <100ns.
fn bench_full_cache_hit_path(c: &mut Criterion) {
    use quiche::h3::NameValue;
    let (cache, _, _) = make_cache_with_entry();
    let headers: Vec<quiche::h3::Header> = vec![
        quiche::h3::Header::new(b":method", b"GET"),
        quiche::h3::Header::new(b":path", b"/blog/hello-world"),
        quiche::h3::Header::new(b":scheme", b"https"),
        quiche::h3::Header::new(b":authority", b"example.com"),
        quiche::h3::Header::new(b"accept-encoding", b"gzip"),
        quiche::h3::Header::new(b"user-agent", b"Mozilla/5.0"),
    ];
    let mut group = c.benchmark_group("full_cache_hit_path");
    group.sample_size(100_000);
    group.bench_function("full_cache_hit_path", |b| {
        b.iter(|| {
            let mut path: &[u8] = b"/";
            let mut enc: &[u8] = b"";
            for h in black_box(&headers) {
                match h.name() {
                    b":path"           => path = h.value(),
                    b"accept-encoding" => enc = h.value(),
                    _ => {}
                }
            }
            let path_str = std::str::from_utf8(path).unwrap_or("/");
            let enc_str  = std::str::from_utf8(enc).unwrap_or("");
            let mut buf = [0u8; 512];
            let key = make_lookup_key(path_str, enc_str, &mut buf);
            black_box(cache.get(key))
        })
    });
    group.finish();
}

criterion_group!(
    benches,
    bench_make_lookup_key,
    bench_cache_hit,
    bench_cache_miss,
    bench_stats_record,
    bench_h3_header_extract,
    bench_full_cache_hit_path,
);

// ── Custom main: criterion + raw percentile report ────────────────────────────

fn main() {
    // criterion_group! generates a zero-arg fn that creates its own Criterion.
    benches();

    // Raw percentile report — 100K samples each, 10K warmup.
    const N: usize = 100_000;

    println!("\n\
        ════════════════════════════════════════════════════════════════\n\
        Raw percentile report  (100K samples, 10K warmup, release mode)\n\
        System: Apple M4 (macOS 15.7.4)\n\
        ════════════════════════════════════════════════════════════════");

    {
        let mut buf = [0u8; 512];
        report_percentiles("make_lookup_key", N, || {
            let key = make_lookup_key(
                black_box("/blog/hello-world"),
                black_box("gzip"),
                &mut buf,
            );
            black_box(key.len());
        });
    }

    {
        let (cache, path, enc) = make_cache_with_entry();
        report_percentiles("cache_hit", N, || {
            let mut buf = [0u8; 512];
            let key = make_lookup_key(black_box(path), black_box(enc), &mut buf);
            black_box(cache.get(key));
        });
    }

    {
        let (cache, _, _) = make_cache_with_entry();
        report_percentiles("cache_miss", N, || {
            let mut buf = [0u8; 512];
            let key = make_lookup_key(black_box("/not/in/cache"), black_box("br"), &mut buf);
            black_box(cache.get(key));
        });
    }

    {
        let mut stats = Stats::new();
        report_percentiles("stats_record", N, || {
            stats.record(black_box(250), black_box(true), black_box(false));
        });
    }

    {
        use quiche::h3::NameValue;
        let headers: Vec<quiche::h3::Header> = vec![
            quiche::h3::Header::new(b":method", b"GET"),
            quiche::h3::Header::new(b":path", b"/blog/hello-world"),
            quiche::h3::Header::new(b":scheme", b"https"),
            quiche::h3::Header::new(b":authority", b"example.com"),
            quiche::h3::Header::new(b"accept-encoding", b"br, gzip;q=0.9"),
            quiche::h3::Header::new(b"user-agent", b"Mozilla/5.0"),
        ];
        report_percentiles("h3_header_extract", N, || {
            let mut path = b"/" as &[u8];
            let mut method = b"GET" as &[u8];
            let mut enc = b"" as &[u8];
            for h in black_box(&headers) {
                match h.name() {
                    b":path"           => path = h.value(),
                    b":method"         => method = h.value(),
                    b"accept-encoding" => enc = h.value(),
                    _ => {}
                }
            }
            black_box((path, method, enc));
        });
    }

    {
        use quiche::h3::NameValue;
        let (cache, _, _) = make_cache_with_entry();
        let headers: Vec<quiche::h3::Header> = vec![
            quiche::h3::Header::new(b":method", b"GET"),
            quiche::h3::Header::new(b":path", b"/blog/hello-world"),
            quiche::h3::Header::new(b":scheme", b"https"),
            quiche::h3::Header::new(b":authority", b"example.com"),
            quiche::h3::Header::new(b"accept-encoding", b"gzip"),
            quiche::h3::Header::new(b"user-agent", b"Mozilla/5.0"),
        ];
        report_percentiles("full_cache_hit_path", N, || {
            let mut path: &[u8] = b"/";
            let mut enc: &[u8] = b"";
            for h in black_box(&headers) {
                match h.name() {
                    b":path"           => path = h.value(),
                    b"accept-encoding" => enc = h.value(),
                    _ => {}
                }
            }
            let path_str = std::str::from_utf8(path).unwrap_or("/");
            let enc_str  = std::str::from_utf8(enc).unwrap_or("");
            let mut buf = [0u8; 512];
            let key = make_lookup_key(path_str, enc_str, &mut buf);
            black_box(cache.get(key));
        });
    }

    println!("\n════════════════════════════════════════════════════════════════\n");
}
