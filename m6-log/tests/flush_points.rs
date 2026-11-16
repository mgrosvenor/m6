//! A 16 KiB buffer writes when the next line would not fit, on the tick and
//! at stop, and a line is never split.

use std::fs;
use std::time::SystemTime;

use m6_log::{with_analytics, Level, Line, Sink, BUFFER_BYTES, BUFFER_MAX_BYTES, FIELD_MAX_BYTES};

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

/// The widest line the known per-request shape can build: every string field
/// at the cap. It is larger than the buffer's start, so the buffer grows to
/// take it, and the line is written whole.
#[test]
fn a_line_of_maximum_fields_fits_the_buffer() {
    let dir = tempfile::tempdir().expect("a directory");
    let path = dir.path().join("widest.jsonl");
    let mut sink = Sink::file(&path).expect("the line file");
    let worst = "x".repeat(FIELD_MAX_BYTES);
    let keys = [
        "node",
        "path",
        "referer_host",
        "referer_path",
        "user_agent",
        "cache_state",
        "client_ip",
        "session_id",
        "bot_reason",
    ];

    let mut line = Line::begin(&mut sink, Level::Info, "request");
    for key in keys {
        line = line.str(key, &worst);
    }
    line.end();
    sink.flush();

    assert_eq!(sink.dropped(), 0, "the widest known line was dropped");
    assert_eq!(sink.lines(), 1, "the widest known line was not accepted");
    let text = fs::read_to_string(&path).expect("the written line");
    assert_eq!(
        text.lines().count(),
        1,
        "the widest line was not written whole"
    );
    assert!(
        text.len() > BUFFER_BYTES && text.len() <= BUFFER_MAX_BYTES,
        "the widest line is {} bytes, not between the start and the maximum",
        text.len()
    );
    assert!(sink.buffer_size() > BUFFER_BYTES, "the buffer did not grow");
    let value: serde_json::Value = serde_json::from_str(text.trim()).expect("valid JSON");
    for key in keys {
        let field = value["fields"][key].as_str().expect("a string field");
        assert_eq!(field.len(), FIELD_MAX_BYTES, "{key} was cut");
    }
}
