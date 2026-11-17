//! The four cache outcomes are four counters, and each path moves exactly one.
//!
//! A hit and a miss are one bit each, and `/perf` used to report only those two.
//! Four different facts were folded into them: a hit on an entry still inside
//! its freshness lifetime, a hit on one past it (a stale serve), a miss answered
//! by fetching a backend, and a miss answered by m6-http itself. This suite
//! drives all four against a real `m6-http` and a fake origin, and asserts the
//! exact delta of every counter after each request.
//!
//! Each test names the line that has to be broken for it to fail.

#![allow(
    clippy::disallowed_methods,
    reason = "test code: threads, sleeps and blocking clients drive the service from outside"
)]

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rustls::StreamOwned;

use m6_core::testkit::{binary, claim_port, read_one, PortClaim, Service};
use m6_http_lib::probe;

/// The body every fake-origin answer carries, so a served copy is identifiable.
const BODY: &str = "origin copy";

// ── The fake origin ───────────────────────────────────────────────────────────

/// A plain-HTTP origin whose `Cache-Control` is chosen by the request path.
///
/// `/fresh` is fresh for a minute. `/stale` is past its freshness the moment it
/// is stored and inside a one-minute `stale-while-revalidate` window, which is
/// the only way this codebase serves an invalid entry. Anything else is fresh.
struct Origin {
    port: u16,
    requests: Arc<AtomicUsize>,
}

impl Origin {
    fn spawn() -> Origin {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind fake origin");
        let port = listener.local_addr().expect("origin addr").port();
        let requests = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&requests);
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let seen = Arc::clone(&seen);
                std::thread::spawn(move || serve(stream, &seen));
            }
        });
        Origin { port, requests }
    }

    /// Total requests answered, any path.
    fn requests(&self) -> usize {
        self.requests.load(Ordering::SeqCst)
    }
}

fn serve(mut stream: TcpStream, seen: &AtomicUsize) {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.windows(4).any(|w| w == b"\r\n\r\n") {
        match stream.read(&mut byte) {
            Ok(0) => return,
            Ok(_) => head.push(byte[0]),
            Err(_) => return,
        }
    }
    let line = String::from_utf8_lossy(&head).to_string();
    let path = line.split_whitespace().nth(1).unwrap_or("/").to_string();
    seen.fetch_add(1, Ordering::SeqCst);
    // The cache node closes and reopens as it pleases, so one response per
    // connection keeps the fake honest without a keep-alive state machine.
    let cache_control = if path.starts_with("/stale") {
        "public, max-age=0, stale-while-revalidate=60"
    } else {
        "public, max-age=60"
    };
    let resp = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nCache-Control: {cache_control}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{BODY}",
        BODY.len()
    );
    let _ = stream.write_all(resp.as_bytes());
    let _ = stream.flush();
}

// ── The node under test ──────────────────────────────────────────────────────

fn keypair() -> (String, String) {
    let ck =
        rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).expect("certificate");
    (ck.cert.pem(), ck.key_pair.serialize_pem())
}

// ── The token `/perf` needs ──────────────────────────────────────────────────

/// The signing key and its public half, for the token `/perf` expects.
///
/// The legacy `[health] metrics_token_file` bearer string is removed at this
/// tip: `/perf` is gated by `metrics_require` and a signed token, so the suite
/// mints one with the claim the system config names.
fn auth_keypair() -> (String, String) {
    let kp = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).expect("ec keypair");
    (kp.serialize_pem(), kp.public_key_pem())
}

fn mint(private_pem: &str, roles: &[&str]) -> String {
    #[derive(serde::Serialize)]
    struct Claims {
        sub: String,
        exp: i64,
        roles: Vec<String>,
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let claims = Claims {
        sub: "cache-accounting-test".to_string(),
        exp: now + 3600,
        roles: roles.iter().map(|r| r.to_string()).collect(),
    };
    let key = jsonwebtoken::EncodingKey::from_ec_pem(private_pem.as_bytes()).expect("ec key");
    let header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::ES256);
    jsonwebtoken::encode(&header, &claims, &key).expect("sign jwt")
}

fn system_toml(
    port: u16,
    cert: &std::path::Path,
    key: &std::path::Path,
    auth_public: &std::path::Path,
) -> String {
    format!(
        "[server]\nbind = \"127.0.0.1:{port}\"\ntls_cert = \"{}\"\ntls_key = \"{}\"\nmax_requests_per_connection = 10000\n\n\
         [auth]\npublic_key = \"{}\"\n\n\
         [health]\nmetrics_require = \"role:monitor\"\n\n\
         [node]\nname = \"cache-accounting\"\n",
        cert.display(),
        key.display(),
        auth_public.display(),
    )
}

fn site_toml(origin_port: u16) -> String {
    format!(
        "[site]\nname = \"cache-accounting\"\ndomain = \"localhost\"\n\n\
         [server]\nbackend_timeout_secs = 5\n\n\
         [log]\nlevel = \"info\"\n\n\
         [analytics]\nenabled = false\n\n\
         [rate_limit]\nenabled = false\n\n\
         [[backend]]\nname = \"origin\"\nurl = \"http://127.0.0.1:{origin_port}\"\n\n\
         [[route]]\npath = \"/fresh\"\nbackend = \"origin\"\n\n\
         [[route]]\npath = \"/stale\"\nbackend = \"origin\"\n"
    )
}

struct Stack {
    _dir: tempfile::TempDir,
    _claim: PortClaim,
    port: u16,
    token: String,
    service: Service,
}

impl Stack {
    fn start(origin_port: u16) -> Stack {
        let dir = tempfile::tempdir().expect("tempdir");
        let claim = claim_port();
        let port = claim.port();
        let (cert_pem, key_pem) = keypair();
        let cert = dir.path().join("cert.pem");
        let key = dir.path().join("key.pem");
        std::fs::write(&cert, cert_pem).expect("cert");
        std::fs::write(&key, key_pem).expect("key");
        let (auth_private, auth_public_pem) = auth_keypair();
        let auth_public = dir.path().join("auth-public.pem");
        std::fs::write(&auth_public, auth_public_pem).expect("auth public key");
        let token = mint(&auth_private, &["monitor"]);
        std::fs::write(dir.path().join("site.toml"), site_toml(origin_port)).expect("site.toml");
        let system: PathBuf = dir.path().join("system.toml");
        std::fs::write(&system, system_toml(port, &cert, &key, &auth_public)).expect("system.toml");
        let mut service = Service::spawn(
            "m6-http-cache-accounting",
            Command::new(binary("m6-http")).arg(dir.path()).arg(&system),
        );
        service.wait_for_tcp(port, m6_core::testkit::STARTUP_BOUND);
        Stack {
            _dir: dir,
            _claim: claim,
            port,
            token,
            service,
        }
    }

    fn get(&self, path: &str) -> (u16, String) {
        self.get_with(path, &[])
    }

    fn get_with(&self, path: &str, extra: &[(&str, &str)]) -> (u16, String) {
        let tcp = TcpStream::connect(("127.0.0.1", self.port)).expect("connect");
        // No socket read timeout: the read is waited for as an event under one
        // generous bound. A read timeout fires on a loaded host while the
        // service is healthy, and the test reports a missing answer.
        let name = rustls::pki_types::ServerName::try_from("localhost".to_string()).expect("name");
        let conn =
            rustls::ClientConnection::new(probe::tls_config(b"http/1.1"), name).expect("tls");
        let mut stream = StreamOwned::new(conn, tcp);
        let mut req = format!(
            "GET {path} HTTP/1.1\r\nHost: localhost:{}\r\nConnection: close\r\n",
            self.port
        );
        for (k, v) in extra {
            req.push_str(&format!("{k}: {v}\r\n"));
        }
        req.push_str("\r\n");
        stream.write_all(req.as_bytes()).expect("write");
        stream.flush().expect("flush");
        let raw =
            match m6_core::testkit::wait_for_event(m6_core::testkit::RESPONSE_BOUND, move || {
                let mut stream = stream;
                let raw = read_one(&mut stream, "GET")?;
                Ok::<_, std::io::Error>(raw)
            }) {
                Ok(Ok(raw)) => raw,
                Ok(Err(e)) => panic!("read: {e}"),
                Err(e) => panic!("read: {e}"),
            };
        let text = String::from_utf8_lossy(&raw).to_string();
        let (head, body) = text.split_once("\r\n\r\n").unwrap_or((text.as_str(), ""));
        let status = head
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .unwrap_or_else(|| panic!("no status in:\n{head}"));
        (status, body.to_string())
    }

    /// One `/perf` counter, cumulative.
    fn counter(&self, key: &str) -> u64 {
        let (status, body) = self.get_with(
            "/perf",
            &[("Authorization", &format!("Bearer {}", self.token))],
        );
        assert_eq!(
            status,
            200,
            "/perf refused:\n{body}\n{}",
            self.service.output()
        );
        let pat = format!("\"{key}\":");
        body.split(&pat)
            .nth(1)
            .and_then(|rest| {
                rest.split(|c: char| !c.is_ascii_digit())
                    .next()
                    .and_then(|n| n.parse().ok())
            })
            .unwrap_or_else(|| panic!("{key} is not in /perf:\n{body}"))
    }
}

/// Every cache figure at once, so a test can name the one that moved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CacheCounts {
    hits_valid: u64,
    hits_invalid: u64,
    misses_fetched: u64,
    misses_local: u64,
    hits_total: u64,
    misses_total: u64,
}

impl Stack {
    fn counts(&self) -> CacheCounts {
        CacheCounts {
            hits_valid: self.counter("cache_hits_valid_total"),
            hits_invalid: self.counter("cache_hits_invalid_total"),
            misses_fetched: self.counter("cache_misses_fetched_total"),
            misses_local: self.counter("cache_misses_answered_locally_total"),
            hits_total: self.counter("cache_hits_total"),
            misses_total: self.counter("cache_misses_total"),
        }
    }
}

/// Wait until the fake origin has answered `want` requests, never on a sleep.
fn wait_for_origin(origin: &Origin, want: usize) -> bool {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if origin.requests() >= want {
            return true;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    false
}

// ── The four paths ───────────────────────────────────────────────────────────

/// Each of the four facts moves its own counter by exactly one, on the request
/// that produces it, and moves no other cache figure by more than its own
/// totals allow.
///
/// Breaks this test carries, one per counter:
///   - `route_request` passing `CacheOutcome::InvalidHit` for a fresh entry, or
///     `ValidHit` for a stale one, fails the two hit assertions.
///   - deleting `cache_misses_fetched_total += 1` fails after the first `/fresh`.
///   - deleting `cache_misses_answered_locally_total += 1` fails on `/nowhere`.
///   - counting the background refresh as a request fails the last assertion.
#[test]
fn each_cache_fact_moves_its_own_counter_once() {
    let origin = Origin::spawn();
    let stack = Stack::start(origin.port);

    // 1. Cold: a miss answered by fetching the origin.
    assert_eq!(stack.get("/fresh").0, 200, "{}", stack.service.output());
    assert!(wait_for_origin(&origin, 1), "the origin was never asked");
    assert_eq!(
        stack.counts(),
        CacheCounts {
            hits_valid: 0,
            hits_invalid: 0,
            misses_fetched: 1,
            misses_local: 0,
            hits_total: 0,
            misses_total: 1,
        },
        "a cold request is one fetched miss"
    );

    // 2. Warm and still fresh: a valid hit.
    let (status, body) = stack.get("/fresh");
    assert_eq!(
        (status, body.as_str()),
        (200, BODY),
        "{}",
        stack.service.output()
    );
    assert_eq!(
        stack.counts(),
        CacheCounts {
            hits_valid: 1,
            hits_invalid: 0,
            misses_fetched: 1,
            misses_local: 0,
            hits_total: 1,
            misses_total: 1,
        },
        "a second request inside the freshness lifetime is one valid hit"
    );

    // 3. Cold again, on the path whose answer goes stale at once.
    assert_eq!(stack.get("/stale").0, 200, "{}", stack.service.output());
    assert!(
        wait_for_origin(&origin, 2),
        "the origin was never asked twice"
    );
    assert_eq!(
        stack.counts(),
        CacheCounts {
            hits_valid: 1,
            hits_invalid: 0,
            misses_fetched: 2,
            misses_local: 0,
            hits_total: 1,
            misses_total: 2,
        },
        "the stale path's first request is still a fetched miss"
    );

    // 4. Past freshness, inside stale-while-revalidate: an invalid hit.
    let (status, body) = stack.get("/stale");
    assert_eq!(
        (status, body.as_str()),
        (200, BODY),
        "{}",
        stack.service.output()
    );
    assert_eq!(
        stack.counts(),
        CacheCounts {
            hits_valid: 1,
            hits_invalid: 1,
            misses_fetched: 2,
            misses_local: 0,
            hits_total: 2,
            misses_total: 2,
        },
        "a stale serve is one invalid hit, and it is still a hit"
    );

    // The stale serve queued a background refresh. It fills the cache and is
    // not a visit: wait until the origin has answered it, then check that no
    // counter moved.
    assert!(
        wait_for_origin(&origin, 3),
        "the stale serve never queued a background refresh:\n{}",
        stack.service.output()
    );
    assert_eq!(
        stack.counts(),
        CacheCounts {
            hits_valid: 1,
            hits_invalid: 1,
            misses_fetched: 2,
            misses_local: 0,
            hits_total: 2,
            misses_total: 2,
        },
        "a background refresh is not a request and must not be counted"
    );

    // 5. A path matching no route: a miss m6-http answers itself.
    assert_eq!(stack.get("/nowhere").0, 404, "{}", stack.service.output());
    assert_eq!(
        stack.counts(),
        CacheCounts {
            hits_valid: 1,
            hits_invalid: 1,
            misses_fetched: 2,
            misses_local: 1,
            hits_total: 2,
            misses_total: 3,
        },
        "a local 404 is a miss answered locally, not a fetched one"
    );
}
