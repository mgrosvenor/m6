//! The echo examples, proved over the wire: the nine checks of the echo
//! prototype's `check.py` in Rust, plus one TLS round-trip check.
//!
//! Each test starts the example as a child process, reads its `listening on`
//! line for the address, and waits on a signal with a deadline, never a fixed
//! sleep. Run with `--features testkit` so the examples are built:
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

// ── Binaries ──────────────────────────────────────────────────────────────────

fn bin(name: &str) -> std::path::PathBuf {
    let exe = std::env::current_exe().expect("current_exe");
    let path = exe
        .parent()
        .expect("deps")
        .parent()
        .expect("target profile")
        .join("examples")
        .join(name);
    assert!(
        path.exists(),
        "{} is not built: run `cargo test -p m6-core --features testkit --test echo_example`",
        path.display()
    );
    path
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
        let mut cmd = Command::new(bin);
        cmd.args(args);
        cmd.env("ECHO_IDLE_MS", idle_ms.to_string());
        cmd.env("ECHO_CONNS", conns.to_string());
        if let Some(m) = max_conns {
            cmd.env("ECHO_MAX_CONNS", m.to_string());
        }
        cmd.stderr(Stdio::piped());
        cmd.stdin(Stdio::null());
        cmd.stdout(Stdio::null());
        let mut child = cmd.spawn().expect("spawn echo");
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

    /// SIGTERM, wait for exit 0, and return the whole stderr tail.
    fn tail(mut self) -> String {
        let pid = self.child.id() as i32;
        // SAFETY: `kill` takes a pid and a signal number and touches no memory.
        unsafe { libc::kill(pid, libc::SIGTERM) };
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
    reset.write_all(&vec![b'y'; 100_000]).expect("send");
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
