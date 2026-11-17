//! A cache node writes its cache to disk off the request path.
//!
//! Every test starts a real `m6-http` source in front of a real `m6-html`, and a
//! second `m6-http` cache node with `[cache] dir` in its system config. The
//! request path only queues a key. The accounting tick writes the file under
//! `<dir>/<genid>/`, and a generation change moves writes to a new directory and
//! removes the old one over the ticks that follow. Nothing waits on a fixed
//! sleep for a result: every wait polls a condition with a deadline.
//!
//! The files are read back with `disk_cache::decode`, the function the
//! start-time load calls. That is the restart half this branch can prove: the
//! bytes are on disk, complete without the writer, and decode to the response
//! the node served. Inserting them into a fresh memory cache at start is the
//! other half.
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
use m6_http_lib::cache::{Cache, CacheKey, CachedResponse, HeaderBlock};
use m6_http_lib::disk_cache;

/// The body of the load test's page, in bytes. At about 400 us per file in this
/// debug build, one 1 ms tick writes two or three of them.
const LOAD_BODY_LEN: usize = 10_000;

/// Entries the load test queues while it holds the tick: far more than one
/// tick's 1 ms loop-time budget, so the queue drains over the ticks that
/// follow the hold.
const LOAD_KEYS: usize = 500;

/// Old-generation files the load test seeds, so the drain also carries the
/// removal work a generation change leaves behind.
const LOAD_OLD_FILES: usize = 1_000;

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

/// A source `m6-http`, a cache `m6-http` with `[cache] dir`, and the `m6-html`
/// behind the source. Services are fields so they die before the directory they
/// serve from is removed.
struct Stack {
    html: Option<Service>,
    source: Option<Service>,
    cache: Option<Service>,
    dir: tempfile::TempDir,
    source_site: PathBuf,
    source_system: PathBuf,
    cache_system: PathBuf,
    cache_dir: PathBuf,
    state_dir: PathBuf,
    source_port: PortClaim,
    cache_port: PortClaim,
    _check: RunningCheck,
}

impl Stack {
    fn new() -> Stack {
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
        std::fs::write(
            source_site.join("templates/load.html"),
            "x".repeat(LOAD_BODY_LEN),
        )
        .unwrap();
        std::fs::write(source_site.join("data/page.json"), "{\"title\": \"one\"}").unwrap();
        std::fs::write(source_site.join("data/site.json"), "{}").unwrap();
        std::fs::write(
            source_site.join("configs/m6-html.conf"),
            "global_params = [\"data/page.json\"]\n\n\
             [[route]]\npath = \"/\"\ntemplate = \"templates/index.html\"\ncache = \"public, max-age=60\"\n\n\
             [[route]]\npath = \"/other\"\ntemplate = \"templates/other.html\"\ncache = \"public, max-age=60\"\n\n\
             [[route]]\npath = \"/load\"\ntemplate = \"templates/load.html\"\ncache = \"public, max-age=60\"\n",
        )
        .unwrap();
        std::fs::write(
            source_site.join("site.toml"),
            format!(
                "[site]\nname = \"disk-source\"\ndomain = \"localhost\"\n\n\
                 [errors]\nmode = \"internal\"\n\n\
                 [analytics]\nenabled = false\n\n\
                 [log]\nlevel = \"info\"\n\n\
                 [rate_limit]\nenabled = false\n\n\
                 [server]\nbackend_timeout_secs = 5\n\n\
                 [[backend]]\nname = \"m6-html\"\nsockets = \"{}/html-*.sock\"\n\n\
                 [[backend]]\nname = \"auth-check\"\nsockets = \"{}/check.sock\"\n\n\
                 [[route]]\npath = \"/\"\nbackend = \"m6-html\"\n\n\
                 [[route]]\npath = \"/other\"\nbackend = \"m6-html\"\n\n\
                 [[route]]\npath = \"/load\"\nbackend = \"m6-html\"\n",
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
                 [node]\nname = \"disk-source\"\n\n\
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

        // ── The cache node, with its disk directory ──────────────────────
        let cache_dir = d.join("disk-cache");
        std::fs::create_dir_all(&cache_dir).unwrap();
        let cache_site = d.join("cache-site");
        std::fs::create_dir_all(&cache_site).unwrap();
        std::fs::write(
            cache_site.join("site.toml"),
            format!(
                "[site]\nname = \"disk-cache\"\ndomain = \"localhost\"\n\n\
                 [errors]\nmode = \"internal\"\n\n\
                 [analytics]\nenabled = false\n\n\
                 [log]\nlevel = \"debug\"\n\n\
                 [rate_limit]\nenabled = false\n\n\
                 [server]\nbackend_timeout_secs = 5\n\n\
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
                "[server]\nbind = \"{}\"\ntls_cert = \"{}/cert.pem\"\ntls_key = \"{}/key.pem\"\n\n\
                 [node]\nname = \"disk-cache\"\n\n\
                 [auth]\nbackend = \"auth-check\"\n\n\
                 [health]\nmetrics_require = \"role:monitor\"\n\n\
                 [genid]\nupstream = \"origin\"\ntoken_file = \"{}/genid-token\"\nheartbeat_secs = 1\n\n\
                 [cache]\ndir = \"{}\"\n",
                cache_port.addr(),
                d.display(),
                d.display(),
                d.display(),
                cache_dir.display()
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
            cache_dir,
            state_dir,
            source_port,
            cache_port,
            _check: check,
        }
    }

    fn write_token(&self) -> String {
        "write-token".to_string()
    }

    fn d(&self) -> &Path {
        self.dir.path()
    }

    fn start(&mut self) {
        self.start_html();
        self.start_source();
        self.start_cache();
    }

    fn start_html(&mut self) {
        let sock = self.d().join("html-1.sock");
        self.html = Some(Service::spawn(
            "m6-html-disk-cache",
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

    fn start_cache(&mut self) {
        let mut cmd = Command::new(binary("m6-http"));
        cmd.arg(self.d().join("cache-site"))
            .arg(&self.cache_system)
            .env("RUST_LOG", "debug")
            // Test instrumentation: a debug build may shorten the stats
            // interval, so `/perf` has a loop trace to report while a test
            // runs. A release build ignores the variable.
            .env("M6_TEST_STATS_INTERVAL_SECS", "1")
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

    fn stop_cache(&mut self) {
        if let Some(mut svc) = self.cache.take() {
            svc.kill();
        }
    }

    fn stop_source(&mut self) {
        if let Some(mut svc) = self.source.take() {
            svc.kill();
        }
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

    fn perf(&self) -> Resp {
        let auth = format!("Bearer {}", "monitor-token");
        self.get_cache("/perf", &[("Authorization", auth.as_str())])
    }

    /// The cache node's `genid` object from `/perf`.
    fn genid_block(&self) -> String {
        let body = self.perf().text();
        let start = body.find("\"genid\":{").map(|i| i + 8).unwrap_or(0);
        let rest = &body[start..];
        let end = rest.find('}').map(|i| i + 1).unwrap_or(rest.len());
        rest[..end].to_string()
    }

    /// The cache node's `disk_cache` object from `/perf`.
    fn disk_block(&self) -> String {
        let body = self.perf().text();
        let Some(start) = body.find("\"disk_cache\":{").map(|i| i + 13) else {
            return String::new();
        };
        let rest = &body[start..];
        let end = rest.find('}').map(|i| i + 1).unwrap_or(rest.len());
        rest[..end].to_string()
    }

    fn disk_u64(&self, key: &str) -> Option<u64> {
        let block = self.disk_block();
        let pat = format!("\"{key}\":");
        block.split(&pat).nth(1).and_then(|rest| {
            rest.split(|c: char| !c.is_ascii_digit())
                .next()
                .and_then(|n| n.parse().ok())
        })
    }

    /// The `disk_cache.state` string from `/perf`.
    fn disk_state(&self) -> Option<String> {
        let block = self.disk_block();
        block
            .split("\"state\":\"")
            .nth(1)
            .and_then(|rest| rest.split('"').next())
            .map(str::to_string)
    }

    /// A cumulative counter anywhere in the `/perf` report, such as
    /// `cache_hits_total`. The cache node counts its own answers, so a hit on
    /// the restarted node is visible there without reading the source's log.
    fn perf_u64(&self, key: &str) -> Option<u64> {
        let body = self.perf().text();
        let pat = format!("\"{key}\":");
        body.split(&pat).nth(1).and_then(|rest| {
            rest.split(|c: char| !c.is_ascii_digit())
                .next()
                .and_then(|n| n.parse().ok())
        })
    }

    /// The longest loop iteration `/perf` currently reports, in nanoseconds.
    fn longest_loop_work_ns(&self) -> u64 {
        loop_work_max_ns(&self.perf().text())
    }

    fn perf_id(&self) -> Option<String> {
        let block = self.genid_block();
        block
            .split("\"id\":\"")
            .nth(1)
            .and_then(|rest| rest.split('"').next())
            .map(str::to_string)
    }

    fn wait_perf_id(&self, want: &str, timeout: Duration) -> bool {
        wait_until(timeout, || self.perf_id().as_deref() == Some(want))
    }

    /// Directory names under the cache root: one per generation.
    fn generation_dirs(&self) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(&self.cache_dir)
            .map(|rd| {
                rd.flatten()
                    .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        names
    }

    /// The files under one generation directory, not counting a leftover
    /// `.tmp`.
    fn files_for(&self, id: &str) -> Vec<PathBuf> {
        let dir = self.cache_dir.join(id);
        let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
            .map(|rd| {
                rd.flatten()
                    .map(|e| e.path())
                    .filter(|p| {
                        p.is_file()
                            && !p
                                .file_name()
                                .is_some_and(|n| n.to_string_lossy().ends_with(".tmp"))
                    })
                    .collect()
            })
            .unwrap_or_default();
        files.sort();
        files
    }
}

/// The longest loop iteration a `/perf` body reports, in nanoseconds.
fn loop_work_max_ns(body: &str) -> u64 {
    body.match_indices("\"work_max_ns\":")
        .filter_map(|(i, pat)| {
            body[i + pat.len()..]
                .split(|c: char| !c.is_ascii_digit())
                .next()
                .and_then(|n| n.parse().ok())
        })
        .max()
        .unwrap_or(0)
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

/// A served response reaches the disk on an idle slice, under the node's
/// generation directory, and decodes to what the client was served.
///
/// The write must appear within one idle turn, under the accounting tick's
/// second, and `/perf` must report the loop's `work_max_ns` for the window
/// that covers the slice.
///
/// Break: skip the idle-turn slice, and `/perf` `dir_bytes` stays 0 and no
/// file appears.
#[test]
fn a_cache_node_writes_what_it_serves_to_disk() {
    let mut stack = Stack::new();
    stack.start();
    assert!(
        stack.wait_perf_id("0", m6_core::testkit::RESPONSE_BOUND),
        "/perf never showed the source's ID:\n{}",
        stack.cache_output()
    );

    let served = stack.get_cache("/", &[]);
    assert_eq!(served.status, 200, "{}", served.text());
    assert_eq!(served.text(), "first: one");
    let hit = stack.get_cache("/", &[]);
    assert!(
        hit.header("age").is_some(),
        "the second GET is a memory hit, so the disk write is not on that path"
    );

    // The slice runs on an idle turn, not once a second: the file is on disk
    // without waiting for the accounting tick. The deadline is under that
    // tick, so a regression back to tick-only writes is caught here.
    assert!(
        wait_until(Duration::from_millis(900), || stack
            .disk_u64("dir_bytes")
            .is_some_and(|b| b > 0)),
        "/perf never showed bytes on disk within one idle slice:\n{}",
        stack.cache_output()
    );
    assert!(
        stack.disk_block().contains("\"queued\":0"),
        "the tick drained the queue: {}",
        stack.disk_block()
    );
    // The loop's own longest iteration while the writer was busy. The direct
    // load test cannot observe `work_max_ns` without the live loop, so the
    // reporting claim lives here: `/perf` must serve the field, and it must be
    // non-zero once a window has covered the slice.
    let startup = stack.longest_loop_work_ns();
    assert!(
        wait_until(m6_core::testkit::STARTUP_BOUND, || {
            stack.longest_loop_work_ns() > 0
        }),
        "/perf never reported the loop's work_max_ns while the writer worked:\n{}",
        stack.cache_output()
    );
    println!(
        "loop work_max_ns: startup window {startup}, window after the disk slice {}",
        stack.longest_loop_work_ns()
    );

    let files = stack.files_for("0");
    assert_eq!(files.len(), 1, "one served path is one file: {files:?}");
    println!(
        "left {} bytes in {}",
        std::fs::metadata(&files[0]).unwrap().len(),
        files[0].display()
    );
    let (id, key, response) = disk_cache::decode(&std::fs::read(&files[0]).unwrap())
        .unwrap_or_else(|e| panic!("decode {}: {e}", files[0].display()));
    assert_eq!(
        id, 0,
        "the file carries the generation it was written under"
    );
    assert!(key.starts_with('/'), "the key is the cache key: {key}");
    assert_eq!(response.status, 200);
    assert_eq!(
        response.genid, 0,
        "the stored entry carries the generation too"
    );
    assert_eq!(&response.body[..], b"first: one");
}

/// A slice stops when its loop-time budget is used up and the next one resumes:
/// one entry is always attempted first, and a zero budget then stops the slice
/// after exactly one unit of work.
///
/// The slice is [`disk_cache::DiskWriter::tick`], so this test calls it
/// directly and never waits for a real accounting tick. It builds the writer,
/// queues 500 entries of 10,000 bytes and seeds 1,000 files under generation
/// `999`, then ticks until the queue and the old directory are empty. Each call
/// is the same slice the loop runs on an idle turn while the writer has work.
///
/// The budget is zero rather than the production 1 ms because a wall-clock
/// assertion cannot hold on a loaded host: the old median assertion failed
/// under 16 CPU burners on 2026-11-16. What is asserted here is the behaviour
/// the budget promises, and how long a slice takes is left to the perf gate.
///
/// Break: drop the time check from `tick`, and the first slice writes the whole
/// queue in one call. Break the one-unit guarantee, and a zero budget writes
/// nothing on any slice and the queue never drains.
#[test]
fn a_full_tick_under_load_stops_at_the_time_budget() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("disk-cache");
    let old = root.join("999");
    std::fs::create_dir_all(&old).unwrap();
    for i in 0..LOAD_OLD_FILES {
        std::fs::write(old.join(format!("{i:064x}")), b"old generation file").unwrap();
    }
    let mut writer = disk_cache::DiskWriter::new(root.clone(), 128 * 1024 * 1024).expect("writer");
    writer.use_id(0);
    let mut cache = Cache::with_max_bytes(128 * 1024 * 1024);
    for i in 0..LOAD_KEYS {
        cache.insert_queued(
            // A distinct path per key: `CacheKey::new` strips a query from the
            // path, so the same path with different queries would collide.
            CacheKey::new(&format!("/load/{i}"), None, ""),
            CachedResponse {
                status: 200,
                headers: HeaderBlock::empty(),
                body: bytes::Bytes::from(vec![b'x'; LOAD_BODY_LEN]),
                genid: 0,
            },
        );
    }
    assert_eq!(cache.queued(), LOAD_KEYS);

    // A zero loop-time budget makes the guarantee exact: one unit of work per
    // slice, then the budget stops it and the next slice resumes. A wall-clock
    // budget cannot promise a number of files on a loaded host, which is what
    // made this test's median assertion fail under 16 CPU burners on
    // 2026-11-16. Timing belongs to the perf gate.
    writer.set_time_budget(Duration::ZERO);

    // One sample per slice: the files it wrote, the old files it removed, and
    // how many entries it left flagged for the slices that follow.
    let mut samples: Vec<(usize, usize, usize)> = Vec::new();
    while cache.queued() > 0 || old.exists() {
        assert!(
            samples.len() < 10_000,
            "the queue did not drain in 10,000 slices"
        );
        let stats = writer.tick(&mut cache);
        samples.push((stats.written, stats.removed, cache.queued()));
    }
    assert_eq!(writer.ticks() as usize, samples.len());

    let files = files_in(&root.join("0"));
    assert_eq!(files.len(), LOAD_KEYS, "every queued entry became one file");
    assert!(!old.exists(), "the old generation directory was removed");
    let want_bytes = (LOAD_KEYS * LOAD_BODY_LEN) as u64;
    assert!(
        writer.dir_bytes() >= want_bytes,
        "{} bytes on disk is less than the {want_bytes} queued",
        writer.dir_bytes()
    );

    let total_written: usize = samples.iter().map(|(written, ..)| written).sum();
    assert_eq!(
        total_written, LOAD_KEYS,
        "every queued entry was written exactly once"
    );
    let total_removed: usize = samples.iter().map(|(_, removed, _)| removed).sum();
    assert_eq!(
        total_removed, LOAD_OLD_FILES,
        "every file of the old generation was removed exactly once"
    );

    // The first slice that wrote anything stopped with the other entries still
    // flagged: the flag on an entry the budget could not reach is what the next
    // slice finds. A zero budget makes that exact, so it is asserted rather
    // than printed.
    let (first_written, _, queued_after) = *samples.first().expect("at least one slice");
    assert_eq!(
        first_written, 1,
        "a zero budget must write one unit of work, not {first_written}"
    );
    assert_eq!(
        queued_after,
        LOAD_KEYS - 1,
        "a slice clears only the flag of the entry it wrote"
    );

    let write_ticks = samples.iter().filter(|(written, ..)| *written > 0).count();
    assert!(
        write_ticks > LOAD_KEYS / 8,
        "500 files took only {write_ticks} write slices; the budget must spread them"
    );
    for (written, removed, _) in &samples {
        assert!(
            *written <= 1,
            "one slice wrote {written} files; the budget must stop it after one unit"
        );
        assert!(
            *removed <= 1,
            "one slice removed {removed} files; the budget must stop it after one unit"
        );
    }
    println!(
        "zero-budget slices: {} ticks wrote {total_written} files and removed \
         {total_removed}, {write_ticks} of them wrote, and the first left \
         {queued_after} entries flagged",
        samples.len(),
    );
}

/// The files under `dir`, not counting a leftover `.tmp`.
fn files_in(dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .map(|e| e.path())
                .filter(|p| p.is_file() && !p.to_string_lossy().ends_with(".tmp"))
                .collect()
        })
        .unwrap_or_default();
    files.sort();
    files
}

/// The node stops, and the file it left behind still decodes to the response it
/// served. A restarted node's start-time load is the other half: these are the bytes it
/// reads, complete without the writer and with their check value intact.
///
/// Break: write the file only on the writer's drop, and the stopped node leaves
/// nothing to read.
#[test]
fn a_restart_reads_the_file_the_node_wrote() {
    let mut stack = Stack::new();
    stack.start();
    assert!(stack.wait_perf_id("0", m6_core::testkit::RESPONSE_BOUND));
    let served = stack.get_cache("/", &[]);
    assert_eq!(served.text(), "first: one");
    assert!(
        wait_until(m6_core::testkit::STARTUP_BOUND, || !stack
            .files_for("0")
            .is_empty()),
        "no file appeared under the generation directory:\n{}",
        stack.cache_output()
    );

    stack.stop_cache();
    let files = stack.files_for("0");
    let (id, _key, response) = disk_cache::decode(&std::fs::read(&files[0]).unwrap())
        .unwrap_or_else(|e| panic!("decode {}: {e}", files[0].display()));
    assert_eq!(id, 0);
    assert_eq!(&response.body[..], served.body.as_slice());
}

/// A new generation writes into its own directory, and the old directory is
/// removed over the ticks that follow.
///
/// Break: skip `disk.use_id` in the generation change, and the new file lands
/// in the old directory and the old directory is never removed.
#[test]
fn a_generation_change_moves_the_files_and_removes_the_old_directory() {
    let mut stack = Stack::new();
    stack.start();
    assert!(stack.wait_perf_id("0", m6_core::testkit::RESPONSE_BOUND));
    assert_eq!(stack.get_cache("/", &[]).text(), "first: one");
    assert!(
        wait_until(m6_core::testkit::STARTUP_BOUND, || !stack
            .files_for("0")
            .is_empty()),
        "the first generation never wrote a file:\n{}",
        stack.cache_output()
    );
    assert_eq!(stack.generation_dirs(), vec!["0".to_string()]);

    std::fs::write(
        stack.source_site.join("data/page.json"),
        "{\"title\": \"two\"}",
    )
    .unwrap();
    let token = stack.write_token();
    let posted = stack.post_source("/genid", Some(&token));
    assert_eq!(posted.status, 200, "{}", posted.text());
    assert_eq!(posted.genid(), "1");

    assert!(
        stack.wait_perf_id("1", m6_core::testkit::RESPONSE_BOUND),
        "the cache never followed the publish:\n{}",
        stack.cache_output()
    );
    let served = stack.get_cache("/", &[]);
    assert_eq!(served.status, 200);
    assert_eq!(served.text(), "first: two");
    assert!(
        wait_until(m6_core::testkit::STARTUP_BOUND, || !stack
            .files_for("1")
            .is_empty()),
        "the new generation never wrote a file:\n{}",
        stack.cache_output()
    );
    let (id, _key, response) =
        disk_cache::decode(&std::fs::read(&stack.files_for("1")[0]).unwrap()).expect("decode");
    assert_eq!(id, 1);
    assert_eq!(&response.body[..], b"first: two");

    assert!(
        wait_until(m6_core::testkit::STARTUP_BOUND, || !stack
            .cache_dir
            .join("0")
            .exists()),
        "the old generation's directory was never removed:\n{}",
        stack.cache_output()
    );
    assert_eq!(stack.generation_dirs(), vec!["1".to_string()]);
}

/// A restart whose source still holds the same ID starts warm: the first GET
/// after the restart is a cache hit, served from the entry the previous
/// process wrote, with no request reaching the source.
///
/// Break: install a load without waiting for the first heartbeat, and test b
/// serves the old body on its first GET and fails. Break the load itself, and
/// the first GET is a miss.
#[test]
fn a_restart_with_the_same_id_starts_warm() {
    let mut stack = Stack::new();
    stack.start();
    assert!(
        stack.wait_perf_id("0", m6_core::testkit::RESPONSE_BOUND),
        "/perf never showed the source's ID:\n{}",
        stack.cache_output()
    );
    let served = stack.get_cache("/", &[]);
    assert_eq!(served.status, 200);
    assert_eq!(served.text(), "first: one");
    assert!(
        wait_until(m6_core::testkit::STARTUP_BOUND, || !stack
            .files_for("0")
            .is_empty()),
        "no file appeared under the generation directory:\n{}",
        stack.cache_output()
    );

    stack.stop_cache();
    stack.start_cache();
    assert!(
        wait_until(m6_core::testkit::STARTUP_BOUND, || stack
            .disk_state()
            .as_deref()
            == Some("kept")),
        "the restart never kept the disk copy: {:?}\n{}",
        stack.disk_block(),
        stack.cache_output()
    );
    assert_eq!(
        stack.disk_u64("loaded_entries"),
        Some(1),
        "one served path is one loaded entry"
    );
    assert_eq!(stack.disk_u64("loaded_bad"), Some(0));

    let hits = stack.perf_u64("cache_hits_total").expect("a hit counter");
    let hit = stack.get_cache("/", &[]);
    assert_eq!(hit.status, 200);
    assert_eq!(
        hit.text(),
        "first: one",
        "the restarted node served what it wrote"
    );
    assert!(hit.header("age").is_some(), "a cached hit carries its age");
    assert_eq!(
        stack.perf_u64("cache_hits_total"),
        Some(hits + 1),
        "the first GET after the restart is a hit, and a hit is the only path \
         that increments this counter"
    );
    assert!(
        wait_until(m6_core::testkit::STARTUP_BOUND, || stack
            .cache_output()
            .contains("disk cache kept")),
        "the kept decision is logged:\n{}",
        stack.cache_output()
    );
}

/// A restart after a publish starts cold: the ID moved while the node was
/// down, so the loaded copy is dropped, the first GET reaches the source and
/// the old generation's directory is removed. A first heartbeat that times out
/// cannot name the change, so the node serves the copy unverified until an
/// answer names the new ID and drops it then. Both are cold starts, and both
/// are asserted.
///
/// Break: keep the load on every first answer, and the first GET serves
/// `first: one` from a generation the site has left.
#[test]
fn a_restart_after_a_publish_starts_cold() {
    let mut stack = Stack::new();
    stack.start();
    assert!(stack.wait_perf_id("0", m6_core::testkit::RESPONSE_BOUND));
    assert_eq!(stack.get_cache("/", &[]).text(), "first: one");
    assert!(
        wait_until(m6_core::testkit::STARTUP_BOUND, || !stack
            .files_for("0")
            .is_empty()),
        "the first generation never wrote a file:\n{}",
        stack.cache_output()
    );
    stack.stop_cache();

    // The publish happens while the cache node is stopped, which is the case a
    // restart has to notice: its disk copy is one generation old.
    std::fs::write(
        stack.source_site.join("data/page.json"),
        "{\"title\": \"two\"}",
    )
    .unwrap();
    let token = stack.write_token();
    let posted = stack.post_source("/genid", Some(&token));
    assert_eq!(posted.status, 200, "{}", posted.text());
    assert_eq!(posted.genid(), "1");

    stack.start_cache();
    assert!(
        wait_until(m6_core::testkit::STARTUP_BOUND, || stack
            .disk_state()
            .as_deref()
            == Some("discarded")),
        "the restart never discarded the old generation: {:?}\n{}",
        stack.disk_block(),
        stack.cache_output()
    );
    // Two designed paths end in `discarded`, and which one runs is not the
    // test's to choose. When the first heartbeat answers, the copy is dropped
    // for the reason this test is named after. When that heartbeat times out on
    // a loaded host, the node installs the copy unverified and says so, then
    // drops it on the next answer that names the new ID. Both reasons are
    // logged and both end cold, so either line satisfies this. Requiring the
    // first one made a healthy host under load fail on the heartbeat's own
    // timeout, which is the fixed deadline this branch exists to remove.
    assert!(
        wait_until(m6_core::testkit::STARTUP_BOUND, || {
            let out = stack.cache_output();
            out.contains("disk cache discarded: the site changed while this node was down")
                || out.contains("disk cache served unverified: the upstream did not answer")
        }),
        "the reason is logged:\n{}",
        stack.cache_output()
    );
    let served = stack.get_cache("/", &[]);
    assert_eq!(served.status, 200);
    assert_eq!(
        served.text(),
        "first: two",
        "the first GET reached the source's new generation"
    );
    assert!(
        wait_until(m6_core::testkit::STARTUP_BOUND, || !stack
            .cache_dir
            .join("0")
            .exists()),
        "the old generation's directory was never removed:\n{}",
        stack.cache_output()
    );
    assert_eq!(stack.generation_dirs(), vec!["1".to_string()]);
}

/// A restart with the source down serves the disk copy unverified, and the
/// first successful heartbeat confirms it. Nothing is flushed while the site
/// has not changed, and the first publish after the recovery flushes it.
///
/// Break: discard the load when no answer arrives, and the node has no pages
/// with its source down.
#[test]
fn a_restart_with_the_source_down_serves_the_disk_copy() {
    let mut stack = Stack::new();
    stack.start();
    assert!(stack.wait_perf_id("0", m6_core::testkit::RESPONSE_BOUND));
    assert_eq!(stack.get_cache("/", &[]).text(), "first: one");
    assert!(
        wait_until(m6_core::testkit::STARTUP_BOUND, || !stack
            .files_for("0")
            .is_empty()),
        "the first generation never wrote a file:\n{}",
        stack.cache_output()
    );
    stack.stop_cache();
    stack.stop_source();

    stack.start_cache();
    assert!(
        wait_until(m6_core::testkit::STARTUP_BOUND, || stack
            .disk_state()
            .as_deref()
            == Some("unverified")),
        "the restart never served the disk copy unverified: {:?}\n{}",
        stack.disk_block(),
        stack.cache_output()
    );
    assert!(
        wait_until(m6_core::testkit::STARTUP_BOUND, || stack
            .cache_output()
            .contains(
                "disk cache served unverified: the upstream did not answer"
            )),
        "the unverified install is logged:\n{}",
        stack.cache_output()
    );
    let offline = stack.get_cache("/", &[]);
    assert_eq!(offline.status, 200, "a cache serves what it has when down");
    assert_eq!(offline.text(), "first: one");

    // The source keeps its state directory, so its ID is still 0.
    stack.start_source();
    assert!(
        wait_until(m6_core::testkit::STARTUP_BOUND, || stack
            .genid_block()
            .contains("\"upstream_down\":false")),
        "the cache node never reconnected:\n{}",
        stack.cache_output()
    );
    assert!(
        wait_until(m6_core::testkit::STARTUP_BOUND, || stack
            .disk_state()
            .as_deref()
            == Some("kept")),
        "the confirmed disk copy is kept: {:?}\n{}",
        stack.disk_block(),
        stack.cache_output()
    );
    // The confirmation is a state transition the operator can see: the log
    // said "served unverified" when the first answer timed out, so it says
    // "kept" when an answer confirms the copy.
    assert!(
        wait_until(m6_core::testkit::STARTUP_BOUND, || stack
            .cache_output()
            .contains("disk cache kept")),
        "the confirmation is logged:\n{}",
        stack.cache_output()
    );
    assert_eq!(stack.perf_id().as_deref(), Some("0"));
    let hits = stack.perf_u64("cache_hits_total").expect("a hit counter");
    let unchanged = stack.get_cache("/", &[]);
    assert_eq!(unchanged.text(), "first: one");
    assert_eq!(
        stack.perf_u64("cache_hits_total"),
        Some(hits + 1),
        "an unchanged site is not flushed"
    );

    std::fs::write(
        stack.source_site.join("data/page.json"),
        "{\"title\": \"two\"}",
    )
    .unwrap();
    let token = stack.write_token();
    let posted = stack.post_source("/genid", Some(&token));
    assert_eq!(posted.status, 200, "{}", posted.text());
    assert_eq!(posted.genid(), "1");
    assert!(
        stack.wait_perf_id("1", m6_core::testkit::RESPONSE_BOUND),
        "the cache never followed the publish:\n{}",
        stack.cache_output()
    );
    let fresh = stack.get_cache("/", &[]);
    assert_eq!(fresh.status, 200);
    assert_eq!(
        fresh.text(),
        "first: two",
        "the publish flushed the loaded copy, so the GET reached the source"
    );
}
