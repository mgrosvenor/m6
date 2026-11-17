//! A Host the node does not serve is answered 301 to the site's canonical name,
//! over TLS, before routing. The `:443` half of the unmatched-host policy.
//!
//! The server is a real m6-http started against a temporary site and system
//! config with no routes. A by-address request to a path that has no route
//! answers 301, which is the proof that the policy runs before route lookup. A
//! request for the served name reaches routing and answers 404. A request for
//! the served name first, then the same path by address, answers 301, so a
//! cached answer for the served name cannot stand in for a name the node does
//! not own.

#![allow(
    clippy::disallowed_methods,
    reason = "test code: threads, sleeps and blocking clients drive the service from outside"
)]

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use m6_core::testkit::{binary, claim_port, Service};

struct Cert {
    cert_pem: String,
    key_pem: String,
    der: Vec<u8>,
}

fn gen_cert() -> Cert {
    let ck = rcgen::generate_simple_self_signed(vec![
        "edge-a.example.com".to_string(),
        "127.0.0.1".to_string(),
    ])
    .expect("self-signed certificate");
    Cert {
        cert_pem: ck.cert.pem(),
        key_pem: ck.key_pair.serialize_pem(),
        der: ck.cert.der().to_vec(),
    }
}

fn client(cert: &Cert) -> Arc<rustls::ClientConfig> {
    // No provider is named here. The workspace enables one provider, so the
    // automatic builder installs it; a second provider in any workspace build
    // panics this builder, which is the invariant the aws-lc-rs change holds.
    let mut store = rustls::RootCertStore::empty();
    store
        .add(rustls::pki_types::CertificateDer::from(cert.der.clone()))
        .expect("trust anchor");
    let mut config = rustls::ClientConfig::builder()
        .with_root_certificates(store)
        .with_no_client_auth();
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Arc::new(config)
}

/// One HTTP/1.1 request over TLS, answered with its status and header block.
fn request(port: u16, tls: Arc<rustls::ClientConfig>, host: &str, path: &str) -> (u16, String) {
    let tcp = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    tcp.set_read_timeout(Some(Duration::from_secs(10)))
        .expect("read timeout");
    let name =
        rustls::pki_types::ServerName::try_from("127.0.0.1".to_string()).expect("server name");
    let conn = rustls::ClientConnection::new(tls, name).expect("client config");
    let mut stream = rustls::StreamOwned::new(conn, tcp);
    let req = format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");
    stream.write_all(req.as_bytes()).expect("write");
    stream.flush().expect("flush");
    let mut raw = Vec::new();
    match stream.read_to_end(&mut raw) {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {}
        Err(e) => panic!("read: {e}"),
    }
    let text = String::from_utf8_lossy(&raw).to_string();
    let head = text.split("\r\n\r\n").next().unwrap_or("");
    let status = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .expect("a status line");
    (status, head.to_ascii_lowercase())
}

struct Server {
    port: u16,
    cert: Cert,
    proc: Service,
    _dir: tempfile::TempDir,
    _claim: m6_core::testkit::PortClaim,
}

fn write_configs(site: &Path, port: u16, cert: &Cert, with_policy: bool) -> std::path::PathBuf {
    std::fs::create_dir_all(site).expect("site dir");
    std::fs::write(site.join("cert.pem"), &cert.cert_pem).expect("cert");
    std::fs::write(site.join("key.pem"), &cert.key_pem).expect("key");
    std::fs::write(
        site.join("site.toml"),
        "[site]\nname = \"unmatched-host\"\ndomain = \"example.com\"\n\n\
         [log]\nlevel = \"info\"\n\n\
         [analytics]\nenabled = false\n",
    )
    .expect("site.toml");
    let mut system = format!(
        "[server]\nbind = \"127.0.0.1:{port}\"\ntls_cert = \"{}\"\ntls_key = \"{}\"\n",
        site.join("cert.pem").display(),
        site.join("key.pem").display()
    );
    if with_policy {
        system.push_str(
            "served_hosts = [\"edge-a.example.com\"]\nredirect_unmatched_to = \"canonical.example\"\n",
        );
    }
    system.push_str("\n[node]\nname = \"origin\"\n");
    let path = site.join("system.toml");
    std::fs::write(&path, system).expect("system.toml");
    path
}

fn start(with_policy: bool) -> Server {
    let dir = tempfile::tempdir().expect("tempdir");
    let site = dir.path().join("site");
    let claim = claim_port();
    let port = claim.port();
    let cert = gen_cert();
    let config = write_configs(&site, port, &cert, with_policy);
    let mut proc = Service::spawn(
        "m6-http-unmatched-host",
        Command::new(binary("m6-http")).arg(&site).arg(&config),
    );
    proc.wait_for_tcp(port, m6_core::testkit::STARTUP_BOUND);
    Server {
        port,
        cert,
        proc,
        _dir: dir,
        _claim: claim,
    }
}

#[test]
fn a_by_address_request_is_answered_before_routing() {
    let mut srv = start(true);
    let tls = client(&srv.cert);
    // The certificate the server presents is the one written for the site; the
    // client trusts it, so this test measures the policy and not TLS trust.
    let (status, head) = request(
        srv.port,
        tls,
        &format!("127.0.0.1:{}", srv.port),
        "/capabilities",
    );
    assert_eq!(status, 301, "head:\n{head}");
    assert!(
        head.contains("location: https://canonical.example/capabilities"),
        "the Location is the canonical name with the path kept:\n{head}"
    );
    let status = srv.proc.terminate(m6_core::testkit::STARTUP_BOUND);
    assert!(status.success(), "clean stop, got {status}");
}

#[test]
fn a_served_name_reaches_routing_and_a_cached_answer_cannot_stand_in() {
    let mut srv = start(true);
    let tls = client(&srv.cert);
    // 404 from the served name, which is the answer a route lookup gives and a
    // warm entry would keep.
    let (status, head) = request(
        srv.port,
        tls.clone(),
        &format!("edge-a.example.com:{}", srv.port),
        "/no-such-page",
    );
    assert_eq!(status, 404, "the served name reaches routing:\n{head}");
    // The same path by address must not replay that answer.
    let (status, head) = request(
        srv.port,
        tls,
        &format!("127.0.0.1:{}", srv.port),
        "/no-such-page",
    );
    assert_eq!(status, 301, "head:\n{head}");
    assert!(
        head.contains("location: https://canonical.example/no-such-page"),
        "head:\n{head}"
    );
    let status = srv.proc.terminate(m6_core::testkit::STARTUP_BOUND);
    assert!(status.success(), "clean stop, got {status}");
}

#[test]
fn the_policy_is_off_without_a_target() {
    let mut srv = start(false);
    let tls = client(&srv.cert);
    let (status, head) = request(
        srv.port,
        tls,
        &format!("127.0.0.1:{}", srv.port),
        "/capabilities",
    );
    assert_eq!(
        status, 404,
        "a node without the policy serves as before:\n{head}"
    );
    let status = srv.proc.terminate(m6_core::testkit::STARTUP_BOUND);
    assert!(status.success(), "clean stop, got {status}");
}
