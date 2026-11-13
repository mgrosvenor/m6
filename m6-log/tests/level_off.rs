//! A line below the configured level builds nothing: its arguments are not
//! formatted.

#![allow(
    clippy::disallowed_methods,
    reason = "test code: a second thread is the only way to prove the cross-thread guard"
)]

mod common;

use std::fmt;
use std::fs;

use common::Stdout;

struct Boom;

impl fmt::Display for Boom {
    fn fmt(&self, _: &mut fmt::Formatter<'_>) -> fmt::Result {
        panic!("the value of a line below the level was formatted");
    }
}

#[test]
fn level_off_builds_nothing() {
    let dir = tempfile::tempdir().expect("a directory");
    let path = dir.path().join("operational.jsonl");
    let stdout = Stdout::to_file(&path);
    m6_log::init(m6_log::Level::Warn, None).expect("the logger");

    m6_log::info!(value = %Boom, "never built");
    m6_log::debug!(value = %Boom, "never built");

    #[cfg(debug_assertions)]
    {
        let other = std::thread::spawn(|| m6_log::warn!("from another thread"));
        assert!(
            other.join().is_err(),
            "a log call from another thread did not trip the debug guard"
        );
    }
    assert!(
        fs::read(&path).expect("the file").is_empty(),
        "a line below the level was written"
    );

    m6_log::warn!(value = "fine", "still built");
    m6_log::stop().expect("a clean stop");
    drop(stdout);
    let text = fs::read_to_string(&path).expect("the file");
    assert_eq!(text.lines().count(), 1, "{text}");
}
