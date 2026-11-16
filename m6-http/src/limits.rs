//! The limits the edge's buffers and queues follow.
//!
//! One set per process, made from the `[limits]` table of the system config
//! (or the one file) when the process starts and read where a buffer is made.
//! A process that never sets it, such as a test, gets the built-in defaults.

use std::sync::OnceLock;

use m6_core::limits::{
    Limit, Limits, CACHE_BYTES, EDGE_REQUEST, EDGE_RESPONSE, H2_HEADER_BLOCK, H2_RECV,
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
pub fn log_growth(limit: &Limit, before: usize, after: usize) {
    if after > before {
        limit.log_growth(after);
    }
}
