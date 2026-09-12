//! Cross-platform file-change notifier.
//!
//! `ConfigWatcher` watches a set of file paths, by watching their parent
//! directories, and signals when any of the watched files change. It exposes a
//! **pollable file descriptor**, so the service's own `poll(2)` waits on it
//! alongside its listener. That is the whole design constraint: this is not a
//! runtime, it is one more descriptor for the loop that already exists.
//!
//! `notify`, the obvious crate, spawns a background thread and delivers over a
//! channel. That is the arrangement `e6ba278` removed from the macOS arm here,
//! and putting it back would reintroduce the startup race and the thread leak
//! it removed. A wrapper is wanted, not a runtime.
//!
//! - **Linux**: `nix::sys::inotify`.
//! - **macOS, FreeBSD, OpenBSD**: `nix::sys::event`, one kqueue, no threads.
//! - **Anything else**: a no-op; `raw_fd()` returns `None` and `read_events`
//!   always returns false, so the service falls back to mtime polling.
//!
//! **This was 390 lines of raw `unsafe` libc across three `#[cfg]` arms**, on
//! the owner's instruction to put it on the list: *"standard OS interfaces to
//! watch a file and poll to wake up when there's a change; extra threads are
//! totally unnecessary; there are standard Unix wrappers around all of these."*
//! The single most valuable part to have gone is the manual walk over the
//! inotify read buffer, which is where the unaligned-read UB came from: the
//! code cast offsets into a `[u8; 4096]` straight to `*const inotify_event` and
//! dereferenced them. `nix` copies each header into an aligned `MaybeUninit`
//! instead, which is the correct way to do it and not something this crate
//! should have been deciding for itself.

use std::os::unix::io::{AsFd, AsRawFd, RawFd};
use std::path::Path;

/// The directories and files to watch, deduplicated.
///
/// Directories catch create, rename and delete of a config file; the files
/// themselves catch an in-place write, which does not change the directory at
/// all. Linux gets both from a directory watch because inotify reports the
/// entry name; kqueue reports only which descriptor changed, so it needs the
/// file registered too.
fn watch_targets(paths: &[&Path], include_files: bool) -> Vec<std::path::PathBuf> {
    use std::collections::HashSet;
    let mut seen: HashSet<std::path::PathBuf> = HashSet::new();
    let mut targets = Vec::new();
    for path in paths {
        let dir = path.parent().unwrap_or(Path::new("/"));
        if !dir.exists() {
            tracing::warn!(dir = %dir.display(), "watch directory does not exist, skipping");
            continue;
        }
        if seen.insert(dir.to_path_buf()) {
            targets.push(dir.to_path_buf());
        }
        if include_files && path.exists() && seen.insert(path.to_path_buf()) {
            targets.push(path.to_path_buf());
        }
    }
    targets
}

// ─────────────────────────────────────────────────────────────────────────────
// Linux: inotify
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(target_os = "linux")]
pub struct ConfigWatcher {
    inotify: nix::sys::inotify::Inotify,
}

#[cfg(target_os = "linux")]
impl ConfigWatcher {
    pub fn new(paths: &[&Path]) -> anyhow::Result<Self> {
        use nix::sys::inotify::{AddWatchFlags, InitFlags, Inotify};

        let inotify = Inotify::init(InitFlags::IN_CLOEXEC | InitFlags::IN_NONBLOCK)
            .map_err(|e| anyhow::anyhow!("inotify_init1 failed: {e}"))?;

        // `IN_CLOSE_WRITE` rather than `IN_MODIFY`: a writer that makes several
        // writes produces one event on close instead of a reload per write.
        // `IN_CREATE` and `IN_MOVED_TO` catch the write-to-temp-then-rename
        // that every careful editor and every deploy script does.
        let mask = AddWatchFlags::IN_CLOSE_WRITE
            | AddWatchFlags::IN_CREATE
            | AddWatchFlags::IN_MOVED_TO;

        for dir in watch_targets(paths, false) {
            if let Err(e) = inotify.add_watch(&dir, mask) {
                tracing::warn!(dir = %dir.display(), error = %e, "inotify_add_watch failed");
            }
        }

        Ok(ConfigWatcher { inotify })
    }

    pub fn raw_fd(&self) -> Option<RawFd> {
        Some(self.inotify.as_fd().as_raw_fd())
    }

    /// Drain pending events, returning true if any names a watched file.
    pub fn read_events(&mut self, filenames: &[&str]) -> bool {
        let mut matched = false;
        // One `read` per call, so loop until the queue is empty. An empty
        // non-blocking queue is `EAGAIN`, which is the normal way out rather
        // than an error to report.
        loop {
            match self.inotify.read_events() {
                Ok(events) if events.is_empty() => break,
                Ok(events) => {
                    for ev in events {
                        if let Some(name) = ev.name.as_ref().and_then(|n| n.to_str()) {
                            if filenames.contains(&name) {
                                matched = true;
                            }
                        }
                    }
                }
                Err(nix::errno::Errno::EAGAIN) => break,
                Err(e) => {
                    tracing::warn!(error = %e, "inotify read failed");
                    break;
                }
            }
        }
        matched
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// macOS / FreeBSD / OpenBSD: kqueue
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(any(target_os = "macos", target_os = "freebsd", target_os = "openbsd"))]
pub struct ConfigWatcher {
    /// **This is what `raw_fd` returns**, because a kqueue descriptor is
    /// pollable: it becomes readable when events are pending, so the service's
    /// existing `poll(2)` waits on it directly.
    kq: nix::sys::event::Kqueue,
    /// The watched directories and files, held open because a kevent
    /// registration lasts exactly as long as its descriptor. `File` rather than
    /// a raw fd, so closing them is the borrow checker's job and not a `Drop`
    /// impl's.
    _watched: Vec<std::fs::File>,
}

#[cfg(any(target_os = "macos", target_os = "freebsd", target_os = "openbsd"))]
impl ConfigWatcher {
    /// Register every watch on one kqueue, synchronously.
    ///
    /// **This used to spawn a thread per watched directory**, each running its
    /// own kqueue loop on a one second timeout and writing a byte into a
    /// self-pipe so the main loop's `poll` would wake: a second event loop,
    /// plus a pipe to get back to the first one, for a descriptor the first one
    /// could already have waited on. It cost a startup race (`new` returned
    /// before the threads had registered, and `EV_CLEAR` is edge-triggered, so
    /// a change in that window was lost silently) and a thread leak (the
    /// threads had no shutdown path and woke once a second forever, writing to
    /// a closed descriptor). Registering here means the watch is live before
    /// `new` returns, which is the contract Linux always had.
    pub fn new(paths: &[&Path]) -> anyhow::Result<Self> {
        use nix::sys::event::{EventFilter, EvFlags, FilterFlag, KEvent, Kqueue};

        let kq = Kqueue::new().map_err(|e| anyhow::anyhow!("kqueue failed: {e}"))?;
        let mut watched = Vec::new();

        for target in watch_targets(paths, true) {
            // `File::open` works on a directory on Unix and gives an owned
            // descriptor. The previous version used `open(O_EVTONLY)`, which is
            // the more precise flag -- it does not hold the volume against
            // unmount -- but it is not in `nix`'s `OFlag`, and reaching past
            // the wrapper for it is what this rewrite is removing. A config
            // file's volume is not being unmounted under a running service.
            let file = match std::fs::File::open(&target) {
                Ok(f) => f,
                Err(e) => {
                    tracing::warn!(path = %target.display(), error = %e, "cannot open to watch, not watching");
                    continue;
                }
            };

            let ev = KEvent::new(
                file.as_raw_fd() as usize,
                EventFilter::EVFILT_VNODE,
                EvFlags::EV_ADD | EvFlags::EV_ENABLE | EvFlags::EV_CLEAR,
                FilterFlag::NOTE_WRITE
                    | FilterFlag::NOTE_EXTEND
                    | FilterFlag::NOTE_ATTRIB
                    | FilterFlag::NOTE_LINK
                    | FilterFlag::NOTE_RENAME
                    | FilterFlag::NOTE_DELETE,
                0,
                0,
            );
            if let Err(e) = kq.kevent(&[ev], &mut [], Some(ZERO_TIMEOUT)) {
                tracing::warn!(path = %target.display(), error = %e, "kevent registration failed, not watching");
                continue;
            }
            watched.push(file);
        }

        Ok(ConfigWatcher { kq, _watched: watched })
    }

    pub fn raw_fd(&self) -> Option<RawFd> {
        Some(self.kq.as_fd().as_raw_fd())
    }

    /// Drain pending events. Returns true if there were any.
    ///
    /// The `filenames` argument is ignored on this platform: kqueue reports
    /// which *descriptor* changed, not which directory entry, so a write to an
    /// unrelated file in the same directory is reported as a config change and
    /// costs a spurious reload. Linux compares the name and does not. The cost
    /// is one wasted reload on a development machine, so it stays.
    pub fn read_events(&mut self, _filenames: &[&str]) -> bool {
        use nix::sys::event::{EventFilter, EvFlags, FilterFlag, KEvent};

        let mut evs = [KEvent::new(
            0,
            EventFilter::EVFILT_VNODE,
            EvFlags::empty(),
            FilterFlag::empty(),
            0,
            0,
        ); 16];
        let mut any = false;
        loop {
            match self.kq.kevent(&[], &mut evs, Some(ZERO_TIMEOUT)) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    any = true;
                    if n < evs.len() {
                        break;
                    }
                }
            }
        }
        any
    }
}

/// Poll the queue rather than wait on it: the service's own `poll(2)` has
/// already told us something is ready, and a blocking drain here would hold the
/// loop.
#[cfg(any(target_os = "macos", target_os = "freebsd", target_os = "openbsd"))]
const ZERO_TIMEOUT: nix::libc::timespec =
    nix::libc::timespec { tv_sec: 0, tv_nsec: 0 };

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
    fn readable_within(w: &ConfigWatcher, ms: u16) -> bool {
        // Through the same shared helper the service loop uses, so the test
        // waits on the descriptor exactly as production does rather than on a
        // second hand-rolled `poll` that could drift from it.
        let fd = w.raw_fd().expect("a supported platform has a watcher fd");
        crate::server::poll_listener_and_watcher(fd, None, ms).listener
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
