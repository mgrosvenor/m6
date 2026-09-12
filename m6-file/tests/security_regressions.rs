//! Security regression tests for m6-file (finding 9: tail routes skip the
//! symlink escape check).
//!
//! The reproduction asserts the **secure** behaviour. It failed when written
//! and passes now that the finding is fixed.
//!
//! Rewritten when m6-file became an `App` service: it is now driven through
//! the handler's real entry point with the `Request` the service loop builds,
//! rather than through a `HandlerContext` that no longer exists. The property
//! under test is unchanged, and deliberately so: this is the test that caught
//! the tail path returning before the escape check, and the migration moved
//! that dispatch.

use m6_core::http::RawRequest;
use m6_core::Request;
use m6_file_lib::handler::serve;
use serde_json::{json, Map};

/// The `Request` the service loop hands the handler: route settings from
/// config, path parameters from core's router.
fn request(path: &str, query: &str, site_dir: &std::path::Path, tail: bool, relpath: &str) -> Request {
    let raw = RawRequest {
        version: "HTTP/1.1".to_string(),
        method: "GET".to_string(),
        path: path.to_string(),
        query: if query.is_empty() { None } else { Some(query.to_string()) },
        headers: vec![],
        body: vec![],
    };
    let mut dict = Map::new();
    dict.insert("relpath".to_string(), json!(relpath));
    let mut settings = Map::new();
    settings.insert("root".to_string(), json!("logs/"));
    settings.insert("tail".to_string(), json!(tail));
    Request::new(raw, dict, site_dir.to_path_buf())
        .with_route_settings(std::sync::Arc::new(settings))
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

/// Status and body as they would go on the wire.
fn wire(req: &Request) -> (u16, Vec<u8>) {
    let resp = serve(req).expect("handler");
    let mut out = Vec::new();
    {
        let mut r = m6_core::h1::Responder::new(&mut out, req.method(), false);
        resp.send(&mut r).expect("send");
    }
    let sep = out.windows(4).position(|w| w == b"\r\n\r\n").expect("header terminator");
    let head = std::str::from_utf8(&out[..sep]).expect("headers are ASCII");
    let status: u16 =
        head.lines().next().unwrap().split_whitespace().nth(1).unwrap().parse().unwrap();
    (status, out[sep + 4..].to_vec())
}

// ── Finding 9: tail routes bypass the symlink check ──────────────────────────

/// The tail dispatch used to happen *before* the symlink escape check, so a
/// symlink under a tail route's root was followed out of the site directory
/// and its contents served.
///
/// Property: a symlink escaping `site_dir` must be refused on every route,
/// tail or not.
#[test]
fn finding_9_tail_route_must_refuse_symlink_outside_site_dir() {
    let (_guard, site_dir) = site_with_escaping_symlink();
    let (status, body) = wire(&request(
        "/logs/tail/escape.log",
        "offset=0",
        &site_dir,
        true,
        "escape.log",
    ));

    assert_ne!(
        body,
        b"ESCAPED SECRET".to_vec(),
        "a tail route served a file from outside site_dir via symlink"
    );
    assert_eq!(status, 404, "the escaping symlink should be refused with 404");
}

/// The identical symlink *is* correctly refused on a non-tail route, which
/// isolates the bug to the early return rather than to the check itself.
#[test]
fn finding_9_control_non_tail_route_refuses_same_symlink() {
    let (_guard, site_dir) = site_with_escaping_symlink();
    let (status, _) = wire(&request("/logs/escape.log", "", &site_dir, false, "escape.log"));
    assert_eq!(status, 404, "the non-tail path correctly rejects the escaping symlink");
}
