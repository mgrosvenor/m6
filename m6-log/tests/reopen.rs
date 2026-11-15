//! Reopen flushes the old file before it opens the path again, so a rename
//! loses nothing.

use std::fs;

use m6_log::{Level, Line};

#[test]
fn reopen_after_rename() {
    let dir = tempfile::tempdir().expect("a directory");
    let path = dir.path().join("analytics.jsonl");
    let moved = dir.path().join("analytics.jsonl.1");
    m6_log::init(Level::Info, Some(&path)).expect("the logger");

    m6_log::with_analytics(|sink| {
        Line::begin(sink, Level::Info, "one").end();
    });
    fs::rename(&path, &moved).expect("the rename");
    m6_log::reopen().expect("the reopen");
    m6_log::with_analytics(|sink| {
        Line::begin(sink, Level::Info, "two").end();
    });
    m6_log::flush();

    let old = fs::read_to_string(&moved).expect("the renamed file");
    let new = fs::read_to_string(&path).expect("the new file");
    assert!(
        old.contains("\"message\":\"one\""),
        "the line before reopen was lost: {old}"
    );
    assert!(
        !old.contains("\"message\":\"two\""),
        "a line after reopen reached the old file: {old}"
    );
    assert!(
        new.contains("\"message\":\"two\""),
        "the line after reopen is missing: {new}"
    );
    assert!(
        !new.contains("\"message\":\"one\""),
        "the line before reopen reached the new file: {new}"
    );
    m6_log::stop().expect("a clean stop");
}
