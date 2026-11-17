//! The front end scales by running more processes, on separate ports or on one
//! shared public port.
//!
//! `[server] reuse_port` routes the TLS listener and the QUIC UDP socket
//! through `m6_platform::net::listen_shared`, so two or three `m6-http`
//! processes bind one address. Each test stands up real processes in front of
//! unix-socket backends, asserts that every request got 200, and stops one
//! process mid-run while the rest keep serving.
//!
//! Every wait is on a signal (the process's own "listeners bound" line, a
//! process exit) with a deadline, never a fixed sleep. `Service` kills and
//! reaps its child on drop, so a test that panics between start and stop leaves
//! no process behind, and `PortClaim` keeps the shared port out of another
//! test's reach until every server on it is gone.
//!
//! No assertion counts which process served a request. `SO_REUSEPORT` spreads
//! TCP connections by a hash on Linux and differently on macOS, so the split
//! is not even and is not the subject. macOS gives a UDP datagram to one
//! socket, so nothing here assumes two processes see QUIC traffic.

#![allow(
    clippy::disallowed_methods,
    reason = "test code: threads and blocking clients drive the service from outside"
)]

use std::io::{Read, Write};
use std::net::TcpStream;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use m6_core::testkit::{binary, claim_port, read_one, Service};
use m6_http_lib::probe;

/// A unix-socket backend that answers every request 200 with a fixed body.
struct Backend {
    path: PathBuf,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Backend {
    fn start(dir: &Path, name: &str) -> Backend {
        let path = dir.join(format!("m6-scale-{name}.sock"));
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).expect("bind backend socket");
        let stop = Arc::new(AtomicBool::new(false));
        let running = Arc::clone(&stop);
        let handle = std::thread::spawn(move || {
            while let Ok((stream, _)) = listener.accept() {
                if running.load(Ordering::SeqCst) {
                    return;
                }
                std::thread::spawn(move || serve(stream));
            }
        });
        Backend {
            path,
            stop,
            handle: Some(handle),
        }
    }
}

fn serve(mut stream: UnixStream) {
    let mut buf = [0u8; 4096];
    let mut got = Vec::new();
    while !got.windows(4).any(|w| w == b"\r\n\r\n") {
        match stream.read(&mut buf) {
            Ok(0) | Err(_) => return,
            Ok(n) => got.extend_from_slice(&buf[..n]),
        }
    }
    let body = "ok";
    let resp = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(resp.as_bytes());
    let _ = stream.flush();
}

impl Drop for Backend {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if self.handle.is_some() {
            // Wake the blocking accept, which then sees the flag and returns.
            let _ = UnixStream::connect(&self.path);
        }
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
        let _ = std::fs::remove_file(&self.path);
    }
}

/// One site tree with its certificate and a backend set.
struct Site {
    // Declared before `dir` so the backends stop while their socket files
    // still exist, which is what wakes the blocking accept, and only then is
    // the directory removed.
    _backends: Vec<Backend>,
    dir: tempfile::TempDir,
}

impl Site {
    fn start(backends: usize) -> Site {
        let dir = tempfile::tempdir_in(m6_core::testkit::socket_scratch_root()).expect("tempdir");
        let ck = rcgen::generate_simple_self_signed(vec![
            "localhost".to_string(),
            "127.0.0.1".to_string(),
        ])
        .expect("certificate");
        std::fs::write(dir.path().join("cert.pem"), ck.cert.pem()).expect("cert");
        std::fs::write(dir.path().join("key.pem"), ck.key_pair.serialize_pem()).expect("key");

        let mut running = Vec::new();
        for name in ["a", "b", "c"].iter().take(backends) {
            running.push(Backend::start(dir.path(), name));
        }
        let glob = dir.path().join("m6-scale-*.sock").display().to_string();
        std::fs::write(dir.path().join("site.toml"), site_toml(&glob)).expect("site.toml");
        Site {
            _backends: running,
            dir,
        }
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }
}

fn site_toml(glob: &str) -> String {
    format!(
        "[site]\nname = \"scale-reuse-port\"\ndomain = \"localhost\"\n\n\
         [log]\nlevel = \"info\"\n\n\
         [errors]\nmode = \"internal\"\n\n\
         [analytics]\nenabled = false\n\n\
         [rate_limit]\nenabled = false\n\n\
         [[backend]]\nname = \"app\"\nsockets = \"{glob}\"\n\n\
         [[route]]\npath = \"/\"\nbackend = \"app\"\n"
    )
}

fn system_toml(site: &Path, port: u16, node: &str, reuse_port: bool, extra: &str) -> String {
    format!(
        "[server]\nbind = \"127.0.0.1:{port}\"\ntls_cert = \"{}\"\ntls_key = \"{}\"\n\
         reuse_port = {reuse_port}\n{extra}\n\n[node]\nname = \"scale-{node}\"\n",
        site.join("cert.pem").display(),
        site.join("key.pem").display(),
    )
}

/// Start one m6-http and wait until it says its own listeners are bound.
///
/// The log line is the per-process readiness signal. Waiting for the port to
/// accept would be satisfied by a sibling on the same port, and a process that
/// failed to bind exits before the line, which `assert_alive` reports with its
/// output.
fn start_front_end(site: &Path, node: &str, port: u16, reuse_port: bool) -> Service {
    start_front_end_with(site, node, port, reuse_port, "")
}

/// [`start_front_end`] with more `[server]` lines.
fn start_front_end_with(
    site: &Path,
    node: &str,
    port: u16,
    reuse_port: bool,
    extra: &str,
) -> Service {
    let system = site.join(format!("system-{node}.toml"));
    std::fs::write(&system, system_toml(site, port, node, reuse_port, extra)).expect("system.toml");
    let mut service = Service::spawn(
        "m6-http",
        Command::new(binary("m6-http")).arg(site).arg(&system),
    );
    let deadline = m6_core::platform::now() + m6_core::testkit::STARTUP_BOUND;
    loop {
        service.assert_alive(&format!("waiting for front end {node} to bind"));
        if service.output().contains("listeners bound") {
            return service;
        }
        if m6_core::platform::now() >= deadline {
            panic!(
                "front end {node} never logged that its listeners were bound within 20s\n\
                 --- output ---\n{}",
                service.output()
            );
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// One GET over HTTP/1.1 and TLS. Returns the status and the body.
fn get(port: u16, path: &str) -> Result<(u16, String), String> {
    let tcp = TcpStream::connect(("127.0.0.1", port)).map_err(|e| format!("connect: {e}"))?;
    tcp.set_read_timeout(Some(Duration::from_secs(10)))
        .map_err(|e| e.to_string())?;
    let name = rustls::pki_types::ServerName::try_from("127.0.0.1".to_string())
        .map_err(|e| e.to_string())?;
    let conn = rustls::ClientConnection::new(probe::tls_config(b"http/1.1"), name)
        .map_err(|e| e.to_string())?;
    let mut stream = rustls::StreamOwned::new(conn, tcp);
    let req = format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n");
    stream
        .write_all(req.as_bytes())
        .map_err(|e| e.to_string())?;
    stream.flush().map_err(|e| e.to_string())?;
    let raw = read_one(&mut stream, "GET").map_err(|e| format!("read: {e}"))?;
    let text = String::from_utf8_lossy(&raw).into_owned();
    let (head, body) = text
        .split_once("\r\n\r\n")
        .ok_or_else(|| format!("no header block: {text}"))?;
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| format!("no status: {head}"))?;
    Ok((status, body.to_string()))
}

fn get_ok(port: u16, context: &str) -> String {
    let (status, body) = get(port, "/").unwrap_or_else(|e| panic!("{context}: {e}"));
    assert_eq!(status, 200, "{context}: {body}");
    body
}

/// Every request in a phase gets 200, and the backend answered.
fn every_request_is_200(port: u16, count: usize, phase: &str) {
    for i in 0..count {
        let body = get_ok(port, &format!("{phase}, request {i}"));
        assert_eq!(body, "ok", "{phase}, request {i}");
    }
}

/// Three processes bind one port with `[server] reuse_port`, every request is
/// 200, and stopping one mid-run leaves the other two serving it.
#[test]
fn three_front_ends_share_one_port_with_reuse_port() {
    let site = Site::start(2);
    let claim = claim_port();
    let port = claim.port();

    // Each start waits for that process's own "listeners bound" line, so the
    // three processes all bound one port: without SO_REUSEPORT the second and
    // third would exit at bind and the wait would report their output.
    let mut front_ends = [
        start_front_end(site.path(), "one", port, true),
        start_front_end(site.path(), "two", port, true),
        start_front_end(site.path(), "three", port, true),
    ];

    every_request_is_200(port, 12, "three front ends on one port");

    // Stop the middle process while the run continues. The request phases do
    // not overlap the stop: m6-http exits on SIGTERM without draining, so a
    // request in flight at that instant is reset by the kernel rather than
    // answered. Every request before and after is 200, and the port is served
    // by the two that remain.
    let status = front_ends[1].terminate(m6_core::testkit::STARTUP_BOUND);
    assert!(status.success(), "front end two did not shut down cleanly");
    every_request_is_200(port, 12, "after one of three front ends stopped");

    // The survivors are still running, and the stopped one is gone.
    assert!(
        TcpStream::connect(("127.0.0.1", port)).is_ok(),
        "the shared port stopped accepting"
    );
    let status = front_ends[0].terminate(m6_core::testkit::STARTUP_BOUND);
    assert!(status.success(), "front end one did not shut down cleanly");
    every_request_is_200(port, 6, "one front end left on the shared port");
}

/// Two processes on separate ports share one backend set: each serves alone,
/// and stopping one mid-run leaves the other serving every request.
#[test]
fn two_front_ends_on_separate_ports_share_one_backend_set() {
    let site = Site::start(2);
    let first = claim_port();
    let second = claim_port();

    let mut front_end = start_front_end(site.path(), "one", first.port(), false);
    let _survivor = start_front_end(site.path(), "two", second.port(), false);

    every_request_is_200(first.port(), 6, "the first front end alone");
    every_request_is_200(second.port(), 6, "the second front end alone");

    // Stop the first while the second keeps serving the same backend set.
    let status = front_end.terminate(m6_core::testkit::STARTUP_BOUND);
    assert!(
        status.success(),
        "the first front end did not shut down cleanly"
    );
    assert!(
        TcpStream::connect(("127.0.0.1", first.port())).is_err(),
        "the stopped front end still accepts connections"
    );
    every_request_is_200(second.port(), 12, "the survivor on its own port");
}

/// Three consecutive free ports, claimed, for a base port plus an index.
fn claim_three_in_a_row() -> Vec<m6_core::testkit::PortClaim> {
    // The claims are handed out in turn, so a run of three is the usual
    // answer. Strays are held until the end so the next try gets new ports.
    let mut held = Vec::new();
    for _ in 0..20 {
        held.push(claim_port());
        let mut ports: Vec<u16> = held.iter().map(|c| c.port()).collect();
        ports.sort_unstable();
        if let Some(base) = ports
            .windows(3)
            .find(|w| w[1] == w[0] + 1 && w[2] == w[1] + 1)
        {
            let keep = *base.first().expect("three");
            held.retain(|c| (keep..keep + 3).contains(&c.port()));
            held.sort_by_key(|c| c.port());
            return held;
        }
    }
    panic!("no three consecutive free ports in 20 claims");
}

/// Each of three processes on one shared port also answers `/health` on its own
/// status port, base plus its index, with that index in the body. Any other
/// path on a status port is 404, stopping one process refuses only its own
/// port, and the others keep answering for themselves. Break: a status port
/// that serves the site, an index that is not reported, a port shared between
/// processes, or a stop that takes the neighbours' ports with it.
#[test]
fn each_front_end_answers_health_on_its_own_status_port() {
    let site = Site::start(1);
    let claim = claim_port();
    let port = claim.port();
    let status_ports = claim_three_in_a_row();
    let base = status_ports[0].port();

    let mut front_ends: Vec<Service> = (0..3u16)
        .map(|i| {
            start_front_end_with(
                site.path(),
                &format!("p{i}"),
                port,
                true,
                &format!(
                    "reuse_port_processes = 3\nprocess_index = {i}\n\
                     status_bind = \"127.0.0.1:{base}\"\n"
                ),
            )
        })
        .collect();

    for i in 0..3u16 {
        let (status, body) = get(base + i, "/health").expect("status port answers");
        assert_eq!(status, 200, "process {i}: {body}");
        assert!(
            body.contains(&format!("\"process_index\":{i}")),
            "process {i} reported another index: {body}"
        );
        let (status, _) = get(base + i, "/").expect("status port answers /");
        assert_eq!(
            status, 404,
            "process {i}: a status port must not serve the site"
        );
    }

    let status = front_ends[1].terminate(m6_core::testkit::STARTUP_BOUND);
    assert!(status.success(), "process 1 did not shut down cleanly");
    assert!(
        TcpStream::connect(("127.0.0.1", base + 1)).is_err(),
        "the stopped process still accepts on its status port"
    );
    for i in [0u16, 2] {
        let (status, body) = get(base + i, "/health").expect("survivor answers");
        assert_eq!(status, 200);
        assert!(body.contains(&format!("\"process_index\":{i}")), "{body}");
    }
    every_request_is_200(port, 6, "the shared port with two processes left");
}
