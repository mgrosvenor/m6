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
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use m6_core::testkit::{binary, read_one};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// A running `m6-html`, its socket, and the directory both live in.
///
/// Unlike `m6_core::testkit::Service`, this keeps the `Child` handle, so a
/// test can send signals by hand and wait for a specific exit status. The
/// output is still drained into a buffer, so a wedged child cannot fill its own
/// pipes and a failing assertion can print what the child said.
struct Server {
    child: Child,
    socket: PathBuf,
    _dir: tempfile::TempDir,
    output: Arc<Mutex<Vec<u8>>>,
    _drains: Vec<JoinHandle<()>>,
}

impl Server {
    /// Spawn `m6-html` on its own socket and wait until it answers a request.
    fn spawn(id: &str) -> Server {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket_path = dir.path().join(format!("{id}.sock"));

        let mut child = Command::new(binary("m6-html"))
            .arg(fixtures_dir())
            .arg(fixtures_dir().join("configs").join("m6-html-shutdown.conf"))
            .env("M6_SOCKET_OVERRIDE", &socket_path)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn m6-html");

        let output = Arc::new(Mutex::new(Vec::new()));
        let mut drains = Vec::with_capacity(2);
        let stdout = child.stdout.take().expect("stdout was piped");
        let stderr = child.stderr.take().expect("stderr was piped");
        drains.push(spawn_drain(stdout, Arc::clone(&output)));
        drains.push(spawn_drain(stderr, Arc::clone(&output)));

        let server = Server {
            child,
            socket: socket_path.clone(),
            _dir: dir,
            output,
            _drains: drains,
        };

        // Readiness is a real answered request, not the socket file existing:
        // `bind` creates the file before the server can serve.
        let ready = m6_core::testkit::wait::until(Duration::from_secs(10), || {
            http_request(
                &socket_path,
                "GET /blog HTTP/1.1\r\nHost: localhost\r\n\r\n",
            )
            .is_some_and(|r| r.contains("200 OK"))
        });
        assert!(
            ready,
            "m6-html never answered a request\n--- output ---\n{}",
            server.output()
        );
        server
    }

    /// Send `sig` to the child.
    fn signal(&self, sig: libc::c_int) {
        // SAFETY: `kill` takes a pid and a signal number and touches no memory.
        unsafe { libc::kill(self.child.id() as libc::pid_t, sig) };
    }

    /// `Some(status)` once the child has exited, `None` while it runs.
    fn try_wait(&mut self) -> Option<ExitStatus> {
        self.child.try_wait().ok().flatten()
    }

    /// Wait for the child to exit, or return `None` when `deadline` passes.
    fn wait_for_exit(&mut self, deadline: Instant) -> Option<ExitStatus> {
        loop {
            if let Some(status) = self.try_wait() {
                return Some(status);
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Panic with the exit status and output if the child is no longer running.
    fn assert_alive(&mut self, context: &str) {
        if let Some(status) = self.try_wait() {
            panic!(
                "m6-html exited while {context}: {status}\n--- output ---\n{}",
                self.output()
            );
        }
    }

    /// Everything the child has written to stdout and stderr so far.
    fn output(&self) -> String {
        let buf = self.output.lock().unwrap_or_else(|e| e.into_inner());
        String::from_utf8_lossy(&buf).into_owned()
    }
}

fn spawn_drain(mut pipe: impl Read + Send + 'static, sink: Arc<Mutex<Vec<u8>>>) -> JoinHandle<()> {
    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            match pipe.read(&mut buf) {
                Ok(0) | Err(_) => return,
                Ok(n) => sink
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .extend_from_slice(&buf[..n]),
            }
        }
    })
}

impl Drop for Server {
    fn drop(&mut self) {
        // A test that panics must not leave the child running: it would keep
        // its socket and drain workers alive into the next test.
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
        for h in self._drains.drain(..) {
            let _ = h.join();
        }
    }
}

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
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

/// Open a request whose body is sent only partway, and confirm the worker is
/// blocked reading it. Returns the held-open stream.
///
/// With one worker and no queue, a probe is answered 503 while the worker is
/// busy. That 503 is the signal this waits on, rather than a fixed sleep.
fn hold_open(socket: &Path, server: &mut Server) -> UnixStream {
    let mut held = UnixStream::connect(socket).expect("connect the held request");
    held.set_read_timeout(Some(Duration::from_secs(10))).ok();
    held.write_all(
        b"POST /blog HTTP/1.1\r\nHost: localhost\r\nContent-Length: 100\r\nConnection: close\r\n\r\n",
    )
    .expect("write the held request head");
    held.write_all(b"0123456789")
        .expect("write the first 10 body bytes");

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if http_request(socket, "GET /blog HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .is_some_and(|r| r.contains("503"))
        {
            return held;
        }
        server.assert_alive("while confirming the held request is in flight");
        assert!(
            Instant::now() < deadline,
            "the held request never took the worker\n--- output ---\n{}",
            server.output()
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// Wait until the child's output contains `marker`, or panic with the output.
fn wait_for_output(server: &Server, marker: &str, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        if server.output().contains(marker) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "m6-html never logged {marker:?} within {timeout:?}\n--- output ---\n{}",
            server.output()
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
    let mut server = Server::spawn("term-twice");
    let socket_path = server.socket.clone();
    let held = hold_open(&socket_path, &mut server);

    server.signal(libc::SIGTERM);
    wait_for_output(&server, "shutdown signal received", Duration::from_secs(5));

    let second = Instant::now();
    server.signal(libc::SIGTERM);

    let status = server
        .wait_for_exit(second + Duration::from_secs(1))
        .unwrap_or_else(|| {
            panic!(
                "m6-html did not exit within 1s of the second SIGTERM\n--- output ---\n{}",
                server.output()
            )
        });
    assert!(
        status.success(),
        "a second SIGTERM must exit 0, got {status}\n--- output ---\n{}",
        server.output()
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
    let mut server = Server::spawn("sigint-once");
    let socket_path = server.socket.clone();

    server.signal(libc::SIGINT);
    let status = server
        .wait_for_exit(Instant::now() + Duration::from_secs(5))
        .unwrap_or_else(|| {
            panic!(
                "m6-html did not exit within 5s of SIGINT\n--- output ---\n{}",
                server.output()
            )
        });
    assert!(
        status.success(),
        "SIGINT must exit 0, got {status}\n--- output ---\n{}",
        server.output()
    );
    assert!(
        !socket_path.exists(),
        "the socket at {} outlived SIGINT",
        socket_path.display()
    );
    m6_core::testkit::assert_lifecycle_logged("m6-html", &server.output());
}

/// A request held open when one SIGTERM arrives receives its complete response
/// before the process exits. The wrong outcome: shutdown exits without
/// draining, cutting the response short.
#[test]
fn a_request_in_flight_at_sigterm_receives_its_full_response() {
    let mut server = Server::spawn("term-inflight");
    let socket_path = server.socket.clone();
    let mut held = hold_open(&socket_path, &mut server);

    server.signal(libc::SIGTERM);
    wait_for_output(&server, "shutdown signal received", Duration::from_secs(5));

    // While the request is in flight the drain must keep the process alive. An
    // exit here means shutdown did not wait for the in-flight request.
    let deadline = Instant::now() + Duration::from_millis(500);
    loop {
        if let Some(status) = server.try_wait() {
            panic!(
                "m6-html exited ({status}) while the request was still in flight; \
                 shutdown did not drain\n--- output ---\n{}",
                server.output()
            );
        }
        if Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
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
        .wait_for_exit(Instant::now() + Duration::from_secs(5))
        .unwrap_or_else(|| {
            panic!(
                "m6-html did not exit after draining\n--- output ---\n{}",
                server.output()
            )
        });
    assert!(
        status.success(),
        "a drained shutdown must exit 0, got {status}\n--- output ---\n{}",
        server.output()
    );
    assert!(
        !socket_path.exists(),
        "the socket at {} outlived the shutdown",
        socket_path.display()
    );
}
