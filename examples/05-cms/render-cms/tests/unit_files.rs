//! Every example unit that declares `RuntimeDirectory=m6` must also set
//! `RuntimeDirectoryPreserve=yes`.
//!
//! The units share `/run/m6`. Without `RuntimeDirectoryPreserve=yes`, systemd
//! deletes that directory when any one unit stops or restarts, which removes
//! every other service's socket. This test scans the example unit files and the
//! ```ini blocks in the guide posts, so a reader copies a unit that works.

use std::fs;
use std::path::{Path, PathBuf};

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .and_then(Path::parent)
        .expect("render-cms lives three levels under the examples root")
        .to_path_buf()
}

/// A unit declares `RuntimeDirectory=` but not `RuntimeDirectoryPreserve=yes`.
fn lacks_preserve(unit: &str) -> bool {
    let has_runtime_dir = unit.lines().any(|line| {
        let line = line.trim();
        line.starts_with("RuntimeDirectory=") && line.len() > "RuntimeDirectory=".len()
    });
    let has_preserve = unit
        .lines()
        .any(|line| line.trim() == "RuntimeDirectoryPreserve=yes");
    has_runtime_dir && !has_preserve
}

/// The text of each ```ini fenced block in a markdown file.
fn ini_blocks(text: &str) -> Vec<String> {
    let mut blocks = Vec::new();
    let mut in_block = false;
    let mut current = String::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if !in_block {
            if trimmed == "```ini" {
                in_block = true;
                current = String::new();
            }
        } else if trimmed.starts_with("```") {
            if !current.trim().is_empty() {
                blocks.push(current.clone());
            }
            in_block = false;
        } else {
            current.push_str(line);
            current.push('\n');
        }
    }
    blocks
}

/// Collect markdown files and unit files under `root`, skipping build output.
fn walk(root: &Path, md_files: &mut Vec<PathBuf>, unit_files: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if name == "target" || name == ".git" {
                continue;
            }
            walk(&path, md_files, unit_files);
        } else if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            match ext {
                "md" => md_files.push(path),
                "service" | "socket" | "mount" => unit_files.push(path),
                _ => {}
            }
        }
    }
}

#[test]
fn a_unit_declaring_runtime_directory_without_preserve_fails() {
    assert!(lacks_preserve(
        "RuntimeDirectory=m6\nRuntimeDirectoryMode=0750\n"
    ));
}

#[test]
fn a_unit_declaring_runtime_directory_with_preserve_passes() {
    assert!(!lacks_preserve(
        "RuntimeDirectory=m6\nRuntimeDirectoryMode=0750\nRuntimeDirectoryPreserve=yes\n"
    ));
}

#[test]
fn example_units_preserve_the_shared_runtime_directory() {
    let root = workspace_root();
    let mut md_files = Vec::new();
    let mut unit_files = Vec::new();
    walk(&root, &mut md_files, &mut unit_files);

    let mut offenders = Vec::new();
    for path in &md_files {
        let text = fs::read_to_string(path).expect("readable markdown");
        for block in ini_blocks(&text) {
            if lacks_preserve(&block) {
                offenders.push(format!("{} (ini block)", path.display()));
            }
        }
    }
    for path in &unit_files {
        let text = fs::read_to_string(path).expect("readable unit file");
        if lacks_preserve(&text) {
            offenders.push(path.display().to_string());
        }
    }

    assert!(
        offenders.is_empty(),
        "units lacking RuntimeDirectoryPreserve=yes:\n{}",
        offenders.join("\n")
    );
}
