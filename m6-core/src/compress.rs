/// Brotli and gzip compression helpers.
use std::io::Write;

use anyhow::Context;
use brotli::{CompressorWriter, Decompressor};

/// Compress `data` with brotli at the given quality level (1-11).
pub fn brotli_compress(data: &[u8], quality: u32) -> anyhow::Result<Vec<u8>> {
    let quality = quality.min(11);
    let mut out = Vec::with_capacity(data.len() / 2 + 128);
    {
        let mut writer = CompressorWriter::new(
            &mut out,
            4096,    // buffer size
            quality,
            22,      // lgwin (window size)
        );
        writer.write_all(data).context("brotli compress write")?;
    }
    Ok(out)
}

/// Decompress brotli data.
pub fn brotli_decompress(data: &[u8]) -> anyhow::Result<Vec<u8>> {
    use std::io::Read;
    let mut out = Vec::new();
    let mut reader = Decompressor::new(data, 4096);
    reader.read_to_end(&mut out).context("brotli decompress")?;
    Ok(out)
}

/// Compress `data` with gzip at the given level (0-9).
pub fn gzip_compress(data: &[u8], level: u32) -> anyhow::Result<Vec<u8>> {
    let level = flate2::Compression::new(level.min(9));
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), level);
    encoder.write_all(data).context("gzip compress write")?;
    encoder.finish().context("gzip compress finish")
}

/// Decompress gzip data.
pub fn gzip_decompress(data: &[u8]) -> anyhow::Result<Vec<u8>> {
    use std::io::Read;
    let mut decoder = flate2::read::GzDecoder::new(data);
    let mut out = Vec::new();
    decoder.read_to_end(&mut out).context("gzip decompress")?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn brotli_round_trip() {
        let data = b"Hello, World! This is a test of brotli compression.";
        let compressed = brotli_compress(data, 6).unwrap();
        let decompressed = brotli_decompress(&compressed).unwrap();
        assert_eq!(decompressed, data);
    }

    #[test]
    fn gzip_round_trip() {
        let data = b"Hello, World! This is a test of gzip compression.";
        let compressed = gzip_compress(data, 6).unwrap();
        let decompressed = gzip_decompress(&compressed).unwrap();
        assert_eq!(decompressed, data);
    }
}
