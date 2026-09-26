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
}

impl Default for Thresholds {
    fn default() -> Self {
        Self {
            disk_used: 0.80,
            memory_used: 0.90,
            load_per_cpu: 2.0,
            thermal_celsius: 80.0,
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
    /// **load-dependent** -- `docs/PERFORMANCE.md` §4: the same binary on the
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
    // "is this fleet uniform", and answering it per node means comparing three
    // lines by eye and being right every time.
    //
    // Drift here is not cosmetic. It means requests are being served by different
    // code depending on which region answered, so a defect reproduces in one
    // region and not another, and a measurement means different things per node.
    // On 2026-09-10 this fleet ran three distinct m6-http binaries for about
    // twenty minutes and it was not noticed, because nothing compared them.
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
            node: "sydney".into(),
            build: m6_core::monitoring::BuildId {
                name: "m6-http".into(),
                version: "1.4.0".into(),
                hash: "0".repeat(32),
            },
            uptime_s: 100,
            pools,
            url_backends: vec![],
            metrics: StatsSnapshot::default(),
            host,
        }
    }

    fn reading(name: &str, status: &str, perf: Option<PerfReport>) -> NodeReading {
        NodeReading {
            name: name.into(),
            role: "origin".into(),
            url: "http://x".into(),
            health: Some(HealthReport {
                status: status.into(),
                node: "sydney".into(),
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
    /// On 2026-09-10 this fleet ran three distinct m6-http binaries for twenty
    /// minutes without anyone noticing, because nothing compared them.
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
}
