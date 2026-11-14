//! The gate that keeps the operating system inside the platform layer.
//!
//! Every `.rs` file and every `Cargo.toml` in the workspace is read, tests and
//! benches included. Outside `m6-platform/src/`, no file may name a platform in a `cfg`, name one of
//! the four OS crates, read the clock directly in shipping code, or read a
//! service manager's variable. The two OS files must
//! define the same items, so a run on one OS sees an arm the other lacks.
//! `OS-1-TESTS.md` section 9 names the four rules.

use std::path::{Path, PathBuf};

/// The workspace root: the parent of m6-platform.
fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("m6-platform sits in the workspace")
        .to_path_buf()
}

/// Inside the layer: the one place the OS is named. This file is excluded as
/// well, because it spells the words it refuses.
fn in_layer(rel: &Path) -> bool {
    rel.starts_with("m6-platform/src") || rel == Path::new("m6-platform/tests/platform_gate.rs")
}

/// Every file under the root with the given name or extension, skipping the
/// build output and git, as paths relative to the root.
fn files(root: &Path, want: &dyn Fn(&Path) -> bool) -> Vec<PathBuf> {
    fn walk(dir: &Path, want: &dyn Fn(&Path) -> bool, out: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).expect("read_dir").flatten() {
            let path = entry.path();
            let name = entry.file_name();
            if path.is_dir() {
                if name != "target" && name != ".git" {
                    walk(&path, want, out);
                }
            } else if want(&path) {
                out.push(path);
            }
        }
    }
    let mut out = Vec::new();
    walk(root, want, &mut out);
    out.sort();
    out.into_iter()
        .map(|p| p.strip_prefix(root).expect("under the root").to_path_buf())
        .collect()
}

fn rust_files(root: &Path) -> Vec<PathBuf> {
    files(root, &|p| p.extension().is_some_and(|e| e == "rs"))
}

/// Lines of `rel` that `offends`, as "path:line: text".
fn offending_lines(root: &Path, rel: &Path, offends: &dyn Fn(&str) -> bool) -> Vec<String> {
    let text = std::fs::read_to_string(root.join(rel)).expect("read the file");
    text.lines()
        .enumerate()
        .filter(|(_, line)| offends(line))
        .map(|(i, line)| format!("{}:{}: {}", rel.display(), i + 1, line.trim()))
        .collect()
}

fn assert_none(rule: &str, found: Vec<String>) {
    assert!(
        found.is_empty(),
        "{rule}: {} offending line(s), each belongs in m6-platform/src/:\n{}",
        found.len(),
        found.join("\n")
    );
}

/// G1. No `cfg` predicate names a platform outside the layer. Break: one
/// `#[cfg(target_os = "linux")]` added to a test file outside the layer.
#[test]
fn no_platform_conditional_outside_the_layer() {
    const WORDS: [&str; 8] = [
        "target_os",
        "target_family",
        "target_vendor",
        "target_arch",
        "target_pointer_width",
        "target_endian",
        "cfg(unix",
        "cfg(windows",
    ];
    let root = root();
    let mut found = Vec::new();
    for rel in rust_files(&root) {
        if in_layer(&rel) {
            continue;
        }
        found.extend(offending_lines(&root, &rel, &|line| {
            let compact: String = line.chars().filter(|c| !c.is_whitespace()).collect();
            WORDS.iter().any(|w| compact.contains(w))
        }));
    }
    assert_none("G1 (platform conditionals)", found);
}

/// G2. No OS crate is named outside the layer, in code or in a manifest.
/// Break: one `use libc::` in a test, or `nix` added to a service's
/// `[dependencies]`.
#[test]
fn no_platform_crate_outside_the_layer() {
    const CRATES: [&str; 4] = ["mio", "nix", "rustix", "libc"];
    let root = root();
    let mut found = Vec::new();
    for rel in rust_files(&root) {
        if in_layer(&rel) {
            continue;
        }
        found.extend(offending_lines(&root, &rel, &|line| {
            CRATES.iter().any(|c| names_crate(line, c))
        }));
    }
    for rel in files(&root, &|p| p.file_name().is_some_and(|n| n == "Cargo.toml")) {
        if rel == Path::new("m6-platform/Cargo.toml") {
            continue;
        }
        found.extend(offending_lines(&root, &rel, &|line| {
            let key = line.split('=').next().unwrap_or("").trim();
            CRATES.contains(&key)
        }));
    }
    assert_none("G2 (OS crates)", found);
}

/// G3. No direct clock read outside the layer in shipping code: a loop reads
/// the clock once per wake through `platform::now`, and the wall clock through
/// `platform::wall`. A test or a bench may time itself. Break: one
/// `Instant::now()` in a service's `src/`.
#[test]
fn no_direct_clock_read_outside_the_layer() {
    let root = root();
    let mut found = Vec::new();
    for rel in rust_files(&root) {
        let under = |dir: &str| rel.components().any(|c| c.as_os_str() == dir);
        if in_layer(&rel) || under("tests") || under("benches") || under("examples") {
            continue;
        }
        found.extend(offending_lines(&root, &rel, &|line| {
            line.contains("Instant::now()") || line.contains("SystemTime::now()")
        }));
    }
    assert_none("G3 (direct clock reads)", found);
}

/// G5. No read of a systemd-only variable anywhere outside the layer: m6
/// never talks to a service manager and never relies on one to create
/// anything, so the same binary runs under launchd. Break: one
/// `std::env::var("NOTIFY_SOCKET")` in a service.
#[test]
fn no_service_manager_variable_outside_the_layer() {
    const VARIABLES: [&str; 8] = [
        "CREDENTIALS_DIRECTORY",
        "STATE_DIRECTORY",
        "RUNTIME_DIRECTORY",
        "NOTIFY_SOCKET",
        "LISTEN_FDS",
        "WATCHDOG_USEC",
        "INVOCATION_ID",
        "JOURNAL_STREAM",
    ];
    let root = root();
    let mut found = Vec::new();
    for rel in rust_files(&root) {
        if in_layer(&rel) {
            continue;
        }
        found.extend(offending_lines(&root, &rel, &|line| {
            VARIABLES.iter().any(|v| line.contains(v))
        }));
    }
    assert_none("G5 (service manager variables)", found);
}

/// Whether `line` names the crate `c` as a path (`c::`), a `use` or an
/// `extern crate`. A path check at a word boundary, so `std::os::unix::` does
/// not read as `nix::`.
fn names_crate(line: &str, c: &str) -> bool {
    let path = format!("{c}::");
    let as_path = line.match_indices(&path).any(|(i, _)| {
        !line[..i]
            .chars()
            .next_back()
            .is_some_and(|ch| ch.is_alphanumeric() || ch == '_' || ch == ':')
    });
    let t = line.trim_start();
    as_path
        || t.starts_with(&format!("use {c};"))
        || t.starts_with(&format!("use {c} "))
        || t.starts_with(&format!("extern crate {c}"))
}

/// G4. The two arms define the same items, so a macOS-only run sees a
/// missing Linux arm and the reverse. Break: a `pub(super) fn` added to one
/// arm only.
#[test]
fn the_two_arms_define_the_same_items() {
    fn items(root: &Path, rel: &str) -> Vec<String> {
        let text = std::fs::read_to_string(root.join(rel)).expect("read the arm");
        let mut out: Vec<String> = text
            .lines()
            .filter_map(|l| {
                let l = l.trim_start().strip_prefix("pub(super) ")?;
                let rest = l
                    .strip_prefix("fn ")
                    .or_else(|| l.strip_prefix("struct "))
                    .or_else(|| l.strip_prefix("const "))?;
                let name: String = rest
                    .chars()
                    .take_while(|c| c.is_alphanumeric() || *c == '_')
                    .collect();
                Some(name)
            })
            .collect();
        out.sort();
        out
    }
    let root = root();
    let linux = items(&root, "m6-platform/src/linux.rs");
    let macos = items(&root, "m6-platform/src/macos.rs");
    assert!(!linux.is_empty() && !macos.is_empty());
    assert_eq!(
        linux, macos,
        "the Linux and macOS arms must define the same items"
    );
}
