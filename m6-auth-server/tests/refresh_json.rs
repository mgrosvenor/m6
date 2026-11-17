//! The `/auth/refresh` JSON path and its rejection paths.
//!
//! `handle_refresh_json` (`m6-auth-server/src/handlers.rs`) answers 200 with a
//! new access token, or 401. The browser cookie paths have tests in
//! `integration.rs`, but the JSON path is only ever asserted for its status.
//! These tests assert what a JSON client sees: a fresh decodable access token,
//! and 401 for an expired, invalid or revoked refresh token.

#![allow(
    clippy::disallowed_methods,
    reason = "test code: threads, sleeps and blocking clients drive the service from outside"
)]

use std::path::{Path, PathBuf};
use std::process::Command;

use m6_auth::jwt::{hash_token, now_secs, AccessClaims, JwtEngine, RefreshClaims};
use m6_core::testkit::{binary, read_response, wait, Service, RESPONSE_BOUND};

const ISSUER: &str = "test.example.com";

// ─── Test fixtures ────────────────────────────────────────────────────────────

/// Generate an RSA key pair (2048-bit) for testing.
/// Returns (private_pem, public_pem).
fn generate_test_rsa_keys() -> (String, String) {
    let dir = tempfile::Builder::new()
        .prefix("auth-")
        .tempdir_in(m6_core::testkit::socket_scratch_root())
        .unwrap();
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
    key_dir: tempfile::TempDir,
}

fn setup_test_env(id: &str) -> TestEnv {
    let temp = tempfile::Builder::new()
        .prefix("auth-")
        .tempdir_in(m6_core::testkit::socket_scratch_root())
        .unwrap();
    let key_dir = tempfile::Builder::new()
        .prefix("auth-")
        .tempdir_in(m6_core::testkit::socket_scratch_root())
        .unwrap();

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

    let socket_path = temp.path().join(format!("{id}.sock"));

    TestEnv {
        _temp: temp,
        site_dir,
        config_path,
        socket_path,
        key_dir,
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

/// Create a test user in the database before the server starts, returning it so
/// a test can mint a token for that user.
///
/// Cost 4 is bcrypt's lowest: a fixture needs a user, not production's round
/// count, and an unoptimised cost-12 hash costs seconds per test.
fn seed_user(env: &TestEnv, username: &str, password: &str) -> m6_auth::User {
    let db_path = env.site_dir.join("data/auth.db");
    std::fs::create_dir_all(db_path.parent().unwrap()).unwrap();
    let db = m6_auth::Db::open(&db_path).expect("open db");
    db.user_create_with_cost(username, password, &["user"], 4)
        .expect("create user")
}

/// Store a refresh token hash against a user, for a token the test minted
/// itself rather than one issued by a login.
fn store_refresh_token(env: &TestEnv, user_id: &str, token: &str, expires_at: i64) {
    let db_path = env.site_dir.join("data/auth.db");
    let db = m6_auth::Db::open(&db_path).expect("open db");
    db.refresh_token_store(user_id, &hash_token(token), expires_at)
        .expect("store refresh token");
}

fn private_key(env: &TestEnv) -> String {
    std::fs::read_to_string(env.key_dir.path().join("auth.pem")).expect("read private key")
}

fn public_key(env: &TestEnv) -> String {
    std::fs::read_to_string(env.key_dir.path().join("auth.pub")).expect("read public key")
}

/// A `JwtEngine` over the same key files the server loaded.
fn engine(env: &TestEnv) -> JwtEngine {
    JwtEngine::new(
        &private_key(env),
        &public_key(env),
        ISSUER.to_string(),
        ISSUER.to_string(),
    )
    .expect("build jwt engine")
}

/// A refresh token signed by the server's key whose `exp` is in the past.
fn expired_refresh_token(env: &TestEnv, user_id: &str, username: &str) -> String {
    let now = now_secs();
    let claims = RefreshClaims {
        iss: ISSUER.to_string(),
        aud: ISSUER.to_string(),
        sub: user_id.to_string(),
        // One hour past expiry, well beyond the engine's zero leeway, so the
        // verdict cannot depend on which second the server checks in.
        exp: now - 3600,
        iat: now - 7200,
        jti: m6_auth::new_token_id(),
        username: username.to_string(),
        token_type: "refresh".to_string(),
    };
    engine(env)
        .encode_refresh(&claims)
        .expect("encode an expired refresh token")
}

/// Decode an access token with the public key alone, as a client would.
fn decode_access(env: &TestEnv, token: &str) -> AccessClaims {
    let decoding_key = jsonwebtoken::DecodingKey::from_rsa_pem(public_key(env).as_bytes())
        .expect("parse public key");
    let mut validation = jsonwebtoken::Validation::new(jsonwebtoken::Algorithm::RS256);
    validation.set_issuer(&[ISSUER]);
    validation.set_audience(&[ISSUER]);
    jsonwebtoken::decode::<AccessClaims>(token, &decoding_key, &validation)
        .expect("a decodable access token")
        .claims
}

// ─── HTTP helpers ─────────────────────────────────────────────────────────────

/// Send a request and wait for the response event or the service's exit.
///
/// [`RESPONSE_BOUND`] is the safety net above the work, not a read deadline
/// guessed at it.
fn http_request(svc: &mut Service, socket_path: &Path, request: &str) -> String {
    let mut stream = std::os::unix::net::UnixStream::connect(socket_path)
        .unwrap_or_else(|e| panic!("connect {:?}: {}", socket_path, e));
    std::io::Write::write_all(&mut stream, request.as_bytes()).unwrap();
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

fn json_body(resp: &str) -> serde_json::Value {
    let at = resp.find("\r\n\r\n").map(|i| i + 4).unwrap_or(resp.len());
    serde_json::from_str(&resp[at..]).expect("a JSON body")
}

/// Log in over JSON and return the parsed body, which carries the tokens.
fn login_json(
    svc: &mut Service,
    socket_path: &Path,
    username: &str,
    password: &str,
) -> serde_json::Value {
    let body = serde_json::json!({ "username": username, "password": password }).to_string();
    let resp = http_request_with_body(
        svc,
        socket_path,
        "POST",
        "/auth/login",
        "application/json",
        &body,
    );
    assert_eq!(parse_status(&resp), 200, "the seeding login failed\n{resp}");
    json_body(&resp)
}

fn post_refresh_json(svc: &mut Service, socket_path: &Path, token: &str) -> String {
    let body = serde_json::json!({ "refresh_token": token }).to_string();
    http_request_with_body(
        svc,
        socket_path,
        "POST",
        "/auth/refresh",
        "application/json",
        &body,
    )
}

// ─── Tests ────────────────────────────────────────────────────────────────────

/// A valid refresh token posted as JSON answers 200 with a new access token
/// that decodes for the same user. The wrong outcome this catches is a refresh
/// that claims success but returns a token no client can use, or one for a
/// different user.
#[test]
fn valid_json_refresh_returns_a_new_decodable_access_token() {
    let env = setup_test_env("valid-json-refresh");
    seed_user(&env, "alice", "alicepw");
    let mut guard = spawn_server(&env);

    let login = login_json(&mut guard, &env.socket_path, "alice", "alicepw");
    let refresh_token = login["refresh_token"].as_str().expect("refresh_token");
    let login_sub = decode_access(&env, login["access_token"].as_str().expect("access_token")).sub;

    let resp = post_refresh_json(&mut guard, &env.socket_path, refresh_token);
    assert_eq!(
        parse_status(&resp),
        200,
        "a valid JSON refresh was rejected\n{resp}"
    );

    let body = json_body(&resp);
    let access_token = body["access_token"]
        .as_str()
        .expect("access_token in the refresh response");
    assert!(!access_token.is_empty(), "the access token is empty");
    assert!(body["expires_in"].is_number(), "missing expires_in\n{body}");

    let claims = decode_access(&env, access_token);
    assert_eq!(
        claims.sub, login_sub,
        "the new access token is for a different user"
    );
    assert!(
        claims.exp > now_secs(),
        "the new access token is already expired"
    );
}

/// A refresh token whose stored row has expired answers 401. A refresh token is
/// verified by its stored hash alone, so the row's `expires_at` is where its
/// lifetime ends (the token's own `exp` is not read: checking it would mean
/// checking a signature that a key rotation invalidates). The wrong outcome
/// this catches is a refresh that issues a new access token after the row's
/// lifetime has elapsed.
#[test]
fn expired_refresh_token_json_answers_401() {
    let env = setup_test_env("expired-json-refresh");
    let user = seed_user(&env, "bob", "bobpw");
    let expired = expired_refresh_token(&env, &user.id, &user.username);
    store_refresh_token(&env, &user.id, &expired, now_secs() - 1);
    let mut guard = spawn_server(&env);

    let resp = post_refresh_json(&mut guard, &env.socket_path, &expired);
    assert_eq!(
        parse_status(&resp),
        401,
        "an expired refresh token was accepted\n{resp}"
    );
}

/// An invalid refresh token answers 401. The wrong outcome this catches is a
/// refresh that accepts a token it cannot verify.
#[test]
fn invalid_refresh_token_json_answers_401() {
    let env = setup_test_env("invalid-json-refresh");
    let mut guard = spawn_server(&env);

    let resp = post_refresh_json(&mut guard, &env.socket_path, "not_a_valid_jwt");
    assert_eq!(
        parse_status(&resp),
        401,
        "an invalid refresh token was accepted\n{resp}"
    );
}

/// Reusing a refresh token after logout answers 401. The wrong outcome this
/// catches is a refresh that still honours a token the logout revoked.
#[test]
fn revoked_refresh_token_after_logout_answers_401() {
    let env = setup_test_env("revoked-json-refresh");
    seed_user(&env, "carol", "carolpw");
    let mut guard = spawn_server(&env);

    let login = login_json(&mut guard, &env.socket_path, "carol", "carolpw");
    let refresh_token = login["refresh_token"].as_str().expect("refresh_token");

    // Browser logout revokes the refresh token by its hash.
    let logout = format!(
        "POST /auth/logout HTTP/1.1\r\nHost: localhost\r\nCookie: refresh={}\r\nContent-Length: 0\r\n\r\n",
        refresh_token
    );
    let logout_resp = http_request(&mut guard, &env.socket_path, &logout);
    assert_eq!(
        parse_status(&logout_resp),
        302,
        "logout did not answer 302\n{logout_resp}"
    );

    let resp = post_refresh_json(&mut guard, &env.socket_path, refresh_token);
    assert_eq!(
        parse_status(&resp),
        401,
        "a revoked refresh token was accepted after logout\n{resp}"
    );
}
