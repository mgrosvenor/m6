//! Every m6 binary answers `--version` and `--version-full` from build-time
//! constants.
//!
//! `--version` prints only the tag form, `v1.16.0-rc.1`, with no crate name and
//! no build metadata. `--version-full` prints one `<crate> - <full SemVer>` line
//! per m6 crate in the binary, the binary first, and each line comes from that
//! crate's own build script. A mixed build therefore shows two commits.
//!
//! The binary list and each binary's expected crate set come from `cargo
//! metadata`, so a new binary or a new m6 dependency fails this test until it is
//! covered. Examples and site apps are a later change; this test covers the
//! workspace's shipping binaries.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use semver::Version;
use serde_json::Value;

use m6_core::testkit::binary;

/// One binary and the m6 crates its package links.
struct Binary {
    name: String,
    package: String,
    linked: BTreeSet<String>,
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("m6-core sits in the workspace")
        .to_path_buf()
}

/// The host triple `cargo metadata` filters the dependency graph by.
///
/// It comes from `rustc -vV`'s `host:` line, the same answer cargo gives for
/// the machine the test is running on. A triple written into this test would be
/// wrong on the other supported target, and it would be a machine name in a
/// public repository.
fn host_triple() -> String {
    let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
    let out = Command::new(rustc)
        .arg("-vV")
        .output()
        .expect("run rustc -vV");
    assert!(
        out.status.success(),
        "rustc -vV failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let reported = String::from_utf8_lossy(&out.stdout);
    reported
        .lines()
        .find_map(|line| line.strip_prefix("host: "))
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .expect("rustc -vV reports a host triple")
        .to_string()
}

/// `cargo metadata` for the workspace, with the resolved dependency graph.
///
/// `--offline --locked` keeps the test out of the network and fails if the
/// lockfile disagrees with a manifest, which is the same check a release makes.
///
/// `--filter-platform` is what keeps `--offline` working on a machine that
/// fetched crates for this target only, which is what a CI runner is. Without
/// it, `cargo metadata` resolves every target's dependencies and refuses an
/// offline run whose cache has no Android-only crate in it. The graph the test
/// reads is this machine's, which is the one whose binaries it runs.
fn metadata() -> Value {
    let root = workspace_root();
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let out = Command::new(cargo)
        .arg("metadata")
        .arg("--format-version")
        .arg("1")
        .arg("--offline")
        .arg("--locked")
        .arg("--filter-platform")
        .arg(host_triple())
        .arg("--manifest-path")
        .arg(root.join("Cargo.toml"))
        .output()
        .expect("run cargo metadata");
    assert!(
        out.status.success(),
        "cargo metadata failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).expect("cargo metadata returns JSON")
}

/// Every shipping binary, with the transitive set of m6 packages it links.
///
/// Examples are excluded: I-377 adopts them in a later change. The filter is the
/// package name and the target's path, so an example package or an `examples/`
/// binary is not mistaken for a service.
fn binaries() -> &'static [Binary] {
    static BINARIES: OnceLock<Vec<Binary>> = OnceLock::new();
    BINARIES.get_or_init(load_binaries)
}

fn load_binaries() -> Vec<Binary> {
    let meta = metadata();
    let packages = meta["packages"].as_array().expect("packages");
    let nodes = meta["resolve"]["nodes"].as_array().expect("resolve nodes");

    let mut names: BTreeMap<String, String> = BTreeMap::new();
    let mut is_workspace_package: BTreeMap<String, bool> = BTreeMap::new();
    for package in packages {
        let id = package["id"].as_str().expect("package id").to_string();
        names.insert(
            id.clone(),
            package["name"].as_str().expect("name").to_string(),
        );
        is_workspace_package.insert(id, package["source"].is_null());
    }

    let mut normal_deps: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for node in nodes {
        let id = node["id"].as_str().expect("node id").to_string();
        let mut deps = Vec::new();
        for dep in node["deps"].as_array().expect("deps") {
            let normal = dep["dep_kinds"].as_array().is_some_and(|kinds| {
                kinds
                    .iter()
                    .any(|kind| kind["kind"].is_null() || kind["kind"].as_str() == Some("normal"))
            });
            if normal {
                deps.push(dep["pkg"].as_str().expect("dep pkg").to_string());
            }
        }
        normal_deps.insert(id, deps);
    }

    let mut binaries = Vec::new();
    for package in packages {
        // Only this workspace's packages ship. A registry package can have a
        // binary of its own (`brotli` does), and it is not an m6 service.
        if !package["source"].is_null() {
            continue;
        }
        let package_id = package["id"].as_str().expect("package id");
        let package_name = package["name"].as_str().expect("name");
        if package_name.starts_with("m6-example") {
            continue;
        }
        for target in package["targets"].as_array().expect("targets") {
            let kinds = target["kind"].as_array().expect("target kind");
            if !kinds.iter().any(|kind| kind.as_str() == Some("bin")) {
                continue;
            }
            let src_path = target["src_path"].as_str().expect("src path");
            if src_path.contains("/examples/") {
                continue;
            }
            let binary_name = target["name"].as_str().expect("target name").to_string();

            // The m6 crates a normal build of this package links. A path
            // dependency is a workspace crate. Nothing outside m6 is listed.
            let mut linked = BTreeSet::new();
            let mut stack = vec![package_id.to_string()];
            let mut seen = BTreeSet::new();
            while let Some(id) = stack.pop() {
                if !seen.insert(id.clone()) {
                    continue;
                }
                let name = names.get(&id).expect("every node is a package").as_str();
                if name.starts_with("m6-")
                    && is_workspace_package.get(&id).copied().unwrap_or(false)
                {
                    linked.insert(name.to_string());
                }
                for dep in normal_deps.get(&id).into_iter().flatten() {
                    stack.push(dep.clone());
                }
            }

            binaries.push(Binary {
                name: binary_name,
                package: package_name.to_string(),
                linked,
            });
        }
    }
    binaries.sort_by(|a, b| a.name.cmp(&b.name));
    binaries
}

/// One git command from the workspace root, trimmed, or `None` when git is
/// absent or the command fails. A source tarball answers `None`.
fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(workspace_root())
        .args(args)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!text.is_empty()).then_some(text)
}

/// Run a binary with one flag and return its stdout, asserting success.
fn run(binary_name: &str, flag: &str) -> String {
    let out = Command::new(binary(binary_name))
        .arg(flag)
        .output()
        .unwrap_or_else(|e| panic!("{binary_name}: could not run: {e}"));
    assert!(
        out.status.success(),
        "{binary_name} {flag} exited {}:\n{}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).expect("version output is UTF-8")
}

/// Split one `--version-full` line into the crate and its full SemVer string.
fn split_line(line: &str) -> (&str, &str) {
    let (name, full) = line
        .split_once(" - ")
        .unwrap_or_else(|| panic!("{line:?} is not '<crate> - <full SemVer>'"));
    assert!(
        !name.contains(" - "),
        "{line:?} has more than one separator"
    );
    assert!(
        !full.contains(" - "),
        "{line:?} has more than one separator"
    );
    (name, full)
}

#[test]
fn version_prints_only_the_tag_form() {
    let expected = Version::parse(env!("CARGO_PKG_VERSION")).expect("workspace version is SemVer");
    let expected_tag = format!("v{expected}");

    for bin in binaries() {
        let stdout = run(&bin.name, "--version");
        let line = stdout.trim_end();
        assert_eq!(
            line, expected_tag,
            "{}: --version must print only the tag form, with no crate name \
             and no build metadata",
            bin.name
        );
        assert!(
            !line.contains('+') && !line.contains("dirty") && !line.contains(' '),
            "{}: --version carried more than the tag: {line:?}",
            bin.name
        );
    }
}

#[test]
fn version_full_names_the_binary_first_and_every_linked_m6_crate() {
    let expected = Version::parse(env!("CARGO_PKG_VERSION")).expect("workspace version is SemVer");
    let commit = git(&["rev-parse", "--short=12", "HEAD"]);

    for bin in binaries() {
        let stdout = run(&bin.name, "--version-full");
        let lines: Vec<&str> = stdout.lines().collect();
        assert!(
            !lines.is_empty(),
            "{}: --version-full printed nothing",
            bin.name
        );

        let (first_name, _) = split_line(lines[0]);
        assert_eq!(
            first_name, bin.name,
            "{}: --version-full must name the binary first, not its package {}",
            bin.name, bin.package
        );

        let mut listed = BTreeSet::new();
        for line in &lines {
            let (name, full) = split_line(line);
            listed.insert(name.to_string());

            let parsed = Version::parse(full)
                .unwrap_or_else(|e| panic!("{}: {full:?} is not SemVer: {e}", bin.name));
            assert_eq!(parsed.major, expected.major, "{}: {line:?}", bin.name);
            assert_eq!(parsed.minor, expected.minor, "{}: {line:?}", bin.name);
            assert_eq!(parsed.patch, expected.patch, "{}: {line:?}", bin.name);
            assert_eq!(parsed.pre, expected.pre, "{}: {line:?}", bin.name);
            assert!(
                !parsed.build.is_empty(),
                "{}: {line:?} has no build metadata",
                bin.name
            );
            assert_eq!(
                format!("{name} - {full}"),
                *line,
                "{}: the line is not '<crate> - <full SemVer>'",
                bin.name
            );

            if let Some(commit) = &commit {
                let metadata = parsed.build.as_str();
                assert!(
                    metadata == commit || metadata == format!("{commit}.dirty"),
                    "{}: {line:?} does not carry this tree's commit {commit} and an \
                     optional .dirty suffix",
                    bin.name
                );
            }
        }

        assert!(
            listed.contains("m6-core"),
            "{}: --version-full did not list m6-core: {stdout:?}",
            bin.name
        );
        let mut expected_listed = bin.linked.clone();
        expected_listed.insert(bin.name.clone());
        assert_eq!(
            listed, expected_listed,
            "{}: --version-full lists a crate the binary does not link, or misses \
             one it does",
            bin.name
        );
    }
}

/// The seven binaries that share the m6-http package each name themselves on
/// the first line, and the package second. Before, all seven printed
/// `m6-http - ...` first, so `m6-bench --version-full` could not be told from
/// the server's.
#[test]
fn every_binary_of_a_shared_package_names_itself_first() {
    let shared: Vec<&Binary> = binaries()
        .iter()
        .filter(|bin| bin.package == "m6-http" && bin.name != bin.package)
        .collect();
    let names: BTreeSet<&str> = shared.iter().map(|bin| bin.name.as_str()).collect();
    assert_eq!(
        names,
        BTreeSet::from([
            "bench-url-backend",
            "m6-bench",
            "m6-bench-detail",
            "m6-perf-ab",
            "m6-probe-h1",
            "m6-probe-h2",
            "m6-probe-h3",
        ]),
        "the m6-http package's other binaries"
    );
    for bin in shared {
        let stdout = run(&bin.name, "--version-full");
        let lines: Vec<&str> = stdout.lines().collect();
        assert!(
            lines[0].starts_with(&format!("{} - ", bin.name)),
            "{}: the first line names the binary: {stdout:?}",
            bin.name
        );
        assert!(
            lines
                .get(1)
                .is_some_and(|line| line.starts_with("m6-http - ")),
            "{}: the second line names the package it is built from: {stdout:?}",
            bin.name
        );
    }
}
