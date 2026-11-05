//! End-to-end tests for `[errors] mode = "custom"` with a socket backend.
//!
//! A custom error page is fetched from the route that matches `[errors].path`.
//! On a node whose backends are Unix sockets, that fetch goes through the
//! synchronous socket path in `apply_error_mode` (`m6-http/src/main.rs`), the
//! one reachable through `forward_to_backend`. The async URL path is already
//! exercised by `custom_error_page_fetch_is_logged_to_analytics` in
//! `analytics_e2e.rs`; this file is the socket half, which had no coverage.
//!
//! Each test stands up a real `m6-http` in front of a fake socket backend and
//! asserts the three behaviours the issue names:
//!
//! 1. a backend 404 and a backend 500 each return the fetched error page body
//!    with the original status;
//! 2. a failing error-page fetch falls back to the internal page once, with no
//!    second fetch;
//! 3. a request for the error path itself does not fetch itself.
//!
//! The fake backend records every path it is asked for, so "no second fetch"
//! and "no self-fetch" are asserted on what the backend actually received, not
//! inferred from a response shape.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rustls::StreamOwned;

use m6_core::testkit::{binary, claim_port, PortClaim, Service};

/// The configured custom error path, and what the fixture serves for it.
const ERROR_PATH: &str = "/errors";
/// The site's error page body, returned by the fixture for the error path.
const ERROR_PAGE_BODY: &str = "SITE ERROR PAGE BODY";
/// Raw bodies the fake backend returns for its own failures, which the custom
/// error page must replace.
const RAW_404: &str = "RAW BACKEND 404";
const RAW_500: &str = "RAW BACKEND 500";

// ── TLS plumbing ─────────────────────────────────────────────────────────────

fn generate_cert() -> (String, String, Vec<u8>) {
    let cert =
        rcgen::generate_simple_self_signed(vec!["localhost".to_string(), "127.0.0.1".to_string()])
            .expect("self-signed cert");
    (
        cert.cert.pem(),
        cert.key_pair.serialize_pem(),
        cert.cert.der().to_vec(),
    )
}

fn trusting_client(cert_der: &[u8]) -> Arc<rustls::ClientConfig> {
    let mut roots = rustls::RootCertStore::empty();
    roots
        .add(rustls::pki_types::CertificateDer::from(cert_der.to_vec()))
        .expect("trust the run's own cert");
    Arc::new(
        rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth(),
    )
}

struct Reply {
    status: u16,
    headers: String,
    body: Vec<u8>,
}

/// A response that never arrived. Transport failures funnel here instead of
/// panicking so a readiness loop can retry, and a final assertion can print the
/// reason. `status` is 0, which the readiness loop reads as "not yet".
fn no_response(reason: impl std::fmt::Display) -> Reply {
    Reply {
        status: 0,
        headers: format!("<no response: {reason}>"),
        body: Vec::new(),
    }
}

/// One HTTP/1.1-over-TLS GET. Skips a leading 1xx block (m6 sends 103 Early
/// Hints ahead of some responses) and returns the final status and body. Never
/// panics on a transport failure; see [`no_response`].
fn https_get(port: u16, path: &str, tls: Arc<rustls::ClientConfig>) -> Reply {
    let tcp = match TcpStream::connect(format!("127.0.0.1:{port}")) {
        Ok(t) => t,
        Err(e) => return no_response(format!("tcp connect: {e}")),
    };
    if let Err(e) = tcp.set_read_timeout(Some(Duration::from_secs(10))) {
        return no_response(format!("set_read_timeout: {e}"));
    }
    let name = rustls::pki_types::ServerName::try_from("127.0.0.1".to_string()).unwrap();
    let conn = match rustls::ClientConnection::new(tls, name) {
        Ok(c) => c,
        Err(e) => return no_response(format!("tls setup: {e}")),
    };
    let mut stream = StreamOwned::new(conn, tcp);

    let req = format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n");
    if let Err(e) = stream.write_all(req.as_bytes()) {
        return no_response(format!("write request: {e}"));
    }
    if let Err(e) = stream.flush() {
        return no_response(format!("flush request: {e}"));
    }

    let mut raw = Vec::new();
    match stream.read_to_end(&mut raw) {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {}
        Err(e) => return no_response(format!("tls read: {e}")),
    }

    // Walk the header blocks; the body begins after the final one. A 103 Early
    // Hints block is informational and a later block carries the real status.
    let mut status = 0u16;
    let mut body_start = raw.len();
    let mut cursor = 0usize;
    while let Some(off) = raw[cursor..].windows(4).position(|w| w == b"\r\n\r\n") {
        let block = &raw[cursor..cursor + off];
        let code = block
            .split(|&b| b == b'\n')
            .next()
            .and_then(|line| {
                let line = std::str::from_utf8(line).ok()?;
                line.split_whitespace().nth(1)?.parse().ok()
            })
            .unwrap_or(0);
        cursor += off + 4;
        status = code;
        body_start = cursor;
        if !(100..200).contains(&code) {
            break;
        }
    }
    let headers = String::from_utf8_lossy(&raw[..body_start]).into_owned();
    let body = raw[body_start..].to_vec();
    Reply {
        status,
        headers,
        body,
    }
}

// ── The fake socket backend ──────────────────────────────────────────────────

/// A fake Unix-socket backend that answers by request path and records every
/// path it is asked for, so a test can assert on what was (and was not)
/// fetched. `n` sockets are bound; two make a buggy retry observable, because a
/// failed member is marked down and a retry then lands on the second member.
struct Fixture {
    requests: Arc<Mutex<Vec<String>>>,
    sockets: Vec<PathBuf>,
}

impl Fixture {
    fn start(prefix: &str, n: usize, fail_error_page: bool) -> Fixture {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let mut sockets = Vec::with_capacity(n);

        for i in 0..n {
            let sock = PathBuf::from(format!("/tmp/{prefix}-{i}.sock"));
            let _ = std::fs::remove_file(&sock);
            let listener = UnixListener::bind(&sock).expect("bind fixture socket");
            let requests = Arc::clone(&requests);
            // The accept threads run for the test process's lifetime, the same
            // as the fake URL backend in analytics_e2e.rs.
            std::thread::spawn(move || {
                for stream in listener.incoming().flatten() {
                    let requests = Arc::clone(&requests);
                    std::thread::spawn(move || {
                        serve_connection(stream, fail_error_page, requests);
                    });
                }
            });
            sockets.push(sock);
        }

        Fixture { requests, sockets }
    }

    fn count(&self, path: &str) -> usize {
        self.requests
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter(|p| p.as_str() == path)
            .count()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // The socket files are removed; the accept threads end with the test
        // process. Each test uses a fresh prefix, so no two tests share a path.
        for sock in &self.sockets {
            let _ = std::fs::remove_file(sock);
        }
    }
}

/// Read the request line and answer. A request for the error path with
/// `fail_error_page` set closes the connection without a response, which is how
/// a test makes the error-page fetch fail.
fn serve_connection(
    mut stream: UnixStream,
    fail_error_page: bool,
    requests: Arc<Mutex<Vec<String>>>,
) {
    let mut buf = [0u8; 4096];
    let mut n = 0usize;
    loop {
        match stream.read(&mut buf[n..]) {
            Ok(0) => break,
            Ok(k) => {
                n += k;
                if buf[..n].windows(2).any(|w| w == b"\r\n") || n >= buf.len() {
                    break;
                }
            }
            Err(_) => break,
        }
    }

    let path = request_path(&buf[..n]);
    requests
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(path.clone());

    if path == ERROR_PATH && fail_error_page {
        // No response: the proxy sees a closed connection and the fetch fails.
        return;
    }

    let (status, reason, body) = match path.as_str() {
        "/ok" => (200u16, "OK", "ok"),
        "/missing" => (404u16, "Not Found", RAW_404),
        "/broken" => (500u16, "Internal Server Error", RAW_500),
        ERROR_PATH => (200u16, "OK", ERROR_PAGE_BODY),
        _ => (404u16, "Not Found", RAW_404),
    };
    let resp = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(resp.as_bytes());
    let _ = stream.flush();
}

fn request_path(head: &[u8]) -> String {
    let line = head.split(|&b| b == b'\n').next().unwrap_or_default();
    let line = String::from_utf8_lossy(line);
    let mut parts = line.split_whitespace();
    let _method = parts.next();
    parts
        .next()
        .unwrap_or_default()
        .split('?')
        .next()
        .unwrap_or_default()
        .to_string()
}

// ── The running stack ────────────────────────────────────────────────────────

struct Stack {
    _edge: Service,
    fixture: Fixture,
    port: u16,
    tls: Arc<rustls::ClientConfig>,
    _claim: PortClaim,
    _tmp: tempfile::TempDir,
}

impl Stack {
    fn start(tag: &str, fail_error_page: bool) -> Stack {
        rustls::crypto::ring::default_provider()
            .install_default()
            .ok();

        let tmp = tempfile::Builder::new().prefix("m6ce-").tempdir().unwrap();
        let base = tmp.path();

        // A short name under /tmp: a Unix socket path is capped at 104 bytes on
        // macOS, and a tempdir under /var/folders spends most of it.
        static N: AtomicUsize = AtomicUsize::new(0);
        let n = N.fetch_add(1, Ordering::SeqCst);
        let prefix = format!("m6ce-{}-{tag}-{n}", std::process::id());

        // Two members: the second makes a buggy retry observable in the
        // fallback test, and is inert for the others.
        let fixture = Fixture::start(&prefix, 2, fail_error_page);
        let glob = format!("/tmp/{prefix}-*.sock");

        let claim = claim_port();
        let port = claim.port();

        let (cert_pem, key_pem, cert_der) = generate_cert();
        let cert_f = base.join("cert.pem");
        let key_f = base.join("key.pem");
        std::fs::write(&cert_f, &cert_pem).unwrap();
        std::fs::write(&key_f, &key_pem).unwrap();

        let site = base.join("site");
        std::fs::create_dir_all(&site).unwrap();
        std::fs::write(
            site.join("site.toml"),
            format!(
                r#"
[site]
name   = "custom-error-e2e"
domain = "localhost"

[log]
level  = "warn"
format = "text"

[errors]
mode = "custom"
path = "{ERROR_PATH}"

[analytics]
enabled = false

[rate_limit]
enabled = false

[[backend]]
name    = "fixture"
sockets = "{glob}"

[[route]]
path    = "/ok"
backend = "fixture"

[[route]]
path    = "/missing"
backend = "fixture"

[[route]]
path    = "/broken"
backend = "fixture"

[[route]]
path    = "{ERROR_PATH}"
backend = "fixture"
"#
            ),
        )
        .unwrap();

        let sys = base.join("system.toml");
        std::fs::write(
            &sys,
            format!(
                "[server]\nbind     = \"127.0.0.1:{port}\"\ntls_cert = \"{}\"\ntls_key  = \"{}\"\n\n[node]\nname = \"test-node\"\n",
                cert_f.display(),
                key_f.display()
            ),
        )
        .unwrap();

        let edge = Service::spawn(
            "m6-http",
            Command::new(binary("m6-http")).args([
                site.to_str().unwrap(),
                sys.to_str().unwrap(),
                "--log-level",
                "warn",
            ]),
        );

        let tls = trusting_client(&cert_der);

        // Wait for a real request to route: a bound port is not enough, because
        // the backend pool must be populated. With an empty pool /ok answers
        // 502; once the fixture's socket is in the pool it answers 200. /ok is
        // a plain 200 that does not run the error machinery, so the probe does
        // not pollute the counts the test asserts on.
        let ready = m6_core::testkit::wait::until(Duration::from_secs(30), || {
            https_get(port, "/ok", Arc::clone(&tls)).status == 200
        });
        assert!(
            ready,
            "m6-http never served /ok as 200 (backend pool not ready) within 30s; \
             edge output:\n{}",
            edge.output()
        );

        Stack {
            _edge: edge,
            fixture,
            port,
            tls,
            _claim: claim,
            _tmp: tmp,
        }
    }

    fn get(&self, path: &str) -> Reply {
        https_get(self.port, path, Arc::clone(&self.tls))
    }

    fn backend_count(&self, path: &str) -> usize {
        self.fixture.count(path)
    }
}

// ── The scenarios ────────────────────────────────────────────────────────────

/// A backend 404 must be replaced by the fetched error page, not the backend's
/// raw 404 body, while the status stays 404. A regression here would serve the
/// backend's own error text to a visitor.
#[test]
fn backend_404_returns_the_error_page_body_with_status_404() {
    let s = Stack::start("b404", false);
    let r = s.get("/missing");

    assert_eq!(r.status, 404, "headers:\n{}", r.headers);
    assert_eq!(
        String::from_utf8_lossy(&r.body),
        ERROR_PAGE_BODY,
        "a backend 404 must serve the fetched error page, not the backend's raw body"
    );
    assert_eq!(
        s.backend_count("/missing"),
        1,
        "the original request must reach the backend exactly once"
    );
    assert_eq!(
        s.backend_count(ERROR_PATH),
        1,
        "the error page must be fetched exactly once"
    );
}

/// A backend 500 must likewise be replaced by the fetched error page while the
/// status stays 500, so a visitor sees the site's page, not a raw stack error.
#[test]
fn backend_500_returns_the_error_page_body_with_status_500() {
    let s = Stack::start("b500", false);
    let r = s.get("/broken");

    assert_eq!(r.status, 500, "headers:\n{}", r.headers);
    assert_eq!(
        String::from_utf8_lossy(&r.body),
        ERROR_PAGE_BODY,
        "a backend 500 must serve the fetched error page, not the backend's raw body"
    );
    assert_eq!(
        s.backend_count("/broken"),
        1,
        "the original request must reach the backend exactly once"
    );
    assert_eq!(
        s.backend_count(ERROR_PATH),
        1,
        "the error page must be fetched exactly once"
    );
}

/// When the error-page fetch itself fails, the response falls back to the
/// internal page once. The wrong outcome this catches is a retry: a second
/// fetch to a broken error page, which would double the backend's load and can
/// become a loop. The backend records each fetch, so a retry shows up as two.
#[test]
fn a_failed_error_page_fetch_falls_back_once_without_a_second_fetch() {
    let s = Stack::start("fallback", true);
    let r = s.get("/missing");

    assert_eq!(r.status, 404, "headers:\n{}", r.headers);
    let body = String::from_utf8_lossy(&r.body);
    assert!(
        body.contains("404") && body.contains("Not Found"),
        "the fallback must be the internal 404 page, got: {body}"
    );
    assert!(
        !body.contains(ERROR_PAGE_BODY),
        "the failed custom fetch must not leak its body into the response: {body}"
    );
    assert_eq!(
        s.backend_count(ERROR_PATH),
        1,
        "the error page must be fetched once and not retried after it fails"
    );
}

/// A request for the error path itself must not fetch the error page. Without
/// the anti-recursion guard the server would look up the error route and fetch
/// itself, returning the page where it should refuse. The backend records the
/// fetch, so a self-fetch shows up as one instead of zero.
#[test]
fn a_request_to_the_error_path_does_not_fetch_itself() {
    let s = Stack::start("recursion", false);
    let r = s.get("/errors?status=500");

    assert_eq!(r.status, 404, "headers:\n{}", r.headers);
    assert_eq!(
        s.backend_count(ERROR_PATH),
        0,
        "a request for the error path must not fetch itself"
    );
    let body = String::from_utf8_lossy(&r.body);
    assert!(
        body.contains("404") && body.contains("Not Found"),
        "a direct request for the error path must answer the internal 404 page, got: {body}"
    );
    assert!(
        !body.contains(ERROR_PAGE_BODY),
        "the self-fetch must not serve the error page body: {body}"
    );
}
