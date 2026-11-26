//! The limits the edge's buffers and queues follow.
//!
//! One set per process, made from the `[limits]` table of the system config
//! (or the one file) when the process starts and read where a buffer is made.
//! A process that never sets it, such as a test, gets the built-in defaults.

use std::sync::OnceLock;

use m6_core::limits::{
    Limit, Limits, CACHE_BYTES, EDGE_REQUEST, EDGE_RESPONSE, H2_HEADER_BLOCK, H2_RECV, H2_SEND,
    H2_STREAM_BODY, RATE_LIMIT_ADDRESSES, REFRESH_QUEUE,
};

/// The edge's limits, each a [`Limit`] copied out of the config table.
#[derive(Debug, Clone)]
pub struct EdgeLimits {
    /// One HTTP/1.1 connection's read buffer: the largest request.
    pub request: Limit,
    /// One HTTP/1.1 connection's write buffer: the largest response held.
    pub response: Limit,
    /// One HTTP/2 connection's read buffer.
    pub h2_recv: Limit,
    /// One HTTP/2 header block before decoding.
    pub h2_header_block: Limit,
    /// The body of one HTTP/2 request stream.
    pub h2_stream_body: Limit,
    /// One HTTP/2 connection's unsent response bytes.
    pub h2_send: Limit,
    /// Stale cache entries waiting to be fetched again.
    pub refresh_queue: Limit,
    /// Client addresses the request counter tracks.
    pub rate_limit_addresses: Limit,
    /// The response cache's memory.
    pub cache_bytes: Limit,
}

impl EdgeLimits {
    pub fn from_limits(limits: &Limits) -> Self {
        Self {
            request: limits.get(&EDGE_REQUEST).clone(),
            response: limits.get(&EDGE_RESPONSE).clone(),
            h2_recv: limits.get(&H2_RECV).clone(),
            h2_header_block: limits.get(&H2_HEADER_BLOCK).clone(),
            h2_stream_body: limits.get(&H2_STREAM_BODY).clone(),
            h2_send: limits.get(&H2_SEND).clone(),
            refresh_queue: limits.get(&REFRESH_QUEUE).clone(),
            rate_limit_addresses: limits.get(&RATE_LIMIT_ADDRESSES).clone(),
            cache_bytes: limits.get(&CACHE_BYTES).clone(),
        }
    }
}

static EDGE: OnceLock<EdgeLimits> = OnceLock::new();

/// Set the process's limits. Start only, once: a second call changes nothing.
pub fn init(limits: &Limits) {
    let _ = EDGE.set(EdgeLimits::from_limits(limits));
}

/// The process's limits, or the built-in defaults when none were set.
pub fn edge() -> &'static EdgeLimits {
    EDGE.get_or_init(|| EdgeLimits::from_limits(&Limits::default()))
}

/// Log the one line a `Vec` that grew from `before` to `after` bytes owes, when
/// it did. A `Vec` doubles itself, so the line is written where the growth
/// was seen.
pub fn log_growth(limit: &Limit, before: usize, after: usize, what: &str) {
    if after > before {
        limit.log_growth(before, after, what);
    }
}

/// What m6-http reserves for its heap, and a warning to print when the
/// operator's `[limits.heap_reserve]` leaves the cache outside it.
///
/// The rule is [`m6_core::limits::heap_reserve`], shared with m6-file so the
/// two services cannot drift: unless `start` is set, the reservation is
/// `cache_bytes` start plus 64 MiB, rounded up to the huge page size, and the
/// cache holds the start the record prints.
pub fn heap_reserve(limits: &Limits) -> (usize, Option<String>) {
    m6_core::limits::heap_reserve(limits)
}

/// The byte bound the response cache is built with: the configured `start`,
/// not the `max` ceiling.
///
/// A default config bounds the cache at 128 MiB, and a config may raise it up
/// to the 4 GiB ceiling. Building the cache with `max` instead would make a
/// default node grow to 4 GiB before its first eviction.
pub fn cache_bound(limits: &Limits) -> usize {
    limits.get(&CACHE_BYTES).start()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits(toml_text: &str) -> Limits {
        let value: toml::Value = toml_text.parse().unwrap();
        Limits::from_toml(
            value.get("limits"),
            std::path::Path::new("system.toml"),
            4096,
        )
        .unwrap()
    }

    #[test]
    fn the_default_reserve_holds_the_cache_and_headroom() {
        let (bytes, warning) = heap_reserve(&Limits::default());
        assert_eq!(bytes, 192 * 1024 * 1024);
        assert!(warning.is_none());
        let (bytes, _) = heap_reserve(&limits(
            "[limits.cache_bytes]\nstart = \"1MiB\"\nmax = \"1MiB\"\n",
        ));
        assert_eq!(bytes, 66 * 1024 * 1024);
    }

    #[test]
    fn a_reserve_below_the_cache_warns_naming_the_key() {
        let (bytes, warning) = heap_reserve(&limits("[limits.heap_reserve]\nstart = \"64MiB\"\n"));
        assert_eq!(bytes, 64 * 1024 * 1024);
        let warning = warning.expect("the cache does not fit in 64 MiB");
        assert!(warning.contains("[limits.heap_reserve] start"), "{warning}");
        assert!(warning.contains("[limits.cache_bytes] start"), "{warning}");
        let (_, warning) = heap_reserve(&limits("[limits.heap_reserve]\nstart = \"256MiB\"\n"));
        assert!(warning.is_none());
    }

    /// The response cache is bounded by the configured `start`, not the
    /// 4 GiB ceiling. A default config must bound the cache at 128 MiB, and a
    /// raised `start` must be honoured, or a default node would run out of
    /// memory before its first eviction.
    #[test]
    fn the_cache_bound_is_the_configured_start_not_the_ceiling() {
        assert_eq!(cache_bound(&Limits::default()), 128 * 1024 * 1024);
        assert_eq!(
            Limits::default().get(&CACHE_BYTES).max(),
            4 * 1024 * 1024 * 1024,
            "the ceiling is 4 GiB, so a default must not build the cache from it"
        );
        assert_eq!(
            cache_bound(&limits("[limits.cache_bytes]\nstart = \"512MiB\"\n")),
            512 * 1024 * 1024
        );
    }
}
