//! Security regression tests for m6-http.
//!
//! Each test asserts the **secure** behaviour for one finding from the security
//! audit. Each one failed when written and passes now that the finding is
//! fixed; they stand as guards against reintroduction.
//!
//! Assertions are deliberately fix-agnostic: they state the property that must
//! hold, not the mechanism, so a future refactor that preserves the property
//! keeps them green.
//!
//! The doc comment on each test describes the original defect, so the reason
//! the test exists survives the fix.

use std::io::{Read, Write};
use std::os::unix::net::UnixListener;
use std::path::Path;

use base64::Engine;

use m6_http_lib::cache::{make_lookup_key, should_cache, Cache, CacheKey, CachedResponse};
use m6_http_lib::forward::{self, HttpRequest};
use m6_http_lib::http11;

// ── Helpers ───────────────────────────────────────────────────────────────────

fn b64(s: &str) -> String {
    base64::engine::general_purpose::STANDARD.encode(s.as_bytes())
}

fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

/// Start a one-shot Unix-socket backend that captures the raw request bytes it
/// receives and replies with a minimal 200. Tests using it send header-only
/// requests, so capture stops at the header terminator.
fn capture_backend(sock_path: &Path) -> std::thread::JoinHandle<Vec<u8>> {
    let listener = UnixListener::bind(sock_path).expect("bind unix socket");
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut buf = Vec::new();
        let mut tmp = [0u8; 4096];
        loop {
            match stream.read(&mut tmp) {
                Ok(0) => break,
                Ok(n) => {
                    buf.extend_from_slice(&tmp[..n]);
                    if find_header_end(&buf).is_some() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok");
        let _ = stream.flush();
        buf
    })
}

/// Values of every header line whose name matches `name`, in wire order.
fn header_values(raw: &str, name: &str) -> Vec<String> {
    let want = format!("{}:", name.to_ascii_lowercase());
    raw.split("\r\n")
        .filter(|line| line.to_ascii_lowercase().starts_with(&want))
        .map(|line| line[line.find(':').unwrap() + 1..].trim().to_string())
        .collect()
}

/// Forward `req` to a throwaway backend and return the raw bytes it received.
fn forward_and_capture(req: &HttpRequest, client_ip: &str) -> String {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("backend.sock");
    let handle = capture_backend(&sock);
    forward::forward_request(&sock, req, client_ip, "example.com").expect("forward");
    String::from_utf8(handle.join().unwrap()).expect("utf8 request")
}

fn get_request(path: &str, headers: Vec<(String, String)>) -> HttpRequest {
    HttpRequest {
        method: "GET".to_string(),
        path: path.to_string(),
        query: None,
        version: "HTTP/1.1".to_string(),
        headers,
        body: vec![],
    }
}

// ── Finding 2: client-supplied X-Auth-Claims reaches the backend ─────────────

/// Original defect: `main.rs` cloned the *client's* headers and appended the
/// verified claims, and `forward.rs` wrote them all out unfiltered. The backend
/// received two `X-Auth-Claims` headers with the attacker's first, and
/// m6-render resolves the header with `.find()` (`m6-render/src/request.rs:39`)
/// — first match wins, so `{"groups":["admins"]}` from the wire took effect.
///
/// The fix strips the header at ingress, so this asserts at that boundary: the
/// parser is the last point where a client-supplied copy can still exist.
///
/// Property: a client-supplied `X-Auth-Claims` must not survive parsing.
#[test]
fn finding_2_forged_x_auth_claims_must_not_survive_ingress() {
    let forged = b64(r#"{"sub":"admin","groups":["admins"],"roles":["admin"]}"#);
    let raw = format!(
        "GET /admin HTTP/1.1\r\nHost: example.com\r\nX-Auth-Claims: {forged}\r\n\r\n"
    );

    // Parse, then strip: the two calls m6-http's ingress makes.
    //
    // Stripping used to happen inside the parser, so this test proved the
    // property by parsing alone. The parser is shared with every backend now
    // and is pure -- a backend has no proxy headers to strip -- so the policy
    // moved to the ingress caller.
    //
    // That makes this test weaker than it was: it proves the two functions
    // work together, not that ingress calls them.
    // `forged_x_auth_claims_does_not_survive_ingress_e2e` in security_e2e.rs
    // covers the real path, through the running server, and is the guard that
    // would fail if the call were dropped from drive_h1.
    let mut req = match http11::parse_request(raw.as_bytes()) {
        http11::ParseResult::Complete(r) => r,
        _ => panic!("expected a complete parse of:\n{raw}"),
    };
    m6_http_lib::forward::strip_untrusted_inbound(&mut req.headers);

    assert!(
        !req.headers
            .iter()
            .any(|(k, _)| k.eq_ignore_ascii_case("x-auth-claims")),
        "forged claims survived ingress: {:?}",
        req.headers
    );
}

/// Every proxy-owned header, not just claims — `x-forwarded-*` and `x-real-ip`
/// are equally assertions only the edge can make truthfully.
///
/// Property: no client-supplied copy of any proxy-owned header survives.
#[test]
fn finding_2b_all_proxy_owned_headers_stripped_at_ingress() {
    let raw = "GET /public HTTP/1.1\r\n\
               Host: example.com\r\n\
               X-Auth-Claims: forged\r\n\
               X-Forwarded-For: 10.0.0.99\r\n\
               X-Forwarded-Proto: http\r\n\
               X-Forwarded-Host: evil.com\r\n\
               X-Real-IP: 10.0.0.99\r\n\
               User-Agent: probe\r\n\r\n";

    // Parse, then strip: see the note on the test above. The end-to-end guard
    // is `forged_x_auth_claims_does_not_survive_ingress_e2e` in security_e2e.rs.
    let mut req = match http11::parse_request(raw.as_bytes()) {
        http11::ParseResult::Complete(r) => r,
        _ => panic!("expected a complete parse"),
    };
    m6_http_lib::forward::strip_untrusted_inbound(&mut req.headers);

    for banned in forward::UNTRUSTED_INBOUND {
        assert!(
            !req.headers.iter().any(|(k, _)| k.eq_ignore_ascii_case(banned)),
            "`{banned}` survived ingress: {:?}",
            req.headers
        );
    }
    // Ordinary headers must be untouched.
    assert!(
        req.headers
            .iter()
            .any(|(k, v)| k.eq_ignore_ascii_case("user-agent") && v == "probe"),
        "stripping must not disturb ordinary headers: {:?}",
        req.headers
    );
}

/// The counterpart to the strip: once m6-http has verified a JWT it appends its
/// *own* `X-Auth-Claims`, and that copy must reach the renderer — otherwise
/// stripping would have broken authentication instead of securing it.
///
/// Property: proxy-added claims are forwarded intact.
#[test]
fn finding_2c_proxy_verified_claims_still_reach_backend() {
    let verified = b64(r#"{"sub":"alice","groups":["users"],"roles":[]}"#);
    let req = get_request(
        "/admin",
        vec![("X-Auth-Claims".to_string(), verified.clone())],
    );

    let raw = forward_and_capture(&req, "203.0.113.9");

    assert_eq!(
        header_values(&raw, "x-auth-claims"),
        vec![verified],
        "the renderer must receive exactly the proxy's verified claims:\n{raw}"
    );
}

// ── Finding 5: cache key drops the query string; Vary ignored ────────────────

/// `cache.rs:31` and `:65` truncate the key at `?`, so every query variant of a
/// path shares one entry and the first request's body is replayed to everyone.
///
/// Property: a different query string must not read another query's entry.
/// Fix-agnostic — satisfied either by including the query in the key or by
/// declining to cache query-bearing responses.
#[test]
fn finding_5_distinct_query_strings_must_not_share_a_cache_entry() {
    let cache = Cache::new();

    // Attacker seeds the entry.
    cache.insert(
        CacheKey::new("/search", Some("q=attacker"), ""),
        CachedResponse {
            status: 200,
            headers: std::sync::Arc::new(vec![(
                "cache-control".to_string(),
                "public".to_string(),
            )]),
            body: bytes::Bytes::from_static(b"ATTACKER CONTROLLED"),
            hints: std::sync::Arc::new(vec![]),
        },
    );

    // Victim asks for a different query.
    let mut buf = [0u8; 512];
    let victim_key = make_lookup_key("/search", Some("q=victim"), "", &mut buf);

    assert!(
        cache.get(victim_key).is_none(),
        "the victim's lookup hit the attacker's cached entry for a different query"
    );
}

/// `should_cache` (`cache.rs:172`) never inspects `Vary`, so a response that
/// explicitly varies per user is cached under a key that ignores the header it
/// varies on.
///
/// Property: a response varying on a per-user header must not be cached.
#[test]
fn finding_5b_responses_that_vary_per_user_must_not_be_cached() {
    let varies_on_cookie = vec![
        ("Cache-Control".to_string(), "public".to_string()),
        ("Vary".to_string(), "Cookie".to_string()),
    ];

    assert!(
        !should_cache(200, &varies_on_cookie),
        "a response varying on Cookie was admitted to a cache keyed only on \
         (path, encoding), so it will be replayed across users"
    );
}

// ── Finding 6: X-Forwarded-For is spoofable ──────────────────────────────────

/// `forward.rs:119` appends the real peer IP *after* the client's own headers.
/// m6-auth-server reads it with `m6-core`'s first-match `.find()`
/// (`m6-core/src/http.rs:38`) at `m6-auth-server/src/main.rs:238`, so the
/// forged value wins and login brute-force protection can be evaded by
/// rotating the header.
///
/// Property: the backend must see exactly one `X-Forwarded-For`, containing
/// the real peer IP.
#[test]
fn finding_6_forged_x_forwarded_for_must_not_reach_backend() {
    let req = get_request(
        "/auth/login",
        vec![("X-Forwarded-For".to_string(), "10.0.0.99".to_string())],
    );

    let raw = forward_and_capture(&req, "203.0.113.9");
    let values = header_values(&raw, "x-forwarded-for");

    assert_eq!(
        values,
        vec!["203.0.113.9".to_string()],
        "per-IP rate limiting keys on attacker-controlled input. \
         Forwarded request:\n{raw}"
    );
}

// ── Finding 7: conflicting framing headers are relayed ───────────────────────

/// The proxy forwards every client header verbatim, so a request carrying two
/// conflicting `Content-Length` values reaches the backend with both intact.
/// m6-http resolves the ambiguity by taking the first (`http11.rs:495`); a
/// backend may resolve it differently.
///
/// Property: a request must never be relayed with ambiguous framing. (A fix
/// that rejects such requests at the edge never reaches this code path, which
/// also satisfies the assertion.)
#[test]
fn finding_7_conflicting_content_length_must_not_be_relayed() {
    let req = get_request(
        "/upload",
        vec![
            ("Content-Length".to_string(), "100".to_string()),
            ("Content-Length".to_string(), "0".to_string()),
        ],
    );

    let raw = forward_and_capture(&req, "203.0.113.9");
    let values = header_values(&raw, "content-length");

    assert!(
        values.len() <= 1,
        "backend received {} Content-Length headers ({:?}); conflicting framing \
         should be rejected at the edge. Forwarded request:\n{raw}",
        values.len(),
        values
    );
}

/// Defence in depth for the same finding, at the ingress boundary: ambiguous
/// framing is refused outright rather than resolved by picking a winner and
/// hoping the backend picks the same one.
///
/// Property: conflicting `Content-Length`, or `Content-Length` alongside
/// `Transfer-Encoding`, must not parse.
#[test]
fn finding_7b_ambiguous_framing_must_be_rejected_at_ingress() {
    let ambiguous = [
        (
            "conflicting content-length",
            "POST /upload HTTP/1.1\r\nHost: x\r\nContent-Length: 100\r\nContent-Length: 0\r\n\r\n",
        ),
        (
            "content-length + transfer-encoding",
            "POST /upload HTTP/1.1\r\nHost: x\r\nContent-Length: 5\r\n\
             Transfer-Encoding: chunked\r\n\r\n0\r\n\r\n",
        ),
        (
            "chunked body we never decode",
            "POST /upload HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\n\r\n\
             5\r\nhello\r\n0\r\n\r\n",
        ),
    ];

    for (label, raw) in ambiguous {
        assert!(
            matches!(http11::parse_request(raw.as_bytes()), http11::ParseResult::Error),
            "{label}: should be rejected, but parsed"
        );
    }
}

/// Guards against over-correction: agreeing duplicate `Content-Length` headers
/// are redundant but unambiguous, and a normal single-header request must of
/// course still work.
#[test]
fn finding_7c_unambiguous_framing_still_accepted() {
    let ok = [
        "POST /upload HTTP/1.1\r\nHost: x\r\nContent-Length: 5\r\n\r\nhello",
        "POST /upload HTTP/1.1\r\nHost: x\r\nContent-Length: 5\r\nContent-Length: 5\r\n\r\nhello",
        "GET /page HTTP/1.1\r\nHost: x\r\n\r\n",
    ];

    for raw in ok {
        assert!(
            matches!(
                http11::parse_request(raw.as_bytes()),
                http11::ParseResult::Complete(_)
            ),
            "should parse cleanly:\n{raw}"
        );
    }
}
