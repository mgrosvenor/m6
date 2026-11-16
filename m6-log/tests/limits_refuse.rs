//! A Sink follows the `log_buffer` and `log_field` limits: its buffer grows
//! by doubling, keeps its size, and refuses a line past the maximum.
//!
//! Its own test binary: `set_limits` is process-wide, and no other test here
//! may see these small maxima.

use std::fs;

use m6_log::{set_limits, Level, Line, LogLimits, Sink};

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
