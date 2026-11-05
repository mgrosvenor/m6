//! Rendering scenarios for the route and params rules in issues #289 to #292.
//!
//! These tests live in their own file with their own fixtures under
//! `tests/fixtures/route_params/`, so they do not disturb the older
//! `integration.rs` suite. Each test below starts from one failure scenario:
//! the route tie-break, params precedence and restart pickup, query keys in
//! templates with the path-parameter clash, and a route's `status` reaching the
//! response.

use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use m6_core::testkit::{binary, Service};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// A running `m6-html`, its socket, and the directory the socket lives in.
///
/// Field order is drop order: the service is killed before the socket directory
/// is removed.
struct Server {
    // Held only to keep the child alive; its Drop sends SIGTERM then SIGKILL.
    _svc: Service,
    _dir: tempfile::TempDir,
}

/// The scenario fixture root, separate from the older integration fixtures.
fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("route_params")
}

fn config_path(name: &str) -> PathBuf {
    fixtures_dir().join("configs").join(name)
}

/// Spawn `m6-html` on a socket of its own and wait until it answers a request.
///
/// Readiness is a real answered request, not the socket file existing, for the
/// same reason the older suite does it that way: `bind` creates the file before
/// `listen`, so a path check can pass while a connect would still fail.
fn spawn_server(id: &str, site_dir: &Path, config: &Path) -> (Server, PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let socket_path = dir.path().join(format!("{id}.sock"));

    let mut svc = Service::spawn(
        "m6-html",
        Command::new(binary("m6-html"))
            .arg(site_dir)
            .arg(config)
            .env("M6_SOCKET_OVERRIDE", &socket_path),
    );
    svc.wait_for_path(&socket_path, Duration::from_secs(10));

    let mut ready = false;
    for _ in 0..200 {
        if !http_request(&socket_path, "GET / HTTP/1.1\r\nHost: localhost\r\n\r\n").is_empty() {
            ready = true;
            break;
        }
        svc.assert_alive("waiting for the first answered request");
        std::thread::sleep(Duration::from_millis(25));
    }
    assert!(
        ready,
        "m6-html at {} never answered a request\n--- output ---\n{}",
        socket_path.display(),
        svc.output()
    );

    (Server { _svc: svc, _dir: dir }, socket_path)
}

/// Send a raw HTTP/1.1 request and return the full response string.
///
/// Returns an empty string on any I/O error so callers can retry rather than
/// panic, matching the readiness contract above.
fn http_request(socket_path: &Path, request: &str) -> String {
    let mut stream = match UnixStream::connect(socket_path) {
        Ok(s) => s,
        Err(_) => return String::new(),
    };
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
    if stream.write_all(request.as_bytes()).is_err() {
        return String::new();
    }
    stream.shutdown(std::net::Shutdown::Write).ok();
    let response = m6_core::testkit::read_one(&mut stream, request.split(' ').next().unwrap_or(""))
        .unwrap_or_default();
    String::from_utf8_lossy(&response).into_owned()
}

/// Copy the scenario fixture root into a fresh temp directory, so a test can
/// rewrite a file (a changed global params file) without dirtying the committed
/// fixtures.
fn copy_site_to_temp() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().expect("tempdir");
    copy_dir_recursive(&fixtures_dir(), tmp.path()).expect("copy scenario fixtures");
    tmp
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let to = dst.join(entry.file_name());
        if ty.is_dir() {
            copy_dir_recursive(&entry.path(), &to)?;
        } else {
            std::fs::copy(entry.path(), &to)?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// #289 — the equal-specificity route tie
// ---------------------------------------------------------------------------

/// Two routes of equal specificity match one path (`/blog/{slug}` then
/// `/blog/{name}`). The wrong outcome this catches: the tie-break flips so the
/// later declaration answers, leaving the first route silently unreachable.
#[test]
fn equal_specificity_tie_first_declared_route_answers() {
    let (_guard, socket_path) = spawn_server("tie", &fixtures_dir(), &config_path("tie.conf"));

    let resp = http_request(
        &socket_path,
        "GET /blog/hello HTTP/1.1\r\nHost: localhost\r\n\r\n",
    );
    assert!(
        resp.contains("200 OK"),
        "expected 200, got: {}",
        &resp[..resp.len().min(300)]
    );
    assert!(
        resp.contains("FIRST-ROUTE"),
        "expected the first declared route to answer the tie, got: {}",
        &resp[..resp.len().min(300)]
    );
    assert!(
        !resp.contains("SECOND-ROUTE"),
        "the later route answered a tie it must lose"
    );
}

// ---------------------------------------------------------------------------
// #290 — params precedence and restart pickup
// ---------------------------------------------------------------------------

/// A route params key beats the same global key. `/brand-one` renders
/// `RouteFirst` from `route_params/first.json`, while `/brand-global` renders
/// `Global` from `data/site.json`. The wrong outcome: the global value
/// overwrites the route value.
#[test]
fn route_params_key_beats_global_key() {
    let (_guard, socket_path) = spawn_server(
        "params-beats-global",
        &fixtures_dir(),
        &config_path("params.conf"),
    );

    let route_resp = http_request(
        &socket_path,
        "GET /brand-one HTTP/1.1\r\nHost: localhost\r\n\r\n",
    );
    assert!(
        route_resp.contains("200 OK"),
        "expected 200, got: {}",
        &route_resp[..route_resp.len().min(300)]
    );
    assert!(
        route_resp.contains("RouteFirst"),
        "route params should beat the same global key, got: {}",
        &route_resp[..route_resp.len().min(300)]
    );

    let global_resp = http_request(
        &socket_path,
        "GET /brand-global HTTP/1.1\r\nHost: localhost\r\n\r\n",
    );
    assert!(
        global_resp.contains("Global"),
        "expected the global value on the params-less route, got: {}",
        &global_resp[..global_resp.len().min(300)]
    );
}

/// Of three route params files declaring one key, the last declared wins:
/// `/brand` renders `RouteThird`. The wrong outcome: the merge order reverses so
/// the first file wins.
#[test]
fn last_declared_route_params_file_wins() {
    let (_guard, socket_path) = spawn_server(
        "params-last-wins",
        &fixtures_dir(),
        &config_path("params.conf"),
    );

    let resp = http_request(
        &socket_path,
        "GET /brand HTTP/1.1\r\nHost: localhost\r\n\r\n",
    );
    assert!(
        resp.contains("200 OK"),
        "expected 200, got: {}",
        &resp[..resp.len().min(300)]
    );
    assert!(
        resp.contains("RouteThird"),
        "expected the last declared params file to win, got: {}",
        &resp[..resp.len().min(300)]
    );
}

/// A changed global params file is picked up after a restart. `/brand-global`
/// renders `Global`, then `GlobalUpdated` after the file changes and the service
/// restarts. The wrong outcome: the stale value survives a restart.
#[test]
fn global_params_change_picked_up_after_restart() {
    let site = copy_site_to_temp();
    let config = site.path().join("configs").join("params.conf");
    let global = site.path().join("data").join("site.json");

    let (guard, socket_path) = spawn_server("params-restart-1", site.path(), &config);
    let before = http_request(
        &socket_path,
        "GET /brand-global HTTP/1.1\r\nHost: localhost\r\n\r\n",
    );
    assert!(
        before.contains("Global"),
        "expected the initial global value, got: {}",
        &before[..before.len().min(300)]
    );
    drop(guard);

    std::fs::write(&global, "{\"brand\": \"GlobalUpdated\"}").expect("rewrite global params");

    let (_guard2, socket_path2) = spawn_server("params-restart-2", site.path(), &config);
    let after = http_request(
        &socket_path2,
        "GET /brand-global HTTP/1.1\r\nHost: localhost\r\n\r\n",
    );
    assert!(
        after.contains("GlobalUpdated"),
        "expected the changed global value after a restart, got: {}",
        &after[..after.len().min(300)]
    );
}

// ---------------------------------------------------------------------------
// #291 — query keys in templates with the path-parameter clash
// ---------------------------------------------------------------------------

/// Query keys reach the template, and a key that also names a path parameter
/// stays out of the top level: `/posts/hello?foo=bar&slug=x` renders
/// `hello|bar|bar|x`. The wrong outcome: a query key overwrites the path
/// parameter at the top level, or the `query` map is missing.
#[test]
fn query_keys_render_with_path_param_clash() {
    let (_guard, socket_path) =
        spawn_server("query-clash", &fixtures_dir(), &config_path("query.conf"));

    let resp = http_request(
        &socket_path,
        "GET /posts/hello?foo=bar&slug=x HTTP/1.1\r\nHost: localhost\r\n\r\n",
    );
    assert!(
        resp.contains("200 OK"),
        "expected 200, got: {}",
        &resp[..resp.len().min(300)]
    );
    assert!(
        resp.contains("hello|bar|bar|x"),
        "expected hello|bar|bar|x, got: {}",
        &resp[..resp.len().min(300)]
    );
}

// ---------------------------------------------------------------------------
// #292 — a route's status reaching the response
// ---------------------------------------------------------------------------

/// A route with `status = 410` answers 410 with its rendered body. The wrong
/// outcome: the status is dropped at the render call and the page is served 200.
#[test]
fn route_status_reaches_the_response() {
    let (_guard, socket_path) =
        spawn_server("status-410", &fixtures_dir(), &config_path("status.conf"));

    let resp = http_request(
        &socket_path,
        "GET /gone HTTP/1.1\r\nHost: localhost\r\n\r\n",
    );
    assert!(
        resp.starts_with("HTTP/1.1 410"),
        "expected 410, got: {}",
        &resp[..resp.len().min(300)]
    );
    assert!(
        resp.contains("GONE-BODY"),
        "expected the rendered body on the 410, got: {}",
        &resp[..resp.len().min(300)]
    );
}
