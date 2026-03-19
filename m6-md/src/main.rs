/// m6-md — convert a directory of Markdown files into a single JSON params file.
///
/// Usage: m6-md <input-dir> --output <output-file> [--log-level <level>]
///              [--watch] [--touch <path>]
///
/// Each *.md file may begin with a TOML frontmatter block delimited by +++.
/// Files beginning with _ are skipped (drafts/partials convention).
/// Output: { "documents": [...] } sorted by date descending.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use serde_json::{json, Map, Value};
use tracing::{debug, info, warn};

// ── Markdown rendering ────────────────────────────────────────────────────────

fn render_markdown(src: &str) -> String {
    use comrak::{markdown_to_html, Options};
    let mut opts = Options::default();
    opts.extension.strikethrough   = true;
    opts.extension.table           = true;
    opts.extension.autolink        = true;
    opts.extension.tasklist        = true;
    opts.extension.footnotes       = true;
    opts.extension.shortcodes      = true;
    opts.render.unsafe_            = true; // pass through raw HTML in source
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
        toml::Value::String(s)   => json!(s),
        toml::Value::Integer(i)  => json!(i),
        toml::Value::Float(f)    => json!(f),
        toml::Value::Boolean(b)  => json!(b),
        toml::Value::Datetime(d) => json!(d.to_string()),
        toml::Value::Array(arr)  => Value::Array(arr.into_iter().map(toml_to_json).collect()),
        toml::Value::Table(tbl)  => {
            let mut m = Map::new();
            for (k, v) in tbl { m.insert(k, toml_to_json(v)); }
            Value::Object(m)
        }
    }
}

// ── File mtime fallback ───────────────────────────────────────────────────────

fn file_mtime_iso(path: &Path) -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let mtime = std::fs::metadata(path)
        .and_then(|m| m.modified())
        .unwrap_or(SystemTime::now());
    let secs = mtime.duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    // Format as YYYY-MM-DD
    let days = secs / 86400;
    let y400 = days / 146097;
    let rem  = days % 146097;
    let y100 = (rem / 36524).min(3);
    let rem  = rem - y100 * 36524;
    let y4   = rem / 1461;
    let rem  = rem % 1461;
    let y1   = (rem / 365).min(3);
    let doy  = rem - y1 * 365;
    let year = (y400 * 400 + y100 * 100 + y4 * 4 + y1 + 1970) as u32;
    let (month, day) = doy_to_md(doy as u32, is_leap(year));
    format!("{:04}-{:02}-{:02}", year, month, day)
}

fn is_leap(y: u32) -> bool { y % 4 == 0 && (y % 100 != 0 || y % 400 == 0) }

fn doy_to_md(doy: u32, leap: bool) -> (u32, u32) {
    let months: &[u32] = if leap {
        &[31,29,31,30,31,30,31,31,30,31,30,31]
    } else {
        &[31,28,31,30,31,30,31,31,30,31,30,31]
    };
    let mut rem = doy;
    for (i, &days) in months.iter().enumerate() {
        if rem < days { return ((i + 1) as u32, rem + 1); }
        rem -= days;
    }
    (12, 31)
}

// ── Process one file ──────────────────────────────────────────────────────────

fn process_file(path: &Path) -> Result<Value> {
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("untitled")
        .to_string();

    let src = std::fs::read_to_string(path)
        .with_context(|| format!("reading {}", path.display()))?;

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
    obj.insert("stem".into(),  json!(stem));
    obj.insert("path".into(),  json!(format!("/{stem}")));
    obj.insert("title".into(), json!(title));
    obj.insert("date".into(),  json!(date));
    obj.insert("body".into(),  json!(body_html));
    for (k, v) in fields {
        obj.insert(k, v);
    }

    Ok(Value::Object(obj))
}

// ── CLI ───────────────────────────────────────────────────────────────────────

struct Cli {
    input_dir: PathBuf,
    output:    PathBuf,
    log_level: String,
    watch:     bool,
    touch:     Option<PathBuf>,
}

fn parse_args(args: &[String]) -> Result<Cli> {
    let mut input_dir = None;
    let mut output    = None;
    let mut log_level = "info".to_string();
    let mut watch     = false;
    let mut touch     = None;
    let mut i = 1usize;

    while i < args.len() {
        match args[i].as_str() {
            "--output" => {
                i += 1;
                if i >= args.len() { bail!("--output requires a value"); }
                output = Some(PathBuf::from(&args[i]));
            }
            "--log-level" => {
                i += 1;
                if i >= args.len() { bail!("--log-level requires a value"); }
                log_level = args[i].clone();
            }
            "--watch" => {
                watch = true;
            }
            "--touch" => {
                i += 1;
                if i >= args.len() { bail!("--touch requires a value"); }
                touch = Some(PathBuf::from(&args[i]));
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
    let output    = output.ok_or_else(|| anyhow::anyhow!("required: --output <file>"))?;

    Ok(Cli { input_dir, output, log_level, watch, touch })
}

// ── Entry point ───────────────────────────────────────────────────────────────

static SHUTDOWN: AtomicBool = AtomicBool::new(false);

fn main() {
    let args: Vec<String> = std::env::args().collect();
    std::process::exit(run(args));
}

fn run(args: Vec<String>) -> i32 {
    let cli = match parse_args(&args) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("usage error: {e}");
            eprintln!("Usage: m6-md <input-dir> --output <file> [--log-level <level>] [--watch] [--touch <path>]");
            return 2;
        }
    };

    // Init logging
    let filter = tracing_subscriber::EnvFilter::try_new(&cli.log_level)
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(filter)
        .init();

    // First pass — always run once.
    if let Err(e) = process(&cli) {
        eprintln!("error: {e:#}");
        return 1;
    }

    if !cli.watch {
        return 0;
    }

    // ── Watch mode ────────────────────────────────────────────────────────────

    // Install SIGTERM / SIGINT handler so the process exits cleanly.
    let shutdown = Arc::new(AtomicBool::new(false));
    {
        let s = Arc::clone(&shutdown);
        ctrlc::set_handler(move || {
            s.store(true, Ordering::SeqCst);
            SHUTDOWN.store(true, Ordering::SeqCst);
        }).ok();
    }

    let (tx, rx) = std::sync::mpsc::channel();

    let mut watcher = match notify::recommended_watcher(move |res| {
        if let Ok(event) = res {
            let _ = tx.send(event);
        }
    }) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("error: could not create file watcher: {e}");
            return 1;
        }
    };

    use notify::Watcher;
    if let Err(e) = watcher.watch(&cli.input_dir, notify::RecursiveMode::NonRecursive) {
        eprintln!("error: could not watch {}: {e}", cli.input_dir.display());
        return 1;
    }

    info!(dir = %cli.input_dir.display(), "watching for .md changes");

    loop {
        if SHUTDOWN.load(Ordering::SeqCst) {
            info!("shutting down");
            break;
        }

        // Block up to 200ms waiting for an event.
        let first = match rx.recv_timeout(std::time::Duration::from_millis(200)) {
            Ok(ev) => ev,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        };

        // Check if the first event is for a .md file.
        if !event_affects_md(&first) {
            // Drain and discard any further pending events.
            drain_channel(&rx);
            continue;
        }

        // Burst-drain: collect any additional events that arrive within 50ms
        // (handles editors that do delete+create / rename swap writes).
        std::thread::sleep(std::time::Duration::from_millis(50));
        drain_channel(&rx);

        if SHUTDOWN.load(Ordering::SeqCst) { break; }

        debug!("md change detected, regenerating");
        match process(&cli) {
            Ok(()) => {
                if let Some(ref touch_path) = cli.touch {
                    if let Err(e) = touch_file(touch_path) {
                        warn!(path = %touch_path.display(), error = %e, "touch failed");
                    } else {
                        debug!(path = %touch_path.display(), "touched");
                    }
                }
            }
            Err(e) => {
                warn!(error = %e, "regeneration failed, will retry on next change");
            }
        }
    }

    0
}

/// Return true if any path in the event has a `.md` extension.
fn event_affects_md(event: &notify::Event) -> bool {
    use notify::EventKind::*;
    matches!(event.kind, Create(_) | Modify(_) | Remove(_))
        && event.paths.iter().any(|p| {
            p.extension().and_then(|e| e.to_str()) == Some("md")
        })
}

/// Drain all pending events from the channel (non-blocking).
fn drain_channel(rx: &std::sync::mpsc::Receiver<notify::Event>) {
    while rx.try_recv().is_ok() {}
}

/// Touch a file: open for writing (creates if missing), updating its mtime.
fn touch_file(path: &Path) -> Result<()> {
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .map(|_| ())
        .with_context(|| format!("touching {}", path.display()))
}

fn process(cli: &Cli) -> Result<()> {
    // Validate input directory
    if !cli.input_dir.exists() {
        bail!("input directory does not exist: {}", cli.input_dir.display());
    }
    if !cli.input_dir.is_dir() {
        bail!("input path is not a directory: {}", cli.input_dir.display());
    }

    // Validate output parent directory
    let output_parent = cli.output.parent().unwrap_or(Path::new("."));
    if !output_parent.exists() {
        bail!("output directory does not exist: {}", output_parent.display());
    }

    // Collect *.md files (non-recursive, skip _ prefix)
    let mut paths: Vec<PathBuf> = std::fs::read_dir(&cli.input_dir)
        .with_context(|| format!("reading directory {}", cli.input_dir.display()))?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.is_file()
                && p.extension().and_then(|e| e.to_str()) == Some("md")
                && !p.file_name()
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
