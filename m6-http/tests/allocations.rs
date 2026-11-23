//! Heap allocations per cached request, measured by source category
//! and held as a guard.
//!
//! Each row starts the `m6-http` binary cargo built for this test, which holds
//! the counting allocator behind its `count-allocations` feature. The server
//! arms the counter when its run loop starts, logs the count at a mark the test
//! names, and logs it again at stop. A row runs the server twice, once for
//! N = 1,000 requests and once for 2N = 2,000, each on one warm keep-alive
//! connection. Both runs set `M6_ALLOC_MARK_AFTER=1`, so the mark is read at the
//! first turn after the warm-up response is on the wire, and the row's window is
//! the requests after it. The per-request figure is
//! `((2N stop - 2N mark) - (N stop - N mark)) / N`, so the difference is the N
//! requests and nothing else.
//!
//! The mark is what keeps one TLS handshake out of the difference. A handshake's
//! own counted calls move with the number of reads its flight arrives in, and
//! that number is the kernel's: one unsplit handshake cost 139 or 140 calls on
//! this host, and 141 or 142 when the ClientHello arrived in two pieces, with
//! the two extra calls in rustls's `DeframerVecBuffer::prepare_read` and the
//! last one in aws-lc-rs's signature buffer. The h1 hit row read 2,003 on the
//! build host against a budget of 2,001. Its N = 1,000 run counted 2,188, which
//! is this host's own N figure, and its 2N = 2,000 run counted 4,191, three
//! calls above this host's 4,188, so the three extra calls were in that run's
//! fixed part and not in a request. Reading the counter after the warm-up keeps
//! the fixed part out of the figure.
//!
//! The rows are the four paths this test splits: a cache hit over HTTP/1.1 on one
//! keep-alive connection, the same over HTTP/2, a cache miss that goes to the
//! backend and is stored, and a cache hit on a new connection with a fresh TLS
//! handshake. The last row sends one request per connection, so it is measured
//! per connection, not per request: it is the separate cost of what a new
//! connection adds.
//!
//! ```text
//! cargo test -p m6-http --test allocations --features count-allocations -- --test-threads=1 --nocapture
//! ```
//!
//! The budgets are the top of the spread the measurement took, with nothing
//! added on top. They are ceilings: each change lowers the row it attacks, and
//! none is ever raised. A budget that a run exceeds means a counted path
//! started allocating more. The new-connection row is its own measure of what a
//! connection costs and is reported rather than budgeted: one full handshake
//! per connection varies with the server's read sizes, so a per-connection
//! ceiling would flake.

#![allow(
    clippy::disallowed_methods,
    reason = "test code: blocking clients with deadlines drive the service from outside"
)]

use std::io::{Read, Write};
use std::net::TcpStream;
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rustls::StreamOwned;

use m6_core::testkit::{claim_port, PortClaim, Service};
use m6_http_lib::forward::HttpRequest;
use m6_http_lib::h2s_client::H2sTlsClientConn;

// The budgets, in calls per 1,000 requests, measured on macOS on 2026-11-15
// with the difference of two runs: N = 1,000 and 2N = 2,000 on one warm
// connection, per-request figure `(2N - N) / N`. Each run's mark is read after
// its warm-up, so the run's one TLS handshake and its backend connection are in
// neither reading. Each change that removes an allocation lowers the row it
// attacks, in calls per 1,000 requests on the four rows this test drives:
// rustls's buffers, the cache and the rendering path. The only calls allowed
// above a row's own per-request calls are the two named TLS-buffer allowances,
// one on the h1 hit and one on the h2 hit. Any other count above its budget is
// the defect the guard exists for.
//
// The integers the code makes are 2 for h1_tls_hit and 3 for h2_tls_hit (the
// per-request calls are all rustls's, see below), and 47 for
// h1_tls_miss_insert. The h1 hit read 2.000 in fourteen runs after its
// close-path allocation was removed, with N = 1,000 counting 2,169 calls and
// 2N = 2,000 counting 4,169, and the path makes the same 2,000 calls per 1,000
// requests now. Its budget is 2,001: those 2,000 calls plus one call of
// headroom for a run whose received-record count is one higher than its
// neighbour's, which the TLS record boundaries decide. The h2 hit is 3,000:
// fourteen runs after its fix read 3,000 twelve times, 3,001 once and 2,999
// once. Its budget is 3,001, and the one call above 3,000 is one more TLS record
// received in a run than that run's baseline, which costs one
// `Payload::into_vec` and can grow rustls's received-plaintext chunk queue. Both
// are TLS buffers.
//
// The h2 row read 3.002 on Linux before that fix, and the extra was ours: the
// HTTP/2 TLS and plain close paths built `io::Error::new(UnexpectedEof,
// "closed")`, which boxes a message. A connection torn down inside the counted
// window then cost 0 to 3 calls, so the row's figure moved with when the
// shutdown landed rather than with anything a request did. Both paths now build
// the error from the kind, so the close allocates nothing and 3,000 is the
// whole request path.
//
// The h1 row read 2.000 to 2.003 before its fix, and the extra was ours too: the
// TLS close path in `advance_tls` built `io::Error::new(UnexpectedEof,
// "closed")` when a peer closed, and that constructor boxes its message, so
// every teardown inside the counted window added calls and the row moved with
// when the shutdown landed. It builds the error from the kind now, and the
// fixed part of each run fell by nine calls (N = 1,000 went 2,178 -> 2,169,
// 2N = 2,000 went 4,178 -> 4,169). The two calls per request are rustls's
// record buffers.
//
// The h1 row read 2.003 on the build host at cf0009a, a tree that touched
// nothing on the hit path. dhat over K = 1,000 and K = 2,000 on this host put
// exactly two sites above zero, both rustls's record buffers at +1,000 each,
// and 0 on every other site, so no request allocated more than it did. The
// three calls above 2,000 were in the fixed part of the 2N run: this host reads
// N = 2,188 and 2N = 4,187 or 4,188, and the build host's 2N run read 4,191.
// One handshake costs 139 or 140 counted calls here, and 141 or 142 when its
// ClientHello arrives in two reads: dhat puts the two extra calls on rustls's
// `DeframerVecBuffer::prepare_read`, which grows the deframer buffer for a
// partial record, and the last one on aws-lc-rs's signature buffer. That is why
// the row reads a mark after its warm-up now: the fixed part moves with the
// kernel's read sizes, and it must not be part of a per-request figure.
//
// The allowance this row carried before the mark was one handshake call, and it
// is aws-lc-rs's. dhat over a 2.001 run and a 2.000 run differed at exactly one
// stack: `EVP_PKEY::sign`'s `vec![0u8; sig_len]` in aws-lc-rs, through
// `ptr::ManagedPointer::sign`, `ec::key_pair::EcdsaKeyPair::sign`, rustls's
// `EcdsaSigner::sign`, `emit_certificate_verify_tls13` and
// `process_new_packets`. Our frames are the callers only: `advance_tls` hands
// the socket to rustls. `EVP_PKEY::sign` allocates the signature buffer at the
// maximum DER length and then shrinks it to the length the signature took, so a
// handshake whose DER signature is shorter than the 72-byte maximum makes one
// more call than one whose signature is exactly 72. The deframer buffer adds up
// to two more, which is why one call of allowance was not enough.
//
// The provider change is why this row did not need the allowance before. ring's
// ECDSA signer built the signature into a fixed-size value and rustls made the
// one `Vec` at the signature's own length, so the DER length could not change
// the call count. aws-lc-rs replaced ring in every crate at 7f96b2f, and its
// `EVP_PKEY::sign` added the maximum-size buffer and its shrink. The
// per-request calls are still exactly two, both rustls's record buffers: dhat
// between K = 1,000 and K = 2,000 puts +1,000 on each of them and 0 on every
// other site.
//
// The 3 left on h2's cached hit are rustls's, and nothing of ours:
// two `rustls::msgs::base::Payload::into_vec` calls per request
// (`CommonState::take_received_plaintext`, which makes one such call per
// received TLS record, and this row counts two of them per request) and one
// `rustls::msgs::message::outbound::PrefixedPayload::with_capacity` per response
// (`RecordLayer::encrypt_outgoing`). They are the per-record buffers the
// allocation budget accepts. rustls's buffered API cannot be told to decrypt
// or encrypt in place.
// Our request fields, cache key and header list are borrowed views of the
// connection's read buffer, and the decoded field list is reused across
// streams, so a cached hit allocates none of them.
//
// The second phase of the h1 work took h1_tls_miss_insert to 58,004 by removing
// the per-header `format!` and the `String` per `Cache-Control` directive.
// The header-arena change took it to 47,004: a cache entry stores its header
// names and values in one byte arena with `Range<u32>` pairs, so
// `strip_uncacheable`'s twelve clones per insert became one arena build.
//
// The new-connection row is a measure, not a budget: a full handshake per
// connection read 122.859 to 123.849 per connection across the same runs, so a
// per-connection ceiling would flake. It is reported.
//
// A path that allocates one more time per request adds 1,000 and fails every
// budgeted row, which is the break.
const H1_TLS_HIT_CALLS: u64 = 2_001;
const H2_TLS_HIT_CALLS: u64 = 3_001;
const H1_TLS_MISS_INSERT_CALLS: u64 = 47_004;

/// The number of requests in the short run. The budgets are per 1,000 of the
/// row's unit, which is this many.
const REQUESTS: usize = 1_000;

/// A cacheable response. `public` is what the cache stores, and the body's
/// length is the one number the test client must know to read a keep-alive
/// reply.
const BACKEND_RESPONSE: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\n\
Cache-Control: public, max-age=60\r\nContent-Length: 9\r\nConnection: close\r\n\r\ncacheable";

// ── The fixture ──────────────────────────────────────────────────────────────

/// A site directory, a system config and the claimed port. The backend socket
/// is created by [`Fixture::start_backend`], before the service starts, because
/// the pool reads its glob once at start.
struct Fixture {
    dir: tempfile::TempDir,
    site: PathBuf,
    system: PathBuf,
    port: PortClaim,
    cert_der: Vec<u8>,
}

impl Fixture {
    fn new() -> Fixture {
        let dir = tempfile::tempdir_in(m6_core::testkit::socket_scratch_root()).expect("tempdir");
        let d = dir.path();
        let cert = rcgen::generate_simple_self_signed(vec![
            "localhost".to_string(),
            "127.0.0.1".to_string(),
        ])
        .expect("rcgen");
        std::fs::write(d.join("cert.pem"), cert.cert.pem()).expect("write cert");
        std::fs::write(d.join("key.pem"), cert.key_pair.serialize_pem()).expect("write key");

        let port = claim_port();
        let site_dir = d.join("site");
        std::fs::create_dir_all(&site_dir).expect("site directory");
        std::fs::write(
            site_dir.join("site.toml"),
            format!(
                "[site]\nname = \"allocations\"\ndomain = \"localhost\"\n\n\
                 [errors]\nmode = \"internal\"\n\n\
                 [analytics]\nenabled = false\n\n\
                 [log]\nlevel = \"info\"\n\n\
                 [rate_limit]\nenabled = false\n\n\
                 [server]\nbackend_timeout_secs = 5\n\n\
                 [[backend]]\nname = \"b\"\nsockets = \"{}\"\n\n\
                 [[route]]\npath = \"/\"\nbackend = \"b\"\n",
                d.join("b-*.sock").display()
            ),
        )
        .expect("write site.toml");
        std::fs::write(
            d.join("system.toml"),
            format!(
                "[server]\nbind = \"{}\"\ntls_cert = \"{}\"\ntls_key = \"{}\"\n\
                 max_requests_per_connection = 1000000\n\n\
                 [node]\nname = \"allocations\"\n",
                port.addr(),
                d.join("cert.pem").display(),
                d.join("key.pem").display()
            ),
        )
        .expect("write system.toml");

        let system = d.join("system.toml");
        Fixture {
            dir,
            site: site_dir,
            system,
            port,
            cert_der: cert.cert.der().to_vec(),
        }
    }

    /// Bind the one backend the test needs, before the service starts. Every
    /// request is answered from this thread pool, and a reply closes the
    /// backend connection, which is what an ordinary HTTP/1.1 origin does.
    fn start_backend(&self) {
        let sock = self.dir.path().join("b-1.sock");
        let listener = UnixListener::bind(&sock).expect("bind backend socket");
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                std::thread::spawn(move || {
                    let mut head = Vec::new();
                    let mut buf = [0u8; 4096];
                    while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                        match stream.read(&mut buf) {
                            Ok(0) | Err(_) => return,
                            Ok(n) => head.extend_from_slice(&buf[..n]),
                        }
                        if head.len() > 16 * 1024 {
                            return;
                        }
                    }
                    let _ = stream.write_all(BACKEND_RESPONSE);
                });
            }
        });
    }

    fn start(&self) -> Service {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_m6-http"));
        cmd.arg(&self.site)
            .arg(&self.system)
            .env("RUST_LOG", "info")
            // Read the counter after the warm-up request, so the run's TLS
            // handshake and its backend connection are outside the measured
            // window. A handshake's allocation count varies with the read
            // sizes the peer's flight arrives in, and that variance was what
            // the h1 hit row counted above its two rustls calls.
            .env("M6_ALLOC_MARK_AFTER", "1")
            // The periodic stats emit is not per request, and a slow run that
            // crossed the ten-second tick would add its allocations to one K
            // and not the other. The interval is the debug build's documented
            // test channel, and this only pushes the first emit past any run.
            .env("M6_TEST_STATS_INTERVAL_SECS", "3600")
            // The once-a-second accounting tick is not request work either: it
            // flushes the log, checks the heap's growth and reads one host
            // source. One tick's calls depend on which source it steps, so a
            // slow runner, whose runs cross more ticks, would see a per-run
            // cost arrive in this per-request figure. Pushing the tick past the
            // run leaves the count with the requests alone.
            .env("M6_TEST_ACCOUNTING_TICK_SECS", "3600")
            .current_dir(self.dir.path());
        let mut svc = Service::spawn("m6-http-allocations", &mut cmd);
        svc.wait_for_tcp(self.port.port(), m6_core::testkit::STARTUP_BOUND);
        svc
    }

    fn port(&self) -> u16 {
        self.port.port()
    }
}

// ── The rows ─────────────────────────────────────────────────────────────────

#[derive(Clone, Copy)]
enum Row {
    H1TlsHit,
    H2TlsHit,
    H1TlsMissInsert,
    H1TlsNewConnection,
}

impl Row {
    fn name(self) -> &'static str {
        match self {
            Row::H1TlsHit => "h1_tls_hit",
            Row::H2TlsHit => "h2_tls_hit",
            Row::H1TlsMissInsert => "h1_tls_miss_insert",
            Row::H1TlsNewConnection => "h1_tls_new_connection_per_connection",
        }
    }

    /// The path of request `i`. The miss row uses a fresh query each time,
    /// because the query is part of the cache key, so every request misses,
    /// reaches the backend and is stored.
    fn path(self, i: usize) -> String {
        match self {
            Row::H1TlsMissInsert => format!("/?n={i}"),
            _ => "/".to_string(),
        }
    }

    /// The row's unit: a request, or a connection for the row that opens one
    /// per request. A budget, where the row has one, is per 1,000 of these.
    fn unit(self) -> &'static str {
        match self {
            Row::H1TlsNewConnection => "connection",
            _ => "request",
        }
    }

    /// The row's calls-per-1,000-requests ceiling. The new-connection row has
    /// none: its per-connection figure is a measure, because a full handshake's
    /// own read sizes move it run to run.
    fn budget_calls(self) -> Option<u64> {
        match self {
            Row::H1TlsHit => Some(H1_TLS_HIT_CALLS),
            Row::H2TlsHit => Some(H2_TLS_HIT_CALLS),
            Row::H1TlsMissInsert => Some(H1_TLS_MISS_INSERT_CALLS),
            Row::H1TlsNewConnection => None,
        }
    }
}

// ── The four tests ───────────────────────────────────────────────────────────

#[test]
fn h1_tls_hit() {
    check(Row::H1TlsHit);
}

#[test]
fn h2_tls_hit() {
    check(Row::H2TlsHit);
}

#[test]
fn h1_tls_miss_insert() {
    check(Row::H1TlsMissInsert);
}

#[test]
fn h1_tls_new_connection_per_connection() {
    check(Row::H1TlsNewConnection);
}

/// Measure a row over N = [`REQUESTS`] and over 2N, record the difference as
/// calls and bytes per request (or per connection), and hold the request rows
/// to their budgets. Each run's own warm-up, handshake and backend connection
/// sit before its mark and cancel inside that run, so the difference is the N
/// requests and nothing else. The new-connection row is printed and not
/// asserted.
fn check(row: Row) {
    let (small_calls, small_bytes) = measure(row, REQUESTS);
    let (large_calls, large_bytes) = measure(row, 2 * REQUESTS);

    assert!(
        large_calls >= small_calls && large_bytes >= small_bytes,
        "{}: the 2N run counted fewer allocations than the N one \
         ({small_calls} calls, {small_bytes} bytes against {large_calls} calls, \
         {large_bytes} bytes). The count is not comparing the same path.",
        row.name()
    );
    let calls = large_calls - small_calls;
    let bytes = large_bytes - small_bytes;

    println!(
        "{}: {}.{:03} calls and {}.{:03} bytes per {} \
         (N={REQUESTS}: {small_calls} calls, {small_bytes} bytes, \
         2N={}: {large_calls} calls, {large_bytes} bytes)",
        row.name(),
        calls / REQUESTS as u64,
        calls % REQUESTS as u64 * 1_000 / REQUESTS as u64,
        bytes / REQUESTS as u64,
        bytes % REQUESTS as u64 * 1_000 / REQUESTS as u64,
        row.unit(),
        2 * REQUESTS,
    );

    if let Some(budget) = row.budget_calls() {
        assert!(
            calls <= budget,
            "{}: {}.{:03} calls per {} exceeds its budget of {}.{:03} calls per \
             1,000 {}s. A counted path started allocating more. Fix the path rather than raising the budget.",
            row.name(),
            calls / REQUESTS as u64,
            calls % REQUESTS as u64 * 1_000 / REQUESTS as u64,
            row.unit(),
            budget / 1_000,
            budget % 1_000,
            row.unit(),
        );
    }
}

/// One server run for `requests` requests, returning the calls and bytes
/// between the mark and stop. The mark is read after the run's warm-up, so the
/// handshake and the first backend connection are in neither reading.
fn measure(row: Row, requests: usize) -> (u64, u64) {
    let fixture = Fixture::new();
    fixture.start_backend();
    let mut svc = fixture.start();
    let port = fixture.port();

    match row {
        Row::H1TlsHit | Row::H1TlsMissInsert => {
            let mut conn = h1_ready(&fixture, "/");
            for i in 0..requests {
                let status = conn
                    .get(&row.path(i), port, false)
                    .unwrap_or_else(|e| panic!("{}: request {i}: {e}", row.name()));
                assert_eq!(status, 200, "{}: request {i} answered {status}", row.name());
            }
        }
        Row::H1TlsNewConnection => {
            // The warm-up connection takes the start-up cost out of both runs.
            drop(h1_ready(&fixture, "/"));
            for i in 0..requests {
                let mut conn = H1Conn::connect(port, tls_config(&fixture.cert_der, &[b"http/1.1"]));
                let status = conn
                    .get(&row.path(i), port, true)
                    .unwrap_or_else(|e| panic!("{}: request {i}: {e}", row.name()));
                assert_eq!(status, 200, "{}: request {i} answered {status}", row.name());
            }
        }
        Row::H2TlsHit => {
            let mut conn = h2_ready(&fixture);
            for i in 0..requests {
                let status = h2_get(&mut conn, &row.path(i), port)
                    .unwrap_or_else(|e| panic!("{}: request {i}: {e}", row.name()));
                assert_eq!(status, 200, "{}: request {i} answered {status}", row.name());
            }
        }
    }

    let status = svc.terminate(m6_core::testkit::STARTUP_BOUND);
    assert!(
        status.success(),
        "{}: m6-http exited {status} on SIGTERM\n{}",
        row.name(),
        svc.output()
    );
    let ((mark_calls, mark_bytes), (stop_calls, stop_bytes)) =
        run_counts(&svc.output(), row.name());
    assert!(
        stop_calls >= mark_calls && stop_bytes >= mark_bytes,
        "{}: the stop reading is smaller than the mark one ({stop_calls} calls, \
         {stop_bytes} bytes against {mark_calls} calls, {mark_bytes} bytes). \
         The count is not comparing the same run.",
        row.name()
    );
    (stop_calls - mark_calls, stop_bytes - mark_bytes)
}

/// The mark and stop readings the server logs, `(mark, stop)` as `(calls, bytes)`
/// each.
fn run_counts(output: &str, row: &str) -> ((u64, u64), (u64, u64)) {
    (
        line_counts(output, row, "allocations at mark"),
        line_counts(output, row, "allocations during the run"),
    )
}

/// The `calls` and `bytes` fields of one count line.
fn line_counts(output: &str, row: &str, needle: &str) -> (u64, u64) {
    for line in output.lines().rev() {
        if !line.contains(needle) {
            continue;
        }
        let calls = json_field(line, "calls")
            .unwrap_or_else(|| panic!("{row}: {needle} line has no calls field: {line}"));
        let bytes = json_field(line, "bytes")
            .unwrap_or_else(|| panic!("{row}: {needle} line has no bytes field: {line}"));
        return (calls, bytes);
    }
    panic!("{row}: no '{needle}' line in m6-http's output\n{output}");
}

/// One integer field from a JSON line.
fn json_field(line: &str, key: &str) -> Option<u64> {
    let needle = format!("\"{key}\":");
    let start = line.find(&needle)? + needle.len();
    let digits: String = line[start..]
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    digits.parse().ok()
}

// ── TLS clients ──────────────────────────────────────────────────────────────

fn tls_config(cert_der: &[u8], alpn: &[&[u8]]) -> Arc<rustls::ClientConfig> {
    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .ok();
    let cert = rustls::pki_types::CertificateDer::from(cert_der.to_vec());
    let mut store = rustls::RootCertStore::empty();
    store.add(cert).expect("trust the test certificate");
    let mut cfg = rustls::ClientConfig::builder()
        .with_root_certificates(store)
        .with_no_client_auth();
    cfg.alpn_protocols = alpn.iter().map(|p| p.to_vec()).collect();
    // Every connection in `h1_tls_new_connection_per_connection` is a full handshake, as the
    // row is defined. The default client session cache would resume some of
    // them, and a resumed handshake allocates a different number of times, so
    // the count would depend on which connections happened to resume.
    cfg.resumption = rustls::client::Resumption::disabled();
    Arc::new(cfg)
}

/// An HTTP/1.1-over-TLS client that keeps one connection and reads one reply
/// at a time, by `Content-Length`. Sequential requests only, which is all the
/// rows need.
struct H1Conn {
    stream: StreamOwned<rustls::ClientConnection, TcpStream>,
    buffered: Vec<u8>,
}

impl H1Conn {
    fn connect(port: u16, config: Arc<rustls::ClientConfig>) -> H1Conn {
        let tcp = TcpStream::connect(("127.0.0.1", port)).expect("connect to m6-http");
        tcp.set_read_timeout(Some(Duration::from_secs(20)))
            .expect("read timeout");
        tcp.set_write_timeout(Some(Duration::from_secs(20)))
            .expect("write timeout");
        tcp.set_nodelay(true).expect("nodelay");
        let name = rustls::pki_types::ServerName::try_from("localhost").expect("server name");
        let conn = rustls::ClientConnection::new(config, name).expect("TLS client");
        H1Conn {
            stream: StreamOwned::new(conn, tcp),
            buffered: Vec::new(),
        }
    }

    fn get(&mut self, path: &str, port: u16, close: bool) -> std::io::Result<u16> {
        let connection = if close { "Connection: close\r\n" } else { "" };
        let request = format!("GET {path} HTTP/1.1\r\nHost: localhost:{port}\r\n{connection}\r\n");
        self.stream.write_all(request.as_bytes())?;
        self.stream.flush()?;
        self.read_response()
    }

    fn read_response(&mut self) -> std::io::Result<u16> {
        loop {
            if let Some(end) = self.buffered.windows(4).position(|w| w == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&self.buffered[..end]).into_owned();
                let status = head
                    .lines()
                    .next()
                    .and_then(|line| line.split_whitespace().nth(1))
                    .and_then(|code| code.parse().ok())
                    .unwrap_or(0);
                let length = head
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        if name.eq_ignore_ascii_case("content-length") {
                            value.trim().parse::<usize>().ok()
                        } else {
                            None
                        }
                    })
                    .unwrap_or(0);
                let wanted = end + 4 + length;
                while self.buffered.len() < wanted {
                    self.fill()?;
                }
                self.buffered.drain(..wanted);
                return Ok(status);
            }
            self.fill()?;
        }
    }

    fn fill(&mut self) -> std::io::Result<()> {
        let mut buf = [0u8; 8192];
        let n = self.stream.read(&mut buf)?;
        if n == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "m6-http closed before a reply",
            ));
        }
        self.buffered.extend_from_slice(&buf[..n]);
        Ok(())
    }
}

/// A keep-alive connection that has answered its warm-up request, retrying
/// until the service is serving rather than merely bound.
fn h1_ready(fixture: &Fixture, warm_path: &str) -> H1Conn {
    let config = tls_config(&fixture.cert_der, &[b"http/1.1"]);
    let deadline = Instant::now() + m6_core::testkit::STARTUP_BOUND;
    loop {
        let mut conn = H1Conn::connect(fixture.port(), Arc::clone(&config));
        match conn.get(warm_path, fixture.port(), false) {
            Ok(200) => return conn,
            Ok(status) => panic!("warm-up answered {status}, not 200"),
            Err(e) => {
                if Instant::now() >= deadline {
                    panic!("m6-http never answered the warm-up request: {e}");
                }
                std::thread::sleep(Duration::from_millis(25));
            }
        }
    }
}

/// An HTTP/2-over-TLS connection that has answered its warm-up request.
fn h2_ready(fixture: &Fixture) -> H2sTlsClientConn {
    let config = tls_config(&fixture.cert_der, &[b"h2"]);
    let deadline = Instant::now() + m6_core::testkit::STARTUP_BOUND;
    loop {
        match H2sTlsClientConn::connect("localhost", fixture.port(), Arc::clone(&config)) {
            Ok(mut conn) => match h2_get(&mut conn, "/", fixture.port()) {
                Ok(200) => return conn,
                Ok(status) => panic!("h2 warm-up answered {status}, not 200"),
                Err(e) => {
                    if Instant::now() >= deadline {
                        panic!("m6-http never answered the h2 warm-up request: {e}");
                    }
                    std::thread::sleep(Duration::from_millis(25));
                }
            },
            Err(e) => {
                if Instant::now() >= deadline {
                    panic!("could not open an h2 connection to m6-http: {e}");
                }
                std::thread::sleep(Duration::from_millis(25));
            }
        }
    }
}

fn h2_get(conn: &mut H2sTlsClientConn, path: &str, port: u16) -> std::io::Result<u16> {
    let request = HttpRequest {
        method: "GET".to_string(),
        path: path.to_string(),
        query: None,
        version: "HTTP/2".to_string(),
        headers: vec![("host".to_string(), format!("localhost:{port}"))],
        body: Vec::new(),
    };
    let stream_id = conn.dispatch(&request, "localhost", "127.0.0.1", "localhost", None)?;
    let deadline = Instant::now() + m6_core::testkit::STARTUP_BOUND;
    loop {
        conn.drive();
        if let Some(done) = conn
            .drain_completions()
            .into_iter()
            .find(|c| c.stream_id == stream_id)
        {
            return done.result.map(|response| response.status);
        }
        if Instant::now() >= deadline {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "h2 reply timed out",
            ));
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}
