//! A warning reaches the destination before the call returns, an info line
//! does not.

mod common;

use std::fs;

use common::Stdout;

#[test]
fn warn_and_error_are_written_at_once() {
    let dir = tempfile::tempdir().expect("a directory");
    let path = dir.path().join("operational.jsonl");
    let stdout = Stdout::to_file(&path);
    m6_log::init(m6_log::Level::Info).expect("the logger");

    m6_log::warn!(reason = "test", "written at once");
    let after_warn = read_lines(&path);
    assert_eq!(after_warn.len(), 1, "the warning was not written at once");
    let warning: serde_json::Value = serde_json::from_str(&after_warn[0]).expect("JSON");
    assert_eq!(warning["level"].as_str(), Some("WARN"));

    m6_log::info!(path = "/", "buffered");
    assert_eq!(
        read_lines(&path).len(),
        1,
        "the info line was written before a flush"
    );

    m6_log::flush();
    let after_flush = read_lines(&path);
    assert_eq!(
        after_flush.len(),
        2,
        "the flush did not write the info line"
    );
    let info: serde_json::Value = serde_json::from_str(&after_flush[1]).expect("JSON");
    assert_eq!(info["level"].as_str(), Some("INFO"));

    m6_log::stop().expect("a clean stop");
    drop(stdout);
}

fn read_lines(path: &std::path::Path) -> Vec<String> {
    match fs::read_to_string(path) {
        Ok(text) => text.lines().map(str::to_string).collect(),
        Err(_) => Vec::new(),
    }
}
