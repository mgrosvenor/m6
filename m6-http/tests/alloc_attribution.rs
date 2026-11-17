//! ALLOC-ATTR: attribute every heap allocation on m6-http's request paths to
//! the source line that made it.
//!
//! The binary this test starts is built with the `alloc-attribution` feature,
//! which installs dhat as the global allocator and writes one `dhat-heap.json`
//! per server run. Each row runs the server twice, at K = 100 and K = 1,100
//! requests, so the difference is 1,000 requests and every start-up allocation
//! cancels. The JSON files are kept, one per row and K, because the report is
//! built from the frames in them.
//!
//! The fifth row, `h1_tls_handshake_only`, opens a full TLS connection and
//! closes it without sending a request. Subtracting it from
//! `h1_tls_new_connection` is what separates the handshake (per connection)
//! from the per-request work, which is the only way dhat's process-wide totals
//! can be read as a request cost.
//!
//! ```text
//! ALLOC_ATTR_OUT=/tmp/m6-alloc-attribution \
//!   cargo test -p m6-http --test alloc_attribution \
//!   --features alloc-attribution -- --test-threads=1 --nocapture
//! ```

#![allow(
    clippy::disallowed_methods,
    reason = "test code: blocking clients with deadlines drive the service from outside"
)]

use std::io::{Read, Write};
use std::net::TcpStream;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rustls::StreamOwned;

use m6_core::testkit::{claim_port, PortClaim, Service};
use m6_http_lib::forward::HttpRequest;
use m6_http_lib::h2s_client::H2sTlsClientConn;

const REQUESTS_SMALL: usize = 100;
const REQUESTS_LARGE: usize = 1_100;

/// A cacheable response. `public` is what the cache stores, and the body's
/// length is the one number the test client must know to read a keep-alive
/// reply.
const BACKEND_RESPONSE: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\n\
Cache-Control: public, max-age=60\r\nContent-Length: 9\r\nConnection: close\r\n\r\ncacheable";

// ── The fixture ──────────────────────────────────────────────────────────────

struct Fixture {
    dir: tempfile::TempDir,
    site: PathBuf,
    system: PathBuf,
    port: PortClaim,
    cert_der: Vec<u8>,
}

impl Fixture {
    fn new() -> Fixture {
        let dir = tempfile::tempdir().expect("tempdir");
        let d = dir.path();
        let cert = rcgen::generate_simple_self_signed(vec![
            "localhost".to_string(),
            "127.0.0.1".to_string(),
        ])
        .expect("rcgen");
        std::fs::write(d.join("cert.pem"), cert.cert.pem()).expect("write cert");
        std::fs::write(d.join("key.pem"), cert.key_pair.serialize_pem()).expect("write key");

        let port = claim_port();
        let site_dir = d.join("site");
        std::fs::create_dir_all(&site_dir).expect("site directory");
        std::fs::write(
            site_dir.join("site.toml"),
            format!(
                "[site]\nname = \"attribution\"\ndomain = \"localhost\"\n\n\
                 [errors]\nmode = \"internal\"\n\n\
                 [analytics]\nenabled = false\n\n\
                 [log]\nlevel = \"info\"\n\n\
                 [rate_limit]\nenabled = false\n\n\
                 [server]\nbackend_timeout_secs = 5\n\n\
                 [[backend]]\nname = \"b\"\nsockets = \"{}\"\n\n\
                 [[route]]\npath = \"/\"\nbackend = \"b\"\n",
                d.join("b-*.sock").display()
            ),
        )
        .expect("write site.toml");
        std::fs::write(
            d.join("system.toml"),
            format!(
                "[server]\nbind = \"{}\"\ntls_cert = \"{}\"\ntls_key = \"{}\"\n\
                 max_requests_per_connection = 1000000\n\n\
                 [node]\nname = \"attribution\"\n",
                port.addr(),
                d.join("cert.pem").display(),
                d.join("key.pem").display()
            ),
        )
        .expect("write system.toml");

        let system = d.join("system.toml");
        Fixture {
            dir,
            site: site_dir,
            system,
            port,
            cert_der: cert.cert.der().to_vec(),
        }
    }

    fn start_backend(&self) {
        let sock = self.dir.path().join("b-1.sock");
        let listener = UnixListener::bind(&sock).expect("bind backend socket");
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                std::thread::spawn(move || {
                    let mut head = Vec::new();
                    let mut buf = [0u8; 4096];
                    while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                        match stream.read(&mut buf) {
                            Ok(0) | Err(_) => return,
                            Ok(n) => head.extend_from_slice(&buf[..n]),
                        }
                        if head.len() > 16 * 1024 {
                            return;
                        }
                    }
                    let _ = stream.write_all(BACKEND_RESPONSE);
                });
            }
        });
    }

    fn start(&self, dhat_out: &Path) -> Service {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_m6-http"));
        cmd.arg(&self.site)
            .arg(&self.system)
            .env("RUST_LOG", "info")
            .env("M6_DHAT_OUT", dhat_out)
            // The periodic stats emit is not per request, and a slow run that
            // crossed the ten-second tick would add its allocations to one K
            // and not the other. This pushes the first emit past any run.
            .env("M6_TEST_STATS_INTERVAL_SECS", "3600")
            .current_dir(self.dir.path());
        let mut svc = Service::spawn("m6-http-alloc-attribution", &mut cmd);
        svc.wait_for_tcp(self.port.port(), m6_core::testkit::STARTUP_BOUND);
        svc
    }

    fn port(&self) -> u16 {
        self.port.port()
    }
}

// ── The rows ─────────────────────────────────────────────────────────────────

#[derive(Clone, Copy)]
enum Row {
    H1TlsHit,
    H2TlsHit,
    H1TlsMissInsert,
    H1TlsNewConnection,
    H1TlsHandshakeOnly,
}

impl Row {
    fn name(self) -> &'static str {
        match self {
            Row::H1TlsHit => "h1_tls_hit",
            Row::H2TlsHit => "h2_tls_hit",
            Row::H1TlsMissInsert => "h1_tls_miss_insert",
            Row::H1TlsNewConnection => "h1_tls_new_connection",
            Row::H1TlsHandshakeOnly => "h1_tls_handshake_only",
        }
    }

    fn path(self, i: usize) -> String {
        match self {
            Row::H1TlsMissInsert => format!("/?n={i}"),
            _ => "/".to_string(),
        }
    }
}

// ── The measurement ──────────────────────────────────────────────────────────

#[test]
fn allocation_attribution() {
    let out = std::env::var_os("ALLOC_ATTR_OUT")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("m6-alloc-attribution"));
    std::fs::create_dir_all(&out).expect("create dhat output directory");
    println!("ALLOC-ATTR: dhat files are in {}", out.display());

    let rows = [
        Row::H1TlsHit,
        Row::H2TlsHit,
        Row::H1TlsMissInsert,
        Row::H1TlsNewConnection,
        Row::H1TlsHandshakeOnly,
    ];
    for row in rows {
        let (small_calls, small_bytes) = measure(row, REQUESTS_SMALL, &out);
        let (large_calls, large_bytes) = measure(row, REQUESTS_LARGE, &out);
        assert!(
            large_calls >= small_calls && large_bytes >= small_bytes,
            "{}: the 1,100-request run counted fewer allocations than the 100-request one \
             ({small_calls} calls, {small_bytes} bytes against {large_calls} calls, \
             {large_bytes} bytes). The count is not comparing the same path.",
            row.name()
        );
        let calls = large_calls - small_calls;
        let bytes = large_bytes - small_bytes;
        println!(
            "ALLOC-ATTR {}: {}.{:03} calls and {}.{:03} bytes per request \
             (K=100: {small_calls} calls, {small_bytes} bytes, \
             K=1,100: {large_calls} calls, {large_bytes} bytes)",
            row.name(),
            calls / 1_000,
            calls % 1_000,
            bytes / 1_000,
            bytes % 1_000,
        );
    }
}

fn measure(row: Row, requests: usize, out: &Path) -> (u64, u64) {
    let fixture = Fixture::new();
    fixture.start_backend();
    let dhat_out = out.join(format!("{}-{requests}.json", row.name()));
    let mut svc = fixture.start(&dhat_out);
    let port = fixture.port();

    match row {
        Row::H1TlsHit | Row::H1TlsMissInsert => {
            let mut conn = h1_ready(&fixture, "/");
            for i in 0..requests {
                let status = conn
                    .get(&row.path(i), port, false)
                    .unwrap_or_else(|e| panic!("{}: request {i}: {e}", row.name()));
                assert_eq!(status, 200, "{}: request {i} answered {status}", row.name());
            }
        }
        Row::H1TlsNewConnection => {
            // The warm-up connection takes the start-up cost out of both runs.
            drop(h1_ready(&fixture, "/"));
            for i in 0..requests {
                let mut conn = H1Conn::connect(port, tls_config(&fixture.cert_der, &[b"http/1.1"]));
                let status = conn
                    .get(&row.path(i), port, true)
                    .unwrap_or_else(|e| panic!("{}: request {i}: {e}", row.name()));
                assert_eq!(status, 200, "{}: request {i} answered {status}", row.name());
            }
        }
        Row::H1TlsHandshakeOnly => {
            // The warm-up handshake takes the start-up cost out of both runs.
            handshake_once(port, tls_config(&fixture.cert_der, &[b"http/1.1"]))
                .expect("warm-up handshake");
            for i in 0..requests {
                handshake_once(port, tls_config(&fixture.cert_der, &[b"http/1.1"]))
                    .unwrap_or_else(|e| panic!("{}: handshake {i}: {e}", row.name()));
            }
        }
        Row::H2TlsHit => {
            let mut conn = h2_ready(&fixture);
            for i in 0..requests {
                let status = h2_get(&mut conn, &row.path(i), port)
                    .unwrap_or_else(|e| panic!("{}: request {i}: {e}", row.name()));
                assert_eq!(status, 200, "{}: request {i} answered {status}", row.name());
            }
        }
    }

    let status = svc.terminate(m6_core::testkit::STARTUP_BOUND);
    assert!(
        status.success(),
        "{}: m6-http exited {status} on SIGTERM\n{}",
        row.name(),
        svc.output()
    );
    assert!(
        dhat_out.is_file(),
        "{}: dhat wrote no file at {}\n{}",
        row.name(),
        dhat_out.display(),
        svc.output()
    );
    dhat_totals(&dhat_out)
}

/// The `calls` and `bytes` totals from one dhat heap profile: the sum over every
/// allocation point of its block count and byte count.
fn dhat_totals(path: &Path) -> (u64, u64) {
    let text = std::fs::read_to_string(path).expect("read dhat profile");
    let json: serde_json::Value = serde_json::from_str(&text).expect("parse dhat profile");
    let pps = json["pps"].as_array().expect("dhat pps array");
    let mut calls = 0u64;
    let mut bytes = 0u64;
    for pp in pps {
        calls += pp["tbk"].as_u64().expect("pp tbk");
        bytes += pp["tb"].as_u64().expect("pp tb");
    }
    (calls, bytes)
}

// ── TLS clients ──────────────────────────────────────────────────────────────

fn tls_config(cert_der: &[u8], alpn: &[&[u8]]) -> Arc<rustls::ClientConfig> {
    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .ok();
    let cert = rustls::pki_types::CertificateDer::from(cert_der.to_vec());
    let mut store = rustls::RootCertStore::empty();
    store.add(cert).expect("trust the test certificate");
    let mut cfg = rustls::ClientConfig::builder()
        .with_root_certificates(store)
        .with_no_client_auth();
    cfg.alpn_protocols = alpn.iter().map(|p| p.to_vec()).collect();
    // Every connection in the connection rows is a full handshake, as the row
    // is defined. A resumed handshake allocates a different number of times,
    // so the count would depend on which connections happened to resume.
    cfg.resumption = rustls::client::Resumption::disabled();
    Arc::new(cfg)
}

/// Complete one full TLS handshake and close cleanly, without requesting
/// anything. The server reads the finished handshake and the close_notify, so
/// every per-connection cost is inside this row and not in the request rows.
fn handshake_once(port: u16, config: Arc<rustls::ClientConfig>) -> std::io::Result<()> {
    let tcp = TcpStream::connect(("127.0.0.1", port))?;
    tcp.set_read_timeout(Some(Duration::from_secs(20)))?;
    tcp.set_write_timeout(Some(Duration::from_secs(20)))?;
    tcp.set_nodelay(true)?;
    let name =
        rustls::pki_types::ServerName::try_from("localhost").map_err(std::io::Error::other)?;
    let conn = rustls::ClientConnection::new(config, name).map_err(std::io::Error::other)?;
    let mut stream = StreamOwned::new(conn, tcp);
    stream.conn.complete_io(&mut stream.sock)?;
    stream.conn.send_close_notify();
    while stream.conn.wants_write() {
        stream.conn.write_tls(&mut stream.sock)?;
    }
    stream.sock.shutdown(std::net::Shutdown::Write)?;
    // Read to EOF: the server has then consumed the close_notify and freed the
    // connection, so the next iteration starts from a clean table.
    let mut buf = [0u8; 64];
    loop {
        match stream.sock.read(&mut buf) {
            Ok(0) => return Ok(()),
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return Ok(()),
            Err(e) => return Err(e),
        }
    }
}

/// An HTTP/1.1-over-TLS client that keeps one connection and reads one reply
/// at a time, by `Content-Length`. Sequential requests only.
struct H1Conn {
    stream: StreamOwned<rustls::ClientConnection, TcpStream>,
    buffered: Vec<u8>,
}

impl H1Conn {
    fn connect(port: u16, config: Arc<rustls::ClientConfig>) -> H1Conn {
        let tcp = TcpStream::connect(("127.0.0.1", port)).expect("connect to m6-http");
        tcp.set_read_timeout(Some(Duration::from_secs(20)))
            .expect("read timeout");
        tcp.set_write_timeout(Some(Duration::from_secs(20)))
            .expect("write timeout");
        tcp.set_nodelay(true).expect("nodelay");
        let name = rustls::pki_types::ServerName::try_from("localhost").expect("server name");
        let conn = rustls::ClientConnection::new(config, name).expect("TLS client");
        H1Conn {
            stream: StreamOwned::new(conn, tcp),
            buffered: Vec::new(),
        }
    }

    fn get(&mut self, path: &str, port: u16, close: bool) -> std::io::Result<u16> {
        let connection = if close { "Connection: close\r\n" } else { "" };
        let request = format!("GET {path} HTTP/1.1\r\nHost: localhost:{port}\r\n{connection}\r\n");
        self.stream.write_all(request.as_bytes())?;
        self.stream.flush()?;
        self.read_response()
    }

    fn read_response(&mut self) -> std::io::Result<u16> {
        loop {
            if let Some(end) = self.buffered.windows(4).position(|w| w == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&self.buffered[..end]).into_owned();
                let status = head
                    .lines()
                    .next()
                    .and_then(|line| line.split_whitespace().nth(1))
                    .and_then(|code| code.parse().ok())
                    .unwrap_or(0);
                let length = head
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        if name.eq_ignore_ascii_case("content-length") {
                            value.trim().parse::<usize>().ok()
                        } else {
                            None
                        }
                    })
                    .unwrap_or(0);
                let wanted = end + 4 + length;
                while self.buffered.len() < wanted {
                    self.fill()?;
                }
                self.buffered.drain(..wanted);
                return Ok(status);
            }
            self.fill()?;
        }
    }

    fn fill(&mut self) -> std::io::Result<()> {
        let mut buf = [0u8; 8192];
        let n = self.stream.read(&mut buf)?;
        if n == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "m6-http closed before a reply",
            ));
        }
        self.buffered.extend_from_slice(&buf[..n]);
        Ok(())
    }
}

fn h1_ready(fixture: &Fixture, warm_path: &str) -> H1Conn {
    let config = tls_config(&fixture.cert_der, &[b"http/1.1"]);
    let deadline = Instant::now() + m6_core::testkit::STARTUP_BOUND;
    loop {
        let mut conn = H1Conn::connect(fixture.port(), Arc::clone(&config));
        match conn.get(warm_path, fixture.port(), false) {
            Ok(200) => return conn,
            Ok(status) => panic!("warm-up answered {status}, not 200"),
            Err(e) => {
                if Instant::now() >= deadline {
                    panic!("m6-http never answered the warm-up request: {e}");
                }
                std::thread::sleep(Duration::from_millis(25));
            }
        }
    }
}

fn h2_ready(fixture: &Fixture) -> H2sTlsClientConn {
    let config = tls_config(&fixture.cert_der, &[b"h2"]);
    let deadline = Instant::now() + m6_core::testkit::STARTUP_BOUND;
    loop {
        match H2sTlsClientConn::connect("localhost", fixture.port(), Arc::clone(&config)) {
            Ok(mut conn) => match h2_get(&mut conn, "/", fixture.port()) {
                Ok(200) => return conn,
                Ok(status) => panic!("h2 warm-up answered {status}, not 200"),
                Err(e) => {
                    if Instant::now() >= deadline {
                        panic!("m6-http never answered the h2 warm-up request: {e}");
                    }
                    std::thread::sleep(Duration::from_millis(25));
                }
            },
            Err(e) => {
                if Instant::now() >= deadline {
                    panic!("could not open an h2 connection to m6-http: {e}");
                }
                std::thread::sleep(Duration::from_millis(25));
            }
        }
    }
}

fn h2_get(conn: &mut H2sTlsClientConn, path: &str, port: u16) -> std::io::Result<u16> {
    let request = HttpRequest {
        method: "GET".to_string(),
        path: path.to_string(),
        query: None,
        version: "HTTP/2".to_string(),
        headers: vec![("host".to_string(), format!("localhost:{port}"))],
        body: Vec::new(),
    };
    let stream_id = conn.dispatch(&request, "localhost", "127.0.0.1", "localhost")?;
    let deadline = Instant::now() + m6_core::testkit::STARTUP_BOUND;
    loop {
        conn.drive();
        if let Some(done) = conn
            .drain_completions()
            .into_iter()
            .find(|c| c.stream_id == stream_id)
        {
            return done.result.map(|response| response.status);
        }
        if Instant::now() >= deadline {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "h2 reply timed out",
            ));
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}
