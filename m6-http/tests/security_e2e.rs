//! End-to-end security regression tests — real `m6-http` + `m6-file`
//! processes, real TLS, real HTTP/1.1 and HTTP/3 clients.
//!
//! `security_regressions.rs` proves each flaw at the level of the function
//! that contains it. This file proves the consequence: what an unauthenticated
//! attacker actually receives over the network.
//!
//! Each test asserts the **secure** behaviour for one finding. Each failed
//! when written and passes now that the finding is fixed.
//!
//! These spawn real processes and bind real ports, so run them serially and
//! against freshly built release binaries:
//!
//! ```text
//! cargo build --workspace --release
//! cargo test -p m6-http --test security_e2e -- --test-threads=1
//! ```

use std::io::{Read, Write};
use std::net::{TcpStream, UdpSocket};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use quiche::h3::NameValue as _;
use rustls::StreamOwned;

use m6_core::testkit::{binary, claim_port, PortClaim, Service};

// ── Crypto fixtures ───────────────────────────────────────────────────────────

/// Self-signed TLS cert for 127.0.0.1/localhost.
fn generate_tls_cert() -> (String, String, Vec<u8>) {
    let ck = rcgen::generate_simple_self_signed(vec![
        "localhost".to_string(),
        "127.0.0.1".to_string(),
    ])
    .expect("rcgen");
    let der = ck.cert.der().to_vec();
    (ck.cert.pem(), ck.key_pair.serialize_pem(), der)
}

/// ES256 keypair for JWT signing. Returns (private_pem, public_pem).
fn generate_jwt_keypair() -> (String, String) {
    let kp = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).expect("ec keypair");
    (kp.serialize_pem(), kp.public_key_pem())
}

/// Mint an ES256 JWT carrying `groups`, valid for one hour.
fn mint_jwt(private_pem: &str, sub: &str, groups: &[&str]) -> String {
    #[derive(serde::Serialize)]
    struct Claims {
        sub: String,
        iss: String,
        exp: u64,
        groups: Vec<String>,
        roles: Vec<String>,
    }
    let exp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 3600;
    let claims = Claims {
        sub: sub.to_string(),
        iss: "m6-auth".to_string(),
        exp,
        groups: groups.iter().map(|s| s.to_string()).collect(),
        roles: vec![],
    };
    let key = jsonwebtoken::EncodingKey::from_ec_pem(private_pem.as_bytes()).expect("ec key");
    let header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::ES256);
    jsonwebtoken::encode(&header, &claims, &key).expect("sign jwt")
}

// ── HTTP/1.1 client ───────────────────────────────────────────────────────────

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

struct HttpResponse {
    status: u16,
    headers: String,
    body: Vec<u8>,
}

impl HttpResponse {
    /// Case-insensitive header lookup over the raw header block.
    fn has_header(&self, name: &str) -> bool {
        let want = format!("{}:", name.to_ascii_lowercase());
        self.headers
            .split("\r\n")
            .skip(1)
            .any(|l| l.to_ascii_lowercase().starts_with(&want))
    }
}

/// One HTTP/1.1-over-TLS GET. A fresh connection each call (the server sends
/// `Connection: close`).
fn https_get(
    srv: &Server,
    path: &str,
    extra: &[(&str, &str)],
    tls: Arc<rustls::ClientConfig>,
) -> HttpResponse {
    let port = srv.port;
    let tcp = srv.tcp();
    tcp.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let name = rustls::pki_types::ServerName::try_from("127.0.0.1".to_string()).unwrap();
    let conn = rustls::ClientConnection::new(tls, name).unwrap();
    let mut stream = StreamOwned::new(conn, tcp);

    let mut req = format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n");
    for (k, v) in extra {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str("Connection: close\r\n\r\n");
    stream.write_all(req.as_bytes()).unwrap();
    stream.flush().unwrap();

    let mut raw = Vec::new();
    match stream.read_to_end(&mut raw) {
        Ok(_) => {}
        // Peer closed without close_notify — normal for Connection: close.
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {}
        Err(e) => panic!("TLS read error: {e}"),
    }

    let text = String::from_utf8_lossy(&raw);
    let end = text.find("\r\n\r\n").unwrap_or(raw.len());
    let headers = text[..end].to_string();
    let status = headers
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let body = if end + 4 <= raw.len() { raw[end + 4..].to_vec() } else { Vec::new() };
    HttpResponse { status, headers, body }
}

// ── HTTP/3 client ─────────────────────────────────────────────────────────────

fn quic_flush(conn: &mut quiche::Connection, udp: &UdpSocket, out: &mut [u8]) {
    loop {
        match conn.send(out) {
            Ok((n, info)) => {
                let _ = udp.send_to(&out[..n], info.to);
            }
            Err(_) => break,
        }
    }
}

/// Issue `count` sequential HTTP/3 GETs for `path` over a single QUIC
/// connection. Returns one status per request.
fn h3_get_many(port: u16, path: &str, count: usize) -> Result<Vec<u16>, String> {
    let server_addr: std::net::SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    let udp = UdpSocket::bind("127.0.0.1:0").map_err(|e| e.to_string())?;
    udp.set_nonblocking(true).unwrap();
    let local = udp.local_addr().unwrap();

    let mut config = quiche::Config::new(quiche::PROTOCOL_VERSION).map_err(|e| e.to_string())?;
    config
        .set_application_protos(quiche::h3::APPLICATION_PROTOCOL)
        .map_err(|e| e.to_string())?;
    config.set_max_idle_timeout(10_000);
    config.set_initial_max_data(10_000_000);
    config.set_initial_max_stream_data_bidi_local(1_000_000);
    config.set_initial_max_stream_data_bidi_remote(1_000_000);
    config.set_initial_max_stream_data_uni(1_000_000);
    config.set_initial_max_streams_bidi(1000);
    config.set_initial_max_streams_uni(100);
    config.grease(false);
    config.verify_peer(false);

    let scid_bytes = [9u8; quiche::MAX_CONN_ID_LEN];
    let scid = quiche::ConnectionId::from_ref(&scid_bytes);
    let mut conn = quiche::connect(Some("localhost"), &scid, local, server_addr, &mut config)
        .map_err(|e| format!("connect: {e}"))?;

    let mut h3: Option<quiche::h3::Connection> = None;
    let mut buf = vec![0u8; 65536];
    let mut out = vec![0u8; 1350];

    let mut statuses: Vec<u16> = Vec::new();
    let mut sent = 0usize;
    let deadline = Instant::now() + Duration::from_secs(60);

    loop {
        if Instant::now() > deadline {
            return Err(format!(
                "h3 timeout: sent={sent} got={} established={}",
                statuses.len(),
                conn.is_established()
            ));
        }

        conn.on_timeout();
        quic_flush(&mut conn, &udp, &mut out);

        // Drain inbound datagrams.
        loop {
            match udp.recv_from(&mut buf) {
                Ok((n, from)) => {
                    let info = quiche::RecvInfo { from, to: local };
                    if conn.recv(&mut buf[..n], info).is_err() {
                        break;
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) => return Err(format!("recv: {e}")),
            }
        }

        if conn.is_closed() {
            return Err(format!("connection closed after {} responses", statuses.len()));
        }

        if conn.is_established() && h3.is_none() {
            let cfg = quiche::h3::Config::new().map_err(|e| e.to_string())?;
            h3 = Some(
                quiche::h3::Connection::with_transport(&mut conn, &cfg)
                    .map_err(|e| format!("h3 init: {e}"))?,
            );
        }

        if let Some(ref mut h3c) = h3 {
            // Keep one request in flight at a time so each response is
            // unambiguously attributable and the server's per-request path
            // (including any rate-limit check) runs sequentially.
            if sent == statuses.len() && sent < count {
                let headers = vec![
                    quiche::h3::Header::new(b":method", b"GET"),
                    quiche::h3::Header::new(b":path", path.as_bytes()),
                    quiche::h3::Header::new(b":scheme", b"https"),
                    quiche::h3::Header::new(b":authority", b"localhost"),
                ];
                match h3c.send_request(&mut conn, &headers, true) {
                    Ok(_) => sent += 1,
                    Err(quiche::h3::Error::Done) => {}
                    Err(e) => return Err(format!("send_request #{sent}: {e}")),
                }
            }

            loop {
                match h3c.poll(&mut conn) {
                    Ok((_, quiche::h3::Event::Headers { list, .. })) => {
                        for h in &list {
                            if h.name() == b":status" {
                                let s = std::str::from_utf8(h.value()).unwrap_or("0");
                                if let Ok(code) = s.parse::<u16>() {
                                    // Ignore 1xx informational (103 Early Hints).
                                    if code >= 200 {
                                        statuses.push(code);
                                    }
                                }
                            }
                        }
                    }
                    Ok((sid, quiche::h3::Event::Data)) => {
                        let mut tmp = [0u8; 4096];
                        while let Ok(n) = h3c.recv_body(&mut conn, sid, &mut tmp) {
                            if n == 0 {
                                break;
                            }
                        }
                    }
                    Ok(_) => {}
                    Err(quiche::h3::Error::Done) => break,
                    Err(e) => return Err(format!("poll: {e}")),
                }
            }
        }

        quic_flush(&mut conn, &udp, &mut out);

        if statuses.len() >= count {
            break;
        }
        std::thread::sleep(Duration::from_millis(1));
    }

    Ok(statuses)
}

// ── Site fixture ──────────────────────────────────────────────────────────────

struct Server {
    port: u16,
    cert_der: Vec<u8>,
    /// JWT signed for a member of `admins`.
    admin_jwt: String,
    http: std::cell::RefCell<Service>,
    file: Service,
    _dir: tempfile::TempDir,
    _port: PortClaim,
}

impl Server {
    fn tls(&self) -> Arc<rustls::ClientConfig> {
        tls_client_config(&self.cert_der)
    }

    /// Open a TCP connection to the server, or say why it could not be opened.
    ///
    /// `ConnectionRefused` means nothing is listening, which means m6-http
    /// died since the last request. Asking the service reports its exit status
    /// and its own last words instead of a bare io error that names neither.
    fn tcp(&self) -> TcpStream {
        match TcpStream::connect(("127.0.0.1", self.port)) {
            Ok(s) => s,
            Err(e) => {
                self.http.borrow_mut().assert_alive("the client was connecting");
                panic!("connect to 127.0.0.1:{} failed with m6-http alive: {e}", self.port);
            }
        }
    }

    /// Kill the m6-file backend. Afterwards only cache hits can be served —
    /// anything reaching the backend pool fails.
    fn kill_backend(&mut self) {
        self.file.kill();
        // Let m6-http notice the socket has gone (2s rescan interval).
        std::thread::sleep(Duration::from_millis(2500));
    }
}

/// Bring up `m6-file` + `m6-http` over a site with:
///   - `/public/{relpath}`  → open
///   - `/private/{relpath}` → `require = "group:admins"`
///
/// `rate_limit_per_min` is written into `site.toml` verbatim.
fn start_server(rate_limit_per_min: u32) -> Server {
    let dir = tempfile::tempdir().unwrap();
    let site = dir.path();

    let (cert_pem, key_pem, cert_der) = generate_tls_cert();
    std::fs::write(site.join("cert.pem"), &cert_pem).unwrap();
    std::fs::write(site.join("key.pem"), &key_pem).unwrap();

    let (jwt_priv, jwt_pub) = generate_jwt_keypair();
    std::fs::write(site.join("auth-public.pem"), &jwt_pub).unwrap();
    let admin_jwt = mint_jwt(&jwt_priv, "admin-user", &["admins"]);

    // Content.
    std::fs::create_dir_all(site.join("public")).unwrap();
    std::fs::create_dir_all(site.join("private")).unwrap();
    std::fs::create_dir_all(site.join("configs")).unwrap();
    std::fs::write(site.join("public/open.txt"), b"PUBLIC CONTENT").unwrap();
    std::fs::write(site.join("private/secret.txt"), b"TOP SECRET").unwrap();

    let sock = dir.path().join("m6-file-1.sock");
    let sock_glob = dir.path().join("m6-file-*.sock");

    std::fs::write(
        site.join("site.toml"),
        format!(
            r#"
[site]
name   = "sec-e2e"
domain = "localhost"

[log]
level  = "warn"
format = "text"

[errors]
mode = "internal"

[analytics]
enabled = false

[rate_limit]
enabled         = true
requests_per_min = {rate_limit_per_min}

[auth]
backend    = "m6-file"
public_key = "auth-public.pem"

[[backend]]
name    = "m6-file"
sockets = "{sock_glob}"

[[route]]
path    = "/public/{{relpath}}"
backend = "m6-file"

[[route]]
path    = "/private/{{relpath}}"
backend = "m6-file"
require = "group:admins"
"#,
            sock_glob = sock_glob.display(),
        ),
    )
    .unwrap();

    std::fs::write(
        site.join("configs/m6-file.conf"),
        r#"
[[route]]
path = "/public/{relpath}"
root = "public/"

[[route]]
path = "/private/{relpath}"
root = "private/"
"#,
    )
    .unwrap();

    let claim = claim_port();
    let port = claim.port();
    std::fs::write(
        site.join("system.toml"),
        format!(
            r#"
[server]
bind     = "127.0.0.1:{port}"
tls_cert = "{cert}"
tls_key  = "{key}"

[node]
name = "test-node"
"#,
            cert = site.join("cert.pem").display(),
            key = site.join("key.pem").display(),
        ),
    )
    .unwrap();

    let mut file_proc = Service::spawn(
        "m6-file",
        Command::new(binary("m6-file"))
            .arg(site)
            .arg(site.join("configs/m6-file.conf"))
            .env("M6_SOCKET_OVERRIDE", &sock),
    );
    file_proc.wait_for_path(&sock, Duration::from_secs(10));

    let mut http_proc = Service::spawn(
        "m6-http",
        Command::new(binary("m6-http")).arg(site).arg(site.join("system.toml")),
    );
    http_proc.wait_for_tcp(port, Duration::from_secs(10));
    // m6-http discovers backend sockets by periodic rescan (2s interval).
    std::thread::sleep(Duration::from_millis(2500));

    Server {
        port,
        cert_der,
        admin_jwt,
        http: std::cell::RefCell::new(http_proc),
        file: file_proc,
        _dir: dir,
        _port: claim,
    }
}

// ── Finding 1 (e2e): cache serves protected content to anonymous clients ─────

/// The full exploit. An authorised request warms the cache for a
/// `require`-protected path; a subsequent request with **no credentials at
/// all** is answered from that cache entry, because the cache lookup at
/// `main.rs:328` runs before the auth check at `main.rs:1038`.
///
/// Reachable because m6-file stamps `Cache-Control: public` on every response
/// (`m6-file/src/handler.rs:104-107`), so protected files are admitted to a
/// cache keyed only on `(path, encoding)` (`cache.rs:30`).
///
/// Property: an unauthenticated request must never receive protected content,
/// cache state notwithstanding.
#[test]
fn finding_1_e2e_anonymous_client_must_not_read_protected_content_from_cache() {
    let srv = start_server(100_000);

    // Sanity: without a token the route is properly refused on a cold cache.
    let cold = https_get(&srv, "/private/secret.txt", &[], srv.tls());
    assert_ne!(
        cold.status, 200,
        "cold-cache anonymous request should never succeed (got {}), \
         body={:?}",
        cold.status,
        String::from_utf8_lossy(&cold.body)
    );

    // An authorised user fetches it, warming the cache.
    let authed = https_get(
        &srv,
        "/private/secret.txt",
        &[("Cookie", &format!("session={}", srv.admin_jwt))],
        srv.tls(),
    );
    assert_eq!(
        authed.status, 200,
        "authorised request should succeed; headers:\n{}",
        authed.headers
    );
    assert_eq!(&authed.body[..], b"TOP SECRET");

    // The same anonymous request as before — now served from cache.
    let anon = https_get(&srv, "/private/secret.txt", &[], srv.tls());

    assert_ne!(
        &anon.body[..],
        b"TOP SECRET",
        "protected content leaked to an unauthenticated client via the cache"
    );
    assert_ne!(
        anon.status, 200,
        "anonymous client received HTTP 200 for a protected path after the \
         cache was warmed"
    );
}

// ── Finding 3 (e2e): rate limiting does not apply to HTTP/3 ──────────────────

/// `check_rate_limit` is wired into the HTTP/1.1 (`main.rs:312`) and h2c
/// (`main.rs:394`) closures only. `handle_h3_request` (`main.rs:743`) never
/// calls it, so an attacker who speaks HTTP/3 — which every response
/// advertises via `alt-svc` — is not throttled at all.
///
/// Both halves run against the same server with the same tiny limit, so the
/// only variable is the protocol.
///
/// Property: the configured per-IP limit must apply on every protocol.
#[test]
fn finding_3_e2e_rate_limit_must_apply_to_http3() {
    const LIMIT: u32 = 5;
    let srv = start_server(LIMIT);
    let requests = (LIMIT as usize) * 4;

    // Control: HTTP/1.1 is throttled.
    let mut h1_statuses = Vec::new();
    for _ in 0..requests {
        h1_statuses.push(https_get(&srv, "/public/open.txt", &[], srv.tls()).status);
    }
    let h1_throttled = h1_statuses.iter().filter(|&&s| s == 429).count();
    assert!(
        h1_throttled > 0,
        "expected HTTP/1.1 to be rate limited past {LIMIT}/min; statuses: {h1_statuses:?}"
    );

    // Same limit, same path, different protocol.
    let h3_statuses = h3_get_many(srv.port, "/public/open.txt", requests)
        .expect("http/3 requests should complete");
    let h3_throttled = h3_statuses.iter().filter(|&&s| s == 429).count();

    assert!(
        h3_throttled > 0,
        "{requests} HTTP/3 requests against a {LIMIT}/min limit produced zero \
         429s, while {h1_throttled}/{requests} were throttled over HTTP/1.1 on \
         the same server. HTTP/3 statuses: {h3_statuses:?}"
    );
}

// ── Finding 4 (e2e): ?_nocache is an unauthenticated cache bypass ────────────

/// Original defect: `main.rs` honoured a magic `?_nocache` query parameter that
/// any anonymous client could set to skip the cache and force a full backend
/// round trip — a one-token switch that disabled the server's main capacity
/// defence.
///
/// The parameter has been removed. `_nocache` now carries no special meaning:
/// it is an ordinary query string like any other.
///
/// Cache state is made observable by killing the backend once the cache is
/// warm — with no backend, only a cache hit can succeed.
///
/// Property: `_nocache` receives no privileged treatment. Specifically, the
/// cached path still serves from cache, and `_nocache` behaves exactly like an
/// arbitrary unknown parameter.
///
/// Note on residual risk: because the cache key now includes the query string
/// (finding 5), *any* novel query is a cache miss — `?a=1`, `?a=2`, … This is
/// inherent to query-correct caching and is the same for every CDN; it is
/// bounded by the per-IP rate limiter, which since finding 3 covers every
/// protocol. What this test pins down is that no single well-known token gets
/// to skip the cache on an otherwise-cacheable path.
#[test]
fn finding_4_e2e_nocache_query_param_is_not_privileged() {
    let mut srv = start_server(100_000);

    // Warm the cache for the bare path.
    let first = https_get(&srv, "/public/open.txt", &[], srv.tls());
    assert_eq!(first.status, 200, "headers:\n{}", first.headers);
    assert_eq!(&first.body[..], b"PUBLIC CONTENT");

    // From here on, only the cache can serve a request.
    srv.kill_backend();

    // The cached path is still served — there is no global cache-disable.
    let cached = https_get(&srv, "/public/open.txt", &[], srv.tls());
    assert_eq!(
        cached.status, 200,
        "the cache should still serve this path with the backend down; \
         headers:\n{}",
        cached.headers
    );
    assert_eq!(&cached.body[..], b"PUBLIC CONTENT");

    // `_nocache` must be indistinguishable from any other unknown parameter.
    let magic = https_get(&srv, "/public/open.txt?_nocache", &[], srv.tls());
    let arbitrary = https_get(&srv, "/public/open.txt?_zzz=1", &[], srv.tls());

    assert_eq!(
        magic.status, arbitrary.status,
        "`_nocache` is still treated specially: it returned {} while an \
         arbitrary parameter returned {}",
        magic.status, arbitrary.status
    );

    // And the cached entry survives both — neither evicted nor poisoned it.
    let after = https_get(&srv, "/public/open.txt", &[], srv.tls());
    assert_eq!(after.status, 200, "headers:\n{}", after.headers);
    assert_eq!(
        &after.body[..],
        b"PUBLIC CONTENT",
        "a query-bearing request must not disturb the cached bare path"
    );
}

// ── Finding 10 (e2e): no security response headers ───────────────────────────

/// m6-http never adds HSTS, `X-Content-Type-Options`, frame protection, or a
/// CSP to any response.
///
/// Property: an internet-facing TLS proxy should set these centrally. Trim the
/// list to whatever policy you settle on — this is the one finding whose
/// "correct" set is a judgement call rather than a fixed requirement.
#[test]
fn finding_10_e2e_security_headers_must_be_present() {
    let srv = start_server(100_000);
    let resp = https_get(&srv, "/public/open.txt", &[], srv.tls());
    assert_eq!(resp.status, 200, "headers:\n{}", resp.headers);

    let missing: Vec<&str> = [
        "strict-transport-security",
        "x-content-type-options",
        "x-frame-options",
        "content-security-policy",
        "referrer-policy",
    ]
    .into_iter()
    .filter(|h| !resp.has_header(h))
    .collect();

    assert!(
        missing.is_empty(),
        "missing security headers: {missing:?}\nFull header block:\n{}",
        resp.headers
    );
}

// ── HTTP/2 raw client: the RFC 9113 4.2 frame-size boundary ──────────────────

const H2_PREFACE: &[u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";

fn h2_frame(ftype: u8, flags: u8, stream_id: u32, payload: &[u8]) -> Vec<u8> {
    let n = payload.len();
    let mut v = Vec::with_capacity(9 + n);
    v.push((n >> 16) as u8);
    v.push((n >> 8) as u8);
    v.push(n as u8);
    v.push(ftype);
    v.push(flags);
    v.extend_from_slice(&stream_id.to_be_bytes());
    v.extend_from_slice(payload);
    v
}

/// HPACK "literal header field without indexing", name taken from the static
/// table, value as a raw (un-Huffman'd) string. Enough to build one request
/// without pulling in an encoder. Values here are always shorter than 127
/// bytes, so the length is a single octet.
fn hpack_literal(static_index: u8, value: &str) -> Vec<u8> {
    let mut v = vec![static_index & 0x0f];
    v.push(value.len() as u8);
    v.extend_from_slice(value.as_bytes());
    v
}

fn tls_client_config_h2(cert_der: &[u8]) -> Arc<rustls::ClientConfig> {
    rustls::crypto::ring::default_provider().install_default().ok();
    let cert = rustls::pki_types::CertificateDer::from(cert_der.to_vec());
    let mut store = rustls::RootCertStore::empty();
    store.add(cert).unwrap();
    let mut cfg = rustls::ClientConfig::builder()
        .with_root_certificates(store)
        .with_no_client_auth();
    cfg.alpn_protocols = vec![b"h2".to_vec()];
    Arc::new(cfg)
}

/// A DATA frame of exactly 2^14 octets must produce a response, not a dropped
/// socket.
///
/// RFC 9113 4.2: every endpoint MUST receive and minimally process a frame up
/// to 2^14 octets, and 2^14 is the SETTINGS_MAX_FRAME_SIZE m6 advertises.
///
/// The defect: rustls caps received plaintext at a fixed 16 KiB and signals
/// backpressure by failing `read_tls` with "received plaintext buffer full".
/// `fill_recv` in http2.rs mapped that onto a dead connection and went to
/// `Phase::Done`, closing without even a GOAWAY. Every HTTP/2 request carrying
/// a body of 2^14 bytes or more died mid-flight, and the peer got no reason
/// why. `advance_tls` in http11.rs had handled this correctly for HTTP/1.1 all
/// along; the fix was simply never ported across.
///
/// This lives here, not in http2.rs, because it needs real TLS: the parser-level
/// test in http2.rs feeds `recv_buf` directly, never trips the plaintext cap,
/// and passed happily throughout while h2spec http2/4.2/1 failed.
#[test]
fn h2_data_frame_at_max_frame_size_must_get_a_response() {
    let srv = start_server(100_000);

    let tcp = srv.tcp();
    tcp.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let name = rustls::pki_types::ServerName::try_from("127.0.0.1".to_string()).unwrap();
    let conn =
        rustls::ClientConnection::new(tls_client_config_h2(&srv.cert_der), name).unwrap();
    let mut s = StreamOwned::new(conn, tcp);

    let authority = format!("127.0.0.1:{}", srv.port);
    // :method POST (static 3), :scheme https (static 7), then :authority and
    // :path as literals. Pseudo-headers first, as RFC 9113 8.3 requires.
    let mut block = vec![0x83, 0x87];
    block.extend(hpack_literal(1, &authority));
    block.extend(hpack_literal(4, "/public/open.txt"));

    let mut out = Vec::new();
    out.extend_from_slice(H2_PREFACE);
    out.extend(h2_frame(0x04, 0, 0, &[]));                    // SETTINGS
    out.extend(h2_frame(0x01, 0x04, 1, &block));              // HEADERS, END_HEADERS
    out.extend(h2_frame(0x00, 0x01, 1, &vec![0u8; 16_384]));  // DATA 2^14, END_STREAM
    s.write_all(&out).unwrap();
    s.flush().unwrap();

    // Read until a HEADERS frame (type 0x01) appears, or the peer goes away.
    // Before the fix the socket just closed, which is exactly what this catches.
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut saw_headers = false;

    while Instant::now() < deadline && !saw_headers {
        match s.read(&mut tmp) {
            Ok(0) => break,
            Ok(n) => buf.extend_from_slice(&tmp[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(_) => break,
        }
        let mut i = 0;
        while i + 9 <= buf.len() {
            let len =
                ((buf[i] as usize) << 16) | ((buf[i + 1] as usize) << 8) | buf[i + 2] as usize;
            if i + 9 + len > buf.len() {
                break;
            }
            if buf[i + 3] == 0x01 {
                saw_headers = true;
            }
            i += 9 + len;
        }
    }

    assert!(
        saw_headers,
        "no HEADERS frame came back: the server dropped the connection on a legal \
         2^14-octet DATA frame. RFC 9113 4.2 requires it to be accepted \
         (h2spec http2/4.2/1)."
    );
}
