//! TLS session resumption actually resumes.
//!
//! # Why this test exists, and why it is driven by rustls rather than by a tool
//!
//! `make_tls_server_config` took rustls' defaults, and rustls' defaults produce
//! a server that never resumes under real traffic:
//!
//!     ticketer:        NeverProducesTickets          (rustls server/builder.rs)
//!     session_storage: ServerSessionMemoryCache(256)
//!
//! With no ticketer, resumption is stateful and every returning client needs
//! its entry still to be in a 256-entry cache. A node taking hundreds of
//! handshakes an hour evicts them long before anyone comes back, so every
//! connection paid a full handshake: two extra round trips, on every visitor,
//! on every node.
//!
//! # The tooling trap this file is the answer to
//!
//! The defect was first "confirmed" with `openssl s_client -sess_out/-sess_in`
//! against production, which reported `New` rather than `Reused`. That evidence
//! was worthless: macOS ships **LibreSSL 3.3.6** as `/usr/bin/openssl`, whose
//! TLS 1.3 client-side resumption is incomplete, and the system `python3` links
//! LibreSSL 2.8.3 which has no TLS 1.3 at all. Both report a full handshake
//! whatever the server does.
//!
//! So the check is made here, with rustls on both ends, where
//! `handshake_kind()` is the protocol's own answer rather than a client's
//! summary of it. A test that cannot measure must fail, not report reassurance.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;

use rustls::{ClientConnection, HandshakeKind, ServerConnection, StreamOwned};

use m6_http_lib::http11::make_tls_server_config;

/// rustls needs a process-level provider before any config is built, and
/// `m6-http` installs `ring` at startup. Tests in one binary share a process, so
/// this runs once and the second caller is a no-op.
fn install_provider() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

/// A self-signed certificate and key on disk, for `make_tls_server_config` to
/// read. It takes paths, which is the interface production uses, so the test
/// exercises the same function rather than a re-implementation of it.
fn cert_and_key(dir: &tempfile::TempDir) -> (String, String) {
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_string()])
        .expect("generate self-signed certificate");
    let cert_path = dir.path().join("c.pem");
    let key_path = dir.path().join("k.pem");
    std::fs::write(&cert_path, cert.cert.pem()).expect("write cert");
    std::fs::write(&key_path, cert.key_pair.serialize_pem()).expect("write key");
    (
        cert_path.to_string_lossy().into_owned(),
        key_path.to_string_lossy().into_owned(),
    )
}

/// Trust exactly the certificate the server is using, and nothing else.
fn client_config(cert_pem_path: &str) -> Arc<rustls::ClientConfig> {
    use rustls_pki_types::pem::PemObject;
    use rustls_pki_types::CertificateDer;

    let mut roots = rustls::RootCertStore::empty();
    for c in CertificateDer::pem_file_iter(cert_pem_path).expect("read cert") {
        roots.add(c.expect("parse cert")).expect("add root");
    }
    Arc::new(
        rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth(),
    )
}

/// One connection, driven to a completed handshake, returning what kind it was.
///
/// The body exchange is deliberate: a TLS 1.3 server sends its session ticket
/// AFTER the handshake completes, so a client that connects and hangs up
/// immediately never receives one and the next connection has nothing to resume
/// from. That is a real way to write this test and have it pass for the wrong
/// reason.
fn connect_once(
    port: u16,
    config: Arc<rustls::ClientConfig>,
) -> (HandshakeKind, Arc<rustls::ClientConfig>) {
    let server_name = "localhost".try_into().expect("server name");
    let conn = ClientConnection::new(config.clone(), server_name).expect("client connection");
    let sock = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    let mut tls = StreamOwned::new(conn, sock);

    tls.write_all(b"ping").expect("write");
    tls.flush().expect("flush");
    let mut buf = [0u8; 4];
    tls.read_exact(&mut buf).expect("read the reply");
    assert_eq!(&buf, b"pong");

    // Read once more so the post-handshake NewSessionTicket is processed before
    // the connection goes away. The server closes, so this returns 0 bytes.
    let mut sink = Vec::new();
    let _ = tls.read_to_end(&mut sink);

    let kind = tls.conn.handshake_kind().expect("handshake completed");
    (kind, config)
}

#[test]
fn a_returning_client_resumes_rather_than_handshaking_again() {
    install_provider();
    let dir = tempfile::tempdir().expect("tempdir");
    let (cert_path, key_path) = cert_and_key(&dir);

    let server_config = make_tls_server_config(&cert_path, &key_path).expect("server config");

    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind");
    let port = listener.local_addr().expect("addr").port();

    // Two connections, served in turn on this thread's own listener.
    let server = std::thread::spawn(move || {
        let mut kinds = Vec::new();
        for _ in 0..2 {
            let (sock, _) = listener.accept().expect("accept");
            let conn = ServerConnection::new(server_config.clone()).expect("server connection");
            let mut tls = StreamOwned::new(conn, sock);
            let mut buf = [0u8; 4];
            if tls.read_exact(&mut buf).is_err() {
                continue;
            }
            tls.write_all(b"pong").expect("write");
            tls.flush().expect("flush");
            // Send the ticket and let the client read it before closing.
            tls.conn.send_close_notify();
            let _ = tls.flush();
            kinds.push(tls.conn.handshake_kind());
        }
        kinds
    });

    // ONE client config across both connections. Resumption is a property of a
    // client that remembers, and rustls keeps its session store on the config;
    // a fresh config per connection could never resume and the test would pass
    // or fail for reasons having nothing to do with the server.
    let config = client_config(&cert_path);
    let (first, config) = connect_once(port, config);
    let (second, _) = connect_once(port, config);

    let server_kinds = server.join().expect("server thread");

    assert_eq!(
        first,
        HandshakeKind::Full,
        "the first connection has nothing to resume from, so it must be a full handshake"
    );
    assert_eq!(
        second,
        HandshakeKind::Resumed,
        "the second connection had a ticket and must have resumed. \
         Before the ticketer was configured this was Full, which is two extra \
         round trips on every visitor. server saw: {server_kinds:?}"
    );
}

/// The server must actually issue a ticket, which is the half that was missing.
///
/// Separate from the test above so a failure says which end is wrong: a server
/// that issues no ticket and a client that ignores one both show up there as
/// `Full`.
#[test]
fn the_server_offers_a_ticketer_at_all() {
    install_provider();
    let dir = tempfile::tempdir().expect("tempdir");
    let (cert_path, key_path) = cert_and_key(&dir);
    let config = make_tls_server_config(&cert_path, &key_path).expect("server config");

    assert!(
        config.ticketer.enabled(),
        "no ticketer: rustls defaults to NeverProducesTickets, so resumption \
         falls back to a 256-entry server-side cache that a busy node evicts \
         before anyone returns"
    );
}
