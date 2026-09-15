//! probe -- minimal clients that measure ONE thing: how long a handshake takes.
//!
//! ## Why these exist
//!
//! m6-http publishes a per-channel handshake figure on `/perf`. A figure the
//! server reports about itself has to be checkable against something, and on
//! 2026-09-15 the only thing reaching for was `h3spec` -- a CONFORMANCE tester,
//! which deliberately opens stalled and malformed connections. It reported an h3
//! handshake p50 of 113ms on loopback for an engine that answers requests in
//! microseconds, and that number was believed long enough to be written down.
//!
//! We are writing a complete HTTP engine. The client halves of all three
//! protocols are already in this crate. So the thing that checks the server's
//! figure should be our own client, doing the one thing being measured.
//!
//! ## Why separate from m6-bench-detail
//!
//! `m6-bench-detail` measures the whole request lifecycle for four protocols,
//! renders charts, pre-warms caches, and reports connection phases from only its
//! first post-warmup connection. That last detail matters: a run that opens 81
//! connections reports `n=1` for the handshake, so it cannot produce a
//! distribution, and its full-page section opens connections CONCURRENTLY, which
//! puts accept-queue delay inside any figure drawn from them.
//!
//! These probes are strictly sequential, one handshake per connection, nothing
//! else on the wire. That is what makes them comparable to the server's own
//! number: if the probe and `/perf` disagree, the difference is the server's
//! queueing rather than a difference in what was being timed.
//!
//! ## What is and is not in the number
//!
//! The TCP connect is EXCLUDED, because the server's h1/h2 figure excludes it
//! too: rustls only sees the socket after the three-way handshake is done. The
//! QUIC figure INCLUDES its equivalent round trip, because QUIC folds transport
//! and crypto together and there is no earlier point to start from. The two are
//! therefore not the same measurement and must never be averaged together.

use std::io;
use std::net::{TcpStream, UdpSocket};
use std::sync::Arc;
use std::time::{Duration, Instant};

use quiche::h3::NameValue as _;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::ClientConfig;

/// Accepts any certificate.
///
/// These probes are pointed at loopback and at staging, whose certificate names
/// an IP address. Verification is not what is being measured, and refusing to
/// run without a valid chain would mean the tool could not check the box it most
/// needs to check.
#[derive(Debug)]
struct NoVerify;

impl rustls::client::danger::ServerCertVerifier for NoVerify {
    fn verify_server_cert(
        &self,
        _: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        _: &[u8],
        _: &CertificateDer<'_>,
        _: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }
    fn verify_tls13_signature(
        &self,
        _: &[u8],
        _: &CertificateDer<'_>,
        _: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }
    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// Install the process-level crypto provider.
///
/// m6-http builds rustls with `default-features = false`, so there is no
/// automatic provider and the first `ClientConfig::builder()` panics without
/// this. `m6-bench-detail` was missing this call and panicked before measuring
/// anything, which is how a conformance tester came to be used instead.
pub fn install_crypto() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// A client config offering exactly one ALPN protocol.
///
/// Exactly one, because m6-http attributes a connection to a channel BY its
/// negotiated ALPN. A probe that offers both `h2` and `http/1.1` measures
/// whichever the server prefers, which is not the same as measuring the one you
/// asked for, and it stops being the same silently.
pub fn tls_config(alpn: &[u8]) -> Arc<ClientConfig> {
    let mut cfg = ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(NoVerify))
        .with_no_client_auth();
    cfg.alpn_protocols = vec![alpn.to_vec()];
    Arc::new(cfg)
}

/// One TLS handshake, timed. Returns the duration and the negotiated ALPN.
///
/// The ALPN comes back so the caller can ASSERT it rather than assume it. A
/// probe that asked for `h2`, silently got `http/1.1`, and reported the figure
/// under an "h2" heading would be the same class of error as trusting h3spec.
pub fn tls_handshake(
    addr: &str,
    cfg: Arc<ClientConfig>,
) -> io::Result<(Duration, Option<Vec<u8>>)> {
    // TCP connect happens BEFORE the clock starts, matching what the server's
    // h1/h2 figure excludes.
    let (host, peer) = resolve(addr)?;
    let mut sock = TcpStream::connect(peer)?;
    sock.set_nodelay(true)?;

    // The real hostname, not a placeholder: it goes out as SNI, and a server
    // handed the wrong one either refuses or answers with a different
    // certificate, which silently changes what is being measured.
    let name =
        ServerName::try_from(host).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    let mut conn = rustls::ClientConnection::new(cfg, name)
        .map_err(|e| io::Error::other(format!("ClientConnection::new: {e}")))?;

    let t0 = Instant::now();
    let deadline = t0 + Duration::from_secs(5);
    while conn.is_handshaking() {
        if Instant::now() > deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "TLS handshake timeout",
            ));
        }
        // Write before read. rustls buffers the ClientHello at construction, so
        // reading first would block waiting for a reply to something not sent.
        if conn.wants_write() {
            conn.write_tls(&mut sock)?;
            continue;
        }
        if conn.wants_read() {
            if conn.read_tls(&mut sock)? == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "server closed during TLS handshake",
                ));
            }
            conn.process_new_packets()
                .map_err(|e| io::Error::other(format!("process_new_packets: {e}")))?;
        }
    }

    // ── Flush the client Finished before stopping the clock ───────────────────
    //
    // `is_handshaking()` goes false on the CLIENT as soon as it has the traffic
    // keys, which is one step before the server knows anything. The client
    // Finished is sitting in rustls' write buffer at this point, and the loop
    // above has already exited, so without this it is never put on the wire.
    //
    // The first version of this function omitted it, and the failure was silent
    // in the worst way: every handshake "succeeded" client-side with a plausible
    // 0.34ms, while the server never completed a single one. It sat in
    // `is_handshaking()` until the probe's close arrived as EOF and recorded
    // nothing. 400 probe connections produced ZERO server-side samples, and the
    // 70 the server did report turned out to be the cache warmer's curl
    // connections at startup -- which very nearly got read as a server bug.
    //
    // A client that reports success while the peer saw no completed handshake is
    // not a measuring tool. Flushing is inside the timed region because putting
    // Finished on the wire is part of the handshake, and it is the event the
    // server's own figure ends at.
    while conn.wants_write() {
        conn.write_tls(&mut sock)?;
    }
    let elapsed = t0.elapsed();
    let alpn = conn.alpn_protocol().map(|p| p.to_vec());

    // Send the close_notify rather than dropping the socket on the server's face.
    // A bare FIN mid-stream is indistinguishable from a truncation attack and
    // makes the server's logs misrepresent what this tool did.
    conn.send_close_notify();
    while conn.wants_write() {
        conn.write_tls(&mut sock)?;
    }
    Ok((elapsed, alpn))
}

/// Flush everything quiche wants to send, returning (datagrams, bytes).
///
/// The BYTES are what matters for the anti-amplification ratio, and counting only
/// the server's side of it was the mistake that made an earlier reading of this
/// look impossible: a server appeared to send 4x what it had received, which no
/// conforming server does. The budget grows with every byte the client sends, so
/// the server's total is meaningless without the client's beside it.
fn quic_flush_counted(conn: &mut quiche::Connection, udp: &UdpSocket) -> (usize, usize) {
    let mut out = [0u8; 1350];
    let mut sent = 0;
    let mut bytes = 0;
    loop {
        match conn.send(&mut out) {
            Ok((n, _)) => {
                if udp.send(&out[..n]).is_err() {
                    return (sent, bytes);
                }
                sent += 1;
                bytes += n;
            }
            Err(quiche::Error::Done) => return (sent, bytes),
            Err(_) => return (sent, bytes),
        }
    }
}

/// Flush and discard the count, for callers that do not care.
fn quic_flush(conn: &mut quiche::Connection, udp: &UdpSocket) {
    let _ = quic_flush_counted(conn, udp);
}

/// What a 0-RTT attempt actually achieved.
///
/// `early_data_used` is the field that matters and the reason this struct exists
/// rather than a bare duration. A server with 0-RTT misconfigured, or a ticket
/// the server declines, silently falls back to a normal 1-RTT handshake and
/// answers correctly. The timing alone cannot tell that apart from 0-RTT working,
/// so the probe records whether the request was on the wire BEFORE the handshake
/// completed, which is the thing being claimed.
#[derive(Debug, Clone)]
pub struct ZeroRtt {
    /// First connection: a full handshake, no ticket held yet.
    pub cold_handshake: Duration,
    /// Whether the server issued a resumption ticket at all. Without one there is
    /// nothing to resume and 0-RTT is impossible, however it is configured.
    pub got_ticket: bool,
    /// Second connection: from the first packet to the response headers.
    pub warm_to_response: Option<Duration>,
    /// Whether the request was sent while still in early data.
    pub early_data_used: bool,
    /// The status the resumed connection got back. 425 means the server accepted
    /// the early data and correctly refused to ACT on it, which is a pass for the
    /// safety gate and a miss for the latency win.
    pub status: Option<u16>,
}

/// A client QUIC config that will offer and use early data.
fn quic_client_cfg() -> io::Result<quiche::Config> {
    let mut cfg = quiche::Config::new(quiche::PROTOCOL_VERSION)
        .map_err(|e| io::Error::other(format!("quiche::Config::new: {e}")))?;
    cfg.verify_peer(false);
    cfg.set_application_protos(quiche::h3::APPLICATION_PROTOCOL)
        .map_err(|e| io::Error::other(format!("set_application_protos: {e}")))?;
    cfg.set_max_idle_timeout(5_000);
    cfg.set_max_recv_udp_payload_size(1350);
    cfg.set_max_send_udp_payload_size(1350);
    cfg.set_initial_max_data(1_000_000);
    cfg.set_initial_max_stream_data_bidi_local(1_000_000);
    cfg.set_initial_max_stream_data_uni(100_000);
    cfg.set_initial_max_streams_bidi(10);
    cfg.set_initial_max_streams_uni(10);
    cfg.set_disable_active_migration(true);
    // Without this the client will not offer early data on the resumed
    // connection, and the probe would report 0-RTT as not working while the
    // server was configured perfectly well.
    cfg.enable_early_data();
    Ok(cfg)
}

/// Split `host:port` into the SNI name and a resolved socket address.
///
/// Both halves are needed and they are not the same thing. Earlier versions
/// parsed the target straight into a `SocketAddr`, which accepts only literal
/// IPs, and sent a hardcoded SNI of "localhost". That was fine against our own
/// loopback and useless for the question that matters most: whether OTHER
/// servers pay the same handshake cost we do. A real host rejects or misserves a
/// wrong SNI, so a probe that cannot send the right one cannot compare anything.
pub fn resolve(addr: &str) -> io::Result<(String, std::net::SocketAddr)> {
    use std::net::ToSocketAddrs;
    let host = match addr.rsplit_once(':') {
        Some((h, _)) => h.trim_matches(['[', ']']).to_string(),
        None => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{addr}: expected host:port"),
            ))
        }
    };
    let sock = addr
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("{addr}: no address")))?;
    Ok((host, sock))
}

/// Open a UDP socket connected to `addr`, returning it with the two addresses
/// quiche needs on every packet, plus the SNI name to present.
fn quic_socket(
    addr: &str,
) -> io::Result<(
    UdpSocket,
    std::net::SocketAddr,
    std::net::SocketAddr,
    String,
)> {
    let (host, peer) = resolve(addr)?;
    let udp = UdpSocket::bind(if peer.is_ipv6() {
        "[::]:0"
    } else {
        "0.0.0.0:0"
    })?;
    udp.connect(peer)?;
    let local = udp.local_addr()?;
    udp.set_read_timeout(Some(Duration::from_millis(5)))?;
    Ok((udp, peer, local, host))
}

/// A request, as h3 headers. The method is a parameter so the 0-RTT safety gate
/// for replayable methods can be PROVEN rather than asserted: an untested
/// security gate is a claim.
fn h3_req(method: &str, path: &str, authority: &str) -> Vec<quiche::h3::Header> {
    vec![
        quiche::h3::Header::new(b":method", method.as_bytes()),
        quiche::h3::Header::new(b":scheme", b"https"),
        quiche::h3::Header::new(b":authority", authority.as_bytes()),
        quiche::h3::Header::new(b":path", path.as_bytes()),
    ]
}

/// Measure a 0-RTT resumption: one cold connection to earn a ticket, then a
/// second that sends its request in the first flight.
///
/// `path` should be something the edge holds in cache. m6-http answers 425 Too
/// Early in early data unless it has a FRESH cache entry, deliberately, because a
/// replayed request that reaches a backend could have side effects. So a 425 here
/// is not a failure of 0-RTT: it means the safety gate fired, and the path needs
/// warming first.
pub fn zero_rtt_h3(addr: &str, path: &str, method: &str) -> io::Result<ZeroRtt> {
    // ── Connection 1: full handshake, then wait for the ticket ────────────────
    let (udp, peer, local, sni) = quic_socket(addr)?;
    let mut cfg = quic_client_cfg()?;
    let mut scid = [0u8; 16];
    getrandom_scid(&mut scid);
    let t0 = Instant::now();
    let mut conn = quiche::connect(
        Some(&sni),
        &quiche::ConnectionId::from_ref(&scid),
        local,
        peer,
        &mut cfg,
    )
    .map_err(|e| io::Error::other(format!("quiche::connect: {e}")))?;
    quic_flush(&mut conn, &udp);

    let mut buf = [0u8; 65535];
    let mut cold = None;
    let mut h3: Option<quiche::h3::Connection> = None;
    let mut sent = false;
    // The ticket arrives in a NewSessionTicket AFTER the handshake, so this has to
    // keep pumping past establishment. Making a real request is part of that: it
    // gives the server a reason to finish talking to us.
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut session: Option<Vec<u8>> = None;
    while Instant::now() < deadline {
        if conn.is_established() && cold.is_none() {
            cold = Some(t0.elapsed());
        }
        if conn.is_established() && h3.is_none() {
            let hc = quiche::h3::Config::new()
                .map_err(|e| io::Error::other(format!("h3 config: {e}")))?;
            h3 = quiche::h3::Connection::with_transport(&mut conn, &hc).ok();
        }
        if let Some(hc) = h3.as_mut() {
            if !sent {
                if hc
                    .send_request(&mut conn, &h3_req("GET", path, &sni), true)
                    .is_ok()
                {
                    sent = true;
                }
            } else {
                while hc.poll(&mut conn).is_ok() {}
            }
        }
        quic_flush(&mut conn, &udp);
        if session.is_none() {
            if let Some(sess) = conn.session() {
                session = Some(sess.to_vec());
            }
        }
        // Stop as soon as there is a ticket and the request went out; there is
        // nothing further this connection can teach us.
        if session.is_some() && sent {
            break;
        }
        match udp.recv(&mut buf) {
            Ok(n) => {
                let _ = conn.recv(
                    &mut buf[..n],
                    quiche::RecvInfo {
                        from: peer,
                        to: local,
                    },
                );
            }
            Err(e)
                if e.kind() == io::ErrorKind::WouldBlock || e.kind() == io::ErrorKind::TimedOut =>
            {
                conn.on_timeout();
            }
            Err(e) => return Err(e),
        }
    }
    let cold_handshake = cold.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::TimedOut,
            "first connection never established",
        )
    })?;
    conn.close(true, 0, b"done").ok();
    quic_flush(&mut conn, &udp);

    let Some(session) = session else {
        // No ticket means resumption is impossible. Reported rather than treated
        // as an error, because "the server issues no tickets" is a finding.
        return Ok(ZeroRtt {
            cold_handshake,
            got_ticket: false,
            warm_to_response: None,
            early_data_used: false,
            status: None,
        });
    };

    // ── Connection 2: resume, and send in the first flight ────────────────────
    let (udp2, peer2, local2, sni2) = quic_socket(addr)?;
    let mut cfg2 = quic_client_cfg()?;
    let mut scid2 = [0u8; 16];
    getrandom_scid(&mut scid2);
    let t1 = Instant::now();
    let mut c2 = quiche::connect(
        Some(&sni2),
        &quiche::ConnectionId::from_ref(&scid2),
        local2,
        peer2,
        &mut cfg2,
    )
    .map_err(|e| io::Error::other(format!("quiche::connect (resume): {e}")))?;
    // Must be before any packet is sent, per quiche's own documentation.
    c2.set_session(&session)
        .map_err(|e| io::Error::other(format!("set_session: {e}")))?;

    let mut h3b: Option<quiche::h3::Connection> = None;
    let mut sent2 = false;
    let mut early_data_used = false;
    let mut status = None;
    let mut warm = None;
    let deadline2 = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline2 {
        // FLUSH FIRST, before looking at whether early data is available.
        //
        // The early-data keys are derived as quiche builds the Initial packet, so
        // on the first pass through this loop `is_in_early_data()` is false until
        // something has been sent. A version of this loop that checked before
        // flushing therefore could not send the request until after a `recv` had
        // timed out, and it measured 18.2ms to a response on loopback against a
        // 1.7ms cold handshake -- 0-RTT looking ten times WORSE than no 0-RTT,
        // entirely because of the order of two statements in the measuring tool.
        quic_flush(&mut c2, &udp2);

        // Send the moment early data is available, which is the entire point. If
        // this waited for `is_established()` it would be a normal 1-RTT request
        // that happened to be measured on a resumed connection, and it would look
        // like a modest win rather than a saved round trip.
        if !sent2 && (c2.is_in_early_data() || c2.is_established()) {
            let hc = quiche::h3::Config::new()
                .map_err(|e| io::Error::other(format!("h3 config: {e}")))?;
            if h3b.is_none() {
                h3b = quiche::h3::Connection::with_transport(&mut c2, &hc).ok();
            }
            if let Some(hc2) = h3b.as_mut() {
                if hc2
                    .send_request(&mut c2, &h3_req(method, path, &sni2), true)
                    .is_ok()
                {
                    sent2 = true;
                    // Recorded at the moment of sending, not afterwards: this is
                    // the claim "the request was on the wire before the handshake
                    // finished", and it stops being checkable a moment later.
                    early_data_used = !c2.is_established();
                }
            }
        }
        if let Some(hc2) = h3b.as_mut() {
            while let Ok((_, ev)) = hc2.poll(&mut c2) {
                if let quiche::h3::Event::Headers { list, .. } = ev {
                    if warm.is_none() {
                        warm = Some(t1.elapsed());
                    }
                    for h in &list {
                        if h.name() == b":status" {
                            status = std::str::from_utf8(h.value())
                                .ok()
                                .and_then(|v| v.parse::<u16>().ok());
                        }
                    }
                }
            }
        }
        quic_flush(&mut c2, &udp2);
        if warm.is_some() {
            break;
        }
        match udp2.recv(&mut buf) {
            Ok(n) => {
                let _ = c2.recv(
                    &mut buf[..n],
                    quiche::RecvInfo {
                        from: peer2,
                        to: local2,
                    },
                );
            }
            Err(e)
                if e.kind() == io::ErrorKind::WouldBlock || e.kind() == io::ErrorKind::TimedOut =>
            {
                c2.on_timeout();
            }
            Err(e) => return Err(e),
        }
    }
    c2.close(true, 0, b"done").ok();
    quic_flush(&mut c2, &udp2);

    Ok(ZeroRtt {
        cold_handshake,
        got_ticket: true,
        warm_to_response: warm,
        early_data_used,
        status,
    })
}

/// What the handshake cost in round trips, not just in milliseconds.
///
/// A duration alone cannot tell a one-round-trip handshake on a slow link from a
/// two-round-trip handshake on a fast one, and the difference is the only part
/// that is actionable: an extra round trip is 300ms between London and Sydney
/// however fast the CPU is.
///
/// `server_bytes_before_established` is the figure that identifies the QUIC
/// anti-amplification limit specifically. A server may send at most about three
/// times what it has received before the client's address is validated, so a
/// certificate chain larger than that budget forces the server to stop and wait
/// for more client data, costing a full round trip. A value sitting just under
/// ~3600 with more than one client flight is that limit, not a slow network.
#[derive(Debug, Clone)]
pub struct HandshakeShape {
    pub elapsed: Duration,
    pub client_flights: usize,
    pub server_datagrams: usize,
    pub server_bytes_before_established: usize,
    /// What the client sent before establishment. The anti-amplification budget
    /// is roughly three times this, so it is the denominator without which the
    /// server's figure says nothing.
    pub client_bytes_before_established: usize,
    /// A merged timeline of who sent what, when, as cumulative byte totals.
    ///
    /// This exists because the TOTALS are ambiguous and led to two wrong
    /// conclusions in a row. The anti-amplification budget is three times what the
    /// server has received AT THAT MOMENT, so a ratio computed from end-of-handshake
    /// totals can look comfortable (1.6x) while the ratio at the instant the server
    /// stopped was right on the 3x limit. Only a timeline distinguishes them.
    ///
    /// Each entry is (time, who, cumulative bytes that side has sent).
    pub timeline: Vec<(Duration, &'static str, usize)>,
    /// When each server datagram arrived, measured from the client's first send.
    ///
    /// The totals say a handshake was slow; only the gaps say WHERE. A pause
    /// between two server datagrams is the server not sending, and its size
    /// identifies the cause: roughly one round trip means it was waiting for the
    /// client (the anti-amplification limit), while a pause much shorter than a
    /// round trip means it had data to send and did not send it.
    pub server_arrivals: Vec<Duration>,
}

/// One QUIC handshake, timed to `is_established()`.
///
/// This INCLUDES the transport round trip, unlike the TLS figure above. There is
/// no earlier point to start from: the first thing the client does is send an
/// Initial packet, and crypto and transport complete together.
pub fn quic_handshake_shape(addr: &str) -> io::Result<HandshakeShape> {
    let (udp, peer, local, sni) = quic_socket(addr)?;

    let mut cfg = quiche::Config::new(quiche::PROTOCOL_VERSION)
        .map_err(|e| io::Error::other(format!("quiche::Config::new: {e}")))?;
    cfg.verify_peer(false);
    cfg.set_application_protos(quiche::h3::APPLICATION_PROTOCOL)
        .map_err(|e| io::Error::other(format!("set_application_protos: {e}")))?;
    cfg.set_max_idle_timeout(5_000);
    cfg.set_max_recv_udp_payload_size(1350);
    cfg.set_max_send_udp_payload_size(1350);
    cfg.set_initial_max_data(1_000_000);
    cfg.set_initial_max_stream_data_bidi_local(100_000);
    cfg.set_initial_max_streams_bidi(10);
    // Unidirectional streams, which h3 needs for its control stream and the two
    // QPACK streams. Omitting this leaves the limit at the default of 0, so the
    // server cannot open them and `h3::Connection::with_transport` fails with
    // StreamLimit -- which it then LOGS. Probing production wrote four
    // `h3 init error: TransportError(StreamLimit)` warnings into edge-a's journal
    // and the same into edge-b's, and they turned up in a health check as an
    // unexplained new error on two nodes.
    //
    // The handshake figure was unaffected, because QUIC establishment completes
    // before h3 is set up. But a measuring tool that makes the thing it measures
    // log errors is not read-only, and the next person reading those logs has no
    // way to know the cause was the probe.
    cfg.set_initial_max_streams_uni(10);
    // And credit to WRITE on those streams. The stream limit alone is not enough:
    // a peer allowed to open a stream but granted zero bytes on it cannot send the
    // h3 SETTINGS frame, and quiche surfaces the resulting failure from
    // `h3::Connection::with_transport` as a transport error. Setting the limit
    // without the credit was the first attempt at this fix and it changed nothing.
    cfg.set_initial_max_stream_data_uni(100_000);
    cfg.set_disable_active_migration(true);

    // A fresh connection ID per connection. A reused one would let the server
    // treat the second connection as the first, and the handshake being measured
    // would not happen at all.
    let mut scid = [0u8; 16];
    getrandom_scid(&mut scid);
    let scid = quiche::ConnectionId::from_ref(&scid);

    let t0 = Instant::now();
    let mut conn = quiche::connect(Some(&sni), &scid, local, peer, &mut cfg)
        .map_err(|e| io::Error::other(format!("quiche::connect: {e}")))?;
    let (_, mut client_bytes) = quic_flush_counted(&mut conn, &udp);
    let mut timeline: Vec<(Duration, &'static str, usize)> =
        vec![(t0.elapsed(), "client", client_bytes)];

    let mut buf = [0u8; 65535];
    let deadline = t0 + Duration::from_secs(5);
    // 5ms rather than the 100ms m6-bench-detail uses. The read timeout is the
    // granularity of any stall this can see, and a 100ms floor would round a
    // real multi-millisecond problem into one indistinguishable bucket.
    udp.set_read_timeout(Some(Duration::from_millis(5)))?;
    // The opening Initial, already sent above, is flight one.
    let mut client_flights = 1usize;
    let mut server_datagrams = 0usize;
    let mut server_bytes = 0usize;
    let mut server_arrivals: Vec<Duration> = Vec::new();
    while !conn.is_established() {
        if Instant::now() > deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "QUIC handshake timeout",
            ));
        }
        if conn.is_closed() {
            return Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "QUIC closed during handshake",
            ));
        }
        // ── FLUSH BEFORE RECV ────────────────────────────────────────────────
        //
        // The first version of this loop read before it wrote. When the server is
        // waiting on the client -- for the ACK that validates its address, which
        // is exactly the case this function exists to detect -- the probe sat in
        // the 5ms read timeout BEFORE sending the thing the server was waiting
        // for, then sent it. That added roughly 5ms per round trip to the
        // measurement and made a 1-RTT handshake read as though it were two.
        //
        // The identical mistake was made and fixed in `zero_rtt_h3`, where it
        // reported 18.2ms against a true 1.05ms. A measuring tool that adds its
        // own latency to the thing it measures reports a plausible number and an
        // entirely wrong conclusion.
        let (dg, by) = quic_flush_counted(&mut conn, &udp);
        if by > 0 {
            client_bytes += by;
            timeline.push((t0.elapsed(), "client", client_bytes));
        }
        if dg > 0 {
            client_flights += 1;
        }
        match udp.recv(&mut buf) {
            Ok(n) => {
                server_datagrams += 1;
                server_bytes += n;
                server_arrivals.push(t0.elapsed());
                timeline.push((t0.elapsed(), "server", server_bytes));
                conn.recv(
                    &mut buf[..n],
                    quiche::RecvInfo {
                        from: peer,
                        to: local,
                    },
                )
                .map_err(|e| io::Error::other(format!("quiche recv: {e}")))?;
            }
            Err(e)
                if e.kind() == io::ErrorKind::WouldBlock || e.kind() == io::ErrorKind::TimedOut =>
            {
                // Nothing arrived inside the window: let quiche run its timers so
                // a lost Initial is retransmitted rather than waiting out the
                // deadline.
                conn.on_timeout();
            }
            Err(e) => return Err(e),
        }
    }
    // One last flush so the client's final flight is actually on the wire, the
    // same reason the TLS probe flushes its Finished: the loop exits on the
    // CLIENT's view of establishment, which is one step before the server's.
    quic_flush(&mut conn, &udp);
    Ok(HandshakeShape {
        elapsed: t0.elapsed(),
        client_flights,
        server_datagrams,
        server_bytes_before_established: server_bytes,
        client_bytes_before_established: client_bytes,
        timeline,
        server_arrivals,
    })
}

/// One QUIC handshake, timed. The duration only; see `quic_handshake_shape` when
/// the round-trip count matters.
pub fn quic_handshake(addr: &str) -> io::Result<Duration> {
    quic_handshake_shape(addr).map(|s| s.elapsed)
}

/// Connection IDs from the OS. `rand` is a dependency here, but reading
/// /dev/urandom directly keeps this module's job obvious and its imports few.
fn getrandom_scid(buf: &mut [u8; 16]) {
    use std::io::Read as _;
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        if f.read_exact(buf).is_ok() {
            return;
        }
    }
    // Fallback: the clock. Weaker, but a probe that cannot open /dev/urandom
    // should still run rather than refuse.
    let ns = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    buf[..16].copy_from_slice(&ns.to_le_bytes()[..16]);
}

/// The figures drawn from a set of samples.
///
/// Separate from printing so the numbers can be asserted in a test. A formatter
/// that can only be eyeballed is how a percentile over six samples gets quoted
/// as a measurement.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Summary {
    pub n: usize,
    pub min: Duration,
    pub p50: Duration,
    pub p90: Duration,
    pub p99: Duration,
    pub max: Duration,
    pub mean: Duration,
}

/// `None` for an empty sample set, never a zero.
///
/// "p50 0.000ms" on a channel that never handshook reads as an impossibly fast
/// server rather than as an absence of data.
pub fn summarise(samples: &mut [Duration]) -> Option<Summary> {
    if samples.is_empty() {
        return None;
    }
    samples.sort_unstable();
    let n = samples.len();
    // The SAME integer arithmetic as `percentiles_n` in stats.rs, deliberately.
    //
    // The whole purpose of these probes is comparing their figure to the server's,
    // and the two conventions disagree. Truncating `(n-1)*50/100` picks index 4
    // of ten samples; rounding `(n-1)*0.50` picks index 5. On a real
    // distribution that is a small offset in a fixed direction, which is exactly
    // the kind of difference that gets read as a finding about the server.
    //
    // n=1 gives index 0 for every quantile: correct, and it cannot panic.
    let at = |pct: usize| samples[(n - 1) * pct / 100];
    let total: Duration = samples.iter().sum();
    Some(Summary {
        n,
        min: samples[0],
        p50: at(50),
        p90: at(90),
        p99: at(99),
        max: samples[n - 1],
        mean: total / n as u32,
    })
}

/// Print the summary, always with its sample count.
///
/// The count goes next to the percentiles and is not optional. A p50 over six
/// handshakes and one over six hundred are different claims, and the number alone
/// cannot be compared to anything without it.
pub fn report(label: &str, mut samples: Vec<Duration>, failures: usize) {
    let ms = |d: Duration| d.as_secs_f64() * 1000.0;
    match summarise(&mut samples) {
        None => println!("{label}: no successful handshakes ({failures} failed)"),
        Some(s) => println!(
            "{label}: n={} failed={failures}  \
             min={:.3}ms  p50={:.3}ms  p90={:.3}ms  p99={:.3}ms  max={:.3}ms  mean={:.3}ms",
            s.n,
            ms(s.min),
            ms(s.p50),
            ms(s.p90),
            ms(s.p99),
            ms(s.max),
            ms(s.mean),
        ),
    }
}

/// `--addr HOST:PORT` and `--n COUNT`, and nothing else.
pub fn args(default_addr: &str, default_n: usize) -> (String, usize) {
    let mut addr = default_addr.to_string();
    let mut n = default_n;
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < raw.len() {
        match raw[i].as_str() {
            "--addr" if i + 1 < raw.len() => {
                addr = raw[i + 1].clone();
                i += 2;
            }
            "--n" if i + 1 < raw.len() => {
                n = raw[i + 1].parse().unwrap_or(default_n);
                i += 2;
            }
            other => {
                eprintln!("usage: [--addr HOST:PORT] [--n COUNT]   (unexpected: {other})");
                std::process::exit(2);
            }
        }
    }
    (addr, n)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An empty sample set summarises to None, never to a set of zeroes.
    #[test]
    fn no_samples_is_not_a_zero() {
        assert_eq!(summarise(&mut []), None);
        // And the printer handles it without inventing a figure.
        report("test/empty", Vec::new(), 3);
    }

    /// One sample is a legal input and must not panic on the quantile indexing.
    /// It is also exactly what m6-bench-detail reported for h2 (`n=1`), which is
    /// why a single sample must never be mistaken for a distribution.
    #[test]
    fn one_sample_does_not_panic_and_is_its_own_everything() {
        let s = summarise(&mut [Duration::from_micros(400)]).expect("some");
        assert_eq!(s.n, 1);
        assert_eq!(s.min, Duration::from_micros(400));
        assert_eq!(s.p50, Duration::from_micros(400));
        assert_eq!(s.p99, Duration::from_micros(400));
        assert_eq!(s.max, Duration::from_micros(400));
    }

    /// The quantiles come off the SORTED set, and the max is the real worst case.
    /// Unsorted input silently producing a plausible-looking p50 is the failure
    /// mode worth pinning down.
    #[test]
    fn quantiles_are_taken_in_order_whatever_order_they_arrive_in() {
        let mut v: Vec<Duration> = [50u64, 10, 90, 30, 70, 100, 20, 80, 40, 60]
            .iter()
            .map(|&m| Duration::from_millis(m))
            .collect();
        let s = summarise(&mut v).expect("some");
        assert_eq!(s.n, 10);
        assert_eq!(s.min, Duration::from_millis(10));
        assert_eq!(s.max, Duration::from_millis(100));
        // Index (10-1)*50/100 = 4, matching stats.rs. Truncating, not rounding.
        assert_eq!(s.p50, Duration::from_millis(50));
        // (10-1)*90/100 = 8 and (10-1)*99/100 = 8, so p90 and p99 coincide at
        // this sample count. That is a property of ten samples, not of the
        // server, and it is why the count is always printed.
        assert_eq!(s.p90, Duration::from_millis(90));
        assert_eq!(s.p99, Duration::from_millis(90));
        assert_eq!(s.mean, Duration::from_millis(55));
    }

    /// A fresh connection ID every time. Two probes sharing one would have the
    /// server treat the second as a continuation and no second handshake would
    /// occur, so the tool would report a duration for something that did not
    /// happen.
    #[test]
    fn connection_ids_differ() {
        let mut a = [0u8; 16];
        let mut b = [0u8; 16];
        getrandom_scid(&mut a);
        getrandom_scid(&mut b);
        assert_ne!(a, b, "two connection IDs came out identical");
    }
}
