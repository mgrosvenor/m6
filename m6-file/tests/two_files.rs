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

// ── Reload: a change to either file reloads the service ─────────────────────

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

use m6_core::testkit::{wait, Service};

const ROUTE: &str =
    "[[route]]\npath = \"/assets/{*relpath}\"\nhandler = \"files\"\nroot = \"assets/\"\n";

/// A running m6-file: a site tree with `site.toml` at the top, the app root at
/// `apps/file`, and a config that names the `site.toml` by `site_toml`.
struct Running {
    svc: Service,
    tmp: tempfile::TempDir,
    socket: std::path::PathBuf,
}

impl Running {
    fn root(&self) -> std::path::PathBuf {
        self.tmp.path().join("apps/file")
    }
    fn site_toml(&self) -> std::path::PathBuf {
        self.tmp.path().join("site.toml")
    }
    fn conf(&self) -> std::path::PathBuf {
        self.root().join("m6-file.conf")
    }
    fn count(&self, needle: &str) -> usize {
        self.svc.output().matches(needle).count()
    }
}

fn conf_body(extra: &str, pool: u32) -> String {
    format!("site_toml = \"../../site.toml\"\n\n[thread_pool]\nsize = {pool}\n\n{ROUTE}\n{extra}\n")
}

fn status_of(socket: &Path, path: &str) -> Option<String> {
    let mut s = UnixStream::connect(socket).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    write!(
        s,
        "GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
    )
    .ok()?;
    let mut buf = String::new();
    s.read_to_string(&mut buf).ok()?;
    buf.lines().next().map(str::to_string)
}

fn start(site_toml_body: &str) -> Running {
    let tmp = tree(site_toml_body);
    std::fs::write(tmp.path().join("apps/file/assets/a.txt"), b"asset").unwrap();
    let root = tmp.path().join("apps/file");
    std::fs::write(root.join("m6-file.conf"), conf_body("", 2)).unwrap();
    let socket = root.join("reload.sock");
    let mut svc = Service::spawn(
        "m6-file",
        Command::new(binary("m6-file"))
            .arg(&root)
            .arg(root.join("m6-file.conf"))
            .env("M6_SOCKET_OVERRIDE", &socket),
    );
    svc.wait_for_path(&socket, Duration::from_secs(10));
    assert!(
        wait::until(Duration::from_secs(10), || {
            status_of(&socket, "/assets/a.txt").is_some_and(|s| s.contains("200"))
        }),
        "m6-file never answered\n{}",
        svc.output()
    );
    Running { svc, tmp, socket }
}

fn wait_for(r: &Running, needle: &str, n: usize) -> bool {
    wait::until(Duration::from_secs(10), || r.count(needle) >= n)
}

#[test]
fn a_site_toml_change_reloads_the_service() {
    let r = start("[log]\nlevel = \"info\"\n");
    // At info, a request leaves no "request complete" line.
    assert!(status_of(&r.socket, "/assets/a.txt")
        .unwrap()
        .contains("200"));
    assert_eq!(r.count("request complete"), 0, "{}", r.svc.output());

    std::fs::write(r.site_toml(), "[log]\nlevel = \"debug\"\n").unwrap();
    assert!(
        wait_for(&r, "Reload complete", 1),
        "a site.toml change never reloaded\n{}",
        r.svc.output()
    );

    // The level moved with it (m6 #198): the same request now logs at debug.
    assert!(status_of(&r.socket, "/assets/a.txt")
        .unwrap()
        .contains("200"));
    assert!(
        wait_for(&r, "request complete", 1),
        "the log level in site.toml was not applied by the reload\n{}",
        r.svc.output()
    );
    let out = r.svc.output();
    assert!(
        out.find("Reload complete").unwrap() < out.find("request complete").unwrap(),
        "{out}"
    );
}

#[test]
fn a_compress_disagreement_in_a_new_site_toml_is_refused_and_the_old_routes_serve() {
    let r = start("");
    // m6-file compresses by default, so this says the opposite.
    std::fs::write(
        r.site_toml(),
        "[[backend]]\nname = \"m6-file\"\ncompresses = false\n",
    )
    .unwrap();
    assert!(
        wait_for(&r, "Reload refused", 1),
        "the disagreement was not refused\n{}",
        r.svc.output()
    );
    assert_eq!(r.count("Reload complete"), 0, "{}", r.svc.output());
    assert!(status_of(&r.socket, "/assets/a.txt")
        .unwrap()
        .contains("200"));
}

#[test]
fn a_site_toml_that_does_not_parse_keeps_the_previous_state() {
    let r = start("");
    std::fs::write(r.site_toml(), "[server\n").unwrap();
    assert!(
        wait_for(&r, "Reload refused", 1),
        "a site.toml that does not parse was not refused\n{}",
        r.svc.output()
    );
    assert_eq!(r.count("Reload complete"), 0, "{}", r.svc.output());
    assert!(status_of(&r.socket, "/assets/a.txt")
        .unwrap()
        .contains("200"));
}

#[test]
fn a_thread_pool_change_is_named_as_needing_a_restart() {
    let r = start("");
    std::fs::write(r.conf(), conf_body("", 3)).unwrap();
    assert!(
        wait_for(&r, "Reload complete", 1),
        "the config change never reloaded\n{}",
        r.svc.output()
    );
    assert!(
        r.svc.output().contains("thread_pool.size changed"),
        "{}",
        r.svc.output()
    );
}

#[test]
fn a_touch_alone_reloads() {
    let r = start("");
    let f = std::fs::OpenOptions::new()
        .write(true)
        .open(r.conf())
        .unwrap();
    f.set_modified(std::time::SystemTime::now()).unwrap();
    drop(f);
    assert!(
        wait_for(&r, "Reload complete", 1),
        "a touch of the config did not reload\n{}",
        r.svc.output()
    );
    std::thread::sleep(Duration::from_millis(1_500));
    assert_eq!(r.count("Reload complete"), 1, "{}", r.svc.output());
}

#[test]
fn a_burst_of_writes_is_one_reload() {
    let r = start("");
    for pad in ["", "# a", "# bb"] {
        std::fs::write(r.conf(), conf_body(pad, 2)).unwrap();
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(wait_for(&r, "Reload complete", 1), "{}", r.svc.output());
    std::thread::sleep(Duration::from_millis(1_500));
    assert_eq!(r.count("Reload complete"), 1, "{}", r.svc.output());
}
