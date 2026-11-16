//! TLS material loaded at startup reaches every protocol, and a certificate or
//! key that does not load is a startup error.
//!
//! Live reload is gone (R1a): a certificate and key are read once, when m6-http
//! starts, and a change takes a restart. These tests pin the two facts that
//! follow. First, the one certificate written before start is the one every
//! protocol serves. Second, a certificate file that is not PEM, or a key that
//! does not match the certificate, stops m6-http with exit 2 rather than
//! serving on stale material.

#![allow(
    clippy::disallowed_methods,
    reason = "test code: threads, sleeps and blocking clients drive the service from outside"
)]

use std::io::{Read, Write};
use std::net::{TcpStream, UdpSocket};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use m6_core::testkit::{self, binary, claim_port, Service};

// ── Certificates ──────────────────────────────────────────────────────────────

/// A generated certificate, with the two facts the assertions need.
struct Cert {
    cert_pem: String,
    key_pem: String,
    /// The leaf, as the server will send it on the wire.
    der: Vec<u8>,
}

/// A fresh self-signed certificate for loopback.
fn gen_cert() -> Cert {
    let ck =
        rcgen::generate_simple_self_signed(vec!["localhost".to_string(), "127.0.0.1".to_string()])
            .expect("self-signed certificate");
    Cert {
        cert_pem: ck.cert.pem(),
        key_pem: ck.key_pair.serialize_pem(),
        der: ck.cert.der().to_vec(),
    }
}

// ── Clients, each reporting the leaf certificate it was served ────────────────

/// A client that trusts the one certificate in the test.
fn client_trusting(cert: &Cert, alpn: &[&[u8]]) -> Arc<rustls::ClientConfig> {
    rustls::crypto::ring::default_provider()
        .install_default()
        .ok();
    let mut store = rustls::RootCertStore::empty();
    store
        .add(rustls::pki_types::CertificateDer::from(cert.der.clone()))
        .expect("trust anchor");
    let mut config = rustls::ClientConfig::builder()
        .with_root_certificates(store)
        .with_no_client_auth();
    config.alpn_protocols = alpn.iter().map(|p| p.to_vec()).collect();
    Arc::new(config)
}

/// Handshake over TCP and report the leaf certificate the server sent.
fn tls_leaf(
    port: u16,
    tls: Arc<rustls::ClientConfig>,
    want_alpn: &[u8],
) -> Result<Vec<u8>, String> {
    let tcp = TcpStream::connect(("127.0.0.1", port)).map_err(|e| format!("connect: {e}"))?;
    tcp.set_read_timeout(Some(Duration::from_secs(10)))
        .map_err(|e| format!("read timeout: {e}"))?;
    let name = rustls::pki_types::ServerName::try_from("127.0.0.1".to_string())
        .map_err(|e| format!("server name: {e}"))?;
    let conn =
        rustls::ClientConnection::new(tls, name).map_err(|e| format!("client config: {e}"))?;
    let mut stream = rustls::StreamOwned::new(conn, tcp);

    // A request, because the handshake completes lazily on first use. `/health`
    // needs no backend and no credential, so a failure here is a TLS failure.
    let req =
        format!("GET /health HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n");
    stream
        .write_all(req.as_bytes())
        .map_err(|e| format!("write: {e}"))?;
    stream.flush().map_err(|e| format!("flush: {e}"))?;
    let mut sink = Vec::new();
    match stream.read_to_end(&mut sink) {
        Ok(_) => {}
        // The server closes without close_notify on `Connection: close`.
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {}
        Err(e) => return Err(format!("read: {e}")),
    }

    let negotiated = stream.conn.alpn_protocol().unwrap_or(b"").to_vec();
    if negotiated != want_alpn {
        return Err(format!(
            "negotiated {:?}, wanted {:?}; this measurement would be about the wrong protocol",
            String::from_utf8_lossy(&negotiated),
            String::from_utf8_lossy(want_alpn)
        ));
    }
    let chain = stream
        .conn
        .peer_certificates()
        .ok_or("the server sent no certificate")?;
    let leaf = chain.first().ok_or("the server sent an empty chain")?;
    Ok(leaf.as_ref().to_vec())
}

fn h1_leaf(port: u16, tls: Arc<rustls::ClientConfig>) -> Result<Vec<u8>, String> {
    tls_leaf(port, tls, b"http/1.1")
}

fn h2_leaf(port: u16, tls: Arc<rustls::ClientConfig>) -> Result<Vec<u8>, String> {
    tls_leaf(port, tls, b"h2")
}

/// Handshake over QUIC and report the leaf certificate the server sent.
fn h3_leaf(port: u16) -> Result<Vec<u8>, String> {
    let server: std::net::SocketAddr = format!("127.0.0.1:{port}").parse().expect("addr");
    let udp = UdpSocket::bind("127.0.0.1:0").map_err(|e| format!("bind: {e}"))?;
    udp.set_nonblocking(true)
        .map_err(|e| format!("nonblock: {e}"))?;
    let local = udp.local_addr().map_err(|e| format!("local addr: {e}"))?;

    let mut config =
        quiche::Config::new(quiche::PROTOCOL_VERSION).map_err(|e| format!("quiche config: {e}"))?;
    config
        .set_application_protos(quiche::h3::APPLICATION_PROTOCOL)
        .map_err(|e| format!("alpn: {e}"))?;
    config.set_max_idle_timeout(10_000);
    config.set_initial_max_data(1_000_000);
    config.set_initial_max_stream_data_bidi_local(100_000);
    config.set_initial_max_stream_data_bidi_remote(100_000);
    config.set_initial_max_streams_bidi(10);
    config.set_initial_max_streams_uni(10);
    config.grease(false);
    config.verify_peer(false);

    let scid_bytes = [7u8; quiche::MAX_CONN_ID_LEN];
    let scid = quiche::ConnectionId::from_ref(&scid_bytes);
    let mut conn = quiche::connect(Some("localhost"), &scid, local, server, &mut config)
        .map_err(|e| format!("connect: {e}"))?;

    let mut buf = vec![0u8; 65536];
    let mut out = vec![0u8; 1350];
    let deadline = Instant::now() + Duration::from_secs(20);

    while !conn.is_established() {
        if Instant::now() > deadline {
            return Err("quic handshake did not complete".to_string());
        }
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
                Err(e) => return Err(format!("recv: {e}")),
            }
        }
        if conn.is_closed() {
            return Err("quic connection closed before it was established".to_string());
        }
        std::thread::sleep(Duration::from_millis(2));
    }

    let leaf = conn
        .peer_cert()
        .ok_or("the server sent no certificate over QUIC")?
        .to_vec();
    Ok(leaf)
}

// ── The server under test ─────────────────────────────────────────────────────

struct Server {
    port: u16,
    proc: Service,
    _dir: tempfile::TempDir,
    _claim: m6_core::testkit::PortClaim,
}

/// Write `site.toml` into `site`.
fn write_site_toml(site: &Path) {
    std::fs::create_dir_all(site).expect("site dir");
    std::fs::write(
        site.join("site.toml"),
        "[site]\nname = \"tls-startup\"\ndomain = \"localhost\"\n\n\
         [log]\nlevel = \"info\"\n\n\
         [analytics]\nenabled = false\n",
    )
    .expect("site.toml");
}

/// Write `system.toml` binding `port`, with the certificate and key named by
/// absolute paths to the files `cert` and `key` in `site`.
fn write_system_toml(site: &Path, port: u16, cert: &str, key: &str) -> std::path::PathBuf {
    let system = site.join("system.toml");
    let cert = site.join(cert).display().to_string();
    let key = site.join(key).display().to_string();
    std::fs::write(
        &system,
        format!(
            "[server]\nbind = \"127.0.0.1:{port}\"\ntls_cert = \"{cert}\"\ntls_key = \"{key}\"\n\n\
             [node]\nname = \"tls-startup\"\n"
        ),
    )
    .expect("system.toml");
    system
}

/// Start m6-http serving the configured pair, with a different pair written
/// beside it so a break that loads the wrong files is visible as a leaf change.
fn start(configured: &Cert, alternate: &Cert) -> Server {
    let dir = tempfile::tempdir().expect("tempdir");
    let site = dir.path().to_path_buf();
    std::fs::write(site.join("cert.pem"), &configured.cert_pem).expect("write cert");
    std::fs::write(site.join("key.pem"), &configured.key_pem).expect("write key");
    std::fs::write(site.join("cert2.pem"), &alternate.cert_pem).expect("write cert2");
    std::fs::write(site.join("key2.pem"), &alternate.key_pem).expect("write key2");
    write_site_toml(&site);
    let claim = claim_port();
    let port = claim.port();
    let system = write_system_toml(&site, port, "cert.pem", "key.pem");

    let mut proc = Service::spawn(
        "m6-http-tls-startup",
        Command::new(binary("m6-http")).arg(&site).arg(&system),
    );
    proc.wait_for_tcp(port, m6_core::testkit::STARTUP_BOUND);

    Server {
        port,
        proc,
        _dir: dir,
        _claim: claim,
    }
}

/// Run m6-http to its exit, which must come within ten seconds.
fn run_until_exit(site: &Path, config: &Path) -> (Option<i32>, String) {
    let mut child = m6_core::testkit::process::Running::spawn(
        Command::new(binary("m6-http"))
            .arg(site)
            .arg(config)
            .env("RUST_LOG", "info")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped()),
    )
    .expect("spawn m6-http");
    let exited = testkit::wait::until(m6_core::testkit::STARTUP_BOUND, || {
        child.child().try_wait().expect("try_wait").is_some()
    });
    if !exited {
        // The guard kills and reaps it while this panic unwinds.
        panic!("m6-http was still running after 10s; it should have refused to start");
    }
    let out = child.take().wait_with_output().expect("output");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.code(), text)
}

// ── The tests ─────────────────────────────────────────────────────────────────

/// The one certificate written before start is the one every protocol serves.
#[test]
fn every_protocol_serves_the_configured_certificate_at_start() {
    let configured = gen_cert();
    let alternate = gen_cert();
    assert_ne!(
        configured.der, alternate.der,
        "the two certificates must differ"
    );
    let mut srv = start(&configured, &alternate);

    assert_eq!(
        h1_leaf(srv.port, client_trusting(&configured, &[b"http/1.1"])).expect("h1"),
        configured.der,
        "HTTP/1.1 serves the configured certificate"
    );
    assert_eq!(
        h2_leaf(srv.port, client_trusting(&configured, &[b"h2"])).expect("h2"),
        configured.der,
        "HTTP/2 serves the configured certificate"
    );
    assert_eq!(
        h3_leaf(srv.port).expect("h3"),
        configured.der,
        "HTTP/3 serves the configured certificate"
    );

    let status = srv.proc.terminate(Duration::from_secs(10));
    assert!(status.success(), "clean stop, got {status}");
}

/// A certificate file that exists but is not PEM is a startup error.
#[test]
fn a_certificate_file_that_is_not_pem_is_a_startup_error() {
    let dir = tempfile::tempdir().expect("tempdir");
    let site = dir.path().join("site");
    write_site_toml(&site);
    let key = gen_cert();
    std::fs::write(site.join("cert.pem"), "this is not a certificate").expect("write cert");
    std::fs::write(site.join("key.pem"), &key.key_pem).expect("write key");
    let system = write_system_toml(&site, 8443, "cert.pem", "key.pem");
    let cert_path = site.join("cert.pem");

    let (code, output) = run_until_exit(&site, &system);
    assert_eq!(code, Some(2), "output:\n{output}");
    assert!(output.contains("quiche config error"), "output:\n{output}");
    assert!(
        output.contains(cert_path.to_str().expect("utf-8 path")),
        "the error must name the certificate file, got: {output}"
    );
}

/// A valid key from a different pair is a startup error.
#[test]
fn a_key_that_does_not_match_the_certificate_is_a_startup_error() {
    let cert = gen_cert();
    let other = gen_cert();
    let dir = tempfile::tempdir().expect("tempdir");
    let site = dir.path().join("site");
    write_site_toml(&site);
    std::fs::write(site.join("cert.pem"), &cert.cert_pem).expect("write cert");
    std::fs::write(site.join("key.pem"), &other.key_pem).expect("write mismatched key");
    let system = write_system_toml(&site, 8443, "cert.pem", "key.pem");

    let (code, output) = run_until_exit(&site, &system);
    assert_eq!(code, Some(2), "output:\n{output}");
    assert!(
        output.contains("quiche config error") || output.contains("TLS config failed"),
        "output:\n{output}"
    );
}
