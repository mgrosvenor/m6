//! Every installed binary can say which version it is.
//!
//! ## Why this is a test and not a convenience
//!
//! It is the only thing that closes the config-and-binary ordering hazard.
//!
//! m6-file's config format changed when it became an `App` service: routes have
//! to name a handler, and `{relpath}` had to become `{*relpath}` because a bare
//! trailing parameter is no longer greedy. Configs and binaries ship in two
//! separate deploy runs, so there is a window in either order, and **both
//! orderings break**:
//!
//! - **new config, old binary**: the old matcher reads `{*relpath}` as a
//!   single-segment parameter, so every nested asset 404s
//! - **old config, new binary**: the routes name no handler, so m6-file exits 2
//!   before binding
//!
//! The second is caught, because `--dump-config` validates before installing. The
//! first is not, and it cannot be caught from the config side: **unknown keys are
//! ignored rather than refused** (verified: a route carrying
//! `min_m6_version = "9.9.9"` loads with exit 0), so a version floor written into
//! a config is invisible to exactly the old binary it is meant to stop.
//!
//! That leaves asking the binary. A deploy that installs both together and then
//! asserts what is actually on the node needs every binary to answer, so a binary
//! that silently lacks the flag would put a hole back in the deploy. Hence a test
//! over all seven rather than a note in a document.
//!
//! Four of these have their own argument parsing rather than m6-core's
//! `parse_invocation`, which is why this covers each one by name: m6-http,
//! m6-md and m6-auth-cli each needed the flag adding separately, and a new binary
//! will too.

use std::process::Command;

use m6_core::testkit::binary;

/// Every binary a deploy installs.
///
/// m6-auth-cli is in the list deliberately: a deploy ships it alongside the
/// services, and a node with a stale CLI is as much a mismatch as a stale server.
const BINARIES: &[&str] = &[
    "m6-http",
    "m6-file",
    "m6-html",
    "m6-md",
    "m6-auth-server",
    "m6-auth-cli",
    "m6-monitor",
];

#[test]
fn every_binary_reports_its_own_name_and_version() {
    let expected_version = env!("CARGO_PKG_VERSION");
    let mut problems = Vec::new();

    for name in BINARIES {
        let out = match Command::new(binary(name)).arg("--version").output() {
            Ok(o) => o,
            Err(e) => {
                problems.push(format!("{name}: could not run: {e}"));
                continue;
            }
        };

        if !out.status.success() {
            problems.push(format!(
                "{name}: --version exited {}, stderr: {}",
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            ));
            continue;
        }

        let line = String::from_utf8_lossy(&out.stdout).trim().to_string();

        // The binary's OWN name, not the crate the code lives in. The first
        // implementation used `env!("CARGO_PKG_NAME")`, which expands where it is
        // written -- in m6-core -- so all four services that share that parser
        // announced themselves as "m6-core" and a deploy log could not say which
        // binary it had checked.
        if !line.starts_with(name) {
            problems.push(format!(
                "{name}: --version said {line:?}, which does not name this binary"
            ));
        }
        if !line.contains(expected_version) {
            problems.push(format!(
                "{name}: --version said {line:?}, expected version {expected_version}"
            ));
        }
    }

    assert!(
        problems.is_empty(),
        "{} binary/binaries cannot report a version, which leaves the deploy unable \
         to assert what it installed:\n{}",
        problems.len(),
        problems.join("\n")
    );
}

/// `--version` must not need a site directory or a config.
///
/// A deploy asks a freshly installed binary what it is *before* any config is in
/// place, so a flag that only works once two positional arguments are present is
/// no use. Three of the seven check their argument count first, and the flag had
/// to be handled ahead of that check in each.
#[test]
fn version_works_with_no_other_arguments() {
    for name in BINARIES {
        let out = Command::new(binary(name))
            .arg("--version")
            .output()
            .unwrap_or_else(|e| panic!("{name}: could not run: {e}"));
        assert!(
            out.status.success(),
            "{name}: --version with no other arguments exited {}. A deploy asks a \
             binary what it is before any config exists.\nstderr: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
}

/// The documented reason the version has to come from the binary: a config cannot
/// carry a floor, because an older binary ignores keys it does not know.
///
/// Pinned as a test rather than left in prose, because if m6 ever starts refusing
/// unknown config keys then a version floor becomes possible and this file's whole
/// argument changes. This failing is the signal to revisit that.
#[test]
fn an_unknown_config_key_is_ignored_not_refused() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(dir.path().join("public")).expect("public/");
    let conf = dir.path().join("m6-file.conf");
    std::fs::write(
        &conf,
        "[[route]]\npath = \"/a/{*rest}\"\nhandler = \"files\"\nroot = \"public/\"\n\
         min_m6_version = \"9.9.9\"\n",
    )
    .expect("write config");

    let out = Command::new(binary("m6-file"))
        .arg(dir.path())
        .arg(&conf)
        .arg("--dump-config")
        .output()
        .expect("run m6-file");

    assert!(
        out.status.success(),
        "m6-file refused a config carrying an unknown key. If that is now the \
         behaviour, a version floor in the config becomes a real option and the \
         deploy no longer has to ask the binary. Revisit \
         m6-core/tests/version_flag.rs and the ordering-hazard note in the \
         deployment's docs/OPERATIONS.md.\nstderr: {}",
        String::from_utf8_lossy(&out.stderr).trim()
    );
}
