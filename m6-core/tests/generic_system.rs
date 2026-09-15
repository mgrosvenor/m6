//! m6 is a generic web system, and this test is what keeps it one.
//!
//! Issue #10. m6 had accumulated one particular deployment's details: node names,
//! WireGuard addresses, analytics paths, a fleet table in the handover, an hourly
//! health check for one site's three servers, and tooling that defaulted to a
//! directory under one person's home. All of it has been moved to the deployment
//! that owns it.
//!
//! A tidy-up like that is true on the day it is done. What makes it stay true is a
//! check that fails when it stops being, which is why this is a test and not a
//! closed issue.
//!
//! ## What counts as a trace
//!
//! A site's **identity**: the domain it serves and the name of its repository.
//! Those are the things that make a generic system describe one deployment.
//!
//! What does not count, and why the allowlist exists:
//!
//! - the author's name in `Cargo.toml`, which is authorship, not a site
//! - `github.com/mgrosvenor/m6`, this repository's own URL
//! - `github.com/mgrosvenor/quiche`, the pinned fork m6-http builds against
//!
//! Those three share a surname with the deployment and nothing else.
//!
//! RFC 1918 addresses are not on the banned list either. `10.0.0.1` appears in
//! `m6-http`'s tests because private-address classification is the property under
//! test, and an earlier pass at this replaced them with RFC 5737 documentation
//! addresses, which broke two tests: the membership of the private range was the
//! whole point. Deleting a fact to satisfy a search is worse than the search.

use std::path::{Path, PathBuf};

/// Strings that name one particular deployment and must not appear.
///
/// Split so this file does not match itself: a literal `"mgrosvenor.com"` here
/// would make the test fail on its own source, and the usual fix for that is to
/// exclude the file, which then also excludes any real trace someone later adds
/// to it.
fn banned() -> Vec<String> {
    vec![
        format!("dr-{}-site", "grosvenor"),
        format!("{}.com", "mgrosvenor"),
        // The maintainer's OTHER domain, added 2026-09-15 after this test missed
        // it. `SECURITY.md` carried a personal address at `thegrosvenors.au` for
        // however long, on the front page of a public repository, and this test
        // said nothing because it only knew one of the two domains. A guard that
        // covers most of the thing it guards is the kind that gets trusted.
        format!("the{}.au", "grosvenors"),
    ]
}

/// Substrings that legitimately contain the surname. Checked before the ban, so a
/// line carrying only these is not a finding.
///
/// The point of this test is to keep one DEPLOYMENT's details -- node names,
/// backbone addresses, analytics paths, fleet topology -- out of a generic web
/// system. It is not to make the maintainer uncontactable, so the project's own
/// URLs and its way of receiving a security report are allowed, and are the only
/// things that are.
const ALLOWED: &[&str] = &[
    "github.com/mgrosvenor/m6",
    "github.com/mgrosvenor/quiche",
    "Matthew P. Grosvenor",
    // SECURITY.md's reporting channel. A project needs a way to receive a
    // vulnerability report, and this is a link to a contact form rather than a
    // published address precisely so nothing is there for a scraper to read.
    "mgrosvenor.com/contact",
];

fn repo_root() -> PathBuf {
    // CARGO_MANIFEST_DIR is m6-core; the workspace is its parent.
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("m6-core has a parent")
        .to_path_buf()
}

/// Files worth reading: source, configuration, scripts and documentation.
fn is_interesting(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|e| e.to_str()),
        Some("rs" | "toml" | "sh" | "py" | "conf" | "md" | "yml" | "yaml" | "html" | "json")
    )
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
        // target/ holds build output including vendored dependency sources, and
        // .git/ holds every past version of every file -- and the point of this
        // test is the working tree, not the history that got it here.
        if name == "target" || name == ".git" || name == "node_modules" {
            continue;
        }
        if p.is_dir() {
            walk(&p, out);
        } else if is_interesting(&p) {
            out.push(p);
        }
    }
}

#[test]
fn no_file_names_one_particular_deployment() {
    let root = repo_root();
    let mut files = Vec::new();
    walk(&root, &mut files);
    assert!(
        files.len() > 50,
        "only found {} files to scan under {}; the walk is not working, and a \
         search that looks at nothing passes",
        files.len(),
        root.display()
    );

    let banned = banned();
    let this_file = Path::new(file!()).file_name().and_then(|n| n.to_str());
    let mut findings = Vec::new();

    for path in &files {
        // Do not read this file: it necessarily contains the banned strings. It is
        // excluded by NAME rather than by extension, so nothing else is skipped.
        if path.file_name().and_then(|n| n.to_str()) == this_file {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(path) else {
            continue; // not UTF-8, so not prose or source
        };
        for (n, line) in text.lines().enumerate() {
            if ALLOWED.iter().any(|a| line.contains(a)) {
                continue;
            }
            for b in &banned {
                if line.contains(b.as_str()) {
                    let rel = path.strip_prefix(&root).unwrap_or(path);
                    findings.push(format!(
                        "{}:{}: {}",
                        rel.display(),
                        n + 1,
                        line.trim().chars().take(120).collect::<String>()
                    ));
                }
            }
        }
    }

    assert!(
        findings.is_empty(),
        "m6 is a generic web system and {} line(s) name one particular deployment. \
         Move it to that deployment's own repository, or add it to ALLOWED here with \
         the reason if it is genuinely generic:\n{}",
        findings.len(),
        findings.join("\n")
    );
}

/// The scan can actually find something.
///
/// Without this, a walk that silently reads nothing, or an `ALLOWED` entry broad
/// enough to swallow everything, would leave the test passing for the wrong
/// reason. The check above is only worth its green if this one is green too.
#[test]
fn the_scan_would_catch_a_trace() {
    let banned = banned();
    assert!(
        !banned.is_empty(),
        "the banned list is empty; the scan guards nothing"
    );

    // EVERY entry, not just one. This used to test `banned[1]` alone, so a third
    // entry could be added and never exercised -- and the entry that was missing
    // was the maintainer's second domain, which sat in a published SECURITY.md
    // with this test passing beside it. A self-test that checks one of n is a
    // self-test for one of n.
    for b in &banned {
        let sample = format!("  sites = [\"{b}\"]");
        assert!(
            banned.iter().any(|x| sample.contains(x.as_str())),
            "the banned list does not match a line that plainly contains {b}"
        );
        assert!(
            !ALLOWED.iter().any(|a| sample.contains(a)),
            "an ALLOWED entry is broad enough to excuse a real trace of {b}"
        );
    }
}
