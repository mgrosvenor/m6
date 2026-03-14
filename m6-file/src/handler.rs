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

    if route.tail {
        return handle_tail(req, route, &params, ctx, stream, start);
    }

    let fs_path = route.resolve_fs_path(&params, ctx.site_dir);

    // Fast symlink check: if the path is (or contains) a symlink that escapes
    // site_dir, return 404.  Regular files skip canonicalize entirely.
    if let Ok(meta) = std::fs::symlink_metadata(&fs_path) {
        if meta.file_type().is_symlink() {
            // Only pay the canonicalize cost when a symlink is actually present.
            match std::fs::canonicalize(&fs_path) {
                Ok(real) if real.starts_with(ctx.site_dir) => {}
                _ => {
                    write_error(stream, 404, "Not Found")?;
                    return Ok(ResponseInfo { status: 404, bytes: 0, latency_us: start.elapsed().as_micros() });
                }
            }
        }
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

    let bytes = if req.method == "HEAD" {
        // HEAD must return the same headers as GET including the correct
        // Content-Length (the size of the body that would be sent for GET).
        let body_len = body.len();
        if let Some(enc) = content_encoding {
            write_head_response(stream, 200, "OK", &[
                ("Content-Type", mime.as_str()),
                ("Cache-Control", "public"),
                ("Content-Encoding", enc),
            ], body_len)?;
        } else {
            write_head_response(stream, 200, "OK", &[
                ("Content-Type", mime.as_str()),
                ("Cache-Control", "public"),
            ], body_len)?;
        }
        0
    } else {
        let len = body.len();
        if let Some(enc) = content_encoding {
            write_response(stream, 200, "OK", &[
                ("Content-Type", mime.as_str()),
                ("Cache-Control", "public"),
                ("Content-Encoding", enc),
            ], &body)?;
        } else {
            write_response(stream, 200, "OK", &[
                ("Content-Type", mime.as_str()),
                ("Cache-Control", "public"),
            ], &body)?;
        }
        len
    };

    Ok(ResponseInfo { status: 200, bytes, latency_us: start.elapsed().as_micros() })
}

/// Serve a file from a byte offset (tail mode).
///
/// Reads `?offset=N` from the query string (default 0), reads from that byte
/// offset to EOF, and returns the bytes with `Cache-Control: no-store` and an
/// `X-Log-End` header containing the new end offset. Safe for log tailing:
/// if the file has not grown since the last request, an empty body is returned.
fn handle_tail<W: Write>(
    req: &Request,
    route: &Route,
    params: &crate::route::Params,
    ctx: &HandlerContext,
    stream: &mut W,
    start: Instant,
) -> Result<ResponseInfo> {
    let fs_path = route.resolve_fs_path(params, ctx.site_dir);

    // Parse ?offset=N (default 0).
    let offset: u64 = req
        .query
        .split('&')
        .find(|p| p.starts_with("offset="))
        .and_then(|p| p["offset=".len()..].parse().ok())
        .unwrap_or(0);

    let mut file = match std::fs::File::open(&fs_path) {
        Ok(f) => f,
        Err(_) => {
            write_error(stream, 404, "Not Found")?;
            return Ok(ResponseInfo { status: 404, bytes: 0, latency_us: start.elapsed().as_micros() });
        }
    };

    // Seek to end to get file size, then seek to the requested offset.
    let file_size = file.seek(SeekFrom::End(0))?;
    let read_from = offset.min(file_size);
    file.seek(SeekFrom::Start(read_from))?;

    let mut body = Vec::new();
    file.read_to_end(&mut body)?;

    let end_offset = read_from + body.len() as u64;
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

    fn tail_route(url_path: &str, root: &str) -> Route {
        Route::from_config(&RouteConfig {
            path: url_path.to_string(),
            root: root.to_string(),
            tail: Some(true),
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
