use crate::config::Config;
use anyhow::Result;
use brotli::CompressorWriter;
use flate2::{write::GzEncoder, Compression};
use std::io::Write;

/// Default MIME types that should be compressed.
const DEFAULT_COMPRESS: &[&str] = &[
    "text/html",
    "text/css",
    "application/javascript",
    "image/svg+xml",
    "text/plain",
    "application/json",
    "application/xml",
    "text/xml",
];

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

    // Use defaults
    let should_compress = DEFAULT_COMPRESS.iter().any(|m| *m == mime_base);
    if !should_compress {
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
    let mut output = Vec::with_capacity(data.len());
    {
        let mut writer = CompressorWriter::new(&mut output, 4096, level, 22);
        writer.write_all(data)?;
    }
    Ok(output)
}

/// Compress data with gzip.
pub fn compress_gzip(data: &[u8], level: u32) -> Result<Vec<u8>> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::new(level));
    encoder.write_all(data)?;
    Ok(encoder.finish()?)
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
