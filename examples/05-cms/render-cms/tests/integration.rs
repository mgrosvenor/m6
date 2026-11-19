//! End-to-end integration test for the example-05 CMS blog site.
//!
//! Spins up the full server stack (m6-html, m6-file, m6-auth-server,
//! render-cms, m6-http) in child processes, exercises every public page,
//! the contact form, auth, all CMS API endpoints, and the full
//! draft → publish → unpublish lifecycle, then tears everything down.
//!
//! # Prerequisites
//!
//! The m6 release binaries must be pre-built:
//!   cargo build --release   (in the m6/ workspace)
//!
//! The test needs nothing else. It copies the site into a temporary directory
//! and makes there its own self-signed certificate, its own auth key pair and
//! auth database (with an `admin` user in the `editors` group and a publisher
//! token), and a system config with a `[genid]` table. It starts every
//! service against that copy, so it neither reads nor changes the development
//! directory (`M6_DEV_HOME`) or the checked-in site. If a binary is absent the test fails and names the command
//! that fixes it.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rustls::pki_types::ServerName;

// ── locate binaries ───────────────────────────────────────────────────────────

fn m6_root() -> PathBuf {
    // render-cms/  →  05-cms/  →  examples/  →  m6-examples/  →  m6/
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
    // Fall back to PATH
    which(name)
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

fn render_cms_bin() -> PathBuf {
    // Cargo sets this env var to the path of the compiled binary.
    PathBuf::from(env!("CARGO_BIN_EXE_render-cms"))
}

fn site_dir() -> PathBuf {
    // render-cms/  →  05-cms/
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .canonicalize()
        .expect("site dir")
}

// ── TLS: no-verify client config ─────────────────────────────────────────────

#[derive(Debug)]
struct NoVerify;

impl rustls::client::danger::ServerCertVerifier for NoVerify {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        _msg: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }
    fn verify_tls13_signature(
        &self,
        _msg: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
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

// ── HTTP/1.1 over TLS helpers ─────────────────────────────────────────────────

struct Response {
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
}

impl Response {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
    fn set_cookies(&self) -> Vec<String> {
        self.headers
            .iter()
            .filter(|(k, _)| k.eq_ignore_ascii_case("set-cookie"))
            .map(|(_, v)| v.clone())
            .collect()
    }
}

fn https_request(
    addr: &str,
    method: &str,
    path: &str,
    extra_headers: &[(&str, &str)],
    body: &[u8],
    tls_cfg: Arc<rustls::ClientConfig>,
) -> Result<Response, String> {
    let tcp = TcpStream::connect(addr).map_err(|e| format!("connect {addr}: {e}"))?;
    tcp.set_read_timeout(Some(Duration::from_secs(10))).ok();
    tcp.set_write_timeout(Some(Duration::from_secs(10))).ok();

    let server_name =
        ServerName::try_from("localhost".to_string()).map_err(|e| format!("server name: {e}"))?;
    let conn = rustls::ClientConnection::new(tls_cfg, server_name)
        .map_err(|e| format!("TLS init: {e}"))?;
    let mut stream = rustls::StreamOwned::new(conn, tcp);

    // Build request
    let mut req = format!("{method} {path} HTTP/1.1\r\nHost: localhost\r\n");
    for (k, v) in extra_headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    if !body.is_empty() {
        req.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    req.push_str("Connection: close\r\n\r\n");

    stream
        .write_all(req.as_bytes())
        .map_err(|e| format!("write: {e}"))?;
    if !body.is_empty() {
        stream
            .write_all(body)
            .map_err(|e| format!("write body: {e}"))?;
    }
    stream.flush().map_err(|e| format!("flush: {e}"))?;

    // Read response
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).ok(); // EOF is normal with Connection: close

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
/// RFC 9110 15.2 requires a client to parse and discard any number of 1xx
/// responses before the final one. `m6-http` sends `103 Early Hints` ahead of
/// the real response on any route carrying preload hints, so the parser skips
/// every 1xx block and reads the status and headers of the final response.
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
        // step over it. `after < s.len()` stops this looping on a truncated
        // response that ends with an informational one.
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
    }

    #[test]
    fn early_hints_are_skipped_and_the_real_status_is_read() {
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
    fn set_cookie_on_the_final_response_survives_the_skip() {
        // This suite logs in, so the header it cares about most must come from
        // the real response rather than being lost with the discarded 103.
        let raw = "HTTP/1.1 103 Early Hints\r\n\
                   Link: </a.css>; rel=preload\r\n\
                   \r\n\
                   HTTP/1.1 302 Found\r\n\
                   set-cookie: session=abc; HttpOnly\r\n\
                   location: /cms\r\n\
                   \r\n";
        let r = parse_response(raw);
        assert_eq!(r.status, 302);
        assert!(
            r.headers
                .iter()
                .any(|(k, v)| k == "set-cookie" && v.contains("session=abc")),
            "headers: {:?}",
            r.headers
        );
    }

    #[test]
    fn a_truncated_informational_response_does_not_loop() {
        let r = parse_response("HTTP/1.1 103 Early Hints\r\nLink: </a.css>\r\n\r\n");
        assert_eq!(r.status, 103);
    }

    #[test]
    fn garbage_does_not_panic() {
        assert_eq!(parse_response("").status, 0);
        assert_eq!(parse_response("not http at all").status, 0);
    }
}

fn get_with_cookie_str(
    addr: &str,
    path: &str,
    cookie_str: &str,
    tls: Arc<rustls::ClientConfig>,
) -> Response {
    let extra: &[(&str, &str)] = if cookie_str.is_empty() {
        &[]
    } else {
        &[("Cookie", cookie_str)]
    };
    https_request(addr, "GET", path, extra, &[], tls).unwrap_or_else(|e| panic!("GET {path}: {e}"))
}

fn post(
    addr: &str,
    path: &str,
    content_type: &str,
    body: &str,
    cookie_str: &str,
    tls: Arc<rustls::ClientConfig>,
) -> Response {
    let mut extra = vec![("Content-Type", content_type)];
    if !cookie_str.is_empty() {
        extra.push(("Cookie", cookie_str));
    }
    https_request(addr, "POST", path, &extra, body.as_bytes(), tls)
        .unwrap_or_else(|e| panic!("POST {path}: {e}"))
}

fn patch(
    addr: &str,
    path: &str,
    body: &str,
    cookie_str: &str,
    tls: Arc<rustls::ClientConfig>,
) -> Response {
    let mut extra = vec![("Content-Type", "application/json")];
    if !cookie_str.is_empty() {
        extra.push(("Cookie", cookie_str));
    }
    https_request(addr, "PATCH", path, &extra, body.as_bytes(), tls)
        .unwrap_or_else(|e| panic!("PATCH {path}: {e}"))
}

fn build_cookie_header(jar: &HashMap<String, String>) -> String {
    jar.iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("; ")
}

/// Extract all Set-Cookie name=value pairs from a response into a jar.
fn harvest_cookies(resp: &Response, jar: &mut HashMap<String, String>) {
    for sc in resp.set_cookies() {
        // Set-Cookie: name=value; Path=/; HttpOnly; ...
        if let Some(pair) = sc.split(';').next() {
            if let Some(eq) = pair.find('=') {
                let name = pair[..eq].trim().to_string();
                let value = pair[eq + 1..].trim().to_string();
                jar.insert(name, value);
            }
        }
    }
}

// ── process management ────────────────────────────────────────────────────────

struct ServerStack {
    addr: String,
    _work: tempfile::TempDir, // kept alive so the temp dir is not deleted while running
    processes: Vec<Child>,
}

impl Drop for ServerStack {
    fn drop(&mut self) {
        for c in &mut self.processes {
            c.kill().ok();
            c.wait().ok();
        }
    }
}

fn wait_for_tcp(addr: &str, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if TcpStream::connect(addr).is_ok() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

fn wait_for_socket(path: &Path, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
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
        if matches!(name.to_str(), Some("render-cms" | "logs" | "keys")) {
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

/// Run an m6-auth-cli command against the test's own auth config and return what
/// it printed on stdout.
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

impl ServerStack {
    /// Start the full server stack on a copy of the site, with a certificate,
    /// auth keys, auth database and publisher token made here. Panics, naming
    /// the command that fixes it, when a binary is missing or a service does
    /// not come up. Children already started are killed on the way out,
    /// because they live in the returned stack's `Drop`.
    fn start() -> Self {
        // Require all m6 binaries.
        let missing = "Run `cargo build --release` in the m6 workspace";
        let m6_http = m6_bin("m6-http").unwrap_or_else(|| panic!("m6-http not found. {missing}"));
        let m6_html = m6_bin("m6-html").unwrap_or_else(|| panic!("m6-html not found. {missing}"));
        let m6_file = m6_bin("m6-file").unwrap_or_else(|| panic!("m6-file not found. {missing}"));
        let m6_auth = m6_bin("m6-auth-server")
            .unwrap_or_else(|| panic!("m6-auth-server not found. {missing}"));
        let m6_cli =
            m6_bin("m6-auth-cli").unwrap_or_else(|| panic!("m6-auth-cli not found. {missing}"));
        let render_cms = render_cms_bin();
        assert!(
            render_cms.exists(),
            "render-cms binary not found at {}. Run `cargo build -p render-cms`",
            render_cms.display()
        );

        // Use a unique port per process ID to avoid collisions with dev server.
        let base_port: u16 = 20000 + (std::process::id() as u16 % 10000);
        let tls_addr = format!("127.0.0.1:{base_port}");

        let work = tempfile::tempdir()
            .expect("cannot create a temporary directory. Check that the temp dir is writable");
        let w = work.path().to_path_buf();
        let site = w.join("site");
        copy_site(&site_dir(), &site);
        let sock = |name: &str| w.join(name);
        let (html_sock, file_sock, auth_sock, cms_sock) = (
            sock("m6-html.sock"),
            sock("m6-file.sock"),
            sock("m6-auth.sock"),
            sock("render-cms.sock"),
        );
        // The check socket is the login socket's name with a `check-` prefix.
        let check_sock = sock("check-m6-auth.sock");

        // The certificate m6-http serves, self-signed for localhost. The same
        // file is what render-cms trusts when it sends `POST /genid`.
        let cert = rcgen::generate_simple_self_signed(vec![
            "localhost".to_string(),
            "127.0.0.1".to_string(),
        ])
        .expect("generate a self-signed certificate");
        let keys = w.join("keys");
        std::fs::create_dir_all(&keys).expect("create the key directory");
        let (tls_cert, tls_key) = (keys.join("dev.pem"), keys.join("dev-key.pem"));
        std::fs::write(&tls_cert, cert.cert.pem()).expect("write the certificate");
        std::fs::write(&tls_key, cert.key_pair.serialize_pem()).expect("write the key");

        // The auth signing key pair: PKCS#8 P-256, as m6-auth-server reads it.
        let signing = rcgen::KeyPair::generate().expect("generate the auth key pair");
        let (auth_pem, auth_pub) = (keys.join("auth.pem"), keys.join("auth.pub"));
        std::fs::write(&auth_pem, signing.serialize_pem()).expect("write the auth key");
        std::fs::write(&auth_pub, signing.public_key_pem()).expect("write the auth public key");

        // Point the site's configs at this run's files.
        let rewrite = |file: &str, pairs: &[(&str, String)]| {
            let path = site.join(file);
            let mut text = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
            for (from, to) in pairs {
                assert!(text.contains(from), "{file} has no {from}");
                text = text.replace(from, to);
            }
            std::fs::write(&path, text).expect("write a rewritten config");
        };
        let auth_conf = site.join("configs/m6-auth.conf");
        rewrite(
            "configs/m6-auth.conf",
            &[
                (
                    "@M6_DEV_HOME@/05-cms/auth.db",
                    w.join("auth.db").display().to_string(),
                ),
                ("@M6_DEV_HOME@/dev/auth.pem", auth_pem.display().to_string()),
                ("@M6_DEV_HOME@/dev/auth.pub", auth_pub.display().to_string()),
            ],
        );
        let token_file = w.join("genid-write.token");
        rewrite(
            "configs/render-cms.conf",
            &[
                (
                    "https://localhost:8443/genid",
                    format!("https://localhost:{base_port}/genid"),
                ),
                ("127.0.0.1:8443", tls_addr.clone()),
                (
                    "@M6_DEV_HOME@/05-cms/genid-write.token",
                    token_file.display().to_string(),
                ),
                ("@M6_DEV_HOME@/dev/ca.pem", tls_cert.display().to_string()),
            ],
        );

        // The auth database: an editor to log in with, and a publisher whose
        // token render-cms sends with `POST /genid`.
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
        auth_cli(
            &m6_cli,
            &auth_conf,
            &[
                "user",
                "add",
                "publisher",
                "--role",
                "genid-write",
                "--password",
                "publisher-pw",
            ],
        );
        let token = auth_cli(
            &m6_cli,
            &auth_conf,
            &["token", "create", "publisher", "--name", "publish"],
        );
        std::fs::write(&token_file, token).expect("write the publisher token");

        // System config: the node's own bind, certificate, check backend and
        // the claims that gate `/genid`.
        let state = w.join("state");
        std::fs::create_dir_all(&state).expect("create the state directory");
        let sys_toml = w.join("system.toml");
        std::fs::write(
            &sys_toml,
            format!(
                "[server]\nbind     = \"{tls_addr}\"\ntls_cert = \"{}\"\ntls_key  = \"{}\"\n\n\
                 [auth]\nbackend = \"m6-auth-check\"\n\n\
                 [genid]\nread_require  = \"role:genid-read\"\nwrite_require = \"role:genid-write\"\n\n\
                 [node]\nname = \"test\"\n",
                tls_cert.display(),
                tls_key.display(),
            ),
        )
        .expect("cannot write system.toml in the temporary directory");

        // site.toml: the backends' sockets move into the temporary directory.
        let patched = std::fs::read_to_string(site.join("site.toml"))
            .expect("cannot read site.toml")
            .replace("/tmp/m6/m6-html*.sock", &html_sock.display().to_string())
            .replace("/tmp/m6/m6-file*.sock", &file_sock.display().to_string())
            .replace(
                "/tmp/m6/check-m6-auth*.sock",
                &check_sock.display().to_string(),
            )
            .replace("/tmp/m6/m6-auth*.sock", &auth_sock.display().to_string())
            .replace("/tmp/m6/render-cms*.sock", &cms_sock.display().to_string());
        std::fs::write(site.join("site.toml"), &patched).expect("cannot write site.toml");

        // The stack owns its children from the first spawn, so a panic below
        // kills what was already started instead of leaking it.
        let mut stack = ServerStack {
            addr: tls_addr.clone(),
            _work: work,
            processes: Vec::new(),
        };
        let spawn = |program: &Path, conf: &str, socket: &Path, extra: &[&str]| {
            Command::new(program)
                .arg(&site)
                .arg(site.join(conf))
                .args(extra)
                .arg("--log-level")
                .arg("warn")
                .env("M6_SOCKET_OVERRIDE", socket)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap_or_else(|e| panic!("cannot start {}: {e}", program.display()))
        };
        stack
            .processes
            .push(spawn(&m6_html, "configs/m6-html.conf", &html_sock, &[]));
        stack
            .processes
            .push(spawn(&m6_file, "configs/m6-file.conf", &file_sock, &[]));
        stack
            .processes
            .push(spawn(&m6_auth, "configs/m6-auth.conf", &auth_sock, &[]));
        // m6-auth-server binds the check socket (`check-m6-auth.sock`) beside the
        // login socket, so one process serves both.
        stack.processes.push(spawn(
            &render_cms,
            "configs/render-cms.conf",
            &cms_sock,
            &[],
        ));

        // Wait for all backend sockets.
        let timeout = Duration::from_secs(15);
        for socket in [&html_sock, &file_sock, &auth_sock, &check_sock, &cms_sock] {
            assert!(
                wait_for_socket(socket, timeout),
                "socket {} did not appear: a service did not start",
                socket.display()
            );
        }

        // m6-http writes the generation ID to $STATE_DIRECTORY/genid.
        stack.processes.push(
            Command::new(&m6_http)
                .arg(&site)
                .arg(&sys_toml)
                .arg("--log-level")
                .arg("warn")
                .env("STATE_DIRECTORY", &state)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("cannot start m6-http"),
        );
        assert!(
            wait_for_tcp(&tls_addr, Duration::from_secs(15)),
            "m6-http did not become ready on {tls_addr}"
        );

        stack
    }
}

// ── assertion helpers ─────────────────────────────────────────────────────────

macro_rules! check_eq {
    ($label:expr, $got:expr, $want:expr) => {{
        let got = $got;
        let want = $want;
        assert_eq!(
            got, want,
            "FAIL  {} — expected {}, got {}",
            $label, want, got
        );
        eprintln!("  PASS  {} ({})", $label, got);
    }};
}

macro_rules! check_contains {
    ($label:expr, $body:expr, $needle:expr) => {{
        let body = &$body;
        let needle = $needle;
        assert!(
            body.contains(needle),
            "FAIL  {} — body does not contain {:?}\nbody: {}",
            $label,
            needle,
            &body[..body.len().min(500)]
        );
        eprintln!("  PASS  {} (contains {:?})", $label, needle);
    }};
}

macro_rules! check_status {
    // Accept any one of a set of expected codes.
    ($label:expr, $got:expr, $($want:expr),+) => {{
        let got = $got;
        let ok = false $(|| got == $want)+;
        assert!(ok, "FAIL  {} — got {}, expected one of [{}]",
            $label, got, stringify!($($want),+));
        eprintln!("  PASS  {} ({})", $label, got);
    }};
}

// ── the test ──────────────────────────────────────────────────────────────────

#[test]
fn blog_site_end_to_end() {
    let stack = ServerStack::start();

    let addr = &stack.addr;
    let tls = make_tls_config();
    let mut cookies: HashMap<String, String> = HashMap::new();

    // ── §0 Server reachability ─────────────────────────────────────────────

    eprintln!("\n── §0 Server reachability ──");
    let r = get_with_cookie_str(addr, "/", "", Arc::clone(&tls));
    check_eq!("GET / reachable", r.status, 200);

    // ── §1 Public pages ───────────────────────────────────────────────────

    eprintln!("\n── §1 Public pages ──");

    let r = get_with_cookie_str(addr, "/", "", Arc::clone(&tls));
    check_eq!("GET / → 200", r.status, 200);
    check_contains!("GET / → site name", r.body, "m6");
    check_contains!("GET / → Recent Posts", r.body, "Recent Posts");

    let r = get_with_cookie_str(addr, "/about", "", Arc::clone(&tls));
    check_eq!("GET /about → 200", r.status, 200);

    let r = get_with_cookie_str(addr, "/blog", "", Arc::clone(&tls));
    check_eq!("GET /blog → 200", r.status, 200);
    check_contains!("GET /blog → lists posts", r.body, "quick-start");

    let r = get_with_cookie_str(addr, "/blog/quick-start", "", Arc::clone(&tls));
    check_eq!("GET /blog/quick-start → 200", r.status, 200);
    check_contains!("GET /blog/quick-start → title", r.body, "Quick Start");

    let r = get_with_cookie_str(addr, "/blog/architecture", "", Arc::clone(&tls));
    check_eq!("GET /blog/architecture → 200", r.status, 200);

    let r = get_with_cookie_str(addr, "/blog/nonexistent-post-xyz", "", Arc::clone(&tls));
    check_eq!(
        "GET /blog/nonexistent → 200 (not-found page)",
        r.status,
        200
    );
    check_contains!(
        "GET /blog/nonexistent → not-found message",
        r.body,
        "not found"
    );

    // ── §2 Static assets ──────────────────────────────────────────────────

    eprintln!("\n── §2 Static assets ──");

    let r = get_with_cookie_str(addr, "/assets/style.css", "", Arc::clone(&tls));
    check_eq!("GET /assets/style.css → 200", r.status, 200);
    let ct = r.header("content-type").unwrap_or("");
    assert!(
        ct.contains("text/css"),
        "FAIL  style.css content-type — got {:?}",
        ct
    );
    eprintln!("  PASS  style.css content-type is CSS ({ct})");

    let r = get_with_cookie_str(addr, "/assets/easymde.min.js", "", Arc::clone(&tls));
    check_eq!("GET /assets/easymde.min.js → 200", r.status, 200);

    let r = get_with_cookie_str(addr, "/assets/nonexistent.css", "", Arc::clone(&tls));
    check_eq!("GET /assets/nonexistent.css → 404", r.status, 404);

    // ── §3 Contact form ───────────────────────────────────────────────────

    eprintln!("\n── §3 Contact form ──");

    let r = get_with_cookie_str(addr, "/contact", "", Arc::clone(&tls));
    check_eq!("GET /contact → 200", r.status, 200);
    check_contains!("GET /contact → has <form>", r.body, "<form");
    check_contains!("GET /contact → has name field", r.body, "name");

    let r = post(
        addr,
        "/contact",
        "application/x-www-form-urlencoded",
        "name=Alice&email=alice%40example.com&message=Hello",
        "",
        Arc::clone(&tls),
    );
    check_eq!("POST /contact → 200", r.status, 200);

    let r = post(
        addr,
        "/contact",
        "application/x-www-form-urlencoded",
        "name=TestUser&email=test%40example.com&message=Test+message",
        "",
        Arc::clone(&tls),
    );
    check_eq!("POST /contact → 200 (name echo)", r.status, 200);
    check_contains!("POST /contact → echoes name", r.body, "TestUser");

    // ── §4 Login page ─────────────────────────────────────────────────────

    eprintln!("\n── §4 Login page ──");

    let r = get_with_cookie_str(addr, "/login", "", Arc::clone(&tls));
    check_eq!("GET /login → 200", r.status, 200);
    check_contains!("GET /login → has <form>", r.body, "<form");
    check_contains!("GET /login → has password field", r.body, "password");

    // ── §5 Auth endpoints ─────────────────────────────────────────────────

    eprintln!("\n── §5 Auth endpoints ──");

    // POST /auth/login → 302 redirect + Set-Cookie
    let r = post(
        addr,
        "/auth/login",
        "application/x-www-form-urlencoded",
        "username=admin&password=admin",
        "",
        Arc::clone(&tls),
    );
    check_status!("POST /auth/login → 302", r.status, 302u16);
    harvest_cookies(&r, &mut cookies);
    check_eq!(
        "POST /auth/login sets an auth cookie",
        !cookies.is_empty(),
        true
    );

    let cookie_str = build_cookie_header(&cookies);

    // ── §6 Protected CMS pages ────────────────────────────────────────────

    eprintln!("\n── §6 Protected CMS pages ──");

    // Without auth
    let r = get_with_cookie_str(addr, "/cms", "", Arc::clone(&tls));
    check_status!(
        "GET /cms without auth → 401/302/403",
        r.status,
        401u16,
        302u16,
        403u16
    );

    let r = get_with_cookie_str(addr, "/cms", &cookie_str, Arc::clone(&tls));
    check_eq!("GET /cms with auth → 200", r.status, 200);
    check_contains!("GET /cms → shows dashboard", r.body, "draft");

    let r = get_with_cookie_str(addr, "/cms/new", &cookie_str, Arc::clone(&tls));
    check_eq!("GET /cms/new with auth → 200", r.status, 200);
    check_contains!("GET /cms/new → has editor", r.body, "title");

    // ── §7 CMS API: draft lifecycle ───────────────────────────────────────

    eprintln!("\n── §7 CMS API: draft → publish → unpublish lifecycle ──");

    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let title = format!("Test Post {ts}");
    let draft_body = format!(
        r#"{{"title":"{title}","body":"Hello world","summary":"A test post","tags":["test"],"date":"2026-03-21"}}"#
    );

    let r = post(
        addr,
        "/api/drafts",
        "application/json",
        &draft_body,
        &cookie_str,
        Arc::clone(&tls),
    );
    check_eq!("POST /api/drafts → 201", r.status, 201);

    let stem: String = serde_json::from_str::<serde_json::Value>(&r.body)
        .ok()
        .and_then(|v| v["stem"].as_str().map(str::to_string))
        .unwrap_or_else(|| {
            panic!(
                "FAIL  could not extract stem from: {}",
                &r.body[..r.body.len().min(200)]
            )
        });
    eprintln!("  PASS  draft stem: {stem}");

    let r = get_with_cookie_str(
        addr,
        &format!("/cms/edit/{stem}"),
        &cookie_str,
        Arc::clone(&tls),
    );
    check_eq!(&format!("GET /cms/edit/{stem} → 200"), r.status, 200);

    let r = patch(
        addr,
        &format!("/api/drafts/{stem}"),
        r#"{"summary":"Updated summary"}"#,
        &cookie_str,
        Arc::clone(&tls),
    );
    check_eq!(&format!("PATCH /api/drafts/{stem} → 200"), r.status, 200);

    let r = post(
        addr,
        &format!("/api/publish/{stem}"),
        "application/json",
        "",
        &cookie_str,
        Arc::clone(&tls),
    );
    check_eq!(&format!("POST /api/publish/{stem} → 200"), r.status, 200);
    check_contains!("publish → published=true", r.body, "published");

    // The publish answered after `POST /genid` finished, so the post is live.
    let r = get_with_cookie_str(addr, &format!("/blog/{stem}"), "", Arc::clone(&tls));
    check_eq!(
        &format!("GET /blog/{stem} → 200 after publish"),
        r.status,
        200
    );
    // An unknown stem answers 200 as well, so the status alone proves nothing:
    // the page has to carry this post's own title.
    check_contains!(
        &format!("GET /blog/{stem} → shows the post's title"),
        r.body,
        &title
    );

    let r = get_with_cookie_str(addr, "/blog", "", Arc::clone(&tls));
    check_contains!("GET /blog lists the published post", r.body, &stem);

    let r = post(
        addr,
        &format!("/api/unpublish/{stem}"),
        "application/json",
        "",
        &cookie_str,
        Arc::clone(&tls),
    );
    check_eq!(&format!("POST /api/unpublish/{stem} → 200"), r.status, 200);
    check_contains!("unpublish → unpublished=true", r.body, "unpublished");

    // The unpublished post is gone from the blog, with no wait.
    let r = get_with_cookie_str(addr, "/blog", "", Arc::clone(&tls));
    assert!(
        !r.body.contains(&stem),
        "FAIL  an unpublished post is still listed on /blog"
    );
    eprintln!("  PASS  unpublished post is no longer listed");

    // ── §8 Auth: logout ───────────────────────────────────────────────────

    eprintln!("\n── §8 Auth logout ──");

    let r = post(
        addr,
        "/auth/logout",
        "application/x-www-form-urlencoded",
        "",
        &cookie_str,
        Arc::clone(&tls),
    );
    check_status!(
        "POST /auth/logout → 200/302/204",
        r.status,
        200u16,
        302u16,
        204u16
    );

    // ── §9 Error handling ─────────────────────────────────────────────────

    eprintln!("\n── §9 Error handling ──");

    let r = get_with_cookie_str(addr, "/this-path-does-not-exist", "", Arc::clone(&tls));
    check_eq!("GET /nonexistent → 404", r.status, 404);

    let r = get_with_cookie_str(
        addr,
        "/blog/this-post-does-not-exist-xyz",
        "",
        Arc::clone(&tls),
    );
    check_eq!(
        "GET /blog/nonexistent → 200 (not-found page)",
        r.status,
        200
    );
    check_contains!(
        "GET /blog/nonexistent → not-found message",
        r.body,
        "not found"
    );

    eprintln!("\n  All assertions passed.");
}
