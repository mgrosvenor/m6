//! A backend that stops reading a request and closes must not stop m6-http
//! serving the next one.
//!
//! The scenario is `m6-file` rejecting an oversized request: it answers 413 and
//! closes without draining the rest of the request, so `m6-http` is still
//! writing when the peer goes. The pool keeps no per-member health state, so
//! the next ordinary request is served by the same member.
//!
//! QUA-017's rule, that a refusal before the whole request was written is not a
//! member failure, is obsolete: no request outcome changes a member's place in
//! the rotation. This test keeps only the property that still applies, that the
//! next request is served.

#![allow(
    clippy::disallowed_methods,
    reason = "test code: threads and blocking clients drive the service from outside"
)]

use std::io::{Read, Write};
use std::net::TcpStream;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use m6_core::testkit::{binary, claim_port, read_one, Service};
use m6_http_lib::probe;

/// The largest request head the test backend reads, as a backend's parser does.
const HEAD_CAP: usize = m6_core::headers::MAX_REQUEST_HEAD;

/// A backend that reads a request head and closes, without a reply and without
/// draining the rest of the request, when the target is `/public/refused.txt`.
/// Every other request is answered.
struct Backend {
    path: PathBuf,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Backend {
    fn start(path: PathBuf) -> Backend {
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).expect("bind backend socket");
        let stop = Arc::new(AtomicBool::new(false));
        let handle = spawn_accept(listener, Arc::clone(&stop));
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
        // Wake the blocking accept so its loop sees `stop` and returns. The
        // listener is still bound here, so this connects to it.
        let _ = UnixStream::connect(&self.path);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
        let _ = std::fs::remove_file(&self.path);
    }
}

fn spawn_accept(listener: UnixListener, stop: Arc<AtomicBool>) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        // A blocking accept, so the backend polls nothing and the order of the
        // test is driven only by the requests arriving. `Drop` wakes this with
        // one connection to the path after setting `stop`.
        while let Ok((stream, _)) = listener.accept() {
            if stop.load(Ordering::SeqCst) {
                return;
            }
            std::thread::spawn(move || serve(stream));
        }
    })
}

/// Read the head byte by byte. The refused target is a request this backend
/// refuses: it closes with the body unread, the state `m6-file` leaves when it
/// rejects a request body over its cap.
fn serve(mut stream: UnixStream) {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.windows(4).any(|w| w == b"\r\n\r\n") {
        if head.len() > HEAD_CAP {
            return;
        }
        match stream.read(&mut byte) {
            Ok(0) | Err(_) => return,
            Ok(_) => head.push(byte[0]),
        }
    }
    if head.starts_with(b"POST /public/refused.txt") {
        return;
    }
    let resp = b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 2\r\nConnection: close\r\n\r\nOK";
    let _ = stream.write_all(resp);
    let _ = stream.flush();
}

/// Send `raw` on a fresh HTTP/1.1-over-TLS connection and return the response
/// bytes, or an empty vector when the server closed without answering.
fn send(port: u16, raw: &[u8]) -> Vec<u8> {
    probe::install_crypto();
    let tcp = TcpStream::connect(("127.0.0.1", port)).expect("connect m6-http");
    tcp.set_read_timeout(Some(Duration::from_secs(10)))
        .expect("read timeout");
    tcp.set_write_timeout(Some(Duration::from_secs(10)))
        .expect("write timeout");
    let name = rustls::pki_types::ServerName::try_from("127.0.0.1".to_string()).expect("name");
    let conn = rustls::ClientConnection::new(probe::tls_config(b"http/1.1"), name)
        .expect("client connection");
    let mut stream = rustls::StreamOwned::new(conn, tcp);
    stream.write_all(raw).expect("write request");
    stream.flush().expect("flush");
    read_one(&mut stream, "GET").unwrap_or_default()
}

fn status(resp: &[u8]) -> u16 {
    String::from_utf8_lossy(resp)
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

fn site_toml(glob: &str) -> String {
    format!(
        "[site]\nname = \"qua017-refusal\"\ndomain = \"localhost\"\n\n\
         [log]\nlevel = \"warn\"\n\n\
         [errors]\nmode = \"internal\"\n\n\
         [analytics]\nenabled = false\n\n\
         [rate_limit]\nenabled = false\n\n\
         [[backend]]\nname = \"app\"\nsockets = \"{glob}\"\n\n\
         [[route]]\npath = \"/public/{{relpath}}\"\nbackend = \"app\"\n"
    )
}

fn system_toml(bind_port: u16, cert: &str, key: &str) -> String {
    format!(
        "[server]\nbind = \"127.0.0.1:{bind_port}\"\ntls_cert = \"{cert}\"\ntls_key = \"{key}\"\n\n\
         [node]\nname = \"qua017-node\"\n"
    )
}

/// A backend that refuses a request by closing while the request is still being
/// written must not stop the next ordinary request being served.
///
/// The order is driven explicitly: the first request's body is large and the
/// backend closes after its head, while `m6-http` still has request bytes to
/// send. The second request then goes to the same member.
#[test]
fn a_backend_that_closes_while_the_request_is_written_does_not_take_the_pool_down() {
    let dir = tempfile::tempdir().expect("tempdir");
    let site = dir.path().to_path_buf();
    let ck =
        rcgen::generate_simple_self_signed(vec!["localhost".to_string(), "127.0.0.1".to_string()])
            .expect("certificate");
    let cert = site.join("cert.pem");
    let key = site.join("key.pem");
    std::fs::write(&cert, ck.cert.pem()).expect("cert");
    std::fs::write(&key, ck.key_pair.serialize_pem()).expect("key");

    let sock = site.join("m6-file-1.sock");
    let glob = site.join("m6-file-*.sock").display().to_string();
    let backend = Backend::start(sock);

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
    let ready = m6_core::testkit::wait::until(Duration::from_secs(20), || {
        status(&send(
            port,
            b"GET /public/probe.txt HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        )) == 200
    });
    assert!(
        ready,
        "m6-http never served a backend request\n--- m6-http ---\n{}",
        http.output()
    );

    // A body of 1 MiB: m6-http forwards it, the backend closes after the head
    // with most of the request still unwritten.
    let mut big = b"POST /public/refused.txt HTTP/1.1\r\nHost: localhost\r\nContent-Length: 1048576\r\nConnection: close\r\n\r\n".to_vec();
    big.extend(std::iter::repeat_n(b'A', 1024 * 1024));
    let first = send(port, &big);
    let first_status = status(&first);
    assert!(
        first.is_empty() || first_status == 413 || first_status == 502,
        "the refused request answered with status {}: {:?}",
        first_status,
        String::from_utf8_lossy(&first)
    );

    // The member stays in rotation for the request after the refusal: the pool
    // carries no per-member health state.
    let second = send(
        port,
        b"GET /public/open.txt HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    );
    assert!(
        second.starts_with(b"HTTP/1.1 200"),
        "the request after a refused one answered {}: {:?}\n--- m6-http ---\n{}",
        status(&second),
        String::from_utf8_lossy(&second),
        http.output()
    );
    assert!(
        String::from_utf8_lossy(&second).ends_with("OK"),
        "the served response is the backend's: {:?}",
        String::from_utf8_lossy(&second)
    );
    drop(backend);
}
