/// Non-blocking HTTP/1.1 and HTTP/2 over TLS (rustls) integrated into the epoll loop.
///
/// Design:
/// - `TcpListener` registered with TOKEN_TCP.
/// - `accept_pending()` drains new connections, eagerly advances the TLS handshake,
///   and registers each fd with the poller.
/// - After the TLS handshake, the negotiated ALPN protocol determines the handler:
///     "h2"       → Http2Conn  (multiplexed streams, full H2 framing)
///     "http/1.1" → H1 state machine (Handshake→Reading→Writing, Connection: close)
/// - `drive_all()` is called after every epoll wakeup (for any TOKEN_TCP event)
///   and drives every active connection one step forward.

use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::io::{AsRawFd, RawFd};
use std::sync::Arc;
use std::time::Instant;

use rustls::ServerConnection;
use tracing::warn;

use crate::forward::{HttpRequest, HttpResponse, PendingUrlContext};
use crate::http2::{Http2Conn, H2Io};
use crate::poller::{Poller, Token};

// ── Request outcome ───────────────────────────────────────────────────────────

/// The result of dispatching a request to a backend.
pub enum RequestOutcome {
    /// Response is available immediately (cache hit, socket backend, auth error, etc.)
    Ready(u16, Vec<(String, String)>, Vec<u8>, String, std::sync::Arc<Vec<String>>),
    /// URL backend I/O dispatched to a thread; poll `rx` with `try_recv()`.
    Pending {
        rx:  std::sync::mpsc::Receiver<std::io::Result<HttpResponse>>,
        ctx: PendingUrlContext,
    },
}

// ── Per-connection state ──────────────────────────────────────────────────────

enum ConnKind {
    /// TLS handshake in progress; protocol not yet known.
    Handshake { client_ip: String, created: Instant },
    /// HTTP/1.1 after handshake.
    Http1(H1Conn),
    /// HTTP/2 after handshake.
    Http2(Http2Conn),
}

struct Conn {
    stream: TcpStream,
    tls:    ServerConnection,
    kind:   ConnKind,
}

impl Conn {
    fn is_done(&self) -> bool {
        match &self.kind {
            ConnKind::Http1(c) => matches!(c.state, H1State::Done),
            ConnKind::Http2(c) => c.is_done(),
            ConnKind::Handshake { .. } => false,
        }
    }
}

// ── HTTP/1.1 state machine ────────────────────────────────────────────────────

enum H1State {
    Reading { buf: Vec<u8> },
    WaitingBackend {
        rx:  std::sync::mpsc::Receiver<std::io::Result<HttpResponse>>,
        ctx: PendingUrlContext,
    },
    Writing { buf: Vec<u8>, pos: usize },
    Done,
}

struct H1Conn {
    state:     H1State,
    client_ip: String,
    created:   Instant,
}

/// Per-request idle timeout for HTTP/1.1 (single request per connection).
pub(crate) const READ_TIMEOUT_SECS: u64 = 30;
/// Idle timeout for HTTP/2 connections (reused across many requests).
pub(crate) const H2_IDLE_TIMEOUT_SECS: u64 = 300;
/// 20 MiB — above m6-render's own 16 MiB multipart body cap, so oversized
/// uploads get a clean rejection from the backend (which has read full,
/// valid HTTP framing) rather than a mid-stream connection drop here.
const MAX_REQUEST_BYTES: usize = 20 * 1024 * 1024;

// ── Public API ────────────────────────────────────────────────────────────────

pub struct Http11Listener {
    listener:   TcpListener,
    tls_config: Arc<rustls::ServerConfig>,
    conns:      Vec<Conn>,
}

impl Http11Listener {
    pub fn bind(addr: &str, tls_config: Arc<rustls::ServerConfig>) -> anyhow::Result<Self> {
        let listener = TcpListener::bind(addr)?;
        listener.set_nonblocking(true)?;
        Ok(Http11Listener { listener, tls_config, conns: Vec::new() })
    }

    pub fn raw_fd(&self) -> RawFd { self.listener.as_raw_fd() }

    pub fn local_addr(&self) -> std::io::Result<std::net::SocketAddr> {
        self.listener.local_addr()
    }

    /// Drain pending `accept()` calls; register each new fd with the poller.
    /// Eagerly starts the TLS handshake so the ServerHello is sent immediately.
    pub fn accept_pending(&mut self, poller: &Poller, token: Token) {
        loop {
            match self.listener.accept() {
                Ok((stream, peer)) => {
                    if let Err(e) = stream.set_nonblocking(true) {
                        warn!("tcp set_nonblocking: {e}");
                        continue;
                    }
                    stream.set_nodelay(true).ok();
                    let mut tls = match ServerConnection::new(Arc::clone(&self.tls_config)) {
                        Ok(t) => t,
                        Err(e) => { warn!("tls ServerConnection::new: {e}"); continue; }
                    };
                    // Raises rustls' *outgoing* buffering caps (sendable_plaintext /
                    // sendable_tls) to match MAX_REQUEST_BYTES, so a large response
                    // written before the peer is ready to receive it doesn't get
                    // truncated. The incoming side (received_plaintext, where large
                    // request bodies land) is a separate, fixed 16 KiB buffer with no
                    // public setter — see the comment on advance_tls's "buffer full"
                    // handling for how that's dealt with instead.
                    tls.set_buffer_limit(Some(MAX_REQUEST_BYTES));
                    poller.add(stream.as_raw_fd(), token).ok();
                    // Eagerly start handshake: ClientHello is already buffered on loopback.
                    let _ = advance_tls(&mut tls, &stream);
                    let kind = ConnKind::Handshake {
                        client_ip: peer.ip().to_string(),
                        created:   Instant::now(),
                    };
                    self.conns.push(Conn { stream, tls, kind });
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => { warn!("tcp accept: {e}"); break; }
            }
        }
    }

    /// Drive all active connections. Done connections are deregistered and dropped.
    pub fn drive_all<F, G>(&mut self, mut on_request: F, mut on_response: G, poller: &Poller)
    where
        F: FnMut(&HttpRequest, &str) -> RequestOutcome,
        G: FnMut(std::io::Result<HttpResponse>, &PendingUrlContext)
               -> (u16, Vec<(String, String)>, Vec<u8>, String, std::sync::Arc<Vec<String>>),
    {
        for conn in &mut self.conns {
            drive_conn(conn, &mut on_request, &mut on_response);
        }
        for conn in &self.conns {
            if conn.is_done() {
                poller.delete(conn.stream.as_raw_fd()).ok();
            }
        }
        self.conns.retain(|c| !c.is_done());
    }
}

// ── H2C (HTTP/2 cleartext) listener ──────────────────────────────────────────

struct H2cPlainConn {
    stream:    TcpStream,
    h2:        Http2Conn,
    client_ip: String,
}

pub struct H2cListener {
    listener: TcpListener,
    conns:    Vec<H2cPlainConn>,
}

impl H2cListener {
    pub fn bind(addr: &str) -> anyhow::Result<Self> {
        let listener = TcpListener::bind(addr)?;
        listener.set_nonblocking(true)?;
        Ok(H2cListener { listener, conns: Vec::new() })
    }

    pub fn raw_fd(&self) -> RawFd { self.listener.as_raw_fd() }

    pub fn local_addr(&self) -> std::io::Result<std::net::SocketAddr> {
        self.listener.local_addr()
    }

    pub fn accept_pending(&mut self, poller: &Poller, token: Token) {
        loop {
            match self.listener.accept() {
                Ok((stream, peer)) => {
                    if let Err(e) = stream.set_nonblocking(true) {
                        warn!("h2c set_nonblocking: {e}");
                        continue;
                    }
                    stream.set_nodelay(true).ok();
                    poller.add(stream.as_raw_fd(), token).ok();
                    self.conns.push(H2cPlainConn {
                        stream,
                        h2: Http2Conn::new(),
                        client_ip: peer.ip().to_string(),
                    });
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => { warn!("h2c accept: {e}"); break; }
            }
        }
    }

    pub fn drive_all<F, G>(&mut self, mut on_request: F, mut on_response: G, poller: &Poller)
    where
        F: FnMut(&HttpRequest, &str) -> RequestOutcome,
        G: FnMut(std::io::Result<HttpResponse>, &PendingUrlContext)
               -> (u16, Vec<(String, String)>, Vec<u8>, String, std::sync::Arc<Vec<String>>),
    {
        for conn in &mut self.conns {
            conn.h2.drive(
                H2Io::Plain { stream: &conn.stream },
                &conn.client_ip,
                &mut on_request,
                &mut on_response,
            );
        }
        for conn in &self.conns {
            if conn.h2.is_done() {
                poller.delete(conn.stream.as_raw_fd()).ok();
            }
        }
        self.conns.retain(|c| !c.h2.is_done());
    }
}

// ── Per-connection driver ─────────────────────────────────────────────────────

fn drive_conn<F, G>(conn: &mut Conn, on_request: &mut F, on_response: &mut G)
where
    F: FnMut(&HttpRequest, &str) -> RequestOutcome,
    G: FnMut(std::io::Result<HttpResponse>, &PendingUrlContext)
           -> (u16, Vec<(String, String)>, Vec<u8>, String, std::sync::Arc<Vec<String>>),
{
    // HTTP/2: stream and tls are in conn; pass them by reference.
    if let ConnKind::Http2(h2) = &mut conn.kind {
        let client_ip = conn.stream.peer_addr()
            .map(|a| a.ip().to_string())
            .unwrap_or_default();
        h2.drive(
            H2Io::Tls { tls: &mut conn.tls, stream: &conn.stream },
            &client_ip,
            on_request, on_response,
        );
        return;
    }

    // Pump TLS I/O for H1 / still-handshaking connections.
    if advance_tls(&mut conn.tls, &conn.stream).is_err() {
        conn.kind = ConnKind::Http1(H1Conn {
            state:     H1State::Done,
            client_ip: String::new(),
            created:   Instant::now(),
        });
        return;
    }

    // If still handshaking, nothing more to do this tick.
    if conn.tls.is_handshaking() {
        return;
    }

    // Handshake just completed — dispatch on ALPN.
    if let ConnKind::Handshake { client_ip, created } = &conn.kind {
        let proto     = conn.tls.alpn_protocol().map(|p| p.to_vec());
        let client_ip = client_ip.clone();
        let created   = *created;
        if proto.as_deref() == Some(b"h2") {
            conn.kind = ConnKind::Http2(Http2Conn::new());
            // Drive immediately — client preface may already be buffered.
            let ConnKind::Http2(h2) = &mut conn.kind else { return };
            h2.drive(
                H2Io::Tls { tls: &mut conn.tls, stream: &conn.stream },
                &client_ip,
                on_request, on_response,
            );
            return;
        } else {
            conn.kind = ConnKind::Http1(H1Conn {
                state:     H1State::Reading { buf: Vec::new() },
                client_ip,
                created,
            });
        }
    }

    // Drive HTTP/1.1 state machine.
    let ConnKind::Http1(h1) = &mut conn.kind else { return };
    drive_h1(&mut conn.tls, &conn.stream, h1, on_request, on_response);
}

fn drive_h1<F, G>(
    tls:         &mut ServerConnection,
    stream:      &TcpStream,
    h1:          &mut H1Conn,
    on_request:  &mut F,
    on_response: &mut G,
)
where
    F: FnMut(&HttpRequest, &str) -> RequestOutcome,
    G: FnMut(std::io::Result<HttpResponse>, &PendingUrlContext)
           -> (u16, Vec<(String, String)>, Vec<u8>, String, std::sync::Arc<Vec<String>>),
{
    if h1.created.elapsed().as_secs() > READ_TIMEOUT_SECS {
        h1.state = H1State::Done;
        return;
    }
    loop {
        match &h1.state {
            H1State::Reading { .. } => {
                let mut tmp = [0u8; 4096];
                let n = match tls.reader().read(&mut tmp) {
                    // rustls' plaintext reader can legitimately yield Ok(0) mid-stream
                    // when a processed TLS record carries no application data (e.g. a
                    // TLS 1.3 post-handshake NewSessionTicket) — this does NOT mean the
                    // peer closed the connection. Genuine closure is already detected
                    // one layer up in advance_tls (Err on raw socket EOF), so treat this
                    // the same as WouldBlock: no plaintext ready this round, keep waiting.
                    Ok(0)  => break,
                    Ok(n)  => n,
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                    Err(_) => { h1.state = H1State::Done; return; }
                };
                let H1State::Reading { buf } = &mut h1.state else { break };
                buf.extend_from_slice(&tmp[..n]);
                if buf.len() > MAX_REQUEST_BYTES {
                    let resp = b"HTTP/1.1 413 Payload Too Large\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                    h1.state = H1State::Writing { buf: resp.to_vec(), pos: 0 };
                    continue;
                }
                // Parse under an immutable borrow. `ParseResult` owns all of
                // its data, so the borrow ends with this statement and
                // `h1.state` can be reassigned below.
                //
                // This used to clone `buf` to release the mutable borrow
                // above. That copied the whole accumulated request on *every*
                // read event, so a body arriving in N chunks was copied O(N²)
                // bytes in total — at the 20 MiB cap, tens of GB of memcpy for
                // a single upload.
                let parsed = match &h1.state {
                    H1State::Reading { buf } => parse_request(buf),
                    _ => break,
                };
                match parsed {
                    ParseResult::Incomplete => break,
                    ParseResult::Error => {
                        let resp = b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                        h1.state = H1State::Writing { buf: resp.to_vec(), pos: 0 };
                        continue;
                    }
                    ParseResult::Complete(req) => {
                        match on_request(&req, &h1.client_ip) {
                            RequestOutcome::Ready(status, resp_headers, body, _, hints) => {
                                let mut buf = Vec::new();
                                if !hints.is_empty() {
                                    buf.extend_from_slice(b"HTTP/1.1 103 Early Hints\r\n");
                                    for url in hints.iter() {
                                        let lh = crate::hints::link_header(url);
                                        buf.extend_from_slice(b"link: ");
                                        buf.extend_from_slice(lh.as_bytes());
                                        buf.extend_from_slice(b"\r\n");
                                    }
                                    buf.extend_from_slice(b"\r\n");
                                }
                                buf.extend_from_slice(&build_response(status, &resp_headers, &body, &req.method));
                                h1.state = H1State::Writing { buf, pos: 0 };
                                continue;
                            }
                            RequestOutcome::Pending { rx, ctx } => {
                                h1.state = H1State::WaitingBackend { rx, ctx };
                                break; // nothing more to do; poll next iteration
                            }
                        }
                    }
                }
            }
            H1State::WaitingBackend { .. } => {
                use std::sync::mpsc::TryRecvError;
                // Move state out so we can destructure and replace.
                let old = std::mem::replace(&mut h1.state, H1State::Done);
                let H1State::WaitingBackend { rx, ctx } = old else { break };
                let http_result = match rx.try_recv() {
                    Ok(r)  => r,
                    Err(TryRecvError::Empty) => {
                        h1.state = H1State::WaitingBackend { rx, ctx }; // put back
                        break;
                    }
                    Err(TryRecvError::Disconnected) => Err(io::Error::new(
                        io::ErrorKind::BrokenPipe, "url backend thread died",
                    )),
                };
                let (status, resp_headers, body, _, hints) = on_response(http_result, &ctx);
                let mut buf = Vec::new();
                if !hints.is_empty() {
                    buf.extend_from_slice(b"HTTP/1.1 103 Early Hints\r\n");
                    for url in hints.iter() {
                        let lh = crate::hints::link_header(url);
                        buf.extend_from_slice(b"link: ");
                        buf.extend_from_slice(lh.as_bytes());
                        buf.extend_from_slice(b"\r\n");
                    }
                    buf.extend_from_slice(b"\r\n");
                }
                buf.extend_from_slice(&build_response(status, &resp_headers, &body, &ctx.req.method));
                h1.state = H1State::Writing { buf, pos: 0 };
                // pump TLS to start sending immediately
                if advance_tls(tls, stream).is_err() { h1.state = H1State::Done; return; }
                continue; // fall through to Writing
            }
            H1State::Writing { buf, pos } => {
                let remaining = &buf[*pos..];
                if remaining.is_empty() { h1.state = H1State::Done; return; }
                match tls.writer().write(remaining) {
                    Ok(0)      => { h1.state = H1State::Done; return; }
                    Ok(w)      => {
                        let H1State::Writing { pos, .. } = &mut h1.state else { break };
                        *pos += w;
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                    Err(_) => { h1.state = H1State::Done; return; }
                }
                if advance_tls(tls, stream).is_err() { h1.state = H1State::Done; return; }
                let H1State::Writing { buf, pos } = &h1.state else { break };
                if *pos >= buf.len() { h1.state = H1State::Done; return; }
                break;
            }
            H1State::Done => return,
        }
    }
}

/// Pump encrypted bytes between socket and rustls.
fn advance_tls(tls: &mut ServerConnection, stream: &TcpStream) -> io::Result<()> {
    loop {
        match tls.read_tls(&mut &*stream) {
            Ok(0)  => return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "closed")),
            Ok(_)  => { tls.process_new_packets().map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?; }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
            // rustls caps its incoming-plaintext buffer at a fixed 16 KiB
            // (not application-configurable — set_buffer_limit only covers
            // the *outgoing* buffers) as backpressure: read_tls refuses to
            // pull more ciphertext until the caller drains already-decoded
            // plaintext via reader(). For any body over ~16 KiB this trips
            // on every advance_tls call. It isn't a real error — stop
            // pumping ciphertext for this round exactly like WouldBlock;
            // drive_h1 drains the reader right after we return, and the
            // next poller wakeup (level-triggered — more data is still
            // sitting in the kernel socket buffer) resumes pumping.
            Err(e) if e.kind() == io::ErrorKind::Other
                && e.to_string().contains("received plaintext buffer full") => break,
            Err(e) => return Err(e),
        }
    }
    loop {
        match tls.write_tls(&mut &*stream) {
            Ok(0)  => break,
            Ok(_)  => {}
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

// ── HTTP/1.1 request parser ───────────────────────────────────────────────────

/// Outcome of parsing a client request. Public so security tests can assert on
/// what the ingress boundary accepts, rejects, and strips.
pub enum ParseResult {
    Incomplete,
    Error,
    Complete(HttpRequest),
}

pub fn parse_request(buf: &[u8]) -> ParseResult {
    let mut headers = [httparse::EMPTY_HEADER; 64];
    let mut req = httparse::Request::new(&mut headers);
    let body_offset = match req.parse(buf) {
        Ok(httparse::Status::Partial) => return ParseResult::Incomplete,
        Ok(httparse::Status::Complete(n)) => n,
        Err(_) => return ParseResult::Error,
    };

    let method = req.method.unwrap_or("GET").to_string();
    let raw_path = req.path.unwrap_or("/");

    let (path, query) = match raw_path.find('?') {
        Some(q) => (raw_path[..q].to_string(), Some(raw_path[q + 1..].to_string())),
        None => (raw_path.to_string(), None),
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
    let mut fwd_headers: Vec<(String, String)> = Vec::with_capacity(nheaders);

    for h in &req.headers[..nheaders] {
        if h.name.eq_ignore_ascii_case("transfer-encoding") {
            // Chunked bodies are not decoded here; accepting one would mean
            // forwarding a body we never read.
            return ParseResult::Error;
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

        // Strip proxy-owned headers on ingress — see
        // `forward::UNTRUSTED_INBOUND`.
        if crate::forward::is_untrusted_inbound(h.name) {
            continue;
        }
        // A header value that is not UTF-8 is dropped rather than rejected
        // (matching prior behaviour); `content-length` is the exception
        // handled above, where it is a framing error.
        let Ok(v) = std::str::from_utf8(h.value) else { continue };
        fwd_headers.push((h.name.to_string(), v.to_string()));
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

    let available = buf.len() - body_offset;
    if available < content_length {
        return ParseResult::Incomplete;
    }

    let body = buf[body_offset..body_offset + content_length].to_vec();

    ParseResult::Complete(HttpRequest {
        method,
        path,
        query,
        version: "HTTP/1.1".to_string(),
        headers: fwd_headers,
        body,
    })
}

// ── Response serialiser ───────────────────────────────────────────────────────

/// Serialise a response. `method` is taken so HEAD can be framed correctly.
///
/// RFC 9110 9.3.2: a HEAD response carries the header fields a GET would --
/// `Content-Length` included, describing the representation that GET *would*
/// have returned -- and no body at all.
///
/// m6-http used to send the full body in response to a HEAD while advertising
/// that same length, so the response was not merely over-sized, it was
/// malformed: curl reported "transfer closed with N bytes remaining" (exit 18)
/// and HTTP/2 aborted the stream with INTERNAL_ERROR. `curl -I` hides all of
/// it, because it parses the response and discards the body -- every hand
/// check looked clean, and only a raw socket read showed the truth. Health
/// checks, link validators, crawlers and uptime monitors all use HEAD, so
/// every one of them was either transferring the whole page or erroring.
///
/// The length is computed from `body` before it is dropped, which is why the
/// caller passes the real body here rather than pre-emptying it.
fn build_response(status: u16, headers: &[(String, String)], body: &[u8], method: &str) -> Vec<u8> {
    let is_head = method.eq_ignore_ascii_case("HEAD");
    let reason = status_reason(status);
    let mut out = Vec::with_capacity(256 + if is_head { 0 } else { body.len() });
    out.extend_from_slice(
        format!("HTTP/1.1 {} {}\r\n", status, reason).as_bytes()
    );
    for (k, v) in headers {
        out.extend_from_slice(format!("{}: {}\r\n", k, v).as_bytes());
    }
    // Applied at serialisation so every response carries them regardless of
    // which path produced it (cache hit, backend, error page, 429).
    crate::security::write_h1_headers(&mut out, headers);
    out.extend_from_slice(format!("content-length: {}\r\n", body.len()).as_bytes());
    out.extend_from_slice(b"connection: close\r\n\r\n");
    if !is_head {
        out.extend_from_slice(body);
    }
    out
}

fn status_reason(status: u16) -> &'static str {
    match status {
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
        413 => "Payload Too Large",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        _ => "Unknown",
    }
}

// ── TLS server config factory ─────────────────────────────────────────────────

pub fn make_tls_server_config(
    cert_path: &str,
    key_path: &str,
) -> anyhow::Result<Arc<rustls::ServerConfig>> {
    use rustls_pemfile::{certs, private_key};
    use std::fs::File;
    use std::io::BufReader;

    let cert_file = File::open(cert_path)
        .map_err(|e| anyhow::anyhow!("open cert {}: {}", cert_path, e))?;
    let key_file = File::open(key_path)
        .map_err(|e| anyhow::anyhow!("open key {}: {}", key_path, e))?;

    let certs: Vec<_> = certs(&mut BufReader::new(cert_file))
        .collect::<Result<_, _>>()
        .map_err(|e| anyhow::anyhow!("parse cert: {}", e))?;

    let key = private_key(&mut BufReader::new(key_file))
        .map_err(|e| anyhow::anyhow!("parse key: {}", e))?
        .ok_or_else(|| anyhow::anyhow!("no private key found in {}", key_path))?;

    let mut config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| anyhow::anyhow!("tls config: {}", e))?;

    // Advertise h2 first so capable clients use HTTP/2; fall back to HTTP/1.1.
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];

    Ok(Arc::new(config))
}

#[cfg(test)]
mod head_framing_tests {
    use super::*;

    fn hdrs() -> Vec<(String, String)> {
        vec![("content-type".to_string(), "text/html; charset=utf-8".to_string())]
    }

    fn split(raw: &[u8]) -> (String, &[u8]) {
        let i = raw.windows(4).position(|w| w == b"\r\n\r\n").expect("header terminator");
        (String::from_utf8_lossy(&raw[..i]).to_string(), &raw[i + 4..])
    }

    const BODY: &[u8] = b"<!doctype html><html><body>hello</body></html>";

    #[test]
    fn get_sends_the_body() {
        let (head, body) = {
            let raw = build_response(200, &hdrs(), BODY, "GET");
            let (h, b) = split(&raw);
            (h, b.to_vec())
        };
        assert!(head.contains(&format!("content-length: {}", BODY.len())));
        assert_eq!(body, BODY);
    }

    /// The defect: HEAD advertised the GET representation's length and then
    /// sent that many body bytes too. curl reported "transfer closed with N
    /// bytes remaining" (exit 18); HTTP/2 aborted the stream. `curl -I` hides
    /// it because it parses and discards the body, so every hand check passed.
    #[test]
    fn head_sends_zero_body_bytes() {
        let raw = build_response(200, &hdrs(), BODY, "HEAD");
        let (_, body) = split(&raw);
        assert_eq!(body.len(), 0, "HEAD must send no body, got {} bytes", body.len());
    }

    /// RFC 9110 9.3.2: the headers are those a GET would have sent, so
    /// Content-Length still describes the GET representation. Deriving it from
    /// the emptied body instead would advertise 0 and make HEAD useless for
    /// the size checks that are most of the reason to send one.
    #[test]
    fn head_still_advertises_the_get_length() {
        let raw = build_response(200, &hdrs(), BODY, "HEAD");
        let (head, _) = split(&raw);
        assert!(head.contains(&format!("content-length: {}", BODY.len())),
                "expected content-length {}, headers were:\n{head}", BODY.len());
    }

    /// A HEAD response must otherwise be indistinguishable from the GET's
    /// header block — same status, same content-type, same everything.
    #[test]
    fn head_and_get_headers_match() {
        let (gh, _) = { let r = build_response(200, &hdrs(), BODY, "GET"); let (h, _) = split(&r); (h, ()) };
        let (hh, _) = { let r = build_response(200, &hdrs(), BODY, "HEAD"); let (h, _) = split(&r); (h, ()) };
        assert_eq!(gh, hh, "HEAD headers differ from GET headers");
    }

    #[test]
    fn method_match_is_case_insensitive() {
        let raw = build_response(200, &hdrs(), BODY, "head");
        let (_, body) = split(&raw);
        assert_eq!(body.len(), 0);
    }

    /// An empty-bodied response is unaffected either way.
    #[test]
    fn empty_body_is_unchanged() {
        for m in ["GET", "HEAD"] {
            let raw = build_response(204, &hdrs(), b"", m);
            let (head, body) = split(&raw);
            assert_eq!(body.len(), 0);
            assert!(head.contains("content-length: 0"));
        }
    }
}
