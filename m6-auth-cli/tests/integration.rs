use std::path::PathBuf;
use std::process::{Command, Output};
use tempfile::TempDir;

// ── Helpers ───────────────────────────────────────────────────────────────────

fn binary_path() -> PathBuf {
    let mut p = std::env::current_exe().expect("current_exe");
    // Strip the test binary name and go up to target/debug/
    p.pop();
    // Sometimes tests run in deps/ subdirectory
    if p.ends_with("deps") {
        p.pop();
    }
    p.push("m6-auth-cli");
    p
}

// ── Test EC P-256 key material (generated offline for tests only) ──────────────

const TEST_PRIVATE_KEY_PEM: &str = "-----BEGIN PRIVATE KEY-----
REDACTED
REDACTED
REDACTED
-----END PRIVATE KEY-----";

const TEST_PUBLIC_KEY_PEM: &str = "-----BEGIN PUBLIC KEY-----
MFkwEwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAEYPw7LhJaPWI0AMSmKUZIuF3vJxo2
5SdJhIU/aqEJsCdBr8Q4RU24UYyHtFHEaJOELA2KdVUI0LgIWQ/GFDtSag==
-----END PUBLIC KEY-----";

// ── TestEnv ───────────────────────────────────────────────────────────────────

struct TestEnv {
    dir: TempDir,
}

impl TestEnv {
    fn new() -> Self {
        let dir = TempDir::new().expect("tempdir");
        // Create data subdirectory
        std::fs::create_dir_all(dir.path().join("data")).expect("mkdir data");
        // Write config
        let cfg = "[storage]\npath = \"data/auth.db\"\n\n[tokens]\naccess_ttl = 900\nrefresh_ttl = 2592000\nissuer = \"test\"\n\n[keys]\nprivate_key = \"keys/auth.pem\"\npublic_key = \"keys/auth.pub\"\n";
        std::fs::write(dir.path().join("m6-auth.conf"), cfg).expect("write config");
        TestEnv { dir }
    }

    /// Write embedded test keys to <tmpdir>/keys/. Required for token commands.
    fn setup_keys(&self) {
        std::fs::create_dir_all(self.dir.path().join("keys")).expect("mkdir keys");
        std::fs::write(self.dir.path().join("keys/auth.pem"), TEST_PRIVATE_KEY_PEM)
            .expect("write private key");
        std::fs::write(self.dir.path().join("keys/auth.pub"), TEST_PUBLIC_KEY_PEM)
            .expect("write public key");
    }

    fn config(&self) -> String {
        self.dir.path().join("m6-auth.conf").to_string_lossy().to_string()
    }

    fn run(&self, args: &[&str]) -> Output {
        let bin = binary_path();
        let mut cmd = Command::new(&bin);
        cmd.arg(self.config());
        for a in args {
            cmd.arg(a);
        }
        cmd.output().expect("run binary")
    }
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).to_string()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).to_string()
}

// ── Bootstrap sequence ────────────────────────────────────────────────────────

#[test]
fn test_bootstrap_user_add() {
    let env = TestEnv::new();
    let out = env.run(&["user", "add", "admin", "--role", "admin", "--password", "secret"]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
}

#[test]
fn test_bootstrap_group_add() {
    let env = TestEnv::new();
    // Need a user first for group member add
    env.run(&["user", "add", "admin", "--role", "admin", "--password", "secret"]);
    let out = env.run(&["group", "add", "editors"]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
}

#[test]
fn test_bootstrap_group_member_add() {
    let env = TestEnv::new();
    env.run(&["user", "add", "admin", "--role", "admin", "--password", "secret"]);
    env.run(&["group", "add", "editors"]);
    let out = env.run(&["group", "member", "add", "editors", "admin"]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
}

#[test]
fn test_bootstrap_user_ls_json_contains_admin() {
    let env = TestEnv::new();
    env.run(&["user", "add", "admin", "--role", "admin", "--password", "secret"]);
    env.run(&["group", "add", "editors"]);
    env.run(&["group", "member", "add", "editors", "admin"]);

    let out = env.run(&["user", "ls", "--json"]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));

    let body = stdout(&out);
    let parsed: serde_json::Value = serde_json::from_str(&body)
        .expect("valid JSON");
    assert!(parsed.is_array(), "should be a JSON array");
    let arr = parsed.as_array().unwrap();
    assert!(!arr.is_empty(), "array should not be empty");
    assert!(arr.iter().any(|u| u["username"] == "admin"),
        "admin not found in: {}", body);
}

#[test]
fn test_bootstrap_group_member_ls_shows_admin() {
    let env = TestEnv::new();
    env.run(&["user", "add", "admin", "--role", "admin", "--password", "secret"]);
    env.run(&["group", "add", "editors"]);
    env.run(&["group", "member", "add", "editors", "admin"]);

    let out = env.run(&["group", "member", "ls", "editors"]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    let body = stdout(&out);
    assert!(body.contains("admin"), "expected 'admin' in: {}", body);
}

// ── Full bootstrap in one test ────────────────────────────────────────────────

#[test]
fn test_full_bootstrap_sequence() {
    let env = TestEnv::new();

    let out = env.run(&["user", "add", "admin", "--role", "admin", "--password", "secret"]);
    assert_eq!(out.status.code(), Some(0));

    let out = env.run(&["group", "add", "editors"]);
    assert_eq!(out.status.code(), Some(0));

    let out = env.run(&["group", "member", "add", "editors", "admin"]);
    assert_eq!(out.status.code(), Some(0));

    // user ls --json
    let out = env.run(&["user", "ls", "--json"]);
    assert_eq!(out.status.code(), Some(0));
    let body = stdout(&out);
    let parsed: serde_json::Value = serde_json::from_str(&body).expect("valid JSON");
    assert!(parsed.is_array());
    let arr = parsed.as_array().unwrap();
    assert!(arr.iter().any(|u| u["username"] == "admin"));

    // group member ls editors
    let out = env.run(&["group", "member", "ls", "editors"]);
    assert_eq!(out.status.code(), Some(0));
    assert!(stdout(&out).contains("admin"));
}

// ── Error cases ───────────────────────────────────────────────────────────────

#[test]
fn test_user_add_duplicate_exits_1() {
    let env = TestEnv::new();
    env.run(&["user", "add", "alice", "--password", "pass1"]);
    let out = env.run(&["user", "add", "alice", "--password", "pass2"]);
    assert_eq!(out.status.code(), Some(1), "stdout: {}", stdout(&out));
    let err = stderr(&out);
    assert!(
        err.contains("alice") && (err.contains("already exists") || err.contains("exist")),
        "expected clear error about duplicate, got: {}", err
    );
}

#[test]
fn test_user_del_unknown_exits_1() {
    let env = TestEnv::new();
    let out = env.run(&["user", "del", "ghost"]);
    assert_eq!(out.status.code(), Some(1), "stdout: {}", stdout(&out));
    let err = stderr(&out);
    assert!(
        err.contains("ghost") || err.contains("not found"),
        "expected clear error, got: {}", err
    );
}

#[test]
fn test_config_not_found_exits_2() {
    let bin = binary_path();
    let out = Command::new(&bin)
        .args(&["/nonexistent/path/m6-auth.conf", "user", "ls"])
        .output()
        .expect("run");
    assert_eq!(out.status.code(), Some(2), "stdout: {}", stdout(&out));
    let err = stderr(&out);
    assert!(
        err.contains("not found") || err.contains("config"),
        "expected config error, got: {}", err
    );
}

#[test]
fn test_no_arguments_exits_2() {
    let bin = binary_path();
    let out = Command::new(&bin).output().expect("run");
    assert_eq!(out.status.code(), Some(2), "stdout: {}", stdout(&out));
    // Should print usage to stderr
    let err = stderr(&out);
    assert!(
        err.contains("Usage") || err.contains("usage") || err.contains("m6-auth-cli"),
        "expected usage message, got: {}", err
    );
}

// ── JSON output ───────────────────────────────────────────────────────────────

#[test]
fn test_user_ls_json_is_valid_array() {
    let env = TestEnv::new();
    env.run(&["user", "add", "alice", "--password", "pw"]);
    env.run(&["user", "add", "bob", "--password", "pw"]);

    let out = env.run(&["user", "ls", "--json"]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    let body = stdout(&out);
    let parsed: serde_json::Value = serde_json::from_str(&body).expect("valid JSON");
    assert!(parsed.is_array(), "expected array, got: {}", body);
    let arr = parsed.as_array().unwrap();
    assert_eq!(arr.len(), 2);
}

#[test]
fn test_group_ls_json_is_valid_array() {
    let env = TestEnv::new();
    env.run(&["group", "add", "alpha"]);
    env.run(&["group", "add", "beta"]);

    let out = env.run(&["group", "ls", "--json"]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    let body = stdout(&out);
    let parsed: serde_json::Value = serde_json::from_str(&body).expect("valid JSON");
    assert!(parsed.is_array(), "expected array, got: {}", body);
    let arr = parsed.as_array().unwrap();
    assert_eq!(arr.len(), 2);
}

// ── Table output ──────────────────────────────────────────────────────────────

#[test]
fn test_user_ls_table_format() {
    let env = TestEnv::new();
    env.run(&["user", "add", "alice", "--role", "admin", "--role", "user", "--password", "pw"]);

    let out = env.run(&["user", "ls"]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    let body = stdout(&out);
    assert!(body.contains("USERNAME"), "missing header: {}", body);
    assert!(body.contains("alice"), "missing user: {}", body);
}

#[test]
fn test_group_ls_table_format() {
    let env = TestEnv::new();
    env.run(&["group", "add", "mygroup"]);

    let out = env.run(&["group", "ls"]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    let body = stdout(&out);
    assert!(body.contains("GROUP"), "missing header: {}", body);
    assert!(body.contains("mygroup"), "missing group: {}", body);
}

// ── user passwd ───────────────────────────────────────────────────────────────

#[test]
fn test_user_passwd() {
    let env = TestEnv::new();
    env.run(&["user", "add", "alice", "--password", "old"]);
    let out = env.run(&["user", "passwd", "alice", "--password", "newpass"]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
}

// ── user roles ────────────────────────────────────────────────────────────────

#[test]
fn test_user_roles_set_unset() {
    let env = TestEnv::new();
    env.run(&["user", "add", "alice", "--role", "user", "--password", "pw"]);

    // Add admin role
    let out = env.run(&["user", "roles", "alice", "--set", "admin"]);
    assert_eq!(out.status.code(), Some(0), "set role failed: {}", stderr(&out));

    // Remove user role
    let out = env.run(&["user", "roles", "alice", "--unset", "user"]);
    assert_eq!(out.status.code(), Some(0), "unset role failed: {}", stderr(&out));

    // Verify via JSON
    let out = env.run(&["user", "ls", "--json"]);
    let body = stdout(&out);
    let parsed: serde_json::Value = serde_json::from_str(&body).expect("valid JSON");
    let alice = parsed.as_array().unwrap()
        .iter()
        .find(|u| u["username"] == "alice")
        .expect("alice not found");
    let roles: Vec<String> = serde_json::from_value(alice["roles"].clone()).unwrap();
    assert!(roles.contains(&"admin".to_string()), "admin role missing");
    assert!(!roles.contains(&"user".to_string()), "user role should be removed");
}

// ── group del ─────────────────────────────────────────────────────────────────

#[test]
fn test_group_del() {
    let env = TestEnv::new();
    env.run(&["group", "add", "tmp"]);
    let out = env.run(&["group", "del", "tmp"]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));

    // Verify gone
    let out2 = env.run(&["group", "del", "tmp"]);
    assert_eq!(out2.status.code(), Some(1));
}

// ── user del ─────────────────────────────────────────────────────────────────

#[test]
fn test_user_del() {
    let env = TestEnv::new();
    env.run(&["user", "add", "tmp", "--password", "pw"]);
    let out = env.run(&["user", "del", "tmp"]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));

    // Verify gone
    let out2 = env.run(&["user", "del", "tmp"]);
    assert_eq!(out2.status.code(), Some(1));
}

// ── token create / ls / revoke ────────────────────────────────────────────────

#[test]
fn test_token_create_prints_jwt() {
    let env = TestEnv::new();
    env.setup_keys();
    env.run(&["user", "add", "alice", "--password", "pw"]);

    let out = env.run(&["token", "create", "alice", "--name", "ci"]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    let body = stdout(&out);
    // A JWT has three dot-separated base64url segments
    assert!(body.trim().split('.').count() == 3, "expected JWT, got: {}", body);
}

#[test]
fn test_token_create_unknown_user_exits_1() {
    let env = TestEnv::new();
    env.setup_keys();

    let out = env.run(&["token", "create", "ghost", "--name", "ci"]);
    assert_eq!(out.status.code(), Some(1), "stdout: {}", stdout(&out));
}

#[test]
fn test_token_create_missing_keys_exits_1() {
    let env = TestEnv::new();
    // No setup_keys() call — keys dir doesn't exist
    env.run(&["user", "add", "alice", "--password", "pw"]);

    let out = env.run(&["token", "create", "alice"]);
    assert_eq!(out.status.code(), Some(1), "stdout: {}", stdout(&out));
    let err = stderr(&out);
    assert!(
        err.contains("key") || err.contains("not found") || err.contains("No such"),
        "expected key error, got: {}", err
    );
}

#[test]
fn test_token_ls_shows_created_token() {
    let env = TestEnv::new();
    env.setup_keys();
    env.run(&["user", "add", "alice", "--password", "pw"]);
    env.run(&["token", "create", "alice", "--name", "mytoken"]);

    let out = env.run(&["token", "ls", "alice"]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    let body = stdout(&out);
    assert!(body.contains("mytoken"), "expected token name in: {}", body);
}

#[test]
fn test_token_ls_json_is_valid_array() {
    let env = TestEnv::new();
    env.setup_keys();
    env.run(&["user", "add", "alice", "--password", "pw"]);
    env.run(&["token", "create", "alice", "--name", "t1"]);
    env.run(&["token", "create", "alice", "--name", "t2"]);

    let out = env.run(&["token", "ls", "alice", "--json"]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    let body = stdout(&out);
    let parsed: serde_json::Value = serde_json::from_str(&body).expect("valid JSON");
    assert!(parsed.is_array(), "expected array, got: {}", body);
    let arr = parsed.as_array().unwrap();
    assert_eq!(arr.len(), 2);
    assert!(arr.iter().any(|t| t["name"] == "t1"), "t1 not found");
    assert!(arr.iter().any(|t| t["name"] == "t2"), "t2 not found");
}

#[test]
fn test_token_revoke_removes_token() {
    let env = TestEnv::new();
    env.setup_keys();
    env.run(&["user", "add", "alice", "--password", "pw"]);
    env.run(&["token", "create", "alice", "--name", "todel"]);

    // Get the token ID from JSON listing
    let out = env.run(&["token", "ls", "alice", "--json"]);
    let body = stdout(&out);
    let parsed: serde_json::Value = serde_json::from_str(&body).expect("valid JSON");
    let arr = parsed.as_array().unwrap();
    let token_id = arr[0]["id"].as_str().expect("id field").to_string();

    // Revoke it
    let out = env.run(&["token", "revoke", &token_id]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));

    // Verify it's gone
    let out = env.run(&["token", "ls", "alice", "--json"]);
    let body = stdout(&out);
    let parsed: serde_json::Value = serde_json::from_str(&body).expect("valid JSON");
    let arr = parsed.as_array().unwrap();
    assert!(arr.is_empty(), "expected empty list after revoke, got: {}", body);
}

#[test]
fn test_token_revoke_unknown_exits_1() {
    let env = TestEnv::new();
    let out = env.run(&["token", "revoke", "nonexistent-id-xxxx"]);
    assert_eq!(out.status.code(), Some(1), "stdout: {}", stdout(&out));
}

// ── group member del ──────────────────────────────────────────────────────────

#[test]
fn test_group_member_del() {
    let env = TestEnv::new();
    env.run(&["user", "add", "alice", "--password", "pw"]);
    env.run(&["group", "add", "team"]);
    env.run(&["group", "member", "add", "team", "alice"]);

    let out = env.run(&["group", "member", "del", "team", "alice"]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));

    // Verify removed
    let out2 = env.run(&["group", "member", "ls", "team"]);
    assert!(!stdout(&out2).contains("alice"), "alice still a member");
}
