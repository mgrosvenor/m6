// m6-http: reverse proxy, cache, and router.
//
// HTTP/3 over QUIC/UDP using quiche (sans-I/O) + single-threaded epoll.
// Standard POSIX UDP socket + epoll, accelerated transparently by
// OpenOnload/ExaSock at deployment. No async runtime, no threads.
#![allow(unused_imports, dead_code)]

use std::collections::HashMap;
use std::net::{UdpSocket, SocketAddr};
use std::os::unix::io::AsRawFd;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Instant;

use anyhow::Context;
use bytes::Bytes;
use quiche::h3::NameValue;
use rand::{thread_rng, RngCore};
use tracing::{debug, error, info, warn};

use m6_http_lib::analytics;
use m6_http_lib::auth;
use m6_http_lib::rate_limit::RateLimiter;
use m6_http_lib::cache::{Cache, CacheKey, CachedResponse, make_lookup_key, should_cache, strip_set_cookie, is_not_modified, not_modified_headers};
use m6_http_lib::stats::Stats;
use m6_http_lib::config::{self, Config};
use m6_http_lib::error::{self as error, ErrorMode};
use m6_http_lib::forward::{self, HttpRequest, HttpResponse};
use m6_http_lib::h2c_client::H2cClientPool;
use m6_http_lib::h2s_client::H2sTlsClientPool;
use m6_http_lib::pool::{self, PoolManager};
use m6_http_lib::poller::{Poller, Token};
use m6_http_lib::router::{self, RouteTable};
use m6_http_lib::watcher::{FsEvent, FsEventKind, FsWatcher};
use m6_http_lib::auth::PublicKey;
use m6_http_lib::http11::{Http11Listener, make_tls_server_config, RequestOutcome, H2cListener};
use m6_http_lib::hints;

// ── Constants ────────────────────────────────────────────────────────────────

const TOKEN_UDP: Token = Token(0);
const TOKEN_INOTIFY: Token = Token(1);
const TOKEN_TCP: Token = Token(2);
const TOKEN_H2C: Token = Token(3);
const TOKEN_H2C_CLIENT: Token = Token(4);
const TOKEN_H2S_CLIENT: Token = Token(5);
const MAX_DATAGRAM_SIZE: usize = 1350;

// ── Shutdown flags ───────────────────────────────────────────────────────────

static SHUTDOWN: AtomicBool = AtomicBool::new(false);
static SHUTDOWN_COUNT: AtomicUsize = AtomicUsize::new(0);

// ── Per-connection state ──────────────────────────────────────────────────────

struct QuicConn {
    conn: quiche::Connection,
    h3_conn: Option<quiche::h3::Connection>,
    /// Pending streams: stream_id -> accumulated request state
    pending: HashMap<u64, PendingRequest>,
    /// Responses awaiting flow-control credit, retried by drain_writable() the
    /// next time each stream reports writable.
    partial_responses: HashMap<u64, PendingH3Response>,
    /// Pending URL-backend requests for H3 streams. Keyed by H3 stream_id.
    pending_url: HashMap<u64, (std::sync::mpsc::Receiver<std::io::Result<forward::HttpResponse>>, forward::PendingUrlContext)>,
    client_addr: SocketAddr,
    /// When we last heard from this connection (for timeout tracking)
    last_active: Instant,
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

/// All mutable server state — owned by the event loop, no Arc/Mutex needed.
struct ServerState {
    config: Config,
    system_config_path: PathBuf,
    route_table: RouteTable,
    pool_manager: PoolManager,
    cache: Cache,
    public_key: Option<PublicKey>,
    invalidation_map: HashMap<String, Vec<String>>,
    error_mode: ErrorMode,
    stats: Stats,
    /// Cache entries queued to be fetched in the background — hint-driven
    /// prefetches and stale-while-revalidate refreshes both land here.
    prefetch_queue: std::collections::VecDeque<Refresh>,
    /// Background fetches dispatched to a URL backend and awaiting a reply.
    ///
    /// A real request parks its pending reply on its own connection, which
    /// the event loop then polls. A background fetch has no connection, so
    /// before this existed the returned receiver was simply dropped: on a
    /// cache node -- whose only backend is origin over h2c, i.e. always the
    /// async path -- every prefetch and every stale-while-revalidate refresh
    /// was dispatched and then silently discarded, so nothing was ever
    /// refilled. Origin was unaffected, its backends being unix sockets that
    /// complete synchronously inside handle_request.
    background_pending: Vec<(
        std::sync::mpsc::Receiver<std::io::Result<forward::HttpResponse>>,
        forward::PendingUrlContext,
    )>,
    /// Persistent non-blocking H2C outbound client pool.
    h2c_pool: H2cClientPool,
    /// Persistent non-blocking H2S (HTTP/2 over TLS) outbound client pool.
    h2s_pool: H2sTlsClientPool,
    /// Per-IP request throttle — general traffic, ahead of cache/routing.
    rate_limiter: RateLimiter,
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

// ── Signal handling ───────────────────────────────────────────────────────────

extern "C" fn handle_signal(_: libc::c_int) {
    let count = SHUTDOWN_COUNT.fetch_add(1, Ordering::SeqCst);
    if count >= 1 {
        std::process::exit(1); // second signal = immediate exit
    }
    SHUTDOWN.store(true, Ordering::SeqCst);
}

fn setup_signals() {
    use nix::sys::signal::{signal, SigHandler, Signal};
    unsafe {
        let _ = signal(Signal::SIGTERM, SigHandler::Handler(handle_signal));
        let _ = signal(Signal::SIGINT, SigHandler::Handler(handle_signal));
    }
}

// ── quiche TLS/QUIC config ────────────────────────────────────────────────────

fn make_quiche_config(server_config: &config::ServerConfig) -> anyhow::Result<quiche::Config> {
    let mut cfg = quiche::Config::new(quiche::PROTOCOL_VERSION)
        .context("quiche::Config::new")?;

    // QUIC is never started in redirect mode, so both are Some by the time we
    // get here; erroring rather than unwrapping keeps that a diagnosable
    // config failure instead of a panic if the call ever moves.
    let tls_cert = server_config.tls_cert.as_deref()
        .context("[server].tls_cert is required to serve QUIC")?;
    let tls_key = server_config.tls_key.as_deref()
        .context("[server].tls_key is required to serve QUIC")?;
    cfg.load_cert_chain_from_pem_file(tls_cert)
        .context("load cert chain")?;
    cfg.load_priv_key_from_pem_file(tls_key)
        .context("load private key")?;

    // ALPN: h3
    cfg.set_application_protos(quiche::h3::APPLICATION_PROTOCOL)
        .context("set alpn")?;

    // Disable GREASE: removes the extra unidirectional and in-band GREASE frames
    // that can confuse some client stacks (e.g. ngtcp2/nghttp3 in curl).
    cfg.grease(false);

    // Performance tuning
    cfg.set_max_idle_timeout(30_000);              // 30 s idle timeout
    cfg.set_max_recv_udp_payload_size(MAX_DATAGRAM_SIZE);
    cfg.set_max_send_udp_payload_size(MAX_DATAGRAM_SIZE);
    cfg.set_initial_max_data(10_000_000);
    cfg.set_initial_max_stream_data_bidi_local(1_000_000);
    cfg.set_initial_max_stream_data_bidi_remote(1_000_000);
    cfg.set_initial_max_stream_data_uni(1_000_000);
    cfg.set_initial_max_streams_bidi(100);
    cfg.set_initial_max_streams_uni(100);
    cfg.set_disable_active_migration(true);

    Ok(cfg)
}

// ── Event loop ────────────────────────────────────────────────────────────────

fn event_loop(
    udp: UdpSocket,
    mut tcp: Option<Http11Listener>,
    mut h2c: Option<H2cListener>,
    mut watcher: Option<FsWatcher>,
    state: &mut ServerState,
    quiche_config: &mut quiche::Config,
    log_handle: &m6_core::log::LogHandle,
) -> i32 {
    let poller = match Poller::new() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("poller init failed: {e}");
            return 2;
        }
    };

    if let Err(e) = poller.add(udp.as_raw_fd(), TOKEN_UDP) {
        eprintln!("poller add UDP failed: {e}");
        return 2;
    }
    if let Some(ref t) = tcp {
        let _ = poller.add(t.raw_fd(), TOKEN_TCP);
    }
    if let Some(ref h) = h2c {
        let _ = poller.add(h.raw_fd(), TOKEN_H2C);
    }
    if let Some(ref w) = watcher {
        if let Some(fd) = w.raw_fd() {
            let _ = poller.add(fd, TOKEN_INOTIFY);
        }
    }

    // On Linux: block SIGTERM/SIGINT at thread level so epoll_pwait delivers
    // them atomically, eliminating the TOCTOU race between checking SHUTDOWN
    // and sleeping in epoll_wait.
    #[cfg(target_os = "linux")]
    let sigmask_unblocked: libc::sigset_t = {
        let mut unblocked: libc::sigset_t = unsafe { std::mem::zeroed() };
        unsafe { libc::sigemptyset(&mut unblocked) };
        // Block SIGTERM and SIGINT at thread level
        let mut mask: libc::sigset_t = unsafe { std::mem::zeroed() };
        unsafe {
            libc::sigemptyset(&mut mask);
            libc::sigaddset(&mut mask, libc::SIGTERM);
            libc::sigaddset(&mut mask, libc::SIGINT);
            libc::pthread_sigmask(libc::SIG_BLOCK, &mask, std::ptr::null_mut());
        }
        unblocked
    };

    // Port for Alt-Svc advertisement: same port for both QUIC/H3 (UDP) and TCP.
    let quic_port = udp.local_addr().map(|a| a.port()).unwrap_or(8443);

    let mut connections: HashMap<Vec<u8>, QuicConn> = HashMap::new();
    // Maps the client's original Initial DCID (may be shorter than MAX_CONN_ID_LEN)
    // to the 20-byte SCID under which the connection is stored.  Needed because
    // quiche generates a 16-byte random Initial DCID on the client side, but
    // Header::from_slice for short-header 1-RTT packets always reads MAX_CONN_ID_LEN
    // (20) bytes.  Using a fresh 20-byte server SCID ensures all subsequent packets
    // (Handshake + 1-RTT) carry a 20-byte DCID that matches the stored key.
    let mut conn_id_map: HashMap<Vec<u8>, Vec<u8>> = HashMap::new();
    let mut recv_buf = vec![0u8; 65536];
    let mut ev_buf = [Token(0); 64];

    // When no filesystem watcher is available (e.g. macOS), rescan backend
    // socket globs every 2 seconds so new workers are picked up automatically.
    let rescan_interval = std::time::Duration::from_secs(2);
    let mut last_rescan = std::time::Instant::now()
        .checked_sub(rescan_interval)
        .unwrap_or_else(std::time::Instant::now);

    loop {
        // Compute the soonest connection timeout
        // Idle poll cap. Was 100ms originally — every async backend round trip
        // (every cache-node→origin fetch) was silently paying up to that much
        // in pure poll latency, since a response arriving mid-wait is only
        // noticed on the *next* tick. A "0ms only while a dispatch is actually
        // pending" version was tried and, on measurement, never actually
        // engaged — the flat cap below was doing all the work both times it
        // was tested. Rather than ship a conditional that doesn't do what it
        // claims, this is a flat cap: verified to cut cache-node→origin
        // latency from ~107ms to ~10-15ms, at the cost of ~100 wakeups/sec/
        // process at genuine idle (vs ~10/sec at the original 100ms) — a
        // real, small, quantified trade, not a hidden one.
        let timeout_ms = connections
            .values()
            .filter_map(|c| c.conn.timeout())
            .min()
            .map(|d| d.as_millis() as i32)
            .unwrap_or(100)
            .min(10);

        #[cfg(target_os = "linux")]
        let n = match poller.wait(&mut ev_buf, timeout_ms, Some(&sigmask_unblocked)) {
            Ok(n) => n,
            Err(e) => {
                error!(error = %e, "poller error");
                return 1;
            }
        };
        #[cfg(not(target_os = "linux"))]
        let n = match poller.wait(&mut ev_buf, timeout_ms, None) {
            Ok(n) => n,
            Err(e) => {
                error!(error = %e, "poller error");
                return 1;
            }
        };

        if SHUTDOWN.load(Ordering::Relaxed) {
            info!("shutdown signal received");
            break;
        }

        for i in 0..n {
            match ev_buf[i] {
                TOKEN_UDP => {
                    drain_udp(
                        &udp,
                        &mut recv_buf,
                        &mut connections,
                        &mut conn_id_map,
                        quiche_config,
                        state,
                        quic_port,
                    );
                }
                TOKEN_TCP => {
                    if let Some(ref mut t) = tcp {
                        t.accept_pending(&poller, TOKEN_TCP);
                    }
                }
                TOKEN_H2C => {
                    if let Some(ref mut h) = h2c {
                        h.accept_pending(&poller, TOKEN_H2C);
                    }
                }
                TOKEN_INOTIFY => {
                    if let Some(ref mut w) = watcher {
                        for event in w.read_events() {
                            handle_fs_event(&event, state, quiche_config, log_handle);
                        }
                    }
                }
                _ => {}
            }
        }

        // Drive HTTP/1.1 connections. Per-connection fds are registered with TOKEN_TCP
        // so this also runs on data-ready events, not only on the periodic tick.
        if let Some(ref mut t) = tcp {
            // Safety: on_request and on_response are called sequentially, never
            // concurrently, so the two `&mut state` aliases never overlap.
            let state_ptr = state as *mut ServerState;
            t.drive_all(
                |req, client_ip| {
                    let state = unsafe { &mut *state_ptr };
                    let ua = analytics::header(&req.headers, "user-agent");
                    if let Some(blocked) = check_rate_limit(state, client_ip, &req.path, ua) {
                        return blocked;
                    }
                    let enc_str = req.headers
                        .iter()
                        .find(|(k, _)| k.eq_ignore_ascii_case("accept-encoding"))
                        .map(|(_, v)| v.as_str())
                        .unwrap_or("");
                    let start = std::time::Instant::now();

                    // ── Cache lookup — check before forwarding to backend ──────────
                    // Routes with `require` are never served from cache: the
                    // key has no identity component, so a hit would bypass the
                    // auth check that runs later in handle_request.
                    let cacheable = !state.route_table.requires_auth(&req.path);
                    let mut key_buf = [0u8; 512];
                    let lookup_key =
                        make_lookup_key(&req.path, req.query.as_deref(), enc_str, &mut key_buf);
                    let looked_up = if cacheable { state.cache.lookup(lookup_key) } else { m6_http_lib::cache::Lookup::Miss };
                    // Serve stale immediately and refresh behind the request:
                    // making this visitor wait on an origin round trip is the
                    // thing the cache exists to avoid. Costs one stale serve.
                    // A stale serve logs as STALE, not HIT: it is a hit for
                    // latency purposes but the visitor got the previous
                    // generation of the content, and that difference has to be
                    // legible in the logs rather than hidden inside "HIT".
                    let cache_state = if matches!(looked_up, m6_http_lib::cache::Lookup::Stale(_)) { "STALE" } else { "HIT" };
                    if let m6_http_lib::cache::Lookup::Stale(_) = looked_up {
                        state.queue_refresh(Refresh {
                            path:  req.path.clone(),
                            query: req.query.clone(),
                            enc:   enc_str.to_string(),
                        });
                    }
                    if let m6_http_lib::cache::Lookup::Fresh(cached) | m6_http_lib::cache::Lookup::Stale(cached) = looked_up {
                        let elapsed_ns = start.elapsed().as_nanos() as u64;
                        state.stats.record(elapsed_ns, true, false);

                        if is_not_modified(&cached.headers, &req.headers) {
                            let mut headers = not_modified_headers(&cached.headers);
                            debug!(
                                path = %req.path,
                                status = 304,
                                version = "HTTP/1.1",
                                backend = "cache",
                                latency_ns = elapsed_ns,
                                cache_hit = true,
                                "request complete"
                            );
                            analytics::finish_response(
                                state.config.analytics.enabled, &mut headers, &req.headers,
                                &state.config.node.name, &req.path, 304, cache_state, client_ip, Some(elapsed_ns),
                            );
                            return RequestOutcome::Ready(304, headers, Vec::new(), "cache".to_string(), cached.hints.clone());
                        }

                        let mut headers: Vec<(String, String)> = (*cached.headers).clone();
                        // Add Link: preload headers to the 200 response for clients/CDNs
                        // that strip 1xx informational responses.
                        for url in cached.hints.iter() {
                            headers.push(("link".to_string(), hints::link_header(url)));
                        }
                        set_alt_svc(&mut headers, quic_port);
                        set_vary_accept_encoding(&mut headers);
                        debug!(
                            path = %req.path,
                            status = cached.status,
                            version = "HTTP/1.1",
                            backend = "cache",
                            latency_ns = elapsed_ns,
                            cache_hit = true,
                            "request complete"
                        );
                        analytics::finish_response(
                            state.config.analytics.enabled, &mut headers, &req.headers,
                            &state.config.node.name, &req.path, cached.status, cache_state, client_ip, Some(elapsed_ns),
                        );
                        return RequestOutcome::Ready(cached.status, headers, cached.body.to_vec(), "cache".to_string(), cached.hints.clone());
                    } // end cache hit

                    let mut outcome = handle_request(req, client_ip, enc_str, state, false);
                    if let RequestOutcome::Ready(_, ref mut headers, ..) = outcome {
                        set_alt_svc(headers, quic_port);
                    }
                    outcome
                },
                |http_result, ctx| {
                    let state = unsafe { &mut *state_ptr };
                    let (status, mut headers, body, backend_name, hints) =
                        finalize_url_response(http_result, ctx, quic_port, state);
                    // Add Link: preload headers to the response (fallback for proxies/CDNs).
                    for url in hints.iter() {
                        headers.push(("link".to_string(), hints::link_header(url)));
                    }
                    let elapsed_ns = ctx.start.elapsed().as_nanos() as u64;
                    let is_backend_error = status >= 500;
                    state.stats.record(elapsed_ns, false, is_backend_error);
                    debug!(
                        path = %ctx.req.path,
                        status,
                        version = "HTTP/1.1",
                        backend = %backend_name,
                        latency_ns = elapsed_ns,
                        cache_hit = false,
                        "request complete (async url backend)"
                    );
                    (status, headers, body, backend_name, hints)
                },
                &poller,
            );
        }

        // Drive H2C (HTTP/2 cleartext) connections.
        if let Some(ref mut h) = h2c {
            let state_ptr2 = state as *mut ServerState;
            h.drive_all(
                |req, client_ip| {
                    let state = unsafe { &mut *state_ptr2 };
                    let ua = analytics::header(&req.headers, "user-agent");
                    if let Some(blocked) = check_rate_limit(state, client_ip, &req.path, ua) {
                        return blocked;
                    }
                    let enc_str = req.headers
                        .iter()
                        .find(|(k, _)| k.eq_ignore_ascii_case("accept-encoding"))
                        .map(|(_, v)| v.as_str())
                        .unwrap_or("");
                    let start = std::time::Instant::now();

                    // ── Cache lookup — check before forwarding to backend ──────────
                    // Routes with `require` are never served from cache: the
                    // key has no identity component, so a hit would bypass the
                    // auth check that runs later in handle_request.
                    let cacheable = !state.route_table.requires_auth(&req.path);
                    let mut key_buf = [0u8; 512];
                    let lookup_key =
                        make_lookup_key(&req.path, req.query.as_deref(), enc_str, &mut key_buf);
                    let looked_up = if cacheable { state.cache.lookup(lookup_key) } else { m6_http_lib::cache::Lookup::Miss };
                    // Serve stale now, refresh behind the request — see the
                    // HTTP/1.1 path above for the reasoning.
                    // A stale serve logs as STALE, not HIT: it is a hit for
                    // latency purposes but the visitor got the previous
                    // generation of the content, and that difference has to be
                    // legible in the logs rather than hidden inside "HIT".
                    let cache_state = if matches!(looked_up, m6_http_lib::cache::Lookup::Stale(_)) { "STALE" } else { "HIT" };
                    if let m6_http_lib::cache::Lookup::Stale(_) = looked_up {
                        state.queue_refresh(Refresh {
                            path:  req.path.clone(),
                            query: req.query.clone(),
                            enc:   enc_str.to_string(),
                        });
                    }
                    if let m6_http_lib::cache::Lookup::Fresh(cached) | m6_http_lib::cache::Lookup::Stale(cached) = looked_up {
                        let elapsed_ns = start.elapsed().as_nanos() as u64;
                        state.stats.record(elapsed_ns, true, false);

                        if is_not_modified(&cached.headers, &req.headers) {
                            let mut headers = not_modified_headers(&cached.headers);
                            debug!(
                                path = %req.path,
                                status = 304,
                                version = "HTTP/2",
                                backend = "cache",
                                latency_ns = elapsed_ns,
                                cache_hit = true,
                                "request complete"
                            );
                            analytics::finish_response(
                                state.config.analytics.enabled, &mut headers, &req.headers,
                                &state.config.node.name, &req.path, 304, cache_state, client_ip, Some(elapsed_ns),
                            );
                            return RequestOutcome::Ready(304, headers, Vec::new(), "cache".to_string(), cached.hints.clone());
                        }

                        let mut headers: Vec<(String, String)> = (*cached.headers).clone();
                        for url in cached.hints.iter() {
                            headers.push(("link".to_string(), hints::link_header(url)));
                        }
                        set_alt_svc(&mut headers, quic_port);
                        set_vary_accept_encoding(&mut headers);
                        debug!(
                            path = %req.path,
                            status = cached.status,
                            version = "HTTP/2",
                            backend = "cache",
                            latency_ns = elapsed_ns,
                            cache_hit = true,
                            "request complete"
                        );
                        analytics::finish_response(
                            state.config.analytics.enabled, &mut headers, &req.headers,
                            &state.config.node.name, &req.path, cached.status, cache_state, client_ip, Some(elapsed_ns),
                        );
                        return RequestOutcome::Ready(cached.status, headers, cached.body.to_vec(), "cache".to_string(), cached.hints.clone());
                    } // end cache hit

                    let mut outcome = handle_request(req, client_ip, enc_str, state, false);
                    if let RequestOutcome::Ready(_, ref mut headers, ..) = outcome {
                        set_alt_svc(headers, quic_port);
                    }
                    outcome
                },
                |http_result, ctx| {
                    let state = unsafe { &mut *state_ptr2 };
                    let (status, mut headers, body, backend_name, hints) =
                        finalize_url_response(http_result, ctx, quic_port, state);
                    for url in hints.iter() {
                        headers.push(("link".to_string(), hints::link_header(url)));
                    }
                    let elapsed_ns = ctx.start.elapsed().as_nanos() as u64;
                    let is_backend_error = status >= 500;
                    state.stats.record(elapsed_ns, false, is_backend_error);
                    debug!(
                        path = %ctx.req.path,
                        status,
                        version = "HTTP/2",
                        backend = %backend_name,
                        latency_ns = elapsed_ns,
                        cache_hit = false,
                        "request complete (async url backend)"
                    );
                    (status, headers, body, backend_name, hints)
                },
                &poller,
            );
        }

        // Drive connection timeouts and flush pending sends
        flush_all(&udp, &mut connections);

        // Drive outbound H2C and H2S client connections.
        state.h2c_pool.drive_all(&poller, TOKEN_H2C_CLIENT);
        state.h2s_pool.drive_all(&poller, TOKEN_H2S_CLIENT);

        // Poll pending URL-backend responses for H3 streams.
        for qconn in connections.values_mut() {
            let sids: Vec<u64> = qconn.pending_url.keys().copied().collect();
            for sid in sids {
                use std::sync::mpsc::TryRecvError;
                // rx sends io::Result<HttpResponse>, so try_recv() gives Result<io::Result<HttpResponse>, TryRecvError>.
                let result: Option<std::io::Result<forward::HttpResponse>> = match qconn.pending_url.get(&sid) {
                    Some((rx, _)) => match rx.try_recv() {
                        Ok(r) => Some(r),  // r is already io::Result<HttpResponse>
                        Err(TryRecvError::Empty) => None,
                        Err(TryRecvError::Disconnected) => Some(Err(std::io::Error::new(
                            std::io::ErrorKind::BrokenPipe, "url backend thread died",
                        ))),
                    },
                    None => None,
                };
                if let Some(http_result) = result {
                    let (_, ctx) = qconn.pending_url.remove(&sid).unwrap();
                    let (status, mut resp_headers, body, _, hints) =
                        finalize_url_response(http_result, &ctx, quic_port, state);
                    // Add Link: preload headers.
                    for url in hints.iter() {
                        resp_headers.push(("link".to_string(), hints::link_header(url)));
                    }
                    if !hints.is_empty() {
                        send_h3_early_hints(sid, qconn, &hints);
                    }
                    send_h3_response(sid, qconn, status, &resp_headers, Bytes::from(body));
                }
            }
        }

        // Remove closed/timed-out connections
        connections.retain(|_, c| !c.conn.is_closed());

        // Emit periodic stats (cheap check every iteration: compares one Instant)
        state.stats.maybe_emit(state.pool_manager.total_active_members());

        // Complete any background fetch whose reply has arrived. Runs through
        // the same finalize_url_response as a real request, so the cache
        // insert, header handling and hint extraction are identical -- the
        // only difference is that the response body is discarded, there being
        // no client to send it to.
        if !state.background_pending.is_empty() {
            use std::sync::mpsc::TryRecvError;
            let mut done: Vec<(std::io::Result<forward::HttpResponse>, forward::PendingUrlContext)> = Vec::new();
            let mut idx = 0;
            while idx < state.background_pending.len() {
                let got = match state.background_pending[idx].0.try_recv() {
                    Ok(r) => Some(r),
                    Err(TryRecvError::Empty) => None,
                    // Backend thread died; take the entry so it cannot leak.
                    Err(TryRecvError::Disconnected) => Some(Err(std::io::Error::new(
                        std::io::ErrorKind::BrokenPipe, "url backend thread died",
                    ))),
                };
                match got {
                    Some(result) => {
                        let (_, ctx) = state.background_pending.remove(idx);
                        done.push((result, ctx));
                    }
                    None => idx += 1,
                }
            }
            for (result, ctx) in done {
                let path = ctx.req.path.clone();
                let _ = finalize_url_response(result, &ctx, quic_port, state);
                debug!(path = %path, "background fetch: cache filled (async)");
            }
        }

        // Drain one background fetch per loop iteration. Each is a synthetic
        // GET to a backend, serving two purposes: warming hinted assets into
        // the cache so they are ready when the browser requests them after a
        // 103, and refreshing entries that have gone stale (which were served
        // stale once, so this is what makes the *next* request fresh).
        if let Some(r) = state.prefetch_queue.pop_front() {
            let mut kbuf = [0u8; 512];
            let lk = make_lookup_key(&r.path, r.query.as_deref(), &r.enc, &mut kbuf);
            // `get` is fresh-only, so this skips entries some earlier fetch
            // already refreshed and proceeds for stale ones — which is exactly
            // the set still needing work.
            if state.cache.get(lk).is_none() {
                let synth = forward::HttpRequest {
                    method:  "GET".to_string(),
                    path:    r.path.clone(),
                    query:   r.query.clone(),
                    version: "HTTP/1.1".to_string(),
                    headers: vec![],
                    body:    vec![],
                };
                match handle_request(&synth, "127.0.0.1", &r.enc, state, true) {
                    // Socket backend: already completed and inserted inline.
                    RequestOutcome::Ready(..) => {
                        debug!(path = %r.path, enc = %r.enc, "background fetch: cache filled");
                    }
                    // URL backend: the reply lands later. Park it so the poll
                    // below can finish it; dropping it here is what made this
                    // a no-op on cache nodes.
                    RequestOutcome::Pending { rx, ctx } => {
                        if state.background_pending.len() < MAX_REFRESH_QUEUE {
                            state.background_pending.push((rx, ctx));
                        }
                    }
                }
            }
        }

        // Periodic rescan of backend socket globs to pick up newly started or
        // removed workers. On Linux, inotify also fires per-socket events, but
        // the rescan is a cheap belt-and-suspenders check for any missed events.
        if last_rescan.elapsed() >= rescan_interval {
            state.pool_manager.rescan_all();
            last_rescan = std::time::Instant::now();
        }
    }

    info!("m6-http shutdown complete");
    0
}

// ── UDP receive + quiche dispatch ─────────────────────────────────────────────

fn drain_udp(
    udp: &UdpSocket,
    recv_buf: &mut Vec<u8>,
    connections: &mut HashMap<Vec<u8>, QuicConn>,
    conn_id_map: &mut HashMap<Vec<u8>, Vec<u8>>,
    quiche_config: &mut quiche::Config,
    state: &mut ServerState,
    quic_port: u16,
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
                    pending_url: HashMap::new(),
                    client_addr: from,
                    last_active: Instant::now(),
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
        qconn.last_active = Instant::now();

        let recv_info = quiche::RecvInfo { from, to: local };
        if let Err(e) = qconn.conn.recv(pkt, recv_info) {
            warn!("conn.recv error: {}", e);
            continue;
        }

        // Establish H3 connection once QUIC handshake is complete
        if qconn.conn.is_established() && qconn.h3_conn.is_none() {
            let h3_config = match quiche::h3::Config::new() {
                Ok(c) => c,
                Err(e) => {
                    warn!("h3 Config::new error: {}", e);
                    continue;
                }
            };
            match quiche::h3::Connection::with_transport(&mut qconn.conn, &h3_config) {
                Ok(h3) => {
                    qconn.h3_conn = Some(h3);
                }
                Err(e) => {
                    warn!("h3 init error: {}", e);
                    continue;
                }
            }
        }

        // Process H3 events
        if qconn.h3_conn.is_some() {
            process_h3(qconn, udp, state, quic_port);
        }

        // Send any pending QUIC packets
        flush_conn(udp, qconn);
    }
}

// ── H3 event processing ───────────────────────────────────────────────────────

fn process_h3(qconn: &mut QuicConn, _udp: &UdpSocket, state: &mut ServerState, quic_port: u16) {
    // client_ip is NOT computed here — deferred to cache-miss path in handle_h3_request.

    loop {
        let h3 = match qconn.h3_conn.as_mut() {
            Some(h) => h,
            None => break,
        };

        match h3.poll(&mut qconn.conn) {
            Ok((stream_id, quiche::h3::Event::Headers { list, more_frames, .. })) => {
                let entry = qconn.pending.entry(stream_id).or_insert_with(|| PendingRequest {
                    headers: Vec::new(),
                    body: Vec::new(),
                    headers_done: false,
                });
                entry.headers = list;
                entry.headers_done = true;
                if !more_frames {
                    // No body — process immediately
                    handle_h3_request(stream_id, qconn, state, quic_port);
                    // After handle_h3_request qconn may be mutated; restart loop
                    continue;
                }
            }
            Ok((stream_id, quiche::h3::Event::Data)) => {
                // Need to borrow h3_conn mutably again
                let mut buf = [0u8; 65536];
                loop {
                    let h3 = match qconn.h3_conn.as_mut() {
                        Some(h) => h,
                        None => break,
                    };
                    match h3.recv_body(&mut qconn.conn, stream_id, &mut buf) {
                        Ok(0) => break,
                        Ok(read) => {
                            if let Some(req) = qconn.pending.get_mut(&stream_id) {
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
                    handle_h3_request(stream_id, qconn, state, quic_port);
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
    quic_port: u16,
) {
    let req = match qconn.pending.remove(&stream_id) {
        Some(r) => r,
        None => return,
    };

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
                    Some(q) => { path_bytes = &v[..q]; query_bytes = Some(&v[q + 1..]); }
                    None    => { path_bytes = v; }
                }
            }
            b":method"         => method_bytes = h.value(),
            b"accept-encoding" => enc_bytes    = h.value(),
            _ => {}
        }
    }

    let path_str = std::str::from_utf8(path_bytes).unwrap_or("/");
    let enc_str  = std::str::from_utf8(enc_bytes).unwrap_or("");
    let query_str = query_bytes.and_then(|q| std::str::from_utf8(q).ok());

    let start = Instant::now();

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
        if let Some(RequestOutcome::Ready(status, headers, body, _, _)) =
            check_rate_limit(state, &client_ip, path_str, ua_owned.as_deref())
        {
            send_h3_response(stream_id, qconn, status, &headers, Bytes::from(body));
            return;
        }
    }

    // ── Cache lookup — zero allocation ────────────────────────────────────────
    // Routes with `require` are never served from cache — see the HTTP/1.1
    // path for the reasoning.
    let cacheable = !state.route_table.requires_auth(path_str);

    let mut key_buf = [0u8; 512];
    let lookup_key = make_lookup_key(path_str, query_str, enc_str, &mut key_buf);

    let looked_up = if cacheable { state.cache.lookup(lookup_key) } else { m6_http_lib::cache::Lookup::Miss };
    // Serve stale now, refresh behind the request — see the HTTP/1.1 path for
    // the reasoning.
    // See the HTTP/1.1 path: a stale serve is logged distinctly from a hit.
    let cache_state = if matches!(looked_up, m6_http_lib::cache::Lookup::Stale(_)) { "STALE" } else { "HIT" };
    if let m6_http_lib::cache::Lookup::Stale(_) = looked_up {
        state.queue_refresh(Refresh {
            path:  path_str.to_string(),
            query: query_str.map(str::to_string),
            enc:   enc_str.to_string(),
        });
    }
    if let m6_http_lib::cache::Lookup::Fresh(cached) | m6_http_lib::cache::Lookup::Stale(cached) = looked_up {
        let elapsed_ns = start.elapsed().as_nanos() as u64;
        state.stats.record(elapsed_ns, true, false);

        if is_not_modified(&cached.headers, &req.headers) {
            let client_ip = qconn.client_addr.ip().to_string();
            let set_cookie = analytics::record(
                state.config.analytics.enabled, &req.headers,
                &state.config.node.name, path_str, 304, cache_state, &client_ip, Some(elapsed_ns),
            );
            let html = analytics::is_html_response(&cached.headers);
            let mut headers = not_modified_headers(&cached.headers);
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
            send_h3_response(stream_id, qconn, 304, &headers, Bytes::new());
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
            state.config.analytics.enabled, &req.headers,
            &state.config.node.name, path_str, cached.status, cache_state, &client_ip, Some(elapsed_ns),
        );

        if !cached.hints.is_empty() {
            send_h3_early_hints(stream_id, qconn, &cached.hints);
        }
        // Build headers with Link: preload / Vary / Set-Cookie appended as
        // needed. Vary is now always added on a cache hit (the cache key is
        // already segmented by encoding, so this just documents that to
        // downstream/shared caches), so this always takes the owned-Vec
        // branch rather than reusing `&cached.headers` unmodified.
        let mut headers_with_links: Vec<(String, String)> = (*cached.headers).clone();
        for url in cached.hints.iter() {
            headers_with_links.push(("link".to_string(), hints::link_header(url)));
        }
        set_vary_accept_encoding(&mut headers_with_links);
        set_alt_svc(&mut headers_with_links, quic_port);
        if let (Some(sc), true) = (set_cookie, analytics::is_html_response(&headers_with_links)) {
            headers_with_links.push(("Set-Cookie".to_string(), sc));
        }
        let resp_headers: &[(String, String)] = &headers_with_links;
        send_h3_response(stream_id, qconn, cached.status, resp_headers, cached.body);
        return;
    } // end cache hit

    // ── Phase 2: cache miss — allocate owned data for forwarding ──────────────
    let path    = path_str.to_string();
    let method  = std::str::from_utf8(method_bytes).unwrap_or("GET").to_string();
    let query   = query_str.map(str::to_string);
    let client_ip = qconn.client_addr.ip().to_string();

    let mut fwd_headers: Vec<(String, String)> = Vec::new();
    for h in &req.headers {
        let name = h.name();
        if name.starts_with(b":") { continue; }
        if let (Ok(k), Ok(v)) = (std::str::from_utf8(name), std::str::from_utf8(h.value())) {
            // Strip proxy-owned headers on ingress — see
            // `forward::UNTRUSTED_INBOUND`.
            if m6_http_lib::forward::is_untrusted_inbound(k) { continue; }
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

    match handle_request(&http_req, &client_ip, enc_str, state, false) {
        RequestOutcome::Ready(status, mut resp_headers, body, backend_name, hints) => {
            // Add Link: preload headers to the response (fallback for proxies/CDNs).
            for url in hints.iter() {
                resp_headers.push(("link".to_string(), hints::link_header(url)));
            }
            // Add alt-svc header.
            set_alt_svc(&mut resp_headers, quic_port);

            let elapsed_ns = start.elapsed().as_nanos() as u64;
            let is_backend_error = status >= 500;
            state.stats.record(elapsed_ns, false, is_backend_error);
            debug!(
                path = %path,
                status,
                version = "HTTP/3",
                backend = %backend_name,
                latency_ns = elapsed_ns,
                cache_hit = false,
                "request complete"
            );

            if !hints.is_empty() {
                send_h3_early_hints(stream_id, qconn, &hints);
            }
            send_h3_response(stream_id, qconn, status, &resp_headers, Bytes::from(body));
        }
        RequestOutcome::Pending { rx, ctx } => {
            // URL backend dispatched async — store and poll later.
            qconn.pending_url.insert(stream_id, (rx, ctx));
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

fn send_h3_early_hints(stream_id: u64, qconn: &mut QuicConn, hint_urls: &[String]) {
    let h3 = match qconn.h3_conn.as_mut() {
        Some(h) => h,
        None => return,
    };
    let link_values: Vec<String> = hint_urls.iter().map(|u| hints::link_header(u)).collect();
    let mut h3_headers: Vec<quiche::h3::Header> = Vec::with_capacity(link_values.len() + 1);
    h3_headers.push(quiche::h3::Header::new(b":status", b"103"));
    for lv in &link_values {
        h3_headers.push(quiche::h3::Header::new(b"link", lv.as_bytes()));
    }
    if let Err(e) = h3.send_response(&mut qconn.conn, stream_id, &h3_headers, false) {
        warn!("h3 early hints send error: {}", e);
    }
}

fn send_h3_response(
    stream_id: u64,
    qconn: &mut QuicConn,
    status: u16,
    headers: &[(String, String)],
    body: Bytes,
) {
    let h3 = match qconn.h3_conn.as_mut() {
        Some(h) => h,
        None => return,
    };

    // Stack-allocated numeric buffers — no String heap allocation.
    let mut status_buf = [0u8; 3];
    status_buf[0] = b'0' + (status / 100) as u8;
    status_buf[1] = b'0' + ((status / 10) % 10) as u8;
    status_buf[2] = b'0' + (status % 10) as u8;

    let mut cl_buf = [0u8; 20];
    let cl_bytes = write_decimal(body.len(), &mut cl_buf);

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
    if body.is_empty() {
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
            qconn.partial_responses.insert(stream_id, PendingH3Response::Headers(h3_headers, body));
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
                qconn.partial_responses.insert(stream_id, PendingH3Response::Body(body, written));
            }
            Err(quiche::h3::Error::Done) | Err(quiche::h3::Error::StreamBlocked) => {
                qconn.partial_responses.insert(stream_id, PendingH3Response::Body(body, 0));
            }
            Err(e) => warn!("h3 send_body error: {}", e),
        }
    }
}

// ── Routing / auth / forwarding ───────────────────────────────────────────────

fn handle_request(
    req: &forward::HttpRequest,
    client_ip: &str,
    content_encoding: &str,
    state: &mut ServerState,
    is_prefetch: bool,
) -> RequestOutcome {
    // Prefetch requests are synthetic (no real client, client_ip is a
    // placeholder) and their response is discarded by the caller — logging
    // them as analytics would mint a session/log a "MISS" line for a visit
    // that never happened, polluting request/session counts downstream.
    let analytics_enabled = state.config.analytics.enabled && !is_prefetch;

    // The custom-error render route takes `status`/`from` (and optional
    // `route`/`backend`/`detail`) straight from its query string and renders
    // them into the page — safe when `dispatch_custom_error_async`/
    // `apply_error_mode` build that query internally, but this route is also
    // a normal, routable path. A direct external request to it would forward
    // to the backend with client-controlled query params instead, letting
    // any caller spoof an arbitrary status/from pair. Refuse it exactly like
    // any other route miss.
    if let ErrorMode::Custom { path: error_path } = &state.error_mode {
        if req.path == *error_path {
            let (s, h, b, n) = apply_error_mode(404, req, client_ip, state, None);
            return RequestOutcome::Ready(s, h, b, n, std::sync::Arc::new(vec![]));
        }
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
                return RequestOutcome::Ready(301, headers, vec![], "redirect".to_string(), std::sync::Arc::new(vec![]));
            }
            // Prefer fetching the real custom error page over the local
            // socket-pool path (`apply_error_mode`/`forward_to_backend` only
            // know how to reach Unix-socket backends). On a node whose error
            // backend is itself a URL/H2C/H2S backend — every cache node,
            // whose only backend is the origin — dispatch that fetch async
            // and let it resolve through the normal Pending machinery.
            if let Some(outcome) = dispatch_custom_error_async(404, req, client_ip, state) {
                return outcome;
            }
            let (s, h, b, n) = apply_error_mode(404, req, client_ip, state, None);
            return RequestOutcome::Ready(s, h, b, n, std::sync::Arc::new(vec![]));
        }
    };

    // Verified JWT claims — populated during auth check, forwarded to backend.
    let mut verified_claims: Option<auth::Claims> = None;

    // Auth check — only if route has `require`
    if let Some(ref require) = route.require {
        if let Some(ref pk) = state.public_key {
            let auth_header = req
                .headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case("authorization"))
                .map(|(_, v)| v.as_str());
            let cookie_header_owned = auth::combined_cookie_header(&req.headers);
            let cookie_header = cookie_header_owned.as_deref();
            let accept_header = req
                .headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case("accept"))
                .map(|(_, v)| v.as_str());

            match auth::extract_token(auth_header, cookie_header) {
                None => {
                    warn!(path = %req.path, "auth: no token");
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
                        return RequestOutcome::Ready(302, headers, vec![], "auth".to_string(), std::sync::Arc::new(vec![]));
                    }
                    let ctx = error::ErrorContext { route: Some(route.path.clone()), backend: Some(route.backend.clone()), detail: Some("no token".to_string()) };
                    let (s, h, b, n) = apply_error_mode(401, req, client_ip, state, Some(&ctx));
                    return RequestOutcome::Ready(s, h, b, n, std::sync::Arc::new(vec![]));
                }
                Some(token) => match pk.verify(token) {
                    Err(e) => {
                        warn!(path = %req.path, error = %e, "auth: token verification failed");
                        let ctx = error::ErrorContext { route: Some(route.path.clone()), backend: Some(route.backend.clone()), detail: Some(e.to_string()) };
                        let (s, h, b, n) = apply_error_mode(401, req, client_ip, state, Some(&ctx));
                        return RequestOutcome::Ready(s, h, b, n, std::sync::Arc::new(vec![]));
                    }
                    Ok(claims) => {
                        if !auth::check_require(&claims, require) {
                            warn!(
                                path = %req.path,
                                require = %require,
                                "auth: insufficient claims"
                            );
                            let ctx = error::ErrorContext { route: Some(route.path.clone()), backend: Some(route.backend.clone()), detail: Some(format!("requires: {require}")) };
                            let (s, h, b, n) = apply_error_mode(403, req, client_ip, state, Some(&ctx));
                            return RequestOutcome::Ready(s, h, b, n, std::sync::Arc::new(vec![]));
                        }
                        // Forward verified claims to backend as X-Auth-Claims header
                        // (base64-encoded JSON so renderers can inspect them).
                        verified_claims = Some(claims);
                    }
                },
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
    let cacheable = route.require.is_none();
    let backend_name = route.backend.clone();

    // Check if URL backend — dispatch async.
    let original_host = req
        .headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(":authority"))
        .or_else(|| req.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case("host")))
        .map(|(_, v)| v.as_str())
        .unwrap_or("");
    let timeout = std::time::Duration::from_secs(state.config.server.backend_timeout_secs);

    if let Some((url, _tls_config, _)) = state.pool_manager.get_url_info(&backend_name) {
        let url = url.to_string();
        let ctx = forward::PendingUrlContext {
            req: req.clone(),
            client_ip: client_ip.to_string(),
            enc: content_encoding.to_string(),
            backend_name: backend_name.clone(),
            cacheable,
            is_prefetch,
            start: std::time::Instant::now(),
            error_status_override: None,
        };

        let rx = if url.starts_with("h2c://") {
            // Persistent non-blocking H2C client — event-loop managed.
            match state.h2c_pool.dispatch(&url, req, client_ip, original_host) {
                Ok(rx) => rx,
                Err(e) => {
                    warn!(backend = %backend_name, error = %e, "h2c dispatch failed");
                    let ctx = error::ErrorContext { route: Some(route.path.clone()), backend: Some(backend_name.clone()), detail: Some(e.to_string()) };
                    let (s, h, b, n) = apply_error_mode(502, req, client_ip, state, Some(&ctx));
                    return RequestOutcome::Ready(s, h, b, n, std::sync::Arc::new(vec![]));
                }
            }
        } else if url.starts_with("h2s://") {
            // Persistent non-blocking H2S (HTTP/2 over TLS) client — event-loop managed.
            match state.h2s_pool.dispatch(&url, req, client_ip, original_host, _tls_config) {
                Ok(rx) => rx,
                Err(e) => {
                    warn!(backend = %backend_name, error = %e, "h2s dispatch failed");
                    let ctx = error::ErrorContext { route: Some(route.path.clone()), backend: Some(backend_name.clone()), detail: Some(e.to_string()) };
                    let (s, h, b, n) = apply_error_mode(502, req, client_ip, state, Some(&ctx));
                    return RequestOutcome::Ready(s, h, b, n, std::sync::Arc::new(vec![]));
                }
            }
        } else {
            forward::dispatch_url_request(
                url, req.clone(), client_ip.to_string(), original_host.to_string(),
                Some(timeout), _tls_config,
            )
        };
        return RequestOutcome::Pending { rx, ctx };
    }

    // Socket backend — synchronous (local, sub-ms).
    let backend_start = std::time::Instant::now();
    let (status, mut resp_headers, body, conn_err) =
        match forward_to_backend(req, &backend_name, client_ip, state) {
            Ok(http_resp) => {
                if cacheable && should_cache(http_resp.status, &http_resp.headers) {
                    // Extract early-hints from the response body (HTML only).
                    // This is done ONLY on the cache-miss path to keep the
                    // cache-hit path at <10 µs.
                    let content_type = http_resp.headers.iter()
                        .find(|(k, _)| k.eq_ignore_ascii_case("content-type"))
                        .map(|(_, v)| v.as_str())
                        .unwrap_or("");
                    let hint_paths = hints::extract_hints(&http_resp.body, content_type);
                    // Queue any hints not already in the cache for prefetch.
                    for hp in &hint_paths {
                        let mut kbuf = [0u8; 512];
                        let lk = make_lookup_key(hp, None, "", &mut kbuf);
                        if state.cache.get(lk).is_none() {
                            state.queue_refresh(Refresh {
                                path:  hp.clone(),
                                query: None,
                                enc:   String::new(),
                            });
                        }
                    }
                    let key = CacheKey::new(&req.path, req.query.as_deref(), content_encoding);
                    state.cache.insert(
                        key,
                        CachedResponse {
                            status:  http_resp.status,
                            // strip_set_cookie: at this point in the socket-
                            // backend path http_resp.headers is the backend's
                            // (m6-html/m6-file/render-*) raw response, before
                            // this request's own analytics Set-Cookie is even
                            // added — so today this is a defensive no-op here.
                            // Applied anyway (matching the async URL-backend
                            // insert below, where it isn't a no-op) so cache
                            // correctness doesn't depend on which code path a
                            // future Set-Cookie-emitting backend happens to use.
                            headers: std::sync::Arc::new(strip_set_cookie(&http_resp.headers)),
                            body:    Bytes::from(http_resp.body.clone()),
                            hints:   std::sync::Arc::new(hint_paths),
                        },
                    );
                }
                (http_resp.status, http_resp.headers, http_resp.body, None::<String>)
            }
            Err(e) => {
                warn!(backend = %backend_name, error = %e, "backend error");
                (502u16, vec![], vec![], Some(e))
            }
        };

    // If the response is an error (4xx/5xx) and not already an error response,
    // apply the error mode: status, internal, or custom.
    if status >= 400 {
        let detail = conn_err.unwrap_or_else(|| format!("{backend_name} returned {status}"));
        let ctx = error::ErrorContext { route: Some(route.path.clone()), backend: Some(backend_name.clone()), detail: Some(detail) };
        let (s, mut h, b, n) = apply_error_mode(status, req, client_ip, state, Some(&ctx));
        // Bug fix: this early return used to skip analytics for every
        // backend-returned error status uniformly — unlike its async sibling
        // (finalize_url_response), which deliberately logs a backend-returned
        // 4xx/5xx and only skips for a genuine connection failure. No
        // equivalent reasoning applied here; it looked like an accidental
        // omission from copy-pasted control flow, not intent — a plain
        // backend 404 should be visible in analytics like any other request.
        let backend_ns = backend_start.elapsed().as_nanos() as u64;
        analytics::finish_response(
            analytics_enabled, &mut h, &req.headers,
            &state.config.node.name, &req.path, s, "MISS", client_ip, Some(backend_ns),
        );
        return RequestOutcome::Ready(s, h, b, n, std::sync::Arc::new(vec![]));
    }

    // Retrieve hints from cache (populated above if cacheable).
    let hints_arc = {
        let mut kbuf = [0u8; 512];
        let lk = make_lookup_key(&req.path, req.query.as_deref(), content_encoding, &mut kbuf);
        state.cache.get(lk)
            .map(|c| c.hints.clone())
            .unwrap_or_else(|| std::sync::Arc::new(vec![]))
    };

    let backend_ns = backend_start.elapsed().as_nanos() as u64;
    analytics::finish_response(
        analytics_enabled, &mut resp_headers, &req.headers,
        &state.config.node.name, &req.path, status, "MISS", client_ip, Some(backend_ns),
    );

    RequestOutcome::Ready(status, resp_headers, body, backend_name, hints_arc)
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
        analytics::log_rate_limited(&state.config.node.name, path, client_ip, user_agent);
    }
    let headers = vec![
        ("Content-Type".to_string(), "text/plain; charset=utf-8".to_string()),
        ("Retry-After".to_string(), "60".to_string()),
    ];
    Some(RequestOutcome::Ready(
        429,
        headers,
        b"Too Many Requests".to_vec(),
        "rate-limit".to_string(),
        std::sync::Arc::new(vec![]),
    ))
}

/// Try to fetch the configured `[errors] mode = "custom"` error page via an
/// async URL/H2C/H2S backend dispatch, for cases where `apply_error_mode`'s
/// synchronous `forward_to_backend` can't reach the error backend (it only
/// knows how to reach Unix-socket pools; a cache node's only backend is the
/// origin, an H2C URL backend).
///
/// Returns `None` when there's no custom error path configured, the request
/// already targets it (anti-recursion), or its backend turns out to be an
/// ordinary socket pool anyway — the caller should fall back to the existing
/// synchronous `apply_error_mode` in every `None` case.
fn dispatch_custom_error_async(
    status: u16,
    req: &forward::HttpRequest,
    client_ip: &str,
    state: &mut ServerState,
) -> Option<RequestOutcome> {
    let error_path = match &state.error_mode {
        ErrorMode::Custom { path } => path.clone(),
        _ => return None,
    };
    if req.path == error_path {
        return None;
    }
    let backend_name = state.route_table.at(&error_path)?.backend.clone();
    let (url, tls_config, _) = state.pool_manager.get_url_info(&backend_name)?;
    let url = url.to_string();

    let original_host = req.headers.iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(":authority"))
        .or_else(|| req.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case("host")))
        .map(|(_, v)| v.as_str())
        .unwrap_or("");
    let timeout = std::time::Duration::from_secs(state.config.server.backend_timeout_secs);

    let error_query = format!("status={}&from={}", status, urlencoded(&req.path));
    let error_req = forward::HttpRequest {
        method: "GET".to_string(),
        path: error_path,
        query: Some(error_query),
        version: req.version.clone(),
        headers: vec![("Host".to_string(), original_host.to_string())],
        body: vec![],
    };

    let ctx = forward::PendingUrlContext {
        req: req.clone(),
        client_ip: client_ip.to_string(),
        enc: String::new(),
        backend_name: backend_name.clone(),
        cacheable: false,
        // A custom-error-page fetch on behalf of a real client, so it is not a
        // background prefetch and its analytics line must still be written.
        is_prefetch: false,
        start: std::time::Instant::now(),
        error_status_override: Some(status),
    };

    let rx = if url.starts_with("h2c://") {
        state.h2c_pool.dispatch(&url, &error_req, client_ip, original_host).ok()?
    } else if url.starts_with("h2s://") {
        state.h2s_pool.dispatch(&url, &error_req, client_ip, original_host, tls_config).ok()?
    } else {
        forward::dispatch_url_request(
            url, error_req, client_ip.to_string(), original_host.to_string(),
            Some(timeout), tls_config,
        )
    };
    Some(RequestOutcome::Pending { rx, ctx })
}

/// Apply the configured error mode for a given status code.
///
/// For `Custom` mode, performs an internal GET to `<errors.path>?status=N&from=/original-path`.
/// Falls back to `Internal` mode if the fetch fails or the request is already to the error path.
fn apply_error_mode(
    status: u16,
    req: &forward::HttpRequest,
    client_ip: &str,
    state: &mut ServerState,
    ctx: Option<&error::ErrorContext>,
) -> (u16, Vec<(String, String)>, Vec<u8>, String) {
    let verbose = state.config.errors.verbose_fallback;
    match &state.error_mode {
        ErrorMode::Status => {
            (status, vec![("Content-Type".to_string(), "text/plain".to_string())], vec![], "error".to_string())
        }
        ErrorMode::Internal => {
            let reason = error::status_reason(status);
            let body = error::internal_error_html(status, reason, verbose, &req.path, ctx);
            (
                status,
                vec![("Content-Type".to_string(), "text/html; charset=utf-8".to_string())],
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
                return (
                    status,
                    vec![("Content-Type".to_string(), "text/html; charset=utf-8".to_string())],
                    body,
                    "error".to_string(),
                );
            }

            // Build error page request: GET <error_path>?status=N&from=/original-path[&route=...&backend=...&detail=...]
            let mut error_query = format!("status={}&from={}", status, urlencoded(&req.path));
            if let Some(c) = ctx {
                if let Some(ref r) = c.route   { error_query.push_str(&format!("&route={}",   urlencoded(r))); }
                if let Some(ref b) = c.backend { error_query.push_str(&format!("&backend={}", urlencoded(b))); }
                if let Some(ref d) = c.detail  { error_query.push_str(&format!("&detail={}",  urlencoded(d))); }
            }
            let error_req = forward::HttpRequest {
                method: "GET".to_string(),
                path: error_path.clone(),
                query: Some(error_query),
                version: "HTTP/3".to_string(),
                headers: vec![
                    ("Host".to_string(), req.headers
                        .iter()
                        .find(|(k, _)| k.eq_ignore_ascii_case("host"))
                        .map(|(_, v)| v.clone())
                        .unwrap_or_default()),
                ],
                body: vec![],
            };

            // Look up the error backend via the route table.
            let error_backend = match state.route_table.at(&error_path) {
                Some(entry) => entry.backend.clone(),
                None => {
                    warn!(error_path = %error_path, "custom error: no route for error path, falling back to internal");
                    let reason = error::status_reason(status);
                    let body = error::internal_error_html(status, reason, verbose, &req.path, ctx);
                    return (
                        status,
                        vec![("Content-Type".to_string(), "text/html; charset=utf-8".to_string())],
                        body,
                        "error".to_string(),
                    );
                }
            };

            match forward_to_backend(&error_req, &error_backend, client_ip, state) {
                Ok(err_resp) => {
                    // Return the error page body with the ORIGINAL status code.
                    (status, err_resp.headers, err_resp.body, "error".to_string())
                }
                Err(e) => {
                    warn!(error = %e, "custom error: error page fetch failed, falling back to internal");
                    let reason = error::status_reason(status);
                    let body = error::internal_error_html(status, reason, verbose, &req.path, ctx);
                    (
                        status,
                        vec![("Content-Type".to_string(), "text/html; charset=utf-8".to_string())],
                        body,
                        "error".to_string(),
                    )
                }
            }
        }
    }
}

fn forward_to_backend(
    req: &forward::HttpRequest,
    backend_name: &str,
    client_ip: &str,
    state: &mut ServerState,
) -> Result<HttpResponse, String> {
    let original_host = req
        .headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(":authority"))
        .or_else(|| {
            req.headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case("host"))
        })
        .map(|(_, v)| v.as_str())
        .unwrap_or("");

    let timeout = std::time::Duration::from_secs(state.config.server.backend_timeout_secs);

    if let Some(pool) = state.pool_manager.get_pool_mut(backend_name) {
        match pool.pick_socket() {
            Ok((socket_path, member_idx)) => {
                match forward::forward_request_timeout(
                    &socket_path,
                    req,
                    client_ip,
                    original_host,
                    Some(timeout),
                ) {
                    Ok(resp) => {
                        pool.release(member_idx);
                        Ok(resp)
                    }
                    Err(e) => {
                        pool.mark_failed(member_idx);
                        Err(e.to_string())
                    }
                }
            }
            Err(pool::PoolError::Empty) => Err("pool empty".to_string()),
            Err(pool::PoolError::ConnectFailed(e)) => Err(e.to_string()),
        }
    } else {
        Err(format!("unknown backend: {}", backend_name))
    }
}

/// Ensure exactly one `alt-svc` header advertising HTTP/3 is present, replacing
/// any pre-existing one instead of appending a duplicate. A pre-existing entry
/// shows up whenever the response body already passed through another m6-http
/// instance (a cache node's backend is the origin, and the origin already
/// advertised its own alt-svc before the cache node forwards or caches it).
fn set_alt_svc(headers: &mut Vec<(String, String)>, quic_port: u16) {
    headers.retain(|(k, _)| !k.eq_ignore_ascii_case("alt-svc"));
    headers.push(("alt-svc".to_string(), format!("h3=\":{quic_port}\"; ma=86400")));
}

/// Same duplication hazard as `set_alt_svc`: a cache node's cached headers
/// are whatever its own upstream (origin) sent, and origin adds this same
/// header on its own cache hits — so a cache node replaying a cache hit of
/// its own would otherwise double it up.
fn set_vary_accept_encoding(headers: &mut Vec<(String, String)>) {
    headers.retain(|(k, _)| !k.eq_ignore_ascii_case("vary"));
    headers.push(("vary".to_string(), "Accept-Encoding".to_string()));
}

/// Called when a URL-backend I/O thread returns its result.  Handles cache
/// insertion, hints extraction, alt-svc injection, and error mode application.
fn finalize_url_response(
    http_result: std::io::Result<forward::HttpResponse>,
    ctx:         &forward::PendingUrlContext,
    quic_port:   u16,
    state:       &mut ServerState,
) -> (u16, Vec<(String, String)>, Vec<u8>, String, std::sync::Arc<Vec<String>>) {
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
        // Bug fix: this dispatch fetches a real error page for a real
        // client's real failing request (ctx.req/ctx.client_ip belong to
        // them, not a discarded synthetic probe) — skipping analytics here
        // meant any node running `[errors] mode = "custom"` had zero
        // visibility into who was hitting error pages, which is the entire
        // point of that feature.
        let latency_ns = ctx.start.elapsed().as_nanos() as u64;
        return match http_result {
            Ok(http_resp) => {
                // finish_proxied_response: the error-page backend can itself
                // be another m6-http instance (a cache node fetching origin's
                // rendered error page), same reasoning as the main MISS tail.
                let mut headers = http_resp.headers;
                analytics::finish_proxied_response(
                    analytics_on, &mut headers, &req.headers,
                    &state.config.node.name, &req.path, original_status, "MISS", &ctx.client_ip, Some(latency_ns),
                );
                (original_status, headers, http_resp.body, "error".to_string(), std::sync::Arc::new(vec![]))
            }
            Err(e) => {
                warn!(backend = %ctx.backend_name, error = %e, "custom error page fetch failed (async), falling back to internal");
                let reason = error::status_reason(original_status);
                let body = error::internal_error_html(original_status, reason, state.config.errors.verbose_fallback, &req.path, None);
                let mut headers = vec![("Content-Type".to_string(), "text/html; charset=utf-8".to_string())];
                analytics::finish_response(
                    analytics_on, &mut headers, &req.headers,
                    &state.config.node.name, &req.path, original_status, "MISS", &ctx.client_ip, Some(latency_ns),
                );
                (original_status, headers, body, "error".to_string(), std::sync::Arc::new(vec![]))
            }
        };
    }

    let (status, resp_headers, body, used_backend, is_connection_failure) = match http_result {
        Ok(http_resp) => {
            if ctx.cacheable && should_cache(http_resp.status, &http_resp.headers) {
                let content_type = http_resp.headers.iter()
                    .find(|(k, _)| k.eq_ignore_ascii_case("content-type"))
                    .map(|(_, v)| v.as_str()).unwrap_or("");
                let hint_paths = hints::extract_hints(&http_resp.body, content_type);
                for hp in &hint_paths {
                    let mut kbuf = [0u8; 512];
                    let lk = make_lookup_key(hp, None, "", &mut kbuf);
                    if state.cache.get(lk).is_none() {
                        state.queue_refresh(Refresh {
                            path:  hp.clone(),
                            query: None,
                            enc:   String::new(),
                        });
                    }
                }
                let key = CacheKey::new(&req.path, req.query.as_deref(), enc);
                // strip_set_cookie matters here specifically: this backend
                // can itself be another m6-http instance (a cache node's
                // only backend is the origin), whose response may already
                // carry a Set-Cookie IT minted for this one request. Caching
                // it verbatim would replay that one visitor's session cookie
                // to every future visitor who hits this same cache entry —
                // the content is shared and cacheable, the cookie isn't.
                state.cache.insert(key, CachedResponse {
                    status:  http_resp.status,
                    headers: std::sync::Arc::new(strip_set_cookie(&http_resp.headers)),
                    body:    Bytes::from(http_resp.body.clone()),
                    hints:   std::sync::Arc::new(hint_paths),
                });
            }
            (http_resp.status, http_resp.headers, http_resp.body, ctx.backend_name.clone(), false)
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
        let err_ctx = error::ErrorContext { route: None, backend: Some(ctx.backend_name.clone()), detail: Some(format!("backend returned {status}")) };
        let (s, mut h, b, n) = apply_error_mode(status, req, &ctx.client_ip, state, Some(&err_ctx));
        // Bug fix: this is the final 502/504 a real client actually receives
        // when the backend is unreachable — arguably the single most
        // operationally important case to have in analytics (a backend-down
        // incident should show up in the request/status log, not just an
        // operational warn!()), and the pre-fix code skipped it unconditionally.
        let latency_ns = ctx.start.elapsed().as_nanos() as u64;
        analytics::finish_response(
            analytics_on, &mut h, &req.headers,
            &state.config.node.name, &req.path, s, "MISS", &ctx.client_ip, Some(latency_ns),
        );
        return (s, h, b, n, std::sync::Arc::new(vec![]));
    }

    // Retrieve hints from cache (populated above if cacheable).
    let hints_arc = {
        let mut kbuf = [0u8; 512];
        let lk = make_lookup_key(&req.path, req.query.as_deref(), enc, &mut kbuf);
        state.cache.get(lk).map(|c| c.hints.clone())
            .unwrap_or_else(|| std::sync::Arc::new(vec![]))
    };

    let mut headers_with_altsvc = resp_headers;
    set_alt_svc(&mut headers_with_altsvc, quic_port);

    // finish_proxied_response, not finish_response: this backend may itself
    // be another m6-http instance (a cache node's only backend is the
    // origin), whose response can already carry a _m6sid the origin just
    // minted for this same request — see the doc comment on
    // finish_proxied_response for why that matters.
    let latency_ns = ctx.start.elapsed().as_nanos() as u64;
    analytics::finish_proxied_response(
        analytics_on, &mut headers_with_altsvc, &req.headers,
        &state.config.node.name, &req.path, status, "MISS", &ctx.client_ip, Some(latency_ns),
    );

    (status, headers_with_altsvc, body, used_backend, hints_arc)
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
    if qconn.partial_responses.is_empty() { return; }
    let h3 = match qconn.h3_conn.as_mut() { Some(h) => h, None => return };
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
                                    qconn.partial_responses.insert(stream_id, PendingH3Response::Body(body, written));
                                }
                                Err(quiche::h3::Error::Done) | Err(quiche::h3::Error::StreamBlocked) => {
                                    qconn.partial_responses.insert(stream_id, PendingH3Response::Body(body, 0));
                                }
                                Err(e) => warn!("h3 drain_writable send_body error: {}", e),
                            }
                        }
                    }
                    Err(quiche::h3::Error::StreamBlocked) => {
                        // Still no credit — put it back for the next writable report.
                        qconn.partial_responses.insert(stream_id, PendingH3Response::Headers(headers, body));
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
                            qconn.partial_responses.insert(stream_id, PendingH3Response::Body(body, new_offset));
                        }
                    }
                    Err(quiche::h3::Error::Done) | Err(quiche::h3::Error::StreamBlocked) => {
                        qconn.partial_responses.insert(stream_id, PendingH3Response::Body(body, offset));
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

// ── Filesystem event handling ─────────────────────────────────────────────────

fn handle_fs_event(
    event: &FsEvent,
    state: &mut ServerState,
    quiche_config: &mut quiche::Config,
    log_handle: &m6_core::log::LogHandle,
) {
    match event.kind {
        FsEventKind::SocketCreated => {
            state.pool_manager.socket_appeared(&event.path);
        }
        FsEventKind::SocketDeleted => {
            state.pool_manager.socket_disappeared(&event.path);
        }
        FsEventKind::SiteTomlChanged => {
            handle_site_reload(state, log_handle);
        }
        FsEventKind::TlsCertChanged => {
            handle_tls_reload(state, quiche_config);
        }
    }
}

fn handle_site_reload(state: &mut ServerState, log_handle: &m6_core::log::LogHandle) {
    info!("config reload: site.toml changed");

    match config::load(&state.config.site_dir, &state.system_config_path) {
        Ok(new_config) => {
            // A reload is the likeliest moment for an inert `cache` key to be
            // introduced — someone editing site.toml on the node.
            config::warn_ignored_route_cache_keys(&new_config);
            let new_route_table = match RouteTable::from_config(&new_config) {
                Ok(t) => t,
                Err(e) => {
                    warn!(error = %e, "config reload: route table error");
                    return;
                }
            };
            let new_pools = PoolManager::from_config(&new_config.backends);
            let new_inv_map = router::build_invalidation_map(&new_config);
            let new_error_mode = ErrorMode::from_config(&new_config.errors);

            let new_public_key = match &new_config.auth {
                Some(auth_cfg) => {
                    let key_path = config::resolve_path(&new_config.site_dir, &auth_cfg.public_key);
                    match PublicKey::from_pem_file(&key_path) {
                        Ok(k) => Some(k),
                        Err(e) => {
                            warn!(error = %e, "config reload: auth key load failed, keeping current key");
                            state.public_key.take()
                        }
                    }
                }
                None => None,
            };

            state.route_table = new_route_table;
            state.pool_manager = new_pools;
            state.invalidation_map = new_inv_map;
            state.error_mode = new_error_mode;
            state.public_key = new_public_key;
            state.config = new_config;
            m6_http_lib::security::configure(&state.config.security);
            state.cache.clear();

            log_handle.reload(&state.config.log.format, &state.config.log.level);
            info!("config reload: complete, cache cleared");
        }
        Err(e) => {
            warn!(error = %e, "config reload: failed, keeping current config");
        }
    }
}

fn handle_tls_reload(state: &ServerState, quiche_config: &mut quiche::Config) {
    info!("TLS config reload: cert/key file changed");
    match make_quiche_config(&state.config.server) {
        Ok(new_cfg) => {
            *quiche_config = new_cfg;
            info!("TLS config reloaded");
        }
        Err(e) => {
            warn!(error = %e, "TLS config reload failed, keeping old config");
        }
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
    // rustls requires an explicit CryptoProvider when multiple are available
    // (ring + aws-lc-rs both get pulled in transitively). Install ring first.
    rustls::crypto::ring::default_provider().install_default().ok();
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
            eprintln!("config error: {}", e);
            return 2;
        }
    };

    // CLI --log-level overrides site.toml [log].level; format always comes from config.
    let log_level = cli.log_level.as_deref().unwrap_or(&config.log.level);
    let analytics_path = config.analytics.enabled
        .then(|| PathBuf::from(&config.analytics.log_path));
    let log_handle = match m6_core::log::init_with_analytics(&config.log.format, log_level, analytics_path.as_deref()) {
        Ok(g) => g,
        Err(e) => {
            eprintln!("logging init error: {}", e);
            return 1;
        }
    };

    config::warn_system_config_extra_keys(&cli.system_config);
    config::warn_ignored_route_cache_keys(&config);

    // --dump-config
    //
    // Ahead of redirect mode below, because --dump-config must never bind a
    // socket: it is how a deploy validates a config before cutting a node over
    // to it, and a redirect config that started a listener instead of printing
    // would hold :80 on the very node the deploy was still checking.
    if cli.dump_config {
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
        let key_path = config::resolve_path(&config.site_dir, &auth_cfg.public_key);
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

    // Build pool manager
    let pool_manager = PoolManager::from_config(&config.backends);

    // Build invalidation map
    let invalidation_map = router::build_invalidation_map(&config);

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

    // Setup signals
    setup_signals();

    // Setup filesystem watcher
    let watcher = match FsWatcher::new(&config) {
        Ok(w) => Some(w),
        Err(e) => {
            warn!(error = %e, "fs watcher setup failed, hot reload disabled");
            None
        }
    };

    // Build TLS config for HTTP/1.1 and bind TCP listener on the same port.
    // Both are Some on this path: redirect mode returned above, and every other
    // mode has them enforced as required keys by config::load().
    let (tls_cert, tls_key) = match (&config.server.tls_cert, &config.server.tls_key) {
        (Some(c), Some(k)) => (c, k),
        _ => {
            eprintln!("config error: [server].tls_cert and [server].tls_key are required to serve TLS");
            return 2;
        }
    };
    let tcp_listener = match make_tls_server_config(tls_cert, tls_key) {
        Ok(tls_cfg) => match Http11Listener::bind(&config.server.bind, tls_cfg) {
            Ok(l) => {
                info!(bind = %config.server.bind, "HTTP/1.1 over TLS listener started");
                Some(l)
            }
            Err(e) => {
                warn!(error = %e, "HTTP/1.1 TCP listener bind failed, HTTP/1.1 disabled");
                None
            }
        },
        Err(e) => {
            warn!(error = %e, "HTTP/1.1 TLS config failed, HTTP/1.1 disabled");
            None
        }
    };

    let h2c_listener = if let Some(ref h2c_bind) = config.server.h2c_bind {
        match H2cListener::bind(h2c_bind) {
            Ok(l) => {
                info!(bind = %h2c_bind, "H2C (HTTP/2 cleartext) listener started");
                Some(l)
            }
            Err(e) => {
                warn!(error = %e, "H2C listener bind failed, H2C disabled");
                None
            }
        }
    } else {
        None
    };

    info!(
        bind = %config.server.bind,
        h2c_bind = ?config.server.h2c_bind,
        site = %config.site.name,
        "m6-http started"
    );

    let mut state = ServerState {
        config,
        system_config_path: cli.system_config.clone(),
        route_table,
        pool_manager,
        cache: Cache::new(),
        public_key,
        invalidation_map,
        error_mode,
        stats: Stats::new(),
        prefetch_queue: std::collections::VecDeque::new(),
        background_pending: Vec::new(),
        h2c_pool: H2cClientPool::new(),
        h2s_pool: H2sTlsClientPool::new(),
        rate_limiter: RateLimiter::new(),
    };

    event_loop(udp, tcp_listener, h2c_listener, watcher, &mut state, &mut quiche_config, &log_handle)
}
