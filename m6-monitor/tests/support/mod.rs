//! The helpers m6-monitor's integration tests share.
//!
//! The test crate that includes this module decides its own clippy allowance
//! for blocking clients, as every test file does.

use m6_core::testkit::{read_response_event, RESPONSE_BOUND};
use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::Path;

/// One GET on the monitor's socket, waiting on the response event.
///
/// The wait is on the response, not on a read deadline, and it returns as soon
/// as the bytes arrive. `RESPONSE_BOUND` is the ceiling that turns a silent
/// service into a failure instead of a hung suite.
pub fn get(sock: &Path, target: &str) -> (u16, String) {
    let mut s = UnixStream::connect(sock).expect("connect to the monitor");
    write!(
        s,
        "GET {target} HTTP/1.1\r\nHost: m6\r\nConnection: close\r\n\r\n"
    )
    .expect("send the request");
    let raw = read_response_event(s, "GET", RESPONSE_BOUND).expect("a response");
    let text = String::from_utf8_lossy(&raw).into_owned();
    let status = text
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    (status, text)
}
