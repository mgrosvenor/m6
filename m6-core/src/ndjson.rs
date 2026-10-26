//! Newline-delimited JSON, read and written.
//!
//! Core already owns TOML for config. NDJSON is the other format m6 puts on
//! disk: it is what the analytics stream is, and it is the natural shape for
//! anything append-only that a later process reads back. It belongs here for
//! the same reason, and so that no service has to decide for itself what a
//! torn line means.
//!
//! # Torn lines are ordinary, not exceptional
//!
//! An NDJSON file is usually being appended to while it is read. The last line
//! can be half-written, and a reader that fails the whole stream on one bad
//! line will fail whenever it happens to read during a write. So [`read_str`]
//! and [`Reader`] skip what they cannot parse and keep going. When you need to
//! know, [`Reader::skipped`] counts them: silence about a skipped line is fine,
//! but only if it is available to anyone who asks.
//!
//! Blank lines are skipped without counting as errors. A trailing newline is
//! normal and is not a torn record.
//!
//! # Pages for responses
//!
//! A service that answers a request with rows builds a [`Page`]. A page holds
//! at most [`PageLimits::max_bytes`] bytes and [`PageLimits::max_rows`] rows, so
//! the memory a request can take is bounded before the first row is read. When
//! a row does not fit, [`Page::push`] says [`Pushed::Full`], the service stores
//! the key of that row in a [`Cursor`], and [`Page::into_response`] sends a
//! `Link: <...>; rel="next"` header naming the next request. The client follows
//! the link until a response has none.
//!
//! A page is a response, not a stream: m6-http buffers every backend body (up
//! to 128 MiB), so streaming from behind it gains nothing. For the same reason
//! a page skips [`write_one`]'s one `Vec` per row. `write_one` builds each line
//! apart so a record and its newline reach a file in one write. A response body
//! is sent whole or not at all, so rows are serialised straight into the one
//! growing buffer.

use std::io::{BufRead, Write};

use crate::response::Response;

use serde::{de::DeserializeOwned, Serialize};

/// Parse every record in a string, skipping blank and unparseable lines.
pub fn read_str<T: DeserializeOwned>(text: &str) -> impl Iterator<Item = T> + '_ {
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str(l).ok())
}

/// A counting reader over any [`BufRead`].
///
/// Use this over [`read_str`] when the stream is large enough that holding it
/// in memory is wasteful, or when the count of unparseable lines matters.
pub struct Reader<R: BufRead, T> {
    inner: R,
    skipped: usize,
    read: usize,
    buf: String,
    _marker: std::marker::PhantomData<T>,
}

impl<R: BufRead, T: DeserializeOwned> Reader<R, T> {
    pub fn new(inner: R) -> Self {
        Self {
            inner,
            skipped: 0,
            read: 0,
            buf: String::new(),
            _marker: std::marker::PhantomData,
        }
    }

    /// Lines that were not blank and did not parse.
    pub fn skipped(&self) -> usize {
        self.skipped
    }

    /// Records successfully parsed.
    pub fn read_count(&self) -> usize {
        self.read
    }
}

impl<R: BufRead, T: DeserializeOwned> Iterator for Reader<R, T> {
    type Item = T;

    fn next(&mut self) -> Option<T> {
        loop {
            self.buf.clear();
            match self.inner.read_line(&mut self.buf) {
                Ok(0) => return None,
                Ok(_) => {}
                Err(_) => return None,
            }
            let line = self.buf.trim();
            if line.is_empty() {
                continue;
            }
            match serde_json::from_str(line) {
                Ok(v) => {
                    self.read += 1;
                    return Some(v);
                }
                Err(_) => {
                    self.skipped += 1;
                    continue;
                }
            }
        }
    }
}

/// Write one record and its newline.
///
/// One record per write call, flushed by the caller. NDJSON's whole value is
/// that a reader can make sense of a prefix, which requires that a record and
/// its terminator reach the file together.
pub fn write_one<W: Write, T: Serialize>(w: &mut W, value: &T) -> std::io::Result<()> {
    let mut line = serde_json::to_vec(value)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    line.push(b'\n');
    w.write_all(&line)
}

/// Write many records.
pub fn write_all<'a, W: Write, T: Serialize + 'a>(
    w: &mut W,
    values: impl IntoIterator<Item = &'a T>,
) -> std::io::Result<()> {
    for v in values {
        write_one(w, v)?;
    }
    Ok(())
}

/// Serialise records to a `String`.
pub fn to_string<'a, T: Serialize + 'a>(
    values: impl IntoIterator<Item = &'a T>,
) -> std::io::Result<String> {
    let mut out = Vec::new();
    write_all(&mut out, values)?;
    String::from_utf8(out).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

/// The `Content-Type` of a page.
pub const CONTENT_TYPE: &str = "application/x-ndjson";
/// Default page size in bytes: 4 MiB.
pub const DEFAULT_PAGE_BYTES: usize = 4 * 1024 * 1024;
/// Default page size in rows.
pub const DEFAULT_PAGE_ROWS: usize = 10_000;
/// The largest page a service can ask for: 64 MiB, half of the 128 MiB that
/// m6-http will buffer from a backend.
pub const MAX_PAGE_BYTES: usize = 64 * 1024 * 1024;
/// The query key that carries a [`Cursor`].
pub const CURSOR_PARAM: &str = "cursor";
/// The longest cursor accepted, in bytes.
pub const MAX_CURSOR_LEN: usize = 128;

/// How much one page may hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PageLimits {
    pub max_bytes: usize,
    pub max_rows: usize,
}

impl Default for PageLimits {
    fn default() -> Self {
        Self {
            max_bytes: DEFAULT_PAGE_BYTES,
            max_rows: DEFAULT_PAGE_ROWS,
        }
    }
}

impl PageLimits {
    /// Limits clamped to something usable: bytes to `1..=`[`MAX_PAGE_BYTES`],
    /// rows to at least 1.
    pub fn new(max_bytes: usize, max_rows: usize) -> Self {
        Self {
            max_bytes: max_bytes.clamp(1, MAX_PAGE_BYTES),
            max_rows: max_rows.max(1),
        }
    }
}

/// What [`Page::push`] did with a row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pushed {
    /// The row is in the page.
    Added,
    /// The row would pass a limit and was not written. Stop and send the page.
    Full,
}

/// One bounded page of JSON Lines, built in a single buffer.
pub struct Page {
    buf: Vec<u8>,
    rows: usize,
    limits: PageLimits,
}

impl Page {
    pub fn new(limits: PageLimits) -> Self {
        Self {
            buf: Vec::with_capacity(limits.max_bytes.min(64 * 1024)),
            rows: 0,
            limits,
        }
    }

    /// Add one row, which must serialise to a JSON object.
    ///
    /// Returns [`Pushed::Full`], leaving the page as it was, when the row would
    /// pass the row or byte limit. The first row of a page is kept even when it
    /// is bigger than `max_bytes`, so a client can always make progress; it is
    /// refused with `InvalidData` only above [`MAX_PAGE_BYTES`]. A value that
    /// is not an object is refused with `InvalidInput`.
    pub fn push<T: Serialize + ?Sized>(&mut self, row: &T) -> std::io::Result<Pushed> {
        if self.rows >= self.limits.max_rows {
            return Ok(Pushed::Full);
        }
        let start = self.buf.len();
        if let Err(e) = serde_json::to_writer(&mut self.buf, row) {
            self.buf.truncate(start);
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, e));
        }
        if self.buf.get(start) != Some(&b'{') {
            self.buf.truncate(start);
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "a JSON Lines row must be an object",
            ));
        }
        self.buf.push(b'\n');
        let row_len = self.buf.len() - start;
        if self.rows == 0 {
            if row_len > MAX_PAGE_BYTES {
                self.buf.truncate(start);
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "a row is larger than the maximum page",
                ));
            }
        } else if self.buf.len() > self.limits.max_bytes {
            self.buf.truncate(start);
            return Ok(Pushed::Full);
        }
        self.rows += 1;
        Ok(Pushed::Added)
    }

    /// Rows in the page.
    pub fn rows(&self) -> usize {
        self.rows
    }

    /// Bytes in the page.
    pub fn bytes(&self) -> usize {
        self.buf.len()
    }

    /// The response: 200, `application/x-ndjson`, `no-store`, `nosniff`, framed
    /// with `Content-Length`, no ETag and no compression.
    ///
    /// With `next`, adds `Link: <?QUERY>; rel="next"` where QUERY is
    /// `request_query` with any `cursor` removed and the new one added. A
    /// query-only reference resolves against the request path (RFC 3986
    /// section 5.2). No `Link` means this is the last page.
    pub fn into_response(self, request_query: &str, next: Option<&Cursor>) -> Response {
        let len = self.buf.len() as u64;
        let mut resp = Response::stream(200, len, std::io::Cursor::new(self.buf))
            .header("Content-Type", CONTENT_TYPE)
            .header("Cache-Control", "no-store")
            .header("X-Content-Type-Options", "nosniff");
        if let Some(c) = next {
            resp = resp.header("Link", &next_link(request_query, c));
        }
        resp
    }
}

/// True for the bytes a URI query may carry unescaped, minus `&`, which
/// separates pairs and is handled by the caller.
fn query_safe(b: u8) -> bool {
    b.is_ascii_alphanumeric()
        || matches!(
            b,
            b'-' | b'.'
                | b'_'
                | b'~'
                | b'!'
                | b'$'
                | b'\''
                | b'('
                | b')'
                | b'*'
                | b'+'
                | b','
                | b';'
                | b'='
                | b':'
                | b'@'
                | b'/'
                | b'?'
                | b'%'
        )
}

fn next_link(request_query: &str, cursor: &Cursor) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(request_query.len() + cursor.0.len() + 24);
    out.push_str("<?");
    for pair in request_query.split('&').filter(|p| !p.is_empty()) {
        if pair.split_once('=').map_or(pair, |(k, _)| k) == CURSOR_PARAM {
            continue;
        }
        for &b in pair.as_bytes() {
            if query_safe(b) {
                out.push(b as char);
            } else {
                out.push('%');
                out.push(HEX[(b >> 4) as usize] as char);
                out.push(HEX[(b & 15) as usize] as char);
            }
        }
        out.push('&');
    }
    out.push_str(CURSOR_PARAM);
    out.push('=');
    out.push_str(&cursor.0);
    out.push_str(">; rel=\"next\"");
    out
}

/// Where the next page starts. Opaque to core.
///
/// The service builds it from the key of the first row that did not fit, for
/// example `"<at_ms>-<id>"`, and reads it back on the next request. It is 1 to
/// [`MAX_CURSOR_LEN`] bytes of `A-Za-z0-9-._~`, so it needs no URL encoding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cursor(String);

/// Why a cursor was refused. Nothing here holds the client's value.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CursorError {
    #[error("`cursor` given more than once")]
    Repeated,
    #[error("`cursor` is empty")]
    Empty,
    #[error("`cursor` is not a cursor this service issued")]
    Malformed,
}

impl CursorError {
    /// A 400 that does not echo the value.
    pub fn to_response(&self) -> Response {
        Response::refusal(self)
    }
}

impl Cursor {
    pub fn new(s: impl Into<String>) -> Result<Self, CursorError> {
        let s = s.into();
        if s.is_empty() {
            return Err(CursorError::Empty);
        }
        let ok = s.len() <= MAX_CURSOR_LEN
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~'));
        if ok {
            Ok(Self(s))
        } else {
            Err(CursorError::Malformed)
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The cursor in a raw query string: `None` when absent, an error when it
    /// is repeated, empty or malformed.
    pub fn from_query(query: &str) -> Result<Option<Self>, CursorError> {
        let found = crate::request::single_query_value(query, CURSOR_PARAM)
            .map_err(|_| CursorError::Repeated)?;
        found.map(Self::new).transpose()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Row {
        n: u32,
        s: String,
    }

    fn row(n: u32, s: &str) -> Row {
        Row {
            n,
            s: s.to_string(),
        }
    }

    #[test]
    fn round_trips() {
        let rows = vec![row(1, "a"), row(2, "b")];
        let text = to_string(&rows).unwrap();
        assert_eq!(text, "{\"n\":1,\"s\":\"a\"}\n{\"n\":2,\"s\":\"b\"}\n");
        let back: Vec<Row> = read_str(&text).collect();
        assert_eq!(back, rows);
    }

    /// The case the whole module exists for: a file being appended to while it
    /// is read ends in half a record. That must not cost the records before it.
    #[test]
    fn a_torn_final_line_does_not_lose_the_file() {
        let text = "{\"n\":1,\"s\":\"a\"}\n{\"n\":2,\"s\":\"b\"}\n{\"n\":3,\"s\":\"c";
        let back: Vec<Row> = read_str(text).collect();
        assert_eq!(back, vec![row(1, "a"), row(2, "b")]);
    }

    #[test]
    fn blank_lines_are_not_errors_but_bad_lines_are_counted() {
        let text = "{\"n\":1,\"s\":\"a\"}\n\n   \nnot json\n{\"n\":2,\"s\":\"b\"}\n";
        let mut r: Reader<_, Row> = Reader::new(std::io::Cursor::new(text));
        let got: Vec<Row> = r.by_ref().collect();
        assert_eq!(got, vec![row(1, "a"), row(2, "b")]);
        assert_eq!(r.read_count(), 2);
        assert_eq!(
            r.skipped(),
            1,
            "the blank lines are not failures; `not json` is"
        );
    }

    /// A record whose JSON contains an escaped newline is still one line, and
    /// splitting on a raw newline must not cut it in half.
    #[test]
    fn embedded_newlines_survive() {
        let rows = vec![row(1, "two\nlines")];
        let text = to_string(&rows).unwrap();
        assert_eq!(text.lines().count(), 1);
        let back: Vec<Row> = read_str(&text).collect();
        assert_eq!(back, rows);
    }

    #[test]
    fn empty_input_yields_nothing() {
        let back: Vec<Row> = read_str("").collect();
        assert!(back.is_empty());
        let back: Vec<Row> = read_str("\n\n").collect();
        assert!(back.is_empty());
    }

    // ---- pages ----

    fn page_row(n: u32) -> Row {
        row(n, "x")
    }

    /// The length of one `page_row` line, newline included.
    fn line_len() -> usize {
        to_string([&page_row(1)]).unwrap().len()
    }

    fn send(r: Response, method: &str) -> (String, Vec<u8>) {
        let mut out = Vec::new();
        {
            let mut resp = crate::h1::Responder::new(&mut out, method, false);
            r.send(&mut resp).expect("send");
        }
        let sep = out
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .expect("header terminator");
        (
            String::from_utf8(out[..sep].to_vec()).unwrap(),
            out[sep + 4..].to_vec(),
        )
    }

    fn header<'a>(r: &'a Response, name: &str) -> Option<&'a str> {
        r.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    #[test]
    fn a_page_holds_rows_until_the_byte_cap() {
        let n = line_len();
        let mut p = Page::new(PageLimits::new(n * 3, 100));
        for i in 0..3 {
            assert_eq!(p.push(&page_row(i)).unwrap(), Pushed::Added);
        }
        assert_eq!(p.push(&page_row(3)).unwrap(), Pushed::Full);
        assert_eq!((p.rows(), p.bytes()), (3, n * 3));
    }

    #[test]
    fn a_page_holds_rows_until_the_row_cap() {
        let mut p = Page::new(PageLimits::new(1 << 20, 2));
        assert_eq!(p.push(&page_row(1)).unwrap(), Pushed::Added);
        assert_eq!(p.push(&page_row(2)).unwrap(), Pushed::Added);
        assert_eq!(p.push(&page_row(3)).unwrap(), Pushed::Full);
        assert_eq!(p.rows(), 2);
    }

    #[test]
    fn a_row_that_does_not_fit_is_not_written() {
        let n = line_len();
        let mut p = Page::new(PageLimits::new(n + 1, 100));
        p.push(&page_row(1)).unwrap();
        let before = (p.rows(), p.bytes());
        assert_eq!(p.push(&page_row(2)).unwrap(), Pushed::Full);
        assert_eq!((p.rows(), p.bytes()), before);
        let resp = p.into_response("", None);
        let (_, body) = send(resp, "GET");
        assert_eq!(body.len(), n);
    }

    #[test]
    fn a_first_row_larger_than_the_page_is_still_served() {
        let mut p = Page::new(PageLimits::new(10, 100));
        assert_eq!(p.push(&row(1, &"y".repeat(100))).unwrap(), Pushed::Added);
        assert!(p.bytes() > 10);
        assert_eq!(p.push(&page_row(2)).unwrap(), Pushed::Full);
        assert_eq!(p.rows(), 1);
    }

    #[test]
    fn a_row_larger_than_the_maximum_page_is_an_error() {
        let mut p = Page::new(PageLimits::default());
        let big = row(1, &"y".repeat(MAX_PAGE_BYTES));
        let err = p.push(&big).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        assert_eq!((p.rows(), p.bytes()), (0, 0));
    }

    #[test]
    fn a_non_object_value_is_refused_and_leaves_the_page_unchanged() {
        let mut p = Page::new(PageLimits::default());
        p.push(&page_row(1)).unwrap();
        let before = p.bytes();
        for bad in [
            serde_json::json!(5),
            serde_json::json!("s"),
            serde_json::json!([1]),
        ] {
            let err = p.push(&bad).unwrap_err();
            assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
            assert_eq!((p.rows(), p.bytes()), (1, before));
        }
    }

    #[test]
    fn embedded_newlines_stay_on_one_line_in_a_page() {
        let mut p = Page::new(PageLimits::default());
        p.push(&row(1, "two\nlines\r\nand more")).unwrap();
        let (_, body) = send(p.into_response("", None), "GET");
        let text = String::from_utf8(body).unwrap();
        assert_eq!(text.lines().count(), 1);
        assert!(text.ends_with('\n'));
    }

    #[test]
    fn limits_are_clamped_to_the_maximum_page() {
        let l = PageLimits::new(usize::MAX, 0);
        assert_eq!(l.max_bytes, MAX_PAGE_BYTES);
        assert_eq!(l.max_rows, 1);
        assert_eq!(PageLimits::new(0, 5).max_bytes, 1);
        assert_eq!(PageLimits::default().max_bytes, DEFAULT_PAGE_BYTES);
        assert_eq!(PageLimits::default().max_rows, DEFAULT_PAGE_ROWS);
    }

    #[test]
    fn the_response_has_the_ndjson_type_no_store_nosniff_and_a_length() {
        let mut p = Page::new(PageLimits::default());
        p.push(&page_row(1)).unwrap();
        let n = p.bytes();
        let (head, body) = send(p.into_response("", None), "GET");
        assert!(head.starts_with("HTTP/1.1 200"), "{head}");
        assert!(
            head.contains("Content-Type: application/x-ndjson"),
            "{head}"
        );
        assert!(head.contains("Cache-Control: no-store"), "{head}");
        assert!(head.contains("X-Content-Type-Options: nosniff"), "{head}");
        assert!(head.contains(&format!("Content-Length: {n}")), "{head}");
        assert_eq!(body.len(), n);
    }

    #[test]
    fn a_page_response_carries_no_etag_and_is_not_rehashed() {
        let mut p = Page::new(PageLimits::default());
        p.push(&page_row(1)).unwrap();
        let resp = p.into_response("", None);
        assert!(resp.body.as_bytes().is_none(), "a page is a stream body");
        let (head, _) = send(resp, "GET");
        assert!(!head.to_ascii_lowercase().contains("etag:"), "{head}");
    }

    #[test]
    fn head_on_a_page_sends_no_body() {
        let mut p = Page::new(PageLimits::default());
        p.push(&page_row(1)).unwrap();
        let n = p.bytes();
        let (head, body) = send(p.into_response("", None), "HEAD");
        assert!(head.contains(&format!("Content-Length: {n}")), "{head}");
        assert!(body.is_empty());
    }

    #[test]
    fn the_next_link_keeps_other_params_and_replaces_the_cursor() {
        let c = Cursor::new("1700000000000-42").unwrap();
        let p = Page::new(PageLimits::default());
        let resp = p.into_response(
            "from=2026-01-01T00%3A00%3A00Z&cursor=old&to=2026-01-02T00%3A00%3A00Z&node=edge-a&cursor=older",
            Some(&c),
        );
        assert_eq!(
            header(&resp, "Link"),
            Some(
                "<?from=2026-01-01T00%3A00%3A00Z&to=2026-01-02T00%3A00%3A00Z&node=edge-a&cursor=1700000000000-42>; rel=\"next\""
            )
        );
    }

    #[test]
    fn the_next_link_percent_encodes_unsafe_bytes() {
        let c = Cursor::new("k").unwrap();
        let p = Page::new(PageLimits::default());
        let resp = p.into_response("q=a<b>c\"d e&&", Some(&c));
        let link = header(&resp, "Link").unwrap();
        assert_eq!(link, "<?q=a%3Cb%3Ec%22d%20e&cursor=k>; rel=\"next\"");
        assert!(!link[1..link.len() - 13].contains(['<', '"', ' ']));
    }

    #[test]
    fn no_next_link_on_the_last_page() {
        let p = Page::new(PageLimits::default());
        let resp = p.into_response("from=a&to=b", None);
        assert!(header(&resp, "Link").is_none());
    }

    #[test]
    fn a_cursor_accepts_unreserved_characters_only() {
        assert!(Cursor::new("Az09-._~").is_ok());
        for bad in ["a b", "a/b", "a%20b", "a&b", "a=b", "é", "a\n"] {
            assert_eq!(Cursor::new(bad), Err(CursorError::Malformed), "{bad:?}");
        }
        assert_eq!(Cursor::new(""), Err(CursorError::Empty));
        assert_eq!(Cursor::new("k").unwrap().as_str(), "k");
    }

    #[test]
    fn an_overlong_cursor_is_refused() {
        assert!(Cursor::new("a".repeat(MAX_CURSOR_LEN)).is_ok());
        assert_eq!(
            Cursor::new("a".repeat(MAX_CURSOR_LEN + 1)),
            Err(CursorError::Malformed)
        );
    }

    #[test]
    fn a_missing_cursor_is_none() {
        assert_eq!(Cursor::from_query("from=a&to=b"), Ok(None));
        assert_eq!(Cursor::from_query(""), Ok(None));
        assert_eq!(
            Cursor::from_query("x=1&cursor=abc"),
            Ok(Some(Cursor::new("abc").unwrap()))
        );
    }

    #[test]
    fn a_repeated_or_empty_cursor_is_400() {
        assert_eq!(
            Cursor::from_query("cursor=a&cursor=b"),
            Err(CursorError::Repeated)
        );
        assert_eq!(Cursor::from_query("cursor="), Err(CursorError::Empty));
        assert_eq!(Cursor::from_query("cursor"), Err(CursorError::Empty));
        assert_eq!(CursorError::Empty.to_response().status, 400);
    }

    #[test]
    fn a_bad_cursor_400_never_echoes_it() {
        let marker = "ZQX<MARKER>7731";
        let err = Cursor::from_query(&format!("cursor={marker}")).unwrap_err();
        let resp = err.to_response();
        assert_eq!(resp.status, 400);
        let body = String::from_utf8(resp.body.as_bytes().unwrap().to_vec()).unwrap();
        assert!(!body.contains("MARKER"), "{body}");
        assert!(body.ends_with('\n'));
        assert_eq!(header(&resp, "Cache-Control"), Some("no-store"));
        assert_eq!(header(&resp, "X-Content-Type-Options"), Some("nosniff"));
    }
}
