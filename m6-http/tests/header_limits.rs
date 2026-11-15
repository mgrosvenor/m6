//! One request-header limit, enforced at the edge.
//!
//! The two numbers are the ones the mainstream servers use: nginx's
//! `large_client_header_buffers 4 8k`, Varnish's `http_req_hdr_len 8k` and
//! `http_req_size 32k`, and Apache's `LimitRequestFieldSize 8190`. They are
//! written here as literals rather than imported from `m6-core`, so this file
//! is a black-box statement of the contract and it compiles against the base
//! commit that has no constants yet.
//!
//! Half the tests feed a real `m6-http` with a request whose request line, one
//! field or whole head is exactly at a limit, and half feed it one byte more.
//! At the limit the request must be served and the backend must receive it.
//! One byte over the request line must be answered `414 Request-URI Too
//! Large`, and one byte over a field or the head must be answered `400
//! Request Header Or Cookie Too Large`, both from `m6-http` with the backend
//! socket left untouched. "Untouched" is asserted on the kernel accept queue,
//! not on a counter a scheduling delay could hide: the socket is bound with no
//! acceptor, and after the response `accept()` must return `WouldBlock`.
//!
//! The last three tests drive a real `m6-file` backend directly, with no
//! `m6-http` in front, because the backend parser carries the same two limits
//! and the same two answers.

#![allow(
    clippy::disallowed_methods,
    reason = "test code: threads, sleeps and blocking clients drive the service from outside"
)]

use std::io::{Read, Write};
use std::net::{TcpStream, UdpSocket};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rustls::StreamOwned;

use quiche::h3::NameValue as _;

use m6_core::testkit::{binary, claim_port, PortClaim, Service};
use m6_http_lib::forward::HttpRequest;
use m6_http_lib::h2s_client::H2sTlsClientConn;
use m6_http_lib::probe;

/// One field line: name plus value, in bytes.
const HEADER_LINE_LIMIT: usize = 8 * 1024;
/// One request head: request line plus every field, in bytes.
const HEAD_LIMIT: usize = 32 * 1024;
/// The page title nginx gives a request whose head is over a limit.
const HEAD_TITLE: &str = "Request Header Or Cookie Too Large";
/// The status line and page title nginx gives a request line over the limit.
const URI_TITLE: &str = "Request-URI Too Large";

// ── Responses ────────────────────────────────────────────────────────────────

struct Reply {
    status: u16,
    body: Vec<u8>,
}

impl Reply {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    /// The `<title>` element, so a test asserts the phrase the page carries
    /// rather than any occurrence of it anywhere in the bytes.
    fn title(&self) -> String {
        let text = self.text();
        let Some(start) = text.find("<title>") else {
            return String::new();
        };
        let rest = &text[start + "<title>".len()..];
        match rest.find("</title>") {
            Some(end) => rest[..end].to_string(),
            None => String::new(),
        }
    }
}

/// Parse the first complete, non-informational response in `raw`.
fn parse_reply(raw: &[u8]) -> Reply {
    let mut cursor = 0usize;
    loop {
        let Some(off) = raw[cursor..].windows(4).position(|w| w == b"\r\n\r\n") else {
            return Reply {
                status: 0,
                body: Vec::new(),
            };
        };
        let end = cursor + off;
        let block = String::from_utf8_lossy(&raw[cursor..end]);
        let status: u16 = block
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .and_then(|code| code.parse().ok())
            .unwrap_or(0);
        let body_at = end + 4;
        if (100..200).contains(&status) {
            cursor = body_at;
            continue;
        }
        let length: usize = block
            .lines()
            .skip(1)
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse().ok())
                    .flatten()
            })
            .unwrap_or(0);
        let body_end = (body_at + length).min(raw.len());
        return Reply {
            status,
            body: raw[body_at..body_end].to_vec(),
        };
    }
}

/// Read one response body without waiting for the peer to close.
fn read_reply<R: Read>(stream: &mut R) -> Reply {
    let mut raw = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        // Stop as soon as a complete response with its body is in hand.
        let complete = {
            let reply = parse_reply(&raw);
            reply.status != 0 && raw.len() >= body_end(&raw)
        };
        if complete {
            return parse_reply(&raw);
        }
        match stream.read(&mut buf) {
            Ok(0) => return parse_reply(&raw),
            Ok(n) => raw.extend_from_slice(&buf[..n]),
            Err(_) => return parse_reply(&raw),
        }
    }
}

/// Where the body of the first complete non-informational response ends.
fn body_end(raw: &[u8]) -> usize {
    let mut cursor = 0usize;
    loop {
        let Some(off) = raw[cursor..].windows(4).position(|w| w == b"\r\n\r\n") else {
            return usize::MAX;
        };
        let end = cursor + off;
        let block = String::from_utf8_lossy(&raw[cursor..end]);
        let status: u16 = block
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .and_then(|code| code.parse().ok())
            .unwrap_or(0);
        let body_at = end + 4;
        if (100..200).contains(&status) {
            cursor = body_at;
            continue;
        }
        let length: usize = block
            .lines()
            .skip(1)
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse().ok())
                    .flatten()
            })
            .unwrap_or(0);
        return body_at + length;
    }
}

// ── TLS clients ──────────────────────────────────────────────────────────────

fn generate_cert() -> (String, String, Vec<u8>) {
    let cert =
        rcgen::generate_simple_self_signed(vec!["localhost".to_string(), "127.0.0.1".to_string()])
            .expect("self-signed cert");
    (
        cert.cert.pem(),
        cert.key_pair.serialize_pem(),
        cert.cert.der().to_vec(),
    )
}

fn trusting_client(cert_der: &[u8]) -> Arc<rustls::ClientConfig> {
    let mut roots = rustls::RootCertStore::empty();
    roots
        .add(rustls::pki_types::CertificateDer::from(cert_der.to_vec()))
        .expect("trust the run's own cert");
    Arc::new(
        rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth(),
    )
}

/// One raw HTTP/1.1 request over TLS. The request bytes are sent exactly as
/// given, because the point is what a client can put on the wire.
fn https_send(port: u16, tls: &Arc<rustls::ClientConfig>, request: &[u8]) -> Reply {
    let tcp = match TcpStream::connect(format!("127.0.0.1:{port}")) {
        Ok(t) => t,
        Err(_) => {
            return Reply {
                status: 0,
                body: Vec::new(),
            }
        }
    };
    tcp.set_read_timeout(Some(Duration::from_secs(10))).ok();
    let name = rustls::pki_types::ServerName::try_from("127.0.0.1".to_string()).unwrap();
    let conn = match rustls::ClientConnection::new(Arc::clone(tls), name) {
        Ok(c) => c,
        Err(_) => {
            return Reply {
                status: 0,
                body: Vec::new(),
            }
        }
    };
    let mut stream = StreamOwned::new(conn, tcp);
    if stream.write_all(request).is_err() || stream.flush().is_err() {
        return Reply {
            status: 0,
            body: Vec::new(),
        };
    }
    read_reply(&mut stream)
}

/// One HTTP/2 request over TLS, driven until its stream completes or the
/// deadline passes. A short poll step is a signal wait with a deadline, as the
/// loop primer allows.
fn h2_send(
    port: u16,
    path: &str,
    headers: Vec<(String, String)>,
    timeout: Duration,
) -> Result<Reply, String> {
    probe::install_crypto();
    let cfg = probe::tls_config(b"h2");
    let mut conn = H2sTlsClientConn::connect("127.0.0.1", port, cfg).map_err(|e| e.to_string())?;
    let req = HttpRequest {
        method: "GET".to_string(),
        path: path.to_string(),
        query: None,
        version: "HTTP/2".to_string(),
        headers,
        body: Vec::new(),
    };
    let stream_id = conn
        .dispatch(&req, "127.0.0.1", "127.0.0.1", "127.0.0.1")
        .map_err(|e| e.to_string())?;
    let deadline = Instant::now() + timeout;
    loop {
        conn.drive();
        if let Some(done) = conn
            .drain_completions()
            .into_iter()
            .find(|c| c.stream_id == stream_id)
        {
            return match done.result {
                Ok(r) => Ok(Reply {
                    status: r.status,
                    body: r.body,
                }),
                Err(e) => Err(format!("h2 response: {e}")),
            };
        }
        if Instant::now() >= deadline {
            return Err(format!("h2 timeout on stream {stream_id}"));
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

// ── HTTP/3 client ────────────────────────────────────────────────────────────

fn quic_flush(conn: &mut quiche::Connection, udp: &UdpSocket, out: &mut [u8]) {
    while let Ok((n, info)) = conn.send(out) {
        let _ = udp.send_to(&out[..n], info.to);
    }
}

/// One HTTP/3 request. Header names and values are byte slices so a test can
/// send a field of exactly the size it needs.
fn h3_send(
    port: u16,
    method: &[u8],
    path: &[u8],
    extra: &[(&[u8], &[u8])],
    timeout: Duration,
) -> Result<Reply, String> {
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
    // A header block is sent atomically and quiche's default initial
    // congestion window is about ten packets, so a one-shot client needs a
    // wider window to put a 32 KiB field section in one HEADERS frame.
    config.set_initial_congestion_window_packets(100);

    let scid_bytes = [9u8; quiche::MAX_CONN_ID_LEN];
    let scid = quiche::ConnectionId::from_ref(&scid_bytes);
    let mut conn = quiche::connect(Some("localhost"), &scid, local, server_addr, &mut config)
        .map_err(|e| format!("h3 connect: {e}"))?;

    let mut h3: Option<quiche::h3::Connection> = None;
    let mut buf = vec![0u8; 65536];
    let mut out = vec![0u8; 1350];
    let mut sent = false;
    let mut status: Option<u16> = None;
    let mut body: Vec<u8> = Vec::new();
    let mut done = false;
    let deadline = Instant::now() + timeout;

    loop {
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
                Err(e) => return Err(format!("h3 recv: {e}")),
            }
        }

        if conn.is_closed() {
            return Err("h3 connection closed before a response".to_string());
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
                let mut headers = vec![
                    quiche::h3::Header::new(b":method", method),
                    quiche::h3::Header::new(b":path", path),
                    quiche::h3::Header::new(b":scheme", b"https"),
                    quiche::h3::Header::new(b":authority", b"localhost"),
                ];
                for (name, value) in extra {
                    headers.push(quiche::h3::Header::new(name, value));
                }
                match h3c.send_request(&mut conn, &headers, true) {
                    Ok(_) => sent = true,
                    // The stream had no capacity for the header block yet.
                    // quiche rolls the stream id back, so the next turn retries
                    // the same request once the connection has drained.
                    Err(quiche::h3::Error::StreamBlocked) => {}
                    Err(e) => return Err(format!("h3 send_request: {e}")),
                }
            }

            loop {
                match h3c.poll(&mut conn) {
                    Ok((_, quiche::h3::Event::Headers { list, .. })) => {
                        for h in &list {
                            if h.name() == b":status" {
                                status = std::str::from_utf8(h.value())
                                    .ok()
                                    .and_then(|v| v.parse().ok());
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
                    Err(e) => return Err(format!("h3 poll: {e}")),
                }
            }
        }

        quic_flush(&mut conn, &udp, &mut out);
        if done {
            if let Some(status) = status {
                return Ok(Reply { status, body });
            }
        }
        if Instant::now() >= deadline {
            return Err(format!("h3 timeout: sent={sent} status={status:?}"));
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

// ── The running edge and its two backends ────────────────────────────────────

/// Bind `path` to a backend that reads every head in full and answers
/// `200 ok`, so a request that reaches it is served rather than truncated.
fn start_served_backend(path: &Path) {
    let _ = std::fs::remove_file(path);
    let listener = UnixListener::bind(path).expect("bind served backend");
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            std::thread::spawn(|| serve_one(stream));
        }
    });
}

fn serve_one(mut stream: UnixStream) {
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
    let mut head = Vec::new();
    let mut buf = [0u8; 8192];
    while !head.windows(4).any(|w| w == b"\r\n\r\n") {
        match stream.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => head.extend_from_slice(&buf[..n]),
        }
        if head.len() > 128 * 1024 {
            break;
        }
    }
    let _ = stream.write_all(
        b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
    );
    let _ = stream.flush();
}

/// A bound backend socket with no acceptor at all.
///
/// The edge must never connect to it for an over-limit request. A connect is
/// visible in the kernel's accept queue whether or not a thread reads it, so
/// the check is a non-blocking `accept()` after the response: `Ok` means the
/// edge contacted the backend and `WouldBlock` means it did not.
struct NeverBackend {
    listener: UnixListener,
}

impl NeverBackend {
    fn start(path: &Path) -> NeverBackend {
        let _ = std::fs::remove_file(path);
        let listener = UnixListener::bind(path).expect("bind never backend");
        listener.set_nonblocking(true).expect("non-blocking accept");
        NeverBackend { listener }
    }

    fn saw_connection(&self) -> bool {
        match self.listener.accept() {
            Ok(_) => true,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => false,
            Err(_) => false,
        }
    }
}

struct Stack {
    port: u16,
    tls: Arc<rustls::ClientConfig>,
    never: NeverBackend,
    _edge: Service,
    _claim: PortClaim,
    _tmp: tempfile::TempDir,
}

impl Stack {
    fn start() -> Stack {
        rustls::crypto::ring::default_provider()
            .install_default()
            .ok();

        let tmp = tempfile::Builder::new().prefix("m6hl-").tempdir().unwrap();
        let base = tmp.path();

        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        // A short name under /tmp: `sockaddr_un` caps at 104 bytes on macOS.
        let prefix = format!("/tmp/m6hl-{}-{n}", std::process::id());
        let served_path = PathBuf::from(format!("{prefix}-served.sock"));
        let never_path = PathBuf::from(format!("{prefix}-never.sock"));
        start_served_backend(&served_path);
        let never = NeverBackend::start(&never_path);

        let (cert_pem, key_pem, cert_der) = generate_cert();
        std::fs::write(base.join("cert.pem"), &cert_pem).unwrap();
        std::fs::write(base.join("key.pem"), &key_pem).unwrap();

        let claim = claim_port();
        let port = claim.port();

        let site = base.join("site");
        std::fs::create_dir_all(&site).unwrap();
        std::fs::write(
            site.join("site.toml"),
            format!(
                r#"
[site]
name   = "header-limits"
domain = "localhost"

[log]
level  = "warn"
format = "text"

[errors]
mode = "internal"

[analytics]
enabled = false

[rate_limit]
enabled = false

[[backend]]
name    = "served"
sockets = "{served}"

[[backend]]
name    = "never"
sockets = "{never}"

[[route]]
path    = "/never"
backend = "never"

[[route]]
path    = "{over_line_http1}"
backend = "never"

[[route]]
path    = "{over_line_http2}"
backend = "never"

[[route]]
path    = "/{{*rest}}"
backend = "served"
"#,
                served = served_path.display(),
                never = never_path.display(),
                over_line_http1 = over_limit_http1_request_line_path(),
                over_line_http2 = over_limit_http2_request_line_path(),
            ),
        )
        .unwrap();

        let system = base.join("system.toml");
        std::fs::write(
            &system,
            format!(
                "[server]\nbind = \"127.0.0.1:{port}\"\ntls_cert = \"{}\"\ntls_key = \"{}\"\n\n[node]\nname = \"test-node\"\n",
                base.join("cert.pem").display(),
                base.join("key.pem").display(),
            ),
        )
        .unwrap();

        let edge = Service::spawn(
            "m6-http",
            Command::new(binary("m6-http")).args([
                site.to_str().unwrap(),
                system.to_str().unwrap(),
                "--log-level",
                "warn",
            ]),
        );

        let tls = trusting_client(&cert_der);
        let ready = m6_core::testkit::wait::until(Duration::from_secs(30), || {
            https_send(
                port,
                &tls,
                b"GET /ok HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n",
            )
            .status
                == 200
        });
        assert!(
            ready,
            "m6-http never served /ok with 200; edge output:\n{}",
            edge.output()
        );

        Stack {
            port,
            tls,
            never,
            _edge: edge,
            _claim: claim,
            _tmp: tmp,
        }
    }

    fn h1(&self, request: &[u8]) -> Reply {
        https_send(self.port, &self.tls, request)
    }

    /// The over-limit response and the untouched backend, asserted together so
    /// no test can forget the half that matters.
    fn edge_output(&self) -> String {
        self._edge.output()
    }

    fn assert_refused(&self, reply: &Reply, status: u16, title: &str) {
        assert_eq!(
            reply.status,
            status,
            "expected {status}; body was {:?}",
            reply.text()
        );
        assert_eq!(
            reply.title(),
            format!("{status} {title}"),
            "the error page must carry nginx's title"
        );
        assert!(
            !self.never.saw_connection(),
            "m6-http must answer before it contacts a backend"
        );
    }
}

// ── Head construction ────────────────────────────────────────────────────────

/// The HTTP/2 and HTTP/3 path whose request line is one byte over the
/// header-line limit: `:method` plus one space plus `:path`.
fn over_limit_http2_request_line_path() -> String {
    format!(
        "/never/{}",
        "a".repeat(HEADER_LINE_LIMIT + 1 - "GET ".len() - "/never/".len())
    )
}

/// The HTTP/1.1 path whose request line is one byte over the limit: method,
/// target and version with their spaces.
fn over_limit_http1_request_line_path() -> String {
    format!(
        "/never/{}",
        "a".repeat(HEADER_LINE_LIMIT + 1 - "GET ".len() - " HTTP/1.1".len() - "/never/".len())
    )
}

/// A request line of exactly `len` bytes whose target is `prefix` followed by
/// padding. `len` counts method, target and version with one space between
/// each, which is the line the client puts on the wire.
fn request_line(prefix: &str, len: usize) -> String {
    let target_len = len - "GET ".len() - " HTTP/1.1".len();
    assert!(target_len > prefix.len(), "prefix leaves no room");
    let mut target = prefix.to_string();
    target.push_str(&"a".repeat(target_len - prefix.len()));
    format!("GET {target} HTTP/1.1")
}

/// An HTTP/1.1 request head of exactly `total` bytes through the closing blank
/// line, with a short request line and every field inside the field limit.
fn h1_head_of_size(total: usize, path: &str) -> Vec<u8> {
    let mut head = format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n");
    let mut n = 0usize;
    loop {
        let line_cost = format!("x-pad{n}: ").len() + 2;
        let remaining = total.saturating_sub(head.len() + 2);
        if remaining == 0 {
            break;
        }
        assert!(
            remaining > line_cost,
            "cannot pad a {total}-byte head exactly, {remaining} bytes left"
        );
        let value_len = (remaining - line_cost).min(8_000);
        assert!(line_cost + value_len <= HEADER_LINE_LIMIT);
        head.push_str(&format!("x-pad{n}: "));
        head.push_str(&"a".repeat(value_len));
        head.push_str("\r\n");
        n += 1;
    }
    assert_eq!(head.len() + 2, total);
    head.push_str("\r\n");
    head.into_bytes()
}

/// One HTTP/1.1 field whose name plus value is exactly `field_len` bytes.
fn h1_field_of_size(field_len: usize) -> String {
    format!("x-pad: {}\r\n", "a".repeat(field_len - "x-pad".len()))
}

/// The RFC 9113 6.5.2 size of a decoded field list: name plus value plus 32
/// bytes per field.
fn field_list_size(fields: &[(Vec<u8>, Vec<u8>)]) -> usize {
    fields
        .iter()
        .map(|(name, value)| name.len() + value.len() + 32)
        .sum()
}

/// Pad a decoded field list to exactly `target` bytes of RFC 9113 6.5.2 size.
///
/// The padding is many small identical fields, not a few large ones. Small
/// repeated fields compress to one byte each in HPACK, so the encoded block of
/// a 32 KiB decoded list still fits one HEADERS frame. A few 8 KiB literals
/// would exceed the peer's 16 KiB frame size and be refused as a framing error
/// before either limit was reached.
fn pad_fields(
    mut fields: Vec<(Vec<u8>, Vec<u8>)>,
    target: usize,
    tag: &str,
) -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut n = 0usize;
    loop {
        let used = field_list_size(&fields);
        if used == target {
            return fields;
        }
        assert!(
            used < target,
            "the base fields ({used}) are already over {target}"
        );
        let remaining = target - used;
        // name 5 + value 290 + 32 is 327.
        if remaining >= 327 {
            fields.push((b"x-pad".to_vec(), vec![b'a'; 290]));
            continue;
        }
        let name = format!("x-tail{tag}{n}");
        assert!(
            remaining >= name.len() + 32,
            "cannot pad exactly: {remaining} bytes left"
        );
        let value_len = remaining - name.len() - 32;
        fields.push((name.into_bytes(), vec![b'a'; value_len]));
        n += 1;
    }
}

/// The decoded list an HTTP/2 client sends for `GET path` before any extra
/// fields: `H2sTlsClientConn` writes these seven itself.
fn h2_base_fields(path: &str) -> Vec<(Vec<u8>, Vec<u8>)> {
    let owned = |name: &str, value: &str| (name.as_bytes().to_vec(), value.as_bytes().to_vec());
    vec![
        owned(":method", "GET"),
        owned(":path", path),
        owned(":scheme", "https"),
        owned(":authority", "127.0.0.1"),
        owned("x-forwarded-for", "127.0.0.1"),
        owned("x-forwarded-proto", "https"),
        owned("x-forwarded-host", "127.0.0.1"),
    ]
}

/// The decoded list the HTTP/3 client sends for `GET path`.
fn h3_base_fields(path: &str) -> Vec<(Vec<u8>, Vec<u8>)> {
    let owned = |name: &str, value: &str| (name.as_bytes().to_vec(), value.as_bytes().to_vec());
    vec![
        owned(":method", "GET"),
        owned(":path", path),
        owned(":scheme", "https"),
        owned(":authority", "localhost"),
    ]
}

fn extra_headers(fields: &[(Vec<u8>, Vec<u8>)]) -> Vec<(String, String)> {
    fields
        .iter()
        .map(|(name, value)| {
            (
                String::from_utf8(name.clone()).expect("utf-8 field name"),
                String::from_utf8(value.clone()).expect("utf-8 field value"),
            )
        })
        .collect()
}

// ── HTTP/1.1 ────────────────────────────────────────────────────────────────

#[test]
fn h1_serves_a_field_at_the_header_line_limit() {
    let s = Stack::start();
    let request = format!(
        "GET /ok HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n{}\r\n",
        h1_field_of_size(HEADER_LINE_LIMIT)
    );
    let reply = s.h1(request.as_bytes());
    assert_eq!(reply.status, 200, "body was {:?}", reply.text());
}

#[test]
fn h1_refuses_a_field_one_byte_over_the_header_line_limit() {
    let s = Stack::start();
    let request = format!(
        "GET /never HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n{}\r\n",
        h1_field_of_size(HEADER_LINE_LIMIT + 1)
    );
    let reply = s.h1(request.as_bytes());
    s.assert_refused(&reply, 400, HEAD_TITLE);
}

#[test]
fn h1_serves_a_request_line_at_the_header_line_limit() {
    let s = Stack::start();
    let head = format!(
        "{}\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n",
        request_line("/", HEADER_LINE_LIMIT)
    );
    let reply = s.h1(head.as_bytes());
    assert_eq!(reply.status, 200, "body was {:?}", reply.text());
}

#[test]
fn h1_refuses_a_request_line_one_byte_over_the_header_line_limit() {
    let s = Stack::start();
    let head = format!(
        "{}\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n",
        request_line("/never/", HEADER_LINE_LIMIT + 1)
    );
    let target = over_limit_http1_request_line_path();
    assert!(
        head.contains(&target),
        "the test's request line must be the path the stack routes to `never`"
    );
    let reply = s.h1(head.as_bytes());
    s.assert_refused(&reply, 414, URI_TITLE);
}

#[test]
fn h1_serves_a_request_line_at_the_limit_after_leading_empty_lines() {
    let s = Stack::start();
    let head = format!(
        "\r\n\r\n{}\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n",
        request_line("/", HEADER_LINE_LIMIT)
    );
    let reply = s.h1(head.as_bytes());
    assert_eq!(reply.status, 200, "body was {:?}", reply.text());
}

#[test]
fn h1_refuses_a_request_line_over_the_limit_after_leading_empty_lines() {
    let s = Stack::start();
    let head = format!(
        "\r\n\r\n{}\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n",
        request_line("/never/", HEADER_LINE_LIMIT + 1)
    );
    let reply = s.h1(head.as_bytes());
    s.assert_refused(&reply, 414, URI_TITLE);
}

#[test]
fn h1_serves_a_head_at_the_head_limit() {
    let s = Stack::start();
    let head = h1_head_of_size(HEAD_LIMIT, "/ok");
    assert_eq!(head.len(), HEAD_LIMIT);
    let reply = s.h1(&head);
    assert_eq!(reply.status, 200, "body was {:?}", reply.text());
}

#[test]
fn h1_refuses_a_head_one_byte_over_the_head_limit() {
    let s = Stack::start();
    let head = h1_head_of_size(HEAD_LIMIT + 1, "/never");
    assert_eq!(head.len(), HEAD_LIMIT + 1);
    let reply = s.h1(&head);
    s.assert_refused(&reply, 400, HEAD_TITLE);
}

// ── HTTP/2 ──────────────────────────────────────────────────────────────────

#[test]
fn h2_serves_a_field_at_the_header_line_limit() {
    let s = Stack::start();
    let mut fields = h2_base_fields("/ok");
    fields.push((b"x-pad".to_vec(), vec![b'a'; HEADER_LINE_LIMIT - 5]));
    assert_eq!(
        fields.last().unwrap().0.len() + fields.last().unwrap().1.len(),
        HEADER_LINE_LIMIT
    );
    let reply = h2_send(
        s.port,
        "/ok",
        extra_headers(&fields[7..]),
        Duration::from_secs(10),
    )
    .expect("a field exactly at the limit must be served");
    assert_eq!(reply.status, 200, "body was {:?}", reply.text());
}

#[test]
fn h2_refuses_a_field_one_byte_over_the_header_line_limit() {
    let s = Stack::start();
    let reply = h2_send(
        s.port,
        "/never",
        vec![(
            "x-pad".to_string(),
            "a".repeat(HEADER_LINE_LIMIT + 1 - "x-pad".len()),
        )],
        Duration::from_secs(10),
    )
    .expect("the edge must answer rather than forward");
    s.assert_refused(&reply, 400, HEAD_TITLE);
}

/// The longest HTTP/2 request line that the *field* limit allows. A line of
/// exactly `MAX_HEADER_LINE` puts `:path` one byte over `MAX_HEADER_LINE`
/// (the field is name plus value, and its name is five bytes), so the highest
/// servable request line is one byte lower. The 414 case is tested over the
/// limit below, where the request-line check runs first.
#[test]
fn h2_serves_a_request_line_one_byte_under_the_header_line_limit() {
    let s = Stack::start();
    let path_len = HEADER_LINE_LIMIT - "GET ".len() - 1;
    let path = format!("/{}", "a".repeat(path_len - 1));
    let reply = h2_send(s.port, &path, vec![], Duration::from_secs(10))
        .expect("the longest servable request line must be served");
    assert_eq!(reply.status, 200, "body was {:?}", reply.text());
}

#[test]
fn h2_refuses_a_request_line_one_byte_over_the_header_line_limit() {
    let s = Stack::start();
    let path = over_limit_http2_request_line_path();
    let reply = h2_send(s.port, &path, vec![], Duration::from_secs(10))
        .expect("the edge must answer rather than forward");
    s.assert_refused(&reply, 414, URI_TITLE);
}

#[test]
fn h2_serves_a_head_at_the_head_limit() {
    let s = Stack::start();
    let fields = pad_fields(h2_base_fields("/ok"), HEAD_LIMIT, "s");
    assert_eq!(field_list_size(&fields), HEAD_LIMIT);
    let reply = h2_send(
        s.port,
        "/ok",
        extra_headers(&fields[7..]),
        Duration::from_secs(10),
    )
    .expect("a head exactly at the limit must be served");
    assert_eq!(reply.status, 200, "body was {:?}", reply.text());
}

#[test]
fn h2_refuses_a_head_one_byte_over_the_head_limit() {
    let s = Stack::start();
    let fields = pad_fields(h2_base_fields("/never"), HEAD_LIMIT + 1, "o");
    assert_eq!(field_list_size(&fields), HEAD_LIMIT + 1);
    let reply = h2_send(
        s.port,
        "/never",
        extra_headers(&fields[7..]),
        Duration::from_secs(10),
    )
    .expect("the edge must answer rather than forward");
    s.assert_refused(&reply, 400, HEAD_TITLE);
}

// ── HTTP/3 ──────────────────────────────────────────────────────────────────

#[test]
fn h3_serves_a_field_at_the_header_line_limit() {
    let s = Stack::start();
    let value = vec![b'a'; HEADER_LINE_LIMIT - "x-pad".len()];
    let reply = h3_send(
        s.port,
        b"GET",
        b"/ok",
        &[(b"x-pad", &value)],
        Duration::from_secs(15),
    )
    .expect("a field exactly at the limit must be served");
    assert_eq!(reply.status, 200, "body was {:?}", reply.text());
}

#[test]
fn h3_refuses_a_field_one_byte_over_the_header_line_limit() {
    let s = Stack::start();
    let value = vec![b'a'; HEADER_LINE_LIMIT + 1 - "x-pad".len()];
    let reply = h3_send(
        s.port,
        b"GET",
        b"/never",
        &[(b"x-pad", &value)],
        Duration::from_secs(15),
    )
    .expect("the edge must answer rather than forward");
    s.assert_refused(&reply, 400, HEAD_TITLE);
}

/// As in HTTP/2, a request line of exactly `MAX_HEADER_LINE` would put the
/// `:path` field one byte over the field limit, so the highest servable line
/// is one byte lower.
#[test]
fn h3_serves_a_request_line_one_byte_under_the_header_line_limit() {
    let s = Stack::start();
    let path_len = HEADER_LINE_LIMIT - "GET ".len() - 1;
    let path = format!("/{}", "a".repeat(path_len - 1));
    let reply = h3_send(
        s.port,
        b"GET",
        path.as_bytes(),
        &[],
        Duration::from_secs(15),
    )
    .expect("the longest servable request line must be served");
    assert_eq!(reply.status, 200, "body was {:?}", reply.text());
}

#[test]
fn h3_refuses_a_request_line_one_byte_over_the_header_line_limit() {
    let s = Stack::start();
    let path = over_limit_http2_request_line_path();
    let reply = h3_send(
        s.port,
        b"GET",
        path.as_bytes(),
        &[],
        Duration::from_secs(15),
    )
    .expect("the edge must answer rather than forward");
    s.assert_refused(&reply, 414, URI_TITLE);
}

#[test]
fn h3_serves_a_head_at_the_head_limit() {
    let s = Stack::start();
    let fields = pad_fields(h3_base_fields("/ok"), HEAD_LIMIT, "s");
    assert_eq!(field_list_size(&fields), HEAD_LIMIT);
    let extra: Vec<(&[u8], &[u8])> = fields[4..]
        .iter()
        .map(|(name, value)| (name.as_slice(), value.as_slice()))
        .collect();
    let reply = match h3_send(s.port, b"GET", b"/ok", &extra, Duration::from_secs(15)) {
        Ok(reply) => reply,
        Err(e) => panic!(
            "a head exactly at the limit must be served: {e}\nedge:\n{}",
            s.edge_output()
        ),
    };
    assert_eq!(reply.status, 200, "body was {:?}", reply.text());
}

/// A head one byte over the limit is a connection error on HTTP/3. The pinned
/// quiche enforces `SETTINGS_MAX_FIELD_SECTION_SIZE` itself, by closing the
/// connection before the decoded list reaches m6, so there is no stream left
/// to answer 400 on. This test pins that outcome and the backend being
/// untouched, and it is not a new behaviour on this branch.
#[test]
fn h3_closes_the_connection_for_a_head_one_byte_over_the_head_limit() {
    let s = Stack::start();
    let fields = pad_fields(h3_base_fields("/never"), HEAD_LIMIT + 1, "o");
    assert_eq!(field_list_size(&fields), HEAD_LIMIT + 1);
    let extra: Vec<(&[u8], &[u8])> = fields[4..]
        .iter()
        .map(|(name, value)| (name.as_slice(), value.as_slice()))
        .collect();
    let result = h3_send(s.port, b"GET", b"/never", &extra, Duration::from_secs(15));
    assert!(
        result.is_err(),
        "quiche must refuse the field section before m6 sees it, got {result:?}",
        result = result.as_ref().map(|r| r.status)
    );
    assert!(
        !s.never.saw_connection(),
        "m6-http must not contact a backend for a head over the limit"
    );
}

// ── The backend parser, with no edge in front ───────────────────────────────

struct FileBackend {
    socket: PathBuf,
    _file: Service,
    _tmp: tempfile::TempDir,
}

impl FileBackend {
    fn start() -> FileBackend {
        let tmp = tempfile::Builder::new().prefix("m6hlf-").tempdir().unwrap();
        let site = tmp.path().join("site");
        std::fs::create_dir_all(site.join("configs")).unwrap();
        std::fs::create_dir_all(site.join("public")).unwrap();
        std::fs::write(site.join("public/index.html"), b"ok").unwrap();
        std::fs::write(
            site.join("site.toml"),
            "[site]\nname = \"header-limits\"\ndomain = \"localhost\"\n\n[log]\nlevel = \"warn\"\nformat = \"text\"\n",
        )
        .unwrap();
        std::fs::write(
            site.join("configs/m6-file.conf"),
            "[[route]]\npath = \"/public/{*relpath}\"\nhandler = \"files\"\nroot = \"public/\"\n",
        )
        .unwrap();

        // One name per test, not one per process: the six tests that use this
        // fixture run at once in one test process, so a pid-only name is one
        // path all six bind, and each unlinks the socket another has bound.
        // The counter gives every test its own name, and the remove below can
        // then touch no socket but this test's own.
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let n = NEXT.fetch_add(1, Ordering::SeqCst);
        let socket = PathBuf::from(format!("/tmp/m6hlf-{}-{n}.sock", std::process::id()));
        let _ = std::fs::remove_file(&socket);
        let mut file = Service::spawn(
            "m6-file",
            Command::new(binary("m6-file"))
                .arg(&site)
                .arg(site.join("configs/m6-file.conf"))
                .env("M6_SOCKET_OVERRIDE", &socket),
        );
        file.wait_for_path(&socket, Duration::from_secs(10));
        FileBackend {
            socket,
            _file: file,
            _tmp: tmp,
        }
    }

    fn send(&self, request: &[u8]) -> Reply {
        let mut stream = UnixStream::connect(&self.socket).expect("connect to m6-file");
        stream.set_read_timeout(Some(Duration::from_secs(10))).ok();
        stream.write_all(request).expect("write request");
        stream.flush().ok();
        read_reply(&mut stream)
    }
}

#[test]
fn a_backend_refuses_a_field_over_the_header_line_limit() {
    let backend = FileBackend::start();
    let request = format!(
        "GET /public/x HTTP/1.1\r\nHost: localhost\r\n{}\r\n",
        h1_field_of_size(HEADER_LINE_LIMIT + 1)
    );
    let reply = backend.send(request.as_bytes());
    assert_eq!(
        reply.status,
        400,
        "a backend field over the limit must be 400; body was {:?}",
        reply.text()
    );
}

#[test]
fn a_backend_refuses_a_request_line_over_the_header_line_limit() {
    let backend = FileBackend::start();
    let request = format!(
        "{}\r\nHost: localhost\r\n\r\n",
        request_line("/public/", HEADER_LINE_LIMIT + 1)
    );
    let reply = backend.send(request.as_bytes());
    assert_eq!(
        reply.status,
        414,
        "a backend request line over the limit must be 414; body was {:?}",
        reply.text()
    );
}

#[test]
fn a_backend_refuses_a_request_line_over_the_limit_after_leading_empty_lines() {
    let backend = FileBackend::start();
    let request = format!(
        "\r\n{}\r\nHost: localhost\r\n\r\n",
        request_line("/public/", HEADER_LINE_LIMIT + 1)
    );
    let reply = backend.send(request.as_bytes());
    assert_eq!(reply.status, 414, "body was {:?}", reply.text());
}

#[test]
fn a_backend_serves_a_request_line_at_the_limit_after_leading_empty_lines() {
    let backend = FileBackend::start();
    let request = format!(
        "\r\n{}\r\nHost: localhost\r\n\r\n",
        request_line("/public/index.html?q=", HEADER_LINE_LIMIT)
    );
    let reply = backend.send(request.as_bytes());
    assert_eq!(reply.status, 200, "body was {:?}", reply.text());
}

#[test]
fn a_backend_refuses_a_head_over_the_head_limit() {
    let backend = FileBackend::start();
    let head = h1_head_of_size(HEAD_LIMIT + 1, "/public/index.html");
    let reply = backend.send(&head);
    assert_eq!(
        reply.status,
        400,
        "a backend head over the limit must be 400; body was {:?}",
        reply.text()
    );
}

#[test]
fn the_backend_serves_a_head_at_the_head_limit() {
    let backend = FileBackend::start();
    let head = h1_head_of_size(HEAD_LIMIT, "/public/index.html");
    let reply = backend.send(&head);
    assert_eq!(
        reply.status,
        200,
        "a backend head exactly at the limit must be served; body was {:?}",
        reply.text()
    );
}
