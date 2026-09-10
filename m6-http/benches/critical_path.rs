/// Criterion benchmarks for the m6-http software critical path.
///
/// Target: cache-hit path under 100ns total application code.
/// Run with: cargo bench -p m6-http
use criterion::{black_box, criterion_group, Criterion};

use m6_http_lib::cache::{Cache, CacheKey, CachedResponse, make_lookup_key};
use m6_http_lib::stats::{Channel, Iface, Stats, Version};

// ── Fixtures ──────────────────────────────────────────────────────────────────

fn make_cache_with_entry() -> (Cache, &'static str, &'static str) {
    let cache = Cache::with_fixed_seed_for_bench();
    let path = "/blog/hello-world";
    let enc = "gzip";
    let key = CacheKey::new(path, None, enc);
    let resp = CachedResponse {
        status: 200,
        headers: std::sync::Arc::new(vec![
            ("content-type".to_string(), "text/html; charset=utf-8".to_string()),
            ("cache-control".to_string(), "public, max-age=3600".to_string()),
            ("vary".to_string(), "accept-encoding".to_string()),
        ]),
        body: bytes::Bytes::from_static(b"<html><body>hello world</body></html>"),
        hints: std::sync::Arc::new(vec![]),
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
                None,
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
            let key = make_lookup_key(black_box(path), None, black_box(enc), &mut buf);
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
            let key = make_lookup_key(black_box("/not/in/cache"), None, black_box("br"), &mut buf);
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
                stats.record(
                    black_box(250),
                    black_box(true),
                    black_box(200),
                    black_box(Channel::new(Version::Http2, Iface::External)),
                    black_box("m6-html"),
                );
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
            let key = make_lookup_key(path_str, None, enc_str, &mut buf);
            black_box(cache.get(key))
        })
    });
    group.finish();
}

// ── Security-fix hot paths ────────────────────────────────────────────────────
//
// The audit fixes added work to three places every request or response passes
// through. These benches exist so that cost is measured rather than assumed.

/// Request ingress: parse + framing validation + proxy-owned header stripping.
/// Runs once per HTTP/1.1 request.
fn bench_parse_request(c: &mut Criterion) {
    let raw: &[u8] = b"GET /blog/hello-world?utm_source=x HTTP/1.1\r\n\
                       Host: example.com\r\n\
                       User-Agent: Mozilla/5.0\r\n\
                       Accept: text/html\r\n\
                       Accept-Encoding: gzip, br\r\n\
                       Cookie: _m6sid=abc123\r\n\r\n";
    let mut group = c.benchmark_group("parse_request");
    group.sample_size(100_000);
    group.bench_function("parse_request", |b| {
        b.iter(|| black_box(m6_http_lib::http11::parse_request(black_box(raw))))
    });
    group.finish();
}

/// Response egress: security-header injection. Runs once per response, on
/// every protocol.
fn bench_security_headers(c: &mut Criterion) {
    m6_http_lib::security::configure(&m6_http_lib::config::SecurityConfig::default());
    let response_headers = vec![
        ("content-type".to_string(), "text/html; charset=utf-8".to_string()),
        ("cache-control".to_string(), "public, max-age=3600".to_string()),
        ("alt-svc".to_string(), "h3=\":8443\"; ma=86400".to_string()),
    ];
    let mut group = c.benchmark_group("security_headers");
    group.sample_size(100_000);
    // Buffer is reused across iterations: `build_response` allocates one Vec
    // for the whole response, so charging a fresh malloc to this step would
    // overstate it.
    let mut out: Vec<u8> = Vec::with_capacity(1024);
    group.bench_function("security_headers", |b| {
        b.iter(|| {
            out.clear();
            m6_http_lib::security::write_h1_headers(
                &mut out,
                black_box(&response_headers),
            );
            black_box(out.len())
        })
    });
    group.finish();
}

/// Decomposition of `parse_request`, to attribute its cost rather than guess.
/// `full` minus `httparse_only` is what m6-http itself adds on top of the
/// parser; `header_array_init` is the fixed setup cost both pay.
fn bench_parse_request_breakdown(c: &mut Criterion) {
    let raw: &[u8] = b"GET /blog/hello-world?utm_source=x HTTP/1.1\r\n\
                       Host: example.com\r\n\
                       User-Agent: Mozilla/5.0\r\n\
                       Accept: text/html\r\n\
                       Accept-Encoding: gzip, br\r\n\
                       Cookie: _m6sid=abc123\r\n\r\n";
    let mut group = c.benchmark_group("parse_breakdown");
    group.sample_size(100_000);

    // Just zeroing the 64-slot header array every call.
    group.bench_function("header_array_init", |b| {
        b.iter(|| {
            let headers = [httparse::EMPTY_HEADER; 64];
            black_box(headers.len())
        })
    });

    // Array init + httparse, with no owned data produced.
    group.bench_function("httparse_only", |b| {
        b.iter(|| {
            let mut headers = [httparse::EMPTY_HEADER; 64];
            let mut req = httparse::Request::new(&mut headers);
            black_box(req.parse(black_box(raw)).is_ok())
        })
    });

    group.finish();
}

/// The H1 read loop re-parses the accumulated buffer on every read event, so
/// a body arriving in N chunks is parsed N times. This models that: it is the
/// shape in which the removed `buf.clone()` was quadratic, and it shows what a
/// chunked upload actually costs to accumulate.
///
/// 256 KiB body in 4 KiB chunks = 64 read events.
fn bench_chunked_body_accumulation(c: &mut Criterion) {
    const CHUNK: usize = 4096;

    let mut group = c.benchmark_group("chunked_body");
    group.sample_size(50);

    // Two sizes, so the shape of the curve is visible rather than asserted.
    // The clone is O(bytes accumulated so far) and runs once per read event,
    // so total copying is O(N²) in the number of chunks: quadrupling the body
    // should roughly 16x the clone variant while only 4x-ing the fixed one.
    for &body in &[256 * 1024usize, 1024 * 1024usize] {
        let head = format!(
            "POST /upload HTTP/1.1\r\nHost: example.com\r\nContent-Length: {body}\r\n\r\n"
        );
        let mut full = head.into_bytes();
        full.extend(std::iter::repeat(b'x').take(body));
        let kib = body / 1024;

        // What the code used to do: clone the accumulated buffer every read
        // event to release a borrow, then parse the clone.
        group.bench_function(format!("{kib}KiB/with_clone_before_parse"), |b| {
            b.iter(|| {
                let mut buf: Vec<u8> = Vec::new();
                let mut completed = false;
                for chunk in full.chunks(CHUNK) {
                    buf.extend_from_slice(chunk);
                    let snap = buf.clone();
                    if let m6_http_lib::http11::ParseResult::Complete(_) =
                        m6_http_lib::http11::parse_request(black_box(&snap))
                    {
                        completed = true;
                    }
                }
                black_box(completed)
            })
        });

        // What it does now: parse the buffer in place.
        group.bench_function(format!("{kib}KiB/parse_in_place"), |b| {
            b.iter(|| {
                let mut buf: Vec<u8> = Vec::new();
                let mut completed = false;
                for chunk in full.chunks(CHUNK) {
                    buf.extend_from_slice(chunk);
                    if let m6_http_lib::http11::ParseResult::Complete(_) =
                        m6_http_lib::http11::parse_request(black_box(&buf))
                    {
                        completed = true;
                    }
                }
                black_box(completed)
            })
        });
    }
    group.finish();
}

/// A/B for the cache-key change (finding 5). The key gained a `query`
/// component, so this measures the same function with and without one — same
/// binary, same run, same thermal state, so the delta is the change's real
/// cost rather than a cross-run comparison on a noisy machine.
fn bench_lookup_key_query_cost(c: &mut Criterion) {
    let mut group = c.benchmark_group("lookup_key_query_cost");
    group.sample_size(100_000);
    let mut buf = [0u8; 512];

    // Shape the pre-fix key had: path + encoding only.
    group.bench_function("no_query", |b| {
        b.iter(|| {
            let k = make_lookup_key(
                black_box("/blog/hello-world"),
                black_box(None),
                black_box("gzip"),
                &mut buf,
            );
            black_box(k.len())
        })
    });

    // Typical query-bearing request.
    group.bench_function("with_query", |b| {
        b.iter(|| {
            let k = make_lookup_key(
                black_box("/blog/hello-world"),
                black_box(Some("utm_source=news&ref=hn")),
                black_box("gzip"),
                &mut buf,
            );
            black_box(k.len())
        })
    });
    group.finish();
}

/// Route lookup guarding the cache. Runs before every cache lookup so that a
/// `require`-protected route is never served from a key with no identity.
fn bench_requires_auth(c: &mut Criterion) {
    use m6_http_lib::router::RouteTable;
    let mut group = c.benchmark_group("requires_auth");
    group.sample_size(100_000);

    // Site with at least one protected route: a real lookup is required.
    let guarded = RouteTable::for_bench(&[("/blog/{stem}", None), ("/admin", Some("group:admins"))]);
    group.bench_function("site_with_protected_routes", |b| {
        b.iter(|| black_box(guarded.requires_auth(black_box("/blog/hello-world"))))
    });

    // A site with no `require` anywhere is deliberately not benchmarked here:
    // `has_protected_routes` short-circuits before the route lookup, so the
    // call folds to a constant and criterion measures zero time per iteration
    // (5B iterations, below timer resolution). Any construct that defeats the
    // optimizer enough to produce a number would be measuring the scaffolding
    // rather than the code. Treat the fast path as free; the number that
    // matters is the guarded case above, which is the worst case.
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
    bench_parse_request,
    bench_security_headers,
    bench_requires_auth,
    bench_lookup_key_query_cost,
    bench_parse_request_breakdown,
    bench_chunked_body_accumulation,
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
                None,
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
            let key = make_lookup_key(black_box(path), None, black_box(enc), &mut buf);
            black_box(cache.get(key));
        });
    }

    {
        let (cache, _, _) = make_cache_with_entry();
        report_percentiles("cache_miss", N, || {
            let mut buf = [0u8; 512];
            let key = make_lookup_key(black_box("/not/in/cache"), None, black_box("br"), &mut buf);
            black_box(cache.get(key));
        });
    }

    {
        let mut stats = Stats::new();
        report_percentiles("stats_record", N, || {
            stats.record(
                    black_box(250),
                    black_box(true),
                    black_box(200),
                    black_box(Channel::new(Version::Http2, Iface::External)),
                    black_box("m6-html"),
                );
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
            let key = make_lookup_key(path_str, None, enc_str, &mut buf);
            black_box(cache.get(key));
        });
    }

    println!("\n════════════════════════════════════════════════════════════════\n");
}
