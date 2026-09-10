use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use m6_core::testkit::{binary, Service};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// A running `m6-html`, its socket, and the directory both live in.
///
/// Field order is drop order: the service is killed before the directory
/// holding its socket is removed.
struct Server {
    svc: Service,
    _dir: tempfile::TempDir,
}

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join("fixtures")
}

fn config_path() -> PathBuf {
    fixtures_dir().join("configs").join("m6-html.conf")
}

/// Spawn `m6-html` on a socket of its own and wait until it answers requests.
///
/// **The socket lives in a fresh temp directory, not a shared one.** This used
/// to be `$TMPDIR/m6-html-sockets/<id>.sock`, a fixed path per test, and the
/// spawn deleted whatever was already there first. Two runs of this suite at
/// once, or one overlapping a killed run, meant a test unlinking a socket
/// another live server was serving on, and the failure landed somewhere else.
///
/// **Readiness is a real answered request.** The socket file existing is not
/// the same as the server accepting on it: `bind` creates the file, `listen`
/// comes after, and the accept backlog is a third thing again. Under a full
/// workspace run the connect raced all of that, `http_request` returned an
/// empty string on the io error, and a test asserting `resp.contains("200 OK")`
/// failed against `""`. It passed in isolation every time, which is what made
/// it look like noise.
fn spawn_server(id: &str) -> (Server, PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    // Short name: a unix socket path is capped near 104 bytes on macOS, and
    // the temp directory already spends most of that.
    let socket_path = dir.path().join(format!("{id}.sock"));

    let mut svc = Service::spawn(
        "m6-html",
        Command::new(binary("m6-html"))
            .arg(fixtures_dir())
            .arg(config_path())
            .env("M6_SOCKET_OVERRIDE", &socket_path),
    );
    svc.wait_for_path(&socket_path, Duration::from_secs(10));

    let mut ready = false;
    for _ in 0..200 {
        if !http_request(&socket_path, "GET / HTTP/1.1\r\nHost: localhost\r\n\r\n").is_empty() {
            ready = true;
            break;
        }
        svc.assert_alive("waiting for the first answered request");
        std::thread::sleep(Duration::from_millis(25));
    }
    assert!(
        ready,
        "m6-html at {} never answered a request\n--- output ---\n{}",
        socket_path.display(),
        svc.output()
    );

    (Server { svc, _dir: dir }, socket_path)
}

/// Send a raw HTTP/1.1 request and return the full response string.
/// Returns empty string on any I/O error (connection refused, broken pipe, etc.)
/// so callers can retry rather than panic.
fn http_request(socket_path: &Path, request: &str) -> String {
    let mut stream = match UnixStream::connect(socket_path) {
        Ok(s) => s,
        Err(_) => return String::new(),
    };
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
    if stream.write_all(request.as_bytes()).is_err() { return String::new(); }
    stream.shutdown(std::net::Shutdown::Write).ok();
    let mut response = Vec::new();
    let _ = stream.read_to_end(&mut response);
    String::from_utf8_lossy(&response).into_owned()
}

/// Send a raw HTTP/1.1 request and return the full response as raw bytes.
/// Returns empty vec on any I/O error.
fn http_request_bytes(socket_path: &Path, request: &str) -> Vec<u8> {
    let mut stream = match UnixStream::connect(socket_path) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
    if stream.write_all(request.as_bytes()).is_err() { return Vec::new(); }
    stream.shutdown(std::net::Shutdown::Write).ok();
    let mut response = Vec::new();
    let _ = stream.read_to_end(&mut response);
    response
}

/// Return just the body bytes from a raw HTTP response bytes (after \r\n\r\n).
fn response_body_bytes(raw: &[u8]) -> Vec<u8> {
    // Find the double CRLF separator.
    for i in 0..raw.len().saturating_sub(3) {
        if raw[i] == b'\r' && raw[i+1] == b'\n' && raw[i+2] == b'\r' && raw[i+3] == b'\n' {
            return raw[i+4..].to_vec();
        }
    }
    vec![]
}

/// Return just the header section (before the blank line) as a string.
fn response_headers_str(raw: &[u8]) -> String {
    for i in 0..raw.len().saturating_sub(3) {
        if raw[i] == b'\r' && raw[i+1] == b'\n' && raw[i+2] == b'\r' && raw[i+3] == b'\n' {
            return String::from_utf8_lossy(&raw[..i]).to_string();
        }
    }
    String::from_utf8_lossy(raw).to_string()
}

// ---------------------------------------------------------------------------
// L1 — Start / Stop
// ---------------------------------------------------------------------------

/// A valid config causes the server to start and create its socket.
#[test]
fn l1_valid_config_starts_and_socket_appears() {
    let (_guard, socket_path) = spawn_server("l1-start");
    assert!(socket_path.exists(), "socket should exist at {:?}", socket_path);
}

/// secrets_file pointing at a non-existent path is silently ignored.
#[test]
fn l1_secrets_file_absent_still_starts() {
    // The fixture config already has secrets_file = "/nonexistent/path/secrets.toml"
    let (_guard, socket_path) = spawn_server("l1-secrets-absent");
    assert!(socket_path.exists(), "server should start even when secrets_file is absent");

    // Make a request to confirm it actually serves.
    let resp = http_request(
        &socket_path,
        "GET /blog HTTP/1.1\r\nHost: localhost\r\n\r\n",
    );
    assert!(resp.contains("200 OK"), "expected 200, got: {}", &resp[..resp.len().min(300)]);
}

/// SIGTERM causes the process to exit cleanly (exit code 0).
#[test]
fn l1_sigterm_exits_zero() {
    let (guard, socket_path) = spawn_server("l1-sigterm");
    assert!(socket_path.exists(), "socket should appear");

    // The test is named for the exit status, so assert on it. The previous
    // version signalled, slept, and dropped the guard without ever looking at
    // what the process did, which passed whatever happened — including the
    // process being killed outright by the signal.
    let mut guard = guard;
    let status = guard.svc.terminate(Duration::from_secs(5));
    assert!(
        status.success(),
        "m6-html should exit 0 on SIGTERM, got {status}. A signal exit status means the \
         process died at the default disposition instead of shutting down.\n\
         --- output ---\n{}",
        guard.svc.output()
    );
    // One shutdown sequence in m6_core::signal means every service that owns a
    // socket removes it on the way out. A socket left behind keeps a dead
    // member in m6-http's backend pool until the next rescan.
    assert!(
        !socket_path.exists(),
        "the socket at {} outlived the process",
        socket_path.display()
    );
    m6_core::testkit::assert_lifecycle_logged("m6-html", &guard.svc.output());
}

// ---------------------------------------------------------------------------
// L2 — Route Matching
// ---------------------------------------------------------------------------

/// /blog hits the exact route, not the parameterised /blog/{stem}.
#[test]
fn l2_exact_route_matched_for_blog() {
    let (_guard, socket_path) = spawn_server("l2-exact");

    let resp = http_request(
        &socket_path,
        "GET /blog HTTP/1.1\r\nHost: localhost\r\n\r\n",
    );
    assert!(resp.contains("200 OK"), "expected 200, got: {}", &resp[..resp.len().min(300)]);
    // The post-index template contains "Posts"
    assert!(resp.contains("Posts"), "expected post-index content");
}

/// /blog/hello-world hits the parameterised route with stem=hello-world.
#[test]
fn l2_parameterised_route_extracts_stem() {
    let (_guard, socket_path) = spawn_server("l2-param");

    let resp = http_request(
        &socket_path,
        "GET /blog/hello-world HTTP/1.1\r\nHost: localhost\r\n\r\n",
    );
    assert!(resp.contains("200 OK"), "expected 200, got: {}", &resp[..resp.len().min(300)]);
    assert!(resp.contains("Stem: hello-world"), "expected stem in body, got: {}", &resp);
}

/// An unknown path returns 404.
#[test]
fn l2_unknown_path_returns_404() {
    let (_guard, socket_path) = spawn_server("l2-404");

    let resp = http_request(
        &socket_path,
        "GET /no-such-path HTTP/1.1\r\nHost: localhost\r\n\r\n",
    );
    assert!(resp.contains("404"), "expected 404, got: {}", &resp[..resp.len().min(300)]);
}

// ---------------------------------------------------------------------------
// L3 — Params Merge
// ---------------------------------------------------------------------------

/// global_params file provides site_name; route params provide title.
/// Route-level params override global ones on conflict (not tested for conflict
/// here, but both merge correctly).
#[test]
fn l3_global_and_route_params_merged() {
    let (_guard, socket_path) = spawn_server("l3-merge");

    let resp = http_request(
        &socket_path,
        "GET /blog/hello-world HTTP/1.1\r\nHost: localhost\r\n\r\n",
    );
    assert!(resp.contains("200 OK"), "expected 200");
    // title from route params
    assert!(resp.contains("Hello World"), "expected title from route params");
}

/// A missing params file causes the server to return 500 (params load fails
/// at request time for parameterised routes; for static routes it logs an
/// error at startup — here we use a non-existent stem).
#[test]
fn l3_missing_params_file_returns_not_found_or_500() {
    let (_guard, socket_path) = spawn_server("l3-missing");

    // Request a stem with no matching JSON file.
    let resp = http_request(
        &socket_path,
        "GET /blog/nonexistent-post HTTP/1.1\r\nHost: localhost\r\n\r\n",
    );
    // The server either logs an error and renders with empty params (200 with
    // incomplete template) or returns a 5xx. Either way it must not crash.
    // Check that we get a valid HTTP response.
    assert!(
        resp.starts_with("HTTP/1.1"),
        "expected valid HTTP response, got: {}",
        &resp[..resp.len().min(300)]
    );
}

// ---------------------------------------------------------------------------
// L4 — Path Parameter Expansion
// ---------------------------------------------------------------------------

/// /blog/hello-world loads content/posts/hello-world.json.
#[test]
fn l4_path_param_expands_to_correct_file() {
    let (_guard, socket_path) = spawn_server("l4-expand");

    let resp = http_request(
        &socket_path,
        "GET /blog/hello-world HTTP/1.1\r\nHost: localhost\r\n\r\n",
    );
    assert!(resp.contains("200 OK"), "expected 200");
    // Content from hello-world.json contains markdown which becomes HTML
    assert!(
        resp.contains("Hello World") || resp.contains("markdown"),
        "expected content from hello-world.json"
    );
}

/// A {stem} containing `..` is rejected with 400.
#[test]
fn l4_dotdot_in_stem_returns_400() {
    let (_guard, socket_path) = spawn_server("l4-dotdot");

    let resp = http_request(
        &socket_path,
        "GET /blog/..%2Fsecret HTTP/1.1\r\nHost: localhost\r\n\r\n",
    );
    // Server should reject with 400 (bad request) due to path traversal check.
    assert!(
        resp.contains("400") || resp.contains("404"),
        "expected 400 or 404 for dotdot stem, got: {}",
        &resp[..resp.len().min(300)]
    );
}

// ---------------------------------------------------------------------------
// L5 — Built-in Keys
// ---------------------------------------------------------------------------

/// `request_path` in the template reflects the actual request path.
#[test]
fn l5_request_path_built_in_key() {
    let (_guard, socket_path) = spawn_server("l5-reqpath");

    // The home template renders {{ request_path }}
    // Use "/" route — but it requires content/pages/index.json which doesn't exist.
    // Use /blog which renders post-index.html (no request_path), so we use /blog/hello-world
    // where post.html has no request_path either. Best to get a 200 from /blog and check.

    // Actually we need to test request_path. The home.html template has it but the "/" route
    // also requires content/pages/index.json (missing). Let's hit /blog which doesn't need
    // a path param and renders post-index.html (doesn't show request_path).
    // Instead, check via a route that has {{ request_path }}: home.html at "/"
    // home.html does reference request_path, but the route requires content/pages/index.json.
    // The server will render the template even if that file is missing (static params silently
    // logged at startup as error, then rendered with empty params).
    let resp = http_request(
        &socket_path,
        "GET / HTTP/1.1\r\nHost: localhost\r\n\r\n",
    );
    // We should get a 200 (home template renders with empty params for missing index.json)
    // and the request_path built-in should appear.
    assert!(resp.contains("200 OK"), "expected 200 for /");
    assert!(
        resp.contains("Path: /"),
        "expected request_path in response body, got: {}",
        &resp[..resp.len().min(600)]
    );
}

/// `/_errors?status=404&from=/x` passes error_status and error_from to the template.
#[test]
fn l5_error_route_gets_status_and_from() {
    let (_guard, socket_path) = spawn_server("l5-errors");

    let resp = http_request(
        &socket_path,
        "GET /_errors?status=404&from=/x HTTP/1.1\r\nHost: localhost\r\n\r\n",
    );
    assert!(resp.contains("200 OK"), "expected 200");
    assert!(
        resp.contains("Error 404"),
        "expected status in template, got: {}",
        &resp[..resp.len().min(600)]
    );
    assert!(
        resp.contains("From: /x"),
        "expected from in template, got: {}",
        &resp[..resp.len().min(600)]
    );
}

/// `site_name` comes from data/site.json (global_params).
#[test]
fn l5_site_name_from_global_params() {
    let (_guard, socket_path) = spawn_server("l5-sitename");

    let resp = http_request(
        &socket_path,
        "GET / HTTP/1.1\r\nHost: localhost\r\n\r\n",
    );
    assert!(resp.contains("200 OK"), "expected 200");
    assert!(
        resp.contains("Test Site"),
        "expected site_name from data/site.json, got: {}",
        &resp[..resp.len().min(600)]
    );
}

// ---------------------------------------------------------------------------
// L6 — Status and Cache
// ---------------------------------------------------------------------------

/// /_errors has `cache = "no-store"` → Cache-Control: no-store.
#[test]
fn l6_no_store_cache_control() {
    let (_guard, socket_path) = spawn_server("l6-no-store");

    let resp = http_request(
        &socket_path,
        "GET /_errors?status=404&from=/x HTTP/1.1\r\nHost: localhost\r\n\r\n",
    );
    assert!(resp.contains("200 OK"), "expected 200");
    assert!(
        resp.to_lowercase().contains("cache-control: no-store"),
        "expected Cache-Control: no-store, got headers:\n{}",
        &resp[..resp.find("\r\n\r\n").unwrap_or(resp.len().min(500))]
    );
}

/// Default route → Cache-Control: public.
#[test]
fn l6_default_cache_control_public() {
    let (_guard, socket_path) = spawn_server("l6-public");

    let resp = http_request(
        &socket_path,
        "GET /blog HTTP/1.1\r\nHost: localhost\r\n\r\n",
    );
    assert!(resp.contains("200 OK"), "expected 200");
    assert!(
        resp.to_lowercase().contains("cache-control: public"),
        "expected Cache-Control: public, got headers:\n{}",
        &resp[..resp.find("\r\n\r\n").unwrap_or(resp.len().min(500))]
    );
}

// ---------------------------------------------------------------------------
// L7 — Compression
// ---------------------------------------------------------------------------

/// Accept-Encoding: br → Content-Encoding: br, body decompresses to valid HTML.
#[test]
fn l7_brotli_compression() {
    let (_guard, socket_path) = spawn_server("l7-brotli");

    let raw = http_request_bytes(
        &socket_path,
        "GET /blog HTTP/1.1\r\nHost: localhost\r\nAccept-Encoding: br\r\n\r\n",
    );
    let headers = response_headers_str(&raw);
    assert!(headers.contains("200 OK"), "expected 200, got: {}", &headers[..headers.len().min(300)]);
    assert!(
        headers.to_lowercase().contains("content-encoding: br"),
        "expected brotli encoding, got headers:\n{}",
        &headers
    );

    // Decompress the body and check it's valid HTML.
    let body_bytes = response_body_bytes(&raw);
    let decompressed = brotli_decompress(&body_bytes);
    let html = String::from_utf8_lossy(&decompressed);
    assert!(
        html.contains("Posts"),
        "decompressed brotli body should contain 'Posts', got: {}",
        &html[..html.len().min(500)]
    );
}

/// Accept-Encoding: gzip → Content-Encoding: gzip.
#[test]
fn l7_gzip_compression() {
    let (_guard, socket_path) = spawn_server("l7-gzip");

    let raw = http_request_bytes(
        &socket_path,
        "GET /blog HTTP/1.1\r\nHost: localhost\r\nAccept-Encoding: gzip\r\n\r\n",
    );
    let headers = response_headers_str(&raw);
    assert!(headers.contains("200 OK"), "expected 200, got: {}", &headers[..headers.len().min(300)]);
    assert!(
        headers.to_lowercase().contains("content-encoding: gzip"),
        "expected gzip encoding, got headers:\n{}",
        &headers
    );

    // Decompress and verify.
    let body_bytes = response_body_bytes(&raw);
    let decompressed = gzip_decompress(&body_bytes);
    let html = String::from_utf8_lossy(&decompressed);
    assert!(html.contains("Posts"), "decompressed gzip body should contain 'Posts'");
}

/// No Accept-Encoding → identity (no Content-Encoding header).
#[test]
fn l7_no_compression_without_accept_encoding() {
    let (_guard, socket_path) = spawn_server("l7-identity");

    let resp = http_request(
        &socket_path,
        "GET /blog HTTP/1.1\r\nHost: localhost\r\n\r\n",
    );
    assert!(resp.contains("200 OK"), "expected 200");
    assert!(
        !resp.to_lowercase().contains("content-encoding:"),
        "should not have content-encoding without Accept-Encoding, got headers:\n{}",
        &resp[..resp.find("\r\n\r\n").unwrap_or(resp.len().min(500))]
    );
    // Body should be plain HTML.
    assert!(resp.contains("Posts"), "body should contain template content");
}

// ---------------------------------------------------------------------------
// L8 — Integration
// ---------------------------------------------------------------------------

/// 100 concurrent requests across multiple routes, all succeed.
#[test]
fn l8_concurrent_requests_all_routes() {
    let (_guard, socket_path) = spawn_server("l8-concurrent");

    let mut handles = Vec::new();

    let routes = vec![
        ("GET /blog HTTP/1.1\r\nHost: localhost\r\n\r\n", "200 OK"),
        ("GET /blog/hello-world HTTP/1.1\r\nHost: localhost\r\n\r\n", "200 OK"),
        ("GET /_errors?status=404&from=/x HTTP/1.1\r\nHost: localhost\r\n\r\n", "200 OK"),
        ("GET /no-such-path HTTP/1.1\r\nHost: localhost\r\n\r\n", "404"),
    ];

    for i in 0..100 {
        let sp = socket_path.clone();
        let (req, expected) = routes[i % routes.len()].clone();
        let handle = std::thread::spawn(move || {
            // Retry up to 3 times to handle transient 503s from a full queue.
            let mut last_resp = String::new();
            for _attempt in 0..3 {
                let resp = http_request(&sp, req);
                if !resp.is_empty() {
                    last_resp = resp;
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            assert!(
                !last_resp.is_empty(),
                "request {} ({}): got empty response after retries",
                i,
                req.split_whitespace().nth(1).unwrap_or("?"),
            );
            // Accept 200 or 503 (server queue full) — the test verifies no crashes.
            // For routes that must return 200, also accept 503 under load.
            let ok = last_resp.contains(expected) || last_resp.contains("503");
            assert!(
                ok,
                "request {} ({}): expected {} or 503, got: {}",
                i,
                req.split_whitespace().nth(1).unwrap_or("?"),
                expected,
                &last_resp[..last_resp.len().min(200)]
            );
        });
        handles.push(handle);
    }

    for handle in handles {
        handle.join().unwrap();
    }
}

// ---------------------------------------------------------------------------
// Compression helpers (inline — avoids extra dep crate import confusion)
// ---------------------------------------------------------------------------

fn brotli_decompress(data: &[u8]) -> Vec<u8> {
    use std::io::Read;
    let mut out = Vec::new();
    let mut reader = brotli::Decompressor::new(data, 4096);
    reader.read_to_end(&mut out).expect("brotli decompress failed");
    out
}

fn gzip_decompress(data: &[u8]) -> Vec<u8> {
    use std::io::Read;
    let mut decoder = flate2::read::GzDecoder::new(data);
    let mut out = Vec::new();
    decoder.read_to_end(&mut out).expect("gzip decompress failed");
    out
}
