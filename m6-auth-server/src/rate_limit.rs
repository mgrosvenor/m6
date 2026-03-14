use std::collections::HashMap;
use std::time::Instant;

const MAX_ATTEMPTS: u32 = 5;
const WINDOW_SECS: u64  = 15 * 60; // 15 minutes

/// Simple in-memory rate limiter: 5 login attempts per 15 minutes per IP.
pub struct RateLimiter {
    map: HashMap<String, (u32, Instant)>,
}

impl RateLimiter {
    pub fn new() -> Self {
        RateLimiter { map: HashMap::new() }
    }

    /// Check whether `ip` is currently rate-limited.
    /// Returns true when the request should be blocked.
    pub fn check_and_increment(&mut self, ip: &str) -> bool {
        let now = Instant::now();
        let window = std::time::Duration::from_secs(WINDOW_SECS);

        let entry = self.map.entry(ip.to_string()).or_insert((0, now));

        // Reset window if expired
        if now.duration_since(entry.1) >= window {
            *entry = (0, now);
        }

        entry.0 += 1;
        entry.0 > MAX_ATTEMPTS
    }
}
