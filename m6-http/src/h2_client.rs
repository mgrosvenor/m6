//! Shared HTTP/2 outbound client core, generic over the transport.
//!
//! The `h2c://` (cleartext TCP) and `h2s://` (TLS) backbone clients are one
//! engine. The only differences are `connect()` and the transport type, which
//! the [`Transport`] trait isolates. The frame loop, flow control, HPACK and
//! stream lifecycle live here once, so a fix to any of them cannot drift
//! between the two clients.

use std::collections::HashMap;
use std::io;
use std::os::unix::io::RawFd;
use std::sync::mpsc;
use std::sync::Arc;

use crate::forward::{
    apply_response_block, too_many_informational, HttpRequest, HttpResponse, ResponseBlock,
    HOP_BY_HOP,
};
use m6_core::mio::{Interest, Registry, Token};

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

/// The bits that differ between the cleartext and TLS transports.
///
/// `connect()` performs the transport setup and the synchronous HTTP/2
/// handshake (client preface + SETTINGS). The rest are the I/O operations the
/// shared frame loop needs, plus the per-transport constants.
pub trait Transport: Sized {
    /// Establish the transport and send the client preface + SETTINGS.
    fn connect(host: &str, port: u16, tls: Option<Arc<rustls::ClientConfig>>) -> io::Result<Self>;

    /// The raw file descriptor, for the poll.
    fn raw_fd(&self) -> RawFd;

    /// The `:scheme` value for dispatched requests.
    fn scheme() -> &'static [u8];

    /// The default port for the URL scheme.
    fn default_port() -> u16;

    /// The URL scheme and log prefix, `h2c` or `h2s`.
    fn prefix() -> &'static str;

    /// Flush as much of `send_buf` to the peer as will be accepted, draining
    /// what was written. `Err(reason)` means the connection must die.
    fn flush_send(&mut self, send_buf: &mut Vec<u8>) -> Result<(), String>;

    /// Read whatever decrypted bytes are available into `recv_buf`. Returns
    /// `Some(reason)` on EOF or read error, deferred until after the frame loop.
    fn fill_recv(&mut self, recv_buf: &mut Vec<u8>) -> Option<String>;

    /// Whether the transport still holds bytes to flush (rustls ciphertext for
    /// TLS, always false for cleartext), so the connection asks for write
    /// readiness rather than dropping `WRITABLE`.
    fn wants_write(&self) -> bool;
}

// ── H2Stream (private) ────────────────────────────────────────────────────────

struct H2Stream {
    resp_status: u16,
    resp_headers: Vec<(String, String)>,
    resp_body: Vec<u8>,
    headers_done: bool,
    /// Informational (1xx) header blocks seen so far on this stream.
    informational: usize,
    tx: mpsc::Sender<io::Result<HttpResponse>>,
    // The request body still owed to the peer, and how much of it has gone out.
    // The body is resumable state rather than a loop because credit arrives
    // asynchronously: see `pump_stream`.
    pending_body: Vec<u8>,
    body_off: usize,
    // RFC 9113 6.9.2: the per-stream send window starts at the peer's
    // SETTINGS_INITIAL_WINDOW_SIZE, not at the 65 535 connection default.
    send_window: i32,
}

// ── H2Client (public) ─────────────────────────────────────────────────────────

pub struct H2Client<T: Transport> {
    transport: T,
    send_buf: Vec<u8>,
    recv_buf: Vec<u8>,
    streams: HashMap<u32, H2Stream>,
    next_stream_id: u32,
    hpack_enc: hpack::Encoder<'static>,
    hpack_dec: hpack::Decoder<'static>,
    header_block_acc: Vec<u8>,
    continuation_stream: Option<u32>,
    conn_send_window: i32,
    peer_initial_window: i32,
    peer_max_frame: u32,
    pub is_dead: bool,
    /// The `mio` token this connection's descriptor is registered under.
    token: Token,
    /// What the poll currently waits for, so `reregister` runs only on change.
    registered_interest: Interest,
}

impl<T: Transport> H2Client<T> {
    /// Build a client around an already-connected transport.
    pub(crate) fn new(transport: T) -> Self {
        Self {
            transport,
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
            token: Token(usize::MAX),
            registered_interest: Interest::READABLE,
        }
    }

    /// Connect via the transport and build the client around it.
    pub(crate) fn connect(
        host: &str,
        port: u16,
        tls: Option<Arc<rustls::ClientConfig>>,
    ) -> io::Result<Self> {
        Ok(Self::new(T::connect(host, port, tls)?))
    }

    /// Return the raw file descriptor for this connection.
    pub fn raw_fd(&self) -> RawFd {
        self.transport.raw_fd()
    }

    /// The `mio` token this connection is registered under.
    pub fn token(&self) -> Token {
        self.token
    }

    /// Set the token the caller registered this connection under.
    pub fn set_token(&mut self, token: Token) {
        self.token = token;
    }

    /// Set the interest the caller registered this connection with.
    pub fn set_interest(&mut self, interest: Interest) {
        self.registered_interest = interest;
    }

    /// The interest to register next: write readiness while request bytes are
    /// still owed or the transport holds unflushed output, read readiness once
    /// both are empty.
    pub fn interest(&self) -> Interest {
        if self.send_buf.is_empty() && !self.transport.wants_write() {
            Interest::READABLE
        } else {
            Interest::READABLE | Interest::WRITABLE
        }
    }

    /// Dispatch an HTTP request as a new H2 stream.
    ///
    /// Returns a receiver that will yield the response when the stream
    /// completes. The connection must not be dead.
    pub fn dispatch(
        &mut self,
        req: &HttpRequest,
        host: &str,
        client_ip: &str,
        original_host: &str,
    ) -> io::Result<mpsc::Receiver<io::Result<HttpResponse>>> {
        if self.is_dead {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                format!("{}: connection dead", T::prefix()),
            ));
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
                // The proxy re-emits x-forwarded-* below from the real
                // connection, and derives content-length from the body it
                // actually sends. (x-auth-claims is NOT dropped here — by this
                // point it can only have been set by m6-http after verifying
                // the JWT; client copies were stripped at ingress.)
                if crate::forward::is_proxy_emitted_hop_header(name) {
                    return None;
                }
                Some((
                    name.to_ascii_lowercase().into_bytes(),
                    value.as_bytes().to_vec(),
                ))
            })
            .collect();

        // Build HPACK header list
        let mut hdr_list: Vec<(&[u8], &[u8])> = vec![
            (b":method", req.method.as_bytes()),
            (b":path", full_path.as_bytes()),
            (b":scheme", T::scheme()),
            (b":authority", host.as_bytes()),
            (b"x-forwarded-for", client_ip.as_bytes()),
            (b"x-forwarded-proto", b"https"),
            (b"x-forwarded-host", original_host.as_bytes()),
        ];
        for (k, v) in &forwarded {
            hdr_list.push((k.as_slice(), v.as_slice()));
        }

        let header_block = self.hpack_enc.encode(hdr_list);

        // Push HEADERS frame
        let has_body = !req.body.is_empty();
        let headers_flags = FLAG_END_HEADERS | if has_body { 0 } else { FLAG_END_STREAM };
        self.push_frame(TYPE_HEADERS, headers_flags, stream_id, &header_block);

        let (tx, rx) = mpsc::channel();
        self.streams.insert(
            stream_id,
            H2Stream {
                resp_status: 0,
                resp_headers: vec![],
                resp_body: vec![],
                headers_done: false,
                informational: 0,
                tx,
                pending_body: if has_body {
                    req.body.clone()
                } else {
                    Vec::new()
                },
                body_off: 0,
                send_window: self.peer_initial_window,
            },
        );

        // Send the body via the flow-control pump. Credit arrives
        // asynchronously on WINDOW_UPDATE, so the remainder is kept as stream
        // state and resumed when credit arrives; blocking instead would stall
        // every other connection this loop drives, and sending without credit
        // is a connection error (RFC 9113 6.9).
        self.pump_stream(stream_id);

        Ok(rx)
    }

    /// Send as much of a stream's pending body as connection and stream credit
    /// allow, in frames no larger than the peer's SETTINGS_MAX_FRAME_SIZE.
    ///
    /// Safe to call at any time: it is a no-op for a stream that is finished,
    /// gone, or currently out of credit. END_STREAM rides on the frame that
    /// drains the body, never on a chunk chosen by a loop that ran out of room,
    /// so the bytes sent always match the advertised `content-length`.
    fn pump_stream(&mut self, stream_id: u32) {
        let max_frame = (self.peer_max_frame as usize).max(1);
        loop {
            let (start, n, last) = {
                let Some(s) = self.streams.get_mut(&stream_id) else {
                    return;
                };
                let remaining = s.pending_body.len() - s.body_off;
                if remaining == 0 {
                    return;
                }
                // Either window can be negative: a SETTINGS_INITIAL_WINDOW_SIZE
                // reduction applies retroactively (6.9.2) and legitimately
                // overdraws an open stream.
                let credit = self.conn_send_window.min(s.send_window);
                if credit <= 0 {
                    return;
                }
                let n = remaining.min(max_frame).min(credit as usize);
                let start = s.body_off;
                s.body_off += n;
                s.send_window -= n as i32;
                let last = s.body_off == s.pending_body.len();
                (start, n, last)
            };
            self.conn_send_window -= n as i32;
            {
                // Append straight from the pending body. `append_frame` borrows
                // only `send_buf`, disjoint from the `streams` borrow holding
                // `pending_body`, so the chunk is not cloned.
                let s = self.streams.get(&stream_id).unwrap();
                crate::http2::append_frame(
                    &mut self.send_buf,
                    TYPE_DATA,
                    if last { FLAG_END_STREAM } else { 0 },
                    stream_id,
                    &s.pending_body[start..start + n],
                );
            }
            if last {
                // Release the buffer as soon as it is owed nothing; a 5 MiB
                // upload should not sit in memory until the response lands.
                if let Some(s) = self.streams.get_mut(&stream_id) {
                    s.pending_body = Vec::new();
                    s.body_off = 0;
                }
                return;
            }
        }
    }

    /// Resume every stream that still owes the peer a body.
    ///
    /// Used after connection-level credit or the initial window size changes,
    /// where the credit is not attributable to one stream.
    fn pump_all(&mut self) {
        let ids: Vec<u32> = self
            .streams
            .iter()
            .filter(|(_, s)| s.body_off < s.pending_body.len())
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            self.pump_stream(id);
        }
    }

    /// Drive I/O: flush send buffer, read incoming data, parse frames.
    ///
    /// Must be called on every event-loop iteration.
    pub fn drive(&mut self) {
        if self.is_dead {
            return;
        }

        // ── Flush send buffer ─────────────────────────────────────────────────
        if let Err(reason) = self.transport.flush_send(&mut self.send_buf) {
            self.mark_dead(&reason);
            return;
        }

        // ── Read incoming data ────────────────────────────────────────────────
        //
        // Defer EOF until the frame loop below has run: a peer may send a
        // complete response and then close, and answer-then-close is legal.
        // Parse what already arrived first; the connection is still dead
        // afterwards.
        let eof = self.transport.fill_recv(&mut self.recv_buf);

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

            // Move the receive buffer out of `self` so the payload can be
            // borrowed as a slice while the frame handlers borrow `self`
            // mutably. The frame is drained and the remainder put back after
            // the frame is handled; a connection killed mid-frame never needs
            // its buffer again.
            let mut buf = std::mem::take(&mut self.recv_buf);
            let payload = &buf[FRAME_HDR..FRAME_HDR + length];

            // CONTINUATION protocol: if we're accumulating a block and the
            // incoming frame is NOT a CONTINUATION on that stream, that's a
            // protocol error.
            if let Some(cont_sid) = self.continuation_stream {
                if ftype != TYPE_CONTINUATION || stream_id != cont_sid {
                    self.mark_dead(&format!("{}: expected CONTINUATION frame", T::prefix()));
                    return;
                }
            }

            match ftype {
                TYPE_SETTINGS if flags & FLAG_ACK == 0 => {
                    // Parse settings pairs (6 bytes each)
                    let mut pos = 0usize;
                    while pos + 6 <= payload.len() {
                        let id = ((payload[pos] as u16) << 8) | (payload[pos + 1] as u16);
                        let val = ((payload[pos + 2] as u32) << 24)
                            | ((payload[pos + 3] as u32) << 16)
                            | ((payload[pos + 4] as u32) << 8)
                            | (payload[pos + 5] as u32);
                        match id {
                            // RFC 9113 6.9.2: a change to INITIAL_WINDOW_SIZE
                            // applies retroactively to every open stream, by
                            // the delta.
                            4 if val <= i32::MAX as u32 => {
                                let delta = val as i32 - self.peer_initial_window;
                                self.peer_initial_window = val as i32;
                                if delta != 0 {
                                    for s in self.streams.values_mut() {
                                        s.send_window = s.send_window.saturating_add(delta);
                                    }
                                }
                            }
                            4 => {
                                self.mark_dead(&format!(
                                    "{}: INITIAL_WINDOW_SIZE above 2^31-1",
                                    T::prefix()
                                ));
                                return;
                            }
                            5 if (16_384..=16_777_215).contains(&val) => self.peer_max_frame = val,
                            _ => {}
                        }
                        pos += 6;
                    }
                    // Send SETTINGS ACK
                    self.push_frame(TYPE_SETTINGS, FLAG_ACK, 0, &[]);
                    // A raised window, or a raised max frame size, may have
                    // unblocked a body that is mid-flight.
                    self.pump_all();
                }

                TYPE_HEADERS if stream_id > 0 => {
                    // Strip PADDED prefix and trailing padding by narrowing the
                    // slice rather than copying into a mutable Vec.
                    let mut pos = 0usize;
                    let mut end = payload.len();
                    if flags & FLAG_PADDED != 0 {
                        if payload.is_empty() {
                            self.mark_dead(&format!(
                                "{}: padded HEADERS with empty payload",
                                T::prefix()
                            ));
                            return;
                        }
                        let pad_len = payload[0] as usize;
                        pos = 1;
                        if payload.len() < pos + pad_len {
                            self.mark_dead(&format!(
                                "{}: HEADERS pad_len exceeds payload",
                                T::prefix()
                            ));
                            return;
                        }
                        end = payload.len() - pad_len;
                    }

                    // Skip PRIORITY fields (5 bytes)
                    if flags & FLAG_PRIORITY != 0 {
                        pos += 5;
                    }

                    if pos <= end {
                        self.header_block_acc.extend_from_slice(&payload[pos..end]);
                    }

                    if flags & FLAG_END_HEADERS != 0 {
                        if !self.finish_header_block(stream_id) {
                            return;
                        }
                    } else {
                        self.continuation_stream = Some(stream_id);
                    }

                    if flags & FLAG_END_STREAM != 0 {
                        self.complete_stream(stream_id);
                    }
                }

                TYPE_CONTINUATION if stream_id > 0 => {
                    self.header_block_acc.extend_from_slice(payload);

                    if flags & FLAG_END_HEADERS != 0 && !self.finish_header_block(stream_id) {
                        return;
                    }
                }

                TYPE_DATA if stream_id > 0 => {
                    let mut pos = 0usize;
                    let mut end = payload.len();

                    if flags & FLAG_PADDED != 0 {
                        if payload.is_empty() {
                            self.mark_dead(&format!(
                                "{}: padded DATA with empty payload",
                                T::prefix()
                            ));
                            return;
                        }
                        let pad_len = payload[0] as usize;
                        pos = 1;
                        if payload.len() < pos + pad_len {
                            self.mark_dead(&format!(
                                "{}: DATA pad_len exceeds payload",
                                T::prefix()
                            ));
                            return;
                        }
                        end = payload.len() - pad_len;
                    }

                    let data = &payload[pos..end];
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
                            format!("{}: stream reset by server", T::prefix()),
                        )));
                    }
                }

                TYPE_GOAWAY => {
                    self.mark_dead(&format!("{}: server sent GOAWAY", T::prefix()));
                    return;
                }

                TYPE_PING if flags & FLAG_ACK == 0 && payload.len() == 8 => {
                    self.push_frame(TYPE_PING, FLAG_ACK, 0, payload);
                }

                TYPE_WINDOW_UPDATE if stream_id == 0 && payload.len() >= 4 => {
                    // The reserved high bit is ignored (RFC 9113 6.9).
                    let inc = (((payload[0] as u32) << 24)
                        | ((payload[1] as u32) << 16)
                        | ((payload[2] as u32) << 8)
                        | (payload[3] as u32))
                        & 0x7FFF_FFFF;
                    if inc == 0 {
                        self.mark_dead(&format!(
                            "{}: WINDOW_UPDATE increment of 0 on stream 0",
                            T::prefix()
                        ));
                        return;
                    }
                    // 6.9.1: a window above 2^31-1 is a FLOW_CONTROL_ERROR,
                    // not a wrap.
                    match self.conn_send_window.checked_add(inc as i32) {
                        Some(w) => self.conn_send_window = w,
                        None => {
                            self.mark_dead(&format!(
                                "{}: connection send window above 2^31-1",
                                T::prefix()
                            ));
                            return;
                        }
                    }
                    self.pump_all();
                }

                // Per-stream credit: grant it to that stream and resume its
                // body if it was out of credit.
                TYPE_WINDOW_UPDATE if stream_id > 0 && payload.len() >= 4 => {
                    let inc = (((payload[0] as u32) << 24)
                        | ((payload[1] as u32) << 16)
                        | ((payload[2] as u32) << 8)
                        | (payload[3] as u32))
                        & 0x7FFF_FFFF;
                    if inc == 0 {
                        // A zero increment is a stream error, not a
                        // connection one (6.9).
                        self.push_frame(
                            TYPE_RST_STREAM,
                            0,
                            stream_id,
                            &1u32.to_be_bytes(), // PROTOCOL_ERROR
                        );
                        self.streams.remove(&stream_id);
                    } else if let Some(s) = self.streams.get_mut(&stream_id) {
                        match s.send_window.checked_add(inc as i32) {
                            Some(w) => {
                                s.send_window = w;
                                self.pump_stream(stream_id);
                            }
                            None => {
                                self.push_frame(
                                    TYPE_RST_STREAM,
                                    0,
                                    stream_id,
                                    &3u32.to_be_bytes(), // FLOW_CONTROL_ERROR
                                );
                                self.streams.remove(&stream_id);
                            }
                        }
                    }
                }

                _ => {
                    // Ignore all other frame types
                }
            }

            // The frame has been consumed; drain it and put the remainder back
            // so the next iteration reads the following frame.
            buf.drain(..FRAME_HDR + length);
            self.recv_buf = buf;
        }

        // Now that everything already received has been parsed and delivered,
        // honour the EOF. Streams still open at this point genuinely lost their
        // response.
        if let Some(reason) = eof {
            self.mark_dead(&reason);
        }
    }

    /// Decode the accumulated header block and apply it to its stream.
    ///
    /// Returns false when the connection is dead and the caller must stop.
    fn finish_header_block(&mut self, stream_id: u32) -> bool {
        let block = std::mem::take(&mut self.header_block_acc);
        let pairs = match self.hpack_dec.decode(&block) {
            Ok(pairs) => pairs,
            Err(e) => {
                self.mark_dead(&format!("{}: HPACK decode error: {:?}", T::prefix(), e));
                return false;
            }
        };
        self.continuation_stream = None;
        let Some(s) = self.streams.get_mut(&stream_id) else {
            return true;
        };
        match apply_response_block(
            pairs,
            &mut s.resp_status,
            &mut s.resp_headers,
            &mut s.informational,
        ) {
            ResponseBlock::Final => s.headers_done = true,
            ResponseBlock::Informational => {}
            ResponseBlock::TooManyInformational => {
                // Fail this stream only. The connection is healthy.
                if let Some(s) = self.streams.remove(&stream_id) {
                    let _ = s.tx.send(Err(too_many_informational()));
                }
                self.push_frame(TYPE_RST_STREAM, 0, stream_id, &8u32.to_be_bytes());
            }
        }
        true
    }

    /// Complete a stream: remove it and send the response (or an error) on tx.
    fn complete_stream(&mut self, stream_id: u32) {
        if let Some(s) = self.streams.remove(&stream_id) {
            // A peer may answer before we have finished sending (an early error
            // response is the common case). The request is then abandoned
            // mid-body, so tell the peer rather than leaving a half-open stream
            // waiting on DATA that will never arrive.
            if s.body_off < s.pending_body.len() {
                self.push_frame(
                    TYPE_RST_STREAM,
                    0,
                    stream_id,
                    &8u32.to_be_bytes(), // CANCEL
                );
            }
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
                    format!("{}: stream ended without headers", T::prefix()),
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
        crate::http2::append_frame(&mut self.send_buf, ftype, flags, stream_id, payload);
    }
}

// ── Pool (public) ─────────────────────────────────────────────────────────────

pub struct Pool<T: Transport> {
    /// backend base_url → connection
    connections: HashMap<String, H2Client<T>>,
}

impl<T: Transport> Default for Pool<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: Transport> Pool<T> {
    pub fn new() -> Self {
        Self {
            connections: HashMap::new(),
        }
    }

    /// Ensure a connection exists for `base_url`, creating one if none exists
    /// or the existing one is dead. Returns whether one was created.
    pub fn ensure_connection(
        &mut self,
        base_url: &str,
        tls: Option<Arc<rustls::ClientConfig>>,
    ) -> io::Result<bool> {
        let (host, port) = parse_host_port(base_url, T::prefix(), T::default_port())?;
        let needs_new = match self.connections.get(base_url) {
            None => true,
            Some(conn) => conn.is_dead && conn.streams.is_empty(),
        };
        if needs_new {
            let new_conn = H2Client::connect(&host, port, tls)?;
            self.connections.insert(base_url.to_string(), new_conn);
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// The connection for `base_url`, to register and set its token.
    pub fn connection_mut(&mut self, base_url: &str) -> Option<&mut H2Client<T>> {
        self.connections.get_mut(base_url)
    }

    /// Dispatch a request on the existing connection. `registry` lets the call
    /// ask for write readiness when the request goes into the send buffer.
    pub fn dispatch(
        &mut self,
        base_url: &str,
        req: &HttpRequest,
        client_ip: &str,
        original_host: &str,
        registry: &Registry,
    ) -> io::Result<mpsc::Receiver<io::Result<HttpResponse>>> {
        let (host, _) = parse_host_port(base_url, T::prefix(), T::default_port())?;
        let conn = self.connections.get_mut(base_url).unwrap();
        let rx = conn.dispatch(req, &host, client_ip, original_host)?;
        let interest = conn.interest();
        if interest != conn.registered_interest {
            registry
                .reregister(
                    &mut m6_core::mio::unix::SourceFd(&conn.raw_fd()),
                    conn.token(),
                    interest,
                )
                .ok();
            conn.registered_interest = interest;
        }
        Ok(rx)
    }

    /// Drive the one connection whose token fired. Returns whether it is dead
    /// with no pending streams, so the caller can free its token and remove it.
    pub fn drive_one(&mut self, token: Token, registry: &Registry) -> bool {
        let Some(conn) = self.connections.values_mut().find(|c| c.token() == token) else {
            return false;
        };
        conn.drive();
        let interest = conn.interest();
        if interest != conn.registered_interest {
            registry
                .reregister(
                    &mut m6_core::mio::unix::SourceFd(&conn.raw_fd()),
                    conn.token(),
                    interest,
                )
                .ok();
            conn.registered_interest = interest;
        }
        conn.is_dead && conn.streams.is_empty()
    }

    /// Deregister and drop the connection registered under `token`, if any.
    pub fn remove_by_token(&mut self, token: Token, registry: &Registry) {
        let url = self
            .connections
            .iter()
            .find(|(_, c)| c.token() == token)
            .map(|(k, _)| k.clone());
        if let Some(url) = url {
            if let Some(conn) = self.connections.remove(&url) {
                registry
                    .deregister(&mut m6_core::mio::unix::SourceFd(&conn.raw_fd()))
                    .ok();
            }
        }
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Build the client preface + SETTINGS frame (INITIAL_WINDOW_SIZE = 1 MiB).
pub(crate) fn client_init_frame() -> Vec<u8> {
    let mut init_buf: Vec<u8> = Vec::with_capacity(CLIENT_PREFACE.len() + FRAME_HDR + 6);
    init_buf.extend_from_slice(CLIENT_PREFACE);

    // SETTINGS frame: one setting, id=4 (INITIAL_WINDOW_SIZE), value=1_048_576
    let settings_payload: &[u8] = &[
        0x00, 0x04, // setting id = 4
        0x00, 0x10, 0x00, 0x00, // value = 1_048_576 (0x00100000)
    ];
    let len = settings_payload.len() as u32;
    init_buf.push((len >> 16) as u8);
    init_buf.push((len >> 8) as u8);
    init_buf.push(len as u8);
    init_buf.push(TYPE_SETTINGS);
    init_buf.push(0); // flags
    init_buf.extend_from_slice(&[0, 0, 0, 0]); // stream_id = 0
    init_buf.extend_from_slice(settings_payload);
    init_buf
}

/// Parse `scheme://host:port[/path]` into `(host, port)`.
fn parse_host_port(base_url: &str, scheme: &str, default_port: u16) -> io::Result<(String, u16)> {
    let rest = base_url
        .strip_prefix(scheme)
        .and_then(|r| r.strip_prefix("://"))
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("not an {scheme}:// URL"),
            )
        })?;

    // Strip any path suffix
    let authority = match rest.find('/') {
        Some(idx) => &rest[..idx],
        None => rest,
    };

    // IPv6: [::1]:port or [::1]
    if let Some(bracket_end) = authority.find(']') {
        let host = authority[1..bracket_end].to_string();
        let port =
            if bracket_end + 1 < authority.len() && authority.as_bytes()[bracket_end + 1] == b':' {
                authority[bracket_end + 2..].parse::<u16>().map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("invalid port in {scheme} URL"),
                    )
                })?
            } else {
                default_port
            };
        return Ok((host, port));
    }

    // host:port or host
    if let Some(colon) = authority.rfind(':') {
        if let Ok(p) = authority[colon + 1..].parse::<u16>() {
            return Ok((authority[..colon].to_string(), p));
        }
    }

    Ok((authority.to_string(), default_port))
}
