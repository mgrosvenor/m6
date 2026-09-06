/// HTTP/1.1 request forwarding over Unix sockets (synchronous blocking I/O).
use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;

/// Hop-by-hop headers to remove before forwarding.
pub const HOP_BY_HOP: &[&str] = &[
    "connection",
    "upgrade",
    "keep-alive",
    "transfer-encoding",
    "te",
    "trailer",
    "proxy-authorization",
    "proxy-connection",
];

/// Headers the proxy generates itself and therefore must never accept from a
/// client. Every one of these is a statement *about* the request that only the
/// edge is in a position to make truthfully.
///
/// Without this, a client could simply send its own copy: backends resolve
/// headers by first match (m6-render's `Request::header`, m6-core's
/// `RawRequest::header`), and the proxy appends its values *after* the
/// client's, so the forged copy is the one that wins. That made
/// `x-auth-claims` an authentication bypass (any client could assert
/// `{"groups":["admins"]}`) and `x-forwarded-for` a rate-limit bypass (rotate
/// the value, never get throttled).
///
/// Stripped on ingress, before routing, on every protocol — so no downstream
/// code has to remember to distrust them.
/// Ceiling on a backend response body.
///
/// `vec![0u8; len]` below allocates the backend's declared `Content-Length`
/// up front, before a single byte is read -- so a backend (or anything able to
/// impersonate one) declaring `Content-Length: 4000000000` allocated 4 GB
/// immediately. The bodyless `read_to_end` path was unbounded in the same way,
/// just more slowly.
///
/// 128 MiB is far above anything this serves -- the largest real asset is a
/// few MB of PDF -- so it never trips in normal operation, while bounding what
/// a misbehaving or hostile backend can cost.
const MAX_BACKEND_BODY: usize = 128 * 1024 * 1024;

pub const UNTRUSTED_INBOUND: &[&str] = &[
    "x-auth-claims",
    "x-forwarded-for",
    "x-forwarded-proto",
    "x-forwarded-host",
    "x-real-ip",
];

/// True if `name` is a header a client is never allowed to supply.
#[inline]
pub fn is_untrusted_inbound(name: &str) -> bool {
    UNTRUSTED_INBOUND.iter().any(|&h| name.eq_ignore_ascii_case(h))
}

/// Drop every client-supplied copy of a proxy-owned header.
///
/// Call on ingress for each protocol, immediately after parsing and before
/// anything reads the header set.
pub fn strip_untrusted_inbound(headers: &mut Vec<(String, String)>) {
    headers.retain(|(name, _)| !is_untrusted_inbound(name));
}

/// True if the proxy — not the client — owns this header on a forwarded
/// request, and therefore emits its own copy below.
///
/// Deliberately **not** the same set as [`UNTRUSTED_INBOUND`]. That set is
/// dropped at ingress; by the time a request reaches here, an `x-auth-claims`
/// header can only have been added by m6-http itself after verifying the JWT,
/// and dropping it here would mean renderers never receive the verified
/// identity at all.
///
/// `content-length` must describe the body *we* are about to write, and
/// `x-forwarded-*` are re-emitted from the real connection below, so any
/// surviving copy of either is discarded.
#[inline]
pub fn is_proxy_emitted_hop_header(name: &str) -> bool {
    proxy_owned_request_header(name)
}

#[inline]
fn proxy_owned_request_header(name: &str) -> bool {
    name.eq_ignore_ascii_case("content-length")
        || name.eq_ignore_ascii_case("x-forwarded-for")
        || name.eq_ignore_ascii_case("x-forwarded-proto")
        || name.eq_ignore_ascii_case("x-forwarded-host")
        || name.eq_ignore_ascii_case("x-real-ip")
}

/// Headers that must not be copied verbatim onto a forwarded request.
#[inline]
fn skip_when_forwarding(name: &str) -> bool {
    HOP_BY_HOP.iter().any(|&h| name.eq_ignore_ascii_case(h))
        || proxy_owned_request_header(name)
}

/// A parsed HTTP request (simplified for forwarding).
#[derive(Debug, Clone)]
pub struct HttpRequest {
    pub method: String,
    pub path: String,
    pub query: Option<String>,
    pub version: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

/// A parsed HTTP response.
#[derive(Debug, Clone)]
pub struct HttpResponse {
    pub status: u16,
    pub reason: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl HttpResponse {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// Can this field value be written into HTTP/1.1 syntax without changing the
/// shape of the message?
///
/// **F036/F094.** HTTP/1.1 delimits headers with CRLF, so a field value that
/// contains CR or LF stops being a value and becomes framing. HTTP/2 and
/// HTTP/3 do not: their header fields are length-delimited, so a decoded HPACK
/// or QPACK value can legitimately carry any byte, CR and LF included. Writing
/// one of those straight into an HTTP/1.1 request — which this proxy did —
/// lets a client smuggle arbitrary extra headers, or an entire second request,
/// into a backend that has no reason to distrust us.
///
/// This is the *egress* half of the bare-LF class fixed earlier on ingress, and
/// it is the more serious half: on ingress a malformed request is the client's
/// own problem, while here the proxy is the one producing the malformed bytes,
/// and it does so with the backend's trust behind it.
///
/// NUL is refused with them: it terminates strings in a good deal of software
/// a request may pass through, and no valid field value contains one.
///
/// RFC 9110 5.5 permits everything else, obs-text (0x80-0xFF) included, so
/// nothing narrower is imposed here. This is a framing check, not a filter.
pub fn h1_field_value_is_safe(v: &str) -> bool {
    !v.bytes().any(|b| b == b'\r' || b == b'\n' || b == 0)
}

/// Field names are tokens (RFC 9110 5.6.2). Anything outside that set could
/// introduce a colon, a space or a line break and split one header into two.
pub fn h1_field_name_is_safe(name: &str) -> bool {
    !name.is_empty()
        && name.bytes().all(|b| {
            b.is_ascii_alphanumeric()
                || matches!(
                    b,
                    b'!' | b'#' | b'$' | b'%' | b'&' | b'\'' | b'*'
                        | b'+' | b'-' | b'.' | b'^' | b'_' | b'`' | b'|' | b'~'
                )
        })
}

/// Everything this proxy is about to write into a request line or header block.
///
/// Checked in one place, and checked for *every* ingress protocol rather than
/// only the ones believed to be risky: the request line is assembled from a
/// method, path and query that arrive by four different routes, and `client_ip`
/// and `original_host` are derived from client-supplied data too. A check that
/// covers only the header loop leaves the request line open.
///
/// Returns the offending component's name so a refusal can be logged with
/// something actionable, rather than a generic "bad request".
pub fn check_forwardable(
    req: &HttpRequest,
    client_ip: &str,
    original_host: &str,
) -> Result<(), String> {
    // The method is written before the first space, so a space in it forges a
    // request line on its own — no CR needed.
    if req.method.is_empty() || !req.method.bytes().all(|b| b.is_ascii_graphic() && b != b'/') {
        return Err(format!("method {:?}", req.method));
    }
    for (label, s) in [
        ("path", req.path.as_str()),
        ("query", req.query.as_deref().unwrap_or("")),
    ] {
        // A space here ends the request target and makes the remainder look
        // like the HTTP version token.
        if s.bytes().any(|b| b == b'\r' || b == b'\n' || b == 0 || b == b' ') {
            return Err(format!("request {label}"));
        }
    }
    for (label, s) in [("X-Forwarded-For", client_ip), ("X-Forwarded-Host", original_host)] {
        if !h1_field_value_is_safe(s) {
            return Err(format!("proxy header {label}"));
        }
    }
    for (name, value) in &req.headers {
        if skip_when_forwarding(name) {
            continue; // never reaches the backend, so it cannot inject
        }
        if !h1_field_name_is_safe(name) {
            return Err(format!("header name {name:?}"));
        }
        if !h1_field_value_is_safe(value) {
            return Err(format!("header {name} value"));
        }
    }
    Ok(())
}

/// Write a usize as decimal into a stack buffer; return the filled slice.
#[inline(always)]
fn write_decimal(mut n: usize, buf: &mut [u8; 20]) -> &[u8] {
    let mut end = buf.len();
    if n == 0 {
        buf[end - 1] = b'0';
        return &buf[end - 1..];
    }
    while n > 0 {
        end -= 1;
        buf[end] = b'0' + (n % 10) as u8;
        n /= 10;
    }
    &buf[end..]
}

/// Forward an HTTP request over a Unix socket (blocking).
/// Adds proxy headers, removes hop-by-hop headers.
/// `timeout` sets both the read and write timeout; `None` means no timeout.
pub fn forward_request(
    socket_path: &Path,
    req: &HttpRequest,
    client_ip: &str,
    original_host: &str,
) -> io::Result<HttpResponse> {
    forward_request_timeout(socket_path, req, client_ip, original_host, None)
}

/// Like `forward_request` but with a configurable timeout for the total
/// backend call (connect + write + read). A `TimedOut` error is returned if
/// the backend does not respond within the deadline.
pub fn forward_request_timeout(
    socket_path: &Path,
    req: &HttpRequest,
    client_ip: &str,
    original_host: &str,
    timeout: Option<std::time::Duration>,
) -> io::Result<HttpResponse> {
    // Before the connection, not after: a request that cannot be safely
    // serialised must never reach a backend socket at all.
    if let Err(what) = check_forwardable(req, client_ip, original_host) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("refusing to forward: unsafe {what} would inject HTTP/1.1 framing"),
        ));
    }

    let mut stream = UnixStream::connect(socket_path)?;

    if let Some(dur) = timeout {
        stream.set_read_timeout(Some(dur))?;
        stream.set_write_timeout(Some(dur))?;
    }

    // Build request bytes
    let mut buf = Vec::with_capacity(8192);

    // Write request line
    buf.extend_from_slice(req.method.as_bytes());
    buf.push(b' ');
    buf.extend_from_slice(req.path.as_bytes());
    if let Some(ref q) = req.query {
        if !q.is_empty() {
            buf.push(b'?');
            buf.extend_from_slice(q.as_bytes());
        }
    }
    buf.extend_from_slice(b" HTTP/1.1\r\n");

    // Forward headers, excluding hop-by-hop and anything the proxy owns.
    for (name, value) in &req.headers {
        if skip_when_forwarding(name) {
            continue;
        }
        buf.extend_from_slice(name.as_bytes());
        buf.extend_from_slice(b": ");
        buf.extend_from_slice(value.as_bytes());
        buf.extend_from_slice(b"\r\n");
    }

    // Add proxy headers
    buf.extend_from_slice(b"X-Forwarded-For: ");
    buf.extend_from_slice(client_ip.as_bytes());
    buf.extend_from_slice(b"\r\nX-Forwarded-Proto: https\r\nX-Forwarded-Host: ");
    buf.extend_from_slice(original_host.as_bytes());
    buf.extend_from_slice(b"\r\n");

    // Content-Length for body — write decimal without allocating.
    if !req.body.is_empty() {
        buf.extend_from_slice(b"Content-Length: ");
        let mut cl_buf = [0u8; 20];
        buf.extend_from_slice(write_decimal(req.body.len(), &mut cl_buf));
        buf.extend_from_slice(b"\r\n");
    }

    // Connection: close for HTTP/1.1 since we're doing per-request connections
    buf.extend_from_slice(b"Connection: close\r\n\r\n");

    // Write body
    if !req.body.is_empty() {
        buf.extend_from_slice(&req.body);
    }

    stream.write_all(&buf)?;
    stream.flush()?;

    // Read response. The method matters: a HEAD response is bodyless whatever
    // its Content-Length says, and without this the read blocks until timeout.
    read_response_for(stream, &req.method)
}

/// Read an HTTP/1.1 response from a synchronous stream.
///
/// Kept for callers that genuinely cannot know the request method. Prefer
/// [`read_response_for`]: without the method this cannot apply RFC 9112 6.3's
/// first rule, and will wait for a body on a HEAD response that will never
/// have one.
pub fn read_response<R: Read>(reader: R) -> io::Result<HttpResponse> {
    read_response_for(reader, "GET")
}

/// Read an HTTP/1.1 response, framing it according to RFC 9112 6.3.
///
/// Message framing is the security boundary between this proxy and its
/// backend: if the two disagree about where a response ends, the leftover
/// bytes become the start of the *next* response on a reused connection. That
/// is response splitting, and it is why each of the rules below is a hard
/// error rather than a best guess.
///
/// - **Responses to HEAD, and 1xx/204/304, never have a body** (F035). The
///   reader previously did not know the method and would block waiting for
///   `Content-Length` bytes that a correct backend will never send -- so a
///   HEAD to a backend stalled until the read timeout.
/// - **`Transfer-Encoding` is a comma-separated list** (F030). Only a value
///   that was literally `chunked` was recognised, so a perfectly valid
///   `gzip, chunked` was treated as unframed and the chunk envelope was handed
///   back as if it were the body.
/// - **`Transfer-Encoding` together with `Content-Length` is refused** (F029).
///   RFC 9112 6.1 forbids sending both; a recipient that guesses which one to
///   believe is the classic smuggling primitive, because the next hop may
///   guess differently.
/// - **Conflicting `Content-Length` values are refused** (F028). The parser
///   kept the last one seen, so a backend emitting two different lengths
///   silently framed the response by whichever came last.
/// - **An unparseable `Content-Length` is refused** (F031). It used to become
///   `None` via `.ok()` and fall through to read-to-EOF -- turning a malformed
///   header into a silently different framing mode.
pub fn read_response_for<R: Read>(mut reader: R, request_method: &str) -> io::Result<HttpResponse> {
    // 8 KiB on the stack — sufficient for all normal responses, no heap alloc.
    let mut header_buf = [0u8; 8192];
    let mut n_total = 0usize;
    let header_end;

    loop {
        let n = reader.read(&mut header_buf[n_total..])?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "connection closed before headers complete",
            ));
        }
        n_total += n;
        if let Some(pos) = find_header_end(&header_buf[..n_total]) {
            header_end = pos;
            break;
        }
        if n_total >= header_buf.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "response headers too large",
            ));
        }
    }

    let header_section = std::str::from_utf8(&header_buf[..header_end])
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "response headers not UTF-8"))?;

    let mut lines = header_section.split("\r\n");

    // Status line
    let status_line = lines.next().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, "empty response")
    })?;
    let (status, reason) = parse_status_line(status_line)?;

    let mut headers: Vec<(String, String)> = Vec::with_capacity(16);
    let mut content_length: Option<usize> = None;
    let mut te_present = false;
    let mut chunked = false;

    for line in lines {
        if line.is_empty() {
            break;
        }
        if let Some(colon) = line.find(':') {
            let name = line[..colon].trim();
            let value = line[colon + 1..].trim();
            if name.eq_ignore_ascii_case("content-length") {
                // A single field may itself carry a comma-separated list, and
                // the field may repeat. Every value present must agree; if any
                // differ the framing is ambiguous and the message is refused
                // rather than resolved by position (F028).
                for part in value.split(',') {
                    let part = part.trim();
                    // `1*DIGIT` (RFC 9110 8.6), checked explicitly rather than
                    // left to `str::parse`, which accepts a leading `+` -- so
                    // `Content-Length: +5` was quietly read as 5. A hop that
                    // rejects it while this one accepts it is a framing
                    // disagreement, which is the whole hazard here.
                    if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!("backend sent an invalid Content-Length: {part:?}"),
                        ));
                    }
                    let parsed: usize = part.parse().map_err(|_| {
                        io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!("backend sent an out-of-range Content-Length: {part:?}"),
                        )
                    })?;
                    match content_length {
                        Some(prev) if prev != parsed => {
                            return Err(io::Error::new(
                                io::ErrorKind::InvalidData,
                                format!(
                                    "backend sent conflicting Content-Length values \
                                     ({prev} and {parsed}); refusing to guess the framing"
                                ),
                            ));
                        }
                        _ => content_length = Some(parsed),
                    }
                }
            }
            if name.eq_ignore_ascii_case("transfer-encoding") {
                te_present = true;
                // A list; only the FINAL coding decides the framing
                // (RFC 9112 6.1). `gzip, chunked` is chunked.
                if let Some(last) = value.split(',').next_back() {
                    if last.trim().eq_ignore_ascii_case("chunked") {
                        chunked = true;
                    }
                }
            }
            headers.push((name.to_owned(), value.to_owned()));
        }
    }

    // Both present: RFC 9112 6.1 forbids sending them together, and a
    // recipient that picks one is the classic smuggling primitive -- the next
    // hop may pick the other (F029).
    if te_present && content_length.is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "backend sent both Transfer-Encoding and Content-Length; refusing to guess the framing",
        ));
    }

    // Transfer-Encoding present but not ending in `chunked`: the message has no
    // self-delimiting framing at all. RFC 9112 6.1 says a server MUST NOT do
    // this; treating it as read-to-EOF would leave the connection unusable.
    if te_present && !chunked {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "backend sent a Transfer-Encoding not ending in chunked; response is unframed",
        ));
    }

    // RFC 9112 6.3 rule 1: these responses never have a body, whatever their
    // headers claim. Checked before any body read, so a HEAD does not block
    // waiting for bytes a correct backend will never send (F035).
    let bodyless = request_method.eq_ignore_ascii_case("HEAD")
        || status == 204
        || status == 304
        || (100..200).contains(&status);
    if bodyless {
        return Ok(HttpResponse { status, reason, headers, body: Vec::new() });
    }

    // Body bytes that arrived in the same read as the headers — borrow from stack buffer.
    let body_prefix = &header_buf[header_end + 4..n_total];

    let body = if chunked {
        read_chunked_body(&mut reader, body_prefix.to_vec())?
    } else if let Some(len) = content_length {
        // Refuse before allocating, not after: the whole point is that the
        // allocation is the damage.
        if len > MAX_BACKEND_BODY {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("backend declared Content-Length {len}, above the {MAX_BACKEND_BODY} limit"),
            ));
        }
        let mut body = vec![0u8; len];
        let already = body_prefix.len().min(len);
        body[..already].copy_from_slice(&body_prefix[..already]);
        if already < len {
            reader.read_exact(&mut body[already..])?;
        }
        body
    } else {
        // No declared length: read to EOF, but bounded. `take` caps it without
        // needing to know the size in advance.
        let mut body = body_prefix.to_vec();
        let remaining = MAX_BACKEND_BODY.saturating_sub(body.len());
        let read = std::io::Read::take(&mut reader, remaining as u64 + 1)
            .read_to_end(&mut body)?;
        let _ = read;
        if body.len() > MAX_BACKEND_BODY {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("backend response body exceeded the {MAX_BACKEND_BODY} limit"),
            ));
        }
        body
    };

    Ok(HttpResponse { status, reason, body, headers })
}

/// Locate the end of the HTTP header section (\r\n\r\n).
/// Returns the byte offset of the first `\r` in the terminating `\r\n\r\n`.
fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

/// Parse the status line `HTTP/x.y NNN Reason` into (status_code, reason).
fn parse_status_line(line: &str) -> io::Result<(u16, String)> {
    let mut parts = line.splitn(3, ' ');
    // Skip the version token
    parts.next();
    let status: u16 = parts
        .next()
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "bad or missing status code"))?;
    let reason = parts.next().unwrap_or("").to_owned();
    Ok((status, reason))
}

/// Decode a chunked body (RFC 9112 7.1).
///
/// Every relaxation below was a way for this decoder and the next hop to
/// disagree about where the body ends, which on a reused connection means the
/// remainder is read as the start of the following response.
///
/// - **The CRLF after each chunk's data is now required** (F032). It used to be
///   skipped only `if` it happened to be there, so a chunk whose data was
///   followed by anything else silently resynchronised onto the wrong offset
///   and the rest of the body was parsed as chunk headers.
/// - **The trailer section is parsed rather than ignored** (F033/F034). The
///   decoder stopped at the zero-size chunk and returned, never confirming the
///   message actually terminated. A truncated trailer section now errors
///   instead of passing as a complete body.
/// - **Chunk sizes must be pure hex digits.** `usize::from_str_radix` accepts a
///   leading `+`, so `+A` parsed as 10 -- the same trap that let
///   `Content-Length: +5` through.
/// - **The read is bounded.** `read_to_end` had no limit here, so while the
///   `Content-Length` path refused to allocate above `MAX_BACKEND_BODY`, a
///   chunked response could allocate without bound. That is the same memory
///   exhaustion the length cap exists to prevent, reachable by simply choosing
///   chunked framing.
fn read_chunked_body<R: Read>(reader: &mut R, prefix: Vec<u8>) -> io::Result<Vec<u8>> {
    let mut body = Vec::new();

    // Bounded: `take` caps the read without needing the size in advance. The
    // +1 lets an over-limit body be detected rather than silently truncated to
    // exactly the cap.
    let mut pending: Vec<u8> = prefix;
    let budget = MAX_BACKEND_BODY.saturating_sub(pending.len());
    let mut rest = Vec::new();
    Read::take(reader, budget as u64 + 1).read_to_end(&mut rest)?;
    pending.extend_from_slice(&rest);
    if pending.len() > MAX_BACKEND_BODY {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("chunked backend response exceeds the {MAX_BACKEND_BODY} byte limit"),
        ));
    }

    let mut pos = 0usize;

    loop {
        let crlf = find_crlf(&pending[pos..]).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "chunked: missing CRLF after size")
        })?;
        let size_line = std::str::from_utf8(&pending[pos..pos + crlf])
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "chunked: size not utf8"))?;
        // chunk-ext (everything from the first ';') is permitted and ignored.
        let size_str = size_line.split(';').next().unwrap_or("").trim();
        if size_str.is_empty() || !size_str.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("chunked: invalid chunk size {size_str:?}"),
            ));
        }
        let size = usize::from_str_radix(size_str, 16)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "chunked: chunk size out of range"))?;
        pos += crlf + 2;

        if size == 0 {
            break;
        }

        if pos + size > pending.len() {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "chunked: data shorter than declared size",
            ));
        }

        body.extend_from_slice(&pending[pos..pos + size]);
        pos += size;

        // Required, not optional. Anything else here means the declared size
        // and the actual data disagree.
        if pos + 2 > pending.len() || &pending[pos..pos + 2] != b"\r\n" {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "chunked: chunk data not terminated by CRLF",
            ));
        }
        pos += 2;
    }

    // Trailer section: zero or more field lines, then a final CRLF. Previously
    // the decoder returned at the zero chunk without looking, so a message that
    // simply stopped mid-trailer was indistinguishable from a complete one.
    loop {
        let crlf = find_crlf(&pending[pos..]).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "chunked: body ended before the trailer section was terminated",
            )
        })?;
        if crlf == 0 {
            break; // the empty line that ends the message
        }
        let line = std::str::from_utf8(&pending[pos..pos + crlf])
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "chunked: trailer not utf8"))?;
        // A trailer is an ordinary field line. Enforced so a malformed one is a
        // parse error rather than being mistaken for the terminator.
        match line.split_once(':') {
            Some((name, _)) if h1_field_name_is_safe(name.trim()) => {}
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("chunked: malformed trailer field {line:?}"),
                ))
            }
        }
        pos += crlf + 2;
    }

    Ok(body)
}

fn find_crlf(buf: &[u8]) -> Option<usize> {
    buf.windows(2).position(|w| w == b"\r\n")
}

/// Parse an incoming HTTP/1.1 request from a byte buffer.
pub fn parse_request(data: &[u8]) -> Result<HttpRequest, String> {
    let mut headers = [httparse::EMPTY_HEADER; 64];
    let mut req = httparse::Request::new(&mut headers);

    let body_start = match req.parse(data) {
        Ok(httparse::Status::Complete(n)) => n,
        Ok(httparse::Status::Partial) => return Err("incomplete request".to_string()),
        Err(e) => return Err(format!("parse error: {}", e)),
    };

    let method = req.method.ok_or("missing method")?.to_string();
    let raw_path = req.path.ok_or("missing path")?.to_string();
    let version = match req.version {
        Some(1) => "HTTP/1.1".to_string(),
        Some(0) => "HTTP/1.0".to_string(),
        _ => "HTTP/1.1".to_string(),
    };

    let (path, query) = if let Some(idx) = raw_path.find('?') {
        let p = raw_path[..idx].to_string();
        let q = raw_path[idx + 1..].to_string();
        (p, Some(q))
    } else {
        (raw_path, None)
    };

    let mut parsed_headers = Vec::new();
    for h in req.headers.iter() {
        if h.name.is_empty() {
            break;
        }
        let value = std::str::from_utf8(h.value)
            .map_err(|_| "header value not UTF-8")?
            .to_string();
        parsed_headers.push((h.name.to_string(), value));
    }

    let body = data[body_start..].to_vec();

    Ok(HttpRequest { method, path, query, version, headers: parsed_headers, body })
}

/// Forward a request to a URL backend over HTTP/1.1.
///
/// `base_url` must be `https://host[:port]` (HTTP/1.1 over TLS) or
/// `http://host[:port]` (HTTP/1.1 plain).
///
/// `h2c://` and `h2s://` backends are dispatched via their respective
/// persistent client pools (`H2cClientPool` / `H2sTlsClientPool`) and must
/// NOT be passed to this function.
///
/// `tls_config` is a pre-built rustls ClientConfig (built once at startup by
/// PoolManager — avoids expensive per-request native cert loading).
///
/// URL backends are not pooled — a new TCP connection is opened for every
/// request. The `timeout` (if given) is applied as both read and write
/// timeout on the underlying TCP stream.
pub fn forward_url_request(
    base_url: &str,
    req: &HttpRequest,
    client_ip: &str,
    original_host: &str,
    timeout: Option<std::time::Duration>,
    tls_config: std::sync::Arc<rustls::ClientConfig>,
) -> io::Result<HttpResponse> {
    // Same gate as the unix-socket path. Checked here rather than only inside
    // `build_forwarded_request_bytes` so the refusal happens before a TCP
    // connection is opened to the upstream.
    if let Err(what) = check_forwardable(req, client_ip, original_host) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("refusing to forward: unsafe {what} would inject HTTP/1.1 framing"),
        ));
    }

    // ── Parse URL ────────────────────────────────────────────────────────────
    let (scheme, authority) = parse_url_scheme_authority(base_url)?;
    let (host, port) = split_host_port(&authority, match scheme.as_str() {
        "https" => 443,
        _ => 80, // http and h2c both default to 80
    })?;

    // ── TCP connect ──────────────────────────────────────────────────────────
    use std::net::TcpStream;
    let addr = format!("{}:{}", host, port);
    let tcp = TcpStream::connect(&addr)?;
    if let Some(dur) = timeout {
        tcp.set_read_timeout(Some(dur))?;
        tcp.set_write_timeout(Some(dur))?;
    }

    // ── Send + receive ───────────────────────────────────────────────────────
    if scheme == "https" {
        let req_bytes = build_forwarded_request_bytes(req, &host, client_ip, original_host);
        forward_over_tls(tcp, &host, req_bytes, tls_config, &req.method)
    } else if scheme == "h2c" {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "h2c:// backends must be dispatched via H2cClientPool, not forward_url_request",
        ))
    } else if scheme == "h2s" {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "h2s:// backends must be dispatched via H2sTlsClientPool, not forward_url_request",
        ))
    } else {
        use std::io::Write;
        let req_bytes = build_forwarded_request_bytes(req, &host, client_ip, original_host);
        let mut stream = tcp;
        stream.write_all(&req_bytes)?;
        stream.flush()?;
        read_response_for(stream, &req.method)
    }
}

/// Context carried alongside a pending URL-backend receiver so the event loop
/// can finish processing (cache insertion, hints, error-mode) when the I/O
/// thread returns.  All fields are cheaply cloneable.
#[derive(Clone)]
pub struct PendingUrlContext {
    pub req:          HttpRequest,
    pub client_ip:    String,
    pub enc:          String,
    pub backend_name: String,
    /// Whether the response may enter the shared cache. False for
    /// `require`-protected routes (the cache key carries no identity, so a
    /// stored entry would later be served to anonymous callers) and for
    /// internal error-page fetches.
    pub cacheable: bool,
    /// This dispatch is a synthetic background fetch (a hint prefetch or a
    /// stale-while-revalidate refresh), not a real visit. It exists only to
    /// fill the cache; there is no client, and `client_ip` is a placeholder.
    /// Analytics must skip it, exactly as the synchronous path already does
    /// via `handle_request`'s `is_prefetch` -- otherwise every refresh logs
    /// itself as a request from 127.0.0.1 and corrupts the visit counts.
    pub is_prefetch: bool,
    pub start:        std::time::Instant,   // request arrival time for miss timing
    /// Set when this dispatch is itself a `[errors] mode = "custom"` fetch of
    /// the error page (rather than a normal routed request) — carries the
    /// ORIGINAL failing status so the fetched body can be returned under it.
    /// `None` for every ordinary request dispatch.
    pub error_status_override: Option<u16>,
}

/// Dispatch a URL-backend request to a dedicated I/O thread.
///
/// Returns a one-shot channel.  The caller MUST poll with `try_recv()` inside
/// the event loop — never block waiting on the receiver.
pub fn dispatch_url_request(
    base_url:      String,
    req:           HttpRequest,
    client_ip:     String,
    original_host: String,
    timeout:       Option<std::time::Duration>,
    tls_config:    std::sync::Arc<rustls::ClientConfig>,
) -> std::sync::mpsc::Receiver<std::io::Result<HttpResponse>> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(forward_url_request(
            &base_url, &req, &client_ip, &original_host, timeout, tls_config,
        ));
    });
    rx
}

/// Build a forwarded HTTP/1.1 request byte buffer (no TLS framing).
/// Serialise an HTTP/1.1 request for an upstream.
///
/// **Invariant: every caller must have run `check_forwardable` first.** This
/// writes header names and values into CRLF-delimited syntax verbatim, so an
/// unchecked CR or LF here is request smuggling (F036/F094). It cannot do the
/// check itself because it returns bytes rather than a Result, and making it
/// fallible would push the failure past the point where a connection has
/// already been opened. Both current callers gate at the top of
/// `forward_url_request`.
fn build_forwarded_request_bytes(
    req: &HttpRequest,
    host: &str,
    client_ip: &str,
    original_host: &str,
) -> Vec<u8> {
    let mut buf = Vec::with_capacity(4096);

    buf.extend_from_slice(req.method.as_bytes());
    buf.push(b' ');
    buf.extend_from_slice(req.path.as_bytes());
    if let Some(ref q) = req.query {
        if !q.is_empty() {
            buf.push(b'?');
            buf.extend_from_slice(q.as_bytes());
        }
    }
    buf.extend_from_slice(b" HTTP/1.1\r\n");

    // Host header
    buf.extend_from_slice(b"Host: ");
    buf.extend_from_slice(host.as_bytes());
    buf.extend_from_slice(b"\r\n");

    // Forward original headers (minus hop-by-hop, proxy-owned, and Host)
    for (name, value) in &req.headers {
        if skip_when_forwarding(name) {
            continue;
        }
        if name.eq_ignore_ascii_case("host") {
            continue; // already written above
        }
        buf.extend_from_slice(name.as_bytes());
        buf.extend_from_slice(b": ");
        buf.extend_from_slice(value.as_bytes());
        buf.extend_from_slice(b"\r\n");
    }

    // Proxy headers
    buf.extend_from_slice(b"X-Forwarded-For: ");
    buf.extend_from_slice(client_ip.as_bytes());
    buf.extend_from_slice(b"\r\nX-Forwarded-Proto: https\r\nX-Forwarded-Host: ");
    buf.extend_from_slice(original_host.as_bytes());
    buf.extend_from_slice(b"\r\n");

    // Content-Length
    if !req.body.is_empty() {
        buf.extend_from_slice(b"Content-Length: ");
        let mut cl_buf = [0u8; 20];
        buf.extend_from_slice(write_decimal(req.body.len(), &mut cl_buf));
        buf.extend_from_slice(b"\r\n");
    }

    buf.extend_from_slice(b"Connection: close\r\n\r\n");

    if !req.body.is_empty() {
        buf.extend_from_slice(&req.body);
    }

    buf
}

/// Parse `scheme://authority` from a URL string.
fn parse_url_scheme_authority(url: &str) -> io::Result<(String, String)> {
    let sep = "://";
    let idx = url.find(sep).ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, format!("invalid URL: {}", url))
    })?;
    let scheme = url[..idx].to_lowercase();
    let rest = &url[idx + sep.len()..];
    // Authority ends at the first `/`, `?`, or `#`
    let auth_end = rest
        .find(|c| c == '/' || c == '?' || c == '#')
        .unwrap_or(rest.len());
    let authority = rest[..auth_end].to_string();
    Ok((scheme, authority))
}

/// Split `host:port` or `[ipv6]:port` into (host, port).
fn split_host_port(authority: &str, default_port: u16) -> io::Result<(String, u16)> {
    // IPv6: `[::1]:8443`
    if let Some(bracket_end) = authority.find(']') {
        let host = authority[1..bracket_end].to_string();
        let port = if bracket_end + 1 < authority.len() && authority.as_bytes()[bracket_end + 1] == b':' {
            authority[bracket_end + 2..].parse::<u16>().map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidInput, "invalid port in URL")
            })?
        } else {
            default_port
        };
        return Ok((host, port));
    }
    if let Some(colon) = authority.rfind(':') {
        if let Ok(p) = authority[colon + 1..].parse::<u16>() {
            return Ok((authority[..colon].to_string(), p));
        }
    }
    Ok((authority.to_string(), default_port))
}

/// Send `req_bytes` over a TLS-wrapped TCP stream to `host` and read back the response.
/// `tls_config` is the pre-built ClientConfig from PoolManager (avoids per-request cert loading).
fn forward_over_tls(
    tcp: std::net::TcpStream,
    host: &str,
    req_bytes: Vec<u8>,
    tls_config: std::sync::Arc<rustls::ClientConfig>,
    request_method: &str,
) -> io::Result<HttpResponse> {
    use std::io::Write;
    use rustls::ClientConnection;
    use rustls::StreamOwned;

    let server_name = rustls::pki_types::ServerName::try_from(host.to_string()).map_err(|_| {
        io::Error::new(io::ErrorKind::InvalidInput, format!("invalid server name: {}", host))
    })?;

    let conn = ClientConnection::new(tls_config, server_name).map_err(|e| {
        io::Error::new(io::ErrorKind::Other, format!("TLS init error: {}", e))
    })?;

    let mut tls_stream = StreamOwned::new(conn, tcp);
    tls_stream.write_all(&req_bytes)?;
    tls_stream.flush()?;
    read_response_for(tls_stream, request_method)
}

/// Build a simple HTTP response buffer (kept for tests).
pub fn build_response(status: u16, reason: &str, headers: &[(String, String)], body: &[u8]) -> Vec<u8> {
    use std::fmt::Write;
    let mut out = String::new();
    let _ = write!(out, "HTTP/1.1 {} {}\r\n", status, reason);
    for (k, v) in headers {
        let _ = write!(out, "{}: {}\r\n", k, v);
    }
    let _ = write!(out, "Content-Length: {}\r\n", body.len());
    let _ = write!(out, "Connection: close\r\n");
    let _ = write!(out, "\r\n");
    let mut bytes = out.into_bytes();
    bytes.extend_from_slice(body);
    bytes
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_status_line() {
        let (status, reason) = parse_status_line("HTTP/1.1 200 OK").unwrap();
        assert_eq!(status, 200);
        assert_eq!(reason, "OK");
    }

    #[test]
    fn test_parse_status_line_no_reason() {
        let (status, reason) = parse_status_line("HTTP/1.1 404").unwrap();
        assert_eq!(status, 404);
        assert_eq!(reason, "");
    }

    #[test]
    fn test_read_response_basic() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello";
        let resp = read_response(&raw[..]).unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body, b"hello");
    }

    #[test]
    fn test_read_response_chunked() {
        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n0\r\n\r\n";
        let resp = read_response(&raw[..]).unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body, b"hello");
    }

    #[test]
    fn test_parse_request_basic() {
        let raw = b"GET /hello?foo=bar HTTP/1.1\r\nHost: example.com\r\n\r\n";
        let req = parse_request(raw).unwrap();
        assert_eq!(req.method, "GET");
        assert_eq!(req.path, "/hello");
        assert_eq!(req.query, Some("foo=bar".to_string()));
    }

    #[test]
    fn test_hop_by_hop_not_forwarded() {
        // The HOP_BY_HOP list should contain expected headers
        assert!(HOP_BY_HOP.contains(&"connection"));
        assert!(HOP_BY_HOP.contains(&"upgrade"));
        assert!(HOP_BY_HOP.contains(&"transfer-encoding"));
        assert!(HOP_BY_HOP.contains(&"keep-alive"));
    }

    #[test]
    fn test_build_response() {
        let buf = build_response(200, "OK", &[], b"hello");
        let s = std::str::from_utf8(&buf).unwrap();
        assert!(s.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(s.contains("Content-Length: 5\r\n"));
        assert!(s.ends_with("hello"));
    }

    // ── URL forwarding helpers ────────────────────────────────────────────────

    #[test]
    fn test_parse_url_scheme_authority_https() {
        let (scheme, authority) = parse_url_scheme_authority("https://api.example.com").unwrap();
        assert_eq!(scheme, "https");
        assert_eq!(authority, "api.example.com");
    }

    #[test]
    fn test_parse_url_scheme_authority_with_path() {
        let (scheme, authority) = parse_url_scheme_authority("https://api.example.com/v1/foo").unwrap();
        assert_eq!(scheme, "https");
        assert_eq!(authority, "api.example.com");
    }

    #[test]
    fn test_parse_url_scheme_authority_with_port() {
        let (scheme, authority) = parse_url_scheme_authority("https://api.example.com:8443/v1").unwrap();
        assert_eq!(scheme, "https");
        assert_eq!(authority, "api.example.com:8443");
    }

    #[test]
    fn test_parse_url_invalid_no_scheme() {
        assert!(parse_url_scheme_authority("api.example.com").is_err());
    }

    #[test]
    fn test_split_host_port_default() {
        let (host, port) = split_host_port("api.example.com", 443).unwrap();
        assert_eq!(host, "api.example.com");
        assert_eq!(port, 443);
    }

    #[test]
    fn test_split_host_port_explicit() {
        let (host, port) = split_host_port("api.example.com:8443", 443).unwrap();
        assert_eq!(host, "api.example.com");
        assert_eq!(port, 8443);
    }

    #[test]
    fn test_split_host_port_ipv6() {
        let (host, port) = split_host_port("[::1]:9000", 443).unwrap();
        assert_eq!(host, "::1");
        assert_eq!(port, 9000);
    }

    #[test]
    fn test_split_host_port_ipv6_default() {
        let (host, port) = split_host_port("[::1]", 443).unwrap();
        assert_eq!(host, "::1");
        assert_eq!(port, 443);
    }

    #[test]
    fn test_build_forwarded_request_bytes_basic() {
        let req = HttpRequest {
            method: "GET".to_string(),
            path: "/api/test".to_string(),
            query: Some("key=value".to_string()),
            version: "HTTP/3".to_string(),
            headers: vec![
                ("accept".to_string(), "application/json".to_string()),
                ("connection".to_string(), "keep-alive".to_string()), // hop-by-hop, should be stripped
            ],
            body: vec![],
        };
        let bytes = build_forwarded_request_bytes(&req, "api.example.com", "1.2.3.4", "original.host");
        let s = std::str::from_utf8(&bytes).unwrap();

        assert!(s.starts_with("GET /api/test?key=value HTTP/1.1\r\n"));
        assert!(s.contains("Host: api.example.com\r\n"));
        assert!(s.contains("accept: application/json\r\n"));
        // hop-by-hop 'connection' must not appear
        assert!(!s.contains("connection: keep-alive"));
        assert!(s.contains("X-Forwarded-For: 1.2.3.4\r\n"));
        assert!(s.contains("X-Forwarded-Host: original.host\r\n"));
        // Connection: close must be present
        assert!(s.contains("Connection: close\r\n"));
    }

    #[test]
    fn test_forward_request_timeout_conn_refused() {
        // Connecting to a port that has no listener should fail, not hang.
        let req = HttpRequest {
            method: "GET".to_string(),
            path: "/".to_string(),
            query: None,
            version: "HTTP/1.1".to_string(),
            headers: vec![],
            body: vec![],
        };
        // /tmp/nonexistent-m6-test.sock will not exist
        let path = std::path::Path::new("/tmp/nonexistent-m6-test-timeout.sock");
        let timeout = std::time::Duration::from_millis(100);
        let result = forward_request_timeout(path, &req, "127.0.0.1", "localhost", Some(timeout));
        assert!(result.is_err());
    }
}

#[cfg(test)]
mod smuggling_tests {
    use super::*;

    fn req_with(name: &str, value: &str) -> HttpRequest {
        HttpRequest {
            method: "GET".to_string(),
            path: "/".to_string(),
            query: None,
            version: "HTTP/1.1".to_string(),
            headers: vec![(name.to_string(), value.to_string())],
            body: Vec::new(),
        }
    }

    /// F036/F094. HTTP/2 and HTTP/3 field values are length-delimited, so a
    /// decoded value may contain any byte. HTTP/1.1 is CRLF-delimited, so
    /// writing such a value out verbatim turns it into framing and smuggles a
    /// request into a backend that trusts this proxy.
    #[test]
    fn crlf_in_a_header_value_is_refused() {
        let attacks = [
            "evil\r\nX-Injected: yes",
            "evil\r\n\r\nGET /admin HTTP/1.1\r\nHost: internal",
            "evil\nX-Injected: bare-lf",   // bare LF: many parsers accept it
            "evil\rX-Injected: bare-cr",
            "evil\0truncated",
        ];
        for a in attacks {
            let r = req_with("X-Test", a);
            assert!(
                check_forwardable(&r, "1.2.3.4", "example.com").is_err(),
                "value {a:?} must be refused"
            );
        }
    }

    /// A header the proxy strips can never reach the backend, so refusing on it
    /// would reject traffic for no gain. Pinned so the skip list and the check
    /// cannot drift apart into either a hole or a false refusal.
    #[test]
    fn hop_by_hop_headers_are_not_judged() {
        let mut r = req_with("Connection", "keep-alive\r\nX-Injected: yes");
        assert!(check_forwardable(&r, "1.2.3.4", "example.com").is_ok());
        // ...but the same value on a forwarded header still fails.
        r.headers = vec![("X-Real".to_string(), "v\r\nX-Injected: yes".to_string())];
        assert!(check_forwardable(&r, "1.2.3.4", "example.com").is_err());
    }

    /// The request line is assembled from method, path and query. A space is as
    /// dangerous as a CR there: it forges the next token.
    #[test]
    fn request_line_components_are_checked() {
        let bad = [
            ("GET /x HTTP/1.1\r\nX-I: 1", "/", None),
            ("GET", "/a b", None),
            ("GET", "/a\r\nX-I: 1", None),
            ("GET", "/", Some("a=1 HTTP/1.1")),
            ("GET", "/", Some("a=1\r\nX-I: 1")),
            ("", "/", None),
        ];
        for (m, p, q) in bad {
            let r = HttpRequest {
                method: m.to_string(),
                path: p.to_string(),
                query: q.map(str::to_string),
                version: "HTTP/1.1".to_string(),
                headers: vec![],
                body: Vec::new(),
            };
            assert!(
                check_forwardable(&r, "1.2.3.4", "example.com").is_err(),
                "method={m:?} path={p:?} query={q:?} must be refused"
            );
        }
    }

    /// Both are derived from client-controlled input and are written into
    /// headers this proxy adds itself.
    #[test]
    fn proxy_added_headers_are_checked() {
        let r = req_with("X-Test", "fine");
        assert!(check_forwardable(&r, "1.2.3.4\r\nX-Injected: yes", "example.com").is_err());
        assert!(check_forwardable(&r, "1.2.3.4", "example.com\r\nX-Injected: yes").is_err());
    }

    /// A field name must be a token: a colon or space in it splits one header
    /// into two without needing a line break at all.
    #[test]
    fn malformed_field_names_are_refused() {
        for n in ["X Test", "X:Test", "X\r\nY", "", "X\tY"] {
            assert!(
                check_forwardable(&req_with(n, "v"), "1.2.3.4", "example.com").is_err(),
                "name {n:?} must be refused"
            );
        }
    }

    /// Ordinary traffic must still pass, including obs-text, which RFC 9110 5.5
    /// permits in a field value. Guards against "fixing" this by rejecting
    /// anything non-ASCII.
    #[test]
    fn legitimate_requests_still_pass() {
        let r = HttpRequest {
            method: "POST".to_string(),
            path: "/contact".to_string(),
            query: Some("v=1&x=%E2%80%99".to_string()),
            version: "HTTP/1.1".to_string(),
            headers: vec![
                ("Content-Type".to_string(), "application/x-www-form-urlencoded".to_string()),
                ("User-Agent".to_string(), "Mozilla/5.0 (Macintosh)".to_string()),
                ("X-Obs-Text".to_string(), "caf\u{e9} \u{2014} fine".to_string()),
                ("Accept".to_string(), "text/html;q=0.9, */*".to_string()),
            ],
            body: b"a=1".to_vec(),
        };
        assert_eq!(check_forwardable(&r, "203.0.113.7", "mgrosvenor.com"), Ok(()));
    }
}

#[cfg(test)]
mod response_framing_tests {
    use super::*;

    /// F028. The parser kept the last `Content-Length` it saw, so a backend
    /// emitting two different lengths framed the response by whichever came
    /// last. If this hop and the next hop disagree about the length, the
    /// remainder becomes the head of the following response on a reused
    /// connection -- response splitting.
    #[test]
    fn conflicting_content_length_is_refused() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nContent-Length: 10\r\n\r\nhello";
        let err = read_response(&raw[..]).unwrap_err();
        assert!(
            err.to_string().contains("conflicting Content-Length"),
            "got: {err}"
        );
    }

    /// Repeated but identical values are unambiguous, so they must still work.
    /// Without this the fix above would reject legitimate traffic.
    #[test]
    fn duplicate_but_equal_content_length_is_accepted() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nContent-Length: 5\r\n\r\nhello";
        let r = read_response(&raw[..]).expect("equal duplicates are unambiguous");
        assert_eq!(r.body, b"hello");
    }

    /// A single field carrying a list must be checked element-wise too.
    #[test]
    fn content_length_list_must_agree() {
        assert!(read_response(&b"HTTP/1.1 200 OK\r\nContent-Length: 5, 6\r\n\r\nhello"[..]).is_err());
        let r = read_response(&b"HTTP/1.1 200 OK\r\nContent-Length: 5, 5\r\n\r\nhello"[..]).unwrap();
        assert_eq!(r.body, b"hello");
    }

    /// F029. RFC 9112 6.1 forbids sending both. A recipient that picks one is
    /// the classic smuggling primitive because the next hop may pick the other.
    #[test]
    fn transfer_encoding_with_content_length_is_refused() {
        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nContent-Length: 5\r\n\r\n0\r\n\r\n";
        let err = read_response(&raw[..]).unwrap_err();
        assert!(err.to_string().contains("both Transfer-Encoding and Content-Length"), "got: {err}");
    }

    /// F030. Transfer-Encoding is a list and only the FINAL coding frames the
    /// message. `gzip, chunked` is chunked; the old check compared the whole
    /// value to "chunked" and so treated this as unframed, handing the chunk
    /// envelope back as though it were the body.
    #[test]
    fn transfer_encoding_list_ending_in_chunked_is_chunked() {
        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: gzip, chunked\r\n\r\n5\r\nhello\r\n0\r\n\r\n";
        let r = read_response(&raw[..]).expect("gzip, chunked is chunked");
        assert_eq!(r.body, b"hello", "chunk envelope was not decoded");
    }

    /// The converse: a Transfer-Encoding NOT ending in chunked leaves the
    /// message with no self-delimiting framing at all.
    #[test]
    fn transfer_encoding_not_ending_in_chunked_is_refused() {
        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked, gzip\r\n\r\nhello";
        let err = read_response(&raw[..]).unwrap_err();
        assert!(err.to_string().contains("not ending in chunked"), "got: {err}");
    }

    /// F031. An unparseable length used to become `None` via `.ok()` and fall
    /// through to read-to-EOF, quietly switching framing mode on malformed
    /// input instead of rejecting it.
    #[test]
    fn invalid_content_length_is_refused_not_read_to_eof() {
        for bad in ["abc", "5x", "-1", "0x10", "+5", ""] {
            let raw = format!("HTTP/1.1 200 OK\r\nContent-Length: {bad}\r\n\r\nhello");
            let err = read_response(raw.as_bytes()).unwrap_err();
            assert!(
                err.to_string().contains("invalid Content-Length"),
                "Content-Length {bad:?} should be refused, got: {err}"
            );
        }
    }

    /// F035. A response to HEAD carries the headers a GET would, Content-Length
    /// included, but no body. Without the method the reader blocked waiting for
    /// bytes a correct backend never sends, so every HEAD to a backend stalled
    /// until the read timeout.
    ///
    /// The reader here yields EOF immediately after the headers: if the code
    /// tries to read a body at all this fails, which is exactly the stall.
    #[test]
    fn head_response_is_bodyless_despite_content_length() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 16422\r\n\r\n";
        let r = read_response_for(&raw[..], "HEAD").expect("HEAD must not wait for a body");
        assert!(r.body.is_empty());
        assert_eq!(r.header("content-length"), Some("16422"),
            "the header must survive; only the body is absent");
    }

    /// Same rule, driven by status rather than method (RFC 9112 6.3).
    #[test]
    fn status_codes_that_never_have_a_body() {
        for status in [204u16, 304, 100, 101] {
            let raw = format!("HTTP/1.1 {status} X\r\nContent-Length: 99\r\n\r\n");
            let r = read_response(raw.as_bytes())
                .unwrap_or_else(|e| panic!("{status} must not wait for a body: {e}"));
            assert!(r.body.is_empty(), "{status} must have no body");
        }
    }

    /// A GET with the same headers still reads its body, so the bodyless rule
    /// cannot have been implemented by ignoring bodies generally.
    #[test]
    fn get_still_reads_its_body() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello";
        assert_eq!(read_response_for(&raw[..], "GET").unwrap().body, b"hello");
    }
}

#[cfg(test)]
mod chunked_tests {
    use super::read_response;

    fn resp(body: &str) -> Vec<u8> {
        format!("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n{body}").into_bytes()
    }

    /// The happy path, including a trailer section, so the stricter parsing
    /// cannot have been achieved by rejecting valid messages.
    #[test]
    fn well_formed_chunked_bodies_decode() {
        for (body, want) in [
            ("5\r\nhello\r\n0\r\n\r\n", "hello"),
            ("5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n", "hello world"),
            ("0\r\n\r\n", ""),
            // chunk-ext is permitted and ignored
            ("5;foo=bar\r\nhello\r\n0\r\n\r\n", "hello"),
            // uppercase hex
            ("A\r\n0123456789\r\n0\r\n\r\n", "0123456789"),
            // trailers
            ("5\r\nhello\r\n0\r\nX-Checksum: abc\r\n\r\n", "hello"),
        ] {
            let r = read_response(&resp(body)[..])
                .unwrap_or_else(|e| panic!("{body:?} should decode: {e}"));
            assert_eq!(String::from_utf8_lossy(&r.body), want, "for {body:?}");
        }
    }

    /// F032. The CRLF after chunk data used to be skipped only if present, so a
    /// chunk whose data was followed by anything else resynchronised onto the
    /// wrong offset and the remainder was parsed as chunk headers.
    #[test]
    fn missing_crlf_after_chunk_data_is_refused() {
        let err = read_response(&resp("5\r\nhelloXX0\r\n\r\n")[..]).unwrap_err();
        assert!(err.to_string().contains("not terminated by CRLF"), "got: {err}");
    }

    /// F033/F034. The decoder returned at the zero chunk without confirming the
    /// message actually ended, so a truncated trailer section was
    /// indistinguishable from a complete body.
    #[test]
    fn truncated_trailer_section_is_refused() {
        for body in ["5\r\nhello\r\n0\r\n", "5\r\nhello\r\n0\r\nX-Trailer: v\r\n"] {
            let err = read_response(&resp(body)[..])
                .unwrap_err();
            assert!(
                err.to_string().contains("trailer section was terminated"),
                "{body:?} should be refused, got: {err}"
            );
        }
    }

    #[test]
    fn malformed_trailer_is_refused() {
        let err = read_response(&resp("5\r\nhello\r\n0\r\nnot a header line\r\n\r\n")[..]).unwrap_err();
        assert!(err.to_string().contains("malformed trailer"), "got: {err}");
    }

    /// `usize::from_str_radix` accepts a leading `+`, so `+A` parsed as 10 --
    /// the same trap that let `Content-Length: +5` through.
    #[test]
    fn chunk_size_must_be_plain_hex() {
        for bad in ["+A", "-5", "", " ", "0x5", "5g"] {
            let body = format!("{bad}\r\nhello\r\n0\r\n\r\n");
            let err = read_response(&resp(&body)[..]).unwrap_err();
            assert!(
                err.to_string().contains("invalid chunk size")
                    || err.to_string().contains("missing CRLF"),
                "chunk size {bad:?} should be refused, got: {err}"
            );
        }
    }

    /// A chunk claiming more data than was sent must not return a short body as
    /// though it were complete.
    #[test]
    fn short_chunk_data_is_refused() {
        let err = read_response(&resp("10\r\nhello\r\n0\r\n\r\n")[..]).unwrap_err();
        assert!(err.to_string().contains("shorter than declared"), "got: {err}");
    }
}
