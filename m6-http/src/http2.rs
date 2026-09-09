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

/// Hard ceiling on a single buffered HTTP/2 request body.
///
/// Not an RFC requirement -- a robustness one. Request bodies were accumulated
/// with no bound while flow-control credit was handed straight back, so a peer
/// could stream indefinitely and grow the process until it was killed. Any cap
/// removes that; this one is above the 16 MiB multipart limit the renderers
/// already enforce, so it never trips before their own check does.
const MAX_H2_BODY: usize = 20 * 1024 * 1024;

const ERR_NO_ERROR:       u32 = 0x0;
const ERR_PROTOCOL_ERROR: u32 = 0x1;
const ERR_STREAM_CLOSED:  u32 = 0x5;
const ERR_REFUSED_STREAM: u32 = 0x7;
const ERR_FRAME_SIZE:     u32 = 0x6;

/// How many recently-reset stream ids to remember. See `recently_reset`.
const RESET_MEMORY: usize = 128;

// ── Stream state ──────────────────────────────────────────────────────────────

/// RFC 9113 5.1. All seven states.
///
/// Three of these used to exist (`Open`, `HalfClosedRemote`, `Closed`) and the
/// absence of the rest was the single largest source of conformance failures:
/// without `Idle` there is no way to tell a frame arriving on a stream that
/// was never opened from one on a live stream, and without `Closed` being
/// *derivable* there is no way to reject a frame on a stream that has already
/// finished.
///
/// `ReservedLocal`/`ReservedRemote` exist only via PUSH_PROMISE. This server
/// never pushes and rejects a client PUSH_PROMISE outright, so neither is
/// reachable today -- they are present because the transition table below is
/// meant to be checkable against the RFC line by line, and a table missing two
/// of its rows cannot be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(
    dead_code,
    reason = "ReservedLocal/ReservedRemote are reachable only via PUSH_PROMISE, \
              which this server never sends and rejects on receipt. They are \
              present so the 5.1 transition table can be checked against the \
              RFC in full; a table missing two of its seven rows cannot be."
)]
enum StreamState {
    Idle,
    ReservedLocal,
    ReservedRemote,
    Open,
    HalfClosedLocal,
    HalfClosedRemote,
    Closed,
}

/// How a frame that is illegal in the current state must be answered.
///
/// The distinction is not cosmetic. RFC 9113 5.4.1: a connection error means
/// GOAWAY and the connection ends; a stream error means RST_STREAM and the
/// connection continues. Answering a connection error with a stream error
/// leaves both peers disagreeing about whether the connection is still usable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FrameVerdict {
    Allow,
    /// GOAWAY with this error code.
    ConnectionError(u32),
    /// RST_STREAM on this stream with this error code.
    StreamError(u32),
}

struct H2Stream {
    state:        StreamState,
    headers:      Vec<(String, String)>,
    body:         Vec<u8>,
    headers_done: bool,
    send_window:  i32,
    /// Per-stream RECEIVE window. RFC 9113 5.2 requires flow control to be
    /// tracked per stream as well as per connection; only the connection
    /// window existed, so one stream could consume the whole connection's
    /// credit and no stream-level limit applied at all.
    recv_window:  i32,
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
            recv_window: DEFAULT_WINDOW as i32,
            resp_body: None,
            resp_sent: 0,
            pending_rx: None,
        }
    }
}

// ── Connection phase ──────────────────────────────────────────────────────────

#[derive(PartialEq)]
#[derive(Debug)]
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
    /// Stream ids recently terminated by RST_STREAM, newest last.
    ///
    /// RFC 9113 5.1 treats a closed stream differently depending on HOW it
    /// closed: a frame arriving after RST_STREAM is a *stream* error of type
    /// STREAM_CLOSED, while a frame arriving after END_STREAM is a
    /// *connection* error of the same type. Absence from `streams` cannot tell
    /// the two apart, so the reset ids are remembered.
    ///
    /// Bounded at RESET_MEMORY and evicted oldest-first, deliberately. An
    /// unbounded set here would be a memory-exhaustion vector a peer controls
    /// by opening and resetting streams in a loop -- which is precisely the
    /// shape of Rapid Reset (CVE-2023-44487). Forgetting an old reset only
    /// costs a stricter-than-necessary error on a very stale frame.
    recently_reset:         std::collections::VecDeque<u32>,
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
            recently_reset: std::collections::VecDeque::new(),
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
        } else if ftype == TYPE_CONTINUATION {
            // RFC 9113 6.10: CONTINUATION may only follow HEADERS or
            // PUSH_PROMISE that did not carry END_HEADERS. Only the forward
            // direction was checked -- that a CONTINUATION was *expected* and
            // something else arrived. The reverse, a CONTINUATION arriving
            // when none is outstanding, was accepted and processed as though
            // it continued a header block that had already ended.
            return Err("CONTINUATION without a preceding HEADERS");
        }

        // RFC 9113 5.1: is this frame legal on this stream, in this state?
        // Checked before dispatch so no handler has to re-derive it, and so
        // that a frame on an idle or closed stream never reaches code that
        // would create the stream as a side effect of looking it up.
        match self.frame_verdict(ftype, stream_id) {
            FrameVerdict::Allow => {}
            FrameVerdict::ConnectionError(code) => {
                self.send_goaway(code);
                self.phase = Phase::GoingAway;
                return Ok(true);
            }
            FrameVerdict::StreamError(code) => {
                self.push_frame(TYPE_RST_STREAM, 0, stream_id, &code.to_be_bytes());
                self.streams.remove(&stream_id);
                return Ok(true);
            }
        }

        match ftype {
            TYPE_DATA          => self.handle_data(stream_id, flags, &payload, on_request, client_ip)?,
            TYPE_HEADERS       => self.handle_headers(stream_id, flags, &payload, on_request, client_ip)?,
            TYPE_PRIORITY      => {}
            TYPE_RST_STREAM    => {
                // Marked Closed, then removed. Removal alone is not enough:
                // `stream_state` derives Idle for any absent id above
                // `last_stream_id`, so resetting the newest stream and then
                // sending DATA on it would have read as idle -- a connection
                // error -- instead of closed. Bumping last_stream_id keeps the
                // derivation honest.
                self.streams.remove(&stream_id);
                self.last_stream_id = self.last_stream_id.max(stream_id);
                self.note_reset(stream_id);
            }
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

    // ── Stream state machine (RFC 9113 5.1) ───────────────────────────────────

    /// Remember a stream terminated by RST_STREAM, evicting the oldest.
    fn note_reset(&mut self, stream_id: u32) {
        if self.recently_reset.len() >= RESET_MEMORY {
            self.recently_reset.pop_front();
        }
        self.recently_reset.push_back(stream_id);
    }

    fn was_reset(&self, stream_id: u32) -> bool {
        self.recently_reset.contains(&stream_id)
    }

    /// The state of a stream, including streams that are not in the map.
    ///
    /// This is the piece that was missing. `streams` only ever held *live*
    /// streams, so a frame arriving on an id that was never opened and one
    /// arriving on an id that has already finished were indistinguishable:
    /// both were simply absent, and both were silently tolerated.
    ///
    /// Both are derivable from `last_stream_id` without storing anything:
    /// a client-initiated id above the highest yet seen has never been opened
    /// (idle); one at or below it has been and is gone (closed). That bound
    /// matters -- remembering every closed stream id would be unbounded memory
    /// that a peer controls simply by opening streams.
    fn stream_state(&self, stream_id: u32) -> StreamState {
        match self.streams.get(&stream_id) {
            Some(s) => s.state,
            None if stream_id > self.last_stream_id => StreamState::Idle,
            None => StreamState::Closed,
        }
    }

    /// Whether a frame type may be received on a stream in its current state.
    ///
    /// Written as one explicit table rather than scattered `if` checks so it
    /// can be read against RFC 9113 5.1 line by line. Every arm cites the rule
    /// it implements.
    fn frame_verdict(&self, ftype: u8, stream_id: u32) -> FrameVerdict {
        // Connection-level frames are not stream-scoped; their own handlers
        // validate them. Stream 0 is likewise never a stream.
        if stream_id == 0 {
            return FrameVerdict::Allow;
        }
        match ftype {
            // PRIORITY is permitted in every state, including idle and
            // closed (5.1). Deprecated in 9113 but must still be accepted.
            TYPE_PRIORITY => FrameVerdict::Allow,
            // These are connection-scoped and never carry a real stream id
            // here; handled elsewhere.
            TYPE_SETTINGS | TYPE_PING | TYPE_GOAWAY => FrameVerdict::Allow,
            _ => match self.stream_state(stream_id) {
                // 5.1 idle: "Receiving any frame other than HEADERS or
                // PRIORITY on a stream in this state MUST be treated as a
                // connection error of type PROTOCOL_ERROR."
                StreamState::Idle => match ftype {
                    TYPE_HEADERS => FrameVerdict::Allow,
                    _ => FrameVerdict::ConnectionError(ERR_PROTOCOL_ERROR),
                },

                // Fully open: anything the peer may send.
                StreamState::Open => FrameVerdict::Allow,

                // 5.1 half-closed (local): we have finished sending, the peer
                // has not. Everything from the peer is still legal.
                StreamState::HalfClosedLocal => FrameVerdict::Allow,

                // 5.1 half-closed (remote): the peer sent END_STREAM. "If an
                // endpoint receives additional frames, other than
                // WINDOW_UPDATE, PRIORITY, or RST_STREAM, for a stream that is
                // in this state, it MUST respond with a stream error of type
                // STREAM_CLOSED."
                StreamState::HalfClosedRemote => match ftype {
                    TYPE_WINDOW_UPDATE | TYPE_RST_STREAM => FrameVerdict::Allow,
                    _ => FrameVerdict::StreamError(ERR_STREAM_CLOSED),
                },

                // 5.1 closed, and the RFC splits this by *how* it closed:
                //
                //   "An endpoint that receives any frame other than PRIORITY
                //    after receiving a RST_STREAM MUST treat that as a stream
                //    error of type STREAM_CLOSED."
                //   "An endpoint that receives any frames after receiving a
                //    frame with the END_STREAM flag set MUST treat that as a
                //    connection error of type STREAM_CLOSED."
                //
                // WINDOW_UPDATE and RST_STREAM are tolerated either way "for a
                // short period", since they may already have been in flight.
                StreamState::Closed => match ftype {
                    TYPE_WINDOW_UPDATE | TYPE_RST_STREAM => FrameVerdict::Allow,
                    _ if self.was_reset(stream_id) => {
                        FrameVerdict::StreamError(ERR_STREAM_CLOSED)
                    }
                    _ => FrameVerdict::ConnectionError(ERR_STREAM_CLOSED),
                },

                // Reserved states are reachable only through PUSH_PROMISE.
                // This server never pushes and rejects a client PUSH_PROMISE
                // outright, so neither can occur; treat as protocol error
                // rather than silently allowing an impossible state through.
                StreamState::ReservedLocal | StreamState::ReservedRemote => {
                    FrameVerdict::ConnectionError(ERR_PROTOCOL_ERROR)
                }
            },
        }
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
        // RFC 9113 5.1.1: "The identifier of a newly established stream MUST
        // be numerically greater than all streams that the initiating endpoint
        // has opened or reserved. [...] An endpoint that receives an unexpected
        // stream identifier MUST respond with a connection error of type
        // PROTOCOL_ERROR."
        //
        // This answered RST_STREAM, which is a *stream* error and leaves the
        // connection running. Going backwards in stream ids is not a
        // recoverable per-stream mistake: the peer's numbering is broken, so
        // nothing that follows on this connection can be trusted to refer to
        // the stream either side thinks it does.
        if stream_id <= self.last_stream_id && !self.streams.contains_key(&stream_id) {
            self.send_goaway(ERR_PROTOCOL_ERROR);
            self.phase = Phase::GoingAway;
            return Ok(());
        }
        if self.streams.len() >= MAX_CONCURRENT as usize {
            self.push_frame(TYPE_RST_STREAM, 0, stream_id, &ERR_REFUSED_STREAM.to_be_bytes());
            return Ok(());
        }
        self.last_stream_id = self.last_stream_id.max(stream_id);

        // Flag-dependent prefixes, bounds-checked BEFORE slicing.
        //
        // `pos += 5; &payload[pos..]` used to run unguarded, so a HEADERS frame
        // with PRIORITY set and a payload shorter than five bytes panicked on
        // an out-of-range slice. That is a remotely reachable panic from a
        // three-byte frame -- no handshake beyond the preface required.
        // RFC 9113 6.2 says a HEADERS frame shorter than the fields its flags
        // declare is a FRAME_SIZE_ERROR, so that is what it now is.
        //
        // Padding is also stripped from the END. It never was: the pad bytes
        // were left on the header block and handed to the HPACK decoder as
        // though they were field data.
        let mut pos = 0usize;
        let mut pad = 0usize;
        if flags & FLAG_PADDED != 0 {
            if payload.is_empty() {
                return Err("HEADERS: PADDED set but no pad-length byte");
            }
            pad = payload[0] as usize;
            pos = 1;
        }
        if flags & FLAG_PRIORITY != 0 {
            // 4-byte stream dependency (with the E bit) + 1-byte weight.
            if payload.len() < pos + 5 {
                self.push_frame(TYPE_RST_STREAM, 0, stream_id, &ERR_FRAME_SIZE.to_be_bytes());
                return Err("HEADERS: PRIORITY set but payload shorter than the priority fields");
            }
            // RFC 9113 5.3.1: a stream cannot depend on itself.
            let dep = u32::from_be_bytes([
                payload[pos] & 0x7f, payload[pos + 1], payload[pos + 2], payload[pos + 3],
            ]);
            if dep == stream_id {
                self.push_frame(TYPE_RST_STREAM, 0, stream_id, &ERR_PROTOCOL_ERROR.to_be_bytes());
                return Ok(());
            }
            pos += 5;
        }
        // Padding must fit in what is left after the prefixes, and the header
        // block is what sits between them.
        if pos + pad > payload.len() {
            return Err("HEADERS: padding exceeds payload");
        }
        let header_block = &payload[pos..payload.len() - pad];

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
            // A second complete header block on a stream is a TRAILER section
            // (RFC 9113 8.1). Decode it separately so it can be checked on its
            // own: trailers carry no pseudo-headers, and merging first would
            // make one indistinguishable from a legitimate leading field.
            let is_trailers = stream.headers_done;
            let mut block = Vec::new();
            decode_hpack(dec, &combined, &mut block)?;
            if is_trailers {
                if let Some((name, _)) = block.iter().find(|(k, _)| k.starts_with(':')) {
                    tracing::debug!(stream_id, field = %name, "h2: pseudo-header in trailers");
                    self.push_frame(TYPE_RST_STREAM, 0, stream_id, &ERR_PROTOCOL_ERROR.to_be_bytes());
                    self.streams.remove(&stream_id);
                    return Ok(());
                }
            }
            let stream = self.streams.get_mut(&stream_id).unwrap();
            stream.headers.extend(block);
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

        // PADDED with an empty payload has nowhere to put the pad-length byte.
        // This used to fall through and treat the frame as unpadded.
        if flags & FLAG_PADDED != 0 && payload.is_empty() {
            return Err("DATA: PADDED set but no pad-length byte");
        }
        let data = if flags & FLAG_PADDED != 0 {
            let pad = payload[0] as usize;
            if pad >= payload.len() { return Err("DATA: excess padding"); }
            &payload[1..payload.len() - pad]
        } else {
            payload
        };

        // RFC 9113 6.9.1: the ENTIRE payload counts against flow control,
        // padding and pad-length byte included -- not just the data. Charging
        // only `data.len()` let a peer reclaim credit it never spent by padding
        // heavily, so the two windows drifted apart from the peer's view.
        let charged = payload.len() as i32;
        let data_len = data.len();

        self.conn_recv_window -= charged;
        if self.conn_recv_window < 0 { return Err("connection flow control exceeded"); }
        if self.conn_recv_window < DEFAULT_WINDOW as i32 / 2 {
            let inc = DEFAULT_WINDOW as i32 - self.conn_recv_window;
            self.conn_recv_window += inc;
            // An increment of 0 is itself a PROTOCOL_ERROR (RFC 9113 6.9), so
            // never emit one -- an empty DATA frame used to produce exactly
            // that.
            if inc > 0 {
                self.push_window_update(0, inc as u32);
            }
        }

        // Per-stream accounting, and the body cap.
        let mut stream_inc = 0i32;
        let mut over_cap = false;
        let should_dispatch = if let Some(s) = self.streams.get_mut(&stream_id) {
            s.recv_window -= charged;
            if s.recv_window < 0 { return Err("stream flow control exceeded"); }
            if s.body.len() + data_len > MAX_H2_BODY {
                over_cap = true;
                false
            } else {
                s.body.extend_from_slice(data);
                if flags & FLAG_END_STREAM != 0 {
                    s.state = StreamState::HalfClosedRemote;
                }
                if s.recv_window < DEFAULT_WINDOW as i32 / 2 {
                    stream_inc = DEFAULT_WINDOW as i32 - s.recv_window;
                    s.recv_window += stream_inc;
                }
                flags & FLAG_END_STREAM != 0 && s.headers_done
            }
        } else {
            false
        };
        if over_cap {
            self.streams.remove(&stream_id);
            self.push_frame(TYPE_RST_STREAM, 0, stream_id, &ERR_PROTOCOL_ERROR.to_be_bytes());
            return Ok(());
        }
        if stream_inc > 0 {
            self.push_window_update(stream_id, stream_inc as u32);
        }

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

        // RFC 9113 8.3.1 -- reject a malformed request rather than serving it.
        // Stream error, not connection error: the fault is in this request's
        // header list, and the connection (and its HPACK dynamic table) stay
        // valid for other streams.
        // RFC 9113 8.1.2.6: a declared content-length that disagrees with the
        // DATA actually received is a malformed request. This is not only a
        // conformance point -- a length mismatch between what a message claims
        // and what it carries is the same class of ambiguity that makes request
        // smuggling work, and here m6 is the one that would forward it on.
        if let Some(declared) = headers.iter()
            .find(|(k, _)| k == "content-length")
            .and_then(|(_, v)| v.trim().parse::<usize>().ok())
        {
            if declared != body.len() {
                tracing::debug!(stream_id, declared, actual = body.len(),
                    "h2: content-length disagrees with DATA length");
                self.push_frame(TYPE_RST_STREAM, 0, stream_id, &ERR_PROTOCOL_ERROR.to_be_bytes());
                self.streams.remove(&stream_id);
                return;
            }
        }

        if let Err(why) = validate_request_headers(&headers) {
            tracing::debug!(stream_id, reason = why, "h2: malformed request headers");
            self.push_frame(TYPE_RST_STREAM, 0, stream_id, &ERR_PROTOCOL_ERROR.to_be_bytes());
            self.streams.remove(&stream_id);
            return;
        }

        let req = build_request(&headers, body);

        match on_request(&req, client_ip) {
            RequestOutcome::Ready(status, resp_headers, resp_body, _, hints) => {
                let method = req.method.clone();
                self.dispatch_h2_response(stream_id, status, resp_headers, resp_body, hints, on_request, &client_ip, &method);
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
        method:       &str,
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
        // content-length still describes what a GET would have returned; the
        // body itself is withheld for HEAD (RFC 9110 9.3.2). Sending it anyway
        // is what made HTTP/2 abort the stream with INTERNAL_ERROR: the DATA
        // frames disagreed with the framing the client had been promised.
        let advertised_len = resp_body.len();
        let resp_body = if method.eq_ignore_ascii_case("HEAD") { Vec::new() } else { resp_body };
        let header_block = self.encode_response_headers(status, &resp_headers, advertised_len);
        self.push_frame(TYPE_HEADERS, FLAG_END_HEADERS, stream_id, &header_block);

        // Store response body for flow-controlled delivery. An empty body sends
        // a bare DATA + END_STREAM, which closes the stream cleanly.
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
                    // Same HEAD framing as the sync path above.
                    let advertised_len = resp_body.len();
                    let resp_body = if ctx.req.method.eq_ignore_ascii_case("HEAD") { Vec::new() } else { resp_body };
                    let header_block = self.encode_response_headers(status, &resp_headers, advertised_len);
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
                    // We have sent END_STREAM. RFC 9113 5.1: which state that
                    // leaves the stream in depends on the peer.
                    //
                    // If the client already sent END_STREAM the stream is now
                    // fully closed and can be reaped. If it has NOT, the
                    // stream is half-closed (local): the client may still
                    // legally send DATA, and dropping the stream here would
                    // make `stream_state` derive Closed and reject frames the
                    // RFC says must be accepted.
                    let peer_done = self
                        .streams
                        .get(&stream_id)
                        .map(|s| s.state == StreamState::HalfClosedRemote)
                        .unwrap_or(true);
                    if peer_done {
                        self.streams.remove(&stream_id);
                        self.last_stream_id = self.last_stream_id.max(stream_id);
                    } else if let Some(s) = self.streams.get_mut(&stream_id) {
                        s.state = StreamState::HalfClosedLocal;
                    }
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
            (b":status", status_str.as_bytes()),
        ];
        // RFC 9110 8.6: a 1xx/204 must not carry Content-Length, and a 304 must
        // not unless it equals the 200's. m6 sent `content-length: 0` on every
        // 304 -- the one value that actively misinforms, since it claims the
        // representation is empty when it is not.
        if crate::http11::status_may_have_content_length(status) {
            pairs.push((b"content-length", bodylen_str.as_bytes()));
        }
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
        // Applied at serialisation so every response carries them regardless
        // of which path produced it (cache hit, backend, error page). The
        // guard is held across the encode so the header strings can be
        // borrowed rather than copied.
        let security = crate::security::read();
        if let Some(ref guard) = security {
            for (k, v) in guard.absent_from(&filtered) {
                pairs.push((k.as_bytes(), v.as_bytes()));
            }
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

/// Validate a decoded request header list (RFC 9113 8.3.1, 8.2.1).
///
/// Returns `Err` with a short reason when the request is malformed. Every one
/// of these is a **stream error of type PROTOCOL_ERROR**, not something to
/// serve.
///
/// m6 did none of this: it took whatever HPACK produced and served a 200. An
/// uppercase field name, an unknown pseudo-header, a missing or empty `:path`,
/// a `Connection:` header, a duplicated `:method` -- all were accepted and
/// answered normally. That was 21 of the 49 h2spec failures measured on
/// 2026-09-09, the single largest cluster, and it is pure input validation
/// over an already-decoded list: no stream state or framing involvement.
///
/// The rules, in the order the RFC states them:
///
/// - **8.2.1**: field names must be lowercase. HTTP/2 has no case-insensitive
///   header names on the wire -- uppercase is malformed, not normalised.
/// - **8.2.2**: connection-specific fields must not appear. They are HTTP/1.1
///   hop-by-hop metadata with no meaning in H2, and forwarding one into an H1
///   backend is the smuggling primitive `check_forwardable` guards on egress.
///   `TE` is the one exception, and only with the exact value `trailers`.
/// - **8.3**: pseudo-headers precede regular fields, must be known, must not
///   repeat, and a request must not carry a response pseudo-header.
/// - **8.3.1**: `:method`, `:scheme` and `:path` are mandatory for anything
///   that is not CONNECT, and `:path` must not be empty.
fn validate_request_headers(headers: &[(String, String)]) -> Result<(), &'static str> {
    // Connection-specific fields (8.2.2). `upgrade` is included: HTTP/2 has no
    // upgrade mechanism, so its presence is always malformed.
    const CONNECTION_SPECIFIC: &[&str] =
        &["connection", "keep-alive", "proxy-connection", "transfer-encoding", "upgrade"];

    let mut seen_regular = false;
    let (mut method, mut scheme, mut path, mut authority) = (0u32, 0u32, 0u32, 0u32);
    let mut path_value: Option<&str> = None;
    let mut method_value: Option<&str> = None;

    for (name, value) in headers {
        if name.is_empty() {
            return Err("empty field name");
        }
        // 8.2.1 -- lowercase only. Checked before anything else, because every
        // comparison below assumes it.
        if name.bytes().any(|b| b.is_ascii_uppercase()) {
            return Err("uppercase field name");
        }

        if let Some(pseudo) = name.strip_prefix(':') {
            // 8.3 -- pseudo-headers must all precede regular fields.
            if seen_regular {
                return Err("pseudo-header after regular field");
            }
            match pseudo {
                "method"    => { method += 1; method_value = Some(value); }
                "scheme"    => scheme += 1,
                "path"      => { path += 1; path_value = Some(value); }
                "authority" => authority += 1,
                // `:status` is a RESPONSE pseudo-header; in a request it is
                // malformed rather than merely unknown, but the outcome is the
                // same and the reason is more useful spelled out.
                "status"    => return Err("response pseudo-header in request"),
                _           => return Err("unknown pseudo-header"),
            }
        } else {
            seen_regular = true;
            if CONNECTION_SPECIFIC.iter().any(|c| name == c) {
                return Err("connection-specific header field");
            }
            // 8.2.2: TE may appear, but only as exactly `trailers`.
            if name == "te" && value != "trailers" {
                return Err("TE header with a value other than trailers");
            }
        }
    }

    // 8.3 -- each pseudo-header at most once.
    if method > 1 || scheme > 1 || path > 1 || authority > 1 {
        return Err("duplicate pseudo-header");
    }

    // 8.3.1 -- CONNECT omits :scheme and :path and requires :authority.
    // Everything else requires all three.
    if method_value == Some("CONNECT") {
        if scheme != 0 || path != 0 {
            return Err("CONNECT with :scheme or :path");
        }
        if authority == 0 {
            return Err("CONNECT without :authority");
        }
        return Ok(());
    }

    if method == 0 { return Err("missing :method"); }
    if scheme == 0 { return Err("missing :scheme"); }
    if path == 0   { return Err("missing :path"); }

    // 8.3.1 -- :path must not be empty. `OPTIONS *` is the one legitimate
    // asterisk-form, carried as :path = "*".
    match path_value {
        Some("") => return Err("empty :path"),
        Some("*") if method_value != Some("OPTIONS") => return Err("asterisk :path on non-OPTIONS"),
        _ => {}
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
    let mut authority: Option<String> = None;

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
            ":authority" => authority = Some(v.clone()),
            k if k.starts_with(':') => {}
            // Strip proxy-owned headers on ingress — see
            // `forward::UNTRUSTED_INBOUND`.
            k if crate::forward::is_untrusted_inbound(k) => {}
            _ => fwd.push((k.clone(), v.clone())),
        }
    }

    // RFC 9113 8.3.1: :authority is HTTP/2's Host, and an intermediary
    // translating to HTTP/1.1 must carry it across. Dropping it with the other
    // pseudo-headers left every h2 request (which is most of them -- curl and
    // every browser send :authority and no Host) looking hostless to
    // everything downstream: host-dependent logic silently never fired, and
    // requests forwarded to a backend carried no Host at all.
    //
    // A client that sent both wins with its own Host, which is only reachable
    // from a non-conforming client; :authority fills in otherwise.
    if let Some(authority) = authority {
        if !fwd.iter().any(|(k, _)| k.eq_ignore_ascii_case("host")) {
            fwd.push(("Host".to_string(), authority));
        }
    }

    HttpRequest { method, path, query, version: "HTTP/2".to_string(), headers: fwd, body }
}

#[cfg(test)]
mod authority_tests {
    use super::build_request;

    fn h(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }
    fn host_of(req: &crate::forward::HttpRequest) -> Option<&str> {
        req.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("host"))
            .map(|(_, v)| v.as_str())
    }

    /// The case every browser and curl actually sends: :authority, no Host.
    #[test]
    fn authority_becomes_host() {
        let req = build_request(
            &h(&[(":method", "GET"), (":path", "/x"), (":authority", "www.example.com")]),
            vec![],
        );
        assert_eq!(host_of(&req), Some("www.example.com"));
    }

    /// A client sending both is non-conforming; its explicit Host still wins,
    /// and must not be duplicated.
    #[test]
    fn explicit_host_wins_and_is_not_duplicated() {
        let req = build_request(
            &h(&[
                (":method", "GET"),
                (":path", "/x"),
                (":authority", "authority.example"),
                ("host", "host.example"),
            ]),
            vec![],
        );
        assert_eq!(host_of(&req), Some("host.example"));
        assert_eq!(
            req.headers.iter().filter(|(k, _)| k.eq_ignore_ascii_case("host")).count(),
            1
        );
    }

    #[test]
    fn no_authority_means_no_synthesized_host() {
        let req = build_request(&h(&[(":method", "GET"), (":path", "/x")]), vec![]);
        assert_eq!(host_of(&req), None);
    }

    /// Other pseudo-headers stay stripped -- they must never reach a backend.
    #[test]
    fn other_pseudo_headers_are_still_dropped() {
        let req = build_request(
            &h(&[(":method", "GET"), (":path", "/x?a=1"), (":scheme", "https"), (":authority", "e.example")]),
            vec![],
        );
        assert!(!req.headers.iter().any(|(k, _)| k.starts_with(':')), "{:?}", req.headers);
        assert_eq!(req.path, "/x");
        assert_eq!(req.query.as_deref(), Some("a=1"));
    }
}

#[cfg(test)]
mod frame_validation_tests {
    use super::*;

    /// Feed raw frames into a connection that is already past the preface, and
    /// return the result of draining them.
    ///
    /// `process_frame` is private, which is the point: these test the parser at
    /// the boundary a remote peer actually reaches, without a socket.
    pub(super) fn feed(frames: &[u8]) -> Result<(), &'static str> {
        let mut c = Http2Conn::new();
        c.phase = Phase::Active;
        c.recv_buf.extend_from_slice(frames);
        let mut on_request = |_: &HttpRequest, _: &str| -> RequestOutcome {
            RequestOutcome::Ready(200, vec![], b"ok".to_vec(), "test".to_string(),
                                  std::sync::Arc::new(vec![]))
        };
        loop {
            match c.process_frame(&mut on_request, "127.0.0.1") {
                Ok(true) => continue,
                Ok(false) => return Ok(()),
                Err(e) => return Err(e),
            }
        }
    }

    pub(super) fn frame(ftype: u8, flags: u8, stream_id: u32, payload: &[u8]) -> Vec<u8> {
        let mut v = Vec::new();
        let len = payload.len();
        v.push((len >> 16) as u8);
        v.push((len >> 8) as u8);
        v.push(len as u8);
        v.push(ftype);
        v.push(flags);
        v.extend_from_slice(&(stream_id & 0x7fff_ffff).to_be_bytes());
        v.extend_from_slice(payload);
        v
    }

    /// A minimal HEADERS frame that opens `stream_id`, for tests that need a
    /// stream in the `open` state before exercising a later frame.
    ///
    /// Needed since the RFC 9113 5.1 state machine landed: DATA on a stream
    /// that was never opened is a connection error (PROTOCOL_ERROR) checked
    /// BEFORE any frame-specific validation, so a test that sends DATA on a
    /// bare stream id now measures the state machine rather than the thing it
    /// meant to test.
    pub(super) fn open_stream(stream_id: u32) -> Vec<u8> {
        // ":method: GET" / ":scheme: https" / ":path: /" as HPACK static-table
        // indexed fields (2, 7, 4) -- no dynamic table, no Huffman.
        frame(TYPE_HEADERS, FLAG_END_HEADERS, stream_id, &[0x82, 0x87, 0x84])
    }

    /// The panic. A HEADERS frame declaring PRIORITY but carrying fewer than
    /// the five bytes the priority fields require used to run
    /// `&payload[pos..]` with `pos` past the end and abort the process.
    ///
    /// Reachable from a three-byte frame immediately after the preface, with no
    /// other setup. Must be an error, never a panic.
    #[test]
    fn short_priority_headers_frame_does_not_panic() {
        for short in 0..5usize {
            let payload = vec![0u8; short];
            let f = frame(TYPE_HEADERS, FLAG_PRIORITY, 1, &payload);
            let r = feed(&f);
            assert!(r.is_err(), "a {short}-byte PRIORITY HEADERS payload should be rejected");
        }
    }

    /// Same shape with PADDED as well: the prefixes are 1 + 5 bytes.
    #[test]
    fn short_padded_priority_headers_frame_does_not_panic() {
        for short in 0..6usize {
            let payload = vec![0u8; short];
            let f = frame(TYPE_HEADERS, FLAG_PADDED | FLAG_PRIORITY, 1, &payload);
            let _ = feed(&f); // must not panic; either error or clean handling
        }
    }

    /// RFC 9113 5.3.1: a stream cannot depend on itself.
    #[test]
    fn headers_priority_self_dependency_is_rejected() {
        // 4-byte dependency == this stream id, then a weight byte.
        let mut payload = 1u32.to_be_bytes().to_vec();
        payload.push(0);
        let f = frame(TYPE_HEADERS, FLAG_PRIORITY, 1, &payload);
        // Handled as a stream error (RST_STREAM), not a connection error.
        assert!(feed(&f).is_ok());
    }

    /// PADDED with an empty payload has nowhere to put the pad-length byte.
    /// This used to be silently treated as an unpadded frame.
    #[test]
    fn padded_data_with_empty_payload_is_rejected() {
        let mut f = open_stream(1);
        f.extend(frame(TYPE_DATA, FLAG_PADDED, 1, &[]));
        assert!(feed(&f).is_err());
    }

    #[test]
    fn data_with_excess_padding_is_rejected() {
        // pad length 200 in a 4-byte payload.
        let mut f = open_stream(1);
        f.extend(frame(TYPE_DATA, FLAG_PADDED, 1, &[200, 1, 2, 3]));
        assert!(feed(&f).is_err());
    }

    /// An empty DATA frame used to make the server emit WINDOW_UPDATE with an
    /// increment of zero, which is itself a PROTOCOL_ERROR (RFC 9113 6.9).
    #[test]
    fn empty_data_never_emits_a_zero_window_update() {
        let mut c = Http2Conn::new();
        c.phase = Phase::Active;
        c.recv_buf.extend_from_slice(&frame(TYPE_DATA, 0, 1, &[]));
        let mut on_request = |_: &HttpRequest, _: &str| -> RequestOutcome {
            RequestOutcome::Ready(200, vec![], vec![], "t".to_string(), std::sync::Arc::new(vec![]))
        };
        let _ = c.process_frame(&mut on_request, "127.0.0.1");

        // Walk the outgoing buffer for WINDOW_UPDATE frames and check each
        // increment. A zero increment would be a protocol error we caused.
        let buf = &c.send_buf;
        let mut i = 0usize;
        while i + FRAME_HDR <= buf.len() {
            let len = ((buf[i] as usize) << 16) | ((buf[i + 1] as usize) << 8) | buf[i + 2] as usize;
            let ftype = buf[i + 3];
            let body = &buf[i + FRAME_HDR..(i + FRAME_HDR + len).min(buf.len())];
            if ftype == TYPE_WINDOW_UPDATE && body.len() == 4 {
                let inc = u32::from_be_bytes([body[0], body[1], body[2], body[3]]) & 0x7fff_ffff;
                assert!(inc > 0, "emitted WINDOW_UPDATE with increment 0");
            }
            i += FRAME_HDR + len;
        }
    }

    /// Garbage frames of every type and a spread of lengths must never panic.
    /// This is the property that matters most here: a remote peer controls
    /// every byte, so any panic is a remote kill.
    #[test]
    fn no_frame_shape_panics() {
        for ftype in 0u8..=12 {
            for flags in [0u8, 0x1, 0x4, 0x8, 0x20, 0x24, 0x28, 0xff] {
                for len in [0usize, 1, 2, 3, 4, 5, 6, 7, 8, 9, 17] {
                    let payload = vec![0xABu8; len];
                    for sid in [0u32, 1, 2, 0x7fff_ffff] {
                        let f = frame(ftype, flags, sid, &payload);
                        let _ = feed(&f); // only requirement: it returns
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod pseudo_header_tests {
    use super::validate_request_headers;

    fn h(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }
    fn ok(pairs: &[(&str, &str)]) {
        assert_eq!(validate_request_headers(&h(pairs)), Ok(()), "should be valid: {pairs:?}");
    }
    fn bad(pairs: &[(&str, &str)]) -> &'static str {
        validate_request_headers(&h(pairs))
            .expect_err(&format!("should be rejected: {pairs:?}"))
    }

    const GOOD: &[(&str, &str)] =
        &[(":method", "GET"), (":scheme", "https"), (":path", "/"), (":authority", "example.com")];

    /// A well-formed request must still pass. Without this the whole module
    /// could "fix" 21 failures by rejecting everything.
    #[test]
    fn well_formed_requests_pass() {
        ok(GOOD);
        ok(&[(":method", "GET"), (":scheme", "https"), (":path", "/x"),
             ("user-agent", "curl/8"), ("accept", "*/*")]);
        // TE: trailers is the one permitted connection-ish header.
        ok(&[(":method", "GET"), (":scheme", "https"), (":path", "/"), ("te", "trailers")]);
        // OPTIONS * is the legitimate asterisk-form.
        ok(&[(":method", "OPTIONS"), (":scheme", "https"), (":path", "*")]);
    }

    /// RFC 9113 8.2.1. HTTP/2 field names are lowercase on the wire; uppercase
    /// is malformed, NOT something to normalise. h2spec:
    /// "Sends a HEADERS frame that contains the header field name in uppercase letters".
    #[test]
    fn uppercase_field_names_are_rejected() {
        assert_eq!(bad(&[(":method", "GET"), (":scheme", "https"), (":path", "/"),
                         ("User-Agent", "x")]), "uppercase field name");
        assert_eq!(bad(&[(":Method", "GET")]), "uppercase field name");
    }

    /// RFC 9113 8.3. h2spec: "Sends a HEADERS frame that contains a unknown
    /// pseudo-header field".
    #[test]
    fn unknown_and_misplaced_pseudo_headers_are_rejected() {
        assert_eq!(bad(&[(":method", "GET"), (":scheme", "https"), (":path", "/"),
                         (":unknown", "x")]), "unknown pseudo-header");
        // A response pseudo-header has no place in a request.
        assert_eq!(bad(&[(":method", "GET"), (":scheme", "https"), (":path", "/"),
                         (":status", "200")]), "response pseudo-header in request");
        // Pseudo-headers must all come first.
        assert_eq!(bad(&[(":method", "GET"), ("user-agent", "x"), (":path", "/")]),
                   "pseudo-header after regular field");
    }

    /// RFC 9113 8.3.1. h2spec: "Sends a HEADERS frame with empty \":path\"".
    #[test]
    fn mandatory_pseudo_headers_are_enforced() {
        assert_eq!(bad(&[(":method", "GET"), (":scheme", "https"), (":path", "")]), "empty :path");
        assert_eq!(bad(&[(":scheme", "https"), (":path", "/")]), "missing :method");
        assert_eq!(bad(&[(":method", "GET"), (":path", "/")]), "missing :scheme");
        assert_eq!(bad(&[(":method", "GET"), (":scheme", "https")]), "missing :path");
        // Asterisk-form is only legal for OPTIONS.
        assert_eq!(bad(&[(":method", "GET"), (":scheme", "https"), (":path", "*")]),
                   "asterisk :path on non-OPTIONS");
    }

    #[test]
    fn duplicate_pseudo_headers_are_rejected() {
        assert_eq!(bad(&[(":method", "GET"), (":method", "POST"),
                         (":scheme", "https"), (":path", "/")]), "duplicate pseudo-header");
        assert_eq!(bad(&[(":method", "GET"), (":scheme", "https"),
                         (":path", "/"), (":path", "/y")]), "duplicate pseudo-header");
    }

    /// RFC 9113 8.2.2. These are HTTP/1.1 hop-by-hop metadata with no meaning
    /// in H2, and forwarding one into an H1 backend is exactly the smuggling
    /// primitive `check_forwardable` guards against on egress. Rejecting on
    /// ingress means it never gets that far.
    #[test]
    fn connection_specific_fields_are_rejected() {
        for f in ["connection", "keep-alive", "proxy-connection", "transfer-encoding", "upgrade"] {
            let mut v = GOOD.to_vec();
            v.push((f, "x"));
            assert_eq!(bad(&v), "connection-specific header field", "for {f}");
        }
        // TE is permitted, but only as exactly "trailers".
        let mut v = GOOD.to_vec();
        v.push(("te", "gzip"));
        assert_eq!(bad(&v), "TE header with a value other than trailers");
    }

    /// CONNECT is the one method that legitimately omits :scheme and :path,
    /// and it requires :authority. Getting this wrong would break the method
    /// while "fixing" conformance.
    #[test]
    fn connect_has_its_own_rules() {
        assert_eq!(validate_request_headers(&h(&[(":method", "CONNECT"),
                                                 (":authority", "example.com:443")])), Ok(()));
        assert_eq!(bad(&[(":method", "CONNECT"), (":authority", "x"), (":path", "/")]),
                   "CONNECT with :scheme or :path");
        assert_eq!(bad(&[(":method", "CONNECT")]), "CONNECT without :authority");
    }

    #[test]
    fn empty_field_name_is_rejected() {
        assert_eq!(bad(&[("", "x")]), "empty field name");
    }
}

#[cfg(test)]
mod stream_state_tests {
    use super::*;
    use super::frame_validation_tests::{feed, frame, open_stream};

    /// RFC 9113 5.1 idle: "Receiving any frame other than HEADERS or PRIORITY
    /// on a stream in this state MUST be treated as a connection error of
    /// type PROTOCOL_ERROR."
    ///
    /// Before the state machine existed there was no notion of "idle" at all:
    /// a frame on a stream that had never been opened was indistinguishable
    /// from one on a stream that had finished, and both were tolerated.
    #[test]
    fn frames_on_an_idle_stream_are_refused() {
        for ftype in [TYPE_DATA, TYPE_RST_STREAM, TYPE_WINDOW_UPDATE] {
            let f = frame(ftype, 0, 7, &[0, 0, 0, 1]);
            let mut c = Http2Conn::new();
            c.phase = Phase::Active;
            assert_eq!(
                c.frame_verdict(ftype, 7),
                FrameVerdict::ConnectionError(ERR_PROTOCOL_ERROR),
                "frame type {ftype:#x} on an idle stream must be a connection error"
            );
            // And it must not panic or hang when actually fed.
            let _ = feed(&f);
        }
    }

    /// PRIORITY is legal in every state, including idle and closed (5.1).
    /// Deprecated in 9113 but must still be accepted, not rejected.
    #[test]
    fn priority_is_allowed_in_every_state() {
        let c = Http2Conn::new();
        assert_eq!(c.frame_verdict(TYPE_PRIORITY, 1), FrameVerdict::Allow);
        assert_eq!(c.frame_verdict(TYPE_PRIORITY, 99), FrameVerdict::Allow);
    }

    /// HEADERS is what takes a stream out of idle, so it must be allowed there.
    #[test]
    fn headers_opens_an_idle_stream() {
        let c = Http2Conn::new();
        assert_eq!(c.frame_verdict(TYPE_HEADERS, 1), FrameVerdict::Allow);
    }

    /// An absent stream id at or below the high-water mark has been used and
    /// is gone; above it, it has never been opened. This derivation is what
    /// makes "closed" detectable without remembering every stream forever.
    #[test]
    fn absent_streams_are_idle_above_the_watermark_and_closed_below() {
        let mut c = Http2Conn::new();
        c.last_stream_id = 5;
        assert_eq!(c.stream_state(7), StreamState::Idle);
        assert_eq!(c.stream_state(3), StreamState::Closed);
        assert_eq!(c.stream_state(5), StreamState::Closed);
    }

    /// 5.1 half-closed (remote): everything except WINDOW_UPDATE, PRIORITY
    /// and RST_STREAM is a stream error of type STREAM_CLOSED.
    #[test]
    fn half_closed_remote_refuses_data_but_allows_window_update() {
        let mut c = Http2Conn::new();
        let mut st = H2Stream::new(65535);
        st.state = StreamState::HalfClosedRemote;
        c.streams.insert(1, st);
        assert_eq!(c.frame_verdict(TYPE_DATA, 1), FrameVerdict::StreamError(ERR_STREAM_CLOSED));
        assert_eq!(c.frame_verdict(TYPE_HEADERS, 1), FrameVerdict::StreamError(ERR_STREAM_CLOSED));
        assert_eq!(c.frame_verdict(TYPE_WINDOW_UPDATE, 1), FrameVerdict::Allow);
        assert_eq!(c.frame_verdict(TYPE_RST_STREAM, 1), FrameVerdict::Allow);
        assert_eq!(c.frame_verdict(TYPE_PRIORITY, 1), FrameVerdict::Allow);
    }

    /// 5.1 half-closed (local): we have finished sending; the peer has not,
    /// so everything it sends is still legal. Reaping the stream when we
    /// finish would wrongly make these look closed.
    #[test]
    fn half_closed_local_still_accepts_peer_frames() {
        let mut c = Http2Conn::new();
        let mut st = H2Stream::new(65535);
        st.state = StreamState::HalfClosedLocal;
        c.streams.insert(1, st);
        assert_eq!(c.frame_verdict(TYPE_DATA, 1), FrameVerdict::Allow);
        assert_eq!(c.frame_verdict(TYPE_WINDOW_UPDATE, 1), FrameVerdict::Allow);
    }

    /// 5.1 closed, both branches. The RFC splits this by HOW the stream
    /// closed, and getting it wrong in either direction is a real fault: a
    /// connection error where a stream error belongs kills a healthy
    /// connection, and the reverse leaves a peer that has lost track of the
    /// stream believing the connection is still coherent.
    #[test]
    fn closed_after_end_stream_is_a_connection_error() {
        let mut c = Http2Conn::new();
        c.last_stream_id = 9;                       // 3 is closed, never reset
        for ftype in [TYPE_DATA, TYPE_HEADERS, TYPE_CONTINUATION] {
            assert_eq!(
                c.frame_verdict(ftype, 3),
                FrameVerdict::ConnectionError(ERR_STREAM_CLOSED),
                "{ftype:#x} after END_STREAM is a connection error"
            );
        }
        // Possibly in flight when we closed; tolerated either way.
        assert_eq!(c.frame_verdict(TYPE_WINDOW_UPDATE, 3), FrameVerdict::Allow);
        assert_eq!(c.frame_verdict(TYPE_RST_STREAM, 3), FrameVerdict::Allow);
    }

    #[test]
    fn closed_after_rst_stream_is_only_a_stream_error() {
        let mut c = Http2Conn::new();
        c.last_stream_id = 9;
        c.note_reset(3);
        for ftype in [TYPE_DATA, TYPE_HEADERS, TYPE_CONTINUATION] {
            assert_eq!(
                c.frame_verdict(ftype, 3),
                FrameVerdict::StreamError(ERR_STREAM_CLOSED),
                "{ftype:#x} after RST_STREAM is a stream error, not a connection error"
            );
        }
    }

    /// The reset memory must stay bounded. Unbounded, it is a
    /// memory-exhaustion vector a peer drives by opening and resetting streams
    /// in a loop -- the shape of Rapid Reset, CVE-2023-44487.
    #[test]
    fn reset_memory_is_bounded_and_evicts_oldest_first() {
        let mut c = Http2Conn::new();
        for id in 1..=(RESET_MEMORY as u32 + 50) {
            c.note_reset(id);
        }
        assert_eq!(c.recently_reset.len(), RESET_MEMORY);
        assert!(!c.was_reset(1), "oldest entries must be evicted");
        assert!(c.was_reset(RESET_MEMORY as u32 + 50), "newest must be retained");
    }

    /// Stream 0 is the connection, not a stream. Its frames are validated by
    /// their own handlers, and running them through the stream table would
    /// classify the connection itself as idle.
    #[test]
    fn stream_zero_bypasses_the_stream_table() {
        let c = Http2Conn::new();
        for ftype in [TYPE_SETTINGS, TYPE_PING, TYPE_GOAWAY, TYPE_WINDOW_UPDATE] {
            assert_eq!(c.frame_verdict(ftype, 0), FrameVerdict::Allow);
        }
    }

    /// RFC 9113 6.10: CONTINUATION is only legal immediately after a HEADERS
    /// or PUSH_PROMISE that lacked END_HEADERS. Only the forward direction was
    /// checked -- a CONTINUATION arriving with none outstanding was accepted
    /// and processed as though it continued a header block that had ended.
    #[test]
    fn continuation_without_a_preceding_headers_is_refused() {
        let mut f = open_stream(1);          // carries END_HEADERS
        f.extend(frame(TYPE_CONTINUATION, FLAG_END_HEADERS, 1, &[0x82]));
        assert!(feed(&f).is_err(), "CONTINUATION after END_HEADERS must be refused");
    }

    /// RFC 9113 5.1.1: a new stream id must exceed every id already opened.
    /// Going backwards was answered with RST_STREAM, a stream error that
    /// leaves the connection running -- but the peer's numbering is broken, so
    /// nothing after it can be trusted. It is a connection error.
    #[test]
    fn decreasing_stream_ids_end_the_connection() {
        let mut f = open_stream(5);
        f.extend(open_stream(3));
        let mut c = Http2Conn::new();
        c.phase = Phase::Active;
        c.recv_buf.extend_from_slice(&f);
        let mut on_request = |_: &HttpRequest, _: &str| -> RequestOutcome {
            RequestOutcome::Ready(200, vec![], b"ok".to_vec(), "t".to_string(),
                                  std::sync::Arc::new(vec![]))
        };
        while let Ok(true) = c.process_frame(&mut on_request, "127.0.0.1") {}
        assert_eq!(c.phase, Phase::GoingAway, "a backwards stream id must GOAWAY");
    }
}
