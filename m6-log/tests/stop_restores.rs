//! stop writes everything the buffer holds before it returns and puts stdout's
//! flags back as it found them.

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
const DEADLINE: Duration = Duration::from_secs(5);

#[test]
fn stop_writes_everything_and_restores_blocking() {
    let (stdout, read) = Stdout::to_pipe();
    let before = Stdout::nonblocking();
    m6_log::init(m6_log::Level::Info, None).expect("the logger");
    assert!(
        Stdout::nonblocking(),
        "init did not make a pipe stdout non-blocking"
    );

    for n in 0..LINES {
        m6_log::info!(
            n = n,
            "a line about one hundred bytes long, written to a pipe"
        );
    }
    let accepted = m6_log::operational_lines();
    let dropped = m6_log::operational_dropped();
    assert!(dropped > 0, "the pipe did not fill");

    let received = read_later(read);
    let started = Instant::now();
    m6_log::stop().expect("a clean stop");
    let elapsed = started.elapsed();
    assert!(
        elapsed < DEADLINE,
        "stop took {elapsed:?} with a reader running"
    );
    assert_eq!(
        Stdout::nonblocking(),
        before,
        "stop left stdout's O_NONBLOCK flag changed"
    );
    drop(stdout);

    let bytes = received.recv_timeout(DEADLINE).expect("the reader's bytes");
    let text = String::from_utf8(bytes).expect("UTF-8 lines");
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(
        lines.len() as u64,
        accepted + 1,
        "a buffered line was lost, or the dropped-count line is missing"
    );
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
