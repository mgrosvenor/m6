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
const EVENT_BUF_LEN: usize = 4096;

/// Read buffer for `inotify_event`, aligned.
///
/// `read_events` casts offsets into this buffer straight to
/// `*const libc::inotify_event` and dereferences them. That struct begins with
/// a `c_int`, so it needs 4-byte alignment, and a bare `[u8; N]` has alignment
/// 1: nothing made the base address suitable. It worked because a 4096-byte
/// stack array is almost always well aligned in practice, which is the kind of
/// luck that holds until a compiler version or a stack layout changes.
///
/// The kernel pads each event's `len` so that the next one stays aligned
/// relative to the start of the buffer, so aligning the base is sufficient.
#[cfg(target_os = "linux")]
#[repr(align(8))]
struct EventBuf([u8; EVENT_BUF_LEN]);

#[cfg(target_os = "linux")]
const _: () = assert!(
    std::mem::align_of::<libc::inotify_event>() <= 8,
    "EventBuf must be at least as aligned as inotify_event"
);

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
        let mut buf = EventBuf([0u8; EVENT_BUF_LEN]);
        let mut matched = false;
        loop {
            let n = unsafe {
                libc::read(
                    self.inotify_fd,
                    buf.0.as_mut_ptr() as *mut libc::c_void,
                    buf.0.len(),
                )
            };
            if n <= 0 {
                break;
            }
            let n = n as usize;
            let mut offset = 0usize;
            while offset + std::mem::size_of::<libc::inotify_event>() <= n {
                let event =
                    unsafe { &*(buf.0.as_ptr().add(offset) as *const libc::inotify_event) };
                let name_len = event.len as usize;
                if name_len > 0 {
                    let name_start = offset + std::mem::size_of::<libc::inotify_event>();
                    let name_end = name_start + name_len;
                    if name_end <= n {
                        let name = std::ffi::CStr::from_bytes_until_nul(&buf.0[name_start..name_end])
                            .ok()
                            .and_then(|s| s.to_str().ok())
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
    /// The kqueue itself. **This is what `raw_fd` returns**, because a kqueue
    /// descriptor is pollable: it becomes readable when events are pending, so
    /// the service's existing `poll(2)` can wait on it directly.
    kq: RawFd,
    /// Descriptors for the watched directories and files, held open because a
    /// kevent registration lasts only as long as its descriptor.
    watched_fds: Vec<RawFd>,
}

#[cfg(any(target_os = "macos", target_os = "freebsd", target_os = "openbsd"))]
impl ConfigWatcher {
    /// Register every watch on one kqueue, synchronously.
    ///
    /// **This used to spawn a thread per watched directory**, each running its
    /// own `kqueue` loop on a one second timeout and writing a byte into a
    /// self-pipe so that the main loop's `poll` would wake. That is a second
    /// event loop, plus a pipe to get back to the first one, for a descriptor
    /// the first one could already have waited on. Three defects came with it:
    ///
    /// - **A startup race.** `new` returned before the threads had registered
    ///   their kevents, and `EV_CLEAR` is edge-triggered, so a config change in
    ///   that window was lost with no sign. Linux never had this, because
    ///   `inotify_add_watch` happens inside `new`.
    /// - **A thread leak.** `Drop` closed the pipe, but the threads looped
    ///   forever with no shutdown path, waking once a second to write to a
    ///   closed descriptor for the life of the process.
    /// - **Two more file descriptors and a thread per watcher**, to carry a
    ///   single readable bit.
    ///
    /// Registering here means the watch is live before `new` returns, which is
    /// the contract Linux already had and the one the caller assumes.
    pub fn new(paths: &[&Path]) -> anyhow::Result<Self> {
        use std::collections::HashSet;

        let kq = unsafe { libc::kqueue() };
        if kq < 0 {
            anyhow::bail!("kqueue failed: {}", std::io::Error::last_os_error());
        }

        let mut watched_fds: Vec<RawFd> = Vec::new();
        let mut seen: HashSet<std::path::PathBuf> = HashSet::new();

        // Directories catch create, rename and delete of the config file;
        // the files themselves catch an in-place write, which does not change
        // the directory at all. Both are needed, and both are just more
        // registrations on the same kqueue.
        let mut targets: Vec<std::path::PathBuf> = Vec::new();
        for path in paths {
            let dir = path.parent().unwrap_or(Path::new("/"));
            if !dir.exists() {
                tracing::warn!(dir = %dir.display(), "watch directory does not exist, skipping");
                continue;
            }
            if seen.insert(dir.to_path_buf()) {
                targets.push(dir.to_path_buf());
            }
            if path.exists() && seen.insert(path.to_path_buf()) {
                targets.push(path.to_path_buf());
            }
        }

        for target in &targets {
            let Ok(cstr) = std::ffi::CString::new(target.as_os_str().as_encoded_bytes()) else {
                tracing::warn!(path = %target.display(), "path contains interior NUL, skipping");
                continue;
            };
            let fd = unsafe { libc::open(cstr.as_ptr(), libc::O_EVTONLY) };
            if fd < 0 {
                tracing::warn!(
                    path = %target.display(),
                    error = %std::io::Error::last_os_error(),
                    "open(O_EVTONLY) failed, not watching"
                );
                continue;
            }
            let ev = libc::kevent {
                ident: fd as libc::uintptr_t,
                filter: libc::EVFILT_VNODE,
                flags: libc::EV_ADD | libc::EV_ENABLE | libc::EV_CLEAR,
                fflags: (libc::NOTE_WRITE
                    | libc::NOTE_EXTEND
                    | libc::NOTE_ATTRIB
                    | libc::NOTE_LINK
                    | libc::NOTE_RENAME
                    | libc::NOTE_DELETE) as u32,
                data: 0,
                udata: std::ptr::null_mut(),
            };
            if unsafe { libc::kevent(kq, &ev, 1, std::ptr::null_mut(), 0, std::ptr::null()) } < 0 {
                tracing::warn!(
                    path = %target.display(),
                    error = %std::io::Error::last_os_error(),
                    "kevent registration failed, not watching"
                );
                unsafe { libc::close(fd) };
                continue;
            }
            watched_fds.push(fd);
        }

        Ok(ConfigWatcher { kq, watched_fds })
    }

    pub fn raw_fd(&self) -> Option<RawFd> {
        Some(self.kq)
    }

    /// Drain pending events. Returns true if there were any.
    ///
    /// The `filenames` argument is ignored on this platform: kqueue reports
    /// which *descriptor* changed, not which directory entry, so a write to an
    /// unrelated file in the same directory is reported as a config change and
    /// costs a spurious reload. Linux compares the name and does not. The cost
    /// is one wasted reload on a development machine, so it stays.
    pub fn read_events(&mut self, _filenames: &[&str]) -> bool {
        let zero = libc::timespec { tv_sec: 0, tv_nsec: 0 };
        let mut evs = unsafe { std::mem::zeroed::<[libc::kevent; 16]>() };
        let mut any = false;
        loop {
            let n = unsafe {
                libc::kevent(self.kq, std::ptr::null(), 0, evs.as_mut_ptr(), 16, &zero)
            };
            if n <= 0 {
                break;
            }
            any = true;
            if (n as usize) < evs.len() {
                break;
            }
        }
        any
    }
}

#[cfg(any(target_os = "macos", target_os = "freebsd", target_os = "openbsd"))]
impl Drop for ConfigWatcher {
    fn drop(&mut self) {
        for fd in self.watched_fds.drain(..) {
            unsafe { libc::close(fd) };
        }
        unsafe { libc::close(self.kq) };
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

// ---------------------------------------------------------------------------

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod tests {
    use super::*;
    use std::io::Write as _;

    /// Block until the watcher's fd is readable, or the deadline passes.
    ///
    /// `poll(2)` on the watcher's own descriptor, which is exactly how
    /// [`crate::app`] waits on it, so these tests exercise the mechanism rather
    /// than a sleep chosen to be probably long enough. A filesystem event is
    /// asynchronous, and the honest way to wait for one is to wait on the thing
    /// that signals it.
    fn readable_within(w: &ConfigWatcher, ms: i32) -> bool {
        let fd = w.raw_fd().expect("a supported platform has a watcher fd");
        let mut pfd = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
        let n = unsafe { libc::poll(&mut pfd, 1, ms) };
        n > 0 && (pfd.revents & libc::POLLIN) != 0
    }

    fn write_file(path: &Path, body: &str) {
        let mut f = std::fs::File::create(path).expect("create");
        f.write_all(body.as_bytes()).expect("write");
        f.sync_all().expect("sync");
        // Dropped here, which is what produces inotify's IN_CLOSE_WRITE. The
        // mask does not include IN_MODIFY, so a write that is never closed is
        // deliberately not an event.
    }

    /// The whole contract in one test: a write to a watched file wakes the
    /// poller, and the watcher reports it against the name the app asked for.
    ///
    /// **This had no test on any platform.** `tests/log_reload.rs` covers
    /// `LogHandle::reload`, which is a different thing, so config hot reload
    /// was shipped and deployed having never been exercised.
    #[test]
    fn a_write_to_a_watched_file_wakes_the_poller_and_matches_its_name() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg = dir.path().join("app.conf");
        write_file(&cfg, "size = 1\n");

        let mut w = ConfigWatcher::new(&[cfg.as_path()]).expect("watcher");
        assert!(w.raw_fd().is_some(), "a supported platform must expose an fd");

        write_file(&cfg, "size = 2\n");

        assert!(
            readable_within(&w, 5_000),
            "the watcher fd never became readable after the config was rewritten"
        );
        assert!(
            w.read_events(&["app.conf"]),
            "the event arrived but was not reported for the watched name"
        );
    }

    /// An idle watcher reports nothing, and does not wake the poller.
    ///
    /// Worth pinning separately: a watcher that always returned true would pass
    /// the test above and would make `App` rebuild its whole `FrameworkState`
    /// every 100ms forever, which is a performance bug that looks like working
    /// hot reload.
    #[test]
    fn an_idle_watcher_reports_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg = dir.path().join("app.conf");
        write_file(&cfg, "size = 1\n");

        let mut w = ConfigWatcher::new(&[cfg.as_path()]).expect("watcher");
        // Drain anything the setup itself produced, then assert quiet.
        let _ = w.read_events(&["app.conf"]);

        assert!(
            !readable_within(&w, 300),
            "the watcher fd was readable with nothing happening"
        );
        assert!(!w.read_events(&["app.conf"]), "an idle watcher reported an event");
    }

    /// A directory that does not exist is skipped, not fatal.
    ///
    /// `App` builds the watcher from the config path and `site.toml`, and a
    /// service whose site directory is absent must still start and report the
    /// real error from config loading, rather than dying here first.
    #[test]
    fn a_missing_directory_is_skipped_rather_than_fatal() {
        let missing = Path::new("/nonexistent-m6-watch-dir/app.conf");
        let w = ConfigWatcher::new(&[missing]).expect("an absent dir must not fail construction");
        drop(w);
    }

    /// Filename precision, and it differs by platform **by design**.
    ///
    /// Linux watches the directory with inotify and compares each event's name
    /// against the list, so an unrelated file is not a reload. macOS watches at
    /// directory granularity through a self-pipe and ignores the names
    /// entirely, so it is, and the doc comment on the macOS `read_events` says
    /// so.
    ///
    /// Pinned in both directions rather than tested only where it is convenient:
    /// production is Linux, so the precise behaviour is the one that matters,
    /// and if macOS ever gains precision this test should be the thing that
    /// notices rather than a surprise.
    #[test]
    fn an_unrelated_file_matches_only_where_the_platform_is_imprecise() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg = dir.path().join("app.conf");
        write_file(&cfg, "size = 1\n");

        let mut w = ConfigWatcher::new(&[cfg.as_path()]).expect("watcher");

        // Prime, then drain, so that what follows is a statement about name
        // matching and not about whether the watcher was up yet. Both platforms
        // now register their watches inside `new`, so this should be immediate;
        // it is here because a test that cannot tell those two failures apart
        // is the one that sent me looking at the wrong thing.
        write_file(&cfg, "size = 2\n");
        assert!(
            readable_within(&w, 5_000),
            "the watcher never came up, so this test cannot say anything about names"
        );
        let _ = w.read_events(&["app.conf"]);

        write_file(&dir.path().join("unrelated.txt"), "not the config\n");
        let woke = readable_within(&w, 5_000);
        let matched = w.read_events(&["app.conf"]);

        #[cfg(target_os = "linux")]
        {
            assert!(
                !matched,
                "inotify compares the event name, so an unrelated file must not \
                 be reported as a config change"
            );
        }
        #[cfg(target_os = "macos")]
        {
            assert!(woke, "the kqueue directory watcher should have fired");
            assert!(
                matched,
                "macOS watches at directory granularity and ignores the name \
                 list; if this now fails, the platform gained precision and the \
                 doc comment on read_events is stale"
            );
        }
        let _ = woke;
    }
}
