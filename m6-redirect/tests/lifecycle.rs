//! m6-redirect's lifecycle: the same contract every m6 service owes.
//!
//! The redirect engine is one process running one event loop (`main.rs`), so
//! it owes the started line, a clean SIGTERM and the socket removed. This test
//! spawns the binary and checks what a supervisor and an operator see.

#![allow(
    clippy::disallowed_methods,
    reason = "test code: blocking clients drive the service from outside"
)]

use std::path::Path;
use std::process::Command;

use m6_core::testkit::{assert_app_lifecycle, binary};

fn setup() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
    let dir = tempfile::tempdir_in(m6_core::testkit::socket_scratch_root()).expect("tempdir");
    let config = dir.path().join("redirect.conf");
    std::fs::write(&config, "").unwrap();
    std::fs::write(dir.path().join("site.toml"), "").unwrap();
    // Short name: a unix socket path is capped near 104 bytes on macOS.
    let sock = dir.path().join("redir.sock");
    (dir, config, sock)
}

fn command(dir: &Path, config: &Path, sock: &Path) -> Command {
    let mut cmd = Command::new(binary("m6-redirect"));
    cmd.arg(dir).arg(config).env("M6_SOCKET_OVERRIDE", sock);
    cmd
}

#[test]
fn lifecycle_is_clean() {
    let (dir, config, sock) = setup();
    assert_app_lifecycle(
        "m6-redirect",
        &mut command(dir.path(), &config, &sock),
        &sock,
    );
}

/// The dump runs the same `[redirect]` parse as the start, so a bad value
/// fails the dump the way it fails the start.
#[test]
fn dump_config_refuses_a_bad_redirect_table() {
    let dir = tempfile::tempdir_in(m6_core::testkit::socket_scratch_root()).expect("tempdir");
    let config = dir.path().join("redirect.conf");
    std::fs::write(&config, "[redirect]\nmax_age_secs = \"forever\"\n").unwrap();
    std::fs::write(dir.path().join("site.toml"), "").unwrap();
    let sock = dir.path().join("redir.sock");

    let out = command(dir.path(), &config, &sock)
        .arg("--dump-config")
        .output()
        .expect("run --dump-config");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        out.status.code(),
        Some(2),
        "--dump-config accepted a config the start refuses\n--- stderr ---\n{stderr}"
    );
    assert!(
        stderr.contains("max_age_secs"),
        "the dump did not name the key it refused\n--- stderr ---\n{stderr}"
    );
}

/// A valid config dumps and prints the line a deploy's check reads.
#[test]
fn dump_config_accepts_a_valid_config() {
    let (dir, config, sock) = setup();
    let out = command(dir.path(), &config, &sock)
        .arg("--dump-config")
        .output()
        .expect("run --dump-config");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        out.status.code(),
        Some(0),
        "a valid config must dump\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}"
    );
    assert!(
        stdout.contains("ok: this binary can serve every route in this config"),
        "the dump did not print the line check-configs reads\n--- stdout ---\n{stdout}"
    );
}
