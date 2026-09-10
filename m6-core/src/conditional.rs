//! Conditional requests and preconditions: RFC 9110 13.2.
//!
//! Version-independent semantics, so they belong here rather than in the edge.
//! They were implemented twice. `m6-http` had this, complete and tested;
//! `m6-file` had five inline lines in a response handler that did strong
//! comparison for `If-None-Match` and implemented neither `If-Match` nor
//! `If-Unmodified-Since`.
//!
//! That divergence was live and measurable. Against `m6-file` directly:
//!
//! | step | `m6-file` before this move | required |
//! |---|---|---|
//! | `If-Match: "nope"` | 200 | 412 |
//! | `If-Unmodified-Since: <past>` | 200 | 412 |
//! | `If-None-Match: W/"<etag>"` | 200 | 304 |
//!
//! On the wire the symptom came and went, because `m6-http` answers a cache
//! hit with the correct implementation and only a miss reaches `m6-file`. A
//! defect that correlates with cache state reads as noise, which is why it
//! survived.

use crate::http::{header, HeaderSource};

/// The outcome of evaluating a request's preconditions (RFC 9110 13.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Precondition {
    /// No precondition applied, or all of them passed. Serve normally.
    Proceed,
    /// The client's cached copy is still current: 304, no body.
    NotModified,
    /// A precondition the client asserted is false: 412, and the request must
    /// NOT be applied. This is the case the old `bool` return could not
    /// express, so `If-Match` and `If-Unmodified-Since` were simply ignored.
    Failed,
}

/// Compare two entity-tags with the **weak** comparison function
/// (RFC 9110 8.8.3.2).
///
/// Weak comparison ignores the `W/` prefix on either side: `W/"abc"` and
/// `"abc"` are the same entity-tag for this purpose. The previous code did a
/// byte-for-byte `==`, which is *strong* comparison, so a client returning the
/// weak validator it had been given never matched and was sent the whole body
/// again. `If-None-Match` and `If-Modified-Since` both require weak comparison;
/// only `If-Match` and `If-Unmodified-Since` use strong.
fn etag_weak_eq(a: &str, b: &str) -> bool {
    let strip = |t: &str| {
        let t = t.trim();
        t.strip_prefix("W/").unwrap_or(t).to_string()
    };
    strip(a) == strip(b)
}

/// Strong comparison (RFC 9110 8.8.3.2): the tags must be byte-identical *and*
/// neither may be weak. A weak validator says "semantically equivalent", which
/// is not a strong enough claim to authorise overwriting a resource, so
/// `If-Match` must reject it.
fn etag_strong_eq(a: &str, b: &str) -> bool {
    let a = a.trim();
    let b = b.trim();
    !a.starts_with("W/") && !b.starts_with("W/") && a == b
}

/// Does a comma-separated `If-*-Match` list contain this tag?
///
/// Split on commas rather than parsed properly: an entity-tag is a quoted
/// string and may itself contain a comma. Real clients do not send such tags
/// -- ours are hex digests -- and the failure mode of a mis-split is a missed
/// match (a full 200 instead of a 304), never a wrongly authorised write.
fn etag_list_contains(list: &str, tag: &str, strong: bool) -> bool {
    list.split(',').any(|candidate| {
        if strong { etag_strong_eq(candidate, tag) } else { etag_weak_eq(candidate, tag) }
    })
}

/// Evaluate the request preconditions against a stored response.
///
/// **Order matters and is fixed by RFC 9110 13.2.2.** The old implementation
/// checked only `If-None-Match` and `If-Modified-Since`, which happened to be
/// steps 3 and 4; steps 1 and 2 did not exist, so a client could send
/// `If-Match: "stale"` on an unsafe request and have it applied anyway. That
/// is the whole point of `If-Match`: it is how a client avoids the lost-update
/// problem, and silently ignoring it turns a safe conditional write into an
/// unconditional one.
///
/// 1. `If-Match` -- if it fails, 412.
/// 2. `If-Unmodified-Since` -- only when `If-Match` is absent; if it fails, 412.
/// 3. `If-None-Match` -- if it *matches*, 304 for GET/HEAD, otherwise 412.
/// 4. `If-Modified-Since` -- only when `If-None-Match` is absent, and only for
///    GET/HEAD; if the resource is unchanged, 304.
///
/// `If-Range` is not implemented because ranges are not served; a 206 is
/// refused storage upstream (F057).
pub fn evaluate_preconditions(
    cached_headers: &[(String, String)],
    req_headers: &(impl HeaderSource + ?Sized),
    method: &str,
) -> Precondition {
    let find = |name: &str| {
        cached_headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    };
    let etag = find("etag");
    let last_modified = find("last-modified");
    let is_get_or_head = method.eq_ignore_ascii_case("GET") || method.eq_ignore_ascii_case("HEAD");

    let lm_secs = || {
        last_modified
            .and_then(|lm| httpdate::parse_http_date(lm).ok())
            .map(|t| t.duration_since(std::time::SystemTime::UNIX_EPOCH).unwrap_or_default().as_secs())
    };
    let hdr_secs = |v: &str| {
        httpdate::parse_http_date(v)
            .ok()
            .map(|t| t.duration_since(std::time::SystemTime::UNIX_EPOCH).unwrap_or_default().as_secs())
    };

    // 1. If-Match — strong comparison.
    if let Some(im) = header(req_headers, "if-match") {
        let im = im.trim();
        let ok = if im == "*" {
            etag.is_some() || last_modified.is_some() // the resource exists
        } else {
            etag.is_some_and(|e| etag_list_contains(im, e, true))
        };
        if !ok {
            return Precondition::Failed;
        }
    } else if let Some(ius) = header(req_headers, "if-unmodified-since") {
        // 2. Only consulted when If-Match is absent.
        match (hdr_secs(ius), lm_secs()) {
            (Some(req_t), Some(lm_t)) if lm_t > req_t => return Precondition::Failed,
            // An unparseable date must be ignored, not treated as a failure
            // (RFC 9110 13.1.4) -- otherwise a malformed header turns every
            // request into a 412.
            _ => {}
        }
    }

    // 3. If-None-Match — weak comparison.
    if let Some(inm) = header(req_headers, "if-none-match") {
        let inm = inm.trim();
        let matched = if inm == "*" {
            etag.is_some() || last_modified.is_some()
        } else {
            etag.is_some_and(|e| etag_list_contains(inm, e, false))
        };
        return if matched {
            // A match means the client already has it. Safe methods get 304;
            // anything else is a failed precondition on a state change.
            if is_get_or_head { Precondition::NotModified } else { Precondition::Failed }
        } else {
            Precondition::Proceed
        };
    }

    // 4. If-Modified-Since — GET/HEAD only, and only when If-None-Match absent.
    if is_get_or_head {
        if let Some(ims) = header(req_headers, "if-modified-since") {
            // HTTP-date has 1-second resolution; compare at that resolution so
            // sub-second mtime noise does not look like a modification.
            if let (Some(req_t), Some(lm_t)) = (hdr_secs(ims), lm_secs()) {
                if lm_t <= req_t {
                    return Precondition::NotModified;
                }
            }
        }
    }

    Precondition::Proceed
}

/// Back-compatible wrapper: true only when the answer is 304.
///
/// Retained so existing call sites keep compiling, but it **cannot express
/// 412** — a caller using this silently ignores `If-Match` and
/// `If-Unmodified-Since` failures. New code should call
/// [`evaluate_preconditions`] and handle all three outcomes.
pub fn is_not_modified(
    cached_headers: &[(String, String)],
    req_headers: &(impl HeaderSource + ?Sized),
) -> bool {
    evaluate_preconditions(cached_headers, req_headers, "GET") == Precondition::NotModified
}

/// Build the minimal header set for a 304 response derived from a cached
/// entry's headers — just the validators a client needs to keep using its
/// cached copy, not the full header set (no Content-Type/Content-Encoding
/// on a bodyless response).
pub fn not_modified_headers(cached_headers: &[(String, String)]) -> Vec<(String, String)> {
    // RFC 9110 15.4.5: a 304 carries the metadata a 200 would have sent, so a
    // client can update its stored response from it.
    //
    // This kept only ETag, Last-Modified and Cache-Control. `Vary` in
    // particular was dropped, which is the damaging one: a client or shared
    // cache updating its stored entry from this 304 would lose the knowledge
    // that the response varies by `Accept-Encoding` and could then reuse a
    // brotli body for a gzip-only request. `Date` was missing too, so the
    // recipient had nothing to compute age from.
    //
    // Content-Length is deliberately NOT carried: RFC 9110 8.6 allows it on a
    // 304 only when it equals the 200's length, and getting that wrong is
    // worse than omitting it. The serialisers set framing for a bodyless
    // response themselves.
    cached_headers.iter()
        .filter(|(k, _)| {
            let k = k.to_ascii_lowercase();
            k == "etag"
                || k == "last-modified"
                || k == "cache-control"
                || k == "vary"
                || k == "date"
                || k == "expires"
                || k == "content-location"
        })
        .cloned()
        .collect()
}

#[cfg(test)]
mod precondition_tests {
    use super::{evaluate_preconditions, Precondition};

    fn stored() -> Vec<(String, String)> {
        vec![
            ("ETag".to_string(), "\"abc123\"".to_string()),
            ("Last-Modified".to_string(), "Thu, 03 Sep 2026 10:00:00 GMT".to_string()),
        ]
    }
    fn req(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }
    fn eval(h: &[(&str, &str)], method: &str) -> Precondition {
        evaluate_preconditions(&stored(), &req(h)[..], method)
    }

    /// F003. `If-None-Match` uses the WEAK comparison function (RFC 9110
    /// 8.8.3.2), so `W/"abc123"` matches `"abc123"`. The old code compared
    /// byte-for-byte, so a client returning the weak validator it was given was
    /// sent the entire body again.
    #[test]
    fn if_none_match_uses_weak_comparison() {
        for tag in ["\"abc123\"", "W/\"abc123\""] {
            assert_eq!(
                eval(&[("if-none-match", tag)], "GET"),
                Precondition::NotModified,
                "{tag} should match weakly"
            );
        }
    }

    #[test]
    fn if_none_match_list_and_wildcard() {
        assert_eq!(eval(&[("if-none-match", "\"x\", \"abc123\", \"y\"")], "GET"), Precondition::NotModified);
        assert_eq!(eval(&[("if-none-match", "*")], "GET"), Precondition::NotModified);
        assert_eq!(eval(&[("if-none-match", "\"nope\"")], "GET"), Precondition::Proceed);
    }

    /// On an unsafe method a matching If-None-Match is a failed precondition,
    /// not a 304 — 304 is meaningless as a response to a state change.
    #[test]
    fn if_none_match_on_unsafe_method_is_412() {
        assert_eq!(eval(&[("if-none-match", "*")], "POST"), Precondition::Failed);
        assert_eq!(eval(&[("if-none-match", "\"abc123\"")], "PUT"), Precondition::Failed);
    }

    /// F006. If-Match was ignored entirely, so a conditional write guarding
    /// against a lost update was applied unconditionally.
    #[test]
    fn if_match_is_enforced_with_strong_comparison() {
        assert_eq!(eval(&[("if-match", "\"abc123\"")], "PUT"), Precondition::Proceed);
        assert_eq!(eval(&[("if-match", "*")], "PUT"), Precondition::Proceed);
        assert_eq!(eval(&[("if-match", "\"stale\"")], "PUT"), Precondition::Failed);
        // Strong comparison: a weak tag must NOT authorise a write, even though
        // it names the same entity.
        assert_eq!(
            eval(&[("if-match", "W/\"abc123\"")], "PUT"),
            Precondition::Failed,
            "a weak validator is not a strong enough claim to authorise a write"
        );
    }

    #[test]
    fn if_unmodified_since_is_enforced() {
        // Resource modified 03 Sep; client believes it is unchanged since 01 Sep.
        assert_eq!(
            eval(&[("if-unmodified-since", "Tue, 01 Sep 2026 10:00:00 GMT")], "PUT"),
            Precondition::Failed
        );
        assert_eq!(
            eval(&[("if-unmodified-since", "Sat, 05 Sep 2026 10:00:00 GMT")], "PUT"),
            Precondition::Proceed
        );
    }

    /// F008/F010. If-Match is step 1 and If-Unmodified-Since step 2, so a
    /// present If-Match makes If-Unmodified-Since irrelevant even when the
    /// latter would have failed.
    #[test]
    fn if_match_takes_precedence_over_if_unmodified_since() {
        assert_eq!(
            eval(
                &[
                    ("if-match", "\"abc123\""),                              // passes
                    ("if-unmodified-since", "Tue, 01 Sep 2026 10:00:00 GMT"), // would fail
                ],
                "PUT"
            ),
            Precondition::Proceed
        );
    }

    /// And a failed step 1 short-circuits before step 3 can return 304.
    #[test]
    fn failed_if_match_beats_if_none_match() {
        assert_eq!(
            eval(&[("if-match", "\"stale\""), ("if-none-match", "\"abc123\"")], "GET"),
            Precondition::Failed
        );
    }

    /// If-None-Match is step 3 and If-Modified-Since step 4, so a
    /// non-matching If-None-Match means "proceed" even if IMS would say 304.
    #[test]
    fn if_none_match_takes_precedence_over_if_modified_since() {
        assert_eq!(
            eval(
                &[
                    ("if-none-match", "\"different\""),                       // no match -> proceed
                    ("if-modified-since", "Sat, 05 Sep 2026 10:00:00 GMT"),   // would say 304
                ],
                "GET"
            ),
            Precondition::Proceed
        );
    }

    #[test]
    fn if_modified_since_still_works_alone() {
        assert_eq!(
            eval(&[("if-modified-since", "Sat, 05 Sep 2026 10:00:00 GMT")], "GET"),
            Precondition::NotModified
        );
        assert_eq!(
            eval(&[("if-modified-since", "Tue, 01 Sep 2026 10:00:00 GMT")], "GET"),
            Precondition::Proceed
        );
    }

    /// RFC 9110 13.1.4: an unparseable date must be ignored, not treated as a
    /// failure. Otherwise a malformed header turns every request into a 412.
    #[test]
    fn unparseable_dates_are_ignored_not_failed() {
        assert_eq!(eval(&[("if-unmodified-since", "not a date")], "PUT"), Precondition::Proceed);
        assert_eq!(eval(&[("if-modified-since", "not a date")], "GET"), Precondition::Proceed);
    }

    #[test]
    fn no_preconditions_proceeds() {
        assert_eq!(eval(&[], "GET"), Precondition::Proceed);
        assert_eq!(eval(&[("accept", "text/html")], "GET"), Precondition::Proceed);
    }
}
