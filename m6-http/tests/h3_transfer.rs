//! A 485 KiB response over HTTP/3, checked byte for byte.
//!
//! The response is the one that would catch a batched flush whose last segment
//! is short: 496,640 bytes is not a multiple of the 1,350-byte packet, so the
//! final datagram of the transfer is short, and a batch that sends the buffer
//! as full segments only would come up a few bytes short or stop early. The
//! test builds the body from a formula, so the expected bytes are in the test
//! rather than in a fixture a change could also edit.
//!
//! The client talks to a real `m6-http` over loopback and drives quiche by
//! hand, which is the shape `tests/header_limits.rs` uses for its h3 rows.

#![allow(
    clippy::disallowed_methods,
    reason = "test code: threads, sleeps and blocking clients drive the service from outside"
)]

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream, UdpSocket};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use m6_core::testkit::{binary, claim_port, PortClaim, Service};
use quiche::h3::NameValue as _;

/// The image's size: 485 KiB, the body the performance rows use.
const BODY_LEN: usize = 496_640;

/// The response the backend serves and the client must receive, in order.
fn expected_body() -> Vec<u8> {
    (0..BODY_LEN).map(|i| ((i * 31 + 7) % 251) as u8).collect()
}

// ── The backend ──────────────────────────────────────────────────────────────

fn serve_one(mut stream: UnixStream, body: Arc<Vec<u8>>) {
    stream.set_read_timeout(Some(Duration::from_secs(10))).ok();
    let mut head = Vec::new();
    let mut buf = [0u8; 8192];
    while !head.windows(4).any(|w| w == b"\r\n\r\n") {
        match stream.read(&mut buf) {
            Ok(0) | Err(_) => return,
            Ok(n) => head.extend_from_slice(&buf[..n]),
        }
        if head.len() > 128 * 1024 {
            return;
        }
    }
    let header = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    if stream.write_all(header.as_bytes()).is_err() {
        return;
    }
    let _ = stream.write_all(&body);
    let _ = stream.flush();
}

fn start_backend(path: &PathBuf, body: Arc<Vec<u8>>) {
    let _ = std::fs::remove_file(path);
    let listener = UnixListener::bind(path).expect("bind the image backend");
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let body = Arc::clone(&body);
            std::thread::spawn(move || serve_one(stream, body));
        }
    });
}

// ── The client ───────────────────────────────────────────────────────────────

struct Reply {
    status: u16,
    body: Vec<u8>,
}

fn quic_flush(conn: &mut quiche::Connection, udp: &UdpSocket, out: &mut [u8]) {
    while let Ok((n, info)) = conn.send(out) {
        let _ = udp.send_to(&out[..n], info.to);
    }
}

fn h3_get(port: u16, path: &[u8]) -> Result<Reply, String> {
    let server_addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
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
    // The response is many packets, so a client that started with quiche's
    // ten-packet window would spend the first turns asking for more. A wider
    // window measures the server's send path, which is what the test is for.
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
    let mut body: Vec<u8> = Vec::with_capacity(BODY_LEN);
    let mut done = false;
    let deadline = Instant::now() + Duration::from_secs(30);

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
                let headers = vec![
                    quiche::h3::Header::new(b":method", b"GET"),
                    quiche::h3::Header::new(b":path", path),
                    quiche::h3::Header::new(b":scheme", b"https"),
                    quiche::h3::Header::new(b":authority", b"localhost"),
                ];
                match h3c.send_request(&mut conn, &headers, true) {
                    Ok(_) => sent = true,
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
            return Err(format!(
                "h3 timeout: sent={sent} status={status:?} body={} bytes",
                body.len()
            ));
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

// ── The edge ─────────────────────────────────────────────────────────────────

struct Stack {
    port: u16,
    _edge: Service,
    _claim: PortClaim,
    _tmp: tempfile::TempDir,
}

impl Stack {
    fn start() -> Stack {
        let tmp = tempfile::Builder::new().prefix("m6h3x-").tempdir().unwrap();
        let base = tmp.path();

        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        // A short name under /tmp: `sockaddr_un` caps at 104 bytes on macOS.
        let socket = PathBuf::from(format!("/tmp/m6h3x-{}-{n}.sock", std::process::id()));
        start_backend(&socket, Arc::new(expected_body()));

        let cert = rcgen::generate_simple_self_signed(vec![
            "localhost".to_string(),
            "127.0.0.1".to_string(),
        ])
        .expect("self-signed cert");
        std::fs::write(base.join("cert.pem"), cert.cert.pem()).unwrap();
        std::fs::write(base.join("key.pem"), cert.key_pair.serialize_pem()).unwrap();

        let claim = claim_port();
        let port = claim.port();

        let site = base.join("site");
        std::fs::create_dir_all(&site).unwrap();
        std::fs::write(
            site.join("site.toml"),
            format!(
                r#"
[site]
name   = "h3-transfer"
domain = "localhost"

[log]
level  = "warn"

[errors]
mode = "internal"

[analytics]
enabled = false

[rate_limit]
enabled = false

[[backend]]
name    = "image"
sockets = "{}"

[[route]]
path    = "/image"
backend = "image"
"#,
                socket.display()
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

        // The TLS listener and the QUIC socket are bound in the same setup, and
        // the event loop accepts on the listener once both are registered.
        let ready = m6_core::testkit::wait::until(m6_core::testkit::STARTUP_BOUND, || {
            TcpStream::connect(("127.0.0.1", port)).is_ok()
        });
        assert!(
            ready,
            "m6-http never bound its port; edge output:\n{}",
            edge.output()
        );

        Stack {
            port,
            _edge: edge,
            _claim: claim,
            _tmp: tmp,
        }
    }
}

/// The whole 485 KiB image arrives, in order, over HTTP/3.
///
/// Break: a batched flush that drops its short last segment, one that sends a
/// full segment past the end of the body, or a batch whose segment size is
/// wrong for a packet after the first.
#[test]
fn a_485_kib_body_arrives_byte_for_byte_over_h3() {
    let stack = Stack::start();
    let reply = h3_get(stack.port, b"/image").expect("the h3 request completes");
    assert_eq!(reply.status, 200);
    assert_eq!(
        reply.body.len(),
        BODY_LEN,
        "the body length is the backend's Content-Length"
    );
    assert_eq!(reply.body, expected_body(), "the body bytes are in order");
}
