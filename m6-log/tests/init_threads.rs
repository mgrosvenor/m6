//! `init` may be called on a second thread after the first has finished: the
//! debug guard follows the thread that called it, not the first caller.

#![allow(
    clippy::disallowed_methods,
    reason = "test code: threads are the only way to prove the cross-thread guard"
)]

mod common;

use std::fs;

use common::Stdout;

fn init_and_log(name: &'static str) {
    std::thread::spawn(move || {
        m6_log::init(m6_log::Level::Info, None).expect("the logger");
        m6_log::info!(name = name, "from the thread that called init");
        m6_log::stop().expect("a clean stop");
    })
    .join()
    .expect("a thread that called init may log");
}

#[test]
fn init_on_two_sequential_threads() {
    let dir = tempfile::tempdir().expect("a directory");
    let path = dir.path().join("operational.jsonl");
    let stdout = Stdout::to_file(&path);

    init_and_log("first");
    init_and_log("second");

    // A thread that did not call init is still caught in a debug build.
    #[cfg(debug_assertions)]
    {
        let other = std::thread::spawn(|| m6_log::warn!("from a thread that did not call init"));
        assert!(
            other.join().is_err(),
            "a log call from a thread that did not call init was not caught"
        );
    }

    drop(stdout);
    let text = fs::read_to_string(&path).expect("the file");
    assert_eq!(text.lines().count(), 2, "{text}");
}
