//! The cache control channel, end to end: the registration listener admits
//! only peers named in `[server] internal_peers`, and a cache node whose
//! control connection drops begins serving held entries.

#![allow(
    clippy::disallowed_methods,
    reason = "test code: a blocking client and scoped threads drive the service from outside"
)]

use std::io::{Read, Write};
use std::net::TcpStream;
use std::process::Command;
use std::time::Duration;

use m6_core::testkit::{binary, claim_port, wait, PortClaim, Service};

fn cert(dir: &std::path::Path) {
    let cert = rcgen::generate_simple_self_signed(vec!["127.0.0.1".to_string()]).unwrap();
    std::fs::write(dir.join("cert.pem"), cert.cert.pem()).unwrap();
    std::fs::write(dir.join("key.pem"), cert.key_pair.serialize_pem()).unwrap();
}

/// A minimal site: no backends, no routes. The control channel needs none.
fn write_site(dir: &std::path::Path, name: &str) {
    std::fs::write(
        dir.join("site.toml"),
        format!(
            "[site]\nname = \"{name}\"\ndomain = \"example.com\"\n\n[analytics]\nenabled = false\n"
        ),
    )
    .unwrap();
}

/// Start an origin with a control listener whose admitted peers are
/// `internal_peers`, and an h2c backbone the cache node reaches it over.
fn start_origin(dir: &std::path::Path, peers: &str) -> (Service, PortClaim, PortClaim) {
    let tls = claim_port();
    let h2c = claim_port();
    let control = claim_port();
    std::fs::write(
        dir.join("system.toml"),
        format!(
            "[server]\nbind = \"{tls}\"\ntls_cert = \"{cert_path}\"\ntls_key = \"{key_path}\"\n\
             h2c_bind = \"{h2c}\"\ninternal_binds = [\"{h2c}\"]\ninternal_peers = [{peers}]\n\
             registration_bind = \"{control}\"\n\n[node]\nname = \"origin\"\n",
            tls = tls.addr(),
            h2c = h2c.addr(),
            control = control.addr(),
            cert_path = dir.join("cert.pem").display(),
            key_path = dir.join("key.pem").display(),
        ),
    )
    .unwrap();
    let origin = Service::spawn(
        "m6-http",
        Command::new(binary("m6-http"))
            .arg(dir)
            .arg(dir.join("system.toml")),
    );
    (origin, h2c, control)
}

/// Start a cache node that registers with the origin's control listener and
/// forwards content over the origin's h2c backbone.
fn start_cache(dir: &std::path::Path, origin_h2c: &str, origin_control: &str) -> Service {
    let tls = claim_port();
    std::fs::write(
        dir.join("system.toml"),
        format!(
            "[server]\nbind = \"{tls}\"\ntls_cert = \"{cert_path}\"\ntls_key = \"{key_path}\"\n\n\
             [node]\nname = \"lon\"\n\n[registration]\norigin = \"{control}\"\n",
            tls = tls.addr(),
            control = origin_control,
            cert_path = dir.join("cert.pem").display(),
            key_path = dir.join("key.pem").display(),
        ),
    )
    .unwrap();
    // The cache node's one backend is the origin over h2c, and every route
    // forwards to it. Nothing is served locally.
    std::fs::write(
        dir.join("site.toml"),
        format!(
            "[site]\nname = \"cache\"\ndomain = \"example.com\"\n\n[analytics]\nenabled = false\n\n\
             [[backend]]\nname = \"origin\"\nurl = \"h2c://{origin_h2c}\"\n\n\
             [[route]]\npath = \"/\"\nbackend = \"origin\"\n",
            origin_h2c = origin_h2c,
        ),
    )
    .unwrap();
    Service::spawn(
        "m6-http",
        Command::new(binary("m6-http"))
            .arg(dir)
            .arg(dir.join("system.toml")),
    )
}

/// A control listener admits a peer named in `[server] internal_peers` and
/// answers its REGISTER with a COUNTER.
#[test]
fn a_peer_in_internal_peers_registers_and_gets_the_counter() {
    let dir = tempfile::Builder::new()
        .prefix("m6-control-")
        .tempdir_in(m6_core::testkit::socket_scratch_root())
        .unwrap();
    cert(dir.path());
    write_site(dir.path(), "origin");
    let (mut origin, _h2c, control) = start_origin(dir.path(), "\"127.0.0.1\"");
    origin.wait_for_started(m6_core::testkit::STARTUP_BOUND);

    let mut stream = TcpStream::connect(control.addr()).expect("connect to the control listener");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream.write_all(b"REGISTER lon 0\n").unwrap();
    let mut buf = Vec::new();
    let _ = stream.read_to_end(&mut buf);
    let answer = String::from_utf8_lossy(&buf);
    assert!(
        answer.starts_with("COUNTER "),
        "a listed peer must get a COUNTER reply, got {answer:?}; origin log:\n{}",
        origin.output()
    );
}

/// A control listener refuses a peer not named in `[server] internal_peers`:
/// the connection is closed without a COUNTER, so a stranger cannot register a
/// fake cache and read the purge fan-out.
#[test]
fn a_peer_not_in_internal_peers_is_refused() {
    let dir = tempfile::Builder::new()
        .prefix("m6-control-")
        .tempdir_in(m6_core::testkit::socket_scratch_root())
        .unwrap();
    cert(dir.path());
    write_site(dir.path(), "origin");
    // The test connects from 127.0.0.1; the list names only 127.0.0.2.
    let (mut origin, _h2c, control) = start_origin(dir.path(), "\"127.0.0.2\"");
    origin.wait_for_started(m6_core::testkit::STARTUP_BOUND);

    let mut stream = TcpStream::connect(control.addr()).expect("connect to the control listener");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream.write_all(b"REGISTER fake 0\n").unwrap();
    let mut buf = Vec::new();
    let _ = stream.read_to_end(&mut buf);
    assert!(
        buf.is_empty(),
        "a refused peer must get no COUNTER reply, got {:?}",
        String::from_utf8_lossy(&buf)
    );
}

/// A cache node whose control connection drops serves held entries: the
/// transition is logged, and the node keeps answering.
#[test]
fn a_cache_node_serves_held_when_its_origin_drops() {
    let dir = tempfile::Builder::new()
        .prefix("m6-held-")
        .tempdir_in(m6_core::testkit::socket_scratch_root())
        .unwrap();
    cert(dir.path());
    write_site(dir.path(), "origin");
    let (mut origin, h2c, control) = start_origin(dir.path(), "\"127.0.0.1\"");
    origin.wait_for_started(m6_core::testkit::STARTUP_BOUND);

    let cache_dir = tempfile::Builder::new()
        .prefix("m6-held-cache-")
        .tempdir_in(m6_core::testkit::socket_scratch_root())
        .unwrap();
    cert(cache_dir.path());
    let mut cache = start_cache(cache_dir.path(), &h2c.addr(), &control.addr());
    cache.wait_for_started(m6_core::testkit::STARTUP_BOUND);

    // The cache registers on its accounting tick (once a second), so a short
    // wait is enough for the control connection to be up before the origin
    // goes away.
    std::thread::sleep(Duration::from_secs(2));
    origin.kill();

    let observed = wait::until(m6_core::testkit::STARTUP_BOUND, || {
        cache.output().contains("serving held content")
    });
    assert!(
        observed,
        "the cache node must begin serving held entries when its origin drops; log:\n{}",
        cache.output()
    );
}

/// A cache node whose control connection is closed by the origin connects
/// again and registers again, so it is back in the fan-out within seconds.
/// The origin here is the test itself, so it can close the connection.
#[test]
fn a_cache_node_whose_connection_closes_registers_again() {
    let control = claim_port();
    let listener = std::net::TcpListener::bind(control.addr()).expect("bind the fake origin");
    // Nothing listens on the h2c backbone: the control channel needs no
    // content.
    let h2c = claim_port();

    let cache_dir = tempfile::Builder::new()
        .prefix("m6-rereg-cache-")
        .tempdir_in(m6_core::testkit::socket_scratch_root())
        .unwrap();
    cert(cache_dir.path());
    let mut cache = start_cache(cache_dir.path(), &h2c.addr(), &control.addr());
    cache.wait_for_started(m6_core::testkit::STARTUP_BOUND);

    // A blocking accept would hang the test if the cache never connects, so
    // the listener is polled to a bound instead.
    listener.set_nonblocking(true).unwrap();
    let accept = |listener: &std::net::TcpListener| {
        let mut got = None;
        let ok = wait::until(m6_core::testkit::STARTUP_BOUND, || {
            match listener.accept() {
                Ok((stream, _)) => {
                    stream.set_nonblocking(false).unwrap();
                    got = Some(stream);
                    true
                }
                Err(_) => false,
            }
        });
        assert!(ok, "the cache must connect; log:\n{}", cache.output());
        got.unwrap()
    };
    let read_register = |stream: TcpStream| -> (TcpStream, String) {
        use std::io::BufRead;
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let mut line = String::new();
        let _ = std::io::BufReader::new(stream.try_clone().unwrap()).read_line(&mut line);
        (stream, line)
    };

    let (mut first, line) = read_register(accept(&listener));
    assert!(line.starts_with("REGISTER lon "), "got {line:?}");
    first.write_all(b"COUNTER 0\n").unwrap();

    // The origin closes the connection. The cache sees the close, serves held
    // content, and registers again on a new connection.
    first.shutdown(std::net::Shutdown::Both).unwrap();
    drop(first);
    let (_second, line) = read_register(accept(&listener));
    assert!(
        line.starts_with("REGISTER lon "),
        "the cache must register again after its connection closed, got {line:?}; log:\n{}",
        cache.output()
    );
    assert!(
        cache.output().contains("serving held content"),
        "the close must be seen as a drop; log:\n{}",
        cache.output()
    );
}
