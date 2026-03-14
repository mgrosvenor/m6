/// Logging initialisation for m6 processes.

use anyhow::Result;
use std::path::Path;
use tracing::Level;
use tracing_subscriber::fmt;

pub use tracing_appender::non_blocking::WorkerGuard;

/// Initialize the tracing subscriber with a non-blocking stdout writer.
///
/// Returns a `WorkerGuard` that must be kept alive for the lifetime of the
/// process. Dropping it flushes and terminates the logging background thread.
///
/// `format`:
///   - `"json"` → JSON output (production)
///   - anything else → human-readable text (development)
///
/// `level`: `"debug"`, `"info"`, `"warn"`, `"error"` (defaults to `"info"`)
pub fn init(format: &str, level: &str) -> Result<WorkerGuard> {
    let level = parse_level(level);
    let (non_blocking, guard) = tracing_appender::non_blocking(std::io::stdout());

    match format {
        "json" => {
            fmt()
                .json()
                .with_max_level(level)
                .with_writer(non_blocking)
                .with_current_span(true)
                .try_init()
                .map_err(|e| anyhow::anyhow!("failed to install tracing subscriber: {}", e))?;
        }
        _ => {
            fmt()
                .with_max_level(level)
                .with_writer(non_blocking)
                .try_init()
                .map_err(|e| anyhow::anyhow!("failed to install tracing subscriber: {}", e))?;
        }
    }

    Ok(guard)
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
