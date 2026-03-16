/// m6-bench — loopback latency + throughput benchmark for m6-http.
///
/// Modes (flags can be combined):
///   --http11-only       only run HTTP/1.1 suites
///   --http2-only        only run HTTP/2 suites
///   --http3-only        only run HTTP/3 suites
///   --skip-verify       skip TLS certificate verification
///   --latency-n N       requests per latency run (default 2000)
///   --duration S        throughput run duration in seconds (default 10)
///   --concurrency C     parallel threads for throughput (default 8)
///   --p99-limit-ms F    fail if p99 latency exceeds this (default 50)
///   --rps-min F         fail if throughput drops below this (default 50)
///   --addr HOST:PORT    target address (default 127.0.0.1:8443)
///
/// Output: one result line per suite; exits non-zero if any threshold exceeded.

use std::io::{Read, Write};
use std::net::{TcpStream, UdpSocket};
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use rand::Rng;
use rustls::ClientConfig;
use rustls::pki_types::{ServerName, CertificateDer, UnixTime};

// ── CLI ───────────────────────────────────────────────────────────────────────

struct Args {
    http11: bool,
    http2:  bool,
    http3:  bool,
    skip_verify:  bool,
    latency_n:    usize,
    duration_s:   u64,
    concurrency:  usize,
    p99_limit_ms: f64,
    rps_min:      f64,
    addr:         String,
}

impl Args {
    fn parse() -> Self {
        let mut a = Args {
            http11: true,
            http2:  true,
            http3:  true,
            skip_verify:  false,
            latency_n:    2000,
            duration_s:   10,
            concurrency:  8,
            p99_limit_ms: 50.0,
            rps_min:      50.0,
            addr: "127.0.0.1:8443".into(),
        };
        let raw: Vec<String> = std::env::args().skip(1).collect();
        let mut i = 0;
        while i < raw.len() {
            match raw[i].as_str() {
                "--http11-only"    => { a.http11 = true;  a.http2 = false; a.http3 = false; }
                "--http2-only"     => { a.http11 = false; a.http2 = true;  a.http3 = false; }
                "--http3-only"     => { a.http11 = false; a.http2 = false; a.http3 = true; }
                "--skip-verify"    => a.skip_verify = true,
                "--latency-n"      => { i += 1; a.latency_n   = raw[i].parse().expect("latency-n"); }
                "--duration"       => { i += 1; a.duration_s  = raw[i].parse().expect("duration"); }
                "--concurrency"    => { i += 1; a.concurrency  = raw[i].parse().expect("concurrency"); }
                "--p99-limit-ms"   => { i += 1; a.p99_limit_ms = raw[i].parse().expect("p99-limit-ms"); }
                "--rps-min"        => { i += 1; a.rps_min      = raw[i].parse().expect("rps-min"); }
                "--addr"           => { i += 1; a.addr = raw[i].clone(); }
                other => { eprintln!("Unknown flag: {other}"); std::process::exit(1); }
            }
            i += 1;
        }
        a
    }
}

// ── TLS client config ─────────────────────────────────────────────────────────

// No-op verifier for --skip-verify.
#[derive(Debug)]
struct NoVerify;

impl rustls::client::danger::ServerCertVerifier for NoVerify {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

fn make_client_config(skip_verify: bool) -> Arc<ClientConfig> {
    if skip_verify {
        let cfg = ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(NoVerify))
            .with_no_client_auth();
        Arc::new(cfg)
    } else {
        let mut roots = rustls::RootCertStore::empty();
        for cert in rustls_native_certs::load_native_certs().certs {
            roots.add(cert).ok();
        }
        let cfg = ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        Arc::new(cfg)
    }
}

// ── HTTP/1.1 helpers ──────────────────────────────────────────────────────────

fn http11_get(addr: &str, tls_cfg: Arc<ClientConfig>) -> anyhow::Result<Vec<u8>> {
    let stream = TcpStream::connect(addr)?;
    stream.set_nodelay(true)?;
    let server_name: ServerName<'static> = "localhost".try_into().unwrap();
    let mut conn = rustls::ClientConnection::new(tls_cfg, server_name)?;
    let mut stream_ref = &stream;
    let mut tls = rustls::Stream::new(&mut conn, &mut stream_ref);
    let req = b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
    tls.write_all(req)?;
    let mut resp = Vec::new();
    // m6-http closes with TCP FIN rather than TLS close_notify; allow that.
    match tls.read_to_end(&mut resp) {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof && !resp.is_empty() => {}
        Err(e) => return Err(e.into()),
    }
    Ok(resp)
}

fn parse_http11_status(resp: &[u8]) -> u16 {
    // "HTTP/1.1 200 ..."
    if resp.len() < 12 { return 0; }
    let s = std::str::from_utf8(&resp[9..12]).unwrap_or("0");
    s.parse().unwrap_or(0)
}

// ── Percentile helper ─────────────────────────────────────────────────────────

fn percentile(mut v: Vec<f64>, pct: f64) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    if v.is_empty() { return 0.0; }
    let idx = ((pct / 100.0) * (v.len() as f64 - 1.0)).round() as usize;
    v[idx.min(v.len() - 1)]
}

// ── HTTP/1.1 latency ──────────────────────────────────────────────────────────

fn bench_http11_latency(addr: &str, n: usize, tls_cfg: Arc<ClientConfig>) -> anyhow::Result<Vec<f64>> {
    let mut latencies = Vec::with_capacity(n);
    // Warmup
    for _ in 0..5 {
        http11_get(addr, Arc::clone(&tls_cfg))?;
    }
    for _ in 0..n {
        let t0 = Instant::now();
        let resp = http11_get(addr, Arc::clone(&tls_cfg))?;
        let elapsed = t0.elapsed().as_secs_f64() * 1000.0;
        let status = parse_http11_status(&resp);
        if status != 200 {
            eprintln!("HTTP/1.1 non-200: {status}");
        }
        latencies.push(elapsed);
    }
    Ok(latencies)
}

// ── HTTP/1.1 throughput ───────────────────────────────────────────────────────

fn bench_http11_throughput(addr: &str, duration_s: u64, concurrency: usize, tls_cfg: Arc<ClientConfig>) -> anyhow::Result<f64> {
    let count = Arc::new(AtomicUsize::new(0));
    let deadline = Instant::now() + Duration::from_secs(duration_s);
    let addr = Arc::new(addr.to_string());

    let handles: Vec<_> = (0..concurrency).map(|_| {
        let tls_cfg = Arc::clone(&tls_cfg);
        let count = Arc::clone(&count);
        let addr = Arc::clone(&addr);
        std::thread::spawn(move || {
            while Instant::now() < deadline {
                if http11_get(&addr, Arc::clone(&tls_cfg)).is_ok() {
                    count.fetch_add(1, Ordering::Relaxed);
                }
            }
        })
    }).collect();
    let t0 = Instant::now();
    for h in handles { h.join().ok(); }
    let elapsed = t0.elapsed().as_secs_f64();
    let completed = count.load(Ordering::Relaxed);
    Ok(completed as f64 / elapsed)
}

// ── HTTP/2 helpers ────────────────────────────────────────────────────────────

const H2_CLIENT_PREFACE: &[u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";

// HPACK static-table encoded: :method GET, :path /, :scheme https, :authority localhost
const H2_HEADERS_GET_ROOT: &[u8] = &[
    0x82, 0x84, 0x87,
    0x41, 0x09, b'l', b'o', b'c', b'a', b'l', b'h', b'o', b's', b't',
];

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

/// Try to parse one H2 frame from the start of `buf`.
/// Returns `(ftype, flags, stream_id, payload_end)` where `payload_end` is the
/// byte offset after the frame (i.e. how many bytes to drain from buf).
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

fn make_client_config_h2(skip_verify: bool) -> Arc<ClientConfig> {
    let mut cfg = if skip_verify {
        ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(NoVerify))
            .with_no_client_auth()
    } else {
        let mut roots = rustls::RootCertStore::empty();
        for cert in rustls_native_certs::load_native_certs().certs { roots.add(cert).ok(); }
        ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth()
    };
    cfg.alpn_protocols = vec![b"h2".to_vec()];
    Arc::new(cfg)
}

/// Persistent HTTP/2 client over TLS — reuses one connection across many requests.
struct H2Client {
    tls:            rustls::StreamOwned<rustls::ClientConnection, TcpStream>,
    recv_buf:       Vec<u8>,
    tmp:            [u8; 8192],
    next_stream_id: u32,
    requests_done:  usize,
}

impl H2Client {
    fn connect(addr: &str, tls_cfg: Arc<ClientConfig>) -> anyhow::Result<Self> {
        let stream = TcpStream::connect(addr)?;
        stream.set_nodelay(true)?;
        stream.set_read_timeout(Some(Duration::from_secs(5)))?;
        let server_name: ServerName<'static> = "localhost".try_into().unwrap();
        let conn = rustls::ClientConnection::new(tls_cfg, server_name)?;
        let mut tls = rustls::StreamOwned::new(conn, stream);
        tls.write_all(H2_CLIENT_PREFACE)?;
        tls.write_all(&make_h2_frame(0x4, 0x0, 0, &[]))?; // empty SETTINGS
        tls.flush()?;
        Ok(H2Client {
            tls,
            recv_buf:       Vec::with_capacity(16_384),
            tmp:            [0u8; 8192],
            next_stream_id: 1,
            requests_done:  0,
        })
    }

    fn get(&mut self) -> anyhow::Result<Vec<u8>> {
        let sid = self.next_stream_id;
        self.next_stream_id += 2;
        self.requests_done += 1;

        // END_STREAM | END_HEADERS = 0x05
        self.tls.write_all(&make_h2_frame(0x1, 0x05, sid, H2_HEADERS_GET_ROOT))?;
        self.tls.flush()?;

        let mut body = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(5);

        loop {
            if Instant::now() > deadline { anyhow::bail!("H2 response timeout"); }

            if let Some((ftype, flags, fsid, total)) = try_parse_h2_frame(&self.recv_buf) {
                let payload = self.recv_buf[9..total].to_vec();
                self.recv_buf.drain(..total);
                match ftype {
                    0x0 => { // DATA
                        if fsid == sid {
                            body.extend_from_slice(&payload);
                            if flags & 0x1 != 0 { return Ok(body); } // END_STREAM
                        }
                    }
                    0x1 if fsid == sid && flags & 0x1 != 0 => {
                        // HEADERS with END_STREAM (empty body response)
                        return Ok(body);
                    }
                    0x7 => anyhow::bail!("server sent GOAWAY"),
                    _ => {} // SETTINGS, SETTINGS-ACK, WINDOW_UPDATE, etc.
                }
                continue;
            }

            let n = match self.tls.read(&mut self.tmp) {
                Ok(0) => anyhow::bail!("H2 connection closed"),
                Ok(n) => n,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock
                       || e.kind() == std::io::ErrorKind::TimedOut => {
                    std::thread::sleep(Duration::from_millis(1));
                    continue;
                }
                Err(e) => return Err(e.into()),
            };
            self.recv_buf.extend_from_slice(&self.tmp[..n]);
        }
    }
}

const H2_MAX_REQUESTS_PER_CONN: usize = 1_000;

fn bench_http2_latency(addr: &str, n: usize, skip_verify: bool) -> anyhow::Result<Vec<f64>> {
    let tls_cfg = make_client_config_h2(skip_verify);
    let mut latencies = Vec::with_capacity(n);
    let mut client = H2Client::connect(addr, Arc::clone(&tls_cfg))?;

    for _ in 0..5 { client.get()?; } // warmup

    for _ in 0..n {
        if client.requests_done >= H2_MAX_REQUESTS_PER_CONN {
            client = H2Client::connect(addr, Arc::clone(&tls_cfg))?;
        }
        let t0 = Instant::now();
        client.get()?;
        latencies.push(t0.elapsed().as_secs_f64() * 1000.0);
    }
    Ok(latencies)
}

fn bench_http2_throughput(addr: &str, duration_s: u64, concurrency: usize, skip_verify: bool) -> anyhow::Result<f64> {
    let count    = Arc::new(AtomicUsize::new(0));
    let deadline = Instant::now() + Duration::from_secs(duration_s);
    let addr     = Arc::new(addr.to_string());

    let handles: Vec<_> = (0..concurrency).map(|_| {
        let count = Arc::clone(&count);
        let addr  = Arc::clone(&addr);
        std::thread::spawn(move || {
            let tls_cfg = make_client_config_h2(skip_verify);
            let result = (|| -> anyhow::Result<()> {
                let mut client = H2Client::connect(&addr, Arc::clone(&tls_cfg))?;
                while Instant::now() < deadline {
                    if client.requests_done >= H2_MAX_REQUESTS_PER_CONN {
                        client = H2Client::connect(&addr, Arc::clone(&tls_cfg))?;
                    }
                    match client.get() {
                        Ok(_) => { count.fetch_add(1, Ordering::Relaxed); }
                        Err(e) => { eprintln!("h2 throughput error: {e}"); }
                    }
                }
                Ok(())
            })();
            if let Err(e) = result { eprintln!("h2 thread error: {e}"); }
        })
    }).collect();

    let t0 = Instant::now();
    for h in handles { h.join().ok(); }
    let elapsed   = t0.elapsed().as_secs_f64();
    let completed = count.load(Ordering::Relaxed);
    Ok(completed as f64 / elapsed)
}

// ── HTTP/3 helpers ────────────────────────────────────────────────────────────

fn make_quiche_client_config(skip_verify: bool) -> quiche::Config {
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

fn new_scid() -> quiche::ConnectionId<'static> {
    let mut scid = [0u8; quiche::MAX_CONN_ID_LEN];
    rand::thread_rng().fill(&mut scid);
    quiche::ConnectionId::from_vec(scid.to_vec())
}

/// Send all pending quiche packets to the socket.
fn quic_flush(conn: &mut quiche::Connection, udp: &UdpSocket) {
    let mut out = [0u8; 1350];
    loop {
        match conn.send(&mut out) {
            Ok((len, _info)) => { udp.send(&out[..len]).ok(); }
            Err(quiche::Error::Done) => break,
            Err(e) => { eprintln!("quic_flush send: {e}"); break; }
        }
    }
}

/// One HTTP/3 GET / on an existing H3 connection. Returns response body.
fn h3_get(
    conn: &mut quiche::Connection,
    h3: &mut quiche::h3::Connection,
    udp: &UdpSocket,
) -> anyhow::Result<Vec<u8>> {
    let req = vec![
        quiche::h3::Header::new(b":method", b"GET"),
        quiche::h3::Header::new(b":path", b"/"),
        quiche::h3::Header::new(b":scheme", b"https"),
        quiche::h3::Header::new(b":authority", b"localhost"),
    ];
    // Use the stream ID assigned by quiche (not a manually tracked counter).
    let stream_id = h3.send_request(conn, &req, true)?;
    quic_flush(conn, udp);

    let mut body = Vec::new();
    let mut buf = [0u8; 65535];
    let deadline = Instant::now() + Duration::from_secs(5);

    loop {
        if Instant::now() > deadline {
            anyhow::bail!("HTTP/3 request timed out");
        }
        conn.on_timeout();
        quic_flush(conn, udp);

        udp.set_read_timeout(Some(Duration::from_millis(100)))?;
        let n = match udp.recv(&mut buf) {
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock
                   || e.kind() == std::io::ErrorKind::TimedOut => {
                if conn.is_closed() { break; }
                continue;
            }
            Err(e) => return Err(e.into()),
        };
        let recv_info = quiche::RecvInfo {
            from: udp.peer_addr()?,
            to:   udp.local_addr()?,
        };
        conn.recv(&mut buf[..n], recv_info)?;
        quic_flush(conn, udp);

        // Poll H3 events. Use the actual sid from each event to drain the
        // correct stream; only accumulate body / return when it's our stream.
        loop {
            match h3.poll(conn) {
                Ok((sid, quiche::h3::Event::Data)) => {
                    while let Ok(read) = h3.recv_body(conn, sid, &mut buf) {
                        if sid == stream_id {
                            body.extend_from_slice(&buf[..read]);
                        }
                    }
                }
                Ok((sid, quiche::h3::Event::Finished)) if sid == stream_id => {
                    return Ok(body);
                }
                Ok(_) => {}
                Err(quiche::h3::Error::Done) => break,
                Err(e) => return Err(e.into()),
            }
        }

        if conn.is_closed() { break; }
    }
    Ok(body)
}

/// Establish one QUIC+H3 connection to addr.
fn h3_connect(addr: &str, cfg: &mut quiche::Config) -> anyhow::Result<(quiche::Connection, quiche::h3::Connection, UdpSocket)> {
    let udp = UdpSocket::bind("0.0.0.0:0")?;
    udp.connect(addr)?;
    let scid = new_scid();
    let peer: std::net::SocketAddr = addr.parse()?;
    let local = udp.local_addr()?;
    let mut conn = quiche::connect(
        Some("localhost"),
        &scid,
        local,
        peer,
        cfg,
    )?;
    // Initial flush (ClientHello)
    quic_flush(&mut conn, &udp);

    // Handshake loop
    let mut buf = [0u8; 65535];
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if Instant::now() > deadline {
            anyhow::bail!("QUIC handshake timed out");
        }
        conn.on_timeout();
        quic_flush(&mut conn, &udp);
        udp.set_read_timeout(Some(Duration::from_millis(100)))?;
        let n = match udp.recv(&mut buf) {
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock
                   || e.kind() == std::io::ErrorKind::TimedOut => continue,
            Err(e) => return Err(e.into()),
        };
        let recv_info = quiche::RecvInfo {
            from: peer,
            to:   local,
        };
        conn.recv(&mut buf[..n], recv_info)?;
        quic_flush(&mut conn, &udp);
        if conn.is_established() { break; }
        if conn.is_closed() { anyhow::bail!("QUIC connection closed during handshake"); }
    }

    let h3_cfg = quiche::h3::Config::new()?;
    let h3 = quiche::h3::Connection::with_transport(&mut conn, &h3_cfg)?;
    Ok((conn, h3, udp))
}

// Maximum bidi streams per connection before reconnecting. Stays well under
// the server's initial_max_streams_bidi(100) to avoid StreamLimit errors.
const H3_MAX_STREAMS_PER_CONN: usize = 90;

// ── HTTP/3 latency ────────────────────────────────────────────────────────────

fn bench_http3_latency(addr: &str, n: usize, skip_verify: bool) -> anyhow::Result<Vec<f64>> {
    let mut cfg = make_quiche_client_config(skip_verify);
    let mut latencies = Vec::with_capacity(n);

    let (mut conn, mut h3, mut udp) = h3_connect(addr, &mut cfg)?;
    let mut reqs: usize = 0; // count requests on this connection

    // Warmup: 5 requests
    for _ in 0..5 {
        if reqs >= H3_MAX_STREAMS_PER_CONN {
            let (c, h, u) = h3_connect(addr, &mut cfg)?;
            conn = c; h3 = h; udp = u; reqs = 0;
        }
        let _ = h3_get(&mut conn, &mut h3, &udp);
        reqs += 1;
    }

    for _ in 0..n {
        if reqs >= H3_MAX_STREAMS_PER_CONN {
            let (c, h, u) = h3_connect(addr, &mut cfg)?;
            conn = c; h3 = h; udp = u; reqs = 0;
        }
        let t0 = Instant::now();
        h3_get(&mut conn, &mut h3, &udp)?;
        latencies.push(t0.elapsed().as_secs_f64() * 1000.0);
        reqs += 1;
    }
    Ok(latencies)
}

// ── HTTP/3 throughput — one connection per "thread", reconnect at stream limit ─

fn bench_http3_throughput(addr: &str, duration_s: u64, concurrency: usize, skip_verify: bool) -> anyhow::Result<f64> {
    let count = Arc::new(AtomicUsize::new(0));
    let deadline = Instant::now() + Duration::from_secs(duration_s);
    let addr = Arc::new(addr.to_string());
    let errors = Arc::new(Mutex::new(Vec::<String>::new()));

    let handles: Vec<_> = (0..concurrency).map(|_| {
        let count = Arc::clone(&count);
        let addr = Arc::clone(&addr);
        let errors = Arc::clone(&errors);
        std::thread::spawn(move || {
            let mut cfg = make_quiche_client_config(skip_verify);
            let result = (|| -> anyhow::Result<()> {
                let (mut conn, mut h3, mut udp) = h3_connect(&addr, &mut cfg)?;
                let mut reqs: usize = 0;
                while Instant::now() < deadline {
                    if reqs >= H3_MAX_STREAMS_PER_CONN {
                        let (c, h, u) = h3_connect(&addr, &mut cfg)?;
                        conn = c; h3 = h; udp = u; reqs = 0;
                    }
                    reqs += 1;
                    match h3_get(&mut conn, &mut h3, &udp) {
                        Ok(_) => { count.fetch_add(1, Ordering::Relaxed); }
                        Err(e) => { eprintln!("h3 throughput error: {e}"); }
                    }
                }
                Ok(())
            })();
            if let Err(e) = result {
                errors.lock().unwrap().push(e.to_string());
            }
        })
    }).collect();
    let t0 = Instant::now();
    for h in handles { h.join().ok(); }
    let elapsed = t0.elapsed().as_secs_f64();
    let completed = count.load(Ordering::Relaxed);
    Ok(completed as f64 / elapsed)
}

// ── Result reporter ───────────────────────────────────────────────────────────

struct BenchResult {
    name:     String,
    p0_ms:    f64,
    p1_ms:    f64,
    p25_ms:   f64,
    p50_ms:   f64,
    p75_ms:   f64,
    p90_ms:   f64,
    p99_ms:   f64,
    p999_ms:  f64,
    p100_ms:  f64,
    rps:      f64,
}

impl BenchResult {
    fn from_latencies(name: impl Into<String>, lats: Vec<f64>) -> Self {
        BenchResult {
            name:    name.into(),
            p0_ms:   percentile(lats.clone(),   0.0),
            p1_ms:   percentile(lats.clone(),   1.0),
            p25_ms:  percentile(lats.clone(),  25.0),
            p50_ms:  percentile(lats.clone(),  50.0),
            p75_ms:  percentile(lats.clone(),  75.0),
            p90_ms:  percentile(lats.clone(),  90.0),
            p99_ms:  percentile(lats.clone(),  99.0),
            p999_ms: percentile(lats.clone(),  99.9),
            p100_ms: percentile(lats,         100.0),
            rps:     0.0,
        }
    }
    fn from_rps(name: impl Into<String>, rps: f64) -> Self {
        BenchResult {
            name: name.into(),
            p0_ms: 0.0, p1_ms: 0.0, p25_ms: 0.0, p50_ms: 0.0, p75_ms: 0.0,
            p90_ms: 0.0, p99_ms: 0.0, p999_ms: 0.0, p100_ms: 0.0, rps,
        }
    }
}

fn print_result(r: &BenchResult, p99_limit: f64, rps_min: f64) -> bool {
    let p99_ok = r.p99_ms <= p99_limit || r.p99_ms == 0.0;
    let rps_ok = r.rps >= rps_min || r.rps == 0.0;
    let status = if p99_ok && rps_ok { "PASS" } else { "FAIL" };
    if r.rps > 0.0 {
        println!("{:<6} {:<24}  {:.1} req/s", status, r.name, r.rps);
    } else {
        println!(
            "{:<6} {:<24}  p0={:6.3}  p1={:6.3}  p25={:6.3}  p50={:6.3}  p75={:6.3}  p90={:6.3}  p99={:6.3}  p99.9={:6.3}  p100={:6.3}  (ms)",
            status, r.name,
            r.p0_ms, r.p1_ms, r.p25_ms, r.p50_ms, r.p75_ms,
            r.p90_ms, r.p99_ms, r.p999_ms, r.p100_ms
        );
    }
    if !p99_ok {
        println!("       p99 {:.3}ms exceeds limit {:.3}ms", r.p99_ms, p99_limit);
    }
    if !rps_ok {
        println!("       throughput {:.1} req/s below minimum {:.1}", r.rps, rps_min);
    }
    p99_ok && rps_ok
}

// ── main ──────────────────────────────────────────────────────────────────────

fn main() {
    let args = Args::parse();
    let mut all_pass = true;

    println!("m6-bench  target={}  skip-verify={}  http11={}  http2={}  http3={}",
             args.addr, args.skip_verify, args.http11, args.http2, args.http3);
    println!("{:-<70}", "");

    if args.http11 {
        let tls_cfg = make_client_config(args.skip_verify);

        // Latency (serial: one new TLS connection per request).
        // HTTP/1.1 connections are driven on each epoll tick (~100ms), so
        // serial latency is ~200ms by design.  Report the numbers but do not
        // apply a p99 limit here.
        match bench_http11_latency(&args.addr, args.latency_n, Arc::clone(&tls_cfg)) {
            Ok(lats) => {
                let r = BenchResult::from_latencies("HTTP/1.1 latency", lats);
                print_result(&r, f64::INFINITY, 0.0);
            }
            Err(e) => { eprintln!("HTTP/1.1 latency error: {e}"); all_pass = false; }
        }

        // Throughput
        match bench_http11_throughput(&args.addr, args.duration_s, args.concurrency, Arc::clone(&tls_cfg)) {
            Ok(rps) => {
                if !print_result(&BenchResult::from_rps("HTTP/1.1 throughput", rps), 0.0, args.rps_min) { all_pass = false; }
            }
            Err(e) => { eprintln!("HTTP/1.1 throughput error: {e}"); all_pass = false; }
        }
    }

    if args.http2 {
        match bench_http2_latency(&args.addr, args.latency_n, args.skip_verify) {
            Ok(lats) => {
                let r = BenchResult::from_latencies("HTTP/2 latency", lats);
                if !print_result(&r, args.p99_limit_ms, 0.0) { all_pass = false; }
            }
            Err(e) => { eprintln!("HTTP/2 latency error: {e}"); all_pass = false; }
        }

        match bench_http2_throughput(&args.addr, args.duration_s, args.concurrency, args.skip_verify) {
            Ok(rps) => {
                if !print_result(&BenchResult::from_rps("HTTP/2 throughput", rps), 0.0, args.rps_min) { all_pass = false; }
            }
            Err(e) => { eprintln!("HTTP/2 throughput error: {e}"); all_pass = false; }
        }
    }

    if args.http3 {
        // Latency
        match bench_http3_latency(&args.addr, args.latency_n, args.skip_verify) {
            Ok(lats) => {
                let r = BenchResult::from_latencies("HTTP/3 latency", lats);
                if !print_result(&r, args.p99_limit_ms, 0.0) { all_pass = false; }
            }
            Err(e) => { eprintln!("HTTP/3 latency error: {e}"); all_pass = false; }
        }

        // Throughput
        match bench_http3_throughput(&args.addr, args.duration_s, args.concurrency, args.skip_verify) {
            Ok(rps) => {
                if !print_result(&BenchResult::from_rps("HTTP/3 throughput", rps), 0.0, args.rps_min) { all_pass = false; }
            }
            Err(e) => { eprintln!("HTTP/3 throughput error: {e}"); all_pass = false; }
        }
    }

    println!("{:-<70}", "");
    if all_pass {
        println!("All benchmarks passed.");
    } else {
        println!("One or more benchmarks FAILED.");
        std::process::exit(1);
    }
}
