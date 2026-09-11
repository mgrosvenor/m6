//! `[server] socket_mode`, checked on the socket rather than in the config.
//!
//! `App` set no mode at all, so its five services took whatever the umask gave
//! them, typically `0o755`. m6-file and m6-auth-server each set `0o666` by
//! hand. Neither was a decision anybody made: both worked because `/run/m6` is
//! `0750` and owned by `m6`, so the directory was doing all the enforcing.
//!
//! What is asserted here is that the configured mode reaches the file. A
//! permissions key that is parsed and never applied reads exactly like one that
//! works, right up until the directory mode changes.

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use m6_core::testkit::{binary, Service};

fn fixtures_dir() -> PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join("fixtures")
}

/// Spawn `m6-html` with `body` as the whole of its `[server]` section and
/// return the mode the socket ended up with.
fn socket_mode_for(id: &str, server_section: &str) -> u32 {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = dir.path().join("m6-html.conf");
    std::fs::write(
        &config,
        format!(
            "global_params = [\"data/site.json\"]\n\
             secrets_file = \"/nonexistent/path/secrets.toml\"\n\
             {server_section}\n\
             [[route]]\n\
             path = \"/\"\n\
             template = \"templates/home.html\"\n\
             params = [\"content/pages/index.json\"]\n"
        ),
    )
    .expect("write config");

    let socket_path = dir.path().join(format!("{id}.sock"));
    let mut svc = Service::spawn(
        "m6-html",
        Command::new(binary("m6-html"))
            .arg(fixtures_dir())
            .arg(&config)
            .env("M6_SOCKET_OVERRIDE", &socket_path),
    );
    svc.wait_for_path(&socket_path, Duration::from_secs(10));

    let mode = std::fs::metadata(&socket_path)
        .expect("socket metadata")
        .permissions()
        .mode()
        & 0o777;

    svc.assert_alive("after reading the socket mode");
    mode
}

/// The default is the one that matters: no production config sets this key.
#[test]
fn the_default_socket_mode_is_0660() {
    let mode = socket_mode_for("dflt", "");
    assert_eq!(
        mode, 0o660,
        "expected 0660, got {mode:04o}; an App socket used to take whatever \
         the umask gave it"
    );
}

/// And the key is honoured, so the default is a choice rather than the only
/// thing the code can do.
#[test]
fn a_configured_socket_mode_is_applied() {
    let mode = socket_mode_for("cfgd", "[server]\nsocket_mode = \"0600\"");
    assert_eq!(mode, 0o600, "expected 0600, got {mode:04o}");
}

/// A mode that cannot be understood stops the service rather than leaving a
/// socket open at a mode nobody chose.
#[test]
fn a_nonsense_socket_mode_stops_the_service() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = dir.path().join("m6-html.conf");
    std::fs::write(&config, "[server]\nsocket_mode = \"rw-rw----\"\n").expect("write config");
    let socket_path = dir.path().join("bad.sock");

    let out = Command::new(binary("m6-html"))
        .arg(fixtures_dir())
        .arg(&config)
        .env("M6_SOCKET_OVERRIDE", &socket_path)
        .output()
        .expect("spawn");

    assert!(!out.status.success(), "a bad socket_mode should not start");
    assert!(
        !socket_path.exists(),
        "the socket was bound despite the config being refused"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("socket_mode"),
        "the refusal should name the key, got:\n{stderr}"
    );
}
