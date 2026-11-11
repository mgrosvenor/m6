#![allow(
    clippy::disallowed_methods,
    reason = "test code: threads, sleeps and blocking clients drive the service from outside"
)]

use std::io::{self, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use m6_core::testkit::{assert_app_lifecycle, binary, wait, Service};

/// A running `m6-file`, its socket, and the directory both live in.
///
/// Field order is drop order: the service is killed before the directory
/// holding its socket is removed.
struct Server {
    svc: Service,
    _dir: tempfile::TempDir,
}

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
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
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream.write_all(request.as_bytes()).unwrap();

    // One response, not read-to-EOF. Backends keep the connection open now
    // (RFC 9112 9.3), so waiting for a close waits for the idle timeout.
    let method = request.split(' ').next().unwrap_or("");
    let response = m6_core::testkit::read_one(&mut stream, method).expect("read one response");
    String::from_utf8_lossy(&response).into_owned()
}

// ─── L1 Start/Stop ────────────────────────────────────────────────────────────

#[test]
fn l1_valid_config_starts_and_socket_appears() {
    let (_guard, socket_path) = spawn_server("l1-start");
    assert!(
        socket_path.exists(),
        "socket should exist at {:?}",
        socket_path
    );
}

/// A config that still asks for a thread pool is refused at start: the
/// operator hears about it with exit 2, and no socket is left behind.
#[test]
fn a_thread_pool_section_is_refused_at_start() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("site.toml"), "").unwrap();
    let config = dir.path().join("m6-file.conf");
    std::fs::write(&config, "[thread_pool]\nsize = 4\n").unwrap();
    let socket = dir.path().join("refused.sock");

    let mut svc = Service::spawn(
        "m6-file",
        Command::new(binary("m6-file"))
            .arg(dir.path())
            .arg(&config)
            .env("M6_SOCKET_OVERRIDE", &socket),
    );
    let status = svc
        .wait_for_exit(Instant::now() + Duration::from_secs(10))
        .unwrap_or_else(|| {
            panic!(
                "m6-file did not refuse [thread_pool]\n--- output ---\n{}",
                svc.output()
            )
        });
    let output = svc.output();
    assert_eq!(
        status.code(),
        Some(2),
        "m6-file should exit 2 for [thread_pool]\n--- output ---\n{output}"
    );
    assert!(
        output.contains("[thread_pool]"),
        "the error must name the table\n{output}"
    );
    assert!(
        output.contains("run more instances"),
        "the error must say what to do instead\n--- output ---\n{output}"
    );
    assert!(
        !socket.exists(),
        "a refused start must not leave a socket at {}",
        socket.display()
    );
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

    let req =
        "GET /assets/css/main.css HTTP/1.1\r\nHost: localhost\r\nAccept-Encoding: identity\r\n\r\n";
    let resp = http_request(&socket_path, req);

    assert!(
        resp.contains("200 OK"),
        "expected 200, got:\n{}",
        &resp[..resp.len().min(300)]
    );
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
    assert!(
        resp.contains("404"),
        "expected 404, got: {}",
        &resp[..resp.len().min(200)]
    );
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
fn l2_query_relpath_cannot_replace_the_path_parameter() {
    let (_guard, socket_path) = spawn_server("l2-query-relpath");

    // The path names a real file, and the query carries a `relpath` that
    // climbs out of `assets/` to the service's own config. The query key
    // must not replace the checked path parameter, so the request serves
    // the file the path names, never the config.
    let req = "GET /assets/css/main.css?relpath=../m6-file-test.conf HTTP/1.1\r\n\
               Host: localhost\r\nAccept-Encoding: identity\r\n\r\n";
    let resp = http_request(&socket_path, req);
    assert!(
        !resp.contains("[compression]"),
        "a query relpath served the service's config: {}",
        &resp[..resp.len().min(300)]
    );
    assert!(
        resp.contains("200 OK"),
        "expected the path's own file, got: {}",
        &resp[..resp.len().min(200)]
    );
}

#[test]
fn l2_query_relpath_cannot_choose_a_file_on_a_filename_route() {
    let (_guard, socket_path) = spawn_server("l2-query-filename");

    // `/content/posts/{stem}/{filename}` captures one segment and no
    // `relpath`. A query `relpath` naming a file in a subdirectory, which
    // that route's shape cannot address, must not be what is served.
    let req = "GET /content/posts/hello/index.html?relpath=drafts/secret.txt HTTP/1.1\r\n\
               Host: localhost\r\nAccept-Encoding: identity\r\n\r\n";
    let resp = http_request(&socket_path, req);
    assert!(
        !resp.contains("UNPUBLISHED-DRAFT"),
        "a query relpath chose a file the route does not address: {}",
        &resp[..resp.len().min(300)]
    );
    assert!(
        resp.contains("200 OK"),
        "expected the path's own file, got: {}",
        &resp[..resp.len().min(200)]
    );
}

#[test]
fn l2_relpath_with_subdirectory() {
    let (_guard, socket_path) = spawn_server("l2-subdir");

    let req =
        "GET /assets/css/main.css HTTP/1.1\r\nHost: localhost\r\nAccept-Encoding: identity\r\n\r\n";
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

    assert!(
        resp.contains("200 OK"),
        "expected 200, got: {}",
        &resp[..resp.len().min(200)]
    );
    assert!(
        resp.to_lowercase().contains("content-encoding: br"),
        "expected brotli encoding for CSS, response headers:\n{}",
        &resp[..resp.find("\r\n\r\n").unwrap_or(resp.len().min(500))]
    );
}

#[test]
fn l3_css_gzip_compressed() {
    let (_guard, socket_path) = spawn_server("l3-gzip");

    let req =
        "GET /assets/css/main.css HTTP/1.1\r\nHost: localhost\r\nAccept-Encoding: gzip\r\n\r\n";
    let resp = http_request(&socket_path, req);

    assert!(
        resp.contains("200 OK"),
        "expected 200, got: {}",
        &resp[..resp.len().min(200)]
    );
    assert!(
        resp.to_lowercase().contains("content-encoding: gzip"),
        "expected gzip encoding for CSS"
    );
}

#[test]
fn l3_no_compression_without_accept_encoding() {
    let (_guard, socket_path) = spawn_server("l3-no-compress");

    // Without Accept-Encoding: br/gzip, identity should be used
    let req =
        "GET /assets/css/main.css HTTP/1.1\r\nHost: localhost\r\nAccept-Encoding: identity\r\n\r\n";
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

    let req =
        "GET /assets/css/main.css HTTP/1.1\r\nHost: localhost\r\nAccept-Encoding: identity\r\n\r\n";
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
    let get_req =
        "GET /assets/css/main.css HTTP/1.1\r\nHost: localhost\r\nAccept-Encoding: identity\r\n\r\n";
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

/// A streamed body never stops the loop for another client.
///
/// The handler serves a 4 MiB identity file as a stream, so the loop writes it
/// one chunk per writable turn. This client takes a 64 KiB receive buffer and
/// reads nothing until a second request on another connection has been
/// answered, which a blocking `write_all` of the body could never allow: the
/// writer would fill 64 KiB and stop the process there.
#[test]
fn a_large_stream_body_to_a_slow_reader_does_not_block_the_process() {
    let dir = tempfile::tempdir().expect("tempdir");
    let site = dir.path();
    std::fs::create_dir_all(site.join("assets")).unwrap();
    std::fs::write(site.join("site.toml"), "").unwrap();
    // Not a text type: identity coding and no minification, so the body is the
    // file on disk and the handler streams it.
    let body: Vec<u8> = (0..4 * 1024 * 1024).map(|i| (i % 251) as u8).collect();
    std::fs::write(site.join("assets").join("big.bin"), &body).unwrap();
    std::fs::write(site.join("assets").join("a.txt"), b"asset").unwrap();
    let config = site.join("m6-file.conf");
    std::fs::write(&config, ASSETS_ROUTE).unwrap();

    let socket_path = dir.path().join("stream.sock");
    let mut svc = Service::spawn(
        "m6-file",
        Command::new(binary("m6-file"))
            .arg(site)
            .arg(&config)
            .env("M6_SOCKET_OVERRIDE", &socket_path),
    );
    svc.wait_for_path(&socket_path, Duration::from_secs(10));
    assert!(
        wait::for_unix(&socket_path, Duration::from_secs(10)),
        "m6-file never accepted a connection"
    );

    // The slow reader: a 64 KiB receive buffer, a request sent, nothing read.
    let mut slow = UnixStream::connect(&socket_path).expect("connect the slow reader");
    socket2::SockRef::from(&slow)
        .set_recv_buffer_size(64 * 1024)
        .expect("a 64 KiB receive buffer");
    slow.write_all(
        b"GET /assets/big.bin HTTP/1.1\r\nHost: localhost\r\nAccept-Encoding: identity\r\n\r\n",
    )
    .expect("send the slow reader's request");

    // Wait until its response has started arriving, which is the signal that
    // the server accepted it and is writing. `peek` reads nothing.
    slow.set_nonblocking(true)
        .expect("non-blocking slow reader");
    let writing = wait::until(Duration::from_secs(10), || {
        let mut byte = [std::mem::MaybeUninit::<u8>::uninit(); 1];
        matches!(socket2::SockRef::from(&slow).peek(&mut byte), Ok(n) if n > 0)
    });
    assert!(
        writing,
        "the 4 MiB response never started arriving\n--- output ---\n{}",
        svc.output()
    );

    // A second connection must be answered while the first one's body is still
    // held by this client, which is the whole test.
    let started = Instant::now();
    let mut probe = UnixStream::connect(&socket_path).expect("connect the probe");
    probe
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("probe timeout");
    probe
        .write_all(
            b"GET /assets/a.txt HTTP/1.1\r\nHost: localhost\r\nAccept-Encoding: identity\r\n\r\n",
        )
        .expect("send the probe's request");
    let response = m6_core::testkit::read_one(&mut probe, "GET").unwrap_or_else(|e| {
        panic!(
            "a second request was not answered while a 4 MiB body waited for its reader: \
             {e}\n--- output ---\n{}",
            svc.output()
        )
    });
    let (status, _, _) = split_response(&response);
    assert!(status.contains("200"), "the probe was not served: {status}");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "the probe took {:?}, beyond the 2 s deadline",
        started.elapsed()
    );

    // Now read the whole streamed body and check it arrived intact.
    slow.set_nonblocking(false).expect("blocking slow reader");
    slow.set_read_timeout(Some(Duration::from_secs(20)))
        .expect("slow reader timeout");
    let raw = m6_core::testkit::read_one(&mut slow, "GET").expect("the 4 MiB response");
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("the response has a header/body split");
    let (status, headers, body_len) = split_response(&raw);
    assert!(
        status.contains("200"),
        "the slow reader was not served: {status}"
    );
    assert!(
        headers.contains(&format!("content-length: {}", body.len())),
        "the response must promise the file's length, got {headers:?}"
    );
    assert_eq!(body_len, body.len(), "the streamed body was short");
    assert!(
        raw[split + 4..] == body[..],
        "the streamed body did not match the file"
    );

    svc.assert_alive("after streaming a 4 MiB body to a slow reader");
}

/// A request pipelined behind a streamed body is answered after that body,
/// not in the middle of it.
///
/// Both requests arrive in one write, so the second is already buffered when
/// the first answer starts. The first answer is a 1 MiB streamed file and the
/// client takes a 64 KiB receive buffer, so the loop's first flush blocks with
/// the body still owed. The loop must finish that body before it reads and
/// answers the second request: reading first would replace the body's reader
/// and write the second answer's head into the first body.
#[test]
fn a_request_pipelined_behind_a_streamed_body_waits_for_the_body() {
    const BIG: usize = 1024 * 1024;
    let dir = tempfile::tempdir().expect("tempdir");
    let site = dir.path();
    std::fs::create_dir_all(site.join("assets")).unwrap();
    std::fs::write(site.join("site.toml"), "").unwrap();
    let big: Vec<u8> = (0..BIG).map(|i| (i % 251) as u8).collect();
    std::fs::write(site.join("assets").join("big.bin"), &big).unwrap();
    std::fs::write(site.join("assets").join("small.txt"), b"small").unwrap();
    let config = site.join("m6-file.conf");
    std::fs::write(&config, ASSETS_ROUTE).unwrap();

    let socket_path = dir.path().join("pipeline.sock");
    let mut svc = Service::spawn(
        "m6-file",
        Command::new(binary("m6-file"))
            .arg(site)
            .arg(&config)
            .env("M6_SOCKET_OVERRIDE", &socket_path),
    );
    svc.wait_for_path(&socket_path, Duration::from_secs(10));
    assert!(
        wait::for_unix(&socket_path, Duration::from_secs(10)),
        "m6-file never accepted a connection"
    );

    let mut client = UnixStream::connect(&socket_path).expect("connect");
    socket2::SockRef::from(&client)
        .set_recv_buffer_size(64 * 1024)
        .expect("a 64 KiB receive buffer");
    client
        .set_read_timeout(Some(Duration::from_secs(20)))
        .expect("timeout");
    client
        .write_all(
            b"GET /assets/big.bin HTTP/1.1\r\nHost: localhost\r\nAccept-Encoding: identity\r\n\r\n\
              GET /assets/small.txt HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        )
        .expect("send both requests in one write");

    let first = m6_core::testkit::read_one(&mut client, "GET").expect("the first response");
    let split = first
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("the first response has a header/body split");
    let (status, headers, body_len) = split_response(&first);
    assert!(status.contains("200"), "the first response was {status}");
    assert!(
        headers.contains(&format!("content-length: {BIG}")),
        "the first response must promise the file's length, got {headers:?}"
    );
    assert_eq!(body_len, BIG, "the first body was not the whole file");
    assert!(
        first[split + 4..] == big[..],
        "the first body did not match the file"
    );

    let second = m6_core::testkit::read_one(&mut client, "GET").expect("the second response");
    let (status, _, body_len) = split_response(&second);
    assert!(status.contains("200"), "the second response was {status}");
    assert_eq!(body_len, 5, "the second body is the small file");
    assert!(
        second[second.len() - 5..] == *b"small",
        "the second body did not match the file"
    );

    svc.assert_alive("after answering a request pipelined behind a streamed body");
}

/// A streamed file holds a second descriptor, so the table is half the
/// descriptor budget. Under a hard limit of 64 the table is 24: twenty
/// connections holding a 2 MiB stream each use 46 descriptors, and the next
/// batch of connections must not reach the accept loop's descriptor limit.
/// The service stays alive, never logs an accept error, and keeps answering
/// on an existing connection. A table sized without the file descriptor
/// reaches EMFILE when the second batch arrives.
#[test]
fn a_lowered_open_file_limit_leaves_room_for_every_streamed_file() {
    const HARD: u64 = 64;
    /// The descriptors `raise_open_file_limit` leaves after its 16 reserved.
    const DESCRIPTORS: usize = HARD as usize - 16;
    /// Held streams, so the accept pass that follows runs with their file
    /// descriptors already in use.
    const HELD: usize = 20;

    let dir = tempfile::tempdir().expect("tempdir");
    let site = dir.path();
    std::fs::create_dir_all(site.join("assets")).unwrap();
    std::fs::write(site.join("site.toml"), "").unwrap();
    // Large enough that an unread body cannot fit a socket buffer, so the
    // streamed file's descriptor stays open behind its connection.
    let big: Vec<u8> = (0..2 * 1024 * 1024).map(|i| (i % 251) as u8).collect();
    std::fs::write(site.join("assets").join("big.bin"), &big).unwrap();
    std::fs::write(site.join("assets").join("a.txt"), b"asset").unwrap();
    let config = site.join("m6-file.conf");
    std::fs::write(&config, ASSETS_ROUTE).unwrap();

    let socket_path = dir.path().join("stream-limit.sock");
    let mut command = m6_core::testkit::lowered_fd_limit_command(HARD, 0, binary("m6-file"));
    command
        .arg(site)
        .arg(&config)
        .env("M6_SOCKET_OVERRIDE", &socket_path);
    let mut svc = Service::spawn("m6-file", &mut command);
    svc.wait_for_path(&socket_path, Duration::from_secs(10));
    assert!(
        wait::for_unix(&socket_path, Duration::from_secs(10)),
        "m6-file never accepted a connection"
    );

    let request =
        b"GET /assets/big.bin HTTP/1.1\r\nHost: localhost\r\nAccept-Encoding: identity\r\n\r\n";
    let mut held: Vec<UnixStream> = (0..HELD)
        .map(|_| {
            let mut client = UnixStream::connect(&socket_path).expect("connect");
            client.write_all(request).expect("send the request");
            client
        })
        .collect();

    // Every held connection must be accepted and its file open: one byte of
    // its body is the signal, and `peek` reads nothing.
    let started = wait::until(Duration::from_secs(10), || {
        held.iter().all(|client| {
            let mut byte = [std::mem::MaybeUninit::<u8>::uninit(); 1];
            matches!(socket2::SockRef::from(client).peek(&mut byte), Ok(n) if n > 0)
        })
    });
    assert!(
        started,
        "the first streams never started\n--- output ---\n{}",
        svc.output()
    );
    svc.assert_alive("with twenty streams holding their files");

    // A batch that cannot fit beside the held streams. With the whole
    // descriptor budget the accept loop reaches EMFILE here.
    let mut more: Vec<UnixStream> = (0..DESCRIPTORS)
        .map(|_| {
            let mut client = UnixStream::connect(&socket_path).expect("connect");
            client.write_all(request).expect("send the request");
            client
        })
        .collect();

    // The loop keeps serving: finish one held body, then ask again on the
    // same connection. A loop spinning in a failed accept answers neither,
    // and a loop that exits answers nothing at all.
    let mut first = held.remove(0);
    first.set_nonblocking(false).expect("blocking");
    first
        .set_read_timeout(Some(Duration::from_secs(20)))
        .expect("timeout");
    let body = m6_core::testkit::read_one(&mut first, "GET").expect("the held streamed body");
    let (status, headers, body_len) = split_response(&body);
    assert!(status.contains("200"), "the held response was {status}");
    assert!(
        headers.contains(&format!("content-length: {}", big.len())),
        "the response must promise the file's length, got {headers:?}"
    );
    assert_eq!(body_len, big.len(), "the streamed body was short");

    first
        .write_all(
            b"GET /assets/a.txt HTTP/1.1\r\nHost: localhost\r\nAccept-Encoding: identity\r\n\r\n",
        )
        .expect("send the second request");
    let second = m6_core::testkit::read_one(&mut first, "GET")
        .expect("the loop did not answer on an existing connection");
    let (status, _, body_len) = split_response(&second);
    assert!(status.contains("200"), "the second response was {status}");
    assert_eq!(body_len, 5, "the second body is the small file");

    svc.assert_alive("after answering on an existing connection");
    assert!(
        !svc.output().contains("accept error"),
        "the descriptor budget was not enough for a stream per connection\n--- output ---\n{}",
        svc.output()
    );
    more.clear();
}

/// The accept arm is reached and survives when inherited descriptors, not the
/// table, exhaust the limit.
///
/// The launcher opens 40 descriptors before the exec, so a hard limit of 64
/// leaves only a few free although the table is 24. Clients are connected one
/// at a time: after each one the test waits for that client's first response
/// byte or for an accept error before connecting the next, and stops at the
/// first error. That discovers the free descriptor count instead of assuming
/// it, and the failed pass is the last listener event. The arm must log exactly
/// one line, not repeat it over a stability window, keep the service alive and
/// leave an accepted connection answered.
#[test]
fn a_descriptor_shortage_from_outside_the_table_sheds_one_accept_pass() {
    const HARD: u64 = 64;
    /// Opened by the launcher before the exec. stdio, the listener, the signal
    /// descriptor and the poll take about 46, so accept runs out well before
    /// the 24-slot table fills.
    const INHERITED: usize = 40;
    /// Enough for any plausible free count. Reaching it means the inherited
    /// descriptors did not bring the service near its limit.
    const MAX_CLIENTS: usize = 40;

    let dir = tempfile::tempdir().expect("tempdir");
    let site = dir.path();
    std::fs::create_dir_all(site.join("assets")).unwrap();
    std::fs::write(site.join("site.toml"), "").unwrap();
    // Larger than any socket buffer, so a served request keeps its file
    // descriptor until the client reads.
    let big: Vec<u8> = (0..1024 * 1024).map(|i| (i % 251) as u8).collect();
    std::fs::write(site.join("assets").join("big.bin"), &big).unwrap();
    let config = site.join("m6-file.conf");
    std::fs::write(&config, ASSETS_ROUTE).unwrap();

    let socket_path = dir.path().join("pressure.sock");
    let mut command =
        m6_core::testkit::lowered_fd_limit_command(HARD, INHERITED, binary("m6-file"));
    command
        .arg(site)
        .arg(&config)
        .env("M6_SOCKET_OVERRIDE", &socket_path);
    let mut svc = Service::spawn("m6-file", &mut command);
    svc.wait_for_path(&socket_path, Duration::from_secs(10));
    assert!(
        wait::for_unix(&socket_path, Duration::from_secs(10)),
        "m6-file never accepted a connection"
    );

    fn accept_errors(output: &str) -> usize {
        output.matches("accept error").count()
    }
    fn answered(client: &UnixStream) -> bool {
        let mut byte = [std::mem::MaybeUninit::<u8>::uninit(); 1];
        matches!(socket2::SockRef::from(client).peek(&mut byte), Ok(n) if n > 0)
    }

    let mut clients: Vec<UnixStream> = Vec::new();
    loop {
        assert!(
            clients.len() < MAX_CLIENTS,
            "{} inherited descriptors never brought accept to its limit at the table's ceiling\n\
             --- output ---\n{}",
            INHERITED,
            svc.output()
        );
        let mut client = UnixStream::connect(&socket_path).expect("connect");
        client.set_nonblocking(true).expect("non-blocking client");
        let sent = client.write_all(
            b"GET /assets/big.bin HTTP/1.1\r\nHost: localhost\r\nAccept-Encoding: identity\r\n\r\n",
        );
        match sent {
            Ok(()) => {}
            // The kernel aborted the connection because its accept failed for
            // want of a descriptor. The accept line is the outcome; wait for
            // it below.
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::BrokenPipe | io::ErrorKind::ConnectionReset
                ) => {}
            Err(e) => panic!(
                "connection {} could not send its request: {e}\n--- output ---\n{}",
                clients.len(),
                svc.output()
            ),
        }
        // Wait for this client's outcome before the next connection exists,
        // so the failed pass is the last listener event.
        let settled = wait::until(Duration::from_secs(10), || {
            accept_errors(&svc.output()) >= 1 || answered(&client)
        });
        assert!(
            settled,
            "connection {} was neither answered nor refused within 10 s\n--- output ---\n{}",
            clients.len(),
            svc.output()
        );
        clients.push(client);
        if accept_errors(&svc.output()) >= 1 {
            break;
        }
    }

    assert_eq!(
        accept_errors(&svc.output()),
        1,
        "the accept arm logged one line per pass, not one\n--- output ---\n{}",
        svc.output()
    );

    // No growth over a stability window: no connection arrives after the
    // failed pass, so no listener event can start another one.
    assert!(
        !wait::until(Duration::from_millis(750), || accept_errors(&svc.output())
            > 1),
        "the accept error repeated\n--- output ---\n{}",
        svc.output()
    );
    svc.assert_alive("after an accept error from inherited descriptors");

    // An accepted connection is still answered. The streamed file may or may
    // not open, depending on how many connections were accepted before the
    // limit; either status proves the loop came back and served it.
    let mut first = clients.remove(0);
    first.set_nonblocking(false).expect("blocking client");
    first
        .set_read_timeout(Some(Duration::from_secs(20)))
        .expect("timeout");
    let response = m6_core::testkit::read_one(&mut first, "GET")
        .expect("an accepted connection was not answered");
    let status = String::from_utf8_lossy(&response[..response.len().min(32)]).to_string();
    assert!(
        status.contains("200") || status.contains("404"),
        "the accepted connection was answered with {status}"
    );

    assert_eq!(
        accept_errors(&svc.output()),
        1,
        "a second accept-error line appeared\n--- output ---\n{}",
        svc.output()
    );
    svc.assert_alive("after answering on an accepted connection");
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
    let status = |resp: &str| -> String { resp.lines().next().unwrap_or("").trim().to_string() };

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
        status(&get(
            "If-Unmodified-Since: Thu, 01 Jan 1970 00:00:00 GMT\r\n"
        ))
        .contains("412"),
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
    assert!(
        status(&get("If-None-Match: *\r\n")).contains("304"),
        "If-None-Match: *"
    );
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

// ─── HEAD answers what GET would, however the answer is produced ─────────────

/// Send a raw request and return the response as bytes.
///
/// Bytes, not a `String`: a brotli body is not UTF-8, and `from_utf8_lossy`
/// replaces each invalid byte with a three-byte U+FFFD, so measuring a
/// compressed body through a `String` reported 135 bytes for 79. That is a
/// measurement bug that looks exactly like a `Content-Length` bug.
fn http_request_bytes(socket_path: &Path, request: &str) -> Vec<u8> {
    let mut stream = UnixStream::connect(socket_path)
        .unwrap_or_else(|e| panic!("connect to {:?}: {}", socket_path, e));
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream.write_all(request.as_bytes()).unwrap();
    let method = request.split(' ').next().unwrap_or("");
    m6_core::testkit::read_one(&mut stream, method).expect("read one response")
}

/// Split a raw response into its status line, its headers as sorted
/// `name: value` pairs (lowercased names), and its body length in bytes.
fn split_response(raw: &[u8]) -> (String, Vec<String>, usize) {
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("response has a header/body split");
    let head = std::str::from_utf8(&raw[..split]).expect("headers are ASCII");
    let body_len = raw.len() - (split + 4);

    let mut lines = head.lines();
    let status = lines.next().unwrap_or("").to_string();
    let mut headers: Vec<String> = lines
        .filter_map(|l| l.split_once(": "))
        .map(|(k, v)| format!("{}: {}", k.to_ascii_lowercase(), v))
        .collect();
    headers.sort();
    (status, headers, body_len)
}

/// A HEAD must carry exactly the headers the matching GET carries, and its
/// `Content-Length` must be the length of the body that GET actually returns.
///
/// m6-file now answers a HEAD without touching the file when the
/// representation *is* the file: identity coding, minification off for the
/// type. Everything else still reads, minifies and compresses, because a
/// `Content-Length` that does not describe the GET body is worse than a slow
/// HEAD. This walks all three shapes together so the fast path cannot drift
/// away from the slow one.
#[test]
fn head_reports_exactly_what_get_would() {
    let (_guard, socket_path) = spawn_server("head-eq-get");

    // (path, Accept-Encoding, which path it should take, why)
    let cases: [(&str, &str, &str); 3] = [
        // Not compressed, not minified: the fast path.
        (
            "/assets/images/photo.txt",
            "identity",
            "identity, unminified",
        ),
        // Minified, so the representation is not the bytes on disk.
        ("/assets/css/style.css", "identity", "identity but minified"),
        // Compressed, so the length is only known after compressing.
        ("/assets/css/style.css", "br", "brotli"),
    ];

    for (path, encoding, what) in cases {
        let req = |method: &str| {
            http_request_bytes(
                &socket_path,
                &format!(
                    "{method} {path} HTTP/1.1\r\nHost: localhost\r\n\
                     Accept-Encoding: {encoding}\r\n\r\n"
                ),
            )
        };

        let (get_status, get_headers, get_body_len) = split_response(&req("GET"));
        let (head_status, head_headers, head_body_len) = split_response(&req("HEAD"));

        assert!(
            get_status.contains("200"),
            "{what}: GET should be 200, got {get_status}"
        );
        assert_eq!(head_status, get_status, "{what}: status line must match");
        assert_eq!(
            head_headers, get_headers,
            "{what}: HEAD and GET must carry identical headers"
        );
        assert_eq!(head_body_len, 0, "{what}: a HEAD must carry no body");

        // The header the fast path is most likely to get wrong: it reports a
        // length it computed from metadata rather than from the bytes sent.
        let declared: usize = get_headers
            .iter()
            .find_map(|h| h.strip_prefix("content-length: "))
            .unwrap_or_else(|| panic!("{what}: no content-length"))
            .trim()
            .parse()
            .unwrap();
        assert_eq!(
            declared, get_body_len,
            "{what}: content-length must be the number of body bytes GET sends"
        );
    }
}

/// The fast path must actually skip the read, not merely produce the right
/// answer by a slower route.
///
/// Correctness and the saving are separate properties, and
/// `head_reports_exactly_what_get_would` only covers the first: a refactor that
/// quietly restored the `fs::read` would keep every one of those assertions
/// green. So this proves the negative directly, by making the file impossible
/// to read and requiring the HEAD to succeed anyway. `stat` needs directory
/// traversal, not read permission, so the metadata the answer is built from is
/// still available.
///
/// The GET beside it is the control. If it also succeeded, the file would be
/// readable and this test would be proving nothing.
#[test]
fn a_head_on_an_unreadable_file_still_answers() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().expect("tempdir");
    let site = dir.path();
    std::fs::create_dir_all(site.join("assets")).unwrap();
    std::fs::write(site.join("site.toml"), "").unwrap();

    // Not a text type: no minification, no compression, so this is the shape
    // the fast path is for.
    let body = vec![b'x'; 4096];
    let file = site.join("assets").join("opaque.bin");
    std::fs::write(&file, &body).unwrap();

    let config = site.join("m6-file.conf");
    std::fs::write(
        &config,
        "[[route]]\npath = \"/assets/{*relpath}\"\nhandler = \"files\"\nroot = \"assets/\"\n",
    )
    .unwrap();

    let socket_path = dir.path().join("unreadable.sock");
    let mut svc = Service::spawn(
        "m6-file",
        Command::new(binary("m6-file"))
            .arg(site)
            .arg(&config)
            .env("M6_SOCKET_OVERRIDE", &socket_path),
    );
    svc.wait_for_path(&socket_path, Duration::from_secs(10));
    assert!(
        wait::for_unix(&socket_path, Duration::from_secs(10)),
        "never accepted"
    );

    // Readable first, so the fixture is known good before it is broken.
    let req = |method: &str| {
        http_request_bytes(
            &socket_path,
            &format!(
                "{method} /assets/opaque.bin HTTP/1.1\r\nHost: localhost\r\n\
                 Accept-Encoding: identity\r\n\r\n"
            ),
        )
    };
    let (status, _, _) = split_response(&req("GET"));
    assert!(
        status.contains("200"),
        "the fixture should serve before chmod, got {status}"
    );

    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o000)).unwrap();

    // Root ignores file permission bits, and the Linux build host runs the
    // suite as root, so there the file is still perfectly readable and this
    // test can prove nothing. Skip rather than assert something untrue: a
    // green tick that depends on the uid of whoever ran it is worse than an
    // absent one. The proof still runs on every developer machine.
    if std::fs::read(&file).is_ok() {
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).ok();
        eprintln!(
            "skipping: this process can read a 0000 file, so it is running as root \
             and cannot demonstrate that the read was skipped"
        );
        return;
    }

    let (head_status, head_headers, _) = split_response(&req("HEAD"));
    assert!(
        head_status.contains("200"),
        "a HEAD must not need to read the file, got {head_status}\n--- output ---\n{}",
        svc.output()
    );
    assert!(
        head_headers.contains(&format!("content-length: {}", body.len())),
        "content-length should be the file size from metadata, got {head_headers:?}"
    );

    // The control: the same request as a GET genuinely cannot be served.
    let (get_status, _, _) = split_response(&req("GET"));
    assert!(
        get_status.contains("404"),
        "the file must really be unreadable or this test proves nothing, got {get_status}"
    );

    // Restore before the tempdir is removed.
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).ok();
    svc.assert_alive("after serving an unreadable file's metadata");
}

/// A path that resolves to a directory is a 404, and a HEAD says so too.
///
/// This is a regression guard with a short and embarrassing history. The HEAD
/// fast path answers from `std::fs::metadata`, which succeeds on a directory
/// and reports its size, so the first version of it answered
/// `HEAD /assets/css` with `200` and `Content-Length: 128` while the GET beside
/// it answered 404. The `fs::read` that the fast path skips was also what had
/// been rejecting non-files, by failing.
///
/// `head_reports_exactly_what_get_would` did not catch it because every path it
/// asks for is a file. A method-equivalence test only covers the inputs it is
/// given.
#[test]
fn a_directory_is_404_for_both_methods() {
    let (_guard, socket_path) = spawn_server("dir-404");

    for path in ["/assets/css", "/assets/images"] {
        for method in ["GET", "HEAD"] {
            let raw = http_request_bytes(
                &socket_path,
                &format!(
                    "{method} {path} HTTP/1.1\r\nHost: localhost\r\n\
                     Accept-Encoding: identity\r\n\r\n"
                ),
            );
            let (status, headers, _) = split_response(&raw);
            assert!(
                status.contains("404"),
                "{method} {path} is a directory and must be 404, got {status} {headers:?}"
            );
        }
    }
}

// ─── Lifecycle ─────────────────────────────────────────────────────────────────

const ASSETS_ROUTE: &str =
    "[[route]]\npath = \"/assets/{*relpath}\"\nhandler = \"files\"\nroot = \"assets/\"\n";

fn site_with_two_trees() -> tempfile::TempDir {
    let site = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(site.path().join("assets")).unwrap();
    std::fs::create_dir_all(site.path().join("files/deep")).unwrap();
    std::fs::write(site.path().join("assets/a.txt"), b"asset").unwrap();
    std::fs::write(site.path().join("files/deep/report.txt"), b"report").unwrap();
    std::fs::write(site.path().join("site.toml"), "").unwrap();
    site
}

fn write_config(path: &Path, routes: &str) {
    std::fs::write(path, routes).unwrap();
}

/// The lifecycle contract, which m6-file gets from the same
/// `assert_app_lifecycle` every service uses rather than from its own
/// hand-written copy.
#[test]
fn lifecycle() {
    let site = site_with_two_trees();
    let config = site.path().join("m6-file.conf");
    write_config(&config, ASSETS_ROUTE);
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("lifecycle.sock");

    assert_app_lifecycle(
        "m6-file",
        Command::new(binary("m6-file"))
            .arg(site.path())
            .arg(&config)
            .env("M6_SOCKET_OVERRIDE", &socket),
        &socket,
    );
}
