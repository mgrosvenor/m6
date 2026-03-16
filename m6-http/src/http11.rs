/// Non-blocking HTTP/1.1 over TLS (rustls) integrated into the existing epoll loop.
///
/// Design:
/// - `Http11Listener` wraps a non-blocking `TcpListener` (registered with TOKEN_TCP).
/// - On each TOKEN_TCP wakeup: `accept_pending()` drains incoming connections into an
///   internal `Vec<Conn>` — no individual fd registration; connections are polled on
///   every event-loop tick via `drive_all()`.
/// - Each connection runs a simple state machine: Handshake → Reading → Writing { pos }.
/// - TLS is handled by rustls in non-blocking (sans-I/O) mode.
/// - When a complete HTTP/1.1 request is parsed, an `on_request` callback is called.
///   The caller returns `(status, headers, body, backend_name)`.
/// - Responses use `Connection: close`; each TCP connection handles exactly one request.

use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::io::{AsRawFd, RawFd};
use std::sync::Arc;
use std::time::Instant;

use rustls::ServerConnection;
use tracing::warn;

use crate::forward::HttpRequest;
use crate::poller::{Poller, Token};

// ── Connection state machine ──────────────────────────────────────────────────

enum State {
    Handshake,
    Reading { buf: Vec<u8> },
    Writing { buf: Vec<u8>, pos: usize },
    Done,
}

struct Conn {
    stream: TcpStream,
    tls: ServerConnection,
    state: State,
    client_ip: String,
    created: Instant,
}

const READ_TIMEOUT_SECS: u64 = 30;
const MAX_REQUEST_BYTES: usize = 64 * 1024;

// ── Public API ────────────────────────────────────────────────────────────────

pub struct Http11Listener {
    listener: TcpListener,
    tls_config: Arc<rustls::ServerConfig>,
    conns: Vec<Conn>,
}

impl Http11Listener {
    pub fn bind(addr: &str, tls_config: Arc<rustls::ServerConfig>) -> anyhow::Result<Self> {
        let listener = TcpListener::bind(addr)?;
        listener.set_nonblocking(true)?;
        Ok(Http11Listener { listener, tls_config, conns: Vec::new() })
    }

    pub fn raw_fd(&self) -> RawFd {
        self.listener.as_raw_fd()
    }

    pub fn local_addr(&self) -> std::io::Result<std::net::SocketAddr> {
        self.listener.local_addr()
    }

    /// Call when the listener fd is readable — drain all pending `accept()` calls.
    /// Each new connection fd is registered with `poller` using `token` so that
    /// incoming data wakes the event loop immediately instead of waiting for the
    /// next tick.
    pub fn accept_pending(&mut self, poller: &Poller, token: Token) {
        loop {
            match self.listener.accept() {
                Ok((stream, peer)) => {
                    if let Err(e) = stream.set_nonblocking(true) {
                        warn!("http11 set_nonblocking: {e}");
                        continue;
                    }
                    // Disable Nagle: TLS handshake writes are small and must
                    // not be coalesced with subsequent data.
                    stream.set_nodelay(true).ok();
                    let tls = match ServerConnection::new(Arc::clone(&self.tls_config)) {
                        Ok(t) => t,
                        Err(e) => { warn!("http11 ServerConnection::new: {e}"); continue; }
                    };
                    poller.add(stream.as_raw_fd(), token).ok();
                    let mut conn = Conn {
                        stream,
                        tls,
                        state: State::Handshake,
                        client_ip: peer.ip().to_string(),
                        created: Instant::now(),
                    };
                    // Eagerly start the TLS handshake: on loopback the
                    // ClientHello is already in the kernel buffer at accept
                    // time, so we can send ServerHello immediately rather
                    // than waiting for the next drive_all() tick.
                    if advance_tls_io(&mut conn).is_ok() && !conn.tls.is_handshaking() {
                        conn.state = State::Reading { buf: Vec::new() };
                    }
                    self.conns.push(conn);
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => { warn!("http11 accept: {e}"); break; }
            }
        }
    }

    /// Drive all active connections one step forward.
    /// `on_request(req, client_ip) -> (status, headers, body, backend_name)`
    /// Done connections are deregistered from `poller` before being dropped.
    pub fn drive_all<F>(&mut self, mut on_request: F, poller: &Poller)
    where
        F: FnMut(&HttpRequest, &str) -> (u16, Vec<(String, String)>, Vec<u8>, String),
    {
        let now = Instant::now();

        for conn in &mut self.conns {
            // Timeout stale connections
            if now.duration_since(conn.created).as_secs() > READ_TIMEOUT_SECS {
                conn.state = State::Done;
                continue;
            }
            drive_conn(conn, &mut on_request);
        }

        for conn in &self.conns {
            if matches!(conn.state, State::Done) {
                poller.delete(conn.stream.as_raw_fd()).ok();
            }
        }
        self.conns.retain(|c| !matches!(c.state, State::Done));
    }
}

// ── Per-connection driver ─────────────────────────────────────────────────────

fn drive_conn<F>(conn: &mut Conn, on_request: &mut F)
where
    F: FnMut(&HttpRequest, &str) -> (u16, Vec<(String, String)>, Vec<u8>, String),
{
    // Advance TLS I/O (read from socket → TLS, TLS → write to socket)
    if advance_tls_io(conn).is_err() {
        conn.state = State::Done;
        return;
    }

    loop {
        match &conn.state {
            State::Handshake => {
                if conn.tls.is_handshaking() {
                    break; // Not done yet
                }
                conn.state = State::Reading { buf: Vec::new() };
            }

            State::Reading { .. } => {
                // Read decrypted bytes from TLS
                let mut tmp = [0u8; 4096];
                let n = match conn.tls.reader().read(&mut tmp) {
                    Ok(0) => { conn.state = State::Done; return; }
                    Ok(n) => n,
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                    Err(_) => { conn.state = State::Done; return; }
                };

                let State::Reading { buf } = &mut conn.state else { break };
                buf.extend_from_slice(&tmp[..n]);

                if buf.len() > MAX_REQUEST_BYTES {
                    let resp = b"HTTP/1.1 413 Payload Too Large\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                    conn.state = State::Writing { buf: resp.to_vec(), pos: 0 };
                    continue;
                }

                // Try to parse a complete HTTP/1.1 request
                let buf_snapshot = buf.clone();
                match parse_request(&buf_snapshot) {
                    ParseResult::Incomplete => break,
                    ParseResult::Error => {
                        let resp = b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                        conn.state = State::Writing { buf: resp.to_vec(), pos: 0 };
                        continue;
                    }
                    ParseResult::Complete(req) => {
                        let (status, resp_headers, body, _backend) =
                            on_request(&req, &conn.client_ip);
                        let resp_bytes = build_response(status, &resp_headers, &body);
                        conn.state = State::Writing { buf: resp_bytes, pos: 0 };
                        continue;
                    }
                }
            }

            State::Writing { buf, pos } => {
                let remaining = &buf[*pos..];
                if remaining.is_empty() {
                    conn.state = State::Done;
                    return;
                }
                match conn.tls.writer().write(remaining) {
                    Ok(0) => { conn.state = State::Done; return; }
                    Ok(written) => {
                        let State::Writing { pos, .. } = &mut conn.state else { break };
                        *pos += written;
                        // Loop to keep writing if more data remains
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                    Err(_) => { conn.state = State::Done; return; }
                }
                // After writing, flush TLS output to socket
                if advance_tls_io(conn).is_err() {
                    conn.state = State::Done;
                    return;
                }
                let State::Writing { buf, pos } = &conn.state else { break };
                if *pos >= buf.len() {
                    conn.state = State::Done;
                    return;
                }
                break;
            }

            State::Done => return,
        }
    }
}

/// Pump bytes between socket and rustls buffers.
fn advance_tls_io(conn: &mut Conn) -> io::Result<()> {
    // Socket → TLS (read encrypted bytes)
    loop {
        match conn.tls.read_tls(&mut conn.stream) {
            Ok(0) => return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "closed")),
            Ok(_) => {
                conn.tls.process_new_packets().map_err(|e| {
                    io::Error::new(io::ErrorKind::InvalidData, e.to_string())
                })?;
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
            Err(e) => return Err(e),
        }
    }
    // TLS → socket (write encrypted bytes)
    loop {
        match conn.tls.write_tls(&mut conn.stream) {
            Ok(0) => break,
            Ok(_) => {}
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

    let config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| anyhow::anyhow!("tls config: {}", e))?;

    Ok(Arc::new(config))
}
