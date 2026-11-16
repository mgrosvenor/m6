//! `[server] internal_binds` as a process: what m6-http does at startup when
//! the declaration is missing, public, or fine.
//!
//! The unit tests in `config.rs` cover the rules through `load`. These cover
//! the part a unit test cannot: the real binary refuses with exit code 2 and
//! a message that names the key, and a declared h2c listener is really internal, shown by whether it
//! believes a forwarded client address.

#![allow(
    clippy::disallowed_methods,
    reason = "test code: threads, sleeps and blocking clients drive the service from outside"
)]

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use m6_http_lib::forward::HttpRequest;
use m6_http_lib::h2c_client::H2cClientConn;

use m6_core::testkit;

/// A site with one backend and one route, written into `dir`.
fn write_site(dir: &std::path::Path) -> std::path::PathBuf {
    let site = dir.join("site");
    std::fs::create_dir_all(&site).expect("site dir");
    // A socket file with no process listening: the pool's glob matches it, so
    // m6-http starts, and a connect is refused. The pool reads its glob once at
    // start, so the file must exist before the spawn.
    let sock = dir.join("b-1.sock");
    let glob = dir.join("b-*.sock");
    let _ = std::os::unix::net::UnixListener::bind(&sock).expect("placeholder socket");
    std::fs::write(
        site.join("site.toml"),
        format!(
            "[site]\nname = \"internal-binds\"\ndomain = \"localhost\"\n\n\
             [analytics]\nenabled = false\n\n\
             [[backend]]\nname = \"b\"\nsockets = \"{}\"\n\n\
             [[route]]\npath = \"/\"\nbackend = \"b\"\n\n\
             [rate_limit]\nrequests_per_min = 2\n",
            glob.display()
        ),
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
    let mut child = m6_core::testkit::process::Running::spawn(
        Command::new(testkit::binary("m6-http"))
            .arg(site)
            .arg(config)
            .env("RUST_LOG", "info")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped()),
    )
    .expect("spawn m6-http");
    let exited = testkit::wait::until(Duration::from_secs(10), || {
        child.child().try_wait().expect("try_wait").is_some()
    });
    if !exited {
        // The guard kills and reaps it while this panic unwinds.
        panic!("m6-http was still running after 10s; it should have refused to start");
    }
    let out = child.take().wait_with_output().expect("output");
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
    let dir = tempfile::tempdir_in(m6_core::testkit::socket_scratch_root()).expect("tempdir");
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
    let dir = tempfile::tempdir_in(m6_core::testkit::socket_scratch_root()).expect("tempdir");
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

/// Start m6-http with the given `[server]` lines for the h2c listener, send
/// `n` requests over h2c from this one peer, each asserting a different client
/// address in `X-Forwarded-For`, and return the status of each.
///
/// The site allows two requests per minute per client address. A listener
/// that believes the forwarded address gives each request its own address and
/// so its own allowance. A listener that does not believe it attributes all of
/// them to this peer and answers 429 from the third.
fn statuses_over_h2c(declare: bool, n: usize) -> Vec<u16> {
    let tls = testkit::claim_port();
    let h2c = testkit::claim_port();
    let dir = tempfile::tempdir_in(m6_core::testkit::socket_scratch_root()).expect("tempdir");
    let site = write_site(dir.path());
    let ck = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).expect("rcgen");
    let declaration = if declare {
        format!(
            "internal_binds = [\"{}\"]\ninternal_peers = [\"127.0.0.1\"]\n",
            h2c.addr()
        )
    } else {
        "internal_binds = []\n".to_string()
    };
    let config = write_system(
        dir.path(),
        &tls.addr(),
        &format!("h2c_bind = \"{}\"\n{declaration}", h2c.addr()),
        &ck.cert.pem(),
        &ck.key_pair.serialize_pem(),
    );

    let mut cmd = Command::new(testkit::binary("m6-http"));
    cmd.arg(&site).arg(&config).env("RUST_LOG", "info");
    let mut svc = testkit::Service::spawn("m6-http-internal-binds", &mut cmd);
    svc.wait_for_tcp(tls.port(), Duration::from_secs(10));
    svc.wait_for_tcp(h2c.port(), Duration::from_secs(10));

    let mut conn = H2cClientConn::connect("127.0.0.1", h2c.port()).expect("connect over h2c");
    let settle = Instant::now() + Duration::from_millis(250);
    while Instant::now() < settle {
        conn.drive();
        std::thread::sleep(Duration::from_millis(1));
    }
    let mut statuses = Vec::new();
    for i in 0..n {
        let req = HttpRequest {
            method: "GET".to_string(),
            path: "/".to_string(),
            query: None,
            version: "HTTP/1.1".to_string(),
            headers: Vec::new(),
            body: Vec::new(),
        };
        let client = format!("203.0.113.{}", i + 1);
        let stream_id = conn
            .dispatch(&req, "127.0.0.1", &client, "localhost")
            .expect("dispatch");
        let deadline = Instant::now() + Duration::from_secs(10);
        let resp = loop {
            conn.drive();
            if let Some(c) = conn
                .drain_completions()
                .into_iter()
                .find(|c| c.stream_id == stream_id)
            {
                break c.result.expect("response");
            }
            if Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(1));
            } else {
                panic!("no response\n{}", svc.output());
            }
        };
        statuses.push(resp.status);
    }

    let status = svc.terminate(Duration::from_secs(5));
    let output = svc.output();
    assert!(status.success(), "exit {status}\n{output}");
    statuses
}

#[test]
fn a_declared_h2c_listener_believes_the_forwarded_address() {
    let statuses = statuses_over_h2c(true, 5);
    assert!(
        !statuses.contains(&429),
        "each forwarded address has its own allowance on an internal listener: {statuses:?}"
    );
}

#[test]
fn an_h2c_listener_declared_public_ignores_the_forwarded_address() {
    let statuses = statuses_over_h2c(false, 5);
    assert_eq!(
        statuses.iter().filter(|s| **s == 429).count(),
        3,
        "all requests count against the relaying peer: {statuses:?}"
    );
}

#[test]
fn an_internal_listener_without_internal_peers_refuses_to_start() {
    let tls = testkit::claim_port();
    let h2c = testkit::claim_port();
    let dir = tempfile::tempdir_in(m6_core::testkit::socket_scratch_root()).expect("tempdir");
    let site = write_site(dir.path());
    // Empty certificate files: the refusal fires before TLS is read.
    let config = write_system(
        dir.path(),
        &tls.addr(),
        &format!(
            "h2c_bind = \"{0}\"\ninternal_binds = [\"{0}\"]\n",
            h2c.addr()
        ),
        "",
        "",
    );

    let (code, output) = run_until_exit(&site, &config);

    assert_eq!(code, Some(2), "output:\n{output}");
    assert!(output.contains("internal_peers"), "output:\n{output}");
}

/// The listener names 127.0.0.2 and never uses it, so a connection from
/// 127.0.0.1 is from an unlisted peer. No second loopback address is needed.
#[test]
fn an_unlisted_peer_is_refused_at_connect() {
    let tls = testkit::claim_port();
    let h2c = testkit::claim_port();
    let dir = tempfile::tempdir_in(m6_core::testkit::socket_scratch_root()).expect("tempdir");
    let site = write_site(dir.path());
    let ck = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).expect("rcgen");
    let config = write_system(
        dir.path(),
        &tls.addr(),
        &format!(
            "h2c_bind = \"{0}\"\ninternal_binds = [\"{0}\"]\ninternal_peers = [\"127.0.0.2\"]\n",
            h2c.addr()
        ),
        &ck.cert.pem(),
        &ck.key_pair.serialize_pem(),
    );

    let mut cmd = Command::new(testkit::binary("m6-http"));
    cmd.arg(&site).arg(&config).env("RUST_LOG", "info");
    let mut svc = testkit::Service::spawn("m6-http-internal-peers", &mut cmd);
    svc.wait_for_tcp(tls.port(), Duration::from_secs(10));
    svc.wait_for_tcp(h2c.port(), Duration::from_secs(10));

    // The TCP handshake completes in the kernel, so connect succeeds and the
    // server then closes the socket without reading a byte.
    let outcome = match H2cClientConn::connect("127.0.0.1", h2c.port()) {
        Err(e) => Err(format!("connect: {e}")),
        Ok(mut conn) => {
            let settle = Instant::now() + Duration::from_millis(250);
            while Instant::now() < settle {
                conn.drive();
                std::thread::sleep(Duration::from_millis(1));
            }
            let req = HttpRequest {
                method: "GET".to_string(),
                path: "/".to_string(),
                query: None,
                version: "HTTP/1.1".to_string(),
                headers: Vec::new(),
                body: Vec::new(),
            };
            match conn.dispatch(&req, "127.0.0.1", "203.0.113.1", "localhost") {
                Err(e) => Err(format!("dispatch: {e}")),
                Ok(stream_id) => {
                    let deadline = Instant::now() + Duration::from_secs(10);
                    loop {
                        conn.drive();
                        if let Some(c) = conn
                            .drain_completions()
                            .into_iter()
                            .find(|c| c.stream_id == stream_id)
                        {
                            break match c.result {
                                Ok(r) => Ok(r.status),
                                Err(e) => Err(format!("response: {e}")),
                            };
                        }
                        if Instant::now() < deadline {
                            std::thread::sleep(Duration::from_millis(1));
                        } else {
                            break Err("no response".to_string());
                        }
                    }
                }
            }
        }
    };
    assert!(
        outcome.is_err(),
        "an unlisted peer must get no response: {outcome:?}\n{}",
        svc.output()
    );

    let status = svc.terminate(Duration::from_secs(5));
    let output = svc.output();
    assert!(
        status.success(),
        "the server must survive a refusal: {status}\n{output}"
    );
    assert!(
        output.contains("not in [server] internal_peers"),
        "{output}"
    );
    assert!(output.contains("127.0.0.1"), "{output}");
}
