//! Who may read `/perf` and `/traffic`, and what a `require` route answers,
//! proved against the real `m6-http` binary over HTTP/1.1, HTTP/2 and HTTP/3.
//!
//! ```text
//! cargo build --workspace --release
//! cargo test -p m6-http --test metrics_access -- --test-threads=1
//! ```

#![allow(
    clippy::disallowed_methods,
    reason = "test code: threads, sleeps and blocking clients drive the service from outside"
)]

use std::io::{Read, Write};
use std::net::{TcpStream, UdpSocket};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use quiche::h3::NameValue as _;
use rustls::StreamOwned;

use m6_core::testkit::{self, claim_port, PortClaim, Reply, Service};

#[path = "support/check.rs"]
mod check_support;
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

    /// The `Retry-After` a 503 carries, for a retry that honours it.
    fn retry_after(&self) -> Option<std::time::Duration> {
        check_support::retry_after(self.header("retry-after"))
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
    rustls::crypto::aws_lc_rs::default_provider()
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

// ── The site under test ───────────────────────────────────────────────────────

/// The `[auth]` block with the check backend the route gate asks.
const AUTH: &str = "[auth]\nbackend = \"auth-check\"\n";

/// A route that needs a role, which belongs in site.toml.
const PRIVATE_ROUTE: &str =
    "[[route]]\npath = \"/private/{relpath}\"\nbackend = \"b\"\nrequire = \"role:monitor\"\n";

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
    /// The authority's answer for the two role sets the tests use.
    monitor_token: String,
    reader_token: String,
    _check: check_support::RunningCheck,
    /// The accounting tick this test needs, in seconds. Production is a fixed
    /// ten and has no key; the test-only variable is read by a debug build.
    stats_interval: Option<u64>,
    /// `RUST_LOG` for the child. Debug is how the once-per-request count is
    /// read: the one token check logs one `auth check` line per call.
    log_level: &'static str,
}

impl Fixture {
    /// `site` is appended to site.toml and `system` to system.toml (after
    /// `[node]`). `server` adds lines to `[server]`. `{H2C}` is a claimed
    /// extra address.
    fn new(site: &str, server: &str, system: &str) -> Fixture {
        Self::build(site, server, system, false)
    }

    /// Like [`Fixture::new`], with analytics on, so each request is logged and
    /// copied into `/traffic`'s window.
    fn logging(site: &str, server: &str, system: &str) -> Fixture {
        Self::build(site, server, system, true)
    }

    fn build(site: &str, server: &str, system: &str, analytics: bool) -> Fixture {
        let dir = tempfile::tempdir().expect("tempdir");
        let ck = rcgen::generate_simple_self_signed(vec![
            "localhost".to_string(),
            "127.0.0.1".to_string(),
        ])
        .expect("rcgen");
        let d = dir.path();
        std::fs::write(d.join("cert.pem"), ck.cert.pem()).unwrap();
        std::fs::write(d.join("key.pem"), ck.key_pair.serialize_pem()).unwrap();
        let monitor_token = "monitor-token".to_string();
        let reader_token = "reader-token".to_string();
        let check_path = d.join("check.sock");
        let check = check_support::RunningCheck::start(
            &check_path,
            vec![
                (
                    monitor_token.clone(),
                    Reply::Identity(check_support::identity(&[], &["monitor"])),
                ),
                (
                    reader_token.clone(),
                    Reply::Identity(check_support::identity(&[], &["reader"])),
                ),
            ],
        )
        .unwrap();
        std::fs::write(d.join("analytics.ndjson"), "").unwrap();
        std::fs::create_dir_all(d.join("state")).unwrap();

        let port = claim_port();
        let h2c = claim_port();
        let sub = |s: &str| s.replace("{H2C}", &h2c.addr());

        let site_dir = d.join("site");
        std::fs::create_dir_all(&site_dir).unwrap();
        // A socket file with no process listening: the pool's glob matches it,
        // so m6-http starts, and a connect to it is refused. The pool reads its
        // glob once at start, so the file must exist before the spawn.
        let backend_sock = d.join("b-1.sock");
        let backend_glob = d.join("b-*.sock");
        let _ = std::os::unix::net::UnixListener::bind(&backend_sock).expect("placeholder socket");
        std::fs::write(
            site_dir.join("site.toml"),
            format!(
                "[site]\nname = \"metrics-access\"\ndomain = \"localhost\"\n\n\
                 [errors]\nmode = \"internal\"\n\n\
                 [analytics]\nenabled = {}\nlog_path = \"{}\"\n\n\
                 [rate_limit]\nenabled = false\n\n\
                 [[backend]]\nname = \"b\"\nsockets = \"{}\"\n\n\
                 [[backend]]\nname = \"auth-check\"\nsockets = \"{}\"\n\n\
                 [[route]]\npath = \"/\"\nbackend = \"b\"\n\n{}",
                analytics,
                d.join("analytics.ndjson").display(),
                backend_glob.display(),
                check_path.display(),
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
            monitor_token,
            reader_token,
            _check: check,
            dir,
            stats_interval: None,
            log_level: "info",
        }
    }

    /// Run the child at debug, so the one token check's `auth check` line is
    /// visible to the test. The level comes from `site.toml`; `RUST_LOG`
    /// alone is not the switch.
    fn with_debug(mut self) -> Self {
        self.log_level = "debug";
        let p = self.site.join("site.toml");
        let mut site = std::fs::read_to_string(&p).expect("site.toml");
        site.push_str("\n[log]\nlevel = \"debug\"\n");
        std::fs::write(&p, site).expect("site.toml");
        self
    }

    /// Shorten this fixture's accounting tick. Only a test needs this: the
    /// production tick is a fixed constant.
    fn with_stats_interval(mut self, secs: u64) -> Self {
        self.stats_interval = Some(secs);
        self
    }

    fn command(&self) -> Command {
        let mut cmd = Command::new(testkit::binary("m6-http"));
        cmd.arg(&self.site)
            .arg(&self.system)
            .env(
                m6_core::platform::STATE_DIR_VAR,
                self.dir.path().join("state"),
            )
            .env("RUST_LOG", self.log_level)
            .current_dir(self.dir.path());
        if let Some(secs) = self.stats_interval {
            cmd.env("M6_TEST_STATS_INTERVAL_SECS", secs.to_string());
        }
        cmd
    }

    /// Start m6-http and wait until `/health` answers. The backend socket is a
    /// placeholder with no listener, so a request through it answers 503: any
    /// status proves serving.
    fn start(self) -> Running {
        let mut svc = Service::spawn("m6-http-metrics", &mut self.command());
        svc.wait_for_tcp(self.port.port(), m6_core::testkit::STARTUP_BOUND);
        let running = Running { svc, fx: self };
        let ready = testkit::wait::until(m6_core::testkit::STARTUP_BOUND, || {
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
        let mut child = m6_core::testkit::process::Running::spawn(
            self.command()
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped()),
        )
        .expect("spawn m6-http");
        let exited = testkit::wait::until(m6_core::testkit::STARTUP_BOUND, || {
            child.child().try_wait().expect("try_wait").is_some()
        });
        if !exited {
            // The guard kills and reaps it while this panic unwinds.
            panic!("m6-http was still running after 10s; it should have refused to start");
        }
        let out = child.take().wait_with_output().expect("output");
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

/// Drive a client connection until `poll` answers, under one generous bound.
///
/// The bound is a safety net for a peer that stays silent, not an expected
/// wait: ten seconds here turned CPU contention into "h2 wait: timed out".
fn await_resp(
    label: &str,
    mut poll: impl FnMut() -> Option<std::io::Result<HttpResponse>>,
) -> Resp {
    let deadline = Instant::now() + testkit::RESPONSE_BOUND;
    loop {
        match poll() {
            Some(Ok(r)) => {
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
            Some(Err(e)) => return no_response(format!("{label} response: {e}")),
            None if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(1));
            }
            None => return no_response(format!("{label} wait: timed out")),
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

    /// A token for `roles` signed by the key the site trusts. The check table
    /// was built from these two, because the authority answers the credential
    /// string and a freshly minted token would not be in it.
    fn token(&self, roles: &[&str]) -> String {
        match roles {
            ["monitor"] => self.fx.monitor_token.clone(),
            ["reader"] => self.fx.reader_token.clone(),
            other => panic!("no check answer for roles {other:?}"),
        }
    }

    /// Number of `auth check` lines m6-http has logged so far. The one token
    /// check emits exactly one per call, so this counts checks, not routes.
    fn auth_check_count(&self) -> usize {
        self.svc
            .output()
            .lines()
            .filter(|l| l.contains("auth check"))
            .count()
    }

    /// Retry an answer from `/perf`, `/traffic` or `/genid` while it is the
    /// design's 503 for a check that missed its 50 ms deadline.
    ///
    /// Those three are readiness signals in this suite: a slow check is not
    /// the property under test, so the call is repeated under the testkit
    /// bound and honours `Retry-After`. Any other path is answered once.
    fn retry_gated(&self, path: &str, mut attempt: impl FnMut() -> Resp) -> Resp {
        if check_support::check_gated(path) {
            check_support::retry_unavailable(testkit::RESPONSE_BOUND, || {
                let r = attempt();
                (r.status, r.retry_after(), r)
            })
        } else {
            attempt()
        }
    }

    /// One GET on the internal h2c listener, claiming `client_ip`.
    fn h2c(&self, path: &str, extra: &[(&str, &str)], client_ip: &str) -> Resp {
        self.retry_gated(path, || self.exchange_h2c(path, extra, client_ip))
    }

    fn exchange_h2c(&self, path: &str, extra: &[(&str, &str)], client_ip: &str) -> Resp {
        let mut conn = match H2cClientConn::connect("127.0.0.1", self.fx.h2c.port()) {
            Ok(c) => c,
            Err(e) => return no_response(format!("h2c connect: {e}")),
        };
        let settle = Instant::now() + Duration::from_millis(250);
        while Instant::now() < settle {
            conn.drive();
            std::thread::sleep(Duration::from_millis(1));
        }
        let stream_id = match conn.dispatch(
            &get_req(path, extra, "HTTP/1.1"),
            "127.0.0.1",
            client_ip,
            "localhost",
        ) {
            Ok(stream_id) => stream_id,
            Err(e) => return no_response(format!("h2c dispatch: {e}")),
        };
        await_resp("h2c", || {
            conn.drive();
            conn.drain_completions()
                .into_iter()
                .find(|c| c.stream_id == stream_id)
                .map(|c| c.result)
        })
    }

    /// One HTTP/1.1 GET over TLS on a fresh connection.
    fn h1(&self, path: &str, extra: &[(&str, &str)]) -> Resp {
        self.retry_gated(path, || self.exchange_h1(path, extra))
    }

    fn exchange_h1(&self, path: &str, extra: &[(&str, &str)]) -> Resp {
        let tcp = match TcpStream::connect(("127.0.0.1", self.port())) {
            Ok(t) => t,
            Err(e) => return no_response(format!("connect: {e}")),
        };
        // No socket read timeout. The response is waited for as an event under
        // one generous outer bound: a read timeout fires on a loaded host while
        // the service is healthy, and the caller sees status 0.
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
        let raw = match testkit::wait_for_event(testkit::RESPONSE_BOUND, move || {
            let mut stream = stream;
            let mut raw = Vec::new();
            match stream.read_to_end(&mut raw) {
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {}
                Err(e) => return Err(format!("tls read: {e}")),
            }
            Ok(raw)
        }) {
            Ok(Ok(raw)) => raw,
            Ok(Err(e)) => return no_response(e),
            Err(e) => return no_response(format!("tls read: {e}")),
        };
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
        self.retry_gated(path, || self.exchange_h2(path, extra, client_ip))
    }

    fn exchange_h2(&self, path: &str, extra: &[(&str, &str)], client_ip: &str) -> Resp {
        let cfg = tls_config(&self.fx.cert_der, &[b"h2"]);
        let mut conn = match H2sTlsClientConn::connect("localhost", self.port(), cfg) {
            Ok(c) => c,
            Err(e) => return no_response(format!("h2 connect: {e}")),
        };
        let stream_id = match conn.dispatch(
            &get_req(path, extra, "HTTP/2"),
            "localhost",
            client_ip,
            "localhost",
        ) {
            Ok(stream_id) => stream_id,
            Err(e) => return no_response(format!("h2 dispatch: {e}")),
        };
        await_resp("h2", || {
            conn.drive();
            conn.drain_completions()
                .into_iter()
                .find(|c| c.stream_id == stream_id)
                .map(|c| c.result)
        })
    }

    /// One HTTP/3 GET over QUIC.
    fn h3(&self, path: &str, extra: &[(&str, &str)]) -> Resp {
        self.retry_gated(path, || self.exchange_h3(path, extra))
    }

    fn exchange_h3(&self, path: &str, extra: &[(&str, &str)]) -> Resp {
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
    // A safety net for a peer that goes silent, not a deadline for the
    // response: ten seconds closed the connection on a loaded host while the
    // server was answering.
    config.set_max_idle_timeout(testkit::RESPONSE_BOUND.as_millis() as u64);
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
    let deadline = Instant::now() + testkit::RESPONSE_BOUND;

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
    let run = Fixture::new(PRIVATE_ROUTE, "", AUTH).start();
    for (proto, r) in run.all("/private/x", &[]) {
        assert_eq!(r.status, 401, "{proto}: {}", r.text());
        assert_eq!(r.header("www-authenticate"), Some("Bearer"), "{proto}");
    }
    let bad = run.h1("/private/x", &[("Authorization", "Bearer not.a.token")]);
    assert_eq!(bad.status, 401);
    assert_eq!(bad.header("www-authenticate"), Some("Bearer"));
}

/// A `require` route asks the check backend: no token is 401 with no call, a
/// token whose identity has the role passes the check and reaches the absent
/// backend, which is a 502 or 503, and an identity without the role is 403.
#[test]
fn a_require_route_is_enforced_with_a_check_only_auth() {
    let run = Fixture::new(PRIVATE_ROUTE, "", AUTH).start();
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
fn auth_in_site_toml_is_refused() {
    let fx = Fixture::new("[auth]\nbackend = \"auth-check\"\n", "", "");
    let (code, output) = fx.run_until_exit();
    assert_eq!(code, Some(2), "output:\n{output}");
    assert!(
        output.contains("site.toml sets [auth]"),
        "output:\n{output}"
    );
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
    let run = Fixture::new(PRIVATE_ROUTE, "", &gated()).start();
    let auth = bearer(&run.token(&["monitor"]));
    for (proto, r) in run.all("/perf", &[("Authorization", &auth)]) {
        assert_report(proto, &r);
    }
}

#[test]
fn perf_reports_the_event_loop() {
    // The accounting tick is a fixed ten seconds in production. A debug build
    // can shorten it for this test through the test-only variable: the trace is
    // copied on the tick, which is the event this waits for.
    let run = Fixture::new(PRIVATE_ROUTE, "", &gated())
        .with_stats_interval(1)
        .start();
    let auth = bearer(&run.token(&["monitor"]));
    // A few requests so the loop has work to report on the accounting tick.
    for _ in 0..3 {
        assert_eq!(run.h1("/health", &[]).status, 200, "health answers");
    }
    // The trace is copied on the accounting tick, once per EMIT_INTERVAL_SECS.
    // Wait for the field to be non-empty, under one generous bound.
    let deadline = Instant::now() + testkit::RESPONSE_BOUND;
    let mut event_loop = None;
    while Instant::now() < deadline {
        let r = run.h1("/perf", &[("Authorization", &auth)]);
        assert_eq!(r.status, 200, "{}", r.text());
        let parsed: m6_core::monitoring::PerfReport =
            serde_json::from_slice(&r.body).expect("a report");
        if !parsed.metrics.event_loop.is_empty() {
            event_loop = Some(parsed.metrics.event_loop);
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let event_loop = event_loop.expect("event_loop never populated within the deadline");
    // The test drives HTTP/1.1 over TLS, so the `conn` trigger has events.
    let trigger = event_loop
        .iter()
        .find(|t| t.name == "conn" && t.count > 0)
        .expect("a conn trigger with events");
    assert!(trigger.work_max_ns >= trigger.work_min_ns, "{trigger:?}");
}

#[test]
fn an_unlisted_peer_gets_404_with_a_valid_token_over_h1_h2_h3() {
    let system = format!("{AUTH}\n{}", GATE.replace("127.0.0.1", "10.0.0.9"));
    let run = Fixture::new(PRIVATE_ROUTE, "", &system).start();
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
    let system = format!("{AUTH}\n{}", GATE.replace("127.0.0.1", "10.0.0.9"));
    let run = Fixture::new(PRIVATE_ROUTE, "", &system).start();
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
        PRIVATE_ROUTE,
        "h2c_bind = \"{H2C}\"\ninternal_binds = [\"{H2C}\"]\ninternal_peers = [\"127.0.0.1\"]\n",
        &gated(),
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
    let run = Fixture::new(PRIVATE_ROUTE, "", &gated()).start();
    for (proto, r) in run.all("/perf", &[]) {
        assert_eq!(r.status, 401, "{proto}: {}", r.text());
        assert_eq!(r.header("www-authenticate"), Some("Bearer"), "{proto}");
    }
}

#[test]
fn perf_with_a_token_lacking_the_role_is_403() {
    let run = Fixture::new(PRIVATE_ROUTE, "", &gated()).start();
    let auth = bearer(&run.token(&["reader"]));
    for (proto, r) in run.all("/perf", &[("Authorization", &auth)]) {
        assert_eq!(r.status, 403, "{proto}: {}", r.text());
        assert_eq!(r.text(), r#"{"error":"forbidden"}"#, "{proto}");
        assert_eq!(r.header("www-authenticate"), None, "{proto}");
    }
}

#[test]
fn perf_with_an_unknown_token_is_401() {
    let run = Fixture::new(PRIVATE_ROUTE, "", &gated()).start();
    let auth = bearer("not-a-known-token");
    for (proto, r) in run.all("/perf", &[("Authorization", &auth)]) {
        assert_eq!(r.status, 401, "{proto}: {}", r.text());
    }
}

#[test]
fn an_unknown_token_is_401() {
    let run = Fixture::new(PRIVATE_ROUTE, "", &gated()).start();
    let auth = bearer("expired-token");
    let r = run.h1("/perf", &[("Authorization", &auth)]);
    assert_eq!(r.status, 401, "{}", r.text());
}

/// The legacy bearer file is removed, not silently ignored: a node still
/// carrying the key does not start, and the message names it.
#[test]
fn the_removed_legacy_metrics_token_file_is_refused() {
    let system = format!("{AUTH}\n{GATE}metrics_token_file = \"{{KEY}}\"\n");
    let fx = Fixture::new(PRIVATE_ROUTE, "", &system);
    let (code, output) = fx.run_until_exit();
    assert_eq!(code, Some(2), "output:\n{output}");
    assert!(output.contains("metrics_token_file"), "output:\n{output}");
}

#[test]
fn traffic_follows_the_same_rule() {
    let run = Fixture::new(PRIVATE_ROUTE, "", &gated()).start();
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

/// `/traffic` reports the requests this process served, from the window it
/// keeps in memory. The analytics log is deleted before the scrape, so an
/// answer that still counts the five requests proves the file was not read.
///
/// `traffic_cache_s = 0` makes every scrape build the report, so the counts are
/// the window's and not a cached report's.
#[test]
fn traffic_reports_requests_served_since_start() {
    let site = format!("{PRIVATE_ROUTE}[health]\ntraffic_cache_s = 0\n");
    let fx = Fixture::logging(&site, "", &gated());
    // The fixture's backend is a placeholder with no listener. Replace it with
    // one that answers, so the requests are served and each logs an analytics
    // line, which is what puts it in the window. The pool reads its glob at
    // start, so the socket exists before the spawn either way.
    let backend_sock = fx.dir.path().join("b-1.sock");
    std::fs::remove_file(&backend_sock).expect("remove the placeholder socket");
    let listener =
        std::os::unix::net::UnixListener::bind(&backend_sock).expect("bind the backend socket");
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut stream = stream;
            let mut request = [0u8; 1024];
            let _ = stream.read(&mut request);
            // One request per connection, so the pool opens a fresh one.
            let _ = stream
                .write_all(b"HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Length: 2\r\n\r\nok");
            let _ = stream.flush();
        }
    });
    let run = fx.start();
    let auth = bearer(&run.token(&["monitor"]));
    // Five public requests. The status the client sees is the status the
    // analytics line records.
    let mut seen: std::collections::BTreeMap<u16, u64> = std::collections::BTreeMap::new();
    for _ in 0..5 {
        let r = run.h1("/", &[]);
        assert_ne!(r.status, 0, "a public request must be answered");
        *seen.entry(r.status).or_default() += 1;
    }
    // The window is memory, not the file: remove the log before the scrape.
    std::fs::remove_file(run.fx.dir.path().join("analytics.ndjson"))
        .expect("the analytics log exists while the process runs");
    let r = run.h1("/traffic", &[("Authorization", &auth)]);
    assert_eq!(r.status, 200, "{}", r.text());
    let report: m6_core::monitoring::TrafficReport =
        serde_json::from_slice(&r.body).expect("a traffic report");
    assert_eq!(report.total_requests, 5, "{}", r.text());
    assert_eq!(report.status, seen, "{}", r.text());
}

#[test]
fn health_stays_open_to_an_unlisted_peer() {
    let system = format!("{AUTH}\n{}", GATE.replace("127.0.0.1", "10.0.0.9"));
    let run = Fixture::new(PRIVATE_ROUTE, "", &system).start();
    for (proto, r) in run.all("/health", &[]) {
        // 200, or 503 when degraded: either way not 404 and not 401.
        assert!(r.status == 200 || r.status == 503, "{proto}: {}", r.status);
        assert!(r.text().contains("\"status\""), "{proto}: {}", r.text());
    }
}

#[test]
fn auth_and_health_keys_are_read_from_the_system_config() {
    let system = format!(
        "{AUTH}\n[health]\nmetrics_require = \"role:monitor\"\nmetrics_peers = [\"127.0.0.1\"]\n"
    );
    let run = Fixture::new("", "", &system).start();
    let auth = bearer(&run.token(&["monitor"]));
    assert_report("h1", &run.h1("/perf", &[("Authorization", &auth)]));
    assert_eq!(run.h1("/perf", &[]).status, 401);
}

/// Without `metrics_require` the endpoint does not exist, whatever credential
/// is presented: there is no second, legacy gate to fall back to.
#[test]
fn without_a_require_perf_is_disabled() {
    let run = Fixture::new("", "", "").start();
    let token = bearer(&run.token(&["monitor"]));
    for headers in [vec![], vec![("Authorization", token.as_str())]] {
        assert_eq!(run.h1("/perf", &headers).status, 404);
    }
}

#[test]
fn a_public_metrics_peer_refuses_to_start() {
    let system = format!("{AUTH}\n{}", GATE.replace("127.0.0.1", "203.0.113.7"));
    let (code, output) = Fixture::new(PRIVATE_ROUTE, "", &system).run_until_exit();
    assert_eq!(code, Some(2), "output:\n{output}");
    assert!(output.contains("not in a local range"), "output:\n{output}");
}

// ── One checker, one refusal ────────────────────────────────────────────────
//
// Four routes gate on a token: a `require` route, `/perf`, `/traffic` and
// `/genid`. Each calls the one token check exactly once, and each answers its
// refusal from the one definition. These tests are the contract the single
// checker has to keep:
//
// - no credential is 401, with `WWW-Authenticate: Bearer`, on every route;
// - a credential that verifies but lacks the required role is 403, without
//   `WWW-Authenticate`, on every route;
// - a credential that verifies and carries the role passes the check, and
//   what happens next is the route's own business (a backend call, a report);
// - the same request runs the check once, not once per layer.
//
// The route block legitimately combines the check with two route-level
// choices, stated here rather than flattened: a browser with no credential is
// redirected to login, and a browser lacking the role gets the site's 403
// error page. The check's status and headers are still the same; the body is
// the site's presentation, which an operator configures.

/// `[health]` and `[genid]` for the gates, both asking for the monitor role.
const GATES: &str = "[health]\nmetrics_require = \"role:monitor\"\n\n\
    [genid]\nread_require = \"role:monitor\"\nwrite_require = \"role:monitor\"\n";

fn all_gated() -> String {
    format!("{AUTH}\n{GATES}")
}

#[test]
fn every_token_gated_route_refuses_with_the_same_bytes() {
    let run = Fixture::new(PRIVATE_ROUTE, "", &all_gated()).start();
    let good = bearer(&run.token(&["monitor"]));
    let wrong = bearer(&run.token(&["reader"]));

    for path in ["/private/x", "/perf", "/traffic", "/genid"] {
        // Without a credential: 401, one body, one scheme hint.
        //
        // Break: give any route its own 401 construction and its body stops
        // matching this one while the status stays right.
        let r = run.h1(path, &[]);
        assert_eq!(r.status, 401, "{path}: {}", r.text());
        assert_eq!(r.text(), r#"{"error":"unauthorised"}"#, "{path}");
        assert_eq!(r.header("www-authenticate"), Some("Bearer"), "{path}");

        // With a token carrying the role: the check passes. What the route
        // does next differs by design: `/private/x` reaches a backend with no
        // listener (503), the other three build their own answer (200).
        //
        // Break: make the route block re-check the token and a valid one can
        // still be refused here.
        let r = run.h1(path, &[("Authorization", &good)]);
        assert!(
            r.status != 401 && r.status != 403,
            "{path} refused a valid token: {} {}",
            r.status,
            r.text()
        );

        // With a token for the wrong audience: 403, the same body, and no
        // scheme hint, because the credential did verify.
        //
        // Break: answer 401 here and a verified credential is reported as no
        // credential, which is the second check wearing this one's name.
        let r = run.h1(path, &[("Authorization", &wrong)]);
        assert_eq!(r.status, 403, "{path}: {}", r.text());
        assert_eq!(r.text(), r#"{"error":"forbidden"}"#, "{path}");
        assert_eq!(r.header("www-authenticate"), None, "{path}");
    }
}

/// The browser choices are the route block's, not the check's: same status,
/// site presentation. This is the "combines the token with something else"
/// case, kept rather than flattened.
#[test]
fn the_route_block_keeps_its_browser_redirect_and_error_page() {
    let run = Fixture::new(PRIVATE_ROUTE, "", &all_gated()).start();
    let browser = [("Accept", "text/html")];
    let r = run.h1("/private/x", &browser);
    assert_eq!(r.status, 302, "a browser with no session logs in");
    assert!(r
        .header("location")
        .unwrap_or("")
        .starts_with("/login?next="));

    let wrong = bearer(&run.token(&["reader"]));
    let r = run.h1(
        "/private/x",
        &[("Accept", "text/html"), ("Authorization", &wrong)],
    );
    assert_eq!(r.status, 403, "the browser's status is the check's status");
    assert!(
        r.text().contains("403 Forbidden"),
        "the browser gets the site's error page, not the JSON refusal: {}",
        r.text()
    );
}

/// One call per request, counted rather than assumed: the check logs exactly
/// one `auth check` line, wherever it is called from.
///
/// Break: add a second call in the route block after the first, and the count
/// for `/private/x` becomes two while every other assertion still passes.
///
/// The request is not retried on the design's 503 here. The count is per
/// request, so a retry would count a second request's one check and look like
/// a second check on this one. A 503 from a missed deadline still logs one
/// `auth check`, which is the fact this test asserts.
#[test]
fn one_auth_check_per_protected_request() {
    let run = Fixture::new(PRIVATE_ROUTE, "", &all_gated())
        .with_debug()
        .start();
    let good = bearer(&run.token(&["monitor"]));
    for path in ["/private/x", "/perf", "/traffic", "/genid"] {
        let before = run.auth_check_count();
        let r = run.exchange_h1(path, &[("Authorization", &good)]);
        assert!(
            r.status != 401 && r.status != 403,
            "{path} refused a valid token: {}",
            r.status
        );
        assert!(
            testkit::wait::until(m6_core::testkit::STARTUP_BOUND, || run.auth_check_count()
                > before),
            "{path} logged no `auth check`\n{}",
            run.svc.output()
        );
        assert_eq!(
            run.auth_check_count() - before,
            1,
            "{path} must run the one check once\n{}",
            run.svc.output()
        );
    }
}
