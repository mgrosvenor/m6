//! Non-blocking HTTP/1.1 and HTTP/2 over TLS (rustls), driven by a service's
//! own `mio` loop one connection at a time.
//!
//! Design:
//! - Each listener is registered under its own fixed token and accepts only
//!   when that token fires.
//! - Every accepted connection is registered under its own token and lives in
//!   a `Slots` table the service owns, so one turn drives one connection.
//! - After the TLS handshake, the negotiated ALPN protocol determines the
//!   handler:
//!
//!   ```text
//!   "h2"       → Http2Conn  (multiplexed streams, full H2 framing)
//!   "http/1.1" → H1 state machine (Reading→Writing, kept alive while
//!                wants_keep_alive(&req) && served + 1 < max_requests)
//!   ```
//!
//! The loop is edge-triggered, so each turn reads and writes until the socket
//! would block. A connection waiting on a backend channel is parked and driven
//! once per iteration by the service's loop, because a channel has no
//! descriptor that fires when its reply arrives.
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::io::{AsRawFd, RawFd};
use std::sync::Arc;
use std::time::Instant;

use m6_core::log::{error, warn};
use rustls::ServerConnection;

use crate::cache::CachedResponse;
use crate::forward::{HttpRequest, PendingDispatch, PendingUrlContext, UpstreamId};
use crate::http2::{H2Io, Http2Conn, RequestRef};
use m6_core::event::{Slots, Taken};
use m6_core::platform::{Interest, Registry, Token};

// ── Request outcome ───────────────────────────────────────────────────────────

/// The result of dispatching a request to a backend.
pub enum RequestOutcome {
    /// Response is available immediately (cache hit, socket backend, auth error, etc.).
    ///
    /// The last field is the backend name, for stats and analytics.
    Ready(u16, Vec<(String, String)>, Vec<u8>, String),
    /// A fresh or stale cache hit, carried as a shared entry so the HTTP/1.1
    /// writer can emit it without copying the headers or the body. The other
    /// protocols convert it back to `Ready` with their owned header form.
    Cached {
        entry: CachedResponse,
        age: std::time::Duration,
        /// A freshly-minted analytics session cookie, when the response is HTML
        /// and this visit has no session yet. None for a returning visitor.
        set_cookie: Option<String>,
    },
    /// Backend I/O dispatched. The caller parks the request by reporting the
    /// `UpstreamId` and the request context back through `ConnDrive`, so the
    /// loop's owner can store the waiter in that upstream's slot.
    Pending {
        id: UpstreamId,
        ctx: Box<PendingUrlContext>,
    },
}

// ── Per-connection state ──────────────────────────────────────────────────────

enum ConnKind {
    /// TLS handshake in progress; protocol not yet known.
    Handshake { created: Instant },
    /// HTTP/1.1 after handshake.
    Http1(H1Conn),
    /// HTTP/2 after handshake.
    ///
    /// Boxed: `Http2Conn` is 336 bytes against the 80 of the largest other
    /// variant, so inline it sized every stored `Conn` to match, H1 and
    /// mid-handshake ones included. The cost is one allocation per HTTP/2
    /// connection, paid once at ALPN selection rather than per request, and a
    /// connection here is long-lived. On a small single-core origin holding a
    /// map of these, the 256 bytes saved on every other entry is the better
    /// side of that trade.
    Http2(Box<Http2Conn>),
}

/// One completed handshake, for the caller that owns the stats.
///
/// `drive_one` returns these rather than recording them itself: the listener has
/// no stats handle, and giving it one would couple the transport to the metrics.
#[derive(Debug, Clone, Copy)]
pub struct HandshakeSample {
    pub elapsed_ns: u64,
    /// True if ALPN selected h2. False means HTTP/1.1.
    pub is_h2: bool,
    /// True if the session was RESUMED rather than negotiated from scratch.
    ///
    /// Reported separately and never blended with full handshakes: a resumed one
    /// skips the certificate and the signature, so mixing them gives a figure that
    /// tracks the returning-visitor mix rather than the cost of either.
    ///
    /// This used to say resumption "happens without anything being configured
    /// for it", on the strength of rustls' default 256-session store. That was
    /// the assumption that let the defect sit: 256 entries is nothing on a node
    /// taking hundreds of handshakes an hour, so the store evicted every browser
    /// before it came back. `make_tls_server_config` now installs a ticketer,
    /// which is what makes resumption stateless and this number mean something.
    pub resumed: bool,
}

pub struct Conn {
    /// Handshake duration, set once when TLS completes and taken by `drive_one`.
    ///
    /// Recorded here rather than passed out through a callback because
    /// `drive_conn` is a free function over one connection and knows nothing
    /// about the stats.
    handshake_ns: Option<u64>,
    /// Whether that handshake negotiated h2. `None` means h1 or no ALPN.
    handshake_h2: bool,
    /// Whether that handshake resumed a session rather than doing full key
    /// exchange. Stamped at the same moment as `handshake_ns`.
    handshake_resumed: bool,
    /// `None` while the slot is free, so a free slot costs no live descriptor.
    stream: Option<TcpStream>,
    /// Client address, captured once at accept. Reading it here avoids a
    /// `peer_addr()` syscall and a `String` allocation per request-path drive.
    client_ip: String,
    /// `None` on a plaintext listener. HTTP/2 already carried this
    /// distinction (`H2Io::Tls` / `H2Io::Plain`); this gives HTTP/1.1 the same
    /// one, so `:80` runs this code rather than a second implementation.
    tls: Option<ServerConnection>,
    kind: ConnKind,
    /// The `mio` token this connection's descriptor is registered under.
    token: Token,
    /// What the poll currently waits for, so `reregister` runs only on change.
    registered_interest: Interest,
    /// Accepted on the status listener, so the loop answers only the
    /// monitoring paths on it.
    status: bool,
}

/// What one connection's turn reports to the loop that owns the timeouts and
/// the pending dispatches.
pub struct ConnDrive {
    /// Requests this turn dispatched to a backend: the loop stores each one's
    /// waiter in the upstream slot it names.
    pub pending: Vec<PendingDispatch>,
    /// The connection is finished: deregister, free its token and close it.
    pub done: bool,
    /// An HTTP/1.1 request window began: touch the read timeout.
    pub restarted: bool,
    /// HTTP/2 frames were received: touch the h2 idle timeout.
    pub moved: bool,
    /// A handshake completed this turn, for the stats owner.
    pub handshake: Option<HandshakeSample>,
}

impl Conn {
    /// The descriptor to register with the poll. Panics on a free slot.
    pub fn raw_fd(&self) -> RawFd {
        self.stream.as_ref().expect("occupied slot").as_raw_fd()
    }

    pub fn set_token(&mut self, token: Token) {
        self.token = token;
    }

    pub fn set_interest(&mut self, interest: Interest) {
        self.registered_interest = interest;
    }

    fn empty() -> Conn {
        Conn {
            handshake_ns: None,
            handshake_h2: false,
            handshake_resumed: false,
            stream: None,
            client_ip: String::new(),
            tls: None,
            kind: ConnKind::Handshake {
                created: m6_core::platform::now(),
            },
            token: Token(usize::MAX),
            registered_interest: Interest::READABLE,
            status: false,
        }
    }

    fn is_done(&self) -> bool {
        match &self.kind {
            ConnKind::Http1(c) => matches!(c.state, H1State::Done),
            ConnKind::Http2(c) => c.is_done(),
            ConnKind::Handshake { .. } => false,
        }
    }

    /// The interest this connection should be registered with next.
    fn interest(&self) -> Interest {
        match &self.kind {
            ConnKind::Handshake { .. } => {
                let mut i = Interest::READABLE;
                if self.tls.as_ref().is_some_and(|t| t.wants_write()) {
                    i |= Interest::WRITABLE;
                }
                i
            }
            ConnKind::Http1(h1) => match h1.state {
                H1State::Reading => Interest::READABLE,
                H1State::WaitingBackend => Interest::READABLE,
                H1State::Writing { .. } => Interest::READABLE | Interest::WRITABLE,
                H1State::Done => Interest::READABLE,
            },
            ConnKind::Http2(h2) => {
                let mut i = Interest::READABLE;
                if h2.has_pending_send() || self.tls.as_ref().is_some_and(|t| t.wants_write()) {
                    i |= Interest::WRITABLE;
                }
                i
            }
        }
    }
}

// ── HTTP/1.1 state machine ────────────────────────────────────────────────────

enum H1State {
    Reading,
    /// A request was dispatched to a backend; the upstream's completion writes
    /// the response straight into this connection, so no receiver or context is
    /// parked here.
    WaitingBackend,
    Writing {
        pos: usize,
    },
    Done,
}

struct H1Conn {
    state: H1State,
    /// The request bytes for the request being read.
    ///
    /// The buffer belongs to the connection, not to the request. It used to be
    /// part of `H1State::Reading` and `finish_response` replaced it with a fresh
    /// `Vec`, so every request on a keep-alive connection paid for a read buffer
    /// that the previous request had already grown. Its capacity is now reused:
    /// `finish_response` clears it and the next request refills it.
    read_buf: Vec<u8>,
    /// The response bytes for the response being written, allocated once per
    /// connection slot and reused for every response on it.
    ///
    /// Both writers append into this buffer, so a cached hit or a backend reply
    /// writes its head and body by offset and length with no growth after the
    /// first response. `Vec::clear` keeps the capacity, which is what makes the
    /// second request on a connection free.
    out_buf: Vec<u8>,
    /// Reset at the start of each request on a reused connection, so the
    /// timeout below is idle time and not total connection age.
    created: Instant,
    /// Whether the response now being written leaves the connection open.
    /// Decided per request from the version and the `Connection` header.
    keep_alive: bool,
    /// Requests answered on this connection. Bounded so one client cannot
    /// hold a slot indefinitely by pipelining forever.
    served: u32,
    /// Requests this connection may serve, from `[server]
    /// max_requests_per_connection`.
    max_requests: u32,
    /// Whether the current request's `Expect` field has been dealt with.
    /// Answering twice would put a second interim response on the wire.
    expect_handled: bool,
}

/// Idle timeout for an HTTP/1.1 connection, measured from the start of the
/// current request rather than from when the connection opened.
pub const READ_TIMEOUT_SECS: u64 = 30;

/// Most requests a single HTTP/1.1 connection may serve before it is closed.
///
/// Persistent connections are the default in HTTP/1.1 (RFC 9112 9.3) and this
/// server did not implement them: every response carried `connection: close`.
/// Legal, and expensive. Public clients mostly negotiate h2 so they were
/// unaffected, but **m6-http talks HTTP/1.1 to its own backends**, so every
/// cache miss paid a fresh connect to m6-file or m6-html.
///
/// Default for `[server] max_requests_per_connection`, which holds the
/// rationale for a cap.
pub const DEFAULT_MAX_REQUESTS_PER_CONN: u32 = 100;
/// Idle timeout for HTTP/2 connections (reused across many requests).
pub const H2_IDLE_TIMEOUT_SECS: u64 = 300;
// ── Public API ────────────────────────────────────────────────────────────────

pub struct Http11Listener {
    listener: TcpListener,
    /// `[server] status_bind`: a second listening socket whose connections
    /// share this table, marked [`Conn::is_status`].
    status: Option<TcpListener>,
    /// `None` binds a plaintext listener. Only HTTP/1.1 is reachable then:
    /// h2 needs ALPN, which needs TLS, and h3 needs QUIC, which mandates it.
    /// A browser following an `http://` link speaks HTTP/1.1 and nothing else.
    tls_config: Option<Arc<rustls::ServerConfig>>,
    conns: Slots<Conn>,
    /// `[server] max_requests_per_connection`, copied into each connection.
    max_requests: u32,
    /// Reused buffer a drive reports its pending dispatches through, so a turn
    /// that parks a request allocates nothing.
    pending: Vec<PendingDispatch>,
    /// The descriptor this listener refuses a queued connection with when the
    /// process is out of descriptors.
    spare: m6_core::platform::net::SpareDescriptor,
    /// The same for the status listener, which may be bound on another address
    /// family.
    status_spare: Option<m6_core::platform::net::SpareDescriptor>,
    /// The refusals it has made, rate limited and counted.
    refusals: m6_core::net::Refusals,
    /// An accept on the main listener failed for a reason other than
    /// descriptor pressure, with connections possibly still queued: the loop
    /// re-registers it.
    rearm: bool,
    /// The same for the status listener.
    status_rearm: bool,
}

/// The connection table's starting size, shared by every owner in the process.
pub const INITIAL_CONNECTIONS: usize = m6_core::limits::CONNECTIONS.start;

/// Resolve a `host:port` bind string to one address.
///
/// `bind_tcp_reuseaddr` takes a `SocketAddr` because it has to choose a socket
/// domain before binding, which `TcpListener::bind`'s `ToSocketAddrs` hides.
/// The first resolved address wins, which is what `TcpListener::bind` does too
/// for a single-address host like `127.0.0.1` or `0.0.0.0`. `main` resolves
/// with this too, so a shared node's TLS and QUIC sockets bind one address.
pub fn resolve_bind(addr: &str) -> anyhow::Result<std::net::SocketAddr> {
    use std::net::ToSocketAddrs;
    addr.to_socket_addrs()?
        .next()
        .ok_or_else(|| anyhow::anyhow!("bind address {addr:?} resolved to nothing"))
}

/// Write the refusal line an accept pass owes, if any. `served` says a
/// connection was accepted in this pass, which ends a burst: the counts a burst
/// left unwritten go out with it, so the operator sees the exact number
/// refused rather than only that some were.
fn report_refusals(refusals: &mut m6_core::net::Refusals, served: bool) {
    let now = m6_core::platform::now();
    let counts = if served {
        refusals.flush(now)
    } else {
        refusals.due(now)
    };
    if let Some(counts) = counts {
        m6_core::net::warn_refused("m6-http", counts);
    }
}

impl Http11Listener {
    pub fn bind(
        addr: &str,
        tls_config: Arc<rustls::ServerConfig>,
        max_requests: u32,
        initial: usize,
        ceiling: usize,
    ) -> anyhow::Result<Self> {
        Self::bind_maybe_tls(addr, Some(tls_config), max_requests, initial, ceiling)
    }

    /// Bind a plaintext HTTP/1.1 listener, for the `:80` half of the pair.
    pub fn bind_plain(
        addr: &str,
        max_requests: u32,
        initial: usize,
        ceiling: usize,
    ) -> anyhow::Result<Self> {
        Self::bind_maybe_tls(addr, None, max_requests, initial, ceiling)
    }

    /// Wrap the TCP half of a pair bound with `SO_REUSEPORT` by
    /// `m6_core::platform::net::listen_shared`, for `[server] reuse_port`.
    pub fn from_shared(
        listener: std::net::TcpListener,
        tls_config: Arc<rustls::ServerConfig>,
        max_requests: u32,
        initial: usize,
        ceiling: usize,
    ) -> anyhow::Result<Self> {
        Self::from_bound(listener, Some(tls_config), max_requests, initial, ceiling)
    }

    fn bind_maybe_tls(
        addr: &str,
        tls_config: Option<Arc<rustls::ServerConfig>>,
        max_requests: u32,
        initial: usize,
        ceiling: usize,
    ) -> anyhow::Result<Self> {
        // SO_REUSEADDR, which `TcpListener::bind` does not set. Without it a
        // restart inside the TIME_WAIT window cannot rebind, and m6-http treats
        // a failed bind as a warning: the process comes up with no listener at
        // all, running and healthy to systemd, serving nobody.
        let listener = m6_core::server::bind_tcp_reuseaddr(resolve_bind(addr)?)?;
        Self::from_bound(listener, tls_config, max_requests, initial, ceiling)
    }

    fn from_bound(
        listener: std::net::TcpListener,
        tls_config: Option<Arc<rustls::ServerConfig>>,
        max_requests: u32,
        initial: usize,
        ceiling: usize,
    ) -> anyhow::Result<Self> {
        listener.set_nonblocking(true)?;
        // The reserve is opened before any pressure, on this listener's own
        // address family.
        let spare = m6_core::platform::net::SpareDescriptor::for_tcp(listener.local_addr()?)?;
        Ok(Http11Listener {
            listener,
            status: None,
            tls_config,
            conns: Slots::new(initial, ceiling, Conn::empty),
            max_requests,
            pending: Vec::with_capacity(8),
            spare,
            status_spare: None,
            refusals: m6_core::net::Refusals::new(),
            rearm: false,
            status_rearm: false,
        })
    }

    pub fn raw_fd(&self) -> RawFd {
        self.listener.as_raw_fd()
    }

    /// Write the refusal count this listener still owes when the process
    /// stops. The status listener reports through the same count, because both
    /// accepts feed one [`Refusals`](m6_core::net::Refusals).
    pub fn close_refusals(&mut self) {
        m6_core::net::close_refusals("m6-http", &mut self.refusals);
    }

    /// Add the status listener, bound by the caller. Start only.
    pub fn with_status(mut self, listener: TcpListener) -> anyhow::Result<Self> {
        listener.set_nonblocking(true)?;
        // Its own reserve, because a status bind may be on another address
        // family than the main listener.
        self.status_spare = Some(m6_core::platform::net::SpareDescriptor::for_tcp(
            listener.local_addr()?,
        )?);
        self.status = Some(listener);
        Ok(self)
    }

    /// The status listener's descriptor, when there is one.
    pub fn status_fd(&self) -> Option<RawFd> {
        self.status.as_ref().map(AsRawFd::as_raw_fd)
    }

    /// Whether the connection in `key` came in on the status listener.
    pub fn is_status(&self, key: usize) -> bool {
        self.conns.get(key).status
    }

    /// [`Self::accept_one`] for the status listener.
    pub fn accept_status(&mut self) -> Option<Conn> {
        self.accept_on(true)
    }

    /// Whether an accept failed for a reason other than descriptor pressure,
    /// for the main listener and for the status listener. The loop re-registers
    /// each one, so a connection still queued is reported again instead of
    /// waiting behind an edge already consumed.
    pub fn take_rearm(&mut self) -> (bool, bool) {
        (
            std::mem::take(&mut self.rearm),
            std::mem::take(&mut self.status_rearm),
        )
    }

    /// Record that the accept pass on `status`'s listener could not drain.
    fn note_rearm(&mut self, status: bool) {
        if status {
            self.status_rearm = true;
        } else {
            self.rearm = true;
        }
    }

    pub fn local_addr(&self) -> std::io::Result<std::net::SocketAddr> {
        self.listener.local_addr()
    }

    /// A free slot in this listener's connection table.
    pub fn take_slot(&mut self) -> Option<Taken> {
        self.conns.take()
    }

    pub fn slot_mut(&mut self, key: usize) -> &mut Conn {
        self.conns.get_mut(key)
    }

    /// The peer address a connection slot was accepted from, for the handshake
    /// statistics. `None` for a free slot or an address that no longer parses,
    /// which the caller counts as external.
    pub fn peer_ip(&self, key: usize) -> Option<std::net::IpAddr> {
        self.conns.get(key).client_ip.parse().ok()
    }

    pub fn free_slot(&mut self, key: usize) {
        self.conns.free(key);
    }

    /// Whether the connection table is at its ceiling, so accepting should stop.
    pub fn is_full(&self) -> bool {
        self.conns.is_full()
    }

    /// Accept one connection, building its `Conn` (stream, TLS, kind). `None`
    /// when no connection is pending. The caller allocates the token and the
    /// slot, sets `conn.token` and `conn.registered_interest`, registers the descriptor,
    /// and stores the `Conn` back into the slot.
    pub fn accept_one(&mut self) -> Option<Conn> {
        self.accept_on(false)
    }

    fn accept_on(&mut self, status: bool) -> Option<Conn> {
        let listener = match status {
            true => self.status.as_ref()?,
            false => &self.listener,
        };
        let spare = match status {
            true => self.status_spare.as_mut()?,
            false => &mut self.spare,
        };
        loop {
            match listener.accept() {
                Ok((stream, peer)) => {
                    if let Err(e) = stream.set_nonblocking(true) {
                        warn!("tcp set_nonblocking: {e}");
                        continue;
                    }
                    stream.set_nodelay(true).ok();
                    let tls = match &self.tls_config {
                        Some(cfg) => {
                            let mut tls = match ServerConnection::new(Arc::clone(cfg)) {
                                Ok(t) => t,
                                Err(e) => {
                                    warn!("tls ServerConnection::new: {e}");
                                    continue;
                                }
                            };
                            // Raises rustls' *outgoing* buffering caps (sendable_plaintext /
                            // sendable_tls) to match MAX_REQUEST_BYTES, so a large response
                            // written before the peer is ready to receive it doesn't get
                            // truncated. The incoming side (received_plaintext, where large
                            // request bodies land) is a separate, fixed 16 KiB buffer with no
                            // public setter — see the comment on advance_tls's "buffer full"
                            // handling for how that's dealt with instead.
                            tls.set_buffer_limit(Some(crate::limits::edge().request.max()));
                            // Eagerly start handshake: ClientHello is already buffered on loopback.
                            let _ = advance_tls(&mut tls, &stream, None);
                            Some(tls)
                        }
                        // Plaintext: no handshake and no ALPN, so the connection
                        // is HTTP/1.1 from its first byte.
                        None => None,
                    };
                    let client_ip = peer.ip().to_string();
                    let kind = if tls.is_some() {
                        ConnKind::Handshake {
                            created: m6_core::platform::now(),
                        }
                    } else {
                        ConnKind::Http1(H1Conn {
                            state: H1State::Reading,
                            read_buf: Vec::with_capacity(crate::limits::edge().request.start()),
                            out_buf: Vec::with_capacity(crate::limits::edge().response.start()),
                            created: m6_core::platform::now(),
                            keep_alive: false,
                            served: 0,
                            max_requests: self.max_requests,
                            expect_handled: false,
                        })
                    };
                    report_refusals(&mut self.refusals, true);
                    return Some(Conn {
                        handshake_ns: None,
                        handshake_h2: false,
                        handshake_resumed: false,
                        stream: Some(stream),
                        client_ip,
                        tls,
                        kind,
                        token: Token(usize::MAX),
                        registered_interest: Interest::READABLE,
                        status,
                    });
                }
                Err(e) => match m6_core::platform::net::accept_error(&e) {
                    m6_core::platform::net::Accept::Drained => {
                        report_refusals(&mut self.refusals, false);
                        return None;
                    }
                    m6_core::platform::net::Accept::SkipOne => continue,
                    // Out of descriptors: close the reserve and take and
                    // close every connection already queued, so the turn
                    // ends with the queue empty and nothing waiting for a
                    // wakeup that a full queue cannot deliver.
                    m6_core::platform::net::Accept::Exhausted => {
                        let drained = spare.drain(|| {
                            let (stream, _) = listener.accept()?;
                            drop(stream);
                            Ok(())
                        });
                        for _ in 0..drained.refused {
                            self.refusals.refused();
                        }
                        if let Some(e) = &drained.stopped {
                            warn!(
                                error = %e,
                                "accept failed while draining the queued connections"
                            );
                        }
                        if let Some(e) = &drained.lost {
                            // The reserve is gone, so the next shortage
                            // cannot be handled.
                            warn!(error = %e, "the reserve descriptor could not be reopened");
                        }
                        if drained.needs_rearm() {
                            self.note_rearm(status);
                        }
                        report_refusals(&mut self.refusals, false);
                        // The queue is empty, so the turn is done.
                        return None;
                    }
                    // Any other error: say so once and have the loop
                    // re-register the listener, so a connection still queued
                    // is reported again rather than left edge-triggered and
                    // unread.
                    m6_core::platform::net::Accept::Stop => {
                        warn!("tcp accept: {e}");
                        self.note_rearm(status);
                        report_refusals(&mut self.refusals, false);
                        return None;
                    }
                },
            }
        }
    }

    /// Drive one connection. Returns what happened, so the loop can manage the
    /// pending dispatches, the timeouts and the descriptor's interest.
    #[allow(clippy::too_many_arguments)] // one request's callbacks plus state, hit headers and registry
    pub fn drive_one<S, FH, FO, FR>(
        &mut self,
        key: usize,
        state: &mut S,
        on_head: &mut FH,
        on_owned: &mut FO,
        on_head_early: &mut FR,
        hit: &HitHeaders,
        registry: &Registry,
    ) -> ConnDrive
    where
        FH: for<'a> FnMut(&mut S, &RequestHead<'a>, &'a [u8], &str) -> RequestOutcome,
        FO: FnMut(&mut S, RequestRef<'_>, &str) -> RequestOutcome,
        FR: for<'a> FnMut(&mut S, &RequestHead<'a>, &str) -> Option<RequestOutcome>,
    {
        let mut drive = ConnDrive {
            pending: std::mem::take(&mut self.pending),
            done: false,
            restarted: false,
            moved: false,
            handshake: None,
        };
        let conn = self.conns.get_mut(key);
        drive_conn(
            conn,
            self.max_requests,
            state,
            on_head,
            on_owned,
            on_head_early,
            hit,
            &mut drive,
        );
        if let Some(ns) = conn.handshake_ns.take() {
            drive.handshake = Some(HandshakeSample {
                elapsed_ns: ns,
                is_h2: conn.handshake_h2,
                resumed: conn.handshake_resumed,
            });
        }
        drive.done = conn.is_done();
        if !drive.done {
            let interest = conn.interest();
            if interest != conn.registered_interest {
                if let Some(stream) = conn.stream.as_ref() {
                    registry
                        .reregister_fd(stream.as_raw_fd(), conn.token, interest)
                        .ok();
                }
                conn.registered_interest = interest;
            }
        }
        drive
    }

    /// Give back the pending-dispatch buffer the last drive took, cleared, so
    /// the next drive reuses it rather than allocating.
    pub fn return_pending(&mut self, mut pending: Vec<PendingDispatch>) {
        pending.clear();
        self.pending = pending;
    }

    /// Write a finished backend response into the HTTP/1.1 connection that was
    /// waiting on it and drive the write now, so the first bytes leave in this
    /// turn. A connection that is no longer waiting (closed, or reused by a
    /// newer connection) is a no-op, so a stale completion drops its result.
    #[allow(clippy::too_many_arguments)] // one response's fields plus the loop's
    pub fn complete_h1(
        &mut self,
        key: usize,
        status: u16,
        headers: &[(String, String)],
        body: Vec<u8>,
        method: &str,
        registry: &Registry,
    ) -> ConnDrive {
        let mut drive = ConnDrive {
            pending: Vec::new(),
            done: false,
            restarted: false,
            moved: false,
            handshake: None,
        };
        let conn = self.conns.get_mut(key);
        let keep_alive = match &conn.kind {
            ConnKind::Http1(h1) if matches!(h1.state, H1State::WaitingBackend) => h1.keep_alive,
            _ => return drive,
        };
        let ConnKind::Http1(h1) = &mut conn.kind else {
            return drive;
        };
        if make_out_room(
            &mut h1.out_buf,
            &mut h1.state,
            &mut h1.keep_alive,
            body.len(),
        ) {
            h1.out_buf.clear();
            write_response(&mut h1.out_buf, status, headers, &body, method, keep_alive);
            h1.state = H1State::Writing { pos: 0 };
        }
        let mut unit = ();
        let mut on_head = |_: &mut (), _: &RequestHead<'_>, _: &[u8], _: &str| -> RequestOutcome {
            unreachable!("a completed h1 connection is not reading a request")
        };
        let mut on_owned = |_: &mut (), _: RequestRef<'_>, _: &str| -> RequestOutcome {
            unreachable!("a completed h1 connection is not reading a request")
        };
        let mut on_head_early =
            |_: &mut (), _: &RequestHead<'_>, _: &str| -> Option<RequestOutcome> {
                unreachable!("a completed h1 connection is not reading a request")
            };
        if let Some(tls) = conn.tls.as_mut() {
            drive_h1(
                &mut H1Io::Tls {
                    tls,
                    stream: conn.stream.as_ref().expect("tls stream"),
                },
                h1,
                &conn.client_ip,
                &mut unit,
                &mut on_head,
                &mut on_owned,
                &mut on_head_early,
                None,
                &mut drive,
            );
        } else {
            drive_h1(
                &mut H1Io::Plain {
                    stream: conn.stream.as_ref().expect("plaintext stream"),
                },
                h1,
                &conn.client_ip,
                &mut unit,
                &mut on_head,
                &mut on_owned,
                &mut on_head_early,
                None,
                &mut drive,
            );
        }
        drive.done = conn.is_done();
        if !drive.done {
            let interest = conn.interest();
            if interest != conn.registered_interest {
                if let Some(stream) = conn.stream.as_ref() {
                    registry
                        .reregister_fd(stream.as_raw_fd(), conn.token, interest)
                        .ok();
                }
                conn.registered_interest = interest;
            }
        }
        drive
    }

    /// Write a finished backend response into a waiting HTTP/2 stream on a TLS
    /// connection and flush now, so the first bytes leave in this turn. Returns
    /// whether the connection is now done and must be closed.
    pub fn complete_h2(
        &mut self,
        key: usize,
        stream_id: u32,
        response: (u16, Vec<(String, String)>, Vec<u8>),
        method: &str,
        registry: &Registry,
    ) -> bool {
        let (status, headers, body) = response;
        let conn = self.conns.get_mut(key);
        let done = {
            let ConnKind::Http2(h2) = &mut conn.kind else {
                return false;
            };
            {
                let mut io = H2Io::Tls {
                    tls: conn.tls.as_mut().expect("tls path"),
                    stream: conn.stream.as_ref().expect("tls stream"),
                };
                h2.complete_stream(&mut io, stream_id, status, headers, body, method);
            }
            h2.is_done()
        };
        if !done {
            let interest = conn.interest();
            if interest != conn.registered_interest {
                if let Some(stream) = conn.stream.as_ref() {
                    registry
                        .reregister_fd(stream.as_raw_fd(), conn.token, interest)
                        .ok();
                }
                conn.registered_interest = interest;
            }
        }
        done
    }

    /// Deregister and close one connection, freeing its slot for the next one.
    pub fn close_one(&mut self, key: usize, registry: &Registry) {
        let conn = self.conns.get_mut(key);
        if let Some(stream) = conn.stream.take() {
            registry.deregister_fd(stream.as_raw_fd()).ok();
        }
        *conn = Conn::empty();
        self.conns.free(key);
    }
}

// ── H2C (HTTP/2 cleartext) listener ──────────────────────────────────────────

pub struct H2cPlainConn {
    /// `None` while the slot is free.
    stream: Option<TcpStream>,
    /// Boxed so a free slot costs no `Http2Conn` (which holds two 16 KiB
    /// buffers). Allocated once at ALPN selection, not per request.
    h2: Option<Box<Http2Conn>>,
    client_ip: String,
    token: Token,
    registered_interest: Interest,
}

impl H2cPlainConn {
    /// The descriptor to register with the poll. Panics on a free slot.
    pub fn raw_fd(&self) -> RawFd {
        self.stream.as_ref().expect("occupied slot").as_raw_fd()
    }

    pub fn set_token(&mut self, token: Token) {
        self.token = token;
    }

    pub fn set_interest(&mut self, interest: Interest) {
        self.registered_interest = interest;
    }

    fn empty() -> H2cPlainConn {
        H2cPlainConn {
            stream: None,
            h2: None,
            client_ip: String::new(),
            token: Token(usize::MAX),
            registered_interest: Interest::READABLE,
        }
    }

    fn interest(&self) -> Interest {
        let mut i = Interest::READABLE;
        if self.h2.as_ref().is_some_and(|h| h.has_pending_send()) {
            i |= Interest::WRITABLE;
        }
        i
    }
}

pub struct H2cListener {
    listener: TcpListener,
    conns: Slots<H2cPlainConn>,
    /// The peers this listener accepts, when it is internal
    /// (`[server] internal_binds`). `None`: not internal, every peer served
    /// and none trusted to forward a client address. `Some`: only these peers
    /// are accepted, and their forwarded address is believed; any other
    /// connection is closed at accept. The firewall is a second fence, not
    /// the one this trust rests on. See `crate::forward::ForwardedTrust`.
    peers: Option<Vec<std::net::IpAddr>>,
    /// Throttles the log line for refused connections, one peer at a time.
    refusals: RefusalLog,
    /// A capped accept left connections in the kernel queue. mio is
    /// edge-triggered, so the listener will not be reported again for them:
    /// the loop reads this and drains before its next wait.
    accept_again: bool,
    /// Reused buffer a drive reports its pending dispatches through, so a turn
    /// that parks a request allocates nothing.
    pending: Vec<PendingDispatch>,
    /// The descriptor this listener refuses a queued connection with when the
    /// process is out of descriptors.
    spare: m6_core::platform::net::SpareDescriptor,
    /// The refusals it has made for want of a descriptor, counted and rate
    /// limited. The peer refusals above are a different log line.
    fd_refusals: m6_core::net::Refusals,
    /// An accept failed for a reason other than descriptor pressure, with
    /// connections possibly still queued: the loop re-registers the listener.
    rearm: bool,
}

/// The most connections one `accept_pending` call handles. A peer that
/// connects and closes in a loop would otherwise keep the single event loop
/// in `accept` and starve the other listeners. What is left stays in the
/// kernel queue and the poll wakes this listener again.
pub const MAX_ACCEPTS_PER_WAKE: usize = 64;

/// The fewest seconds between two log lines for one refused peer, and the
/// length of the window that `REFUSAL_LOG_MAX_LINES` and the summary line use.
const REFUSAL_LOG_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);

/// The most peers `RefusalLog` tracks. When the table is full, a new address
/// takes the slot of the stalest peer whose last line is older than the
/// interval and that owes no count. When there is no such peer, refusals from
/// the new address are counted into one total instead.
const REFUSAL_LOG_MAX_PEERS: usize = 256;

/// The most per-peer lines written in one window across all peers. A host that
/// rotates through source addresses gets a fresh first line for each one, so
/// the number of lines needs its own ceiling. Refusals past it are counted
/// into the next summary line.
const REFUSAL_LOG_MAX_LINES: u32 = 32;

/// The most distinct untracked addresses `RefusalLog` remembers in one window
/// to count them. Past it the count of distinct addresses stops at this value.
const REFUSAL_LOG_MAX_OTHERS: usize = 1024;

/// What the log owes for the refusals of one window that no line carried.
#[derive(Debug, Default, PartialEq, Eq)]
struct RefusalSummary {
    /// Refusals from addresses the full table could not track.
    other_refusals: u64,
    /// Distinct addresses among those, up to `REFUSAL_LOG_MAX_OTHERS`.
    other_addresses: u64,
    /// The most recent of those addresses, so the line names one.
    sample: Option<std::net::IpAddr>,
    /// Refusals from tracked addresses whose line the window limit withheld.
    over_limit: u64,
}

/// What `RefusalLog::record` tells the caller to write.
#[derive(Debug, Default, PartialEq, Eq)]
struct RefusalVerdict {
    /// A line for this peer, carrying the refusals suppressed since its last line.
    peer_line: Option<u64>,
    /// A summary of the window that just ended, when it had anything to report.
    summary: Option<RefusalSummary>,
}

/// Decides which refused connections get a log line.
/// A tracked peer gets a line on its first refusal, then at most one per
/// `interval` carrying the count since its last line.
/// Across all peers a window of `interval` holds at most `REFUSAL_LOG_MAX_LINES` such lines.
/// When the table holds `max_peers` peers, a new address takes the slot of the
/// stalest peer that owes nothing, or else is counted into a total with the
/// most recent such address kept as a sample. The first refusal after the
/// window ends returns one summary.
/// Memory is bounded and a refusal allocates nothing once the tables are full.
struct RefusalLog {
    interval: std::time::Duration,
    max_peers: usize,
    /// Per peer: when it was last logged (`None` while its first line is
    /// withheld by the window limit), and the refusals counted since.
    peers: std::collections::HashMap<std::net::IpAddr, (Option<Instant>, u64)>,
    /// When the current window began. `None` until the first refusal.
    window_start: Option<Instant>,
    lines_in_window: u32,
    summary: RefusalSummary,
    others: std::collections::HashSet<std::net::IpAddr>,
}

impl RefusalLog {
    fn new(interval: std::time::Duration, max_peers: usize) -> Self {
        RefusalLog {
            interval,
            max_peers,
            peers: std::collections::HashMap::with_capacity(max_peers),
            window_start: None,
            lines_in_window: 0,
            summary: RefusalSummary::default(),
            others: std::collections::HashSet::with_capacity(REFUSAL_LOG_MAX_OTHERS),
        }
    }

    /// Record a refusal of `peer` at `now` and say what to write.
    fn record(&mut self, peer: std::net::IpAddr, now: Instant) -> RefusalVerdict {
        let mut verdict = RefusalVerdict::default();
        match self.window_start {
            Some(start) if now.duration_since(start) < self.interval => {}
            started => {
                if started.is_some() && self.summary != RefusalSummary::default() {
                    verdict.summary = Some(std::mem::take(&mut self.summary));
                }
                self.window_start = Some(now);
                self.lines_in_window = 0;
                self.others.clear();
            }
        }

        if let Some((last, suppressed)) = self.peers.get_mut(&peer) {
            if last.is_some_and(|l| now.duration_since(l) < self.interval) {
                *suppressed += 1;
            } else if self.lines_in_window >= REFUSAL_LOG_MAX_LINES {
                *suppressed += 1;
                self.summary.over_limit += 1;
            } else {
                self.lines_in_window += 1;
                verdict.peer_line = Some(*suppressed);
                *last = Some(now);
                *suppressed = 0;
            }
            return verdict;
        }

        if self.peers.len() >= self.max_peers && !self.evict_stale(now) {
            self.summary.other_refusals += 1;
            self.summary.sample = Some(peer);
            if self.others.len() < REFUSAL_LOG_MAX_OTHERS && self.others.insert(peer) {
                self.summary.other_addresses += 1;
            }
        } else if self.lines_in_window >= REFUSAL_LOG_MAX_LINES {
            // Tracked, with its first line owed once the window turns.
            self.peers.insert(peer, (None, 1));
            self.summary.over_limit += 1;
        } else {
            self.lines_in_window += 1;
            self.peers.insert(peer, (Some(now), 0));
            verdict.peer_line = Some(0);
        }
        verdict
    }
}

impl RefusalLog {
    /// Drop the stalest tracked peer whose last line is older than `interval`
    /// and that has no suppressed count owed. Returns whether it dropped one.
    /// Such a peer has nothing left to report, and the window line limit keeps
    /// a rotating host from turning the freed slots back into a line per refusal.
    fn evict_stale(&mut self, now: Instant) -> bool {
        let stalest = self
            .peers
            .iter()
            .filter_map(|(ip, (last, suppressed))| match last {
                Some(l) if *suppressed == 0 && now.duration_since(*l) >= self.interval => {
                    Some((*l, *ip))
                }
                _ => None,
            })
            .min_by_key(|(l, _)| *l);
        match stalest {
            Some((_, ip)) => {
                self.peers.remove(&ip);
                true
            }
            None => false,
        }
    }
}

/// What a connection from `peer` gets on an internal listener: `None` refuses
/// it. `peers` is `None` for a listener that is not internal, which serves
/// everyone and trusts no one. Shared by the h2c listener and the cache
/// control listener, so one rule admits a backbone peer everywhere.
pub fn admit(
    peers: Option<&[std::net::IpAddr]>,
    peer: std::net::IpAddr,
) -> Option<crate::forward::ForwardedTrust> {
    match peers {
        None => Some(crate::forward::ForwardedTrust::Never),
        Some(p) if p.contains(&peer.to_canonical()) => {
            Some(crate::forward::ForwardedTrust::Backbone)
        }
        Some(_) => None,
    }
}

impl H2cListener {
    pub fn bind(
        addr: &str,
        internal_peers: Option<Vec<std::net::IpAddr>>,
        initial: usize,
        ceiling: usize,
    ) -> anyhow::Result<Self> {
        let listener = m6_core::server::bind_tcp_reuseaddr(resolve_bind(addr)?)?;
        listener.set_nonblocking(true)?;

        if internal_peers.is_none() {
            warn!(
                bind = %addr,
                "h2c listener is not named in [server] internal_binds: forwarded \
                 client addresses will NOT be trusted, so requests relayed through it \
                 are attributed to the peer that relayed them"
            );
        }

        // The reserve is opened before any pressure, on this listener's own
        // address family.
        let spare = m6_core::platform::net::SpareDescriptor::for_tcp(listener.local_addr()?)?;
        Ok(H2cListener {
            listener,
            conns: Slots::new(initial, ceiling, H2cPlainConn::empty),
            peers: internal_peers,
            refusals: RefusalLog::new(REFUSAL_LOG_INTERVAL, REFUSAL_LOG_MAX_PEERS),
            accept_again: false,
            pending: Vec::with_capacity(8),
            spare,
            fd_refusals: m6_core::net::Refusals::new(),
            rearm: false,
        })
    }

    pub fn raw_fd(&self) -> RawFd {
        self.listener.as_raw_fd()
    }

    /// Write the refusal count this listener still owes when the process
    /// stops.
    pub fn close_refusals(&mut self) {
        m6_core::net::close_refusals("m6-http", &mut self.fd_refusals);
    }

    pub fn local_addr(&self) -> std::io::Result<std::net::SocketAddr> {
        self.listener.local_addr()
    }

    /// Whether a capped accept left connections in the kernel queue.
    pub fn accept_again(&self) -> bool {
        self.accept_again
    }

    pub fn set_accept_again(&mut self, again: bool) {
        self.accept_again = again;
    }

    /// Whether an accept failed for a reason other than descriptor pressure.
    /// The loop re-registers the listener, so a connection still queued is
    /// reported again instead of waiting behind an edge already consumed.
    pub fn take_rearm(&mut self) -> bool {
        std::mem::take(&mut self.rearm)
    }

    /// A free slot in this listener's connection table.
    pub fn take_slot(&mut self) -> Option<Taken> {
        self.conns.take()
    }

    pub fn slot_mut(&mut self, key: usize) -> &mut H2cPlainConn {
        self.conns.get_mut(key)
    }

    pub fn free_slot(&mut self, key: usize) {
        self.conns.free(key);
    }

    /// Accept one connection, refusing a peer not on the internal list.
    pub fn accept_one(&mut self) -> H2cAccept {
        match self.listener.accept() {
            Ok((stream, peer)) => {
                let Some(trust) = admit(self.peers.as_deref(), peer.ip()) else {
                    let verdict = self.refusals.record(peer.ip(), m6_core::platform::now());
                    if let Some(s) = verdict.summary {
                        warn!(
                            other_refusals = s.other_refusals,
                            other_addresses = s.other_addresses,
                            sample = ?s.sample,
                            over_limit = s.over_limit,
                            "h2c: refused connections in the last interval that no line named, \
                             from addresses beyond the tracked peers or past the line limit"
                        );
                    }
                    if let Some(suppressed) = verdict.peer_line {
                        warn!(
                            peer = %peer.ip(),
                            suppressed,
                            "h2c: refused a connection from a peer not in [server] internal_peers"
                        );
                    }
                    drop(stream);
                    return H2cAccept::Refused;
                };
                if let Err(e) = stream.set_nonblocking(true) {
                    warn!("h2c set_nonblocking: {e}");
                    drop(stream);
                    return H2cAccept::Refused;
                }
                stream.set_nodelay(true).ok();
                let h2 = match trust {
                    crate::forward::ForwardedTrust::Backbone => {
                        Http2Conn::new().trusting_forwarded_for()
                    }
                    crate::forward::ForwardedTrust::Never => Http2Conn::new(),
                };
                H2cAccept::Admitted(H2cPlainConn {
                    stream: Some(stream),
                    h2: Some(Box::new(h2)),
                    client_ip: peer.ip().to_string(),
                    token: Token(usize::MAX),
                    registered_interest: Interest::READABLE,
                })
            }
            Err(e) => match m6_core::platform::net::accept_error(&e) {
                m6_core::platform::net::Accept::Drained => H2cAccept::WouldBlock,
                // Gone before it was accepted: counted and passed over.
                m6_core::platform::net::Accept::SkipOne => H2cAccept::Refused,
                // Out of descriptors: refuse the connection already queued by
                // spending the reserve, so it is not left waiting for a wakeup
                // that a full queue cannot deliver. The caller keeps accepting
                // while its budget lasts.
                m6_core::platform::net::Accept::Exhausted => {
                    let listener = &self.listener;
                    let drained = self.spare.drain(|| {
                        let (stream, _) = listener.accept()?;
                        drop(stream);
                        Ok(())
                    });
                    for _ in 0..drained.refused {
                        self.fd_refusals.refused();
                    }
                    if let Some(e) = &drained.stopped {
                        warn!(
                            error = %e,
                            "accept failed while draining the queued connections"
                        );
                    }
                    if let Some(e) = &drained.lost {
                        // The reserve is gone, so the next shortage cannot be
                        // handled.
                        warn!(error = %e, "the reserve descriptor could not be reopened");
                    }
                    if drained.needs_rearm() {
                        self.rearm = true;
                    }
                    // The queue is empty, so the accept pass is done.
                    H2cAccept::WouldBlock
                }
                // Any other error: say so once and have the loop re-register
                // the listener, so a connection still queued is reported again
                // rather than left edge-triggered and unread.
                m6_core::platform::net::Accept::Stop => {
                    warn!("h2c accept: {e}");
                    self.rearm = true;
                    H2cAccept::WouldBlock
                }
            },
        }
    }

    /// Accept and admit up to `max` connections, refused ones included. Each
    /// admitted connection is handed to `store`, which allocates its token and
    /// slot; it returns `false` to stop early. Returns how many were handled.
    pub fn accept_up_to(
        &mut self,
        max: usize,
        mut store: impl FnMut(&mut H2cListener, H2cPlainConn) -> bool,
    ) -> usize {
        let mut handled = 0;
        let mut served = false;
        while handled < max {
            match self.accept_one() {
                H2cAccept::Admitted(conn) => {
                    handled += 1;
                    served = true;
                    if !store(self, conn) {
                        break;
                    }
                }
                H2cAccept::Refused => handled += 1,
                H2cAccept::WouldBlock => break,
            }
        }
        report_refusals(&mut self.fd_refusals, served);
        handled
    }

    /// Drive one cleartext HTTP/2 connection.
    pub fn drive_one<S, F>(
        &mut self,
        key: usize,
        state: &mut S,
        on_request: &mut F,
        hit: &HitHeaders,
        registry: &Registry,
    ) -> ConnDrive
    where
        F: FnMut(&mut S, RequestRef<'_>, &str) -> RequestOutcome,
    {
        let mut drive = ConnDrive {
            pending: std::mem::take(&mut self.pending),
            done: false,
            restarted: false,
            moved: false,
            handshake: None,
        };
        let conn = self.conns.get_mut(key);
        let moved = conn.h2.as_mut().expect("h2c slot is occupied").drive(
            H2Io::Plain {
                stream: conn.stream.as_ref().expect("h2c stream"),
            },
            &conn.client_ip,
            state,
            on_request,
            Some(hit),
            &mut drive.pending,
        );
        drive.moved = moved;
        drive.done = conn.h2.as_ref().expect("h2c slot").is_done();
        if !drive.done {
            let interest = conn.interest();
            if interest != conn.registered_interest {
                if let Some(stream) = conn.stream.as_ref() {
                    registry
                        .reregister_fd(stream.as_raw_fd(), conn.token, interest)
                        .ok();
                }
                conn.registered_interest = interest;
            }
        }
        drive
    }

    /// Give back the pending-dispatch buffer the last drive took, cleared, so
    /// the next drive reuses it rather than allocating.
    pub fn return_pending(&mut self, mut pending: Vec<PendingDispatch>) {
        pending.clear();
        self.pending = pending;
    }

    /// Write a finished backend response into a waiting HTTP/2 stream on a
    /// cleartext connection and flush now, so the first bytes leave this turn.
    /// Returns whether the connection is now done and must be closed.
    pub fn complete_h2(
        &mut self,
        key: usize,
        stream_id: u32,
        response: (u16, Vec<(String, String)>, Vec<u8>),
        method: &str,
        registry: &Registry,
    ) -> bool {
        let (status, headers, body) = response;
        let conn = self.conns.get_mut(key);
        let done = {
            let Some(h2) = conn.h2.as_mut() else {
                return false;
            };
            {
                let mut io = H2Io::Plain {
                    stream: conn.stream.as_ref().expect("h2c stream"),
                };
                h2.complete_stream(&mut io, stream_id, status, headers, body, method);
            }
            h2.is_done()
        };
        if !done {
            let interest = conn.interest();
            if interest != conn.registered_interest {
                if let Some(stream) = conn.stream.as_ref() {
                    registry
                        .reregister_fd(stream.as_raw_fd(), conn.token, interest)
                        .ok();
                }
                conn.registered_interest = interest;
            }
        }
        done
    }

    /// Deregister and close one connection, freeing its slot for the next one.
    pub fn close_one(&mut self, key: usize, registry: &Registry) {
        let conn = self.conns.get_mut(key);
        if let Some(stream) = conn.stream.take() {
            registry.deregister_fd(stream.as_raw_fd()).ok();
        }
        *conn = H2cPlainConn::empty();
        self.conns.free(key);
    }
}

/// What one `H2cListener::accept_one` call found.
pub enum H2cAccept {
    /// A connection admitted and built, ready to be registered and stored.
    Admitted(H2cPlainConn),
    /// A connection accepted and refused (peer not on the internal list).
    Refused,
    /// No connection was pending.
    WouldBlock,
}

// ── Per-connection driver ─────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)] // the connection, its state and the three dispatch closures
fn drive_conn<S, FH, FO, FR>(
    conn: &mut Conn,
    max_requests: u32,
    state: &mut S,
    on_head: &mut FH,
    on_owned: &mut FO,
    on_head_early: &mut FR,
    hit: &HitHeaders,
    drive: &mut ConnDrive,
) where
    FH: for<'a> FnMut(&mut S, &RequestHead<'a>, &'a [u8], &str) -> RequestOutcome,
    FO: FnMut(&mut S, RequestRef<'_>, &str) -> RequestOutcome,
    FR: for<'a> FnMut(&mut S, &RequestHead<'a>, &str) -> Option<RequestOutcome>,
{
    // Plaintext connection: HTTP/1.1 only, straight to the state machine. No
    // handshake to pump and no ALPN to dispatch on.
    if conn.tls.is_none() {
        let ConnKind::Http1(h1) = &mut conn.kind else {
            return;
        };
        drive_h1(
            &mut H1Io::Plain {
                stream: conn.stream.as_ref().expect("plaintext stream"),
            },
            h1,
            &conn.client_ip,
            state,
            on_head,
            on_owned,
            on_head_early,
            Some(hit),
            drive,
        );
        return;
    }

    // HTTP/2: stream and tls are in conn; pass them by reference.
    if let ConnKind::Http2(h2) = &mut conn.kind {
        let moved = h2.drive(
            H2Io::Tls {
                tls: conn.tls.as_mut().expect("tls path"),
                stream: conn.stream.as_ref().expect("tls stream"),
            },
            &conn.client_ip,
            state,
            on_owned,
            Some(hit),
            &mut drive.pending,
        );
        drive.moved |= moved;
        return;
    }

    // Pump TLS I/O for the handshake phase. Once the handshake completes the
    // connection is HTTP/1.1 or HTTP/2, and the H1 request path pumps its own
    // ciphertext through `drive_h1`.
    let tls_failed = advance_tls(
        conn.tls.as_mut().expect("tls path"),
        conn.stream.as_ref().expect("tls stream"),
        None,
    )
    .is_err();

    // ── Stamp the handshake BEFORE deciding the connection is unusable ────────
    //
    // This sits above the error path deliberately, and the ordering is the whole
    // fix. `advance_tls` reads whatever arrived; when a client completes its
    // handshake and closes immediately, the final flight and the FIN arrive
    // together, so ONE call finishes the handshake and then hits EOF and returns
    // Err. The old code took the error path, replaced `conn.kind` (destroying the
    // `Handshake` variant and its `created` stamp) and returned, roughly forty
    // lines above the point where the handshake was recorded. A completed,
    // perfectly normal handshake was thrown away.
    //
    // A 200-connection probe per protocol, run with m6-probe-h1/h2/h3, shows
    // the bias:
    //
    //   h1   0 of 200 recorded.       Nothing happens after an h1 handshake, so
    //                                 the close won the race every single time.
    //   h2  only a subset recorded.   The survivors were connections the server
    //                                 reached before the close, meaning ones that
    //                                 had already waited an extra event-loop turn.
    //                                 The population was selected for slowness.
    //   h3  all 200 recorded.         Correct, because the QUIC path in main.rs
    //                                 stamps at `is_established()` inside the
    //                                 packet handler where nothing can overtake it.
    //
    // A partial sample set that is also biased is worse than none: the h2
    // median looked plausible and was reported as the node's handshake cost.
    //
    // `handshake_ns.is_none()` guards against re-stamping, since this runs on
    // every wakeup for the connection.
    if conn.handshake_ns.is_none() {
        if let ConnKind::Handshake { created, .. } = &conn.kind {
            let tls = conn.tls.as_ref().expect("tls path");
            if !tls.is_handshaking() {
                let elapsed = created.elapsed().as_nanos() as u64;
                let is_h2 = tls.alpn_protocol() == Some(b"h2".as_slice());
                // `handshake_kind()` is only meaningful once the handshake is
                // over, which is exactly here. `FullWithHelloRetryRequest` counts
                // as full: it did the key exchange, and then some.
                let resumed = matches!(tls.handshake_kind(), Some(rustls::HandshakeKind::Resumed));
                conn.handshake_ns = Some(elapsed);
                conn.handshake_h2 = is_h2;
                conn.handshake_resumed = resumed;
            }
        }
    }

    if tls_failed {
        conn.kind = ConnKind::Http1(H1Conn {
            state: H1State::Done,
            read_buf: Vec::new(),
            out_buf: Vec::new(),
            created: m6_core::platform::now(),
            keep_alive: false,
            served: 0,
            max_requests,
            expect_handled: false,
        });
        return;
    }

    // If still handshaking, nothing more to do this tick.
    if conn.tls.as_ref().expect("tls path").is_handshaking() {
        return;
    }

    // Handshake just completed -- dispatch on ALPN.
    if let ConnKind::Handshake { created } = &conn.kind {
        let proto = conn
            .tls
            .as_ref()
            .expect("tls path")
            .alpn_protocol()
            .map(|p| p.to_vec());
        // Still needed by the H1 branch below, which carries the accept stamp
        // forward as the connection's own age. The handshake DURATION is no
        // longer taken here: it is stamped above, before the error path can
        // discard it.
        let created = *created;
        if proto.as_deref() == Some(b"h2".as_slice()) {
            conn.kind = ConnKind::Http2(Box::default());
            // Drive immediately — client preface may already be buffered.
            let ConnKind::Http2(h2) = &mut conn.kind else {
                return;
            };
            let moved = h2.drive(
                H2Io::Tls {
                    tls: conn.tls.as_mut().expect("tls path"),
                    stream: conn.stream.as_ref().expect("tls stream"),
                },
                &conn.client_ip,
                state,
                on_owned,
                Some(hit),
                &mut drive.pending,
            );
            drive.moved |= moved;
            return;
        } else {
            conn.kind = ConnKind::Http1(H1Conn {
                state: H1State::Reading,
                read_buf: Vec::with_capacity(crate::limits::edge().request.start()),
                out_buf: Vec::with_capacity(crate::limits::edge().response.start()),
                created,
                keep_alive: false,
                served: 0,
                max_requests,
                expect_handled: false,
            });
        }
    }

    // Drive HTTP/1.1 state machine.
    let ConnKind::Http1(h1) = &mut conn.kind else {
        return;
    };
    drive_h1(
        &mut H1Io::Tls {
            tls: conn.tls.as_mut().expect("tls path"),
            stream: conn.stream.as_ref().expect("tls stream"),
        },
        h1,
        &conn.client_ip,
        state,
        on_head,
        on_owned,
        on_head_early,
        Some(hit),
        drive,
    );
}

/// Where an HTTP/1.1 connection's bytes come from and go to.
///
/// **HTTP/2 has had this since h2c existed** -- `H2Io { Tls, Plain }` in
/// `http2.rs`, which is how the cleartext backbone reuses the h2 engine.
/// HTTP/1.1 never got the same treatment, so the plaintext `:80` listener was
/// written as a second, independent HTTP/1.1 implementation: `redirect.rs`,
/// 325 lines with its own poll loop, parser and response writer.
///
/// That second implementation scored 5-7 of 32 on h1spec, varying run to run,
/// while this one is exercised by the whole test suite. It dropped the request
/// of any client that half-closed its write side after sending -- normal,
/// RFC-conformant behaviour -- because it treated read()==0 as "peer gone" and
/// skipped the parse-and-respond it had already buffered. 80% of such clients
/// got no answer at all.
///
/// One implementation per protocol version. The plaintext listener is this
/// same code with `Plain`, and the redirect is a response, not a server.
enum H1Io<'a> {
    Tls {
        tls: &'a mut ServerConnection,
        stream: &'a TcpStream,
    },
    Plain {
        stream: &'a TcpStream,
    },
}

impl H1Io<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            H1Io::Tls { tls, .. } => tls.writer().write(buf),
            H1Io::Plain { stream } => (&mut &**stream).write(buf),
        }
    }

    /// Whether rustls still holds encrypted bytes to flush, so the connection
    /// asks for write readiness rather than finishing.
    fn wants_write(&self) -> bool {
        match self {
            H1Io::Tls { tls, .. } => tls.wants_write(),
            H1Io::Plain { .. } => false,
        }
    }

    /// Flush pending TLS ciphertext to the socket, without pulling ciphertext.
    ///
    /// The write path only owes bytes outward; `read_tls` would be a wasted
    /// `recvfrom` (a WouldBlock on a socket that has nothing for us) once per
    /// response. `write_tls` alone drains rustls's outgoing buffer.
    fn flush_write(&mut self) -> io::Result<()> {
        match self {
            H1Io::Tls { tls, stream } => drain_writes(tls, stream),
            H1Io::Plain { .. } => Ok(()),
        }
    }

    /// Read request bytes into `tmp`, pulling TLS ciphertext from the socket as
    /// needed so a body larger than rustls's plaintext buffer is read in one
    /// turn. `buf` is the request buffer, the overflow target. Returns `Ok(n)`
    /// with `n` bytes in `tmp`, `WouldBlock` when the socket has no more, or
    /// `UnexpectedEof`/another error.
    fn read_pumped(&mut self, buf: &mut Vec<u8>, tmp: &mut [u8]) -> io::Result<usize> {
        match self {
            H1Io::Plain { stream } => (&mut &**stream).read(tmp),
            H1Io::Tls { tls, stream } => loop {
                match tls.reader().read(tmp) {
                    Ok(0) => {}
                    Ok(n) => return Ok(n),
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                    Err(e) => return Err(e),
                }
                match advance_tls(tls, stream, Some(buf))? {
                    true => {}
                    false => return Err(io::Error::new(io::ErrorKind::WouldBlock, "would block")),
                }
            },
        }
    }
}

/// Answer a request's `Expect` field from the buffered bytes, once per request.
///
/// RFC 9110 10.1.1: a client that sent `Expect: 100-continue` holds its body
/// back until the server answers. This must run on every incomplete read, not
/// only when the head is still arriving, or a request whose head is complete but
/// whose body has not arrived stalls until the client times out. Returns `true`
/// when a final 417 was queued, so the caller stops reading and switches to
/// writing.
fn handle_expect(h1: &mut H1Conn, io: &mut H1Io<'_>) -> bool {
    if h1.expect_handled {
        return false;
    }
    let Some(exp) = (match &h1.state {
        H1State::Reading => m6_core::h1::expectation(&h1.read_buf),
        _ => None,
    }) else {
        // Head still arriving; nothing to conclude yet.
        return false;
    };
    match exp {
        m6_core::h1::Expectation::None => h1.expect_handled = true,
        m6_core::h1::Expectation::Continue => {
            h1.expect_handled = true;
            let _ = io.write(m6_core::h1::CONTINUE_RESPONSE);
            let _ = io.flush_write();
        }
        m6_core::h1::Expectation::Unsupported => {
            h1.expect_handled = true;
            start_response(
                h1,
                b"HTTP/1.1 417 Expectation Failed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            );
            return true;
        }
    }
    false
}

/// Refuse a request that has passed the `edge_request` maximum: one error line
/// names the limit, the size needed and the key to raise, and the connection
/// answers 413 and closes.
fn refuse_oversized_request(h1: &mut H1Conn, needed: usize) {
    let limit = &crate::limits::edge().request;
    error!("{}", limit.exceeded(needed, "request"));
    start_response(
        h1,
        b"HTTP/1.1 413 Payload Too Large\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
    );
}

/// Room kept beside a response body for its status line and headers when the
/// body is checked against the `edge_response` maximum.
const RESPONSE_HEAD_ALLOWANCE: usize = 64 * 1024;

/// Make room in the connection's output buffer for a response with a body of
/// `body_len` bytes, following the `edge_response` limit.
///
/// A body past the maximum is refused: one error line names the limit, the size
/// needed and the key to raise, a 500 is started in its place and the answer is
/// `false`. A body that does not fit the buffer grows it by doubling, clamped
/// to the maximum, with one log line, and the buffer keeps the size. A body that
/// fits changes nothing.
fn make_out_room(
    out_buf: &mut Vec<u8>,
    state: &mut H1State,
    keep_alive: &mut bool,
    body_len: usize,
) -> bool {
    let limit = &crate::limits::edge().response;
    let needed = body_len.saturating_add(RESPONSE_HEAD_ALLOWANCE);
    if needed > limit.max() {
        error!("{}", limit.exceeded(needed, "response"));
        *keep_alive = false;
        out_buf.clear();
        out_buf.extend_from_slice(
            b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        );
        *state = H1State::Writing { pos: 0 };
        return false;
    }
    let cap = out_buf.capacity();
    if body_len > cap {
        if let Some(size) = limit.grown(cap, body_len + 4096) {
            out_buf.clear();
            out_buf.reserve_exact(size);
            limit.log_growth(cap, out_buf.capacity(), "a response");
        }
    }
    true
}

/// The response a malformed request keeps: a bare 400 and close.
const BAD_REQUEST: &[u8] =
    b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";

/// Start a response from fixed bytes: reuse the connection's output buffer,
/// copy them in, and switch to writing.
///
/// The buffer is cleared rather than replaced, so an answer the connection has
/// already sent once costs no allocation to repeat.
fn start_response(h1: &mut H1Conn, response: &[u8]) {
    h1.out_buf.clear();
    h1.out_buf.extend_from_slice(response);
    h1.state = H1State::Writing { pos: 0 };
}

/// The raw HTTP/1.1 response for a request refused at a head limit, written
/// straight into the connection's output buffer.
///
/// The body is the service's usual error page, titled as nginx titles these
/// refusals. No `Server` header is added: the answer is about the request and
/// the status is the whole message.
fn write_head_refusal(h1: &mut H1Conn, why: m6_core::h1::HeadError) {
    let status = why.status();
    let body = crate::error::refusal_body(status);
    let reason = m6_core::h1::status_reason(status);
    let out = &mut h1.out_buf;
    out.clear();
    out.extend_from_slice(b"HTTP/1.1 ");
    out.extend_from_slice(itoa::Buffer::new().format(status).as_bytes());
    out.extend_from_slice(b" ");
    out.extend_from_slice(reason.as_bytes());
    out.extend_from_slice(b"\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: ");
    out.extend_from_slice(itoa::Buffer::new().format(body.len()).as_bytes());
    out.extend_from_slice(b"\r\nConnection: close\r\n\r\n");
    out.extend_from_slice(&body);
    h1.state = H1State::Writing { pos: 0 };
}

#[allow(clippy::too_many_arguments)] // transport, connection, state and the three dispatch closures
fn drive_h1<S, FH, FO, FR>(
    io: &mut H1Io<'_>,
    h1: &mut H1Conn,
    client_ip: &str,
    state: &mut S,
    on_head: &mut FH,
    on_owned: &mut FO,
    on_head_early: &mut FR,
    hit: Option<&HitHeaders>,
    drive: &mut ConnDrive,
) where
    FH: for<'a> FnMut(&mut S, &RequestHead<'a>, &'a [u8], &str) -> RequestOutcome,
    FO: FnMut(&mut S, RequestRef<'_>, &str) -> RequestOutcome,
    FR: for<'a> FnMut(&mut S, &RequestHead<'a>, &str) -> Option<RequestOutcome>,
{
    loop {
        match &h1.state {
            H1State::Reading => {
                let mut tmp = [0u8; 4096];
                // Read, parse and answer until the socket would block or the
                // request is dispatched elsewhere. The poll is edge-triggered,
                // so a turn that stops with unread bytes in the socket would
                // never be reported again: keep reading while the socket yields
                // data. `dry` says the socket has no more for this turn.
                let mut dry = false;
                match io.read_pumped(&mut h1.read_buf, &mut tmp) {
                    // Plaintext EOF: a peer that half-closed after sending
                    // is still owed its response. Parse what is buffered.
                    Ok(0) => {
                        if h1.read_buf.is_empty() {
                            h1.state = H1State::Done;
                            return;
                        }
                        dry = true;
                    }
                    Ok(n) => {
                        let limit = &crate::limits::edge().request;
                        // Past the maximum the request is refused before its
                        // bytes are kept: nothing grows the buffer past it.
                        if h1.read_buf.len() + n > limit.max() {
                            refuse_oversized_request(h1, h1.read_buf.len() + n);
                            continue;
                        }
                        let before = h1.read_buf.capacity();
                        h1.read_buf.extend_from_slice(&tmp[..n]);
                        crate::limits::log_growth(
                            limit,
                            before,
                            h1.read_buf.capacity(),
                            "a request body",
                        );
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        dry = true;
                    }
                    Err(_) => {
                        h1.state = H1State::Done;
                        return;
                    }
                }
                if h1.read_buf.len() > crate::limits::edge().request.max() {
                    refuse_oversized_request(h1, h1.read_buf.len());
                    continue;
                }
                // Parse the head under an immutable borrow of the input buffer.
                // A bodyless request routes on the borrowed head, so a cached
                // hit never materialises an owned request. A body request, or a
                // bodyless miss, falls back to the owned parser.
                //
                // This used to clone the buffer to release the mutable borrow
                // above. That copied the whole accumulated request on *every*
                // read event, so a body arriving in N chunks was copied O(N²)
                // bytes in total, at the 20 MiB cap tens of GB of memcpy for a
                // single upload.
                let mut head_buf = [httparse::EMPTY_HEADER; 64];
                let parsed = parse_head(&h1.read_buf, &mut head_buf);
                match parsed {
                    HeadResult::Incomplete => {
                        // A head that has already passed a limit is refused
                        // without waiting for the blank line that would
                        // complete it, so a peer cannot make the process buffer
                        // an unbounded head while it decides.
                        if let Some(why) = m6_core::h1::incomplete_head_status(&h1.read_buf) {
                            write_head_refusal(h1, why);
                            continue;
                        }
                        // The head may be complete even though the message is
                        // not, and a client that sent `Expect: 100-continue` is
                        // holding its body back until we answer. `handle_expect`
                        // answers it once and returns true when a final 417 was
                        // queued, so the loop switches to writing.
                        if handle_expect(h1, io) {
                            continue;
                        }
                        // More request bytes may already be buffered: read on.
                        if dry {
                            break;
                        }
                        continue;
                    }
                    // A malformed head keeps the answer it has always had.
                    HeadResult::Error(m6_core::h1::HeadError::Malformed) => {
                        start_response(h1, BAD_REQUEST);
                        continue;
                    }
                    HeadResult::Error(why) => {
                        write_head_refusal(h1, why);
                        continue;
                    }
                    HeadResult::Complete(head) => {
                        if head.has_body() {
                            // A request whose method the route does not answer
                            // is refused from the head, before this connection
                            // reads one byte of the body. The response says
                            // `Connection: close`, so the unread body is not
                            // taken for the next request on this connection.
                            if let Some(outcome) = on_head_early(state, &head, client_ip) {
                                let RequestOutcome::Ready(status, headers, body, _) = outcome
                                else {
                                    unreachable!("a head refusal is ready")
                                };
                                h1.keep_alive = false;
                                if make_out_room(
                                    &mut h1.out_buf,
                                    &mut h1.state,
                                    &mut h1.keep_alive,
                                    body.len(),
                                ) {
                                    h1.out_buf.clear();
                                    write_response(
                                        &mut h1.out_buf,
                                        status,
                                        &headers,
                                        &body,
                                        head.method,
                                        false,
                                    );
                                    h1.state = H1State::Writing { pos: 0 };
                                }
                                continue;
                            }
                            // A body request routes through the owned parser,
                            // which waits for the full body and carries it.
                            let owned = parse_request(&h1.read_buf);
                            match owned {
                                ParseResult::Incomplete => {
                                    // The head is complete and the body is not:
                                    // answer `Expect: 100-continue` here too, so
                                    // a client holding its body does not stall.
                                    if handle_expect(h1, io) {
                                        continue;
                                    }
                                    if dry {
                                        break;
                                    }
                                    continue;
                                }
                                ParseResult::Error(m6_core::h1::HeadError::Malformed) => {
                                    start_response(h1, BAD_REQUEST);
                                    continue;
                                }
                                ParseResult::Error(why) => {
                                    write_head_refusal(h1, why);
                                    continue;
                                }
                                ParseResult::Complete(mut req) => {
                                    // Strip proxy-owned headers before anything
                                    // reads them. See the note on the parser.
                                    //
                                    // This listener is public: it binds
                                    // `[server] bind`, and the only internal
                                    // listener is the h2c one, so a `Via` that
                                    // reaches here is the client's claim and is
                                    // dropped.
                                    crate::forward::strip_untrusted_inbound(
                                        &mut req.headers,
                                        crate::forward::ForwardedTrust::Never,
                                    );
                                    h1.keep_alive =
                                        wants_keep_alive(&req) && h1.served + 1 < h1.max_requests;
                                    match on_owned(state, RequestRef::Owned(&req), client_ip) {
                                        RequestOutcome::Ready(status, resp_headers, body, _) => {
                                            if make_out_room(
                                                &mut h1.out_buf,
                                                &mut h1.state,
                                                &mut h1.keep_alive,
                                                body.len(),
                                            ) {
                                                h1.out_buf.clear();
                                                write_response(
                                                    &mut h1.out_buf,
                                                    status,
                                                    &resp_headers,
                                                    &body,
                                                    &req.method,
                                                    h1.keep_alive,
                                                );
                                                h1.state = H1State::Writing { pos: 0 };
                                            }
                                            continue;
                                        }
                                        RequestOutcome::Pending { id, ctx } => {
                                            drive.pending.push(PendingDispatch {
                                                stream: None,
                                                id,
                                                ctx,
                                            });
                                            h1.state = H1State::WaitingBackend;
                                            break; // the upstream's completion resumes this connection
                                        }
                                        RequestOutcome::Cached {
                                            entry,
                                            age,
                                            set_cookie,
                                        } => {
                                            // A hit on a body-bearing request is
                                            // written from the entry's own
                                            // bytes, the same as the bodyless
                                            // fast path.
                                            let hit = hit.expect("cached hit carries hit headers");
                                            if make_out_room(
                                                &mut h1.out_buf,
                                                &mut h1.state,
                                                &mut h1.keep_alive,
                                                entry.body.len(),
                                            ) {
                                                h1.out_buf.clear();
                                                write_cached_h1(
                                                    &mut h1.out_buf,
                                                    &entry,
                                                    age,
                                                    hit,
                                                    set_cookie.as_deref(),
                                                    &req.method,
                                                    h1.keep_alive,
                                                );
                                                h1.state = H1State::Writing { pos: 0 };
                                            }
                                            continue;
                                        }
                                    }
                                }
                            }
                        } else {
                            // Bodyless fast path: route on the borrowed head.
                            let method = head.method;
                            let keep_alive = wants_keep_alive_head(head.version, head.headers)
                                && h1.served + 1 < h1.max_requests;
                            match on_head(state, &head, &h1.read_buf, client_ip) {
                                RequestOutcome::Cached {
                                    entry,
                                    age,
                                    set_cookie,
                                    ..
                                } => {
                                    let hit = hit.expect("cached hit carries hit headers");
                                    if make_out_room(
                                        &mut h1.out_buf,
                                        &mut h1.state,
                                        &mut h1.keep_alive,
                                        entry.body.len(),
                                    ) {
                                        h1.out_buf.clear();
                                        write_cached_h1(
                                            &mut h1.out_buf,
                                            &entry,
                                            age,
                                            hit,
                                            set_cookie.as_deref(),
                                            method,
                                            keep_alive,
                                        );
                                        h1.keep_alive = keep_alive;
                                        h1.state = H1State::Writing { pos: 0 };
                                    }
                                    continue;
                                }
                                RequestOutcome::Ready(status, resp_headers, body, _) => {
                                    if make_out_room(
                                        &mut h1.out_buf,
                                        &mut h1.state,
                                        &mut h1.keep_alive,
                                        body.len(),
                                    ) {
                                        h1.out_buf.clear();
                                        write_response(
                                            &mut h1.out_buf,
                                            status,
                                            &resp_headers,
                                            &body,
                                            method,
                                            keep_alive,
                                        );
                                        h1.keep_alive = keep_alive;
                                        h1.state = H1State::Writing { pos: 0 };
                                    }
                                    continue;
                                }
                                RequestOutcome::Pending { id, ctx } => {
                                    drive.pending.push(PendingDispatch {
                                        stream: None,
                                        id,
                                        ctx,
                                    });
                                    h1.keep_alive = keep_alive;
                                    h1.state = H1State::WaitingBackend;
                                    break; // the upstream's completion resumes this connection
                                }
                            }
                        }
                    }
                }
            }
            H1State::WaitingBackend => {
                // The upstream's own token completes this connection directly;
                // there is nothing to poll here.
                break;
            }
            H1State::Writing { pos } => {
                // Flush any ciphertext still pending from a previous turn: a
                // full socket stopped the flush but the response is not done.
                if io.wants_write() {
                    if io.flush_write().is_err() {
                        h1.state = H1State::Done;
                        return;
                    }
                    if io.wants_write() {
                        break; // still flushing; a writable event resumes
                    }
                }
                let remaining = &h1.out_buf[*pos..];
                if remaining.is_empty() {
                    if finish_response(h1) {
                        drive.restarted = true;
                    }
                    return;
                }
                match io.write(remaining) {
                    Ok(0) => {
                        h1.state = H1State::Done;
                        return;
                    }
                    Ok(w) => {
                        let H1State::Writing { pos, .. } = &mut h1.state else {
                            break;
                        };
                        *pos += w;
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                    Err(_) => {
                        h1.state = H1State::Done;
                        return;
                    }
                }
                if io.flush_write().is_err() {
                    h1.state = H1State::Done;
                    return;
                }
                if io.wants_write() {
                    break; // ciphertext still pending; writable resumes
                }
                let H1State::Writing { pos } = &h1.state else {
                    break;
                };
                if *pos >= h1.out_buf.len() {
                    if finish_response(h1) {
                        drive.restarted = true;
                    }
                    return;
                }
                break;
            }
            H1State::Done => return,
        }
    }
}

/// End of a response: either close, or go back to reading for the next one.
///
/// Returning to `Reading` with an empty buffer is what makes the connection
/// persistent. Returns whether it returned to `Reading`, so the loop restarts
/// the read timeout for the next request.
fn finish_response(h1: &mut H1Conn) -> bool {
    h1.served = h1.served.saturating_add(1);
    if h1.keep_alive && h1.served < h1.max_requests {
        // Cleared, not replaced: the next request on this connection reuses the
        // capacity this one grew, and the response buffer keeps its own.
        h1.read_buf.clear();
        h1.state = H1State::Reading;
        h1.created = m6_core::platform::now();
        h1.expect_handled = false;
        true
    } else {
        h1.state = H1State::Done;
        false
    }
}

/// Whether this request leaves the connection open (RFC 9112 9.3).
///
/// HTTP/1.1 is persistent by default and closes only if asked. HTTP/1.0 is the
/// reverse: it closes unless the client asked to keep it. Anything older, or a
/// version we do not recognise, closes.
fn wants_keep_alive(req: &HttpRequest) -> bool {
    wants_keep_alive_head(&req.version, &req.headers[..])
}

/// The borrowed form of [`wants_keep_alive`], for a parsed request head.
fn wants_keep_alive_head(
    version: &str,
    headers: &(impl m6_core::http::HeaderSource + ?Sized),
) -> bool {
    // EVERY `Connection` field line, not just the first. RFC 9110 5.3: repeated
    // field lines are equivalent to one comma-joined value, so a request
    // carrying `Connection: keep-alive` and `Connection: close` means
    // `keep-alive, close` and must close.
    //
    // Taking only the first got this backwards and kept the connection open,
    // which `test_hop_by_hop_stripped` caught immediately: it sends both, and
    // its client then waited for a close that never came.
    let has = |tok: &str| {
        // `find_all`: Connection may be sent as several field lines, and taking
        // only the first would miss a token in the second.
        headers
            .find_all("connection")
            .flat_map(|v| v.split(','))
            .any(|t| t.trim().eq_ignore_ascii_case(tok))
    };

    if version.eq_ignore_ascii_case("HTTP/1.1") {
        !has("close")
    } else if version.eq_ignore_ascii_case("HTTP/1.0") {
        has("keep-alive")
    } else {
        false
    }
}

/// Pump encrypted bytes between the socket and rustls.
///
/// `overflow`, when given, is the connection's request buffer. rustls caps its
/// incoming-plaintext buffer at a fixed 16 KiB (not application-configurable,
/// `set_buffer_limit` only covers the *outgoing* buffers) as backpressure:
/// `read_tls` refuses to pull more ciphertext until the caller drains the
/// decoded plaintext. The socket is edge-triggered, so it is reported once per
/// change: stopping here with ciphertext still in the kernel would leave it
/// unread forever. The overflow buffer is drained and reading continues in the
/// same turn.
///
/// Returns whether any ciphertext was read, so a caller can loop until the
/// socket would block.
/// Flush rustls's outgoing ciphertext to the socket until it is empty or the
/// socket would block. Shared by the read path (`advance_tls`) and the
/// write-only path (`H1Io::flush_write`).
fn drain_writes(tls: &mut ServerConnection, stream: &TcpStream) -> io::Result<()> {
    while tls.wants_write() {
        match tls.write_tls(&mut &*stream) {
            Ok(0) => break,
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

fn advance_tls(
    tls: &mut ServerConnection,
    stream: &TcpStream,
    mut overflow: Option<&mut Vec<u8>>,
) -> io::Result<bool> {
    let mut progress = false;
    loop {
        match tls.read_tls(&mut &*stream) {
            // A peer that has gone is the ordinary end of a connection, and it
            // is not worth an allocation. An error built from a kind is a plain
            // value, where `io::Error::new` with a message boxes one, so a
            // teardown inside a counting run would add a call whose number
            // moves with when the shutdown lands.
            Ok(0) => return Err(io::Error::from(io::ErrorKind::UnexpectedEof)),
            Ok(_) => {
                progress = true;
                tls.process_new_packets()
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
            Err(e)
                if e.kind() == io::ErrorKind::Other
                    && e.to_string().contains("received plaintext buffer full") =>
            {
                match &mut overflow {
                    Some(buf) => {
                        if drain_tls_plaintext(tls, buf)? == 0 {
                            break;
                        }
                    }
                    None => break,
                }
            }
            Err(e) => return Err(e),
        }
    }
    drain_writes(tls, stream)?;
    Ok(progress)
}

/// Drain rustls's decoded plaintext into `into`. Returns how many bytes moved.
/// Shared with the h2 engine so both paths read the reader the same way.
pub(crate) fn drain_tls_plaintext(
    tls: &mut ServerConnection,
    into: &mut Vec<u8>,
) -> io::Result<usize> {
    let mut tmp = [0u8; 4096];
    let mut drained = 0;
    loop {
        match tls.reader().read(&mut tmp) {
            Ok(0) => return Ok(drained),
            Ok(n) => {
                into.extend_from_slice(&tmp[..n]);
                drained += n;
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(drained),
            Err(e) => return Err(e),
        }
    }
}

// ── HTTP/1.1 request parser ───────────────────────────────────────────────────
// The HTTP/1.1 parser lives in `m6_core::h1`: one implementation, used by the
// edge, by m6-file, by m6-render and by m6-auth-server.
//
// This file carried a copy. It was the best of the four measured against
// h1spec (27/32 against 15, 15 and 14), which is why it is the one that
// survived -- but it survived by being *moved*, not duplicated, and leaving
// the original here would have kept two copies free to drift apart again.
pub use m6_core::h1::{parse_head, parse_request, HeadError, HeadResult, ParseResult, RequestHead};

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
/// and every protocol here signals end-of-message its own way (H1 uses the
/// next request boundary, H2/H3 use END_STREAM) -- so omitting it is both
/// correct and safe. Emitting the *correct* non-zero length would also be
/// legal, but it would mean carrying the stored body's length through the 304
/// path for no benefit to any client.
pub fn status_may_have_content_length(status: u16) -> bool {
    !(status == 204 || status == 304 || (100..200).contains(&status))
}

/// Write a completed HTTP/1.1 response into a new buffer.
///
/// The writer itself is the production one. The production request path does
/// not call this: it hands `write_response` (private, so named here in
/// backticks) the connection's own output buffer, which is why a response no
/// longer allocates its bytes.
pub fn build_response(
    status: u16,
    headers: &[(String, String)],
    body: &[u8],
    method: &str,
    keep_alive: bool,
) -> Vec<u8> {
    let is_head = method.eq_ignore_ascii_case("HEAD");
    let mut out = Vec::with_capacity(280 + if is_head { 0 } else { body.len() });
    write_response(&mut out, status, headers, body, method, keep_alive);
    out
}

/// Append one completed HTTP/1.1 response to `out`.
///
/// The writer appends instead of returning a buffer, so the connection's output
/// buffer is reused across responses on it and no response has to be built in a
/// temporary `Vec` and copied out again.
fn write_response(
    out: &mut Vec<u8>,
    status: u16,
    headers: &[(String, String)],
    body: &[u8],
    method: &str,
    keep_alive: bool,
) {
    let is_head = method.eq_ignore_ascii_case("HEAD");
    let reason = status_reason(status);
    // Every line below is appended from the slice it already is: the borrowed
    // name, the borrowed value, `itoa`'s formatting buffer. A `format!` here
    // built a `String` per line, which was fifteen of a miss's ninety-one
    // allocations and bought nothing the output buffer could not hold.
    out.extend_from_slice(b"HTTP/1.1 ");
    out.extend_from_slice(itoa::Buffer::new().format(status).as_bytes());
    out.extend_from_slice(b" ");
    out.extend_from_slice(reason.as_bytes());
    out.extend_from_slice(b"\r\n");
    // Any upstream copy of a header this function emits itself is dropped
    // here, or the response goes out carrying both.
    //
    // A raw-socket check of a HEAD response caught this: an asset answered
    // with `Content-Length: 16580` from the backend AND `content-length: 0`
    // from the line below. Two Content-Length fields with different values is
    // precisely the framing ambiguity this proxy now refuses to accept *from*
    // a backend (F028). It was emitting it. On HTML the two agreed, so it
    // looked like nothing more than a cosmetic duplicate. Only the asset
    // showed the conflict.
    for (k, v) in headers {
        if k.eq_ignore_ascii_case("content-length") || k.eq_ignore_ascii_case("connection") {
            continue;
        }
        out.extend_from_slice(k.as_bytes());
        out.extend_from_slice(b": ");
        out.extend_from_slice(v.as_bytes());
        out.extend_from_slice(b"\r\n");
    }
    // Applied at serialisation so every response carries them regardless of
    // which path produced it (cache hit, backend, error page, 429).
    crate::security::write_h1_headers(out, headers);

    // RFC 9110 9.3.2: a HEAD response's Content-Length describes the
    // representation a GET *would* have returned, not the zero bytes actually
    // sent. Most callers hand us the real body and `body.len()` is that
    // number -- but a backend that framed the HEAD itself returns an empty
    // body, and then only its own header still knows the real length.
    let cl = if is_head && body.is_empty() {
        m6_core::headers::get(headers, "content-length")
            .and_then(|v| v.trim().parse::<usize>().ok())
            .unwrap_or(0)
    } else {
        body.len()
    };
    if status_may_have_content_length(status) {
        out.extend_from_slice(b"content-length: ");
        out.extend_from_slice(itoa::Buffer::new().format(cl).as_bytes());
        out.extend_from_slice(b"\r\n");
    }
    // RFC 9112 6.3 rule 1: a 304 ends at the empty line whatever fields it
    // carries, and a 204 or 1xx likewise, so a bodyless response is
    // self-delimiting without Content-Length. Every other status got a
    // Content-Length above. No response written here can be unframed, so
    // none needs `close` for framing; announcing one makes every client drop
    // a usable connection after a 304.
    if keep_alive {
        out.extend_from_slice(b"connection: keep-alive\r\n\r\n");
    } else {
        out.extend_from_slice(b"connection: close\r\n\r\n");
    }
    if !is_head {
        out.extend_from_slice(body);
    }
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

// ── Cached-hit writer ─────────────────────────────────────────────────────────

/// The start-time header lines a cached hit writes straight into the output
/// buffer, held by `ServerState` as shared slices. Each value is formatted once
/// at start, so replaying a hit copies bytes, not strings. `Arc` so a drive can
/// hold them beside the mutable `ServerState` borrow for the connection table.
pub struct HitHeaders {
    /// The full `alt-svc` line, without the trailing CRLF.
    pub alt_svc: Arc<[u8]>,
    /// The full `link` line advertising the site description, when configured.
    pub describedby: Option<Arc<[u8]>>,
}

/// The merged `Vary` field list for a response, deduplicated case-insensitively
/// and with `Accept-Encoding` appended when absent. Allocation-free: the dedup
/// rescans the header list, so a response naming any number of vary dimensions
/// is handled. [`VaryMerge::is_star`] is true when a `Vary: *` is present, which
/// the caller must replay as-is instead of merging.
pub struct VaryMerge<'a> {
    headers: HeaderList<'a>,
    star: bool,
    next: usize,
    phase: u8,
    has_accept_encoding: bool,
}

/// The header list a `VaryMerge` scans, whichever shape the caller holds.
///
/// A response on its way out is a `HeaderBlock`; a response the cache is
/// deciding about is still an owned pair list, and the tests build one. The
/// merge itself is one implementation over both.
#[derive(Clone, Copy)]
pub enum HeaderList<'a> {
    Pairs(&'a [(String, String)]),
    Block(&'a crate::cache::HeaderBlock),
}

impl<'a> From<&'a [(String, String)]> for HeaderList<'a> {
    fn from(v: &'a [(String, String)]) -> Self {
        HeaderList::Pairs(v)
    }
}

impl<'a> From<&'a crate::cache::HeaderBlock> for HeaderList<'a> {
    fn from(v: &'a crate::cache::HeaderBlock) -> Self {
        HeaderList::Block(v)
    }
}

impl<'a> HeaderList<'a> {
    /// The header lines, whichever shape the caller holds.
    fn lines(self) -> HeaderLines<'a> {
        match self {
            HeaderList::Pairs(p) => HeaderLines::Pairs(p.iter()),
            HeaderList::Block(b) => HeaderLines::Block(b.iter()),
        }
    }

    fn vary_values(self) -> impl Iterator<Item = &'a str> {
        self.lines()
            .filter(|(k, _)| k.eq_ignore_ascii_case("vary"))
            .map(|(_, v)| v)
    }
}

impl<'a> VaryMerge<'a> {
    pub fn new(headers: impl Into<HeaderList<'a>>) -> Self {
        let headers = headers.into();
        let star = headers
            .vary_values()
            .any(|v| v.split(',').any(|f| f.trim() == "*"));
        VaryMerge {
            headers,
            star,
            next: 0,
            phase: 0,
            has_accept_encoding: false,
        }
    }

    /// Whether the response carries `Vary: *`, which is replayed unchanged.
    pub fn is_star(&self) -> bool {
        self.star
    }
}

impl<'a> Iterator for VaryMerge<'a> {
    type Item = &'a str;

    fn next(&mut self) -> Option<&'a str> {
        if self.star {
            return None;
        }
        loop {
            match self.phase {
                0 => match vary_field_at(self.headers, self.next) {
                    None => self.phase = 1,
                    Some(field) => {
                        self.next += 1;
                        if vary_earlier(self.headers, self.next - 1, field) {
                            continue;
                        }
                        if field.eq_ignore_ascii_case("accept-encoding") {
                            self.has_accept_encoding = true;
                        }
                        return Some(field);
                    }
                },
                1 => {
                    self.phase = 2;
                    return if self.has_accept_encoding {
                        None
                    } else {
                        Some("Accept-Encoding")
                    };
                }
                _ => return None,
            }
        }
    }
}

/// Header lines from either container, without boxing: the Vary merge runs on
/// the response path, where a boxed iterator was one allocation per field.
enum HeaderLines<'a> {
    Pairs(std::slice::Iter<'a, (String, String)>),
    Block(crate::cache::TapeIter<'a>),
}

impl<'a> Iterator for HeaderLines<'a> {
    type Item = (&'a str, &'a str);

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            HeaderLines::Pairs(it) => it.next().map(|(k, v)| (k.as_str(), v.as_str())),
            HeaderLines::Block(it) => it.next(),
        }
    }
}

/// The vary field at flattened index `idx`, or `None` past the last one.
fn vary_field_at(headers: HeaderList<'_>, idx: usize) -> Option<&str> {
    let mut i = 0usize;
    for v in headers.vary_values() {
        for f in v.split(',').map(str::trim).filter(|f| !f.is_empty()) {
            if i == idx {
                return Some(f);
            }
            i += 1;
        }
    }
    None
}

/// Whether an equal field appears before the vary field at flattened index `upto`.
fn vary_earlier(headers: HeaderList<'_>, upto: usize, field: &str) -> bool {
    let mut i = 0usize;
    for v in headers.vary_values() {
        for g in v.split(',').map(str::trim).filter(|f| !f.is_empty()) {
            if i == upto {
                return false;
            }
            if g.eq_ignore_ascii_case(field) {
                return true;
            }
            i += 1;
        }
    }
    false
}

/// Write a cached HTTP/1.1 response straight into `out`, allocating nothing.
///
/// This is the cached-hit half of [`build_response`]: the same bytes, but the
/// head is written into a reused output buffer and the body is borrowed from
/// the cache entry's `Bytes` rather than copied. The order matches
/// `build_response` after the hit path's `set_alt_svc`,
/// `set_vary_accept_encoding`, `set_age` and `set_describedby_link`.
pub fn write_cached_h1(
    out: &mut Vec<u8>,
    entry: &CachedResponse,
    age: std::time::Duration,
    extra: &HitHeaders,
    set_cookie: Option<&str>,
    method: &str,
    keep_alive: bool,
) {
    let is_head = method.eq_ignore_ascii_case("HEAD");
    let is_html = crate::analytics::is_html_response(&entry.headers);
    let write_describedby = extra.describedby.is_some() && is_html;

    // Vary is merged (or left alone on `Vary: *`) exactly as
    // `set_vary_accept_encoding(.., true)` does, so the two writers agree.
    let vary = VaryMerge::new(&entry.headers);
    let vary_star = vary.is_star();

    out.extend_from_slice(b"HTTP/1.1 ");
    out.extend_from_slice(itoa::Buffer::new().format(entry.status).as_bytes());
    out.extend_from_slice(b" ");
    out.extend_from_slice(status_reason(entry.status).as_bytes());
    out.extend_from_slice(b"\r\n");

    // Cached headers, except the ones this writer emits itself.
    for (k, v) in entry.headers.iter() {
        if k.eq_ignore_ascii_case("content-length")
            || k.eq_ignore_ascii_case("connection")
            || k.eq_ignore_ascii_case("alt-svc")
            || k.eq_ignore_ascii_case("age")
        {
            continue;
        }
        if !vary_star && k.eq_ignore_ascii_case("vary") {
            continue;
        }
        if write_describedby
            && k.eq_ignore_ascii_case("link")
            && v.as_bytes()
                .windows(b"rel=\"describedby\"".len())
                .any(|w| w.eq_ignore_ascii_case(b"rel=\"describedby\""))
        {
            continue;
        }
        out.extend_from_slice(k.as_bytes());
        out.extend_from_slice(b": ");
        out.extend_from_slice(v.as_bytes());
        out.extend_from_slice(b"\r\n");
    }

    out.extend_from_slice(&extra.alt_svc[..]);
    out.extend_from_slice(b"\r\n");

    if !vary_star {
        out.extend_from_slice(b"vary: ");
        for (i, f) in vary.enumerate() {
            if i > 0 {
                out.extend_from_slice(b", ");
            }
            out.extend_from_slice(f.as_bytes());
        }
        out.extend_from_slice(b"\r\n");
    }

    out.extend_from_slice(b"age: ");
    out.extend_from_slice(itoa::Buffer::new().format(age.as_secs()).as_bytes());
    out.extend_from_slice(b"\r\n");

    if let (true, Some(describedby)) = (write_describedby, extra.describedby.as_deref()) {
        out.extend_from_slice(describedby);
        out.extend_from_slice(b"\r\n");
    }

    if let Some(set_cookie) = set_cookie {
        out.extend_from_slice(b"set-cookie: ");
        out.extend_from_slice(set_cookie.as_bytes());
        out.extend_from_slice(b"\r\n");
    }

    crate::security::write_h1_headers(out, &entry.headers);

    if status_may_have_content_length(entry.status) {
        out.extend_from_slice(b"content-length: ");
        out.extend_from_slice(itoa::Buffer::new().format(entry.body.len()).as_bytes());
        out.extend_from_slice(b"\r\n");
    }

    if keep_alive {
        out.extend_from_slice(b"connection: keep-alive\r\n\r\n");
    } else {
        out.extend_from_slice(b"connection: close\r\n\r\n");
    }

    if !is_head {
        out.extend_from_slice(&entry.body);
    }
}

// ── TLS server config factory ─────────────────────────────────────────────────

/// A TLS server configuration and the certificate chain it was built from.
///
/// The chain is held so the expiry registry is written when a configuration is
/// installed, which at startup is the same step that builds it.
struct TlsMaterial {
    config: Arc<rustls::ServerConfig>,
    chain: Vec<rustls_pki_types::CertificateDer<'static>>,
}

impl TlsMaterial {
    /// Record this chain's expiry as what the server is now serving.
    ///
    /// Called once the configuration is installed, so the metrics document
    /// reports the material in use. A certificate whose `notAfter` cannot be
    /// read is logged and left out of the report, never a reason to refuse to
    /// serve: rustls accepted it or there would be no server at all, so the
    /// honest outcome is that the monitor says "cannot say" for that one rather
    /// than a number nobody took.
    fn record(&self, cert_path: &str) {
        for (depth, e) in m6_core::tls::record_loaded(&self.chain) {
            warn!(
                cert = cert_path, depth, error = %e,
                "could not read the certificate's expiry; it will not be reported"
            );
        }
    }
}

/// Load a certificate and key into a rustls server configuration.
///
/// Records nothing, so `make_tls_server_config` can build the material and
/// record it in one step.
fn load_tls_material(cert_path: &str, key_path: &str) -> anyhow::Result<TlsMaterial> {
    // `rustls_pki_types::pem`, not the `rustls-pemfile` crate. That crate is
    // unmaintained (RUSTSEC-2025-0134, archived August 2025) and its own
    // advisory points here: the last release of it was a thin wrapper around
    // this same code. There is no vulnerability to dodge, and no new dependency
    // either, because rustls already brings rustls-pki-types in.
    //
    // It also reads better: `pem_file_iter` and `from_pem_file` open the file
    // themselves, so the File and BufReader this used to build by hand are gone.
    use rustls_pki_types::pem::PemObject;
    use rustls_pki_types::{CertificateDer, PrivateKeyDer};

    let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_file_iter(cert_path)
        .map_err(|e| anyhow::anyhow!("open cert {}: {}", cert_path, e))?
        .collect::<Result<_, _>>()
        .map_err(|e| anyhow::anyhow!("parse cert: {}", e))?;

    // A file with no key in it is an error here rather than `Ok(None)`, which is
    // what `private_key` used to return and what the `ok_or_else` below it
    // existed to turn into a message.
    let key = PrivateKeyDer::from_pem_file(key_path)
        .map_err(|e| anyhow::anyhow!("parse key {}: {}", key_path, e))?;

    // The chain is kept alongside the configuration so its expiry can be
    // recorded once the configuration is installed, which is what the metrics
    // document must report: reading the file again at report time would answer
    // about the path, and a renewal that wrote a new certificate without a
    // restart is exactly the case where the path and the running server
    // disagree. Issue #176, and #210 for why the recording moved out of this
    // function.
    let chain = certs.clone();

    let mut config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| anyhow::anyhow!("tls config: {}", e))?;

    // Advertise h2 first so capable clients use HTTP/2; fall back to HTTP/1.1.
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];

    // ── Session resumption, which did not work at all before this ────────────
    //
    // rustls' defaults make a server that never resumes under real traffic:
    //
    //     ticketer:        NeverProducesTickets          (builder.rs:116)
    //     session_storage: ServerSessionMemoryCache(256) (builder.rs:113)
    //
    // With no ticketer, resumption is STATEFUL: rustls stores the session
    // server-side and hands the client a handle. Every returning client needs
    // its own entry still to be in a 256-entry cache. A busy node takes hundreds
    // of handshakes an hour, so entries are evicted long before anyone comes
    // back, and the cache does nothing but cost memory.
    //
    // The two traffic patterns then disagree, and correctly. A monitoring client
    // that polls on repeat keeps its few entries resident and resumes. Browsers
    // are many distinct clients against 256 slots, so they are evicted before
    // any of them returns. The channel that resumes is the one that does not
    // need to.
    //
    // NOT established by `openssl s_client -sess_out/-sess_in`, which was tried
    // first and reported a full handshake every time. That evidence was
    // worthless: macOS ships LibreSSL 3.3.6 as /usr/bin/openssl, whose TLS 1.3
    // client resumption is incomplete, so it reports "New" whatever the server
    // does. The test beside this (tests/tls_resumption.rs) uses rustls on both
    // ends for that reason.
    //
    // A Ticketer makes resumption STATELESS. The session state is encrypted into
    // the ticket with ChaCha20Poly1305 under a key the server keeps, so there is
    // no per-client server storage to evict and the cache size stops mattering.
    // Keys are generated randomly and rotate on a 12 hour life, so a ticket
    // cannot outlive its key, and a restart invalidates outstanding tickets:
    // clients then do one full handshake and resume from there.
    //
    // Cost of not having it: a full handshake against a resumed one is three
    // orders of magnitude of difference once a real network path is involved,
    // plus two extra round trips on every new browser connection.
    //
    // The provider's ticketer, matching the one provider this crate builds
    // rustls with (m6-http/Cargo.toml: features = ["aws_lc_rs", "std"]). Its
    // design is the same: random keys on a 12 hour life. This is the provider
    // substitution and nothing else: the ticketer is still installed, its
    // lifetime is unchanged, and `session_storage` below is still the default.
    config.ticketer = rustls::crypto::aws_lc_rs::Ticketer::new()
        .map_err(|e| anyhow::anyhow!("tls ticketer: {}", e))?;

    // `session_storage` is deliberately LEFT at the default 256.
    //
    // Raising it was the first instinct and it is the wrong change. With the
    // ticketer above, TLS 1.3 resumption is stateless and never reads this cache
    // at all; the only thing still using it is a TLS 1.2 client resuming by
    // session id, which in practice means old scanners rather than visitors.
    //
    // `ServerSessionMemoryCache::new(n)` is `HashMap::with_capacity(n)` plus
    // `VecDeque::with_capacity(n)` (rustls limited_cache.rs:63), so it allocates
    // up front. 4096 entries is roughly 350KB per config bought for a path that
    // does not carry anyone who matters, on a small origin.

    Ok(TlsMaterial {
        config: Arc::new(config),
        chain,
    })
}

/// Load a certificate and key, and record what was loaded.
///
/// The startup path and the tests, where there is one listener and building it
/// is installing it.
pub fn make_tls_server_config(
    cert_path: &str,
    key_path: &str,
) -> anyhow::Result<Arc<rustls::ServerConfig>> {
    let material = load_tls_material(cert_path, key_path)?;
    material.record(cert_path);
    Ok(material.config)
}

#[cfg(test)]
mod head_framing_tests {
    use super::*;

    fn hdrs() -> Vec<(String, String)> {
        vec![(
            "content-type".to_string(),
            "text/html; charset=utf-8".to_string(),
        )]
    }

    fn split(raw: &[u8]) -> (String, &[u8]) {
        let i = raw
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .expect("header terminator");
        (
            String::from_utf8_lossy(&raw[..i]).to_string(),
            &raw[i + 4..],
        )
    }

    const BODY: &[u8] = b"<!doctype html><html><body>hello</body></html>";

    #[test]
    fn get_sends_the_body() {
        let (head, body) = {
            let raw = build_response(200, &hdrs(), BODY, "GET", false);
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
        let raw = build_response(200, &hdrs(), BODY, "HEAD", false);
        let (_, body) = split(&raw);
        assert_eq!(
            body.len(),
            0,
            "HEAD must send no body, got {} bytes",
            body.len()
        );
    }

    /// RFC 9110 9.3.2: the headers are those a GET would have sent, so
    /// Content-Length still describes the GET representation. Deriving it from
    /// the emptied body instead would advertise 0 and make HEAD useless for
    /// the size checks that are most of the reason to send one.
    #[test]
    fn head_still_advertises_the_get_length() {
        let raw = build_response(200, &hdrs(), BODY, "HEAD", false);
        let (head, _) = split(&raw);
        assert!(
            head.contains(&format!("content-length: {}", BODY.len())),
            "expected content-length {}, headers were:\n{head}",
            BODY.len()
        );
    }

    /// A HEAD response must otherwise be indistinguishable from the GET's
    /// header block — same status, same content-type, same everything.
    #[test]
    fn head_and_get_headers_match() {
        let (gh, _) = {
            let r = build_response(200, &hdrs(), BODY, "GET", false);
            let (h, _) = split(&r);
            (h, ())
        };
        let (hh, _) = {
            let r = build_response(200, &hdrs(), BODY, "HEAD", false);
            let (h, _) = split(&r);
            (h, ())
        };
        assert_eq!(gh, hh, "HEAD headers differ from GET headers");
    }

    #[test]
    fn method_match_is_case_insensitive() {
        let raw = build_response(200, &hdrs(), BODY, "head", false);
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
            let raw200 = build_response(200, &hdrs(), b"", m, false);
            let (head, body) = split(&raw200);
            assert_eq!(body.len(), 0);
            assert!(
                head.contains("content-length: 0"),
                "200 must state its length:\n{head}"
            );

            let raw204 = build_response(204, &hdrs(), b"", m, false);
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
        assert!(h
            .iter()
            .any(|(k, v)| k.eq_ignore_ascii_case("x-one") && v == "1"));
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
            .filter(|l| {
                l.to_lowercase()
                    .starts_with(&format!("{}:", name.to_lowercase()))
            })
            .map(|l| l.trim().to_string())
            .collect()
    }

    /// An asset HEAD went out with `Content-Length: 16580` from the backend and
    /// `content-length: 0` from the serialiser. Two Content-Length fields with
    /// different values is the same framing ambiguity this proxy refuses to
    /// accept from a backend (F028). It was producing it.
    #[test]
    fn exactly_one_content_length_even_when_upstream_sent_one() {
        let upstream = vec![
            ("Content-Length".to_string(), "16580".to_string()),
            ("Content-Type".to_string(), "image/svg+xml".to_string()),
        ];
        for method in ["GET", "HEAD"] {
            let raw = build_response(200, &upstream, b"", method, false);
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
        let raw = build_response(200, &upstream, b"", "HEAD", false);
        assert_eq!(
            header_lines(&raw, "content-length"),
            vec!["content-length: 16580"]
        );
        // ...and still no body.
        let body = String::from_utf8_lossy(&raw)
            .split("\r\n\r\n")
            .nth(1)
            .unwrap_or("")
            .len();
        assert_eq!(body, 0, "HEAD must send no body");
    }

    /// When the caller passes the real body, that is authoritative -- it may
    /// have been compressed after the backend set its own length.
    #[test]
    fn body_length_wins_when_a_body_is_present() {
        let upstream = vec![("Content-Length".to_string(), "99999".to_string())];
        let raw = build_response(200, &upstream, b"hello", "GET", false);
        assert_eq!(
            header_lines(&raw, "content-length"),
            vec!["content-length: 5"]
        );
    }

    /// Same duplication hazard: `connection: close` is written unconditionally.
    #[test]
    fn exactly_one_connection_header() {
        let upstream = vec![("Connection".to_string(), "keep-alive".to_string())];
        let raw = build_response(200, &upstream, b"x", "GET", false);
        assert_eq!(header_lines(&raw, "connection").len(), 1);
    }

    /// Ordinary headers must still be forwarded; the filter is narrow.
    #[test]
    fn other_headers_are_preserved() {
        let upstream = vec![
            ("Content-Type".to_string(), "text/html".to_string()),
            ("ETag".to_string(), "\"abc\"".to_string()),
        ];
        let raw = build_response(200, &upstream, b"x", "GET", false);
        assert_eq!(header_lines(&raw, "content-type").len(), 1);
        assert_eq!(header_lines(&raw, "etag").len(), 1);
    }
}

#[cfg(test)]
mod bodyless_status_tests {
    use super::{build_response, status_may_have_content_length};

    fn head_of(status: u16) -> String {
        let raw = build_response(
            status,
            &[("ETag".to_string(), "\"x\"".to_string())],
            b"",
            "GET",
            false,
        );
        String::from_utf8_lossy(&raw)
            .split("\r\n\r\n")
            .next()
            .unwrap_or("")
            .to_string()
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

    /// RFC 9112 6.3 rule 1: a 304 ends at the empty line whatever its fields
    /// say, so it is self-delimiting and has no framing reason to close. m6
    /// advertised `connection: close` on every 304, so a client had to
    /// reconnect for the next request even though the server stayed open.
    #[test]
    fn a_304_does_not_advertise_a_close() {
        let raw = build_response(
            304,
            &[("ETag".to_string(), "\"x\"".to_string())],
            b"",
            "GET",
            true,
        );
        let head = String::from_utf8_lossy(&raw);
        let head = head.split("\r\n\r\n").next().unwrap_or("").to_string();
        assert!(
            head.contains("connection: keep-alive"),
            "a 304 must keep the connection:\n{head}"
        );
        assert!(
            !head.contains("connection: close"),
            "a 304 must not advertise a close:\n{head}"
        );
    }
}

#[cfg(test)]
mod keep_alive_tests {
    use super::*;

    fn req(version: &str, conn_headers: &[&str]) -> HttpRequest {
        HttpRequest {
            method: "GET".into(),
            path: "/".into(),
            query: None,
            version: version.into(),
            headers: conn_headers
                .iter()
                .map(|v| ("Connection".to_string(), v.to_string()))
                .collect(),
            body: Vec::new(),
        }
    }

    /// RFC 9112 9.3: HTTP/1.1 persists by default, HTTP/1.0 does not.
    #[test]
    fn version_sets_the_default() {
        assert!(wants_keep_alive(&req("HTTP/1.1", &[])));
        assert!(!wants_keep_alive(&req("HTTP/1.0", &[])));
        // Anything we do not recognise closes.
        assert!(!wants_keep_alive(&req("HTTP/0.9", &[])));
    }

    #[test]
    fn the_connection_header_overrides_the_default() {
        assert!(!wants_keep_alive(&req("HTTP/1.1", &["close"])));
        assert!(wants_keep_alive(&req("HTTP/1.0", &["keep-alive"])));
    }

    /// RFC 9110 5.3: repeated field lines are one comma-joined value.
    ///
    /// Reading only the first `Connection` line answered "keep alive" to a
    /// request that also said `close`. `test_hop_by_hop_stripped` sends both,
    /// and its client then waited for a close that never came.
    #[test]
    fn every_connection_line_counts_not_just_the_first() {
        assert!(!wants_keep_alive(&req(
            "HTTP/1.1",
            &["keep-alive", "close"]
        )));
        assert!(!wants_keep_alive(&req(
            "HTTP/1.1",
            &["close", "keep-alive"]
        )));
        assert!(!wants_keep_alive(&req("HTTP/1.1", &["keep-alive, close"])));
    }

    /// Tokens, not substrings: a value that merely contains the letters must
    /// not match.
    #[test]
    fn matching_is_by_token() {
        assert!(wants_keep_alive(&req("HTTP/1.1", &["x-not-close"])));
        assert!(!wants_keep_alive(&req("HTTP/1.0", &["keep-alive-ish"])));
        // Case-insensitive, per RFC 9110 7.6.1.
        assert!(!wants_keep_alive(&req("HTTP/1.1", &["CLOSE"])));
        assert!(wants_keep_alive(&req("HTTP/1.0", &["Keep-Alive"])));
    }
}

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "test code: a blocking TcpStream::connect drives the connection from outside"
)]
mod expect_tests {
    use super::*;

    fn h1_with_head(buf: Vec<u8>) -> H1Conn {
        H1Conn {
            state: H1State::Reading,
            read_buf: buf,
            out_buf: Vec::new(),
            created: m6_core::platform::now(),
            keep_alive: false,
            served: 0,
            max_requests: 100,
            expect_handled: false,
        }
    }

    /// RFC 9110 10.1.1: a client that sends `Expect: 100-continue` holds its body
    /// back until the server answers. The answer must fire on the body-incomplete
    /// read too, not only while the head is still arriving.
    #[test]
    fn expect_100_continue_is_answered_before_the_body_arrives() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let mut client = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (server, _) = listener.accept().unwrap();
        server.set_nonblocking(true).unwrap();

        let mut h1 = h1_with_head(
            b"POST / HTTP/1.1\r\nHost: x\r\nContent-Length: 5\r\nExpect: 100-continue\r\n\r\n"
                .to_vec(),
        );
        let mut io = H1Io::Plain { stream: &server };
        assert!(!handle_expect(&mut h1, &mut io));
        let mut got = [0u8; 64];
        let n = client.read(&mut got).unwrap();
        assert_eq!(&got[..n], m6_core::h1::CONTINUE_RESPONSE);
    }

    /// An unsupported expectation answers a final 417 and marks it handled, so a
    /// later read does not answer twice.
    #[test]
    fn an_unsupported_expectation_queues_a_417() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let _client = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (server, _) = listener.accept().unwrap();
        server.set_nonblocking(true).unwrap();

        let mut h1 = h1_with_head(
            b"POST / HTTP/1.1\r\nHost: x\r\nContent-Length: 5\r\nExpect: bogus\r\n\r\n".to_vec(),
        );
        let mut io = H1Io::Plain { stream: &server };
        assert!(handle_expect(&mut h1, &mut io));
        assert!(h1.expect_handled);
        assert!(matches!(h1.state, H1State::Writing { .. }));
    }
}

#[cfg(test)]
mod vary_merge_tests {
    use super::VaryMerge;

    fn hdrs(vary: &[&str]) -> Vec<(String, String)> {
        vary.iter()
            .map(|v| ("Vary".to_string(), v.to_string()))
            .collect()
    }

    fn merged(vary: &[&str]) -> Vec<String> {
        VaryMerge::new(&hdrs(vary)[..])
            .map(str::to_string)
            .collect()
    }

    /// The merge deduplicates case-insensitively, keeps field order, and appends
    /// `Accept-Encoding` when absent.
    #[test]
    fn deduplicates_and_appends_accept_encoding() {
        assert_eq!(
            merged(&["Accept-Encoding, Cookie", "cookie, User-Agent"]),
            vec!["Accept-Encoding", "Cookie", "User-Agent"]
        );
        assert_eq!(merged(&[]), vec!["Accept-Encoding"]);
    }

    /// More than eight vary dimensions must all survive: the writer used to cap
    /// the list at 8 and silently drop the rest, which would let a downstream
    /// shared cache share a response it should not.
    #[test]
    fn more_than_eight_fields_survive() {
        let fields = [
            "A",
            "B",
            "C",
            "D",
            "E",
            "F",
            "G",
            "H",
            "Cookie",
            "User-Agent",
        ];
        let mut want: Vec<String> = fields.iter().map(|s| s.to_string()).collect();
        want.push("Accept-Encoding".to_string());
        assert_eq!(merged(&[&fields.join(", ")]), want);
    }

    /// `Vary: *` means unpredictable and must be replayed as-is, never narrowed.
    #[test]
    fn star_is_replayed_as_is() {
        let h = hdrs(&["*", "Accept-Encoding"]);
        let merge = VaryMerge::new(&h[..]);
        assert!(merge.is_star());
        assert_eq!(merge.collect::<Vec<_>>(), Vec::<&str>::new());
    }
}

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "test code: threads, sleeps and blocking clients drive the service from outside"
)]
mod h2c_admit_tests {
    use super::{
        admit, H2cListener, RefusalLog, RefusalSummary, RefusalVerdict, REFUSAL_LOG_MAX_LINES,
        REFUSAL_LOG_MAX_PEERS,
    };
    use crate::forward::ForwardedTrust;
    use std::net::{IpAddr, TcpStream};

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    /// A listener that is not named in `[server] internal_binds` is just
    /// another listener: it serves everyone and believes no forwarded address.
    #[test]
    fn a_listener_that_is_not_internal_serves_everyone_and_trusts_no_one() {
        assert_eq!(admit(None, ip("192.0.2.1")), Some(ForwardedTrust::Never));
    }

    #[test]
    fn a_listed_peer_is_admitted_and_trusted() {
        let peers = [ip("10.0.0.2")];
        assert_eq!(
            admit(Some(&peers), ip("10.0.0.2")),
            Some(ForwardedTrust::Backbone)
        );
    }

    #[test]
    fn an_unlisted_peer_is_refused() {
        let peers = [ip("10.0.0.2")];
        assert_eq!(admit(Some(&peers), ip("10.0.0.3")), None);
    }

    #[test]
    fn a_v4_mapped_peer_matches_its_v4_entry() {
        let peers = [ip("10.0.0.2")];
        assert_eq!(
            admit(Some(&peers), ip("::ffff:10.0.0.2")),
            Some(ForwardedTrust::Backbone)
        );
    }

    const SEC: std::time::Duration = std::time::Duration::from_secs(1);

    fn v4(n: u32) -> IpAddr {
        IpAddr::from(std::net::Ipv4Addr::from(n))
    }

    #[test]
    fn the_first_refusal_of_a_peer_is_logged_and_repeats_are_counted() {
        let t0 = m6_core::platform::now();
        let mut log = RefusalLog::new(60 * SEC, 8);
        let a = ip("10.0.0.3");
        assert_eq!(log.record(a, t0).peer_line, Some(0));
        assert_eq!(log.record(a, t0 + SEC).peer_line, None);
        assert_eq!(log.record(a, t0 + 59 * SEC).peer_line, None);
        // Another peer has its own first line.
        assert_eq!(log.record(ip("10.0.0.4"), t0).peer_line, Some(0));
    }

    #[test]
    fn a_line_after_the_interval_carries_the_suppressed_count() {
        let t0 = m6_core::platform::now();
        let mut log = RefusalLog::new(60 * SEC, 8);
        let a = ip("10.0.0.3");
        assert_eq!(log.record(a, t0).peer_line, Some(0));
        for s in 1..=5 {
            assert_eq!(log.record(a, t0 + s * SEC).peer_line, None);
        }
        let later = t0 + 61 * SEC;
        assert_eq!(log.record(a, later).peer_line, Some(5));
        // The count starts again from the line just written.
        assert_eq!(log.record(a, later + SEC).peer_line, None);
        assert_eq!(log.record(a, later + 61 * SEC).peer_line, Some(1));
    }

    #[test]
    fn a_full_table_keeps_its_peers_and_counts_new_addresses() {
        let t0 = m6_core::platform::now();
        let mut log = RefusalLog::new(60 * SEC, 3);
        for n in 1..=3 {
            assert_eq!(log.record(v4(n), t0).peer_line, Some(0));
        }
        // Neither a new address nor a repeat of it gets a line, and nothing is evicted.
        assert_eq!(log.record(v4(9), t0 + SEC), RefusalVerdict::default());
        assert_eq!(log.record(v4(9), t0 + 2 * SEC), RefusalVerdict::default());
        assert_eq!(log.record(v4(10), t0 + 3 * SEC), RefusalVerdict::default());
        assert_eq!(log.peers.len(), 3);
        assert!(log.peers.contains_key(&v4(1)));
        // A tracked address still counts its repeats.
        assert_eq!(log.record(v4(1), t0 + 4 * SEC).peer_line, None);
        // The first refusal after the window carries the summary and its own line.
        let v = log.record(v4(1), t0 + 61 * SEC);
        assert_eq!(v.peer_line, Some(1));
        assert_eq!(
            v.summary,
            Some(RefusalSummary {
                other_refusals: 3,
                other_addresses: 2,
                sample: Some(v4(10)),
                over_limit: 0
            })
        );
        // The summary is reported once.
        assert_eq!(log.record(v4(1), t0 + 62 * SEC).summary, None);
    }

    #[test]
    fn rotating_addresses_cannot_defeat_the_line_limit() {
        let t0 = m6_core::platform::now();
        let mut log = RefusalLog::new(60 * SEC, REFUSAL_LOG_MAX_PEERS);
        let mut lines = 0;
        let mut summaries = 0;
        for n in 0..300 {
            let v = log.record(v4(n), t0 + SEC);
            lines += usize::from(v.peer_line.is_some());
            summaries += usize::from(v.summary.is_some());
        }
        // 256 tracked addresses fill the table, 32 get a line and 224 wait for
        // one, and 44 are untracked.
        assert_eq!(lines, REFUSAL_LOG_MAX_LINES as usize);
        assert_eq!(summaries, 0);
        assert!(log.peers.len() <= REFUSAL_LOG_MAX_PEERS);
        let v = log.record(v4(0), t0 + 61 * SEC);
        assert_eq!(
            v.summary,
            Some(RefusalSummary {
                other_refusals: 44,
                other_addresses: 44,
                sample: Some(v4(299)),
                over_limit: 300 - 44 - 32
            })
        );
        assert_eq!(v.peer_line, Some(0));
    }

    #[test]
    fn a_withheld_peer_gets_its_line_in_the_next_window() {
        let t0 = m6_core::platform::now();
        let mut log = RefusalLog::new(60 * SEC, 256);
        for n in 0..40 {
            log.record(v4(n), t0);
        }
        // Address 39 was over the limit: its first line comes with the count.
        let v = log.record(v4(39), t0 + 61 * SEC);
        assert_eq!(v.peer_line, Some(1));
        assert_eq!(v.summary.map(|s| s.over_limit), Some(8));
    }

    #[test]
    fn a_new_address_is_named_once_stale_entries_free_a_slot() {
        let t0 = m6_core::platform::now();
        let mut log = RefusalLog::new(60 * SEC, 3);
        for n in 1..=3 {
            assert_eq!(log.record(v4(n), t0 + n * SEC).peer_line, Some(0));
        }
        // Every entry's line is older than the interval and none is pending.
        let later = t0 + 100 * SEC;
        let v = log.record(v4(9), later);
        assert_eq!(v.peer_line, Some(0));
        assert_eq!(log.peers.len(), 3);
        // The stalest entry, v4(1), made room.
        assert!(!log.peers.contains_key(&v4(1)));
        assert!(log.peers.contains_key(&v4(2)) && log.peers.contains_key(&v4(3)));
    }

    #[test]
    fn with_no_stale_entry_a_new_address_is_counted_and_sampled() {
        let t0 = m6_core::platform::now();
        let mut log = RefusalLog::new(60 * SEC, 3);
        for n in 1..=3 {
            log.record(v4(n), t0);
        }
        // v4(1) has a count pending, so it is not stale even though its line is old.
        log.record(v4(1), t0 + 10 * SEC);
        let later = t0 + 61 * SEC;
        // v4(2) and v4(3) are stale, so one of them makes room.
        assert_eq!(log.record(v4(8), later).peer_line, Some(0));
        assert!(log.peers.contains_key(&v4(1)));
        // With every entry recent or pending, nothing makes room.
        let mut log = RefusalLog::new(60 * SEC, 3);
        for n in 1..=3 {
            log.record(v4(n), t0);
        }
        for n in 1..=3 {
            log.record(v4(n), t0 + 10 * SEC);
        }
        let v = log.record(v4(8), t0 + 61 * SEC);
        assert_eq!(v.peer_line, None);
        assert_eq!(log.peers.len(), 3);
        assert!(log.peers.contains_key(&v4(1)));
        let v = log.record(v4(9), t0 + 62 * SEC);
        assert_eq!(v.peer_line, None);
        assert_eq!(log.summary.sample, Some(v4(9)));
        assert_eq!(log.summary.other_addresses, 2);
    }

    /// With more connections pending than the cap, one call returns after the
    /// cap and the rest wait for the next call.
    #[test]
    fn accept_returns_after_the_cap_with_more_pending() {
        // 127.0.0.1 is not listed, so every connection is refused.
        let mut l = H2cListener::bind("127.0.0.1:0", Some(vec![ip("10.9.9.9")]), 8, 8).unwrap();
        let addr = l.local_addr().unwrap();
        let clients: Vec<TcpStream> = (0..12).map(|_| TcpStream::connect(addr).unwrap()).collect();
        assert_eq!(l.accept_up_to(5, |_, _| true), 5);
        assert_eq!(l.accept_up_to(5, |_, _| true), 5);
        assert_eq!(l.accept_up_to(5, |_, _| true), 2);
        assert_eq!(l.accept_up_to(5, |_, _| true), 0);
        drop(clients);
    }
}

#[cfg(test)]
mod out_room_tests {
    use super::*;

    /// A request past the `edge_request` maximum is answered 413 and closed.
    #[test]
    fn a_request_past_the_maximum_is_answered_413() {
        let mut h1 = H1Conn {
            state: H1State::Reading,
            read_buf: Vec::new(),
            out_buf: Vec::new(),
            created: m6_core::platform::now(),
            keep_alive: true,
            served: 0,
            max_requests: 100,
            expect_handled: false,
        };
        refuse_oversized_request(&mut h1, crate::limits::edge().request.max() + 1);
        assert!(h1.out_buf.starts_with(b"HTTP/1.1 413 "));
        assert!(matches!(h1.state, H1State::Writing { pos: 0 }));
    }

    /// A response body past the `edge_response` maximum is refused with a 500,
    /// and one that does not fit the buffer grows it by doubling.
    #[test]
    fn make_out_room_refuses_past_the_maximum_and_grows_below_it() {
        let limit = &crate::limits::edge().response;
        let mut out: Vec<u8> = Vec::with_capacity(limit.start());
        let mut state = H1State::Reading;
        let mut keep_alive = true;

        // A 100 KiB body grows a 512-byte buffer to the next power of two.
        assert!(make_out_room(
            &mut out,
            &mut state,
            &mut keep_alive,
            100 * 1024
        ));
        assert!(out.capacity() >= 128 * 1024, "{}", out.capacity());
        assert!(keep_alive);

        // A body past the maximum starts a 500 and closes.
        let too_big = limit.max();
        assert!(!make_out_room(
            &mut out,
            &mut state,
            &mut keep_alive,
            too_big
        ));
        assert!(out.starts_with(b"HTTP/1.1 500 "));
        assert!(!keep_alive);
        assert!(matches!(state, H1State::Writing { pos: 0 }));
    }
}
