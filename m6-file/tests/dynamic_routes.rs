//! A config reload adds a route, and the running service answers it.
//!
//! This is the test the whole `handler = "..."` change exists for, and it is
//! the one thing the core-side work could not prove on its own: those tests
//! build a state from config A, build another from config B, and assert the
//! second serves a route the first did not. Nothing wrote a config, let the
//! watcher fire, and got a 200 on a path that did not exist a moment earlier.
//!
//! The capability is the owner's instruction, verbatim: *"And dynamicly reload
//! the file list."* `App` bound routes at the call site, so migrating m6-file
//! onto it would have quietly removed the ability to add an asset tree without
//! restarting the service. A handler is code and is registered once; a route
//! is config and is rebuilt on every reload.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use m6_core::testkit::{assert_app_lifecycle, binary, wait, Service};

struct Server {
    svc: Service,
    _dir: tempfile::TempDir,
}

/// One request, returning the status line, or `None` on any I/O trouble.
///
/// Returns rather than unwraps: this is called from inside a readiness loop
/// written to tolerate a service that is not up yet, and a helper that panics
/// on a transport error ends the run instead of being retried.
fn status_of(socket: &Path, path: &str) -> Option<String> {
    let mut s = UnixStream::connect(socket).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    write!(s, "GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n").ok()?;
    let mut buf = String::new();
    s.read_to_string(&mut buf).ok()?;
    buf.lines().next().map(str::to_string)
}

fn write_config(path: &Path, routes: &str) {
    std::fs::write(path, format!("[thread_pool]\nsize = 4\n\n{routes}")).unwrap();
}

const ASSETS_ROUTE: &str = "[[route]]\npath = \"/assets/{*relpath}\"\nhandler = \"files\"\nroot = \"assets/\"\n";
const DOWNLOADS_ROUTE: &str = "[[route]]\npath = \"/downloads/{*relpath}\"\nhandler = \"files\"\nroot = \"files/\"\n";

/// Start m6-file with `routes`, serving `site`.
fn spawn(site: &Path, config: &Path, id: &str) -> (Server, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join(format!("{id}.sock"));
    let mut svc = Service::spawn(
        "m6-file",
        Command::new(binary("m6-file"))
            .arg(site)
            .arg(config)
            .env("M6_SOCKET_OVERRIDE", &socket),
    );
    svc.wait_for_path(&socket, Duration::from_secs(10));
    let ready = wait::until(Duration::from_secs(10), || {
        status_of(&socket, "/assets/a.txt").is_some()
    });
    assert!(ready, "m6-file never answered\n--- output ---\n{}", svc.output());
    (Server { svc, _dir: dir }, socket)
}

fn site_with_two_trees() -> tempfile::TempDir {
    let site = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(site.path().join("assets")).unwrap();
    std::fs::create_dir_all(site.path().join("files/deep")).unwrap();
    std::fs::write(site.path().join("assets/a.txt"), b"asset").unwrap();
    std::fs::write(site.path().join("files/deep/report.txt"), b"report").unwrap();
    site
}

/// Write a config with one route, prove the second tree is unreachable, add
/// the route, and prove the running process serves it. No restart.
#[test]
fn a_config_reload_adds_a_route_to_a_running_service() {
    let site = site_with_two_trees();
    let config = site.path().join("m6-file.conf");
    write_config(&config, ASSETS_ROUTE);

    let (mut server, socket) = spawn(site.path(), &config, "reload-add");

    assert!(
        status_of(&socket, "/assets/a.txt").unwrap().contains("200"),
        "the declared route should serve"
    );
    assert!(
        status_of(&socket, "/downloads/deep/report.txt").unwrap().contains("404"),
        "the undeclared route must not serve"
    );

    // Add it. The watcher sees the write; no signal, no restart.
    write_config(&config, &format!("{ASSETS_ROUTE}\n{DOWNLOADS_ROUTE}"));

    let serving = wait::until(Duration::from_secs(10), || {
        status_of(&socket, "/downloads/deep/report.txt")
            .is_some_and(|s| s.contains("200"))
    });
    assert!(
        serving,
        "a route added by config reload never started serving\n--- output ---\n{}",
        server.svc.output()
    );

    // And the original route is untouched by the reload.
    assert!(status_of(&socket, "/assets/a.txt").unwrap().contains("200"));
    server.svc.assert_alive("after a config reload added a route");
}

/// The other direction: a route removed from config stops being served, and
/// the process keeps running.
#[test]
fn a_config_reload_removes_a_route_from_a_running_service() {
    let site = site_with_two_trees();
    let config = site.path().join("m6-file.conf");
    write_config(&config, &format!("{ASSETS_ROUTE}\n{DOWNLOADS_ROUTE}"));

    let (mut server, socket) = spawn(site.path(), &config, "reload-remove");
    assert!(status_of(&socket, "/downloads/deep/report.txt").unwrap().contains("200"));

    write_config(&config, ASSETS_ROUTE);

    let gone = wait::until(Duration::from_secs(10), || {
        status_of(&socket, "/downloads/deep/report.txt").is_some_and(|s| s.contains("404"))
    });
    assert!(
        gone,
        "a route removed from config kept serving\n--- output ---\n{}",
        server.svc.output()
    );
    assert!(status_of(&socket, "/assets/a.txt").unwrap().contains("200"));
    server.svc.assert_alive("after a config reload removed a route");
}

/// A route naming a handler this binary does not have is refused, and the
/// **previous routes keep serving**. That is what makes a typo in a live
/// config recoverable rather than an outage.
#[test]
fn a_reload_naming_an_unregistered_handler_is_refused_and_the_old_routes_survive() {
    let site = site_with_two_trees();
    let config = site.path().join("m6-file.conf");
    write_config(&config, ASSETS_ROUTE);

    let (mut server, socket) = spawn(site.path(), &config, "reload-bad-handler");
    assert!(status_of(&socket, "/assets/a.txt").unwrap().contains("200"));

    // `file`, not `files`.
    write_config(
        &config,
        "[[route]]\npath = \"/assets/{*relpath}\"\nhandler = \"file\"\nroot = \"assets/\"\n",
    );

    // Give the watcher time to see it and refuse it.
    std::thread::sleep(Duration::from_secs(2));

    assert!(
        status_of(&socket, "/assets/a.txt").unwrap().contains("200"),
        "a refused reload must leave the previous routes serving\n--- output ---\n{}",
        server.svc.output()
    );
    server.svc.assert_alive("after a reload was refused");
}

/// The lifecycle contract, which m6-file now gets from the same assertion as
/// every other `App` service rather than from its own hand-written copy.
#[test]
fn lifecycle() {
    let site = site_with_two_trees();
    let config = site.path().join("m6-file.conf");
    write_config(&config, ASSETS_ROUTE);
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("lifecycle.sock");

    assert_app_lifecycle(
        "m6-file",
        Command::new(binary("m6-file"))
            .arg(site.path())
            .arg(&config)
            .env("M6_SOCKET_OVERRIDE", &socket),
        &socket,
    );
}
