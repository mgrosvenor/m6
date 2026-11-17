//! A cache node follows its source's generation ID, and serves what it holds
//! while the source is down.
//!
//! Every test starts a real `m6-http` source in front of a real `m6-html`, and a
//! second `m6-http` cache node whose `[genid] upstream` names the source and
//! whose routes point at it. The tokens are ES256 JWTs minted here, verified by
//! the source with a credential a fake `/check` authority answers. Nothing waits on
//! a fixed sleep for a result: every wait polls a condition with a deadline.
//!
//! Breaks are named on the tests that carry one.

#![allow(
    clippy::disallowed_methods,
    reason = "test code: threads, sleeps and blocking clients drive the services from outside"
)]

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rustls::StreamOwned;

use m6_core::testkit::{binary, claim_port, PortClaim, Service};

#[path = "support/check.rs"]
mod check_support;
use check_support::{identity, Reply, RunningCheck};

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

fn no_response(reason: impl std::fmt::Display) -> Resp {
    Resp {
        status: 0,
        headers: vec![("x-test-error".to_string(), reason.to_string())],
        body: Vec::new(),
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

// ── The stack under test ──────────────────────────────────────────────────────

/// A source `m6-http` and a cache `m6-http` in front of it, with the ports,
/// credentials and state directory they need. Services are fields so they die
/// before the directory they serve from is removed.
struct Stack {
    html: Option<Service>,
    source: Option<Service>,
    cache: Option<Service>,
    dir: tempfile::TempDir,
    source_site: PathBuf,
    source_system: PathBuf,
    cache_system: PathBuf,
    state_dir: PathBuf,
    source_port: PortClaim,
    cache_port: PortClaim,
    _check: RunningCheck,
}

impl Stack {
    /// `html_cache` is the `Cache-Control` the source's backend stamps on `/`
    /// and `/other`. `heartbeat_secs` is the cache node's interval.
    fn new(html_cache: &str, heartbeat_secs: u64, warm_on_start: bool) -> Stack {
        rustls::crypto::aws_lc_rs::default_provider()
            .install_default()
            .ok();
        let dir = tempfile::tempdir().expect("tempdir");
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
                (
                    "monitor-token".to_string(),
                    Reply::Identity(identity(&[], &["monitor"])),
                ),
            ],
        )
        .unwrap();
        std::fs::write(d.join("genid-token"), "read-token\n").unwrap();

        let source_port = claim_port();
        let cache_port = claim_port();

        // ── The source's site and backend ────────────────────────────────
        let source_site = d.join("source-site");
        std::fs::create_dir_all(source_site.join("templates")).unwrap();
        std::fs::create_dir_all(source_site.join("data")).unwrap();
        std::fs::create_dir_all(source_site.join("configs")).unwrap();
        std::fs::write(
            source_site.join("templates/index.html"),
            "first: {{ title }}",
        )
        .unwrap();
        std::fs::write(
            source_site.join("templates/other.html"),
            "other: {{ title }}",
        )
        .unwrap();
        std::fs::write(source_site.join("data/page.json"), "{\"title\": \"one\"}").unwrap();
        std::fs::write(source_site.join("data/site.json"), "{}").unwrap();
        std::fs::write(
            source_site.join("configs/m6-html.conf"),
            format!(
                "global_params = [\"data/page.json\"]\n\n\
                 [[route]]\npath = \"/\"\ntemplate = \"templates/index.html\"\ncache = \"{html_cache}\"\n\n\
                 [[route]]\npath = \"/other\"\ntemplate = \"templates/other.html\"\ncache = \"{html_cache}\"\n"
            ),
        )
        .unwrap();
        std::fs::write(
            source_site.join("site.toml"),
            format!(
                "[site]\nname = \"genid-source\"\ndomain = \"localhost\"\n\n\
                 [errors]\nmode = \"internal\"\n\n\
                 [analytics]\nenabled = false\n\n\
                 [log]\nlevel = \"info\"\n\n\
                 [rate_limit]\nenabled = false\n\n\
                 [server]\nbackend_timeout_secs = 5\n\n\
                 [[backend]]\nname = \"m6-html\"\nsockets = \"{}/html-*.sock\"\n\n\
                 [[backend]]\nname = \"auth-check\"\nsockets = \"{}/check.sock\"\n\n\
                 [[route]]\npath = \"/\"\nbackend = \"m6-html\"\n\n\
                 [[route]]\npath = \"/other\"\nbackend = \"m6-html\"\n",
                d.display(),
                d.display()
            ),
        )
        .unwrap();

        let source_system = d.join("source-system.toml");
        std::fs::write(
            &source_system,
            format!(
                "[server]\nbind = \"{}\"\ntls_cert = \"{}/cert.pem\"\ntls_key = \"{}/key.pem\"\n\n\
                 [node]\nname = \"genid-source\"\n\n\
                 [auth]\nbackend = \"auth-check\"\n\n\
                 [genid]\nread_require = \"role:reader\"\nwrite_require = \"role:writer\"\n",
                source_port.addr(),
                d.display(),
                d.display()
            ),
        )
        .unwrap();

        let state_dir = d.join("state");
        std::fs::create_dir_all(&state_dir).unwrap();

        // ── The cache node ───────────────────────────────────────────────
        let cache_site = d.join("cache-site");
        std::fs::create_dir_all(&cache_site).unwrap();
        std::fs::write(
            cache_site.join("site.toml"),
            format!(
                "[site]\nname = \"genid-cache\"\ndomain = \"localhost\"\n\n\
                 [errors]\nmode = \"internal\"\n\n\
                 [analytics]\nenabled = false\n\n\
                 [log]\nlevel = \"debug\"\n\n\
                 [rate_limit]\nenabled = false\n\n\
                 [server]\nbackend_timeout_secs = 5\n\n\
                 [stats]\ninterval_s = 1\n\n\
                 [[backend]]\nname = \"origin\"\nurl = \"https://{}\"\ntls_skip_verify = true\n\n\
                 [[backend]]\nname = \"auth-check\"\nsockets = \"{}/check.sock\"\n\n\
                 [[route]]\npath = \"/\"\nbackend = \"origin\"\n\n\
                 [[route]]\npath = \"/{{*path}}\"\nbackend = \"origin\"\n",
                source_port.addr(),
                d.display()
            ),
        )
        .unwrap();

        let cache_system = d.join("cache-system.toml");
        std::fs::write(
            &cache_system,
            format!(
                "[server]\nbind = \"{}\"\ntls_cert = \"{}/cert.pem\"\ntls_key = \"{}/key.pem\"\nwarm_on_start = {warm_on_start}\n\n\
                 [node]\nname = \"genid-cache\"\n\n\
                 [genid]\nupstream = \"origin\"\ntoken_file = \"{}/genid-token\"\nheartbeat_secs = {heartbeat_secs}\n",
                cache_port.addr(),
                d.display(),
                d.display(),
                d.display()
            ),
        )
        .unwrap();

        Stack {
            html: None,
            source: None,
            cache: None,
            dir,
            source_site,
            source_system,
            cache_system,
            state_dir,
            source_port,
            cache_port,
            _check: check,
        }
    }

    fn read_token(&self) -> String {
        "read-token".to_string()
    }

    fn write_token(&self) -> String {
        "write-token".to_string()
    }

    fn d(&self) -> &Path {
        self.dir.path()
    }

    fn start_html(&mut self) {
        let sock = self.d().join("html-1.sock");
        self.html = Some(Service::spawn(
            "m6-html-genid-cache",
            Command::new(binary("m6-html"))
                .args([
                    self.source_site.to_str().unwrap(),
                    self.source_site
                        .join("configs/m6-html.conf")
                        .to_str()
                        .unwrap(),
                    "--log-level",
                    "warn",
                ])
                .env("M6_SOCKET_OVERRIDE", &sock),
        ));
        self.html
            .as_mut()
            .unwrap()
            .wait_for_path(&sock, m6_core::testkit::STARTUP_BOUND);
    }

    fn start_source(&mut self) {
        let mut cmd = Command::new(binary("m6-http"));
        cmd.arg(&self.source_site)
            .arg(&self.source_system)
            .env(m6_core::platform::STATE_DIR_VAR, &self.state_dir)
            .env("RUST_LOG", "info")
            .stdin(Stdio::null())
            .current_dir(self.d());
        let mut svc = Service::spawn("m6-http-source", &mut cmd);
        svc.wait_for_tcp(self.source_port.port(), m6_core::testkit::STARTUP_BOUND);
        self.source = Some(svc);
        let ready = wait_until(m6_core::testkit::STARTUP_BOUND, || {
            self.get_source("/health", &[]).status != 0
        });
        assert!(
            ready,
            "the source never answered /health\n{}",
            self.source_output()
        );
    }

    fn stop_source(&mut self) {
        if let Some(mut svc) = self.source.take() {
            svc.kill();
        }
    }

    fn start_cache(&mut self) {
        let mut cmd = Command::new(binary("m6-http"));
        cmd.arg(self.d().join("cache-site"))
            .arg(&self.cache_system)
            .env("RUST_LOG", "debug")
            .stdin(Stdio::null())
            .current_dir(self.d());
        let mut svc = Service::spawn("m6-http-cache", &mut cmd);
        svc.wait_for_tcp(self.cache_port.port(), m6_core::testkit::STARTUP_BOUND);
        self.cache = Some(svc);
        let ready = wait_until(m6_core::testkit::STARTUP_BOUND, || {
            self.get_cache("/health", &[]).status != 0
        });
        assert!(
            ready,
            "the cache node never answered /health\n{}",
            self.cache_output()
        );
    }

    /// The message for a first request that should have been served: the
    /// status and body, then both services' logs.
    ///
    /// A 502 or a 504 at the first request says the node could not reach its
    /// source, and which side answered is only in these two logs.
    fn served_message(&self, r: &Resp) -> String {
        format!(
            "{}\n--- m6-http-cache ---\n{}\n--- m6-http-source ---\n{}",
            r.text(),
            self.cache_output(),
            self.source_output()
        )
    }

    fn source_output(&self) -> String {
        self.source
            .as_ref()
            .map(Service::output)
            .unwrap_or_default()
    }

    fn cache_output(&self) -> String {
        self.cache.as_ref().map(Service::output).unwrap_or_default()
    }

    // ── Clients ──────────────────────────────────────────────────────────────

    fn get(&self, port: u16, path: &str, headers: &[(&str, &str)]) -> Resp {
        if check_support::check_gated(path) {
            check_support::retry_unavailable(m6_core::testkit::RESPONSE_BOUND, || {
                let r = self.exchange_get(port, path, headers);
                (r.status, r.retry_after(), r)
            })
        } else {
            self.exchange_get(port, path, headers)
        }
    }

    fn exchange_get(&self, port: u16, path: &str, headers: &[(&str, &str)]) -> Resp {
        let tcp = match TcpStream::connect(("127.0.0.1", port)) {
            Ok(t) => t,
            Err(e) => return no_response(format!("connect: {e}")),
        };
        // No socket read or write timeout. The response is waited for as an
        // event under one generous bound: a timeout that fires while the
        // service is healthy turns CPU contention into a missing answer.
        let name = rustls::pki_types::ServerName::try_from("127.0.0.1".to_string()).unwrap();
        let conn = rustls::ClientConnection::new(no_verify_tls(), name).unwrap();
        let mut stream = StreamOwned::new(conn, tcp);
        let mut request = format!("GET {path} HTTP/1.1\r\nHost: localhost:{port}\r\n");
        for (k, v) in headers {
            request.push_str(&format!("{k}: {v}\r\n"));
        }
        request.push_str("Connection: close\r\n\r\n");
        if let Err(e) = stream
            .write_all(request.as_bytes())
            .and_then(|()| stream.flush())
        {
            return no_response(format!("write: {e}"));
        }
        let raw =
            match m6_core::testkit::wait_for_event(m6_core::testkit::RESPONSE_BOUND, move || {
                let mut stream = stream;
                let mut raw = Vec::new();
                match stream.read_to_end(&mut raw) {
                    Ok(_) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {}
                    Err(e) => return Err(format!("read: {e}")),
                }
                Ok(raw)
            }) {
                Ok(Ok(raw)) => raw,
                Ok(Err(e)) => return no_response(e),
                Err(e) => return no_response(format!("read: {e}")),
            };
        parse_h1(&raw)
    }

    fn post(&self, port: u16, path: &str, token: Option<&str>) -> Resp {
        if check_support::check_gated(path) {
            check_support::retry_unavailable(m6_core::testkit::RESPONSE_BOUND, || {
                let r = self.exchange_post(port, path, token);
                (r.status, r.retry_after(), r)
            })
        } else {
            self.exchange_post(port, path, token)
        }
    }

    fn exchange_post(&self, port: u16, path: &str, token: Option<&str>) -> Resp {
        let tcp = match TcpStream::connect(("127.0.0.1", port)) {
            Ok(t) => t,
            Err(e) => return no_response(format!("connect: {e}")),
        };
        // No socket read or write timeout. The response is waited for as an
        // event under one generous bound: a timeout that fires while the
        // service is healthy turns CPU contention into a missing answer.
        let name = rustls::pki_types::ServerName::try_from("127.0.0.1".to_string()).unwrap();
        let conn = rustls::ClientConnection::new(no_verify_tls(), name).unwrap();
        let mut stream = StreamOwned::new(conn, tcp);
        let mut request =
            format!("POST {path} HTTP/1.1\r\nHost: localhost:{port}\r\nContent-Length: 0\r\n");
        if let Some(token) = token {
            request.push_str(&format!("Authorization: Bearer {token}\r\n"));
        }
        request.push_str("Connection: close\r\n\r\n");
        if let Err(e) = stream
            .write_all(request.as_bytes())
            .and_then(|()| stream.flush())
        {
            return no_response(format!("write: {e}"));
        }
        let raw =
            match m6_core::testkit::wait_for_event(m6_core::testkit::RESPONSE_BOUND, move || {
                let mut stream = stream;
                let mut raw = Vec::new();
                match stream.read_to_end(&mut raw) {
                    Ok(_) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {}
                    Err(e) => return Err(format!("read: {e}")),
                }
                Ok(raw)
            }) {
                Ok(Ok(raw)) => raw,
                Ok(Err(e)) => return no_response(e),
                Err(e) => return no_response(format!("read: {e}")),
            };
        parse_h1(&raw)
    }

    fn get_source(&self, path: &str, headers: &[(&str, &str)]) -> Resp {
        self.get(self.source_port.port(), path, headers)
    }

    fn get_cache(&self, path: &str, headers: &[(&str, &str)]) -> Resp {
        self.get(self.cache_port.port(), path, headers)
    }

    fn post_source(&self, path: &str, token: Option<&str>) -> Resp {
        self.post(self.source_port.port(), path, token)
    }

    fn post_cache(&self, path: &str, token: Option<&str>) -> Resp {
        self.post(self.cache_port.port(), path, token)
    }

    /// The `genid` block the newest metrics line carries, or `None` while the
    /// cache has written none.
    ///
    /// The document is the one the removed `/perf` endpoint answered with, and
    /// the line is on the one stream the process writes.
    fn genid(&self) -> Option<serde_json::Value> {
        self.cache_output().lines().rev().find_map(|line| {
            let v: serde_json::Value = serde_json::from_str(line).ok()?;
            (v["type"] == "perf").then(|| v["fields"]["perf"]["genid"].clone())
        })
    }

    fn genid_id(&self) -> Option<String> {
        let id = self.genid()?["id"].clone();
        id.as_str().map(str::to_string)
    }

    fn genid_bool(&self, key: &str) -> Option<bool> {
        self.genid()?[key].as_bool()
    }

    /// How many metrics lines the cache node has written.
    fn perf_lines(&self) -> usize {
        self.cache_output().matches("\"type\":\"perf\"").count()
    }

    /// Wait until a metrics line written after this call started says the node
    /// holds the ID and its upstream is up.
    ///
    /// `/perf` was a live read: it answered with the node's state at the
    /// moment it was asked. The metrics line is a summary written on the
    /// accounting tick, so the newest line can be a second old, and a line
    /// that merely says the ID is set says nothing about whether the node can
    /// reach its source now. The old check asserted the state, so this waits
    /// for a freshly written line whose genid block says the ID is held and
    /// `upstream_down` is false, which is what the live read meant.
    fn wait_perf_ready(&self, want: &str, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut seen = self.perf_lines();
        while Instant::now() < deadline {
            let lines = self.perf_lines();
            if lines > seen {
                seen = lines;
                if self.genid_id().as_deref() == Some(want)
                    && self.genid_bool("upstream_down") == Some(false)
                {
                    return true;
                }
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    fn wait_upstream_down(&self, want: bool, timeout: Duration) -> bool {
        wait_until(timeout, || self.genid_bool("upstream_down") == Some(want))
    }
}

fn no_verify_tls() -> Arc<rustls::ClientConfig> {
    #[derive(Debug)]
    struct NoVerify;
    impl rustls::client::danger::ServerCertVerifier for NoVerify {
        fn verify_server_cert(
            &self,
            _: &rustls::pki_types::CertificateDer<'_>,
            _: &[rustls::pki_types::CertificateDer<'_>],
            _: &rustls::pki_types::ServerName<'_>,
            _: &[u8],
            _: rustls::pki_types::UnixTime,
        ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
            Ok(rustls::client::danger::ServerCertVerified::assertion())
        }
        fn verify_tls12_signature(
            &self,
            _: &[u8],
            _: &rustls::pki_types::CertificateDer<'_>,
            _: &rustls::DigitallySignedStruct,
        ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
            Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
        }
        fn verify_tls13_signature(
            &self,
            _: &[u8],
            _: &rustls::pki_types::CertificateDer<'_>,
            _: &rustls::DigitallySignedStruct,
        ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
            Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
        }
        fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
            rustls::crypto::aws_lc_rs::default_provider()
                .signature_verification_algorithms
                .supported_schemes()
        }
    }
    Arc::new(
        rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(NoVerify))
            .with_no_client_auth(),
    )
}

fn wait_until(timeout: Duration, mut done: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if done() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

/// The cache node learns the source's ID without being asked by a client, and
/// the metrics line reports it, the upstream, the round trip and that it is up.
#[test]
fn the_cache_learns_the_id_at_start() {
    let mut stack = Stack::new("public, max-age=60", 1, false);
    stack.start_html();
    stack.start_source();
    stack.start_cache();

    assert!(
        stack.wait_perf_ready("0", m6_core::testkit::RESPONSE_BOUND),
        "the metrics line never reported the source's ID on a fresh line with its upstream up:\n{}",
        stack.cache_output()
    );
    assert_eq!(stack.genid_bool("upstream_down"), Some(false));
    let block = stack.genid().expect("a metrics line with a genid block");
    assert_eq!(block["role"], "cache", "{block}");
    assert_eq!(block["upstream"], "origin", "{block}");
    assert!(block["upstream_rtt_ms"].is_number(), "{block}");
    assert!(block["last_ok_s"].is_u64(), "{block}");
}

/// A cache node that has no ID retries a failed first heartbeat on the next
/// tick, not after the configured interval.
///
/// A node with no ID cannot serve. With `heartbeat_secs = 3600` one timeout
/// while the source starts would otherwise leave it blind for an hour, and one
/// timeout under load is enough to miss the first answer. The test holds the
/// source down until the node has logged the first failure, then starts it, so
/// the retry is what has to find it.
///
/// Break: schedule the retry a whole heartbeat away, and this waits out
/// `STARTUP_BOUND` with an empty genid block in every metrics line.
#[test]
fn a_node_with_no_id_retries_a_failed_heartbeat() {
    let mut stack = Stack::new("public, max-age=60", 3600, false);
    stack.start_html();
    // Deliberately not started: the first heartbeat fails.
    stack.start_cache();
    assert!(
        wait_until(m6_core::testkit::STARTUP_BOUND, || {
            let out = stack.cache_output();
            out.contains("genid: heartbeat failed to start") || out.contains("genid: upstream down")
        }),
        "the first heartbeat never failed:\n{}",
        stack.cache_output()
    );
    stack.start_source();
    assert!(
        stack.wait_perf_ready("0", m6_core::testkit::STARTUP_BOUND),
        "the metrics line never reported the source's ID after one failed heartbeat, on a fresh \
         line with its upstream up:\n{}",
        stack.cache_output()
    );
}

/// A publish through the cache node reaches it: the POST is forwarded, the
/// source advances, the heartbeat follows, and the node serves the new content
/// under the new ID.
///
/// Break: skip `cache.clear()` in the change on a cache node, and the GET after
/// the ID moves still serves `first: one`.
#[test]
fn a_publish_through_the_cache_node_reaches_it() {
    let mut stack = Stack::new("public, max-age=60", 1, false);
    stack.start_html();
    stack.start_source();
    stack.start_cache();
    assert!(stack.wait_perf_ready("0", m6_core::testkit::RESPONSE_BOUND));

    let first = stack.get_cache("/", &[]);
    assert_eq!(first.status, 200, "{}", stack.served_message(&first));
    assert_eq!(first.text(), "first: one");
    let second = stack.get_cache("/", &[]);
    assert_eq!(second.text(), "first: one");
    assert!(
        second.header("age").is_some(),
        "the second GET is a cache hit"
    );

    std::fs::write(
        stack.source_site.join("data/page.json"),
        "{\"title\": \"two\"}",
    )
    .unwrap();
    let token = stack.write_token();
    let posted = stack.post_cache("/genid", Some(&token));
    assert_eq!(posted.status, 200, "{}", stack.served_message(&posted));
    assert_eq!(posted.genid(), "1", "the source's new ID comes back");

    assert!(
        stack.wait_perf_ready("1", m6_core::testkit::RESPONSE_BOUND),
        "the cache never followed the publish:\n{}",
        stack.cache_output()
    );
    let after = stack.get_cache("/", &[]);
    assert_eq!(after.status, 200, "{}", stack.served_message(&after));
    assert_eq!(after.text(), "first: two");
    assert_eq!(after.header("m6-genid"), Some("1"));
}

/// A reply under a newer ID wakes the heartbeat at once, even when the interval
/// is an hour: only `behind()` can make that deadline.
///
/// Break: drop `behind()` on a reply under another ID, and the ID never moves
/// inside the five-second deadline.
#[test]
fn a_response_under_a_newer_id_wakes_the_heartbeat() {
    let mut stack = Stack::new("public, max-age=60", 3600, false);
    stack.start_html();
    stack.start_source();
    stack.start_cache();
    assert!(
        stack.wait_perf_ready("0", m6_core::testkit::RESPONSE_BOUND),
        "the metrics line never reported the source's ID 0 on a fresh line with its upstream up:\n{}",
        stack.cache_output()
    );
    let first = stack.get_cache("/", &[]);
    assert_eq!(first.status, 200, "{}", stack.served_message(&first));
    assert_eq!(first.text(), "first: one");
    assert_eq!(stack.get_cache("/", &[]).text(), "first: one");

    std::fs::write(
        stack.source_site.join("data/page.json"),
        "{\"title\": \"two\"}",
    )
    .unwrap();
    let token = stack.write_token();
    let posted = stack.post_source("/genid", Some(&token));
    assert_eq!(posted.status, 200, "{}", stack.served_message(&posted));
    assert_eq!(posted.genid(), "1");

    let started = Instant::now();
    let other = stack.get_cache("/other", &[]);
    assert_eq!(other.status, 200, "{}", other.text());
    assert_eq!(
        other.header("m6-genid"),
        Some("1"),
        "the reply carries the source's new ID"
    );
    assert!(
        stack.wait_perf_ready("1", m6_core::testkit::RESPONSE_BOUND),
        "only behind() can make a 3600s heartbeat due this soon; elapsed {:?}:\n{}",
        started.elapsed(),
        stack.cache_output()
    );
    assert!(started.elapsed() < m6_core::testkit::RESPONSE_BOUND);

    let after = stack.get_cache("/", &[]);
    assert_eq!(
        after.text(),
        "first: two",
        "the change emptied the cache: {}",
        stack.served_message(&after)
    );
    let again = stack.get_cache("/", &[]);
    assert_eq!(again.text(), "first: two");
    assert!(again.header("age").is_some(), "and it refills on demand");
}

/// While the source is down the node serves what it holds, at any age, and the
/// operator sees it: the `upstream down` log line and the metrics line's
/// `upstream_down`.
///
/// Break: never set the held flag, and the expired entry becomes a miss that
/// answers 502 instead of the old body.
#[test]
fn the_cache_serves_what_it_holds_while_the_source_is_down() {
    let mut stack = Stack::new("public, max-age=1", 1, false);
    stack.start_html();
    stack.start_source();
    stack.start_cache();
    assert!(stack.wait_perf_ready("0", m6_core::testkit::RESPONSE_BOUND));
    let held = stack.get_cache("/", &[]);
    assert_eq!(held.status, 200, "{}", stack.served_message(&held));
    assert_eq!(held.text(), "first: one");

    stack.stop_source();
    assert!(
        stack.wait_upstream_down(true, m6_core::testkit::RESPONSE_BOUND),
        "the metrics line never reported the upstream down:\n{}",
        stack.cache_output()
    );
    assert!(
        stack.cache_output().contains("genid: upstream down"),
        "the operator sees one warn line on the transition:\n{}",
        stack.cache_output()
    );
    let backend_errors = stack.cache_output().matches("url backend error").count();
    assert_eq!(
        backend_errors,
        0,
        "the outage was learned from the heartbeat, not a failed client request:\n{}",
        stack.cache_output()
    );

    let deadline = Instant::now() + m6_core::testkit::RESPONSE_BOUND;
    loop {
        let r = stack.get_cache("/", &[]);
        assert_eq!(
            r.status,
            200,
            "held content must be served, not an error: {}",
            r.text()
        );
        assert_eq!(r.text(), "first: one", "the old body, at any age");
        let age = r
            .header("age")
            .and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap_or(0);
        if age >= 2 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "Age never reached 2, last was {age}:\n{}",
            stack.cache_output()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(
        stack.cache_output().matches("url backend error").count(),
        backend_errors,
        "held content is served without trying the origin on every request:\n{}",
        stack.cache_output()
    );

    let never = stack.get_cache("/never", &[]);
    assert!(
        matches!(never.status, 502 | 504),
        "a path the node never held is an error, got {}",
        never.status
    );
}

/// The source comes back on the same port and state directory: the node marks
/// the upstream up, the ID is unchanged, and no change is logged.
///
/// Break: treat the first success after down as a change, and `genid changed`
/// appears in the log.
#[test]
fn the_cache_recovers_when_the_source_returns() {
    let mut stack = Stack::new("public, max-age=60", 1, false);
    stack.start_html();
    stack.start_source();
    stack.start_cache();
    assert!(stack.wait_perf_ready("0", m6_core::testkit::RESPONSE_BOUND));

    stack.stop_source();
    assert!(stack.wait_upstream_down(true, m6_core::testkit::RESPONSE_BOUND));

    stack.start_source();
    assert!(
        stack.wait_upstream_down(false, m6_core::testkit::RESPONSE_BOUND),
        "the metrics line never reported the upstream up:\n{}",
        stack.cache_output()
    );
    assert_eq!(
        stack.genid_id().as_deref(),
        Some("0"),
        "a restart keeps the ID, so nothing is old"
    );
    // The recovery line is written on the turn the answer is applied, after
    // the outcome branch that would log a change. The Sink writes complete
    // lines in order, so once that line is on the wire every earlier line has
    // been flushed before it. The order is the assertion, with no bounded
    // absence window: waiting ten seconds here was a wall clock, not an event.
    assert!(
        wait_until(m6_core::testkit::STARTUP_BOUND, || stack
            .cache_output()
            .contains("genid: upstream up")),
        "the recovery is logged:\n{}",
        stack.cache_output()
    );
    assert!(
        !stack.cache_output().contains("genid changed"),
        "a success after down is not a change:\n{}",
        stack.cache_output()
    );

    std::fs::write(
        stack.source_site.join("data/page.json"),
        "{\"title\": \"two\"}",
    )
    .unwrap();
    let token = stack.write_token();
    assert_eq!(stack.post_source("/genid", Some(&token)).status, 200);
    assert!(stack.wait_perf_ready("1", m6_core::testkit::RESPONSE_BOUND));
    let after = stack.get_cache("/", &[]);
    assert_eq!(after.status, 200, "{}", stack.served_message(&after));
    assert_eq!(after.text(), "first: two");
    assert_eq!(after.header("m6-genid"), Some("1"));
}

/// `/genid` through the cache node is authorised by the source with the
/// caller's own token: the node checks no token of its own.
#[test]
fn genid_through_the_cache_is_authorised_by_the_source() {
    let mut stack = Stack::new("public, max-age=60", 1, false);
    stack.start_html();
    stack.start_source();
    stack.start_cache();
    assert!(stack.wait_perf_ready("0", m6_core::testkit::RESPONSE_BOUND));

    let none = stack.get_cache("/genid", &[]);
    assert_eq!(
        none.status,
        401,
        "the source refuses a caller with no token: {}",
        stack.served_message(&none)
    );
    let token = stack.read_token();
    let read = stack.get_cache("/genid", &[("Authorization", &format!("Bearer {token}"))]);
    assert_eq!(read.status, 200, "{}", read.text());
    assert_eq!(read.genid(), "0");
    assert_eq!(read.header("m6-genid"), Some("0"));
}

/// Warm fetches wait until the node has an ID. With the source not started, the
/// queue holds them; once the source answers, the drain fills the cache and a
/// warmed route is a hit.
///
/// Break: drain while `current` is `None`, and a warm fetch fails against the
/// missing source before the ID arrives.
#[test]
fn background_fetches_wait_for_the_id() {
    let mut stack = Stack::new("public, max-age=60", 1, true);
    stack.start_html();
    // Deliberately not started yet.
    stack.start_cache();

    stack.start_source();
    assert!(stack.wait_perf_ready("0", m6_core::testkit::RESPONSE_BOUND));
    // The queue holds one fetch per warmable route per encoding; `/` and the
    // three encodings are the three. Their only trace before a client arrives
    // is the dispatch, because a background fetch has no response to write.
    assert!(
        wait_until(m6_core::testkit::STARTUP_BOUND, || {
            stack.cache_output().matches("url backend dispatch").count() >= 3
        }),
        "the drain never ran once the ID was known:\n{}",
        stack.cache_output()
    );
    // The node logs the ID it learned before the drain that answer unblocks.
    // A warm fetch that ran with no ID to store it under would have dispatched
    // (or failed) before that line, so the order is the assertion. This
    // replaces a ten second absence window with the event that ends the
    // window.
    let out = stack.cache_output();
    let learned = out
        .find("genid: upstream answered; following its ID")
        .expect("the node logs the ID it learned");
    let first_fetch = out
        .find("url backend dispatch")
        .or_else(|| out.find("url backend error"))
        .expect("the drain dispatches the held fetches");
    assert!(
        first_fetch > learned,
        "a warm fetch ran with no ID to store it under:\n{out}"
    );

    // A warmed route is a hit: its entry is fresh, so the answer carries Age
    // and reaches no backend.
    let deadline = Instant::now() + m6_core::testkit::RESPONSE_BOUND;
    loop {
        let r = stack.get_cache("/", &[]);
        assert_eq!(r.status, 200, "{}", r.text());
        assert_eq!(r.text(), "first: one");
        if r.header("age").is_some() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the warmed route was never a hit:\n{}",
            stack.cache_output()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// `/health` on an edge answers `ok`, then 503 `degraded` while the origin is
/// down, then `ok` again when it returns. The degraded body names the node and
/// nothing a stranger can aim at, and the cache's log holds one warn on the
/// loss and one info on the recovery across many requests.
///
/// **What this test is about is the lines its own changes cause.** It does not
/// assert that an unprovoked heartbeat never misses: at `heartbeat_s = 1` under
/// load one can, the node sets its flag and logs the transition, and that is
/// the product being right about an event the test did not drive. So the counts
/// below are taken between the points where the test changes the origin, and
/// what is asserted is that the driven change logs once and that no request
/// adds a line.
///
/// Break: name the dependency in the public body, and the exact-body assertion
/// fails. Break: log on each failed request, and the count grows after the loss.
#[test]
fn health_goes_ok_degraded_ok_and_logs_each_change_once() {
    let mut stack = Stack::new("public, max-age=60", 1, false);
    stack.start_html();
    stack.start_source();
    stack.start_cache();
    assert!(stack.wait_perf_ready("0", m6_core::testkit::RESPONSE_BOUND));

    // `ok` while the origin answers, across many requests.
    for _ in 0..20 {
        let r = stack.get_cache("/", &[]);
        assert_eq!(r.status, 200, "{}", stack.served_message(&r));
        assert_eq!(r.text(), "first: one");
    }
    // Waiting on the state, not asserting that no heartbeat missed. At
    // `heartbeat_s = 1` under load one can, the node sets its flag honestly,
    // and the next answer clears it: that is the product being right about an
    // event this test did not drive, and the doc above says so. What this
    // asserts is that a node whose origin answers reports ok.
    assert!(
        wait_until(m6_core::testkit::RESPONSE_BOUND, || {
            stack.get_cache("/health", &[]).status == 200
        }),
        "/health never answered ok while the origin was up:\n{}",
        stack.cache_output()
    );
    let up = stack.get_cache("/health", &[]);
    assert_eq!(up.text(), r#"{"status":"ok","node":"genid-cache"}"#);

    // The origin goes away. The edge learns it from the heartbeat it already
    // sends, and /health answers while the origin cannot be reached, because
    // the answer is a flag read and never a probe.
    stack.stop_source();
    assert!(
        wait_until(m6_core::testkit::RESPONSE_BOUND, || {
            stack.get_cache("/health", &[]).status == 503
        }),
        "/health never answered degraded:\n{}",
        stack.cache_output()
    );
    let down = stack.get_cache("/health", &[]);
    assert_eq!(down.status, 503, "{}", down.text());
    assert_eq!(
        down.text(),
        r#"{"status":"degraded","node":"genid-cache"}"#,
        "the public body names no dependency and no reason"
    );
    for forbidden in ["origin", "auth", "reason", "dependency", "down"] {
        assert!(
            !down.text().contains(forbidden),
            "the degraded body says {forbidden}: {}",
            down.text()
        );
    }

    // Many requests while the origin is down: held content is served, a miss
    // fails, and neither adds a line.
    for _ in 0..20 {
        let r = stack.get_cache("/", &[]);
        assert_eq!(r.status, 200, "held content must be served: {}", r.text());
        assert_eq!(r.text(), "first: one", "the old body");
    }
    let never = stack.get_cache("/never", &[]);
    assert!(
        matches!(never.status, 502 | 504),
        "a path the node never held is an error, got {}",
        never.status
    );
    // The loss is logged by the time /health reports degraded. The count is
    // taken here, after the change the test drove, and then the requests above
    // and below must not move it.
    let down_after_loss = stack.cache_output().matches("genid: upstream down").count();
    assert!(
        down_after_loss >= 1,
        "the loss was not logged:\n{}",
        stack.cache_output()
    );
    for _ in 0..20 {
        let r = stack.get_cache("/", &[]);
        assert_eq!(r.status, 200, "{}", r.text());
    }
    assert_eq!(
        stack.cache_output().matches("genid: upstream down").count(),
        down_after_loss,
        "a request added a warn after the loss:\n{}",
        stack.cache_output()
    );

    // The origin returns on the same port and the next heartbeat clears the
    // flag. `info` lines flush on the one-second log tick, so wait on the line
    // rather than reading once.
    // Count from just before the change, so the recovery's single info line is
    // the only new one the test can account for.
    let up_before_recovery = stack.cache_output().matches("genid: upstream up").count();
    let down_before_recovery = stack.cache_output().matches("genid: upstream down").count();
    stack.start_source();
    assert!(
        wait_until(m6_core::testkit::RESPONSE_BOUND, || {
            stack.get_cache("/health", &[]).status == 200
        }),
        "/health never recovered:\n{}",
        stack.cache_output()
    );
    assert!(
        wait_until(m6_core::testkit::RESPONSE_BOUND, || {
            stack.cache_output().matches("genid: upstream up").count() == up_before_recovery + 1
        }),
        "the recovery was not logged exactly once:\n{}",
        stack.cache_output()
    );
    for _ in 0..20 {
        let r = stack.get_cache("/", &[]);
        assert_eq!(r.status, 200, "{}", r.text());
    }
    assert_eq!(
        stack.cache_output().matches("genid: upstream up").count(),
        up_before_recovery + 1,
        "a request added an info after the recovery:\n{}",
        stack.cache_output()
    );
    assert_eq!(
        stack.cache_output().matches("genid: upstream down").count(),
        down_before_recovery,
        "a warn appeared after the recovery:\n{}",
        stack.cache_output()
    );
    // The same wait as the first `ok`, for the same reason.
    assert!(
        wait_until(m6_core::testkit::RESPONSE_BOUND, || {
            stack.get_cache("/health", &[]).status == 200
        }),
        "/health never reported ok again after the requests:\n{}",
        stack.cache_output()
    );
    let back = stack.get_cache("/health", &[]);
    assert_eq!(back.text(), r#"{"status":"ok","node":"genid-cache"}"#);
}
