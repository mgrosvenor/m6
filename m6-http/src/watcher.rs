/// Platform-abstracted filesystem watcher for hot reload.
///
/// Watches `site.toml` for changes and emits `SiteTomlChanged` events.
///
/// On Linux: uses inotify to watch the site directory.
/// On macOS/FreeBSD/OpenBSD: uses kqueue EVFILT_VNODE on the site directory.
/// On other platforms: returns an error from `new()`, hot reload disabled.
///
/// Socket pool membership is managed separately via periodic rescan, so this
/// watcher does not need to track socket files.

use std::os::unix::io::RawFd;
use std::path::PathBuf;

use crate::config::Config;

#[derive(Debug, Clone)]
pub enum FsEventKind {
    SocketCreated,
    SocketDeleted,
    SiteTomlChanged,
    /// TLS certificate or key file changed — caller should reload TLS config.
    TlsCertChanged,
}

#[derive(Debug, Clone)]
pub struct FsEvent {
    pub path: PathBuf,
    pub kind: FsEventKind,
}

/// Filesystem watcher. Platform-specific implementation below.
pub struct FsWatcher {
    #[allow(dead_code)]
    inner: FsWatcherInner,
}

// ─────────────────────────────────────────────────────────────────────────────
// Linux: inotify
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(target_os = "linux")]
struct FsWatcherInner {
    inotify: inotify::Inotify,
    tls_filenames: Vec<String>,
    socket_dir: PathBuf,
}

// ─────────────────────────────────────────────────────────────────────────────
// macOS / FreeBSD / OpenBSD: kqueue EVFILT_VNODE on site directory
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(any(target_os = "macos", target_os = "freebsd", target_os = "openbsd"))]
struct FsWatcherInner {
    /// Read end of self-pipe — returned from raw_fd(), registered with poller.
    pipe_read: RawFd,
    /// Write end of self-pipe — written by background watcher thread.
    pipe_write: RawFd,
    /// Background thread kept alive for process lifetime.
    _thread: std::thread::JoinHandle<()>,
}

// No-op fallback
#[cfg(not(any(
    target_os = "linux",
    target_os = "macos",
    target_os = "freebsd",
    target_os = "openbsd"
)))]
struct FsWatcherInner {}

impl FsWatcher {
    pub fn new(config: &Config) -> anyhow::Result<Self> {
        // ── Linux ──────────────────────────────────────────────────────────────
        #[cfg(target_os = "linux")]
        {
            use inotify::{Inotify, WatchMask};
            use std::collections::HashSet;

            let mut inotify = Inotify::init()?;

            let site_dir = &config.site_dir;
            if site_dir.exists() {
                inotify.watches().add(
                    site_dir,
                    WatchMask::CLOSE_WRITE | WatchMask::MOVED_TO | WatchMask::CREATE,
                )?;
            }

            // Determine socket directory from backend configs.
            let socket_dir = config
                .backends
                .iter()
                .filter_map(|b| b.sockets.as_ref())
                .filter_map(|g| std::path::Path::new(g).parent().map(|p| p.to_path_buf()))
                .next()
                .unwrap_or_else(|| PathBuf::from("/run/m6"));

            if socket_dir.exists() {
                inotify.watches().add(
                    &socket_dir,
                    WatchMask::CREATE
                        | WatchMask::DELETE
                        | WatchMask::MOVED_TO
                        | WatchMask::MOVED_FROM,
                )?;
            }

            let mut tls_filenames: Vec<String> = Vec::new();
            let tls_paths = [&config.server.tls_cert, &config.server.tls_key];
            let mut watched_dirs: HashSet<std::path::PathBuf> = HashSet::new();
            for tls_path_str in &tls_paths {
                let tls_path = std::path::Path::new(tls_path_str);
                if let Some(fname) = tls_path.file_name() {
                    tls_filenames.push(fname.to_string_lossy().into_owned());
                }
                if let Some(parent) = tls_path.parent() {
                    if parent.exists() && !watched_dirs.contains(parent) {
                        let _ = inotify.watches().add(
                            parent,
                            WatchMask::CLOSE_WRITE | WatchMask::MOVED_TO | WatchMask::CREATE,
                        );
                        watched_dirs.insert(parent.to_path_buf());
                    }
                }
            }

            Ok(FsWatcher { inner: FsWatcherInner { inotify, tls_filenames, socket_dir } })
        }

        // ── macOS / FreeBSD / OpenBSD ──────────────────────────────────────────
        #[cfg(any(target_os = "macos", target_os = "freebsd", target_os = "openbsd"))]
        {
            let site_dir = config.site_dir.clone();

            // Create self-pipe for signalling the main thread.
            let mut pipe_fds = [0i32; 2];
            if unsafe { libc::pipe(pipe_fds.as_mut_ptr()) } < 0 {
                anyhow::bail!("pipe() failed: {}", std::io::Error::last_os_error());
            }
            let pipe_read = pipe_fds[0];
            let pipe_write = pipe_fds[1];
            for &fd in &[pipe_read, pipe_write] {
                unsafe { libc::fcntl(fd, libc::F_SETFL, libc::O_NONBLOCK) };
            }

            let thread = std::thread::Builder::new()
                .name("m6-kqueue-watcher".into())
                .spawn(move || kqueue_watch_site_dir(site_dir, pipe_write))
                .map_err(|e| anyhow::anyhow!("spawn kqueue watcher: {e}"))?;

            Ok(FsWatcher {
                inner: FsWatcherInner { pipe_read, pipe_write, _thread: thread },
            })
        }

        // ── No-op fallback ─────────────────────────────────────────────────────
        #[cfg(not(any(
            target_os = "linux",
            target_os = "macos",
            target_os = "freebsd",
            target_os = "openbsd"
        )))]
        {
            let _ = config;
            Err(anyhow::anyhow!("filesystem watching not supported on this platform"))
        }
    }

    /// Return the raw file descriptor for polling, if available.
    pub fn raw_fd(&self) -> Option<RawFd> {
        #[cfg(target_os = "linux")]
        {
            use std::os::unix::io::AsRawFd;
            Some(self.inner.inotify.as_raw_fd())
        }

        #[cfg(any(target_os = "macos", target_os = "freebsd", target_os = "openbsd"))]
        {
            Some(self.inner.pipe_read)
        }

        #[cfg(not(any(
            target_os = "linux",
            target_os = "macos",
            target_os = "freebsd",
            target_os = "openbsd"
        )))]
        {
            None
        }
    }

    /// Read and return pending events.
    pub fn read_events(&mut self) -> Vec<FsEvent> {
        // ── Linux ──────────────────────────────────────────────────────────────
        #[cfg(target_os = "linux")]
        {
            use inotify::EventMask;

            let mut buf = [0u8; 4096];
            let events = match self.inner.inotify.read_events(&mut buf) {
                Ok(e) => e,
                Err(e) => {
                    tracing::warn!(error = %e, "inotify read error");
                    return vec![];
                }
            };

            let mut result = Vec::new();
            for event in events {
                let name = event
                    .name
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();

                if event.mask.contains(EventMask::CREATE)
                    || event.mask.contains(EventMask::MOVED_TO)
                {
                    if name.ends_with(".sock") {
                        result.push(FsEvent {
                            path: self.inner.socket_dir.join(&name),
                            kind: FsEventKind::SocketCreated,
                        });
                    }
                    if name == "site.toml" {
                        result.push(FsEvent {
                            path: std::path::PathBuf::from("site.toml"),
                            kind: FsEventKind::SiteTomlChanged,
                        });
                    }
                    if self.inner.tls_filenames.iter().any(|f| f == &name) {
                        result.push(FsEvent {
                            path: std::path::PathBuf::from(&name),
                            kind: FsEventKind::TlsCertChanged,
                        });
                    }
                }

                if event.mask.contains(EventMask::DELETE)
                    || event.mask.contains(EventMask::MOVED_FROM)
                {
                    if name.ends_with(".sock") {
                        result.push(FsEvent {
                            path: self.inner.socket_dir.join(&name),
                            kind: FsEventKind::SocketDeleted,
                        });
                    }
                }

                if event.mask.contains(EventMask::CLOSE_WRITE) {
                    if name == "site.toml" || name.is_empty() {
                        result.push(FsEvent {
                            path: std::path::PathBuf::from("site.toml"),
                            kind: FsEventKind::SiteTomlChanged,
                        });
                    }
                    if self.inner.tls_filenames.iter().any(|f| f == &name) {
                        result.push(FsEvent {
                            path: std::path::PathBuf::from(&name),
                            kind: FsEventKind::TlsCertChanged,
                        });
                    }
                }
            }
            result
        }

        // ── macOS / FreeBSD / OpenBSD ──────────────────────────────────────────
        // The kqueue thread signals us via the pipe whenever any write/delete/create
        // event fires on the site directory. We drain the pipe and emit a
        // SiteTomlChanged event — the reload handler re-reads the file and checks
        // whether the config actually changed, so spurious events are harmless.
        #[cfg(any(target_os = "macos", target_os = "freebsd", target_os = "openbsd"))]
        {
            let mut buf = [0u8; 64];
            loop {
                let n = unsafe {
                    libc::read(
                        self.inner.pipe_read,
                        buf.as_mut_ptr() as *mut libc::c_void,
                        buf.len(),
                    )
                };
                if n <= 0 {
                    break;
                }
            }
            vec![FsEvent {
                path: PathBuf::from("site.toml"),
                kind: FsEventKind::SiteTomlChanged,
            }]
        }

        // ── No-op ──────────────────────────────────────────────────────────────
        #[cfg(not(any(
            target_os = "linux",
            target_os = "macos",
            target_os = "freebsd",
            target_os = "openbsd"
        )))]
        {
            vec![]
        }
    }
}

impl Drop for FsWatcher {
    fn drop(&mut self) {
        #[cfg(any(target_os = "macos", target_os = "freebsd", target_os = "openbsd"))]
        unsafe {
            libc::close(self.inner.pipe_read);
            libc::close(self.inner.pipe_write);
        }
    }
}

// ── macOS/BSD kqueue watcher thread ──────────────────────────────────────────

/// Watch `site_dir` for any file writes/creates/deletes using kqueue EVFILT_VNODE.
/// Also watches `site.toml` directly so that `touch site.toml` (NOTE_ATTRIB on
/// the file itself) triggers a reload — directory NOTE_WRITE only fires on
/// entry creation/deletion, not mtime updates.
/// Writes a byte to `pipe_write` on each event to wake the main thread.
#[cfg(any(target_os = "macos", target_os = "freebsd", target_os = "openbsd"))]
fn kqueue_watch_site_dir(site_dir: PathBuf, pipe_write: RawFd) {
    let kq = unsafe { libc::kqueue() };
    if kq < 0 {
        return;
    }

    let dir_cstr = match std::ffi::CString::new(site_dir.as_os_str().as_encoded_bytes()) {
        Ok(s) => s,
        Err(_) => {
            unsafe { libc::close(kq) };
            return;
        }
    };

    let dir_fd = unsafe { libc::open(dir_cstr.as_ptr(), libc::O_EVTONLY) };
    if dir_fd < 0 {
        unsafe { libc::close(kq) };
        return;
    }

    // Watch the site directory for file creation/deletion.
    let ev_dir = libc::kevent {
        ident: dir_fd as libc::uintptr_t,
        filter: libc::EVFILT_VNODE,
        flags: libc::EV_ADD | libc::EV_ENABLE | libc::EV_CLEAR,
        fflags: (libc::NOTE_WRITE | libc::NOTE_EXTEND | libc::NOTE_ATTRIB | libc::NOTE_LINK)
            as u32,
        data: 0,
        udata: std::ptr::null_mut(),
    };
    unsafe { libc::kevent(kq, &ev_dir, 1, std::ptr::null_mut(), 0, std::ptr::null()) };

    // Also watch site.toml directly: NOTE_ATTRIB fires on `touch`, NOTE_WRITE
    // fires on content writes, NOTE_RENAME/DELETE fires on atomic overwrites.
    let site_toml_path = site_dir.join("site.toml");
    let site_toml_cstr = std::ffi::CString::new(
        site_toml_path.as_os_str().as_encoded_bytes()
    );
    let file_fd = match &site_toml_cstr {
        Ok(cstr) => unsafe { libc::open(cstr.as_ptr(), libc::O_EVTONLY) },
        Err(_) => -1,
    };
    if file_fd >= 0 {
        let ev_file = libc::kevent {
            ident: file_fd as libc::uintptr_t,
            filter: libc::EVFILT_VNODE,
            flags: libc::EV_ADD | libc::EV_ENABLE | libc::EV_CLEAR,
            fflags: (libc::NOTE_WRITE
                | libc::NOTE_ATTRIB
                | libc::NOTE_RENAME
                | libc::NOTE_DELETE) as u32,
            data: 0,
            udata: std::ptr::null_mut(),
        };
        unsafe { libc::kevent(kq, &ev_file, 1, std::ptr::null_mut(), 0, std::ptr::null()) };
    }

    let timeout = libc::timespec { tv_sec: 1, tv_nsec: 0 };
    let mut out_ev = unsafe { std::mem::zeroed::<libc::kevent>() };

    loop {
        let n = unsafe {
            libc::kevent(kq, std::ptr::null(), 0, &mut out_ev, 1, &timeout)
        };
        if n > 0 {
            let byte: u8 = 1;
            unsafe {
                libc::write(pipe_write, &byte as *const u8 as *const libc::c_void, 1);
            }
        }
    }

}
