//! Two m6-http instances on one host are two nodes.
//!
//! The node a record states is the app's configured `[node] name` when it has
//! one, and the platform hostname only as a fallback. Two instances that share
//! a host therefore share a hostname and must still be told apart, which is the
//! case a hostname alone merges. The test runs the real binary twice, with two
//! system configs that name the node differently, and reads the records the two
//! processes wrote.

#![allow(
    clippy::disallowed_methods,
    reason = "test code: it runs two child processes and their probe listeners block"
)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use m6_core::testkit::{binary, claim_port, CheckServer, PortClaim, Service};

const STARTED: Duration = Duration::from_secs(20);

struct Configs {
    site: PathBuf,
    system: PathBuf,
    check: PathBuf,
    _claim: PortClaim,
}

/// One site and one system config, naming `node` and its own check socket.
fn write_configs(dir: &Path, node: &str) -> Configs {
    let site = dir.join("site");
    std::fs::create_dir_all(&site).expect("site dir");
    let cert =
        rcgen::generate_simple_self_signed(vec!["localhost".to_string(), "127.0.0.1".to_string()])
            .expect("self-signed certificate");
    std::fs::write(site.join("cert.pem"), cert.cert.pem()).expect("cert.pem");
    std::fs::write(site.join("key.pem"), cert.key_pair.serialize_pem()).expect("key.pem");

    let claim = claim_port();
    let check = dir.join("check.sock");
    let site_toml = format!(
        "[site]\nname = \"startup-node\"\ndomain = \"example.com\"\n\n\
         [log]\nlevel = \"info\"\n\n[analytics]\nenabled = false\n\n\
         [[backend]]\nname = \"auth\"\nsockets = {check:?}\n",
        check = check.display().to_string(),
    );
    let system = dir.join("system.toml");
    let system_toml = format!(
        "[node]\nname = {node:?}\n\n[server]\nbind = \"127.0.0.1:{port}\"\n\
         tls_cert = \"{cert}\"\ntls_key = \"{key}\"\n\n[auth]\nbackend = \"auth\"\n",
        port = claim.port(),
        cert = site.join("cert.pem").display(),
        key = site.join("key.pem").display(),
    );
    std::fs::write(site.join("site.toml"), site_toml).expect("site.toml");
    std::fs::write(&system, system_toml).expect("system.toml");

    Configs {
        site,
        system,
        check,
        _claim: claim,
    }
}

fn spawn(configs: &Configs) -> Service {
    let mut cmd = Command::new(binary("m6-http"));
    cmd.arg(&configs.site).arg(&configs.system);
    Service::spawn("m6-http", &mut cmd)
}

/// The value of the first `"key":{"known":"..."` in `text`.
fn known_of<'a>(text: &'a str, key: &str) -> &'a str {
    let marker = format!("\"{key}\":{{\"known\":\"");
    let start = text
        .find(&marker)
        .unwrap_or_else(|| panic!("no {key} fact in the record\n--- output ---\n{text}"))
        + marker.len();
    let rest = &text[start..];
    let end = rest.find('"').expect("the fact's value ends");
    &rest[..end]
}

#[test]
fn two_instances_on_one_host_report_two_configured_nodes() {
    let root = tempfile::Builder::new()
        .prefix("m6-")
        .tempdir_in(m6_core::testkit::socket_scratch_root())
        .expect("tempdir");

    let alice = write_configs(&root.path().join("alice"), "edge-a");
    let bob = write_configs(&root.path().join("bob"), "edge-b");
    // One host serves both, so both children see the same hostname.
    let mut check_a = CheckServer::bind(&alice.check).expect("check server a");
    let mut check_b = CheckServer::bind(&bob.check).expect("check server b");

    let mut svc_a = spawn(&alice);
    let mut svc_b = spawn(&bob);
    check_a
        .serve_one()
        .unwrap_or_else(|e| panic!("serving a's probe: {e}\n{}", svc_a.output()));
    check_b
        .serve_one()
        .unwrap_or_else(|e| panic!("serving b's probe: {e}\n{}", svc_b.output()));
    svc_a.wait_for_started(STARTED);
    svc_b.wait_for_started(STARTED);

    let (out_a, out_b) = (svc_a.output(), svc_b.output());
    for (out, mine, other) in [(&out_a, "edge-a", "edge-b"), (&out_b, "edge-b", "edge-a")] {
        assert!(
            out.contains(&format!("\"node\":\"{mine}\"")),
            "the record states its configured node {mine}\n--- output ---\n{out}"
        );
        assert!(
            out.contains("\"node_source\":\"configured\""),
            "the node's source says the application named it\n--- output ---\n{out}"
        );
        assert!(
            !out.contains(&format!("\"node\":\"{other}\"")),
            "the other instance on this host must not appear as this one's node\n--- output ---\n{out}"
        );
    }
    assert_eq!(
        known_of(&out_a, "hostname"),
        known_of(&out_b, "hostname"),
        "both instances run on the one host, so the fact they share is the hostname"
    );

    let status_a = svc_a.terminate(m6_core::testkit::STARTUP_BOUND);
    let status_b = svc_b.terminate(m6_core::testkit::STARTUP_BOUND);
    assert_eq!(status_a.code(), Some(0), "a's record:\n{out_a}");
    assert_eq!(status_b.code(), Some(0), "b's record:\n{out_b}");
}
