/// m6-bench — loopback latency + throughput benchmark for m6-http.
///
/// Modes (flags can be combined):
///   --http11-only       only run HTTP/1.1 suites
///   --http2-only        only run HTTP/2 suites
///   --http3-only        only run HTTP/3 suites
///   --h2c-only          only run H2C suites (HTTP/2 cleartext, for WireGuard tunnels)
///   --url-only          only run URL-backend suites (h2 inbound, http/https/h2c/h2s outbound)
///   --h2c               include H2C suites alongside the selected protocols
///   --url               include URL-backend suites alongside the selected protocols
///   --skip-verify       skip TLS certificate verification
///   --latency-n N       requests per latency run (default 2000)
///   --duration S        throughput run duration in seconds (default 10)
///   --concurrency C     parallel threads for throughput (default 8)
///   --p99-limit-us F    fail if p99 latency exceeds this in µs (default 50000)
///   --rps-min F         fail if throughput drops below this (default 50)
///   --addr HOST:PORT    target address for TLS protocols (default 127.0.0.1:8443)
///   --h2c-addr HOST:PORT  target address for H2C (default 127.0.0.1:8080)
///
/// Output: one result line per suite; exits non-zero if any threshold exceeded.

use std::io::{self, Read, Write};
use std::net::{TcpStream, UdpSocket};
use std::os::unix::io::AsRawFd;
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
    h2c:    bool,
    /// Include URL-backend suites (http/https/h2c/h2s outbound via m6-http H2 inbound).
    url:    bool,
    skip_verify:      bool,
    latency_only:     bool,
    throughput_only:  bool,
    path_only:        bool,
    latency_n:        usize,
    duration_s:       u64,
    concurrency:      usize,
    p99_limit_us:     f64,
    rps_min:          f64,
    addr:             String,
    h2c_addr:         String,
}

impl Args {
    fn parse() -> Self {
        let mut a = Args {
            http11: true,
            http2:  true,
            http3:  true,
            h2c:    false,
            url:    false,
            skip_verify:     false,
            latency_only:    false,
            throughput_only: false,
            path_only:       false,
            latency_n:       2000,
            duration_s:      10,
            concurrency:     8,
            p99_limit_us:    50_000.0,
            rps_min:         50.0,
            addr:     "127.0.0.1:8443".into(),
            h2c_addr: "127.0.0.1:8080".into(),
        };
        let raw: Vec<String> = std::env::args().skip(1).collect();
        let mut i = 0;
        while i < raw.len() {
            match raw[i].as_str() {
                "--http11-only"      => { a.http11 = true;  a.http2 = false; a.http3 = false; a.h2c = false; a.url = false; }
                "--http2-only"       => { a.http11 = false; a.http2 = true;  a.http3 = false; a.h2c = false; a.url = false; }
                "--http3-only"       => { a.http11 = false; a.http2 = false; a.http3 = true;  a.h2c = false; a.url = false; }
                "--h2c-only"         => { a.http11 = false; a.http2 = false; a.http3 = false; a.h2c = true;  a.url = false; }
                "--url-only"         => { a.http11 = false; a.http2 = false; a.http3 = false; a.h2c = false; a.url = true; }
                "--h2c"              => a.h2c = true,
                "--url"              => a.url = true,
                "--skip-verify"      => a.skip_verify = true,
                "--latency-only"     => a.latency_only    = true,
                "--throughput-only"  => a.throughput_only = true,
                "--path-only"        => a.path_only       = true,
                "--latency-n"        => { i += 1; a.latency_n    = raw[i].parse().expect("latency-n"); }
                "--duration"         => { i += 1; a.duration_s   = raw[i].parse().expect("duration"); }
                "--concurrency"      => { i += 1; a.concurrency  = raw[i].parse().expect("concurrency"); }
                "--p99-limit-us"     => { i += 1; a.p99_limit_us = raw[i].parse().expect("p99-limit-us"); }
                "--rps-min"          => { i += 1; a.rps_min      = raw[i].parse().expect("rps-min"); }
                "--addr"             => { i += 1; a.addr     = raw[i].clone(); }
                "--h2c-addr"         => { i += 1; a.h2c_addr = raw[i].clone(); }
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
    http11_get_path(addr, "/", tls_cfg)
}

fn http11_get_path(addr: &str, path: &str, tls_cfg: Arc<ClientConfig>) -> anyhow::Result<Vec<u8>> {
    let stream = TcpStream::connect(addr)?;
    stream.set_nodelay(true)?;
    let server_name: ServerName<'static> = "localhost".try_into().unwrap();
    let mut conn = rustls::ClientConnection::new(tls_cfg, server_name)?;
    let mut stream_ref = &stream;
    let mut tls = rustls::Stream::new(&mut conn, &mut stream_ref);
    let req = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    tls.write_all(req.as_bytes())?;
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
    bench_http11_latency_path(addr, "/", n, tls_cfg)
}

fn bench_http11_latency_path(addr: &str, path: &str, n: usize, tls_cfg: Arc<ClientConfig>) -> anyhow::Result<Vec<f64>> {
    let mut latencies = Vec::with_capacity(n);
    for _ in 0..5 {
        http11_get_path(addr, path, Arc::clone(&tls_cfg))?;
    }
    for _ in 0..n {
        let t0 = Instant::now();
        let resp = http11_get_path(addr, path, Arc::clone(&tls_cfg))?;
        let elapsed = t0.elapsed().as_secs_f64() * 1_000_000.0;
        let status = parse_http11_status(&resp);
        if status != 200 { eprintln!("HTTP/1.1 non-200 ({path}): {status}"); }
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

/// Build a minimal HPACK block for `GET <path> https localhost`.
///
/// Uses HPACK static table entries where possible:
///   0x82 = :method GET   (index 2)
///   0x84 = :path /       (index 4, "/" only)
///   0x87 = :scheme https (index 7)
///   0x41 = :authority    (literal+index, name index 1)
///
/// For paths other than "/", the :path header is encoded as a literal with
/// no dynamic-table indexing (format 0x04 = 0000 0100, name index 4).
fn make_h2_get_headers(path: &str) -> Vec<u8> {
    let mut h = Vec::with_capacity(32);
    h.push(0x82); // :method GET
    let pb = path.as_bytes();
    if path == "/" {
        h.push(0x84); // :path / (static table index 4)
    } else {
        h.push(0x04); // literal no-index, name = static[4] = :path
        // 7-bit integer length (no Huffman); paths used here are all < 128 bytes
        debug_assert!(pb.len() < 128, "path too long for simple HPACK length encoding");
        h.push(pb.len() as u8);
        h.extend_from_slice(pb);
    }
    h.push(0x87); // :scheme https
    // :authority localhost — literal with incremental indexing, name index 1
    h.extend_from_slice(&[0x41, 0x09, b'l', b'o', b'c', b'a', b'l', b'h', b'o', b's', b't']);
    h
}

/// HPACK header block for H2C (`:scheme http`, not `https`).
fn make_h2c_get_headers(path: &str) -> Vec<u8> {
    let mut h = Vec::with_capacity(32);
    h.push(0x82); // :method GET
    let pb = path.as_bytes();
    if path == "/" {
        h.push(0x84); // :path /
    } else {
        h.push(0x04);
        debug_assert!(pb.len() < 128, "path too long for simple HPACK length encoding");
        h.push(pb.len() as u8);
        h.extend_from_slice(pb);
    }
    h.push(0x86); // :scheme http (static table index 6; index 7 = https)
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

/// Persistent HTTP/2 client over TLS.
///
/// Uses explicit read_tls/write_tls calls (same pattern as advance_tls in http11.rs)
/// rather than rustls::StreamOwned, whose complete_io() skips reads on an established
/// connection — causing indefinite blocking waits for server responses.
struct H2Client {
    conn:           rustls::ClientConnection,
    stream:         TcpStream,
    recv_buf:       Vec<u8>,
    tmp:            [u8; 8192],
    next_stream_id: u32,
    requests_done:  usize,
}

impl H2Client {
    fn connect(addr: &str, tls_cfg: Arc<ClientConfig>) -> anyhow::Result<Self> {
        let stream = TcpStream::connect(addr)?;
        stream.set_nodelay(true)?;
        // Always non-blocking; deadline loops use io::ErrorKind::WouldBlock.
        stream.set_nonblocking(true)?;
        let server_name: ServerName<'static> = "localhost".try_into().unwrap();
        let conn = rustls::ClientConnection::new(tls_cfg, server_name)?;

        let mut c = H2Client {
            conn, stream,
            recv_buf:       Vec::with_capacity(16_384),
            tmp:            [0u8; 8192],
            next_stream_id: 1,
            requests_done:  0,
        };

        // Queue preface + empty SETTINGS for sending alongside the TLS handshake.
        c.conn.writer().write_all(H2_CLIENT_PREFACE)?;
        c.conn.writer().write_all(&make_h2_frame(0x4, 0x0, 0, &[]))?;

        // Drive TLS handshake: alternate writes and blocking reads until established.
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            c.flush_write()?;
            if !c.conn.is_handshaking() { break; }
            c.fill_recv_deadline(deadline)?; // spin until next TLS record arrives
        }
        // Drain any buffered server data (SETTINGS, NewSessionTickets, etc.).
        c.fill_recv_drain()?;
        // Acknowledge the server's SETTINGS frame (RFC 9113 §6.5 requirement).
        c.conn.writer().write_all(&make_h2_frame(0x4, 0x1, 0, &[]))?;
        c.flush_write()?;
        Ok(c)
    }

    /// Write all pending TLS output to the socket (non-blocking, with retry).
    /// Returns the total number of TLS bytes written to the socket.
    fn flush_write(&mut self) -> io::Result<usize> {
        let mut total = 0;
        loop {
            let r = { let mut sr = &self.stream; self.conn.write_tls(&mut sr) };
            match r {
                Ok(0)  => break,
                Ok(n)  => { total += n; }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => return Err(e),
            }
        }
        Ok(total)
    }

    /// Block until at least one TLS record arrives or the deadline passes.
    /// Uses poll() to sleep until the socket is readable rather than spinning,
    /// which prevents the client from starving the single-threaded server.
    fn fill_recv_deadline(&mut self, deadline: Instant) -> io::Result<()> {
        loop {
            let r = { let mut sr = &self.stream; self.conn.read_tls(&mut sr) };
            match r {
                Ok(0) => return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "closed")),
                Ok(_) => {
                    self.conn.process_new_packets()
                        .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
                    break; // got at least one TLS record
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        return Err(io::Error::new(io::ErrorKind::TimedOut, "deadline"));
                    }
                    // Flush any pending writes before sleeping so the server
                    // has our request before we block waiting for its response.
                    self.flush_write()?;
                    let timeout_ms = remaining.as_millis().min(100) as i32;
                    unsafe {
                        let mut pfd = libc::pollfd {
                            fd:      self.stream.as_raw_fd(),
                            events:  libc::POLLIN,
                            revents: 0,
                        };
                        libc::poll(&mut pfd, 1, timeout_ms);
                    }
                }
                Err(e) => return Err(e),
            }
        }
        self.drain_plaintext()
    }

    /// Drain whatever TLS records are already buffered (non-blocking).
    fn fill_recv_drain(&mut self) -> io::Result<()> {
        loop {
            let r = { let mut sr = &self.stream; self.conn.read_tls(&mut sr) };
            match r {
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

    fn get(&mut self, path: &str) -> anyhow::Result<Vec<u8>> {
        let sid = self.next_stream_id;
        self.next_stream_id += 2;
        self.requests_done += 1;

        // END_STREAM | END_HEADERS = 0x05
        let headers = make_h2_get_headers(path);
        self.conn.writer().write_all(&make_h2_frame(0x1, 0x05, sid, &headers))?;
        self.flush_write()?;

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
                            let data_len = payload.len() as u32;
                            body.extend_from_slice(&payload);
                            if data_len > 0 {
                                // Send WINDOW_UPDATE for connection (sid=0) and stream
                                self.conn.writer().write_all(&make_h2_frame(0x8, 0, 0, &data_len.to_be_bytes()))?;
                                self.conn.writer().write_all(&make_h2_frame(0x8, 0, sid, &data_len.to_be_bytes()))?;
                                self.flush_write()?;
                            }
                            if flags & 0x1 != 0 { return Ok(body); } // END_STREAM
                        }
                    }
                    0x1 if fsid == sid => {
                        if flags & 0x1 != 0 {
                            return Ok(body); // HEADERS with END_STREAM (no body)
                        }
                        // HEADERS without END_STREAM: response headers only, body follows
                    }
                    0x3 if fsid == sid => anyhow::bail!("server RST_STREAM on sid {sid}"),
                    0x7 => anyhow::bail!("server sent GOAWAY"),
                    _ => {} // SETTINGS, SETTINGS-ACK, WINDOW_UPDATE, PRIORITY, etc.
                }
                continue;
            }

            // Need more data — spin until the server sends something.
            self.fill_recv_deadline(deadline)
                .map_err(|e| anyhow::anyhow!("H2 read: {}", e))?;
        }
    }
}

const H2_MAX_REQUESTS_PER_CONN: usize = 1_000;

fn bench_http2_latency(addr: &str, n: usize, skip_verify: bool) -> anyhow::Result<Vec<f64>> {
    bench_http2_latency_path(addr, "/", n, skip_verify)
}

fn bench_http2_latency_path(addr: &str, path: &str, n: usize, skip_verify: bool) -> anyhow::Result<Vec<f64>> {
    let tls_cfg = make_client_config_h2(skip_verify);
    let mut latencies = Vec::with_capacity(n);
    let mut client = H2Client::connect(addr, Arc::clone(&tls_cfg))?;

    for _ in 0..5 { client.get(path)?; } // warmup

    for _ in 0..n {
        if client.requests_done >= H2_MAX_REQUESTS_PER_CONN {
            client = H2Client::connect(addr, Arc::clone(&tls_cfg))?;
        }
        let t0 = Instant::now();
        client.get(path)?;
        latencies.push(t0.elapsed().as_secs_f64() * 1_000_000.0);
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
                    match client.get("/") {
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

fn bench_http11_throughput_path(addr: &str, path: &str, duration_s: u64, concurrency: usize, tls_cfg: Arc<ClientConfig>) -> anyhow::Result<f64> {
    let count    = Arc::new(AtomicUsize::new(0));
    let deadline = Instant::now() + Duration::from_secs(duration_s);
    let addr     = Arc::new(addr.to_string());
    let path     = Arc::new(path.to_string());

    let handles: Vec<_> = (0..concurrency).map(|_| {
        let tls_cfg = Arc::clone(&tls_cfg);
        let count   = Arc::clone(&count);
        let addr    = Arc::clone(&addr);
        let path    = Arc::clone(&path);
        std::thread::spawn(move || {
            while Instant::now() < deadline {
                if http11_get_path(&addr, &path, Arc::clone(&tls_cfg)).is_ok() {
                    count.fetch_add(1, Ordering::Relaxed);
                }
            }
        })
    }).collect();

    let t0 = Instant::now();
    for h in handles { h.join().ok(); }
    let elapsed   = t0.elapsed().as_secs_f64();
    let completed = count.load(Ordering::Relaxed);
    Ok(completed as f64 / elapsed)
}

fn bench_http2_throughput_path(addr: &str, path: &str, duration_s: u64, concurrency: usize, skip_verify: bool) -> anyhow::Result<f64> {
    let count    = Arc::new(AtomicUsize::new(0));
    let deadline = Instant::now() + Duration::from_secs(duration_s);
    let addr     = Arc::new(addr.to_string());
    let path     = Arc::new(path.to_string());

    let handles: Vec<_> = (0..concurrency).map(|_| {
        let count = Arc::clone(&count);
        let addr  = Arc::clone(&addr);
        let path  = Arc::clone(&path);
        std::thread::spawn(move || {
            let tls_cfg = make_client_config_h2(skip_verify);
            let result = (|| -> anyhow::Result<()> {
                let mut client = H2Client::connect(&addr, Arc::clone(&tls_cfg))?;
                while Instant::now() < deadline {
                    if client.requests_done >= H2_MAX_REQUESTS_PER_CONN {
                        client = H2Client::connect(&addr, Arc::clone(&tls_cfg))?;
                    }
                    match client.get(&path) {
                        Ok(_) => { count.fetch_add(1, Ordering::Relaxed); }
                        Err(e) => { eprintln!("h2 url throughput error: {e}"); }
                    }
                }
                Ok(())
            })();
            if let Err(e) = result { eprintln!("h2 url thread error: {e}"); }
        })
    }).collect();

    let t0 = Instant::now();
    for h in handles { h.join().ok(); }
    let elapsed   = t0.elapsed().as_secs_f64();
    let completed = count.load(Ordering::Relaxed);
    Ok(completed as f64 / elapsed)
}

// ── H2C (HTTP/2 cleartext) helpers ────────────────────────────────────────────
//
// Mirrors H2Client but connects over plain TCP without TLS.
// Intended for the h2c_bind port (e.g. 127.0.0.1:8080) used over WireGuard tunnels.

/// Persistent HTTP/2 cleartext client over plain TCP.
struct H2cClient {
    stream:         TcpStream,
    send_buf:       Vec<u8>,
    recv_buf:       Vec<u8>,
    tmp:            [u8; 8192],
    next_stream_id: u32,
    requests_done:  usize,
}

impl H2cClient {
    fn connect(addr: &str) -> anyhow::Result<Self> {
        // TCP connect — blocking during H2 setup phase.
        let stream = TcpStream::connect(addr)?;
        stream.set_nodelay(true)?;
        stream.set_read_timeout(Some(Duration::from_secs(5)))?;

        // Send client connection preface + empty SETTINGS.
        (&stream).write_all(H2_CLIENT_PREFACE)?;
        (&stream).write_all(&make_h2_frame(0x4, 0x0, 0, &[]))?;

        // Read frames until we receive the server's SETTINGS (non-ACK).
        let mut recv_buf: Vec<u8> = Vec::with_capacity(16_384);
        let mut tmp = [0u8; 8192];
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut got_settings = false;
        while !got_settings {
            if Instant::now() > deadline { anyhow::bail!("H2C setup timeout"); }
            let n = (&stream).read(&mut tmp)?;
            if n == 0 { anyhow::bail!("H2C: connection closed during setup"); }
            recv_buf.extend_from_slice(&tmp[..n]);
            while let Some((ftype, flags, _, total)) = try_parse_h2_frame(&recv_buf) {
                if ftype == 0x4 && flags & 0x1 == 0 { got_settings = true; }
                recv_buf.drain(..total);
            }
        }

        // Acknowledge server SETTINGS.
        (&stream).write_all(&make_h2_frame(0x4, 0x1, 0, &[]))?;

        // Switch to non-blocking for all subsequent I/O.
        stream.set_nonblocking(true)?;
        stream.set_read_timeout(None)?;

        Ok(H2cClient {
            stream,
            send_buf:       Vec::with_capacity(4096),
            recv_buf,
            tmp:            [0u8; 8192],
            next_stream_id: 1,
            requests_done:  0,
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
                    let timeout_ms = remaining.as_millis().min(100) as i32;
                    unsafe {
                        let mut pfd = libc::pollfd {
                            fd: self.stream.as_raw_fd(), events: libc::POLLIN, revents: 0,
                        };
                        libc::poll(&mut pfd, 1, timeout_ms);
                    }
                }
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    fn get(&mut self, path: &str) -> anyhow::Result<Vec<u8>> {
        let sid = self.next_stream_id;
        self.next_stream_id += 2;
        self.requests_done += 1;

        let headers = make_h2c_get_headers(path);
        self.send_buf.extend_from_slice(&make_h2_frame(0x1, 0x05, sid, &headers));
        self.flush_write()?;

        let mut body = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(5);

        loop {
            if Instant::now() > deadline { anyhow::bail!("H2C response timeout"); }

            if let Some((ftype, flags, fsid, total)) = try_parse_h2_frame(&self.recv_buf) {
                let payload = self.recv_buf[9..total].to_vec();
                self.recv_buf.drain(..total);
                match ftype {
                    0x0 => {
                        if fsid == sid {
                            let data_len = payload.len() as u32;
                            body.extend_from_slice(&payload);
                            if data_len > 0 {
                                self.send_buf.extend_from_slice(&make_h2_frame(0x8, 0, 0, &data_len.to_be_bytes()));
                                self.send_buf.extend_from_slice(&make_h2_frame(0x8, 0, sid, &data_len.to_be_bytes()));
                                self.flush_write()?;
                            }
                            if flags & 0x1 != 0 { return Ok(body); }
                        }
                    }
                    0x1 if fsid == sid => {
                        if flags & 0x1 != 0 { return Ok(body); }
                    }
                    0x3 if fsid == sid => anyhow::bail!("server RST_STREAM on sid {sid}"),
                    0x7 => anyhow::bail!("server sent GOAWAY"),
                    _ => {}
                }
                continue;
            }

            self.fill_recv_deadline(deadline)
                .map_err(|e| anyhow::anyhow!("H2C read: {}", e))?;
        }
    }
}

const H2C_MAX_REQUESTS_PER_CONN: usize = 1_000;

fn bench_h2c_latency(addr: &str, n: usize) -> anyhow::Result<Vec<f64>> {
    bench_h2c_latency_path(addr, "/", n)
}

fn bench_h2c_latency_path(addr: &str, path: &str, n: usize) -> anyhow::Result<Vec<f64>> {
    let mut latencies = Vec::with_capacity(n);
    let mut client = H2cClient::connect(addr)?;

    for _ in 0..5 { client.get(path)?; } // warmup

    for _ in 0..n {
        if client.requests_done >= H2C_MAX_REQUESTS_PER_CONN {
            client = H2cClient::connect(addr)?;
        }
        let t0 = Instant::now();
        client.get(path)?;
        latencies.push(t0.elapsed().as_secs_f64() * 1_000_000.0);
    }
    Ok(latencies)
}

fn bench_h2c_throughput(addr: &str, duration_s: u64, concurrency: usize) -> anyhow::Result<f64> {
    let count    = Arc::new(AtomicUsize::new(0));
    let deadline = Instant::now() + Duration::from_secs(duration_s);
    let addr     = Arc::new(addr.to_string());

    let handles: Vec<_> = (0..concurrency).map(|_| {
        let count = Arc::clone(&count);
        let addr  = Arc::clone(&addr);
        std::thread::spawn(move || {
            let result = (|| -> anyhow::Result<()> {
                let mut client = H2cClient::connect(&addr)?;
                while Instant::now() < deadline {
                    if client.requests_done >= H2C_MAX_REQUESTS_PER_CONN {
                        client = H2cClient::connect(&addr)?;
                    }
                    match client.get("/") {
                        Ok(_) => { count.fetch_add(1, Ordering::Relaxed); }
                        Err(e) => { eprintln!("h2c throughput error: {e}"); }
                    }
                }
                Ok(())
            })();
            if let Err(e) = result { eprintln!("h2c thread error: {e}"); }
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
    path: &[u8],
) -> anyhow::Result<Vec<u8>> {
    let req = vec![
        quiche::h3::Header::new(b":method", b"GET"),
        quiche::h3::Header::new(b":path", path),
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
        let _ = h3_get(&mut conn, &mut h3, &udp, b"/");
        reqs += 1;
    }

    for _ in 0..n {
        if reqs >= H3_MAX_STREAMS_PER_CONN {
            let (c, h, u) = h3_connect(addr, &mut cfg)?;
            conn = c; h3 = h; udp = u; reqs = 0;
        }
        let t0 = Instant::now();
        h3_get(&mut conn, &mut h3, &udp, b"/")?;
        latencies.push(t0.elapsed().as_secs_f64() * 1_000_000.0);
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
                    match h3_get(&mut conn, &mut h3, &udp, b"/") {
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

fn bench_h3_throughput_path(addr: &str, path: &str, duration_s: u64, concurrency: usize, skip_verify: bool) -> anyhow::Result<f64> {
    let count    = Arc::new(AtomicUsize::new(0));
    let deadline = Instant::now() + Duration::from_secs(duration_s);
    let addr     = Arc::new(addr.to_string());
    let path     = Arc::new(path.to_string());
    let errors   = Arc::new(Mutex::new(Vec::<String>::new()));

    let handles: Vec<_> = (0..concurrency).map(|_| {
        let count  = Arc::clone(&count);
        let addr   = Arc::clone(&addr);
        let path   = Arc::clone(&path);
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
                    match h3_get(&mut conn, &mut h3, &udp, path.as_bytes()) {
                        Ok(_) => { count.fetch_add(1, Ordering::Relaxed); }
                        Err(e) => { eprintln!("h3 url throughput error: {e}"); }
                    }
                }
                Ok(())
            })();
            if let Err(e) = result { errors.lock().unwrap().push(e.to_string()); }
        })
    }).collect();

    let t0 = Instant::now();
    for h in handles { h.join().ok(); }
    let elapsed   = t0.elapsed().as_secs_f64();
    let completed = count.load(Ordering::Relaxed);
    Ok(completed as f64 / elapsed)
}

fn bench_h2c_throughput_path(addr: &str, path: &str, duration_s: u64, concurrency: usize) -> anyhow::Result<f64> {
    let count    = Arc::new(AtomicUsize::new(0));
    let deadline = Instant::now() + Duration::from_secs(duration_s);
    let addr     = Arc::new(addr.to_string());
    let path     = Arc::new(path.to_string());

    let handles: Vec<_> = (0..concurrency).map(|_| {
        let count = Arc::clone(&count);
        let addr  = Arc::clone(&addr);
        let path  = Arc::clone(&path);
        std::thread::spawn(move || {
            let result = (|| -> anyhow::Result<()> {
                let mut client = H2cClient::connect(&addr)?;
                while Instant::now() < deadline {
                    if client.requests_done >= H2C_MAX_REQUESTS_PER_CONN {
                        client = H2cClient::connect(&addr)?;
                    }
                    match client.get(&path) {
                        Ok(_) => { count.fetch_add(1, Ordering::Relaxed); }
                        Err(e) => { eprintln!("h2c url throughput error: {e}"); }
                    }
                }
                Ok(())
            })();
            if let Err(e) = result { eprintln!("h2c url thread error: {e}"); }
        })
    }).collect();

    let t0 = Instant::now();
    for h in handles { h.join().ok(); }
    let elapsed   = t0.elapsed().as_secs_f64();
    let completed = count.load(Ordering::Relaxed);
    Ok(completed as f64 / elapsed)
}

// ── Result reporter ───────────────────────────────────────────────────────────

struct BenchResult {
    name:       String,
    count:      usize,
    p0_us:      f64,
    p1_us:      f64,
    p25_us:     f64,
    p50_us:     f64,
    p75_us:     f64,
    p99_us:     f64,
    p100_us:    f64,
    iqr_us:     f64,
    range_us:   f64,
    avg_us:     f64,
    std_dev_us: f64,
    rps:        f64,
}

impl BenchResult {
    fn from_latencies(name: impl Into<String>, lats: Vec<f64>) -> Self {
        let count  = lats.len();
        let p0     = percentile(lats.clone(),   0.0);
        let p1     = percentile(lats.clone(),   1.0);
        let p25    = percentile(lats.clone(),  25.0);
        let p50    = percentile(lats.clone(),  50.0);
        let p75    = percentile(lats.clone(),  75.0);
        let p99    = percentile(lats.clone(),  99.0);
        let p100   = percentile(lats.clone(), 100.0);
        let iqr    = p75 - p25;
        let range  = p100 - p0;
        let avg    = if count > 0 { lats.iter().sum::<f64>() / count as f64 } else { 0.0 };
        let std_dev = if count > 1 {
            let var = lats.iter().map(|x| (x - avg).powi(2)).sum::<f64>() / (count - 1) as f64;
            var.sqrt()
        } else { 0.0 };
        BenchResult {
            name: name.into(), count,
            p0_us: p0, p1_us: p1, p25_us: p25, p50_us: p50, p75_us: p75,
            p99_us: p99, p100_us: p100,
            iqr_us: iqr, range_us: range, avg_us: avg, std_dev_us: std_dev,
            rps: 0.0,
        }
    }
    fn from_rps(name: impl Into<String>, rps: f64) -> Self {
        BenchResult {
            name: name.into(), count: 0,
            p0_us: 0.0, p1_us: 0.0, p25_us: 0.0, p50_us: 0.0, p75_us: 0.0,
            p99_us: 0.0, p100_us: 0.0,
            iqr_us: 0.0, range_us: 0.0, avg_us: 0.0, std_dev_us: 0.0,
            rps,
        }
    }
}

fn print_result(r: &BenchResult, p99_limit: f64, rps_min: f64) -> bool {
    let p99_ok = r.p99_us <= p99_limit || r.p99_us == 0.0;
    let rps_ok = r.rps >= rps_min || r.rps == 0.0;
    let status = if p99_ok && rps_ok { "PASS" } else { "FAIL" };
    if r.rps > 0.0 {
        println!("{:<6} {:<30}  {:.1} req/s", status, r.name, r.rps);
    } else {
        println!(
            "{:<6} {:<30}  n={:<5}  p0={:8.1}  p1={:8.1}  p25={:8.1}  p50={:8.1}  p75={:8.1}  p99={:8.1}  p100={:8.1}  IQR={:8.1}  range={:8.1}  avg={:8.1}  std={:8.1}  (µs)",
            status, r.name, r.count,
            r.p0_us, r.p1_us, r.p25_us, r.p50_us, r.p75_us,
            r.p99_us, r.p100_us, r.iqr_us, r.range_us, r.avg_us, r.std_dev_us
        );
    }
    if !p99_ok {
        println!("       p99 {:.1}µs exceeds limit {:.1}µs", r.p99_us, p99_limit);
    }
    if !rps_ok {
        println!("       throughput {:.1} req/s below minimum {:.1}", r.rps, rps_min);
    }
    p99_ok && rps_ok
}

// ── Path benchmarks (all protocols) ──────────────────────────────────────────
//
// Four paths × three protocols = 12 measurements.
//
// Paths:
//   cache-hit→html  GET /               m6-html, Cache-Control:public (default)
//   cache-hit→file  GET /assets/hello.txt  m6-file, Cache-Control:public
//   cache-miss→html GET /nocache/        m6-html, cache="no-store" in config
//   cache-miss→file GET /tail/hello.txt  m6-file tail route, no-store
//
// The m6-http in-memory cache is only active on the HTTP/3 path; H1 and H2
// always forward to the backend.  Comparing the same route across protocols
// shows the cache savings directly.

const PATH_HIT_HTML:  &str = "/";
const PATH_HIT_FILE:  &str = "/assets/hello.txt";
const PATH_MISS_HTML: &str = "/nocache/";
const PATH_MISS_FILE: &str = "/tail/hello.txt";

// ── HTTP/1.1 path helpers ─────────────────────────────────────────────────────

fn bench_h1_path_latency(addr: &str, path: &str, n: usize, tls_cfg: Arc<ClientConfig>) -> anyhow::Result<Vec<f64>> {
    bench_http11_latency_path(addr, path, n, tls_cfg)
}

// ── HTTP/2 path helpers ───────────────────────────────────────────────────────

fn bench_h2_path_latency(addr: &str, path: &str, n: usize, skip_verify: bool) -> anyhow::Result<Vec<f64>> {
    bench_http2_latency_path(addr, path, n, skip_verify)
}

// ── HTTP/3 path helpers ───────────────────────────────────────────────────────

fn bench_h3_path_latency(
    addr: &str,
    path: &str,
    n: usize,
    skip_verify: bool,
) -> anyhow::Result<Vec<f64>> {
    const WARMUP: usize = 5;
    let mut cfg = make_quiche_client_config(skip_verify);
    let mut latencies = Vec::with_capacity(n);
    let (mut conn, mut h3, mut udp) = h3_connect(addr, &mut cfg)?;
    let mut reqs: usize = 0;
    let pb = path.as_bytes();

    for _ in 0..WARMUP {
        if reqs >= H3_MAX_STREAMS_PER_CONN {
            let (c, h, u) = h3_connect(addr, &mut cfg)?;
            conn = c; h3 = h; udp = u; reqs = 0;
        }
        h3_get(&mut conn, &mut h3, &udp, pb)?;
        reqs += 1;
    }
    for _ in 0..n {
        if reqs >= H3_MAX_STREAMS_PER_CONN {
            let (c, h, u) = h3_connect(addr, &mut cfg)?;
            conn = c; h3 = h; udp = u; reqs = 0;
        }
        let t0 = Instant::now();
        h3_get(&mut conn, &mut h3, &udp, pb)?;
        latencies.push(t0.elapsed().as_secs_f64() * 1_000_000.0);
        reqs += 1;
    }
    Ok(latencies)
}

// ── run_path_benchmarks: called for H1, H2, or H3 ────────────────────────────

fn run_path_benchmarks(
    label: &str,
    addr: &str,
    n: usize,
    skip_verify: bool,
    proto: u8,           // 1=H1, 2=H2, 3=H3, 4=H2C
    tls_cfg: Option<Arc<ClientConfig>>,
    all_pass: &mut bool,
) {
    println!("{:-<70}", "");
    println!("Path benchmarks — {label} (n={n}):");

    let paths = [
        ("cache-hit→m6-html",  PATH_HIT_HTML),
        ("cache-hit→m6-file",  PATH_HIT_FILE),
        ("cache-miss→m6-html", PATH_MISS_HTML),
        ("cache-miss→m6-file", PATH_MISS_FILE),
    ];

    for (name, path) in paths {
        let result = match proto {
            1 => bench_h1_path_latency(addr, path, n, Arc::clone(tls_cfg.as_ref().unwrap())),
            2 => bench_h2_path_latency(addr, path, n, skip_verify),
            3 => bench_h3_path_latency(addr, path, n, skip_verify),
            4 => bench_h2c_latency_path(addr, path, n),
            _ => unreachable!(),
        };
        match result {
            Ok(lats) => { print_result(&BenchResult::from_latencies(name, lats), f64::INFINITY, 0.0); }
            Err(e) => { eprintln!("{name} error: {e}"); *all_pass = false; }
        }
    }
}

// ── main ──────────────────────────────────────────────────────────────────────

fn main() {
    rustls::crypto::ring::default_provider().install_default().ok();
    let args = Args::parse();
    let mut all_pass = true;

    let run_latency    = !args.throughput_only && !args.path_only;
    let run_path       = !args.latency_only    && !args.throughput_only;
    let run_throughput = !args.latency_only    && !args.path_only;

    println!("m6-bench  target={}  h2c-target={}  skip-verify={}  http11={}  http2={}  http3={}  h2c={}  url={}",
             args.addr, args.h2c_addr, args.skip_verify, args.http11, args.http2, args.http3, args.h2c, args.url);
    println!("{:-<70}", "");

    // ── Latency ───────────────────────────────────────────────────────────────
    if run_latency {
        if args.http11 {
            let tls_cfg = make_client_config(args.skip_verify);
            match bench_http11_latency(&args.addr, args.latency_n, Arc::clone(&tls_cfg)) {
                Ok(lats) => { print_result(&BenchResult::from_latencies("HTTP/1.1 latency", lats), f64::INFINITY, 0.0); }
                Err(e)   => { eprintln!("HTTP/1.1 latency error: {e}"); all_pass = false; }
            }
        }
        if args.http2 {
            match bench_http2_latency(&args.addr, args.latency_n, args.skip_verify) {
                Ok(lats) => { if !print_result(&BenchResult::from_latencies("HTTP/2 latency", lats), args.p99_limit_us, 0.0) { all_pass = false; } }
                Err(e)   => { eprintln!("HTTP/2 latency error: {e}"); all_pass = false; }
            }
        }
        if args.http3 {
            match bench_http3_latency(&args.addr, args.latency_n, args.skip_verify) {
                Ok(lats) => { if !print_result(&BenchResult::from_latencies("HTTP/3 latency", lats), args.p99_limit_us, 0.0) { all_pass = false; } }
                Err(e)   => { eprintln!("HTTP/3 latency error: {e}"); all_pass = false; }
            }
        }
        if args.h2c {
            match bench_h2c_latency(&args.h2c_addr, args.latency_n) {
                Ok(lats) => { if !print_result(&BenchResult::from_latencies("H2C latency", lats), args.p99_limit_us, 0.0) { all_pass = false; } }
                Err(e)   => { eprintln!("H2C latency error: {e}"); all_pass = false; }
            }
        }
    }

    // ── Path benchmarks ───────────────────────────────────────────────────────
    // bench.sh restarts the server before each suite, so no stale-connection
    // contamination.  Each protocol invocation is an independent fresh run.
    if run_path {
        if args.http11 {
            let tls_cfg = make_client_config(args.skip_verify);
            run_path_benchmarks("HTTP/1.1", &args.addr, args.latency_n,
                args.skip_verify, 1, Some(Arc::clone(&tls_cfg)), &mut all_pass);
        }
        if args.http2 {
            run_path_benchmarks("HTTP/2", &args.addr, args.latency_n,
                args.skip_verify, 2, None, &mut all_pass);
        }
        if args.http3 {
            run_path_benchmarks("HTTP/3", &args.addr, args.latency_n,
                args.skip_verify, 3, None, &mut all_pass);
        }
        if args.h2c {
            run_path_benchmarks("H2C", &args.h2c_addr, args.latency_n,
                false, 4, None, &mut all_pass);
        }
    }

    // ── Throughput ────────────────────────────────────────────────────────────
    if run_throughput {
        println!("{:-<70}", "");
        if args.http11 {
            let tls_cfg = make_client_config(args.skip_verify);
            match bench_http11_throughput(&args.addr, args.duration_s, args.concurrency, Arc::clone(&tls_cfg)) {
                Ok(rps) => { if !print_result(&BenchResult::from_rps("HTTP/1.1 throughput", rps), 0.0, args.rps_min) { all_pass = false; } }
                Err(e)  => { eprintln!("HTTP/1.1 throughput error: {e}"); all_pass = false; }
            }
        }
        if args.http2 {
            match bench_http2_throughput(&args.addr, args.duration_s, args.concurrency, args.skip_verify) {
                Ok(rps) => { if !print_result(&BenchResult::from_rps("HTTP/2 throughput", rps), 0.0, args.rps_min) { all_pass = false; } }
                Err(e)  => { eprintln!("HTTP/2 throughput error: {e}"); all_pass = false; }
            }
        }
        if args.http3 {
            match bench_http3_throughput(&args.addr, args.duration_s, args.concurrency, args.skip_verify) {
                Ok(rps) => { if !print_result(&BenchResult::from_rps("HTTP/3 throughput", rps), 0.0, args.rps_min) { all_pass = false; } }
                Err(e)  => { eprintln!("HTTP/3 throughput error: {e}"); all_pass = false; }
            }
        }
        if args.h2c {
            match bench_h2c_throughput(&args.h2c_addr, args.duration_s, args.concurrency) {
                Ok(rps) => { if !print_result(&BenchResult::from_rps("H2C throughput", rps), 0.0, args.rps_min) { all_pass = false; } }
                Err(e)  => { eprintln!("H2C throughput error: {e}"); all_pass = false; }
            }
        }
    }

    // ── URL-backend suites: all inbound × all URL outbound ───────────────────
    // Produces a 4×4 matrix: (h1|h2|h3|h2c) inbound × (http|https|h2c|h2s) outbound.
    // Routes /url/http/, /url/https/, /url/h2c/, /url/h2s/ must be in site.toml.
    if args.url {
        const URL_ROUTES: &[(&str, &str)] = &[
            ("http",  "/url/http/"),
            ("https", "/url/https/"),
            ("h2c",   "/url/h2c/"),
            ("h2s",   "/url/h2s/"),
        ];
        let tls_addr = args.addr.as_str();
        let h2c_addr = args.h2c_addr.as_str();
        // (label, addr, proto_id)  proto_id: 1=h1, 2=h2, 3=h3, 4=h2c
        let inbounds: &[(&str, &str, u8)] = &[
            ("h1",  tls_addr, 1),
            ("h2",  tls_addr, 2),
            ("h3",  tls_addr, 3),
            ("h2c", h2c_addr, 4),
        ];
        if run_latency {
            println!("{:-<70}", "");
            for &(inbound, addr, proto) in inbounds {
                for &(outbound, path) in URL_ROUTES {
                    let name = format!("{inbound}→{outbound} latency");
                    let result: anyhow::Result<Vec<f64>> = match proto {
                        1 => { let cfg = make_client_config(args.skip_verify); bench_http11_latency_path(addr, path, args.latency_n, cfg) }
                        2 => bench_http2_latency_path(addr, path, args.latency_n, args.skip_verify),
                        3 => bench_h3_path_latency(addr, path, args.latency_n, args.skip_verify),
                        4 => bench_h2c_latency_path(addr, path, args.latency_n),
                        _ => unreachable!(),
                    };
                    match result {
                        Ok(lats) => { if !print_result(&BenchResult::from_latencies(&name, lats), args.p99_limit_us, 0.0) { all_pass = false; } }
                        Err(e)   => { eprintln!("{name} error: {e}"); all_pass = false; }
                    }
                }
                println!(); // blank line between inbound groups
            }
        }
        if run_throughput {
            println!("{:-<70}", "");
            for &(inbound, addr, proto) in inbounds {
                for &(outbound, path) in URL_ROUTES {
                    let name = format!("{inbound}→{outbound} throughput");
                    let result: anyhow::Result<f64> = match proto {
                        1 => { let cfg = make_client_config(args.skip_verify); bench_http11_throughput_path(addr, path, args.duration_s, args.concurrency, cfg) }
                        2 => bench_http2_throughput_path(addr, path, args.duration_s, args.concurrency, args.skip_verify),
                        3 => bench_h3_throughput_path(addr, path, args.duration_s, args.concurrency, args.skip_verify),
                        4 => bench_h2c_throughput_path(addr, path, args.duration_s, args.concurrency),
                        _ => unreachable!(),
                    };
                    match result {
                        Ok(rps) => { if !print_result(&BenchResult::from_rps(&name, rps), 0.0, args.rps_min) { all_pass = false; } }
                        Err(e)  => { eprintln!("{name} error: {e}"); all_pass = false; }
                    }
                }
                println!(); // blank line between inbound groups
            }
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
