//! m6-probe-h2 -- N sequential rustls handshakes with ALPN `h2`, timed.
//!
//! One job. No requests, no warmup, no charts, one connection at a time. The
//! figure is directly comparable to the `http/2/*` channel on m6-http's
//! `/perf`, which times the same span: after TCP connect, to handshake complete.
//!
//! Sequential on purpose. Opening connections concurrently puts accept-queue
//! delay inside the number, and on a 1-core VM that is milliseconds. If this and
//! `/perf` disagree, that queueing is the first thing to suspect, and it can only
//! be isolated by a client that provably is not causing it.

use m6_http_lib::probe;

fn main() {
    probe::install_crypto();
    let (addr, n) = probe::args("127.0.0.1:443", 100);
    let cfg = probe::tls_config(b"h2");

    let mut samples = Vec::with_capacity(n);
    let mut failures = 0usize;
    for _ in 0..n {
        match probe::tls_handshake(&addr, cfg.clone()) {
            // The negotiated ALPN is checked, not assumed. m6-http assigns the
            // channel by ALPN, so a connection that came back as something else
            // belongs in a different bucket and must not be counted here.
            Ok((d, alpn)) if alpn.as_deref() == Some(b"h2".as_slice()) => samples.push(d),
            Ok((_, alpn)) => {
                failures += 1;
                eprintln!(
                    "asked for h2, server negotiated {:?} -- not counted",
                    alpn.map(|a| String::from_utf8_lossy(&a).into_owned())
                );
            }
            Err(e) => {
                failures += 1;
                eprintln!("handshake failed: {e}");
            }
        }
    }
    probe::report(
        "h2 rustls handshake (excludes TCP connect)",
        samples,
        failures,
    );
    if samples_empty(failures, n) {
        std::process::exit(1);
    }
}

/// Exit non-zero when nothing succeeded. A probe that measured nothing must not
/// look like a probe that measured a healthy server.
fn samples_empty(failures: usize, n: usize) -> bool {
    failures == n
}
