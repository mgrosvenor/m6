//! End-to-end security regression tests — real `m6-http` + `m6-file`
//! processes, real TLS, real HTTP/1.1 and HTTP/3 clients.
//!
//! `security_regressions.rs` proves each flaw at the level of the function
//! that contains it. This file proves the consequence: what an unauthenticated
//! attacker actually receives over the network.
//!
//! Each test asserts the **secure** behaviour for one finding. Each failed
//! when written and passes now that the finding is fixed.
//!
//! These spawn real processes and bind real ports, so run them serially and
//! against freshly built release binaries:
//!
//! ```text
//! cargo build --workspace --release
//! cargo test -p m6-http --test security_e2e -- --test-threads=1
//! ```

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rustls::StreamOwned;

// ── Process management ────────────────────────────────────────────────────────

struct TestProcess(Child);
impl Drop for TestProcess {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).parent().unwrap().to_path_buf()
}

fn binary(name: &str) -> PathBuf {
    let p = repo_root().join("target").join("release").join(name);
    assert!(
        p.exists(),
        "missing {}: run `cargo build --workspace --release` first",
        p.display()
    );
    p
}

/// Pick a free TCP port by binding to :0 and releasing it.
fn free_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().port()
}

fn wait_for_tcp(port: u16, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

fn wait_for_path(p: &Path, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if p.exists() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

// ── Crypto fixtures ───────────────────────────────────────────────────────────

/// Self-signed TLS cert for 127.0.0.1/localhost.
fn generate_tls_cert() -> (String, String, Vec<u8>) {
    let ck = rcgen::generate_simple_self_signed(vec![
        "localhost".to_string(),
        "127.0.0.1".to_string(),
    ])
    .expect("rcgen");
    let der = ck.cert.der().to_vec();
    (ck.cert.pem(), ck.key_pair.serialize_pem(), der)
}

/// ES256 keypair for JWT signing. Returns (private_pem, public_pem).
fn generate_jwt_keypair() -> (String, String) {
    let kp = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).expect("ec keypair");
    (kp.serialize_pem(), kp.public_key_pem())
}

/// Mint an ES256 JWT carrying `groups`, valid for one hour.
fn mint_jwt(private_pem: &str, sub: &str, groups: &[&str]) -> String {
    #[derive(serde::Serialize)]
    struct Claims {
        sub: String,
        iss: String,
        exp: u64,
        groups: Vec<String>,
        roles: Vec<String>,
    }
    let exp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 3600;
    let claims = Claims {
        sub: sub.to_string(),
        iss: "m6-auth".to_string(),
        exp,
        groups: groups.iter().map(|s| s.to_string()).collect(),
        roles: vec![],
    };
    let key = jsonwebtoken::EncodingKey::from_ec_pem(private_pem.as_bytes()).expect("ec key");
    let header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::ES256);
    jsonwebtoken::encode(&header, &claims, &key).expect("sign jwt")
}

// ── HTTP/1.1 client ───────────────────────────────────────────────────────────

fn tls_client_config(cert_der: &[u8]) -> Arc<rustls::ClientConfig> {
    rustls::crypto::ring::default_provider().install_default().ok();
    let cert = rustls::pki_types::CertificateDer::from(cert_der.to_vec());
    let mut store = rustls::RootCertStore::empty();
    store.add(cert).unwrap();
    Arc::new(
        rustls::ClientConfig::builder()
            .with_root_certificates(store)
            .with_no_client_auth(),
    )
}

struct HttpResponse {
    status: u16,
    headers: String,
    #[allow(dead_code)]
    body: Vec<u8>,
}

impl HttpResponse {
    /// Case-insensitive header lookup over the raw header block.
    fn has_header(&self, name: &str) -> bool {
        let want = format!("{}:", name.to_ascii_lowercase());
        self.headers
            .split("\r\n")
            .skip(1)
            .any(|l| l.to_ascii_lowercase().starts_with(&want))
    }
}

/// One HTTP/1.1-over-TLS GET. A fresh connection each call (the server sends
/// `Connection: close`).
fn https_get(
    port: u16,
    path: &str,
    extra: &[(&str, &str)],
    tls: Arc<rustls::ClientConfig>,
) -> HttpResponse {
    let tcp = TcpStream::connect(("127.0.0.1", port)).expect("tcp connect");
    tcp.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let name = rustls::pki_types::ServerName::try_from("127.0.0.1".to_string()).unwrap();
    let conn = rustls::ClientConnection::new(tls, name).unwrap();
    let mut stream = StreamOwned::new(conn, tcp);

    let mut req = format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n");
    for (k, v) in extra {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str("Connection: close\r\n\r\n");
    stream.write_all(req.as_bytes()).unwrap();
    stream.flush().unwrap();

    let mut raw = Vec::new();
    match stream.read_to_end(&mut raw) {
        Ok(_) => {}
        // Peer closed without close_notify — normal for Connection: close.
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {}
        Err(e) => panic!("TLS read error: {e}"),
    }

    let text = String::from_utf8_lossy(&raw);
    let end = text.find("\r\n\r\n").unwrap_or(raw.len());
    let headers = text[..end].to_string();
    let status = headers
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let body = if end + 4 <= raw.len() { raw[end + 4..].to_vec() } else { Vec::new() };
    HttpResponse { status, headers, body }
}

// ── Site fixture ──────────────────────────────────────────────────────────────

struct Server {
    port: u16,
    cert_der: Vec<u8>,
    /// JWT signed for a member of `admins`.
    #[allow(dead_code)]
    admin_jwt: String,
    _dir: tempfile::TempDir,
    file: TestProcess,
    _http: TestProcess,
}

impl Server {
    fn tls(&self) -> Arc<rustls::ClientConfig> {
        tls_client_config(&self.cert_der)
    }

    /// Kill the m6-file backend. Afterwards only cache hits can be served —
    /// anything reaching the backend pool fails.
    #[allow(dead_code)]
    fn kill_backend(&mut self) {
        let _ = self.file.0.kill();
        let _ = self.file.0.wait();
        // Let m6-http notice the socket has gone (2s rescan interval).
        std::thread::sleep(Duration::from_millis(2500));
    }
}

/// Bring up `m6-file` + `m6-http` over a site with:
///   - `/public/{relpath}`  → open
///   - `/private/{relpath}` → `require = "group:admins"`
///
/// `rate_limit_per_min` is written into `site.toml` verbatim.
fn start_server(rate_limit_per_min: u32) -> Server {
    let dir = tempfile::tempdir().unwrap();
    let site = dir.path();

    let (cert_pem, key_pem, cert_der) = generate_tls_cert();
    std::fs::write(site.join("cert.pem"), &cert_pem).unwrap();
    std::fs::write(site.join("key.pem"), &key_pem).unwrap();

    let (jwt_priv, jwt_pub) = generate_jwt_keypair();
    std::fs::write(site.join("auth-public.pem"), &jwt_pub).unwrap();
    let admin_jwt = mint_jwt(&jwt_priv, "admin-user", &["admins"]);

    // Content.
    std::fs::create_dir_all(site.join("public")).unwrap();
    std::fs::create_dir_all(site.join("private")).unwrap();
    std::fs::create_dir_all(site.join("configs")).unwrap();
    std::fs::write(site.join("public/open.txt"), b"PUBLIC CONTENT").unwrap();
    std::fs::write(site.join("private/secret.txt"), b"TOP SECRET").unwrap();

    let sock = dir.path().join("m6-file-1.sock");
    let sock_glob = dir.path().join("m6-file-*.sock");

    std::fs::write(
        site.join("site.toml"),
        format!(
            r#"
[site]
name   = "sec-e2e"
domain = "localhost"

[log]
level  = "warn"
format = "text"

[errors]
mode = "internal"

[analytics]
enabled = false

[rate_limit]
enabled         = true
requests_per_min = {rate_limit_per_min}

[auth]
backend    = "m6-file"
public_key = "auth-public.pem"

[[backend]]
name    = "m6-file"
sockets = "{sock_glob}"

[[route]]
path    = "/public/{{relpath}}"
backend = "m6-file"

[[route]]
path    = "/private/{{relpath}}"
backend = "m6-file"
require = "group:admins"
"#,
            sock_glob = sock_glob.display(),
        ),
    )
    .unwrap();

    std::fs::write(
        site.join("configs/m6-file.conf"),
        r#"
[[route]]
path = "/public/{relpath}"
root = "public/"

[[route]]
path = "/private/{relpath}"
root = "private/"
"#,
    )
    .unwrap();

    let port = free_port();
    std::fs::write(
        site.join("system.toml"),
        format!(
            r#"
[server]
bind     = "127.0.0.1:{port}"
tls_cert = "{cert}"
tls_key  = "{key}"

[node]
name = "test-node"
"#,
            cert = site.join("cert.pem").display(),
            key = site.join("key.pem").display(),
        ),
    )
    .unwrap();

    let file_proc = TestProcess(
        Command::new(binary("m6-file"))
            .arg(site)
            .arg(site.join("configs/m6-file.conf"))
            .env("M6_SOCKET_OVERRIDE", &sock)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn m6-file"),
    );
    assert!(
        wait_for_path(&sock, Duration::from_secs(10)),
        "m6-file socket never appeared at {}",
        sock.display()
    );

    let http_proc = TestProcess(
        Command::new(binary("m6-http"))
            .arg(site)
            .arg(site.join("system.toml"))
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn m6-http"),
    );
    assert!(
        wait_for_tcp(port, Duration::from_secs(10)),
        "m6-http never listened on {port}"
    );
    // m6-http discovers backend sockets by periodic rescan (2s interval).
    std::thread::sleep(Duration::from_millis(2500));

    Server {
        port,
        cert_der,
        admin_jwt,
        _dir: dir,
        file: file_proc,
        _http: http_proc,
    }
}

// ── Finding 10 (e2e): no security response headers ───────────────────────────

/// m6-http never adds HSTS, `X-Content-Type-Options`, frame protection, or a
/// CSP to any response.
///
/// Property: an internet-facing TLS proxy should set these centrally. Trim the
/// list to whatever policy you settle on — this is the one finding whose
/// "correct" set is a judgement call rather than a fixed requirement.
#[test]
fn finding_10_e2e_security_headers_must_be_present() {
    let srv = start_server(100_000);
    let resp = https_get(srv.port, "/public/open.txt", &[], srv.tls());
    assert_eq!(resp.status, 200, "headers:\n{}", resp.headers);

    let missing: Vec<&str> = [
        "strict-transport-security",
        "x-content-type-options",
        "x-frame-options",
        "content-security-policy",
        "referrer-policy",
    ]
    .into_iter()
    .filter(|h| !resp.has_header(h))
    .collect();

    assert!(
        missing.is_empty(),
        "missing security headers: {missing:?}\nFull header block:\n{}",
        resp.headers
    );
}
