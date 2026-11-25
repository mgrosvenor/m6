//! m6-http's side of monitoring: the `/health` endpoint and the two documents
//! a monitor reads.
//!
//! The documents themselves, and the shape of what they carry, are
//! `m6_core::monitoring`. They are an m6 feature rather than this binary's:
//! every deployment writes them, and an aggregator that reads them needs the
//! same types the server writes. One definition, shared, rather than a
//! consumer re-deriving the format by looking at a sample.
//!
//! What remains here is the part that is genuinely about this binary: which
//! backend name a monitoring response is tagged with, keeping those responses
//! out of the site traffic counters, and building the metrics and traffic
//! documents the log lines carry.

use std::net::IpAddr;

use crate::config::RegistrationConfig;
use crate::stats::Iface;

pub use m6_core::monitoring::{not_found, HealthReport, PerfReport, PerfSubject, PoolHealth};

/// Backend name tagged onto a `/health` response.
pub const HEALTH_BACKEND: &str = "health";
/// Backend name tagged onto a `PURGE` or `/m6/status` response, for stats and
/// analytics.
pub const CONTROL_BACKEND: &str = "control";

/// Whether a completed response came from the health endpoint.
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
    backend == HEALTH_BACKEND
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
/// loopback is classified by its path instead, which `/health` already is.
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

// ── /health's dependency flags ───────────────────────────────────────────────
//
// `/health` answers for the process and for the dependencies whose failure the
// loop already observes. It reads flags and never probes: each flag is set
// where a real request's outcome is already known and cleared by the next
// heartbeat answer. That is what keeps the answer a JSON serialisation on the
// most frequently hit path on the box.

/// Whether this node's origin is failing, which is what a cache node's
/// `degraded` answer reports.
///
/// The flag is the follower's own `up` bit, set by the control connection's
/// lifecycle in `main.rs`: true when the connection is open, false when it
/// dropped and has not reconnected. `/health` reads the flag and does nothing
/// else, so the cost is one branch, with no timer, no probe and no round trip.
///
/// A second dependency is one more term at the call site in `main.rs`: the auth
/// checker's flag is added where its `/check` call fails and ORed into the
/// same `degraded` bit.
pub fn origin_down(role: &crate::registration::Role) -> bool {
    role.cache().is_some_and(|f| !f.up())
}

/// The `/health` answer: `ok` while every dependency answers, `degraded` while
/// one is failing.
///
/// The status carries the answer, 200 or 503. `m6-monitor` reads a non-200 as
/// `degraded` without consulting the body, which is the mapping the two
/// answers were built to fit. The body names the node and never the dependency:
/// `/health` is public and unsecured, so the dependency is in the warn and info
/// lines the process logs on the change instead.
pub fn health_answer(degraded: bool, node: &str) -> (u16, HealthReport) {
    if degraded {
        HealthReport::degraded(node)
    } else {
        HealthReport::build(node)
    }
}

#[cfg(test)]
mod health_state_tests {
    use super::*;
    use crate::registration::{Follower, Role, Source};

    /// A node with no origin is known to be one from config, so it has no
    /// dependency to lose and answers `ok` while its process runs.
    #[test]
    fn a_source_has_no_origin_flag() {
        let role = Role::Source(Source::new(0));
        assert!(!origin_down(&role));
        let (code, report) = health_answer(origin_down(&role), "origin");
        assert_eq!(code, 200);
        assert_eq!(report.status, "ok");
    }

    /// A cache whose control connection is up answers `ok`.
    #[test]
    fn a_cache_whose_origin_answers_is_ok() {
        let mut f = Follower::new(7);
        f.set_up(true);
        let role = Role::Cache(f);
        assert!(!origin_down(&role));
        let (code, report) = health_answer(origin_down(&role), "edge-a");
        assert_eq!(code, 200);
        assert_eq!(report.status, "ok");
    }

    /// A cache whose control connection dropped answers 503 `degraded`, and
    /// the public body names the node and nothing a stranger can aim at.
    #[test]
    fn a_cache_whose_origin_fails_is_degraded_and_names_nothing() {
        let role = Role::Cache(Follower::new(7));
        assert!(origin_down(&role));
        let (code, report) = health_answer(origin_down(&role), "edge-a");
        assert_eq!(code, 503);
        assert_eq!(report.status, "degraded");
        let (code, _, body) = report.into_response(code);
        assert_eq!(code, 503);
        assert_eq!(
            String::from_utf8(body).unwrap(),
            r#"{"status":"degraded","node":"edge-a"}"#
        );
    }
}

#[cfg(test)]
mod monitoring_exclusion_tests {
    use super::*;

    /// Regression guard. The cache-miss accounting on the serving paths fires
    /// on any `Ready` outcome, and `/health` returns Ready. Left uncounted
    /// for, every health check was recorded as a cache miss: 10 checks moved
    /// requests_total by 11.
    ///
    /// That defeats the counter's purpose. A monitor polling every 30s would
    /// keep requests_total rising through a total traffic stall, so "the
    /// number stopped moving" would never fire.
    #[test]
    fn the_health_endpoint_is_excluded_from_traffic_stats() {
        assert!(is_monitoring_endpoint(HEALTH_BACKEND));
        assert!(is_monitoring_endpoint("health"));
        assert!(!is_monitoring_endpoint("perf"));
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

// ── The traffic document ────────────────────────────────────────────────────

use std::time::Instant;

use m6_core::monitoring::TrafficReport;
use m6_core::telemetry::{RequestView, Stamp};
// The one marker for a value a cap cut short. The slot and the report use the
// same value, so the two cuts cannot drift apart.
use m6_core::telemetry::USER_AGENT_TRUNCATED as TRUNCATED;

/// Where the privileged collector leaves `nft -j list ruleset` output.
const FIREWALL_JSON: &str = "/var/lib/m6/firewall.json";

/// The most events the traffic window keeps. Allocated once at start.
const TRAFFIC_SLOTS: usize = 32_768;

/// How far back the traffic report covers, in minutes.
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

/// One logged event, as the traffic window needs it.
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

/// The last events this process logged, for the traffic report. Allocated at
/// start.
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

/// Build the traffic summary from the window, for the `type=traffic` line.
///
/// The slots of the last hour are read in place as borrowed views, and
/// `TrafficReport::from_views` builds the report body with the rules a log
/// read uses, so the figures match the ones the same lines would produce. A
/// value the slot cut is the one difference, and `truncated` tells the
/// summary, because a cut field can hide a probe marker. The report also
/// states how long a window it actually covers and how many events the ring
/// overwrote inside that window.
pub fn build_report(node: &str, window: &TrafficWindow) -> TrafficReport {
    let now_us = unix_micros_now();
    let cutoff = now_us.saturating_sub(WINDOW_US);
    let views = window
        .oldest_first()
        .filter(|slot| slot.at_us >= cutoff)
        .map(|slot| RequestView {
            at: Stamp::Micros(slot.at_us),
            message: slot.kind.message(),
            status: slot.status,
            path: slot.path.as_str(),
            client_ip: slot.client_ip.as_str(),
            user_agent: slot.user_agent.as_opt().unwrap_or(""),
            truncated: slot.truncated,
        });
    let mut report = TrafficReport::from_views(node, views, window.covered_minutes(now_us));
    report.events_dropped = window.dropped_in_window(now_us);
    // Written by the m6-firewall-stats timer. A node without the timer
    // reports no firewall state rather than an empty one.
    report.firewall =
        m6_core::firewall::FirewallState::from_file(std::path::Path::new(FIREWALL_JSON))
            .unwrap_or(None);
    report
}

/// The verdict of the control gate for one request.
pub enum Gate {
    /// Serve the request.
    Open,
    /// Answer with this exact status, headers and body and do nothing else:
    /// the 404 a disabled endpoint gives.
    Refused(u16, Vec<(String, String)>, Vec<u8>),
    /// A credential check is required, with this `require` string. The caller
    /// asks `POST /check` and resumes with the decision.
    Check(String),
}

impl Gate {
    /// The 404 an endpoint answers when it is not configured.
    fn disabled() -> Self {
        let (code, headers, body) = not_found();
        Gate::Refused(code, headers, body)
    }
}

/// Decide whether a `PURGE` or `GET /m6/status` may be served to this
/// request.
///
/// Two rules, in this order:
///
/// 1. The peer is loopback, or the request arrived on an internal listener.
///    Anyone else gets the 404 an unknown path gives, before a token is read.
///    Neither claim set is the same 404: the endpoint does not exist.
/// 2. The one token check decides with `read_require` for a read (`GET
///    /m6/status`) or `write_require` for a write (`PURGE`). It is called
///    once, here, and its refusal is the answer unchanged.
pub fn control_gate(g: &RegistrationConfig, client_ip: &str, from_internal: bool, write: bool) -> Gate {
    if !control_address_ok(client_ip, from_internal) {
        return Gate::disabled();
    }
    let require = if write {
        g.write_require.as_deref()
    } else {
        g.read_require.as_deref()
    };
    match require {
        Some(require) => Gate::Check(require.to_string()),
        None => Gate::disabled(),
    }
}

/// The address rule alone: loopback, or an internal listener.
///
/// The same rule the old `/genid` endpoint used. A public peer gets the 404
/// an unknown path gives, before the request is read or forwarded.
pub fn control_address_ok(client_ip: &str, from_internal: bool) -> bool {
    let loopback = client_ip
        .parse::<IpAddr>()
        .is_ok_and(|ip| ip.to_canonical().is_loopback());
    from_internal || loopback
}

/// The disk-cache figures the metrics document adds, gathered so the document
/// keeps one argument for them: the writer's counters, the load's counts, and
/// the first heartbeat's decision.
pub struct DiskPerf<'a> {
    pub writer: &'a crate::disk_cache::DiskWriter,
    pub queued: usize,
    pub loaded_entries: usize,
    pub loaded_bad: usize,
    pub state: crate::disk_cache::LoadState,
}

/// The metrics document, with this node's registration and disk blocks added.
///
/// One definition for the metrics line, which carries exactly this document in
/// `fields.perf`.
///
/// The blocks sit beside the report this node already built, so `m6-core`
/// keeps no notion of registration or a disk cache: it hands over the
/// monitoring payload and m6-http adds the fields that are this binary's. A
/// source reports the change counter and the caches registered with it. A
/// cache node reports the counter it saved and whether its origin is up.
///
/// It is `Serialize` and borrows what it reports, so the log line writes it
/// field by field into its buffer with no JSON tree built first.
///
/// `None` is a node with no `[registration]` table: the report alone, which is
/// what such a node's metrics document has always carried.
pub fn perf_document<'a>(
    report: &'a PerfReport,
    registration: Option<(
        &'a crate::registration::Role,
        &'a RegistrationConfig,
        Option<DiskPerf<'a>>,
    )>,
) -> PerfDocument<'a> {
    let (registration, disk_cache) = match registration {
        None => (None, None),
        Some((role, config, disk)) => (
            Some(registration_block(role, config)),
            disk.map(|disk| DiskCacheBlock {
                dir_bytes: disk.writer.dir_bytes(),
                queued: disk.queued,
                written_last_tick: disk.writer.written_last_tick(),
                written_bytes_last_tick: disk.writer.written_bytes_last_tick(),
                skipped_full: disk.writer.skipped_full(),
                ticks: disk.writer.ticks(),
                tick_last_ns: disk.writer.tick_last_ns(),
                loaded_entries: disk.loaded_entries,
                loaded_bad: disk.loaded_bad,
                state: disk.state.as_str(),
            }),
        ),
    };
    PerfDocument {
        report,
        registration,
        disk_cache,
    }
}

/// What [`perf_document`] returns: the report's own fields, then the blocks.
#[derive(serde::Serialize)]
pub struct PerfDocument<'a> {
    #[serde(flatten)]
    report: &'a PerfReport,
    #[serde(skip_serializing_if = "Option::is_none")]
    registration: Option<RegistrationBlock<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    disk_cache: Option<DiskCacheBlock>,
}

/// The `disk_cache` object.
#[derive(serde::Serialize)]
struct DiskCacheBlock {
    dir_bytes: u64,
    queued: usize,
    written_last_tick: usize,
    written_bytes_last_tick: u64,
    skipped_full: u64,
    ticks: u64,
    tick_last_ns: u64,
    loaded_entries: usize,
    loaded_bad: usize,
    state: &'static str,
}

/// The `registration` object, by role.
#[derive(serde::Serialize)]
#[serde(tag = "role", rename_all = "lowercase")]
enum RegistrationBlock<'a> {
    Source {
        counter: Decimal,
        registered: Vec<&'a str>,
    },
    Cache {
        counter: Decimal,
        origin: &'a str,
        origin_down: bool,
    },
}

/// A change counter, written as a decimal string, as the generation ID was.
struct Decimal(u64);

impl serde::Serialize for Decimal {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(&self.0)
    }
}

fn registration_block<'a>(
    role: &'a crate::registration::Role,
    config: &'a RegistrationConfig,
) -> RegistrationBlock<'a> {
    match role {
        crate::registration::Role::Source(s) => RegistrationBlock::Source {
            counter: Decimal(s.counter()),
            registered: s.registered().iter().map(|r| r.name.as_str()).collect(),
        },
        crate::registration::Role::Cache(f) => RegistrationBlock::Cache {
            counter: Decimal(f.counter()),
            origin: config.origin.as_deref().unwrap_or(""),
            origin_down: !f.up(),
        },
    }
}

#[cfg(test)]
mod traffic_document_tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn summarises_the_window_for_the_traffic_line() {
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

        let report = build_report("origin", &window);
        let v = serde_json::to_value(&report).unwrap();
        assert_eq!(v["node"], "origin");
        assert_eq!(v["total_requests"], 2);
        assert!(v["crawlers"][0]["user_agent"]
            .as_str()
            .unwrap()
            .contains("ExampleBot"));
        assert!(v["logging"].is_object(), "logging health travels with it");
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
mod control_gate_tests {
    use super::*;

    fn config() -> RegistrationConfig {
        RegistrationConfig {
            read_require: Some("role:reader".to_string()),
            write_require: Some("role:writer".to_string()),
            ..RegistrationConfig::default()
        }
    }

    /// Address first, check never made: a public peer sees the 404 an unknown
    /// path gives, whether or not it carries a credential.
    #[test]
    fn an_external_peer_is_refused_before_any_check() {
        let g = config();
        for write in [false, true] {
            let Gate::Refused(code, _, _) = control_gate(&g, "192.0.2.10", false, write) else {
                panic!("a public peer must be refused");
            };
            assert_eq!(code, 404);
            let Gate::Refused(code, _, _) = control_gate(&g, "::ffff:192.0.2.10", false, write)
            else {
                panic!("a canonical public peer must be refused");
            };
            assert_eq!(code, 404);
        }
    }

    #[test]
    fn a_loopback_peer_asks_for_the_methods_claim() {
        let g = config();
        let Gate::Check(read) = control_gate(&g, "127.0.0.1", false, false) else {
            panic!("a loopback read must ask for a check");
        };
        assert_eq!(read, "role:reader");
        let Gate::Check(write) = control_gate(&g, "127.0.0.1", false, true) else {
            panic!("a loopback write must ask for a check");
        };
        assert_eq!(write, "role:writer");
        // The internal listener is the other allowed path and asks the same.
        assert!(matches!(
            control_gate(&g, "203.0.113.7", true, false),
            Gate::Check(_)
        ));
    }

    #[test]
    fn no_claims_is_the_endpoint_not_existing() {
        let none = RegistrationConfig::default();
        let Gate::Refused(code, _, _) = control_gate(&none, "127.0.0.1", false, false) else {
            panic!("no claims must be refused");
        };
        assert_eq!(code, 404);
    }
}
