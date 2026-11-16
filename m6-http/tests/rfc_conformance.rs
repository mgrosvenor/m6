//! The m6-side RFC conformance target, run by `tools/conformance.sh`.
//!
//! The three external testers are independent implementations, and that is why
//! they matter. They all point at m6 as a **server**: they send a hostile peer at
//! it and check the answers. Behaviours live on the other side of the edge, or in
//! a field m6 writes, and no external tester can see them:
//!
//! - a backend's HTTP/2 header block split across HEADERS and CONTINUATION, and
//!   END_STREAM arriving before END_HEADERS (RFC 9113 6.2, 6.10, 8.1);
//! - the field names m6 writes on h2 and h3 (RFC 9113 8.2.1, RFC 9114 4.2),
//!   and the connection-specific fields it must not write at all (RFC 9114 4.2);
//! - a backend that closes its HTTP/1.1 connection, so every reply carries
//!   `Connection: close`, which h3 must drop and h1 must keep;
//! - `Content-Length` as `1*DIGIT` on h1 and h2 (RFC 9110 8.6, 5.6.3);
//! - `If-None-Match` answered 304 on h1, h2 and h3 (RFC 9110 13.1.2, 15.4.5).
//!
//! This file is that coverage. Every test names the RFC section it pins, and
//! `tools/conformance.sh` fails the gate when one of them fails.
//!
//! **The lower-case defect (2026-11-15).** The generation-ID work began stamping
//! every reply a source node serves with `M6-Genid`. The name was written
//! title-cased, which is legal in HTTP/1.1 but not in HTTP/2 or HTTP/3: both
//! require lower-case field names (RFC 9113 8.2.1, RFC 9114 4.2). A strict client
//! (curl, and the `h2` crate used here) rejects the whole HEADERS frame as a
//! protocol error, so it read zero body bytes where HTTP/1.1 read 2671. That is
//! example 05's `expected 2671, got 0`, reproduced here without the examples.
//!
//! The h2 client is deliberately the `h2` crate rather than m6's own client:
//! m6's decoder is tolerant and would happily read the body, which is exactly why
//! no existing test saw this. A test whose client accepts what a browser rejects
//! cannot fail on this defect. The h3 client checks the header names itself,
//! because quiche's client is also tolerant; a real h3 client is not.
//!
//! ```text
//! cargo test -p m6-http --test rfc_conformance
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

use m6_core::testkit::{binary, claim_port, PortClaim, Service};
use quiche::h3::NameValue as _;
use rustls::StreamOwned;

/// One received field, name and value, in wire order.
type Fields = Vec<(String, String)>;

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

/// The example 08 `/logs` shape: a page m6-html renders, with
/// `Cache-Control: no-store` so m6-http forwards it on every request instead of
/// answering from cache, and `Connection: close` because the backend closes its
/// HTTP/1.1 connection after each reply. `Content-Length` is present and the
/// body is small, so size is not what made it fail.
fn no_store_backend_response() -> Vec<u8> {
    let body = body();
    let mut r = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\n\
         Cache-Control: no-store\r\nLast-Modified: Sun, 15 Nov 2026 10:32:10 GMT\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    r.extend_from_slice(&body);
    r
}

/// The backend's reply: cacheable, so every protocol's request after the warm-up
/// is served from m6-http's cache, which is the path the examples failed on.
fn backend_response() -> Vec<u8> {
    let body = body();
    let mut r = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\n\
         Cache-Control: public, max-age=60\r\nETag: \"same-body-1\"\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n",
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
        let dir = tempfile::tempdir_in(m6_core::testkit::socket_scratch_root()).expect("tempdir");
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
                 [[route]]\npath = \"/\"\nbackend = \"b\"\n\n\
                 [[route]]\npath = \"/submit\"\nbackend = \"b\"\nmethods = [\"POST\"]\n",
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
        self.start_backend_with(backend_response());
    }

    fn start_backend_with(&self, reply: Vec<u8>) {
        let sock = self._dir.path().join("b-1.sock");
        let listener = UnixListener::bind(&sock).expect("bind backend socket");
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

/// Send one HTTP/1.1 request and return `(status, head, body)`. The head is
/// returned because a conditional reply's fields are the thing under test.
fn h1_request(port: u16, cert_der: &[u8], request: &str) -> (u16, String, Vec<u8>) {
    let tcp = TcpStream::connect(("127.0.0.1", port)).expect("connect h1");
    tcp.set_read_timeout(Some(Duration::from_secs(20)))
        .expect("read timeout");
    let name = rustls::pki_types::ServerName::try_from("localhost").expect("server name");
    let conn = rustls::ClientConnection::new(tls_config(cert_der, &[b"http/1.1"]), name)
        .expect("TLS client");
    let mut stream = StreamOwned::new(conn, tcp);
    stream.write_all(request.as_bytes()).expect("write request");
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
    (status, head, raw[split..].to_vec())
}

fn h1_get(port: u16, cert_der: &[u8], path: &str) -> (u16, Vec<u8>) {
    let req = format!("GET {path} HTTP/1.1\r\nHost: localhost:{port}\r\nConnection: close\r\n\r\n");
    let (status, _, body) = h1_request(port, cert_der, &req);
    (status, body)
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
/// browser does (`http::HeaderName` refuses anything but lower case). Returns
/// the status, the received fields, and the body.
fn h2_get(
    port: u16,
    cert_der: &[u8],
    path: &str,
    extra: &[(&str, &str)],
) -> Result<(u16, Fields, Vec<u8>), String> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .build()
        .map_err(|e| format!("runtime: {e}"))?;
    let extra: Vec<(String, String)> = extra
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
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
        let mut request = http::Request::builder().method("GET").uri(uri);
        for (k, v) in &extra {
            request = request.header(k.as_str(), v.as_str());
        }
        let request = request.body(()).map_err(|e| format!("request: {e}"))?;
        let (response, _send) = client
            .send_request(request, true)
            .map_err(|e| format!("send: {e}"))?;
        let response = response.await.map_err(|e| format!("response: {e}"))?;
        let status = response.status().as_u16();
        let headers: Fields = response
            .headers()
            .iter()
            .map(|(k, v)| (k.as_str().to_string(), v.to_str().unwrap_or("").to_string()))
            .collect();
        let mut body = response.into_body();
        let mut bytes = Vec::new();
        while let Some(chunk) = body.data().await {
            let chunk = chunk.map_err(|e| format!("body: {e}"))?;
            bytes.extend_from_slice(&chunk);
        }
        Ok((status, headers, bytes))
    })
}

#[test]
fn h2_serves_the_same_body_as_h1() {
    let fixture = Fixture::new();
    fixture.start_backend();
    let _svc = fixture.start();
    let h1 = warm_h1(&fixture);

    let (status, _headers, h2) = match h2_get(fixture.port(), &fixture.cert_der, "/", &[]) {
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

/// One GET over h3, returning `(status, fields, body)`. The field names are
/// returned for the caller to check against RFC 9114 4.2 rather than trusted:
/// a tolerant client must not be the only thing keeping the wire legal.
fn h3_get(port: u16, path: &str, extra: &[(&str, &str)]) -> Result<(u16, Fields, Vec<u8>), String> {
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
    let mut names: Vec<(String, String)> = Vec::new();
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
                let mut headers = vec![
                    quiche::h3::Header::new(b":method", b"GET"),
                    quiche::h3::Header::new(b":path", path.as_bytes()),
                    quiche::h3::Header::new(b":scheme", b"https"),
                    quiche::h3::Header::new(b":authority", b"localhost"),
                ];
                for (k, v) in extra {
                    headers.push(quiche::h3::Header::new(k.as_bytes(), v.as_bytes()));
                }
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
                            names.push((name.to_string(), value.to_string()));
                            if name == ":status" {
                                if let Ok(code) = value.parse::<u16>() {
                                    if code >= 200 {
                                        status = Some(code);
                                    }
                                }
                            }
                        }
                        // RFC 9114 4.2: a response carrying a connection-specific
                        // field is malformed, and a client MUST treat it as such --
                        // it resets the stream and reads no body. Checked here
                        // rather than trusted to the quiche client, which is
                        // tolerant, for the same reason the field-name case is
                        // checked by the callers.
                        for (name, _) in &names {
                            if m6_http_lib::fields::is_connection_specific(name) {
                                return Err(format!(
                                    "HTTP/3 response carried the connection-specific field \
                                     {name:?} (RFC 9114 4.2); a conforming client treats the \
                                     message as malformed and reads no body"
                                ));
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

    let (status, fields, h3) = match h3_get(fixture.port(), "/", &[]) {
        Ok(result) => result,
        Err(e) => panic!(
            "HTTP/3 fetch failed ({}); HTTP/1.1 read {} bytes",
            e,
            h1.len()
        ),
    };

    assert_eq!(status, 200, "HTTP/3 status");
    let upper: Vec<&String> = fields
        .iter()
        .map(|(n, _)| n)
        .filter(|n| n.bytes().any(|b| b.is_ascii_uppercase()))
        .collect();
    assert!(
        upper.is_empty(),
        "HTTP/3 field names must be lower case (RFC 9114 4.2); got {upper:?} in {fields:?}. \
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

/// One received field by name, for the h2 and h3 field lists.
fn field<'a>(fields: &'a Fields, name: &str) -> Option<&'a str> {
    fields
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

/// RFC 9110 15.5.6: a method the target resource does not support is answered
/// 405, and RFC 9110 10.2.1 requires `Allow` naming the methods it does
/// support.
///
/// The route `/submit` answers POST only. A GET is refused at the edge on all
/// three protocols, before the backend is contacted, and the answer says
/// `Allow: POST` rather than leaving the client to guess. `HEAD` is part of
/// `GET`'s set (RFC 9110 9.3.2), which is why a GET-only route names both.
#[test]
fn rfc9110_15_5_6_405_names_the_route_methods_on_h1_h2_and_h3() {
    let fixture = Fixture::new();
    fixture.start_backend();
    let _svc = fixture.start();

    let raw = format!(
        "GET /submit HTTP/1.1\r\nHost: localhost:{}\r\nConnection: close\r\n\r\n",
        fixture.port()
    );
    let (status, head, _) = h1_request(fixture.port(), &fixture.cert_der, &raw);
    assert_eq!(status, 405, "HTTP/1.1 GET on a POST-only route: {head}");
    assert!(
        head.lines().any(|l| l.eq_ignore_ascii_case("allow: POST")),
        "HTTP/1.1 405 must carry Allow: POST (RFC 9110 15.5.6):\n{head}"
    );

    let (status, fields, _) = h2_get(fixture.port(), &fixture.cert_der, "/submit", &[])
        .expect("HTTP/2 request for a refused method");
    assert_eq!(status, 405, "HTTP/2 GET on a POST-only route");
    assert_eq!(
        field(&fields, "allow"),
        Some("POST"),
        "HTTP/2 405 must carry Allow: POST: {fields:?}"
    );

    let (status, fields, _) =
        h3_get(fixture.port(), "/submit", &[]).expect("HTTP/3 request for a refused method");
    assert_eq!(status, 405, "HTTP/3 GET on a POST-only route");
    assert_eq!(
        field(&fields, "allow"),
        Some("POST"),
        "HTTP/3 405 must carry Allow: POST: {fields:?}"
    );
}

/// The example 08 `/logs` defect: a page the backend closes the connection on.
///
/// `/logs` is a rendered page from m6-html with `Cache-Control: no-store`, so
/// m6-http forwards every request for it, and the backend answers `Connection:
/// close` because it closes its HTTP/1.1 connection after each reply. That field
/// reached the h3 field section unchanged. HTTP/1.1 requires it and HTTP/2's
/// serialiser drops it, but RFC 9114 4.2 makes a response carrying it malformed,
/// so nghttp3 (curl) reset the stream and the page arrived as 200 with zero
/// bytes. HTTP/3 must serve the same body as HTTP/1.1 and HTTP/2, and must not
/// carry the field at all.
///
/// The reproduction is a **miss** for all three protocols, so the forwarded-reply
/// path is the one under test; the cached path is `h3_serves_the_same_body_as_h1`.
#[test]
fn rfc9114_4_2_h3_serves_the_same_body_as_h1_when_the_backend_closes() {
    let fixture = Fixture::new();
    fixture.start_backend_with(no_store_backend_response());
    let _svc = fixture.start();

    let (h1_status, h1) = h1_get(fixture.port(), &fixture.cert_der, "/");
    assert_eq!(h1_status, 200, "HTTP/1.1 status");
    assert_eq!(h1.len(), BODY_LEN, "HTTP/1.1 body length");

    let (h2_status, _, h2) = match h2_get(fixture.port(), &fixture.cert_der, "/", &[]) {
        Ok(result) => result,
        Err(e) => panic!(
            "HTTP/2 fetch failed ({}); HTTP/1.1 read {} bytes",
            e,
            h1.len()
        ),
    };
    assert_eq!(h2_status, 200, "HTTP/2 status");
    assert_eq!(
        h2.len(),
        h1.len(),
        "HTTP/2 body length must equal HTTP/1.1's ({} vs {})",
        h2.len(),
        h1.len()
    );

    let (h3_status, fields, h3) = match h3_get(fixture.port(), "/", &[]) {
        Ok(result) => result,
        Err(e) => panic!(
            "HTTP/3 fetch failed ({}); HTTP/1.1 read {} bytes and HTTP/2 read {}. \
             A response field the client must treat as malformed costs the whole body.",
            e,
            h1.len(),
            h2.len()
        ),
    };
    assert_eq!(h3_status, 200, "HTTP/3 status");
    assert_eq!(
        h3.len(),
        h1.len(),
        "HTTP/3 body length must equal HTTP/1.1's ({} vs {}); fields: {fields:?}",
        h3.len(),
        h1.len()
    );
}

// ── Content-Length is 1*DIGIT (RFC 9110 8.6, 5.6.3) ─────────────────────────

/// An m6-file on a unix socket, because the h1 parser under test is
/// `m6_core::h1` and every m6-core service shares it. `Content-Length: <NBSP>5`
/// is not a length: NBSP is not optional whitespace in RFC 9110 5.6.3, and
/// `str::trim` used to strip it, which read the field as 5 and served a request
/// whose framing the peer never declared.
struct FileBackend {
    _dir: tempfile::TempDir,
    sock: PathBuf,
    _service: Service,
}

impl FileBackend {
    fn start() -> FileBackend {
        let dir = tempfile::tempdir_in(m6_core::testkit::socket_scratch_root()).expect("tempdir");
        let d = dir.path();
        std::fs::create_dir_all(d.join("public")).expect("public");
        std::fs::write(d.join("public/open.txt"), b"open\n").expect("file");
        std::fs::create_dir_all(d.join("configs")).expect("configs");
        std::fs::write(
            d.join("configs/m6-file.conf"),
            "[[route]]\npath = \"/public/{*relpath}\"\nhandler = \"files\"\nroot = \"public/\"\n",
        )
        .expect("file config");
        std::fs::write(
            d.join("site.toml"),
            "[site]\nname = \"rfc\"\ndomain = \"localhost\"\n",
        )
        .expect("site");
        let sock = d.join("file.sock");
        let mut service = Service::spawn(
            "m6-file-rfc",
            Command::new(binary("m6-file"))
                .arg(d)
                .arg(d.join("configs/m6-file.conf"))
                .env("M6_SOCKET_OVERRIDE", &sock),
        );
        service.wait_for_path(&sock, Duration::from_secs(15));
        FileBackend {
            _dir: dir,
            sock,
            _service: service,
        }
    }

    fn raw(&self, request: &[u8]) -> String {
        let mut stream =
            std::os::unix::net::UnixStream::connect(&self.sock).expect("connect m6-file");
        stream
            .set_read_timeout(Some(Duration::from_secs(20)))
            .expect("read timeout");
        stream.write_all(request).expect("write request");
        let mut out = Vec::new();
        let _ = stream.read_to_end(&mut out);
        String::from_utf8_lossy(&out).into_owned()
    }
}

#[test]
fn rfc9110_8_6_content_length_is_one_or_more_digits_h1() {
    let backend = FileBackend::start();

    let bad = b"GET /public/open.txt HTTP/1.1\r\nHost: localhost\r\n\
                Content-Length: \xc2\xa05\r\nConnection: close\r\n\r\nhello";
    let response = backend.raw(bad);
    assert!(
        response.starts_with("HTTP/1.1 400"),
        "Content-Length: NBSP 5 is not 1*DIGIT and must be refused (RFC 9110 8.6); got:\n{response}"
    );

    let good = b"GET /public/open.txt HTTP/1.1\r\nHost: localhost\r\n\
                 Content-Length: 5\r\nConnection: close\r\n\r\nhello";
    let response = backend.raw(good);
    assert!(
        response.starts_with("HTTP/1.1 200"),
        "a well-formed Content-Length must be served, not refused; got:\n{response}"
    );
}

/// A raw HTTP/2 client, because a malformed `content-length` is a stream error
/// the `h2` crate cannot be asked to send: it validates the field itself.
enum H2Outcome {
    /// A response HEADERS frame arrived on stream 1: the request was served.
    Head,
    /// RST_STREAM arrived on stream 1 with this error code.
    Reset(u32),
}

fn h2_frame(ftype: u8, flags: u8, stream_id: u32, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(9 + payload.len());
    let len = payload.len() as u32;
    out.extend_from_slice(&len.to_be_bytes()[1..]);
    out.push(ftype);
    out.push(flags);
    out.extend_from_slice(&(stream_id & 0x7fff_ffff).to_be_bytes());
    out.extend_from_slice(payload);
    out
}

/// A literal header field without indexing, with an indexed name (RFC 7541
/// 6.2.2). Small indices and short values are enough for this request.
fn hpack_literal(static_index: usize, value: &str) -> Vec<u8> {
    let mut out = Vec::new();
    if static_index < 15 {
        out.push(static_index as u8);
    } else {
        out.push(0x0f);
        out.push((static_index - 15) as u8);
    }
    out.push(value.len() as u8);
    out.extend_from_slice(value.as_bytes());
    out
}

/// POST over h2 with `content-length` exactly as given, then return whether the
/// edge reset the stream or served the request.
fn h2_raw_post(port: u16, cert_der: &[u8], content_length: &str, body: &[u8]) -> H2Outcome {
    let tcp = TcpStream::connect(("127.0.0.1", port)).expect("connect h2");
    tcp.set_read_timeout(Some(Duration::from_secs(10)))
        .expect("read timeout");
    let name = rustls::pki_types::ServerName::try_from("localhost").expect("server name");
    let conn =
        rustls::ClientConnection::new(tls_config(cert_der, &[b"h2"]), name).expect("TLS client");
    let mut stream = StreamOwned::new(conn, tcp);

    stream
        .write_all(b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n")
        .expect("write preface");
    stream
        .write_all(&h2_frame(0x4, 0, 0, &[]))
        .expect("write SETTINGS");

    // Static table: 3 :method POST, 4 :path /, 7 :scheme https, 1 :authority,
    // 28 content-length.
    let mut block = vec![0x83, 0x84, 0x87];
    block.extend(hpack_literal(1, "localhost"));
    block.extend(hpack_literal(28, content_length));
    stream
        .write_all(&h2_frame(0x1, 0x4, 1, &block))
        .expect("write HEADERS");
    stream
        .write_all(&h2_frame(0x0, 0x1, 1, body))
        .expect("write DATA");
    stream.flush().expect("flush");

    loop {
        let mut hdr = [0u8; 9];
        stream
            .read_exact(&mut hdr)
            .expect("a frame header before the connection closed");
        let len = u32::from_be_bytes([0, hdr[0], hdr[1], hdr[2]]) as usize;
        let ftype = hdr[3];
        let stream_id = u32::from_be_bytes([hdr[5], hdr[6], hdr[7], hdr[8]]) & 0x7fff_ffff;
        let mut payload = vec![0u8; len];
        stream.read_exact(&mut payload).expect("a complete frame");
        match ftype {
            0x3 if stream_id == 1 => {
                let code = u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]);
                return H2Outcome::Reset(code);
            }
            0x1 if stream_id == 1 => return H2Outcome::Head,
            0x6 => panic!("the edge sent GOAWAY for content-length {content_length:?}"),
            _ => {}
        }
    }
}

/// RFC 9110 8.6 and RFC 9113 8.1.1: a declared `content-length` is `1*DIGIT`.
/// The old parser read `+5` as 5 and silently ignored a value it could not
/// parse, so a request whose framing the peer never declared was forwarded.
#[test]
fn rfc9110_8_6_content_length_is_one_or_more_digits_h2() {
    let fixture = Fixture::new();
    fixture.start_backend();
    let _svc = fixture.start();
    let _ = warm_h1(&fixture);

    match h2_raw_post(fixture.port(), &fixture.cert_der, "+5", b"hello") {
        H2Outcome::Reset(1) => {}
        H2Outcome::Reset(code) => panic!("expected RST_STREAM(PROTOCOL_ERROR 0x1), got 0x{code:x}"),
        H2Outcome::Head => panic!(
            "content-length +5 is not 1*DIGIT and must be a stream error; the edge served it"
        ),
    }
    match h2_raw_post(fixture.port(), &fixture.cert_der, "5", b"hello") {
        H2Outcome::Head => {}
        H2Outcome::Reset(code) => {
            panic!("a well-formed content-length 5 must be served, not reset 0x{code:x}")
        }
    }
}

// ── Field names are lower case on h2 and h3 (RFC 9113 8.2.1, RFC 9114 4.2) ──

#[test]
fn rfc9113_8_2_1_h2_field_names_are_lower_case() {
    let fixture = Fixture::new();
    fixture.start_backend();
    let _svc = fixture.start();
    let _ = warm_h1(&fixture);

    let (status, headers, _) = h2_get(fixture.port(), &fixture.cert_der, "/", &[])
        .unwrap_or_else(|e| panic!("HTTP/2 fetch failed: {e}"));
    assert_eq!(status, 200);
    let upper: Vec<&String> = headers
        .iter()
        .map(|(n, _)| n)
        .filter(|n| n.bytes().any(|b| b.is_ascii_uppercase()))
        .collect();
    assert!(
        upper.is_empty(),
        "HTTP/2 field names must be lower case (RFC 9113 8.2.1); got {upper:?} in {headers:?}"
    );
    assert!(
        headers.iter().any(|(n, _)| n == "m6-genid"),
        "the generation header must arrive as m6-genid, got {headers:?}"
    );
}

#[test]
fn rfc9114_4_2_h3_field_names_are_lower_case() {
    let fixture = Fixture::new();
    fixture.start_backend();
    let _svc = fixture.start();
    let _ = warm_h1(&fixture);

    let (status, fields, _) =
        h3_get(fixture.port(), "/", &[]).unwrap_or_else(|e| panic!("HTTP/3 fetch failed: {e}"));
    assert_eq!(status, 200);
    let upper: Vec<&String> = fields
        .iter()
        .map(|(n, _)| n)
        .filter(|n| n.bytes().any(|b| b.is_ascii_uppercase()))
        .collect();
    assert!(
        upper.is_empty(),
        "HTTP/3 field names must be lower case (RFC 9114 4.2); got {upper:?} in {fields:?}"
    );
    assert!(
        fields.iter().any(|(n, _)| n == "m6-genid"),
        "the generation header must arrive as m6-genid, got {fields:?}"
    );
}

// ── If-None-Match is 304 on h1, h2 and h3 (RFC 9110 13.1.2, 15.4.5) ─────────

fn h1_header(head: &str, name: &str) -> Option<String> {
    head.lines()
        .filter_map(|l| l.split_once(':'))
        .find(|(k, _)| k.trim().eq_ignore_ascii_case(name))
        .map(|(_, v)| v.trim().to_string())
}

fn field_value(fields: &[(String, String)], name: &str) -> Option<String> {
    fields
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.clone())
}

fn assert_not_modified(status: u16, body: &[u8], etag: Option<String>, what: &str) {
    assert_eq!(status, 304, "{what}: a matching If-None-Match must be 304");
    assert!(body.is_empty(), "{what}: a 304 must carry no body");
    assert_eq!(
        etag.as_deref(),
        Some("\"same-body-1\""),
        "{what}: RFC 9110 15.4.5 requires the 304 to carry the 200's ETag"
    );
}

#[test]
fn rfc9110_13_1_2_if_none_match_304_h1() {
    let fixture = Fixture::new();
    fixture.start_backend();
    let _svc = fixture.start();
    let _ = warm_h1(&fixture);

    let (status, head, _body) = h1_request(
        fixture.port(),
        &fixture.cert_der,
        &format!(
            "GET / HTTP/1.1\r\nHost: localhost:{}\r\nConnection: close\r\n\r\n",
            fixture.port()
        ),
    );
    assert_eq!(status, 200);
    let etag = h1_header(&head, "etag").expect("a 200 carries an ETag");

    let (status, head, body) = h1_request(
        fixture.port(),
        &fixture.cert_der,
        &format!(
            "GET / HTTP/1.1\r\nHost: localhost:{}\r\nIf-None-Match: {etag}\r\n\
             Connection: close\r\n\r\n",
            fixture.port()
        ),
    );
    assert_not_modified(status, &body, h1_header(&head, "etag"), "h1");
    assert!(
        h1_header(&head, "content-length").is_none(),
        "RFC 9110 15.4.5 and 8.6: a 304 carries no content-length it cannot justify"
    );
    assert!(
        h1_header(&head, "date").is_some(),
        "RFC 9110 15.4.5: a 304 carries the Date a 200 would have sent"
    );
}

#[test]
fn rfc9110_13_1_2_if_none_match_304_h2() {
    let fixture = Fixture::new();
    fixture.start_backend();
    let _svc = fixture.start();
    let _ = warm_h1(&fixture);

    let (_, headers, _) = h2_get(fixture.port(), &fixture.cert_der, "/", &[])
        .unwrap_or_else(|e| panic!("HTTP/2 fetch failed: {e}"));
    let etag = field_value(&headers, "etag").expect("a 200 carries an ETag");

    let (status, headers, body) = h2_get(
        fixture.port(),
        &fixture.cert_der,
        "/",
        &[("if-none-match", etag.as_str())],
    )
    .unwrap_or_else(|e| panic!("HTTP/2 conditional fetch failed: {e}"));
    assert_not_modified(status, &body, field_value(&headers, "etag"), "h2");
    assert!(
        field_value(&headers, "content-length").is_none(),
        "RFC 9110 15.4.5 and 8.6: a 304 carries no content-length it cannot justify"
    );
}

#[test]
fn rfc9110_13_1_2_if_none_match_304_h3() {
    let fixture = Fixture::new();
    fixture.start_backend();
    let _svc = fixture.start();
    let _ = warm_h1(&fixture);

    let (_, fields, _) =
        h3_get(fixture.port(), "/", &[]).unwrap_or_else(|e| panic!("HTTP/3 fetch failed: {e}"));
    let etag = field_value(&fields, "etag").expect("a 200 carries an ETag");

    let (status, fields, body) = h3_get(fixture.port(), "/", &[("if-none-match", etag.as_str())])
        .unwrap_or_else(|e| panic!("HTTP/3 conditional fetch failed: {e}"));
    assert_not_modified(status, &body, field_value(&fields, "etag"), "h3");
    assert!(
        field_value(&fields, "content-length").is_none(),
        "RFC 9110 15.4.5 and 8.6: a 304 carries no content-length it cannot justify"
    );
}
