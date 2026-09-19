//! m6-probe-h2 -- N sequential rustls handshakes with ALPN `h2`, timed, and
//! split into full and RESUMED.
//!
//! One job. No requests, no warmup, no charts, one connection at a time. The
//! figures are directly comparable to the `http/2/*` channel on m6-http's
//! `/perf`, which times the same span: after TCP connect, to handshake complete,
//! and which also reports full and resumed separately.
//!
//! Sequential on purpose. Opening connections concurrently puts accept-queue
//! delay inside the number, and on a 1-core VM that is milliseconds. If this and
//! `/perf` disagree, that queueing is the first thing to suspect, and it can only
//! be isolated by a client that provably is not causing it.
//!
//! ## The resumption split, and why it had to be added
//!
//! This probe reported ONE distribution. It has always shared a single
//! `Arc<ClientConfig>` across every connection, and rustls' default config
//! carries an in-memory session store, so handshake 1 earns a ticket and
//! handshakes 2..N offer it. Every run was therefore a mix of one full handshake
//! and N-1 attempted resumptions, reported as a single figure and described in
//! this comment as comparable to a channel that separates them.
//!
//! That mattered on 2026-09-19. m6 1.9.0 installed a ticketer specifically so
//! browser sessions could resume, the fleet was upgraded, and the monitor still
//! read **0% resumed on `http/2/external` on all three nodes** while reading
//! 88-96% on `http/1.1`. Two explanations fit that equally well:
//!
//!   1. the server does not resume, and 1.9.0's ticketer is not effective; or
//!   2. the server resumes fine and no real h2 client offers a ticket, because a
//!      browser opens ONE h2 connection per visit and multiplexes it, so a
//!      resumption only happens on a later visit.
//!
//! Timing cannot separate those and neither can the server's own counter, which
//! is one of the things in question. A client that deliberately offers a ticket
//! and reports what rustls says came back can. m6 issue #101.
//!
//! `--no-resume` builds a fresh config per connection, which measures full
//! handshakes only and reproduces this probe's previous behaviour.

use m6_http_lib::probe;

fn main() {
    probe::install_crypto();
    let (addr, n) = probe::args("127.0.0.1:443", 100);
    // A flag rather than a second binary: the two runs differ by one line and
    // both belong to the same measurement.
    let no_resume = std::env::args().any(|a| a == "--no-resume");

    // ONE config, shared, so the session store persists between connections and
    // handshake 2 onwards offers the ticket handshake 1 earned. With
    // --no-resume a fresh config per connection means each starts cold.
    let shared = probe::tls_config(b"h2");

    let mut full = Vec::with_capacity(n);
    let mut resumed = Vec::with_capacity(n);
    let mut failures = 0usize;
    for _ in 0..n {
        let cfg = if no_resume {
            probe::tls_config(b"h2")
        } else {
            shared.clone()
        };
        match probe::tls_handshake(&addr, cfg) {
            // The negotiated ALPN is checked, not assumed. m6-http assigns the
            // channel by ALPN, so a connection that came back as something else
            // belongs in a different bucket and must not be counted here.
            Ok(h) if h.alpn.as_deref() == Some(b"h2".as_slice()) => {
                if h.resumed {
                    resumed.push(h.elapsed);
                } else {
                    full.push(h.elapsed);
                }
            }
            Ok(h) => {
                failures += 1;
                eprintln!(
                    "asked for h2, server negotiated {:?} -- not counted",
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
    probe::report("h2 full    (excludes TCP connect)", full, failures);
    probe::report("h2 resumed (excludes TCP connect)", resumed, 0);

    // The headline, stated rather than left to be worked out from two n= values.
    //
    // With a shared store, one full handshake and the rest resumed is the healthy
    // shape. All-full means the server issued no usable ticket or declined the
    // one offered, which is the finding m6 #101 is about.
    if !no_resume {
        let offered = n_full + n_resumed;
        if offered > 1 && n_resumed == 0 {
            println!(
                "RESUMPTION: none. {offered} handshakes, every one full, with a ticket \
                 offered from the second onward. The server issued no usable ticket or \
                 declined it."
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
