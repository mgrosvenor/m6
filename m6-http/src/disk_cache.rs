//! A cache node's disk copy of its memory cache, written in slices off the
//! request path.
//!
//! The request path never touches a file. A stored entry carries a `queued`
//! flag, set in the same insert that stores it, and a disk slice scans the
//! cache for flagged entries and does all the disk work, up to
//! [`WRITE_BUDGET`] bytes per tick. The tick stops after [`TIME_BUDGET`] of
//! loop time, checked between files, so a full queue costs the loop about a
//! millisecond and the rest waits for the slices that follow. While
//! [`DiskWriter::has_work`] is true the loop polls without blocking, so a slice
//! rides each idle turn; when the queue is empty it returns to its normal wait.
//! A slice that runs out of any budget leaves the remaining entries flagged, so
//! the next slice continues where it stopped. Nothing is copied or pushed on
//! the request path, so a store for the writer allocates nothing beyond the
//! store it already made.
//!
//! The layout is one folder: each entry is `<dir>/<sha256 of the key>`, and the
//! change counter is `<dir>/change-counter`, a decimal u64 and a newline. A
//! file is written to `<name>.tmp` with one `write_all`, then renamed. No
//! `fsync`: a crash can leave a short or torn file, and the check value catches
//! it at load. The counter is not a generation: it reconciles a disk copy after
//! a cache missed purges while away. On reconnect a cache presents its saved
//! counter and the origin answers its current one. Equal keeps the disk copy;
//! different, or a missing file, purges the folder and refills.
//!
//! A restart reads the folder with [`load`], before the loop serves. The
//! decoded entries go into a fresh [`Cache`] bounded like the live one, and
//! the copy is held until the first registration answer decides whether to
//! keep or purge it. A file that is short or fails its check value is deleted
//! and counted, never served and never a reason to refuse to start.
//!
//! The format, all integers little-endian:
//!
//! ```text
//! b"M6DC" , u8 version = 2
//! u32 meta_len, meta: JSON {"key": "...", "status": ..., "headers": [[...]]}
//! u64 body_len, body
//! [u8; 32] SHA-256 of every byte before it
//! ```

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

use crate::cache::{Cache, CacheKey, CachedResponse};

/// Loop time one slice may spend on disk work, removals included. The write and
/// removal budgets below are upper bounds that a slice may still hit first. A
/// file is never split, so the slice checks this between files and stops before
/// starting one it cannot afford: the loop waits about a millisecond, not the
/// half second a full queue used to take.
pub const TIME_BUDGET: Duration = Duration::from_millis(1);

/// Bytes of files one tick may write. A single entry larger than the budget is
/// written on its own, because a file is never split.
pub const WRITE_BUDGET: u64 = 4 * 1024 * 1024;

const MAGIC: &[u8; 4] = b"M6DC";
const VERSION: u8 = 2;

/// The change-counter file name, one decimal u64 and a newline.
///
/// The counter is not a generation: it exists only to reconcile a disk copy
/// after a cache missed purges while away. On reconnect a cache presents its
/// saved counter and the origin answers with its current one. Equal keeps the
/// disk copy; different, or a missing file, purges the folder and refills.
pub const CHANGE_COUNTER: &str = "change-counter";

/// Read the saved change counter, or `None` when there is no file.
///
/// A file that does not hold a decimal u64 reads as missing, so a torn or
/// hand-edited counter reconciles by purging rather than by trusting garbage.
pub fn read_change_counter(dir: &Path) -> std::io::Result<Option<u64>> {
    match std::fs::read_to_string(dir.join(CHANGE_COUNTER)) {
        Ok(text) => Ok(text.trim().parse::<u64>().ok()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Write the change counter to a temp file and rename it over the saved file,
/// so a reader sees either the old counter or the new one, never a torn write.
pub fn write_change_counter(dir: &Path, counter: u64) -> std::io::Result<()> {
    let path = dir.join(CHANGE_COUNTER);
    let mut tmp = path.to_path_buf().into_os_string();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    let result = (|| {
        use std::io::Write as _;
        let mut file = std::fs::File::create(&tmp)?;
        writeln!(file, "{counter}")?;
        drop(file);
        std::fs::rename(&tmp, &path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// Delete every cache file in `dir`, leaving the change-counter file alone.
///
/// This is the purge half of the counter reconciliation: a cache that
/// reconnects with a different or missing counter drops its disk copy and
/// refills on demand. It runs off the request path, on the counter change.
pub fn purge_files(dir: &Path) -> std::io::Result<usize> {
    let mut removed = 0usize;
    for entry in std::fs::read_dir(dir)? {
        let Ok(entry) = entry else { continue };
        if !entry.file_type().is_ok_and(|t| t.is_file()) {
            continue;
        }
        if entry.file_name() == CHANGE_COUNTER {
            continue;
        }
        match std::fs::remove_file(entry.path()) {
            Ok(()) => removed += 1,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    Ok(removed)
}

/// What one tick of the writer did, for the metrics document and the tests.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct TickStats {
    /// Files written in this tick.
    pub written: usize,
    /// Files removed from old generation directories in this tick.
    pub removed: usize,
}

/// Why a disk cache file could not be read.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum DecodeError {
    /// The file ends before a field it declares.
    #[error("short file")]
    Short,
    /// The check value does not match the bytes before it.
    #[error("check value mismatch")]
    Check,
    /// The four magic bytes are not `M6DC`.
    #[error("not an m6 disk cache file")]
    Magic,
    /// A version this build does not know.
    #[error("unsupported version {0}")]
    Version(u8),
    /// The metadata is not the JSON this format declares.
    #[error("bad metadata: {0}")]
    Meta(String),
}

/// The writer's state: what to write, where, how much is there, and what is
/// waiting to be removed.
pub struct DiskWriter {
    dir: PathBuf,
    /// The change counter this folder's files were saved under. It is a
    /// reconciliation marker, not a generation: the layout is one folder and
    /// the counter lives in the `change-counter` file beside the files.
    counter: u64,
    /// One encode buffer for every file, reserved at start. Grown only when an
    /// entry does not fit, which is table growth rather than a per-entry
    /// allocation.
    buf: Vec<u8>,
    /// Size of each file in the folder, by key hash.
    sizes: HashMap<[u8; 32], u64>,
    /// Bytes of files in the folder.
    bytes_on_disk: u64,
    /// The bound, taken from the memory cache's own.
    max_bytes: u64,
    /// The byte bound has been logged for this counter, so a flood of junk
    /// keys does not log once per entry.
    full_logged: bool,
    /// Entries skipped because writing one would pass the byte bound.
    skipped_full: u64,
    /// A write failure has been logged since the last successful write. One
    /// line per failure episode, with the path and the error, not one per
    /// entry.
    write_error_logged: bool,
    /// Files written in the last tick.
    written_last_tick: usize,
    /// Encoded bytes written in the last tick.
    written_bytes_last_tick: u64,
    /// Loop time one tick may spend, [`TIME_BUDGET`] in a built binary. A test
    /// sets it through [`DiskWriter::set_time_budget`] to make a budget exact.
    time_budget: Duration,
    /// Ticks taken since the writer was created.
    ticks: u64,
    /// Loop time the last tick took, measured from the platform clock.
    tick_last_ns: u64,
}

impl DiskWriter {
    /// A writer for `dir`, bounded at `max_bytes` of files.
    ///
    /// The directory is created if it is missing, so a deployment's
    /// `CacheDirectory=` need not pre-create it. The start-time probe in config
    /// validation is what makes an unwritable path a startup error.
    pub fn new(dir: PathBuf, max_bytes: usize) -> std::io::Result<Self> {
        std::fs::create_dir_all(&dir)?;
        let counter = read_change_counter(&dir)?.unwrap_or(0);
        Ok(DiskWriter {
            dir,
            counter,
            buf: Vec::with_capacity(1024 * 1024),
            sizes: HashMap::new(),
            bytes_on_disk: 0,
            max_bytes: max_bytes as u64,
            full_logged: false,
            skipped_full: 0,
            write_error_logged: false,
            written_last_tick: 0,
            written_bytes_last_tick: 0,
            time_budget: TIME_BUDGET,
            ticks: 0,
            tick_last_ns: 0,
        })
    }

    /// Set the loop time one tick may spend.
    ///
    /// [`TIME_BUDGET`] is what a built binary uses. Only a test sets another
    /// value: zero makes the budget exact, so a slice must stop after one unit
    /// of work whatever the host's speed.
    pub fn set_time_budget(&mut self, budget: Duration) {
        self.time_budget = budget;
    }

    /// Follow `counter`: write the change-counter file, and when the counter
    /// differs from the one the folder holds, purge the folder so the next
    /// writes start a fresh disk copy.
    ///
    /// Called on the first answer and on a change. On a change the caller has
    /// already swapped the memory cache for an empty one, so no entry's
    /// queued flag survives and the scan starts from an empty map.
    pub fn use_id(&mut self, counter: u64) {
        if self.counter == counter {
            let _ = write_change_counter(&self.dir, counter);
            return;
        }
        if let Err(e) = purge_files(&self.dir) {
            m6_core::log::warn!(
                path = %self.dir.display(),
                error = %e,
                "disk cache: purge failed"
            );
        }
        self.sizes.clear();
        self.bytes_on_disk = 0;
        self.full_logged = false;
        self.counter = counter;
        if let Err(e) = write_change_counter(&self.dir, counter) {
            m6_core::log::warn!(
                path = %self.dir.display(),
                error = %e,
                "disk cache: change-counter write failed"
            );
        }
    }

    /// Count the files a start-time [`load`] found in the folder, so the byte
    /// bound covers a directory this process did not write and a rewrite of
    /// one of those keys replaces its file.
    ///
    /// Called once at start, right after [`DiskWriter::use_id`] and before the
    /// loop serves. A later counter change resets the map, and the loaded
    /// files go with it.
    pub fn adopt(&mut self, files: impl IntoIterator<Item = ([u8; 32], u64)>) {
        for (hash, len) in files {
            let old = self.sizes.insert(hash, len).unwrap_or(0);
            self.bytes_on_disk = self.bytes_on_disk.saturating_sub(old).saturating_add(len);
        }
    }

    /// Write and remove a bounded amount of disk work: one slice of the loop's
    /// idle turns.
    ///
    /// Removals run first, because they free bytes the write bound is checked
    /// against. Then the writer scans the cache for entries with the `queued`
    /// flag until it has written [`WRITE_BUDGET`] bytes, the queue is empty, or
    /// [`TIME_BUDGET`] of loop time is used. The time check runs between files,
    /// so a file is never split and the slice never starts one it cannot
    /// afford. An entry that would pass the byte bound is skipped and counted.
    /// An entry larger than a whole slice's budget is written on its own when
    /// nothing else was, because a file is never split.
    pub fn tick(&mut self, cache: &mut Cache) -> TickStats {
        let mut stats = TickStats::default();
        self.written_last_tick = 0;
        self.written_bytes_last_tick = 0;
        self.ticks += 1;
        let started = m6_core::platform::now();
        let dir = self.dir.clone();
        let budget = self.time_budget;
        let mut written_bytes = 0u64;
        // One entry is attempted before the budget is checked, the same
        // guarantee the removal sweep made: a host that deschedules the
        // process inside the budget would otherwise write nothing on every
        // tick and the queue would never drain. The attempt advances the scan
        // by one entry, whether it wrote, skipped a full cache, or failed.
        let mut attempted = false;
        // The scan visits entries in map order. An entry the budget cannot
        // take stops the scan and keeps its flag, so the next tick sees it.
        cache.scan_queued(|key, response| {
            if written_bytes >= WRITE_BUDGET {
                return false;
            }
            if attempted && over_budget(started, budget) {
                return false;
            }
            attempted = true;
            let key_str: &str = std::borrow::Borrow::borrow(key);
            encode_into(&mut self.buf, key_str, response);
            let len = self.buf.len() as u64;
            if written_bytes > 0 && written_bytes.saturating_add(len) > WRITE_BUDGET {
                return false;
            }
            let hash = key_hash(key_str);
            let old_size = self.sizes.get(&hash).copied().unwrap_or(0);
            let projected = self
                .bytes_on_disk
                .saturating_sub(old_size)
                .saturating_add(len);
            if projected > self.max_bytes {
                if !self.full_logged {
                    self.full_logged = true;
                    m6_core::log::info!(
                        dir = %dir.display(),
                        max_bytes = self.max_bytes,
                        "disk cache: byte bound reached; skipping writes until the counter changes"
                    );
                }
                self.skipped_full += 1;
                return true;
            }
            let path = dir.join(hex(&hash));
            if let Err(e) = self.write_file(&path) {
                self.log_write_error(&path, &e);
            } else {
                self.bytes_on_disk = projected;
                self.sizes.insert(hash, len);
                self.write_error_logged = false;
                written_bytes += len;
                stats.written += 1;
            }
            true
        });
        self.written_last_tick = stats.written;
        self.written_bytes_last_tick = written_bytes;
        self.tick_last_ns = elapsed_ns(started);
        stats
    }

    /// Total bytes of files in the current generation's directory.
    pub fn dir_bytes(&self) -> u64 {
        self.bytes_on_disk
    }

    /// Files written in the last tick.
    pub fn written_last_tick(&self) -> usize {
        self.written_last_tick
    }

    /// Encoded bytes written in the last tick, including a single entry larger
    /// than the budget.
    pub fn written_bytes_last_tick(&self) -> u64 {
        self.written_bytes_last_tick
    }

    /// Entries skipped because the byte bound would be passed.
    pub fn skipped_full(&self) -> u64 {
        self.skipped_full
    }

    /// Times the writer skipped a write for being full, cumulative. This is
    /// the `cache.disk_full` metric: a restarted cache with an incomplete disk
    /// copy misses, so a full copy is reported and faulted, never silent.
    pub fn disk_full(&self) -> u64 {
        self.skipped_full
    }

    /// Total bytes of files on disk, the `cache.disk_bytes` metric.
    pub fn disk_bytes(&self) -> u64 {
        self.dir_bytes()
    }

    /// Ticks taken since the writer was created, including ticks with no work.
    pub fn ticks(&self) -> u64 {
        self.ticks
    }

    /// Loop time the last tick took, in nanoseconds, from the platform clock.
    pub fn tick_last_ns(&self) -> u64 {
        self.tick_last_ns
    }

    /// Whether the writer has a slice to run: entries the cache has flagged.
    ///
    /// The loop polls without blocking while this is true, so one slice runs
    /// on each idle turn, and goes back to its normal wait when it is false.
    pub fn has_work(&self, cache: &Cache) -> bool {
        cache.queued() > 0
    }

    /// Write the encode buffer to `path` through `path.tmp` and a rename.
    fn write_file(&self, path: &Path) -> std::io::Result<()> {
        let mut tmp = path.to_path_buf().into_os_string();
        tmp.push(".tmp");
        let tmp = PathBuf::from(tmp);
        let result = (|| {
            let mut file = std::fs::File::create(&tmp)?;
            file.write_all(&self.buf)?;
            drop(file);
            std::fs::rename(&tmp, path)
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        result
    }

    /// One warn line per failure episode, with the path and the error.
    fn log_write_error(&mut self, path: &Path, e: &std::io::Error) {
        if self.write_error_logged {
            return;
        }
        self.write_error_logged = true;
        m6_core::log::warn!(
            path = %path.display(),
            error = %e,
            "disk cache: write failed; dropping the entry and not retrying"
        );
    }
}

/// What a start-time load read, held aside until the first registration answer.
pub struct Loaded {
    /// The change counter the folder was saved under, read from the
    /// `change-counter` file. It is a reconciliation marker, not a generation.
    pub id: u64,
    /// The decoded entries, bounded like the live cache.
    pub cache: Cache,
    /// Entries inserted.
    pub entries: usize,
    /// Files deleted because they could not be read.
    pub bad: usize,
    /// The key hash and file length of every file left in the folder,
    /// including the ones beyond the memory bound and the ones this process
    /// did not write. [`DiskWriter::adopt`] counts them, so the byte bound
    /// covers the directory a restart inherits.
    pub files: Vec<([u8; 32], u64)>,
}

/// What happened to the disk copy a node read at start, for the metrics
/// document.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LoadState {
    /// Nothing is installed: there was nothing to read, or the first heartbeat
    /// has not answered yet.
    #[default]
    None,
    /// The first heartbeat named the ID the files carried, so the load is the
    /// live cache.
    Kept,
    /// The first heartbeat named another ID, so the load was dropped.
    Discarded,
    /// The upstream did not answer, so the load serves until it does.
    Unverified,
}

impl LoadState {
    /// The spelling the metrics document uses.
    pub fn as_str(self) -> &'static str {
        match self {
            LoadState::None => "none",
            LoadState::Kept => "kept",
            LoadState::Discarded => "discarded",
            LoadState::Unverified => "unverified",
        }
    }
}

/// Read the files under `dir` into a fresh cache.
///
/// This is the start-time half of the disk cache: it runs once, before the
/// loop serves, and never on the request path. The layout is one folder, so
/// every file is read and the change counter is taken from the
/// `change-counter` file beside them.
///
/// A file is read and decoded one at a time. The entries that fit `max_bytes`
/// are inserted and the rest stay on disk under their keys, so a node whose
/// disk copy is larger than its memory bound still starts with a cache of the
/// first entries in file-name order, and the writer's bound covers the files
/// that were left. A file that is short, fails its check value or names a key
/// the store could not have written is deleted and counted in `bad`, with one
/// log line naming the path and the error. A `.tmp` file is an interrupted
/// write and is deleted without being counted. The `change-counter` file is
/// left alone.
///
/// `Ok(None)` means there are no cache files to load: an empty `dir`, or one
/// holding only the change counter. A directory that cannot be listed is an
/// error, because a cache node whose `[cache] dir` cannot be read is a
/// configuration fault the operator must see at start.
pub fn load(dir: &Path, max_bytes: usize) -> std::io::Result<Option<Loaded>> {
    let started = m6_core::platform::now();
    let id = read_change_counter(dir)?.unwrap_or(0);
    let cache = Cache::with_max_bytes(max_bytes);
    let mut entries = 0usize;
    let mut bad = 0usize;
    let mut read_bytes = 0usize;
    let mut files: Vec<([u8; 32], u64)> = Vec::new();
    // Name order, so the entries that win the bound and the ones left on disk
    // are the same on every start.
    let mut paths: Vec<PathBuf> = std::fs::read_dir(dir)?
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
        .filter(|e| e.file_name() != CHANGE_COUNTER)
        .map(|e| e.path())
        .collect();
    paths.sort();
    if paths.is_empty() {
        return Ok(None);
    }

    for path in paths {
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned());
        let is_tmp = name.as_deref().is_some_and(|n| n.ends_with(".tmp"));
        let Ok(len) = std::fs::metadata(&path).map(|m| m.len()) else {
            bad += 1;
            m6_core::log::warn!(
                path = %path.display(),
                error = "the file could not be sized",
                "disk cache: cannot read a file; deleting it"
            );
            let _ = std::fs::remove_file(&path);
            continue;
        };
        if is_tmp {
            // An interrupted write, not a value. The next store of the key
            // writes the file again.
            let _ = std::fs::remove_file(&path);
            continue;
        }
        let hash = name.as_deref().and_then(hash_from_name);
        if read_bytes.saturating_add(len as usize) > max_bytes {
            // The rest is left on disk and stays addressable by its key. It
            // still counts towards the writer's byte bound.
            if let Some(hash) = hash {
                files.push((hash, len));
            }
            continue;
        }
        let data = match std::fs::read(&path) {
            Ok(data) => data,
            Err(e) => {
                bad += 1;
                m6_core::log::warn!(
                    path = %path.display(),
                    error = %e,
                    "disk cache: cannot read a file; deleting it"
                );
                let _ = std::fs::remove_file(&path);
                continue;
            }
        };
        let (stored_key, response) = match decode(&data) {
            Ok(decoded) => decoded,
            Err(e) => {
                bad += 1;
                m6_core::log::warn!(
                    path = %path.display(),
                    error = %e,
                    "disk cache: skipping a file that does not decode"
                );
                let _ = std::fs::remove_file(&path);
                continue;
            }
        };
        let Some(key) = key_from_stored(&stored_key) else {
            bad += 1;
            m6_core::log::warn!(
                path = %path.display(),
                error = "the stored key is not a cache key",
                "disk cache: deleting a file that is not addressable"
            );
            let _ = std::fs::remove_file(&path);
            continue;
        };
        let key_hash = key_hash(&stored_key);
        cache.insert(key, response);
        entries += 1;
        read_bytes += data.len();
        files.push((key_hash, len));
    }

    let elapsed_ms = m6_core::platform::now().duration_since(started).as_millis();
    m6_core::log::info!(id, entries, bad, elapsed_ms, "disk cache loaded");
    Ok(Some(Loaded {
        id,
        cache,
        entries,
        bad,
        files,
    }))
}

/// Rebuild the cache key a file stores.
///
/// The stored form is `path\x01query\x01encoding`, which is what
/// [`CacheKey::new`] writes, so splitting on the separator and rebuilding it
/// gives the same key. A string that does not split into three parts cannot
/// have been written by the store, so the file is not addressable.
fn key_from_stored(stored: &str) -> Option<CacheKey> {
    let (path, rest) = stored.split_once('\u{1}')?;
    let (query, encoding) = rest.split_once('\u{1}')?;
    if encoding.contains('\u{1}') {
        return None;
    }
    Some(CacheKey::new(path, Some(query), encoding))
}

/// The key hash a file name spells, when it is the 64 hex characters the
/// writer writes.
fn hash_from_name(name: &str) -> Option<[u8; 32]> {
    if name.len() != 64 {
        return None;
    }
    let mut hash = [0u8; 32];
    let bytes = name.as_bytes();
    for (i, byte) in hash.iter_mut().enumerate() {
        let hi = hex_value(bytes[i * 2])?;
        let lo = hex_value(bytes[i * 2 + 1])?;
        *byte = (hi << 4) | lo;
    }
    Some(hash)
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

/// Whether `budget` of loop time has been spent since `started`.
///
/// The clock comes from the platform layer, the one place that reads one, so
/// the check allocates nothing and the loop never takes a clock dependency of
/// its own.
fn over_budget(started: Instant, budget: Duration) -> bool {
    m6_core::platform::now().duration_since(started) >= budget
}

/// The loop time since `started`, in nanoseconds, from the platform clock.
fn elapsed_ns(started: Instant) -> u64 {
    m6_core::platform::now().duration_since(started).as_nanos() as u64
}

/// Encode one entry into `buf`: the whole file, including its check value.
///
/// The buffer is cleared first. The metadata is serialised straight into it, so
/// no scratch `Vec` or `String` is built per entry.
pub fn encode_into(buf: &mut Vec<u8>, key: &str, r: &CachedResponse) {
    #[derive(serde::Serialize)]
    struct Meta<'a> {
        key: &'a str,
        status: u16,
        headers: &'a crate::cache::HeaderBlock,
    }

    buf.clear();
    buf.extend_from_slice(MAGIC);
    buf.push(VERSION);
    let len_at = buf.len();
    buf.extend_from_slice(&[0u8; 4]);
    let meta_start = buf.len();
    serde_json::to_writer(
        &mut *buf,
        &Meta {
            key,
            status: r.status,
            headers: &r.headers,
        },
    )
    .expect("serialising cache metadata into a Vec cannot fail");
    let meta_len = u32::try_from(buf.len() - meta_start).expect("metadata fits in a u32");
    buf[len_at..len_at + 4].copy_from_slice(&meta_len.to_le_bytes());
    buf.extend_from_slice(&(r.body.len() as u64).to_le_bytes());
    buf.extend_from_slice(&r.body);
    let check = Sha256::digest(&buf[..]);
    buf.extend_from_slice(check.as_slice());
}

/// Decode one file into the cache key and the response.
///
/// This is the function a restarted node reads its disk cache with.
pub fn decode(bytes: &[u8]) -> Result<(String, CachedResponse), DecodeError> {
    #[derive(serde::Deserialize)]
    struct Meta {
        key: String,
        status: u16,
        headers: Vec<(String, String)>,
    }

    let mut r = Reader { bytes, pos: 0 };
    if r.take(4)? != MAGIC {
        return Err(DecodeError::Magic);
    }
    let version = r.take(1)?[0];
    if version != VERSION {
        return Err(DecodeError::Version(version));
    }
    let meta_len = u32::from_le_bytes(r.take(4)?.try_into().expect("4 bytes")) as usize;
    let meta: Meta =
        serde_json::from_slice(r.take(meta_len)?).map_err(|e| DecodeError::Meta(e.to_string()))?;
    let body_len = usize::try_from(u64::from_le_bytes(r.take(8)?.try_into().expect("8 bytes")))
        .map_err(|_| DecodeError::Short)?;
    let body = r.take(body_len)?.to_vec();
    let remaining = r.bytes.len() - r.pos;
    if remaining != 32 {
        return Err(if remaining < 32 {
            DecodeError::Short
        } else {
            DecodeError::Check
        });
    }
    let check = r.take(32)?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes[..bytes.len() - 32]);
    if hasher.finalize().as_slice() != check {
        return Err(DecodeError::Check);
    }
    Ok((
        meta.key,
        CachedResponse {
            genid: 0,
            status: meta.status,
            headers: crate::cache::HeaderBlock::from_pairs(meta.headers),
            body: bytes::Bytes::from(body),
        },
    ))
}

/// The SHA-256 of a cache key's bytes, which is the file's name.
fn key_hash(key: &str) -> [u8; 32] {
    Sha256::digest(key.as_bytes()).into()
}

/// The lower-case hex of a hash, the file's name.
fn hex(hash: &[u8; 32]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(64);
    for byte in hash {
        write!(out, "{byte:02x}").expect("writing to a String cannot fail");
    }
    out
}

/// A bounds-checked cursor over a file's bytes.
struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], DecodeError> {
        let end = self.pos.checked_add(n).ok_or(DecodeError::Short)?;
        if end > self.bytes.len() {
            return Err(DecodeError::Short);
        }
        let out = &self.bytes[self.pos..end];
        self.pos = end;
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::cache::{CacheKey, HeaderBlock};

    use bytes::Bytes;

    /// A response with one header and a body of `body_len` zero bytes.
    fn response(body: &[u8]) -> CachedResponse {
        CachedResponse {
            genid: 0,
            status: 200,
            headers: HeaderBlock::from_pairs(vec![(
                "content-type".to_string(),
                "text/html; charset=utf-8".to_string(),
            )]),
            body: Bytes::copy_from_slice(body),
        }
    }

    fn writer(dir: &Path, max_bytes: usize) -> DiskWriter {
        let mut w = DiskWriter::new(dir.to_path_buf(), max_bytes).expect("writer");
        w.use_id(7);
        w
    }

    /// The store path on a node with a disk writer: the entry is inserted with
    /// its queued flag set, in the same map write.
    fn stored_queued(cache: &mut Cache, path: &str, body: &[u8]) -> CacheKey {
        let key = CacheKey::new(path, None, "");
        cache.insert_queued(key.clone(), response(body));
        key
    }

    fn files(dir: &Path) -> Vec<PathBuf> {
        let mut out: Vec<PathBuf> = std::fs::read_dir(dir)
            .expect("read_dir")
            .flatten()
            .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
            .map(|e| e.path())
            .collect();
        out.sort();
        out
    }

    /// Every field survives the round trip: the key, the status, every header
    /// and the body.
    ///
    /// Break: write the body length in big-endian while the reader takes
    /// little-endian, and the decoded body is empty or the check fails.
    #[test]
    fn encode_then_decode_round_trips() {
        let body = b"hello, disk";
        let r = CachedResponse {
            genid: 0,
            status: 203,
            headers: HeaderBlock::from_pairs(vec![
                ("content-type".to_string(), "text/plain".to_string()),
                (
                    "cache-control".to_string(),
                    "public, max-age=60".to_string(),
                ),
                (
                    "date".to_string(),
                    "Sun, 15 Nov 2026 00:00:00 GMT".to_string(),
                ),
            ]),
            body: Bytes::from_static(body),
        };
        let mut buf = Vec::new();
        encode_into(&mut buf, "/hello\u{1}a=1\u{1}gzip", &r);
        let (key, decoded) = decode(&buf).expect("decode");
        assert_eq!(key, "/hello\u{1}a=1\u{1}gzip");
        assert_eq!(decoded.status, 203);
        assert_eq!(decoded.headers, r.headers);
        assert_eq!(&decoded.body[..], body);
    }

    /// One flipped byte anywhere before the check value fails the check, so a
    /// torn or corrupted file is never served.
    ///
    /// Break: compare the check against the bytes after it, or skip the check,
    /// and this passes with corrupt content.
    #[test]
    fn a_flipped_byte_fails_the_check() {
        let mut buf = Vec::new();
        encode_into(&mut buf, "/", &response(b"body"));
        let body_at = buf.len() - 32 - 4;
        buf[body_at] ^= 0x01;
        assert!(matches!(decode(&buf), Err(DecodeError::Check)));
    }

    /// A truncated file is short, not a panic and not a partial response.
    ///
    /// Break: index the bytes without a bounds check, and this panics instead
    /// of returning `Short`.
    #[test]
    fn a_short_file_fails() {
        let mut buf = Vec::new();
        encode_into(&mut buf, "/", &response(b"body"));
        for cut in [0, 3, 5, 10, buf.len() - 1] {
            assert!(
                matches!(decode(&buf[..cut]), Err(DecodeError::Short)),
                "a file cut at {cut} must be short"
            );
        }
    }

    /// Two ticks spend the byte budget and stop, and the rest goes on the next
    /// tick. Ten entries of just under a MiB: 4, then 4, then 2.
    ///
    /// Break: write every queued key in one tick, and the first tick writes 10.
    #[test]
    fn the_writer_spends_its_budget_and_stops() {
        // A MiB less four KiB, so four files plus their headers and the check
        // value still fit a four MiB tick.
        let body = vec![0u8; 1024 * 1024 - 4096];
        let dir = tempfile::tempdir().unwrap();
        let mut cache = Cache::new();
        let mut w = writer(dir.path(), 128 * 1024 * 1024);
        // Ten MiB of writes take more than a millisecond, so the byte budget
        // is measured on its own here; the time budget has its own test.
        w.time_budget = Duration::from_secs(60);
        for i in 0..10 {
            stored_queued(&mut cache, &format!("/p{i}"), &body);
        }
        assert_eq!(cache.queued(), 10);

        let first = w.tick(&mut cache);
        assert_eq!(first.written, 4, "the first tick writes four");
        assert_eq!(cache.queued(), 6);
        let second = w.tick(&mut cache);
        assert_eq!(second.written, 4, "the second tick writes four");
        assert_eq!(cache.queued(), 2);
        let third = w.tick(&mut cache);
        assert_eq!(third.written, 2, "the third tick writes the rest");
        assert_eq!(cache.queued(), 0);
        assert_eq!(files(dir.path()).len(), 11, "ten files plus the counter");
    }

    /// The writer has work while the cache holds a flagged entry, and not a
    /// slice after the queue is empty. That is the condition the loop polls a
    /// zero timeout on: true keeps one slice per idle turn, false returns the
    /// normal wait.
    ///
    /// Break: return false always, and the loop waits a second between
    /// slices; return true always, and it spins at a zero timeout.
    #[test]
    fn the_writer_has_work_until_the_queue_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let mut cache = Cache::new();
        let mut w = DiskWriter::new(dir.path().to_path_buf(), 128 * 1024 * 1024).unwrap();
        assert!(!w.has_work(&cache), "a new writer has nothing to do");
        w.use_id(7);
        stored_queued(&mut cache, "/w", b"body");
        assert!(
            w.has_work(&cache),
            "a queued entry is work for an idle turn"
        );
        while w.has_work(&cache) {
            w.tick(&mut cache);
        }
        assert_eq!(cache.queued(), 0);
        assert!(
            !w.has_work(&cache),
            "the loop returns to its normal wait once the queue is empty"
        );
    }

    /// The tick stops when its loop-time budget is used, between files, and
    /// the rest keep their flags for the next tick. Eight entries of a MiB
    /// each: one per tick, since a file's encode and write take more than the
    /// budget and a file is never split.
    ///
    /// Break: write every queued key in one tick, and the first tick writes 8.
    #[test]
    fn the_tick_stops_when_its_time_budget_is_used() {
        let body = vec![0u8; 1024 * 1024];
        let dir = tempfile::tempdir().unwrap();
        let mut cache = Cache::new();
        let mut w = writer(dir.path(), 128 * 1024 * 1024);
        for i in 0..8 {
            stored_queued(&mut cache, &format!("/t{i}"), &body);
        }
        for i in 0..8 {
            assert_eq!(
                w.tick(&mut cache).written,
                1,
                "tick {i} writes one file and stops at the budget"
            );
            assert_eq!(cache.queued(), 8 - i - 1, "the rest stay flagged");
        }
        assert_eq!(w.tick(&mut cache).written, 0, "the queue is empty");
        assert_eq!(files(dir.path()).len(), 9, "eight files plus the counter");
    }

    /// The tick writes the entry as it is now, not as it was when queued.
    ///
    /// Break: capture the response at queue time and write that, and the file
    /// still holds `old`.
    #[test]
    fn a_replaced_entry_is_written_as_it_is_now() {
        let dir = tempfile::tempdir().unwrap();
        let mut cache = Cache::new();
        let mut w = writer(dir.path(), 128 * 1024 * 1024);
        let key = stored_queued(&mut cache, "/changing", b"old");
        // Re-store before the tick: the replacement keeps the queued flag, so
        // the key is still queued exactly once.
        cache.insert(key.clone(), response(b"new"));
        assert_eq!(cache.queued(), 1);

        assert_eq!(w.tick(&mut cache).written, 1);
        let file = files(dir.path())
            .into_iter()
            .find(|p| {
                p.file_name()
                    .is_some_and(|n| n.to_string_lossy() != CHANGE_COUNTER)
            })
            .expect("one cache file");
        let (_, decoded) = decode(&std::fs::read(file).unwrap()).expect("decode");
        assert_eq!(&decoded.body[..], b"new");
    }

    /// An entry evicted since it was queued is skipped without an error and
    /// without a file.
    ///
    /// Break: write the response captured at queue time, and a file appears
    /// for an entry the cache no longer holds.
    #[test]
    fn an_evicted_entry_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let mut cache = Cache::new();
        let mut w = writer(dir.path(), 128 * 1024 * 1024);
        stored_queued(&mut cache, "/gone", b"gone");
        stored_queued(&mut cache, "/kept", b"kept");
        cache.evict_path("/gone");
        assert_eq!(cache.queued(), 1, "the eviction takes its flag with it");

        let stats = w.tick(&mut cache);
        assert_eq!(stats.written, 1);
        let names: Vec<PathBuf> = files(dir.path())
            .into_iter()
            .filter(|p| {
                p.file_name()
                    .is_some_and(|n| n.to_string_lossy() != CHANGE_COUNTER)
            })
            .collect();
        assert_eq!(names.len(), 1);
        let (key, _) = decode(&std::fs::read(&names[0]).unwrap()).expect("decode");
        assert_eq!(key, "/kept\u{1}\u{1}");
    }

    /// The byte bound stops writes: a three MiB bound holds three just-under-a-
    /// MiB entries and skips the other two, each counted.
    ///
    /// Break: never check the bound, and all five files appear.
    #[test]
    fn the_bound_stops_writes() {
        let body = vec![0u8; 1024 * 1024 - 4096];
        let dir = tempfile::tempdir().unwrap();
        let mut cache = Cache::new();
        let mut w = writer(dir.path(), 3 * 1024 * 1024);
        w.time_budget = Duration::from_secs(60);
        for i in 0..5 {
            stored_queued(&mut cache, &format!("/b{i}"), &body);
        }
        let stats = w.tick(&mut cache);
        assert_eq!(stats.written, 3, "three files fit the bound");
        assert_eq!(w.skipped_full(), 2, "the other two are skipped and counted");
        assert_eq!(
            files(dir.path())
                .into_iter()
                .filter(|p| p
                    .file_name()
                    .is_some_and(|n| n.to_string_lossy() != CHANGE_COUNTER))
                .count(),
            3
        );
    }

    /// A counter change purges the folder and writes the new counter, so a
    /// cache that missed purges while away never serves its old disk copy.
    ///
    /// Break: keep the old files, and the restart serves them.
    #[test]
    fn a_counter_change_purges_the_folder() {
        let dir = tempfile::tempdir().unwrap();
        let mut cache = Cache::new();
        let mut w = writer(dir.path(), 128 * 1024 * 1024);
        w.use_id(7);
        stored_queued(&mut cache, "/kept", b"kept");
        assert_eq!(w.tick(&mut cache).written, 1);
        assert_eq!(
            files(dir.path())
                .into_iter()
                .filter(|p| p
                    .file_name()
                    .is_some_and(|n| n.to_string_lossy() != CHANGE_COUNTER))
                .count(),
            1
        );

        w.use_id(9);
        assert_eq!(
            files(dir.path())
                .into_iter()
                .filter(|p| p
                    .file_name()
                    .is_some_and(|n| n.to_string_lossy() != CHANGE_COUNTER))
                .count(),
            0,
            "a counter change purges the folder"
        );
        assert_eq!(read_change_counter(dir.path()).unwrap(), Some(9));
    }

    /// A written counter round-trips, and the write is atomic: a temp file is
    /// never left behind and the value is a decimal u64 plus a newline.
    #[test]
    fn a_written_counter_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        write_change_counter(dir.path(), 42).unwrap();
        assert_eq!(read_change_counter(dir.path()).unwrap(), Some(42));
        let text = std::fs::read_to_string(dir.path().join(CHANGE_COUNTER)).unwrap();
        assert_eq!(text, "42\n");
    }

    /// A missing counter reads as none, and garbage reads as missing, so a
    /// torn or hand-edited counter reconciles by purging rather than by
    /// trusting a value that was never saved.
    #[test]
    fn a_missing_or_garbage_counter_reads_as_missing() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read_change_counter(dir.path()).unwrap(), None);
        std::fs::write(dir.path().join(CHANGE_COUNTER), b"not-a-counter\n").unwrap();
        assert_eq!(read_change_counter(dir.path()).unwrap(), None);
    }

    // ── The start-time load ──────────────────────────────────────────────────

    /// Write one encoded entry under `root`.
    fn write_entry(root: &Path, key: &str, body: &[u8]) {
        let mut buf = Vec::new();
        encode_into(&mut buf, key, &response(body));
        std::fs::write(root.join(hex(&key_hash(key))), buf).unwrap();
    }

    /// One good file survives. A flipped byte and a truncated file are deleted
    /// and counted, and a `.tmp` from an interrupted write is deleted without
    /// being counted. The change-counter file is skipped.
    ///
    /// Break: skip a file that does not decode without counting or deleting
    /// it, and `bad` is 0 and the corrupt file stays on disk.
    #[test]
    fn load_deletes_bad_files_and_counts_them() {
        let dir = tempfile::tempdir().unwrap();
        write_entry(dir.path(), "/good\u{1}\u{1}", b"good");

        let mut buf = Vec::new();
        encode_into(&mut buf, "/flip\u{1}\u{1}", &response(b"flipped"));
        let at = buf.len() - 32 - 1;
        buf[at] ^= 0x01;
        std::fs::write(dir.path().join(hex(&key_hash("/flip\u{1}\u{1}"))), &buf).unwrap();

        let mut buf = Vec::new();
        encode_into(&mut buf, "/short\u{1}\u{1}", &response(b"short"));
        let cut = buf.len() / 2;
        std::fs::write(
            dir.path().join(hex(&key_hash("/short\u{1}\u{1}"))),
            &buf[..cut],
        )
        .unwrap();

        std::fs::write(dir.path().join("junk.tmp"), b"partial").unwrap();
        write_change_counter(dir.path(), 7).unwrap();

        let loaded = load(dir.path(), 1024 * 1024).unwrap().expect("a folder");
        assert_eq!(loaded.entries, 1, "only the good file loads");
        assert_eq!(
            loaded.bad, 2,
            "the flipped byte and the short file are counted"
        );
        assert_eq!(loaded.id, 7, "the change counter is the load's id");
        assert_eq!(
            files(dir.path())
                .into_iter()
                .filter(|p| p
                    .file_name()
                    .is_some_and(|n| n.to_string_lossy() != CHANGE_COUNTER))
                .count(),
            1,
            "every bad file and the tmp are gone"
        );
        assert!(loaded
            .cache
            .get(&CacheKey::new("/good", None, ""))
            .is_some());
        for key in ["/flip", "/short"] {
            assert!(
                loaded.cache.get(&CacheKey::new(key, None, "")).is_none(),
                "{key} was not served"
            );
        }
    }

    /// More bytes on disk than `max_bytes`: the entries that fit are inserted
    /// and the rest stay on disk under their keys. The cache stays inside its
    /// bound and nothing is deleted for not fitting.
    ///
    /// Break: insert every entry regardless of the bound, and `entries` is 40.
    #[test]
    fn load_respects_the_bound() {
        let dir = tempfile::tempdir().unwrap();
        let body = vec![b'x'; 4096];
        let total = 40usize;
        for i in 0..total {
            write_entry(dir.path(), &format!("/p{i}\u{1}\u{1}"), &body);
        }
        let bound = 64 * 1024;
        let loaded = load(dir.path(), bound).unwrap().expect("a folder");
        assert!(
            loaded.cache.bytes_held() <= bound,
            "the cache holds {} bytes against a {bound} bound",
            loaded.cache.bytes_held()
        );
        assert!(
            loaded.entries < total,
            "the load took every file: {} entries",
            loaded.entries
        );
        assert!(loaded.entries > 0, "some entries fit");
        assert_eq!(loaded.bad, 0, "an entry that does not fit is not corrupt");
        assert_eq!(
            loaded.files.len(),
            total,
            "every file counts towards the bound"
        );
    }

    /// An empty root means nothing to load, so the node starts exactly as it
    /// did before the disk cache.
    ///
    /// Break: return a load with ID 0, and the node believes it read a
    /// counter from an empty directory.
    #[test]
    fn load_of_nothing_is_none() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load(dir.path(), 1024).unwrap().is_none());
    }
}
