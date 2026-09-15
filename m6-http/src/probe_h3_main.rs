//! m6-probe-h3 -- N sequential QUIC handshakes, timed to `is_established()`.
//!
//! One job. Comparable to the `http/3/*` channel on m6-http's `/perf`, which
//! times the same span.
//!
//! This figure is NOT comparable to the h1 and h2 probes, and the difference is
//! not a detail. QUIC folds transport and crypto together, so this includes the
//! round trip that rustls never sees because TCP had already finished by the time
//! rustls was handed the socket. Expect it to be several times the TLS figure on
//! the same box, and do not average the two.
//!
//! On 2026-09-15 this measurement was taken with `h3spec` instead, which is a
//! conformance tester that deliberately opens stalled and malformed connections.
//! It reported a p50 of 113ms on loopback. Our own client, doing only this,
//! reported 1.58ms. A broken or misapplied measuring tool does not leave you with
//! no number, it leaves you with a wrong one that gets written down.

use m6_http_lib::probe;

fn main() {
    // `--0rtt PATH` switches to the resumption measurement, because 0-RTT is a
    // different question from handshake cost: it asks how long until a RESPONSE,
    // having already held a ticket, and the answer should be shorter than any
    // handshake.
    let zero_rtt_path = std::env::args()
        .position(|a| a == "--0rtt")
        .and_then(|i| std::env::args().nth(i + 1));
    if let Some(path) = zero_rtt_path {
        let addr = std::env::args()
            .position(|a| a == "--addr")
            .and_then(|i| std::env::args().nth(i + 1))
            .unwrap_or_else(|| "127.0.0.1:443".to_string());
        // Defaults to GET. `--method POST` exercises the replayable-method gate.
        let method = std::env::args()
            .position(|a| a == "--method")
            .and_then(|i| std::env::args().nth(i + 1))
            .unwrap_or_else(|| "GET".to_string());
        match probe::zero_rtt_h3(&addr, &path, &method) {
            Ok(r) => {
                println!(
                    "cold handshake (no ticket held): {:.3}ms",
                    ms(r.cold_handshake)
                );
                if !r.got_ticket {
                    println!("NO RESUMPTION TICKET issued: 0-RTT is impossible, not merely off");
                    std::process::exit(1);
                }
                match r.warm_to_response {
                    Some(d) => println!(
                        "resumed, first packet to response headers: {:.3}ms  status={:?}",
                        ms(d),
                        r.status
                    ),
                    None => println!("resumed connection never answered inside 5s"),
                }
                // The claim, stated separately from the timing, because a server
                // that silently declines early data still answers correctly and
                // still looks fast on a warm path.
                println!(
                    "early data actually used: {}",
                    if r.early_data_used {
                        "YES -- the request was on the wire before the handshake completed"
                    } else {
                        "NO -- this was a normal 1-RTT request on a resumed connection"
                    }
                );
                if r.status == Some(425) {
                    println!(
                        "425 Too Early: the server took the early data and refused to ACT on it. \
                         The safety gate works; warm this path in cache to get the latency win."
                    );
                }
                if !r.early_data_used {
                    std::process::exit(1);
                }
            }
            Err(e) => {
                eprintln!("0-RTT probe failed: {e}");
                std::process::exit(1);
            }
        }
        return;
    }

    let (addr, n) = probe::args("127.0.0.1:443", 100);

    // One shaped handshake first, printed separately. The millisecond figure
    // cannot distinguish one round trip on a slow path from two on a fast one,
    // and only the second is something we can fix.
    match probe::quic_handshake_shape(&addr) {
        Ok(sh) => {
            println!(
                "handshake shape: {:.3}ms  client flights={}  server datagrams={}\n    \
                 client sent {}B, server sent {}B before established  (ratio {:.2}x, \
                 QUIC allows ~3x an unvalidated address)",
                sh.elapsed.as_secs_f64() * 1000.0,
                sh.client_flights,
                sh.server_datagrams,
                sh.client_bytes_before_established,
                sh.server_bytes_before_established,
                sh.server_bytes_before_established as f64
                    / sh.client_bytes_before_established.max(1) as f64
            );
            // Per-datagram arrivals, and the gaps between them. A total says a
            // handshake was slow; the gaps say where the time went, and a gap of
            // about one round trip means the server was waiting for the client
            // while a long gap with nothing owed means it was simply not sending.
            // The merged timeline, with the live amplification ratio at each
            // point. The ratio AT THE MOMENT the server stops is the one that
            // matters; an end-of-handshake ratio hides it.
            let mut csent = 0usize;
            let mut ssent = 0usize;
            for (at, who, cum) in &sh.timeline {
                if *who == "client" {
                    csent = *cum;
                } else {
                    ssent = *cum;
                }
                println!(
                    "    +{:>8.3}ms  {:<6} cum {:>5}B    client {:>5}B / server {:>5}B  = {:.2}x",
                    at.as_secs_f64() * 1000.0,
                    who,
                    cum,
                    csent,
                    ssent,
                    ssent as f64 / csent.max(1) as f64
                );
            }
            // The QUIC anti-amplification limit lets a server send only about 3x
            // what it has received until the client address is validated. A
            // certificate chain over that budget makes the server stop and wait,
            // which costs a whole round trip on every new connection.
            if sh.client_flights > 2 {
                println!(
                    "  NOTE: {} client flights for a 1-RTT handshake. If server bytes sit \
                     near 3x the client's opening datagram, the certificate chain is over \
                     the anti-amplification budget and is costing a round trip.",
                    sh.client_flights
                );
            }
        }
        Err(e) => eprintln!("shape probe failed: {e}"),
    }

    let mut samples = Vec::with_capacity(n);
    let mut failures = 0usize;
    for _ in 0..n {
        match probe::quic_handshake(&addr) {
            Ok(d) => samples.push(d),
            Err(e) => {
                failures += 1;
                eprintln!("handshake failed: {e}");
            }
        }
    }
    probe::report(
        "h3 QUIC handshake (INCLUDES the transport round trip)",
        samples,
        failures,
    );
    if failures == n {
        std::process::exit(1);
    }
}

fn ms(d: std::time::Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}
