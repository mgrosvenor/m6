use crate::compress::{choose_encoding, compress_brotli, compress_gzip, Encoding};
use crate::config::Config;
use crate::http::{write_error, write_head_response, write_response, Request};
use crate::route::{MatchResult, Route};
use anyhow::Result;
use std::io::Write;
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
