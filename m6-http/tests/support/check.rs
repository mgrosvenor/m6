#![allow(
    dead_code,
    reason = "one shared helper, and each test binary uses part of it"
)]

//! A `CheckServer` that is already answering, for the process-level fixtures.
//!
//! [`CheckServer::serve_one`] is caller-driven, so a test that blocks in a TLS
//! client cannot call it. [`ServingCheck`] answers on its own thread until the
//! fixture is dropped. It lives in the testkit because every fixture that
//! starts a real service needs the same thing, and the one in this module had
//! a defect the testkit now owns: it stopped answering after ten idle seconds,
//! and one missed check killed it, so a long test saw `NotFound` on every
//! later gate.
//!
//! This module also holds the one place a test distinguishes the design's 503
//! from a service failure. A check that misses its 50 ms deadline is the
//! designed answer, so a test waiting for `/perf`, `/traffic` or `/genid`
//! retries it under the testkit bound and honours `Retry-After`.

use std::time::{Duration, Instant};

use m6_core::auth::Identity;
#[allow(
    unused_imports,
    reason = "each test binary uses part of this one helper, and not every binary names Reply"
)]
pub use m6_core::testkit::Reply;
pub use m6_core::testkit::ServingCheck as RunningCheck;

/// The status the edge answers when a check did not answer in time.
pub const CHECK_UNAVAILABLE: u16 = 503;

/// Retry `attempt` while it answers [`CHECK_UNAVAILABLE`], sleeping the
/// `Retry-After` it names, until `bound` elapses. Returns the last answer.
///
/// `attempt` returns the status, the answer's `Retry-After` if it sent one, and
/// the answer itself. The wait is the server's own instruction rather than a
/// test's guess, which is why it is a sleep and not a poll: RFC 9110 10.2.3
/// says when to come back, and the edge sends `Retry-After: 1`.
pub fn retry_unavailable<T>(
    bound: Duration,
    mut attempt: impl FnMut() -> (u16, Option<Duration>, T),
) -> T {
    let deadline = Instant::now() + bound;
    loop {
        let (status, retry_after, answer) = attempt();
        let left = deadline.saturating_duration_since(Instant::now());
        if status != CHECK_UNAVAILABLE || left.is_zero() {
            return answer;
        }
        let wait = retry_after.unwrap_or(Duration::from_millis(10)).min(left);
        std::thread::sleep(wait);
    }
}

/// `Retry-After` as seconds, for the delta-seconds form m6-http sends.
pub fn retry_after(value: Option<&str>) -> Option<Duration> {
    value?.trim().parse::<u64>().ok().map(Duration::from_secs)
}

/// Whether `path` is answered through the one token check.
pub fn check_gated(path: &str) -> bool {
    matches!(path, "/perf" | "/traffic" | "/genid")
}

/// An identity answer with the given groups and roles.
pub fn identity(groups: &[&str], roles: &[&str]) -> Identity {
    Identity {
        sub: "user-1".to_string(),
        username: "alice".to_string(),
        roles: roles.iter().map(|role| role.to_string()).collect(),
        groups: groups.iter().map(|group| group.to_string()).collect(),
    }
}
