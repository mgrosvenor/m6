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
