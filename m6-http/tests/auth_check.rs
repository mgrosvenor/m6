//! Route `require` asks the authority: the step 5 request path, end to end.
//!
//! A real `m6-http`, a real `m6-file` content backend, and `testkit::CheckServer`
//! as the authority. The edge no longer verifies a token locally: it sends the
//! credential to `POST /check` and applies the route's `require` to the identity
//! that comes back.

#![allow(
    clippy::disallowed_methods,
    reason = "test code: a blocking TLS client and a scoped thread serve the fake checker while the child runs"
)]

use std::cell::RefCell;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use rustls::StreamOwned;

use m6_core::auth::Identity;
use m6_core::testkit::{binary, claim_port, wait, CheckServer, PortClaim, Reply, Service};

/// Build the one identity these tests answer, optionally with a group.
fn identity(groups: &[&str]) -> Identity {
    Identity {
        sub: "user-1".to_string(),
        username: "alice".to_string(),
        roles: vec!["user".to_string()],
        groups: groups.iter().map(|group| group.to_string()).collect(),
    }
}

// ── TLS client, copied from auth_paths ────────────────────────────────────────

fn tls_client_config(cert_der: &[u8]) -> Arc<rustls::ClientConfig> {
    rustls::crypto::aws_lc_rs::default_provider()
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
    body: String,
}

impl HttpResponse {
    fn header_value(&self, name: &str) -> Option<String> {
        let want = format!("{}:", name.to_ascii_lowercase());
        self.headers
            .split("\r\n")
            .skip(1)
            .find(|line| line.to_ascii_lowercase().starts_with(&want))
            .and_then(|line| {
                line.split_once(':')
                    .map(|(_, value)| value.trim().to_string())
            })
    }
}

fn https_get(
    port: u16,
    http: &RefCell<Service>,
    path: &str,
    extra: &[(&str, &str)],
    tls: Arc<rustls::ClientConfig>,
) -> HttpResponse {
    let tcp = match TcpStream::connect(("127.0.0.1", port)) {
        Ok(stream) => stream,
        Err(e) => {
            http.borrow_mut().assert_alive("the client was connecting");
            return HttpResponse {
                status: 0,
                headers: format!("<no connection: {e}>"),
                body: String::new(),
            };
        }
    };
    https_get_on(tcp, port, path, extra, tls)
}

/// The same request without the service liveness check, for a thread that
/// cannot borrow the `RefCell<Service>`.
fn https_get_raw(
    port: u16,
    path: &str,
    extra: &[(&str, &str)],
    tls: Arc<rustls::ClientConfig>,
) -> HttpResponse {
    let tcp = TcpStream::connect(("127.0.0.1", port))
        .unwrap_or_else(|e| panic!("the parked client could not connect: {e}"));
    https_get_on(tcp, port, path, extra, tls)
}

fn https_get_on(
    tcp: TcpStream,
    port: u16,
    path: &str,
    extra: &[(&str, &str)],
    tls: Arc<rustls::ClientConfig>,
) -> HttpResponse {
    tcp.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let name = rustls::pki_types::ServerName::try_from("127.0.0.1".to_string()).unwrap();
    let conn = rustls::ClientConnection::new(tls, name).unwrap();
    let mut stream = StreamOwned::new(conn, tcp);

    let mut req = format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n");
    for (key, value) in extra {
        req.push_str(&format!("{key}: {value}\r\n"));
    }
    req.push_str("Connection: close\r\n\r\n");
    stream.write_all(req.as_bytes()).unwrap();
    stream.flush().unwrap();

    let mut raw = Vec::new();
    let _ = stream.read_to_end(&mut raw);
    let text = String::from_utf8_lossy(&raw);
    let end = text.find("\r\n\r\n").unwrap_or(raw.len());
    let headers = text[..end].to_string();
    let body = text.get(end + 4..).unwrap_or("").to_string();
    let status = headers
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|status| status.parse().ok())
        .unwrap_or(0);
    HttpResponse {
        status,
        headers,
        body,
    }
}

// ── Fixture ───────────────────────────────────────────────────────────────────

struct TestServer {
    port: u16,
    cert_der: Vec<u8>,
    http: RefCell<Service>,
    _file: Service,
    check: CheckServer,
    /// `check.calls()` after the start probe, so a test counts request checks.
    base_calls: usize,
    /// Fires when the parked backend accepts a connection.
    slow_ready: std::sync::mpsc::Receiver<()>,
    _dir: tempfile::TempDir,
    _port: PortClaim,
}

impl TestServer {
    fn start() -> Self {
        Self::start_with(false)
    }

    /// The same fixture with `[errors] verbose_fallback`, so the 503 page
    /// carries the backend name and the detail a client would see.
    fn start_verbose() -> Self {
        Self::start_with(true)
    }

    fn start_with(verbose: bool) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let site = dir.path();

        let cert = rcgen::generate_simple_self_signed(vec![
            "localhost".to_string(),
            "127.0.0.1".to_string(),
        ])
        .unwrap();
        let cert_pem = cert.cert.pem();
        let key_pem = cert.key_pair.serialize_pem();
        let cert_der = cert.cert.der().to_vec();
        std::fs::write(site.join("cert.pem"), &cert_pem).unwrap();
        std::fs::write(site.join("key.pem"), &key_pem).unwrap();
        let jwt_key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
        std::fs::write(site.join("auth-public.pem"), jwt_key.public_key_pem()).unwrap();

        std::fs::create_dir_all(site.join("private")).unwrap();
        std::fs::write(site.join("private/secret.txt"), b"TOP SECRET").unwrap();

        let check_path = site.join("check.sock");
        let mut check = CheckServer::bind(&check_path).unwrap();

        // A content backend that accepts and never answers. One request parked
        // on it is the other upstream the check's deadline must not wait for.
        let slow_path = site.join("slow.sock");
        let slow_listener = std::os::unix::net::UnixListener::bind(&slow_path).unwrap();
        let (slow_tx, slow_ready) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            // Hold every accepted connection open, so the parked request stays
            // parked until m6-http's backend timeout closes it.
            let mut held = Vec::new();
            while let Ok((stream, _)) = slow_listener.accept() {
                let _ = slow_tx.send(());
                held.push(stream);
            }
            drop(held);
        });

        let file_sock = site.join("m6-file-1.sock");
        let file_glob = site.join("m6-file-*.sock");
        std::fs::write(
            site.join("site.toml"),
            format!(
                r#"
[site]
name = "auth-check"
domain = "example.com"

[log]
level = "debug"

[errors]
mode = "internal"
verbose_fallback = {verbose}

[analytics]
enabled = false

[server]
backend_timeout_secs = 3

[[backend]]
name = "m6-file"
sockets = "{file_glob}"

[[backend]]
name = "auth-check"
sockets = "{check}"

[[backend]]
name = "slow"
sockets = "{slow}"

[[route]]
path = "/private/{{relpath}}"
backend = "m6-file"
require = "group:admins"

[[route]]
path = "/slow"
backend = "slow"
"#,
                file_glob = file_glob.display(),
                check = check_path.display(),
                slow = slow_path.display(),
            ),
        )
        .unwrap();
        std::fs::write(
            site.join("m6-file.conf"),
            r#"
[[route]]
path = "/private/{*relpath}"
handler = "files"
root = "private/"
"#,
        )
        .unwrap();

        let claim = claim_port();
        let port = claim.port();
        let system = site.join("system.toml");
        std::fs::write(
            &system,
            format!(
                r#"
[server]
bind = "127.0.0.1:{port}"
tls_cert = "{cert}"
tls_key = "{key}"

[node]
name = "test-node"

[auth]
backend = "auth-check"
public_key = "{pubkey}"
"#,
                cert = site.join("cert.pem").display(),
                key = site.join("key.pem").display(),
                pubkey = site.join("auth-public.pem").display(),
            ),
        )
        .unwrap();

        let mut file = Service::spawn(
            "m6-file",
            Command::new(binary("m6-file"))
                .arg(site)
                .arg(site.join("m6-file.conf"))
                .env("M6_SOCKET_OVERRIDE", &file_sock),
        );
        file.wait_for_path(&file_sock, m6_core::testkit::STARTUP_BOUND);
        file.wait_for_started(m6_core::testkit::STARTUP_BOUND);

        let mut http = Service::spawn(
            "m6-http",
            Command::new(binary("m6-http")).arg(site).arg(&system),
        );
        // The start probe sends no credential, so the fake's default 401 is
        // its answer, and the node starts.
        check.serve_one().expect("serve the start probe");
        http.wait_for_started(m6_core::testkit::STARTUP_BOUND);
        let base_calls = check.calls();

        TestServer {
            port,
            cert_der,
            http: RefCell::new(http),
            _file: file,
            check,
            base_calls,
            slow_ready,
            _dir: dir,
            _port: claim,
        }
    }

    /// One request that is expected to make one check call. The fake serves the
    /// check from a scoped thread while the blocking client waits.
    fn request(&mut self, path: &str, extra: &[(&str, &str)]) -> HttpResponse {
        let TestServer {
            port,
            cert_der,
            http,
            check,
            ..
        } = self;
        let tls = tls_client_config(cert_der);
        std::thread::scope(|scope| {
            let served = scope.spawn(|| check.serve_one());
            let response = https_get(*port, http, path, extra, tls);
            // A timeout test's checker wakes after the edge has closed the
            // slot, so its write may fail. That is the scenario, not a failure
            // of the test.
            let _ = served.join();
            response
        })
    }

    fn request_without_check(&self, path: &str, extra: &[(&str, &str)]) -> HttpResponse {
        https_get(
            self.port,
            &self.http,
            path,
            extra,
            tls_client_config(&self.cert_der),
        )
    }

    fn checks_since_start(&self) -> usize {
        self.check.calls() - self.base_calls
    }

    fn answer(&mut self, token: &str, reply: Reply) {
        self.check.answer(token, reply);
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

/// The test for the whole job: a credential that is not a JWT, holding no
/// locally checkable claim, is accepted because the authority said who it is.
#[test]
fn a_credential_that_is_not_a_jwt_is_checked_by_the_authority() {
    let mut srv = TestServer::start();
    srv.answer("opaque-token", Reply::Identity(identity(&["admins"])));
    let response = srv.request(
        "/private/secret.txt",
        &[("Authorization", "Bearer opaque-token")],
    );
    assert_eq!(
        response.status, 200,
        "the authority's identity was not accepted\n{}",
        response.headers
    );
}

/// A protected request with no credential is refused locally, with no call.
#[test]
fn no_credential_makes_no_call() {
    let srv = TestServer::start();
    let response = srv.request_without_check("/private/secret.txt", &[]);
    assert_eq!(response.status, 401, "{}", response.headers);
    assert_eq!(
        srv.checks_since_start(),
        0,
        "an anonymous request reached the authority"
    );
}

#[test]
fn one_protected_request_makes_one_call() {
    let mut srv = TestServer::start();
    srv.answer("opaque-token", Reply::Identity(identity(&["admins"])));
    let response = srv.request(
        "/private/secret.txt",
        &[("Authorization", "Bearer opaque-token")],
    );
    assert_eq!(response.status, 200);
    assert_eq!(srv.checks_since_start(), 1);
}

/// The authority decides the role on every request. Changing its answer between
/// two requests changes the second answer, so no decision was reused.
#[test]
fn a_removed_role_is_refused_on_the_next_request() {
    let mut srv = TestServer::start();
    srv.answer("opaque-token", Reply::Identity(identity(&["admins"])));
    let first = srv.request(
        "/private/secret.txt",
        &[("Authorization", "Bearer opaque-token")],
    );
    assert_eq!(first.status, 200, "{}", first.headers);

    srv.answer("opaque-token", Reply::Identity(identity(&[])));
    let second = srv.request(
        "/private/secret.txt",
        &[("Authorization", "Bearer opaque-token")],
    );
    assert_eq!(
        second.status, 403,
        "the removed role was not seen on the next request\n{}",
        second.headers
    );
    assert_eq!(
        srv.checks_since_start(),
        2,
        "the second request reused a decision"
    );
}

/// A checker that does not answer inside 50 ms is a 503 with `Retry-After: 1`,
/// not a 401 and not a 502.
#[test]
fn a_check_that_times_out_is_503_with_retry_after() {
    let mut srv = TestServer::start();
    srv.answer(
        "opaque-token",
        Reply::Slow {
            after: Duration::from_millis(200),
            answer: Box::new(Reply::Identity(identity(&["admins"]))),
        },
    );
    let response = srv.request(
        "/private/secret.txt",
        &[("Authorization", "Bearer opaque-token")],
    );
    assert_eq!(response.status, 503, "{}", response.headers);
    assert_eq!(
        response.header_value("retry-after").as_deref(),
        Some("1"),
        "{}",
        response.headers
    );
}

/// A 401 from the authority is the edge's 401, with the one token refusal.
#[test]
fn a_check_that_is_refused_is_401_with_www_authenticate() {
    let mut srv = TestServer::start();
    srv.answer("opaque-token", Reply::Unauthorised);
    let response = srv.request(
        "/private/secret.txt",
        &[("Authorization", "Bearer opaque-token")],
    );
    assert_eq!(response.status, 401, "{}", response.headers);
    assert_eq!(
        response.header_value("www-authenticate").as_deref(),
        Some("Bearer"),
        "{}",
        response.headers
    );
}

/// A browser holding a refresh cookie is sent to the refresh path when the
/// authority refuses, exactly as the local check did.
#[test]
fn a_browser_with_a_refresh_cookie_is_sent_to_refresh_when_the_check_refuses() {
    let mut srv = TestServer::start();
    srv.answer("opaque-token", Reply::Unauthorised);
    let response = srv.request(
        "/private/secret.txt",
        &[
            ("Authorization", "Bearer opaque-token"),
            ("Accept", "text/html"),
            ("Cookie", "refresh=abc"),
        ],
    );
    assert_eq!(response.status, 302, "{}", response.headers);
    assert_eq!(
        response.header_value("location").as_deref(),
        Some("/auth/refresh"),
        "{}",
        response.headers
    );
}

/// A 403 from the authority is `NoDecision`, so the route answers its 503 with
/// `Retry-After: 1`.
///
/// The wrong decision this catches: a caller that treated a 403 as allowed
/// would fetch `/private/secret.txt` and answer 200. The core unit test pins
/// the mapping, and this drives the whole route path that acts on it.
#[test]
fn a_check_that_is_403_is_503_not_200() {
    let mut srv = TestServer::start();
    srv.check.unknown_answer(Reply::Forbidden);
    let response = srv.request(
        "/private/secret.txt",
        &[("Authorization", "Bearer opaque-token")],
    );
    assert_eq!(
        response.status, 503,
        "a 403 from the authority was not the edge's 503\n{}",
        response.headers
    );
    assert_eq!(
        response.header_value("retry-after").as_deref(),
        Some("1"),
        "{}",
        response.headers
    );
}

/// A 503 from the authority is the route's 503 with `Retry-After: 1`, never
/// 200. A caller that read an unavailable answer as allowed would serve the
/// file.
#[test]
fn a_check_that_is_503_is_503_not_200() {
    let mut srv = TestServer::start();
    srv.check.unknown_answer(Reply::Unavailable);
    let response = srv.request(
        "/private/secret.txt",
        &[("Authorization", "Bearer opaque-token")],
    );
    assert_eq!(
        response.status, 503,
        "a 503 from the authority was not the edge's 503\n{}",
        response.headers
    );
    assert_eq!(
        response.header_value("retry-after").as_deref(),
        Some("1"),
        "{}",
        response.headers
    );
}

/// A parked backend must not delay the check's 50 ms deadline.
///
/// The wrong outcome this catches: the check token in the shared upstream
/// deadline table, behind the parked request's `backend_timeout_secs`, so a
/// protected request waits for another request's upstream to expire. Here
/// that is 3 s, and the default is 30 s. Before the check had its own table
/// this test measured about 3 s and failed its one-second bound.
#[test]
fn a_parked_backend_does_not_delay_the_check_deadline() {
    let mut srv = TestServer::start();
    srv.check.unknown_answer(Reply::Hold);
    let tls = tls_client_config(&srv.cert_der);
    let port = srv.port;
    std::thread::scope(|scope| {
        let parked = scope.spawn(move || https_get_raw(port, "/slow", &[], tls));
        srv.slow_ready
            .recv_timeout(Duration::from_secs(10))
            .expect("the slow backend was never reached");
        let started = std::time::Instant::now();
        let response = srv.request(
            "/private/secret.txt",
            &[("Authorization", "Bearer opaque-token")],
        );
        let elapsed = started.elapsed();
        assert_eq!(response.status, 503, "{}", response.headers);
        assert_eq!(
            response.header_value("retry-after").as_deref(),
            Some("1"),
            "{}",
            response.headers
        );
        assert!(
            elapsed < Duration::from_secs(1),
            "the check waited {elapsed:?} behind another request's backend"
        );
        let slow = parked.join().expect("the parked request");
        assert_eq!(
            slow.status, 502,
            "the parked backend did not time out\n{}",
            slow.headers
        );
    });
}

/// A 500 from the authority is `NoDecision`, so the route answers 503 with
/// `Retry-After: 1`, never 200 and never 502.
#[test]
fn a_check_that_is_500_is_503_not_200() {
    let mut srv = TestServer::start();
    srv.check.unknown_answer(Reply::Status(500));
    let response = srv.request(
        "/private/secret.txt",
        &[("Authorization", "Bearer opaque-token")],
    );
    assert_eq!(
        response.status, 503,
        "a 500 from the authority was not the edge's 503\n{}",
        response.headers
    );
    assert_eq!(
        response.header_value("retry-after").as_deref(),
        Some("1"),
        "{}",
        response.headers
    );
}

/// An answer cut off mid-body is unparsable, so the route answers 503 with
/// `Retry-After: 1` rather than serving what the head promised.
#[test]
fn a_check_cut_off_mid_answer_is_503_not_200() {
    let mut srv = TestServer::start();
    srv.check.unknown_answer(Reply::CloseMidAnswer);
    let response = srv.request(
        "/private/secret.txt",
        &[("Authorization", "Bearer opaque-token")],
    );
    assert_eq!(
        response.status, 503,
        "a cut-off answer was not the edge's 503\n{}",
        response.headers
    );
    assert_eq!(
        response.header_value("retry-after").as_deref(),
        Some("1"),
        "{}",
        response.headers
    );
}

/// The re-entry's answers advertise HTTP/3 like every other answer.
///
/// The wrong outcome this catches: `complete_auth` writing a `Ready` outcome
/// straight through `write_waiter`, which skips `finish_miss` and its
/// `Alt-Svc`. Before the fix the authority's 401, the 403 and the 503 were the
/// only answers in the release without the header, so a client could tell that
/// something other than the edge's own path had answered.
#[test]
fn the_authoritys_answers_advertise_http3_like_a_local_answer() {
    let mut srv = TestServer::start();
    let local = srv.request_without_check("/private/secret.txt", &[]);
    assert_eq!(local.status, 401, "{}", local.headers);
    let alt = local
        .header_value("alt-svc")
        .expect("the local 401 has no Alt-Svc");

    srv.answer("refused-token", Reply::Unauthorised);
    let refused = srv.request(
        "/private/secret.txt",
        &[("Authorization", "Bearer refused-token")],
    );
    assert_eq!(refused.status, 401, "{}", refused.headers);
    assert_eq!(
        refused.header_value("alt-svc").as_deref(),
        Some(alt.as_str()),
        "{}",
        refused.headers
    );

    srv.answer("busy-token", Reply::Unavailable);
    let busy = srv.request(
        "/private/secret.txt",
        &[("Authorization", "Bearer busy-token")],
    );
    assert_eq!(busy.status, 503, "{}", busy.headers);
    assert_eq!(
        busy.header_value("alt-svc").as_deref(),
        Some(alt.as_str()),
        "{}",
        busy.headers
    );

    srv.answer("wrong-group", Reply::Identity(identity(&[])));
    let denied = srv.request(
        "/private/secret.txt",
        &[("Authorization", "Bearer wrong-group")],
    );
    assert_eq!(denied.status, 403, "{}", denied.headers);
    assert_eq!(
        denied.header_value("alt-svc").as_deref(),
        Some(alt.as_str()),
        "{}",
        denied.headers
    );
}

/// The 503 names the authority's backend and gives one detail for every check
/// failure.
///
/// The wrong outcome this catches: `backend_unavailable` passed the route's
/// content backend and the real transport error, so a `verbose_fallback` page
/// told the client that `m6-file` failed and whether the check timed out or
/// refused the connection. The detail is now one fixed string, and a timeout
/// renders the same body as a 503 the checker answered.
#[test]
fn the_503_names_the_auth_backend_not_the_content_backend() {
    let mut srv = TestServer::start_verbose();
    srv.answer("busy-token", Reply::Unavailable);
    let busy = srv.request(
        "/private/secret.txt",
        &[("Authorization", "Bearer busy-token")],
    );
    assert_eq!(busy.status, 503, "{}", busy.headers);
    assert!(
        busy.body.contains("auth-check"),
        "the page did not name the auth backend\n{}",
        busy.body
    );
    assert!(
        !busy.body.contains("m6-file"),
        "the page named the content backend\n{}",
        busy.body
    );
    assert!(
        busy.body.contains("auth check unavailable"),
        "the page did not carry the one check detail\n{}",
        busy.body
    );

    srv.answer(
        "slow-token",
        Reply::Slow {
            after: Duration::from_millis(200),
            answer: Box::new(Reply::Identity(identity(&["admins"]))),
        },
    );
    let timed = srv.request(
        "/private/secret.txt",
        &[("Authorization", "Bearer slow-token")],
    );
    assert_eq!(timed.status, 503, "{}", timed.headers);
    assert!(
        timed.body.contains("auth check unavailable"),
        "{}",
        timed.body
    );
    assert!(
        !timed.body.contains("Connect(") && !timed.body.contains("timed out"),
        "the timeout page named the transport failure\n{}",
        timed.body
    );
}

/// The count of log lines carrying `needle`, read from the running service.
fn check_log_lines(srv: &TestServer, needle: &str) -> usize {
    srv.http.borrow().output().matches(needle).count()
}

/// One warn when the check backend stops answering, one info when it answers
/// again, however many requests see each state.
///
/// The wrong outcome this catches: a line per failed request, which makes the
/// outage unreadable in the log, or a line per answered request, which makes
/// the recovery invisible by burying it. The state is one bool read where the
/// outcome is already known, so `/health` can read the same flag later without
/// a timer or a probe.
#[test]
fn a_checker_that_stops_answering_logs_one_warn_and_one_info() {
    let mut srv = TestServer::start();

    // The checker accepts and does not answer, so the 50 ms deadline fires.
    srv.check.unknown_answer(Reply::Hold);
    for _ in 0..3 {
        let response = srv.request(
            "/private/secret.txt",
            &[("Authorization", "Bearer opaque-token")],
        );
        assert_eq!(response.status, 503, "{}", response.headers);
    }
    assert!(
        wait::until(Duration::from_secs(10), || {
            check_log_lines(&srv, "the check backend did not answer") >= 1
        }),
        "the outage was not logged\n--- output ---\n{}",
        srv.http.borrow().output()
    );

    // A checker that answers 503 is present and answering, so it clears the
    // flag: it is not the same fact as a checker that is not there.
    srv.check.unknown_answer(Reply::Unavailable);
    for _ in 0..3 {
        let response = srv.request(
            "/private/secret.txt",
            &[("Authorization", "Bearer opaque-token")],
        );
        assert_eq!(response.status, 503, "{}", response.headers);
    }
    assert!(
        wait::until(Duration::from_secs(10), || {
            check_log_lines(&srv, "the check backend answered again") >= 1
        }),
        "the recovery was not logged\n--- output ---\n{}",
        srv.http.borrow().output()
    );

    assert_eq!(
        check_log_lines(&srv, "the check backend did not answer"),
        1,
        "the outage was logged more than once\n--- output ---\n{}",
        srv.http.borrow().output()
    );
    assert_eq!(
        check_log_lines(&srv, "the check backend answered again"),
        1,
        "the recovery was logged more than once\n--- output ---\n{}",
        srv.http.borrow().output()
    );
}
