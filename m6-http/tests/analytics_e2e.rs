//! End-to-end regression test for the analytics/session-cookie chokepoint
//! refactor (see plan: unify per-response analytics/cookie logic).
//!
//! This is the regression detector for the original bug: a single request
//! getting two different `Set-Cookie` headers with two different random
//! session IDs, caused by duplicated (and non-idempotent) session-minting
//! logic across 5 separate call sites in `main.rs`. It must pass identically
//! before and after every step of that refactor.
//!
//! Real `m6-http` + `m6-file` processes, real TLS, real HTTP/1.1 and HTTP/3
//! clients — same harness shape as `security_e2e.rs`.
//!
//! ```text
//! cargo build --workspace --release
//! cargo test -p m6-http --test analytics_e2e -- --test-threads=1
//! ```

use std::io::{Read, Write};
use std::net::{TcpStream, UdpSocket};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use quiche::h3::NameValue as _;
use rustls::StreamOwned;

use m6_core::testkit::{binary, claim_port, PortClaim, Service};

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

struct HttpResponse {
    status: u16,
    headers: String,
}

impl HttpResponse {
    /// All values of a header (case-insensitive name), in wire order —
    /// deliberately plural, since the whole point of this test is to catch
    /// duplicate `Set-Cookie` headers.
    fn header_values(&self, name: &str) -> Vec<&str> {
        let want = format!("{}:", name.to_ascii_lowercase());
        self.headers
            .split("\r\n")
            .skip(1)
            .filter(|l| l.to_ascii_lowercase().starts_with(&want))
            .map(|l| l[want.len()..].trim())
            .collect()
    }
}

/// A response that never arrived, carrying the reason in place of headers.
///
/// **Every transport failure in these helpers funnels here instead of
/// panicking, and that is the whole point.** They are called from
/// `wait::until(30s, || https_get(..).status == 200)` readiness loops whose
/// entire job is to retry until m6-http has filled its backend pool. A panic
/// inside that closure does not retry, it ends the test.
///
/// That is what failed on 2026-09-12: `write_all(..).unwrap()` met a
/// `ConnectionReset` and the run died at
/// `custom_error_page_fetch_is_logged_to_analytics`, inside the loop written
/// to tolerate exactly that. `wait_for_tcp` proves something is bound, which
/// is not the same as being ready to serve, and a server that accepts and
/// closes before reading sends RST rather than FIN, so the client sees the
/// reset on its *next write* rather than on connect.
///
/// The reason is kept rather than discarded: `status` is already 0 for an
/// unparseable response, so the readiness loop treats this as "not yet", while
/// a final assertion prints the transport error instead of an unwrap panic
/// with no context.
fn no_response(reason: impl std::fmt::Display) -> HttpResponse {
    HttpResponse { status: 0, headers: format!("<no response: {reason}>") }
}

/// One HTTP/1.1-over-TLS GET against a bare port.
///
/// Three tests here build ad-hoc stacks that have no [`Server`], so the
/// primitive takes a port. Tests that do have a `Server` should go through
/// [`Server::get`], which reports a dead service instead of a bare io error.
///
/// Never panics on a transport failure. See [`no_response`].
fn https_get(port: u16, path: &str, extra: &[(&str, &str)], tls: Arc<rustls::ClientConfig>) -> HttpResponse {
    match TcpStream::connect(("127.0.0.1", port)) {
        Ok(tcp) => https_exchange(tcp, port, path, extra, tls),
        Err(e) => no_response(format!("tcp connect: {e}")),
    }
}

fn https_exchange(
    tcp: TcpStream,
    port: u16,
    path: &str,
    extra: &[(&str, &str)],
    tls: Arc<rustls::ClientConfig>,
) -> HttpResponse {
    if let Err(e) = tcp.set_read_timeout(Some(Duration::from_secs(10))) {
        return no_response(format!("set_read_timeout: {e}"));
    }
    let name = rustls::pki_types::ServerName::try_from("127.0.0.1".to_string()).unwrap();
    let conn = match rustls::ClientConnection::new(tls, name) {
        Ok(c) => c,
        Err(e) => return no_response(format!("tls setup: {e}")),
    };
    let mut stream = StreamOwned::new(conn, tcp);

    let mut req = format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n");
    for (k, v) in extra {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str("Connection: close\r\n\r\n");
    // The handshake is driven by this write, so a server that is bound but not
    // yet serving surfaces here rather than at connect.
    if let Err(e) = stream.write_all(req.as_bytes()) {
        return no_response(format!("write request: {e}"));
    }
    if let Err(e) = stream.flush() {
        return no_response(format!("flush request: {e}"));
    }

    let mut raw = Vec::new();
    match stream.read_to_end(&mut raw) {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {}
        Err(e) => return no_response(format!("tls read: {e}")),
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
    HttpResponse { status, headers }
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

/// One HTTP/3 GET, optionally carrying a `cookie` request header. Returns
/// `(status, all set-cookie header values in wire order)`.
fn h3_get(port: u16, path: &str, cookie: Option<&str>) -> Result<(u16, Vec<String>), String> {
    let server_addr: std::net::SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    let udp = UdpSocket::bind("127.0.0.1:0").map_err(|e| e.to_string())?;
    udp.set_nonblocking(true).unwrap();
    let local = udp.local_addr().unwrap();

    let mut config = quiche::Config::new(quiche::PROTOCOL_VERSION).map_err(|e| e.to_string())?;
    config.set_application_protos(quiche::h3::APPLICATION_PROTOCOL).map_err(|e| e.to_string())?;
    config.set_max_idle_timeout(10_000);
    config.set_initial_max_data(10_000_000);
    config.set_initial_max_stream_data_bidi_local(1_000_000);
    config.set_initial_max_stream_data_bidi_remote(1_000_000);
    config.set_initial_max_stream_data_uni(1_000_000);
    config.set_initial_max_streams_bidi(1000);
    config.set_initial_max_streams_uni(100);
    config.grease(false);
    config.verify_peer(false);

    let scid_bytes = [7u8; quiche::MAX_CONN_ID_LEN];
    let scid = quiche::ConnectionId::from_ref(&scid_bytes);
    let mut conn = quiche::connect(Some("localhost"), &scid, local, server_addr, &mut config)
        .map_err(|e| format!("connect: {e}"))?;

    let mut h3: Option<quiche::h3::Connection> = None;
    let mut buf = vec![0u8; 65536];
    let mut out = vec![0u8; 1350];
    let mut sent = false;
    let mut status: Option<u16> = None;
    let mut set_cookies: Vec<String> = Vec::new();
    let mut done = false;
    let deadline = Instant::now() + Duration::from_secs(30);

    loop {
        if Instant::now() > deadline {
            return Err(format!("h3 timeout: sent={sent} status={status:?}"));
        }
        conn.on_timeout();
        quic_flush(&mut conn, &udp, &mut out);

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
            return Err("connection closed before response".to_string());
        }

        if conn.is_established() && h3.is_none() {
            let cfg = quiche::h3::Config::new().map_err(|e| e.to_string())?;
            h3 = Some(quiche::h3::Connection::with_transport(&mut conn, &cfg).map_err(|e| format!("h3 init: {e}"))?);
        }

        if let Some(ref mut h3c) = h3 {
            if !sent {
                let mut headers = vec![
                    quiche::h3::Header::new(b":method", b"GET"),
                    quiche::h3::Header::new(b":path", path.as_bytes()),
                    quiche::h3::Header::new(b":scheme", b"https"),
                    quiche::h3::Header::new(b":authority", b"localhost"),
                ];
                if let Some(c) = cookie {
                    headers.push(quiche::h3::Header::new(b"cookie", c.as_bytes()));
                }
                match h3c.send_request(&mut conn, &headers, true) {
                    Ok(_) => sent = true,
                    Err(quiche::h3::Error::Done) => {}
                    Err(e) => return Err(format!("send_request: {e}")),
                }
            }

            loop {
                match h3c.poll(&mut conn) {
                    Ok((_, quiche::h3::Event::Headers { list, .. })) => {
                        for h in &list {
                            let name = std::str::from_utf8(h.name()).unwrap_or("");
                            let value = std::str::from_utf8(h.value()).unwrap_or("");
                            if name == ":status" {
                                if let Ok(code) = value.parse::<u16>() {
                                    if code >= 200 {
                                        status = Some(code);
                                    }
                                }
                            } else if name.eq_ignore_ascii_case("set-cookie") {
                                set_cookies.push(value.to_string());
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
                    Ok((_, quiche::h3::Event::Finished)) => done = true,
                    Ok(_) => {}
                    Err(quiche::h3::Error::Done) => break,
                    Err(e) => return Err(format!("poll: {e}")),
                }
            }
        }

        quic_flush(&mut conn, &udp, &mut out);
        if done && status.is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(1));
    }

    Ok((status.ok_or("no status received")?, set_cookies))
}

// ── Analytics log parsing ─────────────────────────────────────────────────────

#[derive(Debug)]
struct AnalyticsLine {
    cache_state: String,
    session_id: String,
    session_new: bool,
    // Parsed for completeness / debug output; no test currently asserts on it.
    #[allow(dead_code)]
    client_ip: String,
    path: String,
}

fn read_analytics_lines(path: &Path) -> Vec<AnalyticsLine> {
    let content = std::fs::read_to_string(path).unwrap_or_default();
    content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|line| {
            let v: serde_json::Value = serde_json::from_str(line).ok()?;
            let fields = v.get("fields")?;
            if fields.get("message")?.as_str()? != "request" {
                return None;
            }
            Some(AnalyticsLine {
                cache_state: fields.get("cache_state")?.as_str()?.to_string(),
                session_id: fields.get("session_id")?.as_str()?.to_string(),
                session_new: fields.get("session_new")?.as_bool()?,
                client_ip: fields.get("client_ip")?.as_str()?.to_string(),
                path: fields.get("path")?.as_str()?.to_string(),
            })
        })
        .collect()
}

// ── Site fixture ──────────────────────────────────────────────────────────────

struct Server {
    port: u16,
    cert_der: Vec<u8>,
    analytics_log: PathBuf,
    http: std::cell::RefCell<Service>,
    _file: Service,
    _dir: tempfile::TempDir,
    _port: PortClaim,
}

impl Server {
    fn tls(&self) -> Arc<rustls::ClientConfig> {
        tls_client_config(&self.cert_der)
    }

    /// Open a TCP connection, or `None` if the server is not accepting yet.
    ///
    /// **A refused connect does not mean the server died**, which is what this
    /// used to assume before panicking. `assert_alive` is still checked first
    /// and still fails loudly, with the exit status and stderr, when the
    /// process really is gone. What is left over is the other case, and it is
    /// the common one: m6-http is alive and has not bound the listener yet.
    /// There is a real window for it, because the port is held by a
    /// [`PortClaim`] that is released so m6-http can bind it.
    ///
    /// On 2026-09-12 that window produced
    /// `connect to 127.0.0.1:22143 failed with m6-http alive: Connection
    /// refused`, inside `wait_until_serving`, the loop whose entire job is to
    /// wait for the server to come up. The panic message contained its own
    /// refutation: it said the server was alive.
    fn tcp(&self) -> Option<TcpStream> {
        match TcpStream::connect(("127.0.0.1", self.port)) {
            Ok(s) => Some(s),
            Err(_) => {
                // Fatal and self-reporting when the process is actually dead.
                self.http.borrow_mut().assert_alive("the client was connecting");
                None
            }
        }
    }

    /// One HTTP/1.1-over-TLS GET against this server.
    fn get(&self, path: &str, extra: &[(&str, &str)]) -> HttpResponse {
        match self.tcp() {
            Some(tcp) => https_exchange(tcp, self.port, path, extra, self.tls()),
            None => no_response("tcp connect refused while m6-http was alive"),
        }
    }

    /// Wait until a request actually reaches the backend, then clear the log.
    ///
    /// Listening on the port is not the same as being able to serve: m6-http
    /// fills its backend pool from a periodic rescan, so there is a window
    /// where every request is a 502. This was `sleep(2500)`, a number tuned on
    /// a fast laptop, and on the slower Linux build box it was not enough:
    /// five tests here failed with 502 against an empty pool.
    ///
    /// The probe requests are real, so they are logged. Every test in this file
    /// counts analytics lines and assumes it starts from an empty log, so the
    /// log is truncated once the server is ready. The settle is for
    /// tracing-appender, which writes on its own thread.
    fn wait_until_serving(&self) {
        let ready = m6_core::testkit::wait::until(Duration::from_secs(30), || {
            self.get("/public/probe.html", &[]).status == 200
        });
        assert!(
            ready,
            "m6-http never served a backend request\n--- output ---\n{}",
            self.http.borrow().output()
        );
        std::thread::sleep(Duration::from_millis(250));
        std::fs::write(&self.analytics_log, b"").expect("truncate analytics log");
    }
}

/// Bring up `m6-file` + `m6-http` with analytics enabled (the default —
/// `[analytics].enabled` defaults to `true`, unlike `security_e2e.rs`'s
/// fixture which explicitly disables it) and a single cacheable public file.
fn start_server() -> Server {
    let dir = tempfile::tempdir().unwrap();
    let site = dir.path();

    let (cert_pem, key_pem, cert_der) = generate_tls_cert();
    std::fs::write(site.join("cert.pem"), &cert_pem).unwrap();
    std::fs::write(site.join("key.pem"), &key_pem).unwrap();

    std::fs::create_dir_all(site.join("public")).unwrap();
    std::fs::create_dir_all(site.join("configs")).unwrap();
    std::fs::write(site.join("public/open.txt"), b"PUBLIC CONTENT").unwrap();
    // An HTML page referencing a preloadable asset — used by the prefetch
    // suppression test to trigger a real hint-driven prefetch.
    std::fs::write(
        site.join("public/page.html"),
        br#"<html><head><link rel="stylesheet" href="/public/style.css"></head><body>hi</body></html>"#,
    )
    .unwrap();
    std::fs::write(site.join("public/style.css"), b"body { color: red; }").unwrap();
    // A second HTML page, referencing nothing — the session tests need a
    // text/html response (only those mint a session, per the HTTP-06 fix) but
    // must not drag page.html's prefetch hints into their own line counts.
    std::fs::write(site.join("public/plain.html"), b"<!doctype html><title>hi</title>").unwrap();
    // Used only as the readiness probe, so no test's path is warmed by it.
    // Probing a path a test then asserts on turns its first request into a
    // cache HIT, and `session_cookie_minted_once_and_reused_h3` asserts MISS.
    std::fs::write(site.join("public/probe.html"), b"<!doctype html><title>probe</title>").unwrap();

    let sock = dir.path().join("m6-file-1.sock");
    let sock_glob = dir.path().join("m6-file-*.sock");
    let analytics_log = dir.path().join("analytics.ndjson");

    std::fs::write(
        site.join("site.toml"),
        format!(
            r#"
[site]
name   = "analytics-e2e"
domain = "localhost"

[log]
level  = "warn"
format = "text"

[errors]
mode = "internal"

[analytics]
log_path = "{log_path}"

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
            log_path = analytics_log.display(),
        ),
    )
    .unwrap();

    std::fs::write(
        site.join("configs/m6-file.conf"),
        r#"
[[route]]
path = "/public/{relpath}"
root = "public/"
"#,
    )
    .unwrap();

    let port_claim = claim_port();
    let port = port_claim.port();
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
        Command::new(binary("m6-http"))
            .arg(site)
            .arg(site.join("system.toml")),
    );
    http_proc.wait_for_tcp(port, Duration::from_secs(10));

    let srv = Server {
        port,
        cert_der,
        analytics_log,
        http: std::cell::RefCell::new(http_proc),
        _file: file_proc,
        _dir: dir,
        _port: port_claim,
    };
    srv.wait_until_serving();
    srv
}

// ── Tests ──────────────────────────────────────────────────────────────────────

/// The core regression test: a request with no session cookie gets exactly
/// one `Set-Cookie`; replaying with that cookie gets none, and reuses the
/// same session ID. This is the property that a duplicated, non-idempotent
/// mint-or-reuse call (the original bug) would violate.
#[test]
fn session_cookie_minted_once_and_reused_h1() {
    let srv = start_server();

    let first = srv.get("/public/plain.html", &[]);
    assert_eq!(first.status, 200, "headers:\n{}", first.headers);
    let set_cookies = first.header_values("set-cookie");
    assert_eq!(
        set_cookies.len(),
        1,
        "expected exactly one Set-Cookie on a request with no existing session, got {}: {:?}\nfull headers:\n{}",
        set_cookies.len(),
        set_cookies,
        first.headers
    );
    let cookie_value = set_cookies[0].split(';').next().unwrap(); // "_m6sid=<id>"
    let session_id = cookie_value.split_once('=').unwrap().1;

    // Replay with that cookie: no new Set-Cookie, same session reused.
    let second = srv.get("/public/plain.html", &[("Cookie", cookie_value)]);
    assert_eq!(second.status, 200);
    assert!(
        second.header_values("set-cookie").is_empty(),
        "a request already carrying a valid session cookie must not get a new Set-Cookie, got: {:?}",
        second.header_values("set-cookie")
    );

    std::thread::sleep(Duration::from_millis(200)); // let the log writer flush
    let lines = read_analytics_lines(&srv.analytics_log);
    assert_eq!(lines.len(), 2, "expected exactly 2 analytics lines, got {}: {lines:?}", lines.len());
    assert!(lines[0].session_new, "first request's line should have session_new=true");
    assert_eq!(lines[0].session_id, session_id);
    assert!(!lines[1].session_new, "second (replayed-cookie) request's line should have session_new=false");
    assert_eq!(lines[1].session_id, session_id, "second request should log the SAME session id, not a fresh one");
}

/// Same property over HTTP/3 — the protocol whose analytics code path is
/// structurally different (hand-rolled header scan / HeaderSource impl over
/// raw quiche::h3::Header, rather than an owned Vec<(String,String)>).
#[test]
fn session_cookie_minted_once_and_reused_h3() {
    let srv = start_server();

    let (status1, set_cookies1) = h3_get(srv.port, "/public/plain.html", None).expect("h3 request 1");
    assert_eq!(status1, 200);
    assert_eq!(
        set_cookies1.len(),
        1,
        "expected exactly one set-cookie on H3 request with no existing session, got {}: {:?}",
        set_cookies1.len(),
        set_cookies1
    );
    let cookie_value = set_cookies1[0].split(';').next().unwrap();
    let session_id = cookie_value.split_once('=').unwrap().1;

    let (status2, set_cookies2) = h3_get(srv.port, "/public/plain.html", Some(cookie_value)).expect("h3 request 2");
    assert_eq!(status2, 200);
    assert!(
        set_cookies2.is_empty(),
        "H3 request already carrying a valid session cookie must not get a new set-cookie, got: {:?}",
        set_cookies2
    );

    std::thread::sleep(Duration::from_millis(200));
    let lines = read_analytics_lines(&srv.analytics_log);
    assert_eq!(lines.len(), 2, "expected exactly 2 analytics lines, got {}: {lines:?}", lines.len());
    assert_eq!(lines[0].cache_state, "MISS"); // first hit of this path, nothing cached yet
    assert!(lines[0].session_new);
    assert_eq!(lines[0].session_id, session_id);
    assert!(!lines[1].session_new);
    assert_eq!(lines[1].session_id, session_id);
}

/// Bug-fix (a): a hint-driven prefetch (triggered by requesting an HTML page
/// that references a preloadable asset) must not appear in analytics at all —
/// it's a synthetic internal request with a fabricated client_ip, not a real
/// visit, and logging it would mint a throwaway session and corrupt
/// request/session counts downstream.
#[test]
fn prefetched_assets_are_not_logged_to_analytics() {
    let srv = start_server();

    let page = srv.get("/public/page.html", &[]);
    assert_eq!(page.status, 200, "headers:\n{}", page.headers);

    // The prefetch queue drains one entry per event-loop iteration, but the
    // loop only wakes on real I/O — a plain sleep() doesn't advance it, since
    // nothing is connecting while we wait. Poll with repeated lightweight
    // requests (each one both a possible wake trigger and a check) instead of
    // guessing a fixed delay.
    let mut prefetch_ran = false;
    for _ in 0..20 {
        let css = srv.get("/public/style.css", &[]);
        assert_eq!(css.status, 200);
        if css.status == 200 {
            prefetch_ran = true; // the file is servable either way; this loop exists to pump the event loop
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(prefetch_ran);

    std::thread::sleep(Duration::from_millis(200)); // let the log writer flush

    // This test's own client also connects via 127.0.0.1 (loopback), so
    // client_ip can't distinguish "the prefetch's synthetic request" from
    // "this test's real requests" — the discriminator has to be the total
    // line count instead. Real requests issued: 1 for page.html + 20 for the
    // polling loop above = 21. A prefetch — a synthetic, response-discarded
    // internal warm-up triggered by page.html's hints — logging itself as a
    // 22nd line would mean bug (a) has regressed.
    let lines = read_analytics_lines(&srv.analytics_log);
    assert_eq!(
        lines.len(),
        21,
        "expected exactly 21 analytics lines (the 21 real requests this test made: \
         1 page.html + 20 style.css polls), got {}: {lines:?} — an extra line means \
         the prefetch was logged",
        lines.len()
    );
    assert!(lines.iter().all(|l| l.path == "/public/page.html" || l.path == "/public/style.css"));
    // At least one of the style.css fetches should have been served from
    // cache once the prefetch (which this loop exists to give a chance to
    // run) completed — confirms the prefetch actually happened, not just
    // that it wasn't logged (a test that never actually exercised the code
    // path wouldn't be much of a regression guard).
    assert!(
        lines.iter().any(|l| l.path == "/public/style.css" && l.cache_state == "HIT"),
        "expected at least one style.css fetch to be a cache HIT (warmed by the prefetch): {lines:?}"
    );
}

/// Bug-fix (b): a backend-returned 4xx/5xx on the socket-backend path must
/// still be logged to analytics, same as any other request. `m6-file`
/// returning a plain 404 for a missing file is exactly that case: a real,
/// visible-to-a-real-client response that the pre-fix code silently skipped
/// logging, for every error status uniformly, with no stated reason.
#[test]
fn backend_4xx_is_logged_to_analytics() {
    let srv = start_server();

    let missing = srv.get("/public/does-not-exist.txt", &[]);
    assert_eq!(missing.status, 404, "headers:\n{}", missing.headers);

    std::thread::sleep(Duration::from_millis(400));
    let lines = read_analytics_lines(&srv.analytics_log);
    assert_eq!(lines.len(), 1, "expected exactly 1 analytics line for the 404, got {}: {lines:?}", lines.len());
    assert_eq!(lines[0].path, "/public/does-not-exist.txt");
}

/// A minimal plain-HTTP/1.1 server that answers every request with a fixed
/// 200 body — stands in for a URL-backend error-page renderer, so
/// `[errors] mode = "custom"` can be exercised without a second real m6
/// process. Runs for the test process's lifetime; no cleanup needed.
fn spawn_fake_url_backend() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            std::thread::spawn(move || {
                let mut stream = stream;
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf); // don't care about the request contents
                let body = b"CUSTOM ERROR PAGE";
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(resp.as_bytes());
                let _ = stream.write_all(body);
            });
        }
    });
    port
}

/// Bug-fix (c): a `[errors] mode = "custom"` error-page fetch belongs to a
/// real client's real failing request — skipping analytics here (as the
/// pre-fix code did, unconditionally) meant any node running this mode had
/// zero visibility into who was hitting error pages, which is the entire
/// point of the feature. Exercises the real async-dispatch path via a
/// minimal fake URL backend standing in for the error-page renderer.
#[test]
fn custom_error_page_fetch_is_logged_to_analytics() {
    let dir = tempfile::tempdir().unwrap();
    let site = dir.path();

    let (cert_pem, key_pem, cert_der) = generate_tls_cert();
    std::fs::write(site.join("cert.pem"), &cert_pem).unwrap();
    std::fs::write(site.join("key.pem"), &key_pem).unwrap();
    std::fs::create_dir_all(site.join("public")).unwrap();
    std::fs::create_dir_all(site.join("configs")).unwrap();
    std::fs::write(site.join("public/open.txt"), b"PUBLIC CONTENT").unwrap();

    let error_backend_port = spawn_fake_url_backend();

    let sock = dir.path().join("m6-file-1.sock");
    let sock_glob = dir.path().join("m6-file-*.sock");
    let analytics_log = dir.path().join("analytics.ndjson");

    std::fs::write(
        site.join("site.toml"),
        format!(
            r#"
[site]
name   = "analytics-e2e-custom-error"
domain = "localhost"

[log]
level  = "warn"
format = "text"

[errors]
mode = "custom"
path = "/_error_page"

[analytics]
log_path = "{log_path}"

[rate_limit]
enabled = false

[[backend]]
name    = "m6-file"
sockets = "{sock_glob}"

[[backend]]
name = "error-backend"
url  = "http://127.0.0.1:{error_backend_port}"

[[route]]
path    = "/public/{{relpath}}"
backend = "m6-file"

[[route]]
path    = "/_error_page"
backend = "error-backend"
"#,
            sock_glob = sock_glob.display(),
            log_path = analytics_log.display(),
        ),
    )
    .unwrap();

    std::fs::write(
        site.join("configs/m6-file.conf"),
        r#"
[[route]]
path = "/public/{relpath}"
root = "public/"
"#,
    )
    .unwrap();

    let port_claim = claim_port();
    let port = port_claim.port();
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

    let mut _file_proc = Service::spawn(
        "m6-file",
        Command::new(binary("m6-file"))
            .arg(site)
            .arg(site.join("configs/m6-file.conf"))
            .env("M6_SOCKET_OVERRIDE", &sock),
    );
    _file_proc.wait_for_path(&sock, Duration::from_secs(10));

    let mut _http_proc = Service::spawn(
        "m6-http",
        Command::new(binary("m6-http"))
            .arg(site)
            .arg(site.join("system.toml")),
    );
    _http_proc.wait_for_tcp(port, Duration::from_secs(10));

    let tls = tls_client_config(&cert_der);

    // Wait until the backend pool is actually populated, then clear the log.
    // See Server::wait_until_serving for why a fixed sleep is not enough; this
    // stack has no Server, so the same poll is spelled out.
    let ready = m6_core::testkit::wait::until(Duration::from_secs(30), || {
        https_get(port, "/public/open.txt", &[], tls.clone()).status == 200
    });
    assert!(
        ready,
        "m6-http never served a backend request\n--- output ---\n{}",
        _http_proc.output()
    );
    std::thread::sleep(Duration::from_millis(250));
    std::fs::write(&analytics_log, b"").expect("truncate analytics log");

    // A 404 on a route with no match — dispatches the custom error page
    // fetch via dispatch_custom_error_async, exercising the exact branch
    // this bug fix touches.
    let missing = https_get(port, "/does-not-exist-anywhere", &[], tls);
    assert_eq!(missing.status, 404, "headers:\n{}", missing.headers);

    std::thread::sleep(Duration::from_millis(400));
    let lines = read_analytics_lines(&analytics_log);
    assert_eq!(
        lines.len(), 1,
        "expected exactly 1 analytics line for the custom-error-page fetch, got {}: {lines:?}",
        lines.len()
    );
    assert_eq!(lines[0].path, "/does-not-exist-anywhere");
    assert_eq!(lines[0].cache_state, "MISS");
}

/// Bug-fix (d): a genuine connection failure to a URL backend (the backend
/// is unreachable — the 502/504 a real client actually receives) must still
/// be logged to analytics. Arguably the most operationally important case of
/// the four bugs fixed in this refactor: a backend-down incident should show
/// up in the request log, not just an operational warn!().
#[test]
fn connection_failure_is_logged_to_analytics() {
    let dir = tempfile::tempdir().unwrap();
    let site = dir.path();

    let (cert_pem, key_pem, cert_der) = generate_tls_cert();
    std::fs::write(site.join("cert.pem"), &cert_pem).unwrap();
    std::fs::write(site.join("key.pem"), &key_pem).unwrap();
    std::fs::create_dir_all(site.join("configs")).unwrap();

    // A port nobody is listening on — bind-then-drop to get a free one that
    // will refuse connections deterministically.
    let dead_port_claim = claim_port();
    let dead_port = dead_port_claim.port();
    let analytics_log = dir.path().join("analytics.ndjson");

    std::fs::write(
        site.join("site.toml"),
        format!(
            r#"
[site]
name   = "analytics-e2e-conn-failure"
domain = "localhost"

[log]
level  = "warn"
format = "text"

[errors]
mode = "internal"

[analytics]
log_path = "{log_path}"

[rate_limit]
enabled = false

[[backend]]
name = "unreachable-backend"
url  = "http://127.0.0.1:{dead_port}"

[[route]]
path    = "/proxy/{{relpath}}"
backend = "unreachable-backend"
"#,
            log_path = analytics_log.display(),
        ),
    )
    .unwrap();

    let port_claim = claim_port();
    let port = port_claim.port();
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

    let mut _http_proc = Service::spawn(
        "m6-http",
        Command::new(binary("m6-http"))
            .arg(site)
            .arg(site.join("system.toml")),
    );
    _http_proc.wait_for_tcp(port, Duration::from_secs(10));
    std::thread::sleep(Duration::from_millis(1500));

    let tls = tls_client_config(&cert_der);
    let resp = https_get(port, "/proxy/anything", &[], tls);
    assert!(resp.status >= 500, "expected a 5xx for an unreachable backend, got {}: {}", resp.status, resp.headers);

    std::thread::sleep(Duration::from_millis(400));
    let lines = read_analytics_lines(&analytics_log);
    assert_eq!(
        lines.len(), 1,
        "expected exactly 1 analytics line for the connection-failure response, got {}: {lines:?}",
        lines.len()
    );
    assert_eq!(lines[0].path, "/proxy/anything");
    assert_eq!(lines[0].cache_state, "MISS");
}

/// The bug this test guards against was found live, in production, via this
/// exact topology: a two-tier deployment where an "edge" m6-http's only
/// backend is another m6-http instance ("origin"), exactly like a real cache
/// node relaying to the real origin over H2C (approximated here with a plain
/// TLS URL backend, since the H1 client harness in this file can't speak
/// H2C). Before the fix, hitting the edge produced TWO different Set-Cookie
/// headers with two different session ids — the edge derived "no session"
/// from the original cookie-less client request and minted its own, on top
/// of the one origin had already minted and returned.
#[test]
fn edge_reuses_origin_session_instead_of_minting_a_second_one() {
    // ── Origin: real content backend (m6-file), analytics enabled ──────────
    let origin_dir = tempfile::tempdir().unwrap();
    let origin_site = origin_dir.path();
    // Cert DER unused: the edge talks to origin with tls_skip_verify = true,
    // not by pinning this cert (that's for THIS test's own client, if it
    // ever needed to hit origin directly, which it doesn't).
    let (origin_cert_pem, origin_key_pem, _origin_cert_der) = generate_tls_cert();
    std::fs::write(origin_site.join("cert.pem"), &origin_cert_pem).unwrap();
    std::fs::write(origin_site.join("key.pem"), &origin_key_pem).unwrap();
    std::fs::create_dir_all(origin_site.join("public")).unwrap();
    std::fs::create_dir_all(origin_site.join("configs")).unwrap();
    // HTML, not text: sessions are only minted on text/html responses (the
    // HTTP-06 fix — a session cookie on every image and stylesheet was both
    // noise and a needless per-asset identifier). A .txt fixture here would
    // exercise the no-mint path and never see the bug this test guards.
    std::fs::write(origin_site.join("public/page.html"), b"<!doctype html><title>x</title>")
        .unwrap();
    // A second asset used only as the readiness probe. It must not be the
    // path under test: probing through the edge would populate the edge cache,
    // the real request would then be a HIT, the origin would never see it, and
    // the assertion that both nodes logged a line would fail for a reason that
    // has nothing to do with sessions.
    std::fs::write(origin_site.join("public/probe.html"), b"<!doctype html><title>probe</title>")
        .unwrap();

    let origin_sock = origin_dir.path().join("m6-file-1.sock");
    let origin_sock_glob = origin_dir.path().join("m6-file-*.sock");
    let origin_analytics_log = origin_dir.path().join("analytics.ndjson");

    std::fs::write(
        origin_site.join("site.toml"),
        format!(
            r#"
[site]
name   = "origin"
domain = "localhost"

[log]
level  = "warn"
format = "text"

[errors]
mode = "internal"

[analytics]
log_path = "{log_path}"

[rate_limit]
enabled = false

[[backend]]
name    = "m6-file"
sockets = "{sock_glob}"

[[route]]
path    = "/public/{{relpath}}"
backend = "m6-file"
"#,
            sock_glob = origin_sock_glob.display(),
            log_path = origin_analytics_log.display(),
        ),
    )
    .unwrap();
    std::fs::write(
        origin_site.join("configs/m6-file.conf"),
        "[[route]]\npath = \"/public/{relpath}\"\nroot = \"public/\"\n",
    )
    .unwrap();

    let origin_port_claim = claim_port();
    let origin_port = origin_port_claim.port();
    std::fs::write(
        origin_site.join("system.toml"),
        format!(
            "\n[server]\nbind     = \"127.0.0.1:{origin_port}\"\ntls_cert = \"{cert}\"\ntls_key  = \"{key}\"\n\n[node]\nname = \"origin\"\n",
            cert = origin_site.join("cert.pem").display(),
            key = origin_site.join("key.pem").display(),
        ),
    )
    .unwrap();

    let mut _origin_file_proc = Service::spawn(
        "origin m6-file",
        Command::new(binary("m6-file"))
            .arg(origin_site)
            .arg(origin_site.join("configs/m6-file.conf"))
            .env("M6_SOCKET_OVERRIDE", &origin_sock),
    );
    _origin_file_proc.wait_for_path(&origin_sock, Duration::from_secs(10));

    let mut _origin_http_proc = Service::spawn(
        "origin m6-http",
        Command::new(binary("m6-http"))
            .arg(origin_site)
            .arg(origin_site.join("system.toml")),
    );
    _origin_http_proc.wait_for_tcp(origin_port, Duration::from_secs(10));

    // ── Edge: its only backend IS the origin's m6-http ─────────────────────
    let edge_dir = tempfile::tempdir().unwrap();
    let edge_site = edge_dir.path();
    let (edge_cert_pem, edge_key_pem, edge_cert_der) = generate_tls_cert();
    std::fs::write(edge_site.join("cert.pem"), &edge_cert_pem).unwrap();
    std::fs::write(edge_site.join("key.pem"), &edge_key_pem).unwrap();
    let edge_analytics_log = edge_dir.path().join("analytics.ndjson");

    std::fs::write(
        edge_site.join("site.toml"),
        format!(
            r#"
[site]
name   = "edge"
domain = "localhost"

[log]
level  = "warn"
format = "text"

[errors]
mode = "internal"

[analytics]
log_path = "{log_path}"

[rate_limit]
enabled = false

[[backend]]
name            = "origin"
url             = "https://127.0.0.1:{origin_port}"
tls_skip_verify = true

[[route]]
path    = "/public/{{relpath}}"
backend = "origin"
"#,
            log_path = edge_analytics_log.display(),
        ),
    )
    .unwrap();

    let edge_port_claim = claim_port();
    let edge_port = edge_port_claim.port();
    std::fs::write(
        edge_site.join("system.toml"),
        format!(
            "\n[server]\nbind     = \"127.0.0.1:{edge_port}\"\ntls_cert = \"{cert}\"\ntls_key  = \"{key}\"\n\n[node]\nname = \"edge\"\n",
            cert = edge_site.join("cert.pem").display(),
            key = edge_site.join("key.pem").display(),
        ),
    )
    .unwrap();

    let mut _edge_http_proc = Service::spawn(
        "edge m6-http",
        Command::new(binary("m6-http"))
            .arg(edge_site)
            .arg(edge_site.join("system.toml")),
    );
    _edge_http_proc.wait_for_tcp(edge_port, Duration::from_secs(10));

    // Both stacks must actually be serving, not merely listening: m6-http
    // fills its backend pool from a periodic rescan. This was two
    // `sleep(1500)`s and they were not enough on the Linux build box. Probing
    // the edge exercises the whole chain, so it covers the origin too.
    let probe_tls = tls_client_config(&edge_cert_der);
    let ready = m6_core::testkit::wait::until(Duration::from_secs(30), || {
        https_get(edge_port, "/public/probe.html", &[], probe_tls.clone()).status == 200
    });
    assert!(
        ready,
        "the edge never served a request through to the origin\n         --- edge stderr ---\n{}\n--- origin stderr ---\n{}",
        _edge_http_proc.output(),
        _origin_http_proc.output()
    );
    // The probes are logged like any other request, and this test counts lines.
    std::thread::sleep(Duration::from_millis(250));
    std::fs::write(&origin_analytics_log, b"").expect("truncate origin log");
    std::fs::write(&edge_analytics_log, b"").expect("truncate edge log");

    // ── The actual test: one request through the edge, no cookie yet ──────
    let edge_tls = tls_client_config(&edge_cert_der);
    let resp = https_get(edge_port, "/public/page.html", &[], edge_tls);
    assert_eq!(resp.status, 200, "headers:\n{}", resp.headers);

    let set_cookies = resp.header_values("set-cookie");
    assert_eq!(
        set_cookies.len(), 1,
        "the client must see exactly ONE Set-Cookie even though two m6-http \
         instances (edge + origin) both touched this request — got {}: {:?}\n\
         full headers:\n{}",
        set_cookies.len(), set_cookies, resp.headers
    );
    let session_id = set_cookies[0].split(';').next().unwrap().split_once('=').unwrap().1.to_string();

    std::thread::sleep(Duration::from_millis(400));

    // Both nodes should have logged their own analytics line for this one
    // request (the whole point of per-node analytics — edge hit/latency
    // visibility isn't lost), and BOTH must agree on the same session id.
    let origin_lines = read_analytics_lines(&origin_analytics_log);
    let edge_lines = read_analytics_lines(&edge_analytics_log);
    assert_eq!(origin_lines.len(), 1, "expected exactly 1 analytics line at origin: {origin_lines:?}");
    assert_eq!(edge_lines.len(), 1, "expected exactly 1 analytics line at edge: {edge_lines:?}");
    assert_eq!(origin_lines[0].session_id, session_id, "origin's logged session id must match the cookie the client received");
    assert_eq!(
        edge_lines[0].session_id, session_id,
        "edge's logged session id must match origin's — not a second, independently-minted one"
    );
    assert!(origin_lines[0].session_new, "origin minted the session, so its line should say session_new=true");
    assert!(
        !edge_lines[0].session_new,
        "edge did not mint anything — it reused origin's session — so its line should say session_new=false"
    );
}

/// The readiness probe must survive a server that is bound but not serving.
///
/// This is the failure from 2026-09-12 reproduced deterministically. A server
/// that accepts a connection and closes it without reading sends RST rather
/// than FIN, so the client does not learn about it at connect time: it learns
/// on its next write, which for TLS is the handshake. `https_get` used to
/// `.unwrap()` there, and because it is called from inside
/// `wait::until(30s, ..)` the panic ended the run instead of retrying, in the
/// loop written to tolerate precisely this.
///
/// Verified red against the previous helper: `called Result::unwrap() on an
/// Err value: Os { code: 54, kind: ConnectionReset }`, the same error and the
/// same line as the original failure.
#[test]
fn a_probe_against_a_bound_but_dead_server_does_not_panic() {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind");
    let port = listener.local_addr().unwrap().port();

    // Accept and drop without reading. The client's ClientHello is sitting
    // unread in the receive buffer, which is what turns the close into an RST.
    let accepting = std::thread::spawn(move || {
        for stream in listener.incoming().take(1) {
            drop(stream);
        }
    });

    let (_cert_pem, _key_pem, cert_der) = generate_tls_cert();
    let tls = tls_client_config(&cert_der);

    let resp = https_get(port, "/probe", &[], tls);
    assert_eq!(
        resp.status, 0,
        "a dead server must read as 'not serving', got {} with headers:\n{}",
        resp.status, resp.headers
    );
    assert!(
        resp.headers.starts_with("<no response:"),
        "the reason must survive for the final assertion to print, got:\n{}",
        resp.headers
    );

    accepting.join().ok();
}
