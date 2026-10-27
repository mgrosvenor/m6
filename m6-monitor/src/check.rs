//! The hourly health check, printed.
//!
//! This is what `tools/health-check.py` did, from the binary instead. The
//! script sshed to three nodes, ran `journalctl`, `systemctl`, `df` and a
//! loopback `curl` on each, and parsed the analytics log over the wire. All of
//! that is now either a field on `/perf` or a field on `/traffic`, computed by
//! the node that owns the data.
//!
//! What the script kept getting wrong, three separate times, was measuring its
//! own effect on the fleet: it flagged its own load generator as a security
//! incident, read a latency regression off a single sample taken during that
//! load, and timed a 4ms loopback call immediately after its own 24-hour
//! journal scan. Two of those are gone here by construction, because this
//! generates no load and runs no scans. The third, that it is measuring from
//! wherever it happens to run, is unavoidable and is labelled instead.
//!
//! Section H measures the tunnel to each node with a TCP connect and a
//! WireGuard handshake age, and sends no request. The handshake age comes from
//! the file a separate privileged timer writes, so `--check` from cron judges
//! it only while that timer runs. A file older than
//! `tunnel_handshake_max_age_s` is itself a fault.
//!
//! Section I shows the state of the poll history (`history.rs`).
//!
//! Section A also shows each node's last log reopen, and the limit it is
//! judged against when `log_reopen_max_age_s` is set.
//!
//! Exit status is 1 when there are faults, so it works as a cron check.

use std::fmt::Write as _;

use crate::digest::{Digest, Level};
use crate::poll::NodeReading;

/// Render the check as text.
pub fn render(d: &Digest, readings: &[NodeReading]) -> String {
    let mut o = String::with_capacity(4096);
    let bar = "=".repeat(74);

    let _ = writeln!(o, "\n{bar}");
    let _ = writeln!(
        o,
        "m6 fleet health check   {}   {} node(s)",
        d.generated_at,
        d.nodes.len()
    );
    let _ = writeln!(o, "{bar}");

    // ── A. logging ───────────────────────────────────────────────────────────
    let _ = writeln!(o, "\nA. LOGGING");
    for r in readings {
        match (&r.traffic, &r.traffic_error) {
            (Some(t), _) => {
                let l = &t.logging;
                match crate::digest::quiet_log_text(l) {
                    Some(fault) => {
                        let _ = writeln!(o, "  {:<5} FAULT  {fault}", r.name);
                    }
                    None => {
                        let _ = writeln!(
                            o,
                            "  {:<5} ok     last event {}s ago, {} since start",
                            r.name,
                            l.seconds_since_last.unwrap_or(0),
                            l.events_total
                        );
                    }
                }
            }
            (None, Some(e)) => {
                let _ = writeln!(o, "  {:<5} unknown  {}", r.name, e);
            }
            (None, None) => {
                let _ = writeln!(
                    o,
                    "  {:<5} unknown  {}",
                    r.name,
                    r.unreachable
                        .as_deref()
                        .unwrap_or("no traffic summary and no reason given")
                );
            }
        }
    }

    match d.thresholds.log_reopen_age_s {
        Some(max) => {
            let _ = writeln!(
                o,
                "  log file reopen (rotation renames the file and signals): limit {max}s"
            );
        }
        None => {
            let _ = writeln!(
                o,
                "  log file reopen (rotation renames the file and signals): \
                 no limit configured, shown and not judged"
            );
        }
    }
    for n in &d.nodes {
        let l = &n.log_reopen;
        let uptime = n
            .uptime_s
            .map_or_else(|| "-".to_string(), |u| u.to_string());
        match l.state {
            "ok" => {
                let _ = write!(
                    o,
                    "  {:<5} reopened {}s ago, {} since start",
                    n.name,
                    l.age_s.unwrap_or(0),
                    l.reopens.unwrap_or(0)
                );
                if l.failures.unwrap_or(0) > 0 {
                    let _ = write!(o, ", {} failed", l.failures.unwrap_or(0));
                }
                let _ = writeln!(o);
            }
            "never" => {
                let _ = writeln!(
                    o,
                    "  {:<5} never reopened since start (up {uptime}s)",
                    n.name
                );
            }
            "no_log_file" => {
                let _ = writeln!(o, "  {:<5} writes no log file", n.name);
            }
            _ => {
                let _ = writeln!(
                    o,
                    "  {:<5} unknown  {}",
                    n.name,
                    l.detail.as_deref().unwrap_or("")
                );
            }
        }
    }

    // ── B. performance ───────────────────────────────────────────────────────
    let _ = writeln!(
        o,
        "\nB. PERFORMANCE  (observed; this tool generates no traffic)"
    );
    let _ = writeln!(
        o,
        "  {:<5} {:>9} {:>8} {:>10} {:>10} {:>8} {:>8}",
        "node", "requests", "hit rate", "hit p50", "hit p99", "samples", "errors"
    );
    for n in &d.nodes {
        let _ = writeln!(
            o,
            "  {:<5} {:>9} {:>8} {:>10} {:>10} {:>8} {:>8}",
            n.name,
            n.requests_total
                .map(|v| v.to_string())
                .unwrap_or_else(|| "-".into()),
            n.hit_rate
                .map(|v| format!("{v:.4}"))
                .unwrap_or_else(|| "-".into()),
            n.hit_p50_ns
                .map(|v| format!("{v}ns"))
                .unwrap_or_else(|| "-".into()),
            n.hit_p99_ns
                .map(|v| format!("{v}ns"))
                .unwrap_or_else(|| "-".into()),
            // The count goes beside the percentiles, always. A p50 over 46 hits and
            // one over 1200 are different claims and the number alone cannot be
            // compared to anything without it -- which is why the owner's standing
            // order says "ALWAYS with the sample count beside them", and why this
            // report was sending the reader to /perf on each node to get it.
            n.hit_samples
                .map(|v| v.to_string())
                .unwrap_or_else(|| "-".into()),
            n.backend_errors
                .map(|v| v.to_string())
                .unwrap_or_else(|| "-".into()),
        );
    }
    // ── The channel split ────────────────────────────────────────────────────
    //
    // A node's aggregate hit rate mixes channels and two of them are SUPPOSED to
    // look bad, so the aggregate alone invites a wrong conclusion. This was the last
    // thing a health check had to leave the monitor for: the data was already in the
    // same /perf response and simply was not rendered, so every run ended with an
    // ssh to each production node for figures already in hand.
    //
    //   http/2/internal    the backbone. A cache node forwards to origin only when
    //                      it has already missed, so this is majority-miss by
    //                      construction, and a HIGH rate here would be the problem.
    //   http/1.1/external  the scanner population, asking for paths that do not
    //                      exist. A few percent is normal.
    //   http/2/external    the browsers, and the only channel that answers whether
    //                      the cache is working for visitors.
    let any_channels = readings.iter().any(|r| {
        r.perf
            .as_ref()
            .is_some_and(|p| !p.metrics.channels.is_empty())
    });
    if any_channels {
        let _ = writeln!(o, "\n  per channel (the aggregate above mixes these)");
        for r in readings {
            let Some(p) = &r.perf else { continue };
            for c in &p.metrics.channels {
                let total = c.hits + c.misses;
                if total == 0 {
                    continue;
                }
                // The visitor channel is marked because it is what a reader should
                // look at first, and the two that look bad by design are the ones
                // most often mistaken for a fault.
                let note = match c.channel.as_str() {
                    "http/2/external" => "  <- visitors",
                    "http/2/internal" => "  (backbone: majority-miss by design)",
                    "http/1.1/external" => "  (scanners)",
                    _ => "",
                };
                let _ = writeln!(
                    o,
                    "  {:<5} {:<20} hits {:>6}  misses {:>6}  rate {:.4}  n {:>5}{}",
                    r.name,
                    c.channel,
                    c.hits,
                    c.misses,
                    c.hits as f64 / total as f64,
                    c.hit_samples,
                    note
                );
            }
        }
    }

    let _ = writeln!(
        o,
        "\n  rtt is from this host, connect and TLS included, and is NOT the\n  \
         node's own latency. Do not compare it with an on-box loopback figure."
    );
    for n in &d.nodes {
        let _ = writeln!(
            o,
            "  {:<5} rtt {}",
            n.name,
            n.rtt_ms
                .map(|v| format!("{v:.1}ms"))
                .unwrap_or_else(|| "-".into())
        );
    }

    // ── C. host ──────────────────────────────────────────────────────────────
    let _ = writeln!(o, "\nC. HOST");
    for n in &d.nodes {
        let _ = writeln!(
            o,
            "  {:<5} load {:<10} mem {:<6} disk {:<6} temp {:<6} svc up {}",
            n.name,
            match (n.load_one, n.cpus) {
                (Some(l), Some(c)) => format!("{l:.2}/{c}cpu"),
                _ => "-".to_string(),
            },
            n.memory_used
                .map(|v| format!("{:.0}%", v * 100.0))
                .unwrap_or_else(|| "-".into()),
            n.disk_used
                .map(|v| format!("{:.0}%", v * 100.0))
                .unwrap_or_else(|| "-".into()),
            n.thermal_max_c
                .map(|c| format!("{c:.0}C"))
                .unwrap_or_else(|| "-".into()),
            n.uptime_s.map(fmt_dur).unwrap_or_else(|| "-".into()),
        );
        // What each node runs, on the line with load and disk rather than in a
        // section of its own: the question "is this node the one that is behind"
        // is asked at the same moment as "is this node the busy one".
        //
        // Name, version AND build hash, because the version alone cannot tell
        // two builds of one tag apart and reading three identical version
        // strings is exactly what "the fleet agrees" looked like on 2026-09-20
        // while it did not. Twelve characters of the hash: enough to compare by
        // eye and the same prefix the estate file and the handover quote.
        //
        // The certificate is on this line too, for the same reason: "how long
        // has this node got" is asked beside "what is it running", and a
        // certificate reported anywhere an operator does not already look is
        // reported nowhere. From the certificate m6-http loaded, so it is what
        // the node is serving rather than what is on its disk.
        let _ = writeln!(
            o,
            "        {} {} build {} cert {}",
            n.binary.as_deref().unwrap_or("m6"),
            n.version
                .as_deref()
                .unwrap_or("unknown (node too old to report it)"),
            n.hash
                .as_deref()
                .map(|h| &h[..h.len().min(12)])
                .unwrap_or("unknown"),
            n.cert_expires_in_seconds
                .map(fmt_cert_expiry)
                .unwrap_or_else(|| "-".into()),
        );
    }

    // ── D. security ──────────────────────────────────────────────────────────
    let _ = writeln!(o, "\nD. SECURITY AND TRAFFIC");
    for r in readings {
        let Some(t) = &r.traffic else { continue };
        let _ = writeln!(
            o,
            "  {:<5} {} requests in {}m, status {:?}",
            r.name, t.total_requests, t.window_minutes, t.status
        );
        for h in t.heavy_hitters.iter().take(3) {
            let _ = writeln!(
                o,
                "        top: {} x{} {} ({:.0}% refused, {} UA)",
                h.ip,
                h.requests,
                h.top_path,
                h.error_ratio * 100.0,
                h.distinct_user_agents
            );
        }
        for c in &t.notable {
            let _ = writeln!(
                o,
                "  {:<5} NOTABLE {} x{} {}..{}",
                r.name,
                c.ip,
                c.requests,
                c.first_seen.get(11..19).unwrap_or(""),
                c.last_seen.get(11..19).unwrap_or("")
            );
            if c.rotating_user_agents {
                let _ = writeln!(
                    o,
                    "        {} distinct user agents (rotating)",
                    c.distinct_user_agents
                );
            }
            // ── The agents it presented, which the count alone could not say
            //
            // A line reading `1 UA` left a reader knowing how many claims a
            // source made and not one of them. m6 #219.
            //
            // The count goes beside the sample whenever the sample is partial,
            // because four agents out of four is the set and four out of 526 is
            // a glimpse of a rotator, and the two mean opposite things about
            // what the lines below are evidence of.
            if !c.user_agents.is_empty() {
                // Bounded here as well as at the producer, for the reason
                // every other list in this section is: the lines come from a
                // node's JSON over the network, and a digest a reader scrolls
                // is not the place to find out a payload was longer than
                // expected. The header counts what is printed, so the number
                // and the lines under it cannot disagree.
                let shown = c
                    .user_agents
                    .len()
                    .min(m6_core::telemetry::USER_AGENTS_KEPT);
                if shown < c.distinct_user_agents {
                    let _ = writeln!(
                        o,
                        "        agents ({} of {}):",
                        shown, c.distinct_user_agents
                    );
                } else {
                    let _ = writeln!(o, "        agents:");
                }
                for ua in c.user_agents.iter().take(shown) {
                    // An empty agent is a client that sent no User-Agent
                    // header at all, which is itself worth seeing: a browser
                    // always sends one.
                    if ua.is_empty() {
                        let _ = writeln!(o, "          <none sent>");
                    } else {
                        let _ = writeln!(o, "          {ua}");
                    }
                }
            }
            for p in c.probe_paths.iter().take(8) {
                let _ = writeln!(o, "        probe {p}");
            }
            for p in c.injection_paths.iter().take(4) {
                let _ = writeln!(o, "        INJECTION {p}");
            }
        }
        if !t.probe_noise.is_empty() {
            let _ = writeln!(
                o,
                "  {:<5} single refused probes (noise, not escalated): {}",
                r.name,
                t.probe_noise.join(", ")
            );
        }

        // The firewall, which this report has been receiving and discarding.
        //
        // `/traffic` has carried `firewall: Option<FirewallState>` all along and
        // nothing rendered it. That was reasonable until 2026-09-15: the collector
        // that writes /var/lib/m6/firewall.json was written, tested and deployed
        // nowhere, so every node returned `firewall: null` and there was nothing
        // to print. It is installed now.
        //
        // Blocks that have dropped NOTHING are not printed individually. A block
        // at zero has done its job -- whoever it was aimed at stopped coming -- and
        // thirty-eight of those every three hours would bury the ones that matter.
        // The count is still reported, because "38 blocks, none being hit" and
        // "no firewall data at all" are very different states and must not look
        // the same.
        match &t.firewall {
            Some(fw) => {
                let hit: Vec<_> = fw.active().collect();
                let _ = writeln!(
                    o,
                    "  {:<5} firewall: {} block(s) of {} rules, {} still being hit",
                    r.name,
                    fw.blocks.len(),
                    fw.total_rules,
                    hit.len()
                );
                for b in hit.iter().take(5) {
                    let _ = writeln!(
                        o,
                        "        hit {} {} packets {} bytes",
                        b.address, b.packets, b.bytes
                    );
                }
            }
            // Said out loud rather than omitted. A node with no collector looks
            // exactly like a node with no blocks if the line is simply absent,
            // and the first is a gap in the reporting while the second is a fact
            // about the node.
            None => {
                let _ = writeln!(
                    o,
                    "  {:<5} firewall: no data (collector not installed on this node)",
                    r.name
                );
            }
        }
    }

    // ── E. crawlers ──────────────────────────────────────────────────────────
    // ── F. cache headers, as the deployment declared them ────────────────────
    //
    // The failure these catch is a response that is SERVED CORRECTLY and cached
    // wrongly: a page that should revalidate pinned for a day, or an asset that
    // is not immutable so a content change never reaches anyone. Both are
    // invisible to a status-code check, which is why this section exists.
    //
    // What to expect is declared by the DEPLOYMENT in the fleet config, not here.
    // A cache policy is a property of a site; m6 is a generic web system and has
    // no business knowing that /capabilities wants max-age=60.
    let mut header_faults: Vec<String> = Vec::new();
    let any_declared = readings.iter().any(|r| !r.header_checks.is_empty());
    let _ = writeln!(o, "\nF. CACHE HEADERS");
    if !any_declared {
        // Said out loud. "No checks configured" and "all checks passed" must not
        // look the same, and an empty section reads as the second.
        let _ = writeln!(
            o,
            "  none declared. Add [[monitor.header_check]] entries to the fleet config."
        );
    } else {
        for r in readings {
            for hc in &r.header_checks {
                if hc.passed() {
                    let _ = writeln!(o, "  {:<5} ok     {} {}", r.name, hc.path, hc.header);
                } else if let Some(e) = &hc.error {
                    let _ = writeln!(
                        o,
                        "  {:<5} FAULT  {} could not be fetched: {}",
                        r.name, hc.path, e
                    );
                    header_faults
                        .push(format!("{}: {} could not be fetched: {e}", r.name, hc.path));
                } else {
                    // Absent and wrong are different failures and read
                    // differently: a missing header is usually a route that lost
                    // its policy, a wrong one a policy that changed.
                    let got = hc
                        .actual
                        .clone()
                        .unwrap_or_else(|| "<header absent>".to_string());
                    let code = hc
                        .status
                        .map(|c| format!(" (HTTP {c})"))
                        .unwrap_or_default();
                    let _ = writeln!(
                        o,
                        "  {:<5} FAULT  {} {} is {:?}, expected {:?}{}",
                        r.name, hc.path, hc.header, got, hc.expected, code
                    );
                    header_faults.push(format!(
                        "{}: {} {} is {:?}, expected {:?}",
                        r.name, hc.path, hc.header, got, hc.expected
                    ));
                }
            }
        }
    }

    let _ = writeln!(o, "\nE. CRAWLERS  (reported every run, even a quiet one)");
    let mut any = false;
    for r in readings {
        let Some(t) = &r.traffic else { continue };
        if t.forged_bot_requests > 0 {
            let _ = writeln!(
                o,
                "  {:<5} {} bot-shaped requests were FORGED by {} and are excluded",
                r.name,
                t.forged_bot_requests,
                t.forgers.join(", ")
            );
        }
        if t.crawlers.is_empty() {
            let _ = writeln!(
                o,
                "  {:<5} no genuine crawler traffic in the window",
                r.name
            );
            continue;
        }
        any = true;
        for c in &t.crawlers {
            let more = if c.client_ips.len() > 3 {
                format!(" (+{} more)", c.client_ips.len() - 3)
            } else {
                String::new()
            };
            let _ = writeln!(
                o,
                "  {:<5} {:>4}  {}{}",
                r.name,
                c.requests,
                c.client_ips
                    .iter()
                    .take(3)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", "),
                more
            );
            let _ = writeln!(o, "        UA: {}", c.user_agent);
            let _ = writeln!(o, "        paths: {}", c.paths.join(", "));
        }
    }
    if !any {
        let _ = writeln!(o, "  (none seen on any node)");
    }

    // ── G. connection setup, per channel and per resumption state ────────────
    //
    // Never summed on either axis, and both axes matter:
    //
    //   ACROSS PROTOCOLS  h1 and h2 are the rustls handshake, which EXCLUDES the
    //                     TCP round trip that finished before rustls saw the
    //                     socket. h3 is the QUIC handshake, which INCLUDES its
    //                     equivalent, because QUIC folds transport and crypto
    //                     together. They are not the same span.
    //   ACROSS RESUMPTION A resumed handshake skips the certificate and the
    //                     signature. Blending it with a full one gives a figure
    //                     that moves when the returning-visitor mix moves while
    //                     neither cost has changed.
    //
    // This replaces the loopback curl the ssh health check ran on each node, and
    // is better than it: these are real client handshakes, not a synthetic one
    // against localhost.
    let any_hs = readings.iter().any(|r| {
        r.perf
            .as_ref()
            .map(|p| {
                p.metrics
                    .channels
                    .iter()
                    .any(|c| c.handshake_full.total > 0 || c.handshake_resumed.total > 0)
            })
            .unwrap_or(false)
    });
    let _ = writeln!(o, "\nG. CONNECTION SETUP");
    if !any_hs {
        let _ = writeln!(
            o,
            "  no handshakes recorded. A node running m6-http 1.0.0 does not report them."
        );
    } else {
        for r in readings {
            let Some(p) = &r.perf else { continue };
            for c in &p.metrics.channels {
                let full = &c.handshake_full;
                let res = &c.handshake_resumed;
                if full.total == 0 && res.total == 0 {
                    continue;
                }
                // The resumption rate, from the two counts. Printed because it is
                // the thing that would have silently moved a blended median, and
                // because a low rate on the browser channel is itself a finding:
                // it means returning visitors are paying full handshakes.
                let tot = full.total + res.total;
                let _ = writeln!(
                    o,
                    "  {:<5} {:<20} {} handshakes, {:.0}% resumed",
                    r.name,
                    c.channel,
                    tot,
                    100.0 * res.total as f64 / tot as f64
                );
                for (label, h) in [("full", full), ("resumed", res)] {
                    if h.total == 0 {
                        // Said plainly rather than printed as zeroes. "p50 0.00ms"
                        // on a kind that never happened reads as an impossibly
                        // fast server rather than as an absence of data.
                        let _ = writeln!(o, "        {label:<8} none");
                        continue;
                    }
                    // Two figures with different spans on one line, each labelled:
                    // percentiles over the recent window the reservoir holds, then
                    // the lifetime record the window discards. p99 over the last
                    // 1024 connections says nothing about the worst of the 40,000
                    // before them; max does.
                    let _ = writeln!(
                        o,
                        "        {label:<8} last {:>5}  p50 {:>7.2}ms  p99 {:>7.2}ms   \
                         all {:>7}  mean {:>7.2}ms  min {:>7.2}ms  max {:>7.2}ms",
                        h.samples,
                        h.p50_ns as f64 / 1e6,
                        h.p99_ns as f64 / 1e6,
                        h.total,
                        h.mean_ns as f64 / 1e6,
                        h.min_ns as f64 / 1e6,
                        h.max_ns as f64 / 1e6
                    );
                }
            }
        }
        let _ = writeln!(
            o,
            "  h1/h2 exclude the TCP round trip; h3 includes its equivalent. Do not compare them."
        );
    }

    // ── H. tunnels ───────────────────────────────────────────────────────────
    let _ = writeln!(
        o,
        "\nH. TUNNELS  (TCP connect from this host to each tunnel address; no request is sent)"
    );
    if d.nodes.iter().all(|n| n.tunnel.is_none()) {
        let _ = writeln!(
            o,
            "  none configured. Add tunnel = {{ address = \"ip:port\", public_key = \"...\" }} to a [[monitor.nodes]] entry."
        );
    } else {
        if let Some(age) = d.handshake_file_age_s {
            let _ = writeln!(o, "  handshake file written {age}s ago");
        }
        for n in &d.nodes {
            match &n.tunnel {
                None => {
                    let _ = writeln!(o, "  {:<5} no tunnel configured", n.name);
                }
                Some(t) => {
                    let _ = writeln!(
                        o,
                        "  {:<5} {}  {}  handshake {}",
                        n.name,
                        t.address,
                        t.rtt_text(),
                        t.handshake_text()
                    );
                }
            }
        }
        // The kernel's own count of what the tunnel interface refused to carry,
        // from each node's /perf, when the node reported it.
        for (n, r) in d.nodes.iter().zip(readings) {
            let Some(p) = &r.perf else { continue };
            for dev in p.host.net.iter().filter(|dev| dev.name.starts_with("wg")) {
                let _ = writeln!(
                    o,
                    "  {:<5} {} drops rx {} tx {} (errors rx {} tx {})",
                    n.name, dev.name, dev.rx_drop, dev.tx_drop, dev.rx_errs, dev.tx_errs
                );
            }
        }
    }
    let _ = writeln!(
        o,
        "  The rtt is the tunnel path from this host, unlike section B's HTTPS rtt.\n  \
         It is not the node's own latency."
    );

    // ── I. history ───────────────────────────────────────────────────────────
    let _ = writeln!(o, "\nI. HISTORY  (one row per node per poll, in SQLite)");
    match &d.history {
        None => {
            let _ = writeln!(o, "  not kept by --check; the running service keeps it");
        }
        Some(h) if h.state == "off" => {
            let _ = writeln!(o, "  off. Set [monitor] history_file to keep one.");
        }
        Some(h) => {
            let mb = |b: Option<u64>| {
                b.map_or_else(
                    || "-".to_string(),
                    |b| format!("{:.1}", b as f64 / 1_048_576.0),
                )
            };
            let dash = |v: &Option<String>| v.clone().unwrap_or_else(|| "-".to_string());
            let _ = writeln!(o, "  file {}", h.file.as_deref().unwrap_or("-"));
            let _ = writeln!(
                o,
                "  {} MB used of the {} MB cap (file {} MB); oldest {}, newest {}",
                mb(h.used_bytes),
                mb(h.max_bytes),
                mb(h.file_bytes),
                dash(&h.oldest_at),
                dash(&h.newest_at)
            );
            let _ = writeln!(
                o,
                "  {} polls and {} crawler windows written, last {}s ago; {} pruned at the cap; \
                 {} dropped; {} failed",
                h.writes,
                h.crawl_windows,
                h.last_write_age_s
                    .map_or_else(|| "-".to_string(), |a| a.to_string()),
                h.pruned,
                h.dropped,
                h.failures
            );
            if let Some(e) = &h.last_error {
                let _ = writeln!(o, "  last error: {e}");
            }
        }
    }

    // ── verdict ──────────────────────────────────────────────────────────────
    let _ = writeln!(o, "\n{bar}");
    let faults: Vec<_> = d
        .findings
        .iter()
        .filter(|f| f.level == Level::Fault)
        .collect();
    let warns: Vec<_> = d
        .findings
        .iter()
        .filter(|f| f.level == Level::Warn)
        .collect();
    if !faults.is_empty() {
        let _ = writeln!(o, "FAULTS ({})", faults.len());
        for f in &faults {
            let _ = writeln!(o, "  - {}: {}", f.node, f.text);
        }
    }
    if !warns.is_empty() {
        let _ = writeln!(o, "WARNINGS ({})", warns.len());
        for f in &warns {
            let _ = writeln!(o, "  - {}: {}", f.node, f.text);
        }
    }
    // Header failures are found HERE rather than in the digest, because the digest
    // is built from /health and /perf and knows nothing about them. They still
    // have to reach the verdict: a node serving a page with the wrong cache policy
    // is not "all clear", and printing that it is would be the report lying.
    if !header_faults.is_empty() {
        let _ = writeln!(o, "CACHE HEADER FAULTS ({})", header_faults.len());
        for f in &header_faults {
            let _ = writeln!(o, "  - {f}");
        }
    }
    if faults.is_empty() && warns.is_empty() && header_faults.is_empty() {
        let _ = writeln!(o, "ALL CLEAR on all {} nodes.", d.nodes.len());
    }
    let _ = writeln!(o, "{bar}");
    o
}

fn fmt_dur(secs: u64) -> String {
    let d = secs / 86_400;
    let h = (secs % 86_400) / 3_600;
    if d > 0 {
        format!("{d}d{h}h")
    } else {
        format!("{h}h")
    }
}

/// Seconds left on a certificate, as days.
///
/// Days because that is the unit renewal is discussed in, and a bare `12d` next
/// to a build hash is read correctly without a legend. Separate from `fmt_dur`
/// because this number goes negative and that one cannot: an expired
/// certificate reads `EXPIRED 3d ago`, which is a different sentence from a
/// small positive number and has to look like one on a line being skimmed.
fn fmt_cert_expiry(secs: i64) -> String {
    if secs < 0 {
        format!("EXPIRED {}d ago", -secs / 86_400)
    } else {
        format!("{}d", secs / 86_400)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use m6_core::monitoring::{LoggingHealth, TrafficReport};

    fn traffic(logging: LoggingHealth) -> TrafficReport {
        TrafficReport {
            node: "origin".into(),
            window_minutes: 60,
            total_requests: 0,
            status: Default::default(),
            notable: vec![],
            heavy_hitters: vec![],
            crawlers: vec![],
            forged_bot_requests: 0,
            forgers: vec![],
            probe_noise: vec![],
            logging,
            firewall: None,
        }
    }

    /// A notable client that presented `agents`, out of `distinct` in total.
    fn probing_client(
        ip: &str,
        agents: &[&str],
        distinct: usize,
    ) -> m6_core::monitoring::NotableClient {
        m6_core::monitoring::NotableClient {
            ip: ip.into(),
            requests: 7,
            distinct_user_agents: distinct,
            user_agents: agents.iter().map(|a| a.to_string()).collect(),
            status: Default::default(),
            probe_paths: vec!["/.env".into(), "/.git/config".into()],
            injection_paths: vec![],
            rotating_user_agents: distinct >= 10,
            first_seen: "2026-10-04T01:20:54Z".into(),
            last_seen: "2026-10-04T01:20:58Z".into(),
        }
    }

    /// The report names what a prober claimed to be.
    ///
    /// Before m6 #219 this section printed the probe paths and a count of
    /// agents, so a reader learned how many claims the source made and not one
    /// of them. The agent is a claim rather than a fact, and the claim is
    /// evidence about intent even when it is false.
    #[test]
    fn a_notable_clients_user_agents_are_printed() {
        let mut t = traffic(LoggingHealth {
            events_total: 100,
            seconds_since_last: Some(1),
            reopen: None,
        });
        t.notable = vec![probing_client("203.0.113.9", &["curl/8.4.0"], 1)];
        let out = render_for_test(t);

        assert!(
            out.contains("curl/8.4.0"),
            "the agent itself must appear, not just a count:\n{out}"
        );
        assert!(
            !out.contains("agents (1 of 1)"),
            "a complete set needs no 'x of y', which only means something when \
             the sample is partial:\n{out}"
        );
    }

    /// A sample says it is a sample.
    ///
    /// Four agents out of four is the set; four out of 526 is a glimpse of a
    /// rotator. They mean opposite things about what the listed agents are
    /// evidence of, so the report cannot print them identically.
    #[test]
    fn a_partial_sample_of_agents_says_how_many_there_were() {
        let mut t = traffic(LoggingHealth {
            events_total: 100,
            seconds_since_last: Some(1),
            reopen: None,
        });
        t.notable = vec![probing_client(
            "203.0.113.41",
            &[
                "Googlebot/2.1",
                "GPTBot/1.0",
                "curl/8.4.0",
                "python-requests/2",
            ],
            526,
        )];
        let out = render_for_test(t);

        assert!(
            out.contains("agents (4 of 526)"),
            "a partial sample has to say so:\n{out}"
        );
        assert!(
            out.contains("526 distinct user agents (rotating)"),
            "and the rotation finding stays, because it is a different claim \
             from the sample:\n{out}"
        );
    }

    /// A client that sent no `User-Agent` header at all.
    ///
    /// Worth seeing rather than rendering as a blank line: every browser sends
    /// one, so its absence is itself a signal about the client.
    #[test]
    fn a_client_that_sent_no_user_agent_is_said_to_have_sent_none() {
        let mut t = traffic(LoggingHealth {
            events_total: 100,
            seconds_since_last: Some(1),
            reopen: None,
        });
        t.notable = vec![probing_client("203.0.113.50", &[""], 1)];
        let out = render_for_test(t);
        assert!(
            out.contains("<none sent>"),
            "an empty agent must be named, not printed as whitespace:\n{out}"
        );
    }

    fn render_for_test(t: TrafficReport) -> String {
        let r = reading("origin", Some(t));
        let d = crate::digest::build(
            std::slice::from_ref(&r),
            &crate::digest::Thresholds::default(),
            "2026-10-04T02:16:31Z".into(),
        );
        render(&d, std::slice::from_ref(&r))
    }

    pub(super) fn reading(name: &str, t: Option<TrafficReport>) -> NodeReading {
        NodeReading {
            name: name.into(),
            role: "origin".into(),
            url: "https://origin.example.com".into(),
            health: None,
            health_status: None,
            perf: None,
            perf_error: None,
            traffic: t,
            traffic_error: None,
            header_checks: Vec::new(),
            rtt: None,
            unreachable: None,
            tunnel: None,
        }
    }

    pub(super) fn hc(
        path: &str,
        expected: &str,
        actual: Option<&str>,
    ) -> crate::poll::HeaderCheckResult {
        crate::poll::HeaderCheckResult {
            path: path.into(),
            header: "cache-control".into(),
            expected: expected.into(),
            actual: actual.map(|a| a.into()),
            status: Some(200),
            error: None,
        }
    }

    pub(super) fn digest() -> Digest {
        crate::digest::build(&[], &crate::digest::Thresholds::default(), "now".into())
    }

    /// Part A, without journalctl. A node whose main log has gone quiet is a
    /// FAULT even though it is answering every request normally, which is the
    /// entire shape of the 2026-09-06 defect.
    #[test]
    fn a_silent_log_is_a_fault_even_on_a_responsive_node() {
        let quiet = traffic(LoggingHealth {
            events_total: 5000,
            seconds_since_last: Some(600),
            reopen: None,
        });
        let out = render(&digest(), &[reading("origin", Some(quiet))]);
        assert!(out.contains("FAULT  main log silent for 600s"), "{out}");
    }

    #[test]
    fn a_live_log_is_ok() {
        let alive = traffic(LoggingHealth {
            events_total: 5000,
            seconds_since_last: Some(4),
            reopen: None,
        });
        let out = render(&digest(), &[reading("origin", Some(alive))]);
        assert!(out.contains("ok     last event 4s ago"), "{out}");
    }

    /// Never having logged must not read as healthy.
    #[test]
    fn never_logged_is_a_fault_not_a_zero() {
        let never = traffic(LoggingHealth {
            events_total: 0,
            seconds_since_last: None,
            reopen: None,
        });
        let out = render(&digest(), &[reading("origin", Some(never))]);
        assert!(out.contains("nothing has ever been logged"), "{out}");
    }

    /// A node we could not get traffic from is "unknown", never "ok". The
    /// standing order is explicit: do not report all clear on a blind node.
    #[test]
    fn no_traffic_data_is_unknown_not_ok() {
        let mut r = reading("edge-b", None);
        r.traffic_error = Some("404: /traffic not available on this node".into());
        let out = render(&digest(), &[r]);
        assert!(out.contains("unknown"), "{out}");
        assert!(!out.contains("edge-b   ok"), "{out}");
    }

    /// Crawlers are reported every run, including when there are none.
    #[test]
    fn crawlers_are_always_a_section() {
        let out = render(
            &digest(),
            &[reading(
                "origin",
                Some(traffic(LoggingHealth {
                    events_total: 1,
                    seconds_since_last: Some(1),
                    reopen: None,
                })),
            )],
        );
        assert!(out.contains("E. CRAWLERS"));
        assert!(out.contains("no genuine crawler traffic in the window"));
    }
}

/// The cache-header section, and the verdict it must be able to change.
///
/// These assert the part that is easy to get wrong: a check that reports a
/// failure in its own section and then prints ALL CLEAR at the bottom is worse
/// than no check, because the summary is what gets read.
#[cfg(test)]
mod header_check_tests {
    use super::tests::{digest, hc, reading};
    use super::*;

    #[test]
    fn a_passing_check_is_reported_and_stays_all_clear() {
        let mut r = reading("origin", None);
        r.header_checks.push(hc(
            "/capabilities",
            "public, max-age=60",
            Some("public, max-age=60"),
        ));
        let out = render(&digest(), &[r]);
        assert!(out.contains("F. CACHE HEADERS"), "{out}");
        assert!(out.contains("/capabilities"), "{out}");
        assert!(
            out.contains("ALL CLEAR"),
            "a passing check must not raise a fault:\n{out}"
        );
    }

    /// The one that matters. A wrong cache policy must reach the verdict.
    #[test]
    fn a_wrong_header_is_a_fault_and_removes_all_clear() {
        let mut r = reading("origin", None);
        r.header_checks
            .push(hc("/capabilities", "public, max-age=60", Some("no-store")));
        let out = render(&digest(), &[r]);
        assert!(out.contains("CACHE HEADER FAULTS"), "{out}");
        assert!(
            !out.contains("ALL CLEAR"),
            "a node serving the wrong cache policy is not all clear:\n{out}"
        );
        // Both values, so the reader does not have to go and look them up.
        assert!(out.contains("no-store"), "{out}");
        assert!(out.contains("max-age=60"), "{out}");
    }

    /// An absent header and a wrong one are different problems.
    #[test]
    fn an_absent_header_says_so_rather_than_comparing_to_empty() {
        let mut r = reading("edge-a", None);
        r.header_checks
            .push(hc("/capabilities", "public, max-age=60", None));
        let out = render(&digest(), &[r]);
        assert!(out.contains("<header absent>"), "{out}");
        assert!(!out.contains("ALL CLEAR"), "{out}");
    }

    /// A fetch that never completed is not a wrong header.
    #[test]
    fn a_transport_failure_is_reported_as_one() {
        let mut r = reading("edge-b", None);
        let mut c = hc("/capabilities", "public, max-age=60", None);
        c.error = Some("connection refused".into());
        c.status = None;
        r.header_checks.push(c);
        let out = render(&digest(), &[r]);
        assert!(out.contains("could not be fetched"), "{out}");
        assert!(!out.contains("ALL CLEAR"), "{out}");
    }

    /// Nothing configured must not read as everything passing.
    #[test]
    fn no_checks_declared_says_so() {
        let out = render(&digest(), &[reading("origin", None)]);
        assert!(out.contains("F. CACHE HEADERS"), "{out}");
        assert!(out.contains("none declared"), "{out}");
        // And it is not a fault: declaring none is a choice, not a failure.
        assert!(out.contains("ALL CLEAR"), "{out}");
    }

    /// Certificate expiry on the per-node line, in days and with the sign
    /// handled. Integer division truncates toward zero, so an expired
    /// certificate must be negated before the division and not after: `-1 /
    /// 86400` is 0 either way, and `EXPIRED 0d ago` is the right answer for
    /// something that lapsed an hour ago.
    #[test]
    fn certificate_expiry_renders_in_days_with_the_sign_kept() {
        assert_eq!(fmt_cert_expiry(60 * 86_400), "60d");
        assert_eq!(fmt_cert_expiry(0), "0d");
        // Part of a day left still reads as a day count, rounded down, which is
        // the honest direction for a deadline.
        assert_eq!(fmt_cert_expiry(86_399), "0d");
        assert_eq!(fmt_cert_expiry(-3 * 86_400), "EXPIRED 3d ago");
        assert_eq!(fmt_cert_expiry(-1), "EXPIRED 0d ago");
    }
}

#[cfg(test)]
mod tunnel_tests {
    use super::tests::{digest, reading};
    use super::*;
    use crate::tunnel::{Handshake, Probe, ProbeResult, TunnelReading};
    use std::time::Duration;

    fn with_tunnel(name: &str, handshake: Handshake) -> NodeReading {
        let mut r = reading(name, None);
        r.tunnel = Some(TunnelReading {
            address: "192.0.2.2:443".into(),
            key_prefix: Some("AAAAAAAA".into()),
            probe: ProbeResult {
                samples: vec![Duration::from_micros(400); 3],
                outcome: Probe::Connected,
            },
            handshake,
            file_age_s: Some(7),
        });
        r
    }

    fn out(rs: &[NodeReading]) -> String {
        let d = crate::digest::build(rs, &crate::digest::Thresholds::default(), "now".into());
        render(&d, rs)
    }

    #[test]
    fn the_tunnel_section_is_always_printed() {
        let o = render(&digest(), &[reading("origin", None)]);
        assert!(o.contains("H. TUNNELS"), "{o}");
        assert!(o.contains("none configured"), "{o}");
        assert!(o.contains("ALL CLEAR"), "{o}");
    }

    #[test]
    fn a_tunnel_line_shows_rtt_with_its_sample_count_and_handshake_age() {
        let o = out(&[with_tunnel("edge-a", Handshake::At { age_s: 42 })]);
        assert!(o.contains("rtt 0.40ms (n 3, connected)"), "{o}");
        assert!(o.contains("handshake 42s ago"), "{o}");
        assert!(o.contains("handshake file written 7s ago"), "{o}");
    }

    #[test]
    fn a_peer_with_no_handshake_says_never_and_removes_all_clear() {
        let o = out(&[with_tunnel("edge-a", Handshake::Never)]);
        assert!(o.contains("handshake never"), "{o}");
        assert!(!o.contains("ALL CLEAR"), "{o}");
    }

    #[test]
    fn a_tunnel_with_no_handshake_source_removes_all_clear() {
        let o = out(&[with_tunnel("edge-a", Handshake::NoPublicKey)]);
        assert!(o.contains("public_key"), "{o}");
        assert!(!o.contains("ALL CLEAR"), "{o}");
        let o = out(&[with_tunnel("edge-a", Handshake::NoHandshakesFile)]);
        assert!(o.contains("wg_handshakes_file"), "{o}");
        assert!(!o.contains("ALL CLEAR"), "{o}");
    }

    #[test]
    fn a_node_without_a_tunnel_says_so_beside_one_that_has_one() {
        let o = out(&[
            with_tunnel("edge-a", Handshake::At { age_s: 1 }),
            reading("edge-b", None),
        ]);
        assert!(o.contains("192.0.2.2:443"), "{o}");
        assert!(o.contains("edge-b no tunnel configured"), "{o}");
    }

    #[test]
    fn wg_interface_drops_are_shown_when_the_node_reports_them() {
        let mut r = with_tunnel("edge-a", Handshake::At { age_s: 1 });
        let mut host = m6_core::host::HostSnapshot::default();
        host.net.push(m6_core::host::NetDevice {
            name: "wg0".into(),
            rx_drop: 3,
            tx_drop: 4,
            ..Default::default()
        });
        r.perf = Some(crate::digest::tests::perf(host, vec![]));
        let o = out(&[r]);
        assert!(o.contains("wg0 drops rx 3 tx 4"), "{o}");
    }

    // ── log reopen ───────────────────────────────────────────────────────────

    use m6_core::monitoring::{LogReopenHealth, LoggingHealth, PerfReport};

    /// A node whose `/perf` carries `reopen`, with the given uptime.
    fn reopen_reading(uptime_s: u64, reopen: Option<LogReopenHealth>) -> NodeReading {
        let mut r = reading("origin", None);
        r.health = Some(m6_core::monitoring::HealthReport {
            status: "ok".into(),
            node: "origin".into(),
        });
        r.health_status = Some(200);
        let mut p: PerfReport = crate::digest::tests::perf(
            m6_core::host::HostSnapshot {
                cpus: 1,
                ..Default::default()
            },
            vec![],
        );
        p.uptime_s = uptime_s;
        p.logging = Some(LoggingHealth {
            events_total: 10,
            seconds_since_last: Some(1),
            reopen,
        });
        r.perf = Some(p);
        r
    }

    fn render_reopen(r: NodeReading, max: Option<u64>) -> String {
        let d = crate::digest::build(
            std::slice::from_ref(&r),
            &crate::digest::Thresholds {
                log_reopen_age_s: max,
                ..Default::default()
            },
            "now".into(),
        );
        render(&d, std::slice::from_ref(&r))
    }

    fn health(age: Option<u64>, reopens: u64) -> LogReopenHealth {
        LogReopenHealth {
            reopens_total: reopens,
            failures_total: 0,
            seconds_since_last_reopen: age,
            last_reopen_unix: age.map(|a| 1_789_000_000 - a),
            seconds_since_last_failure: None,
        }
    }

    #[test]
    fn section_a_shows_each_nodes_last_log_reopen_age() {
        let o = render_reopen(reopen_reading(9000, Some(health(Some(60), 3))), Some(7200));
        assert!(o.contains("reopened 60s ago, 3 since start"), "{o}");
        assert!(o.contains("limit 7200s"), "{o}");
    }

    #[test]
    fn section_a_says_never_reopened_since_start_distinctly() {
        let o = render_reopen(reopen_reading(100, Some(health(None, 0))), Some(7200));
        assert!(o.contains("never reopened since start (up 100s)"), "{o}");
    }

    #[test]
    fn a_stale_log_reopen_removes_all_clear() {
        let o = render_reopen(
            reopen_reading(99_999, Some(health(Some(7300), 3))),
            Some(7200),
        );
        assert!(!o.contains("ALL CLEAR"), "{o}");
        assert!(o.contains("FAULTS (1)"), "{o}");
        assert!(
            o.contains("- origin: last reopened its log file 7300s ago"),
            "{o}"
        );
    }

    #[test]
    fn section_a_says_when_no_limit_is_configured() {
        let o = render_reopen(reopen_reading(9000, Some(health(Some(60), 3))), None);
        assert!(o.contains("no limit configured"), "{o}");
    }

    #[test]
    fn section_a_says_a_node_writes_no_log_file() {
        let o = render_reopen(reopen_reading(9000, None), None);
        assert!(o.contains("writes no log file"), "{o}");
    }
}

#[cfg(test)]
mod history_tests {
    use super::tests::{digest, reading};
    use super::*;
    use crate::history::HistoryDigest;

    fn history(state: &'static str) -> HistoryDigest {
        HistoryDigest {
            state,
            file: Some("/var/lib/m6-monitor/history.sqlite3".into()),
            max_bytes: Some(256 << 20),
            file_bytes: Some(3 << 20),
            used_bytes: Some(2 << 20),
            oldest_at: Some("2027-01-01T00:00:00.000Z".into()),
            newest_at: Some("2027-01-02T00:00:00.000Z".into()),
            last_write_age_s: Some(4),
            writes: 120,
            crawl_windows: 6,
            failures: 1,
            dropped: 2,
            pruned: 0,
            last_error: None,
        }
    }

    fn out(h: Option<HistoryDigest>) -> String {
        let mut d = digest();
        d.history = h;
        render(&d, &[reading("origin", None)])
    }

    #[test]
    fn section_i_says_history_is_off() {
        let o = out(Some(HistoryDigest {
            state: "off",
            file: None,
            max_bytes: None,
            file_bytes: None,
            used_bytes: None,
            oldest_at: None,
            newest_at: None,
            last_write_age_s: None,
            writes: 0,
            crawl_windows: 0,
            failures: 0,
            dropped: 0,
            pruned: 0,
            last_error: None,
        }));
        assert!(o.contains("I. HISTORY"), "{o}");
        assert!(
            o.contains("off. Set [monitor] history_file to keep one."),
            "{o}"
        );
    }

    #[test]
    fn section_i_shows_the_cap_and_the_last_write() {
        let o = out(Some(history("ok")));
        assert!(
            o.contains("file /var/lib/m6-monitor/history.sqlite3"),
            "{o}"
        );
        assert!(
            o.contains("2.0 MB used of the 256.0 MB cap (file 3.0 MB); oldest 2027-01-01T00:00:00.000Z, newest 2027-01-02T00:00:00.000Z"),
            "{o}"
        );
        assert!(
            o.contains("120 polls and 6 crawler windows written, last 4s ago; 0 pruned at the cap; 2 dropped; 1 failed"),
            "{o}"
        );
        assert!(!o.contains("last error"), "{o}");
    }

    #[test]
    fn section_i_uses_dashes_for_absent_figures_and_shows_the_last_error() {
        let mut h = history("failing");
        h.used_bytes = None;
        h.oldest_at = None;
        h.last_write_age_s = None;
        h.last_error = Some("disk full".into());
        let o = out(Some(h));
        assert!(o.contains("- MB used of the 256.0 MB cap"), "{o}");
        assert!(o.contains("oldest -, newest"), "{o}");
        assert!(o.contains("written, last -s ago"), "{o}");
        assert!(o.contains("last error: disk full"), "{o}");
    }

    #[test]
    fn a_failing_history_write_removes_all_clear() {
        let h = history("failing");
        let mut d = digest();
        d.history = Some(h.clone());
        d.add_finding(crate::history::finding(&h).expect("a finding"));
        let o = render(&d, &[reading("origin", None)]);
        assert!(!o.contains("ALL CLEAR"), "{o}");
        assert!(
            o.contains("- monitor: the last history write failed"),
            "{o}"
        );
    }

    #[test]
    fn check_one_shot_says_history_is_kept_by_the_service() {
        let o = out(None);
        assert!(
            o.contains("not kept by --check; the running service keeps it"),
            "{o}"
        );
    }
}
