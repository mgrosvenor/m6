//! Locating the build output from inside a test.

use std::path::{Path, PathBuf};

/// The cargo target directory, derived from the running test binary.
///
/// A test binary lives at `<target>/<profile>/deps/<name>-<hash>`, so the
/// target directory is three levels up. Newer cargo puts it at
/// `<target>/<profile>/build/<package>/<hash>/out/<name>-<hash>`, so the
/// nearest ancestor named `deps` or `build` marks the profile directory's
/// child either way. Deriving it this way rather than from
/// `CARGO_MANIFEST_DIR` matters for two reasons: `CARGO_MANIFEST_DIR` is the
/// directory of whichever crate *expanded the macro*, which would be `m6-core`
/// and not the caller, and it is wrong outright when `CARGO_TARGET_DIR` points
/// the build somewhere else.
///
/// When `CARGO_BUILD_TARGET` names a triple, cargo puts the test binary at
/// `<target>/<triple>/<profile>/deps/<name>-<hash>`, and the triple directory
/// is stripped so the result is still the target directory.
///
/// Panics if the executable is not where cargo puts test binaries, because
/// every caller here would otherwise go on to produce a confusing failure
/// about a missing file.
#[cfg(test)]
pub(super) fn target_dir() -> PathBuf {
    let exe = std::env::current_exe().expect("current_exe");
    target_of(&exe, build_target().as_deref())
        .unwrap_or_else(|| panic!("cannot find target/ above {}", exe.display()))
}

/// The target directory above the test binary `exe`, for `target_dir`.
#[cfg(test)]
fn target_of(exe: &Path, triple: Option<&str>) -> Option<PathBuf> {
    let above_profile = exe
        .ancestors()
        .skip(1)
        .find(|a| {
            matches!(
                a.file_name().and_then(|s| s.to_str()),
                Some("deps" | "build")
            )
        })
        .and_then(Path::parent)
        .and_then(Path::parent)?;
    strip_triple(above_profile, triple).map(Path::to_path_buf)
}

/// The `CARGO_BUILD_TARGET` triple, if one is set and not empty.
#[cfg(test)]
fn build_target() -> Option<String> {
    std::env::var("CARGO_BUILD_TARGET")
        .ok()
        .filter(|t| !t.is_empty())
}

/// `dir` with a final `<triple>` component removed, when `triple` is set and
/// is that component. Returns `None` only when the triple matches and there is
/// no parent to return.
#[cfg(test)]
fn strip_triple<'a>(dir: &'a Path, triple: Option<&str>) -> Option<&'a Path> {
    match triple {
        Some(t) if dir.file_name().and_then(|s| s.to_str()) == Some(t) => dir.parent(),
        _ => Some(dir),
    }
}

/// The profile directory holding the running test binary:
/// `<target>/<profile>` or `<target>/<triple>/<profile>`.
///
/// Cargo puts a test executable at `<profile>/deps/<name>-<hash>`, so the
/// nearest ancestor named `deps` or `build` is one level below the profile
/// directory. Deriving it from the executable rather than from
/// `CARGO_TARGET_DIR` also works when the target directory was overridden on
/// the command line.
fn profile_dir() -> PathBuf {
    let exe = std::env::current_exe().expect("current_exe");
    exe.ancestors()
        .skip(1)
        .find(|a| {
            matches!(
                a.file_name().and_then(|s| s.to_str()),
                Some("deps" | "build")
            )
        })
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .unwrap_or_else(|| panic!("cannot find the profile directory above {}", exe.display()))
}

/// Path to a workspace binary cargo built for this test run.
///
/// **Never runs cargo.** A test that shells out to `cargo build --release`
/// makes the whole suite pay a release build with fat LTO, which was 612 s of a
/// 28-minute `cargo test --workspace`. The binaries are already there:
///
/// - A binary of the test's own package is named by Cargo's contract,
///   `CARGO_BIN_EXE_<name>`.
/// - A binary of another workspace package sits next to the test executable in
///   the profile directory, because `cargo build --workspace --bins` (and
///   `cargo test`) builds every bin in the workspace.
///
/// The name is a binary target name. Hyphens are kept, as Cargo keeps them in
/// `CARGO_BIN_EXE_<name>`.
///
/// Panics, naming the command to run, when the binary is absent. That is the
/// case for a package-only test run such as `cargo test -p m6-core`: the gate
/// builds `--workspace --bins` first, so every binary it needs is present.
pub fn binary(name: &str) -> PathBuf {
    if let Some(p) = std::env::var_os(format!("CARGO_BIN_EXE_{name}")) {
        return PathBuf::from(p);
    }
    let path = profile_dir().join(name);
    if path.is_file() {
        return path;
    }
    panic!(
        "no {name} binary at {}. The test harness does not run cargo. Run \
         `cargo build --workspace --bins` first, or run the test with the whole \
         workspace (`cargo test --workspace`), which builds every binary.",
        path.display()
    );
}

/// A scratch directory a test can COMPILE INTO AND THEN EXECUTE FROM:
/// [`crate::platform::scratch_dir`] under the name the suites use. `/tmp` is
/// not guaranteed to permit exec and a hardened host mounts it `noexec`, which
/// on 2026-09-22 failed fourteen tests across two files with "Permission
/// denied" messages that named the example rather than the mount.
///
/// Not for unix sockets: a path under here is longer than one under
/// [`crate::platform::temp_dir`], and the two requirements pull in opposite
/// directions.
pub fn exec_scratch_root() -> PathBuf {
    crate::platform::scratch_dir()
}

// No `exec_tempdir()` helper here on purpose. `tempfile` is a dev-dependency of
// this crate while `testkit` is a real feature-gated module, so returning a
// `TempDir` would pull a new runtime dependency into m6-core to save callers
// one line. What generalises is knowing WHERE exec is permitted; the temp-dir
// plumbing does not. Callers write:
//
//     tempfile::Builder::new()
//         .tempdir_in(m6_core::testkit::exec_scratch_root())

#[cfg(test)]
mod tests {
    use super::*;

    /// The profile directory is the one the running test executable is in.
    ///
    /// This is what `binary()` joins a cross-package binary name onto, so it
    /// has to be `<target>/<profile>` and not `<target>` or a `deps` directory.
    #[test]
    fn the_profile_directory_is_the_one_holding_the_test_binary() {
        let dir = profile_dir();
        let exe = std::env::current_exe().unwrap();
        let deps = exe.parent().expect("the test executable has a directory");
        assert_eq!(
            deps.file_name().and_then(|s| s.to_str()),
            Some("deps"),
            "the test executable is at {}",
            exe.display()
        );
        assert_eq!(dir, deps.parent().expect("deps has a parent"));
        assert!(
            dir.join("deps").is_dir(),
            "{} is not a profile directory",
            dir.display()
        );
    }

    #[test]
    fn the_target_directory_is_found_in_both_cargo_layouts() {
        let old = Path::new("/t/debug/deps/x-1");
        let new = Path::new("/t/debug/build/pkg/abc/out/x-1");
        let triple_new = Path::new("/t/x86_64-unknown-linux-gnu/debug/build/pkg/abc/out/x-1");
        assert_eq!(target_of(old, None), Some(PathBuf::from("/t")));
        assert_eq!(target_of(new, None), Some(PathBuf::from("/t")));
        assert_eq!(
            target_of(triple_new, Some("x86_64-unknown-linux-gnu")),
            Some(PathBuf::from("/t"))
        );
        assert_eq!(target_of(Path::new("/x"), None), None);
    }

    #[test]
    fn the_triple_directory_is_stripped_from_the_target_directory() {
        let d = Path::new("/t/x86_64-unknown-linux-gnu");
        assert_eq!(strip_triple(d, None), Some(d));
        assert_eq!(strip_triple(d, Some("other")), Some(d));
        assert_eq!(
            strip_triple(d, Some("x86_64-unknown-linux-gnu")),
            Some(Path::new("/t"))
        );
    }
}
