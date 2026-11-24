/// Per-request analytics: session cookie issuance, request feature
/// extraction, and structured logging — no external service, no client-side
/// dependency beyond one first-party cookie.
///
/// Everything here is derived from data already present on the request by
/// the time m6-http has a response ready to send: it's server-side feature
/// extraction, not a beacon. UA parsing and geo lookup are deliberately left
/// to a downstream analytics ingester so the hot path here stays
/// a handful of string operations, not a database/ruleset lookup.
use m6_core::log::{Level, Line, Sink, Type};
use quiche::h3::NameValue as _;

use crate::health::{unix_micros_now, Kind, TrafficWindow};

pub use m6_core::http::{header, HeaderSource};

pub const SESSION_COOKIE: &str = "_m6sid";

/// A freshly minted session id: 16 random bytes, 32 lowercase hex characters,
/// on the stack.
///
/// [`record`] returns one instead of a `String`, so minting a session costs no
/// heap allocation. The 32 bytes are a small local, which the event loop's
/// primer allows where a larger stack buffer is not.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SessionId([u8; 32]);

impl SessionId {
    /// The id as a `&str`. The buffer is always ASCII hex.
    pub fn as_str(&self) -> &str {
        std::str::from_utf8(&self.0).expect("a hex session id is ASCII")
    }
}

/// The session id a request carries, or one minted for it.
///
/// The existing arm borrows the request's `cookie` field. Only the new arm
/// holds anything of its own, and that is 32 bytes on the stack. This lets
/// `record` log one id in both cases and answer with an id only when it minted
/// one.
enum Session<'a> {
    Existing(&'a str),
    New(SessionId),
}

impl Session<'_> {
    fn id(&self) -> &str {
        match self {
            Session::Existing(id) => id,
            Session::New(id) => id.as_str(),
        }
    }

    fn is_new(&self) -> bool {
        matches!(self, Session::New(_))
    }
}

/// `HeaderSource` for quiche's header type.
///
/// The trait lives in `m6_core::http`, where it belongs, and `quiche` is
/// `m6-http`'s dependency alone: HTTP/2 and HTTP/3 terminate here and nowhere
/// else. That leaves `impl m6_core::HeaderSource for [quiche::h3::Header]` as
/// a foreign trait on a foreign type, which the orphan rule forbids, so the
/// slice is wrapped.
///
/// The wrapper is what keeps H3's cache-hit path free of an owned `Vec`: it
/// borrows quiche's headers and scans them in place. Materialising a
/// `Vec<(String, String)>` just to read one header would be an allocation per
/// request on the hottest path in the server.
#[derive(Clone, Copy)]
pub struct H3Headers<'a>(pub &'a [quiche::h3::Header]);

impl HeaderSource for H3Headers<'_> {
    fn find(&self, name: &str) -> Option<&str> {
        self.find_all(name).next()
    }
    fn find_all<'a>(&'a self, name: &str) -> impl Iterator<Item = &'a str> {
        self.0.iter().filter_map(move |h| {
            let n = std::str::from_utf8(h.name()).ok()?;
            if !n.eq_ignore_ascii_case(name) {
                return None;
            }
            std::str::from_utf8(h.value()).ok()
        })
    }
    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// Extract `_m6sid` from a raw `Cookie` header value (e.g. `"a=1; _m6sid=xyz; b=2"`).
///
/// The id is a slice of `cookie_header`, not a copy: this runs on every
/// request, and the only reader is the line, which copies it once into the log
/// buffer.
fn session_from_cookie_header(cookie_header: &str) -> Option<&str> {
    cookie_header.split(';').find_map(|kv| {
        let (k, v) = kv.trim().split_once('=')?;
        (k == SESSION_COOKIE && !v.is_empty()).then_some(v)
    })
}

/// A random session id: 16 bytes, hex-encoded into a `[u8; 32]` on the stack.
/// Opaque and not derived from IP/UA, so it can't be used to re-identify a
/// visitor once cookies are cleared — it only ties together requests within one
/// browser's lifetime of the cookie.
fn generate_session_id() -> SessionId {
    let mut id = [0u8; 32];
    m6_core::random_hex_token_into(&mut id);
    SessionId(id)
}

/// Read the session id from the incoming request's `Cookie` headers, or mint
/// a new one. The id borrows the request when it already has one, so a
/// returning visitor's request makes no allocation here.
fn get_or_create_session(headers: &(impl HeaderSource + ?Sized)) -> Session<'_> {
    // Not `header(headers, "cookie")` — a request can carry cookies split
    // across multiple `cookie` header fields (see
    // `auth::combined_cookie_header`'s doc comment), and a single-field
    // lookup would miss `_m6sid` whenever it isn't in whichever field
    // happens to come first. Scanning the fields in turn is why this no
    // longer joins them into a new `String` first. When two fields both carry
    // `_m6sid`, the first one with a value wins, which is the order the old
    // join produced, and `a_cookie_split_across_fields_is_still_read` pins it.
    headers
        .find_all("cookie")
        .find_map(session_from_cookie_header)
        .map_or_else(|| Session::New(generate_session_id()), Session::Existing)
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
///
/// Called only where a response's headers are written and only for a new
/// session, so the one `String` it makes is a new visit's `Set-Cookie` and not
/// a per-request cost.
///
/// Takes the minted [`SessionId`] rather than a string so that a value which
/// came from the request cannot reach a response header by accident.
pub fn session_cookie_header_value(id: SessionId) -> String {
    let id = id.as_str();
    format!("{SESSION_COOKIE}={id}; Max-Age=1800; Path=/; HttpOnly; Secure; SameSite=Lax")
}

/// Referrer, reduced to host + path only — query strings and fragments
/// (which can carry search terms, session tokens, tracking params from the
/// *referring* site) are always dropped before this is logged anywhere.
///
/// Both halves borrow `raw`: the path is the slice from the first `/`, which
/// already carries its slash, so no `format!` and no `String` is built.
pub fn parse_referer(raw: &str) -> (Option<&str>, Option<&str>) {
    let without_scheme = raw.split_once("://").map(|(_, rest)| rest).unwrap_or(raw);
    let end = without_scheme
        .find(['?', '#'])
        .unwrap_or(without_scheme.len());
    let trimmed = &without_scheme[..end];
    match trimmed.split_once('/') {
        Some((host, _)) if !host.is_empty() => (Some(host), Some(&trimmed[host.len()..])),
        _ if !trimmed.is_empty() => (Some(trimmed), None),
        _ => (None, None),
    }
}

/// What happened to one request on one node.
///
/// One thing, not six: no call site has a reason to supply some of these and
/// not the rest, and every function in this module wants all of them.
///
/// Borrowed rather than owned. Every caller is in the middle of handling the
/// request these describe and already holds them; an owned `Event` would mean
/// five `String` allocations on a path that runs once per request.
pub struct Event<'a> {
    pub node: &'a str,
    pub path: &'a str,
    pub status: u16,
    pub cache_state: &'a str,
    pub client_ip: &'a str,
    pub latency_ns: Option<u64>,
}

/// Fields extracted from the request headers, ready to log.
///
/// Borrowed rather than owned: every caller is in the middle of handling the
/// request these describe and already holds them, so an owned `String` per
/// field would put three allocations on the path the log exists to make cheap.
pub struct RequestFeatures<'a> {
    pub referer_host: Option<&'a str>,
    pub referer_path: Option<&'a str>,
    pub user_agent: Option<&'a str>,
}

fn extract_features<'a>(headers: &'a (impl HeaderSource + ?Sized)) -> RequestFeatures<'a> {
    let (referer_host, referer_path) = header(headers, "referer")
        .map(parse_referer)
        .unwrap_or((None, None));
    let user_agent = header(headers, "user-agent");
    RequestFeatures {
        referer_host,
        referer_path,
        user_agent,
    }
}

/// Push one served request into the traffic window.
///
/// `record` and `finish_proxied_response` log the same request shape, so they
/// read the time and the fields in one place.
fn push_request(window: &mut TrafficWindow, ev: &Event<'_>, user_agent: Option<&str>) {
    window.push(
        unix_micros_now(),
        Kind::Request,
        ev.status,
        ev.path,
        ev.client_ip,
        user_agent,
    );
}

/// THE single chokepoint for finishing a response's analytics: mint-or-reuse
/// a session, log exactly one structured analytics line, and return the
/// new session id iff one was minted (`None` if disabled or the session
/// already existed). This must be the only caller of
/// `get_or_create_session`/`extract_features`/`log_request` — duplicating
/// this logic is what caused a real bug (two different Set-Cookie headers on
/// one response), because `get_or_create_session` is NOT idempotent: it
/// mints a fresh random id whenever no session cookie is present yet, so two
/// independent calls for the same still-cookie-less request produce two
/// different ids.
///
/// No heap allocation on either path: an existing id is a slice of the
/// request's `cookie` field, a new one lives in the returned `SessionId`, and
/// the line is built straight into the analytics Sink's buffer. The caller
/// builds the `Set-Cookie` value only where it writes response headers, and
/// only for a new session.
pub fn record(
    enabled: bool,
    request_headers: &(impl HeaderSource + ?Sized),
    ev: &Event<'_>,
    window: &mut TrafficWindow,
) -> Option<SessionId> {
    if !enabled {
        return None;
    }
    let session = get_or_create_session(request_headers);
    let features = extract_features(request_headers);
    log_request(ev, session.id(), session.is_new(), &features);
    push_request(window, ev, features.user_agent);
    match session {
        Session::New(id) => Some(id),
        Session::Existing(_) => None,
    }
}

/// True if the response's `Content-Type` is HTML. The session cookie has no
/// reason to exist on a CSS/JS/image response — a visitor's session identity
/// is only ever read back on a page render, never on an asset fetch — so
/// gating on this keeps `_m6sid` off the vast majority of responses a page
/// load generates (every static asset it references) instead of writing a
/// fresh `Set-Cookie` on each one.
pub fn is_html_response(resp_headers: &(impl m6_core::http::HeaderSource + ?Sized)) -> bool {
    // Case-insensitive prefix check with no allocation: `to_ascii_lowercase`
    // built a `String` on the cached-hit hot path.
    resp_headers.find("content-type").is_some_and(|v| {
        v.as_bytes()
            .get(..9)
            .is_some_and(|p| p.eq_ignore_ascii_case(b"text/html"))
    })
}

/// Convenience veneer over [`record`] for call sites that already own a
/// mutable outgoing-headers `Vec` and just want the cookie appended, if any.
/// The session is still minted/logged for every request regardless of
/// content type (so analytics stay accurate) — only the `Set-Cookie` write
/// itself is scoped to HTML responses, per [`is_html_response`].
pub fn finish_response(
    enabled: bool,
    resp_headers: &mut Vec<(String, String)>,
    request_headers: &(impl HeaderSource + ?Sized),
    ev: &Event<'_>,
    window: &mut TrafficWindow,
) {
    let html = is_html_response(resp_headers);
    if let Some(id) = record(enabled, request_headers, ev, window) {
        if html {
            resp_headers.push(("Set-Cookie".to_string(), session_cookie_header_value(id)));
        }
    }
}

/// Extract `_m6sid` from a `Set-Cookie` *response* header value (e.g.
/// `"_m6sid=xyz; Max-Age=1800; Path=/; HttpOnly; Secure; SameSite=Lax"`).
/// Different shape from a request `Cookie` header: exactly one cookie per
/// `Set-Cookie` field, name=value first, followed by `; `-separated
/// attributes — no need to scan multiple cookie pairs within one field.
///
/// Borrows the value, so a proxied response that already carries a session
/// logs against it without copying the id.
fn session_from_set_cookie_header(set_cookie: &str) -> Option<&str> {
    let first = set_cookie.split(';').next()?;
    let (k, v) = first.trim().split_once('=')?;
    (k == SESSION_COOKIE && !v.is_empty()).then_some(v)
}

/// Find a `_m6sid` already set by an upstream hop's response. A response can
/// carry multiple `Set-Cookie` fields for unrelated reasons, so every
/// occurrence is checked, not just the first.
fn session_from_response_headers(resp_headers: &[(String, String)]) -> Option<&str> {
    // `get_all`, not `get`: Set-Cookie is the field the headers module names as
    // the one everybody folds by mistake, and a response may carry several.
    m6_core::headers::get_all(resp_headers, "set-cookie").find_map(session_from_set_cookie_header)
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
pub fn finish_proxied_response(
    enabled: bool,
    resp_headers: &mut Vec<(String, String)>,
    request_headers: &(impl HeaderSource + ?Sized),
    ev: &Event<'_>,
    window: &mut TrafficWindow,
) {
    if !enabled {
        return;
    }
    if let Some(session_id) = session_from_response_headers(resp_headers) {
        let features = extract_features(request_headers);
        log_request(ev, session_id, false, &features);
        push_request(window, ev, features.user_agent);
        return;
    }
    finish_response(enabled, resp_headers, request_headers, ev, window);
}

/// Emit one structured analytics line into the one Sink, stdout, which
/// journals under systemd. See `m6_core::log`.
fn log_request(
    ev: &Event<'_>,
    session_id: &str,
    session_new: bool,
    features: &RequestFeatures<'_>,
) {
    m6_core::log::with_operational(|sink| {
        write_request_line(sink, ev, session_id, session_new, features);
    });
}

/// Build the `message = "request"` line into `sink`.
///
/// Split out from [`log_request`] so the golden-meaning test can write the
/// line into a file of its own without opening the process's Sink.
fn write_request_line(
    sink: &mut Sink,
    ev: &Event<'_>,
    session_id: &str,
    session_new: bool,
    features: &RequestFeatures<'_>,
) {
    Line::begin(sink, Level::Info, "request")
        .kind(Type::Access)
        .str("node", ev.node)
        .str("path", ev.path)
        .field("status", ev.status)
        .str("cache_state", ev.cache_state)
        .str("client_ip", ev.client_ip)
        .str("session_id", session_id)
        .bool("session_new", session_new)
        .opt_str("referer_host", features.referer_host)
        .opt_str("referer_path", features.referer_path)
        .opt_str("user_agent", features.user_agent)
        .field("latency_ns", ev.latency_ns)
        .end();
}

/// Record a `/health` poll.
///
/// It short-circuits inside `handle_request` and returns before the analytics
/// call further down, and the stats counters skip it inside `Stats::record`.
/// Without a row of its own it appears nowhere at all, which keeps
/// site traffic honest but makes the monitoring itself invisible: a check that
/// has silently stopped looks exactly like a check that is passing, and a flood
/// aimed at `/health` is recorded nowhere.
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
#[allow(clippy::too_many_arguments)] // one poll's fields plus the window it records into
pub fn log_monitor(
    enabled: bool,
    request_headers: &(impl HeaderSource + ?Sized),
    node: &str,
    path: &str,
    status: u16,
    client_ip: &str,
    latency_ns: Option<u64>,
    window: &mut TrafficWindow,
) {
    if !enabled {
        return;
    }
    let features = extract_features(request_headers);
    m6_core::log::with_operational(|sink| {
        write_monitor_line(
            sink,
            node,
            path,
            status,
            client_ip,
            features.user_agent,
            latency_ns,
        );
    });
    window.push(
        unix_micros_now(),
        Kind::Monitor,
        status,
        path,
        client_ip,
        features.user_agent,
    );
}

/// Build the `message = "monitor"` line into `sink`. See [`write_request_line`].
fn write_monitor_line(
    sink: &mut Sink,
    node: &str,
    path: &str,
    status: u16,
    client_ip: &str,
    user_agent: Option<&str>,
    latency_ns: Option<u64>,
) {
    Line::begin(sink, Level::Info, "monitor")
        .kind(Type::Access)
        .str("node", node)
        .str("path", path)
        .field("status", status)
        .str("client_ip", client_ip)
        .opt_str("user_agent", user_agent)
        .field("latency_ns", latency_ns)
        .end();
}

/// Log a request rejected by the per-IP rate limiter, before it ever reaches
/// routing/cache lookup — a separate, lighter event from `log_request` since
/// almost nothing about the request has been processed yet at that point.
pub fn log_rate_limited(
    node: &str,
    path: &str,
    client_ip: &str,
    user_agent: Option<&str>,
    window: &mut TrafficWindow,
) {
    m6_core::log::with_operational(|sink| {
        write_rate_limited_line(sink, node, path, client_ip, user_agent);
    });
    window.push(
        unix_micros_now(),
        Kind::RateLimited,
        429,
        path,
        client_ip,
        user_agent,
    );
}

/// Build the rate limiter's `message = "request"` line into `sink`. See
/// [`write_request_line`].
fn write_rate_limited_line(
    sink: &mut Sink,
    node: &str,
    path: &str,
    client_ip: &str,
    user_agent: Option<&str>,
) {
    Line::begin(sink, Level::Info, "request")
        .str("node", node)
        .str("path", path)
        .u64("status", 429)
        .kind(Type::Access)
        .str("cache_state", "BYPASS")
        .str("client_ip", client_ip)
        .bool("bot_flag", true)
        .str("bot_reason", "rate-exceeded")
        .opt_str("user_agent", user_agent)
        .end();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_session_from_cookie_header_present() {
        let v = session_from_cookie_header("a=1; _m6sid=abc123; b=2");
        assert_eq!(v, Some("abc123"));
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
        let session = get_or_create_session(&headers);
        assert_eq!(session.id(), "existing-id");
        assert!(!session.is_new());
    }

    #[test]
    fn test_get_or_create_session_mints_new_when_absent() {
        let headers = Vec::<(String, String)>::new();
        let session = get_or_create_session(&headers);
        assert!(session.is_new());
        assert_eq!(session.id().len(), 32); // 16 bytes hex-encoded
        assert!(
            session
                .id()
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "a session id is lowercase hex"
        );
    }

    #[test]
    fn a_cookie_split_across_fields_is_scanned_in_turn() {
        // RFC 9113 8.2.3: a client may split Cookie across several fields. The
        // id must be found in the second field, without joining them first.
        let headers = vec![
            ("cookie".to_string(), "a=1".to_string()),
            ("cookie".to_string(), "_m6sid=split-id; b=2".to_string()),
        ];
        let session = get_or_create_session(&headers);
        assert_eq!(session.id(), "split-id");
        assert!(!session.is_new());
    }

    #[test]
    fn test_session_cookie_header_value_shape() {
        let v = session_cookie_header_value(SessionId([b'a'; 32]));
        assert!(v.starts_with(&format!("_m6sid={};", "a".repeat(32))));
        assert!(v.contains("HttpOnly"));
        assert!(v.contains("Secure"));
        assert!(v.contains("SameSite=Lax"));
    }

    #[test]
    fn test_parse_referer_strips_query_and_scheme() {
        let (host, path) = parse_referer("https://www.example.com/search?q=example+search&foo=bar");
        assert_eq!(host, Some("www.example.com"));
        assert_eq!(path, Some("/search"));
    }

    #[test]
    fn test_parse_referer_host_only_no_path() {
        let (host, path) = parse_referer("https://news.example.org");
        assert_eq!(host, Some("news.example.org"));
        assert_eq!(path, None);
    }

    #[test]
    fn test_parse_referer_drops_fragment() {
        let (host, path) = parse_referer("https://example.com/page#section");
        assert_eq!(host, Some("example.com"));
        assert_eq!(path, Some("/page"));
    }

    #[test]
    fn test_extract_features_all_present() {
        let headers = vec![
            (
                "Referer".to_string(),
                "https://example.net/abc?x=1".to_string(),
            ),
            ("User-Agent".to_string(), "Mozilla/5.0 Test".to_string()),
        ];
        let f = extract_features(&headers);
        assert_eq!(f.referer_host, Some("example.net"));
        assert_eq!(f.referer_path, Some("/abc"));
        assert_eq!(f.user_agent, Some("Mozilla/5.0 Test"));
    }

    #[test]
    fn test_extract_features_none_present() {
        let headers = Vec::<(String, String)>::new();
        let f = extract_features(&headers);
        assert!(f.referer_host.is_none());
        assert!(f.user_agent.is_none());
    }

    // ── HeaderSource: both impls must behave identically for logically ──────
    // equivalent input, since that equivalence is the whole point of the trait.

    fn quiche_headers(pairs: &[(&str, &str)]) -> Vec<quiche::h3::Header> {
        pairs
            .iter()
            .map(|(k, v)| quiche::h3::Header::new(k.as_bytes(), v.as_bytes()))
            .collect()
    }

    #[test]
    fn header_source_vec_and_quiche_agree_on_find() {
        let vec_headers = vec![
            ("Cookie".to_string(), "a=1".to_string()),
            ("User-Agent".to_string(), "test-ua".to_string()),
        ];
        let h3_raw = quiche_headers(&[("cookie", "a=1"), ("user-agent", "test-ua")]);
        let h3_headers = H3Headers(&h3_raw);

        assert_eq!(
            vec_headers.find("user-agent"),
            h3_headers.find("user-agent")
        );
        assert_eq!(vec_headers.find("USER-AGENT"), Some("test-ua")); // case-insensitive
        assert_eq!(h3_headers.find("USER-AGENT"), Some("test-ua"));
        assert_eq!(vec_headers.find("absent"), None);
        assert_eq!(h3_headers.find("absent"), None);
    }

    #[test]
    fn header_source_vec_and_quiche_agree_on_find_all_multi_occurrence() {
        // RFC 9113 8.2.3 and RFC 9114 4.2.1: HTTP/2 and HTTP/3 clients may
        // split Cookie across several header fields, so find_all must see
        // every one, not just the first.
        let vec_headers = vec![
            ("cookie".to_string(), "a=1".to_string()),
            ("cookie".to_string(), "b=2".to_string()),
        ];
        let h3_raw = quiche_headers(&[("cookie", "a=1"), ("cookie", "b=2")]);
        let h3_headers = H3Headers(&h3_raw);

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
        let h3_raw = quiche_headers(&[("cookie", "a=1"), ("cookie", "b=2")]);
        let h3_headers = H3Headers(&h3_raw);

        assert_eq!(
            crate::auth::combined_cookie_header(&vec_headers).as_deref(),
            Some("a=1; b=2")
        );
        assert_eq!(
            crate::auth::combined_cookie_header(&h3_headers).as_deref(),
            Some("a=1; b=2")
        );
    }

    #[test]
    fn quiche_header_source_ignores_non_utf8_gracefully() {
        // A malformed/non-UTF8 header value must not panic — just be invisible
        // to find/find_all, same as `str::from_utf8` failing anywhere else in
        // this codebase's header handling.
        let bad = vec![quiche::h3::Header::new(b"user-agent", &[0xFF, 0xFE])];
        assert_eq!(H3Headers(bad.as_slice()).find("user-agent"), None);
    }

    // ── record()/finish_response(): the actual chokepoint ───────────────────

    #[test]
    fn record_returns_none_when_disabled() {
        let headers = Vec::<(String, String)>::new();
        let mut window = TrafficWindow::new();
        let sc = record(
            false,
            &headers,
            &Event {
                node: "node",
                path: "/p",
                status: 200,
                cache_state: "HIT",
                client_ip: "1.2.3.4",
                latency_ns: None,
            },
            &mut window,
        );
        assert!(sc.is_none());
    }

    #[test]
    fn record_mints_a_session_only_when_the_request_has_none() {
        let headers = Vec::<(String, String)>::new(); // no _m6sid cookie present
        let mut window = TrafficWindow::new();
        let sc = record(
            true,
            &headers,
            &Event {
                node: "node",
                path: "/p",
                status: 200,
                cache_state: "HIT",
                client_ip: "1.2.3.4",
                latency_ns: Some(123),
            },
            &mut window,
        );
        assert!(
            sc.is_some(),
            "a request with no existing session must mint one and return its id"
        );
        assert_eq!(sc.unwrap().as_str().len(), 32);
    }

    #[test]
    fn record_reuses_existing_session_no_cookie() {
        let headers = vec![("Cookie".to_string(), "_m6sid=existing-id".to_string())];
        let mut window = TrafficWindow::new();
        let sc = record(
            true,
            &headers,
            &Event {
                node: "node",
                path: "/p",
                status: 200,
                cache_state: "HIT",
                client_ip: "1.2.3.4",
                latency_ns: Some(123),
            },
            &mut window,
        );
        assert!(
            sc.is_none(),
            "a request already carrying a session cookie must not get a new one"
        );
    }

    #[test]
    fn record_h3_and_vec_agree_for_equivalent_requests() {
        // The whole point of HeaderSource: identical logical input through
        // either representation must produce identical mint-or-reuse behavior.
        let vec_headers = vec![("Cookie".to_string(), "_m6sid=shared-id".to_string())];
        let h3_raw = quiche_headers(&[("cookie", "_m6sid=shared-id")]);
        let h3_headers = H3Headers(&h3_raw);
        let mut window = TrafficWindow::new();

        let vec_sc = record(
            true,
            &vec_headers,
            &Event {
                node: "node",
                path: "/p",
                status: 200,
                cache_state: "HIT",
                client_ip: "1.2.3.4",
                latency_ns: None,
            },
            &mut window,
        );
        let h3_sc = record(
            true,
            &h3_headers,
            &Event {
                node: "node",
                path: "/p",
                status: 200,
                cache_state: "HIT",
                client_ip: "1.2.3.4",
                latency_ns: None,
            },
            &mut window,
        );
        assert_eq!(
            vec_sc, None,
            "vec-backed request with existing session should reuse it"
        );
        assert_eq!(
            h3_sc, None,
            "h3-backed request with existing session should reuse it"
        );
    }

    #[test]
    fn finish_response_appends_set_cookie_only_when_minted() {
        let mut resp_headers = vec![(
            "Content-Type".to_string(),
            "text/html; charset=utf-8".to_string(),
        )];
        let req_headers = Vec::<(String, String)>::new();
        let mut window = TrafficWindow::new();
        finish_response(
            true,
            &mut resp_headers,
            &req_headers,
            &Event {
                node: "node",
                path: "/p",
                status: 200,
                cache_state: "HIT",
                client_ip: "1.2.3.4",
                latency_ns: None,
            },
            &mut window,
        );
        assert_eq!(
            resp_headers.len(),
            2,
            "expected exactly one Set-Cookie appended: {resp_headers:?}"
        );
        assert_eq!(resp_headers[1].0, "Set-Cookie");
    }

    #[test]
    fn finish_response_skips_set_cookie_for_non_html() {
        let mut resp_headers = vec![("Content-Type".to_string(), "text/plain".to_string())];
        let req_headers = Vec::<(String, String)>::new();
        let mut window = TrafficWindow::new();
        finish_response(
            true,
            &mut resp_headers,
            &req_headers,
            &Event {
                node: "node",
                path: "/p",
                status: 200,
                cache_state: "HIT",
                client_ip: "1.2.3.4",
                latency_ns: None,
            },
            &mut window,
        );
        assert_eq!(
            resp_headers.len(),
            1,
            "non-HTML responses must not get a session cookie"
        );
    }

    #[test]
    fn finish_response_appends_nothing_when_disabled() {
        let mut resp_headers = vec![("Content-Type".to_string(), "text/plain".to_string())];
        let req_headers = Vec::<(String, String)>::new();
        let mut window = TrafficWindow::new();
        finish_response(
            false,
            &mut resp_headers,
            &req_headers,
            &Event {
                node: "node",
                path: "/p",
                status: 200,
                cache_state: "HIT",
                client_ip: "1.2.3.4",
                latency_ns: None,
            },
            &mut window,
        );
        assert_eq!(
            resp_headers.len(),
            1,
            "disabled analytics must not touch response headers"
        );
    }

    // ── finish_proxied_response(): the multi-hop (cache node → origin) fix ──

    #[test]
    fn session_from_set_cookie_header_extracts_id() {
        let v = session_from_set_cookie_header(
            "_m6sid=abc123; Max-Age=1800; Path=/; HttpOnly; Secure; SameSite=Lax",
        );
        assert_eq!(v, Some("abc123"));
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
            (
                "Set-Cookie".to_string(),
                "_m6sid=upstream-id; Max-Age=1800".to_string(),
            ),
        ];
        assert_eq!(session_from_response_headers(&headers), Some("upstream-id"));
    }

    #[test]
    fn finish_proxied_response_reuses_upstream_session_without_adding_a_second_cookie() {
        // Simulates a cache node's backend response: origin already minted
        // and set _m6sid before this node ever sees the request.
        let mut resp_headers = vec![(
            "Set-Cookie".to_string(),
            "_m6sid=origin-minted-id; Max-Age=1800; Path=/; HttpOnly; Secure; SameSite=Lax"
                .to_string(),
        )];
        let req_headers = Vec::<(String, String)>::new(); // the original client request never had a cookie
        let mut window = TrafficWindow::new();
        finish_proxied_response(
            true,
            &mut resp_headers,
            &req_headers,
            &Event {
                node: "edge-node",
                path: "/p",
                status: 200,
                cache_state: "MISS",
                client_ip: "1.2.3.4",
                latency_ns: None,
            },
            &mut window,
        );

        let set_cookies: Vec<&(String, String)> = resp_headers
            .iter()
            .filter(|(k, _)| k.eq_ignore_ascii_case("set-cookie"))
            .collect();
        assert_eq!(
            set_cookies.len(),
            1,
            "must not add a second Set-Cookie when upstream already set one: {resp_headers:?}"
        );
    }

    #[test]
    fn finish_proxied_response_mints_normally_when_no_upstream_session_present() {
        // A local-socket backend's response (e.g. m6-html) never sets
        // _m6sid — behaves exactly like finish_response in that case.
        let mut resp_headers = vec![("Content-Type".to_string(), "text/html".to_string())];
        let req_headers = Vec::<(String, String)>::new();
        let mut window = TrafficWindow::new();
        finish_proxied_response(
            true,
            &mut resp_headers,
            &req_headers,
            &Event {
                node: "origin",
                path: "/p",
                status: 200,
                cache_state: "MISS",
                client_ip: "1.2.3.4",
                latency_ns: None,
            },
            &mut window,
        );
        assert_eq!(
            resp_headers.len(),
            2,
            "expected exactly one Set-Cookie minted: {resp_headers:?}"
        );
        assert_eq!(resp_headers[1].0, "Set-Cookie");
    }

    #[test]
    fn finish_proxied_response_does_nothing_when_disabled() {
        let mut resp_headers = vec![(
            "Set-Cookie".to_string(),
            "_m6sid=origin-minted-id; Max-Age=1800".to_string(),
        )];
        let req_headers = Vec::<(String, String)>::new();
        let mut window = TrafficWindow::new();
        finish_proxied_response(
            false,
            &mut resp_headers,
            &req_headers,
            &Event {
                node: "edge-node",
                path: "/p",
                status: 200,
                cache_state: "MISS",
                client_ip: "1.2.3.4",
                latency_ns: None,
            },
            &mut window,
        );
        assert_eq!(
            resp_headers.len(),
            1,
            "disabled analytics must not touch response headers even to dedupe"
        );
    }

    // ── The three lines mean what the golden lines mean ─────────────────────

    /// The site's readers filter on `message` and read these fields by name, so
    /// the analytics lines are a format specification. `m6-log/tests/golden`
    /// holds one real line of each shape, captured from example 01's running
    /// stack before m6-log existed.
    ///
    /// Each line is written through the same builder the request path uses, into
    /// a file of its own, and its parsed `level` and `fields` are compared with
    /// the golden file. A renamed key, a value against the wrong key or a lost
    /// field fails here. The timestamp differs by design, and is not compared.
    #[test]
    fn the_analytics_lines_mean_what_the_golden_lines_mean() {
        let dir = tempfile::tempdir().expect("a directory");
        let golden =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../m6-log/tests/golden");

        let request = dir.path().join("request.jsonl");
        let mut sink = Sink::file(&request).expect("the line file");
        write_request_line(
            &mut sink,
            &Event {
                node: "dev",
                path: "/missing",
                status: 404,
                cache_state: "MISS",
                client_ip: "192.0.2.1",
                latency_ns: Some(342_834),
            },
            "0f1e2d3c4b5a69788796a5b4c3d2e1f0",
            true,
            &RequestFeatures {
                referer_host: None,
                referer_path: None,
                user_agent: Some("curl/8.7.1"),
            },
        );
        sink.flush();
        assert_line_means(&golden, "request", &request);

        let monitor = dir.path().join("monitor.jsonl");
        let mut sink = Sink::file(&monitor).expect("the line file");
        write_monitor_line(
            &mut sink,
            "dev",
            "/health",
            200,
            "192.0.2.1",
            Some("curl/8.7.1"),
            None,
        );
        sink.flush();
        assert_line_means(&golden, "monitor", &monitor);

        let rate_limited = dir.path().join("rate_limited.jsonl");
        let mut sink = Sink::file(&rate_limited).expect("the line file");
        write_rate_limited_line(
            &mut sink,
            "dev",
            "/missing",
            "192.0.2.1",
            Some("curl/8.7.1"),
        );
        sink.flush();
        assert_line_means(&golden, "rate_limited", &rate_limited);
    }

    /// Compare one written line's level and fields with its golden file's.
    fn assert_line_means(golden_dir: &std::path::Path, name: &str, written: &std::path::Path) {
        let golden_text = std::fs::read_to_string(golden_dir.join(format!("{name}.jsonl")))
            .expect("the golden file");
        let golden: serde_json::Value =
            serde_json::from_str(golden_text.trim()).expect("golden JSON");
        let text = std::fs::read_to_string(written).expect("the written line");
        let ours: serde_json::Value = serde_json::from_str(text.trim()).expect("written JSON");
        assert_eq!(ours["level"], golden["level"], "{name}: level");
        assert_eq!(
            ours["fields"], golden["fields"],
            "{name}: the line's fields do not mean what the golden line means"
        );
    }
}
