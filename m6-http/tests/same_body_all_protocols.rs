//! The same cached page must leave every protocol with the same body bytes.
//!
//! **The defect this holds down (2026-11-15).** The generation-ID work began
//! stamping every reply a source node serves with `M6-Genid`. The name was
//! written title-cased, which is legal in HTTP/1.1 but not in HTTP/2 or HTTP/3:
//! both require lower-case field names (RFC 9113 8.2.1, RFC 9114 4.2). A strict
//! client (curl, and the `h2` crate used here) rejects the whole HEADERS frame as
//! a protocol error, so it read zero body bytes where HTTP/1.1 read 2671. That is
//! example 05's `expected 2671, got 0`, reproduced here without the examples.
//!
//! The h2 client is deliberately the `h2` crate rather than m6's own client:
//! m6's decoder is tolerant and would happily read the body, which is exactly why
//! no existing test saw this. A test whose client accepts what a browser rejects
//! cannot fail on this defect. The h3 client checks the header names itself,
//! because quiche's client is also tolerant; a real h3 client is not.
//!
//! ```text
//! cargo test -p m6-http --test same_body_all_protocols
//! ```

#![allow(
    clippy::disallowed_methods,
    reason = "test code: threads, sleeps and blocking clients drive the service from outside"
)]

use std::io::{Read, Write};
use std::net::{TcpStream, UdpSocket};
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use m6_core::testkit::{claim_port, PortClaim, Service};
use quiche::h3::NameValue as _;
use rustls::StreamOwned;

/// The example 05 page: 2671 bytes, the size HTTP/1.1 serves for it.
const BODY_LEN: usize = 2671;

fn body() -> Vec<u8> {
    let mut b = Vec::with_capacity(BODY_LEN);
    b.extend_from_slice(b"<!doctype html><html><head><title>same body</title></head><body>");
    while b.len() < BODY_LEN - 7 {
        b.extend_from_slice(b"<p>m6</p>");
    }
    b.extend_from_slice(b"</body>");
    b.truncate(BODY_LEN);
    b
}

/// The backend's reply: cacheable, so every protocol's request after the warm-up
/// is served from m6-http's cache, which is the path the examples failed on.
fn backend_response() -> Vec<u8> {
    let body = body();
    let mut r = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\n\
         Cache-Control: public, max-age=60\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    r.extend_from_slice(&body);
    r
}

// ── The fixture: a real m6-http process with one unix-socket backend ─────────

struct Fixture {
    _dir: tempfile::TempDir,
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
        let site = d.join("site");
        std::fs::create_dir_all(&site).expect("site directory");
        std::fs::write(
            site.join("site.toml"),
            format!(
                "[site]\nname = \"same-body\"\ndomain = \"localhost\"\n\n\
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
        // No `[genid]` table on purpose. A node with no table is still the
        // source (`Role::Source(Genid::new(0, ..))`), and it stamps every reply
        // with the ID. That is what the examples' config does too, and it is why
        // removing `[genid]` did not clear their failure.
        std::fs::write(
            d.join("system.toml"),
            format!(
                "[server]\nbind = \"{}\"\ntls_cert = \"{}\"\ntls_key = \"{}\"\n\
                 max_requests_per_connection = 1000000\n\n\
                 [node]\nname = \"same-body\"\n",
                port.addr(),
                d.join("cert.pem").display(),
                d.join("key.pem").display()
            ),
        )
        .expect("write system.toml");

        Fixture {
            site,
            system: d.join("system.toml"),
            port,
            cert_der: cert.cert.der().to_vec(),
            _dir: dir,
        }
    }

    fn start_backend(&self) {
        let sock = self._dir.path().join("b-1.sock");
        let listener = UnixListener::bind(&sock).expect("bind backend socket");
        let reply = backend_response();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let reply = reply.clone();
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
                    let _ = stream.write_all(&reply);
                });
            }
        });
    }

    fn start(&self) -> Service {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_m6-http"));
        cmd.arg(&self.site)
            .arg(&self.system)
            .env("RUST_LOG", "info")
            .current_dir(self._dir.path());
        let mut svc = Service::spawn("m6-http-same-body", &mut cmd);
        svc.wait_for_tcp(self.port.port(), Duration::from_secs(15));
        svc
    }

    fn port(&self) -> u16 {
        self.port.port()
    }
}

fn tls_config(cert_der: &[u8], alpn: &[&[u8]]) -> Arc<rustls::ClientConfig> {
    rustls::crypto::ring::default_provider()
        .install_default()
        .ok();
    let cert = rustls::pki_types::CertificateDer::from(cert_der.to_vec());
    let mut store = rustls::RootCertStore::empty();
    store.add(cert).expect("trust the test certificate");
    let mut cfg = rustls::ClientConfig::builder()
        .with_root_certificates(store)
        .with_no_client_auth();
    cfg.alpn_protocols = alpn.iter().map(|p| p.to_vec()).collect();
    Arc::new(cfg)
}

// ── HTTP/1.1 ──────────────────────────────────────────────────────────────────

fn h1_get(port: u16, cert_der: &[u8], path: &str) -> (u16, Vec<u8>) {
    let tcp = TcpStream::connect(("127.0.0.1", port)).expect("connect h1");
    tcp.set_read_timeout(Some(Duration::from_secs(20)))
        .expect("read timeout");
    let name = rustls::pki_types::ServerName::try_from("localhost").expect("server name");
    let conn = rustls::ClientConnection::new(tls_config(cert_der, &[b"http/1.1"]), name)
        .expect("TLS client");
    let mut stream = StreamOwned::new(conn, tcp);
    let req = format!("GET {path} HTTP/1.1\r\nHost: localhost:{port}\r\nConnection: close\r\n\r\n");
    stream.write_all(req.as_bytes()).expect("write request");
    stream.flush().expect("flush request");
    let mut raw = Vec::new();
    let _ = stream.read_to_end(&mut raw);
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|i| i + 4)
        .expect("a complete HTTP/1.1 head");
    let head = String::from_utf8_lossy(&raw[..split]).into_owned();
    let status = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    (status, raw[split..].to_vec())
}

/// GET over h1 until the warm cache answers, then return that body. The first
/// call fills the cache; the second is the hit every other protocol must match.
fn warm_h1(fixture: &Fixture) -> Vec<u8> {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match h1_get(fixture.port(), &fixture.cert_der, "/") {
            (200, body) if body.len() == BODY_LEN => return body,
            (200, body) => panic!("h1 served {} bytes, not {BODY_LEN}", body.len()),
            (status, _) => {
                if Instant::now() >= deadline {
                    panic!("m6-http never served / over h1: last status {status}");
                }
                std::thread::sleep(Duration::from_millis(25));
            }
        }
    }
}

// ── HTTP/2: a strict client ───────────────────────────────────────────────────

/// One GET over h2 with the `h2` crate, which validates field names the way a
/// browser does (`http::HeaderName` refuses anything but lower case).
fn h2_get(port: u16, cert_der: &[u8], path: &str) -> Result<(u16, Vec<u8>), String> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .build()
        .map_err(|e| format!("runtime: {e}"))?;
    rt.block_on(async move {
        let tcp = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .map_err(|e| format!("connect: {e}"))?;
        let connector = tokio_rustls::TlsConnector::from(tls_config(cert_der, &[b"h2"]));
        let name = rustls::pki_types::ServerName::try_from("localhost")
            .map_err(|e| format!("server name: {e}"))?
            .to_owned();
        let tls = connector
            .connect(name, tcp)
            .await
            .map_err(|e| format!("TLS: {e}"))?;
        let (mut client, connection) = h2::client::handshake(tls)
            .await
            .map_err(|e| format!("h2 handshake: {e}"))?;
        // The driver owns the socket; without it no frame moves.
        tokio::spawn(async move {
            let _ = connection.await;
        });
        let uri = format!("https://localhost:{port}{path}");
        let request = http::Request::builder()
            .method("GET")
            .uri(uri)
            .body(())
            .map_err(|e| format!("request: {e}"))?;
        let (response, _send) = client
            .send_request(request, true)
            .map_err(|e| format!("send: {e}"))?;
        let response = response.await.map_err(|e| format!("response: {e}"))?;
        let status = response.status().as_u16();
        let mut body = response.into_body();
        let mut bytes = Vec::new();
        while let Some(chunk) = body.data().await {
            let chunk = chunk.map_err(|e| format!("body: {e}"))?;
            bytes.extend_from_slice(&chunk);
        }
        Ok((status, bytes))
    })
}

#[test]
fn h2_serves_the_same_body_as_h1() {
    let fixture = Fixture::new();
    fixture.start_backend();
    let _svc = fixture.start();
    let h1 = warm_h1(&fixture);

    let (status, h2) = match h2_get(fixture.port(), &fixture.cert_der, "/") {
        Ok(result) => result,
        Err(e) => panic!(
            "HTTP/2 fetch failed ({}); HTTP/1.1 read {} bytes. A strict h2 client \
             rejects a header field name that is not lower case, which is the defect \
             this test exists for.",
            e,
            h1.len()
        ),
    };

    assert_eq!(status, 200, "HTTP/2 status");
    assert_eq!(
        h2.len(),
        h1.len(),
        "HTTP/2 body length must equal HTTP/1.1's ({} vs {})",
        h2.len(),
        h1.len()
    );
}

// ── HTTP/3 ───────────────────────────────────────────────────────────────────

fn quic_flush(conn: &mut quiche::Connection, udp: &UdpSocket, out: &mut [u8]) {
    while let Ok((n, info)) = conn.send(out) {
        let _ = udp.send_to(&out[..n], info.to);
    }
}

/// One GET over h3, returning `(status, field names, body)`. The names are
/// returned for the caller to check against RFC 9114 4.2 rather than trusted:
/// a tolerant client must not be the only thing keeping the wire legal.
fn h3_get(port: u16, path: &str) -> Result<(u16, Vec<String>, Vec<u8>), String> {
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

    let scid_bytes = [7u8; quiche::MAX_CONN_ID_LEN];
    let scid = quiche::ConnectionId::from_ref(&scid_bytes);
    let mut conn = quiche::connect(Some("localhost"), &scid, local, server_addr, &mut config)
        .map_err(|e| format!("connect: {e}"))?;

    let mut h3: Option<quiche::h3::Connection> = None;
    let mut buf = vec![0u8; 65536];
    let mut out = vec![0u8; 1350];
    let mut sent = false;
    let mut status: Option<u16> = None;
    let mut names: Vec<String> = Vec::new();
    let mut bytes: Vec<u8> = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(30);

    loop {
        if Instant::now() > deadline {
            return Err(format!(
                "h3 timeout: sent={sent} status={status:?} body={}",
                bytes.len()
            ));
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
                let headers = vec![
                    quiche::h3::Header::new(b":method", b"GET"),
                    quiche::h3::Header::new(b":path", path.as_bytes()),
                    quiche::h3::Header::new(b":scheme", b"https"),
                    quiche::h3::Header::new(b":authority", b"localhost"),
                ];
                match h3c.send_request(&mut conn, &headers, true) {
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
                            names.push(name.to_string());
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
                        loop {
                            match h3c.recv_body(&mut conn, sid, &mut tmp) {
                                Ok(0) => break,
                                Ok(n) => bytes.extend_from_slice(&tmp[..n]),
                                Err(quiche::h3::Error::Done) => break,
                                Err(e) => return Err(format!("recv_body: {e}")),
                            }
                        }
                    }
                    Ok((_, quiche::h3::Event::Finished)) => {
                        return match status {
                            Some(code) => Ok((code, names, bytes)),
                            None => Err("h3 finished with no :status".to_string()),
                        };
                    }
                    Ok(_) => {}
                    Err(quiche::h3::Error::Done) => break,
                    Err(e) => return Err(format!("h3 poll: {e}")),
                }
            }
        }
    }
}

#[test]
fn h3_serves_the_same_body_as_h1() {
    let fixture = Fixture::new();
    fixture.start_backend();
    let _svc = fixture.start();
    let h1 = warm_h1(&fixture);

    let (status, names, h3) = match h3_get(fixture.port(), "/") {
        Ok(result) => result,
        Err(e) => panic!(
            "HTTP/3 fetch failed ({}); HTTP/1.1 read {} bytes",
            e,
            h1.len()
        ),
    };

    assert_eq!(status, 200, "HTTP/3 status");
    let upper: Vec<&String> = names
        .iter()
        .filter(|n| n.bytes().any(|b| b.is_ascii_uppercase()))
        .collect();
    assert!(
        upper.is_empty(),
        "HTTP/3 field names must be lower case (RFC 9114 4.2); got {upper:?} in {names:?}. \
         A real h3 client rejects the header block as a protocol error."
    );
    assert_eq!(
        h3.len(),
        h1.len(),
        "HTTP/3 body length must equal HTTP/1.1's ({} vs {})",
        h3.len(),
        h1.len()
    );
}
