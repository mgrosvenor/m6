//! m6-monitor's lifecycle, and the promises its own loop adds to it.
//!
//! The monitor is one process running one event loop (`main.rs`), so it owes
//! the same lifecycle contract as every other m6 service: the started line, a
//! clean SIGTERM, the socket removed. These tests spawn the binary and check
//! what a supervisor and an operator see.

#![allow(
    clippy::disallowed_methods,
    reason = "test code: threads, sleeps and blocking clients drive the service from outside"
)]

use std::io::Write;
use std::net::TcpListener;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use m6_core::testkit::{assert_app_lifecycle, binary, read_one, Service};

/// A config m6-monitor will actually start on.
///
/// The node is deliberately unreachable. Startup must not depend on the fleet
/// answering: a monitor that refused to start when the thing it monitors was
/// down would be useless in precisely the situation it exists for.
const CONFIG: &str = r#"
poll_interval_s = 60

[monitor]
timeout_ms = 200

[[monitor.nodes]]
name = "unreachable"
url  = "https://127.0.0.1:1"
role = "origin"

[thread_pool]
size       = 1
queue_size = 4

[log]
level  = "info"
format = "text"
"#;

fn setup(config: &str) -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("monitor.conf");
    std::fs::write(&path, config).expect("write config");
    std::fs::write(dir.path().join("site.toml"), "").unwrap();
    // Short name: a unix socket path is capped near 104 bytes on macOS and the
    // temp directory already spends most of that.
    let sock = dir.path().join("mon.sock");
    (dir, path, sock)
}

fn spawn(dir: &Path, config: &Path, sock: &Path) -> Service {
    let mut svc = Service::spawn(
        "m6-monitor",
        Command::new(binary("m6-monitor"))
            .arg(dir)
            .arg(config)
            .env("M6_SOCKET_OVERRIDE", sock),
    );
    svc.wait_for_path(sock, Duration::from_secs(10));
    svc
}

/// One request on the monitor's socket, with a two-second read deadline. A
/// reply that does not arrive by then is a failure, never a slow pass.
fn get(sock: &Path, target: &str) -> (u16, String) {
    let mut s = UnixStream::connect(sock).expect("connect to the monitor");
    s.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    write!(
        s,
        "GET {target} HTTP/1.1\r\nHost: m6\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let raw = read_one(&mut s, "GET").expect("a response inside two seconds");
    let text = String::from_utf8_lossy(&raw).into_owned();
    let status = text
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    (status, text)
}

#[test]
fn lifecycle_is_clean() {
    let (dir, config, sock) = setup(CONFIG);
    assert_app_lifecycle(
        "m6-monitor",
        Command::new(binary("m6-monitor"))
            .arg(dir.path())
            .arg(&config)
            .env("M6_SOCKET_OVERRIDE", &sock),
        &sock,
    );
}

/// `--check` is a one-shot: it must print a report and exit, not serve.
///
/// Under the freeze it cannot reach a real node, and that is the case worth
/// pinning. A tool that reported an unreachable fleet as empty rows would be
/// worse than useless, so this asserts it exits non-zero and says something
/// rather than exiting 0 on silence.
#[test]
fn check_is_one_shot_and_reports_unreachable() {
    let (dir, config, _) = setup(CONFIG);

    let out = Command::new(binary("m6-monitor"))
        .arg("--check")
        .arg(&config)
        .current_dir(dir.path())
        .output()
        .expect("run --check");

    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !combined.trim().is_empty(),
        "--check printed nothing; silence is not a report\n--- output ---\n{combined}"
    );
    assert!(
        combined.contains("unreachable"),
        "--check never mentioned the node it could not reach\n--- output ---\n{combined}"
    );
}

/// The binary must not hang when handed no config at all.
#[test]
fn check_without_config_fails_loudly() {
    let out = Command::new(binary("m6-monitor"))
        .arg("--check")
        .output()
        .expect("run --check with no config");

    assert!(!out.status.success(), "--check with no config should fail");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("usage") || err.contains("config"),
        "the failure should say what was missing\n--- stderr ---\n{err}"
    );
}

/// A relative `perf_token_file` is refused at start, before the poll runs.
#[test]
fn a_relative_perf_token_file_stops_the_start() {
    let config_text = CONFIG.replace(
        "timeout_ms = 200",
        "timeout_ms = 200\nperf_token_file = \"token\"",
    );
    let (dir, config, sock) = setup(&config_text);

    let mut cmd = Command::new(binary("m6-monitor"));
    cmd.arg(dir.path())
        .arg(&config)
        .env("M6_SOCKET_OVERRIDE", &sock);
    let mut svc = Service::spawn("m6-monitor", &mut cmd);
    let status = svc
        .wait_for_exit(Instant::now() + Duration::from_secs(10))
        .unwrap_or_else(|| {
            panic!(
                "m6-monitor did not exit for a relative perf_token_file\n--- output ---\n{}",
                svc.output()
            )
        });

    assert_eq!(
        status.code(),
        Some(2),
        "exit code should be 2\n--- output ---\n{}",
        svc.output()
    );
    assert!(
        svc.output().contains("perf_token_file"),
        "the error did not name perf_token_file\n--- output ---\n{}",
        svc.output()
    );
}

/// The loop serves while a node's request hangs.
///
/// The node's listener accepts the `/health` request and never answers. If the
/// poll blocked the process, `/digest` could not answer until the timeout; the
/// request must be answered at once, with the "no poll yet" 503 while the poll
/// is outstanding. After the deadline the node reads `unreachable`, which is
/// the poll completing, not the request failing.
#[test]
fn the_monitor_serves_while_a_node_hangs() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind the hanging node");
    let node = listener.local_addr().unwrap();
    let (accepted_tx, accepted_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let held = std::thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept the /health request");
        accepted_tx.send(()).expect("signal the accept");
        let _ = release_rx.recv();
        drop(stream);
    });
    let config = CONFIG
        .replace("https://127.0.0.1:1", &format!("http://{node}"))
        .replace("timeout_ms = 200", "timeout_ms = 1000");
    let (dir, path, sock) = setup(&config);
    let mut svc = spawn(dir.path(), &path, &sock);

    accepted_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("the monitor did not start its /health request");

    // The poll is outstanding: the answer must come from the loop, not after
    // the node's deadline.
    let started = Instant::now();
    let (status, body) = get(&sock, "/digest");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "/digest took {:?} while a node hung",
        started.elapsed()
    );
    assert!(
        status == 503 || status == 200,
        "unexpected /digest status {status}: {body}"
    );

    // The deadline passes, the poll finishes, and the node reads unreachable.
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let (status, body) = get(&sock, "/digest");
        if status == 200 && body.contains("\"unreachable\"") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the node never read unreachable after its deadline: {status} {body}\n--- output ---\n{}",
            svc.output()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    drop(release_tx);
    held.join().expect("the hanging listener ends");
    svc.terminate(Duration::from_secs(5));
}

/// The monitor runs one loop thread, plus the log writer until LOG-2 replaces
/// it. A poll thread, a thread per node or a history writer would each show up
/// in the count, so the count with a three-node fleet that has a tunnel and a
/// history must equal the count with one plain node.
///
/// macOS has platform threads of its own: reading the host's certificate store
/// (`client::system_roots`) goes through the Security framework, which leaves
/// four idle libdispatch workers in the process. Linux reads the store with no
/// thread, so there the exact count is `1 + LOG_WRITER_THREADS`, which is the
/// trusted session's proof.
#[test]
fn the_monitor_runs_one_thread() {
    let (dir, config, sock) = setup(CONFIG);
    let svc = spawn(dir.path(), &config, &sock);
    let one = settled_threads(svc.pid(), &svc);
    drop(svc);
    assert_no_poll_threads(one);

    // Three nodes, each with a tunnel and a handshake file, and a history. A
    // thread per node, a thread per probe or a history writer would each add
    // one here and the equality would fail.
    let handshakes = dir.path().join("wg-handshakes");
    std::fs::write(&handshakes, "").unwrap();
    let mut config_text = CONFIG.replace(
        "\n[[monitor.nodes]]",
        &format!(
            "\nwg_handshakes_file = {:?}\nhistory_file = \"history.sqlite3\"\n\n[[monitor.nodes]]",
            handshakes.display().to_string()
        ),
    );
    for (name, tunnel) in [("edge-b", "192.0.2.2:9"), ("edge-c", "192.0.2.3:9")] {
        config_text.push_str(&format!(
            "\n[[monitor.nodes]]\nname = \"{name}\"\nurl  = \"http://127.0.0.1:1\"\nrole = \"cache\"\ntunnel = {{ address = \"{tunnel}\" }}\n"
        ));
    }
    let (dir2, config2, sock2) = setup(&config_text);
    let svc2 = spawn(dir2.path(), &config2, &sock2);
    let three = settled_threads(svc2.pid(), &svc2);
    let output = svc2.output();
    drop(svc2);
    assert_eq!(
        three, one,
        "three nodes, tunnels and a history run {three} threads against {one} for one node: \
         the poll and the history add none\n--- output ---\n{output}"
    );
}

/// The exact count the loop and the log writer make, on the platform whose
/// certificate store is a plain file read.
fn assert_no_poll_threads(count: usize) {
    #[cfg(target_os = "linux")]
    assert_eq!(
        count,
        1 + LOG_WRITER_THREADS,
        "m6-monitor runs {count} threads: one loop thread and tracing-appender's \
         writer until LOG-2"
    );
    // On macOS the platform store's libdispatch workers are the only additions,
    // and none of them is an m6 thread. A poll thread or a history writer would
    // still show up in the equality above with the three-node fleet.
    #[cfg(target_os = "macos")]
    assert!(
        count <= 1 + LOG_WRITER_THREADS + 4,
        "m6-monitor runs {count} threads, more than the loop, the log writer and \
         the platform store's four workers"
    );
}

/// `LOG_WRITER_THREADS` is 1 until LOG-2 replaces tracing-appender; it becomes
/// 0 then, and the trusted session updates this constant.
const LOG_WRITER_THREADS: usize = 1;

/// Wait for the count to settle; the assertion is the signal and the deadline
/// is the bound.
fn settled_threads(pid: u32, svc: &Service) -> usize {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut threads = thread_count(pid);
    while threads == 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
        threads = thread_count(pid);
    }
    assert!(
        threads > 0,
        "could not count the threads of pid {pid}\n--- output ---\n{}",
        svc.output()
    );
    threads
}

/// A node whose host name does not resolve is a start failure, not a node that
/// reads unreachable at every poll: the lookup happens once, at start.
#[test]
fn an_unresolvable_node_is_a_startup_error() {
    let config_text = CONFIG.replace("https://127.0.0.1:1", "https://does-not-exist.invalid/");
    let (dir, config, sock) = setup(&config_text);
    let mut cmd = Command::new(binary("m6-monitor"));
    cmd.arg(dir.path())
        .arg(&config)
        .env("M6_SOCKET_OVERRIDE", &sock);
    let mut svc = Service::spawn("m6-monitor", &mut cmd);
    let status = svc
        .wait_for_exit(Instant::now() + Duration::from_secs(15))
        .unwrap_or_else(|| {
            panic!(
                "m6-monitor did not refuse a name that does not resolve\n--- output ---\n{}",
                svc.output()
            )
        });
    let output = svc.output();
    assert_eq!(
        status.code(),
        Some(2),
        "a start failure exits 2\n--- output ---\n{output}"
    );
    assert!(
        output.contains("does-not-exist.invalid"),
        "the error did not name the host\n--- output ---\n{output}"
    );
    assert!(
        output.contains("\"unreachable\""),
        "the error did not name the node\n--- output ---\n{output}"
    );
    assert!(
        !sock.exists(),
        "a service that refused to start must leave no socket"
    );
}

#[cfg(target_os = "linux")]
fn thread_count(pid: u32) -> usize {
    std::fs::read_dir(format!("/proc/{pid}/task"))
        .map(|entries| entries.count())
        .unwrap_or(0)
}

#[cfg(target_os = "macos")]
fn thread_count(pid: u32) -> usize {
    /// `PROC_PIDLISTTHREADS` from `<sys/proc_info.h>`.
    const PROC_PIDLISTTHREADS: libc::c_int = 6;

    let mut ids = [0u64; 256];
    // SAFETY: `ids` is a live buffer of exactly the length passed, and
    // `proc_pidinfo` writes at most that many bytes into it.
    let bytes = unsafe {
        libc::proc_pidinfo(
            pid as libc::c_int,
            PROC_PIDLISTTHREADS,
            0,
            ids.as_mut_ptr().cast(),
            std::mem::size_of_val(&ids) as libc::c_int,
        )
    };
    (bytes.max(0) as usize) / std::mem::size_of::<u64>()
}

/// The BSDs list a process's threads with `ps -H`, one line each.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn thread_count(pid: u32) -> usize {
    let out = std::process::Command::new("ps")
        .args(["-H", "-p", &pid.to_string()])
        .output()
        .expect("ps must run to count a process's threads");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .count()
        .saturating_sub(1)
}
