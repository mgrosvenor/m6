//! `reopen` moves new lines to the new file after a rotation.
//!
//! This binary has its own `main` (`harness = false` in `Cargo.toml`) so the
//! process runs on one thread from start to finish, as a service does. The
//! check follows the order a rotation tool uses: rename the file, call
//! `log::reopen()` as a loop's `Signal::ReopenLogs` arm does, wait for the
//! path to reappear, then write again. The signal path itself is proved end to
//! end against m6-html by `shutdown_signals.rs`.

use std::path::Path;
use std::time::Duration;

use m6_core::log::{self, Level, Line};
use m6_core::monitoring::LoggingHealth;
use m6_core::testkit::wait::until;

fn lines(path: &Path) -> Vec<String> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

fn write_line(n: u64) {
    let wrote = log::with_analytics(|sink| {
        Line::begin(sink, Level::Info, "reopen").u64("n", n).end();
    });
    assert!(wrote, "the analytics Sink is not open");
    // The loop's one-second tick is what writes the buffer out.
    log::tick(m6_core::platform::wall());
}

fn main() {
    reopen_moves_new_lines_to_the_new_file();
    println!("reopen_moves_new_lines_to_the_new_file ... ok");
}

fn reopen_moves_new_lines_to_the_new_file() {
    let dir = tempfile::tempdir().unwrap();
    let live = dir.path().join("a.ndjson");
    log::init(Level::Info, Some(&live)).expect("init");

    write_line(1);
    assert!(
        until(Duration::from_secs(5), || lines(&live).len() == 1),
        "the first line never reached the file"
    );

    let rotated = dir.path().join("a.ndjson.1");
    std::fs::rename(&live, &rotated).unwrap();
    log::reopen().expect("reopen the analytics file");
    assert!(
        until(Duration::from_secs(5), || live.exists()),
        "the live path never reappeared"
    );

    write_line(2);
    assert!(
        until(Duration::from_secs(5), || lines(&live).len() == 1),
        "the second line never reached the new file"
    );

    let old = lines(&rotated);
    assert_eq!(old.len(), 1);
    assert!(old[0].contains(r#""n":1"#), "{old:?}");
    assert!(lines(&live)[0].contains(r#""n":2"#));
    assert_eq!(log::reopens().successes(), 1);
    let reopen = LoggingHealth::read()
        .reopen
        .expect("this process writes a file");
    assert!(reopen.seconds_since_last_reopen.unwrap() <= 1);
    log::stop().expect("stop the log");
}
