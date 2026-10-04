//! Turning a fleet of readings into one answer.
//!
//! This is the module that is allowed to have opinions. `m6_core::host` and
//! `m6_core::telemetry` report numbers and never judge them; the thresholds
//! live here, because what counts as too full or too hot is a property of a
//! deployment rather than of m6.

use serde::Serialize;

use crate::poll::NodeReading;

/// Thresholds. Deliberately a value rather than constants, so a deployment can
/// set its own without patching the binary.
#[derive(Debug, Clone, Serialize)]
pub struct Thresholds {
    pub disk_used: f64,
    pub memory_used: f64,
    /// Load average per CPU. Load is a queue length, so the only meaningful
    /// comparison is against the number of CPUs.
    pub load_per_cpu: f64,
    pub thermal_celsius: f32,
    /// Days left on the served certificate below which this warns.
    ///
    /// m6-http reports the number and this decides what is too few, which is
    /// the split the rest of this file keeps: core reports, the monitor judges.
    ///
    /// The default is 21 days, and the reasoning is about renewal attempts
    /// rather than about the certificate. An *ACME* (Automatic Certificate
    /// Management Environment) client on the common 90-day certificate starts
    /// renewing at 30 days left and retries daily. A threshold at 21 therefore
    /// means renewal has been failing for about nine days and nine attempts
    /// before anyone is told, which is the point: a threshold inside the
    /// renewal window fires on the first transient failure and gets ignored,
    /// and an ignored warning is the same as no warning. A deployment renewing
    /// on a different schedule sets its own.
    pub cert_expiry_days: i64,
}

impl Default for Thresholds {
    fn default() -> Self {
        Self {
            disk_used: 0.80,
            memory_used: 0.90,
            load_per_cpu: 2.0,
            thermal_celsius: 80.0,
            cert_expiry_days: 21,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub enum Level {
    Ok,
    Warn,
    Fault,
}

#[derive(Debug, Clone, Serialize)]
pub struct Finding {
    pub level: Level,
    pub node: String,
    pub text: String,
}

/// The whole fleet, summarised.
#[derive(Debug, Clone, Serialize)]
pub struct Digest {
    pub generated_at: String,
    pub nodes: Vec<NodeDigest>,
    pub findings: Vec<Finding>,
    /// Worst level across the fleet.
    pub level: Level,
    pub thresholds: Thresholds,
}

#[derive(Debug, Clone, Serialize)]
pub struct NodeDigest {
    pub name: String,
    pub role: String,
    /// Where this reading came from, so a report can be checked against the
    /// fleet config without opening it.
    pub url: String,
    pub status: String,
    /// What the node calls itself. Reported separately from `name` so that a
    /// config pointing at the wrong box is visible rather than silently
    /// relabelled.
    pub reported_node: Option<String>,
    pub rtt_ms: Option<f64>,
    /// The m6 release the node is running, from `/perf`. `None` when the node
    /// predates the field, which reads as "cannot say" and never as agreement:
    /// a fleet where one node is silent about its version is exactly the case the
    /// drift check must not call uniform.
    pub version: Option<String>,
    /// Which binary reported the version, from `/perf`. A node runs several and
    /// "the node is on 1.10.0" has never been one fact.
    pub binary: Option<String>,
    /// The hash of the build actually running, from `/perf`.
    ///
    /// This is what the version cannot say. Rust is not byte-reproducible, so
    /// one tag built twice gives two binaries reporting one version: on
    /// 2026-09-20 staging ran one build of `v1.10.0` and production another, and
    /// every reading available to an operator said the fleet agreed. `None` on a
    /// node too old to say, which counts as drift rather than agreement for the
    /// same reason the version does.
    pub hash: Option<String>,
    pub uptime_s: Option<u64>,
    pub host_uptime_s: Option<u64>,
    pub requests_total: Option<u64>,
    pub hit_rate: Option<f64>,
    pub hit_p50_ns: Option<u64>,
    pub hit_p99_ns: Option<u64>,
    /// How many samples the percentiles above were taken over.
    ///
    /// Carried because `hit_p50_ns` cannot be read without it. The number is
    /// **load-dependent** -- `docs/PERFORMANCE.md` §4, "Cache-hit latency, and
    /// why it is load dependent": the same binary on the
    /// same node reads 3,900ns over a window of 50-70 hits and 1,064ns over
    /// ~1,200, because the cache-hit path goes cold between requests on a
    /// near-idle VM. A percentile with no count beside it looks like a
    /// regression whenever traffic is quiet, and a p50 over one sample reads
    /// exactly like a p50 over a thousand.
    pub hit_samples: Option<usize>,
    pub backend_errors: Option<u64>,
    pub pools: Vec<(String, usize, usize)>,
    pub load_one: Option<f64>,
    pub cpus: Option<usize>,
    pub memory_used: Option<f64>,
    pub memory_total_bytes: Option<u64>,
    pub disk_used: Option<f64>,
    pub disk_total_bytes: Option<u64>,
    pub thermal_max_c: Option<f32>,
    /// Seconds left on the soonest-expiring certificate the node is serving,
    /// from `/perf`.
    ///
    /// The soonest rather than the leaf's, because a chain is only good until
    /// its first expiry and an operator wants one number per node. Negative on
    /// a certificate that has already expired. `None` when the node reported no
    /// certificate at all, which reads as "cannot say" and never as "plenty of
    /// time": a node silent about its expiry is the exact case this check must
    /// not call healthy, for the reason `version` and `hash` give above.
    pub cert_expires_in_seconds: Option<i64>,
    /// `notAfter` of the soonest-expiring certificate the node is serving, as
    /// an absolute time.
    ///
    /// Carried beside the countdown above because the two answer different
    /// questions. A countdown is what one node has left, and it is measured
    /// against the moment that node was polled, so two nodes serving one
    /// certificate report figures that differ by the seconds between their
    /// polls. An absolute `notAfter` is a property of the certificate, so
    /// comparing it ACROSS nodes is exact, and that comparison is the only way
    /// to see a fleet whose nodes have stopped agreeing about what they serve.
    pub cert_not_after_unix: Option<i64>,
    pub perf_error: Option<String>,
}

fn pct(v: f64) -> String {
    format!("{:.0}%", v * 100.0)
}

pub fn build(readings: &[NodeReading], t: &Thresholds, now: String) -> Digest {
    let mut findings = Vec::new();
    let mut nodes = Vec::new();

    for r in readings {
        let mut d = NodeDigest {
            name: r.name.clone(),
            role: r.role.clone(),
            url: r.url.clone(),
            status: r.status().to_string(),
            reported_node: r.health.as_ref().map(|h| h.node.clone()),
            rtt_ms: r.rtt.map(|d| d.as_secs_f64() * 1000.0),
            version: None,
            binary: None,
            hash: None,
            uptime_s: None,
            host_uptime_s: None,
            requests_total: None,
            hit_rate: None,
            hit_p50_ns: None,
            hit_p99_ns: None,
            hit_samples: None,
            backend_errors: None,
            pools: Vec::new(),
            load_one: None,
            cpus: None,
            memory_used: None,
            memory_total_bytes: None,
            disk_used: None,
            disk_total_bytes: None,
            thermal_max_c: None,
            cert_expires_in_seconds: None,
            cert_not_after_unix: None,
            perf_error: r.perf_error.clone(),
        };

        if !r.is_up() {
            let why = r.unreachable.as_deref().unwrap_or("no reason given");
            // Naming the address matters: "edge-b is unreachable" and "edge-b is
            // unreachable at 192.0.2.5" are different problems, and the second
            // one is the one where the fleet config is what is wrong.
            findings.push(Finding {
                level: Level::Fault,
                node: r.name.clone(),
                text: format!("unreachable at {}: {why}", r.url),
            });
            nodes.push(d);
            continue;
        }

        if r.status() == "degraded" {
            findings.push(Finding {
                level: Level::Fault,
                node: r.name.clone(),
                text: "reports degraded: a configured socket pool has no live member".to_string(),
            });
        }

        if let Some(p) = &r.perf {
            // Empty rather than absent on a node older than the field, so each
            // part is reported as unknown instead of as an empty string. The
            // three are taken separately on purpose: a node can legitimately
            // know its version and not its hash, because the hash is read from
            // the executable at startup and confinement can refuse that.
            let empty_to_none = |s: &String| {
                if s.is_empty() {
                    None
                } else {
                    Some(s.clone())
                }
            };
            d.version = empty_to_none(&p.build.version);
            d.binary = empty_to_none(&p.build.name);
            d.hash = empty_to_none(&p.build.hash);
            d.uptime_s = Some(p.uptime_s);
            // The soonest expiry in the chain. `min` over an empty list is
            // `None`, which is the answer for a node too old to report the
            // field and is left to say "cannot say" rather than collapsing to
            // a number.
            // The soonest expiry, but ONLY when the whole chain was readable.
            // `min` over what parsed would report an intermediate's multi-year
            // expiry as the node's figure while the leaf is unaccounted for,
            // and every threshold below would then pass. An incomplete chain
            // is "cannot say", which is the branch that warns.
            d.cert_expires_in_seconds = if p.tls_unreadable == 0 {
                p.tls.iter().map(|c| c.expires_in_seconds).min()
            } else {
                None
            };
            // The same certificate as the countdown above, by the same rule: the
            // soonest in the chain, and only when the whole chain was readable.
            // Both derived here from one reading so the fleet comparison and the
            // per-node threshold can never be talking about different
            // certificates.
            d.cert_not_after_unix = if p.tls_unreadable == 0 {
                p.tls.iter().map(|c| c.not_after_unix).min()
            } else {
                None
            };
            d.requests_total = Some(p.metrics.requests_total);
            d.backend_errors = Some(p.metrics.backend_errors_total);
            let hits = p.metrics.cache_hits_total;
            let misses = p.metrics.cache_misses_total;
            if hits + misses > 0 {
                d.hit_rate = Some(hits as f64 / (hits + misses) as f64);
            }
            // Keyed off the SAMPLE COUNT, not off the percentile being non-zero.
            //
            // Both express "not measured" rather than "zero nanoseconds", and
            // this reading was correct even when it was reporting nothing: until
            // 2026-09-15 m6-http's `/perf` cleared the reservoir these come from
            // every ten seconds, so a node taking a couple of requests a minute
            // served `hit_samples: 0` almost every scrape and the whole fleet
            // digest carried `null` latency. The monitor was honest and the
            // endpoint was not. Fixed in m6-http's `stats.rs`.
            //
            // The count is the authoritative signal, so use it directly: a
            // percentile is absent exactly when nothing was sampled.
            let sampled = p.metrics.hit_samples > 0;
            d.hit_samples = Some(p.metrics.hit_samples);
            d.hit_p50_ns = sampled.then_some(p.metrics.hit_p50_ns);
            d.hit_p99_ns = sampled.then_some(p.metrics.hit_p99_ns);
            d.pools = p
                .pools
                .iter()
                .map(|x| (x.name.clone(), x.active, x.total))
                .collect();

            for pool in &p.pools {
                if pool.active == 0 {
                    findings.push(Finding {
                        level: Level::Fault,
                        node: r.name.clone(),
                        text: format!("pool {} has 0 of {} members live", pool.name, pool.total),
                    });
                }
            }
            if p.metrics.backend_errors_total > 0 {
                // NAME the backend. "3 backend errors since start" left the
                // operator to guess which service, and the guess that matters is
                // render-contact: a submission whose SMTP send fails returns 500
                // and is counted nowhere else, so an anonymous total is the
                // difference between seeing silent mail loss and not.
                //
                // Falls back to the bare total against a node older than 1.3.0,
                // whose /perf carries no attribution, rather than reporting nothing.
                let text = if p.metrics.backend_errors_by_name.is_empty() {
                    format!(
                        "{} backend errors since start (node too old to attribute them)",
                        p.metrics.backend_errors_total
                    )
                } else {
                    let mut by: Vec<(&String, &u64)> =
                        p.metrics.backend_errors_by_name.iter().collect();
                    // Worst first: the operator reads the first line.
                    by.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
                    let named = by
                        .iter()
                        .map(|(n, c)| format!("{n} {c}"))
                        .collect::<Vec<_>>()
                        .join(", ");
                    format!(
                        "{} backend errors since start: {named}",
                        p.metrics.backend_errors_total
                    )
                };
                findings.push(Finding {
                    level: Level::Warn,
                    node: r.name.clone(),
                    text,
                });
            }

            let h = &p.host;
            d.cpus = Some(h.cpus);
            d.host_uptime_s = h.uptime_s;
            if let Some(l) = &h.load {
                d.load_one = Some(l.one);
                let per_cpu = l.one / h.cpus.max(1) as f64;
                if per_cpu >= t.load_per_cpu {
                    findings.push(Finding {
                        level: Level::Warn,
                        node: r.name.clone(),
                        text: format!(
                            "load {:.2} across {} cpu ({:.2} per cpu)",
                            l.one, h.cpus, per_cpu
                        ),
                    });
                }
            }
            if let Some(m) = &h.memory {
                d.memory_used = Some(m.used_fraction());
                d.memory_total_bytes = Some(m.total_bytes);
                if m.used_fraction() >= t.memory_used {
                    findings.push(Finding {
                        level: Level::Warn,
                        node: r.name.clone(),
                        text: format!("memory {} used", pct(m.used_fraction())),
                    });
                }
            }
            if let Some(disk) = &h.disk {
                d.disk_used = Some(disk.used_fraction());
                d.disk_total_bytes = Some(disk.total_bytes);
                if disk.used_fraction() >= t.disk_used {
                    findings.push(Finding {
                        level: Level::Warn,
                        node: r.name.clone(),
                        text: format!("disk {} used", pct(disk.used_fraction())),
                    });
                }
            }
            let hottest = h
                .thermal
                .iter()
                .max_by(|a, b| a.celsius.total_cmp(&b.celsius));
            if let Some(z) = hottest {
                d.thermal_max_c = Some(z.celsius);
                if z.celsius >= t.thermal_celsius {
                    findings.push(Finding {
                        level: Level::Warn,
                        node: r.name.clone(),
                        text: format!("{} at {:.1}C", z.name, z.celsius),
                    });
                }
            }

            // ── The served certificate ───────────────────────────────────────
            //
            // The failure this exists to catch is a silent one: renewal is done
            // by a client outside m6 on a timer, and a timer that fails keeps
            // firing and keeps failing. Nothing looks, so the first signal is
            // the node refusing connections on the day the certificate expires.
            //
            // Gated on the polled URL being TLS rather than on the node
            // reporting a certificate, so the two silences stay apart: a
            // plaintext node has nothing to report and must not warn, and a TLS
            // node that reports nothing is a node this check cannot see and
            // must warn. Reading an absence as healthy is the shape of mistake
            // the build-drift check above was written to stop making.
            // Either signal means a certificate is expected. Keying on the
            // polled URL alone silences the check on a node polled over the
            // backbone in plaintext, which `fleet.rs` actively recommends
            // ("point this at the backbone address, not the public one") and
            // which m6-http supports through `h2c_bind`. Such a node serves
            // TLS publicly and reports its chain, so a reported chain counts
            // as expecting one.
            let expects_tls = r
                .url
                .split_once("://")
                .is_some_and(|(scheme, _)| scheme.eq_ignore_ascii_case("https"))
                || !p.tls.is_empty()
                || p.tls_unreadable > 0;
            match (expects_tls, d.cert_expires_in_seconds) {
                (true, Some(secs)) if secs < 0 => {
                    findings.push(Finding {
                        level: Level::Fault,
                        node: r.name.clone(),
                        text: format!(
                            "certificate EXPIRED {} days ago and is still being served",
                            -secs / 86_400
                        ),
                    });
                }
                (true, Some(secs)) if secs / 86_400 < t.cert_expiry_days => {
                    findings.push(Finding {
                        level: Level::Warn,
                        node: r.name.clone(),
                        text: format!(
                            "certificate expires in {} days, under the {} day threshold",
                            secs / 86_400,
                            t.cert_expiry_days
                        ),
                    });
                }
                (true, None) => {
                    findings.push(Finding {
                        level: Level::Warn,
                        node: r.name.clone(),
                        // Two causes and the same consequence, so the text
                        // names the consequence: the node predates the field,
                        // or it loaded a certificate whose notAfter m6-http
                        // could not read and said so in its own log.
                        text: "node reports no certificate expiry, so a failed renewal \
                               cannot be seen from here"
                            .to_string(),
                    });
                }
                _ => {}
            }

            // A node whose own name disagrees with the fleet config is either
            // mislabelled here or is not the box we think we are polling.
            if let (Some(h), false) = (&d.reported_node, r.name.is_empty()) {
                if !h.is_empty() && !h.eq_ignore_ascii_case(&r.name) && h != &p.node {
                    findings.push(Finding {
                        level: Level::Warn,
                        node: r.name.clone(),
                        text: format!("node calls itself {h:?}"),
                    });
                }
            }
        } else if r.perf_error.is_some() {
            findings.push(Finding {
                level: Level::Warn,
                node: r.name.clone(),
                text: format!(
                    "no /perf: {}. Health is known, everything else is not.",
                    r.perf_error.as_deref().unwrap_or("")
                ),
            });
        }

        nodes.push(d);
    }

    // ── the fleet runs one release, or it does not ───────────────────────────
    //
    // A per-node version is only half the answer. The question an operator has is
    // "is this fleet uniform", and answering it per node means comparing one
    // line per node by eye and being right every time.
    //
    // Drift here is not cosmetic. It means requests are being served by different
    // code depending on which region answered, so a defect reproduces in one
    // region and not another, and a measurement means different things per node.
    // A fleet can run a distinct m6-http binary per node unnoticed for as long
    // as nobody looks, because until this nothing compared them.
    //
    // A node that cannot say counts as drift rather than as agreement: two nodes
    // agreeing while the third is silent is not a uniform fleet, it is an unknown
    // one, and reporting it as uniform is the failure mode this whole change
    // exists to remove.
    // uptime_s is set exactly when /perf was read, so it is the marker for "this
    // node answered" without needing a second flag that could disagree with it.
    let reporting: Vec<&NodeDigest> = nodes.iter().filter(|n| n.uptime_s.is_some()).collect();
    if reporting.len() > 1 {
        let mut seen: Vec<&str> = reporting
            .iter()
            .map(|n| n.version.as_deref().unwrap_or("unknown"))
            .collect();
        seen.sort_unstable();
        seen.dedup();
        if seen.len() > 1 {
            let detail = reporting
                .iter()
                .map(|n| format!("{} {}", n.name, n.version.as_deref().unwrap_or("unknown")))
                .collect::<Vec<_>>()
                .join(", ");
            findings.push(Finding {
                level: Level::Warn,
                node: "fleet".to_string(),
                text: format!(
                    "m6 version drift across the fleet: {detail}.                      Requests are served by different code depending on the region."
                ),
            });
        }

        // ── one version is not one binary ────────────────────────────────────
        //
        // Checked SEPARATELY from the version above, and reported separately,
        // because the two are different faults with different causes and the
        // combined message would name neither.
        //
        // Version drift means somebody deployed different releases. Build drift
        // means one release was BUILT TWICE and the fleet holds both: same
        // source, same tag, different bytes, because Rust is not
        // byte-reproducible. That is not a hypothetical and it is not rare. It
        // happened on 2026-09-20, it was invisible to every reading an operator
        // had, and it took `md5sum` on four machines to find. The whole reason
        // the hash is on the wire is so that this check can exist.
        //
        // Reported only when the versions agree. When they do not, the version
        // finding above already says the fleet is not uniform and the hashes
        // differing is a consequence of it, not a second fault.
        let versions_agree = seen.len() == 1;
        let mut hashes: Vec<&str> = reporting
            .iter()
            .map(|n| n.hash.as_deref().unwrap_or("unknown"))
            .collect();
        hashes.sort_unstable();
        hashes.dedup();
        if versions_agree && hashes.len() > 1 {
            let detail = reporting
                .iter()
                .map(|n| {
                    // Twelve characters, which is what an operator reading a
                    // handover or an estate file is looking at. The full value
                    // is in the JSON digest for anything that wants to compare
                    // exactly.
                    let h = n.hash.as_deref().unwrap_or("unknown");
                    format!("{} {}", n.name, &h[..h.len().min(12)])
                })
                .collect::<Vec<_>>()
                .join(", ");
            findings.push(Finding {
                level: Level::Warn,
                node: "fleet".to_string(),
                text: format!(
                    "build drift across the fleet, all reporting the same version: {detail}. \
                     One release built more than once, so the fleet is running different \
                     binaries from identical source. The version cannot see this."
                ),
            });
        }

        // ── the fleet serves one certificate, or it does not ─────────────────
        //
        // A fleet where every node serves the same names from one certificate
        // has one renewal and one expiry. When a node is left behind by a
        // renewal it keeps serving the superseded certificate, which is well
        // formed, valid, trusted by every client, and expires sooner than the
        // one the operator installed. Nothing in a per-node reading can see
        // that: each node reports a certificate with time left on it, every
        // threshold passes, and the fleet is reported healthy right up to the
        // day the stale node starts refusing connections.
        //
        // So it is checked the same way version drift is: by comparing the
        // nodes to each other rather than each node to a limit. A renewal moves
        // `notAfter` forward, so a node that missed one disagrees here on the
        // day the renewal happened, which is the day there is time to fix it.
        //
        // Only nodes that reported a certificate are compared. A node silent
        // about its chain is already a finding of its own above, and counting
        // it again here would report one silence as two faults and name the
        // cause in neither.
        let certified: Vec<&NodeDigest> = reporting
            .iter()
            .copied()
            .filter(|n| n.cert_not_after_unix.is_some())
            .collect();
        let mut expiries: Vec<i64> = certified
            .iter()
            .filter_map(|n| n.cert_not_after_unix)
            .collect();
        expiries.sort_unstable();
        expiries.dedup();
        if certified.len() > 1 && expiries.len() > 1 {
            let detail = certified
                .iter()
                .map(|n| {
                    // Days, because that is the unit an operator decides in,
                    // and the exact `notAfter` is in the JSON digest for
                    // anything comparing precisely.
                    match n.cert_expires_in_seconds {
                        Some(s) => format!("{} {}d", n.name, s / 86_400),
                        None => format!("{} unknown", n.name),
                    }
                })
                .collect::<Vec<_>>()
                .join(", ");
            findings.push(Finding {
                level: Level::Warn,
                node: "fleet".to_string(),
                text: format!(
                    "the fleet is serving {} different certificates: {detail}. A node left \
                     behind by a renewal serves a valid certificate that expires sooner \
                     than the one that was installed, and no per-node threshold can see it.",
                    expiries.len()
                ),
            });
        }
    }

    findings.sort_by_key(|f| std::cmp::Reverse(f.level));
    let level = findings.iter().map(|f| f.level).max().unwrap_or(Level::Ok);
    Digest {
        generated_at: now,
        nodes,
        findings,
        level,
        thresholds: t.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use m6_core::host::{Disk, HostSnapshot, LoadAverage, Memory, MemorySource};
    use m6_core::monitoring::{HealthReport, PerfReport, PoolHealth};
    use m6_core::telemetry::StatsSnapshot;

    fn perf(host: HostSnapshot, pools: Vec<PoolHealth>) -> PerfReport {
        PerfReport {
            node: "origin".into(),
            build: m6_core::monitoring::BuildId {
                name: "m6-http".into(),
                version: "1.4.0".into(),
                hash: "0".repeat(32),
            },
            uptime_s: 100,
            pools,
            url_backends: vec![],
            tls: vec![],
            tls_unreadable: 0,
            metrics: StatsSnapshot::default(),
            host,
        }
    }

    /// A reading from a node polled over TLS, reporting a chain whose soonest
    /// certificate has `days` left. `days` may be negative, for a certificate
    /// that has already expired and is still being served.
    ///
    /// The URL is what decides whether a certificate is expected, so this is
    /// the only helper here that uses an `https://` one. Every other test in
    /// this module polls `http://x` and therefore never reaches the check.
    /// The instant the generated chains are measured against.
    ///
    /// A fixed base so `not_after_unix` and `expires_in_seconds` in a generated
    /// chain describe the same certificate. They used to be independent, with
    /// `not_after_unix` left at 0 on every entry, and a fleet comparison over
    /// absolute expiries then sees one value whatever the nodes are serving.
    const CHAIN_BASE_UNIX: i64 = 1_789_000_000;

    fn tls_reading(name: &str, days: Option<i64>) -> NodeReading {
        let mut p = perf(plain_host(), vec![]);
        p.tls = days
            .map(|d| {
                let expires_in_seconds = d * 86_400;
                vec![
                    // Leaf, and an intermediate with far longer left: the
                    // reported number must be the soonest, not the first.
                    m6_core::tls::TlsCertificate {
                        depth: 0,
                        not_after_unix: CHAIN_BASE_UNIX + expires_in_seconds,
                        expires_in_seconds,
                    },
                    m6_core::tls::TlsCertificate {
                        depth: 1,
                        not_after_unix: CHAIN_BASE_UNIX + expires_in_seconds + 10_000 * 86_400,
                        expires_in_seconds: expires_in_seconds + 10_000 * 86_400,
                    },
                ]
            })
            .unwrap_or_default();
        let mut r = reading(name, "ok", Some(p));
        r.url = "https://x".into();
        r
    }

    fn reading(name: &str, status: &str, perf: Option<PerfReport>) -> NodeReading {
        NodeReading {
            name: name.into(),
            role: "origin".into(),
            url: "http://origin.example.com".into(),
            health: Some(HealthReport {
                status: status.into(),
                node: "origin".into(),
            }),
            health_status: Some(if status == "degraded" { 503 } else { 200 }),
            perf,
            perf_error: None,
            traffic: None,
            traffic_error: None,
            header_checks: Vec::new(),
            rtt: Some(std::time::Duration::from_millis(3)),
            unreachable: None,
        }
    }

    fn now() -> String {
        "2026-09-11T07:00:00Z".to_string()
    }

    /// A host snapshot with nothing interesting in it, for tests about other things.
    fn plain_host() -> HostSnapshot {
        HostSnapshot {
            cpus: 1,
            uptime_s: Some(1000),
            ..Default::default()
        }
    }

    fn perf_at(version: &str) -> PerfReport {
        let mut p = perf(plain_host(), vec![]);
        p.build.version = version.into();
        p
    }

    /// Same version, a build hash of the caller's choosing. This is the shape
    /// the 2026-09-20 drift had and the shape a version comparison cannot see.
    fn perf_built(version: &str, hash: &str) -> PerfReport {
        let mut p = perf(plain_host(), vec![]);
        p.build.version = version.into();
        p.build.hash = hash.into();
        p
    }

    /// Two nodes on different releases is a warning, and it names both.
    ///
    /// A fleet can run a distinct m6-http binary per node for as long as
    /// nobody looks, because until this nothing compared them.
    #[test]
    fn version_drift_across_the_fleet_is_reported() {
        let d = build(
            &[
                reading("origin", "ok", Some(perf_at("1.4.0"))),
                reading("edge-a", "ok", Some(perf_at("1.3.0"))),
            ],
            &Thresholds::default(),
            now(),
        );
        let drift: Vec<_> = d
            .findings
            .iter()
            .filter(|f| f.text.contains("version drift"))
            .collect();
        assert_eq!(drift.len(), 1, "{:?}", d.findings);
        assert_eq!(drift[0].level, Level::Warn);
        assert!(drift[0].text.contains("origin 1.4.0"), "{}", drift[0].text);
        assert!(drift[0].text.contains("edge-a 1.3.0"), "{}", drift[0].text);
    }

    /// **The case this whole change exists for.** Same version on every node,
    /// different builds, which is what a fleet looks like when one release has
    /// been built twice. Rust is not byte-reproducible, so this is the ordinary
    /// consequence of rebuilding rather than promoting.
    ///
    /// On 2026-09-20 staging ran one build of `v1.10.0` and production another.
    /// Every reading an operator had said the fleet agreed, and it was found by
    /// running `md5sum` on four machines. This test is the thing that would have
    /// said so.
    ///
    /// Verified red before being trusted, per the standing rule: with the hash
    /// comparison removed it reports nothing at all, because the versions match.
    #[test]
    fn one_version_across_two_builds_is_reported_as_build_drift() {
        let d = build(
            &[
                reading("origin", "ok", Some(perf_built("1.10.0", &"a".repeat(32)))),
                reading("edge-a", "ok", Some(perf_built("1.10.0", &"b".repeat(32)))),
            ],
            &Thresholds::default(),
            now(),
        );

        assert!(
            !d.findings.iter().any(|f| f.text.contains("version drift")),
            "the versions agree, so the VERSION finding must stay quiet: {:?}",
            d.findings
        );

        let drift: Vec<_> = d
            .findings
            .iter()
            .filter(|f| f.text.contains("build drift"))
            .collect();
        assert_eq!(drift.len(), 1, "{:?}", d.findings);
        assert_eq!(drift[0].level, Level::Warn);
        assert!(
            drift[0].text.contains("origin aaaaaaaaaaaa"),
            "{}",
            drift[0].text
        );
        assert!(
            drift[0].text.contains("edge-a bbbbbbbbbbbb"),
            "{}",
            drift[0].text
        );
    }

    /// A node too old to report a hash counts as drift, not as agreement. Two
    /// nodes agreeing while a third is silent is an unknown fleet, not a uniform
    /// one, and the version check already takes this position.
    #[test]
    fn a_node_that_cannot_say_its_build_is_drift_not_agreement() {
        let d = build(
            &[
                reading("origin", "ok", Some(perf_built("1.10.0", &"a".repeat(32)))),
                // Empty hash: a node older than the field, or one whose
                // confinement refused it the read.
                reading("edge-a", "ok", Some(perf_built("1.10.0", ""))),
            ],
            &Thresholds::default(),
            now(),
        );
        assert!(
            d.findings.iter().any(|f| f.text.contains("build drift")),
            "a silent node must not be read as agreement: {:?}",
            d.findings
        );
    }

    /// Version drift is reported ONCE, not twice. Different releases have
    /// different binaries by construction, so reporting build drift beside it
    /// would name a consequence as a second fault and tell an operator to look
    /// in two places for one problem.
    #[test]
    fn version_drift_does_not_also_report_build_drift() {
        let d = build(
            &[
                reading("origin", "ok", Some(perf_built("1.10.0", &"a".repeat(32)))),
                reading("edge-a", "ok", Some(perf_built("1.9.0", &"b".repeat(32)))),
            ],
            &Thresholds::default(),
            now(),
        );
        assert_eq!(
            d.findings
                .iter()
                .filter(|f| f.text.contains("version drift"))
                .count(),
            1,
            "{:?}",
            d.findings
        );
        assert!(
            !d.findings.iter().any(|f| f.text.contains("build drift")),
            "the hashes differ because the versions do; that is one fault: {:?}",
            d.findings
        );
    }

    /// A uniform fleet says nothing, which is the whole point of the check being
    /// quiet when there is nothing to say.
    #[test]
    fn a_uniform_fleet_reports_no_drift() {
        let d = build(
            &[
                reading("origin", "ok", Some(perf_at("1.4.0"))),
                reading("edge-a", "ok", Some(perf_at("1.4.0"))),
            ],
            &Thresholds::default(),
            now(),
        );
        assert!(
            !d.findings.iter().any(|f| f.text.contains("version drift")),
            "{:?}",
            d.findings
        );
    }

    /// A node that cannot say its version counts as drift, not as agreement.
    ///
    /// This is the case the change exists for: reporting "uniform" because the
    /// silent node was skipped is worse than reporting nothing, because it is a
    /// claim rather than a gap.
    #[test]
    fn a_node_that_cannot_report_its_version_is_not_agreement() {
        let d = build(
            &[
                reading("origin", "ok", Some(perf_at("1.4.0"))),
                // Empty version: a node older than the field.
                reading("edge-a", "ok", Some(perf_at(""))),
            ],
            &Thresholds::default(),
            now(),
        );
        let drift: Vec<_> = d
            .findings
            .iter()
            .filter(|f| f.text.contains("version drift"))
            .collect();
        assert_eq!(drift.len(), 1, "{:?}", d.findings);
        assert!(
            drift[0].text.contains("edge-a unknown"),
            "{}",
            drift[0].text
        );
    }

    /// One node cannot drift from itself. A single-node fleet on an unknown
    /// version is not a drift finding, it is simply a fleet of one.
    #[test]
    fn a_single_node_never_drifts() {
        let d = build(
            &[reading("origin", "ok", Some(perf_at("")))],
            &Thresholds::default(),
            now(),
        );
        assert!(
            !d.findings.iter().any(|f| f.text.contains("version drift")),
            "{:?}",
            d.findings
        );
    }

    #[test]
    fn a_healthy_fleet_has_no_findings() {
        let host = HostSnapshot {
            cpus: 2,
            load: Some(LoadAverage {
                one: 0.13,
                five: 0.1,
                fifteen: 0.09,
                running: 1,
                total: 231,
            }),
            memory: Some(Memory {
                total_bytes: 1_000_000,
                available_bytes: 700_000,
                source: MemorySource::Host,
            }),
            disk: Some(Disk {
                total_bytes: 1000,
                available_bytes: 600,
            }),
            thermal: vec![],
            uptime_s: Some(694_521),
            ..Default::default()
        };
        let d = build(
            &[reading("origin", "ok", Some(perf(host, vec![])))],
            &Thresholds::default(),
            now(),
        );
        assert_eq!(d.level, Level::Ok);
        assert!(d.findings.is_empty(), "{:?}", d.findings);
        assert_eq!(d.nodes[0].cpus, Some(2));
        assert!((d.nodes[0].disk_used.unwrap() - 0.4).abs() < 1e-9);
    }

    /// Load is a queue length. 3.0 on a 4 core box is fine and on a 1 core box
    /// is not, so the threshold has to be per CPU or it is meaningless.
    #[test]
    fn load_is_judged_per_cpu() {
        let mk = |cpus: usize| HostSnapshot {
            cpus,
            load: Some(LoadAverage {
                one: 3.0,
                five: 3.0,
                fifteen: 3.0,
                running: 3,
                total: 100,
            }),
            ..Default::default()
        };
        let t = Thresholds::default();
        let quiet = build(&[reading("a", "ok", Some(perf(mk(4), vec![])))], &t, now());
        assert!(
            quiet.findings.is_empty(),
            "3.0 across 4 cpu is not a warning"
        );
        let busy = build(&[reading("b", "ok", Some(perf(mk(1), vec![])))], &t, now());
        assert_eq!(busy.level, Level::Warn);
        assert!(busy.findings[0].text.contains("per cpu"));
    }

    #[test]
    fn an_empty_pool_is_a_fault_and_so_is_the_degraded_verdict() {
        let pools = vec![
            PoolHealth {
                name: "m6-html".into(),
                active: 1,
                total: 1,
            },
            PoolHealth {
                name: "render-contact".into(),
                active: 0,
                total: 2,
            },
        ];
        let d = build(
            &[reading(
                "origin",
                "degraded",
                Some(perf(HostSnapshot::default(), pools)),
            )],
            &Thresholds::default(),
            now(),
        );
        assert_eq!(d.level, Level::Fault);
        assert_eq!(d.findings.len(), 2);
        assert!(d.findings.iter().any(|f| f.text.contains("render-contact")));
    }

    /// Backend errors name the backend, worst first.
    ///
    /// The message content IS the feature. "3 backend errors since start" was
    /// already reported; what it could not say is WHICH service, and the service
    /// that matters is the one that sends mail. A contact submission whose SMTP
    /// send fails returns 500 and is counted nowhere else, so an anonymous total is
    /// the difference between seeing silent mail loss and not.
    #[test]
    fn backend_errors_name_the_backend_worst_first() {
        let mut p = perf(HostSnapshot::default(), vec![]);
        p.metrics.backend_errors_total = 7;
        p.metrics.backend_errors_by_name = [
            ("m6-html".to_string(), 2u64),
            ("render-contact".to_string(), 5u64),
        ]
        .into_iter()
        .collect();
        let d = build(
            &[reading("origin", "ok", Some(p))],
            &Thresholds::default(),
            now(),
        );
        let f = d
            .findings
            .iter()
            .find(|f| f.text.contains("backend errors"))
            .expect("a backend-error finding");
        assert!(f.text.contains("render-contact 5"), "{}", f.text);
        assert!(f.text.contains("m6-html 2"), "{}", f.text);
        // Worst first, because the operator reads the first name.
        let (a, b) = (
            f.text.find("render-contact").unwrap(),
            f.text.find("m6-html").unwrap(),
        );
        assert!(a < b, "worst backend must come first: {}", f.text);
    }

    /// An older node sends no attribution. Say so rather than reporting nothing,
    /// and rather than implying the errors did not happen.
    #[test]
    fn an_unattributed_total_still_reports_and_says_why() {
        let mut p = perf(HostSnapshot::default(), vec![]);
        p.metrics.backend_errors_total = 3;
        p.metrics.backend_errors_by_name.clear();
        let d = build(
            &[reading("origin", "ok", Some(p))],
            &Thresholds::default(),
            now(),
        );
        let f = d
            .findings
            .iter()
            .find(|f| f.text.contains("backend errors"))
            .expect("a backend-error finding");
        assert!(f.text.contains('3'), "{}", f.text);
        assert!(f.text.contains("too old"), "{}", f.text);
    }

    /// A node that did not answer must not be reported as healthy, and must
    /// not be quietly missing from the report either.
    #[test]
    fn an_unreachable_node_is_a_fault_and_still_appears() {
        let mut r = reading("edge-b", "ok", None);
        r.unreachable = Some("connection refused".into());
        let d = build(&[r], &Thresholds::default(), now());
        assert_eq!(d.level, Level::Fault);
        assert_eq!(d.nodes.len(), 1);
        assert_eq!(d.nodes[0].status, "unreachable");
    }

    /// A node we can reach but cannot read /perf from is a warning, not a
    /// silent blank: "no numbers" and "good numbers" must not look alike.
    #[test]
    fn a_node_without_perf_says_so() {
        let mut r = reading("edge-a", "ok", None);
        r.perf_error = Some("401: token rejected by this node".into());
        let d = build(&[r], &Thresholds::default(), now());
        assert_eq!(d.level, Level::Warn);
        assert!(d.findings[0].text.contains("401"));
        assert!(d.nodes[0].hit_rate.is_none());
    }

    /// Zero nanoseconds is not a latency, it is an empty window.
    #[test]
    fn an_empty_latency_window_is_absent_not_zero() {
        let d = build(
            &[reading(
                "origin",
                "ok",
                Some(perf(HostSnapshot::default(), vec![])),
            )],
            &Thresholds::default(),
            now(),
        );
        assert_eq!(d.nodes[0].hit_p50_ns, None);
        assert_eq!(d.nodes[0].hit_rate, None);
    }

    // ── The served certificate, issue #176 ───────────────────────────────────

    /// A certificate with plenty of time left is reported and judged healthy.
    /// The number is the soonest in the chain, which is the leaf here and is
    /// deliberately not the first thing a naive read would return.
    #[test]
    fn a_healthy_certificate_is_reported_and_raises_nothing() {
        let d = build(
            &[tls_reading("origin", Some(60))],
            &Thresholds::default(),
            now(),
        );
        assert_eq!(d.nodes[0].cert_expires_in_seconds, Some(60 * 86_400));
        assert_eq!(d.level, Level::Ok);
        assert!(d.findings.is_empty(), "{:?}", d.findings);
    }

    /// Under the threshold is a warning, and the text says how long is left
    /// rather than only that something is wrong.
    #[test]
    fn a_certificate_under_the_threshold_warns() {
        let d = build(
            &[tls_reading("origin", Some(9))],
            &Thresholds::default(),
            now(),
        );
        assert_eq!(d.level, Level::Warn);
        assert!(
            d.findings[0].text.contains("expires in 9 days"),
            "{:?}",
            d.findings[0].text
        );
    }

    /// The boundary, stated so a later edit cannot move it by accident. The
    /// default threshold is 21 days, so 21 is fine and 20 warns.
    #[test]
    fn the_threshold_is_a_floor_not_a_ceiling() {
        let t = Thresholds::default();
        assert!(build(&[tls_reading("origin", Some(21))], &t, now())
            .findings
            .is_empty());
        assert!(!build(&[tls_reading("origin", Some(20))], &t, now())
            .findings
            .is_empty());
    }

    /// An expired certificate is a fault rather than a warning: the node is
    /// refusing connections, so this is not a thing to look at next week.
    #[test]
    fn an_expired_certificate_is_a_fault() {
        let d = build(
            &[tls_reading("origin", Some(-3))],
            &Thresholds::default(),
            now(),
        );
        assert_eq!(d.level, Level::Fault);
        assert!(
            d.findings[0].text.contains("EXPIRED 3 days ago"),
            "{:?}",
            d.findings[0].text
        );
        assert_eq!(d.nodes[0].cert_expires_in_seconds, Some(-3 * 86_400));
    }

    /// The case this check exists for, and the one easiest to get wrong. A TLS
    /// node that reports no certificate is a node whose renewal cannot be
    /// watched from here, and reading that absence as healthy is the whole
    /// failure mode. It warns, and the per-node line shows "-" rather than a
    /// number nobody measured.
    #[test]
    fn a_tls_node_reporting_no_certificate_warns() {
        let d = build(
            &[tls_reading("origin", None)],
            &Thresholds::default(),
            now(),
        );
        assert_eq!(d.level, Level::Warn);
        assert!(
            d.findings[0].text.contains("no certificate expiry"),
            "{:?}",
            d.findings[0].text
        );
        assert_eq!(d.nodes[0].cert_expires_in_seconds, None);
    }

    /// AN INCOMPLETE CHAIN IS "CANNOT SAY", NOT THE MINIMUM OF WHAT PARSED.
    ///
    /// The case the review found: m6-http cannot read the leaf's notAfter, so
    /// it reports only the intermediate, which is years out. Taking `min` of
    /// what arrived returns that long number, both threshold arms fall
    /// through, and the digest says ALL CLEAR while nothing in the fleet knows
    /// when the served leaf expires. That is absence read as health, which is
    /// the exact failure this check exists to prevent.
    #[test]
    fn a_chain_with_an_unreadable_member_cannot_say_rather_than_passing() {
        let mut p = perf(plain_host(), vec![]);
        p.tls = vec![m6_core::tls::TlsCertificate {
            depth: 1,
            not_after_unix: 0,
            expires_in_seconds: 900 * 86_400,
        }];
        p.tls_unreadable = 1;
        let mut r = reading("origin", "ok", Some(p));
        r.url = "https://origin.example.com".into();

        let d = build(&[r], &Thresholds::default(), now());
        assert_eq!(
            d.nodes[0].cert_expires_in_seconds, None,
            "a 900-day intermediate must not stand in for an unreadable leaf"
        );
        assert_eq!(d.level, Level::Warn);
        assert!(
            d.findings[0].text.contains("no certificate expiry"),
            "{:?}",
            d.findings[0].text
        );
    }

    /// A node polled over the backbone in plaintext still serves TLS publicly,
    /// and `fleet.rs` actively recommends pointing the poll URL at the
    /// backbone. Gating on the URL scheme alone silenced the check on exactly
    /// that node, so a reported chain counts as expecting one.
    #[test]
    fn a_plaintext_polled_node_that_reports_a_chain_is_still_checked() {
        let mut p = perf(plain_host(), vec![]);
        p.tls = vec![m6_core::tls::TlsCertificate {
            depth: 0,
            not_after_unix: 0,
            expires_in_seconds: 3 * 86_400,
        }];
        let r = reading("origin", "ok", Some(p)); // url is http://origin.example.com
        let d = build(&[r], &Thresholds::default(), now());
        assert_eq!(
            d.level,
            Level::Warn,
            "3 days left must warn even over a plaintext poll"
        );
        assert!(
            d.findings[0].text.contains("expires in 3 days"),
            "{:?}",
            d.findings[0].text
        );
    }

    /// And the other silence, which must stay silent. A node polled over
    /// plaintext has no certificate to report, so the absence is an answer and
    /// not a gap.
    #[test]
    fn a_plaintext_node_is_not_warned_about() {
        let d = build(
            &[reading("origin", "ok", Some(perf(plain_host(), vec![])))],
            &Thresholds::default(),
            now(),
        );
        assert_eq!(d.nodes[0].cert_expires_in_seconds, None);
        assert!(d.findings.is_empty(), "{:?}", d.findings);
    }

    // ── the fleet serves one certificate, or it does not ─────────────────────

    /// **The case this check exists for.** One node renews, the others keep the
    /// certificate they had. Every node reports time left, every per-node
    /// threshold passes, and before this nothing compared them.
    ///
    /// Verified red before being trusted: with the comparison removed the
    /// digest reports no finding at all and `level` is `Ok`, because 60 and 30
    /// days are both over the 21-day threshold.
    #[test]
    fn a_node_left_behind_by_a_renewal_is_reported_as_fleet_certificate_drift() {
        let d = build(
            &[
                tls_reading("origin", Some(89)),
                tls_reading("edge-a", Some(30)),
                tls_reading("edge-b", Some(30)),
            ],
            &Thresholds::default(),
            now(),
        );
        let drift: Vec<_> = d
            .findings
            .iter()
            .filter(|f| f.text.contains("different certificates"))
            .collect();
        assert_eq!(drift.len(), 1, "{:?}", d.findings);
        assert_eq!(drift[0].level, Level::Warn);
        assert_eq!(drift[0].node, "fleet");
        assert!(
            drift[0].text.contains("2 different certificates"),
            "two distinct expiries across three nodes is two certificates: {}",
            drift[0].text
        );
        for expect in ["origin 89d", "edge-a 30d", "edge-b 30d"] {
            assert!(
                drift[0].text.contains(expect),
                "the finding must name every node and what it is serving, missing {expect}: {}",
                drift[0].text
            );
        }
    }

    /// A fleet that renewed everywhere says nothing. The countdowns are
    /// measured per poll and can differ by seconds for one certificate, which
    /// is why the comparison is over `notAfter` and not over the countdown.
    #[test]
    fn a_fleet_serving_one_certificate_reports_no_drift() {
        let d = build(
            &[
                tls_reading("origin", Some(60)),
                tls_reading("edge-a", Some(60)),
                tls_reading("edge-b", Some(60)),
            ],
            &Thresholds::default(),
            now(),
        );
        assert!(
            !d.findings
                .iter()
                .any(|f| f.text.contains("different certificates")),
            "{:?}",
            d.findings
        );
        assert_eq!(d.level, Level::Ok, "{:?}", d.findings);
    }

    /// One node is not a fleet, and a fleet of one cannot disagree with itself.
    #[test]
    fn a_single_node_never_drifts_on_its_certificate() {
        let d = build(
            &[tls_reading("origin", Some(60))],
            &Thresholds::default(),
            now(),
        );
        assert!(
            !d.findings
                .iter()
                .any(|f| f.text.contains("different certificates")),
            "{:?}",
            d.findings
        );
    }

    /// A node silent about its chain is one finding, not two.
    ///
    /// It already warns per node that its renewal cannot be watched. Counting
    /// the silence as drift as well would report one cause as two faults and
    /// name it in neither, so only nodes that reported a certificate are
    /// compared.
    #[test]
    fn a_node_reporting_no_certificate_is_not_also_counted_as_drift() {
        let d = build(
            &[
                tls_reading("origin", Some(60)),
                tls_reading("edge-a", Some(60)),
                tls_reading("edge-b", None),
            ],
            &Thresholds::default(),
            now(),
        );
        assert!(
            !d.findings
                .iter()
                .any(|f| f.text.contains("different certificates")),
            "the two nodes that can say agree, so the only finding is the silent one: {:?}",
            d.findings
        );
        let silent: Vec<_> = d
            .findings
            .iter()
            .filter(|f| f.text.contains("no certificate expiry"))
            .collect();
        assert_eq!(silent.len(), 1, "{:?}", d.findings);
        assert_eq!(silent[0].node, "edge-b");
    }

    /// An expired certificate on one node is both faults at once: the node is
    /// serving something expired, and the fleet has stopped agreeing. Both are
    /// reported, because fixing the first does not tell an operator the second
    /// was ever true.
    #[test]
    fn an_expired_certificate_on_one_node_is_a_fault_and_also_drift() {
        let d = build(
            &[
                tls_reading("origin", Some(89)),
                tls_reading("edge-a", Some(-2)),
            ],
            &Thresholds::default(),
            now(),
        );
        assert_eq!(d.level, Level::Fault, "{:?}", d.findings);
        assert!(
            d.findings
                .iter()
                .any(|f| f.text.contains("EXPIRED") && f.node == "edge-a"),
            "{:?}",
            d.findings
        );
        assert!(
            d.findings
                .iter()
                .any(|f| f.text.contains("different certificates")),
            "{:?}",
            d.findings
        );
    }
}
