//! Security regression tests for m6-http.
//!
//! Each test asserts the **secure** behaviour for one finding from the security
//! audit. Each one failed when written and passes now that the finding is
//! fixed; they stand as guards against reintroduction.
//!
//! Assertions are deliberately fix-agnostic: they state the property that must
//! hold, not the mechanism, so a future refactor that preserves the property
//! keeps them green.
//!
//! The doc comment on each test describes the original defect, so the reason
//! the test exists survives the fix.

use std::io::{Read, Write};
use std::os::unix::net::UnixListener;
use std::path::Path;

use base64::Engine;

use m6_http_lib::forward::{self, HttpRequest};

// ── Helpers ───────────────────────────────────────────────────────────────────

fn b64(s: &str) -> String {
    base64::engine::general_purpose::STANDARD.encode(s.as_bytes())
}

fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

/// Start a one-shot Unix-socket backend that captures the raw request bytes it
/// receives and replies with a minimal 200. Tests using it send header-only
/// requests, so capture stops at the header terminator.
fn capture_backend(sock_path: &Path) -> std::thread::JoinHandle<Vec<u8>> {
    let listener = UnixListener::bind(sock_path).expect("bind unix socket");
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut buf = Vec::new();
        let mut tmp = [0u8; 4096];
        loop {
            match stream.read(&mut tmp) {
                Ok(0) => break,
                Ok(n) => {
                    buf.extend_from_slice(&tmp[..n]);
                    if find_header_end(&buf).is_some() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok");
        let _ = stream.flush();
        buf
    })
}

/// Values of every header line whose name matches `name`, in wire order.
fn header_values(raw: &str, name: &str) -> Vec<String> {
    let want = format!("{}:", name.to_ascii_lowercase());
    raw.split("\r\n")
        .filter(|line| line.to_ascii_lowercase().starts_with(&want))
        .map(|line| line[line.find(':').unwrap() + 1..].trim().to_string())
        .collect()
}

/// Forward `req` to a throwaway backend and return the raw bytes it received.
fn forward_and_capture(req: &HttpRequest, client_ip: &str) -> String {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("backend.sock");
    let handle = capture_backend(&sock);
    forward::forward_request(&sock, req, client_ip, "example.com").expect("forward");
    String::from_utf8(handle.join().unwrap()).expect("utf8 request")
}

fn get_request(path: &str, headers: Vec<(String, String)>) -> HttpRequest {
    HttpRequest {
        method: "GET".to_string(),
        path: path.to_string(),
        query: None,
        version: "HTTP/1.1".to_string(),
        headers,
        body: vec![],
    }
}

// ── Finding 2c: proxy-verified claims must still reach the backend ──────────

/// The counterpart to stripping forged claims at ingress: once m6-http has
/// verified a JWT it appends its *own* `X-Auth-Claims`, and that copy must
/// reach the renderer — otherwise stripping would have broken authentication
/// instead of securing it.
///
/// Property: proxy-added claims are forwarded intact.
#[test]
fn finding_2c_proxy_verified_claims_still_reach_backend() {
    let verified = b64(r#"{"sub":"alice","groups":["users"],"roles":[]}"#);
    let req = get_request(
        "/admin",
        vec![("X-Auth-Claims".to_string(), verified.clone())],
    );

    let raw = forward_and_capture(&req, "203.0.113.9");

    assert_eq!(
        header_values(&raw, "x-auth-claims"),
        vec![verified],
        "the renderer must receive exactly the proxy's verified claims:\n{raw}"
    );
}

// ── Finding 6: X-Forwarded-For is spoofable ──────────────────────────────────

/// `forward.rs:119` appends the real peer IP *after* the client's own headers.
/// m6-auth-server reads it with `m6-core`'s first-match `.find()`
/// (`m6-core/src/http.rs:38`) at `m6-auth-server/src/main.rs:238`, so the
/// forged value wins and login brute-force protection can be evaded by
/// rotating the header.
///
/// Property: the backend must see exactly one `X-Forwarded-For`, containing
/// the real peer IP.
#[test]
fn finding_6_forged_x_forwarded_for_must_not_reach_backend() {
    let req = get_request(
        "/auth/login",
        vec![("X-Forwarded-For".to_string(), "10.0.0.99".to_string())],
    );

    let raw = forward_and_capture(&req, "203.0.113.9");
    let values = header_values(&raw, "x-forwarded-for");

    assert_eq!(
        values,
        vec!["203.0.113.9".to_string()],
        "per-IP rate limiting keys on attacker-controlled input. \
         Forwarded request:\n{raw}"
    );
}
