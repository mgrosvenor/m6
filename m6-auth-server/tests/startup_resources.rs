//! Startup behaviour when a resource the config names is missing.
//!
//! Three scenarios, each named after the wrong outcome it catches:
//!
//! - a missing `[keys] private_key` must stop the service before the socket
//!   appears, with the missing file named in the error. The wrong outcome is a
//!   service that starts and binds its socket without a signing key.
//! - a missing `[storage]` directory must stop the service with exit 2, naming
//!   the setting, the directory and the remedy, and must not create it. The
//!   wrong outcome is a config that points into a directory that does not exist
//!   starting anyway, so the database lands somewhere the operator did not
//!   choose.
//! - a `[storage]` directory that exists must start and serve. The wrong
//!   outcome is refusing a correctly configured service.
//!
//! The fixtures mirror `integration.rs`. They live here rather than behind a
//! shared module because the integration test crate's helpers are private to
//! it, and this file needs only the key generation and the config write.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use m6_core::testkit::{binary, wait, Service};

/// Generate an RSA key pair with openssl: `(private_pem, public_pem)`.
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

/// A scratch site: a temp directory for the config and socket, and a separate
/// temp directory for the key files so a test can leave one unwritten.
struct TestSite {
    _temp: tempfile::TempDir,
    key_dir: tempfile::TempDir,
    site_dir: PathBuf,
    config_path: PathBuf,
    socket_path: PathBuf,
}

impl TestSite {
    fn new(id: &str) -> TestSite {
        let temp = tempfile::TempDir::new().unwrap();
        let key_dir = tempfile::TempDir::new().unwrap();
        let site_dir = temp.path().to_path_buf();
        let config_path = site_dir.join("m6-auth-test.conf");
        // The socket lives in this test's own temp directory, never a shared one.
        let socket_path = temp.path().join(format!("{id}.sock"));
        std::fs::write(site_dir.join("site.toml"), "").unwrap();

        TestSite {
            _temp: temp,
            key_dir,
            site_dir,
            config_path,
            socket_path,
        }
    }

    /// Write the config naming the two keys and the database path db_path.
    fn write_config(&self, private_key: &Path, public_key: &Path, db_path: &str) {
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
            private_key.display(),
            public_key.display(),
        );
        std::fs::write(&self.config_path, config).unwrap();
    }

    /// The command that runs the service against this site.
    fn command(&self) -> Command {
        let mut cmd = Command::new(binary("m6-auth-server"));
        cmd.arg(&self.site_dir)
            .arg(&self.config_path)
            .env("M6_SOCKET_OVERRIDE", &self.socket_path);
        cmd
    }
}

/// A config whose private key does not exist must refuse to start, name the
/// missing file in its error, and never bind the socket.
///
/// The wrong outcome this catches: the service starts and binds its socket with
/// no signing key, so a site that would answer every request with a signing
/// error looks healthy.
#[test]
fn missing_private_key_exits_nonzero_names_the_file_and_binds_no_socket() {
    let site = TestSite::new("missing-key");
    // The public key is never read: the missing private key is refused first.
    let public = site.key_dir.path().join("auth.pub");
    std::fs::write(&public, "unused").unwrap();
    // Named in the config but never written.
    let missing = site.key_dir.path().join("missing-auth.pem");
    let db_path = site.site_dir.join("data/auth.db").display().to_string();
    site.write_config(&missing, &public, &db_path);

    let mut cmd = site.command();
    let mut svc = Service::spawn("m6-auth-server", &mut cmd);
    let status = svc
        .wait_for_exit(Instant::now() + m6_core::testkit::STARTUP_BOUND)
        .unwrap_or_else(|| {
            panic!(
                "m6-auth-server did not exit for a missing private key\n--- output ---\n{}",
                svc.output()
            )
        });

    assert!(
        !status.success(),
        "m6-auth-server started (exit {status}) with a missing private key\n--- output ---\n{}",
        svc.output()
    );
    assert!(
        svc.output().contains(&missing.display().to_string()),
        "the error did not name the missing key file {}\n--- output ---\n{}",
        missing.display(),
        svc.output()
    );
    assert!(
        !site.socket_path.exists(),
        "the socket {} appeared even though the private key is missing\n--- output ---\n{}",
        site.socket_path.display(),
        svc.output()
    );
}

/// A config whose `[storage] path` sits in a directory that does not exist must
/// stop at start with exit 2, naming the setting, the directory and the remedy,
/// and must not create the directory or bind the socket.
///
/// The wrong outcome this catches: the service creates the directory and starts,
/// so a typo in `[storage] path` puts the database somewhere the operator did
/// not choose and looks healthy.
#[test]
fn a_missing_storage_directory_exits_2_and_names_the_setting_and_path() {
    let site = TestSite::new("missing-db-dir");
    let (private_pem, public_pem) = generate_test_rsa_keys();
    let private = site.key_dir.path().join("auth.pem");
    let public = site.key_dir.path().join("auth.pub");
    std::fs::write(&private, &private_pem).unwrap();
    std::fs::write(&public, &public_pem).unwrap();

    // Two directory levels that do not exist.
    let db_path = site.site_dir.join("missing/deep/auth.db");
    let db_dir = db_path.parent().unwrap().to_path_buf();
    site.write_config(&private, &public, &db_path.display().to_string());

    let mut cmd = site.command();
    let mut svc = Service::spawn("m6-auth-server", &mut cmd);
    let status = svc
        .wait_for_exit(Instant::now() + m6_core::testkit::STARTUP_BOUND)
        .unwrap_or_else(|| {
            panic!(
                "m6-auth-server did not exit for a missing [storage] directory\n--- output ---\n{}",
                svc.output()
            )
        });

    let output = svc.output();
    assert_eq!(
        status.code(),
        Some(2),
        "exit code should be 2\n--- output ---\n{output}"
    );
    assert!(
        output.contains("[storage] path"),
        "the error did not name [storage] path\n--- output ---\n{output}"
    );
    assert!(
        output.contains(&db_dir.display().to_string()),
        "the error did not name the missing directory {}\n--- output ---\n{output}",
        db_dir.display()
    );
    assert!(
        output.contains("create it, owned by") && output.contains("fix [storage] path"),
        "the error did not say what to do\n--- output ---\n{output}"
    );
    assert!(
        !db_dir.exists(),
        "the service created the missing storage directory {}\n--- output ---\n{output}",
        db_dir.display()
    );
    assert!(
        !site.socket_path.exists(),
        "the socket appeared despite a missing [storage] directory"
    );
}

/// A `[storage] path` whose directory exists starts, creates the database file
/// and accepts a connection.
///
/// The wrong outcome this catches: refusing a correctly configured service, so
/// an operator cannot run m6-auth-server at all.
#[test]
fn a_present_storage_directory_starts_and_accepts_a_connection() {
    let site = TestSite::new("present-db-dir");
    let (private_pem, public_pem) = generate_test_rsa_keys();
    let private = site.key_dir.path().join("auth.pem");
    let public = site.key_dir.path().join("auth.pub");
    std::fs::write(&private, &private_pem).unwrap();
    std::fs::write(&public, &public_pem).unwrap();

    let data = site.site_dir.join("data");
    std::fs::create_dir_all(&data).unwrap();
    let db_path = data.join("auth.db");
    site.write_config(&private, &public, &db_path.display().to_string());

    let mut cmd = site.command();
    let mut svc = Service::spawn("m6-auth-server", &mut cmd);
    svc.wait_for_path(&site.socket_path, m6_core::testkit::STARTUP_BOUND);
    svc.wait_for_started(m6_core::testkit::STARTUP_BOUND);
    assert!(
        wait::for_unix(&site.socket_path, m6_core::testkit::STARTUP_BOUND),
        "m6-auth-server never accepted a connection on {}\n--- output ---\n{}",
        site.socket_path.display(),
        svc.output()
    );
    assert!(
        db_path.is_file(),
        "the database file {} was not created\n--- output ---\n{}",
        db_path.display(),
        svc.output()
    );

    let status = svc.terminate(m6_core::testkit::STARTUP_BOUND);
    assert!(
        status.success(),
        "m6-auth-server should exit 0 on SIGTERM, got {status}\n--- output ---\n{}",
        svc.output()
    );
}

/// A relative `[storage] path` is refused at start, names the setting, binds no
/// socket and creates no database or directory.
#[test]
fn a_relative_storage_path_exits_2_and_names_the_setting() {
    let site = TestSite::new("relative-storage");
    let (private_pem, public_pem) = generate_test_rsa_keys();
    let private = site.key_dir.path().join("auth.pem");
    let public = site.key_dir.path().join("auth.pub");
    std::fs::write(&private, &private_pem).unwrap();
    std::fs::write(&public, &public_pem).unwrap();
    site.write_config(&private, &public, "data/auth.db");

    let mut cmd = site.command();
    let mut svc = Service::spawn("m6-auth-server", &mut cmd);
    let status = svc
        .wait_for_exit(Instant::now() + m6_core::testkit::STARTUP_BOUND)
        .unwrap_or_else(|| {
            panic!(
                "m6-auth-server did not exit for a relative [storage] path\n--- output ---\n{}",
                svc.output()
            )
        });

    assert_eq!(
        status.code(),
        Some(2),
        "exit code should be 2\n--- output ---\n{}",
        svc.output()
    );
    assert!(
        svc.output().contains("[storage] path"),
        "the error did not name [storage] path\n--- output ---\n{}",
        svc.output()
    );
    assert!(
        !site.socket_path.exists(),
        "the socket appeared despite a relative [storage] path"
    );
    assert!(
        !site.site_dir.join("data/auth.db").exists() && !site.site_dir.join("data").exists(),
        "the relative database or its directory was created"
    );
}

/// A relative `[keys] private_key` is refused at start and names the setting.
#[test]
fn a_relative_private_key_exits_2_and_names_the_setting() {
    let site = TestSite::new("relative-private-key");
    let (private_pem, public_pem) = generate_test_rsa_keys();
    let public = site.key_dir.path().join("auth.pub");
    std::fs::write(&public, &public_pem).unwrap();
    std::fs::write(site.site_dir.join("auth.pem"), &private_pem).unwrap();

    let db_path = site.site_dir.join("data/auth.db").display().to_string();
    let config = format!(
        r#"[storage]
path = "{}"

[tokens]
access_ttl  = 900
refresh_ttl = 2592000
issuer      = "test.example.com"

[keys]
private_key = "auth.pem"
public_key  = "{}"
"#,
        db_path,
        public.display(),
    );
    std::fs::write(&site.config_path, config).unwrap();

    let mut cmd = site.command();
    let mut svc = Service::spawn("m6-auth-server", &mut cmd);
    let status = svc
        .wait_for_exit(Instant::now() + m6_core::testkit::STARTUP_BOUND)
        .unwrap_or_else(|| {
            panic!(
                "m6-auth-server did not exit for a relative [keys] private_key\n--- output ---\n{}",
                svc.output()
            )
        });

    assert_eq!(
        status.code(),
        Some(2),
        "exit code should be 2\n--- output ---\n{}",
        svc.output()
    );
    assert!(
        svc.output().contains("[keys] private_key"),
        "the error did not name [keys] private_key\n--- output ---\n{}",
        svc.output()
    );
    assert!(
        !site.socket_path.exists(),
        "the socket appeared despite a relative [keys] private_key"
    );
}

/// The check side signs nothing, so it starts without the private key file.
///
/// The wrong outcome this catches: a check process refuses to start because a
/// key it never uses is absent, so a deployment that delivers only the public
/// key to the check unit cannot run it.
#[test]
fn check_only_starts_without_the_private_key() {
    let site = TestSite::new("check-only-no-private-key");
    let (_, public_pem) = generate_test_rsa_keys();
    let public = site.key_dir.path().join("auth.pub");
    std::fs::write(&public, &public_pem).unwrap();
    // Named in the config and never written. The login side refuses this
    // config, which the test above pins.
    let missing = site.key_dir.path().join("missing-auth.pem");
    let data = site.site_dir.join("data");
    std::fs::create_dir_all(&data).unwrap();
    let db_path = data.join("auth.db");
    // The login side owns the schema. Create it as that side does.
    drop(m6_auth::Db::open(&db_path).expect("create the database"));
    site.write_config(&missing, &public, &db_path.display().to_string());

    let check = site.site_dir.join("check.sock");
    let mut cmd = site.command();
    cmd.arg("--check-only")
        .env("M6_CHECK_SOCKET_OVERRIDE", &check);
    let mut svc = Service::spawn("m6-auth-server", &mut cmd);
    svc.wait_for_path(&check, m6_core::testkit::STARTUP_BOUND);
    svc.wait_for_started(m6_core::testkit::STARTUP_BOUND);
    assert!(
        !missing.exists(),
        "the test wrote the private key it says is missing"
    );
    assert!(
        wait::for_unix(&check, Duration::from_secs(10)),
        "the check-only process never accepted a connection on {}\n--- output ---\n{}",
        check.display(),
        svc.output()
    );
    let status = svc.terminate(m6_core::testkit::STARTUP_BOUND);
    assert!(
        status.success(),
        "the check-only process should exit 0 on SIGTERM, got {status}\n--- output ---\n{}",
        svc.output()
    );
}

/// The check side never creates the database file. A file the login side has
/// not created stops the check process at start, and the start creates nothing.
#[test]
fn check_only_does_not_create_the_database() {
    let site = TestSite::new("check-only-no-database");
    let (_, public_pem) = generate_test_rsa_keys();
    let public = site.key_dir.path().join("auth.pub");
    std::fs::write(&public, &public_pem).unwrap();
    let missing = site.key_dir.path().join("missing-auth.pem");
    let data = site.site_dir.join("data");
    std::fs::create_dir_all(&data).unwrap();
    let db_path = data.join("auth.db");
    site.write_config(&missing, &public, &db_path.display().to_string());
    let check = site.site_dir.join("check.sock");

    let mut cmd = site.command();
    cmd.arg("--check-only")
        .env("M6_CHECK_SOCKET_OVERRIDE", &check);
    let mut svc = Service::spawn("m6-auth-server", &mut cmd);
    let status = svc
        .wait_for_exit(Instant::now() + m6_core::testkit::STARTUP_BOUND)
        .unwrap_or_else(|| {
            panic!(
                "the check-only process did not exit for a missing database\n--- output ---\n{}",
                svc.output()
            )
        });
    let output = svc.output();
    assert_eq!(status.code(), Some(2), "output:\n{output}");
    assert!(
        output.contains("[storage] path") && output.contains(&db_path.display().to_string()),
        "the error did not name the setting and the database it could not open\n--- output ---\n{output}"
    );
    assert!(
        !db_path.exists(),
        "the check-only process created {}\n--- output ---\n{output}",
        db_path.display()
    );
    assert!(!check.exists(), "the check socket appeared anyway");
}

/// A database file with no auth schema stops the check process at start with a
/// message that names the side that owns the schema.
#[test]
fn check_only_refuses_a_database_without_the_schema() {
    let site = TestSite::new("check-only-no-schema");
    let (_, public_pem) = generate_test_rsa_keys();
    let public = site.key_dir.path().join("auth.pub");
    std::fs::write(&public, &public_pem).unwrap();
    let missing = site.key_dir.path().join("missing-auth.pem");
    let data = site.site_dir.join("data");
    std::fs::create_dir_all(&data).unwrap();
    let db_path = data.join("auth.db");
    std::fs::write(&db_path, b"").unwrap();
    site.write_config(&missing, &public, &db_path.display().to_string());
    let check = site.site_dir.join("check.sock");

    let mut cmd = site.command();
    cmd.arg("--check-only")
        .env("M6_CHECK_SOCKET_OVERRIDE", &check);
    let mut svc = Service::spawn("m6-auth-server", &mut cmd);
    let status = svc
        .wait_for_exit(Instant::now() + m6_core::testkit::STARTUP_BOUND)
        .unwrap_or_else(|| {
            panic!(
                "the check-only process did not exit for a database without a schema\n--- output ---\n{}",
                svc.output()
            )
        });
    let output = svc.output();
    assert_eq!(status.code(), Some(2), "output:\n{output}");
    assert!(
        output.contains("[storage] path") && output.contains("no auth schema"),
        "the error did not name the setting and the missing schema\n--- output ---\n{output}"
    );
    assert!(!check.exists(), "the check socket appeared anyway");
}
