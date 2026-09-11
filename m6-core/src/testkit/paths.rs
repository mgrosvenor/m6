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
pub(super) fn target_dir() -> PathBuf {
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
fn test_profile() -> String {
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
/// **Release first, always, whatever profile the test itself was built with.**
/// `check.sh` builds the workspace with `--release` and then runs `cargo test`
/// without it, so the test binary is a debug build while the services it is
/// meant to exercise are release builds. `cargo test` also builds each
/// package's `bin` targets in debug as a side effect, so a `target/debug/m6-file`
/// exists whether or not anyone wanted one.
///
/// Preferring the test's own profile therefore silently swapped every spawned
/// service for its debug build. On a fast laptop that only made the suite
/// slower; on the Linux build box a debug `m6-file` started too slowly for
/// `m6-http`'s backend rescan window and five `analytics_e2e` tests failed with
/// 502. Debug binaries are also not the artefact being validated.
///
/// Debug remains the fallback for `cargo test` run on its own with no release
/// build present.
///
/// # The trap that comes with it
///
/// **A stale `target/release/<name>` is preferred over a fresh source tree, and
/// nothing says so.** `cargo test` rebuilds the lib and the test binary from
/// current source, then spawns a service binary that may be hours old, so a fix
/// that is present in the code under test is absent from the process being
/// tested. That reads as the fix not working: on 2026-09-12 a correct read
/// timeout was measured as broken twice, and the same mechanism will just as
/// happily report a broken change as fixed.
///
/// There is no reliable mtime check to add here. In the intended workflow
/// (`cargo build --release` then `cargo test`) cargo relinks the test binary
/// *after* the release binaries, so "release is older than the test binary" is
/// the normal case and not a staleness signal.
///
/// So the rule is procedural: **`cargo build --workspace --release` before
/// `cargo test`**, every time, and when an end-to-end test contradicts what the
/// source plainly says, check the binary's timestamp before debugging the code.
/// `deploy/run-tests.sh` does the release build itself and is not exposed.
///
/// Panics with both paths and the build command to run, because "missing
/// binary" is the single most common first-run failure and a bare `NotFound`
/// says nothing about the fix.
pub fn binary(name: &str) -> PathBuf {
    let target = target_dir();

    let release = target.join("release").join(name);
    if release.exists() {
        return release;
    }
    let fallback = target.join(test_profile()).join(name);
    if fallback.exists() {
        return fallback;
    }
    panic!(
        "missing binary {name:?}\n  looked in: {}\n  looked in: {}\n  \
         build it first: cargo build --workspace --release",
        release.display(),
        fallback.display(),
    );
}
