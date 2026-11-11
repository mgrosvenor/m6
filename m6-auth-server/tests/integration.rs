#![allow(
    clippy::disallowed_methods,
    reason = "test code: threads, sleeps and blocking clients drive the service from outside"
)]

use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use m6_core::testkit::{assert_app_lifecycle, binary, spawn_instances, wait, Service};

// ─── Test fixtures ────────────────────────────────────────────────────────────

/// Generate an RSA key pair (2048-bit) for testing.
/// Returns (private_pem, public_pem).
fn generate_test_rsa_keys() -> (String, String) {
    use std::process::Command;

    let dir = tempfile::TempDir::new().unwrap();
    let key_path = dir.path().join("test.key");
    let pub_path = dir.path().join("test.pub");

    Command::new("openssl")
        .args(["genrsa", "-out"])
        .arg(&key_path)
        .arg("2048")
        .output()
        .expect("openssl genrsa");

    Command::new("openssl")
        .args(["rsa", "-in"])
        .arg(&key_path)
        .arg("-pubout")
        .arg("-out")
        .arg(&pub_path)
        .output()
        .expect("openssl rsa -pubout");

    let private = std::fs::read_to_string(&key_path).unwrap();
    let public = std::fs::read_to_string(&pub_path).unwrap();
    (private, public)
}

struct TestEnv {
    _temp: tempfile::TempDir,
    site_dir: PathBuf,
    config_path: PathBuf,
    socket_path: PathBuf,
    _key_dir: tempfile::TempDir,
}

fn setup_test_env(id: &str) -> TestEnv {
    let temp = tempfile::TempDir::new().unwrap();
    let key_dir = tempfile::TempDir::new().unwrap();

    let site_dir = temp.path().to_path_buf();
    let config_path = site_dir.join("m6-auth-test.conf");

    // Generate keys
    let (private_pem, public_pem) = generate_test_rsa_keys();
    let private_key_path = key_dir.path().join("auth.pem");
    let public_key_path = key_dir.path().join("auth.pub");
    std::fs::write(&private_key_path, &private_pem).unwrap();
    std::fs::write(&public_key_path, &public_pem).unwrap();

    // Write config
    let db_path = site_dir.join("data/auth.db").display().to_string();
    let config = format!(
        r#"[storage]
path = "{}"

[tokens]
access_ttl  = 900
refresh_ttl = 2592000
issuer      = "test.example.com"

[thread_pool]
size = 4
queue_size = 32

[keys]
private_key = "{}"
public_key  = "{}"
"#,
        db_path,
        private_key_path.display(),
        public_key_path.display(),
    );
    std::fs::write(&config_path, &config).unwrap();

    // Create data directory
    std::fs::create_dir_all(site_dir.join("data")).unwrap();
    std::fs::write(site_dir.join("site.toml"), "").unwrap();

    // The socket lives in this test's own temp directory, not a shared one.
    // It used to be `$TMPDIR/m6-test-sockets/<id>.sock`, a fixed path per test
    // that setup deleted before spawning, so two runs at once meant a test
    // unlinking a socket another live server was serving on.
    let socket_path = temp.path().join(format!("{id}.sock"));

    TestEnv {
        _temp: temp,
        site_dir,
        config_path,
        socket_path,
        _key_dir: key_dir,
    }
}

/// Spawn `m6-auth-server` and wait until it accepts a connection.
fn spawn_server(env: &TestEnv) -> Service {
    let mut svc = Service::spawn(
        "m6-auth-server",
        Command::new(binary("m6-auth-server"))
            .arg(&env.site_dir)
            .arg(&env.config_path)
            .env("M6_SOCKET_OVERRIDE", &env.socket_path),
    );
    wait_until_serving(&mut svc, &env.socket_path);
    svc
}

/// Wait until `svc` has created `socket` and accepts a connection on it.
///
/// Readiness is a successful connect, not an existing path: `bind` creates the
/// socket file and `listen` comes after, so the file appearing proves nothing.
fn wait_until_serving(svc: &mut Service, socket: &Path) {
    svc.wait_for_path(socket, Duration::from_secs(10));
    assert!(
        wait::for_unix(socket, Duration::from_secs(10)),
        "m6-auth-server created {} but never accepted a connection\n--- output ---\n{}",
        socket.display(),
        svc.output()
    );
}

/// Spawn `m6-auth-server` expecting it to refuse `what` at start, and return
/// its output.
///
/// The service must exit 2 within 10 s. A service still running is the failure
/// this panics on, before any caller's message assertion can hide it.
fn start_failure(env: &TestEnv, what: &str) -> String {
    let mut svc = Service::spawn(
        "m6-auth-server",
        Command::new(binary("m6-auth-server"))
            .arg(&env.site_dir)
            .arg(&env.config_path)
            .env("M6_SOCKET_OVERRIDE", &env.socket_path),
    );
    let status = svc
        .wait_for_exit(std::time::Instant::now() + Duration::from_secs(10))
        .unwrap_or_else(|| {
            panic!(
                "m6-auth-server did not stop for {what}\n--- output ---\n{}",
                svc.output()
            )
        });
    assert_eq!(
        status.code(),
        Some(2),
        "m6-auth-server should exit 2 for {what}\n--- output ---\n{}",
        svc.output()
    );
    svc.output()
}

/// Create a test user via the database before the server starts.
/// Returns the db path so we can pre-seed it.
fn seed_user(site_dir: &Path, username: &str, password: &str) {
    let db_path = site_dir.join("data/auth.db");
    std::fs::create_dir_all(db_path.parent().unwrap()).unwrap();
    let db = m6_auth::Db::open(&db_path).expect("open db");
    db.user_create(username, password, &["user"])
        .expect("create user");
}

/// Hold the database's write lock until the connection is dropped or the
/// transaction is committed. `BEGIN IMMEDIATE` takes the lock at the begin, so
/// a caller that has returned from here holds it.
fn hold_write_lock(db_path: &Path) -> rusqlite::Connection {
    let conn = rusqlite::Connection::open(db_path).expect("open the lock connection");
    conn.busy_timeout(Duration::from_secs(1))
        .expect("set the lock connection's timeout");
    conn.execute_batch("BEGIN IMMEDIATE")
        .expect("take the database's write lock");
    conn
}

/// Send a raw HTTP request over a Unix socket and return the full response bytes.
fn http_request(socket_path: &Path, request: &str) -> String {
    http_request_within(socket_path, request, Duration::from_secs(10))
}

/// `http_request` with a read deadline, for a response that must arrive while
/// the service is holding another request.
fn http_request_within(socket_path: &Path, request: &str, read_timeout: Duration) -> String {
    let mut stream = UnixStream::connect(socket_path)
        .unwrap_or_else(|e| panic!("connect {:?}: {}", socket_path, e));
    stream.set_read_timeout(Some(read_timeout)).unwrap();
    stream.write_all(request.as_bytes()).unwrap();
    // One response, not read-to-EOF: the connection stays open (RFC 9112 9.3).
    let method = request.split(' ').next().unwrap_or("");
    let resp = m6_core::testkit::read_one(&mut stream, method)
        .unwrap_or_else(|e| panic!("no response within {read_timeout:?}: {e}"));
    String::from_utf8_lossy(&resp).into_owned()
}

/// The bytes of a request with a body, for a test that sends it on a
/// connection it keeps.
fn request_with_body(method: &str, path: &str, content_type: &str, body: &str) -> String {
    format!(
        "{} {} HTTP/1.1\r\nHost: localhost\r\nContent-Type: {}\r\nContent-Length: {}\r\n\r\n{}",
        method,
        path,
        content_type,
        body.len(),
        body
    )
}

/// The JSON login request the locked-database tests send.
fn login_json_request(body: &str) -> String {
    request_with_body("POST", "/auth/login", "application/json", body)
}

fn http_request_with_body(
    socket_path: &Path,
    method: &str,
    path: &str,
    content_type: &str,
    body: &str,
) -> String {
    http_request(
        socket_path,
        &request_with_body(method, path, content_type, body),
    )
}

fn parse_status(resp: &str) -> u16 {
    resp.split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

fn get_header<'a>(resp: &'a str, name: &str) -> Option<&'a str> {
    let name_lower = name.to_ascii_lowercase();
    for line in resp.lines() {
        if let Some(colon) = line.find(':') {
            let k = line[..colon].trim().to_ascii_lowercase();
            if k == name_lower {
                return Some(line[colon + 1..].trim());
            }
        }
    }
    None
}

fn get_all_headers<'a>(resp: &'a str, name: &str) -> Vec<&'a str> {
    let name_lower = name.to_ascii_lowercase();
    let mut result = Vec::new();
    for line in resp.lines() {
        if let Some(colon) = line.find(':') {
            let k = line[..colon].trim().to_ascii_lowercase();
            if k == name_lower {
                result.push(line[colon + 1..].trim());
            }
        }
    }
    result
}

// ─── Tests ────────────────────────────────────────────────────────────────────

/// Login form POST, valid credentials → 302, two HttpOnly cookies set with correct Path.
#[test]
fn t01_login_form_valid_credentials_302_cookies() {
    let env = setup_test_env("t01");
    seed_user(&env.site_dir, "alice", "correct_pass");
    let _guard = spawn_server(&env);

    let body = "username=alice&password=correct_pass&next=/dashboard";
    let resp = http_request_with_body(
        &env.socket_path,
        "POST",
        "/auth/login",
        "application/x-www-form-urlencoded",
        body,
    );

    assert_eq!(parse_status(&resp), 302, "expected 302\n{}", resp);

    let location = get_header(&resp, "Location").unwrap_or("");
    assert_eq!(
        location, "/dashboard",
        "expected redirect to /dashboard, got {}",
        location
    );

    let cookies = get_all_headers(&resp, "Set-Cookie");
    assert!(
        cookies.len() >= 2,
        "expected 2 Set-Cookie headers, got {}",
        cookies.len()
    );

    let has_session = cookies
        .iter()
        .any(|c| c.starts_with("session=") && c.contains("HttpOnly") && c.contains("Path=/"));
    let has_refresh = cookies.iter().any(|c| {
        c.starts_with("refresh=") && c.contains("HttpOnly") && c.contains("Path=/auth/refresh")
    });
    assert!(
        has_session,
        "missing session cookie with HttpOnly and Path=/\ncookies: {:?}",
        cookies
    );
    assert!(
        has_refresh,
        "missing refresh cookie with HttpOnly and Path=/auth/refresh\ncookies: {:?}",
        cookies
    );
}

/// Login JSON, valid credentials → 200, JSON tokens, no cookies.
#[test]
fn t02_login_json_valid_credentials_200_no_cookies() {
    let env = setup_test_env("t02");
    seed_user(&env.site_dir, "bob", "pass123");
    let _guard = spawn_server(&env);

    let body = r#"{"username":"bob","password":"pass123"}"#;
    let resp = http_request_with_body(
        &env.socket_path,
        "POST",
        "/auth/login",
        "application/json",
        body,
    );

    assert_eq!(parse_status(&resp), 200, "expected 200\n{}", resp);

    let cookies = get_all_headers(&resp, "Set-Cookie");
    assert!(
        cookies.is_empty(),
        "JSON login should not set cookies, got: {:?}",
        cookies
    );

    // Verify JSON body
    let body_start = resp.find("\r\n\r\n").map(|i| i + 4).unwrap_or(resp.len());
    let json_body: serde_json::Value =
        serde_json::from_str(&resp[body_start..]).expect("valid JSON");
    assert!(
        json_body["access_token"].is_string(),
        "missing access_token"
    );
    assert!(
        json_body["refresh_token"].is_string(),
        "missing refresh_token"
    );
    assert!(json_body["expires_in"].is_number(), "missing expires_in");
}

/// Login: wrong password → 302 to login error page (form) / 401 (JSON).
#[test]
fn t03_login_form_wrong_password_302_error() {
    let env = setup_test_env("t03");
    seed_user(&env.site_dir, "carol", "right_pass");
    let _guard = spawn_server(&env);

    let body = "username=carol&password=wrong_pass";
    let resp = http_request_with_body(
        &env.socket_path,
        "POST",
        "/auth/login",
        "application/x-www-form-urlencoded",
        body,
    );

    assert_eq!(parse_status(&resp), 302, "expected 302\n{}", resp);
    let location = get_header(&resp, "Location").unwrap_or("");
    assert!(
        location.contains("/login") && location.contains("error=invalid"),
        "expected redirect to login error, got: {}",
        location
    );
}

#[test]
fn t03b_login_json_wrong_password_401() {
    let env = setup_test_env("t03b");
    seed_user(&env.site_dir, "dave", "right_pass");
    let _guard = spawn_server(&env);

    let body = r#"{"username":"dave","password":"wrong_pass"}"#;
    let resp = http_request_with_body(
        &env.socket_path,
        "POST",
        "/auth/login",
        "application/json",
        body,
    );

    assert_eq!(parse_status(&resp), 401, "expected 401\n{}", resp);
}

/// Login: 6th attempt within 15 minutes → 429 with Retry-After.
#[test]
fn t04_login_rate_limited_429() {
    let env = setup_test_env("t04");
    // No user needed — rate limit checked before credential verification
    let _guard = spawn_server(&env);

    let body = r#"{"username":"nobody","password":"bad"}"#;

    // First 5 attempts (fail with 401)
    for _ in 0..5 {
        let resp = http_request_with_body(
            &env.socket_path,
            "POST",
            "/auth/login",
            "application/json",
            body,
        );
        let status = parse_status(&resp);
        assert!(
            status == 401 || status == 429,
            "expected 401 or 429, got {}",
            status
        );
        if status == 429 {
            // We already got rate-limited early, test passes
            let retry_after = get_header(&resp, "Retry-After").unwrap_or("");
            assert!(!retry_after.is_empty(), "expected Retry-After header");
            return;
        }
    }

    // 6th attempt must be 429
    let resp = http_request_with_body(
        &env.socket_path,
        "POST",
        "/auth/login",
        "application/json",
        body,
    );
    assert_eq!(
        parse_status(&resp),
        429,
        "6th attempt should be rate-limited\n{}",
        resp
    );
    let retry_after = get_header(&resp, "Retry-After").unwrap_or("");
    assert!(
        !retry_after.is_empty(),
        "expected Retry-After header on 429"
    );
}

/// Append a `[rate_limit]` table with `max_attempts = n` to the test config.
///
/// Call it before `spawn_server`. `setup_test_env` writes no `[rate_limit]`
/// table, so appending one is valid TOML and the server reads it at start.
fn limit_failures_to(env: &TestEnv, n: u32) {
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&env.config_path)
        .expect("open the test config for appending");
    writeln!(file, "\n[rate_limit]\nmax_attempts = {n}").expect("append the rate limit table");
}

/// Send one wrong JSON login carrying the extra header lines in `forwarded`,
/// and return its status. `forwarded` is each header line ending in `\r\n`, or
/// an empty string for none.
fn failed_login(socket: &Path, forwarded: &str) -> u16 {
    let body = r#"{"username":"nobody","password":"bad"}"#;
    let req = format!(
        "POST /auth/login HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\n{}Content-Length: {}\r\n\r\n{}",
        forwarded,
        body.len(),
        body,
    );
    let resp = http_request(socket, &req);
    parse_status(&resp)
}

/// Each forwarded address is its own rate-limit bucket.
///
/// Two failures from `192.0.2.10` use that address's budget and the third is
/// blocked. A second `X-Forwarded-For` address is not blocked. `X-Real-IP` is
/// read when `X-Forwarded-For` is absent, and `X-Forwarded-For` wins when both
/// are present.
#[test]
fn forwarded_addresses_select_the_rate_limit_bucket() {
    let env = setup_test_env("rate-limit-bucket");
    limit_failures_to(&env, 2);
    let _guard = spawn_server(&env);
    let socket = &env.socket_path;

    assert_eq!(
        failed_login(socket, "X-Forwarded-For: 192.0.2.10\r\n"),
        401,
        "X-Forwarded-For: 192.0.2.10, first failure, expected 401"
    );
    assert_eq!(
        failed_login(socket, "X-Forwarded-For: 192.0.2.10\r\n"),
        401,
        "X-Forwarded-For: 192.0.2.10, second failure, expected 401"
    );
    assert_eq!(
        failed_login(socket, "X-Forwarded-For: 192.0.2.10\r\n"),
        429,
        "X-Forwarded-For: 192.0.2.10, third failure, expected 429"
    );
    assert_eq!(
        failed_login(socket, "X-Forwarded-For: 192.0.2.11\r\n"),
        401,
        "X-Forwarded-For: 192.0.2.11, a second address is not blocked, expected 401"
    );

    assert_eq!(
        failed_login(socket, "X-Real-IP: 192.0.2.12\r\n"),
        401,
        "X-Real-IP: 192.0.2.12, first failure, expected 401"
    );
    assert_eq!(
        failed_login(socket, "X-Real-IP: 192.0.2.12\r\n"),
        401,
        "X-Real-IP: 192.0.2.12, second failure, expected 401"
    );
    assert_eq!(
        failed_login(socket, "X-Real-IP: 192.0.2.12\r\n"),
        429,
        "X-Real-IP: 192.0.2.12, third failure, expected 429"
    );

    assert_eq!(
        failed_login(
            socket,
            "X-Forwarded-For: 192.0.2.11\r\nX-Real-IP: 192.0.2.10\r\n"
        ),
        401,
        "X-Forwarded-For wins over X-Real-IP: 192.0.2.11 is not blocked, expected 401"
    );
}

/// Callers that send neither header count against the one `unix` bucket.
///
/// Two failures with no forwarded header use the bucket and a third is
/// blocked. A forwarded address has its own bucket and is not blocked by them.
#[test]
fn requests_without_a_forwarded_address_share_one_bucket() {
    let env = setup_test_env("rate-limit-unix-bucket");
    limit_failures_to(&env, 2);
    let _guard = spawn_server(&env);
    let socket = &env.socket_path;

    assert_eq!(
        failed_login(socket, ""),
        401,
        "no header, first failure, expected 401"
    );
    assert_eq!(
        failed_login(socket, ""),
        401,
        "no header, second failure, expected 401"
    );
    assert_eq!(
        failed_login(socket, ""),
        429,
        "no header, third failure, expected 429"
    );
    assert_eq!(
        failed_login(socket, "X-Forwarded-For: 192.0.2.20\r\n"),
        401,
        "X-Forwarded-For: 192.0.2.20, a forwarded address is not blocked by the unix bucket, expected 401"
    );
}

/// A restart forgets the failure counts, because the counts live in memory.
///
/// The 429 before the restart proves the address really was blocked, so the
/// 401 after it means the count was gone rather than never counted.
#[test]
fn a_restart_forgets_the_failure_counts() {
    let env = setup_test_env("rate-limit-restart");
    limit_failures_to(&env, 2);
    let mut svc = spawn_server(&env);
    let socket = &env.socket_path;

    assert_eq!(
        failed_login(socket, "X-Forwarded-For: 192.0.2.30\r\n"),
        401,
        "X-Forwarded-For: 192.0.2.30, first failure, expected 401"
    );
    assert_eq!(
        failed_login(socket, "X-Forwarded-For: 192.0.2.30\r\n"),
        401,
        "X-Forwarded-For: 192.0.2.30, second failure, expected 401"
    );
    assert_eq!(
        failed_login(socket, "X-Forwarded-For: 192.0.2.30\r\n"),
        429,
        "X-Forwarded-For: 192.0.2.30, third failure, expected 429"
    );

    assert!(
        svc.terminate(Duration::from_secs(10)).success(),
        "the service did not exit cleanly on SIGTERM\n--- output ---\n{}",
        svc.output()
    );

    let _guard = spawn_server(&env);
    assert_eq!(
        failed_login(socket, "X-Forwarded-For: 192.0.2.30\r\n"),
        401,
        "X-Forwarded-For: 192.0.2.30, after restart, expected 401"
    );
}

/// Refresh: valid refresh cookie → 302, new session cookie.
#[test]
fn t05_refresh_valid_cookie_302_new_session() {
    let env = setup_test_env("t05");
    seed_user(&env.site_dir, "eve", "evepw");
    let _guard = spawn_server(&env);

    // Login to get tokens
    let body = r#"{"username":"eve","password":"evepw"}"#;
    let login_resp = http_request_with_body(
        &env.socket_path,
        "POST",
        "/auth/login",
        "application/json",
        body,
    );
    assert_eq!(parse_status(&login_resp), 200);

    let body_start = login_resp
        .find("\r\n\r\n")
        .map(|i| i + 4)
        .unwrap_or(login_resp.len());
    let json: serde_json::Value = serde_json::from_str(&login_resp[body_start..]).unwrap();
    let refresh_token = json["refresh_token"].as_str().unwrap();

    // Use the refresh token via cookie
    let req = format!(
        "POST /auth/refresh HTTP/1.1\r\nHost: localhost\r\nCookie: refresh={}\r\nContent-Length: 0\r\n\r\n",
        refresh_token
    );
    let resp = http_request(&env.socket_path, &req);

    assert_eq!(
        parse_status(&resp),
        302,
        "expected 302 on refresh\n{}",
        resp
    );

    let cookies = get_all_headers(&resp, "Set-Cookie");
    let has_session = cookies
        .iter()
        .any(|c| c.starts_with("session=") && c.contains("HttpOnly"));
    assert!(
        has_session,
        "expected new session cookie after refresh\ncookies: {:?}",
        cookies
    );
}

/// Refresh: expired/invalid token → 302 /login.
#[test]
fn t06_refresh_invalid_token_302_login() {
    let env = setup_test_env("t06");
    let _guard = spawn_server(&env);

    let req = "POST /auth/refresh HTTP/1.1\r\nHost: localhost\r\nCookie: refresh=bogus_invalid_token\r\nContent-Length: 0\r\n\r\n";
    let resp = http_request(&env.socket_path, req);

    assert_eq!(parse_status(&resp), 302, "expected 302\n{}", resp);
    let location = get_header(&resp, "Location").unwrap_or("");
    assert_eq!(
        location, "/login",
        "expected redirect to /login, got {}",
        location
    );
}

/// Logout: clears both cookies (Max-Age=0).
#[test]
fn t07_logout_clears_cookies() {
    let env = setup_test_env("t07");
    seed_user(&env.site_dir, "frank", "frankpw");
    let _guard = spawn_server(&env);

    // Login via form to get cookies
    let body = "username=frank&password=frankpw";
    let login_resp = http_request_with_body(
        &env.socket_path,
        "POST",
        "/auth/login",
        "application/x-www-form-urlencoded",
        body,
    );
    assert_eq!(parse_status(&login_resp), 302);

    // Extract refresh cookie value
    let cookies = get_all_headers(&login_resp, "Set-Cookie");
    let refresh_cookie = cookies
        .iter()
        .find(|c| c.starts_with("refresh="))
        .map(|c| {
            let token_part = c.split(';').next().unwrap_or("");
            token_part.trim_start_matches("refresh=").to_string()
        })
        .unwrap_or_default();

    // Logout
    let req = format!(
        "POST /auth/logout HTTP/1.1\r\nHost: localhost\r\nCookie: refresh={}\r\nContent-Length: 0\r\n\r\n",
        refresh_cookie
    );
    let logout_resp = http_request(&env.socket_path, &req);
    assert_eq!(
        parse_status(&logout_resp),
        302,
        "expected 302 on logout\n{}",
        logout_resp
    );

    let logout_cookies = get_all_headers(&logout_resp, "Set-Cookie");
    let session_cleared = logout_cookies
        .iter()
        .any(|c| c.contains("session=") && c.contains("Max-Age=0"));
    let refresh_cleared = logout_cookies
        .iter()
        .any(|c| c.contains("refresh=") && c.contains("Max-Age=0"));
    assert!(
        session_cleared,
        "session cookie not cleared (Max-Age=0)\ncookies: {:?}",
        logout_cookies
    );
    assert!(
        refresh_cleared,
        "refresh cookie not cleared (Max-Age=0)\ncookies: {:?}",
        logout_cookies
    );
}

/// Public key endpoint returns valid PEM.
#[test]
fn t08_public_key_returns_pem() {
    let env = setup_test_env("t08");
    let _guard = spawn_server(&env);

    let req = "GET /auth/public-key HTTP/1.1\r\nHost: localhost\r\n\r\n";
    let resp = http_request(&env.socket_path, req);

    assert_eq!(parse_status(&resp), 200, "expected 200\n{}", resp);
    assert!(
        resp.contains("BEGIN PUBLIC KEY") || resp.contains("BEGIN RSA PUBLIC KEY"),
        "expected PEM public key in response\n{}",
        resp
    );
}

/// JWT signature verifiable with returned public key.
#[test]
fn t09_jwt_verifiable_with_public_key() {
    let env = setup_test_env("t09");
    seed_user(&env.site_dir, "grace", "gracepw");
    let _guard = spawn_server(&env);

    // Get access token
    let body = r#"{"username":"grace","password":"gracepw"}"#;
    let login_resp = http_request_with_body(
        &env.socket_path,
        "POST",
        "/auth/login",
        "application/json",
        body,
    );
    assert_eq!(parse_status(&login_resp), 200);

    let body_start = login_resp
        .find("\r\n\r\n")
        .map(|i| i + 4)
        .unwrap_or(login_resp.len());
    let json: serde_json::Value = serde_json::from_str(&login_resp[body_start..]).unwrap();
    let access_token = json["access_token"].as_str().unwrap();

    // Get public key
    let req = "GET /auth/public-key HTTP/1.1\r\nHost: localhost\r\n\r\n";
    let pk_resp = http_request(&env.socket_path, req);
    let pk_body_start = pk_resp
        .find("\r\n\r\n")
        .map(|i| i + 4)
        .unwrap_or(pk_resp.len());
    let public_pem = &pk_resp[pk_body_start..];

    // Verify the JWT using jsonwebtoken directly
    let decoding_key = jsonwebtoken::DecodingKey::from_rsa_pem(public_pem.as_bytes())
        .expect("parse public key from PEM");
    let mut validation = jsonwebtoken::Validation::new(jsonwebtoken::Algorithm::RS256);
    validation.set_issuer(&["test.example.com"]);

    let result =
        jsonwebtoken::decode::<serde_json::Value>(access_token, &decoding_key, &validation);
    assert!(
        result.is_ok(),
        "JWT verification failed: {:?}",
        result.err()
    );
}

/// `next` parameter with external URL → falls back to `/`.
#[test]
fn t10_next_external_url_falls_back_to_root() {
    let env = setup_test_env("t10");
    seed_user(&env.site_dir, "henry", "henrypw");
    let _guard = spawn_server(&env);

    // Attempt to redirect to external URL
    let body = "username=henry&password=henrypw&next=https://evil.example.com/steal";
    let resp = http_request_with_body(
        &env.socket_path,
        "POST",
        "/auth/login",
        "application/x-www-form-urlencoded",
        body,
    );

    assert_eq!(parse_status(&resp), 302, "expected 302\n{}", resp);
    let location = get_header(&resp, "Location").unwrap_or("");
    assert_eq!(
        location, "/",
        "external URL should fall back to /, got: {}",
        location
    );
}

// ─── Shutdown ─────────────────────────────────────────────────────────────────

/// SIGTERM must run the shutdown path, not kill the process, and the socket
/// must be gone afterwards.
///
/// One shutdown sequence lives in `m6_core::signal`, so this test is the same
/// in every service that has one: m6-file, m6-html, m6-http and here. Before
/// that unification this service had neither behaviour. It was killed by the
/// signal, and it left its socket file behind for m6-http to keep in its
/// backend pool.
#[test]
fn sigterm_shuts_down_and_removes_the_socket() {
    let env = setup_test_env("shutdown");

    // The whole lifecycle contract in one call: exit 0 rather than a signal
    // status, the socket removed so m6-http does not keep a dead member in its
    // backend pool, and the three lines logged under the right service name.
    //
    // This used to be written out by hand here, as it was in five other
    // suites, which is how the sixth service ended up with no lifecycle test
    // at all. It is core's now.
    assert_app_lifecycle(
        "m6-auth-server",
        Command::new(binary("m6-auth-server"))
            .arg(&env.site_dir)
            .arg(&env.config_path)
            .env("M6_SOCKET_OVERRIDE", &env.socket_path),
        &env.socket_path,
    );
}

/// With no `[tokens] issuer`, the issuer comes from `[site] domain` in the
/// `site.toml` that the `site_toml` key names, which need not sit in `site_dir`.
#[test]
fn issuer_falls_back_to_the_configured_site_toml() {
    let env = setup_test_env("issuer-site-toml");

    let site_toml = env._key_dir.path().join("site.toml");
    std::fs::write(&site_toml, "[site]\ndomain = \"site.example.com\"\n").unwrap();

    let config = std::fs::read_to_string(&env.config_path).unwrap();
    assert!(config.contains("issuer      = \"test.example.com\"\n"));
    let config = config.replace("issuer      = \"test.example.com\"\n", "");
    let config = format!("site_toml = \"{}\"\n{config}", site_toml.display());
    std::fs::write(&env.config_path, config).unwrap();

    seed_user(&env.site_dir, "ivy", "ivypw");
    let _guard = spawn_server(&env);

    let body = r#"{"username":"ivy","password":"ivypw"}"#;
    let login_resp = http_request_with_body(
        &env.socket_path,
        "POST",
        "/auth/login",
        "application/json",
        body,
    );
    assert_eq!(parse_status(&login_resp), 200, "{login_resp}");
    let body_start = login_resp.find("\r\n\r\n").map(|i| i + 4).unwrap();
    let json: serde_json::Value = serde_json::from_str(&login_resp[body_start..]).unwrap();
    let access_token = json["access_token"].as_str().unwrap();

    let req = "GET /auth/public-key HTTP/1.1\r\nHost: localhost\r\n\r\n";
    let pk_resp = http_request(&env.socket_path, req);
    let pk_start = pk_resp.find("\r\n\r\n").map(|i| i + 4).unwrap();
    let decoding_key =
        jsonwebtoken::DecodingKey::from_rsa_pem(pk_resp.split_at(pk_start).1.as_bytes())
            .expect("parse public key from PEM");
    let mut validation = jsonwebtoken::Validation::new(jsonwebtoken::Algorithm::RS256);
    validation.set_issuer(&["site.example.com"]);
    let result =
        jsonwebtoken::decode::<serde_json::Value>(access_token, &decoding_key, &validation);
    assert!(
        result.is_ok(),
        "issuer was not site.example.com: {:?}",
        result.err()
    );
}

/// The body of `GET /auth/public-key`.
fn public_key_body(env: &TestEnv) -> String {
    let req = "GET /auth/public-key HTTP/1.1\r\nHost: localhost\r\n\r\n";
    let resp = http_request(&env.socket_path, req);
    let at = resp.find("\r\n\r\n").map(|i| i + 4).unwrap_or(0);
    resp[at..].to_string()
}

/// A rotation is: write both key files, then restart the service. The key
/// files are read once at start.
#[test]
fn a_rotated_signing_key_signs_after_a_restart() {
    let env = setup_test_env("rotate");
    let mut guard = spawn_server(&env);
    let key_a = public_key_body(&env);
    assert!(key_a.contains("PUBLIC KEY"), "{key_a}");

    let (private_b, public_b) = generate_test_rsa_keys();
    assert_ne!(public_b, key_a);
    std::fs::write(env._key_dir.path().join("auth.pem"), &private_b).unwrap();
    std::fs::write(env._key_dir.path().join("auth.pub"), &public_b).unwrap();

    // Restart on the same config: the new pair is read at start.
    let status = guard.terminate(Duration::from_secs(5));
    assert!(
        status.success(),
        "m6-auth-server did not stop cleanly, got {status}"
    );
    let guard = spawn_server(&env);

    assert_eq!(
        public_key_body(&env),
        public_b,
        "the new key was not served after a restart\n--- output ---\n{}",
        guard.output()
    );
}

/// A private key beside a public key from a different pair is refused at start
/// with exit 2, so a rotation caught between its two writes never serves.
#[test]
fn a_mismatched_key_pair_at_start_exits_2() {
    let env = setup_test_env("rotate-mismatch");
    // A fresh private key, while the public key setup_test_env wrote stays
    // from the first pair, so the two do not match.
    let (private_b, _public_b) = generate_test_rsa_keys();
    std::fs::write(env._key_dir.path().join("auth.pem"), &private_b).unwrap();

    let output = start_failure(&env, "a mismatched key pair");
    assert!(
        output.contains("not a pair"),
        "the error must say the keys are not a pair\n--- output ---\n{output}"
    );
}

// ─── Database ────────────────────────────────────────────────────────────────

/// Four processes open the one empty database at start. The schema batch runs
/// in every open, so this asserts that every instance starts, that the result
/// is one whole schema, and that every instance's connection can use it.
#[test]
fn instances_opening_an_empty_database_at_once_produce_one_schema() {
    let env = setup_test_env("db-concurrent-open");
    let db_path = env.site_dir.join("data/auth.db");

    const INSTANCES: usize = 4;
    let sockets: Vec<PathBuf> = (0..INSTANCES)
        .map(|i| env.site_dir.join(format!("instance-{i}.sock")))
        .collect();

    let mut instances = spawn_instances("m6-auth-server", INSTANCES, |i| {
        let mut cmd = Command::new(binary("m6-auth-server"));
        cmd.arg(&env.site_dir)
            .arg(&env.config_path)
            .env("M6_SOCKET_OVERRIDE", &sockets[i]);
        cmd
    });
    for (svc, socket) in instances.iter_mut().zip(&sockets) {
        wait_until_serving(svc, socket);
    }

    // One whole schema: a user created through another connection is visible
    // to every instance's login below.
    seed_user(&env.site_dir, "alice", "correct_pass");

    let login_body = r#"{"username":"alice","password":"correct_pass"}"#;
    for socket in &sockets {
        let resp = http_request_with_body(
            socket,
            "POST",
            "/auth/login",
            "application/json",
            login_body,
        );
        assert_eq!(
            parse_status(&resp),
            200,
            "an instance could not read the schema at {}\n{resp}",
            socket.display()
        );
    }

    // One schema, whole: exactly the five tables the batch creates, in WAL.
    let conn =
        rusqlite::Connection::open(&db_path).expect("open the database for the schema check");
    let tables: rusqlite::Result<Vec<String>> = {
        let mut stmt = conn
            .prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
            .expect("prepare the schema query");
        let rows = stmt
            .query_map([], |row| row.get(0))
            .expect("query the schema");
        rows.collect()
    };
    assert_eq!(
        tables.expect("read the table names").join(","),
        "api_tokens,groups,memberships,refresh_tokens,users",
        "the schema batch did not land once and whole"
    );
    let journal_mode: String = conn
        .query_row("PRAGMA journal_mode", [], |row| row.get(0))
        .expect("read the journal mode");
    assert_eq!(journal_mode, "wal", "a connection is not in WAL mode");
}

/// A database that cannot be opened stops the service at start: exit 2, an
/// error naming the path, and no socket file.
#[test]
fn a_database_that_cannot_be_opened_stops_the_service_at_start() {
    let env = setup_test_env("db-open-fails");
    let db_path = env.site_dir.join("data/auth.db");
    std::fs::create_dir(&db_path).expect("put a directory where the database was");

    let output = start_failure(&env, "a database that cannot be opened");
    assert!(
        output.contains(&db_path.display().to_string()),
        "the error must name the database path\n--- output ---\n{output}"
    );
    assert!(
        !env.socket_path.exists(),
        "the socket was created before the database open failed"
    );
}

/// The database's directory cannot be created because a regular file sits at
/// its path, so the service exits 2 and the error names the directory.
///
/// The database that cannot be opened test fails inside `Db::open`. This one
/// fails earlier, in the `create_dir_all` that makes `[storage] path`'s parent,
/// which is the branch whose message names the directory.
#[test]
fn a_storage_directory_that_cannot_be_created_names_the_path() {
    let env = setup_test_env("db-dir-is-a-file");
    let data_dir = env.site_dir.join("data");
    std::fs::remove_dir(&data_dir).expect("remove the data directory");
    std::fs::write(&data_dir, b"not a directory").expect("put a file where the data directory was");

    let output = start_failure(&env, "a storage directory that cannot be created");
    assert!(
        output.contains(&data_dir.display().to_string()),
        "the error must name the database directory\n--- output ---\n{output}"
    );
}

// ─── A locked database ───────────────────────────────────────────────────────

/// A login whose refresh-token write meets another process's write lock is
/// deferred, and the loop answers another connection while it waits. With the
/// five-second busy timeout in the loop the login holds the whole process and
/// the public-key request misses its one-second deadline.
#[test]
fn a_locked_database_defers_and_the_service_keeps_serving() {
    let env = setup_test_env("db-busy-defers");
    seed_user(&env.site_dir, "alice", "correct_pass");
    let _guard = spawn_server(&env);
    let lock = hold_write_lock(&env.site_dir.join("data/auth.db"));

    // One connection throughout, and a request before the locked one: the
    // service has answered on this connection, so the login's bytes reach a
    // loop that is already waiting for them.
    let mut login = UnixStream::connect(&env.socket_path).expect("connect the login");
    login
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("set the login's read timeout");
    login
        .write_all(b"GET /auth/public-key HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .expect("send the public-key request");
    let warm = m6_core::testkit::read_one(&mut login, "GET").expect("the public key");
    assert_eq!(
        parse_status(&String::from_utf8_lossy(&warm)),
        200,
        "the warm-up request must be answered"
    );

    let body = r#"{"username":"alice","password":"correct_pass"}"#;
    login
        .write_all(login_json_request(body).as_bytes())
        .expect("send the login");

    // The login's refresh-token store meets the lock. While it is deferred,
    // another connection must be answered: a read deadline of one second
    // fails the old five-second busy wait in the loop.
    let public_key = http_request_within(
        &env.socket_path,
        "GET /auth/public-key HTTP/1.1\r\nHost: localhost\r\n\r\n",
        Duration::from_secs(1),
    );
    assert_eq!(
        parse_status(&public_key),
        200,
        "the public key must answer while the login is deferred\n{public_key}"
    );

    lock.execute_batch("COMMIT").expect("release the lock");

    // The retry happens when the deferred request's five-millisecond deadline
    // passes, and the answer must arrive within two seconds of the release.
    login
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("set the login's deadline");
    let answer = m6_core::testkit::read_one(&mut login, "POST").expect("the login's answer");
    let answer = String::from_utf8_lossy(&answer);
    assert_eq!(
        parse_status(&answer),
        200,
        "the deferred login must answer once the lock is released\n{answer}"
    );
    let body_start = answer
        .find("\r\n\r\n")
        .map(|i| i + 4)
        .unwrap_or(answer.len());
    let json: serde_json::Value =
        serde_json::from_str(&answer[body_start..]).expect("the login's JSON body");
    assert!(
        json["access_token"].is_string(),
        "the answer has no access token\n{answer}"
    );
    assert!(
        json["refresh_token"].is_string(),
        "the answer has no refresh token\n{answer}"
    );
}

/// A login that meets the lock for over a second is answered with 503 and a
/// one-second `Retry-After`. The test releases the lock only after it has the
/// 503, so the service must give up on its own rather than wait forever.
#[test]
fn a_database_locked_past_a_second_answers_503() {
    let env = setup_test_env("db-busy-503");
    seed_user(&env.site_dir, "alice", "correct_pass");
    let _guard = spawn_server(&env);
    let lock = hold_write_lock(&env.site_dir.join("data/auth.db"));

    let body = r#"{"username":"alice","password":"correct_pass"}"#;
    let answer = http_request_within(
        &env.socket_path,
        &login_json_request(body),
        Duration::from_secs(3),
    );
    assert_eq!(
        parse_status(&answer),
        503,
        "a lock held past a second must answer 503\n{answer}"
    );
    assert_eq!(
        get_header(&answer, "Retry-After"),
        Some("1"),
        "the 503 must carry a one-second Retry-After\n{answer}"
    );

    lock.execute_batch("COMMIT").expect("release the lock");
}
