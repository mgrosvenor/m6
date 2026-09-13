//! Brotli and gzip compression.
//!
//! Compression is applied after minification, and only to types the config
//! names. Which coding to use is not decided here: that is `negotiate`, which
//! reads the client's `Accept-Encoding` q-values. Keeping the two apart is
//! deliberate, because deciding and doing were once the same function in three
//! places and they disagreed.

use std::io::Write;

use anyhow::Context;
use brotli::{CompressorWriter, Decompressor};

/// Compress `data` with brotli at the given quality level (1-11).
pub fn brotli_compress(data: &[u8], quality: u32) -> anyhow::Result<Vec<u8>> {
    let quality = quality.min(11);
    let mut out = Vec::with_capacity(data.len() / 2 + 128);
    {
        let mut writer = CompressorWriter::new(
            &mut out, 4096, // buffer size
            quality, 22, // lgwin (window size)
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

// ── Agreeing with the edge about who compresses ──────────────────────────────

/// Check that `site.toml` tells the edge the truth about this service.
///
/// **m6-http is a cache, not a transformer.** It does not compress: it
/// negotiates between representations a backend produced and caches each one.
/// The compressors are here, in m6-core, on the backend side.
///
/// That was not written down anywhere, and two normative documents said the
/// opposite — `m6-backend-protocol.md` §3.6 told backends not to compress
/// because "the proxy performs content negotiation and compression itself". A
/// backend written from that advice serves uncompressed bytes forever, and it
/// was invisible for the Rust services because core compresses for them. See
/// `docs/m6-backend-examples.md` §10.5.
///
/// So the two sides now agree in config: `[[backend]] compresses = <bool>` in
/// `site.toml`. The edge reads it to decide whether to advertise an encoding
/// dimension; this function is the backend reading the SAME key and refusing to
/// start if it disagrees with what the service can actually do.
///
/// Returns `Err` with the operator-facing message. Both directions are faults:
///
/// - **declared true, compresses nothing**: the edge advertises variants that
///   will never exist, so every shared cache downstream fragments its storage
///   on a header that cannot change the body.
/// - **declared false, compresses**: the worse one. The edge stops varying on
///   `Accept-Encoding` while the backend still returns brotli, so one client's
///   compressed body can be stored and replayed to a client that asked for
///   identity and cannot read it.
///
/// A service with no entry in `site.toml` at all is not an error: it may be
/// reached some other way, or be under test.
pub fn check_declared_support(
    site_dir: &std::path::Path,
    backend_name: &str,
    compresses: bool,
) -> Result<(), String> {
    let declared = match declared_compresses(site_dir, backend_name) {
        Some(d) => d,
        None => return Ok(()),
    };
    if declared == compresses {
        return Ok(());
    }
    if declared {
        Err(format!(
            "site.toml declares `[[backend]] name = \"{backend_name}\"` with \
             compresses = true, but this service compresses nothing. The edge \
             will advertise `Vary: Accept-Encoding` for representations that \
             will never exist, and every shared cache downstream fragments on a \
             header that cannot change the body. Set compresses = false, or \
             configure [compression] in this service's own config."
        ))
    } else {
        Err(format!(
            "site.toml declares `[[backend]] name = \"{backend_name}\"` with \
             compresses = false, but this service DOES compress. The edge will \
             not vary on `Accept-Encoding`, so a compressed body can be cached \
             and replayed to a client that asked for identity and cannot read \
             it. Set compresses = true, or turn [compression] off here."
        ))
    }
}

/// Read `[[backend]] compresses` for `backend_name` from `site.toml`.
///
/// `None` when there is no `site.toml`, no such backend, or the file does not
/// parse: this function reports what the file says about this service, and
/// "nothing" is a legitimate answer. Whether the file must exist is the
/// caller's business.
fn declared_compresses(site_dir: &std::path::Path, backend_name: &str) -> Option<bool> {
    let text = std::fs::read_to_string(site_dir.join("site.toml")).ok()?;
    let val: toml::Value = text.parse().ok()?;
    let entry = val
        .get("backend")?
        .as_array()?
        .iter()
        .find(|b| b.get("name").and_then(|n| n.as_str()) == Some(backend_name))?;
    // Absent means true, matching the edge's own default: every backend in this
    // fleet is built on core, which compresses unless told not to.
    Some(
        entry
            .get("compresses")
            .and_then(|c| c.as_bool())
            .unwrap_or(true),
    )
}

#[cfg(test)]
mod declared_support_tests {
    use super::*;

    fn site_with(body: &str) -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("site.toml"), body).unwrap();
        d
    }

    #[test]
    fn agreement_is_silent_in_both_directions() {
        let d = site_with("[[backend]]\nname = \"r\"\ncompresses = true\n");
        assert!(check_declared_support(d.path(), "r", true).is_ok());
        let d = site_with("[[backend]]\nname = \"r\"\ncompresses = false\n");
        assert!(check_declared_support(d.path(), "r", false).is_ok());
    }

    /// The dangerous direction: the edge stops varying, the backend keeps
    /// compressing, and a shared cache serves brotli to a client that asked for
    /// identity.
    #[test]
    fn declared_false_while_compressing_is_refused() {
        let d = site_with("[[backend]]\nname = \"r\"\ncompresses = false\n");
        let e = check_declared_support(d.path(), "r", true).expect_err("must refuse");
        assert!(e.contains("DOES compress"), "{e}");
        assert!(e.contains("asked for identity"), "{e}");
    }

    #[test]
    fn declared_true_while_not_compressing_is_refused() {
        let d = site_with("[[backend]]\nname = \"r\"\ncompresses = true\n");
        let e = check_declared_support(d.path(), "r", false).expect_err("must refuse");
        assert!(e.contains("compresses nothing"), "{e}");
    }

    /// Absent means true, the same default the edge applies, so a site.toml
    /// written before this key existed keeps working and keeps agreeing.
    #[test]
    fn an_absent_key_means_true() {
        let d = site_with("[[backend]]\nname = \"r\"\nsockets = \"/run/m6/r.sock\"\n");
        assert!(check_declared_support(d.path(), "r", true).is_ok());
        assert!(check_declared_support(d.path(), "r", false).is_err());
    }

    /// A service the file says nothing about is not a fault. It may be reached
    /// another way, or be under test.
    #[test]
    fn a_backend_absent_from_site_toml_is_not_a_fault() {
        let d = site_with("[[backend]]\nname = \"other\"\ncompresses = true\n");
        assert!(check_declared_support(d.path(), "r", true).is_ok());
        assert!(check_declared_support(d.path(), "r", false).is_ok());
    }

    #[test]
    fn no_site_toml_at_all_is_not_a_fault() {
        let d = tempfile::tempdir().unwrap();
        assert!(check_declared_support(d.path(), "r", false).is_ok());
    }
}
