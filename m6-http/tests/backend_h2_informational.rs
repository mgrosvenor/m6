//! An HTTP/2 backend may send informational (1xx) header blocks before its
//! final one (RFC 9110 15.2). The backbone clients must drop them whole: the
//! final response must not carry a 103's fields, because the edge caches the
//! final response and serves it to everyone.
//!
//! Each test runs a stub peer that answers one GET with `informational`
//! blocks of `103` plus a `link` field, then a `200` with a body, and fetches
//! through the real client.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, Instant};

use m6_http_lib::forward::{HttpRequest, HttpResponse};
use m6_http_lib::h2c_client::H2cClientConn;
use m6_http_lib::h2s_client::H2sTlsClientConn;

const TYPE_DATA: u8 = 0x0;
const TYPE_HEADERS: u8 = 0x1;
const TYPE_SETTINGS: u8 = 0x4;
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

/// Answer the first request with `informational` 103 blocks, then a 200.
fn stub_peer<S: Read + Write>(mut sock: S, informational: usize) {
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
                let mut resp = Vec::new();
                for _ in 0..informational {
                    let block = enc.encode(vec![
                        (&b":status"[..], &b"103"[..]),
                        (&b"link"[..], &b"</app.css>; rel=preload"[..]),
                    ]);
                    frame(&mut resp, TYPE_HEADERS, FLAG_END_HEADERS, stream_id, &block);
                }
                let block = enc.encode(vec![(&b":status"[..], &b"200"[..])]);
                frame(&mut resp, TYPE_HEADERS, FLAG_END_HEADERS, stream_id, &block);
                frame(&mut resp, TYPE_DATA, FLAG_END_STREAM, stream_id, b"hello");
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

fn get() -> HttpRequest {
    HttpRequest {
        method: "GET".to_string(),
        path: "/".to_string(),
        query: None,
        version: "HTTP/1.1".to_string(),
        headers: vec![],
        body: vec![],
    }
}

/// Drive `drive` until the response (or a failure) arrives.
fn await_response(
    rx: mpsc::Receiver<std::io::Result<HttpResponse>>,
    mut drive: impl FnMut(),
) -> std::io::Result<HttpResponse> {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        drive();
        match rx.try_recv() {
            Ok(r) => return r,
            Err(mpsc::TryRecvError::Empty) => thread::sleep(Duration::from_millis(1)),
            Err(mpsc::TryRecvError::Disconnected) => panic!("stream dropped without an answer"),
        }
    }
    panic!("no answer within the deadline");
}

fn fetch_h2c(informational: usize) -> std::io::Result<HttpResponse> {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let peer = thread::spawn(move || {
        let (sock, _) = listener.accept().unwrap();
        stub_peer(sock, informational);
    });
    let mut conn = H2cClientConn::connect("127.0.0.1", port).unwrap();
    let rx = conn
        .dispatch(&get(), "127.0.0.1", "203.0.113.9", "example.test")
        .unwrap();
    let r = await_response(rx, || conn.drive());
    drop(conn);
    let _ = peer.join();
    r
}

fn fetch_h2s(informational: usize) -> std::io::Result<HttpResponse> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let ck = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
    let cert = ck.cert.der().clone();
    let key = rustls::pki_types::PrivateKeyDer::try_from(ck.key_pair.serialize_der()).unwrap();
    let mut server = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert.clone()], key)
        .unwrap();
    server.alpn_protocols = vec![b"h2".to_vec()];
    let server = Arc::new(server);

    let mut roots = rustls::RootCertStore::empty();
    roots.add(cert).unwrap();
    let mut client = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    client.alpn_protocols = vec![b"h2".to_vec()];

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let peer = thread::spawn(move || {
        let (tcp, _): (TcpStream, _) = listener.accept().unwrap();
        let conn = rustls::ServerConnection::new(server).unwrap();
        stub_peer(rustls::StreamOwned::new(conn, tcp), informational);
    });
    let mut conn = H2sTlsClientConn::connect("localhost", port, Arc::new(client)).unwrap();
    let rx = conn
        .dispatch(&get(), "localhost", "203.0.113.9", "example.test")
        .unwrap();
    let r = await_response(rx, || conn.drive());
    drop(conn);
    let _ = peer.join();
    r
}

fn assert_final_without_the_103(r: std::io::Result<HttpResponse>) {
    let r = r.expect("fetch");
    assert_eq!(r.status, 200);
    assert_eq!(r.body, b"hello");
    assert!(
        r.headers.iter().all(|(k, _)| k != "link"),
        "the 103's fields reached the final response: {:?}",
        r.headers
    );
}

fn assert_too_many(r: std::io::Result<HttpResponse>) {
    let e = r.expect_err("nine informational blocks must fail the stream");
    assert_eq!(e.kind(), std::io::ErrorKind::InvalidData, "{e}");
    assert!(e.to_string().contains("informational"), "{e}");
}

#[test]
fn h2c_a_103_before_the_final_response_leaves_no_trace() {
    assert_final_without_the_103(fetch_h2c(1));
}

#[test]
fn h2c_eight_informational_blocks_are_accepted() {
    assert_final_without_the_103(fetch_h2c(8));
}

#[test]
fn h2c_nine_informational_blocks_fail_the_stream() {
    assert_too_many(fetch_h2c(9));
}

#[test]
fn h2s_a_103_before_the_final_response_leaves_no_trace() {
    assert_final_without_the_103(fetch_h2s(1));
}

#[test]
fn h2s_eight_informational_blocks_are_accepted() {
    assert_final_without_the_103(fetch_h2s(8));
}

#[test]
fn h2s_nine_informational_blocks_fail_the_stream() {
    assert_too_many(fetch_h2s(9));
}
