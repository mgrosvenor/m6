//! m6-http startup argument and system-config errors, exercised by running the
//! real binary.
//!
//! Every check here existed in `main.rs` or `config.rs` but had no test that
//! ran the built `m6-http` to assert it. A change to argument parsing or to the
//! merge order could turn a clear exit 2 into a start with the wrong config,
//! and nothing would fail. Each test below states the scenario and the wrong
//! outcome it catches.

use std::path::Path;
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use m6_core::testkit::{self, Service};

/// Run the built `m6-http` to completion and return its captured output.
fn run(args: &[&std::ffi::OsStr]) -> Output {
    Command::new(testkit::binary("m6-http"))
        .args(args)
        .output()
        .expect("run m6-http")
}

fn exit_code(out: &Output) -> i32 {
    out.status.code().expect("m6-http was killed by a signal")
}

fn stderr_of(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn stdout_of(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Write a self-signed certificate and key into `site`, so a non-redirect
/// config passes the `[server].tls_cert` / `tls_key` file-exists check.
fn write_cert_key(site: &Path) {
    let ck =
        rcgen::generate_simple_self_signed(vec!["localhost".to_string(), "127.0.0.1".to_string()])
            .expect("generate a self-signed certificate");
    std::fs::write(site.join("cert.pem"), ck.cert.pem()).expect("write cert.pem");
    std::fs::write(site.join("key.pem"), ck.key_pair.serialize_pem()).expect("write key.pem");
}

/// A `site.toml` that names the site, binds `bind`, and uses the certificate
/// `write_cert_key` wrote. Plain `info` logging so warnings are visible.
fn site_toml_with_server(bind: &str) -> String {
    format!(
        "[site]\nname = \"startup-errors\"\ndomain = \"example.com\"\n\n\
         [server]\nbind = \"{bind}\"\ntls_cert = \"cert.pem\"\ntls_key = \"key.pem\"\n\n\
         [log]\nlevel = \"info\"\nformat = \"plain\"\n"
    )
}

/// Wait until the service logs `needle`, with a deadline. This waits on the log
/// line itself, never a fixed sleep.
fn wait_for_output(svc: &mut Service, needle: &str, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        let out = svc.output();
        if out.contains(needle) {
            return;
        }
        svc.assert_alive(&format!("waiting for the log line {needle:?}"));
        if Instant::now() >= deadline {
            panic!("m6-http never logged {needle:?} within {timeout:?}\n--- output ---\n{out}");
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// Scenario: m6-http runs with no arguments. Wrong outcome: it starts with some
/// default config instead of refusing with exit 2 and a usage message.
#[test]
fn no_arguments_exits_2_with_a_usage_message_rather_than_starting() {
    let out = run(&[]);
    assert_eq!(exit_code(&out), 2, "stderr: {}", stderr_of(&out));
    let err = stderr_of(&out);
    assert!(err.contains("usage error"), "stderr: {err}");
    assert!(err.contains("Usage: m6-http"), "stderr: {err}");
}

/// Scenario: m6-http runs with the site dir but no system config. Wrong
/// outcome: it starts with a partial config instead of exit 2 and a usage
/// message.
#[test]
fn site_dir_without_a_system_config_exits_2_with_a_usage_message_rather_than_starting() {
    let dir = tempfile::tempdir().expect("tempdir");
    let site = dir.path().join("site");
    std::fs::create_dir_all(&site).expect("site dir");

    let out = run(&[site.as_os_str()]);
    assert_eq!(exit_code(&out), 2, "stderr: {}", stderr_of(&out));
    let err = stderr_of(&out);
    assert!(err.contains("usage error"), "stderr: {err}");
    assert!(err.contains("Usage: m6-http"), "stderr: {err}");
}

/// Scenario: the system config path names a file that does not exist. Wrong
/// outcome: the missing node state is silently skipped and the service starts.
#[test]
fn a_missing_system_config_exits_2_and_names_the_file_rather_than_starting() {
    let dir = tempfile::tempdir().expect("tempdir");
    let site = dir.path().join("site");
    std::fs::create_dir_all(&site).expect("site dir");
    std::fs::write(
        site.join("site.toml"),
        "[site]\nname = \"startup-errors\"\ndomain = \"example.com\"\n",
    )
    .expect("site.toml");
    let system = dir.path().join("missing-system.toml");

    let out = run(&[site.as_os_str(), system.as_os_str()]);
    assert_eq!(exit_code(&out), 2, "stderr: {}", stderr_of(&out));
    let err = stderr_of(&out);
    assert!(
        err.contains(&system.display().to_string()),
        "the error must name the file, got: {err}"
    );
}

/// Scenario: the system config is unparseable TOML. Wrong outcome: the parse
/// failure is swallowed and the service starts with only the site config.
#[test]
fn an_unparseable_system_config_exits_2_and_names_the_file_rather_than_starting() {
    let dir = tempfile::tempdir().expect("tempdir");
    let site = dir.path().join("site");
    std::fs::create_dir_all(&site).expect("site dir");
    std::fs::write(
        site.join("site.toml"),
        "[site]\nname = \"startup-errors\"\ndomain = \"example.com\"\n",
    )
    .expect("site.toml");
    let system = dir.path().join("broken-system.toml");
    std::fs::write(&system, "this is not valid toml [[[").expect("system config");

    let out = run(&[site.as_os_str(), system.as_os_str()]);
    assert_eq!(exit_code(&out), 2, "stderr: {}", stderr_of(&out));
    let err = stderr_of(&out);
    assert!(
        err.contains(&system.display().to_string()),
        "the error must name the file, got: {err}"
    );
}

/// Scenario: the system config carries a top-level key outside `[server]`,
/// `[node]`, `[auth]` and `[health]`. Wrong outcome: the key is dropped without
/// a word, so a typo in a config section passes silently.
#[test]
fn a_key_outside_the_allowed_tables_warns_naming_the_key_and_the_service_starts() {
    let claim = testkit::claim_port();
    let dir = tempfile::tempdir().expect("tempdir");
    let site = dir.path().join("site");
    std::fs::create_dir_all(&site).expect("site dir");
    write_cert_key(&site);
    std::fs::write(site.join("site.toml"), site_toml_with_server(&claim.addr()))
        .expect("site.toml");
    let system = dir.path().join("system.toml");
    std::fs::write(
        &system,
        "[node]\nname = \"startup-errors\"\n\n[frobnicate]\nsetting = \"value\"\n",
    )
    .expect("system config");

    let mut cmd = Command::new(testkit::binary("m6-http"));
    cmd.arg(&site).arg(&system);
    let mut svc = Service::spawn("m6-http-extra-key", &mut cmd);
    svc.wait_for_tcp(claim.port(), Duration::from_secs(10));

    wait_for_output(&mut svc, "frobnicate", Duration::from_secs(5));
    let out = svc.output();
    assert!(
        out.contains("ignoring key"),
        "the warning must say the key is ignored, got: {out}"
    );

    let status = svc.terminate(Duration::from_secs(5));
    assert!(
        status.success(),
        "m6-http should exit 0 on SIGTERM, got {status}"
    );
}

/// Scenario: `[server]` is absent from `site.toml` but present in the system
/// config. Wrong outcome: validation runs on the site config before the merge,
/// so the missing `[server]` is rejected even though the merge would supply it.
#[test]
fn server_in_the_system_config_only_starts_because_validation_runs_after_the_merge() {
    let claim = testkit::claim_port();
    let dir = tempfile::tempdir().expect("tempdir");
    let site = dir.path().join("site");
    std::fs::create_dir_all(&site).expect("site dir");
    write_cert_key(&site);
    std::fs::write(
        site.join("site.toml"),
        "[site]\nname = \"startup-errors\"\ndomain = \"example.com\"\n\n\
         [log]\nlevel = \"info\"\nformat = \"plain\"\n",
    )
    .expect("site.toml");
    let system = dir.path().join("system.toml");
    std::fs::write(
        &system,
        format!(
            "[node]\nname = \"startup-errors\"\n\n\
             [server]\nbind = \"{}\"\ntls_cert = \"cert.pem\"\ntls_key = \"key.pem\"\n",
            claim.addr()
        ),
    )
    .expect("system config");

    let mut cmd = Command::new(testkit::binary("m6-http"));
    cmd.arg(&site).arg(&system);
    let mut svc = Service::spawn("m6-http-server-in-system", &mut cmd);
    svc.wait_for_tcp(claim.port(), Duration::from_secs(10));

    let status = svc.terminate(Duration::from_secs(5));
    assert!(
        status.success(),
        "m6-http should exit 0 on SIGTERM, got {status}"
    );
}

/// Scenario: `--dump-config` with `[server]` only in the system config. Wrong
/// outcome: the dump omits the merged `[server]` and exits non-zero, so a
/// deploy cannot validate the merged config before cutting a node over.
#[test]
fn dump_config_prints_the_merged_server_from_the_system_file_and_exits_0() {
    let claim = testkit::claim_port();
    let dir = tempfile::tempdir().expect("tempdir");
    let site = dir.path().join("site");
    std::fs::create_dir_all(&site).expect("site dir");
    write_cert_key(&site);
    std::fs::write(
        site.join("site.toml"),
        "[site]\nname = \"startup-errors\"\ndomain = \"example.com\"\n",
    )
    .expect("site.toml");
    let system = dir.path().join("system.toml");
    std::fs::write(
        &system,
        format!(
            "[node]\nname = \"startup-errors\"\n\n\
             [server]\nbind = \"{}\"\ntls_cert = \"cert.pem\"\ntls_key = \"key.pem\"\n",
            claim.addr()
        ),
    )
    .expect("system config");

    let out = run(&[
        site.as_os_str(),
        system.as_os_str(),
        std::ffi::OsStr::new("--dump-config"),
    ]);
    assert_eq!(exit_code(&out), 0, "stderr: {}", stderr_of(&out));
    let stdout = stdout_of(&out);
    assert!(
        stdout.contains(&format!("\"bind\": \"{}\"", claim.addr())),
        "the merged [server] bind must come from the system file, got: {stdout}"
    );
}
