//! A warm m6-http's resident set, and the pin that must not populate what the
//! process only reserved.
//!
//! m6-http reserves its heap at start and pins memory with `mlockall`
//! (`[server] lock_memory`, on by default). A pin that faults in every mapping
//! the allocator has made resident instead of locking pages as they are touched
//! makes the process hold its whole address space: mimalloc reserves 1 GiB of
//! address space by default and the heap reservation sits on top of it, so the
//! build host measured 1,259,140 KiB resident against 95,856 KiB for the base
//! build, with 1 GiB of it on transparent huge pages, and the kernel's
//! out-of-memory killer killed 13 m6-http processes.
//!
//! This test warms a real server with cache hits and holds three properties:
//!
//! - **the bound**: the resident set is at most what the process held before
//!   pinning, plus the reservation the site configured, plus one huge page per
//!   mimalloc segment class in use, plus a tenth of that. The pin may make the
//!   reservation resident, and the kernel rounds each segment class's pages up
//!   to a huge page. It may not make anything else resident.
//! - **the pin is in force**: `VmLck` is above zero, so the test cannot pass by
//!   having no lock at all.
//! - **the hot path**: 1,000 cache hits after the warm-up take zero minor page
//!   faults, so the fix did not move the fault a pinned page used to avoid onto
//!   every request.
//!
//! The origin is a unix socket in this process and counts its requests, so a
//! run whose hits were really misses is caught rather than measured.
//!
//! Linux only. macOS reports no resident set for another process and has
//! nothing to pin, so there is no number to assert.

#![allow(
    clippy::disallowed_methods,
    reason = "test code: threads, sockets and a blocking client drive the service from outside"
)]

use std::io::{Read, Write};
use std::net::TcpStream;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rustls::StreamOwned;

/// The heap reservation the site asks for (`[limits.heap_reserve] start`). A
/// reservation is a power of two, and the cache is 32 MiB, so 128 MiB holds it
/// and needs neither the rounding warning nor the warning a reserve below the
/// cache prints.
const RESERVE_MIB: usize = 128;
/// Requests that warm the cache and the server's own pages, before anything is
/// measured.
const WARMUP: usize = 200;
/// Requests in the measured window. The minor-fault count over these is the
/// hot-path property.
const HOT: usize = 1_000;
/// The body the origin serves, and the length the client reads with it.
const BODY_LEN: usize = 4096;
/// One huge page, in bytes. The kernel backs a huge page at a time, so the
/// first 64 KiB page mimalloc touches in a segment makes the whole 2 MiB page
/// resident.
const HUGE_PAGE: usize = 2 * 1024 * 1024;
/// Huge pages the bound allows for that rounding: one per mimalloc arena or
/// segment class in use.
///
/// 24 comes from the measurement, and not from a guess. On the build host the
/// pinned build held 129,028 KiB against the base's 95,712 with the small
/// cache, an excess of 33,316 KiB, and 277,632 KiB against 228,788 with the
/// cache full, an excess of 48,844 KiB. Those excesses are 16.3 and 23.8 huge
/// pages, and the allowance is the larger one rounded up. A class that the load
/// touches once costs a whole page whether it wanted 64 KiB of it or all 2 MiB.
const SEGMENT_CLASS_HUGE_PAGES: usize = 24;

/// A cacheable response from the fake origin. `public` with a positive
/// `max-age` is what m6-http stores, and `Connection: close` is what an
/// ordinary HTTP/1.1 origin does with one request per connection.
const ORIGIN_RESPONSE: &str = "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\n\
Cache-Control: public, max-age=3600\r\nContent-Length: 4096\r\nConnection: close\r\n\r\n";

#[test]
fn a_warm_server_is_resident_within_its_reservation() {
    if std::env::consts::OS != "linux" {
        return;
    }
    let fixture = Fixture::new();
    let origin = Origin::start(fixture.dir.path());
    let mut svc = fixture.start();
    let pid = svc.pid();

    let mut conn = fixture.h1_ready();
    for _ in 1..WARMUP {
        assert_eq!(conn.get("/").expect("warm-up request"), 200);
    }

    // The process's resident set, read while it runs: /proc is this process's
    // view of another and the numbers are gone once the child exits. The
    // resident set is the number the bound holds. `VmLck` is the mapped size
    // rather than the resident set, so this test reads it only to show that a
    // pin is in force, and never compares it with the bound.
    let rss = status_kib(pid, "VmRSS:").expect("VmRSS") * 1024;
    let mapped = status_kib(pid, "VmSize:").expect("VmSize") * 1024;
    let locked = status_kib(pid, "VmLck:").expect("VmLck") * 1024;

    // The hot path, after the warm-up: a cache hit must take no page fault. A
    // pin that locks pages one at a time as they are touched leaves this at
    // zero, because every page a hit reads was faulted during the warm-up.
    let faults_before = minor_faults(pid);
    for _ in 0..HOT {
        assert_eq!(conn.get("/").expect("hot request"), 200);
    }
    let faults_after = minor_faults(pid);
    assert_eq!(
        faults_after - faults_before,
        0,
        "m6-http took {} minor page faults over {HOT} cache hits after a {WARMUP} hit warm-up. \
         The hot path must take none, which is what the pin is for.",
        faults_after - faults_before
    );

    // Every request above was answered from the cache, so the numbers describe
    // the hit path. One origin request is the first miss that filled it.
    assert_eq!(
        origin.requests(),
        1,
        "the origin answered {} requests for a page that is cacheable and was requested {} \
         times, so the load was not the cache hits this test measures",
        origin.requests(),
        WARMUP + HOT
    );

    // A shutdown request must exit cleanly with the pin in force.
    assert!(
        svc.terminate(Duration::from_secs(30)).success(),
        "m6-http did not exit cleanly:\n{}",
        svc.output()
    );

    // The field is read after the exit, not after the warm-up. m6-core's log
    // holds complete lines in a Sink's buffer and the loop writes them out on
    // its accounting tick, which is one second, and the warm-up finishes well
    // inside that second. So the line that carries the field is not in the
    // captured output while the process still runs, and a test that read it
    // there passed on a slow day and failed on a fast one. Reading it after
    // the exit, as `allocations.rs` reads its stop line, is the wait on the
    // process exit rather than on a clock.
    let output = svc.output();
    let before = json_field(&output, "resident_before_pinning").unwrap_or_else(|| {
        panic!("m6-http logged no resident_before_pinning field, so the bound cannot be built from the process's own start:\n{output}")
    }) as usize;

    // The bound, as the build host fixed it: the resident set the process
    // reported before it pinned, plus the heap reservation the site
    // configured, plus one 2 MiB page per mimalloc arena or segment class in
    // use, plus a tenth. The pin may make the reservation resident and the
    // kernel rounds each segment class up to a huge page. It may not make
    // anything else resident.
    //
    // Against the defect this test exists for, 1,259,140 KiB resident and 1 GiB
    // of it on huge pages, the bound is about 200 MiB: a wide margin.
    let reserve = RESERVE_MIB * 1024 * 1024;
    let bound = before + reserve + SEGMENT_CLASS_HUGE_PAGES * HUGE_PAGE;
    let bound = bound + bound / 10;

    assert!(
        locked > 0,
        "m6-http did not pin its memory (VmLck is 0), so this test measured nothing:\n{output}"
    );
    assert!(
        rss <= bound,
        "m6-http holds {rss} bytes resident of {mapped} mapped after {WARMUP} cache hits, \
         and the bound is {bound} bytes: {before} resident before pinning, plus the {reserve} \
         byte reservation, plus {SEGMENT_CLASS_HUGE_PAGES} huge pages of {HUGE_PAGE} bytes, \
         plus a tenth. Anything above that is memory the pin made resident that the load never \
         touched. VmLck is {locked} bytes and is not the resident set.\n{output}"
    );
}

// ── The fixture ──────────────────────────────────────────────────────────────

struct Fixture {
    dir: tempfile::TempDir,
    site: PathBuf,
    system: PathBuf,
    port: m6_core::testkit::PortClaim,
    cert_der: Vec<u8>,
}

impl Fixture {
    fn new() -> Fixture {
        let dir = tempfile::tempdir().expect("tempdir");
        let d = dir.path();
        let cert = rcgen::generate_simple_self_signed(vec![
            "localhost".to_string(),
            "127.0.0.1".to_string(),
        ])
        .expect("rcgen");
        std::fs::write(d.join("cert.pem"), cert.cert.pem()).expect("write cert");
        std::fs::write(d.join("key.pem"), cert.key_pair.serialize_pem()).expect("write key");

        let port = m6_core::testkit::claim_port();
        let site_dir = d.join("site");
        std::fs::create_dir_all(&site_dir).expect("site directory");
        std::fs::write(
            site_dir.join("site.toml"),
            format!(
                "[site]\nname = \"heap-resident\"\ndomain = \"localhost\"\n\n\
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
                 max_requests_per_connection = 1000000\nlock_memory = true\n\n\
                 [node]\nname = \"heap-resident\"\n\n\
                 [limits.heap_reserve]\nstart = \"128MiB\"\n\n\
                 [limits.cache_bytes]\nstart = \"32MiB\"\nmax = \"32MiB\"\n",
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

    fn port(&self) -> u16 {
        self.port.port()
    }

    fn start(&self) -> m6_core::testkit::Service {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_m6-http"));
        cmd.arg(&self.site)
            .arg(&self.system)
            // The periodic stats emit is not per request, and a run that crossed
            // its tick would turn one fault into two. Push the first emit past
            // any run, as the allocation tests do.
            .env("M6_TEST_STATS_INTERVAL_SECS", "3600")
            .current_dir(self.dir.path());
        let mut svc = m6_core::testkit::Service::spawn("m6-http-heap-resident", &mut cmd);
        svc.wait_for_tcp(self.port(), Duration::from_secs(15));
        svc
    }

    /// A keep-alive connection that has answered its warm-up request, retried
    /// until the service is serving and not merely bound.
    fn h1_ready(&self) -> H1Conn {
        let config = tls_config(&self.cert_der);
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let mut conn = H1Conn::connect(self.port(), Arc::clone(&config));
            match conn.get("/") {
                Ok(200) => return conn,
                Ok(status) => panic!("the warm-up request answered {status}, not 200"),
                Err(e) => {
                    if Instant::now() >= deadline {
                        panic!("m6-http never answered the warm-up request: {e}");
                    }
                    std::thread::sleep(Duration::from_millis(25));
                }
            }
        }
    }
}

// ── The fake origin ──────────────────────────────────────────────────────────

/// A unix-socket origin that answers every request with [`ORIGIN_RESPONSE`] and
/// counts them. One request per connection, which is what the response's
/// `Connection: close` asks m6-http for.
struct Origin {
    requests: Arc<AtomicUsize>,
}

impl Origin {
    fn start(dir: &Path) -> Origin {
        let socket = dir.join("b-1.sock");
        let listener = UnixListener::bind(&socket).expect("bind the origin socket");
        let requests = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&requests);
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let seen = Arc::clone(&seen);
                std::thread::spawn(move || serve(stream, &seen));
            }
        });
        Origin { requests }
    }

    fn requests(&self) -> usize {
        self.requests.load(Ordering::SeqCst)
    }
}

fn serve(mut stream: std::os::unix::net::UnixStream, seen: &AtomicUsize) {
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
    seen.fetch_add(1, Ordering::SeqCst);
    let _ = stream.write_all(ORIGIN_RESPONSE.as_bytes());
    let body = [b'x'; BODY_LEN];
    let _ = stream.write_all(&body);
    let _ = stream.flush();
}

// ── This process's view of another: /proc ────────────────────────────────────

/// One integer field of `/proc/<pid>/status`, in KiB.
fn status_kib(pid: u32, field: &str) -> Option<usize> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    text.lines()
        .find_map(|line| line.strip_prefix(field))?
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

/// The process's minor page faults, field 10 of `/proc/<pid>/stat`. Field 2 is
/// the command in parentheses, which may hold spaces, so the count starts after
/// the last `)`.
fn minor_faults(pid: u32) -> u64 {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).expect("stat");
    let rest = &stat[stat.rfind(')').expect("comm") + 1..];
    let fields: Vec<&str> = rest.split_whitespace().collect();
    // The first field after the command is the state, which is field 3, so
    // minflt (field 10) is the eighth of the remaining ones.
    fields[7].parse().expect("minflt")
}

/// One integer field of a JSON log line.
fn json_field(output: &str, key: &str) -> Option<u64> {
    let needle = format!("\"{key}\":");
    let start = output.find(&needle)? + needle.len();
    output[start..]
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>()
        .parse()
        .ok()
}

// ── The client ───────────────────────────────────────────────────────────────

fn tls_config(cert_der: &[u8]) -> Arc<rustls::ClientConfig> {
    rustls::crypto::ring::default_provider()
        .install_default()
        .ok();
    let cert = rustls::pki_types::CertificateDer::from(cert_der.to_vec());
    let mut store = rustls::RootCertStore::empty();
    store.add(cert).expect("trust the test certificate");
    let mut cfg = rustls::ClientConfig::builder()
        .with_root_certificates(store)
        .with_no_client_auth();
    cfg.alpn_protocols = vec![b"http/1.1".to_vec()];
    Arc::new(cfg)
}

/// HTTP/1.1 over TLS, one connection, one reply at a time read by
/// `Content-Length`. Sequential only, which is all this test does.
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

    fn get(&mut self, path: &str) -> std::io::Result<u16> {
        let request = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\n\r\n");
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
