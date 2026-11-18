//! The build identity on the first metrics tick is fixed at compile time.
//!
//! The identity used to be the md5 of the bytes the process started from, read
//! once at start. This test still proves the placement without a timer. It runs
//! its own copy of `m6-http`, removes that copy once the listener is up, then
//! waits for the first metrics line. The line must carry the full SemVer with
//! the commit in its build metadata. The executable those bytes would have come
//! from is gone, so this pins the identity to compile-time constants.

#![allow(
    clippy::disallowed_methods,
    reason = "test code: threads and sleeps drive the service from outside"
)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use m6_core::testkit::{binary, claim_port, Service};

fn keypair() -> (String, String) {
    let ck =
        rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).expect("certificate");
    (ck.cert.pem(), ck.key_pair.serialize_pem())
}

fn site_toml() -> String {
    format!(
        "[site]\nname = \"first-tick\"\ndomain = \"localhost\"\n\n\
         [errors]\nmode = \"internal\"\n\n\
         [analytics]\nenabled = false\n\n\
         [log]\nlevel = \"info\"\n\n\
         [rate_limit]\nenabled = false\n\n\
         [server]\nbackend_timeout_secs = 5\n\n\
         [stats]\ninterval_s = {}\n\n\
         [[backend]]\nname = \"origin\"\nurl = \"http://127.0.0.1:1\"\n\n\
         [[route]]\npath = \"/\"\nbackend = \"origin\"\n",
        FIRST_TICK_SECS
    )
}

fn system_toml(dir: &Path, port: u16) -> String {
    format!(
        "[server]\nbind = \"127.0.0.1:{port}\"\ntls_cert = \"{}/cert.pem\"\ntls_key = \"{}/key.pem\"\n\n\
         [node]\nname = \"first-tick\"\n",
        dir.display(),
        dir.display()
    )
}

/// Ten times the longest start-up this suite expects under burners, so the
/// first tick cannot fire before the executable is removed.
const FIRST_TICK_SECS: u64 = 10;

/// The first metrics line's `build` object, once one exists.
fn first_perf_build(output: &str) -> Option<serde_json::Value> {
    for line in output.lines() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if v["type"] == "perf" {
            return Some(v["fields"]["perf"]["build"].clone());
        }
    }
    None
}

fn perf_lines(output: &str) -> usize {
    output.matches("\"type\":\"perf\"").count()
}

#[test]
fn the_first_tick_reads_no_executable() {
    let dir = tempfile::tempdir().expect("tempdir");
    let d = dir.path();
    let (cert_pem, key_pem) = keypair();
    std::fs::write(d.join("cert.pem"), cert_pem).expect("cert");
    std::fs::write(d.join("key.pem"), key_pem).expect("key");
    std::fs::write(d.join("site.toml"), site_toml()).expect("site.toml");

    let claim = claim_port();
    let port = claim.port();
    let system: PathBuf = d.join("system.toml");
    std::fs::write(&system, system_toml(d, port)).expect("system.toml");

    // A private copy, because this test removes the executable while the
    // process is running and the workspace binary belongs to every other test.
    let copy = d.join("m6-http");
    std::fs::copy(binary("m6-http"), &copy).expect("copy the binary");
    let mut service = Service::spawn(
        "m6-http-first-tick",
        Command::new(&copy)
            .arg(d)
            .arg(&system)
            .env("RUST_LOG", "info")
            .stdin(std::process::Stdio::null()),
    );
    service.wait_for_tcp(port, m6_core::testkit::STARTUP_BOUND);

    // The build identity is primed with the other start-time work, before any
    // listener binds, so a port that answers means it has been computed.
    std::fs::remove_file(&copy).expect("remove the executable before the first tick");
    assert!(!copy.exists(), "the executable must be gone");

    // A line written before the removal would make this test prove nothing:
    // the identity could have been read from the file then and reported later.
    // Under the ten-second interval the first tick cannot have come yet, and if
    // start-up ever takes that long this assertion says so instead of passing
    // vacuously.
    assert_eq!(
        perf_lines(&service.output()),
        0,
        "a metrics line was written before the executable was removed\n{}",
        service.output()
    );

    let deadline = Instant::now() + m6_core::testkit::STARTUP_BOUND;
    let mut build = None;
    while Instant::now() < deadline {
        service.assert_alive("waiting for the first metrics line");
        build = first_perf_build(&service.output());
        if build.is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    let build = build.unwrap_or_else(|| {
        panic!(
            "the first metrics line never appeared\n--- output ---\n{}",
            service.output()
        )
    });
    let version = build["version"].as_str().unwrap_or_else(|| {
        panic!(
            "the first metrics line's build must carry a version string\n--- output ---\n{}",
            service.output()
        )
    });
    let (_, metadata) = version.split_once('+').unwrap_or_else(|| {
        panic!(
            "build version {version:?} must carry the commit in its build metadata\
             \n--- output ---\n{}",
            service.output()
        )
    });
    let commit = metadata.split('.').next().expect("metadata is not empty");
    assert!(
        commit == "unknown"
            || (commit.len() == 12 && commit.chars().all(|c| c.is_ascii_hexdigit())),
        "build metadata {metadata:?} must start with a twelve-character commit or `unknown`\
         \n--- output ---\n{}",
        service.output()
    );
    assert!(
        build.get("hash").is_none(),
        "the executable hash must not be on the wire\n--- output ---\n{}",
        service.output()
    );
}
