//! m6-http's side of the monitoring endpoints.
//!
//! The endpoints themselves, and the shape of what they publish, are
//! `m6_core::monitoring`. They are an m6 feature rather than this binary's:
//! every deployment serves them, and an aggregator that reads them needs the
//! same types the server writes. One definition, shared, rather than a
//! consumer re-deriving the format by looking at a sample.
//!
//! What remains here is the part that is genuinely about this binary: which
//! backend name a monitoring response is tagged with, and keeping those
//! responses out of the site traffic counters.

pub use m6_core::monitoring::{
    metrics_authorised, HealthReport, PerfOutcome, PerfReport, PoolHealth,
};

/// Backend name tagged onto a `/health` response.
pub const HEALTH_BACKEND: &str = "health";
/// Backend name tagged onto a `/perf` response.
pub const PERF_BACKEND: &str = "perf";

/// Whether a completed response came from the monitoring endpoints.
///
/// These must not be counted as site traffic. Both answer from local state
/// and return `Ready`, so the cache-miss accounting on the serving paths
/// would otherwise record every health check as a cache miss -- inflating
/// requests_total with monitor polling, depressing the hit rate, and
/// poisoning the miss latency percentiles with ~10us samples that never
/// touched a backend.
///
/// That matters most for the signal the endpoint exists to give: if a
/// 30-second monitor contributes to requests_total, then "the request count
/// stopped moving" can no longer detect a traffic stall, because the monitor
/// keeps it moving on its own.
pub fn is_monitoring_endpoint(backend: &str) -> bool {
    backend == HEALTH_BACKEND || backend == PERF_BACKEND
}

/// Compare a presented credential against the expected one without leaking
/// the match position through timing.
///
/// A naive `==` on strings returns at the first differing byte, so response
/// time reveals how many leading bytes were correct and the token can be
/// recovered one byte at a time. Lengths are compared first and unequal
/// lengths rejected outright, which does leak length; that is not
/// recoverable-secret information in the way a prefix is.

#[cfg(test)]
mod token_file_tests {
    use crate::config::HealthConfig;

    /// `--dump-config` serialises the whole Config. The resolved token must
    /// never appear in that output, or the secret ends up in any log or
    /// paste of a config dump.
    #[test]
    fn resolved_token_is_never_serialised() {
        let health = HealthConfig {
            enabled: true,
            path: "/health".to_string(),
            perf_path: "/perf".to_string(),
            traffic_path: "/traffic".to_string(),
            traffic_cache_s: 60,
            metrics_token_file: Some("/etc/m6/perf-token".to_string()),
            metrics_token: Some("super-secret-value".to_string()),
        };
        let dumped = serde_json::to_string(&health).expect("serialisable");
        assert!(
            !dumped.contains("super-secret-value"),
            "token leaked into serialised config: {dumped}"
        );
        // The path is configuration and should still be visible.
        assert!(dumped.contains("/etc/m6/perf-token"));
    }

    /// The file indirection has to be enforced, not advisory: a token written
    /// inline in site.toml must not be honoured, because site.toml is
    /// committed and shipped to every node.
    #[test]
    fn inline_token_in_toml_is_ignored() {
        let parsed: HealthConfig =
            toml::from_str("metrics_token = \"inline-secret\"").expect("parses");
        assert_eq!(
            parsed.metrics_token, None,
            "an inline token must not be accepted from config"
        );
    }
}

#[cfg(test)]
mod monitoring_exclusion_tests {
    use super::*;

    /// Regression guard. The cache-miss accounting on the serving paths fires
    /// on any `Ready` outcome, and /health and /perf both return Ready. Left
    /// uncounted for, every health check was recorded as a cache miss: 10
    /// checks moved requests_total by 11 on the live origin.
    ///
    /// That defeats the counter's purpose. A monitor polling every 30s would
    /// keep requests_total rising through a total traffic stall, so "the
    /// number stopped moving" would never fire.
    #[test]
    fn monitoring_endpoints_are_excluded_from_traffic_stats() {
        assert!(is_monitoring_endpoint(HEALTH_BACKEND));
        assert!(is_monitoring_endpoint(PERF_BACKEND));
        assert!(is_monitoring_endpoint("health"));
        assert!(is_monitoring_endpoint("perf"));
    }

    /// Real backends must still be counted. An over-broad match here would
    /// silently stop recording actual site traffic, which is a worse failure
    /// than the one above and much harder to notice.
    #[test]
    fn real_backends_are_still_counted() {
        for backend in ["cache", "m6-html", "m6-file", "render-contact",
                        "render-analytics", "origin", "method-check", ""] {
            assert!(
                !is_monitoring_endpoint(backend),
                "{backend} is real traffic and must be counted"
            );
        }
    }
}

// ── /traffic ─────────────────────────────────────────────────────────────────

use std::sync::Mutex;
use std::time::{Duration, Instant};

use m6_core::monitoring::TrafficReport;

/// Last summary and when it was built.
///
/// One node, one cache. Behind a Mutex because building a summary reads a file
/// and there is no reason to let two scrapes do it at once; the lock is held
/// across the read deliberately, so a second caller waits for the first
/// answer rather than starting a second 47MB tail.
static CACHE: Mutex<Option<(Instant, TrafficReport)>> = Mutex::new(None);

/// Where the privileged collector leaves `nft -j list ruleset` output.
const FIREWALL_JSON: &str = "/var/lib/m6/firewall.json";

/// Read the last `window_minutes` of the analytics log and summarise it.
///
/// The tail is bounded rather than reading the whole file: an hour of traffic
/// is at most a few hundred KB and the file is tens of MB, so reading it all
/// would be almost entirely wasted work. 24MB is a wide margin over the
/// busiest hour observed (370KB, during a 890-request probe).
fn build_report(node: &str, path: &str, window_minutes: u64) -> anyhow::Result<TrafficReport> {
    use std::io::{Read, Seek, SeekFrom};

    const TAIL_BYTES: u64 = 24 * 1024 * 1024;

    let mut f = std::fs::File::open(path)?;
    let len = f.metadata()?.len();
    if len > TAIL_BYTES {
        f.seek(SeekFrom::Start(len - TAIL_BYTES))?;
    }
    let mut buf = String::new();
    f.read_to_string(&mut buf)?;
    // A mid-record start is expected after a seek, and `ndjson::read_str`
    // skips what it cannot parse, so the partial first line costs nothing.

    let since = m6_core::util::iso8601_minutes_ago(window_minutes);
    let mut report = TrafficReport::build(node, &buf, &since, window_minutes);
    // Written by the m6-firewall-stats timer. A node without the timer
    // reports no firewall state rather than an empty one.
    report.firewall =
        m6_core::firewall::FirewallState::from_file(std::path::Path::new(FIREWALL_JSON))
            .unwrap_or(None);
    Ok(report)
}

/// Serve `/traffic`: this node's summary of its own traffic.
///
/// Same token as `/perf`, and the same order of operations: authorise first,
/// then work. An anonymous caller never causes a file read.
///
/// The cache is what makes a file read safe to expose at all. Without it a
/// monitor polling every thirty seconds makes every node re-read its analytics
/// tail every thirty seconds, forever, to produce an answer that changes
/// slowly. With it the read happens at most once per TTL however often the
/// endpoint is scraped. The lock is held across the read deliberately: a
/// second caller arriving mid-read waits for the first answer rather than
/// starting a second tail.
pub fn traffic(
    node: &str,
    log_path: &str,
    window_minutes: u64,
    cache_for: Duration,
    headers: &[(String, String)],
    configured_token: Option<&str>,
) -> PerfOutcome {
    match configured_token.filter(|t| !t.is_empty()) {
        None => return PerfOutcome::Disabled,
        Some(_) if !metrics_authorised(headers, configured_token) => {
            return PerfOutcome::Unauthorised
        }
        Some(_) => {}
    }

    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((built, report)) = cache.as_ref() {
        if built.elapsed() < cache_for {
            return PerfOutcome::Traffic(report.clone());
        }
    }
    match build_report(node, log_path, window_minutes) {
        Ok(r) => {
            *cache = Some((Instant::now(), r.clone()));
            PerfOutcome::Traffic(r)
        }
        Err(e) => PerfOutcome::TrafficError(format!("{e}")),
    }
}

#[cfg(test)]
mod traffic_endpoint_tests {
    use super::*;
    use std::io::Write;

    fn row(ts: &str, ip: &str, path: &str, status: u16, ua: &str) -> String {
        format!(
            r#"{{"timestamp":"{ts}","level":"INFO","fields":{{"message":"request","node":"sydney","path":"{path}","status":{status},"client_ip":"{ip}","user_agent":"{ua}"}}}}"#
        )
    }

    fn auth(v: &str) -> Vec<(String, String)> {
        vec![("Authorization".to_string(), v.to_string())]
    }

    /// Authorisation happens before any file is opened. An anonymous caller
    /// must not be able to make a node read its analytics log, which is the
    /// whole reason the expensive endpoints are gated.
    #[test]
    fn an_unauthorised_caller_never_reaches_the_log() {
        let out = traffic(
            "sydney",
            "/definitely/not/a/file",
            60,
            Duration::from_secs(60),
            &auth("Bearer wrong"),
            Some("right"),
        );
        // Unauthorised, not TrafficError: it never tried to open the path.
        assert!(matches!(out, PerfOutcome::Unauthorised));
    }

    #[test]
    fn no_token_configured_means_the_endpoint_does_not_exist() {
        let out = traffic("sydney", "/nope", 60, Duration::from_secs(60), &[], None);
        assert!(matches!(out, PerfOutcome::Disabled));
        let (code, _, _) = out.into_response();
        assert_eq!(code, 404, "404 not 401: it must not advertise a door");
    }

    /// An unreadable log is reported, not silently served as a quiet hour.
    /// This codebase has made that mistake: an absent file at the expected
    /// path looked exactly like a feature switched off.
    #[test]
    fn an_unreadable_log_is_a_503_with_a_reason() {
        let out = traffic(
            "sydney",
            "/definitely/not/a/file",
            60,
            Duration::from_secs(0),
            &auth("Bearer tok"),
            Some("tok"),
        );
        let (code, _, body) = out.into_response();
        assert_eq!(code, 503);
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(v["error"].as_str().unwrap_or("").len() > 0);
    }

    #[test]
    fn summarises_a_real_log_and_serves_it() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        let now = m6_core::util::now_iso8601();
        let ts = now.trim_end_matches('Z');
        writeln!(f, "{}", row(&format!("{ts}.100Z"), "1.2.3.4", "/", 200, "Chrome/131")).unwrap();
        writeln!(f, "{}", row(&format!("{ts}.200Z"), "5.6.7.8", "/robots.txt", 200,
            "Mozilla/5.0 (compatible; ClaudeBot/1.0; +claudebot@anthropic.com)")).unwrap();
        f.flush().unwrap();

        let out = traffic(
            "sydney",
            f.path().to_str().unwrap(),
            60,
            Duration::from_secs(0),
            &auth("Bearer tok"),
            Some("tok"),
        );
        let (code, headers, body) = out.into_response();
        assert_eq!(code, 200);
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["node"], "sydney");
        assert_eq!(v["total_requests"], 2);
        assert_eq!(v["crawlers"][0]["user_agent"].as_str().unwrap().contains("ClaudeBot"), true);
        assert!(v["logging"].is_object(), "logging health travels with it");
        // Never cached by anything in between.
        assert!(headers.iter().any(|(k, val)|
            k.eq_ignore_ascii_case("cache-control") && val == "no-store"));
    }
}
