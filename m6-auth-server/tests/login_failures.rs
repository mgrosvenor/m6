//! Login-failure responses, asserted at the wire.
//!
//! Two behaviours the issue #329 test list found untested: a failed login sets
//! no `Set-Cookie`, and an unknown user is answered exactly like a wrong
//! password so the response does not say which of the two failed. Each test
//! below states, in its name and the comment above it, the scenario and the
//! wrong outcome it catches.

#![allow(
    clippy::disallowed_methods,
    reason = "test code: threads, sleeps and blocking clients drive the service from outside"
)]

use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use m6_core::testkit::{binary, wait, Service};

// ─── Test fixtures ────────────────────────────────────────────────────────────

/// Generate an RSA key pair (2048-bit) for testing.
/// Returns (private_pem, public_pem).
fn generate_test_rsa_keys() -> (String, String) {
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

    let (private_pem, public_pem) = generate_test_rsa_keys();
    let private_key_path = key_dir.path().join("auth.pem");
    let public_key_path = key_dir.path().join("auth.pub");
    std::fs::write(&private_key_path, &private_pem).unwrap();
    std::fs::write(&public_key_path, &public_pem).unwrap();

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

    std::fs::create_dir_all(site_dir.join("data")).unwrap();
    std::fs::write(site_dir.join("site.toml"), "").unwrap();

    // The socket lives in this test's own temp directory, not a shared one.
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
///
/// Readiness is a successful connect, not an existing path: `bind` creates the
/// socket file and `listen` comes after, so the file appearing proves nothing.
fn spawn_server(env: &TestEnv) -> Service {
    let mut svc = Service::spawn(
        "m6-auth-server",
        Command::new(binary("m6-auth-server"))
            .arg(&env.site_dir)
            .arg(&env.config_path)
            .env("M6_SOCKET_OVERRIDE", &env.socket_path),
    );
    svc.wait_for_path(&env.socket_path, Duration::from_secs(10));
    assert!(
        wait::for_unix(&env.socket_path, Duration::from_secs(10)),
        "m6-auth-server created {} but never accepted a connection\n--- output ---\n{}",
        env.socket_path.display(),
        svc.output()
    );
    svc
}

/// Create a test user via the database before the server starts.
fn seed_user(site_dir: &Path, username: &str, password: &str) {
    let db_path = site_dir.join("data/auth.db");
    std::fs::create_dir_all(db_path.parent().unwrap()).unwrap();
    let db = m6_auth::Db::open(&db_path).expect("open db");
    db.user_create(username, password, &["user"])
        .expect("create user");
}

/// Send a raw HTTP request over a Unix socket and return the full response bytes.
fn http_request(socket_path: &Path, request: &str) -> String {
    let mut stream = UnixStream::connect(socket_path)
        .unwrap_or_else(|e| panic!("connect {:?}: {}", socket_path, e));
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    stream.write_all(request.as_bytes()).unwrap();
    // One response, not read-to-EOF: the connection stays open (RFC 9112 9.3).
    let method = request.split(' ').next().unwrap_or("");
    let resp = m6_core::testkit::read_one(&mut stream, method).expect("read one response");
    String::from_utf8_lossy(&resp).into_owned()
}

fn http_request_with_body(
    socket_path: &Path,
    method: &str,
    path: &str,
    content_type: &str,
    body: &str,
) -> String {
    let req = format!(
        "{} {} HTTP/1.1\r\nHost: localhost\r\nContent-Type: {}\r\nContent-Length: {}\r\n\r\n{}",
        method,
        path,
        content_type,
        body.len(),
        body
    );
    http_request(socket_path, &req)
}

fn parse_status(resp: &str) -> u16 {
    resp.split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
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

/// A wrong password on the form path and on the JSON path sets no `Set-Cookie`.
///
/// The wrong outcome this catches: a failed login that still mints and sends a
/// session or refresh cookie, which would hand a client that did not
/// authenticate a cookie it could then present.
#[test]
fn failed_form_and_json_login_sets_no_set_cookie() {
    let env = setup_test_env("no-cookie");
    seed_user(&env.site_dir, "alice", "correct_pass");
    let _guard = spawn_server(&env);

    let form = http_request_with_body(
        &env.socket_path,
        "POST",
        "/auth/login",
        "application/x-www-form-urlencoded",
        "username=alice&password=wrong_pass",
    );
    assert_eq!(parse_status(&form), 302, "expected 302\n{form}");
    let cookies = get_all_headers(&form, "Set-Cookie");
    assert!(
        cookies.is_empty(),
        "a failed form login set a cookie: {cookies:?}\n{form}"
    );

    let json = http_request_with_body(
        &env.socket_path,
        "POST",
        "/auth/login",
        "application/json",
        r#"{"username":"alice","password":"wrong_pass"}"#,
    );
    assert_eq!(parse_status(&json), 401, "expected 401\n{json}");
    let cookies = get_all_headers(&json, "Set-Cookie");
    assert!(
        cookies.is_empty(),
        "a failed JSON login set a cookie: {cookies:?}\n{json}"
    );
}

/// An unknown user and a real user with a wrong password receive the same
/// status, headers and body, on both the form and JSON paths.
///
/// The wrong outcome this catches: `user_verify_password` (or the handler)
/// answering the two differently, so a client can tell from the response
/// whether an account exists.
#[test]
fn unknown_user_and_wrong_password_receive_identical_responses() {
    let env = setup_test_env("no-enumeration");
    seed_user(&env.site_dir, "alice", "correct_pass");
    let _guard = spawn_server(&env);

    // Form: the same 302 to the login error page, whether the user is unknown
    // or the password is wrong.
    let form_wrong = http_request_with_body(
        &env.socket_path,
        "POST",
        "/auth/login",
        "application/x-www-form-urlencoded",
        "username=alice&password=wrong_pass",
    );
    let form_ghost = http_request_with_body(
        &env.socket_path,
        "POST",
        "/auth/login",
        "application/x-www-form-urlencoded",
        "username=ghost&password=wrong_pass",
    );
    assert_eq!(parse_status(&form_wrong), 302, "expected 302\n{form_wrong}");
    assert_eq!(
        form_ghost, form_wrong,
        "the form answer names which failed:\nwrong password:\n{form_wrong}\nunknown user:\n{form_ghost}"
    );

    // JSON: the same 401 and body.
    let json_wrong = http_request_with_body(
        &env.socket_path,
        "POST",
        "/auth/login",
        "application/json",
        r#"{"username":"alice","password":"wrong_pass"}"#,
    );
    let json_ghost = http_request_with_body(
        &env.socket_path,
        "POST",
        "/auth/login",
        "application/json",
        r#"{"username":"ghost","password":"wrong_pass"}"#,
    );
    assert_eq!(parse_status(&json_wrong), 401, "expected 401\n{json_wrong}");
    assert_eq!(
        json_ghost, json_wrong,
        "the JSON answer names which failed:\nwrong password:\n{json_wrong}\nunknown user:\n{json_ghost}"
    );
}
