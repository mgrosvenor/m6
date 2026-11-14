//! Shutting down on purpose.
//!
//! **`block()` must be the first statement of `main`.** The signal mask is
//! inherited only by threads created after it, and a service's logging writer
//! is a thread; if it starts first it takes SIGTERM at the default disposition
//! and the process dies instead of shutting down. `Signals::new` asserts the
//! mask is set, and that assertion has since caught a real case: a set of
//! service runners started logging first, so SIGTERM handling was quietly
//! wrong for every service built on them.
//!
//! [`Lifecycle`] unlinks the socket on the way out so m6-http does not keep a
//! dead member in its backend pool.
//!
//! SIGUSR1 reopens the log file and never counts toward shutdown.

use std::path::{Path, PathBuf};

/// Block SIGTERM, SIGINT and SIGUSR1 in the calling thread.
///
/// **This must be the first statement of `main`.** Not the first interesting
/// statement: the first one, before logging, before any pool, before anything
/// that could spawn a thread. Threads inherit the signal mask as it stands
/// when they are created, and a single thread created before this call is
/// enough to make the process die on SIGTERM instead of shutting down.
///
/// Idempotent.
pub fn block() {
    crate::platform::block();
}

/// Reopen the log file for [`crate::platform::Signal::ReopenLogs`] and say what happened.
fn reopen_on_signal(name: &str) {
    match crate::log::reopen_log_file() {
        crate::log::Reopened::NoFile => {
            tracing::info!("{name} log reopen requested; this process writes no log file");
        }
        crate::log::Reopened::Done(p) => {
            tracing::info!(path = %p.display(), "{name} log file reopened");
        }
        crate::log::Reopened::Failed(p, e) => {
            tracing::error!(
                path = %p.display(),
                error = %e,
                "{name} log reopen failed; still writing to the previous file"
            );
        }
    }
}

/// Remove the socket file and log the completion line.
fn finish(svc_name: &str, socket: Option<&Path>, how: &str) {
    if let Some(path) = socket {
        let _ = std::fs::remove_file(path);
    }
    tracing::info!("{svc_name} shutdown {how}");
}

/// The `"<name> started"` line, with the socket when there is one, used by
/// [`Lifecycle::started`].
fn log_started_line(name: &str, socket: Option<&Path>) {
    match socket {
        Some(path) => tracing::info!(socket = %path.display(), "{} started", name),
        None => tracing::info!("{} started", name),
    }
}

/// What a loop does after one signal.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Action {
    Continue,
    Stop,
}

/// One termination: log it, force an exit on the second, and say whether to
/// stop.
fn handle_terminate(name: &str, socket: Option<&Path>, count: u32) -> Action {
    if count >= 2 {
        finish(name, socket, "forced");
        std::process::exit(0);
    }
    tracing::info!("{name} shutdown signal received");
    Action::Stop
}

/// The signal thread's per-signal decisions, without the thread.
///
/// A loop that reads [`crate::platform::Signals`] as a descriptor owns one of
/// these and asks it what each signal means. The first [`crate::platform::Signal::Shutdown`]
/// answers [`Action::Stop`]; a second before the loop has ended forces an exit,
/// exactly as the signal thread did.
pub struct Lifecycle {
    name: String,
    socket: Option<PathBuf>,
    terminations: u32,
}

impl Lifecycle {
    pub fn new(name: impl Into<String>, socket: Option<PathBuf>) -> Lifecycle {
        Lifecycle {
            name: name.into(),
            socket,
            terminations: 0,
        }
    }

    /// `"<name> started"`, as [`ShutdownHandle::install`] logs it.
    pub fn started(&self) {
        log_started_line(&self.name, self.socket.as_deref());
    }

    /// What to do after one signal. [`Action::Stop`] means the loop breaks.
    pub fn on_signal(&mut self, s: crate::platform::Signal) -> Action {
        match s {
            crate::platform::Signal::ReopenLogs => {
                reopen_on_signal(&self.name);
                Action::Continue
            }
            crate::platform::Signal::Shutdown => {
                self.terminations += 1;
                handle_terminate(&self.name, self.socket.as_deref(), self.terminations)
            }
        }
    }

    /// Finish a graceful shutdown: unlink the socket, log the completion line.
    pub fn complete(&self) {
        finish(&self.name, self.socket.as_deref(), "complete");
    }
}
