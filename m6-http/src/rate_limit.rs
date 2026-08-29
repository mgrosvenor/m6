/// Per-IP request throttling at the edge — general traffic, not to be
/// confused with m6-auth-server's rate_limit.rs (login-attempt throttling
/// only, a completely different concern reached only via `/login`).
///
/// Fixed-window counter, same shape as the auth-server's limiter. Runs ahead
/// of cache lookup and backend work for every request, on every protocol.
use std::collections::HashMap;
use std::time::{Duration, Instant};

const WINDOW_SECS: u64 = 60;
/// Opportunistic cleanup threshold — bounds worst-case memory if an attacker
/// rotates source IPs to dodge the per-IP window; swept lazily on the next
/// check rather than on a separate timer, so idle periods cost nothing.
const MAX_TRACKED_IPS: usize = 50_000;

pub struct RateLimiter {
    map: HashMap<String, (u32, Instant)>,
}

impl RateLimiter {
    pub fn new() -> Self {
        RateLimiter { map: HashMap::new() }
    }

    /// Record a request from `ip` and report whether it's over `limit_per_min`.
    /// Keeps counting past the limit (doesn't stop at the threshold) so the
    /// dashboard can show how far over a given IP actually is.
    pub fn check_and_increment(&mut self, ip: &str, limit_per_min: u32) -> bool {
        let now = Instant::now();
        let window = Duration::from_secs(WINDOW_SECS);

        if self.map.len() > MAX_TRACKED_IPS {
            self.map.retain(|_, (_, seen)| now.duration_since(*seen) < window);
        }

        let entry = self.map.entry(ip.to_string()).or_insert((0, now));
        if now.duration_since(entry.1) >= window {
            *entry = (0, now);
        }
        entry.0 += 1;
        entry.0 > limit_per_min
    }
}

impl Default for RateLimiter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_under_limit_not_blocked() {
        let mut rl = RateLimiter::new();
        for _ in 0..5 {
            assert!(!rl.check_and_increment("1.2.3.4", 10));
        }
    }

    #[test]
    fn test_over_limit_blocked() {
        let mut rl = RateLimiter::new();
        for _ in 0..10 {
            rl.check_and_increment("1.2.3.4", 10);
        }
        assert!(rl.check_and_increment("1.2.3.4", 10));
    }

    #[test]
    fn test_different_ips_independent() {
        let mut rl = RateLimiter::new();
        for _ in 0..10 {
            rl.check_and_increment("1.2.3.4", 10);
        }
        assert!(!rl.check_and_increment("5.6.7.8", 10));
    }

    #[test]
    fn test_keeps_counting_past_limit() {
        let mut rl = RateLimiter::new();
        for _ in 0..15 {
            rl.check_and_increment("1.2.3.4", 10);
        }
        // 16th request — still reports over, counter kept climbing past 10.
        assert!(rl.check_and_increment("1.2.3.4", 10));
    }
}
