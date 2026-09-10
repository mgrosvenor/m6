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
        "m6-file should exit 0 on SIGTERM, got {status}. A signal exit status means the \
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
    m6_core::testkit::assert_lifecycle_logged("m6-file", &guard.svc.output());
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

// ─── Conditional requests (RFC 9110 13.2.2) ───────────────────────────────────

/// All four precondition steps, in order, against a real m6-file.
///
/// This is the guard for the defect that moved preconditions into
/// `m6_core::conditional`. What was here did steps 3 and 4 only, and step 3
/// with strong comparison:
///
/// ```ignore
/// inm == "*" || inm.split(',').any(|tag| tag.trim() == etag)
/// ```
///
/// Byte equality is *strong* comparison; `If-None-Match` requires weak
/// (RFC 9110 8.8.3.2). A client returning the validator it had been given as
/// `W/"..."` was sent the whole body again, and `If-Match` and
/// `If-Unmodified-Since` were not consulted at all.
///
/// **These assertions go against the process, not the function.** The function
/// was already unit-tested and already correct, in a different crate; what was
/// wrong was what m6-file did on the wire. A unit test could not have caught
/// that, and did not.
#[test]
fn conditional_requests_follow_rfc9110_precedence() {
    let (_guard, socket_path) = spawn_server("preconditions");

    let get = |extra: &str| -> String {
        http_request(
            &socket_path,
            &format!(
                "GET /assets/css/main.css HTTP/1.1\r\nHost: localhost\r\n\
                 Accept-Encoding: identity\r\n{extra}\r\n"
            ),
        )
    };
    let status = |resp: &str| -> String {
        resp.lines().next().unwrap_or("").trim().to_string()
    };

    let etag = get("")
        .lines()
        .find(|l| l.to_ascii_lowercase().starts_with("etag:"))
        .and_then(|l| l.split_once(':'))
        .map(|(_, v)| v.trim().to_string())
        .expect("m6-file must send an ETag");

    // No preconditions.
    assert!(status(&get("")).contains("200"), "unconditional GET");

    // 1. If-Match uses STRONG comparison; a mismatch is 412, not 200.
    assert!(
        status(&get("If-Match: \"nope\"\r\n")).contains("412"),
        "If-Match with a non-matching tag must be 412"
    );
    assert!(
        status(&get(&format!("If-Match: {etag}\r\n"))).contains("200"),
        "If-Match with the current tag must proceed"
    );

    // 2. If-Unmodified-Since, only consulted when If-Match is absent.
    assert!(
        status(&get("If-Unmodified-Since: Thu, 01 Jan 1970 00:00:00 GMT\r\n")).contains("412"),
        "If-Unmodified-Since in the past must be 412"
    );

    // 3. If-None-Match uses WEAK comparison: W/"x" matches "x".
    assert!(
        status(&get(&format!("If-None-Match: {etag}\r\n"))).contains("304"),
        "strong form of the current tag must be 304"
    );
    assert!(
        status(&get(&format!("If-None-Match: W/{etag}\r\n"))).contains("304"),
        "WEAK form of the current tag must also be 304 (RFC 9110 8.8.3.2) — \
         this is the assertion that was failing"
    );
    assert!(status(&get("If-None-Match: *\r\n")).contains("304"), "If-None-Match: *");
    assert!(
        status(&get("If-None-Match: \"nope\"\r\n")).contains("200"),
        "a non-matching If-None-Match must proceed"
    );

    // 4. If-Modified-Since, only when If-None-Match is absent.
    assert!(
        status(&get("If-Modified-Since: Thu, 23 Aug 2096 20:51:20 GMT\r\n")).contains("304"),
        "a future If-Modified-Since must be 304"
    );
    assert!(
        status(&get("If-Modified-Since: Thu, 01 Jan 1970 00:00:00 GMT\r\n")).contains("200"),
        "an ancient If-Modified-Since must proceed"
    );

    // RFC 9110 13.1.4: an unparseable date is ignored, not a failure. The
    // weekday here does not match the calendar date, which is what a strict
    // parser rejects.
    assert!(
        status(&get("If-Modified-Since: Sun, 06 Nov 2099 08:49:37 GMT\r\n")).contains("200"),
        "an unparseable date must be ignored, not turned into 412"
    );
}
