//! Shutdown signal scenarios, exercised against the real `m6-html` binary.
//!
//! The behaviour under test lives in `m6_core::signal`, shared by every m6
//! service, but m6-html is the smallest service whose suite already starts the
//! binary. On SIGTERM or SIGINT a service stops accepting, removes its socket
//! and exits 0. It does not wait for a request in progress, so each test holds
//! one open and asserts only the exit status, the time taken and the socket.

#![allow(
    clippy::disallowed_methods,
    reason = "test code: threads, sleeps and blocking clients drive the service from outside"
)]

use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use m6_core::testkit::{binary, read_one, Service};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// A running `m6-html`, its socket, and the directory both live in.
///
/// Field order is drop order: the service is killed before the directory
/// holding its socket is removed. The `Service` does the spawn, output drain
/// and signal work; this wrapper only keeps the temp directory alive.
struct Server {
    svc: Service,
    _dir: tempfile::TempDir,
}

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
}

fn config_path(name: &str) -> PathBuf {
    fixtures_dir().join("configs").join(name)
}

/// Spawn `m6-html` on its own socket and wait until it answers a request.
fn spawn(id: &str) -> (Server, PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    // Short name: a unix socket path is capped near 104 bytes on macOS.
    let socket_path = dir.path().join(format!("{id}.sock"));

    let mut svc = Service::spawn(
        "m6-html",
        Command::new(binary("m6-html"))
            .arg(fixtures_dir())
            .arg(config_path("m6-html-shutdown.conf"))
            .env("M6_SOCKET_OVERRIDE", &socket_path),
    );
    svc.wait_for_path(&socket_path, Duration::from_secs(10));

    // Readiness is a real answered request, not the socket file existing:
    // `bind` creates the file before the server can serve.
    let mut ready = false;
    for _ in 0..200 {
        if http_request(
            &socket_path,
            "GET /blog HTTP/1.1\r\nHost: localhost\r\n\r\n",
        )
        .is_some_and(|r| r.contains("200 OK"))
        {
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

/// Send one raw HTTP/1.1 request and return the full response text, or `None`
/// on any I/O trouble.
fn http_request(socket: &Path, request: &str) -> Option<String> {
    let mut stream = UnixStream::connect(socket).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    stream.write_all(request.as_bytes()).ok()?;
    stream.shutdown(std::net::Shutdown::Write).ok();
    let resp = read_one(&mut stream, request.split(' ').next().unwrap_or("GET")).ok()?;
    Some(String::from_utf8_lossy(&resp).into_owned())
}

// ---------------------------------------------------------------------------
// Shutdown signal scenarios
// ---------------------------------------------------------------------------

/// Signal the service while a request is held in a partial body, and require
/// it to exit 0 within a few seconds with its socket removed.
///
/// The client connects and sends a head with ten of its hundred body bytes, so
/// the loop holds one accepted connection that is waiting for the rest. That
/// connection is accepted work when shutdown starts, and the loop drops it
/// without waiting: there is no pool and no wake connection.
fn exits_promptly_with_a_request_in_flight(id: &str) {
    let name = "a shutdown request";
    let (mut server, socket_path) = spawn(id);

    let mut held = UnixStream::connect(&socket_path).expect("connect the held request");
    held.write_all(
        b"POST /blog HTTP/1.1\r\nHost: localhost\r\nContent-Length: 100\r\n\
          Connection: close\r\n\r\n0123456789",
    )
    .expect("send the head of the held request");

    server.svc.request(m6_core::platform::Signal::Shutdown);
    let status = server
        .svc
        .wait_for_exit(Instant::now() + Duration::from_secs(5))
        .unwrap_or_else(|| {
            panic!(
                "m6-html did not exit within 5s of {name} with a request in flight\n\
                 --- output ---\n{}",
                server.svc.output()
            )
        });
    assert!(
        status.success(),
        "{name} must exit 0, got {status}\n--- output ---\n{}",
        server.svc.output()
    );
    assert!(
        !socket_path.exists(),
        "the socket at {} outlived {name}",
        socket_path.display()
    );
    m6_core::testkit::assert_lifecycle_logged("m6-html", &server.svc.output());
    drop(held);
}

/// A shutdown request with a request in flight exits at once. The wrong
/// outcome: the service waits for the request, so a stalled client holds up
/// every deploy. SIGINT takes the same path, which the platform layer's own
/// suite proves (`platform_signals`, S2).
#[test]
fn shutdown_exits_zero_and_removes_the_socket_with_a_request_in_flight() {
    exits_promptly_with_a_request_in_flight("term-inflight");
}

/// SIGUSR1 asks for a log reopen and is not a shutdown. This is the only
/// end-to-end run of a core service's `Signal::ReopenLogs` arm: the service
/// writes the reopen line at once, keeps its socket, and answers the next
/// request. m6-html writes no analytics file, so the line says so.
#[test]
fn a_log_reopen_signal_is_not_a_shutdown() {
    let (mut server, socket_path) = spawn("usr1");
    server.svc.request(m6_core::platform::Signal::ReopenLogs);

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if server.svc.output().contains("log reopen requested") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the reopen line never appeared\n--- output ---\n{}",
            server.svc.output()
        );
        server.svc.assert_alive("waiting for the reopen line");
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        server
            .svc
            .output()
            .contains("log reopen requested; this process writes no log file"),
        "--- output ---\n{}",
        server.svc.output()
    );
    assert!(
        socket_path.exists(),
        "the socket at {} went away on SIGUSR1",
        socket_path.display()
    );
    let response = http_request(
        &socket_path,
        "GET /blog HTTP/1.1\r\nHost: localhost\r\n\r\n",
    )
    .expect("the service still answers after SIGUSR1");
    assert!(
        response.contains("200 OK"),
        "the service stopped serving after SIGUSR1: {response}\n--- output ---\n{}",
        server.svc.output()
    );
    server.svc.terminate(Duration::from_secs(5));
}
