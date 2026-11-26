//! The `PURGE` method and `/m6/status`, end to end against a real origin.
//!
//! A real `m6-http`, a real `m6-file` content backend, and a threaded
//! `CheckServer` as the authority. The origin purges itself and answers a
//! per-node result. One test registers a cache by hand on the control channel
//! and reads the fan-out from it; the registration state is covered by the unit
//! tests in `registration.rs`, and the address rule by the unit tests in
//! `health.rs`.

#![allow(
    clippy::disallowed_methods,
    reason = "test code: a blocking TLS client and a scoped thread serve the fake checker while the child runs"
)]

use std::io::{Read, Write};
use std::net::TcpStream;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use rustls::StreamOwned;

use m6_core::auth::Identity;
use m6_core::testkit::{binary, claim_port, Reply, Service, ServingCheck};

fn identity(role: &str) -> Identity {
    Identity {
        sub: format!("{role}-1"),
        username: role.to_string(),
        roles: vec![role.to_string()],
        groups: Vec::new(),
    }
}

fn tls_config(cert_der: &[u8]) -> Arc<rustls::ClientConfig> {
    let cert = rustls::pki_types::CertificateDer::from(cert_der.to_vec());
    let mut store = rustls::RootCertStore::empty();
    store.add(cert).unwrap();
    Arc::new(
        rustls::ClientConfig::builder()
            .with_root_certificates(store)
            .with_no_client_auth(),
    )
}

struct HttpResponse {
    status: u16,
    body: String,
}

/// One raw request over TLS, with an `Authorization: Bearer` header when a
/// token is given.
fn request(
    port: u16,
    method: &str,
    path: &str,
    token: Option<&str>,
    tls: Arc<rustls::ClientConfig>,
) -> HttpResponse {
    let tcp = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    tcp.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let name = rustls::pki_types::ServerName::try_from("127.0.0.1".to_string()).unwrap();
    let conn = rustls::ClientConnection::new(tls, name).unwrap();
    let mut stream = StreamOwned::new(conn, tcp);

    let mut req = format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n");
    if let Some(token) = token {
        req.push_str(&format!("Authorization: Bearer {token}\r\n"));
    }
    req.push_str("Connection: close\r\n\r\n");
    stream.write_all(req.as_bytes()).unwrap();
    stream.flush().unwrap();

    let mut raw = Vec::new();
    let _ = stream.read_to_end(&mut raw);
    let text = String::from_utf8_lossy(&raw);
    let end = text.find("\r\n\r\n").unwrap_or(raw.len());
    let status = text
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|status| status.parse().ok())
        .unwrap_or(0);
    HttpResponse {
        status,
        body: text.get(end + 4..).unwrap_or("").to_string(),
    }
}

struct Fixture {
    port: u16,
    cert_der: Vec<u8>,
    _http: Service,
    _file: Service,
    _check: ServingCheck,
    _dir: tempfile::TempDir,
    _port: m6_core::testkit::PortClaim,
    /// The origin's control listener, where a cache node registers.
    control: m6_core::testkit::PortClaim,
    /// The internal h2c listener: the control listener's peers are the
    /// internal peers, which need an internal listener.
    _h2c: m6_core::testkit::PortClaim,
}

impl Fixture {
    fn start() -> Fixture {
        let dir = tempfile::Builder::new()
            .prefix("m6-purge-")
            .tempdir_in(m6_core::testkit::socket_scratch_root())
            .unwrap();
        let site = dir.path();

        let cert = rcgen::generate_simple_self_signed(vec!["127.0.0.1".to_string()]).unwrap();
        let cert_pem = cert.cert.pem();
        let key_pem = cert.key_pair.serialize_pem();
        let cert_der = cert.cert.der().to_vec();
        std::fs::write(site.join("cert.pem"), &cert_pem).unwrap();
        std::fs::write(site.join("key.pem"), &key_pem).unwrap();

        let check_path = site.join("check.sock");
        let check = ServingCheck::start(
            &check_path,
            vec![
                ("writer".to_string(), Reply::Identity(identity("writer"))),
                ("reader".to_string(), Reply::Identity(identity("reader"))),
            ],
        )
        .expect("the check server starts");

        let file_sock = site.join("m6-file-1.sock");
        let file_glob = site.join("m6-file-*.sock");
        std::fs::write(
            site.join("site.toml"),
            format!(
                r#"
[site]
name = "purge"
domain = "example.com"

[log]
level = "debug"

[analytics]
enabled = false

[[backend]]
name = "m6-file"
sockets = "{file_glob}"

[[backend]]
name = "auth-check"
sockets = "{check}"

[[route]]
path = "/"
backend = "m6-file"
cache = "public, max-age=3600"

[[route]]
path = "/index.html"
backend = "m6-file"
cache = "public, max-age=3600"

[[route]]
path = "/r/{{code}}"
backend = "m6-file"
"#,
                file_glob = file_glob.display(),
                check = check_path.display(),
            ),
        )
        .unwrap();
        std::fs::write(
            site.join("m6-file.conf"),
            r#"
[[route]]
path = "/{relpath}"
handler = "files"
root = "."
"#,
        )
        .unwrap();
        std::fs::write(site.join("index.html"), b"<html>home</html>").unwrap();

        let claim = claim_port();
        let port = claim.port();
        let control = claim_port();
        let h2c = claim_port();
        let system = site.join("system.toml");
        std::fs::write(
            &system,
            format!(
                r#"
[server]
bind = "127.0.0.1:{port}"
tls_cert = "{cert}"
tls_key = "{key}"
h2c_bind = "{h2c}"
internal_binds = ["{h2c}"]
internal_peers = ["127.0.0.1"]
registration_bind = "{control}"

[node]
name = "origin"

[auth]
backend = "auth-check"

[registration]
read_require = "role:reader"
write_require = "role:writer"
"#,
                cert = site.join("cert.pem").display(),
                key = site.join("key.pem").display(),
                control = control.addr(),
                h2c = h2c.addr(),
            ),
        )
        .unwrap();

        let file = Service::spawn(
            "m6-file",
            Command::new(binary("m6-file"))
                .arg(site)
                .arg(site.join("m6-file.conf"))
                .env("M6_SOCKET_OVERRIDE", &file_sock),
        );
        m6_core::testkit::wait::until(m6_core::testkit::STARTUP_BOUND, || {
            std::path::Path::new(&file_sock).exists()
        });

        let mut http = Service::spawn(
            "m6-http",
            Command::new(binary("m6-http")).arg(site).arg(&system),
        );
        http.wait_for_started(m6_core::testkit::STARTUP_BOUND);

        Fixture {
            port,
            cert_der,
            _http: http,
            _file: file,
            _check: check,
            _dir: dir,
            _port: claim,
            control,
            _h2c: h2c,
        }
    }

    fn purge(&self, path: &str, token: Option<&str>) -> HttpResponse {
        request(self.port, "PURGE", path, token, tls_config(&self.cert_der))
    }

    fn status(&self, token: &str) -> HttpResponse {
        request(
            self.port,
            "GET",
            "/m6/status",
            Some(token),
            tls_config(&self.cert_der),
        )
    }
}

/// A purge with the write token answers 200 and names the origin, resident or
/// not, and raises the change counter by one.
#[test]
fn a_purge_with_the_write_token_answers_and_raises_the_counter() {
    let fixture = Fixture::start();

    // Warm a file so a later purge of "/index.html" reports resident.
    let warm = request(
        fixture.port,
        "GET",
        "/index.html",
        None,
        tls_config(&fixture.cert_der),
    );
    assert_eq!(
        warm.status, 200,
        "the warm fetch serves the file: {}",
        warm.body
    );

    let first = fixture.purge("/r/missing", Some("writer"));
    assert_eq!(first.status, 200, "body: {}", first.body);
    assert!(
        first.body.contains("\"origin\":{\"resident\":false}"),
        "an absent path is purged successfully with resident false: {}",
        first.body
    );

    let second = fixture.purge("/index.html", Some("writer"));
    assert_eq!(second.status, 200, "body: {}", second.body);
    assert!(
        second.body.contains("\"origin\":{\"resident\":true}"),
        "a resident path is purged with resident true: {}",
        second.body
    );

    let status = fixture.status("reader");
    assert_eq!(status.status, 200, "body: {}", status.body);
    assert!(
        status.body.contains("\"counter\":\"2\""),
        "two purges raised the counter to 2: {}",
        status.body
    );
}

/// A purge with no token is 401, and a token without the write claim is 403.
#[test]
fn a_purge_needs_the_write_claim() {
    let fixture = Fixture::start();

    let no_token = fixture.purge("/r/x", None);
    assert_eq!(no_token.status, 401, "body: {}", no_token.body);

    let wrong_claim = fixture.purge("/r/x", Some("reader"));
    assert_eq!(wrong_claim.status, 403, "body: {}", wrong_claim.body);
}

/// `PURGE /*` purges all, and a non-path target is a malformed 400.
#[test]
fn purge_all_and_a_malformed_target() {
    let fixture = Fixture::start();

    // Warm a file so the purge-all has an entry to remove, and so the reply's
    // residency proves the purge actually evicted it.
    let warm = request(
        fixture.port,
        "GET",
        "/index.html",
        None,
        tls_config(&fixture.cert_der),
    );
    assert_eq!(
        warm.status, 200,
        "the warm fetch serves the file: {}",
        warm.body
    );

    let all = fixture.purge("/*", Some("writer"));
    assert_eq!(all.status, 200, "body: {}", all.body);
    assert!(
        all.body.contains("\"origin\":{\"resident\":true}"),
        "a purge-all evicts the warmed entry: {}",
        all.body
    );

    let malformed = fixture.purge("*", Some("writer"));
    assert_eq!(malformed.status, 400, "body: {}", malformed.body);
}

/// `/m6/status` needs the read claim and reports the counter and an empty
/// registered set before any cache has registered.
#[test]
fn status_needs_the_read_claim() {
    let fixture = Fixture::start();

    let no_token = request(
        fixture.port,
        "GET",
        "/m6/status",
        None,
        tls_config(&fixture.cert_der),
    );
    assert_eq!(no_token.status, 401, "body: {}", no_token.body);

    let wrong_claim = fixture.status("writer");
    assert_eq!(wrong_claim.status, 403, "body: {}", wrong_claim.body);

    let ok = fixture.status("reader");
    assert_eq!(ok.status, 200, "body: {}", ok.body);
    assert!(
        ok.body.contains("\"registered\":[]"),
        "no cache has registered yet: {}",
        ok.body
    );
}

/// Read one `\n`-terminated line from a control connection.
fn read_line(reader: &mut std::io::BufReader<TcpStream>) -> String {
    use std::io::BufRead;
    let mut line = String::new();
    let _ = reader.read_line(&mut line);
    line
}

/// A registered cache's control connection is long-lived and mostly idle. Left
/// idle past every request deadline the origin keeps (the 30 s read deadline
/// among them), it still carries the purge fan-out: the cache reads the
/// `PURGE` line, answers `RESIDENT`, and the origin's reply names it.
#[test]
fn a_purge_reaches_a_cache_whose_control_connection_sat_idle() {
    let fixture = Fixture::start();
    let warm = request(
        fixture.port,
        "GET",
        "/index.html",
        None,
        tls_config(&fixture.cert_der),
    );
    assert_eq!(warm.status, 200, "body: {}", warm.body);

    let stream = TcpStream::connect(fixture.control.addr()).expect("connect the control channel");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let mut writer = stream.try_clone().unwrap();
    let mut reader = std::io::BufReader::new(stream);
    writer.write_all(b"REGISTER lon 0\n").unwrap();
    let counter = read_line(&mut reader);
    assert!(counter.starts_with("COUNTER "), "got {counter:?}");

    // Idle well past the request read deadline (30 s), which is what a real
    // cache's connection does between publishes.
    std::thread::sleep(Duration::from_secs(35));

    std::thread::scope(|s| {
        let purge = s.spawn(|| fixture.purge("/index.html", Some("writer")));
        let line = read_line(&mut reader);
        assert!(
            line.starts_with("PURGE /index.html "),
            "the idle cache must receive the fan-out, got {line:?}"
        );
        writer.write_all(b"RESIDENT true\n").unwrap();
        let reply = purge.join().unwrap();
        assert_eq!(reply.status, 200, "body: {}", reply.body);
        assert!(
            reply.body.contains("\"lon\":{\"resident\":true}"),
            "the reply must carry the cache's answer: {}",
            reply.body
        );
    });
}
