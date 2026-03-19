/// Cross-platform file-change notifier.
///
/// `ConfigWatcher` watches a set of file paths (by monitoring their parent
/// directories) and signals when any of the watched files change.
///
/// On Linux: uses raw libc inotify syscalls.
/// On macOS/FreeBSD/OpenBSD: uses kqueue EVFILT_VNODE via a self-pipe and
///   background threads (one per unique parent directory).
/// Fallback: no-op; `raw_fd()` returns `None`, `read_events` always returns
///   false.

use std::os::unix::io::RawFd;
use std::path::Path;

// ─────────────────────────────────────────────────────────────────────────────
// Linux: inotify
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(target_os = "linux")]
pub struct ConfigWatcher {
    inotify_fd: RawFd,
}

#[cfg(target_os = "linux")]
impl ConfigWatcher {
    pub fn new(paths: &[&Path]) -> anyhow::Result<Self> {
        use std::collections::HashSet;
        use std::ffi::CString;

        let fd = unsafe { libc::inotify_init1(libc::IN_CLOEXEC | libc::IN_NONBLOCK) };
        if fd < 0 {
            anyhow::bail!("inotify_init1 failed: {}", std::io::Error::last_os_error());
        }

        let mask = libc::IN_CLOSE_WRITE | libc::IN_CREATE | libc::IN_MOVED_TO;
        let mut watched: HashSet<std::path::PathBuf> = HashSet::new();

        for path in paths {
            let dir = path.parent().unwrap_or(Path::new("/"));
            if !dir.exists() {
                tracing::warn!(dir = %dir.display(), "watch directory does not exist, skipping");
                continue;
            }
            if watched.insert(dir.to_path_buf()) {
                match CString::new(dir.to_string_lossy().as_bytes()) {
                    Ok(cstr) => {
                        let wd = unsafe { libc::inotify_add_watch(fd, cstr.as_ptr(), mask) };
                        if wd < 0 {
                            tracing::warn!(
                                dir = %dir.display(),
                                error = %std::io::Error::last_os_error(),
                                "inotify_add_watch failed"
                            );
                        }
                    }
                    Err(_) => {
                        tracing::warn!(dir = %dir.display(), "directory path contains interior NUL, skipping");
                    }
                }
            }
        }

        Ok(ConfigWatcher { inotify_fd: fd })
    }

    pub fn raw_fd(&self) -> Option<RawFd> {
        Some(self.inotify_fd)
    }

    pub fn read_events(&mut self, filenames: &[&str]) -> bool {
        let mut buf = [0u8; 4096];
        let mut matched = false;
        loop {
            let n = unsafe {
                libc::read(self.inotify_fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len())
            };
            if n <= 0 {
                break;
            }
            let n = n as usize;
            let mut offset = 0usize;
            while offset + std::mem::size_of::<libc::inotify_event>() <= n {
                let event =
                    unsafe { &*(buf.as_ptr().add(offset) as *const libc::inotify_event) };
                let name_len = event.len as usize;
                if name_len > 0 {
                    let name_start = offset + std::mem::size_of::<libc::inotify_event>();
                    let name_end = name_start + name_len;
                    if name_end <= n {
                        let name = std::ffi::CStr::from_bytes_until_nul(&buf[name_start..name_end])
                            .ok()
                            .and_then(|s| s.to_str())
                            .unwrap_or("");
                        if filenames.iter().any(|f| *f == name) {
                            matched = true;
                        }
                    }
                }
                offset += std::mem::size_of::<libc::inotify_event>() + name_len;
            }
        }
        matched
    }
}

#[cfg(target_os = "linux")]
impl Drop for ConfigWatcher {
    fn drop(&mut self) {
        unsafe { libc::close(self.inotify_fd) };
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// macOS / FreeBSD / OpenBSD: kqueue EVFILT_VNODE + self-pipe
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(any(target_os = "macos", target_os = "freebsd", target_os = "openbsd"))]
pub struct ConfigWatcher {
    /// Read end of self-pipe — returned from raw_fd(), registered with poller.
    pipe_read: RawFd,
    /// Write end of self-pipe — written by background watcher threads.
    pipe_write: RawFd,
    /// Background threads kept alive for the lifetime of this struct.
    _threads: Vec<std::thread::JoinHandle<()>>,
}

#[cfg(any(target_os = "macos", target_os = "freebsd", target_os = "openbsd"))]
impl ConfigWatcher {
    pub fn new(paths: &[&Path]) -> anyhow::Result<Self> {
        use std::collections::HashSet;

        let mut pipe_fds = [0i32; 2];
        if unsafe { libc::pipe(pipe_fds.as_mut_ptr()) } < 0 {
            anyhow::bail!("pipe() failed: {}", std::io::Error::last_os_error());
        }
        let pipe_read = pipe_fds[0];
        let pipe_write = pipe_fds[1];
        for &fd in &[pipe_read, pipe_write] {
            unsafe { libc::fcntl(fd, libc::F_SETFL, libc::O_NONBLOCK) };
        }

        let mut watched_dirs: HashSet<std::path::PathBuf> = HashSet::new();
        let mut threads = Vec::new();

        // Collect dirs and individual files.
        let mut file_paths: Vec<std::path::PathBuf> = Vec::new();
        for path in paths {
            let dir = path.parent().unwrap_or(Path::new("/"));
            if !dir.exists() {
                tracing::warn!(dir = %dir.display(), "watch directory does not exist, skipping");
                continue;
            }
            if watched_dirs.insert(dir.to_path_buf()) {
                let dir_owned = dir.to_path_buf();
                let pw = pipe_write;
                match std::thread::Builder::new()
                    .name(format!("m6-kqueue-watcher({})", dir.display()))
                    .spawn(move || kqueue_watch_dir(dir_owned, pw))
                {
                    Ok(handle) => threads.push(handle),
                    Err(e) => {
                        tracing::warn!(
                            dir = %dir.display(),
                            error = %e,
                            "failed to spawn kqueue watcher thread"
                        );
                    }
                }
            }
            if path.exists() {
                file_paths.push(path.to_path_buf());
            }
        }

        // Also watch each individual file directly with NOTE_ATTRIB so that
        // `touch <file>` (mtime change only, no directory entry change) fires.
        if !file_paths.is_empty() {
            let pw = pipe_write;
            match std::thread::Builder::new()
                .name("m6-kqueue-watcher(files)".to_string())
                .spawn(move || kqueue_watch_files(file_paths, pw))
            {
                Ok(handle) => threads.push(handle),
                Err(e) => {
                    tracing::warn!(error = %e, "failed to spawn kqueue file watcher thread");
                }
            }
        }

        Ok(ConfigWatcher { pipe_read, pipe_write, _threads: threads })
    }

    pub fn raw_fd(&self) -> Option<RawFd> {
        Some(self.pipe_read)
    }

    /// Drain the self-pipe. Returns true if any bytes were read (i.e. the
    /// directory changed). The `filenames` argument is ignored on this
    /// platform (directory-level granularity only).
    pub fn read_events(&mut self, _filenames: &[&str]) -> bool {
        let mut buf = [0u8; 64];
        let mut any = false;
        loop {
            let n = unsafe {
                libc::read(self.pipe_read, buf.as_mut_ptr() as *mut libc::c_void, buf.len())
            };
            if n <= 0 {
                break;
            }
            any = true;
        }
        any
    }
}

#[cfg(any(target_os = "macos", target_os = "freebsd", target_os = "openbsd"))]
impl Drop for ConfigWatcher {
    fn drop(&mut self) {
        unsafe {
            libc::close(self.pipe_read);
            libc::close(self.pipe_write);
        }
    }
}

/// Watch individual files for attribute changes (mtime) using kqueue EVFILT_VNODE NOTE_ATTRIB.
/// This catches `touch <file>` which doesn't trigger a directory-level event.
#[cfg(any(target_os = "macos", target_os = "freebsd", target_os = "openbsd"))]
fn kqueue_watch_files(paths: Vec<std::path::PathBuf>, pipe_write: RawFd) {
    let kq = unsafe { libc::kqueue() };
    if kq < 0 {
        return;
    }

    let mut fds: Vec<RawFd> = Vec::new();
    for path in &paths {
        if let Ok(cstr) = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()) {
            let fd = unsafe { libc::open(cstr.as_ptr(), libc::O_EVTONLY) };
            if fd >= 0 {
                let ev = libc::kevent {
                    ident: fd as libc::uintptr_t,
                    filter: libc::EVFILT_VNODE,
                    flags: libc::EV_ADD | libc::EV_ENABLE | libc::EV_CLEAR,
                    fflags: (libc::NOTE_WRITE | libc::NOTE_ATTRIB | libc::NOTE_RENAME
                        | libc::NOTE_DELETE) as u32,
                    data: 0,
                    udata: std::ptr::null_mut(),
                };
                unsafe { libc::kevent(kq, &ev, 1, std::ptr::null_mut(), 0, std::ptr::null()) };
                fds.push(fd);
            }
        }
    }

    let timeout = libc::timespec { tv_sec: 1, tv_nsec: 0 };
    let mut out_ev = unsafe { std::mem::zeroed::<libc::kevent>() };

    loop {
        let n = unsafe { libc::kevent(kq, std::ptr::null(), 0, &mut out_ev, 1, &timeout) };
        if n > 0 {
            let byte: u8 = 1;
            unsafe {
                libc::write(pipe_write, &byte as *const u8 as *const libc::c_void, 1);
            }
        }
    }
}

/// Watch `dir` for any file writes/creates/deletes using kqueue EVFILT_VNODE.
/// Writes a byte to `pipe_write` on each event to wake the main thread.
#[cfg(any(target_os = "macos", target_os = "freebsd", target_os = "openbsd"))]
fn kqueue_watch_dir(dir: std::path::PathBuf, pipe_write: RawFd) {
    let kq = unsafe { libc::kqueue() };
    if kq < 0 {
        return;
    }

    let path = match std::ffi::CString::new(dir.as_os_str().as_encoded_bytes()) {
        Ok(s) => s,
        Err(_) => {
            unsafe { libc::close(kq) };
            return;
        }
    };

    let dir_fd = unsafe { libc::open(path.as_ptr(), libc::O_EVTONLY) };
    if dir_fd < 0 {
        unsafe { libc::close(kq) };
        return;
    }

    let ev = libc::kevent {
        ident: dir_fd as libc::uintptr_t,
        filter: libc::EVFILT_VNODE,
        flags: libc::EV_ADD | libc::EV_ENABLE | libc::EV_CLEAR,
        fflags: (libc::NOTE_WRITE | libc::NOTE_EXTEND | libc::NOTE_ATTRIB | libc::NOTE_LINK)
            as u32,
        data: 0,
        udata: std::ptr::null_mut(),
    };
    unsafe { libc::kevent(kq, &ev, 1, std::ptr::null_mut(), 0, std::ptr::null()) };

    let timeout = libc::timespec { tv_sec: 1, tv_nsec: 0 };
    let mut out_ev = unsafe { std::mem::zeroed::<libc::kevent>() };

    loop {
        let n =
            unsafe { libc::kevent(kq, std::ptr::null(), 0, &mut out_ev, 1, &timeout) };
        if n > 0 {
            let byte: u8 = 1;
            unsafe {
                libc::write(pipe_write, &byte as *const u8 as *const libc::c_void, 1);
            }
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Fallback (no-op)
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(not(any(
    target_os = "linux",
    target_os = "macos",
    target_os = "freebsd",
    target_os = "openbsd"
)))]
pub struct ConfigWatcher {}

#[cfg(not(any(
    target_os = "linux",
    target_os = "macos",
    target_os = "freebsd",
    target_os = "openbsd"
)))]
impl ConfigWatcher {
    pub fn new(_paths: &[&Path]) -> anyhow::Result<Self> {
        Ok(ConfigWatcher {})
    }

    pub fn raw_fd(&self) -> Option<RawFd> {
        None
    }

    pub fn read_events(&mut self, _filenames: &[&str]) -> bool {
        false
    }
}
