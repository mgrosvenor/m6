/// Minimal HTTP/1.1 types shared across all m6 processes.

/// HTTP method constants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Method(pub &'static str);

impl Method {
    pub const GET: Method = Method("GET");
    pub const POST: Method = Method("POST");
    pub const PUT: Method = Method("PUT");
    pub const DELETE: Method = Method("DELETE");
    pub const PATCH: Method = Method("PATCH");
    pub const HEAD: Method = Method("HEAD");
}

impl std::fmt::Display for Method {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Raw parsed HTTP request (before building the request dictionary).
#[derive(Debug, Clone)]
pub struct RawRequest {
    pub method: String,
    pub path: String,
    pub query: Option<String>, // raw query string (without ?)
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl RawRequest {
    /// Look up a header by name (case-insensitive).
    pub fn header(&self, name: &str) -> Option<&str> {
        let name_lower = name.to_ascii_lowercase();
        self.headers
            .iter()
            .find(|(k, _)| k.to_ascii_lowercase() == name_lower)
            .map(|(_, v)| v.as_str())
    }

    /// Return the Content-Type header value if present.
    pub fn content_type(&self) -> Option<&str> {
        self.header("content-type")
    }

    /// Return the Accept header value if present.
    pub fn accept(&self) -> Option<&str> {
        self.header("accept")
    }
}

/// HTTP response to be serialized to wire format.
#[derive(Debug, Clone)]
pub struct RawResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl RawResponse {
    /// Create a new response with the given status and no headers or body.
    pub fn new(status: u16) -> Self {
        RawResponse {
            status,
            headers: Vec::new(),
            body: Vec::new(),
        }
    }

    /// Add a header (builder pattern).
    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    /// Set the response body (builder pattern).
    pub fn body(mut self, body: impl Into<Vec<u8>>) -> Self {
        self.body = body.into();
        self
    }

    /// Set the Content-Type header (builder pattern).
    pub fn content_type(self, ct: &str) -> Self {
        self.header("Content-Type", ct)
    }

    /// Serialize to HTTP/1.1 wire format.
    pub fn to_bytes(&self) -> Vec<u8> {
        let reason = status_reason(self.status);
        let mut out = Vec::new();
        out.extend_from_slice(
            format!("HTTP/1.1 {} {}\r\n", self.status, reason).as_bytes(),
        );
        // Write Content-Length if not already set.
        let has_content_length = self
            .headers
            .iter()
            .any(|(k, _)| k.to_ascii_lowercase() == "content-length");
        if !has_content_length {
            out.extend_from_slice(
                format!("Content-Length: {}\r\n", self.body.len()).as_bytes(),
            );
        }
        for (k, v) in &self.headers {
            out.extend_from_slice(format!("{}: {}\r\n", k, v).as_bytes());
        }
        out.extend_from_slice(b"\r\n");
        out.extend_from_slice(&self.body);
        out
    }
}

fn status_reason(code: u16) -> &'static str {
    match code {
        100 => "Continue",
        101 => "Switching Protocols",
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        204 => "No Content",
        206 => "Partial Content",
        301 => "Moved Permanently",
        302 => "Found",
        303 => "See Other",
        304 => "Not Modified",
        307 => "Temporary Redirect",
        308 => "Permanent Redirect",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        408 => "Request Timeout",
        409 => "Conflict",
        410 => "Gone",
        411 => "Length Required",
        413 => "Content Too Large",
        414 => "URI Too Long",
        415 => "Unsupported Media Type",
        422 => "Unprocessable Content",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => "Unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_raw_request_header_lookup_case_insensitive() {
        let req = RawRequest {
            method: "GET".into(),
            path: "/".into(),
            query: None,
            headers: vec![
                ("Content-Type".into(), "text/html".into()),
                ("Accept".into(), "application/json".into()),
            ],
            body: vec![],
        };
        assert_eq!(req.content_type(), Some("text/html"));
        assert_eq!(req.accept(), Some("application/json"));
        assert_eq!(req.header("CONTENT-TYPE"), Some("text/html"));
        assert_eq!(req.header("x-missing"), None);
    }

    #[test]
    fn test_raw_response_to_bytes() {
        let resp = RawResponse::new(200)
            .content_type("text/plain")
            .body(b"hello".to_vec());
        let bytes = resp.to_bytes();
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(text.contains("Content-Length: 5\r\n"));
        assert!(text.contains("Content-Type: text/plain\r\n"));
        assert!(text.ends_with("\r\nhello"));
    }

    #[test]
    fn test_raw_response_to_bytes_empty_body() {
        let resp = RawResponse::new(204);
        let bytes = resp.to_bytes();
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.starts_with("HTTP/1.1 204 No Content\r\n"));
        assert!(text.contains("Content-Length: 0\r\n"));
        assert!(text.ends_with("\r\n\r\n"));
    }
}

/// Whether `candidate` is safe to use as a redirect `Location` — that is, a
/// path on *this* origin rather than a URL pointing somewhere else.
///
/// `starts_with('/')` alone is **not** sufficient. `//evil.com` is a
/// protocol-relative URL, and browsers normalise `/\evil.com` to the same
/// thing; both pass a naive prefix check and then navigate off-site. Used for
/// post-login `?next=` targets and the `Referer`-derived redirect after a
/// token refresh, either of which would otherwise be an open redirect usable
/// to phish against our own login page.
pub fn is_same_origin_path(candidate: &str) -> bool {
    let b = candidate.as_bytes();
    if b.first() != Some(&b'/') {
        return false;
    }
    !matches!(b.get(1), Some(b'/') | Some(b'\\'))
}

#[cfg(test)]
mod redirect_tests {
    use super::is_same_origin_path;

    #[test]
    fn accepts_ordinary_same_origin_paths() {
        assert!(is_same_origin_path("/"));
        assert!(is_same_origin_path("/dashboard"));
        assert!(is_same_origin_path("/a/b?c=d"));
        assert!(is_same_origin_path("/path/with/slashes"));
    }

    #[test]
    fn rejects_protocol_relative_and_backslash_forms() {
        assert!(!is_same_origin_path("//evil.com"));
        assert!(!is_same_origin_path("//evil.com/phish"));
        assert!(!is_same_origin_path(r"/\evil.com"));
    }

    #[test]
    fn rejects_absolute_urls_and_non_paths() {
        assert!(!is_same_origin_path("https://evil.com"));
        assert!(!is_same_origin_path("evil.com"));
        assert!(!is_same_origin_path(""));
    }
}
