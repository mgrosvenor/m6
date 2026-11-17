//! The limits the edge's buffers and queues follow.
//!
//! One set per process, made from the `[limits]` table of the system config
//! (or the one file) when the process starts and read where a buffer is made.
//! A process that never sets it, such as a test, gets the built-in defaults.

use std::sync::OnceLock;

use m6_core::limits::{
    Limit, Limits, CACHE_BYTES, EDGE_REQUEST, EDGE_RESPONSE, H2_HEADER_BLOCK, H2_RECV, H2_SEND,
    H2_STREAM_BODY, HEAP_RESERVE, RATE_LIMIT_ADDRESSES, REFRESH_QUEUE,
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

/// Room beyond the cache for connections, buffers and the rest of the heap.
const HEAP_HEADROOM: usize = 64 * 1024 * 1024;
/// The huge page size the reservation is rounded to.
const HUGE_PAGE: usize = 2 * 1024 * 1024;

/// What m6-http reserves for its heap, and a warning to print when the
/// operator's `[limits.heap_reserve]` leaves the cache outside it.
///
/// The cache is the hot data, so its memory is carved from the huge-page
/// reservation: unless `start` is set, the reservation is `cache_bytes` max plus
/// 64 MiB, rounded up to the huge page size.
pub fn heap_reserve(limits: &Limits) -> (usize, Option<String>) {
    let reserve = limits.get(&HEAP_RESERVE);
    let cache = limits.get(&CACHE_BYTES).max();
    if !reserve.start_set() {
        return ((cache + HEAP_HEADROOM).next_multiple_of(HUGE_PAGE), None);
    }
    let warning = (reserve.start() < cache).then(|| {
        format!(
            "[limits.heap_reserve] start = {} is below [limits.cache_bytes] max = {}, so the cache cannot fit in the reservation: raise heap_reserve start to at least {} in {}",
            reserve.text(reserve.start()),
            limits.get(&CACHE_BYTES).text(cache),
            reserve.text((cache + HEAP_HEADROOM).next_multiple_of(HUGE_PAGE)),
            limits.file().display()
        )
    });
    (reserve.start(), warning)
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
        assert!(warning.contains("[limits.cache_bytes] max"), "{warning}");
        let (_, warning) = heap_reserve(&limits("[limits.heap_reserve]\nstart = \"256MiB\"\n"));
        assert!(warning.is_none());
    }
}
