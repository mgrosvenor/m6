//! The event series end to end: a config declares one, a journal supplies the
//! lines, and the report counts them.
//!
//! `journalctl` is replaced by a script on `PATH` that prints canned entries.
//! That is the only way to drive the reader without a journal directory, and it
//! also proves the SELECTION: the script answers only when it is asked for
//! `SYSLOG_IDENTIFIER=example-app`, so a reader that asked for a `M6_TYPE`
//! instead would receive nothing and the counts would be zero.

#![allow(
    clippy::disallowed_methods,
    reason = "test code: processes, blocking clients and file modes drive the service from outside"
)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use m6_core::testkit::{binary, wait, Service};

/// A config m6-monitor starts on: one unreachable node, one declared series.
///
/// The identifier and the messages are made up. The engine is generic, so its
/// test fixture is too.
const CONFIG: &str = r#"
poll_interval_s = 60

[monitor]
timeout_ms = 200
journal_dir = "journal"

[[monitor.nodes]]
name = "unreachable"
url  = "https://127.0.0.1:1"
role = "origin"

[[monitor.event_series]]
identifier  = "example-app"
accepted    = "job accepted"
rejected    = "job rejected"
split_by = "credential_present"
fault_field = "error_codes"
fault_codes = ["invalid-credential", "missing-credential"]

[log]
level  = "info"
"#;

/// The canned entries, one per line, as `journalctl --output=json` prints them.
///
/// One accepted event, one refusal that carried a credential and named the
/// service's own configuration, and one refusal that carried none at all.
const ENTRIES: &str = r#"{"__REALTIME_TIMESTAMP":"1700000000000010","SYSLOG_IDENTIFIER":"example-app","MESSAGE":"{\"timestamp\":\"2026-11-23T01:00:00Z\",\"type\":\"event\",\"fields\":{\"message\":\"job accepted\"}}"}
{"__REALTIME_TIMESTAMP":"1700000000000020","SYSLOG_IDENTIFIER":"example-app","MESSAGE":"{\"timestamp\":\"2026-11-23T01:00:01Z\",\"type\":\"event\",\"fields\":{\"message\":\"job rejected\",\"credential_present\":true,\"error_codes\":\"invalid-credential\"}}"}
{"__REALTIME_TIMESTAMP":"1700000000000030","SYSLOG_IDENTIFIER":"example-app","MESSAGE":"{\"timestamp\":\"2026-11-23T01:00:02Z\",\"type\":\"event\",\"fields\":{\"message\":\"job rejected\",\"credential_present\":false,\"error_codes\":\"\"}}"}"#;

/// A `journalctl` that answers EVERY read with the same entries.
///
/// That is the worst case for the overlap: both readers deliver the same
/// three entries, on their own schedules, and the counts must still be one
/// accepted and two refused. A fake that answered only the event read would
/// prove the selection and leave the overlap untested.
fn fake_journalctl(dir: &Path) -> PathBuf {
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).expect("create the fake bin directory");
    let path = bin.join("journalctl");
    let script = format!(
        r#"#!/bin/sh
cat <<'ENTRIES'
{ENTRIES}
ENTRIES
"#
    );
    std::fs::write(&path, script).expect("write the fake journalctl");
    let mut mode = std::fs::metadata(&path).expect("stat").permissions();
    mode.set_mode(0o755);
    std::fs::set_permissions(&path, mode).expect("chmod");
    path
}

fn setup(config: &str) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(dir.path().join("journal")).expect("create the journal directory");
    std::fs::write(dir.path().join("site.toml"), "").expect("write site.toml");
    fake_journalctl(dir.path());
    let config_path = dir.path().join("monitor.conf");
    std::fs::write(&config_path, config).expect("write the config");
    (dir, config_path)
}

fn run_check(dir: &Path, config: &Path) -> (bool, String) {
    let out = Command::new(binary("m6-monitor"))
        .arg("--check")
        .arg(config)
        .current_dir(dir)
        .env(
            "PATH",
            format!(
                "{}:{}",
                dir.join("bin").display(),
                std::env::var("PATH").unwrap_or_default()
            ),
        )
        .output()
        .expect("run --check");
    (
        out.status.success(),
        format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ),
    )
}

/// **The whole path, once.** A declared series, a journal line per message, and
/// the report counting them. The fault comes from the one refusal that named
/// the service's own configuration, and the refusal with no credential is
/// counted without one.
#[test]
fn the_report_counts_a_declared_series_and_faults_on_its_own_code() {
    let (dir, config) = setup(CONFIG);
    let (ok, out) = run_check(dir.path(), &config);

    assert!(out.contains("EVENT SERIES"), "no section:\n{out}");
    assert!(out.contains("example-app"), "no series named:\n{out}");
    assert!(
        out.contains("1 accepted, 2 refused (1 with a credential, 1 without)"),
        "the counts are wrong. Both readers delivered the same three entries, so a \
         count above these numbers is an overlap counted twice:\n{out}"
    );
    assert!(
        out.contains("FAULT: `invalid-credential` at 2026-11-23T01:00:01Z"),
        "the refusal that is the service's own fault is not a fault:\n{out}"
    );
    assert!(
        !ok,
        "--check must exit non-zero when a series is standing on a fault:\n{out}"
    );
}

/// A refusal with no credential is a count, and the page shows all clear when
/// that is all the journal holds. The nodes are still unreachable in this
/// fixture, so the run faults for that reason and this asserts on the section.
#[test]
fn refusals_without_a_credential_are_counted_and_never_a_fault() {
    let config = CONFIG.to_string();
    let (dir, config_path) = setup(&config);
    // Replace the entries with refusals that carry no credential at all.
    let bin = dir.path().join("bin").join("journalctl");
    let script = std::fs::read_to_string(&bin).expect("read the fake");
    let only_refusals = r#"{"__REALTIME_TIMESTAMP":"1700000000000010","SYSLOG_IDENTIFIER":"example-app","MESSAGE":"{\"timestamp\":\"2026-11-23T01:00:00Z\",\"type\":\"event\",\"fields\":{\"message\":\"job rejected\",\"credential_present\":false,\"error_codes\":\"\"}}"}
{"__REALTIME_TIMESTAMP":"1700000000000020","SYSLOG_IDENTIFIER":"example-app","MESSAGE":"{\"timestamp\":\"2026-11-23T01:00:01Z\",\"type\":\"event\",\"fields\":{\"message\":\"job rejected\",\"credential_present\":false,\"error_codes\":\"\"}}"}"#;
    let replaced = script
        .split("cat <<'ENTRIES'")
        .next()
        .expect("the script's head")
        .to_string()
        + &format!("cat <<'ENTRIES'\n{only_refusals}\nENTRIES\n");
    std::fs::write(&bin, replaced).expect("write the fake");

    let (_, out) = run_check(dir.path(), &config_path);
    assert!(
        out.contains("0 accepted, 2 refused (0 with a credential, 2 without)"),
        "the split is wrong:\n{out}"
    );
    assert!(
        !out.contains("FAULT: `"),
        "a refusal with no credential must never be a fault:\n{out}"
    );
}

/// A series that could not do what it says stops the start, naming the table.
#[test]
fn a_broken_series_stops_the_start() {
    let config = CONFIG.replace(
        r#"fault_field = "error_codes"
fault_codes = ["invalid-credential", "missing-credential"]"#,
        r#"fault_codes = ["invalid-credential"]"#,
    );
    let (dir, config_path) = setup(&config);
    let out = Command::new(binary("m6-monitor"))
        .arg("--check")
        .arg(&config_path)
        .current_dir(dir.path())
        .output()
        .expect("run --check");
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !out.status.success(),
        "the start must be refused:\n{combined}"
    );
    assert!(
        combined.contains("event_series[0]"),
        "the refusal must name the table:\n{combined}"
    );
}

/// A series is an ADDITION to the start, not a precondition: a config that
/// declares one starts the service.
#[test]
fn a_config_with_a_declared_series_starts() {
    let (dir, config_path) = setup(CONFIG);
    let sock = dir.path().join("mon.sock");
    let mut svc = Service::spawn(
        "m6-monitor",
        Command::new(binary("m6-monitor"))
            .arg(dir.path())
            .arg(&config_path)
            .env("M6_SOCKET_OVERRIDE", &sock)
            .env(
                "PATH",
                format!(
                    "{}:{}",
                    dir.path().join("bin").display(),
                    std::env::var("PATH").unwrap_or_default()
                ),
            ),
    );
    assert!(
        wait::for_unix(&sock, std::time::Duration::from_secs(60)),
        "the monitor did not start with a declared series\n{}",
        svc.output()
    );
    svc.terminate(std::time::Duration::from_secs(10));
}
