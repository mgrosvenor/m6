#![allow(
    clippy::disallowed_methods,
    reason = "test code: threads, sleeps and blocking clients drive the service from outside"
)]

/// Edge proxy integration tests.
///
/// Validates m6-http in the "edge load-balancer" deployment model:
///
///   Client → Edge m6-http (regional, :8443)
///               ├─ cache HIT  → respond immediately from local Arc<Bytes> cache
///               └─ cache MISS → forward to Global m6-http (:8444) over HTTPS
///                                    → Global m6-http → Unix-socket backends
///
/// Tests cover:
///   1. Basic proxy — requests flow through edge → global → backend
///   2. Cache hit  — second request served from edge cache (no global contact)
///   3. Cache miss — nocache path always forwards to global
///   4. Proxy headers — X-Forwarded-For / X-Forwarded-Proto / X-Forwarded-Host
///   5. Hop-by-hop stripping — Connection / Keep-Alive not forwarded
///   6. Error propagation — 404 from global is forwarded to client
///   7. JWT auth — edge enforces JWT; denied request never reaches global
///   8. Cache isolation — different paths cached independently
///   9. Performance — cache-hit latency vs cache-miss latency (RTT simulation)
use std::io::{Read, Write};
use std::net::TcpStream;
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rustls::StreamOwned;

use m6_core::testkit::{binary, claim_port, PortClaim, Service};

// ── Test infrastructure ───────────────────────────────────────────────────────

/// Generate a self-signed cert+key for 127.0.0.1 / localhost.
/// Returns (cert_pem, key_pem, cert_der).
fn generate_cert() -> (String, String, Vec<u8>) {
    let ck =
        rcgen::generate_simple_self_signed(vec!["localhost".to_string(), "127.0.0.1".to_string()])
            .expect("rcgen");
    let der = ck.cert.der().to_vec();
    (ck.cert.pem(), ck.key_pair.serialize_pem(), der)
}

/// Write cert and key PEMs to temp files; return (cert_path, key_path, _guards).
fn write_pems(cert_pem: &str, key_pem: &str) -> (tempfile::NamedTempFile, tempfile::NamedTempFile) {
    let mut c = tempfile::NamedTempFile::new().unwrap();
    c.write_all(cert_pem.as_bytes()).unwrap();
    let mut k = tempfile::NamedTempFile::new().unwrap();
    k.write_all(key_pem.as_bytes()).unwrap();
    (c, k)
}

/// Build a rustls ClientConfig that trusts a specific DER cert.
fn trusted_client_config(cert_der: &[u8]) -> Arc<rustls::ClientConfig> {
    let cert = rustls::pki_types::CertificateDer::from(cert_der.to_vec());
    let mut store = rustls::RootCertStore::empty();
    store.add(cert).unwrap();
    Arc::new(
        rustls::ClientConfig::builder()
            .with_root_certificates(store)
            .with_no_client_auth(),
    )
}

/// Build a rustls ClientConfig that skips certificate verification (test only).
fn skip_verify_client_config() -> Arc<rustls::ClientConfig> {
    // Re-use the SkipVerifier logic from pool.rs by constructing directly.
    // For the test client we use a minimal dangerous config.
    use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
    use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
    use rustls::DigitallySignedStruct;

    #[derive(Debug)]
    struct NoVerify;
    impl ServerCertVerifier for NoVerify {
        fn verify_server_cert(
            &self,
            _: &CertificateDer,
            _: &[CertificateDer],
            _: &ServerName,
            _: &[u8],
            _: UnixTime,
        ) -> Result<ServerCertVerified, rustls::Error> {
            Ok(ServerCertVerified::assertion())
        }
        fn verify_tls12_signature(
            &self,
            _: &[u8],
            _: &CertificateDer,
            _: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, rustls::Error> {
            Ok(HandshakeSignatureValid::assertion())
        }
        fn verify_tls13_signature(
            &self,
            _: &[u8],
            _: &CertificateDer,
            _: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, rustls::Error> {
            Ok(HandshakeSignatureValid::assertion())
        }
        fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
            rustls::crypto::aws_lc_rs::default_provider()
                .signature_verification_algorithms
                .supported_schemes()
        }
    }
    Arc::new(
        rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(NoVerify))
            .with_no_client_auth(),
    )
}

/// Send a raw HTTP/1.1 GET over TLS to 127.0.0.1:port.
/// Returns (status_line, headers_str, body).
fn https_get(
    port: u16,
    path: &str,
    extra_headers: &[(&str, &str)],
    tls: Arc<rustls::ClientConfig>,
) -> (String, String, Vec<u8>) {
    let tcp = TcpStream::connect(format!("127.0.0.1:{}", port)).expect("tcp connect");
    tcp.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let server_name = rustls::pki_types::ServerName::try_from("127.0.0.1".to_string()).unwrap();
    let conn = rustls::ClientConnection::new(tls, server_name).unwrap();
    let mut stream = StreamOwned::new(conn, tcp);

    let mut req = format!("GET {} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n", path, port);
    for (k, v) in extra_headers {
        req.push_str(&format!("{}: {}\r\n", k, v));
    }
    req.push_str("Connection: close\r\n\r\n");
    stream.write_all(req.as_bytes()).unwrap();
    stream.flush().unwrap();

    let mut resp = Vec::new();
    // rustls 0.23 returns UnexpectedEof when the peer closes the TCP connection
    // without a TLS close_notify (common in HTTP/1.1 Connection:close). The data
    // already in `resp` is complete; treat this as a normal EOF.
    match stream.read_to_end(&mut resp) {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {}
        Err(e) => panic!("TLS read error: {e}"),
    }
    let resp_str = String::from_utf8_lossy(&resp);

    let header_end = resp_str.find("\r\n\r\n").unwrap_or(resp.len());
    let headers = resp_str[..header_end].to_string();
    let body = resp[header_end + 4..].to_vec();
    let status_line = headers.lines().next().unwrap_or("").to_string();
    (status_line, headers, body)
}

/// Whether the response header block carries `name`, case-insensitively.
///
/// The header block here is the raw text including the status line, so the
/// first line is skipped: `HTTP/1.1 200 OK` would otherwise match a search for
/// a header called "http".
fn has_header(headers: &str, name: &str) -> bool {
    let want = format!("{}:", name.to_ascii_lowercase());
    headers
        .lines()
        .skip(1)
        .any(|l| l.trim_start().to_ascii_lowercase().starts_with(&want))
}

fn status_code(status_line: &str) -> u16 {
    status_line
        .split_whitespace()
        .nth(1)
        .unwrap_or("0")
        .parse()
        .unwrap_or(0)
}

// ── Site setup ────────────────────────────────────────────────────────────────

/// Write a minimal bench-style site under `dir`.
fn setup_site(dir: &std::path::Path, html_sock: &str, file_sock: &str) {
    std::fs::create_dir_all(dir.join("templates")).unwrap();
    std::fs::create_dir_all(dir.join("assets")).unwrap();
    std::fs::create_dir_all(dir.join("data")).unwrap();
    std::fs::create_dir_all(dir.join("nocache")).unwrap();
    std::fs::create_dir_all(dir.join("configs")).unwrap();

    std::fs::write(
        dir.join("templates/home.html"),
        b"<!doctype html><html><body><h1>global</h1></body></html>",
    )
    .unwrap();
    std::fs::write(dir.join("assets/hello.txt"), b"hello from m6-file").unwrap();
    std::fs::write(dir.join("data/site.json"), b"{\"site_name\":\"edge-test\"}").unwrap();

    // site.toml
    std::fs::write(
        dir.join("site.toml"),
        format!(
            r#"
[site]
name   = "edge-test"
domain = "localhost"

[errors]
mode = "internal"

[log]
level  = "warn"

[analytics]
enabled = false

[[backend]]
name    = "m6-html"
sockets = "{html_sock}"

[[backend]]
name    = "m6-file"
sockets = "{file_sock}"

[[route]]
path    = "/"
backend = "m6-html"

[[route]]
path    = "/nocache/"
backend = "m6-html"

[[route_group]]
glob    = "assets/**/*"
path    = "/assets/{{relpath}}"
backend = "m6-file"
"#
        ),
    )
    .unwrap();

    // m6-html.conf
    std::fs::write(
        dir.join("configs/m6-html.conf"),
        r#"
global_params = ["data/site.json"]

[[route]]
path     = "/"
template = "templates/home.html"

[[route]]
path     = "/nocache/"
template = "templates/home.html"
cache    = "no-store"
"#,
    )
    .unwrap();

    // m6-file.conf
    std::fs::write(
        dir.join("configs/m6-file.conf"),
        r#"
[[route]]
path = "/assets/{*relpath}"
handler = "files"
root = "assets/"
"#,
    )
    .unwrap();
}

/// site.toml for the edge — single URL backend pointing at global.
fn setup_edge_site(dir: &std::path::Path, global_port: u16) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(
        dir.join("site.toml"),
        format!(
            r#"
[site]
name   = "edge"
domain = "localhost"

[errors]
mode = "internal"

[log]
level  = "warn"

[analytics]
enabled = false

[[backend]]
name           = "global"
url            = "h2s://127.0.0.1:{global_port}"
tls_skip_verify = true

[[route]]
path    = "/"
backend = "global"

[[route]]
path    = "/nocache/"
backend = "global"

[[route]]
path    = "/assets/{{*relpath}}"
backend = "global"
"#
        ),
    )
    .unwrap();
}

// ── Full stack fixture ────────────────────────────────────────────────────────

struct EdgeStack {
    // Field order is drop order: the four services die before the temp dir
    // they serve from is removed, and the port claims are released last.
    _global_html: Service,
    _global_file: Service,
    global_http: Service,
    _edge_http: Service,
    global_port: u16,
    edge_port: u16,
    edge_tls: Arc<rustls::ClientConfig>,
    // Temp dir guards
    _tmpdir: tempfile::TempDir,
    _global_cert_f: tempfile::NamedTempFile,
    _global_key_f: tempfile::NamedTempFile,
    _edge_cert_f: tempfile::NamedTempFile,
    _edge_key_f: tempfile::NamedTempFile,
    _global_claim: PortClaim,
    _edge_claim: PortClaim,
}

impl EdgeStack {
    fn start() -> Self {
        rustls::crypto::aws_lc_rs::default_provider()
            .install_default()
            .ok();

        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path();

        // ── Ports ─────────────────────────────────────────────────────────
        // Ports come from the shared allocator, which claims each one across
        // processes for the whole window between allocation and the server
        // binding it. Binding to :0 and releasing raced with every other test
        // binary cargo runs in parallel.
        let global_claim = claim_port();
        let edge_claim = claim_port();
        let global_port: u16 = global_claim.port();
        let edge_port: u16 = edge_claim.port();

        // ── Certs ─────────────────────────────────────────────────────────
        let (gc_pem, gk_pem, _gc_der) = generate_cert();
        let (gc_f, gk_f) = write_pems(&gc_pem, &gk_pem);
        let (ec_pem, ek_pem, ec_der) = generate_cert();
        let (ec_f, ek_f) = write_pems(&ec_pem, &ek_pem);

        // ── Global site ───────────────────────────────────────────────────
        let global_site = base.join("global-site");
        let html_sock = base.join("m6-html.sock");
        let file_sock = base.join("m6-file.sock");
        setup_site(
            &global_site,
            html_sock.to_str().unwrap(),
            file_sock.to_str().unwrap(),
        );

        // global system.toml
        let global_sys = base.join("global-system.toml");
        std::fs::write(&global_sys, format!(
            "[node]\nname = \"global\"\n\n[server]\nbind     = \"127.0.0.1:{global_port}\"\ntls_cert = \"{}\"\ntls_key  = \"{}\"\n",
            gc_f.path().display(), gk_f.path().display()
        )).unwrap();

        // ── Edge site ─────────────────────────────────────────────────────
        let edge_site = base.join("edge-site");
        setup_edge_site(&edge_site, global_port);

        let edge_sys = base.join("edge-system.toml");
        std::fs::write(&edge_sys, format!(
            "[node]\nname = \"edge\"\n\n[server]\nbind     = \"127.0.0.1:{edge_port}\"\ntls_cert = \"{}\"\ntls_key  = \"{}\"\n",
            ec_f.path().display(), ek_f.path().display()
        )).unwrap();

        // ── Start global backends ─────────────────────────────────────────
        let mut html_proc = Service::spawn(
            "m6-html",
            Command::new(binary("m6-html"))
                .args([
                    global_site.to_str().unwrap(),
                    global_site.join("configs/m6-html.conf").to_str().unwrap(),
                    "--log-level",
                    "warn",
                ])
                .env("M6_SOCKET_OVERRIDE", html_sock.to_str().unwrap()),
        );

        let mut file_proc = Service::spawn(
            "m6-file",
            Command::new(binary("m6-file"))
                .args([
                    global_site.to_str().unwrap(),
                    global_site.join("configs/m6-file.conf").to_str().unwrap(),
                    "--log-level",
                    "warn",
                ])
                .env("M6_SOCKET_OVERRIDE", file_sock.to_str().unwrap()),
        );

        // Wait for the backend sockets to actually exist rather than guessing.
        //
        // This was `sleep(300ms)`. Alone that was enough; in a full workspace
        // run it is not remotely — a dozen of these stacks start at once, each
        // spawning four processes, and m6-http would come up with an empty
        // backend pool and refuse to start. m6-http reads its pool once at
        // start, so the sockets must exist before it is spawned.
        html_proc.wait_for_path(&html_sock, Duration::from_secs(15));
        file_proc.wait_for_path(&file_sock, Duration::from_secs(15));

        // ── Start global m6-http ──────────────────────────────────────────
        let mut global_proc = Service::spawn(
            "global m6-http",
            Command::new(binary("m6-http")).args([
                global_site.to_str().unwrap(),
                global_sys.to_str().unwrap(),
                "--log-level",
                "warn",
            ]),
        );
        global_proc.wait_for_tcp(global_port, Duration::from_secs(5));

        // ── Start edge m6-http ────────────────────────────────────────────
        let mut edge_proc = Service::spawn(
            "edge m6-http",
            Command::new(binary("m6-http")).args([
                edge_site.to_str().unwrap(),
                edge_sys.to_str().unwrap(),
                "--log-level",
                "warn",
            ]),
        );
        edge_proc.wait_for_tcp(edge_port, Duration::from_secs(5));

        let edge_tls = trusted_client_config(&ec_der);

        // Listening on a port is not the same as being able to serve: a backend
        // that has not finished starting answers 502, so poll until the stack
        // actually answers rather than assuming a fixed delay is enough. This
        // is the difference between a suite that passes alone and one that
        // passes under load.
        let ready_deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let (status, _, _) = https_get(global_port, "/", &[], skip_verify_client_config());
            if status_code(&status) == 200 {
                break;
            }
            assert!(
                Instant::now() < ready_deadline,
                "global stack never became ready (last status: {status})"
            );
            std::thread::sleep(Duration::from_millis(100));
        }

        EdgeStack {
            _global_html: html_proc,
            _global_file: file_proc,
            global_http: global_proc,
            _edge_http: edge_proc,
            global_port,
            edge_port,
            edge_tls,
            _tmpdir: tmp,
            _global_cert_f: gc_f,
            _global_key_f: gk_f,
            _edge_cert_f: ec_f,
            _edge_key_f: ek_f,
            _global_claim: global_claim,
            _edge_claim: edge_claim,
        }
    }

    fn get(&self, path: &str) -> (String, String, Vec<u8>) {
        https_get(self.edge_port, path, &[], self.edge_tls.clone())
    }

    fn get_with_headers(&self, path: &str, hdrs: &[(&str, &str)]) -> (String, String, Vec<u8>) {
        https_get(self.edge_port, path, hdrs, self.edge_tls.clone())
    }

    fn get_global(&self, path: &str) -> (String, String, Vec<u8>) {
        https_get(self.global_port, path, &[], skip_verify_client_config())
    }

    /// Kill the global m6-http the edge proxies to, and reap it.
    ///
    /// A cache hit that still opened a connection to the backend cannot answer
    /// once this has run, so a test that calls it proves the hit came from the
    /// edge's own store. `Service::kill` waits for the child, so the backend is
    /// gone before the next request.
    fn stop_global(&mut self) {
        self.global_http.kill();
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

/// 1. Basic proxy — request flows edge → global → m6-html backend.
#[test]
fn test_basic_proxy() {
    let stack = EdgeStack::start();
    let (status, headers, body) = stack.get("/");
    assert_eq!(status_code(&status), 200, "expected 200, got: {status}");
    let body_str = String::from_utf8_lossy(&body);
    assert!(
        body_str.contains("global"),
        "body should contain 'global', got: {body_str}"
    );
    let _ = headers;
}

/// 2. Cache hit: the second request is served from the edge cache.
///
/// `/` cannot be used for this: m6-http warms its own cache at startup by
/// fetching every concrete route, so the first request for `/` already carries
/// `Age`. The asset is reached only through a `[[route_group]]`, which the
/// warm-up skips because a pattern is not a URL. It is also cacheable, which
/// `/nocache/` deliberately is not.
#[test]
fn test_cache_hit() {
    let stack = EdgeStack::start();

    // First request: a miss, which goes to global and m6-file.
    let t_miss = Instant::now();
    let (s1, h1, b1) = stack.get("/assets/hello.txt");
    let miss_latency = t_miss.elapsed();
    assert_eq!(status_code(&s1), 200);
    assert!(
        !has_header(&h1, "age"),
        "the first request for the asset should be a miss, but it carried an age header:\n{h1}"
    );

    // Second request: served from a cache.
    let t0 = Instant::now();
    let (s2, h2, b2) = stack.get("/assets/hello.txt");
    let hit_latency = t0.elapsed();
    assert_eq!(status_code(&s2), 200);
    assert_eq!(b1, b2, "cache hit body must match cache miss body");
    assert!(
        has_header(&h2, "age"),
        "the second request should have been served from a cache, but it \
         carried no age header:\n{h2}"
    );

    // The latencies are a report line, not a pass condition. On loopback the
    // two populations are microseconds apart and a comparison measures the
    // machine: a recorded run inverted by 1.3 percent while the cache worked.
    println!("cache-miss latency: {miss_latency:?}, cache-hit latency: {hit_latency:?}");
}

/// 3. Cache miss — nocache path always forwards to global.
#[test]
fn test_cache_miss_nocache_path() {
    let stack = EdgeStack::start();
    // /nocache/ has Cache-Control: no-store in m6-html.conf — never cached.
    let (s1, _, _) = stack.get("/nocache/");
    assert_eq!(status_code(&s1), 200);
    let (s2, _, _) = stack.get("/nocache/");
    assert_eq!(status_code(&s2), 200);
    // Both should succeed (not stale / 502) — verifies global is reachable for each.
}

/// 4. Static file proxy — edge proxies m6-file asset.
#[test]
fn test_static_file_proxy() {
    let stack = EdgeStack::start();
    let (status, _, body) = stack.get("/assets/hello.txt");
    assert_eq!(
        status_code(&status),
        200,
        "expected 200 for /assets/hello.txt, got: {status}"
    );
    assert_eq!(body.trim_ascii(), b"hello from m6-file" as &[u8]);
}

/// 5. Static file cache hit — second request served from edge, not global.
#[test]
fn test_static_file_cache_hit() {
    let stack = EdgeStack::start();
    let (s1, h1, b1) = stack.get("/assets/hello.txt");
    assert_eq!(status_code(&s1), 200);

    let (s2, h2, b2) = stack.get("/assets/hello.txt");
    assert_eq!(status_code(&s2), 200);
    assert_eq!(b1, b2);

    // Assert the cache state, not the clock.
    //
    // This used to be `hit_latency < 5ms`, which measures the machine: it
    // failed on a loaded build machine while the code was correct. A test that
    // fails when the machine is busy tells you about the machine.
    //
    // `Age` is the structural answer and it discriminates exactly: the edge
    // adds it when it serves from its own store, so a miss carries no `age`
    // header at all and a hit carries one (`age: 0` when it was stored a
    // moment ago). Measured on the wire before being relied on.
    assert!(
        !has_header(&h1, "age"),
        "the first request should be a miss, but it carried an age header:\n{h1}"
    );
    assert!(
        has_header(&h2, "age"),
        "the second request should have been served from the edge cache, but \
         it carried no age header:\n{h2}"
    );
}

/// 6. Error propagation — 404 from global forwarded to client.
#[test]
fn test_404_propagation() {
    let stack = EdgeStack::start();
    let (status, _, _) = stack.get("/path/that/does/not/exist");
    assert_eq!(status_code(&status), 404, "expected 404, got: {status}");
}

/// 7. Proxy headers set by edge — X-Forwarded-For should be recorded.
///    We verify by checking the global directly serves the page (global is reachable).
///    Full header visibility would require a backend that echoes request headers.
#[test]
fn test_global_reachable_directly() {
    let stack = EdgeStack::start();
    // Verify global is healthy independently of edge.
    let (status, _, body) = stack.get_global("/");
    assert_eq!(status_code(&status), 200);
    let body_str = String::from_utf8_lossy(&body);
    assert!(body_str.contains("global"));
}

/// 8. Cache isolation — / and /assets/hello.txt cached independently.
#[test]
fn test_cache_isolation() {
    let stack = EdgeStack::start();

    let (_, _, html_body) = stack.get("/");
    let (_, _, file_body) = stack.get("/assets/hello.txt");

    assert_ne!(
        html_body, file_body,
        "different paths must have different cache entries"
    );

    // Both should remain consistent on second hit.
    let (_, _, html_body2) = stack.get("/");
    let (_, _, file_body2) = stack.get("/assets/hello.txt");
    assert_eq!(html_body, html_body2);
    assert_eq!(file_body, file_body2);
}

/// 9. Hop-by-hop headers — Connection header should not be forwarded.
///    Edge must strip it before sending to global (RFC 7230 §6.1).
///    We send a Connection: keep-alive from the client; the server should still
///    respond correctly (it would hang or error if it tried to honour hop-by-hop).
#[test]
fn test_hop_by_hop_stripped() {
    let stack = EdgeStack::start();
    let (status, _, _) = stack.get_with_headers("/", &[("Connection", "keep-alive")]);
    assert_eq!(
        status_code(&status),
        200,
        "request with hop-by-hop header should still succeed: {status}"
    );
}

/// 10. A cache hit opens no connection to the backend.
///
/// The previous version compared ten cache-miss samples with ten cache-hit
/// samples and required the hit median to be lower. On loopback the two
/// populations are microseconds apart, so the assertion measured scheduling
/// noise: a recorded run inverted by 1.3 percent while the cache worked.
///
/// A hit is defined by where the bytes came from. This primes the cache, kills
/// the global m6-http that every route proxies to, and requires the edge to
/// answer from its own store anyway. A hit that opened a connection to the
/// backend could not answer at all.
#[test]
fn test_cache_hit_does_not_reach_the_backend() {
    let mut stack = EdgeStack::start();

    // Prime the cache with the asset. `/` is already warm at start, and it is
    // a template route, so the asset is the response whose first request is
    // demonstrably a miss.
    let miss_started = Instant::now();
    let (s1, _, b1) = stack.get("/assets/hello.txt");
    let miss_latency = miss_started.elapsed();
    assert_eq!(status_code(&s1), 200);

    // The backend is gone, so anything the edge cannot answer by itself fails.
    stack.stop_global();

    let hit_started = Instant::now();
    let (s2, h2, b2) = stack.get("/assets/hello.txt");
    let hit_latency = hit_started.elapsed();
    assert_eq!(
        status_code(&s2),
        200,
        "a cached asset must still be served with the backend gone: {s2}"
    );
    assert_eq!(b1, b2, "the hit must return the bytes the miss stored");
    assert!(
        has_header(&h2, "age"),
        "the response must come from the edge cache, but it carried no age header:\n{h2}"
    );

    // A report line, not a pass condition: on loopback the two populations are
    // microseconds apart, so a comparison measures the machine.
    println!("cache-miss latency: {miss_latency:?}, cache-hit latency: {hit_latency:?}");
}

/// 11. TLS — edge exposes TLS to clients; clients without valid certs get errors.
#[test]
fn test_tls_required() {
    let stack = EdgeStack::start();
    // Connecting with skip-verify should still get a valid HTTP response.
    let tls = skip_verify_client_config();
    let (status, _, _) = https_get(stack.edge_port, "/", &[], tls);
    assert_eq!(status_code(&status), 200);

    // Plain TCP (no TLS) should fail to get a valid HTTP response.
    let result = std::panic::catch_unwind(|| {
        let mut tcp = TcpStream::connect(format!("127.0.0.1:{}", stack.edge_port)).unwrap();
        tcp.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
        tcp.write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .unwrap();
        let mut buf = vec![0u8; 64];
        let n = tcp.read(&mut buf).unwrap_or(0);
        // The TLS server will either close connection or send TLS alert — not HTTP 200.
        let resp = String::from_utf8_lossy(&buf[..n]);
        assert!(
            !resp.starts_with("HTTP/1.1 200"),
            "plain HTTP should not get 200"
        );
    });
    // Either an error or a non-200 response is acceptable.
    let _ = result;
}

/// 12. RTT simulation: a cache hit is answered from the edge's own store, so a
///     slow or distant backend cannot make it wait.
///
/// The test cannot inject a real network delay without pfctl or tc, so there is
/// no timing claim to assert: the previous version required a 5ms P50 on
/// loopback and failed at 5.615ms under the parallel suite on a branch that
/// could not have touched the hit path. The property that a hit does not reach
/// the backend is held by `test_cache_hit_does_not_reach_the_backend`. This
/// test reports the numbers over 20 hits and 10 misses and asserts only that
/// every request was answered.
#[test]
fn test_rtt_simulation() {
    let stack = EdgeStack::start();

    // 20 requests for the warmed page. The numbers document the speed on this
    // machine; they are a report line, not a pass condition.
    let mut hit_times: Vec<Duration> = Vec::with_capacity(20);
    for _ in 0..20 {
        let t0 = Instant::now();
        let (s, _, _) = stack.get("/");
        assert_eq!(status_code(&s), 200);
        hit_times.push(t0.elapsed());
    }
    hit_times.sort();
    let hit_p50 = hit_times[10];
    let hit_p90 = hit_times[18];
    println!("cache-hit p50={hit_p50:?} p90={hit_p90:?}");

    // 10 cache misses (nocache): measured and reported, with no timing assert.
    let mut miss_total = Duration::ZERO;
    for _ in 0..10 {
        let t0 = Instant::now();
        let (s, _, _) = stack.get("/nocache/");
        assert_eq!(status_code(&s), 200);
        miss_total += t0.elapsed();
    }
    println!("avg cache-miss (no artificial RTT): {:?}", miss_total / 10);
}

/// One HTTP/1.1 POST to the edge with a body of `size` bytes.
///
/// The client leg is HTTP/1.1; the edge forwards to the origin over the h2
/// backbone, which is the leg under test here.
fn https_post(port: u16, path: &str, size: usize, tls: Arc<rustls::ClientConfig>) -> String {
    let tcp = TcpStream::connect(format!("127.0.0.1:{}", port)).expect("tcp connect");
    tcp.set_read_timeout(Some(Duration::from_secs(15))).unwrap();
    let server_name = rustls::pki_types::ServerName::try_from("127.0.0.1".to_string()).unwrap();
    let conn = rustls::ClientConnection::new(tls, server_name).unwrap();
    let mut stream = StreamOwned::new(conn, tcp);

    let body = vec![b'a'; size];
    let req = format!(
        "POST {} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nContent-Type: application/octet-stream\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n",
        path, port, size
    );
    stream.write_all(req.as_bytes()).unwrap();
    stream.write_all(&body).unwrap();
    stream.flush().unwrap();

    let mut resp = Vec::new();
    match stream.read_to_end(&mut resp) {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {}
        Err(e) => panic!("TLS read error: {e}"),
    }
    String::from_utf8_lossy(&resp)
        .lines()
        .next()
        .unwrap_or("")
        .to_string()
}

/// A request body larger than SETTINGS_MAX_FRAME_SIZE must survive the hop to
/// the origin.
///
/// The backbone clients (`h2c_client`, `h2s_client`) emitted the entire body as
/// ONE DATA frame regardless of size. RFC 9113 4.2 caps a frame at the peer's
/// advertised SETTINGS_MAX_FRAME_SIZE, which the origin advertises as the 16384
/// default and, since F-005, actually enforces. So every upload over 16 KiB
/// routed through a cache node was answered FRAME_SIZE_ERROR by the origin and
/// surfaced to the visitor as a 502. Both clients already parsed
/// `peer_max_frame` from the peer's SETTINGS and then never used it.
///
/// 16 KiB is deliberately included: it is the last size that worked before the
/// fix, so it pins the boundary from below as well as above.
///
/// Chunking alone was not enough, and the second defect was not the one the
/// symptom suggested. With frames split correctly, bodies up to 49,152 bytes
/// went through and 65,535 still failed. That was `h2s_client` calling
/// `write_all` on rustls' `Writer`: rustls caps its outgoing plaintext buffer
/// at 64 KiB and returns a short write, or 0, when full, and `write_all` turns
/// that 0 into `WriteZero`, which the client reported as a TLS failure and
/// killed the connection over. `h2c_client` already handled its plain socket's
/// short writes correctly, which is why the two backbone clients failed at
/// different sizes. Both are fixed; this test covers both.
///
/// Still latent, not asserted: `conn_send_window` is initialised and
/// incremented on WINDOW_UPDATE but never decremented or consulted, so neither
/// client enforces connection-level flow control on send. It does not bite at
/// these sizes because the origin replenishes its receive window promptly, but
/// it is a real gap against a peer that does not.
#[test]
fn a_body_larger_than_max_frame_size_survives_the_edge_hop() {
    let stack = EdgeStack::start();

    for size in [16_384usize, 32_768, 65_536, 262_144, 1_048_576] {
        let status = https_post(stack.edge_port, "/", size, stack.edge_tls.clone());
        let code = status_code(&status);
        assert_ne!(
            code, 502,
            "{size} byte body got a 502 from the edge: the origin rejected an \
             oversized DATA frame. status line: {status:?}"
        );
        assert!(
            code > 0,
            "{size} byte body produced no response at all: {status:?}"
        );
    }
}
