use std::collections::HashMap;

use anyhow::Result;
use m6_core::config::CompressionLevel;

#[derive(Debug, Clone, PartialEq)]
pub enum Encoding {
    Brotli,
    Gzip,
    Identity,
}

/// The q-value a client assigned to one content-coding (RFC 9110 12.5.3).
///
/// Re-exported from `m6-core`, which is now the single implementation. It was
/// written here first, to replace `accept_encoding.contains("br")`; m6-render
/// carried the unfixed substring version for months afterwards because the
/// rules lived in this crate rather than a shared one. Moving it removed that
/// second copy. See `m6_core::negotiate` for the reasoning and the tests.
use m6_core::coding_quality;

/// Decide which encoding to use for a given MIME type and Accept-Encoding header.
///
/// Picks the highest-quality coding the client will actually accept, rather
/// than the first one whose name appears somewhere in the header. Ties go to
/// brotli then gzip, which is our own preference order and only consulted when
/// the client expressed none.
pub fn choose_encoding(
    mime_type: &str,
    accept_encoding: &str,
    compression: &HashMap<String, CompressionLevel>,
) -> (Encoding, Option<u32>) {
    let mime_base = mime_type.split(';').next().unwrap_or(mime_type).trim();

    // Which codings are permitted for this MIME type, and at what level.
    let (br_level, gz_level) = match compression.get(mime_base) {
        Some(settings) => (
            if settings.brotli > 0 { Some(settings.brotli) } else { None },
            if settings.gzip > 0 { Some(settings.gzip) } else { None },
        ),
        None if m6_core::should_compress_default(mime_base) => (Some(6), Some(6)),
        None => (None, None),
    };

    // Candidates we could produce, each with the client's stated quality.
    // Ordered br, gzip so a tie resolves to brotli.
    let mut best: Option<(Encoding, Option<u32>, f32)> = None;
    let candidates = [
        (Encoding::Brotli, br_level, "br"),
        (Encoding::Gzip, gz_level, "gzip"),
    ];
    for (enc, level, name) in candidates {
        let Some(level) = level else { continue };
        let Some(q) = coding_quality(accept_encoding, name) else { continue };
        if best.as_ref().is_none_or(|(_, _, bq)| q > *bq) {
            best = Some((enc, Some(level), q));
        }
    }

    match best {
        Some((enc, level, _)) => (enc, level),
        // Nothing compressible was acceptable. Falling back to identity is
        // right even when the client sent `identity;q=0`: refusing to serve a
        // representation at all (a 406) is a worse outcome than sending an
        // uncompressed one, and RFC 9110 12.5.3 explicitly permits ignoring
        // that case.
        None => (Encoding::Identity, None),
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
    use std::collections::HashMap;

    #[test]
    fn test_choose_encoding_default_compress() {
        let config: HashMap<String, CompressionLevel> = HashMap::new();
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
        let config: HashMap<String, CompressionLevel> = HashMap::new();
        let (enc, level) = choose_encoding("text/javascript", "br, gzip", &config);
        assert_eq!(enc, Encoding::Brotli);
        assert!(level.is_some());
    }

    #[test]
    fn test_choose_encoding_no_compress() {
        let config: HashMap<String, CompressionLevel> = HashMap::new();
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

#[cfg(test)]
mod q_value_tests {
    use super::*;
    use std::collections::HashMap;

    fn pick(ae: &str) -> Encoding {
        choose_encoding("text/css", ae, &HashMap::new()).0
    }

    /// The defect. `contains("br")` matched a client that had explicitly
    /// refused brotli, and the response went out brotli-encoded to something
    /// that said it could not accept it. RFC 9110 12.4.2: q=0 means NOT
    /// ACCEPTABLE, not "least preferred".
    #[test]
    fn q_zero_means_refused_not_deprioritised() {
        assert_eq!(pick("gzip, br;q=0"), Encoding::Gzip);
        assert_eq!(pick("br;q=0"), Encoding::Identity);
        assert_eq!(pick("br;q=0, gzip;q=0"), Encoding::Identity);
        assert_eq!(pick("br;q=0.000"), Encoding::Identity);
    }

    /// The other half: preference was ignored entirely, because `br` was simply
    /// tested first.
    #[test]
    fn highest_q_wins_not_first_match() {
        assert_eq!(pick("gzip;q=1.0, br;q=0.1"), Encoding::Gzip);
        assert_eq!(pick("gzip;q=0.1, br;q=1.0"), Encoding::Brotli);
        assert_eq!(pick("br;q=0.5, gzip;q=0.9"), Encoding::Gzip);
    }

    /// A bare token is q=1, and ties fall to our own preference order.
    #[test]
    fn defaults_and_ties() {
        assert_eq!(pick("gzip, br"), Encoding::Brotli);
        assert_eq!(pick("br, gzip"), Encoding::Brotli);
        assert_eq!(pick("gzip"), Encoding::Gzip);
        assert_eq!(pick("gzip;q=1, br;q=1"), Encoding::Brotli);
    }

    /// `*` supplies a q-value for anything not named, and an explicit mention
    /// overrides it in both directions.
    #[test]
    fn wildcard_handling() {
        assert_eq!(pick("*"), Encoding::Brotli);
        assert_eq!(pick("*;q=0"), Encoding::Identity);
        // Explicit beats the wildcard even when the wildcard refuses.
        assert_eq!(pick("*;q=0, gzip"), Encoding::Gzip);
        // ...and even when the wildcard allows: br is refused by name, so the
        // wildcard's q=1 applies to gzip and gzip wins.
        assert_eq!(pick("*, br;q=0"), Encoding::Gzip);
    }

    /// A real browser's header, and the empty/absent cases.
    #[test]
    fn realistic_and_edge_headers() {
        assert_eq!(pick("gzip, deflate, br, zstd"), Encoding::Brotli);
        assert_eq!(pick(""), Encoding::Identity);
        assert_eq!(pick("identity"), Encoding::Identity);
        // Whitespace and case must not matter.
        assert_eq!(pick("  GZIP ;  Q=0.9 ,  BR ; q=0.4 "), Encoding::Gzip);
        // A malformed q is ignored rather than fatal.
        assert_eq!(pick("br;q=notanumber"), Encoding::Brotli);
    }

    /// An uncompressible MIME type is identity regardless of what is offered.
    #[test]
    fn uncompressible_types_stay_identity() {
        let c: HashMap<String, CompressionLevel> = HashMap::new();
        assert_eq!(choose_encoding("image/png", "br, gzip", &c).0, Encoding::Identity);
    }
}
