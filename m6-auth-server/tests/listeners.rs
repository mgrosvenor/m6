//! Tests for the two listeners: which routes answer on which socket, and the
//! flags that bind one side only.
//!
//! The wrong decision the 404 tests catch is one shared route set. A login that
//! ran on the check socket would put its bcrypt verify in front of every
//! `/check`, which is the tail the second process exists to remove.

#![allow(
    clippy::disallowed_methods,
    reason = "test code: blocking clients and processes drive the service from outside"
)]

use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use m6_core::testkit::{binary, Service};

const ISSUER: &str = "test.example.com";
const BOUND: Duration = Duration::from_secs(10);

struct TestEnv {
    _temp: tempfile::TempDir,
    site_dir: PathBuf,
    config_path: PathBuf,
    primary: PathBuf,
    check: PathBuf,
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

fn setup(id: &str) -> TestEnv {
    let temp = tempfile::TempDir::new().unwrap();

    let site_dir = temp.path().to_path_buf();
    let config_path = site_dir.join("m6-auth-test.conf");

    let (private_pem, public_pem) = generate_test_rsa_keys();
    std::fs::create_dir_all(site_dir.join("keys")).unwrap();
    let private_key_path = site_dir.join("keys/auth.pem");
    let public_key_path = site_dir.join("keys/auth.pub");
    std::fs::write(&private_key_path, &private_pem).unwrap();
    std::fs::write(&public_key_path, &public_pem).unwrap();

    let db_path = site_dir.join("data/auth.db");
    std::fs::create_dir_all(site_dir.join("data")).unwrap();
    // The login side creates and owns the schema. A check-only process opens
    // the file for reading without applying it, so the file and its schema
    // exist before any process starts, as they do in a deployment where the
    // login unit ran first.
    drop(m6_auth::Db::open(&db_path).expect("create the auth database"));
    std::fs::write(site_dir.join("site.toml"), "").unwrap();
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

    let primary = site_dir.join(format!("{id}.sock"));
    let check = site_dir.join(format!("{id}-check.sock"));
    TestEnv {
        _temp: temp,
        site_dir,
        config_path,
        primary,
        check,
    }
}

/// Spawn the service with the socket overrides a test needs and any flags.
fn spawn(env: &TestEnv, primary: &Path, check: Option<&Path>, args: &[&str]) -> Service {
    let mut cmd = Command::new(binary("m6-auth-server"));
    cmd.arg(&env.site_dir).arg(&env.config_path);
    cmd.env("M6_SOCKET_OVERRIDE", primary);
    if let Some(check) = check {
        cmd.env("M6_CHECK_SOCKET_OVERRIDE", check);
    }
    for arg in args {
        cmd.arg(arg);
    }
    Service::spawn("m6-auth-server", &mut cmd)
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

/// The wrong decision this catches: a route set shared by both listeners.
#[test]
fn a_check_on_the_login_socket_is_404() {
    let env = setup("check-on-login");
    let mut svc = spawn(&env, &env.primary, Some(&env.check), &[]);
    svc.wait_for_path(&env.primary, BOUND);
    svc.wait_for_path(&env.check, BOUND);
    svc.wait_for_started(BOUND);

    let resp = http_request(
        &env.primary,
        "POST /check HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\n\r\n",
    );
    assert_eq!(
        parse_status(&resp),
        404,
        "a protected route answered on the login socket\n{resp}"
    );
}

/// The wrong decision this catches: a login runs on the check socket, so its
/// bcrypt verify occupies the check loop.
#[test]
fn a_login_on_the_check_socket_is_404() {
    let env = setup("login-on-check");
    let mut svc = spawn(&env, &env.primary, Some(&env.check), &[]);
    svc.wait_for_path(&env.primary, BOUND);
    svc.wait_for_path(&env.check, BOUND);
    svc.wait_for_started(BOUND);

    let resp = http_request(
        &env.check,
        "POST /auth/login HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{}",
    );
    assert_eq!(
        parse_status(&resp),
        404,
        "a login route answered on the check socket\n{resp}"
    );
}

/// The reserved path belongs to the login socket.
///
/// The wrong decision this catches: `POST /genid` on a check-only process
/// answered 204 and ran the reload, because `Site` answers the reserved path
/// before routing and the loop's reload arm had no side test. A process meant
/// to serve only `/check` could be told to reload.
#[test]
fn genid_on_a_check_only_process_is_404() {
    let env = setup("genid-on-check");
    let mut svc = spawn(&env, &env.primary, Some(&env.check), &["--check-only"]);
    svc.wait_for_path(&env.check, BOUND);
    svc.wait_for_started(BOUND);

    for request in [
        "POST /genid HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\n\r\n",
        "POST /genid?x=1 HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\n\r\n",
        "GET /genid HTTP/1.1\r\nHost: localhost\r\n\r\n",
    ] {
        let resp = http_request(&env.check, request);
        assert_eq!(
            parse_status(&resp),
            404,
            "the reserved path answered on the check socket\n{resp}"
        );
    }
}

/// Which path answers on which socket, driven through both sockets.
///
/// The list is the point. `/check` answers only on the check socket. The four
/// login routes and the reserved `/genid` answer only on the login socket.
/// `/perf`, `/traffic`, `/ready` and `/health` answer on neither, because they
/// are m6-http's paths and this process has no route for them: their 404 is
/// routing finding nothing, not the side rule.
///
/// Every wrong-socket answer is the house 404. Before the reserved-path fix,
/// `POST /genid` on the check socket was 204 and ran the reload, and `GET
/// /genid` was 405.
#[test]
fn each_path_answers_on_one_socket_only() {
    let env = setup("one-side-only");
    let mut svc = spawn(&env, &env.primary, Some(&env.check), &[]);
    svc.wait_for_path(&env.primary, BOUND);
    svc.wait_for_path(&env.check, BOUND);
    svc.wait_for_started(BOUND);

    let check_requests = [
        "POST /genid HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\n\r\n",
        "GET /genid HTTP/1.1\r\nHost: localhost\r\n\r\n",
        "POST /auth/login HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{}",
        "POST /auth/refresh HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{}",
        "POST /auth/logout HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\n\r\n",
        "GET /auth/public-key HTTP/1.1\r\nHost: localhost\r\n\r\n",
    ];
    for request in check_requests {
        let resp = http_request(&env.check, request);
        assert_eq!(
            parse_status(&resp),
            404,
            "a login-side path answered on the check socket\n{resp}"
        );
    }

    // Paths this process does not serve at all: 404 on both sockets.
    for request in [
        "GET /perf HTTP/1.1\r\nHost: localhost\r\n\r\n",
        "GET /traffic HTTP/1.1\r\nHost: localhost\r\n\r\n",
        "GET /ready HTTP/1.1\r\nHost: localhost\r\n\r\n",
        "GET /health HTTP/1.1\r\nHost: localhost\r\n\r\n",
    ] {
        for socket in [&env.primary, &env.check] {
            let resp = http_request(socket, request);
            assert_eq!(
                parse_status(&resp),
                404,
                "a path with no route was not the house 404 on {}\n{resp}",
                socket.display()
            );
        }
    }

    // The one route the check socket serves, and the login socket's five.
    let resp = http_request(
        &env.check,
        "POST /check HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\n\r\n",
    );
    assert_eq!(
        parse_status(&resp),
        401,
        "the check route did not answer on its own socket\n{resp}"
    );
    let resp = http_request(
        &env.primary,
        "GET /auth/public-key HTTP/1.1\r\nHost: localhost\r\n\r\n",
    );
    assert_eq!(
        parse_status(&resp),
        200,
        "the login routes do not answer on the login socket\n{resp}"
    );
    let resp = http_request(
        &env.primary,
        "POST /genid HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\n\r\n",
    );
    assert_eq!(
        parse_status(&resp),
        204,
        "the login socket stopped answering the reserved path\n{resp}"
    );
}

/// The check socket is derived beside the primary, with the `check-` prefix
/// that keeps it out of the deployment's login glob.
#[test]
fn the_check_socket_is_beside_the_primary() {
    let env = setup("derived");
    // The deployed primary name, so the glob check below is the real one.
    let primary = env.site_dir.join("m6-auth.sock");
    let derived = env.site_dir.join("check-m6-auth.sock");
    let mut svc = spawn(&env, &primary, None, &[]);
    svc.wait_for_path(&primary, BOUND);
    svc.wait_for_path(&derived, BOUND);
    svc.wait_for_started(BOUND);
    let derived_name = derived.file_name().unwrap().to_string_lossy();
    assert_eq!(
        derived_name, "check-m6-auth.sock",
        "the check socket is not named after the primary"
    );
    assert!(
        !derived_name.starts_with("m6-auth"),
        "the derived check socket would match a login glob: {}",
        derived.display()
    );
}

/// A check-only process binds the check socket and never touches the primary.
///
/// The primary path is an existing regular file, so binding it would stop the
/// process with exit 2. Reaching the started line proves it did not.
#[test]
fn check_only_binds_the_check_socket_and_nothing_else() {
    let env = setup("check-only");
    let primary = env.primary.with_extension("not-a-socket");
    std::fs::write(&primary, b"the primary socket must not be bound").unwrap();

    let mut svc = spawn(&env, &primary, Some(&env.check), &["--check-only"]);
    svc.wait_for_path(&env.check, BOUND);
    svc.wait_for_started(BOUND);
    assert_eq!(
        std::fs::read(&primary).unwrap(),
        b"the primary socket must not be bound",
        "the check-only process touched the primary socket path"
    );
}

/// A login-only process binds the primary socket and never touches the check
/// path. The check path is an existing regular file, so binding it would stop
/// the process with exit 2.
#[test]
fn login_only_binds_the_primary_socket() {
    let env = setup("login-only");
    let check = env.check.with_extension("not-a-socket");
    std::fs::write(&check, b"the check socket must not be bound").unwrap();

    let mut svc = spawn(&env, &env.primary, Some(&check), &["--login-only"]);
    svc.wait_for_path(&env.primary, BOUND);
    svc.wait_for_started(BOUND);
    assert_eq!(
        std::fs::read(&check).unwrap(),
        b"the check socket must not be bound",
        "the login-only process touched the check socket path"
    );
}

#[test]
fn both_flags_together_exits_2() {
    let env = setup("both-flags");
    let mut svc = spawn(
        &env,
        &env.primary,
        Some(&env.check),
        &["--check-only", "--login-only"],
    );
    let status = svc
        .wait_for_exit(Instant::now() + BOUND)
        .expect("both flags should exit rather than serve");
    assert_eq!(status.code(), Some(2), "output:\n{}", svc.output());
    let out = svc.output();
    assert!(
        out.contains("--check-only") && out.contains("--login-only"),
        "the refusal must name both flags: {out}"
    );
}
