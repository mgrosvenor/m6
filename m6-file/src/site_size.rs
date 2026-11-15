//! What m6-file serves from disk, measured once at start.
//!
//! m6-file's first argument is the boundary of everything it can read, and each
//! route's `root` narrows that to one tree. The trees the routes reach are the
//! document roots, so this module walks those once, before the listener is
//! bound, and reports three things:
//!
//! - entries no document root should ever carry (`.git`, `target` and
//!   `node_modules`), which stop the start with exit 2,
//! - entries that are sometimes deliberate and sometimes accidental (`logs`,
//!   editor backups and `.DS_Store`), which are named in a warning and served,
//! - the total size of the regular files those roots reach, compared with
//!   `site_size_budget_mb`, which is a warning and not a refusal.
//!
//! The walk is start-only. Nothing here runs on a request.
//!
//! **The walk follows the routes, not the whole process root.** The examples
//! keep their renderers in the process root and a dev build writes
//! `render-cms/target` inside it. Scanning everything would refuse example 05
//! after its own dev script had built it. A route rooted at `assets/` cannot
//! reach that directory, and a route rooted at `logs/` is a deliberate log
//! viewer, not an accident. Both are clean here because they are what the
//! routes say to serve.

use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

use m6_core::config::RouteConfig;

/// The size budget when the config does not set `site_size_budget_mb`.
///
/// 128 MiB. The largest tree in m6's own examples is 5.1 MB, so the default
/// leaves two orders of magnitude of room for a legitimate asset tree while
/// still catching a `target/` or a log file that has grown for a year. A
/// deployment that really serves more raises the key. The number is a warning,
/// so a false trip costs one line per start, while a missing budget costs the
/// operator the one number they need.
pub const DEFAULT_BUDGET_MB: u64 = 128;

/// Names that must never sit under a document root.
///
/// `.git` is in every checkout and holds history, hooks and config. `target`
/// is Cargo's build output, produced by this workspace and by the private
/// site's renderers. `node_modules` is the same class for a JavaScript build.
/// m6's own examples produce the first two. No example produces a
/// `node_modules`, and a site that adds a JavaScript build will.
const REFUSED: &[&str] = &[".git", "target", "node_modules"];

/// One walk's findings.
#[derive(Debug, Default)]
pub struct Report {
    /// Entries that stop the start.
    pub refused: Vec<PathBuf>,
    /// Entries that are named in a warning and served.
    pub warned: Vec<PathBuf>,
    /// Bytes in the regular files the roots reach, symlinks not followed.
    pub bytes: u64,
    /// How many regular files those bytes are in.
    pub files: u64,
}

/// Resolve every route's `root` to the tree it can reach under `site_dir`.
///
/// A route with no `root` reaches the whole site directory, which is the
/// process root. A root with a `{param}` is cut at the first placeholder: the
/// tree up to that point is the most the route can reach without a request, so
/// it is the tree the start check inspects. An absolute root is skipped, and
/// the handler's `resolve_within` refuses it per request anyway. A root that
/// would climb out with `..` is skipped for the same reason.
pub fn served_roots(site_dir: &Path, routes: &[RouteConfig]) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    for route in routes {
        let root = route
            .settings
            .get("root")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if root.is_empty() {
            roots.push(site_dir.to_path_buf());
            continue;
        }
        if root.starts_with('/') {
            continue;
        }
        let literal = root.split('{').next().unwrap_or("").trim_end_matches('/');
        if Path::new(literal)
            .components()
            .any(|c| matches!(c, Component::ParentDir))
        {
            continue;
        }
        roots.push(if literal.is_empty() {
            site_dir.to_path_buf()
        } else {
            site_dir.join(literal)
        });
    }
    roots.sort();
    roots.dedup();
    // Drop a root another root already covers, so a tree is counted once and
    // a `logs` directory that is a route root is not a `logs` entry inside one.
    let mut kept: Vec<PathBuf> = Vec::with_capacity(roots.len());
    for root in roots {
        if !kept.iter().any(|k| root.starts_with(k)) {
            kept.push(root);
        }
    }
    kept
}

/// Walk every served root and report what it carries.
///
/// A missing root is skipped: a route may point at a tree an earlier start
/// created, and an empty route serves nothing. Any other read failure stops the
/// start, because the check cannot vouch for a tree it could not read.
///
/// Symlinks are skipped: a symlink's target may be outside the root and is
/// already refused at request time by `resolve_within`. A refused directory is
/// not descended into, so a 90 GB `target/` is named without being walked.
pub fn inspect(roots: &[PathBuf]) -> io::Result<Report> {
    let mut report = Report::default();
    for root in roots {
        walk(root, &mut report)?;
    }
    Ok(report)
}

fn walk(path: &Path, report: &mut Report) -> io::Result<()> {
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    if meta.file_type().is_symlink() {
        return Ok(());
    }
    if meta.is_file() {
        // A route whose root names one file makes that file served on purpose,
        // so its name is not a finding.
        report.bytes = report.bytes.saturating_add(meta.len());
        report.files = report.files.saturating_add(1);
        return Ok(());
    }
    if !meta.is_dir() {
        return Ok(());
    }
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let path = entry.path();
        let kind = entry.file_type()?;
        if kind.is_symlink() {
            continue;
        }
        if REFUSED.contains(&name.as_ref()) {
            report.refused.push(path);
            continue;
        }
        if kind.is_dir() {
            // A log directory is legitimate: example 08 is a log viewer and
            // serves a route rooted at `logs/`. It is warned because a
            // wildcard route over a wider root would also publish it.
            if name == "logs" {
                report.warned.push(path.clone());
            }
            walk(&path, report)?;
        } else if kind.is_file() {
            if is_discardable_file(&name) {
                report.warned.push(path.clone());
            }
            report.bytes = report
                .bytes
                .saturating_add(entry.metadata().map(|m| m.len()).unwrap_or(0));
            report.files = report.files.saturating_add(1);
        }
    }
    Ok(())
}

/// Editor backups and OS metadata. Each is a file the operator did not mean to
/// publish, and none is ever a served asset.
fn is_discardable_file(name: &str) -> bool {
    name.ends_with('~')
        || name.ends_with(".swp")
        || name.ends_with(".swo")
        || name.ends_with(".rs.bk")
        || name.starts_with(".#")
        || name == ".DS_Store"
}

/// The exit 2 line that names every refused entry and the remedy.
pub fn refusal(site_dir: &Path, refused: &[PathBuf]) -> String {
    let paths: Vec<String> = refused.iter().map(|p| p.display().to_string()).collect();
    format!(
        "m6-file: the document root {} carries {}: remove it, or start m6-file on a directory \
         that holds only the files it serves",
        site_dir.display(),
        paths.join(", ")
    )
}

/// The warning line that names every entry a wildcard route could serve.
pub fn warning(site_dir: &Path, warned: &[PathBuf]) -> String {
    let paths: Vec<String> = warned.iter().map(|p| p.display().to_string()).collect();
    format!(
        "m6-file: the document root {} carries {}: a wildcard route could serve it. Ignore this \
         if a route is rooted there on purpose, otherwise remove it",
        site_dir.display(),
        paths.join(", ")
    )
}

/// The warning line for roots over their budget. It names the total, the file
/// count, and the setting to change.
pub fn over_budget(site_dir: &Path, report: &Report, budget_mb: u64) -> String {
    let mib = report.bytes as f64 / (1024.0 * 1024.0);
    format!(
        "m6-file: the document root {} holds {mib:.1} MiB in {} files ({} bytes), over \
         site_size_budget_mb = {budget_mb}: raise site_size_budget_mb in the config, or remove \
         files that should not be deployed",
        site_dir.display(),
        report.files,
        report.bytes
    )
}

/// The budget in bytes. Saturation keeps an absurd configured value from
/// wrapping to a tiny budget.
pub fn budget_bytes(budget_mb: u64) -> u64 {
    budget_mb.saturating_mul(1024 * 1024)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn route(root: Option<&str>) -> RouteConfig {
        let mut settings = serde_json::Map::new();
        if let Some(root) = root {
            settings.insert("root".to_string(), serde_json::json!(root));
        }
        RouteConfig {
            path: "/x/{*relpath}".to_string(),
            template: None,
            params: Vec::new(),
            status: 200,
            cache: String::new(),
            methods: None,
            headers: Vec::new(),
            handler: Some("files".to_string()),
            settings: std::sync::Arc::new(settings),
        }
    }

    fn site() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("assets")).unwrap();
        std::fs::write(dir.path().join("assets/style.css"), b"body{}").unwrap();
        dir
    }

    fn root_of(dir: &Path, root: Option<&str>) -> Vec<PathBuf> {
        served_roots(dir, &[route(root)])
    }

    #[test]
    fn a_clean_root_has_no_findings_and_counts_its_files() {
        let dir = site();
        let roots = root_of(dir.path(), Some("assets/"));
        let report = inspect(&roots).unwrap();
        assert!(report.refused.is_empty(), "{:?}", report.refused);
        assert!(report.warned.is_empty(), "{:?}", report.warned);
        assert_eq!(report.files, 1);
        assert_eq!(report.bytes, 6);
    }

    #[test]
    fn git_target_and_node_modules_are_refused() {
        let dir = site();
        for name in [".git", "target", "node_modules"] {
            std::fs::create_dir_all(dir.path().join("assets").join(name)).unwrap();
        }
        let roots = root_of(dir.path(), Some("assets/"));
        let report = inspect(&roots).unwrap();
        assert_eq!(report.refused.len(), 3, "{:?}", report.refused);
        for name in [".git", "target", "node_modules"] {
            assert!(
                report.refused.iter().any(|p| p.ends_with(name)),
                "{name} not refused: {:?}",
                report.refused
            );
        }
    }

    #[test]
    fn a_git_file_is_refused_like_a_git_directory() {
        // A worktree's `.git` is a file, not a directory.
        let dir = site();
        std::fs::write(
            dir.path().join("assets/.git"),
            b"gitdir: ../.git/worktrees/x\n",
        )
        .unwrap();
        let roots = root_of(dir.path(), Some("assets/"));
        let report = inspect(&roots).unwrap();
        assert!(report.refused.iter().any(|p| p.ends_with(".git")));
    }

    #[test]
    fn a_logs_directory_inside_a_wider_root_is_warned_and_not_refused() {
        let dir = site();
        std::fs::create_dir_all(dir.path().join("logs")).unwrap();
        std::fs::write(dir.path().join("logs/m6-http.log"), b"line\n").unwrap();
        let roots = root_of(dir.path(), None);
        let report = inspect(&roots).unwrap();
        assert!(report.refused.is_empty(), "{:?}", report.refused);
        assert!(report.warned.iter().any(|p| p.ends_with("logs")));
        // The log's bytes are counted, so the budget sees what was deployed.
        assert_eq!(report.bytes, 6 + 5);
    }

    #[test]
    fn a_route_rooted_at_logs_is_served_without_a_warning() {
        // Example 08 serves a route rooted at logs/. The deliberate root is
        // the document root, and `logs` is its name, not an entry inside it.
        let dir = site();
        std::fs::create_dir_all(dir.path().join("logs")).unwrap();
        std::fs::write(dir.path().join("logs/app.log"), b"line\n").unwrap();
        let roots = root_of(dir.path(), Some("logs/"));
        let report = inspect(&roots).unwrap();
        assert!(report.refused.is_empty(), "{:?}", report.refused);
        assert!(report.warned.is_empty(), "{:?}", report.warned);
        assert_eq!(report.bytes, 5);
    }

    #[test]
    fn a_build_artifact_outside_the_route_roots_is_not_examined() {
        // The examples build their renderers inside the process root, which
        // writes render-cms/target there. A route rooted at assets/ cannot
        // serve it, so the start must not refuse the example.
        let dir = site();
        std::fs::create_dir_all(dir.path().join("render-cms/target/debug")).unwrap();
        std::fs::write(dir.path().join("render-cms/target/debug/app"), b"bin").unwrap();
        let roots = root_of(dir.path(), Some("assets/"));
        let report = inspect(&roots).unwrap();
        assert!(report.refused.is_empty(), "{:?}", report.refused);
        assert_eq!(report.bytes, 6);
    }

    #[test]
    fn an_editor_backup_is_warned() {
        let dir = site();
        std::fs::write(dir.path().join("assets/style.css~"), b"body{color:red}").unwrap();
        std::fs::write(dir.path().join("assets/.DS_Store"), b"junk").unwrap();
        let roots = root_of(dir.path(), Some("assets/"));
        let report = inspect(&roots).unwrap();
        assert!(report.refused.is_empty(), "{:?}", report.refused);
        assert_eq!(report.warned.len(), 2, "{:?}", report.warned);
    }

    #[test]
    fn a_refused_directory_is_not_walked() {
        // A target/ can hold tens of thousands of files. The refusal must not
        // pay for walking it.
        let dir = site();
        std::fs::create_dir_all(dir.path().join("assets/target/deep")).unwrap();
        std::fs::write(dir.path().join("assets/target/deep/big.bin"), b"0123456789").unwrap();
        let roots = root_of(dir.path(), Some("assets/"));
        let report = inspect(&roots).unwrap();
        assert_eq!(report.refused.len(), 1);
        assert_eq!(report.bytes, 6, "target/ must not be counted");
    }

    #[test]
    fn a_placeholder_root_is_cut_at_the_placeholder() {
        let dir = site();
        std::fs::create_dir_all(dir.path().join("content/posts/one")).unwrap();
        let roots = served_roots(dir.path(), &[route(Some("content/posts/{stem}/"))]);
        assert_eq!(roots, vec![dir.path().join("content/posts")]);
    }

    #[test]
    fn a_narrow_root_drops_the_whole_root_that_contains_it() {
        let dir = site();
        let roots = served_roots(
            dir.path(),
            &[route(Some("assets/")), route(None), route(Some("logs/"))],
        );
        assert_eq!(roots, vec![dir.path().to_path_buf()]);
    }

    #[test]
    fn an_absolute_root_is_skipped() {
        let dir = site();
        let roots = served_roots(dir.path(), &[route(Some("/var/lib/tokens"))]);
        assert!(roots.is_empty(), "{roots:?}");
    }

    #[test]
    fn a_root_that_climbs_out_is_skipped() {
        let dir = site();
        let roots = served_roots(dir.path(), &[route(Some("../outside"))]);
        assert!(roots.is_empty(), "{roots:?}");
    }

    #[test]
    fn the_refusal_names_the_entry_and_the_remedy() {
        let dir = site();
        std::fs::create_dir_all(dir.path().join("assets/.git")).unwrap();
        let roots = root_of(dir.path(), Some("assets/"));
        let report = inspect(&roots).unwrap();
        let line = refusal(dir.path(), &report.refused);
        assert!(line.contains(".git"), "{line}");
        assert!(line.contains(&dir.path().display().to_string()), "{line}");
        assert!(line.contains("holds only the files it serves"), "{line}");
    }

    #[test]
    fn the_budget_line_names_the_total_and_the_setting() {
        let dir = site();
        let roots = root_of(dir.path(), Some("assets/"));
        let report = inspect(&roots).unwrap();
        let line = over_budget(dir.path(), &report, 1);
        assert!(line.contains("site_size_budget_mb = 1"), "{line}");
        assert!(line.contains("0.0 MiB"), "{line}");
        assert!(line.contains("1 files"), "{line}");
        assert!(line.contains("6 bytes"), "{line}");
    }
}
