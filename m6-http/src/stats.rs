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

    /// Per (version, interface) breakdown. Fixed-size dense table rather than
    /// a map: six entries, indexed arithmetically, no allocation and no hash
    /// on the request path.
    channels: Vec<ChannelStats>,

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
            channels: (0..CHANNELS).map(|_| ChannelStats::new()).collect(),
            rps_peak: 0, window_start: now, last_emit: now,
        }
    }

    #[inline(always)]
    pub fn record(
        &mut self,
        elapsed_ns: u64,
        cache_hit: bool,
        backend_error: bool,
        channel: Channel,
    ) {
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
            "periodic stats"
        );

        // Reset window
        self.window_requests = 0; self.window_cache_hits = 0;
        self.window_cache_misses = 0; self.window_backend_errors = 0;
        self.hit_idx = 0; self.hit_count = 0;
        self.miss_idx = 0; self.miss_count = 0;
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
        StatsSnapshot {
            requests_total: self.requests_total,
            cache_hits_total: self.cache_hits_total,
            cache_misses_total: self.cache_misses_total,
            backend_errors_total: self.backend_errors_total,
            rps_peak: self.rps_peak,
            hit_samples: self.hit_count,
            hit_p50_ns: hp50,
            hit_p99_ns: hp99,
            hit_max_ns: hmax,
            miss_samples: self.miss_count,
            miss_p50_ns: mp50,
            miss_p99_ns: mp99,
            miss_max_ns: mmax,
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

    #[test]
    fn test_record_and_counts() {
        let mut s = Stats::new();
        s.record(50, true, false, Channel::new(Version::Http11, Iface::External));
        s.record(200, false, false, Channel::new(Version::Http11, Iface::External));
        s.record(800, false, false, Channel::new(Version::Http11, Iface::External));
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
        for i in 1u64..=100 { s.record(i, true, false, Channel::new(Version::Http11, Iface::External)); }
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
        for i in 1..=1000u64 { s.record(i, i % 2 == 0, false, Channel::new(Version::Http11, Iface::External)); }
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
        stats.record(3_000, true, false, public_h2);
        // Two slow intercontinental misses over the tunnel.
        stats.record(200_000_000, false, false, tunnel_h2);
        stats.record(210_000_000, false, false, tunnel_h2);

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
        stats.record(1_000_000, false, true, h1);
        let snap = stats.snapshot();
        assert_eq!(snap.backend_errors_total, 1);
        let c = snap.channels.iter().find(|c| c.channel == "http/1.1/external").unwrap();
        assert_eq!(c.backend_errors, 1);
    }

    #[test]
    fn silent_channels_are_omitted_not_zero_filled() {
        let mut stats = Stats::new();
        stats.record(1_000, true, false, Channel::new(Version::Http3, Iface::External));
        let snap = stats.snapshot();
        assert_eq!(snap.channels.len(), 1, "a node reports only what it serves");
        assert_eq!(snap.channels[0].channel, "http/3/external");
    }
}
