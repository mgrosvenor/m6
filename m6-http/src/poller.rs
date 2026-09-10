/// Platform-abstracted I/O readiness poller.
///
/// Uses epoll on Linux, kqueue on macOS/FreeBSD/OpenBSD,
/// and falls back to poll(2) on all other Unix platforms.

use std::io;
use std::os::unix::io::RawFd;

/// Opaque token returned with each ready event — assigned by the caller.
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub struct Token(pub u32);

// ─────────────────────────────────────────────────────────────────────────────
// Linux: epoll
// ─────────────────────────────────────────────────────────────────────────────
#[cfg(target_os = "linux")]
mod imp {
    use super::Token;
    use std::io;
    use std::os::unix::io::RawFd;

    pub struct Imp {
        pub epfd: RawFd,
    }

    pub fn new() -> io::Result<Imp> {
        let fd = unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Imp { epfd: fd })
    }

    pub fn add(imp: &Imp, fd: RawFd, token: Token) -> io::Result<()> {
        let mut ev = libc::epoll_event {
            events: (libc::EPOLLIN | libc::EPOLLRDHUP) as u32,
            u64: token.0 as u64,
        };
        let r = unsafe { libc::epoll_ctl(imp.epfd, libc::EPOLL_CTL_ADD, fd, &mut ev) };
        if r < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    pub fn delete(imp: &Imp, fd: RawFd) -> io::Result<()> {
        // Linux 2.6.9+ allows null event pointer for DEL
        let r = unsafe {
            libc::epoll_ctl(imp.epfd, libc::EPOLL_CTL_DEL, fd, std::ptr::null_mut())
        };
        if r < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    pub fn wait(
        imp: &Imp,
        events: &mut [Token; 64],
        timeout_ms: i32,
    ) -> io::Result<usize> {
        let mut raw = [libc::epoll_event { events: 0, u64: 0 }; 64];
        let n = match sigmask {
            Some(mask) => unsafe {
                libc::epoll_wait(imp.epfd, raw.as_mut_ptr(), 64, timeout_ms)
            },
            None => unsafe { libc::epoll_wait(imp.epfd, raw.as_mut_ptr(), 64, timeout_ms) },
        };
        if n < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                return Ok(0);
            }
            return Err(e);
        }
        for i in 0..n as usize {
            events[i] = Token(raw[i].u64 as u32);
        }
        Ok(n as usize)
    }

    pub fn drop_imp(imp: &Imp) {
        unsafe { libc::close(imp.epfd) };
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// macOS / FreeBSD / OpenBSD: kqueue
// ─────────────────────────────────────────────────────────────────────────────
#[cfg(any(
    target_os = "macos",
    target_os = "freebsd",
    target_os = "openbsd"
))]
mod imp {
    use super::Token;
    use std::io;
    use std::os::unix::io::RawFd;

    pub struct Imp {
        pub kqfd: RawFd,
    }

    pub fn new() -> io::Result<Imp> {
        let fd = unsafe { libc::kqueue() };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Imp { kqfd: fd })
    }

    pub fn add(imp: &Imp, fd: RawFd, token: Token) -> io::Result<()> {
        let ev = libc::kevent {
            ident: fd as libc::uintptr_t,
            filter: libc::EVFILT_READ,
            flags: libc::EV_ADD | libc::EV_ENABLE,
            fflags: 0,
            data: 0,
            udata: token.0 as *mut libc::c_void,
        };
        let r = unsafe {
            libc::kevent(imp.kqfd, &ev, 1, std::ptr::null_mut(), 0, std::ptr::null())
        };
        if r < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    pub fn delete(imp: &Imp, fd: RawFd) -> io::Result<()> {
        let ev = libc::kevent {
            ident: fd as libc::uintptr_t,
            filter: libc::EVFILT_READ,
            flags: libc::EV_DELETE,
            fflags: 0,
            data: 0,
            udata: std::ptr::null_mut(),
        };
        let r = unsafe {
            libc::kevent(imp.kqfd, &ev, 1, std::ptr::null_mut(), 0, std::ptr::null())
        };
        if r < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    pub fn wait(
        imp: &Imp,
        events: &mut [Token; 64],
        timeout_ms: i32,
    ) -> io::Result<usize> {
        let ts;
        let ts_ptr = if timeout_ms < 0 {
            std::ptr::null()
        } else {
            ts = libc::timespec {
                tv_sec: (timeout_ms / 1000) as libc::time_t,
                tv_nsec: ((timeout_ms % 1000) * 1_000_000) as libc::c_long,
            };
            &ts as *const _
        };
        let mut raw = [unsafe { std::mem::zeroed::<libc::kevent>() }; 64];
        let n = unsafe {
            libc::kevent(imp.kqfd, std::ptr::null(), 0, raw.as_mut_ptr(), 64, ts_ptr)
        };
        if n < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                return Ok(0);
            }
            return Err(e);
        }
        for i in 0..n as usize {
            events[i] = Token(raw[i].udata as u32);
        }
        Ok(n as usize)
    }

    pub fn drop_imp(imp: &Imp) {
        unsafe { libc::close(imp.kqfd) };
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Fallback: poll(2)
// ─────────────────────────────────────────────────────────────────────────────
#[cfg(not(any(
    target_os = "linux",
    target_os = "macos",
    target_os = "freebsd",
    target_os = "openbsd"
)))]
mod imp {
    use super::Token;
    use std::io;
    use std::os::unix::io::RawFd;
    use std::sync::Mutex;

    pub struct Imp {
        pub fds: Mutex<Vec<(RawFd, Token)>>,
    }

    pub fn new() -> io::Result<Imp> {
        Ok(Imp { fds: Mutex::new(Vec::new()) })
    }

    pub fn add(imp: &Imp, fd: RawFd, token: Token) -> io::Result<()> {
        imp.fds.lock().unwrap().push((fd, token));
        Ok(())
    }

    pub fn delete(imp: &Imp, fd: RawFd) -> io::Result<()> {
        imp.fds.lock().unwrap().retain(|(f, _)| *f != fd);
        Ok(())
    }

    pub fn wait(
        imp: &Imp,
        events: &mut [Token; 64],
        timeout_ms: i32,
    ) -> io::Result<usize> {
        let registered = imp.fds.lock().unwrap().clone();
        if registered.is_empty() {
            // Nothing to poll — sleep briefly and return 0
            if timeout_ms > 0 {
                std::thread::sleep(std::time::Duration::from_millis(timeout_ms as u64));
            }
            return Ok(0);
        }
        let mut poll_fds: Vec<libc::pollfd> = registered
            .iter()
            .map(|(fd, _)| libc::pollfd { fd: *fd, events: libc::POLLIN, revents: 0 })
            .collect();
        let ret = unsafe { libc::poll(poll_fds.as_mut_ptr(), poll_fds.len() as libc::nfds_t, timeout_ms) };
        if ret < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                return Ok(0);
            }
            return Err(e);
        }
        let mut count = 0usize;
        for (i, pfd) in poll_fds.iter().enumerate() {
            if (pfd.revents & libc::POLLIN) != 0 {
                if count < 64 {
                    events[count] = registered[i].1;
                    count += 1;
                }
            }
        }
        Ok(count)
    }

    pub fn drop_imp(_imp: &Imp) {}
}

// ─────────────────────────────────────────────────────────────────────────────
// Public API
// ─────────────────────────────────────────────────────────────────────────────

/// Platform-abstracted I/O readiness poller.
pub struct Poller(imp::Imp);

impl Poller {
    /// Create a new poller.
    pub fn new() -> io::Result<Self> {
        imp::new().map(Poller)
    }

    /// Register `fd` for READABLE events with the given token.
    pub fn add(&self, fd: RawFd, token: Token) -> io::Result<()> {
        imp::add(&self.0, fd, token)
    }

    /// A self-pipe for waking a blocked `wait()` from another thread.
    ///
    /// Register the read end with the poller and keep the writer. Writing one
    /// byte makes the next `wait()` return immediately, whatever timeout it was
    /// given.
    ///
    /// This exists so shutdown works the same way here as in every other m6
    /// service. The alternative previously used was `epoll_pwait` with a
    /// signal mask, which is **Linux only**: the kqueue path ignores the mask
    /// entirely, so on macOS the race it was meant to close was still open.
    /// A self-pipe is portable, needs no code to run in signal context, and is
    /// the same "wake the blocked thread" hook `m6-core`'s shutdown handling
    /// offers everyone else.
    ///
    /// Two file descriptors, created once at startup. Nothing per request.
    pub fn wake_pipe() -> io::Result<(WakeReader, WakeWriter)> {
        let mut fds = [0 as RawFd; 2];
        // SAFETY: fds is a valid two-element array for pipe(2) to fill.
        let rc = unsafe { libc::pipe(fds.as_mut_ptr()) };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
        for fd in fds {
            // SAFETY: fd was just returned by pipe(2).
            unsafe {
                let flags = libc::fcntl(fd, libc::F_GETFL, 0);
                libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
                let fdflags = libc::fcntl(fd, libc::F_GETFD, 0);
                libc::fcntl(fd, libc::F_SETFD, fdflags | libc::FD_CLOEXEC);
            }
        }
        Ok((WakeReader(fds[0]), WakeWriter(fds[1])))
    }

    /// Deregister `fd` (call before closing to avoid stale events).
    pub fn delete(&self, fd: RawFd) -> io::Result<()> {
        imp::delete(&self.0, fd)
    }

    /// Wait for events. Fills `events` slice, returns count.
    ///
    /// `timeout_ms`: -1 = block forever, 0 = non-blocking, >0 = ms timeout.
    ///
    /// Interrupt a blocked wait with `wake_pipe`; see that function.
    pub fn wait(&self, events: &mut [Token; 64], timeout_ms: i32) -> io::Result<usize> {
        imp::wait(&self.0, events, timeout_ms)
    }
}

impl Drop for Poller {
    fn drop(&mut self) {
        imp::drop_imp(&self.0);
    }
}


/// Read end of the wake pipe. Register with `Poller::add`.
pub struct WakeReader(RawFd);

impl WakeReader {
    #[inline]
    pub fn as_raw_fd(&self) -> RawFd {
        self.0
    }

    /// Drain whatever was written. Called after the poller reports the wake
    /// token so a single byte cannot spin the loop.
    #[inline]
    pub fn drain(&self) {
        let mut buf = [0u8; 64];
        // SAFETY: self.0 is a valid non-blocking read end we own.
        while unsafe { libc::read(self.0, buf.as_mut_ptr().cast(), buf.len()) } > 0 {}
    }
}

impl Drop for WakeReader {
    fn drop(&mut self) {
        // SAFETY: we own this descriptor.
        unsafe { libc::close(self.0) };
    }
}

/// Write end of the wake pipe. `Send + Sync` so a shutdown hook on another
/// thread can poke it.
pub struct WakeWriter(RawFd);

// SAFETY: a raw fd is just an integer, and write(2) on a pipe is atomic for
// counts below PIPE_BUF. Nothing here mutates Rust-side state.
unsafe impl Send for WakeWriter {}
unsafe impl Sync for WakeWriter {}

impl WakeWriter {
    /// Wake the poller. Ignores a full pipe: if bytes are already pending, the
    /// wake this one would have caused is already going to happen.
    #[inline]
    pub fn wake(&self) {
        let byte = 1u8;
        // SAFETY: self.0 is a valid non-blocking write end we own.
        unsafe { libc::write(self.0, std::ptr::addr_of!(byte).cast(), 1) };
    }
}

impl Drop for WakeWriter {
    fn drop(&mut self) {
        // SAFETY: we own this descriptor.
        unsafe { libc::close(self.0) };
    }
}
