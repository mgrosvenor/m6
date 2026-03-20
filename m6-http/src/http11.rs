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
const MAX_REQUEST_BYTES: usize = 64 * 1024;

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
                    Ok(0)  => { h1.state = H1State::Done; return; }
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
                let snap = buf.clone();
                match parse_request(&snap) {
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
                                buf.extend_from_slice(&build_response(status, &resp_headers, &body));
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
                buf.extend_from_slice(&build_response(status, &resp_headers, &body));
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

enum ParseResult {
    Incomplete,
    Error,
    Complete(HttpRequest),
}

fn parse_request(buf: &[u8]) -> ParseResult {
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

    // Extract what we need from req.headers before dropping req
    let nheaders = req.headers.len();
    let content_length: usize = req.headers[..nheaders]
        .iter()
        .find(|h| h.name.eq_ignore_ascii_case("content-length"))
        .and_then(|h| std::str::from_utf8(h.value).ok())
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0);

    let fwd_headers: Vec<(String, String)> = req.headers[..nheaders]
        .iter()
        .filter_map(|h| {
            let v = std::str::from_utf8(h.value).ok()?;
            Some((h.name.to_string(), v.to_string()))
        })
        .collect();

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

fn build_response(status: u16, headers: &[(String, String)], body: &[u8]) -> Vec<u8> {
    let reason = status_reason(status);
    let mut out = Vec::with_capacity(256 + body.len());
    out.extend_from_slice(
        format!("HTTP/1.1 {} {}\r\n", status, reason).as_bytes()
    );
    for (k, v) in headers {
        out.extend_from_slice(format!("{}: {}\r\n", k, v).as_bytes());
    }
    out.extend_from_slice(format!("content-length: {}\r\n", body.len()).as_bytes());
    out.extend_from_slice(b"connection: close\r\n\r\n");
    out.extend_from_slice(body);
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
