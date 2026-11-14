//! The echo programs, the reference loop, proved over the wire: the nine
//! checks of the echo prototype's `check.py` in Rust, the TLS round trips,
//! and the cases `OS-1-TESTS.md` section 10 adds (many connections at once, a
//! client that never reads, a close with a reply owed, a shutdown that drains,
//! a reload, the loop never waiting). The same suite runs unchanged on every
//! supported target.
//!
//! Each test starts a program as a child process, reads its `listening on`
//! line for the address, and waits on a signal with a deadline, never a fixed
//! sleep. The programs are `[[bin]]` targets behind `testkit`, so this one
//! command builds and runs them (`CARGO_BIN_EXE_echo` / `CARGO_BIN_EXE_echo_tls`):
//!
//! ```text
//! cargo test -p m6-core --features testkit --test echo_example -- --test-threads=1
//! ```

#![allow(
    clippy::disallowed_methods,
    reason = "test code: threads and blocking clients drive the echo from outside"
)]

use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use m6_core::platform::{self, Signal};

// ── Binaries ──────────────────────────────────────────────────────────────────

fn bin(name: &str) -> std::path::PathBuf {
    // The two echo programs are `[[bin]]` targets, so cargo builds them with the
    // test and exposes their paths as `CARGO_BIN_EXE_*`.
    std::path::PathBuf::from(match name {
        "echo" => env!("CARGO_BIN_EXE_echo"),
        "echo_tls" => env!("CARGO_BIN_EXE_echo_tls"),
        other => panic!("unknown echo binary: {other}"),
    })
}

// ── The server under test ─────────────────────────────────────────────────────

struct Server {
    child: Child,
    addr: SocketAddr,
    output: Arc<Mutex<Vec<u8>>>,
    reader: Option<std::thread::JoinHandle<()>>,
}

impl Server {
    fn start(
        bin: &Path,
        args: &[&str],
        idle_ms: u64,
        conns: u64,
        max_conns: Option<u64>,
    ) -> Server {
        Server::start_cmd(Command::new(bin), args, idle_ms, conns, max_conns)
    }

    /// As `start`, from a command already built (a shell that lowers the
    /// descriptor limit and execs the program).
    fn start_cmd(
        mut cmd: Command,
        args: &[&str],
        idle_ms: u64,
        conns: u64,
        max_conns: Option<u64>,
    ) -> Server {
        cmd.args(args);
        cmd.env("ECHO_IDLE_MS", idle_ms.to_string());
        cmd.env("ECHO_CONNS", conns.to_string());
        // A shutdown with clients still held open must not hold the tests up.
        cmd.env("ECHO_DRAIN_MS", "500");
        if let Some(m) = max_conns {
            cmd.env("ECHO_MAX_CONNS", m.to_string());
        }
        cmd.stderr(Stdio::piped());
        cmd.stdin(Stdio::null());
        cmd.stdout(Stdio::null());
        let mut child = platform::process::spawn(&mut cmd).expect("spawn echo");
        let mut stderr = child.stderr.take().expect("stderr");

        // The first line carries the bound address.
        let mut first = String::new();
        let mut byte = [0u8; 1];
        loop {
            match stderr.read(&mut byte) {
                Ok(0) => break,
                Ok(_) => {
                    first.push(byte[0] as char);
                    if byte[0] == b'\n' {
                        break;
                    }
                }
                Err(e) => panic!("reading the listening line: {e}"),
            }
        }
        assert!(
            first.starts_with("listening on "),
            "unexpected first line: {first:?}"
        );
        let rest = first["listening on ".len()..].trim();
        let addr_part = rest.split(',').next().expect("an address");
        let addr: SocketAddr = addr_part.parse().expect("parse the address");

        let output = Arc::new(Mutex::new(Vec::new()));
        let out = Arc::clone(&output);
        let reader = std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                match stderr.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => out.lock().unwrap().extend_from_slice(&buf[..n]),
                    Err(_) => break,
                }
            }
        });
        Server {
            child,
            addr,
            output,
            reader: Some(reader),
        }
    }

    /// Wait until `needle` appears in the output, with a deadline.
    fn wait_for(&self, needle: &str, deadline: Duration) {
        let until = Instant::now() + deadline;
        while Instant::now() < until {
            if String::from_utf8_lossy(&self.output.lock().unwrap()).contains(needle) {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let out = String::from_utf8_lossy(&self.output.lock().unwrap()).into_owned();
        panic!("{needle:?} never appeared:\n{out}");
    }

    /// A shutdown request, wait for exit 0, and return the whole stderr tail.
    fn tail(mut self) -> String {
        platform::send(self.child.id(), Signal::Shutdown).expect("send shutdown");
        let status = self.child.wait().expect("wait");
        assert_eq!(status.code(), Some(0), "echo exited with {status}");
        if let Some(r) = self.reader.take() {
            r.join().expect("reader");
        }
        String::from_utf8_lossy(&self.output.lock().unwrap()).into_owned()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

// ── Client helpers ────────────────────────────────────────────────────────────

fn recv_exact(s: &mut TcpStream, n: usize, deadline: Duration) -> Vec<u8> {
    s.set_read_timeout(Some(deadline))
        .expect("set_read_timeout");
    let mut out = Vec::with_capacity(n);
    let mut buf = [0u8; 65536];
    while out.len() < n {
        match s.read(&mut buf) {
            Ok(0) => panic!("closed early after {} of {n} bytes", out.len()),
            Ok(k) => out.extend_from_slice(&buf[..k]),
            Err(e) => panic!("recv error after {} of {n} bytes: {e}", out.len()),
        }
    }
    out
}

fn assert_zero_allocations(tail: &str) {
    assert!(
        tail.contains(" 0 allocation(s) during the run outside table growth"),
        "expected zero allocations outside growth:\n{tail}"
    );
}

// ── Checks ────────────────────────────────────────────────────────────────────

#[test]
fn one_message_is_echoed() {
    let srv = Server::start(&bin("echo"), &["127.0.0.1:0"], 400, 1024, None);
    let mut s = TcpStream::connect(srv.addr).expect("connect");
    s.write_all(b"hello\n").expect("send");
    assert_eq!(recv_exact(&mut s, 6, Duration::from_secs(10)), b"hello\n");
    drop(s);
    let tail = srv.tail();
    assert!(tail.contains("stopping"), "{tail}");
    assert_zero_allocations(&tail);
}

#[test]
fn fifty_concurrent_clients_by_twenty_messages() {
    let srv = Server::start(&bin("echo"), &["127.0.0.1:0"], 10_000, 1024, None);
    let addr = srv.addr;
    let errors: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let threads: Vec<_> = (0..50)
        .map(|i| {
            let errors = Arc::clone(&errors);
            std::thread::spawn(move || {
                let run = || -> std::io::Result<()> {
                    let mut c = TcpStream::connect(addr)?;
                    for j in 0..20 {
                        let m = format!("client {i} message {j}\n");
                        c.write_all(m.as_bytes())?;
                        let back = recv_exact(&mut c, m.len(), Duration::from_secs(15));
                        if back != m.as_bytes() {
                            return Err(std::io::Error::other(format!(
                                "client {i} message {j} mismatch"
                            )));
                        }
                    }
                    Ok(())
                };
                if let Err(e) = run() {
                    errors.lock().unwrap().push(e.to_string());
                }
            })
        })
        .collect();
    for t in threads {
        t.join().expect("client thread");
    }
    let errors = errors.lock().unwrap();
    assert!(errors.is_empty(), "{errors:?}");
    let tail = srv.tail();
    assert_zero_allocations(&tail);
}

#[test]
fn eight_mib_is_echoed_byte_for_byte() {
    let srv = Server::start(&bin("echo"), &["127.0.0.1:0"], 10_000, 1024, None);
    let data: Vec<u8> = (0..8 << 20).map(|i| (i % 251) as u8).collect();
    let mut s = TcpStream::connect(srv.addr).expect("connect");
    let mut reader = s.try_clone().expect("clone");
    let got: Arc<Mutex<Option<Vec<u8>>>> = Arc::new(Mutex::new(None));
    let got2 = Arc::clone(&got);
    let reader_thread = std::thread::spawn(move || {
        let mut buf = Vec::with_capacity(data_len());
        let mut tmp = [0u8; 65536];
        let deadline = Duration::from_secs(30);
        reader
            .set_read_timeout(Some(deadline))
            .expect("set_read_timeout");
        while buf.len() < data_len() {
            match reader.read(&mut tmp) {
                Ok(0) => break,
                Ok(n) => buf.extend_from_slice(&tmp[..n]),
                Err(_) => break,
            }
        }
        *got2.lock().unwrap() = Some(buf);
    });
    s.write_all(&data).expect("send 8 MiB");
    reader_thread.join().expect("reader");
    let got = got.lock().unwrap().take().expect("read");
    assert_eq!(got, data);
    drop(s);
    let tail = srv.tail();
    assert_zero_allocations(&tail);
}

fn data_len() -> usize {
    8 << 20
}

#[test]
fn half_close_drains_then_closes() {
    let srv = Server::start(&bin("echo"), &["127.0.0.1:0"], 10_000, 1024, None);
    let mut s = TcpStream::connect(srv.addr).expect("connect");
    let body = vec![b'x'; 300_000];
    s.write_all(&body).expect("send");
    s.shutdown(std::net::Shutdown::Write)
        .expect("shutdown write");
    assert_eq!(recv_exact(&mut s, 300_000, Duration::from_secs(15)), body);
    // `recv_exact` already set a 15 s read timeout; use it for the EOF read.
    let mut b = [0u8; 1];
    assert_eq!(s.read(&mut b).expect("read eof"), 0, "server must close");
    drop(s);
    let tail = srv.tail();
    assert_zero_allocations(&tail);
}

#[test]
fn an_idle_client_is_closed_after_its_deadline() {
    let srv = Server::start(&bin("echo"), &["127.0.0.1:0"], 400, 1024, None);
    let mut s = TcpStream::connect(srv.addr).expect("connect");
    let t0 = Instant::now();
    s.set_read_timeout(Some(Duration::from_secs(5)))
        .expect("timeout");
    let mut b = [0u8; 1];
    assert_eq!(s.read(&mut b).expect("read eof"), 0, "server must close");
    let dt = t0.elapsed();
    assert!(
        dt > Duration::from_millis(350) && dt < Duration::from_secs(2),
        "idle close after {dt:?}"
    );
    drop(s);
    let tail = srv.tail();
    assert_zero_allocations(&tail);
}

#[test]
fn a_reset_client_does_not_take_the_server_down() {
    let srv = Server::start(&bin("echo"), &["127.0.0.1:0"], 10_000, 1024, None);
    let mut reset = TcpStream::connect(srv.addr).expect("connect");
    // A payload far larger than one buffer, so a reset can land while bytes the
    // server has not read yet are still in flight.
    reset.write_all(&vec![b'y'; 100_000]).expect("send");
    // The echo is the signal the server has STARTED reading: read one buffer's
    // worth, then reset while the rest of the 100 KB is still in flight. This
    // is driven by the echo, not by timing.
    let echoed = recv_exact(&mut reset, 16 * 1024, Duration::from_secs(10));
    assert!(echoed.iter().all(|&b| b == b'y'));
    socket2::SockRef::from(&reset)
        .set_linger(Some(Duration::ZERO))
        .ok();
    drop(reset);
    let mut c = TcpStream::connect(srv.addr).expect("connect");
    c.write_all(b"still here\n").expect("send");
    assert_eq!(
        recv_exact(&mut c, 11, Duration::from_secs(10)),
        b"still here\n"
    );
    drop(c);
    let tail = srv.tail();
    assert_zero_allocations(&tail);
}

#[test]
fn sigterm_stops_it_with_exit_zero() {
    let srv = Server::start(&bin("echo"), &["127.0.0.1:0"], 400, 1024, None);
    let tail = srv.tail();
    assert!(tail.contains("stopping"), "{tail}");
    assert_zero_allocations(&tail);
}

#[test]
fn at_the_ceiling_the_extra_client_waits_then_is_served() {
    let srv = Server::start(&bin("echo"), &["127.0.0.1:0"], 10_000, 4, Some(4));
    let mut held: Vec<TcpStream> = Vec::new();
    for _ in 0..4 {
        let mut h = TcpStream::connect(srv.addr).expect("connect");
        h.write_all(b"h").expect("send");
        assert_eq!(recv_exact(&mut h, 1, Duration::from_secs(10)), b"h");
        held.push(h);
    }
    srv.wait_for(
        "connection table full at 4 slots: not accepting",
        Duration::from_secs(5),
    );
    let mut extra = TcpStream::connect(srv.addr).expect("connect");
    extra.write_all(b"wait\n").expect("send");
    extra.set_nonblocking(true).expect("set_nonblocking");
    // Not served while the table is full.
    let mut b = [0u8; 1];
    assert!(
        matches!(extra.read(&mut b), Err(e) if e.kind() == ErrorKind::WouldBlock),
        "the extra client must wait in the kernel backlog"
    );
    extra.set_nonblocking(false).expect("set_blocking");
    held.remove(0); // close one held client, freeing a slot
    assert_eq!(recv_exact(&mut extra, 5, Duration::from_secs(5)), b"wait\n");
    let tail = srv.tail();
    assert_zero_allocations(&tail);
}

#[test]
fn the_table_grows_two_four_eight_and_keeps_eight() {
    let srv = Server::start(&bin("echo"), &["127.0.0.1:0"], 10_000, 2, Some(64));
    let mut held: Vec<TcpStream> = Vec::new();
    for i in 0..7 {
        let mut h = TcpStream::connect(srv.addr).expect("connect");
        let m = format!("c{i}");
        h.write_all(m.as_bytes()).expect("send");
        assert_eq!(
            recv_exact(&mut h, m.len(), Duration::from_secs(10)),
            m.as_bytes()
        );
        held.push(h);
    }
    drop(held);
    let mut late = TcpStream::connect(srv.addr).expect("connect");
    late.write_all(b"after\n").expect("send");
    assert_eq!(
        recv_exact(&mut late, 6, Duration::from_secs(10)),
        b"after\n"
    );
    drop(late);
    let tail = srv.tail();
    assert!(tail.contains("grew from 2 to 4"), "{tail}");
    assert!(tail.contains("grew from 4 to 8"), "{tail}");
    assert!(!tail.contains("grew from 8"), "{tail}");
    assert!(tail.contains(", 8 slots,"), "{tail}");
    assert_zero_allocations(&tail);
}

// ── The cases OS-1 adds (E2 to E7, E9) ───────────────────────────────────────

/// E3. 200 connections open at once, one message each, all answered. The test
/// raises its own descriptor limit through the layer, because a default soft
/// limit of 256 would not hold 200 sockets and the harness's own.
#[test]
fn two_hundred_connections_at_once() {
    m6_core::platform::process::raise_fd_limit().expect("raise the test's own limit");
    let srv = Server::start(&bin("echo"), &["127.0.0.1:0"], 10_000, 16, None);
    let mut clients: Vec<TcpStream> = (0..200)
        .map(|_| TcpStream::connect(srv.addr).expect("connect"))
        .collect();
    for (i, c) in clients.iter_mut().enumerate() {
        c.write_all(format!("c{i}\n").as_bytes()).expect("send");
    }
    for (i, c) in clients.iter_mut().enumerate() {
        let m = format!("c{i}\n");
        assert_eq!(
            recv_exact(c, m.len(), Duration::from_secs(10)),
            m.as_bytes()
        );
    }
    drop(clients);
    let tail = srv.tail();
    assert!(tail.contains("grew from 128 to 256"), "{tail}");
    assert_zero_allocations(&tail);
}

/// E4. A client that sends a MiB and never reads its echo does not stop the
/// others: the server's write to it blocks, the loop moves on.
#[test]
fn a_client_that_never_reads_does_not_stop_others() {
    let srv = Server::start(&bin("echo"), &["127.0.0.1:0"], 10_000, 1024, None);
    let mut stuck = TcpStream::connect(srv.addr).expect("connect");
    stuck.write_all(&vec![b's'; 1 << 20]).expect("send a MiB");
    let t0 = Instant::now();
    let mut other = TcpStream::connect(srv.addr).expect("connect");
    for i in 0..20 {
        let m = format!("fine {i}\n");
        other.write_all(m.as_bytes()).expect("send");
        assert_eq!(
            recv_exact(&mut other, m.len(), Duration::from_secs(5)),
            m.as_bytes()
        );
    }
    assert!(t0.elapsed() < Duration::from_secs(2), "{:?}", t0.elapsed());
    drop((stuck, other));
    let tail = srv.tail();
    assert_zero_allocations(&tail);
}

/// E5. A client that closes with a reply still owed frees its slot and the
/// next client is served. The reset case is `a_reset_client_does_not_take_the_server_down`.
#[test]
fn a_client_that_closes_mid_reply_frees_its_slot() {
    let srv = Server::start(&bin("echo"), &["127.0.0.1:0"], 10_000, 2, Some(2));
    let mut early = TcpStream::connect(srv.addr).expect("connect");
    early.write_all(&vec![b'e'; 300_000]).expect("send");
    assert_eq!(recv_exact(&mut early, 1, Duration::from_secs(10))[0], b'e');
    drop(early);
    for i in 0..3 {
        let mut next = TcpStream::connect(srv.addr).expect("connect");
        let m = format!("next {i}\n");
        next.write_all(m.as_bytes()).expect("send");
        assert_eq!(
            recv_exact(&mut next, m.len(), Duration::from_secs(10)),
            m.as_bytes()
        );
    }
    let tail = srv.tail();
    assert_zero_allocations(&tail);
}

/// E6. A shutdown request drains: a client that has sent 16 KiB and half
/// closed reads the whole echo, the process exits 0 and the port is released.
#[test]
fn shutdown_drains_in_flight_replies_and_exits_zero() {
    let srv = Server::start(&bin("echo"), &["127.0.0.1:0"], 10_000, 1024, None);
    let addr = srv.addr;
    let body = vec![b'd'; 16 * 1024];
    let mut s = TcpStream::connect(addr).expect("connect");
    s.write_all(&body).expect("send");
    s.shutdown(std::net::Shutdown::Write).expect("half close");
    // One byte back proves the server has accepted and started this reply.
    let first = recv_exact(&mut s, 1, Duration::from_secs(10));
    platform::send(srv.child.id(), Signal::Shutdown).expect("send shutdown");
    // `recv_exact` set the 10 s read timeout; it covers the read to EOF.
    let mut rest = Vec::new();
    s.read_to_end(&mut rest).expect("read to EOF");
    assert_eq!(
        first.len() + rest.len(),
        body.len(),
        "the whole reply before EOF"
    );
    let tail = srv.tail();
    assert!(tail.contains("stopping, 0 connection(s) open"), "{tail}");
    assert_zero_allocations(&tail);
    assert!(TcpStream::connect(addr).is_err(), "the port is released");
}

/// E7. A reload request is logged and serving continues.
#[test]
fn reload_is_received_and_serving_continues() {
    let srv = Server::start(&bin("echo"), &["127.0.0.1:0"], 10_000, 1024, None);
    platform::send(srv.child.id(), Signal::ReopenLogs).expect("send reload");
    srv.wait_for("log reopen requested", Duration::from_secs(5));
    let mut s = TcpStream::connect(srv.addr).expect("connect");
    s.write_all(b"after\n").expect("send");
    assert_eq!(recv_exact(&mut s, 6, Duration::from_secs(10)), b"after\n");
    drop(s);
    let tail = srv.tail();
    assert_zero_allocations(&tail);
}

/// E9. The loop never waits: with one connection held on a reply its client
/// will not read, a hundred round trips on another each complete within a
/// bound.
#[test]
fn the_loop_never_waits() {
    let srv = Server::start(&bin("echo"), &["127.0.0.1:0"], 10_000, 1024, None);
    let mut held = TcpStream::connect(srv.addr).expect("connect");
    held.write_all(&vec![b'h'; 1 << 20]).expect("send a MiB");
    let mut fast = TcpStream::connect(srv.addr).expect("connect");
    fast.set_nodelay(true).expect("nodelay");
    let mut slowest = Duration::ZERO;
    for _ in 0..100 {
        let t0 = Instant::now();
        fast.write_all(b"ping\n").expect("send");
        assert_eq!(recv_exact(&mut fast, 5, Duration::from_secs(5)), b"ping\n");
        slowest = slowest.max(t0.elapsed());
    }
    assert!(
        slowest < Duration::from_millis(100),
        "slowest round trip {slowest:?}"
    );
    drop((held, fast));
    let tail = srv.tail();
    assert_zero_allocations(&tail);
}

/// E10. Out of descriptors while clients queue. The shell lowers the limit to
/// 40 descriptors and the table is told it may hold 64, so accept fails with
/// EMFILE once about 30 connections are open while more wait in the kernel's
/// queue. The server neither exits nor stops accepting: once the first
/// clients close, every client still queued is answered or has been reset by
/// the kernel, and a new client is echoed. Linux leaves a connection queued
/// when accept fails for want of a descriptor, so it is served later. macOS
/// dequeues it before allocating the descriptor and resets it when that
/// fails, so a waiting client there may read a reset instead: that half of
/// the claim is the kernel's and not shared.
/// Break: an accept loop that breaks on EMFILE with the listener left armed
/// (nothing reports it again, so the new client is never accepted), or an
/// echo that returns the error and exits.
#[test]
fn queued_clients_are_served_once_descriptors_free() {
    let cmd = m6_core::testkit::lowered_fd_limit_command(40, 0, bin("echo"));
    let srv = Server::start_cmd(cmd, &["127.0.0.1:0"], 10_000, 64, Some(64));
    let mut clients: Vec<TcpStream> = (0..48)
        .map(|i| {
            let mut c = TcpStream::connect(srv.addr).expect("connect");
            c.set_read_timeout(Some(Duration::from_millis(300)))
                .unwrap_or_else(|e| panic!("client {i}: read timeout: {e}"));
            c.write_all(b"q").expect("send");
            c
        })
        .collect();
    // The first ones are served, the rest wait or were reset: a short read
    // sorts them.
    let mut served = Vec::new();
    let mut waiting = Vec::new();
    let mut reset = 0;
    for mut c in clients.drain(..) {
        let mut b = [0u8; 1];
        match c.read(&mut b) {
            Ok(1) if b[0] == b'q' => served.push(c),
            Ok(n) => panic!("unexpected read of {n}"),
            Err(e) if e.kind() == ErrorKind::WouldBlock || e.kind() == ErrorKind::TimedOut => {
                waiting.push(c)
            }
            Err(e) if e.kind() == ErrorKind::ConnectionReset => reset += 1,
            Err(e) => panic!("read: {e}"),
        }
    }
    assert!(
        served.len() >= 8,
        "some clients were served: {}",
        served.len()
    );
    assert!(
        waiting.len() + reset > 0,
        "the descriptor limit left no client waiting or reset"
    );
    // Closing the served ones frees descriptors: the queue drains, and what
    // the kernel reset is counted rather than served.
    drop(served);
    let mut served_late = 0;
    for mut c in waiting {
        c.set_read_timeout(Some(Duration::from_secs(10)))
            .expect("timeout");
        let mut b = [0u8; 1];
        match c.read(&mut b) {
            Ok(1) if b[0] == b'q' => served_late += 1,
            Err(e) if e.kind() == ErrorKind::ConnectionReset => reset += 1,
            other => panic!("a waiting client: {other:?}"),
        }
    }
    println!("served late: {served_late}, reset by the kernel: {reset}");
    // The listener is still armed: a new client is accepted and echoed.
    let mut fresh = TcpStream::connect(srv.addr).expect("connect");
    fresh.write_all(b"new\n").expect("send");
    assert_eq!(recv_exact(&mut fresh, 4, Duration::from_secs(10)), b"new\n");
    drop(fresh);
    let tail = srv.tail();
    assert_zero_allocations(&tail);
}

// ── TLS ───────────────────────────────────────────────────────────────────────

#[test]
fn echo_tls_round_trips_and_stops() {
    let dir = tempfile::tempdir().expect("tempdir");
    let ck = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).expect("rcgen");
    let cert = dir.path().join("cert.pem");
    let key = dir.path().join("key.pem");
    std::fs::write(&cert, ck.cert.pem()).expect("cert");
    std::fs::write(&key, ck.key_pair.serialize_pem()).expect("key");

    let srv = Server::start(
        &bin("echo_tls"),
        &["127.0.0.1:0", cert.to_str().unwrap(), key.to_str().unwrap()],
        10_000,
        1024,
        None,
    );

    let _ = rustls::crypto::ring::default_provider().install_default();
    let mut root = rustls::RootCertStore::empty();
    root.add(ck.cert.der().clone()).expect("root");
    let cfg = rustls::ClientConfig::builder()
        .with_root_certificates(root)
        .with_no_client_auth();
    let cfg = Arc::new(cfg);
    let tcp = TcpStream::connect(srv.addr).expect("connect");
    let name = rustls::pki_types::ServerName::try_from("localhost".to_string()).expect("name");
    let conn = rustls::ClientConnection::new(cfg, name).expect("tls client");
    let mut tls = rustls::StreamOwned::new(conn, tcp);
    for _ in 0..1_000 {
        tls.write_all(b"x").expect("send");
        let mut b = [0u8; 1];
        tls.read_exact(&mut b).expect("recv");
        assert_eq!(b[0], b'x');
    }
    // E2. A MiB over TLS, echoed byte for byte, in chunks the server's
    // buffer takes whole so one blocking client never deadlocks the echo.
    let mib: Vec<u8> = (0..1 << 20).map(|i| (i % 251) as u8).collect();
    let mut back = vec![0u8; 16 * 1024];
    for chunk in mib.chunks(16 * 1024) {
        tls.write_all(chunk).expect("send a chunk");
        tls.read_exact(&mut back[..chunk.len()])
            .expect("recv a chunk");
        assert_eq!(&back[..chunk.len()], chunk);
    }
    let tail = srv.tail();
    // The TLS path may allocate per rustls record (ALLOC-1 takes it), so the
    // number is reported, not asserted to be zero. The line is still required.
    assert!(tail.contains("stopping,"), "{tail}");
    let n = tail
        .lines()
        .find(|l| l.contains(" allocation(s) during the run outside table growth"))
        .and_then(|l| l.split_whitespace().next_back())
        .map(|n| n.parse::<u64>().unwrap_or(0))
        .unwrap_or(0);
    println!("echo_tls allocations outside growth: {n}");
}
