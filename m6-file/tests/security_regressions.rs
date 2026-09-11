//! Security regression tests for m6-file (finding 9: tail routes skip the
//! symlink escape check).
//!
//! The reproduction asserts the **secure** behaviour. It failed when written
//! and passes now that the finding is fixed.

use std::io::Cursor;

use m6_file_lib::config::{Config, RouteConfig};
use m6_file_lib::handler::{handle_request, HandlerContext};
use m6_file_lib::http::Request;
use m6_file_lib::route::Route;

fn get_request(path: &str, query: &str) -> Request {
    let raw = if query.is_empty() {
        format!("GET {path} HTTP/1.1\r\nHost: localhost\r\n\r\n")
    } else {
        format!("GET {path}?{query} HTTP/1.1\r\nHost: localhost\r\n\r\n")
    };
    m6_core::parse::parse_request(&mut Cursor::new(raw.into_bytes())).unwrap()
}

fn route(url_path: &str, root: &str, tail: bool) -> Route {
    Route::from_config(&RouteConfig {
        path: url_path.to_string(),
        root: root.to_string(),
        tail: Some(tail),
        headers: vec![],
    })
}

fn body_of(raw: &[u8]) -> Vec<u8> {
    let end = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("header terminator");
    raw[end + 4..].to_vec()
}

/// Build a site dir containing `logs/escape.log` — a symlink pointing at a
/// secret file outside the site root. Returns (tempdir, site_dir).
fn site_with_escaping_symlink() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();

    // The secret lives outside the site directory entirely.
    let secret = dir.path().join("outside-secret.txt");
    std::fs::write(&secret, b"ESCAPED SECRET").unwrap();

    let site_dir = dir.path().join("site");
    std::fs::create_dir_all(site_dir.join("logs")).unwrap();
    std::os::unix::fs::symlink(&secret, site_dir.join("logs/escape.log")).unwrap();

    (dir, site_dir)
}

// ── Finding 9: tail routes bypass the symlink check ──────────────────────────

/// `handler.rs:55-57` dispatches to `handle_tail` *before* the symlink escape
/// check at `:63-74`, so a symlink under a tail route's root is followed out
/// of the site directory and its contents are served.
///
/// Property: a symlink escaping `site_dir` must be refused on every route,
/// tail or not.
#[test]
fn finding_9_tail_route_must_refuse_symlink_outside_site_dir() {
    let (_guard, site_dir) = site_with_escaping_symlink();

    let routes = vec![route("/logs/tail/{relpath}", "logs/", true)];
    let config = Config::default();
    let ctx = HandlerContext { routes: &routes, config: &config, site_dir: &site_dir };

    let req = get_request("/logs/tail/escape.log", "offset=0");
    let mut out = Vec::new();
    let info = handle_request(&req, &ctx, &mut out).unwrap();

    assert_ne!(
        body_of(&out),
        b"ESCAPED SECRET".to_vec(),
        "a tail route served a file from outside site_dir via symlink"
    );
    assert_eq!(
        info.status, 404,
        "the escaping symlink should be refused with 404"
    );
}

/// The identical symlink *is* correctly refused on a non-tail route, which
/// isolates the bug to the early return rather than to the check itself.
#[test]
fn finding_9_control_non_tail_route_refuses_same_symlink() {
    let (_guard, site_dir) = site_with_escaping_symlink();

    let routes = vec![route("/logs/{relpath}", "logs/", false)];
    let config = Config::default();
    let ctx = HandlerContext { routes: &routes, config: &config, site_dir: &site_dir };

    let req = get_request("/logs/escape.log", "");
    let mut out = Vec::new();
    let info = handle_request(&req, &ctx, &mut out).unwrap();

    assert_eq!(
        info.status, 404,
        "the non-tail path correctly rejects the escaping symlink"
    );
}
