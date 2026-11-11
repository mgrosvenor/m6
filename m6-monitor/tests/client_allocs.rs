//! The client path allocates nothing, proved with the counting allocator.
//!
//! Run with `--features count-allocations`, which builds this test's binary
//! with the counter installed (`main.rs`). The test waits for a completed
//! poll, sends a thousand requests to the page's stored bytes, and reads the
//! counter the binary prints at stop. The poll itself allocates, which is why
//! no poll runs during the requests and why the binary's line is measured
//! around the client arm only.
//!
//! `/digest` and `/check` are not measured here: their bodies carry the age of
//! the reading at the moment of the request, so they are built per request and
//! allocate. The page's figures never change between polls, so it is served
//! from bytes.

#![allow(
    clippy::disallowed_methods,
    reason = "test code: threads, sleeps and blocking clients drive the service from outside"
)]

use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use m6_core::testkit::{read_one, Service};

const CONFIG: &str = r#"
poll_interval_s = 60

[monitor]
timeout_ms = 100

[[monitor.nodes]]
name = "down"
url  = "http://127.0.0.1:1"
role = "origin"

[log]
level  = "debug"
format = "text"
"#;

fn get(sock: &Path, target: &str) -> (u16, String) {
    let mut s = UnixStream::connect(sock).expect("connect");
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    write!(
        s,
        "GET {target} HTTP/1.1\r\nHost: m6\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let raw = read_one(&mut s, "GET").expect("a response");
    let text = String::from_utf8_lossy(&raw).into_owned();
    let status = text
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    (status, text)
}

#[test]
fn the_client_path_allocates_nothing() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = dir.path().join("monitor.conf");
    std::fs::write(&config, CONFIG).expect("write config");
    std::fs::write(dir.path().join("site.toml"), "").unwrap();
    let sock = dir.path().join("mon.sock");
    let mut svc = Service::spawn(
        "m6-monitor",
        Command::new(env!("CARGO_BIN_EXE_m6-monitor"))
            .arg(dir.path())
            .arg(&config)
            .env("M6_SOCKET_OVERRIDE", &sock),
    );
    svc.wait_for_path(&sock, Duration::from_secs(10));

    // A completed poll is the signal its debug line gives. Waiting for it
    // keeps /digest out of the measured window: that route builds its body per
    // request, on purpose.
    let deadline = Instant::now() + Duration::from_secs(15);
    while !svc.output().contains("fleet poll completed") {
        assert!(
            Instant::now() < deadline,
            "no poll completed\n--- output ---\n{}",
            svc.output()
        );
        std::thread::sleep(Duration::from_millis(20));
    }

    // A thousand requests to the page's stored bytes. Each connection carries
    // one request, so the counted client arm is one turn per request.
    for _ in 0..1000 {
        let (status, _) = get(&sock, "/");
        assert!(
            status == 200 || status == 503,
            "the page answered {status}\n--- output ---\n{}",
            svc.output()
        );
    }

    svc.terminate(Duration::from_secs(10));
    let output = svc.output();
    let line = output
        .lines()
        .find(|l| l.ends_with("allocation(s) in the client arm outside table growth"))
        .unwrap_or_else(|| {
            panic!("the binary printed no allocation line\n--- output ---\n{output}")
        });
    assert_eq!(
        line.trim(),
        "0 allocation(s) in the client arm outside table growth",
        "the client path allocated outside table growth\n--- output ---\n{output}"
    );
}
