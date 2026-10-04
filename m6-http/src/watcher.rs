/// Platform-abstracted filesystem watcher for hot reload.
///
/// Watches `site.toml` and the TLS certificate and key, and emits
/// `SiteTomlChanged` and `TlsCertChanged` events.
///
/// On Linux: uses inotify on the site directory and on each certificate's own
/// parent directory, so events carry a filename and are told apart by it.
/// On macOS/FreeBSD/OpenBSD: uses kqueue EVFILT_VNODE on the site directory and
/// on the certificate and key as files, wherever they live. The wake carries no
/// filename, so a certificate change is told from a content write by comparing
/// modification times.
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

/// Which event a watched path produces when its fingerprint moves.
///
/// Only meaningful on the kqueue branch, where the wake carries no filename and
/// the path has to say for itself what it is.
#[cfg(any(target_os = "macos", target_os = "freebsd", target_os = "openbsd"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WatchKind {
    SiteToml,
    TlsMaterial,
}

/// One watched path, and what it looked like when it was last examined.
///
/// A struct rather than a tuple because clippy refused the tuple as too
/// complex, and it was right: `(WatchKind, PathBuf, Option<(SystemTime, u64)>)`
/// says nothing at a call site about which half is the fingerprint.
#[cfg(any(target_os = "macos", target_os = "freebsd", target_os = "openbsd"))]
struct Watched {
    kind: WatchKind,
    path: PathBuf,
    /// `None` when the path could not be read, which covers absent and
    /// unreadable alike. Both mean nothing can be said about the contents.
    seen: Option<(std::time::SystemTime, u64)>,
}

#[derive(Debug, Clone)]
pub struct FsEvent {
    pub path: PathBuf,
    pub kind: FsEventKind,
}

/// Filesystem watcher. Platform-specific implementation below.
pub struct FsWatcher {
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
    /// The certificate, the key and `site.toml`, each with what it last looked
    /// like.
    ///
    /// The kqueue wake carries no filename, so this branch cannot tell which
    /// file moved the way the inotify branch can. It compares fingerprints
    /// instead, which is the smallest thing that distinguishes them. Without it
    /// this platform emitted `SiteTomlChanged` alone and a certificate change
    /// reached the server never: a renewal on a BSD node was invisible until
    /// the process restarted. m6 #210.
    ///
    /// `site.toml` is in here for the opposite reason. The branch used to emit
    /// `SiteTomlChanged` on EVERY wake, and `handle_site_reload` rebuilds the
    /// route table, the pools and the invalidation map and then calls
    /// `cache.clear()`. So once the certificate was watched, a renewal also
    /// emptied the response cache, once per file written. Keying on the file's
    /// own fingerprint is what the inotify branch already does, by filename.
    watch: Vec<Watched>,
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

            let inotify = Inotify::init()?;

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
            // Empty in redirect mode, which has no certificate to watch.
            let tls_paths: Vec<&String> = [
                config.server.tls_cert.as_ref(),
                config.server.tls_key.as_ref(),
            ]
            .into_iter()
            .flatten()
            .collect();
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

            Ok(FsWatcher {
                inner: FsWatcherInner {
                    inotify,
                    tls_filenames,
                    socket_dir,
                },
            })
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

            // Recorded before the thread starts, so the first wake compares
            // against the state the server loaded from.
            let mut watch: Vec<Watched> = Vec::new();
            let site_toml = site_dir.join("site.toml");
            watch.push(Watched {
                kind: WatchKind::SiteToml,
                seen: file_fingerprint(&site_toml),
                path: site_toml,
            });
            for p in [
                config.server.tls_cert.as_ref(),
                config.server.tls_key.as_ref(),
            ]
            .into_iter()
            .flatten()
            {
                let path = PathBuf::from(p);
                watch.push(Watched {
                    kind: WatchKind::TlsMaterial,
                    seen: file_fingerprint(&path),
                    path,
                });
            }

            // ── The DIRECTORIES, not just the files ─────────────────────────
            //
            // An `open(O_EVTONLY)` watch follows a symlink and then holds the
            // inode it resolved to, so it sees a write THROUGH the link and
            // never sees the link being repointed. A certbot lineage is exactly
            // that shape: `live/<name>/fullchain.pem` is a symlink into
            // `archive/`, and a renewal writes a new archive file and moves the
            // link. `docs/m6-site-toml.md` prescribes that path.
            //
            // So the file watch alone covered an in-place overwrite of one inode
            // and missed the renewal this project actually performs. The
            // directory watch fires on the link being replaced, because that
            // changes a directory entry, and the fingerprint comparison then
            // resolves the link afresh and sees the new target. The inotify
            // branch has watched each certificate's parent directory from the
            // start, for the same reason.
            let mut dirs: Vec<PathBuf> = vec![site_dir.clone()];
            for w in &watch {
                if let Some(parent) = w.path.parent() {
                    let parent = parent.to_path_buf();
                    if !dirs.contains(&parent) {
                        dirs.push(parent);
                    }
                }
            }
            let watch_files: Vec<PathBuf> = watch.iter().map(|w| w.path.clone()).collect();
            let thread = std::thread::Builder::new()
                .name("m6-kqueue-watcher".into())
                .spawn(move || kqueue_watch_paths(dirs, watch_files, pipe_write))
                .map_err(|e| anyhow::anyhow!("spawn kqueue watcher: {e}"))?;

            Ok(FsWatcher {
                inner: FsWatcherInner {
                    pipe_read,
                    pipe_write,
                    _thread: thread,
                    watch,
                },
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
            Err(anyhow::anyhow!(
                "filesystem watching not supported on this platform"
            ))
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

                if (event.mask.contains(EventMask::DELETE)
                    || event.mask.contains(EventMask::MOVED_FROM))
                    && name.ends_with(".sock")
                {
                    result.push(FsEvent {
                        path: self.inner.socket_dir.join(&name),
                        kind: FsEventKind::SocketDeleted,
                    });
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
        //
        // The kqueue thread writes a byte whenever anything fires on the site
        // directory, on a certificate's parent directory, or on one of the
        // watched files. The wake carries NO filename, so this branch cannot do
        // what the inotify branch does and key on one. It compares each watched
        // path's fingerprint against what that path looked like last time, and
        // emits an event only for the paths that moved.
        //
        // That is a change from emitting `SiteTomlChanged` on every wake. Doing
        // so was harmless while nothing else was watched, and stopped being
        // harmless the moment the certificate was: `handle_site_reload`
        // rebuilds the route table, the pools and the invalidation map and then
        // calls `cache.clear()`, so a renewal emptied the response cache once
        // per file it wrote. Keying on the file is what the inotify branch has
        // always done.
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

            let mut site_changed = false;
            // One TLS event however many of the two files moved. A renewal
            // writes both, and two events would mean two rebuilds of one
            // configuration where the second can only agree with the first.
            let mut tls_changed: Option<PathBuf> = None;
            for w in self.inner.watch.iter_mut() {
                let now = file_fingerprint(&w.path);
                // `None` to `None` is a file that is still absent, which is not
                // a change. Every other transition is, a file appearing and a
                // file vanishing included: a certificate replaced by a rename
                // passes through both.
                if now == w.seen {
                    continue;
                }
                w.seen = now;
                match w.kind {
                    WatchKind::SiteToml => site_changed = true,
                    WatchKind::TlsMaterial => {
                        tls_changed = tls_changed.take().or_else(|| Some(w.path.clone()))
                    }
                }
            }

            let mut result = Vec::new();
            if site_changed {
                result.push(FsEvent {
                    path: PathBuf::from("site.toml"),
                    kind: FsEventKind::SiteTomlChanged,
                });
            }
            if let Some(path) = tls_changed {
                result.push(FsEvent {
                    path,
                    kind: FsEventKind::TlsCertChanged,
                });
            }
            result
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

/// What a file looked like, as modification time AND size.
///
/// `None` when it cannot be read. An unreadable file and a missing one are one
/// answer on purpose: the caller compares this value with the previous one to
/// decide whether a certificate moved, and both causes answer that question the
/// same way, so a later successful read is a change.
///
/// ── The SIZE is what makes a failed reload retry ────────────────────────────
///
/// With the time alone, `cp new.pem cert.pem` is missed. `cp` truncates and
/// then writes, so the first wake sees an EMPTY file with the new time, the
/// reload fails on it, and the stored time has already advanced. The content
/// write that follows lands in the same timestamp tick wherever the filesystem
/// has one-second granularity, which HFS+ does, so nothing differs and no
/// further event is emitted. The old certificate is then served until the
/// process restarts.
///
/// Size changes between those two observations even when the time does not, so
/// the pair detects the second write and the reload runs again. It does not
/// make every failed reload retry -- a write that lands identical bytes at an
/// identical time is still one observation -- and that case cannot be a
/// renewal.
#[cfg(any(target_os = "macos", target_os = "freebsd", target_os = "openbsd"))]
fn file_fingerprint(path: &std::path::Path) -> Option<(std::time::SystemTime, u64)> {
    let m = std::fs::metadata(path).ok()?;
    Some((m.modified().ok()?, m.len()))
}

// ── macOS/BSD kqueue watcher thread ──────────────────────────────────────────

/// Watch `site_dir` for any file writes/creates/deletes using kqueue EVFILT_VNODE.
/// Also watches `site.toml` and every path in `files` directly, so that a write
/// or a `touch` on one of them triggers a reload — directory NOTE_WRITE only
/// fires on entry creation/deletion, not mtime updates.
/// Writes a byte to `pipe_write` on each event to wake the main thread.
///
/// The certificate and key are in `files` because without them a renewal that
/// rewrites an existing certificate in place produced no event whatsoever on
/// this platform: the directory's entries did not change and nothing watched
/// the file. m6 #210.
///
/// A watch is registered once, on the inode that is there at startup, and
/// `O_EVTONLY` resolves symlinks. So a path whose inode is replaced loses its
/// watch: a rename over it, and a lineage of the shape
/// `live/<name>/cert.pem -> archive/<name>/cert3.pem` whose link is repointed,
/// both leave the watch on a file nobody writes again. The same is true of
/// `site.toml` here, and neither is fixed by this change. Watching each
/// certificate's parent directory, as the inotify branch does, is what would
/// cover it.
#[cfg(any(target_os = "macos", target_os = "freebsd", target_os = "openbsd"))]
fn kqueue_watch_paths(dirs: Vec<PathBuf>, files: Vec<PathBuf>, pipe_write: RawFd) {
    let kq = unsafe { libc::kqueue() };
    if kq < 0 {
        return;
    }

    // ── Every directory, then every file ────────────────────────────────────
    //
    // The directories are the site directory plus each watched file's parent.
    // A directory watch is what sees a NAME change: a rename over a path, and a
    // certbot lineage repointing `live/<name>/cert.pem` at a new file in
    // `archive/`. A file watch cannot, because `open(O_EVTONLY)` resolves the
    // symlink and then holds the inode it landed on.
    //
    // The fds are deliberately never closed. This thread runs for the life of
    // the process and a watch ends when its fd does, so there is one fd per
    // watched path and no more.
    let mut registered = 0usize;
    for dir in &dirs {
        let Ok(cstr) = std::ffi::CString::new(dir.as_os_str().as_encoded_bytes()) else {
            continue;
        };
        let dir_fd = unsafe { libc::open(cstr.as_ptr(), libc::O_EVTONLY) };
        if dir_fd < 0 {
            continue;
        }
        let ev_dir = libc::kevent {
            ident: dir_fd as libc::uintptr_t,
            filter: libc::EVFILT_VNODE,
            flags: libc::EV_ADD | libc::EV_ENABLE | libc::EV_CLEAR,
            fflags: libc::NOTE_WRITE | libc::NOTE_EXTEND | libc::NOTE_ATTRIB | libc::NOTE_LINK,
            data: 0,
            udata: std::ptr::null_mut(),
        };
        unsafe { libc::kevent(kq, &ev_dir, 1, std::ptr::null_mut(), 0, std::ptr::null()) };
        registered += 1;
    }
    // Not one directory opening is a watcher that wakes for nothing, which
    // reads from outside as a fleet whose configuration never reloads.
    if registered == 0 {
        unsafe { libc::close(kq) };
        return;
    }

    // Also watch each file directly: NOTE_ATTRIB fires on `touch`, NOTE_WRITE
    // fires on content writes, NOTE_RENAME/DELETE fires on atomic overwrites.
    //
    // The fds are deliberately not closed. This thread runs for the life of the
    // process and a watch ends when its fd does, so there is one fd per watched
    // file and no more: site.toml, the certificate and the key.
    // `files` already carries site.toml, the certificate and the key: `new()`
    // builds the list the fingerprint comparison reads, and this watches
    // exactly that, so the two cannot drift apart.
    for path in files {
        let Ok(cstr) = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()) else {
            continue;
        };
        let file_fd = unsafe { libc::open(cstr.as_ptr(), libc::O_EVTONLY) };
        if file_fd < 0 {
            continue;
        }
        let ev_file = libc::kevent {
            ident: file_fd as libc::uintptr_t,
            filter: libc::EVFILT_VNODE,
            flags: libc::EV_ADD | libc::EV_ENABLE | libc::EV_CLEAR,
            fflags: libc::NOTE_WRITE | libc::NOTE_ATTRIB | libc::NOTE_RENAME | libc::NOTE_DELETE,
            data: 0,
            udata: std::ptr::null_mut(),
        };
        unsafe { libc::kevent(kq, &ev_file, 1, std::ptr::null_mut(), 0, std::ptr::null()) };
    }

    let timeout = libc::timespec {
        tv_sec: 1,
        tv_nsec: 0,
    };
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
