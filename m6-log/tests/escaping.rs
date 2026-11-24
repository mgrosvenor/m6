//! Escaping is JSON's: every control byte and metacharacter round-trips
//! through serde_json unchanged, and non-ASCII UTF-8 passes through.

use std::fmt;
use std::fs;

use m6_log::{Level, Line, Sink, FIELD_MAX_BYTES};

#[test]
fn escaping_matches_json() {
    let mut input = String::new();
    for byte in 0u8..=0x1f {
        input.push(char::from(byte));
    }
    input.push('"');
    input.push('\\');
    input.push('/');
    input.push('\u{7f}');
    input.push_str("plain text ");
    input.push('é');
    input.push('☃');
    input.push('😀');

    let dir = tempfile::tempdir().expect("a directory");
    let path = dir.path().join("escaping.jsonl");
    let mut sink = Sink::file(&path).expect("the line file");
    Line::begin(&mut sink, Level::Info, "escaped")
        .str("value", &input)
        .end();
    sink.flush();

    let text = fs::read_to_string(&path).expect("the written line");
    let line = text
        .strip_suffix('\n')
        .expect("one newline-terminated line");
    assert!(!line.contains('\n'), "{text}");
    assert!(
        !line.bytes().any(|byte| byte < 0x20),
        "a raw control byte survived: {line}"
    );
    let value: serde_json::Value = serde_json::from_str(line).expect("valid JSON");
    assert_eq!(
        value["fields"]["value"].as_str(),
        Some(input.as_str()),
        "the decoded value differs"
    );
}

/// A `Display` whose multi-byte character crosses the `log_field` maximum:
/// the line is refused whole, with no cut prefix written.
struct CrossesTheCap;

impl fmt::Display for CrossesTheCap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&"a".repeat(FIELD_MAX_BYTES - 1))?;
        f.write_str("é")?;
        f.write_str("abc")
    }
}

impl fmt::Debug for CrossesTheCap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

#[test]
fn display_and_debug_past_the_cap_refuse_the_line() {
    let dir = tempfile::tempdir().expect("a directory");
    let path = dir.path().join("cut.jsonl");
    let mut sink = Sink::file(&path).expect("the line file");
    Line::begin(&mut sink, Level::Info, "cut")
        .display("displayed", &CrossesTheCap)
        .end();
    Line::begin(&mut sink, Level::Info, "cut")
        .debug("debugged", &CrossesTheCap)
        .end();
    sink.flush();

    assert_eq!(sink.dropped(), 2, "both lines are refused");
    // Only the dropped-count line is written, never a cut prefix.
    let text = fs::read_to_string(&path).unwrap_or_default();
    assert!(!text.contains("aaaa"), "a refused line was written");
    assert!(text.contains("log lines dropped"), "{text:.200}");
}

#[test]
fn a_field_at_the_cap_is_written_whole() {
    let dir = tempfile::tempdir().expect("a directory");
    let path = dir.path().join("cap.jsonl");
    let at_cap = "x".repeat(FIELD_MAX_BYTES);
    let mut sink = Sink::file(&path).expect("the line file");
    Line::begin(&mut sink, Level::Info, "cap")
        .str("value", &at_cap)
        .end();
    sink.flush();

    let text = fs::read_to_string(&path).expect("the written line");
    let value: serde_json::Value = serde_json::from_str(text.trim()).expect("valid JSON");
    let field = value["fields"]["value"].as_str().expect("a string field");
    assert_eq!(field, at_cap, "a field at the maximum is not cut");
}

/// One byte past the `log_field` maximum refuses the whole line: nothing is
/// written, the line is counted as refused, and the next line is unharmed.
#[test]
fn a_field_past_the_cap_refuses_the_line() {
    let dir = tempfile::tempdir().expect("a directory");
    let path = dir.path().join("past.jsonl");
    let mut sink = Sink::file(&path).expect("the line file");
    Line::begin(&mut sink, Level::Info, "past")
        .str("value", &"x".repeat(FIELD_MAX_BYTES + 1))
        .end();
    Line::begin(&mut sink, Level::Info, "after")
        .str("value", "fine")
        .end();
    sink.flush();

    assert_eq!(sink.dropped(), 1, "the refused line is counted");
    let text = fs::read_to_string(&path).expect("the written file");
    assert!(!text.contains("past"), "a refused line reached the file");
    assert!(text.contains("after"), "the next line was lost: {text}");
}
