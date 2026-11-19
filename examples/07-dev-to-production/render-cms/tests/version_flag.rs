//! `render-cms-prod --version` and `--version-full` answer from build-time constants, with
//! no site directory and no configuration, as every m6 binary does.

use std::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_render-cms-prod");

fn run(flag: &str) -> String {
    let out = Command::new(BIN)
        .arg(flag)
        .output()
        .expect("run the binary");
    assert!(out.status.success(), "{flag} exited {}", out.status);
    String::from_utf8(out.stdout).expect("version output is UTF-8")
}

#[test]
fn version_prints_only_the_tag_form() {
    assert_eq!(
        run("--version"),
        format!("v{}\n", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn version_full_names_the_binary_first_and_every_linked_m6_crate() {
    let out = run("--version-full");
    let mut names = Vec::new();
    for (i, line) in out.lines().enumerate() {
        let (name, full) = line
            .split_once(" - ")
            .unwrap_or_else(|| panic!("{line:?} is not '<crate> - <full SemVer>'"));
        let (version, metadata) = full
            .split_once('+')
            .unwrap_or_else(|| panic!("{line:?} has no commit"));
        let commit = metadata.strip_suffix(".dirty").unwrap_or(metadata);
        assert!(!commit.is_empty(), "{line:?} has an empty commit");
        if i == 0 {
            assert_eq!(name, "render-cms-prod", "the binary comes first");
            assert_eq!(version, env!("CARGO_PKG_VERSION"));
        }
        names.push(name);
    }
    assert_eq!(
        names,
        ["render-cms-prod", "m6-core", "m6-log", "m6-platform"]
    );
}
