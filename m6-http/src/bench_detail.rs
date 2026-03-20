/// m6-bench-detail — per-phase latency breakdown for m6-http.
///
/// Instruments each step of the request lifecycle from connection open to last byte received,
/// for HTTP/1.1, HTTP/2, HTTP/3, and H2C (HTTP/2 cleartext over plain TCP).
/// Also measures full-page load (HTML + all linked assets) for each protocol.
///
/// Flags:
///   --addr HOST:PORT      target for TLS protocols (default 127.0.0.1:8443)
///   --h2c-addr HOST:PORT  target for H2C plain-TCP port (default 127.0.0.1:8080)
///   --n N                 samples per phase (default 10000)
///   --skip-verify         skip TLS certificate verification
///   --http11-only / --http2-only / --http3-only / --h2c-only
///   --h2c                 include H2C alongside selected protocols
///   --out-dir DIR         directory for SVG chart output (default ".")
///
/// Cache note: all paths are pre-warmed (100 warmup requests) before measurement.
/// Warmup samples are excluded from all reported statistics.

use std::io::{self, Read, Write};
use std::net::{TcpStream, UdpSocket};
use std::os::unix::io::AsRawFd;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rand::Rng;
use rustls::ClientConfig;
use rustls::pki_types::{ServerName, CertificateDer, UnixTime};

// ── CLI ───────────────────────────────────────────────────────────────────────

struct Args {
    addr:        String,
    h2c_addr:    String,
    n:           usize,
    skip_verify: bool,
    http11:      bool,
    http2:       bool,
    http3:       bool,
    h2c:         bool,
    out_dir:     String,
}

impl Args {
    fn parse() -> Self {
        let mut a = Args {
            addr:        "127.0.0.1:8443".into(),
            h2c_addr:    "127.0.0.1:8080".into(),
            n:           10_000,
            skip_verify: false,
            http11:      true,
            http2:       true,
            http3:       true,
            h2c:         false,
            out_dir:     ".".into(),
        };
        let raw: Vec<String> = std::env::args().skip(1).collect();
        let mut i = 0;
        while i < raw.len() {
            match raw[i].as_str() {
                "--addr"         => { i += 1; a.addr     = raw[i].clone(); }
                "--h2c-addr"     => { i += 1; a.h2c_addr = raw[i].clone(); }
                "--n"            => { i += 1; a.n        = raw[i].parse().expect("--n"); }
                "--out-dir"      => { i += 1; a.out_dir  = raw[i].clone(); }
                "--skip-verify"  => { a.skip_verify = true; }
                "--http11-only"  => { a.http11 = true;  a.http2 = false; a.http3 = false; a.h2c = false; }
                "--http2-only"   => { a.http11 = false; a.http2 = true;  a.http3 = false; a.h2c = false; }
                "--http3-only"   => { a.http11 = false; a.http2 = false; a.http3 = true;  a.h2c = false; }
                "--h2c-only"     => { a.http11 = false; a.http2 = false; a.http3 = false; a.h2c = true; }
                "--h2c"          => { a.h2c = true; }
                other => { eprintln!("unknown flag: {other}"); std::process::exit(1); }
            }
            i += 1;
        }
        a
    }
}

// ── TLS / no-verify ───────────────────────────────────────────────────────────

#[derive(Debug)]
struct NoVerify;

impl rustls::client::danger::ServerCertVerifier for NoVerify {
    fn verify_server_cert(&self, _: &CertificateDer<'_>, _: &[CertificateDer<'_>],
        _: &ServerName<'_>, _: &[u8], _: UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(&self, _: &[u8], _: &CertificateDer<'_>,
        _: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }
    fn verify_tls13_signature(&self, _: &[u8], _: &CertificateDer<'_>,
        _: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }
    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider().signature_verification_algorithms.supported_schemes()
    }
}

fn make_tls_config_h1(skip_verify: bool) -> Arc<ClientConfig> {
    if skip_verify {
        Arc::new(ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(NoVerify))
            .with_no_client_auth())
    } else {
        let mut roots = rustls::RootCertStore::empty();
        for cert in rustls_native_certs::load_native_certs().certs { roots.add(cert).ok(); }
        Arc::new(ClientConfig::builder().with_root_certificates(roots).with_no_client_auth())
    }
}

fn make_tls_config_h2(skip_verify: bool) -> Arc<ClientConfig> {
    if skip_verify {
        let mut cfg = ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(NoVerify))
            .with_no_client_auth();
        cfg.alpn_protocols = vec![b"h2".to_vec()];
        Arc::new(cfg)
    } else {
        let mut roots = rustls::RootCertStore::empty();
        for cert in rustls_native_certs::load_native_certs().certs { roots.add(cert).ok(); }
        let mut cfg = ClientConfig::builder().with_root_certificates(roots).with_no_client_auth();
        cfg.alpn_protocols = vec![b"h2".to_vec()];
        Arc::new(cfg)
    }
}

fn make_quiche_cfg(skip_verify: bool) -> quiche::Config {
    let mut cfg = quiche::Config::new(quiche::PROTOCOL_VERSION).unwrap();
    cfg.verify_peer(!skip_verify);
    cfg.set_application_protos(quiche::h3::APPLICATION_PROTOCOL).unwrap();
    cfg.set_max_idle_timeout(5000);
    cfg.set_max_recv_udp_payload_size(1350);
    cfg.set_max_send_udp_payload_size(1350);
    cfg.set_initial_max_data(10_000_000);
    cfg.set_initial_max_stream_data_bidi_local(1_000_000);
    cfg.set_initial_max_stream_data_bidi_remote(1_000_000);
    cfg.set_initial_max_stream_data_uni(1_000_000);
    cfg.set_initial_max_streams_bidi(100);
    cfg.set_initial_max_streams_uni(100);
    cfg.set_disable_active_migration(true);
    cfg
}

// ── Shared stats ──────────────────────────────────────────────────────────────

/// All values in microseconds.
#[derive(Clone)]
struct BoxStats {
    label:  String,
    n:      usize,
    #[allow(dead_code)]
    min:    f64,
    p5:     f64,
    p25:    f64,
    p50:    f64,
    p75:    f64,
    p95:    f64,
    max:    f64,
    mean:   f64,
    stddev: f64,
}

impl BoxStats {
    fn from_samples(label: impl Into<String>, mut v: Vec<f64>) -> Self {
        assert!(!v.is_empty(), "no samples");
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let n    = v.len();
        let pct  = |p: f64| { let idx = ((p / 100.0) * (n as f64 - 1.0)).round() as usize; v[idx.min(n-1)] };
        let mean = v.iter().sum::<f64>() / n as f64;
        let var  = v.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / (n - 1).max(1) as f64;
        BoxStats {
            label: label.into(), n,
            min:    v[0],
            p5:     pct(5.0),
            p25:    pct(25.0),
            p50:    pct(50.0),
            p75:    pct(75.0),
            p95:    pct(95.0),
            max:    *v.last().unwrap(),
            mean,
            stddev: var.sqrt(),
        }
    }
}

/// All per-phase sample collections for one protocol.
struct PhaseSamples {
    connect:      Vec<f64>, // TCP connect (H1/H2) or QUIC handshake (H3)
    tls:          Vec<f64>, // TLS handshake (H1/H2); H3 = 0 (included in QUIC hs)
    proto_setup:  Vec<f64>, // H2: SETTINGS exchange; H3: H3 init; H1: 0
    request_send: Vec<f64>, // write request bytes
    ttfb:         Vec<f64>, // request_sent → first byte of response
    transfer:     Vec<f64>, // first byte → last byte
    total:        Vec<f64>, // connection open → last byte received
}

impl PhaseSamples {
    fn new(n: usize) -> Self {
        PhaseSamples {
            connect:      Vec::with_capacity(n),
            tls:          Vec::with_capacity(n),
            proto_setup:  Vec::with_capacity(n),
            request_send: Vec::with_capacity(n),
            ttfb:         Vec::with_capacity(n),
            transfer:     Vec::with_capacity(n),
            total:        Vec::with_capacity(n),
        }
    }

    fn into_stats(self, proto: &str) -> Vec<BoxStats> {
        let phases: &[(&str, Vec<f64>)] = &[
            ("connect",      self.connect),
            ("tls",          self.tls),
            ("proto_setup",  self.proto_setup),
            ("request_send", self.request_send),
            ("ttfb",         self.ttfb),
            ("transfer",     self.transfer),
            ("total",        self.total),
        ];
        phases.iter()
            .filter(|(_, v)| !v.is_empty() && v.iter().any(|&x| x > 0.0))
            .map(|(name, v)| BoxStats::from_samples(format!("{proto}/{name}"), v.clone()))
            .collect()
    }
}

/// Full-page-load sample collections (one sample = HTML + all assets).
struct FullPageSamples {
    connect:     Vec<f64>, // TCP/QUIC connection setup
    tls:         Vec<f64>, // TLS handshake (H1/H2)
    html_done:   Vec<f64>, // start → HTML fully received
    assets_done: Vec<f64>, // start → last asset fully received
    total:       Vec<f64>, // start → last byte
}

impl FullPageSamples {
    fn new(n: usize) -> Self {
        FullPageSamples {
            connect:     Vec::with_capacity(n),
            tls:         Vec::with_capacity(n),
            html_done:   Vec::with_capacity(n),
            assets_done: Vec::with_capacity(n),
            total:       Vec::with_capacity(n),
        }
    }

    fn into_stats(self, proto: &str) -> Vec<BoxStats> {
        let phases: &[(&str, Vec<f64>)] = &[
            ("connect",     self.connect),
            ("tls",         self.tls),
            ("html_done",   self.html_done),
            ("assets_done", self.assets_done),
            ("total",       self.total),
        ];
        phases.iter()
            .filter(|(_, v)| !v.is_empty() && v.iter().any(|&x| x > 0.0))
            .map(|(name, v)| BoxStats::from_samples(format!("{proto}/fullpage/{name}"), v.clone()))
            .collect()
    }
}

// ── H1 per-phase timed GET ────────────────────────────────────────────────────

/// Single H1 GET with per-phase timestamps. The TLS handshake is driven
/// explicitly before writing the request so the phases can be separated.
fn h1_timed_get(addr: &str, path: &str, tls_cfg: Arc<ClientConfig>)
    -> anyhow::Result<(Vec<u8>, f64, f64, f64, f64, f64)>
{
    // connect
    let t0 = Instant::now();
    let stream = TcpStream::connect(addr)?;
    stream.set_nodelay(true)?;
    let t_connected = Instant::now();

    // TLS handshake — drive explicitly without writing request yet
    let server_name: ServerName<'static> = "localhost".try_into().unwrap();
    let mut conn = rustls::ClientConnection::new(tls_cfg, server_name)?;
    // Drive handshake to completion using read_tls / write_tls loops
    while conn.is_handshaking() {
        // Write any pending data (ClientHello, etc.)
        loop {
            match conn.write_tls(&mut &stream) {
                Ok(0) => break,
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => return Err(e.into()),
            }
        }
        if conn.is_handshaking() {
            // Read server response
            match conn.read_tls(&mut &stream) {
                Ok(0) => anyhow::bail!("TLS closed during handshake"),
                Ok(_) => { conn.process_new_packets().map_err(|e| anyhow::anyhow!("{e}"))?; }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    // Blocking poll: wait for socket readable
                    unsafe {
                        let mut pfd = libc::pollfd { fd: stream.as_raw_fd(), events: libc::POLLIN, revents: 0 };
                        libc::poll(&mut pfd, 1, 5000);
                    }
                }
                Err(e) => return Err(e.into()),
            }
        }
    }
    let t_tls = Instant::now();

    // Write request
    let req = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    {
        let mut stream_ref = &stream;
        let mut tls_stream = rustls::Stream::new(&mut conn, &mut stream_ref);
        tls_stream.write_all(req.as_bytes())?;
    }
    let t_request_sent = Instant::now();

    // Read first byte
    let mut first_byte = [0u8; 1];
    let mut resp = Vec::new();
    {
        let mut stream_ref = &stream;
        let mut tls_stream = rustls::Stream::new(&mut conn, &mut stream_ref);
        // Read one byte at a time until we get something
        loop {
            match tls_stream.read(&mut first_byte) {
                Ok(1) => { resp.push(first_byte[0]); break; }
                Ok(0) => break, // EOF
                Ok(_) => unreachable!(),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => continue,
                Err(e) => return Err(e.into()),
            }
        }
    }
    let t_first_byte = Instant::now();

    // Read rest of response
    {
        let mut stream_ref = &stream;
        let mut tls_stream = rustls::Stream::new(&mut conn, &mut stream_ref);
        match tls_stream.read_to_end(&mut resp) {
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof && !resp.is_empty() => {}
            Err(e) => return Err(e.into()),
        }
    }
    let t_done = Instant::now();

    let us = |a: Instant, b: Instant| (b - a).as_secs_f64() * 1_000_000.0;
    Ok((
        resp,
        us(t0,              t_connected),   // connect
        us(t_connected,     t_tls),         // tls
        us(t_tls,           t_request_sent),// request_send
        us(t_request_sent,  t_first_byte),  // ttfb
        us(t_first_byte,    t_done),        // transfer
    ))
}

fn bench_h1_phases(addr: &str, path: &str, n: usize, warmup: usize, tls_cfg: Arc<ClientConfig>)
    -> anyhow::Result<PhaseSamples>
{
    for _ in 0..warmup {
        h1_timed_get(addr, path, Arc::clone(&tls_cfg))?;
    }
    let mut s = PhaseSamples::new(n);
    for _ in 0..n {
        let t0 = Instant::now();
        let (_, connect, tls, req_send, ttfb, transfer) =
            h1_timed_get(addr, path, Arc::clone(&tls_cfg))?;
        let total = t0.elapsed().as_secs_f64() * 1_000_000.0;
        s.connect.push(connect);
        s.tls.push(tls);
        s.request_send.push(req_send);
        s.ttfb.push(ttfb);
        s.transfer.push(transfer);
        s.total.push(total);
    }
    Ok(s)
}

// ── H2 helpers ────────────────────────────────────────────────────────────────

const H2_CLIENT_PREFACE: &[u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";

/// HPACK block for H2C requests (`:scheme http`, static index 6).
fn make_h2c_get_headers(path: &str) -> Vec<u8> {
    let mut h = Vec::with_capacity(32);
    h.push(0x82); // :method GET
    let pb = path.as_bytes();
    if path == "/" {
        h.push(0x84);
    } else {
        h.push(0x04);
        assert!(pb.len() < 128, "path too long");
        h.push(pb.len() as u8);
        h.extend_from_slice(pb);
    }
    h.push(0x86); // :scheme http (index 6; index 7 = https)
    h.extend_from_slice(&[0x41, 0x09, b'l', b'o', b'c', b'a', b'l', b'h', b'o', b's', b't']);
    h
}

fn make_h2_get_headers(path: &str) -> Vec<u8> {
    let mut h = Vec::with_capacity(32);
    h.push(0x82); // :method GET
    let pb = path.as_bytes();
    if path == "/" {
        h.push(0x84);
    } else {
        h.push(0x04);
        assert!(pb.len() < 128, "path too long");
        h.push(pb.len() as u8);
        h.extend_from_slice(pb);
    }
    h.push(0x87); // :scheme https
    h.extend_from_slice(&[0x41, 0x09, b'l', b'o', b'c', b'a', b'l', b'h', b'o', b's', b't']);
    h
}

fn make_h2_frame(ftype: u8, flags: u8, stream_id: u32, payload: &[u8]) -> Vec<u8> {
    let len = payload.len();
    let mut f = Vec::with_capacity(9 + len);
    f.push((len >> 16) as u8);
    f.push((len >> 8)  as u8);
    f.push(len         as u8);
    f.push(ftype);
    f.push(flags);
    f.extend_from_slice(&(stream_id & 0x7fff_ffff).to_be_bytes());
    f.extend_from_slice(payload);
    f
}

fn try_parse_h2_frame(buf: &[u8]) -> Option<(u8, u8, u32, usize)> {
    if buf.len() < 9 { return None; }
    let length = ((buf[0] as usize) << 16) | ((buf[1] as usize) << 8) | buf[2] as usize;
    let total = 9 + length;
    if buf.len() < total { return None; }
    let ftype     = buf[3];
    let flags     = buf[4];
    let stream_id = u32::from_be_bytes(buf[5..9].try_into().unwrap()) & 0x7fff_ffff;
    Some((ftype, flags, stream_id, total))
}

/// Timed H2 client. Tracks connection-setup phases separately from per-request phases.
struct H2TimedClient {
    conn:           rustls::ClientConnection,
    stream:         TcpStream,
    recv_buf:       Vec<u8>,
    tmp:            [u8; 8192],
    next_stream_id: u32,
    requests_done:  usize,
    // Connection-setup timings (set once on connect)
    pub connect_us:      f64,
    pub tls_us:          f64,
    pub proto_setup_us:  f64,
}

impl H2TimedClient {
    fn connect(addr: &str, tls_cfg: Arc<ClientConfig>) -> anyhow::Result<Self> {
        // TCP connect
        let t0 = Instant::now();
        let stream = TcpStream::connect(addr)?;
        stream.set_nodelay(true)?;
        stream.set_nonblocking(true)?;
        let t_connected = Instant::now();

        let server_name: ServerName<'static> = "localhost".try_into().unwrap();
        let conn = rustls::ClientConnection::new(tls_cfg, server_name)?;

        let mut c = H2TimedClient {
            conn, stream,
            recv_buf:       Vec::with_capacity(16_384),
            tmp:            [0u8; 8192],
            next_stream_id: 1,
            requests_done:  0,
            connect_us:     0.0,
            tls_us:         0.0,
            proto_setup_us: 0.0,
        };

        // Queue preface + empty SETTINGS
        c.conn.writer().write_all(H2_CLIENT_PREFACE)?;
        c.conn.writer().write_all(&make_h2_frame(0x4, 0x0, 0, &[]))?;

        // Drive TLS handshake
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            c.flush_write()?;
            if !c.conn.is_handshaking() { break; }
            c.fill_recv_deadline(deadline)?;
        }
        let t_tls = Instant::now();

        // Drain server data (SETTINGS, tickets, etc.) and send SETTINGS ACK + WINDOW_UPDATE.
        // Expand the connection-level flow-control window to 16 MiB so large responses
        // (blog pages, compressed assets) don't cause a server-side send stall.
        c.fill_recv_drain()?;
        c.conn.writer().write_all(&make_h2_frame(0x4, 0x1, 0, &[]))?;  // SETTINGS ACK
        let window_increment: u32 = 16 * 1024 * 1024 - 65_535;        // 16 MiB - initial
        c.conn.writer().write_all(&make_h2_frame(0x8, 0x0, 0, &window_increment.to_be_bytes()))?;
        c.flush_write()?;
        let t_proto = Instant::now();

        let us = |a: Instant, b: Instant| (b - a).as_secs_f64() * 1_000_000.0;
        c.connect_us     = us(t0,          t_connected);
        c.tls_us         = us(t_connected, t_tls);
        c.proto_setup_us = us(t_tls,       t_proto);

        Ok(c)
    }

    fn flush_write(&mut self) -> io::Result<usize> {
        let mut total = 0;
        loop {
            match { let mut sr = &self.stream; self.conn.write_tls(&mut sr) } {
                Ok(0)  => break,
                Ok(n)  => { total += n; }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => return Err(e),
            }
        }
        Ok(total)
    }

    fn fill_recv_deadline(&mut self, deadline: Instant) -> io::Result<()> {
        loop {
            match { let mut sr = &self.stream; self.conn.read_tls(&mut sr) } {
                Ok(0) => return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "closed")),
                Ok(_) => {
                    self.conn.process_new_packets()
                        .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
                    break;
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        return Err(io::Error::new(io::ErrorKind::TimedOut, "deadline"));
                    }
                    self.flush_write()?;
                    let ms = remaining.as_millis().min(100) as i32;
                    unsafe {
                        let mut pfd = libc::pollfd { fd: self.stream.as_raw_fd(), events: libc::POLLIN, revents: 0 };
                        libc::poll(&mut pfd, 1, ms);
                    }
                }
                Err(e) => return Err(e),
            }
        }
        self.drain_plaintext()
    }

    fn fill_recv_drain(&mut self) -> io::Result<()> {
        loop {
            match { let mut sr = &self.stream; self.conn.read_tls(&mut sr) } {
                Ok(0) => return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "closed")),
                Ok(_) => {
                    self.conn.process_new_packets()
                        .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => return Err(e),
            }
        }
        self.drain_plaintext()
    }

    fn drain_plaintext(&mut self) -> io::Result<()> {
        loop {
            match self.conn.reader().read(&mut self.tmp) {
                Ok(0)  => break,
                Ok(n)  => self.recv_buf.extend_from_slice(&self.tmp[..n]),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    /// Send a GET request and return (body, request_send_us, ttfb_us, transfer_us).
    fn get_timed(&mut self, path: &str)
        -> anyhow::Result<(Vec<u8>, f64, f64, f64)>
    {
        let sid = self.next_stream_id;
        self.next_stream_id += 2;
        self.requests_done += 1;

        let headers = make_h2_get_headers(path);
        let frame = make_h2_frame(0x1, 0x05, sid, &headers);

        let t_req_start = Instant::now();
        self.conn.writer().write_all(&frame)?;
        self.flush_write()?;
        let t_req_sent = Instant::now();

        let mut body = Vec::new();
        let mut first_byte_time: Option<Instant> = None;
        let deadline = Instant::now() + Duration::from_secs(5);

        loop {
            if Instant::now() > deadline { anyhow::bail!("H2 response timeout"); }

            if let Some((ftype, flags, fsid, total)) = try_parse_h2_frame(&self.recv_buf) {
                let payload = self.recv_buf[9..total].to_vec();
                self.recv_buf.drain(..total);

                // Record time of first frame on our stream
                if fsid == sid && first_byte_time.is_none() {
                    first_byte_time = Some(Instant::now());
                }

                match ftype {
                    0x0 if fsid == sid => { // DATA
                        // Return flow-control credit for every DATA frame received
                        // (both connection-level and stream-level) so the server never stalls.
                        if !payload.is_empty() {
                            let inc = payload.len() as u32;
                            let wu = make_h2_frame(0x8, 0x0, 0, &inc.to_be_bytes());
                            let ws = make_h2_frame(0x8, 0x0, sid, &inc.to_be_bytes());
                            self.conn.writer().write_all(&wu)?;
                            self.conn.writer().write_all(&ws)?;
                            self.flush_write()?;
                        }
                        body.extend_from_slice(&payload);
                        if flags & 0x1 != 0 { // END_STREAM
                            let t_done = Instant::now();
                            let us = |a: Instant, b: Instant| (b - a).as_secs_f64() * 1_000_000.0;
                            let t_fb = first_byte_time.unwrap_or(t_done);
                            return Ok((body,
                                us(t_req_start, t_req_sent),
                                us(t_req_sent,  t_fb),
                                us(t_fb,        t_done)));
                        }
                    }
                    0x1 if fsid == sid => { // HEADERS
                        if flags & 0x1 != 0 { // END_STREAM (no body)
                            let t_done = Instant::now();
                            let us = |a: Instant, b: Instant| (b - a).as_secs_f64() * 1_000_000.0;
                            let t_fb = first_byte_time.unwrap_or(t_done);
                            return Ok((body,
                                us(t_req_start, t_req_sent),
                                us(t_req_sent,  t_fb),
                                us(t_fb,        t_done)));
                        }
                    }
                    0x3 if fsid == sid => anyhow::bail!("server RST_STREAM"),
                    0x7 => anyhow::bail!("server GOAWAY"),
                    _ => {}
                }
                continue;
            }
            self.fill_recv_deadline(deadline).map_err(|e| anyhow::anyhow!("H2 read: {}", e))?;
        }
    }
}

const H2_MAX_REQS: usize = 1_000;

fn bench_h2_phases(addr: &str, path: &str, n: usize, warmup: usize, skip_verify: bool)
    -> anyhow::Result<PhaseSamples>
{
    let tls_cfg = make_tls_config_h2(skip_verify);
    let mut s = PhaseSamples::new(n);

    // Warmup on a dedicated connection (not counted)
    {
        let mut client = H2TimedClient::connect(addr, Arc::clone(&tls_cfg))?;
        for _ in 0..warmup {
            if client.requests_done >= H2_MAX_REQS {
                client = H2TimedClient::connect(addr, Arc::clone(&tls_cfg))?;
            }
            client.get_timed(path)?;
        }
    }

    // Measure: each sample includes connection setup phases
    // For n samples we only reconnect when needed; connection-setup phases go in every time
    // we open a new connection. To give a fair per-request view we collect per-request phases
    // on a warm persistent connection and connection setup phases from each new connection.
    let mut client = H2TimedClient::connect(addr, Arc::clone(&tls_cfg))?;
    // Record initial connection setup
    s.connect.push(client.connect_us);
    s.tls.push(client.tls_us);
    s.proto_setup.push(client.proto_setup_us);

    for _ in 0..n {
        if client.requests_done >= H2_MAX_REQS {
            client = H2TimedClient::connect(addr, Arc::clone(&tls_cfg))?;
            s.connect.push(client.connect_us);
            s.tls.push(client.tls_us);
            s.proto_setup.push(client.proto_setup_us);
        }
        let (_, req_send, ttfb, transfer) = client.get_timed(path)?;
        s.request_send.push(req_send);
        s.ttfb.push(ttfb);
        s.transfer.push(transfer);
        s.total.push(req_send + ttfb + transfer);
    }
    Ok(s)
}

// ── H2C (HTTP/2 cleartext) timed client ──────────────────────────────────────

/// Timed H2C client over plain TCP. Tracks connection-setup phases separately.
struct H2cTimedClient {
    stream:         TcpStream,
    send_buf:       Vec<u8>,
    recv_buf:       Vec<u8>,
    tmp:            [u8; 8192],
    next_stream_id: u32,
    requests_done:  usize,
    // Connection-setup timings
    pub connect_us:     f64,
    pub proto_setup_us: f64,
}

impl H2cTimedClient {
    fn connect(addr: &str) -> anyhow::Result<Self> {
        let t0 = Instant::now();
        let stream = TcpStream::connect(addr)?;
        stream.set_nodelay(true)?;
        let t_connected = Instant::now();

        // Send client preface + SETTINGS (blocking).
        stream.set_read_timeout(Some(Duration::from_secs(5)))?;
        (&stream).write_all(H2_CLIENT_PREFACE)?;
        (&stream).write_all(&make_h2_frame(0x4, 0x0, 0, &[]))?;

        // Read until server SETTINGS (non-ACK).
        let mut recv_buf: Vec<u8> = Vec::with_capacity(16_384);
        let mut tmp = [0u8; 8192];
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut got_settings = false;
        while !got_settings {
            if Instant::now() > deadline { anyhow::bail!("H2C setup timeout"); }
            let n = (&stream).read(&mut tmp)?;
            if n == 0 { anyhow::bail!("H2C: closed during setup"); }
            recv_buf.extend_from_slice(&tmp[..n]);
            while let Some((ftype, flags, _, total)) = try_parse_h2_frame(&recv_buf) {
                if ftype == 0x4 && flags & 0x1 == 0 { got_settings = true; }
                recv_buf.drain(..total);
            }
        }
        // Send SETTINGS ACK.
        (&stream).write_all(&make_h2_frame(0x4, 0x1, 0, &[]))?;
        let t_proto = Instant::now();

        stream.set_nonblocking(true)?;
        stream.set_read_timeout(None)?;

        let us = |a: Instant, b: Instant| (b - a).as_secs_f64() * 1_000_000.0;
        Ok(H2cTimedClient {
            stream,
            send_buf:       Vec::with_capacity(4096),
            recv_buf,
            tmp:            [0u8; 8192],
            next_stream_id: 1,
            requests_done:  0,
            connect_us:     us(t0,          t_connected),
            proto_setup_us: us(t_connected, t_proto),
        })
    }

    fn flush_write(&mut self) -> io::Result<()> {
        loop {
            if self.send_buf.is_empty() { break; }
            match (&self.stream).write(&self.send_buf) {
                Ok(0) => break,
                Ok(n) => { self.send_buf.drain(..n); }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    fn fill_recv_deadline(&mut self, deadline: Instant) -> io::Result<()> {
        loop {
            match (&self.stream).read(&mut self.tmp) {
                Ok(0) => return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "closed")),
                Ok(n) => { self.recv_buf.extend_from_slice(&self.tmp[..n]); break; }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        return Err(io::Error::new(io::ErrorKind::TimedOut, "deadline"));
                    }
                    self.flush_write()?;
                    let ms = remaining.as_millis().min(100) as i32;
                    unsafe {
                        let mut pfd = libc::pollfd {
                            fd: self.stream.as_raw_fd(), events: libc::POLLIN, revents: 0,
                        };
                        libc::poll(&mut pfd, 1, ms);
                    }
                }
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    /// Send GET, return (body, request_send_us, ttfb_us, transfer_us).
    fn get_timed(&mut self, path: &str) -> anyhow::Result<(Vec<u8>, f64, f64, f64)> {
        let sid = self.next_stream_id;
        self.next_stream_id += 2;
        self.requests_done += 1;

        let headers = make_h2c_get_headers(path);
        let frame = make_h2_frame(0x1, 0x05, sid, &headers);

        let t_req_start = Instant::now();
        self.send_buf.extend_from_slice(&frame);
        self.flush_write()?;
        let t_req_sent = Instant::now();

        let mut body = Vec::new();
        let mut first_byte_time: Option<Instant> = None;
        let deadline = Instant::now() + Duration::from_secs(5);

        loop {
            if Instant::now() > deadline { anyhow::bail!("H2C response timeout"); }

            if let Some((ftype, flags, fsid, total)) = try_parse_h2_frame(&self.recv_buf) {
                let payload = self.recv_buf[9..total].to_vec();
                self.recv_buf.drain(..total);

                if fsid == sid && first_byte_time.is_none() {
                    first_byte_time = Some(Instant::now());
                }

                match ftype {
                    0x0 if fsid == sid => {
                        if !payload.is_empty() {
                            let inc = payload.len() as u32;
                            self.send_buf.extend_from_slice(&make_h2_frame(0x8, 0x0, 0, &inc.to_be_bytes()));
                            self.send_buf.extend_from_slice(&make_h2_frame(0x8, 0x0, sid, &inc.to_be_bytes()));
                            self.flush_write()?;
                        }
                        body.extend_from_slice(&payload);
                        if flags & 0x1 != 0 {
                            let t_done = Instant::now();
                            let us = |a: Instant, b: Instant| (b - a).as_secs_f64() * 1_000_000.0;
                            let t_fb = first_byte_time.unwrap_or(t_done);
                            return Ok((body, us(t_req_start, t_req_sent), us(t_req_sent, t_fb), us(t_fb, t_done)));
                        }
                    }
                    0x1 if fsid == sid => {
                        if flags & 0x1 != 0 {
                            let t_done = Instant::now();
                            let us = |a: Instant, b: Instant| (b - a).as_secs_f64() * 1_000_000.0;
                            let t_fb = first_byte_time.unwrap_or(t_done);
                            return Ok((body, us(t_req_start, t_req_sent), us(t_req_sent, t_fb), us(t_fb, t_done)));
                        }
                    }
                    0x3 if fsid == sid => anyhow::bail!("server RST_STREAM"),
                    0x7 => anyhow::bail!("server GOAWAY"),
                    _ => {}
                }
                continue;
            }
            self.fill_recv_deadline(deadline).map_err(|e| anyhow::anyhow!("H2C read: {e}"))?;
        }
    }
}

const H2C_MAX_REQS: usize = 1_000;

fn bench_h2c_phases(addr: &str, path: &str, n: usize, warmup: usize)
    -> anyhow::Result<PhaseSamples>
{
    let mut s = PhaseSamples::new(n);

    // Warmup on a dedicated connection (not counted).
    {
        let mut client = H2cTimedClient::connect(addr)?;
        for _ in 0..warmup {
            if client.requests_done >= H2C_MAX_REQS {
                client = H2cTimedClient::connect(addr)?;
            }
            client.get_timed(path)?;
        }
    }

    let mut client = H2cTimedClient::connect(addr)?;
    s.connect.push(client.connect_us);
    s.proto_setup.push(client.proto_setup_us);
    // No TLS phase for H2C (tls vec stays empty → filtered out by into_stats)

    for _ in 0..n {
        if client.requests_done >= H2C_MAX_REQS {
            client = H2cTimedClient::connect(addr)?;
            s.connect.push(client.connect_us);
            s.proto_setup.push(client.proto_setup_us);
        }
        let (_, req_send, ttfb, transfer) = client.get_timed(path)?;
        s.request_send.push(req_send);
        s.ttfb.push(ttfb);
        s.transfer.push(transfer);
        s.total.push(req_send + ttfb + transfer);
    }
    Ok(s)
}

fn bench_h2c_fullpage(addr: &str, html_path: &str, n: usize, warmup: usize)
    -> anyhow::Result<FullPageSamples>
{
    // Discover assets.
    let mut client = H2cTimedClient::connect(addr)?;
    let (html_body, _, _, _) = client.get_timed(html_path)?;
    let assets = extract_assets(&html_body);

    // Warm cache.
    for _ in 0..warmup {
        if client.requests_done >= H2C_MAX_REQS {
            client = H2cTimedClient::connect(addr)?;
        }
        client.get_timed(html_path)?;
        for asset in &assets {
            if client.requests_done >= H2C_MAX_REQS {
                client = H2cTimedClient::connect(addr)?;
            }
            client.get_timed(asset)?;
        }
    }

    let mut s = FullPageSamples::new(n);

    for _ in 0..n {
        let reqs_needed = 1 + assets.len();
        if client.requests_done + reqs_needed > H2C_MAX_REQS {
            client = H2cTimedClient::connect(addr)?;
        }

        let t_start = Instant::now();
        client.get_timed(html_path)?;
        let t_html = Instant::now();

        for asset in &assets {
            client.get_timed(asset)?;
        }
        let t_done = Instant::now();

        let us = |a: Instant, b: Instant| (b - a).as_secs_f64() * 1_000_000.0;
        s.connect.push(0.0); // persistent — no per-sample connection cost
        s.tls.push(0.0);
        s.html_done.push(us(t_start, t_html));
        s.assets_done.push(us(t_start, t_done));
        s.total.push(us(t_start, t_done));
    }
    Ok(s)
}

// ── H3 per-phase timed GET ────────────────────────────────────────────────────

fn new_scid() -> quiche::ConnectionId<'static> {
    let mut scid = [0u8; quiche::MAX_CONN_ID_LEN];
    rand::thread_rng().fill(&mut scid);
    quiche::ConnectionId::from_vec(scid.to_vec())
}

fn quic_flush(conn: &mut quiche::Connection, udp: &UdpSocket) {
    let mut out = [0u8; 1350];
    loop {
        match conn.send(&mut out) {
            Ok((len, _)) => { udp.send(&out[..len]).ok(); }
            Err(quiche::Error::Done) => break,
            Err(e) => { eprintln!("quic_flush: {e}"); break; }
        }
    }
}

/// Connect QUIC+H3. Returns (conn, h3, udp, quic_hs_us, h3_init_us).
fn h3_connect_timed(addr: &str, cfg: &mut quiche::Config)
    -> anyhow::Result<(quiche::Connection, quiche::h3::Connection, UdpSocket, f64, f64)>
{
    let udp = UdpSocket::bind("0.0.0.0:0")?;
    udp.connect(addr)?;
    let scid = new_scid();
    let peer: std::net::SocketAddr = addr.parse()?;
    let local = udp.local_addr()?;

    let t0 = Instant::now();
    let mut conn = quiche::connect(Some("localhost"), &scid, local, peer, cfg)?;
    quic_flush(&mut conn, &udp);

    let mut buf = [0u8; 65535];
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if Instant::now() > deadline { anyhow::bail!("QUIC handshake timeout"); }
        conn.on_timeout();
        quic_flush(&mut conn, &udp);
        udp.set_read_timeout(Some(Duration::from_millis(100)))?;
        let n = match udp.recv(&mut buf) {
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock
                   || e.kind() == io::ErrorKind::TimedOut => continue,
            Err(e) => return Err(e.into()),
        };
        conn.recv(&mut buf[..n], quiche::RecvInfo { from: peer, to: local })?;
        quic_flush(&mut conn, &udp);
        if conn.is_established() { break; }
        if conn.is_closed() { anyhow::bail!("QUIC closed during handshake"); }
    }
    let t_quic = Instant::now();

    let h3_cfg = quiche::h3::Config::new()?;
    let h3 = quiche::h3::Connection::with_transport(&mut conn, &h3_cfg)?;
    let t_h3 = Instant::now();

    let us = |a: Instant, b: Instant| (b - a).as_secs_f64() * 1_000_000.0;
    Ok((conn, h3, udp, us(t0, t_quic), us(t_quic, t_h3)))
}

/// Single H3 GET with per-phase timing. Returns (body, req_send_us, ttfb_us, transfer_us).
fn h3_timed_get(
    conn: &mut quiche::Connection,
    h3:   &mut quiche::h3::Connection,
    udp:  &UdpSocket,
    path: &[u8],
) -> anyhow::Result<(Vec<u8>, f64, f64, f64)> {
    let req = vec![
        quiche::h3::Header::new(b":method",    b"GET"),
        quiche::h3::Header::new(b":path",      path),
        quiche::h3::Header::new(b":scheme",    b"https"),
        quiche::h3::Header::new(b":authority", b"localhost"),
    ];

    let t_req_start = Instant::now();
    let stream_id = h3.send_request(conn, &req, true)?;
    quic_flush(conn, udp);
    let t_req_sent = Instant::now();

    let mut body = Vec::new();
    let mut buf  = [0u8; 65535];
    let mut first_event_time: Option<Instant> = None;
    let deadline = Instant::now() + Duration::from_secs(5);

    loop {
        if Instant::now() > deadline { anyhow::bail!("H3 timeout"); }
        conn.on_timeout();
        quic_flush(conn, udp);
        udp.set_read_timeout(Some(Duration::from_millis(100)))?;
        let n = match udp.recv(&mut buf) {
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock
                   || e.kind() == io::ErrorKind::TimedOut => {
                if conn.is_closed() { break; }
                continue;
            }
            Err(e) => return Err(e.into()),
        };
        conn.recv(&mut buf[..n], quiche::RecvInfo {
            from: udp.peer_addr()?,
            to:   udp.local_addr()?,
        })?;
        quic_flush(conn, udp);

        loop {
            match h3.poll(conn) {
                Ok((sid, quiche::h3::Event::Headers { .. })) if sid == stream_id => {
                    if first_event_time.is_none() { first_event_time = Some(Instant::now()); }
                }
                Ok((sid, quiche::h3::Event::Data)) => {
                    if first_event_time.is_none() { first_event_time = Some(Instant::now()); }
                    while let Ok(read) = h3.recv_body(conn, sid, &mut buf) {
                        if sid == stream_id { body.extend_from_slice(&buf[..read]); }
                    }
                }
                Ok((sid, quiche::h3::Event::Finished)) if sid == stream_id => {
                    let t_done = Instant::now();
                    let us = |a: Instant, b: Instant| (b - a).as_secs_f64() * 1_000_000.0;
                    let t_fb = first_event_time.unwrap_or(t_done);
                    return Ok((body,
                        us(t_req_start, t_req_sent),
                        us(t_req_sent,  t_fb),
                        us(t_fb,        t_done)));
                }
                Ok(_) => {}
                Err(quiche::h3::Error::Done) => break,
                Err(e) => return Err(e.into()),
            }
        }
        if conn.is_closed() { break; }
    }
    anyhow::bail!("H3 connection closed before response finished")
}

const H3_MAX_STREAMS: usize = 90;

fn bench_h3_phases(addr: &str, path: &str, n: usize, warmup: usize, skip_verify: bool)
    -> anyhow::Result<PhaseSamples>
{
    let mut cfg = make_quiche_cfg(skip_verify);
    let mut s   = PhaseSamples::new(n);
    let pb      = path.as_bytes().to_vec();

    let (mut conn, mut h3, mut udp, quic_us, h3_us) = h3_connect_timed(addr, &mut cfg)?;
    let mut reqs = 0usize;

    // Warmup (cache pre-heat)
    for _ in 0..warmup {
        if reqs >= H3_MAX_STREAMS {
            let (c, h, u, _, _) = h3_connect_timed(addr, &mut cfg)?;
            conn = c; h3 = h; udp = u; reqs = 0;
        }
        h3_timed_get(&mut conn, &mut h3, &udp, &pb)?;
        reqs += 1;
    }

    // Record connection phases from the first post-warmup connection
    s.connect.push(quic_us);
    s.tls.push(0.0);         // QUIC hs includes TLS; reported as connect
    s.proto_setup.push(h3_us);

    for _ in 0..n {
        if reqs >= H3_MAX_STREAMS {
            let (c, h, u, qus, h3us) = h3_connect_timed(addr, &mut cfg)?;
            conn = c; h3 = h; udp = u; reqs = 0;
            s.connect.push(qus);
            s.proto_setup.push(h3us);
        }
        let (_, req_send, ttfb, transfer) = h3_timed_get(&mut conn, &mut h3, &udp, &pb)?;
        s.request_send.push(req_send);
        s.ttfb.push(ttfb);
        s.transfer.push(transfer);
        s.total.push(req_send + ttfb + transfer);
        reqs += 1;
    }
    Ok(s)
}

// ── HTML asset extractor ──────────────────────────────────────────────────────

/// Extract linked asset URLs from HTML (href/src attributes on absolute paths).
/// Mirrors the logic in hints.rs so the bench sees the same assets.
fn extract_assets(html: &[u8]) -> Vec<String> {
    const ASSET_EXTS: &[&str] = &[
        ".css", ".js", ".woff2", ".woff", ".png", ".jpg", ".jpeg",
        ".webp", ".gif", ".svg", ".ico",
    ];

    let mut out = Vec::new();
    let mut i   = 0usize;

    while i < html.len() {
        // Match href= or src= (both quoted and unquoted values — minified HTML omits quotes)
        let skip = if html[i..].starts_with(b"href=") { 5 }
                   else if html[i..].starts_with(b"src=") { 4 }
                   else { i += 1; continue; };
        i += skip;
        if i >= html.len() { break; }

        // Skip optional opening quote
        let quoted = html[i] == b'"' || html[i] == b'\'';
        let close  = if quoted { let q = html[i]; i += 1; q } else { b' ' };
        let start  = i;

        // Read until closing quote, whitespace, or '>'
        while i < html.len() {
            let b = html[i];
            if quoted  && b == close { break; }
            if !quoted && (b == b' ' || b == b'\t' || b == b'\r' || b == b'\n' || b == b'>' || b == b'"' || b == b'\'') { break; }
            i += 1;
        }

        let url = std::str::from_utf8(&html[start..i]).unwrap_or("");
        if url.starts_with('/') && !url.starts_with("//") {
            let path_only = url.find('?').map_or(url, |q| &url[..q]);
            if ASSET_EXTS.iter().any(|ext| path_only.ends_with(ext)) {
                out.push(url.to_string());
            }
        }
        i += 1;
    }
    out.sort();
    out.dedup();
    out
}

// ── Full-page-load benchmarks ─────────────────────────────────────────────────

fn bench_h1_fullpage(addr: &str, html_path: &str, n: usize, warmup: usize, tls_cfg: Arc<ClientConfig>)
    -> anyhow::Result<FullPageSamples>
{
    // Discover assets from a pre-warmup fetch
    let (html_body, _, _, _, _, _) = h1_timed_get(addr, html_path, Arc::clone(&tls_cfg))?;
    let assets = extract_assets(&html_body);

    // Warm cache: fetch HTML + all assets
    for _ in 0..warmup {
        h1_timed_get(addr, html_path, Arc::clone(&tls_cfg))?;
        for asset in &assets {
            h1_timed_get(addr, asset, Arc::clone(&tls_cfg))?;
        }
    }

    let mut s = FullPageSamples::new(n);

    for _ in 0..n {
        let t_start = Instant::now();
        let (_, connect, tls, _, _, _) = h1_timed_get(addr, html_path, Arc::clone(&tls_cfg))?;
        let t_html = Instant::now();

        // Fetch each asset sequentially (H1 = one req per connection)
        for asset in &assets {
            h1_timed_get(addr, asset, Arc::clone(&tls_cfg))?;
        }
        let t_done = Instant::now();

        let us = |a: Instant, b: Instant| (b - a).as_secs_f64() * 1_000_000.0;
        s.connect.push(connect);
        s.tls.push(tls);
        s.html_done.push(us(t_start, t_html));
        s.assets_done.push(us(t_start, t_done));
        s.total.push(us(t_start, t_done));
    }
    Ok(s)
}

fn bench_h2_fullpage(addr: &str, html_path: &str, n: usize, warmup: usize, skip_verify: bool)
    -> anyhow::Result<FullPageSamples>
{
    let tls_cfg = make_tls_config_h2(skip_verify);

    // Discover assets
    let mut client = H2TimedClient::connect(addr, Arc::clone(&tls_cfg))?;
    let (html_body, _, _, _) = client.get_timed(html_path)?;
    let assets = extract_assets(&html_body);

    // Warm cache on a persistent connection
    for _ in 0..warmup {
        if client.requests_done >= H2_MAX_REQS {
            client = H2TimedClient::connect(addr, Arc::clone(&tls_cfg))?;
        }
        client.get_timed(html_path)?;
        for asset in &assets {
            if client.requests_done >= H2_MAX_REQS {
                client = H2TimedClient::connect(addr, Arc::clone(&tls_cfg))?;
            }
            client.get_timed(asset)?;
        }
    }

    let mut s = FullPageSamples::new(n);

    for _ in 0..n {
        // Persistent connection: only reconnect between samples when near stream limit.
        // This avoids including connection-setup cost in per-page-load timing.
        let reqs_needed = 1 + assets.len();
        if client.requests_done + reqs_needed > H2_MAX_REQS {
            client = H2TimedClient::connect(addr, Arc::clone(&tls_cfg))?;
        }

        let t_start = Instant::now();
        client.get_timed(html_path)?;
        let t_html = Instant::now();

        for asset in &assets {
            client.get_timed(asset)?;
        }
        let t_done = Instant::now();

        let us = |a: Instant, b: Instant| (b - a).as_secs_f64() * 1_000_000.0;
        s.connect.push(0.0); // persistent — no per-sample connection cost
        s.tls.push(0.0);
        s.html_done.push(us(t_start, t_html));
        s.assets_done.push(us(t_start, t_done));
        s.total.push(us(t_start, t_done));
    }
    Ok(s)
}

fn bench_h3_fullpage(addr: &str, html_path: &str, n: usize, warmup: usize, skip_verify: bool)
    -> anyhow::Result<FullPageSamples>
{
    let mut cfg = make_quiche_cfg(skip_verify);

    // Discover assets
    let (mut conn, mut h3, mut udp, _, _) = h3_connect_timed(addr, &mut cfg)?;
    let (html_body, _, _, _) = h3_timed_get(&mut conn, &mut h3, &udp, html_path.as_bytes())?;
    let assets = extract_assets(&html_body);
    let mut reqs = 1usize;

    // Warm cache on a persistent connection
    for _ in 0..warmup {
        if reqs >= H3_MAX_STREAMS {
            let (c, h, u, _, _) = h3_connect_timed(addr, &mut cfg)?;
            conn = c; h3 = h; udp = u; reqs = 0;
        }
        h3_timed_get(&mut conn, &mut h3, &udp, html_path.as_bytes())?;
        reqs += 1;
        for asset in &assets {
            if reqs >= H3_MAX_STREAMS {
                let (c, h, u, _, _) = h3_connect_timed(addr, &mut cfg)?;
                conn = c; h3 = h; udp = u; reqs = 0;
            }
            h3_timed_get(&mut conn, &mut h3, &udp, asset.as_bytes())?;
            reqs += 1;
        }
    }

    let mut s = FullPageSamples::new(n);
    let assets_owned = assets;

    for _ in 0..n {
        // Persistent connection: only reconnect between samples when near stream limit.
        // This avoids including QUIC handshake cost in per-page-load timing.
        let reqs_needed = 1 + assets_owned.len();
        if reqs + reqs_needed > H3_MAX_STREAMS {
            let (c, h, u, _, _) = h3_connect_timed(addr, &mut cfg)?;
            conn = c; h3 = h; udp = u; reqs = 0;
        }

        let t_start = Instant::now();
        h3_timed_get(&mut conn, &mut h3, &udp, html_path.as_bytes())?;
        reqs += 1;
        let t_html = Instant::now();

        for asset in &assets_owned {
            h3_timed_get(&mut conn, &mut h3, &udp, asset.as_bytes())?;
            reqs += 1;
        }
        let t_done = Instant::now();

        let us = |a: Instant, b: Instant| (b - a).as_secs_f64() * 1_000_000.0;
        s.connect.push(0.0); // persistent — no per-sample connection cost
        s.tls.push(0.0);
        s.html_done.push(us(t_start, t_html));
        s.assets_done.push(us(t_start, t_done));
        s.total.push(us(t_start, t_done));
    }
    Ok(s)
}

// ── Text reporter ─────────────────────────────────────────────────────────────

fn print_stats(stats: &[BoxStats]) {
    if stats.is_empty() { return; }
    println!("{:<35} {:>7} {:>8} {:>8} {:>8} {:>8} {:>8} {:>8} {:>8} {:>8}",
        "phase", "n", "p5", "p25", "p50", "p75", "p95", "mean", "stddev", "max");
    println!("{}", "-".repeat(110));
    for s in stats {
        println!("{:<35} {:>7} {:>8.1} {:>8.1} {:>8.1} {:>8.1} {:>8.1} {:>8.1} {:>8.1} {:>8.1}  µs",
            s.label, s.n, s.p5, s.p25, s.p50, s.p75, s.p95, s.mean, s.stddev, s.max);
    }
    println!();
}

// ── SVG box-and-whisker chart ─────────────────────────────────────────────────
//
// Layout (horizontal box-and-whisker, one row per phase):
//   Left margin: 220px for labels
//   Plot area:   900px wide
//   Row height:  44px (box 22px high, 11px centred)
//   Bottom:      50px for x-axis
//   Top:         50px for title

fn write_boxwhisker_svg(stats: &[BoxStats], title: &str, path: &str) -> std::io::Result<()> {
    if stats.is_empty() { return Ok(()); }

    let label_w: f64 = 220.0;
    let plot_w:  f64 = 900.0;
    let row_h:   f64 = 44.0;
    let top:     f64 = 60.0;
    let bottom:  f64 = 60.0;
    let box_h:   f64 = 20.0;
    let canvas_w = (label_w + plot_w + 40.0) as u32;
    let canvas_h = (top + row_h * stats.len() as f64 + bottom) as u32;

    // Determine x-axis range: 0 to max of all p95 values (with padding)
    let x_max = stats.iter().map(|s| s.p95).fold(0.0_f64, f64::max) * 1.15;
    let x_max = if x_max == 0.0 { 1.0 } else { x_max };

    let to_x = |v: f64| label_w + (v / x_max) * plot_w;

    // SVG helper — avoids r#"..."# raw strings which terminate on `"#` inside SVG hex colours.
    macro_rules! el {
        ($svg:expr, $($arg:tt)*) => { $svg.push_str(&format!($($arg)*)); $svg.push('\n'); }
    }

    let mut svg = String::with_capacity(32_768);
    el!(svg, "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{}\" height=\"{}\" \
              font-family=\"monospace\" font-size=\"13\">", canvas_w, canvas_h);

    // Background
    el!(svg, "<rect width=\"{}\" height=\"{}\" fill=\"{}\"/>", canvas_w, canvas_h, "#f8f9fa");

    // Title
    el!(svg, "<text x=\"{:.1}\" y=\"30\" font-size=\"16\" font-weight=\"bold\" \
              fill=\"{}\" text-anchor=\"middle\">{}</text>",
        canvas_w as f64 / 2.0, "#222", title);

    // X-axis
    let n_ticks = 5usize;
    let axis_y = top + row_h * stats.len() as f64;
    el!(svg, "<line x1=\"{:.1}\" y1=\"{:.1}\" x2=\"{:.1}\" y2=\"{:.1}\" \
              stroke=\"{}\" stroke-width=\"1\"/>",
        to_x(0.0), axis_y, to_x(x_max), axis_y, "#aaa");
    for t in 0..=n_ticks {
        let v  = x_max * t as f64 / n_ticks as f64;
        let tx = to_x(v);
        el!(svg, "<line x1=\"{:.1}\" y1=\"{:.1}\" x2=\"{:.1}\" y2=\"{:.1}\" \
                  stroke=\"{}\" stroke-width=\"1\"/>",
            tx, axis_y, tx, axis_y + 5.0, "#aaa");
        el!(svg, "<text x=\"{:.1}\" y=\"{:.1}\" text-anchor=\"middle\" fill=\"{}\">{:.0}µs</text>",
            tx, axis_y + 18.0, "#555", v);
    }
    el!(svg, "<text x=\"{:.1}\" y=\"{:.1}\" text-anchor=\"middle\" fill=\"{}\" \
              font-size=\"12\">Latency (µs)</text>",
        label_w + plot_w / 2.0, axis_y + 36.0, "#555");

    // Grid lines
    for t in 0..=n_ticks {
        let v  = x_max * t as f64 / n_ticks as f64;
        let tx = to_x(v);
        el!(svg, "<line x1=\"{:.1}\" y1=\"{:.1}\" x2=\"{:.1}\" y2=\"{:.1}\" \
                  stroke=\"{}\" stroke-width=\"1\" stroke-dasharray=\"4,4\"/>",
            tx, top, tx, axis_y, "#ddd");
    }

    // One row per phase
    for (i, s) in stats.iter().enumerate() {
        let cy           = top + row_h * i as f64 + row_h / 2.0;
        let y_box_top    = cy - box_h / 2.0;
        let y_box_bottom = cy + box_h / 2.0;

        // Alternating row background
        if i % 2 == 0 {
            el!(svg, "<rect x=\"0\" y=\"{:.1}\" width=\"{}\" height=\"{:.1}\" \
                      fill=\"{}\" opacity=\"0.5\"/>",
                top + row_h * i as f64, canvas_w, row_h, "#eef0f4");
        }

        // Phase label (strip "proto/" prefix for display)
        let display_label = s.label.split('/').last().unwrap_or(&s.label);
        el!(svg, "<text x=\"{:.1}\" y=\"{:.1}\" text-anchor=\"end\" fill=\"{}\" \
                  dominant-baseline=\"middle\">{}</text>",
            label_w - 8.0, cy, "#333", display_label);

        // Whisker line: p5 → p95
        el!(svg, "<line x1=\"{:.1}\" y1=\"{:.1}\" x2=\"{:.1}\" y2=\"{:.1}\" \
                  stroke=\"{}\" stroke-width=\"2\"/>",
            to_x(s.p5), cy, to_x(s.p95), cy, "#6699cc");
        // Whisker caps
        for &v in &[s.p5, s.p95] {
            let vx = to_x(v);
            el!(svg, "<line x1=\"{:.1}\" y1=\"{:.1}\" x2=\"{:.1}\" y2=\"{:.1}\" \
                      stroke=\"{}\" stroke-width=\"2\"/>",
                vx, y_box_top, vx, y_box_bottom, "#6699cc");
        }

        // IQR box (Q1 → Q3)
        let bx     = to_x(s.p25);
        let bwidth = (to_x(s.p75) - bx).max(1.0);
        el!(svg, "<rect x=\"{:.1}\" y=\"{:.1}\" width=\"{:.1}\" height=\"{:.1}\" \
                  fill=\"{}\" opacity=\"0.7\" rx=\"2\"/>",
            bx, y_box_top, bwidth, box_h, "#4488cc");

        // Median line
        let mx = to_x(s.p50);
        el!(svg, "<line x1=\"{:.1}\" y1=\"{:.1}\" x2=\"{:.1}\" y2=\"{:.1}\" \
                  stroke=\"{}\" stroke-width=\"2.5\"/>",
            mx, y_box_top, mx, y_box_bottom, "#fff");

        // Mean diamond
        let mnx = to_x(s.mean);
        let d   = 5.0_f64;
        el!(svg, "<polygon points=\"{:.1},{:.1} {:.1},{:.1} {:.1},{:.1} {:.1},{:.1}\" \
                  fill=\"{}\" opacity=\"0.9\"/>",
            mnx-d, cy, mnx, cy-d, mnx+d, cy, mnx, cy+d, "#ff6600");

        // p50 label centred in box
        el!(svg, "<text x=\"{:.1}\" y=\"{:.1}\" font-size=\"11\" fill=\"{}\" \
                  dominant-baseline=\"middle\" text-anchor=\"middle\">p50={:.0}</text>",
            (to_x(s.p25) + to_x(s.p75)) / 2.0, cy, "#ddf", s.p50);

        // p5 / p95 labels
        el!(svg, "<text x=\"{:.1}\" y=\"{:.1}\" font-size=\"10\" fill=\"{}\" \
                  text-anchor=\"middle\">{:.0}</text>",
            to_x(s.p5), y_box_top - 2.0, "#666", s.p5);
        el!(svg, "<text x=\"{:.1}\" y=\"{:.1}\" font-size=\"10\" fill=\"{}\" \
                  text-anchor=\"middle\">{:.0}</text>",
            to_x(s.p95), y_box_top - 2.0, "#666", s.p95);
    }

    // Legend
    let lx = label_w + plot_w + 5.0;
    let ly = top + 10.0;
    el!(svg, "<rect x=\"{:.0}\" y=\"{:.0}\" width=\"14\" height=\"14\" fill=\"{}\" opacity=\"0.7\"/>",
        lx, ly, "#4488cc");
    el!(svg, "<text x=\"{:.0}\" y=\"{:.0}\" font-size=\"11\" fill=\"{}\" \
              dominant-baseline=\"middle\">IQR (p25-p75)</text>", lx+18.0, ly+7.0, "#444");
    el!(svg, "<line x1=\"{:.0}\" y1=\"{:.0}\" x2=\"{:.0}\" y2=\"{:.0}\" \
              stroke=\"{}\" stroke-width=\"2\"/>", lx, ly+24.0, lx+14.0, ly+24.0, "#6699cc");
    el!(svg, "<text x=\"{:.0}\" y=\"{:.0}\" font-size=\"11\" fill=\"{}\" \
              dominant-baseline=\"middle\">p5-p95 whiskers</text>", lx+18.0, ly+24.0, "#444");
    let dlx = lx + 10.0; let dly = ly + 38.0;
    el!(svg, "<polygon points=\"{:.1},{:.1} {:.1},{:.1} {:.1},{:.1} {:.1},{:.1}\" \
              fill=\"{}\" opacity=\"0.9\"/>",
        dlx-5.0, dly, dlx, dly-5.0, dlx+5.0, dly, dlx, dly+5.0, "#ff6600");
    el!(svg, "<text x=\"{:.0}\" y=\"{:.0}\" font-size=\"11\" fill=\"{}\" \
              dominant-baseline=\"middle\">mean</text>", lx+18.0, dly, "#444");
    el!(svg, "<line x1=\"{:.0}\" y1=\"{:.0}\" x2=\"{:.0}\" y2=\"{:.0}\" \
              stroke=\"{}\" stroke-width=\"2.5\"/>", lx, ly+52.0, lx+14.0, ly+52.0, "#555");
    el!(svg, "<text x=\"{:.0}\" y=\"{:.0}\" font-size=\"11\" fill=\"{}\" \
              dominant-baseline=\"middle\">median (p50)</text>", lx+18.0, ly+52.0, "#444");

    svg.push_str("\n</svg>\n");
    std::fs::write(path, &svg)
}

// ── main ──────────────────────────────────────────────────────────────────────

fn main() {
    let args = Args::parse();
    // Cache warmup: 100 requests per path before measurement
    const WARMUP: usize = 100;
    const HTML_PATH: &str = "/";

    println!("m6-bench-detail  target={}  h2c-target={}  n={}  skip-verify={}  warmup={}",
             args.addr, args.h2c_addr, args.n, args.skip_verify, WARMUP);
    println!("{}", "=".repeat(110));

    // ── HTTP/1.1 ──────────────────────────────────────────────────────────────
    if args.http11 {
        println!("\n[HTTP/1.1] Per-phase breakdown (n={}, cache pre-warmed, {} warmup req discarded)",
                 args.n, WARMUP);
        let tls_cfg = make_tls_config_h1(args.skip_verify);
        match bench_h1_phases(&args.addr, HTML_PATH, args.n, WARMUP, Arc::clone(&tls_cfg)) {
            Ok(samples) => {
                let stats = samples.into_stats("HTTP/1.1");
                print_stats(&stats);
                let svg_path = format!("{}/m6_latency_h1_phases.svg", args.out_dir);
                if let Err(e) = write_boxwhisker_svg(&stats,
                    &format!("HTTP/1.1 Per-Phase Latency  (n={}, µs)", args.n), &svg_path) {
                    eprintln!("SVG write error: {e}");
                } else {
                    println!("Chart: {svg_path}");
                }
            }
            Err(e) => eprintln!("HTTP/1.1 phase bench error: {e}"),
        }

        println!("\n[HTTP/1.1] Full-page load (n={})", args.n);
        match bench_h1_fullpage(&args.addr, HTML_PATH, args.n, WARMUP, Arc::clone(&tls_cfg)) {
            Ok(samples) => {
                let stats = samples.into_stats("HTTP/1.1");
                print_stats(&stats);
                let svg_path = format!("{}/m6_latency_h1_fullpage.svg", args.out_dir);
                if let Err(e) = write_boxwhisker_svg(&stats,
                    &format!("HTTP/1.1 Full-Page Load  (n={}, µs)", args.n), &svg_path) {
                    eprintln!("SVG write error: {e}");
                } else {
                    println!("Chart: {svg_path}");
                }
            }
            Err(e) => eprintln!("HTTP/1.1 fullpage bench error: {e}"),
        }
    }

    // ── HTTP/2 ────────────────────────────────────────────────────────────────
    if args.http2 {
        println!("\n[HTTP/2] Per-phase breakdown (n={}, cache pre-warmed, {} warmup req discarded)",
                 args.n, WARMUP);
        match bench_h2_phases(&args.addr, HTML_PATH, args.n, WARMUP, args.skip_verify) {
            Ok(samples) => {
                let stats = samples.into_stats("HTTP/2");
                print_stats(&stats);
                let svg_path = format!("{}/m6_latency_h2_phases.svg", args.out_dir);
                if let Err(e) = write_boxwhisker_svg(&stats,
                    &format!("HTTP/2 Per-Phase Latency  (n={}, µs)", args.n), &svg_path) {
                    eprintln!("SVG write error: {e}");
                } else {
                    println!("Chart: {svg_path}");
                }
            }
            Err(e) => eprintln!("HTTP/2 phase bench error: {e}"),
        }

        println!("\n[HTTP/2] Full-page load (n={})", args.n);
        match bench_h2_fullpage(&args.addr, HTML_PATH, args.n, WARMUP, args.skip_verify) {
            Ok(samples) => {
                let stats = samples.into_stats("HTTP/2");
                print_stats(&stats);
                let svg_path = format!("{}/m6_latency_h2_fullpage.svg", args.out_dir);
                if let Err(e) = write_boxwhisker_svg(&stats,
                    &format!("HTTP/2 Full-Page Load  (n={}, µs)", args.n), &svg_path) {
                    eprintln!("SVG write error: {e}");
                } else {
                    println!("Chart: {svg_path}");
                }
            }
            Err(e) => eprintln!("HTTP/2 fullpage bench error: {e}"),
        }
    }

    // ── HTTP/3 ────────────────────────────────────────────────────────────────
    if args.http3 {
        println!("\n[HTTP/3] Per-phase breakdown (n={}, cache pre-warmed, {} warmup req discarded)",
                 args.n, WARMUP);
        match bench_h3_phases(&args.addr, HTML_PATH, args.n, WARMUP, args.skip_verify) {
            Ok(samples) => {
                let stats = samples.into_stats("HTTP/3");
                print_stats(&stats);
                let svg_path = format!("{}/m6_latency_h3_phases.svg", args.out_dir);
                if let Err(e) = write_boxwhisker_svg(&stats,
                    &format!("HTTP/3 Per-Phase Latency  (n={}, µs)", args.n), &svg_path) {
                    eprintln!("SVG write error: {e}");
                } else {
                    println!("Chart: {svg_path}");
                }
            }
            Err(e) => eprintln!("HTTP/3 phase bench error: {e}"),
        }

        println!("\n[HTTP/3] Full-page load (n={})", args.n);
        match bench_h3_fullpage(&args.addr, HTML_PATH, args.n, WARMUP, args.skip_verify) {
            Ok(samples) => {
                let stats = samples.into_stats("HTTP/3");
                print_stats(&stats);
                let svg_path = format!("{}/m6_latency_h3_fullpage.svg", args.out_dir);
                if let Err(e) = write_boxwhisker_svg(&stats,
                    &format!("HTTP/3 Full-Page Load  (n={}, µs)", args.n), &svg_path) {
                    eprintln!("SVG write error: {e}");
                } else {
                    println!("Chart: {svg_path}");
                }
            }
            Err(e) => eprintln!("HTTP/3 fullpage bench error: {e}"),
        }
    }

    // ── H2C ───────────────────────────────────────────────────────────────────
    if args.h2c {
        println!("\n[H2C] Per-phase breakdown (n={}, cache pre-warmed, {} warmup req discarded)",
                 args.n, WARMUP);
        match bench_h2c_phases(&args.h2c_addr, HTML_PATH, args.n, WARMUP) {
            Ok(samples) => {
                let stats = samples.into_stats("H2C");
                print_stats(&stats);
                let svg_path = format!("{}/m6_latency_h2c_phases.svg", args.out_dir);
                if let Err(e) = write_boxwhisker_svg(&stats,
                    &format!("H2C Per-Phase Latency  (n={}, µs)", args.n), &svg_path) {
                    eprintln!("SVG write error: {e}");
                } else {
                    println!("Chart: {svg_path}");
                }
            }
            Err(e) => eprintln!("H2C phase bench error: {e}"),
        }

        println!("\n[H2C] Full-page load (n={})", args.n);
        match bench_h2c_fullpage(&args.h2c_addr, HTML_PATH, args.n, WARMUP) {
            Ok(samples) => {
                let stats = samples.into_stats("H2C");
                print_stats(&stats);
                let svg_path = format!("{}/m6_latency_h2c_fullpage.svg", args.out_dir);
                if let Err(e) = write_boxwhisker_svg(&stats,
                    &format!("H2C Full-Page Load  (n={}, µs)", args.n), &svg_path) {
                    eprintln!("SVG write error: {e}");
                } else {
                    println!("Chart: {svg_path}");
                }
            }
            Err(e) => eprintln!("H2C fullpage bench error: {e}"),
        }
    }
}
