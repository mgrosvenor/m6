//! One outgoing HTTP/1.1 request an event loop drives, without blocking.
//!
//! An [`Upstream`] is a slot made once at start, with the buffers every request
//! reuses. The caller writes the request into [`Upstream::request_buf`], calls
//! [`Upstream::start`] to begin a non-blocking connect (or reuse of the slot's
//! open keep-alive connection), registers [`Upstream::fd`] with
//! [`Upstream::interest`], and calls [`Upstream::drive`] each time the
//! descriptor fires. When `drive` reports the reply is ready, [`Upstream::reply`]
//! parses the head in place with `httparse` and returns a [`Reply`] that borrows
//! the slot's buffers and the caller's header slots. No heap allocation happens
//! on the request path.

use std::io::{self, Read, Write};
use std::net::SocketAddr;
use std::os::fd::{AsRawFd, RawFd};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use rustls::ClientConnection;

/// The default ceiling on a backend response body, the `upstream_read`
/// maximum. A hostile or misbehaving backend must not grow the process without
/// limit, so a declared or decoded body above the maximum is refused before it
/// is read.
pub const MAX_BODY: usize = crate::limits::UPSTREAM_READ.max;

/// The most informational (1xx) responses one fetch skips before the backend
/// must send a final one. A bound, so a backend cannot hold the caller by
/// sending them without end.
pub const MAX_INFORMATIONAL: usize = 8;

/// The number of header slots a caller hands to [`Upstream::reply`]. The slots
/// are an uninitialised array on the caller's stack, so building them costs
/// nothing.
pub const MAX_HEADERS: usize = 64;

/// The slots [`Upstream::reply`] parses a response head into.
///
/// The caller owns them, as httparse intends: the storage sits outside the
/// parse, the parse borrows it, and no reference to the parser outlives the
/// call. [`reply_slots`] makes a set without writing anything.
pub type ReplySlots<'a> = [std::mem::MaybeUninit<httparse::Header<'a>>; MAX_HEADERS];

/// A new set of empty slots.
pub fn reply_slots<'a>() -> ReplySlots<'a> {
    [const { std::mem::MaybeUninit::uninit() }; MAX_HEADERS]
}

/// Ceiling on a response head (status line and headers). Matches the blocking
/// reader's stack buffer, so the two never disagree about what fits.
const MAX_HEAD: usize = 8192;

/// Where a request goes: a Unix socket or a TCP address.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Addr {
    Unix(PathBuf),
    Tcp(SocketAddr),
}

/// A rustls client config and the name to verify, built at start.
#[derive(Clone)]
pub struct TlsClient {
    pub config: Arc<rustls::ClientConfig>,
    pub name: rustls::pki_types::ServerName<'static>,
}

/// The host's certificate store, for a client that makes TLS connections.
///
/// Start only: it reads the platform store, which blocks. A service that
/// trusts the host's store trusts what its operator's tools trust, and a
/// renewal there reaches the service at its next restart, as every other
/// start-time read does. The alternative, a bundled store, is a second answer
/// to which certificates are trusted and it goes stale inside the binary.
///
/// A store that yields no certificate is an error, never an empty trust set: a
/// client that trusts nothing fails every handshake with an error that blames
/// the peer. So is a store whose every certificate fails to parse. There is no
/// third branch and no verifier that accepts anything.
pub fn system_roots() -> io::Result<rustls::RootCertStore> {
    roots_from(rustls_native_certs::load_native_certs().certs)
}

/// The trust store built from the certificates a platform store returned.
///
/// Split from [`system_roots`] so the two fail-closed branches have a test:
/// a store with no certificate, and a store with none that parses.
fn roots_from(
    certs: impl IntoIterator<Item = rustls::pki_types::CertificateDer<'static>>,
) -> io::Result<rustls::RootCertStore> {
    let certs: Vec<_> = certs.into_iter().collect();
    if certs.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "no certificate authority in the system store (the platform certificate store loaded no certificate)",
        ));
    }
    let mut roots = rustls::RootCertStore::empty();
    let (added, ignored) = roots.add_parsable_certificates(certs);
    if added == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "no certificate authority in the system store (the platform certificate store loaded {ignored} certificate(s) and none could be parsed)"
            ),
        ));
    }
    Ok(roots)
}

/// One URL a service posts forms to, resolved at start.
///
/// [`Endpoint::new`] parses the URL, resolves its host once with
/// [`crate::net::resolve`] and keeps the `Host` header, path and query.
/// [`Endpoint::post_form`] encodes an `application/x-www-form-urlencoded` POST
/// into an [`Upstream`]'s request buffer and starts it, so the service drives
/// the request from its own loop exactly as it drives a plain `Upstream`. One
/// connection per call: there is no idle list. HTTPS uses the config given at
/// start.
#[cfg(feature = "http-client")]
pub struct Endpoint {
    addr: Addr,
    https: bool,
    /// The `Host` header value, with the port when it is not the scheme's.
    host: String,
    /// The request target: path plus query.
    target: String,
    tls: Arc<rustls::ClientConfig>,
    name: rustls::pki_types::ServerName<'static>,
}

#[cfg(feature = "http-client")]
impl Endpoint {
    /// At start: parse an `http` or `https` URL, resolve its host once with
    /// [`crate::net::resolve`], and keep the `Host` header, path and query.
    ///
    /// `tls` is used for `https` and ignored for `http`. Any failure is an
    /// error, so a misconfigured URL stops the service at start.
    pub fn new(url: &str, tls: Arc<rustls::ClientConfig>) -> io::Result<Endpoint> {
        let parsed = url::Url::parse(url)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, format!("{url}: {e}")))?;
        let https = match parsed.scheme() {
            "http" => false,
            "https" => true,
            other => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("{url}: scheme {other:?} is not http or https"),
                ))
            }
        };
        let host = parsed.host_str().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, format!("{url}: no host"))
        })?;
        let default_port = if https { 443 } else { 80 };
        let port = parsed.port_or_known_default().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, format!("{url}: no port"))
        })?;
        let addr = crate::net::resolve(host, port)?;
        let name = rustls::pki_types::ServerName::try_from(host.to_owned())
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, format!("{url}: {e}")))?;
        let mut host_header = host.to_owned();
        if port != default_port {
            use std::fmt::Write as _;
            let _ = write!(host_header, ":{port}");
        }
        let mut target = parsed.path().to_owned();
        if let Some(query) = parsed.query() {
            target.push('?');
            target.push_str(query);
        }
        Ok(Endpoint {
            addr: Addr::Tcp(addr),
            https,
            host: host_header,
            target,
            tls,
            name,
        })
    }

    /// Encode a POST of `fields` and start it on `up`.
    ///
    /// The body is `application/x-www-form-urlencoded` and the request asks
    /// `Connection: close`. `Content-Length` counts the encoded bytes, so the
    /// fields are encoded twice into the buffer rather than through an
    /// intermediate `String`: once to sum the length, once to write.
    pub fn post_form(
        &self,
        up: &mut Upstream,
        fields: &[(&str, &str)],
        deadline: Instant,
    ) -> io::Result<()> {
        let mut body_len = 0usize;
        for (i, (name, value)) in fields.iter().enumerate() {
            if i > 0 {
                body_len += 1;
            }
            body_len += encoded_len(name) + 1 + encoded_len(value);
        }
        let request = up.request_buf();
        write!(
            request,
            "POST {} HTTP/1.1\r\nHost: {}\r\nContent-Type: application/x-www-form-urlencoded\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            self.target, self.host, body_len
        )?;
        for (i, (name, value)) in fields.iter().enumerate() {
            if i > 0 {
                request.push(b'&');
            }
            encode_into(name, request);
            request.push(b'=');
            encode_into(value, request);
        }
        let tls = self.https.then(|| TlsClient {
            config: self.tls.clone(),
            name: self.name.clone(),
        });
        up.start(&self.addr, tls.as_ref(), false, deadline)
    }
}

/// The encoded length of one form field name or value.
#[cfg(feature = "http-client")]
fn encoded_len(s: &str) -> usize {
    url::form_urlencoded::byte_serialize(s.as_bytes())
        .map(str::len)
        .sum()
}

/// Append one form field name or value, encoded, straight into `out`.
#[cfg(feature = "http-client")]
fn encode_into(s: &str, out: &mut Vec<u8>) {
    for piece in url::form_urlencoded::byte_serialize(s.as_bytes()) {
        out.extend_from_slice(piece.as_bytes());
    }
}

/// A parsed response head and body, borrowing the slot and the caller's header
/// slots until the next [`Upstream::start`].
pub struct Reply<'a> {
    pub status: u16,
    pub headers: &'a [httparse::Header<'a>],
    pub body: &'a [u8],
}

/// A response body's framing, decided once from its header section.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Framing {
    /// No body: a HEAD request, or status 101, 204 or 304.
    Bodyless,
    /// `Content-Length: N`.
    ContentLength(usize),
    /// `Transfer-Encoding: chunked`.
    Chunked,
    /// No framing: read to end of stream.
    CloseDelimited,
}

/// What the next `drive` call should wait for and work on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    /// No open connection.
    Idle,
    /// A TCP connect is in progress: wait for writable, then check the result.
    Connecting,
    /// The TLS handshake is in progress.
    Handshaking,
    /// Write what is left of the request, then read and parse the response.
    Transfer,
}

/// A connected stream, Unix or TCP.
enum Stream {
    Unix(std::os::unix::net::UnixStream),
    Tcp(std::net::TcpStream),
}

impl Stream {
    fn as_raw_fd(&self) -> RawFd {
        match self {
            Stream::Unix(s) => s.as_raw_fd(),
            Stream::Tcp(s) => s.as_raw_fd(),
        }
    }

    /// The pending socket error for a TCP connect, or `None` when none is.
    fn take_error(&self) -> io::Result<Option<io::Error>> {
        match self {
            Stream::Tcp(s) => s.take_error(),
            Stream::Unix(_) => Ok(None),
        }
    }

    /// Whether the socket has a peer, which a TCP connect gets only when it
    /// completes. A Unix connect completes at once.
    fn is_connected(&self) -> bool {
        match self {
            Stream::Tcp(s) => s.peer_addr().is_ok(),
            Stream::Unix(_) => true,
        }
    }
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Stream::Unix(s) => s.read(buf),
            Stream::Tcp(s) => s.read(buf),
        }
    }
}

impl Write for Stream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Stream::Unix(s) => s.write(buf),
            Stream::Tcp(s) => s.write(buf),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        match self {
            Stream::Unix(s) => s.flush(),
            Stream::Tcp(s) => s.flush(),
        }
    }
}

/// One outgoing HTTP/1.1 request a loop drives. The request and read buffers are
/// allocated at [`Upstream::new`] and reused for every request, so a request
/// over a Unix socket or plain TCP allocates nothing.
pub struct Upstream {
    stream: Option<Stream>,
    tls: Option<ClientConnection>,
    phase: Phase,
    /// The request bytes for the current request, written by the caller.
    request: Vec<u8>,
    /// The `upstream_request` limit `request` is held to, checked at `start`.
    request_limit: crate::limits::Limit,
    /// The `upstream_read` limit `read_buf` grows under.
    read_limit: crate::limits::Limit,
    write_offset: usize,
    /// The read buffer. Its length always equals its capacity, so reads write
    /// straight into the spare region and grow only by doubling, not per request.
    read_buf: Vec<u8>,
    /// Bytes of `read_buf[..read_len]` that hold response data.
    read_len: usize,
    // Response parse state, kept across `drive` calls so a byte that arrives in
    // a later read is not parsed again from the start.
    head_offset: usize,
    informational: usize,
    head_start: usize,
    /// Offset of the `\r` of the final `\r\n\r\n`.
    head_end: usize,
    framing: Option<Framing>,
    status: u16,
    /// Chunked decode: the next undecoded byte, and whether the `0` chunk has
    /// been seen and the trailer section is being consumed.
    chunk_pos: usize,
    in_trailers: bool,
    /// Decoded body length. The body is `read_buf[head_end + 4 ..][..body_len]`.
    body_len: usize,
    eof: bool,
    deadline: Instant,
    method_is_head: bool,
    reusable: bool,
    /// Whether the whole request has been written to the socket. An early reply
    /// can arrive before the write finishes; reuse then truncates the request
    /// and desynchronises the backend, so reuse requires this.
    request_finished: bool,
    /// The peer stopped accepting request bytes (EPIPE or ECONNRESET) before
    /// the request was written. Its reply may still be waiting, so only the
    /// read side continues and the connection is never reusable.
    write_failed: bool,
    /// Test seam: when set, the next `write_tls` on this connection reports a
    /// WouldBlock once. Per-connection, so the parallel test runner cannot
    /// cross the flag between tests.
    #[cfg(test)]
    block_next_write: std::cell::Cell<bool>,
    /// Test seam: when set, the next request write fails with an error that is
    /// not a refusal, so the early-reply path can be pinned independently of
    /// the two error kinds a reset happens to produce.
    #[cfg(test)]
    fail_next_write: std::cell::Cell<bool>,
    /// The address of the open connection, for keep-alive reuse.
    connected: Option<Addr>,
    /// The TLS identity of the open connection: the verified server name and the
    /// config that verified it. Reuse requires the same identity, so one slot
    /// cannot carry a second request over a connection authenticated for a
    /// different name or trust root.
    connected_name: Option<rustls::pki_types::ServerName<'static>>,
    connected_config: Option<Arc<rustls::ClientConfig>>,
}

impl Upstream {
    /// At start: the buffers this slot reuses for every request, with the
    /// built-in maxima of `upstream_read` and `upstream_request`.
    pub fn new(read_cap: usize, write_cap: usize) -> Upstream {
        use crate::limits::{Limit, UPSTREAM_READ, UPSTREAM_REQUEST};
        let read = Limit::custom(
            &UPSTREAM_READ,
            read_cap.max(1),
            UPSTREAM_READ.max.max(read_cap),
        );
        let request = Limit::custom(
            &UPSTREAM_REQUEST,
            write_cap.max(1),
            UPSTREAM_REQUEST.max.max(write_cap),
        );
        Upstream::with_limits(&read, &request)
    }

    /// At start: the buffers this slot reuses for every request, following the
    /// `upstream_read` and `upstream_request` limits.
    pub fn with_limits(read: &crate::limits::Limit, request: &crate::limits::Limit) -> Upstream {
        Upstream {
            stream: None,
            tls: None,
            phase: Phase::Idle,
            request: Vec::with_capacity(request.start()),
            request_limit: request.clone(),
            read_limit: read.clone(),
            write_offset: 0,
            read_buf: vec![0; read.start()],
            read_len: 0,
            head_offset: 0,
            informational: 0,
            head_start: 0,
            head_end: 0,
            framing: None,
            status: 0,
            chunk_pos: 0,
            in_trailers: false,
            body_len: 0,
            eof: false,
            deadline: crate::platform::now(),
            method_is_head: false,
            reusable: false,
            request_finished: false,
            write_failed: false,
            #[cfg(test)]
            block_next_write: std::cell::Cell::new(false),
            #[cfg(test)]
            fail_next_write: std::cell::Cell::new(false),
            connected: None,
            connected_name: None,
            connected_config: None,
        }
    }

    /// The request bytes for the next `start`: cleared, capacity kept.
    pub fn request_buf(&mut self) -> &mut Vec<u8> {
        self.request.clear();
        &mut self.request
    }

    /// A non-blocking connect, or reuse of this slot's open keep-alive
    /// connection to the same address and TLS identity. Never waits.
    pub fn start(
        &mut self,
        addr: &Addr,
        tls: Option<&TlsClient>,
        method_is_head: bool,
        deadline: Instant,
    ) -> io::Result<()> {
        // A request the caller built past the `upstream_request` maximum is
        // refused here, before any byte is sent.
        if self.request.len() > self.request_limit.max() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                self.request_limit
                    .exceeded(self.request.len(), "backend request"),
            ));
        }
        // Decide reuse before resetting, because `reusable` describes the last
        // reply and is cleared with the rest of the per-request state below.
        let can_reuse = self.stream.is_some()
            && self.reusable
            && self.request_finished
            && self.connected.as_ref() == Some(addr)
            && self.tls_identity_matches(tls);

        // Reset the per-request state, keeping the buffers and their capacity.
        self.write_offset = 0;
        self.read_len = 0;
        self.head_offset = 0;
        self.informational = 0;
        self.head_start = 0;
        self.head_end = 0;
        self.framing = None;
        self.status = 0;
        self.chunk_pos = 0;
        self.in_trailers = false;
        self.body_len = 0;
        self.eof = false;
        self.deadline = deadline;
        self.method_is_head = method_is_head;
        self.reusable = false;
        self.request_finished = false;
        self.write_failed = false;

        if can_reuse {
            self.phase = Phase::Transfer;
            return Ok(());
        }

        // Close the previous connection (the buffers stay), then connect.
        self.stream = None;
        self.tls = None;
        self.connect(addr, tls)
    }

    /// Whether the TLS identity of the open connection matches the one being
    /// started: the same verified server name and the same config (by pointer).
    fn tls_identity_matches(&self, tls: Option<&TlsClient>) -> bool {
        match (tls, &self.connected_name, &self.connected_config) {
            (None, None, None) => true,
            (Some(t), Some(name), Some(config)) => {
                *name == t.name && Arc::ptr_eq(config, &t.config)
            }
            _ => false,
        }
    }

    /// Open a non-blocking socket and begin the connect.
    fn connect(&mut self, addr: &Addr, tls: Option<&TlsClient>) -> io::Result<()> {
        let tls_conn = match tls {
            Some(t) => Some(
                ClientConnection::new(t.config.clone(), t.name.clone())
                    .map_err(io::Error::other)?,
            ),
            None => None,
        };
        match addr {
            Addr::Unix(path) => {
                // A Unix connect completes at once or fails. On Linux WouldBlock
                // means the listen backlog is full, which the caller treats as a
                // failed connect and tries the next member for the same request.
                // macOS answers ConnectionRefused for a full queue and for no
                // listener alike, so the same rule applies there without a
                // separate case.
                let sock = crate::platform::net::connect_unix(path)?;
                self.stream = Some(Stream::Unix(sock));
                self.tls = tls_conn;
                self.phase = if self.tls.is_some() {
                    Phase::Handshaking
                } else {
                    Phase::Transfer
                };
            }
            Addr::Tcp(addr) => {
                // A non-blocking TCP connect may still be in progress: the
                // caller then waits for writable, and `take_error` decides.
                let (sock, state) = crate::platform::net::connect_tcp(*addr)?;
                self.tls = tls_conn;
                self.phase = match state {
                    crate::platform::net::Connect::InProgress => Phase::Connecting,
                    crate::platform::net::Connect::Connected if self.tls.is_some() => {
                        Phase::Handshaking
                    }
                    crate::platform::net::Connect::Connected => Phase::Transfer,
                };
                self.stream = Some(Stream::Tcp(sock));
            }
        }
        // Reuse the previous address's buffer when the slot reconnects, so a
        // fresh connection does not allocate a path each time. `Addr` derives
        // `Clone`, so clone into the inner variant by hand.
        match (&mut self.connected, addr) {
            (Some(Addr::Unix(existing)), Addr::Unix(path)) => existing.clone_from(path),
            (Some(Addr::Tcp(existing)), Addr::Tcp(ip)) => *existing = *ip,
            (_, addr) => self.connected = Some(addr.clone()),
        }
        self.connected_name = tls.map(|t| t.name.clone());
        self.connected_config = tls.map(|t| t.config.clone());
        Ok(())
    }

    /// The descriptor to register with the poll.
    pub fn fd(&self) -> RawFd {
        self.stream.as_ref().expect("no connection").as_raw_fd()
    }

    /// The interest to register for the current phase.
    pub fn interest(&self) -> crate::platform::Interest {
        match self.phase {
            Phase::Connecting => crate::platform::Interest::WRITABLE,
            Phase::Handshaking => {
                crate::platform::Interest::READABLE | crate::platform::Interest::WRITABLE
            }
            Phase::Transfer => {
                if self.wants_write() {
                    crate::platform::Interest::READABLE | crate::platform::Interest::WRITABLE
                } else {
                    crate::platform::Interest::READABLE
                }
            }
            Phase::Idle => crate::platform::Interest::READABLE,
        }
    }

    /// Whether request bytes or TLS ciphertext should be written to the socket.
    fn wants_write(&self) -> bool {
        !self.write_failed
            && (self.write_offset < self.request.len()
                || self.tls.as_ref().is_some_and(|t| t.wants_write()))
    }

    /// Do what can be done without blocking. Returns `Ok(true)` when the reply
    /// is ready for [`Upstream::reply`], `Ok(false)` when the descriptor must be
    /// driven again.
    pub fn drive(&mut self) -> io::Result<bool> {
        match self.phase {
            Phase::Idle => Ok(false),
            Phase::Connecting => {
                if !self.finish_connect()? {
                    return Ok(false);
                }
                self.transfer()
            }
            Phase::Handshaking => {
                if !self.drive_handshake()? {
                    return Ok(false);
                }
                self.phase = Phase::Transfer;
                self.transfer()
            }
            Phase::Transfer => self.transfer(),
        }
    }

    /// Check a completed TCP connect. `Ok(false)` when it is still in progress.
    fn finish_connect(&mut self) -> io::Result<bool> {
        let stream = self.stream.as_ref().expect("connecting stream");
        if let Some(e) = stream.take_error()? {
            return Err(e);
        }
        // `take_error` reports no error while a non-blocking connect is still
        // in progress, and a write to such a socket fails with ENOTCONN. The
        // peer address exists only once the handshake has completed.
        if !stream.is_connected() {
            return Ok(false);
        }
        self.phase = if self.tls.is_some() {
            Phase::Handshaking
        } else {
            Phase::Transfer
        };
        Ok(true)
    }

    /// Drive the TLS handshake until it completes or the socket would block.
    fn drive_handshake(&mut self) -> io::Result<bool> {
        let (Some(tls), Some(stream)) = (&mut self.tls, &mut self.stream) else {
            return Ok(true);
        };
        drive_tls_handshake(tls, stream)
    }

    /// Write the rest of the request, then read and parse. Returns `Ok(true)`
    /// when the reply is complete.
    fn transfer(&mut self) -> io::Result<bool> {
        if self.wants_write() {
            if let Err(write_err) = self.write_request() {
                // The peer may have answered before it refused the request, so
                // check what is already waiting before the write error wins:
                // RFC 9112 section 9.5 makes the early response the answer, and
                // a complete one is not a failed request. A read error here is
                // not fatal by itself; `parse_progress` decides.
                let _ = self.read_available();
                if let Ok(true) = self.parse_progress() {
                    return Ok(true);
                }
                return Err(write_err);
            }
        }
        self.read_available()?;
        self.parse_progress()
    }

    /// Write what is left of the request. A backend may answer before it has
    /// read the whole request: RFC 9112 section 9.3 is the rule for a server
    /// that responds early, it must read the remainder and discard it or close
    /// the connection, and this client does the same thing from the other side,
    /// keeping the read half open so the answer is not lost. RFC 9110 section
    /// 6.1 is about what makes a message complete, which is what decides
    /// whether the request can be reused, not whether it may be answered.
    fn write_request(&mut self) -> io::Result<()> {
        #[cfg(test)]
        if self.fail_next_write.replace(false) {
            return Err(io::Error::other("test: the request write failed"));
        }
        loop {
            let written = match (&mut self.tls, &mut self.stream) {
                (Some(tls), Some(_)) => {
                    match tls.writer().write(&self.request[self.write_offset..]) {
                        Ok(n) => n,
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => 0,
                        Err(e) if write_refused(&e) => {
                            self.write_failed = true;
                            return Ok(());
                        }
                        Err(e) => return Err(e),
                    }
                }
                (None, Some(stream)) => match stream.write(&self.request[self.write_offset..]) {
                    Ok(n) => n,
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                    Err(e) if write_refused(&e) => {
                        self.write_failed = true;
                        return Ok(());
                    }
                    Err(e) => return Err(e),
                },
                (None, None) => return Ok(()),
                (Some(_), None) => return Ok(()),
            };
            self.write_offset += written;
            if self.write_offset >= self.request.len() {
                // Flush any TLS ciphertext that is still buffered. If it would
                // block, report it so a later drive retries the flush.
                if !self.flush_request()? || self.write_failed {
                    return Ok(());
                }
                self.request_finished = true;
                return Ok(());
            }
            if self.tls.is_some() {
                // Flush the ciphertext so the plaintext buffer drains, then loop.
                if !self.flush_request()? || self.write_failed {
                    return Ok(());
                }
                if written == 0 {
                    // The plaintext buffer is full and nothing was flushed.
                    return Ok(());
                }
            }
        }
    }

    /// Flush pending TLS ciphertext to the socket. Returns `Ok(false)` when the
    /// socket would block, and `Ok(true)` once it is flushed or the peer has
    /// refused the write. A refusal sets `write_failed`, so the request is
    /// abandoned and only the reply is read from there on.
    fn flush_request(&mut self) -> io::Result<bool> {
        let (Some(tls), Some(stream)) = (&mut self.tls, &mut self.stream) else {
            return Ok(true);
        };
        #[cfg(test)]
        let flushed = flush_tls(tls, stream, &self.block_next_write);
        #[cfg(not(test))]
        let flushed = flush_tls(tls, stream);
        match flushed {
            Ok(flushed) => Ok(flushed),
            Err(e) if write_refused(&e) => {
                self.write_failed = true;
                Ok(true)
            }
            Err(e) => Err(e),
        }
    }

    /// Read what the socket has into `read_buf`, until WouldBlock or EOF. The
    /// read goes straight into the slot's buffer, with no stack buffer.
    fn read_available(&mut self) -> io::Result<()> {
        loop {
            if self.read_len == self.read_buf.len() && !self.grow_read_buf() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    self.read_limit
                        .exceeded(self.read_buf.len() + 1, "backend response"),
                ));
            }
            let before = self.read_len;
            match (&mut self.tls, &mut self.stream) {
                (Some(tls), Some(stream)) => {
                    if read_tls_into(tls, stream, &mut self.read_buf, &mut self.read_len)? {
                        self.eof = true;
                    }
                }
                (None, Some(stream)) => match stream.read(&mut self.read_buf[self.read_len..]) {
                    Ok(0) => self.eof = true,
                    Ok(n) => self.read_len += n,
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(e)
                        if e.kind() == io::ErrorKind::ConnectionReset
                            || e.kind() == io::ErrorKind::BrokenPipe =>
                    {
                        self.eof = true;
                        break;
                    }
                    Err(e) => return Err(e),
                },
                _ => break,
            }
            if self.eof || self.read_len == before {
                // No progress and no more buffered plaintext: stop.
                break;
            }
        }
        Ok(())
    }

    /// Double the read buffer, up to the body ceiling plus head room. Returns
    /// `false` when the buffer is already at the ceiling.
    fn grow_read_buf(&mut self) -> bool {
        let ceiling = self.read_limit.max() + MAX_HEAD;
        let Some(new) =
            self.read_limit
                .grown_to(self.read_buf.len(), self.read_buf.len() + 1, ceiling)
        else {
            return false;
        };
        let from = self.read_buf.len();
        self.read_buf.resize(new, 0);
        self.read_limit
            .log_growth(from, new, "an upstream response");
        true
    }

    /// Advance the incremental response parser over the bytes read so far.
    /// Returns `Ok(true)` when the response is complete.
    fn parse_progress(&mut self) -> io::Result<bool> {
        if self.framing.is_none() && !self.locate_head()? {
            return Ok(false);
        }
        let framing = self.framing.expect("head located");
        match framing {
            Framing::Bodyless => Ok(true),
            Framing::ContentLength(len) => {
                let body_end = self.head_end + 4 + len;
                if self.read_len >= body_end {
                    self.body_len = len;
                    Ok(true)
                } else if self.eof {
                    Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "connection closed before the body was complete",
                    ))
                } else {
                    Ok(false)
                }
            }
            Framing::CloseDelimited => {
                if self.eof {
                    self.body_len = self.read_len - (self.head_end + 4);
                    Ok(true)
                } else {
                    Ok(false)
                }
            }
            Framing::Chunked => self.decode_chunked(),
        }
    }

    /// Locate the final response head, skipping informational (1xx) heads.
    /// Returns `Ok(true)` when the final head is located and its framing set.
    fn locate_head(&mut self) -> io::Result<bool> {
        loop {
            let Some(rel_end) = find_header_end(&self.read_buf[self.head_offset..self.read_len])
            else {
                if self.read_len - self.head_offset >= MAX_HEAD {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "response headers too large",
                    ));
                }
                if self.eof {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "connection closed before headers complete",
                    ));
                }
                return Ok(false);
            };
            let header_end = self.head_offset + rel_end;
            let head = &self.read_buf[self.head_offset..header_end];
            let line_end = head
                .windows(2)
                .position(|w| w == b"\r\n")
                .unwrap_or(head.len());
            let status_line = std::str::from_utf8(&head[..line_end]).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidData, "response headers not UTF-8")
            })?;
            let (status, _reason, http10) = parse_status_line(status_line)?;
            if is_skipped_informational(status) {
                self.informational += 1;
                if self.informational > MAX_INFORMATIONAL {
                    return Err(too_many_informational());
                }
                self.head_offset = header_end + 4;
                continue;
            }
            self.head_start = self.head_offset;
            self.head_end = header_end;
            self.status = status;
            let after = line_end + 2;
            self.framing = Some(parse_framing(
                &head[after..],
                status,
                self.method_is_head,
                http10,
                &mut self.reusable,
                &self.read_limit,
            )?);
            self.chunk_pos = header_end + 4;
            return Ok(true);
        }
    }

    /// Decode the chunked body in place, removing the chunk framing so the body
    /// is one contiguous slice at the front of the body region.
    fn decode_chunked(&mut self) -> io::Result<bool> {
        let body_start = self.head_end + 4;
        loop {
            let Some(crlf) = find_crlf(&self.read_buf[self.chunk_pos..self.read_len]) else {
                if self.eof {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "chunked: body ended before the trailer section was terminated",
                    ));
                }
                return Ok(false);
            };
            let line = &self.read_buf[self.chunk_pos..self.chunk_pos + crlf];
            if self.in_trailers {
                if crlf == 0 {
                    return Ok(true);
                }
                let text = std::str::from_utf8(line).map_err(|_| {
                    io::Error::new(io::ErrorKind::InvalidData, "chunked: trailer not utf8")
                })?;
                match text.split_once(':') {
                    Some((name, _)) if is_field_name(name.trim()) => {}
                    _ => {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!("chunked: malformed trailer field {text:?}"),
                        ))
                    }
                }
                self.chunk_pos += crlf + 2;
                continue;
            }
            let size_text = std::str::from_utf8(line).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidData, "chunked: size not utf8")
            })?;
            let size_str = size_text.split(';').next().unwrap_or("").trim();
            if size_str.is_empty() || !size_str.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("chunked: invalid chunk size {size_str:?}"),
                ));
            }
            let size = usize::from_str_radix(size_str, 16).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "chunked: chunk size out of range",
                )
            })?;
            let data_start = self.chunk_pos + crlf + 2;
            if size == 0 {
                self.chunk_pos = data_start;
                self.in_trailers = true;
                continue;
            }
            if size > self.read_limit.max().saturating_sub(self.body_len) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    self.read_limit.exceeded(
                        self.body_len.saturating_add(size),
                        "chunked backend response",
                    ),
                ));
            }
            let data_end = data_start + size;
            if data_end + 2 > self.read_len {
                if self.eof {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "chunked: data shorter than declared size",
                    ));
                }
                return Ok(false);
            }
            let dest = body_start + self.body_len;
            self.read_buf.copy_within(data_start..data_end, dest);
            self.body_len += size;
            self.chunk_pos = data_end;
            if &self.read_buf[self.chunk_pos..self.chunk_pos + 2] != b"\r\n" {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "chunked: chunk data not terminated by CRLF",
                ));
            }
            self.chunk_pos += 2;
        }
    }

    /// Parse the completed head into the caller's header slots and return the
    /// reply. Only valid once [`Upstream::drive`] has reported the reply ready.
    ///
    /// `slots` is an uninitialised array of [`MAX_HEADERS`] entries, so building
    /// it costs nothing: httparse writes only the entries it fills.
    pub fn reply<'a>(&'a self, slots: &'a mut ReplySlots<'a>) -> Option<Reply<'a>> {
        self.framing?;
        let head = &self.read_buf[self.head_start..self.head_end + 4];
        let mut resp = httparse::Response::new(&mut []);
        let Ok(httparse::Status::Complete(_)) = httparse::ParserConfig::default()
            .parse_response_with_uninit_headers(&mut resp, head, slots)
        else {
            return None;
        };
        let status = resp.code.unwrap_or(self.status);
        let headers: &'a [httparse::Header<'a>] = &*resp.headers;
        let body_start = self.head_end + 4;
        let body = &self.read_buf[body_start..body_start + self.body_len];
        Some(Reply {
            status,
            headers,
            body,
        })
    }

    /// The backend deadline the caller set at `start`.
    pub fn deadline(&self) -> Instant {
        self.deadline
    }

    /// The last reply had keep-alive framing and the whole request was written,
    /// so the connection can carry another request.
    pub fn reusable(&self) -> bool {
        self.reusable && self.request_finished
    }

    /// Whether the whole request was written to the socket.
    ///
    /// False when the peer stopped accepting bytes. A backend that rejects an
    /// oversized request closes with most of it still unread, so the failure
    /// that follows is a refusal and says nothing about the member's health.
    pub fn request_written(&self) -> bool {
        self.request_finished
    }

    /// Whether a write was refused, which is the client's early answer.
    ///
    /// Not the same as "the request was not written": a would-block also
    /// leaves it unfinished, while this is the peer having answered and closed
    /// its side, so the connection cannot carry another request.
    pub fn write_was_refused(&self) -> bool {
        self.write_failed
    }

    /// Drop the connection (closes the socket). The buffers stay, including the
    /// last address, so the next connect reuses its allocation.
    pub fn close(&mut self) {
        self.stream = None;
        self.tls = None;
        self.phase = Phase::Idle;
        self.reusable = false;
    }
}

/// Whether a write error means the peer stopped accepting request bytes. A
/// backend that answers early closes without reading the rest of the request,
/// and its reply is still readable, so the error only ends the write side.
fn write_refused(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::BrokenPipe | io::ErrorKind::ConnectionReset
    )
}

// Test seam (per-connection): when the caller's `block_next_write` flag is set,
// the next `write_tls` reports a WouldBlock once, so the flush's retry is
// exercised without filling the socket send buffer.

/// Flush TLS ciphertext to the socket. Returns `Ok(true)` when all pending
/// bytes are written, `Ok(false)` when the socket would block.
fn flush_tls(
    tls: &mut ClientConnection,
    stream: &mut Stream,
    #[cfg(test)] block_next_write: &std::cell::Cell<bool>,
) -> io::Result<bool> {
    #[cfg(test)]
    if !tls.is_handshaking() && block_next_write.replace(false) {
        return Ok(false);
    }
    flush_tls_to(tls, stream)
}

/// Write pending TLS ciphertext to `stream` until it would block.
///
/// The one writer shared by [`Upstream`] and [`crate::email::Smtp`], so the
/// socket and rustls handshake in one place.
pub(crate) fn flush_tls_to(
    tls: &mut ClientConnection,
    stream: &mut impl Write,
) -> io::Result<bool> {
    while tls.wants_write() {
        match tls.write_tls(stream) {
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(false),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(true)
}

/// Feed the socket into `tls` and drain its plaintext into `buf[*len..]`.
///
/// Returns whether the peer closed. The one reader shared by [`Upstream`] and
/// [`crate::email::Smtp`].
pub(crate) fn read_tls_into(
    tls: &mut ClientConnection,
    stream: &mut (impl Read + Write),
    buf: &mut [u8],
    len: &mut usize,
) -> io::Result<bool> {
    let mut eof = false;
    loop {
        match tls.read_tls(stream) {
            Ok(0) => {
                eof = true;
                break;
            }
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e)
                if e.kind() == io::ErrorKind::ConnectionReset
                    || e.kind() == io::ErrorKind::BrokenPipe =>
            {
                eof = true;
                break;
            }
            Err(e) => return Err(e),
        }
        if let Err(e) = tls.process_new_packets() {
            let _ = tls.write_tls(stream);
            return Err(io::Error::new(io::ErrorKind::InvalidData, e));
        }
    }
    loop {
        if *len == buf.len() {
            break;
        }
        match tls.reader().read(&mut buf[*len..]) {
            Ok(0) => break,
            Ok(n) => *len += n,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
                eof = true;
                break;
            }
            Err(e) => return Err(e),
        }
    }
    Ok(eof)
}

/// Drive the TLS handshake until it completes or the socket would block.
pub(crate) fn drive_tls_handshake(
    tls: &mut ClientConnection,
    stream: &mut (impl Read + Write),
) -> io::Result<bool> {
    loop {
        let (read, written) = match tls.complete_io(stream) {
            Ok(pair) => pair,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(false),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        if !tls.is_handshaking() {
            return Ok(true);
        }
        // No progress and still handshaking: wait for the next readiness
        // edge rather than spin on `complete_io`.
        if read == 0 && written == 0 {
            return Ok(false);
        }
    }
}

/// Locate the end of the HTTP header section (`\r\n\r\n`), as the offset of the
/// first `\r` in the terminating sequence.
fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

fn find_crlf(buf: &[u8]) -> Option<usize> {
    buf.windows(2).position(|w| w == b"\r\n")
}

fn parse_status_line(line: &str) -> io::Result<(u16, &str, bool)> {
    let mut parts = line.splitn(3, ' ');
    let version = parts.next().unwrap_or("");
    let http10 = version.eq_ignore_ascii_case("HTTP/1.0");
    let status: u16 = parts
        .next()
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "bad or missing status code"))?;
    let reason = parts.next().unwrap_or("");
    Ok((status, reason, http10))
}

/// Whether a response status is informational and to be skipped: a 1xx other
/// than 101, which is final for the exchange.
pub fn is_skipped_informational(status: u16) -> bool {
    (100..200).contains(&status) && status != 101
}

/// The error for a backend that sent too many informational responses.
pub fn too_many_informational() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("backend sent more than {MAX_INFORMATIONAL} informational responses without a final one"),
    )
}

/// A field name is a token (RFC 9110 5.6.2): letters, digits and the token
/// punctuation. Anything else could split one header into two.
fn is_field_name(name: &str) -> bool {
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

/// Parse the header section (status line already consumed) and decide the body
/// framing. This is the one place the framing rules live, so the incremental
/// `Upstream` reader and the blocking URL-backend reader cannot disagree about
/// where a response ends.
pub fn parse_framing(
    head: &[u8],
    status: u16,
    method_is_head: bool,
    http10: bool,
    reusable: &mut bool,
    limit: &crate::limits::Limit,
) -> io::Result<Framing> {
    let mut content_length: Option<usize> = None;
    let mut te_present = false;
    // The final transfer-coding across all Transfer-Encoding fields, in order
    // (RFC 9110 5.3 folds repeated fields into one comma-joined list).
    let mut last_coding: Option<&[u8]> = None;
    let mut connection_close = false;
    let mut connection_keep_alive = false;

    for line in head.split(|&b| b == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.is_empty() {
            break;
        }
        let Some(colon) = line.iter().position(|&b| b == b':') else {
            continue;
        };
        let name = trim_ascii(&line[..colon]);
        let value = trim_ascii(&line[colon + 1..]);
        if name.eq_ignore_ascii_case(b"content-length") {
            for part in value.split(|&b| b == b',') {
                let part = trim_ascii(part);
                if part.is_empty() || !part.iter().all(|b| b.is_ascii_digit()) {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "backend sent an invalid Content-Length: {}",
                            String::from_utf8_lossy(part)
                        ),
                    ));
                }
                let parsed: usize = std::str::from_utf8(part)
                    .map_err(|_| {
                        io::Error::new(io::ErrorKind::InvalidData, "invalid Content-Length")
                    })?
                    .parse()
                    .map_err(|_| {
                        io::Error::new(
                            io::ErrorKind::InvalidData,
                            "backend sent an out-of-range Content-Length",
                        )
                    })?;
                match content_length {
                    Some(prev) if prev != parsed => {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!("backend sent conflicting Content-Length values ({prev} and {parsed}); refusing to guess the framing"),
                        ));
                    }
                    _ => content_length = Some(parsed),
                }
            }
        }
        if name.eq_ignore_ascii_case(b"transfer-encoding") {
            te_present = true;
            for coding in value.split(|&b| b == b',') {
                last_coding = Some(trim_ascii(coding));
            }
        }
        if name.eq_ignore_ascii_case(b"connection") {
            for token in value.split(|&b| b == b',') {
                let token = trim_ascii(token);
                if token.eq_ignore_ascii_case(b"close") {
                    connection_close = true;
                }
                if token.eq_ignore_ascii_case(b"keep-alive") {
                    connection_keep_alive = true;
                }
            }
        }
    }

    let chunked = te_present && last_coding.is_some_and(|c| c.eq_ignore_ascii_case(b"chunked"));

    if te_present && content_length.is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "backend sent both Transfer-Encoding and Content-Length; refusing to guess the framing",
        ));
    }
    if te_present && !chunked {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "backend sent a Transfer-Encoding not ending in chunked; response is unframed",
        ));
    }

    let bodyless = method_is_head || status == 204 || status == 304 || status == 101;
    let framing = if bodyless {
        Framing::Bodyless
    } else if chunked {
        Framing::Chunked
    } else if let Some(len) = content_length {
        // The cap is the bound the buffer is sized for, so refuse above it.
        if len > limit.max() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                limit.exceeded(len, "backend response"),
            ));
        }
        Framing::ContentLength(len)
    } else {
        Framing::CloseDelimited
    };

    // Reusable only when self-delimited and persistent: HTTP/1.1 unless the
    // backend asked to close, HTTP/1.0 only when it asked to keep alive.
    // `Connection: close` always wins.
    let self_delimited = matches!(framing, Framing::ContentLength(_) | Framing::Chunked);
    *reusable = self_delimited && !connection_close && (!http10 || connection_keep_alive);

    Ok(framing)
}

fn trim_ascii(mut b: &[u8]) -> &[u8] {
    while let [first, rest @ ..] = b {
        if first.is_ascii_whitespace() {
            b = rest;
        } else {
            break;
        }
    }
    while let [rest @ .., last] = b {
        if last.is_ascii_whitespace() {
            b = rest;
        } else {
            break;
        }
    }
    b
}

/// Copy a borrowed reply's headers into owned strings, for test assertions.
#[cfg(test)]
fn reply_headers(reply: &Reply<'_>) -> Vec<(String, String)> {
    reply
        .headers
        .iter()
        .map(|h| {
            (
                h.name.to_string(),
                String::from_utf8_lossy(h.value).into_owned(),
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A parsed response: status, owned headers, and body.
    type Parsed = (u16, Vec<(String, String)>, Vec<u8>);

    /// Parse a complete response held in one buffer, as if it had arrived in one
    /// read followed by EOF. Returns (status, headers, body).
    fn parse(raw: &[u8], method_is_head: bool) -> io::Result<Parsed> {
        let mut up = Upstream::new(raw.len().max(8192), 1024);
        up.method_is_head = method_is_head;
        up.read_buf[..raw.len()].copy_from_slice(raw);
        up.read_len = raw.len();
        up.eof = true;
        assert!(up.parse_progress()?, "response must be complete at EOF");
        let mut slots: [std::mem::MaybeUninit<httparse::Header<'_>>; MAX_HEADERS] =
            [const { std::mem::MaybeUninit::uninit() }; MAX_HEADERS];
        let reply = up.reply(&mut slots).expect("reply");
        let headers = reply_headers(&reply);
        Ok((reply.status, headers, reply.body.to_vec()))
    }

    #[test]
    fn a_content_length_body_parses() {
        let (status, _h, body) =
            parse(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello", false).unwrap();
        assert_eq!((status, body.as_slice()), (200, &b"hello"[..]));
    }

    #[test]
    fn a_chunked_body_parses() {
        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n0\r\n\r\n";
        let (status, _h, body) = parse(raw, false).unwrap();
        assert_eq!((status, body.as_slice()), (200, &b"hello"[..]));
    }

    #[test]
    fn parse_status_line_reads_status_and_reason() {
        let (status, reason, http10) = parse_status_line("HTTP/1.1 200 OK").unwrap();
        assert_eq!(status, 200);
        assert_eq!(reason, "OK");
        assert!(!http10);
    }

    #[test]
    fn parse_status_line_accepts_a_missing_reason() {
        let (status, reason, http10) = parse_status_line("HTTP/1.1 404").unwrap();
        assert_eq!(status, 404);
        assert_eq!(reason, "");
        assert!(!http10);
    }

    #[test]
    fn parse_status_line_flags_http_10() {
        let (_, _, http10) = parse_status_line("HTTP/1.0 200 OK").unwrap();
        assert!(http10);
    }

    #[test]
    fn a_full_ndjson_page_fits_under_the_body_cap() {
        const { assert!(crate::ndjson::MAX_PAGE_BYTES < MAX_BODY) };
    }

    // ── Framing rules ────────────────────────────────────────────────────────

    #[test]
    fn conflicting_content_length_is_refused() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nContent-Length: 10\r\n\r\nhello";
        let err = parse(raw, false).unwrap_err();
        assert!(
            err.to_string().contains("conflicting Content-Length"),
            "got: {err}"
        );
    }

    #[test]
    fn duplicate_but_equal_content_length_is_accepted() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nContent-Length: 5\r\n\r\nhello";
        let (_, _, body) = parse(raw, false).unwrap();
        assert_eq!(body, b"hello");
    }

    #[test]
    fn content_length_list_must_agree() {
        assert!(parse(
            b"HTTP/1.1 200 OK\r\nContent-Length: 5, 6\r\n\r\nhello",
            false
        )
        .is_err());
        let (_, _, body) = parse(
            b"HTTP/1.1 200 OK\r\nContent-Length: 5, 5\r\n\r\nhello",
            false,
        )
        .unwrap();
        assert_eq!(body, b"hello");
    }

    #[test]
    fn transfer_encoding_with_content_length_is_refused() {
        let raw =
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nContent-Length: 5\r\n\r\n0\r\n\r\n";
        let err = parse(raw, false).unwrap_err();
        assert!(
            err.to_string()
                .contains("both Transfer-Encoding and Content-Length"),
            "got: {err}"
        );
    }

    #[test]
    fn transfer_encoding_list_ending_in_chunked_is_chunked() {
        let raw =
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: gzip, chunked\r\n\r\n5\r\nhello\r\n0\r\n\r\n";
        let (_, _, body) = parse(raw, false).unwrap();
        assert_eq!(body, b"hello", "chunk envelope was not decoded");
    }

    #[test]
    fn transfer_encoding_not_ending_in_chunked_is_refused() {
        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked, gzip\r\n\r\nhello";
        let err = parse(raw, false).unwrap_err();
        assert!(
            err.to_string().contains("not ending in chunked"),
            "got: {err}"
        );
    }

    #[test]
    fn invalid_content_length_is_refused_not_read_to_eof() {
        for bad in ["abc", "5x", "-1", "0x10", "+5", ""] {
            let raw = format!("HTTP/1.1 200 OK\r\nContent-Length: {bad}\r\n\r\nhello");
            let err = parse(raw.as_bytes(), false).unwrap_err();
            assert!(
                err.to_string().contains("invalid Content-Length"),
                "Content-Length {bad:?} should be refused, got: {err}"
            );
        }
    }

    #[test]
    fn head_response_is_bodyless_despite_content_length() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 16422\r\n\r\n";
        let (_, headers, body) = parse(raw, true).unwrap();
        assert!(body.is_empty());
        assert_eq!(
            headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
                .map(|(_, v)| v.as_str()),
            Some("16422"),
            "the header must survive; only the body is absent"
        );
    }

    #[test]
    fn status_codes_that_never_have_a_body() {
        for status in [204u16, 304, 101] {
            let raw = format!("HTTP/1.1 {status} X\r\nContent-Length: 99\r\n\r\n");
            let (_, _, body) = parse(raw.as_bytes(), false)
                .unwrap_or_else(|e| panic!("{status} must not wait for a body: {e}"));
            assert!(body.is_empty(), "{status} must have no body");
        }
    }

    #[test]
    fn informational_responses_other_than_101_are_skipped() {
        for status in [100u16, 102, 103, 199] {
            let raw = format!(
                "HTTP/1.1 {status} X\r\nContent-Length: 99\r\n\r\nHTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok"
            );
            let (s, _, body) = parse(raw.as_bytes(), false)
                .unwrap_or_else(|e| panic!("{status} must be skipped: {e}"));
            assert_eq!((s, body.as_slice()), (200, &b"ok"[..]), "{status}");
        }
    }

    #[test]
    fn informational_responses_with_no_final_one_are_an_error() {
        let raw = b"HTTP/1.1 103 Early Hints\r\n\r\n";
        assert!(parse(raw, false).is_err());
    }

    #[test]
    fn get_still_reads_its_body() {
        let (_, _, body) =
            parse(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello", false).unwrap();
        assert_eq!(body, b"hello");
    }

    #[test]
    fn a_short_content_length_body_fails_at_once_not_at_the_deadline() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\nConnection: close\r\n\r\nshort";
        let err = parse(raw, false).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof, "{err}");
    }

    // ── Chunked rules ────────────────────────────────────────────────────────

    fn chunked(body: &str) -> Vec<u8> {
        format!("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n{body}").into_bytes()
    }

    #[test]
    fn well_formed_chunked_bodies_decode() {
        for (body, want) in [
            ("5\r\nhello\r\n0\r\n\r\n", "hello"),
            ("5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n", "hello world"),
            ("0\r\n\r\n", ""),
            ("5;foo=bar\r\nhello\r\n0\r\n\r\n", "hello"),
            ("A\r\n0123456789\r\n0\r\n\r\n", "0123456789"),
            ("5\r\nhello\r\n0\r\nX-Checksum: abc\r\n\r\n", "hello"),
        ] {
            let (_, _, out) = parse(&chunked(body), false)
                .unwrap_or_else(|e| panic!("{body:?} should decode: {e}"));
            assert_eq!(String::from_utf8_lossy(&out), want, "for {body:?}");
        }
    }

    #[test]
    fn missing_crlf_after_chunk_data_is_refused() {
        let err = parse(&chunked("5\r\nhelloXX0\r\n\r\n"), false).unwrap_err();
        assert!(
            err.to_string().contains("not terminated by CRLF"),
            "got: {err}"
        );
    }

    #[test]
    fn truncated_trailer_section_is_refused() {
        for body in ["5\r\nhello\r\n0\r\n", "5\r\nhello\r\n0\r\nX-Trailer: v\r\n"] {
            let err = parse(&chunked(body), false).unwrap_err();
            assert!(
                err.to_string().contains("trailer section was terminated"),
                "{body:?} should be refused, got: {err}"
            );
        }
    }

    #[test]
    fn malformed_trailer_is_refused() {
        let err = parse(
            &chunked("5\r\nhello\r\n0\r\nnot a header line\r\n\r\n"),
            false,
        )
        .unwrap_err();
        assert!(err.to_string().contains("malformed trailer"), "got: {err}");
    }

    #[test]
    fn chunk_size_must_be_plain_hex() {
        for bad in ["+A", "-5", "", " ", "0x5", "5g"] {
            let body = format!("{bad}\r\nhello\r\n0\r\n\r\n");
            let err = parse(&chunked(&body), false).unwrap_err();
            assert!(
                err.to_string().contains("invalid chunk size")
                    || err.to_string().contains("missing CRLF"),
                "chunk size {bad:?} should be refused, got: {err}"
            );
        }
    }

    #[test]
    fn short_chunk_data_is_refused() {
        let err = parse(&chunked("10\r\nhello\r\n0\r\n\r\n"), false).unwrap_err();
        assert!(
            err.to_string().contains("shorter than declared"),
            "got: {err}"
        );
    }

    /// A chunked, 1xx-prefixed response written one byte per drive gives the
    /// same reply as written in one piece.
    #[test]
    fn a_response_split_into_one_byte_reads_parses_the_same() {
        let raw = b"HTTP/1.1 103 Early Hints\r\nLink: </a>; rel=preload\r\n\r\nHTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n";
        let whole = parse(raw, false).unwrap();

        let mut up = Upstream::new(raw.len() + 1, 1024);
        up.method_is_head = false;
        for (i, &byte) in raw.iter().enumerate() {
            up.read_buf[i] = byte;
            up.read_len = i + 1;
            if up.parse_progress().unwrap() {
                let mut slots: [std::mem::MaybeUninit<httparse::Header<'_>>; MAX_HEADERS] =
                    [const { std::mem::MaybeUninit::uninit() }; MAX_HEADERS];
                let reply = up.reply(&mut slots).unwrap();
                let headers = reply_headers(&reply);
                let body = reply.body.to_vec();
                assert_eq!((reply.status, headers, body), whole);
                return;
            }
        }
        panic!("the one-byte response never completed");
    }

    /// A request built past the `upstream_request` maximum is refused at
    /// `start`, before a socket is opened, and the error names the key.
    #[test]
    fn a_request_past_its_limit_is_refused_at_start() {
        use crate::limits::{Limit, UPSTREAM_READ, UPSTREAM_REQUEST};
        let read = Limit::builtin(&UPSTREAM_READ);
        let request = Limit::custom(&UPSTREAM_REQUEST, 1024, 4096);
        let mut up = Upstream::with_limits(&read, &request);
        up.request_buf().extend(std::iter::repeat_n(b'x', 4097));
        let err = up
            .start(
                &Addr::Unix(PathBuf::from("/nonexistent")),
                None,
                false,
                crate::platform::now() + std::time::Duration::from_secs(5),
            )
            .unwrap_err();
        assert!(
            err.to_string().contains(
                "backend request needs at least 4097 B, over [limits.upstream_request] max = 4 KiB"
            ),
            "{err}"
        );
    }

    /// A declared Content-Length above the cap is refused at the head, before
    /// any body is read.
    #[test]
    fn a_content_length_above_the_cap_is_refused_at_the_head() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 4000000000\r\n\r\n";
        let err = parse(raw, false).unwrap_err();
        assert!(
            err.to_string()
                .contains("over [limits.upstream_read] max = 128 MiB"),
            "got: {err}"
        );
    }

    /// A declared Content-Length that cannot fit is refused, not delivered.
    #[test]
    fn a_declared_content_length_above_the_limit_is_refused() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 18446744073709551615\r\n\r\n";
        let err = parse(raw, false).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData, "got: {err}");
    }

    /// Repeated Transfer-Encoding fields fold into one list (RFC 9110 5.3), so
    /// the final coding decides the framing.
    #[test]
    fn a_transfer_encoding_split_across_header_lines_is_combined() {
        let chunked_then_gzip = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nTransfer-Encoding: gzip\r\n\r\nhello";
        let err = parse(chunked_then_gzip, false).unwrap_err();
        assert!(
            err.to_string().contains("not ending in chunked"),
            "got: {err}"
        );

        let gzip_then_chunked =
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: gzip\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n0\r\n\r\n";
        let (_, _, body) = parse(gzip_then_chunked, false).unwrap();
        assert_eq!(body, b"hello");
    }

    /// An HTTP/1.0 reply is close-by-default, so it must not be reusable unless
    /// the backend asked to keep the connection alive.
    #[test]
    fn an_http_10_content_length_reply_is_not_reusable() {
        let raw = b"HTTP/1.0 200 OK\r\nContent-Length: 5\r\n\r\nhello";
        let mut up = Upstream::new(8192, 1024);
        up.method_is_head = false;
        up.request_finished = true;
        up.read_buf[..raw.len()].copy_from_slice(raw);
        up.read_len = raw.len();
        up.eof = true;
        assert!(up.parse_progress().unwrap());
        assert!(!up.reusable(), "HTTP/1.0 is close-by-default");

        let keep_alive =
            b"HTTP/1.0 200 OK\r\nContent-Length: 5\r\nConnection: keep-alive\r\n\r\nhello";
        let mut up2 = Upstream::new(8192, 1024);
        up2.method_is_head = false;
        up2.request_finished = true;
        up2.read_buf[..keep_alive.len()].copy_from_slice(keep_alive);
        up2.read_len = keep_alive.len();
        up2.eof = true;
        assert!(up2.parse_progress().unwrap());
        assert!(up2.reusable(), "HTTP/1.0 keep-alive is reusable");

        let close_wins =
            b"HTTP/1.0 200 OK\r\nContent-Length: 5\r\nConnection: keep-alive, close\r\n\r\nhello";
        let mut up3 = Upstream::new(8192, 1024);
        up3.method_is_head = false;
        up3.request_finished = true;
        up3.read_buf[..close_wins.len()].copy_from_slice(close_wins);
        up3.read_len = close_wins.len();
        up3.eof = true;
        assert!(up3.parse_progress().unwrap());
        assert!(!up3.reusable(), "Connection: close wins over keep-alive");
    }
}

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "test code: threads and blocking clients drive the service from outside"
)]
mod socket_tests {
    use super::*;
    use std::io::{Read, Write};
    use std::time::Duration;

    /// A parsed response: status, owned headers, and body.
    type Parsed = (u16, Vec<(String, String)>, Vec<u8>);

    /// Drive an upstream with a `platform::Poll` until it completes, blocking
    /// (tests may block). Returns (status, headers, body).
    fn drive_to_completion(up: &mut Upstream) -> io::Result<Parsed> {
        use crate::platform::{Events, Poll, Token};
        let mut poll = Poll::new()?;
        let mut events = Events::with_capacity(8);
        let token = Token(0);
        let mut registered = up.interest();
        poll.registry().register_fd(up.fd(), token, registered)?;
        loop {
            poll.poll(&mut events, Some(Duration::from_millis(100)))?;
            if events.iter().any(|ev| ev.token() == token) {
                if up.drive()? {
                    let mut slots: [std::mem::MaybeUninit<httparse::Header<'_>>; MAX_HEADERS] =
                        [const { std::mem::MaybeUninit::uninit() }; MAX_HEADERS];
                    let reply = up.reply(&mut slots).expect("reply after complete drive");
                    let headers = reply_headers(&reply);
                    return Ok((reply.status, headers, reply.body.to_vec()));
                }
                // `drive` moves the phase on, so the interest it wants now can
                // differ from the one registered before the call. The
                // reregister belongs AFTER the drive: under epoll
                // edge-triggering, a change made before it waits for an edge
                // that may never arrive.
                let interest = up.interest();
                if interest != registered {
                    poll.registry().reregister_fd(up.fd(), token, interest)?;
                    registered = interest;
                }
            }
            if crate::platform::now() >= up.deadline() {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "backend timed out"));
            }
        }
    }

    /// A loopback TCP listener answers one request. `start` returns before the
    /// listener accepts (the connect is in progress), and `drive` after
    /// writable sends the request and reads the reply.
    #[test]
    fn a_tcp_connect_completes_on_writable() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let peer = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().expect("accept");
            let mut got = Vec::new();
            let mut buf = [0u8; 4096];
            while !got.windows(4).any(|w| w == b"\r\n\r\n") {
                match s.read(&mut buf) {
                    Ok(0) | Err(_) => return,
                    Ok(n) => got.extend_from_slice(&buf[..n]),
                }
            }
            let _ =
                s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok");
        });

        let mut up = Upstream::new(64 * 1024, 16 * 1024);
        up.request_buf()
            .extend_from_slice(b"GET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n");
        // The connect is non-blocking: it returns at once, EINPROGRESS included.
        up.start(
            &Addr::Tcp(addr),
            None,
            false,
            crate::platform::now() + Duration::from_secs(5),
        )
        .expect("an in-progress connect is not an error");
        let (status, _, body) = drive_to_completion(&mut up).expect("drive");
        assert_eq!((status, body.as_slice()), (200, &b"ok"[..]));
        peer.join().expect("peer");
    }

    /// A connect that has not completed is not driven as if it had.
    ///
    /// `take_error` reports no error while a non-blocking connect is in
    /// progress, and a write to such a socket fails with ENOTCONN on macOS, so
    /// before the peer-address check the first drive failed the request. RFC
    /// 5737's TEST-NET-1 gives no answer, so the connect stays in progress.
    #[test]
    fn a_connect_still_in_progress_is_not_treated_as_open() {
        let mut up = Upstream::new(64 * 1024, 16 * 1024);
        up.request_buf()
            .extend_from_slice(b"GET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n");
        let addr: std::net::SocketAddr = "192.0.2.1:9".parse().expect("an RFC 5737 address");
        up.start(
            &Addr::Tcp(addr),
            None,
            false,
            crate::platform::now() + Duration::from_secs(30),
        )
        .expect("a non-blocking connect is not an error");
        assert!(
            matches!(up.drive(), Ok(false)),
            "a connect in progress must report not-ready, not the ENOTCONN a write gives"
        );
    }

    /// A `Content-Length` response without `Connection: close` is reusable, and
    /// a second `start` to the same address rides the same socket (the server
    /// accepts once).
    #[test]
    fn a_keep_alive_response_is_reusable() {
        let dir = tempfile::tempdir_in(crate::platform::socket_scratch_root()).expect("tempdir");
        let path = dir.path().join("keep.sock");
        let listener = std::os::unix::net::UnixListener::bind(&path).expect("bind");
        let path2 = path.clone();
        let peer = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().expect("accept");
            for _ in 0..2 {
                let mut got = Vec::new();
                let mut buf = [0u8; 4096];
                while !got.windows(4).any(|w| w == b"\r\n\r\n") {
                    match s.read(&mut buf) {
                        Ok(0) | Err(_) => return,
                        Ok(n) => got.extend_from_slice(&buf[..n]),
                    }
                }
                let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok");
            }
        });

        let addr = Addr::Unix(path2);
        let mut up = Upstream::new(64 * 1024, 16 * 1024);
        up.request_buf()
            .extend_from_slice(b"GET /one HTTP/1.1\r\nHost: x\r\n\r\n");
        up.start(
            &addr,
            None,
            false,
            crate::platform::now() + Duration::from_secs(5),
        )
        .expect("start");
        let (status, _, body) = drive_to_completion(&mut up).expect("drive");
        assert_eq!((status, body.as_slice()), (200, &b"ok"[..]));
        assert!(
            up.reusable(),
            "a Content-Length reply without Connection: close is reusable"
        );

        up.request_buf()
            .extend_from_slice(b"GET /two HTTP/1.1\r\nHost: x\r\n\r\n");
        up.start(
            &addr,
            None,
            false,
            crate::platform::now() + Duration::from_secs(5),
        )
        .expect("second start reuses the socket");
        let (status, _, body) = drive_to_completion(&mut up).expect("drive");
        assert_eq!((status, body.as_slice()), (200, &b"ok"[..]));
        peer.join().expect("peer");
    }

    /// A TLS round trip over loopback: a rustls server in a test thread, the
    /// client gets 200 and the body.
    #[test]
    fn tls_round_trip_over_loopback() {
        let ck = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).expect("cert");
        let cert = ck.cert.der().clone();
        let key =
            rustls::pki_types::PrivateKeyDer::try_from(ck.key_pair.serialize_der()).expect("key");
        let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
        let server = Arc::new(
            rustls::ServerConfig::builder_with_provider(provider.clone())
                .with_safe_default_protocol_versions()
                .expect("protocol versions")
                .with_no_client_auth()
                .with_single_cert(vec![cert.clone()], key)
                .expect("server config"),
        );

        let mut roots = rustls::RootCertStore::empty();
        roots.add(cert).expect("trust the run's own cert");
        let client = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .expect("protocol versions")
            .with_root_certificates(roots)
            .with_no_client_auth();

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let peer = std::thread::spawn(move || {
            let (tcp, _) = listener.accept().expect("accept");
            let conn = rustls::ServerConnection::new(server).expect("server conn");
            let mut stream = rustls::StreamOwned::new(conn, tcp);
            let mut got = Vec::new();
            let mut buf = [0u8; 4096];
            while !got.windows(4).any(|w| w == b"\r\n\r\n") {
                match stream.read(&mut buf) {
                    Ok(0) | Err(_) => return,
                    Ok(n) => got.extend_from_slice(&buf[..n]),
                }
            }
            let _ = stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok");
            let _ = stream.flush();
        });

        let name = rustls::pki_types::ServerName::try_from("localhost".to_string()).expect("name");
        let tls = TlsClient {
            config: Arc::new(client),
            name,
        };
        let mut up = Upstream::new(64 * 1024, 16 * 1024);
        up.request_buf()
            .extend_from_slice(b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
        up.start(
            &Addr::Tcp(addr),
            Some(&tls),
            false,
            crate::platform::now() + Duration::from_secs(5),
        )
        .expect("start");
        let (status, _, body) = drive_to_completion(&mut up).expect("drive");
        assert_eq!((status, body.as_slice()), (200, &b"ok"[..]));
        peer.join().expect("peer");
    }

    /// A backend that answers at once without reading the request is still read
    /// while the request write would block (RFC 9110 6.1), and the next `start`
    /// reconnects instead of riding the truncated stream.
    #[test]
    fn a_backend_that_answers_without_reading_is_read_while_writing() {
        let dir = tempfile::tempdir_in(crate::platform::socket_scratch_root()).expect("tempdir");
        let path = dir.path().join("early.sock");
        let listener = std::os::unix::net::UnixListener::bind(&path).expect("bind");
        let (accept_tx, accept_rx) = std::sync::mpsc::channel();
        let peer = std::thread::spawn(move || {
            let (mut s1, _) = listener.accept().expect("accept");
            accept_tx.send(()).expect("report the first accept");
            // Never read the request; write a keep-alive 413 at once and hold
            // the socket open until the client is done.
            let _ = s1.write_all(b"HTTP/1.1 413 Payload Too Large\r\nContent-Length: 0\r\n\r\n");
            std::thread::park();
            // The client's second start reconnects here.
            let Ok((_s2, _)) = listener.accept() else {
                return;
            };
            accept_tx.send(()).expect("report the second accept");
        });

        let deadline = crate::platform::now() + Duration::from_secs(5);
        let mut up = Upstream::new(64 * 1024, 64 * 1024);
        let req = up.request_buf();
        req.extend_from_slice(b"POST / HTTP/1.1\r\nHost: x\r\nContent-Length: 1048576\r\n\r\n");
        req.extend(std::iter::repeat_n(b'x', 1_048_576));
        up.start(&Addr::Unix(path.clone()), None, false, deadline)
            .expect("start");
        let (status, _, body) = drive_to_completion(&mut up).expect("drive");
        assert_eq!((status, body.as_slice()), (413, &b""[..]));
        assert!(
            !up.reusable(),
            "an early reply before the request is written must not reuse"
        );
        assert!(
            accept_rx.recv_timeout(Duration::from_secs(5)).is_ok(),
            "the first start must connect"
        );

        // A second start must not ride the truncated stream: it reconnects.
        up.request_buf()
            .extend_from_slice(b"GET / HTTP/1.1\r\nHost: x\r\n\r\n");
        up.start(&Addr::Unix(path), None, false, deadline)
            .expect("start");
        peer.thread().unpark();
        assert!(
            accept_rx.recv_timeout(Duration::from_secs(5)).is_ok(),
            "the second start must reconnect, not reuse"
        );
        peer.join().expect("peer");
    }

    /// A TLS backend that answers before reading the whole request closes the
    /// write side. The complete reply it wrote is still read and returned, and
    /// the connection is never reused.
    ///
    /// The peer closes the way RFC 9112 section 9.6 asks a server that answers
    /// early to close: it notifies, shuts its write half, waits for the client
    /// to stop sending, then drains the rest of the request with a read timeout
    /// before the socket is dropped. A peer that drops the socket with the
    /// request unread makes the close a reset on Linux, and a reset can discard
    /// the reply the client has not read yet; that pins the reset path, not the
    /// early-response path.
    ///
    /// Break: write the whole request before reading (remove the early-response
    /// switch) and the client reaches the deadline instead of returning 413.
    /// Break: reuse an early answer (`reusable` without `request_finished`) and
    /// the second assertion fails. Break: read only after the write is complete
    /// and the request is written in full, so `!request_written` fails. The
    /// request is larger than any platform's socket buffer, so that stays true
    /// without relying on a timing window.
    #[test]
    fn a_tls_backend_that_answers_early_is_read_while_writing() {
        let ck = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).expect("cert");
        let cert = ck.cert.der().clone();
        let key =
            rustls::pki_types::PrivateKeyDer::try_from(ck.key_pair.serialize_der()).expect("key");
        let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
        let server = Arc::new(
            rustls::ServerConfig::builder_with_provider(provider.clone())
                .with_safe_default_protocol_versions()
                .expect("versions")
                .with_no_client_auth()
                .with_single_cert(vec![cert.clone()], key)
                .expect("server config"),
        );
        let mut roots = rustls::RootCertStore::empty();
        roots.add(cert).expect("trust the run's own cert");
        let client = Arc::new(
            rustls::ClientConfig::builder_with_provider(provider)
                .with_safe_default_protocol_versions()
                .expect("versions")
                .with_root_certificates(roots)
                .with_no_client_auth(),
        );

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        // The peer does not close until the client has the reply, so the close
        // cannot reset the connection the reply is still travelling on.
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let peer = std::thread::spawn(move || {
            let (tcp, _) = listener.accept().expect("accept");
            let conn = rustls::ServerConnection::new(server).expect("server conn");
            let mut stream = rustls::StreamOwned::new(conn, tcp);
            // Read the head and about 1 KiB of the body, then answer with most
            // of the request still unwritten.
            let mut got = 0usize;
            let mut buf = [0u8; 4096];
            while got < 1024 {
                match stream.read(&mut buf) {
                    Ok(0) | Err(_) => return,
                    Ok(n) => got += n,
                }
            }
            // Keep-alive framing on purpose: only the unfinished request may
            // stop reuse, so removing that rule fails the reuse assertion.
            let _ = stream
                .write_all(b"HTTP/1.1 413 Payload Too Large\r\nContent-Length: 7\r\n\r\nrefused");
            let _ = stream.flush();
            // RFC 9112 section 9.6: notify, shut the write half, and only then
            // discard the remainder before closing. Dropping the socket while
            // the kernel still has request bytes unread is a reset on Linux.
            stream.conn.send_close_notify();
            let _ = stream.flush();
            let _ = stream.get_ref().shutdown(std::net::Shutdown::Write);
            // The client has stopped writing once it has the reply; the read
            // timeout means a failed test cannot hang this thread.
            let _ = release_rx.recv_timeout(Duration::from_secs(5));
            let _ = stream
                .get_ref()
                .set_read_timeout(Some(Duration::from_millis(500)));
            let mut buf = [0u8; 8192];
            while let Ok(n) = stream.read(&mut buf) {
                if n == 0 {
                    break;
                }
            }
        });

        let name = rustls::pki_types::ServerName::try_from("localhost".to_string()).expect("name");
        let tls = TlsClient {
            config: client,
            name,
        };
        let deadline = crate::platform::now() + Duration::from_secs(10);
        let mut up = Upstream::new(64 * 1024, 64 * 1024);
        // 16 MiB: larger than any TcpStream send buffer, so a client that
        // writes the request out before reading cannot pass this test.
        const BODY_LEN: usize = 16 * 1024 * 1024;
        let req = up.request_buf();
        req.extend_from_slice(
            format!(
                "POST / HTTP/1.1\r\nHost: localhost\r\nContent-Length: {BODY_LEN}\r\n\
                 Connection: close\r\n\r\n"
            )
            .as_bytes(),
        );
        req.extend(std::iter::repeat_n(b'x', BODY_LEN));
        up.start(&Addr::Tcp(addr), Some(&tls), false, deadline)
            .expect("start");
        let (status, _, body) = drive_to_completion(&mut up)
            .expect("the early reply is forwarded, not the write error");
        assert_eq!(
            (status, body.as_slice()),
            (413, &b"refused"[..]),
            "the backend's 413 and body, not a 502 or a timeout"
        );
        assert!(!up.reusable(), "an early reply must not be reused");
        assert!(
            !up.request_written(),
            "the whole request never reached the backend"
        );
        up.close();
        release_tx.send(()).expect("release the peer");
        peer.join().expect("peer");
    }

    /// A complete reply already waiting outranks a request write that fails
    /// with an error which is not one of the two reset kinds.
    ///
    /// Break: return the write error before looking at buffered data (the old
    /// `write_request()?` in `transfer`) fails here with that error, not 413.
    #[test]
    fn a_complete_reply_outranks_a_write_error() {
        let (client, mut peer) = std::os::unix::net::UnixStream::pair().expect("socket pair");
        client
            .set_nonblocking(true)
            .expect("the client side is non-blocking, as `connect` leaves it");
        peer.write_all(b"HTTP/1.1 413 Payload Too Large\r\nContent-Length: 7\r\n\r\nrefused")
            .expect("peer writes the reply");
        let mut up = Upstream::new(64 * 1024, 64 * 1024);
        up.stream = Some(Stream::Unix(client));
        up.phase = Phase::Transfer;
        up.request
            .extend_from_slice(b"POST / HTTP/1.1\r\nHost: x\r\nContent-Length: 1048576\r\n\r\n");
        up.request.extend(std::iter::repeat_n(b'x', 1_048_576));
        up.fail_next_write.set(true);
        assert!(up.drive().expect("the reply, not the write error"));
        let mut slots = reply_slots();
        let reply = up.reply(&mut slots).expect("reply");
        assert_eq!((reply.status, reply.body), (413, &b"refused"[..]));
        assert!(!up.reusable(), "the request was not written, so no reuse");
        assert!(!up.request_written());
    }

    /// A different TLS identity (name) for the same address reconnects instead
    /// of reusing the connection authenticated for the first identity.
    #[test]
    fn a_different_tls_identity_reconnects_instead_of_reusing() {
        let ck = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).expect("cert");
        let cert = ck.cert.der().clone();
        let key =
            rustls::pki_types::PrivateKeyDer::try_from(ck.key_pair.serialize_der()).expect("key");
        let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
        let server = Arc::new(
            rustls::ServerConfig::builder_with_provider(provider.clone())
                .with_safe_default_protocol_versions()
                .expect("versions")
                .with_no_client_auth()
                .with_single_cert(vec![cert.clone()], key)
                .expect("server config"),
        );
        let mut roots = rustls::RootCertStore::empty();
        roots.add(cert).expect("trust the run's own cert");
        let config_a = Arc::new(
            rustls::ClientConfig::builder_with_provider(provider)
                .with_safe_default_protocol_versions()
                .expect("versions")
                .with_root_certificates(roots)
                .with_no_client_auth(),
        );

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let server2 = server.clone();
        let (accept_tx, accept_rx) = std::sync::mpsc::channel();
        let peer = std::thread::spawn(move || {
            for _ in 0..2 {
                let Ok((tcp, _)) = listener.accept() else {
                    break;
                };
                accept_tx.send(()).expect("report the accept");
                let conn = rustls::ServerConnection::new(server2.clone()).expect("server conn");
                let mut stream = rustls::StreamOwned::new(conn, tcp);
                loop {
                    let mut got = Vec::new();
                    let mut buf = [0u8; 4096];
                    let mut complete = false;
                    while !complete {
                        match stream.read(&mut buf) {
                            Ok(0) | Err(_) => break,
                            Ok(n) => {
                                got.extend_from_slice(&buf[..n]);
                                complete = got.windows(4).any(|w| w == b"\r\n\r\n");
                            }
                        }
                    }
                    if !complete {
                        break;
                    }
                    let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok");
                    let _ = stream.flush();
                }
            }
        });

        let name_a =
            rustls::pki_types::ServerName::try_from("localhost".to_string()).expect("name");
        let name_b =
            rustls::pki_types::ServerName::try_from("example.com".to_string()).expect("name");
        let tls_a = TlsClient {
            config: config_a.clone(),
            name: name_a,
        };
        let tls_b = TlsClient {
            config: config_a,
            name: name_b,
        };

        let deadline = crate::platform::now() + Duration::from_secs(10);
        let mut up = Upstream::new(64 * 1024, 16 * 1024);
        // Request 1 with identity A: the server accepts once.
        up.request_buf()
            .extend_from_slice(b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n");
        up.start(&Addr::Tcp(addr), Some(&tls_a), false, deadline)
            .expect("start");
        assert_eq!(drive_to_completion(&mut up).expect("drive").0, 200);
        assert!(up.reusable());
        assert!(
            accept_rx.recv_timeout(Duration::from_secs(5)).is_ok(),
            "request 1 must connect"
        );

        // Request 2 with identity A reuses the open connection, so the server
        // accepts nothing new.
        up.request_buf()
            .extend_from_slice(b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n");
        up.start(&Addr::Tcp(addr), Some(&tls_a), false, deadline)
            .expect("start");
        assert_eq!(drive_to_completion(&mut up).expect("drive").0, 200);
        assert!(
            accept_rx.recv_timeout(Duration::from_millis(500)).is_err(),
            "the same identity must reuse the connection"
        );

        // Request 3 with identity B reconnects: the server accepts a second
        // time, and the handshake fails because the certificate is for a
        // different name.
        up.request_buf()
            .extend_from_slice(b"GET / HTTP/1.1\r\nHost: example.com\r\n\r\n");
        up.start(&Addr::Tcp(addr), Some(&tls_b), false, deadline)
            .expect("start");
        assert!(drive_to_completion(&mut up).is_err());
        assert!(
            accept_rx.recv_timeout(Duration::from_secs(5)).is_ok(),
            "a different identity must reconnect"
        );
        // Close the failed handshake so the server's read returns instead of
        // blocking on a close the client never sends.
        up.close();

        peer.join().expect("peer");
    }

    /// A TLS request with a body completes: the client encrypts and flushes the
    /// request and the server answers.
    #[test]
    fn a_tls_request_with_a_body_completes() {
        let ck = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).expect("cert");
        let cert = ck.cert.der().clone();
        let key =
            rustls::pki_types::PrivateKeyDer::try_from(ck.key_pair.serialize_der()).expect("key");
        let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
        let server = Arc::new(
            rustls::ServerConfig::builder_with_provider(provider.clone())
                .with_safe_default_protocol_versions()
                .expect("versions")
                .with_no_client_auth()
                .with_single_cert(vec![cert.clone()], key)
                .expect("server config"),
        );
        let mut roots = rustls::RootCertStore::empty();
        roots.add(cert).expect("trust the run's own cert");
        let client = Arc::new(
            rustls::ClientConfig::builder_with_provider(provider)
                .with_safe_default_protocol_versions()
                .expect("versions")
                .with_root_certificates(roots)
                .with_no_client_auth(),
        );

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let peer = std::thread::spawn(move || {
            let (tcp, _) = listener.accept().expect("accept");
            let conn = rustls::ServerConnection::new(server).expect("server conn");
            let mut stream = rustls::StreamOwned::new(conn, tcp);
            let mut got = Vec::new();
            let mut buf = [0u8; 4096];
            while !got.windows(4).any(|w| w == b"\r\n\r\n") {
                match stream.read(&mut buf) {
                    Ok(0) | Err(_) => return,
                    Ok(n) => got.extend_from_slice(&buf[..n]),
                }
            }
            let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok");
            let _ = stream.flush();
        });

        let name = rustls::pki_types::ServerName::try_from("localhost".to_string()).expect("name");
        let tls = TlsClient {
            config: client,
            name,
        };
        let deadline = crate::platform::now() + Duration::from_secs(10);
        let mut up = Upstream::new(64 * 1024, 64 * 1024);
        up.request_buf().extend_from_slice(
            b"POST / HTTP/1.1\r\nHost: localhost\r\nContent-Length: 4\r\n\r\nbody",
        );
        up.start(&Addr::Tcp(addr), Some(&tls), false, deadline)
            .expect("start");
        let (status, _, body) = drive_to_completion(&mut up).expect("drive");
        assert_eq!((status, body.as_slice()), (200, &b"ok"[..]));
        peer.join().expect("peer");
    }

    /// The final flush's WouldBlock is retried: the seam blocks the flush once,
    /// the write reports the block, and the next drive flushes the tail.
    #[test]
    fn a_blocked_final_flush_is_retried() {
        use crate::platform::{Events, Poll, Token};
        let ck = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).expect("cert");
        let cert = ck.cert.der().clone();
        let key =
            rustls::pki_types::PrivateKeyDer::try_from(ck.key_pair.serialize_der()).expect("key");
        let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
        let server = Arc::new(
            rustls::ServerConfig::builder_with_provider(provider.clone())
                .with_safe_default_protocol_versions()
                .expect("versions")
                .with_no_client_auth()
                .with_single_cert(vec![cert.clone()], key)
                .expect("server config"),
        );
        let mut roots = rustls::RootCertStore::empty();
        roots.add(cert).expect("trust the run's own cert");
        let client = Arc::new(
            rustls::ClientConfig::builder_with_provider(provider)
                .with_safe_default_protocol_versions()
                .expect("versions")
                .with_root_certificates(roots)
                .with_no_client_auth(),
        );

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        // Just over rustls's 64 KiB send-buffer limit, so the write fills the
        // buffer before the handshake and the seam fires on the post-handshake
        // final flush, not the buffered pre-handshake write.
        let body_len = 65 * 1024usize;
        let peer = std::thread::spawn(move || {
            let (tcp, _) = listener.accept().expect("accept");
            let conn = rustls::ServerConnection::new(server).expect("server conn");
            let mut stream = rustls::StreamOwned::new(conn, tcp);
            let mut got = Vec::new();
            let mut buf = [0u8; 4096];
            while !got.windows(4).any(|w| w == b"\r\n\r\n") {
                match stream.read(&mut buf) {
                    Ok(0) | Err(_) => return,
                    Ok(n) => got.extend_from_slice(&buf[..n]),
                }
            }
            let head_end = got.windows(4).position(|w| w == b"\r\n\r\n").expect("head") + 4;
            let mut total = got.len();
            while total < head_end + body_len {
                match stream.read(&mut buf) {
                    Ok(0) | Err(_) => return,
                    Ok(n) => total += n,
                }
            }
            assert_eq!(total - head_end, body_len, "the retry must send the body");
            let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok");
            let _ = stream.flush();
        });

        let name = rustls::pki_types::ServerName::try_from("localhost".to_string()).expect("name");
        let tls = TlsClient {
            config: client,
            name,
        };
        let deadline = crate::platform::now() + Duration::from_secs(10);
        let mut up = Upstream::new(64 * 1024, 64 * 1024);
        let mut req = Vec::new();
        req.extend_from_slice(
            b"POST / HTTP/1.1\r\nHost: localhost\r\nContent-Length: 66560\r\n\r\n",
        );
        req.extend(std::iter::repeat_n(b'x', body_len));
        up.request_buf().extend_from_slice(&req);
        up.start(&Addr::Tcp(addr), Some(&tls), false, deadline)
            .expect("start");

        // Force the next final flush to report a WouldBlock, then drive.
        up.block_next_write.set(true);

        let mut poll = Poll::new().expect("poll");
        let mut events = Events::with_capacity(8);
        let token = Token(0);
        let mut interest = up.interest();
        poll.registry()
            .register_fd(up.fd(), token, interest)
            .expect("register");
        let mut blocked = false;
        let mut reregistered_after_block = false;
        loop {
            poll.poll(&mut events, Some(Duration::from_millis(100)))
                .expect("poll");
            if events.iter().any(|e| e.token() == token) {
                if up.drive().expect("drive") {
                    break;
                }
                // The seam leaves the request written but the ciphertext pending.
                let blocked_now = !up.request_finished
                    && up.write_offset >= up.request.len()
                    && up.tls.as_ref().is_some_and(|t| t.wants_write());
                blocked |= blocked_now;
                // Reregister after the drive, never before: `drive` moves the
                // phase on, and under epoll edge-triggering a mask written
                // before it waits for an edge that may never arrive. A changed
                // interest needs the call anyway. The seam's WouldBlock needs
                // it even when the interest is unchanged: the kernel never
                // returned EAGAIN, so epoll has no readiness change to deliver,
                // and EPOLL_CTL_MOD is what re-reports current readiness. Once
                // is enough; reregistering every pass would spin.
                let next = up.interest();
                let force = blocked_now && !reregistered_after_block;
                if next != interest || force {
                    poll.registry()
                        .reregister_fd(up.fd(), token, next)
                        .expect("reregister");
                    interest = next;
                }
                if force {
                    reregistered_after_block = true;
                }
            }
            if crate::platform::now() >= up.deadline() {
                panic!("timed out");
            }
        }
        assert!(blocked, "the final flush must have blocked once");
        let mut slots: [std::mem::MaybeUninit<httparse::Header<'_>>; MAX_HEADERS] =
            [const { std::mem::MaybeUninit::uninit() }; MAX_HEADERS];
        let reply = up.reply(&mut slots).expect("reply");
        assert_eq!((reply.status, reply.body), (200, &b"ok"[..]));
        peer.join().expect("peer");
    }

    // ── The host's trust store ────────────────────────────────────────────

    /// An empty store is an error, never an empty trust set: a client with no
    /// roots fails every handshake and blames the peer.
    #[test]
    fn a_trust_store_with_no_certificate_is_an_error() {
        let e = roots_from(Vec::new()).expect_err("no certificate must be an error");
        assert_eq!(e.kind(), io::ErrorKind::NotFound);
        assert!(
            e.to_string().contains("certificate store"),
            "the error must name the store: {e}"
        );
    }

    /// A store whose certificates all fail to parse is an error too, never a
    /// store that quietly trusts nothing and never a permissive verifier.
    #[test]
    fn a_trust_store_whose_certificates_do_not_parse_is_an_error() {
        let junk = rustls::pki_types::CertificateDer::from(vec![0u8; 4]);
        let e = roots_from([junk]).expect_err("an unparsable certificate must be an error");
        assert_eq!(e.kind(), io::ErrorKind::InvalidData);
        assert!(
            e.to_string().contains("none could be parsed"),
            "the error must say nothing parsed: {e}"
        );
    }

    /// A parses, so the success path is the third case and the two above are
    /// the only failures.
    #[test]
    fn a_parsable_certificate_becomes_a_root() {
        let ck = rcgen::generate_simple_self_signed(vec!["example.com".to_string()]).expect("cert");
        let root = ck.cert.der().clone();
        let roots = roots_from([root]).expect("one parsable certificate");
        assert_eq!(roots.len(), 1);
    }
}

#[cfg(all(test, feature = "http-client"))]
#[allow(
    clippy::disallowed_methods,
    reason = "test code: the loopback HTTP stub is a blocking thread"
)]
mod endpoint_tests {
    use std::io::{Read, Write};
    use std::sync::Arc;
    use std::time::Duration;

    use super::*;

    fn empty_config() -> Arc<rustls::ClientConfig> {
        Arc::new(
            rustls::ClientConfig::builder()
                .with_root_certificates(rustls::RootCertStore::empty())
                .with_no_client_auth(),
        )
    }

    /// Read one request (head and its `Content-Length` body) and answer 200.
    fn stub(listener: std::net::TcpListener) -> std::thread::JoinHandle<Vec<u8>> {
        std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().expect("accept");
            let mut got = Vec::new();
            let mut buf = [0u8; 4096];
            let head_end = loop {
                if let Some(pos) = got.windows(4).position(|w| w == b"\r\n\r\n") {
                    break pos + 4;
                }
                let n = sock.read(&mut buf).expect("read head");
                assert!(n > 0, "the request head must arrive");
                got.extend_from_slice(&buf[..n]);
            };
            let len: usize = std::str::from_utf8(&got[..head_end])
                .expect("utf8 head")
                .lines()
                .find_map(|l| l.strip_prefix("Content-Length: "))
                .expect("a Content-Length")
                .parse()
                .expect("a length");
            while got.len() < head_end + len {
                let n = sock.read(&mut buf).expect("read body");
                assert!(n > 0, "the body must arrive");
                got.extend_from_slice(&buf[..n]);
            }
            sock.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
                .expect("answer");
            got
        })
    }

    /// Drive one request to its reply with a poll, then assert the reply.
    fn drive(up: &mut Upstream, deadline: std::time::Instant) {
        let mut poll = crate::platform::Poll::new().expect("poll");
        let mut events = crate::platform::Events::with_capacity(8);
        let token = crate::platform::Token(0);
        poll.registry()
            .register_fd(up.fd(), token, up.interest())
            .expect("register");
        let mut registered = up.interest();
        loop {
            if up.drive().expect("drive") {
                // The reply borrows the slot; the caller reads it before the
                // next start. Return through a raw pointer-free path: parse in
                // place and stop.
                break;
            }
            if up.interest() != registered {
                registered = up.interest();
                poll.registry()
                    .reregister_fd(up.fd(), token, registered)
                    .expect("reregister");
            }
            poll.poll(&mut events, Some(Duration::from_millis(100)))
                .expect("poll");
            if crate::platform::now() >= deadline {
                panic!("the stub did not answer before the deadline");
            }
        }
        let mut slots = reply_slots();
        let reply = up.reply(&mut slots).expect("reply");
        assert_eq!(reply.status, 200);
        assert_eq!(reply.body, b"ok");
    }

    #[test]
    fn post_form_sends_an_encoded_body() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let peer = stub(listener);
        let url = format!("http://127.0.0.1:{port}/v0/verify?x=1");
        let endpoint = super::Endpoint::new(&url, empty_config()).expect("endpoint");
        let mut up = Upstream::new(16 * 1024, 16 * 1024);
        let deadline = crate::platform::now() + Duration::from_secs(10);
        endpoint
            .post_form(&mut up, &[("secret", "a b&c"), ("response", "t")], deadline)
            .expect("post_form");
        drive(&mut up, deadline);
        let got = peer.join().expect("stub");
        let text = String::from_utf8(got.clone()).expect("utf8 request");
        let head = text.split("\r\n\r\n").next().expect("head");
        assert!(
            head.starts_with("POST /v0/verify?x=1 HTTP/1.1\r\n"),
            "{head}"
        );
        assert!(
            head.contains(&format!("\r\nHost: 127.0.0.1:{port}\r\n")),
            "{head}"
        );
        assert!(
            head.contains("\r\nContent-Type: application/x-www-form-urlencoded\r\n"),
            "{head}"
        );
        assert!(head.contains("\r\nContent-Length: 25\r\n"), "{head}");
        assert_eq!(&got[got.len() - 25..], b"secret=a+b%26c&response=t");
        assert_eq!(
            got.len(),
            got.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4 + 25
        );
    }

    #[test]
    fn an_endpoint_url_that_is_not_http_is_refused() {
        assert!(super::Endpoint::new("ftp://127.0.0.1/", empty_config()).is_err());
        assert!(super::Endpoint::new("http:///x", empty_config()).is_err());
    }
}
