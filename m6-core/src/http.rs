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
    /// The version the client sent: "HTTP/1.1" or "HTTP/1.0".
    ///
    /// Persistent connections need it (RFC 9112 9.3: 1.1 keeps the connection
    /// by default, 1.0 closes), which is why it is not optional.
    pub version: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl RawRequest {
    /// Look up a header by name, case-insensitively.
    ///
    /// This used to lowercase the name *and every key it scanned*, so a single
    /// lookup allocated one `String` per header examined. It goes through
    /// [`HeaderSource`] now, which compares with `eq_ignore_ascii_case` and
    /// allocates nothing.
    pub fn header(&self, name: &str) -> Option<&str> {
        crate::http::header(&self.headers, name)
    }

    /// The request method.
    pub fn method(&self) -> &str {
        &self.method
    }

    /// The request path, without the query string.
    pub fn path(&self) -> &str {
        &self.path
    }

    /// The raw query string, or `""` when there is none.
    pub fn query(&self) -> &str {
        self.query.as_deref().unwrap_or("")
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
    /// Send this response through the one HTTP/1.1 response writer.
    ///
    /// `to_bytes` used to serialise it here, with its own status table and no
    /// HEAD or `Connection` handling -- a fourth serialiser, inside core.
    pub fn send<W: std::io::Write>(
        &self,
        resp: &mut crate::h1::Responder<'_, W>,
    ) -> std::io::Result<()> {
        let hdrs: Vec<(&str, &str)> =
            self.headers.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        resp.send(self.status, &hdrs, &self.body)
    }

    /// The serialised response, for a caller that has bytes rather than a
    /// stream: the connection is already gone, or the response is being
    /// compared in a test.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        let mut resp = crate::h1::Responder::new(&mut out, "", false);
        // Writing into a Vec cannot fail.
        let _ = self.send(&mut resp);
        out
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_raw_request_header_lookup_case_insensitive() {
        let req = RawRequest {
            version: "HTTP/1.1".to_string(),
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

/// A scannable list of request headers, whatever shape the caller holds them in.
///
/// The same lookup works for an owned `Vec<(String, String)>` (HTTP/1.1 and
/// HTTP/2, and every backend) and for a protocol-native slice that a caller
/// would otherwise have to copy into one just to read a header. Both are
/// slices, so they can be scanned as many times as needed for free.
///
/// `find_all` exists for `Cookie`, which HTTP/2 and HTTP/3 clients may split
/// across several header fields.
///
/// **Case-insensitive, so no caller has to know how its parser stored the
/// names.** That is the point of putting this in core. There were four header
/// parsers in the workspace with three different conventions: `m6-core`'s
/// stores names as sent, `m6-file`'s and `m6-render`'s lowercase them at parse
/// time, and their lookups compared with `k == "literal"` accordingly. Those
/// comparisons are correct only under an invariant established in a different
/// file and invisible at the call site; hand either one a request parsed by
/// the other and every header lookup silently returns `None`. For conditional
/// requests that means a conditional GET quietly stops working, which is a
/// defect that hides for a long time.
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

/// Look up a header by case-insensitive name.
pub fn header<'a>(headers: &'a (impl HeaderSource + ?Sized), name: &str) -> Option<&'a str> {
    headers.find(name)
}

#[cfg(test)]
mod header_source_tests {
    use super::*;

    fn hdrs() -> Vec<(String, String)> {
        vec![
            ("Host".into(), "example.com".into()),
            ("cookie".into(), "a=1".into()),
            ("COOKIE".into(), "b=2".into()),
        ]
    }

    #[test]
    fn lookup_ignores_case_on_both_sides() {
        let h = hdrs();
        assert_eq!(header(&h, "host"), Some("example.com"));
        assert_eq!(header(&h, "HOST"), Some("example.com"));
        assert_eq!(header(&h, "Host"), Some("example.com"));
        assert_eq!(header(&h, "absent"), None);
    }

    #[test]
    fn find_all_returns_every_occurrence_in_order() {
        let h = hdrs();
        let got: Vec<&str> = h.find_all("Cookie").collect();
        assert_eq!(got, vec!["a=1", "b=2"]);
    }
}
