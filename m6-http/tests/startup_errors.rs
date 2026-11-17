//! m6-http startup argument and system-config errors, exercised by running the
//! real binary.
//!
//! Every check here existed in `main.rs` or `config.rs` but had no test that
//! ran the built `m6-http` to assert it. A change to argument parsing or to the
//! merge order could turn a clear exit 2 into a start with the wrong config,
//! and nothing would fail. Each test below states the scenario and the wrong
//! outcome it catches.

use std::path::{Path, PathBuf};
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
fn site_toml() -> String {
    "[site]\nname = \"startup-errors\"\ndomain = \"example.com\"\n\n\
     [log]\nlevel = \"info\"\n\n\
     [analytics]\nenabled = false\n"
        .to_string()
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
fn a_key_outside_the_allowed_tables_exits_2_and_names_it() {
    let claim = testkit::claim_port();
    let dir = tempfile::tempdir().expect("tempdir");
    let site = dir.path().join("site");
    std::fs::create_dir_all(&site).expect("site dir");
    write_cert_key(&site);
    std::fs::write(site.join("site.toml"), site_toml()).expect("site.toml");
    let system = dir.path().join("system.toml");
    std::fs::write(
        &system,
        format!(
            "[node]\nname = \"startup-errors\"\n\n\
             [server]\nbind = \"{}\"\ntls_cert = \"{}\"\ntls_key = \"{}\"\n\n\
             [frobnicate]\nsetting = \"value\"\n",
            claim.addr(),
            site.join("cert.pem").display(),
            site.join("key.pem").display(),
        ),
    )
    .expect("system config");

    let out = run(&[site.as_os_str(), system.as_os_str()]);
    let err = stderr_of(&out);
    assert_eq!(exit_code(&out), 2, "stderr: {err}");
    assert!(
        err.contains("frobnicate"),
        "the error must name the unknown key, got: {err}"
    );
}

/// Scenario: `[server] bind` in `site.toml`, which is this node's own state.
/// Wrong outcome: the key is read from the wrong file and the service starts.
#[test]
fn a_misplaced_key_exits_2_and_names_it() {
    let claim = testkit::claim_port();
    let dir = tempfile::tempdir().expect("tempdir");
    let site = dir.path().join("site");
    std::fs::create_dir_all(&site).expect("site dir");
    write_cert_key(&site);
    std::fs::write(
        site.join("site.toml"),
        format!(
            "[site]\nname = \"startup-errors\"\ndomain = \"example.com\"\n\n\
             [analytics]\nenabled = false\n\n\
             [server]\nbind = \"{}\"\ntls_cert = \"{}\"\ntls_key = \"{}\"\n",
            claim.addr(),
            site.join("cert.pem").display(),
            site.join("key.pem").display(),
        ),
    )
    .expect("site.toml");
    let system = dir.path().join("system.toml");
    std::fs::write(
        &system,
        format!(
            "[node]\nname = \"startup-errors\"\n\n\
             [server]\nbind = \"{}\"\ntls_cert = \"{}\"\ntls_key = \"{}\"\n",
            claim.addr(),
            site.join("cert.pem").display(),
            site.join("key.pem").display(),
        ),
    )
    .expect("system config");

    let out = run(&[site.as_os_str(), system.as_os_str()]);
    let err = stderr_of(&out);
    assert_eq!(exit_code(&out), 2, "stderr: {err}");
    assert!(
        err.contains("site.toml sets [server].bind"),
        "the error must name the key and the file it belongs in, got: {err}"
    );
    assert!(err.contains("belongs in the system config"), "{err}");
}

/// Scenario: a system config with no `[node]`. Wrong outcome: the node identity
/// falls back to `[site].name` and the service starts.
#[test]
fn a_missing_node_exits_2() {
    let claim = testkit::claim_port();
    let dir = tempfile::tempdir().expect("tempdir");
    let site = dir.path().join("site");
    std::fs::create_dir_all(&site).expect("site dir");
    write_cert_key(&site);
    std::fs::write(site.join("site.toml"), site_toml()).expect("site.toml");
    let system = dir.path().join("system.toml");
    std::fs::write(
        &system,
        format!(
            "[server]\nbind = \"{}\"\ntls_cert = \"{}\"\ntls_key = \"{}\"\n",
            claim.addr(),
            site.join("cert.pem").display(),
            site.join("key.pem").display(),
        ),
    )
    .expect("system config");

    let out = run(&[site.as_os_str(), system.as_os_str()]);
    let err = stderr_of(&out);
    assert_eq!(exit_code(&out), 2, "stderr: {err}");
    assert!(err.contains("has no [node]"), "{err}");
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
         [log]\nlevel = \"info\"\n\n\
         [analytics]\nenabled = false\n",
    )
    .expect("site.toml");
    let system = dir.path().join("system.toml");
    std::fs::write(
        &system,
        format!(
            "[node]\nname = \"startup-errors\"\n\n\
             [server]\nbind = \"{}\"\ntls_cert = \"{}\"\ntls_key = \"{}\"\n",
            claim.addr(),
            site.join("cert.pem").display(),
            site.join("key.pem").display(),
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

/// Scenario: two `[[route]]` patterns that can match one path, checked with
/// `--dump-config` before a deploy. The wrong outcome: the dump prints the
/// config and exits 0, and the start then refuses the same config.
#[test]
fn dump_config_refuses_routes_that_can_match_one_path() {
    let claim = testkit::claim_port();
    let dir = tempfile::tempdir().expect("tempdir");
    let site = dir.path().join("site");
    std::fs::create_dir_all(&site).expect("site dir");
    write_cert_key(&site);
    std::fs::write(
        site.join("site.toml"),
        "[site]\nname = \"startup-errors\"\ndomain = \"example.com\"\n\n\
         [analytics]\nenabled = false\n\n\
         [[backend]]\nname = \"b\"\nsockets = \"/run/m6/*.sock\"\n\n\
         [[route]]\npath = \"/blog/{slug}\"\nbackend = \"b\"\n\n\
         [[route]]\npath = \"/{lang}/about\"\nbackend = \"b\"\n",
    )
    .expect("site.toml");
    let system = dir.path().join("system.toml");
    std::fs::write(
        &system,
        format!(
            "[node]\nname = \"startup-errors\"\n\n\
             [server]\nbind = \"{}\"\ntls_cert = \"{}\"\ntls_key = \"{}\"\n",
            claim.addr(),
            site.join("cert.pem").display(),
            site.join("key.pem").display(),
        ),
    )
    .expect("system config");

    let out = run(&[
        site.as_os_str(),
        system.as_os_str(),
        std::ffi::OsStr::new("--dump-config"),
    ]);
    let stderr = stderr_of(&out);
    assert_eq!(exit_code(&out), 2, "stderr: {stderr}");
    assert!(
        stderr.contains("\"/blog/{slug}\"")
            && stderr.contains("\"/{lang}/about\"")
            && stderr.contains("can both answer GET /blog/about"),
        "both routes and an example request must be named, got: {stderr}"
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
        "[site]\nname = \"startup-errors\"\ndomain = \"example.com\"\n\n\
         [analytics]\nenabled = false\n",
    )
    .expect("site.toml");
    let system = dir.path().join("system.toml");
    std::fs::write(
        &system,
        format!(
            "[node]\nname = \"startup-errors\"\n\n\
             [server]\nbind = \"{}\"\ntls_cert = \"{}\"\ntls_key = \"{}\"\n",
            claim.addr(),
            site.join("cert.pem").display(),
            site.join("key.pem").display(),
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

/// Scenario: a `[[backend]]` pool whose socket glob matches no file when
/// m6-http starts. Wrong outcome: m6-http starts with an empty pool and answers
/// 502 for the route forever instead of refusing with exit 2.
#[test]
fn a_pool_whose_glob_matches_no_socket_exits_2() {
    let claim = testkit::claim_port();
    let dir = tempfile::tempdir().expect("tempdir");
    let site = dir.path().join("site");
    std::fs::create_dir_all(&site).expect("site dir");
    write_cert_key(&site);
    let sockets = dir.path().join("sockets");
    std::fs::create_dir_all(&sockets).expect("socket dir");
    std::fs::write(
        site.join("site.toml"),
        format!(
            "[site]\nname = \"startup-errors\"\ndomain = \"example.com\"\n\n\
             [log]\nlevel = \"info\"\n\n\
             [analytics]\nenabled = false\n\n\
             [[backend]]\nname = \"b\"\nsockets = \"{}\"\n\n\
             [[route]]\npath = \"/\"\nbackend = \"b\"\n",
            sockets.join("*.sock").display()
        ),
    )
    .expect("site.toml");
    let system = dir.path().join("system.toml");
    std::fs::write(
        &system,
        format!(
            "[node]\nname = \"startup-errors\"\n\n\
             [server]\nbind = \"{}\"\ntls_cert = \"{}\"\ntls_key = \"{}\"\n",
            claim.addr(),
            site.join("cert.pem").display(),
            site.join("key.pem").display(),
        ),
    )
    .expect("system config");

    let out = run(&[site.as_os_str(), system.as_os_str()]);
    assert_eq!(exit_code(&out), 2, "stderr: {}", stderr_of(&out));
    let err = stderr_of(&out);
    assert!(
        err.contains("matches no socket"),
        "the error must say the glob matches no socket, got: {err}"
    );
    assert!(
        err.contains("[[backend]] b:"),
        "the error must name the backend, got: {err}"
    );
}

/// A relative `[server] tls_key` is refused at start and names the setting,
/// even though the key file is present in the site directory.
#[test]
fn a_relative_tls_key_exits_2_and_names_the_setting() {
    let dir = tempfile::tempdir().expect("tempdir");
    let site = dir.path().join("site");
    std::fs::create_dir_all(&site).expect("site dir");
    write_cert_key(&site);
    std::fs::write(site.join("site.toml"), site_toml()).expect("site.toml");
    let system = dir.path().join("system.toml");
    std::fs::write(
        &system,
        format!(
            "[node]\nname = \"startup-errors\"\n\n\
             [server]\nbind = \"127.0.0.1:8443\"\ntls_cert = \"{}/cert.pem\"\ntls_key = \"key.pem\"\n",
            site.display()
        ),
    )
    .expect("system config");

    let mut cmd = Command::new(testkit::binary("m6-http"));
    cmd.arg(&site).arg(&system);
    let mut svc = Service::spawn("m6-http", &mut cmd);
    let status = svc
        .wait_for_exit(Instant::now() + Duration::from_secs(10))
        .unwrap_or_else(|| {
            panic!(
                "m6-http did not exit for a relative [server] tls_key\n--- output ---\n{}",
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
        svc.output().contains("[server] tls_key"),
        "the error did not name [server] tls_key\n--- output ---\n{}",
        svc.output()
    );
}

/// Write a site holding the `[[backend]]` line `backend` and a route to it,
/// and a system config bound to `bind`. Returns the site and system paths.
fn url_backend_site(dir: &Path, bind: &str, backend: &str) -> (PathBuf, PathBuf) {
    let site = dir.join("site");
    std::fs::create_dir_all(&site).expect("site dir");
    write_cert_key(&site);
    std::fs::write(
        site.join("site.toml"),
        format!(
            "[site]\nname = \"startup-errors\"\ndomain = \"example.com\"\n\n\
             [log]\nlevel = \"info\"\n\n\
             [analytics]\nenabled = false\n\n\
             {backend}\n\n\
             [[route]]\npath = \"/\"\nbackend = \"remote\"\n"
        ),
    )
    .expect("site.toml");
    let system = dir.join("system.toml");
    std::fs::write(
        &system,
        format!(
            "[node]\nname = \"startup-errors\"\n\n\
             [server]\nbind = \"{bind}\"\ntls_cert = \"{}\"\ntls_key = \"{}\"\n",
            site.join("cert.pem").display(),
            site.join("key.pem").display(),
        ),
    )
    .expect("system config");
    (site, system)
}

/// An empty PEM file and an empty directory, which `SSL_CERT_FILE` and
/// `SSL_CERT_DIR` make replace the host's certificate store.
fn empty_store(dir: &Path) -> (PathBuf, PathBuf) {
    let file = dir.join("empty-ca.pem");
    std::fs::write(&file, "").expect("empty CA file");
    let dir_path = dir.join("empty-ca-dir");
    std::fs::create_dir(&dir_path).expect("empty CA dir");
    (file, dir_path)
}

/// Scenario: an `https://` backend is verified against a system store holding
/// no certificate authority. Wrong outcome: m6-http starts and answers 502 for
/// the route forever instead of refusing at start and naming the backend.
#[test]
fn a_verifying_url_backend_with_an_empty_system_store_exits_2() {
    let claim = testkit::claim_port();
    let dir = tempfile::tempdir().expect("tempdir");
    let (site, system) = url_backend_site(
        dir.path(),
        &claim.addr(),
        "[[backend]]\nname = \"remote\"\nurl = \"https://192.0.2.10\"",
    );
    let (empty_file, empty_dir) = empty_store(dir.path());

    let mut cmd = Command::new(testkit::binary("m6-http"));
    cmd.arg(&site)
        .arg(&system)
        .env("SSL_CERT_FILE", &empty_file)
        .env("SSL_CERT_DIR", &empty_dir);
    let mut svc = Service::spawn("m6-http-empty-roots", &mut cmd);
    let status = svc
        .wait_for_exit(Instant::now() + Duration::from_secs(10))
        .unwrap_or_else(|| {
            panic!(
                "m6-http did not exit for an empty system store\n--- output ---\n{}",
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
        output.contains("[[backend]] remote"),
        "the error must name the backend, got: {output}"
    );
    assert!(
        output.contains("system store"),
        "the error must name the store, got: {output}"
    );
}

/// Scenario: an `https://` backend sets `tls_skip_verify = true`, with no
/// certificate authority in the system store. Wrong outcome: the store check
/// refuses a backend that never reads the store.
#[test]
fn a_url_backend_that_skips_verification_starts_with_an_empty_system_store() {
    let claim = testkit::claim_port();
    let dir = tempfile::tempdir().expect("tempdir");
    let (site, system) = url_backend_site(
        dir.path(),
        &claim.addr(),
        "[[backend]]\nname = \"remote\"\nurl = \"https://192.0.2.10\"\ntls_skip_verify = true",
    );
    let (empty_file, empty_dir) = empty_store(dir.path());

    let mut cmd = Command::new(testkit::binary("m6-http"));
    cmd.arg(&site)
        .arg(&system)
        .env("SSL_CERT_FILE", &empty_file)
        .env("SSL_CERT_DIR", &empty_dir);
    let mut svc = Service::spawn("m6-http-empty-roots-skip", &mut cmd);
    svc.wait_for_tcp(claim.port(), Duration::from_secs(10));
    let status = svc.terminate(Duration::from_secs(5));
    assert!(
        status.success(),
        "m6-http should exit 0 on SIGTERM, got {status}\n--- output ---\n{}",
        svc.output()
    );
}

/// Write a site and system config whose `[analytics] log_path` is `log_path`.
fn analytics_site(dir: &Path, addr: &str, log_path: &Path) -> (PathBuf, PathBuf) {
    let site = dir.join("site");
    std::fs::create_dir_all(&site).expect("site dir");
    write_cert_key(&site);
    std::fs::write(
        site.join("site.toml"),
        format!(
            "[site]\nname = \"startup-errors\"\ndomain = \"example.com\"\n\n\
             [log]\nlevel = \"info\"\n\n\
             [analytics]\nenabled = true\nlog_path = \"{}\"\n",
            log_path.display()
        ),
    )
    .expect("site.toml");
    let system = dir.join("system.toml");
    std::fs::write(
        &system,
        format!(
            "[node]\nname = \"startup-errors\"\n\n\
             [server]\nbind = \"{addr}\"\ntls_cert = \"{}\"\ntls_key = \"{}\"\n",
            site.join("cert.pem").display(),
            site.join("key.pem").display(),
        ),
    )
    .expect("system config");
    (site, system)
}

/// Scenario: `[analytics] log_path` names a file in a directory that does not
/// exist. Wrong outcome: m6-http creates the directory and starts, so a typo
/// puts analytics where the operator did not choose and the service looks
/// healthy.
#[test]
fn a_missing_analytics_log_directory_exits_2_and_names_the_setting_and_path() {
    let claim = testkit::claim_port();
    let dir = tempfile::tempdir().expect("tempdir");
    let missing = dir.path().join("missing").join("logs");
    let log_path = missing.join("analytics.ndjson");
    let (site, system) = analytics_site(dir.path(), &claim.addr(), &log_path);

    let mut cmd = Command::new(testkit::binary("m6-http"));
    cmd.arg(&site).arg(&system);
    let mut svc = Service::spawn("m6-http-missing-log-dir", &mut cmd);
    let status = svc
        .wait_for_exit(Instant::now() + Duration::from_secs(10))
        .unwrap_or_else(|| {
            panic!(
                "m6-http did not exit for a missing analytics log directory\n--- output ---\n{}",
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
        output.contains("[analytics] log_path"),
        "the error did not name [analytics] log_path\n--- output ---\n{output}"
    );
    assert!(
        output.contains(&missing.display().to_string()),
        "the error did not name the missing directory {}\n--- output ---\n{output}",
        missing.display()
    );
    assert!(
        output.contains("create it, owned by") && output.contains("fix [analytics] log_path"),
        "the error did not say what to do\n--- output ---\n{output}"
    );
    assert!(
        !missing.exists(),
        "m6-http created the missing analytics directory {}\n--- output ---\n{output}",
        missing.display()
    );
}

/// Scenario: `[analytics] log_path` names a file in a directory that exists.
/// Wrong outcome: a correct analytics path is refused, so a service that should
/// start cannot.
#[test]
fn a_present_analytics_log_directory_starts_and_writes_the_file() {
    let claim = testkit::claim_port();
    let dir = tempfile::tempdir().expect("tempdir");
    let log_dir = dir.path().join("logs");
    std::fs::create_dir_all(&log_dir).expect("log dir");
    let log_path = log_dir.join("analytics.ndjson");
    let (site, system) = analytics_site(dir.path(), &claim.addr(), &log_path);

    let mut cmd = Command::new(testkit::binary("m6-http"));
    cmd.arg(&site).arg(&system);
    let mut svc = Service::spawn("m6-http-present-log-dir", &mut cmd);
    svc.wait_for_tcp(claim.port(), Duration::from_secs(10));
    assert!(
        log_path.is_file(),
        "the analytics file {} was not created\n--- output ---\n{}",
        log_path.display(),
        svc.output()
    );

    let status = svc.terminate(Duration::from_secs(5));
    assert!(
        status.success(),
        "m6-http should exit 0 on SIGTERM, got {status}\n--- output ---\n{}",
        svc.output()
    );
}

/// Scenario: a `[[route]] methods` list names a method m6 does not know.
/// Wrong outcome: the route answers 405 (or 200) at request time, so the typo
/// is discovered by a visitor instead of by the start.
#[test]
fn an_unknown_route_method_name_exits_2_and_names_it() {
    let claim = testkit::claim_port();
    let dir = tempfile::tempdir().expect("tempdir");
    let site = dir.path().join("site");
    std::fs::create_dir_all(&site).expect("site dir");
    write_cert_key(&site);
    // Bind the backend socket the route names, so the unknown method is the
    // only config error in the file. Without it, the socket-glob check would
    // stop the start first and this test would not exercise the method list.
    let _backend =
        std::os::unix::net::UnixListener::bind(dir.path().join("b-1.sock")).expect("backend");
    std::fs::write(
        site.join("site.toml"),
        format!(
            "[site]\nname = \"startup-errors\"\ndomain = \"example.com\"\n\n\
             [log]\nlevel = \"info\"\n\n\
             [analytics]\nenabled = false\n\n\
             [[backend]]\nname = \"b\"\nsockets = \"{}\"\n\n\
             [[route]]\npath = \"/submit\"\nbackend = \"b\"\nmethods = [\"GET\", \"BREW\"]\n",
            dir.path().join("b-*.sock").display()
        ),
    )
    .expect("site.toml");
    let system = dir.path().join("system.toml");
    std::fs::write(
        &system,
        format!(
            "[node]\nname = \"startup-errors\"\n\n\
             [server]\nbind = \"{}\"\ntls_cert = \"{}\"\ntls_key = \"{}\"\n",
            claim.addr(),
            site.join("cert.pem").display(),
            site.join("key.pem").display(),
        ),
    )
    .expect("system config");

    let out = run(&[site.as_os_str(), system.as_os_str()]);
    let err = stderr_of(&out);
    assert_eq!(exit_code(&out), 2, "stderr: {err}");
    assert!(
        err.contains("BREW"),
        "the error must name the unknown method, got: {err}"
    );
    assert!(
        err.contains("/submit") && err.contains("[[route]] number 1"),
        "the error must name the route, got: {err}"
    );
}

/// Scenario: `[server] lock_memory = true` and the account's memlock limit is
/// too low. Wrong outcome: m6-http starts unpinned without a word, or stops
/// without naming the limit and the fix. The limit is lowered with `ulimit -l 0`
/// in the shell that execs m6-http. Linux only: macOS has nothing to pin.
///
/// The shell prints the limit it set before the exec, and the test checks it is
/// 0. `ulimit -S -H -l 0` was the command here, and on Ubuntu `sh` is dash,
/// which rejects it with `ulimit: error setting limit (Invalid argument)` and
/// leaves the limit unchanged, so this test's own assertion that the limit was
/// lowered never ran. A refusal test that never refused is the defect, so the
/// shell's answer is checked before m6-http's.
#[test]
fn lock_memory_with_too_low_a_limit_exits_2_naming_the_limit_and_the_fix() {
    if std::env::consts::OS != "linux" {
        return;
    }
    let claim = testkit::claim_port();
    let dir = tempfile::tempdir().expect("tempdir");
    let site = dir.path().join("site");
    std::fs::create_dir_all(&site).expect("site dir");
    write_cert_key(&site);
    std::fs::write(site.join("site.toml"), site_toml()).expect("site.toml");
    let system = dir.path().join("system.toml");
    std::fs::write(
        &system,
        format!(
            "[node]\nname = \"startup-errors\"\n\n\
             [server]\nbind = \"{}\"\ntls_cert = \"{}\"\ntls_key = \"{}\"\nlock_memory = true\n",
            claim.addr(),
            site.join("cert.pem").display(),
            site.join("key.pem").display(),
        ),
    )
    .expect("system config");

    // `ulimit -l 0` sets both limits and dash accepts it. `ulimit -l` prints the
    // result, so the shell that has lowered the limit is the shell the assertion
    // below relies on. A shell that rejects the command exits before the exec,
    // and a shell that accepted it without lowering the limit prints something
    // else, and both fail here instead of passing a test that measured nothing.
    let out = Command::new("sh")
        .args(["-c", "ulimit -l 0 && ulimit -l && exec \"$0\" \"$@\""])
        .arg(testkit::binary("m6-http"))
        .arg(&site)
        .arg(&system)
        .output()
        .expect("run m6-http under sh");
    let err = stderr_of(&out);
    assert_eq!(
        stdout_of(&out).trim(),
        "0",
        "the shell did not lower RLIMIT_MEMLOCK to 0, so the refusal below was never put to the test. stderr: {err}"
    );
    assert_eq!(exit_code(&out), 2, "stderr: {err}");
    assert!(err.contains("RLIMIT_MEMLOCK"), "{err}");
    assert!(err.contains("LimitMEMLOCK=infinity"), "{err}");
}
