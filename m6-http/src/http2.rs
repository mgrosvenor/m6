/// Sans-I/O HTTP/2 server connection driver.
///
/// `Http2Conn` holds only the HTTP/2 protocol state — it does NOT own the
/// TcpStream or the rustls ServerConnection.  Those live in the outer `Conn`
/// struct in http11.rs and are passed by reference to every method that needs
/// I/O.  This makes ALPN-based promotion from HTTP/1.1 trivial: the outer Conn
/// simply swaps its `ConnKind` from `Handshake` to `Http2`.
///
/// Frame format (RFC 9113 §4.1):
///   [length: u24][type: u8][flags: u8][stream_id: u31][payload: length bytes]

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::time::Instant;

use crate::forward::{HttpRequest, HttpResponse, PendingUrlContext};
use crate::http11::RequestOutcome;

/// I/O abstraction: TLS (HTTPS) or plain TCP (H2C over WireGuard).
pub enum H2Io<'a> {
    Tls { tls: &'a mut rustls::ServerConnection, stream: &'a std::net::TcpStream },
    Plain { stream: &'a std::net::TcpStream },
}

// ── Frame type constants ──────────────────────────────────────────────────────

const TYPE_DATA:          u8 = 0x0;
const TYPE_HEADERS:       u8 = 0x1;
const TYPE_PRIORITY:      u8 = 0x2;
const TYPE_RST_STREAM:    u8 = 0x3;
const TYPE_SETTINGS:      u8 = 0x4;
const TYPE_PUSH_PROMISE:  u8 = 0x5;
const TYPE_PING:          u8 = 0x6;
const TYPE_GOAWAY:        u8 = 0x7;
const TYPE_WINDOW_UPDATE: u8 = 0x8;
const TYPE_CONTINUATION:  u8 = 0x9;

const FLAG_END_STREAM:  u8 = 0x1;
const FLAG_END_HEADERS: u8 = 0x4;
const FLAG_PADDED:      u8 = 0x8;
const FLAG_PRIORITY:    u8 = 0x20;
const FLAG_ACK:         u8 = 0x1;

const SETTING_HEADER_TABLE_SIZE:      u16 = 0x1;
const SETTING_ENABLE_PUSH:            u16 = 0x2;
const SETTING_MAX_CONCURRENT_STREAMS: u16 = 0x3;
const SETTING_INITIAL_WINDOW_SIZE:    u16 = 0x4;
const SETTING_MAX_FRAME_SIZE:         u16 = 0x5;

const CLIENT_PREFACE: &[u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";
const FRAME_HDR:      usize = 9;

const DEFAULT_WINDOW:       u32 = 65_535;
const DEFAULT_MAX_FRAME:    u32 = 16_384;
const MAX_CONCURRENT:       u32 = 100;

const ERR_NO_ERROR:       u32 = 0x0;
const ERR_PROTOCOL_ERROR: u32 = 0x1;
const ERR_STREAM_CLOSED:  u32 = 0x5;
const ERR_REFUSED_STREAM: u32 = 0x7;

// ── Stream state ──────────────────────────────────────────────────────────────

#[derive(Debug, PartialEq)]
enum StreamState { Open, HalfClosedRemote, Closed }

struct H2Stream {
    state:        StreamState,
    headers:      Vec<(String, String)>,
    body:         Vec<u8>,
    headers_done: bool,
    send_window:  i32,
    // Buffered response body for flow-controlled delivery.
    // None  = request not yet dispatched.
    // Some  = response queued; resp_sent bytes already flushed.
    resp_body:    Option<Vec<u8>>,
    resp_sent:    usize,
    /// Pending URL-backend request: set by maybe_dispatch when backend is async.
    pending_rx:   Option<(std::sync::mpsc::Receiver<std::io::Result<HttpResponse>>, PendingUrlContext)>,
}

impl H2Stream {
    fn new(initial_send_window: i32) -> Self {
        H2Stream {
            state: StreamState::Open,
            headers: Vec::new(),
            body: Vec::new(),
            headers_done: false,
            send_window: initial_send_window,
            resp_body: None,
            resp_sent: 0,
            pending_rx: None,
        }
    }
}

// ── Connection phase ──────────────────────────────────────────────────────────

#[derive(PartialEq)]
enum Phase { Preface, Active, GoingAway, Done }

// ── Public type ───────────────────────────────────────────────────────────────

pub struct Http2Conn {
    phase:    Phase,
    recv_buf: Vec<u8>,
    send_buf: Vec<u8>,
    /// Updated on every received frame; drives the idle timeout.
    last_active: Instant,

    streams:       HashMap<u32, H2Stream>,
    hpack_dec:     hpack::Decoder<'static>,
    hpack_enc:     hpack::Encoder<'static>,

    last_stream_id:         u32,
    continuation_stream_id: Option<u32>,
    header_block_buf:       Vec<u8>,

    peer_initial_window: i32,
    peer_max_frame:      u32,
    conn_recv_window:    i32,
    conn_send_window:    i32,

    /// Next server-initiated (push) stream ID.  Server-initiated streams are
    /// even-numbered; starts at 2, incremented by 2 per push.
    next_push_id: u32,
    /// False when the client sends SETTINGS_ENABLE_PUSH=0.
    enable_push:  bool,
}

impl Http2Conn {
    pub fn new() -> Self {
        Http2Conn {
            phase:    Phase::Preface,
            recv_buf: Vec::with_capacity(16_384),
            send_buf: Vec::with_capacity(16_384),
            last_active: Instant::now(),
            streams:  HashMap::new(),
            hpack_dec: hpack::Decoder::new(),
            hpack_enc: hpack::Encoder::new(),
            last_stream_id: 0,
            continuation_stream_id: None,
            header_block_buf: Vec::new(),
            peer_initial_window: DEFAULT_WINDOW as i32,
            peer_max_frame:      DEFAULT_MAX_FRAME,
            conn_recv_window:    DEFAULT_WINDOW as i32,
            conn_send_window:    DEFAULT_WINDOW as i32,
            next_push_id: 2,
            enable_push:  true,
        }
    }

    pub fn is_done(&self) -> bool { self.phase == Phase::Done }

    /// Drive one step: pump I/O, parse frames, dispatch requests.
    pub fn drive<F, G>(
        &mut self,
        mut io:      H2Io<'_>,
        client_ip:   &str,
        on_request:  &mut F,
        on_response: &mut G,
    )
    where
        F: FnMut(&HttpRequest, &str) -> RequestOutcome,
        G: FnMut(std::io::Result<HttpResponse>, &PendingUrlContext)
               -> (u16, Vec<(String, String)>, Vec<u8>, String, std::sync::Arc<Vec<String>>),
    {
        if self.phase == Phase::Done { return; }

        // Poll any pending URL-backend receivers before processing new frames.
        self.poll_pending_url(on_response);

        // Idle timeout: kill only if no frames have arrived for H2_IDLE_TIMEOUT_SECS.
        // H2 connections are long-lived (reused across many requests), so we
        // reset the clock on every received frame rather than from creation time.
        if self.last_active.elapsed().as_secs() > crate::http11::H2_IDLE_TIMEOUT_SECS {
            self.send_goaway(ERR_NO_ERROR);
            self.flush_io(&mut io).ok();
            self.phase = Phase::Done;
            return;
        }

        if let Err(e) = self.fill_recv(&mut io) {
            tracing::trace!("h2 fill_recv: {e}");
            self.phase = Phase::Done;
            return;
        }

        loop {
            match self.process_frame(on_request, client_ip) {
                Ok(true)  => {}
                Ok(false) => break,
                Err(e)    => {
                    tracing::warn!("http2 error: {e}");
                    self.send_goaway(ERR_PROTOCOL_ERROR);
                    self.phase = Phase::Done;
                    break;
                }
            }
        }

        // Flush any response data that became unblocked this iteration
        // (e.g. a WINDOW_UPDATE was processed inside the frame loop above).
        self.flush_pending_streams();

        if let Err(e) = self.flush_io(&mut io) {
            tracing::trace!("h2 flush_io: {e}");
            self.phase = Phase::Done;
        }

        if self.phase == Phase::GoingAway
            && self.streams.values().all(|s| s.state == StreamState::Closed)
        {
            self.phase = Phase::Done;
        }
    }

    // ── TLS I/O ───────────────────────────────────────────────────────────────

    fn fill_recv(&mut self, io: &mut H2Io<'_>) -> io::Result<()> {
        match io {
            H2Io::Tls { tls, stream } => {
                loop {
                    match tls.read_tls(&mut &**stream) {
                        Ok(0)  => return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "closed")),
                        Ok(_)  => { tls.process_new_packets().map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?; }
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                        Err(e) => return Err(e),
                    }
                }
                let mut tmp = [0u8; 8192];
                loop {
                    match tls.reader().read(&mut tmp) {
                        Ok(0)  => break,
                        Ok(n)  => {
                            self.recv_buf.extend_from_slice(&tmp[..n]);
                            self.last_active = Instant::now();
                        }
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                        Err(e) => return Err(e),
                    }
                }
                Ok(())
            }
            H2Io::Plain { stream } => {
                let mut tmp = [0u8; 8192];
                loop {
                    match stream.read(&mut tmp) {
                        Ok(0)  => return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "closed")),
                        Ok(n)  => { self.recv_buf.extend_from_slice(&tmp[..n]); self.last_active = Instant::now(); }
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                        Err(e) => return Err(e),
                    }
                }
                Ok(())
            }
        }
    }

    fn flush_io(&mut self, io: &mut H2Io<'_>) -> io::Result<()> {
        match io {
            H2Io::Tls { tls, stream } => {
                // Write send_buf to rustls in a loop, draining encrypted records to the
                // socket between each chunk.  rustls has a default 64 KB internal buffer
                // limit (DEFAULT_BUFFER_LIMIT); writing more than that in one shot causes
                // writer().write() to return Ok(0) and write_all to fail.  By draining
                // between chunks we keep the internal buffer well below the limit.
                let mut pos = 0;
                while pos < self.send_buf.len() {
                    // write() always returns Ok(n); n may be 0 if rustls buffer is full.
                    let n = tls.writer().write(&self.send_buf[pos..]).unwrap_or(0);
                    pos += n;

                    // Drain encrypted bytes to the socket before writing the next chunk.
                    let mut socket_full = false;
                    loop {
                        match tls.write_tls(&mut &**stream) {
                            Ok(0)  => break,
                            Ok(_)  => {}
                            Err(e) if e.kind() == io::ErrorKind::WouldBlock => { socket_full = true; break; }
                            Err(e) => { self.send_buf.drain(..pos); return Err(e); }
                        }
                    }

                    if n == 0 && socket_full {
                        // Both rustls buffer and TCP socket buffer are full; give up and
                        // retry on the next drive() call (100 ms at most).
                        break;
                    }
                }
                self.send_buf.drain(..pos);

                // Final drain of any remaining encrypted records.
                loop {
                    match tls.write_tls(&mut &**stream) {
                        Ok(0)  => break,
                        Ok(_)  => {}
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                        Err(e) => return Err(e),
                    }
                }
                Ok(())
            }
            H2Io::Plain { stream } => {
                use std::io::Write;
                let mut pos = 0;
                while pos < self.send_buf.len() {
                    match stream.write(&self.send_buf[pos..]) {
                        Ok(0) => break,
                        Ok(n) => { pos += n; }
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                        Err(e) => { self.send_buf.drain(..pos); return Err(e); }
                    }
                }
                self.send_buf.drain(..pos);
                Ok(())
            }
        }
    }

    // ── Frame dispatch ────────────────────────────────────────────────────────

    fn process_frame<F>(
        &mut self,
        on_request: &mut F,
        client_ip: &str,
    ) -> Result<bool, &'static str>
    where
        F: FnMut(&HttpRequest, &str) -> RequestOutcome,
    {
        if self.phase == Phase::Preface {
            if self.recv_buf.len() < CLIENT_PREFACE.len() { return Ok(false); }
            if !self.recv_buf.starts_with(CLIENT_PREFACE) { return Err("bad connection preface"); }
            self.recv_buf.drain(..CLIENT_PREFACE.len());
            self.phase = Phase::Active;
            self.send_server_settings();
        }

        if self.recv_buf.len() < FRAME_HDR { return Ok(false); }

        let length    = u24_be(&self.recv_buf[0..3]) as usize;
        let ftype     = self.recv_buf[3];
        let flags     = self.recv_buf[4];
        let stream_id = u32::from_be_bytes(self.recv_buf[5..9].try_into().unwrap()) & 0x7fff_ffff;

        if self.recv_buf.len() < FRAME_HDR + length { return Ok(false); }

        let payload: Vec<u8> = self.recv_buf[FRAME_HDR..FRAME_HDR + length].to_vec();
        self.recv_buf.drain(..FRAME_HDR + length);

        if let Some(cont) = self.continuation_stream_id {
            if ftype != TYPE_CONTINUATION || stream_id != cont {
                return Err("expected CONTINUATION");
            }
        }

        match ftype {
            TYPE_DATA          => self.handle_data(stream_id, flags, &payload, on_request, client_ip)?,
            TYPE_HEADERS       => self.handle_headers(stream_id, flags, &payload, on_request, client_ip)?,
            TYPE_PRIORITY      => {}
            TYPE_RST_STREAM    => { self.streams.remove(&stream_id); }
            TYPE_SETTINGS      => self.handle_settings(flags, &payload)?,
            TYPE_PUSH_PROMISE  => return Err("client sent PUSH_PROMISE"),
            TYPE_PING          => self.handle_ping(flags, &payload),
            TYPE_GOAWAY        => { self.phase = Phase::GoingAway; }
            TYPE_WINDOW_UPDATE => self.handle_window_update(stream_id, &payload)?,
            TYPE_CONTINUATION  => self.handle_continuation(stream_id, flags, &payload, on_request, client_ip)?,
            _                  => {}
        }
        Ok(true)
    }

    // ── Frame handlers ────────────────────────────────────────────────────────

    fn handle_settings(&mut self, flags: u8, payload: &[u8]) -> Result<(), &'static str> {
        if flags & FLAG_ACK != 0 { return Ok(()); }
        if payload.len() % 6 != 0 { return Err("SETTINGS payload not multiple of 6"); }
        let mut i = 0;
        while i + 6 <= payload.len() {
            let id  = u16::from_be_bytes(payload[i..i+2].try_into().unwrap());
            let val = u32::from_be_bytes(payload[i+2..i+6].try_into().unwrap());
            match id {
                SETTING_HEADER_TABLE_SIZE => { self.hpack_dec.set_max_table_size(val as usize); }
                SETTING_ENABLE_PUSH       => {
                    if val > 1 { return Err("invalid ENABLE_PUSH"); }
                    self.enable_push = val == 1;
                }
                SETTING_INITIAL_WINDOW_SIZE => {
                    if val > 0x7fff_ffff { return Err("INITIAL_WINDOW_SIZE overflow"); }
                    let delta = val as i32 - self.peer_initial_window;
                    self.peer_initial_window = val as i32;
                    for s in self.streams.values_mut() { s.send_window += delta; }
                }
                SETTING_MAX_FRAME_SIZE => {
                    if !(16_384..=16_777_215).contains(&val) { return Err("invalid MAX_FRAME_SIZE"); }
                    self.peer_max_frame = val;
                }
                SETTING_MAX_CONCURRENT_STREAMS | _ => {}
            }
            i += 6;
        }
        self.push_frame(TYPE_SETTINGS, FLAG_ACK, 0, &[]);
        Ok(())
    }

    fn handle_ping(&mut self, flags: u8, payload: &[u8]) {
        if flags & FLAG_ACK == 0 && payload.len() == 8 {
            self.push_frame(TYPE_PING, FLAG_ACK, 0, payload);
        }
    }

    fn handle_window_update(&mut self, stream_id: u32, payload: &[u8]) -> Result<(), &'static str> {
        if payload.len() < 4 { return Err("WINDOW_UPDATE too short"); }
        let inc = u32::from_be_bytes(payload[0..4].try_into().unwrap()) & 0x7fff_ffff;
        if inc == 0 { return Err("zero WINDOW_UPDATE increment"); }
        if stream_id == 0 {
            self.conn_send_window += inc as i32;
        } else if let Some(s) = self.streams.get_mut(&stream_id) {
            s.send_window += inc as i32;
        }
        // A larger window may unblock pending response data.
        self.flush_pending_streams();
        Ok(())
    }

    fn handle_headers<F>(
        &mut self, stream_id: u32, flags: u8, payload: &[u8],
        on_request: &mut F, client_ip: &str,
    ) -> Result<(), &'static str>
    where
        F: FnMut(&HttpRequest, &str) -> RequestOutcome,
    {
        if stream_id == 0 { return Err("HEADERS on stream 0"); }
        if stream_id % 2 == 0 { return Err("client used even stream ID"); }
        if stream_id <= self.last_stream_id && !self.streams.contains_key(&stream_id) {
            self.push_frame(TYPE_RST_STREAM, 0, stream_id, &ERR_STREAM_CLOSED.to_be_bytes());
            return Ok(());
        }
        if self.streams.len() >= MAX_CONCURRENT as usize {
            self.push_frame(TYPE_RST_STREAM, 0, stream_id, &ERR_REFUSED_STREAM.to_be_bytes());
            return Ok(());
        }
        self.last_stream_id = self.last_stream_id.max(stream_id);

        let mut pos = 0;
        if flags & FLAG_PADDED != 0 {
            if payload.is_empty() { return Err("HEADERS: missing pad length"); }
            let pad = payload[0] as usize;
            pos = 1;
            if pad >= payload.len() - pos { return Err("HEADERS: excess padding"); }
        }
        if flags & FLAG_PRIORITY != 0 { pos += 5; }
        let header_block = &payload[pos..];

        let stream = self.streams.entry(stream_id)
            .or_insert_with(|| H2Stream::new(self.peer_initial_window));
        if flags & FLAG_END_STREAM != 0 { stream.state = StreamState::HalfClosedRemote; }

        if flags & FLAG_END_HEADERS != 0 {
            let mut combined = self.header_block_buf.clone();
            combined.extend_from_slice(header_block);
            self.header_block_buf.clear();
            self.continuation_stream_id = None;
            let dec = &mut self.hpack_dec;
            let stream = self.streams.get_mut(&stream_id).unwrap();
            decode_hpack(dec, &combined, &mut stream.headers)?;
            stream.headers_done = true;
        } else {
            self.header_block_buf.extend_from_slice(header_block);
            self.continuation_stream_id = Some(stream_id);
        }

        self.maybe_dispatch(stream_id, on_request, client_ip);
        Ok(())
    }

    fn handle_continuation<F>(
        &mut self, stream_id: u32, flags: u8, payload: &[u8],
        on_request: &mut F, client_ip: &str,
    ) -> Result<(), &'static str>
    where
        F: FnMut(&HttpRequest, &str) -> RequestOutcome,
    {
        self.header_block_buf.extend_from_slice(payload);
        if flags & FLAG_END_HEADERS != 0 {
            self.continuation_stream_id = None;
            let all = self.header_block_buf.clone();
            self.header_block_buf.clear();
            if let Some(stream) = self.streams.get_mut(&stream_id) {
                let dec = &mut self.hpack_dec;
                // Can't call decode_hpack with self.hpack_dec while stream borrowed.
                // Decode into a temp vec and extend.
                let mut tmp = Vec::new();
                decode_hpack(dec, &all, &mut tmp)?;
                stream.headers.extend(tmp);
                stream.headers_done = true;
            }
            self.maybe_dispatch(stream_id, on_request, client_ip);
        }
        Ok(())
    }

    fn handle_data<F>(
        &mut self, stream_id: u32, flags: u8, payload: &[u8],
        on_request: &mut F, client_ip: &str,
    ) -> Result<(), &'static str>
    where
        F: FnMut(&HttpRequest, &str) -> RequestOutcome,
    {
        if stream_id == 0 { return Err("DATA on stream 0"); }
        let data = if flags & FLAG_PADDED != 0 && !payload.is_empty() {
            let pad = payload[0] as usize;
            if pad >= payload.len() { return Err("DATA: excess padding"); }
            &payload[1..payload.len() - pad]
        } else {
            payload
        };
        let data_len = data.len() as i32;

        self.conn_recv_window -= data_len;
        if self.conn_recv_window < 0 { return Err("connection flow control exceeded"); }
        if self.conn_recv_window < DEFAULT_WINDOW as i32 / 2 {
            let inc = DEFAULT_WINDOW as i32 - self.conn_recv_window;
            self.conn_recv_window += inc;
            self.push_window_update(0, inc as u32);
        }

        // Update stream state, then drop borrow before calling push_window_update / maybe_dispatch.
        let should_dispatch = if let Some(s) = self.streams.get_mut(&stream_id) {
            s.body.extend_from_slice(data);
            if flags & FLAG_END_STREAM != 0 {
                s.state = StreamState::HalfClosedRemote;
            }
            flags & FLAG_END_STREAM != 0 && s.headers_done
        } else {
            false
        };
        self.push_window_update(stream_id, data_len as u32);

        if should_dispatch {
            self.maybe_dispatch(stream_id, on_request, client_ip);
        }
        Ok(())
    }

    // ── Request dispatch ──────────────────────────────────────────────────────

    fn maybe_dispatch<F>(&mut self, stream_id: u32, on_request: &mut F, client_ip: &str)
    where
        F: FnMut(&HttpRequest, &str) -> RequestOutcome,
    {
        let ready = self.streams.get(&stream_id).map(|s| {
            s.headers_done
                && (s.state == StreamState::HalfClosedRemote
                    || is_headersonly(&s.headers))
                && s.state != StreamState::Closed
                && s.resp_body.is_none()  // not already dispatched
                && s.pending_rx.is_none() // not already waiting on async
        }).unwrap_or(false);

        if !ready { return; }

        // Clone what we need, then drop the immutable borrow.
        let (headers, body) = {
            let s = &self.streams[&stream_id];
            (s.headers.clone(), s.body.clone())
        };

        let req = build_request(&headers, body);

        match on_request(&req, client_ip) {
            RequestOutcome::Ready(status, resp_headers, resp_body, _, hints) => {
                self.dispatch_h2_response(stream_id, status, resp_headers, resp_body, hints, on_request, &client_ip);
            }
            RequestOutcome::Pending { rx, ctx } => {
                if let Some(s) = self.streams.get_mut(&stream_id) {
                    s.pending_rx = Some((rx, ctx));
                }
            }
        }
    }

    /// Complete a synchronous (Ready) H2 response: send server push, encode headers,
    /// store body, and flush.
    fn dispatch_h2_response<F>(
        &mut self,
        stream_id:    u32,
        status:       u16,
        resp_headers: Vec<(String, String)>,
        resp_body:    Vec<u8>,
        hints:        std::sync::Arc<Vec<String>>,
        on_request:   &mut F,
        client_ip:    &str,
    )
    where
        F: FnMut(&HttpRequest, &str) -> RequestOutcome,
    {
        if !hints.is_empty() {
            if self.enable_push {
                // ── HTTP/2 Server Push ─────────────────────────────────────
                // For each hinted asset that is already in the cache (returned
                // as a 2xx by on_request), send:
                //   1. PUSH_PROMISE on the request stream  (so the browser
                //      knows not to request it separately)
                //   2. HEADERS + DATA on a new server-initiated push stream
                //
                // Chrome 106+ removed push support; Firefox still honours it.
                // Browsers that don't support push will send RST_STREAM on the
                // push stream, which we ignore (the stream isn't in self.streams).
                for hint_url in hints.iter() {
                    // Bail early if the connection send window is exhausted.
                    if self.conn_send_window <= 0 { break; }

                    let push_req = HttpRequest {
                        method:  "GET".to_string(),
                        path:    hint_url.clone(),
                        query:   None,
                        version: "HTTP/2.0".to_string(),
                        headers: vec![],
                        body:    vec![],
                    };
                    let (ps, ph, pb) = match on_request(&push_req, client_ip) {
                        RequestOutcome::Ready(ps, ph, pb, _, _) => (ps, ph, pb),
                        RequestOutcome::Pending { .. } => continue, // can't push async assets
                    };
                    if ps < 200 || ps >= 300 { continue; }
                    // Skip if the body exceeds the current connection send window.
                    if pb.len() as i32 > self.conn_send_window { continue; }

                    let push_stream_id = self.next_push_id;
                    self.next_push_id += 2;

                    // PUSH_PROMISE on the request stream.
                    {
                        let promised_id_bytes = (push_stream_id & 0x7fff_ffff).to_be_bytes();
                        let hpack_req = self.hpack_enc.encode(vec![
                            (b":method".as_ref(), b"GET".as_ref()),
                            (b":path".as_ref(), hint_url.as_bytes()),
                            (b":scheme".as_ref(), b"https".as_ref()),
                        ]);
                        let mut promise_payload = promised_id_bytes.to_vec();
                        promise_payload.extend_from_slice(&hpack_req);
                        self.push_frame(TYPE_PUSH_PROMISE, FLAG_END_HEADERS, stream_id, &promise_payload);
                    }

                    // HEADERS on the push stream.
                    let push_hdr_block = self.encode_response_headers(ps, &ph, pb.len());
                    self.push_frame(TYPE_HEADERS, FLAG_END_HEADERS, push_stream_id, &push_hdr_block);

                    // DATA + END_STREAM on the push stream.
                    self.push_frame(TYPE_DATA, FLAG_END_STREAM, push_stream_id, &pb);
                    self.conn_send_window -= pb.len() as i32;
                }
            } else {
                // Push disabled by client — fall back to 103 Early Hints.
                let early_block = {
                    let mut pairs: Vec<(&[u8], &[u8])> = vec![(b":status", b"103")];
                    let link_values: Vec<String> = hints.iter()
                        .map(|u| crate::hints::link_header(u))
                        .collect();
                    for lv in &link_values {
                        pairs.push((b"link", lv.as_bytes()));
                    }
                    self.hpack_enc.encode(pairs)
                };
                self.push_frame(TYPE_HEADERS, FLAG_END_HEADERS, stream_id, &early_block);
            }
        }

        // Encode HPACK headers (needs &mut self.hpack_enc — no stream borrow active).
        let header_block = self.encode_response_headers(status, &resp_headers, resp_body.len());
        self.push_frame(TYPE_HEADERS, FLAG_END_HEADERS, stream_id, &header_block);

        // Store response body for flow-controlled delivery.
        if let Some(s) = self.streams.get_mut(&stream_id) {
            s.resp_body = Some(resp_body);
            s.resp_sent = 0;
        }

        // Flush as much as the current flow-control window allows.
        self.flush_pending_streams();
    }

    /// Poll all streams that have a pending URL-backend receiver.
    fn poll_pending_url<G>(&mut self, on_response: &mut G)
    where
        G: FnMut(std::io::Result<HttpResponse>, &PendingUrlContext)
               -> (u16, Vec<(String, String)>, Vec<u8>, String, std::sync::Arc<Vec<String>>),
    {
        use std::sync::mpsc::TryRecvError;
        let stream_ids: Vec<u32> = self.streams.keys().copied().collect();
        for sid in stream_ids {
            // Check without taking ownership first.
            // rx sends io::Result<HttpResponse>, so try_recv() gives Result<io::Result<HttpResponse>, TryRecvError>.
            let result: Option<std::io::Result<HttpResponse>> = match self.streams.get(&sid) {
                Some(s) => match &s.pending_rx {
                    Some((rx, _)) => match rx.try_recv() {
                        Ok(r) => Some(r),  // r is already io::Result<HttpResponse>
                        Err(TryRecvError::Empty) => None,
                        Err(TryRecvError::Disconnected) => Some(Err(std::io::Error::new(
                            std::io::ErrorKind::BrokenPipe, "url backend thread died",
                        ))),
                    },
                    None => None,
                },
                None => None,
            };
            if let Some(http_result) = result {
                // Move out the pending context.
                let ctx = self.streams.get_mut(&sid)
                    .and_then(|s| s.pending_rx.take())
                    .map(|(_, ctx)| ctx);
                if let Some(ctx) = ctx {
                    let (status, resp_headers, resp_body, _, _hints) = on_response(http_result, &ctx);
                    // No server push for async responses (hints only exist for cached assets which are Ready).
                    let header_block = self.encode_response_headers(status, &resp_headers, resp_body.len());
                    self.push_frame(TYPE_HEADERS, FLAG_END_HEADERS, sid, &header_block);
                    if let Some(s) = self.streams.get_mut(&sid) {
                        s.resp_body = Some(resp_body);
                        s.resp_sent = 0;
                    }
                    self.flush_pending_streams();
                }
            }
        }
    }

    // ── Flow-controlled response flusher ─────────────────────────────────────

    /// Send buffered response DATA frames for all streams, respecting both the
    /// connection-level and per-stream send windows.  Called after dispatching
    /// a request and after every WINDOW_UPDATE.
    fn flush_pending_streams(&mut self) {
        let stream_ids: Vec<u32> = self.streams.keys().copied().collect();
        let max = self.peer_max_frame as usize;

        'outer: for stream_id in stream_ids {
            loop {
                // Determine how many bytes we can send right now.
                let (to_send, is_last) = {
                    let s = match self.streams.get(&stream_id) {
                        Some(s) => s,
                        None    => continue 'outer,
                    };
                    let body = match &s.resp_body {
                        Some(b) => b,
                        None    => continue 'outer,  // not dispatched yet
                    };
                    let remaining = body.len() - s.resp_sent;
                    if remaining == 0 {
                        // Body fully consumed (or empty); send END_STREAM and reap.
                        (0usize, true)
                    } else {
                        if self.conn_send_window <= 0 || s.send_window <= 0 {
                            tracing::trace!(
                                "h2 stream {} blocked: conn_window={} stream_window={} remaining={}",
                                stream_id, self.conn_send_window, s.send_window, remaining
                            );
                            continue 'outer;  // blocked; wait for WINDOW_UPDATE
                        }
                        let window = (self.conn_send_window.min(s.send_window) as usize)
                            .min(max);
                        let n = remaining.min(window);
                        (n, s.resp_sent + n == body.len())
                    }
                };

                if to_send == 0 && is_last {
                    // Empty DATA + END_STREAM to close the stream.
                    self.push_frame(TYPE_DATA, FLAG_END_STREAM, stream_id, &[]);
                    self.streams.remove(&stream_id);
                    continue 'outer;
                }

                // Clone the chunk to satisfy the borrow checker.
                let chunk: Vec<u8> = {
                    let s = &self.streams[&stream_id];
                    let body = s.resp_body.as_ref().unwrap();
                    body[s.resp_sent..s.resp_sent + to_send].to_vec()
                };

                let flags = if is_last { FLAG_END_STREAM } else { 0 };
                self.push_frame(TYPE_DATA, flags, stream_id, &chunk);

                // Debit both windows.
                self.conn_send_window -= to_send as i32;
                let s = self.streams.get_mut(&stream_id).unwrap();
                s.send_window -= to_send as i32;
                s.resp_sent   += to_send;

                if is_last {
                    self.streams.remove(&stream_id);
                    continue 'outer;
                }
                // More data remains but the window may now be exhausted.
                if self.conn_send_window <= 0 { break; }
                let sw = self.streams.get(&stream_id).map(|s| s.send_window).unwrap_or(0);
                if sw <= 0 { break; }
            }
        }
    }

    // ── Frame encoding helpers ────────────────────────────────────────────────

    fn encode_response_headers(
        &mut self, status: u16,
        headers: &[(String, String)],
        body_len: usize,
    ) -> Vec<u8> {
        let status_str  = status.to_string();
        let bodylen_str = body_len.to_string();

        let mut pairs: Vec<(&[u8], &[u8])> = vec![
            (b":status",        status_str.as_bytes()),
            (b"content-length", bodylen_str.as_bytes()),
        ];
        let filtered: Vec<(String, String)> = headers.iter()
            .filter(|(k, _)| {
                let kl = k.to_lowercase();
                !matches!(kl.as_str(),
                    "connection" | "transfer-encoding" | "keep-alive" | "content-length")
            })
            .map(|(k, v)| (k.to_lowercase(), v.clone()))
            .collect();
        for (k, v) in &filtered {
            pairs.push((k.as_bytes(), v.as_bytes()));
        }
        self.hpack_enc.encode(pairs)
    }

    fn push_frame(&mut self, ftype: u8, flags: u8, stream_id: u32, payload: &[u8]) {
        let len = payload.len();
        self.send_buf.push((len >> 16) as u8);
        self.send_buf.push((len >> 8)  as u8);
        self.send_buf.push(len         as u8);
        self.send_buf.push(ftype);
        self.send_buf.push(flags);
        self.send_buf.extend_from_slice(&(stream_id & 0x7fff_ffff).to_be_bytes());
        self.send_buf.extend_from_slice(payload);
    }

    fn push_window_update(&mut self, stream_id: u32, inc: u32) {
        self.push_frame(TYPE_WINDOW_UPDATE, 0, stream_id, &(inc & 0x7fff_ffff).to_be_bytes());
    }

    fn send_server_settings(&mut self) {
        let mut p = Vec::with_capacity(12);
        setting_bytes(&mut p, SETTING_MAX_CONCURRENT_STREAMS, MAX_CONCURRENT);
        setting_bytes(&mut p, SETTING_INITIAL_WINDOW_SIZE,    1_048_576);
        self.push_frame(TYPE_SETTINGS, 0, 0, &p);
    }

    fn send_goaway(&mut self, code: u32) {
        let mut p = [0u8; 8];
        p[0..4].copy_from_slice(&(self.last_stream_id & 0x7fff_ffff).to_be_bytes());
        p[4..8].copy_from_slice(&code.to_be_bytes());
        self.push_frame(TYPE_GOAWAY, 0, 0, &p);
    }
}

// ── Utilities ─────────────────────────────────────────────────────────────────

fn u24_be(b: &[u8]) -> u32 {
    (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32
}

fn setting_bytes(buf: &mut Vec<u8>, id: u16, val: u32) {
    buf.extend_from_slice(&id.to_be_bytes());
    buf.extend_from_slice(&val.to_be_bytes());
}

fn decode_hpack(
    dec: &mut hpack::Decoder<'_>,
    block: &[u8],
    out: &mut Vec<(String, String)>,
) -> Result<(), &'static str> {
    let headers = dec.decode(block).map_err(|_| "HPACK decode error")?;
    for (k, v) in headers {
        out.push((
            String::from_utf8_lossy(&k).into_owned(),
            String::from_utf8_lossy(&v).into_owned(),
        ));
    }
    Ok(())
}

fn is_headersonly(headers: &[(String, String)]) -> bool {
    for (k, v) in headers {
        if k == ":method" {
            return matches!(v.as_str(), "GET" | "HEAD" | "DELETE" | "OPTIONS" | "CONNECT");
        }
    }
    false
}

fn build_request(headers: &[(String, String)], body: Vec<u8>) -> HttpRequest {
    let mut method = String::new();
    let mut path   = String::new();
    let mut query  = None;
    let mut fwd    = Vec::new();

    for (k, v) in headers {
        match k.as_str() {
            ":method"    => method = v.clone(),
            ":path"      => {
                if let Some(q) = v.find('?') {
                    path  = v[..q].to_string();
                    query = Some(v[q+1..].to_string());
                } else {
                    path = v.clone();
                }
            }
            k if k.starts_with(':') => {}
            _ => fwd.push((k.clone(), v.clone())),
        }
    }

    HttpRequest { method, path, query, version: "HTTP/2".to_string(), headers: fwd, body }
}
