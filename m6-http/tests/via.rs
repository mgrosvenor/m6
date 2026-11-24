//! `Via` through the running service: what a backend actually receives, on
//! every protocol and from every kind of listener.
//!
//! RFC 9110 7.6.3 makes `Via` a comma-separated list of `received-protocol
//! received-by` entries in path order. This node appends one entry naming
//! itself, from `[node] name`, the identity `/health` reports, so a reader can
//! tell which node forwarded a request. The site reads the FIRST entry as the
//! node that took the request, so the order is the contract and every
//! assertion here is on the whole string.
//!
//! Two rules decide what a hop may believe:
//!
//! | listener | peer | a `Via` that arrives | the backend receives |
//! |---|---|---|---|
//! | `[server] bind`, TLS, h1/h2/h3 | anyone | the client's claim, dropped | this node's entry alone |
//! | `[server] h2c_bind` named in `internal_binds` | an `internal_peers` address | a cache node's entry, kept | that entry, then this node's |
//! | the same h2c listener declared public | anyone | the client's claim, dropped | this node's entry alone |
//!
//! The trust is the one that already decides whether a forwarded client
//! address is believed (`forward::ForwardedTrust`), granted by the listener at
//! accept, so a spoofed `Via` fails on the same tests that a spoofed
//! `X-Forwarded-For` does.
//!
//! Each test reads the header from a backend that captures the request bytes,
//! which is the only place the forwarded form exists.

#![allow(
    clippy::disallowed_methods,
    reason = "test code: threads, sleeps and blocking clients drive the service from outside"
)]

use std::io::{Read, Write};
use std::net::{TcpStream, UdpSocket};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use m6_core::testkit::{binary, claim_port, PortClaim, Service};
use m6_http_lib::forward::HttpRequest;
use m6_http_lib::h2c_client::H2cClientConn;

// ── A backend that keeps the request head it receives ────────────────────────

/// A unix-socket backend that records each request head and answers 200.
///
/// The service connects once per request and closes, so each request is one
/// accept. The thread stops when the test's `Capture` is dropped.
struct Capture {
    path: PathBuf,
    requests: mpsc::Receiver<String>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    /// The scratch directory the socket lives in. Its own, so tests running in
    /// parallel do not bind the same path.
    _dir: tempfile::TempDir,
}

impl Capture {
    fn start(root: &Path) -> Self {
        let dir = tempfile::tempdir_in(root).expect("capture tempdir");
        let path = dir.path().join("capture-1.sock");
        let listener = UnixListener::bind(&path).expect("bind the capture backend");
        listener.set_nonblocking(true).expect("non-blocking accept");
        let (tx, requests) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let thread = std::thread::spawn(move || {
            while !flag.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        // The listener is non-blocking so the accept loop can
                        // notice `stop`. On macOS (and the other BSDs) an
                        // accepted socket inherits O_NONBLOCK from its listener,
                        // where Linux clears it. Left as inherited, the first
                        // `read` below returns `WouldBlock` whenever this thread
                        // runs before m6-http has written the request, and the
                        // `Err(_)` arm then closes the connection: m6-http reads
                        // EOF before any headers and answers 502 for a request
                        // the backend never saw. The stream is made blocking so
                        // the read waits, bounded by the timeout.
                        stream.set_nonblocking(false).expect("blocking connection");
                        // One thread per connection, and a response only after a
                        // full request head: m6-http keeps pooled connections
                        // open, and a backend that answered an idle connection
                        // handed m6-http a reply to a request it had not yet
                        // sent, so the test saw 200 and the backend saw nothing.
                        let tx = tx.clone();
                        std::thread::spawn(move || {
                            stream
                                .set_read_timeout(Some(Duration::from_secs(30)))
                                .expect("read timeout");
                            let mut raw = Vec::new();
                            let mut buf = [0u8; 4096];
                            while !raw.windows(4).any(|w| w == b"\r\n\r\n") {
                                match stream.read(&mut buf) {
                                    Ok(0) | Err(_) => return,
                                    Ok(n) => raw.extend_from_slice(&buf[..n]),
                                }
                            }
                            let _ = tx.send(String::from_utf8_lossy(&raw).into_owned());
                            let _ = stream.write_all(
                                b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                            );
                            let _ = stream.flush();
                        });
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(2));
                    }
                    Err(_) => break,
                }
            }
        });
        Capture {
            path,
            requests,
            stop,
            thread: Some(thread),
            _dir: dir,
        }
    }

    /// The socket glob the service's backend pool reads at start.
    fn glob(&self) -> String {
        self.path
            .with_file_name("capture-*.sock")
            .display()
            .to_string()
    }

    /// The request head the backend received. The client reads its response
    /// first, so the head is already queued by the time this is called.
    fn head(&self) -> String {
        let deadline = Instant::now() + m6_core::testkit::STARTUP_BOUND;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.requests.recv_timeout(left) {
                Ok(head) if !head.is_empty() => return head,
                Ok(_) => continue,
                Err(_) => panic!("the backend received no request"),
            }
        }
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// The value of every `name` field in a captured request head, in wire order.
fn header_values(head: &str, name: &str) -> Vec<String> {
    let want = format!("{}:", name.to_ascii_lowercase());
    head.split("\r\n")
        .filter(|line| line.to_ascii_lowercase().starts_with(&want))
        .map(|line| line[line.find(':').unwrap() + 1..].trim().to_string())
        .collect()
}

/// The one `Via` value a backend receives, asserting there is exactly one
/// field. A hop that appends blindly produces two, which is legal to merge but
/// not what this proxy writes.
fn via(head: &str) -> String {
    let values = header_values(head, "via");
    assert_eq!(
        values.len(),
        1,
        "expected exactly one Via field, got {values:?} in:\n{head}"
    );
    values.into_iter().next().unwrap()
}

// ── Nodes under test ────────────────────────────────────────────────────────

struct Node {
    port: u16,
    cert_der: Vec<u8>,
    /// Held so the process lives as long as the node does, then dropped so it
    /// stops. Read only for its output when a test fails.
    _service: Service,
    /// Every port the node listens on. A claim released while its service
    /// still listens waits for the port to come free and never sees it.
    _ports: Vec<PortClaim>,
    _dir: tempfile::TempDir,
}

impl Drop for Node {
    fn drop(&mut self) {
        // What the node logged is the only record of why it answered as it
        // did, and a failed assertion is the only time anyone wants it.
        if std::thread::panicking() {
            eprintln!("--- node output ---\n{}", self._service.output());
        }
    }
}

/// One certificate for both the TLS listener and the clients here.
fn cert() -> (String, String, Vec<u8>) {
    let ck =
        rcgen::generate_simple_self_signed(vec!["localhost".to_string(), "127.0.0.1".to_string()])
            .expect("rcgen");
    (
        ck.cert.pem(),
        ck.key_pair.serialize_pem(),
        ck.cert.der().to_vec(),
    )
}

/// A site whose one route forwards to the capture backend.
fn write_site(site: &Path, backends_and_routes: &str) {
    std::fs::create_dir_all(site).expect("site dir");
    std::fs::write(
        site.join("site.toml"),
        format!(
            "[site]\nname = \"via\"\ndomain = \"localhost\"\n\n\
             [log]\nlevel = \"warn\"\n\n\
             [errors]\nmode = \"internal\"\n\n\
             [analytics]\nenabled = false\n\n\
             {backends_and_routes}"
        ),
    )
    .expect("site.toml");
}

/// A system config naming `node` and, given `extra`, a listener beside the
/// public TLS one.
fn write_system(config: &Path, bind: &str, node: &str, extra: &str, certs: &Path) {
    std::fs::write(
        config,
        format!(
            "[server]\nbind = \"{bind}\"\ntls_cert = \"{cert}\"\ntls_key = \"{key}\"\n\
             {extra}\n[node]\nname = \"{node}\"\n",
            cert = certs.join("cert.pem").display(),
            key = certs.join("key.pem").display(),
        ),
    )
    .expect("system.toml");
}

/// Start a node and wait until it answers on its TLS port. `wait_for_tcp` only
/// proves the socket is bound, and the first request needs a backend ready.
fn spawn(name: &str, site: &Path, config: &Path, port: u16) -> Service {
    let mut service = Service::spawn(
        name,
        Command::new(binary("m6-http"))
            .arg(site)
            .arg(config)
            .env("RUST_LOG", "warn"),
    );
    service.wait_for_tcp(port, m6_core::testkit::STARTUP_BOUND);
    service
}

// ── The three public protocols ──────────────────────────────────────────────

fn tls_config(cert_der: &[u8], alpn: &[u8]) -> Arc<rustls::ClientConfig> {
    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .ok();
    let cert = rustls::pki_types::CertificateDer::from(cert_der.to_vec());
    let mut store = rustls::RootCertStore::empty();
    store.add(cert).expect("trust the node's certificate");
    let mut cfg = rustls::ClientConfig::builder()
        .with_root_certificates(store)
        .with_no_client_auth();
    cfg.alpn_protocols = vec![alpn.to_vec()];
    Arc::new(cfg)
}

fn tls_stream(
    node: &Node,
    alpn: &[u8],
) -> rustls::StreamOwned<rustls::ClientConnection, TcpStream> {
    let tcp = TcpStream::connect(("127.0.0.1", node.port)).expect("connect");
    tcp.set_read_timeout(Some(Duration::from_secs(10)))
        .expect("read timeout");
    let name = rustls::pki_types::ServerName::try_from("127.0.0.1".to_string()).expect("name");
    let conn =
        rustls::ClientConnection::new(tls_config(&node.cert_der, alpn), name).expect("tls client");
    rustls::StreamOwned::new(conn, tcp)
}

/// One HTTP/1.1 GET, carrying `via` as the client's own claim.
fn h1_get(node: &Node, path: &str, via: &str) {
    let mut stream = tls_stream(node, b"http/1.1");
    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nVia: {via}\r\nConnection: close\r\n\r\n",
        node.port
    );
    stream.write_all(request.as_bytes()).expect("write");
    stream.flush().expect("flush");
    let mut raw = Vec::new();
    match stream.read_to_end(&mut raw) {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {}
        Err(e) => panic!("read the response: {e}"),
    }
    assert!(
        String::from_utf8_lossy(&raw).contains(" 200 "),
        "the node did not answer 200:\n{}",
        String::from_utf8_lossy(&raw)
    );
}

const H2_PREFACE: &[u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";

fn h2_frame(ftype: u8, flags: u8, stream_id: u32, payload: &[u8]) -> Vec<u8> {
    let n = payload.len();
    let mut v = Vec::with_capacity(9 + n);
    v.push((n >> 16) as u8);
    v.push((n >> 8) as u8);
    v.push(n as u8);
    v.push(ftype);
    v.push(flags);
    v.extend_from_slice(&stream_id.to_be_bytes());
    v.extend_from_slice(payload);
    v
}

/// HPACK "literal header field without indexing" with a new name, value as a
/// raw (un-Huffman'd) string. Short enough that every length is one octet.
fn hpack_new_name(name: &str, value: &str) -> Vec<u8> {
    let mut v = vec![0x00];
    v.push(name.len() as u8);
    v.extend_from_slice(name.as_bytes());
    v.push(value.len() as u8);
    v.extend_from_slice(value.as_bytes());
    v
}

/// One HTTP/2 GET, carrying `via` as the client's own claim, and wait for the
/// response HEADERS so the backend's capture is certainly queued.
fn h2_get(node: &Node, path: &str, via: &str) {
    let mut stream = tls_stream(node, b"h2");
    // :method GET (static 3), :scheme https (static 7), then :authority and
    // :path as literals, then the claim. Pseudo-headers first, as RFC 9113 8.3
    // requires.
    let authority = format!("127.0.0.1:{}", node.port);
    let mut block = vec![0x83, 0x87];
    block.extend(hpack_new_name(":authority", &authority));
    block.extend(hpack_new_name(":path", path));
    block.extend(hpack_new_name("via", via));

    let mut out = Vec::new();
    out.extend_from_slice(H2_PREFACE);
    out.extend(h2_frame(0x04, 0, 0, &[])); // SETTINGS
    out.extend(h2_frame(0x01, 0x05, 1, &block)); // HEADERS, END_STREAM | END_HEADERS
    stream.write_all(&out).expect("write");
    stream.flush().expect("flush");

    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    let deadline = Instant::now() + m6_core::testkit::STARTUP_BOUND;
    while Instant::now() < deadline {
        match stream.read(&mut tmp) {
            Ok(0) => break,
            Ok(n) => buf.extend_from_slice(&tmp[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(e) => panic!("read a frame: {e}"),
        }
        let mut i = 0;
        while i + 9 <= buf.len() {
            let len =
                ((buf[i] as usize) << 16) | ((buf[i + 1] as usize) << 8) | buf[i + 2] as usize;
            if i + 9 + len > buf.len() {
                break;
            }
            if buf[i + 3] == 0x01 {
                return;
            }
            i += 9 + len;
        }
    }
    panic!("no response HEADERS came back");
}

/// One HTTP/3 GET, carrying `via` as the client's own claim, and wait for the
/// response HEADERS.
fn h3_get(node: &Node, path: &str, via: &str) {
    let server_addr: std::net::SocketAddr =
        format!("127.0.0.1:{}", node.port).parse().expect("addr");
    let udp = UdpSocket::bind("127.0.0.1:0").expect("udp bind");
    udp.set_nonblocking(true).expect("non-blocking");
    let local = udp.local_addr().expect("local addr");

    let mut config = quiche::Config::new(quiche::PROTOCOL_VERSION).expect("quiche config");
    config
        .set_application_protos(quiche::h3::APPLICATION_PROTOCOL)
        .expect("alpn");
    config.set_max_idle_timeout(10_000);
    config.set_initial_max_data(10_000_000);
    config.set_initial_max_stream_data_bidi_local(1_000_000);
    config.set_initial_max_stream_data_bidi_remote(1_000_000);
    config.set_initial_max_stream_data_uni(1_000_000);
    config.set_initial_max_streams_bidi(100);
    config.set_initial_max_streams_uni(10);
    config.grease(false);
    config.verify_peer(false);

    let scid = quiche::ConnectionId::from_ref(&[7u8; quiche::MAX_CONN_ID_LEN]);
    let mut conn = quiche::connect(Some("localhost"), &scid, local, server_addr, &mut config)
        .expect("quic connect");
    let mut h3: Option<quiche::h3::Connection> = None;
    let mut buf = vec![0u8; 65536];
    let mut out = vec![0u8; 1350];
    let mut sent = false;
    let deadline = Instant::now() + m6_core::testkit::STARTUP_BOUND;

    while Instant::now() < deadline {
        conn.on_timeout();
        while let Ok((n, info)) = conn.send(&mut out) {
            let _ = udp.send_to(&out[..n], info.to);
        }
        loop {
            match udp.recv_from(&mut buf) {
                Ok((n, from)) => {
                    let info = quiche::RecvInfo { from, to: local };
                    if conn.recv(&mut buf[..n], info).is_err() {
                        break;
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) => panic!("udp recv: {e}"),
            }
        }
        if conn.is_closed() {
            panic!("the QUIC connection closed before a response");
        }
        if conn.is_established() && h3.is_none() {
            let cfg = quiche::h3::Config::new().expect("h3 config");
            h3 = Some(
                quiche::h3::Connection::with_transport(&mut conn, &cfg).expect("h3 connection"),
            );
        }
        if let Some(h3c) = h3.as_mut() {
            if !sent {
                let authority = format!("127.0.0.1:{}", node.port);
                let headers = vec![
                    quiche::h3::Header::new(b":method", b"GET"),
                    quiche::h3::Header::new(b":path", path.as_bytes()),
                    quiche::h3::Header::new(b":scheme", b"https"),
                    quiche::h3::Header::new(b":authority", authority.as_bytes()),
                    quiche::h3::Header::new(b"via", via.as_bytes()),
                ];
                match h3c.send_request(&mut conn, &headers, true) {
                    Ok(_) => sent = true,
                    Err(quiche::h3::Error::Done) => {}
                    Err(e) => panic!("h3 send_request: {e}"),
                }
            }
            match h3c.poll(&mut conn) {
                Ok((_, quiche::h3::Event::Headers { .. })) => return,
                Ok(_) => {}
                Err(quiche::h3::Error::Done) => {}
                Err(e) => panic!("h3 poll: {e}"),
            }
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    panic!("h3 response timed out");
}

/// Start one public node whose single route reaches the capture backend.
///
/// No h2c listener, so every request arrives on a listener that must not
/// believe a `Via`.
fn public_node(root: &Path, capture: &Capture) -> Node {
    let dir = tempfile::tempdir_in(root).expect("tempdir");
    let site = dir.path().join("site");
    write_site(
        &site,
        &format!(
            "[[backend]]\nname = \"capture\"\nsockets = \"{}\"\n\n\
             [[route]]\npath = \"/capture/{{*rest}}\"\nbackend = \"capture\"\n",
            capture.glob()
        ),
    );
    let (cert_pem, key_pem, cert_der) = cert();
    std::fs::write(dir.path().join("cert.pem"), &cert_pem).expect("cert");
    std::fs::write(dir.path().join("key.pem"), &key_pem).expect("key");
    let port = claim_port();
    let config = dir.path().join("system.toml");
    write_system(&config, &port.addr(), "edge-a", "", dir.path());
    let service = spawn("m6-http-via-public", &site, &config, port.port());
    Node {
        port: port.port(),
        cert_der,
        _service: service,
        _ports: vec![port],
        _dir: dir,
    }
}

// ── The h2c backbone ────────────────────────────────────────────────────────

/// A node with an h2c listener named in `internal_binds`, so a request from an
/// `internal_peers` address is trusted, or one declared public with
/// `internal_binds = []`, so it is not.
fn h2c_node(root: &Path, capture: &Capture, internal: bool) -> (Node, u16) {
    let dir = tempfile::tempdir_in(root).expect("tempdir");
    let site = dir.path().join("site");
    write_site(
        &site,
        &format!(
            "[[backend]]\nname = \"capture\"\nsockets = \"{}\"\n\n\
             [[route]]\npath = \"/capture/{{*rest}}\"\nbackend = \"capture\"\n",
            capture.glob()
        ),
    );
    let (cert_pem, key_pem, der) = cert();
    std::fs::write(dir.path().join("cert.pem"), &cert_pem).expect("cert");
    std::fs::write(dir.path().join("key.pem"), &key_pem).expect("key");
    let tls = claim_port();
    let h2c = claim_port();
    let listener = if internal {
        format!(
            "h2c_bind = \"{}\"\ninternal_binds = [\"{}\"]\ninternal_peers = [\"127.0.0.1\"]\n",
            h2c.addr(),
            h2c.addr()
        )
    } else {
        format!("h2c_bind = \"{}\"\ninternal_binds = []\n", h2c.addr())
    };
    let config = dir.path().join("system.toml");
    write_system(&config, &tls.addr(), "origin", &listener, dir.path());
    let mut service = spawn("m6-http-via-h2c", &site, &config, tls.port());
    // `spawn` waits for the TLS port only. The h2c listener binds separately, so
    // the test also waits for it before `h2c_send` connects.
    service.wait_for_tcp(h2c.port(), m6_core::testkit::STARTUP_BOUND);
    let h2c_port = h2c.port();
    (
        Node {
            port: tls.port(),
            cert_der: der,
            _service: service,
            _ports: vec![tls, h2c],
            _dir: dir,
        },
        h2c_port,
    )
}

/// Dispatch one request over h2c, as a cache node would, and wait for its
/// response so the backend's capture is queued.
fn h2c_send(port: u16, path: &str, via: &str, host: &str) {
    let mut conn = H2cClientConn::connect("127.0.0.1", port).expect("connect over h2c");
    let settle = Instant::now() + Duration::from_millis(250);
    while Instant::now() < settle {
        conn.drive();
        std::thread::sleep(Duration::from_millis(1));
    }
    let req = HttpRequest {
        method: "GET".to_string(),
        path: path.to_string(),
        query: None,
        // The message this client received arrived over HTTP/2, so a Via entry
        // this client adds names 2. This client adds none: it is a plain peer,
        // and `via` is its own claim.
        version: "HTTP/2".to_string(),
        headers: vec![("via".to_string(), via.to_string())],
        body: Vec::new(),
    };
    let stream_id = conn
        .dispatch(&req, "127.0.0.1", "203.0.113.9", host, None)
        .expect("dispatch over h2c");
    let deadline = Instant::now() + m6_core::testkit::STARTUP_BOUND;
    loop {
        conn.drive();
        if let Some(done) = conn
            .drain_completions()
            .into_iter()
            .find(|c| c.stream_id == stream_id)
        {
            // The backend can only have seen the request if m6-http forwarded
            // it, so anything but a 200 is the failure, named here.
            match done.result {
                Ok(resp) if resp.status == 200 => return,
                Ok(resp) => panic!(
                    "h2c answered {} without reaching the backend: {}",
                    resp.status,
                    String::from_utf8_lossy(&resp.body)
                ),
                Err(e) => panic!("h2c request failed: {e}"),
            }
        }
        assert!(Instant::now() < deadline, "no response over h2c");
        std::thread::sleep(Duration::from_millis(1));
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────

/// A client that sends `Via: 1.1 edge-b` to a public listener must not have it
/// reach the backend. If it did, any visitor could name a node and the
/// per-node analytics this header exists for would be poisonable.
#[test]
fn h1_public_listener_strips_a_client_via() {
    let root = m6_core::testkit::socket_scratch_root();
    let capture = Capture::start(&root);
    let node = public_node(&root, &capture);
    h1_get(&node, "/capture/h1", "1.1 edge-b");
    assert_eq!(
        via(&capture.head()),
        "1.1 edge-a",
        "a client's Via survived the public HTTP/1.1 listener"
    );
    drop(node);
}

/// The same on HTTP/2, where the entry also reports the protocol the request
/// arrived on.
#[test]
fn h2_public_listener_strips_a_client_via() {
    let root = m6_core::testkit::socket_scratch_root();
    let capture = Capture::start(&root);
    let node = public_node(&root, &capture);
    h2_get(&node, "/capture/h2", "1.1 edge-b");
    assert_eq!(
        via(&capture.head()),
        "2 edge-a",
        "a client's Via survived the public HTTP/2 listener"
    );
    drop(node);
}

/// The same on HTTP/3.
#[test]
fn h3_public_listener_strips_a_client_via() {
    let root = m6_core::testkit::socket_scratch_root();
    let capture = Capture::start(&root);
    let node = public_node(&root, &capture);
    h3_get(&node, "/capture/h3", "1.1 edge-b");
    assert_eq!(
        via(&capture.head()),
        "3 edge-a",
        "a client's Via survived the public HTTP/3 listener"
    );
    drop(node);
}

/// The whole chain: a client's claim is dropped by the cache, the cache's
/// entry crosses the backbone, the origin keeps it and appends its own, and
/// the backend reads both in path order. The site reads the first entry as the
/// edge that took the request, so a reversed list would be silently wrong.
#[test]
fn cache_to_origin_carries_both_names_in_path_order() {
    let root = m6_core::testkit::socket_scratch_root();
    let capture = Capture::start(&root);

    // The origin: a public listener and an internal h2c backbone.
    let origin_dir = tempfile::tempdir_in(&root).expect("tempdir");
    let origin_site = origin_dir.path().join("site");
    write_site(
        &origin_site,
        &format!(
            "[[backend]]\nname = \"capture\"\nsockets = \"{}\"\n\n\
             [[route]]\npath = \"/capture/{{*rest}}\"\nbackend = \"capture\"\n",
            capture.glob()
        ),
    );
    let (cert_pem, key_pem, origin_der) = cert();
    std::fs::write(origin_dir.path().join("cert.pem"), &cert_pem).expect("cert");
    std::fs::write(origin_dir.path().join("key.pem"), &key_pem).expect("key");
    let origin_tls = claim_port();
    let origin_h2c = claim_port();
    let origin_h2c_port = origin_h2c.port();
    let origin_config = origin_dir.path().join("system.toml");
    write_system(
        &origin_config,
        &origin_tls.addr(),
        "origin",
        &format!(
            "h2c_bind = \"{}\"\ninternal_binds = [\"{}\"]\ninternal_peers = [\"127.0.0.1\"]\n",
            origin_h2c.addr(),
            origin_h2c.addr()
        ),
        origin_dir.path(),
    );
    let origin = spawn(
        "m6-http-via-origin",
        &origin_site,
        &origin_config,
        origin_tls.port(),
    );
    let origin = Node {
        port: origin_tls.port(),
        cert_der: origin_der,
        _service: origin,
        _ports: vec![origin_tls, origin_h2c],
        _dir: origin_dir,
    };

    // The cache node: everything it serves goes to the origin over h2c.
    let cache_dir = tempfile::tempdir_in(&root).expect("tempdir");
    let cache_site = cache_dir.path().join("site");
    write_site(
        &cache_site,
        &format!(
            "[[backend]]\nname = \"origin\"\nurl = \"h2c://127.0.0.1:{}\"\n\n\
             [[route]]\npath = \"/capture/{{*rest}}\"\nbackend = \"origin\"\n",
            origin_h2c_port
        ),
    );
    let (cache_cert, cache_key, cache_der) = cert();
    std::fs::write(cache_dir.path().join("cert.pem"), &cache_cert).expect("cert");
    std::fs::write(cache_dir.path().join("key.pem"), &cache_key).expect("key");
    let cache_tls = claim_port();
    let cache_config = cache_dir.path().join("system.toml");
    write_system(
        &cache_config,
        &cache_tls.addr(),
        "edge-a",
        "",
        cache_dir.path(),
    );
    let cache = spawn(
        "m6-http-via-cache",
        &cache_site,
        &cache_config,
        cache_tls.port(),
    );
    let cache = Node {
        port: cache_tls.port(),
        cert_der: cache_der,
        _service: cache,
        _ports: vec![cache_tls],
        _dir: cache_dir,
    };

    h1_get(&cache, "/capture/two-hop", "1.1 edge-b");

    // The cache received the request over HTTP/1.1, so its entry says 1.1.
    // The origin received it over HTTP/2 on the backbone, so its entry says 2.
    // The client's claim is in neither.
    assert_eq!(
        via(&capture.head()),
        "1.1 edge-a, 2 origin",
        "the backend did not read the cache's name then the origin's"
    );
    drop(cache);
    drop(origin);
}

/// A `Via` from an `internal_peers` address on the internal h2c listener is
/// kept and this node's entry appended. This is the entry the whole change
/// exists for, so it must survive the hop that receives it.
#[test]
fn an_internal_listener_keeps_and_appends_a_peers_via() {
    let root = m6_core::testkit::socket_scratch_root();
    let capture = Capture::start(&root);
    let (node, h2c) = h2c_node(&root, &capture, true);
    h2c_send(h2c, "/capture/backbone", "1.1 edge-b", "localhost");
    assert_eq!(
        via(&capture.head()),
        "1.1 edge-b, 2 origin",
        "a cache node's Via was dropped or reordered on the backbone"
    );
    drop(node);
}

/// The same listener declared public (`internal_binds = []`) trusts nobody, so
/// the same request's `Via` is dropped. The trust is the listener's, granted
/// at accept, and this is what proves it.
#[test]
fn an_h2c_listener_declared_public_strips_a_via() {
    let root = m6_core::testkit::socket_scratch_root();
    let capture = Capture::start(&root);
    let (node, h2c) = h2c_node(&root, &capture, false);
    h2c_send(h2c, "/capture/public-h2c", "1.1 edge-b", "localhost");
    assert_eq!(
        via(&capture.head()),
        "2 origin",
        "a public h2c listener believed a client's Via"
    );
    drop(node);
}
