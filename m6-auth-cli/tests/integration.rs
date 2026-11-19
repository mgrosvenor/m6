use std::process::{Command, Output, Stdio};
use tempfile::TempDir;

// ── Helpers ───────────────────────────────────────────────────────────────────

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
        let cfg = format!(
            "[storage]\npath = \"{}\"\n\n[tokens]\naccess_ttl = 900\nrefresh_ttl = 2592000\nissuer = \"test\"\n\n[keys]\nprivate_key = \"{}\"\npublic_key = \"{}\"\n",
            dir.path().join("data/auth.db").display(),
            dir.path().join("keys/auth.pem").display(),
            dir.path().join("keys/auth.pub").display(),
        );
        std::fs::write(dir.path().join("m6-auth.conf"), cfg).expect("write config");
        TestEnv { dir }
    }

    /// Generate a fresh EC P-256 key pair in <tmpdir>/keys/. Required for token commands.
    ///
    /// Goes via a SEC1 intermediate rather than `openssl genpkey -algorithm EC`
    /// directly into PKCS8: on this toolchain's LibreSSL, `genpkey`'s PKCS8
    /// output omits the embedded public-key field (RFC 5958 optional [1] tag),
    /// which loads fine but fails at actual ECDSA signing with `ring`
    /// ("error: encoding JWT"). Generating SEC1 first always embeds the public
    /// key, and converting that to PKCS8 preserves it. Same recipe as
    /// dev.sh/setup-origin.sh use for the real auth signing keys.
    fn setup_keys(&self) {
        let key_dir = self.dir.path().join("keys");
        std::fs::create_dir_all(&key_dir).expect("mkdir keys");
        let sec1 = key_dir.join("auth-sec1.pem");
        let pem = key_dir.join("auth.pem");
        let pub_ = key_dir.join("auth.pub");

        let st = Command::new("openssl")
            .args([
                "ecparam",
                "-name",
                "prime256v1",
                "-genkey",
                "-noout",
                "-out",
            ])
            .arg(&sec1)
            .stderr(Stdio::null())
            .status()
            .expect("openssl not found — install openssl to run token tests");
        assert!(st.success(), "openssl ecparam failed");

        let st = Command::new("openssl")
            .args(["pkcs8", "-topk8", "-nocrypt", "-in"])
            .arg(&sec1)
            .arg("-out")
            .arg(&pem)
            .stderr(Stdio::null())
            .status()
            .expect("openssl pkcs8 failed");
        assert!(st.success(), "openssl pkcs8 (topk8) failed");

        let st = Command::new("openssl")
            .args(["ec", "-in"])
            .arg(&sec1)
            .arg("-pubout")
            .arg("-out")
            .arg(&pub_)
            .stderr(Stdio::null())
            .status()
            .expect("openssl ec failed");
        assert!(st.success(), "openssl ec (pubout) failed");

        let _ = std::fs::remove_file(&sec1);
    }

    fn config(&self) -> String {
        self.dir
            .path()
            .join("m6-auth.conf")
            .to_string_lossy()
            .to_string()
    }

    fn run(&self, args: &[&str]) -> Output {
        let bin = m6_core::testkit::binary("m6-auth-cli");
        let mut cmd = Command::new(&bin);
        cmd.arg(self.config());
        for a in args {
            cmd.arg(a);
        }
        cmd.output().expect("run binary")
    }

    /// The database this test's config names, for a fixture that writes it
    /// through `m6_auth` rather than through the CLI.
    fn db_path(&self) -> std::path::PathBuf {
        self.dir.path().join("data/auth.db")
    }

    /// Create a user directly at bcrypt's lowest cost, 4.
    ///
    /// `user add` is production code and hashes at cost 12, which an
    /// unoptimised build pays seconds for. A test that only needs the user
    /// seeds it here. The tests whose subject is `user add` or `user passwd`
    /// still run the command.
    fn seed(&self, username: &str, password: &str, roles: &[&str]) {
        let db = m6_auth::Db::open(&self.db_path()).expect("open db");
        db.user_create_with_cost(username, password, roles, 4)
            .expect("seed user");
    }

    /// Run a setup command and assert that it exited 0.
    ///
    /// A setup command that fails silently makes the test fail far from the
    /// cause, so the message carries the command and its stderr.
    fn run_ok(&self, args: &[&str]) -> Output {
        let out = self.run(args);
        assert_eq!(
            out.status.code(),
            Some(0),
            "{:?} failed, stderr: {}",
            args,
            stderr(&out)
        );
        out
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
    let out = env.run(&[
        "user",
        "add",
        "admin",
        "--role",
        "admin",
        "--password",
        "secret",
    ]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
}

#[test]
fn test_bootstrap_group_add() {
    let env = TestEnv::new();
    let out = env.run(&["group", "add", "editors"]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
}

#[test]
fn test_bootstrap_group_member_add() {
    let env = TestEnv::new();
    env.seed("admin", "secret", &["admin"]);
    env.run_ok(&["group", "add", "editors"]);
    let out = env.run(&["group", "member", "add", "editors", "admin"]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
}

#[test]
fn test_bootstrap_user_ls_json_contains_admin() {
    let env = TestEnv::new();
    env.seed("admin", "secret", &["admin"]);
    env.run_ok(&["group", "add", "editors"]);
    env.run_ok(&["group", "member", "add", "editors", "admin"]);

    let out = env.run(&["user", "ls", "--json"]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));

    let body = stdout(&out);
    let parsed: serde_json::Value = serde_json::from_str(&body).expect("valid JSON");
    assert!(parsed.is_array(), "should be a JSON array");
    let arr = parsed.as_array().unwrap();
    assert!(!arr.is_empty(), "array should not be empty");
    assert!(
        arr.iter().any(|u| u["username"] == "admin"),
        "admin not found in: {}",
        body
    );
}

#[test]
fn test_bootstrap_group_member_ls_shows_admin() {
    let env = TestEnv::new();
    env.seed("admin", "secret", &["admin"]);
    env.run_ok(&["group", "add", "editors"]);
    env.run_ok(&["group", "member", "add", "editors", "admin"]);

    let out = env.run(&["group", "member", "ls", "editors"]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    let body = stdout(&out);
    assert!(body.contains("admin"), "expected 'admin' in: {}", body);
}

// ── Full bootstrap in one test ────────────────────────────────────────────────

#[test]
fn test_full_bootstrap_sequence() {
    let env = TestEnv::new();

    env.run_ok(&[
        "user",
        "add",
        "admin",
        "--role",
        "admin",
        "--password",
        "secret",
    ]);

    env.run_ok(&["group", "add", "editors"]);

    env.run_ok(&["group", "member", "add", "editors", "admin"]);

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
    env.seed("alice", "pass1", &[]);
    let out = env.run(&["user", "add", "alice", "--password", "pass2"]);
    assert_eq!(out.status.code(), Some(1), "stdout: {}", stdout(&out));
    let err = stderr(&out);
    assert!(
        err.contains("alice") && (err.contains("already exists") || err.contains("exist")),
        "expected clear error about duplicate, got: {}",
        err
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
        "expected clear error, got: {}",
        err
    );
}

#[test]
fn test_config_not_found_exits_2() {
    let bin = m6_core::testkit::binary("m6-auth-cli");
    let out = Command::new(&bin)
        .args(["/nonexistent/path/m6-auth.conf", "user", "ls"])
        .output()
        .expect("run");
    assert_eq!(out.status.code(), Some(2), "stdout: {}", stdout(&out));
    let err = stderr(&out);
    assert!(
        err.contains("not found") || err.contains("config"),
        "expected config error, got: {}",
        err
    );
}

#[test]
fn test_no_arguments_exits_2() {
    let bin = m6_core::testkit::binary("m6-auth-cli");
    let out = Command::new(&bin).output().expect("run");
    assert_eq!(out.status.code(), Some(2), "stdout: {}", stdout(&out));
    // Should print usage to stderr
    let err = stderr(&out);
    assert!(
        err.contains("Usage") || err.contains("usage") || err.contains("m6-auth-cli"),
        "expected usage message, got: {}",
        err
    );
}

// ── JSON output ───────────────────────────────────────────────────────────────

#[test]
fn test_user_ls_json_is_valid_array() {
    let env = TestEnv::new();
    env.seed("alice", "pw", &[]);
    env.seed("bob", "pw", &[]);

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
    env.run_ok(&["group", "add", "alpha"]);
    env.run_ok(&["group", "add", "beta"]);

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
    env.seed("alice", "pw", &["admin", "user"]);

    let out = env.run(&["user", "ls"]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    let body = stdout(&out);
    assert!(body.contains("USERNAME"), "missing header: {}", body);
    assert!(body.contains("alice"), "missing user: {}", body);
}

#[test]
fn test_group_ls_table_format() {
    let env = TestEnv::new();
    env.run_ok(&["group", "add", "mygroup"]);

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
    env.seed("alice", "old", &[]);
    let out = env.run(&["user", "passwd", "alice", "--password", "newpass"]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
}

// ── user roles ────────────────────────────────────────────────────────────────

#[test]
fn test_user_roles_set_unset() {
    let env = TestEnv::new();
    env.seed("alice", "pw", &["user"]);

    // Add admin role
    let out = env.run(&["user", "roles", "alice", "--set", "admin"]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "set role failed: {}",
        stderr(&out)
    );

    // Remove user role
    let out = env.run(&["user", "roles", "alice", "--unset", "user"]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "unset role failed: {}",
        stderr(&out)
    );

    // Verify via JSON
    let out = env.run(&["user", "ls", "--json"]);
    let body = stdout(&out);
    let parsed: serde_json::Value = serde_json::from_str(&body).expect("valid JSON");
    let alice = parsed
        .as_array()
        .unwrap()
        .iter()
        .find(|u| u["username"] == "alice")
        .expect("alice not found");
    let roles: Vec<String> = serde_json::from_value(alice["roles"].clone()).unwrap();
    assert!(roles.contains(&"admin".to_string()), "admin role missing");
    assert!(
        !roles.contains(&"user".to_string()),
        "user role should be removed"
    );
}

// ── group del ─────────────────────────────────────────────────────────────────

#[test]
fn test_group_del() {
    let env = TestEnv::new();
    env.run_ok(&["group", "add", "tmp"]);
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
    env.seed("tmp", "pw", &[]);
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
    env.seed("alice", "pw", &[]);

    let out = env.run(&["token", "create", "alice", "--name", "ci"]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    let body = stdout(&out);
    // A JWT has three dot-separated base64url segments
    assert!(
        body.trim().split('.').count() == 3,
        "expected JWT, got: {}",
        body
    );
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
    env.seed("alice", "pw", &[]);

    let out = env.run(&["token", "create", "alice"]);
    assert_eq!(out.status.code(), Some(1), "stdout: {}", stdout(&out));
    let err = stderr(&out);
    assert!(
        err.contains("key") || err.contains("not found") || err.contains("No such"),
        "expected key error, got: {}",
        err
    );
}

#[test]
fn test_token_ls_shows_created_token() {
    let env = TestEnv::new();
    env.setup_keys();
    env.seed("alice", "pw", &[]);
    env.run_ok(&["token", "create", "alice", "--name", "mytoken"]);

    let out = env.run(&["token", "ls", "alice"]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    let body = stdout(&out);
    assert!(body.contains("mytoken"), "expected token name in: {}", body);
}

/// The diagnostic for a `token ls --json` failure, built at the moment it is
/// needed.
///
/// The m6 issue 327 record carried neither command stream and no independent
/// view of the rows. This message carries both streams, the array the listing
/// answered, and the rows a second, fresh connection can read right now, so a
/// failure shows whether the rows existed independently of the listing.
fn listing_failure(env: &TestEnv, out: &Output) -> String {
    let out_text = stdout(out);
    let err_text = stderr(out);
    let parsed = match serde_json::from_str::<serde_json::Value>(&out_text) {
        Ok(value) => match value.as_array() {
            Some(arr) => format!("{arr:?}"),
            None => format!("<not an array: {value:?}>"),
        },
        Err(e) => format!("<no JSON array: {e}>"),
    };
    let rows = match m6_auth::Db::open(&env.db_path()) {
        Ok(db) => match db.api_token_list("alice") {
            Ok(tokens) => {
                let names: Vec<&str> = tokens.iter().map(|tok| tok.name.as_str()).collect();
                format!("{} {:?}", tokens.len(), names)
            }
            Err(e) => format!("<fresh connection could not list the rows: {e}>"),
        },
        Err(e) => format!("<fresh connection could not open the database: {e}>"),
    };
    format!(
        "stdout: {out_text:?}\nstderr: {err_text:?}\nparsed array: {parsed}\nfresh-connection rows: {rows}"
    )
}

#[test]
fn test_token_ls_json_is_valid_array() {
    let env = TestEnv::new();
    env.setup_keys();
    env.seed("alice", "pw", &[]);
    // The rows this test counts are written by the two CLI processes it runs,
    // so the count proves that two `token create` processes that exited 0 are
    // both visible to the next command. The failure message reads the rows
    // again through a second, fresh connection, so a failure shows whether the
    // rows existed at all.
    env.run_ok(&["token", "create", "alice", "--name", "t1"]);
    env.run_ok(&["token", "create", "alice", "--name", "t2"]);

    let out = env.run(&["token", "ls", "alice", "--json"]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "token ls failed: {}",
        listing_failure(&env, &out)
    );
    let body = stdout(&out);
    let parsed: serde_json::Value = serde_json::from_str(&body).unwrap_or_else(|e| {
        panic!(
            "token ls did not answer JSON ({e}): {}",
            listing_failure(&env, &out)
        )
    });
    assert!(
        parsed.is_array(),
        "token ls answered no array: {}",
        listing_failure(&env, &out)
    );
    let arr = parsed.as_array().unwrap();
    assert_eq!(
        arr.len(),
        2,
        "the two tokens the CLI created are not both listed: {}",
        listing_failure(&env, &out)
    );
    assert!(
        arr.iter().any(|t| t["name"] == "t1"),
        "t1 is not listed: {}",
        listing_failure(&env, &out)
    );
    assert!(
        arr.iter().any(|t| t["name"] == "t2"),
        "t2 is not listed: {}",
        listing_failure(&env, &out)
    );
}

/// The record from m6 issue 327: a `token create` that exits 0 must be listed
/// by the very next command. No retry and no sleep: the create's exit is the
/// event, and the listing is the assertion, so a create that reports success
/// without its row being visible fails here with both commands' output.
#[test]
fn test_token_ls_lists_the_token_the_previous_command_created() {
    let env = TestEnv::new();
    env.setup_keys();
    env.seed("alice", "pw", &[]);
    env.run_ok(&["token", "create", "alice", "--name", "after-create"]);

    let out = env.run(&["token", "ls", "alice", "--json"]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "token ls failed: {}",
        listing_failure(&env, &out)
    );
    let body = stdout(&out);
    let parsed: serde_json::Value = serde_json::from_str(&body).unwrap_or_else(|e| {
        panic!(
            "token ls did not answer JSON ({e}): {}",
            listing_failure(&env, &out)
        )
    });
    let arr = parsed
        .as_array()
        .unwrap_or_else(|| panic!("expected an array: {}", listing_failure(&env, &out)));
    assert_eq!(
        arr.len(),
        1,
        "the token the previous command created is not listed: {}",
        listing_failure(&env, &out)
    );
    assert!(
        arr.iter().any(|t| t["name"] == "after-create"),
        "the created token is not listed: {}",
        listing_failure(&env, &out)
    );
}

#[test]
fn test_token_revoke_removes_token() {
    let env = TestEnv::new();
    env.setup_keys();
    env.seed("alice", "pw", &[]);
    env.run_ok(&["token", "create", "alice", "--name", "todel"]);

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
    assert!(
        arr.is_empty(),
        "expected empty list after revoke, got: {}",
        body
    );
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
    env.seed("alice", "pw", &[]);
    env.run_ok(&["group", "add", "team"]);
    env.run_ok(&["group", "member", "add", "team", "alice"]);

    let out = env.run(&["group", "member", "del", "team", "alice"]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));

    // Verify removed
    let out2 = env.run(&["group", "member", "ls", "team"]);
    assert!(!stdout(&out2).contains("alice"), "alice still a member");
}

#[test]
fn a_relative_storage_path_exits_2() {
    let env = TestEnv::new();
    std::fs::write(
        env.dir.path().join("m6-auth.conf"),
        "[storage]\npath = \"data/auth.db\"\n",
    )
    .expect("write config");
    let out = env.run(&["user", "ls"]);
    assert_eq!(out.status.code(), Some(2), "stderr: {}", stderr(&out));
    assert!(
        stderr(&out).contains("[storage] path"),
        "stderr: {}",
        stderr(&out)
    );
}

#[test]
fn a_relative_private_key_exits_2() {
    let env = TestEnv::new();
    let db = env.dir.path().join("data/auth.db").display().to_string();
    let pubkey = env.dir.path().join("keys/auth.pub").display().to_string();
    std::fs::write(
        env.dir.path().join("m6-auth.conf"),
        format!(
            "[storage]\npath = \"{db}\"\n\n[tokens]\nissuer = \"test.example.com\"\n\n[keys]\nprivate_key = \"keys/auth.pem\"\npublic_key = \"{pubkey}\"\n"
        ),
    )
    .expect("write config");
    let out = env.run(&["user", "ls"]);
    assert_eq!(out.status.code(), Some(2), "stderr: {}", stderr(&out));
    assert!(
        stderr(&out).contains("[keys] private_key"),
        "stderr: {}",
        stderr(&out)
    );
}

/// A config with no `[tokens] issuer` exits 2 and names the key.
///
/// This tool reads no site.toml, so there is no deployment name to fall back to
/// and it refuses rather than inventing `localhost`. The wrong outcome this
/// catches: every token from a config that names no issuer carries the same
/// default name, so a token minted for one deployment verifies at another that
/// shares the signing key.
#[test]
fn a_config_without_an_issuer_exits_2_naming_the_key() {
    let env = TestEnv::new();
    let db = env.dir.path().join("data/auth.db").display().to_string();
    let pubkey = env.dir.path().join("keys/auth.pub").display().to_string();
    std::fs::write(
        env.dir.path().join("m6-auth.conf"),
        format!(
            "[storage]\npath = \"{db}\"\n\n[tokens]\naccess_ttl = 900\nrefresh_ttl = 2592000\n\n[keys]\nprivate_key = \"{}\"\npublic_key = \"{pubkey}\"\n",
            env.dir.path().join("keys/auth.pem").display(),
        ),
    )
    .expect("write config");
    let out = env.run(&["user", "ls"]);
    assert_eq!(out.status.code(), Some(2), "stderr: {}", stderr(&out));
    assert!(
        stderr(&out).contains("[tokens] issuer"),
        "stderr: {}",
        stderr(&out)
    );
}
