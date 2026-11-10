//! Startup behaviour when a resource the config names is missing.
//!
//! Two scenarios, each named after the wrong outcome it catches:
//!
//! - a missing `[keys] private_key` must stop the service before the socket
//!   appears, with the missing file named in the error. The wrong outcome is a
//!   service that starts and binds its socket without a signing key.
//! - a missing `[storage]` directory must be created on demand, with the
//!   database file, and the service must accept a connection. The wrong outcome
//!   is a config that points into a directory that does not exist being refused.
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

    /// Write the config naming `private_key`, `public_key` and the database at
    /// `db_path`, which the service resolves against the config file's directory.
    fn write_config(&self, private_key: &Path, public_key: &Path, db_path: &str) {
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
    site.write_config(&missing, &public, "data/auth.db");

    let mut cmd = site.command();
    let mut svc = Service::spawn("m6-auth-server", &mut cmd);
    let status = svc
        .wait_for_exit(Instant::now() + Duration::from_secs(10))
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
/// start, create the directory and the database file, and accept a connection.
///
/// The wrong outcome this catches: the service refuses a config that points into
/// a directory it could have created, so an operator must pre-create it by hand.
#[test]
fn missing_storage_directory_is_created_and_the_socket_accepts_a_connection() {
    let site = TestSite::new("missing-db-dir");
    let (private_pem, public_pem) = generate_test_rsa_keys();
    let private = site.key_dir.path().join("auth.pem");
    let public = site.key_dir.path().join("auth.pub");
    std::fs::write(&private, &private_pem).unwrap();
    std::fs::write(&public, &public_pem).unwrap();

    // Two directory levels that do not exist yet.
    let db_rel = "missing/deep/auth.db";
    site.write_config(&private, &public, db_rel);

    let mut cmd = site.command();
    let mut svc = Service::spawn("m6-auth-server", &mut cmd);
    // Readiness is a successful connection, not the socket file appearing.
    svc.wait_for_path(&site.socket_path, Duration::from_secs(10));
    assert!(
        wait::for_unix(&site.socket_path, Duration::from_secs(10)),
        "m6-auth-server never accepted a connection on {}\n--- output ---\n{}",
        site.socket_path.display(),
        svc.output()
    );

    let db_path = site.site_dir.join(db_rel);
    assert!(
        db_path.parent().unwrap().is_dir(),
        "the storage directory {} was not created\n--- output ---\n{}",
        db_path.parent().unwrap().display(),
        svc.output()
    );
    assert!(
        db_path.is_file(),
        "the database file {} was not created\n--- output ---\n{}",
        db_path.display(),
        svc.output()
    );
}
