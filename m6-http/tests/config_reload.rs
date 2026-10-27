//! m6-http applies a change to either config file, and each file's change
//! costs what it should.
//!
//! A change to `site.toml` rebuilds the routes and empties the response cache,
//! and empties it again once the settle window has passed. A change to the
//! system config alone keeps the cache. Keys read only at start are named and
//! keep their running value. The backend counts its requests, so a cached
//! response is the same number twice and a miss is a new one.
//!
//! ```text
//! cargo test -p m6-http --test config_reload -- --test-threads=1
//! ```

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use m6_core::testkit::{binary, claim_port, wait, PortClaim, Service};

/// A backend on loopback, on a port the OS picks. Every request gets
/// `Cache-Control: public, max-age=600` and a body that is the number of
/// requests so far.
fn counting_backend() -> u16 {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("backend listener");
    let port = listener.local_addr().expect("addr").port();
    let count = Arc::new(AtomicU64::new(0));
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(mut s) = conn else { continue };
            let count = Arc::clone(&count);
            std::thread::spawn(move || {
                let mut buf = [0u8; 4096];
                let mut got = Vec::new();
                while !got.windows(4).any(|w| w == b"\r\n\r\n") {
                    match s.read(&mut buf) {
                        Ok(0) | Err(_) => return,
                        Ok(n) => got.extend_from_slice(&buf[..n]),
                    }
                }
                let n = count.fetch_add(1, Ordering::SeqCst) + 1;
                let body = n.to_string();
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\n\
                     Cache-Control: public, max-age=600\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = s.write_all(resp.as_bytes());
            });
        }
    });
    port
}

fn tls_client() -> Arc<rustls::ClientConfig> {
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
            rustls::crypto::ring::default_provider()
                .signature_verification_algorithms
                .supported_schemes()
        }
    }
    rustls::crypto::ring::default_provider()
        .install_default()
        .ok();
    let mut c = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(NoVerify))
        .with_no_client_auth();
    c.alpn_protocols = vec![b"http/1.1".to_vec()];
    Arc::new(c)
}

/// GET over HTTP/1.1 and TLS. Returns the status and the body.
fn get(port: u16, path: &str) -> Result<(u16, String), String> {
    let tcp = TcpStream::connect(("127.0.0.1", port)).map_err(|e| format!("connect: {e}"))?;
    tcp.set_read_timeout(Some(Duration::from_secs(10)))
        .map_err(|e| e.to_string())?;
    let name = rustls::pki_types::ServerName::try_from("127.0.0.1".to_string())
        .map_err(|e| e.to_string())?;
    let conn = rustls::ClientConnection::new(tls_client(), name).map_err(|e| e.to_string())?;
    let mut stream = rustls::StreamOwned::new(conn, tcp);
    let req = format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n");
    stream
        .write_all(req.as_bytes())
        .map_err(|e| e.to_string())?;
    stream.flush().map_err(|e| e.to_string())?;
    let mut raw = Vec::new();
    match stream.read_to_end(&mut raw) {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {}
        Err(e) => return Err(format!("read: {e}")),
    }
    let text = String::from_utf8_lossy(&raw).into_owned();
    let (head, body) = text
        .split_once("\r\n\r\n")
        .ok_or_else(|| format!("no header block: {text}"))?;
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| format!("no status: {head}"))?;
    Ok((status, body.to_string()))
}

/// The body of a GET that must answer 200.
fn body(port: u16) -> String {
    let (status, body) = get(port, "/").expect("GET /");
    assert_eq!(status, 200, "GET / answered {status}: {body}");
    body
}

/// The child's output with the colour codes removed, so a field reads as
/// `cache_cleared=false`.
fn plain(output: &str) -> String {
    let mut out = String::with_capacity(output.len());
    let mut chars = output.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' && chars.peek() == Some(&'[') {
            for d in chars.by_ref() {
                if d.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

struct Stack {
    port: u16,
    site: std::path::PathBuf,
    cert: String,
    key: String,
    backend_port: u16,
    proc: Service,
    _dir: tempfile::TempDir,
    _claim: PortClaim,
}

/// The system config: a bind, the certificate and key, and a node name.
fn system_toml(bind_port: u16, cert: &str, key: &str, node: &str) -> String {
    format!(
        "[server]\nbind = \"127.0.0.1:{bind_port}\"\ntls_cert = \"{cert}\"\ntls_key = \"{key}\"\n\n\
         [node]\nname = \"{node}\"\n"
    )
}

impl Stack {
    fn system_with(&self, bind_port: u16, node: &str) -> String {
        system_toml(bind_port, &self.cert, &self.key, node)
    }

    fn site_toml(backend_port: u16, extra: &str) -> String {
        format!(
            "[site]\nname = \"config-reload\"\ndomain = \"localhost\"\n\n\
             [log]\nlevel = \"info\"\nformat = \"text\"\n\n\
             [analytics]\nenabled = false\n\n\
             [rate_limit]\nenabled = false\n\n\
             [[backend]]\nname = \"counter\"\nurl = \"http://127.0.0.1:{backend_port}\"\n\n\
             [[route]]\npath = \"/\"\nbackend = \"counter\"\n{extra}"
        )
    }

    fn start() -> Stack {
        let dir = tempfile::tempdir().expect("tempdir");
        let site = dir.path().to_path_buf();
        let ck = rcgen::generate_simple_self_signed(vec![
            "localhost".to_string(),
            "127.0.0.1".to_string(),
        ])
        .expect("certificate");
        let cert = site.join("cert.pem");
        let key = site.join("key.pem");
        std::fs::write(&cert, ck.cert.pem()).expect("cert");
        std::fs::write(&key, ck.key_pair.serialize_pem()).expect("key");

        let backend_port = counting_backend();
        let claim = claim_port();
        let port = claim.port();
        let cert = cert.display().to_string();
        let key = key.display().to_string();
        std::fs::write(site.join("site.toml"), Self::site_toml(backend_port, "")).expect("site");
        std::fs::write(
            site.join("system.toml"),
            system_toml(port, &cert, &key, "first"),
        )
        .expect("system");
        let mut proc = Service::spawn(
            "m6-http",
            Command::new(binary("m6-http"))
                .arg(&site)
                .arg(site.join("system.toml")),
        );
        proc.wait_for_tcp(port, Duration::from_secs(20));
        Stack {
            port,
            site,
            cert,
            key,
            backend_port,
            proc,
            _dir: dir,
            _claim: claim,
        }
    }

    fn system_path(&self) -> std::path::PathBuf {
        self.site.join("system.toml")
    }

    fn touch_system(&self) {
        std::fs::File::options()
            .write(true)
            .open(self.system_path())
            .expect("open system.toml")
            .set_modified(std::time::SystemTime::now())
            .expect("touch system.toml");
    }

    fn log(&self) -> String {
        plain(&self.proc.output())
    }

    /// Wait until the child's output holds `needle` `times` times.
    fn wait_for_log(&self, needle: &str, times: usize) -> bool {
        wait::until(Duration::from_secs(10), || {
            self.log().matches(needle).count() >= times
        })
    }
}

#[test]
fn a_process_config_touch_keeps_the_cache() {
    let mut s = Stack::start();
    let first = body(s.port);
    assert_eq!(body(s.port), first, "the second GET must be a cache hit");

    s.touch_system();
    assert!(
        s.wait_for_log("config reload: complete", 1),
        "no reload after the touch\n{}",
        s.log()
    );
    assert_eq!(
        body(s.port),
        first,
        "a process config touch emptied the cache"
    );
    let log = s.log();
    let line = log
        .lines()
        .find(|l| l.contains("config reload: complete"))
        .expect("reload line");
    assert!(line.contains("cache_cleared=false"), "{line}");
    assert!(line.contains("process=true"), "{line}");
    assert!(line.contains("site=false"), "{line}");
    s.proc.terminate(Duration::from_secs(10));
}

#[test]
fn a_site_toml_change_clears_the_cache_twice() {
    let mut s = Stack::start();
    let first = body(s.port);
    assert_eq!(body(s.port), first);

    std::fs::write(
        s.site.join("site.toml"),
        Stack::site_toml(s.backend_port, "\n# edited\n"),
    )
    .expect("rewrite site.toml");
    assert!(
        s.wait_for_log("config reload: complete", 1),
        "no reload after the site.toml write\n{}",
        s.log()
    );
    let second = body(s.port);
    assert_ne!(second, first, "the first clear did not empty the cache");

    std::thread::sleep(Duration::from_millis(300));
    let third = body(s.port);
    assert_ne!(
        third, second,
        "the page cached between the two clears survived the second"
    );
    assert!(
        s.log()
            .contains("cache cleared again after the settle window"),
        "{}",
        s.log()
    );
    s.proc.terminate(Duration::from_secs(10));
}

#[test]
fn a_bind_change_is_named_and_not_applied() {
    let mut s = Stack::start();
    let other = claim_port();
    let other_port = other.port();
    let first = body(s.port);

    std::fs::write(s.system_path(), s.system_with(other_port, "first")).expect("rewrite system");
    assert!(
        s.wait_for_log("[server].bind changed; restart m6-http to apply it", 1),
        "the bind change was not named\n{}",
        s.log()
    );
    assert_eq!(body(s.port), first, "the running port stopped answering");
    assert!(
        TcpStream::connect_timeout(
            &format!("127.0.0.1:{other_port}").parse().unwrap(),
            Duration::from_millis(500)
        )
        .is_err(),
        "the new bind was applied without a restart"
    );
    s.proc.terminate(Duration::from_secs(10));
}

#[test]
fn a_process_change_after_a_refused_site_reload_keeps_the_running_routes() {
    let mut s = Stack::start();
    let first = body(s.port);

    // Two routes that cannot share a router: the file parses and validates,
    // and the route table refuses it.
    std::fs::write(
        s.site.join("site.toml"),
        Stack::site_toml(
            s.backend_port,
            "\n[[route]]\npath = \"/a/{x}\"\nbackend = \"counter\"\n\n\
             [[route]]\npath = \"/a/{y}\"\nbackend = \"counter\"\n",
        ),
    )
    .expect("break routes");
    assert!(
        s.wait_for_log("route table error", 1),
        "the bad route table was not refused\n{}",
        s.log()
    );

    std::fs::write(s.system_path(), s.system_with(s.port, "renamed")).expect("rename node");
    assert!(
        s.wait_for_log("config reload: complete", 1),
        "no reload after the node change\n{}",
        s.log()
    );
    let log = s.log();
    let line = log
        .lines()
        .rev()
        .find(|l| l.contains("config reload: complete"))
        .expect("reload line");
    assert!(line.contains("process=true"), "{line}");
    assert!(line.contains("site=false"), "{line}");
    let (status, got) = get(s.port, "/").expect("GET /");
    assert_eq!(status, 200, "{got}");
    assert_eq!(
        got, first,
        "the running route stopped serving its cached page"
    );
    s.proc.terminate(Duration::from_secs(10));
}

/// The microseconds since the epoch of a log line's leading RFC 3339 stamp.
fn stamp_us(line: &str) -> i64 {
    let t = line.split_whitespace().next().expect("a stamped line");
    // 2026-10-27T13:26:12.480335Z
    let (date, time) = t.trim_end_matches('Z').split_once('T').expect("T");
    let mut d = date.split('-').map(|x| x.parse::<i64>().unwrap());
    let (y, m, day) = (d.next().unwrap(), d.next().unwrap(), d.next().unwrap());
    let (hms, frac) = time.split_once('.').unwrap_or((time, "0"));
    let mut h = hms.split(':').map(|x| x.parse::<i64>().unwrap());
    let (hh, mm, ss) = (h.next().unwrap(), h.next().unwrap(), h.next().unwrap());
    let us: i64 = format!("{frac:0<6}")[..6].parse().unwrap();
    // Days from civil, enough for ordering and differences.
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe;
    ((days * 24 + hh) * 3600 + mm * 60 + ss) * 1_000_000 + us
}

/// The settle window starts when the first clear is done.
///
/// A reload that takes a while must not use up the window. The site has 20000
/// files under a route group, which makes the reload take far longer than the
/// 100 ms window, and the second clear has to come a full window after the
/// reload finished, not a window after it started.
#[test]
fn the_second_clear_comes_a_full_window_after_a_slow_reload() {
    let mut s = Stack::start();
    let posts = s.site.join("posts");
    std::fs::create_dir_all(&posts).expect("posts dir");
    for i in 0..20_000 {
        std::fs::write(posts.join(format!("p{i}.json")), "{}").expect("post");
    }
    let extra =
        "\n[[route_group]]\nglob = \"posts/*.json\"\npath = \"/p/{stem}\"\nbackend = \"counter\"\n";
    std::fs::write(
        s.site.join("site.toml"),
        Stack::site_toml(s.backend_port, extra),
    )
    .expect("rewrite site.toml");
    assert!(
        s.wait_for_log("cache cleared again after the settle window", 1),
        "no second clear\n{}",
        s.log()
    );
    let log = s.log();
    let done = log
        .lines()
        .find(|l| l.contains("config reload: complete"))
        .expect("complete line");
    let again = log
        .lines()
        .find(|l| l.contains("cache cleared again"))
        .expect("second clear line");
    let elapsed_us: i64 = done
        .split("elapsed_us=")
        .nth(1)
        .and_then(|r| r.split_whitespace().next())
        .and_then(|n| n.parse().ok())
        .expect("elapsed_us");
    assert!(
        elapsed_us > 100_000,
        "the reload took {elapsed_us} us, which is not slow enough to test the window"
    );
    let gap_us = stamp_us(again) - stamp_us(done);
    assert!(
        gap_us >= 95_000,
        "the second clear came {gap_us} us after the reload finished, under the 100 ms window"
    );
    s.proc.terminate(Duration::from_secs(10));
}
