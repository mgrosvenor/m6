//! A backend that answers before it has read the whole request: its answer is
//! forwarded, and only a backend that closes with no complete response is a 502.
//!
//! The bug this catches: the backend writes a complete reply and closes while
//! `m6-http` is still sending the request, so the write fails with EPIPE or
//! ECONNRESET. The write error used to win and the client got a 502 even though
//! the reply was already in the receive buffer. The TLS path a `https://` URL
//! backend uses failed this way; a plain socket keeps reading, and its test
//! here pins that.
//!
//! A backend that closes with nothing, or with half a head, still answers 502.

#![allow(
    clippy::disallowed_methods,
    reason = "test code: threads and blocking clients drive the service from outside"
)]

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use m6_core::testkit::{binary, claim_port, read_one, Service};
use m6_http_lib::probe;

/// The request body is large enough that the backend can answer and close while
/// most of it is still unwritten.
const BODY_LEN: usize = 1024 * 1024;

const EARLY_BODY: &str = "early answer";

/// A complete reply, written while the request is still being sent.
const EARLY_HEAD: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\n\
Content-Length: 12\r\nConnection: close\r\n\r\nearly answer";

/// A complete 413 with a body, written while the request is still being sent.
/// The URL path forwards a completed 4xx verbatim, so the https test using this
/// proves the backend's own status and body reached the client.
const EARLY_413_HEAD: &[u8] = b"HTTP/1.1 413 Payload Too Large\r\nContent-Type: text/plain\r\n\
Content-Length: 7\r\nConnection: close\r\n\r\nrefused";

/// A head that stops in the middle of a header line.
const PARTIAL_HEAD: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Len";

/// What the test backend does after reading the request head and about 1 KiB of
/// the body.
#[derive(Clone, Copy)]
enum Refusal {
    /// Write a complete reply and close.
    Answer(&'static [u8]),
    /// Close without writing anything.
    Silent,
    /// Write half a head and close.
    PartialHead,
}

/// A Unix-socket backend that reads part of the request and then acts.
struct SocketBackend {
    path: PathBuf,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl SocketBackend {
    fn start(path: PathBuf, mode: Refusal) -> SocketBackend {
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).expect("bind backend socket");
        let stop = Arc::new(AtomicBool::new(false));
        let handle = spawn_accept(listener, Arc::clone(&stop), mode);
        SocketBackend {
            path,
            stop,
            handle: Some(handle),
        }
    }
}

impl Drop for SocketBackend {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Wake the blocking accept so it sees `stop` and returns. The listener
        // is still bound here, so this connects to it.
        let _ = UnixStream::connect(&self.path);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
        let _ = std::fs::remove_file(&self.path);
    }
}

fn spawn_accept(
    listener: UnixListener,
    stop: Arc<AtomicBool>,
    mode: Refusal,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        // A blocking accept, so the backend polls nothing and the order of the
        // test is driven only by the requests arriving. `Drop` wakes this with
        // one connection to the path after setting `stop`.
        while let Ok((stream, _)) = listener.accept() {
            if stop.load(Ordering::SeqCst) {
                return;
            }
            std::thread::spawn(move || serve_socket(stream, mode));
        }
    })
}

fn serve_socket(mut stream: UnixStream, mode: Refusal) {
    read_head_and_some_body(&mut stream, mode);
}

/// What a test peer that answers early owes the reply before it closes: the
/// same shape as the fixed peer in `m6-core/src/client.rs`
/// (`a_tls_backend_that_answers_early_is_read_while_writing`). A peer that
/// drops the socket with request bytes unread makes the close a reset on Linux,
/// and a reset discards a reply the client has not read yet. So the peer
/// notifies, shuts its write half, and drains the remainder before it closes,
/// so that the close is not a reset while bytes are still arriving.
///
/// The drain is bounded by a 500 ms read timeout and a 5 s cap, so it does not
/// wait for the client's close: it ends at the first quiet half second. That
/// narrows the reset window rather than closing it, and it is the reason a
/// client that resumes writing after the quiet window can still meet a reset.
/// It is a bound first, because a test that cannot end is worse than one that
/// tests slightly less.
trait EarlyClose: Read + Write {
    /// Send the transport's end-of-reply signal, where the transport has one.
    fn notify_end(&mut self);
    /// Shut the write half, so the client sees the complete reply, then EOF.
    fn shutdown_write(&mut self);
    /// Drain what the client sends until it is quiet, bounded so a failed test
    /// cannot hang this thread. Ends at the first 500 ms of quiet or at 5 s,
    /// whichever comes first, not at the client's close.
    fn drain_until_closed(&mut self);
}

impl EarlyClose for UnixStream {
    fn notify_end(&mut self) {}
    fn shutdown_write(&mut self) {
        let _ = self.shutdown(std::net::Shutdown::Write);
    }
    fn drain_until_closed(&mut self) {
        let _ = self.set_read_timeout(Some(Duration::from_millis(500)));
        let until = std::time::Instant::now() + Duration::from_secs(5);
        let mut buf = [0u8; 8192];
        while std::time::Instant::now() < until {
            match self.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
        }
    }
}

impl EarlyClose for rustls::StreamOwned<rustls::ServerConnection, TcpStream> {
    fn notify_end(&mut self) {
        self.conn.send_close_notify();
        let _ = self.flush();
    }
    fn shutdown_write(&mut self) {
        let _ = self.get_ref().shutdown(std::net::Shutdown::Write);
    }
    fn drain_until_closed(&mut self) {
        // Once the reply is written the plaintext does not matter: the unread
        // request bytes are in the TCP receive queue, so drain that.
        let mut tcp = self.get_ref();
        let _ = tcp.set_read_timeout(Some(Duration::from_millis(500)));
        let until = std::time::Instant::now() + Duration::from_secs(5);
        let mut buf = [0u8; 8192];
        while std::time::Instant::now() < until {
            match tcp.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
        }
    }
}

/// Read the head, then up to 1 KiB of the body, then act. A real backend's
/// parser stops there when it refuses the request, leaving the rest unread.
fn read_head_and_some_body<S: EarlyClose>(stream: &mut S, mode: Refusal) {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.windows(4).any(|w| w == b"\r\n\r\n") {
        match stream.read(&mut byte) {
            Ok(0) | Err(_) => return,
            Ok(_) => head.push(byte[0]),
        }
    }
    let mut body = [0u8; 256];
    let mut got = 0usize;
    while got < 1024 {
        match stream.read(&mut body) {
            Ok(0) | Err(_) => break,
            Ok(n) => got += n,
        }
    }
    match mode {
        Refusal::Answer(head) => {
            let _ = stream.write_all(head);
            let _ = stream.flush();
            // Deliberately the same shape as the fixed m6-core peer: notify,
            // shut the write half, wait for the client to take the reply, then
            // drain the remainder before the socket is dropped. The wait is the
            // drain below: it ends at the client's close, which happens after
            // it has the reply. A separate release channel is not needed
            // because this peer is driven by the service, not by hand.
            stream.notify_end();
            stream.shutdown_write();
            stream.drain_until_closed();
        }
        Refusal::Silent => {
            // No reply is written, so there is nothing for a reset to discard:
            // the test's point is the abrupt close itself.
            let _ = stream.flush();
        }
        Refusal::PartialHead => {
            // Only half a head is written, so no complete reply can be lost.
            // The client must still see the partial head and answer 502.
            let _ = stream.write_all(PARTIAL_HEAD);
            let _ = stream.flush();
        }
    }
}

/// An HTTPS backend for a URL backend: the same refusal modes over TLS.
/// A TLS backend on a loopback port that stops when the test ends.
///
/// Its accept loop is a thread, so it needs a way to be told to stop: a flag
/// plus one connection to wake the blocking `incoming()`, then a join. A test
/// that leaves a thread accepting on a port leaks it past the test and, in a
/// full run, past every test after it.
struct TlsBackend {
    port: u16,
    stop: Arc<AtomicBool>,
    accept: Option<std::thread::JoinHandle<()>>,
}

impl Drop for TlsBackend {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        // Wakes the blocking accept, which then sees the flag and returns.
        let _ = std::net::TcpStream::connect(("127.0.0.1", self.port));
        if let Some(accept) = self.accept.take() {
            let _ = accept.join();
        }
    }
}

fn spawn_tls_backend(mode: Refusal) -> TlsBackend {
    probe::install_crypto();
    let ck =
        rcgen::generate_simple_self_signed(vec!["localhost".to_string(), "127.0.0.1".to_string()])
            .expect("certificate");
    let cert = ck.cert.der().clone();
    let key = rustls::pki_types::PrivateKeyDer::try_from(ck.key_pair.serialize_der())
        .expect("private key");
    let server = Arc::new(
        rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![cert], key)
            .expect("server config"),
    );
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind backend");
    let port = listener.local_addr().expect("addr").port();
    let stop = Arc::new(AtomicBool::new(false));
    let loop_stop = Arc::clone(&stop);
    let accept = std::thread::spawn(move || {
        for tcp in listener.incoming().flatten() {
            if loop_stop.load(std::sync::atomic::Ordering::SeqCst) {
                return;
            }
            let server = Arc::clone(&server);
            std::thread::spawn(move || {
                let Ok(conn) = rustls::ServerConnection::new(server) else {
                    return;
                };
                let mut stream = rustls::StreamOwned::new(conn, tcp);
                read_head_and_some_body(&mut stream, mode);
            });
        }
    });
    TlsBackend {
        port,
        stop,
        accept: Some(accept),
    }
}

/// One m6-http in front of the site's backend.
struct Fixture {
    // Field order is drop order: the service stops before its port claim and temp dir are dropped.
    service: Service,
    _dir: tempfile::TempDir,
    port: u16,
    _claim: m6_core::testkit::PortClaim,
}

impl Fixture {
    fn start(backends: &str, tag: &str) -> Fixture {
        let dir = tempfile::tempdir().expect("tempdir");
        let site = dir.path();
        let ck = rcgen::generate_simple_self_signed(vec![
            "localhost".to_string(),
            "127.0.0.1".to_string(),
        ])
        .expect("certificate");
        let cert = site.join("cert.pem");
        let key = site.join("key.pem");
        std::fs::write(&cert, ck.cert.pem()).expect("cert");
        std::fs::write(&key, ck.key_pair.serialize_pem()).expect("key");

        let claim = claim_port();
        let port = claim.port();
        std::fs::write(site.join("site.toml"), site_toml(backends)).expect("site.toml");
        std::fs::write(
            site.join("system.toml"),
            format!(
                "[server]\nbind = \"127.0.0.1:{port}\"\ntls_cert = \"{}\"\ntls_key = \"{}\"\n\n\
                 [node]\nname = \"qua019-early\"\n",
                cert.display(),
                key.display()
            ),
        )
        .expect("system.toml");

        let mut service = Service::spawn(
            tag,
            Command::new(binary("m6-http"))
                .arg(site)
                .arg(site.join("system.toml")),
        );
        service.wait_for_tcp(port, m6_core::testkit::STARTUP_BOUND);
        Fixture {
            service,
            _dir: dir,
            port,
            _claim: claim,
        }
    }

    fn port(&self) -> u16 {
        self.port
    }
}

fn site_toml(backends: &str) -> String {
    format!(
        "[site]\nname = \"qua019-early\"\ndomain = \"localhost\"\n\n\
         [server]\nbackend_timeout_secs = 5\n\n\
         [log]\nlevel = \"info\"\n\n\
         [errors]\nmode = \"internal\"\n\n\
         [analytics]\nenabled = false\n\n\
         [rate_limit]\nenabled = false\n\n\
         {backends}\n\n\
         [[route]]\npath = \"/public/{{relpath}}\"\nbackend = \"app\"\n"
    )
}

/// Send one POST with `BODY_LEN` bytes of body on a fresh TLS connection and
/// return the response bytes, or an empty vector when the server closed without
/// answering.
fn send(port: u16, path: &str) -> Vec<u8> {
    probe::install_crypto();
    let tcp = TcpStream::connect(("127.0.0.1", port)).expect("connect m6-http");
    tcp.set_read_timeout(Some(Duration::from_secs(20)))
        .expect("read timeout");
    tcp.set_write_timeout(Some(Duration::from_secs(20)))
        .expect("write timeout");
    let name = rustls::pki_types::ServerName::try_from("127.0.0.1".to_string()).expect("name");
    let conn = rustls::ClientConnection::new(probe::tls_config(b"http/1.1"), name)
        .expect("client connection");
    let mut stream = rustls::StreamOwned::new(conn, tcp);
    let head = format!(
        "POST {path} HTTP/1.1\r\nHost: localhost\r\nContent-Type: text/plain\r\n\
         Content-Length: {BODY_LEN}\r\nConnection: close\r\n\r\n"
    );
    stream.write_all(head.as_bytes()).expect("write head");
    let chunk = [b'A'; 64 * 1024];
    let mut left = BODY_LEN;
    while left > 0 {
        let n = left.min(chunk.len());
        stream.write_all(&chunk[..n]).expect("write body");
        left -= n;
    }
    stream.flush().expect("flush");
    read_one(&mut stream, "POST").unwrap_or_default()
}

fn status(resp: &[u8]) -> u16 {
    String::from_utf8_lossy(resp)
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

fn body(resp: &[u8]) -> String {
    match resp.windows(4).position(|w| w == b"\r\n\r\n") {
        Some(at) => String::from_utf8_lossy(&resp[at + 4..]).into_owned(),
        None => String::new(),
    }
}

/// A socket backend that reads 1 KiB of a large request, answers it in full and
/// closes: the client gets that status and body, not a 502.
#[test]
fn rfc9112_9_6_a_socket_backend_that_answers_early_is_forwarded() {
    let dir = tempfile::tempdir().expect("tempdir");
    let sock = dir.path().join("m6-file-1.sock");
    let glob = dir.path().join("m6-file-*.sock").display().to_string();
    let _backend = SocketBackend::start(sock, Refusal::Answer(EARLY_HEAD));
    let fx = Fixture::start(
        &format!("[[backend]]\nname = \"app\"\nsockets = \"{glob}\""),
        "m6-http-early-socket",
    );
    let resp = send(fx.port(), "/public/big.txt");
    assert_eq!(
        status(&resp),
        200,
        "the backend's early answer was not forwarded: {:?}\n--- m6-http ---\n{}",
        String::from_utf8_lossy(&resp),
        fx.service.output()
    );
    assert_eq!(body(&resp), EARLY_BODY);
}

/// An HTTPS URL backend that reads 1 KiB of a large request, answers it with a
/// 413 and a body and closes: the write's EPIPE/ECONNRESET must not win over the
/// reply, and the backend's own 413 and body must reach the client.
#[test]
fn rfc9112_9_6_an_https_backend_that_answers_early_is_forwarded() {
    let backend = spawn_tls_backend(Refusal::Answer(EARLY_413_HEAD));
    let port = backend.port;
    let fx = Fixture::start(
        &format!(
            "[[backend]]\nname = \"app\"\nurl = \"https://127.0.0.1:{port}\"\n\
             tls_skip_verify = true"
        ),
        "m6-http-early-https",
    );
    let resp = send(fx.port(), "/public/big.txt");
    assert_eq!(
        status(&resp),
        413,
        "the backend's early 413 was not forwarded: {:?}\n--- m6-http ---\n{}",
        String::from_utf8_lossy(&resp),
        fx.service.output()
    );
    assert_eq!(body(&resp), "refused");
}

/// A socket backend that closes while the request is still being written, with
/// no reply at all, is a 502.
#[test]
fn rfc9112_9_6_a_socket_backend_that_closes_without_a_reply_is_a_502() {
    let dir = tempfile::tempdir().expect("tempdir");
    let sock = dir.path().join("m6-file-1.sock");
    let glob = dir.path().join("m6-file-*.sock").display().to_string();
    let _backend = SocketBackend::start(sock, Refusal::Silent);
    let fx = Fixture::start(
        &format!("[[backend]]\nname = \"app\"\nsockets = \"{glob}\""),
        "m6-http-early-silent",
    );
    let resp = send(fx.port(), "/public/big.txt");
    assert_eq!(
        status(&resp),
        502,
        "a close with no response must answer 502: {:?}\n--- m6-http ---\n{}",
        String::from_utf8_lossy(&resp),
        fx.service.output()
    );
}

/// An HTTPS URL backend that closes after half a response head is a 502.
#[test]
fn rfc9112_9_6_an_https_backend_that_closes_mid_head_is_a_502() {
    let backend = spawn_tls_backend(Refusal::PartialHead);
    let port = backend.port;
    let fx = Fixture::start(
        &format!(
            "[[backend]]\nname = \"app\"\nurl = \"https://127.0.0.1:{port}\"\n\
             tls_skip_verify = true"
        ),
        "m6-http-early-partial",
    );
    let resp = send(fx.port(), "/public/big.txt");
    assert_eq!(
        status(&resp),
        502,
        "an incomplete head must answer 502: {:?}\n--- m6-http ---\n{}",
        String::from_utf8_lossy(&resp),
        fx.service.output()
    );
}
