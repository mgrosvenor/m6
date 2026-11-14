//! Facts about a process: its open-file limit, its threads and descriptors,
//! whether it is alive, whether it listens on TCP, whether it runs as root.
//! The counts are what a test measures a service with from outside. And the
//! one way a service spawns a child, so the child is not affected by the
//! service's signal handling.

use std::io;
use std::process::{Child, Command};

use super::os;

/// Descriptors [`raise_fd_limit`] reserves out of the limit: the listener, the
/// signal descriptor, the poll, stdio and a few more.
pub const FD_MARGIN: usize = 16;

/// Raise the soft open-file limit as high as the kernel allows and return the
/// number of descriptors left for connections: the limit set, less
/// [`FD_MARGIN`]. The connection table's ceiling. Start only.
///
/// A second call returns the same number. The soft limit afterwards is never
/// above the hard limit. On Linux the soft limit becomes the hard limit. On
/// macOS the hard limit reads as unlimited and the kernel enforces
/// `kern.maxfilesperproc`, so the soft limit becomes the lower of the two.
pub fn raise_fd_limit() -> io::Result<usize> {
    use nix::sys::resource::{getrlimit, setrlimit, Resource};
    let (_, hard) = getrlimit(Resource::RLIMIT_NOFILE)?;
    let soft = match os::max_open_files()? {
        Some(cap) => hard.min(cap),
        None => hard,
    };
    setrlimit(Resource::RLIMIT_NOFILE, soft, hard)?;
    Ok(usize::try_from(soft.saturating_sub(FD_MARGIN as u64)).unwrap_or(usize::MAX))
}

/// The soft and hard open-file limits of this process.
pub fn fd_limits() -> io::Result<(u64, u64)> {
    let (soft, hard) = nix::sys::resource::getrlimit(nix::sys::resource::Resource::RLIMIT_NOFILE)?;
    Ok((soft, hard))
}

/// Spawn `cmd` as every m6 process must: with no signal blocked or ignored in
/// the child, and no descriptor of this process above stderr.
///
/// A service blocks SIGTERM, SIGINT and SIGUSR1 for its own loop
/// ([`super::Signals`]), and on macOS ignores them as well. A child inherits
/// both across `exec` on every supported target, so a child spawned with
/// `Command::spawn` alone would ignore the SIGTERM that stops it. This clears
/// the mask and restores the three default dispositions between fork and
/// exec, which is the one thing `pre_exec` is for here.
///
/// A child also inherits every descriptor of this process that is not marked
/// close-on-exec, and a test's lock file or a listener held that way outlives
/// the test in a server it started. [`close_inherited_on_exec`], run once at
/// start by `block()` (and by a test harness before its first spawn), marks
/// them, so the child starts with its three standard streams and whatever
/// `cmd` set up for it. The child stays in this process's group, so an
/// interrupt from a terminal reaches it as well.
pub fn spawn(cmd: &mut Command) -> io::Result<Child> {
    use nix::sys::signal::{signal, SigHandler, SigSet};
    use std::os::unix::process::CommandExt;
    // SAFETY: the closure runs in the child between fork and exec and calls
    // only `pthread_sigmask` and `sigaction`, both async-signal-safe. It
    // installs `SIG_DFL`, which is no handler code, allocates nothing and
    // takes no lock.
    unsafe {
        cmd.pre_exec(|| {
            SigSet::empty().thread_set_mask()?;
            for sig in super::managed().iter() {
                signal(sig, SigHandler::SigDfl)?;
            }
            Ok(())
        });
    }
    cmd.spawn()
}

/// Mark every descriptor above stderr close-on-exec, so no child inherits it.
/// Once, at start, while the process is one thread: `block()` does it for a
/// service, and a test harness does it before its first spawn. A descriptor
/// opened later by Rust is close-on-exec already.
pub fn close_inherited_on_exec() -> io::Result<()> {
    use nix::fcntl::{fcntl, FcntlArg, FdFlag};
    use std::os::fd::BorrowedFd;
    for entry in std::fs::read_dir("/dev/fd")? {
        let Ok(fd) = entry?.file_name().to_string_lossy().parse::<i32>() else {
            continue;
        };
        if fd <= 2 {
            continue;
        }
        // SAFETY: the number was read from this process's own descriptor table
        // a moment ago, and the flag is set without the borrow outliving the
        // call. A descriptor closed in between fails the fcntl, which is
        // ignored: it is not inherited either way.
        let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };
        let _ = fcntl(borrowed, FcntlArg::F_SETFD(FdFlag::FD_CLOEXEC));
    }
    Ok(())
}

/// The effective user's numeric id.
///
/// The test kit names its shared port-claim directory with this, because a
/// port belongs to the host and the marker files have to be writable by
/// whoever claims them.
pub fn uid() -> u32 {
    nix::unistd::Uid::effective().as_raw()
}

/// Whether this process runs as root.
pub fn is_root() -> bool {
    nix::unistd::Uid::effective().is_root()
}

/// Whether a process with this pid exists and may be signalled. A child that
/// has exited and not been reaped still exists.
pub fn is_alive(pid: u32) -> bool {
    nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), None).is_ok()
}

/// How many threads the process `pid` runs. `Err` for a pid that is gone.
///
/// On Linux it is the entries of `/proc/<pid>/task`. On macOS it is
/// `proc_pidinfo` with `PROC_PIDLISTTHREADS`, one id per thread.
pub fn thread_count(pid: u32) -> io::Result<usize> {
    os::thread_count(pid)
}

/// How many descriptors the process `pid` holds open. `Err` for a pid that is
/// gone.
///
/// On Linux it is the entries of `/proc/<pid>/fd`. On macOS it is
/// `proc_pidinfo` with `PROC_PIDLISTFDS`.
pub fn open_descriptors(pid: u32) -> io::Result<usize> {
    os::open_descriptors(pid)
}

/// Whether the process `pid` has a TCP socket in the listening state. `Err`
/// when the question cannot be answered, so a check that cannot measure fails.
///
/// On Linux it is the socket inodes of `/proc/<pid>/fd` against
/// `/proc/net/tcp` and `tcp6`. On macOS it is `lsof -nP -a -p <pid> -iTCP
/// -sTCP:LISTEN`, and a missing `lsof` is the `Err`.
pub fn listens_on_tcp(pid: u32) -> io::Result<bool> {
    os::listens_on_tcp(pid)
}
