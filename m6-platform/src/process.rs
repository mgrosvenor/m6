//! Facts about a process: its open-file limit, its threads and descriptors,
//! whether it is alive, whether it listens on TCP, whether it runs as root.
//! The counts are what a test measures a service with from outside. And the
//! one way a service spawns a child, so the child is not affected by the
//! service's signal handling.

use std::fs::File;
use std::io;
use std::process::{Command, ExitStatus, Output};

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

/// The size of one memory page in bytes: 4 KiB on Linux x86_64, 16 KiB on
/// macOS arm64. A power of two. The unit large buffers are sized in.
/// Start only: a service reads it once, when it builds its limits.
pub fn page_size() -> usize {
    use nix::unistd::{sysconf, SysconfVar};
    match sysconf(SysconfVar::PAGE_SIZE) {
        Ok(Some(n)) if n > 0 => usize::try_from(n).unwrap_or(4096),
        // The call cannot fail for this name on either family. 4 KiB is the
        // smallest page any supported CPU uses, and every larger one is a
        // multiple of it.
        _ => 4096,
    }
}

/// Pin every page this process has faulted, and every page it faults later, in
/// RAM, so the kernel never swaps one out and a hot page is never a page fault.
/// Start only, after the heap is reserved and before serving.
///
/// Pages are pinned as they are touched and not before: memory a service has
/// reserved and not used is not resident, and a page is locked the first time it
/// is faulted, so the hot path still takes no fault once it is warm. Where
/// memory cannot be pinned the error names the limit and the fix. On macOS
/// there is nothing to ask for and the call does nothing: it returns `Ok`, and
/// the process's memory is as the kernel manages it.
///
/// Linux: `mlockall(MCL_CURRENT | MCL_FUTURE | MCL_ONFAULT)`, which needs
/// `RLIMIT_MEMLOCK` unlimited and a kernel of 4.4 or newer.
pub fn lock_memory() -> io::Result<()> {
    os::lock_memory()
}

/// Ask the kernel to back `len` bytes at `start` with transparent huge pages.
///
/// Where transparent huge pages are in `madvise` mode, which is the common
/// default, only a range advised this way gets them; in `always` mode the call
/// is harmless, and in `never` mode it has no effect. A hint, never a promise:
/// [`huge_page_bytes`] says what the kernel gave. Call it on a range this
/// process owns and has not freed, before the range is first touched. Does
/// nothing on macOS.
pub fn advise_huge_pages(start: *mut u8, len: usize) -> io::Result<()> {
    os::advise_huge_pages(start, len)
}

/// Bytes of this process's memory that sit on huge pages now, explicit or
/// transparent. Zero where the kernel gave none, and on macOS, which has no
/// such report. A service reads it once after reserving its heap, to log which
/// kind of page it got.
pub fn huge_page_bytes() -> usize {
    os::huge_page_bytes()
}

/// The size of this process's address space, in bytes, or `None` where the
/// platform keeps no such report (macOS). Read with [`resident_bytes`] at start:
/// the gap between the two is memory that is mapped and not touched, which is
/// what a reservation is.
pub fn virtual_bytes() -> Option<usize> {
    os::virtual_bytes()
}

/// This process's resident set, in bytes: the pages the kernel has in RAM for
/// it. `None` where the platform keeps no such report (macOS).
pub fn resident_bytes() -> Option<usize> {
    os::resident_bytes()
}

/// The soft and hard open-file limits of this process.
pub fn fd_limits() -> io::Result<(u64, u64)> {
    let (soft, hard) = nix::sys::resource::getrlimit(nix::sys::resource::Resource::RLIMIT_NOFILE)?;
    Ok((soft, hard))
}

/// Where one of a child's standard streams goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Io {
    /// The parent's own descriptor for that stream.
    Inherit,
    /// `/dev/null`.
    Null,
    /// A pipe, whose parent end is the matching field of [`Child`].
    Piped,
}

/// The child's three standard streams, the only descriptors it is given.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Streams {
    pub stdin: Io,
    pub stdout: Io,
    pub stderr: Io,
}

/// A child started by [`spawn`].
///
/// The same calls as `std::process::Child`. Dropping it neither kills nor
/// reaps the process, as with `std`, so an owner that wants it gone calls
/// [`Child::kill`] and [`Child::wait`].
#[derive(Debug)]
pub struct Child {
    pid: u32,
    status: Option<ExitStatus>,
    /// The parent's end of a piped stdin.
    pub stdin: Option<File>,
    /// The parent's end of a piped stdout.
    pub stdout: Option<File>,
    /// The parent's end of a piped stderr.
    pub stderr: Option<File>,
}

impl Child {
    /// The child's process id.
    pub fn id(&self) -> u32 {
        self.pid
    }

    /// Send SIGKILL. `Ok` when the child has already been reaped.
    pub fn kill(&mut self) -> io::Result<()> {
        if self.status.is_some() {
            return Ok(());
        }
        let pid = nix::unistd::Pid::from_raw(self.pid as i32);
        match nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGKILL) {
            Ok(()) | Err(nix::errno::Errno::ESRCH) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    /// Wait for the child to exit and reap it. Closes a piped stdin first, so a
    /// child that reads to the end is not waiting on this process.
    pub fn wait(&mut self) -> io::Result<ExitStatus> {
        drop(self.stdin.take());
        if let Some(status) = self.status {
            return Ok(status);
        }
        let status = reap(self.pid, 0)?.expect("a blocking wait returns a status");
        self.status = Some(status);
        Ok(status)
    }

    /// The exit status when the child has exited, reaping it, and `None` while
    /// it runs.
    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        if let Some(status) = self.status {
            return Ok(Some(status));
        }
        let status = reap(self.pid, libc::WNOHANG)?;
        self.status = status;
        Ok(status)
    }

    /// Read every byte of the piped stdout and stderr, then wait. The two are
    /// read together, so a child that fills one pipe while this process reads
    /// the other cannot stall either.
    pub fn wait_with_output(mut self) -> io::Result<Output> {
        drop(self.stdin.take());
        let (stdout, stderr) = read_both(self.stdout.take(), self.stderr.take())?;
        let status = self.wait()?;
        Ok(Output {
            status,
            stdout,
            stderr,
        })
    }
}

/// `waitpid` on one child, retried on `EINTR`. `None` only with `WNOHANG` and a
/// child still running.
fn reap(pid: u32, flags: libc::c_int) -> io::Result<Option<ExitStatus>> {
    use std::os::unix::process::ExitStatusExt;
    let mut raw: libc::c_int = 0;
    loop {
        // SAFETY: `raw` is a valid out-pointer for the call's duration, and the
        // pid is this process's own child.
        let got = unsafe { libc::waitpid(pid as libc::pid_t, &mut raw, flags) };
        if got == -1 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e);
        }
        if got == 0 {
            return Ok(None);
        }
        return Ok(Some(ExitStatus::from_raw(raw)));
    }
}

/// Read two optional pipes to their end, polling both so neither blocks the
/// other.
fn read_both(out: Option<File>, err: Option<File>) -> io::Result<(Vec<u8>, Vec<u8>)> {
    use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
    use std::io::Read;
    use std::os::fd::AsFd;
    let mut pipes = [(out, Vec::new()), (err, Vec::new())];
    let mut buf = [0u8; 8192];
    loop {
        let open: Vec<usize> = (0..2).filter(|&i| pipes[i].0.is_some()).collect();
        if open.is_empty() {
            break;
        }
        let ready: Vec<usize> = {
            let mut fds: Vec<PollFd> = open
                .iter()
                .map(|&i| {
                    PollFd::new(
                        pipes[i].0.as_ref().expect("open").as_fd(),
                        PollFlags::POLLIN,
                    )
                })
                .collect();
            match poll(&mut fds, PollTimeout::NONE) {
                Ok(_) => {}
                Err(nix::errno::Errno::EINTR) => continue,
                Err(e) => return Err(e.into()),
            }
            open.iter()
                .zip(fds.iter())
                .filter(|(_, fd)| fd.revents().is_some_and(|r| !r.is_empty()))
                .map(|(&i, _)| i)
                .collect()
        };
        for i in ready {
            let (pipe, data) = &mut pipes[i];
            let file = pipe.as_mut().expect("open");
            match file.read(&mut buf) {
                Ok(0) => *pipe = None,
                Ok(n) => data.extend_from_slice(&buf[..n]),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
    }
    let [(_, out), (_, err)] = pipes;
    Ok((out, err))
}

/// Spawn `cmd` as every m6 process must: with no signal blocked or ignored in
/// the child, and no descriptor of this process but the three `streams`.
///
/// `cmd` gives the program, its arguments, environment changes and working
/// directory. Its own stdio settings are not read: `streams` is the one place
/// the child's standard streams are chosen.
///
/// A service blocks SIGTERM and SIGINT for its own loop
/// ([`super::Signals`]), and on macOS ignores them as well. A child inherits
/// both across `exec` on every supported target, so the child starts with an
/// empty mask and the default disposition for every managed signal and SIGPIPE.
///
/// **No other descriptor reaches the child, whichever thread opened it and
/// whatever its close-on-exec flag.** A descriptor left open across `exec`
/// outlives the test or service that owned it, in a server it started: a test's
/// listener held that way keeps its port bound, and connections to the port
/// land in a process that never accepts them. On macOS a socket is not created
/// close-on-exec. The flag is set by a second call, so another thread's fork
/// between the two inherits it. macOS therefore spawns with `posix_spawn` and
/// `POSIX_SPAWN_CLOEXEC_DEFAULT`, which closes everything the spawn does not
/// name. Linux creates every descriptor close-on-exec in one call, and the
/// child also marks every descriptor above stderr close-on-exec with
/// `close_range` before it runs the program, so a descriptor something cleared
/// the flag on is not inherited either.
pub fn spawn(cmd: &mut Command, streams: Streams) -> io::Result<Child> {
    let (pid, [stdin, stdout, stderr]) = os::spawn(cmd, streams)?;
    Ok(Child {
        pid,
        status: None,
        stdin,
        stdout,
        stderr,
    })
}

/// Mark every descriptor above stderr close-on-exec, so no child inherits it.
/// Once, at start, while the process is one thread: `block()` does it for a
/// service, and a test harness does it before its first spawn.
///
/// This covers a child started by anything other than [`spawn`], such as a
/// `std::process::Command` run directly. [`spawn`] itself needs no sweep: it
/// gives the child its three standard streams and nothing else. A descriptor
/// opened later by Rust is close-on-exec on Linux, where the flag is set in the
/// same call that creates it. On macOS a socket gets the flag in a second call,
/// so a fork by another thread between the two inherits it, which is why
/// [`spawn`] uses `posix_spawn` there.
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

/// The effective user's name.
///
/// A start-time error tells the operator who should own a directory the service
/// will not create, so it names this user. Falls back to the numeric id when
/// the name cannot be read, the one fact always available.
pub fn user() -> String {
    nix::unistd::User::from_uid(nix::unistd::Uid::effective())
        .ok()
        .flatten()
        .map_or_else(|| uid().to_string(), |user| user.name)
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

/// The idle worker threads the host's certificate store leaves in a process
/// after it has been read, as `(min, max)`.
///
/// The range is a platform fact, not a measurement: macOS reads the store
/// through the Security framework, whose libdispatch workers stay behind
/// (three or four, swinging between starts), while Linux reads the store's
/// files and leaves no thread. A caller that counts a service's threads adds
/// this range to the threads the service makes itself.
pub fn certificate_store_workers() -> (usize, usize) {
    os::certificate_store_workers()
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

/// One process's memory as the kernel accounts it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcessMemory {
    /// The resident set in bytes: the pages the kernel holds in RAM for it.
    pub resident_bytes: u64,
    /// The address space in bytes: every mapping, resident or not.
    pub virtual_bytes: u64,
    /// The bytes pinned in RAM by `mlock`, `mlockall` or `mlock2`.
    pub locked_bytes: u64,
}

/// The memory the kernel accounts to `pid`, read from outside the process.
///
/// `Err` for a pid that is gone, and on a platform whose kernel keeps no such
/// report for another process, so a check that cannot measure fails rather
/// than reading zero. On Linux it is `VmRSS`, `VmSize` and `VmLck` of
/// `/proc/<pid>/status`. macOS keeps no equivalent, so it is `Err` there.
pub fn memory_usage(pid: u32) -> io::Result<ProcessMemory> {
    os::memory_usage(pid)
}

/// The minor page faults `pid` has taken. A test warms a service and then
/// requires this not to move, which is how it proves the hot path took no page
/// it had not already touched. `Err` where the kernel keeps no such report.
pub fn page_faults(pid: u32) -> io::Result<u64> {
    os::page_faults(pid)
}
