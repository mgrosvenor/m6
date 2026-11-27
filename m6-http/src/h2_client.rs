//! Shared HTTP/2 outbound client core, generic over the transport.
//!
//! The `h2c://` (cleartext TCP) and `h2s://` (TLS) backbone clients are one
//! engine. The only differences are `connect()` and the transport type, which
//! the [`Transport`] trait isolates. The frame loop, flow control, HPACK and
//! stream lifecycle live here once, so a fix to any of them cannot drift
//! between the two clients.

use std::collections::HashMap;
use std::io;
use std::net::{SocketAddr, TcpStream};
use std::os::unix::io::RawFd;
use std::time::{Duration, Instant};

use crate::forward::{
    apply_response_block, too_many_informational, HttpRequest, HttpResponse, PendingUrlContext,
    ResponseBlock, Waiter, HOP_BY_HOP,
};
use m6_core::client::TlsClient;
use m6_core::event::Timeouts;
use m6_core::platform::{Interest, Registry, Token};

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
/// `connect()` begins a non-blocking connect to a resolved address and returns
/// at once. The client drives the connect, and the TLS handshake for `h2s`,
/// from [`H2Client::drive`]: the first writable event finishes the connect, and
/// for TLS the handshake follows on the same descriptor. The rest are the I/O
/// operations the shared frame loop needs, plus the per-transport constants.
pub trait Transport: Sized {
    /// Begin a non-blocking connect to `addr`. Never waits.
    fn connect(addr: SocketAddr, tls: Option<&TlsClient>) -> io::Result<Self>;

    /// The raw file descriptor, for the poll.
    fn raw_fd(&self) -> RawFd;

    /// The pending socket error of an in-progress connect, if any.
    fn take_error(&self) -> io::Result<Option<io::Error>>;

    /// Whether a non-blocking connect has completed. A socket whose connect is
    /// still in progress has no peer address yet, and a write to it fails with
    /// `ENOTCONN`, so this is the check that decides the connect is really up.
    fn is_connected(&self) -> bool;

    /// Whether the TLS handshake is still in progress. False for cleartext.
    fn is_handshaking(&self) -> bool;

    /// The negotiated ALPN protocol, when the transport is TLS.
    fn alpn_protocol(&self) -> Option<&[u8]>;

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
    /// END_STREAM has arrived. A header block split across HEADERS and
    /// CONTINUATION frames is one block (RFC 9113 6.2), so the reply is
    /// complete only when the block ends too.
    end_stream: bool,
    /// Informational (1xx) header blocks seen so far on this stream.
    informational: usize,
    /// Who waits for this stream's reply, stored here by the loop's owner once
    /// the request is parked, and moved into the completion when it arrives.
    waiter: Option<Waiter>,
    /// The request context the completion path finalizes with.
    ctx: Option<Box<PendingUrlContext>>,
    // The request body still owed to the peer, and how much of it has gone out.
    // The body is resumable state rather than a loop because credit arrives
    // asynchronously: see `pump_stream`.
    pending_body: Vec<u8>,
    body_off: usize,
    // RFC 9113 6.9.2: the per-stream send window starts at the peer's
    // SETTINGS_INITIAL_WINDOW_SIZE, not at the 65 535 connection default.
    send_window: i32,
    /// This stream's slot in the pool's stream deadlines, or `usize::MAX` when
    /// no pool owns it. The pool clears it when the stream completes.
    deadline_slot: usize,
}

/// One stream's finished reply, pushed onto the pool's completion list for the
/// loop's owner to write straight into the waiting stream.
pub struct H2Completion {
    pub stream_id: u32,
    pub result: io::Result<HttpResponse>,
    pub waiter: Option<Waiter>,
    pub ctx: Option<Box<PendingUrlContext>>,
    /// The pool's deadline slot for this stream, cleared when the completion is
    /// taken.
    pub(crate) deadline_slot: usize,
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
    /// Where this connection is in its own start-up. Request frames go into
    /// `send_buf` while it is not open, and flush when it is.
    phase: Phase,
    pub is_dead: bool,
    /// The `mio` token this connection's descriptor is registered under.
    token: Token,
    /// What the poll currently waits for, so `reregister` runs only on change.
    registered_interest: Interest,
    /// Streams that finished this drive, for the loop's owner to complete.
    /// Allocated at start and drained each drive, never grown on the hot path.
    completions: Vec<H2Completion>,
}

/// Where a connection is in its own start-up.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Phase {
    /// The non-blocking TCP connect is in progress.
    Connecting,
    /// The TLS handshake is in progress.
    Handshaking,
    /// Requests may be written and replies read.
    Open,
}

impl<T: Transport> H2Client<T> {
    /// Build a client around an already-open transport.
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
            phase: Phase::Open,
            is_dead: false,
            token: Token(usize::MAX),
            registered_interest: Interest::READABLE,
            completions: Vec::with_capacity(64),
        }
    }

    /// Begin a non-blocking connect and queue the client preface and SETTINGS
    /// behind it. The connection is not usable until `drive` reports it open,
    /// so a caller that must not wait sends its request now and lets the loop
    /// flush it when the connect finishes.
    pub(crate) fn connect(addr: SocketAddr, tls: Option<&TlsClient>) -> io::Result<Self> {
        let mut client = Self::new(T::connect(addr, tls)?);
        client.phase = Phase::Connecting;
        client.send_buf.extend_from_slice(&client_init_frame());
        Ok(client)
    }

    /// The interest to register while the connection is still starting: a
    /// connect finishes on writability, and the TLS handshake needs both.
    pub fn phase_interest(&self) -> Interest {
        match self.phase {
            Phase::Connecting => Interest::WRITABLE,
            Phase::Handshaking => Interest::READABLE | Interest::WRITABLE,
            Phase::Open => self.interest(),
        }
    }

    /// Whether the connection is open for requests.
    pub fn is_open(&self) -> bool {
        self.phase == Phase::Open && !self.is_dead
    }

    /// Wait for this connection to open.
    ///
    /// Tests and benchmarks only. A service never waits: it registers the
    /// descriptor and calls `drive` when its loop reports it ready. The client
    /// convenience constructors use this so their callers keep the blocking
    /// contract they were written against.
    pub(crate) fn wait_until_open(&mut self) -> io::Result<()> {
        let mut poll = m6_core::platform::Poll::new()?;
        poll.registry().register_fd(
            self.raw_fd(),
            Token(0),
            Interest::READABLE | Interest::WRITABLE,
        )?;
        let mut events = m6_core::platform::Events::with_capacity(8);
        let deadline = m6_core::platform::now() + Duration::from_secs(10);
        loop {
            self.drive();
            if self.is_open() {
                return Ok(());
            }
            if self.is_dead {
                return Err(io::Error::new(
                    io::ErrorKind::ConnectionRefused,
                    format!("{}: connection failed", T::prefix()),
                ));
            }
            let left = deadline.saturating_duration_since(m6_core::platform::now());
            if left.is_zero() {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("{}: connect did not open", T::prefix()),
                ));
            }
            poll.poll(&mut events, Some(left))?;
            events.clear();
        }
    }

    /// Finish the connect, and then the TLS handshake, as far as the descriptor
    /// allows. False when the connection died.
    fn start_progress(&mut self) -> bool {
        match self.phase {
            Phase::Connecting => {
                match self.transport.take_error() {
                    Ok(None) => {}
                    Ok(Some(e)) => {
                        self.mark_dead(&format!("{}: connect: {e}", T::prefix()));
                        return false;
                    }
                    Err(e) => {
                        self.mark_dead(&format!("{}: connect: {e}", T::prefix()));
                        return false;
                    }
                }
                if !self.transport.is_connected() {
                    // The connect is still in progress. A write now would fail
                    // with ENOTCONN; wait for the next readiness instead.
                    return false;
                }
                self.phase = if self.transport.is_handshaking() {
                    Phase::Handshaking
                } else {
                    Phase::Open
                };
                true
            }
            Phase::Handshaking => {
                if let Err(reason) = self.transport.flush_send(&mut self.send_buf) {
                    self.mark_dead(&reason);
                    return false;
                }
                if let Some(reason) = self.transport.fill_recv(&mut self.recv_buf) {
                    self.mark_dead(&reason);
                    return false;
                }
                if self.transport.is_handshaking() {
                    return false;
                }
                if self.transport.alpn_protocol() != Some(b"h2".as_slice()) {
                    let got = self
                        .transport
                        .alpn_protocol()
                        .map(|p| String::from_utf8_lossy(p).into_owned())
                        .unwrap_or_else(|| "none".to_string());
                    self.mark_dead(&format!(
                        "{}: server did not negotiate h2 (got: {got})",
                        T::prefix()
                    ));
                    return false;
                }
                self.phase = Phase::Open;
                true
            }
            Phase::Open => true,
        }
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
    /// Returns the stream id. The reply arrives on `drain_completions` when the
    /// stream finishes. The connection must not be dead.
    ///
    /// `via` is the node's prebuilt [`crate::forward::ViaEntries`] when the
    /// request is one m6-http forwards for a client, and `None` when m6-http
    /// originates it itself (the generation ID's heartbeat over h2c, for one).
    /// A forwarded request gets its `Via` here as it does on the HTTP/1.1 leg,
    /// from the same prebuilt entries, so the backbone hop keeps the path order
    /// the site reads and neither leg formats an entry per request.
    pub fn dispatch(
        &mut self,
        req: &HttpRequest,
        host: &str,
        client_ip: &str,
        original_host: &str,
        via: Option<&crate::forward::ViaEntries>,
    ) -> io::Result<u32> {
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
        let mut forwarded: Vec<(Vec<u8>, Vec<u8>)> = req
            .headers
            .iter()
            .filter_map(|(name, value)| {
                if HOP_BY_HOP.iter().any(|&h| name.eq_ignore_ascii_case(h)) {
                    return None;
                }
                if name.starts_with(':') {
                    return None;
                }
                // Re-emitted below as one joined list when this node is the
                // proxy for this request.
                if via.is_some() && name.eq_ignore_ascii_case(crate::forward::VIA) {
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

        // A request that arrived without a Host header (HTTP/3 strips
        // pseudo-headers, and HTTP/1.0 may omit it) names its origin only in
        // `original_host`. The HTTP/1.1 leg synthesises `Host` from it in
        // `write_host_if_absent`; this leg must carry it too, or the origin
        // derives the host from `:authority` (the backbone address) and a
        // host-matched backend answers 404.
        if !original_host.is_empty() && !m6_core::headers::contains(&req.headers[..], "host") {
            forwarded.push((b"host".to_vec(), original_host.as_bytes().to_vec()));
        }

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
        // This node's entry was built at start. The common case -- a public
        // listener, where an arriving `Via` is stripped -- appends it with no
        // allocation at all. Only a backbone hop that carries a peer's entry
        // joins the two, and that is a cache-to-origin leg, not the hot path.
        let joined_via: String;
        if let Some(via) = via {
            let mine = via.for_version(&req.version);
            let mut existing = crate::forward::existing_via(&req.headers);
            if existing.is_empty() {
                hdr_list.push((crate::forward::VIA.as_bytes(), mine.as_bytes()));
            } else {
                existing.push_str(", ");
                existing.push_str(mine);
                joined_via = existing;
                hdr_list.push((crate::forward::VIA.as_bytes(), joined_via.as_bytes()));
            }
        }

        let header_block = self.hpack_enc.encode(hdr_list);

        // Push HEADERS frame
        let has_body = !req.body.is_empty();
        let headers_flags = FLAG_END_HEADERS | if has_body { 0 } else { FLAG_END_STREAM };
        self.push_frame(TYPE_HEADERS, headers_flags, stream_id, &header_block);

        self.streams.insert(
            stream_id,
            H2Stream {
                resp_status: 0,
                resp_headers: vec![],
                resp_body: vec![],
                headers_done: false,
                end_stream: false,
                informational: 0,
                waiter: None,
                ctx: None,
                pending_body: if has_body {
                    req.body.clone()
                } else {
                    Vec::new()
                },
                body_off: 0,
                send_window: self.peer_initial_window,
                deadline_slot: usize::MAX,
            },
        );

        // Send the body via the flow-control pump. Credit arrives
        // asynchronously on WINDOW_UPDATE, so the remainder is kept as stream
        // state and resumed when credit arrives; blocking instead would stall
        // every other connection this loop drives, and sending without credit
        // is a connection error (RFC 9113 6.9).
        self.pump_stream(stream_id);

        Ok(stream_id)
    }

    /// Store who waits for `stream_id`'s reply, after the loop's owner parked
    /// the request. The reply arrives on `drain_completions` carrying both.
    pub fn set_waiter(&mut self, stream_id: u32, waiter: Waiter, ctx: Box<PendingUrlContext>) {
        if let Some(s) = self.streams.get_mut(&stream_id) {
            s.waiter = Some(waiter);
            s.ctx = Some(ctx);
        }
    }

    /// Take the completions this connection produced since the last drain.
    pub fn drain_completions(&mut self) -> Vec<H2Completion> {
        std::mem::take(&mut self.completions)
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
        if self.phase != Phase::Open && !self.start_progress() {
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
                        if flags & FLAG_END_HEADERS != 0 {
                            self.complete_stream(stream_id);
                        } else if let Some(s) = self.streams.get_mut(&stream_id) {
                            // The header block continues in CONTINUATION
                            // frames, so the reply ends when the block ends.
                            s.end_stream = true;
                        }
                    }
                }

                TYPE_CONTINUATION if stream_id > 0 => {
                    self.header_block_acc.extend_from_slice(payload);

                    if flags & FLAG_END_HEADERS != 0 {
                        if !self.finish_header_block(stream_id) {
                            return;
                        }
                        if self.streams.get(&stream_id).is_some_and(|s| s.end_stream) {
                            self.complete_stream(stream_id);
                        }
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
                        self.completions.push(H2Completion {
                            stream_id,
                            result: Err(io::Error::new(
                                io::ErrorKind::BrokenPipe,
                                format!("{}: stream reset by server", T::prefix()),
                            )),
                            waiter: s.waiter,
                            ctx: s.ctx,
                            deadline_slot: s.deadline_slot,
                        });
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

        // The frame loop may have resumed a body (WINDOW_UPDATE granted credit,
        // or SETTINGS raised the window) and appended fresh DATA frames after
        // the flush at the top of `drive`. Flush them now, in the same turn: an
        // edge-triggered poll will not re-report a socket that is already
        // writable, so leaving them would stall the request until some
        // unrelated event fires.
        if !self.send_buf.is_empty() {
            if let Err(reason) = self.transport.flush_send(&mut self.send_buf) {
                self.mark_dead(&reason);
            }
        }
    }

    /// Test-only: how many bytes are still queued to flush.
    #[cfg(test)]
    pub(crate) fn send_buf_len_for_test(&self) -> usize {
        self.send_buf.len()
    }

    /// Test-only: inject received bytes directly, bypassing the socket.
    #[cfg(test)]
    pub(crate) fn inject_recv_for_test(&mut self, bytes: &[u8]) {
        self.recv_buf.extend_from_slice(bytes);
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
                    self.completions.push(H2Completion {
                        stream_id,
                        result: Err(too_many_informational()),
                        waiter: s.waiter,
                        ctx: s.ctx,
                        deadline_slot: s.deadline_slot,
                    });
                }
                self.push_frame(TYPE_RST_STREAM, 0, stream_id, &8u32.to_be_bytes());
            }
        }
        true
    }

    /// Complete a stream: remove it and push the response (or an error) onto
    /// the completion list.
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
            let result = if s.headers_done {
                Ok(HttpResponse {
                    status: s.resp_status,
                    reason: String::new(),
                    headers: s.resp_headers,
                    body: s.resp_body,
                })
            } else {
                Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    format!("{}: stream ended without headers", T::prefix()),
                ))
            };
            self.completions.push(H2Completion {
                stream_id,
                result,
                waiter: s.waiter,
                ctx: s.ctx,
                deadline_slot: s.deadline_slot,
            });
        }
    }

    /// Fail one stream because its deadline passed: tell the peer to cancel it,
    /// remove it, and hand its completion to the caller.
    fn time_out_stream(&mut self, stream_id: u32) -> Option<H2Completion> {
        let s = self.streams.remove(&stream_id)?;
        self.push_frame(
            TYPE_RST_STREAM,
            0,
            stream_id,
            &8u32.to_be_bytes(), // CANCEL
        );
        Some(H2Completion {
            stream_id,
            result: Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("{}: upstream stream timed out", T::prefix()),
            )),
            waiter: s.waiter,
            ctx: s.ctx,
            deadline_slot: s.deadline_slot,
        })
    }

    /// Mark the connection as dead and drain all pending streams with an error.
    fn mark_dead(&mut self, reason: &str) {
        self.is_dead = true;
        let streams = std::mem::take(&mut self.streams);
        for (stream_id, s) in streams {
            self.completions.push(H2Completion {
                stream_id,
                result: Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    reason.to_string(),
                )),
                waiter: s.waiter,
                ctx: s.ctx,
                deadline_slot: s.deadline_slot,
            });
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
    /// Streams finished since the last drain, for the loop's owner to complete.
    completions: Vec<H2Completion>,
    /// One deadline per in-flight stream. A stream id cannot index this: it
    /// grows past 2^31, so each stream takes a slot from a fixed table when it
    /// is dispatched and gives it back when it completes.
    stream_deadlines: Timeouts,
    /// The connection token and stream id that own each deadline slot.
    deadline_owner: Vec<Option<(Token, u32)>>,
    free_deadline_slots: Vec<usize>,
}

impl<T: Transport> Pool<T> {
    /// `stream_timeout` is the backend deadline every stream is given.
    pub fn new(stream_timeout: Duration) -> Self {
        Self {
            connections: HashMap::new(),
            completions: Vec::with_capacity(64),
            stream_deadlines: Timeouts::new(64, stream_timeout),
            deadline_owner: Vec::with_capacity(64),
            free_deadline_slots: Vec::new(),
        }
    }

    /// Ensure a connection exists for `base_url`, creating one if none exists
    /// or the existing one is dead. `addr` is the backend's address resolved
    /// once at start. Returns whether one was created.
    pub fn ensure_connection(
        &mut self,
        base_url: &str,
        addr: SocketAddr,
        tls: Option<&TlsClient>,
    ) -> io::Result<bool> {
        let needs_new = match self.connections.get(base_url) {
            None => true,
            Some(conn) => conn.is_dead && conn.streams.is_empty(),
        };
        if needs_new {
            let new_conn = H2Client::connect(addr, tls)?;
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
    ///
    /// Returns the connection token and the outbound stream id, so the caller
    /// can store its waiter on the stream with [`Pool::set_waiter`].
    pub fn dispatch(
        &mut self,
        base_url: &str,
        req: &HttpRequest,
        client_ip: &str,
        original_host: &str,
        via: Option<&crate::forward::ViaEntries>,
        registry: &Registry,
    ) -> io::Result<(Token, u32)> {
        let (host, _) = parse_host_port(base_url, T::prefix(), T::default_port())?;
        let (token, stream_id) = {
            let conn = self.connections.get_mut(base_url).unwrap();
            let stream_id = conn.dispatch(req, &host, client_ip, original_host, via)?;
            Self::register_interest(conn, registry);
            (conn.token(), stream_id)
        };
        let slot = self.take_deadline_slot(token, stream_id, m6_core::platform::now());
        if let Some(stream) = self
            .connections
            .get_mut(base_url)
            .and_then(|c| c.streams.get_mut(&stream_id))
        {
            stream.deadline_slot = slot;
        }
        Ok((token, stream_id))
    }

    /// Register the interest the connection's phase now needs.
    ///
    /// Every path that drives a connection calls this afterwards, so a drive
    /// that queued frames asks for the write readiness they need. `dispatch`
    /// calls it without driving: driving there could complete a reply before
    /// the caller has stored its waiter.
    fn register_interest(conn: &mut H2Client<T>, registry: &Registry) {
        let interest = conn.phase_interest();
        if interest != conn.registered_interest {
            registry
                .reregister_fd(conn.raw_fd(), conn.token(), interest)
                .ok();
            conn.registered_interest = interest;
        }
    }

    /// A deadline slot for a new stream, grown on demand.
    fn take_deadline_slot(&mut self, token: Token, stream_id: u32, now: Instant) -> usize {
        let slot = match self.free_deadline_slots.pop() {
            Some(slot) => slot,
            None => {
                let slot = self.deadline_owner.len();
                self.deadline_owner.push(None);
                self.stream_deadlines.grow(slot + 1);
                slot
            }
        };
        self.stream_deadlines.touch(Token(slot), now);
        self.deadline_owner[slot] = Some((token, stream_id));
        slot
    }

    /// Give a completed stream's deadline slot back.
    fn release_deadline_slot(&mut self, slot: usize) {
        if slot >= self.deadline_owner.len() || self.deadline_owner[slot].is_none() {
            return;
        }
        self.stream_deadlines.clear(Token(slot));
        self.deadline_owner[slot] = None;
        self.free_deadline_slots.push(slot);
    }

    /// Time until the earliest stream deadline, capped at `tick`.
    pub fn timeout(&self, now: Instant, tick: Duration) -> Duration {
        self.stream_deadlines.timeout(now, tick)
    }

    /// Fail every stream past its deadline, cancelling it at the peer, and
    /// return the completions so the loop can answer each waiter, plus the
    /// tokens of connections the drives left dead. The caller reaps those like
    /// the ones [`Pool::drive_one`] reports, or their tokens leak.
    pub fn expire(&mut self, now: Instant, registry: &Registry) -> (Vec<Token>, Vec<H2Completion>) {
        let mut dead = Vec::new();
        let mut out = Vec::new();
        while let Some(slot) = self.stream_deadlines.expired(now) {
            let Some((token, stream_id)) = self.deadline_owner[slot.0].take() else {
                continue;
            };
            self.free_deadline_slots.push(slot.0);
            let Some(conn) = self.connections.values_mut().find(|c| c.token() == token) else {
                continue;
            };
            if let Some(completion) = conn.time_out_stream(stream_id) {
                out.push(completion);
                conn.drive();
                Self::register_interest(conn, registry);
                out.append(&mut conn.completions);
                if conn.is_dead && conn.streams.is_empty() {
                    dead.push(conn.token());
                }
            }
        }
        for completion in &out {
            self.release_deadline_slot(completion.deadline_slot);
        }
        (dead, out)
    }

    /// Store who waits for an outbound stream's reply.
    pub fn set_waiter(
        &mut self,
        token: Token,
        stream_id: u32,
        waiter: Waiter,
        ctx: Box<PendingUrlContext>,
    ) {
        if let Some(conn) = self.connections.values_mut().find(|c| c.token() == token) {
            conn.set_waiter(stream_id, waiter, ctx);
        }
    }

    /// Drive the one connection whose token fired. Returns whether it is dead
    /// with no pending streams, so the caller can free its token and remove it.
    /// Finished streams land in [`Pool::drain_completions`].
    pub fn drive_one(&mut self, token: Token, registry: &Registry) -> bool {
        let Some(conn) = self.connections.values_mut().find(|c| c.token() == token) else {
            return false;
        };
        conn.drive();
        Self::register_interest(conn, registry);
        self.completions.append(&mut conn.completions);
        conn.is_dead && conn.streams.is_empty()
    }

    /// Take the completions every driven connection produced since the last
    /// drain, clearing their deadlines.
    pub fn drain_completions(&mut self) -> Vec<H2Completion> {
        for i in 0..self.completions.len() {
            let slot = self.completions[i].deadline_slot;
            self.release_deadline_slot(slot);
        }
        std::mem::take(&mut self.completions)
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
                registry.deregister_fd(conn.raw_fd()).ok();
            }
        }
    }
}

/// Start a non-blocking TCP connect. In progress is success: the caller waits
/// for writability and then checks the socket's error.
pub(crate) fn nonblocking_tcp_connect(addr: SocketAddr) -> io::Result<TcpStream> {
    let (stream, _) = m6_core::platform::net::connect_tcp(addr)?;
    Ok(stream)
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
pub(crate) fn parse_host_port(
    base_url: &str,
    scheme: &str,
    default_port: u16,
) -> io::Result<(String, u16)> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;
    use std::net::SocketAddr;
    use std::os::unix::io::RawFd;

    /// A transport that records every flushed frame. It does no socket I/O: the
    /// client's `drive` flushes the HEADERS frame into `sent`, and the test
    /// reads it straight back, so the assertion sees exactly the bytes a peer
    /// would receive.
    struct CaptureTransport {
        sent: Vec<u8>,
    }

    impl Transport for CaptureTransport {
        fn connect(_addr: SocketAddr, _tls: Option<&TlsClient>) -> io::Result<Self> {
            Ok(Self { sent: Vec::new() })
        }

        fn raw_fd(&self) -> RawFd {
            0
        }

        fn take_error(&self) -> io::Result<Option<io::Error>> {
            Ok(None)
        }

        fn is_connected(&self) -> bool {
            true
        }

        fn is_handshaking(&self) -> bool {
            false
        }

        fn alpn_protocol(&self) -> Option<&[u8]> {
            None
        }

        fn scheme() -> &'static [u8] {
            b"http"
        }

        fn default_port() -> u16 {
            80
        }

        fn prefix() -> &'static str {
            "h2c"
        }

        fn flush_send(&mut self, send_buf: &mut Vec<u8>) -> Result<(), String> {
            self.sent.extend_from_slice(send_buf);
            send_buf.clear();
            Ok(())
        }

        fn fill_recv(&mut self, _recv_buf: &mut Vec<u8>) -> Option<String> {
            None
        }

        fn wants_write(&self) -> bool {
            false
        }
    }

    /// Dispatch one body-less request and decode the single HEADERS frame the
    /// client flushed, returning its decoded HPACK pairs.
    fn dispatched_headers(
        req: &HttpRequest,
        host: &str,
        original_host: &str,
    ) -> Vec<(Vec<u8>, Vec<u8>)> {
        let mut conn = H2Client::new(CaptureTransport { sent: Vec::new() });
        conn.dispatch(req, host, "127.0.0.1", original_host, None)
            .expect("dispatch");
        conn.drive();

        let sent = &conn.transport.sent;
        assert!(sent.len() >= 9, "the client flushed no frame: {sent:?}");
        let length = ((sent[0] as usize) << 16) | ((sent[1] as usize) << 8) | sent[2] as usize;
        assert_eq!(sent[3], TYPE_HEADERS, "expected a HEADERS frame: {sent:?}");
        assert_eq!(sent.len(), 9 + length, "frame length mismatch: {sent:?}");
        let stream_id = (((sent[5] as u32) << 24)
            | ((sent[6] as u32) << 16)
            | ((sent[7] as u32) << 8)
            | (sent[8] as u32))
            & 0x7fff_ffff;
        assert_eq!(stream_id, 1, "the first stream id must be 1");

        let mut dec = hpack::Decoder::new();
        dec.decode(&sent[9..])
            .expect("the client's HPACK block must decode")
    }

    #[test]
    fn a_request_without_host_is_forwarded_with_the_original_host() {
        let req = HttpRequest {
            method: "GET".to_string(),
            path: "/".to_string(),
            query: None,
            version: "HTTP/3".to_string(),
            // HTTP/3 carries the host in :authority, which ingress strips, so a
            // forwarded request names no Host at all. The h2 leg must still
            // carry it, exactly as the HTTP/1.1 leg synthesises `Host` from
            // `original_host`.
            headers: vec![],
            body: vec![],
        };
        let pairs = dispatched_headers(&req, "10.0.0.1:8443", "example.com");
        assert!(
            pairs
                .iter()
                .any(|(k, v)| k == b"host" && v == b"example.com"),
            "the forward must carry host: example.com, got: {pairs:?}"
        );
        assert!(
            pairs
                .iter()
                .any(|(k, v)| k == b":authority" && v == b"10.0.0.1:8443"),
            ":authority stays the backbone address, got: {pairs:?}"
        );
    }
}
