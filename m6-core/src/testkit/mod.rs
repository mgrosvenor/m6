//! Test harness shared by every m6 crate.
//!
//! Integration tests here all do the same four things: find a port nothing
//! else will take, find the binary cargo built for this run, start a service
//! and wait for it to actually be ready, then talk to it at the byte level.
//! Every crate had written its own version of all four, and the versions
//! disagreed in ways that produced intermittent failures rather than honest
//! ones.
//!
//! # What this fixes, beyond removing copies
//!
//! What is here is what the suites use. A byte-level HTTP client was written
//! for this module and then deleted: every suite that talks raw bytes does it
//! over TLS, using its own helper, and none of them adopted it. It can come
//! back when something actually needs it, shaped by that need.
//!
//! **Ports.** Four suites used `TcpListener::bind(":0")` and read the port back
//! after dropping the listener, which is a time-of-check-to-time-of-use race:
//! the port is free again the instant the helper returns, and the server does
//! not claim it until a child process has been spawned seconds later. [`port`]
//! holds the claim across that window.
//!
//! **Readiness.** Suites slept a fixed number of milliseconds and hoped. A
//! sleep that is long enough on an idle laptop is not long enough when a dozen
//! stacks start at once, and the failure is a 502 that looks like a routing
//! bug. [`wait`] waits on the real event, and gives up early if the process it
//! is waiting for has died.
//!
//! **Diagnosis.** Every suite spawned children with `Stdio::piped()` stderr and
//! then never read it. Two consequences: a child that logs more than a pipe
//! buffer blocks forever in `write`, and when a child dies its last words are
//! discarded, so the test reports `ConnectionRefused` and nothing else.
//! [`process::Service`] drains stderr continuously and prints it when an
//! assertion about the service fails.
//!
//! # Availability
//!
//! Behind the `testkit` feature, off by default, so nothing here is compiled
//! into a production binary. Enable it in `dev-dependencies` only:
//!
//! ```toml
//! [dev-dependencies]
//! m6-core = { path = "../m6-core", features = ["testkit"] }
//! ```
//!
//! [`alloc_count`] is also reachable through the `alloc-count` feature on its
//! own, which exposes that one file and none of the harness. A service built
//! with it declares `m6_core::alloc::M6Alloc` as its `#[global_allocator]`, which
//! counts under this feature and is plain mimalloc without it, and reads its
//! loop's count.
//! ALLOC-0's `m6-http` is the first caller.

// Ungated: the `alloc-count` feature exposes this one file without the rest of
// the harness, so a service binary can count its own loop's allocations.
pub mod alloc_count;
#[cfg(feature = "testkit")]
pub mod paths;
#[cfg(feature = "testkit")]
pub mod port;
#[cfg(feature = "testkit")]
pub mod process;
#[cfg(feature = "testkit")]
pub mod response;
#[cfg(feature = "testkit")]
pub mod wait;

/// The ceiling for a service to start: its socket or port exists, its started
/// line is logged, and it accepts a connection.
///
/// Every wait that uses it is on the event itself and returns as soon as the
/// event happens or the child exits, so this is a ceiling, not a sleep and not
/// the expected wait. It is generous because the suite starts real services
/// while the host may be running other suites. On 2026-11-16 a 10 second
/// ceiling turned CPU contention into a startup failure in seven `m6-file`
/// tests and a response timeout in an `m6-auth-server` test.
#[cfg(feature = "testkit")]
pub const STARTUP_BOUND: std::time::Duration = std::time::Duration::from_secs(60);

#[cfg(feature = "testkit")]
pub use paths::{binary, exec_scratch_root};
#[cfg(feature = "testkit")]
pub use port::{claim_port, PortClaim};
#[cfg(feature = "testkit")]
pub use process::{
    assert_app_lifecycle, assert_lifecycle_logged, lowered_fd_limit_command, spawn_instances,
    Service,
};
#[cfg(feature = "testkit")]
pub use response::{read_one, read_response, read_response_event, wait_for_event, RESPONSE_BOUND};
#[cfg(feature = "testkit")]
pub use wait::{for_path, for_tcp};
