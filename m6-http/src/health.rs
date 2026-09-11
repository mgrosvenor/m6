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
