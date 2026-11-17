//! Where a server-side TLS 1.3 handshake spends its CPU, with no sockets.
//!
//! A rustls client and server run in one thread and swap bytes through memory.
//! Only the server's calls are timed, so the figure is the server's rustls work
//! and nothing else: no accept, no read or write syscall, no event loop. What a
//! real server adds on top is the difference between this and the server CPU per
//! handshake that `bench.sh --handshake` reports.
//!
//! Two ways of cutting the handshake up:
//!
//!   * by phase: the ClientHello call (key share, ECDHE, key schedule, signature,
//!     certificate, server flight) and the client-Finished call (verify, session
//!     tickets);
//!   * by primitive, each timed alone: key share generation plus ECDHE as the
//!     server does it, the CertificateVerify signature, and a ticket encryption.
//!
//! Run: `cargo bench -p m6-http --bench handshake_parts` (HS_N sets the count).

use std::io::Cursor;
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, SupportedKxGroup};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::{
    ClientConfig, ClientConnection, DigitallySignedStruct, HandshakeKind, ServerConfig,
    ServerConnection, SignatureScheme,
};

#[derive(Debug)]
struct NoVerify(Arc<CryptoProvider>);

impl ServerCertVerifier for NoVerify {
    fn verify_server_cert(
        &self,
        _: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        _: &[u8],
        _: &CertificateDer<'_>,
        _: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }
    fn verify_tls13_signature(
        &self,
        _: &[u8],
        _: &CertificateDer<'_>,
        _: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

struct Key {
    name: &'static str,
    chain: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
}

fn keys(dir: &std::path::Path) -> Vec<Key> {
    use rustls::pki_types::pem::PemObject;
    let ec = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).expect("ec cert");
    let mut out = vec![Key {
        name: "ECDSA P-256",
        chain: vec![ec.cert.der().clone()],
        key: PrivateKeyDer::from_pem_slice(ec.key_pair.serialize_pem().as_bytes()).expect("ec key"),
    }];
    let (c, k) = (dir.join("rsa.crt"), dir.join("rsa.key"));
    let ok = Command::new("openssl")
        .args([
            "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "2",
        ])
        .args([
            "-subj",
            "/CN=localhost",
            "-addext",
            "subjectAltName=DNS:localhost",
        ])
        .arg("-keyout")
        .arg(&k)
        .arg("-out")
        .arg(&c)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if ok {
        out.push(Key {
            name: "RSA-2048",
            chain: CertificateDer::pem_file_iter(&c)
                .expect("rsa cert")
                .collect::<Result<_, _>>()
                .expect("rsa cert"),
            key: PrivateKeyDer::from_pem_file(&k).expect("rsa key"),
        });
    } else {
        eprintln!("openssl not available: RSA rows skipped");
    }
    out
}

/// The one provider this tree builds, the same one m6-http's listener uses.
fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::aws_lc_rs::default_provider())
}

fn ticketer() -> Arc<dyn rustls::server::ProducesTickets> {
    rustls::crypto::aws_lc_rs::Ticketer::new().expect("ticketer")
}

/// The server as m6-http builds it: one certificate, ALPN, a ticketer.
fn server_config(k: &Key, tickets: Option<usize>) -> Arc<ServerConfig> {
    let mut c = ServerConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .expect("versions")
        .with_no_client_auth()
        .with_single_cert(k.chain.clone(), k.key.clone_key())
        .expect("cert");
    c.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    c.ticketer = ticketer();
    if let Some(n) = tickets {
        c.send_tls13_tickets = n;
    }
    Arc::new(c)
}

fn client_config(resume: bool) -> Arc<ClientConfig> {
    client_config_with(provider(), resume)
}

/// The same client with only the named key exchange groups on offer.
fn client_config_kx(group: &str) -> Arc<ClientConfig> {
    let mut pr = Arc::try_unwrap(provider()).expect("sole owner");
    pr.kx_groups.retain(|g| format!("{:?}", g.name()) == group);
    client_config_with(Arc::new(pr), false)
}

fn client_config_with(pr: Arc<CryptoProvider>, resume: bool) -> Arc<ClientConfig> {
    let mut c = ClientConfig::builder_with_provider(pr.clone())
        .with_safe_default_protocol_versions()
        .expect("versions")
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(NoVerify(pr)))
        .with_no_client_auth();
    c.alpn_protocols = vec![b"h2".to_vec()];
    if !resume {
        c.resumption = rustls::client::Resumption::disabled();
    }
    Arc::new(c)
}

#[derive(Default, Clone, Copy)]
struct Sample {
    hello: Duration,
    finish: Duration,
    flight_bytes: usize,
}

/// One in-memory handshake, timing only the server's calls.
fn handshake(cc: &Arc<ClientConfig>, sc: &Arc<ServerConfig>) -> (Sample, HandshakeKind) {
    let mut c =
        ClientConnection::new(cc.clone(), "localhost".try_into().expect("name")).expect("client");
    let mut s = ServerConnection::new(sc.clone()).expect("server");
    let mut buf = Vec::with_capacity(16384);
    let mut out = Sample::default();
    let mut deliveries = 0;
    while c.is_handshaking() || s.is_handshaking() || c.wants_write() || s.wants_write() {
        let mut progressed = false;
        while c.wants_write() {
            buf.clear();
            c.write_tls(&mut buf).expect("client write");
            let t = Instant::now();
            s.read_tls(&mut Cursor::new(&buf)).expect("server read");
            s.process_new_packets().expect("server process");
            let d = t.elapsed();
            if deliveries == 0 {
                out.hello += d;
            } else {
                out.finish += d;
            }
            deliveries += 1;
            progressed = true;
        }
        while s.wants_write() {
            buf.clear();
            let t = Instant::now();
            s.write_tls(&mut buf).expect("server write");
            let d = t.elapsed();
            if deliveries <= 1 {
                out.hello += d;
                out.flight_bytes += buf.len();
            } else {
                out.finish += d;
            }
            c.read_tls(&mut Cursor::new(&buf)).expect("client read");
            c.process_new_packets().expect("client process");
            progressed = true;
        }
        if !progressed {
            break;
        }
    }
    (out, s.handshake_kind().expect("done"))
}

fn pct(v: &mut [Duration], p: usize) -> Duration {
    v.sort_unstable();
    v[(v.len() - 1) * p / 100]
}

fn us(d: Duration) -> f64 {
    d.as_secs_f64() * 1e6
}

fn time_n<F: FnMut()>(n: usize, mut f: F) -> Duration {
    let mut v: Vec<Duration> = (0..n)
        .map(|_| {
            let t = Instant::now();
            f();
            t.elapsed()
        })
        .collect();
    pct(&mut v, 50)
}

fn main() {
    let n: usize = std::env::var("HS_N")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(2000);
    let dir = tempfile::tempdir().expect("tempdir");
    let keys = keys(dir.path());
    println!(
        "profile: {}   N={n}   microseconds, p50 unless noted\n",
        if cfg!(debug_assertions) {
            "DEBUG (numbers are not valid)"
        } else {
            "release"
        }
    );

    println!("== Server handshake CPU, in memory (no syscalls, no loop) ==");
    println!(
        "{:<10} {:<12} {:<9} {:>8} {:>8} {:>8} {:>8} {:>8} {:>9}",
        "provider", "key", "kind", "total", "p99", "hello", "finish", "tickets0", "flightB"
    );
    for p in ["aws-lc-rs"] {
        for k in &keys {
            for kind in ["full", "resumed"] {
                let sc = server_config(k, None);
                let sc0 = server_config(k, Some(0));
                let cc = client_config(kind == "resumed");
                // Warm up, and for "resumed" earn the ticket.
                for _ in 0..50 {
                    handshake(&cc, &sc);
                }
                let mut tot = Vec::new();
                let mut hello = Vec::new();
                let mut fin = Vec::new();
                let mut bytes = 0;
                let mut resumed = 0;
                for _ in 0..n {
                    let (s, hk) = handshake(&cc, &sc);
                    tot.push(s.hello + s.finish);
                    hello.push(s.hello);
                    fin.push(s.finish);
                    bytes = s.flight_bytes;
                    resumed += usize::from(hk == HandshakeKind::Resumed);
                }
                // Same, with no post-handshake tickets sent.
                let mut t0 = Vec::new();
                for _ in 0..n {
                    let (s, _) = handshake(&cc, &sc0);
                    t0.push(s.hello + s.finish);
                }
                println!(
                    "{:<10} {:<12} {:<9} {:>8.1} {:>8.1} {:>8.1} {:>8.1} {:>8} {:>9}",
                    p,
                    k.name,
                    if kind == "resumed" {
                        format!("res {}%", resumed * 100 / n)
                    } else {
                        kind.to_string()
                    },
                    us(pct(&mut tot, 50)),
                    us(pct(&mut tot, 99)),
                    us(pct(&mut hello, 50)),
                    us(pct(&mut fin, 50)),
                    if kind == "full" {
                        format!("{:.1}", us(pct(&mut t0, 50)))
                    } else {
                        "-".into()
                    },
                    bytes
                );
            }
        }
    }

    // ── Key exchange: the client offers one group, the server must take it ────
    println!("\n== Key exchange group, ECDSA P-256, full handshake, in memory ==");
    println!(
        "{:<10} {:<16} {:>8} {:>8} {:>9}",
        "provider", "group", "total", "p99", "flightB"
    );
    for (p, groups) in [(
        "aws-lc-rs",
        vec!["X25519", "secp256r1", "secp384r1", "X25519MLKEM768"],
    )] {
        let sc = server_config(&keys[0], None);
        for g in groups {
            let cc = client_config_kx(g);
            for _ in 0..50 {
                handshake(&cc, &sc);
            }
            let mut tot = Vec::new();
            let mut bytes = 0;
            for _ in 0..n {
                let (s, _) = handshake(&cc, &sc);
                tot.push(s.hello + s.finish);
                bytes = s.flight_bytes;
            }
            println!(
                "{p:<10} {g:<16} {:>8.1} {:>8.1} {:>9}",
                us(pct(&mut tot, 50)),
                us(pct(&mut tot, 99)),
                bytes
            );
        }
    }

    cert_compression_section(&keys, n);

    println!("\n== Primitives, each alone, as the server calls them ==");
    println!("{:<10} {:<28} {:>8}", "provider", "operation", "us");
    for p in ["aws-lc-rs"] {
        let pr = provider();
        for g in pr.kx_groups.iter() {
            let name = format!("{:?}", g.name());
            // Client side makes the share the server answers.
            let client = g.start().expect("start");
            let share = client.pub_key().to_vec();
            let start = time_n(n, || {
                let _ = g.start().expect("start");
            });
            let both = time_n(n, || {
                let _ = g.start_and_complete(&share).expect("complete");
            });
            println!(
                "{p:<10} {:<28} {:>8.1}",
                format!("kx {name} start (share)"),
                us(start)
            );
            println!(
                "{p:<10} {:<28} {:>8.1}",
                format!("kx {name} server total"),
                us(both)
            );
        }
        for k in &keys {
            let signer = pr
                .key_provider
                .load_private_key(k.key.clone_key())
                .expect("key");
            let schemes = [
                SignatureScheme::ECDSA_NISTP256_SHA256,
                SignatureScheme::RSA_PSS_SHA256,
            ];
            let s = signer.choose_scheme(&schemes).expect("scheme");
            let msg = [0x55u8; 130];
            let t = time_n(n, || {
                let _ = s.sign(&msg).expect("sign");
            });
            println!("{p:<10} {:<28} {:>8.1}", format!("sign {} ", k.name), us(t));
        }
        let tk = ticketer();
        let state = [7u8; 120];
        let t = time_n(n, || {
            let _ = tk.encrypt(&state).expect("encrypt");
        });
        println!("{p:<10} {:<28} {:>8.1}", "ticket encrypt (120 B)", us(t));
    }
    let _: Option<&dyn SupportedKxGroup> = None;
}

/// A chain of the shape a public CA serves: leaf and one intermediate, with the
/// names, key usages and a long-ish subject a real certificate carries.
#[cfg(feature = "cert-compression")]
fn realistic_chain(rsa: bool) -> (Vec<CertificateDer<'static>>, PrivateKeyDer<'static>) {
    use rcgen::{CertificateParams, DnType, IsCa, KeyPair};
    let alg = if rsa {
        &rcgen::PKCS_RSA_SHA256
    } else {
        &rcgen::PKCS_ECDSA_P256_SHA256
    };
    let _ = alg;
    let ca_key = KeyPair::generate().expect("ca key");
    let mut ca = CertificateParams::new(Vec::<String>::new()).expect("ca params");
    ca.is_ca = IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    ca.distinguished_name.push(DnType::CountryName, "US");
    ca.distinguished_name
        .push(DnType::OrganizationName, "Example Trust Services");
    ca.distinguished_name
        .push(DnType::CommonName, "Example Trust Services R3");
    let ca_cert = ca.self_signed(&ca_key).expect("ca");
    let key = KeyPair::generate().expect("leaf key");
    let mut leaf = CertificateParams::new(vec![
        "www.example.net".to_string(),
        "example.net".to_string(),
        "localhost".to_string(),
    ])
    .expect("leaf params");
    leaf.distinguished_name
        .push(DnType::CommonName, "example.net");
    let cert = leaf.signed_by(&key, &ca_cert, &ca_key).expect("leaf");
    (
        vec![cert.der().clone(), ca_cert.der().clone()],
        PrivateKeyDer::try_from(key.serialize_der()).expect("pkcs8"),
    )
}

#[cfg(feature = "cert-compression")]
fn cert_compression_section(_keys: &[Key], n: usize) {
    use rustls::compress::{BROTLI_COMPRESSOR, ZLIB_COMPRESSOR};
    println!("\n== Certificate compression (RFC 8879), ECDSA, full handshake, in memory ==");
    println!(
        "{:<10} {:<10} {:>8} {:>9}",
        "provider", "mode", "total", "flightB"
    );
    let (chain, key) = realistic_chain(false);
    let k = Key {
        name: "chain",
        chain,
        key,
    };
    for p in ["aws-lc-rs"] {
        for mode in ["none", "zlib", "brotli", "both"] {
            let mut sc = ServerConfig::builder_with_provider(provider())
                .with_safe_default_protocol_versions()
                .expect("versions")
                .with_no_client_auth()
                .with_single_cert(k.chain.clone(), k.key.clone_key())
                .expect("cert");
            sc.ticketer = ticketer();
            sc.cert_compressors = match mode {
                "none" => vec![],
                "zlib" => vec![ZLIB_COMPRESSOR],
                "brotli" => vec![BROTLI_COMPRESSOR],
                _ => vec![BROTLI_COMPRESSOR, ZLIB_COMPRESSOR],
            };
            let sc = Arc::new(sc);
            let cc = client_config(false);
            for _ in 0..20 {
                handshake(&cc, &sc);
            }
            let mut tot = Vec::new();
            let mut bytes = 0;
            for _ in 0..n {
                let (s, _) = handshake(&cc, &sc);
                tot.push(s.hello + s.finish);
                bytes = s.flight_bytes;
            }
            println!(
                "{p:<10} {mode:<10} {:>8.1} {:>9}",
                us(pct(&mut tot, 50)),
                bytes
            );
        }
    }
    println!(
        "chain sent: {} B of certificates (leaf + intermediate)",
        k.chain.iter().map(|c| c.len()).sum::<usize>()
    );
    // A real chain's size, compressed offline: HS_REAL_CHAIN is a PEM file.
    if let Ok(path) = std::env::var("HS_REAL_CHAIN") {
        use rustls::pki_types::pem::PemObject;
        let ders: Vec<CertificateDer<'static>> = CertificateDer::pem_file_iter(&path)
            .expect("chain file")
            .collect::<Result<_, _>>()
            .expect("chain");
        let raw: Vec<u8> = ders
            .iter()
            .flat_map(|d| d.as_ref().iter().copied())
            .collect();
        let level = rustls::compress::CompressionLevel::Interactive;
        let b = BROTLI_COMPRESSOR
            .compress(raw.clone(), level)
            .expect("brotli");
        let z = ZLIB_COMPRESSOR.compress(raw.clone(), level).expect("zlib");
        println!(
            "real chain {path}: {} certs, {} B raw, {} B brotli, {} B zlib",
            ders.len(),
            raw.len(),
            b.len(),
            z.len()
        );
    }
}

#[cfg(not(feature = "cert-compression"))]
fn cert_compression_section(_keys: &[Key], _n: usize) {
    println!("\n(certificate compression rows need --features cert-compression)");
}
