/// Tera template engine setup and custom filters.
use std::collections::HashMap;
use std::path::Path;

use anyhow::Context;
use serde_json::Value;
use tera::Tera;

/// Initialise Tera with all templates under `site_dir/templates/` and register custom filters.
/// Templates are registered under paths relative to `site_dir` so that
/// `Response::render_with("templates/foo.html", …)` resolves correctly.
pub fn build_tera(site_dir: &Path) -> anyhow::Result<Tera> {
    let mut contents: Vec<(String, String)> = Vec::new();
    let templates_dir = site_dir.join("templates");
    if templates_dir.is_dir() {
        collect_templates(site_dir, &templates_dir, &mut contents)?;
    }

    let mut tera = Tera::default();
    let pairs: Vec<(&str, &str)> = contents.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    tera.add_raw_templates(pairs).context("compiling templates")?;
    register_filters(&mut tera, site_dir);
    Ok(tera)
}

/// Recursively collect all template files under `dir`, keyed by path relative to `site_dir`.
fn collect_templates(
    site_dir: &Path,
    dir: &Path,
    out: &mut Vec<(String, String)>,
) -> anyhow::Result<()> {
    let entries = std::fs::read_dir(dir)
        .with_context(|| format!("reading template dir {}", dir.display()))?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_templates(site_dir, &path, out)?;
        } else if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            if matches!(ext, "html" | "txt" | "xml" | "json") {
                if let Ok(rel) = path.strip_prefix(site_dir) {
                    if let Some(name) = rel.to_str() {
                        let content = std::fs::read_to_string(&path)
                            .with_context(|| format!("reading template {}", path.display()))?;
                        out.push((name.to_string(), content));
                    }
                }
            }
        }
    }
    Ok(())
}

/// Build a Tera instance from explicit template paths (relative to site_dir).
/// Also loads all *.html/*.txt/*.xml/*.json siblings in each referenced template's
/// directory so that `{% extends %}` and `{% include %}` work correctly.
pub fn build_tera_from_paths(
    site_dir: &Path,
    template_paths: &[String],
) -> anyhow::Result<Tera> {
    use std::collections::HashSet;

    // Collect unique directories that contain route-referenced templates.
    let mut dirs: HashSet<std::path::PathBuf> = HashSet::new();
    for rel_path in template_paths {
        let abs = site_dir.join(rel_path);
        if let Some(parent) = abs.parent() {
            dirs.insert(parent.to_path_buf());
        }
    }

    // Walk each directory and collect all template files.
    let mut all_paths: Vec<String> = Vec::new();
    for dir in &dirs {
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_file() {
                    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
                        if matches!(ext, "html" | "txt" | "xml" | "json") {
                            if let Ok(rel) = path.strip_prefix(site_dir) {
                                if let Some(s) = rel.to_str() {
                                    all_paths.push(s.to_string());
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // Collect all (name, content) pairs and add as a batch so Tera can resolve
    // `extends`/`include` chains regardless of insertion order.
    let mut contents: Vec<(String, String)> = Vec::new();
    for rel_path in &all_paths {
        let abs = site_dir.join(rel_path);
        let content = std::fs::read_to_string(&abs)
            .with_context(|| format!("reading template {}", abs.display()))?;
        contents.push((rel_path.clone(), content));
    }

    let mut tera = Tera::default();
    let pairs: Vec<(&str, &str)> = contents.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    tera.add_raw_templates(pairs)
        .context("compiling templates")?;
    register_filters(&mut tera, site_dir);
    Ok(tera)
}

/// Map of asset path (relative to `assets/`) to a short content hash.
///
/// Content-addressed rather than mtime-based on purpose: a deploy that rewrites
/// a file without changing its bytes should not invalidate every client's copy,
/// and rsync timestamps are not stable across machines anyway.
fn build_asset_manifest(site_dir: &Path) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let root = site_dir.join("assets");
    if root.is_dir() {
        collect_asset_hashes(&root, &root, &mut out);
    }
    out
}

/// Intrinsic pixel dimensions for every image under `assets/`, keyed the same
/// way as the hash manifest.
///
/// Built once with Tera, like the hash manifest, so emitting `width`/`height`
/// costs nothing per render. Read from the file headers rather than from a
/// data file, so a replaced image cannot silently keep stale dimensions --
/// which is the failure mode that matters here, since wrong dimensions are
/// worse than none: they letterbox or stretch the image.
fn build_image_dimensions(site_dir: &Path) -> HashMap<String, (u32, u32)> {
    let mut out = HashMap::new();
    let root = site_dir.join("assets");
    if root.is_dir() {
        collect_image_dimensions(&root, &root, &mut out);
    }
    out
}

fn collect_image_dimensions(root: &Path, dir: &Path, out: &mut HashMap<String, (u32, u32)>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_image_dimensions(root, &path, out);
            continue;
        }
        let Ok(rel) = path.strip_prefix(root) else { continue };
        let Ok(bytes) = std::fs::read(&path) else { continue };
        if let Some(dims) = image_dimensions(&bytes) {
            out.insert(rel.to_string_lossy().replace('\\', "/"), dims);
        }
    }
}

/// Intrinsic size of an image from its header bytes.
///
/// Hand-rolled for the four formats this site actually ships (PNG, JPEG,
/// WebP, SVG) rather than pulling in an image crate: the alternative is a
/// dependency and a pile of decoders for formats that are never used, in a
/// platform where the whole point is having few moving parts. Returns None for
/// anything unrecognised or truncated, and the caller then simply omits the
/// attributes.
fn image_dimensions(b: &[u8]) -> Option<(u32, u32)> {
    // PNG: 8-byte signature, then an IHDR chunk whose width/height are the
    // first two big-endian u32s of its payload.
    if b.len() >= 24 && b.starts_with(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]) {
        return Some((be32(&b[16..20])?, be32(&b[20..24])?));
    }

    // GIF, cheap to support while we are here: little-endian u16s at byte 6.
    if b.len() >= 10 && (b.starts_with(b"GIF87a") || b.starts_with(b"GIF89a")) {
        return Some((
            u16::from_le_bytes([b[6], b[7]]) as u32,
            u16::from_le_bytes([b[8], b[9]]) as u32,
        ));
    }

    if b.len() >= 30 && b.starts_with(b"RIFF") && &b[8..12] == b"WEBP" {
        return webp_dimensions(b);
    }

    if b.len() >= 4 && b[0] == 0xFF && b[1] == 0xD8 {
        return jpeg_dimensions(b);
    }

    // SVG is text; look at the root element's attributes.
    let head = &b[..b.len().min(4096)];
    if let Ok(text) = std::str::from_utf8(head) {
        if text.contains("<svg") {
            return svg_dimensions(text);
        }
    }
    None
}

fn be32(b: &[u8]) -> Option<u32> {
    Some(u32::from_be_bytes([*b.first()?, *b.get(1)?, *b.get(2)?, *b.get(3)?]))
}

/// WebP has three container flavours and they store the size differently.
fn webp_dimensions(b: &[u8]) -> Option<(u32, u32)> {
    match &b[12..16] {
        // Lossy: 14-bit dimensions after the 3-byte start code and "\x9d\x01\x2a".
        b"VP8 " => {
            let w = u16::from_le_bytes([*b.get(26)?, *b.get(27)?]) & 0x3FFF;
            let h = u16::from_le_bytes([*b.get(28)?, *b.get(29)?]) & 0x3FFF;
            Some((w as u32, h as u32))
        }
        // Lossless: 14-bit each, packed across four bytes after the signature.
        b"VP8L" => {
            let n = u32::from_le_bytes([*b.get(21)?, *b.get(22)?, *b.get(23)?, *b.get(24)?]);
            Some(((n & 0x3FFF) + 1, ((n >> 14) & 0x3FFF) + 1))
        }
        // Extended: 24-bit minus-one values in the VP8X chunk.
        b"VP8X" => {
            let w = u32::from_le_bytes([*b.get(24)?, *b.get(25)?, *b.get(26)?, 0]) + 1;
            let h = u32::from_le_bytes([*b.get(27)?, *b.get(28)?, *b.get(29)?, 0]) + 1;
            Some((w, h))
        }
        _ => None,
    }
}

/// JPEG stores size in a Start-Of-Frame marker, which sits an arbitrary
/// distance in behind any number of other segments, so the segment chain has
/// to be walked.
fn jpeg_dimensions(b: &[u8]) -> Option<(u32, u32)> {
    let mut i = 2usize;
    // `+ 9 <=`, not `+ 9 <`: the frame header needs bytes i..i+8 inclusive, so
    // a SOF that ends exactly at the buffer's last byte is still readable. The
    // stricter form silently skipped it -- harmless for real JPEGs, which
    // always carry scan data afterwards, but wrong, and caught by a test whose
    // fixture ends at the header.
    while i + 9 <= b.len() {
        if b[i] != 0xFF {
            i += 1;
            continue;
        }
        let marker = b[i + 1];
        // SOF0..SOF15, excluding the four that are not frame headers.
        if (0xC0..=0xCF).contains(&marker)
            && marker != 0xC4 && marker != 0xC8 && marker != 0xCC
        {
            let h = u16::from_be_bytes([b[i + 5], b[i + 6]]) as u32;
            let w = u16::from_be_bytes([b[i + 7], b[i + 8]]) as u32;
            return Some((w, h));
        }
        let len = u16::from_be_bytes([*b.get(i + 2)?, *b.get(i + 3)?]) as usize;
        if len < 2 { return None; }
        i += 2 + len;
    }
    None
}

/// SVG: prefer explicit width/height, fall back to the viewBox extent. Values
/// carrying units (`80px`) are accepted; percentages are not, since a
/// percentage is not an intrinsic size.
fn svg_dimensions(text: &str) -> Option<(u32, u32)> {
    let svg = &text[text.find("<svg")?..];
    let end = svg.find('>').unwrap_or(svg.len());
    let tag = &svg[..end];

    let attr = |name: &str| -> Option<f64> {
        let pat = format!("{name}=\"");
        let start = tag.find(&pat)? + pat.len();
        let rest = &tag[start..];
        let val = &rest[..rest.find('"')?];
        if val.ends_with('%') { return None; }
        val.trim_end_matches(|c: char| c.is_ascii_alphabetic())
            .trim()
            .parse::<f64>()
            .ok()
    };

    if let (Some(w), Some(h)) = (attr("width"), attr("height")) {
        if w > 0.0 && h > 0.0 {
            return Some((w.round() as u32, h.round() as u32));
        }
    }

    let pat = "viewBox=\"";
    let start = tag.find(pat)? + pat.len();
    let rest = &tag[start..];
    let val = &rest[..rest.find('"')?];
    let nums: Vec<f64> = val
        .split(|c: char| c == ',' || c.is_whitespace())
        .filter(|s| !s.is_empty())
        .filter_map(|s| s.parse::<f64>().ok())
        .collect();
    if nums.len() == 4 && nums[2] > 0.0 && nums[3] > 0.0 {
        return Some((nums[2].round() as u32, nums[3].round() as u32));
    }
    None
}

fn collect_asset_hashes(root: &Path, dir: &Path, out: &mut HashMap<String, String>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_asset_hashes(root, &path, out);
        } else if let Ok(bytes) = std::fs::read(&path) {
            if let Ok(rel) = path.strip_prefix(root) {
                let mut h = std::collections::hash_map::DefaultHasher::new();
                std::hash::Hasher::write(&mut h, &bytes);
                let hash = format!("{:08x}", std::hash::Hasher::finish(&h) as u32);
                out.insert(rel.to_string_lossy().replace('\\', "/"), hash);
            }
        }
    }
}

/// Build the versioned URL for one asset.
///
/// Accepts a path with or without the leading `/assets/`, so templates can pass
/// either a literal (`"css/style.css"`) or a value out of data that already
/// carries the prefix.
///
/// An unknown path is returned unversioned rather than erroring. A missing hash
/// is a caching miss, not a broken page, and failing the render over it would
/// turn a typo in one icon name into a blank site.
fn asset_url(manifest: &HashMap<String, String>, raw: &str) -> String {
    let rel = raw.trim_start_matches('/').strip_prefix("assets/").unwrap_or(raw.trim_start_matches('/'));
    match manifest.get(rel) {
        Some(hash) => format!("/assets/{rel}?v={hash}"),
        None => format!("/assets/{rel}"),
    }
}

/// Look up an image and render its dimensions as HTML attributes.
///
/// Accepts the same path shapes as `asset_url` — bare, `/assets/`-prefixed, or
/// carrying a `?v=` cache-busting query, since callers often pass the already
/// versioned URL.
///
/// Returns an empty string when the file is unknown or its header could not be
/// read. Omitting the attributes costs some layout stability; guessing them
/// would distort the image, which is worse.
fn img_dims_attrs(dims: &HashMap<String, (u32, u32)>, raw: &str) -> String {
    let no_query = raw.split('?').next().unwrap_or(raw);
    let trimmed = no_query.trim_start_matches('/');
    let rel = trimmed.strip_prefix("assets/").unwrap_or(trimmed);
    match dims.get(rel) {
        Some((w, h)) => format!(" width=\"{w}\" height=\"{h}\""),
        None => String::new(),
    }
}

/// Sentinel used by `not_found()` so the render-error handler can distinguish
/// "this resource doesn't exist" from a genuine template bug.
pub const NOT_FOUND_SENTINEL: &str = "__M6_NOT_FOUND__";

fn register_filters(tera: &mut Tera, site_dir: &Path) {
    tera.register_filter("slugify", filter_slugify);
    tera.register_filter("date_format", filter_date_format);
    tera.register_filter("markdown", filter_markdown);
    tera.register_filter("truncate_words", filter_truncate_words);

    // `| asset` — content-addressed URL for a file under assets/.
    //
    // The manifest is built once here, not per render: hashing on every request
    // would put a filesystem read and a hash in the hot path of a cache miss.
    // It is rebuilt whenever Tera is, which is what a config reload already
    // does, so a redeployed asset gets a new hash without a restart.
    let manifest = std::sync::Arc::new(build_asset_manifest(site_dir));
    tera.register_filter("asset", move |value: &Value, _args: &HashMap<String, Value>| {
        Ok(Value::String(asset_url(&manifest, value.as_str().unwrap_or(""))))
    });

    // `{{ img_dims(path=x) | safe }}` — ready-to-paste ` width="W" height="H"`.
    //
    // Emitted as one attribute pair rather than two separate filters so that a
    // file with no readable dimensions produces *nothing*, never a half pair.
    // A lone `width` is worse than neither: the HTML width/height attributes
    // are presentational hints for BOTH axes, so one on its own distorts the
    // image. That is exactly the bug that stretched the headline image
    // earlier, and this shape makes it unrepresentable.
    //
    // Needs `| safe` at the call site because it returns markup, not text.
    let dims = std::sync::Arc::new(build_image_dimensions(site_dir));
    tera.register_function("img_dims", move |args: &HashMap<String, Value>| {
        let raw = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
        Ok(Value::String(img_dims_attrs(&dims, raw)))
    });

    // `{{ not_found() }}` — call from a template when a lookup produces no result.
    // Causes the render to fail with the NOT_FOUND sentinel; app.rs maps this to a 404.
    tera.register_function("not_found", |_args: &HashMap<String, Value>| {
        Err(tera::Error::msg(NOT_FOUND_SENTINEL))
    });
}

/// `| slugify` — "Hello World" → "hello-world"
fn filter_slugify(
    value: &Value,
    _args: &HashMap<String, Value>,
) -> tera::Result<Value> {
    let s = value.as_str().unwrap_or("");
    Ok(Value::String(slug::slugify(s)))
}

/// `| date_format(fmt="%B %d, %Y")` — format a date string
fn filter_date_format(
    value: &Value,
    args: &HashMap<String, Value>,
) -> tera::Result<Value> {
    use chrono::NaiveDate;

    let fmt = args
        .get("fmt")
        .and_then(|v| v.as_str())
        .unwrap_or("%B %d, %Y");

    let s = value.as_str().unwrap_or("");
    // Try parsing common date formats.
    let formatted = if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
        dt.format(fmt).to_string()
    } else if let Ok(d) = NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        d.format(fmt).to_string()
    } else {
        s.to_string()
    };
    Ok(Value::String(formatted))
}

/// `| markdown` — render Markdown via comrak
fn filter_markdown(
    value: &Value,
    _args: &HashMap<String, Value>,
) -> tera::Result<Value> {
    let s = value.as_str().unwrap_or("");
    let options = comrak::Options::default();
    let html = comrak::markdown_to_html(s, &options);
    Ok(Value::String(html))
}

/// `| truncate_words(n=50)` — truncate to N words
fn filter_truncate_words(
    value: &Value,
    args: &HashMap<String, Value>,
) -> tera::Result<Value> {
    let n = args
        .get("n")
        .and_then(|v| v.as_u64())
        .unwrap_or(50) as usize;

    let s = value.as_str().unwrap_or("");
    let words: Vec<&str> = s.split_whitespace().collect();
    if words.len() <= n {
        Ok(Value::String(s.to_string()))
    } else {
        Ok(Value::String(words[..n].join(" ") + "…"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_tera_with_template(name: &str, content: &str) -> Tera {
        let mut tera = Tera::default();
        tera.add_raw_template(name, content).unwrap();
        // No assets/ dir under a bare temp path, so the manifest is empty and
        // `| asset` degrades to unversioned URLs -- fine for the filter tests
        // here, which do not exercise it.
        register_filters(&mut tera, std::path::Path::new("."));
        tera
    }

    #[test]
    fn test_slugify_filter() {
        let tera = make_tera_with_template("t", "{{ val | slugify }}");
        let mut ctx = tera::Context::new();
        ctx.insert("val", "Hello World");
        let out = tera.render("t", &ctx).unwrap();
        assert_eq!(out, "hello-world");
    }

    #[test]
    fn test_markdown_filter() {
        let tera = make_tera_with_template("t", "{{ content | markdown }}");
        let mut ctx = tera::Context::new();
        ctx.insert("content", "# Hello\n\nWorld");
        let out = tera.render("t", &ctx).unwrap();
        assert!(out.contains("<h1>"), "got: {}", out);
        assert!(out.contains("World"), "got: {}", out);
    }

    #[test]
    fn test_truncate_words_filter() {
        let tera = make_tera_with_template("t", r#"{{ content | truncate_words(n=3) }}"#);
        let mut ctx = tera::Context::new();
        ctx.insert("content", "one two three four five");
        let out = tera.render("t", &ctx).unwrap();
        assert!(out.starts_with("one two three"));
    }
}

#[cfg(test)]
mod asset_filter_tests {
    use super::{asset_url, build_asset_manifest};
    use std::collections::HashMap;

    fn write(dir: &std::path::Path, rel: &str, bytes: &[u8]) {
        let p = dir.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, bytes).unwrap();
    }

    #[test]
    fn manifest_hashes_every_asset_including_nested() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), "assets/css/style.css", b"body{}");
        write(tmp.path(), "assets/icons/logo.svg", b"<svg/>");
        write(tmp.path(), "assets/fonts/deep/nested.woff2", b"font");
        let m = build_asset_manifest(tmp.path());
        assert_eq!(m.len(), 3, "{m:?}");
        for k in ["css/style.css", "icons/logo.svg", "fonts/deep/nested.woff2"] {
            assert!(m.contains_key(k), "missing {k} in {m:?}");
        }
    }

    /// Content-addressed, not mtime-based: identical bytes must keep the same
    /// hash so a redeploy that changes nothing does not bust every cache.
    #[test]
    fn hash_follows_content_not_the_file() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        write(a.path(), "assets/x.css", b"same bytes");
        write(b.path(), "assets/x.css", b"same bytes");
        assert_eq!(
            build_asset_manifest(a.path())["x.css"],
            build_asset_manifest(b.path())["x.css"]
        );

        let c = tempfile::tempdir().unwrap();
        write(c.path(), "assets/x.css", b"different bytes");
        assert_ne!(
            build_asset_manifest(a.path())["x.css"],
            build_asset_manifest(c.path())["x.css"],
            "changed content must produce a new hash, or deploys go unnoticed"
        );
    }

    /// Templates pass either a bare relative path or a value out of data that
    /// already carries the /assets/ prefix. Both must land on the same URL.
    #[test]
    fn accepts_bare_and_prefixed_paths_identically() {
        let mut m = HashMap::new();
        m.insert("css/style.css".to_string(), "deadbeef".to_string());
        let want = "/assets/css/style.css?v=deadbeef";
        for input in ["css/style.css", "/assets/css/style.css", "assets/css/style.css"] {
            assert_eq!(asset_url(&m, input), want, "input {input:?}");
        }
    }

    /// A path with no manifest entry degrades to the plain URL. Erroring here
    /// would turn one mistyped icon name into a failed render for the whole
    /// page, which is far worse than that icon missing its cache-busting.
    #[test]
    fn unknown_asset_degrades_to_an_unversioned_url() {
        let m = HashMap::new();
        assert_eq!(asset_url(&m, "icons/nope.svg"), "/assets/icons/nope.svg");
        assert_eq!(asset_url(&m, "/assets/icons/nope.svg"), "/assets/icons/nope.svg");
    }

    /// The filter has to work through Tera, not just as a function: registration
    /// closes over the manifest, and a template calls it by name.
    #[test]
    fn filter_renders_a_versioned_url_through_tera() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), "assets/css/style.css", b"body{}");
        let tera = super::build_tera(tmp.path()).unwrap();

        let hash = build_asset_manifest(tmp.path())["css/style.css"].clone();
        let mut ctx = tera::Context::new();
        ctx.insert("icon", "css/style.css");

        let mut t = tera;
        t.add_raw_template("t", r#"{{ "css/style.css" | asset }}|{{ icon | asset }}"#).unwrap();
        let out = t.render("t", &ctx).unwrap();
        assert_eq!(out, format!("/assets/css/style.css?v={hash}|/assets/css/style.css?v={hash}"));
    }

    #[test]
    fn empty_input_does_not_panic() {
        let m = HashMap::new();
        assert_eq!(asset_url(&m, ""), "/assets/");
    }
}

#[cfg(test)]
mod image_dimension_tests {
    use super::{image_dimensions, img_dims_attrs, svg_dimensions};
    use std::collections::HashMap;

    /// Minimal but structurally real headers, so the parsers are exercised on
    /// byte layout rather than on a fixture that happens to match.
    fn png(w: u32, h: u32) -> Vec<u8> {
        let mut v = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
        v.extend_from_slice(&[0, 0, 0, 13]);        // IHDR length
        v.extend_from_slice(b"IHDR");
        v.extend_from_slice(&w.to_be_bytes());
        v.extend_from_slice(&h.to_be_bytes());
        v.extend_from_slice(&[8, 6, 0, 0, 0]);
        v
    }

    fn jpeg(w: u16, h: u16) -> Vec<u8> {
        let mut v = vec![0xFF, 0xD8];
        // A JFIF APP0 segment first, so the SOF is genuinely not at the front
        // and the segment walk has to do its job.
        v.extend_from_slice(&[0xFF, 0xE0, 0x00, 0x10]);
        v.extend_from_slice(b"JFIF\0");
        v.extend_from_slice(&[0u8; 9]);
        v.extend_from_slice(&[0xFF, 0xC0, 0x00, 0x11, 0x08]);
        v.extend_from_slice(&h.to_be_bytes());
        v.extend_from_slice(&w.to_be_bytes());
        v
    }

    fn webp_lossy(w: u16, h: u16) -> Vec<u8> {
        let mut v = b"RIFF\0\0\0\0WEBPVP8 ".to_vec();
        v.extend_from_slice(&[0u8; 10]);            // chunk size + start code
        v.extend_from_slice(&w.to_le_bytes());
        v.extend_from_slice(&h.to_le_bytes());
        v
    }

    #[test]
    fn reads_png_jpeg_and_webp_headers() {
        assert_eq!(image_dimensions(&png(1300, 1476)), Some((1300, 1476)));
        assert_eq!(image_dimensions(&jpeg(640, 480)), Some((640, 480)));
        assert_eq!(image_dimensions(&webp_lossy(369, 246)), Some((369, 246)));
    }

    /// JPEG width and height are stored height-first; getting that backwards
    /// silently transposes every photo on the site.
    #[test]
    fn jpeg_does_not_transpose_width_and_height() {
        assert_eq!(image_dimensions(&jpeg(800, 200)), Some((800, 200)));
    }

    #[test]
    fn svg_prefers_explicit_size_then_falls_back_to_viewbox() {
        assert_eq!(svg_dimensions(r#"<svg width="39" height="39" viewBox="0 0 78 78">"#), Some((39, 39)));
        assert_eq!(svg_dimensions(r#"<svg viewBox="0 0 24 24">"#), Some((24, 24)));
        assert_eq!(svg_dimensions(r#"<svg width="80px" height="58px">"#), Some((80, 58)));
    }

    /// A percentage is not an intrinsic size. Emitting `width="100"` for
    /// `width="100%"` would be actively wrong.
    #[test]
    fn svg_percentage_is_not_a_size() {
        assert_eq!(svg_dimensions(r#"<svg width="100%" height="100%">"#), None);
        // ...but a viewBox alongside it still is.
        assert_eq!(svg_dimensions(r#"<svg width="100%" height="100%" viewBox="0 0 16 9">"#), Some((16, 9)));
    }

    #[test]
    fn unrecognised_or_truncated_input_yields_nothing() {
        assert_eq!(image_dimensions(b""), None);
        assert_eq!(image_dimensions(b"not an image at all"), None);
        assert_eq!(image_dimensions(&png(10, 10)[..12]), None);   // truncated PNG
        assert_eq!(image_dimensions(&[0xFF, 0xD8]), None);        // JPEG with no SOF
    }

    /// The attribute pair is all-or-nothing: a lone `width` is a presentational
    /// hint for both axes and would distort the image.
    #[test]
    fn attributes_are_emitted_as_a_pair_or_not_at_all() {
        let mut m = HashMap::new();
        m.insert("icons/logo.svg".to_string(), (80u32, 80u32));
        assert_eq!(img_dims_attrs(&m, "icons/logo.svg"), r#" width="80" height="80""#);
        assert_eq!(img_dims_attrs(&m, "icons/unknown.svg"), "");
    }

    /// Callers pass whatever they have: bare, /assets/-prefixed, or the already
    /// versioned URL straight out of `| asset`.
    #[test]
    fn accepts_prefixed_and_versioned_paths() {
        let mut m = HashMap::new();
        m.insert("icons/logo.svg".to_string(), (80u32, 80u32));
        for input in [
            "icons/logo.svg",
            "/assets/icons/logo.svg",
            "assets/icons/logo.svg",
            "/assets/icons/logo.svg?v=deadbeef",
        ] {
            assert_eq!(img_dims_attrs(&m, input), r#" width="80" height="80""#, "input {input:?}");
        }
    }
}

// ---------------------------------------------------------------------------
// The `m6-core` renderer seam
// ---------------------------------------------------------------------------

/// Tera, behind `m6_core::render::Renderer`.
///
/// The service loop in `m6-core` hands over a template name and a finished
/// context and gets bytes back. It does not link Tera and does not know this
/// type exists.
pub struct TeraRenderer {
    tera: Tera,
}

impl m6_core::render::Renderer for TeraRenderer {
    fn render(
        &self,
        template: &str,
        ctx: &serde_json::Map<String, Value>,
    ) -> std::result::Result<String, m6_core::render::RenderError> {
        let mut tctx = tera::Context::new();
        for (k, v) in ctx {
            tctx.insert(k.as_str(), v);
        }
        self.tera.render(template, &tctx).map_err(|e| {
            // `{{ not_found() }}` fails the render with a sentinel, because a
            // Tera error carries nothing but a string. The sentinel stops
            // here: core is told `NotFound`, not handed a string to search.
            let msg = format!("{e:#}");
            if msg.contains(NOT_FOUND_SENTINEL) {
                m6_core::render::RenderError::NotFound
            } else {
                m6_core::render::RenderError::Failed(
                    anyhow::Error::new(e).context(format!("rendering template {template}")),
                )
            }
        })
    }
}

/// Builds a `TeraRenderer`, at startup and again on every config reload.
pub struct TeraFactory;

impl m6_core::render::RendererFactory for TeraFactory {
    fn build(
        &self,
        site_dir: &Path,
        template_paths: &[String],
    ) -> anyhow::Result<Box<dyn m6_core::render::Renderer>> {
        // No templates named by config routes means a handler app calling
        // `render_with` directly, so load everything under `site_dir`.
        let tera = if template_paths.is_empty() {
            build_tera(site_dir).context("compiling templates")?
        } else {
            build_tera_from_paths(site_dir, template_paths).context("compiling templates")?
        };
        Ok(Box::new(TeraRenderer { tera }))
    }
}
