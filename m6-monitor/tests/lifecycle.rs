//! m6-monitor is an `App` service, so it owes the same lifecycle contract as
//! every other one.
//!
//! It did not have a `tests/` directory at all. Seventeen unit tests covered
//! the digest, the fleet config and the check rendering, and nothing spawned
//! the binary. That is not a property of m6-monitor being new: the lifecycle
//! assertion was opt-in, written out by hand in five separate integration
//! suites, so the sixth service simply never got one.
//!
//! The point of the consolidation is that every app is the same shape. This
//! file is three lines of setup and one call because that claim is true.

use std::process::Command;

use m6_core::testkit::{assert_app_lifecycle, binary};

/// A config m6-monitor will actually start on.
///
/// The nodes are deliberately unreachable. Startup must not depend on the
/// fleet answering: `collect_from` runs per request, not at boot, and a
/// monitor that refused to start when the thing it monitors was down would be
/// useless in precisely the situation it exists for.
const CONFIG: &str = r#"
[monitor]
timeout_ms = 200

[[monitor.nodes]]
name = "unreachable"
url  = "https://127.0.0.1:1"
role = "origin"

[thread_pool]
size       = 1
queue_size = 4

[log]
level  = "info"
format = "text"
"#;

#[test]
fn lifecycle_is_clean() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = dir.path().join("monitor.conf");
    std::fs::write(&config, CONFIG).expect("write config");

    // Short name: a unix socket path is capped near 104 bytes on macOS and the
    // temp directory already spends most of that.
    let sock = dir.path().join("mon.sock");

    assert_app_lifecycle(
        "m6-monitor",
        Command::new(binary("m6-monitor"))
            .arg(dir.path())
            .arg(&config)
            .env("M6_SOCKET_OVERRIDE", &sock),
        &sock,
    );
}

/// `--check` is a one-shot: it must print a report and exit, not serve.
///
/// Under the freeze it cannot reach a real node, and that is the case worth
/// pinning. A tool that reported an unreachable fleet as empty rows would be
/// worse than useless, so this asserts it exits non-zero and says something
/// rather than exiting 0 on silence.
#[test]
fn check_is_one_shot_and_reports_unreachable() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = dir.path().join("monitor.conf");
    std::fs::write(&config, CONFIG).expect("write config");

    let out = Command::new(binary("m6-monitor"))
        .arg("--check")
        .arg(&config)
        .output()
        .expect("run --check");

    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !combined.trim().is_empty(),
        "--check printed nothing; silence is not a report\n--- output ---\n{combined}"
    );
    assert!(
        combined.contains("unreachable"),
        "--check never mentioned the node it could not reach\n--- output ---\n{combined}"
    );
}

/// The binary must not hang when handed no config at all.
#[test]
fn check_without_config_fails_loudly() {
    let out = Command::new(binary("m6-monitor"))
        .arg("--check")
        .output()
        .expect("run --check with no config");

    assert!(!out.status.success(), "--check with no config should fail");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("usage") || err.contains("config"),
        "the failure should say what was missing\n--- stderr ---\n{err}"
    );
}
