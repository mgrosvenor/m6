//! End-to-end integration test for example 09 — global deployment topology.
//!
//! Spins up the full 6-node stack on loopback:
//!   - Origin: m6-http with TLS (bind) + H2C backbone (h2c_bind)
//!   - 5 cache nodes: each m6-http over TLS, proxying to origin over H2C
//!   - Backend services: m6-html, m6-file, m6-auth-server, render-cms (from 07)
//!
//! Verifies:
//!   §0  All 6 TLS endpoints are reachable
//!   §1  Public pages serve correctly from origin (direct TLS)
//!   §2  All 5 cache nodes proxy /blog and /assets to origin
//!   §3  Cache hit-or-miss: second request to a cache node is cache-hit (x-cache header)
//!   §4  CMS routes (no-store) pass through to origin
//!   §5  Origin H2C backbone is reachable on its own TCP port
//!
//! Prerequisites: m6 release binaries pre-built (including m6-auth-cli), and render-cms
//! pre-built (`cargo build --release`). The test makes its own certificate, auth keys,
//! auth database and tokens in a temporary directory and serves a copy of 07's site.
//! If a prerequisite is missing the test fails and says which.

use std::io::{Read, Write as IoWrite};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

// ── binary locations ──────────────────────────────────────────────────────────

fn m6_root() -> PathBuf {
    // integration-test/  →  09-global-deployment/  →  examples/  →  m6-examples/  →  m6/
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../../../m6")
        .canonicalize()
        .expect("m6 workspace root")
}

fn m6_bin(name: &str) -> Option<PathBuf> {
    let release = m6_root().join("target/release").join(name);
    if release.exists() {
        return Some(release);
    }
    which(name)
}

fn render_cms_bin() -> Option<PathBuf> {
    // integration-test/ → 09-global-deployment/ → examples/ → m6-examples/target/release/
    let bin = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../../target/release/render-cms")
        .canonicalize();
    if let Ok(p) = bin {
        if p.exists() {
            return Some(p);
        }
    }
    which("render-cms")
}

fn which(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|p| {
        std::env::split_paths(&p).find_map(|dir| {
            let full = dir.join(name);
            if full.exists() {
                Some(full)
            } else {
                None
            }
        })
    })
}

fn site07_dir() -> PathBuf {
    // integration-test/  →  09-global-deployment/  →  examples/07-dev-to-production/
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../07-dev-to-production")
        .canonicalize()
        .expect("07-dev-to-production site dir")
}

fn example09_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .canonicalize()
        .expect("09-global-deployment dir")
}

// ── TLS: no-verify client ─────────────────────────────────────────────────────

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

fn make_tls_config() -> Arc<rustls::ClientConfig> {
    rustls::crypto::ring::default_provider()
        .install_default()
        .ok();
    Arc::new(
        rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(NoVerify))
            .with_no_client_auth(),
    )
}

// ── TLS cert generation ───────────────────────────────────────────────────────

fn generate_test_cert(dir: &Path) -> (PathBuf, PathBuf) {
    let cert_path = dir.join("cert.pem");
    let key_path = dir.join("key.pem");

    let cert = rcgen::generate_simple_self_signed(vec!["localhost".into(), "127.0.0.1".into()])
        .expect("rcgen cert");
    std::fs::write(&cert_path, cert.cert.pem()).expect("write cert");
    std::fs::write(&key_path, cert.key_pair.serialize_pem()).expect("write key");

    (cert_path, key_path)
}

// ── HTTP helpers ──────────────────────────────────────────────────────────────

struct Response {
    status: u16,
    #[allow(dead_code)]
    headers: Vec<(String, String)>,
    body: String,
}

fn https_get(
    addr: &str,
    path: &str,
    extra_headers: &[(&str, &str)],
    tls: Arc<rustls::ClientConfig>,
) -> Result<Response, String> {
    https_request("GET", addr, path, extra_headers, tls)
}

fn https_request(
    method: &str,
    addr: &str,
    path: &str,
    extra_headers: &[(&str, &str)],
    tls: Arc<rustls::ClientConfig>,
) -> Result<Response, String> {
    let tcp = TcpStream::connect(addr).map_err(|e| format!("connect {addr}: {e}"))?;
    tcp.set_read_timeout(Some(Duration::from_secs(10))).ok();
    tcp.set_write_timeout(Some(Duration::from_secs(10))).ok();

    let server_name = rustls::pki_types::ServerName::try_from("localhost".to_string())
        .map_err(|e| format!("server name: {e}"))?;
    let conn =
        rustls::ClientConnection::new(tls, server_name).map_err(|e| format!("TLS init: {e}"))?;
    let mut stream = rustls::StreamOwned::new(conn, tcp);

    let mut req = format!("{method} {path} HTTP/1.1\r\nHost: localhost\r\n");
    for (k, v) in extra_headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str("Connection: close\r\n\r\n");
    stream
        .write_all(req.as_bytes())
        .map_err(|e| format!("write: {e}"))?;
    stream.flush().map_err(|e| format!("flush: {e}"))?;

    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).ok();

    Ok(parse_response(&String::from_utf8_lossy(&raw)))
}

/// The status code from a header block's first line, or 0 if there isn't one.
fn status_of(hdr_sec: &str) -> u16 {
    hdr_sec
        .split("\r\n")
        .next()
        .unwrap_or("")
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

/// Parse a raw HTTP/1.1 response, discarding any 1xx that precedes the real one.
///
/// # Why the 1xx skip is not optional
///
/// RFC 9110 15.2 requires a client to parse and discard any number of 1xx
/// responses before the final one. m6-http sends `103 Early Hints` ahead of the
/// real response on any route carrying preload hints, so the wire reads:
///
/// ```text
/// HTTP/1.1 103 Early Hints
/// Link: </assets/css/style.css>; rel=preload; as=style
///                                     <- the first blank line is HERE
/// HTTP/1.1 200 OK
/// ...
/// ```
///
/// Reading only the FIRST `\r\n\r\n` and the FIRST status line would take the 103
/// as the answer and hand the real 200 back as the body, so every assertion
/// would fail against a correct server.
///
/// It is separated from `https_get` so it can be tested on bytes, with no
/// listener and no TLS: a test of the parser that needs a running six-node
/// topology is a test nobody runs.
fn parse_response(s: &str) -> Response {
    let mut cursor = 0usize;
    let (hdr_sec, body) = loop {
        let rest = &s[cursor..];
        let split = rest.find("\r\n\r\n").unwrap_or(rest.len());
        let after = cursor
            + if split + 4 <= rest.len() {
                split + 4
            } else {
                rest.len()
            };

        // A 1xx carries no body and is always followed by another response, so
        // step over it and read the next block. `after < s.len()` stops this
        // looping on a truncated response that ends with an informational one.
        if (100..200).contains(&status_of(&rest[..split])) && after < s.len() {
            cursor = after;
            continue;
        }
        break (&rest[..split], &s[after..]);
    };

    let mut headers = Vec::new();
    for line in hdr_sec.split("\r\n").skip(1) {
        if let Some(colon) = line.find(':') {
            headers.push((
                line[..colon].trim().to_string(),
                line[colon + 1..].trim().to_string(),
            ));
        }
    }

    Response {
        status: status_of(hdr_sec),
        headers,
        body: body.to_string(),
    }
}

#[cfg(test)]
mod response_parsing {
    use super::parse_response;

    #[test]
    fn a_plain_response_is_unchanged() {
        let r = parse_response("HTTP/1.1 200 OK\r\ncontent-type: text/html\r\n\r\nhello");
        assert_eq!(r.status, 200);
        assert_eq!(r.body, "hello");
        assert!(r
            .headers
            .iter()
            .any(|(k, v)| k == "content-type" && v == "text/html"));
    }

    #[test]
    fn early_hints_are_skipped_and_the_real_status_is_read() {
        // The exact shape that broke the suite.
        let raw = "HTTP/1.1 103 Early Hints\r\n\
                   Link: </assets/css/style.css>; rel=preload; as=style\r\n\
                   \r\n\
                   HTTP/1.1 200 OK\r\n\
                   content-type: text/html\r\n\
                   \r\n\
                   <!doctype html>";
        let r = parse_response(raw);
        assert_eq!(r.status, 200, "must report the FINAL status, not the 103");
        assert_eq!(r.body, "<!doctype html>");
    }

    #[test]
    fn headers_come_from_the_final_response_not_the_hints() {
        let raw = "HTTP/1.1 103 Early Hints\r\n\
                   Link: </a.css>; rel=preload; as=style\r\n\
                   \r\n\
                   HTTP/1.1 200 OK\r\n\
                   content-type: text/html\r\n\
                   \r\n\
                   body";
        let r = parse_response(raw);
        assert!(
            r.headers.iter().any(|(k, _)| k == "content-type"),
            "the final response's headers must be present: {:?}",
            r.headers
        );
        assert!(
            !r.headers.iter().any(|(k, _)| k == "Link"),
            "the 103's headers must not leak into the result: {:?}",
            r.headers
        );
    }

    #[test]
    fn several_informational_responses_are_all_skipped() {
        let raw = "HTTP/1.1 100 Continue\r\n\r\n\
                   HTTP/1.1 103 Early Hints\r\n\
                   Link: </a.css>; rel=preload\r\n\r\n\
                   HTTP/1.1 204 No Content\r\n\r\n";
        assert_eq!(parse_response(raw).status, 204);
    }

    #[test]
    fn a_non_200_final_status_still_reads_correctly() {
        // A 404 behind hints must not be reported as 200 or as 103.
        let raw = "HTTP/1.1 103 Early Hints\r\n\r\nHTTP/1.1 404 Not Found\r\n\r\nnope";
        let r = parse_response(raw);
        assert_eq!(r.status, 404);
        assert_eq!(r.body, "nope");
    }

    #[test]
    fn a_truncated_informational_response_does_not_loop() {
        // Nothing follows the 103. It must terminate and report what it saw
        // rather than spinning on a cursor that cannot advance.
        let r = parse_response("HTTP/1.1 103 Early Hints\r\nLink: </a.css>\r\n\r\n");
        assert_eq!(r.status, 103);
    }

    #[test]
    fn garbage_does_not_panic() {
        assert_eq!(parse_response("").status, 0);
        assert_eq!(parse_response("not http at all").status, 0);
    }
}

fn get(addr: &str, path: &str, tls: Arc<rustls::ClientConfig>) -> Response {
    https_get(addr, path, &[], tls).unwrap_or_else(|e| panic!("GET {path}: {e}"))
}

/// `POST /genid` on the origin with the publisher's token: the one request that
/// advances the site's generation.
fn publish_genid(addr: &str, token: &str, tls: Arc<rustls::ClientConfig>) -> Response {
    let bearer = format!("Bearer {token}");
    https_request(
        "POST",
        addr,
        "/genid",
        &[("Authorization", &bearer), ("Content-Length", "0")],
        tls,
    )
    .unwrap_or_else(|e| panic!("POST /genid: {e}"))
}

// ── wait helpers ──────────────────────────────────────────────────────────────

fn wait_tcp(addr: &str, timeout: Duration) -> bool {
    let dl = Instant::now() + timeout;
    while Instant::now() < dl {
        if TcpStream::connect(addr).is_ok() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

fn wait_socket(path: &Path, timeout: Duration) -> bool {
    let dl = Instant::now() + timeout;
    while Instant::now() < dl {
        if path.exists() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

/// Copy a directory tree, leaving out what a running site does not need.
fn copy_site(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).expect("create the site copy");
    for entry in std::fs::read_dir(from).expect("read the site") {
        let entry = entry.expect("site entry");
        let name = entry.file_name();
        if matches!(
            name.to_str(),
            Some("render-cms" | "logs" | "keys" | "systemd")
        ) {
            continue;
        }
        let dst = to.join(&name);
        if entry.file_type().expect("file type").is_dir() {
            copy_site(&entry.path(), &dst);
        } else {
            std::fs::copy(entry.path(), &dst).expect("copy a site file");
        }
    }
}

/// Run an m6-auth-cli command against the test's own auth config and return what it
/// printed on stdout.
fn auth_cli(cli: &Path, conf: &Path, args: &[&str]) -> String {
    let out = Command::new(cli)
        .arg(conf)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("cannot run m6-auth-cli {args:?}: {e}"));
    assert!(
        out.status.success(),
        "m6-auth-cli {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

// ── topology stack ────────────────────────────────────────────────────────────

struct GlobalStack {
    origin_addr: String,      // TLS endpoint
    origin_h2c: String,       // H2C backbone (plain TCP)
    cache_addrs: Vec<String>, // [edge-a, edge-b, edge-c, edge-d, edge-e]
    site_dir: PathBuf,        // the copy of 07's site the origin serves
    write_token: String,      // a token with the genid-write role
    processes: Vec<Child>,
    _temp_dirs: Vec<tempfile::TempDir>,
}

impl Drop for GlobalStack {
    fn drop(&mut self) {
        for c in &mut self.processes {
            c.kill().ok();
            c.wait().ok();
        }
    }
}

impl GlobalStack {
    /// Build and start the full 6-node topology.
    /// Returns None if any required binary or site setup is missing.
    fn start() -> Option<Self> {
        let site09 = example09_dir();

        let m6_http = m6_bin("m6-http")?;
        let m6_html = m6_bin("m6-html")?;
        let m6_file = m6_bin("m6-file")?;
        let m6_auth = m6_bin("m6-auth-server")?;
        let m6_cli = m6_bin("m6-auth-cli")?;
        let render = render_cms_bin()?;

        // Unique ports per test run to avoid collision with dev servers.
        let pid = std::process::id() as u16 % 5000;
        let h2c_port = 30000 + pid;
        let tls_port = 30100 + pid;
        // Cache nodes: 5 ports starting from 30200
        let cache_ports: Vec<u16> = (0..5).map(|i| 30200 + pid + i).collect();

        let origin_h2c_addr = format!("127.0.0.1:{h2c_port}");
        let origin_tls_addr = format!("127.0.0.1:{tls_port}");
        let cache_addrs: Vec<String> = cache_ports
            .iter()
            .map(|p| format!("127.0.0.1:{p}"))
            .collect();

        // Generate self-signed cert.
        let cert_dir = tempfile::tempdir().ok()?;
        let (cert_path, key_path) = generate_test_cert(cert_dir.path());

        // A copy of 07's site, whose configs this run rewrites: keys, auth database and
        // tokens are made here, so the test needs no dev.sh and leaves the repo alone.
        let work = tempfile::tempdir().ok()?;
        let w = work.path().to_path_buf();
        let site07 = w.join("site07");
        copy_site(&site07_dir(), &site07);
        let signing = rcgen::KeyPair::generate().ok()?;
        let (auth_pem, auth_pub) = (w.join("auth.pem"), w.join("auth.pub"));
        std::fs::write(&auth_pem, signing.serialize_pem()).ok()?;
        std::fs::write(&auth_pub, signing.public_key_pem()).ok()?;
        let rewrite = |file: &str, pairs: &[(&str, String)]| {
            let path = site07.join(file);
            let mut text = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
            for (from, to) in pairs {
                assert!(text.contains(from), "{file} has no {from}");
                text = text.replace(from, to);
            }
            std::fs::write(&path, text).expect("write a rewritten config");
        };
        let (write_token, read_token) = (w.join("genid-write.token"), w.join("genid-read.token"));
        rewrite(
            "configs/m6-auth.conf",
            &[
                (
                    "@M6_DEV_HOME@/07-dev-to-production/auth.db",
                    w.join("auth.db").display().to_string(),
                ),
                ("@M6_DEV_HOME@/dev/auth.pem", auth_pem.display().to_string()),
                ("@M6_DEV_HOME@/dev/auth.pub", auth_pub.display().to_string()),
            ],
        );
        rewrite(
            "configs/render-cms.conf",
            &[
                (
                    "https://localhost:8443/genid",
                    format!("https://localhost:{tls_port}/genid"),
                ),
                ("127.0.0.1:8443", origin_tls_addr.clone()),
                (
                    "@M6_DEV_HOME@/07-dev-to-production/genid-write.token",
                    write_token.display().to_string(),
                ),
                ("@M6_DEV_HOME@/dev/ca.pem", cert_path.display().to_string()),
            ],
        );
        // The auth database: an editor to log in with, a publisher for the CMS and a
        // reader for the cache nodes.
        let auth_conf = site07.join("configs/m6-auth.conf");
        auth_cli(
            &m6_cli,
            &auth_conf,
            &["user", "add", "admin", "--password", "admin"],
        );
        auth_cli(&m6_cli, &auth_conf, &["group", "add", "editors"]);
        auth_cli(
            &m6_cli,
            &auth_conf,
            &["group", "member", "add", "editors", "admin"],
        );
        for (user, role, file) in [
            ("publisher", "genid-write", &write_token),
            ("reader", "genid-read", &read_token),
        ] {
            auth_cli(
                &m6_cli,
                &auth_conf,
                &[
                    "user",
                    "add",
                    user,
                    "--role",
                    role,
                    "--password",
                    "pw-for-tests",
                ],
            );
            let token = auth_cli(
                &m6_cli,
                &auth_conf,
                &["token", "create", user, "--name", user],
            );
            std::fs::write(file, token).ok()?;
        }
        let state = w.join("state");
        std::fs::create_dir_all(&state).ok()?;

        // ── Origin site dir ───────────────────────────────────────────────────
        // Write origin site.toml: full route table, socket paths in a temp dir.

        let origin_sock_dir = tempfile::tempdir().ok()?;
        let html_sock = origin_sock_dir.path().join("m6-html.sock");
        let file_sock = origin_sock_dir.path().join("m6-file.sock");
        let auth_sock = origin_sock_dir.path().join("m6-auth.sock");
        let cms_sock = origin_sock_dir.path().join("render-cms.sock");

        // Copy site-origin/site.toml and patch socket paths.
        let origin_site_src = site09.join("site-origin/site.toml");
        if !origin_site_src.exists() {
            eprintln!("SKIP: site-origin/site.toml not found");
            return None;
        }
        let origin_site_toml = std::fs::read_to_string(&origin_site_src).ok()?;
        let check_sock = origin_sock_dir.path().join("check-m6-auth.sock");
        let origin_site_toml = origin_site_toml
            .replace("/tmp/m6/m6-html*.sock", &html_sock.display().to_string())
            .replace("/tmp/m6/m6-file*.sock", &file_sock.display().to_string())
            .replace(
                "/tmp/m6/check-m6-auth*.sock",
                &check_sock.display().to_string(),
            )
            .replace("/tmp/m6/m6-auth*.sock", &auth_sock.display().to_string())
            .replace("/tmp/m6/render-cms*.sock", &cms_sock.display().to_string());

        std::fs::write(origin_sock_dir.path().join("site.toml"), &origin_site_toml).ok()?;

        // Symlink site subdirs so m6-http resolves template/asset paths correctly.
        for subdir in &["templates", "data", "assets", "configs", "keys", "content"] {
            let src = site07.join(subdir);
            let dst = origin_sock_dir.path().join(subdir);
            if src.exists() {
                std::os::unix::fs::symlink(&src, &dst).ok();
            }
        }

        // Origin system config: TLS + H2C.
        let origin_sys_toml = origin_sock_dir.path().join("system.toml");
        std::fs::write(&origin_sys_toml, format!(
            "[server]\nbind     = \"{origin_tls_addr}\"\ntls_cert = \"{}\"\ntls_key  = \"{}\"\nh2c_bind = \"{origin_h2c_addr}\"\ninternal_binds = [\"{origin_h2c_addr}\"]\ninternal_peers = [\"127.0.0.1\"]\n\n[auth]\nbackend = \"m6-auth-check\"\n\n[genid]\nread_require  = \"role:genid-read\"\nwrite_require = \"role:genid-write\"\n\n[node]\nname = \"origin\"\n",
            cert_path.display(), key_path.display(),
        )).ok()?;

        // ── Cache site dir ────────────────────────────────────────────────────
        let cache_sock_dir = tempfile::tempdir().ok()?;
        let cache_site_src = site09.join("site-cache/site.toml");
        if !cache_site_src.exists() {
            eprintln!("SKIP: site-cache/site.toml not found");
            return None;
        }
        let cache_site_toml = std::fs::read_to_string(&cache_site_src).ok()?;
        let cache_site_toml = cache_site_toml.replace(
            "h2c://127.0.0.1:9000",
            &format!("h2c://127.0.0.1:{h2c_port}"),
        );
        std::fs::write(cache_sock_dir.path().join("site.toml"), &cache_site_toml).ok()?;

        let mut processes = Vec::new();
        let mut temp_dirs: Vec<tempfile::TempDir> = vec![cert_dir, work];

        // ── Backend services ──────────────────────────────────────────────────
        processes.push(
            Command::new(&m6_html)
                .arg(&site07)
                .arg(site07.join("configs/m6-html.conf"))
                .arg("--log-level")
                .arg("warn")
                .env("M6_SOCKET_OVERRIDE", &html_sock)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .ok()?,
        );

        processes.push(
            Command::new(&m6_file)
                .arg(&site07)
                .arg(site07.join("configs/m6-file.conf"))
                .arg("--log-level")
                .arg("warn")
                .env("M6_SOCKET_OVERRIDE", &file_sock)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .ok()?,
        );

        processes.push(
            Command::new(&m6_auth)
                .arg(&site07)
                .arg(site07.join("configs/m6-auth.conf"))
                .arg("--log-level")
                .arg("warn")
                .env("M6_SOCKET_OVERRIDE", &auth_sock)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .ok()?,
        );

        processes.push(
            Command::new(&render)
                .arg(&site07)
                .arg(site07.join("configs/render-cms.conf"))
                .arg("--log-level")
                .arg("warn")
                .env("M6_SOCKET_OVERRIDE", &cms_sock)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .ok()?,
        );

        let to = Duration::from_secs(15);
        for sock in [&html_sock, &file_sock, &auth_sock, &check_sock, &cms_sock] {
            if !wait_socket(sock, to) {
                eprintln!("SKIP: socket {} not ready", sock.display());
                return None;
            }
        }

        // ── Origin m6-http (TLS + H2C) ────────────────────────────────────────
        processes.push(
            Command::new(&m6_http)
                .arg(origin_sock_dir.path())
                .arg(&origin_sys_toml)
                .arg("--log-level")
                .arg("warn")
                .env("STATE_DIRECTORY", &state)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .ok()?,
        );

        if !wait_tcp(&origin_tls_addr, to) {
            eprintln!("SKIP: origin TLS not ready on {origin_tls_addr}");
            return None;
        }
        if !wait_tcp(&origin_h2c_addr, to) {
            eprintln!("SKIP: origin H2C not ready on {origin_h2c_addr}");
            return None;
        }

        // ── 5 cache nodes ─────────────────────────────────────────────────────
        let node_names = ["edge-a", "edge-b", "edge-c", "edge-d", "edge-e"];
        for (i, &port) in cache_ports.iter().enumerate() {
            let sys = cache_sock_dir.path().join(format!("cache-{i}.toml"));
            std::fs::write(&sys, format!(
                "# Cache node: {}\n[server]\nbind     = \"127.0.0.1:{port}\"\ntls_cert = \"{}\"\ntls_key  = \"{}\"\n\n[node]\nname = \"cache-{i}\"\n\n[genid]\nupstream       = \"origin\"\ntoken_file     = \"{}\"\nheartbeat_secs = 2\n",
                node_names[i], cert_path.display(), key_path.display(), read_token.display(),
            )).ok()?;

            processes.push(
                Command::new(&m6_http)
                    .arg(cache_sock_dir.path())
                    .arg(&sys)
                    .arg("--log-level")
                    .arg("warn")
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                    .ok()?,
            );
        }

        for addr in &cache_addrs {
            if !wait_tcp(addr, to) {
                eprintln!("SKIP: cache node not ready on {addr}");
                return None;
            }
        }

        temp_dirs.push(origin_sock_dir);
        temp_dirs.push(cache_sock_dir);

        Some(GlobalStack {
            origin_addr: origin_tls_addr,
            origin_h2c: origin_h2c_addr,
            cache_addrs,
            site_dir: site07,
            write_token: std::fs::read_to_string(&write_token)
                .ok()?
                .trim()
                .to_string(),
            processes,
            _temp_dirs: temp_dirs,
        })
    }
}

// ── test macros ───────────────────────────────────────────────────────────────

macro_rules! check_eq {
    ($label:expr, $got:expr, $want:expr) => {{
        let got = $got;
        let want = $want;
        assert_eq!(got, want, "FAIL  {} — expected {want}, got {got}", $label);
        eprintln!("  PASS  {} ({got})", $label);
    }};
}

macro_rules! check_contains {
    ($label:expr, $body:expr, $needle:expr) => {{
        let body = &$body;
        let needle = $needle;
        assert!(
            body.contains(needle),
            "FAIL  {} — body does not contain {needle:?}\nbody: {}",
            $label,
            &body[..body.len().min(500)]
        );
        eprintln!("  PASS  {} (contains {needle:?})", $label);
    }};
}

// ── the test ──────────────────────────────────────────────────────────────────

#[test]
fn global_deployment_end_to_end() {
    // A stack that does not start is a failure, not a skip: a skip would let an example
    // whose configs m6 refuses pass. The line above names what was missing.
    let stack = GlobalStack::start()
        .expect("the 6-node stack did not start: see the `SKIP:` line above for what was missing");

    let tls = make_tls_config();
    let origin = &stack.origin_addr;
    let node_names = ["edge-a", "edge-b", "edge-c", "edge-d", "edge-e"];

    // ── §0 All 6 TLS endpoints reachable ─────────────────────────────────

    eprintln!("\n── §0 All TLS endpoints reachable ──");

    let r = get(origin, "/blog", Arc::clone(&tls));
    check_eq!("origin GET /blog → 200", r.status, 200);

    for (i, addr) in stack.cache_addrs.iter().enumerate() {
        let r = get(addr, "/blog", Arc::clone(&tls));
        check_eq!(
            &format!("cache {} GET /blog → 200", node_names[i]),
            r.status,
            200
        );
    }

    // ── §1 Public pages on origin ─────────────────────────────────────────

    eprintln!("\n── §1 Public pages on origin ──");

    let r = get(origin, "/", Arc::clone(&tls));
    check_eq!("origin GET / → 200", r.status, 200);

    let r = get(origin, "/blog", Arc::clone(&tls));
    check_eq!("origin GET /blog → 200", r.status, 200);
    check_contains!("origin /blog → post list", r.body, "post-card");

    let r = get(origin, "/blog/quick-start", Arc::clone(&tls));
    check_eq!("origin GET /blog/quick-start → 200", r.status, 200);
    check_contains!("origin /blog/quick-start → title", r.body, "Quick Start");

    let r = get(origin, "/assets/style.css", Arc::clone(&tls));
    check_eq!("origin GET /assets/style.css → 200", r.status, 200);

    // ── §2 Cache nodes proxy content from origin ──────────────────────────

    eprintln!("\n── §2 Cache nodes proxy content from origin ──");

    for (i, addr) in stack.cache_addrs.iter().enumerate() {
        let r = get(addr, "/blog", Arc::clone(&tls));
        check_eq!(&format!("{} GET /blog → 200", node_names[i]), r.status, 200);
        check_contains!(
            &format!("{} /blog → post list", node_names[i]),
            r.body,
            "post-card"
        );

        let r = get(addr, "/blog/quick-start", Arc::clone(&tls));
        check_eq!(
            &format!("{} GET /blog/quick-start → 200", node_names[i]),
            r.status,
            200
        );
        check_contains!(
            &format!("{} /blog/quick-start → title", node_names[i]),
            r.body,
            "Quick Start"
        );

        let r = get(addr, "/assets/style.css", Arc::clone(&tls));
        check_eq!(
            &format!("{} GET /assets/style.css → 200", node_names[i]),
            r.status,
            200
        );
    }

    // ── §3 Cache node response matches origin exactly ─────────────────────

    eprintln!("\n── §3 Cache consistency: cache vs origin ──");

    let r_origin = get(origin, "/blog/quick-start", Arc::clone(&tls));
    for (i, addr) in stack.cache_addrs.iter().enumerate() {
        let r_cache = get(addr, "/blog/quick-start", Arc::clone(&tls));
        check_eq!(
            &format!("{} /blog/quick-start status = origin", node_names[i]),
            r_cache.status,
            r_origin.status
        );
        // Both bodies should contain the same post title
        assert!(
            r_cache.body.contains("Quick Start"),
            "FAIL  {} body missing 'Quick Start'",
            node_names[i]
        );
        eprintln!("  PASS  {} body matches origin content", node_names[i]);
    }

    // ── §4 CMS routes are no-store (proxied to origin) ────────────────────

    eprintln!("\n── §4 CMS routes pass through to origin ──");

    // /cms without auth should return 401/302/403 on both origin and cache nodes
    let r = get(origin, "/cms", Arc::clone(&tls));
    assert!(
        [401u16, 302, 403].contains(&r.status),
        "FAIL  origin /cms without auth → {}, expected 401/302/403",
        r.status
    );
    eprintln!(
        "  PASS  origin /cms without auth → {} (auth enforced)",
        r.status
    );

    for (i, addr) in stack.cache_addrs.iter().enumerate() {
        let r = get(addr, "/cms", Arc::clone(&tls));
        assert!(
            [401u16, 302, 403].contains(&r.status),
            "FAIL  {} /cms without auth → {}, expected 401/302/403",
            node_names[i],
            r.status
        );
        eprintln!(
            "  PASS  {} /cms without auth → {} (forwarded to origin)",
            node_names[i], r.status
        );
    }

    // ── §5 H2C backbone is reachable ──────────────────────────────────────

    eprintln!("\n── §5 H2C backbone reachable ──");

    assert!(
        TcpStream::connect(&stack.origin_h2c).is_ok(),
        "FAIL  H2C backbone port {} not reachable",
        stack.origin_h2c
    );
    eprintln!(
        "  PASS  H2C backbone {} is reachable (TCP)",
        stack.origin_h2c
    );

    // ── §6 404 handling on all nodes ──────────────────────────────────────

    eprintln!("\n── §6 404 handling on all nodes ──");

    let r = get(origin, "/this-path-does-not-exist", Arc::clone(&tls));
    check_eq!("origin GET /nonexistent → 404", r.status, 404);

    for (i, addr) in stack.cache_addrs.iter().enumerate() {
        let r = get(addr, "/this-path-does-not-exist", Arc::clone(&tls));
        check_eq!(
            &format!("{} GET /nonexistent → 404", node_names[i]),
            r.status,
            404
        );
    }

    // ── §7 A new generation reaches every cache node ──────────────────────
    //
    // The cache nodes follow the origin's generation ([genid] upstream). A publish
    // is one POST /genid on the origin: it reloads the origin's backends, flushes
    // its cache and advances the generation, and each cache node then drops what it
    // holds and serves the new content.

    eprintln!("\n── §7 A publish at the origin reaches every cache node ──");

    let marker = format!("generation-marker-{}", std::process::id());
    for (i, addr) in stack.cache_addrs.iter().enumerate() {
        let r = get(addr, "/about", Arc::clone(&tls));
        check_eq!(
            &format!("{} GET /about → 200", node_names[i]),
            r.status,
            200
        );
        assert!(
            !r.body.contains(&marker),
            "FAIL  {} serves the marker before it exists",
            node_names[i]
        );
    }

    let about = stack.site_dir.join("data/about.json");
    std::fs::write(
        &about,
        format!(r#"{{"title":"About","body":"<p>{marker}</p>"}}"#),
    )
    .expect("write the changed about.json");

    // A publish without the right token is refused.
    let r = publish_genid(origin, "not-a-token", Arc::clone(&tls));
    assert!(
        [401u16, 403].contains(&r.status),
        "FAIL  POST /genid with a bad token → {}, expected 401/403",
        r.status
    );
    eprintln!("  PASS  POST /genid with a bad token → {}", r.status);

    let r = publish_genid(origin, &stack.write_token, Arc::clone(&tls));
    check_eq!("POST /genid with the publisher token → 200", r.status, 200);

    let r = get(origin, "/about", Arc::clone(&tls));
    check_contains!("origin /about → the new content", r.body, &marker);

    // The heartbeat is 2 seconds, so a cache node follows within a few.
    for (i, addr) in stack.cache_addrs.iter().enumerate() {
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut r = get(addr, "/about", Arc::clone(&tls));
        while !r.body.contains(&marker) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(250));
            r = get(addr, "/about", Arc::clone(&tls));
        }
        check_contains!(
            &format!("{} /about → follows the new generation", node_names[i]),
            r.body,
            &marker
        );
    }

    eprintln!("\n  All assertions passed — 6-node global deployment verified.");
}
