//! A cache node follows its source's generation ID, and serves what it holds
//! while the source is down.
//!
//! Every test starts a real `m6-http` source in front of a real `m6-html`, and a
//! second `m6-http` cache node whose `[genid] upstream` names the source and
//! whose routes point at it. The tokens are ES256 JWTs minted here, verified by
//! the source with the `[auth] public_key` the fixture writes. Nothing waits on
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

// ── Crypto fixtures ───────────────────────────────────────────────────────────

fn keypair() -> (String, String) {
    let kp = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).expect("ec keypair");
    (kp.serialize_pem(), kp.public_key_pem())
}

fn mint(private_pem: &str, roles: &[&str]) -> String {
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
        sub: "genid-cache-test".to_string(),
        exp: now + 3600,
        roles: roles.iter().map(|r| r.to_string()).collect(),
    };
    let key = jsonwebtoken::EncodingKey::from_ec_pem(private_pem.as_bytes()).expect("ec key");
    let header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::ES256);
    jsonwebtoken::encode(&header, &claims, &key).expect("sign jwt")
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
    private_pem: String,
}

impl Stack {
    /// `html_cache` is the `Cache-Control` the source's backend stamps on `/`
    /// and `/other`. `heartbeat_secs` is the cache node's interval.
    fn new(html_cache: &str, heartbeat_secs: u64, warm_on_start: bool) -> Stack {
        rustls::crypto::ring::default_provider()
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
        let (private_pem, public_pem) = keypair();
        std::fs::write(d.join("auth-public.pem"), public_pem).unwrap();
        std::fs::write(
            d.join("genid-token"),
            format!("{}\n", mint(&private_pem, &["reader"])),
        )
        .unwrap();

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
                 [[route]]\npath = \"/\"\nbackend = \"m6-html\"\n\n\
                 [[route]]\npath = \"/other\"\nbackend = \"m6-html\"\n",
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
                 [auth]\npublic_key = \"{}/auth-public.pem\"\n\n\
                 [genid]\nread_require = \"role:reader\"\nwrite_require = \"role:writer\"\n",
                source_port.addr(),
                d.display(),
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
                 [[backend]]\nname = \"origin\"\nurl = \"https://{}\"\ntls_skip_verify = true\n\n\
                 [[route]]\npath = \"/\"\nbackend = \"origin\"\n\n\
                 [[route]]\npath = \"/{{*path}}\"\nbackend = \"origin\"\n",
                source_port.addr()
            ),
        )
        .unwrap();

        let cache_system = d.join("cache-system.toml");
        std::fs::write(
            &cache_system,
            format!(
                "[server]\nbind = \"{}\"\ntls_cert = \"{}/cert.pem\"\ntls_key = \"{}/key.pem\"\nwarm_on_start = {warm_on_start}\n\n\
                 [node]\nname = \"genid-cache\"\n\n\
                 [auth]\npublic_key = \"{}/auth-public.pem\"\n\n\
                 [health]\nmetrics_require = \"role:monitor\"\n\n\
                 [genid]\nupstream = \"origin\"\ntoken_file = \"{}/genid-token\"\nheartbeat_secs = {heartbeat_secs}\n",
                cache_port.addr(),
                d.display(),
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
            private_pem,
        }
    }

    fn token(&self, roles: &[&str]) -> String {
        mint(&self.private_pem, roles)
    }

    fn read_token(&self) -> String {
        self.token(&["reader"])
    }

    fn write_token(&self) -> String {
        self.token(&["writer"])
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
            .wait_for_path(&sock, Duration::from_secs(15));
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
        svc.wait_for_tcp(self.source_port.port(), Duration::from_secs(10));
        self.source = Some(svc);
        let ready = wait_until(Duration::from_secs(10), || {
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
        svc.wait_for_tcp(self.cache_port.port(), Duration::from_secs(10));
        self.cache = Some(svc);
        let ready = wait_until(Duration::from_secs(10), || {
            self.get_cache("/health", &[]).status != 0
        });
        assert!(
            ready,
            "the cache node never answered /health\n{}",
            self.cache_output()
        );
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
        let tcp = match TcpStream::connect(("127.0.0.1", port)) {
            Ok(t) => t,
            Err(e) => return no_response(format!("connect: {e}")),
        };
        tcp.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
        tcp.set_write_timeout(Some(Duration::from_secs(20)))
            .unwrap();
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
        let mut raw = Vec::new();
        match stream.read_to_end(&mut raw) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {}
            Err(e) => return no_response(format!("read: {e}")),
        }
        parse_h1(&raw)
    }

    fn post(&self, port: u16, path: &str, token: Option<&str>) -> Resp {
        let tcp = match TcpStream::connect(("127.0.0.1", port)) {
            Ok(t) => t,
            Err(e) => return no_response(format!("connect: {e}")),
        };
        tcp.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
        tcp.set_write_timeout(Some(Duration::from_secs(20)))
            .unwrap();
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
        let mut raw = Vec::new();
        match stream.read_to_end(&mut raw) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {}
            Err(e) => return no_response(format!("read: {e}")),
        }
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

    fn perf(&self) -> Resp {
        let auth = format!("Bearer {}", self.token(&["monitor"]));
        self.get_cache("/perf", &[("Authorization", auth.as_str())])
    }

    /// The cache node's `genid` block from `/perf`, or an empty string.
    fn genid_block(&self) -> String {
        let perf = self.perf();
        let body = perf.text();
        let start = body.find("\"genid\":{").map(|i| i + 8).unwrap_or(0);
        let rest = &body[start..];
        let end = rest.find('}').map(|i| i + 1).unwrap_or(rest.len());
        rest[..end].to_string()
    }

    fn perf_id(&self) -> Option<String> {
        let block = self.genid_block();
        let pat = "\"id\":\"";
        block
            .split(pat)
            .nth(1)
            .and_then(|rest| rest.split('"').next())
            .map(str::to_string)
    }

    fn perf_bool(&self, key: &str) -> Option<bool> {
        let block = self.genid_block();
        let pat = format!("\"{key}\":");
        let rest = block.split(&pat).nth(1)?;
        if rest.starts_with("true") {
            Some(true)
        } else if rest.starts_with("false") {
            Some(false)
        } else {
            None
        }
    }

    fn wait_perf_id(&self, want: &str, timeout: Duration) -> bool {
        wait_until(timeout, || self.perf_id().as_deref() == Some(want))
    }

    fn wait_upstream_down(&self, want: bool, timeout: Duration) -> bool {
        wait_until(timeout, || self.perf_bool("upstream_down") == Some(want))
    }

    /// Seconds since the origin last answered, from the `/perf` genid block.
    ///
    /// The readiness tests wait on this rather than on a sleep: it is state
    /// the node already keeps, so polling it asks the origin nothing.
    fn last_ok_s(&self) -> Option<u64> {
        let block = self.genid_block();
        let rest = block.split("\"last_ok_s\":").nth(1)?;
        let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        digits.parse().ok()
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
            rustls::crypto::ring::default_provider()
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
/// `/perf` reports it, the upstream, the round trip and that it is up.
#[test]
fn the_cache_learns_the_id_at_start() {
    let mut stack = Stack::new("public, max-age=60", 1, false);
    stack.start_html();
    stack.start_source();
    stack.start_cache();

    assert!(
        stack.wait_perf_id("0", Duration::from_secs(10)),
        "/perf never showed the source's ID:\n{}",
        stack.cache_output()
    );
    assert_eq!(stack.perf_bool("upstream_down"), Some(false));
    let block = stack.genid_block();
    assert!(block.contains("\"role\":\"cache\""), "{block}");
    assert!(block.contains("\"upstream\":\"origin\""), "{block}");
    assert!(block.contains("\"upstream_rtt_ms\":"), "{block}");
    let rest = block.split("\"last_ok_s\":").nth(1).unwrap_or("");
    assert!(rest.starts_with(|c: char| c.is_ascii_digit()), "{block}");
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
    assert!(stack.wait_perf_id("0", Duration::from_secs(10)));

    let first = stack.get_cache("/", &[]);
    assert_eq!(first.status, 200, "{}", first.text());
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
    assert_eq!(posted.status, 200, "{}", posted.text());
    assert_eq!(posted.genid(), "1", "the source's new ID comes back");

    assert!(
        stack.wait_perf_id("1", Duration::from_secs(10)),
        "the cache never followed the publish:\n{}",
        stack.cache_output()
    );
    let after = stack.get_cache("/", &[]);
    assert_eq!(after.status, 200);
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
    assert!(stack.wait_perf_id("0", Duration::from_secs(10)));
    assert_eq!(stack.get_cache("/", &[]).text(), "first: one");
    assert_eq!(stack.get_cache("/", &[]).text(), "first: one");

    std::fs::write(
        stack.source_site.join("data/page.json"),
        "{\"title\": \"two\"}",
    )
    .unwrap();
    let token = stack.write_token();
    let posted = stack.post_source("/genid", Some(&token));
    assert_eq!(posted.status, 200, "{}", posted.text());
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
        stack.wait_perf_id("1", Duration::from_secs(5)),
        "only behind() can make a 3600s heartbeat due inside 5s; elapsed {:?}:\n{}",
        started.elapsed(),
        stack.cache_output()
    );
    assert!(started.elapsed() < Duration::from_secs(5));

    let after = stack.get_cache("/", &[]);
    assert_eq!(after.text(), "first: two", "the change emptied the cache");
    let again = stack.get_cache("/", &[]);
    assert_eq!(again.text(), "first: two");
    assert!(again.header("age").is_some(), "and it refills on demand");
}

/// While the source is down the node serves what it holds, at any age, and the
/// operator sees it: the `upstream down` log line and `/perf`'s
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
    assert!(stack.wait_perf_id("0", Duration::from_secs(10)));
    let held = stack.get_cache("/", &[]);
    assert_eq!(held.status, 200, "{}", held.text());
    assert_eq!(held.text(), "first: one");

    stack.stop_source();
    assert!(
        stack.wait_upstream_down(true, Duration::from_secs(10)),
        "/perf never reported the upstream down:\n{}",
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

    let deadline = Instant::now() + Duration::from_secs(10);
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
    assert!(stack.wait_perf_id("0", Duration::from_secs(10)));

    stack.stop_source();
    assert!(stack.wait_upstream_down(true, Duration::from_secs(10)));

    stack.start_source();
    assert!(
        stack.wait_upstream_down(false, Duration::from_secs(10)),
        "/perf never reported the upstream up:\n{}",
        stack.cache_output()
    );
    assert_eq!(
        stack.perf_id().as_deref(),
        Some("0"),
        "a restart keeps the ID, so nothing is old"
    );
    // `info` lines are buffered and flushed once a second, so this polls the
    // absence for a bounded window rather than reading once.
    assert!(
        !wait_until(Duration::from_secs(2), || stack
            .cache_output()
            .contains("genid changed")),
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
    assert!(stack.wait_perf_id("1", Duration::from_secs(10)));
    let after = stack.get_cache("/", &[]);
    assert_eq!(after.status, 200);
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
    assert!(stack.wait_perf_id("0", Duration::from_secs(10)));

    let none = stack.get_cache("/genid", &[]);
    assert_eq!(
        none.status, 401,
        "the source refuses a caller with no token"
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
    // A bounded absence check: `debug` is buffered, so the dispatch line can
    // surface a second after it happens.
    assert!(
        !wait_until(Duration::from_secs(2), || {
            let out = stack.cache_output();
            out.contains("url backend dispatch") || out.contains("url backend error")
        }),
        "a warm fetch ran with no ID to store it under:\n{}",
        stack.cache_output()
    );

    stack.start_source();
    assert!(stack.wait_perf_id("0", Duration::from_secs(10)));
    // The queue holds one fetch per warmable route per encoding; `/` and the
    // three encodings are the three. Their only trace before a client arrives
    // is the dispatch, because a background fetch has no response to write.
    assert!(
        wait_until(Duration::from_secs(10), || {
            stack.cache_output().matches("url backend dispatch").count() >= 3
        }),
        "the drain never ran once the ID was known:\n{}",
        stack.cache_output()
    );

    // A warmed route is a hit: its entry is fresh, so the answer carries Age
    // and reaches no backend.
    let deadline = Instant::now() + Duration::from_secs(10);
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

// ── Readiness: `/ready` beside an unchanged `/health` ────────────────────────
//
// Every test below asks both endpoints. The split is only proven when the same
// state gives /ready one answer and /health another: a change that moved the
// answer onto /health would fail the exact-body assertions here.

/// A cache whose origin answered inside the window is ready, and `/health`
/// still answers for the process alone.
///
/// Break: answer 503 for every cache node, dropping the last-answer test, and
/// a node whose origin answered a moment ago is pulled from rotation.
#[test]
fn a_cache_with_a_live_origin_is_ready() {
    let mut stack = Stack::new("public, max-age=60", 10, false);
    stack.start_html();
    stack.start_source();
    stack.start_cache();
    assert!(
        stack.wait_perf_id("0", Duration::from_secs(10)),
        "/perf never showed the source's ID:\n{}",
        stack.cache_output()
    );

    let ready = stack.get_cache("/ready", &[]);
    assert_eq!(ready.status, 200, "{}", ready.text());
    assert!(
        ready.text().contains("\"origin\":\"up\""),
        "a live origin is not named up: {}",
        ready.text()
    );
    assert_eq!(
        stack.get_cache("/health", &[]).text(),
        "{\"status\":\"ok\",\"node\":\"genid-cache\"}",
        "/health must stay the process answer"
    );
}

/// ISS-175: an edge cache reports `/health` ok while it cannot serve a request
/// that depends on its origin.
///
/// Liveness asks whether the process works, readiness whether it can serve a
/// request that needs the origin. `/health` answers the first and keeps
/// answering 200 for the process alone. Readiness is the second answer, `GET
/// /ready`, which answers 503 when the origin has been gone past two
/// heartbeats and names the follower's last failure in the body.
///
/// Break: answer 200 by dropping the window test, the defect this replaces,
/// and the dead origin is reported ready again.
#[test]
fn an_edge_with_a_dead_origin_past_the_window_reports_ready() {
    let heartbeat_s = 1;
    let mut stack = Stack::new("public, max-age=60", heartbeat_s, false);
    stack.start_html();
    stack.start_source();
    stack.start_cache();
    assert!(
        stack.wait_perf_id("0", Duration::from_secs(10)),
        "/perf never showed the source's ID:\n{}",
        stack.cache_output()
    );

    stack.stop_source();
    assert!(
        stack.wait_upstream_down(true, Duration::from_secs(10)),
        "/perf never showed the upstream down:\n{}",
        stack.cache_output()
    );
    // Past the window: two heartbeats with no answer. `last_ok_s` is state the
    // node already keeps, so this poll asks the origin nothing.
    assert!(
        wait_until(Duration::from_secs(10), || {
            stack.last_ok_s().is_some_and(|s| s > 2 * heartbeat_s)
        }),
        "the origin state never left the window:\n{}",
        stack.genid_block()
    );

    let ready = stack.get_cache("/ready", &[]);
    assert_eq!(
        ready.status,
        503,
        "an edge whose origin had been dead past the window reported ready: {} {} (last_ok_s {:?})",
        ready.status,
        ready.text(),
        stack.last_ok_s()
    );
    assert!(
        ready.text().contains("\"reason\":\"connect failed\""),
        "the body does not name why the origin is gone: {}",
        ready.text()
    );
    // The check that proves a split rather than a move.
    let health = stack.get_cache("/health", &[]);
    assert_eq!(health.status, 200, "{}", health.text());
    assert_eq!(
        health.text(),
        "{\"status\":\"ok\",\"node\":\"genid-cache\"}",
        "/health must keep answering for the process"
    );
}

/// A node with no origin is known to be a source from config, so it is ready
/// and names no origin rather than reporting a missing heartbeat.
///
/// Break: decide readiness from `last_ok` instead of the role, and a source
/// that never heartbeats answers 503.
#[test]
fn a_source_node_is_ready_and_names_no_origin() {
    let mut stack = Stack::new("public, max-age=60", 10, false);
    stack.start_html();
    stack.start_source();

    let ready = stack.get_source("/ready", &[]);
    assert_eq!(ready.status, 200, "{}", ready.text());
    assert_eq!(
        ready.text(),
        "{\"status\":\"ready\",\"node\":\"genid-source\",\"origin\":\"not_applicable\"}"
    );
    assert_eq!(
        stack.get_source("/health", &[]).text(),
        "{\"status\":\"ok\",\"node\":\"genid-source\"}",
        "/health must stay the process answer"
    );
}

/// `/health` is unchanged in all three states: a source (no origin), a cache
/// with a live origin, and a cache whose origin is past the window. Only
/// `/ready` moves.
///
/// Break: make `/health` answer 503 when the origin is down, or add an origin
/// field to it, and an exact body here fails. That is the move this endpoint
/// exists to avoid.
#[test]
fn health_answers_process_only_in_all_three_states() {
    let heartbeat_s = 1;
    let mut stack = Stack::new("public, max-age=60", heartbeat_s, false);
    stack.start_html();
    stack.start_source();

    // 1. A source: no origin to wait for.
    assert_eq!(
        stack.get_source("/health", &[]).text(),
        "{\"status\":\"ok\",\"node\":\"genid-source\"}"
    );

    stack.start_cache();
    assert!(
        stack.wait_perf_id("0", Duration::from_secs(10)),
        "/perf never showed the source's ID:\n{}",
        stack.cache_output()
    );
    // 2. A cache with a live origin.
    let live = stack.get_cache("/health", &[]);
    assert_eq!(live.status, 200, "{}", live.text());
    assert_eq!(
        live.text(),
        "{\"status\":\"ok\",\"node\":\"genid-cache\"}",
        "a live origin changed the process answer"
    );

    stack.stop_source();
    assert!(
        stack.wait_upstream_down(true, Duration::from_secs(10)),
        "/perf never showed the upstream down:\n{}",
        stack.cache_output()
    );
    assert!(
        wait_until(Duration::from_secs(10), || {
            stack.last_ok_s().is_some_and(|s| s > 2 * heartbeat_s)
        }),
        "the origin state never left the window:\n{}",
        stack.genid_block()
    );
    // 3. A cache whose origin is past the window. /ready moved, /health did
    // not, and the body carries no origin state for a monitor to misread.
    assert_eq!(
        stack.get_cache("/ready", &[]).status,
        503,
        "the readiness answer did not move"
    );
    let down = stack.get_cache("/health", &[]);
    assert_eq!(down.status, 200, "{}", down.text());
    assert_eq!(
        down.text(),
        "{\"status\":\"ok\",\"node\":\"genid-cache\"}",
        "/health moved instead of /ready"
    );
    assert!(
        !down.text().contains("origin") && !down.text().contains("ready"),
        "the process answer gained origin state: {}",
        down.text()
    );
}
