//! The platform layer's behavioural contract, one shared suite.
//!
//! Every test here is written once against `m6_platform`'s public API
//! and runs unchanged on macOS arm64, Linux x86_64 and Linux arm64. No `cfg`,
//! no per-OS branch, no OS-specific helper. Each test's comment names the
//! wrong implementation that must fail it, and `OS-1-TESTS.md` is the list.
//!
//! Section 2 (signals) is `platform_signals.rs`, which needs its own `main`.
//! Section 9 (the gate) is `platform_gate.rs`. Section 10 is the echo
//! reference app's suite in `echo_example.rs`.

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
fn ready(poll: &mut Poll, events: &mut Events, wait: Duration) -> Vec<(Token, bool, bool)> {
    poll.poll(events, Some(wait)).expect("poll");
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

    /// P6. The EL-5b shape. Break: `reregister` short-circuited when the kernel
    /// was already told, which is the bug kqueue hid on macOS.
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
    use std::process::{Command, Stdio};

    /// C9. Break: `close_inherited_on_exec` not run (or run after the lock
    /// was taken). A lock this process holds would then be held by the child
    /// too, and the second `try_lock` below would fail.
    #[test]
    fn a_spawned_child_inherits_no_descriptor_above_stderr() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("lock");
        let held = std::fs::File::create(&path).expect("create");
        held.try_lock().expect("this process takes the lock");
        // The harness's start-time sweep, here after the lock so it is the
        // sweep and not Rust's own close-on-exec that the test holds.
        process::close_inherited_on_exec().expect("sweep");
        let mut child = process::spawn(
            Command::new("sleep")
                .arg("30")
                .stdin(Stdio::null())
                .stdout(Stdio::null()),
        )
        .expect("spawn");
        drop(held);
        let again = std::fs::File::open(&path).expect("open");
        let free = again.try_lock().is_ok();
        child.kill().expect("kill");
        child.wait().expect("wait");
        assert!(
            free,
            "the child held the lock it should never have inherited"
        );
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
        let mut server = process::spawn(Command::new("sleep").arg("30").stdin(Stdio::null()))
            .expect("start the server");
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
        let dir = tempfile::tempdir_in(platform::temp_dir()).expect("tempdir");
        let path = path_of_len(dir.path(), MAX_SOCKET_PATH - 1);
        let listener = net::bind_unix(&path, 0o600).expect("bind at the limit");
        let bound = listener.local_addr().expect("addr");
        assert_eq!(bound.as_pathname(), Some(path.as_path()));
    }

    /// U2. Break: the OS's own text, or truncation.
    #[test]
    fn a_path_over_the_limit_names_the_path_and_the_limit() {
        let dir = tempfile::tempdir_in(platform::temp_dir()).expect("tempdir");
        let path = path_of_len(dir.path(), MAX_SOCKET_PATH);
        let err = net::bind_unix(&path, 0o600).expect_err("one byte too long");
        let msg = err.to_string();
        assert!(msg.contains(&path.display().to_string()), "{msg}");
        assert!(msg.contains(&(MAX_SOCKET_PATH - 1).to_string()), "{msg}");
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
        let dir = tempfile::tempdir_in(platform::temp_dir()).expect("tempdir");
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
        let dir = tempfile::tempdir_in(platform::temp_dir()).expect("tempdir");
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

    /// U8. Break: a stale file refused.
    #[test]
    fn a_stale_socket_file_is_replaced_at_bind() {
        let dir = tempfile::tempdir_in(platform::temp_dir()).expect("tempdir");
        let path = dir.path().join("nested").join("m6.sock");
        drop(net::bind_unix(&path, 0o660).expect("first bind creates the parent"));
        assert!(path.exists(), "the socket file is left behind");
        drop(net::bind_unix(&path, 0o660).expect("a stale socket is replaced"));
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o660);
    }

    /// U10. Break: `bind_unix` unlinking whatever is at the path, so a second
    /// instance steals a live listener's socket.
    #[test]
    fn a_live_listeners_socket_is_refused_not_replaced() {
        let dir = tempfile::tempdir_in(platform::temp_dir()).expect("tempdir");
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
        let dir = tempfile::tempdir_in(platform::temp_dir()).expect("tempdir");
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
}

// ── 8. Directories and private files ─────────────────────────────────────────

mod directories {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// D1. Break: a long default path.
    #[test]
    fn the_temp_directory_takes_a_socket() {
        let dir = platform::temp_dir();
        assert!(dir.is_dir());
        let sub = tempfile::tempdir_in(&dir).expect("writable");
        let sock = sub.path().join("m6.sock");
        platform::check_socket_path(&sock).expect("a socket under the temp directory fits");
        drop(net::bind_unix(&sock, 0o600).expect("binds"));
    }

    /// D2. Break: `TMPDIR` ignored.
    #[test]
    fn tmpdir_is_respected() {
        let exe = std::env::current_exe().expect("current_exe");
        let dir = tempfile::tempdir().expect("tempdir");
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
        let out = std::process::Command::new(&script)
            .output()
            .expect("exec from the scratch directory");
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "ran");
    }

    /// D3a. Break: a fixed system path preferred to the temp directory the
    /// process was given, so a sandbox that denies `/var/tmp` fails every
    /// compiled asset with `PermissionDenied`.
    #[test]
    fn the_scratch_directory_is_the_one_tmpdir_names() {
        let exe = std::env::current_exe().expect("current_exe");
        let dir = tempfile::tempdir().expect("tempdir");
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
        let dir = tempfile::tempdir().expect("tempdir");
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
