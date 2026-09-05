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
