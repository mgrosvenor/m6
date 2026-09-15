/// Performance statistics for the critical path.
use std::time::Instant;

const EMIT_INTERVAL_SECS: u64 = 10;

/// Number of latency samples kept per window per category (hit / miss).
/// Ring-buffer: oldest sample is overwritten when full.
/// 4096 × 8 bytes = 32 KB per reservoir.
const RESERVOIR: usize = 4096;

/// Per-channel reservoir. Smaller than the aggregate one on purpose, but not as
/// small as it was: six channels × three categories (hit, miss, handshake) at
/// 1024 u64s is 144 KB, against 72 KB at 512 and 1.1 MB at the aggregate 4096.
///
/// 1024 rather than 512 on the owner's call. The extra 72 KB is nothing on these
/// VMs and it doubles the window a p99 is drawn from, which matters most for
/// handshakes: they arrive far less often than requests, so a fixed number of
/// samples covers a much longer stretch of wall clock time on that channel than
/// it does for cache hits.
const CHANNEL_RESERVOIR: usize = 1024;

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
        if private {
            Iface::Internal
        } else {
            Iface::External
        }
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
        let iface = if i % 2 == 1 {
            Iface::Internal
        } else {
            Iface::External
        };
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
    /// TLS or QUIC handshake durations for connections on this channel.
    ///
    /// Per channel and NEVER aggregated across channels, because the three are
    /// not the same measurement:
    ///
    ///   http/1.1 and http/2  rustls, timed from `ServerConnection::new` to
    ///                        `!is_handshaking()`. EXCLUDES the TCP round trip,
    ///                        which happened before rustls saw the socket.
    ///   http/3               QUIC via quiche, timed to `is_established()`.
    ///                        INCLUDES its equivalent of that round trip,
    ///                        because QUIC folds transport and crypto together.
    ///
    /// A single "handshake p50" over all three would track the protocol mix
    /// rather than the cost of anything, which is the same error as the /perf
    /// aggregate fixed earlier.
    ///
    /// Split again by resumption, for exactly the same reason. A resumed
    /// handshake skips the certificate and the signature and is far cheaper, so a
    /// blended figure moves when the mix of returning and first-time visitors
    /// moves while neither cost has changed. rustls with the `std` feature
    /// defaults to a 256-session store, so this is already happening on h1 and
    /// h2, and h3 resumption became common when 0-RTT was enabled.
    ///
    /// The ratio of the two `total` counts is the resumption rate, so splitting
    /// loses nothing: the mix is recoverable from the parts, where the parts are
    /// not recoverable from a blend.
    handshake_full: DurationStats,
    handshake_resumed: DurationStats,
}

/// A bounded reservoir of durations, plus the lifetime figures a reservoir cannot
/// keep.
///
/// One type used by both handshake reservoirs rather than two copies of the ring
/// arithmetic. The hit and miss reservoirs above predate this and still inline the
/// same pattern; they could adopt it, but they sit on the per-request hot path and
/// changing them is not part of this.
struct DurationStats {
    /// Only the most recent `CHANNEL_RESERVOIR` durations. The percentiles
    /// describe these and nothing older.
    ring: Box<[u64; CHANNEL_RESERVOIR]>,
    idx: usize,
    /// Saturates at the ring size on purpose: it says how many samples the
    /// percentiles came from.
    ring_len: usize,
    /// Every duration ever recorded. Never reset, never capped. Reporting only
    /// `ring_len` would print "1024 samples" forever on a node serving millions.
    total: u64,
    /// For a lifetime mean the ring cannot give. At 10ms a handshake this
    /// overflows u64 after ~6e10 handshakes, which a 1-core VM will not reach.
    sum_ns: u64,
    /// Lifetime extremes, which survive the ring overwriting. `min` starts at
    /// u64::MAX as a "nothing yet" sentinel that must never reach a reader: it
    /// would render as 18 billion milliseconds.
    min_ns: u64,
    max_ns: u64,
}

impl DurationStats {
    fn new() -> DurationStats {
        DurationStats {
            ring: Box::new([0u64; CHANNEL_RESERVOIR]),
            idx: 0,
            ring_len: 0,
            total: 0,
            sum_ns: 0,
            min_ns: u64::MAX,
            max_ns: 0,
        }
    }

    fn record(&mut self, elapsed_ns: u64) {
        if elapsed_ns == 0 {
            return;
        }
        // Ring first, for the percentiles.
        self.ring[self.idx] = elapsed_ns;
        self.idx = (self.idx + 1) % CHANNEL_RESERVOIR;
        if self.ring_len < CHANNEL_RESERVOIR {
            self.ring_len += 1;
        }
        // Then the lifetime figures, which the ring overwrite cannot touch.
        // saturating_add rather than wrapping: on the one machine where this
        // could ever overflow, a stuck maximum is a readable wrong answer and a
        // wrapped one is not.
        self.total = self.total.saturating_add(1);
        self.sum_ns = self.sum_ns.saturating_add(elapsed_ns);
        self.min_ns = self.min_ns.min(elapsed_ns);
        self.max_ns = self.max_ns.max(elapsed_ns);
    }

    /// The reportable form. Zero throughout when nothing has been recorded, and
    /// the sentinel never escapes.
    fn snapshot(&self) -> m6_core::telemetry::HandshakeStats {
        let (_, p50, p99, _) = percentiles_n(&self.ring[..], self.ring_len);
        m6_core::telemetry::HandshakeStats {
            samples: self.ring_len,
            p50_ns: p50,
            p99_ns: p99,
            total: self.total,
            mean_ns: self.sum_ns.checked_div(self.total).unwrap_or(0),
            min_ns: if self.total > 0 { self.min_ns } else { 0 },
            max_ns: self.max_ns,
        }
    }
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
            handshake_full: DurationStats::new(),
            handshake_resumed: DurationStats::new(),
        }
    }

    fn record_handshake(&mut self, elapsed_ns: u64, resumed: bool) {
        if resumed {
            self.handshake_resumed.record(elapsed_ns);
        } else {
            self.handshake_full.record(elapsed_ns);
        }
    }

    /// Any handshake at all, either kind. Used only to decide whether a channel
    /// is worth reporting.
    fn handshakes_seen(&self) -> u64 {
        self.handshake_full.total + self.handshake_resumed.total
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

// The snapshot types moved to `m6_core::telemetry`, so the nodes that write
// them and anything that reads them share one definition. `Stats` still builds
// them; only the shape is shared.
pub use m6_core::telemetry::{ChannelSnapshot, StatsSnapshot};

pub struct Stats {
    // Cumulative
    pub requests_total: u64,
    pub cache_hits_total: u64,
    pub cache_misses_total: u64,
    pub backend_errors_total: u64,
    /// Errors attributed to the BACKEND that produced them.
    ///
    /// The total alone says "3 backend errors since start" and leaves the operator
    /// to guess which service. That guess matters most for the one that sends mail:
    /// a contact-form submission whose SMTP send fails returns 500, so it is
    /// counted here and nowhere else, and "render-contact: 3" is the difference
    /// between noticing silent mail loss and not.
    ///
    /// A small map rather than a fixed array: backend names come from config and
    /// this deployment has six. Only backends that have actually errored appear.
    backend_errors_by_name: std::collections::BTreeMap<String, u64>,

    // Window counters (reset each emit)
    window_requests: u64,
    window_cache_hits: u64,
    window_cache_misses: u64,
    window_backend_errors: u64,

    // Raw latency samples — ring buffers, one per category.
    //
    // ── These are NOT reset on emit, and that is the fix for a real defect ──
    //
    // They used to be. `maybe_emit` set every `_idx` and `_count` back to 0
    // every ten seconds, and `snapshot()` -- which is what `/perf` serves --
    // read those same fields. So `/perf` reported the percentiles of whatever
    // fraction of a ten-second window happened to be open when it was scraped.
    //
    // **On a site taking a couple of requests a minute that is almost always
    // nothing.** Observed on syd, 2026-09-14: `cache_hits_total: 338` beside
    // `hit_samples: 0, hit_p50_ns: 0, hit_p99_ns: 0`. m6-monitor faithfully
    // turned zero samples into `null`, so the fleet digest carried no latency
    // at all, on any node, and never had. The one number the monitor exists to
    // trend was structurally absent, while the periodic log ten lines above was
    // printing 3,878ns for the same counter.
    //
    // So the ring now runs as a ring: never cleared, wrapping at RESERVOIR,
    // holding the most recent samples however long they took to arrive.
    // `snapshot()` reads all of it. The periodic log still reports its own
    // ten-second window, from `*_window_added` below, so the operational
    // logging is unchanged -- which is what `snapshot()`'s own comment was
    // protecting when it declined to reset. Declining to reset was right; also
    // reading the window the emitter reset was the bug.
    //
    // No extra memory and no extra work on the request path: one reservoir,
    // one store per sample, exactly as before.
    hit_samples: Box<[u64; RESERVOIR]>,
    hit_idx: usize,
    hit_count: usize, // total held, capped at RESERVOIR
    /// Samples added since the last emit. The periodic log's window.
    hit_window_added: usize,

    miss_samples: Box<[u64; RESERVOIR]>,
    miss_idx: usize,
    miss_count: usize,
    miss_window_added: usize,

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
    window_monitor_requests: u64,
    monitor_samples: Box<[u64; RESERVOIR]>,
    monitor_idx: usize,
    monitor_count: usize,
    monitor_window_added: usize,

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
    last_emit: Instant,
}

impl Default for Stats {
    fn default() -> Self {
        Self::new()
    }
}

impl Stats {
    pub fn new() -> Self {
        let now = Instant::now();
        Stats {
            requests_total: 0,
            cache_hits_total: 0,
            cache_misses_total: 0,
            backend_errors_total: 0,
            backend_errors_by_name: std::collections::BTreeMap::new(),
            window_requests: 0,
            window_cache_hits: 0,
            window_cache_misses: 0,
            window_backend_errors: 0,
            hit_samples: Box::new([0u64; RESERVOIR]),
            hit_idx: 0,
            hit_count: 0,
            hit_window_added: 0,
            miss_samples: Box::new([0u64; RESERVOIR]),
            miss_idx: 0,
            miss_count: 0,
            miss_window_added: 0,
            monitor_requests_total: 0,
            window_monitor_requests: 0,
            monitor_samples: Box::new([0u64; RESERVOIR]),
            monitor_idx: 0,
            monitor_count: 0,
            monitor_window_added: 0,
            channels: (0..CHANNELS).map(|_| ChannelStats::new()).collect(),
            status_counts: Box::new([0u64; 500]),
            rps_peak: 0,
            window_start: now,
            last_emit: now,
        }
    }

    /// Record one completed handshake on a channel.
    ///
    /// Separate from `record`, and called at a different time: a handshake
    /// happens once per CONNECTION and a request many times within it. Folding
    /// it into `record` would have meant either timing it per request, which is
    /// meaningless, or carrying it on the request path, which is the one place
    /// this project does not add work.
    pub fn record_handshake(&mut self, elapsed_ns: u64, channel: Channel, resumed: bool) {
        self.channels[channel.index()].record_handshake(elapsed_ns, resumed);
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
                self.monitor_window_added += 1;
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
        self.requests_total += 1;
        self.window_requests += 1;

        if elapsed_ns > 0 {
            if cache_hit {
                self.cache_hits_total += 1;
                self.window_cache_hits += 1;
                self.hit_samples[self.hit_idx] = elapsed_ns;
                self.hit_idx = (self.hit_idx + 1) & (RESERVOIR - 1);
                if self.hit_count < RESERVOIR {
                    self.hit_count += 1;
                }
                self.hit_window_added += 1;
            } else {
                self.cache_misses_total += 1;
                self.window_cache_misses += 1;
                self.miss_samples[self.miss_idx] = elapsed_ns;
                self.miss_idx = (self.miss_idx + 1) & (RESERVOIR - 1);
                if self.miss_count < RESERVOIR {
                    self.miss_count += 1;
                }
                self.miss_window_added += 1;
            }
        } else if cache_hit {
            self.cache_hits_total += 1;
            self.window_cache_hits += 1;
        } else {
            self.cache_misses_total += 1;
            self.window_cache_misses += 1;
        }

        if backend_error {
            self.backend_errors_total += 1;
            // Keyed by the backend m6-http actually dispatched to, so the name in
            // the report is the name in the config rather than a guess.
            *self
                .backend_errors_by_name
                .entry(backend.to_string())
                .or_insert(0) += 1;
            self.window_backend_errors += 1;
        }
    }

    #[inline]
    pub fn maybe_emit(&mut self, pool_members: usize) {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_emit);
        if elapsed.as_secs() < EMIT_INTERVAL_SECS {
            return;
        }

        let elapsed_secs = elapsed.as_secs_f64().max(0.001);
        let rps_avg = (self.window_requests as f64 / elapsed_secs) as u64;
        if rps_avg > self.rps_peak {
            self.rps_peak = rps_avg;
        }

        let total_window = self.window_cache_hits + self.window_cache_misses;
        let cache_hit_rate = if total_window > 0 {
            self.window_cache_hits as f64 / total_window as f64
        } else {
            0.0
        };

        // THIS WINDOW only, which is what a periodic line means. `*_window_added`
        // rather than `*_count`: the reservoirs are no longer cleared here, so
        // the count is everything the ring holds and would turn each line into a
        // running average.
        let (hp0, hp50, hp99, hp100) = percentiles_ring(
            &self.hit_samples,
            self.hit_idx,
            self.hit_count,
            self.hit_window_added,
        );
        let (mp0, mp50, mp99, mp100) = percentiles_ring(
            &self.miss_samples,
            self.miss_idx,
            self.miss_count,
            self.miss_window_added,
        );
        let (_, kp50, kp99, _) = percentiles_ring(
            &self.monitor_samples,
            self.monitor_idx,
            self.monitor_count,
            self.monitor_window_added,
        );

        tracing::info!(
            requests = self.requests_total,
            rps_avg = rps_avg,
            rps_peak = self.rps_peak,
            cache_hits = self.window_cache_hits,
            cache_misses = self.window_cache_misses,
            cache_hit_rate = format_args!("{:.4}", cache_hit_rate),
            backend_errors = self.window_backend_errors,
            pool_members = pool_members,
            // The sample count beside the percentiles, not just the numbers.
            // `hit_p50_ns` is load-dependent (docs/PERFORMANCE.md §4: 3,900ns at
            // 50-70 hits in a window, 1,064ns at ~1,200), so a percentile with no
            // count attached cannot be compared to anything, and a p50 over one
            // sample reads exactly like a p50 over a thousand. This is also not
            // the same number as `cache_hits`: a request timed at 0ns is counted
            // as a hit and contributes no sample.
            hit_samples = self.hit_window_added,
            hit_p0_ns = hp0,
            hit_p50_ns = hp50,
            hit_p99_ns = hp99,
            hit_max_ns = hp100,
            miss_samples = self.miss_window_added,
            miss_p0_ns = mp0,
            miss_p50_ns = mp50,
            miss_p99_ns = mp99,
            miss_max_ns = mp100,
            // Monitoring endpoints, deliberately outside every counter above.
            // Reported so a monitor that stops polling, or one that starts
            // flooding, is visible; a reader can tell those apart from a
            // traffic change because these never move the traffic figures.
            monitor_requests = self.window_monitor_requests,
            monitor_p50_ns = kp50,
            monitor_p99_ns = kp99,
            "periodic stats"
        );

        // Reset the window COUNTERS. The reservoirs are deliberately left alone:
        // see the note on `hit_samples`. Clearing `hit_idx`/`hit_count` here is
        // what left `/perf` with nothing to report, on every node, permanently.
        self.window_requests = 0;
        self.window_cache_hits = 0;
        self.window_cache_misses = 0;
        self.window_backend_errors = 0;
        self.window_monitor_requests = 0;
        self.hit_window_added = 0;
        self.miss_window_added = 0;
        self.monitor_window_added = 0;
        self.window_start = now;
        self.last_emit = now;
    }
}

impl Stats {
    /// Snapshot without mutating anything.
    ///
    /// Takes `&self` deliberately. `maybe_emit` both reports and resets; if
    /// the health endpoint shared that path, every scrape would clear the
    /// window and the periodic stats log would report only the traffic that
    /// arrived between scrapes. A monitor polling every 30 seconds would
    /// silently gut the operational logging it exists to complement.
    ///
    /// **That reasoning was right and incomplete, and the gap was the defect.**
    /// Not resetting was correct; reading the window that `maybe_emit` DID reset
    /// was not. `/perf` served the percentiles of a partial ten-second window,
    /// which on a site taking a couple of requests a minute is almost always
    /// empty, so it reported zeros and m6-monitor showed `null` latency on every
    /// node. See the note on `hit_samples`.
    ///
    /// The percentiles here now span **the most recent up to `RESERVOIR`
    /// samples, not a period of time.** On a quiet node that can reach back
    /// hours and will blend idle and busy traffic, which matters because this
    /// number is load-dependent (`docs/PERFORMANCE.md` §4). `hit_samples` is
    /// reported beside it for exactly that reason: it is the only thing that
    /// makes the percentile interpretable, and zero samples means "not
    /// measured" rather than "zero nanoseconds". For the fine-grained view, the
    /// periodic log still reports per-window figures every ten seconds.
    pub fn snapshot(&self) -> StatsSnapshot {
        let (_, hp50, hp99, hmax) =
            percentiles_ring(&self.hit_samples, self.hit_idx, self.hit_count, RESERVOIR);
        let (_, mp50, mp99, mmax) = percentiles_ring(
            &self.miss_samples,
            self.miss_idx,
            self.miss_count,
            RESERVOIR,
        );
        let (_, kp50, kp99, _) = percentiles_ring(
            &self.monitor_samples,
            self.monitor_idx,
            self.monitor_count,
            RESERVOIR,
        );
        StatsSnapshot {
            requests_total: self.requests_total,
            cache_hits_total: self.cache_hits_total,
            cache_misses_total: self.cache_misses_total,
            backend_errors_total: self.backend_errors_total,
            backend_errors_by_name: self
                .backend_errors_by_name
                .iter()
                .map(|(k, v)| (k.clone(), *v))
                .collect(),
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
                // Requests OR handshakes. A channel can have a completed
                // handshake and no request yet: a scanner that connects and
                // disconnects, a connection still in flight, or a client that
                // gave up after the TLS exchange. Filtering on requests alone
                // recorded those handshakes and then dropped them from the
                // report, which a test caught by asking for a channel that had
                // one sample and no traffic.
                .filter(|(_, c)| c.requests > 0 || c.handshakes_seen() > 0)
                .map(|(i, c)| {
                    let ch = Channel::from_index(i);
                    let (_, hp50, hp99, _) = percentiles_n(&c.hit_samples[..], c.hit_count);
                    let (_, mp50, mp99, _) = percentiles_n(&c.miss_samples[..], c.miss_count);

                    ChannelSnapshot {
                        channel: ch.label(),
                        version: ch.version.as_str().to_string(),
                        iface: ch.iface.as_str().to_string(),
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
                        // Full and resumed separately, never combined. The
                        // percentiles, the lifetime count, the mean and the
                        // extremes are all derived inside DurationStats so every
                        // consumer divides and guards the same way.
                        handshake_full: c.handshake_full.snapshot(),
                        handshake_resumed: c.handshake_resumed.snapshot(),
                    }
                })
                .collect(),
        }
    }
}

/// Slice-based percentiles, for the per-channel reservoirs (which are a
/// different fixed size from the aggregate ones).
fn percentiles_n(samples: &[u64], n: usize) -> (u64, u64, u64, u64) {
    if n == 0 {
        return (0, 0, 0, 0);
    }
    let mut buf: Vec<u64> = samples[..n].to_vec();
    buf.sort_unstable();
    (
        buf[0],
        buf[(n - 1) * 50 / 100],
        buf[(n - 1) * 99 / 100],
        buf[n - 1],
    )
}

/// Sort the first `n` samples and return exact (p0, p50, p99, p100).
/// Percentiles over the `take` most recently written entries of a ring buffer.
///
/// `idx` is where the next write will go, so the newest sample is at `idx - 1`
/// and the run of `take` newest ends there. `held` caps it: a ring that has seen
/// fewer samples than `take` has only what it has.
///
/// This replaced a version that read `samples[..n]` from the front. That was
/// correct only while `maybe_emit` reset `idx` to 0 every ten seconds, which is
/// the reset that made `/perf` report nothing. Reading from the front of a ring
/// that genuinely wraps would silently report the OLDEST samples as if they were
/// the window, so the two changes had to go together.
///
/// Returns `(p0, p50, p99, p100)`, and `(0, 0, 0, 0)` when there is nothing to
/// measure. The caller reports the sample count beside these, which is what
/// distinguishes "0 ns" from "not measured" -- a distinction this endpoint
/// needs, because it had been reporting the first while meaning the second.
fn percentiles_ring(
    samples: &[u64; RESERVOIR],
    idx: usize,
    held: usize,
    take: usize,
) -> (u64, u64, u64, u64) {
    let n = take.min(held).min(RESERVOIR);
    if n == 0 {
        return (0, 0, 0, 0);
    }
    let mut buf: Vec<u64> = Vec::with_capacity(n);
    // Walk back from the newest. `+ RESERVOIR` keeps the subtraction in usize.
    for k in 1..=n {
        buf.push(samples[(idx + RESERVOIR - k) & (RESERVOIR - 1)]);
    }
    buf.sort_unstable();
    let p0 = buf[0];
    let p50 = buf[(n - 1) * 50 / 100];
    let p99 = buf[(n - 1) * 99 / 100];
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
        assert_eq!(
            s.requests_total, 1,
            "monitor polls leaked into requests_total"
        );
        assert_eq!(
            s.cache_misses_total, 1,
            "monitor polls leaked into the miss count"
        );
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
        s.record(
            50,
            true,
            200,
            Channel::new(Version::Http11, Iface::External),
            "m6-html",
        );
        s.record(
            200,
            false,
            200,
            Channel::new(Version::Http11, Iface::External),
            "m6-html",
        );
        s.record(
            800,
            false,
            200,
            Channel::new(Version::Http11, Iface::External),
            "m6-html",
        );
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
        for i in 1u64..=100 {
            s.record(
                i,
                true,
                200,
                Channel::new(Version::Http11, Iface::External),
                "m6-html",
            );
        }
        let (p0, p50, p99, p100) =
            percentiles_ring(&s.hit_samples, s.hit_idx, s.hit_count, RESERVOIR);
        assert_eq!(p0, 1);
        assert_eq!(p50, 50);
        assert_eq!(p99, 99);
        assert_eq!(p100, 100);
    }

    #[test]
    fn test_record_overhead() {
        let mut s = Stats::new();
        let start = Instant::now();
        for i in 1..=1000u64 {
            s.record(
                i,
                i % 2 == 0,
                200,
                Channel::new(Version::Http11, Iface::External),
                "m6-html",
            );
        }
        let elapsed = start.elapsed();
        #[cfg(debug_assertions)]
        let threshold_us = 1_000;
        #[cfg(not(debug_assertions))]
        let threshold_us = 100;
        assert!(
            elapsed.as_micros() < threshold_us,
            "record() too slow: {}µs for 1000 calls",
            elapsed.as_micros()
        );
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

        let pubc = snap
            .channels
            .iter()
            .find(|c| c.iface == "external")
            .unwrap();
        assert_eq!(pubc.channel, "http/2/external");
        assert_eq!((pubc.requests, pubc.hits, pubc.misses), (1, 1, 0));
        assert_eq!(pubc.hit_p50_ns, 3_000);

        let tun = snap
            .channels
            .iter()
            .find(|c| c.iface == "internal")
            .unwrap();
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
        let c = snap
            .channels
            .iter()
            .find(|c| c.channel == "http/1.1/external")
            .unwrap();
        assert_eq!(c.backend_errors, 1);
    }

    #[test]
    fn silent_channels_are_omitted_not_zero_filled() {
        let mut stats = Stats::new();
        stats.record(
            1_000,
            true,
            200,
            Channel::new(Version::Http3, Iface::External),
            "m6-html",
        );
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
        for _ in 0..5 {
            s.record(1_000, true, 200, ch(), "m6-html");
        }
        for _ in 0..3 {
            s.record(2_000, false, 404, ch(), "m6-html");
        }
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
        assert_eq!(
            snap.backend_errors_total, 2,
            "only 5xx counts as a backend error"
        );
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
        assert!(
            snap.status_counts.is_empty(),
            "no bogus code may be counted"
        );
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

    fn ch() -> Channel {
        Channel::new(Version::Http2, Iface::External)
    }

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
        for _ in 0..10 {
            s.record(1_000, true, 500, ch(), "cache");
        }
        assert_eq!(s.snapshot().backend_errors_total, 0);
    }
}

/// `/perf` must report the latency it measured, not the latency of whichever
/// ten-second window happened to be open when it was scraped.
///
/// ## The defect these cover
///
/// `maybe_emit` reset `hit_idx` and `hit_count` to 0 every ten seconds, and
/// `snapshot()` -- which is what `/perf` serves -- read those same fields. On a
/// site taking a couple of requests a minute, almost every ten-second window
/// holds no cache hit at all, so `/perf` reported zeros essentially always.
///
/// Observed on syd, 2026-09-14: `cache_hits_total: 338` beside `hit_samples: 0,
/// hit_p50_ns: 0, hit_p99_ns: 0`. m6-monitor turned zero samples into `null`,
/// so the fleet digest carried NO LATENCY FOR ANY NODE and never had, while the
/// periodic log was printing 3,878ns for the same counter in the same minute.
///
/// Nothing caught it because every existing test recorded samples and read them
/// back **without an emit in between**, which is the one ordering where the old
/// code was correct. `an_emit_does_not_erase_what_perf_reports` is the test that
/// was missing, and it fails against the old implementation.
#[cfg(test)]
mod perf_reservoir_tests {
    use super::*;

    fn hit(s: &mut Stats, ns: u64) {
        s.record(
            ns,
            true,
            200,
            Channel::new(Version::Http2, Iface::External),
            "cache",
        );
    }

    /// The regression test. An emit between the traffic and the scrape used to
    /// leave `/perf` with nothing.
    #[test]
    fn an_emit_does_not_erase_what_perf_reports() {
        let mut s = Stats::new();
        for ns in [1_000, 2_000, 3_000, 4_000, 5_000] {
            hit(&mut s, ns);
        }
        // Force the window boundary the emitter would hit on a timer.
        s.last_emit = Instant::now() - std::time::Duration::from_secs(EMIT_INTERVAL_SECS + 1);
        s.maybe_emit(1);

        let snap = s.snapshot();
        assert_eq!(
            snap.hit_samples, 5,
            "the emit cleared the reservoir /perf reads. This is the defect: \
             m6-monitor showed null latency on every node because of it."
        );
        assert!(
            snap.hit_p50_ns > 0,
            "hit_p50_ns is {} with 5 samples recorded",
            snap.hit_p50_ns
        );
        assert_eq!(snap.hit_max_ns, 5_000);
        // The cumulative counter was never the problem and must not change.
        assert_eq!(snap.cache_hits_total, 5);
    }

    /// The periodic log keeps its per-window meaning, which is the property
    /// `snapshot()`'s comment was protecting when it declined to reset.
    ///
    /// Checked through the window counter the log reports rather than by
    /// capturing tracing output: after an emit the window is empty, and new
    /// traffic lands in the next window only.
    #[test]
    fn the_periodic_window_still_only_covers_its_own_window() {
        let mut s = Stats::new();
        for ns in [10, 20, 30] {
            hit(&mut s, ns);
        }
        assert_eq!(s.hit_window_added, 3);

        s.last_emit = Instant::now() - std::time::Duration::from_secs(EMIT_INTERVAL_SECS + 1);
        s.maybe_emit(1);
        assert_eq!(
            s.hit_window_added, 0,
            "the window counter must reset on emit, or every periodic line \
             becomes a running average instead of a window"
        );

        hit(&mut s, 40);
        assert_eq!(
            s.hit_window_added, 1,
            "new traffic belongs to the new window"
        );
        // And the ring kept everything, which is what /perf now reads.
        assert_eq!(s.snapshot().hit_samples, 4);
    }

    /// The window view and the `/perf` view genuinely differ after an emit.
    ///
    /// Both are computed from one reservoir now, so this is what proves the two
    /// readers are not accidentally sharing an answer.
    #[test]
    fn the_window_and_the_lifetime_view_differ() {
        let mut s = Stats::new();
        for _ in 0..10 {
            hit(&mut s, 1_000);
        }
        s.last_emit = Instant::now() - std::time::Duration::from_secs(EMIT_INTERVAL_SECS + 1);
        s.maybe_emit(1);
        // A slow second window.
        for _ in 0..10 {
            hit(&mut s, 9_000);
        }

        let window = percentiles_ring(&s.hit_samples, s.hit_idx, s.hit_count, s.hit_window_added);
        assert_eq!(
            window.1, 9_000,
            "the window should see only the slow samples"
        );
        let snap = s.snapshot();
        assert_eq!(snap.hit_samples, 20, "/perf should see both windows");
        assert!(
            snap.hit_p50_ns >= 1_000 && snap.hit_p50_ns <= 9_000,
            "lifetime p50 {} should sit between the two regimes",
            snap.hit_p50_ns
        );
    }

    /// Reading a wrapped ring must return the NEWEST samples, not the oldest.
    ///
    /// The old `percentiles` read `samples[..n]` from the front, which was only
    /// correct because the index was reset to 0 every window. Against a ring
    /// that genuinely wraps -- which it now does -- reading from the front
    /// reports the oldest samples as if they were current, so this had to change
    /// with the reset and is the half that would fail silently.
    #[test]
    fn a_wrapped_ring_reports_the_newest_samples() {
        let mut s = Stats::new();
        // Fill the ring with a slow value, then overwrite it all with a fast one.
        for _ in 0..RESERVOIR {
            hit(&mut s, 9_999);
        }
        assert_eq!(s.snapshot().hit_samples, RESERVOIR);
        assert_eq!(s.snapshot().hit_p50_ns, 9_999);

        for _ in 0..RESERVOIR {
            hit(&mut s, 111);
        }
        let snap = s.snapshot();
        assert_eq!(snap.hit_samples, RESERVOIR, "the ring is capped, not grown");
        assert_eq!(
            snap.hit_p50_ns, 111,
            "a wrapped ring reported stale samples as current"
        );
        assert_eq!(snap.cache_hits_total, (RESERVOIR * 2) as u64);
    }

    /// Zero samples must stay distinguishable from zero nanoseconds.
    ///
    /// `tools/conformance.sh`'s rule: a check that cannot measure must fail
    /// rather than print a number it did not take. The percentile is 0 when
    /// there is nothing to measure, so `hit_samples` is what carries the
    /// difference, and it has to be reported for the number to mean anything.
    #[test]
    fn no_samples_is_reported_as_no_samples() {
        let snap = Stats::new().snapshot();
        assert_eq!(snap.hit_samples, 0);
        assert_eq!(snap.hit_p50_ns, 0);
        assert_eq!(snap.cache_hits_total, 0);
    }

    /// A request timed at 0ns counts as a hit and contributes no sample, so the
    /// two numbers legitimately differ and neither is a substitute for the other.
    #[test]
    fn a_zero_nanosecond_request_counts_but_does_not_sample() {
        let mut s = Stats::new();
        hit(&mut s, 0);
        hit(&mut s, 5_000);
        let snap = s.snapshot();
        assert_eq!(snap.cache_hits_total, 2);
        assert_eq!(
            snap.hit_samples, 1,
            "the 0ns request must not enter the reservoir"
        );
        assert_eq!(snap.hit_p50_ns, 5_000);
    }
}

/// Handshake timing, and the separation that makes it meaningful.
///
/// Connection setup is measured per channel and never aggregated, because the
/// three protocols do not measure the same span:
///
///   h1 and h2  rustls, from `ServerConnection::new` to `!is_handshaking()`.
///              EXCLUDES the TCP round trip, already done before rustls saw the
///              socket.
///   h3         QUIC, to `is_established()`. INCLUDES the equivalent round trip,
///              because QUIC folds transport and crypto together.
///
/// A combined figure would track the protocol mix rather than the cost of
/// anything -- the same error as the `/perf` aggregate fixed earlier today.
#[cfg(test)]
mod handshake_tests {

    /// The reservoir overwrites. The lifetime figures must not.
    ///
    /// This is the property the owner asked for in as many words: do not throw
    /// values away and keep the count. Before this, `handshake_count` saturated
    /// at the reservoir size, so a node that had served a million handshakes
    /// reported "512 samples" and had silently forgotten every duration outside
    /// the last 512 -- including its worst.
    #[test]
    fn a_full_reservoir_does_not_lose_the_count_or_the_extremes() {
        let mut s = Stats::new();
        let c = ch(Version::Http11);

        // One deliberately slow handshake FIRST, then enough traffic to push it
        // out of the ring entirely.
        s.record_handshake(900_000_000, c, false);
        for _ in 0..(CHANNEL_RESERVOIR * 2) {
            s.record_handshake(1_000_000, c, false);
        }

        let snap = s.snapshot();
        let h = snap
            .channels
            .iter()
            .find(|x| x.channel == c.label())
            .expect("channel present");

        // The window is capped, and says so.
        assert_eq!(h.handshake_full.samples, CHANNEL_RESERVOIR);
        // The count is not capped.
        assert_eq!(h.handshake_full.total, (CHANNEL_RESERVOIR * 2 + 1) as u64);
        // The 900ms outlier is long gone from the ring, so the percentiles
        // cannot see it. That is expected and is exactly why max exists.
        assert!(
            h.handshake_full.p99_ns < 900_000_000,
            "outlier should have been evicted from the ring, p99 was {}",
            h.handshake_full.p99_ns
        );
        // And it is still on the record.
        assert_eq!(h.handshake_full.max_ns, 900_000_000);
        assert_eq!(h.handshake_full.min_ns, 1_000_000);
        // Mean over everything, not over the window.
        let expect_mean = (900_000_000 + 1_000_000 * (CHANNEL_RESERVOIR as u64 * 2))
            / (CHANNEL_RESERVOIR as u64 * 2 + 1);
        assert_eq!(h.handshake_full.mean_ns, expect_mean);
    }

    /// A channel with no handshake reports zero for min, not u64::MAX. The
    /// sentinel must never reach a reader, who would render it as 18 billion
    /// milliseconds and reasonably conclude the node was broken.
    #[test]
    fn the_min_sentinel_never_escapes() {
        let mut s = Stats::new();
        // Traffic but no handshake: an h2c or internal channel.
        s.record(1_000, false, 200, ch(Version::Http11), "");
        let snap = s.snapshot();
        for c in &snap.channels {
            assert_eq!(c.handshake_full.total, 0);
            assert_eq!(
                c.handshake_full.min_ns, 0,
                "channel {} leaked the sentinel",
                c.channel
            );
        }
    }

    use super::*;

    fn ch(v: Version) -> Channel {
        Channel::new(v, Iface::External)
    }

    #[test]
    fn a_handshake_is_recorded_on_its_own_channel() {
        let mut s = Stats::new();
        s.record_handshake(4_000_000, ch(Version::Http11), false);
        let snap = s.snapshot();
        let h1 = snap
            .channels
            .iter()
            .find(|c| c.channel == "http/1.1/external")
            .expect("h1 channel present once it has a sample");
        assert_eq!(h1.handshake_full.samples, 1);
        assert_eq!(h1.handshake_full.p50_ns, 4_000_000);
    }

    /// The property the whole design rests on.
    #[test]
    fn the_three_protocols_never_share_a_figure() {
        let mut s = Stats::new();
        // Deliberately far apart, as a real fleet would be: a resumed TLS
        // handshake and a QUIC one that includes a round trip are not close.
        s.record_handshake(1_000_000, ch(Version::Http11), false);
        s.record_handshake(2_000_000, ch(Version::Http2), false);
        s.record_handshake(90_000_000, ch(Version::Http3), false);

        let snap = s.snapshot();
        let get = |name: &str| {
            snap.channels
                .iter()
                .find(|c| c.channel == name)
                .unwrap_or_else(|| panic!("{name} missing"))
                .handshake_full
                .p50_ns
        };
        assert_eq!(get("http/1.1/external"), 1_000_000);
        assert_eq!(get("http/2/external"), 2_000_000);
        assert_eq!(get("http/3/external"), 90_000_000);
        // And the slow h3 figure has not contaminated the others, which is what
        // an aggregate would have done.
        assert!(get("http/1.1/external") < get("http/3/external"));
    }

    /// External and internal are different events even on one protocol: a
    /// browser handshake and one from the WireGuard backbone.
    #[test]
    fn interface_separates_them_too() {
        let mut s = Stats::new();
        s.record_handshake(5_000, Channel::new(Version::Http2, Iface::External), false);
        s.record_handshake(50_000, Channel::new(Version::Http2, Iface::Internal), false);
        let snap = s.snapshot();
        let ext = snap
            .channels
            .iter()
            .find(|c| c.channel == "http/2/external")
            .unwrap();
        let int = snap
            .channels
            .iter()
            .find(|c| c.channel == "http/2/internal")
            .unwrap();
        assert_eq!(ext.handshake_full.p50_ns, 5_000);
        assert_eq!(int.handshake_full.p50_ns, 50_000);
    }

    /// Zero samples must not read as a zero-nanosecond handshake. Same rule as
    /// the hit percentiles: the count is what distinguishes them.
    #[test]
    fn no_handshake_is_no_samples_not_zero_nanoseconds() {
        let mut s = Stats::new();
        // Traffic, but no handshake recorded -- a plaintext or reused connection.
        s.record(1_000, true, 200, ch(Version::Http11), "cache");
        let snap = s.snapshot();
        let h1 = snap
            .channels
            .iter()
            .find(|c| c.channel == "http/1.1/external")
            .unwrap();
        assert_eq!(h1.handshake_full.samples, 0);
        assert_eq!(h1.handshake_full.p50_ns, 0);
        // The request itself was still counted.
        assert_eq!(h1.requests, 1);
    }

    /// A zero duration is not a sample. The clock resolution is nanoseconds and a
    /// real handshake is microseconds at minimum, so a zero means "not measured".
    #[test]
    fn a_zero_duration_is_not_recorded() {
        let mut s = Stats::new();
        s.record_handshake(0, ch(Version::Http2), false);
        let snap = s.snapshot();
        assert!(
            snap.channels.iter().all(|c| c.handshake_full.samples == 0),
            "a zero duration must not enter the reservoir"
        );
    }
}
