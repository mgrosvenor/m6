//! Tests for `POST /check`, the route that answers the *current* identity.
//!
//! The point of the route is that the answer is the database's, not the
//! token's. `a_role_change_is_seen_without_a_new_token` is the test that proves
//! it: remove the test's ability to read the database and the token's frozen
//! role comes back instead. `a_near_expiry_token_gets_a_ttl_within_its_remaining_life`
//! guards the one way the answer can be wrong: a `ttl_ms` that outlives the
//! token it describes would let a caller hold a decision past the token's death.

#![allow(
    clippy::disallowed_methods,
    reason = "test code: threads, sleeps and blocking clients drive the service from outside"
)]

use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use m6_auth::jwt::{now_secs, AccessClaims, JwtEngine};
use m6_core::testkit::{binary, wait, Service};

const ISSUER: &str = "test.example.com";

struct TestEnv {
    _temp: tempfile::TempDir,
    site_dir: PathBuf,
    config_path: PathBuf,
    socket_path: PathBuf,
    db_path: PathBuf,
    private_key_path: PathBuf,
    public_key_path: PathBuf,
    _key_dir: tempfile::TempDir,
}

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

    let db_path = site_dir.join("data/auth.db");
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
        db_path.display(),
        ISSUER,
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
        db_path,
        private_key_path,
        public_key_path,
        _key_dir: key_dir,
    }
}

fn spawn_server(env: &TestEnv) -> Service {
    let mut svc = Service::spawn(
        "m6-auth-server",
        Command::new(binary("m6-auth-server"))
            .arg(&env.site_dir)
            .arg(&env.config_path)
            .env("M6_SOCKET_OVERRIDE", &env.socket_path),
    );
    svc.wait_for_path(&env.socket_path, Duration::from_secs(10));
    svc.wait_for_started(Duration::from_secs(10));
    assert!(
        wait::for_unix(&env.socket_path, Duration::from_secs(10)),
        "m6-auth-server created {} but never accepted a connection\n--- output ---\n{}",
        env.socket_path.display(),
        svc.output()
    );
    svc
}

fn seed_user(env: &TestEnv, username: &str, password: &str) {
    std::fs::create_dir_all(env.db_path.parent().unwrap()).unwrap();
    let db = m6_auth::Db::open(&env.db_path).expect("open db");
    db.user_create_with_cost(username, password, &["user"], 4)
        .expect("create user");
}

/// The access token a real login issues.
fn login(env: &TestEnv, username: &str, password: &str) -> String {
    let body = format!(r#"{{"username":"{username}","password":"{password}"}}"#);
    let resp = http_request(
        &env.socket_path,
        &format!(
            "POST /auth/login HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        ),
    );
    assert_eq!(parse_status(&resp), 200, "login failed\n{resp}");
    json_body(&resp)["access_token"]
        .as_str()
        .expect("login returned no access token")
        .to_string()
}

/// A token for `sub`, signed with the service's key, so a test can choose the
/// expiry and the subject. It asserts nothing about the database.
fn mint(env: &TestEnv, sub: &str, username: &str, exp: i64) -> String {
    let private = std::fs::read_to_string(&env.private_key_path).unwrap();
    let public = std::fs::read_to_string(&env.public_key_path).unwrap();
    let engine = JwtEngine::new(&private, &public, ISSUER.to_string()).expect("key pair");
    engine
        .encode_access(&AccessClaims {
            iss: ISSUER.to_string(),
            sub: sub.to_string(),
            exp,
            iat: now_secs(),
            username: username.to_string(),
            groups: Vec::new(),
            roles: vec!["user".to_string()],
        })
        .expect("encode token")
}

fn check(env: &TestEnv, token: &str) -> String {
    http_request(
        &env.socket_path,
        &format!(
            "POST /check HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {token}\r\nContent-Length: 0\r\n\r\n"
        ),
    )
}

fn http_request(socket_path: &Path, request: &str) -> String {
    let mut stream = UnixStream::connect(socket_path)
        .unwrap_or_else(|e| panic!("connect {:?}: {}", socket_path, e));
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    stream.write_all(request.as_bytes()).unwrap();
    let method = request.split(' ').next().unwrap_or("");
    let resp = m6_core::testkit::read_one(&mut stream, method)
        .unwrap_or_else(|e| panic!("no response within 10s: {e}"));
    String::from_utf8_lossy(&resp).into_owned()
}

fn parse_status(resp: &str) -> u16 {
    resp.split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

fn json_body(resp: &str) -> serde_json::Value {
    let start = resp.find("\r\n\r\n").map(|i| i + 4).unwrap_or(resp.len());
    serde_json::from_str(&resp[start..]).unwrap_or_else(|e| panic!("no JSON body: {e}\n{resp}"))
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

// ─── Tests ────────────────────────────────────────────────────────────────────

/// The database decides the roles, the token does not.
///
/// The wrong decision this catches: answering with the roles frozen in the
/// token, which would make a role change invisible until the token expires.
#[test]
fn a_role_change_is_seen_without_a_new_token() {
    let env = setup_test_env("check-role-change");
    seed_user(&env, "alice", "correct_pass");
    let _guard = spawn_server(&env);

    let token = login(&env, "alice", "correct_pass");

    // Read the token through its own decoder: it still says `user`.
    let private = std::fs::read_to_string(&env.private_key_path).unwrap();
    let public = std::fs::read_to_string(&env.public_key_path).unwrap();
    let engine = JwtEngine::new(&private, &public, ISSUER.to_string()).unwrap();
    let claims = engine.decode_access(&token).unwrap().claims;
    assert_eq!(claims.roles, vec!["user".to_string()]);

    // The database moves to `admin`. No token is minted again.
    let db = m6_auth::Db::open(&env.db_path).unwrap();
    db.user_set_roles("alice", &["admin"]).unwrap();
    drop(db);

    let resp = check(&env, &token);
    assert_eq!(
        parse_status(&resp),
        200,
        "a valid token for a live user was refused\n{resp}"
    );
    let body = json_body(&resp);
    assert_eq!(body["sub"], claims.sub, "the answer names another subject");
    assert_eq!(body["username"], "alice");
    assert_eq!(
        body["roles"],
        serde_json::json!(["admin"]),
        "the answer must be the database's current role, not the token's frozen `user`"
    );
    assert_eq!(
        engine.decode_access(&token).unwrap().claims.roles,
        vec!["user".to_string()],
        "the token itself must be unchanged, or the test proves nothing"
    );
}

/// An answer never outlives the token it describes.
///
/// The wrong decision this catches: a constant `ttl_ms` (for example the 5 s
/// ceiling) returned for a token that has 2 s left. A caller that cached it
/// would hold the decision 3 s past the token's death.
#[test]
fn a_near_expiry_token_gets_a_ttl_within_its_remaining_life() {
    let env = setup_test_env("check-ttl");
    seed_user(&env, "bob", "correct_pass");
    let _guard = spawn_server(&env);

    let user = m6_auth::Db::open(&env.db_path)
        .unwrap()
        .user_get("bob")
        .unwrap()
        .expect("seeded user");
    let exp = now_secs() + 2;
    let token = mint(&env, &user.id, "bob", exp);

    let before = now_ms();
    let resp = check(&env, &token);
    assert_eq!(
        parse_status(&resp),
        200,
        "check refused a live token\n{resp}"
    );
    let body = json_body(&resp);
    let ttl = body["ttl_ms"].as_u64().expect("no ttl_ms in the answer");
    assert!(
        ttl as i64 <= exp * 1000 - before,
        "ttl_ms {ttl} outlives the token: {} ms were left at request time",
        exp * 1000 - before
    );

    // Relative, not absolute: the answer carries a duration and no timestamp.
    assert!(
        body.get("exp").is_none() && body.get("expires_at").is_none(),
        "the answer must not carry a wall-clock deadline: {body}"
    );

    // The 5 s ceiling binds a token with 900 s of life, which proves the
    // short answer above came from the expiry and not from a constant of 0.
    let fresh = login(&env, "bob", "correct_pass");
    let resp = check(&env, &fresh);
    assert_eq!(
        parse_status(&resp),
        200,
        "check refused a fresh token\n{resp}"
    );
    assert_eq!(
        json_body(&resp)["ttl_ms"].as_u64(),
        Some(5000),
        "a token with 900 s left should get the 5 s ceiling"
    );
}

/// A subject with no database row is refused, not described with no roles.
#[test]
fn an_unknown_subject_is_401() {
    let env = setup_test_env("check-unknown");
    let _guard = spawn_server(&env);

    let token = mint(&env, "no-such-user-id", "nobody", now_secs() + 900);
    let resp = check(&env, &token);
    assert_eq!(parse_status(&resp), 401, "expected 401\n{resp}");
    assert_eq!(json_body(&resp)["error"], "unauthorised");
}

/// A deleted user is a revoked user: 401, not a 200 with `"roles":[]`.
#[test]
fn a_deleted_user_is_401() {
    let env = setup_test_env("check-deleted");
    seed_user(&env, "carol", "correct_pass");
    let _guard = spawn_server(&env);
    let token = login(&env, "carol", "correct_pass");

    let db = m6_auth::Db::open(&env.db_path).unwrap();
    db.user_delete("carol").unwrap();
    drop(db);

    let resp = check(&env, &token);
    assert_eq!(
        parse_status(&resp),
        401,
        "a deleted user must not be answered with an identity\n{resp}"
    );
    assert_eq!(json_body(&resp)["error"], "unauthorised");
}

/// A missing credential is refused, and never reaches the database.
#[test]
fn a_missing_credential_is_401() {
    let env = setup_test_env("check-no-credential");
    let _guard = spawn_server(&env);

    let resp = http_request(
        &env.socket_path,
        "POST /check HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\n\r\n",
    );
    assert_eq!(parse_status(&resp), 401, "expected 401\n{resp}");
}
