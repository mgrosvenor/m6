//! Shutdown signal scenarios, exercised against the real `m6-html` binary.
//!
//! The behaviour under test lives in `m6_core::signal`, shared by every m6
//! service, but m6-html is the smallest service whose suite already starts the
//! binary. Each test below names the failure it guards against: a second
//! SIGTERM that fails to force an exit, a SIGINT that does not take the same
//! graceful path as SIGTERM, and a SIGTERM that cuts off an in-flight response.

use std::io::{Read, Write};
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
/// holding its socket is removed. The `Service` does the spawn, drain, signal
/// and output work; this wrapper only keeps the temp directory alive.
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
    spawn_with(id, "m6-html-shutdown.conf")
}

fn spawn_with(id: &str, config: &str) -> (Server, PathBuf) {
    spawn_with_args(id, config, &[])
}

fn spawn_with_args(id: &str, config: &str, args: &[&str]) -> (Server, PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    // Short name: a unix socket path is capped near 104 bytes on macOS.
    let socket_path = dir.path().join(format!("{id}.sock"));

    let mut svc = Service::spawn(
        "m6-html",
        Command::new(binary("m6-html"))
            .arg(fixtures_dir())
            .arg(config_path(config))
            .args(args)
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

/// Open a request whose body is sent only partway, and confirm the worker has
/// accepted it and is blocked reading the body. Returns the held-open stream.
///
/// With one worker and no queue, a probe is answered 503 while the worker is
/// busy. The held request connects before the probe and the service accepts in
/// order, so once any probe is answered the service has already decided the
/// held request's fate. A refusal (the worker was not yet back waiting for
/// work) shows as a write that fails or a 503 already readable on the held
/// stream, and the attempt is repeated. Otherwise the request was accepted and
/// the first 503 to a probe proves the worker is still on it.
fn hold_open(socket: &Path, svc: &mut Service) -> UnixStream {
    hold_open_asking(socket, svc, "close")
}

/// `hold_open` with the request's `Connection` header set to `connection`.
fn hold_open_asking(socket: &Path, svc: &mut Service, connection: &str) -> UnixStream {
    let head = format!(
        "POST /blog HTTP/1.1\r\nHost: localhost\r\nContent-Length: 100\r\nConnection: {connection}\r\n\r\n0123456789"
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let mut held = UnixStream::connect(socket).expect("connect the held request");
        held.set_read_timeout(Some(Duration::from_secs(10))).ok();
        let sent = held.write_all(head.as_bytes()).is_ok();

        let mut refused = !sent;
        while !refused {
            let probe = http_request(socket, "GET /blog HTTP/1.1\r\nHost: localhost\r\n\r\n");
            refused = was_refused(&mut held);
            if refused {
                break;
            }
            if probe.is_some_and(|r| r.contains("503")) {
                return held;
            }
            svc.assert_alive("while confirming the held request is in flight");
            assert!(
                Instant::now() < deadline,
                "the held request never took the worker\n--- output ---\n{}",
                svc.output()
            );
            std::thread::sleep(Duration::from_millis(25));
        }
        assert!(
            Instant::now() < deadline,
            "the held request was refused every time\n--- output ---\n{}",
            svc.output()
        );
    }
}

/// True if the service has already answered or closed the held stream, which
/// it does only to refuse it.
fn was_refused(held: &mut UnixStream) -> bool {
    held.set_nonblocking(true).expect("poll the held stream");
    let refused = !matches!(
        held.read(&mut [0u8; 1]),
        Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock
    );
    held.set_nonblocking(false).expect("block the held stream");
    refused
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

/// A second SIGTERM while a request holds the drain open forces an immediate
/// exit. The wrong outcome this catches: shutdown waits for the slow request,
/// so an operator who signals twice gets no faster stop.
#[test]
fn a_second_sigterm_forces_exit_without_waiting_for_the_request() {
    let (mut server, socket_path) = spawn("term-twice");
    let held = hold_open(&socket_path, &mut server.svc);

    server.svc.signal(libc::SIGTERM);
    wait_for_output(
        &server.svc,
        "shutdown signal received",
        Duration::from_secs(5),
    );

    let second = Instant::now();
    server.svc.signal(libc::SIGTERM);

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

/// One SIGINT takes the same graceful path as SIGTERM: exit 0, socket removed,
/// the lifecycle lines logged. The wrong outcome: SIGINT kills the process at
/// the default disposition, leaving the socket behind and a non-zero status.
#[test]
fn one_sigint_exits_zero_and_removes_the_socket() {
    let (mut server, socket_path) = spawn("sigint-once");

    server.svc.signal(libc::SIGINT);
    let status = server
        .svc
        .wait_for_exit(Instant::now() + Duration::from_secs(5))
        .unwrap_or_else(|| {
            panic!(
                "m6-html did not exit within 5s of SIGINT\n--- output ---\n{}",
                server.svc.output()
            )
        });
    assert!(
        status.success(),
        "SIGINT must exit 0, got {status}\n--- output ---\n{}",
        server.svc.output()
    );
    assert!(
        !socket_path.exists(),
        "the socket at {} outlived SIGINT",
        socket_path.display()
    );
    m6_core::testkit::assert_lifecycle_logged("m6-html", &server.svc.output());
}

/// A request held open when one SIGTERM arrives receives its complete response
/// before the process exits. The wrong outcome: shutdown exits without
/// draining, cutting the response short.
#[test]
fn a_request_in_flight_at_sigterm_receives_its_full_response() {
    let (mut server, socket_path) = spawn("term-inflight");
    // It asks for keep-alive, so the `Connection: close` checked below is the
    // service's own decision once shutdown has started.
    let mut held = hold_open_asking(&socket_path, &mut server.svc, "keep-alive");

    server.svc.signal(libc::SIGTERM);
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
    // A request served after shutdown started closes its connection, so the
    // client does not send another on a connection that is about to go.
    assert!(
        text.to_ascii_lowercase().contains("connection: close"),
        "the response drained at shutdown kept its connection alive: {}",
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

/// An idle keep-alive connection is closed when shutdown starts, so it cannot
/// hold the drain. The wrong outcome: a client (a reverse proxy's connection
/// pool, typically) keeps a connection open between requests, its worker
/// counts as busy until the read times out, and every graceful stop waits the
/// whole drain deadline.
#[test]
fn an_idle_keep_alive_connection_does_not_hold_the_drain() {
    let (mut server, socket_path) = spawn("term-keepalive");
    let mut conn = UnixStream::connect(&socket_path).expect("connect");
    conn.set_read_timeout(Some(Duration::from_secs(30))).ok();
    conn.write_all(b"GET /blog HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .expect("send a request");
    let first = String::from_utf8_lossy(&read_one(&mut conn, "GET").expect("read the response"))
        .to_ascii_lowercase();
    assert!(
        !first.contains("connection: close"),
        "the response closed the connection, so nothing is left idle to test: {}",
        &first[..first.len().min(300)]
    );

    // The connection now sits idle between requests. Its read timeout is far
    // longer than the wait below, so only closing it at shutdown lets the
    // process exit in time.
    server.svc.signal(libc::SIGTERM);
    let status = server
        .svc
        .wait_for_exit(Instant::now() + Duration::from_secs(5))
        .unwrap_or_else(|| {
            panic!(
                "an idle keep-alive connection held the drain\n--- output ---\n{}",
                server.svc.output()
            )
        });
    assert!(
        status.success(),
        "exit {status}\n--- output ---\n{}",
        server.svc.output()
    );
    let mut rest = Vec::new();
    conn.read_to_end(&mut rest).ok();
    assert!(
        rest.is_empty(),
        "the idle connection received data after shutdown"
    );
}

/// A connection that has sent nothing is idle too, so it cannot hold the
/// drain. The wrong outcome: a proxy's warm pooled connection, or a client
/// that connected just before the signal, keeps its worker counted as busy
/// until the read times out, and the stop waits out the whole deadline.
#[test]
fn a_connection_that_has_sent_nothing_does_not_hold_the_drain() {
    let (mut server, socket_path) = spawn("term-silent");
    let mut conn = UnixStream::connect(&socket_path).expect("connect");
    conn.set_read_timeout(Some(Duration::from_secs(30))).ok();

    server.svc.signal(libc::SIGTERM);
    let status = server
        .svc
        .wait_for_exit(Instant::now() + Duration::from_secs(5))
        .unwrap_or_else(|| {
            panic!(
                "a connection that sent nothing held the drain\n--- output ---\n{}",
                server.svc.output()
            )
        });
    assert!(
        status.success(),
        "exit {status}\n--- output ---\n{}",
        server.svc.output()
    );
    let mut rest = Vec::new();
    conn.read_to_end(&mut rest).ok();
    assert!(
        rest.is_empty(),
        "the silent connection received data after shutdown"
    );
}

/// A request already arriving on a kept-alive connection when shutdown starts
/// is served in full. The wrong outcome: the connection still counts as idle
/// while its next request's body streams in, shutdown closes it for reading,
/// and a request that cannot safely be retried is cut off.
#[test]
fn a_second_request_arriving_at_sigterm_is_served_in_full() {
    // At debug level the service logs "request arriving" when a request's
    // first byte is read and the connection stops counting as idle.
    let (mut server, socket_path) = spawn_with_args(
        "term-second",
        "m6-html-shutdown.conf",
        &["--log-level", "debug"],
    );
    let mut conn = UnixStream::connect(&socket_path).expect("connect");
    conn.set_read_timeout(Some(Duration::from_secs(10))).ok();
    conn.write_all(b"GET /blog HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .expect("send the first request");
    read_one(&mut conn, "GET").expect("read the first response");

    conn.write_all(
        b"POST /blog HTTP/1.1\r\nHost: localhost\r\nContent-Length: 100\r\n\r\n0123456789",
    )
    .expect("start the second request");
    // The second "request arriving": the connection is no longer idle, so
    // shutdown must wait for this request rather than close it.
    let deadline = Instant::now() + Duration::from_secs(10);
    while server.svc.output().matches("request arriving").count() < 2 {
        assert!(
            Instant::now() < deadline,
            "the service never logged the second request arriving\n--- output ---\n{}",
            server.svc.output()
        );
        std::thread::sleep(Duration::from_millis(10));
    }

    server.svc.signal(libc::SIGTERM);
    wait_for_output(&server.svc, "draining", Duration::from_secs(5));
    conn.write_all(&[b'x'; 90])
        .expect("finish the second request's body");
    let resp =
        String::from_utf8_lossy(&read_one(&mut conn, "POST").expect("read the second response"))
            .into_owned();
    assert!(
        resp.contains("200 OK"),
        "the request arriving at shutdown was cut off: {}",
        &resp[..resp.len().min(300)]
    );
    let status = server
        .svc
        .wait_for_exit(Instant::now() + Duration::from_secs(5))
        .unwrap_or_else(|| {
            panic!(
                "m6-html did not exit\n--- output ---\n{}",
                server.svc.output()
            )
        });
    assert!(
        status.success(),
        "exit {status}\n--- output ---\n{}",
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

    server.svc.signal(libc::SIGTERM);
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
