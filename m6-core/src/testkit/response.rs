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

#![allow(
    clippy::disallowed_methods,
    reason = "testkit: the waiter runs beside the service, and a bounded wait blocks the test's own thread"
)]

use std::io::{self, Read};
use std::os::unix::net::UnixStream;
use std::sync::mpsc;
use std::time::Duration;

use super::Service;

/// The safety bound for one response from a service.
///
/// Above the slowest work a login does, two cost-12 bcrypt verifies, which cost
/// about 1.5 to 2 seconds each on an unoptimised build and more on a loaded
/// host. The wait is on the response event or the service's exit; this bound
/// exists so a service that stays alive and silent fails instead of hanging the
/// suite. A test that asserts a shorter prompt answer passes its own bound.
pub const RESPONSE_BOUND: Duration = Duration::from_secs(60);

/// How often the waiter checks whether the service has exited.
const RESPONSE_POLL: Duration = Duration::from_millis(25);

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

/// Wait for one response, or for `svc` to exit, under `within`.
///
/// A reader thread blocks on `stream` and sends the response back, while the
/// test thread checks the child every [`RESPONSE_POLL`]. A complete response
/// wins as soon as it arrives; a child that exits wins when it happens first
/// and fails with its output. A partial response, including the empty read at
/// end of file, is reported as the child's death when the child has exited;
/// otherwise it is returned for the caller's assertion to name.
///
/// The bound is a safety net, not the expected wait: a silent child that stays
/// alive fails at `within` instead of hanging the suite.
pub fn read_response(
    svc: &mut Service,
    mut stream: UnixStream,
    method: &str,
    within: Duration,
) -> Result<Vec<u8>, String> {
    let method = method.to_string();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(read_one(&mut stream, &method));
    });

    let started = crate::platform::now();
    loop {
        match rx.recv_timeout(RESPONSE_POLL) {
            Ok(Ok(resp)) if resp.starts_with(b"HTTP/") => return Ok(resp),
            Ok(Ok(resp)) => {
                return Err(match svc.wait_for_exit(crate::platform::now()) {
                    Some(status) => format!(
                        "the service exited before answering: {status}\n--- output ---\n{}",
                        svc.output()
                    ),
                    None => return Ok(resp),
                });
            }
            Ok(Err(e)) => {
                return Err(match svc.wait_for_exit(crate::platform::now()) {
                    Some(status) => format!(
                        "the service exited while reading the response: {status}\n--- output ---\n{}",
                        svc.output()
                    ),
                    None => format!("reading the response: {e}"),
                });
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if let Some(status) = svc.wait_for_exit(crate::platform::now()) {
                    return Err(format!(
                        "the service exited while waiting for the response: {status}\n--- output ---\n{}",
                        svc.output()
                    ));
                }
                if started.elapsed() >= within {
                    return Err(format!(
                        "no response within {within:?} and the service was still running\n--- output ---\n{}",
                        svc.output()
                    ));
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err("the response reader ended without a result".to_string());
            }
        }
    }
}
