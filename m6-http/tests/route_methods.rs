//! Route `methods` at the edge: 405 with `Allow`, before body, cache or backend.
//!
//! The issue: a submit-only endpoint answered `GET` like any other route,
//! because an edge route could not declare which methods it answers. These
//! tests drive a real m6-http process and a real unix-socket backend, so the
//! refusal is observed where a client sees it and the backend connection count
//! proves the request never got there.

#![allow(
    clippy::disallowed_methods,
    reason = "test code: threads, sleeps and blocking clients drive the service from outside"
)]

use std::io::{Read, Write};
use std::net::TcpStream;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use m6_core::testkit::{claim_port, PortClaim, Service};
use rustls::StreamOwned;

/// A backend that counts how many requests reached it, and answers a fixed
/// body. The count is the assertion that a refusal stopped at the edge.
struct Backend {
    hits: Arc<AtomicUsize>,
}

impl Backend {
    fn start(dir: &Path) -> Backend {
        let sock = dir.join("b-1.sock");
        let listener = UnixListener::bind(&sock).expect("bind backend socket");
        let hits = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&hits);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                seen.fetch_add(1, Ordering::SeqCst);
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
                    // Drain a small declared body so the proxy's write does
                    // not fail before it reads the reply.
                    let text = String::from_utf8_lossy(&head).to_ascii_lowercase();
                    let len = text
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length:"))
                        .and_then(|v| v.trim().parse::<usize>().ok())
                        .unwrap_or(0);
                    let body_start = head
                        .windows(4)
                        .position(|w| w == b"\r\n\r\n")
                        .map(|i| i + 4)
                        .unwrap_or(head.len());
                    let mut got = head.len() - body_start;
                    while got < len {
                        match stream.read(&mut buf) {
                            Ok(0) | Err(_) => break,
                            Ok(n) => got += n,
                        }
                    }
                    let body = b"backend";
                    let reply = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = stream.write_all(reply.as_bytes());
                    let _ = stream.write_all(body);
                });
            }
        });
        Backend { hits }
    }

    fn hits(&self) -> usize {
        self.hits.load(Ordering::SeqCst)
    }
}

struct Fixture {
    _dir: tempfile::TempDir,
    site: PathBuf,
    system: PathBuf,
    port: PortClaim,
    cert_der: Vec<u8>,
    backend: Backend,
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

        let backend = Backend::start(d);
        let port = claim_port();
        let site = d.join("site");
        std::fs::create_dir_all(&site).expect("site directory");
        std::fs::write(
            site.join("site.toml"),
            format!(
                "[site]\nname = \"route-methods\"\ndomain = \"localhost\"\n\n\
                 [errors]\nmode = \"internal\"\n\n\
                 [analytics]\nenabled = false\n\n\
                 [log]\nlevel = \"info\"\n\n\
                 [rate_limit]\nenabled = false\n\n\
                 [server]\nbackend_timeout_secs = 5\n\n\
                 [[backend]]\nname = \"b\"\nsockets = \"{}\"\n\n\
                 [[route]]\npath = \"/\"\nbackend = \"b\"\n\n\
                 [[route]]\npath = \"/submit\"\nbackend = \"b\"\nmethods = [\"POST\"]\n\n\
                 [[route]]\npath = \"/both\"\nbackend = \"b\"\nmethods = [\"GET\", \"POST\"]\n\n\
                 [[route]]\npath = \"/read\"\nbackend = \"b\"\nmethods = [\"GET\"]\n",
                d.join("b-*.sock").display()
            ),
        )
        .expect("write site.toml");
        std::fs::write(
            d.join("system.toml"),
            format!(
                "[server]\nbind = \"{}\"\ntls_cert = \"{}\"\ntls_key = \"{}\"\n\
                 max_requests_per_connection = 1000\n\n\
                 [node]\nname = \"route-methods\"\n",
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
            backend,
            _dir: dir,
        }
    }

    fn start(&self) -> Service {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_m6-http"));
        cmd.arg(&self.site)
            .arg(&self.system)
            .env("RUST_LOG", "info")
            .current_dir(self._dir.path());
        let mut svc = Service::spawn("m6-http-route-methods", &mut cmd);
        svc.wait_for_tcp(self.port.port(), m6_core::testkit::STARTUP_BOUND);
        svc
    }

    fn port(&self) -> u16 {
        self.port.port()
    }
}

fn tls_config(cert_der: &[u8]) -> Arc<rustls::ClientConfig> {
    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .ok();
    let cert = rustls::pki_types::CertificateDer::from(cert_der.to_vec());
    let mut store = rustls::RootCertStore::empty();
    store.add(cert).expect("trust the test certificate");
    Arc::new(
        rustls::ClientConfig::builder()
            .with_root_certificates(store)
            .with_no_client_auth(),
    )
}

/// Send one HTTP/1.1 request and return `(status, head, body)`.
fn request(port: u16, cert_der: &[u8], raw: &str) -> (u16, String, Vec<u8>) {
    let tcp = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    tcp.set_read_timeout(Some(Duration::from_secs(20)))
        .expect("read timeout");
    let name = rustls::pki_types::ServerName::try_from("localhost").expect("server name");
    let conn = rustls::ClientConnection::new(tls_config(cert_der), name).expect("TLS client");
    let mut stream = StreamOwned::new(conn, tcp);
    stream.write_all(raw.as_bytes()).expect("write request");
    stream.flush().expect("flush request");
    let mut bytes = Vec::new();
    let _ = stream.read_to_end(&mut bytes);
    let split = bytes
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|i| i + 4)
        .expect("a complete HTTP/1.1 head");
    let head = String::from_utf8_lossy(&bytes[..split]).into_owned();
    let status = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    (status, head, bytes[split..].to_vec())
}

fn header<'a>(head: &'a str, name: &str) -> Option<&'a str> {
    head.lines().find_map(|line| {
        let (k, v) = line.split_once(':')?;
        k.eq_ignore_ascii_case(name).then(|| v.trim())
    })
}

fn get(port: u16, cert_der: &[u8], path: &str) -> (u16, String) {
    let raw = format!("GET {path} HTTP/1.1\r\nHost: localhost:{port}\r\nConnection: close\r\n\r\n");
    let (status, head, _) = request(port, cert_der, &raw);
    (status, head)
}

#[test]
fn a_post_only_route_answers_405_with_allow_post_and_never_reaches_the_backend() {
    let fixture = Fixture::new();
    let _svc = fixture.start();

    let (status, head) = get(fixture.port(), &fixture.cert_der, "/submit");
    assert_eq!(status, 405, "GET on a POST-only route must be 405, not 404");
    assert_eq!(
        header(&head, "allow"),
        Some("POST"),
        "a 405 must name the methods the route does answer (RFC 9110 10.2.1):\n{head}"
    );
    assert_eq!(
        fixture.backend.hits(),
        0,
        "the refusal must happen before any backend contact"
    );
}

/// An invented method stays 501. Route methods decide among methods the server
/// knows; claiming 405 would say the server implements it and the resource
/// refuses it (RFC 9110 15.5.6 and 15.6.2).
#[test]
fn an_invented_method_is_501_not_405() {
    let fixture = Fixture::new();
    let _svc = fixture.start();

    let raw = format!(
        "FOO /submit HTTP/1.1\r\nHost: localhost:{}\r\nConnection: close\r\n\r\n",
        fixture.port()
    );
    let (status, _, _) = request(fixture.port(), &fixture.cert_der, &raw);
    assert_eq!(status, 501, "an unknown method is Not Implemented");
}

#[test]
fn a_route_that_allows_get_and_post_answers_both() {
    let fixture = Fixture::new();
    let _svc = fixture.start();

    let (status, _) = get(fixture.port(), &fixture.cert_der, "/both");
    assert_eq!(status, 200, "GET on a GET+POST route is served");
    assert_eq!(fixture.backend.hits(), 1, "the GET reached the backend");

    let raw = format!(
        "POST /both HTTP/1.1\r\nHost: localhost:{}\r\nContent-Length: 3\r\nConnection: close\r\n\r\nxyz",
        fixture.port()
    );
    let (status, _, body) = request(fixture.port(), &fixture.cert_der, &raw);
    assert_eq!(status, 200, "POST on a GET+POST route is served");
    assert_eq!(body, b"backend");
    assert_eq!(fixture.backend.hits(), 2, "the POST reached the backend");
}

#[test]
fn head_is_answered_where_get_is_and_refused_where_it_is_not() {
    let fixture = Fixture::new();
    let _svc = fixture.start();

    let raw = format!(
        "HEAD /read HTTP/1.1\r\nHost: localhost:{}\r\nConnection: close\r\n\r\n",
        fixture.port()
    );
    let (status, head, body) = request(fixture.port(), &fixture.cert_der, &raw);
    assert_eq!(status, 200, "GET implies HEAD (RFC 9110 9.3.2)");
    assert!(
        body.is_empty(),
        "a HEAD response carries no body, got {} bytes",
        body.len()
    );
    assert_eq!(
        header(&head, "content-length"),
        Some("7"),
        "a HEAD response keeps the length a GET would have returned"
    );
    let reached = fixture.backend.hits();

    let raw = format!(
        "HEAD /submit HTTP/1.1\r\nHost: localhost:{}\r\nConnection: close\r\n\r\n",
        fixture.port()
    );
    let (status, head, body) = request(fixture.port(), &fixture.cert_der, &raw);
    assert_eq!(status, 405, "HEAD is refused where GET is not allowed");
    assert_eq!(header(&head, "allow"), Some("POST"));
    assert!(body.is_empty(), "the 405 HEAD response carries no body");
    assert_eq!(
        fixture.backend.hits(),
        reached,
        "the refused HEAD must not reach the backend"
    );
}

#[test]
fn options_is_refused_at_the_edge_with_the_routes_allow() {
    let fixture = Fixture::new();
    let _svc = fixture.start();

    let raw = format!(
        "OPTIONS /submit HTTP/1.1\r\nHost: localhost:{}\r\nConnection: close\r\n\r\n",
        fixture.port()
    );
    let (status, head, _) = request(fixture.port(), &fixture.cert_der, &raw);
    assert_eq!(status, 405, "OPTIONS is decided at the edge, not forwarded");
    assert_eq!(
        header(&head, "allow"),
        Some("POST"),
        "OPTIONS is told what the route does answer:\n{head}"
    );
    assert_eq!(fixture.backend.hits(), 0, "OPTIONS never reaches a backend");
}

/// An unrestricted route does not fall through to the backend for `OPTIONS`
/// either. The answer names the site-wide methods, with `OPTIONS` itself left
/// out because the answer refuses it.
#[test]
fn options_on_an_unrestricted_route_is_still_decided_at_the_edge() {
    let fixture = Fixture::new();
    let _svc = fixture.start();

    let raw = format!(
        "OPTIONS / HTTP/1.1\r\nHost: localhost:{}\r\nConnection: close\r\n\r\n",
        fixture.port()
    );
    let (status, head, _) = request(fixture.port(), &fixture.cert_der, &raw);
    assert_eq!(status, 405, "OPTIONS is answered at the edge");
    assert_eq!(
        header(&head, "allow"),
        Some("GET, HEAD, POST"),
        "Allow names the site-wide methods and not OPTIONS:\n{head}"
    );
    assert_eq!(fixture.backend.hits(), 0, "OPTIONS never reaches a backend");
}

/// The refusal is decided from the request head: a client holding its body
/// behind `Expect: 100-continue` is answered 405 without ever being invited to
/// send that body.
#[test]
fn a_wrong_method_is_refused_before_the_body_is_read() {
    let fixture = Fixture::new();
    let _svc = fixture.start();

    let raw = format!(
        "POST /read HTTP/1.1\r\nHost: localhost:{}\r\nContent-Length: 999999\r\n\
         Expect: 100-continue\r\nConnection: close\r\n\r\n",
        fixture.port()
    );
    let (status, head, _) = request(fixture.port(), &fixture.cert_der, &raw);
    assert_eq!(status, 405, "POST on a GET-only route is refused");
    assert!(
        !head.contains("100 Continue"),
        "the edge must not read the body it is refusing:\n{head}"
    );
    assert_eq!(header(&head, "allow"), Some("GET, HEAD"));
    assert_eq!(fixture.backend.hits(), 0);
}
