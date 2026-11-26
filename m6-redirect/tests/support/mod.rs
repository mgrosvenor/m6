//! The helpers m6-redirect's integration tests share.

use m6_core::testkit::{read_response_event, RESPONSE_BOUND};
use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::Path;

/// One HTTP/1.1 response, head and body together as text.
pub struct Response {
    pub status: u16,
    pub raw: String,
}

impl Response {
    /// The first value of a named header, lowercased for comparison.
    pub fn header(&self, name: &str) -> Option<String> {
        self.raw
            .lines()
            .skip(1)
            .take_while(|l| !l.is_empty())
            .find_map(|l| {
                let (k, v) = l.split_once(':')?;
                k.eq_ignore_ascii_case(name).then(|| v.trim().to_string())
            })
    }
}

/// Send one raw request and read one response. `method` is the request's
/// method, so a HEAD is read with the right framing.
pub fn send(sock: &Path, method: &str, request: &str) -> Response {
    let mut stream = UnixStream::connect(sock).expect("connect to m6-redirect");
    write!(stream, "{request}").expect("send the request");
    let bytes = read_response_event(stream, method, RESPONSE_BOUND).expect("a response");
    let raw = String::from_utf8_lossy(&bytes).into_owned();
    let status = raw
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    Response { status, raw }
}

/// A GET with one host header.
pub fn get(sock: &Path, host: &str, path: &str) -> Response {
    send(
        sock,
        "GET",
        &format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n"),
    )
}

/// A HEAD with one host header.
pub fn head(sock: &Path, host: &str, path: &str) -> Response {
    send(
        sock,
        "HEAD",
        &format!("HEAD {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n"),
    )
}

/// A POST with a body.
pub fn post(sock: &Path, path: &str, body: &str) -> Response {
    send(
        sock,
        "POST",
        &format!(
            "POST {path} HTTP/1.1\r\nHost: m6\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        ),
    )
}
