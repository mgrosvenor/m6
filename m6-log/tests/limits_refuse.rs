//! A Sink follows the `log_buffer` and `log_field` limits: its buffer grows
//! by doubling, keeps its size, and refuses a line past the maximum.
//!
//! Its own test binary: `set_limits` is process-wide, and no other test here
//! may see these small maxima.

use std::fs;

use m6_log::{set_limits, Level, Line, LogLimits, Sink, Type};

fn limits() -> LogLimits {
    LogLimits {
        buffer_start: 4096,
        buffer_max: 8192,
        field_max: 6000,
        file: "/etc/m6/site.toml".to_string(),
    }
}

#[test]
fn the_buffer_grows_keeps_its_size_and_refuses_past_the_maximum() {
    set_limits(limits());
    let dir = tempfile::tempdir().expect("a directory");
    let path = dir.path().join("lines.jsonl");
    let mut sink = Sink::file(&path).expect("the line file");
    assert_eq!(sink.buffer_size(), 4096);

    // A 5 KiB line does not fit the start: the buffer doubles.
    Line::begin(&mut sink, Level::Info, "grow")
        .str("value", &"x".repeat(5000))
        .end();
    assert_eq!(sink.buffer_size(), 8192);
    sink.flush();
    assert_eq!(sink.lines(), 1);

    // It keeps the size for small lines.
    Line::begin(&mut sink, Level::Info, "small").end();
    sink.flush();
    assert_eq!(sink.buffer_size(), 8192, "the buffer never shrinks");

    // Two fields of 5 KiB make a line past the 8 KiB maximum: refused whole.
    Line::begin(&mut sink, Level::Info, "refused")
        .str("a", &"x".repeat(5000))
        .str("b", &"y".repeat(5000))
        .end();
    sink.flush();
    assert_eq!(sink.dropped(), 1);
    assert_eq!(sink.buffer_size(), 8192);
    let text = fs::read_to_string(&path).expect("the written file");
    assert!(!text.contains("refused"), "a refused line reached the file");
    assert!(text.contains("small"));
}

#[test]
fn each_refusal_names_its_key_and_file() {
    let l = limits();
    assert_eq!(
        l.buffer_refusal(9000),
        "m6-log: the log buffer needs at least 9000 B, over [limits.log_buffer] max = 8 KiB in /etc/m6/site.toml; raise max to fix this. A log line was refused."
    );
    assert_eq!(
        l.field_refusal(),
        "m6-log: a log field needs at least 6001 B, over [limits.log_field] max = 6000 B in /etc/m6/site.toml; raise max to fix this. A log line was refused."
    );
}

/// A nested value is bounded by the buffer, not by the string-field cap. A
/// 7,000-byte document fits an 8 KiB line and its field is over the 6,000-byte
/// `log_field` limit, which applies to string fields alone.
#[test]
fn a_nested_value_is_not_cut_by_the_string_field_limit() {
    set_limits(limits());
    let dir = tempfile::tempdir().expect("a directory");
    let path = dir.path().join("perf.jsonl");
    let mut sink = Sink::file(&path).expect("the line file");

    let long = "x".repeat(7000);
    let document = serde_json::json!({"v": long, "n": 7});
    Line::begin(&mut sink, Level::Info, "stats")
        .kind(Type::Perf)
        .json("perf", &document)
        .end();
    sink.flush();
    assert_eq!(sink.dropped(), 0, "the line was refused, not written");

    let text = fs::read_to_string(&path).expect("the written file");
    let value: serde_json::Value = serde_json::from_str(text.trim()).expect("one JSON line");
    assert_eq!(value["fields"]["perf"]["v"].as_str().unwrap().len(), 7000);
    assert_eq!(value["type"], "perf");
}

/// A nested value the buffer cannot hold refuses the whole line: no half
/// document reaches the file, and the line counts as dropped.
#[test]
fn a_nested_value_past_the_buffer_refuses_the_whole_line() {
    set_limits(limits());
    let dir = tempfile::tempdir().expect("a directory");
    let path = dir.path().join("perf.jsonl");
    let mut sink = Sink::file(&path).expect("the line file");

    // About 12 KB of digits, past the 8 KiB maximum and under no field cap.
    let numbers: Vec<u64> = (0..2000).collect();
    let document = serde_json::json!({ "counts": numbers });
    Line::begin(&mut sink, Level::Info, "stats")
        .kind(Type::Perf)
        .json("perf", &document)
        .end();
    sink.flush();
    assert_eq!(sink.dropped(), 1);
    // The next flush writes the dropped-count line, which is the only line in
    // the file: the refused document is not there in any part.
    let text = fs::read_to_string(&path).expect("the written file");
    for line in text.lines() {
        let value: serde_json::Value = serde_json::from_str(line).expect("one JSON line");
        assert_ne!(
            value["fields"]["message"], "stats",
            "a refused line reached the file"
        );
    }
    assert!(
        text.contains("log lines dropped"),
        "the loss was not reported: {text}"
    );
}
