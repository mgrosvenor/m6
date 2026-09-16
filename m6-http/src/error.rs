/// Error mode handling: status, internal, custom.
use crate::config::ErrorsConfig;

/// Generate an error response based on the configured mode.
pub enum ErrorMode {
    Status,
    Internal,
    Custom { path: String },
}

impl ErrorMode {
    pub fn from_config(errors: &ErrorsConfig) -> Self {
        match errors.mode.as_str() {
            "status" => ErrorMode::Status,
            "custom" => {
                if let Some(ref p) = errors.path {
                    ErrorMode::Custom { path: p.clone() }
                } else {
                    ErrorMode::Internal
                }
            }
            _ => ErrorMode::Internal,
        }
    }
}

/// Diagnostic context for verbose error pages.
pub struct ErrorContext {
    /// Matched route pattern (e.g. "/blog/{stem}"), or None if no route matched.
    pub route: Option<String>,
    /// Backend that was (or would have been) invoked (e.g. "m6-html").
    pub backend: Option<String>,
    /// Specific error detail (e.g. token parse error, require clause).
    pub detail: Option<String>,
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// (detail, hint) for verbose internal error pages.
pub fn internal_error_detail(status: u16) -> (&'static str, Option<&'static str>) {
    match status {
        400 => (
            "The server could not understand the request — malformed syntax or invalid parameters.",
            Some("Check the URL and any form data you submitted."),
        ),
        401 => (
            "This page requires a valid login session.",
            Some("Log in and try again."),
        ),
        403 => (
            "Your account does not have permission to access this resource.",
            Some("If you believe this is a mistake, contact the site administrator."),
        ),
        404 => (
            "The page or resource you requested does not exist.",
            Some("Check the URL for typos, or use the navigation above."),
        ),
        405 => (
            "The HTTP method used is not supported for this URL.",
            Some("This is likely a bug — please report it."),
        ),
        500 => (
            "The server encountered an unexpected error while processing your request.",
            Some("Check the server logs for details."),
        ),
        502 => (
            "A backend service failed to respond or returned an invalid response.",
            Some("Check that all backend services are running."),
        ),
        503 => (
            "The service is temporarily unavailable — it may be starting up or overloaded.",
            Some("Wait a moment and try again."),
        ),
        504 => (
            "A backend service took too long to respond.",
            Some("Check backend service health and resource usage."),
        ),
        _ => ("An unexpected error occurred.", None),
    }
}

/// Generate HTML for internal error mode.
/// When `verbose` is true, includes descriptive detail, hints, request path, and diagnostic context.
pub fn internal_error_html(
    status: u16,
    reason: &str,
    verbose: bool,
    path: &str,
    ctx: Option<&ErrorContext>,
) -> Vec<u8> {
    if verbose {
        let (detail, hint) = internal_error_detail(status);
        let hint_html = hint
            .map(|h| format!("<p><em>{h}</em></p>"))
            .unwrap_or_default();

        let debug_html = {
            let route = ctx.and_then(|c| c.route.as_deref()).unwrap_or("—");
            let backend = ctx.and_then(|c| c.backend.as_deref()).unwrap_or("—");
            let err_det = ctx.and_then(|c| c.detail.as_deref()).unwrap_or("—");
            format!(
                "<hr><table style='font:13px monospace;border-collapse:collapse'>\
                <tr><td style='padding:2px 12px 2px 0;color:#888'>path</td><td><code>{}</code></td></tr>\
                <tr><td style='padding:2px 12px 2px 0;color:#888'>route</td><td><code>{}</code></td></tr>\
                <tr><td style='padding:2px 12px 2px 0;color:#888'>backend</td><td><code>{}</code></td></tr>\
                <tr><td style='padding:2px 12px 2px 0;color:#888'>cache</td><td><code>miss</code></td></tr>\
                <tr><td style='padding:2px 12px 2px 0;color:#888'>detail</td><td><code>{}</code></td></tr>\
                </table>",
                html_escape(path), html_escape(route), html_escape(backend), html_escape(err_det)
            )
        };

        format!(
            "<!DOCTYPE html><html><head><title>{status} {reason}</title></head>\
            <body><h1>{status} {reason}</h1><p>{detail}</p>{hint_html}{debug_html}</body></html>"
        )
        .into_bytes()
    } else {
        // The status and nothing else. This used to print "m6-http" here, which
        // told a visitor nothing and told a scanner what was serving: on a fleet
        // where the origin routes unknown paths to an HTML backend and the cache
        // nodes do not, every probe for /.env at an edge was answered with the
        // name of the software that refused it. A production error page should
        // not identify its implementation. Verbose mode still carries detail,
        // because that is for a developer reading it, not a stranger.
        format!(
            "<!DOCTYPE html><html><head><title>{status} {reason}</title></head>\
            <body><h1>{status} {reason}</h1></body></html>"
        )
        .into_bytes()
    }
}

/// Build a simple error response given mode and status.
pub fn make_error_response(
    status: u16,
    mode: &ErrorMode,
    _from_path: &str,
) -> (u16, Vec<(String, String)>, Vec<u8>) {
    match mode {
        ErrorMode::Status => (
            status,
            vec![("Content-Type".to_string(), "text/plain".to_string())],
            vec![],
        ),
        ErrorMode::Internal => {
            let reason = status_reason(status);
            let body = internal_error_html(status, reason, false, _from_path, None);
            (
                status,
                vec![(
                    "Content-Type".to_string(),
                    "text/html; charset=utf-8".to_string(),
                )],
                body,
            )
        }
        ErrorMode::Custom { .. } => {
            // Return a marker indicating we need to fetch from the error backend.
            // The caller handles the actual fetch.
            // For now return empty — caller will detect Custom mode and fetch.
            (status, vec![], vec![])
        }
    }
}

/// Headers that must survive an error-page substitution.
///
/// When a backend answers 4xx or 5xx, `apply_error_mode` replaces the whole
/// response with this server's own error page, which throws the backend's header
/// block away. For nearly every header that is right: they describe a body that
/// is no longer being sent.
///
/// These four are not about the body at all. They tell the client what to do
/// next, and three of them are required:
///
///   - `WWW-Authenticate`   MUST be sent on 401 (RFC 9110 11.6.1)
///   - `Proxy-Authenticate` MUST be sent on 407 (RFC 9110 11.7.1)
///   - `Allow`              MUST be sent on 405 (RFC 9110 10.2.1)
///   - `Retry-After`        says when to come back (RFC 9110 10.2.3)
///
/// Dropping the first three turns a conformant backend response into a
/// non-conformant proxy response, and the client cannot recover because the one
/// header telling it how is the one that went missing.
///
/// Found by the CMS example's end-to-end test. m6-auth-server answers a
/// throttled login with `429` and `Retry-After: 60`; what reached the client was
/// a generic "An unexpected error occurred" page carrying no `Retry-After`, so
/// nothing downstream could know when to try again. The throttle worked and was
/// unusable.
pub const PRESERVED_ERROR_HEADERS: &[&str] = &[
    "retry-after",
    "www-authenticate",
    "proxy-authenticate",
    "allow",
];

/// Copy the actionable headers from a backend's error response onto the error
/// page that replaces it.
///
/// Anything the error page set for itself wins: this fills gaps and never
/// overwrites, so a proxy-generated `Retry-After` is not replaced by a
/// backend's.
pub fn preserve_actionable_headers(
    backend_headers: &[(String, String)],
    page_headers: &mut Vec<(String, String)>,
) {
    for name in PRESERVED_ERROR_HEADERS {
        if page_headers
            .iter()
            .any(|(k, _)| k.eq_ignore_ascii_case(name))
        {
            continue;
        }
        if let Some((k, v)) = backend_headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
        {
            page_headers.push((k.clone(), v.clone()));
        }
    }
}

pub fn status_reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        301 => "Moved Permanently",
        302 => "Found",
        304 => "Not Modified",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        408 => "Request Timeout",
        409 => "Conflict",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => "Unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ErrorsConfig;

    #[test]
    fn test_internal_mode_returns_html() {
        let errors = ErrorsConfig {
            mode: "internal".to_string(),
            path: None,
            verbose_fallback: false,
        };
        let mode = ErrorMode::from_config(&errors);
        let (status, _headers, body) = make_error_response(404, &mode, "/missing");
        assert_eq!(status, 404);
        let body_str = std::str::from_utf8(&body).unwrap();
        assert!(body_str.contains("404"));
        assert!(body_str.contains("Not Found"));
    }

    #[test]
    fn test_status_mode_returns_empty_body() {
        let errors = ErrorsConfig {
            mode: "status".to_string(),
            path: None,
            verbose_fallback: false,
        };
        let mode = ErrorMode::from_config(&errors);
        let (status, _, body) = make_error_response(503, &mode, "/");
        assert_eq!(status, 503);
        assert!(body.is_empty());
    }

    #[test]
    fn test_status_reason() {
        assert_eq!(status_reason(200), "OK");
        assert_eq!(status_reason(404), "Not Found");
        assert_eq!(status_reason(503), "Service Unavailable");
    }

    #[test]
    fn test_custom_mode_without_path_falls_back_to_internal() {
        // When mode = "custom" but no path given, from_config falls back to Internal.
        let errors = ErrorsConfig {
            mode: "custom".to_string(),
            path: None,
            verbose_fallback: false,
        };
        let mode = ErrorMode::from_config(&errors);
        // Should behave like Internal
        let (status, _headers, body) = make_error_response(404, &mode, "/missing");
        assert_eq!(status, 404);
        // Internal mode returns empty vec from make_error_response when custom path is set
        // (the actual fetch is done by apply_error_mode in main.rs); here path=None means Internal fallback
        let body_str = std::str::from_utf8(&body).unwrap();
        assert!(body_str.contains("404"));
    }

    #[test]
    fn test_custom_mode_with_path_returns_empty_placeholder() {
        // Custom mode with a path set: make_error_response returns placeholder (empty body/headers).
        // The real fetch is done by apply_error_mode in main.rs.
        let errors = ErrorsConfig {
            mode: "custom".to_string(),
            path: Some("/_errors".to_string()),
            verbose_fallback: false,
        };
        let mode = ErrorMode::from_config(&errors);
        let (status, _headers, body) = make_error_response(404, &mode, "/missing");
        assert_eq!(status, 404);
        // Placeholder is empty
        assert!(body.is_empty());
    }

    #[test]
    fn test_internal_error_html_contains_status_and_reason() {
        let html = internal_error_html(503, "Service Unavailable", false, "/test", None);
        let s = std::str::from_utf8(&html).unwrap();
        assert!(s.contains("503"));
        assert!(s.contains("Service Unavailable"));
        assert!(s.starts_with("<!DOCTYPE html>"));
    }

    /// A production error page must not name the software serving it.
    ///
    /// The body used to be `<h1>404 Not Found</h1><p>m6-http</p>`. Harmless on an
    /// origin, where a missing path is usually routed to a backend that renders the
    /// site's own page, and a disclosure on a cache node, where nothing matches an
    /// arbitrary path and this page is what answers every probe for /.env.
    ///
    /// The test above passed throughout, because it asserted only what the body
    /// SHOULD contain and never what it should not. That is the gap this closes: a
    /// page can be correct about the status and still say too much.
    #[test]
    fn a_production_error_page_does_not_name_the_software() {
        for status in [400u16, 404, 500, 503] {
            let html = internal_error_html(status, status_reason(status), false, "/x", None);
            let s = std::str::from_utf8(&html).unwrap();
            assert!(s.contains(&status.to_string()), "{status}: lost the status");
            assert!(
                !s.to_lowercase().contains("m6"),
                "{status} error page names the software: {s}"
            );
        }
    }

    /// Verbose mode is for a developer reading it, so it keeps its detail. It is
    /// documented as dev-only and defaults to off.
    #[test]
    fn verbose_mode_still_explains_the_status() {
        let html = internal_error_html(503, "Service Unavailable", true, "/x", None);
        let s = std::str::from_utf8(&html).unwrap();
        assert!(s.contains("503"));
        // Something more than the bare status: the verbose body carries a
        // description and a debug table.
        assert!(
            s.len() > 200,
            "verbose body is no richer than the plain one"
        );
    }

    // ── Headers that survive an error-page substitution ──────────────────────
    //
    // The defect these cover: the CMS example's throttled login answered 429
    // with `Retry-After: 60` from m6-auth-server, and the client received the
    // proxy's generic error page with no `Retry-After` on it at all.

    fn h(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn value_of(headers: &[(String, String)], name: &str) -> Option<String> {
        headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.clone())
    }

    #[test]
    fn retry_after_survives_the_error_page() {
        let backend = h(&[("Retry-After", "60"), ("Content-Type", "application/json")]);
        let mut page = h(&[("Content-Type", "text/html; charset=utf-8")]);
        preserve_actionable_headers(&backend, &mut page);
        assert_eq!(value_of(&page, "retry-after").as_deref(), Some("60"));
    }

    /// The backend's Content-Type describes a body that is no longer being sent,
    /// so it must NOT come across: the page is HTML, not the backend's JSON.
    #[test]
    fn headers_describing_the_discarded_body_do_not_survive() {
        let backend = h(&[("Content-Type", "application/json"), ("ETag", "\"abc\"")]);
        let mut page = h(&[("Content-Type", "text/html; charset=utf-8")]);
        preserve_actionable_headers(&backend, &mut page);
        assert_eq!(
            value_of(&page, "content-type").as_deref(),
            Some("text/html; charset=utf-8")
        );
        assert!(value_of(&page, "etag").is_none());
    }

    /// RFC 9110 11.6.1: a 401 without WWW-Authenticate is not a conformant
    /// response, so a proxy that drops it makes a correct backend incorrect.
    #[test]
    fn www_authenticate_survives_on_401() {
        let backend = h(&[("WWW-Authenticate", "Bearer realm=\"api\"")]);
        let mut page = Vec::new();
        preserve_actionable_headers(&backend, &mut page);
        assert_eq!(
            value_of(&page, "www-authenticate").as_deref(),
            Some("Bearer realm=\"api\"")
        );
    }

    /// RFC 9110 10.2.1: Allow is required on 405.
    #[test]
    fn allow_survives_on_405() {
        let backend = h(&[("Allow", "GET, HEAD, POST")]);
        let mut page = Vec::new();
        preserve_actionable_headers(&backend, &mut page);
        assert_eq!(value_of(&page, "allow").as_deref(), Some("GET, HEAD, POST"));
    }

    #[test]
    fn proxy_authenticate_survives_on_407() {
        let backend = h(&[("Proxy-Authenticate", "Basic realm=\"proxy\"")]);
        let mut page = Vec::new();
        preserve_actionable_headers(&backend, &mut page);
        assert!(value_of(&page, "proxy-authenticate").is_some());
    }

    /// The page's own value wins. A proxy that set its own Retry-After knows
    /// something the backend does not, so it is not overwritten.
    #[test]
    fn the_error_page_wins_where_it_set_the_header_itself() {
        let backend = h(&[("Retry-After", "60")]);
        let mut page = h(&[("Retry-After", "5")]);
        preserve_actionable_headers(&backend, &mut page);
        assert_eq!(value_of(&page, "retry-after").as_deref(), Some("5"));
        assert_eq!(
            page.iter()
                .filter(|(k, _)| k.eq_ignore_ascii_case("retry-after"))
                .count(),
            1,
            "no duplicate Retry-After"
        );
    }

    /// Header names are case-insensitive (RFC 9110 5.1), and backends do not
    /// agree on the case they send.
    #[test]
    fn matching_ignores_header_case() {
        let backend = h(&[("rEtRy-AfTeR", "30")]);
        let mut page = Vec::new();
        preserve_actionable_headers(&backend, &mut page);
        assert_eq!(value_of(&page, "retry-after").as_deref(), Some("30"));
    }

    /// A connection failure leaves no backend headers at all, which must be a
    /// no-op rather than a panic.
    #[test]
    fn no_backend_headers_is_a_no_op() {
        let mut page = h(&[("Content-Type", "text/html")]);
        let before = page.len();
        preserve_actionable_headers(&[], &mut page);
        assert_eq!(page.len(), before);
    }
}
