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

use crate::auth::{self, Decision, PublicKey};
use crate::config::{GenidConfig, HealthConfig};
use crate::stats::Iface;

pub use m6_core::monitoring::{HealthReport, PerfOutcome, PerfReport, PerfSubject, PoolHealth};

/// Backend name tagged onto a `/health` response.
pub const HEALTH_BACKEND: &str = "health";
/// Backend name tagged onto a `/ready` response.
pub const READY_BACKEND: &str = "ready";
/// Backend name tagged onto a `/perf` response.
pub const PERF_BACKEND: &str = "perf";
/// Backend name tagged onto a `/genid` response, for stats and analytics.
pub const GENID_BACKEND: &str = "genid";

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
    backend == HEALTH_BACKEND || backend == READY_BACKEND || backend == PERF_BACKEND
}

/// Which population a request on a public listener belongs to, from the peer
/// address the connection proved.
///
/// The TLS and QUIC listener faces the internet, so a real visitor's address
/// is public after NAT. The monitor runs on the private network, and its polls
/// arrive from a private, link-local, shared or unique-local address. That
/// address class is the signal this returns: [`Iface::Monitor`] for such a
/// peer and [`Iface::External`] for every other one. A connect from a private
/// address cannot be forged from the internet, so no header is read and no
/// list is consulted.
///
/// Loopback is external on purpose. It is the address of a shell on this host,
/// a development client and every loopback test, so counting it as monitor
/// would hide the traffic of the machine the server runs on. A health check on
/// loopback is classified by its path instead, which every monitoring endpoint
/// already is.
///
/// The internal h2c listener is classified by the socket instead, before this
/// is called. Classification is one address parse and a range check: no
/// allocation, and no table whose lookup grows with traffic.
pub fn peer_iface(peer: IpAddr) -> Iface {
    let peer = peer.to_canonical();
    if !peer.is_loopback() && m6_core::net::is_private_ip(peer) {
        Iface::Monitor
    } else {
        Iface::External
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
        assert!(is_monitoring_endpoint(READY_BACKEND));
        assert!(is_monitoring_endpoint(PERF_BACKEND));
        assert!(is_monitoring_endpoint("health"));
        assert!(is_monitoring_endpoint("ready"));
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
// The one marker for a value a cap cut short. The slot and the report use the
// same value, so the two cuts cannot drift apart.
use m6_core::telemetry::USER_AGENT_TRUNCATED as TRUNCATED;

/// Where the privileged collector leaves `nft -j list ruleset` output.
const FIREWALL_JSON: &str = "/var/lib/m6/firewall.json";

/// The most events `/traffic` keeps. Allocated once at start.
const TRAFFIC_SLOTS: usize = 32_768;

/// How far back `/traffic` reports, in minutes.
///
/// The ring holds far more than an hour at the busiest rate observed (about
/// 1,000 lines in an hour), so the hour is applied when the report is built
/// rather than when an event is kept. A node busy enough to wrap the ring
/// inside the hour reports the shorter span it still holds and counts what it
/// lost.
const TRAFFIC_WINDOW_MINUTES: u64 = 60;

/// Microseconds in one minute.
const MICROS_PER_MINUTE: u64 = 60 * 1_000_000;

/// The reporting window in microseconds.
const WINDOW_US: u64 = TRAFFIC_WINDOW_MINUTES * MICROS_PER_MINUTE;

/// One drop cell per minute of the window plus the minute in progress.
const DROP_CELLS: usize = TRAFFIC_WINDOW_MINUTES as usize + 1;

/// The most bytes a slot keeps of a path, marker included. The report shows
/// the cut path, the log keeps the whole one.
const PATH_CAP: usize = 512;

/// The most bytes a slot keeps of a client address. An IPv6 address is at most
/// 45 characters.
const IP_CAP: usize = 45;

/// The most bytes a slot keeps of a user agent, marker included.
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
    ///
    /// A value the cap cuts ends in [`TRUNCATED`], so every reader of a slot
    /// can tell a cut value from a whole one. The marker reserves its own
    /// bytes inside the cap. The return value is the machine-readable form of
    /// the same fact, which the report needs because the marker is a rendering
    /// choice and a cut field can hide a probe marker.
    fn set(&mut self, s: &str) -> bool {
        if s.len() <= N {
            self.bytes[..s.len()].copy_from_slice(s.as_bytes());
            self.len = s.len() as u16;
            return false;
        }
        let mut end = N - TRUNCATED.len();
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        self.bytes[..end].copy_from_slice(&s.as_bytes()[..end]);
        self.bytes[end..end + TRUNCATED.len()].copy_from_slice(TRUNCATED.as_bytes());
        self.len = (end + TRUNCATED.len()) as u16;
        true
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
/// fields are the ones the report reads, plus the time, the kind and whether a
/// cap cut the event.
#[derive(Clone)]
struct TrafficSlot {
    at_us: u64,
    kind: Kind,
    status: u16,
    path: Field<PATH_CAP>,
    client_ip: Field<IP_CAP>,
    user_agent: Field<UA_CAP>,
    /// A cap cut the path or the user agent.
    ///
    /// The slot keeps the marker on the cut value, and this bit beside it is
    /// what the report reads. A cut field can hide a probe marker past the
    /// cap, so the request counts as probe-shaped. The bit is assigned on
    /// every push, so a reused slot never carries the previous event's answer.
    truncated: bool,
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
            truncated: false,
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
/// is applied when the report is built. When the ring wraps inside the hour,
/// the report says which span it still holds and how many events it dropped.
pub struct TrafficWindow {
    slots: Box<[TrafficSlot]>,
    next: usize,
    filled: usize,
    started: Instant,
    /// Events overwritten while still inside the window, one cell per minute
    /// of the event's timestamp. A cell counts one minute only, so a later
    /// event in the same minute slot replaces a stale count. Nothing allocates
    /// and nothing expires on a timer.
    drops: [DropCell; DROP_CELLS],
}

/// The events dropped in one minute of event time.
#[derive(Clone, Copy)]
struct DropCell {
    /// Minutes since the Unix epoch, or `NONE` for a cell never written.
    minute: u64,
    count: u64,
}

impl DropCell {
    const NONE: u64 = u64::MAX;
    const EMPTY: Self = Self {
        minute: Self::NONE,
        count: 0,
    };
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
            started: m6_core::platform::now(),
            drops: [DropCell::EMPTY; DROP_CELLS],
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
        let overwritten_at_us = slot.at_us;
        slot.at_us = at_us;
        slot.kind = kind;
        slot.status = status;
        let path_cut = slot.path.set(path);
        slot.client_ip.set(client_ip);
        let user_agent_cut = match user_agent {
            Some(ua) => slot.user_agent.set(ua),
            None => {
                slot.user_agent.mark_absent();
                false
            }
        };
        slot.truncated = path_cut || user_agent_cut;
        self.next += 1;
        if self.next == self.slots.len() {
            self.next = 0;
        }
        if self.filled < self.slots.len() {
            self.filled += 1;
        } else {
            self.count_drop(overwritten_at_us, at_us);
        }
    }

    /// Count an event the ring just overwrote, if it was still in the window.
    ///
    /// `now_us` is the pushed event's time: the overwritten event leaves the
    /// ring at the same moment, so it belonged in a report only when it was
    /// not already older than the window.
    fn count_drop(&mut self, dropped_at_us: u64, now_us: u64) {
        if dropped_at_us < now_us.saturating_sub(WINDOW_US) {
            return;
        }
        let minute = dropped_at_us / MICROS_PER_MINUTE;
        let cell = &mut self.drops[(minute % DROP_CELLS as u64) as usize];
        if cell.minute != minute {
            cell.minute = minute;
            cell.count = 0;
        }
        cell.count += 1;
    }

    /// The minutes the ring covers: the configured hour, less the time since
    /// start, less the span the ring still holds once it has wrapped.
    ///
    /// The last term keeps a full ring honest. Overwriting events that are
    /// still inside the hour means the report no longer covers the hour, and
    /// the age of the oldest retained slot is the span it does cover. A report
    /// soon after a restart says how long it covers, so an empty figure is not
    /// read as an hour with no traffic.
    fn covered_minutes(&self, now_us: u64) -> u64 {
        let elapsed = (self.started.elapsed().as_secs() / 60).min(TRAFFIC_WINDOW_MINUTES);
        if self.filled < self.slots.len() {
            return elapsed;
        }
        let Some(oldest) = self.oldest_first().next() else {
            return elapsed;
        };
        elapsed.min(now_us.saturating_sub(oldest.at_us) / MICROS_PER_MINUTE)
    }

    /// Events the ring overwrote while they were still inside the window.
    ///
    /// The count is by the minute of the overwritten event, and a cell older
    /// than the window is left out, so a flood an hour ago is not reported
    /// against a quiet hour now. The report states it, so a short window is
    /// not read as a quiet one.
    fn dropped_in_window(&self, now_us: u64) -> u64 {
        let first_minute = now_us.saturating_sub(WINDOW_US) / MICROS_PER_MINUTE;
        self.drops
            .iter()
            .filter(|c| c.minute != DropCell::NONE && c.minute >= first_minute)
            .map(|c| c.count)
            .sum()
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
    m6_core::platform::wall()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_micros() as u64)
}

/// Build the `/traffic` summary from the window.
///
/// The records of the last hour are turned back into the records the log
/// lines would have parsed to, and `TrafficReport::from_records` builds the
/// report body, so the figures match the ones a log read produces. A value the
/// slot cut is the one difference, and `truncated` tells the record, because a
/// cut field can hide a probe marker. The report also states how long a window
/// it actually covers and how many events the ring overwrote inside that
/// window.
fn build_report(node: &str, window: &TrafficWindow) -> TrafficReport {
    let now_us = unix_micros_now();
    let cutoff = now_us.saturating_sub(WINDOW_US);
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
            slot.truncated,
        ));
    }
    let mut report = TrafficReport::from_records(node, &records, window.covered_minutes(now_us));
    report.events_dropped = window.dropped_in_window(now_us);
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
    *cache = Some((m6_core::platform::now(), report.clone()));
    PerfOutcome::Traffic(Box::new(report))
}

/// The verdict of the metrics gate for one request.
pub enum Gate {
    /// Serve the report.
    Open,
    /// Answer with this exact status, headers and body and do nothing else:
    /// the 404 a disabled endpoint gives, or the token check's refusal.
    Refused(u16, Vec<(String, String)>, Vec<u8>),
}

impl Gate {
    /// The 404 an endpoint answers when it is not configured.
    fn disabled() -> Self {
        let (code, headers, body) = PerfOutcome::Disabled.into_response();
        Gate::Refused(code, headers, body)
    }

    /// The one token check's refusal, as the wire answer.
    fn refusal(decision: Decision) -> Self {
        let (code, headers, body) = decision.refusal().expect("a refusal, by construction");
        Gate::Refused(code, headers, body)
    }
}

/// Decide whether `/perf` or `/traffic` may be served to this request.
///
/// Two rules, in this order:
///
/// 1. `metrics_peers`, when set: the request must not have arrived on an
///    internal h2c listener (there the client address is a forwarded claim),
///    and its peer address must be listed. Otherwise the 404 an unconfigured
///    endpoint gives, before any token is looked at.
/// 2. `metrics_require`, when set: the one token check decides. It is called
///    once, here, and its refusal is the answer unchanged. When unset the
///    endpoint is off, as it is with no token at all.
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
            return Gate::disabled();
        }
    }
    let Some(require) = h.metrics_require.as_deref() else {
        return Gate::disabled();
    };
    match auth::check(key, headers, require) {
        Decision::Allow(_) => Gate::Open,
        // Spelled out, not a catch-all. When this return type becomes the core
        // four-variant `Decision`, `NoDecision` must be a compile error here,
        // and its answer is a 503 rather than `Gate::refusal`'s panic.
        refusal @ (Decision::NoCredential | Decision::Deny) => Gate::refusal(refusal),
    }
}

/// Decide whether a `GET` or `POST /genid` may be served to this request.
///
/// Two rules, in this order, the shape [`metrics_gate`] uses:
///
/// 1. The peer is loopback, or the request arrived on an internal listener.
///    Anyone else gets the 404 an unknown path gives, before a token is read.
///    Neither claim set is the same 404: the endpoint does not exist.
/// 2. The one token check decides with `read_require` for a `GET` or
///    `write_require` for a `POST`. It is called once, here, and its refusal
///    is the answer unchanged. Unconfigured is the same 404.
pub fn genid_gate(
    g: &GenidConfig,
    key: Option<&PublicKey>,
    headers: &[(String, String)],
    client_ip: &str,
    from_internal: bool,
    write: bool,
) -> Gate {
    if !genid_address_ok(client_ip, from_internal) {
        return Gate::disabled();
    }
    let require = if write {
        g.write_require.as_deref()
    } else {
        g.read_require.as_deref()
    };
    let Some(require) = require else {
        return Gate::disabled();
    };
    match auth::check(key, headers, require) {
        Decision::Allow(_) => Gate::Open,
        // Spelled out for the same reason as `metrics_gate`: a fourth variant
        // must be a compile error here and must never reach the `expect`.
        refusal @ (Decision::NoCredential | Decision::Deny) => Gate::refusal(refusal),
    }
}

/// The address rule alone: loopback, or an internal listener.
///
/// A cache node forwards the caller's own token to the source, so it applies
/// this rule and nothing else. A public peer still gets the 404 an unknown
/// path gives, before the request is forwarded.
pub fn genid_address_ok(client_ip: &str, from_internal: bool) -> bool {
    let loopback = client_ip
        .parse::<IpAddr>()
        .is_ok_and(|ip| ip.to_canonical().is_loopback());
    from_internal || loopback
}

/// The authorised `/perf` answer with this node's generation block added.
///
/// The block is inserted into the report this node already built, so `m6-core`
/// keeps no notion of a generation: it hands over the monitoring payload and
/// m6-http adds the one field that is this binary's. A source reports the ID it
/// owns and the peers that read it. A cache node reports the ID it follows,
/// where it follows it from, and whether that upstream is answering.
/// The disk-cache figures `/perf` adds, gathered so the response keeps one
/// argument for them: the writer's counters, the load's counts, and the first
/// heartbeat's decision.
pub struct DiskPerf<'a> {
    pub writer: &'a crate::disk_cache::DiskWriter,
    pub queued: usize,
    pub loaded_entries: usize,
    pub loaded_bad: usize,
    pub state: crate::disk_cache::LoadState,
}

pub fn genid_perf_response(
    report: PerfReport,
    role: &crate::genid::Role,
    config: &GenidConfig,
    disk: Option<DiskPerf<'_>>,
) -> (u16, Vec<(String, String)>, Vec<u8>) {
    let mut value = serde_json::to_value(&report)
        .unwrap_or_else(|_| serde_json::json!({"error": "serialisation failed"}));
    if let Some(object) = value.as_object_mut() {
        object.insert("genid".to_string(), genid_block(role, config));
        if let Some(disk) = disk {
            let writer = disk.writer;
            object.insert(
                "disk_cache".to_string(),
                serde_json::json!({
                    "dir_bytes": writer.dir_bytes(),
                    "queued": disk.queued,
                    "written_last_tick": writer.written_last_tick(),
                    "written_bytes_last_tick": writer.written_bytes_last_tick(),
                    "skipped_full": writer.skipped_full(),
                    "ticks": writer.ticks(),
                    "tick_last_ns": writer.tick_last_ns(),
                    "loaded_entries": disk.loaded_entries,
                    "loaded_bad": disk.loaded_bad,
                    "state": disk.state.as_str(),
                }),
            );
        }
    }
    let body = serde_json::to_vec(&value)
        .unwrap_or_else(|_| br#"{"error":"serialisation failed"}"#.to_vec());
    (
        200,
        vec![
            (
                "Content-Type".to_string(),
                "application/json; charset=utf-8".to_string(),
            ),
            ("Cache-Control".to_string(), "no-store".to_string()),
            ("X-Robots-Tag".to_string(), "noindex, nofollow".to_string()),
        ],
        body,
    )
}

/// The `genid` object `/perf` adds, by role.
fn genid_block(role: &crate::genid::Role, config: &GenidConfig) -> serde_json::Value {
    match role {
        crate::genid::Role::Source(g) => {
            let peers: Vec<serde_json::Value> = g
                .peers(m6_core::platform::now())
                .into_iter()
                .map(|(ip, last_get_s)| {
                    serde_json::json!({"addr": ip.to_string(), "last_get_s": last_get_s})
                })
                .collect();
            serde_json::json!({
                "role": "source",
                "id": g.current().to_string(),
                "sequence_running": g.sequence_running(),
                "peers": peers,
            })
        }
        crate::genid::Role::Cache(f) => {
            let now = m6_core::platform::now();
            serde_json::json!({
                "role": "cache",
                "id": f.current().map(|id| id.to_string()),
                "upstream": config.upstream.as_deref().unwrap_or(""),
                "upstream_down": f.down(),
                "upstream_rtt_ms": f.rtt().map(|rtt| rtt.as_secs_f64() * 1000.0),
                "last_ok_s": f.last_ok_s(now),
            })
        }
    }
}

// ── /ready ───────────────────────────────────────────────────────────────────
//
// Readiness is a second answer, not a different first one. `/health` answers
// for the m6-http process and keeps answering 200 while it runs. `/ready`
// answers for a request that may need the origin, and a load balancer reads its
// status code because that is what a health check consumes. The two answers
// differ by construction rather than by a field: `/health` carries no origin
// state at all.

/// The path `/ready` answers on.
///
/// Fixed rather than a `[health]` key. `/health` has a path key because a site
/// may want to serve its own page at that name. No site owns an answer about
/// whether this node can reach its origin, so the decision gives the endpoint
/// one public name and no config surface of its own.
pub const READY_PATH: &str = "/ready";

/// How many heartbeats without a successful answer leave a cache not ready.
///
/// The window and not the last failure decides the status, so a single failed
/// heartbeat cannot pull a node whose cache still serves, and a heartbeat that
/// never returns cannot leave the answer ready past the age. It derives from
/// `[genid] heartbeat_secs`, so there is no second interval to configure.
const READY_WINDOW_HEARTBEATS: u32 = 2;

/// The most bytes of the node name the body keeps.
///
/// A node name is operator config and the answer is public. Cutting it keeps
/// the body in one fixed buffer, and every name a deployment uses is far
/// shorter than this.
const READY_NODE_CAP: usize = 64;

/// The body buffer, on the stack. The fixed text is under 100 bytes, the reason
/// is a short static string and the name is capped at 64, so the worst case
/// fits with room to spare.
const READY_BODY_CAP: usize = 512;

/// Where this node's readiness stands, as `GET /ready` publishes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ready {
    /// 200 when a cache can still serve a request that may need the origin,
    /// 503 when it cannot. A source is always 200.
    pub code: u16,
    /// `up`, `down`, `unknown` or `not_applicable`. The status is the answer.
    /// This names the origin state the answer came from.
    pub origin: &'static str,
    /// The follower's last failure reason, when the answer is not ready.
    ///
    /// One `&'static str` already produced by `Follower::answered`, so
    /// formatting the answer needs no second source of truth and no allocation
    /// for a message.
    pub reason: Option<&'static str>,
}

/// Decide `GET /ready` from state the loop already holds.
///
/// The role comes from config, decided once at start: `[genid] upstream` builds
/// `Role::Cache` and no upstream builds `Role::Source`, so a node with no
/// origin is known to be one rather than guessed from a missing timestamp. A
/// source answers 200 `not_applicable`.
///
/// A cache is ready while its last successful answer is no older than
/// `2 * heartbeat`. That reads `last_ok`, `down` and the last failure reason,
/// and asks the origin nothing. A cache that has never had a successful answer
/// is 503: it holds no ID, so it can serve neither a hit under one nor a miss.
pub fn readiness(role: &crate::genid::Role, now: Instant) -> Ready {
    let Some(follower) = role.cache() else {
        return Ready {
            code: 200,
            origin: "not_applicable",
            reason: None,
        };
    };
    let Some(last_ok) = follower.last_ok() else {
        return Ready {
            code: 503,
            origin: if follower.down() { "down" } else { "unknown" },
            reason: follower.last_failure(),
        };
    };
    let window = follower.heartbeat().saturating_mul(READY_WINDOW_HEARTBEATS);
    if now.saturating_duration_since(last_ok) > window {
        return Ready {
            code: 503,
            origin: "down",
            reason: Some(
                follower
                    .last_failure()
                    .unwrap_or("no answer within two heartbeats"),
            ),
        };
    }
    Ready {
        code: 200,
        origin: if follower.down() { "down" } else { "up" },
        reason: None,
    }
}

/// A fixed stack buffer the body is formatted into.
///
/// The readiness decision allocates nothing. The body is written here and
/// copied once into the response's owned bytes, because `RequestOutcome::Ready`
/// carries owned parts. That one copy is the same single body allocation
/// `/health` makes, and it is the only one on this path.
struct StackBuf<const N: usize> {
    bytes: [u8; N],
    len: usize,
}

impl<const N: usize> StackBuf<N> {
    fn new() -> Self {
        Self {
            bytes: [0; N],
            len: 0,
        }
    }

    fn as_bytes(&self) -> &[u8] {
        &self.bytes[..self.len]
    }
}

impl<const N: usize> std::fmt::Write for StackBuf<N> {
    fn write_str(&mut self, s: &str) -> std::fmt::Result {
        let room = N - self.len;
        if s.len() > room {
            // The caller's budget makes this unreachable. Cut on a character
            // boundary rather than split one, and stop the format rather than
            // pad the tail with a plausible-looking body.
            let mut end = room;
            while end > 0 && !s.is_char_boundary(end) {
                end -= 1;
            }
            self.bytes[self.len..self.len + end].copy_from_slice(&s.as_bytes()[..end]);
            self.len += end;
            return Err(std::fmt::Error);
        }
        self.bytes[self.len..self.len + s.len()].copy_from_slice(s.as_bytes());
        self.len += s.len();
        Ok(())
    }

    fn write_char(&mut self, c: char) -> std::fmt::Result {
        let mut buf = [0u8; 4];
        self.write_str(c.encode_utf8(&mut buf))
    }
}

/// Write `s` as a JSON string, escaping what JSON requires.
fn write_json_string(w: &mut impl std::fmt::Write, s: &str) {
    let _ = w.write_char('"');
    for c in s.chars() {
        match c {
            '"' => {
                let _ = w.write_str("\\\"");
            }
            '\\' => {
                let _ = w.write_str("\\\\");
            }
            c if (c as u32) < 0x20 => {
                let _ = write!(w, "\\u{:04x}", c as u32);
            }
            c => {
                let _ = w.write_char(c);
            }
        }
    }
    let _ = w.write_char('"');
}

/// The node name the body carries, cut on a character boundary past the cap.
fn capped_node(node: &str) -> &str {
    if node.len() <= READY_NODE_CAP {
        return node;
    }
    let mut end = READY_NODE_CAP;
    while !node.is_char_boundary(end) {
        end -= 1;
    }
    &node[..end]
}

/// The readiness answer: its status, headers and body.
///
/// `no-store` for the reason `/health` uses it: a stale readiness answer is
/// worse than none. The body names the node and the origin state, and on a 503
/// the follower's failure reason.
pub fn ready_response(node: &str, ready: Ready) -> (u16, Vec<(String, String)>, Vec<u8>) {
    use std::fmt::Write as _;

    let mut body = StackBuf::<READY_BODY_CAP>::new();
    let status = if ready.code == 200 {
        "ready"
    } else {
        "not_ready"
    };
    let _ = write!(body, "{{\"status\":\"{status}\",\"node\":");
    write_json_string(&mut body, capped_node(node));
    let _ = body.write_str(",\"origin\":");
    write_json_string(&mut body, ready.origin);
    if let Some(reason) = ready.reason {
        let _ = body.write_str(",\"reason\":");
        write_json_string(&mut body, reason);
    }
    let _ = body.write_str("}");
    let body = body.as_bytes().to_vec();
    let headers = vec![
        (
            "Content-Type".to_string(),
            "application/json; charset=utf-8".to_string(),
        ),
        ("Cache-Control".to_string(), "no-store".to_string()),
        ("X-Robots-Tag".to_string(), "noindex, nofollow".to_string()),
    ];
    (ready.code, headers, body)
}

#[cfg(test)]
mod ready_tests {
    use super::*;
    use crate::genid::{Follower, Genid, Role};
    use std::time::Duration;

    fn cache(heartbeat: Duration) -> (Role, Instant) {
        let t0 = m6_core::platform::now();
        (Role::Cache(Follower::new(heartbeat, t0)), t0)
    }

    /// A node with no origin is known to be one from config, so it is ready
    /// and says why the origin question does not apply. Reading a missing
    /// `last_ok` as not ready instead would take every source out of rotation.
    #[test]
    fn a_source_is_ready_and_has_no_origin() {
        let role = Role::Source(Genid::new(0, None));
        let ready = readiness(&role, m6_core::platform::now());
        assert_eq!(
            ready,
            Ready {
                code: 200,
                origin: "not_applicable",
                reason: None,
            }
        );
    }

    /// A cache whose origin answered inside the window is ready.
    #[test]
    fn a_cache_with_a_live_origin_is_ready() {
        let (mut role, t0) = cache(Duration::from_secs(10));
        role.cache_mut()
            .unwrap()
            .answered(Ok((1, Duration::from_millis(2))), t0);
        assert_eq!(
            readiness(&role, t0 + Duration::from_secs(19)),
            Ready {
                code: 200,
                origin: "up",
                reason: None,
            }
        );
    }

    /// Past two heartbeats the node cannot serve a miss, and the body carries
    /// the failure the follower already named.
    #[test]
    fn a_cache_past_the_window_is_not_ready_and_carries_the_reason() {
        let (mut role, t0) = cache(Duration::from_secs(10));
        let f = role.cache_mut().unwrap();
        f.answered(Ok((1, Duration::from_millis(2))), t0);
        f.sent();
        f.answered(Err("connect failed"), t0 + Duration::from_secs(10));
        assert_eq!(
            readiness(&role, t0 + Duration::from_secs(21)),
            Ready {
                code: 503,
                origin: "down",
                reason: Some("connect failed"),
            }
        );
    }

    /// One failed heartbeat inside the window keeps a cache ready, and the
    /// body still names the origin down: the window delays the status, it does
    /// not hide the state. A single failure cannot pull a node whose cache is
    /// one heartbeat old.
    #[test]
    fn a_failed_heartbeat_inside_the_window_stays_ready() {
        let (mut role, t0) = cache(Duration::from_secs(10));
        let f = role.cache_mut().unwrap();
        f.answered(Ok((1, Duration::from_millis(2))), t0);
        f.sent();
        f.answered(Err("connect failed"), t0 + Duration::from_secs(10));
        assert_eq!(
            readiness(&role, t0 + Duration::from_secs(15)),
            Ready {
                code: 200,
                origin: "down",
                reason: None,
            }
        );
    }

    /// At the window the node is still ready: the boundary is two whole
    /// heartbeats, not less.
    #[test]
    fn the_window_is_inclusive_and_a_heartbeat_late_is_not() {
        let (mut role, t0) = cache(Duration::from_secs(10));
        role.cache_mut()
            .unwrap()
            .answered(Ok((1, Duration::ZERO)), t0);
        assert_eq!(readiness(&role, t0 + Duration::from_secs(20)).code, 200);
        assert_eq!(readiness(&role, t0 + Duration::from_secs(21)).code, 503);
    }

    /// A cache with no successful answer holds no ID, so it is not ready even
    /// before a heartbeat has failed.
    #[test]
    fn a_cache_that_never_answered_is_not_ready() {
        let (role, _) = cache(Duration::from_secs(10));
        let ready = readiness(&role, m6_core::platform::now());
        assert_eq!(ready.code, 503);
        assert_eq!(ready.origin, "unknown");
    }

    /// The body is JSON naming the node, the origin state and the reason,
    /// formatted into the fixed buffer rather than through serde.
    #[test]
    fn the_body_names_the_node_and_the_origin() {
        let (code, headers, body) = ready_response(
            "edge-a",
            Ready {
                code: 503,
                origin: "down",
                reason: Some("connect failed"),
            },
        );
        assert_eq!(code, 503);
        assert_eq!(
            std::str::from_utf8(&body).unwrap(),
            r#"{"status":"not_ready","node":"edge-a","origin":"down","reason":"connect failed"}"#
        );
        assert!(headers
            .iter()
            .any(|(k, v)| k == "Cache-Control" && v == "no-store"));
    }

    /// A ready source body is the same shape with no reason.
    #[test]
    fn a_ready_body_has_no_reason() {
        let (code, _, body) = ready_response(
            "origin",
            Ready {
                code: 200,
                origin: "not_applicable",
                reason: None,
            },
        );
        assert_eq!(code, 200);
        assert_eq!(
            std::str::from_utf8(&body).unwrap(),
            r#"{"status":"ready","node":"origin","origin":"not_applicable"}"#
        );
    }
}

#[cfg(test)]
mod traffic_endpoint_tests {
    use super::*;

    /// No token configured means the endpoint does not exist: a 404, so it
    /// does not advertise a door. A wrong credential is refused by the one
    /// token check, which runs before `traffic_authorised` and so before the
    /// report is built.
    #[test]
    fn a_disabled_endpoint_refuses_before_the_report_is_built() {
        let none = HealthConfig::default();
        let Gate::Refused(code, _, _) = metrics_gate(&none, None, &[], "127.0.0.1", false) else {
            panic!("no token configured must refuse");
        };
        assert_eq!(code, 404);

        let h = HealthConfig {
            metrics_require: Some("role:monitor".to_string()),
            ..HealthConfig::default()
        };
        let wrong = vec![("Authorization".to_string(), "Bearer wrong".to_string())];
        let Gate::Refused(code, _, _) = metrics_gate(&h, None, &wrong, "127.0.0.1", false) else {
            panic!("no key loaded must fail closed");
        };
        assert_eq!(code, 401);
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
            "192.0.2.10",
            Some("Chrome/131"),
        );
        window.push(
            now - 1_000_000,
            Kind::Request,
            200,
            "/robots.txt",
            "192.0.2.11",
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

    /// A ring that wraps inside the hour reports the span it still holds and
    /// the number of events it overwrote while they were still inside the
    /// hour, rather than claiming a full 60 minutes over a short total.
    #[test]
    fn a_full_ring_reports_the_span_it_holds_and_what_it_dropped() {
        let now = unix_micros_now();
        let mut window = TrafficWindow {
            slots: vec![TrafficSlot::empty(); TRAFFIC_SLOTS].into_boxed_slice(),
            next: 0,
            filled: 0,
            // An hour since start, so the span is the ring's, not elapsed time.
            started: m6_core::platform::now() - Duration::from_secs(3600),
            drops: [DropCell::EMPTY; DROP_CELLS],
        };
        // Fill the ring over 50 minutes and 30 seconds, leaving a margin over
        // 50 whole minutes after the overwrites below.
        let span_us = 50 * MICROS_PER_MINUTE + 30 * 1_000_000;
        let step = span_us / (TRAFFIC_SLOTS as u64 - 1);
        for i in 0..TRAFFIC_SLOTS {
            window.push(
                now - span_us + i as u64 * step,
                Kind::Request,
                200,
                "/kept",
                "192.0.2.1",
                None,
            );
        }
        // Every one of these overwrites an event still inside the hour.
        for _ in 0..10 {
            window.push(now, Kind::Request, 200, "/kept", "192.0.2.1", None);
        }

        let report = build_report("origin", &window);
        assert_eq!(
            report.events_dropped, 10,
            "ten events were overwritten inside the hour"
        );
        assert_eq!(
            report.window_minutes, 50,
            "the ring holds 50 minutes, not the 60 it claims"
        );
    }

    /// An overwrite of an event that had already left the hour is not counted
    /// against the hour, so the count does not grow for the life of the
    /// process.
    #[test]
    fn drops_older_than_the_window_are_not_counted() {
        let now = unix_micros_now();
        let mut window = TrafficWindow {
            slots: vec![TrafficSlot::empty(); TRAFFIC_SLOTS].into_boxed_slice(),
            next: 0,
            filled: 0,
            started: m6_core::platform::now() - Duration::from_secs(7200),
            drops: [DropCell::EMPTY; DROP_CELLS],
        };
        // 70 minutes of events, so the ten overwritten below are older than
        // the hour.
        let span_us = 70 * MICROS_PER_MINUTE;
        let step = span_us / (TRAFFIC_SLOTS as u64 - 1);
        for i in 0..TRAFFIC_SLOTS {
            window.push(
                now - span_us + i as u64 * step,
                Kind::Request,
                200,
                "/kept",
                "192.0.2.1",
                None,
            );
        }
        for _ in 0..10 {
            window.push(now, Kind::Request, 200, "/kept", "192.0.2.1", None);
        }

        let report = build_report("origin", &window);
        assert_eq!(
            report.events_dropped, 0,
            "the overwritten events were already outside the hour"
        );
        assert_eq!(report.window_minutes, 60, "the ring holds over an hour");
    }

    /// A drop cell is reported for one window and then forgotten. The cell
    /// stays in the ring until a later event reuses its minute 61 minutes on,
    /// so the read-time filter is what keeps a past burst out of a quiet hour.
    /// `dropped_in_window` takes the time, so this needs no sleep.
    #[test]
    fn a_drop_leaves_the_count_once_the_window_passes_it() {
        let mut window = TrafficWindow::new();
        let now = unix_micros_now();
        // One drop inside the window and one at the oldest minute the window
        // still covers. The report includes an event exactly at the cutoff, so
        // the edge drop counts too.
        window.count_drop(now, now);
        window.count_drop(now - WINDOW_US, now);
        assert_eq!(
            window.dropped_in_window(now),
            2,
            "both drops are inside the window"
        );
        assert_eq!(
            window.dropped_in_window(now + 61 * MICROS_PER_MINUTE),
            0,
            "a past burst's drops are not reported against a quiet hour"
        );
    }

    /// A path longer than its cap is cut on a character boundary and marked as
    /// cut. Cutting at the byte cap regardless can split a multi-byte
    /// character, and the slot would then not be UTF-8.
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
        let kept = stored
            .strip_suffix(TRUNCATED)
            .expect("a cut path ends in the marker");
        assert!(!kept.is_empty(), "the cut keeps as much as it can");
        assert!(
            path.starts_with(kept),
            "the stored value is a prefix of the request path"
        );
    }

    /// A user agent the slot cap cut is marked where the report reads it, so
    /// the report's 160-character cut cannot mistake the stored prefix for a
    /// short agent and present it as whole.
    #[test]
    fn a_cut_user_agent_is_marked_in_the_report() {
        let mut window = TrafficWindow::new();
        // 200 two-byte characters is 400 bytes, so the 256-byte cap keeps 126
        // characters: fewer than the report's 160-character mark.
        let ua = "é".repeat(200);
        for path in ["/.env", "/wp-admin/setup.php", "/@fs/etc/passwd"] {
            window.push(
                unix_micros_now(),
                Kind::Request,
                404,
                path,
                "192.0.2.1",
                Some(&ua),
            );
        }
        let report = build_report("origin", &window);
        assert_eq!(report.notable.len(), 1, "three probes from one address");
        let shown = &report.notable[0].user_agents[0];
        assert!(shown.ends_with(TRUNCATED), "shown as whole: {shown:?}");
        assert!(shown.chars().count() <= 160);
    }

    /// The owner's test: a path that hides a probe marker past the 512-byte
    /// cap is still probe-shaped. "Not shown" is not "not a probe", and an
    /// honest client does not send a 512-byte path.
    #[test]
    fn a_probe_marker_past_the_path_cap_is_still_probe_shaped() {
        let mut window = TrafficWindow::new();
        let path = format!("/{}/.env", "a".repeat(PATH_CAP + 8));
        window.push(
            unix_micros_now(),
            Kind::Request,
            404,
            &path,
            "192.0.2.1",
            None,
        );
        let report = build_report("origin", &window);
        // `probe_noise` is the report's record of `ClientSummary::probe_paths`
        // for an address below the notable threshold.
        assert_eq!(report.probe_noise.len(), 1, "the cut request is flagged");
        let shown = report.probe_noise[0]
            .strip_prefix("192.0.2.1 ")
            .expect("the noise line names the address");
        assert!(!shown.contains(".env"), "the marker was cut: {shown:?}");
        assert!(
            !m6_core::telemetry::looks_like_probe(shown),
            "the stored bytes carry no marker, so the flag is what catches it"
        );
        assert_eq!(
            report.forgers,
            vec!["192.0.2.1".to_string()],
            "the address reads as scanning"
        );
    }

    /// A user agent the cap cut counts as probe-shaped too, on an ordinary
    /// path: the part not shown may carry a marker.
    #[test]
    fn a_cut_user_agent_counts_as_probe_shaped() {
        let mut window = TrafficWindow::new();
        let ua = format!("Mozilla/5.0 {}", "x".repeat(UA_CAP));
        window.push(
            unix_micros_now(),
            Kind::Request,
            200,
            "/",
            "192.0.2.2",
            Some(&ua),
        );
        let report = build_report("origin", &window);
        assert_eq!(
            report.probe_noise,
            vec!["192.0.2.2 /".to_string()],
            "a cut user agent is probe-shaped"
        );
        assert_eq!(report.forgers, vec!["192.0.2.2".to_string()]);
    }
}

#[cfg(test)]
mod genid_gate_tests {
    use super::*;

    /// Address first, token never read: a public peer sees the 404 an unknown
    /// path gives, whether or not it carries a valid token. `None` for the key
    /// is the proof: a gate that looked at a token would have to answer 401.
    #[test]
    fn an_external_peer_is_refused_before_any_token() {
        let g = GenidConfig {
            read_require: Some("role:reader".to_string()),
            write_require: Some("role:writer".to_string()),
            ..GenidConfig::default()
        };
        let Gate::Refused(code, _, _) = genid_gate(&g, None, &[], "192.0.2.10", false, false)
        else {
            panic!("a public peer must be refused");
        };
        assert_eq!(code, 404);
        let Gate::Refused(code, _, _) = genid_gate(&g, None, &[], "::ffff:192.0.2.10", false, true)
        else {
            panic!("a canonical public peer must be refused");
        };
        assert_eq!(code, 404);
        // The same gate opens the address check for a loopback caller, which
        // then has no token, so it is refused as unauthorised rather than as
        // the nonexistent endpoint.
        let Gate::Refused(code, _, _) = genid_gate(&g, None, &[], "127.0.0.1", false, false) else {
            panic!("no token must be refused");
        };
        assert_eq!(code, 401);
        // Neither claim set is the endpoint not existing, to everyone.
        let none = GenidConfig::default();
        let Gate::Refused(code, _, _) = genid_gate(&none, None, &[], "127.0.0.1", false, false)
        else {
            panic!("no claims must be refused");
        };
        assert_eq!(code, 404);
    }
}

#[cfg(test)]
mod metrics_gate_tests {
    use super::*;
    use crate::auth::test_keys::{bearer, keypair, mint};

    fn token(private_pem: &str, roles: &[&str]) -> Vec<(String, String)> {
        bearer(&mint(private_pem, roles, 3600))
    }

    fn health(peers: Option<&[&str]>, require: Option<&str>) -> HealthConfig {
        HealthConfig {
            metrics_require: require.map(str::to_string),
            metrics_peer_ips: peers.map(|p| {
                p.iter()
                    .map(|s| s.parse::<IpAddr>().unwrap().to_canonical())
                    .collect()
            }),
            ..HealthConfig::default()
        }
    }

    fn code(g: &Gate) -> u16 {
        match g {
            Gate::Open => 200,
            Gate::Refused(code, _, _) => *code,
        }
    }

    #[test]
    fn an_unlisted_peer_is_404_before_any_token_check() {
        let (_d, key, private) = keypair();
        let h = health(Some(&["10.0.0.9"]), Some("role:monitor"));
        let good = token(&private, &["monitor"]);
        for ip in ["203.0.113.7", "2001:db8::1", "10.0.0.8"] {
            let g = metrics_gate(&h, Some(&key), &good, ip, false);
            assert_eq!(code(&g), 404, "{ip} must get 404 even with a valid token");
        }
        // No token and an unlisted peer is still 404, not 401: a stranger
        // learns nothing about the credential.
        assert_eq!(
            code(&metrics_gate(&h, Some(&key), &[], "203.0.113.7", false)),
            404
        );
    }

    #[test]
    fn a_listed_peer_passes_the_address_rule() {
        let (_d, key, private) = keypair();
        let h = health(Some(&["10.0.0.9"]), Some("role:monitor"));
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
        let h = health(Some(&["10.0.0.9"]), Some("role:monitor"));
        let good = token(&private, &["monitor"]);
        for ip in ["", "unknown", "10.0.0.9:443", "10.0.0.9, 1.2.3.4"] {
            assert_eq!(
                code(&metrics_gate(&h, Some(&key), &good, ip, false)),
                404,
                "{ip:?}"
            );
        }
    }

    #[test]
    fn a_listed_peer_on_an_internal_listener_is_refused() {
        let (_d, key, private) = keypair();
        let h = health(Some(&["10.0.0.9"]), Some("role:monitor"));
        let good = token(&private, &["monitor"]);
        assert_eq!(
            code(&metrics_gate(&h, Some(&key), &good, "10.0.0.9", true)),
            404
        );
    }

    #[test]
    fn a_listed_peer_with_a_valid_token_but_wrong_role_is_403() {
        let (_d, key, private) = keypair();
        let h = health(Some(&["10.0.0.9"]), Some("role:monitor"));
        let reader = token(&private, &["reader"]);
        let Gate::Refused(code, _, body) = metrics_gate(&h, Some(&key), &reader, "10.0.0.9", false)
        else {
            panic!("the wrong role must be refused");
        };
        assert_eq!(code, 403);
        assert_eq!(body, br#"{"error":"forbidden"}"#);
    }

    #[test]
    fn metrics_require_without_a_loaded_key_fails_closed() {
        let h = health(None, Some("role:monitor"));
        let Gate::Refused(code, _, _) = metrics_gate(&h, None, &[], "10.0.0.9", false) else {
            panic!("a missing key must fail closed");
        };
        assert_eq!(code, 401);
    }

    #[test]
    fn without_a_require_the_endpoint_is_disabled() {
        let h = health(None, None);
        let ok = vec![("authorization".to_string(), "Bearer anything".to_string())];
        // Any peer, internal or not: no require key means no endpoint.
        for (ip, internal) in [("203.0.113.7", false), ("10.0.0.9", true)] {
            assert_eq!(code(&metrics_gate(&h, None, &ok, ip, internal)), 404);
        }
    }
}
