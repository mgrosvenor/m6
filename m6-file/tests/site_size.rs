//! What m6-file's document root carries, asserted by running the real binary.
//!
//! The check is a start question, so each test spawns `m6-file`, not a handler.
//! Every test states the wrong outcome it catches.
#![allow(
    clippy::disallowed_methods,
    reason = "test code: blocking clients drive the service from outside"
)]

use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use m6_core::testkit::{binary, read_response_event, Service, RESPONSE_BOUND};

/// One route that serves the whole root, so the root is the document root.
const ROUTE: &str = "[[route]]\npath = \"/{*relpath}\"\nhandler = \"files\"\nroot = \"\"\n";

/// A site root with one served file and an empty `site.toml`, which every
/// service needs to find.
fn tree() -> tempfile::TempDir {
    let dir = tempfile::Builder::new()
        .prefix("m6-")
        .tempdir_in(m6_core::testkit::socket_scratch_root())
        .unwrap();
    std::fs::write(dir.path().join("hello.txt"), b"hello").unwrap();
    std::fs::write(dir.path().join("site.toml"), "").unwrap();
    dir
}

fn config(root: &Path, extra: &str) -> PathBuf {
    let path = root.join("m6-file.conf");
    // `extra` goes first: TOML puts every key after a `[[route]]` header inside
    // that route, so a top-level setting written after the routes is not one.
    std::fs::write(&path, format!("{extra}\n{ROUTE}")).unwrap();
    path
}

fn spawn(root: &Path, config: &Path) -> Service {
    Service::spawn(
        "m6-file",
        Command::new(binary("m6-file"))
            .arg(root)
            .arg(config)
            .env("M6_SOCKET_OVERRIDE", root.join("m6-file.sock")),
    )
}

/// GET one path over the service's socket and return the wire response.
fn get(socket: &Path, path: &str) -> String {
    let mut stream = UnixStream::connect(socket).expect("connect");
    stream
        .write_all(
            format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        )
        .expect("write request");
    let response =
        read_response_event(stream, "GET", RESPONSE_BOUND).unwrap_or_else(|e| panic!("{e}"));
    String::from_utf8_lossy(&response).into_owned()
}

/// Scenario: the document root carries a `.git` directory.
/// Wrong outcome: the service starts and a wildcard route publishes history
/// and hooks. It must exit 2, name the entry, and leave no socket behind.
#[test]
fn a_document_root_with_git_exits_2_and_names_it() {
    let dir = tree();
    std::fs::create_dir_all(dir.path().join(".git")).unwrap();
    std::fs::write(dir.path().join(".git/HEAD"), b"ref: refs/heads/main\n").unwrap();
    let conf = config(dir.path(), "");

    let mut svc = spawn(dir.path(), &conf);
    let status = svc.wait_for_exit(Instant::now() + m6_core::testkit::STARTUP_BOUND);
    let text = svc.output();
    assert_eq!(
        status.and_then(|s| s.code()),
        Some(2),
        "the service did not refuse .git: {text}"
    );
    assert!(text.contains(".git"), "the refusal must name .git: {text}");
    assert!(
        text.contains("holds only the files it serves"),
        "the refusal must say what to do: {text}"
    );
    assert!(
        !dir.path().join("m6-file.sock").exists(),
        "a refused start must not bind a socket"
    );
}

/// Scenario: the document root carries a `logs/` directory, as example
/// 08-logviewer's does on purpose and m6-http's analytics path creates.
/// Wrong outcome: the service refuses and breaks a legitimate site, or says
/// nothing and a wildcard route publishes the logs. It must warn and serve.
#[test]
fn a_document_root_with_logs_starts_and_warns() {
    let dir = tree();
    std::fs::create_dir_all(dir.path().join("logs")).unwrap();
    std::fs::write(dir.path().join("logs/app.log"), b"one line\n").unwrap();
    let conf = config(dir.path(), "");

    let mut svc = spawn(dir.path(), &conf);
    svc.wait_for_path(
        &dir.path().join("m6-file.sock"),
        m6_core::testkit::STARTUP_BOUND,
    );
    svc.wait_for_started(m6_core::testkit::STARTUP_BOUND);
    let response = get(&dir.path().join("m6-file.sock"), "/hello.txt");
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    let status = svc.terminate(m6_core::testkit::STARTUP_BOUND);
    assert!(status.success(), "clean stop after a warning: {status}");
    let text = svc.output();
    assert!(
        text.contains("logs") && text.contains("wildcard route"),
        "the warning must name logs and what could serve it: {text}"
    );
}

/// Scenario: a site tree is over its configured budget.
/// Wrong outcome: the budget has no default, is never compared, or the warning
/// omits the total or the setting. It must warn with both and keep serving.
#[test]
fn a_root_over_its_budget_starts_and_warns_with_total_and_setting() {
    let dir = tree();
    // One byte over 1 MiB.
    std::fs::write(dir.path().join("large.bin"), vec![0u8; 1024 * 1024 + 1]).unwrap();
    let conf = config(dir.path(), "site_size_budget_mb = 1\n");

    let mut svc = spawn(dir.path(), &conf);
    svc.wait_for_path(
        &dir.path().join("m6-file.sock"),
        m6_core::testkit::STARTUP_BOUND,
    );
    svc.wait_for_started(m6_core::testkit::STARTUP_BOUND);
    let response = get(&dir.path().join("m6-file.sock"), "/hello.txt");
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    let status = svc.terminate(m6_core::testkit::STARTUP_BOUND);
    assert!(status.success(), "clean stop after a warning: {status}");
    let text = svc.output();
    assert!(
        text.contains("site_size_budget_mb = 1"),
        "the warning must name the setting: {text}"
    );
    assert!(
        text.contains("MiB") && text.contains("bytes"),
        "the warning must name the total: {text}"
    );
}

/// Scenario: the process root also holds a renderer's build output, as every
/// example does after its own dev script runs.
/// Wrong outcome: the check refuses the example because it scans the process
/// root instead of the trees the routes reach. It must start clean.
#[test]
fn a_build_artifact_outside_the_route_roots_does_not_refuse_the_start() {
    let dir = tree();
    std::fs::create_dir_all(dir.path().join("assets")).unwrap();
    std::fs::write(dir.path().join("assets/site.css"), b"body{}").unwrap();
    std::fs::create_dir_all(dir.path().join("render-cms/target/debug")).unwrap();
    std::fs::write(dir.path().join("render-cms/target/debug/app"), b"bin").unwrap();
    let conf = dir.path().join("m6-file.conf");
    std::fs::write(
        &conf,
        "[[route]]\npath = \"/assets/{*relpath}\"\nhandler = \"files\"\nroot = \"assets/\"\n",
    )
    .unwrap();

    let mut svc = spawn(dir.path(), &conf);
    svc.wait_for_path(
        &dir.path().join("m6-file.sock"),
        m6_core::testkit::STARTUP_BOUND,
    );
    svc.wait_for_started(m6_core::testkit::STARTUP_BOUND);
    let response = get(&dir.path().join("m6-file.sock"), "/assets/site.css");
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    let status = svc.terminate(m6_core::testkit::STARTUP_BOUND);
    assert!(status.success(), "clean stop: {status}");
    let text = svc.output();
    assert!(
        !text.contains("the document root"),
        "a target/ outside every route root must not be named: {text}"
    );
}

/// Scenario: a clean document root.
/// Wrong outcome: the check refuses or warns on a root that carries only what
/// it serves. It must start silently and answer.
#[test]
fn a_clean_document_root_starts_without_a_finding() {
    let dir = tree();
    let conf = config(dir.path(), "");

    let mut svc = spawn(dir.path(), &conf);
    svc.wait_for_path(
        &dir.path().join("m6-file.sock"),
        m6_core::testkit::STARTUP_BOUND,
    );
    svc.wait_for_started(m6_core::testkit::STARTUP_BOUND);
    let response = get(&dir.path().join("m6-file.sock"), "/hello.txt");
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    assert!(response.ends_with("hello"), "{response}");
    let status = svc.terminate(m6_core::testkit::STARTUP_BOUND);
    assert!(status.success(), "clean stop: {status}");
    let text = svc.output();
    assert!(
        !text.contains("the document root"),
        "a clean root must not be named: {text}"
    );
    assert!(
        !text.contains("site_size_budget_mb ="),
        "a clean root must not warn about a budget: {text}"
    );
}
