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
    match format {
        "json" => Box::new(
            fmt::layer()
                .json()
                .with_writer(writer)
                .with_current_span(true)
                .with_filter(filter),
        ),
        _ => Box::new(fmt::layer().with_writer(writer).with_filter(filter)),
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
