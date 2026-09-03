use crate::compress::{choose_encoding, compress_brotli, compress_gzip, Encoding};
use crate::config::Config;
use crate::http::{write_error, write_head_response, write_response, Request};
use crate::route::{MatchResult, Route};
use anyhow::Result;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::time::Instant;
use tracing::debug;

pub struct HandlerContext<'a> {
    pub routes: &'a [Route],
    pub config: &'a Config,
    pub site_dir: &'a Path,
}

pub struct ResponseInfo {
    pub status: u16,
    pub bytes: usize,
    pub latency_us: u128,
}

/// True if `fs_path` is a symlink resolving outside `site_dir`.
///
/// Only pays the `canonicalize` cost when a symlink is actually present;
/// regular files return immediately. A path that cannot be resolved at all is
/// treated as escaping — it will 404 either way.
fn escapes_site_dir(fs_path: &Path, site_dir: &Path) -> bool {
    let Ok(meta) = std::fs::symlink_metadata(fs_path) else {
        return false; // missing file — the normal 404 path handles it
    };
    if !meta.file_type().is_symlink() {
        return false;
    }
    match (std::fs::canonicalize(fs_path), std::fs::canonicalize(site_dir)) {
        (Ok(real), Ok(root)) => !real.starts_with(root),
        _ => true,
    }
}

/// Handle a single HTTP request.
///
/// Route param validation in `route.rs` (no `..`, safe chars only) prevents
/// path traversal — no per-request `canonicalize` needed.
/// Compression is applied per-request according to Accept-Encoding + config;
/// m6-http caches the full response so subsequent requests never reach here.
pub fn handle_request<W: Write>(
    req: &Request,
    ctx: &HandlerContext,
    stream: &mut W,
) -> Result<ResponseInfo> {
    let start = Instant::now();

    if req.method != "GET" && req.method != "HEAD" {
        write_error(stream, 405, "Method Not Allowed")?;
        return Ok(ResponseInfo { status: 405, bytes: 0, latency_us: start.elapsed().as_micros() });
    }

    let (route, params) = match find_route(&req.path, ctx.routes) {
        FindRouteResult::Found(r, p) => (r, p),
        FindRouteResult::InvalidParam => {
            debug!(path = req.path, "invalid path parameter");
            write_error(stream, 400, "Bad Request")?;
            return Ok(ResponseInfo { status: 400, bytes: 0, latency_us: start.elapsed().as_micros() });
        }
        FindRouteResult::NotFound => {
            debug!(path = req.path, "no route matched");
            write_error(stream, 404, "Not Found")?;
            return Ok(ResponseInfo { status: 404, bytes: 0, latency_us: start.elapsed().as_micros() });
        }
    };

    let fs_path = route.resolve_fs_path(&params, ctx.site_dir);

    // Fast symlink check: if the path is (or contains) a symlink that escapes
    // site_dir, return 404.  Regular files skip canonicalize entirely.
    //
    // Runs before the `tail` dispatch below — tail routes read the same
    // filesystem through the same resolver, so exempting them just moved the
    // escape one route type over.
    if escapes_site_dir(&fs_path, ctx.site_dir) {
        write_error(stream, 404, "Not Found")?;
        return Ok(ResponseInfo { status: 404, bytes: 0, latency_us: start.elapsed().as_micros() });
    }

    if route.tail {
        return handle_tail(req, route, &params, ctx, stream, start);
    }

    // Every static asset used to go out as bare `Cache-Control: public` with
    // no ETag/Last-Modified at all — with no freshness info and no way to
    // revalidate, a browser that had already cached a file had no reason to
    // ever ask again, and a plain reload (not a hard refresh) couldn't
    // discover a newer deploy either. mtime+size is cheap to read and stable
    // across the minify/compress steps below (those transform the same
    // source bytes deterministically), so it's computed once, up front,
    // before doing any of that work — a conditional-GET hit skips reading,
    // minifying, and compressing the file entirely, not just the transfer.
    let metadata = match std::fs::metadata(&fs_path) {
        Ok(m) => m,
        Err(_) => {
            debug!(path = %fs_path.display(), "file not found");
            write_error(stream, 404, "Not Found")?;
            return Ok(ResponseInfo { status: 404, bytes: 0, latency_us: start.elapsed().as_micros() });
        }
    };
    let mtime = metadata.modified().unwrap_or(std::time::SystemTime::UNIX_EPOCH);
    let mtime_secs = mtime.duration_since(std::time::SystemTime::UNIX_EPOCH).unwrap_or_default().as_secs();
    let etag = format!("\"{:x}-{:x}\"", mtime_secs, metadata.len());
    let last_modified = httpdate::fmt_http_date(mtime);

    let if_none_match = req.headers.iter().find(|(k, _)| k == "if-none-match").map(|(_, v)| v.as_str());
    let not_modified = if let Some(inm) = if_none_match {
        // A real client sends exactly one ETag here, but the If-None-Match
        // grammar allows a comma-separated list (and "*"), so honor that.
        inm == "*" || inm.split(',').any(|tag| tag.trim() == etag)
    } else if let Some(ims) = req.headers.iter().find(|(k, _)| k == "if-modified-since").map(|(_, v)| v.as_str()) {
        // HTTP-date has 1-second resolution; compare at that resolution too
        // so a file that hasn't changed since the client's cached copy
        // doesn't spuriously look "modified" from sub-second mtime noise.
        httpdate::parse_http_date(ims)
            .map(|t| t.duration_since(std::time::SystemTime::UNIX_EPOCH).unwrap_or_default().as_secs() >= mtime_secs)
            .unwrap_or(false)
    } else {
        false
    };

    // Short max-age (fast repeat loads within it) plus must-revalidate (a
    // stale cache entry always checks back rather than being reused past
    // that window) — the conditional-GET machinery above is what makes
    // "checks back" cheap: a 304 on an unchanged file, not a full refetch.
    let cache_control = "public, max-age=60, must-revalidate";
    if not_modified {
        let hdrs: Vec<(&str, &str)> = vec![
            ("Cache-Control", cache_control),
            ("ETag", &etag),
            ("Last-Modified", &last_modified),
        ];
        write_response(stream, 304, "Not Modified", &hdrs, &[])?;
        return Ok(ResponseInfo { status: 304, bytes: 0, latency_us: start.elapsed().as_micros() });
    }

    let data = match std::fs::read(&fs_path) {
        Ok(d) => d,
        Err(_) => {
            debug!(path = %fs_path.display(), "file not found");
            write_error(stream, 404, "Not Found")?;
            return Ok(ResponseInfo { status: 404, bytes: 0, latency_us: start.elapsed().as_micros() });
        }
    };

    let mime = mime_guess::from_path(&fs_path).first_or_octet_stream().to_string();
    let mime_base = mime.split(';').next().unwrap_or(&mime);

    // Minification is applied BEFORE compression, gated by content-type and
    // config — mirroring m6-render's pipeline so a static asset gets the
    // same treatment here as it would through the render path.
    let data = if ctx.config.minification.is_enabled(mime_base) {
        match mime_base {
            "text/html" => m6_core::minify::minify_html(&data, ctx.config.minification.inline_js),
            "text/css" => m6_core::minify::minify_css(&data),
            "application/json" => m6_core::minify::minify_json(&data),
            "application/javascript" | "text/javascript" => m6_core::minify::minify_js(&data),
            _ => data,
        }
    } else {
        data
    };

    let accept_encoding = req.accept_encoding();
    let (encoding, level) = choose_encoding(&mime, accept_encoding, ctx.config);

    let (body, content_encoding): (Vec<u8>, Option<&str>) = match encoding {
        Encoding::Identity => (data, None),
        Encoding::Brotli => {
            let lvl = level.unwrap_or(6);
            let compressed = compress_brotli(&data, lvl).unwrap_or(data);
            (compressed, Some("br"))
        }
        Encoding::Gzip => {
            let lvl = level.unwrap_or(6);
            let compressed = compress_gzip(&data, lvl).unwrap_or(data);
            (compressed, Some("gzip"))
        }
    };

    let mut hdrs: Vec<(&str, &str)> = vec![
        ("Content-Type", mime.as_str()),
        ("Cache-Control", cache_control),
        ("ETag", &etag),
        ("Last-Modified", &last_modified),
    ];
    if let Some(enc) = content_encoding {
        hdrs.push(("Content-Encoding", enc));
    }
    for (k, v) in &route.headers {
        hdrs.push((k.as_str(), v.as_str()));
    }

    let bytes = if req.method == "HEAD" {
        write_head_response(stream, 200, "OK", &hdrs, body.len())?;
        0
    } else {
        let len = body.len();
        write_response(stream, 200, "OK", &hdrs, &body)?;
        len
    };

    Ok(ResponseInfo { status: 200, bytes, latency_us: start.elapsed().as_micros() })
}

/// Lookback window used to locate the last N lines when `?n=N&offset=0`.
/// 64 KiB covers several hundred typical JSON log lines; enlarge if very
/// long lines are common.
const TAIL_LOOKBACK: u64 = 64 * 1024;

/// Hard cap on bytes returned per incremental chunk (`offset > 0` path).
/// Prevents blocking the event loop for more than a few milliseconds.
const MAX_TAIL_BYTES: u64 = 512 * 1024;

/// Serve a file from a byte offset (tail mode).
///
/// Query parameters:
///   `offset=N` – start byte (default 0).
///   `n=N`      – when `offset=0`, return the **last N lines** of the file
///                (like `tail -n N`).  When `offset>0` or `n` is absent,
///                read up to `MAX_TAIL_BYTES` bytes from `offset`.
///
/// Always responds with `Cache-Control: no-store` and an `X-Log-End` header
/// containing the end byte of the returned slice so the caller can request
/// the next chunk.
fn handle_tail<W: Write>(
    req: &Request,
    route: &Route,
    params: &crate::route::Params,
    ctx: &HandlerContext,
    stream: &mut W,
    start: Instant,
) -> Result<ResponseInfo> {
    let fs_path = route.resolve_fs_path(params, ctx.site_dir);

    // Parse ?offset=N (default 0) and ?n=N (default 0 = no-line-limit).
    let offset: u64 = req
        .query
        .split('&')
        .find(|p| p.starts_with("offset="))
        .and_then(|p| p["offset=".len()..].parse().ok())
        .unwrap_or(0);
    let n: u64 = req
        .query
        .split('&')
        .find(|p| p.starts_with("n="))
        .and_then(|p| p["n=".len()..].parse().ok())
        .unwrap_or(0);

    let mut file = match std::fs::File::open(&fs_path) {
        Ok(f) => f,
        Err(_) => {
            write_error(stream, 404, "Not Found")?;
            return Ok(ResponseInfo { status: 404, bytes: 0, latency_us: start.elapsed().as_micros() });
        }
    };

    // Determine current file size.
    let file_size = file.seek(SeekFrom::End(0))?;

    let (body, end_offset) = if offset == 0 && n > 0 {
        // ── tail -n N mode ────────────────────────────────────────────────────
        // Scan the last TAIL_LOOKBACK bytes for the start of the last N lines.
        let lookback = TAIL_LOOKBACK.min(file_size);
        let scan_start = file_size - lookback;
        file.seek(SeekFrom::Start(scan_start))?;
        let mut buf = Vec::new();
        std::io::Read::by_ref(&mut file).take(lookback).read_to_end(&mut buf)?;

        // Walk backwards through buf counting newlines; `cut` becomes the
        // index of the first byte of the last-N-lines slice.
        //
        // Most log files end with '\n'.  That final newline is the terminator
        // of the last line — not the start of a new empty line — so we skip it
        // before counting to get the right N-line boundary.
        let mut found = 0u64;
        let mut cut = 0; // default: return everything when file has fewer than N lines
        let scan_end = if buf.last() == Some(&b'\n') { buf.len() - 1 } else { buf.len() };
        for i in (0..scan_end).rev() {
            if buf[i] == b'\n' {
                found += 1;
                if found >= n {
                    cut = i + 1;
                    break;
                }
            }
        }
        let body: Vec<u8> = buf[cut..].to_vec();
        // Always advance the caller to the current EOF so the next
        // incremental poll picks up only new content.
        (body, file_size)
    } else {
        // ── incremental / byte-offset mode ───────────────────────────────────
        let read_from = offset.min(file_size);
        file.seek(SeekFrom::Start(read_from))?;
        let mut body = Vec::new();
        std::io::Read::by_ref(&mut file).take(MAX_TAIL_BYTES).read_to_end(&mut body)?;
        let end_offset = read_from + body.len() as u64;
        (body, end_offset)
    };

    let end_str = end_offset.to_string();

    let mime = mime_guess::from_path(&fs_path)
        .first()
        .map(|m| m.to_string())
        .unwrap_or_else(|| "text/plain".to_string());

    write_response(
        stream,
        200,
        "OK",
        &[
            ("Content-Type", mime.as_str()),
            ("Cache-Control", "no-store"),
            ("X-Log-End", end_str.as_str()),
        ],
        &body,
    )?;

    Ok(ResponseInfo { status: 200, bytes: body.len(), latency_us: start.elapsed().as_micros() })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, RouteConfig};
    use crate::http::Request;
    use crate::route::Route;
    use std::io::Cursor;

    fn make_tail_request(path: &str, offset: u64) -> Request {
        let query = format!("offset={}", offset);
        let raw = format!("GET {}?{} HTTP/1.1\r\nHost: localhost\r\n\r\n", path, query);
        Request::read(Cursor::new(raw.into_bytes())).unwrap()
    }

    fn make_tail_n_request(path: &str, n: u64) -> Request {
        let query = format!("offset=0&n={}", n);
        let raw = format!("GET {}?{} HTTP/1.1\r\nHost: localhost\r\n\r\n", path, query);
        Request::read(Cursor::new(raw.into_bytes())).unwrap()
    }

    fn tail_route(url_path: &str, root: &str) -> Route {
        Route::from_config(&RouteConfig {
            path: url_path.to_string(),
            root: root.to_string(),
            tail: Some(true),
            headers: vec![],
        })
    }

    fn parse_response(buf: &[u8]) -> (u16, Vec<(String, String)>, Vec<u8>) {
        let s = std::str::from_utf8(buf).unwrap();
        let (head, body_str) = s.split_once("\r\n\r\n").unwrap();
        let mut lines = head.lines();
        let status_line = lines.next().unwrap();
        let status: u16 = status_line.split_whitespace().nth(1).unwrap().parse().unwrap();
        let headers: Vec<(String, String)> = lines
            .filter_map(|l| l.split_once(": ").map(|(k, v)| (k.to_lowercase(), v.to_string())))
            .collect();
        (status, headers, body_str.as_bytes().to_vec())
    }

    #[test]
    fn tail_from_zero_returns_full_content() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("app.log"), b"line1\nline2\n").unwrap();

        let req = make_tail_request("/logs/tail/app.log", 0);
        let route = tail_route("/logs/tail/{relpath}", "");
        let routes = vec![route];
        let config = Config::default();
        let ctx = HandlerContext { routes: &routes, config: &config, site_dir: dir.path() };

        let mut out = Vec::new();
        let info = handle_request(&req, &ctx, &mut out).unwrap();

        assert_eq!(info.status, 200);
        let (status, headers, body) = parse_response(&out);
        assert_eq!(status, 200);
        assert_eq!(body, b"line1\nline2\n");
        let end: u64 = headers.iter().find(|(k, _)| k == "x-log-end").unwrap().1.parse().unwrap();
        assert_eq!(end, 12);
        let cc = headers.iter().find(|(k, _)| k == "cache-control").unwrap();
        assert_eq!(cc.1, "no-store");
    }

    #[test]
    fn tail_from_mid_offset_returns_new_bytes_only() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("app.log"), b"line1\nline2\nline3\n").unwrap();

        let req = make_tail_request("/logs/tail/app.log", 12); // skip "line1\nline2\n"
        let route = tail_route("/logs/tail/{relpath}", "");
        let routes = vec![route];
        let config = Config::default();
        let ctx = HandlerContext { routes: &routes, config: &config, site_dir: dir.path() };

        let mut out = Vec::new();
        handle_request(&req, &ctx, &mut out).unwrap();
        let (_, headers, body) = parse_response(&out);

        assert_eq!(body, b"line3\n");
        let end: u64 = headers.iter().find(|(k, _)| k == "x-log-end").unwrap().1.parse().unwrap();
        assert_eq!(end, 18);
    }

    #[test]
    fn tail_beyond_eof_returns_empty_body() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("app.log"), b"abc").unwrap();

        let req = make_tail_request("/logs/tail/app.log", 999);
        let route = tail_route("/logs/tail/{relpath}", "");
        let routes = vec![route];
        let config = Config::default();
        let ctx = HandlerContext { routes: &routes, config: &config, site_dir: dir.path() };

        let mut out = Vec::new();
        handle_request(&req, &ctx, &mut out).unwrap();
        let (status, headers, body) = parse_response(&out);

        assert_eq!(status, 200);
        assert!(body.is_empty());
        let end: u64 = headers.iter().find(|(k, _)| k == "x-log-end").unwrap().1.parse().unwrap();
        assert_eq!(end, 3); // clamped to file size
    }

    #[test]
    fn tail_n_returns_last_n_lines() {
        let dir = tempfile::tempdir().unwrap();
        // 4 lines; requesting last 2 should skip "line1\n" and "line2\n"
        std::fs::write(dir.path().join("app.log"), b"line1\nline2\nline3\nline4\n").unwrap();

        let req = make_tail_n_request("/logs/tail/app.log", 2);
        let route = tail_route("/logs/tail/{relpath}", "");
        let routes = vec![route];
        let config = Config::default();
        let ctx = HandlerContext { routes: &routes, config: &config, site_dir: dir.path() };

        let mut out = Vec::new();
        handle_request(&req, &ctx, &mut out).unwrap();
        let (status, headers, body) = parse_response(&out);

        assert_eq!(status, 200);
        assert_eq!(body, b"line3\nline4\n");
        // X-Log-End must equal file size so next poll starts at EOF
        let end: u64 = headers.iter().find(|(k, _)| k == "x-log-end").unwrap().1.parse().unwrap();
        assert_eq!(end, 24); // full file size
    }

    #[test]
    fn tail_n_fewer_lines_than_n_returns_all() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("app.log"), b"only\none\n").unwrap();

        let req = make_tail_n_request("/logs/tail/app.log", 100);
        let route = tail_route("/logs/tail/{relpath}", "");
        let routes = vec![route];
        let config = Config::default();
        let ctx = HandlerContext { routes: &routes, config: &config, site_dir: dir.path() };

        let mut out = Vec::new();
        handle_request(&req, &ctx, &mut out).unwrap();
        let (_, headers, body) = parse_response(&out);

        assert_eq!(body, b"only\none\n");
        let end: u64 = headers.iter().find(|(k, _)| k == "x-log-end").unwrap().1.parse().unwrap();
        assert_eq!(end, 9);
    }

    #[test]
    fn tail_n_x_log_end_equals_file_size() {
        // The X-Log-End on a tail-n response must point to current EOF so that
        // the next incremental poll starts right after all existing content.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("app.log"), b"a\nb\nc\nd\n").unwrap();

        let req = make_tail_n_request("/logs/tail/app.log", 1);
        let route = tail_route("/logs/tail/{relpath}", "");
        let routes = vec![route];
        let config = Config::default();
        let ctx = HandlerContext { routes: &routes, config: &config, site_dir: dir.path() };

        let mut out = Vec::new();
        handle_request(&req, &ctx, &mut out).unwrap();
        let (_, headers, body) = parse_response(&out);

        assert_eq!(body, b"d\n");
        let end: u64 = headers.iter().find(|(k, _)| k == "x-log-end").unwrap().1.parse().unwrap();
        assert_eq!(end, 8); // file size, not just the last-line offset
    }

    #[test]
    fn static_html_is_minified_before_serving() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("index.html"),
            b"<html>\n  <body>\n    <!-- comment -->\n    <p>Hi</p>\n  </body>\n</html>\n",
        )
        .unwrap();

        let raw = "GET /index.html HTTP/1.1\r\nHost: localhost\r\n\r\n";
        let req = Request::read(Cursor::new(raw.as_bytes().to_vec())).unwrap();
        let route = Route::from_config(&RouteConfig {
            path: "/{relpath}".to_string(),
            root: "".to_string(),
            tail: None,
            headers: vec![],
        });
        let routes = vec![route];
        let config = Config::default();
        let ctx = HandlerContext { routes: &routes, config: &config, site_dir: dir.path() };

        let mut out = Vec::new();
        handle_request(&req, &ctx, &mut out).unwrap();
        let (status, _headers, body) = parse_response(&out);

        assert_eq!(status, 200);
        let body_str = std::str::from_utf8(&body).unwrap();
        assert!(!body_str.contains("<!-- comment -->"), "comment should be stripped: {}", body_str);
        assert!(body_str.contains("Hi"), "content missing: {}", body_str);
    }

    #[test]
    fn minification_disabled_for_mime_leaves_body_untouched() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("photo.svg"), b"<svg>   <!-- kept --> </svg>").unwrap();

        let raw = "GET /photo.svg HTTP/1.1\r\nHost: localhost\r\n\r\n";
        let req = Request::read(Cursor::new(raw.as_bytes().to_vec())).unwrap();
        let route = Route::from_config(&RouteConfig {
            path: "/{relpath}".to_string(),
            root: "".to_string(),
            tail: None,
            headers: vec![],
        });
        let routes = vec![route];
        let config = Config::default();
        let ctx = HandlerContext { routes: &routes, config: &config, site_dir: dir.path() };

        let mut out = Vec::new();
        handle_request(&req, &ctx, &mut out).unwrap();
        let (status, _headers, body) = parse_response(&out);

        assert_eq!(status, 200);
        assert_eq!(body, b"<svg>   <!-- kept --> </svg>");
    }

    #[test]
    fn tail_missing_file_returns_404() {
        let dir = tempfile::tempdir().unwrap();

        let req = make_tail_request("/logs/tail/missing.log", 0);
        let route = tail_route("/logs/tail/{relpath}", "");
        let routes = vec![route];
        let config = Config::default();
        let ctx = HandlerContext { routes: &routes, config: &config, site_dir: dir.path() };

        let mut out = Vec::new();
        let info = handle_request(&req, &ctx, &mut out).unwrap();
        assert_eq!(info.status, 404);
    }
}

enum FindRouteResult<'a> {
    Found(&'a Route, crate::route::Params),
    /// A route matched the prefix/structure but the param value was invalid.
    InvalidParam,
    NotFound,
}

fn find_route<'a>(url_path: &str, routes: &'a [Route]) -> FindRouteResult<'a> {
    let mut saw_invalid = false;
    for route in routes {
        match route.match_path(url_path) {
            MatchResult::Matched(params) => return FindRouteResult::Found(route, params),
            MatchResult::InvalidParam => saw_invalid = true,
            MatchResult::NoMatch => {}
        }
    }
    if saw_invalid {
        FindRouteResult::InvalidParam
    } else {
        FindRouteResult::NotFound
    }
}
