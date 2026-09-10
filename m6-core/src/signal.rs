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
///
/// # The ordering rule, and why it is enforced
///
/// [`block`] must be the first statement of `main`, before anything creates a
/// thread. Blocking a signal is per-thread: `pthread_sigmask` changes only the
/// calling thread, and threads inherit the mask **at creation**. A
/// process-directed signal is delivered to any thread that does not block it,
/// so one unblocked thread anywhere in the process is enough to take the
/// default action, which for SIGTERM is death.
///
/// That is not hypothetical. Every m6 service initialised logging first, and
/// `tracing_appender::non_blocking` spawns a writer thread. By the time
/// `install_with_hooks` blocked the signals in `main`, that writer thread had
/// existed for a hundred lines and had SIGTERM unblocked. The kernel delivered
/// every SIGTERM to it, so `m6-file` exited 143 on `systemctl stop` rather than
/// running its shutdown path, and never unlinked its socket. The `sigwait`
/// thread was correct and never received a signal in its life.
///
/// [`install_with_hooks`](ShutdownHandle::install_with_hooks) now refuses to
/// start unless [`block`] has already run, because the failure is silent and
/// only shows up as a service that will not stop cleanly.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use nix::sys::signal::{SigSet, Signal};

/// Global shutdown state. Global rather than per-handle so every clone, and
/// every thread, observes the same value.
static SHUTDOWN_FLAG: AtomicBool = AtomicBool::new(false);
static SIGNAL_COUNT: AtomicUsize = AtomicUsize::new(0);

/// The signals this module owns.
fn managed() -> SigSet {
    let mut mask = SigSet::empty();
    mask.add(Signal::SIGTERM);
    mask.add(Signal::SIGINT);
    mask
}

/// Block SIGTERM and SIGINT in the calling thread.
///
/// **This must be the first statement of `main`.** Not the first interesting
/// statement: the first one, before logging, before any pool, before anything
/// that could spawn a thread. Threads inherit the signal mask as it stands
/// when they are created, and a single thread created before this call is
/// enough to make the process die on SIGTERM instead of shutting down.
///
/// Idempotent, so calling it again from
/// [`install_with_hooks`](ShutdownHandle::install_with_hooks) costs nothing.
pub fn block() {
    let _ = managed().thread_block();
}

/// Whether the calling thread currently blocks the signals this module owns.
fn blocked_here() -> bool {
    match SigSet::thread_get_mask() {
        Ok(m) => m.contains(Signal::SIGTERM) && m.contains(Signal::SIGINT),
        // If the mask cannot be read, do not turn that into a startup failure.
        Err(_) => true,
    }
}

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
        assert!(
            blocked_here(),
            "m6_core::signal::block() must be the first statement of main, before \
             anything spawns a thread. Without it the signal mask is not inherited \
             by threads that already exist (the tracing-appender writer, for one), \
             the kernel delivers SIGTERM to one of them, and the process dies at \
             the default disposition instead of shutting down."
        );
        // Idempotent: block() has already run, but a service that installs from
        // a thread other than main still needs the mask set here.
        block();

        std::thread::Builder::new()
            .name("m6-signal".into())
            .spawn(move || {
                let wait_mask = managed();
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

    /// The ordering rule is enforced, not merely documented.
    ///
    /// Each of these sets its own precondition explicitly rather than relying
    /// on the ambient mask, because under `--test-threads=1` every test in the
    /// file runs on the same thread and a mask set by one would otherwise leak
    /// into the next.
    #[test]
    #[should_panic(expected = "must be the first statement of main")]
    fn installing_without_blocking_first_is_refused() {
        let _ = managed().thread_unblock();
        let _ = ShutdownHandle::install();
    }

    #[test]
    fn block_blocks_both_signals_this_module_owns() {
        let _ = managed().thread_unblock();
        assert!(!blocked_here(), "precondition: signals start unblocked here");
        block();
        assert!(blocked_here());
        let _ = managed().thread_unblock();
    }

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
