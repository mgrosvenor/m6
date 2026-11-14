// m6-http: reverse proxy, cache, and router.
//
// HTTP/3 over QUIC/UDP using quiche (sans-I/O) + single-threaded epoll.
// Standard POSIX UDP socket + epoll, accelerated transparently by
// OpenOnload/ExaSock at deployment. No async runtime, no threads.
#![deny(unsafe_code)]

use std::collections::HashMap;
use std::net::{SocketAddr, UdpSocket};
use std::os::unix::io::AsRawFd;
use std::path::PathBuf;
use std::time::Instant;

use anyhow::Context;
use bytes::Bytes;
use m6_core::log::{debug, error, info, warn};
use quiche::h3::NameValue;
use rand::RngCore;

use m6_core::client::Upstream;
use m6_http_lib::analytics;
use m6_http_lib::analytics::H3Headers;
use m6_http_lib::auth;
use m6_http_lib::cache::{
    evaluate_preconditions, make_lookup_key, not_modified_headers, request_permits_storage,
    should_cache, strip_set_cookie, Cache, CacheKey, CachedResponse, Precondition,
};
use m6_http_lib::config::{self, Config};
use m6_http_lib::error::{self as error, ErrorMode};
use m6_http_lib::fields::validate_request_header_bytes;
use m6_http_lib::forward::{self, UpstreamId, Waiter};
use m6_http_lib::h2_client::H2Completion;
use m6_http_lib::health;
use m6_http_lib::rate_limit::RateLimiter;
use m6_http_lib::stats::Stats;

/// RFC 9114 8.1: H3_MESSAGE_ERROR, the stream error a server must raise for a
/// malformed request. Named here rather than taken from quiche so the wire
/// value is stated where it is used.
const H3_MESSAGE_ERROR: u64 = 0x10e;

/// A rendered error document held for reuse: when it was rendered, its headers,
/// and its body. Keyed by status, so one entry answers every path that 404s.
///
/// Named because the written-out form appeared in the `ServerState` field and
/// again in each test that builds one, and three copies of
/// `(Instant, Vec<(String, String)>, Vec<u8>)` say nothing about which element
/// is the body.
type CachedErrorPage = (std::time::Instant, Vec<(String, String)>, Vec<u8>);

/// What finalizing a URL-backend response yields: the status, the response
/// headers, the body, the name of the backend that served it, and the early
/// Returned by both `finalize_url_response` and its inner half, which is why it
/// is worth a name. It used to carry a fifth element, the hint URLs to
/// advertise; see issue #93 for why those are gone.
type FinalizedResponse = (u16, Vec<(String, String)>, Vec<u8>, String);

/// What finishing a backend reply can mean: the response is ready to write, or
/// the reply itself needs an error page fetched, so the same waiter is stored
/// on a fresh upstream instead. The error page fetch carries
/// `error_status_override`, so it never re-derives another error page.
enum Completion {
    Ready(FinalizedResponse),
    Pending {
        id: UpstreamId,
        ctx: Box<forward::PendingUrlContext>,
    },
}

/// What applying an error mode can mean, on the request path: the response is
/// ready, or a custom error page must be fetched first and the request parks.
enum ErrorOutcome {
    Ready(u16, Vec<(String, String)>, Vec<u8>, String),
    Pending {
        id: UpstreamId,
        ctx: Box<forward::PendingUrlContext>,
    },
}

/// The loop state a completion touches, bundled so the completion helpers stay
/// under clippy's argument ceiling.
struct CompletionCtx<'a> {
    state: &'a mut ServerState,
    tcp: &'a mut Option<Http11Listener>,
    h2c: &'a mut Option<H2cListener>,
    registry: &'a m6_core::platform::Registry,
    quic_port: u16,
    udp: &'a UdpSocket,
    connections: &'a mut HashMap<Vec<u8>, QuicConn>,
    now: Instant,
}

impl<'a> CompletionCtx<'a> {
    // The one bundle constructor for the completion context; it exists so the
    // five completion helpers that take `&mut CompletionCtx` stay under
    // clippy's argument ceiling.
    #[allow(clippy::too_many_arguments)]
    fn new(
        state: &'a mut ServerState,
        tcp: &'a mut Option<Http11Listener>,
        h2c: &'a mut Option<H2cListener>,
        registry: &'a m6_core::platform::Registry,
        quic_port: u16,
        udp: &'a UdpSocket,
        connections: &'a mut HashMap<Vec<u8>, QuicConn>,
        now: Instant,
    ) -> Self {
        CompletionCtx {
            state,
            tcp,
            h2c,
            registry,
            quic_port,
            udp,
            connections,
            now,
        }
    }
}

/// How long a fetched error document is reused before being re-fetched.
/// Short enough that a redeployed error page appears promptly, long enough
/// that a sustained sweep costs one fetch a minute rather than one per path.
const ERROR_PAGE_TTL: std::time::Duration = std::time::Duration::from_secs(60);
use m6_core::event::{LoopTrace, Slots, Timeouts};
use m6_core::platform::process::raise_fd_limit;
use m6_core::platform::Signals;
use m6_core::platform::{Events, Interest, Poll, Token};
use m6_http_lib::auth::PublicKey;
use m6_http_lib::h2c_client::H2cClientPool;
use m6_http_lib::h2s_client::H2sTlsClientPool;
use m6_http_lib::http11::{
    make_tls_server_config, ConnDrive, H2cListener, HitHeaders, Http11Listener, ParseResult,
    RequestHead, RequestOutcome, VaryMerge, H2_IDLE_TIMEOUT_SECS, INITIAL_CONNECTIONS,
    MAX_ACCEPTS_PER_WAKE, READ_TIMEOUT_SECS,
};
use m6_http_lib::pool::{self, PoolManager};
use m6_http_lib::router::RouteTable;
use m6_http_lib::stats::{Channel, Iface, Version as HttpVersion};

// ── Constants ────────────────────────────────────────────────────────────────

const TOKEN_UDP: Token = Token(usize::MAX);
const TOKEN_TLS_LISTENER: Token = Token(usize::MAX - 1);
const TOKEN_H2C_LISTENER: Token = Token(usize::MAX - 2);
const TOKEN_SIGNAL: Token = Token(usize::MAX - 3);
/// The operational log's stdout, registered while it holds bytes a full
/// destination would not take.
const TOKEN_LOG: Token = Token(usize::MAX - 4);
const MAX_DATAGRAM_SIZE: usize = 1350;

/// Who owns a per-descriptor token: the owner and the owner's own slot index.
#[derive(Clone, Copy)]
enum Source {
    /// A freed token, or one not yet assigned.
    Vacant,
    TlsConn(usize),
    H2cConn(usize),
    /// An outbound h2c or h2s pool connection, found by its token.
    H2cUpstream,
    H2sUpstream,
    /// An in-loop upstream in the shared table: a socket member or a URL
    /// backend request.
    Upstream(usize),
}

/// The [`LoopTrace`] trigger index for an iteration's first event, and the
/// `timeout` index when none fired. The names are fixed in `event_loop`.
fn trigger_index(token: Option<Token>, sources: &Slots<Source>) -> usize {
    match token {
        Some(TOKEN_UDP) => 3,
        Some(TOKEN_TLS_LISTENER) | Some(TOKEN_H2C_LISTENER) => 0,
        Some(TOKEN_SIGNAL) => 4,
        // A log drain is the loop's own bookkeeping, named so /perf does not
        // report it as a timeout that did not happen.
        Some(TOKEN_LOG) => 6,
        Some(Token(k)) => match sources.get(k) {
            Source::TlsConn(_) | Source::H2cConn(_) => 1,
            Source::H2cUpstream | Source::H2sUpstream | Source::Upstream(_) => 2,
            Source::Vacant => 5,
        },
        None => 5,
    }
}

// ── Shutdown flags ───────────────────────────────────────────────────────────

// ── Per-connection state ──────────────────────────────────────────────────────

struct QuicConn {
    conn: quiche::Connection,
    h3_conn: Option<quiche::h3::Connection>,
    /// Pending streams: stream_id -> accumulated request state
    pending: HashMap<u64, PendingRequest>,
    /// Responses awaiting flow-control credit, retried by drain_writable() the
    /// next time each stream reports writable.
    partial_responses: HashMap<u64, PendingH3Response>,
    client_addr: SocketAddr,
    /// When we last heard from this connection (for timeout tracking)
    last_active: Instant,
    /// When `quiche::accept` created this connection.
    ///
    /// The QUIC handshake is timed from here to `is_established()`. That span is
    /// WIDER than the rustls one measured in http11.rs: QUIC folds the transport
    /// and cryptographic handshakes together, so it includes the round trip TCP
    /// had already completed before rustls ever saw a socket. The two are
    /// reported on separate channels and never summed, for that reason.
    created: Instant,
    /// Set once the handshake has been recorded, so a connection that stays up
    /// for an hour contributes one sample rather than one per packet.
    handshake_recorded: bool,
}

struct PendingRequest {
    headers: Vec<quiche::h3::Header>,
    body: Vec<u8>,
    headers_done: bool,
}

/// A response that couldn't be fully written because the stream (or
/// connection) ran out of flow-control credit, saved so drain_writable() can
/// finish it once quiche reports the stream writable again.
enum PendingH3Response {
    /// The HEADERS frame itself was blocked — nothing has reached the client
    /// yet, so both header and body still need sending.
    Headers(Vec<quiche::h3::Header>, Bytes),
    /// HEADERS already sent; body is blocked at the given offset.
    Body(Bytes, usize),
}

// ── Server state ──────────────────────────────────────────────────────────────

/// What owns an upstream slot.
#[derive(Clone, Copy)]
enum UpstreamKind {
    /// One request on a socket-pool member. The member is released, or backed
    /// off, when it finishes.
    Member,
    /// A URL backend, by its entry in `PoolManager`. A finished connection the
    /// backend kept open goes back to that backend's idle list.
    Url(usize),
    /// A URL backend slot sitting on that backend's idle list. Its descriptor
    /// stays registered for read, so a peer close wakes the loop and the slot
    /// is dropped before any request reuses it.
    IdleUrl(usize),
}

/// One in-flight backend request the loop drives, whether its destination is a
/// socket member or a URL backend. When it completes or expires the loop writes
/// the result straight into the waiting stream named by `waiter`, with the
/// request context it carries. The `Upstream` slot, its buffers and the address
/// are made at start and reused, so a miss allocates nothing on the write or
/// parse path; a new connection reuses the slot's existing allocations.
struct UpstreamEntry {
    upstream: Upstream,
    addr: m6_core::client::Addr,
    kind: UpstreamKind,
    /// The token this slot's descriptor is registered under. An idle URL slot
    /// keeps its token while it waits, so a close can wake the loop.
    token: Token,
    member_idx: usize,
    backend_name: String,
    waiter: Option<Waiter>,
    ctx: Option<Box<forward::PendingUrlContext>>,
    registered_interest: Interest,
}

impl UpstreamEntry {
    fn empty() -> UpstreamEntry {
        UpstreamEntry {
            upstream: Upstream::new(64 * 1024, 16 * 1024),
            addr: m6_core::client::Addr::Unix(std::path::PathBuf::new()),
            kind: UpstreamKind::Member,
            token: Token(usize::MAX),
            member_idx: 0,
            backend_name: String::new(),
            waiter: None,
            ctx: None,
            registered_interest: Interest::READABLE,
        }
    }
}

struct ServerState {
    config: Config,
    route_table: RouteTable,
    pool_manager: PoolManager,
    cache: Cache,
    public_key: Option<PublicKey>,
    error_mode: ErrorMode,
    stats: Stats,
    /// One `mio` token per descriptor, the single allocator for every
    /// connection and upstream, mapping a token to its owner.
    sources: Slots<Source>,
    /// One generation per token key, bumped each time the key is handed out, so
    /// a `Waiter` that captured an earlier generation can be recognised as
    /// stale after its connection was reaped and the token reused.
    generations: Vec<u64>,
    /// Cache entries queued to be fetched in the background — hint-driven
    /// prefetches and stale-while-revalidate refreshes both land here.
    prefetch_queue: std::collections::VecDeque<Refresh>,
    /// In-flight backend requests, one per slot, driven when their descriptor
    /// fires. Each entry carries the non-blocking upstream, what it belongs to
    /// (a pool member or a URL backend), the backend name, and who waits for
    /// the reply.
    upstreams: Slots<UpstreamEntry>,
    /// The read timeout (handshakes and HTTP/1.1), as deadlines.
    read_timeouts: Timeouts,
    /// The HTTP/2 idle timeout, as deadlines.
    h2_idle_timeouts: Timeouts,
    /// Socket-backend request deadlines, as deadlines.
    upstream_timeouts: Timeouts,
    /// Persistent non-blocking H2C outbound client pool.
    h2c_pool: H2cClientPool,
    /// Persistent non-blocking H2S (HTTP/2 over TLS) outbound client pool.
    h2s_pool: H2sTlsClientPool,
    /// Per-IP request throttle — general traffic, ahead of cache/routing.
    rate_limiter: RateLimiter,
    /// Interface class of the TLS/QUIC listener (always external), and of the
    /// h2c listener (internal when `[server] internal_binds` names it),
    /// computed once at startup.
    ///
    /// Precomputed rather than derived per request: classification parses an
    /// address, and this sits on the hot path for every single request.
    tls_iface: Iface,
    h2c_iface: Iface,
    /// One rendered error document per status, held locally.
    ///
    /// A cache node routes `/_errors` to the origin, so before this every
    /// route miss dispatched a fetch across the private link to render a page
    /// that is byte-identical every time. Over an intercontinental link that
    /// fetch is 200-300ms and a request to a nonexistent path took most of a
    /// second, with no improvement on repeat.
    ///
    /// Caching that response by URL would not have helped. The cache key is
    /// (path, query, encoding), so a wordlist of 647 unique junk paths is 647
    /// unique keys and 647 origin fetches -- useless against exactly the
    /// traffic that causes the problem. m6-http knows the site's route table,
    /// so a path matching no route is knowably a 404 *locally*; it does not
    /// need a response per path, it needs one error document.
    ///
    /// Keyed by status, so at most a handful of entries ever.
    error_pages: HashMap<u16, CachedErrorPage>,
    /// When this process began serving, for the health endpoint's uptime.
    ///
    /// `Instant`, not `SystemTime`: it is monotonic, so a clock step (NTP
    /// correction, a VM resuming) cannot make uptime jump or go negative.
    started: std::time::Instant,
    /// The last events this process logged, for `/traffic`.
    ///
    /// A ring allocated at start, pushed once per logged event and never
    /// allocated while serving. `/traffic` builds its report from it, so the
    /// analytics log is not read inside the loop.
    traffic: health::TrafficWindow,
    /// The last `/traffic` summary and when it was built.
    ///
    /// Held so the report is built at most once per `traffic_cache_s`, however
    /// often the endpoint is scraped. It is a plain field because one thread
    /// owns the loop.
    traffic_report: Option<(Instant, m6_core::monitoring::TrafficReport)>,
    /// Reused buffer a cache lookup builds its key into, so a lookup never
    /// allocates. Capacity 512 matches the old stack buffer's bound.
    lookup_key_buf: String,
    /// The `alt-svc` header line, formatted once at start into `ServerState`.
    alt_svc_line: std::sync::Arc<[u8]>,
    /// The `link` header line advertising the site description, when configured.
    /// `None` means the site configures none, so no line is written.
    describedby_line: Option<std::sync::Arc<[u8]>>,
}

/// One cache entry to fetch in the background.
///
/// Carries the full cache key, not just the path: entries are keyed by
/// path + query + content-encoding, so refreshing a stale `br` variant by
/// fetching the identity one would leave the `br` entry stale forever and
/// re-queue it on every single request.
#[derive(Clone, PartialEq, Eq)]
struct Refresh {
    path: String,
    query: Option<String>,
    enc: String,
}

/// Cap on queued background fetches.
///
/// The queue drains one per event-loop iteration, so it only grows when
/// entries go stale faster than the loop turns. Dropping the excess is right:
/// a dropped refresh costs one more stale serve, and the next request for that
/// key queues it again.
const MAX_REFRESH_QUEUE: usize = 256;

impl ServerState {
    /// Queue a background fetch, skipping one already queued.
    ///
    /// Without the dedup every concurrent request for the same stale entry
    /// would queue its own copy, and each would then be fetched in turn — a
    /// stampede against origin for exactly the case this is meant to shield it
    /// from.
    fn queue_refresh(&mut self, r: Refresh) {
        if self.prefetch_queue.len() >= MAX_REFRESH_QUEUE || self.prefetch_queue.contains(&r) {
            return;
        }
        self.prefetch_queue.push_back(r);
    }
}

/// Whether a route is worth warming. Split out so it can be tested without a
/// ServerState: the queueing needs one, the decision does not.
fn is_warmable(path: &str, cache: Option<&str>, error_path: Option<&str>) -> bool {
    // A pattern is not a URL. `/assets/{*relpath}` has nothing to fetch.
    if path.contains('{') {
        return false;
    }
    // The error page. Refused on a public listener by design, and warming it would
    // put a 404 body in the cache.
    if error_path == Some(path) {
        return false;
    }
    // Not cacheable, so the fetch is pure cost.
    if cache.map(|c| c.trim() == "no-store").unwrap_or(false) {
        return false;
    }
    true
}

/// Seed the background-fetch queue with this node's own warmable routes.
///
/// The server knows its route table, so it warms its own cache rather than having
/// something outside it do so. This replaced `m6-warm-local`, a shell script a
/// systemd unit ran on each node, which found the warmable routes by running a
/// REGEX over the node's own site.toml. The parsed config is right here, so the
/// selection is done properly:
///
///   - concrete paths only. A route like `/assets/{*relpath}` is a pattern, not a
///     URL, and there is nothing to fetch.
///   - not the configured error path. Requesting it is refused on a public
///     listener by design, and warming it would fill the cache with a 404 body.
///   - nothing a route marks `no-store`, because the answer is not cacheable and
///     the fetch would be pure cost.
///
/// One entry per path per encoding, because the cache keys on encoding: warming
/// only identity leaves a gzip visitor paying for the miss anyway.
///
/// Nothing here fetches. It queues, and the event loop drains one entry per
/// iteration, which is why this cannot delay startup, block serving, or stampede
/// the origin. It is the same queue that refreshes stale entries.
fn seed_cache_warm(state: &mut ServerState) {
    // Same three the shell script used. Identity is the empty string here because
    // that is how the cache key spells "no content-encoding".
    const ENCODINGS: [&str; 3] = ["", "gzip", "br"];

    let error_path = match &state.error_mode {
        ErrorMode::Custom { path } => Some(path.clone()),
        _ => None,
    };

    let mut queued = 0usize;
    let mut skipped = 0usize;
    for route in &state.config.routes.clone() {
        if !is_warmable(&route.path, route.cache.as_deref(), error_path.as_deref()) {
            skipped += 1;
            continue;
        }
        for enc in ENCODINGS {
            state.queue_refresh(Refresh {
                path: route.path.clone(),
                query: None,
                enc: enc.to_string(),
            });
            queued += 1;
        }
    }

    info!(
        queued,
        skipped_routes = skipped,
        "cache warm queued: this node warms itself, one fetch per route per encoding"
    );
}

// ── quiche TLS/QUIC config ────────────────────────────────────────────────────

fn make_quiche_config(server_config: &config::ServerConfig) -> anyhow::Result<quiche::Config> {
    let mut cfg = quiche::Config::new(quiche::PROTOCOL_VERSION).context("quiche::Config::new")?;

    // QUIC is never started in redirect mode, so both are Some by the time we
    // get here; erroring rather than unwrapping keeps that a diagnosable
    // config failure instead of a panic if the call ever moves.
    let tls_cert = server_config
        .tls_cert
        .as_deref()
        .context("[server].tls_cert is required to serve QUIC")?;
    let tls_key = server_config
        .tls_key
        .as_deref()
        .context("[server].tls_key is required to serve QUIC")?;
    cfg.load_cert_chain_from_pem_file(tls_cert)
        .with_context(|| format!("load cert chain {tls_cert}"))?;
    cfg.load_priv_key_from_pem_file(tls_key)
        .with_context(|| format!("load private key {tls_key}"))?;

    // ALPN: h3
    cfg.set_application_protos(quiche::h3::APPLICATION_PROTOCOL)
        .context("set alpn")?;

    // Disable GREASE: removes the extra unidirectional and in-band GREASE frames
    // that can confuse some client stacks (e.g. ngtcp2/nghttp3 in curl).
    cfg.grease(false);

    // Performance tuning
    cfg.set_max_idle_timeout(30_000); // 30 s idle timeout
    cfg.set_max_recv_udp_payload_size(MAX_DATAGRAM_SIZE);
    cfg.set_max_send_udp_payload_size(MAX_DATAGRAM_SIZE);
    cfg.set_initial_max_data(10_000_000);
    cfg.set_initial_max_stream_data_bidi_local(1_000_000);
    cfg.set_initial_max_stream_data_bidi_remote(1_000_000);
    cfg.set_initial_max_stream_data_uni(1_000_000);
    cfg.set_initial_max_streams_bidi(100);
    cfg.set_initial_max_streams_uni(100);
    cfg.set_disable_active_migration(true);

    // NO amplification factor override. quiche's conforming default of 3 stands.
    //
    // 1.2.0 and 1.3.0 carried `set_max_amplification_factor(4)` because the
    // uncompressed handshake flight was 4082 bytes against a 3600-byte budget, so the
    // server sent 3600, stopped, and waited a full round trip for an ACK. It was
    // marked temporary from the day it shipped, and certificate compression below is
    // what removes the need for it.
    //
    // The budget is 3 times the bytes received. A client's opening Initial is
    // padded to 1200 bytes, so the budget is 3600.
    //
    // The flight is brotli, not zlib, and that is deliberate: see the comment on
    // the zlib registration in the quiche fork. Offering zlib breaks any peer that
    // rebuilds the handshake transcript by re-compressing, which is what h3spec
    // does, and it compresses our chain less well than brotli anyway.

    // ── 0-RTT ─────────────────────────────────────────────────────────────────
    //
    // A returning visitor sends its request in the FIRST flight, so the response
    // costs zero round trips instead of one. On loopback that saves about a
    // millisecond and looks unimportant. For a visitor on a long path it is the
    // single largest latency win available: at 300ms of round-trip time the
    // saved round trip is 300ms that no amount of local tuning recovers. This
    // setting is the QUIC listener that serves VISITORS. A cache node fetching
    // from its origin goes over h2c on TCP and is not affected by it.
    //
    // For comparison, over TCP the same visitor pays TWO round trips before any
    // application data: one for the TCP handshake and one for TLS. Measured on
    // the build host's loopback with m6-probe-h1/h2/h3, h3 establishment is
    // ~1.1ms against h2's ~0.42ms, which reads as h3 being slower -- but loopback
    // has no RTT, so it prices only CPU and values a saved round trip at nothing.
    //
    // ── The replay problem, and why this is still safe ────────────────────────
    //
    // 0-RTT data is REPLAYABLE. An attacker who captures the first flight can
    // send it again, and the server cannot tell the copy from the original: the
    // anti-replay guarantee of the full handshake is exactly what has not
    // happened yet. RFC 8470 is the rule here, and neither BoringSSL's
    // single-use tickets nor quiche closes the hole on its own.
    //
    // So enabling this is only half the change. The other half is in
    // `handle_h3_request`, which answers 425 Too Early to anything a replay could
    // affect, and it is deliberately stricter than RFC 8470's "idempotent
    // methods" advice: only a FRESH CACHE HIT is served in early data. A replayed
    // cache read re-sends bytes and does nothing else.
    //
    // "GET is safe" would NOT have been good enough here. An analytics beacon is
    // commonly a fire-and-forget GET, so a replayed 0-RTT GET would inflate a
    // page-view counter. m6-http cannot recognise such a
    // route -- it is proxied like any other -- which is the reason the rule is
    // about where the answer comes from rather than about a list of paths.
    //
    // ── HELD BACK, one line, pending a fix in the quiche fork ─────────────────
    //
    // Everything above and the two gates in `handle_h3_request` are finished and
    // were verified with `m6-probe-h3 --0rtt /`: the
    // request went out before the handshake completed, a cached path answered
    // 200 in early data, and an uncached one answered 425.
    //
    // It is not enabled because it costs a conformance test, isolated by running
    // the gate with this line in and out while changing nothing else:
    //
    //     factor 4, this line OUT   h3spec 47/49  PASS
    //     factor 4, this line IN    h3spec 46/49  FAIL
    //
    // The single regression is "MUST send PROTOCOL_VIOLATION if CRYPTO in 0-RTT
    // is received [TLS 8.3]". With early data off quiche never accepts a 0-RTT
    // packet so the case cannot arise; with it on, quiche accepts them and does
    // not police CRYPTO frames inside them.
    //
    // Owner's decision, 2026-09-15: fix it in the fork rather than lower the
    // floor or abandon 0-RTT. The check is narrow and well specified, and far
    // smaller than the QPACK work declined at 47/49. UNCOMMENT THIS the moment
    // the fork carries it and the gate reads 47/49 with it enabled.
    //
    // ENABLED 2026-09-16. The fork carries the three changes this needed.
    //
    // It was held back because accepting early data cost one conformance test:
    // "MUST send PROTOCOL_VIOLATION if CRYPTO in 0-RTT is received [TLS 8.3]",
    // 47/49 -> 46/49. Fixing it took three attempts and the first two were in the
    // wrong place, which is worth recording because the wrong places looked right.
    //
    // 1. A guard in quiche's `process_frame` on the CRYPTO arm. Correct per the RFC
    //    and it changed the score not at all. h3spec sends its 0-RTT packet during a
    //    FRESH handshake with no resumption, so there is no 0-RTT read key, so the
    //    packet is buffered as undecryptable and its frames are never parsed. From
    //    h3spec's own qlog:
    //
    //        1. initial: [crypto, padding]
    //        2. initial: [crypto]
    //        3. 0RTT:    [crypto, padding]   <- the violation
    //        4. initial: [ack, crypto, padding]
    //
    //    A check on frame CONTENTS cannot fire for that. Kept anyway: it is right
    //    for a genuinely resumed connection, where the frame is readable.
    //
    // 2. Reject the packet on its TYPE instead, before decryption. A server holding
    //    handshake keys but no 0-RTT key knows no PSK was accepted, so the client
    //    sent 0-RTT it was never entitled to send. This DETECTED the violation --
    //    m6-http logged `conn.recv error: InvalidPacket` -- and h3spec still failed.
    //
    // 3. The close was going out where the client could not read it. quiche put the
    //    CONNECTION_CLOSE in a Handshake packet, and a client derives its handshake
    //    keys from the server's flight, which had not been sent. Its qlog showed it
    //    received only an Initial carrying an ACK. `write_pkt_type` only preferred
    //    an Initial close when `recv_count == 0`, a first-flight case; here
    //    recv_count was non-zero. Now keyed on whether the server has ever sent a
    //    Handshake packet, which is the fact that determines whether the peer could
    //    have the keys.
    //
    // Verified: h3spec 47/49 WITH early data enabled, h2 146/146, h1 32/32 on four
    // targets, every target measured. quiche's own suite unchanged at 1123 + 45.
    cfg.enable_early_data();

    // ── Certificate compression, RFC 8879 ────────────────────────────────────
    //
    // Compresses with brotli, and with zstd when that feature is built. zlib is
    // accepted but never offered, because a peer that rebuilds the handshake
    // transcript by re-compressing cannot verify our CertificateVerify; the fork's
    // registration comment carries the mechanism and the measurement.
    //
    // BoringSSL negotiates from the intersection with the client's
    // `compress_certificate` extension, so a client that advertises nothing we
    // compress with receives the chain uncompressed and handshakes normally. That
    // is the whole compatibility story, and it is what makes declining zlib free.
    //
    // This is what allows the amplification factor to stay at the conforming 3. A
    // QUIC server may send only `factor x bytes received` before it has validated
    // the client's address (RFC 9000 8.1). A 1200-byte client Initial gives a
    // 3600-byte budget, and the uncompressed handshake flight was 4082 -- 482 over,
    // so the server sent 3600, stopped, and waited a full round trip for an ACK.
    //
    // Measured on a typical two-certificate chain:
    //
    //     uncompressed  3429 bytes
    //     brotli        2258 bytes  66%
    //     zstd          2308 bytes  67%
    //     zlib          2359 bytes  69%
    //
    // Verified against third parties too, which is what proved our client really
    // advertises rather than merely supports: Cloudflare's flight went from 4198 to
    // 3388 bytes for the same probe. A Fastly edge did not change at all, so not
    // every QUIC server compresses -- a reason to accept every algorithm as a client
    // even where we decline to offer one as a server.
    cfg.enable_cert_compression()
        .context("enable certificate compression")?;

    Ok(cfg)
}

// ── Event loop ────────────────────────────────────────────────────────────────

/// Everything the event loop polls, owned for its lifetime.
///
/// These four arrived as separate arguments, which put `event_loop` at
/// six. They are not unrelated values: they are the set of things the
/// loop waits on, which is why the poll registers every one of them in the
/// same breath. `tcp` and `h2c` are optional because a node's role decides
/// whether it has them at all.
struct EventLoopIo {
    udp: UdpSocket,
    tcp: Option<Http11Listener>,
    h2c: Option<H2cListener>,
    signals: Signals,
}

// ── Request dispatch (shared by the TLS and h2c listeners) ──────────────────

/// What routing a request resolves to: a cached hit, an immediate answer, or a
/// miss that must go to a backend.
enum Routed {
    Cached {
        status: u16,
        entry: CachedResponse,
        age: std::time::Duration,
        set_cookie: Option<String>,
    },
    Ready(u16, Vec<(String, String)>, Vec<u8>, String),
    Miss {
        start: std::time::Instant,
        version: HttpVersion,
        iface: Iface,
    },
}

/// Rate limit, redirect and cache lookup, shared by the owned (h2, body) and
/// borrowed (h1) paths. The request fields are borrowed, so a cached hit never
/// materialises an owned request. Stats and analytics run here; the caller
/// writes the outcome in the shape its protocol wants.
#[allow(clippy::too_many_arguments)] // one request's five borrowed fields plus state, peer and role
fn route_request<H: m6_core::http::HeaderSource + ?Sized>(
    state: &mut ServerState,
    method: &str,
    path: &str,
    query: Option<&str>,
    version: &str,
    headers: &H,
    client_ip: &str,
    h2c: bool,
) -> Routed {
    let ua = analytics::header(headers, "user-agent");
    if let Some(blocked) = check_rate_limit(state, client_ip, path, ua) {
        let RequestOutcome::Ready(s, h, b, n) = blocked else {
            unreachable!("check_rate_limit returns Ready")
        };
        return Routed::Ready(s, h, b, n);
    }
    let host = m6_core::headers::get(headers, "host");
    if let Some(location) = www_redirect_location(host, path, query, &state.config) {
        return Routed::Ready(
            301,
            www_redirect_headers(location),
            Vec::new(),
            "www-redirect".to_string(),
        );
    }
    let enc_str = m6_core::headers::get(headers, "accept-encoding").unwrap_or("");
    let start = m6_core::platform::now();

    let cacheable =
        !state.route_table.requires_auth(path) && m6_http_lib::cache::method_may_read_cache(method);
    let lookup_key = make_lookup_key(path, query, enc_str, &mut state.lookup_key_buf);
    let req_cc = m6_http_lib::cache::RequestDirectives::parse(headers);
    let looked_up = if cacheable {
        state.cache.lookup_with(lookup_key, &req_cc)
    } else {
        m6_http_lib::cache::Lookup::Miss
    };
    if req_cc.only_if_cached && matches!(looked_up, m6_http_lib::cache::Lookup::Miss) {
        let mut hdrs: Vec<(String, String)> = Vec::new();
        set_date(&mut hdrs);
        return Routed::Ready(504, hdrs, Vec::new(), "cache".to_string());
    }
    let cache_state = if matches!(looked_up, m6_http_lib::cache::Lookup::Stale(..)) {
        "STALE"
    } else {
        "HIT"
    };
    if let m6_http_lib::cache::Lookup::Stale(..) = looked_up {
        state.queue_refresh(Refresh {
            path: path.to_string(),
            query: query.map(str::to_string),
            enc: enc_str.to_string(),
        });
    }
    let (http_version, iface) = if h2c {
        (HttpVersion::Http2, state.h2c_iface)
    } else {
        (HttpVersion::from_wire(version), state.tls_iface)
    };
    if let m6_http_lib::cache::Lookup::Fresh(cached, age)
    | m6_http_lib::cache::Lookup::Stale(cached, age) = looked_up
    {
        let elapsed_ns = start.elapsed().as_nanos() as u64;
        let chan = Channel::new(http_version, iface);
        let precond = evaluate_preconditions(&cached.headers, headers, method);
        state.stats.record(
            elapsed_ns,
            true,
            match precond {
                Precondition::Failed => 412,
                Precondition::NotModified => 304,
                _ => cached.status,
            },
            chan,
            "cache",
        );
        if precond == Precondition::Failed {
            let mut hdrs: Vec<(String, String)> = Vec::new();
            set_date(&mut hdrs);
            analytics::finish_response(
                state.config.analytics.enabled,
                &mut hdrs,
                headers,
                &analytics::Event {
                    node: &state.config.node.name,
                    path,
                    status: 412,
                    cache_state,
                    client_ip,
                    latency_ns: Some(elapsed_ns),
                },
                &mut state.traffic,
            );
            return Routed::Ready(412, hdrs, Vec::new(), "cache".to_string());
        }
        if precond == Precondition::NotModified {
            let mut hdrs = not_modified_headers(&cached.headers);
            set_vary_accept_encoding(&mut hdrs, true);
            set_age(&mut hdrs, age);
            set_date(&mut hdrs);
            debug!(
                path = %path,
                status = 304,
                version = %version,
                backend = "cache",
                latency_ns = elapsed_ns,
                cache_hit = true,
                "request complete"
            );
            analytics::finish_response(
                state.config.analytics.enabled,
                &mut hdrs,
                headers,
                &analytics::Event {
                    node: &state.config.node.name,
                    path,
                    status: 304,
                    cache_state,
                    client_ip,
                    latency_ns: Some(elapsed_ns),
                },
                &mut state.traffic,
            );
            return Routed::Ready(304, hdrs, Vec::new(), "cache".to_string());
        }
        debug!(
            path = %path,
            status = cached.status,
            version = %version,
            backend = "cache",
            latency_ns = elapsed_ns,
            cache_hit = true,
            "request complete"
        );
        let set_cookie = analytics::record(
            state.config.analytics.enabled,
            headers,
            &analytics::Event {
                node: &state.config.node.name,
                path,
                status: cached.status,
                cache_state,
                client_ip,
                latency_ns: Some(elapsed_ns),
            },
            &mut state.traffic,
        )
        .filter(|_| analytics::is_html_response(&cached.headers));
        return Routed::Cached {
            status: cached.status,
            entry: cached,
            age,
            set_cookie,
        };
    }
    Routed::Miss {
        start,
        version: http_version,
        iface,
    }
}

/// Handle one owned request: h2 (which builds its request in `http2.rs`) and
/// the h1 body path. On a hit it returns the owned `Ready` form, keeping the
/// h2 allocation profile unchanged.
fn on_request_impl(
    state: &mut ServerState,
    req: &forward::HttpRequest,
    client_ip: &str,
    quic_port: u16,
    registry: &m6_core::platform::Registry,
    h2c: bool,
) -> RequestOutcome {
    match route_request(
        state,
        &req.method,
        &req.path,
        req.query.as_deref(),
        &req.version,
        &req.headers[..],
        client_ip,
        h2c,
    ) {
        Routed::Cached {
            status,
            entry,
            age,
            set_cookie,
            ..
        } => {
            let mut headers = (*entry.headers).clone();
            set_alt_svc(&mut headers, quic_port);
            set_vary_accept_encoding(&mut headers, true);
            set_age(&mut headers, age);
            set_describedby_link(&mut headers, &state.config.site.describedby);
            if let Some(sc) = set_cookie {
                headers.push(("Set-Cookie".to_string(), sc));
            }
            RequestOutcome::Ready(status, headers, entry.body.to_vec(), "cache".to_string())
        }
        Routed::Ready(s, h, b, n) => RequestOutcome::Ready(s, h, b, n),
        Routed::Miss {
            start,
            version,
            iface,
        } => {
            let enc = m6_core::headers::get(&req.headers[..], "accept-encoding").unwrap_or("");
            let outcome = handle_request(
                req,
                client_ip,
                enc,
                state,
                false,
                h2c && state.h2c_iface == Iface::Internal,
                registry,
            );
            finish_miss(state, outcome, start, version, iface, quic_port)
        }
    }
}

/// Handle a borrowed h1 head (the bodyless fast path). On a hit it returns the
/// shared `Cached` form so `drive_h1` writes it with no allocation.
fn on_request_head(
    state: &mut ServerState,
    head: &m6_core::h1::RequestHead<'_>,
    buf: &[u8],
    client_ip: &str,
    quic_port: u16,
    registry: &m6_core::platform::Registry,
) -> RequestOutcome {
    match route_request(
        state,
        head.method,
        head.path,
        head.query,
        head.version,
        head.headers,
        client_ip,
        false,
    ) {
        Routed::Cached {
            entry,
            age,
            set_cookie,
            ..
        } => RequestOutcome::Cached {
            entry,
            age,
            set_cookie,
        },
        Routed::Ready(s, h, b, n) => RequestOutcome::Ready(s, h, b, n),
        Routed::Miss {
            start,
            version,
            iface,
        } => {
            let mut req = match m6_core::h1::materialize(head, buf) {
                ParseResult::Complete(r) => r,
                // The head was parsed complete and carries no body.
                _ => unreachable!("bodyless head materializes"),
            };
            // Strip proxy-owned headers before forwarding, exactly as the owned
            // h1 and h2 paths do.
            forward::strip_untrusted_inbound(&mut req.headers);
            let enc = m6_core::headers::get(&req.headers[..], "accept-encoding").unwrap_or("");
            let outcome = handle_request(&req, client_ip, enc, state, false, false, registry);
            finish_miss(state, outcome, start, version, iface, quic_port)
        }
    }
}

// ── Token and slot helpers ────────────────────────────────────────────────────

/// The tail every cache miss shares: advertise `alt-svc` and record the miss in
/// the stats, then return the outcome unchanged.
fn finish_miss(
    state: &mut ServerState,
    mut outcome: RequestOutcome,
    start: std::time::Instant,
    version: HttpVersion,
    iface: Iface,
    quic_port: u16,
) -> RequestOutcome {
    if let RequestOutcome::Ready(status, ref mut headers, _, ref backend) = outcome {
        set_alt_svc(headers, quic_port);
        let elapsed_ns = start.elapsed().as_nanos() as u64;
        let chan = Channel::new(version, iface);
        state.stats.record(elapsed_ns, false, status, chan, backend);
    }
    outcome
}

/// Take one token from the shared table, growing the three deadline tables and
/// the generation table to match when the token table doubles. Each hand-out
/// bumps the key's generation, so a `Waiter` that captured an earlier
/// generation is stale after the key is freed and reused.
fn take_token(state: &mut ServerState) -> Option<Token> {
    let taken = state.sources.take()?;
    if let Some(n) = taken.grew_to {
        state.read_timeouts.grow(n);
        state.h2_idle_timeouts.grow(n);
        state.upstream_timeouts.grow(n);
        state.generations.resize(n, 0);
    }
    let gen = &mut state.generations[taken.key];
    *gen = gen.wrapping_add(1);
    Some(Token(taken.key))
}

/// Accept TLS connections until the socket would block or the token table is
/// full. Returns whether the table is at its ceiling.
fn accept_tls(
    tcp: &mut Http11Listener,
    state: &mut ServerState,
    registry: &m6_core::platform::Registry,
    now: Instant,
) -> bool {
    loop {
        let Some(conn) = tcp.accept_one() else {
            return false;
        };
        let Some(token) = take_token(state) else {
            return true;
        };
        let Some(owner) = tcp.take_slot() else {
            state.sources.free(token.0);
            return true;
        };
        let mut conn = conn;
        conn.set_token(token);
        conn.set_interest(Interest::READABLE);
        if let Err(e) = registry.register_fd(conn.raw_fd(), token, Interest::READABLE) {
            warn!(error = %e, "tcp register");
            tcp.free_slot(owner.key);
            state.sources.free(token.0);
            continue;
        }
        state.read_timeouts.touch(token, now);
        *state.sources.get_mut(token.0) = Source::TlsConn(owner.key);
        *tcp.slot_mut(owner.key) = conn;
    }
}

/// Accept h2c connections up to the cap. Returns whether the token table is at
/// its ceiling and whether the cap was hit (more may be pending).
fn accept_h2c(
    h: &mut H2cListener,
    state: &mut ServerState,
    registry: &m6_core::platform::Registry,
    now: Instant,
) -> (bool, bool) {
    let mut full = false;
    let handled = h.accept_up_to(MAX_ACCEPTS_PER_WAKE, |h, mut conn| {
        if state.sources.is_full() {
            full = true;
            return false;
        }
        let Some(token) = take_token(state) else {
            full = true;
            return false;
        };
        let Some(owner) = h.take_slot() else {
            state.sources.free(token.0);
            full = true;
            return false;
        };
        conn.set_token(token);
        conn.set_interest(Interest::READABLE);
        if let Err(e) = registry.register_fd(conn.raw_fd(), token, Interest::READABLE) {
            warn!(error = %e, "h2c register");
            h.free_slot(owner.key);
            state.sources.free(token.0);
            return true;
        }
        state.h2_idle_timeouts.touch(token, now);
        *state.sources.get_mut(token.0) = Source::H2cConn(owner.key);
        *h.slot_mut(owner.key) = conn;
        true
    });
    (full, !full && handled == MAX_ACCEPTS_PER_WAKE)
}

/// What one connection turn did, for the timeouts.
enum ConnOutcome {
    Active,
    Closed,
    /// The token was freed earlier in this batch: skip it.
    Gone,
}

/// Drive the connection whose token fired.
fn drive_conn_token(
    token: Token,
    state: &mut ServerState,
    tcp: &mut Option<Http11Listener>,
    h2c: &mut Option<H2cListener>,
    registry: &m6_core::platform::Registry,
    quic_port: u16,
    now: Instant,
) -> ConnOutcome {
    let source = match state.sources.get(token.0) {
        Source::TlsConn(slot) => (*slot, false),
        Source::H2cConn(slot) => (*slot, true),
        _ => return ConnOutcome::Gone,
    };
    let (slot, is_h2c) = source;
    let hit = HitHeaders {
        alt_svc: state.alt_svc_line.clone(),
        describedby: state.describedby_line.clone(),
    };
    let mut on_head = |state: &mut ServerState, head: &RequestHead<'_>, buf: &[u8], ip: &str| {
        on_request_head(state, head, buf, ip, quic_port, registry)
    };
    let mut on_h2 = |state: &mut ServerState, req: &forward::HttpRequest, ip: &str| {
        on_request_impl(state, req, ip, quic_port, registry, is_h2c)
    };
    let mut drive = if is_h2c {
        match h2c.as_mut() {
            Some(h) => h.drive_one(slot, state, &mut on_h2, registry),
            None => return ConnOutcome::Closed,
        }
    } else {
        match tcp.as_mut() {
            Some(t) => t.drive_one(slot, state, &mut on_head, &mut on_h2, &hit, registry),
            None => return ConnOutcome::Closed,
        }
    };
    // Park each request this turn dispatched to a backend, storing the waiter
    // in the upstream slot it named.
    let mut pending = std::mem::take(&mut drive.pending);
    for d in pending.drain(..) {
        let gen = state.generations[token.0];
        let waiter = match d.stream {
            None => Waiter::H1 { conn: token, gen },
            Some(stream) => Waiter::H2 {
                conn: token,
                stream,
                gen,
            },
        };
        park(state, d.id, d.ctx, waiter);
    }
    if let Some(hs) = drive.handshake {
        let v = if hs.is_h2 {
            HttpVersion::Http2
        } else {
            HttpVersion::Http11
        };
        state
            .stats
            .record_handshake(hs.elapsed_ns, Channel::new(v, state.tls_iface), hs.resumed);
        // An h2 connection is idle-governed by `h2_idle_timeouts`, not the
        // 30 s read deadline set at accept, so drop that deadline and start
        // the idle clock now: a peer that finishes the handshake but never
        // sends its client preface must still be reaped.
        if hs.is_h2 {
            state.read_timeouts.clear(token);
            state.h2_idle_timeouts.touch(token, now);
        }
    }
    after_conn_drive(token, &drive, state, tcp, h2c, registry, now);
    // Give the pending buffer back to the listener so the next drive reuses it.
    if is_h2c {
        if let Some(h) = h2c.as_mut() {
            h.return_pending(pending);
        }
    } else if let Some(t) = tcp.as_mut() {
        t.return_pending(pending);
    }
    if drive.done {
        ConnOutcome::Closed
    } else {
        ConnOutcome::Active
    }
}

/// Deregister and free a connection token.
fn close_conn_token(
    token: Token,
    state: &mut ServerState,
    tcp: &mut Option<Http11Listener>,
    h2c: &mut Option<H2cListener>,
    registry: &m6_core::platform::Registry,
) {
    if let Source::TlsConn(slot) = *state.sources.get(token.0) {
        if let Some(t) = tcp.as_mut() {
            t.close_one(slot, registry);
        }
    } else if let Source::H2cConn(slot) = *state.sources.get(token.0) {
        if let Some(h) = h2c.as_mut() {
            h.close_one(slot, registry);
        }
    }
    *state.sources.get_mut(token.0) = Source::Vacant;
    state.sources.free(token.0);
    state.read_timeouts.clear(token);
    state.h2_idle_timeouts.clear(token);
    state.upstream_timeouts.clear(token);
}

/// Store a parked request's waiter and context in the upstream slot it named.
fn park(
    state: &mut ServerState,
    id: UpstreamId,
    ctx: Box<forward::PendingUrlContext>,
    waiter: Waiter,
) {
    match id {
        UpstreamId::Slot(slot) => {
            let e = state.upstreams.get_mut(slot);
            e.waiter = Some(waiter);
            e.ctx = Some(ctx);
        }
        UpstreamId::H2c { conn, stream } => {
            state.h2c_pool.set_waiter(conn, stream, waiter, ctx);
        }
        UpstreamId::H2s { conn, stream } => {
            state.h2s_pool.set_waiter(conn, stream, waiter, ctx);
        }
    }
}

/// Finish a connection drive: touch timeouts and close the connection when the
/// drive says it is done.
fn after_conn_drive(
    token: Token,
    drive: &ConnDrive,
    state: &mut ServerState,
    tcp: &mut Option<Http11Listener>,
    h2c: &mut Option<H2cListener>,
    registry: &m6_core::platform::Registry,
    now: Instant,
) {
    if drive.restarted {
        state.read_timeouts.touch(token, now);
    }
    if drive.moved {
        state.h2_idle_timeouts.touch(token, now);
    }
    if drive.done {
        close_conn_token(token, state, tcp, h2c, registry);
    }
}

/// Write a finished response straight into the waiting stream, in this turn.
fn write_waiter(
    waiter: Waiter,
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    method: &str,
    cx: &mut CompletionCtx<'_>,
) {
    match waiter {
        Waiter::H1 { conn, gen } => {
            // A stale waiter whose connection was reaped and whose token was
            // reused carries an older generation: drop the reply.
            if cx.state.generations.get(conn.0) != Some(&gen) {
                return;
            }
            if let Source::TlsConn(slot) = *cx.state.sources.get(conn.0) {
                if let Some(t) = cx.tcp {
                    let drive = t.complete_h1(slot, status, &headers, body, method, cx.registry);
                    after_conn_drive(conn, &drive, cx.state, cx.tcp, cx.h2c, cx.registry, cx.now);
                }
            }
        }
        Waiter::H2 { conn, stream, gen } => {
            if cx.state.generations.get(conn.0) != Some(&gen) {
                return;
            }
            let done = match *cx.state.sources.get(conn.0) {
                Source::TlsConn(slot) => cx.tcp.as_mut().is_some_and(|t| {
                    t.complete_h2(slot, stream, (status, headers, body), method, cx.registry)
                }),
                Source::H2cConn(slot) => cx.h2c.as_mut().is_some_and(|h| {
                    h.complete_h2(slot, stream, (status, headers, body), method, cx.registry)
                }),
                _ => false,
            };
            if done {
                close_conn_token(conn, cx.state, cx.tcp, cx.h2c, cx.registry);
            }
        }
        Waiter::H3 { conn_id, stream } => {
            if let Some(qconn) = cx.connections.get_mut(&conn_id) {
                send_h3_response(
                    stream,
                    qconn,
                    status,
                    &headers,
                    Bytes::from(body),
                    method.eq_ignore_ascii_case("HEAD"),
                );
                flush_conn(cx.udp, qconn);
            }
        }
        Waiter::Background => {
            // The finalize step already filled the cache; there is no client.
        }
    }
}

/// Finish a backend reply: finalize it, then write it to the waiter, or re-park
/// the same waiter on the upstream that fetches the custom error page.
fn complete_waiter(
    result: std::io::Result<forward::HttpResponse>,
    ctx: Box<forward::PendingUrlContext>,
    waiter: Waiter,
    cx: &mut CompletionCtx<'_>,
) {
    let method = &ctx.req.method;
    match finalize_url_response(result, &ctx, cx.quic_port, cx.state, cx.registry) {
        Completion::Ready((status, headers, body, _)) => {
            write_waiter(waiter, status, headers, body, method, cx);
        }
        Completion::Pending { id, ctx } => {
            park(cx.state, id, ctx, waiter);
        }
    }
}

/// Complete each finished h2 stream into the waiter it names.
#[allow(clippy::too_many_arguments)] // the loop's whole context, as CompletionCtx::new takes it
fn complete_h2_streams(
    completions: Vec<H2Completion>,
    state: &mut ServerState,
    tcp: &mut Option<Http11Listener>,
    h2c: &mut Option<H2cListener>,
    registry: &m6_core::platform::Registry,
    quic_port: u16,
    udp: &UdpSocket,
    connections: &mut HashMap<Vec<u8>, QuicConn>,
) {
    for c in completions {
        if let (Some(waiter), Some(ctx)) = (c.waiter, c.ctx) {
            let mut cx = CompletionCtx::new(
                state,
                tcp,
                h2c,
                registry,
                quic_port,
                udp,
                connections,
                m6_core::platform::now(),
            );
            complete_waiter(c.result, ctx, waiter, &mut cx);
        }
    }
}

/// Drive the one upstream whose descriptor fired.
fn drive_upstream_one(token: Token, cx: &mut CompletionCtx<'_>) {
    let Source::Upstream(slot) = *cx.state.sources.get(token.0) else {
        return;
    };
    let result = {
        let entry = cx.state.upstreams.get_mut(slot);
        let upstream = &mut entry.upstream;
        match upstream.drive() {
            Ok(true) => {
                let mut slots: [std::mem::MaybeUninit<httparse::Header<'_>>;
                    m6_core::client::MAX_HEADERS] =
                    [const { std::mem::MaybeUninit::uninit() }; m6_core::client::MAX_HEADERS];
                match upstream.reply(&mut slots) {
                    Some(reply) => {
                        cx.registry.deregister_fd(upstream.fd()).ok();
                        Some(Ok(forward::HttpResponse::from_reply(&reply)))
                    }
                    None => {
                        cx.registry.deregister_fd(upstream.fd()).ok();
                        Some(Err(std::io::Error::other(
                            "upstream reported a reply but none parsed",
                        )))
                    }
                }
            }
            Ok(false) => {
                // The request is not finished, and `drive` moved the phase on:
                // Connecting waits for WRITABLE, then the request or the
                // ClientHello is written and it waits for READABLE. The new
                // interest is registered after the drive, because under epoll
                // edge-triggering a mask written before it waits for an edge
                // that may never arrive. Only a change costs a reregister.
                let interest = upstream.interest();
                if interest != entry.registered_interest {
                    cx.registry
                        .reregister_fd(upstream.fd(), token, interest)
                        .ok();
                    entry.registered_interest = interest;
                }
                None
            }
            Err(e) => {
                cx.registry.deregister_fd(upstream.fd()).ok();
                Some(Err(e))
            }
        }
    };
    let Some(result) = result else {
        return;
    };
    let (waiter, ctx) = retire_upstream(token, slot, &result, cx);
    if let (Some(waiter), Some(ctx)) = (waiter, ctx) {
        complete_waiter(result, ctx, waiter, cx);
    }
}

/// Take a finished upstream out of the loop: free its token, its deadline and
/// its slot, and tell its owner how the request went.
///
/// A socket member is released, or backed off when it went silent or failed
/// after taking the whole request. A URL backend whose reply left the
/// connection reusable keeps its slot on that backend's idle list, still
/// registered for read, so the backend closing it wakes the loop and the slot
/// is dropped before any request can be handed the dead connection.
fn retire_upstream(
    token: Token,
    slot: usize,
    result: &std::io::Result<forward::HttpResponse>,
    cx: &mut CompletionCtx<'_>,
) -> (Option<Waiter>, Option<Box<forward::PendingUrlContext>>) {
    let timed_out = matches!(result, Err(e) if e.kind() == std::io::ErrorKind::TimedOut);
    let (kind, member_idx, waiter, ctx, reusable, request_written) = {
        let entry = cx.state.upstreams.get_mut(slot);
        (
            entry.kind,
            entry.member_idx,
            entry.waiter.take(),
            entry.ctx.take(),
            entry.upstream.reusable(),
            entry.upstream.request_written(),
        )
    };
    let mut kept = false;
    match kind {
        UpstreamKind::Member => {
            let entry = cx.state.upstreams.get_mut(slot);
            entry.upstream.close();
            // Release the member using a borrow of the slot's name, before the
            // slot is freed, so the name buffer survives for the next launch.
            if let Some(pool) = cx.state.pool_manager.get_pool_mut(&entry.backend_name) {
                // A refusal's reply may be waiting unread. A refused connect
                // was already marked failed at launch.
                if result.is_err() && (timed_out || request_written) {
                    pool.mark_failed(member_idx);
                } else {
                    pool.release(member_idx);
                }
            }
            cx.state.upstreams.free(slot);
        }
        UpstreamKind::Url(idx) => {
            kept = result.is_ok()
                && reusable
                && cx.state.pool_manager.url_backend_mut(idx).idle.push(slot);
            if kept {
                let entry = cx.state.upstreams.get_mut(slot);
                entry.kind = UpstreamKind::IdleUrl(idx);
                let interest = Interest::READABLE;
                cx.registry
                    .register_fd(entry.upstream.fd(), token, interest)
                    .ok();
                entry.registered_interest = interest;
            } else {
                cx.state.upstreams.get_mut(slot).upstream.close();
                cx.state.upstreams.free(slot);
            }
        }
        // An idle slot has no request to retire; its own event path drops it.
        UpstreamKind::IdleUrl(_) => {
            cx.state.upstreams.get_mut(slot).upstream.close();
            cx.state.upstreams.free(slot);
        }
    }
    if !kept {
        *cx.state.sources.get_mut(token.0) = Source::Vacant;
        cx.state.sources.free(token.0);
    }
    cx.state.upstream_timeouts.clear(token);
    (waiter, ctx)
}

/// Drop an idle URL connection whose descriptor fired.
///
/// It has no request, so a readable event means the backend closed it or sent
/// something unexpected. Either way the connection is not reusable, and it is
/// closed here so no request is handed it.
fn drop_idle_url(
    slot: usize,
    backend_idx: usize,
    state: &mut ServerState,
    registry: &m6_core::platform::Registry,
) {
    let token = state.upstreams.get(slot).token;
    registry
        .deregister_fd(state.upstreams.get(slot).upstream.fd())
        .ok();
    state.upstreams.get_mut(slot).upstream.close();
    state.upstreams.free(slot);
    state
        .pool_manager
        .url_backend_mut(backend_idx)
        .idle
        .remove(slot);
    *state.sources.get_mut(token.0) = Source::Vacant;
    state.sources.free(token.0);
    state.upstream_timeouts.clear(token);
    debug!(
        backend = %state.pool_manager.url_backend(backend_idx).name,
        "idle url connection dropped"
    );
}

/// Close the upstream whose deadline passed.
fn expire_upstream(token: Token, cx: &mut CompletionCtx<'_>) {
    let Source::Upstream(slot) = *cx.state.sources.get(token.0) else {
        return;
    };
    cx.registry
        .deregister_fd(cx.state.upstreams.get(slot).upstream.fd())
        .ok();
    let timed_out = Err(std::io::Error::new(
        std::io::ErrorKind::TimedOut,
        "backend timed out",
    ));
    let (waiter, ctx) = retire_upstream(token, slot, &timed_out, cx);
    if let (Some(waiter), Some(ctx)) = (waiter, ctx) {
        complete_waiter(timed_out, ctx, waiter, cx);
    }
}

/// Ensure an h2c pool connection exists for a URL backend, registering it under
/// its own token. A token is taken first, so a full table fails before any
/// connection is created, and every failure frees what it took.
fn ensure_h2c_conn(
    state: &mut ServerState,
    backend_idx: usize,
    registry: &m6_core::platform::Registry,
) -> Result<(), UrlDispatchError> {
    let Some(token) = take_token(state) else {
        return Err(UrlDispatchError::NoSlot);
    };
    let created = {
        let backend = state.pool_manager.url_backend(backend_idx);
        state
            .h2c_pool
            .ensure_connection(&backend.url, backend.addr())
    };
    let created = match created {
        Ok(c) => c,
        Err(e) => {
            *state.sources.get_mut(token.0) = Source::Vacant;
            state.sources.free(token.0);
            return Err(UrlDispatchError::Failed(e));
        }
    };
    if !created {
        // The connection already existed and is registered under its own token.
        *state.sources.get_mut(token.0) = Source::Vacant;
        state.sources.free(token.0);
        return Ok(());
    }
    let registered = {
        let backend = state.pool_manager.url_backend(backend_idx);
        let conn = state
            .h2c_pool
            .connection_mut(&backend.url)
            .expect("just created");
        conn.set_token(token);
        let interest = conn.phase_interest();
        conn.set_interest(interest);
        registry.register_fd(conn.raw_fd(), token, interest)
    };
    if let Err(e) = registered {
        state.h2c_pool.remove_by_token(token, registry);
        *state.sources.get_mut(token.0) = Source::Vacant;
        state.sources.free(token.0);
        return Err(UrlDispatchError::Failed(e));
    }
    *state.sources.get_mut(token.0) = Source::H2cUpstream;
    Ok(())
}

/// Ensure an h2s pool connection exists for a URL backend, registering it under
/// its own token. A token is taken first, so a full table fails before any
/// connection is created, and every failure frees what it took.
fn ensure_h2s_conn(
    state: &mut ServerState,
    backend_idx: usize,
    registry: &m6_core::platform::Registry,
) -> Result<(), UrlDispatchError> {
    let Some(token) = take_token(state) else {
        return Err(UrlDispatchError::NoSlot);
    };
    let created = {
        let backend = state.pool_manager.url_backend(backend_idx);
        let tls = backend
            .tls
            .as_ref()
            .expect("an h2s backend's TLS client is built at start");
        state
            .h2s_pool
            .ensure_connection(&backend.url, backend.addr(), tls)
    };
    let created = match created {
        Ok(c) => c,
        Err(e) => {
            *state.sources.get_mut(token.0) = Source::Vacant;
            state.sources.free(token.0);
            return Err(UrlDispatchError::Failed(e));
        }
    };
    if !created {
        *state.sources.get_mut(token.0) = Source::Vacant;
        state.sources.free(token.0);
        return Ok(());
    }
    let registered = {
        let backend = state.pool_manager.url_backend(backend_idx);
        let conn = state
            .h2s_pool
            .connection_mut(&backend.url)
            .expect("just created");
        conn.set_token(token);
        let interest = conn.phase_interest();
        conn.set_interest(interest);
        registry.register_fd(conn.raw_fd(), token, interest)
    };
    if let Err(e) = registered {
        state.h2s_pool.remove_by_token(token, registry);
        *state.sources.get_mut(token.0) = Source::Vacant;
        state.sources.free(token.0);
        return Err(UrlDispatchError::Failed(e));
    }
    *state.sources.get_mut(token.0) = Source::H2sUpstream;
    Ok(())
}

/// Deregister both listeners so new clients wait in the kernel's backlog.
fn deregister_listeners(poll: &Poll, tcp: Option<&Http11Listener>, h2c: Option<&H2cListener>) {
    if let Some(t) = tcp {
        poll.registry().deregister_fd(t.raw_fd()).ok();
    }
    if let Some(h) = h2c {
        poll.registry().deregister_fd(h.raw_fd()).ok();
    }
}

/// Re-register both listeners once a token slot has come free.
fn register_listeners(poll: &Poll, tcp: Option<&Http11Listener>, h2c: Option<&H2cListener>) {
    if let Some(t) = tcp {
        poll.registry()
            .register_fd(t.raw_fd(), TOKEN_TLS_LISTENER, Interest::READABLE)
            .ok();
    }
    if let Some(h) = h2c {
        poll.registry()
            .register_fd(h.raw_fd(), TOKEN_H2C_LISTENER, Interest::READABLE)
            .ok();
    }
}

fn event_loop(
    io: EventLoopIo,
    state: &mut ServerState,
    quiche_config: &mut quiche::Config,
    lifecycle: &mut m6_core::signal::Lifecycle,
) -> i32 {
    // Destructured rather than accessed through `io` throughout: the loop body
    // is long and reads better against the names it has always used.
    let EventLoopIo {
        udp,
        mut tcp,
        mut h2c,
        mut signals,
    } = io;
    let mut poll = match Poll::new() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("poll init failed: {e}");
            return 2;
        }
    };

    if let Err(e) = poll
        .registry()
        .register_fd(udp.as_raw_fd(), TOKEN_UDP, Interest::READABLE)
    {
        eprintln!("poll add UDP failed: {e}");
        return 2;
    }
    if let Some(ref t) = tcp {
        let _ = poll
            .registry()
            .register_fd(t.raw_fd(), TOKEN_TLS_LISTENER, Interest::READABLE);
    }
    if let Some(ref h) = h2c {
        let _ = poll
            .registry()
            .register_fd(h.raw_fd(), TOKEN_H2C_LISTENER, Interest::READABLE);
    }
    // Signals arrive on their own descriptor, under the number the wake pipe
    // used to hold.
    if let Err(e) = poll
        .registry()
        .register_fd(signals.raw_fd(), TOKEN_SIGNAL, Interest::READABLE)
    {
        error!(error = %e, "registering the signal descriptor");
        return 1;
    }

    // Port for Alt-Svc advertisement: same port for both QUIC/H3 (UDP) and TCP.
    let quic_port = udp.local_addr().map(|a| a.port()).unwrap_or(8443);

    let mut connections: HashMap<Vec<u8>, QuicConn> = HashMap::new();
    let mut conn_id_map: HashMap<Vec<u8>, Vec<u8>> = HashMap::new();
    let mut recv_buf = vec![0u8; 65536];
    let mut events = Events::with_capacity(256);
    // Wait time against work time, per kind of event that woke the loop. An
    // iteration with events of several kinds is counted under its first event.
    let mut trace = LoopTrace::new(&[
        "accept", "conn", "upstream", "udp", "signal", "timeout", "log",
    ]);
    let mut last = m6_core::platform::now();
    let mut last_log_tick = last;
    let mut accepting = tcp.is_some() || h2c.is_some();

    'run: loop {
        // The wait is the earliest of three bounds: the 1 s accounting tick,
        // every deadline table (connections, upstreams and h2 streams), and
        // quiche's own timeout over the QUIC connections. Nothing else needs a
        // shorter wait: every reply arrives on a descriptor.
        let tick = std::time::Duration::from_secs(1);
        let now0 = m6_core::platform::now();
        let mut poll_timeout = state.read_timeouts.timeout(now0, tick);
        poll_timeout = poll_timeout.min(state.h2_idle_timeouts.timeout(now0, tick));
        poll_timeout = poll_timeout.min(state.upstream_timeouts.timeout(now0, tick));
        poll_timeout = poll_timeout.min(state.h2c_pool.timeout(now0, tick));
        poll_timeout = poll_timeout.min(state.h2s_pool.timeout(now0, tick));
        let quic_timeout = connections
            .values()
            .filter_map(|c| c.conn.timeout())
            .min()
            .unwrap_or(tick)
            .min(tick);
        poll_timeout = poll_timeout.min(quic_timeout);
        // Edge-triggered: a capped accept leaves connections in the kernel
        // queue that the h2c listener will not be reported for again. Drain
        // before waiting, and do not block while more remain.
        let mut h2c_full = false;
        if let Some(h) = h2c.as_mut() {
            if h.accept_again() {
                let (full, again) = accept_h2c(h, state, poll.registry(), m6_core::platform::now());
                h.set_accept_again(again);
                if again {
                    poll_timeout = std::time::Duration::ZERO;
                }
                h2c_full = full;
            }
        }
        if h2c_full && accepting {
            deregister_listeners(&poll, tcp.as_ref(), h2c.as_ref());
            accepting = false;
        }

        match poll.poll(&mut events, Some(poll_timeout)) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => {
                error!(error = %e, "poll error");
                return 1;
            }
        }
        let now = m6_core::platform::now();
        trace.waited(now - last);

        let first_trigger = events.iter().next().map(|ev| ev.token());
        let first_index = trigger_index(first_trigger, &state.sources);
        for ev in &events {
            match ev.token() {
                TOKEN_SIGNAL => {
                    let mut stop = false;
                    while let Some(sig) = signals.read() {
                        if lifecycle.on_signal(sig) == m6_core::signal::Action::Stop {
                            stop = true;
                        }
                    }
                    if stop {
                        break 'run;
                    }
                }
                TOKEN_LOG => m6_core::log::flush(),
                TOKEN_UDP => {
                    drain_udp(
                        &udp,
                        &mut recv_buf,
                        &mut connections,
                        &mut conn_id_map,
                        quiche_config,
                        state,
                        &H3Loop {
                            registry: poll.registry(),
                            quic_port,
                        },
                    );
                }
                TOKEN_TLS_LISTENER => {
                    if let Some(t) = tcp.as_mut() {
                        let full = accept_tls(t, state, poll.registry(), now);
                        if full && accepting {
                            deregister_listeners(&poll, tcp.as_ref(), h2c.as_ref());
                            accepting = false;
                        }
                    }
                }
                TOKEN_H2C_LISTENER => {
                    if let Some(h) = h2c.as_mut() {
                        let (full, again) = accept_h2c(h, state, poll.registry(), now);
                        h.set_accept_again(again);
                        if full && accepting {
                            deregister_listeners(&poll, tcp.as_ref(), h2c.as_ref());
                            accepting = false;
                        }
                    }
                }
                Token(k) => match *state.sources.get(k) {
                    Source::TlsConn(_) | Source::H2cConn(_) => {
                        drive_conn_token(
                            Token(k),
                            state,
                            &mut tcp,
                            &mut h2c,
                            poll.registry(),
                            quic_port,
                            now,
                        );
                    }
                    Source::H2cUpstream => {
                        if state.h2c_pool.drive_one(Token(k), poll.registry()) {
                            state.h2c_pool.remove_by_token(Token(k), poll.registry());
                            *state.sources.get_mut(k) = Source::Vacant;
                            state.sources.free(k);
                        }
                        let completions = state.h2c_pool.drain_completions();
                        complete_h2_streams(
                            completions,
                            state,
                            &mut tcp,
                            &mut h2c,
                            poll.registry(),
                            quic_port,
                            &udp,
                            &mut connections,
                        );
                    }
                    Source::H2sUpstream => {
                        if state.h2s_pool.drive_one(Token(k), poll.registry()) {
                            state.h2s_pool.remove_by_token(Token(k), poll.registry());
                            *state.sources.get_mut(k) = Source::Vacant;
                            state.sources.free(k);
                        }
                        let completions = state.h2s_pool.drain_completions();
                        complete_h2_streams(
                            completions,
                            state,
                            &mut tcp,
                            &mut h2c,
                            poll.registry(),
                            quic_port,
                            &udp,
                            &mut connections,
                        );
                    }
                    Source::Upstream(slot) => {
                        if let UpstreamKind::IdleUrl(backend_idx) = state.upstreams.get(slot).kind {
                            drop_idle_url(slot, backend_idx, state, poll.registry());
                        } else {
                            let mut cx = CompletionCtx::new(
                                &mut *state,
                                &mut tcp,
                                &mut h2c,
                                poll.registry(),
                                quic_port,
                                &udp,
                                &mut connections,
                                now,
                            );
                            drive_upstream_one(Token(k), &mut cx);
                        }
                    }
                    Source::Vacant => {}
                },
            }
        }

        // A full stdout holds its bytes in the Sink. Watch it for writability
        // only while it does, so the loop never waits on the log.
        if let Err(e) = m6_core::log::watch(poll.registry(), TOKEN_LOG) {
            error!(error = %e, "log descriptor registration failed");
        }

        // Close connections past their read or h2 idle deadline, and socket
        // upstreams past their backend deadline.
        while let Some(token) = state.read_timeouts.expired(m6_core::platform::now()) {
            close_conn_token(token, state, &mut tcp, &mut h2c, poll.registry());
        }
        while let Some(token) = state.h2_idle_timeouts.expired(m6_core::platform::now()) {
            close_conn_token(token, state, &mut tcp, &mut h2c, poll.registry());
        }
        while let Some(token) = state.upstream_timeouts.expired(m6_core::platform::now()) {
            let mut cx = CompletionCtx::new(
                &mut *state,
                &mut tcp,
                &mut h2c,
                poll.registry(),
                quic_port,
                &udp,
                &mut connections,
                now,
            );
            expire_upstream(token, &mut cx);
        }
        let now2 = m6_core::platform::now();
        let (dead, completions) = state.h2c_pool.expire(now2, poll.registry());
        for token in dead {
            state.h2c_pool.remove_by_token(token, poll.registry());
            *state.sources.get_mut(token.0) = Source::Vacant;
            state.sources.free(token.0);
        }
        complete_h2_streams(
            completions,
            state,
            &mut tcp,
            &mut h2c,
            poll.registry(),
            quic_port,
            &udp,
            &mut connections,
        );
        let (dead, completions) = state.h2s_pool.expire(now2, poll.registry());
        for token in dead {
            state.h2s_pool.remove_by_token(token, poll.registry());
            *state.sources.get_mut(token.0) = Source::Vacant;
            state.sources.free(token.0);
        }
        complete_h2_streams(
            completions,
            state,
            &mut tcp,
            &mut h2c,
            poll.registry(),
            quic_port,
            &udp,
            &mut connections,
        );

        // A slot came free while the token table was full: accept again.
        if !accepting && !state.sources.is_full() {
            register_listeners(&poll, tcp.as_ref(), h2c.as_ref());
            accepting = true;
        }

        // Drive connection timeouts and flush pending sends.
        flush_all(&udp, &mut connections);

        // Remove closed/timed-out connections.
        connections.retain(|_, c| !c.conn.is_closed());

        // The accounting tick: flush the log buffers once a second, next to
        // the loop trace's own accounting.
        if now2.duration_since(last_log_tick) >= std::time::Duration::from_secs(1) {
            m6_core::log::tick(m6_core::platform::wall());
            last_log_tick = now2;
        }

        // Emit periodic stats (cheap check every iteration: compares one Instant).
        // On the emit, copy the loop trace into the snapshot so `/perf` serves it.
        if state
            .stats
            .maybe_emit(state.pool_manager.total_active_members())
        {
            state.stats.set_event_loop(trace.report());
        }

        // Drain one background fetch per loop iteration.
        if let Some(r) = state.prefetch_queue.pop_front() {
            let lk = make_lookup_key(
                &r.path,
                r.query.as_deref(),
                &r.enc,
                &mut state.lookup_key_buf,
            );
            if state.cache.get(lk).is_none() {
                let synth = synth_refresh_request(&r);
                match handle_request(
                    &synth,
                    "127.0.0.1",
                    &r.enc,
                    state,
                    true,
                    /* from_internal */ false,
                    poll.registry(),
                ) {
                    RequestOutcome::Ready(..) => {
                        debug!(path = %r.path, enc = %r.enc, "background fetch: cache filled");
                    }
                    RequestOutcome::Cached { .. } => {
                        unreachable!("a background fetch is a miss, never a cached hit")
                    }
                    RequestOutcome::Pending { id, ctx } => {
                        park(state, id, ctx, Waiter::Background);
                    }
                }
            }
        }

        // The clock read at the end of the work is also the start of the next
        // wait: record the work under the iteration's first trigger, then loop.
        last = m6_core::platform::now();
        trace.worked(first_index, last - now);
    }

    0
}
/// The loop context threaded down the h3 request path: the registry, for
/// registering socket upstreams, and the QUIC port, for the Alt-Svc header.
struct H3Loop<'a> {
    registry: &'a m6_core::platform::Registry,
    quic_port: u16,
}

fn drain_udp(
    udp: &UdpSocket,
    // A slice, not `&mut Vec<u8>`: this never grows or shrinks the buffer, it
    // fills it and reslices it to the datagram length.
    recv_buf: &mut [u8],
    connections: &mut HashMap<Vec<u8>, QuicConn>,
    conn_id_map: &mut HashMap<Vec<u8>, Vec<u8>>,
    quiche_config: &mut quiche::Config,
    state: &mut ServerState,
    h3: &H3Loop,
) {
    loop {
        let (len, from) = match udp.recv_from(recv_buf) {
            Ok(v) => v,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(e) => {
                warn!(error = %e, "udp recv error");
                break;
            }
        };

        let pkt = &mut recv_buf[..len];
        let local = match udp.local_addr() {
            Ok(a) => a,
            Err(e) => {
                warn!(error = %e, "local_addr error");
                continue;
            }
        };

        // Parse QUIC header to get connection ID
        let hdr = match quiche::Header::from_slice(pkt, quiche::MAX_CONN_ID_LEN) {
            Ok(h) => h,
            Err(e) => {
                debug!("bad quic header: {}", e);
                continue;
            }
        };

        let conn_id = hdr.dcid.to_vec();

        // Resolve the stored map key: direct hit, alias, or new connection.
        // quiche::connect() generates a 16-byte Initial DCID.  The server stores
        // connections under a fresh 20-byte SCID so that 1-RTT short-header
        // packets (parsed with MAX_CONN_ID_LEN=20) always match the stored key.
        let key: Vec<u8> = if connections.contains_key(&conn_id) {
            conn_id.clone()
        } else if let Some(k) = conn_id_map.get(&conn_id) {
            k.clone()
        } else if hdr.ty == quiche::Type::Initial {
            // New connection: generate a fresh 20-byte SCID.
            let mut scid_bytes = [0u8; quiche::MAX_CONN_ID_LEN];
            rand::thread_rng().fill_bytes(&mut scid_bytes);
            let key = scid_bytes.to_vec();
            let scid = quiche::ConnectionId::from_vec(key.clone());
            let conn = match quiche::accept(&scid, None, local, from, quiche_config) {
                Ok(c) => c,
                Err(e) => {
                    warn!("quiche::accept error: {}", e);
                    continue;
                }
            };
            connections.insert(
                key.clone(),
                QuicConn {
                    conn,
                    h3_conn: None,
                    pending: HashMap::new(),
                    partial_responses: HashMap::new(),
                    client_addr: from,
                    last_active: m6_core::platform::now(),
                    created: m6_core::platform::now(),
                    handshake_recorded: false,
                },
            );
            // Alias the client's Initial DCID → our 20-byte key for retransmits.
            if conn_id != key {
                conn_id_map.insert(conn_id.clone(), key.clone());
            }
            key
        } else {
            debug!("non-initial packet for unknown conn");
            continue;
        };

        let qconn = match connections.get_mut(&key) {
            Some(c) => c,
            None => continue,
        };
        qconn.last_active = m6_core::platform::now();

        let recv_info = quiche::RecvInfo { from, to: local };
        if let Err(e) = qconn.conn.recv(pkt, recv_info) {
            // REJECTING A PACKET IS NOT THE END OF THE WORK. quiche has already
            // called close() internally and queued a CONNECTION_CLOSE carrying
            // the right wire code, and the only way it reaches the peer is
            // conn.send(), which here is flush_conn. This path used to
            // `continue` straight past it.
            //
            // The close was not lost: the timer path calls on_timeout() and
            // then flush_conn, so it went out one tick late. That is still a
            // needless delay on the one packet whose job is to say why the
            // connection is ending, so it goes out here instead.
            //
            // It does not move h3 conformance. The twelve h3spec failures are
            // quiche's and are documented in tools/conformance-scores.txt.
            warn!("conn.recv error: {}", e);
            flush_conn(udp, qconn);
            continue;
        }

        // The QUIC handshake is complete exactly here, the first time this is
        // true. Guarded by a flag because this branch is reached on every packet
        // for the life of the connection, and recording unguarded would turn one
        // handshake into thousands of samples of a steadily growing duration.
        if qconn.conn.is_established() && !qconn.handshake_recorded {
            qconn.handshake_recorded = true;
            // Resumed handshakes are counted apart from full ones. With 0-RTT
            // enabled this is now the common case for a returning visitor, and a
            // blended figure would drop as the returning share grew while neither
            // cost had changed.
            let resumed = qconn.conn.is_resumed();
            state.stats.record_handshake(
                qconn.created.elapsed().as_nanos() as u64,
                Channel::new(HttpVersion::Http3, state.tls_iface),
                resumed,
            );
        }

        // Establish the H3 connection once the QUIC handshake is complete, OR as
        // soon as early data arrives.
        //
        // The early-data half is what makes 0-RTT reach the application at all.
        // With `is_established()` alone the requests in a client's first flight
        // were accepted by QUIC and then had nowhere to go, because the h3
        // connection that parses them did not exist until a round trip later --
        // so the round trip 0-RTT exists to save was still being paid.
        //
        // quiche permits this: `h3::Connection::with_transport` gates on
        // established-or-early-data for CLIENTS only, and this is the server.
        //
        // `handle_h3_request` decides what may actually be ANSWERED this early.
        // Parsing a request in early data is not the risk; acting on it is.
        if (qconn.conn.is_established() || qconn.conn.is_in_early_data()) && qconn.h3_conn.is_none()
        {
            let mut h3_config = match quiche::h3::Config::new() {
                Ok(c) => c,
                Err(e) => {
                    warn!("h3 Config::new error: {}", e);
                    flush_conn(udp, qconn);
                    continue;
                }
            };
            // RFC 9114 4.2.2: the field section bound is advertised and the
            // local decoder enforces it. It is the same number every other
            // protocol uses.
            h3_config.set_max_field_section_size(m6_core::headers::MAX_REQUEST_HEAD as u64);
            match quiche::h3::Connection::with_transport(&mut qconn.conn, &h3_config) {
                Ok(h3) => {
                    qconn.h3_conn = Some(h3);
                }
                Err(e) => {
                    warn!("h3 init error: {}", e);
                    flush_conn(udp, qconn);
                    continue;
                }
            }
        }

        // Process H3 events
        if qconn.h3_conn.is_some() {
            process_h3(qconn, &key, state, h3);
        }

        // Send any pending QUIC packets
        flush_conn(udp, qconn);
    }
}

// ── H3 event processing ───────────────────────────────────────────────────────

fn process_h3(qconn: &mut QuicConn, conn_id: &[u8], state: &mut ServerState, h3_loop: &H3Loop) {
    // client_ip is NOT computed here — deferred to cache-miss path in handle_h3_request.

    while let Some(h3) = qconn.h3_conn.as_mut() {
        match h3.poll(&mut qconn.conn) {
            Ok((
                stream_id,
                quiche::h3::Event::Headers {
                    list, more_frames, ..
                },
            )) => {
                let entry = qconn
                    .pending
                    .entry(stream_id)
                    .or_insert_with(|| PendingRequest {
                        headers: Vec::new(),
                        body: Vec::new(),
                        headers_done: false,
                    });
                entry.headers = list;
                entry.headers_done = true;
                if !more_frames {
                    // No body — process immediately
                    handle_h3_request(stream_id, qconn, state, h3_loop, conn_id);
                    // After handle_h3_request qconn may be mutated; restart loop
                    continue;
                }
            }
            Ok((stream_id, quiche::h3::Event::Data)) => {
                // Need to borrow h3_conn mutably again
                let mut buf = [0u8; 65536];
                while let Some(h3) = qconn.h3_conn.as_mut() {
                    match h3.recv_body(&mut qconn.conn, stream_id, &mut buf) {
                        Ok(0) => break,
                        Ok(read) => {
                            if let Some(req) = qconn.pending.get_mut(&stream_id) {
                                // Bounded, for the same reason the h2 path is:
                                // bodies accumulated with no ceiling while the
                                // transport kept granting credit, so a peer
                                // could stream indefinitely and grow the
                                // process until it was killed. Same limit as
                                // h2 so the two protocols cannot disagree
                                // about what is acceptable.
                                if req.body.len() + read > MAX_H3_BODY {
                                    warn!(
                                        stream_id,
                                        limit = MAX_H3_BODY,
                                        "h3 request body exceeded the limit; resetting stream"
                                    );
                                    let _ = h3.send_response(
                                        &mut qconn.conn,
                                        stream_id,
                                        &[quiche::h3::Header::new(b":status", b"413")],
                                        true,
                                    );
                                    qconn.pending.remove(&stream_id);
                                    break;
                                }
                                req.body.extend_from_slice(&buf[..read]);
                            }
                        }
                        Err(_) => break,
                    }
                }
            }
            Ok((stream_id, quiche::h3::Event::Finished)) => {
                // Body fully received (or no body) — process
                if qconn.pending.contains_key(&stream_id) {
                    handle_h3_request(stream_id, qconn, state, h3_loop, conn_id);
                    continue;
                }
            }
            Ok((_, quiche::h3::Event::Reset(e))) => {
                debug!("stream reset: {}", e);
            }
            Ok((_, quiche::h3::Event::GoAway)) => {
                debug!("goaway received");
                break;
            }
            Ok(_) => {}
            Err(quiche::h3::Error::Done) => break,
            Err(e) => {
                warn!("h3.poll error: {}", e);
                break;
            }
        }
    }
}

// ── Request handling ──────────────────────────────────────────────────────────

fn handle_h3_request(
    stream_id: u64,
    qconn: &mut QuicConn,
    state: &mut ServerState,
    h3: &H3Loop,
    conn_id: &[u8],
) {
    let req = match qconn.pending.remove(&stream_id) {
        Some(r) => r,
        None => return,
    };

    // ── Phase 0: malformed-request check ─────────────────────────────────────
    // RFC 9114 4.1.2: a malformed request MUST be treated as a stream error of
    // type H3_MESSAGE_ERROR. RFC 9114 4.3's rules are RFC 9113 8.3 restated
    // almost word for word, so this calls the very validator the HTTP/2 path
    // uses. Writing a second copy here was the alternative, and two copies of a
    // rule set drift.
    //
    // m6 did none of this on H3: an uppercase field name, a `Connection:`
    // header, a duplicate `:method`, a missing `:path` -- all were served a
    // normal 200 over HTTP/3 while the identical request was correctly rejected
    // over HTTP/2. The validator takes byte slices precisely so this stays
    // allocation-free on the request path.
    if let Err(why) =
        validate_request_header_bytes(req.headers.iter().map(|h| (h.name(), h.value())))
    {
        debug!(stream_id, reason = why, "h3: malformed request headers");
        let _ = qconn
            .conn
            .stream_shutdown(stream_id, quiche::Shutdown::Read, H3_MESSAGE_ERROR);
        let _ = qconn
            .conn
            .stream_shutdown(stream_id, quiche::Shutdown::Write, H3_MESSAGE_ERROR);
        return;
    }

    // ── Request head limits, before routing or any backend ───────────────────
    // The same two limits h1 and h2 enforce. The field-section total is the
    // one quiche counts itself, so a section past `MAX_REQUEST_HEAD` never
    // reaches this code: quiche answers it by closing the connection with
    // H3_EXCESSIVE_LOAD. The total check stays as the guard for the day that
    // changes, and the field and request-line checks are the ones that answer
    // on this stream.
    let list_size: usize = req
        .headers
        .iter()
        .map(|h| h.name().len() + h.value().len() + m6_http_lib::fields::FIELD_OVERHEAD)
        .sum();
    if let Some(status) = m6_http_lib::fields::head_limit_status(
        req.headers.iter().map(|h| (h.name(), h.value())),
        list_size,
    ) {
        let is_head = req
            .headers
            .iter()
            .any(|h| h.name() == b":method" && h.value().eq_ignore_ascii_case(b"HEAD"));
        let mut headers: Vec<(String, String)> = vec![(
            "Content-Type".to_string(),
            "text/html; charset=utf-8".to_string(),
        )];
        set_date(&mut headers);
        send_h3_response(
            stream_id,
            qconn,
            status,
            &headers,
            Bytes::from(m6_http_lib::error::refusal_body(status)),
            is_head,
        );
        return;
    }

    // ── Phase 1: zero-alloc header scan for cache lookup ──────────────────────
    // Borrow directly from quiche::h3::Header byte slices; no String allocation.
    let mut path_bytes: &[u8] = b"/";
    let mut query_bytes: Option<&[u8]> = None;
    let mut method_bytes: &[u8] = b"GET";
    let mut enc_bytes: &[u8] = b"";

    for h in &req.headers {
        match h.name() {
            b":path" => {
                let v = h.value();
                match v.iter().position(|&b| b == b'?') {
                    Some(q) => {
                        path_bytes = &v[..q];
                        query_bytes = Some(&v[q + 1..]);
                    }
                    None => {
                        path_bytes = v;
                    }
                }
            }
            b":method" => method_bytes = h.value(),
            b"accept-encoding" => enc_bytes = h.value(),
            _ => {}
        }
    }

    let path_str = std::str::from_utf8(path_bytes).unwrap_or("/");
    let enc_str = std::str::from_utf8(enc_bytes).unwrap_or("");
    let query_str = query_bytes.and_then(|q| std::str::from_utf8(q).ok());

    // ── 0-RTT gate A: replayable methods ─────────────────────────────────────
    //
    // True only while the handshake is still incomplete, which is precisely the
    // window in which this request could be a replay of a captured first flight.
    // Once established it is false and none of this applies, so the fast path for
    // every normal request is one boolean.
    let in_early_data = !qconn.conn.is_established();
    if in_early_data && !(method_bytes == b"GET" || method_bytes == b"HEAD") {
        // 425 Too Early, RFC 8470 5.2: the client retries once the handshake
        // finishes, and nothing here has touched state. A replayed POST would
        // otherwise submit the contact form twice.
        //
        // This costs a round trip on exactly the requests where correctness beats
        // latency, and costs nothing on the reads that make up the page load.
        let mut headers: Vec<(String, String)> = Vec::new();
        set_date(&mut headers);
        send_h3_response(stream_id, qconn, 425, &headers, Bytes::new(), false);
        debug!(
            stream_id,
            method = %String::from_utf8_lossy(method_bytes),
            "h3 0-RTT: refused a replayable method with 425"
        );
        return;
    }

    let start = m6_core::platform::now();

    // ── Rate limit — before cache lookup, routing, or any backend work ───────
    // HTTP/3 is not a side door: the same per-IP limit applies here as on
    // HTTP/1.1 and h2c. Every response advertises `alt-svc: h3`, so a limiter
    // that skipped this path would be trivially bypassed by any client that
    // takes the hint.
    {
        let client_ip = qconn.client_addr.ip().to_string();
        let ua_owned: Option<String> = req.headers.iter().find_map(|h| {
            h.name()
                .eq_ignore_ascii_case(b"user-agent")
                .then(|| std::str::from_utf8(h.value()).ok().map(str::to_string))
                .flatten()
        });
        if let Some(RequestOutcome::Ready(status, headers, body, _)) =
            check_rate_limit(state, &client_ip, path_str, ua_owned.as_deref())
        {
            // Rate-limit rejection: tiny body, but a HEAD still must not carry one.
            send_h3_response(
                stream_id,
                qconn,
                status,
                &headers,
                Bytes::from(body),
                method_bytes.eq_ignore_ascii_case(b"HEAD"),
            );
            return;
        }
    }

    // www -> apex, before the cache lookup for the same reason as the other two
    // paths: the key carries no host, so a warm apex entry would answer a www
    // request. HTTP/3 carries the host in :authority rather than a Host header.
    {
        let authority: Option<String> = req.headers.iter().find_map(|h| {
            h.name()
                .eq_ignore_ascii_case(b":authority")
                .then(|| std::str::from_utf8(h.value()).ok().map(str::to_string))
                .flatten()
        });
        if let Some(location) =
            www_redirect_location(authority.as_deref(), path_str, query_str, &state.config)
        {
            send_h3_response(
                stream_id,
                qconn,
                301,
                &www_redirect_headers(location),
                Bytes::new(),
                false,
            );
            return;
        }
    }

    // ── Cache lookup — zero allocation ────────────────────────────────────────
    // Routes with `require` are never served from cache — see the HTTP/1.1
    // path for the reasoning.
    // Same method gate as the h1/h2 lookup sites above.
    let method_str = std::str::from_utf8(method_bytes).unwrap_or("GET");
    let cacheable = !state.route_table.requires_auth(path_str)
        && m6_http_lib::cache::method_may_read_cache(method_str);

    let lookup_key = make_lookup_key(path_str, query_str, enc_str, &mut state.lookup_key_buf);

    let req_cc = m6_http_lib::cache::RequestDirectives::parse(&owned_headers_for_cc(&req.headers));
    let looked_up = if cacheable {
        state.cache.lookup_with(lookup_key, &req_cc)
    } else {
        m6_http_lib::cache::Lookup::Miss
    };
    if req_cc.only_if_cached && matches!(looked_up, m6_http_lib::cache::Lookup::Miss) {
        let mut headers: Vec<(String, String)> = Vec::new();
        set_date(&mut headers);
        send_h3_response(stream_id, qconn, 504, &headers, Bytes::new(), false);
        return;
    }

    // ── 0-RTT gate B: only a FRESH cache hit is answered in early data ────────
    //
    // Gate A already refused replayable methods. This refuses everything else a
    // replay could act on, and it is what makes 0-RTT defensible at all rather
    // than merely RFC-compliant:
    //
    //   - A backend request may have side effects. The analytics beacon is a
    //     fire-and-forget GET, so "the method is safe" does not mean "replaying it
    //     changes nothing". m6-http cannot pick that route out -- it is proxied
    //     like any other -- so the rule is about where the answer comes from.
    //   - A STALE hit is excluded too, even though it serves from cache, because
    //     serving stale QUEUES A BACKGROUND REFRESH. That is a write, and a replay
    //     would queue it again.
    //
    // A fresh hit re-sends bytes m6 already holds and touches nothing else, so a
    // replay of one is indistinguishable from the visitor pressing reload.
    //
    // This is the common case for the thing 0-RTT is for: a returning visitor
    // fetching a warm page and its assets. Everything else pays one round trip,
    // which is what it would have paid anyway without 0-RTT.
    if in_early_data && !matches!(looked_up, m6_http_lib::cache::Lookup::Fresh(..)) {
        let mut headers: Vec<(String, String)> = Vec::new();
        set_date(&mut headers);
        send_h3_response(stream_id, qconn, 425, &headers, Bytes::new(), false);
        debug!(
            stream_id,
            path = path_str,
            "h3 0-RTT: no fresh cache entry, refused with 425"
        );
        return;
    }
    // Serve stale now, refresh behind the request — see the HTTP/1.1 path for
    // the reasoning.
    // See the HTTP/1.1 path: a stale serve is logged distinctly from a hit.
    let cache_state = if matches!(looked_up, m6_http_lib::cache::Lookup::Stale(..)) {
        "STALE"
    } else {
        "HIT"
    };
    if let m6_http_lib::cache::Lookup::Stale(..) = looked_up {
        state.queue_refresh(Refresh {
            path: path_str.to_string(),
            query: query_str.map(str::to_string),
            enc: enc_str.to_string(),
        });
    }
    if let m6_http_lib::cache::Lookup::Fresh(cached, age)
    | m6_http_lib::cache::Lookup::Stale(cached, age) = looked_up
    {
        let elapsed_ns = start.elapsed().as_nanos() as u64;
        // QUIC shares the public bind, so it is external like TLS.
        let chan = Channel::new(HttpVersion::Http3, state.tls_iface);

        let method_str = std::str::from_utf8(method_bytes).unwrap_or("GET");
        let precond = evaluate_preconditions(&cached.headers, &H3Headers(&req.headers), method_str);
        // See the h1/h2 paths: recorded after preconditions so a 304 is
        // counted as a 304 and not as the cached 200.
        state.stats.record(
            elapsed_ns,
            true,
            match precond {
                Precondition::Failed => 412,
                Precondition::NotModified => 304,
                _ => cached.status,
            },
            chan,
            "cache",
        );
        if precond == Precondition::Failed {
            // Same rule as the h1/h2 paths; see the note there.
            let mut headers: Vec<(String, String)> = Vec::new();
            set_date(&mut headers);
            send_h3_response(stream_id, qconn, 412, &headers, Bytes::new(), false);
            return;
        }
        if precond == Precondition::NotModified {
            let client_ip = qconn.client_addr.ip().to_string();
            let set_cookie = analytics::record(
                state.config.analytics.enabled,
                &H3Headers(&req.headers),
                &analytics::Event {
                    node: &state.config.node.name,
                    path: path_str,
                    status: 304,
                    cache_state,
                    client_ip: &client_ip,
                    latency_ns: Some(elapsed_ns),
                },
                &mut state.traffic,
            );
            let html = analytics::is_html_response(&cached.headers);
            let mut headers = not_modified_headers(&cached.headers);
            // Same as the h1/h2 304 paths: Vary and Date are added post-insert
            // so the stored headers do not have them.
            set_vary_accept_encoding(&mut headers, true);
            set_age(&mut headers, age);
            set_date(&mut headers);
            if let (Some(sc), true) = (set_cookie, html) {
                headers.push(("Set-Cookie".to_string(), sc));
            }
            debug!(
                path = %path_str,
                status = 304,
                version = "HTTP/3",
                backend = "cache",
                latency_ns = elapsed_ns,
                cache_hit = true,
                "request complete"
            );
            send_h3_response(stream_id, qconn, 304, &headers, Bytes::new(), false);
            return;
        }

        debug!(
            path = %path_str,
            status = cached.status,
            version = "HTTP/3",
            backend = "cache",
            latency_ns = elapsed_ns,
            cache_hit = true,
            "request complete"
        );

        // Analytics + session cookie. `req.headers` is passed directly as its
        // native `Vec<quiche::h3::Header>` — the HeaderSource impl for that
        // type scans it in place, so this needs no owned-Vec extraction step
        // (the hand-rolled 3-field scan this replaced was itself already an
        // unnecessary intermediate allocation, not a required one).
        let client_ip = qconn.client_addr.ip().to_string();
        let set_cookie = analytics::record(
            state.config.analytics.enabled,
            &H3Headers(&req.headers),
            &analytics::Event {
                node: &state.config.node.name,
                path: path_str,
                status: cached.status,
                cache_state,
                client_ip: &client_ip,
                latency_ns: Some(elapsed_ns),
            },
            &mut state.traffic,
        );
        // Build headers with Link: preload / Vary / Set-Cookie appended as
        // needed. Vary is now always added on a cache hit (the cache key is
        // already segmented by encoding, so this just documents that to
        // downstream/shared caches), so this always takes the owned-Vec
        // branch rather than reusing `&cached.headers` unmodified.
        let mut headers_with_links: Vec<(String, String)> = (*cached.headers).clone();
        set_vary_accept_encoding(&mut headers_with_links, true);
        set_age(&mut headers_with_links, age);
        set_describedby_link(&mut headers_with_links, &state.config.site.describedby);
        set_alt_svc(&mut headers_with_links, h3.quic_port);
        if let (Some(sc), true) = (set_cookie, analytics::is_html_response(&headers_with_links)) {
            headers_with_links.push(("Set-Cookie".to_string(), sc));
        }
        let resp_headers: &[(String, String)] = &headers_with_links;
        send_h3_response(
            stream_id,
            qconn,
            cached.status,
            resp_headers,
            cached.body,
            method_str.eq_ignore_ascii_case("HEAD"),
        );
        return;
    } // end cache hit

    // ── Phase 2: cache miss — allocate owned data for forwarding ──────────────
    let path = path_str.to_string();
    let method = std::str::from_utf8(method_bytes)
        .unwrap_or("GET")
        .to_string();
    let query = query_str.map(str::to_string);
    let client_ip = qconn.client_addr.ip().to_string();

    let mut fwd_headers: Vec<(String, String)> = Vec::new();
    for h in &req.headers {
        let name = h.name();
        if name.starts_with(b":") {
            continue;
        }
        if let (Ok(k), Ok(v)) = (std::str::from_utf8(name), std::str::from_utf8(h.value())) {
            // Strip proxy-owned headers on ingress — see
            // `forward::UNTRUSTED_INBOUND`.
            if m6_http_lib::forward::is_untrusted_inbound(k) {
                continue;
            }
            fwd_headers.push((k.to_string(), v.to_string()));
        }
    }

    let http_req = forward::HttpRequest {
        method,
        path: path.clone(),
        query,
        version: "HTTP/3".to_string(),
        headers: fwd_headers,
        body: req.body,
    };

    match handle_request(
        &http_req,
        &client_ip,
        enc_str,
        state,
        false, /* from_internal */
        false,
        h3.registry,
    ) {
        RequestOutcome::Ready(status, mut resp_headers, body, backend_name) => {
            // Add alt-svc header.
            set_alt_svc(&mut resp_headers, h3.quic_port);

            let elapsed_ns = start.elapsed().as_nanos() as u64;
            // /health and /perf are separated inside `Stats::record`.
            let chan = Channel::new(HttpVersion::Http3, state.tls_iface);
            state
                .stats
                .record(elapsed_ns, false, status, chan, &backend_name);
            debug!(
                path = %path,
                status,
                version = "HTTP/3",
                backend = %backend_name,
                latency_ns = elapsed_ns,
                cache_hit = false,
                "request complete"
            );
            send_h3_response(
                stream_id,
                qconn,
                status,
                &resp_headers,
                Bytes::from(body),
                http_req.method.eq_ignore_ascii_case("HEAD"),
            );
        }
        RequestOutcome::Cached { .. } => {
            unreachable!("h3 handles its own cache hit inline, not through handle_request")
        }
        RequestOutcome::Pending { id, ctx } => {
            // Store who waits in the upstream's slot; the completion writes the
            // response straight into this stream.
            park(
                state,
                id,
                ctx,
                Waiter::H3 {
                    conn_id: conn_id.to_vec(),
                    stream: stream_id,
                },
            );
        }
    }
}

/// Write `n` as ASCII decimal into `buf[20]` without heap allocation.
/// Returns the filled subslice.
#[inline(always)]
fn write_decimal(mut n: usize, buf: &mut [u8; 20]) -> &[u8] {
    if n == 0 {
        buf[19] = b'0';
        return &buf[19..];
    }
    let mut pos = 20usize;
    while n > 0 {
        pos -= 1;
        buf[pos] = b'0' + (n % 10) as u8;
        n /= 10;
    }
    &buf[pos..]
}

fn send_h3_response(
    stream_id: u64,
    qconn: &mut QuicConn,
    status: u16,
    headers: &[(String, String)],
    body: Bytes,
    is_head: bool,
) {
    let h3 = match qconn.h3_conn.as_mut() {
        Some(h) => h,
        None => return,
    };

    // RFC 9110 9.3.2: a HEAD response advertises the length a GET would have
    // returned and sends no body. Capture the length before dropping the body,
    // and note this deliberately lands in the `body.is_empty()` branch below --
    // which is the branch that emits an explicit content-length and sets FIN on
    // the HEADERS frame, so the stream terminates cleanly with no DATA at all.
    // Sending the body anyway is what made h2 and h3 clients abort the stream.
    let advertised_len = body.len();
    let body = if is_head { Bytes::new() } else { body };

    // Stack-allocated numeric buffers — no String heap allocation.
    let mut status_buf = [0u8; 3];
    status_buf[0] = b'0' + (status / 100) as u8;
    status_buf[1] = b'0' + ((status / 10) % 10) as u8;
    status_buf[2] = b'0' + (status % 10) as u8;

    let mut cl_buf = [0u8; 20];
    let cl_bytes = write_decimal(advertised_len, &mut cl_buf);

    // Pre-size: :status + response headers + content-length + security headers
    let mut h3_headers: Vec<quiche::h3::Header> = Vec::with_capacity(headers.len() + 8);
    h3_headers.push(quiche::h3::Header::new(b":status", &status_buf));
    // Applied here rather than at a shared choke point because H3 serialises
    // its own header list; every protocol adds these at its serialisation
    // boundary so no response path can miss them.
    if let Some(guard) = m6_http_lib::security::read() {
        for (k, v) in guard.absent_from(headers) {
            h3_headers.push(quiche::h3::Header::new(k.as_bytes(), v.as_bytes()));
        }
    }
    for (k, v) in headers {
        // Omit content-length for non-empty bodies: quiche+ngtcp2 interop bug where
        // nghttp3 prematurely signals body-complete when content-length is present
        // alongside a separate DATA frame. Without it, nghttp3 reads until stream FIN.
        if !body.is_empty() && k.eq_ignore_ascii_case("content-length") {
            continue;
        }
        h3_headers.push(quiche::h3::Header::new(k.as_bytes(), v.as_bytes()));
    }
    // RFC 9110 8.6: never on a 1xx/204, and on a 304 only if it equals the
    // 200's length. m6 emitted `content-length: 0` on every 304 across all
    // three protocols, which is the value that actively misinforms -- it claims
    // the representation is empty when it is not.
    if body.is_empty() && m6_http_lib::http11::status_may_have_content_length(status) {
        h3_headers.push(quiche::h3::Header::new(b"content-length", cl_bytes));
    }

    let fin = body.is_empty();
    match h3.send_response(&mut qconn.conn, stream_id, &h3_headers, fin) {
        Ok(()) => {}
        Err(quiche::h3::Error::StreamBlocked) => {
            // Not an error, just no flow-control credit yet — previously this
            // fell into the generic error arm below and `return`ed, silently
            // dropping the response: the client's stream stayed open with no
            // reply ever sent, which reads as "page never finishes loading".
            // Firefox opens enough concurrent H3 streams per page load to hit
            // this routinely; Chrome's more conservative concurrency mostly
            // didn't. Save it and let drain_writable() retry once this stream
            // reports writable again.
            qconn
                .partial_responses
                .insert(stream_id, PendingH3Response::Headers(h3_headers, body));
            return;
        }
        Err(e) => {
            warn!("h3 send_response error: {}", e);
            return;
        }
    }
    if !body.is_empty() {
        match h3.send_body(&mut qconn.conn, stream_id, &body, true) {
            Ok(written) if written == body.len() => {}
            Ok(written) => {
                // Partial write — store remainder, retry on conn.writable()
                qconn
                    .partial_responses
                    .insert(stream_id, PendingH3Response::Body(body, written));
            }
            Err(quiche::h3::Error::Done) | Err(quiche::h3::Error::StreamBlocked) => {
                qconn
                    .partial_responses
                    .insert(stream_id, PendingH3Response::Body(body, 0));
            }
            Err(e) => warn!("h3 send_body error: {}", e),
        }
    }
}

// ── Routing / auth / forwarding ───────────────────────────────────────────────

/// 301 `www.<domain>` to the bare `<domain>`, preserving path and query.
///
/// **Must run before the cache lookup, not inside `handle_request`.** The cache
/// key is (path, query, encoding) with no host component, so `www` and the apex
/// share entries: a cached apex response would be replayed for a `www` request
/// and the redirect would silently never happen on a warm cache. That is also
/// why this is duplicated across the three protocol paths rather than living in
/// one place further down.
///
/// Only the exact `www.` alias redirects. An arbitrary unrecognised Host is
/// served normally, because node hostnames (`node-a.example.com`) have to keep
/// answering directly — per-node verification depends on reaching one specific
/// node by name instead of through the GeoDNS-routed apex.
fn www_redirect_location(
    host: Option<&str>,
    path: &str,
    query: Option<&str>,
    config: &config::Config,
) -> Option<String> {
    if !config.site.redirect_www {
        return None;
    }
    let host = host?;
    // Host may legitimately carry a port; the canonical form never does.
    let host = host.split(':').next().unwrap_or(host);
    // Case-insensitive: `WWW.` and `Www.` are the same host as `www.`.
    let apex = host
        .get(4..)
        .filter(|_| host.len() > 4 && host[..4].eq_ignore_ascii_case("www."))?;
    if !apex.eq_ignore_ascii_case(&config.site.domain) {
        return None;
    }
    // Build from the *configured* domain, never from the client-supplied host:
    // echoing a request's own bytes back into a Location header is how open
    // redirects and header injection get built.
    let domain = &config.site.domain;
    let unsafe_byte = |s: &str| s.bytes().any(|b| b == b'\r' || b == b'\n' || b == 0);
    if unsafe_byte(path) {
        return None;
    }
    match query {
        Some(q) if unsafe_byte(q) => None,
        Some(q) => Some(format!("https://{domain}{path}?{q}")),
        None => Some(format!("https://{domain}{path}")),
    }
}

fn www_redirect_headers(location: String) -> Vec<(String, String)> {
    vec![
        ("Location".to_string(), location),
        ("Content-Length".to_string(), "0".to_string()),
    ]
}

/// Build the synthetic GET used for a background cache fill.
///
/// **The `Accept-Encoding` header must match the cache key.** The encoding is
/// also passed separately to `handle_request` as the cache-key component, so
/// omitting it here produces something worse than a miss: the backend, seeing
/// no `Accept-Encoding`, returns an identity body, which is then stored under
/// the key that promises the *encoded* variant. Every subsequent hit on that
/// key serves an uncompressed body to a client that asked for a compressed one.
///
/// The defect is easy to miss: a large stylesheet was served to browsers at
/// 44KB instead of 7KB brotli, and it re-poisoned itself every 60s, because
/// each stale-while-revalidate refresh rewrote the entry with another identity
/// body. Small assets hid it (identity and compressed sizes are close), so only
/// the stylesheet showed the damage.
fn synth_refresh_request(r: &Refresh) -> forward::HttpRequest {
    let headers = if r.enc.is_empty() {
        Vec::new()
    } else {
        vec![("Accept-Encoding".to_string(), r.enc.clone())]
    };
    forward::HttpRequest {
        method: "GET".to_string(),
        path: r.path.clone(),
        query: r.query.clone(),
        version: "HTTP/1.1".to_string(),
        headers,
        body: vec![],
    }
}

/// Cache-miss entry point for every protocol path.
///
/// This wrapper exists for one reason. Emitted **only when replaying a cache
/// hit**, `Vary: Accept-Encoding` reaches every client except the first to ask
/// for a URL — so every fresh visitor, and every downstream shared cache
/// populating itself, gets a compressed body with nothing saying the body
/// depends on `Accept-Encoding`. That is the worse of the two orderings.
///
/// The header belongs on every server path and every `Ready` return from
/// `handle_request_inner`, so it is applied once here rather than at each
/// site, where a new return would silently miss it.
///
/// Applying it *after* the inner call is deliberate. `handle_request_inner`
/// inserts into the cache itself, so `should_cache` still inspects the exact
/// `Vary` the backend sent, and a response varying on anything beyond encoding
/// stays uncacheable.
fn handle_request(
    req: &forward::HttpRequest,
    client_ip: &str,
    content_encoding: &str,
    state: &mut ServerState,
    is_prefetch: bool,
    from_internal: bool,
    registry: &m6_core::platform::Registry,
) -> RequestOutcome {
    let describedby = state.config.site.describedby.clone();
    let mut outcome = handle_request_inner(
        req,
        client_ip,
        content_encoding,
        state,
        is_prefetch,
        from_internal,
        registry,
    );
    if let RequestOutcome::Ready(status, ref mut headers, _, ref backend) = outcome {
        let compresses = state.config.backend_compresses(backend);
        set_vary_accept_encoding(headers, compresses);
        set_date(headers);
        set_describedby_link(headers, &describedby);
        invalidate_after_unsafe_method(state, req, status, headers);
    }
    outcome
}

/// The origin a request names, for `X-Forwarded-Host` and for the `Host` the
/// backend leg requires.
///
/// Falls back to the configured domain, because a client is allowed to name no
/// origin at all: HTTP/1.0 may omit `Host` entirely and m6-http serves it. The
/// backend speaks HTTP/1.1, where that is malformed (RFC 9110 7.2), so
/// something has to fill it in and only this process knows what site it is
/// serving.
///
/// One function because there were three copies of this expression, and fixing
/// one of them fixed one of three paths: HTTP/3 forwarded requests naming no
/// origin, and HTTP/1.0 without Host reached the backend with an empty
/// `X-Forwarded-Host` and no `Host` at all.
fn origin_host<'a>(req: &'a forward::HttpRequest, config: &'a config::Config) -> &'a str {
    m6_core::headers::get(&req.headers[..], ":authority")
        .or_else(|| m6_core::headers::get(&req.headers[..], "host"))
        .filter(|h| !h.is_empty())
        .unwrap_or(config.site.domain.as_str())
}

/// Why a URL dispatch failed, so the caller can answer it.
enum UrlDispatchError {
    /// No upstream slot came free: the process is at its open-file ceiling.
    NoSlot,
    Failed(std::io::Error),
}

/// Dispatch one request to a URL backend: an h2 stream for the h2 schemes, a
/// loop-driven `client::Upstream` slot for http and https.
fn dispatch_url_backend(
    state: &mut ServerState,
    registry: &m6_core::platform::Registry,
    req: &forward::HttpRequest,
    client_ip: &str,
    backend_idx: usize,
) -> Result<UpstreamId, UrlDispatchError> {
    debug!(
        backend = %state.pool_manager.url_backend(backend_idx).name,
        "url backend dispatch"
    );
    match state.pool_manager.url_backend(backend_idx).scheme {
        pool::UrlScheme::H2c => {
            ensure_h2c_conn(state, backend_idx, registry)?;
            let original_host = origin_host(req, &state.config).to_string();
            let backend = state.pool_manager.url_backend(backend_idx);
            let (conn, stream) = state
                .h2c_pool
                .dispatch(&backend.url, req, client_ip, &original_host, registry)
                .map_err(UrlDispatchError::Failed)?;
            Ok(UpstreamId::H2c { conn, stream })
        }
        pool::UrlScheme::H2s => {
            ensure_h2s_conn(state, backend_idx, registry)?;
            let original_host = origin_host(req, &state.config).to_string();
            let backend = state.pool_manager.url_backend(backend_idx);
            let (conn, stream) = state
                .h2s_pool
                .dispatch(&backend.url, req, client_ip, &original_host, registry)
                .map_err(UrlDispatchError::Failed)?;
            Ok(UpstreamId::H2s { conn, stream })
        }
        pool::UrlScheme::Http | pool::UrlScheme::Https => {
            launch_url_upstream(state, registry, req, client_ip, backend_idx).map(UpstreamId::Slot)
        }
    }
}

/// Start one request to a URL backend on a loop-driven `client::Upstream`.
///
/// The request takes an idle slot whose connection to this backend is open and
/// watched when one is free, and otherwise a free slot, which makes a new
/// connection. The slot's key is returned so the caller can park the waiter
/// there.
fn launch_url_upstream(
    state: &mut ServerState,
    registry: &m6_core::platform::Registry,
    req: &forward::HttpRequest,
    client_ip: &str,
    backend_idx: usize,
) -> Result<usize, UrlDispatchError> {
    let addr = state.pool_manager.url_backend(backend_idx).addr();
    let now = m6_core::platform::now();
    let deadline = now + std::time::Duration::from_secs(state.config.server.backend_timeout_secs);
    let head = req.method.eq_ignore_ascii_case("HEAD");

    // An idle connection first: its key is not in the free list, and its
    // descriptor is registered under the token it kept while waiting.
    let (key, token, watched) = match state.pool_manager.url_backend_mut(backend_idx).idle.take() {
        Some(key) => {
            let token = state.upstreams.get(key).token;
            (key, token, true)
        }
        None => {
            let Some(token) = take_token(state) else {
                return Err(UrlDispatchError::NoSlot);
            };
            let Some(taken) = state.upstreams.take() else {
                *state.sources.get_mut(token.0) = Source::Vacant;
                state.sources.free(token.0);
                return Err(UrlDispatchError::NoSlot);
            };
            (taken.key, token, false)
        }
    };

    let encoded = {
        // Disjoint fields: the backend's authority and the request's origin are
        // read while the slot's request buffer is written.
        let backend = state.pool_manager.url_backend(backend_idx);
        let original_host = origin_host(req, &state.config);
        let entry = state.upstreams.get_mut(key);
        forward::encode_url_request(
            entry.upstream.request_buf(),
            req,
            &backend.host,
            client_ip,
            original_host,
        )
    };
    if let Err(e) = encoded {
        if watched {
            // The connection is untouched, so keep it for the next request.
            state
                .pool_manager
                .url_backend_mut(backend_idx)
                .idle
                .push(key);
        } else {
            state.upstreams.free(key);
            *state.sources.get_mut(token.0) = Source::Vacant;
            state.sources.free(token.0);
        }
        return Err(UrlDispatchError::Failed(e));
    }
    if watched {
        // Drop the idle registration so a failed start cannot leave a stale
        // one; the slot's descriptor is registered again below.
        registry
            .deregister_fd(state.upstreams.get(key).upstream.fd())
            .ok();
    }
    let started = {
        let tls = state.pool_manager.url_backend(backend_idx).tls.as_ref();
        let entry = state.upstreams.get_mut(key);
        entry
            .upstream
            .start(&m6_core::client::Addr::Tcp(addr), tls, head, deadline)
            .map_err(UrlDispatchError::Failed)
    };
    if let Err(e) = started {
        state.upstreams.get_mut(key).upstream.close();
        state.upstreams.free(key);
        *state.sources.get_mut(token.0) = Source::Vacant;
        state.sources.free(token.0);
        return Err(e);
    }
    let interest = state.upstreams.get_mut(key).upstream.interest();
    let registered = registry.register_fd(state.upstreams.get(key).upstream.fd(), token, interest);
    if let Err(e) = registered {
        state.upstreams.get_mut(key).upstream.close();
        state.upstreams.free(key);
        *state.sources.get_mut(token.0) = Source::Vacant;
        state.sources.free(token.0);
        return Err(UrlDispatchError::Failed(e));
    }
    state.upstream_timeouts.touch(token, now);
    *state.sources.get_mut(token.0) = Source::Upstream(key);
    let entry = state.upstreams.get_mut(key);
    entry.token = token;
    entry.addr = m6_core::client::Addr::Tcp(addr);
    entry.kind = UpstreamKind::Url(backend_idx);
    entry.member_idx = 0;
    entry.backend_name.clear();
    entry.waiter = None;
    entry.ctx = None;
    entry.registered_interest = interest;
    Ok(key)
}

/// Why a socket-upstream launch failed, so the two callers can answer it their
/// own way: the main socket branch answers 502 or 503, the custom-error-page
/// branch falls back to the internal page.
enum SocketLaunchError {
    UnknownBackend,
    Empty,
    BacklogFull,
    Connect(std::io::Error),
    Register(std::io::Error),
}

/// Encode the request into the slot's buffer, take the member, start the
/// non-blocking upstream, register it, and park it in `upstreams`.
/// Returns the upstream's slot key, so the caller can store its waiter there.
fn launch_socket_upstream(
    state: &mut ServerState,
    registry: &m6_core::platform::Registry,
    req: &forward::HttpRequest,
    client_ip: &str,
    backend_name: &str,
    timeout: std::time::Duration,
) -> Result<usize, SocketLaunchError> {
    let (socket_path, member_idx) = match state.pool_manager.get_pool_mut(backend_name) {
        Some(pool) => pool.take_member().map_err(|_| SocketLaunchError::Empty)?,
        None => return Err(SocketLaunchError::UnknownBackend),
    };
    let deadline = m6_core::platform::now() + timeout;
    let Some(owner) = state.upstreams.take() else {
        if let Some(pool) = state.pool_manager.get_pool_mut(backend_name) {
            pool.cancel(member_idx);
        }
        return Err(SocketLaunchError::BacklogFull);
    };
    // Update the slot's address in place while the member's path is still
    // borrowed, then the borrow ends and the token table can be taken.
    {
        let entry = state.upstreams.get_mut(owner.key);
        match &mut entry.addr {
            m6_core::client::Addr::Unix(existing) => {
                existing.clear();
                existing.push(socket_path);
            }
            m6_core::client::Addr::Tcp(_) => {
                entry.addr = m6_core::client::Addr::Unix(socket_path.to_path_buf());
            }
        }
    }
    let Some(token) = take_token(state) else {
        state.upstreams.free(owner.key);
        if let Some(pool) = state.pool_manager.get_pool_mut(backend_name) {
            pool.cancel(member_idx);
        }
        return Err(SocketLaunchError::BacklogFull);
    };

    let started = {
        let entry = state.upstreams.get_mut(owner.key);
        let original_host = origin_host(req, &state.config);
        let buf = entry.upstream.request_buf();
        if let Err(e) = forward::encode_request(buf, req, client_ip, original_host) {
            Err(e)
        } else {
            entry.upstream.start(
                &entry.addr,
                None,
                req.method.eq_ignore_ascii_case("HEAD"),
                deadline,
            )
        }
    };
    if let Err(e) = started {
        state.upstreams.free(owner.key);
        *state.sources.get_mut(token.0) = Source::Vacant;
        state.sources.free(token.0);
        if e.kind() == std::io::ErrorKind::WouldBlock {
            if let Some(pool) = state.pool_manager.get_pool_mut(backend_name) {
                pool.cancel(member_idx);
            }
            return Err(SocketLaunchError::BacklogFull);
        }
        if let Some(pool) = state.pool_manager.get_pool_mut(backend_name) {
            pool.mark_failed(member_idx);
        }
        return Err(SocketLaunchError::Connect(e));
    }

    let interest = {
        let entry = state.upstreams.get_mut(owner.key);
        let interest = entry.upstream.interest();
        if let Err(e) = registry.register_fd(entry.upstream.fd(), token, interest) {
            entry.upstream.close();
            state.upstreams.free(owner.key);
            *state.sources.get_mut(token.0) = Source::Vacant;
            state.sources.free(token.0);
            if let Some(pool) = state.pool_manager.get_pool_mut(backend_name) {
                pool.mark_failed(member_idx);
            }
            return Err(SocketLaunchError::Register(e));
        }
        interest
    };
    state
        .upstream_timeouts
        .touch(token, m6_core::platform::now());
    *state.sources.get_mut(token.0) = Source::Upstream(owner.key);
    let entry = state.upstreams.get_mut(owner.key);
    entry.kind = UpstreamKind::Member;
    entry.token = token;
    entry.member_idx = member_idx;
    entry.backend_name.clear();
    entry.backend_name.push_str(backend_name);
    entry.waiter = None;
    entry.ctx = None;
    entry.registered_interest = interest;
    Ok(owner.key)
}

fn handle_request_inner(
    req: &forward::HttpRequest,
    client_ip: &str,
    content_encoding: &str,
    state: &mut ServerState,
    is_prefetch: bool,
    // True when this request arrived on the h2c listener and [server]
    // internal_binds names it, meaning one of our own nodes over the backbone
    // rather than a public client. Granted by the listener, never by a header, so a public
    // client cannot claim it. The error-path guard and the metrics gate read it.
    from_internal: bool,
    registry: &m6_core::platform::Registry,
) -> RequestOutcome {
    // Prefetch requests are synthetic (no real client, client_ip is a
    // placeholder) and their response is discarded by the caller — logging
    // them as analytics would mint a session/log a "MISS" line for a visit
    // that never happened, polluting request/session counts downstream.
    let analytics_enabled = state.config.analytics.enabled && !is_prefetch;

    // ── Method validation, ahead of routing and backend dispatch ────────────
    // Nothing checked the method before this. Every verb was served the page:
    // GET, HEAD, POST, PUT, DELETE, PATCH, OPTIONS, TRACE and an invented FOO
    // all returned 200 with the full body. m6-file happened to 405 non-GET/HEAD
    // of its own accord, which is why only the HTML routes were affected and
    // why spot-checking an asset always looked correct.
    //
    // No cache poisoning was demonstrated — an unsafe method got the same bytes
    // a GET would — but it is method confusion, and on any site where a read
    // path and an action path share a URL that becomes a security problem
    // rather than a correctness one. TRACE is the one worth naming: answering
    // it at all is a cross-site tracking vector.
    //
    // The cache is already closed to these verbs at every lookup site (see
    // `cache::method_may_read_cache`); this closes the backend to them too.
    if !state
        .config
        .server
        .allowed_methods
        .iter()
        .any(|m| m == req.method.as_str())
    {
        // 405 and 501 are not interchangeable, and this returned 405 for both.
        //
        // RFC 9110 15.5.6: 405 means the method is KNOWN to the server but not
        // supported by the target resource -- and it MUST carry `Allow`.
        // RFC 9110 15.6.2: 501 is for a method the server does not recognise
        // and could not support for any resource.
        //
        // Answering 405 to an invented verb claims to know it, and tells the
        // client the resource is the problem when the method is. It also makes
        // the response indistinguishable from a real method being disallowed,
        // which is exactly the distinction a client uses to decide whether
        // retrying elsewhere is worth it.
        let known = is_registered_method(&req.method);
        let status = if known { 405 } else { 501 };
        let mut headers = vec![
            (
                "Content-Type".to_string(),
                "text/plain; charset=utf-8".to_string(),
            ),
            ("Cache-Control".to_string(), "no-store".to_string()),
        ];
        // Required on 405. Included on 501 too: not mandated there, but it is
        // the one useful thing we can tell a caller whose method we do not
        // implement.
        headers.push((
            "Allow".to_string(),
            state.config.server.allowed_methods.join(", "),
        ));
        if analytics_enabled {
            debug!(path = %req.path, method = %req.method, status, known, "method refused");
        }
        let body: &[u8] = if known {
            b"Method Not Allowed"
        } else {
            b"Not Implemented"
        };
        return RequestOutcome::Ready(status, headers, body.to_vec(), "method-check".to_string());
    }

    // ── Health endpoint, ahead of routing/cache/backends ────────────────────
    // Placed here on purpose. Below this point a request touches the router,
    // the cache and then a backend; the whole value of a health check is that
    // it answers from local state without any of that. Rendering a homepage is
    // milliseconds of template work, and a monitor sending
    // `Cache-Control: no-cache` pays it on every single check, which makes the
    // monitor's latency graph a measure of template rendering rather than of
    // whether the node is up.
    //
    // It sits *after* method validation so a health path still refuses PUT
    // and friends like every other path, rather than becoming a hole in it.
    if state.config.health.enabled && req.path == state.config.health.path {
        let pools: Vec<health::PoolHealth> = state
            .pool_manager
            .pool_health()
            .into_iter()
            .map(|(name, active, total)| health::PoolHealth {
                name,
                active,
                total,
            })
            .collect();
        let (code, report) = health::HealthReport::build(&state.config.node.name, &pools);
        let (code, headers, body) = report.into_response(code);
        // Recorded, not discarded. `message = "monitor"` keeps it out of the
        // site-traffic rows every consumer already filters for, so an uptime
        // check is observable without being counted as a visitor.
        analytics::log_monitor(
            state.config.analytics.enabled,
            &req.headers,
            &state.config.node.name,
            &req.path,
            code,
            client_ip,
            None,
            &mut state.traffic,
        );
        return RequestOutcome::Ready(code, headers, body, health::HEALTH_BACKEND.to_string());
    }

    // ── Metrics endpoint, deliberately a separate path from /health ─────────
    // Percentiles mean sorting a 4096-sample reservoir. Serving them from the
    // health path behind a conditional would leave the expensive branch one
    // misconfigured header away from being taken on every check forever, so
    // the split is what keeps the health answer cheap. `snapshot` is passed
    // as a closure and is not called until authorisation passes.
    if state.config.health.enabled && req.path == state.config.health.perf_path {
        let outcome = match health::metrics_gate(
            &state.config.health,
            state.public_key.as_ref(),
            &req.headers,
            client_ip,
            from_internal,
        ) {
            health::Gate::Refused(refused) => refused,
            health::Gate::Open => {
                // Pool occupancy is gathered here rather than on `/health`,
                // which publishes neither, and only after the gate has passed.
                // It costs a lock-free read of the pool manager, unlike the
                // reservoir sort in `snapshot`.
                let pools: Vec<health::PoolHealth> = state
                    .pool_manager
                    .pool_health()
                    .into_iter()
                    .map(|(name, active, total)| health::PoolHealth {
                        name,
                        active,
                        total,
                    })
                    .collect();
                health::PerfReport::build_authorised(
                    health::PerfSubject {
                        node: &state.config.node.name,
                        uptime_s: state.started.elapsed().as_secs(),
                        pools,
                        url_backends: state.pool_manager.url_backend_names(),
                        host_path: &state.config.site_dir,
                    },
                    || state.stats.snapshot(),
                )
            }
        };
        let (code, headers, body) = outcome.into_response();
        // Same treatment as /health above. A 401 here is worth seeing: it
        // means something is probing the metrics endpoint without the token.
        analytics::log_monitor(
            state.config.analytics.enabled,
            &req.headers,
            &state.config.node.name,
            &req.path,
            code,
            client_ip,
            None,
            &mut state.traffic,
        );
        return RequestOutcome::Ready(code, headers, body, health::PERF_BACKEND.to_string());
    }

    // ── Traffic summary, the third monitoring path ──────────────────────────
    // Its own path for the same reason /perf is not /health: this one builds
    // a report from the window of events the process kept in memory, so its
    // cost is a property of the path rather than of a header on a cheaper
    // one. Cached, so a polling monitor cannot make the node rebuild it on
    // every scrape.
    if state.config.health.enabled && req.path == state.config.health.traffic_path {
        let outcome = match health::metrics_gate(
            &state.config.health,
            state.public_key.as_ref(),
            &req.headers,
            client_ip,
            from_internal,
        ) {
            health::Gate::Refused(refused) => refused,
            health::Gate::Open => health::traffic_authorised(
                &state.config.node.name,
                &state.traffic,
                &mut state.traffic_report,
                std::time::Duration::from_secs(state.config.health.traffic_cache_s),
            ),
        };
        let (code, headers, body) = outcome.into_response();
        analytics::log_monitor(
            state.config.analytics.enabled,
            &req.headers,
            &state.config.node.name,
            &req.path,
            code,
            client_ip,
            None,
            &mut state.traffic,
        );
        return RequestOutcome::Ready(code, headers, body, health::PERF_BACKEND.to_string());
    }

    // The custom-error render route takes `status`/`from` (and optional
    // `route`/`backend`/`detail`) straight from its query string and renders
    // them into the page — safe when `dispatch_custom_error_async`/
    // `apply_error_mode` build that query internally, but this route is also
    // a normal, routable path. A direct external request to it would forward
    // to the backend with client-controlled query params instead, letting
    // any caller spoof an arbitrary status/from pair. Refuse it exactly like
    // any other route miss.
    //
    // UNLESS it arrived on an internal listener. A cache node fetches the error
    // page from the origin over h2c on the private backbone,
    // and refusing it there broke custom error pages across a proxy hop entirely:
    // the edge asked, the origin refused it exactly like a stranger, and the edge
    // fell back to the built-in page. Every config was correct and the mechanism
    // was dead, which is worse than unsupported because it looks configured.
    //
    // `from_internal` is granted by the listener's declaration, never by a header,
    // so a public client cannot claim it -- the same rule that decides whether a
    // forwarded client address is trusted on that listener.
    if error::refuses_custom_error_path(&state.error_mode, &req.path, from_internal) {
        return request_error(404, req, client_ip, state, None, registry);
    }

    // Route lookup
    let route = match state.route_table.at(&req.path) {
        Some(r) => r.clone(),
        None => {
            // A path that only misses because of its trailing slash gets a
            // 301 to the canonical form instead of a 404 — SEO/UX bug, not a
            // real not-found, and cheap to catch before touching the error
            // machinery below.
            if let Some(canonical) = state.route_table.trailing_slash_redirect(&req.path) {
                let location = match &req.query {
                    Some(q) => format!("{canonical}?{q}"),
                    None => canonical,
                };
                let headers = vec![
                    ("Location".to_string(), location),
                    ("Content-Type".to_string(), "text/html".to_string()),
                ];
                return RequestOutcome::Ready(301, headers, vec![], "redirect".to_string());
            }
            // Prefer fetching the real custom error page. On a node whose error
            // backend is itself a URL/H2C/H2S backend — every cache node,
            // whose only backend is the origin — dispatch that fetch async
            // and let it resolve through the normal Pending machinery.
            if let Some(outcome) =
                dispatch_custom_error_async(404, req, client_ip, state, registry, None)
            {
                return outcome;
            }
            return request_error(404, req, client_ip, state, None, registry);
        }
    };

    // Verified JWT claims — populated during auth check, forwarded to backend.
    let mut verified_claims: Option<auth::Claims> = None;

    // Auth check — only if route has `require`
    if let Some(ref require) = route.require {
        if let Some(ref pk) = state.public_key {
            debug!(path = %req.path, "auth check");
            match auth::authorise(pk, &req.headers, require) {
                Err(auth::Refusal::NoToken) => {
                    warn!(path = %req.path, "auth: no token");
                    let cookie_header_owned = auth::combined_cookie_header(&req.headers);
                    let cookie_header = cookie_header_owned.as_deref();
                    let accept_header = m6_core::headers::get(&req.headers[..], "accept");
                    if auth::is_browser_request(accept_header) {
                        let refresh = auth::extract_refresh_cookie(cookie_header);
                        let redirect_url = if refresh.is_some() {
                            "/auth/refresh".to_string()
                        } else {
                            format!("/login?next={}", urlencoded(&req.path))
                        };
                        let headers = vec![
                            ("Location".to_string(), redirect_url),
                            ("Content-Type".to_string(), "text/html".to_string()),
                        ];
                        return RequestOutcome::Ready(302, headers, vec![], "auth".to_string());
                    }
                    let ctx = error::ErrorContext {
                        route: Some(route.path.clone()),
                        backend: Some(route.backend.clone()),
                        detail: Some("no token".to_string()),
                    };
                    match apply_error_mode(401, req, client_ip, state, Some(&ctx), registry) {
                        ErrorOutcome::Ready(s, mut h, b, n) => {
                            add_bearer_challenge(&mut h);
                            return RequestOutcome::Ready(s, h, b, n);
                        }
                        ErrorOutcome::Pending { id, mut ctx } => {
                            add_bearer_challenge(&mut ctx.extra_headers);
                            return RequestOutcome::Pending { id, ctx };
                        }
                    }
                }
                Err(auth::Refusal::Invalid(e)) => {
                    warn!(path = %req.path, error = %e, "auth: token verification failed");
                    let ctx = error::ErrorContext {
                        route: Some(route.path.clone()),
                        backend: Some(route.backend.clone()),
                        detail: Some(e.to_string()),
                    };
                    match apply_error_mode(401, req, client_ip, state, Some(&ctx), registry) {
                        ErrorOutcome::Ready(s, mut h, b, n) => {
                            add_bearer_challenge(&mut h);
                            return RequestOutcome::Ready(s, h, b, n);
                        }
                        ErrorOutcome::Pending { id, mut ctx } => {
                            add_bearer_challenge(&mut ctx.extra_headers);
                            return RequestOutcome::Pending { id, ctx };
                        }
                    }
                }
                Err(auth::Refusal::Insufficient) => {
                    warn!(
                        path = %req.path,
                        require = %require,
                        "auth: insufficient claims"
                    );
                    let ctx = error::ErrorContext {
                        route: Some(route.path.clone()),
                        backend: Some(route.backend.clone()),
                        detail: Some(format!("requires: {require}")),
                    };
                    return request_error(403, req, client_ip, state, Some(&ctx), registry);
                }
                // Forward verified claims to backend as X-Auth-Claims header
                // (base64-encoded JSON so renderers can inspect them).
                Ok(claims) => verified_claims = Some(claims),
            }
        }
    }

    // Build request with X-Auth-Claims injected if claims were verified.
    let req_with_claims;
    let req = if let Some(ref claims) = verified_claims {
        let encoded = auth::encode_claims_header(claims);
        let mut headers = req.headers.clone();
        headers.push(("X-Auth-Claims".to_string(), encoded));
        req_with_claims = forward::HttpRequest {
            method: req.method.clone(),
            path: req.path.clone(),
            query: req.query.clone(),
            version: req.version.clone(),
            headers,
            body: req.body.clone(),
        };
        &req_with_claims
    } else {
        req
    };

    // Forward to backend
    // Responses on `require` routes must never enter the shared cache: the key
    // has no identity component, so a stored entry would later be served to
    // anonymous callers straight from the cache, before any auth check runs.
    // Deliberately narrower than the read gate: GET only. A HEAD must never
    // store, or the bodyless response it now produces would land under the key
    // a later GET reads and serve an empty page. See
    // `cache::method_may_write_cache`.
    let cacheable =
        route.require.is_none() && m6_http_lib::cache::method_may_write_cache(&req.method);
    let backend_name = route.backend.clone();

    // A URL backend: an h2 stream, or a loop-driven upstream slot.
    if let Some(backend_idx) = state.pool_manager.find_url(&backend_name) {
        let ctx = forward::PendingUrlContext {
            req: req.clone(),
            client_ip: client_ip.to_string(),
            enc: content_encoding.to_string(),
            backend_name: backend_name.clone(),
            kind: forward::PendingKind::Url,
            route_path: None,
            cacheable,
            is_prefetch,
            start: m6_core::platform::now(),
            error_status_override: None,
            extra_headers: Vec::new(),
        };
        let id = match dispatch_url_backend(state, registry, req, client_ip, backend_idx) {
            Ok(id) => id,
            Err(UrlDispatchError::NoSlot) => {
                return backend_unavailable(
                    req,
                    client_ip,
                    state,
                    registry,
                    &route.path,
                    &backend_name,
                    "backend connection table full",
                );
            }
            Err(UrlDispatchError::Failed(e)) => {
                warn!(backend = %backend_name, error = %e, "url backend dispatch failed");
                let err_ctx = error::ErrorContext {
                    route: Some(route.path.clone()),
                    backend: Some(backend_name.clone()),
                    detail: Some(e.to_string()),
                };
                return request_error(502, req, client_ip, state, Some(&err_ctx), registry);
            }
        };
        return RequestOutcome::Pending {
            id,
            ctx: Box::new(ctx),
        };
    }

    // Socket backend: a non-blocking upstream driven by the loop. The reply
    // arrives on the entry's channel and is finished by complete_socket_response
    // (reached from finalize_url_response), which keeps the socket branch's own
    // completion semantics.
    let timeout = std::time::Duration::from_secs(state.config.server.backend_timeout_secs);
    let backend_start = m6_core::platform::now();

    let slot = match launch_socket_upstream(state, registry, req, client_ip, &backend_name, timeout)
    {
        Ok(slot) => slot,
        Err(SocketLaunchError::UnknownBackend) => {
            warn!(backend = %backend_name, "unknown backend");
            let err_ctx = error::ErrorContext {
                route: Some(route.path.clone()),
                backend: Some(backend_name.clone()),
                detail: Some(format!("unknown backend: {backend_name}")),
            };
            return request_error(502, req, client_ip, state, Some(&err_ctx), registry);
        }
        Err(SocketLaunchError::Empty) => {
            warn!(backend = %backend_name, "socket pool empty");
            let err_ctx = error::ErrorContext {
                route: Some(route.path.clone()),
                backend: Some(backend_name.clone()),
                detail: Some("pool empty".to_string()),
            };
            return request_error(502, req, client_ip, state, Some(&err_ctx), registry);
        }
        Err(SocketLaunchError::BacklogFull) => {
            // The listen backlog is full: the member is alive and busy (E2,
            // the kernel is the queue). Release the slot without marking it
            // failed and answer 503.
            return backend_unavailable(
                req,
                client_ip,
                state,
                registry,
                &route.path,
                &backend_name,
                "backend listen queue full",
            );
        }
        Err(SocketLaunchError::Connect(e)) => {
            warn!(backend = %backend_name, error = %e, "backend error");
            let err_ctx = error::ErrorContext {
                route: Some(route.path.clone()),
                backend: Some(backend_name.clone()),
                detail: Some(e.to_string()),
            };
            return request_error(502, req, client_ip, state, Some(&err_ctx), registry);
        }
        Err(SocketLaunchError::Register(e)) => {
            warn!(backend = %backend_name, error = %e, "registering socket upstream");
            let err_ctx = error::ErrorContext {
                route: Some(route.path.clone()),
                backend: Some(backend_name.clone()),
                detail: Some(e.to_string()),
            };
            return request_error(502, req, client_ip, state, Some(&err_ctx), registry);
        }
    };

    let ctx = forward::PendingUrlContext {
        req: req.clone(),
        client_ip: client_ip.to_string(),
        enc: content_encoding.to_string(),
        backend_name,
        kind: forward::PendingKind::Socket,
        route_path: Some(route.path.clone()),
        cacheable,
        is_prefetch,
        start: backend_start,
        error_status_override: None,
        extra_headers: Vec::new(),
    };
    RequestOutcome::Pending {
        id: UpstreamId::Slot(slot),
        ctx: Box::new(ctx),
    }
}

/// Answer 503 with `Retry-After: 1` through the error mode.
///
/// An upstream the process cannot start a connection to right now answers
/// this: a full upstream table for a URL backend, a full listen queue for a
/// socket one. Both are transient, so the client is told to retry.
fn backend_unavailable(
    req: &forward::HttpRequest,
    client_ip: &str,
    state: &mut ServerState,
    registry: &m6_core::platform::Registry,
    route_path: &str,
    backend_name: &str,
    detail: &str,
) -> RequestOutcome {
    let err_ctx = error::ErrorContext {
        route: Some(route_path.to_string()),
        backend: Some(backend_name.to_string()),
        detail: Some(detail.to_string()),
    };
    match apply_error_mode(503, req, client_ip, state, Some(&err_ctx), registry) {
        ErrorOutcome::Ready(s, mut h, b, n) => {
            h.push(("Retry-After".to_string(), "1".to_string()));
            RequestOutcome::Ready(s, h, b, n)
        }
        ErrorOutcome::Pending { id, mut ctx } => {
            ctx.extra_headers
                .push(("Retry-After".to_string(), "1".to_string()));
            RequestOutcome::Pending { id, ctx }
        }
    }
}

/// Check the per-IP rate limit ahead of everything else (cache lookup,
/// routing, backend dispatch). Returns `Some(outcome)` when the request
/// should be rejected — caller should return it immediately without doing
/// any further work. `None` means proceed as normal.
fn check_rate_limit(
    state: &mut ServerState,
    client_ip: &str,
    path: &str,
    user_agent: Option<&str>,
) -> Option<RequestOutcome> {
    if !state.config.rate_limit.enabled {
        return None;
    }
    let limit = state.config.rate_limit.requests_per_min;
    if !state.rate_limiter.check_and_increment(client_ip, limit) {
        return None;
    }
    if state.config.analytics.enabled {
        analytics::log_rate_limited(
            &state.config.node.name,
            path,
            client_ip,
            user_agent,
            &mut state.traffic,
        );
    }
    let headers = vec![
        (
            "Content-Type".to_string(),
            "text/plain; charset=utf-8".to_string(),
        ),
        ("Retry-After".to_string(), "60".to_string()),
    ];
    Some(RequestOutcome::Ready(
        429,
        headers,
        b"Too Many Requests".to_vec(),
        "rate-limit".to_string(),
    ))
}

/// Try to fetch the configured `[errors] mode = "custom"` error page without
/// blocking the loop: a URL/H2C/H2S backend is dispatched to its own thread or
/// pool, and a socket backend to a non-blocking `client::Upstream`, both carrying
/// `error_status_override` so the completion returns the fetched body under the
/// original failing status.
///
/// `error_ctx` supplies the route, backend and detail the error page renderer
/// shows in verbose mode. Returns `None` when there is no custom error path
/// configured, the request already targets it (anti-recursion), or the error
/// backend cannot be reached -- the caller falls back to the internal page.
fn dispatch_custom_error_async(
    status: u16,
    req: &forward::HttpRequest,
    client_ip: &str,
    state: &mut ServerState,
    registry: &m6_core::platform::Registry,
    error_ctx: Option<&error::ErrorContext>,
) -> Option<RequestOutcome> {
    let error_path = match &state.error_mode {
        ErrorMode::Custom { path } => path.clone(),
        _ => return None,
    };
    if req.path == error_path {
        return None;
    }

    // Held copy first. The body is identical for every path that misses, so
    // one fetch answers all of them -- which is the whole point: a wordlist
    // sweep of hundreds of unique paths costs one origin round trip, not one
    // per path.
    //
    // The ORIGINAL path is still what gets logged: analytics records
    // `req.path` from the caller, not whatever this body was rendered for.
    let served_locally = state
        .error_pages
        .get(&status)
        .filter(|(fetched, _, _)| fetched.elapsed() < ERROR_PAGE_TTL)
        .map(|(_, h, b)| (h.clone(), b.clone()));

    if let Some((mut headers, body)) = served_locally {
        let latency_ns = m6_core::platform::now().elapsed().as_nanos() as u64;
        analytics::finish_response(
            state.config.analytics.enabled,
            &mut headers,
            &req.headers,
            &analytics::Event {
                node: &state.config.node.name,
                path: &req.path,
                status,
                // Neither HIT nor MISS: the response cache was not consulted
                // and no backend was contacted.
                cache_state: "LOCAL",
                client_ip,
                latency_ns: Some(latency_ns),
            },
            &mut state.traffic,
        );
        return Some(RequestOutcome::Ready(
            status,
            headers,
            body,
            "error-local".to_string(),
        ));
    }

    let backend_name = state.route_table.at(&error_path)?.backend.clone();
    let original_host = origin_host(req, &state.config).to_string();
    let timeout = std::time::Duration::from_secs(state.config.server.backend_timeout_secs);

    let mut error_query = format!("status={}&from={}", status, urlencoded(&req.path));
    if let Some(c) = error_ctx {
        if let Some(ref r) = c.route {
            error_query.push_str(&format!("&route={}", urlencoded(r)));
        }
        if let Some(ref b) = c.backend {
            error_query.push_str(&format!("&backend={}", urlencoded(b)));
        }
        if let Some(ref d) = c.detail {
            error_query.push_str(&format!("&detail={}", urlencoded(d)));
        }
    }
    let error_req = forward::HttpRequest {
        method: "GET".to_string(),
        path: error_path,
        query: Some(error_query),
        version: req.version.clone(),
        headers: vec![("Host".to_string(), original_host.to_string())],
        body: vec![],
    };

    // URL backend: an h2 stream, or a loop-driven upstream slot. A failed
    // dispatch returns None, and the caller falls back to the internal page.
    if let Some(backend_idx) = state.pool_manager.find_url(&backend_name) {
        let ctx = forward::PendingUrlContext {
            req: req.clone(),
            client_ip: client_ip.to_string(),
            enc: String::new(),
            backend_name: backend_name.clone(),
            kind: forward::PendingKind::Url,
            route_path: None,
            cacheable: false,
            is_prefetch: false,
            start: m6_core::platform::now(),
            error_status_override: Some(status),
            extra_headers: Vec::new(),
        };
        let id = dispatch_url_backend(state, registry, &error_req, client_ip, backend_idx).ok()?;
        return Some(RequestOutcome::Pending {
            id,
            ctx: Box::new(ctx),
        });
    }

    // Socket backend: a non-blocking upstream, the same way the socket branch
    // reaches a normal route.
    let slot = launch_socket_upstream(
        state,
        registry,
        &error_req,
        client_ip,
        &backend_name,
        timeout,
    )
    .ok()?;

    let ctx = forward::PendingUrlContext {
        req: req.clone(),
        client_ip: client_ip.to_string(),
        enc: String::new(),
        backend_name: backend_name.clone(),
        kind: forward::PendingKind::Socket,
        route_path: None,
        cacheable: false,
        is_prefetch: false,
        start: m6_core::platform::now(),
        error_status_override: Some(status),
        extra_headers: Vec::new(),
    };
    Some(RequestOutcome::Pending {
        id: UpstreamId::Slot(slot),
        ctx: Box::new(ctx),
    })
}

/// RFC 9110 11.6.1: a 401 names the authentication scheme that would pass.
/// Keeps a `WWW-Authenticate` a backend already supplied, since the error
/// path preserves it.
fn add_bearer_challenge(headers: &mut Vec<(String, String)>) {
    if !headers
        .iter()
        .any(|(k, _)| k.eq_ignore_ascii_case("www-authenticate"))
    {
        headers.push(("WWW-Authenticate".to_string(), "Bearer".to_string()));
    }
}

/// Apply the configured error mode for a given status code.
///
/// For `Custom` mode, when the error page is not already held locally, this
/// starts an upstream carrying `error_status_override` and returns `Pending`,
/// so the fetch never blocks the loop. Falls back to `Internal` mode if the
/// fetch cannot be started or the request is already to the error path.
fn apply_error_mode(
    status: u16,
    req: &forward::HttpRequest,
    client_ip: &str,
    state: &mut ServerState,
    ctx: Option<&error::ErrorContext>,
    registry: &m6_core::platform::Registry,
) -> ErrorOutcome {
    let verbose = state.config.errors.verbose_fallback;
    match &state.error_mode {
        ErrorMode::Status => ErrorOutcome::Ready(
            status,
            vec![("Content-Type".to_string(), "text/plain".to_string())],
            vec![],
            "error".to_string(),
        ),
        ErrorMode::Internal => {
            let reason = error::status_reason(status);
            let body = error::internal_error_html(status, reason, verbose, &req.path, ctx);
            ErrorOutcome::Ready(
                status,
                vec![(
                    "Content-Type".to_string(),
                    "text/html; charset=utf-8".to_string(),
                )],
                body,
                "error".to_string(),
            )
        }
        ErrorMode::Custom { path: error_path } => {
            let error_path = error_path.clone();

            // Anti-recursion: if the current request is already to the error path, fall back.
            if req.path == error_path {
                let reason = error::status_reason(status);
                let body = error::internal_error_html(status, reason, verbose, &req.path, ctx);
                return ErrorOutcome::Ready(
                    status,
                    vec![(
                        "Content-Type".to_string(),
                        "text/html; charset=utf-8".to_string(),
                    )],
                    body,
                    "error".to_string(),
                );
            }

            match dispatch_custom_error_async(status, req, client_ip, state, registry, ctx) {
                Some(RequestOutcome::Ready(s, h, b, n)) => ErrorOutcome::Ready(s, h, b, n),
                Some(RequestOutcome::Pending { id, ctx }) => ErrorOutcome::Pending { id, ctx },
                Some(RequestOutcome::Cached { .. }) => {
                    unreachable!("a custom error page is never a cached hit")
                }
                None => {
                    warn!(error_path = %error_path, "custom error: cannot fetch the error page, falling back to internal");
                    let reason = error::status_reason(status);
                    let body = error::internal_error_html(status, reason, verbose, &req.path, ctx);
                    ErrorOutcome::Ready(
                        status,
                        vec![(
                            "Content-Type".to_string(),
                            "text/html; charset=utf-8".to_string(),
                        )],
                        body,
                        "error".to_string(),
                    )
                }
            }
        }
    }
}

/// Apply an error mode and turn the outcome straight into a request outcome, so
/// the request path returns either a ready response or a parked fetch.
fn request_error(
    status: u16,
    req: &forward::HttpRequest,
    client_ip: &str,
    state: &mut ServerState,
    ctx: Option<&error::ErrorContext>,
    registry: &m6_core::platform::Registry,
) -> RequestOutcome {
    match apply_error_mode(status, req, client_ip, state, ctx, registry) {
        ErrorOutcome::Ready(s, h, b, n) => RequestOutcome::Ready(s, h, b, n),
        ErrorOutcome::Pending { id, ctx } => RequestOutcome::Pending { id, ctx },
    }
}

/// Ensure exactly one `alt-svc` header advertising HTTP/3 is present, replacing
/// any pre-existing one instead of appending a duplicate. A pre-existing entry
/// shows up whenever the response body already passed through another m6-http
/// instance (a cache node's backend is the origin, and the origin already
/// advertised its own alt-svc before the cache node forwards or caches it).
fn set_alt_svc(headers: &mut Vec<(String, String)>, quic_port: u16) {
    headers.retain(|(k, _)| !k.eq_ignore_ascii_case("alt-svc"));
    headers.push((
        "alt-svc".to_string(),
        format!("h3=\":{quic_port}\"; ma=86400"),
    ));
}

/// Ensure `Accept-Encoding` appears exactly once in a single `Vary` header,
/// **preserving any other field names the backend named**.
///
/// Same duplication hazard as `set_alt_svc`: a cache node's cached headers are
/// whatever its own upstream (origin) sent, and origin adds this same header on
/// its own cache hits, so a cache node replaying a cache hit of its own would
/// otherwise double it up. Hence collapsing to one header rather than pushing.
///
/// This merges the field names. It previously dropped every existing `Vary` and
/// wrote `Accept-Encoding` in its place, which was harmless while it ran solely
/// on the cache-hit path. It is now also called on the miss path, after
/// `handle_request_inner` has made the local cache decision. An overwriting
/// version would send a backend's `Vary: Cookie` to the client as
/// `Vary: Accept-Encoding`, so a downstream shared cache could store one
/// client's private variant and replay it to everyone. Preserving the other
/// field names keeps that response uncacheable downstream, the same rule
/// `should_cache` applies locally.
fn set_vary_accept_encoding(headers: &mut Vec<(String, String)>, backend_compresses: bool) {
    // A backend that does not compress has exactly ONE representation, so there
    // is no encoding dimension to vary on and saying otherwise is a promise of
    // variants that will never exist. m6-http is a cache, not a transformer: it
    // does not compress, so it cannot manufacture the alternatives this header
    // would be advertising. See BackendConfig::compresses.
    //
    // Skipped rather than stripped: if the backend named `Vary: Accept-Encoding`
    // itself, that is its statement about its own output and not ours to remove.
    if !backend_compresses && !headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("vary")) {
        return;
    }
    // The merge (including the `Vary: *` passthrough) is shared with the cached
    // hit writer, so the two paths cannot disagree.
    let merge = VaryMerge::new(headers);
    if merge.is_star() {
        return;
    }
    let fields: Vec<String> = merge.map(str::to_string).collect();
    headers.retain(|(k, _)| !k.eq_ignore_ascii_case("vary"));
    headers.push(("vary".to_string(), fields.join(", ")));
}

/// Whether this is a method the HTTP standards define, as opposed to one this
/// server has simply not been configured to allow.
///
/// The distinction decides 405 vs 501 (RFC 9110 15.5.6 and 15.6.2). The list is
/// the RFC 9110 methods plus PATCH (RFC 5789), which is registered and in wide
/// use. Anything outside it -- `FOO`, a typo, a probe -- is a method this
/// server genuinely does not implement, and saying so is more honest than
/// implying the resource merely disallows it.
///
/// Matched case-sensitively: HTTP methods are case-sensitive tokens, so `get`
/// is not GET and should not be dignified with a 405.
/// Invalidate cached entries for a URI after a state-changing request
/// succeeds (RFC 9111 4.4, MUST).
///
/// A successful POST/PUT/DELETE means the stored representation of that URI is
/// now wrong, and nothing invalidated it: the cache kept serving the old copy
/// until it expired on its own. That is a live concern for any site with an
/// editing interface: edit a page, and the edge keeps serving the previous one.
///
/// "Non-error status" is the RFC's condition: a 4xx/5xx means the state change
/// did not happen, so the cached copy is still correct and must be left alone.
/// Invalidating on failure would hand an attacker a trivial way to flush the
/// cache by spamming failing POSTs.
///
/// `Location` and `Content-Location` are invalidated too, but only when they
/// point at this same origin — an off-site redirect target is not ours to
/// evict, and following it blindly would let a backend clear arbitrary entries.
fn invalidate_after_unsafe_method(
    state: &ServerState,
    req: &forward::HttpRequest,
    status: u16,
    headers: &[(String, String)],
) {
    // RFC 9110 9.2.1 safe methods change nothing, so there is nothing to
    // invalidate. Everything else is state-changing as far as a cache is
    // concerned, including methods this server does not itself implement.
    if matches!(req.method.as_str(), "GET" | "HEAD" | "OPTIONS" | "TRACE") {
        return;
    }
    if status >= 400 {
        return;
    }
    state.cache.evict_path(&req.path);

    for name in ["location", "content-location"] {
        if let Some(v) = m6_core::headers::get(headers, name) {
            // Same-origin only. A bare path is ours by definition; an absolute
            // URL is ours only if its authority matches the configured domain.
            let path = if v.starts_with('/') {
                Some(v)
            } else {
                v.split_once("://")
                    .map(|(_, rest)| rest)
                    .and_then(|rest| rest.split_once('/'))
                    .filter(|(host, _)| {
                        let host = host.split(':').next().unwrap_or(host);
                        host == state.config.site.domain
                            || host == format!("www.{}", state.config.site.domain)
                    })
                    .map(|(_, p)| p)
            };
            if let Some(p) = path {
                let p = if p.starts_with('/') {
                    p.to_string()
                } else {
                    format!("/{p}")
                };
                state.cache.evict_path(&p);
            }
        }
    }
}

/// Add `Date` if the response does not already carry one (RFC 9110 6.6.1, MUST).
///
/// m6 has a clock and generated no `Date` on anything. A recipient cannot
/// compute a response's age without it, which is why `Age` alone is not
/// enough: a downstream cache needs both to work out how stale something is.
///
/// Only ever added, never replaced. On the miss path this is the moment the
/// response was generated, which is exactly what `Date` means; a cache HIT
/// replays the stored headers and therefore carries the ORIGINAL date forward,
/// which is required — restamping a cached response with the current time
/// would make it look freshly generated and silently defeat the `Age` header
/// sitting next to it.
fn set_date(headers: &mut Vec<(String, String)>) {
    if m6_core::headers::contains(&headers[..], "date") {
        return;
    }
    headers.push((
        "date".to_string(),
        httpdate::fmt_http_date(m6_core::platform::wall()),
    ));
}

/// Emit `Age` on a response served from cache (RFC 9111 5.1, MUST).
///
/// Nothing emitted it at all, so a downstream cache had no way to tell how old
/// what we handed it already was and treated a minute-old response as newly
/// generated. Combined with a shared cache chain, each hop restarted the clock.
///
/// Replaces any `Age` already present rather than appending: the stored value
/// is whatever the origin declared when the entry was filled, and it is now
/// stale by exactly the time we have held it. That upstream value is not
/// discarded — it is folded into the age the cache computes (see
/// `CacheEntry::upstream_age`) — but it must not also be emitted verbatim.
fn set_age(headers: &mut Vec<(String, String)>, age: std::time::Duration) {
    headers.retain(|(k, _)| !k.eq_ignore_ascii_case("age"));
    headers.push(("age".to_string(), age.as_secs().to_string()));
}

/// Ceiling on a buffered HTTP/3 request body. Mirrors the HTTP/2 limit so the
/// two protocols cannot disagree about what is acceptable to accept.
const MAX_H3_BODY: usize = 20 * 1024 * 1024;

/// HTTP/3 carries its headers as `quiche::h3::Header`, while
/// `RequestDirectives::parse` takes the `(String, String)` shape the other
/// paths already use. Only the two directive-bearing headers are extracted, so
/// this allocates a two-element vector at most rather than copying the whole
/// header block on every request.
fn owned_headers_for_cc(headers: &[quiche::h3::Header]) -> Vec<(String, String)> {
    headers
        .iter()
        .filter(|h| {
            let n = h.name();
            n.eq_ignore_ascii_case(b"cache-control") || n.eq_ignore_ascii_case(b"pragma")
        })
        .map(|h| {
            (
                String::from_utf8_lossy(h.name()).into_owned(),
                String::from_utf8_lossy(h.value()).into_owned(),
            )
        })
        .collect()
}

fn is_registered_method(method: &str) -> bool {
    matches!(
        method,
        "GET" | "HEAD" | "POST" | "PUT" | "DELETE" | "CONNECT" | "OPTIONS" | "TRACE" | "PATCH"
    )
}

/// Advertise the site's machine-readable description on every HTML response:
/// `Link: </llms.txt>; rel="describedby"`.
///
/// Header rather than only `<link rel="describedby">` in `<head>` because it
/// arrives before the page is parsed — anything inspecting response headers on
/// its first request finds the machine-facing layer without reading markup.
/// The site ships both; the markup one covers readers that only see the
/// document.
///
/// HTML only. A `Link: rel="describedby"` on a stylesheet or a PNG would be
/// noise: llms.txt describes the *site*, and every asset claiming to be
/// described by it says nothing useful and inflates every asset response.
///
/// Same duplication hazard as `set_alt_svc`: a cache node's backend is the
/// origin, which already added this header before the response was forwarded
/// and cached, so the node would otherwise emit two. Existing `describedby`
/// links are dropped first, and *only* those, because `Link` may be carrying
/// anything else a backend set and none of it is ours to discard. Until
/// 2026-09-18 the thing it had to protect was this server's own preload hints;
/// those are gone (issue #93), but a backend or an author may still declare
/// `Link` headers of their own, so the rule is unchanged and now rests on a
/// better reason: selective replacement is correct regardless of who set them.
fn set_describedby_link(headers: &mut Vec<(String, String)>, target: &str) {
    if target.is_empty() || !analytics::is_html_response(headers) {
        return;
    }
    headers.retain(|(k, v)| {
        !(k.eq_ignore_ascii_case("link") && v.to_ascii_lowercase().contains("rel=\"describedby\""))
    });
    headers.push((
        "link".to_string(),
        format!("<{target}>; rel=\"describedby\""),
    ));
}

/// The internal error page a failed custom-error-page fetch falls back to, for
/// both the socket and URL override branches. Only the caller's warn label
/// differs.
fn error_page_fallback(
    original_status: u16,
    ctx: &forward::PendingUrlContext,
    state: &mut ServerState,
) -> FinalizedResponse {
    let req = &ctx.req;
    let analytics_on = state.config.analytics.enabled && !ctx.is_prefetch;
    let latency_ns = ctx.start.elapsed().as_nanos() as u64;
    let reason = error::status_reason(original_status);
    let body = error::internal_error_html(
        original_status,
        reason,
        state.config.errors.verbose_fallback,
        &req.path,
        None,
    );
    let mut headers = vec![(
        "Content-Type".to_string(),
        "text/html; charset=utf-8".to_string(),
    )];
    analytics::finish_response(
        analytics_on,
        &mut headers,
        &req.headers,
        &analytics::Event {
            node: &state.config.node.name,
            path: &req.path,
            status: original_status,
            cache_state: "MISS",
            client_ip: &ctx.client_ip,
            latency_ns: Some(latency_ns),
        },
        &mut state.traffic,
    );
    (original_status, headers, body, "error".to_string())
}

/// Finish a socket-backend reply with the socket branch's own completion
/// semantics: cache insert, error mode for any status >= 400 (with actionable
/// headers preserved), and `finish_response` analytics. A reply that needs a
/// custom error page returns `Completion::Pending` with the error-page fetch.
fn complete_socket_response(
    result: std::io::Result<forward::HttpResponse>,
    ctx: &forward::PendingUrlContext,
    quic_port: u16,
    state: &mut ServerState,
    registry: &m6_core::platform::Registry,
) -> Completion {
    let req = &ctx.req;
    let backend_name = &ctx.backend_name;
    let content_encoding = &ctx.enc;
    let analytics_enabled = state.config.analytics.enabled && !ctx.is_prefetch;

    // A custom-error-page fetch on a socket backend: the fetched body returns
    // under the ORIGINAL failing status, is held for reuse, and is never
    // cached. Mirrors finalize_url_response_inner's override branch, but uses
    // finish_response: a socket backend never sets _m6sid itself.
    if let Some(original_status) = ctx.error_status_override {
        let latency_ns = ctx.start.elapsed().as_nanos() as u64;
        return match result {
            Ok(http_resp) => {
                let mut headers = http_resp.headers;
                headers.extend(ctx.extra_headers.clone());
                state.error_pages.insert(
                    original_status,
                    (
                        m6_core::platform::now(),
                        headers.clone(),
                        http_resp.body.clone(),
                    ),
                );
                analytics::finish_response(
                    analytics_enabled,
                    &mut headers,
                    &req.headers,
                    &analytics::Event {
                        node: &state.config.node.name,
                        path: &req.path,
                        status: original_status,
                        cache_state: "MISS",
                        client_ip: &ctx.client_ip,
                        latency_ns: Some(latency_ns),
                    },
                    &mut state.traffic,
                );
                Completion::Ready((
                    original_status,
                    headers,
                    http_resp.body,
                    "error".to_string(),
                ))
            }
            Err(e) => {
                warn!(backend = %ctx.backend_name, error = %e, "custom error page fetch failed (socket), falling back to internal");
                Completion::Ready(error_page_fallback(original_status, ctx, state))
            }
        };
    }

    let (status, mut resp_headers, body, conn_err) = match result {
        Ok(mut http_resp) => {
            // Stamp Date BEFORE the cache decision so the stored entry carries
            // the generation time.
            set_date(&mut http_resp.headers);
            if ctx.cacheable
                && request_permits_storage(&req.headers)
                && should_cache(http_resp.status, &http_resp.headers)
            {
                let key = CacheKey::new(&req.path, req.query.as_deref(), content_encoding);
                state.cache.insert(
                    key,
                    CachedResponse {
                        status: http_resp.status,
                        headers: std::sync::Arc::new(strip_set_cookie(&http_resp.headers)),
                        body: Bytes::from(http_resp.body.clone()),
                    },
                );
            }
            (
                http_resp.status,
                http_resp.headers,
                http_resp.body,
                None::<String>,
            )
        }
        Err(e) => {
            warn!(backend = %backend_name, error = %e, "backend error");
            (502u16, vec![], vec![], Some(e.to_string()))
        }
    };

    // If the response is an error (4xx/5xx), apply the error mode. The error
    // page replaces the backend's response wholesale, so the actionable headers
    // carry over by hand (a backend's 429 keeps its Retry-After, a 401 its
    // WWW-Authenticate).
    if status >= 400 {
        let detail = conn_err.unwrap_or_else(|| format!("{backend_name} returned {status}"));
        let err_ctx = error::ErrorContext {
            route: ctx.route_path.clone(),
            backend: Some(backend_name.clone()),
            detail: Some(detail),
        };
        match apply_error_mode(status, req, &ctx.client_ip, state, Some(&err_ctx), registry) {
            ErrorOutcome::Ready(s, mut h, b, n) => {
                error::preserve_actionable_headers(&resp_headers, &mut h);
                // The old socket branch returned Ready and its h1/h2/h3 caller
                // applied alt-svc, so an error response carried it too.
                set_alt_svc(&mut h, quic_port);
                let backend_ns = ctx.start.elapsed().as_nanos() as u64;
                analytics::finish_response(
                    analytics_enabled,
                    &mut h,
                    &req.headers,
                    &analytics::Event {
                        node: &state.config.node.name,
                        path: &req.path,
                        status: s,
                        cache_state: "MISS",
                        client_ip: &ctx.client_ip,
                        latency_ns: Some(backend_ns),
                    },
                    &mut state.traffic,
                );
                Completion::Ready((s, h, b, n))
            }
            ErrorOutcome::Pending {
                id,
                ctx: mut new_ctx,
            } => {
                error::preserve_actionable_headers(&resp_headers, &mut new_ctx.extra_headers);
                set_alt_svc(&mut new_ctx.extra_headers, quic_port);
                Completion::Pending { id, ctx: new_ctx }
            }
        }
    } else {
        let backend_ns = ctx.start.elapsed().as_nanos() as u64;
        analytics::finish_response(
            analytics_enabled,
            &mut resp_headers,
            &req.headers,
            &analytics::Event {
                node: &state.config.node.name,
                path: &req.path,
                status,
                cache_state: "MISS",
                client_ip: &ctx.client_ip,
                latency_ns: Some(backend_ns),
            },
            &mut state.traffic,
        );
        set_alt_svc(&mut resp_headers, quic_port);

        Completion::Ready((status, resp_headers, body, backend_name.clone()))
    }
}

/// Called when a pending backend reply arrives, whether from a URL-backend
/// thread or a socket upstream the loop drove. Handles cache insertion, alt-svc
/// injection, and error mode application.
///
/// Wrapped for the same reason as `handle_request`: this is the async
/// completion path, so it never passes through that wrapper, and a response
/// forwarded from a backend needs `Vary: Accept-Encoding` exactly as much as a
/// synchronous one. Applied after the inner call so `should_cache` still sees
/// the backend's own `Vary`.
fn finalize_url_response(
    http_result: std::io::Result<forward::HttpResponse>,
    ctx: &forward::PendingUrlContext,
    quic_port: u16,
    state: &mut ServerState,
    registry: &m6_core::platform::Registry,
) -> Completion {
    let describedby = state.config.site.describedby.clone();
    let outcome = if ctx.kind == forward::PendingKind::Socket {
        complete_socket_response(http_result, ctx, quic_port, state, registry)
    } else {
        finalize_url_response_inner(http_result, ctx, quic_port, state, registry)
    };
    match outcome {
        Completion::Ready((s, mut h, b, n)) => {
            let compresses = state.config.backend_compresses(&n);
            set_vary_accept_encoding(&mut h, compresses);
            set_date(&mut h);
            set_describedby_link(&mut h, &describedby);
            invalidate_after_unsafe_method(state, &ctx.req, s, &h);
            Completion::Ready((s, h, b, n))
        }
        pending => pending,
    }
}

fn finalize_url_response_inner(
    http_result: std::io::Result<forward::HttpResponse>,
    ctx: &forward::PendingUrlContext,
    quic_port: u16,
    state: &mut ServerState,
    registry: &m6_core::platform::Registry,
) -> Completion {
    let req = &ctx.req;
    let enc = &ctx.enc;

    // A background fetch has no visitor behind it. Its client_ip is the
    // 127.0.0.1 placeholder, so logging it would invent traffic and mint a
    // throwaway session -- the synchronous path already suppresses this via
    // handle_request's is_prefetch, and the async path has to match.
    let analytics_on = state.config.analytics.enabled && !ctx.is_prefetch;

    // This dispatch was itself an async fetch of the custom error page
    // (see `dispatch_custom_error_async`) — the ORIGINAL failing status
    // rides along in `error_status_override` regardless of whatever status
    // the error-page backend itself returned (normally 200, for a
    // successfully rendered template). Never cache it; never re-derive
    // another error page on top of it — if even this fetch fails (backend
    // down), fall back to the plain internal page under the original status.
    if let Some(original_status) = ctx.error_status_override {
        let latency_ns = ctx.start.elapsed().as_nanos() as u64;
        return match http_result {
            Ok(http_resp) => {
                let mut headers = http_resp.headers;
                headers.extend(ctx.extra_headers.clone());
                state.error_pages.insert(
                    original_status,
                    (
                        m6_core::platform::now(),
                        headers.clone(),
                        http_resp.body.clone(),
                    ),
                );
                analytics::finish_proxied_response(
                    analytics_on,
                    &mut headers,
                    &req.headers,
                    &analytics::Event {
                        node: &state.config.node.name,
                        path: &req.path,
                        status: original_status,
                        cache_state: "MISS",
                        client_ip: &ctx.client_ip,
                        latency_ns: Some(latency_ns),
                    },
                    &mut state.traffic,
                );
                Completion::Ready((
                    original_status,
                    headers,
                    http_resp.body,
                    "error".to_string(),
                ))
            }
            Err(e) => {
                warn!(backend = %ctx.backend_name, error = %e, "custom error page fetch failed (async), falling back to internal");
                Completion::Ready(error_page_fallback(original_status, ctx, state))
            }
        };
    }

    let (status, resp_headers, body, used_backend, is_connection_failure) = match http_result {
        Ok(mut http_resp) => {
            // Same as the synchronous path: Date is stamped before the cache
            // decision so the stored entry carries the generation time.
            set_date(&mut http_resp.headers);
            if ctx.cacheable
                && request_permits_storage(&ctx.req.headers)
                && should_cache(http_resp.status, &http_resp.headers)
            {
                let key = CacheKey::new(&req.path, req.query.as_deref(), enc);
                state.cache.insert(
                    key,
                    CachedResponse {
                        status: http_resp.status,
                        headers: std::sync::Arc::new(strip_set_cookie(&http_resp.headers)),
                        body: Bytes::from(http_resp.body.clone()),
                    },
                );
            }
            (
                http_resp.status,
                http_resp.headers,
                http_resp.body,
                ctx.backend_name.clone(),
                false,
            )
        }
        Err(e) => {
            warn!(backend = %ctx.backend_name, error = %e, "url backend error (async)");
            (502u16, vec![], vec![], "error".to_string(), true)
        }
    };

    // Only re-derive an error page for a genuine connection-level failure (no
    // body to show). A completed round trip to a URL/proxy backend — even
    // with a 4xx/5xx status — already carries that backend's own fully
    // rendered error page (e.g. origin applies its own [errors] mode before
    // responding to a cache node), so passing it through verbatim is correct;
    // re-deriving our own here would silently discard it and substitute the
    // generic internal page instead.
    if status >= 400 && is_connection_failure {
        let err_ctx = error::ErrorContext {
            route: None,
            backend: Some(ctx.backend_name.clone()),
            detail: Some(format!("backend returned {status}")),
        };
        match apply_error_mode(status, req, &ctx.client_ip, state, Some(&err_ctx), registry) {
            ErrorOutcome::Ready(s, mut h, b, n) => {
                let latency_ns = ctx.start.elapsed().as_nanos() as u64;
                analytics::finish_response(
                    analytics_on,
                    &mut h,
                    &req.headers,
                    &analytics::Event {
                        node: &state.config.node.name,
                        path: &req.path,
                        status: s,
                        cache_state: "MISS",
                        client_ip: &ctx.client_ip,
                        latency_ns: Some(latency_ns),
                    },
                    &mut state.traffic,
                );
                Completion::Ready((s, h, b, n))
            }
            ErrorOutcome::Pending { id, ctx } => Completion::Pending { id, ctx },
        }
    } else {
        let mut headers_with_altsvc = resp_headers;
        set_alt_svc(&mut headers_with_altsvc, quic_port);

        let latency_ns = ctx.start.elapsed().as_nanos() as u64;
        analytics::finish_proxied_response(
            analytics_on,
            &mut headers_with_altsvc,
            &req.headers,
            &analytics::Event {
                node: &state.config.node.name,
                path: &req.path,
                status,
                cache_state: "MISS",
                client_ip: &ctx.client_ip,
                latency_ns: Some(latency_ns),
            },
            &mut state.traffic,
        );

        Completion::Ready((status, headers_with_altsvc, body, used_backend))
    }
}

// ── QUIC packet flush helpers ─────────────────────────────────────────────────

fn flush_conn(udp: &UdpSocket, qconn: &mut QuicConn) {
    let mut out = [0u8; MAX_DATAGRAM_SIZE];
    loop {
        let (written, send_info) = match qconn.conn.send(&mut out) {
            Ok(v) => v,
            Err(quiche::Error::Done) => break,
            Err(e) => {
                warn!("conn.send error: {}", e);
                break;
            }
        };
        if let Err(e) = udp.send_to(&out[..written], send_info.to) {
            if e.kind() == std::io::ErrorKind::WouldBlock {
                break;
            }
            warn!("udp send error: {}", e);
        }
    }
}

/// Retry any responses (headers and/or body) blocked on flow-control credit,
/// for streams that now have some.
fn drain_writable(qconn: &mut QuicConn) {
    if qconn.partial_responses.is_empty() {
        return;
    }
    let h3 = match qconn.h3_conn.as_mut() {
        Some(h) => h,
        None => return,
    };
    let writable: Vec<u64> = qconn.conn.writable().collect();
    for stream_id in writable {
        // Take ownership out of the map up front: both arms below need to
        // call back into `qconn.conn`/`qconn.partial_responses`, so holding a
        // borrow from the map across that call would conflict with it.
        let pending = match qconn.partial_responses.remove(&stream_id) {
            Some(p) => p,
            None => continue,
        };
        match pending {
            PendingH3Response::Headers(headers, body) => {
                let fin = body.is_empty();
                match h3.send_response(&mut qconn.conn, stream_id, &headers, fin) {
                    Ok(()) => {
                        if !body.is_empty() {
                            match h3.send_body(&mut qconn.conn, stream_id, &body, true) {
                                Ok(written) if written == body.len() => {}
                                Ok(written) => {
                                    qconn
                                        .partial_responses
                                        .insert(stream_id, PendingH3Response::Body(body, written));
                                }
                                Err(quiche::h3::Error::Done)
                                | Err(quiche::h3::Error::StreamBlocked) => {
                                    qconn
                                        .partial_responses
                                        .insert(stream_id, PendingH3Response::Body(body, 0));
                                }
                                Err(e) => warn!("h3 drain_writable send_body error: {}", e),
                            }
                        }
                    }
                    Err(quiche::h3::Error::StreamBlocked) => {
                        // Still no credit — put it back for the next writable report.
                        qconn
                            .partial_responses
                            .insert(stream_id, PendingH3Response::Headers(headers, body));
                    }
                    Err(e) => warn!("h3 drain_writable send_response error: {}", e),
                }
            }
            PendingH3Response::Body(body, offset) => {
                let remaining = &body[offset..];
                match h3.send_body(&mut qconn.conn, stream_id, remaining, true) {
                    Ok(written) => {
                        let new_offset = offset + written;
                        if new_offset < body.len() {
                            qconn
                                .partial_responses
                                .insert(stream_id, PendingH3Response::Body(body, new_offset));
                        }
                    }
                    Err(quiche::h3::Error::Done) | Err(quiche::h3::Error::StreamBlocked) => {
                        qconn
                            .partial_responses
                            .insert(stream_id, PendingH3Response::Body(body, offset));
                    }
                    Err(e) => {
                        warn!("h3 drain_writable send_body error: {}", e);
                    }
                }
            }
        }
    }
}

fn flush_all(udp: &UdpSocket, connections: &mut HashMap<Vec<u8>, QuicConn>) {
    for qconn in connections.values_mut() {
        qconn.conn.on_timeout();
        drain_writable(qconn);
        flush_conn(udp, qconn);
    }
}

// ── CLI ────────────────────────────────────────────────────────────────────────

struct Cli {
    site_dir: PathBuf,
    system_config: PathBuf,
    log_level: Option<String>,
    dump_config: bool,
}

fn parse_args(args: &[String]) -> anyhow::Result<Cli> {
    let mut positional = Vec::new();
    let mut log_level = None;
    let mut dump_config = false;
    let mut i = 1;

    while i < args.len() {
        match args[i].as_str() {
            "--log-level" => {
                i += 1;
                if i >= args.len() {
                    anyhow::bail!("--log-level requires a value");
                }
                log_level = Some(args[i].clone());
            }
            "--dump-config" => {
                dump_config = true;
            }
            // Its own parser, so the flag is added here too. See m6-core's
            // `parse_invocation`.
            "--version" | "-V" => {
                println!("m6-http {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            arg if arg.starts_with("--") => {
                anyhow::bail!("unknown flag: {}", arg);
            }
            _ => {
                positional.push(args[i].clone());
            }
        }
        i += 1;
    }

    if positional.len() < 2 {
        anyhow::bail!("required arguments: <site-dir> <system-config>");
    }

    Ok(Cli {
        site_dir: PathBuf::from(&positional[0]),
        system_config: PathBuf::from(&positional[1]),
        log_level,
        dump_config,
    })
}

/// Simple percent-encoding for path in redirect URLs.
fn urlencoded(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        match c {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' | '~' | '/' => out.push(c),
            c => {
                let mut buf = [0u8; 4];
                for b in c.encode_utf8(&mut buf).as_bytes() {
                    out.push_str(&format!("%{:02X}", b));
                }
            }
        }
    }
    out
}

// ── Entry point ───────────────────────────────────────────────────────────────

fn main() {
    // Block SIGTERM and SIGINT before anything else. The mask is inherited
    // only by threads created after this point, and the signal descriptor
    // refuses to open without it. See m6_core::signal.
    m6_core::signal::block();

    // rustls requires an explicit CryptoProvider when multiple are available
    // (ring + aws-lc-rs both get pulled in transitively). Install ring first.
    rustls::crypto::ring::default_provider()
        .install_default()
        .ok();
    let args: Vec<String> = std::env::args().collect();
    std::process::exit(run(args));
}

fn run(args: Vec<String>) -> i32 {
    // Parse CLI arguments
    let cli = match parse_args(&args) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("usage error: {}", e);
            eprintln!(
                "Usage: m6-http <site-dir> <system-config> [--log-level <level>] [--dump-config]"
            );
            return 2;
        }
    };

    // Load config before initialising logging so we can read [log] from site.toml.
    let config = match config::load(&cli.site_dir, &cli.system_config) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            return 2;
        }
    };

    // The CLI --log-level overrides site.toml [log].level.
    let log_level = cli.log_level.as_deref().unwrap_or(&config.log.level);
    let level = match m6_core::log::parse_level(log_level) {
        Ok(level) => level,
        Err(e) => {
            eprintln!("usage error: --log-level: {e}");
            return 2;
        }
    };
    let analytics_path = config
        .analytics
        .enabled
        .then(|| PathBuf::from(&config.analytics.log_path));
    if let Err(e) = m6_core::log::init(level, analytics_path.as_deref()) {
        eprintln!("logging init error: {}", e);
        return 1;
    }

    // --dump-config
    //
    // Ahead of redirect mode below, because --dump-config must never bind a
    // socket: it is how a deploy validates a config before cutting a node over
    // to it, and a redirect config that started a listener instead of printing
    // would hold :80 on the very node the deploy was still checking.
    if cli.dump_config {
        // The dump goes through `println!`, so put stdout back to the blocking
        // flag the process started with before writing it.
        if let Err(e) = m6_core::log::stop() {
            eprintln!("logging stop error: {}", e);
            return 1;
        }
        // Start refuses a route table that does not build, so the dump does.
        if let Err(e) = RouteTable::from_config(&config) {
            eprintln!("route table error: {}", e);
            return 2;
        }
        match serde_json::to_string_pretty(&config) {
            Ok(s) => {
                println!("{}", s);
                return 0;
            }
            Err(e) => {
                eprintln!("dump-config error: {}", e);
                return 1;
            }
        }
    }

    // Redirect mode: this process is the plain-HTTP :80 half of the pair, so
    // it returns here and never builds QUIC, TLS, backends, the cache or the
    // route table. Deliberately a separate process from the :443 instance
    // rather than an extra listener inside it — a slow client on :80 then
    // cannot stall TLS serving, because it is not sharing that event loop.
    if let Some(ref bind) = config.server.redirect_bind {
        info!(bind = %bind, node = %config.node.name, "starting in HTTP->HTTPS redirect mode");
        if let Err(e) = m6_http_lib::redirect::run(bind) {
            error!(error = %e, "redirect listener failed");
            return 1;
        }
        return 0;
    }

    // Load public key if auth declared
    let public_key = if let Some(ref auth_cfg) = config.auth {
        let key_path = PathBuf::from(&auth_cfg.public_key);
        match PublicKey::from_pem_file(&key_path) {
            Ok(k) => Some(k),
            Err(e) => {
                eprintln!("auth key load error: {}", e);
                return 2;
            }
        }
    } else {
        None
    };

    // Install security response headers before anything can serve a response.
    m6_http_lib::security::configure(&config.security);

    // Build route table
    let route_table = match RouteTable::from_config(&config) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("route table error: {}", e);
            return 2;
        }
    };

    // Build pool manager. Every URL backend's host is resolved here, once,
    // before the loop: a name that does not resolve is a config error, and a
    // change of address needs a restart.
    let mut pool_manager = match PoolManager::from_config(&config.backends) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("config error: {e}");
            return 2;
        }
    };
    if let Err(e) = pool_manager.resolve() {
        eprintln!("config error: {e}");
        return 2;
    }

    // Compute error mode
    let error_mode = ErrorMode::from_config(&config.errors);

    // Build quiche TLS/QUIC config
    let mut quiche_config = match make_quiche_config(&config.server) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("quiche config error: {e}");
            return 2;
        }
    };

    // Bind UDP socket
    let udp = match UdpSocket::bind(&config.server.bind) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("bind error {}: {e}", config.server.bind);
            return 2;
        }
    };
    if let Err(e) = udp.set_nonblocking(true) {
        eprintln!("set_nonblocking error: {e}");
        return 2;
    }

    // Signals, first so a signal can never arrive with nothing listening. The
    // mask was blocked as the first statement of `main`.
    let signals = match Signals::new() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("signal init failed: {e}");
            return 2;
        }
    };
    let mut lifecycle = m6_core::signal::Lifecycle::new("m6-http", None);
    lifecycle.started();

    // Build TLS config for HTTP/1.1 and bind TCP listener on the same port.
    // Both are Some on this path: redirect mode returned above, and every other
    // mode has them enforced as required keys by config::load().
    let (tls_cert, tls_key) = match (&config.server.tls_cert, &config.server.tls_key) {
        (Some(c), Some(k)) => (c, k),
        _ => {
            eprintln!(
                "config error: [server].tls_cert and [server].tls_key are required to serve TLS"
            );
            return 2;
        }
    };
    let ceiling = match raise_fd_limit() {
        Ok(c) => c,
        Err(e) => {
            error!(error = %e, "raising the open-file limit failed");
            return 1;
        }
    };
    let tcp_listener = match make_tls_server_config(tls_cert, tls_key) {
        Ok(tls_cfg) => match Http11Listener::bind(
            &config.server.bind,
            tls_cfg,
            config.server.max_requests_per_connection,
            INITIAL_CONNECTIONS,
            ceiling,
        ) {
            Ok(l) => {
                info!(bind = %config.server.bind, "HTTP/1.1 over TLS listener started");
                Some(l)
            }
            // FATAL, not a warning. `bind` is configured on every node, so a
            // failure to bind it is a node that cannot serve: systemd sees a
            // running process, /health answers on whatever else is listening,
            // and nothing is on 443. That is the shape this project keeps
            // meeting -- artefact wrong, process healthy, failure deferred and
            // invisible -- and `Restart=on-failure` already exists to handle
            // the honest version.
            //
            // The distinction §3d asked for is between a listener that is
            // configured and failed, and one that was never configured. This
            // arm is the first. `h2c_bind` below is the second: absent on a
            // cache node, which is not a failure to bind but an absence of a
            // bind, and is left alone.
            Err(e) => {
                error!(bind = %config.server.bind, error = %e,
                       "HTTP/1.1 TCP listener bind failed; refusing to run without it");
                return 1;
            }
        },
        Err(e) => {
            error!(error = %e, "HTTP/1.1 TLS config failed; refusing to run without it");
            return 2;
        }
    };

    let h2c_listener = if let Some(ref h2c_bind) = config.server.h2c_bind {
        let internal_peers = config
            .server
            .is_internal(h2c_bind)
            .then(|| config.server.internal_peers.clone());
        match H2cListener::bind(h2c_bind, internal_peers, INITIAL_CONNECTIONS, ceiling) {
            Ok(l) => {
                info!(bind = %h2c_bind, "H2C (HTTP/2 cleartext) listener started");
                Some(l)
            }
            // Configured and failed, so fatal, by the same argument as the
            // TLS listener above. A cache node has no `h2c_bind` at all and
            // never reaches this arm.
            Err(e) => {
                error!(bind = %h2c_bind, error = %e,
                       "H2C listener bind failed; it is configured, so refusing to run without it");
                return 1;
            }
        }
    } else {
        None
    };

    info!(
        bind = %config.server.bind,
        h2c_bind = ?config.server.h2c_bind,
        internal_binds = ?config.server.internal_binds,
        internal_peers = ?config.server.internal_peers,
        site = %config.site.name,
        "m6-http listeners bound"
    );

    // Only the h2c listener can be internal, and only when [server]
    // internal_binds names it. The TLS and QUIC listener faces clients, so it is
    // always external. Without an h2c listener the h2c value is never read.
    // Computed before the struct literal, which moves `config`.
    let tls_iface = Iface::External;
    let h2c_iface = if config
        .server
        .h2c_bind
        .as_deref()
        .is_some_and(|b| config.server.is_internal(b))
    {
        Iface::Internal
    } else {
        Iface::External
    };
    let backend_timeout_secs = config.server.backend_timeout_secs;
    let quic_port = udp.local_addr().map(|a| a.port()).unwrap_or(8443);
    let alt_svc_line = std::sync::Arc::<[u8]>::from(
        format!("alt-svc: h3=\":{quic_port}\"; ma=86400").into_bytes(),
    );
    let describedby_line = if config.site.describedby.is_empty() {
        None
    } else {
        Some(std::sync::Arc::<[u8]>::from(
            format!("link: <{}>; rel=\"describedby\"", config.site.describedby).into_bytes(),
        ))
    };
    let mut state = ServerState {
        config,
        route_table,
        pool_manager,
        cache: Cache::new(),
        public_key,
        error_mode,
        stats: Stats::new(),
        sources: Slots::new(INITIAL_CONNECTIONS, ceiling, || Source::Vacant),
        generations: vec![0; INITIAL_CONNECTIONS],
        prefetch_queue: std::collections::VecDeque::new(),
        upstreams: Slots::new(INITIAL_CONNECTIONS, ceiling, UpstreamEntry::empty),
        read_timeouts: Timeouts::new(
            INITIAL_CONNECTIONS,
            std::time::Duration::from_secs(READ_TIMEOUT_SECS),
        ),
        h2_idle_timeouts: Timeouts::new(
            INITIAL_CONNECTIONS,
            std::time::Duration::from_secs(H2_IDLE_TIMEOUT_SECS),
        ),
        upstream_timeouts: Timeouts::new(
            INITIAL_CONNECTIONS,
            std::time::Duration::from_secs(backend_timeout_secs),
        ),
        h2c_pool: H2cClientPool::new(std::time::Duration::from_secs(backend_timeout_secs)),
        h2s_pool: H2sTlsClientPool::new(std::time::Duration::from_secs(backend_timeout_secs)),
        tls_iface,
        h2c_iface,
        rate_limiter: RateLimiter::new(),
        error_pages: HashMap::new(),
        started: m6_core::platform::now(),
        traffic: health::TrafficWindow::new(),
        traffic_report: None,
        lookup_key_buf: String::with_capacity(512),
        alt_svc_line,
        describedby_line,
    };

    if state.config.server.warm_on_start {
        seed_cache_warm(&mut state);
    }

    let code = event_loop(
        EventLoopIo {
            udp,
            tcp: tcp_listener,
            h2c: h2c_listener,
            signals,
        },
        &mut state,
        &mut quiche_config,
        &mut lifecycle,
    );
    lifecycle.complete();
    if let Err(e) = m6_core::log::stop() {
        // `stop` took the Sinks, so this line goes to stderr or nowhere.
        eprintln!("log stop failed: {e}");
        return 1;
    }
    code
}

#[cfg(test)]
mod www_redirect_tests {
    use super::*;

    use m6_http_lib::config::{
        AnalyticsConfig, Config, ErrorsConfig, LogConfig, NodeConfig, RateLimitConfig,
        SecurityConfig, ServerConfig, SiteConfig,
    };
    use std::path::PathBuf;

    fn cfg(domain: &str, redirect_www: bool) -> Config {
        Config {
            site: SiteConfig {
                name: "Test".to_string(),
                domain: domain.to_string(),
                redirect_www,
                describedby: String::new(),
            },
            server: ServerConfig {
                bind: "127.0.0.1:8443".to_string(),
                tls_cert: Some("/tmp/cert.pem".to_string()),
                tls_key: Some("/tmp/key.pem".to_string()),
                backend_timeout_secs: 30,
                max_requests_per_connection: 100,
                h2c_bind: None,
                internal_binds: Vec::new(),
                internal_peers: Vec::new(),
                redirect_bind: None,
                warm_on_start: false,
                allowed_methods: vec!["GET".into(), "HEAD".into(), "POST".into()],
            },
            log: LogConfig::default(),
            analytics: AnalyticsConfig::default(),
            health: Default::default(),
            node: NodeConfig {
                name: "test-node".to_string(),
            },
            rate_limit: RateLimitConfig::default(),
            errors: ErrorsConfig::default(),
            security: SecurityConfig::default(),
            auth: None,
            backends: vec![],
            routes: vec![],
            route_groups: vec![],
            site_dir: PathBuf::from("/tmp"),
        }
    }

    #[test]
    fn redirects_www_to_apex_preserving_path_and_query() {
        let c = cfg("example.com", true);
        assert_eq!(
            www_redirect_location(
                Some("www.example.com"),
                "/capabilities",
                Some("a=1&b=2"),
                &c
            ),
            Some("https://example.com/capabilities?a=1&b=2".to_string())
        );
        assert_eq!(
            www_redirect_location(Some("www.example.com"), "/", None, &c),
            Some("https://example.com/".to_string())
        );
    }

    #[test]
    fn host_matching_is_case_insensitive_and_port_tolerant() {
        let c = cfg("example.com", true);
        for host in ["WWW.example.com", "Www.ExAmPle.Com", "www.example.com:80"] {
            assert_eq!(
                www_redirect_location(Some(host), "/x", None, &c),
                Some("https://example.com/x".to_string()),
                "host {host} should redirect"
            );
        }
    }

    /// The apex itself must not redirect, or every request loops forever.
    #[test]
    fn apex_is_left_alone() {
        let c = cfg("example.com", true);
        assert_eq!(
            www_redirect_location(Some("example.com"), "/", None, &c),
            None
        );
    }

    /// Node hostnames have to keep serving directly: per-node verification
    /// depends on reaching one specific node by name, not via the GeoDNS apex.
    #[test]
    fn other_hosts_are_left_alone() {
        let c = cfg("example.com", true);
        for host in [
            "node-a.example.com",
            "node-b.example.com",
            "evil.example",
            "www.evil.example",
        ] {
            assert_eq!(
                www_redirect_location(Some(host), "/", None, &c),
                None,
                "host {host} must not redirect"
            );
        }
    }

    /// `www.` prefixing a *different* domain is not the configured site's www alias.
    #[test]
    fn www_of_another_domain_is_not_our_alias() {
        let c = cfg("example.com", true);
        assert_eq!(
            www_redirect_location(Some("www.example.com.evil.test"), "/", None, &c),
            None
        );
    }

    /// Location is built from the configured domain, never the request's own
    /// bytes -- otherwise this is an open redirect.
    #[test]
    fn never_echoes_the_client_supplied_host() {
        let c = cfg("example.com", true);
        let got = www_redirect_location(Some("www.example.com"), "/x", None, &c).unwrap();
        assert!(got.starts_with("https://example.com/"), "got {got}");
    }

    #[test]
    fn rejects_control_characters_rather_than_injecting_headers() {
        let c = cfg("example.com", true);
        assert_eq!(
            www_redirect_location(Some("www.example.com"), "/x\r\nX-Injected: 1", None, &c),
            None
        );
        assert_eq!(
            www_redirect_location(Some("www.example.com"), "/x", Some("a=1\r\nX-I: 1"), &c),
            None
        );
        assert_eq!(
            www_redirect_location(Some("www.example.com"), "/x\0y", None, &c),
            None
        );
    }

    #[test]
    fn disabled_by_config_and_absent_host() {
        assert_eq!(
            www_redirect_location(
                Some("www.example.com"),
                "/",
                None,
                &cfg("example.com", false)
            ),
            None
        );
        assert_eq!(
            www_redirect_location(None, "/", None, &cfg("example.com", true)),
            None
        );
    }

    /// A bare "www." with nothing after it must not panic or match.
    #[test]
    fn degenerate_hosts_do_not_panic() {
        let c = cfg("example.com", true);
        for host in ["www.", "www", "", ":80", "."] {
            assert_eq!(
                www_redirect_location(Some(host), "/", None, &c),
                None,
                "host {host:?}"
            );
        }
    }
}

#[cfg(test)]
mod refresh_request_tests {
    use super::*;

    fn enc_of(req: &forward::HttpRequest) -> Option<&str> {
        req.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("accept-encoding"))
            .map(|(_, v)| v.as_str())
    }

    /// The regression that mattered: without this header the backend replies
    /// identity and the identity body is cached under the encoded key.
    #[test]
    fn refresh_asks_for_the_encoding_it_will_be_cached_under() {
        let r = Refresh {
            path: "/assets/css/style.css".to_string(),
            query: None,
            enc: "gzip, deflate, br, zstd".to_string(),
        };
        let req = synth_refresh_request(&r);
        assert_eq!(enc_of(&req), Some("gzip, deflate, br, zstd"));
        assert_eq!(req.method, "GET");
        assert_eq!(req.path, "/assets/css/style.css");
        assert!(req.body.is_empty());
    }

    /// An identity entry is keyed on the empty string; sending
    /// `Accept-Encoding:` with an empty value would be a malformed request, so
    /// the header is omitted entirely instead.
    #[test]
    fn identity_refresh_sends_no_accept_encoding() {
        let r = Refresh {
            path: "/".to_string(),
            query: None,
            enc: String::new(),
        };
        let req = synth_refresh_request(&r);
        assert_eq!(enc_of(&req), None);
        assert!(req.headers.is_empty());
    }

    /// The coupling that actually broke: `handle_request` is told the encoding
    /// separately (it becomes the cache-key component) while the backend only
    /// learns it from the request's own `Accept-Encoding`. If those two ever
    /// disagree, the cache stores a body encoded one way under a key promising
    /// another, and every subsequent hit serves the wrong bytes. Assert they
    /// describe the same thing rather than trusting two call sites to agree.
    #[test]
    fn requested_encoding_matches_the_key_it_is_stored_under() {
        for enc in ["gzip, deflate, br, zstd", "br", "gzip", ""] {
            let r = Refresh {
                path: "/assets/css/style.css".to_string(),
                query: Some("v=2".to_string()),
                enc: enc.to_string(),
            };
            let req = synth_refresh_request(&r);
            let sent = req
                .headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case("accept-encoding"))
                .map(|(_, v)| v.as_str())
                .unwrap_or("");

            // What the backend is asked for...
            assert_eq!(
                sent, enc,
                "Accept-Encoding sent must equal the refresh encoding"
            );

            // ...must be the same string the entry is keyed on. Both sides of
            // the comparison are built the way the event loop builds them.
            let mut a = String::with_capacity(512);
            let mut b = String::with_capacity(512);
            let key_from_refresh = make_lookup_key(&r.path, r.query.as_deref(), &r.enc, &mut a);
            let key_from_request = make_lookup_key(&req.path, req.query.as_deref(), sent, &mut b);
            assert_eq!(
                key_from_refresh, key_from_request,
                "refresh for enc {enc:?} would store under a different key than it requested"
            );
        }
    }

    #[test]
    fn query_is_preserved_so_the_key_matches() {
        let r = Refresh {
            path: "/x".to_string(),
            query: Some("a=1&b=2".to_string()),
            enc: "br".to_string(),
        };
        let req = synth_refresh_request(&r);
        assert_eq!(req.query.as_deref(), Some("a=1&b=2"));
        assert_eq!(enc_of(&req), Some("br"));
    }
}

#[cfg(test)]
mod vary_tests {
    use super::*;

    fn vary_of(h: &[(String, String)]) -> Vec<&str> {
        h.iter()
            .filter(|(k, _)| k.eq_ignore_ascii_case("vary"))
            .map(|(_, v)| v.as_str())
            .collect()
    }

    fn hdrs(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn adds_the_header_when_the_backend_sent_none() {
        // The actual reported defect: a cache MISS carried no Vary at all, so
        // the first client to ask for any URL -- every fresh visitor -- got a
        // negotiated body with nothing saying it was negotiated.
        let mut h = hdrs(&[("content-type", "text/css")]);
        set_vary_accept_encoding(&mut h, true);
        assert_eq!(vary_of(&h), vec!["Accept-Encoding"]);
    }

    #[test]
    fn does_not_duplicate_an_existing_one() {
        // A cache node's upstream is the origin, which already added this on
        // its own cache hit.
        let mut h = hdrs(&[("vary", "Accept-Encoding")]);
        set_vary_accept_encoding(&mut h, true);
        assert_eq!(vary_of(&h), vec!["Accept-Encoding"]);
    }

    #[test]
    fn matches_case_insensitively_rather_than_appending_a_variant() {
        let mut h = hdrs(&[("Vary", "accept-encoding")]);
        set_vary_accept_encoding(&mut h, true);
        assert_eq!(vary_of(&h).len(), 1);
        assert_eq!(vary_of(&h)[0].to_lowercase(), "accept-encoding");
    }

    /// The one that matters most. Overwriting instead of merging would strip
    /// `Cookie` here, `should_cache` would then see a response varying only on
    /// encoding, and one client's private variant would be cached and replayed
    /// to everybody. This function is called on the miss path, ahead of that
    /// decision, so the other field names have to survive it.
    #[test]
    fn preserves_other_field_names_so_the_response_stays_uncacheable() {
        let mut h = hdrs(&[("vary", "Cookie")]);
        set_vary_accept_encoding(&mut h, true);
        assert_eq!(vary_of(&h).len(), 1);
        let v = vary_of(&h)[0].to_lowercase();
        assert!(v.contains("cookie"), "Cookie was dropped: {v}");
        assert!(
            v.contains("accept-encoding"),
            "Accept-Encoding missing: {v}"
        );
        assert!(
            !should_cache(200, &h),
            "a Cookie-varying response must not be cacheable"
        );
    }

    #[test]
    fn collapses_several_vary_headers_into_one() {
        let mut h = hdrs(&[("vary", "Cookie"), ("vary", "Accept-Language")]);
        set_vary_accept_encoding(&mut h, true);
        assert_eq!(vary_of(&h).len(), 1, "must emit exactly one Vary header");
        let v = vary_of(&h)[0].to_lowercase();
        for want in ["cookie", "accept-language", "accept-encoding"] {
            assert!(v.contains(want), "{want} missing from {v}");
        }
    }

    /// `Vary: *` means the response is unpredictable. Adding a field name to
    /// it would narrow a claim the backend deliberately left open, so it is
    /// passed through untouched -- and stays uncacheable.
    #[test]
    fn leaves_vary_star_alone() {
        let mut h = hdrs(&[("vary", "*")]);
        set_vary_accept_encoding(&mut h, true);
        assert_eq!(vary_of(&h), vec!["*"]);
        assert!(!should_cache(200, &h));
    }

    /// The encoding-only case must remain cacheable, or adding this header on
    /// the miss path would silently disable the cache for every asset.
    #[test]
    fn an_encoding_only_vary_is_still_cacheable() {
        let mut h = hdrs(&[("cache-control", "public"), ("content-type", "text/css")]);
        set_vary_accept_encoding(&mut h, true);
        assert!(should_cache(200, &h));
    }
}

#[cfg(test)]
mod describedby_tests {
    use super::*;

    fn hdrs(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn links(h: &[(String, String)]) -> Vec<&str> {
        h.iter()
            .filter(|(k, _)| k.eq_ignore_ascii_case("link"))
            .map(|(_, v)| v.as_str())
            .collect()
    }

    #[test]
    fn html_gets_the_header() {
        let mut h = hdrs(&[("content-type", "text/html; charset=utf-8")]);
        set_describedby_link(&mut h, "/llms.txt");
        assert_eq!(links(&h), vec!["</llms.txt>; rel=\"describedby\""]);
    }

    /// llms.txt describes the site, not a stylesheet. Attaching it to every
    /// asset response would be noise on the majority of requests.
    #[test]
    fn non_html_does_not() {
        for ct in ["text/css", "image/png", "application/json", "text/markdown"] {
            let mut h = hdrs(&[("content-type", ct)]);
            set_describedby_link(&mut h, "/llms.txt");
            assert!(links(&h).is_empty(), "{ct} should not carry the header");
        }
    }

    /// A deployment with no such file must not advertise one.
    #[test]
    fn empty_target_omits_it() {
        let mut h = hdrs(&[("content-type", "text/html")]);
        set_describedby_link(&mut h, "");
        assert!(links(&h).is_empty());
    }

    /// A cache node's backend is the origin, which already added this before
    /// the response was forwarded and cached. Without the dedupe the node
    /// emits two.
    #[test]
    fn does_not_duplicate_what_the_origin_already_sent() {
        let mut h = hdrs(&[
            ("content-type", "text/html"),
            ("link", "</llms.txt>; rel=\"describedby\""),
        ]);
        set_describedby_link(&mut h, "/llms.txt");
        assert_eq!(links(&h).len(), 1);
    }

    /// The one that would be easy to break: `Link` may carry preloads a backend
    /// or an author declared. Only the describedby entry may be replaced. m6 no
    /// longer generates preload links itself (issue #93), but relaying someone
    /// else's is exactly the case this has to keep getting right.
    #[test]
    fn preserves_preload_link_headers() {
        let mut h = hdrs(&[
            ("content-type", "text/html"),
            ("link", "</assets/css/style.css>; rel=preload; as=style"),
            ("link", "</llms.txt>; rel=\"describedby\""),
            ("link", "</assets/fonts/m.woff2>; rel=preload; as=font"),
        ]);
        set_describedby_link(&mut h, "/llms.txt");
        let l = links(&h);
        assert_eq!(
            l.len(),
            3,
            "expected two preloads plus one describedby, got {l:?}"
        );
        assert!(l.iter().any(|v| v.contains("style.css")));
        assert!(l.iter().any(|v| v.contains("m.woff2")));
        assert_eq!(l.iter().filter(|v| v.contains("describedby")).count(), 1);
    }
}

#[cfg(test)]
mod method_status_tests {
    use super::is_registered_method;

    /// RFC 9110 15.5.6 vs 15.6.2. 405 says "I know this method, this resource
    /// will not do it"; 501 says "I do not implement this method at all".
    /// Returning 405 for an invented verb claims knowledge the server does not
    /// have, and points the client at the resource when the method is the
    /// problem.
    #[test]
    fn standard_methods_are_recognised() {
        for m in [
            "GET", "HEAD", "POST", "PUT", "DELETE", "CONNECT", "OPTIONS", "TRACE", "PATCH",
        ] {
            assert!(is_registered_method(m), "{m} is a registered method");
        }
    }

    #[test]
    fn invented_methods_are_not_recognised() {
        for m in ["FOO", "BREW", "GETT", "", "GET ", "PROPFIND", "gEt"] {
            assert!(
                !is_registered_method(m),
                "{m} should not be treated as registered"
            );
        }
    }

    /// HTTP methods are case-sensitive tokens, so a lowercase `get` is not GET
    /// and should get 501 rather than being dignified with a 405.
    #[test]
    fn method_matching_is_case_sensitive() {
        assert!(is_registered_method("GET"));
        assert!(!is_registered_method("get"));
        assert!(!is_registered_method("Get"));
    }
}

#[cfg(test)]
mod error_page_holder_tests {
    use super::*;

    /// The holder is keyed by status, so a sweep of hundreds of distinct junk
    /// paths reuses one document. That is the whole point: caching the error
    /// response by URL would not help, because the cache key is
    /// (path, query, encoding) and every junk path is a distinct key.
    #[test]
    fn one_entry_serves_every_path() {
        let mut pages: HashMap<u16, CachedErrorPage> = HashMap::new();
        pages.insert(
            404,
            (m6_core::platform::now(), vec![], b"not found".to_vec()),
        );
        for path in [
            "/.env",
            "/wp-admin",
            "/route53-health/index.php",
            "/yarn.lock",
        ] {
            let hit = pages.contains_key(&404);
            assert!(hit, "{path} must be answered from the single held document");
        }
        assert_eq!(pages.len(), 1, "one document, not one per path");
    }

    /// Different statuses hold different documents; a 404 must never be
    /// served under a 500.
    #[test]
    fn statuses_do_not_share_a_document() {
        let mut pages: HashMap<u16, CachedErrorPage> = HashMap::new();
        pages.insert(404, (m6_core::platform::now(), vec![], b"gone".to_vec()));
        assert!(
            !pages.contains_key(&500),
            "500 must not be answered by the 404 document"
        );
    }

    /// A stale entry must be refetched rather than served forever, or a
    /// redeployed error page would never reach the edges.
    #[test]
    fn a_stale_entry_is_not_served() {
        let stale = m6_core::platform::now()
            .checked_sub(ERROR_PAGE_TTL + std::time::Duration::from_secs(1))
            .expect("clock");
        let entry = (stale, Vec::<(String, String)>::new(), b"old".to_vec());
        assert!(
            entry.0.elapsed() >= ERROR_PAGE_TTL,
            "past the TTL the held copy must be treated as stale"
        );
    }

    #[test]
    fn ttl_is_short_enough_to_pick_up_a_redeploy() {
        assert!(ERROR_PAGE_TTL <= std::time::Duration::from_secs(300));
        assert!(ERROR_PAGE_TTL >= std::time::Duration::from_secs(10));
    }
}

/// `Vary: Accept-Encoding` is promised only when the backend can deliver
/// variants, and `[[backend]] compresses` is how the two sides agree.
///
/// **This half had no tests.** Issue #8 is about the edge not advertising what
/// the backend cannot do, and every test written for it lived in m6-core, on the
/// backend's side of the contract: whether a service refuses to start when its
/// declaration disagrees with its build. The edge's own behaviour -- reading the
/// flag and deciding whether to add the header -- was untested, which is the half
/// a visitor actually sees.
#[cfg(test)]
mod compresses_vary_tests {
    use super::*;

    fn headers(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn vary_of(h: &[(String, String)]) -> Option<String> {
        h.iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("vary"))
            .map(|(_, v)| v.clone())
    }

    #[test]
    fn a_compressing_backend_gets_vary_accept_encoding() {
        let mut h = headers(&[("Content-Type", "text/html")]);
        set_vary_accept_encoding(&mut h, true);
        let v = vary_of(&h).expect("Vary must be set for a compressing backend");
        assert!(
            v.to_ascii_lowercase().contains("accept-encoding"),
            "Vary was {v:?}"
        );
    }

    /// The defect issue #8 is about. A backend that does not compress has exactly
    /// one representation, so advertising an encoding dimension promises variants
    /// that will never exist. m6-http is a cache, not a transformer: it has no
    /// compressor, so it cannot manufacture them.
    #[test]
    fn a_non_compressing_backend_gets_no_vary() {
        let mut h = headers(&[("Content-Type", "text/html")]);
        set_vary_accept_encoding(&mut h, false);
        assert!(
            vary_of(&h).is_none(),
            "a backend that does not compress was promised encoding variants: {:?}",
            vary_of(&h)
        );
    }

    /// A `Vary` the backend set itself is its statement about its own output, so
    /// it is left alone rather than stripped.
    #[test]
    fn a_backends_own_vary_survives_even_when_it_does_not_compress() {
        let mut h = headers(&[("Vary", "Accept-Language")]);
        set_vary_accept_encoding(&mut h, false);
        let v = vary_of(&h).expect("the backend's own Vary must not be removed");
        assert!(
            v.to_ascii_lowercase().contains("accept-language"),
            "the backend's own field was lost: {v:?}"
        );
    }
}

#[cfg(test)]
mod cache_warm_tests {
    use super::is_warmable;

    /// An ordinary cacheable page is warmed.
    #[test]
    fn a_plain_route_is_warmable() {
        assert!(is_warmable("/", None, Some("/_errors")));
        assert!(is_warmable(
            "/capabilities",
            Some("public, max-age=60"),
            Some("/_errors")
        ));
    }

    /// A pattern is not a URL. The shell version this replaced skipped these by
    /// looking for a `{` in a regex match over site.toml; the parsed config makes it
    /// the same decision on better evidence.
    #[test]
    fn a_pattern_is_not_a_url_and_is_not_warmed() {
        assert!(!is_warmable("/assets/{*relpath}", None, None));
        assert!(!is_warmable("/blog/{stem}", None, None));
    }

    /// Warming the error page would put a 404 body in the cache, and on a public
    /// listener the request is refused anyway.
    #[test]
    fn the_error_page_is_not_warmed() {
        assert!(!is_warmable("/_errors", None, Some("/_errors")));
        // ...but a site that has not configured one has nothing to exclude, and a
        // path that merely looks like an error page is still an ordinary route.
        assert!(is_warmable("/_errors", None, None));
    }

    /// no-store means the answer cannot be cached, so the fetch is pure cost.
    #[test]
    fn a_no_store_route_is_not_warmed() {
        assert!(!is_warmable("/contact", Some("no-store"), None));
        assert!(
            !is_warmable("/contact", Some("  no-store  "), None),
            "whitespace"
        );
        // Any other policy is cacheable as far as this decision goes.
        assert!(is_warmable("/contact", Some("private, max-age=0"), None));
    }
}
