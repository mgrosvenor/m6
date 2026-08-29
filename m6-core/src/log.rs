/// Logging initialisation for m6 processes.

use anyhow::Result;
use std::path::Path;
use std::sync::Mutex;
use tracing::Level;
use tracing_appender::non_blocking::{NonBlocking, WorkerGuard};
use tracing_subscriber::filter::{filter_fn, FilterExt, LevelFilter};
use tracing_subscriber::prelude::*;
use tracing_subscriber::{fmt, reload, Layer, Registry};

type BoxedLayer = Box<dyn Layer<Registry> + Send + Sync + 'static>;

/// Events logged with `target: "analytics"` are routed only to the dedicated
/// analytics file (see [`init_with_analytics`]) — the main dev/prod log layer
/// excludes them so per-request traffic logging doesn't drown out ordinary
/// operational logging.
const ANALYTICS_TARGET: &str = "analytics";

/// Handle returned by [`init`] that allows runtime log level / format reloads.
///
/// Keep the handle alive for the lifetime of the process. Dropping it flushes
/// and terminates the logging background thread.
pub struct LogHandle {
    handle: reload::Handle<BoxedLayer, Registry>,
    guard:  Mutex<WorkerGuard>,
    // Kept alive for the process lifetime so the analytics writer thread
    // isn't torn down early; never read again after init.
    _analytics_guard: Option<WorkerGuard>,
}

impl LogHandle {
    /// Swap the active log layer for one built from `format` and `level`.
    ///
    /// On success the old `WorkerGuard` is replaced so that the previous
    /// non-blocking writer is flushed and the new one takes over.
    pub fn reload(&self, format: &str, level: &str) {
        let lvl = parse_level(level);
        let (writer, new_guard) = tracing_appender::non_blocking(std::io::stdout());
        let new_layer = make_layer(format, lvl, writer);
        match self.handle.modify(|l| *l = new_layer) {
            Ok(()) => {
                if let Ok(mut g) = self.guard.lock() {
                    *g = new_guard;
                }
            }
            Err(e) => {
                tracing::warn!("log reload failed: {}", e);
            }
        }
    }
}

/// The main layer always excludes `target: "analytics"` events — those are
/// high-volume, per-request, and machine-read; routing them here as well
/// would drown out ordinary operational logging (and double-write them when
/// [`init_with_analytics`] is in use).
fn make_layer(format: &str, level: Level, writer: NonBlocking) -> BoxedLayer {
    let filter = LevelFilter::from_level(level).and(filter_fn(|meta| meta.target() != ANALYTICS_TARGET));
    match format {
        "json" => Box::new(
            fmt::layer()
                .json()
                .with_writer(writer)
                .with_current_span(true)
                .with_filter(filter),
        ),
        _ => Box::new(
            fmt::layer()
                .with_writer(writer)
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
/// process. Call [`LogHandle::reload`] at any time to swap the log level or
/// format without restarting.
///
/// `format`:
///   - `"json"` → JSON output (production)
///   - anything else → human-readable text (development)
///
/// `level`: `"debug"`, `"info"`, `"warn"`, `"error"` (defaults to `"info"`)
pub fn init(format: &str, level: &str) -> Result<LogHandle> {
    let lvl = parse_level(level);
    let (writer, guard) = tracing_appender::non_blocking(std::io::stdout());
    let layer = make_layer(format, lvl, writer);
    let (reload_layer, handle) = reload::Layer::new(layer);
    Registry::default()
        .with(reload_layer)
        .try_init()
        .map_err(|e| anyhow::anyhow!("failed to install tracing subscriber: {}", e))?;
    Ok(LogHandle {
        handle,
        guard: Mutex::new(guard),
        _analytics_guard: None,
    })
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
    let layer = make_layer(format, lvl, writer);
    let (reload_layer, handle) = reload::Layer::new(layer);

    // Chaining multiple `.with(boxed_layer)` calls changes the subscriber
    // type at each step (S becomes `Layered<_, Registry>`), which a
    // `Box<dyn Layer<Registry>>` no longer satisfies. Collecting into a
    // `Vec<BoxedLayer>` and calling `.with()` once keeps every element's
    // trait object anchored to plain `Registry`.
    let mut layers: Vec<BoxedLayer> = vec![Box::new(reload_layer)];
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
        handle,
        guard: Mutex::new(guard),
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
