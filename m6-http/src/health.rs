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
//! # What is deliberately not exposed
//!
//! No version, build or commit string (it tells a scanner which
//! vulnerabilities to try), and no request counts or cache statistics
//! (traffic volume is not public information). Those belong behind
//! authentication, alongside the richer metrics a dashboard would want. What
//! is here is the minimum an uptime monitor needs and nothing more.

use serde::Serialize;

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
    #[test]
    fn payload_leaks_no_version_or_traffic_data() {
        let (code, report) = HealthReport::build(
            "sydney", 7, vec![pool("m6-html", 1, 1)], vec!["origin".to_string()],
        );
        let (_, _, body) = report.into_response(code);
        let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let object = parsed.as_object().expect("object");
        let allowed = ["status", "node", "uptime_s", "pools", "url_backends"];
        for key in object.keys() {
            assert!(allowed.contains(&key.as_str()), "unexpected public field: {key}");
        }
    }
}
