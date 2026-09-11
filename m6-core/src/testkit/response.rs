//! Reading one HTTP/1.1 response from a socket, for tests.
//!
//! Every backend test used to do this with `read_to_end`, which is a read
//! until the server closes. That worked only because no backend kept a
//! connection open. Once they did (RFC 9112 9.3, and h1spec's "Keep-alive
//! default" test), `read_to_end` sat there until the connection's own idle
//! timeout expired: a hundred-thread concurrency test went from under a second
//! to over a minute and started failing.
//!
//! A test that means "one response" should say so, rather than inferring it
//! from the server hanging up.

use std::io::{self, Read};

/// Read exactly one HTTP/1.1 response: the head, then the body its
/// `Content-Length` declares.
///
/// `method` is the method of the request this answers, because that is part of
/// the framing: a response to HEAD carries the `Content-Length` of the
/// representation a GET would have returned, and none of those bytes (RFC 9112
/// 6.3). Waiting for them is waiting forever.
///
/// Returns the whole thing, head included, so a caller can assert on either.
/// A response framed by `Transfer-Encoding: chunked` is not decoded -- no m6
/// backend sends one, and a test that needs it should say what it expects
/// rather than have this guess.
pub fn read_one<R: Read>(r: &mut R, method: &str) -> io::Result<Vec<u8>> {
    let mut buf = Vec::with_capacity(1024);
    let mut byte = [0u8; 1];

    // Byte at a time to the end of the head: reading in blocks would overshoot
    // into a pipelined second response, which is exactly what this must not
    // consume.
    while !buf.ends_with(b"\r\n\r\n") {
        match r.read(&mut byte)? {
            0 => return Ok(buf), // peer hung up mid-head; the caller asserts
            _ => buf.push(byte[0]),
        }
    }

    let head = String::from_utf8_lossy(&buf).to_ascii_lowercase();
    let status: u16 = head
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    // RFC 9112 6.3: these carry no body whatever their Content-Length says.
    let bodyless = method.eq_ignore_ascii_case("HEAD")
        || status == 204
        || status == 304
        || (100..200).contains(&status);
    if bodyless {
        return Ok(buf);
    }

    let len: usize = head
        .lines()
        .find_map(|l| l.strip_prefix("content-length:"))
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0);

    let mut body = vec![0u8; len];
    r.read_exact(&mut body)?;
    buf.extend_from_slice(&body);
    Ok(buf)
}
