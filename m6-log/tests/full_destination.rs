//! A full destination never blocks, keeps the buffered lines, and reports the
//! count it dropped once it drains.
//!
//! Both behaviours replace process standard output, so they run one after the
//! other inside one `#[test]`. The binary needs no `--test-threads=1`.

#![allow(
    clippy::disallowed_methods,
    reason = "test code: a pipe, a reader thread and a deadline drive the sink from outside"
)]

mod common;

use std::fs;
use std::fs::File;
use std::io::Read;
use std::os::fd::OwnedFd;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use common::Stdout;
use m6_platform::fd::{dup, dup2_stdout};

/// These tests replace process stdout, so they take turns even when the
/// harness runs them on several threads.
static STDOUT: std::sync::Mutex<()> = std::sync::Mutex::new(());

const LINES: u64 = 100_000;
const PROMPTLY: Duration = Duration::from_secs(2);
const DEADLINE: Duration = Duration::from_secs(5);

/// Both behaviours below replace the process's standard output, and the
/// harness writes its own `test ... ok` lines to that same descriptor. Two
/// `#[test]` functions in one binary therefore race the harness against the
/// redirect even though the bodies take turns on `STDOUT`: the failure is an
/// empty line in the captured stream ("every line is JSON: EOF"). They are one
/// test so the harness has no output to write between them.
#[test]
fn a_full_destination_never_blocks_and_drops_are_counted() {
    a_never_blocking_destination_drops_and_reports();
    a_refused_report_keeps_the_count();
}

fn a_never_blocking_destination_drops_and_reports() {
    let _stdout = STDOUT
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let saved_stdout = dup(&std::io::stdout()).expect("a saved stdout");
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let (stdout, read) = Stdout::to_pipe();
        m6_log::init(m6_log::Level::Info, None).expect("the logger");

        let started = Instant::now();
        for n in 0..LINES {
            m6_log::info!(n = n, "to a pipe nobody reads");
        }
        let elapsed = started.elapsed();
        let dropped = m6_log::operational_dropped();

        let received = read_later(read);
        m6_log::flush();
        m6_log::stop().expect("a clean stop");
        drop(stdout);

        let text = received.recv_timeout(DEADLINE).expect("the reader's bytes");
        let text = String::from_utf8(text).expect("UTF-8 lines");
        let _ = sender.send((elapsed, dropped, text));
    });

    let (elapsed, dropped, text) = match receiver.recv_timeout(DEADLINE) {
        Ok(result) => result,
        Err(_) => {
            // The blocked writer holds the pipe; put the harness's own output
            // back before failing, so the failure is reported instead of lost.
            dup2_stdout(&saved_stdout).expect("stdout restored");
            panic!("logging to a full pipe did not return before the deadline");
        }
    };
    assert!(
        elapsed < PROMPTLY,
        "logging to a full pipe took {elapsed:?}"
    );
    assert!(dropped > 0, "a full pipe dropped nothing");

    let lines: Vec<&str> = text.lines().collect();
    assert!(lines.len() > 1, "the buffered lines did not arrive");
    let reports = lines
        .iter()
        .filter(|line| line.contains("\"log lines dropped\""))
        .count();
    assert_eq!(reports, 1, "the drain reported the dropped count once");
    let report: serde_json::Value =
        serde_json::from_str(lines.last().expect("a last line")).expect("the report is JSON");
    assert_eq!(
        report["fields"]["message"].as_str(),
        Some("log lines dropped")
    );
    assert_eq!(report["fields"]["dropped"].as_u64(), Some(dropped));
    for line in &lines {
        let _: serde_json::Value = serde_json::from_str(line).expect("every line is JSON");
    }
}

/// A report line the destination refuses must not consume the failed count:
/// the next report, once the destination accepts, still names the whole loss.
fn a_refused_report_keeps_the_count() {
    let _stdout = STDOUT
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let dir = tempfile::tempdir().expect("a directory");
    let (stdout, read) = Stdout::to_pipe();
    drop(read);
    let mut sink = m6_log::Sink::stdout().expect("the sink");

    let payload = "x".repeat(100);
    for _ in 0..1_000 {
        m6_log::Line::begin(&mut sink, m6_log::Level::Info, "line")
            .str("payload", &payload)
            .end();
    }
    let counted = sink.dropped();
    assert!(
        counted > 0,
        "a pipe with no reader kept every line and counted nothing"
    );

    // The flush writes the complete lines it still holds and then the report
    // line, and the refused destination loses both.
    sink.flush();
    let dropped = sink.dropped();
    assert!(dropped >= counted, "the flush forgot a buffered drop");
    sink.flush();
    assert_eq!(
        sink.dropped(),
        dropped,
        "the failed report line counted itself as a data drop"
    );

    let path = dir.path().join("report.jsonl");
    let to_file = Stdout::to_file(&path);
    sink.flush();
    drop(to_file);
    drop(stdout);

    let text = fs::read_to_string(&path).expect("the report");
    let line = text.lines().last().expect("a report line");
    let report: serde_json::Value = serde_json::from_str(line).expect("the report is JSON");
    assert_eq!(
        report["fields"]["message"].as_str(),
        Some("log lines dropped")
    );
    assert_eq!(
        report["fields"]["dropped"].as_u64(),
        Some(dropped),
        "the report after the refusal names a smaller loss than happened"
    );
}

fn read_later(read: OwnedFd) -> mpsc::Receiver<Vec<u8>> {
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let mut file = File::from(read);
        let mut bytes = Vec::new();
        let _ = file.read_to_end(&mut bytes);
        let _ = sender.send(bytes);
    });
    receiver
}
