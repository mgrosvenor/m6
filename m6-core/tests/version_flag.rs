//! Every installed binary can say which version it is.
//!
//! There is one deployment and we control it, so this is not about compatibility
//! with binaries in the wild. It is so a deploy can log and record what it
//! actually installed: `docs/RELEASES.md` records the m6 commit per deploy, and a
//! binary that cannot answer makes that a claim rather than a reading.
//!
//! It is a test rather than a note because four of the seven parse their own
//! arguments instead of using m6-core's `parse_invocation`, so m6-http, m6-md and
//! m6-auth-cli each needed the flag adding separately, and a new binary will too.

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
