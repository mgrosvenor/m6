//! The timestamp is right across a second boundary even when no tick ran,
//! because the line's own clock refreshes the cached second.

use std::fs;
use std::time::{Duration, UNIX_EPOCH};

use m6_log::{line_at, Level, Sink};

#[test]
fn the_timestamp_is_right_across_a_second() {
    let dir = tempfile::tempdir().expect("a directory");
    let path = dir.path().join("timestamp.jsonl");
    let mut sink = Sink::file(&path).expect("the line file");
    let before =
        UNIX_EPOCH + Duration::from_secs(1_700_000_039) + Duration::from_nanos(999_999_000);
    let after = UNIX_EPOCH + Duration::from_secs(1_700_000_040) + Duration::from_nanos(1_000);
    line_at(before, &mut sink, Level::Info, "before").end();
    line_at(after, &mut sink, Level::Info, "after").end();
    sink.flush();

    let text = fs::read_to_string(&path).expect("the written lines");
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 2, "{text}");
    assert!(
        lines[0].contains("\"timestamp\":\"2023-11-14T22:13:59.999999Z\""),
        "{}",
        lines[0]
    );
    assert!(
        lines[1].contains("\"timestamp\":\"2023-11-14T22:14:00.000001Z\""),
        "{}",
        lines[1]
    );
}
