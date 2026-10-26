//! `[server] internal_binds` as a process: what m6-http does at startup when
//! the declaration is missing, public, or fine.
//!
//! The unit tests in `config.rs` cover the rules through `load`. These cover
//! the part a unit test cannot: the real binary refuses with exit code 2 and
//! a message that names the key, and a valid declaration starts the listeners
//! and says so in the startup log.

use std::process::{Child, Command, Stdio};
use std::time::Duration;

use m6_core::testkit;

/// A site with one backend and one route, written into `dir`.
fn write_site(dir: &std::path::Path) -> std::path::PathBuf {
    let site = dir.join("site");
    std::fs::create_dir_all(&site).expect("site dir");
    std::fs::write(
        site.join("site.toml"),
        "[site]\nname = \"internal-binds\"\ndomain = \"localhost\"\n\n\
         [[backend]]\nname = \"b\"\nsockets = \"/nonexistent/b-*.sock\"\n\n\
         [[route]]\npath = \"/\"\nbackend = \"b\"\n",
    )
    .expect("site.toml");
    site
}

/// Write `[server]` with the given extra lines and the certificate files.
fn write_system(
    dir: &std::path::Path,
    bind: &str,
    extra: &str,
    cert: &str,
    key: &str,
) -> std::path::PathBuf {
    std::fs::write(dir.join("cert.pem"), cert).expect("cert");
    std::fs::write(dir.join("key.pem"), key).expect("key");
    let config = dir.join("system.toml");
    std::fs::write(
        &config,
        format!(
            "[server]\nbind = \"{bind}\"\ntls_cert = \"{}\"\ntls_key = \"{}\"\n{extra}\n\
             [node]\nname = \"internal-binds\"\n",
            dir.join("cert.pem").display(),
            dir.join("key.pem").display(),
        ),
    )
    .expect("system.toml");
    config
}

/// Run m6-http until it exits and return (exit code, stdout plus stderr).
/// Fails the test, after killing the process, if it is still running after
/// ten seconds: a refusal must be immediate.
fn run_until_exit(site: &std::path::Path, config: &std::path::Path) -> (Option<i32>, String) {
    let mut child: Child = Command::new(testkit::binary("m6-http"))
        .arg(site)
        .arg(config)
        .env("RUST_LOG", "info")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn m6-http");
    let exited = testkit::wait::until(Duration::from_secs(10), || {
        child.try_wait().expect("try_wait").is_some()
    });
    if !exited {
        let _ = child.kill();
        let _ = child.wait();
        panic!("m6-http was still running after 10s; it should have refused to start");
    }
    let out = child.wait_with_output().expect("output");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.code(), text)
}

#[test]
fn an_undeclared_h2c_listener_refuses_to_start() {
    let tls = testkit::claim_port();
    let h2c = testkit::claim_port();
    let dir = tempfile::tempdir().expect("tempdir");
    let site = write_site(dir.path());
    // Empty certificate files: the refusal fires before TLS is read.
    let config = write_system(
        dir.path(),
        &tls.addr(),
        &format!("h2c_bind = \"{}\"\n", h2c.addr()),
        "",
        "",
    );

    let (code, output) = run_until_exit(&site, &config);

    assert_eq!(code, Some(2), "output:\n{output}");
    assert!(output.contains("internal_binds"), "output:\n{output}");
}

#[test]
fn a_public_internal_binding_refuses_to_start() {
    let tls = testkit::claim_port();
    let h2c = testkit::claim_port();
    let dir = tempfile::tempdir().expect("tempdir");
    let site = write_site(dir.path());
    let public = format!("0.0.0.0:{}", h2c.port());
    let config = write_system(
        dir.path(),
        &tls.addr(),
        &format!("h2c_bind = \"{public}\"\ninternal_binds = [\"{public}\"]\n"),
        "",
        "",
    );

    let (code, output) = run_until_exit(&site, &config);

    assert_eq!(code, Some(2), "output:\n{output}");
    assert!(output.contains("not in a local range"), "output:\n{output}");
}

#[test]
fn a_declared_h2c_listener_starts() {
    let tls = testkit::claim_port();
    let h2c = testkit::claim_port();
    let dir = tempfile::tempdir().expect("tempdir");
    let site = write_site(dir.path());
    let ck = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).expect("rcgen");
    let config = write_system(
        dir.path(),
        &tls.addr(),
        &format!(
            "h2c_bind = \"{0}\"\ninternal_binds = [\"{0}\"]\n",
            h2c.addr()
        ),
        &ck.cert.pem(),
        &ck.key_pair.serialize_pem(),
    );

    let mut cmd = Command::new(testkit::binary("m6-http"));
    cmd.arg(&site).arg(&config).env("RUST_LOG", "info");
    let mut svc = testkit::Service::spawn("m6-http-internal-binds", &mut cmd);
    svc.wait_for_tcp(tls.port(), Duration::from_secs(10));
    svc.wait_for_tcp(h2c.port(), Duration::from_secs(10));

    let status = svc.terminate(Duration::from_secs(5));
    let output = svc.output();
    assert!(status.success(), "exit {status}\n{output}");
    assert!(output.contains("m6-http listeners bound"), "{output}");
    assert!(output.contains("internal_binds"), "{output}");
}
