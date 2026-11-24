//! A body-less HTTP/2 backend response whose header block is split across a
//! HEADERS frame and CONTINUATION frames is forwarded, not turned into a 502.
//!
//! RFC 9113 6.2 makes the HEADERS frame and the CONTINUATION frames after it
//! one header block, and 4.3 makes END_STREAM the end of the stream. A reply is
//! complete only when the header block ends and END_STREAM has been seen.
//!
//! The bug this catches: the client completed the stream on END_STREAM in the
//! HEADERS frame, before END_HEADERS arrived, so the reply was failed as
//! "stream ended without headers" and the edge answered 502. The h2c and h2s
//! clients share this decoder, so one h2c peer pins the rule for both.
//!
//! The stub answers one GET with a split header block and no DATA frame. The
//! old client sees a 502 here and the fixed one sees the backend's 200.

#![allow(
    clippy::disallowed_methods,
    reason = "test code: threads, sleeps and blocking clients drive the service from outside"
)]

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::Command;
use std::thread;
use std::time::Duration;

use m6_core::testkit::{binary, claim_port, read_one, PortClaim, Service};
use m6_http_lib::probe;

const TYPE_HEADERS: u8 = 0x1;
const TYPE_SETTINGS: u8 = 0x4;
const TYPE_CONTINUATION: u8 = 0x9;
const FLAG_ACK: u8 = 0x1;
const FLAG_END_STREAM: u8 = 0x1;
const FLAG_END_HEADERS: u8 = 0x4;

fn frame(out: &mut Vec<u8>, ftype: u8, flags: u8, stream_id: u32, payload: &[u8]) {
    let len = payload.len() as u32;
    out.extend_from_slice(&len.to_be_bytes()[1..]);
    out.push(ftype);
    out.push(flags);
    out.extend_from_slice(&stream_id.to_be_bytes());
    out.extend_from_slice(payload);
}

fn read_n<S: Read>(sock: &mut S, n: usize) -> Option<Vec<u8>> {
    let mut buf = vec![0u8; n];
    sock.read_exact(&mut buf).ok()?;
    Some(buf)
}

/// Answer the first request with its header block split across HEADERS (which
/// carries END_STREAM) and one CONTINUATION (which carries END_HEADERS).
fn stub_peer<S: Read + Write>(mut sock: S) {
    if read_n(&mut sock, 24).is_none() {
        return;
    }
    let mut out = Vec::new();
    frame(&mut out, TYPE_SETTINGS, 0, 0, &[]);
    sock.write_all(&out).ok();
    sock.flush().ok();

    while let Some(hdr) = read_n(&mut sock, 9) {
        let len = ((hdr[0] as usize) << 16) | ((hdr[1] as usize) << 8) | hdr[2] as usize;
        let (ftype, flags) = (hdr[3], hdr[4]);
        let stream_id = u32::from_be_bytes([hdr[5], hdr[6], hdr[7], hdr[8]]) & 0x7FFF_FFFF;
        if len > 0 && read_n(&mut sock, len).is_none() {
            return;
        }
        match ftype {
            TYPE_SETTINGS if flags & FLAG_ACK == 0 => {
                let mut ack = Vec::new();
                frame(&mut ack, TYPE_SETTINGS, FLAG_ACK, 0, &[]);
                sock.write_all(&ack).ok();
                sock.flush().ok();
            }
            TYPE_HEADERS => {
                let mut enc = hpack::Encoder::new();
                let block = enc.encode(vec![
                    (&b":status"[..], &b"200"[..]),
                    (&b"content-type"[..], &b"text/plain"[..]),
                ]);
                let split = block.len() / 2;
                let mut resp = Vec::new();
                frame(
                    &mut resp,
                    TYPE_HEADERS,
                    FLAG_END_STREAM,
                    stream_id,
                    &block[..split],
                );
                frame(
                    &mut resp,
                    TYPE_CONTINUATION,
                    FLAG_END_HEADERS,
                    stream_id,
                    &block[split..],
                );
                sock.write_all(&resp).ok();
                sock.flush().ok();
                // Keep the connection open until the client has read it.
                let mut sink = [0u8; 256];
                while matches!(sock.read(&mut sink), Ok(n) if n > 0) {}
                return;
            }
            _ => {}
        }
    }
}

/// One m6-http in front of a stub h2c backend.
struct Fixture {
    // Field order is drop order: the service stops before its port claim and
    // temp directory are dropped.
    service: Service,
    _dir: tempfile::TempDir,
    port: u16,
    _claim: PortClaim,
}

impl Fixture {
    fn start(backend_port: u16) -> Fixture {
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
        std::fs::write(site.join("site.toml"), site_toml(backend_port)).expect("site.toml");
        std::fs::write(
            site.join("system.toml"),
            format!(
                "[server]\nbind = \"127.0.0.1:{port}\"\ntls_cert = \"{}\"\ntls_key = \"{}\"\n\n\
                 [node]\nname = \"iss315\"\n",
                cert.display(),
                key.display()
            ),
        )
        .expect("system.toml");

        let mut service = Service::spawn(
            "m6-http-iss315",
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

fn site_toml(backend_port: u16) -> String {
    format!(
        "[site]\nname = \"iss315\"\ndomain = \"localhost\"\n\n\
         [server]\nbackend_timeout_secs = 5\n\n\
         [log]\nlevel = \"info\"\n\n\
         [errors]\nmode = \"internal\"\n\n\
         [analytics]\nenabled = false\n\n\
         [rate_limit]\nenabled = false\n\n\
         [[backend]]\nname = \"app\"\nurl = \"h2c://127.0.0.1:{backend_port}\"\n\n\
         [[route]]\npath = \"/split\"\nbackend = \"app\"\n"
    )
}

/// Send one GET through the edge and return the response bytes.
fn get(port: u16) -> Vec<u8> {
    let tcp = TcpStream::connect(("127.0.0.1", port)).expect("connect m6-http");
    tcp.set_read_timeout(Some(Duration::from_secs(20)))
        .expect("read timeout");
    tcp.set_write_timeout(Some(Duration::from_secs(20)))
        .expect("write timeout");
    let name = rustls::pki_types::ServerName::try_from("127.0.0.1".to_string()).expect("name");
    let conn = rustls::ClientConnection::new(probe::tls_config(b"http/1.1"), name)
        .expect("client connection");
    let mut stream = rustls::StreamOwned::new(conn, tcp);
    let req = "GET /split HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
    stream.write_all(req.as_bytes()).expect("write request");
    stream.flush().expect("flush");
    read_one(&mut stream, "GET").expect("read response")
}

fn status(resp: &[u8]) -> u16 {
    String::from_utf8_lossy(resp)
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

/// A backend that sends HEADERS with END_STREAM and finishes it in a
/// CONTINUATION frame has its body-less 200 forwarded.
#[test]
fn rfc9113_6_2_a_split_bodyless_response_is_forwarded() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind backend");
    let backend_port = listener.local_addr().expect("addr").port();
    thread::spawn(move || {
        for sock in listener.incoming().flatten() {
            thread::spawn(move || stub_peer(sock));
        }
    });

    let fx = Fixture::start(backend_port);
    let resp = get(fx.port());
    assert_eq!(
        status(&resp),
        200,
        "a split body-less response was not forwarded: {:?}\n--- m6-http ---\n{}",
        String::from_utf8_lossy(&resp),
        fx.service.output()
    );
}
