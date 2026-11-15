//! Logging for every service, over the `m6-log` crate.
//!
//! `m6-log` owns the two Sinks, the JSON line format, the macros and the log
//! heartbeat. This module re-exports them as `m6_core::log`, reads the `[log]`
//! level from `site.toml` and the service config, and adds [`watch`], the one
//! piece that needs the reactor.
//!
//! Resolution order is fixed and shared: `site.toml`'s `[log]`, then the
//! service config's `[log]`, then `--log-level`. The level is read once, at
//! start, and a change takes a restart.
//!
//! **The analytics file can be reopened.**
//! A rotation tool renames the file and sends SIGUSR1 (see `signal`), and the
//! service's loop calls [`reopen`]. That call flushes the buffered lines, then
//! opens the path again. From then on every line reaches the new file and none
//! reaches the renamed one.

use std::io;
use std::path::Path;
use std::time::{Instant, UNIX_EPOCH};

pub use m6_log::pulse;
pub use m6_log::{
    debug, error, flush, info, init, init_with_setting, stop, tick, trace, warn, with_analytics,
    Level, Line, LogPulse, Sink,
};

/// Watch stdout for writability under `token` while the operational Sink holds
/// bytes a full destination would not take.
///
/// A service's loop calls this after the events of one turn are handled, before
/// it waits again. The Sink never blocks: a full stdout keeps the bytes and
/// [`Sink::wants_write`] answers true, which registers the descriptor here.
/// Edge-triggered readiness reports it once per change, and the loop's `LOG`
/// arm calls [`flush`], which writes until the destination is empty or would
/// block again. Once drained, this removes the descriptor from the reactor.
pub fn watch(
    registry: &crate::platform::Registry,
    token: crate::platform::Token,
) -> io::Result<()> {
    let wants = m6_log::stdout_wants_write();
    let watched = STDOUT_WATCHED.with(|w| w.get());
    if wants && !watched {
        let Some(fd) = m6_log::stdout_fd() else {
            return Ok(());
        };
        registry.register_fd(fd, token, crate::platform::Interest::WRITABLE)?;
        STDOUT_WATCHED.with(|w| w.set(true));
    } else if !wants && watched {
        if let Some(fd) = m6_log::stdout_fd() {
            registry.deregister_fd(fd)?;
        }
        STDOUT_WATCHED.with(|w| w.set(false));
    }
    Ok(())
}

thread_local! {
    /// Whether the operational Sink's descriptor is registered with this
    /// thread's reactor. One loop thread owns one reactor and one Sink.
    static STDOUT_WATCHED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

// ── Is logging alive ─────────────────────────────────────────────────────────

/// When this process last reopened the analytics file, and when a reopen last
/// failed.
///
/// Atomics only, like [`LogPulse`]. A count of zero means "never". The node
/// answers the ages itself, so a reader's clock never matters.
pub struct ReopenRecord {
    ok: std::sync::atomic::AtomicU64,
    failed: std::sync::atomic::AtomicU64,
    /// Milliseconds since process start, at the last success.
    last_ok_ms: std::sync::atomic::AtomicU64,
    /// Unix seconds at the last success.
    last_ok_unix: std::sync::atomic::AtomicU64,
    /// Milliseconds since process start, at the last failure.
    last_fail_ms: std::sync::atomic::AtomicU64,
}

impl ReopenRecord {
    const fn new() -> Self {
        Self {
            ok: std::sync::atomic::AtomicU64::new(0),
            failed: std::sync::atomic::AtomicU64::new(0),
            last_ok_ms: std::sync::atomic::AtomicU64::new(0),
            last_ok_unix: std::sync::atomic::AtomicU64::new(0),
            last_fail_ms: std::sync::atomic::AtomicU64::new(0),
        }
    }

    fn record_success(&self) {
        use std::sync::atomic::Ordering::Relaxed;
        self.last_ok_ms.store(elapsed_ms(), Relaxed);
        self.last_ok_unix.store(unix_now(), Relaxed);
        self.ok.fetch_add(1, Relaxed);
    }

    fn record_failure(&self) {
        use std::sync::atomic::Ordering::Relaxed;
        self.last_fail_ms.store(elapsed_ms(), Relaxed);
        self.failed.fetch_add(1, Relaxed);
    }

    /// Reopens that succeeded since start.
    pub fn successes(&self) -> u64 {
        self.ok.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Reopens that failed since start.
    pub fn failures(&self) -> u64 {
        self.failed.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Seconds since the last successful reopen, or `None` if there was none.
    pub fn seconds_since_last_success(&self) -> Option<u64> {
        use std::sync::atomic::Ordering::Relaxed;
        if self.ok.load(Relaxed) == 0 {
            return None;
        }
        Some(elapsed_ms().saturating_sub(self.last_ok_ms.load(Relaxed)) / 1000)
    }

    /// Unix time of the last successful reopen, or `None` if there was none.
    pub fn last_success_unix(&self) -> Option<u64> {
        use std::sync::atomic::Ordering::Relaxed;
        if self.ok.load(Relaxed) == 0 {
            return None;
        }
        Some(self.last_ok_unix.load(Relaxed)).filter(|&u| u != 0)
    }

    /// Seconds since the last failed reopen, or `None` if there was none.
    pub fn seconds_since_last_failure(&self) -> Option<u64> {
        use std::sync::atomic::Ordering::Relaxed;
        if self.failed.load(Relaxed) == 0 {
            return None;
        }
        Some(elapsed_ms().saturating_sub(self.last_fail_ms.load(Relaxed)) / 1000)
    }
}

static REOPENS: ReopenRecord = ReopenRecord::new();

fn process_start() -> Instant {
    use std::sync::OnceLock;
    static START: OnceLock<Instant> = OnceLock::new();
    *START.get_or_init(crate::platform::now)
}

fn elapsed_ms() -> u64 {
    process_start().elapsed().as_millis() as u64
}

fn unix_now() -> u64 {
    crate::platform::wall()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// When this process last reopened its log file.
pub fn reopens() -> &'static ReopenRecord {
    &REOPENS
}

/// Whether this process writes a log file that can be reopened.
pub fn writes_log_file() -> bool {
    m6_log::with_analytics(|_| {})
}

/// Flush the analytics file and open its path afresh. Called on
/// [`crate::platform::Signal::ReopenLogs`].
///
/// A process with no analytics file answers `Ok` and writes nothing. A failed
/// open leaves the previous file open, so the lines keep reaching the file the
/// process already had, and the failure is recorded for [`reopens`].
pub fn reopen() -> io::Result<()> {
    if !writes_log_file() {
        return Ok(());
    }
    let result = m6_log::reopen();
    match &result {
        Ok(()) => REOPENS.record_success(),
        Err(_) => REOPENS.record_failure(),
    }
    result
}

// ── Reopening the log file: the tail reader ──────────────────────────────────

/// Append the last `max_bytes` of a plain or gzip file to `out`.
///
/// A plain file is read from a seek, so only the tail is read.
/// A gzip file cannot seek and is decoded from the start.
/// A gzip file that is cut short yields what decoded before the cut.
fn append_tail(path: &Path, max_bytes: usize, out: &mut Vec<u8>) -> std::io::Result<()> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path)?;
    if !path.extension().is_some_and(|e| e == "gz") {
        let len = file.metadata()?.len();
        let max = max_bytes as u64;
        if len > max {
            file.seek(SeekFrom::Start(len - max))?;
        }
        file.read_to_end(out)?;
        return Ok(());
    }
    let mut reader = flate2::read::GzDecoder::new(file);
    let mut kept: Vec<u8> = Vec::new();
    let mut chunk = vec![0u8; 64 * 1024];
    loop {
        let n = match reader.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => n,
            // A gzip file cut short ends the read, it is not a failure.
            Err(_) if !kept.is_empty() => break,
            Err(e) => return Err(e),
        };
        kept.extend_from_slice(&chunk[..n]);
        if kept.len() > max_bytes * 2 {
            kept.drain(..kept.len() - max_bytes);
        }
    }
    let skip = kept.len().saturating_sub(max_bytes);
    out.extend_from_slice(&kept[skip..]);
    Ok(())
}

fn into_text(bytes: Vec<u8>) -> String {
    String::from_utf8(bytes).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
}

/// The last `max_bytes` of a plain or gzip file, as text.
///
/// A gzip file that is cut short yields what decoded before the cut.
/// A first line that starts mid-record is expected and is the caller's to skip.
pub fn read_tail(path: &Path, max_bytes: usize) -> std::io::Result<String> {
    let mut out = Vec::new();
    append_tail(path, max_bytes, &mut out)?;
    Ok(into_text(out))
}

/// What `site.toml`'s `[log]` says about this process: its level.
///
/// A read or parse failure is an error naming the file, a `level` that is
/// present and not a string is an error naming the key, and any key other than
/// `level` is an error naming the setting. An absent key keeps the default
/// `"info"`. The refusal matches the service config: every m6 log is a JSON
/// line, so `format` and a misspelled `level` stop the service at start.
pub fn read_site_log_config(site_toml: &Path) -> anyhow::Result<String> {
    let text = std::fs::read_to_string(site_toml)
        .map_err(|e| anyhow::anyhow!("{} cannot be read: {e}", site_toml.display()))?;
    let val: toml::Value = text.parse::<toml::Value>().map_err(|e| {
        anyhow::anyhow!("{} is not valid TOML: {}", site_toml.display(), e.message())
    })?;
    let Some(log) = val.get("log") else {
        return Ok("info".to_string());
    };
    let table = log
        .as_table()
        .ok_or_else(|| anyhow::anyhow!("[log] must be a table"))?;
    if let Some(key) = table.keys().find(|k| k.as_str() != "level") {
        anyhow::bail!("unknown key [log] {key}: known keys are level");
    }
    match table.get("level") {
        Some(v) => Ok(v
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("[log] level must be a string, got {v}"))?
            .to_string()),
        None => Ok("info".to_string()),
    }
}

/// Parse a log level string. Accepts `trace`, `debug`, `info`, `warn` (or
/// `warning`) and `error`, in any case. Anything else is an error naming the
/// value.
pub fn parse_level(s: &str) -> anyhow::Result<Level> {
    match s.to_ascii_lowercase().as_str() {
        "trace" => Ok(Level::Trace),
        "debug" => Ok(Level::Debug),
        "info" => Ok(Level::Info),
        "warn" | "warning" => Ok(Level::Warn),
        "error" => Ok(Level::Error),
        _ => Err(anyhow::anyhow!(
            "unknown log level {s:?}: use trace, debug, info, warn or error"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_level_known() {
        assert_eq!(parse_level("debug").unwrap(), Level::Debug);
        assert_eq!(parse_level("INFO").unwrap(), Level::Info);
        assert_eq!(parse_level("WARN").unwrap(), Level::Warn);
        assert_eq!(parse_level("error").unwrap(), Level::Error);
        assert_eq!(parse_level("trace").unwrap(), Level::Trace);
        assert_eq!(parse_level("warning").unwrap(), Level::Warn);
    }

    #[test]
    fn an_unknown_level_is_refused_and_named() {
        let err = parse_level("verbose").unwrap_err().to_string();
        assert!(err.contains("unknown log level"), "{err}");
        assert!(err.contains("verbose"), "{err}");
        assert!(parse_level("").is_err());
    }

    #[test]
    fn a_site_toml_that_does_not_parse_is_an_error_for_the_log() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("site.toml");
        std::fs::write(&f, "[log").unwrap();
        assert!(read_site_log_config(&f).is_err());
    }

    #[test]
    fn a_site_toml_level_is_read() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("site.toml");
        std::fs::write(&f, "[log]\nlevel = \"warn\"\n").unwrap();
        assert_eq!(read_site_log_config(&f).unwrap(), "warn");
        std::fs::write(&f, "[site]\nname = \"example\"\n").unwrap();
        assert_eq!(read_site_log_config(&f).unwrap(), "info");
    }

    #[test]
    fn a_site_toml_format_key_is_refused_and_named() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("site.toml");
        std::fs::write(&f, "[log]\nlevel = \"warn\"\nformat = \"text\"\n").unwrap();
        let err = read_site_log_config(&f).unwrap_err().to_string();
        assert!(err.contains("unknown key [log] format"), "{err}");
        std::fs::write(&f, "[log]\nlevle = \"warn\"\n").unwrap();
        let err = read_site_log_config(&f).unwrap_err().to_string();
        assert!(err.contains("unknown key [log] levle"), "{err}");
    }

    #[test]
    fn a_site_toml_log_table_that_is_not_a_table_is_refused() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("site.toml");
        std::fs::write(&f, "log = 3\n").unwrap();
        let err = read_site_log_config(&f).unwrap_err().to_string();
        assert!(err.contains("[log] must be a table"), "{err}");
    }

    #[test]
    fn a_non_string_level_is_an_error_naming_the_key() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("site.toml");
        std::fs::write(&f, "[log]\nlevel = 3\n").unwrap();
        let err = read_site_log_config(&f).unwrap_err().to_string();
        assert!(err.contains("[log] level"), "{err}");
    }

    #[test]
    fn read_tail_reads_plain_and_gzip_and_keeps_only_the_end() {
        let tmp = tempfile::tempdir().unwrap();
        let text: String = (0..1000).map(|i| format!("{i:04}\n")).collect();
        let plain = tmp.path().join("p");
        std::fs::write(&plain, &text).unwrap();
        let gz = tmp.path().join("g.gz");
        let mut enc = flate2::write::GzEncoder::new(
            std::fs::File::create(&gz).unwrap(),
            flate2::Compression::default(),
        );
        use std::io::Write;
        enc.write_all(text.as_bytes()).unwrap();
        enc.finish().unwrap();
        assert_eq!(read_tail(&plain, 1 << 20).unwrap(), text);
        assert_eq!(read_tail(&gz, 1 << 20).unwrap(), text);
        let tail = read_tail(&gz, 50).unwrap();
        assert_eq!(tail, &text[text.len() - 50..]);
        // Cut short: what decoded is kept.
        let bytes = std::fs::read(&gz).unwrap();
        std::fs::write(&gz, &bytes[..bytes.len() / 2]).unwrap();
        assert!(!read_tail(&gz, 1 << 20).unwrap().is_empty());
    }
}

#[cfg(test)]
mod reopen_tests {
    use super::*;

    #[test]
    fn the_record_says_never_until_a_reopen_succeeds() {
        let rec = ReopenRecord::new();
        assert_eq!(rec.seconds_since_last_success(), None);
        assert_eq!(rec.last_success_unix(), None);
        assert_eq!(rec.successes(), 0);
        assert_eq!(rec.seconds_since_last_failure(), None);
        rec.record_success();
        assert_eq!(rec.seconds_since_last_success(), Some(0));
        let now = unix_now();
        assert!(now.abs_diff(rec.last_success_unix().unwrap()) <= 5);
        rec.record_failure();
        assert_eq!(rec.failures(), 1);
        assert_eq!(rec.seconds_since_last_failure(), Some(0));
    }

    #[test]
    fn a_reopen_with_no_log_file_is_ok_and_records_nothing() {
        // No `init` on this thread, which is the one case `reopen` answers
        // without touching the filesystem.
        assert!(!writes_log_file());
        assert!(reopen().is_ok());
    }
}
