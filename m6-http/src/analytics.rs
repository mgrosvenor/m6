/// Per-request analytics: session cookie issuance, request feature
/// extraction, and structured logging — no external service, no client-side
/// dependency beyond one first-party cookie.
///
/// Everything here is derived from data already present on the request by
/// the time m6-http has a response ready to send: it's server-side feature
/// extraction, not a beacon. UA parsing and geo lookup are deliberately left
/// to the downstream ingester (render-analytics) so the hot path here stays
/// a handful of string operations, not a database/ruleset lookup.
use quiche::h3::NameValue as _;
use rand::RngCore;

pub const SESSION_COOKIE: &str = "_m6sid";

/// Abstraction over "a scannable list of request headers," so the same
/// scanning logic (single-header lookup, and multi-occurrence lookup for
/// `Cookie`, which HTTP/2+ clients may split across several header fields)
/// works identically whether the caller holds an owned `Vec<(String,String)>`
/// (H1/H2/H2C, and the shared MISS-path code) or a raw `&[quiche::h3::Header]`
/// (H3's cache-hit path, which deliberately avoids building an owned Vec on
/// that path — see the comment at the H3 cache-hit call site in main.rs).
///
/// Both concrete types are slices, so both impls can be scanned as many times
/// as needed for free — there's no ownership/consumption cost to worry about,
/// which is what makes a shared trait here strictly better than either (a)
/// forcing H3 to materialize an owned Vec just to match H1/H2's shape, or (b)
/// keeping H3's hand-rolled extraction as a permanent special case.
pub trait HeaderSource {
    fn find(&self, name: &str) -> Option<&str>;
    fn find_all<'a>(&'a self, name: &str) -> impl Iterator<Item = &'a str>;
}

impl HeaderSource for [(String, String)] {
    fn find(&self, name: &str) -> Option<&str> {
        self.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
    fn find_all<'a>(&'a self, name: &str) -> impl Iterator<Item = &'a str> {
        self.iter().filter(move |(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
}

// `&Vec<T>` does not itself satisfy a generic `impl HeaderSource` bound even
// though it coerces to `&[T]` in ordinary (non-generic) call positions — trait
// resolution for `impl Trait` arguments needs the concrete type to implement
// the trait, and `Vec<T>` is a distinct type from `[T]`. Delegate rather than
// touch every `&req.headers` call site's syntax.
impl HeaderSource for Vec<(String, String)> {
    fn find(&self, name: &str) -> Option<&str> {
        self.as_slice().find(name)
    }
    fn find_all<'a>(&'a self, name: &str) -> impl Iterator<Item = &'a str> {
        self.as_slice().find_all(name)
    }
}

impl HeaderSource for [quiche::h3::Header] {
    fn find(&self, name: &str) -> Option<&str> {
        self.find_all(name).next()
    }
    fn find_all<'a>(&'a self, name: &str) -> impl Iterator<Item = &'a str> {
        self.iter().filter_map(move |h| {
            let n = std::str::from_utf8(h.name()).ok()?;
            if !n.eq_ignore_ascii_case(name) {
                return None;
            }
            std::str::from_utf8(h.value()).ok()
        })
    }
}

// H3's `PendingRequest.headers` field (main.rs) is `Vec<quiche::h3::Header>` —
// same Vec-vs-slice coercion issue as above.
impl HeaderSource for Vec<quiche::h3::Header> {
    fn find(&self, name: &str) -> Option<&str> {
        self.as_slice().find(name)
    }
    fn find_all<'a>(&'a self, name: &str) -> impl Iterator<Item = &'a str> {
        self.as_slice().find_all(name)
    }
}

/// Look up a header by case-insensitive name.
pub fn header<'a>(headers: &'a (impl HeaderSource + ?Sized), name: &str) -> Option<&'a str> {
    headers.find(name)
}

/// Extract `_m6sid` from a raw `Cookie` header value (e.g. `"a=1; _m6sid=xyz; b=2"`).
fn session_from_cookie_header(cookie_header: &str) -> Option<String> {
    cookie_header.split(';').find_map(|kv| {
        let (k, v) = kv.trim().split_once('=')?;
        (k == SESSION_COOKIE && !v.is_empty()).then(|| v.to_string())
    })
}

/// A random session id: 16 bytes, hex-encoded. Opaque and not derived from
/// IP/UA, so it can't be used to re-identify a visitor once cookies are
/// cleared — it only ties together requests within one browser's lifetime
/// of the cookie.
fn generate_session_id() -> String {
    let mut bytes = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

/// Read the session id from the incoming request's `Cookie` header, or mint
/// a new one. Returns `(id, is_new)` — callers append a `Set-Cookie` only
/// when `is_new`.
fn get_or_create_session(headers: &(impl HeaderSource + ?Sized)) -> (String, bool) {
    // Not `header(headers, "cookie")` — a request can carry cookies split
    // across multiple `cookie` header fields (see
    // `auth::combined_cookie_header`'s doc comment), and a single-field
    // lookup would miss `_m6sid` whenever it isn't in whichever field
    // happens to come first.
    match crate::auth::combined_cookie_header(headers).as_deref().and_then(session_from_cookie_header) {
        Some(id) => (id, false),
        None => (generate_session_id(), true),
    }
}

/// Build the `Set-Cookie` header value for a freshly-minted session id.
/// First-party, HttpOnly (never read by JS), Secure, SameSite=Lax, and a
/// 30 minute sliding expiry (re-issued on every request that already carries
/// a still-valid cookie extends nothing server-side — the browser just keeps
/// resending the same cookie until it naturally expires; a stricter sliding
/// window would need a re-issue-on-every-request policy, deliberately not
/// done here to keep the hit path a pure read when a cookie is present).
/// Lax, not Strict — Strict cookies set on a redirect response are dropped
/// by some browsers (confirmed: Firefox) on the immediately-following
/// same-site navigation that follows that same redirect, which is exactly
/// the login → 302 → protected-page flow this cookie needs to survive. Lax
/// still blocks the cookie on cross-site requests (the actual CSRF threat
/// model), it just doesn't also block it on top-level same-site redirects.
fn session_cookie_header_value(id: &str) -> String {
    format!("{SESSION_COOKIE}={id}; Max-Age=1800; Path=/; HttpOnly; Secure; SameSite=Lax")
}

/// Referrer, reduced to host + path only — query strings and fragments
/// (which can carry search terms, session tokens, tracking params from the
/// *referring* site) are always dropped before this is logged anywhere.
pub fn parse_referer(raw: &str) -> (Option<String>, Option<String>) {
    let without_scheme = raw.split_once("://").map(|(_, rest)| rest).unwrap_or(raw);
    let end = without_scheme.find(['?', '#']).unwrap_or(without_scheme.len());
    let trimmed = &without_scheme[..end];
    match trimmed.split_once('/') {
        Some((host, path)) if !host.is_empty() => {
            let path = format!("/{path}");
            (Some(host.to_string()), Some(path))
        }
        _ if !trimmed.is_empty() => (Some(trimmed.to_string()), None),
        _ => (None, None),
    }
}

/// Fields extracted from the request headers, ready to log.
pub struct RequestFeatures {
    pub referer_host: Option<String>,
    pub referer_path: Option<String>,
    pub user_agent: Option<String>,
}

fn extract_features(headers: &(impl HeaderSource + ?Sized)) -> RequestFeatures {
    let (referer_host, referer_path) = header(headers, "referer")
        .map(parse_referer)
        .unwrap_or((None, None));
    let user_agent = header(headers, "user-agent").map(|s| s.to_string());
    RequestFeatures { referer_host, referer_path, user_agent }
}

/// THE single chokepoint for finishing a response's analytics: mint-or-reuse
/// a session, log exactly one structured analytics line, and return the
/// Set-Cookie value iff a new session was minted (`None` if disabled or the
/// session already existed). This must be the only caller of
/// `get_or_create_session`/`extract_features`/`log_request` — duplicating
/// this logic is what caused a real bug (two different Set-Cookie headers on
/// one response), because `get_or_create_session` is NOT idempotent: it
/// mints a fresh random id whenever no session cookie is present yet, so two
/// independent calls for the same still-cookie-less request produce two
/// different ids.
#[allow(clippy::too_many_arguments)]
pub fn record(
    enabled: bool,
    request_headers: &(impl HeaderSource + ?Sized),
    node: &str,
    path: &str,
    status: u16,
    cache_state: &str,
    client_ip: &str,
    latency_ns: Option<u64>,
) -> Option<String> {
    if !enabled {
        return None;
    }
    let (session_id, session_new) = get_or_create_session(request_headers);
    let features = extract_features(request_headers);
    log_request(node, path, status, cache_state, client_ip, &session_id, session_new, &features, latency_ns);
    session_new.then(|| session_cookie_header_value(&session_id))
}

/// True if the response's `Content-Type` is HTML. The session cookie has no
/// reason to exist on a CSS/JS/image response — a visitor's session identity
/// is only ever read back on a page render, never on an asset fetch — so
/// gating on this keeps `_m6sid` off the vast majority of responses a page
/// load generates (every static asset it references) instead of writing a
/// fresh `Set-Cookie` on each one.
pub fn is_html_response(resp_headers: &[(String, String)]) -> bool {
    resp_headers.iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-type"))
        .is_some_and(|(_, v)| v.to_ascii_lowercase().starts_with("text/html"))
}

/// Convenience veneer over [`record`] for call sites that already own a
/// mutable outgoing-headers `Vec` and just want the cookie appended, if any.
/// The session is still minted/logged for every request regardless of
/// content type (so analytics stay accurate) — only the `Set-Cookie` write
/// itself is scoped to HTML responses, per [`is_html_response`].
#[allow(clippy::too_many_arguments)]
pub fn finish_response(
    enabled: bool,
    resp_headers: &mut Vec<(String, String)>,
    request_headers: &(impl HeaderSource + ?Sized),
    node: &str,
    path: &str,
    status: u16,
    cache_state: &str,
    client_ip: &str,
    latency_ns: Option<u64>,
) {
    let html = is_html_response(resp_headers);
    if let Some(sc) = record(enabled, request_headers, node, path, status, cache_state, client_ip, latency_ns) {
        if html {
            resp_headers.push(("Set-Cookie".to_string(), sc));
        }
    }
}

/// Extract `_m6sid` from a `Set-Cookie` *response* header value (e.g.
/// `"_m6sid=xyz; Max-Age=1800; Path=/; HttpOnly; Secure; SameSite=Lax"`).
/// Different shape from a request `Cookie` header: exactly one cookie per
/// `Set-Cookie` field, name=value first, followed by `; `-separated
/// attributes — no need to scan multiple cookie pairs within one field.
fn session_from_set_cookie_header(set_cookie: &str) -> Option<String> {
    let first = set_cookie.split(';').next()?;
    let (k, v) = first.trim().split_once('=')?;
    (k == SESSION_COOKIE && !v.is_empty()).then(|| v.to_string())
}

/// Find a `_m6sid` already set by an upstream hop's response. A response can
/// carry multiple `Set-Cookie` fields for unrelated reasons, so every
/// occurrence is checked, not just the first.
fn session_from_response_headers(resp_headers: &[(String, String)]) -> Option<String> {
    resp_headers
        .iter()
        .filter(|(k, _)| k.eq_ignore_ascii_case("set-cookie"))
        .find_map(|(_, v)| session_from_set_cookie_header(v))
}

/// Like [`finish_response`], but for a response whose backend may itself be
/// another m6-http instance — concretely, a cache node's only backend *is*
/// the origin's m6-http, so a cache node's `resp_headers` here can already
/// carry a `_m6sid` the origin just minted for this same request. Deriving
/// the session from `request_headers` alone (as `finish_response` does)
/// can't see that: the *request* forwarded to origin never had a cookie
/// either, so each hop independently concludes "no session yet" and mints
/// its own — two different session ids, two Set-Cookie headers, on one
/// response. (Local-socket backends — m6-html, m6-file, render-* — never set
/// `_m6sid` themselves, so this distinction doesn't apply to sites that only
/// ever talk to those.)
///
/// When the response already carries a session, this node's own analytics
/// line is logged against it — the per-node hit/latency visibility that's
/// the actual point of edge analytics is preserved — without adding a
/// second, conflicting Set-Cookie. Otherwise behaves exactly like
/// `finish_response`.
#[allow(clippy::too_many_arguments)]
pub fn finish_proxied_response(
    enabled: bool,
    resp_headers: &mut Vec<(String, String)>,
    request_headers: &(impl HeaderSource + ?Sized),
    node: &str,
    path: &str,
    status: u16,
    cache_state: &str,
    client_ip: &str,
    latency_ns: Option<u64>,
) {
    if !enabled {
        return;
    }
    if let Some(session_id) = session_from_response_headers(resp_headers) {
        let features = extract_features(request_headers);
        log_request(node, path, status, cache_state, client_ip, &session_id, false, &features, latency_ns);
        return;
    }
    finish_response(enabled, resp_headers, request_headers, node, path, status, cache_state, client_ip, latency_ns);
}

/// Emit one structured analytics line. Routed by `m6_core::log`'s
/// `target: "analytics"` filter to its own file, independent of the main
/// operational log — see `m6-core/src/log.rs::init_with_analytics`.
#[allow(clippy::too_many_arguments)]
fn log_request(
    node: &str,
    path: &str,
    status: u16,
    cache_state: &str,
    client_ip: &str,
    session_id: &str,
    session_new: bool,
    features: &RequestFeatures,
    latency_ns: Option<u64>,
) {
    tracing::info!(
        target: "analytics",
        node = node,
        path = path,
        status = status,
        cache_state = cache_state,
        client_ip = client_ip,
        session_id = session_id,
        session_new = session_new,
        referer_host = features.referer_host.as_deref(),
        referer_path = features.referer_path.as_deref(),
        user_agent = features.user_agent.as_deref(),
        latency_ns = latency_ns,
        "request"
    );
}

/// Record a poll of `/health` or `/perf`.
///
/// These used to appear nowhere at all. They short-circuit inside
/// `handle_request` and return before the analytics call further down, so the
/// request log had no row for them, and the stats counters skipped them at
/// three separate call sites. That kept site traffic honest and made the
/// monitoring itself invisible: a check that had silently stopped looked
/// exactly like a check that was passing, and a flood aimed at `/health` was
/// not recorded anywhere.
///
/// Emitted with `message = "monitor"` rather than `"request"`, which is what
/// keeps it out of site traffic. Every existing consumer already filters on
/// `message == "request"` (`node-summary.py` does), so none of them change
/// behaviour and none needed editing, while a consumer that wants monitoring
/// can now ask for it. The alternative, a `traffic_class` field on every row,
/// would have meant plumbing a parameter through four call sites to say
/// "site" in all but two of them.
///
/// No session is minted: a monitor is not a visitor, and issuing it a session
/// cookie would put an unbounded number of one-request sessions into the data.
pub fn log_monitor(
    enabled: bool,
    request_headers: &(impl HeaderSource + ?Sized),
    node: &str,
    path: &str,
    status: u16,
    client_ip: &str,
    latency_ns: Option<u64>,
) {
    if !enabled {
        return;
    }
    let features = extract_features(request_headers);
    tracing::info!(
        target: "analytics",
        node = node,
        path = path,
        status = status,
        client_ip = client_ip,
        user_agent = features.user_agent.as_deref(),
        latency_ns = latency_ns,
        "monitor"
    );
}

/// Log a request rejected by the per-IP rate limiter, before it ever reaches
/// routing/cache lookup — a separate, lighter event from `log_request` since
/// almost nothing about the request has been processed yet at that point.
pub fn log_rate_limited(node: &str, path: &str, client_ip: &str, user_agent: Option<&str>) {
    tracing::info!(
        target: "analytics",
        node = node,
        path = path,
        status = 429u16,
        cache_state = "BYPASS",
        client_ip = client_ip,
        bot_flag = true,
        bot_reason = "rate-exceeded",
        user_agent = user_agent,
        "request"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_session_from_cookie_header_present() {
        let v = session_from_cookie_header("a=1; _m6sid=abc123; b=2");
        assert_eq!(v, Some("abc123".to_string()));
    }

    #[test]
    fn test_session_from_cookie_header_absent() {
        assert_eq!(session_from_cookie_header("a=1; b=2"), None);
    }

    #[test]
    fn test_session_from_cookie_header_empty_value_ignored() {
        assert_eq!(session_from_cookie_header("_m6sid=; b=2"), None);
    }

    #[test]
    fn test_get_or_create_session_reuses_existing() {
        let headers = vec![("Cookie".to_string(), "_m6sid=existing-id".to_string())];
        let (id, is_new) = get_or_create_session(&headers);
        assert_eq!(id, "existing-id");
        assert!(!is_new);
    }

    #[test]
    fn test_get_or_create_session_mints_new_when_absent() {
        let (id, is_new) = get_or_create_session(&Vec::<(String, String)>::new());
        assert!(is_new);
        assert_eq!(id.len(), 32); // 16 bytes hex-encoded
    }

    #[test]
    fn test_session_cookie_header_value_shape() {
        let v = session_cookie_header_value("abc");
        assert!(v.starts_with("_m6sid=abc;"));
        assert!(v.contains("HttpOnly"));
        assert!(v.contains("Secure"));
        assert!(v.contains("SameSite=Lax"));
    }

    #[test]
    fn test_parse_referer_strips_query_and_scheme() {
        let (host, path) = parse_referer("https://www.google.com/search?q=dr+grosvenor&foo=bar");
        assert_eq!(host, Some("www.google.com".to_string()));
        assert_eq!(path, Some("/search".to_string()));
    }

    #[test]
    fn test_parse_referer_host_only_no_path() {
        let (host, path) = parse_referer("https://news.ycombinator.com");
        assert_eq!(host, Some("news.ycombinator.com".to_string()));
        assert_eq!(path, None);
    }

    #[test]
    fn test_parse_referer_drops_fragment() {
        let (host, path) = parse_referer("https://example.com/page#section");
        assert_eq!(host, Some("example.com".to_string()));
        assert_eq!(path, Some("/page".to_string()));
    }

    #[test]
    fn test_extract_features_all_present() {
        let headers = vec![
            ("Referer".to_string(), "https://t.co/abc?x=1".to_string()),
            ("User-Agent".to_string(), "Mozilla/5.0 Test".to_string()),
        ];
        let f = extract_features(&headers);
        assert_eq!(f.referer_host.as_deref(), Some("t.co"));
        assert_eq!(f.referer_path.as_deref(), Some("/abc"));
        assert_eq!(f.user_agent.as_deref(), Some("Mozilla/5.0 Test"));
    }

    #[test]
    fn test_extract_features_none_present() {
        let f = extract_features(&Vec::<(String, String)>::new());
        assert!(f.referer_host.is_none());
        assert!(f.user_agent.is_none());
    }

    // ── HeaderSource: both impls must behave identically for logically ──────
    // equivalent input, since that equivalence is the whole point of the trait.

    fn quiche_headers(pairs: &[(&str, &str)]) -> Vec<quiche::h3::Header> {
        pairs.iter().map(|(k, v)| quiche::h3::Header::new(k.as_bytes(), v.as_bytes())).collect()
    }

    #[test]
    fn header_source_vec_and_quiche_agree_on_find() {
        let vec_headers = vec![
            ("Cookie".to_string(), "a=1".to_string()),
            ("User-Agent".to_string(), "test-ua".to_string()),
        ];
        let h3_headers = quiche_headers(&[("cookie", "a=1"), ("user-agent", "test-ua")]);

        assert_eq!(vec_headers.find("user-agent"), h3_headers.find("user-agent"));
        assert_eq!(vec_headers.find("USER-AGENT"), Some("test-ua")); // case-insensitive
        assert_eq!(h3_headers.find("USER-AGENT"), Some("test-ua"));
        assert_eq!(vec_headers.find("absent"), None);
        assert_eq!(h3_headers.find("absent"), None);
    }

    #[test]
    fn header_source_vec_and_quiche_agree_on_find_all_multi_occurrence() {
        // RFC 7540 §8.1.2.5: HTTP/2+ clients may split Cookie across several
        // header fields — find_all must see every one, not just the first.
        let vec_headers = vec![
            ("cookie".to_string(), "a=1".to_string()),
            ("cookie".to_string(), "b=2".to_string()),
        ];
        let h3_headers = quiche_headers(&[("cookie", "a=1"), ("cookie", "b=2")]);

        let vec_all: Vec<&str> = vec_headers.find_all("cookie").collect();
        let h3_all: Vec<&str> = h3_headers.find_all("cookie").collect();
        assert_eq!(vec_all, vec!["a=1", "b=2"]);
        assert_eq!(h3_all, vec!["a=1", "b=2"]);
    }

    #[test]
    fn combined_cookie_header_joins_multiple_fields_for_both_impls() {
        let vec_headers = vec![
            ("cookie".to_string(), "a=1".to_string()),
            ("cookie".to_string(), "b=2".to_string()),
        ];
        let h3_headers = quiche_headers(&[("cookie", "a=1"), ("cookie", "b=2")]);

        assert_eq!(crate::auth::combined_cookie_header(&vec_headers).as_deref(), Some("a=1; b=2"));
        assert_eq!(crate::auth::combined_cookie_header(&h3_headers).as_deref(), Some("a=1; b=2"));
    }

    #[test]
    fn quiche_header_source_ignores_non_utf8_gracefully() {
        // A malformed/non-UTF8 header value must not panic — just be invisible
        // to find/find_all, same as `str::from_utf8` failing anywhere else in
        // this codebase's header handling.
        let bad = vec![quiche::h3::Header::new(b"user-agent", &[0xFF, 0xFE])];
        assert_eq!(bad.as_slice().find("user-agent"), None);
    }

    // ── record()/finish_response(): the actual chokepoint ───────────────────

    #[test]
    fn record_returns_none_when_disabled() {
        let headers = Vec::<(String, String)>::new();
        let sc = record(false, &headers, "node", "/p", 200, "HIT", "1.2.3.4", None);
        assert!(sc.is_none());
    }

    #[test]
    fn record_returns_cookie_only_when_session_new() {
        let headers = Vec::<(String, String)>::new(); // no _m6sid cookie present
        let sc = record(true, &headers, "node", "/p", 200, "HIT", "1.2.3.4", Some(123));
        assert!(sc.is_some(), "a request with no existing session must mint one and return its cookie");
        assert!(sc.unwrap().starts_with("_m6sid="));
    }

    #[test]
    fn record_reuses_existing_session_no_cookie() {
        let headers = vec![("Cookie".to_string(), "_m6sid=existing-id".to_string())];
        let sc = record(true, &headers, "node", "/p", 200, "HIT", "1.2.3.4", Some(123));
        assert!(sc.is_none(), "a request already carrying a session cookie must not get a new one");
    }

    #[test]
    fn record_h3_and_vec_agree_for_equivalent_requests() {
        // The whole point of HeaderSource: identical logical input through
        // either representation must produce identical mint-or-reuse behavior.
        let vec_headers = vec![("Cookie".to_string(), "_m6sid=shared-id".to_string())];
        let h3_headers = quiche_headers(&[("cookie", "_m6sid=shared-id")]);

        let vec_sc = record(true, &vec_headers, "node", "/p", 200, "HIT", "1.2.3.4", None);
        let h3_sc = record(true, &h3_headers, "node", "/p", 200, "HIT", "1.2.3.4", None);
        assert_eq!(vec_sc, None, "vec-backed request with existing session should reuse it");
        assert_eq!(h3_sc, None, "h3-backed request with existing session should reuse it");
    }

    #[test]
    fn finish_response_appends_set_cookie_only_when_minted() {
        let mut resp_headers = vec![("Content-Type".to_string(), "text/html; charset=utf-8".to_string())];
        let req_headers = Vec::<(String, String)>::new();
        finish_response(true, &mut resp_headers, &req_headers, "node", "/p", 200, "HIT", "1.2.3.4", None);
        assert_eq!(resp_headers.len(), 2, "expected exactly one Set-Cookie appended: {resp_headers:?}");
        assert_eq!(resp_headers[1].0, "Set-Cookie");
    }

    #[test]
    fn finish_response_skips_set_cookie_for_non_html() {
        let mut resp_headers = vec![("Content-Type".to_string(), "text/plain".to_string())];
        let req_headers = Vec::<(String, String)>::new();
        finish_response(true, &mut resp_headers, &req_headers, "node", "/p", 200, "HIT", "1.2.3.4", None);
        assert_eq!(resp_headers.len(), 1, "non-HTML responses must not get a session cookie");
    }

    #[test]
    fn finish_response_appends_nothing_when_disabled() {
        let mut resp_headers = vec![("Content-Type".to_string(), "text/plain".to_string())];
        let req_headers = Vec::<(String, String)>::new();
        finish_response(false, &mut resp_headers, &req_headers, "node", "/p", 200, "HIT", "1.2.3.4", None);
        assert_eq!(resp_headers.len(), 1, "disabled analytics must not touch response headers");
    }

    // ── finish_proxied_response(): the multi-hop (cache node → origin) fix ──

    #[test]
    fn session_from_set_cookie_header_extracts_id() {
        let v = session_from_set_cookie_header("_m6sid=abc123; Max-Age=1800; Path=/; HttpOnly; Secure; SameSite=Lax");
        assert_eq!(v, Some("abc123".to_string()));
    }

    #[test]
    fn session_from_set_cookie_header_ignores_other_cookies() {
        let v = session_from_set_cookie_header("other=xyz; Path=/");
        assert_eq!(v, None);
    }

    #[test]
    fn session_from_response_headers_scans_all_set_cookie_occurrences() {
        let headers = vec![
            ("Content-Type".to_string(), "text/html".to_string()),
            ("Set-Cookie".to_string(), "other=xyz; Path=/".to_string()),
            ("Set-Cookie".to_string(), "_m6sid=upstream-id; Max-Age=1800".to_string()),
        ];
        assert_eq!(session_from_response_headers(&headers), Some("upstream-id".to_string()));
    }

    #[test]
    fn finish_proxied_response_reuses_upstream_session_without_adding_a_second_cookie() {
        // Simulates a cache node's backend response: origin already minted
        // and set _m6sid before this node ever sees the request.
        let mut resp_headers = vec![(
            "Set-Cookie".to_string(),
            "_m6sid=origin-minted-id; Max-Age=1800; Path=/; HttpOnly; Secure; SameSite=Lax".to_string(),
        )];
        let req_headers = Vec::<(String, String)>::new(); // the original client request never had a cookie
        finish_proxied_response(true, &mut resp_headers, &req_headers, "edge-node", "/p", 200, "MISS", "1.2.3.4", None);

        let set_cookies: Vec<&(String, String)> =
            resp_headers.iter().filter(|(k, _)| k.eq_ignore_ascii_case("set-cookie")).collect();
        assert_eq!(
            set_cookies.len(), 1,
            "must not add a second Set-Cookie when upstream already set one: {resp_headers:?}"
        );
    }

    #[test]
    fn finish_proxied_response_mints_normally_when_no_upstream_session_present() {
        // A local-socket backend's response (e.g. m6-html) never sets
        // _m6sid — behaves exactly like finish_response in that case.
        let mut resp_headers = vec![("Content-Type".to_string(), "text/html".to_string())];
        let req_headers = Vec::<(String, String)>::new();
        finish_proxied_response(true, &mut resp_headers, &req_headers, "origin", "/p", 200, "MISS", "1.2.3.4", None);
        assert_eq!(resp_headers.len(), 2, "expected exactly one Set-Cookie minted: {resp_headers:?}");
        assert_eq!(resp_headers[1].0, "Set-Cookie");
    }

    #[test]
    fn finish_proxied_response_does_nothing_when_disabled() {
        let mut resp_headers = vec![(
            "Set-Cookie".to_string(),
            "_m6sid=origin-minted-id; Max-Age=1800".to_string(),
        )];
        let req_headers = Vec::<(String, String)>::new();
        finish_proxied_response(false, &mut resp_headers, &req_headers, "edge-node", "/p", 200, "MISS", "1.2.3.4", None);
        assert_eq!(resp_headers.len(), 1, "disabled analytics must not touch response headers even to dedupe");
    }
}
