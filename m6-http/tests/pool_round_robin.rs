//! Backend choice is plain round robin, and a request whose member refuses the
//! connect tries the next member.
//!
//! Each test stands up real `m6-http` processes in front of unix-socket
//! backends. Every wait is on a signal (a port accepting, a process exiting)
//! with a deadline, never a fixed sleep. The servers are stopped by `Drop` when
//! the test ends.

#![allow(
    clippy::disallowed_methods,
    reason = "test code: threads and blocking clients drive the service from outside"
)]

use std::io::{Read, Write};
use std::net::TcpStream;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use m6_core::testkit::{binary, claim_port, read_one, PortClaim, Service};
use m6_http_lib::probe;

/// A unix-socket backend serving one body and counting the requests it served.
///
/// `crash` closes the listener but leaves the socket file, the state a killed
/// backend leaves: the pool still holds the member and its connect is
/// refused. `restart` binds a fresh listener and atomically renames it over the
/// same path, so the member rejoins at the path it always had.
struct Backend {
    path: PathBuf,
    body: &'static str,
    served: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Backend {
    fn start(path: PathBuf, body: &'static str) -> Backend {
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).expect("bind backend socket");
        let stop = Arc::new(AtomicBool::new(false));
        let served = Arc::new(AtomicUsize::new(0));
        let handle = spawn_accept(listener, Arc::clone(&stop), body, Arc::clone(&served));
        Backend {
            path,
            body,
            served,
            stop,
            handle: Some(handle),
        }
    }

    fn served(&self) -> usize {
        self.served.load(Ordering::SeqCst)
    }

    /// Close the listener without removing the socket file: a crash.
    fn crash(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Wake the blocking accept, which then sees `stop` and returns.
        let _ = UnixStream::connect(&self.path);
        let handle = self.handle.take().expect("backend already crashed");
        handle.join().expect("backend accept loop");
    }

    /// Bind a fresh listener and move it onto the same path atomically.
    fn restart(&mut self) {
        let temp = self.path.with_extension("tmp");
        let _ = std::fs::remove_file(&temp);
        let listener = UnixListener::bind(&temp).expect("bind replacement socket");
        std::fs::rename(&temp, &self.path).expect("atomically replace the socket");
        let stop = Arc::new(AtomicBool::new(false));
        self.handle = Some(spawn_accept(
            listener,
            Arc::clone(&stop),
            self.body,
            Arc::clone(&self.served),
        ));
        self.stop = stop;
    }
}

impl Drop for Backend {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if self.handle.is_some() {
            let _ = UnixStream::connect(&self.path);
        }
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
        let _ = std::fs::remove_file(&self.path);
    }
}

fn spawn_accept(
    listener: UnixListener,
    stop: Arc<AtomicBool>,
    body: &'static str,
    served: Arc<AtomicUsize>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        while let Ok((stream, _)) = listener.accept() {
            if stop.load(Ordering::SeqCst) {
                return;
            }
            let served = Arc::clone(&served);
            std::thread::spawn(move || serve(stream, body, &served));
        }
    })
}

fn serve(mut stream: UnixStream, body: &'static str, served: &AtomicUsize) {
    let mut buf = [0u8; 4096];
    let mut got = Vec::new();
    while !got.windows(4).any(|w| w == b"\r\n\r\n") {
        match stream.read(&mut buf) {
            Ok(0) | Err(_) => return,
            Ok(n) => got.extend_from_slice(&buf[..n]),
        }
    }
    served.fetch_add(1, Ordering::SeqCst);
    let resp = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(resp.as_bytes());
    let _ = stream.flush();
}

/// A front end on a claimed loopback port, killed when it drops.
struct FrontEnd {
    service: Service,
    port: u16,
    _claim: PortClaim,
}

impl FrontEnd {
    fn start(site: &Path, tag: &str) -> FrontEnd {
        let claim = claim_port();
        let port = claim.port();
        let system = site.join(format!("system-{tag}.toml"));
        std::fs::write(
            &system,
            system_toml(
                port,
                &site.join("cert.pem").display().to_string(),
                &site.join("key.pem").display().to_string(),
                tag,
            ),
        )
        .expect("system.toml");
        let mut service = Service::spawn(
            "m6-http",
            Command::new(binary("m6-http")).arg(site).arg(&system),
        );
        service.wait_for_tcp(port, Duration::from_secs(20));
        FrontEnd {
            service,
            port,
            _claim: claim,
        }
    }
}

/// The backends, in the order the pool's glob found them, and the front ends.
struct Fixture {
    // Declared before `backends` so the front ends stop first when the fixture
    // drops.
    front_ends: Vec<FrontEnd>,
    backends: Vec<Backend>,
    _dir: tempfile::TempDir,
}

impl Fixture {
    fn start(members: &[&'static str], front_ends: usize) -> Fixture {
        let dir = tempfile::tempdir().expect("tempdir");
        let site = dir.path().to_path_buf();
        let ck = rcgen::generate_simple_self_signed(vec![
            "localhost".to_string(),
            "127.0.0.1".to_string(),
        ])
        .expect("certificate");
        std::fs::write(site.join("cert.pem"), ck.cert.pem()).expect("cert");
        std::fs::write(site.join("key.pem"), ck.key_pair.serialize_pem()).expect("key");

        let mut backends = Vec::new();
        for body in members {
            let path = site.join(format!("m6-rr-{body}.sock"));
            backends.push(Backend::start(path, body));
        }
        let glob = site.join("m6-rr-*.sock").display().to_string();
        std::fs::write(site.join("site.toml"), site_toml(&glob)).expect("site.toml");

        let names = ["one", "two", "three"];
        let mut running = Vec::new();
        for tag in names.iter().take(front_ends) {
            running.push(FrontEnd::start(&site, tag));
        }
        Fixture {
            front_ends: running,
            backends,
            _dir: dir,
        }
    }

    fn counts(&self) -> Vec<usize> {
        self.backends.iter().map(Backend::served).collect()
    }
}

/// One GET over HTTP/1.1 and TLS. Returns the status and the body.
fn get(port: u16, path: &str) -> Result<(u16, String), String> {
    probe::install_crypto();
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

fn site_toml(glob: &str) -> String {
    format!(
        "[site]\nname = \"pool-round-robin\"\ndomain = \"localhost\"\n\n\
         [log]\nlevel = \"warn\"\n\n\
         [errors]\nmode = \"internal\"\n\n\
         [analytics]\nenabled = false\n\n\
         [rate_limit]\nenabled = false\n\n\
         [[backend]]\nname = \"app\"\nsockets = \"{glob}\"\n\n\
         [[route]]\npath = \"/\"\nbackend = \"app\"\n"
    )
}

fn system_toml(bind_port: u16, cert: &str, key: &str, node: &str) -> String {
    format!(
        "[server]\nbind = \"127.0.0.1:{bind_port}\"\ntls_cert = \"{cert}\"\ntls_key = \"{key}\"\n\n\
         [node]\nname = \"pool-round-robin-{node}\"\n"
    )
}

/// `requests = members * k` requests leave each member with exactly `k`.
fn each_member_serves_k_times(members: &[&'static str], k: usize) {
    let fx = Fixture::start(members, 1);
    let port = fx.front_ends[0].port;
    for i in 0..members.len() * k {
        get_ok(port, &format!("request {i}"));
    }
    assert_eq!(
        fx.counts(),
        vec![k; members.len()],
        "each of {} members must serve exactly {k} of {} requests",
        members.len(),
        members.len() * k
    );
}

#[test]
fn three_members_each_serve_k_times() {
    each_member_serves_k_times(&["a", "b", "c"], 4);
}

#[test]
fn four_members_each_serve_k_times() {
    each_member_serves_k_times(&["a", "b", "c", "d"], 3);
}

#[test]
fn a_dead_member_is_skipped_and_serves_again_in_turn_after_restart() {
    let fx = Fixture::start(&["a", "b", "c"], 1);
    let port = fx.front_ends[0].port;

    // Three requests put one on each member and leave the turn on the first.
    for i in 0..3 {
        get_ok(port, &format!("warm-up {i}"));
    }
    assert_eq!(
        fx.counts(),
        vec![1, 1, 1],
        "warm-up must reach every member"
    );

    let mut backends = fx.backends;
    // Crash one member. Its socket file stays, so the pool still holds it and a
    // connect to it is refused.
    let mut dead = backends.remove(0);
    let dead_body = dead.body;
    dead.crash();
    let dead_served = dead.served();

    // Every later request is served: the turn skips the dead member for that
    // request, and the two live members split eight requests evenly.
    for i in 0..8 {
        let body = get_ok(port, &format!("request {i} after the crash"));
        assert_ne!(body, dead_body, "the crashed member served a request");
    }
    let mut counts = backends.iter().map(Backend::served).collect::<Vec<_>>();
    counts.sort_unstable();
    assert_eq!(
        counts,
        vec![5, 5],
        "the two live members must serve four each after the crash"
    );
    assert_eq!(
        dead.served(),
        dead_served,
        "the crashed member must serve nothing while it is down"
    );

    // Restart it on its own path. Six requests put two on each member again,
    // the restarted one included.
    let before = backends.iter().map(Backend::served).collect::<Vec<_>>();
    dead.restart();
    for i in 0..6 {
        get_ok(port, &format!("request {i} after the restart"));
    }
    for (backend, had) in backends.iter().zip(before) {
        assert_eq!(
            backend.served() - had,
            2,
            "member {} must serve two of the six requests after the restart",
            backend.body
        );
    }
    assert_eq!(
        dead.served() - dead_served,
        2,
        "the restarted member must serve again in its turn"
    );
}

#[test]
fn front_ends_on_separate_ports_share_one_backend_set() {
    let mut fx = Fixture::start(&["a", "b"], 3);

    // Each front end serves on its own, taking its own turn through the shared
    // members: four requests put two on each member.
    for i in 0..fx.front_ends.len() {
        let port = fx.front_ends[i].port;
        let before = fx.counts();
        for n in 0..4 {
            get_ok(port, &format!("front end {i}, request {n}"));
        }
        let after = fx.counts();
        for (n, (had, has)) in before.iter().zip(after.iter()).enumerate() {
            assert_eq!(
                has - had,
                2,
                "front end {i} must send two of its four requests to member {n}"
            );
        }
    }

    // Stop the middle front end. The other two keep serving.
    let mut stopped = fx.front_ends.remove(1);
    let stopped_port = stopped.port;
    let status = stopped.service.terminate(Duration::from_secs(10));
    assert!(status.success(), "front end did not shut down cleanly");
    assert!(
        TcpStream::connect(("127.0.0.1", stopped_port)).is_err(),
        "the stopped front end still accepts connections"
    );
    for i in 0..fx.front_ends.len() {
        let port = fx.front_ends[i].port;
        let before = fx.counts();
        for n in 0..4 {
            get_ok(port, &format!("surviving front end {i}, request {n}"));
        }
        let after = fx.counts();
        for (n, (had, has)) in before.iter().zip(after.iter()).enumerate() {
            assert_eq!(
                has - had,
                2,
                "surviving front end {i} must send two of its four requests to member {n}"
            );
        }
    }
}
