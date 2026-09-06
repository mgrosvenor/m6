//! A config reload must not silence logging.
//!
//! This exists because it happened in production. On 2026-09-06 every m6-http
//! node stopped logging the moment a deploy touched `site.toml`: `periodic
//! stats`, pool events, warnings and errors all vanished, while the server kept
//! serving traffic and reporting itself healthy. Only the `analytics` layer
//! survived, because it has its own writer that reload never touches.
//!
//! The tell was that `handle_site_reload` logs "config reload: complete"
//! immediately after calling `LogHandle::reload`, and that line was itself
//! missing from the journal -- so the writer was dead the instant reload
//! returned.
//!
//! Nothing caught it. The only tests over this module checked `parse_level`,
//! and `reload()` returns `()` and returns it perfectly happily while writing
//! into a dead channel. **So a test that asserts `reload()` succeeded proves
//! nothing at all** -- it has to assert that a log line emitted *afterwards*
//! actually reaches the writer.
//!
//! `tracing`'s subscriber is installed once per process and cannot be torn
//! down, so this runs the probe in a child process (this same test binary,
//! re-executed with `M6_LOG_RELOAD_CHILD` set) and inspects its real stdout.

use std::process::Command;

const CHILD_ENV: &str = "M6_LOG_RELOAD_CHILD";
const BEFORE: &str = "MARKER_BEFORE_RELOAD";
const AFTER: &str = "MARKER_AFTER_RELOAD";
const AFTER_SECOND: &str = "MARKER_AFTER_SECOND_RELOAD";

/// The probe. Only runs in the child; in the parent it returns immediately so
/// the parent's own run of this test name is a no-op.
#[test]
fn child_probe() {
    if std::env::var(CHILD_ENV).is_err() {
        return;
    }

    let handle = m6_core::log::init("text", "info").expect("init");
    tracing::info!("{BEFORE}");

    // Exactly what handle_site_reload does on every config reload.
    handle.reload("text", "info");
    tracing::info!("{AFTER}");

    // Twice, because the failure mode is cumulative: each reload replaces the
    // worker, so a bug that leaks or drops one guard may only bite on a later
    // reload. Production reloads on every deploy, not once.
    handle.reload("text", "info");
    tracing::info!("{AFTER_SECOND}");

    // The writer is non-blocking: the worker thread flushes asynchronously, so
    // exiting here could lose the lines for reasons unrelated to the bug.
    // Dropping the handle drops the WorkerGuard, which flushes and joins.
    drop(handle);
}

#[test]
fn reload_does_not_silence_logging() {
    // Guard against recursing if something re-enters with the env var set.
    if std::env::var(CHILD_ENV).is_ok() {
        return;
    }

    let exe = std::env::current_exe().expect("current_exe");
    let out = Command::new(exe)
        .args(["--exact", "child_probe", "--nocapture", "--test-threads=1"])
        .env(CHILD_ENV, "1")
        .output()
        .expect("spawn child probe");

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert!(
        stdout.contains(BEFORE),
        "the child logged nothing even before reloading, so this test is not \
         measuring what it claims -- fix the harness before trusting a pass.\n\
         stdout:\n{stdout}\nstderr:\n{stderr}"
    );

    assert!(
        stdout.contains(AFTER),
        "logging died after ONE config reload: the line emitted immediately \
         after reload() never reached the writer. This is the production \
         failure -- every deploy blinds the server to its own errors and \
         warnings while it keeps serving and looks healthy.\n\
         stdout:\n{stdout}\nstderr:\n{stderr}"
    );

    assert!(
        stdout.contains(AFTER_SECOND),
        "logging survived one reload but died on the second, so the writer is \
         being leaked or dropped per reload. Production reloads on every \
         deploy.\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
}
