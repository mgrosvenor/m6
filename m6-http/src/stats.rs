/// Performance statistics for the critical path.
///
/// Single-threaded: plain u64 fields (no atomics needed — the event loop
/// owns this exclusively). Emits a structured JSON log line every
/// EMIT_INTERVAL_SECS seconds.
use std::time::Instant;

/// Emit period.
const EMIT_INTERVAL_SECS: u64 = 10;

/// Latency histogram bucket upper bounds (nanoseconds).
/// 9 buckets: <100ns, <500ns, <1µs, <5µs, <10µs, <50µs, <100µs, <1ms, ≥1ms
const BOUNDS_NS: [u64; 8] = [100, 500, 1_000, 5_000, 10_000, 50_000, 100_000, 1_000_000];
pub const N_BUCKETS: usize = 9;

pub struct Stats {
    // Cumulative since process start
    pub requests_total: u64,
    pub cache_hits_total: u64,
    pub cache_misses_total: u64,
    pub backend_errors_total: u64,

    // Current emission window (reset on each emit)
    window_requests: u64,
    window_cache_hits: u64,
    window_cache_misses: u64,
    window_backend_errors: u64,
    /// Latency histogram for the current window (nanoseconds, N_BUCKETS buckets).
    latency_hist: [u64; N_BUCKETS],

    // RPS tracking
    pub rps_peak: u64,
    window_start: Instant,

    // Periodic emit
    last_emit: Instant,
}

impl Stats {
    pub fn new() -> Self {
        let now = Instant::now();
        Stats {
            requests_total: 0,
            cache_hits_total: 0,
            cache_misses_total: 0,
            backend_errors_total: 0,
            window_requests: 0,
            window_cache_hits: 0,
            window_cache_misses: 0,
            window_backend_errors: 0,
            latency_hist: [0; N_BUCKETS],
            rps_peak: 0,
            window_start: now,
            last_emit: now,
        }
    }

    /// Record one completed request. `elapsed_ns` is the software latency
    /// (from first byte parsed to response queued). `cache_hit` and
    /// `backend_error` are mutually exclusive.
    ///
    /// Hot path: 3 integer increments + 1 array write. ~5ns total.
    #[inline(always)]
    pub fn record(&mut self, elapsed_ns: u64, cache_hit: bool, backend_error: bool) {
        self.requests_total += 1;
        self.window_requests += 1;

        if cache_hit {
            self.cache_hits_total += 1;
            self.window_cache_hits += 1;
        } else {
            self.cache_misses_total += 1;
            self.window_cache_misses += 1;
        }
        if backend_error {
            self.backend_errors_total += 1;
            self.window_backend_errors += 1;
        }

        // Bucket lookup: linear scan over 8 bounds = ~3ns
        let bucket = BOUNDS_NS.iter().position(|&b| elapsed_ns < b).unwrap_or(N_BUCKETS - 1);
        self.latency_hist[bucket] += 1;
    }

    /// Check whether it's time to emit stats; if so, emit and reset window.
    /// Call once per event-loop iteration (very cheap when not emitting).
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

        let (p50_ns, p99_ns) = percentiles(&self.latency_hist);
        let total_window = self.window_cache_hits + self.window_cache_misses;
        let cache_hit_rate = if total_window > 0 {
            self.window_cache_hits as f64 / total_window as f64
        } else {
            0.0
        };

        tracing::info!(
            requests           = self.requests_total,
            rps_avg            = rps_avg,
            rps_peak           = self.rps_peak,
            latency_p50_us     = p50_ns / 1_000,
            latency_p99_us     = p99_ns / 1_000,
            cache_hits         = self.window_cache_hits,
            cache_misses       = self.window_cache_misses,
            cache_hit_rate     = format_args!("{:.4}", cache_hit_rate),
            backend_errors     = self.window_backend_errors,
            pool_members       = pool_members,
            "periodic stats"
        );

        // Reset window (totals are cumulative and never reset)
        self.window_requests = 0;
        self.window_cache_hits = 0;
        self.window_cache_misses = 0;
        self.window_backend_errors = 0;
        self.latency_hist = [0; N_BUCKETS];
        self.window_start = now;
        self.last_emit = now;
    }
}

/// Compute p50 and p99 from the histogram. Returns (p50_ns, p99_ns) as
/// the upper bound of the bucket containing the percentile.
/// O(N_BUCKETS) = O(9) — negligible.
fn percentiles(hist: &[u64; N_BUCKETS]) -> (u64, u64) {
    const UPPER_NS: [u64; N_BUCKETS] = [100, 500, 1_000, 5_000, 10_000, 50_000, 100_000, 1_000_000, 10_000_000];
    let total: u64 = hist.iter().sum();
    if total == 0 {
        return (0, 0);
    }
    let (mut p50, mut p99) = (0u64, 0u64);
    let mut cumulative = 0u64;
    for (i, &count) in hist.iter().enumerate() {
        cumulative += count;
        if p50 == 0 && cumulative * 100 >= total * 50 {
            p50 = UPPER_NS[i];
        }
        if cumulative * 100 >= total * 99 {
            p99 = UPPER_NS[i];
            break;
        }
    }
    (p50, p99)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_record_and_bucket() {
        let mut s = Stats::new();
        s.record(50, true, false);    // <100ns bucket
        s.record(200, false, false);  // <500ns bucket
        s.record(800, false, false);  // <1000ns bucket
        assert_eq!(s.requests_total, 3);
        assert_eq!(s.cache_hits_total, 1);
        assert_eq!(s.cache_misses_total, 2);
        assert_eq!(s.latency_hist[0], 1); // <100ns
        assert_eq!(s.latency_hist[1], 1); // <500ns
        assert_eq!(s.latency_hist[2], 1); // <1µs
    }

    #[test]
    fn test_percentiles_uniform() {
        let mut hist = [0u64; N_BUCKETS];
        // 100 requests all in <100ns bucket
        hist[0] = 100;
        let (p50, p99) = percentiles(&hist);
        assert_eq!(p50, 100);
        assert_eq!(p99, 100);
    }

    #[test]
    fn test_percentiles_spread() {
        let mut hist = [0u64; N_BUCKETS];
        hist[0] = 50;  // 50 in <100ns
        hist[1] = 40;  // 40 in <500ns
        hist[8] = 10;  // 10 in ≥1ms
        // p50 = first bucket where cumulative >= 50% (bucket 0: 50/100 = 50%)
        let (p50, p99) = percentiles(&hist);
        assert_eq!(p50, 100);    // ≤100ns
        assert_eq!(p99, 10_000_000); // ≥1ms bucket
    }

    #[test]
    fn test_record_overhead() {
        // Verify record() is fast: 1000 calls should complete in well under 1ms
        let mut s = Stats::new();
        let start = Instant::now();
        for i in 0..1000u64 {
            s.record(i % 1000, i % 2 == 0, false);
        }
        let elapsed = start.elapsed();
        // Allow 10× more headroom in debug builds; the threshold is still strict enough
        // to catch accidentally blocking code on either build profile.
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
