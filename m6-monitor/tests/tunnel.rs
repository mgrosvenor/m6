//! `m6-monitor --check` reports each node's tunnel, through the real binary.

use std::io::Write;
use std::net::TcpListener;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use m6_core::testkit::binary;

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
