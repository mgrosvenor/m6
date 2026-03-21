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

/// (detail, hint) for verbose internal error pages.
pub fn internal_error_detail(status: u16) -> (&'static str, Option<&'static str>) {
    match status {
        400 => ("The server could not understand the request — malformed syntax or invalid parameters.", Some("Check the URL and any form data you submitted.")),
        401 => ("This page requires a valid login session.", Some("Log in and try again.")),
        403 => ("Your account does not have permission to access this resource.", Some("If you believe this is a mistake, contact the site administrator.")),
        404 => ("The page or resource you requested does not exist.", Some("Check the URL for typos, or use the navigation above.")),
        405 => ("The HTTP method used is not supported for this URL.", Some("This is likely a bug — please report it.")),
        500 => ("The server encountered an unexpected error while processing your request.", Some("Check the server logs for details.")),
        502 => ("A backend service failed to respond or returned an invalid response.", Some("Check that all backend services are running.")),
        503 => ("The service is temporarily unavailable — it may be starting up or overloaded.", Some("Wait a moment and try again.")),
        504 => ("A backend service took too long to respond.", Some("Check backend service health and resource usage.")),
        _ => ("An unexpected error occurred.", None),
    }
}

/// Generate HTML for internal error mode.
/// When `verbose` is true, includes descriptive detail, hints, and the request path.
pub fn internal_error_html(status: u16, reason: &str, verbose: bool, path: &str) -> Vec<u8> {
    if verbose {
        let (detail, hint) = internal_error_detail(status);
        let hint_html = hint.map(|h| format!("<p><em>{h}</em></p>")).unwrap_or_default();
        let path_html = if !path.is_empty() {
            format!("<p><small>Path: <code>{path}</code></small></p>")
        } else {
            String::new()
        };
        format!(
            "<!DOCTYPE html><html><head><title>{status} {reason}</title></head>\
            <body><h1>{status} {reason}</h1><p>{detail}</p>{hint_html}{path_html}</body></html>"
        )
        .into_bytes()
    } else {
        format!(
            "<!DOCTYPE html><html><head><title>{status} {reason}</title></head>\
            <body><h1>{status} {reason}</h1><p>m6-http</p></body></html>"
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
        ErrorMode::Status => {
            (status, vec![("Content-Type".to_string(), "text/plain".to_string())], vec![])
        }
        ErrorMode::Internal => {
            let reason = status_reason(status);
            let body = internal_error_html(status, reason, false, _from_path);
            (
                status,
                vec![("Content-Type".to_string(), "text/html; charset=utf-8".to_string())],
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
        let errors = ErrorsConfig { mode: "internal".to_string(), path: None, verbose_fallback: false };
        let mode = ErrorMode::from_config(&errors);
        let (status, headers, body) = make_error_response(404, &mode, "/missing");
        assert_eq!(status, 404);
        let body_str = std::str::from_utf8(&body).unwrap();
        assert!(body_str.contains("404"));
        assert!(body_str.contains("Not Found"));
    }

    #[test]
    fn test_status_mode_returns_empty_body() {
        let errors = ErrorsConfig { mode: "status".to_string(), path: None, verbose_fallback: false };
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
        let errors = ErrorsConfig { mode: "custom".to_string(), path: None, verbose_fallback: false };
        let mode = ErrorMode::from_config(&errors);
        // Should behave like Internal
        let (status, headers, body) = make_error_response(404, &mode, "/missing");
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
        let errors = ErrorsConfig { mode: "custom".to_string(), path: Some("/_errors".to_string()), verbose_fallback: false };
        let mode = ErrorMode::from_config(&errors);
        let (status, _headers, body) = make_error_response(404, &mode, "/missing");
        assert_eq!(status, 404);
        // Placeholder is empty
        assert!(body.is_empty());
    }

    #[test]
    fn test_internal_error_html_contains_status_and_reason() {
        let html = internal_error_html(503, "Service Unavailable", false, "/test");
        let s = std::str::from_utf8(&html).unwrap();
        assert!(s.contains("503"));
        assert!(s.contains("Service Unavailable"));
        assert!(s.starts_with("<!DOCTYPE html>"));
    }
}
