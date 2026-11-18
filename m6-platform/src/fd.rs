//! Descriptor I/O a logger needs and `std` does not offer: a raw `write(2)`
//! with no buffering or locking, a non-blocking flag, and the `dup` family a
//! test uses to redirect standard output. POSIX on every supported target, so
//! there is no arm.

use std::io;
use std::os::fd::{AsFd, OwnedFd};

/// One `write(2)` on `fd`: the bytes the kernel took, `WouldBlock` when a
/// non-blocking descriptor is full, `Interrupted` when a signal cut the call.
/// No buffering and no lock, which is why a logger that must never wait uses
/// it in place of `std::io::Write`.
pub fn write(fd: &impl AsFd, bytes: &[u8]) -> io::Result<usize> {
    Ok(nix::unistd::write(fd, bytes)?)
}

/// One `read(2)` on `fd`: the bytes the kernel gave, `WouldBlock` when a
/// non-blocking descriptor has none, `Interrupted` when a signal cut the call.
/// So a loop that reads another process's pipe never waits on it.
pub fn read(fd: &impl AsFd, bytes: &mut [u8]) -> io::Result<usize> {
    Ok(nix::unistd::read(fd, bytes)?)
}

/// Set or clear `O_NONBLOCK` on `fd` and return whether it was set before, so
/// the caller can put it back.
pub fn set_nonblocking(fd: &impl AsFd, on: bool) -> io::Result<bool> {
    use nix::fcntl::{fcntl, FcntlArg, OFlag};
    let flags = OFlag::from_bits_truncate(fcntl(fd, FcntlArg::F_GETFL)?);
    let was = flags.contains(OFlag::O_NONBLOCK);
    let wanted = if on {
        flags | OFlag::O_NONBLOCK
    } else {
        flags & !OFlag::O_NONBLOCK
    };
    if wanted != flags {
        fcntl(fd, FcntlArg::F_SETFL(wanted))?;
    }
    Ok(was)
}

/// Whether `fd` carries `O_NONBLOCK`.
pub fn is_nonblocking(fd: &impl AsFd) -> io::Result<bool> {
    use nix::fcntl::{fcntl, FcntlArg, OFlag};
    let flags = OFlag::from_bits_truncate(fcntl(fd, FcntlArg::F_GETFL)?);
    Ok(flags.contains(OFlag::O_NONBLOCK))
}

/// A duplicate of `fd`.
pub fn dup(fd: &impl AsFd) -> io::Result<OwnedFd> {
    Ok(nix::unistd::dup(fd)?)
}

/// Make standard output a duplicate of `fd`: a test redirects a process's
/// output to a pipe or a file, and restores it from a saved [`dup`].
pub fn dup2_stdout(fd: &impl AsFd) -> io::Result<()> {
    nix::unistd::dup2_stdout(fd)?;
    Ok(())
}

/// A pipe: the read end, then the write end.
pub fn pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    Ok(nix::unistd::pipe()?)
}

/// Whether a failed send on a non-blocking socket is a full receiver that
/// will drain, so the same bytes may be sent again later.
///
/// `EAGAIN` and `EINTR` on every system, and `ENOBUFS`, which is what macOS
/// returns for a unix datagram socket whose receiver's queue is full where
/// Linux returns `EAGAIN`.
pub fn send_would_retry(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
    ) || error.raw_os_error() == Some(nix::errno::Errno::ENOBUFS as i32)
}
