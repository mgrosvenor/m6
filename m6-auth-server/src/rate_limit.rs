use std::collections::HashMap;
use std::time::Instant;

/// Defaults: 5 failed logins per 15 minutes per IP.
pub const DEFAULT_MAX_ATTEMPTS: u32 = 5;
pub const DEFAULT_WINDOW_SECS: u64 = 15 * 60;

/// In-memory login throttle, keyed by the client IP the proxy forwarded.
///
/// ## Only failures count
///
/// This used to increment on **every** login request, before the credentials
/// were even looked at, and the counter was never cleared. Six logins in fifteen
/// minutes locked the account's IP out with a 429, whether or not any of them
/// were wrong.
///
/// That costs security nothing and costs correctness plenty. An attacker who
/// already has the password does not need six attempts, so counting successes
/// stops no attack; what it does stop is a person logging in from a phone, a
/// laptop and a second browser, and any test suite that logs in more than five
/// times in a quarter of an hour.
///
/// It made the CMS example's end-to-end test unreliable by construction. That
/// test logs in twice per run, so the third run inside the window returned 429
/// from `/auth/login`, every authenticated check after it failed, and the
/// failures pointed at authentication rather than at the throttle. Restarting
/// the service was the only reset, because the map is in memory. A test that
/// fails for a reason it does not report is worse than no test.
///
/// So: `is_blocked` only reads, `record_failure` is called when the credentials
/// were actually wrong, and `clear` wipes the IP's count on a successful login.
///
/// ## Still in memory, deliberately
///
/// A restart forgets every count. That is a real limit and it is the right
/// trade here: the alternative is a write to the auth database on every failed
/// password, which is a cheap denial-of-service against the disk. Brute force
/// at a rate slow enough to survive restarts is what `fail2ban` and the
/// firewall are for.
pub struct RateLimiter {
    map: HashMap<String, (u32, Instant)>,
    max_attempts: u32,
    window_secs: u64,
}

impl RateLimiter {
    pub fn new() -> Self {
        Self::with_limits(DEFAULT_MAX_ATTEMPTS, DEFAULT_WINDOW_SECS)
    }

    pub fn with_limits(max_attempts: u32, window_secs: u64) -> Self {
        RateLimiter {
            map: HashMap::new(),
            max_attempts,
            window_secs,
        }
    }

    /// True when `ip` has already used up its failure budget for the window.
    ///
    /// Read-only on purpose: called before the password is checked, so a
    /// correct password is never counted against the budget.
    pub fn is_blocked(&self, ip: &str) -> bool {
        match self.map.get(ip) {
            Some((count, started)) => {
                if started.elapsed().as_secs() >= self.window_secs {
                    false // window has rolled over
                } else {
                    *count >= self.max_attempts
                }
            }
            None => false,
        }
    }

    /// Count one failed login against `ip`.
    pub fn record_failure(&mut self, ip: &str) {
        let now = Instant::now();
        let window = self.window_secs;
        let entry = self.map.entry(ip.to_string()).or_insert((0, now));
        if entry.1.elapsed().as_secs() >= window {
            *entry = (0, now);
        }
        entry.0 += 1;
    }

    /// Forget `ip`'s failures. Called on a successful login.
    pub fn clear(&mut self, ip: &str) {
        self.map.remove(ip);
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
    fn a_fresh_ip_is_not_blocked() {
        let rl = RateLimiter::new();
        assert!(!rl.is_blocked("192.0.2.1"));
    }

    #[test]
    fn blocking_starts_only_after_the_budget_is_used_up() {
        let mut rl = RateLimiter::with_limits(3, 900);
        for _ in 0..3 {
            assert!(!rl.is_blocked("192.0.2.1"));
            rl.record_failure("192.0.2.1");
        }
        assert!(rl.is_blocked("192.0.2.1"));
    }

    /// The defect this file was rewritten for. Checking must not increment, or a
    /// caller that only ever succeeds still locks itself out.
    #[test]
    fn checking_does_not_consume_the_budget() {
        let rl = RateLimiter::with_limits(1, 900);
        for _ in 0..100 {
            assert!(!rl.is_blocked("192.0.2.1"));
        }
    }

    /// A successful login clears the count, so five wrong passwords followed by
    /// the right one leaves the next attempt unthrottled.
    #[test]
    fn success_clears_the_count() {
        let mut rl = RateLimiter::with_limits(5, 900);
        for _ in 0..5 {
            rl.record_failure("192.0.2.1");
        }
        assert!(rl.is_blocked("192.0.2.1"));
        rl.clear("192.0.2.1");
        assert!(!rl.is_blocked("192.0.2.1"));
    }

    #[test]
    fn addresses_are_counted_separately() {
        let mut rl = RateLimiter::with_limits(1, 900);
        rl.record_failure("192.0.2.1");
        assert!(rl.is_blocked("192.0.2.1"));
        assert!(!rl.is_blocked("192.0.2.2"));
    }

    /// A zero-second window means the count has always already expired, which
    /// is how a deployment turns the throttle off without a second config flag.
    #[test]
    fn a_zero_second_window_never_blocks() {
        let mut rl = RateLimiter::with_limits(1, 0);
        rl.record_failure("192.0.2.1");
        assert!(!rl.is_blocked("192.0.2.1"));
    }
}
