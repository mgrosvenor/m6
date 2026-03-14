/// Functional integration tests for m6-http protocol handling.
///
/// HTTP/1.1: spins up Http11Listener in-process with a self-signed cert (rcgen),
///           connects with a raw TLS client, verifies request parsing and response.
///
/// HTTP/3: spins up a quiche QUIC server in-process on UDP loopback,
///         connects with a quiche client, verifies H3 request/response.

use std::io::{Read, Write};
use std::net::{TcpStream, UdpSocket};
use std::sync::{Arc, atomic::{AtomicBool, Ordering}};
use std::thread;
use std::time::Duration;

use m6_http_lib::http11::{Http11Listener, make_tls_server_config};
use m6_http_lib::forward::HttpRequest;
use quiche::h3::NameValue as _;

// ── Helpers ───────────────────────────────────────────────────────────────────

struct TestCert {
    cert_pem: String,
    key_pem: String,
    cert_der: Vec<u8>,
}

/// Generate a self-signed cert+key for localhost using rcgen.
fn generate_test_cert() -> TestCert {
    let ck = rcgen::generate_simple_self_signed(
        vec!["localhost".to_string(), "127.0.0.1".to_string()]
    ).expect("rcgen cert generation failed");
    let cert_der = ck.cert.der().to_vec();
    TestCert {
        cert_pem: ck.cert.pem(),
        key_pem: ck.key_pair.serialize_pem(),
        cert_der,
    }
}

/// Write cert+key PEM strings to temp files; return (cert_file, key_file).
fn write_pem_files(tc: &TestCert) -> (tempfile::NamedTempFile, tempfile::NamedTempFile) {
    use std::io::Write as _;
    let mut cert_file = tempfile::NamedTempFile::new().unwrap();
    cert_file.write_all(tc.cert_pem.as_bytes()).unwrap();
    let mut key_file = tempfile::NamedTempFile::new().unwrap();
    key_file.write_all(tc.key_pem.as_bytes()).unwrap();
    (cert_file, key_file)
}

/// Build a rustls ClientConfig that trusts the given DER certificate.
fn make_test_client_config(cert_der: &[u8]) -> Arc<rustls::ClientConfig> {
    let cert = rustls::pki_types::CertificateDer::from(cert_der.to_vec());
    let mut root_store = rustls::RootCertStore::empty();
    root_store.add(cert).expect("add test cert to root store");
    Arc::new(
        rustls::ClientConfig::builder()
            .with_root_certificates(root_store)
            .with_no_client_auth()
    )
}

// ── HTTP/1.1 Tests ────────────────────────────────────────────────────────────

/// Start Http11Listener on a random port. Returns (port, stop_flag).
/// The background thread calls accept_pending + drive_all with `handler` until `stop` is set.
fn start_http11_server<F>(
    cert_path: &str,
    key_path: &str,
    handler: F,
) -> (u16, Arc<AtomicBool>)
where
    F: Fn(&HttpRequest, &str) -> (u16, Vec<(String, String)>, Vec<u8>, String) + Send + 'static,
{
    let tls_cfg = make_tls_server_config(cert_path, key_path).expect("tls config");
    let mut listener = Http11Listener::bind("127.0.0.1:0", tls_cfg).expect("bind");
    let port = listener.local_addr().unwrap().port();
    let stop = Arc::new(AtomicBool::new(false));
    let stop2 = Arc::clone(&stop);

    thread::spawn(move || {
        while !stop2.load(Ordering::Relaxed) {
            listener.accept_pending();
            listener.drive_all(|req, client_ip| handler(req, client_ip));
            thread::sleep(Duration::from_millis(2));
        }
    });

    (port, stop)
}

/// Send a raw HTTP/1.1 request over TLS to localhost:port.
/// Returns the full raw response bytes.
fn http11_request(port: u16, client_cfg: Arc<rustls::ClientConfig>, request_bytes: &[u8]) -> Vec<u8> {
    let stream = TcpStream::connect(format!("127.0.0.1:{}", port))
        .expect("tcp connect");
    stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    stream.set_write_timeout(Some(Duration::from_secs(5))).unwrap();

    let server_name = rustls::pki_types::ServerName::try_from("localhost").unwrap().to_owned();
    let mut conn = rustls::ClientConnection::new(client_cfg, server_name).unwrap();
    let mut stream_ref = &stream;
    let mut tls = rustls::Stream::new(&mut conn, &mut stream_ref);

    tls.write_all(request_bytes).expect("write request");
    tls.flush().ok();

    let mut response = Vec::new();
    let _ = tls.read_to_end(&mut response); // will return Err on close-notify, that's fine
    response
}

#[test]
fn http11_get_returns_200_with_body() {
    let tc = generate_test_cert();
    let (cert_file, key_file) = write_pem_files(&tc);
    let client_cfg = make_test_client_config(&tc.cert_der);

    let (port, stop) = start_http11_server(
        cert_file.path().to_str().unwrap(),
        key_file.path().to_str().unwrap(),
        |_req, _ip| {
            (200,
             vec![("content-type".to_string(), "text/plain".to_string())],
             b"hello world".to_vec(),
             "test".to_string())
        },
    );
    // Give server time to start
    thread::sleep(Duration::from_millis(20));

    let request = b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
    let response = http11_request(port, client_cfg, request);
    let response_str = String::from_utf8_lossy(&response);

    stop.store(true, Ordering::Relaxed);

    assert!(response_str.starts_with("HTTP/1.1 200 OK"), "expected 200, got: {}", &response_str[..response_str.len().min(200)]);
    assert!(response_str.ends_with("hello world"), "expected body 'hello world', got: {}", &response_str[response_str.len().saturating_sub(50)..]);
}

#[test]
fn http11_path_and_headers_are_parsed() {
    let tc = generate_test_cert();
    let (cert_file, key_file) = write_pem_files(&tc);
    let client_cfg = make_test_client_config(&tc.cert_der);

    let (port, stop) = start_http11_server(
        cert_file.path().to_str().unwrap(),
        key_file.path().to_str().unwrap(),
        |req, _ip| {
            let body = format!("path={} method={}", req.path, req.method).into_bytes();
            (200, vec![], body, "test".to_string())
        },
    );
    thread::sleep(Duration::from_millis(20));

    let request = b"GET /some/path HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
    let response = http11_request(port, Arc::clone(&client_cfg), request);
    let response_str = String::from_utf8_lossy(&response);

    stop.store(true, Ordering::Relaxed);

    assert!(response_str.contains("path=/some/path"), "path not parsed: {}", response_str);
    assert!(response_str.contains("method=GET"), "method not parsed: {}", response_str);
}

#[test]
fn http11_connection_close_after_response() {
    let tc = generate_test_cert();
    let (cert_file, key_file) = write_pem_files(&tc);
    let client_cfg = make_test_client_config(&tc.cert_der);

    let (port, stop) = start_http11_server(
        cert_file.path().to_str().unwrap(),
        key_file.path().to_str().unwrap(),
        |_req, _ip| (204, vec![], vec![], "test".to_string()),
    );
    thread::sleep(Duration::from_millis(20));

    let request = b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
    let response = http11_request(port, client_cfg, request);
    let response_str = String::from_utf8_lossy(&response);

    stop.store(true, Ordering::Relaxed);

    assert!(response_str.contains("HTTP/1.1 204"), "expected 204");
    assert!(response_str.to_ascii_lowercase().contains("connection: close"), "expected Connection: close");
}

#[test]
fn http11_version_field_is_http11() {
    let tc = generate_test_cert();
    let (cert_file, key_file) = write_pem_files(&tc);
    let client_cfg = make_test_client_config(&tc.cert_der);

    let (port, stop) = start_http11_server(
        cert_file.path().to_str().unwrap(),
        key_file.path().to_str().unwrap(),
        |req, _ip| {
            let body = req.version.clone().into_bytes();
            (200, vec![], body, "test".to_string())
        },
    );
    thread::sleep(Duration::from_millis(20));

    let request = b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
    let response = http11_request(port, client_cfg, request);
    let response_str = String::from_utf8_lossy(&response);

    stop.store(true, Ordering::Relaxed);

    assert!(response_str.ends_with("HTTP/1.1"), "expected version=HTTP/1.1 in body, got: {}", response_str);
}

// ── HTTP/3 Tests ──────────────────────────────────────────────────────────────
//
// Spins up a quiche QUIC server + H3 stack in one thread, connects from another
// thread using a quiche client, exchanges a single GET /ping → 200 "pong".

#[test]
fn http3_get_returns_200_with_body() {
    // Generate quiche TLS config (using self-signed cert)
    let tc = generate_test_cert();
    let (cert_file, key_file) = write_pem_files(&tc);

    let server_udp = UdpSocket::bind("127.0.0.1:0").unwrap();
    server_udp.set_nonblocking(true).unwrap();
    let server_addr = server_udp.local_addr().unwrap();

    let stop = Arc::new(AtomicBool::new(false));
    let stop2 = Arc::clone(&stop);

    // Server thread
    let cert_path = cert_file.path().to_str().unwrap().to_string();
    let key_path  = key_file.path().to_str().unwrap().to_string();
    let server_handle = thread::spawn(move || {
        run_h3_server_once(server_udp, &cert_path, &key_path, stop2)
    });

    // Give server a moment to start
    thread::sleep(Duration::from_millis(50));

    // Client
    let result = run_h3_client_once(server_addr);

    stop.store(true, Ordering::Relaxed);
    server_handle.join().unwrap();

    assert!(result.is_ok(), "HTTP/3 client error: {:?}", result.err());
    let (status, body) = result.unwrap();
    assert_eq!(status, 200u16, "expected HTTP/3 status 200");
    assert_eq!(body, b"pong", "expected body 'pong'");
}

// ── quiche loopback helpers ───────────────────────────────────────────────────

fn quic_flush(conn: &mut quiche::Connection, udp: &UdpSocket, out: &mut [u8]) {
    loop {
        match conn.send(out) {
            Ok((written, info)) => { let _ = udp.send_to(&out[..written], info.to); }
            Err(quiche::Error::Done) => break,
            Err(_) => break,
        }
    }
}

// ── quiche server (single request) ───────────────────────────────────────────

fn run_h3_server_once(udp: UdpSocket, cert_path: &str, key_path: &str, stop: Arc<AtomicBool>) {
    let mut config = quiche::Config::new(quiche::PROTOCOL_VERSION).unwrap();
    config.load_cert_chain_from_pem_file(cert_path).unwrap();
    config.load_priv_key_from_pem_file(key_path).unwrap();
    config.set_application_protos(quiche::h3::APPLICATION_PROTOCOL).unwrap();
    config.set_max_idle_timeout(5_000);
    config.set_initial_max_data(1_000_000);
    config.set_initial_max_stream_data_bidi_local(1_000_000);
    config.set_initial_max_stream_data_bidi_remote(1_000_000);
    config.set_initial_max_stream_data_uni(1_000_000);
    config.set_initial_max_streams_bidi(10);
    config.set_initial_max_streams_uni(10);
    config.grease(false);
    config.verify_peer(false);

    let mut buf = vec![0u8; 65536];
    let mut out = vec![0u8; 1350];
    let mut conn: Option<quiche::Connection> = None;
    let mut h3_conn: Option<quiche::h3::Connection> = None;
    let mut responded = false;

    let local = udp.local_addr().unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);

    loop {
        if std::time::Instant::now() > deadline { break; }
        if stop.load(Ordering::Relaxed) { break; }

        // Always call on_timeout + flush (handles retransmits etc.)
        if let Some(ref mut c) = conn {
            c.on_timeout();
            quic_flush(c, &udp, &mut out);
        }

        // Receive available packets
        loop {
            match udp.recv_from(&mut buf) {
                Ok((len, from)) => {
                    let pkt = &mut buf[..len];
                    if conn.is_none() {
                        let hdr = match quiche::Header::from_slice(pkt, quiche::MAX_CONN_ID_LEN) {
                            Ok(h) => h,
                            Err(_) => break,
                        };
                        let scid = quiche::ConnectionId::from_ref(hdr.dcid.as_ref());
                        conn = Some(quiche::accept(&scid, None, local, from, &mut config).unwrap());
                    }
                    let c = conn.as_mut().unwrap();
                    let recv_info = quiche::RecvInfo { from, to: local };
                    let _ = c.recv(pkt, recv_info);

                    if c.is_established() && h3_conn.is_none() {
                        let h3cfg = quiche::h3::Config::new().unwrap();
                        h3_conn = Some(quiche::h3::Connection::with_transport(c, &h3cfg).unwrap());
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(_) => return,
            }
        }

        // Drive H3
        if let (Some(ref mut h3), Some(ref mut c)) = (&mut h3_conn, &mut conn) {
            loop {
                match h3.poll(c) {
                    Ok((stream_id, quiche::h3::Event::Headers { .. })) => {
                        if !responded {
                            let headers = vec![
                                quiche::h3::Header::new(b":status", b"200"),
                                quiche::h3::Header::new(b"content-length", b"4"),
                            ];
                            let _ = h3.send_response(c, stream_id, &headers, false);
                            let _ = h3.send_body(c, stream_id, b"pong", true);
                            responded = true;
                        }
                    }
                    Ok((_, quiche::h3::Event::Finished)) => {}
                    Err(quiche::h3::Error::Done) => break,
                    Err(_) => break,
                    Ok(_) => {}
                }
            }
        }

        // Flush after H3 work
        if let Some(ref mut c) = conn {
            quic_flush(c, &udp, &mut out);
        }

        if responded && conn.as_ref().map(|c| c.is_closed()).unwrap_or(false) {
            break;
        }

        thread::sleep(Duration::from_millis(1));
    }
}

// ── quiche client (single request) ───────────────────────────────────────────

fn run_h3_client_once(server_addr: std::net::SocketAddr) -> Result<(u16, Vec<u8>), String> {
    let client_udp = UdpSocket::bind("127.0.0.1:0").map_err(|e| e.to_string())?;
    client_udp.set_nonblocking(true).unwrap();
    let local = client_udp.local_addr().unwrap();

    let mut config = quiche::Config::new(quiche::PROTOCOL_VERSION).map_err(|e| e.to_string())?;
    config.set_application_protos(quiche::h3::APPLICATION_PROTOCOL).map_err(|e| e.to_string())?;
    config.set_max_idle_timeout(5_000);
    config.set_initial_max_data(1_000_000);
    config.set_initial_max_stream_data_bidi_local(1_000_000);
    config.set_initial_max_stream_data_bidi_remote(1_000_000);
    config.set_initial_max_stream_data_uni(1_000_000);
    config.set_initial_max_streams_bidi(10);
    config.set_initial_max_streams_uni(10);
    config.grease(false);
    config.verify_peer(false);

    let scid_bytes = [7u8; quiche::MAX_CONN_ID_LEN];
    let scid = quiche::ConnectionId::from_ref(&scid_bytes);
    let mut conn = quiche::connect(Some("localhost"), &scid, local, server_addr, &mut config)
        .map_err(|e| format!("quiche::connect: {e}"))?;

    let mut h3_conn: Option<quiche::h3::Connection> = None;
    let mut buf = vec![0u8; 65536];
    let mut out = vec![0u8; 1350];
    let mut request_sent = false;
    let mut status: Option<u16> = None;
    let mut body: Vec<u8> = Vec::new();
    let mut done = false;

    let deadline = std::time::Instant::now() + Duration::from_secs(5);

    loop {
        if std::time::Instant::now() > deadline {
            return Err(format!(
                "HTTP/3 client timeout (established={} h3={} req_sent={} closed={})",
                conn.is_established(), h3_conn.is_some(), request_sent, conn.is_closed(),
            ));
        }

        // Check for connection error
        if let Some(e) = conn.peer_error() {
            return Err(format!("peer_error: code={} reason={:?}", e.error_code, String::from_utf8_lossy(&e.reason)));
        }
        if let Some(e) = conn.local_error() {
            return Err(format!("local_error: code={} reason={:?}", e.error_code, String::from_utf8_lossy(&e.reason)));
        }

        // on_timeout + flush
        conn.on_timeout();
        quic_flush(&mut conn, &client_udp, &mut out);

        // Recv
        loop {
            match client_udp.recv_from(&mut buf) {
                Ok((len, from)) => {
                    let recv_info = quiche::RecvInfo { from, to: local };
                    let _ = conn.recv(&mut buf[..len], recv_info);
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) => return Err(format!("recv: {e}")),
            }
        }

        // Establish H3
        if conn.is_established() && h3_conn.is_none() {
            let h3cfg = quiche::h3::Config::new().map_err(|e| format!("h3::Config::new: {e}"))?;
            h3_conn = Some(quiche::h3::Connection::with_transport(&mut conn, &h3cfg)
                .map_err(|e| format!("h3::with_transport: {e}"))?);
        }

        if let Some(ref mut h3) = h3_conn {
            // Send request once
            if !request_sent {
                let headers = vec![
                    quiche::h3::Header::new(b":method", b"GET"),
                    quiche::h3::Header::new(b":path", b"/ping"),
                    quiche::h3::Header::new(b":scheme", b"https"),
                    quiche::h3::Header::new(b":authority", b"localhost"),
                ];
                match h3.send_request(&mut conn, &headers, true) {
                    Ok(_) => { request_sent = true; }
                    Err(quiche::h3::Error::Done) => {}
                    Err(e) => return Err(format!("send_request: {e}")),
                }
            }

            // Poll responses
            loop {
                match h3.poll(&mut conn) {
                    Ok((_, quiche::h3::Event::Headers { list, .. })) => {
                        for h in &list {
                            if h.name() == b":status" {
                                let s = std::str::from_utf8(h.value()).unwrap_or("0");
                                status = s.parse().ok();
                            }
                        }
                    }
                    Ok((stream_id, quiche::h3::Event::Data)) => {
                        let mut tmp = [0u8; 4096];
                        loop {
                            match h3.recv_body(&mut conn, stream_id, &mut tmp) {
                                Ok(0) => break,
                                Ok(n) => body.extend_from_slice(&tmp[..n]),
                                Err(_) => break,
                            }
                        }
                    }
                    Ok((_, quiche::h3::Event::Finished)) => { done = true; }
                    Err(quiche::h3::Error::Done) => break,
                    Err(e) => return Err(format!("h3.poll: {e}")),
                    Ok(_) => {}
                }
            }
        }

        // Flush after H3 work
        quic_flush(&mut conn, &client_udp, &mut out);

        if done || conn.is_closed() { break; }

        thread::sleep(Duration::from_millis(1));
    }

    let s = status.ok_or_else(|| "no :status received".to_string())?;
    Ok((s, body))
}
