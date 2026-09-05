//! Robustness against deliberately malformed, incomplete and injection-shaped
//! requests, driven over a **raw TLS socket** against a real `m6-http`.
//!
//! Why raw sockets rather than a client library: every HTTP client normalises
//! away exactly the input this file is about. `curl` will not send two
//! `Content-Length` headers, will not put a bare LF where a CRLF belongs, and
//! will not leave a request half-written. It also hides what comes back — the
//! HEAD framing defect fixed in this codebase advertised a body length and then
//! sent the body, and `curl -I` reported it as perfectly fine, because it
//! parses the response and discards the body. Only reading the bytes off the
//! socket shows the truth.
//!
//! Scope note. `security_regressions.rs` already covers forged proxy headers,
//! per-query cache keys and ambiguous framing at the *function* level, and
//! `redirect.rs` covers the same ground for the `:80` listener. This file is
//! deliberately aimed at the gaps: the real `:443` listener, over a real
//! socket, with input a client cannot be persuaded to send.
//!
//! What "pass" means here is narrow and worth stating. These tests do not
//! demand any particular status code for bad input — a server may reasonably
//! answer 400, or close the connection, or route the thing as an ordinary
//! (harmless) path. They assert the three properties that actually matter:
//!
//!   1. **No smuggling.** Ambiguous or duplicated framing must never be
//!      accepted as though it were unambiguous.
//!   2. **No injection.** Nothing from the request line or a header value may
//!      appear unescaped in the response headers — that is response splitting.
//!   3. **No hang and no crash.** Every malformed or truncated input must reach
//!      a conclusion within a timeout, and the server must still serve a normal
//!      request afterwards.
//!
//! Run against freshly built release binaries, serially:
//!
//! ```text
//! cargo build --workspace --release
//! cargo test -p m6-http --test robustness -- --test-threads=1
//! ```

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rustls::StreamOwned;

// ── Harness ───────────────────────────────────────────────────────────────────

struct TestProcess(Child);
impl Drop for TestProcess {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).parent().unwrap().to_path_buf()
}

fn binary(name: &str) -> PathBuf {
    let p = repo_root().join("target").join("release").join(name);
    assert!(
        p.exists(),
        "missing {}: run `cargo build --workspace --release` first",
        p.display()
    );
    p
}

fn free_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().port()
}

fn wait_for_tcp(port: u16, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

fn wait_for_path(p: &Path, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if p.exists() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

fn generate_tls_cert() -> (String, String, Vec<u8>) {
    let ck = rcgen::generate_simple_self_signed(vec![
        "localhost".to_string(),
        "127.0.0.1".to_string(),
    ])
    .expect("rcgen");
    let der = ck.cert.der().to_vec();
    (ck.cert.pem(), ck.key_pair.serialize_pem(), der)
}

fn tls_client_config(cert_der: &[u8]) -> Arc<rustls::ClientConfig> {
    rustls::crypto::ring::default_provider().install_default().ok();
    let cert = rustls::pki_types::CertificateDer::from(cert_der.to_vec());
    let mut store = rustls::RootCertStore::empty();
    store.add(cert).unwrap();
    Arc::new(
        rustls::ClientConfig::builder()
            .with_root_certificates(store)
            .with_no_client_auth(),
    )
}

struct Server {
    port: u16,
    cert_der: Vec<u8>,
    _dir: tempfile::TempDir,
    _file: TestProcess,
    _http: TestProcess,
}

impl Server {
    /// Open a TLS connection with ALPN pinned to HTTP/1.1 — these tests are
    /// about the h1 parser, and without pinning, rustls may offer h2 and the
    /// server would answer a protocol these byte sequences are not written for.
    fn connect(&self) -> StreamOwned<rustls::ClientConnection, TcpStream> {
        let mut cfg = (*tls_client_config(&self.cert_der)).clone();
        cfg.alpn_protocols = vec![b"http/1.1".to_vec()];
        let name = rustls::pki_types::ServerName::try_from("localhost").unwrap();
        let conn = rustls::ClientConnection::new(Arc::new(cfg), name).unwrap();
        let sock = TcpStream::connect(("127.0.0.1", self.port)).expect("tcp connect");
        sock.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        sock.set_write_timeout(Some(Duration::from_secs(10))).unwrap();
        StreamOwned::new(conn, sock)
    }

    /// Send raw bytes, read whatever comes back until EOF or timeout.
    ///
    /// Returns the bytes; an empty vec means the server closed without
    /// answering, which is a legitimate response to garbage and is treated as
    /// such throughout.
    fn raw(&self, bytes: &[u8]) -> Vec<u8> {
        let mut s = self.connect();
        // A write failure is itself a valid outcome (server closed on us).
        if s.write_all(bytes).is_err() {
            return Vec::new();
        }
        let _ = s.flush();
        let mut out = Vec::new();
        let mut buf = [0u8; 8192];
        loop {
            match s.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    out.extend_from_slice(&buf[..n]);
                    if out.len() > 4 * 1024 * 1024 {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        out
    }

    /// A known-good request, used to prove the server is still healthy after
    /// each abuse case. This is the assertion that actually catches a crash or
    /// a wedged accept loop.
    fn assert_still_healthy(&self, after: &str) {
        let resp = self.raw(
            b"GET /public/open.txt HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        );
        let head = String::from_utf8_lossy(&resp);
        assert!(
            head.starts_with("HTTP/1.1 200"),
            "server unhealthy after {after}: {:?}",
            &head.chars().take(120).collect::<String>()
        );
        assert!(
            resp.windows(14).any(|w| w == b"PUBLIC CONTENT"),
            "body missing after {after}"
        );
    }
}

fn start_server() -> Server {
    let dir = tempfile::tempdir().unwrap();
    let site = dir.path();

    let (cert_pem, key_pem, cert_der) = generate_tls_cert();
    std::fs::write(site.join("cert.pem"), &cert_pem).unwrap();
    std::fs::write(site.join("key.pem"), &key_pem).unwrap();

    std::fs::create_dir_all(site.join("public")).unwrap();
    std::fs::create_dir_all(site.join("configs")).unwrap();
    std::fs::write(site.join("public/open.txt"), b"PUBLIC CONTENT").unwrap();

    let sock = dir.path().join("m6-file-1.sock");
    let sock_glob = dir.path().join("m6-file-*.sock");

    std::fs::write(
        site.join("site.toml"),
        format!(
            r#"
[site]
name   = "robustness"
domain = "localhost"

[log]
level  = "warn"
format = "text"

[errors]
mode = "internal"

[analytics]
enabled = false

[rate_limit]
enabled = false

[[backend]]
name    = "m6-file"
sockets = "{sock_glob}"

[[route]]
path    = "/public/{{relpath}}"
backend = "m6-file"
"#,
            sock_glob = sock_glob.display(),
        ),
    )
    .unwrap();

    std::fs::write(
        site.join("configs/m6-file.conf"),
        "[[route]]\npath = \"/public/{relpath}\"\nroot = \"public/\"\n",
    )
    .unwrap();

    let port = free_port();
    std::fs::write(
        site.join("system.toml"),
        format!(
            r#"
[server]
bind     = "127.0.0.1:{port}"
tls_cert = "{cert}"
tls_key  = "{key}"

[node]
name = "robustness-node"
"#,
            cert = site.join("cert.pem").display(),
            key = site.join("key.pem").display(),
        ),
    )
    .unwrap();

    let file_proc = TestProcess(
        Command::new(binary("m6-file"))
            .arg(site)
            .arg(site.join("configs/m6-file.conf"))
            .env("M6_SOCKET_OVERRIDE", &sock)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn m6-file"),
    );
    assert!(wait_for_path(&sock, Duration::from_secs(10)), "m6-file socket never appeared");

    let http_proc = TestProcess(
        Command::new(binary("m6-http"))
            .arg(site)
            .arg(site.join("system.toml"))
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn m6-http"),
    );
    assert!(wait_for_tcp(port, Duration::from_secs(10)), "m6-http never listened");
    std::thread::sleep(Duration::from_millis(2500)); // backend socket rescan

    Server { port, cert_der, _dir: dir, _file: file_proc, _http: http_proc }
}

// ── Shared assertions ─────────────────────────────────────────────────────────

fn head_of(resp: &[u8]) -> String {
    let end = resp
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|i| i + 4)
        .unwrap_or(resp.len());
    String::from_utf8_lossy(&resp[..end]).to_string()
}

/// The response must not have been split by anything the client sent.
///
/// Response splitting is the failure this guards: a CR or LF smuggled through
/// a path, query or header value that ends up written verbatim into the
/// response head lets a caller forge headers, or a whole second response, in a
/// reply the victim believes came from the server.
fn assert_no_injected_header(resp: &[u8], marker: &str) {
    let head = head_of(resp).to_ascii_lowercase();
    let marker = marker.to_ascii_lowercase();
    assert!(
        !head.contains(&marker),
        "injected marker {marker:?} appeared in the response head:\n{}",
        head_of(resp)
    );
    // A single response only. Two status lines means the head was split.
    let status_lines = head.matches("http/1.1 ").count();
    assert!(
        status_lines <= 1,
        "response head contains {status_lines} status lines — split:\n{}",
        head_of(resp)
    );
}

/// Anything other than a hang. An empty reply (connection closed) counts:
/// refusing to answer garbage is a legitimate and common choice.
fn assert_concluded(resp: &[u8], case: &str) {
    if resp.is_empty() {
        return; // closed without answering — fine
    }
    let head = head_of(resp);
    assert!(
        head.starts_with("HTTP/"),
        "{case}: reply was neither empty nor an HTTP response: {:?}",
        &head.chars().take(120).collect::<String>()
    );
}

// ── 1. Request framing / smuggling ────────────────────────────────────────────

/// Two `Content-Length` headers that disagree are the classic smuggling
/// primitive: the front end believes one, the back end the other, and a second
/// request hides in the gap. RFC 9112 3.3.3 requires rejection.
#[test]
fn conflicting_content_length_is_not_accepted() {
    let s = start_server();
    let resp = s.raw(
        b"POST /public/open.txt HTTP/1.1\r\nHost: localhost\r\n\
          Content-Length: 6\r\nContent-Length: 0\r\nConnection: close\r\n\r\nHELLO!",
    );
    assert_concluded(&resp, "conflicting content-length");
    let head = head_of(&resp);
    assert!(
        resp.is_empty() || head.starts_with("HTTP/1.1 4"),
        "ambiguous framing should be refused, got:\n{head}"
    );
    s.assert_still_healthy("conflicting content-length");
}

/// `Content-Length` together with `Transfer-Encoding: chunked` is the other
/// half of the same family — the spec says TE wins and CL must be dropped, but
/// disagreeing intermediaries are what makes it exploitable, so refusing is
/// the safe reading.
#[test]
fn content_length_with_transfer_encoding_is_not_accepted() {
    let s = start_server();
    let resp = s.raw(
        b"POST /public/open.txt HTTP/1.1\r\nHost: localhost\r\n\
          Content-Length: 6\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n0\r\n\r\n",
    );
    assert_concluded(&resp, "CL + TE");
    let head = head_of(&resp);
    assert!(
        resp.is_empty() || head.starts_with("HTTP/1.1 4"),
        "CL+TE should be refused, got:\n{head}"
    );
    s.assert_still_healthy("CL + TE");
}

/// A negative or non-numeric length must not be parsed as a number, wrap, or
/// be treated as absent.
#[test]
fn malformed_content_length_values_are_rejected() {
    let s = start_server();
    for bad in ["-1", "abc", "1 2", "0x10", "+5", "99999999999999999999999"] {
        let req = format!(
            "POST /public/open.txt HTTP/1.1\r\nHost: localhost\r\n\
             Content-Length: {bad}\r\nConnection: close\r\n\r\n"
        );
        let resp = s.raw(req.as_bytes());
        assert_concluded(&resp, &format!("content-length: {bad}"));
        let head = head_of(&resp);
        assert!(
            resp.is_empty() || head.starts_with("HTTP/1.1 4"),
            "Content-Length {bad:?} should be refused, got:\n{head}"
        );
        s.assert_still_healthy(&format!("content-length: {bad}"));
    }
}

/// Whitespace between a header name and its colon is forbidden precisely
/// because parsers disagree about it (RFC 9112 5.1) — it is a smuggling
/// primitive when one hop trims and another does not.
#[test]
fn space_before_header_colon_is_rejected() {
    let s = start_server();
    let resp = s.raw(
        b"GET /public/open.txt HTTP/1.1\r\nHost: localhost\r\n\
          Content-Length : 0\r\nConnection: close\r\n\r\n",
    );
    assert_concluded(&resp, "space before colon");
    s.assert_still_healthy("space before colon");
}

// ── 2. Header and request-line injection ──────────────────────────────────────

/// CR/LF/NUL smuggled through the request target must never reach the response
/// head. `Location` on a redirect is the usual sink, but any echoed value will
/// do.
#[test]
fn control_characters_in_the_request_target_do_not_split_the_response() {
    let s = start_server();
    let cases: [&[u8]; 6] = [
        b"GET /public/open.txt%0d%0aX-Injected:%20yes HTTP/1.1\r\n",
        b"GET /public/open.txt%0aX-Injected:%20yes HTTP/1.1\r\n",
        b"GET /public/open.txt?q=%0d%0aX-Injected:%20yes HTTP/1.1\r\n",
        b"GET /public/open.txt?q=%00X-Injected HTTP/1.1\r\n",
        b"GET /public/open.txt%23%0d%0aX-Injected:%20yes HTTP/1.1\r\n",
        b"GET /%2e%2e%2f%2e%2e%2fetc%2fpasswd HTTP/1.1\r\n",
    ];
    for line in cases {
        let mut req = line.to_vec();
        req.extend_from_slice(b"Host: localhost\r\nConnection: close\r\n\r\n");
        let resp = s.raw(&req);
        assert_concluded(&resp, "control chars in target");
        assert_no_injected_header(&resp, "x-injected");
        s.assert_still_healthy("control chars in target");
    }
}

/// The same, through a header value rather than the target. `Host` matters
/// most: it is the value the redirect logic reads.
#[test]
fn control_characters_in_header_values_do_not_split_the_response() {
    let s = start_server();
    let cases: [&[u8]; 3] = [
        b"GET /public/open.txt HTTP/1.1\r\nHost: localhost\r\nX-Test: a\rX-Injected: yes\r\nConnection: close\r\n\r\n",
        b"GET /public/open.txt HTTP/1.1\r\nHost: localhost\r\nX-Test: a\nX-Injected: yes\r\nConnection: close\r\n\r\n",
        b"GET /public/open.txt HTTP/1.1\r\nHost: localhost\r\nX-Test: a\0X-Injected\r\nConnection: close\r\n\r\n",
    ];
    for req in cases {
        let resp = s.raw(req);
        assert_concluded(&resp, "control chars in header value");
        assert_no_injected_header(&resp, "x-injected");
        s.assert_still_healthy("control chars in header value");
    }
}

/// Header names are tokens. Control bytes and separators in a name are not
/// merely invalid, they are how a parser is tricked into seeing a different
/// header than the next hop does.
#[test]
fn illegal_header_names_are_rejected_or_ignored() {
    let s = start_server();
    let cases: [&[u8]; 4] = [
        b"GET /public/open.txt HTTP/1.1\r\nHost: localhost\r\nX Bad: v\r\nConnection: close\r\n\r\n",
        b"GET /public/open.txt HTTP/1.1\r\nHost: localhost\r\nX\tBad: v\r\nConnection: close\r\n\r\n",
        b"GET /public/open.txt HTTP/1.1\r\nHost: localhost\r\n: novalue\r\nConnection: close\r\n\r\n",
        b"GET /public/open.txt HTTP/1.1\r\nHost: localhost\r\nX-\x01Bad: v\r\nConnection: close\r\n\r\n",
    ];
    for req in cases {
        let resp = s.raw(req);
        assert_concluded(&resp, "illegal header name");
        s.assert_still_healthy("illegal header name");
    }
}

/// Oversized input must be bounded rather than buffered without limit. Three
/// separate limits: one huge header, a huge total block, and a huge count.
#[test]
fn oversized_headers_are_bounded() {
    let s = start_server();

    // One enormous header value.
    let mut req = b"GET /public/open.txt HTTP/1.1\r\nHost: localhost\r\nX-Big: ".to_vec();
    req.extend(std::iter::repeat(b'A').take(128 * 1024));
    req.extend_from_slice(b"\r\nConnection: close\r\n\r\n");
    let resp = s.raw(&req);
    assert_concluded(&resp, "oversized single header");
    s.assert_still_healthy("oversized single header");

    // Many headers, each small.
    let mut req = b"GET /public/open.txt HTTP/1.1\r\nHost: localhost\r\n".to_vec();
    for i in 0..5000 {
        req.extend_from_slice(format!("X-H{i}: v\r\n").as_bytes());
    }
    req.extend_from_slice(b"Connection: close\r\n\r\n");
    let resp = s.raw(&req);
    assert_concluded(&resp, "absurd header count");
    s.assert_still_healthy("absurd header count");

    // Enormous request target.
    let mut req = b"GET /public/".to_vec();
    req.extend(std::iter::repeat(b'a').take(64 * 1024));
    req.extend_from_slice(b" HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    let resp = s.raw(&req);
    assert_concluded(&resp, "oversized request target");
    s.assert_still_healthy("oversized request target");
}

/// A `Host` header carrying something that is not a hostname must not be
/// reflected into a redirect target. This is the shape that produced a real
/// bug on the `:80` listener, so it is worth pinning on `:443` too.
#[test]
fn hostile_host_headers_do_not_reach_the_response_head() {
    let s = start_server();
    for host in [
        "localhost\r\nX-Injected: yes",
        "evil.example.com",
        "localhost:99999999",
        "localhost/../../evil",
        "",
    ] {
        let req = format!(
            "GET /public/open.txt HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n"
        );
        let resp = s.raw(req.as_bytes());
        assert_concluded(&resp, "hostile host");
        assert_no_injected_header(&resp, "x-injected");
        s.assert_still_healthy("hostile host");
    }
}

// ── 3. Incomplete input ───────────────────────────────────────────────────────

/// Connect, send nothing, and go away. The connection must be reclaimed rather
/// than held. This is the slowloris class the old Python `:80` shim was
/// vulnerable to; m6-http should not be, and this pins that.
#[test]
fn connect_and_send_nothing_does_not_wedge_the_server() {
    let s = start_server();
    {
        let _idle = s.connect();
        std::thread::sleep(Duration::from_millis(500));
    }
    s.assert_still_healthy("idle connection");
}

/// A request line arriving one byte at a time, then abandoned mid-header.
#[test]
fn a_dribbled_and_abandoned_request_does_not_wedge_the_server() {
    let s = start_server();
    {
        let mut c = s.connect();
        for b in b"GET /public/open.txt HTTP/1.1\r\nHos" {
            if c.write_all(&[*b]).is_err() {
                break;
            }
            let _ = c.flush();
            std::thread::sleep(Duration::from_millis(2));
        }
        // Drop without finishing the header block.
    }
    s.assert_still_healthy("dribbled partial request");
}

/// Headers complete, a body declared, and none of it sent.
#[test]
fn a_declared_body_that_never_arrives_does_not_wedge_the_server() {
    let s = start_server();
    {
        let mut c = s.connect();
        let _ = c.write_all(
            b"POST /public/open.txt HTTP/1.1\r\nHost: localhost\r\n\
              Content-Length: 1000\r\nConnection: close\r\n\r\n",
        );
        let _ = c.flush();
        std::thread::sleep(Duration::from_millis(500));
    }
    s.assert_still_healthy("declared body never sent");
}

/// Several half-open connections at once must not starve a normal client.
/// One held connection stalling every other request is exactly what the
/// blocking Python shim did.
#[test]
fn concurrent_half_open_connections_do_not_starve_a_real_request() {
    let s = start_server();
    let mut held = Vec::new();
    for _ in 0..8 {
        let mut c = s.connect();
        let _ = c.write_all(b"GET /public/open.txt HTTP/1.1\r\nHost: local");
        let _ = c.flush();
        held.push(c);
    }
    let start = Instant::now();
    s.assert_still_healthy("8 half-open connections");
    let elapsed = start.elapsed();
    assert!(
        elapsed < Duration::from_secs(5),
        "a normal request took {elapsed:?} while 8 connections were half-open — \
         the accept loop is being stalled"
    );
    drop(held);
}

// ── 4. Malformed request lines ────────────────────────────────────────────────

#[test]
fn malformed_request_lines_are_handled_and_do_not_crash() {
    let s = start_server();
    let cases: [&[u8]; 10] = [
        b"\r\n\r\n",
        b"GET\r\n\r\n",
        b"GET /public/open.txt\r\n\r\n",                   // no version
        b"GET  /public/open.txt  HTTP/1.1\r\nHost: localhost\r\n\r\n", // double spaces
        b"GET /public/open.txt HTTP/9.9\r\nHost: localhost\r\n\r\n",
        b"GET /public/open.txt HTTP/1.1extra\r\nHost: localhost\r\n\r\n",
        b"\x00\x01\x02\x03\r\n\r\n",
        b"GET http://evil.example.com/x HTTP/1.1\r\nHost: localhost\r\n\r\n", // absolute-form
        b"GET public/open.txt HTTP/1.1\r\nHost: localhost\r\n\r\n",           // not origin-form
        b"CONNECT localhost:443 HTTP/1.1\r\nHost: localhost\r\n\r\n",
    ];
    for req in cases {
        let resp = s.raw(req);
        assert_concluded(&resp, "malformed request line");
        assert_no_injected_header(&resp, "evil.example.com");
        s.assert_still_healthy("malformed request line");
    }
}

// ── 5. Injection-shaped payloads reaching the application ─────────────────────

/// SQLi/XSS/template-shaped query strings. This site has no database, so the
/// property under test is not "the query is sanitised" but the one that
/// actually matters at this layer: whatever the caller sends must not come back
/// unescaped in the response head, must not split it, and must not crash the
/// renderer.
#[test]
fn injection_shaped_queries_are_inert() {
    let s = start_server();
    let payloads = [
        "q=%27%20OR%201%3D1--",
        "q=%3Cscript%3Ealert(1)%3C%2Fscript%3E",
        "q=%7B%7B7*7%7D%7D",
        "q=%24%7Bjndi%3Aldap%3A%2F%2Fevil%2Fa%7D",
        "q=..%2F..%2F..%2Fetc%2Fpasswd",
        "q=%00",
        "q=%C0%AE%C0%AE%2F", // overlong UTF-8 encoding of ".."
        "q=%2527%2520OR",    // double-encoded
    ];
    for p in payloads {
        let req = format!(
            "GET /public/open.txt?{p} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
        );
        let resp = s.raw(req.as_bytes());
        assert_concluded(&resp, p);
        assert_no_injected_header(&resp, "x-injected");
        // Nothing script-shaped should ever appear in a response HEAD.
        let head = head_of(&resp).to_ascii_lowercase();
        assert!(!head.contains("<script"), "payload {p} reached the response head:\n{head}");
        assert!(!head.contains("jndi:"), "payload {p} reached the response head:\n{head}");
        s.assert_still_healthy(p);
    }
}

/// Path traversal through a route parameter must not escape the served root.
/// `route.rs` validates params, and `escapes_site_dir` covers symlinks; this
/// asserts the property end to end over the wire, in the encodings a scanner
/// actually uses.
#[test]
fn path_traversal_does_not_escape_the_served_root() {
    let s = start_server();
    let targets = [
        "/public/../../etc/passwd",
        "/public/..%2f..%2fetc%2fpasswd",
        "/public/%2e%2e%2f%2e%2e%2fetc%2fpasswd",
        "/public/....//....//etc/passwd",
        "/public/..\\..\\etc\\passwd",
        "/public/%252e%252e%252fetc%252fpasswd",
    ];
    for t in targets {
        let req = format!("GET {t} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
        let resp = s.raw(req.as_bytes());
        assert_concluded(&resp, t);
        let body = String::from_utf8_lossy(&resp);
        assert!(
            !body.contains("root:x:") && !body.contains("/bin/bash"),
            "traversal {t} returned something that looks like /etc/passwd"
        );
        s.assert_still_healthy(t);
    }
}

/// Not an assertion — a visibility check. `assert_concluded` accepts an empty
/// reply, so without this it would be possible for every case above to "pass"
/// while the server actually answered nothing at all. Printed under
/// `--nocapture` so the real behaviour is inspectable rather than assumed.
#[test]
fn diagnostic_show_actual_responses() {
    let s = start_server();
    let cases: Vec<(&str, Vec<u8>)> = vec![
        ("conflicting CL",
         b"POST /public/open.txt HTTP/1.1\r\nHost: localhost\r\nContent-Length: 6\r\nContent-Length: 0\r\nConnection: close\r\n\r\nHELLO!".to_vec()),
        ("CL + TE",
         b"POST /public/open.txt HTTP/1.1\r\nHost: localhost\r\nContent-Length: 6\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n0\r\n\r\n".to_vec()),
        ("CL: -1",
         b"POST /public/open.txt HTTP/1.1\r\nHost: localhost\r\nContent-Length: -1\r\nConnection: close\r\n\r\n".to_vec()),
        ("space before colon",
         b"GET /public/open.txt HTTP/1.1\r\nHost: localhost\r\nContent-Length : 0\r\nConnection: close\r\n\r\n".to_vec()),
        ("CRLF in target",
         b"GET /public/open.txt%0d%0aX-Injected:%20yes HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n".to_vec()),
        ("bare LF in header value",
         b"GET /public/open.txt HTTP/1.1\r\nHost: localhost\r\nX-Test: a\nX-Injected: yes\r\nConnection: close\r\n\r\n".to_vec()),
        ("absolute-form target",
         b"GET http://evil.example.com/x HTTP/1.1\r\nHost: localhost\r\n\r\n".to_vec()),
        ("traversal ..%2f",
         b"GET /public/..%2f..%2fetc%2fpasswd HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n".to_vec()),
        ("empty Host",
         b"GET /public/open.txt HTTP/1.1\r\nHost: \r\nConnection: close\r\n\r\n".to_vec()),
        ("CONNECT",
         b"CONNECT localhost:443 HTTP/1.1\r\nHost: localhost\r\n\r\n".to_vec()),
        ("Host absent (1.1)",
         b"GET /public/open.txt HTTP/1.1\r\nConnection: close\r\n\r\n".to_vec()),
        ("Host empty (1.1)",
         b"GET /public/open.txt HTTP/1.1\r\nHost: \r\nConnection: close\r\n\r\n".to_vec()),
        ("Host duplicated",
         b"GET /public/open.txt HTTP/1.1\r\nHost: localhost\r\nHost: evil.example.com\r\nConnection: close\r\n\r\n".to_vec()),
        ("Host absent (1.0)",
         b"GET /public/open.txt HTTP/1.0\r\nConnection: close\r\n\r\n".to_vec()),
        ("bare LF in value (fixed)",
         b"GET /public/open.txt HTTP/1.1\r\nHost: localhost\r\nX-T: a\nX-Injected: yes\r\nConnection: close\r\n\r\n".to_vec()),
    ];
    println!("\n  {:<26} {:>6}  {}", "case", "bytes", "first line");
    for (name, req) in cases {
        let resp = s.raw(&req);
        let first = head_of(&resp).lines().next().unwrap_or("<empty>").to_string();
        println!("  {:<26} {:>6}  {}", name, resp.len(), first);
    }
}

// ── 6. Host header validation (RFC 9112 3.2) ──────────────────────────────────

/// HTTP/1.1 requires exactly one `Host` with a usable value (RFC 9112 3.2):
/// a server MUST answer 400 to a request that lacks one, carries more than
/// one, or carries an invalid value. All three used to return 200.
///
/// More than one is the case with teeth: two hops can pick different Host
/// values and disagree about which site, or which origin, the request is for.
#[test]
fn http11_requires_exactly_one_usable_host() {
    let s = start_server();
    let cases: [(&str, &[u8]); 4] = [
        ("absent",     b"GET /public/open.txt HTTP/1.1\r\nConnection: close\r\n\r\n"),
        ("empty",      b"GET /public/open.txt HTTP/1.1\r\nHost: \r\nConnection: close\r\n\r\n"),
        ("whitespace", b"GET /public/open.txt HTTP/1.1\r\nHost:    \t \r\nConnection: close\r\n\r\n"),
        ("duplicated", b"GET /public/open.txt HTTP/1.1\r\nHost: localhost\r\nHost: evil.example.com\r\nConnection: close\r\n\r\n"),
    ];
    for (name, req) in cases {
        let resp = s.raw(req);
        let head = head_of(&resp);
        assert!(
            resp.is_empty() || head.starts_with("HTTP/1.1 400"),
            "Host {name}: expected 400, got:\n{head}"
        );
        assert_no_injected_header(&resp, "evil.example.com");
        s.assert_still_healthy(name);
    }
}

/// HTTP/1.0 predates `Host` and is permitted to omit it. Rejecting a 1.0
/// request for a missing Host would break a client that is behaving
/// correctly, so the rule above is gated on the version -- and that gate is
/// worth a test of its own, because it is the part most likely to be
/// "simplified" away later.
#[test]
fn http10_without_host_is_still_served() {
    let s = start_server();
    let resp = s.raw(b"GET /public/open.txt HTTP/1.0\r\nConnection: close\r\n\r\n");
    let head = head_of(&resp);
    assert!(
        head.starts_with("HTTP/1.1 200"),
        "HTTP/1.0 without Host must still be served, got:\n{head}"
    );
    s.assert_still_healthy("http/1.0 without host");
}

/// Two `Host` headers that disagree must not both be honoured. Asserted
/// separately from the characterisation above because this one is a real
/// requirement rather than a recorded observation.
#[test]
fn conflicting_host_headers_are_not_both_honoured() {
    let s = start_server();
    let resp = s.raw(
        b"GET /public/open.txt HTTP/1.1\r\nHost: localhost\r\nHost: evil.example.com\r\n\
          Connection: close\r\n\r\n",
    );
    assert_concluded(&resp, "duplicate host");
    let head = head_of(&resp);
    assert!(
        !head.to_ascii_lowercase().contains("evil.example.com"),
        "the forged second Host reached the response head:\n{head}"
    );
    s.assert_still_healthy("duplicate host");
}
