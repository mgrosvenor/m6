//! Shutting down on purpose.
//!
//! **`block()` must be the first statement of `main`.** The signal mask is
//! inherited only by threads created after it, and the platform's signal
//! descriptor refuses to open without it. A thread started first takes SIGTERM
//! at the default disposition, and the process dies instead of shutting down.
//!
//! [`Lifecycle`] is the per-service surface. A service's loop reads the
//! platform's signal descriptor and asks it what each signal means. SIGUSR1
//! reopens the log file and never counts toward shutdown.
//!
//! **Log the shutdown line before the loop stops, not after.** `on_signal`
//! logs it before it returns [`Action::Stop`], so the line is buffered ahead
//! of `complete`'s completion line and `log::stop` flushes both. A line logged
//! after the decision to stop is racing the process exit, and on a loaded
//! machine it loses.

use std::path::{Path, PathBuf};

use crate::log::{error, info};

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

/// Remove the socket file and log the completion line.
fn finish(svc_name: &str, socket: Option<&Path>, how: &str) {
    if let Some(path) = socket {
        let _ = std::fs::remove_file(path);
    }
    info!("{svc_name} shutdown {how}");
}

/// The `"<name> started"` line, with the socket when there is one, used by
/// [`Lifecycle::started`].
fn log_started_line(name: &str, socket: Option<&Path>) {
    match socket {
        Some(path) => info!(socket = %path.display(), "{} started", name),
        None => info!("{} started", name),
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
    info!("{name} shutdown signal received");
    Action::Stop
}

/// The per-service signal decisions, without a thread.
///
/// A loop that reads [`crate::platform::Signals`] as a descriptor owns one of
/// these and asks it what each signal means. The first [`crate::platform::Signal::Shutdown`]
/// answers [`Action::Stop`]; a second before the loop has ended forces an exit.
pub struct Lifecycle {
    name: String,
    socket: Option<PathBuf>,
    terminations: u32,
}

/// The lines a log reopen logs, with the service's name, so a loop that
/// performs the reopen itself says what happened.
fn log_reopen(name: &str, result: &std::io::Result<()>) {
    if !crate::log::writes_log_file() {
        info!("{name} log reopen requested; this process writes no log file");
        crate::log::flush();
        return;
    }
    match result {
        Ok(()) => info!("{name} log file reopened"),
        Err(e) => error!(
            error = %e,
            "{name} log reopen failed; still writing to the previous file"
        ),
    }
    // The operator asked for this reopen, so its line is written now rather
    // than on the loop's next tick.
    crate::log::flush();
}

impl Lifecycle {
    pub fn new(name: impl Into<String>, socket: Option<PathBuf>) -> Lifecycle {
        Lifecycle {
            name: name.into(),
            socket,
            terminations: 0,
        }
    }

    /// `"<name> started"`, the lifecycle line every unit shares.
    pub fn started(&self) {
        log_started_line(&self.name, self.socket.as_deref());
    }

    /// The lines a log reopen logs, with the service's name, so a loop that
    /// performs the reopen itself says what happened.
    pub fn reopened(&self, result: &std::io::Result<()>) {
        log_reopen(&self.name, result);
    }

    /// What to do after one signal. [`Action::Stop`] means the loop breaks.
    pub fn on_signal(&mut self, s: crate::platform::Signal) -> Action {
        match s {
            crate::platform::Signal::ReopenLogs => {
                self.reopened(&crate::log::reopen());
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
