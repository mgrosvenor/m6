//! The `[auth] backend` start probe, exercised by running the real binary.
//!
//! This is the step where a wrong probe becomes a start-time outage, so each
//! test pins one outcome: 401 and 503 pass, 404 stops the start naming the
//! setting, a connect failure warns and the node still starts, and a member
//! that answers anything else is not a checker.
//!
//! The check backend is `m6_core::testkit::CheckServer`. The service is a child
//! process, so the test serves one probe connection with `serve_one` while the
//! child starts.

#![allow(
    clippy::disallowed_methods,
    reason = "test code: the dribbling peer sleeps between bytes and its listener blocks, while a child process is driven from outside"
)]

use std::io::{Read, Write};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use m6_core::testkit::{binary, claim_port, CheckServer, PortClaim, Reply, Service};

const STARTED: Duration = Duration::from_secs(20);

struct Configs {
    site: PathBuf,
    system: PathBuf,
    _claim: PortClaim,
}

/// Write a site and a system config. `check_glob` is the `[auth] backend`'s
/// socket glob, or `None` for the config that carries only `public_key`.
fn write_configs(dir: &Path, check_glob: Option<&str>) -> Configs {
    let site = dir.join("site");
    std::fs::create_dir_all(&site).expect("site dir");

    let cert =
        rcgen::generate_simple_self_signed(vec!["localhost".to_string(), "127.0.0.1".to_string()])
            .expect("self-signed certificate");
    std::fs::write(site.join("cert.pem"), cert.cert.pem()).expect("cert.pem");
    std::fs::write(site.join("key.pem"), cert.key_pair.serialize_pem()).expect("key.pem");
    let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).expect("ec keypair");
    std::fs::write(site.join("auth-public.pem"), key.public_key_pem()).expect("auth public key");

    let claim = claim_port();
    let backend = match check_glob {
        Some(glob) => format!("[[backend]]\nname = \"auth\"\nsockets = {glob:?}\n"),
        None => String::new(),
    };
    let auth_backend = match check_glob {
        Some(_) => "backend = \"auth\"\n",
        None => "",
    };

    let site_toml = format!(
        "[site]\nname = \"auth-startup\"\ndomain = \"example.com\"\n\n[log]\nlevel = \"info\"\n\n[analytics]\nenabled = false\n\n{backend}"
    );
    let system = dir.join("system.toml");
    let system_toml = format!(
        "[node]\nname = \"test-node\"\n\n[server]\nbind = \"127.0.0.1:{port}\"\ntls_cert = \"{cert}\"\ntls_key = \"{key}\"\n\n[auth]\n{auth_backend}public_key = \"{pubkey}\"\n",
        port = claim.port(),
        cert = site.join("cert.pem").display(),
        key = site.join("key.pem").display(),
        pubkey = site.join("auth-public.pem").display(),
    );
    std::fs::write(site.join("site.toml"), site_toml).expect("site.toml");
    std::fs::write(&system, system_toml).expect("system.toml");

    Configs {
        site,
        system,
        _claim: claim,
    }
}

fn spawn(configs: &Configs) -> Service {
    let mut cmd = Command::new(binary("m6-http"));
    cmd.arg(&configs.site).arg(&configs.system);
    Service::spawn("m6-http", &mut cmd)
}

fn stop(mut svc: Service) -> std::process::ExitStatus {
    svc.terminate(Duration::from_secs(10))
}

fn serve(server: &mut CheckServer, svc: &Service) {
    if let Err(e) = server.serve_one() {
        panic!(
            "serving the probe failed: {e}\n--- m6-http output ---\n{}",
            svc.output()
        );
    }
}

fn check_socket(dir: &Path) -> PathBuf {
    dir.join("check.sock")
}

/// A 401 proves the socket is a checker, so the node starts.
#[test]
fn a_401_from_the_check_backend_starts() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = check_socket(dir.path());
    let mut server = CheckServer::bind(&path).expect("check server");
    let configs = write_configs(dir.path(), Some(path.to_str().expect("utf-8 socket path")));

    let mut svc = spawn(&configs);
    serve(&mut server, &svc);
    svc.wait_for_started(STARTED);
    let status = stop(svc);
    assert_eq!(status.code(), Some(0), "the node did not stop cleanly");
}

/// A 503 is a checker that is there and busy, so the node starts.
#[test]
fn a_503_from_the_check_backend_starts() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = check_socket(dir.path());
    let mut server = CheckServer::bind(&path).expect("check server");
    server.unknown_answer(Reply::Unavailable);
    let configs = write_configs(dir.path(), Some(path.to_str().expect("utf-8 socket path")));

    let mut svc = spawn(&configs);
    serve(&mut server, &svc);
    svc.wait_for_started(STARTED);
    let status = stop(svc);
    assert_eq!(status.code(), Some(0), "the node did not stop cleanly");
}

/// A 404 means the thing behind the address is not an auth server. Every
/// protected request would be refused, so the start stops and names the
/// setting, the backend and the socket.
#[test]
fn a_404_from_the_check_backend_exits_2_naming_the_setting() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = check_socket(dir.path());
    let mut server = CheckServer::bind(&path).expect("check server");
    server.unknown_answer(Reply::Status(404));
    let configs = write_configs(dir.path(), Some(path.to_str().expect("utf-8 socket path")));

    let mut svc = spawn(&configs);
    serve(&mut server, &svc);
    let status = svc
        .wait_for_exit(std::time::Instant::now() + STARTED)
        .unwrap_or_else(|| panic!("m6-http did not exit\n--- output ---\n{}", svc.output()));
    let output = svc.output();
    assert_eq!(status.code(), Some(2), "output:\n{output}");
    assert!(output.contains("[auth] backend"), "output:\n{output}");
    assert!(output.contains("\"auth\""), "output:\n{output}");
    assert!(
        output.contains(&path.display().to_string()),
        "the error must name the socket, output:\n{output}"
    );
    assert!(output.contains("404"), "output:\n{output}");
}

/// A member that answers something other than 401, 503 or 404 is not a
/// checker, and no member answering 401 or 503 stops the start.
#[test]
fn a_status_that_is_not_a_check_exits_2() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = check_socket(dir.path());
    let mut server = CheckServer::bind(&path).expect("check server");
    server.unknown_answer(Reply::Status(200));
    let configs = write_configs(dir.path(), Some(path.to_str().expect("utf-8 socket path")));

    let mut svc = spawn(&configs);
    serve(&mut server, &svc);
    let status = svc
        .wait_for_exit(std::time::Instant::now() + STARTED)
        .unwrap_or_else(|| panic!("m6-http did not exit\n--- output ---\n{}", svc.output()));
    let output = svc.output();
    assert_eq!(status.code(), Some(2), "output:\n{output}");
    assert!(
        output.contains("no member answered 401 or 503"),
        "output:\n{output}"
    );
}

/// A checker that is being replaced refuses the connection. The node starts,
/// with a warning naming the socket, because public traffic must not wait for
/// a restarted checker.
#[test]
fn a_connect_failure_warns_and_the_node_starts() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = check_socket(dir.path());
    // Bind, then close: the socket file stays and nothing listens on it.
    let mut dead = CheckServer::bind(&path).expect("check server");
    dead.close();
    let configs = write_configs(dir.path(), Some(path.to_str().expect("utf-8 socket path")));

    let mut svc = spawn(&configs);
    svc.wait_for_started(STARTED);
    let output = svc.output();
    assert!(
        output.contains("not reachable at start"),
        "output:\n{output}"
    );
    assert!(
        output.contains(&path.display().to_string()),
        "output:\n{output}"
    );
    let status = stop(svc);
    assert_eq!(status.code(), Some(0), "the node did not stop cleanly");
}

/// One down member warns and the live member still proves a checker, so the
/// node starts.
#[test]
fn one_member_down_still_starts() {
    let dir = tempfile::tempdir().expect("tempdir");
    let live = dir.path().join("check-live.sock");
    let dead_path = dir.path().join("check-dead.sock");
    let mut live_server = CheckServer::bind(&live).expect("live check server");
    let mut dead = CheckServer::bind(&dead_path).expect("dead check server");
    dead.close();

    let glob = format!("{}", dir.path().join("check-*.sock").display());
    let configs = write_configs(dir.path(), Some(glob.as_str()));
    let mut svc = spawn(&configs);
    serve(&mut live_server, &svc);
    svc.wait_for_started(STARTED);
    let output = svc.output();
    assert!(
        output.contains(&dead_path.display().to_string()),
        "the warning must name the dead socket, output:\n{output}"
    );
    let status = stop(svc);
    assert_eq!(status.code(), Some(0), "the node did not stop cleanly");
}

/// `[auth]` without `backend` carries only `public_key`, and no probe runs.
#[test]
fn no_backend_needs_no_probe() {
    let dir = tempfile::tempdir().expect("tempdir");
    let configs = write_configs(dir.path(), None);
    let mut svc = spawn(&configs);
    svc.wait_for_started(STARTED);
    let status = stop(svc);
    assert_eq!(status.code(), Some(0), "the node did not stop cleanly");
}

/// A 403 to the no-credential probe is a service that is not a checker, so the
/// start stops and names it.
///
/// The probe passes on 401 and 503 because both prove the `/check` route is
/// there. A 403 is neither, and a catch-all that accepted any refusal would
/// call a service that refuses the probe a checker.
#[test]
fn a_403_from_the_check_backend_exits_2() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = check_socket(dir.path());
    let mut server = CheckServer::bind(&path).expect("check server");
    server.unknown_answer(Reply::Forbidden);
    let configs = write_configs(dir.path(), Some(path.to_str().expect("utf-8 socket path")));

    let mut svc = spawn(&configs);
    serve(&mut server, &svc);
    let status = svc
        .wait_for_exit(std::time::Instant::now() + STARTED)
        .unwrap_or_else(|| panic!("m6-http did not exit\n--- output ---\n{}", svc.output()));
    let output = svc.output();
    assert_eq!(status.code(), Some(2), "output:\n{output}");
    assert!(
        output.contains("no member answered 401 or 503"),
        "output:\n{output}"
    );
    assert!(output.contains("403"), "output:\n{output}");
}

/// The time of day, in seconds, of the first log line carrying `needle`.
fn seconds_of_day(output: &str, needle: &str) -> f64 {
    let line = output
        .lines()
        .find(|line| line.contains(needle))
        .unwrap_or_else(|| panic!("no line carrying {needle:?}\n--- output ---\n{output}"));
    let stamp = line
        .split("\"timestamp\":\"")
        .nth(1)
        .and_then(|rest| rest.split('"').next())
        .unwrap_or_else(|| panic!("no timestamp on {line}"));
    let time = stamp
        .split('T')
        .nth(1)
        .unwrap_or_else(|| panic!("no time in {stamp}"))
        .trim_end_matches('Z');
    let mut parts = time.split(':');
    let hours: f64 = parts.next().unwrap_or("0").parse().unwrap_or(0.0);
    let minutes: f64 = parts.next().unwrap_or("0").parse().unwrap_or(0.0);
    let seconds: f64 = parts.next().unwrap_or("0").parse().unwrap_or(0.0);
    hours * 3600.0 + minutes * 60.0 + seconds
}

/// A peer that answers one byte every 100 ms and never completes the head.
fn dribbling_peer(path: &Path) -> std::thread::JoinHandle<()> {
    let listener = UnixListener::bind(path).expect("bind the dribbling peer");
    std::thread::spawn(move || {
        let Ok((mut stream, _)) = listener.accept() else {
            return;
        };
        let mut buf = [0u8; 1024];
        let _ = stream.read(&mut buf);
        if stream.write_all(b"HTTP/1.1 200 OK\r\nX-Pad: ").is_err() {
            return;
        }
        // One byte every 100 ms, never completing the head. The read that
        // returns when the edge closes is what ends this loop.
        for _ in 0..200 {
            if stream.write_all(b"a").is_err() {
                return;
            }
            let _ = stream.flush();
            std::thread::sleep(Duration::from_millis(100));
        }
    })
}

/// One byte every 100 ms must not hold the start.
///
/// The wrong outcome this catches: `AUTH_PROBE_TIMEOUT` as a per-read socket
/// timeout. Before, this peer held the start for 8192 bytes at 100 ms each,
/// which is why the deadline is now one wall clock across the write and every
/// read. The probe cannot complete an answer, so it times out and takes the
/// warn-and-start path: a checker mid-restart is transient, and the node still
/// serves public and cached traffic.
#[test]
fn a_dribbling_peer_does_not_extend_the_probe_deadline() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = check_socket(dir.path());
    let dribbler = dribbling_peer(&path);
    let configs = write_configs(dir.path(), Some(path.to_str().expect("utf-8 socket path")));

    let mut svc = spawn(&configs);
    svc.wait_for_started(STARTED);
    let output = svc.output();
    assert!(
        output.contains("not reachable at start"),
        "the probe did not warn and start\n--- output ---\n{output}"
    );
    assert!(
        output.contains("deadline passed"),
        "the warning did not name the deadline\n--- output ---\n{output}"
    );
    // The service's own two timestamps, so a slow process start on a loaded
    // host is not counted as probe time. The pool line is written before the
    // probe, the started line after the listeners bind.
    let found = seconds_of_day(&output, "socket found");
    let started = seconds_of_day(&output, "m6-http started");
    let probe_s = started - found;
    assert!(
        probe_s < 4.0,
        "the probe took {probe_s:.3}s for a peer that answers one byte per 100 ms, with a 2 s deadline\n--- output ---\n{output}"
    );
    let status = stop(svc);
    assert_eq!(status.code(), Some(0), "the node did not stop cleanly");
    dribbler.join().expect("the dribbling peer");
}
