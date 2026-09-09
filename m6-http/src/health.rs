//! A cheap, uncacheable per-node health endpoint.
//!
//! Answered inside m6-http before routing, the cache and any backend, so it
//! costs a JSON serialisation and nothing else. It exists so that an uptime
//! monitor has something better to hit than a real page.
//!
//! # Why not just monitor `/`
//!
//! Monitoring the homepage conflates three questions that have different
//! answers and different fixes:
//!
//! 1. Is this node's m6-http process alive and accepting TLS?
//! 2. Are this node's backends present?
//! 3. Does the site render?
//!
//! Measured on 2026-09-09, rendering `/` costs ~6 ms of Tera work on the
//! origin (105 images, ~43 us of per-image manifest work each), and a monitor
//! that sends `Cache-Control: no-cache` forces every check to pay it. That is
//! a large, variable answer to what should be a small, constant question.
//!
//! It also cannot tell you *which* node is unwell. Checks against the apex go
//! wherever GeoDNS sends the checking region, so one dead node can hide
//! behind two healthy ones. `/health` is per-node by construction: point the
//! monitor at `syd.`/`lon.`/`chi.` and the answer names the node that
//! produced it.
//!
//! # Health means "this node can serve", not "the fleet is fine"
//!
//! The two node roles have genuinely different backend shapes, and conflating
//! them produces a health check that lies:
//!
//! - The **origin** has unix-socket pools (m6-html, m6-file, the renderers).
//!   If every socket in a pool is gone, that pool cannot serve, and the node
//!   is degraded.
//! - A **cache node** has one URL backend (the origin over h2c) and *no*
//!   socket pools at all. `total_active_members()` counts only socket pools,
//!   so it reads 0 on London and Chicago permanently.
//!
//! A naive "0 members means unhealthy" check would therefore report both edge
//! nodes as permanently down. Worse, it would be wrong in the other direction
//! too: a cache node with a warm cache keeps serving correctly while the
//! origin is unreachable, which is the entire point of having edges. So an
//! unreachable origin is deliberately *not* this node's health problem.
//!
//! What is reported is strictly what this node can answer for: every
//! configured socket pool has at least one live member. That is checkable
//! locally, cheaply, and without a network round trip that could itself hang.
//!
//! # Two endpoints, because they have opposite requirements
//!
//! `/health` answers up or down. `/perf` answers counters and latency
//! percentiles. They are separate paths rather than one path with a richer
//! response for authorised callers, and the reason is cost.
//!
//! **A health check has to be constant-cost by construction.** It is the most
//! frequently hit path on the box, it runs when the machine is already in
//! trouble, and it is the thing a monitor uses to decide whether the node is
//! alive. Percentiles mean sorting a 4096-sample reservoir and serialising a
//! larger payload. Put that behind a conditional on the same path and the
//! expensive branch is one misconfigured header away from being taken on
//! every check, on every node, forever. Splitting the paths makes the cheap
//! answer cheap because there is no other answer it can give.
//!
//! **`/perf` is token-gated, and not out of modesty about traffic volume.**
//! Live latency and error counters are a feedback channel for whoever is
//! attacking you. An attacker probing for a resource-exhaustion path is
//! normally blind: they cannot tell whether a request is expensive or whether
//! their load is landing. Publishing hit and miss percentiles tells them
//! exactly which requests cost 2.8us and which cost 7.8ms, about 2800 to 1 on
//! this deployment, and `backend_errors_total` then confirms in real time
//! when they have found something that hurts. That turns a blind probe into a
//! tuning loop. Gating it also means an anonymous scrape loop can never reach
//! the sort.
//!
//! No version or build string is exposed by either endpoint, at any tier:
//! that only tells a scanner which vulnerabilities are worth trying.
//!
//! With no token configured `/perf` does not exist at all (404, not 401), so
//! forgetting to configure it fails closed and does not advertise a door.

use serde::Serialize;

use crate::stats::StatsSnapshot;

/// Compare a presented credential against the expected one without leaking
/// the match position through timing.
///
/// A naive `==` on strings returns at the first differing byte, so response
/// time reveals how many leading bytes were correct and the token can be
/// recovered one byte at a time. Lengths are compared first and unequal
/// lengths rejected outright, which does leak length; that is not
/// recoverable-secret information in the way a prefix is.
fn credential_matches(presented: &str, expected: &str) -> bool {
    let (a, b) = (presented.as_bytes(), expected.as_bytes());
    if a.len() != b.len() || expected.is_empty() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Whether this request may see metrics.
///
/// Expects `Authorization: Bearer <token>`. Returns false when no token is
/// configured, so metrics are off unless deliberately switched on.
pub fn metrics_authorised(
    headers: &[(String, String)],
    configured_token: Option<&str>,
) -> bool {
    let Some(expected) = configured_token.filter(|t| !t.is_empty()) else {
        return false;
    };
    headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("authorization"))
        .and_then(|(_, value)| {
            // The scheme is case-insensitive per RFC 9110 11.1.
            let rest = value.strip_prefix("Bearer ").or_else(|| {
                value
                    .get(..7)
                    .filter(|p| p.eq_ignore_ascii_case("bearer "))
                    .and_then(|_| value.get(7..))
            })?;
            Some(credential_matches(rest.trim(), expected))
        })
        .unwrap_or(false)
}

/// One socket-backed backend pool's occupancy.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct PoolHealth {
    pub name: String,
    /// Members currently available (not marked failed).
    pub active: usize,
    /// Members known to the pool, including temporarily unavailable ones.
    pub total: usize,
}

/// The public payload. Field order here is the field order on the wire.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct HealthReport {
    /// `"ok"` or `"degraded"`.
    pub status: &'static str,
    /// This node's identity, e.g. `"sydney"`. The reason to have the endpoint
    /// at all: the answer says which node produced it.
    pub node: String,
    /// Whole seconds since this process began serving.
    pub uptime_s: u64,
    /// Socket-backed pools. Empty on a cache node, which has none.
    pub pools: Vec<PoolHealth>,
    /// Names of URL backends. Presence only: proving one reachable would mean
    /// a network round trip inside a health check, which is how a health
    /// check learns to hang.
    pub url_backends: Vec<String>,
}

impl HealthReport {
    /// Build a report and the HTTP status that should carry it.
    ///
    /// Degraded when any configured socket pool has no live member. A node
    /// with no socket pools at all (every cache node) is not degraded by
    /// that fact alone.
    pub fn build(
        node: &str,
        uptime_s: u64,
        pools: Vec<PoolHealth>,
        url_backends: Vec<String>,
    ) -> (u16, HealthReport) {
        let degraded = pools.iter().any(|p| p.active == 0);
        let status = if degraded { "degraded" } else { "ok" };
        // 503 is the honest code: the node is up enough to answer, but not
        // able to serve. A monitor treating any non-2xx as down is then
        // correct without needing to parse the body.
        //
        // Deliberately never derived from performance. A latency spike is a
        // signal for a human or a dashboard to act on, not grounds for a node
        // to declare itself down and be pulled from rotation while it is
        // still serving every request correctly. That is also why the numbers
        // live on `/perf` and not here.
        let code = if degraded { 503 } else { 200 };
        (code, HealthReport { status, node: node.to_string(), uptime_s, pools, url_backends })
    }

    /// Serialise to a body plus headers.
    ///
    /// `no-store`, not `no-cache`: a stale health answer is worse than no
    /// answer, and this must never be stored by us, by a downstream cache, or
    /// by the monitor itself.
    pub fn into_response(self, code: u16) -> (u16, Vec<(String, String)>, Vec<u8>) {
        let body = serde_json::to_vec(&self)
            // A serialisation failure here is not a reason to fail the health
            // check with an unparseable body; fall back to something a
            // monitor can still read.
            .unwrap_or_else(|_| br#"{"status":"degraded"}"#.to_vec());
        let headers = vec![
            ("Content-Type".to_string(), "application/json; charset=utf-8".to_string()),
            ("Cache-Control".to_string(), "no-store".to_string()),
            // Nothing here is meant for a browser to render or a crawler to
            // index, and it is a public URL.
            ("X-Robots-Tag".to_string(), "noindex, nofollow".to_string()),
        ];
        (code, headers, body)
    }
}

/// The `/perf` payload: everything `/health` reports, plus the numbers.
///
/// Carries `node` and `uptime_s` as well so a dashboard scraping several
/// nodes can attribute a sample without correlating two requests, and so a
/// counter reset is distinguishable from a quiet node (uptime went
/// backwards means the process restarted, not that traffic stopped).
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct PerfReport {
    pub node: String,
    pub uptime_s: u64,
    pub metrics: StatsSnapshot,
}

/// Outcome of a `/perf` request.
pub enum PerfOutcome {
    /// No token configured: the endpoint is switched off and answers 404, so
    /// it does not advertise a door that cannot be opened.
    Disabled,
    /// Configured, but the caller presented no or wrong credentials.
    Unauthorised,
    Ok(PerfReport),
}

impl PerfReport {
    pub fn build(
        node: &str,
        uptime_s: u64,
        headers: &[(String, String)],
        configured_token: Option<&str>,
        snapshot: impl FnOnce() -> StatsSnapshot,
    ) -> PerfOutcome {
        match configured_token.filter(|t| !t.is_empty()) {
            None => PerfOutcome::Disabled,
            Some(_) if !metrics_authorised(headers, configured_token) => {
                PerfOutcome::Unauthorised
            }
            Some(_) => {
                // `snapshot` is a closure so the reservoir sort happens only
                // after authorisation passes, never for an anonymous caller.
                PerfOutcome::Ok(PerfReport {
                    node: node.to_string(),
                    uptime_s,
                    metrics: snapshot(),
                })
            }
        }
    }
}

impl PerfOutcome {
    pub fn into_response(self) -> (u16, Vec<(String, String)>, Vec<u8>) {
        let mut headers = vec![
            ("Content-Type".to_string(), "application/json; charset=utf-8".to_string()),
            ("Cache-Control".to_string(), "no-store".to_string()),
            ("X-Robots-Tag".to_string(), "noindex, nofollow".to_string()),
        ];
        match self {
            PerfOutcome::Disabled => (404, headers, br#"{"error":"not found"}"#.to_vec()),
            PerfOutcome::Unauthorised => {
                // RFC 9110 11.6.1: a 401 MUST carry WWW-Authenticate naming
                // the scheme, otherwise a client cannot know how to retry.
                headers.push(("WWW-Authenticate".to_string(), "Bearer".to_string()));
                (401, headers, br#"{"error":"unauthorised"}"#.to_vec())
            }
            PerfOutcome::Ok(report) => {
                let body = serde_json::to_vec(&report)
                    .unwrap_or_else(|_| br#"{"error":"serialisation failed"}"#.to_vec());
                (200, headers, body)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pool(name: &str, active: usize, total: usize) -> PoolHealth {
        PoolHealth { name: name.to_string(), active, total }
    }

    #[test]
    fn origin_with_live_pools_is_ok() {
        let (code, report) = HealthReport::build(
            "sydney", 42,
            vec![pool("m6-html", 1, 1), pool("m6-file", 2, 2)],
            vec![],
        );
        assert_eq!(code, 200);
        assert_eq!(report.status, "ok");
        assert_eq!(report.node, "sydney");
        assert_eq!(report.uptime_s, 42);
    }

    /// The regression this endpoint most needs to not have.
    ///
    /// A cache node has no socket pools at all, so `total_active_members()`
    /// is 0 on London and Chicago at all times. Deriving health from that
    /// count alone reports both edge nodes as permanently down.
    #[test]
    fn cache_node_with_no_socket_pools_is_ok_not_degraded() {
        let (code, report) = HealthReport::build(
            "london", 900, vec![], vec!["origin".to_string()],
        );
        assert_eq!(code, 200, "a cache node has no socket pools and is not degraded for it");
        assert_eq!(report.status, "ok");
        assert_eq!(report.url_backends, vec!["origin".to_string()]);
        assert!(report.pools.is_empty());
    }

    #[test]
    fn an_empty_pool_degrades_the_node() {
        let (code, report) = HealthReport::build(
            "sydney", 1,
            vec![pool("m6-html", 1, 1), pool("render-contact", 0, 1)],
            vec![],
        );
        assert_eq!(code, 503);
        assert_eq!(report.status, "degraded");
    }

    /// A member marked failed still counts in `total`, so a pool that has
    /// members but none available must degrade. Reading `total` instead of
    /// `active` would call this healthy.
    #[test]
    fn members_present_but_all_failed_is_degraded() {
        let (code, _) = HealthReport::build("sydney", 1, vec![pool("m6-html", 0, 3)], vec![]);
        assert_eq!(code, 503);
    }

    #[test]
    fn response_is_never_stored_and_never_indexed() {
        let (code, report) = HealthReport::build("sydney", 5, vec![], vec![]);
        let (code, headers, body) = report.into_response(code);
        assert_eq!(code, 200);
        let get = |k: &str| {
            headers.iter().find(|(n, _)| n.eq_ignore_ascii_case(k)).map(|(_, v)| v.as_str())
        };
        assert_eq!(get("Cache-Control"), Some("no-store"));
        assert_eq!(get("X-Robots-Tag"), Some("noindex, nofollow"));
        assert_eq!(get("Content-Type"), Some("application/json; charset=utf-8"));

        let parsed: serde_json::Value = serde_json::from_slice(&body).expect("valid JSON");
        assert_eq!(parsed["status"], "ok");
        assert_eq!(parsed["node"], "sydney");
    }

    /// Traffic volume and build identity are not public. If a future change
    /// adds them to the public payload, this fails.
    fn auth(value: &str) -> Vec<(String, String)> {
        vec![("Authorization".to_string(), value.to_string())]
    }

    #[test]
    fn no_configured_token_means_metrics_are_never_served() {
        // The secure default: forgetting to configure it fails closed, and no
        // presented credential can talk its way past a token that is unset.
        assert!(!metrics_authorised(&auth("Bearer anything"), None));
        assert!(!metrics_authorised(&auth("Bearer anything"), Some("")));
        assert!(!metrics_authorised(&[], None));
    }

    #[test]
    fn correct_bearer_token_authorises() {
        assert!(metrics_authorised(&auth("Bearer s3cret"), Some("s3cret")));
        // RFC 9110 11.1: the auth scheme is case-insensitive.
        assert!(metrics_authorised(&auth("bearer s3cret"), Some("s3cret")));
        assert!(metrics_authorised(&auth("BEARER s3cret"), Some("s3cret")));
        // Header field names are case-insensitive too.
        assert!(metrics_authorised(
            &[("authorization".to_string(), "Bearer s3cret".to_string())],
            Some("s3cret")
        ));
    }

    #[test]
    fn wrong_or_malformed_credentials_are_refused() {
        for value in [
            "Bearer wrong",
            "Bearer s3cre",       // prefix of the real token
            "Bearer s3secret1",   // longer
            "Basic s3cret",       // wrong scheme
            "s3cret",             // no scheme
            "Bearer",             // no token
            "",
        ] {
            assert!(
                !metrics_authorised(&auth(value), Some("s3cret")),
                "must refuse {value:?}"
            );
        }
        assert!(!metrics_authorised(&[], Some("s3cret")));
    }

    /// A prefix of the real token must not compare equal. This is the
    /// property that a length check alone would give, but the comparison is
    /// also constant-time so response latency cannot be used to recover the
    /// token one byte at a time.
    #[test]
    fn credential_comparison_rejects_prefixes_and_empty() {
        assert!(credential_matches("abc123", "abc123"));
        assert!(!credential_matches("abc12", "abc123"));
        assert!(!credential_matches("abc1234", "abc123"));
        assert!(!credential_matches("", ""));
        assert!(!credential_matches("", "abc123"));
    }

    fn snap() -> StatsSnapshot {
        crate::stats::Stats::new().snapshot()
    }

    #[test]
    fn perf_is_404_when_no_token_is_configured() {
        // Off by default, and it does not advertise a door that cannot be
        // opened: 404, not 401.
        let out = PerfReport::build("sydney", 5, &auth("Bearer x"), None, snap);
        let (code, _, body) = out.into_response();
        assert_eq!(code, 404);
        assert!(!String::from_utf8_lossy(&body).contains("unauthorised"));
    }

    #[test]
    fn perf_is_401_with_a_scheme_hint_when_credentials_are_wrong() {
        let out = PerfReport::build("sydney", 5, &auth("Bearer wrong"), Some("right"), snap);
        let (code, headers, _) = out.into_response();
        assert_eq!(code, 401);
        assert!(headers
            .iter()
            .any(|(n, v)| n.eq_ignore_ascii_case("WWW-Authenticate") && v == "Bearer"));
    }

    /// The reservoir sort must never run for an unauthorised caller. If it
    /// did, an anonymous scrape loop would be a cheap way to make the server
    /// work.
    #[test]
    fn perf_does_not_snapshot_unless_authorised() {
        let mut taken = false;
        let counting = || {
            taken = true;
            snap()
        };
        let _ = PerfReport::build("sydney", 5, &[], Some("tok"), counting);
        assert!(!taken, "snapshot must not be taken without authorisation");
    }

    #[test]
    fn perf_returns_metrics_when_authorised() {
        let out = PerfReport::build("sydney", 11, &auth("Bearer tok"), Some("tok"), snap);
        let (code, _, body) = out.into_response();
        assert_eq!(code, 200);
        let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(parsed["node"], "sydney");
        assert_eq!(parsed["uptime_s"], 11);
        assert_eq!(parsed["metrics"]["requests_total"], 0);
        assert!(parsed["metrics"]["hit_samples"].is_number());
    }

    #[test]
    fn payload_leaks_no_version_or_traffic_data() {
        let (code, report) = HealthReport::build(
            "sydney", 7, vec![pool("m6-html", 1, 1)], vec!["origin".to_string()],
        );
        let (_, _, body) = report.into_response(code);
        let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let object = parsed.as_object().expect("object");
        let allowed = ["status", "node", "uptime_s", "pools", "url_backends"];
        assert!(!object.contains_key("metrics"), "metrics must be absent without a token");
        for key in object.keys() {
            assert!(allowed.contains(&key.as_str()), "unexpected public field: {key}");
        }
    }
}

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
