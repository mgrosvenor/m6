//! m6-probe-h1 -- N sequential rustls handshakes with ALPN `http/1.1`, timed,
//! and split into full and RESUMED.
//!
//! One job. No requests, no warmup, no charts, one connection at a time. The
//! figures are directly comparable to the `http/1.1/*` channel on m6-http's
//! `/perf`, which times the same span: after TCP connect, to handshake complete,
//! and which also reports full and resumed separately.
//!
//! The split is here for the same reason as in m6-probe-h2, and this channel is
//! the control for that one: the fleet reads 88-96% resumed on http/1.1 and 0%
//! on http/2, so a probe that agrees with the server here and disagrees there
//! localises the disagreement. See m6 issue #101. `--no-resume` builds a fresh
//! config per connection and measures full handshakes only.
//!
//! Sequential on purpose. Opening connections concurrently puts accept-queue
//! delay inside the number, and on a 1-core VM that is milliseconds. If this and
//! `/perf` disagree, that queueing is the first thing to suspect, and it can only
//! be isolated by a client that provably is not causing it.

use m6_http_lib::probe;

fn main() {
    probe::install_crypto();
    let (addr, n) = probe::args("127.0.0.1:443", 100);
    let no_resume = std::env::args().any(|a| a == "--no-resume");
    // ONE config, shared, so the session store persists between connections.
    let shared = probe::tls_config(b"http/1.1");

    let mut full = Vec::with_capacity(n);
    let mut resumed = Vec::with_capacity(n);
    let mut failures = 0usize;
    for _ in 0..n {
        let cfg = if no_resume {
            probe::tls_config(b"http/1.1")
        } else {
            shared.clone()
        };
        match probe::tls_handshake(&addr, cfg) {
            // The negotiated ALPN is checked, not assumed. m6-http assigns the
            // channel by ALPN, so a connection that came back as something else
            // belongs in a different bucket and must not be counted here.
            Ok(h) if h.alpn.as_deref() == Some(b"http/1.1".as_slice()) => {
                if h.resumed {
                    resumed.push(h.elapsed);
                } else {
                    full.push(h.elapsed);
                }
            }
            Ok(h) => {
                failures += 1;
                eprintln!(
                    "asked for http/1.1, server negotiated {:?} -- not counted",
                    h.alpn.map(|a| String::from_utf8_lossy(&a).into_owned())
                );
            }
            Err(e) => {
                failures += 1;
                eprintln!("handshake failed: {e}");
            }
        }
    }
    let n_full = full.len();
    let n_resumed = resumed.len();
    probe::report("h1 full    (excludes TCP connect)", full, failures);
    probe::report("h1 resumed (excludes TCP connect)", resumed, 0);
    if !no_resume {
        let offered = n_full + n_resumed;
        if offered > 1 && n_resumed == 0 {
            println!(
                "RESUMPTION: none. {offered} handshakes, every one full, with a ticket \
                 offered from the second onward."
            );
        } else if offered > 0 {
            println!(
                "RESUMPTION: {n_resumed} of {offered} resumed ({:.0}%), {n_full} full",
                (n_resumed as f64 / offered as f64) * 100.0
            );
        }
    }
    // ── Exit status, so this can gate ───────────────────────────────────────
    //
    // Two distinct failures, both non-zero, because tools/conformance.sh runs
    // this and a probe that only ever exits 0 cannot gate anything:
    //
    //   nothing handshook at all  -- the server is not there or refused every one
    //   a ticket was offered and never accepted -- the server cannot resume
    //
    // The second is the one that matters here. m6 1.9.0 installed a ticketer for
    // browser resumption and nothing in the suite could tell whether it worked;
    // the fleet read 0% resumed for a day and that was indistinguishable from a
    // broken ticketer until a client offered one deliberately. m6 issue #101.
    if probe::resumption_failed(no_resume, n_full, n_resumed) {
        std::process::exit(1);
    }
}

// `samples_empty` is gone: the exit guard now asks whether either bucket has a
// sample, which is the same question without a helper that took the counts it
// was not given.
