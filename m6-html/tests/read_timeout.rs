//! The read deadline on an accepted connection, proven against the real binary.
//!
//! A peer that connects and says nothing holds a connection slot, never a
//! thread, so it can no longer starve a worker pool. The deadline is what lets
//! the slot go: the loop closes the connection when `Timeouts` reports its read
//! deadline passed, with no response, and logs nothing, because a `400` written
//! into an idle pooled connection would be read as the answer to the next
//! request sent on it.
//!
//! These tests are written against the fixture config `m6-html-timeout.conf`,
//! which sets a three second deadline. Its `[thread_pool]` table is inert: the
//! service reads no pool, and EL-6d refuses the key.

#![allow(
    clippy::disallowed_methods,
    reason = "test code: threads, sleeps and blocking clients drive the service from outside"
)]

use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use m6_core::testkit::{binary, Service};

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
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

    let ready = m6_core::testkit::wait::until(Duration::from_secs(10), || {
        get_root(&socket_path, Duration::from_secs(5)).is_some()
    });
    assert!(
        ready,
        "m6-html never answered a request\n--- output ---\n{}",
        svc.output()
    );
    svc.assert_alive("after the first answered request");

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
    // Comfortably longer than the config's three seconds, so a read that returns
    // is the server's decision and not this timeout firing.
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("timeout");

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

/// Silent peers hold connection slots, and a slot never stops the loop.
///
/// The old shape filled a two-worker, two-deep pool and answered a real request
/// with a `503` until the pool drained. There is no pool now, and no queue a
/// peer can fill: this opens 64 connections (more than the old pool held) and
/// requires a real request on another connection to be answered within 2 s,
/// while all 64 are still open. A service that served one connection to
/// completion before accepting the next fails the deadline; a service that
/// answered the 64 with a refusal fails the last loop.
#[test]
fn silent_peers_do_not_block_other_connections() {
    let (mut server, socket_path) = spawn_server("silent-peers");

    let mut silent: Vec<UnixStream> = (0..64)
        .filter_map(|_| UnixStream::connect(&socket_path).ok())
        .collect();
    assert_eq!(
        silent.len(),
        64,
        "could not open the silent connections\n--- output ---\n{}",
        server.svc.output()
    );

    // One request on a connection of its own, with the config's three second
    // deadline still ahead of the 64 silent peers.
    let started = Instant::now();
    let response = get_root(&socket_path, Duration::from_secs(2)).unwrap_or_else(|| {
        panic!(
            "a real request was not answered within 2 s while 64 silent connections were open\n\
             --- output ---\n{}",
            server.svc.output()
        )
    });
    assert!(
        response.contains("200 OK"),
        "the answer was not a served page: {}",
        &response[..response.len().min(200)]
    );
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "the answer took {:?}, beyond the 2 s deadline",
        started.elapsed()
    );

    // All 64 are still open, and none has been sent anything. A response here
    // would be the old pool's 503; a close would mean the loop served them to
    // completion rather than holding them. Nothing can be waiting to read from
    // a peer that never sent a request, so a one-byte non-blocking read asks
    // the question without waiting.
    for (i, stream) in silent.iter_mut().enumerate() {
        stream
            .set_nonblocking(true)
            .expect("set the silent connection non-blocking");
        let mut byte = [0u8; 1];
        match stream.read(&mut byte) {
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
            Err(e) => panic!("silent connection {i} failed: {e}"),
            Ok(0) => panic!("silent connection {i} was closed by the server"),
            Ok(n) => panic!(
                "silent connection {i} was sent {:?}",
                String::from_utf8_lossy(&byte[..n])
            ),
        }
    }

    server
        .svc
        .assert_alive("after answering beside 64 silent connections");
}
