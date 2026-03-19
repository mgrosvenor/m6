/// Logging initialisation for m6 processes.

use anyhow::Result;
use std::path::Path;
use std::sync::Mutex;
use tracing::Level;
use tracing_appender::non_blocking::{NonBlocking, WorkerGuard};
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::prelude::*;
use tracing_subscriber::{fmt, reload, Layer, Registry};

type BoxedLayer = Box<dyn Layer<Registry> + Send + Sync + 'static>;

/// Handle returned by [`init`] that allows runtime log level / format reloads.
///
/// Keep the handle alive for the lifetime of the process. Dropping it flushes
/// and terminates the logging background thread.
pub struct LogHandle {
    handle: reload::Handle<BoxedLayer, Registry>,
    guard:  Mutex<WorkerGuard>,
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

fn make_layer(format: &str, level: Level, writer: NonBlocking) -> BoxedLayer {
    let filter = LevelFilter::from_level(level);
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
