//! A certificate reload reaches every protocol, or it is not a reload.
//!
//! m6 #210. `handle_tls_reload` rebuilt the quiche configuration and nothing
//! else, so after a renewal one server answered with two different
//! certificates at the same time, chosen by the protocol the client
//! negotiated: HTTP/3 served the new one, HTTP/1.1 and HTTP/2 served the
//! replaced one until the process restarted. The log said `TLS config
//! reloaded`, which was true of the one protocol it had reloaded.
//!
//! The gap was found by reading the code and no test could see it, because
//! every TLS test built one configuration and never replaced it. So this file
//! asks the only question that distinguishes a reload from a partial one:
//! **after the certificate on disk changes, do all three protocols present the
//! same certificate, and is it the new one?**
//!
//! It is a process test rather than a unit test deliberately. The reload is
//! driven by a filesystem event and crosses the event loop, the rustls
//! listener and the quiche configuration, and a unit test of any one of those
//! is a test of the half that already worked.
//!
//! ```text
//! cargo build --workspace
//! cargo test -p m6-http --test tls_reload
//! ```

use std::io::{Read, Write};
use std::net::{TcpStream, UdpSocket};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use m6_core::testkit::{binary, claim_port, PortClaim, Service};

// ── Certificates ──────────────────────────────────────────────────────────────

/// A generated certificate, with the two facts the assertions need.
struct Cert {
    cert_pem: String,
    key_pem: String,
    /// The leaf, as the server will send it on the wire.
    der: Vec<u8>,
    not_after: i64,
}

/// A self-signed certificate for loopback, expiring on 1 January of `year`.
///
/// The year is a parameter so the two certificates in a test have **different**
/// expiries. Generated with one default validity window they would differ in
/// key and serial but agree on `notAfter`, and the assertion that the reported
/// expiry followed the renewal would pass without measuring anything.
fn cert_expiring_in(year: i32) -> Cert {
    let mut params =
        rcgen::CertificateParams::new(vec!["localhost".to_string(), "127.0.0.1".to_string()])
            .expect("certificate params");
    params.not_after = rcgen::date_time_ymd(year, 1, 1);
    let key_pair = rcgen::KeyPair::generate().expect("key pair");
    let cert = params.self_signed(&key_pair).expect("self-signed");
    let der = cert.der().to_vec();
    let not_after = m6_core::tls::not_after_unix(&der).expect("notAfter of a certificate we made");
    Cert {
        cert_pem: cert.pem(),
        key_pem: key_pair.serialize_pem(),
        der,
        not_after,
    }
}

// ── Clients, each reporting the leaf certificate it was served ────────────────

/// A client that trusts **both** certificates in the test.
///
/// Trusting only the expected one would turn "the server is still serving the
/// old certificate" into a handshake failure, and a failure cannot say which
/// certificate it was refusing. Trusting both makes every handshake succeed and
/// leaves `peer_certificates` to say what arrived, which is the measurement.
fn client_trusting_both(a: &Cert, b: &Cert, alpn: &[&[u8]]) -> Arc<rustls::ClientConfig> {
    rustls::crypto::ring::default_provider()
        .install_default()
        .ok();
    let mut store = rustls::RootCertStore::empty();
    for der in [&a.der, &b.der] {
        store
            .add(rustls::pki_types::CertificateDer::from(der.clone()))
            .expect("trust anchor");
    }
    let mut config = rustls::ClientConfig::builder()
        .with_root_certificates(store)
        .with_no_client_auth();
    config.alpn_protocols = alpn.iter().map(|p| p.to_vec()).collect();
    Arc::new(config)
}

/// Handshake over TCP and report the leaf certificate the server sent.
///
/// `alpn` selects the protocol, which is the whole point: the defect served a
/// different certificate per protocol, so the certificate has to be read once
/// per negotiated protocol and not once per connection.
fn tls_leaf(
    port: u16,
    tls: Arc<rustls::ClientConfig>,
    want_alpn: &[u8],
) -> Result<Vec<u8>, String> {
    let tcp = TcpStream::connect(("127.0.0.1", port)).map_err(|e| format!("connect: {e}"))?;
    tcp.set_read_timeout(Some(Duration::from_secs(10)))
        .map_err(|e| format!("read timeout: {e}"))?;
    let name = rustls::pki_types::ServerName::try_from("127.0.0.1".to_string())
        .map_err(|e| format!("server name: {e}"))?;
    let conn =
        rustls::ClientConnection::new(tls, name).map_err(|e| format!("client config: {e}"))?;
    let mut stream = rustls::StreamOwned::new(conn, tcp);

    // A request, because the handshake completes lazily on first use. `/health`
    // needs no backend and no credential, so a failure here is a TLS failure.
    let req =
        format!("GET /health HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n");
    stream
        .write_all(req.as_bytes())
        .map_err(|e| format!("write: {e}"))?;
    stream.flush().map_err(|e| format!("flush: {e}"))?;
    let mut sink = Vec::new();
    match stream.read_to_end(&mut sink) {
        Ok(_) => {}
        // The server closes without close_notify on `Connection: close`.
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {}
        Err(e) => return Err(format!("read: {e}")),
    }

    let negotiated = stream.conn.alpn_protocol().unwrap_or(b"").to_vec();
    if negotiated != want_alpn {
        return Err(format!(
            "negotiated {:?}, wanted {:?}; this measurement would be about the wrong protocol",
            String::from_utf8_lossy(&negotiated),
            String::from_utf8_lossy(want_alpn)
        ));
    }
    let chain = stream
        .conn
        .peer_certificates()
        .ok_or("the server sent no certificate")?;
    let leaf = chain.first().ok_or("the server sent an empty chain")?;
    Ok(leaf.as_ref().to_vec())
}

fn h1_leaf(port: u16, tls: Arc<rustls::ClientConfig>) -> Result<Vec<u8>, String> {
    tls_leaf(port, tls, b"http/1.1")
}

fn h2_leaf(port: u16, tls: Arc<rustls::ClientConfig>) -> Result<Vec<u8>, String> {
    tls_leaf(port, tls, b"h2")
}

/// Handshake over QUIC and report the leaf certificate the server sent.
///
/// `verify_peer(false)` because this client is measuring what arrived, not
/// deciding whether to trust it. `peer_cert` is populated either way.
fn h3_leaf(port: u16) -> Result<Vec<u8>, String> {
    let server: std::net::SocketAddr = format!("127.0.0.1:{port}").parse().expect("addr");
    let udp = UdpSocket::bind("127.0.0.1:0").map_err(|e| format!("bind: {e}"))?;
    udp.set_nonblocking(true)
        .map_err(|e| format!("nonblock: {e}"))?;
    let local = udp.local_addr().map_err(|e| format!("local addr: {e}"))?;

    let mut config =
        quiche::Config::new(quiche::PROTOCOL_VERSION).map_err(|e| format!("quiche config: {e}"))?;
    config
        .set_application_protos(quiche::h3::APPLICATION_PROTOCOL)
        .map_err(|e| format!("alpn: {e}"))?;
    config.set_max_idle_timeout(10_000);
    config.set_initial_max_data(1_000_000);
    config.set_initial_max_stream_data_bidi_local(100_000);
    config.set_initial_max_stream_data_bidi_remote(100_000);
    config.set_initial_max_streams_bidi(10);
    config.set_initial_max_streams_uni(10);
    config.grease(false);
    config.verify_peer(false);

    let scid_bytes = [7u8; quiche::MAX_CONN_ID_LEN];
    let scid = quiche::ConnectionId::from_ref(&scid_bytes);
    let mut conn = quiche::connect(Some("localhost"), &scid, local, server, &mut config)
        .map_err(|e| format!("connect: {e}"))?;

    let mut buf = vec![0u8; 65536];
    let mut out = vec![0u8; 1350];
    let deadline = Instant::now() + Duration::from_secs(20);

    while !conn.is_established() {
        if Instant::now() > deadline {
            return Err("quic handshake did not complete".to_string());
        }
        conn.on_timeout();
        while let Ok((n, info)) = conn.send(&mut out) {
            let _ = udp.send_to(&out[..n], info.to);
        }
        loop {
            match udp.recv_from(&mut buf) {
                Ok((n, from)) => {
                    let info = quiche::RecvInfo { from, to: local };
                    if conn.recv(&mut buf[..n], info).is_err() {
                        break;
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) => return Err(format!("recv: {e}")),
            }
        }
        if conn.is_closed() {
            return Err("quic connection closed before it was established".to_string());
        }
        std::thread::sleep(Duration::from_millis(2));
    }

    let leaf = conn
        .peer_cert()
        .ok_or("the server sent no certificate over QUIC")?
        .to_vec();
    Ok(leaf)
}

// ── The server under test ─────────────────────────────────────────────────────

struct Server {
    port: u16,
    token: String,
    site: std::path::PathBuf,
    layout: Layout,
    /// Which archive generation the lineage is on, so a renewal writes the next
    /// one rather than overwriting the file the link already points at.
    generation: std::cell::Cell<u32>,
    proc: Service,
    _dir: tempfile::TempDir,
    _claim: PortClaim,
}

impl Server {
    /// Write a certificate and key over the paths the running server watches.
    ///
    /// The key goes first. certbot writes a lineage then runs its hooks, and
    /// each file arriving is its own filesystem event, so a reload can catch a
    /// new certificate beside the previous key. That pair does not match and
    /// the rebuild fails, which must leave every protocol on the material it
    /// already had; the second write then fires another event and the reload
    /// succeeds. Ordering the writes this way exercises that sequence rather
    /// than hiding it.
    fn install(&self, cert: &Cert) {
        let gen = self.generation.get() + 1;
        self.generation.set(gen);
        write_material(&self.site, cert, self.layout, gen);
    }

    /// `notAfter` as `/perf` reports it, for the leaf.
    ///
    /// This is the half of #210 that defeated expiry reporting: the registry was
    /// written where the configuration was built, so a monitor kept reading the
    /// certificate loaded at startup however many times the material changed.
    ///
    /// Every failure carries its cause, because the subject of this file is a
    /// number nobody took being read as a number. A helper answering the same
    /// nothing to a refused connection, a 401, a body with no leaf in it and an
    /// unparsable digit makes the assertion below report an absent measurement
    /// as a wrong one.
    fn reported_not_after(&self, tls: Arc<rustls::ClientConfig>) -> Result<i64, String> {
        let tcp =
            TcpStream::connect(("127.0.0.1", self.port)).map_err(|e| format!("connect: {e}"))?;
        tcp.set_read_timeout(Some(Duration::from_secs(10)))
            .map_err(|e| format!("read timeout: {e}"))?;
        let name = rustls::pki_types::ServerName::try_from("127.0.0.1".to_string())
            .map_err(|e| format!("server name: {e}"))?;
        let conn =
            rustls::ClientConnection::new(tls, name).map_err(|e| format!("client config: {e}"))?;
        let mut stream = rustls::StreamOwned::new(conn, tcp);
        let req = format!(
            "GET /perf HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nAuthorization: Bearer {}\r\n\
             Connection: close\r\n\r\n",
            self.port, self.token
        );
        stream
            .write_all(req.as_bytes())
            .map_err(|e| format!("write: {e}"))?;
        stream.flush().map_err(|e| format!("flush: {e}"))?;
        let mut raw = Vec::new();
        match stream.read_to_end(&mut raw) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {}
            Err(e) => return Err(format!("read: {e}")),
        }
        let text = String::from_utf8_lossy(&raw);
        // Hand-scanned rather than parsed: the test asserts one number and
        // pulling in a JSON dependency to find it would be the larger change.
        // `"depth":0` is the leaf, and `not_after_unix` follows it.
        let body = text
            .split("\r\n\r\n")
            .nth(1)
            .ok_or_else(|| format!("/perf answered with no body: {text}"))?;
        let at = body
            .find("\"depth\":0")
            .ok_or_else(|| format!("/perf reported no leaf certificate: {body}"))?;
        let rest = &body[at..];
        let key = rest
            .find("\"not_after_unix\":")
            .ok_or_else(|| format!("/perf's leaf carries no notAfter: {rest}"))?
            + "\"not_after_unix\":".len();
        let digits: String = rest[key..]
            .chars()
            .take_while(|c| c.is_ascii_digit() || *c == '-')
            .collect();
        digits
            .parse()
            .map_err(|e| format!("notAfter {digits:?} is not a number: {e}"))
    }
}

/// Start m6-http serving `first`, with `/perf` reachable.
/// Where the server's configured certificate path actually points.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Layout {
    /// `cert.pem` and `key.pem`, written in place. The simplest shape, and the
    /// only one the first version of this file tested.
    Flat,
    /// `live/<name>/fullchain.pem` as a SYMLINK into `archive/`, which is the
    /// layout `docs/m6-site-toml.md` prescribes and the one certbot produces. A
    /// renewal writes a new archive file and repoints the link, so the inode
    /// behind the configured path changes and is never written through.
    CertbotLineage,
}

fn start(first: &Cert) -> Server {
    start_with(first, Layout::Flat)
}

fn start_with(first: &Cert, layout: Layout) -> Server {
    let dir = tempfile::tempdir().expect("tempdir");
    let site = dir.path().to_path_buf();

    let (cert_path, key_path) = write_material(&site, first, layout, 1);
    let token = "tls-reload-test-token".to_string();
    let token_file = site.join("perf-token");
    std::fs::write(&token_file, &token).expect("token file");

    // `[health]` is read from site.toml, not from the system config: config.rs
    // takes it from `site_parsed`. Put it in the wrong file and /perf answers
    // 404, because an unconfigured token means the endpoint does not exist.
    std::fs::write(
        site.join("site.toml"),
        format!(
            "[site]\nname = \"tls-reload\"\ndomain = \"localhost\"\n\n\
             [log]\nlevel = \"info\"\nformat = \"text\"\n\n\
             [analytics]\nenabled = false\n\n\
             [health]\nmetrics_token_file = \"{token_file}\"\n",
            token_file = token_file.display(),
        ),
    )
    .expect("site.toml");

    let claim = claim_port();
    let port = claim.port();
    std::fs::write(
        site.join("system.toml"),
        format!(
            "[server]\nbind = \"127.0.0.1:{port}\"\ntls_cert = \"{cert}\"\ntls_key = \"{key}\"\n\n\
             [node]\nname = \"tls-reload\"\n",
            cert = cert_path.display(),
            key = key_path.display(),
        ),
    )
    .expect("system.toml");

    let mut proc = Service::spawn(
        "m6-http",
        Command::new(binary("m6-http"))
            .arg(&site)
            .arg(site.join("system.toml")),
    );
    proc.wait_for_tcp(port, Duration::from_secs(20));

    Server {
        port,
        token,
        site,
        layout,
        generation: std::cell::Cell::new(1),
        proc,
        _dir: dir,
        _claim: claim,
    }
}

/// Put `cert` on disk in `layout`, and return the paths the server is configured
/// with.
///
/// For a lineage, `generation` names the archive file, and the symlink is
/// replaced by `rename` rather than removed and recreated. That is what certbot
/// does and it matters: a remove-then-create leaves a window with no
/// certificate at all, and `rename` is atomic, so a reader either sees the old
/// target or the new one.
fn write_material(
    site: &std::path::Path,
    cert: &Cert,
    layout: Layout,
    generation: u32,
) -> (std::path::PathBuf, std::path::PathBuf) {
    match layout {
        Layout::Flat => {
            let c = site.join("cert.pem");
            let k = site.join("key.pem");
            std::fs::write(site.join("key.pem"), &cert.key_pem).expect("write key");
            std::fs::write(&c, &cert.cert_pem).expect("write cert");
            (c, k)
        }
        Layout::CertbotLineage => {
            let archive = site.join("archive/node");
            let live = site.join("live/node");
            std::fs::create_dir_all(&archive).expect("archive dir");
            std::fs::create_dir_all(&live).expect("live dir");

            let ac = archive.join(format!("fullchain{generation}.pem"));
            let ak = archive.join(format!("privkey{generation}.pem"));
            std::fs::write(&ac, &cert.cert_pem).expect("write archive cert");
            std::fs::write(&ak, &cert.key_pem).expect("write archive key");

            let lc = live.join("fullchain.pem");
            let lk = live.join("privkey.pem");
            for (link, target) in [(&lc, &ac), (&lk, &ak)] {
                // Written beside the link and renamed over it, because
                // `symlink` fails on an existing path and removing it first
                // would leave a gap where the server has no certificate.
                let tmp = link.with_extension("pem.new");
                let _ = std::fs::remove_file(&tmp);
                std::os::unix::fs::symlink(target, &tmp).expect("symlink");
                std::fs::rename(&tmp, link).expect("rename symlink into place");
            }
            (lc, lk)
        }
    }
}

// ── The test ──────────────────────────────────────────────────────────────────

/// Every protocol serves the certificate that is on disk, before and after it
/// changes.
///
/// Written against the defect: on the code before #210 the poll below times out,
/// because HTTP/1.1 keeps presenting the first certificate for the life of the
/// process while HTTP/3 has already moved to the second. The failure message
/// names which protocol was left behind, since that is the fact a future
/// regression needs to state.
#[test]
fn a_reload_moves_http1_http2_and_http3_to_the_new_certificate() {
    let first = cert_expiring_in(2030);
    let second = cert_expiring_in(2035);
    assert_ne!(
        first.not_after, second.not_after,
        "the two certificates must expire on different days or the expiry assertion measures nothing"
    );

    let mut srv = start(&first);
    let h1_tls = || client_trusting_both(&first, &second, &[b"http/1.1"]);
    let h2_tls = || client_trusting_both(&first, &second, &[b"h2"]);

    // Baseline. Without this a test that never sees the certificate change
    // would pass for the wrong reason, and the h3 reading in particular has to
    // be shown to work before its agreement with the others means anything.
    assert_eq!(
        h1_leaf(srv.port, h1_tls()).expect("h1 before the reload"),
        first.der,
        "HTTP/1.1 must serve the certificate it started with"
    );
    assert_eq!(
        h2_leaf(srv.port, h2_tls()).expect("h2 before the reload"),
        first.der,
        "HTTP/2 must serve the certificate it started with"
    );
    assert_eq!(
        h3_leaf(srv.port).expect("h3 before the reload"),
        first.der,
        "HTTP/3 must serve the certificate it started with"
    );
    assert_eq!(
        srv.reported_not_after(h1_tls()),
        Ok(first.not_after),
        "/perf must report the expiry of the certificate loaded at startup"
    );

    srv.install(&second);

    // The reload is a filesystem event, so it is not instantaneous. Polling on
    // HTTP/1.1 because that is the protocol the defect left behind: HTTP/3
    // would pass this wait on the unfixed code.
    let reloaded = m6_core::testkit::wait::until(Duration::from_secs(30), || {
        h1_leaf(srv.port, h1_tls()).as_deref() == Ok(second.der.as_slice())
    });
    srv.proc.assert_alive("after the certificate was replaced");
    assert!(
        reloaded,
        "HTTP/1.1 never picked up the replaced certificate. This is m6 #210: the \
         reload rebuilt the HTTP/3 configuration alone and the rustls listener kept \
         the material it was built with at startup.\n--- server output ---\n{}",
        srv.proc.output()
    );

    let h1 = h1_leaf(srv.port, h1_tls()).expect("h1 after the reload");
    let h2 = h2_leaf(srv.port, h2_tls()).expect("h2 after the reload");
    let h3 = h3_leaf(srv.port).expect("h3 after the reload");

    assert_eq!(h1, second.der, "HTTP/1.1 serves the replaced certificate");
    assert_eq!(h2, second.der, "HTTP/2 serves the replaced certificate");
    assert_eq!(h3, second.der, "HTTP/3 serves the replaced certificate");

    // The property stated in the issue, asserted on its own terms: one server
    // presents one expiry, whatever protocol the client negotiated.
    let expiries = [
        ("HTTP/1.1", m6_core::tls::not_after_unix(&h1)),
        ("HTTP/2", m6_core::tls::not_after_unix(&h2)),
        ("HTTP/3", m6_core::tls::not_after_unix(&h3)),
    ];
    for (proto, read) in &expiries {
        assert_eq!(
            read.as_ref().ok(),
            Some(&second.not_after),
            "{proto} presents notAfter {read:?}, and the certificate on disk expires at {}. \
             Two protocols disagreeing here is one server answering with two certificates.",
            second.not_after
        );
    }

    assert_eq!(
        srv.reported_not_after(h1_tls()),
        Ok(second.not_after),
        "/perf must report the expiry of the material now being served. Reporting the \
         startup certificate is what made a renewal invisible to a monitor."
    );

    let status = srv.proc.terminate(Duration::from_secs(10));
    assert!(
        status.success(),
        "m6-http should still stop cleanly after a reload, got {status}\n--- output ---\n{}",
        srv.proc.output()
    );
}

/// A renewal that repoints a symlink reaches every protocol too.
///
/// This is the shape `docs/m6-site-toml.md` prescribes and the one certbot
/// produces: the configured path is `live/<name>/fullchain.pem`, a symlink into
/// `archive/`, and a renewal writes a new archive file and moves the link. The
/// file behind the configured path is never written through.
///
/// The first version of the kqueue fix watched the certificate PATH with
/// `open(O_EVTONLY)`, which resolves the symlink and then holds the inode it
/// landed on. It saw an in-place overwrite, which is what the test above does,
/// and saw nothing at all when the link moved. So the fix passed its own test
/// and missed the only renewal this project performs, which is the kind of gap
/// a test written from the same assumption as the code cannot find.
#[test]
fn a_renewal_that_repoints_a_symlink_also_reaches_every_protocol() {
    let first = cert_expiring_in(2030);
    let second = cert_expiring_in(2035);
    assert_ne!(first.not_after, second.not_after);

    let mut srv = start_with(&first, Layout::CertbotLineage);
    let h1_tls = || client_trusting_both(&first, &second, &[b"http/1.1"]);
    let h2_tls = || client_trusting_both(&first, &second, &[b"h2"]);

    // The configured path is a symlink, and the server resolved it at startup.
    assert!(
        srv.site.join("live/node/fullchain.pem").is_symlink(),
        "the test is only meaningful if the configured path really is a link"
    );
    assert_eq!(
        h1_leaf(srv.port, h1_tls()).expect("h1 before the renewal"),
        first.der
    );

    // Renew: a new archive generation, and the link moved onto it.
    srv.install(&second);
    assert!(
        std::fs::read_link(srv.site.join("live/node/fullchain.pem"))
            .expect("still a link")
            .ends_with("fullchain2.pem"),
        "the renewal must have repointed the link, not written through it"
    );

    let reloaded = m6_core::testkit::wait::until(Duration::from_secs(30), || {
        h1_leaf(srv.port, h1_tls()).as_deref() == Ok(second.der.as_slice())
    });
    srv.proc.assert_alive("after the lineage was renewed");
    assert!(
        reloaded,
        "HTTP/1.1 never picked up a certificate installed by repointing the \
         symlink. The watch resolves the link and holds the inode behind it, so \
         it needs the certificate's PARENT DIRECTORY watched to see the link \
         move.\n--- server output ---\n{}",
        srv.proc.output()
    );

    for (proto, got) in [
        ("HTTP/1.1", h1_leaf(srv.port, h1_tls()).expect("h1")),
        ("HTTP/2", h2_leaf(srv.port, h2_tls()).expect("h2")),
        ("HTTP/3", h3_leaf(srv.port).expect("h3")),
    ] {
        assert_eq!(
            got, second.der,
            "{proto} is serving the pre-renewal certificate after the link moved"
        );
    }

    let status = srv.proc.terminate(Duration::from_secs(10));
    assert!(
        status.success(),
        "clean stop after a lineage renewal, got {status}"
    );
}
