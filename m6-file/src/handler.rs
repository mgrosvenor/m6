use crate::compress::{choose_encoding, compress_brotli, compress_gzip, Encoding};
use crate::config::Config;
use crate::http::Request;
use m6_core::h1::Responder;
use crate::route::{MatchResult, Route};
use anyhow::Result;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::time::Instant;
use tracing::{debug, warn};

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

/// Pick the `Cache-Control` for an asset from its query string.
///
/// One function rather than an inline expression because the rule has two
/// audiences that must not be conflated, and the tests need to assert the
/// emitted strings rather than a re-implementation of the condition. The
/// previous tests checked a `is_versioned` helper copied into the test module,
/// so the directives themselves were never covered: the browser-facing window
/// could have been lengthened without a single test failing.
///
/// A `?v=<hash>` URL addresses one exact version — changed bytes mean a changed
/// hash and therefore a different URL — so it can be pinned for a year and
/// marked `immutable`, which also stops a browser revalidating it on reload.
///
/// Everything else splits the two audiences deliberately:
/// - `max-age=60` / `stale-while-revalidate=60` are honoured by BROWSERS, and
///   no invalidation can reach a browser cache. They stay short so a deploy is
///   visible promptly. Raising either one strands visitors on the old file.
/// - `s-maxage=86400` is honoured ONLY by shared caches (RFC 9111 5.2.2.10).
///   A browser ignores it. It lengthens just the edge's copy, which
///   `invalidate-cache.sh` evicts on every deploy.
fn cache_control_for(query: &str) -> &'static str {
    let versioned = query.split('&').any(|p| p.starts_with("v=") && p.len() > 2);
    if versioned {
        "public, max-age=31536000, immutable"
    } else {
        "public, max-age=60, s-maxage=86400, stale-while-revalidate=60"
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
    resp: &mut Responder<'_, W>,
) -> Result<ResponseInfo> {
    let start = Instant::now();

    if req.method != "GET" && req.method != "HEAD" {
        resp.error(405)?;
        return Ok(ResponseInfo { status: 405, bytes: 0, latency_us: start.elapsed().as_micros() });
    }

    let (route, params) = match find_route(&req.path, ctx.routes) {
        FindRouteResult::Found(r, p) => (r, p),
        FindRouteResult::InvalidParam => {
            debug!(path = req.path, "invalid path parameter");
            resp.error(400)?;
            return Ok(ResponseInfo { status: 400, bytes: 0, latency_us: start.elapsed().as_micros() });
        }
        FindRouteResult::NotFound => {
            debug!(path = req.path, "no route matched");
            resp.error(404)?;
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
        resp.error(404)?;
        return Ok(ResponseInfo { status: 404, bytes: 0, latency_us: start.elapsed().as_micros() });
    }

    if route.tail {
        return handle_tail(req, route, &params, ctx, resp, start);
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
            resp.error(404)?;
            return Ok(ResponseInfo { status: 404, bytes: 0, latency_us: start.elapsed().as_micros() });
        }
    };
    let mtime = metadata.modified().unwrap_or(std::time::SystemTime::UNIX_EPOCH);
    let mtime_secs = mtime.duration_since(std::time::SystemTime::UNIX_EPOCH).unwrap_or_default().as_secs();
    let last_modified = httpdate::fmt_http_date(mtime);

    // Content negotiation is resolved HERE, before the ETag, because the ETag
    // has to name the representation actually served.
    //
    // mtime+size alone gave brotli, gzip and identity of the same file one
    // shared strong validator — three representations, three different byte
    // strings, one tag asserting they are the same. RFC 9110 requires a strong
    // validator to be unique per representation, and the practical consequence
    // is not theoretical: a downstream shared cache holding the brotli entry
    // can match that tag against a gzip-only client's request and hand it a
    // brotli body it cannot decode. Folding the coding into the tag makes the
    // three variants distinguishable.
    //
    // `mime_from_path` and `choose_encoding` both work off the path and
    // the request headers, never the file contents, so moving them above the
    // conditional check costs nothing and still lets a 304 skip the read,
    // minify and compress entirely.
    // m6-core's table, NOT the mime_guess crate.
    //
    // Two MIME implementations existed and the wrong one was serving. Every
    // text type went out with no charset -- `text/markdown`, `text/plain`,
    // `text/css`, `text/javascript` -- so a client fell back to Latin-1 and
    // rendered UTF-8 as mojibake. An em dash (U+2014, bytes e2 80 94) came
    // out as "a EUR --" in every .md file, in llms.txt and in llms-full.txt:
    // the three documents that exist specifically to be machine-read.
    //
    // The bytes were always correct; only the label was missing. m6-core's
    // table has carried `text/markdown; charset=utf-8` all along and simply
    // was not consulted here.
    let mime = m6_core::mime::mime_from_path(&fs_path).to_string();
    let mime_base = mime.split(';').next().unwrap_or(&mime).to_string();
    let accept_encoding = crate::http::accept_encoding(req);
    let (encoding, level) = choose_encoding(&mime, accept_encoding, ctx.config);
    let etag_suffix = match encoding {
        Encoding::Identity => "",
        Encoding::Brotli => "-br",
        Encoding::Gzip => "-gz",
    };
    let mut etag = format!("\"{:x}-{:x}{}\"", mtime_secs, metadata.len(), etag_suffix);

    // Preconditions come from m6-core, which implements all four steps of
    // RFC 9110 13.2.2 in the required order. What was here did steps 3 and 4
    // only, and step 3 with strong comparison:
    //
    //     inm == "*" || inm.split(',').any(|tag| tag.trim() == etag)
    //
    // Byte equality is *strong* comparison. `If-None-Match` requires weak
    // (8.8.3.2), so a client returning the validator it had been given as
    // `W/"..."` never matched and was sent the whole body again. `If-Match`
    // and `If-Unmodified-Since` were not consulted at all, so a client could
    // assert a precondition and have it silently ignored.
    let validators = [
        ("ETag".to_string(), etag.clone()),
        ("Last-Modified".to_string(), last_modified.clone()),
    ];
    let precondition =
        m6_core::evaluate_preconditions(&validators, &req.headers, &req.method);

    // Short max-age (fast repeat loads within it) plus stale-while-revalidate
    // (a shared cache past that window serves its stale copy immediately and
    // refreshes behind the request, so no visitor ever waits on an origin
    // round trip). The conditional-GET machinery above is what keeps that
    // refresh cheap: a 304 on an unchanged file, not a full refetch.
    //
    // This replaced `must-revalidate`, which says the opposite — never reuse
    // a stale entry without checking first — and so forbade exactly the
    // behaviour above. The two cannot both be advertised; a blocking
    // revalidation on every expiry is what the edge cache exists to avoid,
    // and one stale serve per minute per entry is the accepted price.
    //
    // The window is deliberately short. stale-while-revalidate is not a
    // shared-cache-only directive: browsers honour it too, so a long window
    // means a visitor keeps rendering the previous stylesheet for that long
    // after a deploy, and invalidate-cache.sh cannot reach into their cache
    // to help. 86400 was tried and made every CSS change invisible until a
    // visitor's second page load. 60s bounds that to ~2 minutes worst case
    // while still giving the edge what it actually needs -- a refresh takes
    // about a second, so the herd never blocks on origin.
    //
    // The cost is origin-down grace: the edge now serves stale for a minute
    // rather than a day. Raising it is safe once asset URLs are
    // content-hashed, since a changed file would then be a new URL.
    // A request carrying `?v=<content-hash>` (emitted by the `| asset` template
    // filter) addresses one exact version of the file: changed bytes produce a
    // different hash and therefore a different URL, so this response can never
    // go stale for that URL. `immutable` additionally tells the browser not to
    // revalidate even on reload, which is the whole point -- otherwise every
    // reload still costs a conditional request per asset.
    //
    // Everything else keeps the short window. An unversioned URL is exactly the
    // case where a long max-age strands visitors on the previous file with no
    // server-side way to reach them: no invalidation can touch a browser cache.
    // Notably the webfont is still requested unversioned from inside
    // style.css's @font-face, so it must stay on the short window.
    //
    // `s-maxage=86400` is the exception, and the distinction is the whole point.
    // The note above records that 86400 was tried and rolled back -- but what
    // was raised then was `stale-while-revalidate`, which browsers honour, so it
    // stranded visitors on the previous file for a day. `s-maxage` is defined
    // for SHARED caches only (RFC 9111 5.2.2.10): a browser ignores it outright
    // and keeps obeying the 60s `max-age` beside it. So this lengthens only the
    // copy held by the edge -- the one copy `invalidate-cache.sh` can actually
    // reach and evict on deploy.
    //
    // Without it the edge re-fetched every unversioned asset once a minute, and
    // on a low-traffic site almost every visit arrived after expiry: measured at
    // a 45% asset hit rate, with the webfont at 21 misses to 14 hits.
    //
    // This is only safe because a deploy evicts. deploy.sh invalidates by
    // default for exactly this reason -- see the guard there before shortening
    // that path.
    let cache_control = cache_control_for(req.query.as_deref().unwrap_or(""));

    // 412: a precondition the client asserted is false, and the request must
    // not be applied. The previous `bool` could not express this outcome,
    // which is why If-Match was ignored rather than honoured.
    if precondition == m6_core::Precondition::Failed {
        let hdrs: Vec<(&str, &str)> = vec![
            ("Cache-Control", cache_control),
            ("ETag", &etag),
            ("Last-Modified", &last_modified),
        ];
        resp.send(412, &hdrs, &[])?;
        return Ok(ResponseInfo { status: 412, bytes: 0, latency_us: start.elapsed().as_micros() });
    }

    if precondition == m6_core::Precondition::NotModified {
        let hdrs: Vec<(&str, &str)> = vec![
            ("Cache-Control", cache_control),
            ("ETag", &etag),
            ("Last-Modified", &last_modified),
        ];
        resp.send(304, &hdrs, &[])?;
        return Ok(ResponseInfo { status: 304, bytes: 0, latency_us: start.elapsed().as_micros() });
    }

    let data = match std::fs::read(&fs_path) {
        Ok(d) => d,
        Err(_) => {
            debug!(path = %fs_path.display(), "file not found");
            resp.error(404)?;
            return Ok(ResponseInfo { status: 404, bytes: 0, latency_us: start.elapsed().as_micros() });
        }
    };

    // Minification is applied BEFORE compression, gated by content-type and
    // config — mirroring m6-render's pipeline so a static asset gets the
    // same treatment here as it would through the render path.
    let data = if ctx.config.minification.is_enabled(&mime_base) {
        match mime_base.as_str() {
            "text/html" => m6_core::minify::minify_html(&data, ctx.config.minification.inline_js),
            "text/css" => m6_core::minify::minify_css(&data),
            "application/json" => m6_core::minify::minify_json(&data),
            "application/javascript" | "text/javascript" => m6_core::minify::minify_js(&data),
            _ => data,
        }
    } else {
        data
    };

    // On a compression failure this used to keep the `Content-Encoding: br`
    // (or gzip) label while handing back the *uncompressed* bytes that
    // `.unwrap_or(data)` fell through to — a body no client could decode,
    // announced as one it could. Falling back has to drop the label with it,
    // and the ETag's coding suffix has to come off too, or the identity bytes
    // would go out tagged as the brotli representation.
    let (body, content_encoding): (Vec<u8>, Option<&str>) = match encoding {
        Encoding::Identity => (data, None),
        Encoding::Brotli => match compress_brotli(&data, level.unwrap_or(6)) {
            Ok(compressed) => (compressed, Some("br")),
            Err(e) => {
                warn!(path = %fs_path.display(), error = %e, "brotli compression failed, serving identity");
                etag = format!("\"{:x}-{:x}\"", mtime_secs, metadata.len());
                (data, None)
            }
        },
        Encoding::Gzip => match compress_gzip(&data, level.unwrap_or(6)) {
            Ok(compressed) => (compressed, Some("gzip")),
            Err(e) => {
                warn!(path = %fs_path.display(), error = %e, "gzip compression failed, serving identity");
                etag = format!("\"{:x}-{:x}\"", mtime_secs, metadata.len());
                (data, None)
            }
        },
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

    // The HEAD rule lives in the responder now, and applies to every status
    // this file can answer with -- not just this one, which is all
    // `write_head_response` ever covered.
    let before = resp.body_bytes();
    resp.send(200, &hdrs, &body)?;
    let bytes = resp.body_bytes() - before;

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
    resp: &mut Responder<'_, W>,
    start: Instant,
) -> Result<ResponseInfo> {
    let fs_path = route.resolve_fs_path(params, ctx.site_dir);

    // Parse ?offset=N (default 0) and ?n=N (default 0 = no-line-limit).
    let query = req.query.as_deref().unwrap_or("");
    let offset: u64 = query
        .split('&')
        .find(|p| p.starts_with("offset="))
        .and_then(|p| p["offset=".len()..].parse().ok())
        .unwrap_or(0);
    let n: u64 = query
        .split('&')
        .find(|p| p.starts_with("n="))
        .and_then(|p| p["n=".len()..].parse().ok())
        .unwrap_or(0);

    let mut file = match std::fs::File::open(&fs_path) {
        Ok(f) => f,
        Err(_) => {
            resp.error(404)?;
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

    // Same table as the main path above; see the note there.
    let mime = m6_core::mime::mime_from_path(&fs_path).to_string();

    let before = resp.body_bytes();
    resp.send(
        200,
        &[
            ("Content-Type", mime.as_str()),
            ("Cache-Control", "no-store"),
            ("X-Log-End", end_str.as_str()),
        ],
        &body,
    )?;

    Ok(ResponseInfo {
        status: 200,
        bytes: resp.body_bytes() - before,
        latency_us: start.elapsed().as_micros(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The handler answers through a `Responder`, which is what applies the
    /// HEAD rule and the connection policy. Tests build one over a `Vec` so
    /// they exercise the same writer production does.
    fn responder<'a>(out: &'a mut Vec<u8>, req: &'a Request) -> Responder<'a, Vec<u8>> {
        Responder::new(out, &req.method, false)
    }
    use crate::config::{Config, RouteConfig};
    use crate::route::Route;
    use std::io::Cursor;

    fn make_tail_request(path: &str, offset: u64) -> Request {
        let query = format!("offset={}", offset);
        let raw = format!("GET {}?{} HTTP/1.1\r\nHost: localhost\r\n\r\n", path, query);
        m6_core::parse::parse_request(&mut Cursor::new(raw.into_bytes())).unwrap()
    }

    fn make_tail_n_request(path: &str, n: u64) -> Request {
        let query = format!("offset=0&n={}", n);
        let raw = format!("GET {}?{} HTTP/1.1\r\nHost: localhost\r\n\r\n", path, query);
        m6_core::parse::parse_request(&mut Cursor::new(raw.into_bytes())).unwrap()
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
        let info = handle_request(&req, &ctx, &mut responder(&mut out, &req)).unwrap();

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
        handle_request(&req, &ctx, &mut responder(&mut out, &req)).unwrap();
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
        handle_request(&req, &ctx, &mut responder(&mut out, &req)).unwrap();
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
        handle_request(&req, &ctx, &mut responder(&mut out, &req)).unwrap();
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
        handle_request(&req, &ctx, &mut responder(&mut out, &req)).unwrap();
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
        handle_request(&req, &ctx, &mut responder(&mut out, &req)).unwrap();
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
        let req = m6_core::parse::parse_request(&mut Cursor::new(raw.as_bytes().to_vec())).unwrap();
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
        handle_request(&req, &ctx, &mut responder(&mut out, &req)).unwrap();
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
        let req = m6_core::parse::parse_request(&mut Cursor::new(raw.as_bytes().to_vec())).unwrap();
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
        handle_request(&req, &ctx, &mut responder(&mut out, &req)).unwrap();
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
        let info = handle_request(&req, &ctx, &mut responder(&mut out, &req)).unwrap();
        assert_eq!(info.status, 404);
    }

    /// `parse_response` above runs the whole buffer through `from_utf8`, which
    /// is fine for the text bodies every other test sends but panics on a
    /// brotli or gzip one. Split on the header terminator as bytes instead and
    /// only decode the head.
    fn parse_response_bytes(buf: &[u8]) -> (u16, Vec<(String, String)>, Vec<u8>) {
        let sep = buf.windows(4).position(|w| w == b"\r\n\r\n").expect("header terminator");
        let head = std::str::from_utf8(&buf[..sep]).expect("headers are ASCII");
        let mut lines = head.lines();
        let status: u16 = lines.next().unwrap().split_whitespace().nth(1).unwrap().parse().unwrap();
        let headers = lines
            .filter_map(|l| l.split_once(": ").map(|(k, v)| (k.to_lowercase(), v.to_string())))
            .collect();
        (status, headers, buf[sep + 4..].to_vec())
    }

    // ── Representation-specific ETags ────────────────────────────────────────

    fn asset_route() -> Route {
        Route::from_config(&RouteConfig {
            path: "/assets/{relpath}".to_string(),
            root: "assets/".to_string(),
            tail: None,
            headers: vec![],
        })
    }

    /// Drive one GET for `/assets/<name>` with the given Accept-Encoding and
    /// return (etag, content-encoding, body length).
    fn fetch(dir: &std::path::Path, name: &str, accept_encoding: Option<&str>)
        -> (String, Option<String>, usize)
    {
        let ae = match accept_encoding {
            Some(v) => format!("Accept-Encoding: {}\r\n", v),
            None => String::new(),
        };
        let raw = format!("GET /assets/{} HTTP/1.1\r\nHost: localhost\r\n{}\r\n", name, ae);
        let req = m6_core::parse::parse_request(&mut Cursor::new(raw.into_bytes())).unwrap();
        let routes = vec![asset_route()];
        let config = Config::default();
        let ctx = HandlerContext { routes: &routes, config: &config, site_dir: dir };
        let mut out = Vec::new();
        handle_request(&req, &ctx, &mut responder(&mut out, &req)).unwrap();
        let (status, headers, body) = parse_response_bytes(&out);
        assert_eq!(status, 200, "expected 200 for {name}");
        let etag = headers.iter().find(|(k, _)| k == "etag").expect("etag header").1.clone();
        let ce = headers.iter().find(|(k, _)| k == "content-encoding").map(|(_, v)| v.clone());
        (etag, ce, body.len())
    }

    /// A file with enough redundancy that brotli and gzip both actually
    /// compress it, and compress it to different sizes.
    fn compressible_css() -> Vec<u8> {
        let mut s = String::new();
        for i in 0..400 {
            s.push_str(&format!(".selector-{} {{ color: #aabbcc; margin: 0 auto; }}\n", i));
        }
        s.into_bytes()
    }

    /// The defect: brotli, gzip and identity of the same file all carried one
    /// strong validator. RFC 9110 requires a strong ETag to identify the
    /// representation actually sent, and a downstream shared cache that
    /// believes otherwise can hand a brotli body to a gzip-only client.
    #[test]
    fn each_content_coding_gets_its_own_etag() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("assets")).unwrap();
        std::fs::write(dir.path().join("assets/style.css"), compressible_css()).unwrap();

        let (e_br, ce_br, _)   = fetch(dir.path(), "style.css", Some("br"));
        let (e_gz, ce_gz, _)   = fetch(dir.path(), "style.css", Some("gzip"));
        let (e_id, ce_id, _)   = fetch(dir.path(), "style.css", None);

        assert_eq!(ce_br.as_deref(), Some("br"));
        assert_eq!(ce_gz.as_deref(), Some("gzip"));
        assert_eq!(ce_id, None);

        assert_ne!(e_br, e_gz, "brotli and gzip share an ETag");
        assert_ne!(e_br, e_id, "brotli and identity share an ETag");
        assert_ne!(e_gz, e_id, "gzip and identity share an ETag");
    }

    /// The identity tag keeps its historical `<mtime>-<size>` shape, so an
    /// unversioned URL a client already cached does not spuriously miss.
    #[test]
    fn identity_keeps_the_unsuffixed_etag() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("assets")).unwrap();
        std::fs::write(dir.path().join("assets/style.css"), compressible_css()).unwrap();

        let (e_id, _, _) = fetch(dir.path(), "style.css", None);
        assert!(!e_id.contains("-br"), "identity tag carries a coding suffix: {e_id}");
        assert!(!e_id.contains("-gz"), "identity tag carries a coding suffix: {e_id}");
    }

    /// Two requests for the same representation must agree, or every reload
    /// is a full transfer.
    #[test]
    fn the_same_representation_is_stable_across_requests() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("assets")).unwrap();
        std::fs::write(dir.path().join("assets/style.css"), compressible_css()).unwrap();

        let (first, _, _)  = fetch(dir.path(), "style.css", Some("br"));
        let (second, _, _) = fetch(dir.path(), "style.css", Some("br"));
        assert_eq!(first, second);
    }

    /// A conditional request carrying the brotli tag must 304 for brotli, and
    /// must NOT 304 for a client that can only take gzip -- that pairing is
    /// exactly what the shared tag made indistinguishable.
    #[test]
    fn a_brotli_etag_does_not_validate_a_gzip_request() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("assets")).unwrap();
        std::fs::write(dir.path().join("assets/style.css"), compressible_css()).unwrap();

        let (e_br, _, _) = fetch(dir.path(), "style.css", Some("br"));

        let cond = |ae: &str| -> u16 {
            let raw = format!(
                "GET /assets/style.css HTTP/1.1\r\nHost: localhost\r\nAccept-Encoding: {}\r\nIf-None-Match: {}\r\n\r\n",
                ae, e_br);
            let req = m6_core::parse::parse_request(&mut Cursor::new(raw.into_bytes())).unwrap();
            let routes = vec![asset_route()];
            let config = Config::default();
            let ctx = HandlerContext { routes: &routes, config: &config, site_dir: dir.path() };
            let mut out = Vec::new();
            handle_request(&req, &ctx, &mut responder(&mut out, &req)).unwrap();
            parse_response_bytes(&out).0
        };

        assert_eq!(cond("br"), 304, "the brotli tag should validate a brotli request");
        assert_eq!(cond("gzip"), 200, "the brotli tag must not validate a gzip request");
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

#[cfg(test)]
mod cache_control_tests {
    use std::io::Cursor;

    use super::cache_control_for;

    /// Was a copy of the rule living in the test module, so these tests passed
    /// no matter what `handle_request` actually emitted. Now it calls the real
    /// function.
    fn is_versioned(query: &str) -> bool {
        cache_control_for(query).contains("immutable")
    }

    fn query_of(url: &str) -> String {
        let raw = format!("GET {url} HTTP/1.1\r\nHost: localhost\r\n\r\n");
        m6_core::parse::parse_request(&mut Cursor::new(raw.into_bytes()))
            .unwrap()
            .query
            .unwrap_or_default()
    }

    /// A `?v=<hash>` URL addresses one exact version, so it can be cached hard.
    #[test]
    fn versioned_urls_are_treated_as_immutable() {
        for url in [
            "/assets/css/style.css?v=56de07f9",
            "/assets/icons/logo.svg?v=abc123",
            "/assets/x.js?foo=1&v=deadbeef",
        ] {
            assert!(is_versioned(&query_of(url)), "{url} should be versioned");
        }
    }

    /// Everything else keeps the short window. The webfont matters most here:
    /// it is requested unversioned from @font-face inside style.css, and
    /// pinning it for a year would strand clients on an old file that no
    /// server-side invalidation can reach.
    #[test]
    fn unversioned_and_malformed_urls_keep_the_short_window() {
        for url in [
            "/assets/fonts/montserrat-normal.woff2",
            "/assets/css/style.css",
            "/assets/css/style.css?v=",          // empty hash is not a version
            "/assets/css/style.css?version=1",   // must not match on prefix
            "/assets/css/style.css?vv=1",
            "/assets/css/style.css?other=v=1",
        ] {
            assert!(!is_versioned(&query_of(url)), "{url} must NOT be treated as versioned");
        }
    }

    /// The exact strings, not just which branch was taken. These are the bytes
    /// a browser and an edge cache each act on.
    #[test]
    fn emitted_directives_are_exact() {
        assert_eq!(
            cache_control_for(&query_of("/assets/css/style.css?v=56de07f9")),
            "public, max-age=31536000, immutable"
        );
        assert_eq!(
            cache_control_for(&query_of("/assets/fonts/montserrat-normal.woff2")),
            "public, max-age=60, s-maxage=86400, stale-while-revalidate=60"
        );
    }

    /// The load-bearing property of the unversioned directive, stated as its
    /// own test so the reason survives.
    ///
    /// An edge cache can be emptied on deploy; a browser cache cannot be
    /// reached at all. So the long window may only ever appear on `s-maxage`,
    /// which browsers ignore. Raising `max-age` or `stale-while-revalidate` to
    /// buy the same hit rate would instead strand every visitor on the previous
    /// file for a day -- that was tried once and rolled back.
    #[test]
    fn unversioned_lengthens_only_the_edge_never_the_browser() {
        let cc = cache_control_for(&query_of("/assets/css/style.css"));

        assert!(cc.contains("s-maxage=86400"), "edge must hold it long: {cc}");
        assert!(cc.contains("max-age=60"), "browser must stay short: {cc}");
        assert!(
            cc.contains("stale-while-revalidate=60"),
            "browser-visible stale window must stay short: {cc}"
        );

        // The directive browsers obey must never carry the long value. Checked
        // by stripping `s-maxage=86400` and asserting 86400 appears nowhere
        // else -- a plain `contains` would be satisfied by s-maxage itself.
        let browser_visible = cc.replace("s-maxage=86400", "");
        assert!(
            !browser_visible.contains("86400"),
            "a browser-honoured directive carries the long window: {cc}"
        );
    }
}

#[cfg(test)]
mod charset_tests {
    use std::path::Path;

    /// Every text type must declare UTF-8. Without it a client falls back to
    /// Latin-1 and renders UTF-8 as mojibake: an em dash (U+2014, bytes
    /// e2 80 94) came out as three garbage characters in every .md file, in
    /// llms.txt and in llms-full.txt -- the documents that exist specifically
    /// to be machine-read.
    ///
    /// The bytes on the wire were always correct. Only the label was missing,
    /// because m6-file used the mime_guess crate instead of m6-core's table,
    /// which has carried the charset all along.
    #[test]
    fn text_types_declare_utf8() {
        for (file, want) in [
            ("a.md", "text/markdown; charset=utf-8"),
            ("llms.txt", "text/plain; charset=utf-8"),
            ("style.css", "text/css; charset=utf-8"),
            ("nav.js", "text/javascript; charset=utf-8"),
            ("page.html", "text/html; charset=utf-8"),
        ] {
            assert_eq!(m6_core::mime::mime_from_path(Path::new(file)), want, "{file}");
        }
    }

    /// Binary types must NOT carry a charset -- it is meaningless there, and
    /// on application/json the parameter is undefined by RFC 8259.
    #[test]
    fn binary_and_json_carry_no_charset() {
        for file in ["a.webp", "a.png", "a.woff2", "a.pdf", "a.json", "a.ico"] {
            let m = m6_core::mime::mime_from_path(Path::new(file));
            assert!(!m.contains("charset"), "{file} must not declare a charset, got {m}");
        }
    }

    /// The table must cover every extension this site actually serves, or the
    /// swap away from mime_guess would silently downgrade files to
    /// application/octet-stream.
    #[test]
    fn every_extension_in_use_is_known() {
        for file in [
            "a.webp", "a.jpg", "a.png", "a.js", "a.svg", "a.pdf",
            "a.md", "a.txt", "a.woff2", "a.css", "a.xml", "a.json", "a.ico",
        ] {
            let m = m6_core::mime::mime_from_path(Path::new(file));
            assert_ne!(m, "application/octet-stream", "{file} fell through to the default");
        }
    }
}
