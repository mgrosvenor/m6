//! Locating the build output from inside a test.

use std::path::{Path, PathBuf};

/// The cargo target directory, derived from the running test binary.
///
/// A test binary lives at `<target>/<profile>/deps/<name>-<hash>`, so the
/// target directory is three levels up. Deriving it this way rather than from
/// `CARGO_MANIFEST_DIR` matters for two reasons: `CARGO_MANIFEST_DIR` is the
/// directory of whichever crate *expanded the macro*, which would be `m6-core`
/// and not the caller, and it is wrong outright when `CARGO_TARGET_DIR` points
/// the build somewhere else.
///
/// Panics if the executable is not where cargo puts test binaries, because
/// every caller here would otherwise go on to produce a confusing failure
/// about a missing file.
pub fn target_dir() -> PathBuf {
    let exe = std::env::current_exe().expect("current_exe");
    let deps = exe.parent().expect("test binary has no parent directory");
    debug_assert_eq!(deps.file_name().and_then(|s| s.to_str()), Some("deps"));
    deps.parent()
        .and_then(Path::parent)
        .unwrap_or_else(|| panic!("cannot find target/ above {}", exe.display()))
        .to_path_buf()
}

/// The profile the running test was built with: `debug`, `release`, or a
/// custom profile name.
pub fn test_profile() -> String {
    let exe = std::env::current_exe().expect("current_exe");
    exe.parent()
        .and_then(Path::parent)
        .and_then(|p| p.file_name())
        .and_then(|s| s.to_str())
        .unwrap_or("debug")
        .to_string()
}

/// Path to a built workspace binary, for tests that spawn real services.
///
/// **Why this searches two profiles.** `check.sh` builds the workspace with
/// `--release` and then runs `cargo test` without it, so the test binary is a
/// debug build while the services it spawns are release builds. Looking only
/// in the test's own profile would fail in the standard CI path; looking only
/// in `release` would fail for anyone running `cargo test` alone. So: the
/// test's own profile first, then the other one.
///
/// Panics with both paths and the build command to run, because "missing
/// binary" is the single most common first-run failure and a bare
/// `NotFound` says nothing about the fix.
pub fn binary(name: &str) -> PathBuf {
    let target = target_dir();
    let own = test_profile();
    let other = if own == "release" { "debug" } else { "release" };

    let first = target.join(&own).join(name);
    if first.exists() {
        return first;
    }
    let second = target.join(other).join(name);
    if second.exists() {
        return second;
    }
    panic!(
        "missing binary {name:?}\n  looked in: {}\n  looked in: {}\n  \
         build it first: cargo build --workspace --release",
        first.display(),
        second.display(),
    );
}
