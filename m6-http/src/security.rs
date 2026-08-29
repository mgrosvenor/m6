//! Security response headers, applied centrally at response serialisation.
//!
//! These are set at the edge rather than by each renderer so that *every*
//! response carries them — cache hits, backend responses, internally generated
//! error pages, and rate-limit rejections alike. A renderer that sets its own
//! value for one of these headers wins; we only fill in what is absent.
//!
//! The resolved set is stored in a process-global because the serialisation
//! points (`http11::build_response`, `http2::encode_response_headers`,
//! `send_h3_response`) sit below the layer that owns `Config`, and threading
//! config through three protocol stacks to deliver five constant strings would
//! be a poor trade. `configure()` is called once at startup and again on each
//! `site.toml` reload.

use std::sync::RwLock;

/// Resolved headers, plus the pre-rendered HTTP/1.1 block.
///
/// Pre-rendering matters: this is emitted on every single response, and in the
/// overwhelmingly common case (no backend override) writing it is a single
/// `memcpy` rather than five formatted writes.
#[derive(Default)]
struct Resolved {
    pairs: Vec<(String, String)>,
    /// `"name: value\r\n"` for every pair, concatenated.
    h1_block: String,
}

static HEADERS: RwLock<Resolved> = RwLock::new(Resolved {
    pairs: Vec::new(),
    h1_block: String::new(),
});

/// Install the header set derived from `[security]`. Called at startup and on
/// config reload.
pub fn configure(cfg: &crate::config::SecurityConfig) {
    let pairs = cfg.resolved_headers();
    let mut h1_block = String::new();
    for (k, v) in &pairs {
        h1_block.push_str(k);
        h1_block.push_str(": ");
        h1_block.push_str(v);
        h1_block.push_str("\r\n");
    }
    if let Ok(mut guard) = HEADERS.write() {
        *guard = Resolved { pairs, h1_block };
    }
}

/// True if `existing` sets any header we would otherwise add.
///
/// Typically false, which is what makes the `memcpy` fast path in
/// [`write_h1_headers`] worthwhile. Costs `existing.len() * pairs.len()` short
/// case-insensitive comparisons with no allocation.
fn any_overridden(existing: &[(String, String)], pairs: &[(String, String)]) -> bool {
    existing
        .iter()
        .any(|(k, _)| pairs.iter().any(|(name, _)| k.eq_ignore_ascii_case(name)))
}

/// Write the security headers absent from `existing` into an HTTP/1.1 header
/// block, as `name: value\r\n` lines. Allocation-free.
pub fn write_h1_headers(out: &mut Vec<u8>, existing: &[(String, String)]) {
    let Ok(guard) = HEADERS.read() else { return };
    if !any_overridden(existing, &guard.pairs) {
        out.extend_from_slice(guard.h1_block.as_bytes());
        return;
    }
    for (name, value) in &guard.pairs {
        if existing.iter().any(|(k, _)| k.eq_ignore_ascii_case(name)) {
            continue;
        }
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(b": ");
        out.extend_from_slice(value.as_bytes());
        out.extend_from_slice(b"\r\n");
    }
}

/// A read guard over the configured headers.
///
/// Callers that build a borrowed header list (HTTP/2 HPACK, HTTP/3) hold this
/// across the encode so the `&str`s stay valid without any string copies.
pub struct HeadersGuard(std::sync::RwLockReadGuard<'static, Resolved>);

impl HeadersGuard {
    pub fn pairs(&self) -> &[(String, String)] {
        &self.0.pairs
    }

    /// The configured headers not already present in `existing`.
    pub fn absent_from<'a>(
        &'a self,
        existing: &'a [(String, String)],
    ) -> impl Iterator<Item = (&'a str, &'a str)> + 'a {
        self.0.pairs.iter().filter_map(move |(name, value)| {
            let overridden = existing.iter().any(|(k, _)| k.eq_ignore_ascii_case(name));
            (!overridden).then_some((name.as_str(), value.as_str()))
        })
    }
}

/// Acquire a read guard over the configured security headers.
pub fn read() -> Option<HeadersGuard> {
    HEADERS.read().ok().map(HeadersGuard)
}

/// Append the configured security headers to `headers`, skipping any the
/// response already sets. Allocates; prefer [`write_h1_headers`] or
/// [`with_absent_headers`] on hot paths.
pub fn apply(headers: &mut Vec<(String, String)>) {
    let Some(guard) = read() else { return };
    let extra: Vec<(String, String)> = guard
        .absent_from(headers)
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    drop(guard);
    headers.extend(extra);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SecurityConfig;

    /// `HEADERS` is process-global, so tests that call `configure` must not run
    /// concurrently with each other.
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn defaults_are_applied_to_a_bare_response() {
        let _g = LOCK.lock().unwrap();
        configure(&SecurityConfig::default());

        let mut headers = vec![("content-type".to_string(), "text/html".to_string())];
        apply(&mut headers);

        for expected in [
            "strict-transport-security",
            "x-content-type-options",
            "x-frame-options",
            "referrer-policy",
            "content-security-policy",
        ] {
            assert!(
                headers.iter().any(|(k, _)| k.eq_ignore_ascii_case(expected)),
                "missing {expected} in {headers:?}"
            );
        }
    }

    #[test]
    fn backend_value_is_not_overridden() {
        let _g = LOCK.lock().unwrap();
        configure(&SecurityConfig::default());

        let mut headers = vec![(
            "X-Frame-Options".to_string(),
            "SAMEORIGIN".to_string(),
        )];
        apply(&mut headers);

        let values: Vec<&str> = headers
            .iter()
            .filter(|(k, _)| k.eq_ignore_ascii_case("x-frame-options"))
            .map(|(_, v)| v.as_str())
            .collect();
        assert_eq!(values, vec!["SAMEORIGIN"], "backend's value must win, exactly once");
    }

    #[test]
    fn report_only_mode_sends_report_only_header_name_with_same_policy() {
        let _g = LOCK.lock().unwrap();
        let cfg = SecurityConfig { csp_mode: crate::config::CspMode::ReportOnly, ..SecurityConfig::default() };
        let policy = cfg.content_security_policy.clone();
        configure(&cfg);

        let mut headers = Vec::new();
        apply(&mut headers);

        assert!(
            !headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("content-security-policy")),
            "report-only mode must not send the enforcing header name"
        );
        let report_only = headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("content-security-policy-report-only"));
        assert_eq!(
            report_only.map(|(_, v)| v.as_str()),
            Some(policy.as_str()),
            "report-only header must carry the same policy string"
        );
    }

    #[test]
    fn off_mode_omits_csp_header_entirely_regardless_of_policy_string() {
        let _g = LOCK.lock().unwrap();
        let cfg = SecurityConfig { csp_mode: crate::config::CspMode::Off, ..SecurityConfig::default() };
        configure(&cfg);

        let mut headers = Vec::new();
        apply(&mut headers);

        assert!(
            !headers.iter().any(|(k, _)| k.to_ascii_lowercase().starts_with("content-security-policy")),
            "off mode must send neither the enforcing nor report-only CSP header"
        );
        assert!(
            headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("x-frame-options")),
            "off mode must only affect CSP, not the other security headers"
        );
    }

    #[test]
    fn empty_config_value_omits_the_header() {
        let _g = LOCK.lock().unwrap();
        let cfg = SecurityConfig {
            content_security_policy: String::new(),
            ..SecurityConfig::default()
        };
        configure(&cfg);

        let mut headers = Vec::new();
        apply(&mut headers);

        assert!(
            !headers
                .iter()
                .any(|(k, _)| k.eq_ignore_ascii_case("content-security-policy")),
            "an empty config value should omit the header entirely"
        );
        assert!(
            headers
                .iter()
                .any(|(k, _)| k.eq_ignore_ascii_case("x-content-type-options")),
            "other headers should be unaffected"
        );
    }
}
