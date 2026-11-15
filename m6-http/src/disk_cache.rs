//! A cache node's disk copy of its memory cache, written from the loop's tick.
//!
//! The request path never touches a file. A stored entry pushes its key on the
//! writer's ring, once, and the accounting tick does all the disk work: up to
//! [`WRITE_BUDGET`] bytes of current entries, and up to [`REMOVE_BUDGET`] files
//! of an old generation's directory. A tick that runs out of budget with more
//! to write leaves the remaining keys on the ring, still marked queued, so the
//! next tick continues where it stopped.
//!
//! The layout is `<dir>/<genid decimal>/<sha256 of the key>`, one subdirectory
//! per generation, so an ID change needs no per-file rewrite. A file is written
//! to `<name>.tmp` with one `write_all`, then renamed. No `fsync`: a crash can
//! leave a short or torn file, and the check value catches it at load.
//!
//! The format, all integers little-endian:
//!
//! ```text
//! b"M6DC" , u8 version = 1
//! u64 genid
//! u32 meta_len, meta: JSON {"key": "...", "status": ..., "headers": [[...]]}
//! u64 body_len, body
//! [u8; 32] SHA-256 of every byte before it
//! ```

use std::collections::{HashMap, VecDeque};
use std::io::Write;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::cache::{Cache, CacheKey, CachedResponse};

/// Keys the writer may hold at once. The ring is reserved at start and never
/// grows: a push to a full ring is refused and counted, and a later store of
/// the same key tries again.
pub const QUEUE_SLOTS: usize = 4096;

/// Bytes of files one tick may write. A single entry larger than the budget is
/// written on its own, because a file is never split.
pub const WRITE_BUDGET: u64 = 4 * 1024 * 1024;

/// Files one tick may remove from old generation directories.
pub const REMOVE_BUDGET: usize = 512;

const MAGIC: &[u8; 4] = b"M6DC";
const VERSION: u8 = 1;

/// What one tick of the writer did, for `/perf` and the tests.
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
    /// The current ID and its subdirectory. `None` means the node has no ID
    /// yet, and nothing is written.
    current: Option<(u64, PathBuf)>,
    /// Keys to write, each queued once (the entry's `queued` flag is the
    /// dedup). Reserved to [`QUEUE_SLOTS`] at start.
    ring: VecDeque<CacheKey>,
    /// One encode buffer for every file, reserved at start. Grown only when an
    /// entry does not fit, which is table growth rather than a per-entry
    /// allocation.
    buf: Vec<u8>,
    /// Size of each file in the current subdirectory, by key hash.
    sizes: HashMap<[u8; 32], u64>,
    /// Bytes of files in the current subdirectory.
    bytes_on_disk: u64,
    /// The bound, taken from the memory cache's own.
    max_bytes: u64,
    /// Old generation directories waiting to be removed, a bounded amount per
    /// tick.
    removals: Vec<PathBuf>,
    /// The byte bound has been logged for this ID, so a flood of junk keys
    /// does not log once per entry.
    full_logged: bool,
    /// Entries skipped because writing one would pass the byte bound.
    skipped_full: u64,
    /// Pushes refused because the ring was full.
    skipped_queue_full: u64,
    /// A write failure has been logged since the last successful write. One
    /// line per failure episode, with the path and the error, not one per
    /// entry.
    write_error_logged: bool,
    /// Files written in the last tick.
    written_last_tick: usize,
}

impl DiskWriter {
    /// A writer for `dir`, bounded at `max_bytes` of files.
    ///
    /// The directory is created if it is missing, so a deployment's
    /// `CacheDirectory=` need not pre-create it. The start-time probe in config
    /// validation is what makes an unwritable path a startup error.
    pub fn new(dir: PathBuf, max_bytes: usize) -> std::io::Result<Self> {
        std::fs::create_dir_all(&dir)?;
        Ok(DiskWriter {
            dir,
            current: None,
            ring: VecDeque::with_capacity(QUEUE_SLOTS),
            buf: Vec::with_capacity(1024 * 1024),
            sizes: HashMap::new(),
            bytes_on_disk: 0,
            max_bytes: max_bytes as u64,
            removals: Vec::new(),
            full_logged: false,
            skipped_full: 0,
            skipped_queue_full: 0,
            write_error_logged: false,
            written_last_tick: 0,
        })
    }

    /// Follow `id`: a new subdirectory from the next write, and every other
    /// subdirectory under the root queued for removal over later ticks.
    ///
    /// Called on the first ID and on a change. On a change the caller has
    /// already swapped the memory cache for an empty one, so no entry's
    /// queued flag survives on a key this writer still holds.
    pub fn use_id(&mut self, id: u64) {
        if self.current.as_ref().is_some_and(|(cur, _)| *cur == id) {
            return;
        }
        self.ring.clear();
        self.sizes.clear();
        self.bytes_on_disk = 0;
        self.full_logged = false;
        let subdir = self.dir.join(id.to_string());
        if let Err(e) = std::fs::create_dir_all(&subdir) {
            self.log_write_error(&subdir, &e);
        }
        self.current = Some((id, subdir));
        // Every other generation's directory goes, so a change needs no
        // per-file rewrite and the old content does not stay on disk.
        let keep = id.to_string();
        if let Ok(entries) = std::fs::read_dir(&self.dir) {
            for entry in entries.flatten() {
                if entry.file_name() == keep.as_str() {
                    continue;
                }
                if entry.file_type().is_ok_and(|t| t.is_dir()) {
                    self.removals.push(entry.path());
                }
            }
        }
    }

    /// Queue `key` to be written, once, and allocate nothing.
    ///
    /// Called on the request path right after the memory cache stored the
    /// entry. The key is moved in, so the caller's one clone is the ring's
    /// copy. A duplicate is refused by the entry's `queued` flag. A full ring
    /// refuses the push and counts it, and a later store of the same key tries
    /// again.
    pub fn queue(&mut self, key: CacheKey, cache: &mut Cache) {
        if self.current.is_none() {
            return;
        }
        if self.ring.len() >= QUEUE_SLOTS {
            self.skipped_queue_full += 1;
            return;
        }
        if cache.set_queued(&key, true) {
            return;
        }
        self.ring.push_back(key);
    }

    /// Write and remove a bounded amount of disk work.
    ///
    /// Removals run first, because they free bytes the write bound is checked
    /// against. Then the writer takes keys from the ring until it has written
    /// [`WRITE_BUDGET`] bytes or the ring is empty. An entry that would pass
    /// the bound is skipped and counted. An entry larger than a whole tick's
    /// budget is written on its own when nothing else was, because a file is
    /// never split.
    pub fn tick(&mut self, cache: &mut Cache) -> TickStats {
        let mut stats = TickStats::default();
        self.written_last_tick = 0;
        let Some((id, dir)) = self.current.clone() else {
            return stats;
        };
        stats.removed = self.remove_old();
        let mut written_bytes = 0u64;
        while written_bytes < WRITE_BUDGET {
            let Some(key) = self.ring.pop_front() else {
                break;
            };
            let Some(response) = cache.get_any(&key) else {
                // Evicted since it was queued. Nothing to write, and the flag
                // went with the entry.
                continue;
            };
            let key_str: &str = std::borrow::Borrow::borrow(&key);
            encode_into(&mut self.buf, id, key_str, &response);
            let len = self.buf.len() as u64;
            if written_bytes > 0 && written_bytes.saturating_add(len) > WRITE_BUDGET {
                // Out of budget with more to write: the key stays at the front
                // of the ring, still queued, and the next tick writes it.
                self.ring.push_front(key);
                break;
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
                        "disk cache: byte bound reached; skipping writes until the ID changes"
                    );
                }
                self.skipped_full += 1;
                let _ = cache.set_queued(&key, false);
                continue;
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
            let _ = cache.set_queued(&key, false);
        }
        self.written_last_tick = stats.written;
        stats
    }

    /// Total bytes of files in the current generation's directory.
    pub fn dir_bytes(&self) -> u64 {
        self.bytes_on_disk
    }

    /// Keys waiting to be written.
    pub fn queued(&self) -> usize {
        self.ring.len()
    }

    /// Files written in the last tick.
    pub fn written_last_tick(&self) -> usize {
        self.written_last_tick
    }

    /// Entries skipped because the byte bound would be passed.
    pub fn skipped_full(&self) -> u64 {
        self.skipped_full
    }

    /// Pushes refused because the ring was full.
    pub fn skipped_queue_full(&self) -> u64 {
        self.skipped_queue_full
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

    /// Remove up to [`REMOVE_BUDGET`] files from the oldest queued directory.
    ///
    /// A directory is removed only on a visit that found it empty, so a
    /// thousand files take two ticks to empty and a third to remove the
    /// directory itself.
    fn remove_old(&mut self) -> usize {
        let mut removed = 0usize;
        while removed < REMOVE_BUDGET {
            let Some(dir) = self.removals.first().cloned() else {
                break;
            };
            let mut visited = 0usize;
            match std::fs::read_dir(&dir) {
                Ok(entries) => {
                    for entry in entries {
                        if removed >= REMOVE_BUDGET {
                            break;
                        }
                        let Ok(entry) = entry else { continue };
                        let path = entry.path();
                        match std::fs::remove_file(&path) {
                            Ok(()) => {
                                removed += 1;
                                visited += 1;
                            }
                            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                                visited += 1;
                            }
                            Err(_) => break,
                        }
                    }
                    if visited == 0 {
                        match std::fs::remove_dir(&dir) {
                            Ok(()) => {
                                self.removals.remove(0);
                            }
                            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                                self.removals.remove(0);
                            }
                            Err(_) => break,
                        }
                    } else {
                        // This visit removed files, so the directory is left
                        // for a later tick even when budget remains: an empty
                        // directory goes on the tick that found it empty.
                        break;
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    self.removals.remove(0);
                }
                Err(_) => break,
            }
        }
        removed
    }
}

/// Encode one entry into `buf`: the whole file, including its check value.
///
/// The buffer is cleared first. The metadata is serialised straight into it, so
/// no scratch `Vec` or `String` is built per entry.
pub fn encode_into(buf: &mut Vec<u8>, id: u64, key: &str, r: &CachedResponse) {
    #[derive(serde::Serialize)]
    struct Meta<'a> {
        key: &'a str,
        status: u16,
        headers: &'a [(String, String)],
    }

    buf.clear();
    buf.extend_from_slice(MAGIC);
    buf.push(VERSION);
    buf.extend_from_slice(&id.to_le_bytes());
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

/// Decode one file into the generation ID, the cache key and the response.
///
/// This is the function a restarted node reads its disk cache with.
pub fn decode(bytes: &[u8]) -> Result<(u64, String, CachedResponse), DecodeError> {
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
    let id = u64::from_le_bytes(r.take(8)?.try_into().expect("8 bytes"));
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
        id,
        meta.key,
        CachedResponse {
            genid: id,
            status: meta.status,
            headers: std::sync::Arc::new(meta.headers),
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
    use std::sync::Arc;

    use bytes::Bytes;

    /// A response with one header and a body of `body_len` zero bytes.
    fn response(genid: u64, body: &[u8]) -> CachedResponse {
        CachedResponse {
            genid,
            status: 200,
            headers: Arc::new(vec![(
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

    fn stored(cache: &mut Cache, path: &str, body: &[u8]) -> CacheKey {
        let key = CacheKey::new(path, None, "");
        cache.insert(key.clone(), response(7, body));
        key
    }

    fn files(dir: &Path) -> Vec<PathBuf> {
        let mut out: Vec<PathBuf> = std::fs::read_dir(dir)
            .expect("read_dir")
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_file())
            .collect();
        out.sort();
        out
    }

    /// Every field survives the round trip: the ID, the key, the status, every
    /// header and the body.
    ///
    /// Break: write the body length in big-endian while the reader takes
    /// little-endian, and the decoded body is empty or the check fails.
    #[test]
    fn encode_then_decode_round_trips() {
        let body = b"hello, disk";
        let r = CachedResponse {
            genid: 7,
            status: 203,
            headers: Arc::new(vec![
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
        encode_into(&mut buf, 42, "/hello\u{1}a=1\u{1}gzip", &r);
        let (id, key, decoded) = decode(&buf).expect("decode");
        assert_eq!(id, 42);
        assert_eq!(key, "/hello\u{1}a=1\u{1}gzip");
        assert_eq!(decoded.genid, 42);
        assert_eq!(decoded.status, 203);
        assert_eq!(*decoded.headers, *r.headers);
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
        encode_into(&mut buf, 7, "/", &response(7, b"body"));
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
        encode_into(&mut buf, 7, "/", &response(7, b"body"));
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
        for i in 0..10 {
            let key = stored(&mut cache, &format!("/p{i}"), &body);
            w.queue(key, &mut cache);
        }
        assert_eq!(w.queued(), 10);

        let first = w.tick(&mut cache);
        assert_eq!(first.written, 4, "the first tick writes four");
        assert_eq!(w.queued(), 6);
        let second = w.tick(&mut cache);
        assert_eq!(second.written, 4, "the second tick writes four");
        assert_eq!(w.queued(), 2);
        let third = w.tick(&mut cache);
        assert_eq!(third.written, 2, "the third tick writes the rest");
        assert_eq!(w.queued(), 0);
        assert_eq!(files(&dir.path().join("7")).len(), 10);
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
        let key = stored(&mut cache, "/changing", b"old");
        w.queue(key.clone(), &mut cache);
        // Re-store before the tick: the replacement keeps the queued flag, so
        // the key is still queued exactly once.
        cache.insert(key.clone(), response(7, b"new"));
        w.queue(key.clone(), &mut cache);
        assert_eq!(w.queued(), 1);

        assert_eq!(w.tick(&mut cache).written, 1);
        let file = files(&dir.path().join("7")).pop().expect("one file");
        let (_, _, decoded) = decode(&std::fs::read(file).unwrap()).expect("decode");
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
        let gone = stored(&mut cache, "/gone", b"gone");
        let kept = stored(&mut cache, "/kept", b"kept");
        w.queue(gone, &mut cache);
        w.queue(kept, &mut cache);
        cache.evict_path("/gone");

        let stats = w.tick(&mut cache);
        assert_eq!(stats.written, 1);
        let names = files(&dir.path().join("7"));
        assert_eq!(names.len(), 1);
        let (_, key, _) = decode(&std::fs::read(&names[0]).unwrap()).expect("decode");
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
        for i in 0..5 {
            let key = stored(&mut cache, &format!("/b{i}"), &body);
            w.queue(key, &mut cache);
        }
        let stats = w.tick(&mut cache);
        assert_eq!(stats.written, 3, "three files fit the bound");
        assert_eq!(w.skipped_full(), 2, "the other two are skipped and counted");
        assert_eq!(files(&dir.path().join("7")).len(), 3);
    }

    /// A new ID moves writes to its own subdirectory and queues the old one
    /// for removal: 1,000 files empty over two ticks, the directory goes on the
    /// third.
    ///
    /// Break: never queue the old directory, and it still holds 1,000 files.
    #[test]
    fn a_new_id_removes_the_old_directory_over_ticks() {
        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join("6");
        std::fs::create_dir_all(&old).unwrap();
        for i in 0..1000 {
            std::fs::write(old.join(format!("{i:064x}")), b"x").unwrap();
        }
        let mut cache = Cache::new();
        let mut w = writer(dir.path(), 128 * 1024 * 1024);

        assert_eq!(w.tick(&mut cache).removed, 512, "first tick removes 512");
        assert!(old.is_dir(), "and leaves the directory");
        assert_eq!(w.tick(&mut cache).removed, 488, "second tick empties it");
        assert!(old.is_dir(), "the empty directory waits for its own tick");
        assert_eq!(w.tick(&mut cache).removed, 0);
        assert!(!old.exists(), "third tick removes the directory");
    }

    /// Two stores of one key before a tick write one file.
    ///
    /// Break: dedup with a second ring entry instead of the entry's queued
    /// flag, and the tick writes the same file twice.
    #[test]
    fn a_key_is_queued_once() {
        let dir = tempfile::tempdir().unwrap();
        let mut cache = Cache::new();
        let mut w = writer(dir.path(), 128 * 1024 * 1024);
        let key = stored(&mut cache, "/once", b"first");
        w.queue(key.clone(), &mut cache);
        cache.insert(key.clone(), response(7, b"second"));
        w.queue(key, &mut cache);
        assert_eq!(w.queued(), 1);
        assert_eq!(w.tick(&mut cache).written, 1);
        assert_eq!(files(&dir.path().join("7")).len(), 1);
    }

    /// What the writer leaves behind reads back after it is gone: a fresh
    /// writer on the same directory, which is what a restart starts with,
    /// decodes every file. The file is complete without the writer.
    ///
    /// Break: drop the check value from the encode, and the file no longer
    /// decodes on its own.
    #[test]
    fn a_restart_reads_what_the_writer_wrote() {
        let dir = tempfile::tempdir().unwrap();
        let mut cache = Cache::new();
        let mut w = writer(dir.path(), 128 * 1024 * 1024);
        let key = stored(&mut cache, "/restart", b"warm");
        w.queue(key, &mut cache);
        assert_eq!(w.tick(&mut cache).written, 1);
        drop(w);

        let subdir = dir.path().join("7");
        let files = files(&subdir);
        assert_eq!(files.len(), 1);
        // No writer is alive: the file itself carries everything a restart
        // needs, and the check value proves it was not torn.
        let (id, key, response) = decode(&std::fs::read(&files[0]).unwrap()).expect("decode");
        assert_eq!(id, 7);
        assert_eq!(key, "/restart\u{1}\u{1}");
        assert_eq!(&response.body[..], b"warm");
        // A fresh writer over the same root follows the same ID with no work.
        let mut fresh = DiskWriter::new(dir.path().to_path_buf(), 128 * 1024 * 1024).unwrap();
        fresh.use_id(7);
        assert_eq!(fresh.tick(&mut Cache::new()).written, 0);
        assert_eq!(fresh.dir_bytes(), 0);
    }

    /// The tick is a no-op while the ID is unknown, because there is nothing
    /// to write under.
    ///
    /// Break: write into the root directory, and a file appears after a tick
    /// with no ID.
    #[test]
    fn no_id_means_no_write() {
        let dir = tempfile::tempdir().unwrap();
        let mut cache = Cache::new();
        let mut w = DiskWriter::new(dir.path().to_path_buf(), 128 * 1024 * 1024).unwrap();
        let key = stored(&mut cache, "/early", b"body");
        w.queue(key, &mut cache);
        assert_eq!(w.tick(&mut cache).written, 0);
        assert_eq!(files(dir.path()).len(), 0);
    }

    /// The budget is per tick and a slow entry that cannot fit after earlier
    /// writes is left queued, not dropped.
    ///
    /// Break: drop a key that does not fit the remaining budget, and the
    /// second tick writes nothing.
    #[test]
    fn a_leftover_key_waits_for_the_next_tick() {
        let dir = tempfile::tempdir().unwrap();
        let mut cache = Cache::new();
        let mut w = writer(dir.path(), 128 * 1024 * 1024);
        // Three MiB, then three again: the first fills most of a tick and the
        // second cannot fit beside it.
        let big = vec![0u8; 3 * 1024 * 1024 - 4096];
        for i in 0..2 {
            let key = stored(&mut cache, &format!("/big{i}"), &big);
            w.queue(key, &mut cache);
        }
        assert_eq!(w.tick(&mut cache).written, 1);
        assert_eq!(w.queued(), 1, "the second key is still waiting");
        assert_eq!(w.tick(&mut cache).written, 1);
        assert_eq!(w.queued(), 0);
    }
}
