//! The echo programs, the reference loop, proved over the wire: the nine
//! checks of the echo prototype's `check.py` in Rust, the TLS round trips,
//! and the platform cases (many connections at once, a
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
use std::process::Command;

use m6_core::platform::process::{Child, Io, Streams};
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
        static SWEPT: std::sync::OnceLock<()> = std::sync::OnceLock::new();
        SWEPT.get_or_init(|| {
            let _ = platform::process::close_inherited_on_exec();
        });
        cmd.args(args);
        cmd.env("ECHO_IDLE_MS", idle_ms.to_string());
        cmd.env("ECHO_CONNS", conns.to_string());
        // A shutdown with clients still held open must not hold the tests up.
        cmd.env("ECHO_DRAIN_MS", "500");
        if let Some(m) = max_conns {
            cmd.env("ECHO_MAX_CONNS", m.to_string());
        }
        let streams = Streams {
            stdin: Io::Null,
            stdout: Io::Null,
            stderr: Io::Piped,
        };
        let mut child = platform::process::spawn(&mut cmd, streams).expect("spawn echo");
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

    /// Everything the program has written so far, without stopping it.
    fn output_so_far(&self) -> String {
        let out = self.output.lock().unwrap_or_else(|e| e.into_inner());
        String::from_utf8_lossy(&out).into_owned()
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
    read_exact_now(s, n)
}

/// Read `n` bytes under the read timeout the caller already set. For a socket
/// the server may close before the read starts: macOS refuses `setsockopt`
/// (`EINVAL`) on a socket whose peer has closed, so the timeout must be set
/// while the socket is still open, never after the server has had time to act.
fn read_exact_now(s: &mut TcpStream, n: usize) -> Vec<u8> {
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

/// One integer count beside `key` in a refusal line, in either the JSON log
/// line a service writes or the plain stderr line the echo program writes.
fn count_field(line: &str, key: &str) -> u64 {
    let at = line
        .find(key)
        .unwrap_or_else(|| panic!("no {key} in {line}"))
        + key.len();
    let rest = &line[at..];
    let start = rest
        .find(|c: char| c.is_ascii_digit())
        .unwrap_or_else(|| panic!("no count after {key} in {line}"));
    let rest = &rest[start..];
    let end = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    rest[..end]
        .parse()
        .unwrap_or_else(|e| panic!("{key} in {line}: {e}"))
}

/// The sum of the `rejected` fields and the greatest `total` in the refusal
/// lines of `output`. A service that has written every count it took has the
/// two equal.
fn refused_totals(output: &str) -> (u64, u64) {
    let mut counted = 0u64;
    let mut total = 0u64;
    for line in output
        .lines()
        .filter(|line| line.contains(m6_core::net::REFUSED_MESSAGE) && line.contains("total"))
    {
        counted += count_field(line, "rejected");
        total = total.max(count_field(line, "total"));
    }
    (counted, total)
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
    // Set before the half-close: the server closes as soon as it has drained,
    // and a loaded host can let that happen before this thread's next call.
    s.set_read_timeout(Some(Duration::from_secs(15)))
        .expect("set_read_timeout");
    let body = vec![b'x'; 300_000];
    s.write_all(&body).expect("send");
    s.shutdown(std::net::Shutdown::Write)
        .expect("shutdown write");
    assert_eq!(read_exact_now(&mut s, 300_000), body);
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

// ── The cases the platform layer adds (E2 to E6, E9) ─────────────────────────

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

/// E10 (restored). Out of descriptors while clients queue: every client that
/// is already queued reaches a definite outcome with no new connection
/// arriving, and the listener serves again once descriptors free.
///
/// The shell lowers the limit to 40 descriptors while the table is told it may
/// hold 64, so `accept` fails with `EMFILE` once about thirty connections are
/// open and the rest wait in the kernel's queue. This is the case the audit
/// found: the accept arm stopped on the error, the listener was left
/// edge-triggered with a connection queued, and the only thing that could wake
/// it was another connection, which a full queue cannot deliver. The queued
/// client therefore waited for the life of the process.
///
/// **Break:** an accept loop that returns from the arm on `EMFILE` (the queued
/// clients then sit in the queue: this test's read times out), one that logs
/// the refusal and stops draining, or an echo that returns the error and exits.
#[test]
fn queued_clients_are_refused_with_no_new_connection_arriving() {
    let cmd = m6_core::testkit::lowered_fd_limit_command(40, 0, bin("echo"));
    // A long idle deadline: the served clients are held across two intervals
    // and a stopped burst, and an idle close would free descriptors under the
    // burst.
    let srv = Server::start_cmd(cmd, &["127.0.0.1:0"], 120_000, 64, Some(64));
    let addr = srv.addr;

    // Each client writes one byte and waits for it back. The ones the limit
    // cannot take are refused by the loop with the reserve descriptor, so the
    // outcome is written to the socket and no client is left waiting.
    let mut served: Vec<TcpStream> = Vec::new();
    let mut refused = 0usize;
    let mut waiting: Vec<TcpStream> = Vec::new();
    for i in 0..48 {
        let mut c = TcpStream::connect(addr).unwrap_or_else(|e| panic!("client {i}: connect: {e}"));
        c.set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap_or_else(|e| panic!("client {i}: read timeout: {e}"));
        // The kernel may have reset the connection already, because accepting
        // it failed for want of a descriptor. That is the refusal.
        let _ = c.write_all(b"q");
        let mut b = [0u8; 1];
        match c.read(&mut b) {
            Ok(1) if b[0] == b'q' => served.push(c),
            // Closed by the loop: the connection was refused.
            Ok(0) => refused += 1,
            Ok(n) => panic!("client {i}: unexpected read of {n}"),
            Err(e) if e.kind() == ErrorKind::WouldBlock || e.kind() == ErrorKind::TimedOut => {
                waiting.push(c)
            }
            Err(e) if e.kind() == ErrorKind::ConnectionReset => refused += 1,
            Err(e) => panic!("client {i}: read: {e}"),
        }
    }

    assert!(
        served.len() >= 8,
        "some clients were served: {}",
        served.len()
    );
    assert!(refused >= 1, "the descriptor limit refused nobody");
    // The audit's finding, as an assertion: with no descriptor free, a queued
    // connection is refused in the same turn instead of waiting for a new
    // connection that a full queue cannot deliver.
    assert!(
        waiting.is_empty(),
        "{} client(s) were left waiting for a wakeup that cannot arrive: the \
         listener consumed the readiness and never drained",
        waiting.len()
    );
    // The interval passes, and one more connection is refused: its turn ends by
    // writing what the rate limit held back, so the burst below starts with
    // nothing owed. When the service counted nothing for the clients above,
    // there is nothing to write and this leaves the totals where they are.
    std::thread::sleep(m6_core::net::REFUSED_INTERVAL + Duration::from_millis(300));
    let before_flush = refused_totals(&srv.output_so_far()).1;
    let mut flush = TcpStream::connect(addr).expect("flush connect");
    flush
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("flush read timeout");
    let _ = flush.write_all(b"flush\n");
    let mut byte = [0u8; 1];
    let _ = flush.read(&mut byte);
    drop(flush);
    let deadline = Instant::now() + Duration::from_secs(5);
    let before = loop {
        let totals = refused_totals(&srv.output_so_far());
        if totals.1 > before_flush || Instant::now() >= deadline {
            break totals;
        }
        std::thread::sleep(Duration::from_millis(10));
    };

    // A burst the service must count, with clients that send nothing.
    //
    // Three connections queue while the process is stopped, so one arm sees
    // all three.
    //
    // **The client side cannot say which of them the service counted.** When
    // `accept` fails for want of a descriptor, the kernel may drop a queued
    // connection itself, and the peer reads that as end-of-file, the same read
    // as the service's close over a connection that sent nothing. So the count
    // is bounded below only, and the bound is one: with three connections
    // queued in one arm, a kernel can consume at most two of them, and the
    // reserve takes the last.
    //
    // The interval passes while the process is stopped, so the arm writes its
    // line as it ends and the total can be read without another connection.
    std::thread::sleep(m6_core::net::REFUSED_INTERVAL + Duration::from_millis(300));
    let pid = srv.child.id().to_string();
    let stop = std::process::Command::new("kill")
        .args(["-STOP", &pid])
        .status()
        .expect("stop the echo");
    assert!(stop.success(), "kill -STOP");
    let mut probes: Vec<TcpStream> = (0..3)
        .map(|_| TcpStream::connect(addr).expect("probe connect"))
        .collect();
    for probe in &probes {
        probe
            .set_read_timeout(Some(Duration::from_secs(10)))
            .expect("probe read timeout");
    }
    let cont = std::process::Command::new("kill")
        .args(["-CONT", &pid])
        .status()
        .expect("continue the echo");
    assert!(cont.success(), "kill -CONT");
    for probe in &mut probes {
        let mut byte = [0u8; 1];
        match probe.read(&mut byte) {
            Ok(0) => {}
            Err(e) if e.kind() == ErrorKind::ConnectionReset => {}
            other => panic!(
                "a connection queued while the echo was stopped was neither closed nor reset: \
                 {other:?}"
            ),
        }
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    let after = loop {
        let totals = refused_totals(&srv.output_so_far());
        if totals.1 > before.1 {
            break totals;
        }
        assert!(
            Instant::now() < deadline,
            "the queued burst wrote no line at all:\n{}",
            srv.output_so_far()
        );
        std::thread::sleep(Duration::from_millis(10));
    };
    let counted_in_burst = after.1 - before.1;
    assert!(
        (1..=3).contains(&counted_in_burst),
        "the service counted {counted_in_burst} of the three connections queued in one arm\n{}",
        srv.output_so_far()
    );

    // Descriptors free when the served clients close. The listener serves
    // again, and the line said how many it refused.
    drop(served);
    let mut fresh = TcpStream::connect(addr).expect("connect");
    fresh.write_all(b"new\n").expect("send");
    assert_eq!(recv_exact(&mut fresh, 4, Duration::from_secs(10)), b"new\n");
    drop(fresh);

    let tail = srv.tail();
    // Rate limiting delays a count and never loses one: the counts the lines
    // carry add up to the total, and the total is what the service refused.
    let (counted, total) = refused_totals(&tail);
    assert_eq!(
        counted, total,
        "the counts in the lines add up to the total:\n{tail}"
    );
    // Everything the service counted before the burst is still bounded by the
    // clients that were refused. The burst itself is tied to the arm that
    // queued three connections in one turn.
    assert!(
        total - counted_in_burst <= refused as u64 + 1,
        "the service counted {} refusals beyond the burst, but {refused} clients were refused:\n{tail}",
        total - counted_in_burst
    );
}

/// The listener serves again as soon as a descriptor frees, with no new
/// connection needed.
///
/// Four clients fill a four-slot table and a fifth waits in the kernel's
/// queue. Nothing new connects after that: the fifth is served because closing
/// one held connection frees a descriptor, and the loop re-registers the
/// listener it deregistered at the ceiling. That re-registration reaches the
/// kernel, so a connection still queued is reported again.
///
/// **Break:** a listener left registered and unread, or one whose re-arm is
/// gated on a condition that cannot be true: the fifth client's read times out.
#[test]
fn the_listener_serves_again_as_soon_as_descriptors_free_with_no_new_connection() {
    let srv = Server::start(&bin("echo"), &["127.0.0.1:0"], 30_000, 4, Some(4));
    let addr = srv.addr;

    // Four connections fill the table: each answers before the next connects,
    // so none of them is the queued one.
    let mut held: Vec<TcpStream> = Vec::new();
    for i in 0..4 {
        let mut c = TcpStream::connect(addr).unwrap_or_else(|e| panic!("client {i}: {e}"));
        c.set_read_timeout(Some(Duration::from_secs(10)))
            .expect("read timeout");
        c.write_all(b"a").expect("send");
        assert_eq!(read_exact_now(&mut c, 1), b"a");
        held.push(c);
    }

    // The fifth is queued: the table is full, so the listener is deregistered
    // and no accept happens for it.
    let mut queued = TcpStream::connect(addr).expect("connect");
    queued
        .set_read_timeout(Some(Duration::from_millis(500)))
        .expect("read timeout");
    queued.write_all(b"b").expect("send");
    let mut byte = [0u8; 1];
    assert!(
        queued.read(&mut byte).is_err(),
        "the fifth client was served with the table full"
    );

    // One descriptor frees, and no new connection arrives: the queued client
    // is served.
    drop(held.remove(0));
    queued
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("read timeout");
    assert_eq!(read_exact_now(&mut queued, 1), b"b");
    drop((queued, held));
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

    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
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
    // The TLS path may allocate per rustls record, so the number is reported,
    // not asserted to be zero. The line is still required.
    assert!(tail.contains("stopping,"), "{tail}");
    let n = tail
        .lines()
        .find(|l| l.contains(" allocation(s) during the run outside table growth"))
        .and_then(|l| l.split_whitespace().next_back())
        .map(|n| n.parse::<u64>().unwrap_or(0))
        .unwrap_or(0);
    println!("echo_tls allocations outside growth: {n}");
}
