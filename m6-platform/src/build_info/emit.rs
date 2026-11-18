// The build-script half of `super::build_info`.
//
// It is included by `m6-platform/build.rs` and by the module itself, so one
// implementation serves both callers. A crate that links m6-platform uses it
// as a build dependency and calls `emit` from its own `build.rs`.
//
// Cargo runs a build script once per crate, and `cargo:rustc-env` names a
// variable for that crate's compilation only. That is why every crate emits its
// own three constants. A binary that links two m6 crates built from different
// commits carries both answers, and `--version-full` shows the difference.

use std::process::{Command, Stdio};

/// Emit this crate's name, version, commit and dirty flag for its build.
///
/// `cargo:rustc-env` writes a variable into the environment rustc sees when it
/// compiles this crate. `build_identity!` reads them back with `env!`, so a
/// missing build script is a compile error rather than a wrong answer at run
/// time.
///
/// A missing git, a directory outside a work tree and a repository with no
/// commit all fall back to the commit `unknown` and a clean tree. A source
/// tarball has no history and must still build.
pub fn emit() {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default();
    let name = std::env::var("CARGO_PKG_NAME").unwrap_or_else(|_| "unknown".to_string());
    let version = std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.0.0".to_string());
    let (commit, dirty) = git_identity(&manifest_dir);

    println!("cargo:rustc-env=M6_BUILD_NAME={name}");
    println!("cargo:rustc-env=M6_BUILD_VERSION={version}");
    println!("cargo:rustc-env=M6_BUILD_COMMIT={commit}");
    println!("cargo:rustc-env=M6_BUILD_DIRTY={}", u8::from(dirty));

    // The commit and the index are what `rev-parse` and `status` read, so a new
    // commit or a staged change rebuilds the crate that carries the identity.
    // An unstaged change to a file in this package is caught by the directory
    // watch below. A change in another package is that package's own rebuild,
    // and the dirty flag describes the tree at this crate's build.
    if let Some(git_dir) = git(&manifest_dir, &["rev-parse", "--absolute-git-dir"]) {
        println!("cargo:rerun-if-changed={git_dir}/HEAD");
        println!("cargo:rerun-if-changed={git_dir}/index");
    }
    if !manifest_dir.is_empty() {
        println!("cargo:rerun-if-changed={manifest_dir}");
    }
}

/// What git says about the tree `dir` is in.
///
/// The short hash is twelve characters, the length `git rev-parse
/// --short=12` promises and long enough that two builds of this repository
/// cannot collide by accident.
///
/// Dirty means a tracked file is modified or a change is staged. An untracked
/// file does not count: cargo writes `.cargo-ok` into every git-dependency
/// checkout, so counting untracked files marked every build of a pinned
/// revision dirty.
fn git_identity(dir: &str) -> (String, bool) {
    let Some(commit) = git(dir, &["rev-parse", "--short=12", "HEAD"]).filter(|s| !s.is_empty())
    else {
        return ("unknown".to_string(), false);
    };
    let dirty = git(dir, &["status", "--porcelain", "--untracked-files=no"])
        .is_some_and(|s| !s.is_empty());
    (commit, dirty)
}

/// One git command, trimmed, or `None` when git is missing or the command fails.
fn git(dir: &str, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
}
