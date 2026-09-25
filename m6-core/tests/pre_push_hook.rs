//! The pre-push hook is the branch rules, so the branch rules get tests.
//!
//! `.githooks/pre-push` decides what may be pushed where. It had three defects
//! at once, all of them found by hand, two of them only after they had already
//! cost something:
//!
//!   1. It `exec`ed from inside the `while read` loop, so only the FIRST ref was
//!      ever validated. Pushing two branches at once checked one of them.
//!   2. A tag ref did not start with `refs/heads/`, so `refs/tags/v1.0.0` fell
//!      through to the work-branch case and was refused for "not naming an
//!      issue". **No release could be tagged at all.** It went unnoticed because
//!      v1.0.0 was the first tag pushed after the hook was written, and the
//!      release script died half way through: main pushed, tag refused.
//!   3. Under `set -u`, bash 3.2 -- which is still what macOS ships, and this
//!      hook runs on a Mac -- treats expanding an EMPTY array as an unbound
//!      variable. git can invoke the hook with nothing on stdin, and the push
//!      then failed for a reason unrelated to the push.
//!
//! None of the three is visible by reading the script, which is why they are
//! here. A hook is a program; it gets tested like one.
//!
//! ## How
//!
//! The hook reads refs on stdin and communicates entirely through its exit
//! status, so each test runs the real file with crafted stdin and a chosen
//! environment. Nothing here touches a repository or a remote: no test can push
//! anything, and the hook is never invoked by git.
//!
//! It is run under `bash` explicitly rather than by its shebang, so a test
//! failure is about the hook's logic and not about which bash is on PATH.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

fn hook_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("m6-core has a parent")
        .join(".githooks/pre-push")
}

struct Outcome {
    allowed: bool,
    output: String,
}

/// Run the hook with `stdin_lines` on stdin and `release` deciding M6_RELEASE.
///
/// One line per ref, in git's own format:
/// `<local ref> <local sha> <remote ref> <remote sha>`.
fn run(stdin_lines: &[&str], release: bool) -> Outcome {
    let hook = hook_path();
    assert!(hook.is_file(), "{} does not exist", hook.display());

    let mut cmd = Command::new("bash");
    cmd.arg(&hook)
        .env_remove("M6_RELEASE")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if release {
        cmd.env("M6_RELEASE", "1");
    }

    let mut child = cmd.spawn().expect("could not run bash");
    {
        let stdin = child.stdin.as_mut().expect("stdin");
        for line in stdin_lines {
            writeln!(stdin, "{line}").expect("write to hook stdin");
        }
    }
    let out = child.wait_with_output().expect("hook did not finish");

    Outcome {
        allowed: out.status.success(),
        output: format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ),
    }
}

/// A ref line for a branch push. The shas are arbitrary: the hook reads names.
fn branch(name: &str) -> String {
    format!("refs/heads/{name} 1111111111111111111111111111111111111111 refs/heads/{name} 2222222222222222222222222222222222222222")
}

fn tag(name: &str) -> String {
    format!("refs/tags/{name} 1111111111111111111111111111111111111111 refs/tags/{name} 0000000000000000000000000000000000000000")
}

// ── Defect 2: a release tag could not be pushed ──────────────────────────────

#[test]
fn a_release_tag_is_allowed_when_m6_release_is_set() {
    let r = run(&[&tag("v1.0.0")], true);
    assert!(
        r.allowed,
        "the hook refused a release tag, so no release can be cut:\n{}",
        r.output
    );
}

#[test]
fn a_release_tag_is_refused_without_m6_release() {
    let r = run(&[&tag("v1.0.0")], false);
    assert!(
        !r.allowed,
        "a release tag pushed by hand was allowed; only tag.sh should push one:\n{}",
        r.output
    );
}

#[test]
fn a_tag_that_is_not_a_version_is_refused() {
    // Even with the release variable set: `v<major>.<minor>.<patch>` is the only
    // shape of tag this repository has.
    for name in ["nightly", "v1.0", "1.0.0", "v1.0.0-rc1"] {
        let r = run(&[&tag(name)], true);
        assert!(
            !r.allowed,
            "tag '{name}' was allowed and is not a release version:\n{}",
            r.output
        );
    }
}

// ── Defect 1: only the first ref was checked ─────────────────────────────────

#[test]
fn every_ref_is_checked_not_just_the_first() {
    // A legitimate branch first, then one with no issue number. The second is
    // the one that must be refused, and the whole push with it.
    let refs = [branch("fix/42-a-real-issue"), branch("my-scratch-branch")];
    let lines: Vec<&str> = refs.iter().map(String::as_str).collect();
    let r = run(&lines, false);
    assert!(
        !r.allowed,
        "a branch with no issue number was pushed because a valid branch came \
         first on stdin. This is the `exec`-inside-the-loop defect:\n{}",
        r.output
    );
}

#[test]
fn a_refusable_ref_is_caught_in_either_order() {
    // The same two refs the other way round, so the test above cannot pass for
    // the accidental reason that the hook only ever reads the last line either.
    let refs = [branch("my-scratch-branch"), branch("fix/42-a-real-issue")];
    let lines: Vec<&str> = refs.iter().map(String::as_str).collect();
    let r = run(&lines, false);
    assert!(!r.allowed, "expected a refusal:\n{}", r.output);
}

// ── Defect 3: empty stdin failed the push ────────────────────────────────────

#[test]
fn no_refs_at_all_is_not_a_failure() {
    let r = run(&[], false);
    assert!(
        r.allowed,
        "the hook failed with nothing on stdin. On bash 3.2 under `set -u` an \
         empty array expansion is an unbound variable, and the push then fails \
         for a reason that has nothing to do with the push:\n{}",
        r.output
    );
}

#[test]
fn a_blank_line_on_stdin_is_not_a_failure() {
    let r = run(&["", "   "], false);
    assert!(
        r.allowed,
        "blank input was treated as a ref to refuse:\n{}",
        r.output
    );
}

// ── The rules themselves ─────────────────────────────────────────────────────

#[test]
fn main_refuses_a_direct_push() {
    let r = run(&[&branch("main")], false);
    assert!(
        !r.allowed,
        "main took a direct push. It advances only by a merged pull request:\n{}",
        r.output
    );
    assert!(
        r.output.contains("pull request"),
        "the refusal should say what to do instead, and it said:\n{}",
        r.output
    );
}

#[test]
fn main_accepts_the_release_path() {
    let r = run(&[&branch("main")], true);
    assert!(
        r.allowed,
        "the release path to main was refused with M6_RELEASE=1:\n{}",
        r.output
    );
}

#[test]
fn develop_is_allowed() {
    let r = run(&[&branch("develop")], false);
    assert!(r.allowed, "develop was refused:\n{}", r.output);
}

#[test]
fn a_work_branch_must_name_an_issue() {
    let allowed = [
        "feat/1-one",
        "fix/42-h3-gate-measures-nothing",
        "perf/57-stop-copying-the-config",
        "docs/9-say-what-it-does",
        "refactor/12-one-way-in",
        "test/8-cover-the-hook",
        "chore/3-tidy",
    ];
    for name in allowed {
        let r = run(&[&branch(name)], false);
        assert!(r.allowed, "'{name}' should be allowed:\n{}", r.output);
    }

    let refused = [
        "my-branch",               // no type, no issue
        "fix/no-issue-number",     // type, no issue
        "wip/42-wrong-type",       // issue, type not in the list
        "fix-42-dashes-not-slash", // not the separator
    ];
    for name in refused {
        let r = run(&[&branch(name)], false);
        assert!(!r.allowed, "'{name}' should be refused:\n{}", r.output);
    }
}

#[test]
fn a_branch_deletion_is_not_this_hooks_business() {
    // git sends an all-zero local sha for a delete. The hook has nothing to say
    // about removing a branch, including one whose name it would have refused.
    let line = "(delete) 0000000000000000000000000000000000000000 refs/heads/my-scratch-branch 2222222222222222222222222222222222222222";
    let r = run(&[line], false);
    assert!(
        r.allowed,
        "deleting a branch was refused because of its name:\n{}",
        r.output
    );
}

#[test]
fn the_hook_says_what_it_did_not_check() {
    // The same rule tools/conformance.sh follows: a check that stays quiet about
    // its scope gets mistaken for one that covered everything. This hook runs no
    // tests, and it has to say so, because it used to run all of them.
    let r = run(&[&branch("fix/42-a-real-issue")], false);
    assert!(r.allowed);
    assert!(
        r.output.contains("branch rules only"),
        "the hook did not say that tests were not run here:\n{}",
        r.output
    );
}

// ── Defect 4: a refusal named a script that had been deleted ─────────────────

/// Every script this hook offers as a remedy has to exist.
///
/// Issue #118. Both tag refusals said `./tools/release.sh`, which was deleted on
/// 2026-09-14 along with `tools/merge.sh` when a pull request became the only way
/// into `develop` and the only way from `develop` to `main`. The hook's own header
/// records that deletion ten lines above the first refusal, and its `main` guidance
/// correctly says `./tag.sh`. So the file disagreed with itself for twelve days, and
/// it disagreed in the only part of a hook anybody ever reads.
///
/// **A refusal is the entire user interface of a gate.** These two fire at the moment
/// someone is cutting a release, which is exactly when they are least inclined to go
/// and check whether the remedy exists, and following it gets `no such file or
/// directory` from the tool whose whole job is to be believed.
///
/// COMMENTS ARE EXEMPT, and deliberately. The header names both deleted scripts to
/// say that they are gone, which is the record of why the rules are what they are.
/// What may not name them is the text the hook PRINTS, and every refusal string sits
/// on a non-comment line.
///
/// `.github/pull_request_template.md` is held to the same rule and is scanned whole,
/// having no comments. It told the author of every pull request to run
/// `./tools/merge.sh <branch>`, which is the same deleted script in front of a far
/// larger audience.
#[test]
fn every_script_offered_as_a_remedy_exists() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("m6-core has a parent")
        .to_path_buf();

    // `#` comments are skipped for the hook and not for the template, which has
    // none: a `#` there is a markdown heading and carries instructions.
    let sources: [(&str, bool); 2] = [
        (".githooks/pre-push", true),
        (".github/pull_request_template.md", false),
    ];

    let mut missing = Vec::new();
    for (rel, skip_comments) in sources {
        let path = root.join(rel);
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("could not read {}: {e}", path.display()));

        for (n, line) in text.lines().enumerate() {
            if skip_comments && line.trim_start().starts_with('#') {
                continue;
            }
            for script in scripts_named(line) {
                // Relative to the repository root, which is how every one of
                // these is written and how a reader would run it.
                if !root.join(&script).is_file() {
                    missing.push(format!("{rel}:{}: {script}", n + 1));
                }
            }
        }
    }

    assert!(
        missing.is_empty(),
        "{} reference(s) to a script that does not exist:\n  {}\n\n\
         These files are pure instruction: every path in them is something a person \
         is being told to run. `tools/merge.sh` and `tools/release.sh` were deleted \
         on 2026-09-14 and a pull request replaced both. Use `./tag.sh` for a \
         release tag and `./tools/branch.sh` for a work branch. Issue #118.",
        missing.len(),
        missing.join("\n  ")
    );
}

/// The tag refusals name the tool that actually pushes a tag.
///
/// The other half of #118, and it needs saying separately: a refusal that merely
/// avoids naming a deleted script is not yet a refusal that helps. `./tag.sh` is also
/// what sets `M6_RELEASE`, so the second refusal was naming the wrong script for the
/// one fact it exists to convey.
#[test]
fn a_tag_refusal_names_tag_sh() {
    for (refs, release, what) in [
        (tag("nightly"), true, "a tag that is not a version"),
        (tag("v1.0.0"), false, "a release tag without M6_RELEASE"),
    ] {
        let r = run(&[&refs], release);
        assert!(!r.allowed, "expected {what} to be refused:\n{}", r.output);
        assert!(
            r.output.contains("tag.sh"),
            "the refusal for {what} does not name ./tag.sh, which is what pushes a \
             release tag and what sets M6_RELEASE. It named ./tools/release.sh, \
             deleted on 2026-09-14, until 2026-09-26. Issue #118. It said:\n{}",
            r.output
        );
        assert!(
            !r.output.contains("release.sh"),
            "the refusal for {what} still names a deleted script:\n{}",
            r.output
        );
    }
}

/// Pull `*.sh` paths out of one line.
///
/// Hand-rolled rather than a regex, because this test crate has no regex
/// dependency and adding one to read four lines of shell would be the larger
/// change. It walks back from each `.sh` over the characters a path may contain,
/// which stops at a quote, a backtick or a space and so takes `./tools/branch.sh`
/// out of `Create one with:  ./tools/branch.sh <issue> <slug>`.
fn scripts_named(line: &str) -> Vec<String> {
    let c: Vec<char> = line.chars().collect();
    let mut found = Vec::new();
    let mut i = 0;
    while i + 3 <= c.len() {
        if c[i] == '.' && c[i + 1] == 's' && c[i + 2] == 'h' {
            // `.shell` is not a script; `.sh's` and `.sh,` are.
            let ends = c.get(i + 3).is_none_or(|ch| !ch.is_alphanumeric());
            if ends {
                let mut j = i;
                while j > 0 {
                    let prev = c[j - 1];
                    if prev.is_alphanumeric() || prev == '-' || prev == '_' || prev == '/' {
                        j -= 1;
                    } else {
                        break;
                    }
                }
                let raw: String = c[j..i + 3].iter().collect();
                // Everything here is written relative to the repository root, as
                // `./tag.sh` or `tools/branch.sh`. The leading `.` stops the walk
                // above (it is not a path character for this purpose), so the
                // slash it left behind comes off here.
                //
                // A bare `.sh` is prose about the extension rather than a path.
                let rel = raw.trim_start_matches('/');
                if rel.len() > 3 {
                    found.push(rel.to_string());
                }
            }
            i += 3;
        } else {
            i += 1;
        }
    }
    found
}

/// The hook must be fast, and that is a correctness property rather than a
/// preference.
///
/// It used to run the whole workspace test suite plus conformance, over ten
/// minutes. **That broke pushing outright**: git opens its connection to the
/// remote, runs the hook, then sends the pack. With a ten minute hook in the
/// middle the server closes the idle connection, and git writes the pack to a
/// dead socket -- killed by SIGPIPE, exit 141, no error message. From outside
/// it looked exactly like a push that silently did nothing, while the checks
/// printed "All checks passed".
///
/// Two seconds is far above what the branch rules need and far below anything
/// that could idle out a connection.
#[test]
fn the_hook_finishes_quickly() {
    let start = std::time::Instant::now();
    let r = run(&[&branch("develop")], false);
    let elapsed = start.elapsed();
    assert!(r.allowed);
    assert!(
        elapsed < std::time::Duration::from_secs(2),
        "the hook took {elapsed:?}. A slow pre-push hook does not fail loudly, \
         it makes git write its pack to a connection the server has already \
         closed. Content checks belong in CI, not here."
    );
}
