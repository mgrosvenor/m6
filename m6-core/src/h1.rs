//! The one HTTP/1.1 request parser.
//!
//! There were four. Measured against h1spec, the independent RFC 9112
//! conformance tester:
//!
//! | implementation | score |
//! |---|---|
//! | `m6-http/src/http11.rs` (this one, moved here) | **27/32** |
//! | `m6-file/src/http.rs` | 15/32 |
//! | `m6-render/src/server.rs` | 15/32 |
//! | `m6-core/src/parse.rs` (parser removed, see below) | **14/32** |
//!
//! The migration plan said to consolidate onto `m6-core/src/parse.rs`, which
//! turned out to be the *worst* of the four. Measuring the candidate before
//! moving anything onto it is what caught that; the survivor is the parser the
//! edge already used, because it is the only one that has been attacked.
//!
//! **`parse.rs` still exists, and is on the production path of every socket
//! backend** through [`crate::server::serve_connection`]. What was deleted is
//! the parser that used to be *inside* it: it is now a stream adapter that
//! reads bytes into a buffer and asks [`parse_request`] whether the message is
//! complete, with no framing logic of its own. Reading that table as "the file
//! is gone" costs whoever does it a detour through
//! `server::serve_connection` looking for a second parser that is not there.
//!
//! **This function is pure.** The edge's version stripped proxy-owned headers
//! (`X-Forwarded-For` and friends) inline while parsing, which is ingress
//! policy wearing a parser's clothes. That moves to the caller:
//! `m6_http_lib::forward::strip_untrusted_inbound` already exists and does
//! exactly this, and a backend has no proxy headers to strip.

use crate::http::RawRequest;
use crate::parse::{MAX_BODY_BYTES, MAX_HEADER_BYTES};
use crate::response::{Body, Response};
use crate::server::MAX_REQUESTS_PER_CONN;
use std::io;
use std::io::Read as _;
use std::io::Write;

/// Reduce an absolute-form request target to its path.
///
/// `http://example.com/a/b?c` becomes `/a/b?c`. Anything already in origin-form,
/// asterisk-form (`OPTIONS *`) or authority-form (`CONNECT host:port`) is
/// returned unchanged: only absolute-form carries a scheme.
///
/// A scheme-relative target (`//host/path`) is left alone. It is not
/// absolute-form, and treating it as one would let `//evil.example.com/x` be rewritten
/// to `/x` -- turning a request the router should refuse into one it serves.
/// Is this a `Host` field value a server may act on (RFC 9110 4.2, RFC 3986)?
///
/// `Host` decides which site a request is for, so it is echoed into redirects,
/// used as a cache key and handed to backends. A value with a space in it is
/// not one authority: it is two tokens that different hops will split
/// differently. The permitted set is the RFC 3986 authority alphabet, which
/// excludes whitespace, controls and the delimiters that would let a value
/// escape whatever it is later interpolated into.
fn host_is_valid(v: &str) -> bool {
    if v.len() > 253 {
        return false;
    }
    // An IPv6 literal is bracketed and the only place `:` may repeat.
    let (host, port) = match v.strip_prefix('[') {
        Some(rest) => match rest.split_once(']') {
            Some((inside, after)) => {
                if !inside
                    .bytes()
                    .all(|b| b.is_ascii_hexdigit() || b == b':' || b == b'.')
                {
                    return false;
                }
                (None, after)
            }
            None => return false,
        },
        None => match v.split_once(':') {
            Some((h, p)) => (Some(h), p),
            None => (Some(v), ""),
        },
    };

    if let Some(host) = host {
        if host.is_empty() {
            return false;
        }
        // reg-name: unreserved / pct-encoded / sub-delims.
        let ok = |b: u8| {
            b.is_ascii_alphanumeric()
                || matches!(
                    b,
                    b'-' | b'.'
                        | b'_'
                        | b'~'
                        | b'%'
                        | b'!'
                        | b'$'
                        | b'&'
                        | b'\''
                        | b'('
                        | b')'
                        | b'*'
                        | b'+'
                        | b','
                        | b';'
                        | b'='
                )
        };
        if !host.bytes().all(ok) {
            return false;
        }
    }

    let port = port.strip_prefix(':').unwrap_or(port);
    port.is_empty() || (port.len() <= 5 && port.bytes().all(|b| b.is_ascii_digit()))
}

/// The interim response a client waiting on `Expect: 100-continue` needs.
pub const CONTINUE_RESPONSE: &[u8] = b"HTTP/1.1 100 Continue\r\n\r\n";

/// What a client's `Expect` field asks of the server (RFC 9110 10.1.1).
#[derive(Debug, PartialEq, Eq)]
pub enum Expectation {
    /// No `Expect` field, or one this version of HTTP ignores. Read the body.
    None,
    /// `Expect: 100-continue`. The client is holding the body back until it
    /// hears that the request head was acceptable. Answer with
    /// [`CONTINUE_RESPONSE`], or with a final status, *before* reading on.
    ///
    /// Answering is not optional in practice: a server that says nothing makes
    /// every such client wait out its own timeout (curl waits a full second)
    /// before sending the body anyway. That was the behaviour here.
    Continue,
    /// An expectation this server does not understand. RFC 9110 10.1.1: answer
    /// 417 and do not read a body. Forwarding an expectation we cannot honour
    /// would be worse, because the client would keep waiting for a response
    /// nothing in the chain is going to send.
    Unsupported,
}

/// Read the `Expect` field from a request head.
///
/// Returns `None` while the header block is still arriving: until the blank
/// line lands, a later byte could still introduce an `Expect`, so there is
/// nothing to conclude yet.
///
/// HTTP/1.0 clients get [`Expectation::None`] whatever they sent. `100
/// (Continue)` is an HTTP/1.1 interim response and an HTTP/1.0 client has no
/// way to read one: it would take the status line as the response.
pub fn expectation(buf: &[u8]) -> Option<Expectation> {
    let end = buf.windows(4).position(|w| w == b"\r\n\r\n")?;
    let head = &buf[..end];

    let mut lines = head.split(|&b| b == b'\n');
    // The request line is not a field, so `Expect` cannot appear in it, and
    // the version it carries decides whether an interim response is readable.
    let request_line = lines.next()?;
    if !request_line.ends_with(b"HTTP/1.1\r") && !request_line.ends_with(b"HTTP/1.1") {
        return Some(Expectation::None);
    }

    let mut found = Expectation::None;
    for line in lines {
        let Some(colon) = line.iter().position(|&b| b == b':') else {
            continue;
        };
        if !line[..colon].eq_ignore_ascii_case(b"expect") {
            continue;
        }
        // A field may be repeated or comma-separated, and every member has to
        // be one we understand before we can read a body.
        for member in line[colon + 1..].split(|&b| b == b',') {
            let member = trim_ascii(member);
            if member.is_empty() {
                continue;
            }
            if member.eq_ignore_ascii_case(b"100-continue") {
                if found == Expectation::None {
                    found = Expectation::Continue;
                }
            } else {
                return Some(Expectation::Unsupported);
            }
        }
    }
    Some(found)
}

fn trim_ascii(mut b: &[u8]) -> &[u8] {
    while let [first, rest @ ..] = b {
        if first.is_ascii_whitespace() {
            b = rest
        } else {
            break;
        }
    }
    while let [rest @ .., last] = b {
        if last.is_ascii_whitespace() {
            b = rest
        } else {
            break;
        }
    }
    b
}

/// Largest decoded chunked body accepted, matching the Content-Length cap.
const MAX_CHUNKED_BODY: usize = 16 * 1024 * 1024;

/// Outcome of decoding a chunked body in place.
enum Chunked {
    /// Fully decoded: `decoded` bytes at the front of the buffer, from `raw`
    /// bytes of chunk framing at its start.
    Done { decoded: usize, raw: usize },
    /// A complete body has not arrived yet; read more and retry.
    Incomplete,
    /// Malformed framing. Not recoverable: the connection cannot be trusted to
    /// resynchronise, because the next bytes might be a smuggled request.
    Bad,
}

/// Decode a chunked transfer-coded body in place (RFC 9112 7.1).
///
/// RFC 9112 7.1 makes this mandatory: "A server MUST be able to receive and
/// decode the chunked transfer coding". It was refused outright before, with
/// the reasoning that forwarding a body the proxy never read would be worse --
/// true, and the answer is to read it, not to refuse every client that streams
/// a request.
///
/// Decoding here rather than forwarding the coding onward is also the safer
/// choice: the backend receives a plain `Content-Length` body, so the proxy
/// and the backend cannot disagree about where the request ends. That
/// disagreement is request smuggling.
///
/// In place because the bytes only shrink: each chunk's data is copied behind
/// its framing, so a connection's buffer holds the decoded body with no second
/// allocation, and a pipelined request behind the raw body stays where it is.
fn decode_chunked(buf: &mut [u8]) -> Chunked {
    let mut out = 0usize;
    let mut i = 0usize;

    loop {
        // chunk-size [ ";" chunk-ext ] CRLF
        let Some(eol) = find_crlf(&buf[i..]) else {
            return Chunked::Incomplete;
        };
        // Extensions are permitted and ignored; the size ends at ';'. No sign,
        // no whitespace, no underscores: a lenient size parser is exactly how
        // two hops end up disagreeing.
        let size = {
            let line = &buf[i..i + eol];
            let size_str = match line.iter().position(|&c| c == b';') {
                Some(semi) => &line[..semi],
                None => line,
            };
            if size_str.is_empty() || size_str.len() > 16 {
                return Chunked::Bad;
            }
            let mut size: usize = 0;
            for &c in size_str {
                let d = match c {
                    b'0'..=b'9' => c - b'0',
                    b'a'..=b'f' => c - b'a' + 10,
                    b'A'..=b'F' => c - b'A' + 10,
                    _ => return Chunked::Bad,
                };
                size = match size.checked_mul(16).and_then(|v| v.checked_add(d as usize)) {
                    Some(v) => v,
                    None => return Chunked::Bad,
                };
            }
            size
        };
        i += eol + 2;

        if size == 0 {
            // Last chunk. Trailer fields may follow, ending with a blank line.
            loop {
                let Some(eol) = find_crlf(&buf[i..]) else {
                    return Chunked::Incomplete;
                };
                i += eol + 2;
                if eol == 0 {
                    return Chunked::Done {
                        decoded: out,
                        raw: i,
                    };
                }
            }
        }

        if out + size > MAX_CHUNKED_BODY {
            return Chunked::Bad;
        }
        if buf.len() < i + size + 2 {
            return Chunked::Incomplete;
        }
        buf.copy_within(i..i + size, out);
        out += size;
        i += size;
        // Every chunk's data is followed by CRLF, exactly.
        if &buf[i..i + 2] != b"\r\n" {
            return Chunked::Bad;
        }
        i += 2;
    }
}

fn find_crlf(buf: &[u8]) -> Option<usize> {
    buf.windows(2).position(|w| w == b"\r\n")
}

fn strip_absolute_form(target: &str) -> &str {
    let Some(scheme_end) = target.find("://") else {
        return target;
    };
    // A scheme is `ALPHA *( ALPHA / DIGIT / "+" / "-" / "." )` (RFC 3986 3.1).
    // Anything else before "://" is not a scheme, so this is not absolute-form.
    let scheme = &target[..scheme_end];
    if scheme.is_empty()
        || !scheme.starts_with(|c: char| c.is_ascii_alphabetic())
        || !scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
    {
        return target;
    }
    let after_scheme = &target[scheme_end + 3..];
    match after_scheme.find('/') {
        Some(slash) => &after_scheme[slash..],
        // `http://example.com` with no path at all means the origin's root.
        None => "/",
    }
}

/// Outcome of parsing a client request. Public so security tests can assert on
/// what the ingress boundary accepts, rejects, and strips.
pub enum ParseResult {
    Incomplete,
    Error,
    Complete(RawRequest),
}

/// A request head parsed into slices of the input buffer, with no allocation.
///
/// Every string borrows from `buf` (the method, the path, the query and every
/// header name and value) except `version`, which is one of the two static
/// HTTP/1.x strings. This is the borrowed half of [`parse_request`]: the same
/// checks run once, and the owned parser copies the result.
pub struct RequestHead<'a> {
    pub method: &'a str,
    pub path: &'a str,
    pub query: Option<&'a str>,
    pub version: &'a str,
    pub headers: &'a [httparse::Header<'a>],
    /// Byte offset in `buf` where the body begins (the end of the head).
    pub head_len: usize,
    /// Body length for a `Content-Length` request, 0 otherwise. A chunked
    /// request carries no `Content-Length`, so it reads 0 here and
    /// [`RequestHead::has_body`] finds the `Transfer-Encoding` header instead.
    pub body_len: usize,
    /// The body bytes, as far as they have arrived. Empty until the body is
    /// complete, and filled by the chunked decoder before a slot returns it.
    body: &'a [u8],
}

impl<'a> RequestHead<'a> {
    /// Whether a body follows the head: `Content-Length` above zero, or chunked.
    pub fn has_body(&self) -> bool {
        self.body_len > 0
            || self
                .headers
                .iter()
                .any(|h| h.name.eq_ignore_ascii_case("transfer-encoding"))
    }

    /// The body bytes this head framed. One accessor rather than a stored
    /// `Vec`, so a handler reads the bytes the parser already has.
    pub fn body(&self) -> &'a [u8] {
        self.body
    }
}

/// Outcome of parsing a request head. Mirrors [`ParseResult`] for the borrowed
/// head: `Complete` carries no body, only its length.
pub enum HeadResult<'a> {
    Incomplete,
    Error,
    Complete(RequestHead<'a>),
}

/// Parse a request head into borrowed slices of `buf`.
///
/// This is the shared half of [`parse_request`]: every framing check the owned
/// parser makes lives here, so the two cannot disagree about what is accepted.
/// The owned parser calls this and copies the head into `RawRequest`.
pub fn parse_head<'a>(
    buf: &'a [u8],
    headers: &'a mut [httparse::Header<'a>; 64],
) -> HeadResult<'a> {
    let mut req = httparse::Request::new(headers);
    let body_offset = match req.parse(buf) {
        Ok(httparse::Status::Partial) => return HeadResult::Incomplete,
        Ok(httparse::Status::Complete(n)) => n,
        Err(_) => return HeadResult::Error,
    };
    finish_head(req, buf, body_offset)
}

/// As [`parse_head`], over header slots that start uninitialised.
///
/// This is the form a service's loop uses: its fixed array of slots is never
/// written with zeros (the same shape as `client::Upstream`'s reply slots),
/// and every entry the parser fills is initialised before the head is
/// returned.
pub fn parse_head_uninit<'a>(
    buf: &'a [u8],
    headers: &'a mut [std::mem::MaybeUninit<httparse::Header<'a>>; 64],
) -> HeadResult<'a> {
    let mut req = httparse::Request::new(&mut []);
    let body_offset = match httparse::ParserConfig::default().parse_request_with_uninit_headers(
        &mut req,
        buf,
        &mut headers[..],
    ) {
        Ok(httparse::Status::Partial) => return HeadResult::Incomplete,
        Ok(httparse::Status::Complete(n)) => n,
        Err(_) => return HeadResult::Error,
    };
    finish_head(req, buf, body_offset)
}

/// The checks both parse entry points make once the head bytes are framed.
fn finish_head<'a>(
    req: httparse::Request<'a, 'a>,
    buf: &'a [u8],
    body_offset: usize,
) -> HeadResult<'a> {
    // Reject a bare LF anywhere in the header block. (See `parse_request`'s
    // original note: the split it permits is the smuggling primitive.)
    {
        let head = &buf[..body_offset];
        let mut i = 0;
        while let Some(off) = head[i..].iter().position(|&c| c == b'\n') {
            let at = i + off;
            if at == 0 || head[at - 1] != b'\r' {
                return HeadResult::Error;
            }
            i = at + 1;
        }
    }

    let method = req.method.unwrap_or("GET");
    let raw_path = req.path.unwrap_or("/");

    // Absolute-form is reduced to its path, exactly as the owned parser does.
    let target = strip_absolute_form(raw_path);
    let (path, query) = match target.find('?') {
        Some(q) => (&target[..q], Some(&target[q + 1..])),
        None => (target, None),
    };

    let nheaders = req.headers.len();

    // Framing validation: one pass, no allocation. The owned parser copies the
    // surviving headers afterwards; the checks themselves are shared.
    let mut first_cl: Option<&str> = None;
    let mut saw_cl = false;
    let mut host_count = 0usize;
    let mut host_empty = false;
    let mut saw_te = false;

    for h in &req.headers[..nheaders] {
        if h.name.eq_ignore_ascii_case("transfer-encoding") {
            let Ok(v) = std::str::from_utf8(h.value) else {
                return HeadResult::Error;
            };
            let last = v.rsplit(',').next().unwrap_or("").trim();
            if !last.eq_ignore_ascii_case("chunked") {
                return HeadResult::Error;
            }
            if v.split(',').count() > 1 {
                return HeadResult::Error;
            }
            saw_te = true;
            continue;
        }
        if h.name.eq_ignore_ascii_case("content-length") {
            let Ok(value) = std::str::from_utf8(h.value) else {
                return HeadResult::Error;
            };
            let value = value.trim();
            match first_cl {
                Some(prev) if prev != value => return HeadResult::Error,
                Some(_) => {}
                None => first_cl = Some(value),
            }
            saw_cl = true;
        }
        if h.name.eq_ignore_ascii_case("host") {
            host_count += 1;
            match std::str::from_utf8(h.value).map(str::trim) {
                Ok(v) if !v.is_empty() => {
                    if !host_is_valid(v) {
                        return HeadResult::Error;
                    }
                }
                _ => host_empty = true,
            }
        }
    }

    if host_count > 1 {
        return HeadResult::Error;
    }
    let is_http11 = req.version == Some(1);
    if is_http11 && (host_count == 0 || host_empty) {
        return HeadResult::Error;
    }
    if saw_te && !is_http11 {
        return HeadResult::Error;
    }
    if saw_te && saw_cl {
        return HeadResult::Error;
    }

    let body_len: usize = match first_cl {
        Some(v) => match v.parse() {
            Ok(n) => n,
            Err(_) => return HeadResult::Error,
        },
        None if saw_cl => return HeadResult::Error,
        None => 0,
    };

    let version = if is_http11 { "HTTP/1.1" } else { "HTTP/1.0" };

    // The body bytes are here only when the whole body has arrived. A caller
    // that frames a message itself waits for `head_len + body_len` bytes
    // before it hands the head on.
    let body: &'a [u8] = if saw_te {
        &[]
    } else {
        let end = body_offset + body_len;
        if buf.len() >= end {
            &buf[body_offset..end]
        } else {
            &[]
        }
    };

    let headers: &'a [httparse::Header<'a>] = req.headers;

    HeadResult::Complete(RequestHead {
        method,
        path,
        query,
        version,
        headers,
        head_len: body_offset,
        body_len,
        body,
    })
}

pub fn parse_request(buf: &[u8]) -> ParseResult {
    let mut headers = [httparse::EMPTY_HEADER; 64];
    let head = match parse_head(buf, &mut headers) {
        HeadResult::Incomplete => return ParseResult::Incomplete,
        HeadResult::Error => return ParseResult::Error,
        HeadResult::Complete(h) => h,
    };
    materialize(&head, buf)
}

/// Copy a parsed head into an owned [`RawRequest`], reading and framing the
/// body. The borrowed half of the parser: a caller that already ran
/// [`parse_head`] (the HTTP/1.1 fast path) calls this on the miss, so the
/// head is not parsed twice.
pub fn materialize(head: &RequestHead<'_>, buf: &[u8]) -> ParseResult {
    let method = head.method.to_string();
    let path = head.path.to_string();
    let query = head.query.map(str::to_string);
    let version = head.version.to_string();

    // Copy the surviving headers: transfer-coding is hop-by-hop and not
    // forwarded, and a value that is not UTF-8 is dropped rather than rejected
    // (matching prior behaviour).
    let mut fwd_headers: Vec<(String, String)> = Vec::with_capacity(head.headers.len());
    for h in head.headers {
        if h.name.eq_ignore_ascii_case("transfer-encoding") {
            continue;
        }
        let Ok(v) = std::str::from_utf8(h.value) else {
            continue;
        };
        fwd_headers.push((h.name.to_string(), v.to_string()));
    }

    let chunked = head
        .headers
        .iter()
        .any(|h| h.name.eq_ignore_ascii_case("transfer-encoding"));
    let body = if chunked {
        // Decoded here, so the body handed on is a plain one of known length
        // and no downstream hop has to agree with us about chunk framing. The
        // copy is the owned body this function exists to produce, and the
        // decoder shrinks it in place.
        let mut body = buf[head.head_len..].to_vec();
        match decode_chunked(&mut body) {
            Chunked::Incomplete => return ParseResult::Incomplete,
            Chunked::Bad => return ParseResult::Error,
            Chunked::Done { decoded, .. } => {
                body.truncate(decoded);
                fwd_headers.push(("Content-Length".to_string(), decoded.to_string()));
                body
            }
        }
    } else {
        let available = buf.len() - head.head_len;
        if available < head.body_len {
            return ParseResult::Incomplete;
        }
        buf[head.head_len..head.head_len + head.body_len].to_vec()
    };

    ParseResult::Complete(RawRequest {
        method,
        path,
        query,
        version,
        headers: fwd_headers,
        body,
    })
}

// ── Responses ────────────────────────────────────────────────────────────────

/// The reason phrase for a status code.
///
/// Reason phrases are advisory (RFC 9112 4.1) but an incorrect one is worse
/// than a terse one: this used to answer `HTTP/1.1 501 Unknown`, a real status
/// with a phrase that described nothing.
pub fn status_reason(status: u16) -> &'static str {
    match status {
        100 => "Continue",
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        206 => "Partial Content",
        301 => "Moved Permanently",
        302 => "Found",
        304 => "Not Modified",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        412 => "Precondition Failed",
        413 => "Payload Too Large",
        417 => "Expectation Failed",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => "Unknown",
    }
}

/// Whether this request leaves the connection open (RFC 9112 9.3).
///
/// HTTP/1.1 is persistent by default and closes only if asked. HTTP/1.0 is the
/// reverse: it closes unless the client asked to keep it. Anything older, or a
/// version we do not recognise, closes.
pub fn keep_alive(req: &RawRequest) -> bool {
    keep_alive_lines(
        &req.version,
        req.headers
            .iter()
            .map(|(k, v)| (k.as_bytes(), v.as_bytes())),
    )
}

/// [`keep_alive`] for an already parsed head: the same decision, without an
/// owned request to read it from.
pub fn keep_alive_head(head: &RequestHead<'_>) -> bool {
    keep_alive_lines(
        head.version,
        head.headers.iter().map(|h| (h.name.as_bytes(), h.value)),
    )
}

/// The one keep-alive rule (RFC 9112 9.3), over either header representation.
///
/// EVERY `Connection` field line counts, not just the first. RFC 9110 5.3:
/// repeated field lines are equivalent to one comma-joined value, so a request
/// carrying `Connection: keep-alive` and `Connection: close` means
/// `keep-alive, close` and must close.
fn keep_alive_lines<'a>(
    version: &str,
    headers: impl Iterator<Item = (&'a [u8], &'a [u8])>,
) -> bool {
    let (mut close, mut keep) = (false, false);
    for (name, value) in headers {
        if !name.eq_ignore_ascii_case(b"connection") {
            continue;
        }
        for token in value.split(|&b| b == b',') {
            let token = trim_ascii(token);
            if token.eq_ignore_ascii_case(b"close") {
                close = true;
            } else if token.eq_ignore_ascii_case(b"keep-alive") {
                keep = true;
            }
        }
    }

    if version.eq_ignore_ascii_case("HTTP/1.1") {
        !close
    } else if version.eq_ignore_ascii_case("HTTP/1.0") {
        keep
    } else {
        false
    }
}

/// The one way an m6 backend answers an HTTP/1.1 request.
///
/// Handlers are handed this instead of the raw stream, which is the point:
/// two rules have to hold for **every** response, and a handler that writes
/// the stream itself can only be trusted to remember them by inspection.
///
/// 1. **A HEAD response carries no body** (RFC 9110 9.3.2), while its
///    `Content-Length` still describes the representation a GET would have
///    returned. m6-file had a `write_head_response` for this and used it on
///    the success path only, so every 404, 405 and 400 answering a HEAD went
///    out with a body attached. m6-http fixed the identical defect months
///    earlier and the fix was never carried across, because nothing tested it.
///
/// 2. **Persistence is the connection's decision, not the handler's**
///    (RFC 9112 9.3). Every backend hardcoded `Connection: close`, so m6-http
///    paid a fresh connect to m6-file or m6-html on every cache miss.
pub struct Responder<'a, W: std::io::Write> {
    w: &'a mut W,
    /// Decides whether a body is written at all.
    ///
    /// Stored as the decision rather than as the method string, which is the
    /// only thing it was ever used for (`eq_ignore_ascii_case("HEAD")`).
    /// Holding a `&'a str` tied the responder's lifetime to the request that
    /// produced it, which is why `serve_connection` could not hand the request
    /// to its handler and every `App` service cloned one per request instead.
    is_head: bool,
    /// Decided by the connection from the request, before the handler runs.
    keep_alive: bool,
    /// Body bytes actually written, for the caller's access log.
    written: usize,
}

impl<'a, W: std::io::Write> Responder<'a, W> {
    /// Answer a request whose framing the connection has already decided.
    pub fn new(w: &'a mut W, method: &str, keep_alive: bool) -> Self {
        Responder {
            w,
            is_head: method.eq_ignore_ascii_case("HEAD"),
            keep_alive,
            written: 0,
        }
    }

    /// Whether the connection stays open after this response.
    pub fn keeps_alive(&self) -> bool {
        self.keep_alive
    }

    /// Body bytes written so far.
    pub fn body_bytes(&self) -> usize {
        self.written
    }

    /// Send a response with `body` as its representation.
    ///
    /// On HEAD the bytes are not sent, but `Content-Length` still reports
    /// their number: that is what a GET would have returned, which is exactly
    /// what the field is for.
    pub fn send(
        &mut self,
        status: u16,
        headers: &[(&str, &str)],
        body: &[u8],
    ) -> std::io::Result<()> {
        self.send_with_length(status, headers, body, body.len())
    }

    /// A plain-text error response.
    pub fn error(&mut self, status: u16) -> std::io::Result<()> {
        let reason = status_reason(status);
        let body = format!("{status} {reason}");
        self.send(status, &[("Content-Type", "text/plain")], body.as_bytes())
    }

    /// Send a response whose `Content-Length` is known separately from the
    /// bytes in hand -- a HEAD answered without reading the file, say.
    ///
    /// `body` is written only when it is the representation; `length` is
    /// always what the header reports.
    pub fn send_with_length(
        &mut self,
        status: u16,
        headers: &[(&str, &str)],
        body: &[u8],
        length: usize,
    ) -> std::io::Result<()> {
        let bodyless = self.write_head(status, headers, length as u64)?;
        if !bodyless {
            self.w.write_all(body)?;
            self.w.flush()?;
            self.written += body.len();
        }
        Ok(())
    }

    /// Send a response whose body is copied from a reader rather than held in
    /// memory.
    ///
    /// **`length` is a promise already on the wire.** m6 frames with
    /// `Content-Length`, so by the time the first body byte is written the peer
    /// has been told exactly how many are coming. That makes a short read a
    /// framing fault and not a small disappointment: whatever is written next
    /// on a reused connection would be read as the tail of this body. So a
    /// reader that ends early returns `UnexpectedEof` here, and
    /// `serve_connection` closes the connection rather than continuing on it.
    /// The peer then sees fewer bytes than promised followed by EOF, which it
    /// must discard. That is the honest failure; silently sending fewer bytes
    /// than the header claims is the smuggling primitive this codebase rejects
    /// on ingress.
    ///
    /// The reader is capped at `length` as well as required to reach it, so a
    /// file that grew between `stat` and `read` cannot overrun the promise
    /// either.
    ///
    /// On HEAD the reader is never touched, which is the point: the file is
    /// not opened, let alone read.
    pub fn send_stream<R: std::io::Read>(
        &mut self,
        status: u16,
        headers: &[(&str, &str)],
        length: u64,
        body: R,
    ) -> std::io::Result<()> {
        let bodyless = self.write_head(status, headers, length)?;
        if bodyless {
            return Ok(());
        }

        // 64 KiB: large enough that a multi-megabyte asset is tens of writes
        // rather than thousands, small enough that concurrent streams do not
        // each cost a meaningful slice of a 950MB origin.
        use std::io::Read as _;
        let mut buf = [0u8; 64 * 1024];
        let mut remaining = length;
        let mut reader = body.take(length);
        while remaining > 0 {
            let n = reader.read(&mut buf)?;
            if n == 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    format!(
                        "body ended {remaining} bytes short of the \
                         Content-Length already sent"
                    ),
                ));
            }
            self.w.write_all(&buf[..n])?;
            self.written += n;
            remaining -= n as u64;
        }
        self.w.flush()?;
        Ok(())
    }

    /// Write the status line and headers. Returns whether a body may follow.
    ///
    /// One implementation, because the two senders differ only in where the
    /// bytes come from, and a second copy of the `Content-Length`/`Connection`
    /// handling is exactly how two responses end up framed differently.
    fn write_head(
        &mut self,
        status: u16,
        headers: &[(&str, &str)],
        length: u64,
    ) -> std::io::Result<bool> {
        // One buffered writer, so the head is one write syscall rather than
        // one per header. A caller whose writer is already a buffer (a
        // connection slot) calls `write_head_to` directly and skips this
        // allocation.
        let mut w = std::io::BufWriter::with_capacity(1024, &mut *self.w);
        let bodyless = write_head_to(
            &mut w,
            status,
            headers,
            length,
            self.is_head,
            self.keep_alive,
        )?;
        w.flush()?;
        drop(w);
        Ok(bodyless)
    }
}

/// Write a response head into `w`, with no buffering of its own.
///
/// The one place the status line, the caller's fields, `Content-Length` and the
/// persistence decision are written. `Responder` wraps `w` in a `BufWriter` so
/// a socket gets one write per head; a connection slot's writer is already the
/// slot's buffer, so it calls this directly. Returns whether a body follows.
fn write_head_to<W: std::io::Write>(
    w: &mut W,
    status: u16,
    headers: &[(&str, &str)],
    length: u64,
    is_head: bool,
    keep_alive: bool,
) -> std::io::Result<bool> {
    write!(w, "HTTP/1.1 {} {}\r\n", status, status_reason(status))?;
    for (k, v) in headers {
        // Any caller copy of a field this function emits itself is dropped,
        // or the response goes out carrying both. Two `Content-Length`
        // fields with different values is the framing ambiguity the parser
        // refuses to accept from anyone else.
        if k.eq_ignore_ascii_case("content-length") || k.eq_ignore_ascii_case("connection") {
            continue;
        }
        w.write_all(k.as_bytes())?;
        w.write_all(b": ")?;
        w.write_all(v.as_bytes())?;
        w.write_all(b"\r\n")?;
    }
    write!(
        w,
        "Content-Length: {}\r\nConnection: {}\r\n\r\n",
        length,
        if keep_alive { "keep-alive" } else { "close" }
    )?;

    // RFC 9110 9.3.2. A 304 and a 204 have no body either (RFC 9110 15.4.5,
    // 15.3.5), and one sent on those is unframed bytes the peer will read
    // as the start of the next response.
    Ok(is_head || status == 204 || status == 304 || (100..200).contains(&status))
}

// ── One connection slot ──────────────────────────────────────────────────────

/// Header slots a service's loop hands to [`ServerConn::read`].
///
/// Made once at start with [`head_slots`], one array per connection or one per
/// loop, never one per request. The slots are `MaybeUninit` so making the
/// array writes nothing: the parser fills exactly the entries it uses.
pub type HeadSlots<'a> = [std::mem::MaybeUninit<httparse::Header<'a>>; 64];

/// A new set of empty header slots.
pub fn head_slots<'a>() -> HeadSlots<'a> {
    [const { std::mem::MaybeUninit::uninit() }; 64]
}

/// What one read turn produced.
pub enum Read<'a> {
    /// No complete request yet. Read again when the socket is readable.
    More,
    /// One complete request, borrowed from the slot's buffers and the caller's
    /// header slots. Dropped before the connection is used again.
    Request(RequestHead<'a>),
    /// The peer closed the connection.
    Closed,
}

/// What one flush turn did.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Flush {
    /// Everything is written and the connection keeps alive.
    Done,
    /// The socket would block. Write again when it is writable.
    More,
    /// Everything is written and the connection closes.
    Close,
}

/// Buffered body bytes still to write, and the reader they come from.
struct StreamBody {
    remaining: u64,
    reader: Box<dyn std::io::Read + Send>,
}

/// How many bytes one flush turn reads from a stream body. The cost the owner
/// accepted for regular files: one `read(2)` per writable turn.
const STREAM_CHUNK: u64 = 64 * 1024;

/// One HTTP/1.1 connection slot for a service's own loop.
///
/// A slot is made at start with its buffers ([`ServerConn::new`]), a stream is
/// attached on accept and detached on close. Reading, parsing and writing go
/// through those buffers, so serving a request allocates only when a request
/// or a response is larger than them.
///
/// The turn loop a service writes per connection:
///
/// ```no_run
/// # fn serve(conn: &mut m6_core::h1::ServerConn) -> std::io::Result<()> {
/// use m6_core::h1::{Flush, Read};
///
/// loop {
///     // Made here, not before the loop: the head borrows the slots, and a
///     // mutable reference to a long-lived array cannot be reborrowed for a
///     // shorter head on each turn. `head_slots` writes nothing.
///     let mut heads = m6_core::h1::head_slots();
///     let read = conn.read(&mut heads)?;
///     let answered = match read {
///         // The head borrows the slot. It is dropped here, before the answer,
///         // so the slot can be written to below.
///         Read::Request(_head) => true,
///         // Nothing more to read. Flush an answer queued for a refusal.
///         Read::More => false,
///         Read::Closed => return Ok(()),
///     };
///     if answered {
///         // `site.handle(&head)` goes here, then `respond_response`.
///         conn.respond(200, &[("Content-Type", "text/plain")], b"ok");
///     }
///     match conn.flush()? {
///         Flush::Done if answered => {}
///         Flush::Done => return Ok(()),
///         Flush::More | Flush::Close => return Ok(()),
///     }
/// }
/// # }
/// ```
pub struct ServerConn {
    stream: Option<mio::net::UnixStream>,
    /// Input: `in_buf[..in_len]` is valid, `in_buf.len()` is how much room the
    /// slot has. The buffer is filled directly, so no zeroing per read.
    in_buf: Vec<u8>,
    in_len: usize,
    in_read_cap: usize,
    /// Bytes of the buffer the request being answered occupies, dropped when
    /// the next `read` starts.
    consumed: std::cell::Cell<usize>,
    /// Output: `out_buf[..out_len]` is queued, `out_pos` bytes of it written.
    out_buf: Vec<u8>,
    out_len: usize,
    out_pos: usize,
    out_write_cap: usize,
    body: Option<StreamBody>,
    is_head: std::cell::Cell<bool>,
    keep_alive: std::cell::Cell<bool>,
    served: std::cell::Cell<u32>,
    close_after: bool,
    expect_answered: bool,
}

impl ServerConn {
    /// A slot with `read_cap` and `write_cap` bytes. Start only.
    pub fn new(read_cap: usize, write_cap: usize) -> ServerConn {
        assert!(
            read_cap > 0 && write_cap > 0,
            "a connection slot needs both buffers"
        );
        ServerConn {
            stream: None,
            in_buf: vec![0; read_cap],
            in_len: 0,
            in_read_cap: read_cap,
            consumed: std::cell::Cell::new(0),
            out_buf: vec![0; write_cap],
            out_len: 0,
            out_pos: 0,
            out_write_cap: write_cap,
            body: None,
            is_head: std::cell::Cell::new(false),
            keep_alive: std::cell::Cell::new(true),
            served: std::cell::Cell::new(0),
            close_after: false,
            expect_answered: false,
        }
    }

    /// Attach an accepted stream. Resets what the previous connection left.
    pub fn attach(&mut self, stream: mio::net::UnixStream) {
        self.reset();
        self.stream = Some(stream);
    }

    /// Detach the stream, if any, and reset the slot for its next connection.
    pub fn detach(&mut self) -> Option<mio::net::UnixStream> {
        let stream = self.stream.take();
        self.reset();
        stream
    }

    /// The attached stream, for `mio`'s register and reregister.
    pub fn stream(&mut self) -> Option<&mut mio::net::UnixStream> {
        self.stream.as_mut()
    }

    /// `READABLE`, or `WRITABLE` while an answer is owed.
    pub fn interest(&self) -> mio::Interest {
        if self.out_pos < self.out_len || self.body.is_some() {
            mio::Interest::WRITABLE
        } else {
            mio::Interest::READABLE
        }
    }

    /// Read and parse one request. Never waits: `More` when the socket has
    /// nothing complete, `Closed` when the peer is gone.
    ///
    /// A request already complete in the buffer is returned without a read, so
    /// a pipelined second request is served on the same readiness event. A
    /// refusal a malformed request earns is queued as a response and returned
    /// as `More`, with the connection marked to close after the flush.
    pub fn read<'a>(&'a mut self, heads: &'a mut HeadSlots<'a>) -> io::Result<Read<'a>> {
        if self.stream.is_none() {
            return Ok(Read::Closed);
        }
        self.drop_consumed();

        loop {
            // The decision step borrows the slot only for its own call, so its
            // result is owned and the slot can be used again below.
            match inspect(&self.in_buf[..self.in_len]) {
                Next::Complete {
                    len,
                    is_head,
                    keep_alive,
                } => {
                    let parsed = parse_head_uninit(&self.in_buf[..self.in_len], heads);
                    let HeadResult::Complete(head) = parsed else {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "a request head changed between two parses",
                        ));
                    };
                    self.begin(is_head, keep_alive, len);
                    return Ok(Read::Request(head));
                }
                Next::Error(status) => {
                    self.queue_error(status);
                    return Ok(Read::More);
                }
                Next::Need(total) => {
                    self.answer_expectation();
                    if self.close_after {
                        return Ok(Read::More);
                    }
                    if self.in_buf.len() < total {
                        self.in_buf.resize(total, 0);
                    }
                }
                Next::Chunked(head_len) => {
                    match decode_chunked(&mut self.in_buf[head_len..self.in_len]) {
                        Chunked::Bad => {
                            self.queue_error(400);
                            return Ok(Read::More);
                        }
                        Chunked::Incomplete => {
                            self.answer_expectation();
                            if self.close_after {
                                return Ok(Read::More);
                            }
                        }
                        Chunked::Done { decoded, raw } => {
                            let parsed = parse_head_uninit(&self.in_buf[..self.in_len], heads);
                            let HeadResult::Complete(head) = parsed else {
                                return Err(io::Error::new(
                                    io::ErrorKind::InvalidData,
                                    "a request head changed between two parses",
                                ));
                            };
                            let head = with_body(head, &self.in_buf[head_len..head_len + decoded]);
                            self.begin(
                                head.method.eq_ignore_ascii_case("HEAD"),
                                keep_alive_head(&head),
                                head_len + raw,
                            );
                            return Ok(Read::Request(head));
                        }
                    }
                }
                Next::Read => {}
            }

            // Room for more bytes: the request's own size when it is known,
            // otherwise double, up to the two caps.
            if self.in_len == self.in_buf.len() {
                let cap = MAX_HEADER_BYTES + MAX_BODY_BYTES;
                if self.in_buf.len() >= cap {
                    self.queue_error(413);
                    return Ok(Read::More);
                }
                let grown = (self.in_buf.len() * 2).min(cap);
                self.in_buf.resize(grown, 0);
            }

            match self.fill()? {
                Fill::Got => {}
                Fill::WouldBlock => return Ok(Read::More),
                Fill::Closed => {
                    if self.in_len == 0 {
                        return Ok(Read::Closed);
                    }
                    // A half-sent request cannot become complete.
                    self.queue_error(400);
                    return Ok(Read::More);
                }
            }
        }
    }

    /// Answer `Expect: 100-continue` once, before a read could wait for a body
    /// the client is holding back. An expectation this server cannot honour is
    /// refused and the connection closes.
    fn answer_expectation(&mut self) {
        if self.expect_answered {
            return;
        }
        match crate::h1::expectation(&self.in_buf[..self.in_len]) {
            Some(Expectation::Continue) => {
                self.expect_answered = true;
                self.queue_bytes(CONTINUE_RESPONSE);
            }
            Some(Expectation::Unsupported) => self.queue_error(417),
            Some(Expectation::None) => self.expect_answered = true,
            None => {}
        }
    }

    /// The slot's input and output buffer sizes.
    ///
    /// Testkit only: it proves a buffer grown for a request larger than the
    /// slot was cut back afterwards, which nothing on the wire can show.
    #[cfg(feature = "testkit")]
    pub fn buffer_caps(&self) -> (usize, usize) {
        (self.in_buf.len(), self.out_buf.len())
    }

    /// Queue a short response into the slot's output buffer.
    pub fn respond(&mut self, status: u16, headers: &[(&str, &str)], body: &[u8]) {
        let mut out = OutBuf::new(&mut self.out_buf, &mut self.out_len);
        let bodyless = write_head_to(
            &mut out,
            status,
            headers,
            body.len() as u64,
            self.is_head.get(),
            self.keep_alive.get(),
        )
        .expect("the slot's output buffer cannot fail");
        if !bodyless {
            out.write_all(body)
                .expect("the slot's output buffer cannot fail");
        }
    }

    /// Queue a [`Response`]. A `Body::Stream` writes its head here and its body
    /// is pumped by [`flush`](Self::flush), one chunk per writable turn.
    pub fn respond_response(&mut self, response: Response) {
        let add_content_type = !crate::headers::contains(&response.headers[..], "content-type")
            && !response.body.is_empty();
        if matches!(response.body, Body::Stream { .. }) {
            let Response {
                status,
                headers,
                body,
                ..
            } = response;
            let Body::Stream { len, reader } = body else {
                unreachable!("matched above");
            };
            let hdrs = crate::response::wire_headers(&headers, add_content_type);
            let bodyless = {
                let mut out = OutBuf::new(&mut self.out_buf, &mut self.out_len);
                write_head_to(
                    &mut out,
                    status,
                    &hdrs,
                    len,
                    self.is_head.get(),
                    self.keep_alive.get(),
                )
                .expect("the slot's output buffer cannot fail")
            };
            if !bodyless {
                self.body = Some(StreamBody {
                    remaining: len,
                    reader,
                });
            }
        } else {
            let mut out = OutBuf::new(&mut self.out_buf, &mut self.out_len);
            let mut responder = Responder::new(
                &mut out,
                if self.is_head.get() { "HEAD" } else { "" },
                self.keep_alive.get(),
            );
            response
                .send(&mut responder)
                .expect("the slot's output buffer cannot fail");
        }
    }

    /// Write what the socket will take. Never waits.
    pub fn flush(&mut self) -> io::Result<Flush> {
        let Some(stream) = self.stream.as_mut() else {
            return Ok(Flush::Close);
        };
        loop {
            while self.out_pos < self.out_len {
                match stream.write(&self.out_buf[self.out_pos..self.out_len]) {
                    Ok(0) => return Ok(Flush::Close),
                    Ok(n) => self.out_pos += n,
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        return Ok(Flush::More)
                    }
                    // A signal interrupted the write; the bytes were not sent.
                    Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => return Ok(Flush::Close),
                }
            }
            self.out_pos = 0;
            self.out_len = 0;

            let Some(body) = self.body.as_mut() else {
                break;
            };
            if body.remaining == 0 {
                self.body = None;
                break;
            }
            let want = body.remaining.min(STREAM_CHUNK) as usize;
            if self.out_buf.len() < want {
                self.out_buf.resize(want, 0);
            }
            let n = body.reader.read(&mut self.out_buf[..want])?;
            if n == 0 {
                // The Content-Length promise is already on the wire, so a
                // short reader is a framing fault: close rather than send a
                // body shorter than the one the peer was promised.
                return Err(io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "streamed body ended before its Content-Length",
                ));
            }
            body.remaining -= n as u64;
            self.out_len = n;
        }

        // A response larger than the slot's buffer grew it; cut it back now the
        // response is out, so memory does not creep.
        if self.out_buf.len() > self.out_write_cap {
            self.out_buf.truncate(self.out_write_cap);
            self.out_buf.shrink_to(self.out_write_cap);
        }

        Ok(if self.close_after || !self.keep_alive.get() {
            Flush::Close
        } else {
            Flush::Done
        })
    }

    /// Record the request being answered: its method, its persistence and how
    /// much of the input buffer it occupies.
    fn begin(&self, is_head: bool, keep_alive: bool, len: usize) {
        self.served.set(self.served.get() + 1);
        self.is_head.set(is_head);
        self.keep_alive
            .set(keep_alive && self.served.get() < MAX_REQUESTS_PER_CONN);
        self.consumed.set(len);
    }

    /// Drop the previous request from the input buffer when the next read
    /// starts, and cut a grown buffer back to its start size.
    fn drop_consumed(&mut self) {
        let consumed = self.consumed.get();
        if consumed == 0 {
            return;
        }
        self.in_buf.copy_within(consumed..self.in_len, 0);
        self.in_len -= consumed;
        self.consumed.set(0);
        self.expect_answered = false;
        if self.in_buf.len() > self.in_read_cap && self.in_len <= self.in_read_cap {
            self.in_buf.truncate(self.in_read_cap);
            self.in_buf.shrink_to(self.in_read_cap);
        }
    }

    /// Queue a refusal, and mark the connection to close after it is written.
    fn queue_error(&mut self, status: u16) {
        let mut out = OutBuf::new(&mut self.out_buf, &mut self.out_len);
        let mut responder = Responder::new(&mut out, "", false);
        responder
            .error(status)
            .expect("the slot's output buffer cannot fail");
        self.close_after = true;
        self.keep_alive.set(false);
    }

    fn queue_bytes(&mut self, bytes: &[u8]) {
        let mut out = OutBuf::new(&mut self.out_buf, &mut self.out_len);
        out.write_all(bytes)
            .expect("the slot's output buffer cannot fail");
    }

    /// One `read(2)` into the slot's input buffer.
    fn fill(&mut self) -> io::Result<Fill> {
        let Some(stream) = self.stream.as_mut() else {
            return Ok(Fill::Closed);
        };
        let start = self.in_len;
        match stream.read(&mut self.in_buf[start..]) {
            Ok(0) => Ok(Fill::Closed),
            Ok(n) => {
                self.in_len += n;
                Ok(Fill::Got)
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(Fill::WouldBlock),
            Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => Ok(Fill::Got),
            Err(e) => Err(e),
        }
    }

    /// Reset every per-connection field and cut grown buffers back.
    fn reset(&mut self) {
        self.in_len = 0;
        self.consumed.set(0);
        self.out_len = 0;
        self.out_pos = 0;
        self.body = None;
        self.is_head.set(false);
        self.keep_alive.set(true);
        self.served.set(0);
        self.close_after = false;
        self.expect_answered = false;
        if self.in_buf.len() > self.in_read_cap {
            self.in_buf.truncate(self.in_read_cap);
            self.in_buf.shrink_to(self.in_read_cap);
        }
        if self.out_buf.len() > self.out_write_cap {
            self.out_buf.truncate(self.out_write_cap);
            self.out_buf.shrink_to(self.out_write_cap);
        }
    }
}

/// What one parse of the buffered bytes says to do next.
enum Next {
    /// A complete request is buffered, with the framing `read` records before
    /// it parses once more to lend the head.
    Complete {
        len: usize,
        is_head: bool,
        keep_alive: bool,
    },
    /// More bytes are needed.
    Read,
    /// Refuse with this status and close.
    Error(u16),
    /// A `Content-Length` body this many bytes long is still incomplete.
    Need(usize),
    /// A chunked body starting after this head length is still incomplete.
    Chunked(usize),
}

/// Parse the buffered bytes for a framing decision.
///
/// Its own slots on the stack, so the caller's slots stay free for the parse
/// that lends the request head. `MaybeUninit`, so this writes nothing.
fn inspect(in_buf: &[u8]) -> Next {
    let mut slots = head_slots();
    match parse_head_uninit(in_buf, &mut slots) {
        HeadResult::Error => Next::Error(400),
        HeadResult::Incomplete => {
            // Until the blank line arrives, an unbounded head is how a peer
            // makes the process buffer forever.
            if in_buf.len() > MAX_HEADER_BYTES {
                Next::Error(413)
            } else {
                Next::Read
            }
        }
        HeadResult::Complete(head) => {
            if !head.has_body() {
                complete(&head, head.head_len)
            } else if head.body_len > MAX_BODY_BYTES {
                Next::Error(413)
            } else if head
                .headers
                .iter()
                .any(|h| h.name.eq_ignore_ascii_case("transfer-encoding"))
            {
                Next::Chunked(head.head_len)
            } else {
                let total = head.head_len + head.body_len;
                if in_buf.len() >= total {
                    complete(&head, total)
                } else {
                    Next::Need(total)
                }
            }
        }
    }
}

/// The framing decision for a complete request, as owned values.
fn complete(head: &RequestHead<'_>, len: usize) -> Next {
    Next::Complete {
        len,
        is_head: head.method.eq_ignore_ascii_case("HEAD"),
        keep_alive: keep_alive_head(head),
    }
}

/// A request head whose body slice has been filled in (the chunked path).
fn with_body<'a>(mut head: RequestHead<'a>, body: &'a [u8]) -> RequestHead<'a> {
    head.body = body;
    head
}

/// One `read(2)` outcome.
enum Fill {
    Got,
    WouldBlock,
    Closed,
}

/// A writer over the slot's output buffer with its valid length kept beside it,
/// so a streamed body can be read into the same memory without zeroing it.
struct OutBuf<'a> {
    buf: &'a mut Vec<u8>,
    len: &'a mut usize,
}

impl<'a> OutBuf<'a> {
    fn new(buf: &'a mut Vec<u8>, len: &'a mut usize) -> OutBuf<'a> {
        OutBuf { buf, len }
    }
}

impl std::io::Write for OutBuf<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let end = *self.len + bytes.len();
        if self.buf.len() < end {
            self.buf.resize(end, 0);
        }
        self.buf[*self.len..end].copy_from_slice(bytes);
        *self.len = end;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod absolute_form_tests {
    use super::*;

    fn parse(raw: &[u8]) -> RawRequest {
        match parse_request(raw) {
            ParseResult::Complete(r) => r,
            other => panic!(
                "expected Complete, got {:?}",
                std::mem::discriminant(&other)
            ),
        }
    }

    /// RFC 9112 3.2.2: a server MUST accept absolute-form, and the target it
    /// routes on is the path.
    #[test]
    fn absolute_form_is_reduced_to_its_path() {
        let r = parse(b"GET http://example.com/a/b?c=1 HTTP/1.1\r\nHost: example.com\r\n\r\n");
        assert_eq!(r.path, "/a/b");
        assert_eq!(r.query.as_deref(), Some("c=1"));

        let r = parse(b"GET https://example.com/x HTTP/1.1\r\nHost: example.com\r\n\r\n");
        assert_eq!(r.path, "/x");

        // No path component at all means the root.
        let r = parse(b"GET http://example.com HTTP/1.1\r\nHost: example.com\r\n\r\n");
        assert_eq!(r.path, "/");
    }

    /// Origin-form, asterisk-form and authority-form are untouched.
    #[test]
    fn other_target_forms_pass_through() {
        let r = parse(b"GET /a/b HTTP/1.1\r\nHost: x\r\n\r\n");
        assert_eq!(r.path, "/a/b");

        let r = parse(b"OPTIONS * HTTP/1.1\r\nHost: x\r\n\r\n");
        assert_eq!(r.path, "*", "asterisk-form is a valid target for OPTIONS");

        let r = parse(b"CONNECT example.com:443 HTTP/1.1\r\nHost: example.com\r\n\r\n");
        assert_eq!(r.path, "example.com:443", "authority-form is not a path");
    }

    /// A scheme-relative target must NOT be treated as absolute-form.
    ///
    /// `//evil.example.com/x` has no scheme. Rewriting it to `/x` would turn a request
    /// the router should refuse into one it serves, which is the same
    /// open-redirect shape `is_same_origin_path` exists to stop.
    #[test]
    fn scheme_relative_targets_are_not_rewritten() {
        let r = parse(b"GET //evil.example.com/x HTTP/1.1\r\nHost: x\r\n\r\n");
        assert_eq!(
            r.path, "//evil.example.com/x",
            "scheme-relative must survive intact"
        );
    }

    /// Only a real scheme counts. A path that merely contains "://" is a path.
    #[test]
    fn only_a_valid_scheme_triggers_the_rewrite() {
        for target in [
            "/redirect?to=http://evil.example.com/x",
            "/a://b",
            "/1http://evil.example.com/x",
        ] {
            let raw = format!("GET {target} HTTP/1.1\r\nHost: x\r\n\r\n");
            let r = parse(raw.as_bytes());
            let expected = target.split('?').next().unwrap();
            assert_eq!(r.path, expected, "target {target:?} must not be rewritten");
        }
    }
}

#[cfg(test)]
mod chunked_tests {
    use super::*;

    fn complete(raw: &[u8]) -> RawRequest {
        match parse_request(raw) {
            ParseResult::Complete(r) => r,
            ParseResult::Incomplete => panic!("expected Complete, got Incomplete"),
            ParseResult::Error => panic!("expected Complete, got Error"),
        }
    }
    fn is_error(raw: &[u8]) -> bool {
        matches!(parse_request(raw), ParseResult::Error)
    }
    fn is_incomplete(raw: &[u8]) -> bool {
        matches!(parse_request(raw), ParseResult::Incomplete)
    }

    const HEAD: &[u8] = b"POST /x HTTP/1.1\r\nHost: a\r\nTransfer-Encoding: chunked\r\n\r\n";

    fn with(body: &[u8]) -> Vec<u8> {
        let mut v = HEAD.to_vec();
        v.extend_from_slice(body);
        v
    }

    /// RFC 9112 7.1: a server MUST be able to receive and decode chunked.
    #[test]
    fn a_chunked_body_is_decoded() {
        let r = complete(&with(b"5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n"));
        assert_eq!(r.body, b"hello world");
    }

    /// The decoded body is handed on with a Content-Length, so the next hop
    /// never has to agree with us about chunk framing.
    #[test]
    fn the_decoded_body_gets_a_content_length_and_te_is_not_forwarded() {
        let r = complete(&with(b"5\r\nhello\r\n0\r\n\r\n"));
        let cl = r
            .headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("content-length"));
        assert_eq!(cl.map(|(_, v)| v.as_str()), Some("5"));
        assert!(
            !r.headers
                .iter()
                .any(|(k, _)| k.eq_ignore_ascii_case("transfer-encoding")),
            "transfer-coding is hop-by-hop and must not be forwarded"
        );
    }

    #[test]
    fn chunk_extensions_and_trailers_are_tolerated() {
        let r = complete(&with(b"5;name=value\r\nhello\r\n0\r\nX-Trailer: t\r\n\r\n"));
        assert_eq!(r.body, b"hello");
    }

    #[test]
    fn an_empty_chunked_body_is_valid() {
        assert_eq!(complete(&with(b"0\r\n\r\n")).body, b"");
    }

    /// A body that has not fully arrived must be Incomplete, never Complete
    /// with a truncated body.
    #[test]
    fn a_partial_chunked_body_is_incomplete() {
        assert!(is_incomplete(&with(b"5\r\nhel")));
        assert!(is_incomplete(&with(b"5\r\nhello\r\n"))); // no terminator yet
        assert!(is_incomplete(&with(b"5\r\nhello\r\n0\r\n"))); // trailers unterminated
    }

    /// A lenient chunk-size parser is how two hops end up disagreeing about
    /// where a request ends, which is request smuggling.
    #[test]
    fn malformed_chunk_sizes_are_refused() {
        assert!(is_error(&with(b"+5\r\nhello\r\n0\r\n\r\n")), "sign");
        assert!(is_error(&with(b"0x5\r\nhello\r\n0\r\n\r\n")), "0x prefix");
        assert!(
            is_error(&with(b" 5\r\nhello\r\n0\r\n\r\n")),
            "leading space"
        );
        assert!(is_error(&with(b"5_0\r\nhello\r\n0\r\n\r\n")), "underscore");
        assert!(is_error(&with(b"\r\nhello\r\n0\r\n\r\n")), "empty size");
        assert!(is_error(&with(b"ffffffffffffffffff\r\n")), "size overflow");
    }

    /// Chunk data must be followed by exactly CRLF.
    #[test]
    fn a_missing_chunk_terminator_is_refused() {
        assert!(is_error(&with(b"5\r\nhelloXX0\r\n\r\n")));
    }

    /// RFC 9112 6.1: chunked must be the final coding, and nothing else is
    /// implemented.
    #[test]
    fn other_transfer_codings_are_refused() {
        let mk = |te: &str| {
            format!("POST /x HTTP/1.1\r\nHost: a\r\nTransfer-Encoding: {te}\r\n\r\n0\r\n\r\n")
                .into_bytes()
        };
        assert!(is_error(&mk("chunked, gzip")), "chunked must be last");
        assert!(is_error(&mk("gzip, chunked")), "gzip is not implemented");
        assert!(is_error(&mk("gzip")), "unknown coding");
        assert!(is_error(&mk("bogus")), "unknown coding");
    }

    /// Transfer-Encoding with Content-Length is the smuggling primitive.
    #[test]
    fn transfer_encoding_with_content_length_is_refused() {
        let raw = b"POST /x HTTP/1.1\r\nHost: a\r\nContent-Length: 5\r\n\
                    Transfer-Encoding: chunked\r\n\r\n0\r\n\r\n";
        assert!(is_error(raw));
    }
}

#[cfg(test)]
mod host_tests {
    use super::*;

    fn is_error(raw: &[u8]) -> bool {
        matches!(parse_request(raw), ParseResult::Error)
    }
    fn is_ok(raw: &[u8]) -> bool {
        matches!(parse_request(raw), ParseResult::Complete(_))
    }

    fn get(host: &str) -> Vec<u8> {
        format!("GET / HTTP/1.1\r\nHost: {host}\r\n\r\n").into_bytes()
    }

    /// `Host` decides which site a request is for. A value that two hops would
    /// split differently is not one authority.
    #[test]
    fn a_host_with_whitespace_is_refused() {
        assert!(is_error(&get("bad host")));
        assert!(is_error(&get("bad\thost")));
    }

    #[test]
    fn hosts_outside_the_authority_alphabet_are_refused() {
        assert!(is_error(&get("ex<ample>.com")), "delimiters");
        assert!(
            is_error(&get("example.com/path")),
            "a path is not an authority"
        );
        assert!(is_error(&get("example.com:https")), "port must be digits");
        assert!(is_error(&get("example.com:99999999")), "port out of range");
        assert!(is_error(&get("[::1")), "unterminated literal");
        assert!(is_error(&get(&"a".repeat(254))), "over the length limit");
    }

    #[test]
    fn ordinary_hosts_are_accepted() {
        assert!(is_ok(&get("example.com")));
        assert!(is_ok(&get("example.com:8443")));
        assert!(is_ok(&get("localhost")));
        assert!(is_ok(&get("127.0.0.1:80")));
        assert!(is_ok(&get("[::1]")));
        assert!(is_ok(&get("[::1]:8443")));
        assert!(is_ok(&get("xn--n3h.example")), "punycode is ordinary");
        assert!(is_ok(&get("under_score.example")));
    }

    /// RFC 9112 6.1: chunked is an HTTP/1.1 transfer coding. Honouring it on a
    /// 1.0 message is the version-straddling disagreement smuggling exploits.
    #[test]
    fn chunked_on_http10_is_refused() {
        let raw = b"POST / HTTP/1.0\r\nHost: localhost\r\n\
                    Transfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n0\r\n\r\n";
        assert!(is_error(raw));
    }
}

#[cfg(test)]
mod stream_tests {
    use super::*;

    fn respond(
        method: &str,
        keep_alive: bool,
        len: u64,
        body: &[u8],
    ) -> (std::io::Result<()>, String) {
        let mut out: Vec<u8> = Vec::new();
        let r = {
            let mut resp = Responder::new(&mut out, method, keep_alive);
            resp.send_stream(200, &[("Content-Type", "text/plain")], len, body)
        };
        (r, String::from_utf8_lossy(&out).into_owned())
    }

    /// A streamed body is byte-identical to a buffered one, head included.
    /// If it were not, the two senders would be two framings.
    #[test]
    fn streaming_produces_the_same_bytes_as_sending_a_slice() {
        let body = b"hello from a reader";
        let (r, streamed) = respond("GET", true, body.len() as u64, &body[..]);
        r.expect("stream");

        let mut out: Vec<u8> = Vec::new();
        {
            let mut resp = Responder::new(&mut out, "GET", true);
            resp.send(200, &[("Content-Type", "text/plain")], body)
                .expect("send");
        }
        assert_eq!(streamed, String::from_utf8_lossy(&out));
    }

    /// The promise on the wire has to match the bytes after it.
    #[test]
    fn the_content_length_matches_what_is_written() {
        let body = vec![b'x'; 200_000];
        let (r, s) = respond("GET", true, body.len() as u64, &body[..]);
        r.expect("stream");
        assert!(
            s.contains("Content-Length: 200000"),
            "head was:\n{}",
            &s[..120.min(s.len())]
        );
        let split = s.find("\r\n\r\n").unwrap() + 4;
        assert_eq!(s.len() - split, 200_000);
    }

    /// A reader that ends early must fail, not quietly send a short body.
    ///
    /// `Content-Length` is already on the wire by then, so the peer is waiting
    /// for a fixed number of bytes. Sending fewer and carrying on would make
    /// whatever is written next read as the tail of this body, which is the
    /// smuggling primitive the parser rejects on ingress. The error closes the
    /// connection instead.
    #[test]
    fn a_short_reader_is_an_error_and_not_a_short_body() {
        let (r, _) = respond("GET", true, 100, b"only twelve\n");
        let e = r.expect_err("a body that ends early must not be sent as if complete");
        assert_eq!(e.kind(), std::io::ErrorKind::UnexpectedEof);
    }

    /// And a reader with more to give cannot overrun the promise either, which
    /// is the case of a file that grew between `stat` and `read`.
    #[test]
    fn a_long_reader_is_truncated_to_the_promised_length() {
        let (r, s) = respond("GET", true, 5, b"0123456789");
        r.expect("stream");
        let split = s.find("\r\n\r\n").unwrap() + 4;
        assert_eq!(&s[split..], "01234");
    }

    /// On HEAD the reader is never touched. That is the point of streaming a
    /// file: it is not opened, let alone read.
    #[test]
    fn head_reports_the_length_and_reads_nothing() {
        struct Exploding;
        impl std::io::Read for Exploding {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                panic!("HEAD must not read the body");
            }
        }
        let mut out: Vec<u8> = Vec::new();
        {
            let mut resp = Responder::new(&mut out, "HEAD", true);
            resp.send_stream(200, &[], 4096, Exploding).expect("head");
        }
        let s = String::from_utf8_lossy(&out);
        assert!(s.contains("Content-Length: 4096"), "{s}");
        assert!(s.ends_with("\r\n\r\n"), "a HEAD must carry no body");
    }
}

#[cfg(test)]
mod parse_head_tests {
    use super::*;

    fn is_error(raw: &[u8]) -> bool {
        let mut headers = [httparse::EMPTY_HEADER; 64];
        matches!(parse_head(raw, &mut headers), HeadResult::Error)
    }

    /// The borrowed parser rejects exactly what the owned parser rejects, so the
    /// two cannot drift apart on what reaches the ingress boundary.
    #[test]
    fn parse_head_refuses_what_parse_request_refuses() {
        assert!(is_error(b"GET / HTTP/1.1\nHost: x\n\n"), "bare LF");
        assert!(
            is_error(
                b"GET / HTTP/1.1\r\nHost: x\r\nContent-Length: 1\r\nContent-Length: 2\r\n\r\n"
            ),
            "two disagreeing Content-Length"
        );
        assert!(
            is_error(
                b"POST / HTTP/1.1\r\nHost: x\r\nContent-Length: 5\r\n\
                  Transfer-Encoding: chunked\r\n\r\n0\r\n\r\n"
            ),
            "Transfer-Encoding with Content-Length"
        );
        let mut raw = String::from("GET / HTTP/1.1\r\nHost: x\r\n");
        for i in 0..70 {
            raw.push_str(&format!("X-H{i}: v\r\n"));
        }
        raw.push_str("\r\n");
        assert!(is_error(raw.as_bytes()), "more than 64 headers");
    }

    /// The borrowed head points into the request buffer and reads nothing owned.
    #[test]
    fn parse_head_borrows_without_copying() {
        let raw = b"GET /a/b?c=1 HTTP/1.1\r\nHost: example.com\r\nAccept-Encoding: gzip\r\n\r\n";
        let mut headers = [httparse::EMPTY_HEADER; 64];
        let h = match parse_head(raw, &mut headers) {
            HeadResult::Complete(h) => h,
            HeadResult::Incomplete => panic!("expected Complete, got Incomplete"),
            HeadResult::Error => panic!("expected Complete, got Error"),
        };
        assert_eq!(h.method, "GET");
        assert_eq!(h.path, "/a/b");
        assert_eq!(h.query, Some("c=1"));
        assert_eq!(h.version, "HTTP/1.1");
        assert_eq!(h.body_len, 0);
        assert!(!h.has_body());
        assert_eq!(h.headers.len(), 2);
        assert_eq!(crate::http::header(h.headers, "host"), Some("example.com"));
    }
}
