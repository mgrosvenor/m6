//! Who may read `/perf` and `/traffic`, and what a `require` route answers,
//! proved against the real `m6-http` binary over HTTP/1.1, HTTP/2 and HTTP/3.
//!
//! ```text
//! cargo build --workspace --release
//! cargo test -p m6-http --test metrics_access -- --test-threads=1
//! ```

use std::io::{Read, Write};
use std::net::{TcpStream, UdpSocket};
use std::process::{Child, Command, Stdio};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use quiche::h3::NameValue as _;
use rustls::StreamOwned;

use m6_core::testkit::{self, claim_port, PortClaim, Service};
use m6_http_lib::forward::{HttpRequest, HttpResponse};
use m6_http_lib::h2c_client::H2cClientConn;
use m6_http_lib::h2s_client::H2sTlsClientConn;

// ── Responses ─────────────────────────────────────────────────────────────────

struct Resp {
    status: u16,
    /// Header names lower-cased.
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Resp {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

fn no_response(reason: impl std::fmt::Display) -> Resp {
    Resp {
        status: 0,
        headers: vec![("x-test-error".to_string(), reason.to_string())],
        body: Vec::new(),
    }
}

// ── Crypto fixtures ───────────────────────────────────────────────────────────

fn tls_config(cert_der: &[u8], alpn: &[&[u8]]) -> Arc<rustls::ClientConfig> {
    rustls::crypto::ring::default_provider()
        .install_default()
        .ok();
    let cert = rustls::pki_types::CertificateDer::from(cert_der.to_vec());
    let mut store = rustls::RootCertStore::empty();
    store.add(cert).unwrap();
    let mut cfg = rustls::ClientConfig::builder()
        .with_root_certificates(store)
        .with_no_client_auth();
    cfg.alpn_protocols = alpn.iter().map(|p| p.to_vec()).collect();
    Arc::new(cfg)
}

/// ES256 keypair. Returns (private_pem, public_pem).
fn keypair() -> (String, String) {
    let kp = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).expect("ec keypair");
    (kp.serialize_pem(), kp.public_key_pem())
}

/// Mint an ES256 token carrying `roles`, expiring `exp_offset_s` from now.
fn mint(private_pem: &str, roles: &[&str], exp_offset_s: i64) -> String {
    #[derive(serde::Serialize)]
    struct Claims {
        sub: String,
        exp: i64,
        roles: Vec<String>,
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let claims = Claims {
        sub: "metrics-test".to_string(),
        exp: now + exp_offset_s,
        roles: roles.iter().map(|r| r.to_string()).collect(),
    };
    let key = jsonwebtoken::EncodingKey::from_ec_pem(private_pem.as_bytes()).expect("ec key");
    let header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::ES256);
    jsonwebtoken::encode(&header, &claims, &key).expect("sign jwt")
}

// ── The site under test ───────────────────────────────────────────────────────

/// The legacy bearer string every fixture writes to `metrics.token`.
const LEGACY_TOKEN: &str = "legacy-secret-token-0123456789abcdef";

/// `[auth]` with a verify key only, and a route that needs a role.
const AUTH: &str = "[auth]\npublic_key = \"{KEY}\"\n\n\
    [[route]]\npath = \"/private/{relpath}\"\nbackend = \"b\"\nrequire = \"role:monitor\"\n";

/// The metrics gate, for the listed peer 127.0.0.1.
const GATE: &str =
    "[health]\nmetrics_require = \"role:monitor\"\nmetrics_peers = [\"127.0.0.1\"]\n";

/// The files and ports one test needs, written but not yet running.
struct Fixture {
    dir: tempfile::TempDir,
    site: std::path::PathBuf,
    system: std::path::PathBuf,
    port: PortClaim,
    /// A second claimed address, `{H2C}` in the configs.
    h2c: PortClaim,
    cert_der: Vec<u8>,
    /// Signing key whose public half is `auth-public.pem`.
    private_pem: String,
    /// A signing key the site does not trust.
    other_private_pem: String,
}

impl Fixture {
    /// `site` is appended to site.toml and `system` to system.toml (after
    /// `[node]`). `server` adds lines to `[server]`. In all three `{KEY}` is
    /// the verify key path, `{TOKEN}` the legacy token file, `{H2C}` a claimed
    /// extra address.
    fn new(site: &str, server: &str, system: &str) -> Fixture {
        let dir = tempfile::tempdir().expect("tempdir");
        let ck = rcgen::generate_simple_self_signed(vec![
            "localhost".to_string(),
            "127.0.0.1".to_string(),
        ])
        .expect("rcgen");
        let d = dir.path();
        std::fs::write(d.join("cert.pem"), ck.cert.pem()).unwrap();
        std::fs::write(d.join("key.pem"), ck.key_pair.serialize_pem()).unwrap();
        let (private_pem, public_pem) = keypair();
        let (other_private_pem, _) = keypair();
        std::fs::write(d.join("auth-public.pem"), public_pem).unwrap();
        std::fs::write(d.join("metrics.token"), LEGACY_TOKEN).unwrap();
        std::fs::write(d.join("analytics.ndjson"), "").unwrap();

        let port = claim_port();
        let h2c = claim_port();
        let sub = |s: &str| {
            s.replace("{KEY}", &d.join("auth-public.pem").display().to_string())
                .replace("{TOKEN}", &d.join("metrics.token").display().to_string())
                .replace("{H2C}", &h2c.addr())
        };

        let site_dir = d.join("site");
        std::fs::create_dir_all(&site_dir).unwrap();
        std::fs::write(
            site_dir.join("site.toml"),
            format!(
                "[site]\nname = \"metrics-access\"\ndomain = \"localhost\"\n\n\
                 [errors]\nmode = \"internal\"\n\n\
                 [analytics]\nenabled = false\nlog_path = \"{}\"\n\n\
                 [rate_limit]\nenabled = false\n\n\
                 [[backend]]\nname = \"b\"\nsockets = \"/nonexistent/b-*.sock\"\n\n\
                 [[route]]\npath = \"/\"\nbackend = \"b\"\n\n{}",
                d.join("analytics.ndjson").display(),
                sub(site),
            ),
        )
        .unwrap();

        let system_path = d.join("system.toml");
        std::fs::write(
            &system_path,
            format!(
                "[server]\nbind = \"{}\"\ntls_cert = \"{}\"\ntls_key = \"{}\"\n{}\n\n\
                 [node]\nname = \"metrics-access\"\n\n{}",
                port.addr(),
                d.join("cert.pem").display(),
                d.join("key.pem").display(),
                sub(server),
                sub(system),
            ),
        )
        .unwrap();

        Fixture {
            site: site_dir,
            system: system_path,
            port,
            h2c,
            cert_der: ck.cert.der().to_vec(),
            private_pem,
            other_private_pem,
            dir,
        }
    }

    fn command(&self) -> Command {
        let mut cmd = Command::new(testkit::binary("m6-http"));
        cmd.arg(&self.site)
            .arg(&self.system)
            .env("RUST_LOG", "info")
            .current_dir(self.dir.path());
        cmd
    }

    /// Start m6-http and wait until `/health` answers. The backend socket does
    /// not exist, so the answer is 503 (degraded): any status proves serving.
    fn start(self) -> Running {
        let mut svc = Service::spawn("m6-http-metrics", &mut self.command());
        svc.wait_for_tcp(self.port.port(), Duration::from_secs(10));
        let running = Running { svc, fx: self };
        let ready = testkit::wait::until(Duration::from_secs(30), || {
            running.h1("/health", &[]).status != 0
        });
        assert!(
            ready,
            "m6-http never answered /health\n{}",
            running.svc.output()
        );
        running
    }

    /// Run m6-http to its exit, which must come within ten seconds.
    fn run_until_exit(&self) -> (Option<i32>, String) {
        let mut child: Child = self
            .command()
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn m6-http");
        let exited = testkit::wait::until(Duration::from_secs(10), || {
            child.try_wait().expect("try_wait").is_some()
        });
        if !exited {
            let _ = child.kill();
            let _ = child.wait();
            panic!("m6-http was still running after 10s; it should have refused to start");
        }
        let out = child.wait_with_output().expect("output");
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        (out.status.code(), text)
    }
}

/// A GET for `path` with `extra` headers, as the h2 and h2c clients take it.
fn get_req(path: &str, extra: &[(&str, &str)], version: &str) -> HttpRequest {
    HttpRequest {
        method: "GET".to_string(),
        path: path.to_string(),
        query: None,
        version: version.to_string(),
        headers: extra
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
        body: Vec::new(),
    }
}

/// Drive a client connection until `rx` answers, or ten seconds pass.
fn await_resp(
    label: &str,
    rx: &mpsc::Receiver<std::io::Result<HttpResponse>>,
    mut drive: impl FnMut(),
) -> Resp {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        drive();
        match rx.try_recv() {
            Ok(Ok(r)) => {
                return Resp {
                    status: r.status,
                    headers: r
                        .headers
                        .iter()
                        .map(|(k, v)| (k.to_ascii_lowercase(), v.clone()))
                        .collect(),
                    body: r.body,
                }
            }
            Ok(Err(e)) => return no_response(format!("{label} response: {e}")),
            Err(mpsc::TryRecvError::Empty) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(1));
            }
            Err(e) => return no_response(format!("{label} wait: {e:?}")),
        }
    }
}

struct Running {
    svc: Service,
    fx: Fixture,
}

fn bearer(token: &str) -> String {
    format!("Bearer {token}")
}

impl Running {
    fn port(&self) -> u16 {
        self.fx.port.port()
    }

    /// A token for `roles` signed by the key the site trusts.
    fn token(&self, roles: &[&str]) -> String {
        mint(&self.fx.private_pem, roles, 3600)
    }

    /// A token for `roles` signed by a key the site does not trust.
    fn foreign_token(&self, roles: &[&str]) -> String {
        mint(&self.fx.other_private_pem, roles, 3600)
    }

    /// One GET on the internal h2c listener, claiming `client_ip`.
    fn h2c(&self, path: &str, extra: &[(&str, &str)], client_ip: &str) -> Resp {
        let mut conn = match H2cClientConn::connect("127.0.0.1", self.fx.h2c.port()) {
            Ok(c) => c,
            Err(e) => return no_response(format!("h2c connect: {e}")),
        };
        let settle = Instant::now() + Duration::from_millis(250);
        while Instant::now() < settle {
            conn.drive();
            std::thread::sleep(Duration::from_millis(1));
        }
        let rx = match conn.dispatch(
            &get_req(path, extra, "HTTP/1.1"),
            "127.0.0.1",
            client_ip,
            "localhost",
        ) {
            Ok(rx) => rx,
            Err(e) => return no_response(format!("h2c dispatch: {e}")),
        };
        await_resp("h2c", &rx, || conn.drive())
    }

    /// One HTTP/1.1 GET over TLS on a fresh connection.
    fn h1(&self, path: &str, extra: &[(&str, &str)]) -> Resp {
        let tcp = match TcpStream::connect(("127.0.0.1", self.port())) {
            Ok(t) => t,
            Err(e) => return no_response(format!("connect: {e}")),
        };
        if let Err(e) = tcp.set_read_timeout(Some(Duration::from_secs(10))) {
            return no_response(format!("set_read_timeout: {e}"));
        }
        let name = rustls::pki_types::ServerName::try_from("127.0.0.1".to_string()).unwrap();
        let conn = match rustls::ClientConnection::new(tls_config(&self.fx.cert_der, &[]), name) {
            Ok(c) => c,
            Err(e) => return no_response(format!("tls setup: {e}")),
        };
        let mut stream = StreamOwned::new(conn, tcp);
        let mut req = format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n", self.port());
        for (k, v) in extra {
            req.push_str(&format!("{k}: {v}\r\n"));
        }
        req.push_str("Connection: close\r\n\r\n");
        if let Err(e) = stream
            .write_all(req.as_bytes())
            .and_then(|()| stream.flush())
        {
            return no_response(format!("write request: {e}"));
        }
        let mut raw = Vec::new();
        match stream.read_to_end(&mut raw) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {}
            Err(e) => return no_response(format!("tls read: {e}")),
        }
        let text = String::from_utf8_lossy(&raw).into_owned();
        let end = text.find("\r\n\r\n").unwrap_or(raw.len());
        let mut lines = text[..end].split("\r\n");
        let status = lines
            .next()
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let headers = lines
            .filter_map(|l| l.split_once(':'))
            .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
            .collect();
        let body = if end + 4 <= raw.len() {
            raw[end + 4..].to_vec()
        } else {
            Vec::new()
        };
        Resp {
            status,
            headers,
            body,
        }
    }

    /// One HTTP/2 GET over TLS. `client_ip` goes to the `x-forwarded-for`
    /// header that the client helper always adds.
    fn h2(&self, path: &str, extra: &[(&str, &str)], client_ip: &str) -> Resp {
        let cfg = tls_config(&self.fx.cert_der, &[b"h2"]);
        let mut conn = match H2sTlsClientConn::connect("localhost", self.port(), cfg) {
            Ok(c) => c,
            Err(e) => return no_response(format!("h2 connect: {e}")),
        };
        let rx = match conn.dispatch(
            &get_req(path, extra, "HTTP/2"),
            "localhost",
            client_ip,
            "localhost",
        ) {
            Ok(rx) => rx,
            Err(e) => return no_response(format!("h2 dispatch: {e}")),
        };
        await_resp("h2", &rx, || conn.drive())
    }

    /// One HTTP/3 GET over QUIC.
    fn h3(&self, path: &str, extra: &[(&str, &str)]) -> Resp {
        match h3_get(self.port(), path, extra) {
            Ok(r) => r,
            Err(e) => no_response(format!("h3: {e}")),
        }
    }

    /// The same GET on all three protocols, as (label, response).
    fn all(&self, path: &str, extra: &[(&str, &str)]) -> Vec<(&'static str, Resp)> {
        vec![
            ("h1", self.h1(path, extra)),
            ("h2", self.h2(path, extra, "127.0.0.1")),
            ("h3", self.h3(path, extra)),
        ]
    }
}

// ── HTTP/3 client ─────────────────────────────────────────────────────────────

fn quic_flush(conn: &mut quiche::Connection, udp: &UdpSocket, out: &mut [u8]) {
    while let Ok((n, info)) = conn.send(out) {
        let _ = udp.send_to(&out[..n], info.to);
    }
}

fn h3_get(port: u16, path: &str, extra: &[(&str, &str)]) -> Result<Resp, String> {
    let server_addr: std::net::SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    let udp = UdpSocket::bind("127.0.0.1:0").map_err(|e| e.to_string())?;
    udp.set_nonblocking(true).unwrap();
    let local = udp.local_addr().unwrap();

    let mut config = quiche::Config::new(quiche::PROTOCOL_VERSION).map_err(|e| e.to_string())?;
    config
        .set_application_protos(quiche::h3::APPLICATION_PROTOCOL)
        .map_err(|e| e.to_string())?;
    config.set_max_idle_timeout(10_000);
    config.set_initial_max_data(10_000_000);
    config.set_initial_max_stream_data_bidi_local(1_000_000);
    config.set_initial_max_stream_data_bidi_remote(1_000_000);
    config.set_initial_max_stream_data_uni(1_000_000);
    config.set_initial_max_streams_bidi(1000);
    config.set_initial_max_streams_uni(100);
    config.grease(false);
    config.verify_peer(false);

    let scid_bytes = [5u8; quiche::MAX_CONN_ID_LEN];
    let scid = quiche::ConnectionId::from_ref(&scid_bytes);
    let mut conn = quiche::connect(Some("localhost"), &scid, local, server_addr, &mut config)
        .map_err(|e| format!("connect: {e}"))?;

    let mut h3: Option<quiche::h3::Connection> = None;
    let mut buf = vec![0u8; 65536];
    let mut out = vec![0u8; 1350];
    let mut sent = false;
    let mut status: Option<u16> = None;
    let mut headers: Vec<(String, String)> = Vec::new();
    let mut body: Vec<u8> = Vec::new();
    let mut done = false;
    let deadline = Instant::now() + Duration::from_secs(30);

    loop {
        if Instant::now() > deadline {
            return Err(format!("timeout: sent={sent} status={status:?}"));
        }
        conn.on_timeout();
        quic_flush(&mut conn, &udp, &mut out);

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
            return Err("connection closed before response".to_string());
        }

        if conn.is_established() && h3.is_none() {
            let cfg = quiche::h3::Config::new().map_err(|e| e.to_string())?;
            h3 = Some(
                quiche::h3::Connection::with_transport(&mut conn, &cfg)
                    .map_err(|e| format!("h3 init: {e}"))?,
            );
        }

        if let Some(ref mut h3c) = h3 {
            if !sent {
                let mut req = vec![
                    quiche::h3::Header::new(b":method", b"GET"),
                    quiche::h3::Header::new(b":path", path.as_bytes()),
                    quiche::h3::Header::new(b":scheme", b"https"),
                    quiche::h3::Header::new(b":authority", b"localhost"),
                ];
                for (k, v) in extra {
                    req.push(quiche::h3::Header::new(
                        k.to_ascii_lowercase().as_bytes(),
                        v.as_bytes(),
                    ));
                }
                match h3c.send_request(&mut conn, &req, true) {
                    Ok(_) => sent = true,
                    Err(quiche::h3::Error::Done) => {}
                    Err(e) => return Err(format!("send_request: {e}")),
                }
            }

            loop {
                match h3c.poll(&mut conn) {
                    Ok((_, quiche::h3::Event::Headers { list, .. })) => {
                        for h in &list {
                            let name = std::str::from_utf8(h.name()).unwrap_or("");
                            let value = std::str::from_utf8(h.value()).unwrap_or("");
                            if name == ":status" {
                                if let Ok(code) = value.parse::<u16>() {
                                    if code >= 200 {
                                        status = Some(code);
                                    }
                                }
                            } else {
                                headers.push((name.to_ascii_lowercase(), value.to_string()));
                            }
                        }
                    }
                    Ok((sid, quiche::h3::Event::Data)) => {
                        let mut tmp = [0u8; 4096];
                        while let Ok(n) = h3c.recv_body(&mut conn, sid, &mut tmp) {
                            if n == 0 {
                                break;
                            }
                            body.extend_from_slice(&tmp[..n]);
                        }
                    }
                    Ok((_, quiche::h3::Event::Finished)) => done = true,
                    Ok(_) => {}
                    Err(quiche::h3::Error::Done) => break,
                    Err(e) => return Err(format!("poll: {e}")),
                }
            }
        }

        quic_flush(&mut conn, &udp, &mut out);
        if done && status.is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(1));
    }

    Ok(Resp {
        status: status.ok_or("no status received")?,
        headers,
        body,
    })
}

// ── Tests ─────────────────────────────────────────────────────────────────────

/// The proxy's own 401 on a `require` route names the scheme (RFC 9110
/// 11.6.1), on every protocol.
#[test]
fn a_require_route_401_carries_www_authenticate() {
    let run = Fixture::new(AUTH, "", "").start();
    for (proto, r) in run.all("/private/x", &[]) {
        assert_eq!(r.status, 401, "{proto}: {}", r.text());
        assert_eq!(r.header("www-authenticate"), Some("Bearer"), "{proto}");
    }
    let bad = run.h1("/private/x", &[("Authorization", "Bearer not.a.token")]);
    assert_eq!(bad.status, 401);
    assert_eq!(bad.header("www-authenticate"), Some("Bearer"));
}

/// `[auth]` without a `backend` still verifies for a `require` route: no token
/// is 401, a token with the role passes the check and reaches the (absent)
/// backend, which is a 502 or 503.
#[test]
fn a_require_route_is_enforced_with_a_verify_only_auth() {
    let run = Fixture::new(AUTH, "", "").start();
    assert_eq!(run.h1("/private/x", &[]).status, 401);
    let token = run.token(&["monitor"]);
    let r = run.h1("/private/x", &[("Authorization", &bearer(&token))]);
    assert!(
        r.status != 401 && r.status != 403,
        "a valid token must pass the check, got {}",
        r.status
    );
    let wrong = run.token(&["reader"]);
    let r = run.h1("/private/x", &[("Authorization", &bearer(&wrong))]);
    assert_eq!(r.status, 403);
}

#[test]
fn auth_in_both_files_refuses_to_start() {
    let fx = Fixture::new(AUTH, "", "[auth]\npublic_key = \"{KEY}\"\n");
    let (code, output) = fx.run_until_exit();
    assert_eq!(code, Some(2), "output:\n{output}");
    assert!(output.contains("declare it in one"), "output:\n{output}");
}

// ── /perf and /traffic ────────────────────────────────────────────────────────

/// The default config: verify-only `[auth]`, the role gate, listed peer
/// 127.0.0.1 (the address every test client connects from).
fn gated() -> String {
    format!("{AUTH}\n{GATE}")
}

fn assert_report(proto: &str, r: &Resp) {
    assert_eq!(r.status, 200, "{proto}: {}", r.text());
    let report: m6_core::monitoring::PerfReport =
        serde_json::from_slice(&r.body).unwrap_or_else(|e| panic!("{proto}: not a report: {e}"));
    assert_eq!(report.node, "metrics-access", "{proto}");
}

#[test]
fn perf_answers_a_listed_peer_with_the_role_over_h1_h2_h3() {
    let run = Fixture::new(&gated(), "", "").start();
    let auth = bearer(&run.token(&["monitor"]));
    for (proto, r) in run.all("/perf", &[("Authorization", &auth)]) {
        assert_report(proto, &r);
    }
}

#[test]
fn an_unlisted_peer_gets_404_with_a_valid_token_over_h1_h2_h3() {
    let site = format!("{AUTH}\n{}", GATE.replace("127.0.0.1", "10.0.0.9"));
    let run = Fixture::new(&site, "", "").start();
    let auth = bearer(&run.token(&["monitor"]));
    for path in ["/perf", "/traffic"] {
        for (proto, r) in run.all(path, &[("Authorization", &auth)]) {
            assert_eq!(r.status, 404, "{proto} {path}: {}", r.text());
            assert_eq!(r.text(), r#"{"error":"not found"}"#, "{proto} {path}");
        }
    }
}

#[test]
fn a_forwarded_address_naming_a_listed_peer_does_not_open_perf() {
    let site = format!("{AUTH}\n{}", GATE.replace("127.0.0.1", "10.0.0.9"));
    let run = Fixture::new(&site, "", "").start();
    let auth = bearer(&run.token(&["monitor"]));
    let r = run.h1(
        "/perf",
        &[("Authorization", &auth), ("X-Forwarded-For", "10.0.0.9")],
    );
    assert_eq!(r.status, 404, "h1: {}", r.text());
    // The h2 client helper sends the claimed address as x-forwarded-for.
    let r = run.h2("/perf", &[("Authorization", &auth)], "10.0.0.9");
    assert_eq!(r.status, 404, "h2: {}", r.text());
}

#[test]
fn perf_is_refused_on_an_internal_h2c_listener() {
    let run = Fixture::new(
        &gated(),
        "h2c_bind = \"{H2C}\"\ninternal_binds = [\"{H2C}\"]\ninternal_peers = [\"127.0.0.1\"]\n",
        "",
    )
    .start();
    let auth = bearer(&run.token(&["monitor"]));
    let r = run.h2c("/perf", &[("Authorization", &auth)], "127.0.0.1");
    assert_eq!(r.status, 404, "{}", r.text());
    let r = run.h2c("/traffic", &[("Authorization", &auth)], "127.0.0.1");
    assert_eq!(r.status, 404, "{}", r.text());
}

#[test]
fn perf_without_a_token_is_401_with_www_authenticate() {
    let run = Fixture::new(&gated(), "", "").start();
    for (proto, r) in run.all("/perf", &[]) {
        assert_eq!(r.status, 401, "{proto}: {}", r.text());
        assert_eq!(r.header("www-authenticate"), Some("Bearer"), "{proto}");
    }
}

#[test]
fn perf_with_a_token_lacking_the_role_is_403() {
    let run = Fixture::new(&gated(), "", "").start();
    let auth = bearer(&run.token(&["reader"]));
    for (proto, r) in run.all("/perf", &[("Authorization", &auth)]) {
        assert_eq!(r.status, 403, "{proto}: {}", r.text());
        assert_eq!(r.text(), r#"{"error":"forbidden"}"#, "{proto}");
        assert_eq!(r.header("www-authenticate"), None, "{proto}");
    }
}

#[test]
fn perf_with_another_keys_token_is_401() {
    let run = Fixture::new(&gated(), "", "").start();
    let auth = bearer(&run.foreign_token(&["monitor"]));
    for (proto, r) in run.all("/perf", &[("Authorization", &auth)]) {
        assert_eq!(r.status, 401, "{proto}: {}", r.text());
    }
}

#[test]
fn an_expired_token_is_401() {
    let run = Fixture::new(&gated(), "", "").start();
    let auth = bearer(&mint(&run.fx.private_pem, &["monitor"], -3600));
    let r = run.h1("/perf", &[("Authorization", &auth)]);
    assert_eq!(r.status, 401, "{}", r.text());
}

#[test]
fn the_legacy_bearer_string_is_refused_once_metrics_require_is_set() {
    let site = format!("{AUTH}\n{GATE}metrics_token_file = \"{{TOKEN}}\"\n");
    let run = Fixture::new(&site, "", "").start();
    let auth = bearer(LEGACY_TOKEN);
    let r = run.h1("/perf", &[("Authorization", &auth)]);
    assert_eq!(r.status, 401, "{}", r.text());
}

#[test]
fn traffic_follows_the_same_rule() {
    let run = Fixture::new(&gated(), "", "").start();
    let good = bearer(&run.token(&["monitor"]));
    let reader = bearer(&run.token(&["reader"]));
    for (want, extra) in [
        (200, vec![("Authorization", good.as_str())]),
        (401, vec![]),
        (403, vec![("Authorization", reader.as_str())]),
    ] {
        for (proto, r) in run.all("/traffic", &extra) {
            assert_eq!(r.status, want, "{proto}: {}", r.text());
        }
    }
}

#[test]
fn health_stays_open_to_an_unlisted_peer() {
    let site = format!("{AUTH}\n{}", GATE.replace("127.0.0.1", "10.0.0.9"));
    let run = Fixture::new(&site, "", "").start();
    for (proto, r) in run.all("/health", &[]) {
        // 200, or 503 when degraded: either way not 404 and not 401.
        assert!(r.status == 200 || r.status == 503, "{proto}: {}", r.status);
        assert!(r.text().contains("\"status\""), "{proto}: {}", r.text());
    }
}

#[test]
fn auth_and_health_keys_are_read_from_the_system_config() {
    let system = "[auth]\npublic_key = \"{KEY}\"\n\n\
        [health]\nmetrics_require = \"role:monitor\"\nmetrics_peers = [\"127.0.0.1\"]\n";
    let run = Fixture::new("", "", system).start();
    let auth = bearer(&run.token(&["monitor"]));
    assert_report("h1", &run.h1("/perf", &[("Authorization", &auth)]));
    assert_eq!(run.h1("/perf", &[]).status, 401);
}

#[test]
fn without_new_keys_perf_behaves_as_before() {
    let run = Fixture::new("[health]\nmetrics_token_file = \"{TOKEN}\"\n", "", "").start();
    let right = bearer(LEGACY_TOKEN);
    assert_report("h1", &run.h1("/perf", &[("Authorization", &right)]));
    assert_eq!(
        run.h1("/perf", &[("Authorization", "Bearer wrong")]).status,
        401
    );
    assert_eq!(run.h1("/perf", &[]).status, 401);
}

#[test]
fn a_public_metrics_peer_refuses_to_start() {
    let site = format!("{AUTH}\n{}", GATE.replace("127.0.0.1", "203.0.113.7"));
    let (code, output) = Fixture::new(&site, "", "").run_until_exit();
    assert_eq!(code, Some(2), "output:\n{output}");
    assert!(output.contains("not in a local range"), "output:\n{output}");
}
