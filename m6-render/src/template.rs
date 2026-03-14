/// Tera template engine setup and custom filters.
use std::collections::HashMap;
use std::path::Path;

use anyhow::Context;
use serde_json::Value;
use tera::Tera;

/// Initialise Tera with all templates under `site_dir` and register custom filters.
/// Returns Err if any template has a syntax error (→ exit 2).
pub fn build_tera(site_dir: &Path) -> anyhow::Result<Tera> {
    // Load all *.html and *.txt templates under site_dir.
    let pattern = format!("{}/**/*.{{html,txt,xml,json}}", site_dir.display());
    let mut tera = Tera::new(&pattern).context("compiling templates")?;

    // Also load plain HTML pattern for subdirectories.
    register_filters(&mut tera);
    Ok(tera)
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
    register_filters(&mut tera);
    Ok(tera)
}

fn register_filters(tera: &mut Tera) {
    tera.register_filter("slugify", filter_slugify);
    tera.register_filter("date_format", filter_date_format);
    tera.register_filter("markdown", filter_markdown);
    tera.register_filter("truncate_words", filter_truncate_words);
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
        register_filters(&mut tera);
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
