//! The hourly health check, printed.
//!
//! HEALTH comes first, because it is the question the check exists to answer:
//! this poll's `/health` for each node, and the age of the node's last 200 from
//! the history. A node that has not answered 200 for longer than
//! `health_last_ok_max_age_s`, which defaults to three poll intervals, is
//! flagged there and in the verdict.
//!
//! Apart from section H's tunnel check, this takes every figure from the
//! nodes' own HTTP endpoints. It does not log in
//! to any node or run `journalctl`, `systemctl`, `df` or `curl` there, and it
//! does not parse an analytics log over the wire. Each figure is a field on
//! `/perf` or `/traffic`, computed by the node that owns the data.
//!
//! A health check that measures its own effect on the nodes gives wrong
//! answers. Three ways it goes wrong: it flags its own load generator as a
//! security incident, it reads a latency regression off a single sample taken
//! during that load, and it times a fast loopback call straight after its own
//! journal scan. The first two cannot happen here, because this generates no
//! load and runs no scans. The third, that it measures from wherever it
//! happens to run, is unavoidable and is labelled instead.
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

use crate::digest::{fmt_uptime, header_check_text, Digest, Finding, Level};
use crate::poll::NodeReading;

/// Reverse-lookup results by address, read by [`render`].
pub type Verdicts = std::collections::HashMap<String, m6_core::resolve::Ptr>;

/// The most new addresses one poll starts a lookup for.
///
/// Each lookup carries the fleet's `timeout_ms` deadline, and the poll keeps up
/// to this many in flight at once, starting the next as each answer or
/// deadline frees a slot. A resolver that never answers therefore costs one
/// fleet timeout per batch, not one per address. An address past the cap reads
/// "not looked up" in this poll and is tried in the next.
pub const MAX_LOOKUPS_PER_POLL: usize = 8;

/// The lookups one `--check` run makes. A one-shot run has no later poll to
/// cover what it skips, and it is already waiting on a full fleet poll, so it
/// takes a wider cap than the daemon's poller.
pub const MAX_LOOKUPS_PER_CHECK: usize = 64;

/// How long a completed lookup is kept before the address is looked up again.
pub const VERDICT_TTL: std::time::Duration = std::time::Duration::from_secs(3600);

/// How long a failed lookup is kept. Short, because a failure says nothing
/// about the address, but long enough that a resolver that never answers is
/// not asked again at every poll.
pub const FAILED_TTL: std::time::Duration = std::time::Duration::from_secs(300);

/// The most addresses kept between polls.
pub const MAX_CACHED_VERDICTS: usize = 512;

/// Lookup results kept across polls, so a report only reads them.
///
/// The poll calls [`VerdictCache::due`] to learn which addresses need a lookup
/// and [`VerdictCache::record`] for each answer its loop receives. [`render`]
/// never resolves anything, so `/check` cannot be stalled by a resolver and
/// cannot hold up the poll.
pub struct VerdictCache {
    entries: std::collections::HashMap<String, (std::time::Instant, m6_core::resolve::Ptr)>,
    cap: usize,
}

#[cfg(test)]
impl Default for VerdictCache {
    fn default() -> Self {
        Self::with_cap(MAX_LOOKUPS_PER_POLL)
    }
}

impl VerdictCache {
    /// A cache for the poller: at most [`MAX_LOOKUPS_PER_POLL`] lookups a poll.
    #[cfg(test)]
    pub fn new() -> Self {
        Self::default()
    }

    /// A cache that makes at most `cap` lookups per refresh.
    pub fn with_cap(cap: usize) -> Self {
        Self {
            entries: std::collections::HashMap::new(),
            cap,
        }
    }

    /// The addresses in `readings` that need a lookup now: not cached, not
    /// expired, and at most the cache's cap, in the order the report shows
    /// them. `out` is reused every poll.
    pub fn due(
        &mut self,
        readings: &[NodeReading],
        now: std::time::Instant,
        out: &mut Vec<String>,
    ) {
        self.entries.retain(|_, (expires, _)| *expires > now);
        out.clear();
        for ip in readings
            .iter()
            .filter_map(|r| r.traffic.as_ref())
            .flat_map(|t| t.crawlers.iter())
            .flat_map(|c| c.client_ips.iter().take(CHECKED_ADDRESSES))
        {
            if out.contains(ip) || self.entries.contains_key(ip) {
                continue;
            }
            out.push(ip.clone());
            if out.len() == self.cap {
                break;
            }
        }
    }

    /// Store one lookup's answer, replacing an older one for the address.
    pub fn record(&mut self, addr: &str, ptr: m6_core::resolve::Ptr, now: std::time::Instant) {
        let ttl = if ptr == m6_core::resolve::Ptr::Failed {
            FAILED_TTL
        } else {
            VERDICT_TTL
        };
        if self.entries.len() >= MAX_CACHED_VERDICTS && !self.entries.contains_key(addr) {
            // Drop the entry closest to expiry.
            if let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, (e, _))| *e)
                .map(|(k, _)| k.clone())
            {
                self.entries.remove(&oldest);
            }
        }
        self.entries.insert(addr.to_string(), (now + ttl, ptr));
    }

    /// What the report reads: one answer for every address in `readings` that
    /// has one.
    pub fn verdicts(&self, readings: &[NodeReading]) -> Verdicts {
        let mut out = Verdicts::new();
        for ip in readings
            .iter()
            .filter_map(|r| r.traffic.as_ref())
            .flat_map(|t| t.crawlers.iter())
            .flat_map(|c| c.client_ips.iter().take(CHECKED_ADDRESSES))
        {
            if out.contains_key(ip) {
                continue;
            }
            if let Some((_, ptr)) = self.entries.get(ip) {
                out.insert(ip.clone(), ptr.clone());
            }
        }
        out
    }

    /// Look up every due address through `lookup`, then return the report's
    /// answers. The unit tests drive the cache through this; the service's
    /// loop and `--check` call [`due`](Self::due) and [`record`](Self::record)
    /// around their own non-blocking lookups.
    #[cfg(test)]
    pub fn refresh(
        &mut self,
        readings: &[NodeReading],
        now: std::time::Instant,
        lookup: &dyn Fn(&str) -> m6_core::resolve::Ptr,
    ) -> Verdicts {
        let mut due = Vec::new();
        self.due(readings, now, &mut due);
        for addr in &due {
            let ptr = lookup(addr);
            self.record(addr, ptr, now);
        }
        self.verdicts(readings)
    }
}

/// How many addresses of one sighting the report checks and shows.
const CHECKED_ADDRESSES: usize = 3;

/// Render the check as text, with the stored lookup results.
pub fn render(d: &Digest, readings: &[NodeReading], verdicts: &Verdicts) -> String {
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

    // ── HEALTH ───────────────────────────────────────────────────────────────
    //
    // The first section, because a reader of a fleet report asks whether the
    // nodes are serving before anything else. Both facts are already collected:
    // the poll asks every node for `/health` and the service keeps every poll
    // in its history, so this reads rather than adds a poll.
    let limit_s = d.thresholds.health_last_ok_age_s;
    let _ = writeln!(
        o,
        "\nHEALTH  (this poll's /health per node and the age of its last 200 from \
         history. Older than {limit_s}s is flagged)"
    );
    for r in readings {
        let l = health_line(r, d.history.as_ref(), limit_s);
        let label = if r.status() == "ok" && !l.stale {
            "ok"
        } else {
            "FAULT"
        };
        let _ = writeln!(o, "  {:<5} {:<7} {}, {}", r.name, label, l.status, l.age);
    }

    // ── PROCESSES ────────────────────────────────────────────────────────────
    //
    // Only for a node polled as several processes. Each process has its own
    // line above and below as `<node>/<index>`; this adds them up, because one
    // process's counters are its share of the node and not the node.
    if !d.process_groups.is_empty() {
        let _ = writeln!(
            o,
            "\nPROCESSES  (each node's processes added together; every process is also listed \
             as <node>/<index>)"
        );
        for g in &d.process_groups {
            let label = if g.down.is_empty() { "ok" } else { "FAULT" };
            let _ = write!(
                o,
                "  {:<5} {:<7} {} of {} processes up, {} request(s), {} hit(s), {} miss(es)",
                g.node,
                label,
                g.up,
                g.processes,
                g.requests_total,
                g.cache_hits_total,
                g.cache_misses_total
            );
            if g.reporting < g.processes {
                let _ = write!(o, " (from {} of {} reports)", g.reporting, g.processes);
            }
            if !g.down.is_empty() {
                let _ = write!(o, "; not answering: {}", g.down.join(", "));
            }
            let _ = writeln!(o);
        }
    }

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
    // ssh to each node for figures already in hand.
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
                    // A monitor channel has requests and no hit or miss, so it
                    // is skipped here. This section compares visitor traffic.
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
            n.uptime_s.map(fmt_uptime).unwrap_or_else(|| "-".into()),
        );
        // What each node runs, on the line with load and disk rather than in a
        // section of its own: the question "is this node the one that is behind"
        // is asked at the same moment as "is this node the busy one".
        //
        // Name, version AND build hash, because the version alone cannot tell
        // two builds of one tag apart and reading three identical version
        // strings is exactly what "the fleet agrees" looks like while it
        // does not. Twelve characters of the hash: enough to compare by
        // eye and the prefix an operator's notes would quote.
        //
        // The certificate is on this line too, for the same reason: "how long
        // has this node got" is asked beside "what is it running", and a
        // certificate reported anywhere an operator does not already look is
        // reported nowhere. From the certificate m6-http loaded, so it is what
        // the node is serving rather than what is on its disk.
        //
        // The fingerprint sits beside the expiry because the expiry alone
        // cannot tell two certificates apart when they expire on the same day,
        // and issue #275 is that: a reader comparing two nodes needs the
        // identity, not only the countdown. Twelve characters, like the build
        // hash, enough to compare by eye.
        let _ = writeln!(
            o,
            "        {} {} build {} cert {} fp {}",
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
            n.cert_fingerprint
                .map(|f| f.short())
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
                } else if hc.error.is_some() {
                    let _ = writeln!(o, "  {:<5} FAULT  {}", r.name, header_check_text(hc));
                } else {
                    let code = hc
                        .status
                        .map(|c| format!(" (HTTP {c})"))
                        .unwrap_or_default();
                    let _ = writeln!(
                        o,
                        "  {:<5} FAULT  {}{}",
                        r.name,
                        header_check_text(hc),
                        code
                    );
                }
            }
        }
    }

    let _ = writeln!(
        o,
        "\nE. CRAWLERS  (reported every run, even a quiet one; a user agent is a claim)"
    );
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
            let more = if c.client_ips.len() > CHECKED_ADDRESSES {
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
            if !c.first_seen.is_empty() {
                let _ = writeln!(o, "        seen: {} .. {}", c.first_seen, c.last_seen);
            }
            for line in verdict_lines(c, verdicts) {
                let _ = writeln!(o, "        {line}");
            }
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
    if faults.is_empty() && warns.is_empty() {
        let _ = writeln!(o, "ALL CLEAR on all {} nodes.", d.nodes.len());
    }
    let _ = writeln!(o, "{bar}");
    o
}

/// One node's HEALTH line.
struct HealthLine {
    /// Whether the last 200 is older than the limit.
    stale: bool,
    /// `200`, `503 degraded` with the reason, or what the node did instead.
    status: String,
    /// `last 200 45s ago`, or which absence the line is reporting.
    age: String,
}

/// What this poll's `/health` was, in words. Whether it is a fault is
/// `NodeReading::status()`, the one rule, and this only describes it.
fn health_text(r: &NodeReading) -> String {
    if let Some(why) = &r.unreachable {
        return format!("unreachable: {why}");
    }
    let code = r
        .health_status
        .map_or("no answer".into(), |c| c.to_string());
    match r.status() {
        "ok" => code,
        "degraded" => format!("{code} degraded: {}", crate::digest::DEGRADED_REASON),
        other => format!("{code} {other}"),
    }
}

fn health_line(
    r: &NodeReading,
    history: Option<&crate::history::HistoryDigest>,
    limit_s: u64,
) -> HealthLine {
    let (age, stale) = if r.status() == "ok" {
        // This poll's 200 is the node's last 200, and history cannot make it
        // older. A node that has just recovered must not be flagged for the
        // time it was away.
        ("last 200 0s ago".to_string(), false)
    } else {
        last_ok(r, history, limit_s)
    };
    HealthLine {
        stale,
        status: health_text(r),
        age,
    }
}

/// The age of a node's last 200, and whether it is over `limit_s`.
fn last_ok(
    r: &NodeReading,
    history: Option<&crate::history::HistoryDigest>,
    limit_s: u64,
) -> (String, bool) {
    let Some(h) = history else {
        return (
            "last 200 unknown (--check keeps no history)".to_string(),
            false,
        );
    };
    if h.state == "off" {
        return ("last 200 unknown (history off)".to_string(), false);
    }
    let Some(list) = &h.last_ok else {
        return (
            "last 200 unknown (history could not be read)".to_string(),
            false,
        );
    };
    match list.iter().find(|l| l.node == r.name) {
        None => ("last 200 unknown (no 200 in history)".to_string(), false),
        Some(l) if l.age_s > limit_s => (
            format!("last 200 {}s ago, over the {limit_s}s limit", l.age_s),
            true,
        ),
        Some(l) => (format!("last 200 {}s ago", l.age_s), false),
    }
}

/// The verdict's finding for a node whose last 200 is over the limit.
///
/// The HEALTH line already prints the fault. This puts the same judgement in
/// the FAULTS list and the exit status, so a line that reads FAULT cannot sit
/// above an ALL CLEAR.
pub(crate) fn stale_last_ok_finding(
    r: &NodeReading,
    history: Option<&crate::history::HistoryDigest>,
    limit_s: u64,
) -> Option<Finding> {
    if r.status() == "ok" {
        return None;
    }
    let (text, stale) = last_ok(r, history, limit_s);
    stale.then(|| Finding {
        level: Level::Fault,
        node: r.name.clone(),
        text,
    })
}

/// Whether each address behind a sighting supports the agent's claim.
///
/// One line per address, because a sighting with three addresses where two
/// verify and one does not is a different thing from three that all verify,
/// and a single summary verdict would hide which.
///
/// **The expectation comes from the operator's documented method, never from
/// the agent's own URL.** An earlier version of this compared the lookup
/// against the domain in the user agent and marked genuine Googlebot, bingbot
/// and Amazonbot as forgeries, because none of the three publishes PTR records
/// under the domain its agent string advertises. See the note above
/// `telemetry::KNOWN_CRAWLERS`.
///
/// Every outcome is kept distinct. Only a name that resolves under nothing the
/// operator documents is called a mismatch. "No PTR record" may be an honest
/// crawler behind a provider that publishes none; "checked against published
/// ranges" means this method cannot settle it at all; "no published method"
/// means we have no expectation to judge against. Reporting any of those as a
/// forgery would be a confident answer nobody measured, and reporting any as a
/// pass is the failure this section exists to stop.
fn verdict_lines(c: &m6_core::monitoring::CrawlerReport, verdicts: &Verdicts) -> Vec<String> {
    use m6_core::telemetry::{verify_crawler, CrawlerVerdict};

    let mut out = Vec::new();
    for ip in c.client_ips.iter().take(CHECKED_ADDRESSES) {
        // Nothing here resolves. An address with no stored result was past
        // the per-poll cap, or the poll has not reached it yet.
        let Some(ptr) = verdicts.get(ip) else {
            out.push(format!("{ip} -> not looked up yet  UNVERIFIED"));
            continue;
        };
        let line = match verify_crawler(&c.user_agent, ptr) {
            CrawlerVerdict::Verified { name, resolved } => {
                format!("{ip} -> {resolved}  VERIFIED as {name}")
            }
            CrawlerVerdict::Mismatch {
                name,
                resolved,
                expected,
            } => format!(
                "{ip} -> {resolved}  MISMATCH: claims {name}, which resolves under {}",
                expected.join(" or ")
            ),
            CrawlerVerdict::NotCheckableByPtr {
                name,
                how,
                resolved,
            } => {
                let seen = resolved
                    .map(|r| format!(" -> {r}"))
                    .unwrap_or_else(|| " -> no PTR record".into());
                format!("{ip}{seen}  UNVERIFIED: {name} is checked against {how}")
            }
            CrawlerVerdict::Unrecognised { resolved } => match resolved {
                Some(r) => format!("{ip} -> {r}  no published method for this agent"),
                None => format!("{ip} -> no PTR record  no published method for this agent"),
            },
            CrawlerVerdict::ForwardMismatch { name, resolved } => match name {
                Some(n) => format!(
                    "{ip} -> {resolved}  MISMATCH: claims {n}, but {resolved} does not resolve back to {ip}"
                ),
                None => format!(
                    "{ip} -> {resolved}  MISMATCH: {resolved} does not resolve back to {ip}"
                ),
            },
            CrawlerVerdict::NoPtr { name } => match name {
                Some(n) => format!("{ip} -> no PTR record  UNVERIFIED, claims {n}"),
                None => format!("{ip} -> no PTR record  UNVERIFIED"),
            },
            CrawlerVerdict::LookupFailed { .. } => {
                format!("{ip} -> lookup failed  UNVERIFIED")
            }
        };
        out.push(line);
    }
    if c.client_ips.len() > CHECKED_ADDRESSES {
        out.push(format!(
            "(+{} more address(es) not checked)",
            c.client_ips.len() - CHECKED_ADDRESSES
        ));
    }
    out
}

/// Seconds left on a certificate, as days.
///
/// Days because that is the unit renewal is discussed in, and a bare `12d` next
/// to a build hash is read correctly without a legend. Separate from `fmt_uptime`
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

    /// A render with no stored lookup results.
    pub fn render(d: &Digest, readings: &[NodeReading]) -> String {
        super::render(d, readings, &Verdicts::new())
    }
    use m6_core::monitoring::{LoggingHealth, TrafficReport};

    fn traffic(logging: LoggingHealth) -> TrafficReport {
        TrafficReport {
            node: "origin".into(),
            window_minutes: 60,
            events_dropped: 0,
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

    /// A node that answers `/health` with 200, the state every section of this
    /// report describes unless a test says otherwise.
    pub(super) fn reading(name: &str, t: Option<TrafficReport>) -> NodeReading {
        NodeReading {
            name: name.into(),
            role: "origin".into(),
            url: "https://origin.example.com".into(),
            health: Some(m6_core::monitoring::HealthReport {
                process_index: None,
                status: "ok".into(),
                node: name.into(),
            }),
            health_status: Some(200),
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

    /// The PROCESSES section exists only for a node polled as processes, and
    /// names the one that is down. A plain fleet prints no such section, which
    /// is what keeps its output as it was.
    #[test]
    fn the_processes_section_appears_only_for_processes() {
        let plain = crate::digest::build(
            &[reading("origin", None)],
            &crate::digest::Thresholds::default(),
            "now".into(),
        );
        assert!(!render(&plain, &[reading("origin", None)]).contains("PROCESSES"));

        let mut down = reading("edge/1", None);
        down.unreachable = Some("refused".into());
        let readings = vec![reading("edge/0", None), down];
        let d = crate::digest::build(
            &readings,
            &crate::digest::Thresholds::default(),
            "now".into(),
        );
        let text = render(&d, &readings);
        assert!(text.contains("PROCESSES"), "{text}");
        assert!(
            text.contains("1 of 2 processes up") && text.contains("not answering: edge/1"),
            "{text}"
        );
    }

    /// A node up minutes, not a whole hour, must read "42m" in the check rather
    /// than collapsing to "0h". The page and the check share one formatter, and
    /// this pins the minute-aware wording the check renders.
    #[test]
    fn a_node_up_minutes_reads_minutes_in_the_check() {
        let mut r = reading("origin", None);
        r.perf = Some(crate::digest::tests::perf(
            m6_core::platform::host::HostSnapshot {
                cpus: 1,
                ..Default::default()
            },
            vec![],
        ));
        r.perf.as_mut().unwrap().uptime_s = 42 * 60;
        let d = crate::digest::build(
            std::slice::from_ref(&r),
            &crate::digest::Thresholds::default(),
            "now".into(),
        );
        let out = render(&d, std::slice::from_ref(&r));
        assert!(out.contains("svc up 42m"), "{out}");
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

    /// The check and the JSON digest describe one traffic report the same way.
    ///
    /// This is the #220 guard. Every fact section D and E print must appear in
    /// `nodes[].traffic`, with the same number. A fact printed but not
    /// serialised, or serialised under a different name, fails here.
    #[test]
    fn the_check_and_the_json_digest_agree_on_traffic_and_security() {
        use m6_core::firewall::{Block, FirewallState};
        use m6_core::monitoring::HeavyHitter;

        let mut t = traffic(LoggingHealth {
            events_total: 9000,
            seconds_since_last: Some(600),
            reopen: None,
        });
        t.total_requests = 1234;
        t.status.insert(200, 1200);
        t.status.insert(404, 34);
        t.heavy_hitters = vec![HeavyHitter {
            ip: "203.0.113.7".into(),
            requests: 900,
            top_path: "/".into(),
            distinct_user_agents: 3,
            error_ratio: 0.5,
        }];
        t.notable = vec![probing_client("203.0.113.9", &["curl/8.4.0"], 1)];
        t.crawlers = vec![crawler("GPTBot/1.0", &["203.0.113.20"])];
        t.forged_bot_requests = 12;
        t.forgers = vec!["203.0.113.66".into()];
        t.probe_noise = vec!["203.0.113.5 /.git/config".into()];
        t.firewall = Some(FirewallState {
            blocks: vec![Block {
                address: "203.0.113.4".into(),
                packets: 1215,
                bytes: 72900,
                comment: None,
                chain: "INPUT".into(),
            }],
            total_rules: 217,
        });

        let r = reading("origin", Some(t));
        let d = crate::digest::build(
            std::slice::from_ref(&r),
            &crate::digest::Thresholds::default(),
            "2026-10-04T02:16:31Z".into(),
        );
        let out = render(&d, std::slice::from_ref(&r));
        let v = serde_json::to_value(&d).unwrap();
        let j = &v["nodes"][0]["traffic"];

        // Section D: the totals, the loud clients, the probes and the firewall.
        assert_eq!(j["total_requests"], 1234);
        assert!(out.contains("1234 requests in 60m"), "{out}");
        assert_eq!(j["window_minutes"], 60);
        assert_eq!(j["status"]["200"], 1200);
        assert_eq!(j["status"]["404"], 34);
        assert_eq!(j["heavy_hitters"][0]["ip"], "203.0.113.7");
        assert_eq!(j["heavy_hitters"][0]["requests"], 900);
        assert_eq!(j["heavy_hitters"][0]["error_ratio"], 0.5);
        assert!(
            out.contains("top: 203.0.113.7 x900 / (50% refused, 3 UA)"),
            "{out}"
        );
        assert_eq!(j["notable"][0]["ip"], "203.0.113.9");
        assert_eq!(j["notable"][0]["requests"], 7);
        assert!(out.contains("NOTABLE 203.0.113.9 x7"), "{out}");
        assert_eq!(j["notable"][0]["user_agents"][0], "curl/8.4.0");
        assert!(out.contains("curl/8.4.0"), "{out}");
        assert_eq!(j["probe_noise"][0], "203.0.113.5 /.git/config");
        assert!(
            out.contains("single refused probes (noise, not escalated): 203.0.113.5 /.git/config"),
            "{out}"
        );
        assert_eq!(j["firewall"]["blocks"], 1);
        assert_eq!(j["firewall"]["total_rules"], 217);
        assert!(
            out.contains("firewall: 1 block(s) of 217 rules, 1 still being hit"),
            "{out}"
        );
        assert_eq!(j["firewall"]["hit"][0]["packets"], 1215);
        assert!(
            out.contains("hit 203.0.113.4 1215 packets 72900 bytes"),
            "{out}"
        );

        // Section E: the forged count and the genuine sighting.
        assert_eq!(j["forged_bot_requests"], 12);
        assert_eq!(j["forgers"][0], "203.0.113.66");
        assert!(
            out.contains("12 bot-shaped requests were FORGED by 203.0.113.66"),
            "{out}"
        );
        assert_eq!(j["crawlers"][0]["user_agent"], "GPTBot/1.0");
        assert_eq!(j["crawlers"][0]["requests"], 3);
        assert!(out.contains("UA: GPTBot/1.0"), "{out}");

        // Section A's first line comes from the same report.
        assert_eq!(j["logging"]["state"], "quiet");
        assert_eq!(j["logging"]["events_total"], 9000);
        assert!(out.contains("FAULT  main log silent for 600s"), "{out}");
    }

    // ── Crawler verification, issue #205 ─────────────────────────────────────

    fn crawler(ua: &str, ips: &[&str]) -> m6_core::monitoring::CrawlerReport {
        m6_core::monitoring::CrawlerReport {
            user_agent: ua.into(),
            requests: 3,
            client_ips: ips.iter().map(|s| s.to_string()).collect(),
            paths: vec!["/robots.txt".into()],
            first_seen: "2026-10-01T09:00:00Z".into(),
            last_seen: "2026-10-01T09:05:00Z".into(),
        }
    }

    /// THE REGRESSION THIS CLOSES, and it is the whole reason the table exists.
    ///
    /// These three user agents and PTR names follow the shape each operator
    /// publishes. Every one of them advertises a documentation domain that is
    /// NOT where its operator publishes PTR records, so comparing the lookup
    /// against the agent's own URL marks all three as forgeries. A verifier
    /// that accuses the genuine ones gets ignored, and then so does a true
    /// finding.
    ///
    /// The cache is pre-populated, so none of this touches a resolver.
    #[test]
    fn genuine_crawlers_whose_ptr_differs_from_their_url_are_not_called_forgeries() {
        use m6_core::resolve::Ptr;
        let cases = [
            (
                "Mozilla/5.0 AppleWebKit/537.36 (KHTML, like Gecko; compatible; Amazonbot/0.1; +https://developer.amazon.com/support/amazonbot) Chrome/119.0",
                "192-0-2-1.crawl.amazonbot.amazon",
                "Amazonbot",
            ),
            (
                "Mozilla/5.0 AppleWebKit/537.36 (KHTML, like Gecko; compatible; bingbot/2.0; +http://www.bing.com/bingbot.htm) Chrome/116.0",
                "msnbot-192-0-2-2.search.msn.com",
                "bingbot",
            ),
            (
                "Mozilla/5.0 (compatible; Googlebot/2.1; +http://www.google.com/bot.html)",
                "crawl-192-0-2-3.googlebot.com",
                "Googlebot",
            ),
        ];
        for (ua, ptr, name) in cases {
            let mut cache = std::collections::HashMap::new();
            cache.insert("192.0.2.1".to_string(), Ptr::Name(ptr.into()));
            let out = verdict_lines(&crawler(ua, &["192.0.2.1"]), &cache);
            assert!(
                out[0].contains("VERIFIED") && out[0].contains(name),
                "{name} must verify against {ptr}, got {out:?}"
            );
            assert!(!out[0].contains("MISMATCH"), "{name}: {out:?}");
        }
    }

    /// An operator that publishes address ranges instead of PTR records cannot
    /// be settled this way, and that is a third answer: not a forgery and not a pass.
    #[test]
    fn an_operator_publishing_ranges_is_reported_as_uncheckable_not_forged() {
        use m6_core::resolve::Ptr;
        let mut cache = std::collections::HashMap::new();
        cache.insert("192.0.2.1".to_string(), Ptr::None);
        for ua in [
            "Mozilla/5.0 AppleWebKit/537.36 (KHTML, like Gecko; compatible; ClaudeBot/1.0; +claudebot@anthropic.com)",
            "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36; compatible; OAI-SearchBot/1.0; +https://openai.com/searchbot",
        ] {
            let out = verdict_lines(&crawler(ua, &["192.0.2.1"]), &cache);
            assert!(out[0].contains("UNVERIFIED"), "{out:?}");
            assert!(out[0].contains("published address ranges"), "{out:?}");
            assert!(!out[0].contains("MISMATCH"), "{out:?}");
        }
    }

    /// A forgery still has to be called one. An address claiming a crawler
    /// whose operator documents PTR records, resolving somewhere else, is the
    /// one case this can call false, and it says what the address actually is.
    #[test]
    fn a_real_forgery_is_still_called_a_mismatch() {
        use m6_core::resolve::Ptr;
        let mut cache = std::collections::HashMap::new();
        cache.insert(
            "192.0.2.9".to_string(),
            Ptr::Name("vps-1234.cheap-hosting.example".into()),
        );
        let out = verdict_lines(
            &crawler(
                "Mozilla/5.0 (compatible; Googlebot/2.1; +http://www.google.com/bot.html)",
                &["192.0.2.9"],
            ),
            &cache,
        );
        assert!(out[0].contains("MISMATCH"), "{out:?}");
        assert!(out[0].contains("vps-1234.cheap-hosting.example"), "{out:?}");
        assert!(
            out[0].contains("googlebot.com"),
            "expected domains named: {out:?}"
        );
    }

    /// A forged PTR (the name is right, the forward lookup does not return the
    /// address) is called a mismatch and says why.
    #[test]
    fn a_ptr_that_does_not_resolve_back_is_called_a_mismatch() {
        use m6_core::resolve::Ptr;
        let mut cache = std::collections::HashMap::new();
        cache.insert(
            "192.0.2.9".to_string(),
            Ptr::Unconfirmed("crawl-192-0-2-3.googlebot.com".into()),
        );
        let out = verdict_lines(
            &crawler(
                "Mozilla/5.0 (compatible; Googlebot/2.1; +http://www.google.com/bot.html)",
                &["192.0.2.9"],
            ),
            &cache,
        );
        assert!(out[0].contains("MISMATCH"), "{out:?}");
        assert!(
            out[0].contains("does not resolve back to 192.0.2.9"),
            "{out:?}"
        );
        assert!(!out[0].contains("VERIFIED"), "{out:?}");
    }

    fn sighted(ips: &[&str]) -> Vec<NodeReading> {
        let mut r = reading("origin", None);
        let mut t = traffic(LoggingHealth {
            events_total: 1,
            seconds_since_last: Some(1),
            reopen: None,
        });
        t.crawlers = vec![crawler(
            "Mozilla/5.0 (compatible; Googlebot/2.1; +http://www.google.com/bot.html)",
            ips,
        )];
        r.traffic = Some(t);
        vec![r]
    }

    /// The cap: with more new addresses than the cap allows, a poll looks up
    /// the cap and no more, the rest read as not looked up, and the next poll
    /// takes the next ones without repeating the first.
    #[test]
    fn a_poll_looks_up_at_most_the_cap_and_the_next_poll_continues() {
        use m6_core::resolve::Ptr;
        let calls = std::cell::RefCell::new(Vec::<String>::new());
        let lookup = |ip: &str| {
            calls.borrow_mut().push(ip.to_string());
            Ptr::None
        };
        // Four sightings of three addresses each: twelve distinct addresses.
        let mut readings = Vec::new();
        for n in 0..4 {
            let ips: Vec<String> = (1..=3).map(|k| format!("192.0.2.{}", n * 3 + k)).collect();
            let ips: Vec<&str> = ips.iter().map(String::as_str).collect();
            readings.extend(sighted(&ips));
        }
        let mut cache = VerdictCache::new();
        let now = m6_core::platform::now();
        let first = cache.refresh(&readings, now, &lookup);
        assert_eq!(calls.borrow().len(), MAX_LOOKUPS_PER_POLL);
        assert_eq!(first.len(), MAX_LOOKUPS_PER_POLL);
        let second = cache.refresh(&readings, now, &lookup);
        assert_eq!(calls.borrow().len(), 12, "the rest, and no repeats");
        assert_eq!(second.len(), 12);
        let mut seen = calls.borrow().clone();
        seen.sort();
        seen.dedup();
        assert_eq!(seen.len(), 12, "no address looked up twice");
    }

    #[test]
    fn a_one_shot_check_looks_up_past_the_poll_cap() {
        use m6_core::resolve::Ptr;
        let calls = std::cell::Cell::new(0usize);
        let lookup = |_: &str| {
            calls.set(calls.get() + 1);
            Ptr::None
        };
        let mut readings = Vec::new();
        for n in 0..4 {
            let ips: Vec<String> = (1..=3).map(|k| format!("192.0.2.{}", n * 3 + k)).collect();
            let ips: Vec<&str> = ips.iter().map(String::as_str).collect();
            readings.extend(sighted(&ips));
        }
        let v = VerdictCache::with_cap(MAX_LOOKUPS_PER_CHECK).refresh(
            &readings,
            m6_core::platform::now(),
            &lookup,
        );
        assert_eq!(calls.get(), 12);
        assert_eq!(v.len(), 12);
    }

    /// The poll's split path and the one-shot path agree: `due` never returns
    /// more than the cap, a `record` for each answer, then `verdicts` gives the
    /// same map `refresh` gives from the same lookups. The next poll continues
    /// with the addresses the cap held back, without repeating one.
    #[test]
    fn crawler_lookups_go_through_confirm() {
        use m6_core::resolve::Ptr;
        let answer = |ip: &str| {
            if ip.ends_with(".1") {
                Ptr::Name("crawl.example.com".into())
            } else if ip.ends_with(".2") {
                Ptr::None
            } else {
                Ptr::Failed
            }
        };
        let mut readings = Vec::new();
        for n in 0..4 {
            let ips: Vec<String> = (1..=3).map(|k| format!("192.0.2.{}", n * 3 + k)).collect();
            let ips: Vec<&str> = ips.iter().map(String::as_str).collect();
            readings.extend(sighted(&ips));
        }
        let now = m6_core::platform::now();

        let mut cache = VerdictCache::new();
        let mut due = Vec::new();
        cache.due(&readings, now, &mut due);
        assert_eq!(due.len(), MAX_LOOKUPS_PER_POLL, "the cap holds");
        for ip in &due {
            let ptr = answer(ip);
            cache.record(ip, ptr, now);
        }
        let split = cache.verdicts(&readings);

        let mut through_refresh = VerdictCache::new();
        let refreshed = through_refresh.refresh(&readings, now, &answer);
        assert_eq!(split.len(), refreshed.len());
        for (addr, ptr) in &refreshed {
            assert_eq!(split.get(addr), Some(ptr), "{addr}");
        }

        let mut rest = Vec::new();
        cache.due(&readings, now, &mut rest);
        assert_eq!(rest.len(), 12 - MAX_LOOKUPS_PER_POLL, "the rest, once");
        assert!(
            rest.iter().all(|ip| !due.contains(ip)),
            "an address looked up is not due again: {rest:?} after {due:?}"
        );
    }

    /// A stored result is reused until it expires, a failure expires sooner,
    /// and the cache never holds more than its bound.
    #[test]
    fn stored_results_expire_and_the_cache_is_bounded() {
        use m6_core::resolve::Ptr;
        let n = std::cell::Cell::new(0);
        let lookup = |_: &str| {
            n.set(n.get() + 1);
            Ptr::Failed
        };
        let readings = sighted(&["192.0.2.1"]);
        let mut cache = VerdictCache::new();
        let t0 = m6_core::platform::now();
        cache.refresh(&readings, t0, &lookup);
        cache.refresh(&readings, t0 + std::time::Duration::from_secs(60), &lookup);
        assert_eq!(n.get(), 1, "a failure is kept for FAILED_TTL");
        cache.refresh(
            &readings,
            t0 + FAILED_TTL + std::time::Duration::from_secs(1),
            &lookup,
        );
        assert_eq!(n.get(), 2, "and looked up again after it");

        let mut big = VerdictCache::new();
        for k in 0..(MAX_CACHED_VERDICTS + 50) {
            let ip = format!("198.51.{}.{}", k / 200, k % 200 + 1);
            big.refresh(&sighted(&[ip.as_str()]), t0, &|_| Ptr::None);
        }
        assert!(big.entries.len() <= MAX_CACHED_VERDICTS);
    }

    /// Rendering reads stored results and has no way to look anything up: its
    /// inputs are data, so `/check` cannot wait on a resolver. An address with
    /// no stored result reads unverified.
    #[test]
    fn render_reads_stored_verdicts_and_never_looks_anything_up() {
        use m6_core::resolve::Ptr;
        let readings = sighted(&["192.0.2.1", "192.0.2.2"]);
        let mut v = Verdicts::new();
        v.insert(
            "192.0.2.1".into(),
            Ptr::Name("crawl-192-0-2-1.googlebot.com".into()),
        );
        let out = super::render(&digest(), &readings, &v);
        assert!(out.contains("VERIFIED"), "{out}");
        assert!(
            out.contains("192.0.2.2 -> not looked up yet  UNVERIFIED"),
            "{out}"
        );
    }

    /// An agent in no table gets its lookup reported and no verdict, because
    /// there is no published expectation to judge it against. Inventing one is
    /// what produced the regression above.
    #[test]
    fn an_unrecognised_agent_gets_no_verdict() {
        use m6_core::resolve::Ptr;
        let mut cache = std::collections::HashMap::new();
        cache.insert(
            "192.0.2.1".to_string(),
            Ptr::Name("host.example.net".into()),
        );
        let out = verdict_lines(
            &crawler(
                "Mozilla/5.0 (compatible; NeverHeardOfItBot/1.0)",
                &["192.0.2.1"],
            ),
            &cache,
        );
        assert!(out[0].contains("no published method"), "{out:?}");
        assert!(!out[0].contains("MISMATCH"), "{out:?}");
        assert!(!out[0].contains("VERIFIED"), "{out:?}");
    }

    /// No PTR for a PTR-documented crawler is unverified and names the claim.
    #[test]
    fn a_missing_ptr_for_a_ptr_documented_crawler_is_unverified() {
        use m6_core::resolve::Ptr;
        let mut cache = std::collections::HashMap::new();
        cache.insert("192.0.2.1".to_string(), Ptr::None);
        cache.insert("192.0.2.2".to_string(), Ptr::Failed);
        let ua = "Mozilla/5.0 (compatible; Googlebot/2.1; +http://www.google.com/bot.html)";
        let out = verdict_lines(&crawler(ua, &["192.0.2.1"]), &cache);
        assert!(
            out[0].contains("no PTR record") && out[0].contains("UNVERIFIED"),
            "{out:?}"
        );
        assert!(out[0].contains("Googlebot"), "{out:?}");
        let out = verdict_lines(&crawler(ua, &["192.0.2.2"]), &cache);
        assert!(
            out[0].contains("lookup failed") && out[0].contains("UNVERIFIED"),
            "{out:?}"
        );
        assert!(!out[0].contains("MISMATCH"), "{out:?}");
    }

    /// Every address gets its own line, because two verifying and one not is
    /// a different fact from three verifying.
    #[test]
    fn each_address_is_reported_separately_and_the_rest_are_counted() {
        use m6_core::resolve::Ptr;
        let ua = "Mozilla/5.0 (compatible; Googlebot/2.1; +http://www.google.com/bot.html)";
        let mut cache = std::collections::HashMap::new();
        for ip in ["192.0.2.1", "192.0.2.3", "192.0.2.4"] {
            cache.insert(ip.to_string(), Ptr::Name("crawl.googlebot.com".into()));
        }
        cache.insert("192.0.2.2".to_string(), Ptr::None);

        let c = crawler(ua, &["192.0.2.1", "192.0.2.2", "192.0.2.3", "192.0.2.4"]);
        let out = verdict_lines(&c, &cache);
        assert_eq!(out.len(), 4, "three addresses plus the count: {out:?}");
        assert!(out[0].contains("VERIFIED"), "{out:?}");
        assert!(out[1].contains("UNVERIFIED"), "{out:?}");
        assert!(out[3].contains("+1 more"), "{out:?}");
    }

    /// Section E carries the window and the verdict, and still appears on a
    /// quiet run.
    #[test]
    fn the_crawler_section_carries_times_and_a_verdict() {
        let mut r = reading("origin", None);
        let mut t = traffic(LoggingHealth {
            events_total: 1,
            seconds_since_last: Some(1),
            reopen: None,
        });
        t.crawlers = vec![crawler(
            "Mozilla/5.0 (compatible; Googlebot/2.1; +http://www.google.com/bot.html)",
            &["192.0.2.1"],
        )];
        r.traffic = Some(t);
        let out = render(&digest(), &[r]);
        assert!(out.contains("a user agent is a claim"), "{out}");
        assert!(
            out.contains("seen: 2026-10-01T09:00:00Z .. 2026-10-01T09:05:00Z"),
            "{out}"
        );
        assert!(out.contains("192.0.2.1 ->"), "{out}");
    }
}

/// The cache-header section, and the verdict it must be able to change.
///
/// These assert the part that is easy to get wrong: a check that reports a
/// failure in its own section and then prints ALL CLEAR at the bottom is worse
/// than no check, because the summary is what gets read.
#[cfg(test)]
mod header_check_tests {
    use super::tests::{hc, reading, render};
    use super::*;

    fn out(rs: &[NodeReading]) -> String {
        let d = crate::digest::build(rs, &crate::digest::Thresholds::default(), "now".into());
        render(&d, rs)
    }

    #[test]
    fn a_passing_check_is_reported_and_stays_all_clear() {
        let mut r = reading("origin", None);
        r.header_checks.push(hc(
            "/capabilities",
            "public, max-age=60",
            Some("public, max-age=60"),
        ));
        let out = out(&[r]);
        assert!(out.contains("F. CACHE HEADERS"), "{out}");
        assert!(out.contains("/capabilities"), "{out}");
        assert!(
            out.contains("ALL CLEAR"),
            "a passing check must not raise a fault:\n{out}"
        );
    }

    /// The one that matters. A wrong cache policy must reach the verdict, and
    /// it reaches it through the digest's shared FAULTS list, not a side list.
    #[test]
    fn a_wrong_header_is_a_fault_and_removes_all_clear() {
        let mut r = reading("origin", None);
        r.header_checks
            .push(hc("/capabilities", "public, max-age=60", Some("no-store")));
        let out = out(&[r]);
        assert!(out.contains("FAULTS (1)"), "{out}");
        assert!(
            out.contains(
                "- origin: /capabilities cache-control is \"no-store\", expected \"public, max-age=60\""
            ),
            "{out}"
        );
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
        let out = out(&[r]);
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
        let out = out(&[r]);
        assert!(out.contains("could not be fetched"), "{out}");
        assert!(!out.contains("ALL CLEAR"), "{out}");
    }

    /// Nothing configured must not read as everything passing.
    #[test]
    fn no_checks_declared_says_so() {
        let out = out(&[reading("origin", None)]);
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
    use super::tests::{digest, reading, render};
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
        let mut host = m6_core::platform::host::HostSnapshot::default();
        host.net.push(m6_core::platform::host::NetDevice {
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
            process_index: None,
            status: "ok".into(),
            node: "origin".into(),
        });
        r.health_status = Some(200);
        let mut p: PerfReport = crate::digest::tests::perf(
            m6_core::platform::host::HostSnapshot {
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
    use super::tests::{digest, reading, render};
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
            last_ok: None,
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
            last_ok: None,
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
    fn a_prune_failure_and_an_unmet_cap_are_not_a_missing_poll() {
        for (state, error, said) in [
            (
                "prune_failed",
                "polls are kept",
                "could not delete old polls",
            ),
            (
                "over_cap",
                "the history is over its cap",
                "is over its cap of",
            ),
        ] {
            let mut h = history(state);
            h.last_error = Some(error.into());
            let mut d = digest();
            d.history = Some(h.clone());
            d.add_finding(crate::history::finding(&h).expect("a finding"));
            let o = render(&d, &[reading("origin", None)]);
            assert!(!o.contains("ALL CLEAR"), "{o}");
            assert!(o.contains(said), "{o}");
            assert!(o.contains(&format!("last error: {error}")), "{o}");
            assert!(!o.contains("that poll is missing"), "{o}");
        }
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

/// The HEALTH section: this poll's `/health` per node, and the age of each
/// node's last 200 from the history the service already keeps.
///
/// These run against a real history file, so the age the section prints is the
/// age the service would have rather than a number handed to the renderer.
#[cfg(test)]
mod health_tests {
    use super::tests::reading;
    use super::*;

    fn degraded(name: &str) -> NodeReading {
        let mut r = reading(name, None);
        r.health = Some(m6_core::monitoring::HealthReport {
            process_index: None,
            status: "degraded".into(),
            node: name.into(),
        });
        r.health_status = Some(503);
        r
    }

    fn unreachable(name: &str, why: &str) -> NodeReading {
        let mut r = reading(name, None);
        r.health = None;
        r.health_status = None;
        r.unreachable = Some(why.into());
        r
    }

    /// The line a node is printed on. The first section a node appears in is
    /// HEALTH, so one match is enough to find its line there.
    fn line_of<'a>(out: &'a str, node: &str) -> &'a str {
        let start = format!("  {node}");
        out.lines()
            .find(|l| l.starts_with(&start))
            .unwrap_or_else(|| panic!("no line for {node}:\n{out}"))
    }

    fn check(rs: &[NodeReading], history: Option<crate::history::HistoryDigest>) -> String {
        let mut d = crate::digest::build(
            rs,
            &crate::digest::Thresholds::default(),
            "2026-11-14T00:00:00Z".into(),
        );
        d.history = history;
        super::tests::render(&d, rs)
    }

    /// A history holding one poll, `age_s` seconds before now, in which `node`
    /// answered 200.
    fn history_with_last_ok(
        node: &str,
        age_s: u64,
    ) -> (crate::history::HistoryDigest, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("a temp directory");
        let cfg = crate::history::Config {
            file: dir.path().join("history.sqlite3"),
            max_bytes: 64 << 20,
        };
        let mut history = crate::history::start(Some(cfg)).expect("history opens");
        let r = reading(node, None);
        let at_ms = crate::history::now_ms() - i64::try_from(age_s).expect("a small age") * 1000;
        let d = crate::digest::build(
            std::slice::from_ref(&r),
            &crate::digest::Thresholds::default(),
            "2026-11-14T00:00:00Z".into(),
        );
        history.record(crate::history::batch(at_ms, std::slice::from_ref(&r), &d));
        (history.digest(), dir)
    }

    /// Three nodes that answer 200 are the baseline: one `ok` line each, first
    /// in the report, and nothing for the verdict to raise.
    #[test]
    fn three_healthy_nodes_print_ok_in_the_first_section() {
        let rs = [
            reading("origin", None),
            reading("edge-a", None),
            reading("edge-b", None),
        ];
        let out = check(&rs, None);
        let health = out
            .find("HEALTH")
            .unwrap_or_else(|| panic!("no HEALTH section:\n{out}"));
        let logging = out
            .find("A. LOGGING")
            .unwrap_or_else(|| panic!("no logging section:\n{out}"));
        assert!(health < logging, "HEALTH must come first:\n{out}");
        for name in ["origin", "edge-a", "edge-b"] {
            let line = line_of(&out, name);
            assert!(line.contains("ok"), "{name} is healthy:\n{out}");
            assert!(line.contains("200"), "{name} answered 200:\n{out}");
        }
        assert!(!out.contains("FAULT"), "{out}");
    }

    /// A degraded node is a fault, and the reason the body's verdict carries is
    /// printed beside the code rather than left to the operator to look up.
    #[test]
    fn a_degraded_node_prints_fault_with_the_reason() {
        let (h, _dir) = history_with_last_ok("edge-a", 30);
        let out = check(&[degraded("edge-a")], Some(h));
        let line = line_of(&out, "edge-a");
        assert!(line.contains("FAULT"), "{out}");
        assert!(line.contains("503 degraded"), "{out}");
        assert!(
            line.contains("a configured socket pool has no live member"),
            "{out}"
        );
        assert!(line.contains("last 200"), "{out}");
    }

    /// A node that cannot be reached is a fault too, with the error that
    /// stopped the request.
    #[test]
    fn an_unreachable_node_prints_fault_with_the_error() {
        let (h, _dir) = history_with_last_ok("edge-b", 30);
        let out = check(&[unreachable("edge-b", "connection refused")], Some(h));
        let line = line_of(&out, "edge-b");
        assert!(line.contains("FAULT"), "{out}");
        assert!(line.contains("unreachable"), "{out}");
        assert!(line.contains("connection refused"), "{out}");
        assert!(line.contains("last 200"), "{out}");
    }

    /// A node that has not answered 200 for longer than the limit says so on
    /// its line, with the age and the limit.
    #[test]
    fn a_last_200_older_than_the_limit_prints_fault_with_the_reason() {
        let (h, _dir) = history_with_last_ok("edge-a", 300);
        let out = check(&[degraded("edge-a")], Some(h));
        let line = line_of(&out, "edge-a");
        assert!(line.contains("FAULT"), "{out}");
        assert!(line.contains("last 200"), "{out}");
        assert!(line.contains("over the 90s limit"), "{out}");
    }

    /// The other side of the threshold: a recent 200 is shown and not called
    /// stale, so the comparison has to be doing the deciding.
    #[test]
    fn a_last_200_inside_the_limit_is_not_called_stale() {
        let (h, _dir) = history_with_last_ok("edge-a", 30);
        let out = check(&[degraded("edge-a")], Some(h));
        let line = line_of(&out, "edge-a");
        assert!(line.contains("last 200"), "{out}");
        assert!(!line.contains("over the"), "{out}");
    }

    /// The limit is a setting, so a deployment that wants a longer one gets it
    /// rather than the default.
    #[test]
    fn the_limit_comes_from_the_thresholds() {
        let (h, _dir) = history_with_last_ok("edge-a", 300);
        let r = degraded("edge-a");
        let mut d = crate::digest::build(
            std::slice::from_ref(&r),
            &crate::digest::Thresholds {
                health_last_ok_age_s: 600,
                ..Default::default()
            },
            "2026-11-14T00:00:00Z".into(),
        );
        d.history = Some(h);
        let out = super::tests::render(&d, std::slice::from_ref(&r));
        let line = line_of(&out, "edge-a");
        assert!(line.contains("last 200"), "{out}");
        assert!(!line.contains("over the"), "{out}");
    }

    /// The section and the verdict cannot disagree: the same stale judgement
    /// is added to the FAULTS list that the exit status reads.
    #[test]
    fn a_stale_last_200_reaches_the_verdict() {
        let (h, _dir) = history_with_last_ok("edge-a", 300);
        let r = degraded("edge-a");
        let mut d = crate::digest::build(
            std::slice::from_ref(&r),
            &crate::digest::Thresholds::default(),
            "2026-11-14T00:00:00Z".into(),
        );
        d.history = Some(h);
        let f = stale_last_ok_finding(&r, d.history.as_ref(), d.thresholds.health_last_ok_age_s)
            .expect("a node over the limit raises a finding");
        assert_eq!(f.level, Level::Fault);
        assert!(f.text.contains("over the 90s limit"), "{}", f.text);
        d.add_finding(f);
        let out = super::tests::render(&d, std::slice::from_ref(&r));
        assert!(!out.contains("ALL CLEAR"), "{out}");
        assert!(out.contains("last 200"), "{out}");
    }

    /// A node with this poll's `/health` code and body status.
    fn answering(name: &str, code: u16, body: &str) -> NodeReading {
        let mut r = reading(name, None);
        r.health = Some(m6_core::monitoring::HealthReport {
            process_index: None,
            status: body.into(),
            node: name.into(),
        });
        r.health_status = Some(code);
        r
    }

    /// The verdict a set of readings gives with a recent last 200 on file, so
    /// only this poll's own answer can raise a fault.
    fn verdict(rs: &[NodeReading]) -> String {
        let (h, _dir) = history_with_last_ok("edge-a", 10);
        check(rs, Some(h))
    }

    /// The one rule: a 503 is not healthy whatever its body says.
    #[test]
    fn a_503_with_body_ok_is_a_fault() {
        let out = verdict(&[answering("edge-a", 503, "ok")]);
        assert!(line_of(&out, "edge-a").contains("FAULT"), "{out}");
        assert!(!out.contains("ALL CLEAR"), "{out}");
    }

    /// The 503 never counts as a 200, so the last-200 age keeps growing: the
    /// row the poll writes for it is not an ok row.
    #[test]
    fn a_503_with_body_ok_does_not_reset_the_last_200_age() {
        let dir = tempfile::tempdir().expect("a temp directory");
        let cfg = crate::history::Config {
            file: dir.path().join("history.sqlite3"),
            max_bytes: 64 << 20,
        };
        let mut history = crate::history::start(Some(cfg)).expect("history opens");
        let now = crate::history::now_ms();
        for (ago_s, r) in [
            (300, answering("edge-a", 200, "ok")),
            (10, answering("edge-a", 503, "ok")),
        ] {
            let d = crate::digest::build(
                std::slice::from_ref(&r),
                &crate::digest::Thresholds::default(),
                "2026-11-14T00:00:00Z".into(),
            );
            history.record(crate::history::batch(
                now - ago_s * 1000,
                std::slice::from_ref(&r),
                &d,
            ));
        }
        let last = history.digest().last_ok.expect("history reads");
        assert_eq!(last.len(), 1);
        assert!(last[0].age_s >= 300, "{last:?}");
        let r = answering("edge-a", 503, "ok");
        let f = stale_last_ok_finding(&r, Some(&history.digest()), 90)
            .expect("the age is over the limit");
        assert!(f.text.contains("over the 90s limit"), "{}", f.text);
    }

    #[test]
    fn a_200_with_body_degraded_is_a_fault() {
        let out = verdict(&[answering("edge-a", 200, "degraded")]);
        assert!(line_of(&out, "edge-a").contains("FAULT"), "{out}");
        assert!(!out.contains("ALL CLEAR"), "{out}");
    }

    #[test]
    fn a_200_with_an_unknown_body_is_a_fault() {
        let out = verdict(&[answering("edge-a", 200, "starting")]);
        assert!(line_of(&out, "edge-a").contains("FAULT"), "{out}");
        assert!(!out.contains("ALL CLEAR"), "{out}");
    }

    #[test]
    fn a_node_with_no_answer_is_a_fault() {
        let mut r = reading("edge-a", None);
        r.health = None;
        r.health_status = None;
        let out = verdict(&[r]);
        assert!(line_of(&out, "edge-a").contains("FAULT"), "{out}");
        assert!(!out.contains("ALL CLEAR"), "{out}");
    }

    #[test]
    fn three_healthy_nodes_give_no_fault() {
        let rs = [
            answering("origin", 200, "ok"),
            answering("edge-a", 200, "ok"),
            answering("edge-b", 200, "ok"),
        ];
        let out = check(&rs, None);
        assert!(!out.contains("FAULT"), "{out}");
        assert!(out.contains("ALL CLEAR"), "{out}");
    }
}
