//! A 16 KiB buffer writes when the next line would not fit, on the tick and
//! at stop, and a line is never split.

use std::fs;
use std::time::SystemTime;

use m6_log::{with_analytics, Level, Line};

const LINES: u64 = 200;
const TAIL: u64 = 10;

#[test]
fn flushes_when_full_on_tick_and_at_stop() {
    let dir = tempfile::tempdir().expect("a directory");
    let path = dir.path().join("points.jsonl");
    m6_log::init(Level::Info, Some(&path)).expect("the logger");

    let value = "0123456789".repeat(12);
    for n in 0..LINES {
        with_analytics(|sink| {
            Line::begin(sink, Level::Info, "flush")
                .u64("n", n)
                .str("value", &value)
                .end();
        });
    }
    let in_file = complete_lines(&path);
    assert!(in_file > 0, "the buffer never wrote when it filled");
    assert!(
        in_file < LINES,
        "every line was written, so the buffer is not a buffer"
    );

    m6_log::tick(SystemTime::now());
    assert_eq!(
        complete_lines(&path),
        LINES,
        "the tick did not flush the buffer"
    );
    m6_log::tick(SystemTime::now());
    assert_eq!(
        complete_lines(&path),
        LINES,
        "a second tick wrote the lines twice"
    );

    for n in LINES..LINES + TAIL {
        with_analytics(|sink| {
            Line::begin(sink, Level::Info, "flush")
                .u64("n", n)
                .str("value", &value)
                .end();
        });
    }
    assert_eq!(
        complete_lines(&path),
        LINES,
        "lines below the buffer's size were written early"
    );

    m6_log::stop().expect("a clean stop");
    assert_eq!(
        complete_lines(&path),
        LINES + TAIL,
        "stop did not write everything"
    );
    for line in fs::read_to_string(&path).expect("the file").lines() {
        let value: serde_json::Value = serde_json::from_str(line).expect("every line is JSON");
        assert_eq!(value["fields"]["message"].as_str(), Some("flush"));
    }
}

fn complete_lines(path: &std::path::Path) -> u64 {
    match fs::read_to_string(path) {
        Ok(text) => text.lines().count() as u64,
        Err(_) => 0,
    }
}
