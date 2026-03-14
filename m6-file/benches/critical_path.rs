/// Critical-path benchmarks for m6-file.
///
/// Two tiers:
///   1. Route matching (no I/O)
///   2. handle_request with in-memory writer (route + disk read + response serialise)
///   3. Unix socket round-trip (full HTTP/1.1 request ↔ response, as m6-http sees it)
///
/// Run with: cargo bench -p m6-file
use criterion::{black_box, criterion_group, Criterion};
use std::io::Write;
use std::os::unix::net::UnixStream;
use tempfile::TempDir;

use m6_file_lib::config::{Config, RouteConfig};
use m6_file_lib::handler::{handle_request, HandlerContext};
use m6_file_lib::http::Request;
use m6_file_lib::route::{sort_routes, Route};

// ── Fixtures ──────────────────────────────────────────────────────────────────

const MINIMAL_HTML: &[u8] = b"<!doctype html><html><body><h1>Hello</h1></body></html>";

fn make_site() -> (TempDir, Config, Vec<Route>) {
    let dir = TempDir::new().unwrap();
    let assets = dir.path().join("assets");
    std::fs::create_dir_all(&assets).unwrap();
    std::fs::write(assets.join("index.html"), MINIMAL_HTML).unwrap();

    let config = Config {
        route: vec![RouteConfig {
            path: "/assets/{relpath}".to_string(),
            root: "assets".to_string(),
            tail: None,
        }],
        compression: Default::default(),
        thread_pool: None,
        log: None,
    };

    let mut routes: Vec<Route> = config.route.iter().map(Route::from_config).collect();
    sort_routes(&mut routes);
    (dir, config, routes)
}

fn make_get_request(path: &str) -> Request {
    Request {
        method: "GET".to_string(),
        path: path.to_string(),
        query: String::new(),
        headers: vec![],
    }
}

// ── Percentile reporter ────────────────────────────────────────────────────────

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
    let variance = samples.iter().map(|&x| { let d = x as f64 - avg; d * d }).sum::<f64>() / count;
    let stddev = variance.sqrt();
    println!(
        "\n── {label} (n={n}) ─────────────────────────────────────────────\n\
         p0={p0}ns  p1={p1}ns  p50={p50}ns  p99={p99}ns  p100={p100}ns\n\
         avg={avg:.1}ns  stddev={stddev:.1}ns",
        p0 = p(0.0), p1 = p(1.0), p50 = p(50.0), p99 = p(99.0), p100 = p(100.0),
    );
}

// ── Benchmarks ────────────────────────────────────────────────────────────────

fn bench_route_match(c: &mut Criterion) {
    let (_dir, _config, routes) = make_site();
    let mut group = c.benchmark_group("route_match");
    group.sample_size(5_000);
    group.bench_function("route_match", |b| {
        b.iter(|| {
            for route in &routes {
                black_box(route.match_path(black_box("/assets/index.html")));
            }
        })
    });
    group.finish();
}

/// handle_request writing to an in-memory Vec — disk read each time (no cache).
fn bench_handle_request(c: &mut Criterion) {
    let (dir, config, routes) = make_site();
    let req = make_get_request("/assets/index.html");

    let mut group = c.benchmark_group("handle_request");
    group.sample_size(5_000);
    group.bench_function("handle_request", |b| {
        b.iter(|| {
            let ctx = HandlerContext { routes: &routes, config: &config, site_dir: dir.path() };
            let mut buf = Vec::with_capacity(256);
            black_box(handle_request(black_box(&req), &ctx, &mut buf).unwrap());
        })
    });
    group.finish();
}

/// Full Unix socket round-trip: HTTP/1.1 request → response.
fn bench_socket_round_trip(c: &mut Criterion) {
    let dir = TempDir::new().unwrap();
    std::fs::create_dir_all(dir.path().join("assets")).unwrap();
    std::fs::write(dir.path().join("assets/index.html"), MINIMAL_HTML).unwrap();

    let sock_path = dir.path().join("bench.sock");
    let listener = std::os::unix::net::UnixListener::bind(&sock_path).unwrap();
    let dir_path = dir.path().to_path_buf();

    std::thread::spawn(move || {
        let config = Config {
            route: vec![RouteConfig {
                path: "/assets/{relpath}".to_string(),
                root: "assets".to_string(),
                tail: None,
            }],
            compression: Default::default(),
            thread_pool: None,
            log: None,
        };
        let mut routes: Vec<Route> = config.route.iter().map(Route::from_config).collect();
        sort_routes(&mut routes);

        for stream in listener.incoming() {
            let mut stream = match stream { Ok(s) => s, Err(_) => break };
            stream.set_read_timeout(Some(std::time::Duration::from_secs(5))).ok();
            let req = match Request::read(stream.try_clone().unwrap()) {
                Ok(r) => r,
                Err(_) => continue,
            };
            let ctx = HandlerContext { routes: &routes, config: &config, site_dir: &dir_path };
            handle_request(&req, &ctx, &mut stream).ok();
        }
    });
    std::thread::sleep(std::time::Duration::from_millis(10));

    const RAW: &[u8] =
        b"GET /assets/index.html HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";

    let mut group = c.benchmark_group("socket_round_trip");
    group.sample_size(5_000);
    group.bench_function("socket_round_trip", |b| {
        b.iter(|| {
            let mut conn = UnixStream::connect(&sock_path).unwrap();
            conn.write_all(RAW).unwrap();
            use std::io::Read;
            let mut buf = Vec::with_capacity(512);
            conn.read_to_end(&mut buf).unwrap();
            black_box(buf.len())
        })
    });
    group.finish();
}

criterion_group!(benches, bench_route_match, bench_handle_request, bench_socket_round_trip);

// ── Custom main ───────────────────────────────────────────────────────────────

fn main() {
    benches();

    const N: usize = 100_000;
    const N_SLOW: usize = 5_000;

    println!(
        "\n════════════════════════════════════════════════════════════════\n\
        m6-file  Raw percentile report  (release mode)\n\
        ════════════════════════════════════════════════════════════════"
    );

    {
        let (_dir, _config, routes) = make_site();
        report_percentiles("route_match", N, || {
            for route in &routes {
                black_box(route.match_path(black_box("/assets/index.html")));
            }
        });
    }

    {
        let (dir, config, routes) = make_site();
        let req = make_get_request("/assets/index.html");
        report_percentiles("handle_request (disk read)", N_SLOW, || {
            let ctx = HandlerContext { routes: &routes, config: &config, site_dir: dir.path() };
            let mut buf = Vec::with_capacity(256);
            black_box(handle_request(&req, &ctx, &mut buf).unwrap());
        });
    }

    {
        let dir2 = TempDir::new().unwrap();
        std::fs::create_dir_all(dir2.path().join("assets")).unwrap();
        std::fs::write(dir2.path().join("assets/index.html"), MINIMAL_HTML).unwrap();
        let sock_path2 = dir2.path().join("bench2.sock");
        let listener2 = std::os::unix::net::UnixListener::bind(&sock_path2).unwrap();
        let dir2_path = dir2.path().to_path_buf();

        std::thread::spawn(move || {
            let config = Config {
                route: vec![RouteConfig {
                    path: "/assets/{relpath}".to_string(),
                    root: "assets".to_string(),
                    tail: None,
                }],
                compression: Default::default(),
                thread_pool: None,
                log: None,
            };
            let mut routes: Vec<Route> = config.route.iter().map(Route::from_config).collect();
            sort_routes(&mut routes);
            for stream in listener2.incoming() {
                let mut stream = match stream { Ok(s) => s, Err(_) => break };
                stream.set_read_timeout(Some(std::time::Duration::from_secs(5))).ok();
                let req = match Request::read(stream.try_clone().unwrap()) {
                    Ok(r) => r,
                    Err(_) => continue,
                };
                let ctx = HandlerContext { routes: &routes, config: &config, site_dir: &dir2_path };
                handle_request(&req, &ctx, &mut stream).ok();
            }
        });
        std::thread::sleep(std::time::Duration::from_millis(10));

        const RAW: &[u8] =
            b"GET /assets/index.html HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
        report_percentiles("socket_round_trip (end-to-end)", N_SLOW, || {
            let mut conn = UnixStream::connect(&sock_path2).unwrap();
            conn.write_all(RAW).unwrap();
            use std::io::Read;
            let mut buf = Vec::with_capacity(512);
            conn.read_to_end(&mut buf).unwrap();
            black_box(buf.len());
        });
    }

    println!("\n════════════════════════════════════════════════════════════════\n");
}
