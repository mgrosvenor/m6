//! A backend socket that crashes is skipped until its backoff ends, then serves
//! again once it has rejoined.
//!
//! The bug this guards against: the pool's `pick_member` either ignoring the
//! backoff (so a dead socket stays in rotation and every request to it answers
//! 502) or treating the backoff as permanent (so a recovered backend stays out
//! of the pool forever). Both halves are what issue #297 asks to pin down.
//!
//! The scenario is the one the code performs today. `forward_to_backend` sends
//! the request to whichever member `pick_member` chose and, on a connect error,
//! marks that member failed and returns the error with no retry. So when one of
//! two backends crashes, the next request routed to it answers 502 and only the
//! requests after that go to the healthy member while the backoff lasts (at
//! least one second). The test asserts that behaviour rather than changing it.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use m6_core::testkit::{binary, claim_port, read_one, wait, Service};

/// A single backend on a unix socket, serving one fixed body.
///
/// `crash` closes the listener but leaves the socket file, the state a
/// SIGKILLed backend leaves: the pool still sees the member and a connect to it
/// is refused. `restart` binds a fresh listener and atomically renames it over
/// the socket path, so the path is never absent and the member is never dropped
/// from the pool by a rescan.
struct Backend {
    path: PathBuf,
    body: String,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Backend {
    fn start(path: PathBuf, body: &str) -> Backend {
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).expect("bind backend socket");
        listener
            .set_nonblocking(true)
            .expect("nonblocking listener");
        let stop = Arc::new(AtomicBool::new(false));
        let handle = spawn_accept(listener, Arc::clone(&stop), body.to_string());
        Backend {
            path,
            body: body.to_string(),
            stop,
            handle: Some(handle),
        }
    }

    /// Close the listener without removing the socket file: a crash.
    fn crash(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let handle = self.handle.take().expect("backend already crashed");
        handle.join().expect("backend accept loop");
    }

    /// Bind a fresh listener and move it onto the same path atomically.
    fn restart(&mut self) {
        // The temp path must not match the pool's `*.sock` glob, or a rescan
        // could add it as a second member. `.tmp` does not match.
        let temp = self.path.with_extension("tmp");
        let _ = std::fs::remove_file(&temp);
        let listener = UnixListener::bind(&temp).expect("bind replacement socket");
        listener
            .set_nonblocking(true)
            .expect("nonblocking listener");
        std::fs::rename(&temp, &self.path).expect("atomically replace the socket");
        let stop = Arc::new(AtomicBool::new(false));
        self.handle = Some(spawn_accept(listener, Arc::clone(&stop), self.body.clone()));
        self.stop = stop;
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
    body: String,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        while !stop.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((stream, _)) => {
                    let body = body.clone();
                    std::thread::spawn(move || serve(stream, &body));
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(_) => break,
            }
        }
    })
}

fn serve(mut stream: UnixStream, body: &str) {
    let mut buf = [0u8; 4096];
    let mut got = Vec::new();
    while !got.windows(4).any(|w| w == b"\r\n\r\n") {
        match stream.read(&mut buf) {
            Ok(0) | Err(_) => return,
            Ok(n) => got.extend_from_slice(&buf[..n]),
        }
    }
    let resp = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(resp.as_bytes());
    let _ = stream.flush();
}

fn tls_client() -> Arc<rustls::ClientConfig> {
    use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
    use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
    use rustls::DigitallySignedStruct;

    #[derive(Debug)]
    struct NoVerify;
    impl ServerCertVerifier for NoVerify {
        fn verify_server_cert(
            &self,
            _: &CertificateDer,
            _: &[CertificateDer],
            _: &ServerName,
            _: &[u8],
            _: UnixTime,
        ) -> Result<ServerCertVerified, rustls::Error> {
            Ok(ServerCertVerified::assertion())
        }
        fn verify_tls12_signature(
            &self,
            _: &[u8],
            _: &CertificateDer,
            _: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, rustls::Error> {
            Ok(HandshakeSignatureValid::assertion())
        }
        fn verify_tls13_signature(
            &self,
            _: &[u8],
            _: &CertificateDer,
            _: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, rustls::Error> {
            Ok(HandshakeSignatureValid::assertion())
        }
        fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
            rustls::crypto::ring::default_provider()
                .signature_verification_algorithms
                .supported_schemes()
        }
    }
    rustls::crypto::ring::default_provider()
        .install_default()
        .ok();
    let mut c = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(NoVerify))
        .with_no_client_auth();
    c.alpn_protocols = vec![b"http/1.1".to_vec()];
    Arc::new(c)
}

/// GET over HTTP/1.1 and TLS. Returns the status and the body.
fn get(port: u16, path: &str) -> Result<(u16, String), String> {
    let tcp = TcpStream::connect(("127.0.0.1", port)).map_err(|e| format!("connect: {e}"))?;
    tcp.set_read_timeout(Some(Duration::from_secs(10)))
        .map_err(|e| e.to_string())?;
    let name = rustls::pki_types::ServerName::try_from("127.0.0.1".to_string())
        .map_err(|e| e.to_string())?;
    let conn = rustls::ClientConnection::new(tls_client(), name).map_err(|e| e.to_string())?;
    let mut stream = rustls::StreamOwned::new(conn, tcp);
    let req = format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n");
    stream
        .write_all(req.as_bytes())
        .map_err(|e| e.to_string())?;
    stream.flush().map_err(|e| e.to_string())?;
    let raw = read_one(&mut stream, "GET").map_err(|e| format!("read: {e}"))?;
    let text = String::from_utf8_lossy(&raw).into_owned();
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

fn system_toml(bind_port: u16, cert: &str, key: &str) -> String {
    format!(
        "[server]\nbind = \"127.0.0.1:{bind_port}\"\ntls_cert = \"{cert}\"\ntls_key = \"{key}\"\n\n\
         [node]\nname = \"pool-failover\"\n"
    )
}

fn site_toml(glob: &str) -> String {
    format!(
        "[site]\nname = \"pool-failover\"\ndomain = \"localhost\"\n\n\
         [log]\nlevel = \"info\"\nformat = \"text\"\n\n\
         [analytics]\nenabled = false\n\n\
         [rate_limit]\nenabled = false\n\n\
         [[backend]]\nname = \"app\"\nsockets = \"{glob}\"\n\n\
         [[route]]\npath = \"/\"\nbackend = \"app\"\n"
    )
}

#[test]
fn a_crashed_backend_is_skipped_until_its_backoff_ends_then_serves_again() {
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::SeqCst);
    let pid = std::process::id();
    let dir = tempfile::tempdir().expect("tempdir");
    let site = dir.path().to_path_buf();

    let ck =
        rcgen::generate_simple_self_signed(vec!["localhost".to_string(), "127.0.0.1".to_string()])
            .expect("certificate");
    let cert = site.join("cert.pem");
    let key = site.join("key.pem");
    std::fs::write(&cert, ck.cert.pem()).expect("cert");
    std::fs::write(&key, ck.key_pair.serialize_pem()).expect("key");

    let sock_a = PathBuf::from(format!("/tmp/m6pf-{pid}-{n}-a.sock"));
    let sock_b = PathBuf::from(format!("/tmp/m6pf-{pid}-{n}-b.sock"));
    let glob = format!("/tmp/m6pf-{pid}-{n}-*.sock");

    let mut a = Backend::start(sock_a, "a");
    let mut b = Backend::start(sock_b, "b");

    let claim = claim_port();
    let port = claim.port();
    std::fs::write(site.join("site.toml"), site_toml(&glob)).expect("site.toml");
    std::fs::write(
        site.join("system.toml"),
        system_toml(
            port,
            &cert.display().to_string(),
            &key.display().to_string(),
        ),
    )
    .expect("system.toml");

    let mut http = Service::spawn(
        "m6-http",
        Command::new(binary("m6-http"))
            .arg(&site)
            .arg(site.join("system.toml")),
    );
    http.wait_for_tcp(port, Duration::from_secs(20));

    // The idle pool routes to the least-connections winner, which is the first
    // member. The warm-up body names which backend that is, so the test crashes
    // exactly the member the next request will reach.
    let (warm_status, warm_body) = get(port, "/").expect("warm-up GET");
    assert_eq!(warm_status, 200, "warm-up: {warm_body}");
    let dead_is_a = warm_body == "a";

    if dead_is_a {
        a.crash();
    } else {
        b.crash();
    }

    // The next request still reaches the crashed member, whose connect is
    // refused: 502, and the pool marks it failed with a backoff.
    let (first_status, _) = get(port, "/").expect("first request after the crash");
    assert_eq!(
        first_status, 502,
        "the first request routed to the crashed backend must answer 502 \
         (m6-http does not retry it on the healthy member)"
    );

    // Restart the crashed backend while its backoff lasts. The socket path is
    // never absent, so the member stays in the pool and its backoff alone
    // decides when it serves again.
    if dead_is_a {
        a.restart();
    } else {
        b.restart();
    }

    // While the backoff lasts (one second), every request must go to the
    // healthy member. The deadline stays inside the backoff window.
    let alive_body = if dead_is_a { "b" } else { "a" };
    let deadline = Instant::now() + Duration::from_millis(400);
    loop {
        let (s, body) = get(port, "/").expect("failover GET");
        assert_eq!(s, 200, "during the backoff: {body}");
        assert_eq!(
            body, alive_body,
            "a request leaked to the crashed member during its backoff"
        );
        if Instant::now() >= deadline {
            break;
        }
    }

    // After the backoff the recovered backend is picked again and serves. Poll
    // with a deadline rather than sleeping for the one second backoff.
    let dead_body = if dead_is_a { "a" } else { "b" };
    let rejoined = wait::until(Duration::from_secs(5), || {
        get(port, "/")
            .map(|(s, body)| s == 200 && body == dead_body)
            .unwrap_or(false)
    });
    assert!(
        rejoined,
        "the restarted backend never served again after its backoff\n--- m6-http ---\n{}",
        http.output()
    );
}
