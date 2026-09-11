//! The `:80` redirect listener is a service like any other, and must start,
//! answer, and stop like one.
//!
//! It runs as its own process on every production node, and it took a
//! different path through `main` from the `:443` instance: it returned before
//! the call that installs the shutdown thread. `main` blocks SIGTERM and
//! SIGINT as its first statement so that only core's `sigwait` thread sees
//! them, so returning early left the signals blocked with nobody waiting on
//! them. **The process could not be stopped by anything short of SIGKILL.**
//!
//! Nothing caught it because nothing exercised redirect mode as a process; the
//! unit tests call `redirect_for` directly, and the conformance runner started
//! it and left it running. It surfaced as a conformance run that could not
//! reclaim its own port, two runs after the process it was complaining about
//! had been sent SIGTERM twice.
//!
//! That is the same class of quiet failure as the one
//! `sigterm_shuts_down_rather_than_killing` guards for the main instance:
//! systemd counts death by the signal it sent as a clean stop, so nothing
//! upstream reports anything. Here it was worse, because the signal did not
//! even kill it.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::process::Command;
use std::time::Duration;

use m6_core::testkit;

/// Start m6-http in redirect mode on a claimed port, with a site and config
/// written into a temporary directory.
fn start() -> (testkit::Service, testkit::PortClaim, tempfile::TempDir) {
    let claim = testkit::claim_port();
    let dir = tempfile::tempdir().expect("tempdir");
    let site = dir.path().join("site");
    std::fs::create_dir_all(&site).expect("site dir");
    std::fs::write(
        site.join("site.toml"),
        "[site]\nname = \"redirect-lifecycle\"\ndomain = \"localhost\"\n",
    )
    .expect("site.toml");

    let config = dir.path().join("redirect.toml");
    std::fs::write(
        &config,
        format!(
            // `bind` is required but never listened on in redirect mode; port 1
            // is unbindable, which is the point: if this process ever stopped
            // returning early it would fail loudly here instead of quietly
            // serving.
            "[server]\nbind = \"127.0.0.1:1\"\nredirect_bind = \"127.0.0.1:{}\"\n\n\
             [node]\nname = \"redirect-lifecycle\"\n",
            claim.port()
        ),
    )
    .expect("redirect.toml");

    let mut cmd = Command::new(testkit::binary("m6-http"));
    cmd.arg(&site).arg(&config).env("RUST_LOG", "info");
    let mut svc = testkit::Service::spawn("m6-http-redirect", &mut cmd);
    svc.wait_for_tcp(claim.port(), Duration::from_secs(10));
    (svc, claim, dir)
}

/// SIGTERM must stop it, and it must say so.
///
/// The assertion that matters is `terminate` returning at all: before the fix
/// it burned its whole timeout and then had to SIGKILL.
#[test]
fn sigterm_shuts_down_rather_than_being_ignored() {
    let (mut svc, _claim, _dir) = start();

    let status = svc.terminate(Duration::from_secs(5));
    assert!(
        status.success(),
        "redirect mode should exit 0 on SIGTERM, got {status}. A signal exit status \
         means it died at the default disposition; no exit at all means SIGTERM was \
         blocked with nothing waiting on it, which is the bug this guards.\n\
         --- output ---\n{}",
        svc.output()
    );
    testkit::assert_lifecycle_logged("m6-http-redirect", &svc.output());
}

/// And it still redirects, so the test above is not passing on a process that
/// never worked.
#[test]
fn it_answers_a_redirect_before_it_is_stopped() {
    let (mut svc, claim, _dir) = start();

    let mut s = TcpStream::connect(("127.0.0.1", claim.port())).expect("connect");
    s.set_read_timeout(Some(Duration::from_secs(5))).expect("timeout");
    s.write_all(b"GET /capabilities HTTP/1.1\r\nHost: mgrosvenor.com\r\n\r\n")
        .expect("write");
    // One response, not read-to-EOF: HTTP/1.1 is persistent by default (RFC
    // 9112 9.3) and this listener honours that, so the socket stays open.
    let mut resp = Vec::new();
    let mut byte = [0u8; 1];
    while !resp.ends_with(b"\r\n\r\n") {
        match s.read(&mut byte) {
            Ok(0) => break,
            Ok(_) => resp.push(byte[0]),
            Err(e) => panic!("read: {e}"),
        }
    }
    let resp = String::from_utf8_lossy(&resp).to_string();

    assert!(resp.starts_with("HTTP/1.1 301 "), "got: {resp}");
    assert!(
        resp.to_ascii_lowercase().contains("location: https://mgrosvenor.com/capabilities"),
        "got: {resp}"
    );

    svc.terminate(Duration::from_secs(5));
}
