//! Reading and writing a header list, once.
//!
//! Core already had [`crate::http::header`] for "find the first one,
//! case-insensitively", and it was not used: eight or more sites across the
//! workspace wrote `.find(|(k, _)| k.eq_ignore_ascii_case(name))` inline
//! instead, including several inside core. That is a cheap kind of
//! duplication, but it is the kind that hides the expensive questions, and
//! there are two of those.
//!
//! # A field can appear more than once
//!
//! `find` returns the first. For `Host` that is all there is; for
//! `Set-Cookie`, `Via`, `Warning` or a request with two `Accept-Encoding`
//! lines, the first is one of several and silently using it drops the rest.
//! [`get_all`] is the honest read when a field may repeat.
//!
//! # Combining repeated fields is conditional, not automatic
//!
//! RFC 9110 5.3 permits a recipient to combine multiple field lines of the
//! same name into one comma-separated value **only when the field's value is
//! defined as a comma-separated list**. For anything else, combining changes
//! the meaning.
//!
//! `Set-Cookie` is the case everybody gets wrong. RFC 6265 3 is explicit that
//! it must not be folded: cookie values may themselves contain commas, so
//! joining two `Set-Cookie` lines produces one malformed cookie and silently
//! loses the other. [`combine`] refuses it rather than trusting the caller to
//! remember, and [`NEVER_COMBINED`] is the list.
//!
//! # Request head limits
//!
//! Two numbers bound an incoming request head, and they live here so that one
//! definition serves the HTTP/1.1 parser, the HTTP/2 and HTTP/3 edges and every
//! backend. [`MAX_HEADER_LINE`] bounds one field and [`MAX_REQUEST_HEAD`] bounds
//! the whole head. Both follow the numbers the mainstream servers use: nginx's
//! `large_client_header_buffers 4 8k`, Varnish's `http_req_hdr_len 8k` and
//! `http_req_size 32k`, and Apache's `LimitRequestFieldSize 8190`.
//!
//! The edge answers a request over either limit itself, before routing or a
//! backend, with 400 for a field or a head and 414 for a request line
//! (nginx's two answers for the two cases).

use crate::http::HeaderSource;

/// The text of one response header name or value.
///
/// A response's headers were `String` pairs, so every response paid an
/// allocation for each name and each value, including the ones core sets the
/// same way for every request on a route. This holds the three shapes a header
/// text takes without one:
///
/// - [`HeaderText::Static`] borrows a `&'static str`: a name such as
///   `Cache-Control` or a constant value.
/// - [`HeaderText::Shared`] shares an `Arc<str>`: a route's value, made once
///   when the site loads and owned by that site. A response clones the `Arc`,
///   which is a count and not an allocation, and keeps its text alive until it
///   is sent even if a reload has replaced the site.
/// - [`HeaderText::Inline`] keeps up to 23 bytes in the value itself: an `ETag`
///   is 18.
/// - [`HeaderText::Owned`] is the `String` a handler supplied.
///
/// It derefs to `str`, so a reader needs no change.
#[derive(Clone)]
pub enum HeaderText {
    Static(&'static str),
    Shared(std::sync::Arc<str>),
    Inline { len: u8, bytes: [u8; 23] },
    Owned(String),
}

impl HeaderText {
    /// A text that borrows a `'static` string.
    pub const fn from_static(s: &'static str) -> Self {
        HeaderText::Static(s)
    }

    /// A text holding `s` in the value itself, or `None` when it is longer than
    /// 23 bytes.
    pub fn inline(s: &str) -> Option<Self> {
        if s.len() > 23 {
            return None;
        }
        let mut bytes = [0u8; 23];
        bytes[..s.len()].copy_from_slice(s.as_bytes());
        Some(HeaderText::Inline {
            len: s.len() as u8,
            bytes,
        })
    }

    /// The text as a `str`.
    pub fn as_str(&self) -> &str {
        match self {
            HeaderText::Static(s) => s,
            HeaderText::Shared(s) => s,
            HeaderText::Inline { len, bytes } => {
                std::str::from_utf8(&bytes[..*len as usize]).expect("an inline text is UTF-8")
            }
            HeaderText::Owned(s) => s,
        }
    }

    /// The text as an owned `String`.
    pub fn into_string(self) -> String {
        match self {
            HeaderText::Owned(s) => s,
            other => other.as_str().to_string(),
        }
    }
}

impl std::ops::Deref for HeaderText {
    type Target = str;
    fn deref(&self) -> &str {
        self.as_str()
    }
}

impl AsRef<str> for HeaderText {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl std::fmt::Display for HeaderText {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::fmt::Debug for HeaderText {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(self.as_str(), f)
    }
}

impl From<String> for HeaderText {
    fn from(s: String) -> Self {
        HeaderText::Owned(s)
    }
}

impl From<&str> for HeaderText {
    fn from(s: &str) -> Self {
        HeaderText::Owned(s.to_string())
    }
}

impl From<std::sync::Arc<str>> for HeaderText {
    fn from(s: std::sync::Arc<str>) -> Self {
        HeaderText::Shared(s)
    }
}

impl From<&String> for HeaderText {
    fn from(s: &String) -> Self {
        HeaderText::Owned(s.clone())
    }
}

impl From<HeaderText> for String {
    fn from(t: HeaderText) -> String {
        t.into_string()
    }
}

impl PartialEq for HeaderText {
    fn eq(&self, other: &Self) -> bool {
        self.as_str() == other.as_str()
    }
}

impl Eq for HeaderText {}

impl PartialEq<str> for HeaderText {
    fn eq(&self, other: &str) -> bool {
        self.as_str() == other
    }
}

impl PartialEq<&str> for HeaderText {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

impl PartialEq<String> for HeaderText {
    fn eq(&self, other: &String) -> bool {
        self.as_str() == other.as_str()
    }
}

impl PartialEq<HeaderText> for &str {
    fn eq(&self, other: &HeaderText) -> bool {
        *self == other.as_str()
    }
}

impl PartialEq<HeaderText> for String {
    fn eq(&self, other: &HeaderText) -> bool {
        self.as_str() == other.as_str()
    }
}

/// A writer that fills the 23 inline bytes of a [`HeaderText`], for a value
/// formatted per response, such as an `ETag`. Writing more than 23 bytes is an
/// error, so a caller that cannot bound its value uses a `String`.
#[derive(Default)]
pub struct InlineBuf {
    len: usize,
    bytes: [u8; 23],
}

impl InlineBuf {
    pub fn new() -> Self {
        Self::default()
    }

    /// The bytes written, as an inline [`HeaderText`].
    pub fn into_text(self) -> HeaderText {
        HeaderText::Inline {
            len: self.len as u8,
            bytes: self.bytes,
        }
    }
}

impl std::fmt::Write for InlineBuf {
    fn write_str(&mut self, s: &str) -> std::fmt::Result {
        let end = self.len + s.len();
        if end > self.bytes.len() {
            return Err(std::fmt::Error);
        }
        self.bytes[self.len..end].copy_from_slice(s.as_bytes());
        self.len = end;
        Ok(())
    }
}

/// One response header, name then value.
pub type Header = (HeaderText, HeaderText);

/// The largest one request header field, counted as its name plus its value.
///
/// The number is the one nginx bounds a field line with (`8k`), the one
/// Varnish states as `http_req_hdr_len 8k`, and one byte above Apache's
/// `LimitRequestFieldSize` default of 8190, which excludes Apache's own
/// framing. m6 counts the name and the value, and nothing else.
pub const MAX_HEADER_LINE: usize = 8 * 1024;

/// The largest request head: the request line plus every field.
///
/// nginx's `large_client_header_buffers 4 8k` keeps 32 KiB of buffers for one
/// request, and Varnish states the same total as `http_req_size 32k`. HTTP/2
/// and HTTP/3 count a decoded head as RFC 9113 6.5.2 does for
/// `SETTINGS_MAX_HEADER_LIST_SIZE`: name plus value plus 32 bytes per field.
pub const MAX_REQUEST_HEAD: usize = 32 * 1024;

/// The length of a request line, counted with one space between the method and
/// the target.
///
/// HTTP/1.1 adds one space and the version, and measures the line it receives.
/// HTTP/2 and HTTP/3 have no version on the wire, so their request line is
/// `:method` plus `:path`.
pub fn request_line_len(method: &[u8], target: &[u8]) -> usize {
    method.len() + 1 + target.len()
}

/// Whether one request field is over [`MAX_HEADER_LINE`], counted as its name
/// plus its value.
pub fn field_over_limit(name: &[u8], value: &[u8]) -> bool {
    name.len() + value.len() > MAX_HEADER_LINE
}

/// Fields that must never be folded into one comma-separated line.
///
/// `Set-Cookie` is the one that matters and the one that is routinely got
/// wrong. Its value can contain commas (in an `Expires` date, for one), so a
/// combined pair is not merely ugly, it is unparseable.
pub const NEVER_COMBINED: &[&str] = &["set-cookie"];

/// Whether a field may be folded into a comma-separated line.
pub fn may_combine(name: &str) -> bool {
    !NEVER_COMBINED.iter().any(|n| name.eq_ignore_ascii_case(n))
}

/// The first value for `name`, case-insensitively.
///
/// The same thing [`crate::http::header`] does; here so that a caller reaching
/// for this module finds it without having to know the other name.
pub fn get<'a>(headers: &'a (impl HeaderSource + ?Sized), name: &str) -> Option<&'a str> {
    headers.find(name)
}

/// Every value for `name`, in the order sent.
pub fn get_all<'a>(
    headers: &'a (impl HeaderSource + ?Sized),
    name: &'a str,
) -> impl Iterator<Item = &'a str> + 'a {
    headers.find_all(name)
}

pub fn contains(headers: &(impl HeaderSource + ?Sized), name: &str) -> bool {
    headers.find(name).is_some()
}

/// All values for `name`, folded into one comma-separated string.
///
/// `None` when the field is absent **or** when it is one that must not be
/// combined; the two are distinguished by [`contains`]. Returning `None` for
/// `Set-Cookie` rather than a joined string is the point: a caller that wanted
/// the values gets nothing and goes looking, instead of getting something
/// plausible and wrong.
pub fn combine(headers: &(impl HeaderSource + ?Sized), name: &str) -> Option<String> {
    if !may_combine(name) {
        return None;
    }
    let mut out: Option<String> = None;
    for v in get_all(headers, name) {
        match &mut out {
            None => out = Some(v.to_string()),
            Some(acc) => {
                acc.push_str(", ");
                acc.push_str(v);
            }
        }
    }
    out
}

/// Replace every occurrence of `name` with one line carrying `value`.
///
/// The replacement goes where the first one was, so header order is stable
/// across a `set`. Appending after a remove would move the field to the end
/// and make two otherwise identical responses differ byte for byte.
pub fn set(headers: &mut Vec<(String, String)>, name: &str, value: impl Into<String>) {
    let first = headers
        .iter()
        .position(|(k, _)| k.eq_ignore_ascii_case(name));
    match first {
        Some(i) => {
            headers[i] = (name.to_string(), value.into());
            let mut j = i + 1;
            while j < headers.len() {
                if headers[j].0.eq_ignore_ascii_case(name) {
                    headers.remove(j);
                } else {
                    j += 1;
                }
            }
        }
        None => headers.push((name.to_string(), value.into())),
    }
}

/// Add a line without touching any existing one.
///
/// What `Set-Cookie` needs, and what `set` must not be used for.
pub fn append(headers: &mut Vec<(String, String)>, name: &str, value: impl Into<String>) {
    headers.push((name.to_string(), value.into()));
}

/// Set `name` only if it is not already present. Returns whether it was added.
///
/// The shape of "supply a default the handler did not": `Content-Type` and
/// `ETag` in `Response::send` both do this by hand today.
pub fn set_if_absent(
    headers: &mut Vec<(String, String)>,
    name: &str,
    value: impl Into<String>,
) -> bool {
    if headers.iter().any(|(k, _)| k.eq_ignore_ascii_case(name)) {
        return false;
    }
    headers.push((name.to_string(), value.into()));
    true
}

/// Remove every occurrence of `name`. Returns how many went.
pub fn remove(headers: &mut Vec<(String, String)>, name: &str) -> usize {
    let before = headers.len();
    headers.retain(|(k, _)| !k.eq_ignore_ascii_case(name));
    before - headers.len()
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;

    #[test]
    fn a_header_text_holds_static_inline_and_owned_text_the_same_way() {
        let texts = [
            HeaderText::from_static("etag"),
            HeaderText::from(std::sync::Arc::<str>::from("etag")),
            HeaderText::inline("etag").unwrap(),
            HeaderText::from("etag".to_string()),
            HeaderText::from("etag"),
        ];
        for t in &texts {
            assert_eq!(t, "etag");
            assert_eq!(&**t, "etag");
            assert_eq!(*t, texts[0]);
            assert_eq!(format!("{t} {t:?}"), "etag \"etag\"");
        }
        assert!(HeaderText::inline(&"x".repeat(23)).is_some());
        assert!(HeaderText::inline(&"x".repeat(24)).is_none());
    }

    #[test]
    fn an_inline_buffer_takes_an_etag_and_refuses_what_does_not_fit() {
        let mut b = InlineBuf::new();
        write!(b, "\"{:x}\"", u64::MAX).unwrap();
        assert_eq!(b.into_text(), "\"ffffffffffffffff\"");
        let mut b = InlineBuf::new();
        assert!(write!(b, "{}", "x".repeat(24)).is_err());
    }

    use super::*;

    fn h() -> Vec<(String, String)> {
        vec![
            ("Host".into(), "example.com".into()),
            ("Set-Cookie".into(), "a=1; Path=/".into()),
            ("set-cookie".into(), "b=2; Path=/".into()),
            ("Accept-Encoding".into(), "gzip".into()),
            ("accept-encoding".into(), "br".into()),
        ]
    }

    #[test]
    fn get_is_case_insensitive_and_takes_the_first() {
        assert_eq!(get(&h()[..], "host"), Some("example.com"));
        assert_eq!(get(&h()[..], "HOST"), Some("example.com"));
        assert_eq!(get(&h()[..], "set-cookie"), Some("a=1; Path=/"));
        assert_eq!(get(&h()[..], "absent"), None);
    }

    /// The reason `get` alone is not enough: taking the first silently drops
    /// the rest, and for a repeated field that is data loss, not a shortcut.
    #[test]
    fn get_all_returns_every_occurrence() {
        let hs = h();
        let cookies: Vec<_> = get_all(&hs, "Set-Cookie").collect();
        assert_eq!(cookies, vec!["a=1; Path=/", "b=2; Path=/"]);
        let enc: Vec<_> = get_all(&hs, "ACCEPT-ENCODING").collect();
        assert_eq!(enc, vec!["gzip", "br"]);
    }

    /// RFC 9110 5.3 allows folding for list-valued fields.
    #[test]
    fn a_list_valued_field_combines() {
        assert_eq!(
            combine(&h(), "accept-encoding"),
            Some("gzip, br".to_string())
        );
    }

    /// RFC 6265 3: Set-Cookie must not be folded. A cookie value can contain a
    /// comma, so a combined pair is one malformed cookie and one lost cookie,
    /// and it looks fine until something tries to parse it.
    #[test]
    fn set_cookie_is_never_combined() {
        assert_eq!(combine(&h(), "Set-Cookie"), None);
        assert!(!may_combine("set-cookie"));
        assert!(!may_combine("Set-Cookie"));
        // And absent is distinguishable from refused.
        assert!(contains(&h()[..], "Set-Cookie"));
        assert!(!contains(&h()[..], "nonesuch"));
        assert_eq!(combine(&h(), "nonesuch"), None);
    }

    /// `set` replaces every duplicate, not just the first. Leaving a second
    /// line behind is how a response ends up asserting two content types.
    #[test]
    fn set_replaces_all_duplicates() {
        let mut hs = h();
        set(&mut hs, "Accept-Encoding", "identity");
        let enc: Vec<_> = get_all(&hs, "accept-encoding").collect();
        assert_eq!(enc, vec!["identity"]);
    }

    /// Position is preserved, so two responses built the same way are
    /// byte-identical rather than differing by field order.
    #[test]
    fn set_keeps_the_original_position() {
        let mut hs = h();
        set(&mut hs, "host", "other.example");
        assert_eq!(hs[0].0, "host");
        assert_eq!(hs[0].1, "other.example");
    }

    #[test]
    fn set_on_an_absent_field_appends() {
        let mut hs = vec![("A".to_string(), "1".to_string())];
        set(&mut hs, "B", "2");
        assert_eq!(hs.len(), 2);
        assert_eq!(get(&hs[..], "b"), Some("2"));
    }

    #[test]
    fn append_does_not_disturb_what_is_there() {
        let mut hs = h();
        append(&mut hs, "Set-Cookie", "c=3");
        let cookies: Vec<_> = get_all(&hs, "set-cookie").collect();
        assert_eq!(cookies, vec!["a=1; Path=/", "b=2; Path=/", "c=3"]);
    }

    #[test]
    fn set_if_absent_defers_to_the_caller() {
        let mut hs = vec![("Content-Type".to_string(), "text/plain".to_string())];
        assert!(!set_if_absent(&mut hs, "content-type", "text/html"));
        assert_eq!(get(&hs[..], "Content-Type"), Some("text/plain"));
        assert!(set_if_absent(&mut hs, "ETag", "\"abc\""));
        assert_eq!(get(&hs[..], "etag"), Some("\"abc\""));
    }

    #[test]
    fn remove_takes_every_occurrence_and_counts() {
        let mut hs = h();
        assert_eq!(remove(&mut hs, "SET-COOKIE"), 2);
        assert!(!contains(&hs[..], "set-cookie"));
        assert_eq!(remove(&mut hs, "set-cookie"), 0);
    }
}
