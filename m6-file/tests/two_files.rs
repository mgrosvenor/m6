//! Where an App finds `site.toml`.
//!
//! `docs/m6-site-layout.md` gives each app its own root under `apps/`, with
//! `site.toml` at the top of the tree. An App used to look only at
//! `<site_dir>/site.toml`, so in that layout it read none and the
//! `[[backend]] compresses` agreement passed without checking. The reserved
//! config key `site_toml` names the file.

use std::path::Path;
use std::process::{Command, Output};

use m6_core::testkit::binary;

/// A site tree: `site.toml` at the top, the app root at `apps/file`.
fn tree(site_toml_body: &str) -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("apps/file/assets")).unwrap();
    std::fs::write(tmp.path().join("site.toml"), site_toml_body).unwrap();
    tmp
}

fn config(app_root: &Path, extra: &str) -> std::path::PathBuf {
    let path = app_root.join("m6-file.conf");
    std::fs::write(&path, format!("{extra}\n[thread_pool]\nsize = 2\n")).unwrap();
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

    // With no key and no file at the default place, the line says so.
    let conf = config(&root, "");
    let out = run(&root, &conf, Some("--dump-config"));
    let stdout = String::from_utf8_lossy(&out.stdout);
    let line = stdout
        .lines()
        .find(|l| l.starts_with("site.toml"))
        .unwrap_or_else(|| panic!("no site.toml line in:\n{stdout}"));
    assert!(line.ends_with("(absent)"), "{line}");
}
