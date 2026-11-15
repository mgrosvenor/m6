//! H7, H8 and H9 of the platform contract, in a test binary of their own.
//!
//! Each counts this process's threads or descriptors, so nothing else may
//! start a thread or open a descriptor while it runs. One `#[test]` runs the
//! three in sequence, and the binary holds no other test.

#![allow(
    clippy::disallowed_methods,
    reason = "test code: a helper thread and a settle pause are the thing counted"
)]

use std::time::{Duration, Instant};

use m6_platform::{host, process};

/// The thread count once it has stopped changing: a thread that just ended is
/// still being torn down when `join` returns, on every supported target, so a
/// count read at once can be one high. Two readings 20 ms apart must agree,
/// within a deadline.
fn settled_threads(pid: u32) -> usize {
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut last = process::thread_count(pid).expect("threads");
    loop {
        std::thread::sleep(Duration::from_millis(20));
        let next = process::thread_count(pid).expect("threads");
        if next == last || Instant::now() > deadline {
            return next;
        }
        last = next;
    }
}

#[test]
fn process_wide_counts_follow_the_process() {
    a_snapshot_starts_no_thread();
    thread_count_follows_the_threads();
    open_descriptors_follow_the_files();
}

/// H7. Break: `sysinfo` with its default features (`multithread` pulls
/// rayon), or a reader that starts a helper thread.
fn a_snapshot_starts_no_thread() {
    let me = std::process::id();
    let before = settled_threads(me);
    for _ in 0..10 {
        let _ = host::snapshot(std::path::Path::new("/"));
    }
    let after = settled_threads(me);
    assert_eq!(before, after, "H7");
}

/// H8. Break: constant.
fn thread_count_follows_the_threads() {
    let me = std::process::id();
    let before = settled_threads(me);
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    let (started_tx, started_rx) = std::sync::mpsc::channel::<()>();
    let t = std::thread::spawn(move || {
        started_tx.send(()).unwrap();
        rx.recv().ok();
    });
    started_rx.recv().unwrap();
    let during = process::thread_count(me).expect("threads");
    assert_eq!(during, before + 1, "H8");
    tx.send(()).unwrap();
    t.join().unwrap();
    assert_eq!(settled_threads(me), before, "H8");
}

/// H9. Break: constant.
fn open_descriptors_follow_the_files() {
    let me = std::process::id();
    let before = process::open_descriptors(me).expect("fds");
    let f = std::fs::File::open("/dev/null").expect("open");
    assert_eq!(
        process::open_descriptors(me).expect("fds"),
        before + 1,
        "H9"
    );
    drop(f);
    assert_eq!(process::open_descriptors(me).expect("fds"), before, "H9");
}
