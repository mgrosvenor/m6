//! Where an App finds `site.toml`.
//!
//! `docs/m6-site-layout.md` gives each app its own root under `apps/`, with
//! `site.toml` at the top of the tree. An App used to look only at
//! `<site_dir>/site.toml`, so in that layout it read none and the
//! `[[backend]] compresses` agreement passed without checking. The reserved
//! config key `site_toml` names the file.

use std::path::Path;
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use m6_core::testkit::{binary, Service};

/// The one route every test's config declares.
const ROUTE: &str =
    "[[route]]\npath = \"/assets/{*relpath}\"\nhandler = \"files\"\nroot = \"assets/\"\n";

/// A site tree: `site.toml` at the top, the app root at `apps/file`.
///
/// The app root also holds an empty `site.toml`, because every App needs a
/// readable `<site_dir>/site.toml` even when no key names the one at the top.
fn tree(site_toml_body: &str) -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("apps/file/assets")).unwrap();
    std::fs::write(tmp.path().join("site.toml"), site_toml_body).unwrap();
    std::fs::write(tmp.path().join("apps/file/site.toml"), "").unwrap();
    tmp
}

fn config(app_root: &Path, extra: &str) -> std::path::PathBuf {
    let path = app_root.join("m6-file.conf");
    std::fs::write(&path, format!("{extra}\n")).unwrap();
    path
}

fn run(app_root: &Path, config: &Path, flag: Option<&str>) -> Output {
    let mut cmd = Command::new(binary("m6-file"));
    cmd.arg(app_root)
        .arg(config)
        .env("M6_SOCKET_OVERRIDE", app_root.join("two-files.sock"));
    if let Some(f) = flag {
        cmd.arg(f);
    }
    cmd.output().unwrap()
}

#[test]
fn an_app_reads_site_toml_from_the_configured_path() {
    // m6-file's config file is m6-file.conf, so its backend name is "m6-file".
    // It compresses by default, so `compresses = false` is a disagreement.
    let tmp = tree("[[backend]]\nname = \"m6-file\"\ncompresses = false\n");
    let root = tmp.path().join("apps/file");
    let conf = config(&root, "site_toml = \"../../site.toml\"");

    let out = run(&root, &conf, None);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(2), "{stderr}");
    assert!(stderr.contains("DOES compress"), "{stderr}");
}

#[test]
fn a_configured_site_toml_that_does_not_exist_exits_2() {
    let tmp = tree("");
    let root = tmp.path().join("apps/file");
    let conf = config(&root, "site_toml = \"../../nowhere.toml\"");

    let out = run(&root, &conf, None);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(2), "{stderr}");
    assert!(
        stderr.contains("site_toml =") && stderr.contains("does not exist"),
        "{stderr}"
    );
}

#[test]
fn dump_config_names_the_site_toml() {
    let tmp = tree("");
    let root = tmp.path().join("apps/file");

    let conf = config(&root, "site_toml = \"../../site.toml\"");
    let out = run(&root, &conf, Some("--dump-config"));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(0), "{stdout}");
    let line = stdout
        .lines()
        .find(|l| l.starts_with("site.toml"))
        .unwrap_or_else(|| panic!("no site.toml line in:\n{stdout}"));
    assert!(line.ends_with("apps/file/../../site.toml"), "{line}");

    // With no key, the default <app_root>/site.toml is read.
    let conf = config(&root, "");
    let out = run(&root, &conf, Some("--dump-config"));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(0), "{stdout}");
    let line = stdout
        .lines()
        .find(|l| l.starts_with("site.toml"))
        .unwrap_or_else(|| panic!("no site.toml line in:\n{stdout}"));
    assert!(line.ends_with("apps/file/site.toml"), "{line}");
}

#[test]
fn dump_config_refuses_a_site_toml_the_start_would_refuse() {
    let tmp = tree("");
    let root = tmp.path().join("apps/file");

    // Missing.
    let conf = config(&root, "site_toml = \"../../nowhere.toml\"");
    let out = run(&root, &conf, Some("--dump-config"));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(2), "{stderr}");
    assert!(stderr.contains("does not exist"), "{stderr}");

    // Invalid TOML.
    std::fs::write(tmp.path().join("bad.toml"), "[server\n").unwrap();
    let conf = config(&root, "site_toml = \"../../bad.toml\"");
    let out = run(&root, &conf, Some("--dump-config"));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(2), "{stderr}");
    assert!(stderr.contains("not valid TOML"), "{stderr}");
}

/// A second `[[route]]` pattern that matches the same paths as
/// `/assets/{*relpath}`, so neither could be said to answer them.
const OVERLAPPING_ROUTE: &str =
    "[[route]]\npath = \"/assets/{*other}\"\nhandler = \"files\"\nroot = \"assets/\"\n";

/// Both routes as written, and the reason, so the operator can find the line.
/// The log is JSON, so quotes arrive escaped; they are unescaped first.
fn assert_names_the_overlap(output: &str) {
    let text = &output.replace("\\\"", "\"");
    for route in ["\"/assets/{*relpath}\"", "\"/assets/{*other}\""] {
        assert!(text.contains(route), "{route} not named in: {text}");
    }
    assert!(text.contains("can both answer GET /assets/x"), "{text}");
}

/// Scenario: two patterns that can match one path.
/// The wrong outcome: the service starts and the second route silently never
/// answers.
#[test]
fn a_start_with_overlapping_routes_exits_2_and_names_both() {
    let tmp = tree("");
    let root = tmp.path().join("apps/file");
    let conf = config(&root, &format!("{ROUTE}\n{OVERLAPPING_ROUTE}"));

    // Spawned, not `run`: a service that wrongly starts never exits, and the
    // test must fail on the deadline rather than hang.
    let mut svc = Service::spawn(
        "m6-file",
        Command::new(binary("m6-file"))
            .arg(&root)
            .arg(&conf)
            .env("M6_SOCKET_OVERRIDE", root.join("two-files.sock")),
    );
    let status = svc.wait_for_exit(Instant::now() + Duration::from_secs(30));
    let text = svc.output();
    assert_eq!(
        status.and_then(|s| s.code()),
        Some(2),
        "the service did not refuse and exit 2: {text}"
    );
    assert_names_the_overlap(&text);
}

/// Scenario: a typo in a route's `methods`. The wrong outcome (#336): the route
/// answers every method and the typo is never reported. The load must refuse it
/// and name the route and the value.
#[test]
fn dump_config_refuses_an_unknown_method_in_a_route() {
    let tmp = tree("");
    let root = tmp.path().join("apps/file");
    let conf = config(
        &root,
        "[[route]]\npath = \"/assets/{*relpath}\"\nhandler = \"files\"\nroot = \"assets/\"\nmethods = [\"GET\", \"GTE\"]\n",
    );

    let out = run(&root, &conf, Some("--dump-config"));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(2), "{stderr}");
    assert!(
        stderr.contains("\"/assets/{*relpath}\"") && stderr.contains("\"GTE\""),
        "{stderr}"
    );
}

/// Scenario: a deploy validates with `--dump-config`. The wrong outcome: it
/// passes a config the start then refuses.
#[test]
fn dump_config_refuses_overlapping_routes_as_the_start_does() {
    let tmp = tree("");
    let root = tmp.path().join("apps/file");
    let conf = config(&root, &format!("{ROUTE}\n{OVERLAPPING_ROUTE}"));

    let out = run(&root, &conf, Some("--dump-config"));
    let text = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(2), "{text}");
    assert_names_the_overlap(&text);
}
