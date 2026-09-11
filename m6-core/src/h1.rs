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
use std::io::Write;

/// Reduce an absolute-form request target to its path.
///
/// `http://host/a/b?c` becomes `/a/b?c`. Anything already in origin-form,
/// asterisk-form (`OPTIONS *`) or authority-form (`CONNECT host:port`) is
/// returned unchanged: only absolute-form carries a scheme.
///
/// A scheme-relative target (`//host/path`) is left alone. It is not
/// absolute-form, and treating it as one would let `//evil.com/x` be rewritten
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
                if !inside.bytes().all(|b| b.is_ascii_hexdigit() || b == b':' || b == b'.') {
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
                || matches!(b, b'-' | b'.' | b'_' | b'~' | b'%'
                             | b'!' | b'$' | b'&' | b'\'' | b'(' | b')'
                             | b'*' | b'+' | b',' | b';' | b'=')
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
        let Some(colon) = line.iter().position(|&b| b == b':') else { continue };
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
        if first.is_ascii_whitespace() { b = rest } else { break }
    }
    while let [rest @ .., last] = b {
        if last.is_ascii_whitespace() { b = rest } else { break }
    }
    b
}

/// Largest decoded chunked body accepted, matching the Content-Length cap.
const MAX_CHUNKED_BODY: usize = 16 * 1024 * 1024;

/// Outcome of decoding a chunked body.
enum Chunked {
    /// Fully decoded.
    Done(Vec<u8>),
    /// A complete body has not arrived yet; read more and retry.
    Incomplete,
    /// Malformed framing. Not recoverable: the connection cannot be trusted to
    /// resynchronise, because the next bytes might be a smuggled request.
    Bad,
}

/// Decode a chunked transfer-coded body (RFC 9112 7.1).
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
fn decode_chunked(buf: &[u8]) -> Chunked {
    let mut out: Vec<u8> = Vec::new();
    let mut i = 0usize;

    loop {
        // chunk-size [ ";" chunk-ext ] CRLF
        let Some(eol) = find_crlf(&buf[i..]) else { return Chunked::Incomplete };
        let line = &buf[i..i + eol];
        // Extensions are permitted and ignored; the size ends at ';'.
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
                // No sign, no whitespace, no underscores. A lenient size
                // parser is exactly how two hops end up disagreeing.
                _ => return Chunked::Bad,
            };
            size = match size.checked_mul(16).and_then(|v| v.checked_add(d as usize)) {
                Some(v) => v,
                None => return Chunked::Bad,
            };
        }
        i += eol + 2;

        if size == 0 {
            // Last chunk. Trailer fields may follow, ending with a blank line.
            loop {
                let Some(eol) = find_crlf(&buf[i..]) else { return Chunked::Incomplete };
                i += eol + 2;
                if eol == 0 {
                    return Chunked::Done(out);
                }
            }
        }

        if out.len() + size > MAX_CHUNKED_BODY {
            return Chunked::Bad;
        }
        if buf.len() < i + size + 2 {
            return Chunked::Incomplete;
        }
        out.extend_from_slice(&buf[i..i + size]);
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
        // `http://host` with no path at all means the origin's root.
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

pub fn parse_request(buf: &[u8]) -> ParseResult {
    let mut headers = [httparse::EMPTY_HEADER; 64];
    let mut req = httparse::Request::new(&mut headers);
    let body_offset = match req.parse(buf) {
        Ok(httparse::Status::Partial) => return ParseResult::Incomplete,
        Ok(httparse::Status::Complete(n)) => n,
        Err(_) => return ParseResult::Error,
    };

    // Reject a bare LF anywhere in the header block.
    //
    // httparse accepts LF alone as a line terminator, which is lenient in the
    // way most servers historically were. The consequence is that a header
    // VALUE containing a raw \n is silently split into two headers:
    //
    //     X-Test: a\nX-Injected: yes   ->   X-Test: a  +  X-Injected: yes
    //
    // and the second one is then forwarded to the backend. Found by a raw
    // socket test; no client library will send this, which is why it survived.
    //
    // Honest scope: this is not a demonstrated exploit against the current
    // topology. Ingress stripping runs after the split, so proxy-owned headers
    // are still removed, and m6-http re-serialises with CRLF, so its own
    // cache-node-to-origin hop cannot desync with itself. The hazard is the
    // classic one from RFC 9112 11.2 -- two hops disagreeing about where a
    // header ends -- and it goes live the moment anything is placed in front
    // of m6. Rejecting is what current hardened servers do, and it costs one
    // scan of a small buffer on a path that has already parsed it once.
    //
    // Scanning `buf[..body_offset]` rather than the whole buffer keeps a body
    // containing legitimate LF bytes out of it.
    {
        let head = &buf[..body_offset];
        let mut i = 0;
        while let Some(off) = head[i..].iter().position(|&c| c == b'\n') {
            let at = i + off;
            if at == 0 || head[at - 1] != b'\r' {
                return ParseResult::Error;
            }
            i = at + 1;
        }
    }

    let method = req.method.unwrap_or("GET").to_string();
    let raw_path = req.path.unwrap_or("/");

    // RFC 9112 3.2.2: a server MUST accept absolute-form
    // (`GET http://host/path HTTP/1.1`), which is what a request through a
    // proxy looks like and what an attacker sends to see whether the origin
    // and the proxy agree about the target.
    //
    // The scheme and authority are dropped and the path kept, so routing sees
    // the same target it would for origin-form. Previously the whole URI was
    // handed to the router, which matched nothing and answered 404 -- accepted
    // in name, unusable in fact.
    //
    // The authority is deliberately NOT used to override `Host`: doing so
    // lets a client name one origin in the URI and another in `Host`, which is
    // the routing-confusion primitive. `Host` stays authoritative; a
    // disagreement is the client's problem, not a licence to pick one.
    let target = strip_absolute_form(raw_path);

    let (path, query) = match target.find('?') {
        Some(q) => (target[..q].to_string(), Some(target[q + 1..].to_string())),
        None => (target.to_string(), None),
    };

    // Extract what we need from req.headers before dropping req.
    //
    // One pass does three jobs — framing validation, ingress stripping, and
    // materialisation — because this loop is the single hottest piece of
    // per-request work in the proxy. It used to be two passes plus a
    // `filter_map().collect()` whose size hint forced the Vec to grow.
    let nheaders = req.headers.len();

    // Request framing must be unambiguous. Two disagreeing `Content-Length`
    // values, or a `Content-Length` alongside a `Transfer-Encoding`, let two
    // hops disagree about where this request ends and the next begins — the
    // basis of request smuggling. We refuse such a request rather than pick a
    // winner and hope the backend picks the same one.
    let mut first_cl: Option<&str> = None;
    let mut saw_cl = false;
    let mut host_count = 0usize;
    let mut host_empty = false;
    let mut saw_te = false;
    let mut fwd_headers: Vec<(String, String)> = Vec::with_capacity(nheaders);

    for h in &req.headers[..nheaders] {
        if h.name.eq_ignore_ascii_case("transfer-encoding") {
            let Ok(v) = std::str::from_utf8(h.value) else { return ParseResult::Error };
            // RFC 9112 6.1: `chunked` must be the FINAL coding, and any other
            // coding is one this server does not implement. `chunked, gzip`
            // is malformed; `gzip, chunked` names a coding we cannot decode.
            let last = v.rsplit(',').next().unwrap_or("").trim();
            if !last.eq_ignore_ascii_case("chunked") {
                return ParseResult::Error;
            }
            if v.split(',').count() > 1 {
                return ParseResult::Error;
            }
            saw_te = true;
            // Not forwarded: transfer-coding is hop-by-hop, and the body is
            // decoded here so the next hop gets a plain Content-Length.
            continue;
        }
        if h.name.eq_ignore_ascii_case("content-length") {
            let Ok(value) = std::str::from_utf8(h.value) else {
                return ParseResult::Error;
            };
            let value = value.trim();
            match first_cl {
                // Duplicates are tolerable only when they agree.
                Some(prev) if prev != value => return ParseResult::Error,
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
                        return ParseResult::Error;
                    }
                }
                _ => host_empty = true,
            }
        }

        // A header value that is not UTF-8 is dropped rather than rejected
        // (matching prior behaviour); `content-length` is the exception
        // handled above, where it is a framing error.
        let Ok(v) = std::str::from_utf8(h.value) else { continue };
        fwd_headers.push((h.name.to_string(), v.to_string()));
    }

    // Host validation, RFC 9112 3.2: a server MUST answer 400 to an HTTP/1.1
    // request that lacks a Host, carries more than one, or carries an invalid
    // value. All three returned 200 before this.
    //
    // More than one is the one with teeth: two hops can pick different Host
    // values and disagree about which site — or which origin — a request is
    // for. Rejected regardless of version for that reason.
    //
    // The version gate matters. HTTP/1.0 predates Host and is allowed to omit
    // it, so rejecting a 1.0 request for that would break clients that are
    // behaving correctly. httparse reports `version` as the minor number, so
    // 1 means HTTP/1.1.
    //
    // Note the H2C and HTTP/2 paths do not come through here (they build their
    // request in http2.rs, where `:authority` is already translated to Host),
    // and the synthetic refresh request is constructed directly rather than
    // parsed — so neither is affected by this.
    if host_count > 1 {
        return ParseResult::Error;
    }
    let is_http11 = req.version == Some(1);
    if is_http11 && (host_count == 0 || host_empty) {
        return ParseResult::Error;
    }

    // RFC 9112 6.1: chunked is an HTTP/1.1 transfer coding. An HTTP/1.0
    // message claiming it is malformed, and treating it as chunked anyway is
    // exactly the version-straddling disagreement smuggling exploits: a hop
    // that honours the header and one that falls back to read-to-close frame
    // the same bytes differently.
    if saw_te && !is_http11 {
        return ParseResult::Error;
    }

    // Transfer-Encoding and Content-Length together is the smuggling
    // primitive: two hops can pick different framings and disagree about
    // where the request ends. RFC 9112 6.1 says reject.
    if saw_te && saw_cl {
        return ParseResult::Error;
    }

    let content_length: usize = match first_cl {
        Some(v) => match v.parse() {
            Ok(n) => n,
            Err(_) => return ParseResult::Error,
        },
        // `saw_cl` without a value is unreachable (the loop sets both
        // together), but keep the framing decision explicit rather than
        // silently defaulting a malformed header to zero.
        None if saw_cl => return ParseResult::Error,
        None => 0,
    };

    drop(req); // release borrow of `headers`

    if saw_te {
        // Decoded here, so the body handed on is a plain one of known length
        // and no downstream hop has to agree with us about chunk framing.
        return match decode_chunked(&buf[body_offset..]) {
            Chunked::Incomplete => ParseResult::Incomplete,
            Chunked::Bad => ParseResult::Error,
            Chunked::Done(body) => {
                fwd_headers.push(("Content-Length".to_string(), body.len().to_string()));
                ParseResult::Complete(RawRequest {
                    method,
                    path,
                    query,
                    version: if is_http11 { "HTTP/1.1".to_string() } else { "HTTP/1.0".to_string() },
                    headers: fwd_headers,
                    body,
                })
            }
        };
    }

    let available = buf.len() - body_offset;
    if available < content_length {
        return ParseResult::Incomplete;
    }

    let body = buf[body_offset..body_offset + content_length].to_vec();

    ParseResult::Complete(RawRequest {
        method,
        path,
        query,
        // The version the client actually sent, not a constant.
        //
        // This was hardcoded to "HTTP/1.1" for every request, so the field was
        // a lie for anything that read it. It went unnoticed while nothing
        // did: the code above uses httparse's own `req.version` and never this
        // string. Persistent connections need it, because HTTP/1.1 defaults to
        // keeping the connection and HTTP/1.0 defaults to closing it -- and
        // with the constant in place an HTTP/1.0 client was told keep-alive.
        version: if is_http11 { "HTTP/1.1".to_string() } else { "HTTP/1.0".to_string() },
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
    // EVERY `Connection` field line, not just the first. RFC 9110 5.3:
    // repeated field lines are equivalent to one comma-joined value, so a
    // request carrying `Connection: keep-alive` and `Connection: close` means
    // `keep-alive, close` and must close.
    //
    // Taking only the first got this backwards and kept the connection open,
    // which `test_hop_by_hop_stripped` caught immediately: it sends both, and
    // its client then waited for a close that never came.
    let has = |tok: &str| {
        req.headers
            .iter()
            .filter(|(k, _)| k.eq_ignore_ascii_case("connection"))
            .flat_map(|(_, v)| v.split(','))
            .any(|t| t.trim().eq_ignore_ascii_case(tok))
    };

    if req.version.eq_ignore_ascii_case("HTTP/1.1") {
        !has("close")
    } else if req.version.eq_ignore_ascii_case("HTTP/1.0") {
        has("keep-alive")
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
    method: &'a str,
    /// Decided by the connection from the request, before the handler runs.
    keep_alive: bool,
    /// Body bytes actually written, for the caller's access log.
    written: usize,
}

impl<'a, W: std::io::Write> Responder<'a, W> {
    /// Answer a request whose framing the connection has already decided.
    pub fn new(w: &'a mut W, method: &'a str, keep_alive: bool) -> Self {
        Responder { w, method, keep_alive, written: 0 }
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
        // One buffered writer, so a response is one write syscall rather than
        // one per header.
        let mut w = std::io::BufWriter::with_capacity(1024, &mut *self.w);
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
            if self.keep_alive { "keep-alive" } else { "close" }
        )?;

        // RFC 9110 9.3.2. A 304 and a 204 have no body either (RFC 9110 15.4.5,
        // 15.3.5), and one sent on those is unframed bytes the peer will read
        // as the start of the next response.
        let bodyless = self.method.eq_ignore_ascii_case("HEAD")
            || status == 204
            || status == 304
            || (100..200).contains(&status);
        if !bodyless {
            w.write_all(body)?;
        }
        w.flush()?;
        drop(w);

        if !bodyless {
            self.written += body.len();
        }
        Ok(())
    }
}

#[cfg(test)]
mod absolute_form_tests {
    use super::*;

    fn parse(raw: &[u8]) -> RawRequest {
        match parse_request(raw) {
            ParseResult::Complete(r) => r,
            other => panic!("expected Complete, got {:?}", std::mem::discriminant(&other)),
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
    /// `//evil.com/x` has no scheme. Rewriting it to `/x` would turn a request
    /// the router should refuse into one it serves, which is the same
    /// open-redirect shape `is_same_origin_path` exists to stop.
    #[test]
    fn scheme_relative_targets_are_not_rewritten() {
        let r = parse(b"GET //evil.com/x HTTP/1.1\r\nHost: x\r\n\r\n");
        assert_eq!(r.path, "//evil.com/x", "scheme-relative must survive intact");
    }

    /// Only a real scheme counts. A path that merely contains "://" is a path.
    #[test]
    fn only_a_valid_scheme_triggers_the_rewrite() {
        for target in [
            "/redirect?to=http://evil.com/x",
            "/a://b",
            "/1http://evil.com/x",
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
    fn is_error(raw: &[u8]) -> bool { matches!(parse_request(raw), ParseResult::Error) }
    fn is_incomplete(raw: &[u8]) -> bool { matches!(parse_request(raw), ParseResult::Incomplete) }

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
        let cl = r.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case("content-length"));
        assert_eq!(cl.map(|(_, v)| v.as_str()), Some("5"));
        assert!(
            !r.headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("transfer-encoding")),
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
        assert!(is_incomplete(&with(b"5\r\nhello\r\n")));      // no terminator yet
        assert!(is_incomplete(&with(b"5\r\nhello\r\n0\r\n"))); // trailers unterminated
    }

    /// A lenient chunk-size parser is how two hops end up disagreeing about
    /// where a request ends, which is request smuggling.
    #[test]
    fn malformed_chunk_sizes_are_refused() {
        assert!(is_error(&with(b"+5\r\nhello\r\n0\r\n\r\n")), "sign");
        assert!(is_error(&with(b"0x5\r\nhello\r\n0\r\n\r\n")), "0x prefix");
        assert!(is_error(&with(b" 5\r\nhello\r\n0\r\n\r\n")), "leading space");
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

    fn is_error(raw: &[u8]) -> bool { matches!(parse_request(raw), ParseResult::Error) }
    fn is_ok(raw: &[u8]) -> bool { matches!(parse_request(raw), ParseResult::Complete(_)) }

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
        assert!(is_error(&get("example.com/path")), "a path is not an authority");
        assert!(is_error(&get("example.com:https")), "port must be digits");
        assert!(is_error(&get("example.com:99999999")), "port out of range");
        assert!(is_error(&get("[::1")), "unterminated literal");
        assert!(is_error(&get(&"a".repeat(254))), "over the length limit");
    }

    #[test]
    fn ordinary_hosts_are_accepted() {
        assert!(is_ok(&get("mgrosvenor.com")));
        assert!(is_ok(&get("mgrosvenor.com:8443")));
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
