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
/// until the next connection arrives. [`Service::socket`] exists to poke it
/// awake, by connecting to the service's own socket.
///
/// # One sequence, for every service
///
/// [`ShutdownHandle::install`] is the only entry point and it takes a
/// [`Service`]. The sequence it runs is the same everywhere: log that the
/// signal arrived, wake the parked loop, and on the way out unlink the socket
/// and log a completion line. What a service supplies is data, not a different
/// code path.
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
/// `install` blocked the signals in `main`, that writer thread had
/// existed for a hundred lines and had SIGTERM unblocked. The kernel delivered
/// every SIGTERM to it, so `m6-file` exited 143 on `systemctl stop` rather than
/// running its shutdown path, and never unlinked its socket. The `sigwait`
/// thread was correct and never received a signal in its life.
///
/// [`ShutdownHandle::install`] now refuses to start unless [`block`] has
/// already run, because the failure is silent and only shows up as a service
/// that will not stop cleanly.

use std::os::unix::io::RawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

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
/// Idempotent, so calling it again from [`ShutdownHandle::install`] costs
/// nothing.
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

/// What one m6 service needs in order to shut down.
///
/// **This is the whole of the per-service surface, and it is data.** There
/// used to be three install functions, `install`, `install_with_wake` and
/// `install_with_hooks`, which is three shutdown sequences with a shared
/// signal thread rather than one shutdown sequence. The services diverged
/// accordingly: one of five unlinked its socket, two of five logged a
/// completion line, and there were three different ways of waking a parked
/// loop. None of that divergence was demanded by anything the services do.
///
/// Shutdown is now identical everywhere and the differences are these fields.
pub struct Service {
    /// Used in the shutdown log lines, so `journalctl -u <unit> | grep
    /// shutdown` means the same thing for every service.
    ///
    /// Owned rather than `&'static str` because `m6-render` hosts three
    /// different processes (`m6-html`, `render-contact`, `render-analytics`)
    /// and has to take the name from `argv[0]` at startup. One allocation, at
    /// startup, is not worth a lifetime for.
    pub name: String,

    /// The unix socket this service listens on, if it has one.
    ///
    /// Two jobs, both of which used to be per-service. It is connected to on
    /// the first signal, which returns a thread parked in `accept()`
    /// immediately instead of leaving it there until the next real request.
    /// And it is **unlinked on every exit path**, graceful and forced alike,
    /// because a socket file left behind keeps a dead member in m6-http's
    /// backend pool until its next rescan.
    ///
    /// Only `m6-file` did either of those. `m6-render`'s only `remove_file`
    /// was at startup, clearing a stale socket before `bind`, which is the
    /// workaround for the missing cleanup rather than the cleanup.
    pub socket: Option<PathBuf>,

    /// The write end of a pipe this service's poller watches, if it parks in
    /// `epoll`/`kqueue` rather than `accept()`.
    ///
    /// One byte is written to it on the first signal, which returns the
    /// poller immediately. `m6-http` is the only service that parks this way.
    /// A file descriptor rather than a closure because that is all the
    /// difference amounts to, and core can do the write.
    pub wake_fd: Option<RawFd>,
}

impl Service {
    /// A service with no unix socket and no wake pipe.
    pub fn new(name: impl Into<String>) -> Self {
        Service { name: name.into(), socket: None, wake_fd: None }
    }

    /// Set the unix socket to wake through and unlink.
    pub fn socket(mut self, path: impl Into<PathBuf>) -> Self {
        self.socket = Some(path.into());
        self
    }

    /// Set the wake pipe for a loop that does not park in `accept()`.
    ///
    /// The caller keeps the pipe open for the life of the process.
    pub fn wake_fd(mut self, fd: RawFd) -> Self {
        self.wake_fd = Some(fd);
        self
    }
}

/// Unblock whatever the service's main loop is parked in.
fn wake(socket: Option<&Path>, wake_fd: Option<RawFd>) {
    if let Some(path) = socket {
        // Return a thread parked in accept(). The connection carries no
        // request and is closed immediately; every m6 accept loop re-checks
        // the shutdown flag on wake and exits without reading it.
        let _ = std::os::unix::net::UnixStream::connect(path);
    }
    if let Some(fd) = wake_fd {
        // Return a poller parked in epoll/kqueue. A full pipe is fine: bytes
        // already pending mean the wake is already going to happen.
        let byte = 1u8;
        // SAFETY: the caller owns this fd and keeps it open for the process.
        unsafe { libc::write(fd, std::ptr::addr_of!(byte).cast(), 1) };
    }
}

/// Remove the socket file and log the completion line.
fn finish(svc_name: &str, socket: Option<&Path>, how: &str) {
    if let Some(path) = socket {
        let _ = std::fs::remove_file(path);
    }
    tracing::info!("{svc_name} shutdown {how}");
}

/// Shared shutdown state installed once at process start.
///
/// First SIGTERM/SIGINT sets the shutdown flag (graceful).
/// Second SIGTERM/SIGINT unlinks the socket and exits immediately with 0.
///
/// The handle is `Clone` so it can be shared across threads. All clones
/// observe the same global flag.
#[derive(Clone)]
pub struct ShutdownHandle {
    name: Arc<str>,
    socket: Option<Arc<PathBuf>>,
}

impl ShutdownHandle {
    /// Install signal handling for one service.
    ///
    /// Call once at startup. [`block`] must already have run as the first
    /// statement of `main`; this asserts it, because the failure it prevents
    /// is otherwise silent.
    pub fn install(service: Service) -> Self {
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

        let Service { name, socket, wake_fd } = service;
        let name: Arc<str> = Arc::from(name);
        let socket = socket.map(Arc::new);

        let thread_socket = socket.clone();
        let thread_name = Arc::clone(&name);
        std::thread::Builder::new()
            .name("m6-signal".into())
            .spawn(move || {
                let wait_mask = managed();
                let _ = wait_mask.thread_block();
                let sock = thread_socket.as_deref().map(|p| p.as_path());
                let name = &*thread_name;

                loop {
                    match wait_mask.wait() {
                        Ok(_sig) => {
                            let count = SIGNAL_COUNT.fetch_add(1, Ordering::SeqCst) + 1;
                            if count >= 2 {
                                finish(name, sock, "forced");
                                std::process::exit(0);
                            }
                            // Log BEFORE publishing the flag, not after.
                            //
                            // The flag is what releases the main thread: it
                            // sees `is_shutdown()`, drains, logs "shutdown
                            // complete" and returns from `main`, and process
                            // exit discards whatever is still queued in
                            // `tracing_appender`'s non-blocking writer. With
                            // the store first, this line was racing the entire
                            // drain, so a fast service could exit with
                            // "shutdown complete" written and "shutdown signal
                            // received" lost.
                            //
                            // That is the whole of
                            // `redirect_lifecycle::sigterm_shuts_down_rather_than_being_ignored`,
                            // which failed intermittently for weeks and never
                            // reproduced in isolation: the redirect listener
                            // gets from start to complete in about 20ms, so
                            // under load the appender thread is simply
                            // scheduled too late. Captured 2026-09-12 with the
                            // journal showing `started`, the listener line, and
                            // `shutdown complete` -- and nothing in between.
                            //
                            // Ordering the other way does not make the log
                            // durable, it makes it *ordered*: the main thread
                            // cannot observe the flag until this call has
                            // returned, so the line is queued ahead of the
                            // completion line rather than concurrently with it.
                            tracing::info!("{name} shutdown signal received");
                            SHUTDOWN_FLAG.store(true, Ordering::SeqCst);
                            wake(sock, wake_fd);
                        }
                        Err(_) => break,
                    }
                }
            })
            .expect("spawning the signal thread");

        let handle = ShutdownHandle { name, socket };
        handle.log_started();
        handle
    }

    /// `"<name> started"`, with the socket when there is one.
    ///
    /// Emitted by `install`, which every service calls once it is bound and
    /// ready to serve. Startup used to be logged as unevenly as shutdown: two
    /// services said "starting" and never "started", `m6-md` said nothing at
    /// all, and all three render apps logged `"m6-render started"` rather than
    /// their own name. Apps still log their own domain fields (route counts,
    /// thread pool size, bind address); the lifecycle line is core's, so it
    /// reads the same for every unit.
    fn log_started(&self) {
        match self.socket.as_deref() {
            Some(path) => tracing::info!(socket = %path.display(), "{} started", self.name),
            None => tracing::info!("{} started", self.name),
        }
    }

    /// Finish a graceful shutdown: unlink the socket, log the completion line.
    ///
    /// Call once, at the end of `main`, after the service loop has drained.
    /// Every service says the same thing, so `grep 'shutdown complete'` is a
    /// uniform signal across the fleet rather than a per-app accident.
    pub fn complete(&self) {
        finish(&self.name, self.socket.as_deref().map(|p| p.as_path()), "complete");
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

    /// Serialises the tests that read and write [`SHUTDOWN_FLAG`].
    ///
    /// **That flag is one `static` for the whole process, and `cargo test` runs
    /// the tests in a binary on several threads at once.** So
    /// `the_flag_is_shared_by_every_clone_and_the_free_function`, which stores
    /// `true` and then restores `false`, and `a_fresh_flag_is_clear`, which
    /// asserts the flag is `false`, were two threads writing and reading one
    /// global with nothing between them. Whenever the store landed inside the
    /// other test's window, the assert failed.
    ///
    /// It failed once in a full-workspace run on 2026-09-12 and not once in
    /// 300 runs of this module on its own, because the window is a couple of
    /// atomic stores wide and only a loaded machine schedules the two threads
    /// far enough apart. **The narrowness is why it read as noise, and it was
    /// never noise:** widening each test's window by 50 ms reproduces
    /// `assertion failed: !handle.is_shutdown()` every single time.
    ///
    /// The module comment below already worried about `--test-threads=1`
    /// letting a signal mask leak between tests. A thread mask is per-thread
    /// and parallel tests each get their own, so that direction was safe. This
    /// is the opposite direction, and it is the one that bites.
    static FLAG_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Take [`FLAG_LOCK`], surviving a panic in a previous holder.
    ///
    /// A poisoned mutex here means some earlier test panicked, which the
    /// harness has already reported. Refusing to run the rest because of it
    /// would turn one failure into several.
    fn flag_guard() -> std::sync::MutexGuard<'static, ()> {
        FLAG_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

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
        let _ = ShutdownHandle::install(Service::new("test"));
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
        let _guard = flag_guard();
        SHUTDOWN_FLAG.store(false, Ordering::SeqCst);
        SIGNAL_COUNT.store(0, Ordering::SeqCst);
        let handle = ShutdownHandle { name: Arc::from("test"), socket: None };
        assert!(!handle.is_shutdown());
        assert!(!is_shutdown());
    }

    #[test]
    fn the_flag_is_shared_by_every_clone_and_the_free_function() {
        let _guard = flag_guard();
        SHUTDOWN_FLAG.store(false, Ordering::SeqCst);
        let handle = ShutdownHandle { name: Arc::from("test"), socket: None };
        let clone = handle.clone();
        SHUTDOWN_FLAG.store(true, Ordering::SeqCst);
        assert!(handle.is_shutdown());
        assert!(clone.is_shutdown());
        assert!(is_shutdown());
        SHUTDOWN_FLAG.store(false, Ordering::SeqCst);
    }
}
