//! Content-coding negotiation (RFC 9110 12.5.3), in one place.
//!
//! This lives in `m6-core` rather than in any one service because all three
//! consumers have to reach the same answer from the same header, and two of
//! them already disagreed:
//!
//! - `m6-file` had a correct q-value parser.
//! - `m6-render` had `ae_contains`, a raw substring match over the header.
//! - `m6-http` did not negotiate at all; it used the *raw header text* as the
//!   response cache key, so `gzip` and `gzip, deflate, br, zstd` were separate
//!   entries for one byte-identical response.
//!
//! Measured on production 2026-09-10, against the HTML path served by
//! `m6-render`:
//!
//! | Accept-Encoding        | correct  | served |
//! |------------------------|----------|--------|
//! | `gzip, br;q=0`         | gzip     | **br** |
//! | `notbr`                | identity | **br** |
//! | `gzip;q=1.0, br;q=0.1` | gzip     | **br** |
//!
//! The first row is the one that matters: `q=0` means "not acceptable"
//! (RFC 9110 12.4.2), so that response was brotli sent to a client that had
//! explicitly refused brotli. The second is `contains("br")` matching inside
//! an unrelated token. The third ignores preference entirely, because the
//! candidates were tested in our order and the first match won.
//!
//! One implementation, because the alternative is what the site already had:
//! the rules restated per crate, drifting apart, with only one copy ever
//! getting the fix.

/// The q-value a client assigned to one content-coding.
///
/// Returns `None` when the coding is not acceptable at all, otherwise its
/// quality in (0.0, 1.0].
///
/// Rules implemented here:
/// - a bare token defaults to `q=1`
/// - `q=0` means unacceptable, not "least preferred"
/// - `*` supplies the q-value for any coding not named explicitly
/// - an explicitly named coding always beats `*`, whichever way it goes
/// - `identity` is acceptable unless refused by name or by `*;q=0`
pub fn coding_quality(accept_encoding: &str, coding: &str) -> Option<f32> {
    let mut wildcard: Option<f32> = None;
    let mut explicit: Option<f32> = None;

    for part in accept_encoding.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let mut bits = part.split(';');
        let name = bits.next().unwrap_or("").trim();
        let mut q: f32 = 1.0;
        for param in bits {
            let param = param.trim();
            if let Some(v) = param.strip_prefix("q=").or_else(|| param.strip_prefix("Q=")) {
                // An unparseable q is treated as 1, per the general rule that a
                // malformed parameter is ignored rather than made fatal.
                q = v.trim().parse::<f32>().unwrap_or(1.0);
            }
        }
        if name == "*" {
            wildcard = Some(q);
        } else if name.eq_ignore_ascii_case(coding) {
            explicit = Some(q);
        }
    }

    let q = match (explicit, wildcard) {
        (Some(q), _) => q,
        (None, Some(q)) => q,
        // Not mentioned at all. identity is acceptable by default; a coding we
        // would have to apply is not.
        (None, None) => {
            if coding.eq_ignore_ascii_case("identity") {
                1.0
            } else {
                return None;
            }
        }
    };
    if q > 0.0 {
        Some(q)
    } else {
        None
    }
}

/// Pick the best coding the client will actually accept, from a
/// preference-ordered candidate list.
///
/// `candidates` is in *our* preference order, consulted only to break a tie
/// the client did not express. Returns `None` when nothing offered is
/// acceptable, which the caller should read as identity.
pub fn preferred_coding<'a>(accept_encoding: &str, candidates: &[&'a str]) -> Option<&'a str> {
    let mut best: Option<(&'a str, f32)> = None;
    for name in candidates {
        let Some(q) = coding_quality(accept_encoding, name) else {
            continue;
        };
        // Strictly greater, so an earlier candidate wins a tie and our own
        // order is the tiebreak.
        if best.as_ref().is_none_or(|(_, bq)| q > *bq) {
            best = Some((name, q));
        }
    }
    best.map(|(n, _)| n)
}

/// Canonicalise an `Accept-Encoding` header to the single coding that will
/// actually be served.
///
/// **Deliberately ahead of its caller.** Nothing uses this yet. It exists for
/// handover open item 11: `m6-http` keys the response cache on the raw
/// `Accept-Encoding` header text, so `gzip` and `gzip, deflate, br, zstd` are
/// separate entries for one byte-identical response. Narrowing the header to
/// one unambiguous token, both as the cache key and as what is sent upstream,
/// is the fix, and it keeps the key and the backend's own negotiation in
/// agreement by construction.
///
/// It is tested, so it is not untested dead weight, and it should not be
/// removed by a dead-code sweep before item 11 lands.
///
/// Returns `""` for identity. The result is suitable both as a cache-key
/// component and as the `Accept-Encoding` to send upstream: narrowing the
/// header to one unambiguous token means a backend cannot negotiate something
/// different from what the key promises, whatever parser that backend uses.
pub fn canonical_coding(accept_encoding: &str) -> &'static str {
    match preferred_coding(accept_encoding, &["br", "gzip"]) {
        Some("br") => "br",
        Some("gzip") => "gzip",
        _ => "",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn q_zero_means_unacceptable_not_least_preferred() {
        // The live defect: this served brotli to a client refusing brotli.
        assert_eq!(coding_quality("gzip, br;q=0", "br"), None);
        assert_eq!(canonical_coding("gzip, br;q=0"), "gzip");
    }

    #[test]
    fn a_coding_name_is_not_matched_as_a_substring() {
        // `contains("br")` matched inside `notbr`.
        assert_eq!(coding_quality("notbr", "br"), None);
        assert_eq!(canonical_coding("notbr"), "");
    }

    #[test]
    fn client_preference_beats_our_own_order() {
        // We prefer brotli, but the client asked for gzip more strongly.
        assert_eq!(canonical_coding("gzip;q=1.0, br;q=0.1"), "gzip");
        assert_eq!(canonical_coding("gzip;q=0.1, br;q=1.0"), "br");
    }

    #[test]
    fn our_order_breaks_a_tie_the_client_did_not_express() {
        assert_eq!(canonical_coding("gzip, br"), "br");
        assert_eq!(canonical_coding("br, gzip"), "br");
    }

    #[test]
    fn wildcard_supplies_a_default_and_an_explicit_name_overrides_it() {
        assert_eq!(coding_quality("*", "br"), Some(1.0));
        assert_eq!(coding_quality("*;q=0", "br"), None);
        // Explicit beats the wildcard in both directions.
        assert_eq!(coding_quality("*;q=0, br", "br"), Some(1.0));
        assert_eq!(coding_quality("*, br;q=0", "br"), None);
    }

    #[test]
    fn identity_is_acceptable_unless_refused() {
        assert_eq!(coding_quality("gzip", "identity"), Some(1.0));
        assert_eq!(coding_quality("identity;q=0", "identity"), None);
        assert_eq!(coding_quality("*;q=0", "identity"), None);
    }

    #[test]
    fn an_absent_or_empty_header_is_identity() {
        assert_eq!(canonical_coding(""), "");
        assert_eq!(coding_quality("", "br"), None);
        assert_eq!(coding_quality("", "identity"), Some(1.0));
    }

    /// The whole point of `canonical_coding` for the cache: every header a real
    /// browser sends that resolves to brotli must produce ONE key, not five.
    #[test]
    fn real_browser_headers_collapse_to_one_key() {
        for header in [
            "gzip, deflate, br, zstd", // Chrome, Edge, Firefox
            "gzip, deflate, br",       // Safari
            "br",
            "br;q=1.0, gzip;q=0.8, *;q=0.1",
            "identity;q=0.5, br",
        ] {
            assert_eq!(canonical_coding(header), "br", "header: {header}");
        }
        for header in ["gzip, deflate", "deflate, gzip", "gzip"] {
            assert_eq!(canonical_coding(header), "gzip", "header: {header}");
        }
    }

    #[test]
    fn whitespace_and_case_are_tolerated() {
        assert_eq!(canonical_coding("  GZIP ,  BR  "), "br");
        assert_eq!(coding_quality("GZIP;Q=0", "gzip"), None);
    }

    #[test]
    fn a_malformed_q_is_ignored_rather_than_fatal() {
        assert_eq!(coding_quality("br;q=banana", "br"), Some(1.0));
    }
}
