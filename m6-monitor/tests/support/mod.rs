//! The helpers m6-monitor's integration tests share.
//!
//! The test crate that includes this module decides its own clippy allowance
//! for blocking clients, as every test file does.

use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use m6_core::testkit::read_one;

/// One GET on the monitor's socket, with a read deadline.
///
/// A reply that does not arrive within `within` is a failure, never a slow
/// pass: `read_one` returns the error a timed-out read gives.
pub fn get(sock: &Path, target: &str, within: Duration) -> (u16, String) {
    let mut s = UnixStream::connect(sock).expect("connect to the monitor");
    s.set_read_timeout(Some(within)).expect("a read deadline");
    write!(
        s,
        "GET {target} HTTP/1.1\r\nHost: m6\r\nConnection: close\r\n\r\n"
    )
    .expect("send the request");
    let raw = read_one(&mut s, "GET").expect("a response inside the deadline");
    let text = String::from_utf8_lossy(&raw).into_owned();
    let status = text
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    (status, text)
}
