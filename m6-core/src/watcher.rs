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
        self.read_matched(filenames).contains(&true)
    }

    /// Drain pending events, returning for each of `filenames` whether an
    /// event named it.
    pub fn read_matched(&mut self, filenames: &[&str]) -> Vec<bool> {
        let mut matched = vec![false; filenames.len()];
        // One `read` per call, so loop until the queue is empty. An empty
        // non-blocking queue is `EAGAIN`, which is the normal way out rather
        // than an error to report.
        loop {
            match self.inotify.read_events() {
                Ok(events) if events.is_empty() => break,
                Ok(events) => {
                    for ev in events {
                        if let Some(name) = ev.name.as_ref().and_then(|n| n.to_str()) {
                            for (i, f) in filenames.iter().enumerate() {
                                if *f == name {
                                    matched[i] = true;
                                }
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

    /// Drain pending events. kqueue does not say which file, so nothing is
    /// reported as named and the caller falls back to its fingerprints.
    pub fn read_matched(&mut self, filenames: &[&str]) -> Vec<bool> {
        self.read_events(&[]);
        vec![false; filenames.len()]
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

    pub fn read_matched(&mut self, filenames: &[&str]) -> Vec<bool> {
        vec![false; filenames.len()]
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// ConfigReloader: both of a process's files, with a settle window and a poll
// ─────────────────────────────────────────────────────────────────────────────

/// The settle interval.
///
/// A file whose fingerprint differs from the last report is reported once its
/// fingerprint is the same at two checks one interval apart. A rename, or a
/// writer that truncates a file and then writes it, is therefore read in its
/// final state, and a burst of writes is one report. The first change after
/// quiet is reported this long after it is first seen.
///
/// The interval is short on purpose. Every held change is time in which an
/// edge's cache can refill from a backend that has not reloaded.
pub const RELOAD_DEBOUNCE: Duration = Duration::from_millis(20);

/// How often the reloader compares fingerprints whatever the watcher says, and
/// the longest a change is held back while the file keeps changing.
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

/// What matters about a config file, followed through a symlink.
///
/// Two fingerprints are equal when the file has the same bytes, mode, owner and
/// modification time. The bytes are what a reload reads, the mode and owner
/// decide whether it can, and the modification time makes a `touch` a change.
/// The inode and `ctime` are left out on purpose: a rename over a file bumps the
/// old inode's `ctime` just before the name moves, and that state has the old
/// bytes and times, so it is not a change. A file that cannot be read has no
/// hash, which is itself a fingerprint, so fixing its permissions is a change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fingerprint {
    hash: Option<u64>,
    mode: u32,
    uid: u32,
    gid: u32,
    mtime_ns: i128,
}

impl Fingerprint {
    /// The fingerprint of `path`, or `None` when it does not exist.
    pub fn of(path: &Path) -> Option<Fingerprint> {
        use std::os::unix::fs::MetadataExt;
        let m = std::fs::metadata(path).ok()?;
        Some(Fingerprint {
            hash: hash_file(path),
            mode: m.mode(),
            uid: m.uid(),
            gid: m.gid(),
            mtime_ns: i128::from(m.mtime()) * 1_000_000_000 + i128::from(m.mtime_nsec()),
        })
    }
}

/// A hash of the bytes of `path`, or `None` when it cannot be read. Read in
/// chunks from the stack, so a check allocates nothing.
fn hash_file(path: &Path) -> Option<u64> {
    use std::hash::Hasher;
    use std::io::Read;
    let mut file = std::fs::File::open(path).ok()?;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    let mut chunk = [0u8; 8192];
    loop {
        match file.read(&mut chunk) {
            Ok(0) => return Some(hasher.finish()),
            Ok(n) => hasher.write(&chunk[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return None,
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

/// A file whose fingerprint differs from the last report, and is settling.
#[derive(Debug, Clone, Copy)]
struct Settle {
    /// The fingerprint at the latest check.
    fingerprint: Option<Fingerprint>,
    /// When the file first had that fingerprint.
    since: Instant,
    /// When the file first differed from the last report.
    first: Instant,
}

impl Settle {
    /// When the file is reported if it stays as it is.
    fn due(&self) -> Instant {
        (self.since + RELOAD_DEBOUNCE).min(self.first + RELOAD_POLL)
    }
}

/// Watches `site.toml` and a process's own config and reports which changed.
///
/// A [`ConfigWatcher`] wakes the loop and the poll wakes it once a second, and
/// each only triggers a check. A check fingerprints both files and compares
/// them with what was last reported, so the answer is the same on inotify,
/// kqueue and with no watcher at all, and other files in a shared directory
/// change nothing. A file that differs is reported once its fingerprint is the
/// same at two checks [`RELOAD_DEBOUNCE`] apart, or [`RELOAD_POLL`] after it
/// first differed if it never stops changing. The loop calls
/// [`on_readable`](Self::on_readable) when [`raw_fd`](Self::raw_fd) is readable
/// and [`tick`](Self::tick) on every iteration, idle or not. No thread, no
/// allocation in `tick`.
pub struct ConfigReloader {
    watcher: Option<ConfigWatcher>,
    site_toml: PathBuf,
    process: PathBuf,
    /// The fingerprints last reported.
    seen: Seen,
    /// The file names events are matched against.
    site_name: String,
    process_name: String,
    site_settle: Option<Settle>,
    process_settle: Option<Settle>,
    /// Files reported as changed, waiting to be handed out.
    pending: Changed,
    /// When the next check of a settling file is due.
    settle_due: Option<Instant>,
    next_poll: Instant,
    /// Consecutive failed reloads that were asked to be retried.
    failures: u32,
    /// When a retry that was asked for is due.
    retry_due: Option<Instant>,
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
            site_settle: None,
            process_settle: None,
            pending: Changed::default(),
            settle_due: None,
            next_poll: Instant::now() + RELOAD_POLL,
            failures: 0,
            retry_due: None,
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

    /// The `poll(2)` timeout to use, at most `max_ms`: shorter when a check or a
    /// retry falls due sooner, so it is made on time on an idle loop.
    pub fn timeout_ms(&self, now: Instant, max_ms: u16) -> u16 {
        let due = [self.settle_due, self.retry_due]
            .into_iter()
            .flatten()
            .min();
        due.map_or(max_ms, |due| {
            let us = due.saturating_duration_since(now).as_micros();
            u16::try_from(us.div_ceil(1000)).map_or(max_ms, |ms| ms.min(max_ms))
        })
    }

    /// Call when [`raw_fd`](Self::raw_fd) is readable. Whatever the events were,
    /// including an overflow of the kernel's queue, they only ask for a check.
    pub fn on_readable(&mut self, now: Instant) {
        if let Some(w) = self.watcher.as_mut() {
            w.read_events(&[&self.site_name, &self.process_name]);
        }
        self.check(now);
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
        self.retry_due = Some(now + delay);
        delay
    }

    /// Call on every loop iteration. Returns the change when a report is due.
    pub fn tick(&mut self, now: Instant) -> Option<Changed> {
        let poll_due = now >= self.next_poll;
        if poll_due || self.settle_due.is_some_and(|due| now >= due) {
            self.check(now);
        }
        if poll_due {
            self.next_poll = now + RELOAD_POLL;
        }
        if self.pending.any() && self.retry_due.is_none_or(|due| now >= due) {
            self.retry_due = None;
            return Some(std::mem::take(&mut self.pending));
        }
        None
    }

    /// Fingerprint both files and compare each with what was last reported. A
    /// file that differs settles, and a file that has settled is reported and
    /// recorded, so a file that fails to load is not retried until it is written
    /// again.
    fn check(&mut self, now: Instant) {
        let site = Fingerprint::of(&self.site_toml);
        let process = Fingerprint::of(&self.process);
        let site_changed = Self::settle(site, &mut self.seen.site, &mut self.site_settle, now);
        let process_changed = Self::settle(
            process,
            &mut self.seen.process,
            &mut self.process_settle,
            now,
        );
        self.pending.site |= site_changed;
        self.pending.process |= process_changed;
        if site_changed || process_changed {
            // A real change, not a retry: the backoff starts again.
            self.failures = 0;
            self.retry_due = None;
        }
        self.settle_due = [self.site_settle, self.process_settle]
            .into_iter()
            .flatten()
            .map(|settle| settle.due())
            .min();
    }

    /// True when `current` has settled and is to be reported. Records it as
    /// reported then. Otherwise keeps track of how long it has been as it is.
    fn settle(
        current: Option<Fingerprint>,
        reported: &mut Option<Fingerprint>,
        settling: &mut Option<Settle>,
        now: Instant,
    ) -> bool {
        if current == *reported {
            *settling = None;
            return false;
        }
        let settle = match *settling {
            Some(s) if s.fingerprint == current => s,
            Some(s) => Settle {
                fingerprint: current,
                since: now,
                first: s.first,
            },
            None => Settle {
                fingerprint: current,
                since: now,
                first: now,
            },
        };
        if now >= settle.due() {
            *reported = current;
            *settling = None;
            return true;
        }
        *settling = Some(settle);
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

    /// Run the reloader's poll at `at`, which fingerprints both files then.
    fn poll_at(r: &mut ConfigReloader, at: Instant) -> Option<Changed> {
        r.next_poll_for_test(at);
        r.tick(at)
    }

    /// Poll at `at`, and again one settle interval later. A change that was
    /// there at `at` is reported by the second.
    fn changed_after_settling(r: &mut ConfigReloader, at: Instant) -> Option<Changed> {
        assert_eq!(poll_at(r, at), None, "reported before it settled");
        r.tick(at + RELOAD_DEBOUNCE)
    }

    /// Only the process config.
    const PROCESS: Changed = Changed {
        site: false,
        process: true,
    };

    /// A rename of the config is one report each, whenever the loop happens to
    /// wake relative to the rename, on a machine with every core busy.
    ///
    /// Ignored in the workspace run because its busy threads starve the other
    /// timing tests for about 20 s. Run it alone on the build host: `cargo test
    /// -p m6-core --lib -- --ignored each_rename_over_the_config`.
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
            // Anything reported inside the next 100 ms is a second report.
            if let Some(c) = wait_for_change(&mut r, 100) {
                extra.push((i, c));
            }
        }
        stop.store(true, Ordering::Relaxed);
        for b in burners {
            b.join().expect("burner");
        }
        assert!(extra.is_empty(), "renames reported twice: {extra:?}");
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

    /// The first change after quiet is reported one settle interval after it is
    /// seen, once, and not again.
    #[test]
    fn a_single_write_is_one_report_one_debounce_after_it_is_seen() {
        let (_d, site, proc, seen) = pair();
        let mut r = ConfigReloader::polling_only(&site, &proc, seen);
        let t0 = Instant::now() + RELOAD_POLL;
        put(&site, "a = 12\n");
        assert_eq!(poll_at(&mut r, t0), None);
        assert_eq!(r.timeout_ms(t0, 100), 20, "the loop must wake to settle");
        assert_eq!(
            r.tick(t0 + RELOAD_DEBOUNCE - Duration::from_millis(1)),
            None
        );
        let c = r.tick(t0 + RELOAD_DEBOUNCE).expect("reported when settled");
        assert!(c.site && !c.process, "{c:?}");
        for ms in [50, 100, 150, 400, 900] {
            assert_eq!(
                poll_at(&mut r, t0 + Duration::from_millis(ms)),
                None,
                "{ms} ms"
            );
        }
    }

    /// A write that lands while a change is settling restarts the settle, so a
    /// writer that truncates and then writes is read once, after the write.
    #[test]
    fn a_write_during_the_settle_restarts_it() {
        let (_d, site, proc, seen) = pair();
        let mut r = ConfigReloader::polling_only(&site, &proc, seen);
        let t0 = Instant::now() + RELOAD_POLL;
        put(&proc, "b = 12\n");
        assert_eq!(poll_at(&mut r, t0), None);
        let t1 = t0 + Duration::from_millis(10);
        put(&proc, "b = 123\n");
        assert_eq!(poll_at(&mut r, t1), None);
        assert_eq!(
            r.tick(t0 + RELOAD_DEBOUNCE),
            None,
            "the first settle was kept"
        );
        assert_eq!(r.tick(t1 + RELOAD_DEBOUNCE), Some(PROCESS));
    }

    #[test]
    fn a_burst_of_writes_is_one_report() {
        let (_d, site, proc, seen) = pair();
        let mut r = ConfigReloader::polling_only(&site, &proc, seen);
        let t0 = Instant::now() + RELOAD_POLL;
        for (i, body) in ["b = 12\n", "b = 123\n", "b = 1234\n"]
            .into_iter()
            .enumerate()
        {
            put(&proc, body);
            let at = t0 + Duration::from_millis(10 * i as u64);
            assert_eq!(poll_at(&mut r, at), None);
        }
        let settled = t0 + Duration::from_millis(20) + RELOAD_DEBOUNCE;
        assert_eq!(r.tick(settled), Some(PROCESS));
        assert_eq!(r.tick(t0 + Duration::from_millis(600)), None);
    }

    /// A change that follows a report is reported as soon as it settles, so a
    /// publisher that touches the config twice gets a reload for each.
    #[test]
    fn a_change_soon_after_a_report_is_reported_one_debounce_later() {
        let (_d, site, proc, seen) = pair();
        let mut r = ConfigReloader::polling_only(&site, &proc, seen);
        let t0 = Instant::now() + RELOAD_POLL;
        put(&proc, "b = 12\n");
        assert!(changed_after_settling(&mut r, t0).is_some());
        put(&proc, "b = 123\n");
        let at = t0 + Duration::from_millis(40);
        assert!(changed_after_settling(&mut r, at).is_some());
    }

    #[test]
    fn retries_back_off_to_a_cap() {
        let (_d, site, proc, seen) = pair();
        let mut r = ConfigReloader::polling_only(&site, &proc, seen);
        put(&proc, "b = 12\n");
        let t0 = Instant::now() + RELOAD_POLL;
        let mut c = changed_after_settling(&mut r, t0).expect("reported");
        let mut now = t0 + RELOAD_DEBOUNCE;
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
    fn a_real_write_after_failures_is_reported_once_settled_and_resets_the_backoff() {
        let (_d, site, proc, seen) = pair();
        let mut r = ConfigReloader::polling_only(&site, &proc, seen);
        put(&proc, "b = 12\n");
        let t0 = Instant::now() + RELOAD_POLL;
        let c = changed_after_settling(&mut r, t0).expect("reported");
        let mut now = t0 + RELOAD_DEBOUNCE;
        assert_eq!(r.retry(c, now), Duration::from_secs(1));
        now += Duration::from_secs(1);
        let c = r.tick(now).unwrap();
        assert_eq!(r.retry(c, now), Duration::from_secs(2));
        // A real write, long before the 2 s retry is due.
        put(&proc, "b = 123456\n");
        now += Duration::from_millis(300);
        let c = changed_after_settling(&mut r, now).expect("reported once settled");
        assert!(c.process);
        // And the next failure starts from one second again.
        assert_eq!(r.retry(c, now), Duration::from_secs(1));
    }

    /// A writer that never stops is reported once a second while it goes on,
    /// not never.
    #[test]
    fn a_writer_that_never_stops_still_gets_a_change_each_second() {
        let (_d, site, proc, seen) = pair();
        let mut r = ConfigReloader::polling_only(&site, &proc, seen);
        let t0 = Instant::now() + RELOAD_POLL;
        let mut reported = 0;
        // A write every 10 ms for just over two seconds, so the settle is
        // restarted before it is ever reached.
        for i in 0..=225u64 {
            let now = t0 + Duration::from_millis(10 * i);
            put(&proc, &format!("b = {}\n", "1".repeat(2 + i as usize)));
            if poll_at(&mut r, now).is_some() {
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

    /// Both files changing together are one report that names both, and no
    /// later report names either.
    #[test]
    fn both_files_changing_together_are_reported_once_each() {
        let (_d, site, proc, seen) = pair();
        let mut r = ConfigReloader::polling_only(&site, &proc, seen);
        let t0 = Instant::now() + RELOAD_POLL;
        put(&site, "a = 22\n");
        put(&proc, "b = 22\n");
        let c = changed_after_settling(&mut r, t0).expect("reported");
        assert!(c.site && c.process, "{c:?}");
        assert_eq!(poll_at(&mut r, t0 + RELOAD_POLL), None);
        assert_eq!(poll_at(&mut r, t0 + RELOAD_POLL * 2), None);
    }

    /// One file settling must not hold back, or cancel, the other.
    #[test]
    fn a_change_to_one_file_does_not_restart_the_settle_of_the_other() {
        let (_d, site, proc, seen) = pair();
        let mut r = ConfigReloader::polling_only(&site, &proc, seen);
        let t0 = Instant::now() + RELOAD_POLL;
        put(&site, "a = 22\n");
        assert_eq!(poll_at(&mut r, t0), None);
        let t1 = t0 + Duration::from_millis(10);
        put(&proc, "b = 22\n");
        assert_eq!(poll_at(&mut r, t1), None);
        let c = r.tick(t0 + RELOAD_DEBOUNCE).expect("site settled first");
        assert!(c.site && !c.process, "{c:?}");
        let c = r.tick(t1 + RELOAD_DEBOUNCE).expect("process settled next");
        assert!(c.process && !c.site, "{c:?}");
    }

    #[test]
    fn the_poll_finds_a_change_with_no_watcher() {
        let (_d, site, proc, seen) = pair();
        let mut r = ConfigReloader::polling_only(&site, &proc, seen);
        assert_eq!(r.raw_fd(), None);
        put(&proc, "b = 99999\n");
        let t0 = Instant::now() + RELOAD_POLL;
        let c = changed_after_settling(&mut r, t0).expect("the poll saw it");
        assert_eq!(c, PROCESS);
    }

    /// A lost watch, a full event queue and a watch that could not be added all
    /// leave events unread. The poll alone must still find the change.
    #[test]
    fn the_poll_finds_a_change_whose_events_are_never_read() {
        let (_d, site, proc, seen) = pair();
        let mut r = ConfigReloader::new(&site, &proc, seen);
        put(&proc, "b = 99999\n");
        let t0 = Instant::now() + RELOAD_POLL;
        assert_eq!(changed_after_settling(&mut r, t0), Some(PROCESS));
    }

    #[test]
    fn a_change_before_the_reloader_existed_is_reported_once_settled() {
        let (_d, site, proc, seen) = pair();
        // Seeded first, then written, then the reloader built: the write is in
        // the gap a watcher created afterwards would never see.
        put(&site, "a = 4444\n");
        let mut r = ConfigReloader::polling_only(&site, &proc, seen);
        let t0 = Instant::now() + RELOAD_POLL;
        let c = changed_after_settling(&mut r, t0).expect("reported");
        assert!(c.site && !c.process, "{c:?}");
    }

    #[test]
    fn a_failed_load_is_not_retried_until_the_next_write() {
        let (_d, site, proc, seen) = pair();
        let mut r = ConfigReloader::polling_only(&site, &proc, seen);
        put(&proc, "b = 5555\n");
        let t0 = Instant::now() + RELOAD_POLL;
        assert!(changed_after_settling(&mut r, t0).is_some());
        // Nothing written since: the next polls report nothing.
        assert_eq!(poll_at(&mut r, t0 + RELOAD_POLL * 2), None);
        assert_eq!(poll_at(&mut r, t0 + RELOAD_POLL * 3), None);
    }

    // ── what counts as a change ──────────────────────────────────────────

    fn mtime_of(path: &Path) -> filetime::FileTime {
        filetime::FileTime::from_last_modification_time(&std::fs::metadata(path).expect("stat"))
    }

    /// A rename shows, for an instant, the old file with a newer `ctime`. A new
    /// inode with the same bytes, mode, owner and mtime is the closest state a
    /// test can make on purpose, and it is not a change.
    #[test]
    fn a_new_inode_with_the_same_bytes_and_times_is_not_a_change() {
        let (d, site, proc, seen) = pair();
        let mut r = ConfigReloader::polling_only(&site, &proc, seen);
        let mtime = mtime_of(&proc);
        let tmp = d.path().join("app.conf.new");
        std::fs::copy(&proc, &tmp).expect("copy");
        filetime::set_file_mtime(&tmp, mtime).expect("mtime");
        std::fs::rename(&tmp, &proc).expect("rename");
        let t0 = Instant::now() + RELOAD_POLL;
        assert_eq!(poll_at(&mut r, t0), None);
        assert_eq!(r.tick(t0 + RELOAD_DEBOUNCE), None);
    }

    /// The completed rename is the change: once, whichever of the event and the
    /// poll finds it first.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_completed_rename_is_one_report() {
        let (d, site, proc, seen) = pair();
        let mut r = ConfigReloader::new(&site, &proc, seen);
        let tmp = d.path().join("app.conf.new");
        put(&tmp, "b = 22222\n");
        std::fs::rename(&tmp, &proc).expect("rename");
        let fd = r.raw_fd().unwrap();
        assert!(crate::server::poll_listener_and_watcher(fd, None, 5_000).listener);
        let now = Instant::now();
        r.on_readable(now);
        assert_eq!(r.tick(now), None);
        assert_eq!(r.tick(now + RELOAD_DEBOUNCE), Some(PROCESS));
        // The poll finds the same file and has nothing to add.
        assert_eq!(poll_at(&mut r, now + RELOAD_POLL), None);
        assert_eq!(r.tick(now + RELOAD_POLL * 2), None);
    }

    /// A `chmod` that fixes an unreadable config is a change: the mode differs.
    #[test]
    fn fixing_permissions_is_a_change() {
        use std::os::unix::fs::PermissionsExt as _;
        let (_d, site, proc, seen) = pair();
        let mut r = ConfigReloader::polling_only(&site, &proc, seen);
        let mut t = Instant::now() + RELOAD_POLL;
        for mode in [0o000, 0o644] {
            std::fs::set_permissions(&proc, std::fs::Permissions::from_mode(mode)).expect("chmod");
            let c = changed_after_settling(&mut r, t);
            assert_eq!(c, Some(PROCESS), "mode {mode:o}");
            t += RELOAD_POLL;
        }
    }

    /// A change of mode alone, on a file that stays readable, is a change.
    #[test]
    fn a_mode_change_alone_is_a_change() {
        use std::os::unix::fs::PermissionsExt as _;
        let (_d, site, proc, seen) = pair();
        let mut r = ConfigReloader::polling_only(&site, &proc, seen);
        let was = std::fs::metadata(&proc).expect("stat").permissions().mode() & 0o777;
        let now = if was == 0o600 { 0o640 } else { 0o600 };
        std::fs::set_permissions(&proc, std::fs::Permissions::from_mode(now)).expect("chmod");
        let t0 = Instant::now() + RELOAD_POLL;
        assert_eq!(changed_after_settling(&mut r, t0), Some(PROCESS));
    }

    /// A rewrite of the same length inside one timestamp tick, which leaves the
    /// mtime where it was, is a change because the bytes differ.
    #[test]
    fn a_same_length_rewrite_with_the_same_mtime_is_a_change() {
        let (_d, site, proc, seen) = pair();
        let mut r = ConfigReloader::polling_only(&site, &proc, seen);
        let mtime = mtime_of(&proc);
        put(&proc, "b = 2\n");
        filetime::set_file_mtime(&proc, mtime).expect("mtime");
        let t0 = Instant::now() + RELOAD_POLL;
        assert_eq!(changed_after_settling(&mut r, t0), Some(PROCESS));
    }

    /// `touch` is a change: the documented way to ask for a reload.
    #[test]
    fn a_touch_with_the_same_bytes_is_a_change() {
        let (_d, site, proc, seen) = pair();
        let mut r = ConfigReloader::polling_only(&site, &proc, seen);
        let later = filetime::FileTime::from_unix_time(mtime_of(&proc).unix_seconds() + 5, 0);
        filetime::set_file_mtime(&proc, later).expect("touch");
        let t0 = Instant::now() + RELOAD_POLL;
        assert_eq!(changed_after_settling(&mut r, t0), Some(PROCESS));
    }
}
