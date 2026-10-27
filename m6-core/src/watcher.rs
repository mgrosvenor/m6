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
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

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
        //
        // `IN_ATTRIB` for a timestamp-only change, and this one was missing.
        //
        // `docs/m6-site-toml.md` tells renderers to "touch site.toml after
        // writing to trigger the reload", and a bare timestamp update is exactly
        // what that reads as. `utimensat(2)` with no open -- what
        // `filetime::set_file_times` does, and therefore what m6-core's own
        // `Request::touch` did -- reports IN_ATTRIB and nothing else. None of the
        // three flags above match it, so the event was read and discarded.
        //
        // On Linux that meant the documented invalidation mechanism had never
        // worked. Measured on the build host: `utimensat(AT_FDCWD, "site.toml",
        // NULL, 0)` produced zero reload lines, while opening the file for write
        // and closing it produced two. macOS was unaffected, because kqueue
        // registers the files themselves and reports the attribute change, so
        // the defect was invisible on the machine the code was written on and
        // live on the platform that serves production.
        //
        // The cost of including it is a reload on `chmod` or `chown` of a
        // watched file, which a reload is idempotent about, against a silent
        // failure to reload at all.
        let mask = AddWatchFlags::IN_CLOSE_WRITE
            | AddWatchFlags::IN_CREATE
            | AddWatchFlags::IN_MOVED_TO
            | AddWatchFlags::IN_ATTRIB;

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
        use nix::sys::event::{EvFlags, EventFilter, FilterFlag, KEvent, Kqueue};

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

        Ok(ConfigWatcher {
            kq,
            _watched: watched,
        })
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
        use nix::sys::event::{EvFlags, EventFilter, FilterFlag, KEvent};

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
const ZERO_TIMEOUT: nix::libc::timespec = nix::libc::timespec {
    tv_sec: 0,
    tv_nsec: 0,
};

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

// ─────────────────────────────────────────────────────────────────────────────
// ConfigReloader: both of a process's files, with a settle window and a poll
// ─────────────────────────────────────────────────────────────────────────────

/// The settle window after a report.
///
/// The first change after quiet is reported at once. A change that arrives
/// within this window of a report is held and reported once more when the
/// window of quiet ends, so a burst is at most two reports and a single write
/// is exactly one. Reporting at once keeps a backend from reloading later than
/// an edge that cleared its cache on the same change.
pub const RELOAD_DEBOUNCE: Duration = Duration::from_millis(100);

/// How often the reloader compares fingerprints whatever the watcher says, and
/// the longest a batch of changes is held back.
///
/// A watch that has been lost, a replaced directory, or a kqueue watch left on
/// an unlinked inode after an atomic rename all stop delivering events. This
/// poll is two `stat` calls a second and finds the change anyway.
pub const RELOAD_POLL: Duration = Duration::from_secs(1);

/// Which of a process's two files changed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Changed {
    /// `site.toml` changed.
    pub site: bool,
    /// The process's own config changed.
    pub process: bool,
}

impl Changed {
    /// True when either file changed.
    pub fn any(self) -> bool {
        self.site || self.process
    }
}

/// What `stat` says about a file, followed through a symlink.
///
/// Two fingerprints are equal when the same bytes are in the same file with the
/// same times. A touch moves `ctime` even when nothing else moves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fingerprint {
    dev: u64,
    ino: u64,
    len: u64,
    mtime_ns: i128,
    ctime_ns: i128,
}

impl Fingerprint {
    /// The fingerprint of `path`, or `None` when it cannot be read.
    pub fn of(path: &Path) -> Option<Fingerprint> {
        use std::os::unix::fs::MetadataExt;
        let m = std::fs::metadata(path).ok()?;
        Some(Fingerprint {
            dev: m.dev(),
            ino: m.ino(),
            len: m.len(),
            mtime_ns: i128::from(m.mtime()) * 1_000_000_000 + i128::from(m.mtime_nsec()),
            ctime_ns: i128::from(m.ctime()) * 1_000_000_000 + i128::from(m.ctime_nsec()),
        })
    }
}

/// The fingerprints of both files as last seen.
#[derive(Debug, Clone, Copy, Default)]
pub struct Seen {
    /// `site.toml`.
    pub site: Option<Fingerprint>,
    /// The process config.
    pub process: Option<Fingerprint>,
}

impl Seen {
    /// Fingerprint both files. Call this before the first read of either, so a
    /// change between that read and the reloader's creation is reported on its
    /// first tick instead of lost.
    pub fn read(site_toml: &Path, process: &Path) -> Seen {
        Seen {
            site: Fingerprint::of(site_toml),
            process: Fingerprint::of(process),
        }
    }
}

/// Watches `site.toml` and a process's own config and reports which changed.
///
/// A [`ConfigWatcher`] wakes the loop. The fingerprints decide what changed, so
/// the answer is the same on inotify and kqueue and ignores other files in a
/// shared directory. The loop calls [`on_readable`](Self::on_readable) when
/// [`raw_fd`](Self::raw_fd) is readable and [`tick`](Self::tick) on every
/// iteration, idle or not. No thread, no allocation in `tick`.
pub struct ConfigReloader {
    watcher: Option<ConfigWatcher>,
    site_toml: PathBuf,
    process: PathBuf,
    same_file: bool,
    seen: Seen,
    pending: Changed,
    first: Option<Instant>,
    due: Option<Instant>,
    window_end: Option<Instant>,
    next_poll: Instant,
    site_name: String,
    process_name: String,
}

impl ConfigReloader {
    /// Watch both files. `seen` is what the caller fingerprinted before it first
    /// read them. Without a working watcher the reloader still polls.
    pub fn new(site_toml: &Path, process: &Path, seen: Seen) -> ConfigReloader {
        let watcher = match ConfigWatcher::new(&[process, site_toml]) {
            Ok(w) => Some(w),
            Err(e) => {
                tracing::warn!("config watch unavailable, polling every second: {e}");
                None
            }
        };
        Self::with_watcher(watcher, site_toml, process, seen)
    }

    fn with_watcher(
        watcher: Option<ConfigWatcher>,
        site_toml: &Path,
        process: &Path,
        seen: Seen,
    ) -> ConfigReloader {
        let same_file = match (site_toml.canonicalize(), process.canonicalize()) {
            (Ok(a), Ok(b)) => a == b,
            _ => site_toml == process,
        };
        let name = |p: &Path| {
            p.file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_string()
        };
        ConfigReloader {
            watcher,
            site_toml: site_toml.to_path_buf(),
            process: process.to_path_buf(),
            same_file,
            seen,
            pending: Changed::default(),
            first: None,
            due: None,
            window_end: None,
            next_poll: Instant::now() + RELOAD_POLL,
            site_name: name(site_toml),
            process_name: name(process),
        }
    }

    /// A reloader with no watcher, so only the poll can find a change.
    #[cfg(test)]
    fn polling_only(site_toml: &Path, process: &Path, seen: Seen) -> ConfigReloader {
        Self::with_watcher(None, site_toml, process, seen)
    }

    /// Make the next `tick` poll, whatever the time.
    #[cfg(test)]
    fn next_poll_for_test(&mut self, now: Instant) {
        self.next_poll = now;
    }

    /// The descriptor to add to the loop's `poll(2)`, if there is a watcher.
    pub fn raw_fd(&self) -> Option<RawFd> {
        self.watcher.as_ref().and_then(|w| w.raw_fd())
    }

    /// Call when [`raw_fd`](Self::raw_fd) is readable.
    pub fn on_readable(&mut self, now: Instant) {
        if let Some(w) = self.watcher.as_mut() {
            // The fingerprints decide what changed, not the event name.
            let _ = w.read_events(&[&self.site_name, &self.process_name]);
        }
        self.compare(now);
    }

    /// Call on every loop iteration. Returns the change when a report is due.
    pub fn tick(&mut self, now: Instant) -> Option<Changed> {
        if now >= self.next_poll {
            self.compare(now);
            self.next_poll = now + RELOAD_POLL;
        }
        match self.due {
            Some(due) if now >= due => {
                // Every report opens a window. A change inside it is held for
                // one more report when it settles.
                self.first = Some(now);
                self.due = None;
                self.window_end = Some(now + RELOAD_DEBOUNCE);
                Some(std::mem::take(&mut self.pending))
            }
            _ => {
                if self.due.is_none() && self.window_end.is_some_and(|w| now >= w) {
                    self.first = None;
                    self.window_end = None;
                }
                None
            }
        }
    }

    /// Fingerprint both files and mark each that moved. The new fingerprint is
    /// stored at once, so a file that fails to load is not retried until it is
    /// written again.
    fn compare(&mut self, now: Instant) {
        let site = Fingerprint::of(&self.site_toml);
        let process = Fingerprint::of(&self.process);
        let mut hit = false;
        if site != self.seen.site {
            self.seen.site = site;
            self.pending.site = true;
            hit = true;
        }
        if process != self.seen.process {
            self.seen.process = process;
            self.pending.process = true;
            hit = true;
        }
        if !hit {
            return;
        }
        if self.same_file {
            self.pending = Changed {
                site: true,
                process: true,
            };
        }
        self.due = Some(match (self.window_end, self.first) {
            // A change inside the window of an earlier report: one more report
            // when the burst settles, and never later than a second after the
            // window opened.
            (Some(_), Some(first)) => (now + RELOAD_DEBOUNCE).min(first + RELOAD_POLL).max(now),
            // The first change after quiet: report at once.
            _ => now,
        });
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
        assert!(
            w.raw_fd().is_some(),
            "a supported platform must expose an fd"
        );

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

    /// A timestamp-only change wakes the poller, and this is the one that was
    /// broken.
    ///
    /// `utimensat(2)` with no open is what `filetime::set_file_times` does, and
    /// therefore what m6-core's own `Request::touch` did. It reports `IN_ATTRIB`
    /// and nothing else, and the mask asked only for
    /// `IN_CLOSE_WRITE | IN_CREATE | IN_MOVED_TO`, so the event was read and
    /// thrown away. **The documented reload mechanism had never worked on Linux.**
    ///
    /// It worked on macOS, because kqueue registers the files themselves and
    /// reports the attribute change -- so on the machine this code was written on
    /// there was nothing to see, while on the platform that serves production a
    /// CMS publish rebuilt its index, reported success, and did not appear.
    ///
    /// The test writes no bytes on purpose. Touching the content would produce
    /// IN_CLOSE_WRITE and pass against the old mask, which is how the existing
    /// tests all passed: every one of them changed the file's contents.
    #[test]
    fn a_timestamp_only_touch_wakes_the_poller() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg = dir.path().join("app.conf");
        write_file(&cfg, "size = 1\n");

        let mut w = ConfigWatcher::new(&[cfg.as_path()]).expect("watcher");
        let _ = w.read_events(&["app.conf"]); // drain the setup's own events

        // No open, no write: only the timestamps move.
        let now = filetime::FileTime::now();
        filetime::set_file_times(&cfg, now, now).expect("set_file_times");

        assert!(
            readable_within(&w, 5_000),
            "a timestamp-only touch did not wake the poller; the documented \
             `touch site.toml` reload mechanism is broken on this platform"
        );
        assert!(
            w.read_events(&["app.conf"]),
            "the timestamp change arrived but was not reported for the watched name"
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
        assert!(
            !w.read_events(&["app.conf"]),
            "an idle watcher reported an event"
        );
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

    // ── ConfigReloader ───────────────────────────────────────────────────

    /// Write `body` to `path`. Callers vary the length so a change never
    /// depends on the filesystem's timestamp granularity.
    fn put(path: &Path, body: &str) {
        write_file(path, body);
    }

    /// Two files in one temp directory, written, with their fingerprints taken.
    fn pair() -> (
        tempfile::TempDir,
        std::path::PathBuf,
        std::path::PathBuf,
        Seen,
    ) {
        let dir = tempfile::tempdir().expect("tempdir");
        let site = dir.path().join("site.toml");
        let proc = dir.path().join("app.conf");
        put(&site, "a = 1\n");
        put(&proc, "b = 1\n");
        let seen = Seen::read(&site, &proc);
        (dir, site, proc, seen)
    }

    /// Run the loop's two calls until a change is due or `ms` real time passes.
    fn wait_for_change(r: &mut ConfigReloader, ms: u64) -> Option<Changed> {
        let end = Instant::now() + Duration::from_millis(ms);
        while Instant::now() < end {
            if let Some(fd) = r.raw_fd() {
                if crate::server::poll_listener_and_watcher(fd, None, 20).listener {
                    r.on_readable(Instant::now());
                }
            } else {
                std::thread::sleep(Duration::from_millis(20));
            }
            if let Some(c) = r.tick(Instant::now()) {
                return Some(c);
            }
        }
        None
    }

    #[test]
    fn a_touch_of_the_process_config_is_reported_as_process() {
        let (_d, site, proc, seen) = pair();
        let mut r = ConfigReloader::new(&site, &proc, seen);
        let now = filetime::FileTime::now();
        filetime::set_file_times(&proc, now, now).expect("touch");
        let c = wait_for_change(&mut r, 5_000).expect("a touch is a change");
        assert!(c.process, "{c:?}");
        assert!(!c.site, "{c:?}");
    }

    #[test]
    fn a_rename_over_site_toml_is_reported_as_site() {
        let (d, site, proc, seen) = pair();
        let mut r = ConfigReloader::new(&site, &proc, seen);
        let tmp = d.path().join("site.toml.new");
        put(&tmp, "a = 22222\n");
        std::fs::rename(&tmp, &site).expect("rename");
        let c = wait_for_change(&mut r, 5_000).expect("a rename is a change");
        assert!(c.site, "{c:?}");
        assert!(!c.process, "{c:?}");
    }

    #[test]
    fn a_burst_of_writes_is_at_most_two_reloads() {
        let (_d, site, proc, seen) = pair();
        let mut r = ConfigReloader::polling_only(&site, &proc, seen);
        let t0 = Instant::now() + RELOAD_POLL;
        put(&proc, "b = 12\n");
        // The first change is reported at once.
        assert_eq!(
            r.tick(t0),
            Some(Changed {
                site: false,
                process: true
            })
        );
        put(&proc, "b = 123\n");
        r.next_poll_for_test(t0 + Duration::from_millis(10));
        assert_eq!(r.tick(t0 + Duration::from_millis(10)), None);
        put(&proc, "b = 1234\n");
        r.next_poll_for_test(t0 + Duration::from_millis(20));
        assert_eq!(r.tick(t0 + Duration::from_millis(20)), None);
        // One more when the burst has settled, and then nothing.
        let c = r.tick(t0 + Duration::from_millis(20) + RELOAD_DEBOUNCE);
        assert_eq!(
            c,
            Some(Changed {
                site: false,
                process: true
            })
        );
        assert_eq!(r.tick(t0 + Duration::from_millis(600)), None);
    }

    #[test]
    fn a_single_write_is_one_reload_with_no_delay() {
        let (_d, site, proc, seen) = pair();
        let mut r = ConfigReloader::polling_only(&site, &proc, seen);
        let t0 = Instant::now() + RELOAD_POLL;
        put(&site, "a = 12\n");
        let c = r.tick(t0).expect("reported at once");
        assert!(c.site && !c.process, "{c:?}");
        for ms in [50, 100, 150, 400, 900] {
            r.next_poll_for_test(t0 + Duration::from_millis(ms));
            assert_eq!(r.tick(t0 + Duration::from_millis(ms)), None, "{ms} ms");
        }
    }

    #[test]
    fn a_writer_that_never_stops_still_gets_a_change_each_second() {
        let (_d, site, proc, seen) = pair();
        let mut r = ConfigReloader::polling_only(&site, &proc, seen);
        let t0 = Instant::now() + RELOAD_POLL;
        let mut reported = 0;
        // A write every 50 ms for just over two seconds, polled every 50 ms, so
        // the deadline is re-armed before it is ever reached.
        for i in 0..=45u64 {
            let now = t0 + Duration::from_millis(50 * i);
            put(&proc, &format!("b = {}\n", "1".repeat(2 + i as usize)));
            // The poll runs once a second, so make the loop poll every step.
            r.next_poll_for_test(now);
            if r.tick(now).is_some() {
                reported += 1;
            }
        }
        assert!(
            reported >= 2,
            "a writer that never stops starved the reload: {reported}"
        );
    }

    #[test]
    fn another_file_in_the_same_directory_is_not_a_change() {
        let (d, site, proc, seen) = pair();
        let mut r = ConfigReloader::new(&site, &proc, seen);
        put(&d.path().join("other.toml"), "x = 1\n");
        assert_eq!(wait_for_change(&mut r, 400), None);
    }

    #[test]
    fn one_file_for_both_reports_both() {
        let dir = tempfile::tempdir().expect("tempdir");
        let one = dir.path().join("site.toml");
        put(&one, "a = 1\n");
        let seen = Seen::read(&one, &one);
        let mut r = ConfigReloader::new(&one, &one, seen);
        put(&one, "a = 22\n");
        let c = wait_for_change(&mut r, 5_000).expect("change");
        assert!(c.site && c.process, "{c:?}");
    }

    #[test]
    fn the_poll_finds_a_change_with_no_watcher() {
        let (_d, site, proc, seen) = pair();
        let mut r = ConfigReloader::polling_only(&site, &proc, seen);
        assert_eq!(r.raw_fd(), None);
        put(&proc, "b = 99999\n");
        let t0 = Instant::now() + RELOAD_POLL;
        let c = r.tick(t0).expect("the poll saw it");
        assert!(c.process && !c.site, "{c:?}");
    }

    #[test]
    fn a_change_before_the_reloader_existed_is_reported_on_the_first_tick() {
        let (_d, site, proc, seen) = pair();
        // Seeded first, then written, then the reloader built: the write is in
        // the gap a watcher created afterwards would never see.
        put(&site, "a = 4444\n");
        let mut r = ConfigReloader::polling_only(&site, &proc, seen);
        let t0 = Instant::now() + RELOAD_POLL;
        let c = r.tick(t0).expect("reported");
        assert!(c.site && !c.process, "{c:?}");
    }

    #[test]
    fn a_failed_load_is_not_retried_until_the_next_write() {
        let (_d, site, proc, seen) = pair();
        let mut r = ConfigReloader::polling_only(&site, &proc, seen);
        put(&proc, "b = 5555\n");
        let t0 = Instant::now() + RELOAD_POLL;
        assert!(r.tick(t0).is_some());
        // Nothing written since: the next polls report nothing.
        assert_eq!(r.tick(t0 + RELOAD_POLL * 2), None);
        assert_eq!(r.tick(t0 + RELOAD_POLL * 3), None);
    }
}
