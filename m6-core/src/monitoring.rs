//! The `/health` and `/perf` endpoints: what they publish, and why so little.
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

use serde::{Deserialize, Serialize};

use crate::telemetry::StatsSnapshot;

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
    crate::headers::get(headers, "authorization")
        .and_then(|value| {
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
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PoolHealth {
    pub name: String,
    /// Members currently available (not marked failed).
    pub active: usize,
    /// Members known to the pool, including temporarily unavailable ones.
    pub total: usize,
}

/// The public payload. Field order here is the field order on the wire.
///
/// Two fields, and that is the whole endpoint. It is an unauthenticated public
/// URL, so everything on it is published to anyone who asks, and a monitor
/// needs exactly two things from it: whether this node can serve, and which
/// node answered.
///
/// It used to also carry `uptime_s`, the name and occupancy of every socket
/// pool, and the names of the URL backends. That told an anonymous caller the
/// internal service topology (`m6-html`, `m6-file`, `render-contact`,
/// `render-analytics`), how many workers back each one, when the process last
/// restarted, and, by polling, exactly when a deploy or a crash happened and
/// whether a pool was losing members. None of that helps a monitor decide up
/// or down, and all of it helps someone deciding what to aim at. It lives on
/// `/perf` now, which is token-gated.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HealthReport {
    /// `"ok"` or `"degraded"`.
    ///
    /// `String` rather than `&'static str` because an aggregator reads this
    /// back off the wire and cannot produce a `'static` one.
    pub status: String,
    /// This node's identity, e.g. `"sydney"`. The reason to have the endpoint
    /// at all: the answer says which node produced it.
    pub node: String,
}

impl HealthReport {
    /// Build a report and the HTTP status that should carry it.
    ///
    /// Degraded when any configured socket pool has no live member. A node
    /// with no socket pools at all (every cache node) is not degraded by
    /// that fact alone.
    /// `pools` decides the verdict and is not reported. The status code still
    /// carries the whole answer a monitor acts on, and `/perf` has the detail
    /// for whoever is allowed to see which pool went quiet.
    pub fn build(node: &str, pools: &[PoolHealth]) -> (u16, HealthReport) {
        let degraded = pools.iter().any(|p| p.active == 0);
        let status = if degraded { "degraded" } else { "ok" }.to_string();
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
        (code, HealthReport { status, node: node.to_string() })
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

/// The `/perf` payload: the numbers, plus the detail `/health` used to leak.
///
/// Carries `node` and `uptime_s` so a dashboard scraping several nodes can
/// attribute a sample without correlating two requests, and so a counter reset
/// is distinguishable from a quiet node (uptime going backwards means the
/// process restarted, not that traffic stopped).
///
/// `pools` and `url_backends` moved here from `/health`. They are genuinely
/// useful for diagnosing a degraded node, which is why they are kept rather
/// than dropped, and they are behind the token for the same reason the
/// latency percentiles are: they describe the shape of the thing being served.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PerfReport {
    pub node: String,
    /// This process's uptime. `host.uptime_s` is the machine's, and the two
    /// differing is how a service restart is told apart from a reboot.
    pub uptime_s: u64,
    /// Socket-backed pools. Empty on a cache node, which has none.
    pub pools: Vec<PoolHealth>,
    /// Names of URL backends. Presence only: proving one reachable would mean
    /// a network round trip inside a monitoring endpoint, which is how a
    /// monitoring endpoint learns to hang.
    pub url_backends: Vec<String>,
    pub metrics: StatsSnapshot,
    /// The machine underneath: load, memory, disk, temperature, uptime.
    ///
    /// Here rather than on a separate endpoint because an aggregator wants one
    /// request per node, and because these are the numbers you want *next to*
    /// the latency figures: a p99 that moved is a different conversation
    /// depending on whether load also moved. Every field is optional, and a
    /// field a platform cannot answer is absent rather than zero.
    pub host: crate::host::HostSnapshot,
}

/// Outcome of a `/perf` request.
pub enum PerfOutcome {
    /// A traffic summary from `/traffic`. Same gate as `/perf`.
    ///
    /// Its own path rather than more fields on `/perf`, for the reason that
    /// split `/health` from `/perf`: the cost profiles differ. `/perf` sorts a
    /// reservoir in memory; this reads the tail of a 47MB file. Putting the
    /// expensive one behind a conditional on the cheaper path leaves the
    /// expensive branch one misconfigured header away from being taken on
    /// every request, and splitting the paths is what keeps each answer's cost
    /// a property of the path rather than of the caller.
    Traffic(TrafficReport),
    /// The analytics log could not be read. Said out loud rather than reported
    /// as a quiet hour, which is a mistake this codebase has made: an absent
    /// file at the expected path looked exactly like a feature switched off.
    TrafficError(String),
    /// No token configured: the endpoint is switched off and answers 404, so
    /// it does not advertise a door that cannot be opened.
    Disabled,
    /// Configured, but the caller presented no or wrong credentials.
    Unauthorised,
    Ok(PerfReport),
}

impl PerfReport {
    #[allow(clippy::too_many_arguments)]
    pub fn build(
        node: &str,
        uptime_s: u64,
        pools: Vec<PoolHealth>,
        url_backends: Vec<String>,
        host_path: &std::path::Path,
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
                // Read after authorisation, like the reservoir sort: an
                // anonymous caller never causes a /proc read either.
                PerfOutcome::Ok(PerfReport {
                    node: node.to_string(),
                    uptime_s,
                    pools,
                    url_backends,
                    metrics: snapshot(),
                    host: crate::host::snapshot(host_path),
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
            PerfOutcome::Traffic(report) => {
                let body = serde_json::to_vec(&report)
                    .unwrap_or_else(|_| br#"{"error":"serialisation failed"}"#.to_vec());
                (200, headers, body)
            }
            // 503, not 500: the node is serving, but its own view of its
            // traffic is not available. A monitor shows that as a gap rather
            // than as a quiet hour.
            PerfOutcome::TrafficError(why) => {
                let body = serde_json::to_vec(&serde_json::json!({"error": why}))
                    .unwrap_or_else(|_| br#"{"error":"unreadable"}"#.to_vec());
                (503, headers, body)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn pool(name: &str, active: usize, total: usize) -> PoolHealth {
        PoolHealth { name: name.to_string(), active, total }
    }

    #[test]
    fn origin_with_live_pools_is_ok() {
        let (code, report) = HealthReport::build(
            "sydney",
            &[pool("m6-html", 1, 1), pool("m6-file", 2, 2)],
        );
        assert_eq!(code, 200);
        assert_eq!(report.status, "ok");
        assert_eq!(report.node, "sydney");
    }

    /// The regression this endpoint most needs to not have.
    ///
    /// A cache node has no socket pools at all, so `total_active_members()`
    /// is 0 on London and Chicago at all times. Deriving health from that
    /// count alone reports both edge nodes as permanently down.
    #[test]
    fn cache_node_with_no_socket_pools_is_ok_not_degraded() {
        let (code, report) = HealthReport::build("london", &[]);
        assert_eq!(code, 200, "a cache node has no socket pools and is not degraded for it");
        assert_eq!(report.status, "ok");
    }

    #[test]
    fn an_empty_pool_degrades_the_node() {
        let (code, report) = HealthReport::build(
            "sydney",
            &[pool("m6-html", 1, 1), pool("render-contact", 0, 1)],
        );
        assert_eq!(code, 503);
        assert_eq!(report.status, "degraded");
    }

    /// A member marked failed still counts in `total`, so a pool that has
    /// members but none available must degrade. Reading `total` instead of
    /// `active` would call this healthy.
    #[test]
    fn members_present_but_all_failed_is_degraded() {
        let (code, _) = HealthReport::build("sydney", &[pool("m6-html", 0, 3)]);
        assert_eq!(code, 503);
    }

    #[test]
    fn response_is_never_stored_and_never_indexed() {
        let (code, report) = HealthReport::build("sydney", &[]);
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

    /// An empty snapshot. The collector that fills one in lives in m6-http;
    /// these tests are about the endpoint's shape and authorisation, not about
    /// how the numbers are gathered.
    fn snap() -> StatsSnapshot {
        StatsSnapshot::default()
    }

    #[test]
    fn perf_is_404_when_no_token_is_configured() {
        // Off by default, and it does not advertise a door that cannot be
        // opened: 404, not 401.
        let out = PerfReport::build("sydney", 5, vec![], vec![], Path::new("/"), &auth("Bearer x"), None, snap);
        let (code, _, body) = out.into_response();
        assert_eq!(code, 404);
        assert!(!String::from_utf8_lossy(&body).contains("unauthorised"));
    }

    #[test]
    fn perf_is_401_with_a_scheme_hint_when_credentials_are_wrong() {
        let out = PerfReport::build("sydney", 5, vec![], vec![], Path::new("/"), &auth("Bearer wrong"), Some("right"), snap);
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
        let _ = PerfReport::build("sydney", 5, vec![], vec![], Path::new("/"), &[], Some("tok"), counting);
        assert!(!taken, "snapshot must not be taken without authorisation");
    }

    #[test]
    fn perf_returns_metrics_when_authorised() {
        let out = PerfReport::build(
            "sydney",
            11,
            vec![pool("m6-html", 1, 1)],
            vec!["origin".to_string()],
            Path::new("/"),
            &auth("Bearer tok"),
            Some("tok"),
            snap,
        );
        let (code, _, body) = out.into_response();
        assert_eq!(code, 200);
        let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(parsed["node"], "sydney");
        assert_eq!(parsed["uptime_s"], 11);
        assert_eq!(parsed["metrics"]["requests_total"], 0);
        assert!(parsed["metrics"]["hit_samples"].is_number());
        // The detail that /health used to publish to anyone lives here now.
        assert_eq!(parsed["pools"][0]["name"], "m6-html");
        assert_eq!(parsed["url_backends"][0], "origin");
        // And the machine underneath, so an aggregator gets a node in one
        // request and can read latency next to the load that produced it.
        assert!(parsed["host"].is_object(), "/perf carries the host snapshot");
        assert!(parsed["host"]["cpus"].as_u64().unwrap_or(0) >= 1);
        assert!(parsed["host"]["disk"]["total_bytes"].as_u64().unwrap_or(0) > 0);
        // Absent, not zero, where the platform cannot answer.
        assert!(parsed["host"].get("thermal").is_some());
    }

    /// The public payload is exactly two fields, and this asserts the set
    /// rather than an allowlist.
    ///
    /// An allowlist is the wrong shape for this test: it passes when a field
    /// is removed and, more to the point, it passed for as long as `/health`
    /// was publishing `uptime_s`, every pool name and occupancy, and the URL
    /// backend names, because those were on the list. Naming the exact set
    /// means anything added to this struct has to be added here too, in a
    /// test whose name says why that is a decision and not a formality.
    #[test]
    fn public_payload_is_exactly_status_and_node() {
        let (code, report) = HealthReport::build("sydney", &[pool("m6-html", 1, 1)]);
        let (_, _, body) = report.into_response(code);
        let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let object = parsed.as_object().expect("object");

        let mut keys: Vec<&str> = object.keys().map(|k| k.as_str()).collect();
        keys.sort_unstable();
        assert!(
            !object.contains_key("host"),
            "/health must not publish load, memory or disk either: it is the \
             same class of thing as the pool topology that was moved to /perf"
        );
        assert_eq!(
            keys,
            ["node", "status"],
            "/health is unauthenticated and public: every field here is \
             published to anyone who asks. Topology, worker counts and uptime \
             belong on token-gated /perf."
        );
    }
}


// ── /traffic ─────────────────────────────────────────────────────────────────

/// What `/traffic` publishes: the node's own view of who has been asking it
/// for things, and whether its logging is alive.
///
/// This is the endpoint that lets a fleet report be assembled without ssh. The
/// alternative was shipping the analytics log to a central box, and the
/// numbers say not to: the file is 31MB on syd and 47MB on chi, one request is
/// about 350 bytes, and the summary a human reads is about 1KB. Roughly
/// 1000:1. A second copy of the log would also land on a box that is not
/// backed up, and inherit a retention problem the original already has.
///
/// So the node summarises its own log and publishes the summary. The raw
/// NDJSON never moves, which is right for what it is: forensic material, read
/// by hand when something is being investigated, which is rare.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrafficReport {
    pub node: String,
    /// The window summarised, in minutes.
    pub window_minutes: u64,
    pub total_requests: u64,
    pub status: std::collections::BTreeMap<u16, u64>,
    /// Clients worth a human's attention. Already graded: see
    /// `telemetry::ClientSummary::is_notable`.
    pub notable: Vec<NotableClient>,
    /// Top talkers by request count, whatever they are.
    ///
    /// Separate from `notable` on purpose. Volume is not suspicion, and the
    /// rule that conflated them fired on the operator's own address; but "who
    /// is asking for the most" is still the first question about an hour of
    /// traffic, and the answer is usually a crawler or a person.
    pub heavy_hitters: Vec<HeavyHitter>,
    pub crawlers: Vec<CrawlerReport>,
    /// Bot-shaped requests discarded as forged, and the addresses that sent
    /// them. Reported rather than silently dropped, because "no crawlers" and
    /// "886 forged crawler requests" are very different quiet hours.
    pub forged_bot_requests: u64,
    pub forgers: Vec<String>,
    /// Single refused probes: recorded, not escalated. One 404 to
    /// `/.git/config` is internet weather.
    pub probe_noise: Vec<String>,
    pub logging: LoggingHealth,
    /// Deliberate firewall blocks and whether they are still being hit.
    ///
    /// `None` when no collector is installed on the node, which is different
    /// from a node with no blocks. Read from a file written by a privileged
    /// timer, because `nft list ruleset` needs CAP_NET_ADMIN and the process
    /// answering public requests must not have it.
    #[serde(default)]
    pub firewall: Option<crate::firewall::FirewallState>,
}

/// Whether this process's main log layer is still emitting.
///
/// Replaces counting log targets out of `journalctl` over ssh. A config reload
/// can silence every target except `analytics`, and when it does the process
/// looks entirely healthy from outside. It does not look healthy from inside,
/// which is where this is measured.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct LoggingHealth {
    pub events_total: u64,
    /// `None` means nothing has ever been emitted, which is not the same as
    /// "emitted zero seconds ago" and must not be reported as healthy.
    pub seconds_since_last: Option<u64>,
}

impl LoggingHealth {
    pub fn read() -> Self {
        Self {
            events_total: crate::log::pulse().events(),
            seconds_since_last: crate::log::pulse().seconds_since_last(),
        }
    }

    /// A healthy m6-http emits `periodic stats` every ten seconds. Past this,
    /// on a process that is up and serving, the main layer has gone quiet.
    pub const QUIET_SECONDS: u64 = 40;

    pub fn is_blind(&self) -> bool {
        match self.seconds_since_last {
            None => true,
            Some(s) => s > Self::QUIET_SECONDS,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NotableClient {
    pub ip: String,
    pub requests: u64,
    pub distinct_user_agents: usize,
    pub status: std::collections::BTreeMap<u16, u64>,
    pub probe_paths: Vec<String>,
    pub injection_paths: Vec<String>,
    pub rotating_user_agents: bool,
    pub first_seen: String,
    pub last_seen: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HeavyHitter {
    pub ip: String,
    pub requests: u64,
    pub top_path: String,
    pub user_agents: usize,
    /// Share of responses that were 4xx or 5xx, so a loud client that is
    /// being served is distinguishable from one that is being refused.
    pub error_ratio: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CrawlerReport {
    pub user_agent: String,
    pub requests: u64,
    pub client_ips: Vec<String>,
    pub paths: Vec<String>,
}

impl TrafficReport {
    /// Summarise an analytics stream.
    ///
    /// `since` is an RFC 3339 prefix; the timestamps are fixed-width UTC so a
    /// string compare is a time compare.
    pub fn build(node: &str, ndjson: &str, since: &str, window_minutes: u64) -> Self {
        let records: Vec<crate::telemetry::AnalyticsRecord> =
            crate::telemetry::parse_analytics(ndjson, since).collect();
        let summary = crate::telemetry::TrafficSummary::from_records(&records);

        let notable: Vec<NotableClient> = summary
            .clients
            .iter()
            .filter(|(_, c)| c.is_notable())
            .map(|(ip, c)| NotableClient {
                ip: ip.clone(),
                requests: c.requests,
                distinct_user_agents: c.distinct_user_agents,
                status: c.status.clone(),
                probe_paths: c.probe_paths.iter().map(|(p, _)| p.clone()).collect(),
                injection_paths: c.injection_paths.iter().map(|(p, _)| p.clone()).collect(),
                rotating_user_agents: c.is_rotating_user_agents,
                first_seen: c.first_seen.clone(),
                last_seen: c.last_seen.clone(),
            })
            .collect();

        let probe_noise: Vec<String> = summary
            .clients
            .iter()
            .filter(|(_, c)| !c.is_notable() && !c.probe_paths.is_empty())
            .map(|(ip, c)| format!("{ip} {}", c.probe_paths[0].0))
            .collect();

        let heavy_hitters: Vec<HeavyHitter> = summary
            .clients
            .iter()
            .take(8)
            .map(|(ip, c)| HeavyHitter {
                ip: ip.clone(),
                requests: c.requests,
                top_path: c.paths.first().map(|(p, _)| p.clone()).unwrap_or_default(),
                user_agents: c.distinct_user_agents,
                error_ratio: c.error_ratio(),
            })
            .collect();

        Self {
            node: node.to_string(),
            window_minutes,
            heavy_hitters,
            total_requests: summary.total_requests,
            status: summary.status.clone(),
            notable,
            crawlers: summary
                .crawlers
                .iter()
                .map(|c| CrawlerReport {
                    user_agent: c.user_agent.clone(),
                    requests: c.requests,
                    client_ips: c.client_ips.clone(),
                    paths: c.paths.iter().map(|(p, _)| p.clone()).collect(),
                })
                .collect(),
            forged_bot_requests: summary.forged_bot_requests,
            forgers: summary.forgers.clone(),
            probe_noise,
            logging: LoggingHealth::read(),
            firewall: None,
        }
    }
}

#[cfg(test)]
mod traffic_tests {
    use super::*;

    fn row(ts: &str, ip: &str, path: &str, status: u16, ua: &str) -> String {
        format!(
            r#"{{"timestamp":"{ts}","level":"INFO","fields":{{"message":"request","node":"sydney","path":"{path}","status":{status},"client_ip":"{ip}","user_agent":"{ua}"}}}}"#
        )
    }

    #[test]
    fn summarises_a_stream_into_something_small() {
        let mut lines = vec![
            row("2026-09-11T08:00:00Z", "1.2.3.4", "/", 200, "Mozilla/5.0 Chrome/131"),
            row("2026-09-11T08:00:01Z", "5.6.7.8", "/robots.txt", 200,
                "Mozilla/5.0 (compatible; ClaudeBot/1.0; +claudebot@anthropic.com)"),
            row("2026-09-11T08:00:02Z", "9.9.9.9", "/.git/config", 404, "curl/8"),
        ];
        // A real scan: three distinct probe paths from one address.
        for (i, p) in ["/.env", "/wp-admin/setup.php", "/@fs/etc/passwd"].iter().enumerate() {
            lines.push(row(&format!("2026-09-11T08:01:{:02}Z", i), "203.0.113.5", p, 404, "curl/8"));
        }
        let r = TrafficReport::build("sydney", &lines.join("\n"), "", 60);

        assert_eq!(r.total_requests, 6);
        assert_eq!(r.crawlers.len(), 1);
        assert!(r.crawlers[0].user_agent.contains("ClaudeBot"));
        assert_eq!(r.notable.len(), 1, "the scanner, not the single 404");
        assert_eq!(r.notable[0].ip, "203.0.113.5");
        assert_eq!(r.probe_noise, vec!["9.9.9.9 /.git/config".to_string()]);
        assert_eq!(r.status.get(&200), Some(&2));
        assert_eq!(r.status.get(&404), Some(&4));

        // The whole point: what goes over the wire is small.
        let json = serde_json::to_string(&r).unwrap();
        assert!(json.len() < 2000, "summary was {} bytes", json.len());
    }

    /// Never having logged is not the same as having logged just now.
    #[test]
    fn logging_health_distinguishes_never_from_recently() {
        let never = LoggingHealth { events_total: 0, seconds_since_last: None };
        assert!(never.is_blind());

        let alive = LoggingHealth { events_total: 5000, seconds_since_last: Some(3) };
        assert!(!alive.is_blind());

        // Silenced: the process is up and the main layer stopped.
        let quiet = LoggingHealth { events_total: 5000, seconds_since_last: Some(600) };
        assert!(quiet.is_blind(), "ten minutes of silence from a 10s heartbeat");
    }
}
