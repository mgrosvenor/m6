//! A cache node writes its cache to disk off the request path.
//!
//! Every test starts a real `m6-http` source in front of a real `m6-html`, and a
//! second `m6-http` cache node with `[cache] dir` in its system config. The
//! request path only queues a key. The accounting tick writes the file under
//! `<dir>/<genid>/`, and a generation change moves writes to a new directory and
//! removes the old one over the ticks that follow. Nothing waits on a fixed
//! sleep for a result: every wait polls a condition with a deadline.
//!
//! The files are read back with `disk_cache::decode`, the function G2b's
//! start-time load calls. That is the restart half this branch can prove: the
//! bytes are on disk, complete without the writer, and decode to the response
//! the node served. Inserting them into a fresh memory cache at start is G2b.
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
use m6_http_lib::disk_cache;

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

// ── Crypto fixture ───────────────────────────────────────────────────────────

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
        sub: "disk-cache-test".to_string(),
        exp: now + 3600,
        roles: roles.iter().map(|r| r.to_string()).collect(),
    };
    let key = jsonwebtoken::EncodingKey::from_ec_pem(private_pem.as_bytes()).expect("ec key");
    let header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::ES256);
    jsonwebtoken::encode(&header, &claims, &key).expect("sign jwt")
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
    private_pem: String,
}

impl Stack {
    fn new() -> Stack {
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
        std::fs::write(d.join("metrics-token"), "perf-token\n").unwrap();
        std::fs::write(
            d.join("genid-token"),
            format!("{}\n", mint(&private_pem, &["reader", "writer"])),
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
            "global_params = [\"data/page.json\"]\n\n\
             [[route]]\npath = \"/\"\ntemplate = \"templates/index.html\"\ncache = \"public, max-age=60\"\n\n\
             [[route]]\npath = \"/other\"\ntemplate = \"templates/other.html\"\ncache = \"public, max-age=60\"\n",
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
                 [node]\nname = \"disk-source\"\n\n\
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
                "[server]\nbind = \"{}\"\ntls_cert = \"{}/cert.pem\"\ntls_key = \"{}/key.pem\"\n\n\
                 [node]\nname = \"disk-cache\"\n\n\
                 [health]\nmetrics_token_file = \"{}/metrics-token\"\n\n\
                 [genid]\nupstream = \"origin\"\ntoken_file = \"{}/genid-token\"\nheartbeat_secs = 1\n\n\
                 [cache]\ndir = \"{}\"\n",
                cache_port.addr(),
                d.display(),
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
            private_pem,
        }
    }

    fn write_token(&self) -> String {
        mint(&self.private_pem, &["writer"])
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

    fn stop_cache(&mut self) {
        if let Some(mut svc) = self.cache.take() {
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

    fn perf(&self) -> Resp {
        self.get_cache("/perf", &[("Authorization", "Bearer perf-token")])
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

    /// The longest loop iteration `/perf` currently reports, in nanoseconds.
    fn longest_loop_work_ns(&self) -> u64 {
        let body = self.perf().text();
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

/// A served response reaches the disk on a tick, under the node's generation
/// directory, and decodes to what the client was served.
///
/// Break: skip `disk.tick` in the accounting tick, and `/perf` `dir_bytes`
/// stays 0 and no file appears.
#[test]
fn a_cache_node_writes_what_it_serves_to_disk() {
    let mut stack = Stack::new();
    stack.start();
    assert!(
        stack.wait_perf_id("0", Duration::from_secs(10)),
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

    assert!(
        wait_until(Duration::from_secs(10), || stack
            .disk_u64("dir_bytes")
            .is_some_and(|b| b > 0)),
        "/perf never showed bytes on disk:\n{}",
        stack.cache_output()
    );
    assert!(
        stack.disk_block().contains("\"queued\":0"),
        "the tick drained the queue: {}",
        stack.disk_block()
    );
    // The loop's own longest iteration while the writer was busy, for the
    // report's latency line. The first `/perf` window covers startup, so this
    // waits for the next one, which contains the tick that did the write. It
    // is a measurement, not an assertion.
    let startup = stack.longest_loop_work_ns();
    wait_until(Duration::from_secs(2), || {
        let now = stack.longest_loop_work_ns();
        now > 0 && now != startup
    });
    println!(
        "loop work_max_ns: startup window {startup}, window with the disk tick {}",
        stack.longest_loop_work_ns()
    );

    let files = stack.files_for("0");
    assert_eq!(files.len(), 1, "one served path is one file: {files:?}");
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

/// The node stops, and the file it left behind still decodes to the response it
/// served. A restarted node's start-time load is G2b: these are the bytes it
/// reads, complete without the writer and with their check value intact.
///
/// Break: write the file only on the writer's drop, and the stopped node leaves
/// nothing to read.
#[test]
fn a_restart_reads_the_file_the_node_wrote() {
    let mut stack = Stack::new();
    stack.start();
    assert!(stack.wait_perf_id("0", Duration::from_secs(10)));
    let served = stack.get_cache("/", &[]);
    assert_eq!(served.text(), "first: one");
    assert!(
        wait_until(Duration::from_secs(10), || !stack.files_for("0").is_empty()),
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
    assert!(stack.wait_perf_id("0", Duration::from_secs(10)));
    assert_eq!(stack.get_cache("/", &[]).text(), "first: one");
    assert!(
        wait_until(Duration::from_secs(10), || !stack.files_for("0").is_empty()),
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
        stack.wait_perf_id("1", Duration::from_secs(10)),
        "the cache never followed the publish:\n{}",
        stack.cache_output()
    );
    let served = stack.get_cache("/", &[]);
    assert_eq!(served.status, 200);
    assert_eq!(served.text(), "first: two");
    assert!(
        wait_until(Duration::from_secs(10), || !stack.files_for("1").is_empty()),
        "the new generation never wrote a file:\n{}",
        stack.cache_output()
    );
    let (id, _key, response) =
        disk_cache::decode(&std::fs::read(&stack.files_for("1")[0]).unwrap()).expect("decode");
    assert_eq!(id, 1);
    assert_eq!(&response.body[..], b"first: two");

    assert!(
        wait_until(Duration::from_secs(10), || !stack
            .cache_dir
            .join("0")
            .exists()),
        "the old generation's directory was never removed:\n{}",
        stack.cache_output()
    );
    assert_eq!(stack.generation_dirs(), vec!["1".to_string()]);
}
