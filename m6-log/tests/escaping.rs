//! Escaping is JSON's: every control byte and metacharacter round-trips
//! through serde_json unchanged, and non-ASCII UTF-8 passes through.

use std::fmt;
use std::fs;

use m6_log::{Level, Line, Sink, MAX_FIELD};

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

/// A `Display` that crosses the cut in the middle of a multi-byte character:
/// the field must be a valid prefix of the value, never the skipped character
/// followed by later text.
struct CutOnACharacter;

impl fmt::Display for CutOnACharacter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&"a".repeat(1020))?;
        f.write_str("é")?;
        f.write_str("abc")
    }
}

impl fmt::Debug for CutOnACharacter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

#[test]
fn display_and_debug_cut_on_a_character_boundary() {
    let dir = tempfile::tempdir().expect("a directory");
    let path = dir.path().join("cut.jsonl");
    let mut sink = Sink::file(&path).expect("the line file");
    Line::begin(&mut sink, Level::Info, "cut")
        .display("displayed", &CutOnACharacter)
        .debug("debugged", &CutOnACharacter)
        .end();
    sink.flush();

    let text = fs::read_to_string(&path).expect("the written line");
    let value: serde_json::Value = serde_json::from_str(text.trim()).expect("valid JSON");
    let expected = format!("{}...", "a".repeat(1020));
    assert_eq!(
        value["fields"]["displayed"].as_str(),
        Some(expected.as_str())
    );
    assert_eq!(
        value["fields"]["debugged"].as_str(),
        Some(expected.as_str())
    );
}

#[test]
fn keys_and_targets_are_escaped() {
    let dir = tempfile::tempdir().expect("a directory");
    let path = dir.path().join("target.jsonl");
    let key = "key\"with\ncontrol";
    let target = "target\\with\"quote\n";
    let mut sink = Sink::file(&path).expect("the line file");
    Line::begin(&mut sink, Level::Info, "escaped")
        .str(key, "value")
        .target(target)
        .end();
    sink.flush();

    let text = fs::read_to_string(&path).expect("the written line");
    assert!(
        !text.chars().any(|c| c.is_control() && c != '\n'),
        "a raw control byte reached the line: {text}"
    );
    let value: serde_json::Value = serde_json::from_str(text.trim()).expect("valid JSON");
    assert_eq!(value["fields"][key].as_str(), Some("value"));
    assert_eq!(value["target"].as_str(), Some(target));
}

#[test]
fn a_field_at_the_cap_is_valid_json() {
    let dir = tempfile::tempdir().expect("a directory");
    let path = dir.path().join("cap.jsonl");
    let at_cap = "x".repeat(MAX_FIELD);
    let mut sink = Sink::file(&path).expect("the line file");
    Line::begin(&mut sink, Level::Info, "cap")
        .str("value", &at_cap)
        .end();
    sink.flush();

    let text = fs::read_to_string(&path).expect("the written line");
    let value: serde_json::Value = serde_json::from_str(text.trim()).expect("valid JSON");
    let field = value["fields"]["value"].as_str().expect("a string field");
    assert!(field.ends_with("..."), "the cut marker is missing");
    assert_eq!(field.len(), MAX_FIELD, "the cut field is not the cap");
    assert_eq!(&field[..MAX_FIELD - 3], "x".repeat(MAX_FIELD - 3));
}
