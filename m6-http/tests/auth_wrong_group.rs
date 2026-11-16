//! End-to-end test that a protected route answers 403 to a validly signed
//! token that lacks the required group, and serves a token that has it, over
//! the real `m6-http` and `m6-file` binaries.
//!
//! The unit tests assert only the parts: `valid_jwt_wrong_group_gets_403`
//! checks that `check_require` returns false for a `["users"]` token on a
//! `group:editors` route, and `check_the_wrong_role_is_denied_with_403`
//! checks that the `Deny` decision exists for a role. Neither sends a
//! wrong-group token through a protected route and reads the 403 that
//! `m6-http/src/main.rs` maps `Decision::Deny` to. This file closes that gap.
//!
//! These spawn real processes and bind real ports, so run them serially and
//! against freshly built release binaries (the testkit finds the binary):
//!
//! ```text
//! cargo build --workspace --release
//! cargo test -p m6-http --test auth_wrong_group -- --test-threads=1
//! ```

#![allow(
    clippy::disallowed_methods,
    reason = "test code: threads, sleeps and blocking clients drive the service from outside"
)]

use std::io::{Read, Write};
use std::net::TcpStream;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use rustls::StreamOwned;

use m6_core::testkit::{binary, claim_port, PortClaim, Service};

// ── Crypto fixtures ───────────────────────────────────────────────────────────

/// Self-signed TLS cert for 127.0.0.1/localhost.
fn generate_tls_cert() -> (String, String, Vec<u8>) {
    let ck =
        rcgen::generate_simple_self_signed(vec!["localhost".to_string(), "127.0.0.1".to_string()])
            .expect("rcgen");
    let der = ck.cert.der().to_vec();
    (ck.cert.pem(), ck.key_pair.serialize_pem(), der)
}

/// ES256 keypair for JWT signing. Returns `(private_pem, public_pem)`.
fn generate_jwt_keypair() -> (String, String) {
    let kp = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).expect("ec keypair");
    (kp.serialize_pem(), kp.public_key_pem())
}

/// Mint an ES256 JWT carrying `groups`, valid for one hour.
fn mint_jwt(private_pem: &str, groups: &[&str]) -> String {
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
        sub: "t".to_string(),
        iss: "m6-auth".to_string(),
        exp,
        groups: groups.iter().map(|s| s.to_string()).collect(),
        roles: vec![],
    };
    let key = jsonwebtoken::EncodingKey::from_ec_pem(private_pem.as_bytes()).expect("ec key");
    let header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::ES256);
    jsonwebtoken::encode(&header, &claims, &key).expect("sign jwt")
}

// ── HTTP/1.1 over TLS client ──────────────────────────────────────────────────

fn tls_client_config(cert_der: &[u8]) -> Arc<rustls::ClientConfig> {
    rustls::crypto::ring::default_provider()
        .install_default()
        .ok();
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
    body: Vec<u8>,
}

/// A response that never arrived, carrying the reason in place of headers.
fn no_response(reason: impl std::fmt::Display) -> HttpResponse {
    HttpResponse {
        status: 0,
        headers: format!("<no response: {reason}>"),
        body: Vec::new(),
    }
}

/// One HTTP/1.1-over-TLS GET. A fresh connection each call (the server sends
/// `Connection: close`).
fn https_get(
    srv: &Server,
    path: &str,
    extra: &[(&str, &str)],
    tls: Arc<rustls::ClientConfig>,
) -> HttpResponse {
    let port = srv.port;
    let Some(tcp) = srv.tcp() else {
        return no_response("tcp connect refused while m6-http was alive");
    };
    if let Err(e) = tcp.set_read_timeout(Some(Duration::from_secs(10))) {
        return no_response(format!("set_read_timeout: {e}"));
    }
    let name = rustls::pki_types::ServerName::try_from("127.0.0.1".to_string()).unwrap();
    let conn = match rustls::ClientConnection::new(tls, name) {
        Ok(c) => c,
        Err(e) => return no_response(format!("tls setup: {e}")),
    };
    let mut stream = StreamOwned::new(conn, tcp);

    let mut req = format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n");
    for (k, v) in extra {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str("Connection: close\r\n\r\n");
    if let Err(e) = stream.write_all(req.as_bytes()) {
        return no_response(format!("write request: {e}"));
    }
    if let Err(e) = stream.flush() {
        return no_response(format!("flush request: {e}"));
    }

    let mut raw = Vec::new();
    match stream.read_to_end(&mut raw) {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {}
        Err(e) => return no_response(format!("tls read: {e}")),
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
    let body = raw.get(end + 4..).unwrap_or_default().to_vec();
    HttpResponse {
        status,
        headers,
        body,
    }
}

// ── Site fixture ──────────────────────────────────────────────────────────────

struct Server {
    port: u16,
    cert_der: Vec<u8>,
    http: std::cell::RefCell<Service>,
    /// The m6-file backend, held so it stays alive for the test's lifetime.
    _file: Service,
    /// The signing key whose public half is `auth-public.pem` in the site.
    private_pem: String,
    _dir: tempfile::TempDir,
    _port: PortClaim,
}

impl Server {
    fn tls(&self) -> Arc<rustls::ClientConfig> {
        tls_client_config(&self.cert_der)
    }

    /// Open a TCP connection to the server, or `None` if it is not accepting.
    fn tcp(&self) -> Option<TcpStream> {
        match TcpStream::connect(("127.0.0.1", self.port)) {
            Ok(s) => Some(s),
            Err(_) => {
                self.http
                    .borrow_mut()
                    .assert_alive("the client was connecting");
                None
            }
        }
    }

    /// Wait until a request actually reaches the backend.
    fn wait_until_serving(&self) {
        let ready = m6_core::testkit::wait::until(m6_core::testkit::STARTUP_BOUND, || {
            https_get(self, "/public/probe.txt", &[], self.tls()).status == 200
        });
        assert!(
            ready,
            "m6-http never served a backend request\n--- output ---\n{}",
            self.http.borrow().output()
        );
    }
}

/// Bring up `m6-file` + `m6-http` over a site with:
///   - `/public/{relpath}`   → open, the readiness probe
///   - `/editors/{relpath}`  → `require = "group:editors"`
fn start_server() -> Server {
    let dir = tempfile::tempdir().unwrap();
    let site = dir.path();

    let (cert_pem, key_pem, cert_der) = generate_tls_cert();
    std::fs::write(site.join("cert.pem"), &cert_pem).unwrap();
    std::fs::write(site.join("key.pem"), &key_pem).unwrap();

    let (private_pem, public_pem) = generate_jwt_keypair();
    std::fs::write(site.join("auth-public.pem"), &public_pem).unwrap();

    std::fs::create_dir_all(site.join("public")).unwrap();
    std::fs::create_dir_all(site.join("editors")).unwrap();
    std::fs::create_dir_all(site.join("configs")).unwrap();
    std::fs::write(site.join("public/probe.txt"), b"PROBE").unwrap();
    std::fs::write(site.join("editors/draft.txt"), b"DRAFT").unwrap();

    let sock = dir.path().join("m6-file-1.sock");
    let sock_glob = dir.path().join("m6-file-*.sock");

    std::fs::write(
        site.join("site.toml"),
        format!(
            r#"
[site]
name   = "auth-wrong-group"
domain = "localhost"

[log]
level  = "debug"

[errors]
mode = "internal"

[analytics]
enabled = false

[[backend]]
name    = "m6-file"
sockets = "{sock_glob}"

[[route]]
path    = "/public/{{relpath}}"
backend = "m6-file"

[[route]]
path    = "/editors/{{relpath}}"
backend = "m6-file"
require = "group:editors"
"#,
            sock_glob = sock_glob.display(),
        ),
    )
    .unwrap();

    std::fs::write(
        site.join("configs/m6-file.conf"),
        r#"
[[route]]
path = "/public/{*relpath}"
handler = "files"
root = "public/"

[[route]]
path = "/editors/{*relpath}"
handler = "files"
root = "editors/"
"#,
    )
    .unwrap();

    let claim = claim_port();
    let port = claim.port();
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

[auth]
backend    = "m6-file"
public_key = "{pubkey}"
"#,
            cert = site.join("cert.pem").display(),
            key = site.join("key.pem").display(),
            pubkey = site.join("auth-public.pem").display(),
        ),
    )
    .unwrap();

    let mut file_proc = Service::spawn(
        "m6-file",
        Command::new(binary("m6-file"))
            .arg(site)
            .arg(site.join("configs/m6-file.conf"))
            .env("M6_SOCKET_OVERRIDE", &sock),
    );
    file_proc.wait_for_path(&sock, m6_core::testkit::STARTUP_BOUND);

    let mut http_proc = Service::spawn(
        "m6-http",
        Command::new(binary("m6-http"))
            .arg(site)
            .arg(site.join("system.toml")),
    );
    http_proc.wait_for_tcp(port, m6_core::testkit::STARTUP_BOUND);

    let srv = Server {
        port,
        cert_der,
        http: std::cell::RefCell::new(http_proc),
        _file: file_proc,
        private_pem,
        _dir: dir,
        _port: claim,
    };
    srv.wait_until_serving();
    srv
}

// ── #333: a wrong-group token is answered 403 ─────────────────────────────────

/// A protected route with `require = "group:editors"` answers 403 to a validly
/// signed token whose groups are only `["users"]`. The wrong outcome it
/// catches: the `Refusal::Insufficient` branch answering anything but 403, so
/// a token without the required group is served or mislabelled.
#[test]
fn valid_token_without_required_group_gets_403() {
    let srv = start_server();
    let token = mint_jwt(&srv.private_pem, &["users"]);
    let bearer = format!("Bearer {token}");

    let resp = https_get(
        &srv,
        "/editors/draft.txt",
        &[("Authorization", bearer.as_str())],
        srv.tls(),
    );
    assert_eq!(
        resp.status, 403,
        "a valid token without the editors group must be answered 403, got {}\nheaders:\n{}",
        resp.status, resp.headers
    );
}

/// A token whose groups include the required group is served, so the 403 test
/// above does not pass because every request is refused. The wrong outcome it
/// catches: the protected route refusing all requests, which would make the
/// 403 assertion vacuous.
#[test]
fn valid_token_with_required_group_is_served() {
    let srv = start_server();
    let token = mint_jwt(&srv.private_pem, &["editors", "users"]);
    let bearer = format!("Bearer {token}");

    let resp = https_get(
        &srv,
        "/editors/draft.txt",
        &[("Authorization", bearer.as_str())],
        srv.tls(),
    );
    assert_eq!(
        resp.status, 200,
        "a token with the editors group must be served, got {}\nheaders:\n{}",
        resp.status, resp.headers
    );
    assert_eq!(
        String::from_utf8_lossy(&resp.body),
        "DRAFT",
        "the served response must be the backend file, not an empty or error body"
    );
}
