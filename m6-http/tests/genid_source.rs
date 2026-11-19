//! The source node's generation ID: who may read and advance it, and that a
//! `POST /genid` refreshes every backend before it serves the new generation.
//!
//! Each test starts a real `m6-http` on loopback in front of real backends:
//! `m6-html` over a Unix socket for the content cases, and small in-file Unix
//! backends where a test needs exact control of when a reply lands. The
//! credentials are opaque strings answered by a fake `/check` authority in the
//! fixture. The unit's StateDirectory is a temp directory.
//!
//! Breaks are named on the tests that carry one. Nothing here waits on a fixed
//! sleep for a result: every wait polls a condition with a deadline.

#![allow(
    clippy::disallowed_methods,
    reason = "test code: threads, sleeps and blocking clients drive the service from outside"
)]

use std::io::{Read, Write};
use std::net::TcpStream;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rustls::StreamOwned;

use m6_core::testkit::{binary, claim_port, PortClaim, Service};

#[path = "support/check.rs"]
mod check_support;
use check_support::{identity, Reply, RunningCheck};
use m6_http_lib::forward::HttpRequest;
use m6_http_lib::h2c_client::H2cClientConn;
use m6_http_lib::h2s_client::H2sTlsClientConn;

// ── Responses ─────────────────────────────────────────────────────────────────

struct Resp {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Resp {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    /// The `Retry-After` a 503 carries, for a retry that honours it.
    fn retry_after(&self) -> Option<Duration> {
        check_support::retry_after(self.header("retry-after"))
    }

    /// The body's `genid`, from the tiny JSON the endpoint answers.
    fn genid(&self) -> String {
        let text = self.text();
        text.split("\"genid\":\"")
            .nth(1)
            .unwrap_or("")
            .split('"')
            .next()
            .unwrap_or("")
            .to_string()
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

/// One fixture: a site directory, a system config and the claimed ports. The
/// backends are started by the test, because which ones it needs is the test's
/// own choice.
struct Fixture {
    dir: tempfile::TempDir,
    site: PathBuf,
    system: PathBuf,
    port: PortClaim,
    h2c: PortClaim,
    cert_der: Vec<u8>,
    state_dir: PathBuf,
    _check: RunningCheck,
    html_sock: PathBuf,
}

impl Fixture {
    /// `site_extra` is the site's `[[backend]]`, route and `[[route_group]]`
    /// tables, and may name `{DIR}`. `server_extra` adds lines inside the
    /// system config's `[server]` table. `html` is written as `m6-html`'s own
    /// config when given. `timeout_s` is the socket backend deadline.
    fn new(site_extra: &str, server_extra: &str, html: Option<&str>, timeout_s: u64) -> Fixture {
        let dir = tempfile::tempdir_in(m6_core::testkit::socket_scratch_root()).expect("tempdir");
        let d = dir.path();
        let ck = rcgen::generate_simple_self_signed(vec![
            "localhost".to_string(),
            "127.0.0.1".to_string(),
        ])
        .expect("rcgen");
        std::fs::write(d.join("cert.pem"), ck.cert.pem()).unwrap();
        std::fs::write(d.join("key.pem"), ck.key_pair.serialize_pem()).unwrap();
        let check_path = d.join("check.sock");
        let check = RunningCheck::start(
            &check_path,
            vec![
                (
                    "read-token".to_string(),
                    Reply::Identity(identity(&[], &["reader"])),
                ),
                (
                    "write-token".to_string(),
                    Reply::Identity(identity(&[], &["writer"])),
                ),
            ],
        )
        .unwrap();

        let port = claim_port();
        let h2c = claim_port();
        let site_dir = d.join("site");
        std::fs::create_dir_all(site_dir.join("templates")).unwrap();
        std::fs::create_dir_all(site_dir.join("data")).unwrap();
        std::fs::create_dir_all(site_dir.join("posts")).unwrap();
        std::fs::create_dir_all(site_dir.join("configs")).unwrap();
        std::fs::write(site_dir.join("templates/index.html"), "first: {{ title }}").unwrap();
        std::fs::write(site_dir.join("templates/posts.html"), "a post page").unwrap();
        std::fs::write(site_dir.join("data/page.json"), "{\"title\": \"one\"}").unwrap();
        std::fs::write(site_dir.join("data/site.json"), "{}").unwrap();
        if let Some(conf) = html {
            std::fs::write(site_dir.join("configs/m6-html.conf"), conf).unwrap();
        }

        let html_sock = d.join("html-1.sock");
        let sub = |s: &str| {
            s.replace("{DIR}", &d.display().to_string())
                .replace("{H2C}", &h2c.addr())
                .replace("{PORT}", &port.addr())
        };
        std::fs::write(
            site_dir.join("site.toml"),
            sub(&format!(
                "[site]\nname = \"genid-source\"\ndomain = \"localhost\"\n\n\
                 [errors]\nmode = \"internal\"\n\n\
                 [analytics]\nenabled = false\n\n\
                 [log]\nlevel = \"info\"\n\n\
                 [rate_limit]\nenabled = false\n\n\
                 [server]\nbackend_timeout_secs = {timeout_s}\n\n\
                 [[backend]]\nname = \"auth-check\"\nsockets = \"{{DIR}}/check.sock\"\n\n\
                 {site_extra}"
            )),
        )
        .unwrap();

        let state_dir = d.join("state");
        std::fs::create_dir_all(&state_dir).unwrap();
        std::fs::write(
            d.join("system.toml"),
            sub(&format!(
                "[server]\nbind = \"{{PORT}}\"\ntls_cert = \"{{DIR}}/cert.pem\"\ntls_key = \"{{DIR}}/key.pem\"\n{server_extra}\n\n\
                 [node]\nname = \"genid-source\"\n\n\
                 [auth]\nbackend = \"auth-check\"\n\n\
                 [genid]\nread_require = \"role:reader\"\nwrite_require = \"role:writer\"\n"
            )),
        )
        .unwrap();

        Fixture {
            site: site_dir,
            system: d.join("system.toml"),
            port,
            h2c,
            cert_der: ck.cert.der().to_vec(),
            state_dir,
            _check: check,
            html_sock,
            dir,
        }
    }

    fn read_token(&self) -> String {
        "read-token".to_string()
    }

    fn write_token(&self) -> String {
        "write-token".to_string()
    }

    fn start_html(&self) -> Service {
        let sock = self.html_sock.to_str().unwrap();
        let mut svc = Service::spawn(
            "m6-html-genid",
            Command::new(binary("m6-html"))
                .args([
                    self.site.to_str().unwrap(),
                    self.site.join("configs/m6-html.conf").to_str().unwrap(),
                    "--log-level",
                    "info",
                ])
                .env("M6_SOCKET_OVERRIDE", sock),
        );
        svc.wait_for_path(&self.html_sock, m6_core::testkit::STARTUP_BOUND);
        svc
    }

    fn command(&self, state: bool) -> Command {
        let mut cmd = Command::new(binary("m6-http"));
        cmd.arg(&self.site).arg(&self.system);
        if state {
            cmd.env(m6_core::platform::STATE_DIR_VAR, &self.state_dir);
        }
        cmd.env("RUST_LOG", "info")
            .stdin(Stdio::null())
            .current_dir(self.dir.path());
        cmd
    }

    fn start_http(&self) -> Running {
        let mut svc = Service::spawn("m6-http-genid", &mut self.command(true));
        svc.wait_for_tcp(self.port.port(), m6_core::testkit::STARTUP_BOUND);
        let running = Running { svc };
        let ready = m6_core::testkit::wait::until(m6_core::testkit::STARTUP_BOUND, || {
            self.h1_wire(&self.h1_get("/health")).status != 0
        });
        assert!(
            ready,
            "m6-http never answered /health\n{}",
            running.svc.output()
        );
        running
    }

    /// Run m6-http to its exit, which must come within ten seconds.
    fn run_until_exit(&self, state: bool, extra: &[&str]) -> (Option<i32>, String) {
        let mut cmd = self.command(state);
        for arg in extra {
            cmd.arg(arg);
        }
        let mut child = m6_core::testkit::process::Running::spawn(&mut cmd).expect("spawn m6-http");
        let exited = m6_core::testkit::wait::until(m6_core::testkit::STARTUP_BOUND, || {
            child.child().try_wait().expect("try_wait").is_some()
        });
        assert!(
            exited,
            "m6-http was still running after 10s; it should have refused to start"
        );
        let out = child.take().wait_with_output().expect("output");
        (
            out.status.code(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    }

    // ── Clients ──────────────────────────────────────────────────────────────

    fn h1_wire(&self, request: &str) -> Resp {
        let tcp = match TcpStream::connect(("127.0.0.1", self.port.port())) {
            Ok(t) => t,
            Err(e) => return no_response(format!("connect: {e}")),
        };
        tcp.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
        tcp.set_write_timeout(Some(Duration::from_secs(20)))
            .unwrap();
        let name = rustls::pki_types::ServerName::try_from("127.0.0.1".to_string()).unwrap();
        let conn = rustls::ClientConnection::new(tls_config(&self.cert_der, &[]), name).unwrap();
        let mut stream = StreamOwned::new(conn, tcp);
        if let Err(e) = stream
            .write_all(request.as_bytes())
            .and_then(|()| stream.flush())
        {
            return no_response(format!("write: {e}"));
        }
        let mut raw = Vec::new();
        match stream.read_to_end(&mut raw) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {}
            Err(e) => return no_response(format!("read: {e}")),
        }
        parse_h1(&raw)
    }

    fn h1_get(&self, path: &str) -> String {
        format!(
            "GET {path} HTTP/1.1\r\nHost: localhost:{}\r\nConnection: close\r\n\r\n",
            self.port.port()
        )
    }

    fn h1_request(&self, method: &str, path: &str, token: Option<&str>) -> Resp {
        self.retry_gated(path, || self.exchange_h1_request(method, path, token))
    }

    fn exchange_h1_request(&self, method: &str, path: &str, token: Option<&str>) -> Resp {
        let mut request = format!(
            "{method} {path} HTTP/1.1\r\nHost: localhost:{}\r\nContent-Length: 0\r\nConnection: close\r\n",
            self.port.port()
        );
        if let Some(token) = token {
            request.push_str(&format!("Authorization: Bearer {token}\r\n"));
        }
        request.push_str("\r\n");
        self.h1_wire(&request)
    }

    /// Retry an answer from a check-gated endpoint while it is the design's
    /// 503 for a check that missed its 50 ms deadline.
    ///
    /// A slow check is not a behaviour this suite tests, so the call is
    /// repeated under the testkit bound and honours `Retry-After`. Any other
    /// path, and any other status, is answered once.
    fn retry_gated(&self, path: &str, mut attempt: impl FnMut() -> Resp) -> Resp {
        if check_support::check_gated(path) {
            check_support::retry_unavailable(m6_core::testkit::RESPONSE_BOUND, || {
                let r = attempt();
                (r.status, r.retry_after(), r)
            })
        } else {
            attempt()
        }
    }

    fn h2(&self, method: &str, path: &str, token: Option<&str>, client_ip: &str) -> Resp {
        self.retry_gated(path, || self.exchange_h2(method, path, token, client_ip))
    }

    fn exchange_h2(&self, method: &str, path: &str, token: Option<&str>, client_ip: &str) -> Resp {
        let cfg = tls_config(&self.cert_der, &[b"h2"]);
        let mut conn = match H2sTlsClientConn::connect("localhost", self.port.port(), cfg) {
            Ok(c) => c,
            Err(e) => return no_response(format!("h2 connect: {e}")),
        };
        let mut headers = Vec::new();
        if let Some(token) = token {
            headers.push(("Authorization".to_string(), format!("Bearer {token}")));
        }
        let req = HttpRequest {
            method: method.to_string(),
            path: path.to_string(),
            query: None,
            version: "HTTP/2".to_string(),
            headers,
            body: Vec::new(),
        };
        let stream_id = match conn.dispatch(&req, "localhost", client_ip, "localhost") {
            Ok(id) => id,
            Err(e) => return no_response(format!("h2 dispatch: {e}")),
        };
        await_resp(|| {
            conn.drive();
            conn.drain_completions()
                .into_iter()
                .find(|c| c.stream_id == stream_id)
                .map(|c| c.result)
        })
    }

    fn h2c(&self, path: &str, token: Option<&str>, client_ip: &str) -> Resp {
        self.retry_gated(path, || self.exchange_h2c(path, token, client_ip))
    }

    fn exchange_h2c(&self, path: &str, token: Option<&str>, client_ip: &str) -> Resp {
        let mut conn = match H2cClientConn::connect("127.0.0.1", self.h2c.port()) {
            Ok(c) => c,
            Err(e) => return no_response(format!("h2c connect: {e}")),
        };
        let settle = Instant::now() + Duration::from_millis(250);
        while Instant::now() < settle {
            conn.drive();
            std::thread::sleep(Duration::from_millis(1));
        }
        let mut headers = Vec::new();
        if let Some(token) = token {
            headers.push(("Authorization".to_string(), format!("Bearer {token}")));
        }
        let req = HttpRequest {
            method: "GET".to_string(),
            path: path.to_string(),
            query: None,
            version: "HTTP/1.1".to_string(),
            headers,
            body: Vec::new(),
        };
        let stream_id = match conn.dispatch(&req, "127.0.0.1", client_ip, "localhost") {
            Ok(id) => id,
            Err(e) => return no_response(format!("h2c dispatch: {e}")),
        };
        await_resp(|| {
            conn.drive();
            conn.drain_completions()
                .into_iter()
                .find(|c| c.stream_id == stream_id)
                .map(|c| c.result)
        })
    }
}

struct Running {
    svc: Service,
}

impl Running {
    fn output(&self) -> String {
        self.svc.output()
    }

    fn stop(&mut self) {
        self.svc.kill();
    }
}

fn no_response(reason: impl std::fmt::Display) -> Resp {
    Resp {
        status: 0,
        headers: vec![("x-test-error".to_string(), reason.to_string())],
        body: Vec::new(),
    }
}

fn await_resp(
    mut poll: impl FnMut() -> Option<std::io::Result<m6_http_lib::forward::HttpResponse>>,
) -> Resp {
    let deadline = Instant::now() + m6_core::testkit::STARTUP_BOUND;
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
            Some(Err(e)) => return no_response(format!("response: {e}")),
            None if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(1)),
            None => return no_response("timed out"),
        }
    }
}

fn parse_h1(raw: &[u8]) -> Resp {
    let end = raw.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4);
    let Some(end) = end else {
        return no_response("no response head");
    };
    let text = String::from_utf8_lossy(&raw[..end]);
    let mut lines = text.split("\r\n");
    let status = lines
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let headers = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    Resp {
        status,
        headers,
        body: raw[end..].to_vec(),
    }
}

// ── Small Unix backends ───────────────────────────────────────────────────────

/// Answer `POST /genid` with 204 and every other request with `200 ok`.
fn spawn_echo_backend(path: &Path) {
    let listener = UnixListener::bind(path).expect("bind echo backend");
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            std::thread::spawn(move || {
                let head = read_head(&mut stream);
                if head.starts_with("POST /genid ") {
                    let _ = stream.write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                } else {
                    let _ = stream.write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nCache-Control: public\r\nConnection: close\r\n\r\nok",
                    );
                }
            });
        }
    });
}

/// A backend that answers `POST /genid` at once and holds the first
/// `GET /slow` until it is released. Records how many `/slow` requests arrived.
struct HoldingBackend {
    held: std::sync::mpsc::Receiver<()>,
    release: std::sync::mpsc::Sender<()>,
    gets: Arc<AtomicUsize>,
}

fn spawn_holding_backend(path: &Path) -> HoldingBackend {
    let listener = UnixListener::bind(path).expect("bind holding backend");
    let (held_tx, held_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let release_rx = Arc::new(Mutex::new(release_rx));
    let gets = Arc::new(AtomicUsize::new(0));
    let gets_thread = Arc::clone(&gets);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let held_tx = held_tx.clone();
            let release_rx = Arc::clone(&release_rx);
            let gets = Arc::clone(&gets_thread);
            std::thread::spawn(move || {
                let head = read_head(&mut stream);
                if head.starts_with("POST /genid ") {
                    let _ = stream.write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                    return;
                }
                let n = gets.fetch_add(1, Ordering::SeqCst);
                if n == 0 {
                    let _ = held_tx.send(());
                    let _ = release_rx
                        .lock()
                        .unwrap()
                        .recv_timeout(m6_core::testkit::STARTUP_BOUND);
                }
                let _ = stream.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nCache-Control: public\r\nConnection: close\r\n\r\nslow",
                );
            });
        }
    });
    HoldingBackend {
        held: held_rx,
        release: release_tx,
        gets,
    }
}

/// A listener that accepts and holds without ever answering, to make one
/// member miss the backend deadline.
fn spawn_hanging_backend(path: &Path) {
    let listener = UnixListener::bind(path).expect("bind hanging backend");
    std::thread::spawn(move || {
        let mut held = Vec::new();
        for stream in listener.incoming().flatten() {
            held.push(stream);
        }
    });
}

fn read_head(stream: &mut std::os::unix::net::UnixStream) -> String {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 1024];
    loop {
        match stream.read(&mut tmp) {
            Ok(0) => break,
            Ok(n) => {
                buf.extend_from_slice(&tmp[..n]);
                if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    String::from_utf8_lossy(&buf).into_owned()
}

// ── Tests ────────────────────────────────────────────────────────────────────

/// `GET /genid` needs the read claim, and its body and header agree.
///
/// Break: let a write-only token through, and the 403 case returns 200.
#[test]
fn get_genid_needs_the_read_claim() {
    let fx = Fixture::new("", "", None, 5);
    let _http = fx.start_http();

    assert_eq!(fx.h1_request("GET", "/genid", None).status, 401);
    assert_eq!(
        fx.h1_request("GET", "/genid", Some(&fx.write_token()))
            .status,
        403
    );
    let read = fx.h1_request("GET", "/genid", Some(&fx.read_token()));
    assert_eq!(read.status, 200);
    assert_eq!(read.genid(), read.header("m6-genid").unwrap_or(""));
    assert_eq!(read.header("cache-control"), Some("no-store"));

    let over_h2 = fx.h2("GET", "/genid", Some(&fx.read_token()), "127.0.0.1");
    assert_eq!(over_h2.status, 200);
    assert_eq!(over_h2.header("m6-genid"), Some("0"));
}

/// `POST /genid` needs the write claim, advances by one, and a later GET sees
/// the new ID.
///
/// Break: let the read token post, and the 403 case returns 200.
#[test]
fn post_genid_needs_the_write_claim() {
    let fx = Fixture::new("", "", None, 5);
    let _http = fx.start_http();

    assert_eq!(
        fx.h1_request("POST", "/genid", Some(&fx.read_token()))
            .status,
        403
    );
    let posted = fx.h1_request("POST", "/genid", Some(&fx.write_token()));
    assert_eq!(posted.status, 200);
    assert_eq!(posted.genid(), "1");
    assert_eq!(posted.header("m6-genid"), Some("1"));
    assert_eq!(
        fx.h1_request("GET", "/genid", Some(&fx.read_token()))
            .genid(),
        "1"
    );

    let posted_h2 = fx.h2("POST", "/genid", Some(&fx.write_token()), "127.0.0.1");
    assert_eq!(posted_h2.status, 200);
    assert_eq!(posted_h2.genid(), "2");
    assert_eq!(
        fx.h1_request("GET", "/genid", Some(&fx.read_token()))
            .genid(),
        "2"
    );
}

/// A publish reaches the content in order: the old generation keeps serving
/// until the POST has done its work, then the new one is served with its ID.
///
/// Break: skip `cache.clear()` in the change step, and the last GET still says
/// `first: one`.
#[test]
fn a_publish_reaches_the_cache_in_order() {
    let html = "global_params = [\"data/page.json\"]\n\n\
                [[route]]\npath = \"/\"\ntemplate = \"templates/index.html\"\n";
    let fx = Fixture::new(
        "[[backend]]\nname = \"m6-html\"\nsockets = \"{DIR}/html-*.sock\"\n\n\
         [[route]]\npath = \"/\"\nbackend = \"m6-html\"\n",
        "",
        Some(html),
        5,
    );
    let html_svc = fx.start_html();
    let _http = fx.start_http();

    let first = fx.h1_request("GET", "/", Some(&fx.read_token()));
    assert_eq!(first.status, 200, "{}", first.text());
    assert_eq!(first.text(), "first: one");
    let second = fx.h1_request("GET", "/", Some(&fx.read_token()));
    assert_eq!(second.text(), "first: one");
    assert_eq!(second.header("m6-genid"), Some("0"));

    std::fs::write(fx.site.join("data/page.json"), "{\"title\": \"two\"}").unwrap();
    let before = fx.h1_request("GET", "/", Some(&fx.read_token()));
    assert_eq!(
        before.text(),
        "first: one",
        "the publish has not happened yet"
    );

    let posted = fx.h1_request("POST", "/genid", Some(&fx.write_token()));
    assert_eq!(posted.status, 200);
    assert_eq!(posted.genid(), "1");

    let after = fx.h1_request("GET", "/", Some(&fx.read_token()));
    assert_eq!(after.status, 200);
    assert_eq!(after.text(), "first: two");
    assert_eq!(after.header("m6-genid"), Some("1"));

    // The backend writes one access line for its own reload, which is what an
    // operator sees the publish as.
    let logged = m6_core::testkit::wait::until(m6_core::testkit::STARTUP_BOUND, || {
        let out = html_svc.output();
        out.contains("/genid") && out.contains("request complete")
    });
    assert!(
        logged,
        "no reload access line in the backend log:\n{}",
        html_svc.output()
    );
}

/// A response dispatched before an advance is not stored: it keeps the old
/// generation's header, and the next request reaches the backend again.
///
/// Break: drop the `ctx.genid == current` store check, and the second `/slow`
/// is a cache hit, so the backend sees one request and not two.
#[test]
fn a_response_dispatched_before_the_advance_is_not_stored() {
    let fx = Fixture::new(
        "[[backend]]\nname = \"slow\"\nsockets = \"{DIR}/slow-*.sock\"\n\n\
         [[route]]\npath = \"/slow\"\nbackend = \"slow\"\n",
        "",
        None,
        5,
    );
    let backend = spawn_holding_backend(&fx.dir.path().join("slow-1.sock"));
    let _http = fx.start_http();

    let slow = std::thread::scope(|scope| {
        let slow = scope.spawn(|| fx.h1_request("GET", "/slow", None));
        backend
            .held
            .recv_timeout(m6_core::testkit::STARTUP_BOUND)
            .expect("the slow request never reached the backend");
        let posted = fx.h1_request("POST", "/genid", Some(&fx.write_token()));
        assert_eq!(posted.status, 200);
        backend.release.send(()).unwrap();
        slow.join().expect("the slow client")
    });
    assert_eq!(slow.status, 200);
    assert_eq!(slow.text(), "slow");
    assert_eq!(
        slow.header("m6-genid"),
        Some("0"),
        "the held response carries the generation it was dispatched under"
    );

    // The held reply was not stored: the next request reaches the backend.
    let after = fx.h1_request("GET", "/slow", None);
    assert_eq!(after.status, 200);
    assert_eq!(after.text(), "slow");
    assert_eq!(
        backend.gets.load(Ordering::SeqCst),
        2,
        "the pre-advance response must not have filled the cache"
    );
}

/// A publish rebuilds the route table from the config loaded at start, so a
/// file a `[[route_group]]` glob now matches gains its route.
///
/// Break: skip the route-table rebuild in the change step, and `/posts/new`
/// stays 404.
#[test]
fn a_publish_rebuilds_the_route_table() {
    let fx = Fixture::new(
        "[[backend]]\nname = \"echo\"\nsockets = \"{DIR}/echo-*.sock\"\n\n\
         [[route_group]]\nglob = \"posts/*.html\"\npath = \"/posts/{stem}\"\nbackend = \"echo\"\n",
        "",
        None,
        5,
    );
    spawn_echo_backend(&fx.dir.path().join("echo-1.sock"));
    let _http = fx.start_http();

    assert_eq!(fx.h1_request("GET", "/posts/new", None).status, 404);
    std::fs::write(fx.site.join("posts/new.html"), "new").unwrap();
    let posted = fx.h1_request("POST", "/genid", Some(&fx.write_token()));
    assert_eq!(posted.status, 200);
    assert_eq!(fx.h1_request("GET", "/posts/new", None).status, 200);
}

/// A member that never answers does not stop the advance. The POST succeeds,
/// the operator sees the member named and `failed = 1`, and the remaining
/// member's content is the new generation.
#[test]
fn a_failed_member_does_not_stop_the_advance() {
    let html = "global_params = [\"data/page.json\"]\n\n\
                [[route]]\npath = \"/\"\ntemplate = \"templates/index.html\"\n";
    let fx = Fixture::new(
        "[[backend]]\nname = \"m6-html\"\nsockets = \"{DIR}/html-*.sock\"\n\n\
         [[route]]\npath = \"/\"\nbackend = \"m6-html\"\n\n\
         [[backend]]\nname = \"hang\"\nsockets = \"{DIR}/hang-*.sock\"\n",
        "",
        Some(html),
        1,
    );
    let _html = fx.start_html();
    spawn_hanging_backend(&fx.dir.path().join("hang-1.sock"));
    let http = fx.start_http();

    let started = Instant::now();
    let posted = fx.h1_request("POST", "/genid", Some(&fx.write_token()));
    assert_eq!(posted.status, 200);
    assert_eq!(posted.genid(), "1");
    assert!(
        started.elapsed() >= Duration::from_millis(900),
        "the deadline must be the reason the POST took this long"
    );

    let logged = m6_core::testkit::wait::until(m6_core::testkit::STARTUP_BOUND, || {
        let out = http.output();
        out.contains("\"failed\":1") && out.contains("genid advanced")
    });
    assert!(
        logged,
        "no failed-member line in the log:\n{}",
        http.output()
    );
    assert!(
        http.output().contains("hang-1.sock"),
        "the failure names the member's socket:\n{}",
        http.output()
    );
}

/// A reachable internal peer may read the ID. A public address gets the 404 an
/// unknown path gets, before its token is read, which the gate's unit test
/// proves; this proves the internal listener is a real way in.
#[test]
fn an_internal_peer_reads_genid() {
    let fx = Fixture::new(
        "",
        "h2c_bind = \"{H2C}\"\ninternal_binds = [\"{H2C}\"]\ninternal_peers = [\"127.0.0.1\"]",
        None,
        5,
    );
    let _http = fx.start_http();
    let resp = fx.h2c("/genid", Some(&fx.read_token()), "10.0.0.9");
    assert_eq!(resp.status, 200, "{}", resp.text());
    assert_eq!(resp.header("m6-genid"), Some("0"));
}

/// No `[genid]` table is no endpoint: the path is the 404 every unknown path
/// gets, to a loopback caller with a valid token as much as to anyone.
#[test]
fn genid_is_404_when_unconfigured() {
    let fx = Fixture::new("", "", None, 5);
    // Rewrite the system config without `[genid]`.
    let system = std::fs::read_to_string(&fx.system).unwrap();
    let without = system
        .split("[genid]")
        .next()
        .unwrap_or(&system)
        .to_string();
    std::fs::write(&fx.system, without).unwrap();
    let _http = fx.start_http();
    assert_eq!(
        fx.h1_request("GET", "/genid", Some(&fx.read_token()))
            .status,
        404
    );
    assert_eq!(
        fx.h1_request("POST", "/genid", Some(&fx.write_token()))
            .status,
        404
    );
}

/// The ID survives a restart, because the file is the only place it lives.
///
/// Break: advance the ID at start, and the restarted node reports 2.
#[test]
fn a_restart_keeps_the_id() {
    let fx = Fixture::new("", "", None, 5);
    let mut http = fx.start_http();
    let posted = fx.h1_request("POST", "/genid", Some(&fx.write_token()));
    assert_eq!(posted.status, 200);
    assert_eq!(posted.genid(), "1");
    http.stop();
    drop(http);

    let _http = fx.start_http();
    assert_eq!(
        fx.h1_request("GET", "/genid", Some(&fx.read_token()))
            .genid(),
        "1"
    );
}

/// A source with `[genid]` and no StateDirectory refuses to start, and the
/// same config dumped without one prints and exits 0: the check belongs to
/// serving, not to reading the config.
///
/// Break: move the check into `config::load`, and `--dump-config` exits 2.
#[test]
fn a_source_without_a_state_directory_exits_2() {
    let fx = Fixture::new("", "", None, 5);
    let (code, stderr) = fx.run_until_exit(false, &[]);
    assert_eq!(code, Some(2));
    assert!(
        stderr.contains("[genid] needs the unit's StateDirectory="),
        "{stderr}"
    );

    let (code, stderr) = fx.run_until_exit(false, &["--dump-config"]);
    assert_eq!(code, Some(0));
    assert!(stderr.is_empty(), "the dump must not warn: {stderr}");
}
