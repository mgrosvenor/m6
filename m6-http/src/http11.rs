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
    let mut host_count = 0usize;
    let mut host_empty = false;
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

        if h.name.eq_ignore_ascii_case("host") {
            host_count += 1;
            host_empty = std::str::from_utf8(h.value)
                .map(|v| v.trim().is_empty())
                .unwrap_or(true);
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
/// May a response with this status carry `Content-Length` at all?
///
/// RFC 9110 8.6. A 1xx or 204 MUST NOT have one, and a 304 MUST NOT unless the
/// value equals what the 200 would have sent.
///
/// m6 emitted `content-length: 0` on every 304, on all protocols. Zero is
/// precisely the harmful value: the 200 for that resource is 16 KB, so the
/// response was telling a client the representation is empty. A cache updating
/// its stored entry from that 304 can conclude the body it holds is the wrong
/// length.
///
/// The framing is unambiguous without it -- a 304 has no body by definition,
/// and every protocol here signals end-of-message its own way (H1 closes or
/// uses the next request boundary, H2/H3 use END_STREAM) -- so omitting it is
/// both correct and safe. Emitting the *correct* non-zero length would also be
/// legal, but it would mean carrying the stored body's length through the 304
/// path for no benefit to any client.
pub fn status_may_have_content_length(status: u16) -> bool {
    !(status == 204 || status == 304 || (100..200).contains(&status))
}

fn build_response(status: u16, headers: &[(String, String)], body: &[u8], method: &str) -> Vec<u8> {
    let is_head = method.eq_ignore_ascii_case("HEAD");
    let reason = status_reason(status);
    let mut out = Vec::with_capacity(256 + if is_head { 0 } else { body.len() });
    out.extend_from_slice(
        format!("HTTP/1.1 {} {}\r\n", status, reason).as_bytes()
    );
    // Any upstream copy of a header this function emits itself is dropped
    // here, or the response goes out carrying both.
    //
    // Found by reading a live HEAD off a raw socket after deploying the HEAD
    // work: an asset answered with `Content-Length: 16580` from the backend
    // AND `content-length: 0` from the line below. Two Content-Length fields
    // with different values is precisely the framing ambiguity this proxy now
    // refuses to accept *from* a backend (F028) -- it was emitting it. On HTML
    // the two agreed, so it looked like nothing more than a cosmetic duplicate;
    // only the asset showed the conflict.
    for (k, v) in headers {
        if k.eq_ignore_ascii_case("content-length") || k.eq_ignore_ascii_case("connection") {
            continue;
        }
        out.extend_from_slice(format!("{}: {}\r\n", k, v).as_bytes());
    }
    // Applied at serialisation so every response carries them regardless of
    // which path produced it (cache hit, backend, error page, 429).
    crate::security::write_h1_headers(&mut out, headers);

    // RFC 9110 9.3.2: a HEAD response's Content-Length describes the
    // representation a GET *would* have returned, not the zero bytes actually
    // sent. Most callers hand us the real body and `body.len()` is that
    // number -- but a backend that framed the HEAD itself returns an empty
    // body, and then only its own header still knows the real length.
    let cl = if is_head && body.is_empty() {
        headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
            .and_then(|(_, v)| v.trim().parse::<usize>().ok())
            .unwrap_or(0)
    } else {
        body.len()
    };
    if status_may_have_content_length(status) {
        out.extend_from_slice(format!("content-length: {}\r\n", cl).as_bytes());
    }
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
        501 => "Not Implemented",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        // 501 used to fall through to "Unknown", so the server answered
        // `HTTP/1.1 501 Unknown` -- a real status with a reason phrase that
        // described nothing. Reason phrases are advisory, but an incorrect one
        // is worse than a terse one.
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

    /// An empty-bodied response is unaffected by the HEAD logic either way.
    ///
    /// This used to assert `content-length: 0` on a **204**, which encoded a
    /// spec violation: RFC 9110 8.6 forbids Content-Length on a 204 entirely.
    /// The test passed for as long as the bug existed and failed the moment it
    /// was fixed -- a test can pin wrong behaviour just as firmly as right
    /// behaviour, and this one did.
    ///
    /// Split so each status asserts what its own rule requires: 200 carries the
    /// header, 204 must not.
    #[test]
    fn empty_body_is_unchanged() {
        for m in ["GET", "HEAD"] {
            let raw200 = build_response(200, &hdrs(), b"", m);
            let (head, body) = split(&raw200);
            assert_eq!(body.len(), 0);
            assert!(head.contains("content-length: 0"), "200 must state its length:\n{head}");

            let raw204 = build_response(204, &hdrs(), b"", m);
            let (head, body) = split(&raw204);
            assert_eq!(body.len(), 0);
            assert!(
                !head.to_lowercase().contains("content-length"),
                "204 must not carry Content-Length (RFC 9110 8.6):\n{head}"
            );
        }
    }
}

#[cfg(test)]
mod line_ending_tests {
    use super::*;

    fn headers_of(raw: &[u8]) -> Option<Vec<(String, String)>> {
        match parse_request(raw) {
            ParseResult::Complete(r) => Some(r.headers),
            _ => None,
        }
    }

    /// Baseline: a normal CRLF request parses as expected.
    #[test]
    fn crlf_headers_parse() {
        let h = headers_of(b"GET / HTTP/1.1\r\nHost: a\r\nX-One: 1\r\n\r\n").expect("parsed");
        assert!(h.iter().any(|(k, v)| k.eq_ignore_ascii_case("x-one") && v == "1"));
    }

    /// The question this file exists to answer: does a **bare LF** inside the
    /// header block terminate a header line?
    ///
    /// If it does, `X-Test: a\nX-Injected: yes` is two headers rather than one
    /// with a control character in its value, and the injected one is
    /// forwarded to the backend. That is header injection through a value the
    /// caller controls, and the fact that the *response* looks clean is
    /// precisely why it would go unnoticed.
    #[test]
    fn bare_lf_inside_a_header_value() {
        let parsed = headers_of(b"GET / HTTP/1.1\r\nHost: a\r\nX-Test: a\nX-Injected: yes\r\n\r\n");
        match parsed {
            None => { /* rejected outright — the strict, safe outcome */ }
            Some(h) => {
                let injected = h.iter().any(|(k, _)| k.eq_ignore_ascii_case("x-injected"));
                assert!(
                    !injected,
                    "a bare LF in a header value split it into a separate \
                     `X-Injected` header, which is then forwarded to the backend. \
                     Parsed headers: {h:?}"
                );
            }
        }
    }

    /// The same shape, one layer up: a bare LF terminating the request line.
    #[test]
    fn bare_lf_after_the_request_line() {
        let parsed = headers_of(b"GET / HTTP/1.1\nHost: a\nX-Injected: yes\n\n");
        if let Some(h) = parsed {
            let injected = h.iter().any(|(k, _)| k.eq_ignore_ascii_case("x-injected"));
            assert!(!injected, "LF-only request framing accepted headers: {h:?}");
        }
    }
}

#[cfg(test)]
mod response_header_tests {
    use super::build_response;

    fn header_lines(raw: &[u8], name: &str) -> Vec<String> {
        let text = String::from_utf8_lossy(raw);
        let head = text.split("\r\n\r\n").next().unwrap_or("").to_string();
        head.lines()
            .filter(|l| l.to_lowercase().starts_with(&format!("{}:", name.to_lowercase())))
            .map(|l| l.trim().to_string())
            .collect()
    }

    /// Found on the live site, not in review: an asset HEAD went out with
    /// `Content-Length: 16580` from the backend and `content-length: 0` from
    /// the serialiser. Two Content-Length fields with different values is the
    /// same framing ambiguity this proxy refuses to accept from a backend
    /// (F028) -- it was producing it.
    #[test]
    fn exactly_one_content_length_even_when_upstream_sent_one() {
        let upstream = vec![
            ("Content-Length".to_string(), "16580".to_string()),
            ("Content-Type".to_string(), "image/svg+xml".to_string()),
        ];
        for method in ["GET", "HEAD"] {
            let raw = build_response(200, &upstream, b"", method);
            let found = header_lines(&raw, "content-length");
            assert_eq!(
                found.len(),
                1,
                "{method} emitted {} Content-Length fields: {found:?}",
                found.len()
            );
        }
    }

    /// RFC 9110 9.3.2. A backend that frames the HEAD itself returns no body,
    /// and then only its own header still knows the GET representation's size.
    /// Reporting 0 there tells a crawler the resource is empty.
    #[test]
    fn head_reports_the_get_representation_length() {
        let upstream = vec![("Content-Length".to_string(), "16580".to_string())];
        let raw = build_response(200, &upstream, b"", "HEAD");
        assert_eq!(header_lines(&raw, "content-length"), vec!["content-length: 16580"]);
        // ...and still no body.
        let body = String::from_utf8_lossy(&raw).split("\r\n\r\n").nth(1).unwrap_or("").len();
        assert_eq!(body, 0, "HEAD must send no body");
    }

    /// When the caller passes the real body, that is authoritative -- it may
    /// have been compressed after the backend set its own length.
    #[test]
    fn body_length_wins_when_a_body_is_present() {
        let upstream = vec![("Content-Length".to_string(), "99999".to_string())];
        let raw = build_response(200, &upstream, b"hello", "GET");
        assert_eq!(header_lines(&raw, "content-length"), vec!["content-length: 5"]);
    }

    /// Same duplication hazard: `connection: close` is written unconditionally.
    #[test]
    fn exactly_one_connection_header() {
        let upstream = vec![("Connection".to_string(), "keep-alive".to_string())];
        let raw = build_response(200, &upstream, b"x", "GET");
        assert_eq!(header_lines(&raw, "connection").len(), 1);
    }

    /// Ordinary headers must still be forwarded; the filter is narrow.
    #[test]
    fn other_headers_are_preserved() {
        let upstream = vec![
            ("Content-Type".to_string(), "text/html".to_string()),
            ("ETag".to_string(), "\"abc\"".to_string()),
        ];
        let raw = build_response(200, &upstream, b"x", "GET");
        assert_eq!(header_lines(&raw, "content-type").len(), 1);
        assert_eq!(header_lines(&raw, "etag").len(), 1);
    }
}

#[cfg(test)]
mod bodyless_status_tests {
    use super::{build_response, status_may_have_content_length};

    fn head_of(status: u16) -> String {
        let raw = build_response(status, &[("ETag".to_string(), "\"x\"".to_string())], b"", "GET");
        String::from_utf8_lossy(&raw).split("\r\n\r\n").next().unwrap_or("").to_string()
    }

    /// RFC 9110 8.6. m6 sent `content-length: 0` on every 304, on all three
    /// protocols. Zero is the value that actively misinforms: the 200 for that
    /// resource is 16 KB, so the response claimed the representation was empty,
    /// and a cache updating its stored entry from that 304 could conclude the
    /// body it holds is the wrong length.
    #[test]
    fn bodyless_statuses_omit_content_length() {
        for status in [204u16, 304, 100, 101, 199] {
            assert!(
                !status_may_have_content_length(status),
                "{status} must not carry Content-Length"
            );
            let head = head_of(status);
            assert!(
                !head.to_lowercase().contains("content-length"),
                "{status} emitted Content-Length:\n{head}"
            );
        }
    }

    /// ...and every other status still must, or the framing breaks.
    #[test]
    fn ordinary_statuses_still_carry_content_length() {
        for status in [200u16, 201, 301, 400, 404, 412, 500, 502, 504] {
            assert!(status_may_have_content_length(status), "{status}");
            let head = head_of(status);
            assert!(
                head.to_lowercase().contains("content-length: 0"),
                "{status} lost its Content-Length:\n{head}"
            );
        }
    }

    /// The validators a client needs must survive on a 304 -- omitting
    /// Content-Length must not have been achieved by stripping the header block.
    #[test]
    fn a_304_keeps_its_validators() {
        let head = head_of(304);
        assert!(head.contains("ETag"), "304 lost its ETag:\n{head}");
        assert!(head.starts_with("HTTP/1.1 304 Not Modified"), "{head}");
    }
}
