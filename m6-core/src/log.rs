/// Logging initialisation for m6 processes.

use anyhow::Result;
use tracing::Level;
use tracing_subscriber::fmt;

/// Initialize the tracing subscriber.
///
/// `format`:
///   - `"json"` → JSON output (production)
///   - anything else → human-readable text (development)
///
/// `level`: `"debug"`, `"info"`, `"warn"`, `"error"` (defaults to `"info"`)
pub fn init(format: &str, level: &str) -> Result<()> {
    let level = parse_level(level);

    match format {
        "json" => {
            fmt()
                .json()
                .with_max_level(level)
                .with_current_span(true)
                .try_init()
                .map_err(|e| anyhow::anyhow!("failed to install tracing subscriber: {}", e))?;
        }
        _ => {
            fmt()
                .with_max_level(level)
                .try_init()
                .map_err(|e| anyhow::anyhow!("failed to install tracing subscriber: {}", e))?;
        }
    }

    Ok(())
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
