#![allow(
    clippy::disallowed_methods,
    reason = "a load generator, not a service: it measures from outside"
)]

/// m6-bench — loopback latency + throughput benchmark for m6-http.
///
/// Modes (flags can be combined):
///   --http11-only       only run HTTP/1.1 suites
///   --http2-only        only run HTTP/2 suites
///   --http3-only        only run HTTP/3 suites
///   --h2c-only          only run H2C suites (HTTP/2 cleartext, for a private backbone)
///   --url-only          only run URL-backend suites (h2 inbound, http/https/h2c/h2s outbound)
///   --h1-vs-h2          run HTTP/1.1 and HTTP/2 latency in one process and fail if the
///                       keep-alive HTTP/1.1 p50 exceeds H1_VS_H2_P50_MAX_RATIO times h2's
///   --h2c               include H2C suites alongside the selected protocols
///   --url               include URL-backend suites alongside the selected protocols
///   --mixed-handshake   hot-hit p50/p99 while full TLS handshakes arrive at a set rate
///                       (--mixed-rates 0,50,100,200,500 --mixed-seconds 10 --mixed-path /)
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
use std::os::fd::AsFd;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use m6_core::platform::net::{RecvMeta, UdpBatch, MAX_BATCH};
use m6_http_lib::probe;
use rand::Rng;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::ClientConfig;

// ── CLI ───────────────────────────────────────────────────────────────────────

struct Args {
    http11: bool,
    http2: bool,
    http3: bool,
    h2c: bool,
    /// Include URL-backend suites (http/https/h2c/h2s outbound via m6-http H2 inbound).
    url: bool,
    skip_verify: bool,
    latency_only: bool,
    throughput_only: bool,
    path_only: bool,
    latency_n: usize,
    duration_s: u64,
    concurrency: usize,
    p99_limit_us: f64,
    rps_min: f64,
    addr: String,
    h2c_addr: String,
    /// Run the handshake suite instead of the request suites.
    handshake: bool,
    /// `full`, `resumed` or `early` (QUIC 0-RTT).
    handshake_kind: String,
    handshake_n: usize,
    /// Run the mixed hot-hit suite instead of the request suites.
    mixed_handshake: bool,
    /// Handshake arrival rates to run, in new full handshakes per second.
    mixed_rates: Vec<u32>,
    /// Seconds of hits and handshake load at each rate.
    mixed_seconds: u64,
    /// The hot path fetched on the kept-alive connection.
    mixed_path: String,
}

impl Args {
    fn parse() -> Self {
        let mut a = Args {
            http11: true,
            http2: true,
            http3: true,
            h2c: false,
            url: false,
            skip_verify: false,
            latency_only: false,
            throughput_only: false,
            path_only: false,
            latency_n: 2000,
            duration_s: 10,
            concurrency: 8,
            p99_limit_us: 50_000.0,
            rps_min: 50.0,
            addr: "127.0.0.1:8443".into(),
            h2c_addr: "127.0.0.1:8080".into(),
            handshake: false,
            handshake_kind: "full".into(),
            handshake_n: 1000,
            mixed_handshake: false,
            mixed_rates: vec![0, 50, 100, 200, 500],
            mixed_seconds: 10,
            mixed_path: "/".into(),
        };
        let raw: Vec<String> = std::env::args().skip(1).collect();
        let mut i = 0;
        while i < raw.len() {
            match raw[i].as_str() {
                "--http11-only" => {
                    a.http11 = true;
                    a.http2 = false;
                    a.http3 = false;
                    a.h2c = false;
                    a.url = false;
                }
                "--http2-only" => {
                    a.http11 = false;
                    a.http2 = true;
                    a.http3 = false;
                    a.h2c = false;
                    a.url = false;
                }
                "--http3-only" => {
                    a.http11 = false;
                    a.http2 = false;
                    a.http3 = true;
                    a.h2c = false;
                    a.url = false;
                }
                "--h2c-only" => {
                    a.http11 = false;
                    a.http2 = false;
                    a.http3 = false;
                    a.h2c = true;
                    a.url = false;
                }
                "--url-only" => {
                    a.http11 = false;
                    a.http2 = false;
                    a.http3 = false;
                    a.h2c = false;
                    a.url = true;
                }
                "--h1-vs-h2" => {
                    a.http11 = true;
                    a.http2 = true;
                    a.http3 = false;
                    a.h2c = false;
                    a.url = false;
                }
                "--handshake" => a.handshake = true,
                "--mixed-handshake" => a.mixed_handshake = true,
                "--mixed-rates" => {
                    i += 1;
                    a.mixed_rates = parse_rate_list(&raw[i]);
                }
                "--mixed-seconds" => {
                    i += 1;
                    a.mixed_seconds = raw[i].parse().expect("mixed-seconds");
                }
                "--mixed-path" => {
                    i += 1;
                    a.mixed_path = raw[i].clone();
                }
                "--handshake-kind" => {
                    i += 1;
                    a.handshake_kind = raw[i].clone();
                }
                "--handshake-n" => {
                    i += 1;
                    a.handshake_n = raw[i].parse().expect("handshake-n");
                }
                "--h2c" => a.h2c = true,
                "--url" => a.url = true,
                "--skip-verify" => a.skip_verify = true,
                "--latency-only" => a.latency_only = true,
                "--throughput-only" => a.throughput_only = true,
                "--path-only" => a.path_only = true,
                "--latency-n" => {
                    i += 1;
                    a.latency_n = raw[i].parse().expect("latency-n");
                }
                "--duration" => {
                    i += 1;
                    a.duration_s = raw[i].parse().expect("duration");
                }
                "--concurrency" => {
                    i += 1;
                    a.concurrency = raw[i].parse().expect("concurrency");
                }
                "--p99-limit-us" => {
                    i += 1;
                    a.p99_limit_us = raw[i].parse().expect("p99-limit-us");
                }
                "--rps-min" => {
                    i += 1;
                    a.rps_min = raw[i].parse().expect("rps-min");
                }
                "--addr" => {
                    i += 1;
                    a.addr = raw[i].clone();
                }
                "--h2c-addr" => {
                    i += 1;
                    a.h2c_addr = raw[i].clone();
                }
                other => {
                    eprintln!("Unknown flag: {other}");
                    std::process::exit(1);
                }
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
        rustls::crypto::aws_lc_rs::default_provider()
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
    if resp.len() < 12 {
        return 0;
    }
    let s = std::str::from_utf8(&resp[9..12]).unwrap_or("0");
    s.parse().unwrap_or(0)
}

// ── Percentile helper ─────────────────────────────────────────────────────────

fn percentile(mut v: Vec<f64>, pct: usize) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    if v.is_empty() {
        return 0.0;
    }
    // Same rule as stats.rs: index (n-1)*pct/100 with integer arithmetic,
    // truncating. Rounding the same fraction shifts p50 of ten samples from
    // index 4 to 5, so the bench would disagree with the server's own
    // metrics document.
    let idx = (v.len() - 1) * pct / 100;
    v[idx.min(v.len() - 1)]
}

// ── HTTP/1.1 latency ──────────────────────────────────────────────────────────

fn bench_http11_latency(
    addr: &str,
    n: usize,
    tls_cfg: Arc<ClientConfig>,
) -> anyhow::Result<Vec<f64>> {
    bench_http11_latency_path(addr, "/", n, tls_cfg)
}

fn bench_http11_latency_path(
    addr: &str,
    path: &str,
    n: usize,
    tls_cfg: Arc<ClientConfig>,
) -> anyhow::Result<Vec<f64>> {
    let mut latencies = Vec::with_capacity(n);
    for _ in 0..5 {
        http11_get_path(addr, path, Arc::clone(&tls_cfg))?;
    }
    for _ in 0..n {
        let t0 = m6_core::platform::now();
        let resp = http11_get_path(addr, path, Arc::clone(&tls_cfg))?;
        let elapsed = t0.elapsed().as_secs_f64() * 1_000_000.0;
        let status = parse_http11_status(&resp);
        if status != 200 {
            eprintln!("HTTP/1.1 non-200 ({path}): {status}");
        }
        latencies.push(elapsed);
    }
    Ok(latencies)
}

// ── HTTP/1.1 keep-alive latency ───────────────────────────────────────────────

/// One TLS connection reused for many requests, like a browser. Blocking
/// sockets with explicit `read_tls`/`write_tls`, as `rustls::Stream` stops
/// reading once the handshake is done and the response is still in flight.
struct H1Client {
    conn: rustls::ClientConnection,
    stream: TcpStream,
    requests_done: usize,
}

/// The server closes a connection after `[server] max_requests_per_connection`
/// (default 100) requests. Reconnect before that, outside the timed region.
/// A cache hit over a kept-alive HTTP/1.1 connection costs about what one costs
/// over HTTP/2 (measured 0.65 times, dev build, loopback). Twice is the bound
/// above which something on the h1 path, such as Nagle, has regressed. A
/// connection per request is a handshake, not a hit, and is not compared.
const H1_VS_H2_P50_MAX_RATIO: f64 = 2.0;

const H1_MAX_REQUESTS_PER_CONN: usize = 90;

impl H1Client {
    fn connect(addr: &str, tls_cfg: Arc<ClientConfig>) -> anyhow::Result<Self> {
        let stream = TcpStream::connect(addr)?;
        stream.set_nodelay(true)?;
        stream.set_read_timeout(Some(Duration::from_secs(5)))?;
        let server_name: ServerName<'static> = "localhost".try_into().unwrap();
        let conn = rustls::ClientConnection::new(tls_cfg, server_name)?;
        let mut c = H1Client {
            conn,
            stream,
            requests_done: 0,
        };
        while c.conn.is_handshaking() {
            c.pump()?;
        }
        Ok(c)
    }

    /// Flush pending TLS output, then read one batch of TLS records if the
    /// connection wants input.
    fn pump(&mut self) -> anyhow::Result<()> {
        while self.conn.wants_write() {
            self.conn.write_tls(&mut &self.stream)?;
        }
        if self.conn.wants_read() {
            if self.conn.read_tls(&mut &self.stream)? == 0 {
                anyhow::bail!("H1 server closed the connection");
            }
            self.conn.process_new_packets()?;
        }
        Ok(())
    }

    fn get(&mut self, path: &str) -> anyhow::Result<Vec<u8>> {
        self.requests_done += 1;
        let req = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\n\r\n");
        self.conn.writer().write_all(req.as_bytes())?;
        while self.conn.wants_write() {
            self.conn.write_tls(&mut &self.stream)?;
        }
        let mut resp = Vec::new();
        let mut need: Option<usize> = None; // total bytes, once the head is parsed
        loop {
            let mut tmp = [0u8; 8192];
            match self.conn.reader().read(&mut tmp) {
                Ok(0) => anyhow::bail!("H1 server closed the connection"),
                Ok(n) => resp.extend_from_slice(&tmp[..n]),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                Err(e) => return Err(e.into()),
            }
            if need.is_none() {
                if let Some(end) = resp.windows(4).position(|w| w == b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&resp[..end]).to_ascii_lowercase();
                    let len = head
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length:"))
                        .and_then(|v| v.trim().parse::<usize>().ok())
                        .unwrap_or(0);
                    need = Some(end + 4 + len);
                }
            }
            if need.is_some_and(|n| resp.len() >= n) {
                return Ok(resp);
            }
            self.pump()?;
        }
    }
}

/// HTTP/1.1 over one kept-alive connection: the number to compare with h2,
/// h3 and h2c, which also reuse their connection.
fn bench_http11_keepalive_latency_path(
    addr: &str,
    path: &str,
    n: usize,
    tls_cfg: Arc<ClientConfig>,
) -> anyhow::Result<Vec<f64>> {
    let mut latencies = Vec::with_capacity(n);
    let mut client = H1Client::connect(addr, Arc::clone(&tls_cfg))?;
    for _ in 0..5 {
        client.get(path)?;
    }
    for _ in 0..n {
        if client.requests_done >= H1_MAX_REQUESTS_PER_CONN {
            client = H1Client::connect(addr, Arc::clone(&tls_cfg))?;
        }
        let t0 = m6_core::platform::now();
        let resp = client.get(path)?;
        let elapsed = t0.elapsed().as_secs_f64() * 1_000_000.0;
        let status = parse_http11_status(&resp);
        if status != 200 {
            eprintln!("HTTP/1.1 non-200 ({path}): {status}");
        }
        latencies.push(elapsed);
    }
    Ok(latencies)
}

// ── HTTP/1.1 throughput ───────────────────────────────────────────────────────

fn bench_http11_throughput(
    addr: &str,
    duration_s: u64,
    concurrency: usize,
    tls_cfg: Arc<ClientConfig>,
) -> anyhow::Result<f64> {
    let count = Arc::new(AtomicUsize::new(0));
    let deadline = m6_core::platform::now() + Duration::from_secs(duration_s);
    let addr = Arc::new(addr.to_string());

    let handles: Vec<_> = (0..concurrency)
        .map(|_| {
            let tls_cfg = Arc::clone(&tls_cfg);
            let count = Arc::clone(&count);
            let addr = Arc::clone(&addr);
            std::thread::spawn(move || {
                while m6_core::platform::now() < deadline {
                    if http11_get(&addr, Arc::clone(&tls_cfg)).is_ok() {
                        count.fetch_add(1, Ordering::Relaxed);
                    }
                }
            })
        })
        .collect();
    let t0 = m6_core::platform::now();
    for h in handles {
        h.join().ok();
    }
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
        debug_assert!(
            pb.len() < 128,
            "path too long for simple HPACK length encoding"
        );
        h.push(pb.len() as u8);
        h.extend_from_slice(pb);
    }
    h.push(0x87); // :scheme https
                  // :authority localhost — literal with incremental indexing, name index 1
    h.extend_from_slice(&[
        0x41, 0x09, b'l', b'o', b'c', b'a', b'l', b'h', b'o', b's', b't',
    ]);
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
        debug_assert!(
            pb.len() < 128,
            "path too long for simple HPACK length encoding"
        );
        h.push(pb.len() as u8);
        h.extend_from_slice(pb);
    }
    h.push(0x86); // :scheme http (static table index 6; index 7 = https)
    h.extend_from_slice(&[
        0x41, 0x09, b'l', b'o', b'c', b'a', b'l', b'h', b'o', b's', b't',
    ]);
    h
}

fn make_h2_frame(ftype: u8, flags: u8, stream_id: u32, payload: &[u8]) -> Vec<u8> {
    let len = payload.len();
    let mut f = Vec::with_capacity(9 + len);
    f.push((len >> 16) as u8);
    f.push((len >> 8) as u8);
    f.push(len as u8);
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
    if buf.len() < 9 {
        return None;
    }
    let length = ((buf[0] as usize) << 16) | ((buf[1] as usize) << 8) | buf[2] as usize;
    let total = 9 + length;
    if buf.len() < total {
        return None;
    }
    let ftype = buf[3];
    let flags = buf[4];
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
        for cert in rustls_native_certs::load_native_certs().certs {
            roots.add(cert).ok();
        }
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
    conn: rustls::ClientConnection,
    stream: TcpStream,
    recv_buf: Vec<u8>,
    tmp: [u8; 8192],
    next_stream_id: u32,
    requests_done: usize,
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
            conn,
            stream,
            recv_buf: Vec::with_capacity(16_384),
            tmp: [0u8; 8192],
            next_stream_id: 1,
            requests_done: 0,
        };

        // Queue preface + empty SETTINGS for sending alongside the TLS handshake.
        c.conn.writer().write_all(H2_CLIENT_PREFACE)?;
        c.conn
            .writer()
            .write_all(&make_h2_frame(0x4, 0x0, 0, &[]))?;

        // Drive TLS handshake: alternate writes and blocking reads until established.
        let deadline = m6_core::platform::now() + Duration::from_secs(5);
        loop {
            c.flush_write()?;
            if !c.conn.is_handshaking() {
                break;
            }
            c.fill_recv_deadline(deadline)?; // spin until next TLS record arrives
        }
        // Drain any buffered server data (SETTINGS, NewSessionTickets, etc.).
        c.fill_recv_drain()?;
        // Acknowledge the server's SETTINGS frame (RFC 9113 §6.5 requirement).
        c.conn
            .writer()
            .write_all(&make_h2_frame(0x4, 0x1, 0, &[]))?;
        c.flush_write()?;
        Ok(c)
    }

    /// Write all pending TLS output to the socket (non-blocking, with retry).
    /// Returns the total number of TLS bytes written to the socket.
    fn flush_write(&mut self) -> io::Result<usize> {
        let mut total = 0;
        loop {
            let r = {
                let mut sr = &self.stream;
                self.conn.write_tls(&mut sr)
            };
            match r {
                Ok(0) => break,
                Ok(n) => {
                    total += n;
                }
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
            let r = {
                let mut sr = &self.stream;
                self.conn.read_tls(&mut sr)
            };
            match r {
                Ok(0) => return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "closed")),
                Ok(_) => {
                    self.conn
                        .process_new_packets()
                        .map_err(|e| io::Error::other(e.to_string()))?;
                    break; // got at least one TLS record
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    let remaining = deadline.saturating_duration_since(m6_core::platform::now());
                    if remaining.is_zero() {
                        return Err(io::Error::new(io::ErrorKind::TimedOut, "deadline"));
                    }
                    // Flush any pending writes before sleeping so the server
                    // has our request before we block waiting for its response.
                    self.flush_write()?;
                    let timeout_ms = remaining.as_millis().min(100) as i32;
                    let _ = m6_core::platform::wait_readable(
                        self.stream.as_fd(),
                        Duration::from_millis(timeout_ms as u64),
                    );
                }
                Err(e) => return Err(e),
            }
        }
        self.drain_plaintext()
    }

    /// Drain whatever TLS records are already buffered (non-blocking).
    fn fill_recv_drain(&mut self) -> io::Result<()> {
        loop {
            let r = {
                let mut sr = &self.stream;
                self.conn.read_tls(&mut sr)
            };
            match r {
                Ok(0) => return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "closed")),
                Ok(_) => {
                    self.conn
                        .process_new_packets()
                        .map_err(|e| io::Error::other(e.to_string()))?;
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
                Ok(0) => break,
                Ok(n) => self.recv_buf.extend_from_slice(&self.tmp[..n]),
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
        self.conn
            .writer()
            .write_all(&make_h2_frame(0x1, 0x05, sid, &headers))?;
        self.flush_write()?;

        let mut body = Vec::new();
        let deadline = m6_core::platform::now() + Duration::from_secs(5);

        loop {
            if m6_core::platform::now() > deadline {
                anyhow::bail!("H2 response timeout");
            }

            if let Some((ftype, flags, fsid, total)) = try_parse_h2_frame(&self.recv_buf) {
                let payload = self.recv_buf[9..total].to_vec();
                self.recv_buf.drain(..total);
                match ftype {
                    0x0
                        // DATA
                        if fsid == sid => {
                            let data_len = payload.len() as u32;
                            body.extend_from_slice(&payload);
                            if data_len > 0 {
                                // Send WINDOW_UPDATE for connection (sid=0) and stream
                                self.conn.writer().write_all(&make_h2_frame(
                                    0x8,
                                    0,
                                    0,
                                    &data_len.to_be_bytes(),
                                ))?;
                                self.conn.writer().write_all(&make_h2_frame(
                                    0x8,
                                    0,
                                    sid,
                                    &data_len.to_be_bytes(),
                                ))?;
                                self.flush_write()?;
                            }
                            if flags & 0x1 != 0 {
                                return Ok(body);
                            } // END_STREAM
                        }
                    0x1 if fsid == sid
                        && flags & 0x1 != 0 => {
                            return Ok(body); // HEADERS with END_STREAM (no body)
                        }
                        // HEADERS without END_STREAM: response headers only, body follows
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

fn bench_http2_latency_path(
    addr: &str,
    path: &str,
    n: usize,
    skip_verify: bool,
) -> anyhow::Result<Vec<f64>> {
    let tls_cfg = make_client_config_h2(skip_verify);
    let mut latencies = Vec::with_capacity(n);
    let mut client = H2Client::connect(addr, Arc::clone(&tls_cfg))?;

    for _ in 0..5 {
        client.get(path)?;
    } // warmup

    for _ in 0..n {
        if client.requests_done >= H2_MAX_REQUESTS_PER_CONN {
            client = H2Client::connect(addr, Arc::clone(&tls_cfg))?;
        }
        let t0 = m6_core::platform::now();
        client.get(path)?;
        latencies.push(t0.elapsed().as_secs_f64() * 1_000_000.0);
    }
    Ok(latencies)
}

fn bench_http2_throughput(
    addr: &str,
    duration_s: u64,
    concurrency: usize,
    skip_verify: bool,
) -> anyhow::Result<f64> {
    let count = Arc::new(AtomicUsize::new(0));
    let deadline = m6_core::platform::now() + Duration::from_secs(duration_s);
    let addr = Arc::new(addr.to_string());

    let handles: Vec<_> = (0..concurrency)
        .map(|_| {
            let count = Arc::clone(&count);
            let addr = Arc::clone(&addr);
            std::thread::spawn(move || {
                let tls_cfg = make_client_config_h2(skip_verify);
                let result = (|| -> anyhow::Result<()> {
                    let mut client = H2Client::connect(&addr, Arc::clone(&tls_cfg))?;
                    while m6_core::platform::now() < deadline {
                        if client.requests_done >= H2_MAX_REQUESTS_PER_CONN {
                            client = H2Client::connect(&addr, Arc::clone(&tls_cfg))?;
                        }
                        match client.get("/") {
                            Ok(_) => {
                                count.fetch_add(1, Ordering::Relaxed);
                            }
                            Err(e) => {
                                eprintln!("h2 throughput error: {e}");
                            }
                        }
                    }
                    Ok(())
                })();
                if let Err(e) = result {
                    eprintln!("h2 thread error: {e}");
                }
            })
        })
        .collect();

    let t0 = m6_core::platform::now();
    for h in handles {
        h.join().ok();
    }
    let elapsed = t0.elapsed().as_secs_f64();
    let completed = count.load(Ordering::Relaxed);
    Ok(completed as f64 / elapsed)
}

fn bench_http11_throughput_path(
    addr: &str,
    path: &str,
    duration_s: u64,
    concurrency: usize,
    tls_cfg: Arc<ClientConfig>,
) -> anyhow::Result<f64> {
    let count = Arc::new(AtomicUsize::new(0));
    let deadline = m6_core::platform::now() + Duration::from_secs(duration_s);
    let addr = Arc::new(addr.to_string());
    let path = Arc::new(path.to_string());

    let handles: Vec<_> = (0..concurrency)
        .map(|_| {
            let tls_cfg = Arc::clone(&tls_cfg);
            let count = Arc::clone(&count);
            let addr = Arc::clone(&addr);
            let path = Arc::clone(&path);
            std::thread::spawn(move || {
                while m6_core::platform::now() < deadline {
                    if http11_get_path(&addr, &path, Arc::clone(&tls_cfg)).is_ok() {
                        count.fetch_add(1, Ordering::Relaxed);
                    }
                }
            })
        })
        .collect();

    let t0 = m6_core::platform::now();
    for h in handles {
        h.join().ok();
    }
    let elapsed = t0.elapsed().as_secs_f64();
    let completed = count.load(Ordering::Relaxed);
    Ok(completed as f64 / elapsed)
}

fn bench_http2_throughput_path(
    addr: &str,
    path: &str,
    duration_s: u64,
    concurrency: usize,
    skip_verify: bool,
) -> anyhow::Result<f64> {
    let count = Arc::new(AtomicUsize::new(0));
    let deadline = m6_core::platform::now() + Duration::from_secs(duration_s);
    let addr = Arc::new(addr.to_string());
    let path = Arc::new(path.to_string());

    let handles: Vec<_> = (0..concurrency)
        .map(|_| {
            let count = Arc::clone(&count);
            let addr = Arc::clone(&addr);
            let path = Arc::clone(&path);
            std::thread::spawn(move || {
                let tls_cfg = make_client_config_h2(skip_verify);
                let result = (|| -> anyhow::Result<()> {
                    let mut client = H2Client::connect(&addr, Arc::clone(&tls_cfg))?;
                    while m6_core::platform::now() < deadline {
                        if client.requests_done >= H2_MAX_REQUESTS_PER_CONN {
                            client = H2Client::connect(&addr, Arc::clone(&tls_cfg))?;
                        }
                        match client.get(&path) {
                            Ok(_) => {
                                count.fetch_add(1, Ordering::Relaxed);
                            }
                            Err(e) => {
                                eprintln!("h2 url throughput error: {e}");
                            }
                        }
                    }
                    Ok(())
                })();
                if let Err(e) = result {
                    eprintln!("h2 url thread error: {e}");
                }
            })
        })
        .collect();

    let t0 = m6_core::platform::now();
    for h in handles {
        h.join().ok();
    }
    let elapsed = t0.elapsed().as_secs_f64();
    let completed = count.load(Ordering::Relaxed);
    Ok(completed as f64 / elapsed)
}

// ── H2C (HTTP/2 cleartext) helpers ────────────────────────────────────────────
//
// Mirrors H2Client but connects over plain TCP without TLS.
// Intended for the h2c_bind port (e.g. 127.0.0.1:8080) used over a private backbone.

/// Persistent HTTP/2 cleartext client over plain TCP.
struct H2cClient {
    stream: TcpStream,
    send_buf: Vec<u8>,
    recv_buf: Vec<u8>,
    tmp: [u8; 8192],
    next_stream_id: u32,
    requests_done: usize,
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
        let deadline = m6_core::platform::now() + Duration::from_secs(5);
        let mut got_settings = false;
        while !got_settings {
            if m6_core::platform::now() > deadline {
                anyhow::bail!("H2C setup timeout");
            }
            let n = (&stream).read(&mut tmp)?;
            if n == 0 {
                anyhow::bail!("H2C: connection closed during setup");
            }
            recv_buf.extend_from_slice(&tmp[..n]);
            while let Some((ftype, flags, _, total)) = try_parse_h2_frame(&recv_buf) {
                if ftype == 0x4 && flags & 0x1 == 0 {
                    got_settings = true;
                }
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
            send_buf: Vec::with_capacity(4096),
            recv_buf,
            tmp: [0u8; 8192],
            next_stream_id: 1,
            requests_done: 0,
        })
    }

    fn flush_write(&mut self) -> io::Result<()> {
        loop {
            if self.send_buf.is_empty() {
                break;
            }
            match (&self.stream).write(&self.send_buf) {
                Ok(0) => break,
                Ok(n) => {
                    self.send_buf.drain(..n);
                }
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
                Ok(n) => {
                    self.recv_buf.extend_from_slice(&self.tmp[..n]);
                    break;
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    let remaining = deadline.saturating_duration_since(m6_core::platform::now());
                    if remaining.is_zero() {
                        return Err(io::Error::new(io::ErrorKind::TimedOut, "deadline"));
                    }
                    self.flush_write()?;
                    let timeout_ms = remaining.as_millis().min(100) as i32;
                    let _ = m6_core::platform::wait_readable(
                        self.stream.as_fd(),
                        Duration::from_millis(timeout_ms as u64),
                    );
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
        self.send_buf
            .extend_from_slice(&make_h2_frame(0x1, 0x05, sid, &headers));
        self.flush_write()?;

        let mut body = Vec::new();
        let deadline = m6_core::platform::now() + Duration::from_secs(5);

        loop {
            if m6_core::platform::now() > deadline {
                anyhow::bail!("H2C response timeout");
            }

            if let Some((ftype, flags, fsid, total)) = try_parse_h2_frame(&self.recv_buf) {
                let payload = self.recv_buf[9..total].to_vec();
                self.recv_buf.drain(..total);
                match ftype {
                    0x0 if fsid == sid => {
                        let data_len = payload.len() as u32;
                        body.extend_from_slice(&payload);
                        if data_len > 0 {
                            self.send_buf.extend_from_slice(&make_h2_frame(
                                0x8,
                                0,
                                0,
                                &data_len.to_be_bytes(),
                            ));
                            self.send_buf.extend_from_slice(&make_h2_frame(
                                0x8,
                                0,
                                sid,
                                &data_len.to_be_bytes(),
                            ));
                            self.flush_write()?;
                        }
                        if flags & 0x1 != 0 {
                            return Ok(body);
                        }
                    }
                    0x1 if fsid == sid && flags & 0x1 != 0 => {
                        return Ok(body);
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

    for _ in 0..5 {
        client.get(path)?;
    } // warmup

    for _ in 0..n {
        if client.requests_done >= H2C_MAX_REQUESTS_PER_CONN {
            client = H2cClient::connect(addr)?;
        }
        let t0 = m6_core::platform::now();
        client.get(path)?;
        latencies.push(t0.elapsed().as_secs_f64() * 1_000_000.0);
    }
    Ok(latencies)
}

fn bench_h2c_throughput(addr: &str, duration_s: u64, concurrency: usize) -> anyhow::Result<f64> {
    let count = Arc::new(AtomicUsize::new(0));
    let deadline = m6_core::platform::now() + Duration::from_secs(duration_s);
    let addr = Arc::new(addr.to_string());

    let handles: Vec<_> = (0..concurrency)
        .map(|_| {
            let count = Arc::clone(&count);
            let addr = Arc::clone(&addr);
            std::thread::spawn(move || {
                let result = (|| -> anyhow::Result<()> {
                    let mut client = H2cClient::connect(&addr)?;
                    while m6_core::platform::now() < deadline {
                        if client.requests_done >= H2C_MAX_REQUESTS_PER_CONN {
                            client = H2cClient::connect(&addr)?;
                        }
                        match client.get("/") {
                            Ok(_) => {
                                count.fetch_add(1, Ordering::Relaxed);
                            }
                            Err(e) => {
                                eprintln!("h2c throughput error: {e}");
                            }
                        }
                    }
                    Ok(())
                })();
                if let Err(e) = result {
                    eprintln!("h2c thread error: {e}");
                }
            })
        })
        .collect();

    let t0 = m6_core::platform::now();
    for h in handles {
        h.join().ok();
    }
    let elapsed = t0.elapsed().as_secs_f64();
    let completed = count.load(Ordering::Relaxed);
    Ok(completed as f64 / elapsed)
}

// ── HTTP/3 helpers ────────────────────────────────────────────────────────────

fn make_quiche_client_config(skip_verify: bool) -> quiche::Config {
    probe::bench_quic_client_config(skip_verify).expect("building the QUIC client config")
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
            Ok((len, _info)) => {
                udp.send(&out[..len]).ok();
            }
            Err(quiche::Error::Done) => break,
            Err(e) => {
                eprintln!("quic_flush send: {e}");
                break;
            }
        }
    }
}

/// Close the connection and put the close on the wire before dropping it.
///
/// Dropping a connection without this leaves the peer holding it until its idle
/// timeout, and the bench client reconnects every [`H3_MAX_STREAMS_PER_CONN`]
/// requests, so one idle connection per reconnect piles up on the server. A
/// connection the peer already closed is not a failure to close.
fn close_and_flush(conn: &mut quiche::Connection, udp: &UdpSocket) {
    let _ = conn.close(true, 0x100, b"");
    quic_flush(conn, udp);
}

/// One receive slot in the bench client's batch. The server sends QUIC
/// packets of at most 1,350 bytes, so a slot holds one datagram with room to
/// spare.
const H3_RECV_SLOT: usize = 2048;

/// How long one turn waits for a datagram before the connection's timer is
/// driven. This was the socket's read timeout before the receive was batched.
const H3_TURN_WAIT: Duration = Duration::from_millis(100);

/// One turn's receive: wait up to `wait` for the socket, then take every
/// datagram already queued, up to [`MAX_BATCH`].
///
/// `Ok(None)` is a read that timed out with no datagram, which is the only
/// answer that drives the connection's timer. A turn that read anything has
/// already driven the connection through `conn.recv`.
fn receive_turn(
    udp: &UdpSocket,
    batch: &UdpBatch,
    buf: &mut [u8],
    metas: &mut [RecvMeta],
    wait: Duration,
) -> io::Result<Option<usize>> {
    if !m6_core::platform::wait_readable(udp.as_fd(), wait)? {
        return Ok(None);
    }
    match batch.recv(udp, buf, H3_RECV_SLOT, metas) {
        Ok(n) => Ok(Some(n)),
        // Readable and empty: a wake with nothing behind it. This is not a
        // timeout, so the caller does not drive the timer on it.
        Err(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(Some(0)),
        Err(e) => Err(e),
    }
}

/// Establish one QUIC+H3 connection to addr.
fn h3_connect(
    addr: &str,
    cfg: &mut quiche::Config,
) -> anyhow::Result<(
    quiche::Connection,
    quiche::h3::Connection,
    UdpSocket,
    quiche::RecvInfo,
)> {
    let udp = UdpSocket::bind("0.0.0.0:0")?;
    udp.connect(addr)?;
    // The socket is connected, so both addresses are fixed for its life. One
    // `getpeername` and one `getsockname` here replace two per datagram.
    let recv_info = quiche::RecvInfo {
        from: udp.peer_addr()?,
        to: udp.local_addr()?,
    };
    // The turn's wait, not a deadline per datagram: set once per socket.
    udp.set_read_timeout(Some(Duration::from_millis(100)))?;
    let scid = new_scid();
    let peer: std::net::SocketAddr = addr.parse()?;
    let local = udp.local_addr()?;
    let mut conn = quiche::connect(Some("localhost"), &scid, local, peer, cfg)?;
    // Initial flush (ClientHello)
    quic_flush(&mut conn, &udp);

    // Handshake loop
    let mut buf = [0u8; 65535];
    let deadline = m6_core::platform::now() + Duration::from_secs(5);
    loop {
        if m6_core::platform::now() > deadline {
            anyhow::bail!("QUIC handshake timed out");
        }
        conn.on_timeout();
        quic_flush(&mut conn, &udp);
        udp.set_read_timeout(Some(Duration::from_millis(100)))?;
        let n = match udp.recv(&mut buf) {
            Ok(n) => n,
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                continue
            }
            Err(e) => return Err(e.into()),
        };
        let recv_info = quiche::RecvInfo {
            from: peer,
            to: local,
        };
        conn.recv(&mut buf[..n], recv_info)?;
        quic_flush(&mut conn, &udp);
        if conn.is_established() {
            break;
        }
        if conn.is_closed() {
            anyhow::bail!("QUIC connection closed during handshake");
        }
    }

    let h3_cfg = quiche::h3::Config::new()?;
    let h3 = quiche::h3::Connection::with_transport(&mut conn, &h3_cfg)?;
    // The handshake waits on a read timeout, one datagram at a time. From here
    // the receive is a non-blocking batch taken after one readiness wait.
    udp.set_nonblocking(true)?;
    Ok((conn, h3, udp, recv_info))
}

// Maximum bidi streams per connection before reconnecting. Stays well under
// the server's initial_max_streams_bidi(100) to avoid StreamLimit errors.
const H3_MAX_STREAMS_PER_CONN: usize = 90;

/// One bench client's HTTP/3 connection and the requests it has carried.
///
/// A run keeps opening connections because no connection may carry more than
/// [`H3_MAX_STREAMS_PER_CONN`] requests, so every loop that sends them needs
/// the same reconnect. It lives here rather than in each of them.
struct H3Client {
    conn: quiche::Connection,
    h3: quiche::h3::Connection,
    udp: UdpSocket,
    recv_info: quiche::RecvInfo,
    /// The socket's batch receive.
    batch: UdpBatch,
    /// The turn's receive slots, [`MAX_BATCH`] datagrams of [`H3_RECV_SLOT`]
    /// bytes each. Allocated once per connection, not per request.
    recv_buf: Vec<u8>,
    /// One length and source per receive slot.
    recv_metas: [RecvMeta; MAX_BATCH],
    /// Requests this connection has carried, which is what fills it up.
    reqs: usize,
}

impl H3Client {
    fn connect(addr: &str, cfg: &mut quiche::Config) -> anyhow::Result<Self> {
        let (conn, h3, udp, recv_info) = h3_connect(addr, cfg)?;
        let batch = UdpBatch::for_socket(&udp);
        Ok(Self {
            conn,
            h3,
            udp,
            recv_info,
            batch,
            recv_buf: vec![0u8; MAX_BATCH * H3_RECV_SLOT],
            recv_metas: [RecvMeta::default(); MAX_BATCH],
            reqs: 0,
        })
    }

    /// Open a new connection once this one has carried its stream limit.
    ///
    /// The old one is closed before it is dropped: dropping it would leave the
    /// server holding it until its idle timeout.
    fn rotate_if_full(&mut self, addr: &str, cfg: &mut quiche::Config) -> anyhow::Result<()> {
        if self.reqs < H3_MAX_STREAMS_PER_CONN {
            return Ok(());
        }
        close_and_flush(&mut self.conn, &self.udp);
        *self = Self::connect(addr, cfg)?;
        Ok(())
    }

    /// One GET on this connection.
    ///
    /// A turn reads a whole receive batch, feeds every datagram to the
    /// connection, and flushes once. quiche coalesces the ACKs for the batch
    /// into that one send, so the client answers sixteen datagrams with one
    /// ACK datagram. The connection's timer is driven only by a read that
    /// actually timed out.
    fn get(&mut self, path: &[u8]) -> anyhow::Result<Vec<u8>> {
        let req = vec![
            quiche::h3::Header::new(b":method", b"GET"),
            quiche::h3::Header::new(b":path", path),
            quiche::h3::Header::new(b":scheme", b"https"),
            quiche::h3::Header::new(b":authority", b"localhost"),
        ];
        // Use the stream ID assigned by quiche (not a manually tracked counter).
        let stream_id = self.h3.send_request(&mut self.conn, &req, true)?;
        quic_flush(&mut self.conn, &self.udp);

        let mut body = Vec::new();
        let mut buf = [0u8; 65535];
        let deadline = m6_core::platform::now() + Duration::from_secs(5);

        loop {
            if m6_core::platform::now() > deadline {
                anyhow::bail!("HTTP/3 request timed out");
            }
            match receive_turn(
                &self.udp,
                &self.batch,
                &mut self.recv_buf,
                &mut self.recv_metas,
                H3_TURN_WAIT,
            )? {
                Some(n) => {
                    for i in 0..n {
                        let start = i * H3_RECV_SLOT;
                        let len = self.recv_metas[i].len;
                        self.conn
                            .recv(&mut self.recv_buf[start..start + len], self.recv_info)?;
                    }
                    // One flush covers the whole batch: every packet the batch
                    // answered is acknowledged in this send, not one per read.
                    quic_flush(&mut self.conn, &self.udp);
                }
                None => {
                    // The read timed out, so the timer is the only thing that
                    // can move the connection now.
                    self.conn.on_timeout();
                    quic_flush(&mut self.conn, &self.udp);
                    if self.conn.is_closed() {
                        break;
                    }
                    continue;
                }
            }

            // Poll H3 events. Use the actual sid from each event to drain the
            // correct stream; only accumulate body / return when it's our stream.
            loop {
                match self.h3.poll(&mut self.conn) {
                    Ok((sid, quiche::h3::Event::Data)) => {
                        while let Ok(read) = self.h3.recv_body(&mut self.conn, sid, &mut buf) {
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

            if self.conn.is_closed() {
                break;
            }
        }
        Ok(body)
    }
}

// ── HTTP/3 latency ────────────────────────────────────────────────────────────

fn bench_http3_latency(addr: &str, n: usize, skip_verify: bool) -> anyhow::Result<Vec<f64>> {
    let mut cfg = make_quiche_client_config(skip_verify);
    let mut latencies = Vec::with_capacity(n);

    let mut client = H3Client::connect(addr, &mut cfg)?;

    // Warmup: 5 requests
    for _ in 0..5 {
        client.rotate_if_full(addr, &mut cfg)?;
        let _ = client.get(b"/");
        client.reqs += 1;
    }

    for _ in 0..n {
        client.rotate_if_full(addr, &mut cfg)?;
        let t0 = m6_core::platform::now();
        client.get(b"/")?;
        latencies.push(t0.elapsed().as_secs_f64() * 1_000_000.0);
        client.reqs += 1;
    }
    Ok(latencies)
}

// ── HTTP/3 throughput — one connection per "thread", reconnect at stream limit ─

fn bench_http3_throughput(
    addr: &str,
    duration_s: u64,
    concurrency: usize,
    skip_verify: bool,
) -> anyhow::Result<f64> {
    let count = Arc::new(AtomicUsize::new(0));
    let deadline = m6_core::platform::now() + Duration::from_secs(duration_s);
    let addr = Arc::new(addr.to_string());
    let errors = Arc::new(Mutex::new(Vec::<String>::new()));

    let handles: Vec<_> = (0..concurrency)
        .map(|_| {
            let count = Arc::clone(&count);
            let addr = Arc::clone(&addr);
            let errors = Arc::clone(&errors);
            std::thread::spawn(move || {
                let mut cfg = make_quiche_client_config(skip_verify);
                let result = (|| -> anyhow::Result<()> {
                    let mut client = H3Client::connect(&addr, &mut cfg)?;
                    while m6_core::platform::now() < deadline {
                        client.rotate_if_full(&addr, &mut cfg)?;
                        client.reqs += 1;
                        match client.get(b"/") {
                            Ok(_) => {
                                count.fetch_add(1, Ordering::Relaxed);
                            }
                            Err(e) => {
                                eprintln!("h3 throughput error: {e}");
                            }
                        }
                    }
                    Ok(())
                })();
                if let Err(e) = result {
                    errors.lock().unwrap().push(e.to_string());
                }
            })
        })
        .collect();
    let t0 = m6_core::platform::now();
    for h in handles {
        h.join().ok();
    }
    let elapsed = t0.elapsed().as_secs_f64();
    let completed = count.load(Ordering::Relaxed);
    Ok(completed as f64 / elapsed)
}

fn bench_h3_throughput_path(
    addr: &str,
    path: &str,
    duration_s: u64,
    concurrency: usize,
    skip_verify: bool,
) -> anyhow::Result<f64> {
    let count = Arc::new(AtomicUsize::new(0));
    let deadline = m6_core::platform::now() + Duration::from_secs(duration_s);
    let addr = Arc::new(addr.to_string());
    let path = Arc::new(path.to_string());
    let errors = Arc::new(Mutex::new(Vec::<String>::new()));

    let handles: Vec<_> = (0..concurrency)
        .map(|_| {
            let count = Arc::clone(&count);
            let addr = Arc::clone(&addr);
            let path = Arc::clone(&path);
            let errors = Arc::clone(&errors);
            std::thread::spawn(move || {
                let mut cfg = make_quiche_client_config(skip_verify);
                let result = (|| -> anyhow::Result<()> {
                    let mut client = H3Client::connect(&addr, &mut cfg)?;
                    while m6_core::platform::now() < deadline {
                        client.rotate_if_full(&addr, &mut cfg)?;
                        client.reqs += 1;
                        match client.get(path.as_bytes()) {
                            Ok(_) => {
                                count.fetch_add(1, Ordering::Relaxed);
                            }
                            Err(e) => {
                                eprintln!("h3 url throughput error: {e}");
                            }
                        }
                    }
                    Ok(())
                })();
                if let Err(e) = result {
                    errors.lock().unwrap().push(e.to_string());
                }
            })
        })
        .collect();

    let t0 = m6_core::platform::now();
    for h in handles {
        h.join().ok();
    }
    let elapsed = t0.elapsed().as_secs_f64();
    let completed = count.load(Ordering::Relaxed);
    Ok(completed as f64 / elapsed)
}

fn bench_h2c_throughput_path(
    addr: &str,
    path: &str,
    duration_s: u64,
    concurrency: usize,
) -> anyhow::Result<f64> {
    let count = Arc::new(AtomicUsize::new(0));
    let deadline = m6_core::platform::now() + Duration::from_secs(duration_s);
    let addr = Arc::new(addr.to_string());
    let path = Arc::new(path.to_string());

    let handles: Vec<_> = (0..concurrency)
        .map(|_| {
            let count = Arc::clone(&count);
            let addr = Arc::clone(&addr);
            let path = Arc::clone(&path);
            std::thread::spawn(move || {
                let result = (|| -> anyhow::Result<()> {
                    let mut client = H2cClient::connect(&addr)?;
                    while m6_core::platform::now() < deadline {
                        if client.requests_done >= H2C_MAX_REQUESTS_PER_CONN {
                            client = H2cClient::connect(&addr)?;
                        }
                        match client.get(&path) {
                            Ok(_) => {
                                count.fetch_add(1, Ordering::Relaxed);
                            }
                            Err(e) => {
                                eprintln!("h2c url throughput error: {e}");
                            }
                        }
                    }
                    Ok(())
                })();
                if let Err(e) = result {
                    eprintln!("h2c url thread error: {e}");
                }
            })
        })
        .collect();

    let t0 = m6_core::platform::now();
    for h in handles {
        h.join().ok();
    }
    let elapsed = t0.elapsed().as_secs_f64();
    let completed = count.load(Ordering::Relaxed);
    Ok(completed as f64 / elapsed)
}

// ── Result reporter ───────────────────────────────────────────────────────────

struct BenchResult {
    name: String,
    count: usize,
    p0_us: f64,
    p1_us: f64,
    p25_us: f64,
    p50_us: f64,
    p75_us: f64,
    p99_us: f64,
    p100_us: f64,
    iqr_us: f64,
    range_us: f64,
    avg_us: f64,
    std_dev_us: f64,
    rps: f64,
}

impl BenchResult {
    fn from_latencies(name: impl Into<String>, lats: Vec<f64>) -> Self {
        let count = lats.len();
        let p0 = percentile(lats.clone(), 0);
        let p1 = percentile(lats.clone(), 1);
        let p25 = percentile(lats.clone(), 25);
        let p50 = percentile(lats.clone(), 50);
        let p75 = percentile(lats.clone(), 75);
        let p99 = percentile(lats.clone(), 99);
        let p100 = percentile(lats.clone(), 100);
        let iqr = p75 - p25;
        let range = p100 - p0;
        let avg = if count > 0 {
            lats.iter().sum::<f64>() / count as f64
        } else {
            0.0
        };
        let std_dev = if count > 1 {
            let var = lats.iter().map(|x| (x - avg).powi(2)).sum::<f64>() / (count - 1) as f64;
            var.sqrt()
        } else {
            0.0
        };
        BenchResult {
            name: name.into(),
            count,
            p0_us: p0,
            p1_us: p1,
            p25_us: p25,
            p50_us: p50,
            p75_us: p75,
            p99_us: p99,
            p100_us: p100,
            iqr_us: iqr,
            range_us: range,
            avg_us: avg,
            std_dev_us: std_dev,
            rps: 0.0,
        }
    }
    fn from_rps(name: impl Into<String>, rps: f64) -> Self {
        BenchResult {
            name: name.into(),
            count: 0,
            p0_us: 0.0,
            p1_us: 0.0,
            p25_us: 0.0,
            p50_us: 0.0,
            p75_us: 0.0,
            p99_us: 0.0,
            p100_us: 0.0,
            iqr_us: 0.0,
            range_us: 0.0,
            avg_us: 0.0,
            std_dev_us: 0.0,
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
        println!(
            "       p99 {:.1}µs exceeds limit {:.1}µs",
            r.p99_us, p99_limit
        );
    }
    if !rps_ok {
        println!(
            "       throughput {:.1} req/s below minimum {:.1}",
            r.rps, rps_min
        );
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

const PATH_HIT_HTML: &str = "/";
const PATH_HIT_FILE: &str = "/assets/hello.txt";
const PATH_MISS_HTML: &str = "/nocache/";
const PATH_MISS_FILE: &str = "/tail/hello.txt";

// ── HTTP/1.1 path helpers ─────────────────────────────────────────────────────

fn bench_h1_path_latency(
    addr: &str,
    path: &str,
    n: usize,
    tls_cfg: Arc<ClientConfig>,
) -> anyhow::Result<Vec<f64>> {
    bench_http11_latency_path(addr, path, n, tls_cfg)
}

// ── HTTP/2 path helpers ───────────────────────────────────────────────────────

fn bench_h2_path_latency(
    addr: &str,
    path: &str,
    n: usize,
    skip_verify: bool,
) -> anyhow::Result<Vec<f64>> {
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
    let mut client = H3Client::connect(addr, &mut cfg)?;
    let pb = path.as_bytes();

    for _ in 0..WARMUP {
        client.rotate_if_full(addr, &mut cfg)?;
        client.get(pb)?;
        client.reqs += 1;
    }
    for _ in 0..n {
        client.rotate_if_full(addr, &mut cfg)?;
        let t0 = m6_core::platform::now();
        client.get(pb)?;
        latencies.push(t0.elapsed().as_secs_f64() * 1_000_000.0);
        client.reqs += 1;
    }
    Ok(latencies)
}

// ── run_path_benchmarks: called for H1, H2, or H3 ────────────────────────────

fn run_path_benchmarks(
    label: &str,
    addr: &str,
    n: usize,
    skip_verify: bool,
    proto: u8, // 1=H1, 2=H2, 3=H3, 4=H2C
    tls_cfg: Option<Arc<ClientConfig>>,
    all_pass: &mut bool,
) {
    println!("{:-<70}", "");
    println!("Path benchmarks — {label} (n={n}):");

    let paths = [
        ("cache-hit→m6-html", PATH_HIT_HTML),
        ("cache-hit→m6-file", PATH_HIT_FILE),
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
            Ok(lats) => {
                print_result(&BenchResult::from_latencies(name, lats), f64::INFINITY, 0.0);
            }
            Err(e) => {
                eprintln!("{name} error: {e}");
                *all_pass = false;
            }
        }
    }
}

// ── main ──────────────────────────────────────────────────────────────────────

fn main() {
    // `--version` and `--version-full` answer from constants fixed at build
    // time, before any measurement arguments are required.
    m6_core::version_flags!(m6_core::build_info::linked());

    // The crate enables exactly one rustls provider feature (aws-lc-rs), so the
    // first config builder resolves and installs it. `supported_verify_schemes`
    // reads the provider directly and does not need an installed default.
    let args = Args::parse();
    if args.handshake {
        std::process::exit(run_handshake_suite(&args));
    }
    if args.mixed_handshake {
        std::process::exit(run_mixed_handshake_suite(&args));
    }
    let mut all_pass = true;

    let run_latency = !args.throughput_only && !args.path_only;
    let run_path = !args.latency_only && !args.throughput_only;
    let run_throughput = !args.latency_only && !args.path_only;

    println!("m6-bench  target={}  h2c-target={}  skip-verify={}  http11={}  http2={}  http3={}  h2c={}  url={}",
             args.addr, args.h2c_addr, args.skip_verify, args.http11, args.http2, args.http3, args.h2c, args.url);
    println!("{:-<70}", "");

    // ── Latency ───────────────────────────────────────────────────────────────
    if run_latency {
        let mut h1_keepalive_p50 = None;
        let mut h2_p50 = None;
        if args.http11 {
            let tls_cfg = make_client_config(args.skip_verify);
            match bench_http11_keepalive_latency_path(
                &args.addr,
                "/",
                args.latency_n,
                Arc::clone(&tls_cfg),
            ) {
                Ok(lats) => {
                    h1_keepalive_p50 = Some(percentile(lats.clone(), 50));
                    print_result(
                        &BenchResult::from_latencies("HTTP/1.1 latency (keep-alive)", lats),
                        f64::INFINITY,
                        0.0,
                    );
                }
                Err(e) => {
                    eprintln!("HTTP/1.1 keep-alive latency error: {e}");
                    all_pass = false;
                }
            }
            match bench_http11_latency(&args.addr, args.latency_n, Arc::clone(&tls_cfg)) {
                Ok(lats) => {
                    print_result(
                        &BenchResult::from_latencies(
                            "HTTP/1.1 latency (new connection per request, includes TLS handshake)",
                            lats,
                        ),
                        f64::INFINITY,
                        0.0,
                    );
                }
                Err(e) => {
                    eprintln!("HTTP/1.1 latency error: {e}");
                    all_pass = false;
                }
            }
        }
        if args.http2 {
            match bench_http2_latency(&args.addr, args.latency_n, args.skip_verify) {
                Ok(lats) => {
                    h2_p50 = Some(percentile(lats.clone(), 50));
                    if !print_result(
                        &BenchResult::from_latencies("HTTP/2 latency", lats),
                        args.p99_limit_us,
                        0.0,
                    ) {
                        all_pass = false;
                    }
                }
                Err(e) => {
                    eprintln!("HTTP/2 latency error: {e}");
                    all_pass = false;
                }
            }
        }
        if let (Some(h1), Some(h2)) = (h1_keepalive_p50, h2_p50) {
            let ratio = h1 / h2;
            let ok = ratio <= H1_VS_H2_P50_MAX_RATIO;
            println!(
                "{}   HTTP/1.1 keep-alive p50 / HTTP/2 p50 = {ratio:.2}  (limit {H1_VS_H2_P50_MAX_RATIO})",
                if ok { "PASS" } else { "FAIL" }
            );
            all_pass &= ok;
        }
        if args.http3 {
            match bench_http3_latency(&args.addr, args.latency_n, args.skip_verify) {
                Ok(lats) => {
                    if !print_result(
                        &BenchResult::from_latencies("HTTP/3 latency", lats),
                        args.p99_limit_us,
                        0.0,
                    ) {
                        all_pass = false;
                    }
                }
                Err(e) => {
                    eprintln!("HTTP/3 latency error: {e}");
                    all_pass = false;
                }
            }
        }
        if args.h2c {
            match bench_h2c_latency(&args.h2c_addr, args.latency_n) {
                Ok(lats) => {
                    if !print_result(
                        &BenchResult::from_latencies("H2C latency", lats),
                        args.p99_limit_us,
                        0.0,
                    ) {
                        all_pass = false;
                    }
                }
                Err(e) => {
                    eprintln!("H2C latency error: {e}");
                    all_pass = false;
                }
            }
        }
    }

    // ── Path benchmarks ───────────────────────────────────────────────────────
    // bench.sh restarts the server before each suite, so no stale-connection
    // contamination.  Each protocol invocation is an independent fresh run.
    if run_path {
        if args.http11 {
            let tls_cfg = make_client_config(args.skip_verify);
            run_path_benchmarks(
                "HTTP/1.1",
                &args.addr,
                args.latency_n,
                args.skip_verify,
                1,
                Some(Arc::clone(&tls_cfg)),
                &mut all_pass,
            );
        }
        if args.http2 {
            run_path_benchmarks(
                "HTTP/2",
                &args.addr,
                args.latency_n,
                args.skip_verify,
                2,
                None,
                &mut all_pass,
            );
        }
        if args.http3 {
            run_path_benchmarks(
                "HTTP/3",
                &args.addr,
                args.latency_n,
                args.skip_verify,
                3,
                None,
                &mut all_pass,
            );
        }
        if args.h2c {
            run_path_benchmarks(
                "H2C",
                &args.h2c_addr,
                args.latency_n,
                false,
                4,
                None,
                &mut all_pass,
            );
        }
    }

    // ── Throughput ────────────────────────────────────────────────────────────
    if run_throughput {
        println!("{:-<70}", "");
        if args.http11 {
            let tls_cfg = make_client_config(args.skip_verify);
            match bench_http11_throughput(
                &args.addr,
                args.duration_s,
                args.concurrency,
                Arc::clone(&tls_cfg),
            ) {
                Ok(rps) => {
                    if !print_result(
                        &BenchResult::from_rps("HTTP/1.1 throughput", rps),
                        0.0,
                        args.rps_min,
                    ) {
                        all_pass = false;
                    }
                }
                Err(e) => {
                    eprintln!("HTTP/1.1 throughput error: {e}");
                    all_pass = false;
                }
            }
        }
        if args.http2 {
            match bench_http2_throughput(
                &args.addr,
                args.duration_s,
                args.concurrency,
                args.skip_verify,
            ) {
                Ok(rps) => {
                    if !print_result(
                        &BenchResult::from_rps("HTTP/2 throughput", rps),
                        0.0,
                        args.rps_min,
                    ) {
                        all_pass = false;
                    }
                }
                Err(e) => {
                    eprintln!("HTTP/2 throughput error: {e}");
                    all_pass = false;
                }
            }
        }
        if args.http3 {
            match bench_http3_throughput(
                &args.addr,
                args.duration_s,
                args.concurrency,
                args.skip_verify,
            ) {
                Ok(rps) => {
                    if !print_result(
                        &BenchResult::from_rps("HTTP/3 throughput", rps),
                        0.0,
                        args.rps_min,
                    ) {
                        all_pass = false;
                    }
                }
                Err(e) => {
                    eprintln!("HTTP/3 throughput error: {e}");
                    all_pass = false;
                }
            }
        }
        if args.h2c {
            match bench_h2c_throughput(&args.h2c_addr, args.duration_s, args.concurrency) {
                Ok(rps) => {
                    if !print_result(
                        &BenchResult::from_rps("H2C throughput", rps),
                        0.0,
                        args.rps_min,
                    ) {
                        all_pass = false;
                    }
                }
                Err(e) => {
                    eprintln!("H2C throughput error: {e}");
                    all_pass = false;
                }
            }
        }
    }

    // ── URL-backend suites: all inbound × all URL outbound ───────────────────
    // Produces a 4×4 matrix: (h1|h2|h3|h2c) inbound × (http|https|h2c|h2s) outbound.
    // Routes /url/http/, /url/https/, /url/h2c/, /url/h2s/ must be in site.toml.
    if args.url {
        const URL_ROUTES: &[(&str, &str)] = &[
            ("http", "/url/http/"),
            ("https", "/url/https/"),
            ("h2c", "/url/h2c/"),
            ("h2s", "/url/h2s/"),
        ];
        let tls_addr = args.addr.as_str();
        let h2c_addr = args.h2c_addr.as_str();
        // (label, addr, proto_id)  proto_id: 1=h1, 2=h2, 3=h3, 4=h2c
        let inbounds: &[(&str, &str, u8)] = &[
            ("h1", tls_addr, 1),
            ("h2", tls_addr, 2),
            ("h3", tls_addr, 3),
            ("h2c", h2c_addr, 4),
        ];
        if run_latency {
            println!("{:-<70}", "");
            for &(inbound, addr, proto) in inbounds {
                for &(outbound, path) in URL_ROUTES {
                    let name = format!("{inbound}→{outbound} latency");
                    let result: anyhow::Result<Vec<f64>> = match proto {
                        1 => {
                            let cfg = make_client_config(args.skip_verify);
                            bench_http11_keepalive_latency_path(addr, path, args.latency_n, cfg)
                        }
                        2 => bench_http2_latency_path(addr, path, args.latency_n, args.skip_verify),
                        3 => bench_h3_path_latency(addr, path, args.latency_n, args.skip_verify),
                        4 => bench_h2c_latency_path(addr, path, args.latency_n),
                        _ => unreachable!(),
                    };
                    match result {
                        Ok(lats) => {
                            if !print_result(
                                &BenchResult::from_latencies(&name, lats),
                                args.p99_limit_us,
                                0.0,
                            ) {
                                all_pass = false;
                            }
                        }
                        Err(e) => {
                            eprintln!("{name} error: {e}");
                            all_pass = false;
                        }
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
                        1 => {
                            let cfg = make_client_config(args.skip_verify);
                            bench_http11_throughput_path(
                                addr,
                                path,
                                args.duration_s,
                                args.concurrency,
                                cfg,
                            )
                        }
                        2 => bench_http2_throughput_path(
                            addr,
                            path,
                            args.duration_s,
                            args.concurrency,
                            args.skip_verify,
                        ),
                        3 => bench_h3_throughput_path(
                            addr,
                            path,
                            args.duration_s,
                            args.concurrency,
                            args.skip_verify,
                        ),
                        4 => {
                            bench_h2c_throughput_path(addr, path, args.duration_s, args.concurrency)
                        }
                        _ => unreachable!(),
                    };
                    match result {
                        Ok(rps) => {
                            if !print_result(&BenchResult::from_rps(&name, rps), 0.0, args.rps_min)
                            {
                                all_pass = false;
                            }
                        }
                        Err(e) => {
                            eprintln!("{name} error: {e}");
                            all_pass = false;
                        }
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

// ── Handshake suite ───────────────────────────────────────────────────────────

/// Handshakes only, one protocol per run, strictly sequential: the figures a
/// visitor's first (`full`) and returning (`resumed`) connection pay, and for
/// h3 the QUIC 0-RTT request (`early`).
///
/// TCP+TLS (h1, h2) times from after the TCP connect to the client's Finished on
/// the wire, as `probe::tls_handshake` does. QUIC (h3) times from the first
/// Initial, so it INCLUDES the transport round trip; the two are not comparable.
///
/// Prints one `HS` line a table can be built from, then `HANDSHAKES_SERVED n`:
/// every handshake the server saw, warm-ups included, which `bench.sh` divides
/// the server's CPU delta by. Returns the exit status: non-zero if any
/// handshake failed or a `resumed` run never resumed.
fn run_handshake_suite(args: &Args) -> i32 {
    use m6_http_lib::probe;
    use std::time::Duration;

    let proto = if args.http3 && !args.http11 && !args.http2 {
        "h3"
    } else if args.http2 && !args.http11 {
        "h2"
    } else {
        "h1"
    };
    let kind = args.handshake_kind.as_str();
    let n = args.handshake_n;
    let mut samples: Vec<Duration> = Vec::with_capacity(n);
    let mut failures = 0usize;
    let mut served = 0usize;
    let mut resumed = 0usize;
    let mut early_used = 0usize;
    let mut too_early = 0usize;

    match (proto, kind) {
        ("h1" | "h2", "full" | "resumed") => {
            let alpn: &[u8] = if proto == "h2" { b"h2" } else { b"http/1.1" };
            let shared = probe::tls_config(alpn);
            if kind == "resumed" {
                // The first handshake earns the ticket and is not measured.
                served += 1;
                if probe::tls_handshake(&args.addr, Arc::clone(&shared)).is_err() {
                    failures += 1;
                }
            }
            for _ in 0..n {
                let cfg = if kind == "resumed" {
                    Arc::clone(&shared)
                } else {
                    probe::tls_config(alpn)
                };
                served += 1;
                match probe::tls_handshake(&args.addr, cfg) {
                    Ok(h) => {
                        resumed += usize::from(h.resumed);
                        samples.push(h.elapsed);
                    }
                    Err(e) => {
                        failures += 1;
                        eprintln!("handshake failed: {e}");
                    }
                }
            }
        }
        ("h3", "full") => {
            for _ in 0..n {
                served += 1;
                match probe::quic_handshake(&args.addr) {
                    Ok(d) => samples.push(d),
                    Err(e) => {
                        failures += 1;
                        eprintln!("handshake failed: {e}");
                    }
                }
            }
        }
        ("h3", "early") => {
            // Each round is one full handshake (to earn the ticket) and one
            // resumed connection whose request goes in the first flight.
            for _ in 0..n {
                served += 2;
                match probe::zero_rtt_h3(&args.addr, "/", "GET") {
                    Ok(r) => {
                        if let Some(d) = r.warm_to_response {
                            samples.push(d);
                            resumed += 1;
                        }
                        early_used += usize::from(r.early_data_used);
                        too_early += usize::from(r.status == Some(425));
                    }
                    Err(e) => {
                        failures += 1;
                        eprintln!("0-RTT round failed: {e}");
                    }
                }
            }
        }
        _ => {
            eprintln!("no handshake suite for proto {proto} kind {kind} (h1/h2: full|resumed, h3: full|early)");
            return 2;
        }
    }

    let ok = samples.len();
    let summary = probe::summarise(&mut samples);
    let ms = |d: Duration| d.as_secs_f64() * 1000.0;
    match summary {
        Some(s) => println!(
            "HS {proto} {kind}: n={ok} failed={failures} resumed={resumed} early_data={early_used} \
             status425={too_early} p50_ms={:.3} p90_ms={:.3} p99_ms={:.3} mean_ms={:.3}",
            ms(s.p50),
            ms(s.p90),
            ms(s.p99),
            ms(s.mean)
        ),
        None => println!("HS {proto} {kind}: no successful handshakes ({failures} failed)"),
    }
    println!("HANDSHAKES_SERVED {served}");
    i32::from(failures > 0 || ok == 0 || (kind == "resumed" && resumed == 0))
}

// ── Mixed hot hits and full-handshake load ───────────────────────────────────
//
// The existing hot-hit number is measured on a warm cache with nothing else on
// the server. A real edge serves those hits while new visitors arrive and do a
// full TLS handshake. This mode runs the SAME hit client (`H1Client`, one
// kept-alive connection, the same timer and `percentile` rule) while a second
// thread opens new TCP connections and does a fresh full handshake at a set
// arrival rate.
//
// What is held constant, so the only difference between rows is the load:
//
//   * the hit connection is opened and warmed BEFORE the clock starts, and is
//     not reconnected during the window. A reconnect would inject a handshake
//     the arrival rate does not account for, so the server's
//     `max_requests_per_connection` is raised by bench.sh for this suite and
//     any reconnect is counted and fails the run.
//   * every load handshake uses a NEW `probe::tls_config` (and therefore a cold
//     session store) on a NEW TCP connection. No ticket is offered, so these
//     are full handshakes even though the server can resume (it installs a
//     ticketer). The run prints the client's own `resumed` count, which must be
//     zero, and a shared-config probe that shows the server would resume if a
//     client offered a ticket.
//
// The arrival rate is scheduled against absolute deadlines, not "sleep between
// calls": arrival i is due at start + i/rate. The achieved rate is the number of
// completed full handshakes divided by the window, printed BESIDE the target,
// and `reached=no` when it is more than 2% under. Handshake latency is measured
// around the whole `probe::tls_handshake` call, so it includes TCP connect and
// any accept-queue wait; a run that meets its rate only by queueing shows that
// in the handshake p50/p99. (The server-side handshake figure excludes connect.)

/// One rate's result. Latencies are microseconds for hits and milliseconds for
/// handshakes, matching the rest of this file.
struct MixedRate {
    target: u32,
    achieved: f64,
    reached: bool,
    hs: Vec<Duration>,
    hs_resumed: usize,
    hs_failed: usize,
    hs_late: usize,
    hs_scheduled: usize,
    hits: Vec<f64>,
    hits_with_age: usize,
    non200: usize,
    reconnects: usize,
    window: Duration,
}

/// The load thread's half of one rate.
struct MixedLoad {
    samples: Vec<Duration>,
    resumed: usize,
    failed: usize,
    late: usize,
    scheduled: usize,
}

fn parse_rate_list(raw: &str) -> Vec<u32> {
    raw.split(',')
        .filter_map(|v| v.trim().parse::<u32>().ok())
        .collect()
}

fn sleep_until(deadline: Instant) {
    // The platform layer owns the clock; the gate refuses a direct read here.
    let now = m6_core::platform::now();
    if now < deadline {
        std::thread::sleep(deadline - now);
    }
}

/// Is `name` (including its colon) a header in the response head?
///
/// A cached hit is the only path that writes `Age`, so this is the hit-ratio
/// signal. It runs AFTER the hit's latency is recorded and is not on the timed
/// path.
fn response_has_header(resp: &[u8], name: &[u8]) -> bool {
    let head_end = resp
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .unwrap_or(resp.len());
    let head = &resp[..head_end];
    let mut at = 0;
    while let Some(pos) = head[at..]
        .windows(name.len())
        .position(|w| w.eq_ignore_ascii_case(name))
    {
        let abs = at + pos;
        if abs == 0 || head[abs - 1] == b'\n' {
            return true;
        }
        at = abs + 1;
        if at >= head.len() {
            break;
        }
    }
    false
}

/// Arrival-rate scheduled load: one new TCP connection and one fresh full TLS
/// handshake per iteration, at most one in flight.
fn handshake_load(addr: String, target: u32, start: Instant, window: Duration) -> MixedLoad {
    let scheduled = (u64::from(target) * window.as_secs()) as usize;
    let mut out = MixedLoad {
        samples: Vec::with_capacity(scheduled),
        resumed: 0,
        failed: 0,
        late: 0,
        scheduled,
    };
    if target == 0 {
        return out;
    }
    let interval = Duration::from_secs_f64(1.0 / f64::from(target));
    let deadline = start + window;
    for i in 0..scheduled {
        sleep_until(start + interval * i as u32);
        let issued = m6_core::platform::now();
        if issued >= deadline {
            break;
        }
        if issued.saturating_duration_since(start + interval * i as u32) > Duration::from_millis(1)
        {
            out.late += 1;
        }
        // A fresh config per handshake: no session store, so no ticket is
        // offered and the handshake is full. A new TcpStream is opened inside
        // `tls_handshake`.
        let cfg = probe::tls_config(b"http/1.1");
        let t0 = m6_core::platform::now();
        match probe::tls_handshake(&addr, cfg) {
            Ok(h) => {
                out.resumed += usize::from(h.resumed);
                // Around the whole call, so this includes TCP connect and any
                // time the connection waited in the accept queue.
                out.samples.push(t0.elapsed());
            }
            Err(e) => {
                out.failed += 1;
                if out.failed <= 3 {
                    eprintln!("mixed: handshake load failed: {e}");
                }
            }
        }
    }
    out
}

fn run_mixed_rate(
    addr: &str,
    path: &str,
    target: u32,
    seconds: u64,
    tls_cfg: Arc<ClientConfig>,
) -> anyhow::Result<MixedRate> {
    let window = Duration::from_secs(seconds);
    // The hit connection is opened and warmed before the window. Its handshake
    // and the first requests fill the cache; neither is in the measurement.
    let mut client = H1Client::connect(addr, Arc::clone(&tls_cfg))?;
    for _ in 0..5 {
        client.get(path)?;
    }

    // A short lead so the load thread is running before the first arrival.
    let start = m6_core::platform::now() + Duration::from_millis(100);
    let load = if target == 0 {
        None
    } else {
        let addr = addr.to_string();
        Some(std::thread::spawn(move || {
            handshake_load(addr, target, start, window)
        }))
    };

    sleep_until(start);
    let deadline = start + window;
    let mut hits: Vec<f64> = Vec::new();
    let mut hits_with_age = 0usize;
    let mut non200 = 0usize;
    let mut reconnects = 0usize;

    while m6_core::platform::now() < deadline {
        let t0 = m6_core::platform::now();
        let resp = match client.get(path) {
            Ok(resp) => resp,
            Err(e) => {
                // A server that closes the kept-alive connection at its default
                // 100 requests would inject extra handshakes. Count it; refuse
                // the run rather than measure a rate that is not the target.
                reconnects += 1;
                eprintln!("mixed: hit connection closed ({e}); reconnecting");
                client = H1Client::connect(addr, Arc::clone(&tls_cfg))?;
                continue;
            }
        };
        let elapsed = t0.elapsed().as_secs_f64() * 1_000_000.0;
        hits.push(elapsed);
        let status = parse_http11_status(&resp);
        if status == 200 {
            if response_has_header(&resp, b"age:") {
                hits_with_age += 1;
            }
        } else {
            non200 += 1;
        }
    }

    let load = match load {
        Some(h) => h
            .join()
            .map_err(|_| anyhow::anyhow!("handshake load thread panicked"))?,
        None => MixedLoad {
            samples: Vec::new(),
            resumed: 0,
            failed: 0,
            late: 0,
            scheduled: 0,
        },
    };

    let achieved = load.samples.len() as f64 / window.as_secs_f64();
    // The 2% tolerance applies to the count as well as the rate: the last
    // scheduled arrival can still be in flight when the window closes.
    let reached = target == 0
        || (achieved >= f64::from(target) * 0.98
            && load.failed == 0
            && load.samples.len() >= (load.scheduled as f64 * 0.98) as usize);

    Ok(MixedRate {
        target,
        achieved,
        reached,
        hs: load.samples,
        hs_resumed: load.resumed,
        hs_failed: load.failed,
        hs_late: load.late,
        hs_scheduled: load.scheduled,
        hits,
        hits_with_age,
        non200,
        reconnects,
        window,
    })
}

fn print_mixed_rate(r: &MixedRate) {
    let mut hs = r.hs.clone();
    let summary = probe::summarise(&mut hs);
    let (hs_p50, hs_p99) = match summary {
        Some(s) => (s.p50.as_secs_f64() * 1000.0, s.p99.as_secs_f64() * 1000.0),
        None => (0.0, 0.0),
    };
    let hits_n = r.hits.len();
    let hit_ratio = if hits_n == 0 {
        0.0
    } else {
        r.hits_with_age as f64 / hits_n as f64
    };
    let hit_p50 = percentile(r.hits.clone(), 50);
    let hit_p99 = percentile(r.hits.clone(), 99);
    let hit_max = percentile(r.hits.clone(), 100);
    println!(
        "MIX target={} achieved={:.1} reached={} hs_n={} hs_scheduled={} hs_resumed={} \
         hs_failed={} hs_late={} hs_p50_ms={:.3} hs_p99_ms={:.3} \
         hits_n={} hit_rate={:.1} hit_p50_us={:.1} hit_p99_us={:.1} hit_max_us={:.1} \
         hit_ratio={:.3} non200={} hit_reconnects={}",
        r.target,
        r.achieved,
        if r.reached { "yes" } else { "no" },
        r.hs.len(),
        r.hs_scheduled,
        r.hs_resumed,
        r.hs_failed,
        r.hs_late,
        hs_p50,
        hs_p99,
        hits_n,
        hits_n as f64 / r.window.as_secs_f64(),
        hit_p50,
        hit_p99,
        hit_max,
        hit_ratio,
        r.non200,
        r.reconnects
    );
}

/// Hot-hit p50/p99 at each handshake arrival rate. Returns non-zero if a rate
/// was not reached, a load handshake resumed or failed, the hit connection had
/// to reconnect, or the responses were not cache hits.
fn run_mixed_handshake_suite(args: &Args) -> i32 {
    if args.mixed_rates.is_empty() {
        eprintln!("mixed: --mixed-rates is empty");
        return 2;
    }
    if args.mixed_seconds == 0 {
        eprintln!("mixed: --mixed-seconds must be at least 1");
        return 2;
    }

    println!(
        "MIXED-HANDSHAKE addr={} path={} seconds={} rates={:?}",
        args.addr, args.mixed_path, args.mixed_seconds, args.mixed_rates
    );

    // Resumption is configured on the server (http11.rs installs a ticketer), so
    // prove it empirically with one client that DOES offer its ticket. The load
    // below never does.
    let shared = probe::tls_config(b"http/1.1");
    let (mut proof_full, mut proof_resumed) = (0usize, 0usize);
    for _ in 0..2 {
        match probe::tls_handshake(&args.addr, Arc::clone(&shared)) {
            Ok(h) => {
                if h.resumed {
                    proof_resumed += 1;
                } else {
                    proof_full += 1;
                }
            }
            Err(e) => eprintln!("mixed: resumption probe failed: {e}"),
        }
    }
    println!(
        "RESUMPTION server=on ticketer=crypto::ticketer proof_full={proof_full} \
         proof_resumed={proof_resumed} load=fresh-ClientConfig-per-handshake"
    );

    let tls_cfg = make_client_config(args.skip_verify);
    let mut all_ok = true;
    for &target in &args.mixed_rates {
        match run_mixed_rate(
            &args.addr,
            &args.mixed_path,
            target,
            args.mixed_seconds,
            Arc::clone(&tls_cfg),
        ) {
            Ok(r) => {
                let ok = r.reached
                    && r.hs_resumed == 0
                    && r.hs_failed == 0
                    && r.reconnects == 0
                    && !r.hits.is_empty()
                    && r.non200 == 0
                    && r.hits_with_age == r.hits.len();
                print_mixed_rate(&r);
                if !ok {
                    all_ok = false;
                }
            }
            Err(e) => {
                eprintln!("mixed: rate {target}: {e}");
                all_ok = false;
            }
        }
        // Let the server finish closing the last load connections before the
        // next rate, so a rate does not inherit the previous one's backlog.
        std::thread::sleep(Duration::from_millis(100));
    }
    i32::from(!all_ok)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bench percentiles must match the server's own metrics document
    /// figures.
    /// stats.rs (`percentiles_n`) takes index `(n-1)*pct/100` with integer
    /// arithmetic, truncating. Rounding the same fraction, as this bench did,
    /// shifts p50 of ten samples from index 4 to 5, which reads as a false
    /// server regression when the two are compared side by side.
    #[test]
    fn percentile_truncates_like_stats_rs() {
        // Ten sorted samples: 10, 20, ... 100 microseconds.
        let v: Vec<f64> = (1..=10).map(|x| x as f64 * 10.0).collect();
        assert_eq!(percentile(v.clone(), 0), 10.0);
        assert_eq!(percentile(v.clone(), 50), 50.0);
        assert_eq!(percentile(v.clone(), 99), 90.0);
        assert_eq!(percentile(v.clone(), 100), 100.0);
    }

    /// A close reaches the socket through the flush that follows it, which is
    /// what stops a reconnect from leaving the server an idle connection until
    /// its timeout.
    ///
    /// A connection with no authenticated packet from its peer sends no close
    /// at all: quiche closes it locally, because there is nobody it can
    /// authenticate the close to. So the closing end here is a real quiche
    /// server that has read a client's Initial, and the check is that the
    /// client, fed whatever the flush put on the wire, sees the connection
    /// closed.
    ///
    /// Break: a close that quiche queues and nothing sends.
    #[test]
    fn a_close_reaches_the_socket() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cert =
            rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).expect("certificate");
        std::fs::write(dir.path().join("cert.pem"), cert.cert.pem()).expect("cert file");
        std::fs::write(dir.path().join("key.pem"), cert.key_pair.serialize_pem())
            .expect("key file");

        let client_sock = UdpSocket::bind("127.0.0.1:0").expect("bind");
        client_sock.set_nonblocking(true).expect("nonblocking");
        let client_addr = client_sock.local_addr().expect("client address");
        let server_sock = UdpSocket::bind("127.0.0.1:0").expect("bind");
        server_sock
            .set_read_timeout(Some(Duration::from_millis(500)))
            .expect("read timeout");
        let server_addr = server_sock.local_addr().expect("server address");
        client_sock.connect(server_addr).expect("connect");

        let mut client_cfg = make_quiche_client_config(true);
        let mut client = quiche::connect(
            Some("localhost"),
            &new_scid(),
            client_addr,
            server_addr,
            &mut client_cfg,
        )
        .expect("connect");
        let mut buf = [0u8; 65535];
        let (n, _) = client.send(&mut buf).expect("the client's Initial");
        client_sock.send(&buf[..n]).expect("send the Initial");

        let mut server_cfg = quiche::Config::new(quiche::PROTOCOL_VERSION).expect("config");
        server_cfg
            .load_cert_chain_from_pem_file(dir.path().join("cert.pem").to_str().expect("path"))
            .expect("cert chain");
        server_cfg
            .load_priv_key_from_pem_file(dir.path().join("key.pem").to_str().expect("path"))
            .expect("private key");
        server_cfg
            .set_application_protos(quiche::h3::APPLICATION_PROTOCOL)
            .expect("alpn");
        let (n, from) = server_sock
            .recv_from(&mut buf)
            .expect("the Initial arrived");
        let mut server =
            quiche::accept(&new_scid(), None, server_addr, from, &mut server_cfg).expect("accept");
        server
            .recv(
                &mut buf[..n],
                quiche::RecvInfo {
                    from,
                    to: server_addr,
                },
            )
            .expect("server read the Initial");
        server_sock
            .connect(from)
            .expect("connect the server socket");

        close_and_flush(&mut server, &server_sock);

        // Feed the client whatever arrives, including any of the handshake
        // flight still queued when the close was made, and wait for the close
        // itself to reach it.
        let mut buf = [0u8; 2048];
        let deadline = m6_core::platform::now() + Duration::from_millis(500);
        while m6_core::platform::now() < deadline && !(client.is_closed() || client.is_draining()) {
            match client_sock.recv(&mut buf) {
                Ok(n) => {
                    client
                        .recv(
                            &mut buf[..n],
                            quiche::RecvInfo {
                                from: server_addr,
                                to: client_addr,
                            },
                        )
                        .ok();
                }
                Err(_) => std::thread::sleep(Duration::from_millis(1)),
            }
        }
        assert!(
            client.is_closed() || client.is_draining(),
            "the peer saw the connection closed"
        );
    }

    /// One turn takes every datagram already queued, and a read with nothing
    /// behind it is reported as the timeout that happened.
    ///
    /// The client's whole receive loop is this function. Its `Some` arm feeds
    /// the batch to the connection and flushes once, and its `None` arm is the
    /// only place `on_timeout` is called.
    ///
    /// Break: a receive that takes one datagram per call (the count would be
    /// 1), a read with nothing queued reported as a batch of zero (the timer
    /// would never be driven), or a datagram dropped from the batch.
    #[test]
    fn a_receive_turn_takes_the_whole_batch_and_reports_a_timed_out_read() {
        let a = UdpSocket::bind("127.0.0.1:0").expect("bind");
        a.set_nonblocking(true).expect("nonblocking");
        let b = UdpSocket::bind("127.0.0.1:0").expect("bind");
        let to = a.local_addr().expect("a address");
        let batch = UdpBatch::without_gso();
        let mut buf = vec![0u8; MAX_BATCH * H3_RECV_SLOT];
        let mut metas = [RecvMeta::default(); MAX_BATCH];
        for i in 0..3u8 {
            b.send_to(&[i; 100], to).expect("send");
        }

        let n = receive_turn(&a, &batch, &mut buf, &mut metas, H3_TURN_WAIT)
            .expect("receive")
            .expect("a datagram was queued");
        assert_eq!(n, 3, "one turn took all three datagrams");
        for (i, meta) in metas.iter().take(3).enumerate() {
            let start = i * H3_RECV_SLOT;
            assert_eq!(meta.len, 100);
            assert_eq!(&buf[start..start + 100], &[i as u8; 100]);
            assert_eq!(meta.from, Some(b.local_addr().expect("b address")));
        }

        // Nothing is queued: the wait ends with no datagram, which is the one
        // answer that drives the connection's timer.
        let started = m6_core::platform::now();
        let none = receive_turn(&a, &batch, &mut buf, &mut metas, Duration::from_millis(20))
            .expect("receive");
        assert_eq!(
            none, None,
            "an empty read is a timed-out read, not a batch of zero"
        );
        assert!(
            started.elapsed() >= Duration::from_millis(15),
            "the wait did not wait for the timeout it reported"
        );
    }
}
