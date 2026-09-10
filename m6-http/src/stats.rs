/// Performance statistics for the critical path.
use std::time::Instant;

const EMIT_INTERVAL_SECS: u64 = 10;

/// Number of latency samples kept per window per category (hit / miss).
/// Ring-buffer: oldest sample is overwritten when full.
/// 4096 × 8 bytes = 32 KB per reservoir.
const RESERVOIR: usize = 4096;

/// Per-channel reservoir. Smaller than the aggregate one on purpose: there
/// are up to six channels × two categories, so the full 4096 would cost
/// 384 KB to answer a question that a few hundred samples already answers.
const CHANNEL_RESERVOIR: usize = 512;

/// Whether a response was produced by m6-http itself rather than fetched
/// from a backend.
///
/// These names are set at the point the response is constructed, so they are
/// the authoritative signal that no backend was involved. `cache` is included
/// deliberately: a replayed 5xx is a stored copy of an old backend failure,
/// not a new one, and counting it again would inflate the total every time
/// the entry is served.
fn is_self_generated(backend: &str) -> bool {
    matches!(
        backend,
        "cache" | "method-check" | "health" | "perf" | "error" | "error-local"
    )
}

/// HTTP version actually used, taken from the request rather than assumed.
///
/// It has to be per-request, not per-listener: HTTP/1.1 and HTTP/2 share the
/// single TLS listener and are separated only by ALPN. Assuming otherwise is
/// how the request log came to label every TLS request `version = "HTTP/1.1"`
/// including the HTTP/2 ones, which is most browser traffic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Version {
    Http11,
    Http2,
    Http3,
}

impl Version {
    /// Parse the wire version string. Both `HTTP/2` and `HTTP/2.0` occur in
    /// this codebase, so both are accepted.
    pub fn from_wire(s: &str) -> Version {
        if s.starts_with("HTTP/3") {
            Version::Http3
        } else if s.starts_with("HTTP/2") {
            Version::Http2
        } else {
            Version::Http11
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Version::Http11 => "http/1.1",
            Version::Http2 => "http/2",
            Version::Http3 => "http/3",
        }
    }
}

/// Which network the request arrived on.
///
/// The origin serves two completely different populations and pooling them
/// makes both numbers meaningless: real visitors arrive on the public NIC
/// over TLS or QUIC, while cache-miss forwards from London and Chicago
/// arrive over the WireGuard tunnel as h2c. The second group carries
/// intercontinental RTT (~200-300ms) that has nothing to do with how fast
/// this node is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Iface {
    /// Public network interface.
    External,
    /// WireGuard tunnel between nodes.
    Internal,
}

impl Iface {
    /// Classify a listener by its bind address.
    ///
    /// Derived rather than hardcoded per listener: h2c is *conventionally*
    /// the WireGuard listener here, but that is deployment configuration, not
    /// a property of the protocol, and a future node that exposes h2c
    /// publicly should not be silently labelled internal.
    pub fn for_bind(bind: &str) -> Iface {
        let host = bind.rsplit_once(':').map(|(h, _)| h).unwrap_or(bind);
        let host = host.trim_start_matches('[').trim_end_matches(']');
        let octets: Vec<u8> = host.split('.').filter_map(|o| o.parse().ok()).collect();
        let private = match octets.as_slice() {
            [10, ..] => true,
            [172, b, ..] if (16..=31).contains(b) => true,
            [192, 168, ..] => true,
            [127, ..] => true,
            _ => host.starts_with("fd") || host.starts_with("fc") || host == "::1",
        };
        if private { Iface::Internal } else { Iface::External }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Iface::External => "external",
            Iface::Internal => "internal",
        }
    }
}

/// One (version, interface) pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Channel {
    pub version: Version,
    pub iface: Iface,
}

impl Channel {
    pub fn new(version: Version, iface: Iface) -> Channel {
        Channel { version, iface }
    }

    /// Dense index for the fixed-size table: 3 versions × 2 interfaces.
    fn index(self) -> usize {
        let v = match self.version {
            Version::Http11 => 0,
            Version::Http2 => 1,
            Version::Http3 => 2,
        };
        v * 2 + matches!(self.iface, Iface::Internal) as usize
    }

    fn from_index(i: usize) -> Channel {
        let version = match i / 2 {
            0 => Version::Http11,
            1 => Version::Http2,
            _ => Version::Http3,
        };
        let iface = if i % 2 == 1 { Iface::Internal } else { Iface::External };
        Channel { version, iface }
    }

    pub fn label(self) -> String {
        format!("{}/{}", self.version.as_str(), self.iface.as_str())
    }
}

const CHANNELS: usize = 6;

/// Counters and latency samples for one channel.
struct ChannelStats {
    requests: u64,
    hits: u64,
    misses: u64,
    backend_errors: u64,
    hit_samples: Box<[u64; CHANNEL_RESERVOIR]>,
    hit_idx: usize,
    hit_count: usize,
    miss_samples: Box<[u64; CHANNEL_RESERVOIR]>,
    miss_idx: usize,
    miss_count: usize,
}

impl ChannelStats {
    fn new() -> ChannelStats {
        ChannelStats {
            requests: 0,
            hits: 0,
            misses: 0,
            backend_errors: 0,
            hit_samples: Box::new([0u64; CHANNEL_RESERVOIR]),
            hit_idx: 0,
            hit_count: 0,
            miss_samples: Box::new([0u64; CHANNEL_RESERVOIR]),
            miss_idx: 0,
            miss_count: 0,
        }
    }

    fn record(&mut self, elapsed_ns: u64, cache_hit: bool, backend_error: bool) {
        self.requests += 1;
        if backend_error {
            self.backend_errors += 1;
        }
        if cache_hit {
            self.hits += 1;
            if elapsed_ns > 0 {
                self.hit_samples[self.hit_idx] = elapsed_ns;
                self.hit_idx = (self.hit_idx + 1) % CHANNEL_RESERVOIR;
                if self.hit_count < CHANNEL_RESERVOIR {
                    self.hit_count += 1;
                }
            }
        } else {
            self.misses += 1;
            if elapsed_ns > 0 {
                self.miss_samples[self.miss_idx] = elapsed_ns;
                self.miss_idx = (self.miss_idx + 1) % CHANNEL_RESERVOIR;
                if self.miss_count < CHANNEL_RESERVOIR {
                    self.miss_count += 1;
                }
            }
        }
    }
}

/// One channel's figures, as reported by `/perf`.
///
/// Cumulative counters, unlike the aggregate percentiles, which are windowed.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ChannelSnapshot {
    /// e.g. `"http/2/external"`.
    pub channel: String,
    pub version: &'static str,
    pub iface: &'static str,
    pub requests: u64,
    pub hits: u64,
    pub misses: u64,
    pub backend_errors: u64,
    pub hit_samples: usize,
    pub hit_p50_ns: u64,
    pub hit_p99_ns: u64,
    pub miss_samples: usize,
    pub miss_p50_ns: u64,
    pub miss_p99_ns: u64,
}

pub struct Stats {
    // Cumulative
    pub requests_total:       u64,
    pub cache_hits_total:     u64,
    pub cache_misses_total:   u64,
    pub backend_errors_total: u64,

    // Window counters (reset each emit)
    window_requests:       u64,
    window_cache_hits:     u64,
    window_cache_misses:   u64,
    window_backend_errors: u64,

    // Raw latency samples — ring buffers, one per category
    hit_samples:  Box<[u64; RESERVOIR]>,
    hit_idx:      usize,
    hit_count:    usize,   // capped at RESERVOIR

    miss_samples: Box<[u64; RESERVOIR]>,
    miss_idx:     usize,
    miss_count:   usize,

    // ── Monitoring endpoints, accounted separately ────────────────────────
    //
    // /health and /perf are not site traffic, but they are not nothing
    // either. They used to be dropped on the floor at three call sites, which
    // kept the traffic counters honest and made the monitor itself invisible:
    // you could not tell a working uptime check from a monitor that had
    // silently stopped, and a flood aimed at /health showed up nowhere at all.
    //
    // Counted here instead of discarded. Nothing that follows is mixed into
    // requests_total, the hit rate, or the latency reservoirs above, so "the
    // request count stopped moving" still detects a traffic stall even while a
    // 30-second monitor keeps polling. That property is the whole reason the
    // exclusion existed, and separate accounting preserves it without
    // throwing the data away.
    pub monitor_requests_total: u64,
    window_monitor_requests:    u64,
    monitor_samples: Box<[u64; RESERVOIR]>,
    monitor_idx:     usize,
    monitor_count:   usize,

    /// Per (version, interface) breakdown. Fixed-size dense table rather than
    /// a map: six entries, indexed arithmetically, no allocation and no hash
    /// on the request path.
    channels: Vec<ChannelStats>,

    /// Response codes 100..=599, indexed by `status - 100`. A dense 4 KB
    /// array rather than a map: incrementing is one bounds-checked index with
    /// no hashing and no allocation, on a path that runs for every response.
    /// Only the non-zero entries are ever reported.
    status_counts: Box<[u64; 500]>,

    // RPS
    pub rps_peak: u64,
    window_start: Instant,
    last_emit:    Instant,
}

impl Stats {
    pub fn new() -> Self {
        let now = Instant::now();
        Stats {
            requests_total: 0, cache_hits_total: 0, cache_misses_total: 0, backend_errors_total: 0,
            window_requests: 0, window_cache_hits: 0, window_cache_misses: 0, window_backend_errors: 0,
            hit_samples:  Box::new([0u64; RESERVOIR]),
            hit_idx: 0, hit_count: 0,
            miss_samples: Box::new([0u64; RESERVOIR]),
            miss_idx: 0, miss_count: 0,
            monitor_requests_total: 0, window_monitor_requests: 0,
            monitor_samples: Box::new([0u64; RESERVOIR]),
            monitor_idx: 0, monitor_count: 0,
            channels: (0..CHANNELS).map(|_| ChannelStats::new()).collect(),
            status_counts: Box::new([0u64; 500]),
            rps_peak: 0, window_start: now, last_emit: now,
        }
    }

    #[inline(always)]
    /// Record one completed response.
    ///
    /// Takes the FINAL status rather than a precomputed `backend_error` flag.
    /// The flag was always `status >= 500` at every call site, so passing the
    /// status removes a duplicated derivation and yields the response-code
    /// breakdown for free. It also forced the cache-hit sites to be corrected:
    /// they recorded before evaluating preconditions, so a conditional request
    /// answered 304 or 412 would have been counted as the cached 200.
    pub fn record(
        &mut self,
        elapsed_ns: u64,
        cache_hit: bool,
        status: u16,
        channel: Channel,
        backend: &str,
    ) {
        // /health and /perf are accounted separately and return here, so
        // nothing below touches the traffic counters. The decision lives in
        // this one place rather than at each call site: it used to be three
        // `if !is_monitoring_endpoint(..)` guards in main.rs, one per protocol
        // path, and the h3 one was added later precisely because a guard is
        // easy to forget when a fourth call site appears.
        if crate::health::is_monitoring_endpoint(backend) {
            self.monitor_requests_total += 1;
            self.window_monitor_requests += 1;
            if elapsed_ns > 0 {
                self.monitor_samples[self.monitor_idx] = elapsed_ns;
                self.monitor_idx = (self.monitor_idx + 1) % RESERVOIR;
                if self.monitor_count < RESERVOIR {
                    self.monitor_count += 1;
                }
            }
            return;
        }

        // A 5xx that m6-http generated itself is not a BACKEND error, and
        // counting it as one makes the metric cry wolf.
        //
        // Observed for nine consecutive hours: a bot sending an unrecognised
        // verb gets 501 Not Implemented from method validation, no backend is
        // ever contacted, and `backend_errors_total` rose on all three nodes.
        // An operator watching that counter would go looking for a failing
        // renderer that was never involved.
        let backend_error = status >= 500 && !is_self_generated(backend);
        // 100..=599. Anything outside is not a status this server emits;
        // counting it would mean trusting an index derived from it.
        if (100..600).contains(&status) {
            self.status_counts[usize::from(status) - 100] += 1;
        }
        self.channels[channel.index()].record(elapsed_ns, cache_hit, backend_error);
        self.requests_total  += 1;
        self.window_requests += 1;

        if elapsed_ns > 0 {
            if cache_hit {
                self.cache_hits_total  += 1;
                self.window_cache_hits += 1;
                self.hit_samples[self.hit_idx] = elapsed_ns;
                self.hit_idx = (self.hit_idx + 1) & (RESERVOIR - 1);
                if self.hit_count < RESERVOIR { self.hit_count += 1; }
            } else {
                self.cache_misses_total  += 1;
                self.window_cache_misses += 1;
                self.miss_samples[self.miss_idx] = elapsed_ns;
                self.miss_idx = (self.miss_idx + 1) & (RESERVOIR - 1);
                if self.miss_count < RESERVOIR { self.miss_count += 1; }
            }
        } else if cache_hit {
            self.cache_hits_total  += 1;
            self.window_cache_hits += 1;
        } else {
            self.cache_misses_total  += 1;
            self.window_cache_misses += 1;
        }

        if backend_error {
            self.backend_errors_total  += 1;
            self.window_backend_errors += 1;
        }
    }

    #[inline]
    pub fn maybe_emit(&mut self, pool_members: usize) {
        let now     = Instant::now();
        let elapsed = now.duration_since(self.last_emit);
        if elapsed.as_secs() < EMIT_INTERVAL_SECS { return; }

        let elapsed_secs = elapsed.as_secs_f64().max(0.001);
        let rps_avg      = (self.window_requests as f64 / elapsed_secs) as u64;
        if rps_avg > self.rps_peak { self.rps_peak = rps_avg; }

        let total_window   = self.window_cache_hits + self.window_cache_misses;
        let cache_hit_rate = if total_window > 0 { self.window_cache_hits as f64 / total_window as f64 } else { 0.0 };

        let (hp0, hp50, hp99, hp100) = percentiles(&self.hit_samples,  self.hit_count);
        let (mp0, mp50, mp99, mp100) = percentiles(&self.miss_samples, self.miss_count);
        let (_, kp50, kp99, _)       = percentiles(&self.monitor_samples, self.monitor_count);

        tracing::info!(
            requests       = self.requests_total,
            rps_avg        = rps_avg,
            rps_peak       = self.rps_peak,
            cache_hits     = self.window_cache_hits,
            cache_misses   = self.window_cache_misses,
            cache_hit_rate = format_args!("{:.4}", cache_hit_rate),
            backend_errors = self.window_backend_errors,
            pool_members   = pool_members,
            hit_p0_ns      = hp0,
            hit_p50_ns     = hp50,
            hit_p99_ns     = hp99,
            hit_max_ns     = hp100,
            miss_p0_ns     = mp0,
            miss_p50_ns    = mp50,
            miss_p99_ns    = mp99,
            miss_max_ns    = mp100,
            // Monitoring endpoints, deliberately outside every counter above.
            // Reported so a monitor that stops polling, or one that starts
            // flooding, is visible; a reader can tell those apart from a
            // traffic change because these never move the traffic figures.
            monitor_requests = self.window_monitor_requests,
            monitor_p50_ns   = kp50,
            monitor_p99_ns   = kp99,
            "periodic stats"
        );

        // Reset window
        self.window_requests = 0; self.window_cache_hits = 0;
        self.window_cache_misses = 0; self.window_backend_errors = 0;
        self.hit_idx = 0; self.hit_count = 0;
        self.miss_idx = 0; self.miss_count = 0;
        self.window_monitor_requests = 0;
        self.monitor_idx = 0; self.monitor_count = 0;
        self.window_start = now;
        self.last_emit    = now;
    }
}

/// A read-only view of the counters, for the health endpoint.
///
/// Cumulative fields are monotonic for the life of the process, which is what
/// makes "the request count stopped increasing" a usable liveness signal.
///
/// The percentile fields are **not** cumulative. `maybe_emit` clears the
/// latency reservoirs every 10 seconds, so these describe the current partial
/// window only, which may hold very few samples or none. That is why
/// `hit_samples`/`miss_samples` are reported alongside: a p99 drawn from three
/// samples is noise, and a consumer that cannot see the sample count has no
/// way to tell it apart from a real one.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct StatsSnapshot {
    pub requests_total: u64,
    pub cache_hits_total: u64,
    pub cache_misses_total: u64,
    pub backend_errors_total: u64,
    /// /health and /perf polls. Cumulative, and deliberately excluded from
    /// `requests_total` so a stalled site is still detectable while a monitor
    /// keeps polling. Reported rather than discarded so the monitor itself is
    /// observable: a check that stops, or one that floods, shows up here.
    pub monitor_requests_total: u64,
    pub monitor_samples: usize,
    pub monitor_p50_ns: u64,
    pub monitor_p99_ns: u64,
    pub rps_peak: u64,
    /// Samples backing the hit percentiles in the current window.
    pub hit_samples: usize,
    pub hit_p50_ns: u64,
    pub hit_p99_ns: u64,
    pub hit_max_ns: u64,
    /// Samples backing the miss percentiles in the current window.
    pub miss_samples: usize,
    pub miss_p50_ns: u64,
    pub miss_p99_ns: u64,
    pub miss_max_ns: u64,
    /// Per (version, interface) breakdown. Channels that have seen no traffic
    /// are omitted rather than reported as rows of zeros, so the list shows
    /// what this node actually serves.
    pub channels: Vec<ChannelSnapshot>,
    /// Response codes actually emitted, keyed by code. Codes never returned
    /// are omitted entirely: a fixed 100..599 table would be 500 rows of
    /// zeros around the four that matter, and the useful signal here is
    /// exactly which codes appeared.
    pub status_counts: std::collections::BTreeMap<u16, u64>,
}

impl Stats {
    /// Snapshot without mutating anything.
    ///
    /// Takes `&self` deliberately. `maybe_emit` both reports and resets; if
    /// the health endpoint shared that path, every scrape would clear the
    /// window and the periodic stats log would report only the traffic that
    /// arrived between scrapes. A monitor polling every 30 seconds would
    /// silently gut the operational logging it exists to complement.
    pub fn snapshot(&self) -> StatsSnapshot {
        let (_, hp50, hp99, hmax) = percentiles(&self.hit_samples, self.hit_count);
        let (_, mp50, mp99, mmax) = percentiles(&self.miss_samples, self.miss_count);
        let (_, kp50, kp99, _) = percentiles(&self.monitor_samples, self.monitor_count);
        StatsSnapshot {
            requests_total: self.requests_total,
            cache_hits_total: self.cache_hits_total,
            cache_misses_total: self.cache_misses_total,
            backend_errors_total: self.backend_errors_total,
            monitor_requests_total: self.monitor_requests_total,
            monitor_samples: self.monitor_count,
            monitor_p50_ns: kp50,
            monitor_p99_ns: kp99,
            rps_peak: self.rps_peak,
            hit_samples: self.hit_count,
            hit_p50_ns: hp50,
            hit_p99_ns: hp99,
            hit_max_ns: hmax,
            miss_samples: self.miss_count,
            miss_p50_ns: mp50,
            miss_p99_ns: mp99,
            miss_max_ns: mmax,
            status_counts: self
                .status_counts
                .iter()
                .enumerate()
                .filter(|(_, &n)| n > 0)
                .map(|(i, &n)| (i as u16 + 100, n))
                .collect(),
            channels: self
                .channels
                .iter()
                .enumerate()
                .filter(|(_, c)| c.requests > 0)
                .map(|(i, c)| {
                    let ch = Channel::from_index(i);
                    let (_, hp50, hp99, _) =
                        percentiles_n(&c.hit_samples[..], c.hit_count);
                    let (_, mp50, mp99, _) =
                        percentiles_n(&c.miss_samples[..], c.miss_count);
                    ChannelSnapshot {
                        channel: ch.label(),
                        version: ch.version.as_str(),
                        iface: ch.iface.as_str(),
                        requests: c.requests,
                        hits: c.hits,
                        misses: c.misses,
                        backend_errors: c.backend_errors,
                        hit_samples: c.hit_count,
                        hit_p50_ns: hp50,
                        hit_p99_ns: hp99,
                        miss_samples: c.miss_count,
                        miss_p50_ns: mp50,
                        miss_p99_ns: mp99,
                    }
                })
                .collect(),
        }
    }
}

/// Slice-based percentiles, for the per-channel reservoirs (which are a
/// different fixed size from the aggregate ones).
fn percentiles_n(samples: &[u64], n: usize) -> (u64, u64, u64, u64) {
    if n == 0 { return (0, 0, 0, 0); }
    let mut buf: Vec<u64> = samples[..n].to_vec();
    buf.sort_unstable();
    (buf[0], buf[(n - 1) * 50 / 100], buf[(n - 1) * 99 / 100], buf[n - 1])
}

/// Sort the first `n` samples and return exact (p0, p50, p99, p100).
fn percentiles(samples: &[u64; RESERVOIR], n: usize) -> (u64, u64, u64, u64) {
    if n == 0 { return (0, 0, 0, 0); }
    let mut buf: Vec<u64> = samples[..n].to_vec();
    buf.sort_unstable();
    let p0   = buf[0];
    let p50  = buf[(n - 1) * 50 / 100];
    let p99  = buf[(n - 1) * 99 / 100];
    let p100 = buf[n - 1];
    (p0, p50, p99, p100)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Monitoring polls are counted, and counted APART.
    ///
    /// Both halves matter and each catches a different mistake. If they leak
    /// into `requests_total`, a 30-second monitor keeps that counter rising
    /// through a total traffic stall and "the number stopped moving" never
    /// fires, which is the one thing it exists for. If they are dropped
    /// instead, as they were until 2026-09-10, a check that has silently
    /// stopped is indistinguishable from one that is passing, and a flood
    /// aimed at /health is recorded nowhere.
    ///
    /// Delete the early return in `record` and the first assertion fails on
    /// `requests_total`; delete the monitor counters and the second fails.
    #[test]
    fn monitor_polls_are_counted_separately_from_site_traffic() {
        let mut s = Stats::new();
        let ch = Channel::new(Version::Http11, Iface::External);
        s.record(1_000, false, 200, ch, "m6-html");
        for _ in 0..25 {
            s.record(9_000, false, 200, ch, crate::health::HEALTH_BACKEND);
            s.record(9_000, false, 200, ch, crate::health::PERF_BACKEND);
        }

        // Site traffic is untouched by 50 monitor polls.
        assert_eq!(s.requests_total, 1, "monitor polls leaked into requests_total");
        assert_eq!(s.cache_misses_total, 1, "monitor polls leaked into the miss count");
        assert_eq!(s.backend_errors_total, 0);

        // And the polls are not lost.
        assert_eq!(s.monitor_requests_total, 50, "monitor polls were discarded");
        let snap = s.snapshot();
        assert_eq!(snap.monitor_requests_total, 50);
        assert_eq!(snap.requests_total, 1);
        assert!(snap.monitor_p50_ns > 0, "monitor latency not sampled");
    }

    /// A monitor 5xx is not a backend error: no backend was contacted. It also
    /// must not reach `status_counts`, or the response-code table in the
    /// hourly check reports codes the site never served.
    #[test]
    fn a_failing_monitor_poll_does_not_pollute_traffic_figures() {
        let mut s = Stats::new();
        let ch = Channel::new(Version::Http11, Iface::External);
        s.record(500, false, 503, ch, crate::health::HEALTH_BACKEND);
        s.record(400, false, 401, ch, crate::health::PERF_BACKEND);
        assert_eq!(s.backend_errors_total, 0);
        assert_eq!(s.requests_total, 0);
        assert_eq!(s.monitor_requests_total, 2);
        assert!(
            s.snapshot().status_counts.is_empty(),
            "monitor status codes leaked into the site response-code table"
        );
    }

    #[test]
    fn test_record_and_counts() {
        let mut s = Stats::new();
        s.record(50, true, 200, Channel::new(Version::Http11, Iface::External), "m6-html");
        s.record(200, false, 200, Channel::new(Version::Http11, Iface::External), "m6-html");
        s.record(800, false, 200, Channel::new(Version::Http11, Iface::External), "m6-html");
        assert_eq!(s.requests_total, 3);
        assert_eq!(s.cache_hits_total, 1);
        assert_eq!(s.cache_misses_total, 2);
        assert_eq!(s.hit_count, 1);
        assert_eq!(s.miss_count, 2);
        assert_eq!(s.hit_samples[0], 50);
        assert_eq!(s.miss_samples[0], 200);
        assert_eq!(s.miss_samples[1], 800);
    }

    #[test]
    fn test_exact_percentiles() {
        let mut s = Stats::new();
        // 100 hit samples: 1..=100 ns
        for i in 1u64..=100 { s.record(i, true, 200, Channel::new(Version::Http11, Iface::External), "m6-html"); }
        let (p0, p50, p99, p100) = percentiles(&s.hit_samples, s.hit_count);
        assert_eq!(p0,   1);
        assert_eq!(p50,  50);
        assert_eq!(p99,  99);
        assert_eq!(p100, 100);
    }

    #[test]
    fn test_record_overhead() {
        let mut s = Stats::new();
        let start = Instant::now();
        for i in 1..=1000u64 { s.record(i, i % 2 == 0, 200, Channel::new(Version::Http11, Iface::External), "m6-html"); }
        let elapsed = start.elapsed();
        #[cfg(debug_assertions)]
        let threshold_us = 1_000;
        #[cfg(not(debug_assertions))]
        let threshold_us = 100;
        assert!(elapsed.as_micros() < threshold_us,
            "record() too slow: {}µs for 1000 calls", elapsed.as_micros());
    }
}

#[cfg(test)]
mod channel_tests {
    use super::*;

    #[test]
    fn version_comes_from_the_wire_string() {
        assert_eq!(Version::from_wire("HTTP/1.1"), Version::Http11);
        assert_eq!(Version::from_wire("HTTP/1.0"), Version::Http11);
        // Both spellings occur in this codebase; both must map to h2.
        assert_eq!(Version::from_wire("HTTP/2"), Version::Http2);
        assert_eq!(Version::from_wire("HTTP/2.0"), Version::Http2);
        assert_eq!(Version::from_wire("HTTP/3"), Version::Http3);
        // Unknown falls back to 1.1 rather than panicking on hostile input.
        assert_eq!(Version::from_wire("garbage"), Version::Http11);
    }

    #[test]
    fn interface_is_classified_from_the_bind_address() {
        // The WireGuard backbone between nodes.
        assert_eq!(Iface::for_bind("10.0.0.1:80"), Iface::Internal);
        assert_eq!(Iface::for_bind("192.168.1.5:80"), Iface::Internal);
        assert_eq!(Iface::for_bind("172.16.0.1:80"), Iface::Internal);
        assert_eq!(Iface::for_bind("127.0.0.1:8080"), Iface::Internal);
        // Public listeners.
        assert_eq!(Iface::for_bind("149.28.160.27:443"), Iface::External);
        assert_eq!(Iface::for_bind("0.0.0.0:443"), Iface::External);
        // 172.32 is OUTSIDE the private 172.16/12 block; treating the whole
        // 172/8 as private would misclassify real public addresses.
        assert_eq!(Iface::for_bind("172.32.0.1:443"), Iface::External);
        assert_eq!(Iface::for_bind("172.15.0.1:443"), Iface::External);
    }

    #[test]
    fn channel_index_round_trips_for_every_combination() {
        for version in [Version::Http11, Version::Http2, Version::Http3] {
            for iface in [Iface::External, Iface::Internal] {
                let ch = Channel::new(version, iface);
                assert!(ch.index() < CHANNELS);
                assert_eq!(Channel::from_index(ch.index()), ch);
            }
        }
    }

    /// The point of the whole exercise: an origin's public visitor traffic and
    /// its WireGuard cache-miss forwards must not land in the same bucket.
    #[test]
    fn traffic_is_separated_by_version_and_interface() {
        let mut stats = Stats::new();
        let public_h2 = Channel::new(Version::Http2, Iface::External);
        let tunnel_h2 = Channel::new(Version::Http2, Iface::Internal);

        // A fast public cache hit.
        stats.record(3_000, true, 200, public_h2, "m6-html");
        // Two slow intercontinental misses over the tunnel.
        stats.record(200_000_000, false, 200, tunnel_h2, "m6-html");
        stats.record(210_000_000, false, 404, tunnel_h2, "m6-html");

        let snap = stats.snapshot();
        assert_eq!(snap.requests_total, 3);
        // Only channels that saw traffic are reported.
        assert_eq!(snap.channels.len(), 2);

        let pubc = snap.channels.iter().find(|c| c.iface == "external").unwrap();
        assert_eq!(pubc.channel, "http/2/external");
        assert_eq!((pubc.requests, pubc.hits, pubc.misses), (1, 1, 0));
        assert_eq!(pubc.hit_p50_ns, 3_000);

        let tun = snap.channels.iter().find(|c| c.iface == "internal").unwrap();
        assert_eq!(tun.channel, "http/2/internal");
        assert_eq!((tun.requests, tun.hits, tun.misses), (2, 0, 2));
        assert!(tun.miss_p50_ns >= 200_000_000);

        // Pooled, the median would sit at 200ms and describe neither.
        assert!(pubc.hit_p50_ns * 1000 < tun.miss_p50_ns);
    }

    #[test]
    fn backend_errors_are_attributed_to_their_channel() {
        let mut stats = Stats::new();
        let h1 = Channel::new(Version::Http11, Iface::External);
        stats.record(1_000_000, false, 502, h1, "m6-html");
        let snap = stats.snapshot();
        assert_eq!(snap.backend_errors_total, 1);
        let c = snap.channels.iter().find(|c| c.channel == "http/1.1/external").unwrap();
        assert_eq!(c.backend_errors, 1);
    }

    #[test]
    fn silent_channels_are_omitted_not_zero_filled() {
        let mut stats = Stats::new();
        stats.record(1_000, true, 200, Channel::new(Version::Http3, Iface::External), "m6-html");
        let snap = stats.snapshot();
        assert_eq!(snap.channels.len(), 1, "a node reports only what it serves");
        assert_eq!(snap.channels[0].channel, "http/3/external");
    }
}

#[cfg(test)]
mod status_code_tests {
    use super::*;

    fn ch() -> Channel {
        Channel::new(Version::Http2, Iface::External)
    }

    #[test]
    fn only_codes_actually_emitted_are_reported() {
        let mut s = Stats::new();
        for _ in 0..5 { s.record(1_000, true, 200, ch(), "m6-html"); }
        for _ in 0..3 { s.record(2_000, false, 404, ch(), "m6-html"); }
        s.record(3_000, true, 304, ch(), "m6-html");

        let snap = s.snapshot();
        // Exactly the three codes seen -- not 500 rows of zeros around them.
        assert_eq!(snap.status_counts.len(), 3);
        assert_eq!(snap.status_counts.get(&200), Some(&5));
        assert_eq!(snap.status_counts.get(&404), Some(&3));
        assert_eq!(snap.status_counts.get(&304), Some(&1));
        assert_eq!(snap.status_counts.get(&500), None);
    }

    /// `backend_errors_total` used to be a separate bool argument that every
    /// call site derived as `status >= 500`. It is now derived once, here.
    #[test]
    fn backend_errors_are_derived_from_the_status() {
        let mut s = Stats::new();
        s.record(1_000, false, 200, ch(), "m6-html");
        s.record(1_000, false, 404, ch(), "m6-html");
        s.record(1_000, false, 499, ch(), "m6-html");
        s.record(1_000, false, 500, ch(), "m6-html");
        s.record(1_000, false, 503, ch(), "m6-html");
        let snap = s.snapshot();
        assert_eq!(snap.backend_errors_total, 2, "only 5xx counts as a backend error");
        assert_eq!(snap.status_counts.get(&499), Some(&1));
    }

    /// The index is `status - 100`, so anything outside 100..=599 must be
    /// rejected rather than trusted as an offset.
    #[test]
    fn out_of_range_statuses_do_not_index_the_table() {
        let mut s = Stats::new();
        s.record(1_000, false, 0, ch(), "m6-html");
        s.record(1_000, false, 99, ch(), "m6-html");
        s.record(1_000, false, 600, ch(), "m6-html");
        s.record(1_000, false, u16::MAX, ch(), "m6-html");
        let snap = s.snapshot();
        assert!(snap.status_counts.is_empty(), "no bogus code may be counted");
        // The request itself is still counted; only the code is discarded.
        assert_eq!(snap.requests_total, 4);
    }

    #[test]
    fn boundaries_are_inclusive_at_100_and_599() {
        let mut s = Stats::new();
        s.record(1_000, false, 100, ch(), "m6-html");
        s.record(1_000, false, 599, ch(), "m6-html");
        let snap = s.snapshot();
        assert_eq!(snap.status_counts.get(&100), Some(&1));
        assert_eq!(snap.status_counts.get(&599), Some(&1));
    }
}

#[cfg(test)]
mod backend_error_attribution_tests {
    use super::*;

    fn ch() -> Channel { Channel::new(Version::Http2, Iface::External) }

    /// The regression this closes, seen on all three nodes for nine
    /// consecutive hours: a bot sends an unrecognised verb, method validation
    /// answers 501 without contacting anything, and backend_errors_total
    /// rises. An operator watching that counter goes hunting for a failing
    /// renderer that was never involved.
    #[test]
    fn self_generated_5xx_is_not_a_backend_error() {
        let mut s = Stats::new();
        s.record(1_000, false, 501, ch(), "method-check");
        s.record(1_000, false, 500, ch(), "error-local");
        // Was `"health"`. Changed 2026-09-10, when /health and /perf stopped
        // being ordinary self-generated responses and became separately
        // accounted monitoring polls: they no longer reach `requests_total`
        // at all, so using one here would assert the opposite of the intended
        // behaviour. `error` is self-generated and is site traffic, which is
        // what this test is actually about. The monitoring case is covered by
        // `monitor_polls_are_counted_separately_from_site_traffic`.
        s.record(1_000, false, 503, ch(), "error");
        let snap = s.snapshot();
        assert_eq!(snap.backend_errors_total, 0, "m6 generated these itself");
        // The responses are still counted; only the attribution changes.
        assert_eq!(snap.requests_total, 3);
        assert_eq!(snap.status_counts.get(&501), Some(&1));
    }

    /// A real backend failure must still register.
    #[test]
    fn a_real_backend_5xx_still_counts() {
        let mut s = Stats::new();
        s.record(1_000, false, 502, ch(), "m6-html");
        s.record(1_000, false, 500, ch(), "origin");
        assert_eq!(s.snapshot().backend_errors_total, 2);
    }

    /// A replayed 5xx from cache is a stored copy of one old failure, not a
    /// new one; counting it again would inflate the total on every hit.
    #[test]
    fn a_cached_5xx_is_not_recounted() {
        let mut s = Stats::new();
        for _ in 0..10 { s.record(1_000, true, 500, ch(), "cache"); }
        assert_eq!(s.snapshot().backend_errors_total, 0);
    }
}
