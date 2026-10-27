//! Locating the build output from inside a test.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};

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

/// True for the variables `cargo test` adds to a test process's environment.
///
/// A nested cargo that inherits them treats the workspace as a different build
/// from the one `cargo build --workspace --release` made, so the two rebuild
/// each other's artefacts and the "no-op" check costs a full release build
/// each time.
fn is_test_run_variable(name: &str) -> bool {
    name.starts_with("CARGO_PKG_")
        || name.starts_with("CARGO_BIN_EXE_")
        || matches!(
            name,
            "CARGO_MANIFEST_DIR"
                | "CARGO_MANIFEST_PATH"
                | "CARGO_CRATE_NAME"
                | "CARGO_BIN_NAME"
                | "CARGO_PRIMARY_PACKAGE"
                | "CARGO_TARGET_TMPDIR"
        )
}

/// Builds `name` in the release profile into `target` and returns its path.
///
/// Runs from `manifest_dir` so cargo finds the workspace of the test crate.
/// The inputs are parameters so a unit test can point it at a scratch
/// workspace.
fn build_release(
    cargo: &OsStr,
    manifest_dir: &Path,
    target: &Path,
    name: &str,
) -> Result<PathBuf, String> {
    let mut cmd = Command::new(cargo);
    cmd.args([
        "build",
        "--quiet",
        "--release",
        "--workspace",
        "--bin",
        name,
    ])
    .arg("--target-dir")
    .arg(target)
    .current_dir(manifest_dir)
    .stdin(Stdio::null());
    for (var, _) in std::env::vars_os() {
        if var.to_str().is_some_and(is_test_run_variable) {
            cmd.env_remove(var);
        }
    }
    let status = cmd
        .status()
        .map_err(|e| format!("cannot run {cargo:?} to build {name:?}: {e}"))?;
    if !status.success() {
        return Err(format!(
            "cargo build --release --workspace --bin {name} failed ({status}); its errors are printed above"
        ));
    }
    let path = target.join("release").join(name);
    if !path.is_file() {
        return Err(format!(
            "cargo built {name:?} but {} does not exist",
            path.display()
        ));
    }
    Ok(path)
}

/// Path to a built workspace binary, for tests that spawn real services.
///
/// **Release, always, whatever profile the test itself was built with.**
/// `check.sh` builds the workspace with `--release` and then runs `cargo test`
/// without it, so the test binary is a debug build while the services it is
/// meant to exercise are release builds. On the Linux build box a debug
/// `m6-file` started too slowly for `m6-http`'s backend rescan window and five
/// `analytics_e2e` tests failed with 502. Debug binaries are also not the
/// artefact being validated.
///
/// **The binary is built before its path is returned.** The first call for a
/// name in a test process runs
/// `cargo build --quiet --release --workspace --bin <name>` from the test
/// crate's directory and lets cargo's own fingerprints decide whether anything
/// needs compiling. A stale release binary is rebuilt before it is spawned, so
/// a plain `cargo test` after an edit runs the edited code. Later calls for the
/// same name return the same path without running cargo again.
///
/// Two costs follow. The first call after a change pays the release build,
/// which uses fat LTO and takes minutes for `m6-http`. A build that is current
/// costs one cargo call that does nothing. `--workspace` keeps feature
/// resolution the same as `cargo build --workspace --release`, so that build
/// and this one produce the same binary.
///
/// The name must be a binary of the test crate's own workspace. A name from a
/// dependency fetched from git fails with cargo's "no bin target named" error.
///
/// Panics if the build fails, with the cargo errors printed above the panic
/// message.
pub fn binary(name: &str) -> PathBuf {
    static BUILT: OnceLock<Mutex<HashMap<String, Result<PathBuf, String>>>> = OnceLock::new();
    let mut built = BUILT
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let result = built.entry(name.to_string()).or_insert_with(|| {
        let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
        let manifest_dir = std::env::var_os("CARGO_MANIFEST_DIR").unwrap_or_else(|| {
            panic!(
                "m6_core::testkit::binary: CARGO_MANIFEST_DIR is not set; run the test with cargo test"
            )
        });
        build_release(&cargo, Path::new(&manifest_dir), &target_dir(), name)
    });
    match result {
        Ok(p) => p.clone(),
        Err(e) => panic!("{e}"),
    }
}

/// A scratch directory a test can COMPILE INTO AND THEN EXECUTE FROM.
///
/// `std::env::temp_dir()` and `tempfile::tempdir()` both land under `/tmp`, and
/// `/tmp` is not guaranteed to permit exec. On this estate it provably does
/// not: the hardened baseline mounts it `noexec` (site issue #98), and on
/// 2026-09-22 that failed fourteen tests across two files with
///
/// ```text
/// cannot spawn c example: Permission denied (os error 13)
/// c: spawn: Permission denied (os error 13)
/// ```
///
/// Both messages name the example rather than the mount, so the first reading
/// is that the backend examples are broken. They were not; the filesystem
/// changed underneath them when the box was hardened.
///
/// Here rather than in either test file because two of them need it, which is
/// the point at which a third copy becomes inevitable. Standing rule 11.
///
/// ── What this is NOT for ─────────────────────────────────────────────────
///
/// Unix sockets. `sockaddr_un::sun_path` caps at 104 bytes on macOS and 108 on
/// Linux, and a socket under here would be longer than one under `/tmp`. The
/// two requirements pull in opposite directions and must not be served by one
/// directory: short paths for sockets, exec-permitting paths for binaries.
///
/// `TMPDIR` is deliberately ignored. On macOS it is a long
/// `/var/folders/...` path, and the caller that wants short is asking for
/// something else anyway.
pub fn exec_scratch_root() -> PathBuf {
    // /var/tmp: disk-backed, and not among the mounts the hardening restricts.
    // Measured on the hardened build host rather than assumed, because
    // `findmnt` returned nothing at all for these paths there and a check that
    // cannot see is a check that lies: /tmp EXEC BLOCKED, /var/tmp EXEC OK.
    let var_tmp = PathBuf::from("/var/tmp");
    if var_tmp.is_dir() {
        return var_tmp;
    }
    std::env::temp_dir()
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

    /// A one-package workspace named `j18probe` whose binary prints `body`.
    fn probe(dir: &Path, main_rs: &str) {
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(
            dir.join("Cargo.toml"),
            "[workspace]\n\n[package]\nname = \"j18probe\"\nversion = \"0.0.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        std::fs::write(dir.join("src/main.rs"), main_rs).unwrap();
    }

    fn run(path: &Path) -> String {
        let out = Command::new(path).output().unwrap();
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    }

    #[test]
    fn build_release_rebuilds_after_the_source_changes() {
        let Some(cargo) = std::env::var_os("CARGO") else {
            eprintln!("CARGO is not set; skipping");
            return;
        };
        let dir = tempfile::Builder::new()
            .tempdir_in(exec_scratch_root())
            .unwrap();
        let target = dir.path().join("target");
        probe(dir.path(), "fn main() { println!(\"one\"); }");
        let first = build_release(&cargo, dir.path(), &target, "j18probe").unwrap();
        assert_eq!(run(&first), "one");

        std::fs::write(
            dir.path().join("src/main.rs"),
            "fn main() { println!(\"two\"); }",
        )
        .unwrap();
        let second = build_release(&cargo, dir.path(), &target, "j18probe").unwrap();
        assert_eq!(run(&second), "two");
    }

    #[test]
    fn build_release_names_the_failing_command() {
        let Some(cargo) = std::env::var_os("CARGO") else {
            eprintln!("CARGO is not set; skipping");
            return;
        };
        let dir = tempfile::Builder::new()
            .tempdir_in(exec_scratch_root())
            .unwrap();
        probe(dir.path(), "fn main( {");
        let err =
            build_release(&cargo, dir.path(), &dir.path().join("target"), "j18probe").unwrap_err();
        assert!(
            err.contains("cargo build --release --workspace --bin j18probe failed"),
            "{err}"
        );
    }

    #[test]
    fn build_release_refuses_an_unknown_binary() {
        let Some(cargo) = std::env::var_os("CARGO") else {
            eprintln!("CARGO is not set; skipping");
            return;
        };
        let dir = tempfile::Builder::new()
            .tempdir_in(exec_scratch_root())
            .unwrap();
        probe(dir.path(), "fn main() {}");
        let err =
            build_release(&cargo, dir.path(), &dir.path().join("target"), "nosuch").unwrap_err();
        assert!(err.contains("nosuch"), "{err}");
    }

    #[test]
    fn only_the_variables_cargo_test_adds_are_dropped() {
        for name in [
            "CARGO_PKG_NAME",
            "CARGO_PKG_VERSION",
            "CARGO_BIN_EXE_m6-file",
            "CARGO_MANIFEST_DIR",
            "CARGO_MANIFEST_PATH",
            "CARGO_CRATE_NAME",
            "CARGO_BIN_NAME",
            "CARGO_PRIMARY_PACKAGE",
            "CARGO_TARGET_TMPDIR",
        ] {
            assert!(is_test_run_variable(name), "{name}");
        }
        for name in [
            "CARGO",
            "CARGO_HOME",
            "CARGO_TARGET_DIR",
            "CARGO_NET_OFFLINE",
            "CARGO_REGISTRIES_X_TOKEN",
            "PATH",
            "HOME",
        ] {
            assert!(!is_test_run_variable(name), "{name}");
        }
    }
}
