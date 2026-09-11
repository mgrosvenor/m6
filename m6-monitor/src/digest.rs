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
    pub uptime_s: Option<u64>,
    pub host_uptime_s: Option<u64>,
    pub requests_total: Option<u64>,
    pub hit_rate: Option<f64>,
    pub hit_p50_ns: Option<u64>,
    pub hit_p99_ns: Option<u64>,
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
            uptime_s: None,
            host_uptime_s: None,
            requests_total: None,
            hit_rate: None,
            hit_p50_ns: None,
            hit_p99_ns: None,
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
            // Naming the address matters: "chi is unreachable" and "chi is
            // unreachable at 10.0.0.5" are different problems, and the second
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
                text: "reports degraded: a configured socket pool has no live member"
                    .to_string(),
            });
        }

        if let Some(p) = &r.perf {
            d.uptime_s = Some(p.uptime_s);
            d.requests_total = Some(p.metrics.requests_total);
            d.backend_errors = Some(p.metrics.backend_errors_total);
            let hits = p.metrics.cache_hits_total;
            let misses = p.metrics.cache_misses_total;
            if hits + misses > 0 {
                d.hit_rate = Some(hits as f64 / (hits + misses) as f64);
            }
            // Zero means "no sample in this window", not "zero nanoseconds".
            d.hit_p50_ns = (p.metrics.hit_p50_ns > 0).then_some(p.metrics.hit_p50_ns);
            d.hit_p99_ns = (p.metrics.hit_p99_ns > 0).then_some(p.metrics.hit_p99_ns);
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
                findings.push(Finding {
                    level: Level::Warn,
                    node: r.name.clone(),
                    text: format!("{} backend errors since start", p.metrics.backend_errors_total),
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

    findings.sort_by(|a, b| b.level.cmp(&a.level));
    let level = findings.iter().map(|f| f.level).max().unwrap_or(Level::Ok);
    Digest { generated_at: now, nodes, findings, level, thresholds: t.clone() }
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
            health: Some(HealthReport { status: status.into(), node: "sydney".into() }),
            health_status: Some(if status == "degraded" { 503 } else { 200 }),
            perf,
            perf_error: None,
            traffic: None,
            traffic_error: None,
            rtt: Some(std::time::Duration::from_millis(3)),
            unreachable: None,
        }
    }

    fn now() -> String {
        "2026-09-11T07:00:00Z".to_string()
    }

    #[test]
    fn a_healthy_fleet_has_no_findings() {
        let host = HostSnapshot {
            cpus: 2,
            load: Some(LoadAverage { one: 0.13, five: 0.1, fifteen: 0.09, running: 1, total: 231 }),
            memory: Some(Memory {
                total_bytes: 1_000_000,
                available_bytes: 700_000,
                source: MemorySource::Host,
            }),
            disk: Some(Disk { total_bytes: 1000, available_bytes: 600 }),
            thermal: vec![],
            uptime_s: Some(694_521),
        };
        let d = build(&[reading("syd", "ok", Some(perf(host, vec![])))], &Thresholds::default(), now());
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
            load: Some(LoadAverage { one: 3.0, five: 3.0, fifteen: 3.0, running: 3, total: 100 }),
            ..Default::default()
        };
        let t = Thresholds::default();
        let quiet = build(&[reading("a", "ok", Some(perf(mk(4), vec![])))], &t, now());
        assert!(quiet.findings.is_empty(), "3.0 across 4 cpu is not a warning");
        let busy = build(&[reading("b", "ok", Some(perf(mk(1), vec![])))], &t, now());
        assert_eq!(busy.level, Level::Warn);
        assert!(busy.findings[0].text.contains("per cpu"));
    }

    #[test]
    fn an_empty_pool_is_a_fault_and_so_is_the_degraded_verdict() {
        let pools = vec![
            PoolHealth { name: "m6-html".into(), active: 1, total: 1 },
            PoolHealth { name: "render-contact".into(), active: 0, total: 2 },
        ];
        let d = build(
            &[reading("syd", "degraded", Some(perf(HostSnapshot::default(), pools)))],
            &Thresholds::default(),
            now(),
        );
        assert_eq!(d.level, Level::Fault);
        assert_eq!(d.findings.len(), 2);
        assert!(d.findings.iter().any(|f| f.text.contains("render-contact")));
    }

    /// A node that did not answer must not be reported as healthy, and must
    /// not be quietly missing from the report either.
    #[test]
    fn an_unreachable_node_is_a_fault_and_still_appears() {
        let mut r = reading("chi", "ok", None);
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
        let mut r = reading("lon", "ok", None);
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
            &[reading("syd", "ok", Some(perf(HostSnapshot::default(), vec![])))],
            &Thresholds::default(),
            now(),
        );
        assert_eq!(d.nodes[0].hit_p50_ns, None);
        assert_eq!(d.nodes[0].hit_rate, None);
    }
}
