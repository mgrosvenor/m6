use crate::config::Config;
use anyhow::Result;

#[derive(Debug, Clone, PartialEq)]
pub enum Encoding {
    Brotli,
    Gzip,
    Identity,
}

/// Decide which encoding to use for a given MIME type and Accept-Encoding header.
pub fn choose_encoding(
    mime_type: &str,
    accept_encoding: &str,
    config: &Config,
) -> (Encoding, Option<u32>) {
    let mime_base = mime_type.split(';').next().unwrap_or(mime_type).trim();

    // Check config for explicit settings
    if let Some(settings) = config.compression.get(mime_base) {
        // Level 0 means no compression
        if settings.brotli > 0 && accept_encoding.contains("br") {
            return (Encoding::Brotli, Some(settings.brotli));
        }
        if settings.gzip > 0 && accept_encoding.contains("gzip") {
            return (Encoding::Gzip, Some(settings.gzip));
        }
        // Level 0 for both, or no matching encoding
        return (Encoding::Identity, None);
    }

    // Use defaults — the shared, already-tested MIME table in m6-core, not a
    // locally maintained list that can drift out of sync with it.
    if !m6_core::should_compress_default(mime_base) {
        return (Encoding::Identity, None);
    }

    if accept_encoding.contains("br") {
        (Encoding::Brotli, Some(6))
    } else if accept_encoding.contains("gzip") {
        (Encoding::Gzip, Some(6))
    } else {
        (Encoding::Identity, None)
    }
}

/// Compress data with brotli.
pub fn compress_brotli(data: &[u8], level: u32) -> Result<Vec<u8>> {
    m6_core::compress::brotli_compress(data, level)
}

/// Compress data with gzip.
pub fn compress_gzip(data: &[u8], level: u32) -> Result<Vec<u8>> {
    m6_core::compress::gzip_compress(data, level)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    #[test]
    fn test_choose_encoding_default_compress() {
        let config = Config::default();
        let (enc, level) = choose_encoding("text/css", "br, gzip", &config);
        assert_eq!(enc, Encoding::Brotli);
        assert!(level.is_some());
    }

    #[test]
    fn test_choose_encoding_js_mime_guess_string() {
        // Regression: mime_guess resolves .js to "text/javascript" (RFC
        // 9239), not "application/javascript" — this is the string that
        // actually reaches choose_encoding from a real request, so it's the
        // one that must match, not the deprecated form alone.
        let config = Config::default();
        let (enc, level) = choose_encoding("text/javascript", "br, gzip", &config);
        assert_eq!(enc, Encoding::Brotli);
        assert!(level.is_some());
    }

    #[test]
    fn test_choose_encoding_no_compress() {
        let config = Config::default();
        let (enc, _) = choose_encoding("image/jpeg", "br, gzip", &config);
        assert_eq!(enc, Encoding::Identity);
    }

    #[test]
    fn test_brotli_roundtrip() {
        let data = b"hello world, this is a test of compression";
        let compressed = compress_brotli(data, 6).unwrap();
        // Just ensure it compresses without error
        assert!(!compressed.is_empty());
    }

    #[test]
    fn test_gzip_roundtrip() {
        let data = b"hello world, this is a test of compression";
        let compressed = compress_gzip(data, 6).unwrap();
        assert!(!compressed.is_empty());
    }
}
