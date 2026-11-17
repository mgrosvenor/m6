//! `m6-monitor --check` reports each node's tunnel, through the real binary.

#![allow(
    clippy::disallowed_methods,
    reason = "test code: threads, sleeps and blocking clients drive the service from outside"
)]

use std::io::Write;
use std::net::TcpListener;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use m6_core::testkit::binary;

mod support;

const KEY_A: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";
const KEY_B: &str = "BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB=";
const KEY_C: &str = "CCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCC=";

fn run_check(config: &str) -> (String, String) {
    let mut cfg = tempfile::NamedTempFile::new().unwrap();
    cfg.write_all(config.as_bytes()).unwrap();
    let out = Command::new(binary("m6-monitor"))
        .arg("--check")
        .arg(cfg.path())
        .output()
        .unwrap();
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn check_reports_every_tunnel_peer_including_ones_with_no_handshake() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let open = listener.local_addr().unwrap();
    let closed = {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap()
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let mut hs = tempfile::NamedTempFile::new().unwrap();
    writeln!(hs, "{KEY_A}\t{}", now - 30).unwrap();
    writeln!(hs, "{KEY_B}\t0").unwrap();

    let config = format!(
        r#"
[monitor]
timeout_ms = 300
wg_handshakes_file = {:?}

[[monitor.nodes]]
name = "a"
url = "http://127.0.0.1:1"
tunnel = {{ address = "{open}", public_key = "{KEY_A}" }}

[[monitor.nodes]]
name = "b"
url = "http://127.0.0.1:1"
tunnel = {{ address = "{closed}", public_key = "{KEY_B}" }}

[[monitor.nodes]]
name = "c"
url = "http://127.0.0.1:1"
tunnel = {{ address = "{open}", public_key = "{KEY_C}" }}

[[monitor.nodes]]
name = "d"
url = "http://127.0.0.1:1"
"#,
        hs.path()
    );
    let (stdout, stderr) = run_check(&config);
    for want in [
        "H. TUNNELS",
        "connected",
        "handshake never",
        "peer not on this host's interface",
        "no tunnel configured",
    ] {
        assert!(stdout.contains(want), "missing {want:?}:\n{stdout}");
    }
    assert!(!stderr.contains("panicked"), "{stderr}");
}

#[test]
fn check_says_when_the_handshake_file_is_missing() {
    let config = format!(
        r#"
[monitor]
timeout_ms = 300
wg_handshakes_file = "/no/such/dir/wg-handshakes"

[[monitor.nodes]]
name = "a"
url = "http://127.0.0.1:1"
tunnel = {{ address = "127.0.0.1:1", public_key = "{KEY_A}" }}
"#
    );
    let (stdout, stderr) = run_check(&config);
    assert!(stdout.contains("file missing"), "{stdout}");
    assert!(
        stdout.contains("monitor: WireGuard handshake file"),
        "{stdout}"
    );
    assert!(!stderr.contains("panicked"), "{stderr}");
}

/// A tunnel probe that gets no answer must not hold the loop.
///
/// The node's URL points at a closed loopback port, so its `/health` fails at
/// once and the poll reaches the tunnel probe. The probe goes to
/// `192.0.2.1:9`, an address in the RFC 5737 documentation range: the SYN gets
/// no answer, so the connect is outstanding for its whole budget. `/digest`
/// must answer while it is outstanding, and after the budget the tunnel reads
/// as lost.
#[test]
fn a_tunnel_probe_does_not_hold_the_loop() {
    use std::time::{Duration, Instant};

    use m6_core::testkit::{wait, Service};

    let config = r#"
poll_interval_s = 60

[monitor]
timeout_ms = 3000

[[monitor.nodes]]
name = "edge-a"
url = "http://127.0.0.1:1"
role = "cache"
tunnel = { address = "192.0.2.1:9" }

[log]
level = "warn"
"#;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("monitor.conf");
    std::fs::write(&path, config).unwrap();
    std::fs::write(dir.path().join("site.toml"), "").unwrap();
    let sock = dir.path().join("mon.sock");
    let mut svc = Service::spawn(
        "m6-monitor",
        Command::new(binary("m6-monitor"))
            .arg(dir.path())
            .arg(&path)
            .env("M6_SOCKET_OVERRIDE", &sock),
    );
    svc.wait_for_path(&sock, m6_core::testkit::STARTUP_BOUND);
    // The socket file appears at bind, before listen. A connect in that
    // window fails with ConnectionRefused. Wait on a connect that succeeds,
    // under one bound, so the first request below is not the readiness probe.
    assert!(
        wait::for_unix(&sock, m6_core::testkit::STARTUP_BOUND),
        "m6-monitor created {} but never accepted a connection\n--- output ---\n{}",
        sock.display(),
        svc.output()
    );

    let started = Instant::now();
    let (status, body) = support::get(&sock, "/digest");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "/digest took {:?} while the tunnel probe waited",
        started.elapsed()
    );
    assert!(
        status == 200 || status == 503,
        "unexpected /digest status {status}: {body}"
    );

    // The budget passes and the poll completes with the tunnel lost.
    let deadline = Instant::now() + m6_core::testkit::STARTUP_BOUND;
    loop {
        let (status, body) = support::get(&sock, "/digest");
        if status == 200 && body.contains("no answer") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the tunnel never read as lost: {status} {body}\n--- output ---\n{}",
            svc.output()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    svc.terminate(m6_core::testkit::STARTUP_BOUND);
}
