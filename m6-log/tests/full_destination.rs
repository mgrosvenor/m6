//! A full destination never blocks, keeps the buffered lines, and reports the
//! count it dropped once it drains.

#![allow(
    clippy::disallowed_methods,
    reason = "test code: a pipe, a reader thread and a deadline drive the sink from outside"
)]

mod common;

use std::fs::File;
use std::io::Read;
use std::os::fd::OwnedFd;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use common::Stdout;

const LINES: u64 = 100_000;
const PROMPTLY: Duration = Duration::from_secs(2);
const DEADLINE: Duration = Duration::from_secs(5);

#[test]
fn a_full_destination_never_blocks_and_drops_are_counted() {
    let (stdout, read) = Stdout::to_pipe();
    m6_log::init(m6_log::Level::Info, None).expect("the logger");

    let started = Instant::now();
    for n in 0..LINES {
        m6_log::info!(n = n, "to a pipe nobody reads");
    }
    let elapsed = started.elapsed();
    assert!(
        elapsed < PROMPTLY,
        "logging to a full pipe took {elapsed:?}"
    );
    let dropped = m6_log::operational_dropped();
    assert!(dropped > 0, "a full pipe dropped nothing");

    let received = read_later(read);
    m6_log::flush();
    m6_log::stop().expect("a clean stop");
    drop(stdout);

    let text = received.recv_timeout(DEADLINE).expect("the reader's bytes");
    let text = String::from_utf8(text).expect("UTF-8 lines");
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
