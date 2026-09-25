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
//! **`/health` exposes no version or build string.** It is unauthenticated, and a
//! version on a public URL tells a scanner which vulnerabilities are worth trying.
//! That has not changed.
//!
//! It was very nearly changed on 2026-09-20, and the reason it was not is worth
//! keeping. Issue #105 wanted the running binary's identity visible, and the
//! first attempt put a bare `hash` on `/health` on the argument that a hash is
//! not a version and so discloses nothing. The disclosure argument holds. The
//! usefulness argument does not: **a hash on its own tells a reader nothing at
//! all.** It could be a hash of anything. An identity is a hash keyed to the
//! name and version of the thing it identifies, and the place those already live
//! is `/perf`. Owner's decision. See `BuildId`.
//!
//! `/perf` does report the release, from 2026-09-16. This reverses "no version at
//! any tier", so the reasoning is recorded rather than left as a silent edit.
//!
//! The disclosure argument applies to an anonymous reader, and `/perf` has none:
//! with no token configured it is a 404, and with one it is a 401 without the
//! credential. An attacker holding the perf token already reads live latency
//! percentiles and error counters, which is a far better tuning signal than a
//! version string. So the marginal disclosure is small, and it is bounded by a
//! secret we already treat as sensitive.
//!
//! Against that: with no version anywhere, "every node runs the release we think
//! it does" was an invariant nothing could check without ssh, and on 2026-09-16
//! four written records disagreed about this fleet while all three nodes served
//! something none of them named. An operator cannot act on a fleet they cannot
//! observe, and a silent version turned a deploy defect into four months of
//! plausible-looking bookkeeping. The version goes behind the token, not on
//! `/health`, which keeps the public surface exactly as it was.
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
pub fn metrics_authorised(headers: &[(String, String)], configured_token: Option<&str>) -> bool {
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
/// It deliberately does not carry `uptime_s`, the name and occupancy of every
/// socket pool, or the names of the URL backends. That would tell an anonymous
/// caller the internal service topology (`m6-html`, `m6-file`, `render-contact`,
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

/// What is running: the binary's name, its version, and a hash of its bytes.
///
/// **A hash alone is not an identity.** That was the first shape of #105 and it
/// was wrong: an opaque number on its own could be a hash of anything, and a
/// reader who sees two of them differ learns that something differs, not what.
/// Keyed to the name and the version it becomes the answer to a question an
/// operator actually asks, which is "what is this node running".
///
/// The three carry different information and none is redundant:
///
/// - `name` says WHICH binary. A node runs several, and "the node is on 1.10.0"
///   has never been one fact: on 2026-09-16 this fleet had m6-http and the
///   renderers built from different trees and nothing could express that.
/// - `version` says which release it claims to be. It is what an operator reads
///   and what release notes are written against.
/// - `hash` says which BUILD it actually is. Rust is not byte-reproducible, so
///   one tag built twice gives two binaries reporting one version. On
///   2026-09-20 staging ran one build of `v1.10.0` and production another, every
///   reading said the fleet agreed, and it was found by running `md5sum` on four
///   machines.
///
/// `hash` and not `md5`: the algorithm is how the value is produced, not what it
/// means, and naming the field after it makes changing it a wire break for every
/// reader. It IS md5 today, because that is the number the rest of the estate
/// already compares: `deploy/estate/*.json` records md5, `ops.sh capture` writes
/// md5, and the promotion gate refuses on md5. A second hash of the same bytes
/// under another algorithm would give an operator two numbers and answer nothing.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildId {
    /// The binary's own name, e.g. `"m6-http"`, from its crate at compile time.
    pub name: String,
    /// The m6 release this was built against. Same value, and the same
    /// reasoning, as the `version` field this replaced.
    pub version: String,
    /// Hex md5 of the running executable. Empty when it could not be read, which
    /// the monitor renders as unknown and counts as drift rather than agreement.
    pub hash: String,
}

/// This process's own [`BuildId`], computed once.
///
/// Computed once and cached for the life of the process, which is correct and
/// not merely cheap: a deploy replaces the file on disk while this process keeps
/// running the bytes it started with, and the bytes it started with are the
/// honest answer to "what is serving". Re-reading per request would report the
/// NEW binary while still running the old one, which is exactly the lie this is
/// here to prevent.
///
/// The name comes from the running executable rather than from a compile-time
/// constant, because the constant available to a library is m6-core's own name
/// and the answer wanted is the binary's. The version is m6-core's, which is the
/// m6 release: for the workspace binaries that is their own version, and for a
/// service linking core from a git tag it is that tag, which is the more useful
/// answer for a service whose own version means nothing to this fleet.
pub fn build_id() -> &'static BuildId {
    static BUILD: std::sync::OnceLock<BuildId> = std::sync::OnceLock::new();
    BUILD.get_or_init(|| BuildId {
        name: std::env::current_exe()
            .ok()
            .and_then(|p| p.file_stem().map(|s| s.to_string_lossy().into_owned()))
            .unwrap_or_default(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        hash: executable_md5(),
    })
}

/// Hex md5 of this process's own executable, or empty if it cannot be read.
///
/// Read in chunks rather than with `fs::read`, which would allocate the whole
/// binary: m6-http is tens of megabytes and this runs on a 1 cpu node with the
/// event loop about to start.
///
/// Empty on any failure, which is a real possibility under confinement and must
/// never take `/perf` down: a node that cannot say what it is running is still a
/// node that can report its latency. The monitor reads an empty hash as unknown
/// and counts it as drift rather than as agreement.
fn executable_md5() -> String {
    use md5::{Digest, Md5};
    use std::io::Read;

    let Ok(path) = std::env::current_exe() else {
        return String::new();
    };
    let Ok(mut file) = std::fs::File::open(path) else {
        return String::new();
    };
    let mut hasher = Md5::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        match file.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => hasher.update(&buf[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            // A partial read is worse than none: it produces a
            // plausible-looking hash of a prefix, which compares unequal
            // across two nodes running identical binaries and reports drift
            // that is not there.
            Err(_) => return String::new(),
        }
    }
    format!("{:x}", hasher.finalize())
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
        (
            code,
            HealthReport {
                status,
                node: node.to_string(),
            },
        )
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
            (
                "Content-Type".to_string(),
                "application/json; charset=utf-8".to_string(),
            ),
            ("Cache-Control".to_string(), "no-store".to_string()),
            // Nothing here is meant for a browser to render or a crawler to
            // index, and it is a public URL.
            ("X-Robots-Tag".to_string(), "noindex, nofollow".to_string()),
        ];
        (code, headers, body)
    }
}

/// The `/perf` payload: the numbers, plus the detail `/health` must not leak.
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
    /// What this node is running: name, version and build hash.
    ///
    /// This was a bare `version: String` until 2026-09-20 (#105), and the
    /// version is still in it, unchanged in meaning. What it could not do is
    /// tell two builds of one tag apart, and that is not a hypothetical: on
    /// 2026-09-20 staging ran one build of `v1.10.0` and production another,
    /// every reading said the fleet agreed, and it took `md5sum` on four
    /// machines to see it.
    ///
    /// Why the version alone was already worth having, kept because the argument
    /// still applies to the whole structure: without it the only way to learn
    /// what a node runs was to ssh in and ask the binary, so "every node runs the
    /// pinned release" was an invariant nothing could check. On 2026-09-16 four
    /// written records disagreed about this fleet and none matched it: the
    /// deployment repo's pin said v1.2.0, its captured config said 1.2.0 with an
    /// md5 matching nothing running, its release log named v1.1.0, and all three
    /// nodes served 1.3.0. No node was faulty. Nothing could observe the truth,
    /// so the records rotted without anyone being wrong on purpose.
    ///
    /// `serde(default)` so a node older than this change deserialises to an empty
    /// `BuildId` instead of making the whole payload unparseable to an
    /// aggregator. The monitor renders that as "too old to say" rather than as
    /// agreement.
    #[serde(default)]
    pub build: BuildId,
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
    /// Boxed: `TrafficReport` is far larger than every other variant here, so
    /// carrying it inline made the whole enum that size even when the outcome
    /// was a one-word error.
    Traffic(Box<TrafficReport>),
    /// The analytics log could not be read. Said out loud rather than reported
    /// as a quiet hour, which is a mistake this codebase has made: an absent
    /// file at the expected path looked exactly like a feature switched off.
    TrafficError(String),
    /// No token configured: the endpoint is switched off and answers 404, so
    /// it does not advertise a door that cannot be opened.
    Disabled,
    /// Configured, but the caller presented no or wrong credentials.
    Unauthorised,
    /// Boxed for the same reason as `Traffic`: `PerfReport` is 608 bytes
    /// against the 24 of the largest error variant, and an inline copy made
    /// every `PerfOutcome` that size.
    Ok(Box<PerfReport>),
}

/// What a report is about: the node, and the live facts to report on it.
pub struct PerfSubject<'a> {
    pub node: &'a str,
    pub uptime_s: u64,
    pub pools: Vec<PoolHealth>,
    pub url_backends: Vec<String>,
    pub host_path: &'a std::path::Path,
}

/// Who is asking, and what would authorise them.
///
/// Separate from [`PerfSubject`] because the two are answered at different
/// times and by different code: access decides whether there is a report at
/// all, and the subject is only read once it has. Keeping them apart is also
/// what stopped `build` taking eight positional arguments, six of which were
/// `&str`-ish and easy to transpose.
pub struct PerfAccess<'a> {
    pub headers: &'a [(String, String)],
    pub configured_token: Option<&'a str>,
}

impl PerfReport {
    pub fn build(
        subject: PerfSubject<'_>,
        access: PerfAccess<'_>,
        snapshot: impl FnOnce() -> StatsSnapshot,
    ) -> PerfOutcome {
        let PerfSubject {
            node,
            uptime_s,
            pools,
            url_backends,
            host_path,
        } = subject;
        let PerfAccess {
            headers,
            configured_token,
        } = access;
        match configured_token.filter(|t| !t.is_empty()) {
            None => PerfOutcome::Disabled,
            Some(_) if !metrics_authorised(headers, configured_token) => PerfOutcome::Unauthorised,
            Some(_) => {
                // `snapshot` is a closure so the reservoir sort happens only
                // after authorisation passes, never for an anonymous caller.
                // Read after authorisation, like the reservoir sort: an
                // anonymous caller never causes a /proc read either.
                PerfOutcome::Ok(Box::new(PerfReport {
                    node: node.to_string(),
                    build: build_id().clone(),
                    uptime_s,
                    pools,
                    url_backends,
                    metrics: snapshot(),
                    host: crate::host::snapshot(host_path),
                }))
            }
        }
    }
}

impl PerfOutcome {
    pub fn into_response(self) -> (u16, Vec<(String, String)>, Vec<u8>) {
        let mut headers = vec![
            (
                "Content-Type".to_string(),
                "application/json; charset=utf-8".to_string(),
            ),
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
        PoolHealth {
            name: name.to_string(),
            active,
            total,
        }
    }

    #[test]
    fn origin_with_live_pools_is_ok() {
        let (code, report) =
            HealthReport::build("sydney", &[pool("m6-html", 1, 1), pool("m6-file", 2, 2)]);
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
        assert_eq!(
            code, 200,
            "a cache node has no socket pools and is not degraded for it"
        );
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
            headers
                .iter()
                .find(|(n, _)| n.eq_ignore_ascii_case(k))
                .map(|(_, v)| v.as_str())
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
            "Bearer s3cre",     // prefix of the real token
            "Bearer s3secret1", // longer
            "Basic s3cret",     // wrong scheme
            "s3cret",           // no scheme
            "Bearer",           // no token
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
        let out = PerfReport::build(
            PerfSubject {
                node: "sydney",
                uptime_s: 5,
                pools: vec![],
                url_backends: vec![],
                host_path: Path::new("/"),
            },
            PerfAccess {
                headers: &auth("Bearer x"),
                configured_token: None,
            },
            snap,
        );
        let (code, _, body) = out.into_response();
        assert_eq!(code, 404);
        assert!(!String::from_utf8_lossy(&body).contains("unauthorised"));
    }

    /// The build identity reaches the wire, whole. An aggregator cannot report
    /// drift it never receives, and each of the three parts answers a different
    /// question: which binary, which release it claims to be, which build it
    /// actually is.
    #[test]
    fn perf_reports_the_running_build_identity() {
        let out = PerfReport::build(
            PerfSubject {
                node: "sydney",
                uptime_s: 5,
                pools: vec![],
                url_backends: vec![],
                host_path: Path::new("/"),
            },
            PerfAccess {
                headers: &auth("Bearer right"),
                configured_token: Some("right"),
            },
            snap,
        );
        let (code, _, body) = out.into_response();
        assert_eq!(code, 200);
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            v["build"]["version"].as_str(),
            Some(env!("CARGO_PKG_VERSION"))
        );
        assert!(!v["build"]["version"].as_str().unwrap().is_empty());

        // The name is the RUNNING binary's, not m6-core's. A library cannot
        // know the binary's name at compile time, so this reads the executable,
        // and a regression here would silently label every service "m6-core".
        let name = v["build"]["name"].as_str().expect("a name");
        assert!(!name.is_empty());
        assert_ne!(
            name, "m6-core",
            "the name must come from the running executable, not from the \
             library's own CARGO_PKG_NAME: every service would report m6-core"
        );

        // The hash is the md5 of this very binary. Checked against a hash taken
        // here rather than against a constant, which would pin whatever the
        // code produced on the day and prove only that it has not changed.
        // This is the property that makes the field worth having: it must equal
        // `md5sum` of the artefact, which is what deploy/estate/*.json records
        // and what the promotion gate compares.
        use md5::{Digest, Md5};
        let exe = std::env::current_exe().expect("a test binary has a path");
        let expected = format!("{:x}", Md5::digest(std::fs::read(&exe).unwrap()));
        assert_eq!(v["build"]["hash"].as_str(), Some(expected.as_str()));
        assert_eq!(expected.len(), 32, "hex md5 is 32 characters");
    }

    /// Two builds of one tag report the same version and different hashes. That
    /// is the whole reason the hash is there, so it is asserted rather than
    /// assumed: a version comparison cannot see this and a hash comparison can.
    ///
    /// Simulated by hashing two different byte strings, because the real case
    /// needs two compilations of one source and Rust gives no way to force that
    /// in a unit test. What is being pinned is the CLAIM: same version, different
    /// bytes, and only the hash distinguishes them.
    #[test]
    fn one_version_two_builds_differ_only_in_the_hash() {
        use md5::{Digest, Md5};
        let a = BuildId {
            name: "m6-http".into(),
            version: "1.10.0".into(),
            hash: format!("{:x}", Md5::digest(b"build one")),
        };
        let b = BuildId {
            hash: format!("{:x}", Md5::digest(b"build two")),
            ..a.clone()
        };

        assert_eq!(a.version, b.version, "same tag");
        assert_eq!(a.name, b.name, "same binary");
        assert_ne!(a.hash, b.hash, "different bytes");
        assert_ne!(
            a, b,
            "a fleet holding these two is NOT uniform, and comparing versions \
             alone would call it uniform. That is what happened on 2026-09-20."
        );
    }

    /// A payload without the field still parses, because the fleet is upgraded one
    /// node at a time and an aggregator that cannot read an older node learns
    /// nothing about the node it most needs to ask about.
    #[test]
    fn a_perf_payload_without_a_build_still_parses() {
        // A real payload with the field taken out, rather than a hand-written
        // fixture: a fixture only proves the fixture parses, and the first attempt
        // at one failed on unrelated required fields of StatsSnapshot.
        let out = PerfReport::build(
            PerfSubject {
                node: "sydney",
                uptime_s: 5,
                pools: vec![],
                url_backends: vec![],
                host_path: Path::new("/"),
            },
            PerfAccess {
                headers: &auth("Bearer right"),
                configured_token: Some("right"),
            },
            snap,
        );
        let (_, _, body) = out.into_response();
        let mut v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(v.as_object_mut().unwrap().remove("build").is_some());
        let p: PerfReport = serde_json::from_value(v).expect("older node must parse");
        assert_eq!(p.build, BuildId::default());
        assert_eq!(
            p.build.version, "",
            "and it reads as unknown, not as agreement"
        );
    }

    #[test]
    fn perf_is_401_with_a_scheme_hint_when_credentials_are_wrong() {
        let out = PerfReport::build(
            PerfSubject {
                node: "sydney",
                uptime_s: 5,
                pools: vec![],
                url_backends: vec![],
                host_path: Path::new("/"),
            },
            PerfAccess {
                headers: &auth("Bearer wrong"),
                configured_token: Some("right"),
            },
            snap,
        );
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
        let _ = PerfReport::build(
            PerfSubject {
                node: "sydney",
                uptime_s: 5,
                pools: vec![],
                url_backends: vec![],
                host_path: Path::new("/"),
            },
            PerfAccess {
                headers: &[],
                configured_token: Some("tok"),
            },
            counting,
        );
        assert!(!taken, "snapshot must not be taken without authorisation");
    }

    #[test]
    fn perf_returns_metrics_when_authorised() {
        let out = PerfReport::build(
            PerfSubject {
                node: "sydney",
                uptime_s: 11,
                pools: vec![pool("m6-html", 1, 1)],
                url_backends: vec!["origin".to_string()],
                host_path: Path::new("/"),
            },
            PerfAccess {
                headers: &auth("Bearer tok"),
                configured_token: Some("tok"),
            },
            snap,
        );
        let (code, _, body) = out.into_response();
        assert_eq!(code, 200);
        let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(parsed["node"], "sydney");
        assert_eq!(parsed["uptime_s"], 11);
        assert_eq!(parsed["metrics"]["requests_total"], 0);
        assert!(parsed["metrics"]["hit_samples"].is_number());
        // The detail that /health must not publish to anyone lives here.
        assert_eq!(parsed["pools"][0]["name"], "m6-html");
        assert_eq!(parsed["url_backends"][0], "origin");
        // And the machine underneath, so an aggregator gets a node in one
        // request and can read latency next to the load that produced it.
        assert!(
            parsed["host"].is_object(),
            "/perf carries the host snapshot"
        );
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
/// numbers say not to: the file is 31MB on origin and 47MB on edge-b, one request is
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
            row(
                "2026-09-11T08:00:00Z",
                "1.2.3.4",
                "/",
                200,
                "Mozilla/5.0 Chrome/131",
            ),
            row(
                "2026-09-11T08:00:01Z",
                "5.6.7.8",
                "/robots.txt",
                200,
                "Mozilla/5.0 (compatible; ClaudeBot/1.0; +claudebot@anthropic.com)",
            ),
            row(
                "2026-09-11T08:00:02Z",
                "9.9.9.9",
                "/.git/config",
                404,
                "curl/8",
            ),
        ];
        // A real scan: three distinct probe paths from one address.
        for (i, p) in ["/.env", "/wp-admin/setup.php", "/@fs/etc/passwd"]
            .iter()
            .enumerate()
        {
            lines.push(row(
                &format!("2026-09-11T08:01:{:02}Z", i),
                "203.0.113.5",
                p,
                404,
                "curl/8",
            ));
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
        let never = LoggingHealth {
            events_total: 0,
            seconds_since_last: None,
        };
        assert!(never.is_blind());

        let alive = LoggingHealth {
            events_total: 5000,
            seconds_since_last: Some(3),
        };
        assert!(!alive.is_blind());

        // Silenced: the process is up and the main layer stopped.
        let quiet = LoggingHealth {
            events_total: 5000,
            seconds_since_last: Some(600),
        };
        assert!(
            quiet.is_blind(),
            "ten minutes of silence from a 10s heartbeat"
        );
    }
}
