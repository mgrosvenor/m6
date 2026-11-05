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

/// What the events of one drain say about one file.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Named {
    /// The name arrived: a rename onto it, or a new file created at it.
    pub arrived: bool,
    /// The file was changed in place: written and closed, or its attributes
    /// set. Also set for every file when the kernel's event queue overflowed,
    /// because events were lost.
    pub touched: bool,
}

impl Named {
    /// True when any event named the file.
    pub fn any(self) -> bool {
        self.arrived || self.touched
    }
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
        self.read_matched(filenames).iter().any(|n| n.any())
    }

    /// Drain pending events, returning for each of `filenames` what the events
    /// said about it.
    pub fn read_matched(&mut self, filenames: &[&str]) -> Vec<Named> {
        let mut named = vec![Named::default(); filenames.len()];
        // One `read` per call, so loop until the queue is empty. An empty
        // non-blocking queue is `EAGAIN`, which is the normal way out rather
        // than an error to report.
        loop {
            match self.inotify.read_events() {
                Ok(events) if events.is_empty() => break,
                Ok(events) => fold_events(
                    &mut named,
                    filenames,
                    events
                        .iter()
                        .map(|ev| (ev.mask, ev.name.as_ref().and_then(|n| n.to_str()))),
                ),
                Err(nix::errno::Errno::EAGAIN) => break,
                Err(e) => {
                    tracing::warn!(error = %e, "inotify read failed");
                    break;
                }
            }
        }
        named
    }
}

/// Fold inotify events into what each of `filenames` was told.
///
/// A rename onto a name and a file created at it are an arrival. A write that
/// is closed and a change of attributes are a touch. An overflow means events
/// were lost, so every file is treated as touched and the reloader looks at
/// each one.
#[cfg(target_os = "linux")]
fn fold_events<'a>(
    named: &mut [Named],
    filenames: &[&str],
    events: impl IntoIterator<Item = (nix::sys::inotify::AddWatchFlags, Option<&'a str>)>,
) {
    use nix::sys::inotify::AddWatchFlags;
    for (mask, name) in events {
        if mask.contains(AddWatchFlags::IN_Q_OVERFLOW) {
            named.iter_mut().for_each(|n| n.touched = true);
        }
        let Some(name) = name else { continue };
        for (n, f) in named.iter_mut().zip(filenames) {
            if *f == name {
                if mask.intersects(AddWatchFlags::IN_MOVED_TO | AddWatchFlags::IN_CREATE) {
                    n.arrived = true;
                } else {
                    n.touched = true;
                }
            }
        }
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

    /// Drain pending events. kqueue does not say which file, so nothing is
    /// reported as named and the caller falls back to its fingerprints.
    pub fn read_matched(&mut self, filenames: &[&str]) -> Vec<Named> {
        self.read_events(&[]);
        vec![Named::default(); filenames.len()]
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

    pub fn read_matched(&mut self, filenames: &[&str]) -> Vec<Named> {
        vec![Named::default(); filenames.len()]
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
///
/// The window is short on purpose. It has to cover a writer that truncates a
/// file and then writes it, which the first report can land between, and it
/// must not hold back a second touch that follows the first by a few tens of
/// milliseconds, because every held change is time in which an edge's cache can
/// refill from a backend that has not reloaded.
pub const RELOAD_DEBOUNCE: Duration = Duration::from_millis(20);

/// How often the reloader compares fingerprints whatever the watcher says, and
/// the longest a batch of changes is held back.
///
/// A watch that has been lost, a replaced directory, or a kqueue watch left on
/// an unlinked inode after an atomic rename all stop delivering events. This
/// poll is two `stat` calls a second and finds the change anyway.
pub const RELOAD_POLL: Duration = Duration::from_secs(1);

/// The longest delay before a failed reload is retried.
pub const RETRY_CAP: Duration = Duration::from_secs(60);

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

fn file_name(p: &Path) -> String {
    p.file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_string()
}

/// What `stat` says about a file, followed through a symlink.
///
/// Two fingerprints are equal when the same bytes are in the same file with the
/// same times. A touch or a `chmod` moves `ctime` even when nothing else moves.
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

    /// True when only `ctime` differs. A rename over a file does this to the old
    /// inode just before the name moves, so a `stat` in that gap sees a change
    /// that is not the new file.
    fn differs_in_ctime_only(self, other: Fingerprint) -> bool {
        self != other
            && Fingerprint {
                ctime_ns: 0,
                ..self
            } == Fingerprint {
                ctime_ns: 0,
                ..other
            }
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
    seen: Seen,
    /// The file names events are matched against.
    site_name: String,
    process_name: String,
    pending: Changed,
    /// For each file whose last change was found by fingerprint alone, when
    /// that was, because its event has not been read yet.
    owed_site: Option<Instant>,
    owed_process: Option<Instant>,
    /// When the pending change is to be reported.
    due: Option<Instant>,
    /// When the last report was made. A change within [`RELOAD_DEBOUNCE`] of
    /// it, or one arriving while a report is still due, is held.
    last_report: Option<Instant>,
    next_poll: Instant,
    /// Consecutive failed reloads that were asked to be retried.
    failures: u32,
    /// True while `due` is a retry and not a change.
    retrying: bool,
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
        ConfigReloader {
            watcher,
            site_toml: site_toml.to_path_buf(),
            process: process.to_path_buf(),
            seen,
            site_name: file_name(site_toml),
            process_name: file_name(process),
            pending: Changed::default(),
            owed_site: None,
            owed_process: None,
            due: None,
            last_report: None,
            next_poll: Instant::now() + RELOAD_POLL,
            failures: 0,
            retrying: false,
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

    /// The `poll(2)` timeout to use, at most `max_ms`: shorter when a held
    /// report falls due sooner, so it is made on time on an idle loop.
    pub fn timeout_ms(&self, now: Instant, max_ms: u16) -> u16 {
        self.due.map_or(max_ms, |due| {
            let us = due.saturating_duration_since(now).as_micros();
            u16::try_from(us.div_ceil(1000)).map_or(max_ms, |ms| ms.min(max_ms))
        })
    }

    /// Call when [`raw_fd`](Self::raw_fd) is readable.
    pub fn on_readable(&mut self, now: Instant) {
        // An event that names a file is a change even when its fingerprint
        // is equal: with coarse timestamps a same-length rewrite, or two
        // renames that reuse an inode within one tick, leave the fingerprint
        // as it was, and the poll compares only fingerprints.
        let named = match self.watcher.as_mut() {
            Some(w) => {
                let m = w.read_matched(&[&self.site_name, &self.process_name]);
                [m[0], m[1]]
            }
            None => [Named::default(); 2],
        };
        self.compare(now, named);
    }

    /// Ask for `changed` to be reported again after a delay, and return the
    /// delay. Use it when a reload failed for a reason that is not the file's
    /// fault and a retry may succeed.
    ///
    /// The delay doubles with each consecutive failure, from one second to
    /// [`RETRY_CAP`]. A real change to either file reports as usual and starts
    /// the delay again at one second.
    pub fn retry(&mut self, changed: Changed, now: Instant) -> Duration {
        let delay = RELOAD_POLL
            .saturating_mul(1u32 << self.failures.min(6))
            .min(RETRY_CAP);
        self.failures = self.failures.saturating_add(1);
        self.pending.site |= changed.site;
        self.pending.process |= changed.process;
        self.due = Some(now + delay);
        self.retrying = true;
        delay
    }

    /// Call on every loop iteration. Returns the change when a report is due.
    pub fn tick(&mut self, now: Instant) -> Option<Changed> {
        if now >= self.next_poll {
            self.expire_owed(now);
            self.compare(now, [Named::default(); 2]);
            self.next_poll = now + RELOAD_POLL;
        }
        if self.due.is_some_and(|due| now >= due) {
            // Every report opens a window. A change inside it is held for one
            // more report when it settles.
            self.due = None;
            self.retrying = false;
            self.last_report = Some(now);
            return Some(std::mem::take(&mut self.pending));
        }
        None
    }

    /// Forget an event owed for a whole poll interval: it is not coming, for
    /// instance because the watch was lost.
    fn expire_owed(&mut self, now: Instant) {
        for owed in [&mut self.owed_site, &mut self.owed_process] {
            if owed.is_some_and(|since| now >= since + RELOAD_POLL) {
                *owed = None;
            }
        }
    }

    /// Decide whether one file changed, from its fingerprint and the events of
    /// this drain, and keep `owed` up to date.
    ///
    /// A rename shows in `stat` as soon as the name moves, a moment before its
    /// event is queued. A compare in that gap finds the change by fingerprint
    /// alone, and the arrival event then comes with an equal fingerprint. That
    /// one event is the echo of the change already found, so it is dropped. An
    /// event that touched the file is a change the fingerprint cannot show, so
    /// it counts even in the same drain as an echo.
    fn settle(
        new: Option<Fingerprint>,
        seen: Option<Fingerprint>,
        named: Named,
        owed: &mut Option<Instant>,
        now: Instant,
    ) -> Found {
        if new != seen {
            *owed = (!named.arrived).then_some(now);
            let transient = new
                .zip(seen)
                .is_some_and(|(new, seen)| new.differs_in_ctime_only(seen));
            return if transient && !named.any() {
                Found::Metadata
            } else {
                Found::Content
            };
        }
        let echo = named.arrived && owed.take().is_some();
        if named.touched || (named.arrived && !echo) {
            Found::Content
        } else {
            Found::Nothing
        }
    }

    /// Fingerprint both files and mark each that moved. The new fingerprint is
    /// stored at once, so a file that fails to load is not retried until it is
    /// written again.
    fn compare(&mut self, now: Instant, [site_named, process_named]: [Named; 2]) {
        let site = Fingerprint::of(&self.site_toml);
        let process = Fingerprint::of(&self.process);
        let site_found = Self::settle(site, self.seen.site, site_named, &mut self.owed_site, now);
        let process_found = Self::settle(
            process,
            self.seen.process,
            process_named,
            &mut self.owed_process,
            now,
        );
        if site_found != Found::Nothing {
            self.seen.site = site;
            self.pending.site = true;
        }
        if process_found != Found::Nothing {
            self.seen.process = process;
            self.pending.process = true;
        }
        let found = site_found.max(process_found);
        if found == Found::Nothing {
            return;
        }
        // One file serving as both: a change to it is a change to both.
        if site.is_some() && site == process {
            self.pending = Changed {
                site: true,
                process: true,
            };
        }
        // A real change, not a retry: the backoff starts again.
        self.failures = 0;
        let held_by_a_change = self.due.is_some() && !self.retrying;
        self.retrying = false;
        let at = match self.last_report {
            // A change inside the window of an earlier report: one more report
            // when the burst settles, and never later than a second after that
            // report.
            Some(last) if now < last + RELOAD_DEBOUNCE || held_by_a_change => {
                (now + RELOAD_DEBOUNCE).min(last + RELOAD_POLL).max(now)
            }
            // The first change after quiet: report at once.
            _ => now,
        };
        self.due = Some(if found == Found::Metadata {
            // Only `ctime` moved, which is also how a rename looks for an
            // instant before the name moves. Wait one debounce interval, so the
            // rename's own change lands first and the two are one report. An
            // earlier report already due stays.
            let settled = at.max(now + RELOAD_DEBOUNCE);
            self.due
                .filter(|_| held_by_a_change)
                .map_or(settled, |due| due.min(settled))
        } else {
            at
        });
    }
}

/// What one compare found in a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Found {
    Nothing,
    /// Only `ctime` moved, and no event named the file.
    Metadata,
    /// The file or an event says it changed.
    Content,
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

    /// A rename of the config is one report each, whenever the loop happens to
    /// wake relative to the rename.
    ///
    /// Ignored in the workspace run because its busy threads starve the other
    /// timing tests. Run it alone on the build host: `cargo test -p m6-core
    /// --lib -- --ignored each_rename_over_the_config`.
    #[test]
    #[ignore = "busy threads starve other timing tests; run alone, see above"]
    fn each_rename_over_the_config_is_one_report_under_load() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;
        const RENAMES: usize = 300;
        let (d, site, proc, seen) = pair();
        let stop = Arc::new(AtomicBool::new(false));
        let burners: Vec<_> = (0..8)
            .map(|_| {
                let stop = stop.clone();
                std::thread::spawn(move || {
                    while !stop.load(Ordering::Relaxed) {
                        std::hint::spin_loop();
                    }
                })
            })
            .collect();
        let mut r = ConfigReloader::new(&site, &proc, seen);
        let tmp = d.path().join("app.conf.new");
        let mut extra = Vec::new();
        for i in 0..RENAMES {
            std::thread::scope(|s| {
                s.spawn(|| {
                    put(&tmp, &format!("b = {}\n", "2".repeat(i % 50 + 2)));
                    std::fs::rename(&tmp, &proc).expect("rename");
                });
                assert!(
                    wait_for_change(&mut r, 5_000).is_some(),
                    "rename {i} not reported"
                );
            });
            if let Some(c) = wait_for_change(&mut r, 40) {
                extra.push((i, c));
            }
        }
        stop.store(true, Ordering::Relaxed);
        for b in burners {
            b.join().expect("burner");
        }
        assert!(extra.is_empty(), "renames reported twice: {extra:?}");
    }

    /// The poll finds a rename before the loop has read its event, and the
    /// event must not report the same rename again.
    #[test]
    fn an_event_for_a_rename_the_poll_already_found_is_not_a_second_report() {
        let (d, site, proc, seen) = pair();
        let mut r = ConfigReloader::new(&site, &proc, seen);
        let tmp = d.path().join("app.conf.new");
        put(&tmp, "b = 22222\n");
        std::fs::rename(&tmp, &proc).expect("rename");
        let now = Instant::now();
        r.next_poll_for_test(now);
        assert!(r.tick(now).is_some(), "the poll missed the rename");
        let fd = r.raw_fd().unwrap();
        assert!(crate::server::poll_listener_and_watcher(fd, None, 5_000).listener);
        r.on_readable(now);
        let later = now + RELOAD_DEBOUNCE;
        assert!(
            r.tick(later).is_none(),
            "the rename's event reported it a second time"
        );
        assert!(r.due.is_none(), "a second report is waiting to be made");
    }

    /// Record that the process config's `ctime` was one tick earlier than it is
    /// now, as it is when a `chmod` follows a write, or when a `stat` lands
    /// between a rename's `ctime` bump and the name moving. Done by hand so the
    /// test does not depend on the clock the kernel stamps `ctime` from.
    fn record_an_earlier_ctime(r: &mut ConfigReloader) {
        let mut fp = r.seen.process.expect("the config was fingerprinted");
        fp.ctime_ns -= 1;
        r.seen.process = Some(fp);
    }

    /// The reloader's poll at `now`, which must find a change if there is one.
    fn poll_at(r: &mut ConfigReloader, now: Instant) -> Option<Changed> {
        r.next_poll_for_test(now);
        r.tick(now)
    }

    /// A change to `ctime` alone is reported after one debounce interval, with no
    /// event to say anything. kqueue and a reloader with no watcher are this
    /// case, and so is the poll of an inotify reloader.
    fn assert_a_ctime_change_is_reported_after_the_debounce(mut r: ConfigReloader) {
        let now = Instant::now();
        record_an_earlier_ctime(&mut r);
        assert!(poll_at(&mut r, now).is_none(), "reported at once");
        let c = r
            .tick(now + RELOAD_DEBOUNCE)
            .expect("a changed ctime was never reported");
        assert!(c.process && !c.site, "{c:?}");
        assert!(
            r.tick(now + RELOAD_DEBOUNCE * 3).is_none(),
            "reported twice"
        );
    }

    #[test]
    fn a_changed_ctime_is_reported_when_no_watcher_exists() {
        let (_d, site, proc, seen) = pair();
        assert_a_ctime_change_is_reported_after_the_debounce(ConfigReloader::polling_only(
            &site, &proc, seen,
        ));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_changed_ctime_is_reported_when_inotify_is_watching() {
        let (_d, site, proc, seen) = pair();
        assert_a_ctime_change_is_reported_after_the_debounce(ConfigReloader::new(
            &site, &proc, seen,
        ));
    }

    /// A `chmod` that fixes an unreadable config is reported at once when inotify
    /// names it, through the attribute event and not through the clock.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_chmod_is_reported_at_once_when_inotify_names_it() {
        use std::os::unix::fs::PermissionsExt as _;
        let (_d, site, proc, seen) = pair();
        let mut r = ConfigReloader::new(&site, &proc, seen);
        std::fs::set_permissions(&proc, std::fs::Permissions::from_mode(0o600)).expect("chmod");
        let fd = r.raw_fd().unwrap();
        assert!(crate::server::poll_listener_and_watcher(fd, None, 5_000).listener);
        let now = Instant::now();
        r.on_readable(now);
        let c = r.tick(now).expect("the chmod was not reported at once");
        assert!(c.process && !c.site, "{c:?}");
    }

    /// A `stat` that lands after a rename bumped the old inode's `ctime` and
    /// before the name moved sees a state that is not the new file. It must not
    /// be reported on its own: the rename that follows is the one report.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_transient_state_of_a_rename_is_not_reported_on_its_own() {
        let (d, site, proc, seen) = pair();
        let mut r = ConfigReloader::new(&site, &proc, seen);
        let now = Instant::now();
        record_an_earlier_ctime(&mut r);
        assert!(poll_at(&mut r, now).is_none(), "the transient was reported");
        let tmp = d.path().join("app.conf.new");
        put(&tmp, "b = 22222\n");
        std::fs::rename(&tmp, &proc).expect("rename");
        let fd = r.raw_fd().unwrap();
        assert!(crate::server::poll_listener_and_watcher(fd, None, 5_000).listener);
        r.on_readable(now);
        let c = r.tick(now).expect("the rename was not reported at once");
        assert!(c.process && !c.site, "{c:?}");
        assert!(
            r.tick(now + RELOAD_DEBOUNCE * 3).is_none(),
            "reported twice"
        );
        assert!(r.due.is_none(), "a second report is waiting to be made");
    }

    /// The loop finds a rename by fingerprint in one iteration and reads its
    /// event in the next, and the poll falls due in between. The poll must not
    /// forget the event is owed.
    #[test]
    fn a_poll_between_a_found_rename_and_its_event_does_not_forget_the_event() {
        let (d, site, proc, seen) = pair();
        let mut r = ConfigReloader::new(&site, &proc, seen);
        let tmp = d.path().join("app.conf.new");
        put(&tmp, "b = 22222\n");
        std::fs::rename(&tmp, &proc).expect("rename");
        let now = Instant::now();
        r.compare(now, [Named::default(); 2]);
        r.next_poll_for_test(now);
        assert!(r.tick(now).is_some(), "the rename was not reported");
        let fd = r.raw_fd().unwrap();
        assert!(crate::server::poll_listener_and_watcher(fd, None, 5_000).listener);
        r.on_readable(now);
        assert!(
            r.tick(now + RELOAD_DEBOUNCE).is_none(),
            "the rename's event reported it a second time"
        );
        assert!(r.due.is_none(), "a second report is waiting to be made");
    }

    /// A rewrite of the config that reaches the queue before the loop drains it,
    /// beside the echo of a rename the poll already found, is its own change.
    /// Coarse timestamps can leave its fingerprint equal, so only the event shows
    /// it.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_rewrite_in_the_same_drain_as_a_rename_echo_is_still_reported() {
        let (d, site, proc, seen) = pair();
        let mut r = ConfigReloader::new(&site, &proc, seen);
        let tmp = d.path().join("app.conf.new");
        put(&tmp, "b = 22222\n");
        std::fs::rename(&tmp, &proc).expect("rename");
        let now = Instant::now();
        r.compare(now, [Named::default(); 2]);
        assert!(r.tick(now).is_some(), "the rename was not reported");
        put(&proc, "b = 33333\n");
        r.seen = Seen::read(&site, &proc);
        let fd = r.raw_fd().unwrap();
        assert!(crate::server::poll_listener_and_watcher(fd, None, 5_000).listener);
        r.on_readable(now);
        assert!(
            r.tick(now + RELOAD_DEBOUNCE).is_some(),
            "the rewrite was taken for the rename's echo"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn events_are_folded_per_file_by_what_they_say() {
        use nix::sys::inotify::AddWatchFlags as F;
        let files = ["site.toml", "app.conf"];
        let fold = |events: Vec<(F, Option<&str>)>| {
            let mut named = [Named::default(); 2];
            fold_events(&mut named, &files, events);
            named
        };
        let n = fold(vec![
            (F::IN_MOVED_TO, Some("app.conf")),
            (F::IN_CREATE, Some("other")),
        ]);
        assert_eq!(
            n[1],
            Named {
                arrived: true,
                touched: false
            }
        );
        assert_eq!(n[0], Named::default());
        let n = fold(vec![
            (F::IN_ATTRIB, Some("app.conf")),
            (F::IN_CLOSE_WRITE, Some("site.toml")),
        ]);
        assert_eq!(
            n[1],
            Named {
                arrived: false,
                touched: true
            }
        );
        assert_eq!(
            n[0],
            Named {
                arrived: false,
                touched: true
            }
        );
    }

    /// Events lost to a full queue could have named either file, so each is
    /// treated as touched and compared.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_queue_overflow_touches_every_file() {
        use nix::sys::inotify::AddWatchFlags as F;
        let mut named = [Named::default(); 2];
        fold_events(
            &mut named,
            &["site.toml", "app.conf"],
            [(F::IN_Q_OVERFLOW, None)],
        );
        assert!(named.iter().all(|n| n.touched && !n.arrived), "{named:?}");
    }

    /// A touch forced by an overflow is a change even when the fingerprint is
    /// equal, as any other touch is.
    #[test]
    fn a_touched_file_is_a_change_even_when_its_fingerprint_is_equal() {
        let (_d, site, proc, seen) = pair();
        let mut r = ConfigReloader::polling_only(&site, &proc, seen);
        let now = Instant::now();
        r.compare(
            now,
            [
                Named::default(),
                Named {
                    arrived: false,
                    touched: true,
                },
            ],
        );
        let c = r.tick(now).expect("a touch was not reported");
        assert!(c.process && !c.site, "{c:?}");
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
    fn the_poll_timeout_shrinks_to_a_held_report() {
        let (_d, site, proc, seen) = pair();
        let mut r = ConfigReloader::polling_only(&site, &proc, seen);
        let t0 = Instant::now() + RELOAD_POLL;
        assert_eq!(r.timeout_ms(t0, 100), 100, "nothing held");
        put(&proc, "b = 12\n");
        assert!(r.tick(t0).is_some());
        put(&proc, "b = 123\n");
        let at = t0 + Duration::from_millis(5);
        r.next_poll_for_test(at);
        assert_eq!(r.tick(at), None);
        // Held until `at + RELOAD_DEBOUNCE`, so the loop must wake by then.
        assert_eq!(r.timeout_ms(at, 100), 20);
        assert_eq!(r.timeout_ms(at, 10), 10);
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
    fn a_change_soon_after_a_report_is_not_held_for_long() {
        // A publisher that touches the config twice in quick succession needs
        // the second reload promptly: an edge clears its cache on each touch,
        // and a backend that reloads late leaves a stale page cached.
        let (_d, site, proc, seen) = pair();
        let mut r = ConfigReloader::polling_only(&site, &proc, seen);
        let t0 = Instant::now() + RELOAD_POLL;
        put(&proc, "b = 12\n");
        assert!(r.tick(t0).is_some());
        put(&proc, "b = 123\n");
        let at = t0 + Duration::from_millis(40);
        r.next_poll_for_test(at);
        assert!(r.tick(at).is_some(), "a change 40 ms later was held back");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn an_event_naming_a_file_is_a_change_even_when_its_fingerprint_is_equal() {
        let (_d, site, proc, seen) = pair();
        let mut r = ConfigReloader::new(&site, &proc, seen);
        put(&proc, "b = 2\n");
        // Coarse timestamps can leave the fingerprint of a same-length rewrite
        // equal. Make it so: the reloader already holds the post-write one.
        r.seen = Seen::read(&site, &proc);
        let fd = r.raw_fd().unwrap();
        assert!(crate::server::poll_listener_and_watcher(fd, None, 5_000).listener);
        r.on_readable(Instant::now());
        let c = r.tick(Instant::now()).expect("the event was a change");
        assert!(c.process && !c.site, "{c:?}");
    }

    #[test]
    fn retries_back_off_to_a_cap() {
        let (_d, site, proc, seen) = pair();
        let mut r = ConfigReloader::polling_only(&site, &proc, seen);
        put(&proc, "b = 12\n");
        let mut now = Instant::now() + RELOAD_POLL;
        let mut c = r.tick(now).expect("reported");
        let mut delays = Vec::new();
        for _ in 0..9 {
            let d = r.retry(c, now);
            delays.push(d.as_secs());
            // Not before the delay, then reported.
            assert_eq!(r.tick(now + d - Duration::from_millis(1)), None);
            now += d;
            c = r.tick(now).expect("the retry is reported when due");
        }
        assert_eq!(delays, [1, 2, 4, 8, 16, 32, 60, 60, 60]);
    }

    #[test]
    fn a_real_write_after_failures_is_reported_at_once_and_resets_the_backoff() {
        let (_d, site, proc, seen) = pair();
        let mut r = ConfigReloader::polling_only(&site, &proc, seen);
        put(&proc, "b = 12\n");
        let mut now = Instant::now() + RELOAD_POLL;
        let c = r.tick(now).expect("reported");
        assert_eq!(r.retry(c, now), Duration::from_secs(1));
        now += Duration::from_secs(1);
        let c = r.tick(now).unwrap();
        assert_eq!(r.retry(c, now), Duration::from_secs(2));
        // A real write, long before the 2 s retry is due.
        put(&proc, "b = 123456\n");
        now += Duration::from_millis(300);
        r.next_poll_for_test(now);
        let c = r.tick(now + RELOAD_DEBOUNCE).expect("reported at once");
        assert!(c.process);
        // And the next failure starts from one second again.
        assert_eq!(r.retry(c, now), Duration::from_secs(1));
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
