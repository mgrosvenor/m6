/// Per-IP request throttling at the edge — general traffic, not to be
/// confused with m6-auth-server's rate_limit.rs (login-attempt throttling
/// only, a completely different concern reached only via `/login`).
///
/// Fixed-window counter, same shape as the auth-server's limiter. Runs ahead
/// of cache lookup and backend work for every request, on every protocol.
use std::collections::HashMap;
use std::time::{Duration, Instant};

const WINDOW_SECS: u64 = 60;
pub struct RateLimiter {
    map: HashMap<String, (u32, Instant)>,
    /// The `rate_limit_addresses` limit: how many client addresses are tracked.
    /// Bounds worst-case memory if an attacker rotates source addresses to dodge
    /// the per-address window.
    limit: m6_core::limits::Limit,
    /// Requests refused for want of room, for the sampled error line.
    refused: u64,
}

impl RateLimiter {
    pub fn new() -> Self {
        Self::with_limit(crate::limits::edge().rate_limit_addresses.clone())
    }

    /// A limiter that tracks as many addresses as `limit` allows. The table
    /// starts at the limit's `start` and grows by doubling.
    pub fn with_limit(limit: m6_core::limits::Limit) -> Self {
        RateLimiter {
            map: HashMap::with_capacity(limit.start()),
            limit,
            refused: 0,
        }
    }

    /// Record a request from `ip` and report whether it's over `limit_per_min`.
    /// Keeps counting past the limit (doesn't stop at the threshold) so the
    /// dashboard can show how far over a given IP actually is.
    pub fn check_and_increment(&mut self, ip: &str, limit_per_min: u32) -> bool {
        let now = m6_core::platform::now();
        let window = Duration::from_secs(WINDOW_SECS);

        // At the maximum, expired addresses are swept first. If the table is
        // still full, a new address is refused as over the limit, with an
        // error that names the key: counting it would grow the table past its
        // maximum, and serving it uncounted would let rotating addresses past
        // the limiter.
        if self.map.len() >= self.limit.max() && !self.map.contains_key(ip) {
            self.map
                .retain(|_, (_, seen)| now.duration_since(*seen) < window);
            if self.map.len() >= self.limit.max() {
                self.refused += 1;
                if self.refused == 1 || self.refused.is_multiple_of(1024) {
                    m6_core::log::error!(
                        "{} ({} refused so far)",
                        self.limit.exceeded(self.map.len() + 1, "a client address"),
                        self.refused
                    );
                }
                return true;
            }
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

    /// A new address past the `rate_limit_addresses` maximum is refused as over
    /// the limit, not tracked and not let through, and a tracked address still
    /// counts.
    #[test]
    fn a_new_address_past_the_maximum_is_refused() {
        use m6_core::limits::{Limit, RATE_LIMIT_ADDRESSES};
        let mut rl = RateLimiter::with_limit(Limit::custom(&RATE_LIMIT_ADDRESSES, 2, 2));
        assert!(!rl.check_and_increment("10.0.0.1", 300));
        assert!(!rl.check_and_increment("10.0.0.2", 300));
        assert!(
            rl.check_and_increment("10.0.0.3", 300),
            "a third address is refused"
        );
        assert!(
            !rl.check_and_increment("10.0.0.1", 300),
            "a tracked address still counts"
        );
        assert_eq!(rl.map.len(), 2, "the table never passed its maximum");
    }
}
