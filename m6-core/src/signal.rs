/// Signal handling for m6 processes: double-SIGTERM graceful shutdown pattern.
///
/// `m6-decisions.md` specifies this once, for all tools: SIGTERM and SIGINT are
/// identical, the first requests a clean shutdown and the second exits
/// immediately. It was then implemented four times, in four different ways.
///
/// **This is the `sigwait` design, adopted from `m6-file`, not the
/// signal-handler design this module used to carry.** A dedicated thread blocks
/// the signals and waits for one, so no code ever runs in signal context and
/// async-signal-safety stops being a concern: the "handler" is ordinary code on
/// an ordinary thread and may allocate, log or take a lock.
///
/// It also solves the problem the handler version ignored. A service blocked in
/// `accept()` does not notice a flag being set by a handler, so it sits there
/// until the next connection arrives. `on_shutdown` exists to poke it awake,
/// which for a Unix-socket service means connecting to its own socket.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use nix::sys::signal::{SigSet, Signal};

/// Global shutdown state. Global rather than per-handle so every clone, and
/// every thread, observes the same value.
static SHUTDOWN_FLAG: AtomicBool = AtomicBool::new(false);
static SIGNAL_COUNT: AtomicUsize = AtomicUsize::new(0);

/// Shared shutdown state installed once at process start.
///
/// First SIGTERM/SIGINT sets the shutdown flag (graceful).
/// Second SIGTERM/SIGINT exits immediately with status 0.
///
/// The handle is `Clone` so it can be shared across threads. All clones
/// observe the same global flag.
#[derive(Clone)]
pub struct ShutdownHandle(());

impl ShutdownHandle {
    /// Install signal handling. Call once at startup, before spawning threads
    /// that should inherit the blocked mask.
    pub fn install() -> Self {
        Self::install_with_hooks(|| {}, || {})
    }

    /// Install with a wake hook.
    ///
    /// `on_shutdown` runs on the first signal, after the flag is set, and is
    /// for unblocking a thread parked in `accept()` or similar. For a
    /// Unix-socket service that is a self-connect.
    pub fn install_with_wake<F>(on_shutdown: F) -> Self
    where
        F: Fn() + Send + 'static,
    {
        Self::install_with_hooks(on_shutdown, || {})
    }

    /// Install with both hooks.
    ///
    /// `on_shutdown` runs on the first signal (wake blocked threads).
    /// `on_force_exit` runs on the second signal, immediately before
    /// `exit(0)`, and is for cleanup that must happen even on an abrupt stop:
    /// removing a socket file, for instance, because a socket left behind
    /// keeps a dead member in the proxy's pool.
    ///
    /// Both are boxed once at startup. Nothing here runs per request.
    pub fn install_with_hooks<F, G>(on_shutdown: F, on_force_exit: G) -> Self
    where
        F: Fn() + Send + 'static,
        G: Fn() + Send + 'static,
    {
        let mut mask = SigSet::empty();
        mask.add(Signal::SIGTERM);
        mask.add(Signal::SIGINT);
        // Block in this thread so every thread spawned afterwards inherits the
        // mask and the dedicated waiter is the only place they are delivered.
        let _ = mask.thread_block();

        std::thread::Builder::new()
            .name("m6-signal".into())
            .spawn(move || {
                let mut wait_mask = SigSet::empty();
                wait_mask.add(Signal::SIGTERM);
                wait_mask.add(Signal::SIGINT);
                let _ = wait_mask.thread_block();

                loop {
                    match wait_mask.wait() {
                        Ok(_sig) => {
                            let count = SIGNAL_COUNT.fetch_add(1, Ordering::SeqCst) + 1;
                            if count >= 2 {
                                on_force_exit();
                                std::process::exit(0);
                            }
                            SHUTDOWN_FLAG.store(true, Ordering::SeqCst);
                            on_shutdown();
                        }
                        Err(_) => break,
                    }
                }
            })
            .expect("spawning the signal thread");

        ShutdownHandle(())
    }

    /// Returns true if graceful shutdown has been requested.
    #[inline]
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

/// Process-wide shutdown flag, for code that has no handle to hand.
#[inline]
pub fn is_shutdown() -> bool {
    SHUTDOWN_FLAG.load(Ordering::SeqCst)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_flag_is_clear() {
        SHUTDOWN_FLAG.store(false, Ordering::SeqCst);
        SIGNAL_COUNT.store(0, Ordering::SeqCst);
        let handle = ShutdownHandle(());
        assert!(!handle.is_shutdown());
        assert!(!is_shutdown());
    }

    #[test]
    fn the_flag_is_shared_by_every_clone_and_the_free_function() {
        SHUTDOWN_FLAG.store(false, Ordering::SeqCst);
        let handle = ShutdownHandle(());
        let clone = handle.clone();
        SHUTDOWN_FLAG.store(true, Ordering::SeqCst);
        assert!(handle.is_shutdown());
        assert!(clone.is_shutdown());
        assert!(is_shutdown());
        SHUTDOWN_FLAG.store(false, Ordering::SeqCst);
    }
}
