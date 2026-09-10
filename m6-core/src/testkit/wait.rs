//! Waiting on the real event instead of sleeping and hoping.
//!
//! These are the process-free forms. When there is a
//! [`Service`](super::Service) to wait on, prefer its methods: they stop early
//! and report the child's stderr when the thing being waited for will never
//! happen because the child is dead.

use std::net::TcpStream;
use std::path::Path;
use std::time::{Duration, Instant};

const POLL: Duration = Duration::from_millis(25);

/// Wait for a filesystem path to appear.
pub fn for_path(p: &Path, timeout: Duration) -> bool {
    poll_until(timeout, || p.exists())
}

/// Wait for a loopback port to accept a connection.
///
/// A successful connect proves a listener exists; it does not prove the
/// listener will answer. Tests that need the stronger property should follow
/// this with a real request.
pub fn for_tcp(port: u16, timeout: Duration) -> bool {
    poll_until(timeout, || {
        TcpStream::connect_timeout(
            &(std::net::Ipv4Addr::LOCALHOST, port).into(),
            Duration::from_millis(250),
        )
        .is_ok()
    })
}

/// Wait for a unix socket to exist and accept a connection.
///
/// Stronger than [`for_path`]: m6 services create the socket file before they
/// listen on it, so a path check alone can return while a connect would still
/// be refused.
pub fn for_unix(path: &Path, timeout: Duration) -> bool {
    poll_until(timeout, || {
        std::os::unix::net::UnixStream::connect(path).is_ok()
    })
}

/// Poll `ready` until it returns true or `timeout` elapses.
///
/// The general form, for readiness that only the caller can express. The
/// specific case this exists for: a proxy accepting on its port does not mean
/// it can serve, because its backend pool is filled by a periodic rescan and
/// there is a window where every request is a 502. Suites used to cover that
/// window with `sleep(2500)`, tuned on a fast laptop, and it was not enough on
/// the slower build box.
pub fn until(timeout: Duration, ready: impl FnMut() -> bool) -> bool {
    poll_until(timeout, ready)
}

fn poll_until(timeout: Duration, mut ready: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if ready() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(POLL);
    }
}
