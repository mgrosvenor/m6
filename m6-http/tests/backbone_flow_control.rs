//! Send-side flow control on the backbone H2 clients (RFC 9113 6.9).
//!
//! `h2c_client` and `h2s_client` used to carry a `conn_send_window` that was
//! declared, initialised to 65 535, incremented on WINDOW_UPDATE, and then
//! never decremented and never read. Both sent as much DATA as they had,
//! whenever they had it, regardless of what the peer said it would accept.
//!
//! It never showed in production because the only peer these clients talk to
//! is m6's own origin, which advertises a 1 MiB initial window and tops its
//! connection window back up inside `handle_data`. That is luck, not
//! correctness: it ends the moment a backbone client is pointed at a peer that
//! is not m6, which is the whole point of `h2c://` and `h2s://` being URL
//! schemes rather than an internal detail.
//!
//! WHY THE OBVIOUS TEST DOES NOT WORK
//!
//! `edge_proxy.rs::a_body_larger_than_max_frame_size_survives_the_edge_hop`
//! pushes 16 KiB to 1 MiB through the real edge and passes *with the defect
//! present*, because m6's origin is generous enough to absorb the overrun. A
//! stub that merely reads whatever arrives has the same problem: the body turns
//! up intact either way.
//!
//! So the stub here is a *conforming receiver*. It advertises a deliberately
//! small SETTINGS_INITIAL_WINDOW_SIZE, counts every DATA byte against the
//! credit it has actually granted, and records a violation the moment the
//! client overruns it. That is the assertion that distinguishes the fixed
//! client from the broken one; the body-content assertion then catches the
//! failure mode a naive fix would introduce, which is silent truncation.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use m6_http_lib::forward::HttpRequest;
use m6_http_lib::h2c_client::H2cClientConn;

const FRAME_HDR: usize = 9;
const TYPE_DATA: u8 = 0x0;
const TYPE_HEADERS: u8 = 0x1;
const TYPE_SETTINGS: u8 = 0x4;
const TYPE_WINDOW_UPDATE: u8 = 0x8;
const FLAG_END_STREAM: u8 = 0x1;
const FLAG_END_HEADERS: u8 = 0x4;
const FLAG_ACK: u8 = 0x1;

/// The stream window the stub advertises. Small enough that a 256 KiB body
/// cannot be sent without the client waiting for credit several times over.
const STUB_INITIAL_WINDOW: u32 = 4096;

/// What the stub observed, reported back to the test.
struct Observed {
    body: Vec<u8>,
    /// Bytes that arrived beyond the credit granted at the time they arrived.
    /// Non-zero means the client ignored flow control.
    overrun: u64,
    /// How many times the client had to stop and wait for credit. Zero would
    /// mean the stub never actually withheld anything and the test proves
    /// nothing.
    stalls: u32,
}

fn be32(b: &[u8]) -> u32 {
    ((b[0] as u32) << 24) | ((b[1] as u32) << 16) | ((b[2] as u32) << 8) | (b[3] as u32)
}

fn frame(out: &mut Vec<u8>, ftype: u8, flags: u8, stream_id: u32, payload: &[u8]) {
    let len = payload.len() as u32;
    out.push((len >> 16) as u8);
    out.push((len >> 8) as u8);
    out.push(len as u8);
    out.push(ftype);
    out.push(flags);
    out.extend_from_slice(&stream_id.to_be_bytes());
    out.extend_from_slice(payload);
}

/// Read exactly `n` bytes or return what little arrived before the peer went
/// away.
fn read_exact_or_eof(sock: &mut TcpStream, n: usize) -> Option<Vec<u8>> {
    let mut buf = vec![0u8; n];
    let mut got = 0usize;
    while got < n {
        match sock.read(&mut buf[got..]) {
            Ok(0) => return None,
            Ok(k) => got += k,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
            Err(_) => return None,
        }
    }
    Some(buf)
}

/// A minimal h2c origin that enforces the flow control it advertises.
///
/// It grants credit only *after* consuming what it already granted, and pauses
/// before doing so, which is exactly the condition the real fix has to survive:
/// credit arrives asynchronously, long after `dispatch` returned.
fn stub_origin(listener: TcpListener, report: mpsc::Sender<Observed>) {
    let (mut sock, _) = listener.accept().expect("stub: accept");
    sock.set_nodelay(true).ok();

    // Client preface, then our SETTINGS.
    if read_exact_or_eof(&mut sock, 24).is_none() {
        return;
    }
    let mut out = Vec::new();
    let settings = [
        0x00, 0x04, // SETTINGS_INITIAL_WINDOW_SIZE
        (STUB_INITIAL_WINDOW >> 24) as u8,
        (STUB_INITIAL_WINDOW >> 16) as u8,
        (STUB_INITIAL_WINDOW >> 8) as u8,
        STUB_INITIAL_WINDOW as u8,
    ];
    frame(&mut out, TYPE_SETTINGS, 0, 0, &settings);
    sock.write_all(&out).ok();
    sock.flush().ok();

    let mut body = Vec::new();
    let mut overrun = 0u64;
    let mut stalls = 0u32;

    // Credit granted to the client so far, per the values we advertised.
    // Connection credit starts at the fixed 65 535 default (SETTINGS cannot
    // change it); stream credit starts at what we just advertised.
    let mut stream_credit: i64 = STUB_INITIAL_WINDOW as i64;
    let mut conn_credit: i64 = 65_535;
    let mut req_stream = 0u32;

    loop {
        let Some(hdr) = read_exact_or_eof(&mut sock, FRAME_HDR) else { break };
        let len = ((hdr[0] as usize) << 16) | ((hdr[1] as usize) << 8) | hdr[2] as usize;
        let ftype = hdr[3];
        let flags = hdr[4];
        let stream_id = be32(&hdr[5..9]) & 0x7FFF_FFFF;
        let payload = if len > 0 {
            match read_exact_or_eof(&mut sock, len) {
                Some(p) => p,
                None => break,
            }
        } else {
            Vec::new()
        };

        match ftype {
            TYPE_SETTINGS if flags & FLAG_ACK == 0 => {
                let mut ack = Vec::new();
                frame(&mut ack, TYPE_SETTINGS, FLAG_ACK, 0, &[]);
                sock.write_all(&ack).ok();
            }
            TYPE_HEADERS => {
                req_stream = stream_id;
            }
            TYPE_DATA => {
                // Charge the frame against the credit outstanding at the moment
                // it arrived. Anything past that is a flow-control violation
                // and is what this test exists to catch.
                let n = payload.len() as i64;
                stream_credit -= n;
                conn_credit -= n;
                if stream_credit < 0 {
                    overrun += (-stream_credit) as u64;
                    stream_credit = 0;
                }
                if conn_credit < 0 {
                    overrun += (-conn_credit) as u64;
                    conn_credit = 0;
                }
                body.extend_from_slice(&payload);

                if flags & FLAG_END_STREAM != 0 {
                    // Respond: HPACK indexed field 8 is `:status: 200`, so the
                    // whole header block is one byte and the test needs no
                    // encoder.
                    let mut resp = Vec::new();
                    frame(
                        &mut resp,
                        TYPE_HEADERS,
                        FLAG_END_HEADERS | FLAG_END_STREAM,
                        stream_id,
                        &[0x88],
                    );
                    sock.write_all(&resp).ok();
                    sock.flush().ok();
                    break;
                }

                // Withhold, then replenish exactly what was consumed, the way a
                // real receiver does. The pause is what forces the client to
                // keep the remainder as resumable state instead of a loop.
                //
                // Granting the consumed byte count rather than a fixed quantum
                // matters: it is the only scheme that cannot deadlock if the
                // client is ever legitimately overdrawn (RFC 9113 6.9.2 lets a
                // window reduction push an open stream negative).
                stalls += 1;
                thread::sleep(Duration::from_millis(1));
                let grant = payload.len() as u32;
                if grant > 0 {
                    let mut wu = Vec::new();
                    frame(&mut wu, TYPE_WINDOW_UPDATE, 0, req_stream, &grant.to_be_bytes());
                    frame(&mut wu, TYPE_WINDOW_UPDATE, 0, 0, &grant.to_be_bytes());
                    if sock.write_all(&wu).is_err() {
                        break;
                    }
                    sock.flush().ok();
                    stream_credit += grant as i64;
                    conn_credit += grant as i64;
                }
            }
            _ => {}
        }
    }

    let _ = report.send(Observed { body, overrun, stalls });
}

/// A body several times the peer's advertised window arrives whole, in order,
/// and without ever overrunning the credit the peer granted.
///
/// Disable `pump_stream`'s credit check and this goes red on `overrun`: the
/// client blasts the entire body the instant `dispatch` is called, hundreds of
/// KiB past a 1 KiB window. That is the check that makes this a regression test
/// rather than something that merely looks like coverage.
#[test]
fn a_backbone_body_respects_the_peers_send_window() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind stub");
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = mpsc::channel();
    let origin = thread::spawn(move || stub_origin(listener, tx));

    // Distinctive, non-repeating content so a reordering or a dropped middle
    // chunk cannot pass a length-only check.
    const BODY_LEN: usize = 256 * 1024;
    let sent: Vec<u8> = (0..BODY_LEN).map(|i| (i % 251) as u8).collect();

    let mut conn = H2cClientConn::connect("127.0.0.1", port).expect("connect to stub");

    // Let the peer's SETTINGS land before dispatching, which is what the
    // production path does: these connections are pooled and driven by the
    // event loop long before a request is routed to one. Dispatching into a
    // connection that has not yet read its peer's SETTINGS means the stream
    // opens on the 65 535 default and is then retroactively cut to the real
    // window under 6.9.2, which is a legal but quite different scenario.
    let settle = Instant::now() + Duration::from_millis(250);
    while Instant::now() < settle {
        conn.drive();
        thread::sleep(Duration::from_millis(1));
    }
    assert!(!conn.is_dead, "connection died during the SETTINGS exchange");

    let req = HttpRequest {
        method: "POST".to_string(),
        path: "/upload".to_string(),
        query: None,
        version: "HTTP/1.1".to_string(),
        headers: vec![("content-length".to_string(), BODY_LEN.to_string())],
        body: sent.clone(),
    };
    let rx_resp = conn
        .dispatch(&req, "127.0.0.1", "203.0.113.9", "example.test")
        .expect("dispatch");

    // Drive the non-blocking client until the response lands. The whole point
    // is that this takes many iterations: credit arrives over time.
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut response = None;
    while Instant::now() < deadline {
        conn.drive();
        match rx_resp.try_recv() {
            Ok(r) => {
                response = Some(r);
                break;
            }
            Err(mpsc::TryRecvError::Empty) => thread::sleep(Duration::from_millis(1)),
            Err(mpsc::TryRecvError::Disconnected) => break,
        }
        if conn.is_dead {
            break;
        }
    }

    let observed = rx
        .recv_timeout(Duration::from_secs(20))
        .expect("stub origin reported nothing");
    origin.join().expect("stub origin panicked");

    assert_eq!(
        observed.overrun, 0,
        "client sent {} bytes beyond the credit the peer had granted \
         (advertised stream window {STUB_INITIAL_WINDOW}); send-side flow \
         control is not being honoured",
        observed.overrun
    );
    assert!(
        observed.stalls > 1,
        "the stub never actually withheld credit ({} stalls), so this run \
         proves nothing about flow control",
        observed.stalls
    );
    assert_eq!(
        observed.body.len(),
        BODY_LEN,
        "body truncated: the peer received {} of {BODY_LEN} bytes. A body \
         shorter than its content-length is the smuggling primitive that \
         skipping a credit-less write would manufacture",
        observed.body.len()
    );
    assert!(
        observed.body == sent,
        "body corrupted or reordered in transit despite arriving at full length"
    );

    let response = response.expect("no response from the stub origin");
    let response = response.expect("stream failed");
    assert_eq!(response.status, 200);
}
