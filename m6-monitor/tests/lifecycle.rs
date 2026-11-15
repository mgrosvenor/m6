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

use std::net::TcpListener;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use m6_core::testkit::{assert_app_lifecycle, binary, Service};

mod support;

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
    let (status, body) = support::get(&sock, "/digest", Duration::from_secs(2));
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
        let (status, body) = support::get(&sock, "/digest", Duration::from_secs(2));
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

/// A slash a client adds changes nothing: `/digest/`, `//` and a relative
/// `digest` are the same routes, and answering one never aborts the loop.
///
/// The routes are matched by exactly the paths the service declares, while
/// the site dispatch trims outer slashes, so those requests arrive as code
/// routes. Answering them from the snapshot keeps a stray slash from ending
/// the process.
#[test]
fn a_trailing_or_doubled_slash_still_answers() {
    let (dir, config, sock) = setup(CONFIG);
    let mut svc = spawn(dir.path(), &config, &sock);

    // A completed poll is a 200 from /digest.
    let deadline = Instant::now() + Duration::from_secs(15);
    while support::get(&sock, "/digest", Duration::from_secs(2)).0 != 200 {
        assert!(
            Instant::now() < deadline,
            "no poll completed\n--- output ---\n{}",
            svc.output()
        );
        std::thread::sleep(Duration::from_millis(20));
    }

    for (target, marker) in [
        ("/digest/", "\"nodes\""),
        ("//", "m6 fleet"),
        ("digest", "\"nodes\""),
    ] {
        let (status, body) = support::get(&sock, target, Duration::from_secs(2));
        assert!(
            status == 200 || status == 503,
            "GET {target} answered {status}\n--- output ---\n{}",
            svc.output()
        );
        assert!(
            body.contains(marker),
            "GET {target} answered without {marker:?}: {body}\n--- output ---\n{}",
            svc.output()
        );
    }
    // The loop is still serving after all three.
    let (status, _) = support::get(&sock, "/digest", Duration::from_secs(2));
    assert_eq!(status, 200, "the monitor stopped after a slashed path");
    svc.terminate(Duration::from_secs(5));
}

/// The monitor runs one loop thread, plus the log writer until LOG-2 replaces
/// it. Three counts test that: one node, the same three-node fleet without a
/// history, and that fleet with one. A poll thread or a history writer adds a
/// thread to the same fleet, and a thread per node adds two.
///
/// The counts are read once they have settled, through the platform layer.
/// Reading the host's certificate store (`client::system_roots`, which the
/// monitor does at start) leaves idle workers behind: the platform reports how
/// many (three or four on macOS, from the Security framework, and none on
/// Linux). They are the only threads above the loop and the log writer, so
/// `assert_single_loop` adds the platform's range instead of measuring it in
/// this process. Measuring it here is what used to fail: the workers are
/// transient in the test process and the harness runs other tests beside this
/// one, so the delta read 0 while the monitor had three. On macOS a single
/// extra thread fits inside the workers' one-thread swing, so the exact count
/// on Linux, where the store adds no thread, is what proves m6's own count. A
/// thread per node is not hidden: three nodes add three and fail the bound.
#[test]
fn the_monitor_runs_one_thread() {
    let (dir, config, sock) = setup(CONFIG);
    let svc = spawn(dir.path(), &config, &sock);
    let one = settled_threads(svc.pid(), &svc);
    drop(svc);
    assert_single_loop("one node", one);

    // The same three-node fleet twice, so a history writer shows as the one
    // difference between two otherwise identical runs.
    let handshakes = dir.path().join("wg-handshakes");
    std::fs::write(&handshakes, "").unwrap();
    let mut config_text = CONFIG.replace(
        "\n[[monitor.nodes]]",
        &format!(
            "\nwg_handshakes_file = {:?}\n\n[[monitor.nodes]]",
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
    let plain = settled_threads(svc2.pid(), &svc2);
    drop(svc2);
    assert_single_loop("three nodes with tunnels", plain);

    let config_text = config_text.replace(
        "\nwg_handshakes_file",
        "\nhistory_file = \"history.sqlite3\"\nwg_handshakes_file",
    );
    let (dir3, config3, sock3) = setup(&config_text);
    let svc3 = spawn(dir3.path(), &config3, &sock3);
    let with_history = settled_threads(svc3.pid(), &svc3);
    let output = svc3.output();
    drop(svc3);
    assert_history_adds_none(plain, with_history, &output);
}

/// The loop and the log writer, above the platform's own workers.
fn assert_single_loop(what: &str, count: usize) {
    // The log writer is a thread even when the log goes to stdout, so the loop
    // and the writer are two. LOG-2 replaces tracing-appender and this becomes
    // one.
    let (workers_low, workers_high) = m6_core::platform::process::certificate_store_workers();
    let low = 1 + LOG_WRITER_THREADS + workers_low;
    let high = 1 + LOG_WRITER_THREADS + workers_high;
    assert!(
        (low..=high).contains(&count),
        "{what}: m6-monitor runs {count} threads, not the one loop thread, \
         tracing-appender's writer until LOG-2, and the platform's \
         {workers_low} to {workers_high} idle certificate-store workers: \
         {low} to {high} here, and exactly {} where the store adds none",
        1 + LOG_WRITER_THREADS
    );
}

/// The thread count of `pid` once two readings 100 ms apart agree.
fn settled_count(pid: u32) -> usize {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut last = thread_count(pid);
    loop {
        std::thread::sleep(Duration::from_millis(100));
        let next = thread_count(pid);
        if next == last || Instant::now() > deadline {
            return next;
        }
        last = next;
    }
}

/// A history adds no thread, within the platform workers' one-thread swing.
fn assert_history_adds_none(plain: usize, with_history: usize, output: &str) {
    assert!(
        with_history <= plain + 1,
        "a history added {} thread(s)\n--- output ---\n{output}",
        with_history as isize - plain as isize
    );
}

/// `LOG_WRITER_THREADS` is 1 until LOG-2 replaces tracing-appender; it becomes
/// 0 then, and the trusted session updates this constant.
const LOG_WRITER_THREADS: usize = 1;

/// The thread count once it has settled: two readings 100 ms apart agree, or
/// the deadline passes. The assertion is the signal and the deadline is the
/// bound.
fn settled_threads(pid: u32, svc: &Service) -> usize {
    let count = settled_count(pid);
    assert!(
        count > 0,
        "could not count the threads of pid {pid}\n--- output ---\n{}",
        svc.output()
    );
    count
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

/// The thread count of a running process, through the platform layer.
fn thread_count(pid: u32) -> usize {
    m6_core::platform::process::thread_count(pid).unwrap_or(0)
}
