//! The platform layer's behavioural contract, one shared suite.
//!
//! Every test here is written once against `m6_platform`'s public API
//! and runs unchanged on macOS arm64, Linux x86_64 and Linux arm64. No `cfg`,
//! no per-OS branch, no OS-specific helper. Each test's comment names the
//! wrong implementation that must fail it, and this file is the list.
//!
//! The signal contract is `platform_signals.rs`, which needs its own `main`.
//! The gate is `platform_gate.rs`. The echo reference app's suite is
//! `echo_example.rs`.

#![allow(
    clippy::disallowed_methods,
    reason = "test code: blocking peers and threads drive the layer from outside"
)]

use std::io::{self, Read, Write};
use std::net::SocketAddr;
use std::os::fd::AsRawFd;
use std::time::{Duration, Instant};

use m6_platform::{self as platform, net, process, Events, Interest, Poll, Token};

/// One bounded wait: what is ready after at most `wait`.
///
/// The layer passes `Interrupted` up, and a loop waits again for what is left
/// (see `Poll::poll`). On Linux a child that exits while another thread is
/// inside `fork` (every test here runs on its own thread, and `fork` blocks
/// all signals around itself) leaves a pending `SIGCHLD` that wakes a
/// different thread's `epoll_wait` with `EINTR`, though no handler is
/// installed. So the wait is retried for the time that remains.
fn ready(poll: &mut Poll, events: &mut Events, wait: Duration) -> Vec<(Token, bool, bool)> {
    let deadline = Instant::now() + wait;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match poll.poll(events, Some(left)) {
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            done => break done.expect("poll"),
        }
    }
    events
        .iter()
        .map(|ev| (ev.token(), ev.is_readable(), ev.is_writable()))
        .collect()
}

/// Short, for "no event" assertions. Long, for "an event" assertions.
const QUIET: Duration = Duration::from_millis(100);
const SOON: Duration = Duration::from_secs(5);

/// A connected pair on loopback: the layer's non-blocking stream and a
/// blocking std peer.
fn pair() -> (net::TcpStream, std::net::TcpStream) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let peer = std::net::TcpStream::connect(addr).expect("connect");
    let (ours, _) = listener.accept().expect("accept");
    ours.set_nonblocking(true).expect("nonblocking");
    (net::TcpStream::from_std(ours), peer)
}

fn poll_and_events() -> (Poll, Events) {
    (Poll::new().expect("poll"), Events::with_capacity(256))
}

const T: Token = Token(3);

// ── 1. Poll ──────────────────────────────────────────────────────────────────

mod poll {
    use super::*;

    /// P1. Break: report readable on registration.
    #[test]
    fn a_quiet_socket_is_not_reported() {
        let (mut ours, _peer) = pair();
        let (mut poll, mut events) = poll_and_events();
        poll.registry()
            .register(&mut ours, T, Interest::READABLE)
            .expect("register");
        assert_eq!(ready(&mut poll, &mut events, QUIET), []);
    }

    /// P2. Break: no event, or the token twice.
    #[test]
    fn a_peer_write_is_one_readable_event() {
        let (mut ours, mut peer) = pair();
        let (mut poll, mut events) = poll_and_events();
        poll.registry()
            .register(&mut ours, T, Interest::READABLE)
            .expect("register");
        peer.write_all(b"hello").expect("peer write");
        let got = ready(&mut poll, &mut events, SOON);
        assert_eq!(got.len(), 1, "{got:?}");
        assert_eq!((got[0].0, got[0].1), (T, true), "{got:?}");
    }

    /// P3. Break: level-triggered behaviour, which reports again while data
    /// waits.
    #[test]
    fn no_repeat_without_a_new_edge() {
        let (mut ours, mut peer) = pair();
        let (mut poll, mut events) = poll_and_events();
        poll.registry()
            .register(&mut ours, T, Interest::READABLE)
            .expect("register");
        peer.write_all(b"hello").expect("peer write");
        assert_eq!(ready(&mut poll, &mut events, SOON).len(), 1);
        let mut one = [0u8; 1];
        assert_eq!(ours.read(&mut one).expect("read one"), 1);
        assert_eq!(
            ready(&mut poll, &mut events, QUIET),
            [],
            "four bytes still wait"
        );
    }

    /// P4. Break: the second edge missed.
    #[test]
    fn a_new_write_after_would_block_is_a_new_event() {
        let (mut ours, mut peer) = pair();
        let (mut poll, mut events) = poll_and_events();
        poll.registry()
            .register(&mut ours, T, Interest::READABLE)
            .expect("register");
        peer.write_all(b"hello").expect("peer write");
        assert_eq!(ready(&mut poll, &mut events, SOON).len(), 1);
        let mut buf = [0u8; 64];
        loop {
            match ours.read(&mut buf) {
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => panic!("read: {e}"),
            }
        }
        peer.write_all(b"again").expect("peer write");
        let got = ready(&mut poll, &mut events, SOON);
        assert_eq!(got.len(), 1, "{got:?}");
        assert!(got[0].1, "readable: {got:?}");
    }

    /// P5. Break: no writable event after the peer drains.
    #[test]
    fn writable_returns_when_the_peer_drains() {
        let (mut ours, mut peer) = pair();
        net::set_buffer_sizes(&ours, 16 * 1024, 16 * 1024).expect("sizes");
        net::set_buffer_sizes(&peer, 16 * 1024, 16 * 1024).expect("sizes");
        let (mut poll, mut events) = poll_and_events();
        poll.registry()
            .register(&mut ours, T, Interest::WRITABLE)
            .expect("register");
        assert!(ready(&mut poll, &mut events, SOON).iter().any(|e| e.2));
        let chunk = [b'x'; 64 * 1024];
        let mut sent = 0usize;
        loop {
            match ours.write(&chunk) {
                Ok(n) => sent += n,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => panic!("write: {e}"),
            }
        }
        assert!(sent > 0);
        let drained = std::thread::spawn(move || {
            let mut buf = vec![0u8; sent];
            peer.read_exact(&mut buf).expect("drain");
            peer
        });
        let got = ready(&mut poll, &mut events, SOON);
        assert!(got.iter().any(|e| e.0 == T && e.2), "writable: {got:?}");
        let _peer = drained.join().expect("drain thread");
    }

    /// P6. The reregistration shape. Break: `reregister` short-circuited when
    /// the kernel was already told, which is the bug kqueue hid on macOS.
    #[test]
    fn interest_changed_after_the_handler_reports_waiting_data() {
        let (mut ours, mut peer) = pair();
        let (mut poll, mut events) = poll_and_events();
        poll.registry()
            .register(&mut ours, T, Interest::WRITABLE)
            .expect("register");
        // A fresh socket is writable: take that report.
        assert!(ready(&mut poll, &mut events, SOON).iter().any(|e| e.2));
        peer.write_all(b"reply").expect("peer write");
        // Stale interest: WRITABLE registered, data waiting, no event.
        assert_eq!(ready(&mut poll, &mut events, QUIET), []);
        // The handler re-reads its interest and re-registers.
        poll.registry()
            .reregister(&mut ours, T, Interest::READABLE)
            .expect("reregister");
        let got = ready(&mut poll, &mut events, SOON);
        assert!(got.iter().any(|e| e.0 == T && e.1), "readable: {got:?}");
    }

    /// P7. Break: `reregister` skipped when the interest is unchanged.
    #[test]
    fn reregistering_a_ready_source_reports_it() {
        let (mut ours, mut peer) = pair();
        let (mut poll, mut events) = poll_and_events();
        poll.registry()
            .register(&mut ours, T, Interest::READABLE)
            .expect("register");
        peer.write_all(b"hello").expect("peer write");
        assert_eq!(ready(&mut poll, &mut events, SOON).len(), 1);
        assert_eq!(ready(&mut poll, &mut events, QUIET), []);
        poll.registry()
            .reregister(&mut ours, T, Interest::READABLE)
            .expect("reregister");
        let got = ready(&mut poll, &mut events, SOON);
        assert!(got.iter().any(|e| e.0 == T && e.1), "readable: {got:?}");
    }

    /// P8. Break: close not reported, or reported as an error only.
    #[test]
    fn peer_close_is_readable_then_eof() {
        let (mut ours, peer) = pair();
        let (mut poll, mut events) = poll_and_events();
        poll.registry()
            .register(&mut ours, T, Interest::READABLE)
            .expect("register");
        drop(peer);
        let got = ready(&mut poll, &mut events, SOON);
        assert!(got.iter().any(|e| e.0 == T && e.1), "readable: {got:?}");
        let mut one = [0u8; 1];
        assert_eq!(ours.read(&mut one).expect("read at eof"), 0);
    }

    /// P9. Break: a reset is silent.
    #[test]
    fn peer_reset_is_reported_then_read_errors() {
        let (mut ours, mut peer) = pair();
        let (mut poll, mut events) = poll_and_events();
        poll.registry()
            .register(&mut ours, T, Interest::READABLE)
            .expect("register");
        peer.write_all(b"unread").expect("peer write");
        assert_eq!(ready(&mut poll, &mut events, SOON).len(), 1);
        // Unread data on our side plus linger 0 on the peer's: the close is a
        // reset.
        socket2::SockRef::from(&peer)
            .set_linger(Some(Duration::ZERO))
            .expect("linger");
        drop(peer);
        let got = ready(&mut poll, &mut events, SOON);
        assert!(got.iter().any(|e| e.0 == T), "an event: {got:?}");
        let mut buf = [0u8; 64];
        let mut saw_error = false;
        for _ in 0..3 {
            match ours.read(&mut buf) {
                Ok(0) => break,
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(_) => {
                    saw_error = true;
                    break;
                }
            }
        }
        assert!(saw_error, "the reset reaches the read");
    }

    /// P10. Break: no writable event for a completed connect.
    #[test]
    fn a_completed_connect_is_writable() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let (stream, _state) =
            net::connect_tcp(listener.local_addr().expect("addr")).expect("connect");
        let mut stream = net::TcpStream::from_std(stream);
        let (mut poll, mut events) = poll_and_events();
        poll.registry()
            .register(&mut stream, T, Interest::WRITABLE)
            .expect("register");
        let got = ready(&mut poll, &mut events, SOON);
        assert!(got.iter().any(|e| e.0 == T && e.2), "writable: {got:?}");
        assert!(stream.take_error().expect("take_error").is_none());
    }

    /// P11. Break: a refused connect hangs.
    #[test]
    fn a_refused_connect_is_reported_with_its_error() {
        // Port 1 (tcpmux) on loopback: nothing listens there, and a parallel
        // test binding port 0 is never handed it. A port bound and released
        // here would be handed to another test between the release and the
        // connect.
        let closed: SocketAddr = "127.0.0.1:1".parse().expect("addr");
        let (stream, _state) = net::connect_tcp(closed).expect("connect starts");
        let mut stream = net::TcpStream::from_std(stream);
        let (mut poll, mut events) = poll_and_events();
        poll.registry()
            .register(&mut stream, T, Interest::WRITABLE)
            .expect("register");
        let got = ready(&mut poll, &mut events, SOON);
        assert!(got.iter().any(|e| e.0 == T), "an event: {got:?}");
        let err = stream
            .take_error()
            .expect("take_error")
            .or_else(|| stream.peer_addr().err())
            .expect("the connect failed");
        assert!(
            matches!(
                err.kind(),
                io::ErrorKind::ConnectionRefused | io::ErrorKind::NotConnected
            ),
            "{err}"
        );
    }

    /// P12. Break: one accept per event.
    #[test]
    fn accept_drains_the_backlog_then_would_block() {
        let mut listener = net::listen_tcp("127.0.0.1:0".parse().unwrap()).expect("listen");
        let addr = listener.local_addr().expect("addr");
        let (mut poll, mut events) = poll_and_events();
        poll.registry()
            .register(&mut listener, T, Interest::READABLE)
            .expect("register");
        let held: Vec<_> = (0..3)
            .map(|_| std::net::TcpStream::connect(addr).expect("connect"))
            .collect();
        // The kernel moves each connection to the accept queue as its
        // handshake completes, so a connection can land after the first
        // drain. Each drain runs to `WouldBlock`, and a connection still
        // missing must bring its own event.
        let mut accepted = Vec::new();
        while accepted.len() < 3 {
            assert!(!ready(&mut poll, &mut events, SOON).is_empty());
            loop {
                match listener.accept() {
                    Ok((s, _)) => accepted.push(s),
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                    Err(e) => panic!("accept: {e}"),
                }
            }
        }
        assert_eq!(accepted.len(), 3);
        let fourth = std::net::TcpStream::connect(addr).expect("connect");
        assert!(!ready(&mut poll, &mut events, SOON).is_empty());
        accepted.push(listener.accept().expect("the fourth").0);
        drop((held, fourth));
    }

    /// P13. Break: an event after deregister.
    #[test]
    fn a_deregistered_source_is_silent() {
        let (mut ours, mut peer) = pair();
        let (mut poll, mut events) = poll_and_events();
        poll.registry()
            .register(&mut ours, T, Interest::READABLE)
            .expect("register");
        poll.registry().deregister(&mut ours).expect("deregister");
        peer.write_all(b"hello").expect("peer write");
        assert_eq!(ready(&mut poll, &mut events, QUIET), []);
    }

    /// P14. Break: the wrong token.
    #[test]
    fn the_ready_token_among_many_is_the_one_reported() {
        let mut pairs: Vec<_> = (0..64).map(|_| pair()).collect();
        let (mut poll, mut events) = poll_and_events();
        for (i, (ours, _)) in pairs.iter_mut().enumerate() {
            poll.registry()
                .register(ours, Token(i), Interest::READABLE)
                .expect("register");
        }
        pairs[41].1.write_all(b"me").expect("peer write");
        assert_eq!(
            ready(&mut poll, &mut events, SOON),
            [(Token(41), true, false)]
        );
    }

    /// P15. Break: blocks, or returns early.
    #[test]
    fn a_timeout_with_nothing_ready_returns_empty() {
        let (mut ours, _peer) = pair();
        let (mut poll, mut events) = poll_and_events();
        poll.registry()
            .register(&mut ours, T, Interest::READABLE)
            .expect("register");
        let t0 = Instant::now();
        assert_eq!(ready(&mut poll, &mut events, Duration::from_millis(50)), []);
        let dt = t0.elapsed();
        assert!(
            dt >= Duration::from_millis(50) && dt < Duration::from_secs(2),
            "{dt:?}"
        );
    }

    /// P16. Break: a datagram is not reported.
    #[test]
    fn a_datagram_is_readable() {
        let mut sock = net::UdpSocket::bind("127.0.0.1:0".parse().unwrap()).expect("bind");
        let addr = sock.local_addr().expect("addr");
        let (mut poll, mut events) = poll_and_events();
        poll.registry()
            .register(&mut sock, T, Interest::READABLE)
            .expect("register");
        let sender = std::net::UdpSocket::bind("127.0.0.1:0").expect("sender");
        sender.send_to(b"dgram", addr).expect("send");
        let got = ready(&mut poll, &mut events, SOON);
        assert_eq!(got.len(), 1, "{got:?}");
        assert!(got[0].1);
    }

    /// P17. Break: `register_fd` registers the wrong descriptor.
    #[test]
    fn register_fd_watches_a_bare_descriptor() {
        let (ours, mut peer) = std::os::unix::net::UnixStream::pair().expect("pair");
        ours.set_nonblocking(true).expect("nonblocking");
        let (mut poll, mut events) = poll_and_events();
        poll.registry()
            .register_fd(ours.as_raw_fd(), T, Interest::READABLE)
            .expect("register_fd");
        assert_eq!(ready(&mut poll, &mut events, QUIET), []);
        peer.write_all(b"x").expect("peer write");
        assert_eq!(ready(&mut poll, &mut events, SOON), [(T, true, false)]);
        poll.registry()
            .deregister_fd(ours.as_raw_fd())
            .expect("deregister_fd");
        peer.write_all(b"y").expect("peer write");
        assert_eq!(ready(&mut poll, &mut events, QUIET), []);
    }
}

// ── 3. ChildExit ─────────────────────────────────────────────────────────────

#[allow(
    clippy::zombie_processes,
    reason = "every child here is reaped by ChildExit::reap, the layer's waitpid, which the lint cannot see"
)]
mod child_exit {
    use super::*;
    use m6_platform::ChildExit;
    use std::os::unix::process::ExitStatusExt;
    use std::process::{Child, Command, Stdio};

    fn bash(script: &str) -> Child {
        Command::new("bash")
            .args(["-c", script])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawn bash")
    }

    /// Wait for the exit descriptor and reap. Panics at the deadline.
    fn reap_when_reported(exit: &mut ChildExit) -> std::process::ExitStatus {
        let (mut poll, mut events) = poll_and_events();
        poll.registry()
            .register_fd(exit.raw_fd(), T, Interest::READABLE)
            .expect("register the exit descriptor");
        let got = ready(&mut poll, &mut events, SOON);
        assert!(
            got.iter().any(|e| e.0 == T),
            "the exit is reported: {got:?}"
        );
        exit.reap().expect("reap")
    }

    /// C1. Break: no event, or no reap (a zombie left).
    #[test]
    fn a_child_that_exits_zero_is_reported_and_reaped() {
        let child = bash("exit 0");
        let pid = child.id();
        let mut exit = ChildExit::new(&child).expect("ChildExit::new");
        let status = reap_when_reported(&mut exit);
        assert!(status.success(), "{status}");
        assert!(!process::is_alive(pid), "reaped: the pid is gone");
    }

    /// C2. Break: the status lost.
    #[test]
    fn a_nonzero_status_is_reported_as_that_status() {
        let child = bash("exit 3");
        let mut exit = ChildExit::new(&child).expect("ChildExit::new");
        assert_eq!(reap_when_reported(&mut exit).code(), Some(3));
    }

    /// C3. Break: reported as exit 0.
    #[test]
    fn a_child_killed_by_a_signal_is_reported_as_killed() {
        let mut child = bash("read x");
        let mut exit = ChildExit::new(&child).expect("ChildExit::new");
        child.kill().expect("SIGKILL");
        let status = reap_when_reported(&mut exit);
        assert_eq!(
            (status.code(), status.signal()),
            (None, Some(9)),
            "{status}"
        );
    }

    /// C4. render-admin's case. Break: a watcher on the pipe instead of the
    /// process, or a blocking wait.
    #[test]
    fn a_child_that_closes_stdout_is_not_reported_until_it_exits() {
        let mut child = bash("exec 1>&-; read x; true");
        let mut exit = ChildExit::new(&child).expect("ChildExit::new");
        let mut out = Vec::new();
        child
            .stdout
            .take()
            .expect("stdout")
            .read_to_end(&mut out)
            .expect("the pipe reaches EOF when the child closes stdout");
        let (mut poll, mut events) = poll_and_events();
        poll.registry()
            .register_fd(exit.raw_fd(), T, Interest::READABLE)
            .expect("register");
        let (mut other, mut peer) = pair();
        poll.registry()
            .register(&mut other, Token(9), Interest::READABLE)
            .expect("register the other socket");
        assert_eq!(
            ready(&mut poll, &mut events, QUIET),
            [],
            "stdout closed is not an exit"
        );
        peer.write_all(b"served").expect("peer write");
        assert_eq!(
            ready(&mut poll, &mut events, SOON),
            [(Token(9), true, false)]
        );
        drop(child.stdin.take());
        let got = ready(&mut poll, &mut events, SOON);
        assert!(
            got.iter().any(|e| e.0 == T),
            "the exit is reported: {got:?}"
        );
        assert!(exit.reap().expect("reap").success());
    }

    /// C5. Break: kqueue `EVFILT_PROC` on a zombie is `ESRCH` and the arm gives
    /// up, so the exit is never reported.
    #[test]
    fn an_already_exited_child_is_reported_at_once() {
        let mut child = bash("exit 7");
        let mut out = Vec::new();
        child
            .stdout
            .take()
            .expect("stdout")
            .read_to_end(&mut out)
            .expect("EOF: the child has exited");
        let mut exit = ChildExit::new(&child).expect("ChildExit::new on an exited child");
        assert_eq!(reap_when_reported(&mut exit).code(), Some(7));
    }

    /// C6. Break: the statuses crossed. Each descriptor is deregistered on its
    /// report and then reaped, the loop's order, because a reaped descriptor
    /// left registered is reported again on Linux.
    #[test]
    fn two_children_report_their_own_status() {
        let one = bash("exit 1");
        let two = bash("exit 2");
        let mut exit_one = ChildExit::new(&one).expect("one");
        let mut exit_two = ChildExit::new(&two).expect("two");
        let (mut poll, mut events) = poll_and_events();
        poll.registry()
            .register_fd(exit_one.raw_fd(), Token(1), Interest::READABLE)
            .expect("register one");
        poll.registry()
            .register_fd(exit_two.raw_fd(), Token(2), Interest::READABLE)
            .expect("register two");
        let deadline = Instant::now() + SOON;
        let mut codes = [None, None];
        while codes.iter().any(Option::is_none) {
            assert!(Instant::now() < deadline, "both exits reported: {codes:?}");
            for (token, _, _) in ready(&mut poll, &mut events, SOON) {
                let (exit, slot) = match token {
                    Token(1) => (&mut exit_one, 0),
                    Token(2) => (&mut exit_two, 1),
                    t => panic!("unexpected token {t:?}"),
                };
                poll.registry()
                    .deregister_fd(exit.raw_fd())
                    .expect("deregister on the report");
                codes[slot] = exit.reap().expect("reap").code();
            }
        }
        assert_eq!(codes, [Some(1), Some(2)]);
    }

    /// C8. Break: the probe reports the wrong thing.
    #[test]
    fn supported_answers_at_start() {
        ChildExit::supported().expect("a child's exit can be a descriptor here");
    }
}

// ── 3b. The harness: what a spawned child inherits ───────────────────────────

mod spawn {
    use super::*;
    use m6_platform::process::{Io, Streams};
    use std::process::{Command, Stdio};

    /// Every standard stream to `/dev/null`.
    const NO_STREAMS: Streams = Streams {
        stdin: Io::Null,
        stdout: Io::Null,
        stderr: Io::Null,
    };

    /// C11. Break: `spawn` by fork and exec on macOS, as `std`'s
    /// `Command::spawn` does whenever a `pre_exec` is set. The child keeps the
    /// listener, holds the port in LISTEN after this process has closed it,
    /// and the rebind below fails with `Address already in use`.
    ///
    /// On macOS a socket is created, and marked close-on-exec by a second call.
    /// A spawn by another thread between the two copies a listener with no
    /// close-on-exec flag. `dup` makes that state on demand: the duplicate
    /// carries no flag. So this is the race with its window held open, rather
    /// than a thread that hits the window some of the time.
    #[test]
    fn a_listener_without_close_on_exec_is_not_inherited() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("local_addr").port();
        let bare = platform::fd::dup(&listener).expect("a duplicate with no close-on-exec");
        let mut child = process::spawn(Command::new("sleep").arg("30"), NO_STREAMS).expect("spawn");
        let listening = process::listens_on_tcp(child.id());
        drop(bare);
        drop(listener);
        // A child another test thread has forked and not yet exec'd holds a
        // copy of the listener (close-on-exec acts only at its exec), so the
        // port is released a moment later. Retry only `AddrInUse`, to a
        // deadline: the child above is what must not hold it, and it has
        // exec'd by the time `spawn` returns.
        let deadline = Instant::now() + SOON;
        let rebound = loop {
            match std::net::TcpListener::bind(("127.0.0.1", port)) {
                Err(e) if e.kind() == io::ErrorKind::AddrInUse && Instant::now() < deadline => {
                    std::thread::yield_now();
                }
                done => break done,
            }
        };
        let _ = child.kill();
        let _ = child.wait();
        assert!(
            !listening.expect("the child's sockets can be read"),
            "the child holds a TCP listener it was never given"
        );
        assert!(
            rebound.is_ok(),
            "port {port} is still bound after this process closed its listener: {rebound:?}"
        );
    }

    /// C9. Break: a child that holds a descriptor above stderr after `exec`, so
    /// a lock this process holds outlives it.
    ///
    /// The child is this binary again, running the ignored helper below, and it
    /// names the descriptors it holds after `exec`. The parent cannot ask
    /// itself: `fork` copies the descriptor table before `exec`, so a copy of
    /// the lock is in the child for the moment between the fork and its `exec`,
    /// and a `try_lock` from this process in that moment fails whether or not
    /// the descriptor would survive `exec`. The contract is about the table a
    /// child starts with, which is the one the child reports.
    ///
    /// Rust marks a descriptor it opens close-on-exec itself, so a missing
    /// sweep is not what this separates. It separates a child that holds the
    /// lock after `exec` from one that does not.
    #[test]
    fn a_spawned_child_inherits_no_descriptor_above_stderr() {
        let dir = tempfile::tempdir_in(platform::socket_scratch_root()).expect("tempdir");
        let path = dir.path().join("lock");
        let held = std::fs::File::create(&path).expect("create");
        held.try_lock().expect("this process takes the lock");
        // The harness's start-time sweep, as a service runs it at start.
        process::close_inherited_on_exec().expect("sweep");
        let exe = std::env::current_exe().expect("current_exe");
        let child = process::spawn(
            Command::new(exe)
                .args([
                    "--exact",
                    "--ignored",
                    "--nocapture",
                    "spawn::a_spawned_child_reports_what_it_inherited",
                ])
                .env("M6_TEST_INHERITED_LOCK", &path),
            Streams {
                stdin: Io::Null,
                stdout: Io::Piped,
                stderr: Io::Piped,
            },
        )
        .expect("spawn");
        drop(held);
        let out = child.wait_with_output().expect("the child's report");
        let text = format!(
            "{}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(out.status.success(), "{text}");
        assert!(
            text.contains("no descriptor above stderr refers to the lock"),
            "the child must report the table it holds: {text}"
        );
    }

    /// The child half of C9: after `exec`, name every descriptor that is the
    /// lock file. One is the failure the test is named for.
    ///
    /// The inode is the identity both targets agree on. macOS `/dev/fd` is its
    /// own filesystem and reports a device of its own for an entry there, while
    /// the inode is the file's, so a device comparison would miss every
    /// descriptor on macOS.
    #[test]
    #[ignore = "run by a_spawned_child_inherits_no_descriptor_above_stderr in its own process"]
    fn a_spawned_child_reports_what_it_inherited() {
        use std::os::unix::fs::MetadataExt;
        let path = std::env::var_os("M6_TEST_INHERITED_LOCK").expect("the lock file");
        let path = std::path::Path::new(&path);
        let want = std::fs::metadata(path).expect("stat the lock");
        let ino = want.ino();
        let mut held = Vec::new();
        for entry in std::fs::read_dir("/dev/fd").expect("/dev/fd") {
            let Ok(entry) = entry else { continue };
            let Ok(fd) = entry.file_name().to_string_lossy().parse::<i32>() else {
                continue;
            };
            if fd <= 2 {
                continue;
            }
            let Ok(meta) = std::fs::metadata(entry.path()) else {
                continue;
            };
            if meta.is_file() && meta.ino() == ino {
                held.push(fd);
            }
        }
        assert!(
            held.is_empty(),
            "the child held the lock it should never have inherited: {held:?}"
        );
        println!("no descriptor above stderr refers to the lock");
    }

    /// C10. Break: a harness that puts its servers in another process group,
    /// or a server that ignores SIGINT: an interrupt from the terminal reaches
    /// the harness and not them, and they outlive it. The harness here is this
    /// binary running `an_interrupted_harness_leaves_no_server_inside`, in a
    /// group of its own so the interrupt reaches nothing else.
    #[test]
    fn an_interrupted_harness_leaves_no_server() {
        use std::io::BufRead;
        use std::os::unix::process::CommandExt;
        let exe = std::env::current_exe().expect("current_exe");
        let mut harness = Command::new(exe)
            .args([
                "--exact",
                "--ignored",
                "--nocapture",
                "spawn::an_interrupted_harness_leaves_no_server_inside",
            ])
            .process_group(0)
            .stdout(Stdio::piped())
            .spawn()
            .expect("start the harness");
        let mut lines = std::io::BufReader::new(harness.stdout.take().expect("stdout")).lines();
        let server: u32 = lines
            .by_ref()
            .find_map(|l| {
                l.ok()?
                    .strip_prefix("server ")
                    .map(|p| p.parse().expect("a pid"))
            })
            .expect("the harness names its server");
        assert!(process::is_alive(server));
        // SIGINT to the harness's process group, which is what a terminal's
        // interrupt does. A negative pid names a group, and bash's builtin
        // `kill` takes it the same way on every target (the `kill` binaries
        // differ in how they read it).
        let status = Command::new("bash")
            .args(["-c", "kill -INT -- -$1", "kill", &harness.id().to_string()])
            .status()
            .expect("kill runs");
        assert!(status.success(), "{status}");
        let deadline = Instant::now() + SOON;
        while process::is_alive(server) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = harness.wait();
        assert!(
            !process::is_alive(server),
            "the server outlived the interrupted harness"
        );
    }

    /// The harness half of C10: start a server through the layer, name it,
    /// then wait to be interrupted.
    #[test]
    #[ignore = "run by an_interrupted_harness_leaves_no_server in its own process group"]
    fn an_interrupted_harness_leaves_no_server_inside() {
        let mut server =
            process::spawn(Command::new("sleep").arg("30"), NO_STREAMS).expect("start the server");
        println!("server {}", server.id());
        let _ = server.wait();
    }
}

// ── 4. Unix and TCP sockets ──────────────────────────────────────────────────

mod sockets {
    use super::*;
    use m6_platform::MAX_SOCKET_PATH;

    /// A path of exactly `len` bytes under a fresh directory.
    fn path_of_len(dir: &std::path::Path, len: usize) -> std::path::PathBuf {
        let base = dir.join("s");
        let have = base.as_os_str().len();
        assert!(
            have < len,
            "the temp directory is short enough: {have} < {len}"
        );
        let mut name = String::from("s");
        name.push_str(&"x".repeat(len - have));
        dir.join(name)
    }

    /// U1. Break: a path within the limit refused.
    #[test]
    fn a_unix_listener_binds_within_the_limit() {
        let dir = tempfile::tempdir_in(platform::socket_scratch_root()).expect("tempdir");
        let path = path_of_len(dir.path(), MAX_SOCKET_PATH - 1);
        let listener = net::bind_unix(&path, 0o600).expect("bind at the limit");
        let bound = listener.local_addr().expect("addr");
        assert_eq!(bound.as_pathname(), Some(path.as_path()));
    }

    /// U2. Break: the OS's own text, or truncation.
    #[test]
    fn a_path_over_the_limit_names_the_path_and_the_limit() {
        let dir = tempfile::tempdir_in(platform::socket_scratch_root()).expect("tempdir");
        let path = path_of_len(dir.path(), MAX_SOCKET_PATH);
        let err = net::bind_unix(&path, 0o600).expect_err("one byte too long");
        let msg = err.to_string();
        assert!(msg.contains(&path.display().to_string()), "{msg}");
        assert!(
            msg.contains(&format!("is {MAX_SOCKET_PATH} bytes")),
            "the message names the length: {msg}"
        );
        assert!(
            msg.contains(&format!("at most {}", MAX_SOCKET_PATH - 1)),
            "the message names the limit: {msg}"
        );
        let err = net::connect_unix(&path).expect_err("connect is checked too");
        assert!(err.to_string().contains(&path.display().to_string()));
    }

    /// U3. A full queue never connects and never hangs: the connect fails at
    /// once with `WouldBlock` (Linux, the listener is busy) or
    /// `ConnectionRefused` (macOS, which cannot tell a full queue from a dead
    /// listener). Break: a connect that blocks waiting for an accept, or an
    /// error of any other kind.
    #[test]
    fn a_full_backlog_is_refused_at_once() {
        use socket2::{Domain, SockAddr, Socket, Type};
        let dir = tempfile::tempdir_in(platform::socket_scratch_root()).expect("tempdir");
        let path = dir.path().join("full.sock");
        let listener = Socket::new(Domain::UNIX, Type::STREAM, None).expect("listener");
        listener
            .bind(&SockAddr::unix(&path).expect("addr"))
            .expect("bind");
        listener.listen(0).expect("listen with backlog 0");
        let mut fillers = Vec::new();
        let t0 = Instant::now();
        let busy = loop {
            assert!(fillers.len() < 256, "the backlog never filled");
            assert!(
                t0.elapsed() < Duration::from_secs(5),
                "a filler connect blocked"
            );
            match net::connect_unix(&path) {
                Ok(s) => fillers.push(s),
                Err(e) => break e,
            }
        };
        assert!(!fillers.is_empty(), "the first connect succeeds");
        assert!(
            matches!(
                busy.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::ConnectionRefused
            ),
            "{busy}"
        );
        drop((fillers, listener));
    }

    /// U4. Break: a different kind.
    #[test]
    fn a_missing_socket_file_is_not_found() {
        let dir = tempfile::tempdir_in(platform::socket_scratch_root()).expect("tempdir");
        let err = net::connect_unix(&dir.path().join("none.sock")).expect_err("no file");
        assert_eq!(err.kind(), io::ErrorKind::NotFound, "{err}");
    }

    /// U5. Break: a socket without `SO_NOSIGPIPE` on an arm that does not
    /// ignore SIGPIPE: the write becomes a signal and the process dies here.
    #[test]
    fn a_write_to_a_closed_peer_is_an_error_not_a_signal() {
        let mut listener = net::listen_tcp("127.0.0.1:0".parse().unwrap()).expect("listen");
        let (peer, _) = net::connect_tcp(listener.local_addr().expect("addr")).expect("connect");
        let (mut poll, mut events) = poll_and_events();
        poll.registry()
            .register(&mut listener, T, Interest::READABLE)
            .expect("register");
        assert!(!ready(&mut poll, &mut events, SOON).is_empty());
        let (mut ours, _) = listener.accept().expect("accept");
        drop(peer);
        let chunk = [b'z'; 64 * 1024];
        let mut err = None;
        for _ in 0..64 {
            match ours.write(&chunk) {
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(e) => {
                    err = Some(e);
                    break;
                }
            }
        }
        let err = err.expect("the write to a closed peer errors");
        assert!(
            matches!(
                err.kind(),
                io::ErrorKind::BrokenPipe | io::ErrorKind::ConnectionReset
            ),
            "{err}"
        );
    }

    /// U6. Break: the sizes ignored.
    #[test]
    fn buffer_sizes_are_set_as_asked() {
        let (ours, _peer) = pair();
        net::set_buffer_sizes(&ours, 64 * 1024, 64 * 1024).expect("set");
        let (send, recv) = net::buffer_sizes(&ours).expect("read back");
        assert!(send >= 64 * 1024 && recv >= 64 * 1024, "{send} {recv}");
    }

    /// U7. Break: the wrong address.
    #[test]
    fn a_tcp_listener_on_port_zero_reports_its_port() {
        let listener = net::listen_tcp("127.0.0.1:0".parse().unwrap()).expect("listen");
        let addr = listener.local_addr().expect("addr");
        assert_ne!(addr.port(), 0);
        assert!(addr.ip().is_loopback());
        std::net::TcpStream::connect(addr).expect("reachable");
    }

    /// U8. Break: a stale file refused, or a live listener's file unlinked.
    ///
    /// The file is made by another process that binds it and exits, which is
    /// what leaves a stale socket behind. Binding it here and closing it would
    /// leave the same file, but `fork` copies this process's descriptor table
    /// before the child execs, and a fork that caught the listener between its
    /// `bind` and the `close` here answers `connect`, so the file reads as live
    /// to the bind below. A process that has exited holds no descriptor at all.
    #[test]
    fn a_stale_socket_file_is_replaced_at_bind() {
        use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
        let dir = tempfile::tempdir_in(platform::socket_scratch_root()).expect("tempdir");
        let path = dir.path().join("nested").join("m6.sock");
        let exe = std::env::current_exe().expect("current_exe");
        let out = std::process::Command::new(exe)
            .args([
                "--exact",
                "--ignored",
                "--nocapture",
                "sockets::a_stale_socket_file_is_left_by_a_process_that_is_gone",
            ])
            .env("M6_TEST_STALE_SOCKET", &path)
            .output()
            .expect("run the process that leaves the socket");
        let text = String::from_utf8_lossy(&out.stdout).into_owned();
        assert!(out.status.success(), "{text}");
        assert!(path.exists(), "the socket file is left behind");
        let stale = std::fs::metadata(&path).expect("stat the stale file");
        assert!(
            stale.file_type().is_socket(),
            "the file a gone process left is a socket"
        );
        let (stale_dev, stale_ino) = (stale.dev(), stale.ino());

        let replaced = net::bind_unix(&path, 0o660).expect("a stale socket is replaced");
        let fresh = std::fs::metadata(&path).expect("stat the socket just bound");
        assert!(
            (fresh.dev(), fresh.ino()) != (stale_dev, stale_ino),
            "the stale socket file is replaced, not reused"
        );
        assert_eq!(fresh.permissions().mode() & 0o777, 0o660);
        drop(replaced);
    }

    /// The process half of U8: bind the socket, create its parent, and exit,
    /// which is the state a killed service leaves.
    #[test]
    #[ignore = "run by a_stale_socket_file_is_replaced_at_bind in a process that then exits"]
    fn a_stale_socket_file_is_left_by_a_process_that_is_gone() {
        let path = std::env::var_os("M6_TEST_STALE_SOCKET").expect("the socket path");
        let path = std::path::Path::new(&path);
        drop(net::bind_unix(path, 0o660).expect("first bind creates the parent"));
        println!("left {}", path.display());
    }

    /// U10. Break: `bind_unix` unlinking whatever is at the path, so a second
    /// instance steals a live listener's socket.
    #[test]
    fn a_live_listeners_socket_is_refused_not_replaced() {
        let dir = tempfile::tempdir_in(platform::socket_scratch_root()).expect("tempdir");
        let path = dir.path().join("live.sock");
        let first = net::bind_unix(&path, 0o600).expect("first bind");
        let err = net::bind_unix(&path, 0o600).expect_err("a live listener is refused");
        assert_eq!(err.kind(), io::ErrorKind::AddrInUse, "{err}");
        assert!(
            err.to_string().contains(&path.display().to_string()),
            "{err}"
        );
        assert!(path.exists(), "the live socket file is left alone");
        drop(first);
    }

    /// U11. Break: a regular file at the path removed as if it were a stale
    /// socket.
    #[test]
    fn a_non_socket_at_the_path_is_an_error_naming_it() {
        let dir = tempfile::tempdir_in(platform::socket_scratch_root()).expect("tempdir");
        let path = dir.path().join("not-a-socket");
        std::fs::write(&path, b"data").expect("write");
        let err = net::bind_unix(&path, 0o600).expect_err("a file is not replaced");
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists, "{err}");
        assert!(err.to_string().contains("not a socket"), "{err}");
        assert!(path.exists(), "the file is left alone");
    }

    /// U9. Break: `EINPROGRESS` returned as an error.
    #[test]
    fn a_tcp_connect_reports_in_progress_or_connected() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let (stream, state) =
            net::connect_tcp(listener.local_addr().expect("addr")).expect("connect");
        assert!(matches!(
            state,
            net::Connect::Connected | net::Connect::InProgress
        ));
        let mut stream = net::TcpStream::from_std(stream);
        let (mut poll, mut events) = poll_and_events();
        poll.registry()
            .register(&mut stream, T, Interest::WRITABLE)
            .expect("register");
        assert!(ready(&mut poll, &mut events, SOON).iter().any(|e| e.2));
        assert!(stream.take_error().expect("take_error").is_none());
    }

    /// U12. Two listeners on one port, every connection answered, and dropping
    /// one leaves the port served. Break: the second bind refused because
    /// `SO_REUSEPORT` was not set before it, a listener that answers nothing,
    /// or a stop that leaves the port dead.
    ///
    /// The test counts the connections answered, never the split between the
    /// two listeners: Linux hashes the 4-tuple to choose a socket and macOS
    /// does not, so the split is not even and is not the subject.
    #[test]
    fn two_shared_tcp_listeners_share_one_port_and_one_may_stop() {
        let (mut first, _udp) =
            net::listen_shared(SocketAddr::from(([127, 0, 0, 1], 0)), None).expect("first bind");
        let addr = first.local_addr().expect("addr");
        let (mut second, _udp2) =
            net::listen_shared(addr, None).expect("second bind on the same port");

        fn answer_all(listener: &mut std::net::TcpListener, tag: u8, answered: &mut usize) {
            loop {
                match listener.accept() {
                    Ok((mut conn, _)) => {
                        conn.write_all(&[tag]).expect("answer");
                        *answered += 1;
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => return,
                    Err(e) => panic!("accept: {e}"),
                }
            }
        }

        let other = Token(4);
        let mut poll = Poll::new().expect("poll");
        let mut events = Events::with_capacity(64);
        poll.registry()
            .register_fd(first.as_raw_fd(), T, Interest::READABLE)
            .expect("register first");
        poll.registry()
            .register_fd(second.as_raw_fd(), other, Interest::READABLE)
            .expect("register second");

        let mut answered = 0;
        for i in 0..8 {
            let mut client =
                std::net::TcpStream::connect(addr).expect("connect to the shared port");
            client.set_read_timeout(Some(SOON)).expect("read timeout");
            let waits = ready(&mut poll, &mut events, SOON);
            assert!(
                !waits.is_empty(),
                "connection {i} produced no readable listener"
            );
            for (token, readable, _) in waits {
                assert!(readable);
                match token {
                    T => answer_all(&mut first, b'a', &mut answered),
                    t if t == other => answer_all(&mut second, b'b', &mut answered),
                    other => panic!("unexpected token {other:?}"),
                }
            }
            let mut tag = [0u8; 1];
            client
                .read_exact(&mut tag)
                .expect("every connection is answered");
            assert!(tag[0] == b'a' || tag[0] == b'b', "tag {tag:?}");
        }
        assert_eq!(answered, 8, "every connection is answered once");

        // Stop one listener. The other still owns the port, so every later
        // connection is answered.
        poll.registry()
            .deregister_fd(first.as_raw_fd())
            .expect("deregister first");
        drop(first);
        // The stopped listener is gone when the last copy of its descriptor
        // is closed. A child another test thread has forked and not yet
        // exec'd holds a copy until then, and the kernel still hashes some
        // connections to it, where nobody accepts. So each connection is
        // retried, to one deadline, until the remaining listener answers it.
        let deadline = Instant::now() + SOON;
        for i in 0..8 {
            loop {
                let mut client =
                    std::net::TcpStream::connect(addr).expect("connect after one listener stopped");
                client.set_read_timeout(Some(SOON)).expect("read timeout");
                if !ready(&mut poll, &mut events, QUIET).is_empty() {
                    answer_all(&mut second, b'b', &mut answered);
                    let mut tag = [0u8; 1];
                    client
                        .read_exact(&mut tag)
                        .expect("the remaining listener answers");
                    assert_eq!(tag[0], b'b');
                    break;
                }
                assert!(Instant::now() < deadline, "connection {i} after the stop");
            }
        }
        assert_eq!(answered, 16, "the port is served after the stop");
    }

    /// U13. Two UDP sockets on one port, every datagram delivered exactly
    /// once, and dropping one leaves the port served. Break: the second bind
    /// refused, a datagram delivered twice or to neither, or a stop that
    /// leaves the port dead.
    ///
    /// No assertion counts datagrams per socket: a UDP datagram goes to one
    /// socket on both targets, and neither picks an even split.
    #[test]
    fn two_shared_udp_sockets_share_one_port_and_one_may_stop() {
        let (_tcp, first) =
            net::listen_shared(SocketAddr::from(([127, 0, 0, 1], 0)), None).expect("first bind");
        let addr = first.local_addr().expect("addr");
        let (_tcp2, second) = net::listen_shared(addr, None).expect("second bind on the same port");

        let other = Token(4);
        let mut poll = Poll::new().expect("poll");
        let mut events = Events::with_capacity(64);
        poll.registry()
            .register_fd(first.as_raw_fd(), T, Interest::READABLE)
            .expect("register first");
        poll.registry()
            .register_fd(second.as_raw_fd(), other, Interest::READABLE)
            .expect("register second");

        fn drain(sock: &std::net::UdpSocket, from: &str, got: &mut Vec<String>) {
            loop {
                let mut buf = [0u8; 64];
                match sock.recv_from(&mut buf) {
                    Ok((n, _)) => {
                        got.push(format!("{from}:{}", String::from_utf8_lossy(&buf[..n])))
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => return,
                    Err(e) => panic!("recv_from: {e}"),
                }
            }
        }

        let mut got = Vec::new();
        for i in 0..6 {
            let client = std::net::UdpSocket::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
                .expect("client socket");
            client
                .send_to(format!("d{i}").as_bytes(), addr)
                .expect("send to the shared port");
            let waits = ready(&mut poll, &mut events, SOON);
            assert!(!waits.is_empty(), "datagram {i} reached neither socket");
            let before = got.len();
            for (token, readable, _) in waits {
                assert!(readable);
                match token {
                    T => drain(&first, "first", &mut got),
                    t if t == other => drain(&second, "second", &mut got),
                    other => panic!("unexpected token {other:?}"),
                }
            }
            assert_eq!(
                got.len() - before,
                1,
                "datagram {i} is delivered exactly once: {got:?}"
            );
        }
        for i in 0..6 {
            let (_, payload) = got[i].split_once(':').expect("a sender prefix");
            assert_eq!(
                payload,
                format!("d{i}"),
                "datagram {i} arrived once: {got:?}"
            );
        }

        // Stop one socket. The other still owns the port.
        poll.registry()
            .deregister_fd(first.as_raw_fd())
            .expect("deregister first");
        drop(first);
        // A child another test thread has forked and not yet exec'd holds a
        // copy of the stopped socket, and the kernel still hashes a datagram
        // to it, where nobody reads. So send until one arrives at `second`.
        let client = std::net::UdpSocket::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .expect("client socket");
        let deadline = Instant::now() + SOON;
        loop {
            client
                .send_to(b"after", addr)
                .expect("send after one socket stopped");
            if !ready(&mut poll, &mut events, QUIET).is_empty() {
                break;
            }
            assert!(Instant::now() < deadline, "the port is dead after the stop");
        }
        drain(&second, "second", &mut got);
        assert_eq!(
            got.last().map(String::as_str),
            Some("second:after"),
            "the remaining socket receives: {got:?}"
        );
    }
}

// ── 5. Clock ─────────────────────────────────────────────────────────────────

mod clock {
    use super::*;

    /// T3. Break: the wall clock used as the monotonic one.
    #[test]
    fn now_is_monotonic() {
        let mut last = platform::now();
        for _ in 0..10_000 {
            let t = platform::now();
            assert!(t >= last);
            last = t;
        }
    }

    /// T4. Break: the monotonic clock used as the wall one.
    #[test]
    fn wall_is_the_wall_clock() {
        let a = std::time::SystemTime::now();
        let w = platform::wall();
        let b = std::time::SystemTime::now();
        assert!(w >= a && w <= b, "{w:?} between {a:?} and {b:?}");
    }
}

// ── 6. Limits ────────────────────────────────────────────────────────────────

mod limits {
    use super::*;
    use m6_platform::process::FD_MARGIN;

    /// The page size is a power of two of at least 4 KiB, the unit the
    /// bounded buffers are sized in.
    #[test]
    fn page_size_is_a_power_of_two_of_at_least_4_kib() {
        let page = process::page_size();
        assert!(page >= 4096, "{page}");
        assert!(page.is_power_of_two(), "{page}");
    }

    /// L1. Break: the hard limit returned on an OS that refuses it (macOS
    /// reports `RLIM_INFINITY` and enforces `kern.maxfilesperproc`).
    #[test]
    fn raise_fd_limit_returns_the_limit_it_set() {
        let n = process::raise_fd_limit().expect("raise");
        let (soft, hard) = process::fd_limits().expect("limits");
        assert_eq!(
            soft,
            (n + FD_MARGIN) as u64,
            "the soft limit is what was returned plus the margin"
        );
        assert!(soft <= hard);
        assert!(soft < u64::MAX / 2, "a real number, not unlimited: {soft}");
        assert_eq!(process::raise_fd_limit().expect("again"), n);
    }

    /// L2. Break: the clamp above the hard limit, or an error. The test
    /// re-runs its own binary under a lowered hard limit, where
    /// `a_lowered_hard_limit_is_respected_inside` (ignored here) measures.
    #[test]
    fn a_lowered_hard_limit_is_respected() {
        let exe = std::env::current_exe().expect("current_exe");
        let out = std::process::Command::new("bash")
            .args([
                "-c",
                "ulimit -n 64 && exec \"$0\" --exact --ignored --nocapture limits::a_lowered_hard_limit_is_respected_inside",
                exe.to_str().expect("utf-8 path"),
            ])
            .output()
            .expect("run the inner test under a lowered limit");
        let text = format!(
            "{}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(out.status.success(), "the inner test failed:\n{text}");
        assert!(text.contains("inner: ok"), "{text}");
    }

    /// The inner half of L2: only meaningful under `ulimit -n 64`.
    #[test]
    #[ignore = "run by a_lowered_hard_limit_is_respected under ulimit -n 64"]
    fn a_lowered_hard_limit_is_respected_inside() {
        let (_, hard) = process::fd_limits().expect("limits");
        assert_eq!(hard, 64, "the shell lowered the hard limit");
        let n = process::raise_fd_limit().expect("raise under a low hard limit");
        assert_eq!(n, 64 - FD_MARGIN);
        let open_already = process::open_descriptors(std::process::id()).expect("count");
        let mut held = Vec::new();
        loop {
            match std::fs::File::open("/dev/null") {
                Ok(f) => held.push(f),
                Err(e) => {
                    assert_eq!(e.raw_os_error(), Some(24), "EMFILE: {e}");
                    break;
                }
            }
            assert!(held.len() < 1000, "the limit is enforced");
        }
        assert_eq!(
            open_already + held.len(),
            64,
            "every descriptor up to the limit opened"
        );
        println!("inner: ok");
    }
}

// ── 7. Host and process facts ────────────────────────────────────────────────

mod facts {
    use super::*;
    use m6_platform::host;

    /// H1. Break: the empty stub (today's macOS).
    #[test]
    fn memory_is_real() {
        let m = host::memory().expect("memory");
        assert!(m.total_bytes > 0);
        assert!(
            m.available_bytes > 0 && m.available_bytes <= m.total_bytes,
            "{m:?}"
        );
    }

    /// H2. Break: empty.
    #[test]
    fn load_is_real() {
        let l = host::load_average().expect("load");
        assert!(l.one >= 0.0 && l.five >= 0.0 && l.fifteen >= 0.0, "{l:?}");
    }

    /// H3. Break: empty.
    #[test]
    fn uptime_is_real() {
        assert!(host::uptime().expect("uptime") > Duration::ZERO);
    }

    /// H4. Break: empty, or a row of zeros. The Linux reader skips the
    /// loopback row by design (it is always busy and never a fault), and
    /// macOS lists it, so the shared claim is about the interfaces reported:
    /// at least one, each with a name, and at least one that has carried
    /// traffic.
    #[test]
    fn an_interface_with_traffic_is_listed() {
        let devices = host::net_devices();
        assert!(!devices.is_empty(), "no interface listed");
        assert!(devices.iter().all(|d| !d.name.is_empty()), "{devices:?}");
        assert!(
            devices.iter().any(|d| d.rx_bytes > 0 || d.tx_bytes > 0),
            "no interface has carried traffic: {devices:?}"
        );
    }

    /// H5. Break: zero.
    #[test]
    fn disk_space_is_real() {
        let d = host::disk(std::path::Path::new("/")).expect("disk");
        assert!(
            d.total_bytes > 0 && d.available_bytes <= d.total_bytes,
            "{d:?}"
        );
        assert!(host::disk(std::path::Path::new("/no/such/path")).is_none());
    }

    /// H6. Break: a stub that fills zeros.
    #[test]
    fn absent_readings_are_absent_not_zero() {
        let s = host::snapshot(std::path::Path::new("/"));
        for d in &s.disks {
            assert!(!d.name.is_empty(), "{d:?}");
        }
        for z in &s.thermal {
            assert!(!z.name.is_empty(), "{z:?}");
        }
        for p in [&s.cpu_pressure, &s.io_pressure, &s.memory_pressure]
            .into_iter()
            .flatten()
        {
            assert!(p.some_avg10 >= 0.0 && p.some_avg10 <= 100.0, "{p:?}");
        }
        assert!(s.cpus >= 1);
        assert!(s.fds.as_ref().expect("fds").open > 0);
    }

    /// H10. Break: zero for a dead pid.
    #[test]
    fn a_reaped_pid_is_an_error() {
        let mut child = std::process::Command::new("true").spawn().expect("spawn");
        let pid = child.id();
        child.wait().expect("wait");
        assert!(process::thread_count(pid).is_err());
        assert!(process::open_descriptors(pid).is_err());
    }

    /// H11. Break: constant.
    #[test]
    fn is_alive_follows_the_process() {
        let mut child = std::process::Command::new("bash")
            .args(["-c", "read x"])
            .stdin(std::process::Stdio::piped())
            .spawn()
            .expect("spawn");
        let pid = child.id();
        assert!(process::is_alive(pid));
        drop(child.stdin.take());
        child.wait().expect("wait");
        assert!(!process::is_alive(pid));
    }

    /// H12. Break: constant.
    #[test]
    fn listens_on_tcp_sees_a_listener() {
        let me = std::process::id();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        assert!(process::listens_on_tcp(me).expect("answer"));
        drop(listener);
        // Other tests in this binary may hold a listener of their own, so the
        // negative half runs in a child that holds none.
        let out = std::process::Command::new("bash")
            .args(["-c", "echo $$; read x"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("spawn");
        let mut out = out;
        let mut line = String::new();
        std::io::BufRead::read_line(
            &mut std::io::BufReader::new(out.stdout.take().expect("stdout")),
            &mut line,
        )
        .expect("the child's pid");
        assert!(
            !process::listens_on_tcp(out.id()).expect("answer"),
            "{line}"
        );
        drop(out.stdin.take());
        out.wait().expect("wait");
    }

    /// H13. Break: hard-coded.
    #[test]
    fn is_root_agrees_with_the_shell() {
        let out = std::process::Command::new("id")
            .arg("-u")
            .output()
            .expect("id -u");
        let uid = String::from_utf8_lossy(&out.stdout).trim().to_string();
        assert_eq!(process::is_root(), uid == "0");
    }

    /// H14. The platform states the worker range a certificate-store read
    /// leaves behind, so a caller can count the threads a service makes
    /// instead of the platform's own.
    ///
    /// Break: a range whose low end is above its high end.
    #[test]
    fn the_certificate_store_worker_range_is_ordered() {
        let (low, high) = process::certificate_store_workers();
        assert!(low <= high, "{low}..={high}");
    }

    /// A host fact in its serialised form is exactly one of `{"known": value}`
    /// or `{"unknown": "reason"}`, and the reason is never empty.
    fn assert_fact(value: &serde_json::Value, what: &str) {
        let object = value
            .as_object()
            .unwrap_or_else(|| panic!("{what} is not a fact: {value}"));
        let known = object.get("known");
        let unknown = object.get("unknown");
        assert!(
            known.is_some() != unknown.is_some(),
            "{what} is not exactly one of a value and a reason: {value}"
        );
        if let Some(reason) = unknown {
            let reason = reason
                .as_str()
                .unwrap_or_else(|| panic!("{what}: the reason is not a string: {value}"));
            assert!(!reason.is_empty(), "{what} is unknown with no reason");
        }
    }

    /// Every key the start-up record expects is present, every leaf is a fact,
    /// and the address list may be empty. This is the shape a reader filters
    /// on, so a missing key is the defect it refuses.
    fn assert_complete_record(value: &serde_json::Value) {
        let object = value.as_object().expect("the record is an object");
        for key in [
            "hostname",
            "fqdn",
            "addresses",
            "os",
            "kernel",
            "cpu",
            "memory",
            "limits",
            "clock",
            "virtualization",
        ] {
            assert!(object.contains_key(key), "no {key} key: {value}");
        }
        assert_fact(&object["hostname"], "hostname");
        assert_fact(&object["fqdn"], "fqdn");
        assert_fact(&object["os"], "os");
        if let Some(os) = object["os"].get("known") {
            assert_fact(&os["version"], "os.version");
        }
        assert_fact(&object["kernel"], "kernel");
        assert_fact(&object["cpu"]["model"], "cpu.model");
        assert_fact(&object["cpu"]["count"], "cpu.count");
        assert_fact(&object["memory"]["total_bytes"], "memory.total_bytes");
        assert_fact(&object["memory"]["page_size"], "memory.page_size");
        assert_fact(&object["memory"]["huge_pages"], "memory.huge_pages");
        for key in [
            "memlock",
            "nofile",
            "somaxconn",
            "cgroup_memory",
            "cgroup_cpu",
        ] {
            assert_fact(&object["limits"][key], key);
        }
        assert_fact(&object["clock"]["timezone"], "clock.timezone");
        assert_fact(&object["clock"]["source"], "clock.source");
        assert_fact(&object["clock"]["boot_time"], "clock.boot_time");
        assert_fact(&object["virtualization"], "virtualization");
        let addresses = object["addresses"].as_array().expect("a list");
        for row in addresses {
            let name = row
                .get("name")
                .and_then(|name| name.as_str())
                .unwrap_or_else(|| panic!("an address with no name: {row}"));
            assert!(!name.is_empty(), "{row}");
            assert!(row["address"].is_string(), "{row}");
            assert!(row["scope"].is_string(), "{row}");
        }
    }

    /// H15. The facts this host gives serialise as a complete record, with
    /// every unknown carrying its reason. The address list may be empty: a
    /// host with no configured interface is an ordinary answer.
    ///
    /// The record is printed so an operator can read what this host answered,
    /// including the two cgroup limits: `{"known": null}` is unlimited and
    /// `{"unknown": "reason"}` is a file that could not be read. Run it with
    /// `--nocapture` in the process's own cgroup.
    ///
    /// Break: a reader that panics on a file the container does not have, or a
    /// serialisation that drops a key it could not fill.
    #[test]
    fn host_facts_answers_every_fact_and_serialises_every_key() {
        let facts = m6_platform::host_facts();
        let value = serde_json::to_value(&facts).expect("host facts serialise");
        println!(
            "H15 host_facts():\n{}",
            serde_json::to_string_pretty(&facts).expect("host facts serialise")
        );
        assert_complete_record(&value);
    }

    /// H16. A host that knows only its host name still serialises a complete
    /// record: every key present, almost all of them unknown with a reason.
    /// This is the container with half of `/proc` missing, where the record
    /// must stay countable and comparable between nodes.
    ///
    /// Break: an implementation that omits a key it could not fill, which
    /// turns a monitoring comparison into a missing-data puzzle.
    #[test]
    fn a_host_that_knows_only_its_hostname_is_still_a_complete_record() {
        use m6_platform::facts::{ClockFacts, CpuFacts, Fact, HostFacts, Limits, MemoryFacts};

        fn unknown<T>() -> Fact<T> {
            Fact::Unknown("the host said nothing")
        }
        let facts = HostFacts {
            hostname: Fact::Known("host".to_string()),
            fqdn: unknown(),
            addresses: Vec::new(),
            os: unknown(),
            kernel: unknown(),
            cpu: CpuFacts {
                model: unknown(),
                count: unknown(),
            },
            memory: MemoryFacts {
                total_bytes: unknown(),
                page_size: unknown(),
                huge_pages: unknown(),
            },
            limits: Limits {
                memlock: unknown(),
                nofile: unknown(),
                somaxconn: unknown(),
                cgroup_memory: unknown(),
                cgroup_cpu: unknown(),
            },
            clock: ClockFacts {
                timezone: unknown(),
                source: unknown(),
                boot_time: unknown(),
            },
            virtualization: unknown(),
        };
        let value = serde_json::to_value(&facts).expect("host facts serialise");
        assert_complete_record(&value);
        assert_eq!(value["hostname"]["known"], "host");
        assert_eq!(
            value["kernel"]["unknown"], "the host said nothing",
            "an unknown fact keeps the reason it was given"
        );
    }

    /// H17. One reading costs microseconds, not milliseconds. The number is
    /// printed for the record and the bound catches only the two things that
    /// would make it a start-up cost: a network read, such as a resolver
    /// lookup for the FQDN, and a reader that walks something large.
    ///
    /// Break: a loop over `/proc/net/dev` per interface, or a resolver lookup
    /// for a host name with no domain.
    #[test]
    fn host_facts_is_read_once_and_costs_microseconds() {
        let start = std::time::Instant::now();
        let facts = m6_platform::host_facts();
        let elapsed = start.elapsed();
        println!("H17 host_facts() took {elapsed:?}");
        std::hint::black_box(&facts);
        assert!(
            elapsed < std::time::Duration::from_secs(1),
            "host_facts() took {elapsed:?}, which is a start-up cost, not a reading"
        );
    }
}

// ── 8. Directories and private files ─────────────────────────────────────────

mod directories {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// D1. Break: a socket root that is the long `TMPDIR` itself, so the
    /// path depends on the environment rather than on the platform.
    ///
    /// `temp_dir()` must still follow `TMPDIR` (D2), but it is not where a
    /// socket goes: the root for that is [`platform::socket_scratch_root`],
    /// which is short whatever `TMPDIR` is.
    #[test]
    fn the_socket_scratch_root_takes_a_socket() {
        let dir = platform::socket_scratch_root();
        assert!(dir.is_dir());
        let sub = tempfile::tempdir_in(&dir).expect("writable");
        let sock = sub.path().join("m6.sock");
        platform::check_socket_path(&sock).expect("a socket under the socket root fits");
        drop(net::bind_unix(&sock, 0o600).expect("binds"));
    }

    /// D2. Break: `TMPDIR` ignored.
    #[test]
    fn tmpdir_is_respected() {
        let exe = std::env::current_exe().expect("current_exe");
        let dir = tempfile::tempdir_in(platform::socket_scratch_root()).expect("tempdir");
        let out = std::process::Command::new(exe)
            .args([
                "--exact",
                "--ignored",
                "--nocapture",
                "directories::tmpdir_is_respected_inside",
            ])
            .env("TMPDIR", dir.path())
            .output()
            .expect("run the inner test");
        let text = String::from_utf8_lossy(&out.stdout).into_owned();
        assert!(out.status.success(), "{text}");
        assert!(
            text.contains(&format!("inner: {}", dir.path().display())),
            "{text}"
        );
    }

    #[test]
    #[ignore = "run by tmpdir_is_respected with TMPDIR set"]
    fn tmpdir_is_respected_inside() {
        println!("inner: {}", platform::temp_dir().display());
    }

    /// D3. Break: a `noexec` mount chosen.
    #[test]
    fn the_scratch_directory_permits_exec() {
        let dir = tempfile::tempdir_in(platform::scratch_dir()).expect("tempdir");
        let script = dir.path().join("run.sh");
        std::fs::write(&script, "#!/bin/bash\necho ran\n").expect("write");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        // A thread that forks while this one writes holds the script's write
        // descriptor until its own `exec`, and Linux refuses to run the file
        // until then (`ETXTBSY`). That is a wait for the other `exec`, not a
        // `noexec` mount, so retry only that refusal.
        let deadline = Instant::now() + Duration::from_secs(10);
        let out = loop {
            match std::process::Command::new(&script).output() {
                Err(e)
                    if e.kind() == io::ErrorKind::ExecutableFileBusy
                        && Instant::now() < deadline =>
                {
                    std::thread::yield_now();
                }
                done => break done.expect("exec from the scratch directory"),
            }
        };
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "ran");
    }

    /// D3a. Break: a fixed system path preferred to the temp directory the
    /// process was given, so a sandbox that denies `/var/tmp` fails every
    /// compiled asset with `PermissionDenied`.
    #[test]
    fn the_scratch_directory_is_the_one_tmpdir_names() {
        let exe = std::env::current_exe().expect("current_exe");
        // The directory has to be one a program can be *run* from, because
        // that is what the probe checks: it compiles a script and executes it.
        // `/var/tmp` is where that works on this platform, and a TMPDIR under a
        // `noexec` mount is correctly refused by the probe, which would make
        // this test assert the opposite of the contract.
        let dir = match tempfile::tempdir_in("/var/tmp") {
            Ok(dir) => dir,
            Err(e) => {
                // A host with no writable /var/tmp cannot run this contract at
                // all. Say so by name rather than pass or fail on it.
                println!("D3a needs a writable /var/tmp to prove TMPDIR wins: {e}");
                return;
            }
        };
        let out = std::process::Command::new(exe)
            .args([
                "--exact",
                "--ignored",
                "--nocapture",
                "directories::scratch_dir_is_respected_inside",
            ])
            .env("TMPDIR", dir.path())
            .output()
            .expect("run the inner test");
        let text = String::from_utf8_lossy(&out.stdout).into_owned();
        assert!(out.status.success(), "{text}");
        assert!(
            text.contains(&format!("inner: {}", dir.path().display())),
            "{text}"
        );
    }

    #[test]
    #[ignore = "run by the_scratch_directory_is_the_one_tmpdir_names with TMPDIR set"]
    fn scratch_dir_is_respected_inside() {
        println!("inner: {}", platform::scratch_dir().display());
    }

    /// D4. Break: a path that drifts between services.
    #[test]
    fn the_runtime_directory_is_fixed() {
        let dir = platform::runtime_dir();
        assert_eq!(dir, std::path::Path::new("/run/m6"));
        platform::check_socket_path(&dir.join("m6-html.sock")).expect("fits");
    }

    /// D5. Break: a mode set only under a unix conditional, or the parent changed.
    #[test]
    fn create_private_sets_and_tightens_modes() {
        fn mode(p: &std::path::Path) -> u32 {
            std::fs::metadata(p).unwrap().permissions().mode() & 0o777
        }
        let dir = tempfile::tempdir_in(platform::socket_scratch_root()).expect("tempdir");
        let file = dir.path().join("private").join("h.sqlite3");
        platform::create_private(&file).expect("create");
        assert_eq!(mode(file.parent().unwrap()), 0o700);
        assert_eq!(mode(&file), 0o600);
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
        std::fs::set_permissions(
            file.parent().unwrap(),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        platform::create_private(&file).expect("again");
        assert_eq!(
            mode(&file),
            0o600,
            "a file left by an earlier start is tightened"
        );
        assert_eq!(
            mode(file.parent().unwrap()),
            0o755,
            "an existing parent is left alone"
        );
    }
}
