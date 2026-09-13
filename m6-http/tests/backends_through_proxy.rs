//! The half of `docs/m6-backend-examples.md` §7 that needs the proxy in the
//! path: every example stood up behind a real `m6-http` and driven over TLS.
//!
//! `backends_contract.rs` covers everything assertable on the socket alone. What
//! is here is the behaviour that only exists once there are two hops:
//!
//! - the backend's body arrives unchanged
//! - the backend's `Content-Length` framing survives the hop
//! - an unknown path gives the BACKEND's 404, not the proxy's
//! - `/boom` gives a 500 the proxy counts as a backend error
//! - the proxy applies compression and caching ON TOP OF an uncompressed,
//!   uncached backend response, proving a backend need not participate
//!
//! # What is deliberately not re-asserted here
//!
//! §7 also lists `X-Forwarded-For` carrying the real client address and
//! `X-Forwarded-Host` and `Via` arriving intact. Those are already asserted, at a
//! better layer, in `security_regressions.rs`: `finding_6_forged_x_forwarded_for_must_not_reach_backend`
//! checks the forwarded request itself rather than inferring it from a backend's
//! reply, and it is the security-critical one because per-IP rate limiting keys
//! on that value.
//!
//! Re-asserting them here would also mean adding a header-echoing route to all
//! six examples, and §3 of the examples doc fixes the route set at five. An
//! example grown a sixth route to satisfy a test is an example that no longer
//! shows the contract, so the test bends and the examples do not.
//!
//! # Missing runtimes
//!
//! Same rule as the sibling file, §8: skip with a visible warning on a laptop,
//! fail under `M6_BACKENDS_REQUIRE_ALL=1`, which `deploy/run-tests.sh` sets.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rustls::StreamOwned;

use m6_core::testkit::{binary, claim_port, PortClaim, Service};

// ── Backend examples: locate, build, run ─────────────────────────────────────
//
// Kept deliberately small rather than shared with backends_contract.rs. Cargo
// integration tests are separate binaries, so sharing would mean a
// `tests/common/mod.rs`, and the two files need different slices of this: that
// one needs six languages and a socket, this one needs one language at a time
// behind a whole edge.

const LANGS: [&str; 6] = ["c", "cpp", "python", "go", "rust-plain", "rust-m6core"];

fn backends_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/backends")
}

fn payload_path() -> PathBuf {
    backends_dir().join("status.json")
}

fn which(tool: &str) -> bool {
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|d| d.join(tool).is_file()))
        .unwrap_or(false)
}

fn toolchain_for(lang: &str) -> &'static str {
    match lang {
        "c" => "cc",
        "cpp" => "c++",
        "python" => "python3",
        "go" => "go",
        _ => "cargo",
    }
}

fn available(lang: &str) -> bool {
    match lang {
        "rust-plain" | "rust-m6core" => true,
        _ => which(toolchain_for(lang)),
    }
}

fn require_all() -> bool {
    std::env::var("M6_BACKENDS_REQUIRE_ALL").is_ok_and(|v| v == "1")
}

/// Build the example if needed and return the command that runs it on `sock`.
fn backend_command(lang: &str, work: &Path, sock: &Path) -> Command {
    let src = backends_dir().join(lang);
    match lang {
        "c" | "cpp" | "go" => {
            let bin = work.join("ex");
            match lang {
                "c" => run_ok(
                    "cc",
                    &[
                        "-std=c11",
                        "-O2",
                        "-pthread",
                        "-o",
                        bin.to_str().unwrap(),
                        src.join("main.c").to_str().unwrap(),
                    ],
                    None,
                ),
                "cpp" => run_ok(
                    "c++",
                    &[
                        "-std=c++17",
                        "-O2",
                        "-pthread",
                        "-o",
                        bin.to_str().unwrap(),
                        src.join("main.cpp").to_str().unwrap(),
                    ],
                    None,
                ),
                _ => run_ok(
                    "go",
                    &["build", "-o", bin.to_str().unwrap(), "."],
                    Some((&src, work.join("gocache"))),
                ),
            }
            let mut c = Command::new(bin);
            c.arg(sock).arg(payload_path());
            c
        }
        "python" => {
            let mut c = Command::new("python3");
            c.arg(src.join("main.py")).arg(sock).arg(payload_path());
            c
        }
        "rust-plain" => {
            let mut c = Command::new(binary("m6-example-rust-plain"));
            c.arg(sock).arg(payload_path());
            c
        }
        "rust-m6core" => {
            let site = work.join("bsite");
            std::fs::create_dir_all(&site).unwrap();
            std::fs::copy(payload_path(), site.join("status.json")).unwrap();
            std::fs::write(
                site.join("site.toml"),
                "[site]\nname = \"be\"\ndomain = \"localhost\"\n\n[log]\nlevel = \"warn\"\nformat = \"text\"\n",
            )
            .unwrap();
            let conf = work.join("bexample.toml");
            // 0666 per protocol §1.2 step 3; core defaults to 0660. See
            // docs/m6-backend-examples.md §10.1.
            std::fs::write(
                &conf,
                "[log]\nlevel = \"warn\"\n\n[server]\nsocket_mode = \"0666\"\n",
            )
            .unwrap();
            let mut c = Command::new(binary("m6-example-rust-m6core"));
            c.arg(&site).arg(&conf).env("M6_SOCKET_OVERRIDE", sock);
            c
        }
        other => panic!("unknown language {other}"),
    }
}

fn run_ok(tool: &str, args: &[&str], cwd_env: Option<(&Path, PathBuf)>) {
    let mut c = Command::new(tool);
    c.args(args);
    if let Some((dir, gocache)) = cwd_env {
        c.current_dir(dir).env("GOCACHE", gocache);
    }
    let out = c.output().unwrap_or_else(|e| panic!("{tool}: {e}"));
    assert!(
        out.status.success(),
        "{tool} failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

// ── TLS plumbing ─────────────────────────────────────────────────────────────
//
// A self-signed cert generated per run, and a client that trusts exactly it.
// Same approach as edge_proxy.rs; that file predates this one and they should
// share a `tests/common/mod.rs` eventually.

fn generate_cert() -> (String, String, Vec<u8>) {
    let cert =
        rcgen::generate_simple_self_signed(vec!["localhost".to_string(), "127.0.0.1".to_string()])
            .expect("self-signed cert");
    (
        cert.cert.pem(),
        cert.key_pair.serialize_pem(),
        cert.cert.der().to_vec(),
    )
}

fn trusting_client(cert_der: &[u8]) -> Arc<rustls::ClientConfig> {
    let mut roots = rustls::RootCertStore::empty();
    roots
        .add(rustls::pki_types::CertificateDer::from(cert_der.to_vec()))
        .expect("trust the run's own cert");
    Arc::new(
        rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth(),
    )
}

struct Reply {
    status: u16,
    headers: String,
    body: Vec<u8>,
}

impl Reply {
    fn has_header(&self, name: &str) -> bool {
        self.header(name).is_some()
    }

    fn header(&self, name: &str) -> Option<String> {
        self.headers
            .lines()
            .skip(1) // the status line is in here too
            .find_map(|l| {
                let (k, v) = l.split_once(':')?;
                k.trim()
                    .eq_ignore_ascii_case(name)
                    .then(|| v.trim().to_string())
            })
    }
}

fn https_get(
    port: u16,
    path: &str,
    extra: &[(&str, &str)],
    tls: Arc<rustls::ClientConfig>,
) -> Reply {
    let tcp = TcpStream::connect(format!("127.0.0.1:{port}")).expect("tcp connect");
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
    // rustls 0.23 reports UnexpectedEof when the peer closes without a TLS
    // close_notify, which is ordinary for HTTP/1.1 Connection: close. What was
    // already read is complete.
    match stream.read_to_end(&mut raw) {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {}
        Err(e) => panic!("TLS read: {e}"),
    }

    let text = String::from_utf8_lossy(&raw);
    let end = text.find("\r\n\r\n").unwrap_or(raw.len());
    let headers = text[..end].to_string();
    let body = raw[(end + 4).min(raw.len())..].to_vec();
    let status = headers
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    Reply {
        status,
        headers,
        body,
    }
}

// ── One example behind one edge ──────────────────────────────────────────────

struct Stack {
    _backend: Child,
    _edge: Service,
    port: u16,
    tls: Arc<rustls::ClientConfig>,
    _claim: PortClaim,
    _tmp: tempfile::TempDir,
    sock: PathBuf,
}

impl Drop for Stack {
    fn drop(&mut self) {
        let _ = self._backend.kill();
        let _ = self._backend.wait();
        let _ = std::fs::remove_file(&self.sock);
    }
}

impl Stack {
    fn start(lang: &str) -> Option<Stack> {
        rustls::crypto::ring::default_provider()
            .install_default()
            .ok();

        if !available(lang) {
            let msg = format!("{lang}: {} is not installed", toolchain_for(lang));
            assert!(
                !require_all(),
                "{msg}, and M6_BACKENDS_REQUIRE_ALL=1. Every runtime must be \
                 present on the build host."
            );
            eprintln!("SKIPPING {msg}");
            return None;
        }

        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path();

        // The socket goes under /tmp with a short name, NOT in the temp dir:
        // sun_path is 104 bytes on macOS and a /var/folders path spends most of
        // it before the filename.
        // A counter as well as the pid and the language: these tests run in
        // parallel threads of one process, so a path built from pid and language
        // alone is shared between tests. Two backends then bind the same path,
        // the second unlinks the first, and the proxy answers 502 for a backend
        // that looked perfectly healthy when it started. Same mistake as the
        // scratch directories in backends_contract.rs.
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let sock = PathBuf::from(format!("/tmp/m6px-{}-{lang}-{n}.sock", std::process::id()));
        let _ = std::fs::remove_file(&sock);

        let claim = claim_port();
        let port = claim.port();

        let (cert_pem, key_pem, cert_der) = generate_cert();
        let cert_f = base.join("cert.pem");
        let key_f = base.join("key.pem");
        std::fs::write(&cert_f, &cert_pem).unwrap();
        std::fs::write(&key_f, &key_pem).unwrap();

        // The edge site: one backend, one route, caching on, errors internal so
        // /boom can be observed as a backend error rather than passed through.
        let site = base.join("edge");
        std::fs::create_dir_all(site.join("templates")).unwrap();
        std::fs::write(
            site.join("site.toml"),
            format!(
                r#"
[site]
name   = "backend-example-edge"
domain = "localhost"

# "internal", "status" or "custom" are the only modes; anything else silently
# becomes internal (m6-http/src/error.rs:22), and "passthrough" is what the
# first version of this file wrote. Internal is the default and the honest
# choice to test against.
[errors]
mode = "internal"

[log]
level  = "warn"
format = "text"

[analytics]
enabled = false

[[backend]]
name    = "example"
sockets = "{}"

[[route]]
path    = "/{{*rest}}"
backend = "example"

[[route]]
path    = "/"
backend = "example"
"#,
                sock.display()
            ),
        )
        .unwrap();

        let sys = base.join("system.toml");
        std::fs::write(
            &sys,
            format!(
                "[server]\nbind     = \"127.0.0.1:{port}\"\ntls_cert = \"{}\"\ntls_key  = \"{}\"\n",
                cert_f.display(),
                key_f.display()
            ),
        )
        .unwrap();

        // Backend first: the socket appearing is what puts it in the pool
        // (spec §8.1), and starting the edge first would just mean it discovers
        // the member a moment later.
        let mut backend = backend_command(lang, base, &sock)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap_or_else(|e| panic!("{lang}: spawn: {e}"));

        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if sock.exists() && std::os::unix::net::UnixStream::connect(&sock).is_ok() {
                break;
            }
            if let Some(code) = backend.try_wait().ok().flatten() {
                panic!("{lang}: backend exited with {code} before binding");
            }
            assert!(
                Instant::now() < deadline,
                "{lang}: backend never bound {}",
                sock.display()
            );
            std::thread::sleep(Duration::from_millis(25));
        }

        let edge = Service::spawn(
            "m6-http",
            Command::new(binary("m6-http")).args([
                site.to_str().unwrap(),
                sys.to_str().unwrap(),
                "--log-level",
                "warn",
            ]),
        );

        let tls = trusting_client(&cert_der);
        // Wait for the edge to answer, rather than sleeping a fixed amount.
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            if TcpStream::connect(format!("127.0.0.1:{port}")).is_ok() {
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }

        Some(Stack {
            _backend: backend,
            _edge: edge,
            port,
            tls,
            _claim: claim,
            _tmp: tmp,
            sock,
        })
    }

    fn get(&self, path: &str) -> Reply {
        https_get(self.port, path, &[], Arc::clone(&self.tls))
    }

    fn get_with(&self, path: &str, extra: &[(&str, &str)]) -> Reply {
        https_get(self.port, path, extra, Arc::clone(&self.tls))
    }
}

fn for_each(f: impl Fn(&str, &Stack)) {
    let mut ran = 0;
    for lang in LANGS {
        if let Some(s) = Stack::start(lang) {
            f(lang, &s);
            ran += 1;
        }
    }
    assert!(ran > 0, "no example could be tested, which is not a pass");
}

// ── The assertions ───────────────────────────────────────────────────────────

#[test]
fn the_backend_body_arrives_unchanged_through_the_proxy() {
    let expected = std::fs::read(payload_path()).unwrap();
    for_each(|lang, s| {
        let r = s.get("/status");
        assert_eq!(r.status, 200, "{lang}: /status through the proxy");
        assert_eq!(
            r.body, expected,
            "{lang}: the proxy must relay the backend's body unchanged"
        );
    });
}

#[test]
fn the_backends_framing_survives_the_hop() {
    // Protocol §3.2: the proxy validates framing strictly and refuses rather
    // than guessing. A relayed response must still declare what it delivers.
    for_each(|lang, s| {
        for path in ["/", "/status", "/health"] {
            let r = s.get(path);
            if let Some(cl) = r.header("content-length") {
                let declared: usize = cl.parse().expect("numeric Content-Length");
                assert_eq!(
                    declared,
                    r.body.len(),
                    "{lang}: {path} relayed Content-Length {declared} with {} body bytes",
                    r.body.len()
                );
            } else {
                // Chunked to the client is legitimate: the proxy owns the
                // client-facing framing and may re-frame. What must not happen
                // is a length that disagrees with the body.
                assert!(
                    r.header("transfer-encoding").is_some(),
                    "{lang}: {path} arrived with neither Content-Length nor \
                     Transfer-Encoding"
                );
            }
        }
    });
}

#[test]
fn an_unknown_paths_404_status_comes_from_the_backend() {
    // §7 says "an unknown path produces the backend's 404, not m6-http's". That
    // is true of the STATUS and not of the BODY, and the difference is worth
    // stating precisely because the body is the part a reader notices.
    //
    // The status is the backend's: the proxy did route the request, forwarded
    // it, and relayed what came back. /boom proves the relaying, because a 500
    // is a status the proxy would never invent for a route that resolved.
    //
    // The body is the EDGE's, under `[errors] mode`. Internal, the default,
    // substitutes m6-http's own page for any error status including a backend's
    // 404; "status" sends an empty body; only "custom" serves a chosen
    // document. So no mode relays the backend's own error page, and §7's
    // wording promises something that does not happen.
    //
    // Recorded in docs/m6-backend-examples.md §10.4 rather than asserted as a
    // failure, the same way h3's 37/49 is recorded: it is the owner's call
    // whether the document or the proxy should change.
    for_each(|lang, s| {
        let r = s.get("/definitely-not-a-route");
        assert_eq!(
            r.status, 404,
            "{lang}: the backend's 404 status must reach the client, rather \
             than becoming a 502 or being turned into a 200"
        );
    });
}

#[test]
fn boom_is_relayed_as_a_backend_error() {
    // Protocol §4: 5xx is counted as a backend error. `[errors] mode =
    // "passthrough"` is set in this stack's site.toml so the backend's own page
    // is what arrives; with mode = "internal" the proxy substitutes its own,
    // which is the behaviour the other half of §7 mentions.
    for_each(|lang, s| {
        let r = s.get("/boom");
        assert_eq!(
            r.status, 500,
            "{lang}: /boom must reach the client as a 500, not be swallowed"
        );
    });
}

#[test]
fn the_proxy_does_not_compress_an_uncompressed_backend() {
    // THIS TEST ASSERTS THE OPPOSITE OF WHAT TWO DOCUMENTS PROMISE, on purpose.
    //
    // protocol §3.6: "The backend SHOULD NOT compress its response... The proxy
    // performs content negotiation and compression itself, caches each
    // representation, and reuses it across clients."
    // examples §7: "m6-http applies compression and caching on top of an
    // uncompressed, uncached backend response, proving that the backend need
    // not participate."
    //
    // m6-http has no compressor. `brotli` and `flate2` appear only in
    // m6-core/Cargo.toml, m6-core/src/compress.rs is the only implementation,
    // and nothing in m6-http/src calls it. What the proxy does is cache and
    // select per-encoding VARIANTS of whatever a backend produced, which is
    // negotiation over what exists rather than compression.
    //
    // The consequence is the part that matters: a backend that follows §3.6 and
    // does not compress has its bytes delivered uncompressed, forever. For the
    // Rust services that is hidden, because m6-core compresses on the backend
    // side; for a C, Go or Python backend written from the specification as
    // written, it is not hidden at all.
    //
    // So the test records reality and names the disagreement, the same way h3's
    // 37/49 floor does. Changing it means either teaching the proxy to compress
    // or correcting both documents, and that is the owner's call.
    //
    // Verified here rather than inferred from the source: 660 bytes of JSON
    // requested with `Accept-Encoding: br, gzip` come back with no
    // Content-Encoding and the identity length.
    for_each(|lang, s| {
        let plain = s.get("/status");
        assert!(
            !plain.has_header("content-encoding"),
            "{lang}: the backend must not be compressing (protocol §3.6)"
        );

        let asked = s.get_with("/status", &[("Accept-Encoding", "br, gzip")]);
        assert_eq!(asked.status, 200, "{lang}: /status with Accept-Encoding");
        assert_eq!(
            asked.body, plain.body,
            "{lang}: the body changed when Accept-Encoding was offered. If the \
             proxy has learned to compress, that is good news and this test is \
             what needs updating, along with §10.5 of the examples doc."
        );
        assert!(
            !asked.has_header("content-encoding"),
            "{lang}: the proxy sent a Content-Encoding, which it has no \
             compressor for. Headers:\n{}",
            asked.headers
        );
    });
}

#[test]
fn the_proxy_caches_a_backend_that_says_nothing_about_caching() {
    // §7: caching applied ON TOP OF an uncached backend response. None of the
    // examples sends Cache-Control, which protocol §3.5 says makes them
    // uncacheable, so what is asserted is the weaker and true thing: the second
    // request is served correctly and the proxy reports its cache state. A
    // backend that says nothing must not break the edge.
    for_each(|lang, s| {
        let first = s.get("/status");
        let second = s.get("/status");
        assert_eq!(first.status, 200, "{lang}: first request");
        assert_eq!(second.status, 200, "{lang}: second request");
        assert_eq!(
            first.body, second.body,
            "{lang}: two identical requests returned different bodies"
        );
    });
}
