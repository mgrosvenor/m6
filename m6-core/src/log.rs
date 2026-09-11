/// Logging initialisation for m6 processes.

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

/// A layer that does nothing but count. Composed with the format layer
/// *inside* the reload-able filter, so both see the same events.
struct PulseLayer;

impl<S: tracing::Subscriber> Layer<S> for PulseLayer {
    fn on_event(&self, _event: &tracing::Event<'_>, _ctx: tracing_subscriber::layer::Context<'_, S>) {
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
    Box::new(LevelFilter::from_level(level).and(filter_fn(|meta| meta.target() != ANALYTICS_TARGET)))
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
    let file = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
    let (writer, guard) = tracing_appender::non_blocking(file);
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
/// opened in append mode and grown across restarts.
pub fn init_with_analytics(format: &str, level: &str, analytics_path: Option<&Path>) -> Result<LogHandle> {
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
