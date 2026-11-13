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

use std::net::IpAddr;

use crate::auth::{self, PublicKey, Refusal};
use crate::config::HealthConfig;

pub use m6_core::monitoring::{
    metrics_authorised, HealthReport, PerfAccess, PerfOutcome, PerfReport, PerfSubject, PoolHealth,
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
            metrics_require: None,
            metrics_peers: None,
            metrics_peer_ips: None,
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
    /// inline in site.toml must be refused, because site.toml is committed and
    /// shipped to every node.
    #[test]
    fn inline_token_in_toml_is_refused() {
        let err = toml::from_str::<HealthConfig>("metrics_token = \"inline-secret\"")
            .expect_err("an inline token must be refused, not ignored");
        assert!(
            err.to_string().contains("metrics_token"),
            "the error should name the key: {err}"
        );
    }
}

#[cfg(test)]
mod monitoring_exclusion_tests {
    use super::*;

    /// Regression guard. The cache-miss accounting on the serving paths fires
    /// on any `Ready` outcome, and /health and /perf both return Ready. Left
    /// uncounted for, every health check was recorded as a cache miss: 10
    /// checks moved requests_total by 11.
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
        for backend in [
            "cache",
            "m6-html",
            "m6-file",
            "render-contact",
            "render-reports",
            "origin",
            "method-check",
            "",
        ] {
            assert!(
                !is_monitoring_endpoint(backend),
                "{backend} is real traffic and must be counted"
            );
        }
    }
}

// ── /traffic ─────────────────────────────────────────────────────────────────

use std::time::{Duration, Instant};

use m6_core::monitoring::TrafficReport;
use m6_core::telemetry::AnalyticsRecord;

/// Where the privileged collector leaves `nft -j list ruleset` output.
const FIREWALL_JSON: &str = "/var/lib/m6/firewall.json";

/// The most events `/traffic` keeps. Allocated once at start.
const TRAFFIC_SLOTS: usize = 32_768;

/// How far back `/traffic` reports, in minutes.
///
/// The ring holds far more than an hour at the busiest rate observed (about
/// 1,000 lines in an hour), so the hour is applied when the report is built
/// rather than when an event is kept.
const TRAFFIC_WINDOW_MINUTES: u64 = 60;

/// The most bytes a slot keeps of a path. The report shows the cut path, the
/// log keeps the whole one.
const PATH_CAP: usize = 512;

/// The most bytes a slot keeps of a client address. An IPv6 address is at most
/// 45 characters.
const IP_CAP: usize = 45;

/// The most bytes a slot keeps of a user agent.
const UA_CAP: usize = 256;

/// The three analytics lines m6-http writes per request or poll.
///
/// A request and a rate-limited request both log `"message":"request"`: the
/// rate limiter logs that row with status 429. A monitor poll logs
/// `"message":"monitor"`, which keeps it out of the site traffic counts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Request,
    Monitor,
    RateLimited,
}

impl Kind {
    fn message(self) -> &'static str {
        match self {
            Kind::Request | Kind::RateLimited => "request",
            Kind::Monitor => "monitor",
        }
    }
}

/// A capped string in a fixed array, copied into a slot without allocating.
#[derive(Clone)]
struct Field<const N: usize> {
    /// Byte length, or `ABSENT` when the event carried no such value.
    len: u16,
    bytes: [u8; N],
}

impl<const N: usize> Field<N> {
    /// The length that marks a value the event did not carry.
    ///
    /// Only the user agent uses it. An absent user agent is not an empty one,
    /// and the record has to keep the difference because the report is built
    /// from the record the log line would have parsed to.
    const ABSENT: u16 = u16::MAX;

    fn empty() -> Self {
        Self {
            len: 0,
            bytes: [0; N],
        }
    }

    /// Copy `s` in, cut at `N` bytes on a character boundary.
    fn set(&mut self, s: &str) {
        let mut end = s.len().min(N);
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        self.bytes[..end].copy_from_slice(&s.as_bytes()[..end]);
        self.len = end as u16;
    }

    fn mark_absent(&mut self) {
        self.len = Self::ABSENT;
    }

    fn as_str(&self) -> &str {
        let len = if self.len == Self::ABSENT {
            0
        } else {
            usize::from(self.len)
        };
        std::str::from_utf8(&self.bytes[..len]).unwrap_or_default()
    }

    fn as_opt(&self) -> Option<&str> {
        (self.len != Self::ABSENT).then(|| self.as_str())
    }
}

/// One logged event, as `/traffic` needs it.
///
/// Fixed size, so the ring is one allocation and a push is one copy. The
/// fields are the ones the report reads, plus the time and the kind.
#[derive(Clone)]
struct TrafficSlot {
    at_us: u64,
    kind: Kind,
    status: u16,
    path: Field<PATH_CAP>,
    client_ip: Field<IP_CAP>,
    user_agent: Field<UA_CAP>,
}

impl TrafficSlot {
    fn empty() -> Self {
        Self {
            at_us: 0,
            kind: Kind::Request,
            status: 0,
            path: Field::empty(),
            client_ip: Field::empty(),
            user_agent: Field::empty(),
        }
    }
}

/// The last events this process logged, for `/traffic`. Allocated at start.
///
/// A ring of fixed slots and a write index. A logged request copies a few
/// hundred bytes into the next slot, overwriting the oldest when the ring is
/// full: no allocation and no system call. The report reads the slots.
///
/// The window is a count of events rather than a size in bytes, and the hour
/// is applied when the report is built.
pub struct TrafficWindow {
    slots: Box<[TrafficSlot]>,
    next: usize,
    filled: usize,
    started: Instant,
}

impl Default for TrafficWindow {
    fn default() -> Self {
        Self::new()
    }
}

impl TrafficWindow {
    /// Allocate the ring once, at start, at the full configured size.
    pub fn new() -> Self {
        Self {
            slots: vec![TrafficSlot::empty(); TRAFFIC_SLOTS].into_boxed_slice(),
            next: 0,
            filled: 0,
            started: Instant::now(),
        }
    }

    /// Record one logged event, overwriting the oldest slot when the ring is
    /// full.
    ///
    /// One copy into a slot that was allocated at start. No allocation and no
    /// system call, which is what keeps the cost of logging a request in
    /// memory instead of a file read.
    pub fn push(
        &mut self,
        at_us: u64,
        kind: Kind,
        status: u16,
        path: &str,
        client_ip: &str,
        user_agent: Option<&str>,
    ) {
        let slot = &mut self.slots[self.next];
        slot.at_us = at_us;
        slot.kind = kind;
        slot.status = status;
        slot.path.set(path);
        slot.client_ip.set(client_ip);
        match user_agent {
            Some(ua) => slot.user_agent.set(ua),
            None => slot.user_agent.mark_absent(),
        }
        self.next += 1;
        if self.next == self.slots.len() {
            self.next = 0;
        }
        if self.filled < self.slots.len() {
            self.filled += 1;
        }
    }

    /// The minutes the ring covers: the configured hour, or less since start.
    ///
    /// A report soon after a restart says how long it covers, so an empty
    /// figure is not read as an hour with no traffic.
    fn covered_minutes(&self) -> u64 {
        (self.started.elapsed().as_secs() / 60).min(TRAFFIC_WINDOW_MINUTES)
    }

    /// The recorded slots from oldest to newest.
    fn oldest_first(&self) -> impl Iterator<Item = &TrafficSlot> {
        let len = self.slots.len();
        let start = if self.filled == len { self.next } else { 0 };
        let slots = &self.slots;
        (0..self.filled).map(move |i| &slots[(start + i) % len])
    }
}

/// Unix microseconds, the precision the analytics envelope records.
pub(crate) fn unix_micros_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_micros() as u64)
}

/// Build the `/traffic` summary from the window.
///
/// The records of the last hour are turned back into the records the log
/// lines would have parsed to, and `TrafficReport::from_records` builds the
/// report body, so the figures match the ones a log read produces.
fn build_report(node: &str, window: &TrafficWindow) -> TrafficReport {
    let cutoff = unix_micros_now().saturating_sub(TRAFFIC_WINDOW_MINUTES * 60 * 1_000_000);
    let mut records: Vec<AnalyticsRecord> = Vec::with_capacity(window.filled);
    for slot in window.oldest_first() {
        if slot.at_us < cutoff {
            continue;
        }
        records.push(AnalyticsRecord::from_parts(
            slot.at_us,
            slot.kind.message(),
            slot.status,
            slot.path.as_str(),
            slot.client_ip.as_str(),
            slot.user_agent.as_opt(),
        ));
    }
    let mut report = TrafficReport::from_records(node, &records, window.covered_minutes());
    // Written by the m6-firewall-stats timer. A node without the timer
    // reports no firewall state rather than an empty one.
    report.firewall =
        m6_core::firewall::FirewallState::from_file(std::path::Path::new(FIREWALL_JSON))
            .unwrap_or(None);
    report
}

/// Serve `/traffic` to a caller [`metrics_gate`] has already let through:
/// this node's summary of its own traffic, built from the window in memory.
///
/// Gate first, then work, as for `/perf`, so an anonymous caller never causes
/// the report to be built.
///
/// The cache keeps the build and its allocation to at most one per `cache_for`,
/// however often the endpoint is scraped. It is a plain field of the loop's
/// state: one thread owns the loop, so there is no lock to take.
pub fn traffic_authorised(
    node: &str,
    window: &TrafficWindow,
    cache: &mut Option<(Instant, TrafficReport)>,
    cache_for: Duration,
) -> PerfOutcome {
    if let Some((built, report)) = cache.as_ref() {
        if built.elapsed() < cache_for {
            return PerfOutcome::Traffic(Box::new(report.clone()));
        }
    }
    let report = build_report(node, window);
    *cache = Some((Instant::now(), report.clone()));
    PerfOutcome::Traffic(Box::new(report))
}

/// The verdict of the metrics gate for one request.
pub enum Gate {
    /// Serve the report.
    Open,
    /// Answer with this and do nothing else.
    Refused(PerfOutcome),
}

/// Decide whether `/perf` or `/traffic` may be served to this request.
///
/// Two rules, in this order:
///
/// 1. `metrics_peers`, when set: the request must not have arrived on an
///    internal h2c listener (there the client address is a forwarded claim),
///    and its peer address must be listed. Otherwise `Disabled`, the 404 an
///    unconfigured endpoint gives, before any token is looked at.
/// 2. `metrics_require`, when set: an API token that passes the claim check
///    (`Unauthorised` for none or a bad one, `Forbidden` for the wrong claim).
///    When unset, the legacy `metrics_token_file` gate.
///
/// The address rule allocates nothing: one parse of the peer address and a
/// scan of a short slice, and only on these two paths.
pub fn metrics_gate(
    h: &HealthConfig,
    key: Option<&PublicKey>,
    headers: &[(String, String)],
    client_ip: &str,
    from_internal: bool,
) -> Gate {
    if let Some(peers) = &h.metrics_peer_ips {
        let listed = !from_internal
            && client_ip
                .parse::<IpAddr>()
                .is_ok_and(|ip| peers.contains(&ip.to_canonical()));
        if !listed {
            return Gate::Refused(PerfOutcome::Disabled);
        }
    }
    match h.metrics_require.as_deref() {
        Some(require) => {
            let Some(key) = key else {
                return Gate::Refused(PerfOutcome::Unauthorised);
            };
            match auth::authorise(key, headers, require) {
                Ok(_) => Gate::Open,
                Err(Refusal::Insufficient) => Gate::Refused(PerfOutcome::Forbidden),
                Err(Refusal::NoToken | Refusal::Invalid(_)) => {
                    Gate::Refused(PerfOutcome::Unauthorised)
                }
            }
        }
        None => match (PerfAccess {
            headers,
            configured_token: h.metrics_token.as_deref(),
        })
        .refusal()
        {
            Some(refused) => Gate::Refused(refused),
            None => Gate::Open,
        },
    }
}

#[cfg(test)]
mod traffic_endpoint_tests {
    use super::*;

    /// No token configured means the endpoint does not exist: a 404, so it
    /// does not advertise a door. A wrong token is refused by the gate,
    /// which runs before `traffic_authorised` and so before the report is
    /// built.
    #[test]
    fn the_legacy_gate_refuses_before_the_report_is_built() {
        let none = HealthConfig::default();
        let Gate::Refused(out) = metrics_gate(&none, None, &[], "127.0.0.1", false) else {
            panic!("no token configured must refuse");
        };
        assert_eq!(out.into_response().0, 404);

        let h = HealthConfig {
            metrics_token: Some("right".to_string()),
            ..HealthConfig::default()
        };
        let wrong = vec![("Authorization".to_string(), "Bearer wrong".to_string())];
        assert!(matches!(
            metrics_gate(&h, None, &wrong, "127.0.0.1", false),
            Gate::Refused(PerfOutcome::Unauthorised)
        ));
    }

    #[test]
    fn summarises_the_window_and_serves_it() {
        let mut window = TrafficWindow::new();
        let now = unix_micros_now();
        window.push(
            now - 2_000_000,
            Kind::Request,
            200,
            "/",
            "1.2.3.4",
            Some("Chrome/131"),
        );
        window.push(
            now - 1_000_000,
            Kind::Request,
            200,
            "/robots.txt",
            "5.6.7.8",
            Some("Mozilla/5.0 (compatible; ExampleBot/1.0)"),
        );

        let mut cache = None;
        let out = traffic_authorised("origin", &window, &mut cache, Duration::from_secs(0));
        let (code, headers, body) = out.into_response();
        assert_eq!(code, 200);
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["node"], "origin");
        assert_eq!(v["total_requests"], 2);
        assert!(v["crawlers"][0]["user_agent"]
            .as_str()
            .unwrap()
            .contains("ExampleBot"));
        assert!(v["logging"].is_object(), "logging health travels with it");
        // Never cached by anything in between.
        assert!(headers
            .iter()
            .any(|(k, val)| k.eq_ignore_ascii_case("cache-control") && val == "no-store"));
    }

    /// The age filter keeps the window to an hour: 61 minutes old is out,
    /// 59 is in.
    #[test]
    fn the_window_drops_records_older_than_an_hour() {
        let mut window = TrafficWindow::new();
        let now = unix_micros_now();
        window.push(
            now - 61 * 60 * 1_000_000,
            Kind::Request,
            404,
            "/old",
            "192.0.2.1",
            None,
        );
        window.push(
            now - 59 * 60 * 1_000_000,
            Kind::Request,
            200,
            "/new",
            "192.0.2.2",
            None,
        );
        let report = build_report("origin", &window);
        assert_eq!(report.total_requests, 1, "only the last hour counts");
        assert_eq!(report.status.get(&200), Some(&1));
        assert_eq!(report.status.get(&404), None);
    }

    /// A full ring keeps the newest events and drops the oldest, rather than
    /// stopping and reporting a frozen hour forever.
    #[test]
    fn the_ring_overwrites_the_oldest() {
        let mut window = TrafficWindow::new();
        let now = unix_micros_now();
        for _ in 0..10 {
            window.push(now, Kind::Request, 418, "/oldest", "192.0.2.1", None);
        }
        for _ in 0..TRAFFIC_SLOTS {
            window.push(now, Kind::Request, 200, "/kept", "192.0.2.2", None);
        }
        window.push(now, Kind::Request, 429, "/newest", "192.0.2.3", None);

        let report = build_report("origin", &window);
        assert_eq!(report.total_requests, TRAFFIC_SLOTS as u64);
        assert_eq!(
            report.status.get(&418),
            None,
            "the ten oldest slots were overwritten"
        );
        assert_eq!(
            report.status.get(&429),
            Some(&1),
            "the newest event is present"
        );
    }

    /// A path longer than its cap is cut on a character boundary. Cutting at
    /// the byte cap regardless can split a multi-byte character, and the slot
    /// would then not be UTF-8.
    #[test]
    fn a_long_path_is_cut_on_a_character_boundary() {
        let mut window = TrafficWindow::new();
        // "/" plus 300 two-byte characters: 601 bytes, and byte 512 falls in
        // the middle of a character.
        let path = format!("/{}", "é".repeat(300));
        window.push(
            unix_micros_now(),
            Kind::Request,
            200,
            &path,
            "192.0.2.1",
            None,
        );
        let stored = window.slots[0].path.as_str();
        assert!(stored.len() <= PATH_CAP, "stored {} bytes", stored.len());
        assert!(!stored.is_empty(), "the cut keeps as much as it can");
        assert!(
            path.starts_with(stored),
            "the stored value is a prefix of the request path"
        );
        assert!(
            std::str::from_utf8(stored.as_bytes()).is_ok(),
            "the cut left valid UTF-8"
        );
    }
}

#[cfg(test)]
mod metrics_gate_tests {
    use super::*;
    use crate::auth::test_keys::{bearer, keypair, mint};

    fn token(private_pem: &str, roles: &[&str]) -> Vec<(String, String)> {
        bearer(&mint(private_pem, roles, 3600))
    }

    fn health(peers: Option<&[&str]>, require: Option<&str>, legacy: Option<&str>) -> HealthConfig {
        HealthConfig {
            metrics_require: require.map(str::to_string),
            metrics_peer_ips: peers.map(|p| {
                p.iter()
                    .map(|s| s.parse::<IpAddr>().unwrap().to_canonical())
                    .collect()
            }),
            metrics_token: legacy.map(str::to_string),
            ..HealthConfig::default()
        }
    }

    fn is_disabled(g: &Gate) -> bool {
        matches!(g, Gate::Refused(PerfOutcome::Disabled))
    }

    #[test]
    fn an_unlisted_peer_is_404_before_any_token_check() {
        let (_d, key, private) = keypair();
        let h = health(Some(&["10.0.0.9"]), Some("role:monitor"), None);
        let good = token(&private, &["monitor"]);
        for ip in ["203.0.113.7", "2001:db8::1", "10.0.0.8"] {
            let g = metrics_gate(&h, Some(&key), &good, ip, false);
            assert!(is_disabled(&g), "{ip} must get 404 even with a valid token");
        }
        // No token and an unlisted peer is still 404, not 401: a stranger
        // learns nothing about the credential.
        assert!(is_disabled(&metrics_gate(
            &h,
            Some(&key),
            &[],
            "203.0.113.7",
            false
        )));
    }

    #[test]
    fn a_listed_peer_passes_the_address_rule() {
        let (_d, key, private) = keypair();
        let h = health(Some(&["10.0.0.9"]), Some("role:monitor"), None);
        let good = token(&private, &["monitor"]);
        assert!(matches!(
            metrics_gate(&h, Some(&key), &good, "10.0.0.9", false),
            Gate::Open
        ));
        // An IPv4-mapped IPv6 peer is the same host, so the comparison is canonical.
        assert!(matches!(
            metrics_gate(&h, Some(&key), &good, "::ffff:10.0.0.9", false),
            Gate::Open
        ));
    }

    #[test]
    fn an_unparseable_client_address_is_refused() {
        let (_d, key, private) = keypair();
        let h = health(Some(&["10.0.0.9"]), Some("role:monitor"), None);
        let good = token(&private, &["monitor"]);
        for ip in ["", "unknown", "10.0.0.9:443", "10.0.0.9, 1.2.3.4"] {
            assert!(
                is_disabled(&metrics_gate(&h, Some(&key), &good, ip, false)),
                "{ip:?}"
            );
        }
    }

    #[test]
    fn a_listed_peer_on_an_internal_listener_is_refused() {
        let (_d, key, private) = keypair();
        let h = health(Some(&["10.0.0.9"]), Some("role:monitor"), None);
        let good = token(&private, &["monitor"]);
        assert!(is_disabled(&metrics_gate(
            &h,
            Some(&key),
            &good,
            "10.0.0.9",
            true
        )));
    }

    #[test]
    fn a_listed_peer_with_a_valid_token_but_wrong_role_is_403() {
        let (_d, key, private) = keypair();
        let h = health(Some(&["10.0.0.9"]), Some("role:monitor"), None);
        let reader = token(&private, &["reader"]);
        assert!(matches!(
            metrics_gate(&h, Some(&key), &reader, "10.0.0.9", false),
            Gate::Refused(PerfOutcome::Forbidden)
        ));
    }

    #[test]
    fn metrics_require_without_a_loaded_key_fails_closed() {
        let h = health(None, Some("role:monitor"), None);
        assert!(matches!(
            metrics_gate(&h, None, &[], "10.0.0.9", false),
            Gate::Refused(PerfOutcome::Unauthorised)
        ));
    }

    #[test]
    fn metrics_require_refuses_the_legacy_bearer_string() {
        let (_d, key, _) = keypair();
        let h = health(None, Some("role:monitor"), Some("legacy-secret"));
        let legacy = vec![(
            "authorization".to_string(),
            "Bearer legacy-secret".to_string(),
        )];
        assert!(matches!(
            metrics_gate(&h, Some(&key), &legacy, "10.0.0.9", false),
            Gate::Refused(PerfOutcome::Unauthorised)
        ));
    }

    #[test]
    fn without_new_keys_the_gate_is_todays() {
        let h = health(None, None, Some("right"));
        let ok = vec![("authorization".to_string(), "Bearer right".to_string())];
        let bad = vec![("authorization".to_string(), "Bearer wrong".to_string())];
        // Any peer, internal or not: there is no address rule.
        for (ip, internal) in [("203.0.113.7", false), ("10.0.0.9", true)] {
            assert!(matches!(
                metrics_gate(&h, None, &ok, ip, internal),
                Gate::Open
            ));
            assert!(matches!(
                metrics_gate(&h, None, &bad, ip, internal),
                Gate::Refused(PerfOutcome::Unauthorised)
            ));
        }
        let none = health(None, None, None);
        assert!(is_disabled(&metrics_gate(
            &none, None, &ok, "10.0.0.9", false
        )));
    }
}
