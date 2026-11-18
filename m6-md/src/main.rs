/// m6-md — convert a directory of Markdown files into a single JSON params file.
///
/// Usage: m6-md <input-dir> --output <file> [--log-level <level>]
///
/// Each *.md file may begin with a TOML frontmatter block delimited by +++.
/// Files beginning with _ are skipped (drafts/partials convention).
/// Output: { "documents": [...] } sorted by date descending.
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use m6_core::log::{info, warn};
use serde_json::{json, Map, Value};

// ── Markdown rendering ────────────────────────────────────────────────────────

fn render_markdown(src: &str) -> String {
    use comrak::{markdown_to_html, Options};
    let mut opts = Options::default();
    opts.extension.strikethrough = true;
    opts.extension.table = true;
    opts.extension.autolink = true;
    opts.extension.tasklist = true;
    opts.extension.footnotes = true;
    opts.extension.shortcodes = true;
    opts.render.unsafe_ = true; // pass through raw HTML in source
    markdown_to_html(src, &opts)
}

// ── Front-matter parsing ──────────────────────────────────────────────────────

/// Split a .md source into (frontmatter_toml_str, body_str).
/// Returns (None, full_source) if no +++ block is present.
fn split_frontmatter(src: &str) -> (Option<&str>, &str) {
    let src = src.trim_start_matches('\u{FEFF}'); // strip BOM
    if !src.starts_with("+++") {
        return (None, src);
    }
    // Find closing +++
    let after_open = &src[3..];
    if let Some(close) = after_open.find("\n+++") {
        let fm = &after_open[..close];
        let body = &after_open[close + 4..]; // skip "\n+++"
        let body = body.trim_start_matches('\n');
        (Some(fm), body)
    } else {
        (None, src)
    }
}

/// Parse TOML frontmatter into a JSON map.
/// Protected fields: stem, path, body — warned and ignored if present.
fn parse_frontmatter(toml_str: &str, stem: &str) -> Result<Map<String, Value>> {
    let val: toml::Value = toml::from_str(toml_str)
        .with_context(|| format!("malformed TOML frontmatter in {stem}.md"))?;

    let mut map = Map::new();
    if let toml::Value::Table(tbl) = val {
        for (k, v) in tbl {
            match k.as_str() {
                "stem" | "path" | "body" => {
                    warn!(stem, key = %k, "frontmatter key conflicts with fixed field, ignoring");
                }
                _ => {
                    map.insert(k, toml_to_json(v));
                }
            }
        }
    }
    Ok(map)
}

fn toml_to_json(v: toml::Value) -> Value {
    match v {
        toml::Value::String(s) => json!(s),
        toml::Value::Integer(i) => json!(i),
        toml::Value::Float(f) => json!(f),
        toml::Value::Boolean(b) => json!(b),
        toml::Value::Datetime(d) => json!(d.to_string()),
        toml::Value::Array(arr) => Value::Array(arr.into_iter().map(toml_to_json).collect()),
        toml::Value::Table(tbl) => {
            let mut m = Map::new();
            for (k, v) in tbl {
                m.insert(k, toml_to_json(v));
            }
            Value::Object(m)
        }
    }
}

// ── File mtime fallback ───────────────────────────────────────────────────────

fn file_mtime_iso(path: &Path) -> String {
    let mtime = std::fs::metadata(path)
        .and_then(|m| m.modified())
        .unwrap_or_else(|_| m6_core::platform::wall());
    m6_core::util::iso_date_from(mtime)
}

// ── Process one file ──────────────────────────────────────────────────────────

fn process_file(path: &Path) -> Result<Value> {
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("untitled")
        .to_string();

    let src =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;

    let (fm_str, body_md) = split_frontmatter(&src);

    let mut fields: Map<String, Value> = if let Some(fm) = fm_str {
        parse_frontmatter(fm, &stem)?
    } else {
        Map::new()
    };

    // Fixed fields (set after frontmatter so they can't be overridden)
    let title = fields
        .remove("title")
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| stem.replace('-', " "));

    let date = fields
        .remove("date")
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| file_mtime_iso(path));

    let body_html = render_markdown(body_md);

    // Build final object: fixed fields first, then passthrough
    let mut obj = Map::new();
    obj.insert("stem".into(), json!(stem));
    obj.insert("path".into(), json!(format!("/{stem}")));
    obj.insert("title".into(), json!(title));
    obj.insert("date".into(), json!(date));
    obj.insert("body".into(), json!(body_html));
    for (k, v) in fields {
        obj.insert(k, v);
    }

    Ok(Value::Object(obj))
}

// ── CLI ───────────────────────────────────────────────────────────────────────

struct Cli {
    input_dir: PathBuf,
    output: PathBuf,
    log_level: String,
}

fn parse_args(args: &[String]) -> Result<Cli> {
    let mut input_dir = None;
    let mut output = None;
    let mut log_level = "info".to_string();
    let mut i = 1usize;

    while i < args.len() {
        match args[i].as_str() {
            "--output" => {
                i += 1;
                if i >= args.len() {
                    bail!("--output requires a value");
                }
                output = Some(PathBuf::from(&args[i]));
            }
            "--log-level" => {
                i += 1;
                if i >= args.len() {
                    bail!("--log-level requires a value");
                }
                log_level = args[i].clone();
            }
            arg if arg.starts_with("--") => bail!("unknown flag: {arg}"),
            _ => {
                if input_dir.is_none() {
                    input_dir = Some(PathBuf::from(&args[i]));
                } else {
                    bail!("unexpected positional argument: {}", args[i]);
                }
            }
        }
        i += 1;
    }

    let input_dir = input_dir.ok_or_else(|| anyhow::anyhow!("required: <input-dir>"))?;
    let output = output.ok_or_else(|| anyhow::anyhow!("required: --output <file>"))?;

    m6_core::log::parse_level(&log_level)
        .map_err(|e| anyhow::anyhow!("--log-level {log_level:?}: {e}"))?;

    Ok(Cli {
        input_dir,
        output,
        log_level,
    })
}

// ── Entry point ───────────────────────────────────────────────────────────────

/// The global allocator: mimalloc, which also counts calls when the
/// `count-allocations` feature turns on m6-core's `alloc-count`. The counting
/// is not compiled in a plain build.
#[global_allocator]
static ALLOC: m6_core::alloc::M6Alloc = m6_core::alloc::M6Alloc;

fn main() {
    // `--version` and `--version-full` answer before the input directory is
    // required.
    m6_core::version_flags!(m6_core::build_info::linked());
    // The start-up record's identity line: the binary first, then each m6
    // crate it links. The record itself is written once logging is up.
    m6_core::startup_identity!(m6_core::build_info::linked());

    let args: Vec<String> = std::env::args().collect();
    std::process::exit(run(args));
}

fn run(args: Vec<String>) -> i32 {
    let cli = match parse_args(&args) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("usage error: {e}");
            eprintln!("Usage: m6-md <input-dir> --output <file> [--log-level <level>]");
            return 2;
        }
    };

    // Init logging
    let level = match m6_core::log::parse_level(&cli.log_level) {
        Ok(level) => level,
        Err(e) => {
            eprintln!("usage error: --log-level {e}");
            return 2;
        }
    };
    if let Err(e) = m6_core::log::init(level) {
        eprintln!("logging init error: {e}");
        return 1;
    }

    // The build tool has no config file: its invocation is its config, so the
    // record carries the identity, the flags, the platform facts and the count.
    {
        let argv = args.clone();
        let mut trace = m6_core::startup::ConfigTrace::new();
        let flags = [
            m6_core::startup::FlagDecl {
                name: "--output",
                takes_value: true,
                setting: None,
                secret: false,
            },
            m6_core::startup::FlagDecl {
                name: "--log-level",
                takes_value: true,
                setting: None,
                secret: false,
            },
        ];
        let invocation = m6_core::startup::Invocation::read(&argv, &flags, &[], &mut trace);
        let (own, linked) = m6_core::startup::app_identity();
        let record = m6_core::startup::Record::new("m6-md", own, linked, argv)
            .invocation(invocation, trace)
            .host(m6_core::startup::HostReport::decided())
            .limits(m6_core::startup::LimitsReport::read(
                &m6_core::limits::Limits::default(),
                &m6_core::startup::ConfigTrace::new(),
            ));
        record.emit();
    }

    // Run the one pass, then write everything buffered before this tool exits.
    let result = process(&cli);
    m6_core::startup::shutdown(if result.is_ok() {
        "pass complete"
    } else {
        "pass failed"
    });
    if let Err(e) = m6_core::log::stop() {
        eprintln!("logging stop error: {e}");
        return 1;
    }
    if let Err(e) = result {
        eprintln!("error: {e:#}");
        return 1;
    }

    0
}

fn process(cli: &Cli) -> Result<()> {
    // Validate input directory
    if !cli.input_dir.exists() {
        bail!(
            "input directory does not exist: {}",
            cli.input_dir.display()
        );
    }
    if !cli.input_dir.is_dir() {
        bail!("input path is not a directory: {}", cli.input_dir.display());
    }

    // Validate output parent directory
    let output_parent = cli.output.parent().unwrap_or(Path::new("."));
    if !output_parent.exists() {
        bail!(
            "output directory does not exist: {}",
            output_parent.display()
        );
    }

    // Collect *.md files (non-recursive, skip _ prefix)
    let mut paths: Vec<PathBuf> = std::fs::read_dir(&cli.input_dir)
        .with_context(|| format!("reading directory {}", cli.input_dir.display()))?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.is_file()
                && p.extension().and_then(|e| e.to_str()) == Some("md")
                && !p
                    .file_name()
                    .and_then(|n| n.to_str())
                    .map(|n| n.starts_with('_'))
                    .unwrap_or(false)
        })
        .collect();

    paths.sort();

    info!(count = paths.len(), dir = %cli.input_dir.display(), "processing markdown files");

    // Process each file
    let mut documents: Vec<Value> = Vec::new();
    for path in &paths {
        match process_file(path) {
            Ok(doc) => {
                info!(file = %path.display(), "processed");
                documents.push(doc);
            }
            Err(e) => {
                eprintln!("error processing {}: {e:#}", path.display());
                return Err(e);
            }
        }
    }

    // Sort by date descending, then stem ascending for ties
    documents.sort_by(|a, b| {
        let da = a.get("date").and_then(|v| v.as_str()).unwrap_or("");
        let db = b.get("date").and_then(|v| v.as_str()).unwrap_or("");
        db.cmp(da).then_with(|| {
            let sa = a.get("stem").and_then(|v| v.as_str()).unwrap_or("");
            let sb = b.get("stem").and_then(|v| v.as_str()).unwrap_or("");
            sa.cmp(sb)
        })
    });

    let output_json = serde_json::to_string_pretty(&json!({ "documents": documents }))
        .context("serialising output")?;

    // Atomic write: write to .tmp, then rename
    let tmp_path = cli.output.with_extension("json.tmp");
    std::fs::write(&tmp_path, &output_json)
        .with_context(|| format!("writing {}", tmp_path.display()))?;
    std::fs::rename(&tmp_path, &cli.output)
        .with_context(|| format!("renaming to {}", cli.output.display()))?;

    info!(
        output = %cli.output.display(),
        documents = documents.len(),
        "wrote output"
    );

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    fn parse_err(args: &[&str]) -> String {
        match parse_args(&argv(args)) {
            Ok(_) => panic!("expected a parse error"),
            Err(e) => e.to_string(),
        }
    }

    #[test]
    fn watch_and_touch_are_refused() {
        let err = parse_err(&["m6-md", "in", "--output", "o.json", "--watch"]);
        assert!(err.contains("unknown flag: --watch"), "got: {err}");

        let err = parse_err(&["m6-md", "in", "--output", "o.json", "--touch", "x"]);
        assert!(err.contains("unknown flag: --touch"), "got: {err}");
    }

    #[test]
    fn an_unknown_log_level_is_a_usage_error() {
        let level = "info,=bad==";
        assert!(
            m6_core::log::parse_level(level).is_err(),
            "{level:?} should be a level the logger refuses"
        );
        let err = parse_err(&["m6-md", "in", "--output", "o.json", "--log-level", level]);
        assert!(err.contains("--log-level"), "got: {err}");
    }

    #[test]
    fn a_valid_run_writes_the_output() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("post.md"),
            "+++\ntitle = \"t\"\ndate = \"2026-01-01\"\n+++\nbody\n",
        )
        .unwrap();
        let out = dir.path().join("out.json");
        let rc = run(argv(&[
            "m6-md",
            &dir.path().to_string_lossy(),
            "--output",
            &out.to_string_lossy(),
        ]));
        assert_eq!(rc, 0);
        let written: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&out).unwrap()).unwrap();
        assert_eq!(written["documents"][0]["stem"].as_str(), Some("post"));
    }
}
