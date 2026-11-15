//! A service sends mail from its own loop, in process, while it keeps serving.
//!
//! `harness = false`: this binary's `main` speaks libtest's listing protocol
//! through `libtest-mimic`, so `nextest` lists and runs the four checks. The
//! service under test is the reference loop from `examples/echo.rs` with
//! `h1::ServerConn` clients and one `email::Smtp` slot per client, run in
//! this process on its own thread. Each test owns a loopback SMTP stub in a
//! second thread and talks to the service over its unix socket.

#![allow(
    clippy::disallowed_methods,
    reason = "test code: the scripted SMTP stub is a blocking thread"
)]

use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use libtest_mimic::{Arguments, Failed, Trial};
use m6_core::email::{self, Relay, RelayConfig, Smtp};
use m6_core::event::{Slots, Timeouts};
use m6_core::h1::{head_slots, Flush, Read as H1Read, ServerConn};
use m6_core::net::listen_unix;
use m6_core::platform;
use m6_core::platform::{Events, Interest, Poll, Token};
use m6_core::testkit::wait;

const LISTENER: Token = Token(usize::MAX);
const READ_CAP: usize = 16 * 1024;
const WRITE_CAP: usize = 16 * 1024;
const INITIAL: usize = 8;
const CEILING: usize = 64;
const TICK: Duration = Duration::from_millis(50);
/// The relay deadline the service puts in its outgoing `Timeouts`.
const SMTP_TIMEOUT: Duration = Duration::from_secs(1);

fn main() {
    let mut args = Arguments::from_args();
    args.test_threads = Some(1);
    let tests = vec![
        Trial::test(
            "the_service_answers_while_a_message_is_in_flight",
            the_service_answers_while_a_message_is_in_flight,
        ),
        Trial::test(
            "a_send_that_fails_is_reported_to_the_client",
            a_send_that_fails_is_reported_to_the_client,
        ),
        Trial::test(
            "a_send_past_its_deadline_answers_504",
            a_send_past_its_deadline_answers_504,
        ),
        Trial::test(
            "a_client_that_leaves_drops_the_delivery",
            a_client_that_leaves_drops_the_delivery,
        ),
    ];
    libtest_mimic::run(&args, tests).exit();
}

// ── the service loop ─────────────────────────────────────────────────────────

#[derive(PartialEq, Eq, Clone, Copy)]
enum State {
    Reading,
    Sending,
}

struct Conn {
    conn: ServerConn,
    smtp: Smtp,
    state: State,
    interest: Interest,
    smtp_interest: Interest,
}

impl Conn {
    fn empty() -> Conn {
        Conn {
            conn: ServerConn::new(READ_CAP, WRITE_CAP),
            smtp: Smtp::new(READ_CAP, WRITE_CAP),
            state: State::Reading,
            interest: Interest::READABLE,
            smtp_interest: Interest::READABLE,
        }
    }
}

enum Action {
    Keep,
    Close,
    StartOutgoing,
}

/// One service's loop: the reference every service author copies.
fn service(path: &Path, relay_addr: SocketAddr, stop: Arc<AtomicBool>) -> io::Result<()> {
    let relay = Relay::new(&RelayConfig {
        host: "127.0.0.1".into(),
        port: relay_addr.port(),
        starttls: None,
        credentials: None,
        timeout: SMTP_TIMEOUT,
    })?;
    let mut poll = Poll::new()?;
    let mut listener = listen_unix(path, 0o600)?;
    poll.registry()
        .register(&mut listener, LISTENER, Interest::READABLE)?;
    let mut conns = Slots::new(INITIAL, CEILING, Conn::empty);
    let mut deadlines = Timeouts::new(INITIAL, SMTP_TIMEOUT);
    let mut events = Events::with_capacity(64);
    let mut now = platform::now();
    loop {
        match poll.poll(&mut events, Some(deadlines.timeout(now, TICK))) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
        now = platform::now();
        for ev in &events {
            match ev.token() {
                LISTENER => {
                    while let Some(taken) = conns.take() {
                        if let Some(new_len) = taken.grew_to {
                            deadlines.grow(new_len);
                        }
                        let mut stream = match listener.accept() {
                            Ok((stream, _)) => stream,
                            Err(e) => match m6_core::platform::net::accept_error(&e) {
                                m6_core::platform::net::Accept::Drained => {
                                    conns.free(taken.key);
                                    break;
                                }
                                m6_core::platform::net::Accept::SkipOne => {
                                    conns.free(taken.key);
                                    continue;
                                }
                                m6_core::platform::net::Accept::Stop => {
                                    conns.free(taken.key);
                                    break;
                                }
                            },
                        };
                        poll.registry().register(
                            &mut stream,
                            Token(2 * taken.key),
                            Interest::READABLE,
                        )?;
                        let conn = conns.get_mut(taken.key);
                        conn.conn.attach(stream);
                        conn.state = State::Reading;
                        conn.interest = Interest::READABLE;
                    }
                }
                // One outgoing slot's turn.
                Token(t) if t % 2 == 1 => {
                    outgoing_event(&mut poll, &mut conns, &mut deadlines, t / 2)?
                }
                // One client's turn.
                Token(t) => {
                    client_event(&mut poll, &mut conns, &mut deadlines, &relay, t / 2, now)?
                }
            }
        }
        // The loop's deadline for a delivery in flight: 504, and the slot's
        // socket closes.
        while let Some(Token(k)) = deadlines.expired(now) {
            let conn = conns.get_mut(k);
            if conn.state == State::Sending {
                poll.registry().deregister_fd(conn.smtp.fd())?;
                conn.smtp.close();
                conn.state = State::Reading;
                conn.conn.respond(504, &[], b"timeout");
                let _ = conn.conn.flush()?;
                refresh_client(&poll, conns.get_mut(k), k)?;
            }
        }
        if stop.load(Ordering::Relaxed) {
            return Ok(());
        }
    }
}

fn client_event(
    poll: &mut Poll,
    conns: &mut Slots<Conn>,
    deadlines: &mut Timeouts,
    relay: &Relay,
    k: usize,
    now: std::time::Instant,
) -> io::Result<()> {
    let action = client_turn(conns.get_mut(k), relay)?;
    match action {
        Action::Close => return free(poll, conns, deadlines, k),
        Action::StartOutgoing => {
            let conn = conns.get_mut(k);
            let interest = conn.smtp.interest();
            poll.registry()
                .register_fd(conn.smtp.fd(), Token(2 * k + 1), interest)?;
            conn.smtp_interest = interest;
            deadlines.touch(Token(k), now);
        }
        Action::Keep => {}
    }
    refresh_client(poll, conns.get_mut(k), k)
}

fn client_turn(conn: &mut Conn, relay: &Relay) -> io::Result<Action> {
    if conn.state == State::Sending {
        // The client's token stays readable while it waits. A read that finds
        // the client gone frees its slot and its delivery.
        let mut heads = head_slots();
        return match conn.conn.read(&mut heads)? {
            H1Read::Closed => Ok(Action::Close),
            _ => Ok(Action::Keep),
        };
    }
    loop {
        if conn.conn.interest() == Interest::WRITABLE {
            match conn.conn.flush()? {
                Flush::Done => {}
                Flush::More => return Ok(Action::Keep),
                Flush::Close => return Ok(Action::Close),
            }
        }
        let mut heads = head_slots();
        match conn.conn.read(&mut heads)? {
            H1Read::More => return Ok(Action::Keep),
            H1Read::Closed => return Ok(Action::Close),
            H1Read::Request(head) => match head.path {
                "/send" => match conn.smtp.start(relay, &message()) {
                    Ok(()) => {
                        conn.state = State::Sending;
                        return Ok(Action::StartOutgoing);
                    }
                    Err(e) => conn
                        .conn
                        .respond(500, &[], format!("failed: {e}").as_bytes()),
                },
                "/now" => conn.conn.respond(200, &[], b"now"),
                _ => conn.conn.respond(404, &[], b"not found"),
            },
        }
    }
}

fn outgoing_event(
    poll: &mut Poll,
    conns: &mut Slots<Conn>,
    deadlines: &mut Timeouts,
    k: usize,
) -> io::Result<()> {
    let conn = conns.get_mut(k);
    match conn.smtp.drive() {
        Ok(None) => {
            let interest = conn.smtp.interest();
            if interest != conn.smtp_interest {
                poll.registry()
                    .reregister_fd(conn.smtp.fd(), Token(2 * k + 1), interest)?;
                conn.smtp_interest = interest;
            }
        }
        Ok(Some(())) => {
            conn.state = State::Reading;
            conn.conn.respond(200, &[], b"sent");
            finish(poll, conns, deadlines, k)?;
        }
        Err(e) => {
            conn.state = State::Reading;
            conn.conn
                .respond(500, &[], format!("failed: {e}").as_bytes());
            finish(poll, conns, deadlines, k)?;
        }
    }
    Ok(())
}

/// Drop the delivery's socket and register the client for its answer.
fn finish(
    poll: &mut Poll,
    conns: &mut Slots<Conn>,
    deadlines: &mut Timeouts,
    k: usize,
) -> io::Result<()> {
    let conn = conns.get_mut(k);
    poll.registry().deregister_fd(conn.smtp.fd())?;
    conn.smtp.close();
    deadlines.clear(Token(k));
    let _ = conn.conn.flush()?;
    refresh_client(poll, conns.get_mut(k), k)
}

fn refresh_client(poll: &Poll, conn: &mut Conn, k: usize) -> io::Result<()> {
    let interest = conn.conn.interest();
    if interest != conn.interest {
        let stream = conn
            .conn
            .stream()
            .expect("a registered client has a stream");
        poll.registry().reregister(stream, Token(2 * k), interest)?;
        conn.interest = interest;
    }
    Ok(())
}

fn free(
    poll: &Poll,
    conns: &mut Slots<Conn>,
    deadlines: &mut Timeouts,
    k: usize,
) -> io::Result<()> {
    deadlines.clear(Token(k));
    let conn = conns.get_mut(k);
    if conn.state == State::Sending {
        poll.registry().deregister_fd(conn.smtp.fd())?;
        conn.smtp.close();
    }
    conn.state = State::Reading;
    if let Some(mut stream) = conn.conn.detach() {
        poll.registry().deregister(&mut stream)?;
    }
    conns.free(k);
    Ok(())
}

fn message() -> email::lettre::Message {
    email::lettre::Message::builder()
        .from("Sender <from@example.com>".parse().expect("from"))
        .to("To <to@example.com>".parse().expect("to"))
        .subject("hello")
        .body("hello from the loop".to_owned())
        .expect("message")
}

// ── fixtures ─────────────────────────────────────────────────────────────────

struct Service {
    socket: std::path::PathBuf,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<io::Result<()>>>,
}

impl Service {
    fn start(dir: &Path, relay: SocketAddr) -> Service {
        let socket = dir.join("email.sock");
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let path = socket.clone();
        let handle = std::thread::spawn(move || service(&path, relay, flag));
        assert!(
            wait::for_unix(&socket, Duration::from_secs(5)),
            "the service socket never appeared"
        );
        Service {
            socket,
            stop,
            handle: Some(handle),
        }
    }

    fn send(&self, method: &str, target: &str) -> UnixStream {
        let mut stream = UnixStream::connect(&self.socket).expect("connect");
        stream
            .write_all(format!("{method} {target} HTTP/1.1\r\nHost: x\r\n\r\n").as_bytes())
            .expect("request");
        stream
    }
}

impl Drop for Service {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // The loop's tick wakes it; joining keeps the socket dir alive.
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// Read one HTTP reply: the head, then `Content-Length` bytes.
fn response(stream: &mut UnixStream, timeout: Duration) -> (u16, String) {
    stream.set_read_timeout(Some(timeout)).expect("timeout");
    let mut got = Vec::new();
    let mut buf = [0u8; 4096];
    while !got.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = stream.read(&mut buf).expect("read head");
        assert!(n > 0, "the service closed before answering");
        got.extend_from_slice(&buf[..n]);
    }
    let head_end = got.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
    let head = String::from_utf8_lossy(&got[..head_end]).into_owned();
    let status: u16 = head
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .expect("a status");
    let len: usize = head
        .lines()
        .find_map(|l| l.strip_prefix("Content-Length: "))
        .and_then(|v| v.parse().ok())
        .expect("a Content-Length");
    while got.len() < head_end + len {
        let n = stream.read(&mut buf).expect("read body");
        assert!(n > 0, "the body ended early");
        got.extend_from_slice(&buf[..n]);
    }
    (
        status,
        String::from_utf8_lossy(&got[head_end..head_end + len]).into_owned(),
    )
}

fn read_until(stream: &mut impl Read, needle: &[u8]) -> Vec<u8> {
    let mut got = Vec::new();
    let mut buf = [0u8; 4096];
    while !got.windows(needle.len()).any(|w| w == needle) {
        let n = stream.read(&mut buf).expect("stub read");
        assert!(n > 0, "the client closed before {needle:?}");
        got.extend_from_slice(&buf[..n]);
    }
    got
}

fn bind() -> (TcpListener, SocketAddr) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    (listener, addr)
}

/// A stub that holds its greeting until `go`, then runs the success script.
/// `accepted` fires once the client's connection arrives.
fn successful_stub() -> (
    SocketAddr,
    Receiver<()>,
    Sender<()>,
    JoinHandle<Vec<String>>,
) {
    let (listener, addr) = bind();
    let (accepted_tx, accepted) = mpsc::channel();
    let (go_tx, go) = mpsc::channel::<()>();
    let handle = std::thread::spawn(move || {
        let (mut sock, _) = listener.accept().expect("accept");
        sock.set_read_timeout(Some(Duration::from_secs(10)))
            .expect("timeout");
        accepted_tx.send(()).expect("accepted");
        go.recv_timeout(Duration::from_secs(5)).expect("go");
        sock.write_all(b"220 test\r\n").expect("greeting");
        let mut log = Vec::new();
        for (expect, reply) in [
            (&b"EHLO"[..], &b"250 test\r\n"[..]),
            (b"MAIL", b"250 ok\r\n"),
            (b"RCPT", b"250 ok\r\n"),
            (b"DATA", b"354 go\r\n"),
        ] {
            let line = read_until(&mut sock, b"\r\n");
            assert!(
                line.starts_with(expect),
                "{:?}",
                String::from_utf8_lossy(&line)
            );
            log.push(String::from_utf8_lossy(&line).trim_end().to_owned());
            sock.write_all(reply).expect("reply");
        }
        let wire = read_until(&mut sock, b"\r\n.\r\n");
        log.push(format!(
            "body:{}",
            String::from_utf8_lossy(&wire[..wire.len() - 5])
        ));
        sock.write_all(b"250 queued\r\n").expect("data reply");
        log
    });
    (addr, accepted, go_tx, handle)
}

/// A stub that accepts and never writes. `eof` fires when its peer closes.
fn silent_stub() -> (SocketAddr, Receiver<()>, Receiver<bool>, JoinHandle<()>) {
    let (listener, addr) = bind();
    let (accepted_tx, accepted) = mpsc::channel();
    let (eof_tx, eof) = mpsc::channel();
    let handle = std::thread::spawn(move || {
        let (mut sock, _) = listener.accept().expect("accept");
        sock.set_read_timeout(Some(Duration::from_secs(5)))
            .expect("timeout");
        accepted_tx.send(()).expect("accepted");
        let mut buf = [0u8; 64];
        loop {
            match sock.read(&mut buf) {
                Ok(0) => {
                    let _ = eof_tx.send(true);
                    return;
                }
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    let _ = eof_tx.send(false);
                    return;
                }
                Err(_) => return,
            }
        }
    });
    (addr, accepted, eof, handle)
}

// ── the checks ───────────────────────────────────────────────────────────────

fn the_service_answers_while_a_message_is_in_flight() -> Result<(), Failed> {
    let dir = tempfile::tempdir().expect("tempdir");
    let (relay, accepted, go, peer) = successful_stub();
    let service = Service::start(dir.path(), relay);
    let mut sending = service.send("POST", "/send");
    accepted
        .recv_timeout(Duration::from_secs(2))
        .expect("the service must connect to the relay");
    // The relay holds its greeting. The service must still answer /now.
    let mut now = service.send("GET", "/now");
    let (status, body) = response(&mut now, Duration::from_secs(2));
    assert_eq!((status, body.as_str()), (200, "now"));
    go.send(()).expect("release the greeting");
    let (status, body) = response(&mut sending, Duration::from_secs(5));
    assert_eq!((status, body.as_str()), (200, "sent"));
    let log = peer.join().expect("stub");
    assert!(log[0].starts_with("EHLO "), "{log:?}");
    assert_eq!(log[1], "MAIL FROM:<from@example.com>");
    assert_eq!(log[2], "RCPT TO:<to@example.com>");
    Ok(())
}

fn a_send_that_fails_is_reported_to_the_client() -> Result<(), Failed> {
    let dir = tempfile::tempdir().expect("tempdir");
    let (listener, relay) = bind();
    let peer = std::thread::spawn(move || {
        let (mut sock, _) = listener.accept().expect("accept");
        sock.set_read_timeout(Some(Duration::from_secs(5)))
            .expect("timeout");
        sock.write_all(b"220 test\r\n").expect("greeting");
        let ehlo = read_until(&mut sock, b"\r\n");
        assert!(ehlo.starts_with(b"EHLO"));
        sock.write_all(b"250 test\r\n").expect("ehlo reply");
        let mail = read_until(&mut sock, b"\r\n");
        assert!(mail.starts_with(b"MAIL"));
        sock.write_all(b"554 no\r\n").expect("refusal");
    });
    let service = Service::start(dir.path(), relay);
    let mut sending = service.send("POST", "/send");
    let (status, body) = response(&mut sending, Duration::from_secs(3));
    assert_eq!(status, 500, "{body}");
    assert!(body.contains("failed:"), "{body}");
    assert!(body.contains("MAIL"), "{body}");
    assert!(body.contains("554"), "{body}");
    peer.join().expect("stub");
    Ok(())
}

fn a_send_past_its_deadline_answers_504() -> Result<(), Failed> {
    let dir = tempfile::tempdir().expect("tempdir");
    let (relay, _accepted, eof, peer) = silent_stub();
    let service = Service::start(dir.path(), relay);
    let mut sending = service.send("POST", "/send");
    let started = platform::now();
    let (status, body) = response(&mut sending, Duration::from_secs(3));
    assert_eq!((status, body.as_str()), (504, "timeout"));
    assert!(
        platform::now() - started < Duration::from_secs(3),
        "the 504 must come from the loop's deadline"
    );
    assert!(eof
        .recv_timeout(Duration::from_secs(2))
        .expect("the stub must see EOF"));
    peer.join().expect("stub");
    Ok(())
}

fn a_client_that_leaves_drops_the_delivery() -> Result<(), Failed> {
    let dir = tempfile::tempdir().expect("tempdir");
    let (relay, accepted, eof, peer) = silent_stub();
    let service = Service::start(dir.path(), relay);
    let sending = service.send("POST", "/send");
    accepted
        .recv_timeout(Duration::from_secs(2))
        .expect("the service must connect to the relay");
    drop(sending);
    assert!(
        eof.recv_timeout(Duration::from_secs(2))
            .expect("the relay must see EOF"),
        "close() must close the slot's socket"
    );
    peer.join().expect("stub");
    Ok(())
}
