/// Signal handling for m6 processes: double-SIGTERM graceful shutdown pattern.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use nix::sys::signal::{self, SaFlags, SigAction, SigHandler, SigSet, Signal};

/// Global shutdown flag and count, set by the signal handler.
/// Using global statics because signal handlers cannot capture environment.
static SHUTDOWN_FLAG: AtomicBool = AtomicBool::new(false);
static SIGNAL_COUNT: AtomicUsize = AtomicUsize::new(0);

extern "C" fn handle_signal(_signum: libc::c_int) {
    let prev = SIGNAL_COUNT.fetch_add(1, Ordering::SeqCst);
    if prev == 0 {
        // First signal: request graceful shutdown.
        SHUTDOWN_FLAG.store(true, Ordering::SeqCst);
    } else {
        // Second signal: immediate exit.
        std::process::exit(0);
    }
}

/// Shared shutdown state installed once at process start.
///
/// First SIGTERM/SIGINT sets the shutdown flag (graceful).
/// Second SIGTERM/SIGINT calls `std::process::exit(0)` immediately.
///
/// The handle is `Clone` so it can be shared across threads.  All clones
/// observe the same global flag.
#[derive(Clone)]
pub struct ShutdownHandle(());

impl ShutdownHandle {
    /// Install signal handlers. Call once at startup.
    pub fn install() -> Self {
        let sa = SigAction::new(
            SigHandler::Handler(handle_signal),
            SaFlags::empty(),
            SigSet::empty(),
        );

        // SAFETY: Installing a signal handler with a plain C function is safe.
        unsafe {
            let _ = signal::sigaction(Signal::SIGTERM, &sa);
            let _ = signal::sigaction(Signal::SIGINT, &sa);
        }

        ShutdownHandle(())
    }

    /// Returns true if graceful shutdown has been requested.
    pub fn is_shutdown(&self) -> bool {
        SHUTDOWN_FLAG.load(Ordering::SeqCst)
    }

    /// Block until shutdown is requested (spin with exponential back-off).
    pub fn wait(&self) {
        use std::time::Duration;
        let mut sleep_ms = 1u64;
        while !self.is_shutdown() {
            std::thread::sleep(Duration::from_millis(sleep_ms));
            sleep_ms = (sleep_ms * 2).min(100);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_shutdown_handle_initially_false() {
        // Reset global state for test isolation.
        SHUTDOWN_FLAG.store(false, Ordering::SeqCst);
        SIGNAL_COUNT.store(0, Ordering::SeqCst);

        let handle = ShutdownHandle(());
        assert!(!handle.is_shutdown());
    }

    #[test]
    fn test_shutdown_flag_set_on_first_signal() {
        SHUTDOWN_FLAG.store(false, Ordering::SeqCst);
        SIGNAL_COUNT.store(0, Ordering::SeqCst);

        let handle = ShutdownHandle(());

        // Simulate first signal by directly triggering the handler.
        handle_signal(libc::SIGTERM);
        assert!(handle.is_shutdown());

        // Restore for other tests.
        SHUTDOWN_FLAG.store(false, Ordering::SeqCst);
        SIGNAL_COUNT.store(0, Ordering::SeqCst);
    }
}
