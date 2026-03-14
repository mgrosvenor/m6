use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::Duration;

// ─── ProcessGuard ─────────────────────────────────────────────────────────────

struct ProcessGuard(Child);

impl Drop for ProcessGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

// ─── Test fixtures ────────────────────────────────────────────────────────────

fn binary_path() -> PathBuf {
    let mut p = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    if p.ends_with("deps") {
        p = p.parent().unwrap().to_path_buf();
    }
    p.join("m6-auth-server")
}

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
    let public  = std::fs::read_to_string(&pub_path).unwrap();
    (private, public)
}

struct TestEnv {
    _temp:       tempfile::TempDir,
    site_dir:    PathBuf,
    config_path: PathBuf,
    socket_path: PathBuf,
    key_dir:     tempfile::TempDir,
}

fn setup_test_env(id: &str) -> TestEnv {
    let temp    = tempfile::TempDir::new().unwrap();
    let key_dir = tempfile::TempDir::new().unwrap();

    let site_dir    = temp.path().to_path_buf();
    let config_path = site_dir.join("m6-auth-test.conf");

    // Generate keys
    let (private_pem, public_pem) = generate_test_rsa_keys();
    let private_key_path = key_dir.path().join("auth.pem");
    let public_key_path  = key_dir.path().join("auth.pub");
    std::fs::write(&private_key_path, &private_pem).unwrap();
    std::fs::write(&public_key_path,  &public_pem).unwrap();

    // Write config
    let db_path = "data/auth.db";
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

    // Create data directory
    std::fs::create_dir_all(site_dir.join("data")).unwrap();

    // Socket path in temp dir
    let socket_path = std::env::temp_dir()
        .join("m6-test-sockets")
        .join(format!("{}.sock", id));
    std::fs::create_dir_all(socket_path.parent().unwrap()).unwrap();
    let _ = std::fs::remove_file(&socket_path);

    TestEnv { _temp: temp, site_dir, config_path, socket_path, key_dir }
}

fn spawn_server(env: &TestEnv) -> ProcessGuard {
    let binary = binary_path();
    let child = Command::new(&binary)
        .arg(&env.site_dir)
        .arg(&env.config_path)
        .env("M6_SOCKET_OVERRIDE", &env.socket_path)
        .spawn()
        .unwrap_or_else(|e| panic!("spawn {}: {}", binary.display(), e));

    // Wait for socket to appear (up to 10s)
    for _ in 0..200 {
        if env.socket_path.exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(env.socket_path.exists(), "socket did not appear at {:?}", env.socket_path);

    ProcessGuard(child)
}

/// Create a test user via the database before the server starts.
/// Returns the db path so we can pre-seed it.
fn seed_user(site_dir: &Path, username: &str, password: &str) {
    let db_path = site_dir.join("data/auth.db");
    std::fs::create_dir_all(db_path.parent().unwrap()).unwrap();
    let db = m6_auth::Db::open(&db_path).expect("open db");
    db.user_create(username, password, &["user"]).expect("create user");
}

/// Send a raw HTTP request over a Unix socket and return the full response bytes.
fn http_request(socket_path: &Path, request: &str) -> String {
    let mut stream = UnixStream::connect(socket_path)
        .unwrap_or_else(|e| panic!("connect {:?}: {}", socket_path, e));
    stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    stream.write_all(request.as_bytes()).unwrap();
    let mut resp = Vec::new();
    let _ = stream.read_to_end(&mut resp);
    String::from_utf8_lossy(&resp).into_owned()
}

fn http_request_with_body(socket_path: &Path, method: &str, path: &str, content_type: &str, body: &str) -> String {
    let req = format!(
        "{} {} HTTP/1.1\r\nHost: localhost\r\nContent-Type: {}\r\nContent-Length: {}\r\n\r\n{}",
        method, path, content_type, body.len(), body
    );
    http_request(socket_path, &req)
}

fn parse_status(resp: &str) -> u16 {
    resp.split_whitespace().nth(1).and_then(|s| s.parse().ok()).unwrap_or(0)
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
    let resp = http_request_with_body(&env.socket_path, "POST", "/auth/login",
        "application/x-www-form-urlencoded", body);

    assert_eq!(parse_status(&resp), 302, "expected 302\n{}", resp);

    let location = get_header(&resp, "Location").unwrap_or("");
    assert_eq!(location, "/dashboard", "expected redirect to /dashboard, got {}", location);

    let cookies = get_all_headers(&resp, "Set-Cookie");
    assert!(cookies.len() >= 2, "expected 2 Set-Cookie headers, got {}", cookies.len());

    let has_session = cookies.iter().any(|c| c.starts_with("session=") && c.contains("HttpOnly") && c.contains("Path=/"));
    let has_refresh = cookies.iter().any(|c| c.starts_with("refresh=") && c.contains("HttpOnly") && c.contains("Path=/auth/refresh"));
    assert!(has_session, "missing session cookie with HttpOnly and Path=/\ncookies: {:?}", cookies);
    assert!(has_refresh, "missing refresh cookie with HttpOnly and Path=/auth/refresh\ncookies: {:?}", cookies);
}

/// Login JSON, valid credentials → 200, JSON tokens, no cookies.
#[test]
fn t02_login_json_valid_credentials_200_no_cookies() {
    let env = setup_test_env("t02");
    seed_user(&env.site_dir, "bob", "pass123");
    let _guard = spawn_server(&env);

    let body = r#"{"username":"bob","password":"pass123"}"#;
    let resp = http_request_with_body(&env.socket_path, "POST", "/auth/login",
        "application/json", body);

    assert_eq!(parse_status(&resp), 200, "expected 200\n{}", resp);

    let cookies = get_all_headers(&resp, "Set-Cookie");
    assert!(cookies.is_empty(), "JSON login should not set cookies, got: {:?}", cookies);

    // Verify JSON body
    let body_start = resp.find("\r\n\r\n").map(|i| i + 4).unwrap_or(resp.len());
    let json_body: serde_json::Value = serde_json::from_str(&resp[body_start..]).expect("valid JSON");
    assert!(json_body["access_token"].is_string(), "missing access_token");
    assert!(json_body["refresh_token"].is_string(), "missing refresh_token");
    assert!(json_body["expires_in"].is_number(), "missing expires_in");
}

/// Login: wrong password → 302 to login error page (form) / 401 (JSON).
#[test]
fn t03_login_form_wrong_password_302_error() {
    let env = setup_test_env("t03");
    seed_user(&env.site_dir, "carol", "right_pass");
    let _guard = spawn_server(&env);

    let body = "username=carol&password=wrong_pass";
    let resp = http_request_with_body(&env.socket_path, "POST", "/auth/login",
        "application/x-www-form-urlencoded", body);

    assert_eq!(parse_status(&resp), 302, "expected 302\n{}", resp);
    let location = get_header(&resp, "Location").unwrap_or("");
    assert!(location.contains("/login") && location.contains("error=invalid"),
        "expected redirect to login error, got: {}", location);
}

#[test]
fn t03b_login_json_wrong_password_401() {
    let env = setup_test_env("t03b");
    seed_user(&env.site_dir, "dave", "right_pass");
    let _guard = spawn_server(&env);

    let body = r#"{"username":"dave","password":"wrong_pass"}"#;
    let resp = http_request_with_body(&env.socket_path, "POST", "/auth/login",
        "application/json", body);

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
        let resp = http_request_with_body(&env.socket_path, "POST", "/auth/login",
            "application/json", body);
        let status = parse_status(&resp);
        assert!(status == 401 || status == 429, "expected 401 or 429, got {}", status);
        if status == 429 {
            // We already got rate-limited early, test passes
            let retry_after = get_header(&resp, "Retry-After").unwrap_or("");
            assert!(!retry_after.is_empty(), "expected Retry-After header");
            return;
        }
    }

    // 6th attempt must be 429
    let resp = http_request_with_body(&env.socket_path, "POST", "/auth/login",
        "application/json", body);
    assert_eq!(parse_status(&resp), 429, "6th attempt should be rate-limited\n{}", resp);
    let retry_after = get_header(&resp, "Retry-After").unwrap_or("");
    assert!(!retry_after.is_empty(), "expected Retry-After header on 429");
}

/// Refresh: valid refresh cookie → 302, new session cookie.
#[test]
fn t05_refresh_valid_cookie_302_new_session() {
    let env = setup_test_env("t05");
    seed_user(&env.site_dir, "eve", "evepw");
    let _guard = spawn_server(&env);

    // Login to get tokens
    let body = r#"{"username":"eve","password":"evepw"}"#;
    let login_resp = http_request_with_body(&env.socket_path, "POST", "/auth/login",
        "application/json", body);
    assert_eq!(parse_status(&login_resp), 200);

    let body_start = login_resp.find("\r\n\r\n").map(|i| i + 4).unwrap_or(login_resp.len());
    let json: serde_json::Value = serde_json::from_str(&login_resp[body_start..]).unwrap();
    let refresh_token = json["refresh_token"].as_str().unwrap();

    // Use the refresh token via cookie
    let req = format!(
        "POST /auth/refresh HTTP/1.1\r\nHost: localhost\r\nCookie: refresh={}\r\nContent-Length: 0\r\n\r\n",
        refresh_token
    );
    let resp = http_request(&env.socket_path, &req);

    assert_eq!(parse_status(&resp), 302, "expected 302 on refresh\n{}", resp);

    let cookies = get_all_headers(&resp, "Set-Cookie");
    let has_session = cookies.iter().any(|c| c.starts_with("session=") && c.contains("HttpOnly"));
    assert!(has_session, "expected new session cookie after refresh\ncookies: {:?}", cookies);
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
    assert_eq!(location, "/login", "expected redirect to /login, got {}", location);
}

/// Logout: clears both cookies (Max-Age=0).
#[test]
fn t07_logout_clears_cookies() {
    let env = setup_test_env("t07");
    seed_user(&env.site_dir, "frank", "frankpw");
    let _guard = spawn_server(&env);

    // Login via form to get cookies
    let body = "username=frank&password=frankpw";
    let login_resp = http_request_with_body(&env.socket_path, "POST", "/auth/login",
        "application/x-www-form-urlencoded", body);
    assert_eq!(parse_status(&login_resp), 302);

    // Extract refresh cookie value
    let cookies = get_all_headers(&login_resp, "Set-Cookie");
    let refresh_cookie = cookies.iter()
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
    assert_eq!(parse_status(&logout_resp), 302, "expected 302 on logout\n{}", logout_resp);

    let logout_cookies = get_all_headers(&logout_resp, "Set-Cookie");
    let session_cleared = logout_cookies.iter().any(|c| c.contains("session=") && c.contains("Max-Age=0"));
    let refresh_cleared = logout_cookies.iter().any(|c| c.contains("refresh=") && c.contains("Max-Age=0"));
    assert!(session_cleared, "session cookie not cleared (Max-Age=0)\ncookies: {:?}", logout_cookies);
    assert!(refresh_cleared, "refresh cookie not cleared (Max-Age=0)\ncookies: {:?}", logout_cookies);
}

/// Public key endpoint returns valid PEM.
#[test]
fn t08_public_key_returns_pem() {
    let env = setup_test_env("t08");
    let _guard = spawn_server(&env);

    let req = "GET /auth/public-key HTTP/1.1\r\nHost: localhost\r\n\r\n";
    let resp = http_request(&env.socket_path, req);

    assert_eq!(parse_status(&resp), 200, "expected 200\n{}", resp);
    assert!(resp.contains("BEGIN PUBLIC KEY") || resp.contains("BEGIN RSA PUBLIC KEY"),
        "expected PEM public key in response\n{}", resp);
}

/// JWT signature verifiable with returned public key.
#[test]
fn t09_jwt_verifiable_with_public_key() {
    let env = setup_test_env("t09");
    seed_user(&env.site_dir, "grace", "gracepw");
    let _guard = spawn_server(&env);

    // Get access token
    let body = r#"{"username":"grace","password":"gracepw"}"#;
    let login_resp = http_request_with_body(&env.socket_path, "POST", "/auth/login",
        "application/json", body);
    assert_eq!(parse_status(&login_resp), 200);

    let body_start = login_resp.find("\r\n\r\n").map(|i| i + 4).unwrap_or(login_resp.len());
    let json: serde_json::Value = serde_json::from_str(&login_resp[body_start..]).unwrap();
    let access_token = json["access_token"].as_str().unwrap();

    // Get public key
    let req = "GET /auth/public-key HTTP/1.1\r\nHost: localhost\r\n\r\n";
    let pk_resp = http_request(&env.socket_path, req);
    let pk_body_start = pk_resp.find("\r\n\r\n").map(|i| i + 4).unwrap_or(pk_resp.len());
    let public_pem = &pk_resp[pk_body_start..];

    // Verify the JWT using jsonwebtoken directly
    let decoding_key = jsonwebtoken::DecodingKey::from_rsa_pem(public_pem.as_bytes())
        .expect("parse public key from PEM");
    let mut validation = jsonwebtoken::Validation::new(jsonwebtoken::Algorithm::RS256);
    validation.set_issuer(&["test.example.com"]);

    let result = jsonwebtoken::decode::<serde_json::Value>(access_token, &decoding_key, &validation);
    assert!(result.is_ok(), "JWT verification failed: {:?}", result.err());
}

/// `next` parameter with external URL → falls back to `/`.
#[test]
fn t10_next_external_url_falls_back_to_root() {
    let env = setup_test_env("t10");
    seed_user(&env.site_dir, "henry", "henrypw");
    let _guard = spawn_server(&env);

    // Attempt to redirect to external URL
    let body = "username=henry&password=henrypw&next=https://evil.example.com/steal";
    let resp = http_request_with_body(&env.socket_path, "POST", "/auth/login",
        "application/x-www-form-urlencoded", body);

    assert_eq!(parse_status(&resp), 302, "expected 302\n{}", resp);
    let location = get_header(&resp, "Location").unwrap_or("");
    assert_eq!(location, "/", "external URL should fall back to /, got: {}", location);
}
