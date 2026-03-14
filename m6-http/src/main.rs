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
use tracing::{debug, error, info, warn};

use m6_http_lib::auth;
use m6_http_lib::cache::{Cache, CacheKey, CachedResponse, make_lookup_key, should_cache};
use m6_http_lib::stats::Stats;
use m6_http_lib::config::{self, Config};
use m6_http_lib::error::{self as error, make_error_response, ErrorMode};
use m6_http_lib::forward::{self, HttpRequest, HttpResponse};
use m6_http_lib::pool::{self, PoolManager};
use m6_http_lib::poller::{Poller, Token};
use m6_http_lib::router::{self, RouteTable};
use m6_http_lib::watcher::{FsEvent, FsEventKind, FsWatcher};
use m6_http_lib::auth::PublicKey;
use m6_http_lib::http11::{Http11Listener, make_tls_server_config};

// ── Constants ────────────────────────────────────────────────────────────────

const TOKEN_UDP: Token = Token(0);
const TOKEN_INOTIFY: Token = Token(1);
const TOKEN_TCP: Token = Token(2);
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
    /// Partial responses awaiting flow-control credit: stream_id -> (body, offset_written)
    partial_responses: HashMap<u64, (Bytes, usize)>,
    client_addr: SocketAddr,
    /// When we last heard from this connection (for timeout tracking)
    last_active: Instant,
}

struct PendingRequest {
    headers: Vec<quiche::h3::Header>,
    body: Vec<u8>,
    headers_done: bool,
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

    cfg.load_cert_chain_from_pem_file(&server_config.tls_cert)
        .context("load cert chain")?;
    cfg.load_priv_key_from_pem_file(&server_config.tls_key)
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
    mut watcher: Option<FsWatcher>,
    state: &mut ServerState,
    quiche_config: &mut quiche::Config,
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

    let mut connections: HashMap<Vec<u8>, QuicConn> = HashMap::new();
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
        let timeout_ms = connections
            .values()
            .filter_map(|c| c.conn.timeout())
            .min()
            .map(|d| d.as_millis() as i32)
            .unwrap_or(100)
            .min(100); // always check SHUTDOWN at least every 100 ms

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
                        quiche_config,
                        state,
                    );
                }
                TOKEN_TCP => {
                    if let Some(ref mut t) = tcp {
                        t.accept_pending();
                    }
                }
                TOKEN_INOTIFY => {
                    if let Some(ref mut w) = watcher {
                        for event in w.read_events() {
                            handle_fs_event(&event, state, quiche_config);
                        }
                    }
                }
                _ => {}
            }
        }

        // Drive HTTP/1.1 connections (runs every tick regardless of which fd fired)
        if let Some(ref mut t) = tcp {
            t.drive_all(|req, client_ip| {
                let enc_str = req.headers
                    .iter()
                    .find(|(k, _)| k.eq_ignore_ascii_case("accept-encoding"))
                    .map(|(_, v)| v.as_str())
                    .unwrap_or("");
                let start = std::time::Instant::now();
                let (status, headers, body, backend_name) =
                    handle_request(req, client_ip, enc_str, state);
                let elapsed_ns = start.elapsed().as_nanos() as u64;
                let is_backend_error = status >= 500;
                state.stats.record(elapsed_ns, false, is_backend_error);
                debug!(
                    path = %req.path,
                    status,
                    version = "HTTP/1.1",
                    backend = %backend_name,
                    latency_us = elapsed_ns / 1_000,
                    cache_hit = false,
                    "request complete"
                );
                (status, headers, body, backend_name)
            });
        }

        // Drive connection timeouts and flush pending sends
        flush_all(&udp, &mut connections);

        // Remove closed/timed-out connections
        connections.retain(|_, c| !c.conn.is_closed());

        // Emit periodic stats (cheap check every iteration: compares one Instant)
        state.stats.maybe_emit(state.pool_manager.total_active_members());

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
    quiche_config: &mut quiche::Config,
    state: &mut ServerState,
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

        // Get or create connection
        if !connections.contains_key(&conn_id) {
            if hdr.ty != quiche::Type::Initial {
                debug!("non-initial packet for unknown conn");
                continue;
            }
            // New connection
            let scid = quiche::ConnectionId::from_ref(&conn_id);
            let conn = match quiche::accept(&scid, None, local, from, quiche_config) {
                Ok(c) => c,
                Err(e) => {
                    warn!("quiche::accept error: {}", e);
                    continue;
                }
            };
            connections.insert(
                conn_id.clone(),
                QuicConn {
                    conn,
                    h3_conn: None,
                    pending: HashMap::new(),
                    partial_responses: HashMap::new(),
                    client_addr: from,
                    last_active: Instant::now(),
                },
            );
        }

        let qconn = match connections.get_mut(&conn_id) {
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
            process_h3(qconn, udp, state);
        }

        // Send any pending QUIC packets
        flush_conn(udp, qconn);
    }
}

// ── H3 event processing ───────────────────────────────────────────────────────

fn process_h3(qconn: &mut QuicConn, _udp: &UdpSocket, state: &mut ServerState) {
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
                    handle_h3_request(stream_id, qconn, state);
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
                    handle_h3_request(stream_id, qconn, state);
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

    let start = Instant::now();

    // ── Cache lookup — zero allocation ────────────────────────────────────────
    let mut key_buf = [0u8; 512];
    let lookup_key = make_lookup_key(path_str, enc_str, &mut key_buf);

    if let Some(cached) = state.cache.get(lookup_key) {
        let elapsed_ns = start.elapsed().as_nanos() as u64;
        state.stats.record(elapsed_ns, true, false);
        debug!(
            path = %path_str,
            status = cached.status,
            version = "HTTP/3",
            backend = "cache",
            latency_us = elapsed_ns / 1_000,
            cache_hit = true,
            "request complete"
        );
        send_h3_response(stream_id, qconn, cached.status, &cached.headers, cached.body);
        return;
    }

    // ── Phase 2: cache miss — allocate owned data for forwarding ──────────────
    let path    = path_str.to_string();
    let method  = std::str::from_utf8(method_bytes).unwrap_or("GET").to_string();
    let query   = query_bytes.and_then(|qb| std::str::from_utf8(qb).ok()).map(str::to_string);
    let client_ip = qconn.client_addr.ip().to_string();

    let mut fwd_headers: Vec<(String, String)> = Vec::new();
    for h in &req.headers {
        let name = h.name();
        if name.starts_with(b":") { continue; }
        if let (Ok(k), Ok(v)) = (std::str::from_utf8(name), std::str::from_utf8(h.value())) {
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

    let (status, resp_headers, body, backend_name) =
        handle_request(&http_req, &client_ip, enc_str, state);

    let elapsed_ns = start.elapsed().as_nanos() as u64;
    let is_backend_error = status >= 500;
    state.stats.record(elapsed_ns, false, is_backend_error);
    debug!(
        path = %path,
        status,
        version = "HTTP/3",
        backend = %backend_name,
        latency_us = elapsed_ns / 1_000,
        cache_hit = false,
        "request complete"
    );

    send_h3_response(stream_id, qconn, status, &resp_headers, Bytes::from(body));
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

    // Pre-size: :status + response headers + content-length
    let mut h3_headers: Vec<quiche::h3::Header> = Vec::with_capacity(headers.len() + 2);
    h3_headers.push(quiche::h3::Header::new(b":status", &status_buf));
    for (k, v) in headers {
        h3_headers.push(quiche::h3::Header::new(k.as_bytes(), v.as_bytes()));
    }
    h3_headers.push(quiche::h3::Header::new(b"content-length", cl_bytes));

    let fin = body.is_empty();
    if let Err(e) = h3.send_response(&mut qconn.conn, stream_id, &h3_headers, fin) {
        warn!("h3 send_response error: {}", e);
        return;
    }
    if !body.is_empty() {
        match h3.send_body(&mut qconn.conn, stream_id, &body, true) {
            Ok(written) if written == body.len() => {}
            Ok(written) => {
                // Partial write — store remainder, retry on conn.writable()
                qconn.partial_responses.insert(stream_id, (body, written));
            }
            Err(quiche::h3::Error::Done) | Err(quiche::h3::Error::StreamBlocked) => {
                qconn.partial_responses.insert(stream_id, (body, 0));
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
) -> (u16, Vec<(String, String)>, Vec<u8>, String) {
    // Route lookup
    let route = match state.route_table.at(&req.path) {
        Some(r) => r.clone(),
        None => {
            let (s, h, b) = make_error_response(404, &state.error_mode, &req.path);
            return (s, h, b, "none".to_string());
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
            let cookie_header = req
                .headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case("cookie"))
                .map(|(_, v)| v.as_str());
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
                        return (302, headers, vec![], "auth".to_string());
                    }
                    let (s, h, b) =
                        make_error_response(401, &state.error_mode, &req.path);
                    return (s, h, b, "auth".to_string());
                }
                Some(token) => match pk.verify(token) {
                    Err(e) => {
                        warn!(path = %req.path, error = %e, "auth: token verification failed");
                        let (s, h, b) =
                            make_error_response(401, &state.error_mode, &req.path);
                        return (s, h, b, "auth".to_string());
                    }
                    Ok(claims) => {
                        if !auth::check_require(&claims, require) {
                            warn!(
                                path = %req.path,
                                require = %require,
                                "auth: insufficient claims"
                            );
                            let (s, h, b) =
                                make_error_response(403, &state.error_mode, &req.path);
                            return (s, h, b, "auth".to_string());
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
    let backend_name = route.backend.clone();
    let (status, resp_headers, body, used_backend) =
        match forward_to_backend(req, &backend_name, client_ip, state) {
            Ok(http_resp) => {
                if should_cache(http_resp.status, &http_resp.headers) {
                    let key = CacheKey::new(&req.path, content_encoding);
                    state.cache.insert(
                        key,
                        CachedResponse {
                            status: http_resp.status,
                            headers: std::sync::Arc::new(http_resp.headers.clone()),
                            body: Bytes::from(http_resp.body.clone()),
                        },
                    );
                }
                (http_resp.status, http_resp.headers, http_resp.body, backend_name)
            }
            Err(e) => {
                warn!(backend = %backend_name, error = %e, "backend error");
                (502u16, vec![], vec![], "error".to_string())
            }
        };

    // If the response is an error (4xx/5xx) and not already an error response,
    // apply the error mode: status, internal, or custom.
    if status >= 400 {
        return apply_error_mode(status, req, client_ip, state);
    }

    (status, resp_headers, body, used_backend)
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
) -> (u16, Vec<(String, String)>, Vec<u8>, String) {
    match &state.error_mode {
        ErrorMode::Status => {
            (status, vec![("Content-Type".to_string(), "text/plain".to_string())], vec![], "error".to_string())
        }
        ErrorMode::Internal => {
            let reason = error::status_reason(status);
            let body = error::internal_error_html(status, reason);
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
                let body = error::internal_error_html(status, reason);
                return (
                    status,
                    vec![("Content-Type".to_string(), "text/html; charset=utf-8".to_string())],
                    body,
                    "error".to_string(),
                );
            }

            // Build error page request: GET <error_path>?status=N&from=/original-path
            let error_query = format!("status={}&from={}", status, urlencoded(&req.path));
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
                    let body = error::internal_error_html(status, reason);
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
                    let body = error::internal_error_html(status, reason);
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
    } else if let Some(url) = state.pool_manager.get_url(backend_name).map(str::to_string) {
        forward::forward_url_request(&url, req, client_ip, original_host, Some(timeout))
            .map_err(|e| e.to_string())
    } else {
        Err(format!("unknown backend: {}", backend_name))
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

/// Retry any partially-written response bodies on streams that have new flow-control credit.
fn drain_writable(qconn: &mut QuicConn) {
    if qconn.partial_responses.is_empty() { return; }
    let h3 = match qconn.h3_conn.as_mut() { Some(h) => h, None => return };
    let writable: Vec<u64> = qconn.conn.writable().collect();
    for stream_id in writable {
        let (body, offset) = match qconn.partial_responses.get(&stream_id) {
            Some(r) => r,
            None => continue,
        };
        let remaining = &body[*offset..];
        match h3.send_body(&mut qconn.conn, stream_id, remaining, true) {
            Ok(written) => {
                let (body, offset) = qconn.partial_responses.get_mut(&stream_id).unwrap();
                *offset += written;
                if *offset >= body.len() {
                    qconn.partial_responses.remove(&stream_id);
                }
            }
            Err(quiche::h3::Error::Done) | Err(quiche::h3::Error::StreamBlocked) => {}
            Err(e) => {
                warn!("h3 drain_writable send_body error: {}", e);
                qconn.partial_responses.remove(&stream_id);
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
) {
    match event.kind {
        FsEventKind::SocketCreated => {
            state.pool_manager.socket_appeared(&event.path);
        }
        FsEventKind::SocketDeleted => {
            state.pool_manager.socket_disappeared(&event.path);
        }
        FsEventKind::SiteTomlChanged => {
            handle_site_reload(state);
        }
        FsEventKind::TlsCertChanged => {
            handle_tls_reload(state, quiche_config);
        }
    }
}

fn handle_site_reload(state: &mut ServerState) {
    info!("config reload: site.toml changed");

    match config::load(&state.config.site_dir, &state.system_config_path) {
        Ok(new_config) => {
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

            state.route_table = new_route_table;
            state.pool_manager = new_pools;
            state.invalidation_map = new_inv_map;
            state.error_mode = new_error_mode;
            state.config = new_config;

            info!("config reload: complete");
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
    let _log_guard = match m6_core::log::init(&config.log.format, log_level) {
        Ok(g) => g,
        Err(e) => {
            eprintln!("logging init error: {}", e);
            return 1;
        }
    };

    config::warn_system_config_extra_keys(&cli.system_config);

    // --dump-config
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

    // Build TLS config for HTTP/1.1 and bind TCP listener on the same port
    let tcp_listener = match make_tls_server_config(
        &config.server.tls_cert,
        &config.server.tls_key,
    ) {
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

    info!(
        bind = %config.server.bind,
        site = %config.site.name,
        "m6-http started (HTTP/3 over QUIC + HTTP/1.1 over TLS, single-threaded epoll)"
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
    };

    event_loop(udp, tcp_listener, watcher, &mut state, &mut quiche_config)
}
