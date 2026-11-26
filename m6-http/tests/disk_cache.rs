//! A cache node writes its cache to disk off the request path.
//!
//! Every test starts a real `m6-http` origin in front of a real `m6-html`, and
//! a second `m6-http` cache node with `[cache] dir` and `[registration] origin`
//! in its system config. The request path only queues a key. The accounting
//! tick writes one file per entry under `<dir>/<sha256 of the key>`, and the
//! change counter is saved in `<dir>/change-counter`. A purge raises the
//! origin's counter, and on reconnect a cache node whose saved counter differs
//! purges its folder and refills. Nothing waits on a fixed sleep for a result:
//! every wait polls a condition with a deadline.
//!
//! The files are read back with `disk_cache::decode`, the function the
//! start-time load calls. That is the restart half this test proves: the bytes
//! are on disk, complete without the writer, and decode to the response the
//! node served.

#![allow(
    clippy::disallowed_methods,
    reason = "test code: threads, sleeps and blocking clients drive the services from outside"
)]

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rustls::StreamOwned;

use m6_core::auth::Identity;
use m6_core::testkit::{binary, claim_port, PortClaim, Reply, ServingCheck, Service};

fn identity(role: &str) -> Identity {
    Identity {
        sub: format!("{role}-1"),
        username: role.to_string(),
        roles: vec![role.to_string()],
        groups: Vec::new(),
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

struct Resp {
    status: u16,
    body: String,
}

/// One raw TLS request, with an `Authorization: Bearer` header when a token is
/// given.
fn request(port: u16, method: &str, path: &str, token: Option<&str>) -> Resp {
    let tcp = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    tcp.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    let name = rustls::pki_types::ServerName::try_from("127.0.0.1".to_string()).unwrap();
    let conn = rustls::ClientConnection::new(no_verify_tls(), name).unwrap();
    let mut stream = StreamOwned::new(conn, tcp);
    let mut req = format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n");
    if let Some(token) = token {
        req.push_str(&format!("Authorization: Bearer {token}\r\n"));
    }
    req.push_str("Connection: close\r\n\r\n");
    stream.write_all(req.as_bytes()).unwrap();
    stream.flush().unwrap();
    let mut raw = Vec::new();
    let _ = stream.read_to_end(&mut raw);
    let text = String::from_utf8_lossy(&raw);
    let end = text.find("\r\n\r\n").unwrap_or(raw.len());
    let status = text
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|status| status.parse().ok())
        .unwrap_or(0);
    Resp {
        status,
        body: text.get(end + 4..).unwrap_or("").to_string(),
    }
}

/// The origin and cache node under test.
struct Stack {
    _html: Service,
    _source: Service,
    _check: ServingCheck,
    _dir: tempfile::TempDir,
    _source_port: PortClaim,
    _control_port: PortClaim,
    cache_port: u16,
    cache_dir: PathBuf,
    cache: Option<Service>,
}

impl Stack {
    fn start() -> Stack {
        rustls::crypto::aws_lc_rs::default_provider()
            .install_default()
            .ok();
        let dir = tempfile::Builder::new()
            .prefix("m6-disk-cache-")
            .tempdir_in(m6_core::testkit::socket_scratch_root())
            .unwrap();
        let site = dir.path();

        let cert = rcgen::generate_simple_self_signed(vec!["127.0.0.1".to_string()]).unwrap();
        std::fs::write(site.join("cert.pem"), cert.cert.pem()).unwrap();
        std::fs::write(site.join("key.pem"), cert.key_pair.serialize_pem()).unwrap();

        let check_path = site.join("check.sock");
        let check = ServingCheck::start(
            &check_path,
            vec![
                (
                    "writer".to_string(),
                    Reply::Identity(identity("writer")),
                ),
                (
                    "reader".to_string(),
                    Reply::Identity(identity("reader")),
                ),
            ],
        )
        .expect("the check server starts");

        // The content backend behind the origin.
        let file_sock = site.join("m6-file-1.sock");
        let file_glob = site.join("m6-file-*.sock");
        let file_conf = site.join("m6-file.conf");
        std::fs::write(
            &file_conf,
            "[[route]]\npath = \"/{relpath}\"\nhandler = \"files\"\nroot = \".\"\n",
        )
        .unwrap();
        std::fs::write(site.join("index.html"), b"first").unwrap();
        std::fs::write(
            site.join("site.toml"),
            format!(
                r#"
[site]
name = "disk-cache"
domain = "localhost"

[log]
level = "debug"

[analytics]
enabled = false

[server]
backend_timeout_secs = 5

[[backend]]
name = "m6-file"
sockets = "{file_glob}"

[[backend]]
name = "auth-check"
sockets = "{check}"

[[route]]
path = "/"
backend = "m6-file"
cache = "public, max-age=3600"

[[route]]
path = "/{{*path}}"
backend = "m6-file"
"#,
                file_glob = file_glob.display(),
                check = check_path.display(),
            ),
        )
        .unwrap();

        let source_port = claim_port();
        let control_port = claim_port();
        let cache_port = claim_port();

        let source_system = site.join("source-system.toml");
        std::fs::write(
            &source_system,
            format!(
                r#"
[server]
bind = "127.0.0.1:{source_port}"
registration_bind = "127.0.0.1:{control_port}"
tls_cert = "{cert}"
tls_key = "{key}"

[node]
name = "origin"

[auth]
backend = "auth-check"

[registration]
read_require = "role:reader"
write_require = "role:writer"
"#,
                source_port = source_port.port(),
                control_port = control_port.port(),
                cert = site.join("cert.pem").display(),
                key = site.join("key.pem").display(),
            ),
        )
        .unwrap();

        // The cache node, with its disk directory.
        let cache_dir = site.join("disk-cache");
        std::fs::create_dir_all(&cache_dir).unwrap();
        let cache_site = site.join("cache-site");
        std::fs::create_dir_all(&cache_site).unwrap();
        std::fs::write(
            cache_site.join("site.toml"),
            format!(
                r#"
[site]
name = "disk-cache-node"
domain = "localhost"

[log]
level = "debug"

[analytics]
enabled = false

[server]
backend_timeout_secs = 5

[[backend]]
name = "origin"
url = "https://127.0.0.1:{source_port}"
tls_skip_verify = true

[[backend]]
name = "auth-check"
sockets = "{check}"

[[route]]
path = "/"
backend = "origin"

[[route]]
path = "/{{*path}}"
backend = "origin"
"#,
                source_port = source_port.port(),
                check = check_path.display(),
            ),
        )
        .unwrap();

        let cache_system = site.join("cache-system.toml");
        std::fs::write(
            &cache_system,
            format!(
                r#"
[server]
bind = "127.0.0.1:{cache_port}"
tls_cert = "{cert}"
tls_key = "{key}"

[node]
name = "disk-cache-node"

[auth]
backend = "auth-check"

[registration]
origin = "127.0.0.1:{control_port}"

[cache]
dir = "{cache_dir}"
"#,
                cache_port = cache_port.port(),
                cert = site.join("cert.pem").display(),
                key = site.join("key.pem").display(),
                control_port = control_port.port(),
                cache_dir = cache_dir.display(),
            ),
        )
        .unwrap();

        let html = Service::spawn(
            "m6-file",
            Command::new(binary("m6-file"))
                .arg(site)
                .arg(&file_conf)
                .env("M6_SOCKET_OVERRIDE", &file_sock),
        );
        m6_core::testkit::wait::until(m6_core::testkit::STARTUP_BOUND, || {
            std::path::Path::new(&file_sock).exists()
        });

        let mut source = Service::spawn(
            "m6-http-origin",
            Command::new(binary("m6-http")).arg(site).arg(&source_system),
        );
        source.wait_for_tcp(source_port.port(), m6_core::testkit::STARTUP_BOUND);

        let mut stack = Stack {
            _html: html,
            _source: source,
            _check: check,
            _dir: dir,
            _source_port: source_port,
            _control_port: control_port,
            cache_port: cache_port.port(),
            cache_dir,
            cache: None,
        };
        stack.start_cache();
        stack
    }

    fn start_cache(&mut self) {
        let site = self._dir.path();
        let mut svc = Service::spawn(
            "m6-http-cache",
            Command::new(binary("m6-http"))
                .arg(site.join("cache-site"))
                .arg(site.join("cache-system.toml"))
                .env("M6_TEST_STATS_INTERVAL_SECS", "1")
                .stdin(Stdio::null()),
        );
        svc.wait_for_tcp(self.cache_port, m6_core::testkit::STARTUP_BOUND);
        self.cache = Some(svc);
    }

    fn get_cache(&self, path: &str) -> Resp {
        request(self.cache_port, "GET", path, None)
    }

    fn purge_origin(&self, path: &str) -> Resp {
        request(self._source_port.port(), "PURGE", path, Some("writer"))
    }

    /// The cache node's saved change counter, read from its disk folder.
    fn saved_counter(&self) -> Option<u64> {
        m6_http_lib::disk_cache::read_change_counter(&self.cache_dir).ok().flatten()
    }

    /// The cache files under the one folder, excluding the change counter and
    /// a leftover `.tmp`.
    fn cache_files(&self) -> Vec<PathBuf> {
        let mut files: Vec<PathBuf> = std::fs::read_dir(&self.cache_dir)
            .map(|rd| {
                rd.flatten()
                    .map(|e| e.path())
                    .filter(|p| {
                        p.is_file()
                            && p.file_name()
                                .is_some_and(|n| n != "change-counter")
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

    fn stop_cache(&mut self) {
        if let Some(mut svc) = self.cache.take() {
            svc.kill();
        }
    }

    fn cache_output(&self) -> String {
        self.cache.as_ref().map(Service::output).unwrap_or_default()
    }
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

/// A served response reaches the disk on an idle slice, in the one-folder
/// layout, and decodes to what the client was served.
///
/// Break: skip the idle-turn slice, and no file appears.
#[test]
fn a_cache_node_writes_what_it_serves_to_disk() {
    let stack = Stack::start();

    let served = stack.get_cache("/");
    assert_eq!(served.status, 200, "{}", stack.cache_output());
    assert_eq!(served.body, "first");
    // The second GET is a memory hit, so the disk write is not on that path.
    let hit = stack.get_cache("/");
    assert_eq!(hit.status, 200);

    assert!(
        wait_until(Duration::from_secs(5), || !stack.cache_files().is_empty()),
        "no file appeared within the slice window:\n{}",
        stack.cache_output()
    );
    let files = stack.cache_files();
    assert_eq!(files.len(), 1, "one served path is one file: {files:?}");
    let (key, response) = m6_http_lib::disk_cache::decode(&std::fs::read(&files[0]).unwrap())
        .unwrap_or_else(|e| panic!("decode {}: {e}", files[0].display()));
    assert!(key.starts_with('/'), "the key is the cache key: {key}");
    assert_eq!(response.status, 200);
    assert_eq!(&response.body[..], b"first");
}

/// The node stops, and the file it left behind still decodes to the response
/// it served. A restarted node's start-time load reads these bytes, complete
/// without the writer and with their check value intact.
///
/// Break: write the file only on the writer's drop, and the stopped node
/// leaves nothing to read.
#[test]
fn a_restart_reads_the_file_the_node_wrote() {
    let mut stack = Stack::start();
    assert_eq!(stack.get_cache("/").body, "first");
    assert!(
        wait_until(Duration::from_secs(5), || !stack.cache_files().is_empty()),
        "no file appeared:\n{}",
        stack.cache_output()
    );

    stack.stop_cache();
    let files = stack.cache_files();
    let (_key, response) =
        m6_http_lib::disk_cache::decode(&std::fs::read(&files[0]).unwrap())
            .expect("the stopped node left a complete file");
    assert_eq!(&response.body[..], b"first");
}

/// A restart with the same saved counter starts warm: the disk copy is kept.
///
/// Break: always purge, and the restarted node refetches.
#[test]
fn a_restart_with_the_same_counter_starts_warm() {
    let mut stack = Stack::start();
    assert_eq!(stack.get_cache("/").body, "first");
    assert!(
        wait_until(Duration::from_secs(5), || !stack.cache_files().is_empty()),
        "no file appeared:\n{}",
        stack.cache_output()
    );

    stack.stop_cache();
    let before = stack.cache_files();
    assert!(!before.is_empty());
    let counter = stack.saved_counter();
    assert!(counter.is_some(), "the folder holds a change counter");
    // Change the content so a cold restart is observable, but do NOT purge:
    // the saved counter is unchanged, so the disk copy is still valid.
    std::fs::write(stack._dir.path().join("index.html"), b"second").unwrap();
    stack.start_cache();
    assert_eq!(
        stack.saved_counter(),
        counter,
        "the restarted node kept the same counter"
    );

    assert!(
        wait_until(Duration::from_secs(10), || stack
            .get_cache("/")
            .body
            .eq("first")),
        "the same counter must keep the disk copy:\n{}",
        stack.cache_output()
    );
}

/// A purge raises the origin's counter, so a restarting cache node purges its
/// folder and refills from the origin.
///
/// Break: never reconcile the counter, and the restarted node serves the
/// stale disk copy.
#[test]
fn a_restart_after_a_purge_starts_cold() {
    let mut stack = Stack::start();
    assert_eq!(stack.get_cache("/").body, "first");
    assert!(
        wait_until(Duration::from_secs(5), || !stack.cache_files().is_empty()),
        "no file appeared:\n{}",
        stack.cache_output()
    );

    std::fs::write(stack._dir.path().join("index.html"), b"second").unwrap();
    let purged = stack.purge_origin("/*");
    assert_eq!(purged.status, 200, "the purge must be accepted");

    stack.stop_cache();
    stack.start_cache();

    assert!(
        wait_until(Duration::from_secs(10), || stack
            .get_cache("/")
            .body
            .eq("second")),
        "a different counter must purge the disk copy and refill:\n{}",
        stack.cache_output()
    );
}
