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
/// ## The window is measured from the most recent failure
///
/// It used to be measured from the first failure, so a series that took longer
/// than the window forgot its earliest attempts and never blocked. A login that
/// verifies a password in an unoptimised build takes about a second, so five
/// wrong passwords against the example's five-second window can span more than
/// five seconds on a loaded host. The sixth was then answered normally and the
/// suite reported the throttle as missing. Measuring from the most recent
/// failure also stops an attacker spacing attempts to stay under the count.
///
/// A block lifts after `window_secs` with no further failure. A correct password
/// still clears the count at once.
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
        self.is_blocked_at(ip, m6_core::platform::now())
    }

    /// [`is_blocked`](Self::is_blocked) at a caller-supplied time, so a test can
    /// walk a series of failures without sleeping.
    fn is_blocked_at(&self, ip: &str, now: Instant) -> bool {
        match self.map.get(ip) {
            Some((count, last)) => {
                now.saturating_duration_since(*last).as_secs() < self.window_secs
                    && *count >= self.max_attempts
            }
            None => false,
        }
    }

    /// Whole seconds until `ip`'s window ends, rounded up.
    ///
    /// The block lifts once `window_secs` has passed since the most recent
    /// failure, so the true wait is `window_secs` minus the elapsed fraction.
    /// The fraction is truncated before it is subtracted, which makes this the
    /// ceiling of that wait: never below the true wait and at most one second
    /// above it. `Retry-After` must not send a client back early, so the
    /// rounding goes up.
    ///
    /// Zero means the window has already elapsed. An address with no recorded
    /// failure also answers zero, because it has no wait to report.
    pub fn retry_after_secs(&self, ip: &str) -> u64 {
        self.retry_after_secs_at(ip, m6_core::platform::now())
    }

    /// [`retry_after_secs`](Self::retry_after_secs) at a caller-supplied time,
    /// so a test can check the rounding without sleeping.
    fn retry_after_secs_at(&self, ip: &str, now: Instant) -> u64 {
        match self.map.get(ip) {
            Some((_, last)) => self
                .window_secs
                .saturating_sub(now.saturating_duration_since(*last).as_secs()),
            None => 0,
        }
    }

    /// Count one failed login against `ip`.
    pub fn record_failure(&mut self, ip: &str) {
        self.record_failure_at(ip, m6_core::platform::now());
    }

    /// [`record_failure`](Self::record_failure) at a caller-supplied time, so a
    /// test can space a series of failures without sleeping.
    fn record_failure_at(&mut self, ip: &str, now: Instant) {
        let entry = self.map.entry(ip.to_string()).or_insert((0, now));
        if now.saturating_duration_since(entry.1).as_secs() >= self.window_secs {
            entry.0 = 0;
        }
        entry.0 += 1;
        entry.1 = now;
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
    use std::time::Duration;

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

    /// A series of failures each inside the window but spanning more than one
    /// window still uses up the budget. The old code measured the window from
    /// the first failure, so the fifth reset the count to one and the sixth was
    /// answered normally. That is what a loaded host showed: five wrong
    /// passwords, each about a second apart, did not throttle the sixth.
    #[test]
    fn failures_spread_across_more_than_one_window_exhaust_the_budget() {
        let mut rl = RateLimiter::with_limits(5, 5);
        let start = m6_core::platform::now();
        for i in 0..5 {
            let at = start + Duration::from_millis(1400 * i);
            rl.record_failure_at("192.0.2.1", at);
        }
        let sixth = start + Duration::from_millis(5600);
        assert!(
            rl.is_blocked_at("192.0.2.1", sixth),
            "five failures 1.4s apart must block the sixth against a 5s window"
        );
        assert!(
            !rl.is_blocked_at("192.0.2.1", sixth + Duration::from_secs(5)),
            "the block must lift after a window with no further failure"
        );
    }

    /// A client that has used up its failure budget is blocked, so the login
    /// path answers 429. After the window elapses the rollover in `is_blocked`
    /// must unblock it again. Removing that rollover keeps the client blocked
    /// forever, which is the wrong outcome this test catches.
    #[test]
    fn a_blocked_client_is_unblocked_after_the_window_rolls_over() {
        let mut rl = RateLimiter::with_limits(1, 60);
        rl.record_failure("192.0.2.1");
        assert!(rl.is_blocked("192.0.2.1"), "the budget must block first");

        // Move the recorded most recent failure into the past instead of
        // sleeping. The test module sees the private field, and a fixed sleep
        // only works when the machine is idle.
        let (_, last) = rl
            .map
            .get_mut("192.0.2.1")
            .expect("the failure is recorded");
        *last = m6_core::platform::now() - Duration::from_secs(61);

        assert!(
            !rl.is_blocked("192.0.2.1"),
            "the client stayed blocked after the window rolled over"
        );
    }

    /// The header value is the remaining window, rounded up. A failure just
    /// recorded leaves the whole window. Any elapsed fraction costs one whole
    /// second, because a `Retry-After` below the true wait sends the client
    /// back early.
    #[test]
    fn retry_after_is_the_remaining_window_rounded_up() {
        let mut rl = RateLimiter::with_limits(1, 60);
        let t0 = m6_core::platform::now();
        rl.record_failure_at("192.0.2.1", t0);

        assert_eq!(rl.retry_after_secs_at("192.0.2.1", t0), 60);
        assert_eq!(
            rl.retry_after_secs_at("192.0.2.1", t0 + Duration::from_millis(1)),
            60,
            "a fraction of a second still leaves the whole 60-second wait"
        );
        assert_eq!(
            rl.retry_after_secs_at("192.0.2.1", t0 + Duration::from_millis(59_500)),
            1,
            "the last half second is reported as a whole one"
        );
        assert_eq!(
            rl.retry_after_secs_at("192.0.2.1", t0 + Duration::from_secs(60)),
            0,
            "the window has elapsed"
        );
        assert_eq!(
            rl.retry_after_secs_at("192.0.2.2", t0),
            0,
            "an address with no failure has no wait"
        );
    }

    /// Two configured windows answer in proportion, and every answer is at or
    /// above the true wait and less than one second above it. Rounding down
    /// would break the first bound, and a constant cannot satisfy both windows.
    #[test]
    fn retry_after_is_never_below_the_true_wait() {
        for window_secs in [120u64, 900] {
            let mut rl = RateLimiter::with_limits(1, window_secs);
            let t0 = m6_core::platform::now();
            rl.record_failure_at("192.0.2.1", t0);
            assert_eq!(rl.retry_after_secs_at("192.0.2.1", t0), window_secs);

            for offset in [
                Duration::ZERO,
                Duration::from_millis(1),
                Duration::from_millis(999),
                Duration::from_secs(1),
                Duration::from_secs(window_secs / 2),
                Duration::from_secs(window_secs - 1),
            ] {
                let answered =
                    Duration::from_secs(rl.retry_after_secs_at("192.0.2.1", t0 + offset));
                let true_wait = Duration::from_secs(window_secs) - offset;
                assert!(
                    answered >= true_wait,
                    "window {window_secs}s, {offset:?} elapsed: answered {answered:?}, true wait {true_wait:?}"
                );
                assert!(
                    answered < true_wait + Duration::from_secs(1),
                    "window {window_secs}s, {offset:?} elapsed: answered {answered:?} is more than a second above the true wait {true_wait:?}"
                );
            }
        }
    }
}
