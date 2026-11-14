//! The platform layer: the only crate in m6 that knows which operating system
//! it runs on. m6-core re-exports it as `m6_core::platform`.
//!
//! Everything a service needs from the kernel is here, stated in the service's
//! terms: readiness ([`Poll`]), the clock ([`now`] and [`wall`]),
//! a shutdown or reload request ([`Signals`]), a child's exit
//! ([`ChildExit`]), sockets with the right options ([`net`]), facts about a
//! process and the host ([`process`], [`host`]), the open-file limit, the
//! unix socket path limit and the directories a service and its tests use.
//!
//! One implementation file per OS family holds the code that differs:
//! `src/linux.rs` and `src/macos.rs`. They are selected once, below,
//! by family and never by CPU, so Linux on arm64 and macOS on x86_64 build the
//! same code. A family with no file fails to build with a message naming the
//! file a port adds. No other crate, test or service names `mio`, `nix`,
//! `rustix`, `libc` or a `cfg(target_os)`: `tests/platform_gate.rs` refuses
//! them.
//!
//! Every behaviour the layer promises is held by one shared test suite,
//! `tests/platform_contract.rs` and `tests/platform_signals.rs`, run unchanged
//! on every supported target. `docs/m6-platform.md` states each contract, how
//! each OS meets it and what a port would do.

use std::io;
use std::os::fd::{BorrowedFd, RawFd};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
use linux as os;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
use macos as os;

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
compile_error!(
    "the m6 platform layer has no implementation for this OS family. A port is \
     one file: copy m6-platform/src/macos.rs to m6-platform/src/<family>.rs, \
     implement every pub(super) item it lists, and select it at the top of \
     m6-platform/src/lib.rs."
);

pub mod fd;
pub mod host;
pub mod net;
pub mod process;

// ── Readiness ────────────────────────────────────────────────────────────────

pub use mio::event::{Event, Source};
pub use mio::{Events, Interest, Token};

/// The reactor: one wait over every registered descriptor.
///
/// # The contract
///
/// 1. An event is delivered for every new readiness edge on an interest the
///    caller registered for that token: a peer write, a completed connect,
///    buffer space freed after a write returned `WouldBlock`, a peer close.
/// 2. A token appears at most once in one wait, with its readable and writable
///    bits combined.
/// 3. [`Registry::reregister`] always issues the system call, so a source that
///    is ready at that moment is reported on the next wait even when the
///    interest did not change. This is what makes the loop rule "drive, then
///    re-read interest, then reregister" sufficient on every kernel.
/// 4. A peer close is readable, and the first read then returns `Ok(0)`.
/// 5. The poll is edge-triggered: a source is reported once per change and not
///    while data waits, so each turn reads and writes until `WouldBlock`.
/// 6. The loop reads [`now`] once per wake and passes that time to every
///    deadline and log line in the turn, so a turn sees one time.
///
/// What the layer does not promise: that a ready source is never reported
/// twice without a new edge. On macOS, kqueue re-arms a writable filter when
/// the peer's acknowledgement frees send-buffer space, and Linux epoll does
/// not. A loop that writes until `WouldBlock` pays one failed write for the
/// repeat and is otherwise unaffected.
///
/// An interrupted wait (`ErrorKind::Interrupted`) is passed up unchanged on
/// every target: a loop treats it as "wait again".
///
/// On Linux this is epoll in edge-triggered mode. On macOS it is kqueue with
/// `EV_CLEAR`. Both through `mio`, which no code outside this layer names.
pub struct Poll {
    inner: mio::Poll,
    registry: Registry,
}

impl Poll {
    pub fn new() -> io::Result<Poll> {
        let inner = mio::Poll::new()?;
        // The same reactor under a second descriptor, so the handle is a value
        // this struct owns and `registry()` is a plain borrow. Start only.
        let registry = Registry(inner.registry().try_clone()?);
        Ok(Poll { inner, registry })
    }

    /// The handle sources are registered with. A loop hands it to the
    /// functions that drive its connections.
    #[inline]
    pub fn registry(&self) -> &Registry {
        &self.registry
    }

    /// Wait for readiness, at most `timeout` (`None` waits until something is
    /// ready). On return `events` holds what is ready.
    #[inline]
    pub fn poll(&mut self, events: &mut Events, timeout: Option<Duration>) -> io::Result<()> {
        self.inner.poll(events, timeout)
    }
}

/// Where sources are registered. Obtained from [`Poll::registry`].
pub struct Registry(mio::Registry);

impl Registry {
    /// Watch `source` for `interest` under `token`.
    #[inline]
    pub fn register(
        &self,
        source: &mut impl Source,
        token: Token,
        interest: Interest,
    ) -> io::Result<()> {
        self.0.register(source, token, interest)
    }

    /// Change what `source` is watched for. Always issues the system call, so a
    /// source that is ready now is reported on the next wait (contract rule 3).
    #[inline]
    pub fn reregister(
        &self,
        source: &mut impl Source,
        token: Token,
        interest: Interest,
    ) -> io::Result<()> {
        self.0.reregister(source, token, interest)
    }

    /// Stop watching `source`. Nothing is reported for it afterwards.
    #[inline]
    pub fn deregister(&self, source: &mut impl Source) -> io::Result<()> {
        self.0.deregister(source)
    }

    /// Watch a bare descriptor: a signal descriptor, a child's exit descriptor,
    /// a pipe or a socket a connection type holds by number.
    #[inline]
    pub fn register_fd(&self, fd: RawFd, token: Token, interest: Interest) -> io::Result<()> {
        self.0
            .register(&mut mio::unix::SourceFd(&fd), token, interest)
    }

    /// As [`Registry::reregister`], for a bare descriptor.
    #[inline]
    pub fn reregister_fd(&self, fd: RawFd, token: Token, interest: Interest) -> io::Result<()> {
        self.0
            .reregister(&mut mio::unix::SourceFd(&fd), token, interest)
    }

    /// As [`Registry::deregister`], for a bare descriptor.
    #[inline]
    pub fn deregister_fd(&self, fd: RawFd) -> io::Result<()> {
        self.0.deregister(&mut mio::unix::SourceFd(&fd))
    }
}

/// Block until `fd` is readable or `timeout` passes. `Ok(true)` when it is
/// readable. For a blocking client, such as the benchmark tools, that waits on
/// one socket at a time and must not pay a reactor per wait. Never in a loop.
pub fn wait_readable(fd: BorrowedFd<'_>, timeout: Duration) -> io::Result<bool> {
    let deadline = Instant::now() + timeout;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        // One `poll` takes at most 65535 ms: longer waits go round again.
        let ms = u16::try_from(left.as_millis()).unwrap_or(u16::MAX);
        let mut fds = [nix::poll::PollFd::new(fd, nix::poll::PollFlags::POLLIN)];
        match nix::poll::poll(&mut fds, nix::poll::PollTimeout::from(ms)) {
            Ok(0) if left.as_millis() > u128::from(u16::MAX) => continue,
            Ok(0) => return Ok(false),
            Ok(_) => return Ok(true),
            Err(nix::errno::Errno::EINTR) => continue,
            Err(e) => return Err(e.into()),
        }
    }
}

// ── Time ─────────────────────────────────────────────────────────────────────

/// The monotonic clock, for deadlines. A loop reads it once per wake and
/// passes that one time to every deadline and log line in the turn.
///
/// Never goes backwards. On Linux it is `CLOCK_MONOTONIC`, on macOS
/// `CLOCK_UPTIME_RAW`. Both stop while the machine is suspended, so a deadline
/// set before a laptop sleeps fires after it wakes, late by the sleep's length,
/// on every target. Code never subtracts one [`wall`] reading from another to
/// measure an interval.
#[inline]
pub fn now() -> Instant {
    Instant::now()
}

/// The wall clock, for log timestamps and the HTTP `Date` header. It can jump
/// when the system's time is set. On Linux it is `CLOCK_REALTIME`, on macOS
/// `gettimeofday`.
#[inline]
pub fn wall() -> SystemTime {
    SystemTime::now()
}

// ── Signals: a shutdown or reload request ────────────────────────────────────

/// A request that arrived as a signal.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Signal {
    /// Stop: SIGTERM or SIGINT.
    Shutdown,
    /// Reopen the log file: SIGUSR1, the convention a rotation tool uses.
    /// Not SIGHUP, which by convention re-reads a configuration, which m6
    /// never does, and which also arrives when a terminal closes.
    ReopenLogs,
}

/// SIGTERM, SIGINT and SIGUSR1 as one readable descriptor, registered with
/// [`Registry::register_fd`].
///
/// [`Signals::new`] asserts [`block`] already ran, because a thread started
/// before the mask is set takes the signal at its default disposition. The
/// three signals stay blocked for the life of the process. A child inherits
/// that mask across `exec`, so a service spawns children through
/// [`process::spawn`], which clears it.
///
/// On Linux the descriptor is a `signalfd`, whose read dequeues the signal. On
/// macOS it is a kqueue registered for `EVFILT_SIGNAL`, which reports a posted
/// signal and does not dequeue it, so the three are also set to `SIG_IGN`
/// there and are discarded once reported.
pub struct Signals(os::Signals);

impl Signals {
    pub fn new() -> io::Result<Signals> {
        require_blocked();
        Ok(Signals(os::Signals::new()?))
    }

    /// The readable descriptor.
    pub fn raw_fd(&self) -> RawFd {
        self.0.raw_fd()
    }

    /// One pending request, or `None` when there are no more. The caller loops
    /// until `None`: the descriptor is edge-triggered, so every pending request
    /// is drained on one wake. A burst of one kind may coalesce, and none is
    /// lost.
    pub fn read(&mut self) -> Option<Signal> {
        self.0.read()
    }
}

/// Send `what` to the process `pid`, as a test or a supervisor does: SIGTERM
/// for [`Signal::Shutdown`], SIGUSR1 for [`Signal::ReopenLogs`].
pub fn send(pid: u32, what: Signal) -> io::Result<()> {
    let sig = match what {
        Signal::Shutdown => nix::sys::signal::Signal::SIGTERM,
        Signal::ReopenLogs => REOPEN_SIGNAL,
    };
    nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), sig)?;
    Ok(())
}

/// The signal that reopens the log file.
const REOPEN_SIGNAL: nix::sys::signal::Signal = nix::sys::signal::Signal::SIGUSR1;

/// The three signals the layer owns, as a mask.
fn managed() -> nix::sys::signal::SigSet {
    use nix::sys::signal::Signal as Sig;
    let mut set = nix::sys::signal::SigSet::empty();
    set.add(Sig::SIGTERM);
    set.add(Sig::SIGINT);
    set.add(REOPEN_SIGNAL);
    set
}

/// Map a signal number to the request it carries.
fn signal_from_signo(signo: i32) -> Option<Signal> {
    use nix::sys::signal::Signal as Sig;
    match Sig::try_from(signo).ok()? {
        Sig::SIGTERM | Sig::SIGINT => Some(Signal::Shutdown),
        Sig::SIGUSR1 => Some(Signal::ReopenLogs),
        _ => None,
    }
}

/// Block the three signals in the calling thread, and mark every descriptor
/// this process inherited close-on-exec. The first statement of every `main`,
/// under the name `m6_core::signal::block`, which delegates here: both are
/// facts about the process that are set once, while it is still one thread.
pub fn block() {
    let _ = managed().thread_block();
    let _ = process::close_inherited_on_exec();
}

/// Unblock the three signals in the calling thread, so a test can prove the
/// refusal [`require_blocked`] gives. Never called by a service.
pub fn unblock_for_test() {
    let _ = managed().thread_unblock();
}

/// Whether the calling thread blocks the three signals.
fn blocked_here() -> bool {
    match nix::sys::signal::SigSet::thread_get_mask() {
        Ok(m) => managed().iter().all(|s| m.contains(s)),
        Err(_) => false,
    }
}

/// Assert the calling thread blocks the signals, with the one reason every
/// refusal shares.
pub fn require_blocked() {
    assert!(
        blocked_here(),
        "m6_core::signal::block() must be the first statement of main, before \
         anything spawns a thread. Without it the signal mask is not inherited \
         by threads that already exist (the tracing-appender writer, for one), \
         the kernel delivers SIGTERM to one of them, and the process dies at \
         the default disposition instead of shutting down."
    );
}

// ── A child process's exit ───────────────────────────────────────────────────

/// A child's exit as a readable descriptor, so a loop never calls `wait(2)`.
///
/// The descriptor becomes readable when the child exits, at once when it
/// already has. A loop deregisters it on that report and then calls
/// [`ChildExit::reap`], which collects the status without blocking. The order
/// matters: on Linux reaping changes the descriptor's state and epoll reports
/// it once more, so a descriptor left registered is reported for a child
/// that is gone. Dropping it closes the descriptor and does not reap.
///
/// On Linux it is a `pidfd`. On macOS it is a kqueue registered for
/// `EVFILT_PROC` with `NOTE_EXIT`, or, when the child has already exited and
/// the kernel refuses the registration, a kqueue with one triggered user event
/// so it is readable at once.
pub struct ChildExit {
    pid: u32,
    inner: os::ChildExit,
}

impl ChildExit {
    pub fn new(child: &std::process::Child) -> io::Result<ChildExit> {
        let pid = child.id();
        Ok(ChildExit {
            pid,
            inner: os::ChildExit::new(pid)?,
        })
    }

    /// The readable descriptor.
    pub fn raw_fd(&self) -> RawFd {
        self.inner.raw_fd()
    }

    /// The child's exit status. Call only once the descriptor was reported
    /// readable. The report can come a moment before the kernel makes the
    /// child waitable (macOS posts `NOTE_EXIT` first, and a child whose
    /// output has closed may still be exiting when the exit is armed), so
    /// this waits for that moment, which is bounded by the exit already
    /// under way. Called before the child exits, it blocks until it does.
    pub fn reap(&mut self) -> io::Result<std::process::ExitStatus> {
        use nix::sys::wait::{waitpid, WaitStatus};
        use std::os::unix::process::ExitStatusExt;
        let pid = nix::unistd::Pid::from_raw(self.pid as i32);
        match waitpid(pid, None)? {
            WaitStatus::Exited(_, code) => Ok(ExitStatusExt::from_raw(code << 8)),
            WaitStatus::Signaled(_, sig, _) => Ok(ExitStatusExt::from_raw(sig as i32)),
            other => Err(io::Error::other(format!(
                "child {} reported {other:?}, not an exit",
                self.pid
            ))),
        }
    }

    /// A start-time probe: `Ok` when this kernel can report a child's exit as
    /// a descriptor. On Linux that needs `pidfd_open`, kernel 5.3 or newer. A
    /// service that will spawn children calls it at start and exits 2 on `Err`.
    pub fn supported() -> io::Result<()> {
        os::child_exit_supported()
    }
}

// ── Paths and directories ────────────────────────────────────────────────────

/// The longest unix socket path the kernel accepts, in bytes, excluding the
/// terminating NUL: 108 on Linux, 104 on macOS.
pub const MAX_SOCKET_PATH: usize = os::MAX_SOCKET_PATH;

/// `Err` naming the path and the limit when `path` is too long to bind.
/// Called by every constructor in [`net`] that binds or connects a unix socket,
/// so the failure is at start and says what is wrong.
pub fn check_socket_path(path: &Path) -> io::Result<()> {
    let len = path.as_os_str().len();
    if len >= MAX_SOCKET_PATH {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "socket path {} is {len} bytes, and this platform allows at most {} \
                 (sockaddr_un::sun_path): use a shorter path",
                path.display(),
                MAX_SOCKET_PATH - 1
            ),
        ));
    }
    Ok(())
}

/// Where a service's unix sockets live in production: `/run/m6`. A test sets
/// `M6_SOCKET_OVERRIDE` instead, which `m6_core::server::socket_path_from_config`
/// honours.
pub fn runtime_dir() -> PathBuf {
    PathBuf::from("/run/m6")
}

/// The temporary directory: `TMPDIR` when set, else the system's. Short enough
/// on every supported target for a socket under a fresh subdirectory to fit
/// [`MAX_SOCKET_PATH`].
pub fn temp_dir() -> PathBuf {
    std::env::temp_dir()
}

/// A directory a test can compile into and then execute from.
///
/// `/tmp` is not guaranteed to permit exec, and a hardened host mounts it
/// `noexec`. `/var/tmp` is disk-backed and not among the mounts hardening
/// restricts, so it is used when it exists, and the temporary directory
/// otherwise. Not for sockets: a path under here is longer than one under
/// [`temp_dir`].
pub fn scratch_dir() -> PathBuf {
    let var_tmp = PathBuf::from("/var/tmp");
    if var_tmp.is_dir() {
        return var_tmp;
    }
    temp_dir()
}

/// Create `file` as a private file: its parent with mode 0700 when the parent
/// is created here, the file with 0600, and a file left by an earlier start
/// tightened to 0600. A parent that already exists keeps its mode, because it
/// is not this process's to change.
pub fn create_private(file: &Path) -> io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
    if let Some(dir) = file.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
    }
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(file)?;
    std::fs::set_permissions(file, std::fs::Permissions::from_mode(0o600))
}
