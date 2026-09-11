//! The read timeout on an accepted connection, proven against the real binary.
//!
//! `App` set no read timeout at all, so `serve_connection` blocked in `read`
//! waiting for a request line that a silent peer was never going to send. The
//! worker was held until the peer went away, and with a bounded pool a handful
//! of silent peers is the entire pool. The service answers 503 or nothing while
//! looking healthy: no panic, no error, no log line.
//!
//! These tests are written against the fixture config `m6-html-timeout.conf`,
//! which sets a two-worker pool and a one second deadline. Both fail against an
//! `App` with no timeout: the first hangs until the harness timeout, the second
//! never gets its request answered.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use m6_core::testkit::{binary, Service};

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join("fixtures")
}

struct Server {
    svc: Service,
    _dir: tempfile::TempDir,
}

/// Spawn `m6-html` against the short-timeout config and wait until it answers.
fn spawn_server(id: &str) -> (Server, PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let socket_path = dir.path().join(format!("{id}.sock"));

    let mut svc = Service::spawn(
        "m6-html",
        Command::new(binary("m6-html"))
            .arg(fixtures_dir())
            .arg(fixtures_dir().join("configs").join("m6-html-timeout.conf"))
            .env("M6_SOCKET_OVERRIDE", &socket_path),
    );
    svc.wait_for_path(&socket_path, Duration::from_secs(10));

    let mut ready = false;
    for _ in 0..200 {
        if get_root(&socket_path, Duration::from_secs(5)).is_some() {
            ready = true;
            break;
        }
        svc.assert_alive("waiting for the first answered request");
        std::thread::sleep(Duration::from_millis(25));
    }
    assert!(
        ready,
        "m6-html never answered a request\n--- output ---\n{}",
        svc.output()
    );

    (Server { svc, _dir: dir }, socket_path)
}

/// One `GET /`, returning the response text, or `None` on any I/O trouble.
fn get_root(socket_path: &Path, timeout: Duration) -> Option<String> {
    let mut stream = UnixStream::connect(socket_path).ok()?;
    stream.set_read_timeout(Some(timeout)).ok()?;
    stream
        .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .ok()?;
    stream.shutdown(std::net::Shutdown::Write).ok();
    let bytes = m6_core::testkit::read_one(&mut stream, "GET").ok()?;
    if bytes.is_empty() {
        return None;
    }
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

/// A peer that connects and says nothing is let go, rather than held forever.
///
/// It is let go *silently*. That is the part worth pinning: m6-http keeps
/// backend connections pooled, so a `400` written into an idle socket would sit
/// in its buffer and be read as the response to the next request sent on it.
#[test]
fn a_silent_peer_is_disconnected_without_a_response() {
    let (mut server, socket_path) = spawn_server("silent");

    let mut stream = UnixStream::connect(&socket_path).expect("connect");
    // Comfortably longer than the config's one second, so a read that returns
    // is the server's decision and not this timeout firing.
    stream.set_read_timeout(Some(Duration::from_secs(10))).expect("timeout");

    let started = Instant::now();
    let mut buf = [0u8; 1024];
    let n = stream.read(&mut buf).expect("read should end, not error");
    let elapsed = started.elapsed();

    assert_eq!(
        n,
        0,
        "the server wrote {:?} to a peer that never sent a request; an idle \
         pooled connection must be closed silently",
        String::from_utf8_lossy(&buf[..n])
    );
    assert!(
        elapsed < Duration::from_secs(9),
        "connection was not closed by the server's own deadline (took {elapsed:?})"
    );

    server.svc.assert_alive("after disconnecting a silent peer");
}

/// The outage this exists to prevent: silent peers must not take the pool.
///
/// The config gives two workers and a two-deep queue, so the four connections
/// opened here are the whole of it. Without a deadline every one of those is
/// held until its peer goes away, and a real request is never served.
#[test]
fn silent_peers_do_not_starve_the_thread_pool() {
    let (mut server, socket_path) = spawn_server("starve");

    // Hold these open and silent for the rest of the test.
    let hogs: Vec<UnixStream> = (0..4)
        .filter_map(|_| UnixStream::connect(&socket_path).ok())
        .collect();
    assert_eq!(hogs.len(), 4, "could not open the connections to fill the pool");

    // Give the server a moment to accept them onto its workers and queue.
    std::thread::sleep(Duration::from_millis(200));

    // Recovery is not instant and is not meant to be. The two workers time out
    // after a second and pick up the two queued connections, which time out a
    // second after that. Until then the queue is full and the honest answer is
    // a 503, so a 503 is a retry rather than a result: what is being asserted
    // is that the pool comes back at all, not how fast.
    let started = Instant::now();
    let mut served = None;
    let mut last = String::from("(no response at all)");
    while started.elapsed() < Duration::from_secs(15) {
        if let Some(resp) = get_root(&socket_path, Duration::from_secs(5)) {
            if resp.contains("200 OK") {
                served = Some(resp);
                break;
            }
            last = resp;
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    served.unwrap_or_else(|| {
        panic!(
            "the pool never recovered while four silent peers held it; last \
             answer was:\n{last}\n--- output ---\n{}",
            server.svc.output()
        )
    });

    drop(hogs);
    server.svc.assert_alive("after the pool recovered");
}
