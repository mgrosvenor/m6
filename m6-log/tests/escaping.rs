//! Escaping is JSON's: every control byte and metacharacter round-trips
//! through serde_json unchanged, and non-ASCII UTF-8 passes through.

use std::fs;

use m6_log::{Level, Line, Sink};

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
