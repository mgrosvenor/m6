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
use m6_core::signal::{self, Service, ShutdownHandle};

fn until(timeout: Duration, mut ready: impl FnMut() -> bool) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if ready() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    ready()
}

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
    let _handle = ShutdownHandle::install(Service::new("test"));

    tracing::info!(target: "analytics", n = 1u64);
    assert!(
        until(Duration::from_secs(5), || lines(&live).len() == 1),
        "the first line never reached the file"
    );

    let rotated = dir.path().join("a.ndjson.1");
    std::fs::rename(&live, &rotated).unwrap();
    // SAFETY: `kill` takes a pid and a signal number and touches no memory.
    unsafe { libc::kill(libc::getpid(), libc::SIGUSR1) };
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
    assert!(!signal::is_shutdown());
    assert_eq!(m6_core::log::reopens().successes(), 1);
    let reopen = LoggingHealth::read()
        .reopen
        .expect("this process writes a file");
    assert!(reopen.seconds_since_last_reopen.unwrap() <= 1);
}
