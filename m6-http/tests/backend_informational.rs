//! A backend may send informational (1xx) responses before its final one
//! (RFC 9110 15.2). The proxy must skip them and return the final response.
//!
//! Each test runs a loopback backend on a Unix socket and fetches from it
//! through `forward_request`, the call the edge makes for a cache miss.

use std::io::{Read, Write};
use std::os::unix::net::UnixListener;
use std::path::Path;

use m6_http_lib::forward::{self, HttpRequest};

const EARLY_HINTS: &[u8] =
    b"HTTP/1.1 103 Early Hints\r\nLink: </app.css>; rel=preload; as=style\r\n\r\n";
const FINAL: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello";

/// A one-shot backend. It reads the request head, then writes `informational`
/// copies of a 103 followed by the final response, in one write.
fn backend(sock: &Path, informational: usize) -> std::thread::JoinHandle<()> {
    let listener = UnixListener::bind(sock).expect("bind unix socket");
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut buf = [0u8; 4096];
        let mut seen = Vec::new();
        while !seen.windows(4).any(|w| w == b"\r\n\r\n") {
            let n = stream.read(&mut buf).expect("read request");
            if n == 0 {
                return;
            }
            seen.extend_from_slice(&buf[..n]);
        }
        let mut all = EARLY_HINTS.repeat(informational);
        all.extend_from_slice(FINAL);
        let _ = stream.write_all(&all);
    })
}

fn fetch(informational: usize) -> std::io::Result<forward::HttpResponse> {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("backend.sock");
    let handle = backend(&sock, informational);
    let req = HttpRequest {
        method: "GET".to_string(),
        path: "/".to_string(),
        query: None,
        version: "HTTP/1.1".to_string(),
        headers: vec![("Host".to_string(), "example.com".to_string())],
        body: vec![],
    };
    let r = forward::forward_request_timeout(
        &sock,
        &req,
        "127.0.0.1",
        "example.com",
        Some(std::time::Duration::from_secs(5)),
    );
    let _ = handle.join();
    r
}

#[test]
fn a_103_before_the_final_response_is_skipped() {
    let r = fetch(1).expect("fetch");
    assert_eq!(r.status, 200);
    assert_eq!(r.body, b"hello");
    assert!(r.header("link").is_none(), "the 103's headers are dropped");
}

#[test]
fn eight_informational_responses_are_still_accepted() {
    let r = fetch(8).expect("fetch");
    assert_eq!(r.status, 200);
    assert_eq!(r.body, b"hello");
}

#[test]
fn nine_informational_responses_fail_the_fetch() {
    let err = fetch(9).expect_err("nine informational responses must fail");
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidData, "{err}");
}
