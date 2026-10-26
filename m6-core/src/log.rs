//! Logging setup, once, for every service.
//!
//! Resolution order is fixed and shared: `site.toml`'s `[log]`, then the
//! service config's `[log]`, then `--log-level`. Each service used to resolve
//! this itself and they did not agree.
//!
//! **The writer thread is why `signal::block()` must come first.**
//! `tracing_appender`'s non-blocking writer is a thread, it inherits the
//! signal mask as it was when it started, and if SIGTERM is not blocked by
//! then the kernel can deliver it there and kill the process at the default
//! disposition.
//!
//! It is also why a line logged after the shutdown flag is set can be lost:
//! process exit discards whatever is still queued. See the `signal` module.
//!
//! **The analytics file can be reopened.**
//! A rotation tool renames the file and sends SIGUSR1 (see `signal`), and the signal thread calls [`reopen_log_file`].
//! The reopen opens the new file while holding the lock the writer takes for every line.
//! The writer therefore cannot write to the old file once the lock is released, so the live path reappearing proves the renamed file is finished.
//! If the open fails, the old handle stays in place and the failure is recorded.

use anyhow::Result;
use std::path::Path;
use tracing::Level;
use tracing_appender::non_blocking::{NonBlocking, WorkerGuard};
use tracing_subscriber::filter::{filter_fn, FilterExt, LevelFilter};
use tracing_subscriber::layer::Filter;
use tracing_subscriber::prelude::*;
use tracing_subscriber::{fmt, reload, Layer, Registry};

type BoxedLayer = Box<dyn Layer<Registry> + Send + Sync + 'static>;
type BoxedFilter = Box<dyn Filter<Registry> + Send + Sync + 'static>;

/// Events logged with `target: "analytics"` are routed only to the dedicated
/// analytics file (see [`init_with_analytics`]) — the main dev/prod log layer
/// excludes them so per-request traffic logging doesn't drown out ordinary
/// operational logging.
const ANALYTICS_TARGET: &str = "analytics";

// ── Is logging alive ─────────────────────────────────────────────────────────

/// A heartbeat for the main log layer.
///
/// A config reload can silence every target except `analytics`, found
/// 2026-09-06 and still live. When it happens the process keeps serving, the
/// analytics file keeps growing, and the operational log goes quiet: stats,
/// pool events, warnings and errors all stop. Nothing about the process looks
/// wrong, and a performance check then reads the silence as idle traffic.
///
/// Detecting it used to mean sshing to each node and counting log targets out
/// of `journalctl`. It does not have to: the process knows what it emitted.
/// This counts events **after** the reload-able filter, because the whole
/// point is to observe what the filter is letting through rather than what the
/// code tried to log.
///
/// Two atomics and no lock. It is incremented once per emitted event, on a
/// path that is already formatting and writing a log line, and `analytics` is
/// excluded from the main layer so per-request traffic does not reach it.
pub struct LogPulse {
    events: std::sync::atomic::AtomicU64,
    /// Milliseconds since process start, at the last emitted event.
    last_ms: std::sync::atomic::AtomicU64,
}

impl LogPulse {
    const fn new() -> Self {
        Self {
            events: std::sync::atomic::AtomicU64::new(0),
            last_ms: std::sync::atomic::AtomicU64::new(0),
        }
    }

    fn record(&self) {
        use std::sync::atomic::Ordering::Relaxed;
        self.events.fetch_add(1, Relaxed);
        self.last_ms.store(elapsed_ms(), Relaxed);
    }

    /// Events the main layer has emitted since start.
    pub fn events(&self) -> u64 {
        self.events.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Seconds since the last emitted event, or `None` if none ever was.
    ///
    /// A healthy m6-http emits `periodic stats` every ten seconds, so anything
    /// past about thirty on a running process means the main layer has gone
    /// quiet. That is the check, and it needs no log reader.
    pub fn seconds_since_last(&self) -> Option<u64> {
        use std::sync::atomic::Ordering::Relaxed;
        if self.events.load(Relaxed) == 0 {
            return None;
        }
        Some((elapsed_ms().saturating_sub(self.last_ms.load(Relaxed))) / 1000)
    }
}

static PULSE: LogPulse = LogPulse::new();

fn process_start() -> std::time::Instant {
    use std::sync::OnceLock;
    static START: OnceLock<std::time::Instant> = OnceLock::new();
    *START.get_or_init(std::time::Instant::now)
}

fn elapsed_ms() -> u64 {
    process_start().elapsed().as_millis() as u64
}

/// The process-wide log heartbeat.
pub fn pulse() -> &'static LogPulse {
    &PULSE
}

// ── Reopening the log file ──

fn open_append(path: &Path) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
}

/// The log file and the absolute path it was opened at.
///
/// The path is made absolute once, so a later change of working directory
/// cannot redirect a reopen.
struct ReopenableFile {
    path: std::path::PathBuf,
    file: std::sync::Mutex<std::fs::File>,
}

impl ReopenableFile {
    fn open(path: &Path) -> std::io::Result<Self> {
        let path = std::path::absolute(path)?;
        let file = open_append(&path)?;
        Ok(Self {
            path,
            file: std::sync::Mutex::new(file),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, std::fs::File> {
        self.file
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Open the path afresh and swap it in. On failure the old handle stays.
    ///
    /// The open happens under the lock on purpose. While this thread holds
    /// the lock the writer cannot write to the old file, so once the new file
    /// exists no further line will reach the renamed one.
    fn reopen(&self) -> std::io::Result<()> {
        let mut guard = self.lock();
        let fresh = open_append(&self.path)?;
        let old = std::mem::replace(&mut *guard, fresh);
        drop(guard);
        drop(old);
        Ok(())
    }
}

/// The writer `tracing_appender` owns for the analytics file.
#[derive(Clone)]
struct FileWriter(std::sync::Arc<ReopenableFile>);

impl std::io::Write for FileWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().write(buf)
    }

    /// `tracing_appender` calls this once per event, so taking the lock once
    /// here means a swap lands between lines and never inside one.
    fn write_all(&mut self, buf: &[u8]) -> std::io::Result<()> {
        self.0.lock().write_all(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.0.lock().flush()
    }
}

/// When the log file was last reopened, and when a reopen last failed.
///
/// Atomics only, like [`LogPulse`]. A count of zero means "never".
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
        let unix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        self.last_ok_ms.store(elapsed_ms(), Relaxed);
        self.last_ok_unix.store(unix, Relaxed);
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

static LOG_FILE: std::sync::OnceLock<std::sync::Arc<ReopenableFile>> = std::sync::OnceLock::new();
static REOPENS: ReopenRecord = ReopenRecord::new();

/// What a reopen request did.
#[derive(Debug)]
pub enum Reopened {
    /// This process writes no log file.
    NoFile,
    /// The path was opened afresh.
    Done(std::path::PathBuf),
    /// The open failed. The previous file is still being written.
    Failed(std::path::PathBuf, std::io::Error),
}

fn reopen_in(slot: Option<&ReopenableFile>, rec: &ReopenRecord) -> Reopened {
    let Some(f) = slot else {
        return Reopened::NoFile;
    };
    match f.reopen() {
        Ok(()) => {
            rec.record_success();
            Reopened::Done(f.path.clone())
        }
        Err(e) => {
            rec.record_failure();
            Reopened::Failed(f.path.clone(), e)
        }
    }
}

/// Reopen the analytics file at its path. Called on [`crate::signal::REOPEN_SIGNAL`].
pub fn reopen_log_file() -> Reopened {
    reopen_in(LOG_FILE.get().map(|a| &**a), &REOPENS)
}

/// When this process last reopened its log file.
pub fn reopens() -> &'static ReopenRecord {
    &REOPENS
}

/// Whether this process writes a log file that can be reopened.
pub fn writes_log_file() -> bool {
    LOG_FILE.get().is_some()
}

/// The newest rotated sibling of a log file, if there is one.
///
/// A rotation tool that renames the live file with a date extension leaves
/// `<name>-YYYYmmddHH`, which is what logrotate's `dateext` produces for an
/// hourly rotation, and compression adds `.gz`.
/// The highest stamp is the newest.
/// An uncompressed file is preferred when both exist, because the `.gz` may still be written.
pub fn newest_rotated(live: &Path) -> Option<std::path::PathBuf> {
    let dir = live.parent().filter(|d| !d.as_os_str().is_empty());
    let dir = dir.unwrap_or_else(|| Path::new("."));
    let name = live.file_name()?.to_str()?;
    let prefix = format!("{name}-");
    let mut best: Option<(String, bool, std::path::PathBuf)> = None;
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let file = entry.file_name();
        let Some(file) = file.to_str() else { continue };
        let Some(rest) = file.strip_prefix(&prefix) else {
            continue;
        };
        let (stamp, gz) = match rest.strip_suffix(".gz") {
            Some(stamp) => (stamp, true),
            None => (rest, false),
        };
        if stamp.len() != 10 || !stamp.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let better = match &best {
            None => true,
            Some((s, was_gz, _)) => stamp > s.as_str() || (stamp == s.as_str() && *was_gz && !gz),
        };
        if better {
            best = Some((stamp.to_string(), gz, entry.path()));
        }
    }
    best.map(|(_, _, path)| path)
}

/// The last `max_bytes` of a plain or gzip file, as text.
///
/// A gzip file that is cut short yields what decoded before the cut.
/// A first line that starts mid-record is expected and is the caller's to skip.
pub fn read_tail(path: &Path, max_bytes: usize) -> std::io::Result<String> {
    use std::io::Read;
    let file = std::fs::File::open(path)?;
    let gz = path.extension().is_some_and(|e| e == "gz");
    let mut reader: Box<dyn Read> = if gz {
        Box::new(flate2::read::GzDecoder::new(file))
    } else {
        Box::new(file)
    };
    let mut kept: Vec<u8> = Vec::new();
    let mut chunk = vec![0u8; 64 * 1024];
    loop {
        let n = match reader.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => n,
            // A gzip file cut short ends the read, it is not a failure.
            Err(_) if gz && !kept.is_empty() => break,
            Err(e) => return Err(e),
        };
        kept.extend_from_slice(&chunk[..n]);
        if kept.len() > max_bytes * 2 {
            kept.drain(..kept.len() - max_bytes);
        }
    }
    if kept.len() > max_bytes {
        kept.drain(..kept.len() - max_bytes);
    }
    Ok(String::from_utf8_lossy(&kept).into_owned())
}

/// A layer that does nothing but count. Composed with the format layer
/// *inside* the reload-able filter, so both see the same events.
struct PulseLayer;

impl<S: tracing::Subscriber> Layer<S> for PulseLayer {
    fn on_event(
        &self,
        _event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        PULSE.record();
    }
}

/// Handle returned by [`init`] that allows runtime log level reloads.
///
/// Keep the handle alive for the lifetime of the process. Dropping it flushes
/// and terminates the logging background thread.
pub struct LogHandle {
    /// Reloads the *filter*, never the layer. See [`LogHandle::reload`].
    filter: reload::Handle<BoxedFilter, Registry>,
    /// The format chosen at init. Recorded only so a reload asking for a
    /// different one can say why it is being ignored.
    format: String,
    // The main stdout writer is created once and lives for the whole process.
    // Its worker thread stops the moment this guard drops, and any writes
    // after that are silently discarded.
    _guard: WorkerGuard,
    // Same, for the analytics writer; never read again after init.
    _analytics_guard: Option<WorkerGuard>,
}

impl LogHandle {
    /// Apply a new log level to the running process.
    ///
    /// **Only the filter is swapped, never the layer, and that is the whole
    /// design.** An earlier version rebuilt the entire layer here — new writer,
    /// new `fmt` layer, new `.with_filter(..)` — and handed it to
    /// `reload::Handle::modify`. That is unsupported, and it took down logging
    /// on all three production nodes on 2026-09-06.
    ///
    /// `.with_filter(..)` produces a `Filtered` layer, and per-layer filters
    /// are assigned a `FilterId` when the subscriber is *constructed*. A
    /// `Filtered` layer swapped in afterwards has no id, so the first event
    /// through it panics with "a `Filtered` layer was used, but it had no
    /// `FilterId`". The visible result was not a crash: m6-http kept serving
    /// traffic and reporting itself healthy while every log target except
    /// `analytics` (a separate layer, never touched by reload) went silent —
    /// stats, pool events, warnings and errors all gone. Since a deploy
    /// touches `site.toml` and that triggers a reload, every deploy blinded
    /// the server to its own errors.
    ///
    /// Swapping the filter is the supported operation: the `Filtered` wrapper
    /// is built once at registration and keeps its id forever, and only the
    /// filter value inside it changes. Keeping the writer for the process
    /// lifetime also removes the per-reload writer churn that made the old
    /// version look plausible.
    ///
    /// Format cannot change this way — json and text are different layer types
    /// — so a reload requesting a different format keeps the current one and
    /// says so rather than pretending. In practice format is set once per
    /// deployment and never toggled at runtime; the level is the useful knob.
    pub fn reload(&self, format: &str, level: &str) {
        if format != self.format {
            tracing::warn!(
                current = %self.format,
                requested = %format,
                "log format cannot be changed without a restart; keeping the \
                 current format (the new level is still applied)"
            );
        }
        let lvl = parse_level(level);
        // Built inside the closure so the handle owns the only copy.
        if let Err(e) = self.filter.modify(|f| *f = make_filter(lvl)) {
            // Loud, and at error level: if this fails the process keeps the
            // old level, which is recoverable — but silence here is what
            // turned the original bug into an invisible one.
            tracing::error!(error = %e, "log level reload failed; keeping the previous level");
        }
    }
}

/// The main layer's filter: a level gate, plus the exclusion of
/// `target: "analytics"` events, which are high-volume, per-request and
/// machine-read. Routing them here as well would drown out ordinary
/// operational logging (and double-write them when [`init_with_analytics`]
/// is in use).
fn make_filter(level: Level) -> BoxedFilter {
    Box::new(
        LevelFilter::from_level(level).and(filter_fn(|meta| meta.target() != ANALYTICS_TARGET)),
    )
}

/// Build the main stdout layer around an already-registered reloadable filter.
///
/// The filter is passed in rather than built here so that the `Filtered`
/// wrapper this produces is the one registered with the subscriber, and stays
/// registered. Nothing about this layer is replaceable at runtime.
fn make_main_layer(
    format: &str,
    writer: NonBlocking,
    filter: reload::Layer<BoxedFilter, Registry>,
) -> BoxedLayer {
    // `and_then` puts the counter inside the same `Filtered` wrapper as the
    // format layer, so it counts exactly what is emitted. A counter outside
    // the filter would keep ticking while the log was silenced, which is the
    // one thing it must not do.
    match format {
        "json" => Box::new(
            fmt::layer()
                .json()
                .with_writer(writer)
                .with_current_span(true)
                .and_then(PulseLayer)
                .with_filter(filter),
        ),
        _ => Box::new(
            fmt::layer()
                .with_writer(writer)
                .and_then(PulseLayer)
                .with_filter(filter),
        ),
    }
}

/// Build the analytics-only layer: always JSON (machine-read regardless of
/// the main log's format), always to `path` (append mode, created if
/// missing), only events tagged `target: "analytics"`.
fn make_analytics_layer(path: &Path) -> Result<(BoxedLayer, WorkerGuard)> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = std::sync::Arc::new(ReopenableFile::open(path)?);
    // The subscriber is installed once per process (`try_init`), so only the
    // first registration is reopenable.
    let _ = LOG_FILE.set(std::sync::Arc::clone(&file));
    let (writer, guard) = tracing_appender::non_blocking(FileWriter(file));
    let filter = filter_fn(|meta| meta.target() == ANALYTICS_TARGET);
    let layer: BoxedLayer = Box::new(
        fmt::layer()
            .json()
            .with_writer(writer)
            .with_current_span(false)
            .with_target(false)
            .with_filter(filter),
    );
    Ok((layer, guard))
}

/// Initialize the tracing subscriber with a non-blocking stdout writer.
///
/// Returns a [`LogHandle`] that must be kept alive for the lifetime of the
/// process. Call [`LogHandle::reload`] to change the log level at runtime;
/// the format is fixed for the life of the process.
///
/// `format`:
///   - `"json"` → JSON output (production)
///   - anything else → human-readable text (development)
///
/// `level`: `"debug"`, `"info"`, `"warn"`, `"error"` (defaults to `"info"`)
pub fn init(format: &str, level: &str) -> Result<LogHandle> {
    init_with_analytics(format, level, None)
}

/// Like [`init`], but also registers a second, always-JSON layer that
/// captures only `target: "analytics"` events and appends them to
/// `analytics_path` — independent of the main log's format/level, and never
/// mixed into it. Pass `None` to behave exactly like [`init`].
///
/// `analytics_path` should point outside any directory that gets wiped on
/// redeploy (e.g. not inside a rendered/generated site tree) — the file is
/// opened in append mode and grown across restarts. Rotate it by renaming the
/// file and sending SIGUSR1: the process then reopens the path.
pub fn init_with_analytics(
    format: &str,
    level: &str,
    analytics_path: Option<&Path>,
) -> Result<LogHandle> {
    // Anchor the clock before anything can log, so `seconds_since_last` is
    // measured from process start rather than from the first call.
    let _ = process_start();
    let lvl = parse_level(level);
    let (writer, guard) = tracing_appender::non_blocking(std::io::stdout());

    // The reload handle is over the FILTER. Registering the `Filtered` layer
    // that wraps it is what assigns the FilterId, and it is never replaced.
    let (reload_filter, filter_handle) = reload::Layer::new(make_filter(lvl));
    let main_layer = make_main_layer(format, writer, reload_filter);

    // Chaining multiple `.with(boxed_layer)` calls changes the subscriber
    // type at each step (S becomes `Layered<_, Registry>`), which a
    // `Box<dyn Layer<Registry>>` no longer satisfies. Collecting into a
    // `Vec<BoxedLayer>` and calling `.with()` once keeps every element's
    // trait object anchored to plain `Registry`.
    let mut layers: Vec<BoxedLayer> = vec![main_layer];
    let mut analytics_guard = None;
    if let Some(path) = analytics_path {
        let (l, g) = make_analytics_layer(path)?;
        layers.push(l);
        analytics_guard = Some(g);
    }

    Registry::default()
        .with(layers)
        .try_init()
        .map_err(|e| anyhow::anyhow!("failed to install tracing subscriber: {}", e))?;

    Ok(LogHandle {
        filter: filter_handle,
        format: format.to_string(),
        _guard: guard,
        _analytics_guard: analytics_guard,
    })
}

/// Read `[log]` from `site_dir/site.toml`. Returns `(level, format)`.
/// Falls back to `("info", "json")` if the file is absent or unparseable.
pub fn read_site_log_config(site_dir: &Path) -> (String, String) {
    let site_toml = site_dir.join("site.toml");
    if let Ok(text) = std::fs::read_to_string(site_toml) {
        if let Ok(val) = text.parse::<toml::Value>() {
            let level = val
                .get("log")
                .and_then(|l| l.get("level"))
                .and_then(|v| v.as_str())
                .unwrap_or("info")
                .to_string();
            let format = val
                .get("log")
                .and_then(|l| l.get("format"))
                .and_then(|v| v.as_str())
                .unwrap_or("json")
                .to_string();
            return (level, format);
        }
    }
    ("info".to_string(), "json".to_string())
}

/// Parse a log level string, defaulting to `Level::INFO` on unrecognised input.
pub fn parse_level(s: &str) -> Level {
    match s.to_ascii_lowercase().as_str() {
        "trace" => Level::TRACE,
        "debug" => Level::DEBUG,
        "info" => Level::INFO,
        "warn" | "warning" => Level::WARN,
        "error" => Level::ERROR,
        _ => Level::INFO,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_level_known() {
        assert_eq!(parse_level("debug"), Level::DEBUG);
        assert_eq!(parse_level("INFO"), Level::INFO);
        assert_eq!(parse_level("WARN"), Level::WARN);
        assert_eq!(parse_level("error"), Level::ERROR);
        assert_eq!(parse_level("trace"), Level::TRACE);
    }

    #[test]
    fn test_parse_level_unknown_defaults_info() {
        assert_eq!(parse_level(""), Level::INFO);
        assert_eq!(parse_level("verbose"), Level::INFO);
    }
}

#[cfg(test)]
mod pulse_tests {
    use super::*;

    /// The counter must not tick before anything is logged, and
    /// `seconds_since_last` must say "never" rather than "zero seconds ago".
    ///
    /// Zero would read as perfectly healthy on a process that has never
    /// emitted a line, which is exactly the state this exists to catch.
    #[test]
    fn never_logged_is_not_logged_recently() {
        let p = LogPulse::new();
        assert_eq!(p.events(), 0);
        assert_eq!(p.seconds_since_last(), None);
    }

    #[test]
    fn recording_advances_the_count_and_starts_the_clock() {
        let p = LogPulse::new();
        p.record();
        p.record();
        assert_eq!(p.events(), 2);
        assert_eq!(p.seconds_since_last(), Some(0));
    }
}

#[cfg(test)]
mod reopen_tests {
    use super::*;
    use std::io::Write;

    fn read(p: &Path) -> String {
        std::fs::read_to_string(p).unwrap()
    }

    #[test]
    fn a_reopen_after_a_rename_sends_new_lines_to_the_new_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("a.ndjson");
        let f = std::sync::Arc::new(ReopenableFile::open(&path).unwrap());
        let mut w = FileWriter(std::sync::Arc::clone(&f));
        w.write_all(b"a\n").unwrap();
        let moved = tmp.path().join("a.ndjson.1");
        std::fs::rename(&path, &moved).unwrap();
        f.reopen().unwrap();
        w.write_all(b"b\n").unwrap();
        assert_eq!(read(&moved), "a\n");
        assert_eq!(read(&path), "b\n");
    }

    #[test]
    fn concurrent_reopens_lose_tear_and_duplicate_nothing() {
        const LINES: usize = 20_000;
        const REOPENS: usize = 20;
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("a.ndjson");
        let f = std::sync::Arc::new(ReopenableFile::open(&path).unwrap());
        let mut w = FileWriter(std::sync::Arc::clone(&f));
        let writer = std::thread::spawn(move || {
            for i in 0..LINES {
                w.write_all(format!("{i:06}\n").as_bytes()).unwrap();
            }
        });
        for k in 0..REOPENS {
            std::fs::rename(&path, tmp.path().join(format!("a.ndjson.{k}"))).unwrap();
            f.reopen().unwrap();
            std::thread::sleep(std::time::Duration::from_micros(200));
        }
        writer.join().unwrap();
        let mut seen = vec![0u8; LINES];
        for entry in std::fs::read_dir(tmp.path()).unwrap() {
            let text = read(&entry.unwrap().path());
            let mut last: Option<usize> = None;
            for line in text.split_inclusive('\n') {
                assert_eq!(line.len(), 7, "torn line {line:?}");
                let n: usize = line.trim_end().parse().expect("a number");
                assert!(last.is_none_or(|l| n > l), "out of order within a file");
                last = Some(n);
                seen[n] += 1;
            }
        }
        assert!(
            seen.iter().all(|&c| c == 1),
            "a line was lost or duplicated"
        );
    }

    #[test]
    fn a_failed_reopen_keeps_writing_to_the_old_file() {
        let tmp = tempfile::tempdir().unwrap();
        let sub = tmp.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        let path = sub.join("a.ndjson");
        let f = std::sync::Arc::new(ReopenableFile::open(&path).unwrap());
        let mut w = FileWriter(std::sync::Arc::clone(&f));
        w.write_all(b"a\n").unwrap();
        let moved = tmp.path().join("moved");
        std::fs::rename(&path, &moved).unwrap();
        std::fs::remove_dir(&sub).unwrap();
        assert!(f.reopen().is_err());
        w.write_all(b"b\n").unwrap();
        assert_eq!(read(&moved), "a\nb\n");
    }

    #[test]
    fn the_record_says_never_until_a_reopen_succeeds() {
        let rec = ReopenRecord::new();
        assert_eq!(rec.seconds_since_last_success(), None);
        assert_eq!(rec.last_success_unix(), None);
        assert_eq!(rec.successes(), 0);
        assert_eq!(rec.seconds_since_last_failure(), None);
        rec.record_success();
        assert_eq!(rec.seconds_since_last_success(), Some(0));
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        assert!(now.abs_diff(rec.last_success_unix().unwrap()) <= 5);
        rec.record_failure();
        assert_eq!(rec.failures(), 1);
        assert_eq!(rec.seconds_since_last_failure(), Some(0));
    }

    #[test]
    fn reopen_with_no_log_file_says_so() {
        let rec = ReopenRecord::new();
        assert!(matches!(reopen_in(None, &rec), Reopened::NoFile));
        assert_eq!(rec.successes(), 0);
        assert_eq!(rec.failures(), 0);
    }

    #[test]
    fn newest_rotated_picks_the_highest_stamp_and_prefers_the_plain_twin() {
        let tmp = tempfile::tempdir().unwrap();
        let live = tmp.path().join("a.ndjson");
        for n in [
            "a.ndjson",
            "a.ndjson.1",
            "a.ndjson-2026102600.gz",
            "a.ndjson-2026102601.gz",
            "a.ndjson-2026102601",
            "a.ndjson-short.gz",
            "other.ndjson-2026102609.gz",
        ] {
            std::fs::write(tmp.path().join(n), b"").unwrap();
        }
        assert_eq!(
            newest_rotated(&live),
            Some(tmp.path().join("a.ndjson-2026102601"))
        );
        std::fs::remove_file(tmp.path().join("a.ndjson-2026102601")).unwrap();
        assert_eq!(
            newest_rotated(&live),
            Some(tmp.path().join("a.ndjson-2026102601.gz"))
        );
        assert_eq!(newest_rotated(&tmp.path().join("none.log")), None);
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

    #[test]
    fn the_reopen_path_is_absolute() {
        let tmp = tempfile::tempdir().unwrap();
        let abs = tmp.path().join("a.ndjson");
        let f = ReopenableFile::open(&abs).unwrap();
        assert!(f.path.is_absolute());
        // A relative path is made absolute against the working directory,
        // which the test does not change, so use a name that exists nowhere.
        let rel = Path::new("m6-reopen-test-relative.ndjson");
        let g = ReopenableFile::open(rel).unwrap();
        assert!(g.path.is_absolute());
        drop(g);
        let _ = std::fs::remove_file(std::env::current_dir().unwrap().join(rel));
    }
}
