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
                match l.seconds_since_last {
                    None => {
                        let _ = writeln!(o, "  {:<5} FAULT  nothing has ever been logged", r.name);
                    }
                    Some(s) if l.is_blind() => {
                        let _ = writeln!(
                            o,
                            "  {:<5} FAULT  main log silent for {}s ({} events since start)",
                            r.name, s, l.events_total
                        );
                    }
                    Some(s) => {
                        let _ = writeln!(
                            o,
                            "  {:<5} ok     last event {}s ago, {} since start",
                            r.name, s, l.events_total
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

    // ── B. performance ───────────────────────────────────────────────────────
    let _ = writeln!(
        o,
        "\nB. PERFORMANCE  (observed; this tool generates no traffic)"
    );
    let _ = writeln!(
        o,
        "  {:<5} {:>9} {:>8} {:>10} {:>10} {:>8}",
        "node", "requests", "hit rate", "hit p50", "hit p99", "errors"
    );
    for n in &d.nodes {
        let _ = writeln!(
            o,
            "  {:<5} {:>9} {:>8} {:>10} {:>10} {:>8}",
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
            n.backend_errors
                .map(|v| v.to_string())
                .unwrap_or_else(|| "-".into()),
        );
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
                h.user_agents
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

#[cfg(test)]
mod tests {
    use super::*;
    use m6_core::monitoring::{LoggingHealth, TrafficReport};

    fn traffic(logging: LoggingHealth) -> TrafficReport {
        TrafficReport {
            node: "sydney".into(),
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

    pub(super) fn reading(name: &str, t: Option<TrafficReport>) -> NodeReading {
        NodeReading {
            name: name.into(),
            role: "origin".into(),
            url: "https://x".into(),
            health: None,
            health_status: None,
            perf: None,
            perf_error: None,
            traffic: t,
            traffic_error: None,
            header_checks: Vec::new(),
            rtt: None,
            unreachable: None,
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
        });
        let out = render(&digest(), &[reading("syd", Some(quiet))]);
        assert!(out.contains("FAULT  main log silent for 600s"), "{out}");
    }

    #[test]
    fn a_live_log_is_ok() {
        let alive = traffic(LoggingHealth {
            events_total: 5000,
            seconds_since_last: Some(4),
        });
        let out = render(&digest(), &[reading("syd", Some(alive))]);
        assert!(out.contains("ok     last event 4s ago"), "{out}");
    }

    /// Never having logged must not read as healthy.
    #[test]
    fn never_logged_is_a_fault_not_a_zero() {
        let never = traffic(LoggingHealth {
            events_total: 0,
            seconds_since_last: None,
        });
        let out = render(&digest(), &[reading("syd", Some(never))]);
        assert!(out.contains("nothing has ever been logged"), "{out}");
    }

    /// A node we could not get traffic from is "unknown", never "ok". The
    /// standing order is explicit: do not report all clear on a blind node.
    #[test]
    fn no_traffic_data_is_unknown_not_ok() {
        let mut r = reading("chi", None);
        r.traffic_error = Some("404: /traffic not available on this node".into());
        let out = render(&digest(), &[r]);
        assert!(out.contains("unknown"), "{out}");
        assert!(!out.contains("chi   ok"), "{out}");
    }

    /// Crawlers are reported every run, including when there are none.
    #[test]
    fn crawlers_are_always_a_section() {
        let out = render(
            &digest(),
            &[reading(
                "syd",
                Some(traffic(LoggingHealth {
                    events_total: 1,
                    seconds_since_last: Some(1),
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
        let mut r = reading("syd", None);
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
        let mut r = reading("syd", None);
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
        let mut r = reading("lon", None);
        r.header_checks
            .push(hc("/capabilities", "public, max-age=60", None));
        let out = render(&digest(), &[r]);
        assert!(out.contains("<header absent>"), "{out}");
        assert!(!out.contains("ALL CLEAR"), "{out}");
    }

    /// A fetch that never completed is not a wrong header.
    #[test]
    fn a_transport_failure_is_reported_as_one() {
        let mut r = reading("chi", None);
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
        let out = render(&digest(), &[reading("syd", None)]);
        assert!(out.contains("F. CACHE HEADERS"), "{out}");
        assert!(out.contains("none declared"), "{out}");
        // And it is not a fault: declaring none is a choice, not a failure.
        assert!(out.contains("ALL CLEAR"), "{out}");
    }
}
