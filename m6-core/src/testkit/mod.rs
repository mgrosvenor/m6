//! Test harness shared by every m6 crate.
//!
//! Integration tests here all do the same four things: find a port nothing
//! else will take, find the release binary, start a service and wait for it to
//! actually be ready, then talk to it at the byte level. Every crate had
//! written its own version of all four, and the versions disagreed in ways
//! that produced intermittent failures rather than honest ones.
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

pub mod paths;
pub mod port;
pub mod process;
pub mod response;
pub mod wait;

pub use paths::binary;
pub use port::{claim_port, PortClaim};
pub use process::{assert_lifecycle_logged, Service};
pub use response::read_one;
pub use wait::{for_path, for_tcp};
