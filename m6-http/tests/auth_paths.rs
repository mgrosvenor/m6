//! End-to-end auth path tests: real `m6-http` and `m6-file` processes, real
//! TLS, real HTTP/1.1 requests.
//!
//! Two contracts are pinned down here, both of which had tests that could not
//! catch the failure they were named for:
//!
//! - A public route runs no auth code at all, with or without a token, and
//!   even when answered from the cache. The old `public_route_no_auth_code_executed`
//!   test only checked that `extract_token` returns `None` for empty input, so
//!   it stayed green if every public request ran `auth::authorise`. Here the
//!   server runs at log level `debug`, and the tests count the `auth check`
//!   line that `handle_request` emits only on the auth path.
//! - A browser with no valid session is redirected (302 to `/login?next=<path>`
//!   or to `/auth/refresh`), while an API client gets a plain 401. The old
//!   tests asserted the `is_browser_request` and `extract_refresh_cookie`
//!   helpers, so a change that answered 401 to a browser, or dropped `next`,
//!   stayed green. Here the request and its `Location` header are asserted end
//!   to end.
//!
//! These spawn real processes and bind real ports, so run them serially and
//! against freshly built release binaries (the testkit finds the binary):
//!
//! ```text
//! cargo build --workspace --release
//! cargo test -p m6-http --test auth_paths -- --test-threads=1
//! ```

use std::io::{Read, Write};
use std::net::TcpStream;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use rustls::StreamOwned;

use m6_core::testkit::{binary, claim_port, PortClaim, Service};

/// How long to wait for a log line to reach the child's output buffer.
const LOG_DEADLINE: Duration = Duration::from_secs(10);

// ── Fixtures ──────────────────────────────────────────────────────────────────

/// Self-signed TLS cert for 127.0.0.1/localhost.
fn generate_tls_cert() -> (String, String, Vec<u8>) {
    let ck =
        rcgen::generate_simple_self_signed(vec!["localhost".to_string(), "127.0.0.1".to_string()])
            .expect("rcgen");
    let der = ck.cert.der().to_vec();
    (ck.cert.pem(), ck.key_pair.serialize_pem(), der)
}

/// A public key PEM for `[auth] public_key`. Only the verify key is needed:
/// no test here mints a token, because a public route ignores it and the
/// redirects happen before a token is verified.
fn generate_jwt_public_key() -> String {
    let kp = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).expect("ec keypair");
    kp.public_key_pem()
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
}

/// A response that never arrived, carrying the reason in place of headers.
fn no_response(reason: impl std::fmt::Display) -> HttpResponse {
    HttpResponse {
        status: 0,
        headers: format!("<no response: {reason}>"),
    }
}

impl HttpResponse {
    /// Case-insensitive header value, or `None`.
    fn header_value(&self, name: &str) -> Option<String> {
        let want = format!("{}:", name.to_ascii_lowercase());
        self.headers
            .split("\r\n")
            .skip(1)
            .find(|l| l.to_ascii_lowercase().starts_with(&want))
            .and_then(|l| l.split_once(':').map(|(_, v)| v.trim().to_string()))
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
    HttpResponse { status, headers }
}

// ── Site fixture ──────────────────────────────────────────────────────────────

struct Server {
    port: u16,
    cert_der: Vec<u8>,
    http: std::cell::RefCell<Service>,
    file: Service,
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
        let ready = m6_core::testkit::wait::until(Duration::from_secs(30), || {
            https_get(self, "/public/probe.txt", &[], self.tls()).status == 200
        });
        assert!(
            ready,
            "m6-http never served a backend request\n--- output ---\n{}",
            self.http.borrow().output()
        );
    }

    /// Kill the m6-file backend. Only a warm cache can then answer.
    fn kill_backend(&mut self) {
        self.file.kill();
    }

    /// Everything m6-http has logged so far.
    fn output(&self) -> String {
        self.http.borrow().output()
    }

    /// Number of `auth check` lines m6-http has logged so far.
    fn auth_check_count(&self) -> usize {
        self.output()
            .lines()
            .filter(|l| l.contains("auth check"))
            .count()
    }

    /// Wait, with a deadline, until at least `n` `auth check` lines appear.
    fn wait_for_auth_checks(&self, n: usize) -> bool {
        m6_core::testkit::wait::until(LOG_DEADLINE, || self.auth_check_count() >= n)
    }
}

/// Bring up `m6-file` + `m6-http` over a site with:
///   - `/public/{relpath}`  → open, cacheable
///   - `/private/{relpath}` → `require = "group:admins"`
///
/// m6-http runs at log level `debug` so the `auth check` line is emitted.
fn start_server() -> Server {
    let dir = tempfile::tempdir().unwrap();
    let site = dir.path();

    let (cert_pem, key_pem, cert_der) = generate_tls_cert();
    std::fs::write(site.join("cert.pem"), &cert_pem).unwrap();
    std::fs::write(site.join("key.pem"), &key_pem).unwrap();

    let jwt_pub = generate_jwt_public_key();
    std::fs::write(site.join("auth-public.pem"), &jwt_pub).unwrap();

    std::fs::create_dir_all(site.join("public")).unwrap();
    std::fs::create_dir_all(site.join("private")).unwrap();
    std::fs::create_dir_all(site.join("configs")).unwrap();
    std::fs::write(site.join("public/open.txt"), b"PUBLIC CONTENT").unwrap();
    // Used only as the readiness probe, so no test's path is warmed by it.
    std::fs::write(site.join("public/probe.txt"), b"PROBE").unwrap();
    std::fs::write(site.join("private/secret.txt"), b"TOP SECRET").unwrap();

    let sock = dir.path().join("m6-file-1.sock");
    let sock_glob = dir.path().join("m6-file-*.sock");

    std::fs::write(
        site.join("site.toml"),
        format!(
            r#"
[site]
name   = "auth-paths"
domain = "localhost"

[log]
level  = "debug"
format = "text"

[errors]
mode = "internal"

[analytics]
enabled = false

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
path = "/public/{*relpath}"
handler = "files"
root = "public/"

[[route]]
path = "/private/{*relpath}"
handler = "files"
root = "private/"
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
"#,
            cert = site.join("cert.pem").display(),
            key = site.join("key.pem").display(),
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
    file_proc.wait_for_path(&sock, Duration::from_secs(10));

    let mut http_proc = Service::spawn(
        "m6-http",
        Command::new(binary("m6-http"))
            .arg(site)
            .arg(site.join("system.toml")),
    );
    http_proc.wait_for_tcp(port, Duration::from_secs(10));

    let srv = Server {
        port,
        cert_der,
        http: std::cell::RefCell::new(http_proc),
        file: file_proc,
        _dir: dir,
        _port: claim,
    };
    srv.wait_until_serving();
    srv
}

// ── #295: public routes run no auth code ──────────────────────────────────────

/// A public route must run no auth code, with no token, with any token, and
/// even when answered from the cache. The wrong outcome it catches: a public
/// request entering the auth path, which would emit an `auth check` log line.
#[test]
fn public_route_logs_no_auth_check_without_token_with_any_token_and_from_cache() {
    let mut srv = start_server();
    let tls = srv.tls();

    // No token: served from the backend, no auth check.
    let cold = https_get(&srv, "/public/open.txt", &[], tls.clone());
    assert_eq!(cold.status, 200, "headers:\n{}", cold.headers);
    assert_eq!(
        srv.auth_check_count(),
        0,
        "a public route with no token ran auth code\n--- output ---\n{}",
        srv.output()
    );

    // Any token: still a public route, so the token is never read.
    let token = https_get(
        &srv,
        "/public/open.txt",
        &[("Authorization", "Bearer x.y.z")],
        tls.clone(),
    );
    assert_eq!(token.status, 200, "headers:\n{}", token.headers);
    assert_eq!(
        srv.auth_check_count(),
        0,
        "a public route with a token ran auth code\n--- output ---\n{}",
        srv.output()
    );

    // From the cache: with the backend gone only a cache hit can answer, and
    // it must still not run auth code.
    srv.kill_backend();
    let cached = https_get(&srv, "/public/open.txt", &[], tls.clone());
    assert_eq!(
        cached.status, 200,
        "the public path was not served from the cache\nheaders:\n{}",
        cached.headers
    );
    assert_eq!(
        srv.auth_check_count(),
        0,
        "a public route served from the cache ran auth code\n--- output ---\n{}",
        srv.output()
    );
}

/// A protected route runs the auth path exactly once, which emits one
/// `auth check` line. This is the guard that makes the zero-count assertions
/// above meaningful: if the debug line were removed, this fails while the
/// public-route test still passes.
#[test]
fn protected_route_logs_one_auth_check() {
    let srv = start_server();

    let resp = https_get(&srv, "/private/secret.txt", &[], srv.tls());
    assert_ne!(
        resp.status, 200,
        "a protected route without a token must not be served"
    );

    assert!(
        srv.wait_for_auth_checks(1),
        "a protected route never logged `auth check`\n--- output ---\n{}",
        srv.output()
    );
    assert_eq!(
        srv.auth_check_count(),
        1,
        "a protected route should log exactly one `auth check`\n--- output ---\n{}",
        srv.output()
    );
}

// ── #296: browser redirects for a protected route ─────────────────────────────

/// A browser (Accept includes text/html) with no session is redirected to
/// `/login?next=<path>`. The wrong outcome it catches: the `next` parameter
/// being dropped, or a browser being answered 401 instead of 302.
#[test]
fn browser_without_session_is_redirected_to_login_with_next() {
    let srv = start_server();

    let resp = https_get(
        &srv,
        "/private/secret.txt",
        &[("Accept", "text/html")],
        srv.tls(),
    );
    assert_eq!(
        resp.status, 302,
        "a browser without a session should be redirected, got {}\nheaders:\n{}",
        resp.status, resp.headers
    );
    assert_eq!(
        resp.header_value("Location").as_deref(),
        Some("/login?next=/private/secret.txt"),
        "the redirect must carry the protected path in `next`\nheaders:\n{}",
        resp.headers
    );
}

/// A browser with a refresh cookie but no session is redirected to
/// `/auth/refresh`. The wrong outcome it catches: the refresh cookie being
/// ignored, so the browser is sent to `/login` instead.
#[test]
fn browser_with_refresh_cookie_and_no_session_is_redirected_to_refresh() {
    let srv = start_server();

    let resp = https_get(
        &srv,
        "/private/secret.txt",
        &[
            ("Accept", "text/html"),
            ("Cookie", "refresh=some-refresh-token"),
        ],
        srv.tls(),
    );
    assert_eq!(
        resp.status, 302,
        "a browser with a refresh cookie should be redirected, got {}\nheaders:\n{}",
        resp.status, resp.headers
    );
    assert_eq!(
        resp.header_value("Location").as_deref(),
        Some("/auth/refresh"),
        "the redirect must go to /auth/refresh, not /login\nheaders:\n{}",
        resp.headers
    );
}

/// An API client (Accept application/json) with no session gets a plain 401,
/// never a browser redirect. The wrong outcome it catches: browser detection
/// leaking into the API path, so an API client is answered 302.
#[test]
fn api_client_without_session_gets_401_not_a_redirect() {
    let srv = start_server();

    let resp = https_get(
        &srv,
        "/private/secret.txt",
        &[("Accept", "application/json")],
        srv.tls(),
    );
    assert_eq!(
        resp.status, 401,
        "an API client without a session should get 401, got {}\nheaders:\n{}",
        resp.status, resp.headers
    );
    assert!(
        resp.header_value("Location").is_none(),
        "an API client must not be redirected\nheaders:\n{}",
        resp.headers
    );
}
