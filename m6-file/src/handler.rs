use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use m6_core::{Request, Response, Result};
use tracing::{debug, warn};

use crate::compress::{choose_encoding, compress_brotli, compress_gzip, Encoding};

/// Resolve the file this request addresses, from the route's `root` setting
/// and the path parameters core captured.
///
/// `root` may itself carry placeholders (`content/posts/{stem}/`), which are
/// filled from the same parameters. The trailing component is `relpath` for a
/// `{*relpath}` wildcard route or `filename` for a two-parameter one; a route
/// with neither addresses the file `root` names outright, which is how
/// `/favicon.ico` works.
fn resolve_fs_path(req: &Request, site_dir: &Path) -> PathBuf {
    let mut root = req.route_str("root").unwrap_or("").to_string();
    if root.contains('{') {
        for (k, v) in req.dict().iter() {
            if let Some(s) = v.as_str() {
                root = root.replace(&format!("{{{}}}", k), s);
            }
        }
    }
    let rel = req
        .dict()
        .get("relpath")
        .or_else(|| req.dict().get("filename"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if rel.is_empty() {
        site_dir.join(&root)
    } else {
        site_dir.join(&root).join(rel)
    }
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

/// Serve one static file.
///
/// Registered with `App::handler("files", serve)`; the route that reaches it,
/// and the directory it serves from, are config. Core routes, validates the
/// path parameters and applies the route's extra headers; everything from the
/// filesystem down is here.
///
/// **This handler owns its own representation.** It negotiates the content
/// coding, compresses, and builds an ETag naming the result, so every response
/// it returns is `verbatim` or a stream, and core's pipeline leaves it alone.
/// Letting core compress afterwards would put brotli bytes on the wire under a
/// tag asserting identity, which is what the `-br`/`-gz` suffixes prevent.
pub fn serve(req: &Request) -> Result<Response> {
    if req.method() != "GET" && req.method() != "HEAD" {
        return Ok(Response::status(405));
    }

    let site_dir = req.site_path("");
    let fs_path = resolve_fs_path(req, &site_dir);

    // Fast symlink check: if the path is (or contains) a symlink that escapes
    // site_dir, return 404.  Regular files skip canonicalize entirely.
    //
    // Runs before the `tail` dispatch below — tail routes read the same
    // filesystem through the same resolver, so exempting them just moved the
    // escape one route type over.
    if escapes_site_dir(&fs_path, &site_dir) {
        return Ok(Response::not_found());
    }

    if req.route_bool("tail", false) {
        return serve_tail(req, &fs_path);
    }

    let empty_compression = std::collections::HashMap::new();
    let (compression, minification) = match req.config() {
        Some(c) => (&c.compression, Some(&c.minification)),
        None => (&empty_compression, None),
    };

    // Every static asset used to go out as bare `Cache-Control: public` with
    // no ETag/Last-Modified at all — with no freshness info and no way to
    // revalidate, a browser that had already cached a file had no reason to
    // ever ask again. mtime+size is cheap to read and stable across the
    // minify/compress steps below (those transform the same source bytes
    // deterministically), so it is computed once, up front: a conditional-GET
    // hit skips reading, minifying and compressing the file entirely.
    let Ok(metadata) = std::fs::metadata(&fs_path) else {
        debug!(path = %fs_path.display(), "file not found");
        return Ok(Response::not_found());
    };
    let mtime = metadata.modified().unwrap_or(std::time::SystemTime::UNIX_EPOCH);
    let mtime_secs =
        mtime.duration_since(std::time::SystemTime::UNIX_EPOCH).unwrap_or_default().as_secs();
    let last_modified = httpdate::fmt_http_date(mtime);

    // Content negotiation is resolved HERE, before the ETag, because the ETag
    // has to name the representation actually served.
    //
    // mtime+size alone gave brotli, gzip and identity of the same file one
    // shared strong validator — three representations, three different byte
    // strings, one tag asserting they are the same. RFC 9110 requires a strong
    // validator to be unique per representation, and a downstream shared cache
    // holding the brotli entry can otherwise match that tag against a
    // gzip-only client's request and hand it a body it cannot decode.
    //
    // The MIME table is m6-core's, NOT the mime_guess crate. Two
    // implementations existed and the wrong one was serving: every text type
    // went out with no charset, so a client fell back to Latin-1 and rendered
    // UTF-8 as mojibake in every .md file, in llms.txt and in llms-full.txt.
    let mime = m6_core::mime::mime_from_path(&fs_path).to_string();
    let mime_base = mime.split(';').next().unwrap_or(&mime).to_string();
    let accept_encoding = req.header("accept-encoding").unwrap_or("");
    let (encoding, level) = choose_encoding(&mime, accept_encoding, compression);
    let etag_suffix = match encoding {
        Encoding::Identity => "",
        Encoding::Brotli => "-br",
        Encoding::Gzip => "-gz",
    };
    let mut etag = format!("\"{:x}-{:x}{}\"", mtime_secs, metadata.len(), etag_suffix);

    // Preconditions come from m6-core, which implements all four steps of
    // RFC 9110 13.2.2 in the required order. What was here did steps 3 and 4
    // only, and step 3 with strong comparison, so a client returning the
    // validator it had been given as `W/"..."` never matched.
    let validators = [
        ("ETag".to_string(), etag.clone()),
        ("Last-Modified".to_string(), last_modified.clone()),
    ];
    let precondition =
        m6_core::evaluate_preconditions(&validators, req.headers(), req.method());

    let cache_control = cache_control_for(req.query());

    let validator_headers = |r: Response| {
        r.header("Cache-Control", cache_control)
            .header("ETag", &etag)
            .header("Last-Modified", &last_modified)
    };

    // 412: a precondition the client asserted is false, and the request must
    // not be applied.
    if precondition == m6_core::Precondition::Failed {
        return Ok(validator_headers(Response::status(412)).verbatim());
    }
    if precondition == m6_core::Precondition::NotModified {
        return Ok(validator_headers(Response::status(304)).verbatim());
    }

    // A HEAD whose representation is the file on disk needs no file on disk,
    // and neither does a GET: the bytes on the wire are the bytes on disk, so
    // there is nothing to hold in memory.
    //
    // `Content-Length` is why it cannot simply be skipped for a HEAD: a HEAD
    // has to report what the matching GET would send, so a compressed or
    // minified representation genuinely has to be produced to be measured. The
    // case that does not is identity coding with minification off for this
    // type, where the representation *is* the file and `metadata.len()` is
    // already its length. That is also the common case for the assets worth
    // caring about, since images are neither compressed nor minified here.
    //
    // `is_file` is load-bearing and was missing in the first version of this.
    // `std::fs::metadata` succeeds on a directory and reports its size, so
    // `HEAD /assets/css` answered `200` with `Content-Length: 128` while the
    // GET beside it answered 404. The read this block skips is also what used
    // to reject a non-file, by failing.
    let minify_this = minification.map(|m| m.is_enabled(&mime_base)).unwrap_or(false);
    let representation_is_the_file =
        metadata.is_file() && encoding == Encoding::Identity && !minify_this;

    if representation_is_the_file {
        let base = validator_headers(Response::status(200)).header("Content-Type", &mime);
        if req.method() == "HEAD" {
            // No body to produce, but the length still has to be the one the
            // GET would send. `Response::stream` promises it without reading:
            // the responder drops the body for a HEAD.
            return Ok(Response::stream(200, metadata.len(), std::io::empty())
                .header("Content-Type", &mime)
                .header("Cache-Control", cache_control)
                .header("ETag", &etag)
                .header("Last-Modified", &last_modified));
        }
        let Ok(file) = std::fs::File::open(&fs_path) else {
            debug!(path = %fs_path.display(), "file not found");
            return Ok(Response::not_found());
        };
        // `lute.min.js` is 3.6MB that used to be read into a `Vec` on every
        // cache miss to be copied straight out again. A file that changed
        // between the `stat` and the `open` is handled by the responder rather
        // than here: short reads fail and overruns are capped, because
        // `Content-Length` is already on the wire by then.
        let mut streamed = Response::stream(200, metadata.len(), file);
        streamed.headers = base.headers;
        return Ok(streamed);
    }

    let Ok(data) = std::fs::read(&fs_path) else {
        debug!(path = %fs_path.display(), "file not found");
        return Ok(Response::not_found());
    };

    // Minification is applied BEFORE compression, gated by content-type and
    // config, mirroring core's pipeline so a static asset gets the same
    // treatment here as it would through the render path.
    let data = if minify_this {
        match mime_base.as_str() {
            "text/html" => m6_core::minify::minify_html(
                &data,
                minification.map(|m| m.inline_js).unwrap_or(false),
            ),
            "text/css" => m6_core::minify::minify_css(&data),
            "application/json" => m6_core::minify::minify_json(&data),
            "application/javascript" | "text/javascript" => m6_core::minify::minify_js(&data),
            _ => data,
        }
    } else {
        data
    };

    // On a compression failure this used to keep the `Content-Encoding: br`
    // label while handing back the *uncompressed* bytes — a body no client
    // could decode, announced as one it could. Falling back has to drop the
    // label with it, and the ETag's coding suffix has to come off too, or the
    // identity bytes would go out tagged as the brotli representation.
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

    let mut resp = Response::status(200)
        .header("Content-Type", &mime)
        .header("Cache-Control", cache_control)
        .header("ETag", &etag)
        .header("Last-Modified", &last_modified)
        .body(body)
        .verbatim();
    if let Some(enc) = content_encoding {
        resp = resp.header("Content-Encoding", enc);
    }
    Ok(resp)
}

/// Lookback window used to locate the last N lines when `?n=N&offset=0`.
/// 64 KiB covers several hundred typical JSON log lines; enlarge if very
/// long lines are common.
const TAIL_LOOKBACK: u64 = 64 * 1024;

/// Hard cap on bytes returned per incremental chunk (`offset > 0` path).
/// Prevents blocking the event loop for more than a few milliseconds.
const MAX_TAIL_BYTES: u64 = 512 * 1024;

/// Serve a file from a byte offset (tail mode), for a route with `tail = true`.
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
fn serve_tail(req: &Request, fs_path: &Path) -> Result<Response> {
    // Parse ?offset=N (default 0) and ?n=N (default 0 = no-line-limit).
    let query = req.query();
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

    let Ok(mut file) = std::fs::File::open(fs_path) else {
        return Ok(Response::not_found());
    };

    // Determine current file size.
    let file_size = file.seek(SeekFrom::End(0)).map_err(|e| m6_core::Error::Other(e.into()))?;

    let (body, end_offset) = if offset == 0 && n > 0 {
        // ── tail -n N mode ────────────────────────────────────────────────────
        // Scan the last TAIL_LOOKBACK bytes for the start of the last N lines.
        let lookback = TAIL_LOOKBACK.min(file_size);
        let scan_start = file_size - lookback;
        file.seek(SeekFrom::Start(scan_start)).map_err(|e| m6_core::Error::Other(e.into()))?;
        let mut buf = Vec::new();
        Read::by_ref(&mut file)
            .take(lookback)
            .read_to_end(&mut buf)
            .map_err(|e| m6_core::Error::Other(e.into()))?;

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
        file.seek(SeekFrom::Start(read_from)).map_err(|e| m6_core::Error::Other(e.into()))?;
        let mut body = Vec::new();
        Read::by_ref(&mut file)
            .take(MAX_TAIL_BYTES)
            .read_to_end(&mut body)
            .map_err(|e| m6_core::Error::Other(e.into()))?;
        let end_offset = read_from + body.len() as u64;
        (body, end_offset)
    };

    // Same table as the main path above; see the note there.
    let mime = m6_core::mime::mime_from_path(fs_path).to_string();

    Ok(Response::status(200)
        .header("Content-Type", &mime)
        .header("Cache-Control", "no-store")
        .header("X-Log-End", &end_offset.to_string())
        .body(body)
        .verbatim())
}

#[cfg(test)]
mod tests {
    use super::*;
    use m6_core::http::RawRequest;
    use serde_json::{json, Map, Value};

    /// Build the `Request` the service loop would hand this handler: the
    /// route's settings, the path parameters core captured, and the config.
    fn request(
        method: &str,
        path: &str,
        query: Option<&str>,
        headers: &[(&str, &str)],
        site_dir: &std::path::Path,
        settings: Value,
        params: &[(&str, &str)],
    ) -> Request {
        let raw = RawRequest {
            version: "HTTP/1.1".to_string(),
            method: method.to_string(),
            path: path.to_string(),
            query: query.map(str::to_string),
            headers: headers
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            body: vec![],
        };
        let mut dict = Map::new();
        for (k, v) in params {
            dict.insert(k.to_string(), json!(v));
        }
        let mut s = Map::new();
        if let Some(obj) = settings.as_object() {
            for (k, v) in obj {
                s.insert(k.clone(), v.clone());
            }
        }
        Request::new(raw, dict, site_dir.to_path_buf())
            .with_route_settings(std::sync::Arc::new(s))
    }

    /// Serve one request and return (status, headers, body) as they would go
    /// on the wire, through the same responder production uses.
    fn wire(req: &Request) -> (u16, Vec<(String, String)>, Vec<u8>) {
        let resp = serve(req).expect("handler");
        let mut out = Vec::new();
        {
            let mut r = m6_core::h1::Responder::new(&mut out, req.method(), false);
            resp.send(&mut r).expect("send");
        }
        let sep = out.windows(4).position(|w| w == b"\r\n\r\n").expect("header terminator");
        let head = std::str::from_utf8(&out[..sep]).expect("headers are ASCII");
        let mut lines = head.lines();
        let status: u16 =
            lines.next().unwrap().split_whitespace().nth(1).unwrap().parse().unwrap();
        let headers = lines
            .filter_map(|l| l.split_once(": ").map(|(k, v)| (k.to_lowercase(), v.to_string())))
            .collect();
        (status, headers, out[sep + 4..].to_vec())
    }

    fn header<'a>(hs: &'a [(String, String)], name: &str) -> Option<&'a str> {
        hs.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }

    fn tail_req(dir: &std::path::Path, name: &str, query: &str) -> Request {
        request(
            "GET",
            &format!("/logs/tail/{name}"),
            Some(query),
            &[],
            dir,
            json!({"root": "", "tail": true}),
            &[("relpath", name)],
        )
    }

    #[test]
    fn tail_from_zero_returns_full_content() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("app.log"), b"line1\nline2\n").unwrap();
        let (status, headers, body) = wire(&tail_req(dir.path(), "app.log", "offset=0"));
        assert_eq!(status, 200);
        assert_eq!(body, b"line1\nline2\n");
        assert_eq!(header(&headers, "x-log-end").unwrap(), "12");
        assert_eq!(header(&headers, "cache-control").unwrap(), "no-store");
    }

    #[test]
    fn tail_from_mid_offset_returns_new_bytes_only() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("app.log"), b"line1\nline2\nline3\n").unwrap();
        let (_, headers, body) = wire(&tail_req(dir.path(), "app.log", "offset=12"));
        assert_eq!(body, b"line3\n");
        assert_eq!(header(&headers, "x-log-end").unwrap(), "18");
    }

    #[test]
    fn tail_beyond_eof_returns_empty_body() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("app.log"), b"abc").unwrap();
        let (status, headers, body) = wire(&tail_req(dir.path(), "app.log", "offset=999"));
        assert_eq!(status, 200);
        assert!(body.is_empty());
        assert_eq!(header(&headers, "x-log-end").unwrap(), "3", "clamped to file size");
    }

    #[test]
    fn tail_n_returns_last_n_lines() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("app.log"), b"line1\nline2\nline3\nline4\n").unwrap();
        let (status, headers, body) = wire(&tail_req(dir.path(), "app.log", "offset=0&n=2"));
        assert_eq!(status, 200);
        assert_eq!(body, b"line3\nline4\n");
        // X-Log-End must equal file size so the next poll starts at EOF.
        assert_eq!(header(&headers, "x-log-end").unwrap(), "24");
    }

    #[test]
    fn tail_n_fewer_lines_than_n_returns_all() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("app.log"), b"only\none\n").unwrap();
        let (_, headers, body) = wire(&tail_req(dir.path(), "app.log", "offset=0&n=100"));
        assert_eq!(body, b"only\none\n");
        assert_eq!(header(&headers, "x-log-end").unwrap(), "9");
    }

    #[test]
    fn tail_missing_file_returns_404() {
        let dir = tempfile::tempdir().unwrap();
        let (status, _, _) = wire(&tail_req(dir.path(), "missing.log", "offset=0"));
        assert_eq!(status, 404);
    }

    fn asset_req(
        dir: &std::path::Path,
        name: &str,
        accept_encoding: Option<&str>,
        extra: &[(&str, &str)],
    ) -> Request {
        let mut headers: Vec<(&str, &str)> = vec![];
        if let Some(ae) = accept_encoding {
            headers.push(("Accept-Encoding", ae));
        }
        headers.extend_from_slice(extra);
        request(
            "GET",
            &format!("/assets/{name}"),
            None,
            &headers,
            dir,
            json!({"root": "assets/"}),
            &[("relpath", name)],
        )
    }

    fn with_compression(mut req: Request, mime: &str) -> Request {
        let mut cfg = m6_core::config::RendererConfig::default();
        cfg.compression.insert(
            mime.to_string(),
            m6_core::config::CompressionLevel { brotli: 6, gzip: 6 },
        );
        req = req.with_config(std::sync::Arc::new(cfg));
        req
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

    fn css_dir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("assets")).unwrap();
        std::fs::write(dir.path().join("assets/style.css"), compressible_css()).unwrap();
        dir
    }

    fn fetch(dir: &std::path::Path, ae: Option<&str>) -> (String, Option<String>, usize) {
        let req = with_compression(asset_req(dir, "style.css", ae, &[]), "text/css");
        let (status, headers, body) = wire(&req);
        assert_eq!(status, 200);
        (
            header(&headers, "etag").expect("etag").to_string(),
            header(&headers, "content-encoding").map(str::to_string),
            body.len(),
        )
    }

    /// The defect: brotli, gzip and identity of the same file all carried one
    /// strong validator. RFC 9110 requires a strong ETag to identify the
    /// representation actually sent, and a downstream shared cache that
    /// believes otherwise can hand a brotli body to a gzip-only client.
    #[test]
    fn each_content_coding_gets_its_own_etag() {
        let dir = css_dir();
        let (e_br, ce_br, _) = fetch(dir.path(), Some("br"));
        let (e_gz, ce_gz, _) = fetch(dir.path(), Some("gzip"));
        let (e_id, ce_id, _) = fetch(dir.path(), None);

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
        let dir = css_dir();
        let (e_id, _, _) = fetch(dir.path(), None);
        assert!(!e_id.contains("-br"), "identity tag carries a coding suffix: {e_id}");
        assert!(!e_id.contains("-gz"), "identity tag carries a coding suffix: {e_id}");
    }

    #[test]
    fn the_same_representation_is_stable_across_requests() {
        let dir = css_dir();
        assert_eq!(fetch(dir.path(), Some("br")).0, fetch(dir.path(), Some("br")).0);
    }

    /// A conditional request carrying the brotli tag must 304 for brotli, and
    /// must NOT 304 for a client that can only take gzip: that pairing is
    /// exactly what the shared tag made indistinguishable.
    #[test]
    fn a_brotli_etag_does_not_validate_a_gzip_request() {
        let dir = css_dir();
        let (e_br, _, _) = fetch(dir.path(), Some("br"));
        let cond = |ae: &str| -> u16 {
            let req = with_compression(
                asset_req(dir.path(), "style.css", Some(ae), &[("If-None-Match", &e_br)]),
                "text/css",
            );
            wire(&req).0
        };
        assert_eq!(cond("br"), 304, "the brotli tag should validate a brotli request");
        assert_eq!(cond("gzip"), 200, "the brotli tag must not validate a gzip request");
    }

    /// A directory has metadata and a size, so the HEAD fast path answered
    /// `200` with a `Content-Length` for one while the GET beside it answered
    /// 404. The read the fast path skips was also what rejected a non-file.
    #[test]
    fn a_head_on_a_directory_is_not_a_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("assets/css")).unwrap();
        let req = request(
            "HEAD",
            "/assets/css",
            None,
            &[],
            dir.path(),
            json!({"root": "assets/"}),
            &[("relpath", "css")],
        );
        let (status, _, _) = wire(&req);
        assert_ne!(status, 200, "a directory is not a representation");
    }

    /// A HEAD must report exactly what the GET would send.
    #[test]
    fn head_reports_exactly_what_get_would() {
        let dir = css_dir();
        let get = wire(&with_compression(asset_req(dir.path(), "style.css", None, &[]), "text/css"));
        let mut head_req = request(
            "HEAD",
            "/assets/style.css",
            None,
            &[],
            dir.path(),
            json!({"root": "assets/"}),
            &[("relpath", "style.css")],
        );
        head_req = with_compression(head_req, "text/css");
        let (status, headers, body) = wire(&head_req);

        assert_eq!(status, get.0);
        assert!(body.is_empty(), "a HEAD carries no body");
        assert_eq!(header(&headers, "etag"), header(&get.1, "etag"));
        assert_eq!(header(&headers, "content-type"), header(&get.1, "content-type"));
        assert_eq!(
            header(&headers, "content-length").unwrap().parse::<usize>().unwrap(),
            get.2.len(),
            "the length must be what the GET actually sent"
        );
    }

    #[test]
    fn a_method_other_than_get_or_head_is_405() {
        let dir = css_dir();
        let req = request(
            "POST",
            "/assets/style.css",
            None,
            &[],
            dir.path(),
            json!({"root": "assets/"}),
            &[("relpath", "style.css")],
        );
        assert_eq!(wire(&req).0, 405);
    }

    #[test]
    fn a_missing_file_is_404() {
        let dir = css_dir();
        let req = asset_req(dir.path(), "nothing.css", None, &[]);
        assert_eq!(wire(&req).0, 404);
    }

    /// Text types must declare UTF-8 or a client falls back to Latin-1 and
    /// renders UTF-8 as mojibake.
    #[test]
    fn text_types_declare_utf8() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("assets")).unwrap();
        std::fs::write(dir.path().join("assets/a.md"), b"# hi").unwrap();
        let (_, headers, _) = wire(&asset_req(dir.path(), "a.md", None, &[]));
        assert_eq!(header(&headers, "content-type").unwrap(), "text/markdown; charset=utf-8");
    }

    /// A symlink pointing outside the site directory is a 404, not a file.
    #[test]
    #[cfg(unix)]
    fn a_symlink_escaping_the_site_dir_is_404() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("assets")).unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret"), b"secret").unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("secret"),
            dir.path().join("assets/escape"),
        )
        .unwrap();
        assert_eq!(wire(&asset_req(dir.path(), "escape", None, &[])).0, 404);
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
