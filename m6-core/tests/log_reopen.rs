//! SIGUSR1 reopens the analytics file, and is not a shutdown.
//!
//! This binary has its own `main` (`harness = false` in `Cargo.toml`) for two
//! reasons. `tracing`'s subscriber is installed once per process, and
//! `signal::block()` must run before any other thread exists, which the libtest
//! harness does not allow. The check follows the order a
//! rotation tool uses: rename the file, send the signal, wait for the path to
//! reappear, then write again.

use std::path::Path;
use std::time::Duration;

use m6_core::monitoring::LoggingHealth;
use m6_core::platform::{Signal, Signals};
use m6_core::signal::{self, Action, Lifecycle};
use m6_core::testkit::wait::until;

fn lines(path: &Path) -> Vec<String> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

fn main() {
    sigusr1_reopens_the_file_and_is_not_a_shutdown();
    println!("sigusr1_reopens_the_file_and_is_not_a_shutdown ... ok");
}

fn sigusr1_reopens_the_file_and_is_not_a_shutdown() {
    signal::block();
    let dir = tempfile::tempdir().unwrap();
    let live = dir.path().join("a.ndjson");
    let _log = m6_core::log::init_with_analytics("json", "info", Some(&live)).expect("init");
    let mut signals = Signals::new().expect("open the signal descriptor");
    let mut lifecycle = Lifecycle::new("test", None);

    tracing::info!(target: "analytics", n = 1u64);
    assert!(
        until(Duration::from_secs(5), || lines(&live).len() == 1),
        "the first line never reached the file"
    );

    let rotated = dir.path().join("a.ndjson.1");
    std::fs::rename(&live, &rotated).unwrap();
    m6_core::platform::send(std::process::id(), Signal::ReopenLogs)
        .expect("send the reload request");
    let mut request = None;
    assert!(
        until(Duration::from_secs(5), || {
            request = signals.read();
            request.is_some()
        }),
        "the reload request never arrived on the signal descriptor"
    );
    let action = lifecycle.on_signal(request.expect("a request"));
    assert!(
        until(Duration::from_secs(5), || live.exists()),
        "the live path never reappeared"
    );

    tracing::info!(target: "analytics", n = 2u64);
    assert!(
        until(Duration::from_secs(5), || lines(&live).len() == 1),
        "the second line never reached the new file"
    );

    let old = lines(&rotated);
    assert_eq!(old.len(), 1);
    assert!(old[0].contains(r#""n":1"#), "{old:?}");
    assert!(lines(&live)[0].contains(r#""n":2"#));
    assert_eq!(action, Action::Continue, "SIGUSR1 is not a shutdown");
    assert_eq!(m6_core::log::reopens().successes(), 1);
    let reopen = LoggingHealth::read()
        .reopen
        .expect("this process writes a file");
    assert!(reopen.seconds_since_last_reopen.unwrap() <= 1);
}
