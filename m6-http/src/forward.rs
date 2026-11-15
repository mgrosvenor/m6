/// HTTP/1.1 request forwarding: the request bytes a backend receives, in the
/// two connection policies (one request per connection, or keep-alive).
use std::io;

pub use m6_core::client::too_many_informational;
use m6_core::client::{is_skipped_informational, MAX_INFORMATIONAL};
use m6_core::platform::Token;

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
pub const UNTRUSTED_INBOUND: &[&str] = &[
    "x-auth-claims",
    "x-forwarded-for",
    "x-forwarded-proto",
    "x-forwarded-host",
    "x-real-ip",
];

/// Whether a listener is allowed to believe a forwarded client IP.
///
/// The address a request is attributed to decides two things that matter: the
/// rate-limit bucket it counts against, and the `client_ip` in analytics. Get
/// it from a header a client can set and the rate limiter is trivially
/// bypassed by rotating the value, which is why [`UNTRUSTED_INBOUND`] strips
/// `x-forwarded-for` from every request on every protocol and nothing
/// downstream has ever been allowed to look at it.
///
/// That is right for the public listener and wrong for the backbone. A cache
/// node forwards to origin over `h2c://origin.example.com:80` and already sends the real
/// client IP (`h2c_client.rs`), so origin was stripping the one accurate
/// answer it had and attributing every relayed request to the tunnel address.
/// The visible cost was analytics: three of five crawler sightings in an hour
/// were unattributable. The larger cost was the rate limiter, where **every
/// request arriving through one cache node shared a single bucket** -- so one
/// abusive client could throttle every other visitor routed through that node,
/// and its own quota was diluted by theirs.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ForwardedTrust {
    /// A forwarded client IP here is a forgery attempt. The public listener.
    Never,
    /// The peer is one of the deployment's own cache nodes, accepted only from
    /// the addresses in `[server] internal_peers`. Granted by the listener,
    /// never by a header, and only on an h2c listener named in `[server]
    /// internal_binds`, which load accepts only in a local range -- see
    /// `H2cListener::bind`.
    Backbone,
}

/// The single client address a peer asserted, if it asserted exactly one
/// well-formed one.
///
/// `X-Forwarded-For` is defined as a list, and the general rule for reading
/// one is that only the entries appended by hops you trust mean anything. Here
/// the topology makes that simpler and stricter: a cache node strips the
/// client's copy at its own ingress and emits exactly one value of its own, so
/// on the backbone leg there is never a list. Anything else -- two entries, an
/// empty value, something that is not an IP -- did not come from the path this
/// trusts, so it is refused rather than parsed for a plausible-looking member.
///
/// Refusing also bounds the rate limiter's key space. The key would otherwise
/// be an arbitrary string from a header, and a peer able to vary it freely
/// could grow the limiter's map without limit.
pub fn sole_forwarded_ip(value: &str) -> Option<&str> {
    let candidate = value.trim();
    if candidate.contains(',') {
        return None;
    }
    // Parsed, not pattern-matched: this becomes a map key and a log field.
    if candidate.parse::<std::net::IpAddr>().is_err() {
        return None;
    }
    Some(candidate)
}

/// The address to attribute a request to, for rate limiting and analytics.
///
/// `forwarded` is whatever the request carried in `x-forwarded-for` before
/// ingress stripped it. It is stripped either way: the header never reaches a
/// handler, a backend or the cache key on any path. Only the derived address
/// is ever trusted, and only where [`ForwardedTrust::Backbone`] says the peer
/// is entitled to assert one.
pub fn attributed_client_ip<'a>(
    forwarded: Option<&'a str>,
    peer_ip: &'a str,
    trust: ForwardedTrust,
) -> &'a str {
    if trust == ForwardedTrust::Never {
        return peer_ip;
    }
    forwarded.and_then(sole_forwarded_ip).unwrap_or(peer_ip)
}

/// True if `name` is a header a client is never allowed to supply.
#[inline]
pub fn is_untrusted_inbound(name: &str) -> bool {
    UNTRUSTED_INBOUND
        .iter()
        .any(|&h| name.eq_ignore_ascii_case(h))
}

/// Ensure the backend leg carries a `Host`.
///
/// That leg is HTTP/1.1, where a request without `Host` is malformed
/// (RFC 9110 7.2). The client's own is forwarded when it sent one, but three
/// cases legitimately arrive without:
///
/// | client | why |
/// |---|---|
/// | HTTP/1.0 | omitting `Host` is legal, and m6-http serves it |
/// | HTTP/2 | carries the origin in `:authority` |
/// | HTTP/3 | the same, and pseudo-headers are stripped on ingress |
///
/// `original_host` is already the authority in all three, so it is synthesised
/// here, once, rather than in each protocol's own path. Doing it per protocol
/// is exactly what went wrong: HTTP/2 translated `:authority` and HTTP/3 never
/// did, so every HTTP/3 request reached the backend naming no origin at all.
///
/// It stayed invisible while the backends accepted anything. Making them
/// strict turned it into a 400 on every HTTP/3 cache miss and on every
/// HTTP/1.0 request, which is how it was found.
fn write_host_if_absent(buf: &mut Vec<u8>, req: &HttpRequest, original_host: &str) {
    if original_host.is_empty() || m6_core::headers::contains(&req.headers[..], "host") {
        return;
    }
    buf.extend_from_slice(b"Host: ");
    buf.extend_from_slice(original_host.as_bytes());
    buf.extend_from_slice(b"\r\n");
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
    HOP_BY_HOP.iter().any(|&h| name.eq_ignore_ascii_case(h)) || proxy_owned_request_header(name)
}

/// The pseudonym this proxy identifies itself by in `Via`.
///
/// Deliberately a fixed token rather than the node's hostname. RFC 9110 7.6.3
/// allows a pseudonym precisely so an intermediary need not disclose its
/// internal naming, and "origin"/"edge-a"/"edge-b" would hand every client a map of the
/// topology for no benefit.
const VIA_PSEUDONYM: &str = "m6";

/// Field names nominated by a `Connection` header, which must be removed
/// before forwarding (RFC 9110 7.6.1).
///
/// **F018/F019.** `Connection` lists the fields that apply to *this* hop only.
/// The static [`HOP_BY_HOP`] list covers the well-known ones, but a sender may
/// nominate any field -- `Connection: X-Session-Hint` means that header dies
/// here. Forwarding it on is both a spec violation and a real hazard: it lets a
/// client smuggle a header past an intermediary that believed it had been
/// consumed, which is the same shape of confusion that makes request smuggling
/// work.
///
/// Returns lowercase names so the caller can compare cheaply.
fn connection_nominated(headers: &[(String, String)]) -> Vec<String> {
    let mut out = Vec::new();
    for (name, value) in headers {
        if !name.eq_ignore_ascii_case("connection") {
            continue;
        }
        for token in value.split(',') {
            let token = token.trim();
            // `close` and `keep-alive` are connection OPTIONS, not field names.
            // Treating them as fields would be harmless but wrong, and would
            // make the list confusing to read in a log.
            if token.is_empty()
                || token.eq_ignore_ascii_case("close")
                || token.eq_ignore_ascii_case("keep-alive")
                || token.eq_ignore_ascii_case("upgrade")
            {
                continue;
            }
            out.push(token.to_ascii_lowercase());
        }
    }
    out
}

/// Build the `Via` value to append for a forwarded request (RFC 9110 7.6.3).
///
/// **F017.** A proxy must announce itself, and `Via` is how a loop is detected
/// and how a response's path is explained. m6 forwarded everything anonymously,
/// so a request that came back round to this proxy was indistinguishable from a
/// fresh one.
///
/// `received-protocol` is the version the request arrived on -- HTTP/2 and
/// HTTP/3 requests are reported as `2` and `3` even though they leave here as
/// HTTP/1.1, because Via describes the hop that was received, not the one being
/// sent. Any existing Via is preserved and this appends to it.
fn via_value(existing: Option<&str>, received_version: &str) -> String {
    let proto = match received_version {
        v if v.eq_ignore_ascii_case("HTTP/2") || v == "2" => "2",
        v if v.eq_ignore_ascii_case("HTTP/3") || v == "3" => "3",
        v if v.eq_ignore_ascii_case("HTTP/1.0") => "1.0",
        _ => "1.1",
    };
    let mine = format!("{proto} {VIA_PSEUDONYM}");
    match existing {
        Some(prev) if !prev.trim().is_empty() => format!("{}, {}", prev.trim(), mine),
        _ => mine,
    }
}

/// The request type is `m6_core::http::RawRequest`, parsed by the one parser
/// in `m6_core::h1`.
///
/// m6-http used to define its own, structurally identical to core's once
/// `version` moved there. Two names for one shape is how two parsers stay
/// alive: the edge kept its copy because its copy returned its type.
pub use m6_core::http::RawRequest as HttpRequest;

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
        m6_core::headers::get(&self.headers[..], name)
    }

    /// Copy a borrowed [`m6_core::client::Reply`] into the owned form the cache
    /// insert and error mode need. This is the miss path's only per-request
    /// allocation.
    pub fn from_reply(reply: &m6_core::client::Reply<'_>) -> HttpResponse {
        let headers = reply
            .headers
            .iter()
            .map(|h| {
                (
                    h.name.to_string(),
                    String::from_utf8_lossy(h.value).into_owned(),
                )
            })
            .collect();
        HttpResponse {
            status: reply.status,
            reason: String::new(),
            headers,
            body: reply.body.to_vec(),
        }
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
                    b'!' | b'#'
                        | b'$'
                        | b'%'
                        | b'&'
                        | b'\''
                        | b'*'
                        | b'+'
                        | b'-'
                        | b'.'
                        | b'^'
                        | b'_'
                        | b'`'
                        | b'|'
                        | b'~'
                )
        })
}

/// True when `target`, a request path or query, can be written into an
/// HTTP/1.1 request line: no space, which ends the target, and no control byte
/// (RFC 9112 3.2, RFC 3986 2). The one rule for HTTP/2 and HTTP/3 ingress and
/// for the forwarder.
pub fn request_target_is_safe(target: &[u8]) -> bool {
    !target.iter().any(|&b| b <= b' ' || b == 0x7f)
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
    if req.method.is_empty()
        || !req
            .method
            .bytes()
            .all(|b| b.is_ascii_graphic() && b != b'/')
    {
        return Err(format!("method {:?}", req.method));
    }
    for (label, s) in [
        ("path", req.path.as_str()),
        ("query", req.query.as_deref().unwrap_or("")),
    ] {
        // A space here ends the request target and makes the remainder look
        // like the HTTP version token.
        if !request_target_is_safe(s.as_bytes()) {
            return Err(format!("request {label}"));
        }
    }
    for (label, s) in [
        ("X-Forwarded-For", client_ip),
        ("X-Forwarded-Host", original_host),
    ] {
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

/// Build the exact HTTP/1.1 request bytes a socket backend receives, refusing
/// (before any connection) a request that cannot be serialised safely. Writes
/// into `buf` (already cleared), so the caller can point it at a reused buffer.
///
/// The connection closes after one request, so the bytes ask the backend to
/// close it.
pub fn encode_request(
    buf: &mut Vec<u8>,
    req: &HttpRequest,
    client_ip: &str,
    original_host: &str,
) -> io::Result<()> {
    encode_forwarded(buf, req, client_ip, original_host, None, false)
}

/// Build the request bytes a URL backend receives, on a connection kept open
/// for the next request.
///
/// `host` is the backend's own authority and replaces any `Host` the client
/// sent: the backend is the origin of this hop, so its own name is what it must
/// see.
pub fn encode_url_request(
    buf: &mut Vec<u8>,
    req: &HttpRequest,
    host: &str,
    client_ip: &str,
    original_host: &str,
) -> io::Result<()> {
    encode_forwarded(buf, req, client_ip, original_host, Some(host), true)
}

/// The one serializer for both. `host_override` replaces the client's `Host`,
/// and `keep_alive` decides the connection header.
fn encode_forwarded(
    buf: &mut Vec<u8>,
    req: &HttpRequest,
    client_ip: &str,
    original_host: &str,
    host_override: Option<&str>,
    keep_alive: bool,
) -> io::Result<()> {
    // Before the connection, not after: a request that cannot be safely
    // serialised must never reach a backend socket at all.
    if let Err(what) = check_forwardable(req, client_ip, original_host) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("refusing to forward: unsafe {what} would inject HTTP/1.1 framing"),
        ));
    }

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

    // A URL backend is addressed by its own authority, and its virtual host is
    // chosen by that name. A socket backend sits behind the site's name, so it
    // receives the client's.
    if let Some(host) = host_override {
        buf.extend_from_slice(b"Host: ");
        buf.extend_from_slice(host.as_bytes());
        buf.extend_from_slice(b"\r\n");
    }

    // Forward headers, excluding hop-by-hop, anything the proxy owns, and
    // anything the client nominated in `Connection` (RFC 9110 7.6.1).
    let nominated = connection_nominated(&req.headers);
    let mut existing_via: Option<&str> = None;
    for (name, value) in &req.headers {
        if name.eq_ignore_ascii_case("via") {
            existing_via = Some(value.as_str());
            continue; // re-emitted below with our own hop appended
        }
        if skip_when_forwarding(name) || nominated.iter().any(|n| name.eq_ignore_ascii_case(n)) {
            continue;
        }
        if host_override.is_some() && name.eq_ignore_ascii_case("host") {
            continue; // replaced above
        }
        buf.extend_from_slice(name.as_bytes());
        buf.extend_from_slice(b": ");
        buf.extend_from_slice(value.as_bytes());
        buf.extend_from_slice(b"\r\n");
    }
    buf.extend_from_slice(b"Via: ");
    buf.extend_from_slice(via_value(existing_via, &req.version).as_bytes());
    buf.extend_from_slice(b"\r\n");
    if host_override.is_none() {
        write_host_if_absent(buf, req, original_host);
    }

    // Add proxy headers
    buf.extend_from_slice(b"X-Forwarded-For: ");
    buf.extend_from_slice(client_ip.as_bytes());
    buf.extend_from_slice(b"\r\nX-Forwarded-Proto: https\r\nX-Forwarded-Host: ");
    buf.extend_from_slice(original_host.as_bytes());
    buf.extend_from_slice(b"\r\n");

    // Content-Length for body: write decimal without allocating.
    if !req.body.is_empty() {
        buf.extend_from_slice(b"Content-Length: ");
        let mut cl_buf = [0u8; 20];
        buf.extend_from_slice(write_decimal(req.body.len(), &mut cl_buf));
        buf.extend_from_slice(b"\r\n");
    }

    // A socket backend serves one request per connection. A URL backend may
    // keep the connection for the next one, and the reply's own framing decides
    // whether it really does.
    buf.extend_from_slice(if keep_alive {
        b"Connection: keep-alive\r\n\r\n"
    } else {
        b"Connection: close\r\n\r\n"
    });

    // Write body
    if !req.body.is_empty() {
        buf.extend_from_slice(&req.body);
    }

    Ok(())
}

/// What a decoded HTTP/2 response header block turned out to be.
pub enum ResponseBlock {
    /// The final response, or trailers. Its fields are now in the response.
    Final,
    /// An informational block, discarded.
    Informational,
    /// The block that went past [`MAX_INFORMATIONAL`]. The stream should fail.
    TooManyInformational,
}

/// Apply one decoded HTTP/2 response header block to a stream's response.
///
/// An informational block (RFC 9110 15.2) is not the answer, so it is dropped
/// whole: its status and fields must not reach the final response, which the
/// edge may cache. The HTTP/2 backbone clients share this so they cannot
/// disagree with the HTTP/1.1 reader or with each other.
pub fn apply_response_block(
    pairs: Vec<(Vec<u8>, Vec<u8>)>,
    status: &mut u16,
    headers: &mut Vec<(String, String)>,
    informational: &mut usize,
) -> ResponseBlock {
    let block_status = pairs
        .iter()
        .find(|(k, _)| k == b":status")
        .and_then(|(_, v)| String::from_utf8_lossy(v).parse::<u16>().ok());
    if block_status.is_some_and(is_skipped_informational) {
        *informational += 1;
        return if *informational > MAX_INFORMATIONAL {
            ResponseBlock::TooManyInformational
        } else {
            ResponseBlock::Informational
        };
    }
    for (k, v) in pairs {
        if k == b":status" {
            *status = String::from_utf8_lossy(&v).parse().unwrap_or(0);
        } else {
            headers.push((
                String::from_utf8_lossy(&k).into_owned(),
                String::from_utf8_lossy(&v).into_owned(),
            ));
        }
    }
    ResponseBlock::Final
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

    Ok(HttpRequest {
        method,
        path,
        query,
        version,
        headers: parsed_headers,
        body,
    })
}

/// Which backend kind a pending request is waiting on. The event loop finishes
/// a socket reply with the socket branch's own semantics and a URL reply with
/// the URL branch's, so the two never share a completion path.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PendingKind {
    Url,
    Socket,
}

/// Which upstream a pending request was dispatched to, so the loop can store
/// who waits in that upstream's own slot and complete the waiter directly when
/// the upstream finishes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum UpstreamId {
    /// An upstream in the shared table, by its slot key. A socket-backend
    /// request and a URL-backend request both live there.
    Slot(usize),
    /// An h2c pool stream, by the connection token and the outbound stream id.
    H2c { conn: Token, stream: u32 },
    /// An h2s pool stream, by the connection token and the outbound stream id.
    H2s { conn: Token, stream: u32 },
}

/// Who is waiting for an upstream reply, so a completion can be written
/// straight into the waiting stream in the same turn the upstream finished.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Waiter {
    H1 {
        conn: Token,
        /// The connection's generation, so a reply that lands after the client
        /// was reaped and its token reused is dropped rather than misdelivered.
        gen: u64,
    },
    H2 {
        conn: Token,
        stream: u32,
        gen: u64,
    },
    H3 {
        conn_id: Vec<u8>,
        stream: u64,
    },
    /// A background fetch has no connection; the reply only fills the cache.
    Background,
}

/// One request the loop dispatched and parked, reported by a connection turn so
/// the loop's owner can store the waiter in the upstream's slot.
pub struct PendingDispatch {
    /// The server-side stream id, only for an HTTP/2 request.
    pub stream: Option<u32>,
    /// The upstream the request was dispatched to.
    pub id: UpstreamId,
    /// The request context the completion path finalizes with.
    pub ctx: Box<PendingUrlContext>,
}

/// Context carried alongside a pending backend request so the event loop can
/// finish processing (cache insertion, hints, error-mode) when the reply
/// arrives, whether the upstream is a socket member or a URL backend. All
/// fields are cheaply cloneable.
#[derive(Clone)]
pub struct PendingUrlContext {
    pub req: HttpRequest,
    pub client_ip: String,
    pub enc: String,
    pub backend_name: String,
    /// Which backend kind this pending request is waiting on.
    pub kind: PendingKind,
    /// The matched route pattern, carried so the socket completion can build
    /// the error context `apply_error_mode` expects. `None` for URL backends,
    /// whose completion already builds `route: None`.
    pub route_path: Option<String>,
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
    pub start: std::time::Instant, // request arrival time for miss timing
    /// Set when this dispatch is itself a `[errors] mode = "custom"` fetch of
    /// the error page (rather than a normal routed request) — carries the
    /// ORIGINAL failing status so the fetched body can be returned under it.
    /// `None` for every ordinary request dispatch.
    pub error_status_override: Option<u16>,
    /// Response headers to add when the fetched error page is served, such as
    /// `WWW-Authenticate` for a 401 or `Retry-After` for a 503. Empty for every
    /// ordinary dispatch.
    pub extra_headers: Vec<(String, String)>,
}

/// Build a simple HTTP response buffer (kept for tests).
pub fn build_response(
    status: u16,
    reason: &str,
    headers: &[(String, String)],
    body: &[u8],
) -> Vec<u8> {
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
#[allow(
    clippy::disallowed_methods,
    reason = "test code: threads, sleeps and blocking clients drive the service from outside"
)]
mod tests {
    use super::*;

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

    // ── The forwarded request bytes ───────────────────────────────────────────

    fn upstream_req() -> HttpRequest {
        HttpRequest {
            method: "GET".to_string(),
            path: "/api/test".to_string(),
            query: Some("key=value".to_string()),
            version: "HTTP/3".to_string(),
            headers: vec![
                ("accept".to_string(), "application/json".to_string()),
                ("connection".to_string(), "keep-alive".to_string()),
            ],
            body: vec![],
        }
    }

    #[test]
    fn a_forwarded_request_carries_the_proxy_headers_and_no_hop_by_hop_field() {
        let req = upstream_req();
        let mut bytes = Vec::new();
        encode_request(&mut bytes, &req, "1.2.3.4", "original.host").expect("encode");
        let s = std::str::from_utf8(&bytes).unwrap();

        assert!(s.starts_with("GET /api/test?key=value HTTP/1.1\r\n"));
        assert!(s.contains("Host: original.host\r\n"));
        assert!(s.contains("accept: application/json\r\n"));
        // hop-by-hop 'connection' must not appear
        assert!(!s.contains("connection: keep-alive"));
        assert!(s.contains("X-Forwarded-For: 1.2.3.4\r\n"));
        assert!(s.contains("X-Forwarded-Host: original.host\r\n"));
        assert!(s.contains("Connection: close\r\n"));
    }

    #[test]
    fn a_url_backend_request_names_the_backend_and_asks_to_keep_the_connection() {
        let req = upstream_req();
        let mut bytes = Vec::new();
        encode_url_request(
            &mut bytes,
            &req,
            "api.example.com",
            "1.2.3.4",
            "original.host",
        )
        .expect("encode");
        let s = std::str::from_utf8(&bytes).unwrap();

        assert!(
            s.lines().any(|l| l == "Host: api.example.com"),
            "the backend's authority is the Host: {s}"
        );
        assert!(
            !s.lines().any(|l| l == "Host: original.host"),
            "the client's Host is replaced, not repeated: {s}"
        );
        assert!(s.contains("Connection: keep-alive\r\n"), "{s}");
        assert!(!s.contains("Connection: close\r\n"), "{s}");
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
            "evil\nX-Injected: bare-lf", // bare LF: many parsers accept it
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
        // A tab or DEL cannot be written into a request line either; the
        // forwarder and HTTP/2 ingress share one rule for it.
        for bad in ["/a\tb", "/a\u{7f}b"] {
            let mut t = r.clone();
            t.path = bad.to_string();
            assert!(
                check_forwardable(&t, "1.2.3.4", "example.com").is_err(),
                "{bad:?}"
            );
        }
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
                (
                    "Content-Type".to_string(),
                    "application/x-www-form-urlencoded".to_string(),
                ),
                (
                    "User-Agent".to_string(),
                    "Mozilla/5.0 (Macintosh)".to_string(),
                ),
                (
                    "X-Obs-Text".to_string(),
                    "caf\u{e9} \u{2014} fine".to_string(),
                ),
                ("Accept".to_string(), "text/html;q=0.9, */*".to_string()),
            ],
            body: b"a=1".to_vec(),
        };
        assert_eq!(check_forwardable(&r, "203.0.113.7", "example.com"), Ok(()));
    }
}

#[cfg(test)]
mod via_and_connection_tests {
    use super::*;

    fn req(headers: &[(&str, &str)], version: &str) -> HttpRequest {
        HttpRequest {
            method: "GET".to_string(),
            path: "/".to_string(),
            query: None,
            version: version.to_string(),
            headers: headers
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            body: Vec::new(),
        }
    }

    fn serialised(r: &HttpRequest) -> String {
        let mut bytes = Vec::new();
        encode_url_request(&mut bytes, r, "backend.internal", "1.2.3.4", "example.com")
            .expect("encode");
        String::from_utf8_lossy(&bytes).into_owned()
    }

    fn header_values(raw: &str, name: &str) -> Vec<String> {
        raw.split("\r\n\r\n")
            .next()
            .unwrap_or("")
            .lines()
            .filter(|l| {
                l.to_lowercase()
                    .starts_with(&format!("{}:", name.to_lowercase()))
            })
            .map(|l| l.split_once(':').unwrap().1.trim().to_string())
            .collect()
    }

    /// F018/F019. `Connection` names the fields that apply to this hop only.
    /// Forwarding one on lets a client smuggle a header past an intermediary
    /// that believed it had been consumed.
    #[test]
    fn connection_nominated_fields_are_stripped() {
        let r = req(
            &[
                ("Connection", "X-Session-Hint, X-Internal-Flag"),
                ("X-Session-Hint", "smuggled"),
                ("X-Internal-Flag", "also-smuggled"),
                ("X-Legitimate", "kept"),
            ],
            "HTTP/1.1",
        );
        let out = serialised(&r);
        assert!(
            header_values(&out, "x-session-hint").is_empty(),
            "nominated field forwarded:\n{out}"
        );
        assert!(
            header_values(&out, "x-internal-flag").is_empty(),
            "nominated field forwarded:\n{out}"
        );
        assert_eq!(
            header_values(&out, "x-legitimate"),
            vec!["kept"],
            "unrelated header dropped:\n{out}"
        );
    }

    /// `close`/`keep-alive`/`upgrade` are connection OPTIONS, not field names.
    /// Treating them as fields would be wrong, and a header legitimately called
    /// `Close` must survive.
    #[test]
    fn connection_options_are_not_treated_as_field_names() {
        let r = req(
            &[("Connection", "keep-alive, close"), ("X-Keep", "v")],
            "HTTP/1.1",
        );
        let out = serialised(&r);
        assert_eq!(header_values(&out, "x-keep"), vec!["v"]);
    }

    /// F017. A proxy must announce itself so a loop is detectable.
    #[test]
    fn via_is_appended() {
        let out = serialised(&req(&[], "HTTP/1.1"));
        assert_eq!(header_values(&out, "via"), vec!["1.1 m6"], "\n{out}");
    }

    /// Via describes the protocol the request was RECEIVED on, not the one it
    /// is being forwarded over -- h2/h3 requests all leave here as HTTP/1.1.
    #[test]
    fn via_reports_the_received_protocol() {
        assert_eq!(
            header_values(&serialised(&req(&[], "HTTP/2")), "via"),
            vec!["2 m6"]
        );
        assert_eq!(
            header_values(&serialised(&req(&[], "HTTP/3")), "via"),
            vec!["3 m6"]
        );
        assert_eq!(
            header_values(&serialised(&req(&[], "HTTP/1.0")), "via"),
            vec!["1.0 m6"]
        );
    }

    /// An upstream proxy's Via must be preserved and appended to, not replaced
    /// -- the whole point is the chain.
    #[test]
    fn existing_via_is_extended_not_replaced() {
        let out = serialised(&req(&[("Via", "1.1 upstream-cache")], "HTTP/1.1"));
        assert_eq!(
            header_values(&out, "via"),
            vec!["1.1 upstream-cache, 1.1 m6"],
            "\n{out}"
        );
    }

    /// Exactly one Via, whatever the input. Two would be legal to merge but
    /// suggests the header is being appended blindly.
    #[test]
    fn exactly_one_via_header() {
        let out = serialised(&req(&[("Via", "1.1 a")], "HTTP/1.1"));
        assert_eq!(header_values(&out, "via").len(), 1);
    }

    /// The pseudonym must not leak the node's real hostname.
    #[test]
    fn via_does_not_disclose_internal_topology() {
        let out = serialised(&req(&[], "HTTP/1.1"));
        let via = header_values(&out, "via").join(" ");
        for leak in ["node-a", "node-b", "node-c", "example", "backend.internal"] {
            assert!(!via.contains(leak), "Via leaks {leak:?}: {via}");
        }
    }
}

#[cfg(test)]
mod forwarded_trust_tests {
    use super::*;

    /// The public listener must never believe a forwarded address, whatever it
    /// looks like. This is the rate-limit bypass: rotate the value, never get
    /// throttled.
    #[test]
    fn a_public_listener_always_attributes_to_the_peer() {
        for forged in ["203.0.113.9", "", "not-an-ip", "1.1.1.1, 2.2.2.2"] {
            assert_eq!(
                attributed_client_ip(Some(forged), "198.51.100.7", ForwardedTrust::Never),
                "198.51.100.7",
                "forged {forged:?} was believed on a public listener"
            );
        }
    }

    /// On the backbone the forwarded address is the accurate one, and is the
    /// whole point of the change: without it every request relayed by one
    /// cache node is attributed to that node.
    #[test]
    fn the_backbone_attributes_to_the_forwarded_address() {
        assert_eq!(
            attributed_client_ip(Some("203.0.113.9"), "192.0.2.4", ForwardedTrust::Backbone),
            "203.0.113.9"
        );
        assert_eq!(
            attributed_client_ip(Some(" 203.0.113.9 "), "192.0.2.4", ForwardedTrust::Backbone),
            "203.0.113.9",
            "surrounding whitespace is field syntax, not part of the address"
        );
        assert_eq!(
            attributed_client_ip(Some("2001:db8::1"), "192.0.2.4", ForwardedTrust::Backbone),
            "2001:db8::1"
        );
    }

    /// A request that arrives on the backbone without one is attributed to the
    /// peer, not dropped: origin is directly reachable too, and its own
    /// monitoring calls arrive with no forwarded address at all.
    #[test]
    fn a_missing_forwarded_address_falls_back_to_the_peer() {
        assert_eq!(
            attributed_client_ip(None, "192.0.2.4", ForwardedTrust::Backbone),
            "192.0.2.4"
        );
    }

    /// Anything that is not exactly one well-formed address did not come from
    /// the path this trusts.
    ///
    /// A list is the interesting case. The general reading of
    /// `X-Forwarded-For` takes the leftmost entry as the original client, and
    /// doing that here would let a cache node's peer prepend whatever it liked
    /// and have origin believe it. A cache node emits exactly one value, so a
    /// list means something other than the expected topology produced it.
    #[test]
    fn anything_but_a_single_address_is_refused() {
        for bad in [
            "203.0.113.9, 192.0.2.4",
            "203.0.113.9,192.0.2.4",
            "",
            "   ",
            "localhost",
            "203.0.113.9:443",
            "<script>",
            "203.0.113.999",
        ] {
            assert_eq!(sole_forwarded_ip(bad), None, "{bad:?} should be refused");
            assert_eq!(
                attributed_client_ip(Some(bad), "192.0.2.4", ForwardedTrust::Backbone),
                "192.0.2.4",
                "{bad:?} was believed on the backbone"
            );
        }
    }

    /// The value becomes a rate-limiter map key, so it must come from a
    /// bounded set. Parsing as an address is what bounds it: a peer cannot
    /// grow the limiter's map by varying a free-form string.
    #[test]
    fn the_attributed_address_is_always_a_parseable_address_or_the_peer() {
        let huge = "a".repeat(10_000);
        let out = attributed_client_ip(Some(&huge), "192.0.2.4", ForwardedTrust::Backbone);
        assert_eq!(out, "192.0.2.4");
    }

    /// `x-forwarded-for` stays in the set stripped from every inbound request
    /// on every protocol. Trust decides whether a *derived address* is
    /// believed, never whether the header survives.
    #[test]
    fn the_header_itself_is_still_untrusted_inbound() {
        assert!(is_untrusted_inbound("x-forwarded-for"));
        assert!(is_untrusted_inbound("X-Forwarded-For"));
        let mut headers = vec![
            ("X-Forwarded-For".to_string(), "203.0.113.9".to_string()),
            ("Accept".to_string(), "*/*".to_string()),
        ];
        strip_untrusted_inbound(&mut headers);
        assert_eq!(headers, vec![("Accept".to_string(), "*/*".to_string())]);
    }
}
