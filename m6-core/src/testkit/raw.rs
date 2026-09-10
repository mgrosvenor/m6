//! A byte-level HTTP client, for tests about framing rather than semantics.
//!
//! Nothing here parses or validates on the way out. That is the point: these
//! tests send request-shaped byte sequences that no correct client would
//! produce (a smuggled `CR`, two `Content-Length` headers, a header line four
//! kilobytes long) and assert on what comes back. A client that fixed the
//! bytes up would test nothing.
//!
//! Generic over the transport so the same helpers serve a plain `TcpStream`, a
//! `UnixStream` to a backend, and a `rustls::StreamOwned` for the TLS suites.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

/// Stop reading a response at this size. Large enough for anything the suites
/// serve, small enough that a server stuck in a send loop fails the test
/// rather than filling memory.
const MAX_RESPONSE: usize = 4 * 1024 * 1024;

const IO_TIMEOUT: Duration = Duration::from_secs(10);

/// A connection carrying raw bytes in both directions.
pub struct RawConn<S> {
    inner: S,
}

impl RawConn<TcpStream> {
    /// Connect to a loopback port with read and write timeouts set.
    ///
    /// The timeouts matter more than they look: without them a test against a
    /// server that accepts and then never answers hangs the whole suite
    /// instead of failing.
    pub fn tcp(port: u16) -> std::io::Result<Self> {
        let sock = TcpStream::connect(("127.0.0.1", port))?;
        sock.set_read_timeout(Some(IO_TIMEOUT))?;
        sock.set_write_timeout(Some(IO_TIMEOUT))?;
        Ok(RawConn { inner: sock })
    }
}

impl RawConn<UnixStream> {
    /// Connect to a backend's unix socket with read and write timeouts set.
    pub fn unix(path: &Path) -> std::io::Result<Self> {
        let sock = UnixStream::connect(path)?;
        sock.set_read_timeout(Some(IO_TIMEOUT))?;
        sock.set_write_timeout(Some(IO_TIMEOUT))?;
        Ok(RawConn { inner: sock })
    }
}

impl<S: Read + Write> RawConn<S> {
    /// Wrap an already-established stream, such as a TLS session.
    pub fn new(inner: S) -> Self {
        RawConn { inner }
    }

    /// Send bytes, then read until EOF or timeout.
    ///
    /// An empty result means the server closed without answering, which is a
    /// legitimate response to garbage and is treated as such throughout. A
    /// write failure is likewise an outcome, not a test error: the server is
    /// entitled to close on us mid-request.
    pub fn exchange(&mut self, request: &[u8]) -> Vec<u8> {
        if self.inner.write_all(request).is_err() {
            return Vec::new();
        }
        let _ = self.inner.flush();
        self.read_to_close()
    }

    /// Read until the peer closes, the timeout expires, or `MAX_RESPONSE`.
    pub fn read_to_close(&mut self) -> Vec<u8> {
        let mut out = Vec::new();
        let mut buf = [0u8; 8192];
        loop {
            match self.inner.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    out.extend_from_slice(&buf[..n]);
                    if out.len() >= MAX_RESPONSE {
                        break;
                    }
                }
            }
        }
        out
    }

    /// The underlying stream, for a test that needs to do something this type
    /// deliberately does not offer.
    pub fn get_mut(&mut self) -> &mut S {
        &mut self.inner
    }
}

/// A plain `GET` with `Connection: close`, the request most tests send to
/// prove a server is still healthy.
pub fn get(path: &str, host: &str) -> Vec<u8> {
    format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n").into_bytes()
}

/// The response head, up to and including the blank line.
///
/// Falls back to the whole input when there is no blank line, because a
/// truncated response is exactly what several of these tests are looking for
/// and returning nothing would hide it.
pub fn head_of(resp: &[u8]) -> String {
    let end = resp
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|i| i + 4)
        .unwrap_or(resp.len());
    String::from_utf8_lossy(&resp[..end]).into_owned()
}

/// The three-digit status, or `None` if the response does not start with a
/// status line.
pub fn status_of(resp: &[u8]) -> Option<u16> {
    let line = resp.split(|&b| b == b'\r' || b == b'\n').next()?;
    let line = std::str::from_utf8(line).ok()?;
    line.split(' ').nth(1)?.parse().ok()
}

/// The value of a response header, matched case-insensitively.
///
/// Searches the head only, so a body that happens to contain the header name
/// cannot satisfy the lookup.
pub fn header_of(resp: &[u8], name: &str) -> Option<String> {
    let head = head_of(resp);
    for line in head.lines().skip(1) {
        let (k, v) = line.split_once(':')?;
        if k.trim().eq_ignore_ascii_case(name) {
            return Some(v.trim().to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const RESP: &[u8] = b"HTTP/1.1 404 Not Found\r\nContent-Length: 3\r\nX-Node: syd\r\n\r\nabc";

    #[test]
    fn parses_status_and_headers() {
        assert_eq!(status_of(RESP), Some(404));
        assert_eq!(header_of(RESP, "content-length").as_deref(), Some("3"));
        assert_eq!(header_of(RESP, "X-NODE").as_deref(), Some("syd"));
        assert_eq!(header_of(RESP, "absent"), None);
    }

    #[test]
    fn head_stops_at_the_blank_line() {
        assert!(!head_of(RESP).contains("abc"));
        // A response with no blank line is returned whole, not discarded.
        assert_eq!(head_of(b"HTTP/1.1 200 OK\r\n"), "HTTP/1.1 200 OK\r\n");
    }

    #[test]
    fn status_of_rejects_a_non_response() {
        assert_eq!(status_of(b"garbage"), None);
        assert_eq!(status_of(b""), None);
    }
}
