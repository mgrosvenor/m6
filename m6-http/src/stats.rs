/// Performance statistics for the critical path.
use std::time::Instant;

const EMIT_INTERVAL_SECS: u64 = 10;

/// Number of latency samples kept per window per category (hit / miss).
/// Ring-buffer: oldest sample is overwritten when full.
/// 4096 × 8 bytes = 32 KB per reservoir.
const RESERVOIR: usize = 4096;

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
            rps_peak: 0, window_start: now, last_emit: now,
        }
    }

    #[inline(always)]
    pub fn record(&mut self, elapsed_ns: u64, cache_hit: bool, backend_error: bool) {
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
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
        }
    }
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
        s.record(50, true, false);
        s.record(200, false, false);
        s.record(800, false, false);
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
        for i in 1u64..=100 { s.record(i, true, false); }
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
        for i in 1..=1000u64 { s.record(i, i % 2 == 0, false); }
        let elapsed = start.elapsed();
        #[cfg(debug_assertions)]
        let threshold_us = 1_000;
        #[cfg(not(debug_assertions))]
        let threshold_us = 100;
        assert!(elapsed.as_micros() < threshold_us,
            "record() too slow: {}µs for 1000 calls", elapsed.as_micros());
    }
}
