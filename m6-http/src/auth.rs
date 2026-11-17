//! Credential extraction and the one `POST /check` request's bytes.
//!
//! No token is decoded here. `m6-auth-server` answers `/check`, and the edge
//! forwards the identity it gets back.

/// How long a `require` route waits for `POST /check` before answering 503.
///
/// The measured parts of one check add to about 111 µs, so this fires only on
/// an authority that has stopped answering. A page carrying one protected
/// request stalls at most this long on a refused credential.
pub const CHECK_DEADLINE: std::time::Duration = std::time::Duration::from_millis(50);

/// Reconstruct the logical `Cookie` header from a request's header list.
///
/// HTTP/2 (and HTTP/3) clients are RFC-compliant to send multiple separate
/// `cookie` header *fields* instead of one combined `a=1; b=2` field (RFC
/// 7540 §8.1.2.5 — it's specifically recommended for better HPACK/QPACK
/// compression, since each crumb can then be referenced independently from
/// the dynamic table). A `.find()` for "the cookie header" only ever sees
/// the first such field and silently drops every cookie after it — the
/// exact bug this was: `curl` always sends one pre-joined field so it never
/// tripped this, but a browser splitting cookies across fields could make
/// an auth cookie in a later field invisible to a naive single-field
/// lookup. The spec requires the receiver to join multiple fields with
/// `"; "` before treating it as a single logical header — that's this.
pub fn combined_cookie_header(
    headers: &(impl crate::analytics::HeaderSource + ?Sized),
) -> Option<String> {
    let mut parts = headers.find_all("cookie").peekable();
    parts.peek()?;
    Some(parts.collect::<Vec<_>>().join("; "))
}

/// The most a credential can be and still fit one request header line:
/// [`m6_core::headers::MAX_HEADER_LINE`] counts the field's name and value,
/// and the field and scheme this edge writes are `Authorization: Bearer `.
pub const MAX_CREDENTIAL: usize =
    m6_core::headers::MAX_HEADER_LINE - "Authorization: Bearer ".len();

/// Whether a credential is safe to write into an HTTP/1.1 field value.
///
/// The forward path refuses a field value with CR, LF or NUL through
/// `forward::h1_field_value_is_safe`, but the check request is written by hand
/// before that serializer runs. A credential that would inject a header or a
/// request line is refused here, at the one place a client string enters the
/// hand-built request.
fn credential_is_safe(credential: &str) -> bool {
    credential.len() <= MAX_CREDENTIAL
        && !credential
            .bytes()
            .any(|byte| byte == b'\r' || byte == b'\n' || byte == 0)
}

/// Extract the credential from `Authorization: Bearer`, else the `session`
/// cookie. `None` when neither carries a safe value.
pub fn extract_token<'a>(
    auth_header: Option<&'a str>,
    cookie_header: Option<&'a str>,
) -> Option<&'a str> {
    if let Some(auth) = auth_header {
        if let Some(token) = auth.strip_prefix("Bearer ") {
            let token = token.trim();
            if !token.is_empty() && credential_is_safe(token) {
                return Some(token);
            }
        }
    }
    if let Some(cookies) = cookie_header {
        for cookie in cookies.split(';') {
            let cookie = cookie.trim();
            if let Some(val) = cookie.strip_prefix("session=") {
                let val = val.trim();
                if !val.is_empty() && credential_is_safe(val) {
                    return Some(val);
                }
            }
        }
    }
    None
}

/// The bytes of one `POST /check`, written into `buf`.
///
/// Hand-built: a check is one small request on the socket-backend dispatch, so
/// it costs no `HttpRequest` and no allocation once `buf` has capacity. No
/// credential sends no `Authorization`, which is the start probe's request.
pub fn encode_check_request(buf: &mut Vec<u8>, credential: Option<&str>) {
    buf.clear();
    buf.extend_from_slice(b"POST /check HTTP/1.1\r\nHost: localhost\r\n");
    // The one second line of defence: `extract_token` already refuses an
    // unsafe credential, and this is the place every caller passes through.
    // An unsafe one writes no `Authorization`, so the checker answers 401 and
    // the request is refused rather than a header or a request line injected.
    if let Some(credential) = credential.filter(|credential| credential_is_safe(credential)) {
        buf.extend_from_slice(b"Authorization: Bearer ");
        buf.extend_from_slice(credential.as_bytes());
        buf.extend_from_slice(b"\r\n");
    }
    buf.extend_from_slice(b"Content-Length: 0\r\nConnection: close\r\n\r\n");
}

/// Check if Accept header indicates a browser (text/html).
pub fn is_browser_request(accept: Option<&str>) -> bool {
    accept.map(|a| a.contains("text/html")).unwrap_or(false)
}

/// Extract the `refresh` cookie value if present.
pub fn extract_refresh_cookie(cookie_header: Option<&str>) -> Option<&str> {
    let cookies = cookie_header?;
    for cookie in cookies.split(';') {
        let cookie = cookie.trim();
        if let Some(val) = cookie.strip_prefix("refresh=") {
            let val = val.trim();
            if !val.is_empty() {
                return Some(val);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_token_from_bearer() {
        let token = extract_token(Some("Bearer mytoken123"), None);
        assert_eq!(token, Some("mytoken123"));
    }

    #[test]
    fn test_combined_cookie_header_single_field() {
        let headers = vec![("cookie".to_string(), "a=1; b=2".to_string())];
        assert_eq!(
            combined_cookie_header(&headers),
            Some("a=1; b=2".to_string())
        );
    }

    #[test]
    fn test_combined_cookie_header_split_across_fields() {
        let headers = vec![
            ("cookie".to_string(), "_m6sid=abc".to_string()),
            ("cookie".to_string(), "session=xyz".to_string()),
            ("cookie".to_string(), "refresh=def".to_string()),
        ];
        assert_eq!(
            combined_cookie_header(&headers),
            Some("_m6sid=abc; session=xyz; refresh=def".to_string())
        );
    }

    #[test]
    fn test_combined_cookie_header_absent() {
        let headers = vec![("user-agent".to_string(), "test".to_string())];
        assert_eq!(combined_cookie_header(&headers), None);
    }

    #[test]
    fn test_extract_token_finds_session_when_cookies_split_across_fields() {
        let headers = vec![
            ("cookie".to_string(), "_m6sid=abc".to_string()),
            ("cookie".to_string(), "session=realtoken".to_string()),
        ];
        let combined = combined_cookie_header(&headers);
        let token = extract_token(None, combined.as_deref());
        assert_eq!(token, Some("realtoken"));
    }

    #[test]
    fn test_extract_token_from_cookie() {
        let token = extract_token(None, Some("session=cookietoken; other=val"));
        assert_eq!(token, Some("cookietoken"));
    }

    #[test]
    fn test_bearer_takes_precedence_over_cookie() {
        let token = extract_token(Some("Bearer headertoken"), Some("session=cookietoken"));
        assert_eq!(token, Some("headertoken"));
    }

    #[test]
    fn test_extract_token_none() {
        assert_eq!(extract_token(None, None), None);
        assert_eq!(extract_token(None, Some("other=val")), None);
        assert_eq!(extract_token(Some("Basic abc"), None), None);
    }

    #[test]
    fn a_credential_with_a_line_break_is_not_extracted() {
        assert_eq!(
            extract_token(Some("Bearer a\r\nX-Injected: yes"), None),
            None
        );
        assert_eq!(extract_token(Some("Bearer a\nX-Injected: yes"), None), None);
        assert_eq!(
            extract_token(None, Some("session=a\r\nX-Injected: yes")),
            None
        );
    }

    #[test]
    fn a_credential_with_a_nul_is_not_extracted() {
        assert_eq!(extract_token(Some("Bearer a\0b"), None), None);
        assert_eq!(extract_token(None, Some("session=a\0b")), None);
    }

    #[test]
    fn an_overlong_credential_is_not_extracted() {
        let long = "a".repeat(MAX_CREDENTIAL + 1);
        let header = format!("Bearer {long}");
        assert_eq!(extract_token(Some(&header), None), None);
        let cookie = format!("session={long}");
        assert_eq!(extract_token(None, Some(&cookie)), None);
        let ok = "a".repeat(MAX_CREDENTIAL);
        let header = format!("Bearer {ok}");
        assert_eq!(extract_token(Some(&header), None), Some(ok.as_str()));
    }

    #[test]
    fn an_unsafe_credential_writes_no_authorization_line() {
        let mut buf = Vec::new();
        encode_check_request(&mut buf, Some("a\r\nX-Injected: yes"));
        let text = String::from_utf8_lossy(&buf);
        assert!(!text.contains("Authorization"), "{text}");
        assert!(!text.contains("X-Injected"), "{text}");

        encode_check_request(&mut buf, Some("safe-token"));
        let text = String::from_utf8_lossy(&buf);
        assert_eq!(
            text.matches("Authorization: Bearer safe-token").count(),
            1,
            "{text}"
        );
    }

    #[test]
    fn test_is_browser_request() {
        assert!(is_browser_request(Some("text/html,application/xhtml+xml")));
        assert!(!is_browser_request(Some("application/json")));
        assert!(!is_browser_request(None));
    }

    #[test]
    fn test_extract_refresh_cookie() {
        assert_eq!(
            extract_refresh_cookie(Some("session=abc; refresh=def")),
            Some("def")
        );
        assert_eq!(extract_refresh_cookie(Some("session=abc")), None);
        assert_eq!(extract_refresh_cookie(None), None);
    }
}
