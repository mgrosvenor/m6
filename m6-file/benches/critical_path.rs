//! m6-file's critical path.
//!
//!   1. `serve` with an in-memory writer: route settings → disk read →
//!      response serialise.
//!   2. Full Unix socket round-trip through the same `serve_connection` loop
//!      production runs.
//!
//! `bench_route_match` was removed when m6-file became an `App` service: the
//! router it measured was m6-file's own, and there is one router now. Core's
//! is benchmarked in `m6-core/benches/critical_path.rs`.

use criterion::{black_box, criterion_group, Criterion};
use std::io::Write;
use std::os::unix::net::UnixStream;
use tempfile::TempDir;

use m6_core::http::RawRequest;
use m6_core::Request;
use m6_file_lib::handler::serve;
use serde_json::{json, Map};

const MINIMAL_HTML: &[u8] = b"<!doctype html><html><body><h1>Hello</h1></body></html>";

fn make_site() -> TempDir {
    let dir = TempDir::new().unwrap();
    let assets = dir.path().join("assets");
    std::fs::create_dir_all(&assets).unwrap();
    std::fs::write(assets.join("index.html"), MINIMAL_HTML).unwrap();
    dir
}

/// The `Request` the service loop hands the handler: route settings from
/// config, path parameters from core's router.
fn make_get_request(site_dir: &std::path::Path, relpath: &str) -> Request {
    let raw = RawRequest {
        version: "HTTP/1.1".to_string(),
        body: Vec::new(),
        method: "GET".to_string(),
        path: format!("/assets/{relpath}"),
        query: None,
        headers: vec![],
    };
    let mut dict = Map::new();
    dict.insert("relpath".to_string(), json!(relpath));
    let mut settings = Map::new();
    settings.insert("root".to_string(), json!("assets"));
    Request::new(raw, dict, site_dir.to_path_buf())
        .with_route_settings(std::sync::Arc::new(settings))
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

/// `serve` writing to an in-memory Vec — disk read each time (no cache).
fn bench_handle_request(c: &mut Criterion) {
    let dir = make_site();
    let req = make_get_request(dir.path(), "index.html");

    let mut group = c.benchmark_group("handle_request");
    group.sample_size(5_000);
    group.bench_function("handle_request", |b| {
        b.iter(|| {
            let mut buf = Vec::with_capacity(256);
            let resp = serve(black_box(&req)).unwrap();
            let mut r = m6_core::h1::Responder::new(&mut buf, req.method(), false);
            black_box(resp.send(&mut r).unwrap());
        })
    });
    group.finish();
}

/// Serve `serve` over a unix socket on a background thread, the way the
/// service loop does, and return the socket path.
fn spawn_server(dir: &std::path::Path, name: &str) -> std::path::PathBuf {
    let sock_path = dir.join(name);
    let listener = std::os::unix::net::UnixListener::bind(&sock_path).unwrap();
    let site_dir = dir.to_path_buf();

    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let mut stream = match stream { Ok(s) => s, Err(_) => break };
            stream.set_read_timeout(Some(std::time::Duration::from_secs(5))).ok();
            let site_dir = site_dir.clone();
            // The same connection loop production runs, so the round trip
            // measured here is the one that actually happens.
            let _ = m6_core::server::serve_connection(&mut stream, move |raw, resp| {
                let relpath = raw.path().trim_start_matches("/assets/").to_string();
                let mut dict = Map::new();
                dict.insert("relpath".to_string(), json!(relpath));
                let mut settings = Map::new();
                settings.insert("root".to_string(), json!("assets"));
                let req = Request::new(raw, dict, site_dir.clone())
                    .with_route_settings(std::sync::Arc::new(settings));
                let r = serve(&req).map_err(|e| std::io::Error::other(e.to_string()))?;
                r.send(resp).map_err(|e| std::io::Error::other(e.to_string()))
            });
        }
    });
    std::thread::sleep(std::time::Duration::from_millis(10));
    sock_path
}

const RAW: &[u8] =
    b"GET /assets/index.html HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";

fn round_trip(sock_path: &std::path::Path) -> usize {
    let mut conn = UnixStream::connect(sock_path).unwrap();
    conn.write_all(RAW).unwrap();
    use std::io::Read;
    let mut buf = Vec::with_capacity(512);
    conn.read_to_end(&mut buf).unwrap();
    buf.len()
}

/// Full Unix socket round-trip: HTTP/1.1 request → response.
fn bench_socket_round_trip(c: &mut Criterion) {
    let dir = make_site();
    let sock_path = spawn_server(dir.path(), "bench.sock");

    let mut group = c.benchmark_group("socket_round_trip");
    group.sample_size(5_000);
    group.bench_function("socket_round_trip", |b| {
        b.iter(|| black_box(round_trip(&sock_path)))
    });
    group.finish();
}

criterion_group!(benches, bench_handle_request, bench_socket_round_trip);

// ── Custom main ───────────────────────────────────────────────────────────────

fn main() {
    benches();

    const N_SLOW: usize = 5_000;

    println!(
        "\n════════════════════════════════════════════════════════════════\n\
        m6-file  Raw percentile report  (release mode)\n\
        ════════════════════════════════════════════════════════════════"
    );

    {
        let dir = make_site();
        let req = make_get_request(dir.path(), "index.html");
        report_percentiles("serve (disk read)", N_SLOW, || {
            let mut buf = Vec::with_capacity(256);
            let resp = serve(&req).unwrap();
            let mut r = m6_core::h1::Responder::new(&mut buf, req.method(), false);
            black_box(resp.send(&mut r).unwrap());
        });
    }

    {
        let dir = make_site();
        let sock_path = spawn_server(dir.path(), "bench2.sock");
        report_percentiles("socket round trip", N_SLOW, || {
            black_box(round_trip(&sock_path));
        });
    }
}
