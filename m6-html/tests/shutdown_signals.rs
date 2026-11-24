//! Shutdown signal scenarios, exercised against the real `m6-html` binary.
//!
//! On SIGTERM or SIGINT a service stops accepting, answers the requests it has
//! already accepted, up to `[server] drain_timeout_s`, removes its socket and
//! exits 0. The count that makes that wait correct is taken at the accept, not
//! when a turn reads the request, which is the defect issue #338 measured
//! (57 failures in 150 loaded runs before the count moved to the accept).
//!
//! SIGINT takes the same path, which the platform layer's own suite proves
//! (`platform_signals`, S2), so these tests signal SIGTERM.

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
fn spawn_with(id: &str, config: &str) -> (Server, PathBuf) {
    let dir = tempfile::tempdir_in(m6_core::testkit::socket_scratch_root()).expect("tempdir");
    // Short name: a unix socket path is capped near 104 bytes on macOS.
    let socket_path = dir.path().join(format!("{id}.sock"));

    let mut svc = Service::spawn(
        "m6-html",
        Command::new(binary("m6-html"))
            .arg(fixtures_dir())
            .arg(config_path(config))
            .env("M6_SOCKET_OVERRIDE", &socket_path),
    );
    svc.wait_for_path(&socket_path, m6_core::testkit::STARTUP_BOUND);
    svc.wait_for_started(m6_core::testkit::STARTUP_BOUND);

    // Readiness is a real answered request, not the socket file existing:
    // `bind` creates the file before the server can serve.
    let mut ready = false;
    for _ in 0..200 {
        if http_request(
            &socket_path,
            "GET /blog HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
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

fn spawn(id: &str) -> (Server, PathBuf) {
    spawn_with(id, "m6-html-shutdown.conf")
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

/// A connection the service has accepted, held open with a request whose body
/// is not finished.
///
/// The held connection is made before a probe, and the service accepts in
/// order, so the probe's answered response proves the held connection was
/// accepted. That is the state the drain must wait for: accepted, and not yet
/// answered.
fn hold_open(socket: &Path, svc: &mut Service) -> UnixStream {
    let head = "POST /blog HTTP/1.1\r\nHost: localhost\r\nContent-Length: 100\r\nConnection: close\r\n\r\n0123456789";
    let mut held = UnixStream::connect(socket).expect("connect the held request");
    held.set_read_timeout(Some(Duration::from_secs(10))).ok();
    held.write_all(head.as_bytes())
        .expect("send the head of the held request");

    let probe = http_request(
        socket,
        "GET /blog HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    );
    assert!(
        probe.is_some_and(|r| r.contains("200 OK")),
        "the probe was not answered, so the held connection was not accepted\n\
         --- output ---\n{}",
        svc.output()
    );
    held
}

/// Wait until the child's output contains `marker`, or panic with the output.
fn wait_for_output(svc: &Service, marker: &str, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        if svc.output().contains(marker) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "m6-html never logged {marker:?} within {timeout:?}\n--- output ---\n{}",
            svc.output()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

// ---------------------------------------------------------------------------
// Shutdown signal scenarios
// ---------------------------------------------------------------------------

/// With nothing accepted, shutdown is immediate. The wrong outcome: the loop
/// waits out the drain deadline with no work, so every deploy and every unit
/// restart takes 30 s longer than it should.
#[test]
fn shutdown_with_nothing_in_flight_is_immediate() {
    let (mut server, socket_path) = spawn("term-idle");
    let sent = Instant::now();
    server.svc.request(m6_core::platform::Signal::Shutdown);
    let status = server
        .svc
        .wait_for_exit(sent + Duration::from_secs(2))
        .unwrap_or_else(|| {
            panic!(
                "m6-html waited with nothing in flight\n--- output ---\n{}",
                server.svc.output()
            )
        });
    assert!(
        status.success(),
        "an immediate shutdown must exit 0, got {status}\n--- output ---\n{}",
        server.svc.output()
    );
    assert!(
        !socket_path.exists(),
        "the socket at {} outlived the shutdown",
        socket_path.display()
    );
    m6_core::testkit::assert_lifecycle_logged("m6-html", &server.svc.output());
}

/// A connection that has answered its request and holds nothing is idle, so
/// it does not hold the drain. The wrong outcome: every deploy waits out the
/// drain deadline behind a keep-alive client that is doing nothing.
#[test]
fn an_idle_keep_alive_connection_does_not_hold_shutdown() {
    let (mut server, socket_path) = spawn("term-idle-keepalive");
    // One kept-alive request, answered, with the socket left open.
    let mut idle = UnixStream::connect(&socket_path).expect("connect the idle client");
    idle.set_read_timeout(Some(Duration::from_secs(10))).ok();
    idle.write_all(b"GET /blog HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .expect("send the kept-alive request");
    let resp = read_one(&mut idle, "GET").expect("read the kept-alive response");
    assert!(
        String::from_utf8_lossy(&resp).contains("200 OK"),
        "the kept-alive request was not answered: {}",
        String::from_utf8_lossy(&resp)
    );

    let sent = Instant::now();
    server.svc.request(m6_core::platform::Signal::Shutdown);
    let status = server
        .svc
        .wait_for_exit(sent + Duration::from_secs(2))
        .unwrap_or_else(|| {
            panic!(
                "m6-html waited behind an idle keep-alive connection\n--- output ---\n{}",
                server.svc.output()
            )
        });
    assert!(
        status.success(),
        "an idle keep-alive shutdown must exit 0, got {status}\n--- output ---\n{}",
        server.svc.output()
    );
    assert!(
        !socket_path.exists(),
        "the socket at {} outlived the shutdown",
        socket_path.display()
    );
    drop(idle);
}

/// A request held open when one SIGTERM arrives receives its complete response
/// before the process exits. The wrong outcome: shutdown exits without
/// draining, cutting the response short.
#[test]
fn a_request_in_flight_at_sigterm_receives_its_full_response() {
    let (mut server, socket_path) = spawn("term-inflight");
    let mut held = hold_open(&socket_path, &mut server.svc);

    server.svc.request(m6_core::platform::Signal::Shutdown);
    wait_for_output(
        &server.svc,
        "shutdown signal received",
        Duration::from_secs(5),
    );

    // While the request is in flight the drain must keep the process alive. An
    // exit within this window means shutdown did not wait for the request.
    if let Some(status) = server
        .svc
        .wait_for_exit(Instant::now() + Duration::from_millis(500))
    {
        panic!(
            "m6-html exited ({status}) while the request was still in flight; \
             shutdown did not drain\n--- output ---\n{}",
            server.svc.output()
        );
    }

    // Finish the body. The response must arrive whole before the process exits.
    held.write_all(&[b'x'; 90])
        .expect("finish the held request body");
    let resp = read_one(&mut held, "POST").expect("read the held response");
    let text = String::from_utf8_lossy(&resp).into_owned();
    assert!(
        text.contains("200 OK"),
        "expected a complete 200 response, got: {}",
        &text[..text.len().min(300)]
    );
    assert!(
        text.contains("Posts"),
        "the response was cut short; expected post-index content, got: {}",
        &text[..text.len().min(300)]
    );

    let status = server
        .svc
        .wait_for_exit(Instant::now() + Duration::from_secs(5))
        .unwrap_or_else(|| {
            panic!(
                "m6-html did not exit after draining\n--- output ---\n{}",
                server.svc.output()
            )
        });
    assert!(
        status.success(),
        "a drained shutdown must exit 0, got {status}\n--- output ---\n{}",
        server.svc.output()
    );
    assert!(
        !socket_path.exists(),
        "the socket at {} outlived the shutdown",
        socket_path.display()
    );
}

/// A keep-alive connection that has answered one request and has the next
/// request already in the kernel's receive buffer when SIGTERM lands is
/// answered, not closed.
///
/// The count is per accepted connection, and a connection that has answered a
/// request is released, so the count alone cannot see the request waiting in
/// its socket. The drain gives the loop one non-blocking turn after the stop,
/// which is what reads it.
#[test]
fn a_kept_alive_request_waiting_at_sigterm_is_answered() {
    let (mut server, socket_path) = spawn("term-kept-alive");
    let mut held = UnixStream::connect(&socket_path).expect("connect the kept-alive request");
    held.set_read_timeout(Some(Duration::from_secs(10))).ok();

    // First request, answered; the connection stays open.
    held.write_all(b"GET /blog HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .expect("send the first request");
    let first = read_one(&mut held, "GET").expect("read the first answer");
    assert!(
        String::from_utf8_lossy(&first).contains("200 OK"),
        "the first request was not answered\n--- output ---\n{}",
        server.svc.output()
    );

    let pid = server.svc.pid().to_string();
    let stop = std::process::Command::new("kill")
        .args(["-STOP", &pid])
        .status()
        .expect("stop the service");
    assert!(stop.success(), "kill -STOP");
    std::thread::sleep(Duration::from_millis(100));

    // The stop lands first, then the second request is written into the
    // stopped process's socket buffer. Both are pending when it resumes.
    server.svc.request(m6_core::platform::Signal::Shutdown);
    held.write_all(b"GET /blog HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .expect("send the request while stopped");
    std::thread::sleep(Duration::from_millis(100));
    let cont = std::process::Command::new("kill")
        .args(["-CONT", &pid])
        .status()
        .expect("continue the service");
    assert!(cont.success(), "kill -CONT");

    let second = read_one(&mut held, "GET")
        .expect("the request waiting at SIGTERM must be answered, not closed");
    let text = String::from_utf8_lossy(&second).into_owned();
    assert!(
        text.contains("200 OK") && text.contains("Posts"),
        "the waiting request was not answered whole: {}",
        &text[..text.len().min(300)]
    );

    let status = server
        .svc
        .wait_for_exit(Instant::now() + Duration::from_secs(5))
        .unwrap_or_else(|| {
            panic!(
                "m6-html did not exit after draining the kept-alive request\n--- output ---\n{}",
                server.svc.output()
            )
        });
    assert!(
        status.success(),
        "a drained shutdown must exit 0, got {status}\n--- output ---\n{}",
        server.svc.output()
    );
}

/// A connection the listener accepted, with no request read yet, is counted as
/// accepted work: a shutdown that arrives before the client sends anything
/// still waits, and the request sent afterwards is answered.
///
/// This is the case the count exists for. The wrong outcome: the count rises
/// when the request is dispatched, so the drain sees nothing in flight, closes
/// the listener and exits under the connection.
#[test]
fn a_request_accepted_but_not_yet_dispatched_is_counted() {
    let (mut server, socket_path) = spawn("term-accepted");
    // Connected and silent: the listener accepted it, and no turn has read a
    // request from it.
    let mut held = UnixStream::connect(&socket_path).expect("connect the silent request");
    held.set_read_timeout(Some(Duration::from_secs(10))).ok();
    let probe = http_request(
        &socket_path,
        "GET /blog HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    );
    assert!(
        probe.is_some_and(|r| r.contains("200 OK")),
        "the probe was not answered, so the silent connection was not accepted\n\
         --- output ---\n{}",
        server.svc.output()
    );

    server.svc.request(m6_core::platform::Signal::Shutdown);
    wait_for_output(
        &server.svc,
        "shutdown signal received",
        Duration::from_secs(5),
    );

    // The request arrives after the stop. If the accepted connection was not
    // counted, the process has already exited and this write or read fails.
    std::thread::sleep(Duration::from_millis(300));
    if let Some(status) = server
        .svc
        .wait_for_exit(Instant::now() + Duration::from_millis(1))
    {
        panic!(
            "m6-html exited ({status}) with an accepted connection counted nowhere\n\
             --- output ---\n{}",
            server.svc.output()
        );
    }
    held.write_all(b"GET /blog HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .expect("send the request on the accepted connection");
    let resp = read_one(&mut held, "GET").expect("read the answer on the accepted connection");
    let text = String::from_utf8_lossy(&resp).into_owned();
    assert!(
        text.contains("200 OK"),
        "the accepted request was not answered, got: {}",
        &text[..text.len().min(300)]
    );

    let status = server
        .svc
        .wait_for_exit(Instant::now() + Duration::from_secs(5))
        .unwrap_or_else(|| {
            panic!(
                "m6-html did not exit after answering the accepted request\n\
                 --- output ---\n{}",
                server.svc.output()
            )
        });
    assert!(
        status.success(),
        "a drained shutdown must exit 0, got {status}\n--- output ---\n{}",
        server.svc.output()
    );
}

/// A request that never finishes cannot hold shutdown past `drain_timeout_s`.
/// The wrong outcome: the drain waits for the stalled request until the
/// supervisor kills the service mid-response. The service logs how many
/// requests it left behind, then exits 0 with its socket removed.
#[test]
fn a_stalled_request_cannot_hold_shutdown_past_the_drain_deadline() {
    let (mut server, socket_path) =
        spawn_with("term-deadline", "m6-html-shutdown-short-drain.conf");
    let held = hold_open(&socket_path, &mut server.svc);

    server.svc.request(m6_core::platform::Signal::Shutdown);
    wait_for_output(
        &server.svc,
        "drain deadline passed",
        Duration::from_secs(10),
    );
    let status = server
        .svc
        .wait_for_exit(Instant::now() + Duration::from_secs(10))
        .unwrap_or_else(|| {
            panic!(
                "m6-html did not exit after the drain deadline\n--- output ---\n{}",
                server.svc.output()
            )
        });
    assert!(
        status.success(),
        "shutdown past the drain deadline must exit 0, got {status}\n--- output ---\n{}",
        server.svc.output()
    );
    assert!(
        server.svc.output().contains("\"unfinished\":1"),
        "the warning must count the one request left behind\n--- output ---\n{}",
        server.svc.output()
    );
    assert!(
        !socket_path.exists(),
        "the socket at {} outlived the shutdown",
        socket_path.display()
    );
    drop(held);
}

/// A second SIGTERM while a request holds the drain open forces an immediate
/// exit. The wrong outcome: shutdown waits for the slow request, so an
/// operator who signals twice gets no faster stop.
#[test]
fn a_second_sigterm_forces_exit_without_waiting_for_the_request() {
    let (mut server, socket_path) = spawn("term-twice");
    let held = hold_open(&socket_path, &mut server.svc);

    server.svc.request(m6_core::platform::Signal::Shutdown);
    wait_for_output(
        &server.svc,
        "shutdown signal received",
        Duration::from_secs(5),
    );

    let second = Instant::now();
    server.svc.request(m6_core::platform::Signal::Shutdown);

    let status = server
        .svc
        .wait_for_exit(second + Duration::from_secs(1))
        .unwrap_or_else(|| {
            panic!(
                "m6-html did not exit within 1s of the second SIGTERM\n--- output ---\n{}",
                server.svc.output()
            )
        });
    assert!(
        status.success(),
        "a second SIGTERM must exit 0, got {status}\n--- output ---\n{}",
        server.svc.output()
    );
    assert!(
        !socket_path.exists(),
        "the socket at {} outlived the forced exit",
        socket_path.display()
    );
    drop(held);
}
