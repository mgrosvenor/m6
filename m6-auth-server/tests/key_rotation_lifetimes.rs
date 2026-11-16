//! Key-rotation token lifetimes.
//!
//! The rotation mechanics (a key is served only after a restart, a bad key is
//! refused, a mismatched pair is refused) are covered in `integration.rs`.
//! These tests cover what a rotation does to tokens already issued and to
//! tokens issued afterwards, per `keys.rs`: a refresh token is verified against
//! the database hash, not re-validated against the current signing key, so a
//! token minted under the old key stays usable, a token minted under the new
//! key works at once, and a token under a third key is rejected.

#![allow(
    clippy::disallowed_methods,
    reason = "test code: threads, sleeps and blocking clients drive the service from outside"
)]

use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use m6_core::testkit::{binary, read_response, wait, Service, RESPONSE_BOUND};

const ISSUER: &str = "test.example.com";

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
issuer      = "{}"

[keys]
private_key = "{}"
public_key  = "{}"
"#,
        db_path,
        ISSUER,
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
fn spawn_server(env: &TestEnv) -> Service {
    let mut svc = Service::spawn(
        "m6-auth-server",
        Command::new(binary("m6-auth-server"))
            .arg(&env.site_dir)
            .arg(&env.config_path)
            .env("M6_SOCKET_OVERRIDE", &env.socket_path),
    );
    svc.wait_for_path(&env.socket_path, m6_core::testkit::STARTUP_BOUND);
    svc.wait_for_started(m6_core::testkit::STARTUP_BOUND);
    assert!(
        wait::for_unix(&env.socket_path, m6_core::testkit::STARTUP_BOUND),
        "m6-auth-server created {} but never accepted a connection\n--- output ---\n{}",
        env.socket_path.display(),
        svc.output()
    );
    svc
}

/// Create a test user and return it, so a test can read its id.
///
/// Cost 4 is bcrypt's lowest: a fixture needs a user, not production's round
/// count, and an unoptimised cost-12 hash costs seconds per test.
fn seed_user(site_dir: &Path, username: &str, password: &str) -> m6_auth::User {
    let db_path = site_dir.join("data/auth.db");
    std::fs::create_dir_all(db_path.parent().unwrap()).unwrap();
    let db = m6_auth::Db::open(&db_path).expect("open db");
    db.user_create_with_cost(username, password, &["user"], 4)
        .expect("create user")
}

/// Send a raw HTTP request over a Unix socket and return the full response bytes.
/// Send a request and wait for the response event or the service's exit.
///
/// [`RESPONSE_BOUND`] is the safety net above the work, not a read deadline
/// guessed at it.
fn http_request(svc: &mut Service, socket_path: &Path, request: &str) -> String {
    let mut stream = UnixStream::connect(socket_path)
        .unwrap_or_else(|e| panic!("connect {:?}: {}", socket_path, e));
    stream.write_all(request.as_bytes()).unwrap();
    let method = request.split(' ').next().unwrap_or("");
    let resp = read_response(svc, stream, method, RESPONSE_BOUND).unwrap_or_else(|e| panic!("{e}"));
    String::from_utf8_lossy(&resp).into_owned()
}

fn http_request_with_body(
    svc: &mut Service,
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
    http_request(svc, socket_path, &req)
}

fn parse_status(resp: &str) -> u16 {
    resp.split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

/// Log in over JSON and return `(access_token, refresh_token)`.
fn login_json(
    svc: &mut Service,
    socket_path: &Path,
    username: &str,
    password: &str,
) -> (String, String) {
    let body = serde_json::json!({ "username": username, "password": password }).to_string();
    let resp = http_request_with_body(
        svc,
        socket_path,
        "POST",
        "/auth/login",
        "application/json",
        &body,
    );
    assert_eq!(parse_status(&resp), 200, "login failed\n{resp}");
    let body_start = resp.find("\r\n\r\n").map(|i| i + 4).unwrap_or(resp.len());
    let json: serde_json::Value = serde_json::from_str(&resp[body_start..]).unwrap();
    let access = json["access_token"].as_str().unwrap().to_string();
    let refresh = json["refresh_token"].as_str().unwrap().to_string();
    (access, refresh)
}

/// Refresh a token over JSON and return the response.
fn refresh_json(svc: &mut Service, socket_path: &Path, refresh_token: &str) -> String {
    let body = serde_json::json!({ "refresh_token": refresh_token }).to_string();
    http_request_with_body(
        svc,
        socket_path,
        "POST",
        "/auth/refresh",
        "application/json",
        &body,
    )
}

/// Mint a refresh token with an arbitrary key pair, for the foreign-key case.
fn mint_refresh_token(
    private_pem: &str,
    public_pem: &str,
    issuer: &str,
    user: &m6_auth::User,
) -> String {
    let engine = m6_auth::jwt::JwtEngine::new(private_pem, public_pem, issuer.to_string())
        .expect("build engine");
    let now = m6_auth::jwt::now_secs();
    let claims = m6_auth::jwt::RefreshClaims {
        iss: issuer.to_string(),
        sub: user.id.clone(),
        exp: now + 3600,
        iat: now,
        username: user.username.clone(),
        token_type: "refresh".to_string(),
    };
    engine.encode_refresh(&claims).expect("encode")
}

/// Rotate the signing key: write both key files, restart the service, and
/// return the new service handle. The public key endpoint then serves the new
/// key.
fn rotate_key(env: &TestEnv, mut guard: Service) -> Service {
    let (private_b, public_b) = generate_test_rsa_keys();
    std::fs::write(env._key_dir.path().join("auth.pem"), &private_b).unwrap();
    std::fs::write(env._key_dir.path().join("auth.pub"), &public_b).unwrap();
    let status = guard.terminate(Duration::from_secs(5));
    assert!(
        status.success(),
        "m6-auth-server did not stop cleanly for a rotation, got {status}"
    );
    spawn_server(env)
}

// ─── Tests ────────────────────────────────────────────────────────────────────

/// A refresh token issued under the old key still refreshes after a rotation.
///
/// The wrong outcome it catches: refresh re-validates the JWT signature against
/// the current key, so a token minted under the previous key is rejected even
/// though its hash is still in the database.
#[test]
fn a_refresh_token_issued_before_rotation_still_refreshes_after_the_key_rotates() {
    let env = setup_test_env("rotate-pre-token");
    seed_user(&env.site_dir, "ada", "ada-password");
    let mut guard = spawn_server(&env);

    let (_, refresh_a) = login_json(&mut guard, &env.socket_path, "ada", "ada-password");

    let mut guard = rotate_key(&env, guard);

    let resp = refresh_json(&mut guard, &env.socket_path, &refresh_a);
    assert_eq!(
        parse_status(&resp),
        200,
        "a pre-rotation refresh token was rejected after the rotation\n{resp}\n--- output ---\n{}",
        guard.output()
    );
}

/// A refresh token minted after the rotation refreshes at once.
///
/// The wrong outcome it catches: the service does not actually begin signing
/// with the new key after the rotation, so a fresh token cannot refresh.
#[test]
fn a_refresh_token_issued_after_rotation_refreshes_at_once() {
    let env = setup_test_env("rotate-post-token");
    seed_user(&env.site_dir, "bee", "bee-password");
    let guard = spawn_server(&env);

    let mut guard = rotate_key(&env, guard);

    let (_, refresh_b) = login_json(&mut guard, &env.socket_path, "bee", "bee-password");
    let resp = refresh_json(&mut guard, &env.socket_path, &refresh_b);
    assert_eq!(
        parse_status(&resp),
        200,
        "a post-rotation refresh token was rejected\n{resp}\n--- output ---\n{}",
        guard.output()
    );
}

/// A refresh token signed by a third key (neither the old nor the new) is
/// rejected, because its hash was never stored.
///
/// The wrong outcome it catches: refresh trusts the token itself rather than
/// the database hash, so a token the service never issued is accepted.
#[test]
fn a_refresh_token_signed_by_a_foreign_key_is_rejected() {
    let env = setup_test_env("rotate-foreign-key");
    let user = seed_user(&env.site_dir, "cee", "cee-password");
    let guard = spawn_server(&env);

    let mut guard = rotate_key(&env, guard);

    let (foreign_private, foreign_public) = generate_test_rsa_keys();
    let foreign_token = mint_refresh_token(&foreign_private, &foreign_public, ISSUER, &user);

    let resp = refresh_json(&mut guard, &env.socket_path, &foreign_token);
    assert_eq!(
        parse_status(&resp),
        401,
        "a refresh token signed by a foreign key was accepted\n{resp}\n--- output ---\n{}",
        guard.output()
    );
}
