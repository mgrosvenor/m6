//! Logging for every service, over the `m6-log` crate.
//!
//! `m6-log` owns the Sink, the JSON line format, the macros and the log
//! heartbeat. This module re-exports them as `m6_core::log`, reads the `[log]`
//! level from `site.toml` and the service config, and adds [`watch`], the one
//! piece that needs the reactor.
//!
//! Resolution order is fixed and shared: `site.toml`'s `[log]`, then the
//! service config's `[log]`, then `--log-level`. The level is read once, at
//! start, and a change takes a restart.
//!
//! **One stream.**
//! Every line goes to stdout, which journals under systemd. There is no second
//! file to rotate, because the journal rotates.

use std::io;
use std::path::Path;

pub use m6_log::pulse;
pub use m6_log::{
    debug, error, flush, info, init, set_limits, stop, tick, trace, warn, with_operational, Level,
    Line, LogLimits, LogPulse, Sink, Type,
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

// ── Reading a log file's tail ───────────────────────────────────────────────

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
/// `"info"`. The table itself is read by [`crate::config::LogConfig`], which
/// every service uses for the same table.
pub fn read_site_log_config(site_toml: &Path) -> anyhow::Result<String> {
    let text = std::fs::read_to_string(site_toml)
        .map_err(|e| anyhow::anyhow!("{} cannot be read: {e}", site_toml.display()))?;
    let val: toml::Value = text.parse::<toml::Value>().map_err(|e| {
        anyhow::anyhow!("{} is not valid TOML: {}", site_toml.display(), e.message())
    })?;
    let log = crate::config::LogConfig::from_toml(val.get("log"))?;
    Ok(log.level.unwrap_or_else(|| "info".to_string()))
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
