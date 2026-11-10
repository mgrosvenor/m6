//! A slow socket backend no longer stalls the event loop, and concurrent
//! socket-backend requests spread across a pool's members.
//!
//! The bug this guards against: m6-http forwarded a socket-backend miss with a
//! blocking connect, write and read inside its one event loop, so a single
//! slow backend held every connection on the node -- cache hits included -- for
//! up to `backend_timeout_secs`. The fix drives each socket-backend request as
//! a non-blocking upstream in the loop, so a cache hit is served while a slow
//! backend is still thinking, and least-connections now sees real in-flight
//! counts so concurrent misses spread across the members.
//!
//! Each test stands up a real `m6-http` in front of one or two fake socket
//! backends. Every wait is on a signal (a backend reporting it received a
//! request, a response arriving) with a deadline, never a fixed sleep.

#![allow(
    clippy::disallowed_methods,
    reason = "test code: threads, sleeps and blocking clients drive the service from outside"
)]

use std::io::{Read, Write};
use std::net::{TcpStream, UdpSocket};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use rustls::StreamOwned;

use quiche::h3::NameValue as _;

use m6_core::testkit::{binary, claim_port, read_one, PortClaim, Service};
use m6_http_lib::forward::HttpRequest;
use m6_http_lib::h2s_client::H2sTlsClientConn;
use m6_http_lib::probe;

/// The body a backend returns for a request it answers at once.
const FAST_BODY: &str = "fast";
/// The body a backend returns for a request it holds until released.
const SLOW_BODY: &str = "slow";

/// One GET: the port, the path and a deadline in, the status and body out.
type GetFn = fn(u16, &str, Duration) -> Result<(u16, String), String>;

// ── The fake backend ──────────────────────────────────────────────────────────

/// One fake socket backend. `GET /fast` answers at once with a cacheable body;
/// `GET /slow` signals `slow_seen` and holds until `release` (or shutdown). A
/// resetting backend reads the request then closes without replying.
struct Backend {
    path: PathBuf,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Backend {
    fn start(
        path: PathBuf,
        slow_seen: mpsc::Sender<()>,
        large_seen: mpsc::Sender<()>,
        release: Arc<AtomicBool>,
    ) -> Backend {
        Self::spawn(path, slow_seen, large_seen, release, false)
    }

    /// A backend that accepts a request and closes without replying, the state a
    /// backend that resets mid-response leaves.
    fn start_resetting(path: PathBuf) -> Backend {
        let (slow_seen, _) = mpsc::channel();
        let (large_seen, _) = mpsc::channel();
        let release = Arc::new(AtomicBool::new(false));
        Self::spawn(path, slow_seen, large_seen, release, true)
    }

    fn spawn(
        path: PathBuf,
        slow_seen: mpsc::Sender<()>,
        large_seen: mpsc::Sender<()>,
        release: Arc<AtomicBool>,
        reset: bool,
    ) -> Backend {
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).expect("bind backend socket");
        listener
            .set_nonblocking(true)
            .expect("nonblocking listener");
        let stop = Arc::new(AtomicBool::new(false));
        let handle = spawn_accept(
            listener,
            Arc::clone(&stop),
            Arc::clone(&release),
            slow_seen,
            large_seen,
            reset,
        );
        Backend {
            path,
            stop,
            handle: Some(handle),
        }
    }
}

impl Drop for Backend {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
        let _ = std::fs::remove_file(&self.path);
    }
}

fn spawn_accept(
    listener: UnixListener,
    stop: Arc<AtomicBool>,
    release: Arc<AtomicBool>,
    slow_seen: mpsc::Sender<()>,
    large_seen: mpsc::Sender<()>,
    reset: bool,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        while !stop.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((stream, _)) => {
                    let stop = Arc::clone(&stop);
                    let release = Arc::clone(&release);
                    let slow_seen = slow_seen.clone();
                    let large_seen = large_seen.clone();
                    std::thread::spawn(move || {
                        serve(stream, &stop, &release, &slow_seen, &large_seen, reset)
                    });
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(_) => break,
            }
        }
    })
}

/// Read the request head, then answer `/fast` at once, `/large` with 4 MiB,
/// `/echo` with the request body length, or hold `/slow` until `release`. A
/// held request is also released by `stop`. A resetting backend closes without
/// replying.
fn serve(
    mut stream: UnixStream,
    stop: &AtomicBool,
    release: &AtomicBool,
    slow_seen: &mpsc::Sender<()>,
    large_seen: &mpsc::Sender<()>,
    reset: bool,
) {
    // The accepted socket inherits the listener's non-blocking flag. Blocking
    // here so `write_all` of a large body writes it whole rather than stopping
    // at a full buffer.
    let _ = stream.set_nonblocking(false);
    // Read the head byte by byte so the request body stays in the socket for
    // the `/echo` path to read with `read_exact`.
    let mut got = Vec::new();
    let mut byte = [0u8; 1];
    while !got.windows(4).any(|w| w == b"\r\n\r\n") {
        match stream.read(&mut byte) {
            Ok(0) => return,
            Ok(_) => got.push(byte[0]),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(1));
            }
            Err(_) => return,
        }
    }
    if reset {
        return; // close without replying
    }
    let head = String::from_utf8_lossy(&got).to_ascii_lowercase();
    if head.starts_with("get /slow") {
        let _ = slow_seen.send(());
        while !release.load(Ordering::SeqCst) && !stop.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(1));
        }
        if !release.load(Ordering::SeqCst) {
            return; // shutdown while still held
        }
        respond(&mut stream, 200, SLOW_BODY, false);
    } else if head.starts_with("get /large") {
        respond_bytes(&mut stream, 200, &LARGE_BODY, true);
        let _ = large_seen.send(());
    } else if head.starts_with("post /echo") {
        let len = head
            .lines()
            .find_map(|l| l.trim().strip_prefix("content-length:"))
            .and_then(|v| v.trim().parse::<usize>().ok())
            .unwrap_or(0);
        let mut body = vec![0u8; len];
        let _ = stream.read_exact(&mut body);
        respond(&mut stream, 200, &format!("{}", body.len()), false);
    } else {
        respond(&mut stream, 200, FAST_BODY, true);
    }
}

/// 4 MiB, served by `/large` for the slow-reader test.
static LARGE_BODY: [u8; 4 * 1024 * 1024] = [b'x'; 4 * 1024 * 1024];

fn respond_bytes(stream: &mut UnixStream, status: u16, body: &[u8], cache: bool) {
    let cache_header = if cache {
        "Cache-Control: public, max-age=60\r\n"
    } else {
        ""
    };
    let head = format!(
        "HTTP/1.1 {status} OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\
         {cache_header}Connection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(body);
    let _ = stream.flush();
}

fn respond(stream: &mut UnixStream, status: u16, body: &str, cache: bool) {
    let cache_header = if cache {
        "Cache-Control: public, max-age=60\r\n"
    } else {
        ""
    };
    let resp = format!(
        "HTTP/1.1 {status} OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\
         {cache_header}Connection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(resp.as_bytes());
    let _ = stream.flush();
}

// ── Clients: one GET per protocol ─────────────────────────────────────────────

/// HTTP/1.1 over TLS, blocking, with a read timeout.
fn h1_get(port: u16, path: &str, timeout: Duration) -> Result<(u16, String), String> {
    probe::install_crypto();
    let tcp = TcpStream::connect(("127.0.0.1", port)).map_err(|e| format!("connect: {e}"))?;
    tcp.set_read_timeout(Some(timeout))
        .map_err(|e| e.to_string())?;
    let name = rustls::pki_types::ServerName::try_from("127.0.0.1".to_string())
        .map_err(|e| e.to_string())?;
    let conn = rustls::ClientConnection::new(probe::tls_config(b"http/1.1"), name)
        .map_err(|e| e.to_string())?;
    let mut stream = StreamOwned::new(conn, tcp);
    let req = format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n");
    stream
        .write_all(req.as_bytes())
        .map_err(|e| e.to_string())?;
    stream.flush().map_err(|e| e.to_string())?;
    let raw = read_one(&mut stream, "GET").map_err(|e| format!("read: {e}"))?;
    parse_h1(&raw)
}

fn parse_h1(raw: &[u8]) -> Result<(u16, String), String> {
    let text = String::from_utf8_lossy(raw).into_owned();
    let (head, body) = text
        .split_once("\r\n\r\n")
        .ok_or_else(|| format!("no header block: {text}"))?;
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| format!("no status: {head}"))?;
    Ok((status, body.to_string()))
}

/// Whether an h1 response head advertises HTTP/3 via `alt-svc`.
fn h1_has_alt_svc(port: u16, path: &str) -> Result<bool, String> {
    probe::install_crypto();
    let tcp = TcpStream::connect(("127.0.0.1", port)).map_err(|e| format!("connect: {e}"))?;
    tcp.set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(|e| e.to_string())?;
    let name = rustls::pki_types::ServerName::try_from("127.0.0.1".to_string())
        .map_err(|e| e.to_string())?;
    let conn = rustls::ClientConnection::new(probe::tls_config(b"http/1.1"), name)
        .map_err(|e| e.to_string())?;
    let mut stream = StreamOwned::new(conn, tcp);
    let req = format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n");
    stream
        .write_all(req.as_bytes())
        .map_err(|e| e.to_string())?;
    stream.flush().map_err(|e| e.to_string())?;
    let raw = read_one(&mut stream, "GET").map_err(|e| format!("read: {e}"))?;
    let text = String::from_utf8_lossy(&raw).to_ascii_lowercase();
    let head = text.split("\r\n\r\n").next().unwrap_or("");
    Ok(head.contains("alt-svc:"))
}

/// HTTP/2 over TLS, driven in a loop until the reply arrives or the deadline
/// passes.
fn h2_get(port: u16, path: &str, timeout: Duration) -> Result<(u16, String), String> {
    probe::install_crypto();
    let cfg = probe::tls_config(b"h2");
    let mut conn = H2sTlsClientConn::connect("127.0.0.1", port, cfg).map_err(|e| e.to_string())?;
    let req = HttpRequest {
        method: "GET".to_string(),
        path: path.to_string(),
        query: None,
        version: "HTTP/2".to_string(),
        headers: vec![],
        body: vec![],
    };
    let stream_id = conn
        .dispatch(&req, "127.0.0.1", "127.0.0.1", "127.0.0.1")
        .map_err(|e| e.to_string())?;
    let deadline = Instant::now() + timeout;
    loop {
        conn.drive();
        if let Some(c) = conn
            .drain_completions()
            .into_iter()
            .find(|c| c.stream_id == stream_id)
        {
            return match c.result {
                Ok(r) => Ok((r.status, String::from_utf8_lossy(&r.body).into_owned())),
                Err(e) => Err(format!("response: {e}")),
            };
        }
        if Instant::now() >= deadline {
            return Err("timeout".to_string());
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn quic_flush(conn: &mut quiche::Connection, udp: &UdpSocket, out: &mut [u8]) {
    while let Ok((n, info)) = conn.send(out) {
        let _ = udp.send_to(&out[..n], info.to);
    }
}

/// HTTP/3 over QUIC, driven in a loop until the reply arrives or the deadline
/// passes.
fn h3_get(port: u16, path: &str, timeout: Duration) -> Result<(u16, String), String> {
    let server_addr: std::net::SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    let udp = UdpSocket::bind("127.0.0.1:0").map_err(|e| e.to_string())?;
    udp.set_nonblocking(true).unwrap();
    let local = udp.local_addr().unwrap();

    let mut config = quiche::Config::new(quiche::PROTOCOL_VERSION).map_err(|e| e.to_string())?;
    config
        .set_application_protos(quiche::h3::APPLICATION_PROTOCOL)
        .map_err(|e| e.to_string())?;
    config.set_max_idle_timeout(10_000);
    config.set_initial_max_data(10_000_000);
    config.set_initial_max_stream_data_bidi_local(1_000_000);
    config.set_initial_max_stream_data_bidi_remote(1_000_000);
    config.set_initial_max_stream_data_uni(1_000_000);
    config.set_initial_max_streams_bidi(1000);
    config.set_initial_max_streams_uni(100);
    config.grease(false);
    config.verify_peer(false);

    let scid_bytes = [5u8; quiche::MAX_CONN_ID_LEN];
    let scid = quiche::ConnectionId::from_ref(&scid_bytes);
    let mut conn = quiche::connect(Some("localhost"), &scid, local, server_addr, &mut config)
        .map_err(|e| format!("connect: {e}"))?;

    let mut h3: Option<quiche::h3::Connection> = None;
    let mut buf = vec![0u8; 65536];
    let mut out = vec![0u8; 1350];
    let mut sent = false;
    let mut status: Option<u16> = None;
    let mut body: Vec<u8> = Vec::new();
    let mut done = false;
    let deadline = Instant::now() + timeout;

    loop {
        if Instant::now() > deadline {
            return Err(format!("timeout: sent={sent} status={status:?}"));
        }
        conn.on_timeout();
        quic_flush(&mut conn, &udp, &mut out);

        loop {
            match udp.recv_from(&mut buf) {
                Ok((n, from)) => {
                    let info = quiche::RecvInfo { from, to: local };
                    if conn.recv(&mut buf[..n], info).is_err() {
                        break;
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) => return Err(format!("recv: {e}")),
            }
        }

        if conn.is_closed() {
            return Err("connection closed before response".to_string());
        }

        if conn.is_established() && h3.is_none() {
            let cfg = quiche::h3::Config::new().map_err(|e| e.to_string())?;
            h3 = Some(
                quiche::h3::Connection::with_transport(&mut conn, &cfg)
                    .map_err(|e| format!("h3 init: {e}"))?,
            );
        }

        if let Some(ref mut h3c) = h3 {
            if !sent {
                let req = vec![
                    quiche::h3::Header::new(b":method", b"GET"),
                    quiche::h3::Header::new(b":path", path.as_bytes()),
                    quiche::h3::Header::new(b":scheme", b"https"),
                    quiche::h3::Header::new(b":authority", b"localhost"),
                ];
                match h3c.send_request(&mut conn, &req, true) {
                    Ok(_) => sent = true,
                    Err(quiche::h3::Error::Done) => {}
                    Err(e) => return Err(format!("send_request: {e}")),
                }
            }

            loop {
                match h3c.poll(&mut conn) {
                    Ok((_, quiche::h3::Event::Headers { list, .. })) => {
                        for h in &list {
                            let name = std::str::from_utf8(h.name()).unwrap_or("");
                            let value = std::str::from_utf8(h.value()).unwrap_or("");
                            if name == ":status" {
                                if let Ok(code) = value.parse::<u16>() {
                                    if code >= 200 {
                                        status = Some(code);
                                    }
                                }
                            }
                        }
                    }
                    Ok((sid, quiche::h3::Event::Data)) => {
                        let mut tmp = [0u8; 4096];
                        while let Ok(n) = h3c.recv_body(&mut conn, sid, &mut tmp) {
                            if n == 0 {
                                break;
                            }
                            body.extend_from_slice(&tmp[..n]);
                        }
                    }
                    Ok((_, quiche::h3::Event::Finished)) => done = true,
                    Ok(_) => {}
                    Err(quiche::h3::Error::Done) => break,
                    Err(e) => return Err(format!("poll: {e}")),
                }
            }
        }

        quic_flush(&mut conn, &udp, &mut out);
        if done && status.is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(1));
    }

    Ok((
        status.ok_or("no status received")?,
        String::from_utf8_lossy(&body).into_owned(),
    ))
}

// ── The m6-http fixture ───────────────────────────────────────────────────────

struct Fixture {
    _dir: tempfile::TempDir,
    _claim: PortClaim,
    port: u16,
    service: Service,
    _backends: Vec<Backend>,
    slow_seens: Vec<mpsc::Receiver<()>>,
    large_seens: Vec<mpsc::Receiver<()>>,
    release: Arc<AtomicBool>,
}

impl Fixture {
    /// Start `n_backends` socket backends and one m6-http with a pool on their
    /// glob. `backend_timeout_secs` is m6-http's backend deadline.
    fn start(n_backends: usize, backend_timeout_secs: u64, tag: &str) -> Fixture {
        let dir = tempfile::tempdir().expect("tempdir");
        let site = dir.path().to_path_buf();

        let ck = rcgen::generate_simple_self_signed(vec![
            "localhost".to_string(),
            "127.0.0.1".to_string(),
        ])
        .expect("certificate");
        let cert = site.join("cert.pem");
        let key = site.join("key.pem");
        std::fs::write(&cert, ck.cert.pem()).expect("cert");
        std::fs::write(&key, ck.key_pair.serialize_pem()).expect("key");

        let pid = std::process::id();
        let mut backends = Vec::new();
        let mut slow_seens = Vec::new();
        let mut large_seens = Vec::new();
        let release = Arc::new(AtomicBool::new(false));
        for i in 0..n_backends {
            let path = PathBuf::from(format!("/tmp/m6lnw-{pid}-{tag}-{i}.sock"));
            let (tx, rx) = mpsc::channel();
            let (ltx, lrx) = mpsc::channel();
            backends.push(Backend::start(path, tx, ltx, Arc::clone(&release)));
            slow_seens.push(rx);
            large_seens.push(lrx);
        }
        let glob = format!("/tmp/m6lnw-{pid}-{tag}-*.sock");

        let claim = claim_port();
        let port = claim.port();
        std::fs::write(
            site.join("site.toml"),
            site_toml(&glob, backend_timeout_secs),
        )
        .expect("site.toml");
        std::fs::write(site.join("metrics.token"), "loop-never-waits-token").expect("token");
        std::fs::write(
            site.join("system.toml"),
            system_toml(port, &cert, &key, &site.join("metrics.token")),
        )
        .expect("system.toml");

        let service = Service::spawn(
            "m6-http",
            Command::new(binary("m6-http"))
                .arg(&site)
                .arg(site.join("system.toml")),
        );
        let mut service = service;
        service.wait_for_tcp(port, Duration::from_secs(20));

        Fixture {
            _dir: dir,
            _claim: claim,
            port,
            service,
            _backends: backends,
            slow_seens,
            large_seens,
            release,
        }
    }

    fn port(&self) -> u16 {
        self.port
    }
}

fn site_toml(glob: &str, backend_timeout_secs: u64) -> String {
    format!(
        "[site]\nname = \"loop-never-waits\"\ndomain = \"localhost\"\n\n\
         [server]\nbackend_timeout_secs = {backend_timeout_secs}\n\n\
         [log]\nlevel = \"info\"\nformat = \"text\"\n\n\
         [analytics]\nenabled = false\n\n\
         [rate_limit]\nenabled = false\n\n\
         [[backend]]\nname = \"app\"\nsockets = \"{glob}\"\n\n\
         [[route]]\npath = \"/fast\"\nbackend = \"app\"\n\n\
         [[route]]\npath = \"/slow\"\nbackend = \"app\"\n\n\
         [[route]]\npath = \"/large\"\nbackend = \"app\"\n\n\
         [[route]]\npath = \"/echo\"\nbackend = \"app\"\n\n\
         [[route]]\npath = \"/miss/{{id}}\"\nbackend = \"app\"\n"
    )
}

fn system_toml(
    port: u16,
    cert: &std::path::Path,
    key: &std::path::Path,
    token: &std::path::Path,
) -> String {
    format!(
        "[server]\nbind = \"127.0.0.1:{port}\"\ntls_cert = \"{}\"\ntls_key = \"{}\"\nmax_requests_per_connection = 10000\n\n\
         [health]\nmetrics_token_file = \"{}\"\n\n\
         [node]\nname = \"loop-never-waits\"\n",
        cert.display(),
        key.display(),
        token.display(),
    )
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Wait on a channel for a backend to report it received a `/slow` request.
fn await_slow_seen(rx: &mpsc::Receiver<()>, deadline: Instant) -> bool {
    loop {
        match rx.try_recv() {
            Ok(()) => return true,
            Err(mpsc::TryRecvError::Empty) => {
                if Instant::now() >= deadline {
                    return false;
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            Err(_) => return false,
        }
    }
}

/// Wait until `needle` appears in the service's captured log, so a test can
/// synchronise on a server-observable event rather than sleep for a fixed time.
fn wait_for_log(service: &Service, needle: &str, deadline: Instant) {
    while Instant::now() < deadline {
        if service.output().contains(needle) {
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!(
        "no \"{needle}\" in the log within the deadline\n--- m6-http ---\n{}",
        service.output()
    );
}

// ── Tests ─────────────────────────────────────────────────────────────────────

/// Run the "a held backend does not delay a cache hit" scenario with one GET
/// function, shared by the h1, h2 and h3 tests.
fn held_backend_does_not_delay_a_cache_hit(tag: &str, get: GetFn) {
    let fx = Fixture::start(1, 30, tag);

    // Warm /fast: a miss that fills the cache.
    let (warm_status, warm_body) =
        get(fx.port(), "/fast", Duration::from_secs(5)).expect("warm /fast");
    assert_eq!(
        warm_status,
        200,
        "warm /fast: {warm_body}\n--- m6-http ---\n{}",
        fx.service.output()
    );
    assert_eq!(warm_body, FAST_BODY);

    // Start /slow on its own connection, held by the backend.
    let slow_port = fx.port();
    let slow_handle = std::thread::spawn(move || get(slow_port, "/slow", Duration::from_secs(20)));

    // Wait until the backend reports it received /slow.
    let seen = await_slow_seen(&fx.slow_seens[0], Instant::now() + Duration::from_secs(5));
    assert!(
        seen,
        "the backend never received /slow\n--- m6-http ---\n{}",
        fx.service.output()
    );

    // A cache hit must answer within two seconds while /slow is still held.
    let fast = get(fx.port(), "/fast", Duration::from_secs(2));
    let (fast_status, fast_body) = match fast {
        Ok(r) => r,
        Err(e) => {
            fx.release.store(true, Ordering::SeqCst);
            let _ = slow_handle.join();
            panic!(
                "a cache hit stalled behind a held backend: {e}\n--- m6-http ---\n{}",
                fx.service.output()
            );
        }
    };
    assert_eq!(fast_status, 200, "cache hit: {fast_body}");
    assert_eq!(fast_body, FAST_BODY);

    // Release /slow; it must answer.
    fx.release.store(true, Ordering::SeqCst);
    let (slow_status, slow_body) = slow_handle.join().expect("slow thread").expect("/slow");
    assert_eq!(slow_status, 200, "/slow: {slow_body}");
    assert_eq!(slow_body, SLOW_BODY);
}

#[test]
fn a_held_backend_request_does_not_delay_a_cache_hit_over_h1() {
    held_backend_does_not_delay_a_cache_hit("h1", h1_get);
}

#[test]
fn a_held_backend_request_does_not_delay_a_cache_hit_over_h2() {
    held_backend_does_not_delay_a_cache_hit("h2", h2_get);
}

#[test]
fn a_held_backend_request_does_not_delay_a_cache_hit_over_h3() {
    held_backend_does_not_delay_a_cache_hit("h3", h3_get);
}

#[test]
fn two_held_requests_land_on_two_members() {
    let fx = Fixture::start(2, 30, "two");

    // Two concurrent /slow requests on two h1 connections.
    let a_port = fx.port();
    let a = std::thread::spawn(move || h1_get(a_port, "/slow", Duration::from_secs(20)));
    let b_port = fx.port();
    let b = std::thread::spawn(move || h1_get(b_port, "/slow", Duration::from_secs(20)));

    // Both backends must report a received request within two seconds, before
    // either is released.
    let deadline = Instant::now() + Duration::from_secs(2);
    let a_seen = await_slow_seen(&fx.slow_seens[0], deadline);
    let b_seen = await_slow_seen(&fx.slow_seens[1], deadline);
    if !(a_seen && b_seen) {
        fx.release.store(true, Ordering::SeqCst);
        let _ = a.join();
        let _ = b.join();
        panic!(
            "both members must receive a request (a={a_seen}, b={b_seen})\n--- m6-http ---\n{}",
            fx.service.output()
        );
    }

    fx.release.store(true, Ordering::SeqCst);
    let (a_status, a_body) = a.join().expect("a thread").expect("/slow a");
    let (b_status, b_body) = b.join().expect("b thread").expect("/slow b");
    assert_eq!(a_status, 200, "/slow a: {a_body}");
    assert_eq!(a_body, SLOW_BODY);
    assert_eq!(b_status, 200, "/slow b: {b_body}");
    assert_eq!(b_body, SLOW_BODY);
}

#[test]
fn a_backend_past_its_timeout_answers_and_frees_the_member() {
    // A one-second deadline: /slow is never released, so it must answer 502.
    let fx = Fixture::start(1, 1, "timeout");

    let (status, body) =
        h1_get(fx.port(), "/slow", Duration::from_secs(3)).expect("/slow timed out request");
    assert_eq!(
        status, 502,
        "a backend past its timeout must answer 502: {body}"
    );

    // The member was backed off for one second; once it recovers, a /fast miss
    // on the same member is served. Poll with a deadline rather than sleep.
    let recovered = (|| {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match h1_get(fx.port(), "/fast", Duration::from_secs(2)) {
                Ok((200, b)) if b == FAST_BODY => return true,
                Ok(_) => {}
                Err(_) => {}
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    })();
    assert!(
        recovered,
        "the member never served /fast after its backoff\n--- m6-http ---\n{}",
        fx.service.output()
    );
}

#[test]
fn a_socket_backend_miss_advertises_alt_svc() {
    let fx = Fixture::start(1, 30, "altsvc");
    // The cache is empty, so this is a miss that runs through the socket
    // upstream; the pre-EL-1 Ready caller advertised alt-svc, and the miss must
    // keep doing so.
    let has = h1_has_alt_svc(fx.port(), "/fast").expect("GET /fast");
    assert!(
        has,
        "a socket-backend miss must advertise alt-svc\n--- m6-http ---\n{}",
        fx.service.output()
    );
}

// ── Keep-alive client and the per-descriptor-token guards ─────────────────────

/// One keep-alive HTTP/1.1-over-TLS connection, reused across requests.
struct H1KeepAlive {
    stream: rustls::StreamOwned<rustls::ClientConnection, TcpStream>,
}

impl H1KeepAlive {
    fn connect(port: u16) -> H1KeepAlive {
        probe::install_crypto();
        let tcp = TcpStream::connect(("127.0.0.1", port)).expect("connect");
        tcp.set_read_timeout(Some(Duration::from_secs(10)))
            .expect("read timeout");
        let name =
            rustls::pki_types::ServerName::try_from("127.0.0.1".to_string()).expect("server name");
        let conn = rustls::ClientConnection::new(probe::tls_config(b"http/1.1"), name)
            .expect("client conn");
        H1KeepAlive {
            stream: rustls::StreamOwned::new(conn, tcp),
        }
    }

    fn request(&mut self, path: &str, extra: &[(&str, &str)]) -> (u16, String) {
        let mut req = format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n");
        for (k, v) in extra {
            req.push_str(&format!("{k}: {v}\r\n"));
        }
        req.push_str("\r\n");
        self.stream.write_all(req.as_bytes()).expect("write");
        self.stream.flush().expect("flush");
        let raw = read_one(&mut self.stream, "GET").expect("read one response");
        parse_h1(&raw).expect("parse response")
    }
}

/// The `(name, count)` pairs of the event-loop figures `/perf` reports,
/// read over the already-open keep-alive connection.
fn perf_event_loop(conn: &mut H1KeepAlive) -> Vec<(String, u64)> {
    let (status, body) = conn.request(
        "/perf",
        &[("Authorization", "Bearer loop-never-waits-token")],
    );
    assert_eq!(status, 200, "/perf answered {status}: {body}");
    let report: m6_core::monitoring::PerfReport =
        serde_json::from_str(&body).expect("a perf report");
    report
        .metrics
        .event_loop
        .into_iter()
        .map(|t| (t.name, t.count))
        .collect()
}

/// One keep-alive connection that sends 100 requests must fire the connection
/// arm 100 times and the accept arm at most twice: every connection event used
/// to fire the listener's arm, which then tried a failing accept.
#[test]
fn a_connection_event_never_accepts() {
    let fx = Fixture::start(1, 30, "accept");
    let mut conn = H1KeepAlive::connect(fx.port());
    // Warm /fast so the 100 requests below are cache hits.
    let (warm_status, warm_body) = conn.request("/fast", &[]);
    assert_eq!(warm_status, 200, "warm /fast: {warm_body}");
    assert_eq!(warm_body, FAST_BODY);
    for _ in 0..100 {
        let (status, body) = conn.request("/fast", &[]);
        assert_eq!(status, 200, "GET /fast: {body}");
        assert_eq!(body, FAST_BODY);
    }
    // Wait for the accounting tick that copies the loop trace into /perf.
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut accept = None;
    let mut conn_events = None;
    while Instant::now() < deadline {
        let loops = perf_event_loop(&mut conn);
        let a = loops.iter().find(|(n, _)| n == "accept").map(|(_, c)| *c);
        let c = loops.iter().find(|(n, _)| n == "conn").map(|(_, c)| *c);
        if let (Some(a), Some(c)) = (a, c) {
            if c >= 100 {
                accept = Some(a);
                conn_events = Some(c);
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let accept = accept.expect("the accept trigger never reported 100 connection events");
    let conn_events = conn_events.expect("the conn trigger never reached 100");
    assert!(
        accept <= 2,
        "accept fired {accept} times; expected at most 2"
    );
    assert!(
        conn_events >= 100,
        "conn fired {conn_events} times; expected at least 100"
    );
}

/// A 4 MiB response to a client with a 64 KiB receive buffer is resumed by
/// write readiness, not by a timer, and a cache hit answers while it drains.
#[test]
fn a_large_response_to_a_slow_reader_is_resumed_by_write_readiness() {
    let fx = Fixture::start(1, 30, "large");
    probe::install_crypto();
    let tcp = TcpStream::connect(("127.0.0.1", fx.port())).expect("connect");
    let sock = socket2::SockRef::from(&tcp);
    sock.set_recv_buffer_size(64 * 1024).expect("recv buffer");
    tcp.set_read_timeout(Some(Duration::from_secs(10)))
        .expect("read timeout");
    let name =
        rustls::pki_types::ServerName::try_from("127.0.0.1".to_string()).expect("server name");
    let conn =
        rustls::ClientConnection::new(probe::tls_config(b"http/1.1"), name).expect("client conn");
    let mut stream = rustls::StreamOwned::new(conn, tcp);
    stream
        .write_all(b"GET /large HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
        .expect("write");
    stream.flush().expect("flush");
    // Wait until the backend has written the whole response, then drain it.
    let seen = await_slow_seen(&fx.large_seens[0], Instant::now() + Duration::from_secs(5));
    assert!(seen, "the backend never wrote /large");
    let mut body = Vec::new();
    let _ = stream.read_to_end(&mut body);
    let head_len = body
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|p| p + 4)
        .unwrap_or(0);
    assert_eq!(
        body.len() - head_len,
        LARGE_BODY.len(),
        "the 4 MiB body arrived intact (got {} body bytes)",
        body.len().saturating_sub(head_len)
    );
    // While the slow reader drains, a cache hit on a second connection answers
    // within two seconds.
    let fast = h1_get(fx.port(), "/fast", Duration::from_secs(2));
    let (fast_status, fast_body) = match fast {
        Ok(r) => r,
        Err(e) => panic!("a cache hit stalled behind the large write: {e}"),
    };
    assert_eq!(fast_status, 200, "cache hit: {fast_body}");
    assert_eq!(fast_body, FAST_BODY);
}

/// A 1 MiB request body over TLS arrives whole: the turn drains rustls's
/// plaintext buffer and reads on, rather than stopping at the 16 KiB limit.
#[test]
fn a_request_body_larger_than_the_tls_plaintext_buffer_arrives_whole() {
    let fx = Fixture::start(1, 30, "echo");
    probe::install_crypto();
    let tcp = TcpStream::connect(("127.0.0.1", fx.port())).expect("connect");
    tcp.set_read_timeout(Some(Duration::from_secs(10)))
        .expect("read timeout");
    let name =
        rustls::pki_types::ServerName::try_from("127.0.0.1".to_string()).expect("server name");
    let conn =
        rustls::ClientConnection::new(probe::tls_config(b"http/1.1"), name).expect("client conn");
    let mut stream = rustls::StreamOwned::new(conn, tcp);
    let body = vec![b'p'; 1024 * 1024];
    let head = format!(
        "POST /echo HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).expect("write head");
    stream.write_all(&body).expect("write body");
    stream.flush().expect("flush");
    let raw = match read_one(&mut stream, "POST") {
        Ok(r) => r,
        Err(e) => panic!(
            "read one response: {e}\n--- m6-http ---\n{}",
            fx.service.output()
        ),
    };
    let (status, echoed) = parse_h1(&raw).expect("parse response");
    assert_eq!(status, 200, "POST /echo: {echoed}");
    assert_eq!(
        echoed.trim(),
        "1048576",
        "the backend echoed the full 1 MiB body length"
    );
}

#[test]
fn a_backend_that_resets_mid_response_is_backed_off_and_the_pool_fails_over() {
    let dir = tempfile::tempdir().expect("tempdir");
    let site = dir.path().to_path_buf();
    let ck =
        rcgen::generate_simple_self_signed(vec!["localhost".to_string(), "127.0.0.1".to_string()])
            .expect("certificate");
    let cert = site.join("cert.pem");
    let key = site.join("key.pem");
    std::fs::write(&cert, ck.cert.pem()).expect("cert");
    std::fs::write(&key, ck.key_pair.serialize_pem()).expect("key");

    let pid = std::process::id();
    // The glob yields paths in ascending name order, so the resetting member is
    // member 0 and takes the first request.
    let reset_path = PathBuf::from(format!("/tmp/m6lnw-{pid}-fover-0.sock"));
    let healthy_path = PathBuf::from(format!("/tmp/m6lnw-{pid}-fover-1.sock"));
    let glob = format!("/tmp/m6lnw-{pid}-fover-*.sock");
    let _reset = Backend::start_resetting(reset_path);
    let (slow_seen, _slow_rx) = mpsc::channel();
    let (large_seen, _large_rx) = mpsc::channel();
    let release = Arc::new(AtomicBool::new(false));
    let _healthy = Backend::start(healthy_path, slow_seen, large_seen, Arc::clone(&release));

    std::fs::write(site.join("site.toml"), site_toml(&glob, 30)).expect("site.toml");
    std::fs::write(site.join("metrics.token"), "loop-never-waits-token").expect("token");
    let claim = claim_port();
    let port = claim.port();
    std::fs::write(
        site.join("system.toml"),
        system_toml(port, &cert, &key, &site.join("metrics.token")),
    )
    .expect("system.toml");

    let service = Service::spawn(
        "m6-http",
        Command::new(binary("m6-http"))
            .arg(&site)
            .arg(site.join("system.toml")),
    );
    let mut service = service;
    service.wait_for_tcp(port, Duration::from_secs(20));

    // The first request hits the resetting member and answers 502, and the
    // member is backed off. The overcorrected form (release on every error)
    // keeps it in rotation, so this assertion pins the backoff.
    let (first, _) = h1_get(port, "/fast", Duration::from_secs(5)).expect("first GET");
    assert_eq!(first, 502, "a resetting backend must answer 502");

    // The next request must be served by the healthy member.
    let (second, body) = h1_get(port, "/fast", Duration::from_secs(5)).expect("second GET");
    assert_eq!(
        second,
        200,
        "the pool must fail over to the healthy member, got: {body}\n--- m6-http ---\n{}",
        service.output()
    );
    assert_eq!(body, FAST_BODY);
}

// ── Direct completion guards ─────────────────────────────────────────────────

/// How many times the accounting tick's "periodic stats" line has been written.
fn periodic_lines(fx: &Fixture) -> usize {
    fx.service.output().matches("periodic stats").count()
}

/// Wait for the next accounting tick, observed as a new "periodic stats" line.
fn wait_for_periodic(fx: &Fixture, after: usize) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        if periodic_lines(fx) > after {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!(
        "no accounting tick within the deadline\n--- m6-http ---\n{}",
        fx.service.output()
    );
}

/// The count of one named event-loop trigger in a `/perf` snapshot.
fn trigger_count(loops: &[(String, u64)], name: &str) -> u64 {
    loops
        .iter()
        .find(|(n, _)| n == name)
        .map(|(_, c)| *c)
        .unwrap_or(0)
}

/// A socket reply is written into the waiting h1 connection in the upstream's
/// own turn, so it costs one `conn` event (the request read) and no writable
/// `conn` wake. The broken form -- writing on the connection's next turn after
/// setting write interest -- adds a `conn` writable wake per request.
#[test]
fn a_socket_reply_is_written_in_the_turn_it_arrives() {
    let fx = Fixture::start(1, 30, "sameturn");
    let mut conn = H1KeepAlive::connect(fx.port());
    // One warm miss so the pool and the loop are live before the measured window.
    let (warm, _) = conn.request("/fast", &[]);
    assert_eq!(warm, 200, "warm /fast");

    // Start the measured window at an accounting tick, then issue 50 misses.
    let lines = periodic_lines(&fx);
    wait_for_periodic(&fx, lines);
    for i in 0..50 {
        let (status, body) = conn.request(&format!("/miss/{i}"), &[]);
        assert_eq!(status, 200, "/miss/{i}: {body}");
        assert_eq!(body, FAST_BODY);
    }
    // The next tick snapshots the window that held the 50 misses.
    wait_for_periodic(&fx, periodic_lines(&fx));

    let loops = perf_event_loop(&mut conn);
    let upstream = trigger_count(&loops, "upstream");
    let conn_events = trigger_count(&loops, "conn");
    assert!(
        upstream >= 50,
        "the upstream trigger rose {upstream}; expected at least 50\n--- m6-http ---\n{}",
        fx.service.output()
    );
    assert!(
        conn_events <= 60,
        "the conn trigger rose {conn_events}; expected at most 60 (no writable wake)\n--- m6-http ---\n{}",
        fx.service.output()
    );
}

/// A client that closes while its `/slow` is in flight does not leak the pool
/// member: when the reply arrives for the gone waiter, the member's in-flight
/// count returns to 0, so the next two concurrent misses spread across both.
#[test]
fn a_closed_client_does_not_leak_its_upstream() {
    let fx = Fixture::start(2, 30, "leak");
    probe::install_crypto();

    // Open a connection, send /slow, and keep it open until member A reports
    // the request was dispatched, then close it while the backend still holds.
    let port = fx.port();
    let tcp = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    let name =
        rustls::pki_types::ServerName::try_from("127.0.0.1".to_string()).expect("server name");
    let tls =
        rustls::ClientConnection::new(probe::tls_config(b"http/1.1"), name).expect("client conn");
    let mut stream = StreamOwned::new(tls, tcp);
    stream
        .write_all(b"GET /slow HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
        .expect("write");
    stream.flush().expect("flush");
    let seen = await_slow_seen(&fx.slow_seens[0], Instant::now() + Duration::from_secs(5));
    assert!(
        seen,
        "member A never received /slow\n--- m6-http ---\n{}",
        fx.service.output()
    );
    drop(stream); // the client closes with the request still held by the backend

    // Release member A's held reply; it lands on a gone waiter, so the member
    // must be released back to the pool.
    fx.release.store(true, Ordering::SeqCst);

    // Two concurrent /slow must now spread across both members within 2 s.
    let a = std::thread::spawn(move || h1_get(port, "/slow", Duration::from_secs(20)));
    let b = std::thread::spawn(move || h1_get(port, "/slow", Duration::from_secs(20)));
    let deadline = Instant::now() + Duration::from_secs(2);
    let a_seen = await_slow_seen(&fx.slow_seens[0], deadline);
    let b_seen = await_slow_seen(&fx.slow_seens[1], deadline);
    if !(a_seen && b_seen) {
        let _ = a.join();
        let _ = b.join();
        panic!(
            "both members must receive a request after the gone waiter released its member (a={a_seen}, b={b_seen})\n--- m6-http ---\n{}",
            fx.service.output()
        );
    }
    let (a_status, a_body) = a.join().expect("a thread").expect("/slow a");
    let (b_status, b_body) = b.join().expect("b thread").expect("/slow b");
    assert_eq!(a_status, 200, "/slow a: {a_body}");
    assert_eq!(b_status, 200, "/slow b: {b_body}");
}

/// A custom error page fetched from a socket backend is non-blocking: while the
/// error-page backend holds its answer, a cache hit on another connection still
/// answers within two seconds, and the original request then gets the page.
#[test]
fn a_custom_error_page_from_a_socket_backend_is_served_without_a_blocking_fetch() {
    const ERROR_PAGE_BODY: &str = "SITE ERROR PAGE BODY";
    let dir = tempfile::tempdir().expect("tempdir");
    let site = dir.path().to_path_buf();

    let ck =
        rcgen::generate_simple_self_signed(vec!["localhost".to_string(), "127.0.0.1".to_string()])
            .expect("certificate");
    let cert = site.join("cert.pem");
    let key = site.join("key.pem");
    std::fs::write(&cert, ck.cert.pem()).expect("cert");
    std::fs::write(&key, ck.key_pair.serialize_pem()).expect("key");

    // One socket backend: /errors is held until released, /missing is a 404,
    // everything else answers at once and is cacheable.
    let pid = std::process::id();
    let sock = PathBuf::from(format!("/tmp/m6lnw-{pid}-err.sock"));
    let _ = std::fs::remove_file(&sock);
    let listener = UnixListener::bind(&sock).expect("bind backend socket");
    listener.set_nonblocking(true).expect("nonblocking");
    let (error_seen_tx, error_seen_rx) = mpsc::channel();
    let release = Arc::new(AtomicBool::new(false));
    let backend_stop = Arc::new(AtomicBool::new(false));
    let backend = std::thread::spawn({
        let stop = Arc::clone(&backend_stop);
        let release = Arc::clone(&release);
        move || {
            while !stop.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let stop = Arc::clone(&stop);
                        let release = Arc::clone(&release);
                        let error_seen_tx = error_seen_tx.clone();
                        std::thread::spawn(move || {
                            let mut stream = stream;
                            let _ = stream.set_nonblocking(false);
                            let mut got = Vec::new();
                            let mut byte = [0u8; 1];
                            while !got.windows(4).any(|w| w == b"\r\n\r\n") {
                                match stream.read(&mut byte) {
                                    Ok(0) => return,
                                    Ok(_) => got.push(byte[0]),
                                    Err(_) => return,
                                }
                            }
                            let head = String::from_utf8_lossy(&got).to_ascii_lowercase();
                            if head.starts_with("get /errors") {
                                let _ = error_seen_tx.send(());
                                while !release.load(Ordering::SeqCst)
                                    && !stop.load(Ordering::SeqCst)
                                {
                                    std::thread::sleep(Duration::from_millis(1));
                                }
                                if !release.load(Ordering::SeqCst) {
                                    return;
                                }
                                let resp = format!(
                                    "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{ERROR_PAGE_BODY}",
                                    ERROR_PAGE_BODY.len()
                                );
                                let _ = stream.write_all(resp.as_bytes());
                                let _ = stream.flush();
                            } else if head.starts_with("get /missing") {
                                let resp = b"HTTP/1.1 404 Not Found\r\nContent-Type: text/html\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                                let _ = stream.write_all(resp);
                                let _ = stream.flush();
                            } else {
                                let resp = format!(
                                    "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nCache-Control: public, max-age=60\r\nConnection: close\r\n\r\n{FAST_BODY}",
                                    FAST_BODY.len()
                                );
                                let _ = stream.write_all(resp.as_bytes());
                                let _ = stream.flush();
                            }
                        });
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    Err(_) => break,
                }
            }
        }
    });

    let glob = format!("/tmp/m6lnw-{pid}-err.sock");
    std::fs::write(
        site.join("site.toml"),
        format!(
            "[site]\nname = \"loop-never-waits\"\ndomain = \"localhost\"\n\n\
             [server]\nbackend_timeout_secs = 30\n\n\
             [log]\nlevel = \"info\"\nformat = \"text\"\n\n\
             [analytics]\nenabled = false\n\n\
             [rate_limit]\nenabled = false\n\n\
             [errors]\nmode = \"custom\"\npath = \"/errors\"\n\n\
             [[backend]]\nname = \"app\"\nsockets = \"{glob}\"\n\n\
             [[route]]\npath = \"/fast\"\nbackend = \"app\"\n\n\
             [[route]]\npath = \"/missing\"\nbackend = \"app\"\n\n\
             [[route]]\npath = \"/errors\"\nbackend = \"app\"\n"
        ),
    )
    .expect("site.toml");
    std::fs::write(site.join("metrics.token"), "loop-never-waits-token").expect("token");
    let claim = claim_port();
    let port = claim.port();
    std::fs::write(
        site.join("system.toml"),
        system_toml(port, &cert, &key, &site.join("metrics.token")),
    )
    .expect("system.toml");

    let mut service = Service::spawn(
        "m6-http",
        Command::new(binary("m6-http"))
            .arg(&site)
            .arg(site.join("system.toml")),
    );
    service.wait_for_tcp(port, Duration::from_secs(20));

    // Warm /fast so a later request is a cache hit.
    let (warm, _) = h1_get(port, "/fast", Duration::from_secs(5)).expect("warm /fast");
    assert_eq!(warm, 200);

    // /missing 404s and triggers the error-page fetch, which the backend holds.
    let missing = std::thread::spawn(move || {
        h1_get(port, "/missing", Duration::from_secs(20)).expect("/missing")
    });

    // Wait until the backend reports the /errors fetch.
    let seen = await_slow_seen(&error_seen_rx, Instant::now() + Duration::from_secs(5));
    assert!(
        seen,
        "the backend never received /errors\n--- m6-http ---\n{}",
        service.output()
    );

    // While /errors is held, a cache hit on another connection answers in 2 s.
    let fast = h1_get(port, "/fast", Duration::from_secs(2));
    match fast {
        Ok((status, body)) => {
            assert_eq!(status, 200, "cache hit: {body}");
            assert_eq!(body, FAST_BODY);
        }
        Err(e) => {
            release.store(true, Ordering::SeqCst);
            let _ = missing.join();
            panic!(
                "a cache hit stalled behind a held error-page fetch: {e}\n--- m6-http ---\n{}",
                service.output()
            );
        }
    }

    // Release the error page; /missing must now get the custom page body.
    release.store(true, Ordering::SeqCst);
    let (status, body) = missing.join().expect("missing thread");
    assert_eq!(status, 404, "the failing request keeps its 404: {body}");
    assert_eq!(body, ERROR_PAGE_BODY);

    backend_stop.store(true, Ordering::SeqCst);
    let _ = backend.join();
    let _ = std::fs::remove_file(&sock);
}

/// A reply that lands after its client was reaped must not be delivered into a
/// later connection that reused the token. Each `Waiter` carries the
/// connection's generation, so the stale completion is dropped.
#[test]
fn a_reaped_clients_stale_reply_is_not_delivered_to_a_later_connection() {
    const SECOND_BODY: &str = "second";
    let dir = tempfile::tempdir().expect("tempdir");
    let site = dir.path().to_path_buf();

    let ck =
        rcgen::generate_simple_self_signed(vec!["localhost".to_string(), "127.0.0.1".to_string()])
            .expect("certificate");
    let cert = site.join("cert.pem");
    let key = site.join("key.pem");
    std::fs::write(&cert, ck.cert.pem()).expect("cert");
    std::fs::write(&key, ck.key_pair.serialize_pem()).expect("key");

    // One socket backend: /slow is held with SLOW_BODY, /slow2 is held with a
    // distinct body, each reporting its arrival on its own channel.
    let pid = std::process::id();
    let sock = PathBuf::from(format!("/tmp/m6lnw-{pid}-stale.sock"));
    let _ = std::fs::remove_file(&sock);
    let listener = UnixListener::bind(&sock).expect("bind backend socket");
    listener.set_nonblocking(true).expect("nonblocking");
    let (slow_seen_tx, slow_seen_rx) = mpsc::channel();
    let (slow2_seen_tx, slow2_seen_rx) = mpsc::channel();
    // Two releases, so the stale /slow reply can be forced to arrive while
    // /slow2 is still held, making the misdelivery (and its guard) observable.
    let release_slow = Arc::new(AtomicBool::new(false));
    let release_slow2 = Arc::new(AtomicBool::new(false));
    let stop = Arc::new(AtomicBool::new(false));
    let backend = std::thread::spawn({
        let stop = Arc::clone(&stop);
        let release_slow = Arc::clone(&release_slow);
        let release_slow2 = Arc::clone(&release_slow2);
        move || {
            while !stop.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let stop = Arc::clone(&stop);
                        let release_slow = Arc::clone(&release_slow);
                        let release_slow2 = Arc::clone(&release_slow2);
                        let slow_seen_tx = slow_seen_tx.clone();
                        let slow2_seen_tx = slow2_seen_tx.clone();
                        std::thread::spawn(move || {
                            let mut stream = stream;
                            let _ = stream.set_nonblocking(false);
                            let mut got = Vec::new();
                            let mut byte = [0u8; 1];
                            while !got.windows(4).any(|w| w == b"\r\n\r\n") {
                                match stream.read(&mut byte) {
                                    Ok(0) => return,
                                    Ok(_) => got.push(byte[0]),
                                    Err(_) => return,
                                }
                            }
                            let head = String::from_utf8_lossy(&got).to_ascii_lowercase();
                            let (seen_tx, body, release) = if head.starts_with("get /slow2") {
                                (&slow2_seen_tx, SECOND_BODY, &release_slow2)
                            } else {
                                (&slow_seen_tx, SLOW_BODY, &release_slow)
                            };
                            let _ = seen_tx.send(());
                            while !release.load(Ordering::SeqCst) && !stop.load(Ordering::SeqCst) {
                                std::thread::sleep(Duration::from_millis(1));
                            }
                            if !release.load(Ordering::SeqCst) {
                                return;
                            }
                            let resp = format!(
                                "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                                body.len()
                            );
                            let _ = stream.write_all(resp.as_bytes());
                            let _ = stream.flush();
                        });
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    Err(_) => break,
                }
            }
        }
    });

    std::fs::write(
        site.join("site.toml"),
        format!(
            "[site]\nname = \"loop-never-waits\"\ndomain = \"localhost\"\n\n\
             [server]\nbackend_timeout_secs = 30\n\n\
             [log]\nlevel = \"trace\"\nformat = \"text\"\n\n\
             [analytics]\nenabled = false\n\n\
             [rate_limit]\nenabled = false\n\n\
             [[backend]]\nname = \"app\"\nsockets = \"{}\"\n\n\
             [[route]]\npath = \"/slow\"\nbackend = \"app\"\n\n\
             [[route]]\npath = \"/slow2\"\nbackend = \"app\"\n",
            sock.display()
        ),
    )
    .expect("site.toml");
    std::fs::write(site.join("metrics.token"), "loop-never-waits-token").expect("token");
    let claim = claim_port();
    let port = claim.port();
    let _claim = claim;
    std::fs::write(
        site.join("system.toml"),
        system_toml(port, &cert, &key, &site.join("metrics.token")),
    )
    .expect("system.toml");

    let mut service = Service::spawn(
        "m6-http",
        Command::new(binary("m6-http"))
            .arg(&site)
            .arg(site.join("system.toml")),
    );
    service.wait_for_tcp(port, Duration::from_secs(20));

    let h2_req = |path: &str| HttpRequest {
        method: "GET".to_string(),
        path: path.to_string(),
        query: None,
        version: "HTTP/2".to_string(),
        headers: vec![],
        body: vec![],
    };

    // Client A dispatches /slow, then closes while the backend holds it.
    probe::install_crypto();
    {
        let cfg = probe::tls_config(b"h2");
        let mut a = H2sTlsClientConn::connect("127.0.0.1", port, cfg).expect("client A");
        a.dispatch(&h2_req("/slow"), "127.0.0.1", "127.0.0.1", "127.0.0.1")
            .expect("dispatch /slow");
        a.drive();
        let seen = await_slow_seen(&slow_seen_rx, Instant::now() + Duration::from_secs(5));
        assert!(
            seen,
            "the backend never received /slow\n--- m6-http ---\n{}",
            service.output()
        );
        // `a` drops: the client closes with its /slow still in flight.
    }

    // Wait until the loop has reaped A (its h2 EOF is observed at trace level),
    // so the freed token is handed back out and client B deterministically
    // reuses it.
    wait_for_log(
        &service,
        "h2 fill_recv",
        Instant::now() + Duration::from_secs(5),
    );

    // Client B, on the reused token, dispatches /slow2 (also held).
    let cfg = probe::tls_config(b"h2");
    let mut b = H2sTlsClientConn::connect("127.0.0.1", port, cfg).expect("client B");
    let sid = b
        .dispatch(&h2_req("/slow2"), "127.0.0.1", "127.0.0.1", "127.0.0.1")
        .expect("dispatch /slow2");
    b.drive();
    let seen = await_slow_seen(&slow2_seen_rx, Instant::now() + Duration::from_secs(5));
    assert!(
        seen,
        "the backend never received /slow2\n--- m6-http ---\n{}",
        service.output()
    );

    // Release A's stale /slow first. Its reply must be dropped, so B receives
    // nothing yet; a SLOW_BODY here means the stale waiter was delivered.
    release_slow.store(true, Ordering::SeqCst);
    let stale_deadline = Instant::now() + Duration::from_secs(2);
    loop {
        b.drive();
        if let Some(c) = b
            .drain_completions()
            .into_iter()
            .find(|c| c.stream_id == sid)
        {
            let r = c.result.expect("client B's response");
            assert_eq!(
                String::from_utf8_lossy(&r.body),
                SECOND_BODY,
                "client B received a stale reply before /slow2 was released\n--- m6-http ---\n{}",
                service.output()
            );
        }
        if Instant::now() >= stale_deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(1));
    }

    // Now release B's own /slow2 and collect its reply.
    release_slow2.store(true, Ordering::SeqCst);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        b.drive();
        if let Some(c) = b
            .drain_completions()
            .into_iter()
            .find(|c| c.stream_id == sid)
        {
            let r = c.result.expect("client B's response");
            assert_eq!(r.status, 200);
            assert_eq!(
                String::from_utf8_lossy(&r.body),
                SECOND_BODY,
                "client B got the wrong body\n--- m6-http ---\n{}",
                service.output()
            );
            break;
        }
        if Instant::now() >= deadline {
            panic!(
                "client B got no response\n--- m6-http ---\n{}",
                service.output()
            );
        }
        std::thread::sleep(Duration::from_millis(1));
    }

    stop.store(true, Ordering::SeqCst);
    let _ = backend.join();
    let _ = std::fs::remove_file(&sock);
}
