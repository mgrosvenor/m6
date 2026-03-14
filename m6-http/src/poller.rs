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
        sigmask: Option<&libc::sigset_t>,
    ) -> io::Result<usize> {
        let mut raw = [libc::epoll_event { events: 0, u64: 0 }; 64];
        let n = match sigmask {
            Some(mask) => unsafe {
                libc::epoll_pwait(imp.epfd, raw.as_mut_ptr(), 64, timeout_ms, mask)
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
        _sigmask: Option<&libc::sigset_t>,
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
        _sigmask: Option<&libc::sigset_t>,
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

    /// Deregister `fd` (call before closing to avoid stale events).
    pub fn delete(&self, fd: RawFd) -> io::Result<()> {
        imp::delete(&self.0, fd)
    }

    /// Wait for events. Fills `events` slice, returns count.
    ///
    /// `timeout_ms`: -1 = block forever, 0 = non-blocking, >0 = ms timeout.
    ///
    /// On Linux, `sigmask` is passed to `epoll_pwait` so that signals are
    /// delivered atomically during the wait, eliminating the SIGTERM polling race.
    /// On other platforms `sigmask` is ignored.
    pub fn wait(
        &self,
        events: &mut [Token; 64],
        timeout_ms: i32,
        sigmask: Option<&libc::sigset_t>,
    ) -> io::Result<usize> {
        imp::wait(&self.0, events, timeout_ms, sigmask)
    }
}

impl Drop for Poller {
    fn drop(&mut self) {
        imp::drop_imp(&self.0);
    }
}
