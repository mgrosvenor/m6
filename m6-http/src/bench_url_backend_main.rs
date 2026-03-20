/// bench-url-backend — minimal HTTP server for URL-backend benchmarking.
///
/// Listens on a TCP port and responds to every GET with a fixed 200 OK body.
/// Supports all four outbound protocols that m6-http can forward to:
///
///   --proto http    HTTP/1.1 plain TCP  (one conn per request)
///   --proto https   HTTP/1.1 over TLS  (one conn per request)
///   --proto h2c     HTTP/2 cleartext   (multiplexed persistent conn)
///   --proto h2s     HTTP/2 over TLS   (multiplexed persistent conn)
///   --addr  HOST:PORT  (required)
///   --cert  CERT.PEM   (required for https/h2s)
///   --key   KEY.PEM    (required for https/h2s)
///
/// All responses carry Cache-Control: no-store so m6-http forwards every
/// request to the backend — ensuring the benchmark measures steady-state
/// forwarding cost, not the cache-hit path.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::Arc;

// ── Fixed response ────────────────────────────────────────────────────────────

const RESP_BODY: &[u8] = b"<!doctype html>\n<html><head><meta charset=\"utf-8\"><title>bench</title></head>\n<body><h1>m6-bench</h1><p>ok</p></body>\n</html>\n";

// ── CLI ───────────────────────────────────────────────────────────────────────

struct Args {
    proto: String,
    addr:  String,
    cert:  String,
    key:   String,
}

fn parse_args() -> Args {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let mut a = Args {
        proto: String::new(),
        addr:  String::new(),
        cert:  "cert.pem".into(),
        key:   "key.pem".into(),
    };
    let mut i = 0;
    while i < raw.len() {
        match raw[i].as_str() {
            "--proto" => { i += 1; a.proto = raw[i].clone(); }
            "--addr"  => { i += 1; a.addr  = raw[i].clone(); }
            "--cert"  => { i += 1; a.cert  = raw[i].clone(); }
            "--key"   => { i += 1; a.key   = raw[i].clone(); }
            other     => { eprintln!("unknown flag: {other}"); std::process::exit(1); }
        }
        i += 1;
    }
    if a.proto.is_empty() || a.addr.is_empty() {
        eprintln!("usage: bench-url-backend --proto <http|https|h2c|h2s> --addr HOST:PORT [--cert C --key K]");
        std::process::exit(1);
    }
    a
}

// ── TLS server config ─────────────────────────────────────────────────────────

fn make_tls_server_config(cert: &str, key: &str, h2: bool) -> Arc<rustls::ServerConfig> {
    use rustls_pemfile::{certs, private_key};
    use std::fs::File;
    use std::io::BufReader;

    let certs: Vec<_> = certs(&mut BufReader::new(File::open(cert).expect("cert")))
        .collect::<Result<_, _>>()
        .expect("parse cert");
    let pkey = private_key(&mut BufReader::new(File::open(key).expect("key")))
        .expect("parse key")
        .expect("no key");

    let mut cfg = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, pkey)
        .expect("TLS config");

    cfg.alpn_protocols = if h2 {
        vec![b"h2".to_vec()]
    } else {
        vec![b"http/1.1".to_vec()]
    };
    Arc::new(cfg)
}

// ── HTTP/1.1 connection handler ───────────────────────────────────────────────

fn handle_h1_conn(mut stream: impl Read + Write) {
    let mut buf = vec![0u8; 8192];
    let mut total = 0usize;

    // Read until we have the full request headers (double CRLF).
    loop {
        match stream.read(&mut buf[total..]) {
            Ok(0) | Err(_) => return,
            Ok(n) => total += n,
        }
        if buf[..total].windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
        if total == buf.len() {
            buf.extend_from_slice(&vec![0u8; 8192]);
        }
    }

    // Check for Content-Length and read any body (bench GETs have none).
    let hdrs = &buf[..total];
    let body_start = hdrs.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
    let cl: usize = std::str::from_utf8(hdrs).unwrap_or("")
        .lines()
        .find(|l| l.to_ascii_lowercase().starts_with("content-length:"))
        .and_then(|l| l.splitn(2, ':').nth(1))
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0);
    let body_read = total - body_start;
    if cl > body_read {
        let mut body_rest = vec![0u8; cl - body_read];
        let _ = stream.read_exact(&mut body_rest);
    }

    let hdr = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\n\
         Cache-Control: no-store\r\nConnection: close\r\n\r\n",
        RESP_BODY.len()
    );
    let _ = stream.write_all(hdr.as_bytes());
    let _ = stream.write_all(RESP_BODY);
    let _ = stream.flush();
}

// ── HTTP/2 connection handler ─────────────────────────────────────────────────
//
// Minimal server-side H2 framing.  Handles the single-connection multiplexed
// stream model used by H2cClientPool and H2sTlsClientPool.

const H2_PREFACE: &[u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";
const FT_DATA:         u8 = 0x0;
const FT_HEADERS:      u8 = 0x1;
const FT_SETTINGS:     u8 = 0x4;
const FT_PING:         u8 = 0x6;
const FT_GOAWAY:       u8 = 0x7;
const FT_WINDOW_UPDATE:u8 = 0x8;
const FL_END_STREAM:   u8 = 0x1;
const FL_END_HEADERS:  u8 = 0x4;
const FL_ACK:          u8 = 0x1;

fn push_frame(buf: &mut Vec<u8>, ftype: u8, flags: u8, sid: u32, payload: &[u8]) {
    let len = payload.len() as u32;
    buf.push((len >> 16) as u8);
    buf.push((len >> 8) as u8);
    buf.push(len as u8);
    buf.push(ftype);
    buf.push(flags);
    buf.push(((sid >> 24) & 0x7f) as u8);
    buf.push((sid >> 16) as u8);
    buf.push((sid >> 8) as u8);
    buf.push(sid as u8);
    buf.extend_from_slice(payload);
}

fn handle_h2_conn(mut stream: impl Read + Write) {
    // Read and validate client preface.
    let mut preface_buf = [0u8; H2_PREFACE.len()];
    if stream.read_exact(&mut preface_buf).is_err() { return; }
    if &preface_buf != H2_PREFACE { return; }

    // Send server SETTINGS (empty — use defaults).
    let mut send_buf: Vec<u8> = Vec::with_capacity(4096);
    push_frame(&mut send_buf, FT_SETTINGS, 0, 0, &[]);
    if stream.write_all(&send_buf).is_err() { return; }
    if stream.flush().is_err() { return; }
    send_buf.clear();

    let mut recv_buf: Vec<u8> = Vec::with_capacity(65536);
    let mut tmp = [0u8; 16384];
    let mut hpack_dec = hpack::Decoder::new();
    let mut hpack_enc = hpack::Encoder::new();
    // Track streams that have been requested but not yet responded to.
    // key = stream_id, value = header block bytes (we don't use them but need to decode)
    let mut pending: Vec<u32> = Vec::new();

    loop {
        // ── Flush pending sends ───────────────────────────────────────────────
        if !send_buf.is_empty() {
            if stream.write_all(&send_buf).is_err() { return; }
            if stream.flush().is_err() { return; }
            send_buf.clear();
        }

        // ── Read more data ────────────────────────────────────────────────────
        match stream.read(&mut tmp) {
            Ok(0) | Err(_) => return,
            Ok(n) => recv_buf.extend_from_slice(&tmp[..n]),
        }

        // ── Process frames ────────────────────────────────────────────────────
        loop {
            if recv_buf.len() < 9 { break; }

            let length = ((recv_buf[0] as usize) << 16)
                | ((recv_buf[1] as usize) << 8)
                | (recv_buf[2] as usize);

            if recv_buf.len() < 9 + length { break; }

            let ftype = recv_buf[3];
            let flags = recv_buf[4];
            let sid   = (((recv_buf[5] as u32) << 24)
                | ((recv_buf[6] as u32) << 16)
                | ((recv_buf[7] as u32) << 8)
                | (recv_buf[8] as u32))
                & 0x7fff_ffff;
            let payload = recv_buf[9..9 + length].to_vec();
            recv_buf.drain(..9 + length);

            match ftype {
                FT_SETTINGS => {
                    if flags & FL_ACK == 0 {
                        // Client's SETTINGS — ACK it.
                        push_frame(&mut send_buf, FT_SETTINGS, FL_ACK, 0, &[]);
                    }
                    // If it's our SETTINGS ACK, nothing to do.
                }

                FT_HEADERS if sid > 0 => {
                    // Decode to keep HPACK state consistent; ignore the result.
                    let _ = hpack_dec.decode(&payload);

                    if flags & FL_END_HEADERS != 0 {
                        // For bench GET requests, END_HEADERS + END_STREAM arrive together.
                        if flags & FL_END_STREAM != 0 {
                            send_response(&mut send_buf, &mut hpack_enc, sid);
                        } else {
                            // Request has a body — queue the stream.
                            pending.push(sid);
                        }
                    }
                }

                FT_DATA if sid > 0 => {
                    if flags & FL_END_STREAM != 0 {
                        // Body complete — send response if this was a queued stream.
                        if let Some(pos) = pending.iter().position(|&s| s == sid) {
                            pending.remove(pos);
                            send_response(&mut send_buf, &mut hpack_enc, sid);
                        }
                    }
                }

                FT_WINDOW_UPDATE => {
                    // Ignore flow control for bench (responses fit in default window).
                }

                FT_PING => {
                    if flags & FL_ACK == 0 && payload.len() == 8 {
                        push_frame(&mut send_buf, FT_PING, FL_ACK, 0, &payload);
                    }
                }

                FT_GOAWAY => return,

                _ => {}
            }
        }
    }
}

fn send_response(send_buf: &mut Vec<u8>, enc: &mut hpack::Encoder, sid: u32) {
    let cl = RESP_BODY.len().to_string();
    let hdr_block = enc.encode(vec![
        (b":status".as_slice(),        b"200".as_slice()),
        (b"content-type",              b"text/html"),
        (b"content-length",            cl.as_bytes()),
        (b"cache-control",             b"no-store"),
    ]);
    push_frame(send_buf, FT_HEADERS, FL_END_HEADERS, sid, &hdr_block);
    push_frame(send_buf, FT_DATA, FL_END_STREAM, sid, RESP_BODY);
}

// ── main ──────────────────────────────────────────────────────────────────────

fn main() {
    rustls::crypto::ring::default_provider().install_default().ok();
    let args = parse_args();

    let tls_config = if args.proto == "https" || args.proto == "h2s" {
        Some(make_tls_server_config(&args.cert, &args.key, args.proto == "h2s"))
    } else {
        None
    };

    let listener = TcpListener::bind(&args.addr).expect("bind");
    eprintln!("bench-url-backend: {} on {}", args.proto, args.addr);

    for tcp in listener.incoming() {
        let tcp = match tcp { Ok(s) => s, Err(_) => continue };
        let proto  = args.proto.clone();
        let tls_cfg = tls_config.clone();

        std::thread::spawn(move || {
            match proto.as_str() {
                "http" => handle_h1_conn(tcp),
                "https" => {
                    let conn = match rustls::ServerConnection::new(tls_cfg.unwrap()) {
                        Ok(c) => c,
                        Err(_) => return,
                    };
                    let mut stream = rustls::StreamOwned::new(conn, tcp);
                    handle_h1_conn(&mut stream);
                }
                "h2c" => handle_h2_conn(tcp),
                "h2s" => {
                    let conn = match rustls::ServerConnection::new(tls_cfg.unwrap()) {
                        Ok(c) => c,
                        Err(_) => return,
                    };
                    let mut stream = rustls::StreamOwned::new(conn, tcp);
                    handle_h2_conn(&mut stream);
                }
                _ => {}
            }
        });
    }
}
