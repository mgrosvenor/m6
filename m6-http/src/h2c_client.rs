//! Persistent non-blocking H2C (HTTP/2 cleartext) outbound client.
//!
//! One `H2cClientConn` is maintained per backend URL. It is registered with
//! the epoll/kqueue poller and driven on every event-loop iteration.
//! Requests are dispatched as new H2 streams; the caller gets an
//! `mpsc::Receiver` that resolves when the stream completes.

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::os::unix::io::{AsRawFd, RawFd};
use std::sync::mpsc;

use crate::forward::{HttpRequest, HttpResponse, HOP_BY_HOP};
use crate::poller::{Poller, Token};

// ── Frame constants ───────────────────────────────────────────────────────────

const CLIENT_PREFACE: &[u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";
const FRAME_HDR: usize = 9;
const TYPE_DATA: u8 = 0x0;
const TYPE_HEADERS: u8 = 0x1;
const TYPE_RST_STREAM: u8 = 0x3;
const TYPE_SETTINGS: u8 = 0x4;
const TYPE_PING: u8 = 0x6;
const TYPE_GOAWAY: u8 = 0x7;
const TYPE_WINDOW_UPDATE: u8 = 0x8;
const TYPE_CONTINUATION: u8 = 0x9;
const FLAG_END_STREAM: u8 = 0x1;
const FLAG_END_HEADERS: u8 = 0x4;
const FLAG_PADDED: u8 = 0x8;
const FLAG_PRIORITY: u8 = 0x20;
const FLAG_ACK: u8 = 0x1;
const DEFAULT_WINDOW: i32 = 65_535;
const DEFAULT_MAX_FRAME: u32 = 16_384;

// ── H2cStream (private) ───────────────────────────────────────────────────────

struct H2cStream {
    resp_status: u16,
    resp_headers: Vec<(String, String)>,
    resp_body: Vec<u8>,
    headers_done: bool,
    tx: mpsc::Sender<io::Result<HttpResponse>>,
}

// ── H2cClientConn (public) ────────────────────────────────────────────────────

pub struct H2cClientConn {
    stream: TcpStream,
    send_buf: Vec<u8>,
    recv_buf: Vec<u8>,
    streams: HashMap<u32, H2cStream>,
    next_stream_id: u32,
    hpack_enc: hpack::Encoder<'static>,
    hpack_dec: hpack::Decoder<'static>,
    header_block_acc: Vec<u8>,
    continuation_stream: Option<u32>,
    conn_send_window: i32,
    peer_initial_window: i32,
    peer_max_frame: u32,
    pub is_dead: bool,
    registered: bool,
}

impl H2cClientConn {
    /// Connect to an H2C backend at `host:port`.
    ///
    /// Sends the client preface and a SETTINGS frame synchronously (before
    /// switching to non-blocking mode) so the initial handshake never stalls
    /// the event loop.
    pub fn connect(host: &str, port: u16) -> io::Result<Self> {
        let mut stream = TcpStream::connect(format!("{}:{}", host, port))?;
        stream.set_nodelay(true)?;

        // Send client preface + SETTINGS (INITIAL_WINDOW_SIZE = 1 MiB) while
        // still in blocking mode so the write never returns WouldBlock.
        let mut init_buf: Vec<u8> = Vec::with_capacity(CLIENT_PREFACE.len() + FRAME_HDR + 6);
        init_buf.extend_from_slice(CLIENT_PREFACE);

        // SETTINGS frame: one setting, id=4 (INITIAL_WINDOW_SIZE), value=1_048_576
        let settings_payload: &[u8] = &[
            0x00, 0x04, // setting id = 4
            0x00, 0x10, 0x00, 0x00, // value = 1_048_576 (0x00100000)
        ];
        // Build frame manually (push_frame requires &mut self)
        let len = settings_payload.len() as u32;
        init_buf.push((len >> 16) as u8);
        init_buf.push((len >> 8) as u8);
        init_buf.push(len as u8);
        init_buf.push(TYPE_SETTINGS);
        init_buf.push(0); // flags
        init_buf.push(0);
        init_buf.push(0);
        init_buf.push(0);
        init_buf.push(0); // stream_id = 0
        init_buf.extend_from_slice(settings_payload);

        stream.write_all(&init_buf)?;
        stream.flush()?;

        stream.set_nonblocking(true)?;

        Ok(Self {
            stream,
            send_buf: Vec::new(),
            recv_buf: Vec::new(),
            streams: HashMap::new(),
            next_stream_id: 1,
            hpack_enc: hpack::Encoder::new(),
            hpack_dec: hpack::Decoder::new(),
            header_block_acc: Vec::new(),
            continuation_stream: None,
            conn_send_window: DEFAULT_WINDOW,
            peer_initial_window: DEFAULT_WINDOW,
            peer_max_frame: DEFAULT_MAX_FRAME,
            is_dead: false,
            registered: false,
        })
    }

    /// Return the raw file descriptor for this connection.
    pub fn raw_fd(&self) -> RawFd {
        self.stream.as_raw_fd()
    }

    /// Dispatch an HTTP request as a new H2 stream.
    ///
    /// Returns a receiver that will yield the response when the stream
    /// completes.  The connection must not be dead.
    pub fn dispatch(
        &mut self,
        req: &HttpRequest,
        host: &str,
        client_ip: &str,
        original_host: &str,
    ) -> io::Result<mpsc::Receiver<io::Result<HttpResponse>>> {
        if self.is_dead {
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, "h2c: connection dead"));
        }

        let stream_id = self.next_stream_id;
        self.next_stream_id += 2;

        // Build :path
        let full_path: String = match &req.query {
            Some(q) if !q.is_empty() => format!("{}?{}", req.path, q),
            _ => req.path.clone(),
        };

        // Collect forwarded headers (skip hop-by-hop and pseudo-headers)
        let forwarded: Vec<(Vec<u8>, Vec<u8>)> = req
            .headers
            .iter()
            .filter_map(|(name, value)| {
                if HOP_BY_HOP.iter().any(|&h| name.eq_ignore_ascii_case(h)) {
                    return None;
                }
                if name.starts_with(':') {
                    return None;
                }
                Some((
                    name.to_ascii_lowercase().into_bytes(),
                    value.as_bytes().to_vec(),
                ))
            })
            .collect();

        // Build HPACK header list
        let mut hdr_list: Vec<(&[u8], &[u8])> = Vec::new();
        hdr_list.push((b":method", req.method.as_bytes()));
        hdr_list.push((b":path", full_path.as_bytes()));
        hdr_list.push((b":scheme", b"http"));
        hdr_list.push((b":authority", host.as_bytes()));
        hdr_list.push((b"x-forwarded-for", client_ip.as_bytes()));
        hdr_list.push((b"x-forwarded-proto", b"https"));
        hdr_list.push((b"x-forwarded-host", original_host.as_bytes()));
        for (k, v) in &forwarded {
            hdr_list.push((k.as_slice(), v.as_slice()));
        }

        let header_block = self.hpack_enc.encode(hdr_list);

        // Push HEADERS frame
        let has_body = !req.body.is_empty();
        let headers_flags =
            FLAG_END_HEADERS | if has_body { 0 } else { FLAG_END_STREAM };
        self.push_frame(TYPE_HEADERS, headers_flags, stream_id, &header_block);

        // Push DATA frame if body present
        if has_body {
            self.push_frame(TYPE_DATA, FLAG_END_STREAM, stream_id, &req.body);
        }

        let (tx, rx) = mpsc::channel();
        self.streams.insert(
            stream_id,
            H2cStream {
                resp_status: 0,
                resp_headers: vec![],
                resp_body: vec![],
                headers_done: false,
                tx,
            },
        );

        Ok(rx)
    }

    /// Drive I/O: flush send buffer, read incoming data, parse frames.
    ///
    /// Must be called on every event-loop iteration.
    pub fn drive(&mut self) {
        if self.is_dead {
            return;
        }

        // ── Flush send buffer ─────────────────────────────────────────────────
        let mut flushed = 0usize;
        while flushed < self.send_buf.len() {
            match self.stream.write(&self.send_buf[flushed..]) {
                Ok(0) => {
                    self.mark_dead("h2c: send got zero bytes");
                    return;
                }
                Ok(n) => flushed += n,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => {
                    self.mark_dead(&format!("h2c: send error: {}", e));
                    return;
                }
            }
        }
        if flushed > 0 {
            self.send_buf.drain(..flushed);
        }

        // ── Read incoming data ────────────────────────────────────────────────
        let mut tmp = [0u8; 16384];
        loop {
            match self.stream.read(&mut tmp) {
                Ok(0) => {
                    self.mark_dead("h2c: connection closed");
                    return;
                }
                Ok(n) => self.recv_buf.extend_from_slice(&tmp[..n]),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => {
                    self.mark_dead(&format!("h2c: read error: {}", e));
                    return;
                }
            }
        }

        // ── Process frames ────────────────────────────────────────────────────
        loop {
            if self.recv_buf.len() < FRAME_HDR {
                break;
            }

            let length = ((self.recv_buf[0] as usize) << 16)
                | ((self.recv_buf[1] as usize) << 8)
                | (self.recv_buf[2] as usize);

            if self.recv_buf.len() < FRAME_HDR + length {
                break;
            }

            let ftype = self.recv_buf[3];
            let flags = self.recv_buf[4];
            let stream_id = (((self.recv_buf[5] as u32) << 24)
                | ((self.recv_buf[6] as u32) << 16)
                | ((self.recv_buf[7] as u32) << 8)
                | (self.recv_buf[8] as u32))
                & 0x7fff_ffff;

            let payload = self.recv_buf[FRAME_HDR..FRAME_HDR + length].to_vec();
            self.recv_buf.drain(..FRAME_HDR + length);

            // CONTINUATION protocol: if we're accumulating a block and the
            // incoming frame is NOT a CONTINUATION on that stream, that's a
            // protocol error.
            if let Some(cont_sid) = self.continuation_stream {
                if ftype != TYPE_CONTINUATION || stream_id != cont_sid {
                    self.mark_dead("h2c: expected CONTINUATION frame");
                    return;
                }
            }

            match ftype {
                TYPE_SETTINGS => {
                    if flags & FLAG_ACK == 0 {
                        // Parse settings pairs (6 bytes each)
                        let mut pos = 0usize;
                        while pos + 6 <= payload.len() {
                            let id = ((payload[pos] as u16) << 8) | (payload[pos + 1] as u16);
                            let val = ((payload[pos + 2] as u32) << 24)
                                | ((payload[pos + 3] as u32) << 16)
                                | ((payload[pos + 4] as u32) << 8)
                                | (payload[pos + 5] as u32);
                            match id {
                                4 => self.peer_initial_window = val as i32,
                                5 if val >= 16_384 && val <= 16_777_215 => {
                                    self.peer_max_frame = val
                                }
                                _ => {}
                            }
                            pos += 6;
                        }
                        // Send SETTINGS ACK
                        self.push_frame(TYPE_SETTINGS, FLAG_ACK, 0, &[]);
                    }
                }

                TYPE_HEADERS if stream_id > 0 => {
                    let mut pl = payload;

                    // Strip PADDED prefix
                    let mut pos = 0usize;
                    if flags & FLAG_PADDED != 0 {
                        if pl.is_empty() {
                            self.mark_dead("h2c: padded HEADERS with empty payload");
                            return;
                        }
                        let pad_len = pl[0] as usize;
                        pos = 1;
                        if pl.len() < pos + pad_len {
                            self.mark_dead("h2c: HEADERS pad_len exceeds payload");
                            return;
                        }
                        let new_len = pl.len() - pad_len;
                        pl.truncate(new_len);
                    }

                    // Skip PRIORITY fields (5 bytes)
                    if flags & FLAG_PRIORITY != 0 {
                        pos += 5;
                    }

                    if pos <= pl.len() {
                        self.header_block_acc.extend_from_slice(&pl[pos..]);
                    }

                    if flags & FLAG_END_HEADERS != 0 {
                        let block = std::mem::take(&mut self.header_block_acc);
                        match self.hpack_dec.decode(&block) {
                            Ok(pairs) => {
                                if let Some(s) = self.streams.get_mut(&stream_id) {
                                    for (k, v) in pairs {
                                        if k == b":status" {
                                            s.resp_status =
                                                String::from_utf8_lossy(&v).parse().unwrap_or(0);
                                        } else {
                                            s.resp_headers.push((
                                                String::from_utf8_lossy(&k).into_owned(),
                                                String::from_utf8_lossy(&v).into_owned(),
                                            ));
                                        }
                                    }
                                    s.headers_done = true;
                                }
                            }
                            Err(e) => {
                                self.mark_dead(&format!("h2c: HPACK decode error: {:?}", e));
                                return;
                            }
                        }
                        self.continuation_stream = None;
                    } else {
                        self.continuation_stream = Some(stream_id);
                    }

                    if flags & FLAG_END_STREAM != 0 {
                        self.complete_stream(stream_id);
                    }
                }

                TYPE_CONTINUATION if stream_id > 0 => {
                    self.header_block_acc.extend_from_slice(&payload);

                    if flags & FLAG_END_HEADERS != 0 {
                        let block = std::mem::take(&mut self.header_block_acc);
                        match self.hpack_dec.decode(&block) {
                            Ok(pairs) => {
                                if let Some(s) = self.streams.get_mut(&stream_id) {
                                    for (k, v) in pairs {
                                        if k == b":status" {
                                            s.resp_status =
                                                String::from_utf8_lossy(&v).parse().unwrap_or(0);
                                        } else {
                                            s.resp_headers.push((
                                                String::from_utf8_lossy(&k).into_owned(),
                                                String::from_utf8_lossy(&v).into_owned(),
                                            ));
                                        }
                                    }
                                    s.headers_done = true;
                                }
                            }
                            Err(e) => {
                                self.mark_dead(&format!("h2c: HPACK decode error: {:?}", e));
                                return;
                            }
                        }
                        self.continuation_stream = None;
                    }
                }

                TYPE_DATA if stream_id > 0 => {
                    let mut pl = payload;
                    let mut pos = 0usize;

                    if flags & FLAG_PADDED != 0 {
                        if pl.is_empty() {
                            self.mark_dead("h2c: padded DATA with empty payload");
                            return;
                        }
                        let pad_len = pl[0] as usize;
                        pos = 1;
                        if pl.len() < pos + pad_len {
                            self.mark_dead("h2c: DATA pad_len exceeds payload");
                            return;
                        }
                        let new_len = pl.len() - pad_len;
                        pl.truncate(new_len);
                    }

                    let data = &pl[pos..];
                    let data_len = data.len();

                    if data_len > 0 {
                        let inc = (data_len as u32) & 0x7fff_ffff;
                        let inc_bytes = inc.to_be_bytes();
                        // WINDOW_UPDATE for connection (stream_id = 0)
                        self.push_frame(TYPE_WINDOW_UPDATE, 0, 0, &inc_bytes);
                        // WINDOW_UPDATE for this stream
                        self.push_frame(TYPE_WINDOW_UPDATE, 0, stream_id, &inc_bytes);
                    }

                    if let Some(s) = self.streams.get_mut(&stream_id) {
                        s.resp_body.extend_from_slice(data);
                    }

                    if flags & FLAG_END_STREAM != 0 {
                        self.complete_stream(stream_id);
                    }
                }

                TYPE_RST_STREAM if stream_id > 0 => {
                    if let Some(s) = self.streams.remove(&stream_id) {
                        let _ = s.tx.send(Err(io::Error::new(
                            io::ErrorKind::BrokenPipe,
                            "h2c: stream reset by server",
                        )));
                    }
                }

                TYPE_GOAWAY => {
                    self.mark_dead("h2c: server sent GOAWAY");
                    return;
                }

                TYPE_PING => {
                    if flags & FLAG_ACK == 0 && payload.len() == 8 {
                        let ping_payload = payload.clone();
                        self.push_frame(TYPE_PING, FLAG_ACK, 0, &ping_payload);
                    }
                }

                TYPE_WINDOW_UPDATE if stream_id == 0 => {
                    if payload.len() >= 4 {
                        let inc = ((payload[0] as u32) << 24)
                            | ((payload[1] as u32) << 16)
                            | ((payload[2] as u32) << 8)
                            | (payload[3] as u32);
                        self.conn_send_window += inc as i32;
                    }
                }

                _ => {
                    // Ignore all other frame types
                }
            }
        }
    }

    /// Complete a stream: remove it and send the response (or an error) on tx.
    fn complete_stream(&mut self, stream_id: u32) {
        if let Some(s) = self.streams.remove(&stream_id) {
            if s.headers_done {
                let _ = s.tx.send(Ok(HttpResponse {
                    status: s.resp_status,
                    reason: String::new(),
                    headers: s.resp_headers,
                    body: s.resp_body,
                }));
            } else {
                let _ = s.tx.send(Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "h2c: stream ended without headers",
                )));
            }
        }
    }

    /// Mark the connection as dead and drain all pending streams with an error.
    fn mark_dead(&mut self, reason: &str) {
        self.is_dead = true;
        let streams = std::mem::take(&mut self.streams);
        for (_, s) in streams {
            let _ = s.tx.send(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                reason.to_string(),
            )));
        }
    }

    /// Append a well-formed H2 frame to `send_buf`.
    fn push_frame(&mut self, ftype: u8, flags: u8, stream_id: u32, payload: &[u8]) {
        let len = payload.len() as u32;
        self.send_buf.push((len >> 16) as u8);
        self.send_buf.push((len >> 8) as u8);
        self.send_buf.push(len as u8);
        self.send_buf.push(ftype);
        self.send_buf.push(flags);
        let sid = stream_id & 0x7fff_ffff;
        self.send_buf.push((sid >> 24) as u8);
        self.send_buf.push((sid >> 16) as u8);
        self.send_buf.push((sid >> 8) as u8);
        self.send_buf.push(sid as u8);
        self.send_buf.extend_from_slice(payload);
    }
}

// ── H2cClientPool (public) ────────────────────────────────────────────────────

pub struct H2cClientPool {
    /// backend base_url → connection
    connections: HashMap<String, H2cClientConn>,
}

impl H2cClientPool {
    pub fn new() -> Self {
        Self {
            connections: HashMap::new(),
        }
    }

    /// Dispatch a request to the named backend URL.
    ///
    /// Creates a new connection if none exists or the existing one is dead.
    pub fn dispatch(
        &mut self,
        base_url: &str,
        req: &HttpRequest,
        client_ip: &str,
        original_host: &str,
    ) -> io::Result<mpsc::Receiver<io::Result<HttpResponse>>> {
        // Check if we need a new connection
        let needs_new = match self.connections.get(base_url) {
            None => true,
            Some(conn) => conn.is_dead && conn.streams.is_empty(),
        };

        if needs_new {
            let (host, port) = parse_h2c_host_port(base_url)?;
            let new_conn = H2cClientConn::connect(&host, port)?;
            self.connections.insert(base_url.to_string(), new_conn);
        }

        // Get host for :authority header
        let (host, _) = parse_h2c_host_port(base_url)?;

        let conn = self.connections.get_mut(base_url).unwrap();
        conn.dispatch(req, &host, client_ip, original_host)
    }

    /// Register new connections with the poller and drive all connections.
    ///
    /// Must be called on every event-loop iteration.
    pub fn drive_all(&mut self, poller: &Poller, token: Token) {
        for conn in self.connections.values_mut() {
            if !conn.registered {
                let _ = poller.add(conn.raw_fd(), token);
                conn.registered = true;
            }
            conn.drive();
        }

        // Remove connections that are dead and have no pending streams
        self.connections
            .retain(|_, conn| !(conn.is_dead && conn.streams.is_empty()));
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Parse `h2c://host:port[/path]` into `(host, port)`.
fn parse_h2c_host_port(base_url: &str) -> io::Result<(String, u16)> {
    let rest = base_url
        .strip_prefix("h2c://")
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "not an h2c:// URL"))?;

    // Strip any path suffix
    let authority = match rest.find('/') {
        Some(idx) => &rest[..idx],
        None => rest,
    };

    // IPv6: [::1]:port or [::1]
    if let Some(bracket_end) = authority.find(']') {
        let host = authority[1..bracket_end].to_string();
        let port = if bracket_end + 1 < authority.len()
            && authority.as_bytes()[bracket_end + 1] == b':'
        {
            authority[bracket_end + 2..].parse::<u16>().map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidInput, "invalid port in h2c URL")
            })?
        } else {
            80
        };
        return Ok((host, port));
    }

    // host:port or host
    if let Some(colon) = authority.rfind(':') {
        if let Ok(p) = authority[colon + 1..].parse::<u16>() {
            return Ok((authority[..colon].to_string(), p));
        }
    }

    Ok((authority.to_string(), 80))
}
