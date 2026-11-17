//! A fake `/check` authority for tests.
//!
//! It binds a Unix socket before it returns, answers `POST /check` from a table
//! of token to [`Reply`], answers 404 for any other path or method, and counts
//! the check requests it served. A test can therefore drive the edge's check
//! path without `m6-auth-server`, a database or a signing key, which is what
//! lets `jsonwebtoken` leave `m6-http` and its tests.
//!
//! **Two ways to serve it.** [`CheckServer::serve_one`] accepts and answers one
//! connection, for a test that is driving the order itself.
//! [`ServingCheck`] binds one and answers every connection on a thread until it
//! is dropped, for a fixture that needs the authority up while a service runs.
//! Both bind and listen before they return, so the edge's connect is queued
//! rather than raced.
//!
//! The fake can produce every answer a check test needs:
//!
//! - a 200 identity, a 401, a 403, a 503, or any other status
//! - a slow answer, for the edge's deadline
//! - a close in the middle of an answer
//! - a refused connection, after [`CheckServer::close`] leaves the socket file
//!   with no listener

#![allow(
    clippy::disallowed_methods,
    reason = "test code: the fake answers on its own thread and dials its own socket while a test drives the service from outside"
)]

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::Shutdown;
use std::os::unix::io::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::thread::JoinHandle;
use std::time::Duration;

/// How long `serve_one` waits for the service's check connection.
const SERVE_BOUND: Duration = Duration::from_secs(10);

use crate::auth::Identity;

/// What the fake answers for one token.
#[derive(Clone, Debug)]
pub enum Reply {
    /// 200 with this identity.
    Identity(Identity),
    /// 401 `{"error":"unauthorised"}`.
    Unauthorised,
    /// 403 `{"error":"forbidden"}`.
    Forbidden,
    /// 503 `{"error":"unavailable"}`.
    Unavailable,
    /// Any other status, with an empty body.
    Status(u16),
    /// Wait, then answer with `answer`. The wait is the scenario under test,
    /// such as the edge's 50 ms deadline.
    Slow { after: Duration, answer: Box<Reply> },
    /// Read the check and never answer it, until the edge closes the
    /// connection. For a test that needs one check outstanding while another
    /// upstream is in flight.
    Hold,
    /// Write a 200 head that promises a body and close after a prefix of it.
    CloseMidAnswer,
}

/// A fake `/check` authority on a Unix socket.
pub struct CheckServer {
    path: PathBuf,
    listener: Option<UnixListener>,
    answers: HashMap<String, Reply>,
    /// What an unlisted token gets. A 404 here lets a test make the start probe
    /// see a socket that is not an auth server.
    unknown: Reply,
    calls: usize,
}

impl CheckServer {
    /// Bind `path` and listen. A client can connect as soon as this returns.
    pub fn bind(path: impl Into<PathBuf>) -> std::io::Result<Self> {
        let path = path.into();
        // A previous server at this path may have left a stale socket.
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path)?;
        listener.set_nonblocking(true)?;
        Ok(Self {
            path,
            listener: Some(listener),
            answers: HashMap::new(),
            unknown: Reply::Unauthorised,
            calls: 0,
        })
    }

    /// The socket a client dials.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Answer `token` with `reply`.
    pub fn answer(&mut self, token: &str, reply: Reply) {
        self.answers.insert(token.to_string(), reply);
    }

    /// Stop answering `token`.
    pub fn forget(&mut self, token: &str) {
        self.answers.remove(token);
    }

    /// Set what an unlisted token gets. The start probe sends no credential, so
    /// this is the answer it sees.
    pub fn unknown_answer(&mut self, reply: Reply) {
        self.unknown = reply;
    }

    /// How many `POST /check` requests this server has served.
    pub fn calls(&self) -> usize {
        self.calls
    }

    /// Wait until the socket accepts a connection, under `timeout`.
    ///
    /// [`CheckServer::bind`] listens before it returns, so this succeeds at
    /// once for a fresh server. It states the rule every fixture in the tree
    /// follows: a dependency is up when its socket accepts, and the fixture
    /// waits for that rather than racing the first request. The probe carries
    /// no request, so it counts no check call.
    pub fn wait_ready(&self, timeout: Duration) -> std::io::Result<()> {
        if super::wait::for_unix(&self.path, timeout) {
            return Ok(());
        }
        Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            format!(
                "{} never accepted a connection within {timeout:?}",
                self.path.display()
            ),
        ))
    }

    /// Accept and answer one connection. Returns after the answer is written
    /// and the connection closed.
    ///
    /// A test spawns the service first, then calls this, so the service's check
    /// connection is already queued or about to arrive. The wait is bounded at
    /// ten seconds so a config error fails the test instead of hanging it.
    pub fn serve_one(&mut self) -> std::io::Result<()> {
        let stream = self.accept_one()?;
        self.serve(stream)
    }

    /// Accept one connection, or time out after [`SERVE_BOUND`].
    fn accept_one(&self) -> std::io::Result<UnixStream> {
        let listener = self.listener.as_ref().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotConnected,
                "the check server is closed",
            )
        })?;
        let mut poll = crate::platform::Poll::new()?;
        poll.registry().register_fd(
            listener.as_raw_fd(),
            crate::platform::Token(0),
            crate::platform::Interest::READABLE,
        )?;
        let deadline = crate::platform::now() + SERVE_BOUND;
        let mut events = crate::platform::Events::with_capacity(1);
        loop {
            match listener.accept() {
                Ok((stream, _)) => return Ok(stream),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    let now = crate::platform::now();
                    if now >= deadline {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::TimedOut,
                            "no /check connection arrived within 10s",
                        ));
                    }
                    poll.poll(&mut events, Some(deadline - now))?;
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
    }

    /// Stop listening and leave the socket file with no listener, so the next
    /// connect is refused. [`Drop`] removes the file as well.
    pub fn close(&mut self) {
        self.listener = None;
    }

    /// One queued connection, or `None` when the queue is empty.
    ///
    /// [`ServingCheck`] does the waiting: it polls the listener and its stop
    /// socket in one reactor, so the accept itself must not wait. The listener
    /// is non-blocking, which is what makes this the drain step of an
    /// edge-triggered event.
    fn accept_ready(&mut self) -> std::io::Result<Option<UnixStream>> {
        let listener = self.listener.as_ref().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotConnected,
                "the check server is closed",
            )
        })?;
        loop {
            match listener.accept() {
                Ok((stream, _)) => return Ok(Some(stream)),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return Ok(None),
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
    }

    fn serve(&mut self, mut stream: UnixStream) -> std::io::Result<()> {
        // An accepted stream can inherit the listener's non-blocking flag on
        // some hosts.
        stream.set_nonblocking(false)?;
        let mut head = Vec::new();
        let mut buf = [0u8; 1024];
        while !head.windows(4).any(|window| window == b"\r\n\r\n") {
            if head.len() > 16 * 1024 {
                return Ok(());
            }
            match stream.read(&mut buf) {
                Ok(0) => return Ok(()),
                Ok(n) => head.extend_from_slice(&buf[..n]),
                Err(e) => return Err(e),
            }
        }

        let text = String::from_utf8_lossy(&head);
        let mut lines = text.split("\r\n");
        let request_line = lines.next().unwrap_or_default();
        let mut parts = request_line.split_whitespace();
        let method = parts.next().unwrap_or_default();
        let path = parts.next().unwrap_or_default();
        if method != "POST" || path != "/check" {
            return not_found(&mut stream);
        }

        let token = lines
            .filter_map(|line| line.split_once(':'))
            .find(|(name, _)| name.eq_ignore_ascii_case("authorization"))
            .and_then(|(_, value)| value.trim().strip_prefix("Bearer "))
            .map(str::trim)
            .filter(|token| !token.is_empty());

        self.calls += 1;
        let reply = token
            .and_then(|token| self.answers.get(token).cloned())
            .unwrap_or_else(|| self.unknown.clone());
        write_reply(&mut stream, &reply)
    }
}

impl Drop for CheckServer {
    fn drop(&mut self) {
        self.close();
        let _ = std::fs::remove_file(&self.path);
    }
}

/// A fake `/check` authority answering on its own thread until it is dropped.
///
/// A fixture that starts a service uses this instead of driving
/// [`CheckServer::serve_one`] by hand. The worker must outlive every check the
/// service sends, including one that arrives after a long idle period and one
/// whose client has already gone away: a deadline the fake misses is the
/// service's designed 503, so it is not a reason to stop answering, and
/// stopping would delete the socket file and turn every later check into a
/// connect failure.
///
/// [`Drop`] ends the worker on an event, not on the accept deadline. The worker
/// waits on the listener and on one end of a socket pair, and [`Drop`] writes a
/// byte to the other end. Waking it by connecting to `path` failed whenever a
/// fixture dropped its temporary directory first and the socket file went with
/// it, and then the worker sat out the rest of the accept deadline, ten
/// seconds, in every test that fixture ran.
pub struct ServingCheck {
    path: PathBuf,
    /// One byte written here ends the worker's wait. Dropped after [`Drop`]
    /// writes it.
    stop: UnixStream,
    worker: Option<JoinHandle<()>>,
}

impl ServingCheck {
    /// Bind `path`, wait for its socket to accept, then answer every connection
    /// with the table until dropped.
    ///
    /// The listener exists and accepts before this returns, so a service
    /// spawned next cannot see a missing socket.
    pub fn start(path: impl Into<PathBuf>, answers: Vec<(String, Reply)>) -> std::io::Result<Self> {
        let path = path.into();
        let mut server = CheckServer::bind(&path)?;
        for (token, reply) in answers {
            server.answer(&token, reply);
        }
        // The rule every start in the tree follows. The probe carries no
        // request, so the worker ignores it and `calls` stays at zero.
        server.wait_ready(SERVE_BOUND)?;
        let listener = server.listener.as_ref().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotConnected,
                "the check server is closed",
            )
        })?;
        let listener_fd = listener.as_raw_fd();
        let (stop_reader, stop) = UnixStream::pair()?;
        let worker = std::thread::Builder::new()
            .name("serving-check".to_string())
            .spawn(move || {
                // Held for the worker's life, so the descriptor registered
                // below stays open.
                let stop_fd = stop_reader.as_raw_fd();
                let mut poll = match crate::platform::Poll::new() {
                    Ok(poll) => poll,
                    Err(_) => return,
                };
                if poll
                    .registry()
                    .register_fd(
                        listener_fd,
                        crate::platform::Token(0),
                        crate::platform::Interest::READABLE,
                    )
                    .is_err()
                {
                    return;
                }
                if poll
                    .registry()
                    .register_fd(
                        stop_fd,
                        crate::platform::Token(1),
                        crate::platform::Interest::READABLE,
                    )
                    .is_err()
                {
                    return;
                }
                let mut events = crate::platform::Events::with_capacity(2);
                loop {
                    match poll.poll(&mut events, None) {
                        Ok(()) => {}
                        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                        // The listener or the stop socket failed. Nothing can
                        // be served.
                        Err(_) => return,
                    }
                    if events
                        .iter()
                        .any(|event| event.token() == crate::platform::Token(1))
                    {
                        return;
                    }
                    // Edge-triggered: drain the queue the event reported, or a
                    // connection left behind is not reported again.
                    loop {
                        match server.accept_ready() {
                            Ok(Some(stream)) => {
                                // A client that went away, because its check
                                // timed out, is not a reason to stop serving.
                                let _ = server.serve(stream);
                            }
                            Ok(None) => break,
                            // The listener itself failed. Nothing can be served.
                            Err(_) => return,
                        }
                    }
                }
            })?;
        Ok(Self {
            path,
            stop,
            worker: Some(worker),
        })
    }

    /// The socket a service dials.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for ServingCheck {
    fn drop(&mut self) {
        // The event the worker waits on. The byte cannot block: the pair's
        // buffer is empty and one byte fits. A failed write means the worker
        // has already gone, which is the outcome this is asking for.
        let _ = self.stop.write_all(&[0u8]);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn not_found(stream: &mut UnixStream) -> std::io::Result<()> {
    let body = br#"{"error":"not found"}"#;
    write!(
        stream,
        "HTTP/1.1 404 Not Found\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )?;
    stream.write_all(body)?;
    let _ = stream.shutdown(Shutdown::Both);
    Ok(())
}

fn write_reply(stream: &mut UnixStream, reply: &Reply) -> std::io::Result<()> {
    match reply {
        Reply::Identity(identity) => {
            let body = serde_json::to_vec(identity).unwrap_or_default();
            response(stream, 200, "OK", &body)
        }
        Reply::Unauthorised => {
            response(stream, 401, "Unauthorized", br#"{"error":"unauthorised"}"#)
        }
        Reply::Forbidden => response(stream, 403, "Forbidden", br#"{"error":"forbidden"}"#),
        Reply::Unavailable => response(
            stream,
            503,
            "Service Unavailable",
            br#"{"error":"unavailable"}"#,
        ),
        Reply::Status(status) => response(stream, *status, "", b""),
        Reply::Slow { after, answer } => {
            wait(*after);
            write_reply(stream, answer)
        }
        // No answer. The read returns when the edge closes the connection,
        // which is its deadline firing, so the call is bounded by the peer.
        Reply::Hold => {
            let mut byte = [0u8; 1];
            while let Ok(n) = stream.read(&mut byte) {
                if n == 0 {
                    break;
                }
            }
            Ok(())
        }
        Reply::CloseMidAnswer => {
            let head = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 64\r\nConnection: close\r\n\r\n{\"sub\":\"user-1\"";
            let _ = stream.write_all(head);
            let _ = stream.shutdown(Shutdown::Both);
            Ok(())
        }
    }
}

/// The fake's own delay, on the platform's clock, not a test's wait.
fn wait(after: Duration) {
    if after.is_zero() {
        return;
    }
    let Ok(mut poll) = crate::platform::Poll::new() else {
        return;
    };
    let mut events = crate::platform::Events::with_capacity(1);
    let _ = poll.poll(&mut events, Some(after));
}

fn response(
    stream: &mut UnixStream,
    status: u16,
    reason: &str,
    body: &[u8],
) -> std::io::Result<()> {
    write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )?;
    stream.write_all(body)?;
    let _ = stream.shutdown(Shutdown::Both);
    Ok(())
}

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "test code: a blocking client drives the fake from outside"
)]
mod tests {
    use super::*;

    fn identity() -> Identity {
        Identity {
            sub: "user-1".to_string(),
            username: "alice".to_string(),
            roles: vec!["admin".to_string()],
            groups: vec!["editors".to_string()],
        }
    }

    fn ask(server: &mut CheckServer, request: &str) -> String {
        let mut stream = UnixStream::connect(server.path()).expect("connect");
        stream.write_all(request.as_bytes()).expect("send");
        server.serve_one().expect("serve one");
        let mut buf = Vec::new();
        stream.read_to_end(&mut buf).expect("read the answer");
        String::from_utf8_lossy(&buf).into_owned()
    }

    fn check_request(token: &str) -> String {
        format!(
            "POST /check HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {token}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        )
    }

    fn status(answer: &str) -> u16 {
        answer
            .split_whitespace()
            .nth(1)
            .and_then(|status| status.parse().ok())
            .unwrap_or(0)
    }

    #[test]
    fn the_fake_answers_an_identity_and_counts_the_call() {
        let dir = tempfile::Builder::new()
            .prefix("m6-")
            .tempdir_in(crate::platform::socket_scratch_root())
            .unwrap();
        let mut server = CheckServer::bind(dir.path().join("check.sock")).expect("bind");
        server.answer("token", Reply::Identity(identity()));
        let answer = ask(&mut server, &check_request("token"));
        assert_eq!(status(&answer), 200, "{answer}");
        assert!(answer.contains("\"roles\":[\"admin\"]"), "{answer}");
        assert_eq!(server.calls(), 1);
    }

    #[test]
    fn the_fake_answers_401_403_and_503() {
        let dir = tempfile::Builder::new()
            .prefix("m6-")
            .tempdir_in(crate::platform::socket_scratch_root())
            .unwrap();
        let mut server = CheckServer::bind(dir.path().join("check.sock")).expect("bind");
        server.answer("none", Reply::Unauthorised);
        server.answer("wrong", Reply::Forbidden);
        server.answer("busy", Reply::Unavailable);
        assert_eq!(status(&ask(&mut server, &check_request("none"))), 401);
        assert_eq!(status(&ask(&mut server, &check_request("wrong"))), 403);
        assert_eq!(status(&ask(&mut server, &check_request("busy"))), 503);
    }

    #[test]
    fn an_unknown_token_is_401_and_another_path_is_404() {
        let dir = tempfile::Builder::new()
            .prefix("m6-")
            .tempdir_in(crate::platform::socket_scratch_root())
            .unwrap();
        let mut server = CheckServer::bind(dir.path().join("check.sock")).expect("bind");
        assert_eq!(status(&ask(&mut server, &check_request("absent"))), 401);
        let not_found = ask(
            &mut server,
            "GET /elsewhere HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        );
        assert_eq!(status(&not_found), 404);
        assert_eq!(
            server.calls(),
            1,
            "a wrong path must not count as a check call"
        );
    }

    #[test]
    fn a_slow_answer_is_served_after_its_delay() {
        let dir = tempfile::Builder::new()
            .prefix("m6-")
            .tempdir_in(crate::platform::socket_scratch_root())
            .unwrap();
        let mut server = CheckServer::bind(dir.path().join("check.sock")).expect("bind");
        server.answer(
            "token",
            Reply::Slow {
                after: Duration::from_millis(30),
                answer: Box::new(Reply::Identity(identity())),
            },
        );
        assert_eq!(status(&ask(&mut server, &check_request("token"))), 200);
    }

    #[test]
    fn a_close_mid_answer_ends_the_stream() {
        let dir = tempfile::Builder::new()
            .prefix("m6-")
            .tempdir_in(crate::platform::socket_scratch_root())
            .unwrap();
        let mut server = CheckServer::bind(dir.path().join("check.sock")).expect("bind");
        server.answer("token", Reply::CloseMidAnswer);
        let answer = ask(&mut server, &check_request("token"));
        assert_eq!(status(&answer), 200);
        assert!(
            !answer.contains("\"roles\""),
            "the answer was not cut short: {answer}"
        );
    }

    #[test]
    fn close_leaves_a_socket_that_refuses_the_connection() {
        let dir = tempfile::Builder::new()
            .prefix("m6-")
            .tempdir_in(crate::platform::socket_scratch_root())
            .unwrap();
        let mut server = CheckServer::bind(dir.path().join("check.sock")).expect("bind");
        server.close();
        let refused = UnixStream::connect(server.path()).expect_err("connect must be refused");
        assert_eq!(
            refused.kind(),
            std::io::ErrorKind::ConnectionRefused,
            "{refused}"
        );
    }

    #[test]
    fn the_answer_can_change_between_requests() {
        let dir = tempfile::Builder::new()
            .prefix("m6-")
            .tempdir_in(crate::platform::socket_scratch_root())
            .unwrap();
        let mut server = CheckServer::bind(dir.path().join("check.sock")).expect("bind");
        server.answer("token", Reply::Identity(identity()));
        assert_eq!(status(&ask(&mut server, &check_request("token"))), 200);
        server.answer("token", Reply::Unauthorised);
        assert_eq!(status(&ask(&mut server, &check_request("token"))), 401);
        server.forget("token");
        assert_eq!(status(&ask(&mut server, &check_request("token"))), 401);
    }

    #[test]
    fn the_unknown_answer_is_what_the_start_probe_sees() {
        let dir = tempfile::Builder::new()
            .prefix("m6-")
            .tempdir_in(crate::platform::socket_scratch_root())
            .unwrap();
        let mut server = CheckServer::bind(dir.path().join("check.sock")).expect("bind");
        server.unknown_answer(Reply::Status(404));
        assert_eq!(
            status(&ask(&mut server, &check_request("no-credential"))),
            404
        );
    }
}
