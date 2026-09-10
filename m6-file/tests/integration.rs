use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use m6_core::testkit::{binary, wait, Service};

/// A running `m6-file`, its socket, and the directory both live in.
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
    fixtures_dir().join("m6-file-test.conf")
}

/// Spawn `m6-file` on a socket of its own and wait until it is answering.
///
/// **The socket lives in a fresh temp directory, not a shared one.** This used
/// to be `$TMPDIR/m6-sockets/<id>.sock`, a fixed path per test, and
/// `spawn_server` deleted whatever was already there before starting. Two runs
/// of this suite at once, or one run overlapping a killed one, meant a test
/// unlinking a socket another live server was serving on. The failure surfaced
/// somewhere else entirely, as a connect error in an unrelated test.
///
/// **Readiness is a successful connect, not an existing path.** The old loop
/// polled `socket_path.exists()` and then carried on regardless of the answer,
/// so a slow start became `connect: Connection refused` at the first request.
/// `bind` creates the socket file, but the server does not accept until it has
/// called `listen`, so the file appearing proves nothing.
fn spawn_server(id: &str) -> (Server, PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    // Short name: a unix socket path is capped near 104 bytes on macOS, and
    // the temp directory already spends most of that.
    let socket_path = dir.path().join(format!("{id}.sock"));

    let mut svc = Service::spawn(
        "m6-file",
        Command::new(binary("m6-file"))
            .arg(fixtures_dir())
            .arg(config_path())
            .env("M6_SOCKET_OVERRIDE", &socket_path),
    );
    svc.wait_for_path(&socket_path, Duration::from_secs(10));
    assert!(
        wait::for_unix(&socket_path, Duration::from_secs(10)),
        "m6-file created {} but never accepted a connection",
        socket_path.display()
    );

    (Server { svc, _dir: dir }, socket_path)
}

/// Send a raw HTTP request over a Unix socket and return the full response.
fn http_request(socket_path: &Path, request: &str) -> String {
    let mut stream = UnixStream::connect(socket_path)
        .unwrap_or_else(|e| panic!("connect to {:?}: {}", socket_path, e));
    stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    stream.write_all(request.as_bytes()).unwrap();

    let mut response = Vec::new();
    let _ = stream.read_to_end(&mut response);
    String::from_utf8_lossy(&response).into_owned()
}

// ─── L1 Start/Stop ────────────────────────────────────────────────────────────

#[test]
fn l1_valid_config_starts_and_socket_appears() {
    let (_guard, socket_path) = spawn_server("l1-start");
    assert!(socket_path.exists(), "socket should exist at {:?}", socket_path);
}

#[test]
fn l1_sigterm_exits_zero() {
    let (guard, socket_path) = spawn_server("l1-sigterm");
    assert!(socket_path.exists(), "socket should appear");

    // The test is named for the exit status, so assert on it. The previous
    // version signalled, slept, and dropped the guard without ever looking at
    // what the process did, which passed whatever happened.
    let mut guard = guard;
    let status = guard.svc.terminate(Duration::from_secs(5));
    assert!(
        status.success(),
        "m6-file should exit 0 on SIGTERM, got {status}\n--- stderr ---\n{}",
        guard.svc.stderr_text()
    );
}

// ─── L2 Path Resolution ───────────────────────────────────────────────────────

#[test]
fn l2_existing_file_correct_bytes_and_content_type() {
    let (_guard, socket_path) = spawn_server("l2-existing");

    let req = "GET /assets/css/main.css HTTP/1.1\r\nHost: localhost\r\nAccept-Encoding: identity\r\n\r\n";
    let resp = http_request(&socket_path, req);

    assert!(resp.contains("200 OK"), "expected 200, got:\n{}", &resp[..resp.len().min(300)]);
    assert!(
        resp.to_lowercase().contains("content-type: text/css"),
        "expected text/css content type, headers:\n{}",
        &resp[..resp.find("\r\n\r\n").unwrap_or(resp.len().min(500))]
    );
    // Check that some CSS content is in the body
    assert!(
        resp.contains("body") || resp.contains("font-family"),
        "expected CSS content in body"
    );
}

#[test]
fn l2_nonexistent_file_404() {
    let (_guard, socket_path) = spawn_server("l2-404");

    let req = "GET /assets/css/nonexistent.css HTTP/1.1\r\nHost: localhost\r\n\r\n";
    let resp = http_request(&socket_path, req);
    assert!(resp.contains("404"), "expected 404, got: {}", &resp[..resp.len().min(200)]);
}

#[test]
fn l2_dotdot_in_url_returns_404() {
    let (_guard, socket_path) = spawn_server("l2-traversal");

    // `../` traversal in URL — spec (impl-plan §l2): "../ in URL → 404"
    let req = "GET /assets/../m6-file-test.conf HTTP/1.1\r\nHost: localhost\r\n\r\n";
    let resp = http_request(&socket_path, req);
    assert!(
        resp.contains("404"),
        "expected 404 for traversal attempt, got: {}",
        &resp[..resp.len().min(200)]
    );
}

#[test]
fn l2_relpath_with_subdirectory() {
    let (_guard, socket_path) = spawn_server("l2-subdir");

    let req = "GET /assets/css/main.css HTTP/1.1\r\nHost: localhost\r\nAccept-Encoding: identity\r\n\r\n";
    let resp = http_request(&socket_path, req);
    assert!(
        resp.contains("200 OK"),
        "expected 200 for css/main.css, got: {}",
        &resp[..resp.len().min(200)]
    );
}

#[test]
fn l2_symlink_outside_root_returns_404() {
    // Create a symlink in fixtures that points outside the fixtures dir
    let fixtures = fixtures_dir();
    let link_path = fixtures.join("assets").join("css").join("evil-link.css");
    let _ = std::fs::remove_file(&link_path);
    std::os::unix::fs::symlink("/etc/hosts", &link_path).ok();

    let (_guard, socket_path) = spawn_server("l2-symlink");

    let req = "GET /assets/css/evil-link.css HTTP/1.1\r\nHost: localhost\r\nAccept-Encoding: identity\r\n\r\n";
    let resp = http_request(&socket_path, req);

    // Cleanup the symlink
    let _ = std::fs::remove_file(&link_path);

    assert!(
        resp.contains("404"),
        "symlink outside root should return 404, got: {}",
        &resp[..resp.len().min(200)]
    );
}

// ─── L3 Compression ───────────────────────────────────────────────────────────

#[test]
fn l3_css_brotli_compressed() {
    let (_guard, socket_path) = spawn_server("l3-brotli");

    let req = "GET /assets/css/main.css HTTP/1.1\r\nHost: localhost\r\nAccept-Encoding: br\r\n\r\n";
    let resp = http_request(&socket_path, req);

    assert!(resp.contains("200 OK"), "expected 200, got: {}", &resp[..resp.len().min(200)]);
    assert!(
        resp.to_lowercase().contains("content-encoding: br"),
        "expected brotli encoding for CSS, response headers:\n{}",
        &resp[..resp.find("\r\n\r\n").unwrap_or(resp.len().min(500))]
    );
}

#[test]
fn l3_css_gzip_compressed() {
    let (_guard, socket_path) = spawn_server("l3-gzip");

    let req = "GET /assets/css/main.css HTTP/1.1\r\nHost: localhost\r\nAccept-Encoding: gzip\r\n\r\n";
    let resp = http_request(&socket_path, req);

    assert!(resp.contains("200 OK"), "expected 200, got: {}", &resp[..resp.len().min(200)]);
    assert!(
        resp.to_lowercase().contains("content-encoding: gzip"),
        "expected gzip encoding for CSS"
    );
}

#[test]
fn l3_no_compression_without_accept_encoding() {
    let (_guard, socket_path) = spawn_server("l3-no-compress");

    // Without Accept-Encoding: br/gzip, identity should be used
    let req = "GET /assets/css/main.css HTTP/1.1\r\nHost: localhost\r\nAccept-Encoding: identity\r\n\r\n";
    let resp = http_request(&socket_path, req);
    assert!(resp.contains("200 OK"), "expected 200");
    assert!(
        !resp.to_lowercase().contains("content-encoding:"),
        "should not have content-encoding with Accept-Encoding: identity"
    );
}

// ─── L4 Cache-Control ─────────────────────────────────────────────────────────

#[test]
fn l4_cache_control_public() {
    let (_guard, socket_path) = spawn_server("l4-cache");

    let req = "GET /assets/css/main.css HTTP/1.1\r\nHost: localhost\r\nAccept-Encoding: identity\r\n\r\n";
    let resp = http_request(&socket_path, req);

    assert!(resp.contains("200 OK"), "expected 200");
    assert!(
        resp.to_lowercase().contains("cache-control: public"),
        "expected Cache-Control: public header, headers:\n{}",
        &resp[..resp.find("\r\n\r\n").unwrap_or(resp.len().min(500))]
    );
}

// ─── L4b Error codes ──────────────────────────────────────────────────────────

#[test]
fn l4b_method_not_allowed_405() {
    let (_guard, socket_path) = spawn_server("l4b-405");

    let req = "POST /assets/css/main.css HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\n\r\n";
    let resp = http_request(&socket_path, req);
    assert!(
        resp.contains("405"),
        "expected 405 for POST, got: {}",
        &resp[..resp.len().min(200)]
    );
}

#[test]
fn l4b_dotdot_in_relpath_returns_404() {
    let (_guard, socket_path) = spawn_server("l4b-traversal");

    // /assets/{relpath} where relpath contains `..` — spec: "../ in URL → 404"
    let req = "GET /assets/css/../main.css HTTP/1.1\r\nHost: localhost\r\n\r\n";
    let resp = http_request(&socket_path, req);
    assert!(
        resp.contains("404"),
        "expected 404 for relpath with .., got: {}",
        &resp[..resp.len().min(200)]
    );
}

#[test]
fn l4b_head_returns_correct_content_length() {
    let (_guard, socket_path) = spawn_server("l4b-head");

    // First get the body length via GET.
    let get_req = "GET /assets/css/main.css HTTP/1.1\r\nHost: localhost\r\nAccept-Encoding: identity\r\n\r\n";
    let get_resp = http_request(&socket_path, get_req);

    // Extract Content-Length from GET response.
    let get_cl = get_resp
        .lines()
        .find(|l| l.to_lowercase().starts_with("content-length:"))
        .and_then(|l| l.split(':').nth(1))
        .and_then(|v| v.trim().parse::<usize>().ok())
        .expect("GET response must include Content-Length");

    // Now HEAD — Content-Length must match GET's.
    let head_req = "HEAD /assets/css/main.css HTTP/1.1\r\nHost: localhost\r\nAccept-Encoding: identity\r\n\r\n";
    let head_resp = http_request(&socket_path, head_req);

    let head_cl = head_resp
        .lines()
        .find(|l| l.to_lowercase().starts_with("content-length:"))
        .and_then(|l| l.split(':').nth(1))
        .and_then(|v| v.trim().parse::<usize>().ok())
        .expect("HEAD response must include Content-Length");

    assert_eq!(
        get_cl, head_cl,
        "HEAD Content-Length ({}) must equal GET Content-Length ({})",
        head_cl, get_cl
    );
}

// ─── L5 Integration ───────────────────────────────────────────────────────────

#[test]
fn l5_concurrent_requests() {
    let (_guard, socket_path) = spawn_server("l5-concurrent");

    let mut handles = Vec::new();

    for i in 0..100 {
        let sp = socket_path.clone();
        let handle = std::thread::spawn(move || {
            let req = format!(
                "GET /assets/css/main.css HTTP/1.1\r\nHost: localhost\r\nX-Request-Id: {}\r\nAccept-Encoding: identity\r\n\r\n",
                i
            );
            let resp = http_request(&sp, &req);
            assert!(
                resp.contains("200 OK"),
                "request {} failed: {}",
                i,
                &resp[..resp.len().min(100)]
            );
        });
        handles.push(handle);
    }

    for handle in handles {
        handle.join().unwrap();
    }
}
