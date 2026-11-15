//! A line the destination can take waits for the loop's tick, not for a
//! writable edge.
//!
//! This is the property `Sink::wants_write` gates: it must answer true only
//! after a write returned `EAGAIN`, so the loop does not register a
//! destination that is holding buffered lines. Standard output is replaced by
//! a regular file, which never returns `EAGAIN`, so nothing but the tick or an
//! explicit flush can write the line.

mod common;

use std::fs;
use std::time::SystemTime;

use common::Stdout;
use m6_log::Level;

#[test]
fn a_line_waits_for_the_tick_when_stdout_is_not_blocked() {
    let dir = tempfile::tempdir().expect("a directory");
    let path = dir.path().join("batched.jsonl");
    let stdout = Stdout::to_file(&path);
    m6_log::init(Level::Info, None).expect("the logger");

    m6_log::info!("first line");
    assert!(
        !m6_log::stdout_wants_write(),
        "a regular file accepted the bytes, so the loop must not watch it"
    );
    assert_eq!(
        read(&path),
        "",
        "the line was visible before the tick without a blocked destination"
    );

    m6_log::flush();
    assert!(
        read(&path).contains("first line"),
        "the explicit flush did not write the line"
    );

    m6_log::info!("second line");
    assert!(
        !read(&path).contains("second line"),
        "the second line was written before the tick"
    );
    m6_log::tick(SystemTime::now());
    let text = read(&path);
    assert!(
        text.contains("first line") && text.contains("second line"),
        "the tick did not write both lines: {text:?}"
    );

    m6_log::stop().expect("a clean stop");
    drop(stdout);
}

fn read(path: &std::path::Path) -> String {
    fs::read_to_string(path).unwrap_or_default()
}
