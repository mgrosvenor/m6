mod compress;
mod config;
mod handler;
mod http;
mod route;

use anyhow::{Context, Result};
use config::{socket_path_from_config, Config};
use handler::{handle_request, HandlerContext};
use http::Request;
use route::Route;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, RwLock};
use tracing::{debug, error, info, warn};

// ---------------------------------------------------------------------------
// Shared hot-reload state
// ---------------------------------------------------------------------------

struct FileState {
    config: Arc<Config>,
    routes: Arc<Vec<Route>>,
}

// ---------------------------------------------------------------------------
// main / run
// ---------------------------------------------------------------------------

fn main() {
    let code = run();
    std::process::exit(code);
}

fn run() -> i32 {
    let args: Vec<String> = std::env::args().collect();

    // Parse CLI: m6-file <site-dir> <config-path> [--log-level debug]
    let mut site_dir_str: Option<String> = None;
    let mut config_path_str: Option<String> = None;
    let mut log_level: Option<String> = None;

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--log-level" => {
                i += 1;
                if i < args.len() {
                    log_level = Some(args[i].clone());
                }
            }
            arg if !arg.starts_with('-') => {
                if site_dir_str.is_none() {
                    site_dir_str = Some(arg.to_string());
                } else if config_path_str.is_none() {
                    config_path_str = Some(arg.to_string());
                }
            }
            _ => {}
        }
        i += 1;
    }

    let site_dir_str = match site_dir_str {
        Some(s) => s,
        None => {
            eprintln!("Usage: m6-file <site-dir> <config-path> [--log-level LEVEL]");
            return 2;
        }
    };
    let config_path_str = match config_path_str {
        Some(s) => s,
        None => {
            eprintln!("Usage: m6-file <site-dir> <config-path> [--log-level LEVEL]");
            return 2;
        }
    };

    let site_dir = PathBuf::from(&site_dir_str);
    let config_path = PathBuf::from(&config_path_str);

    let config = match Config::load(&config_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("failed to load config: {}", e);
            return 2;
        }
    };

    // Resolve log settings: site.toml base → per-app [log] override → CLI --log-level.
    let (site_level, site_format) = m6_core::log::read_site_log_config(&site_dir);
    let format = config.log.as_ref()
        .and_then(|l| l.format.as_deref())
        .unwrap_or(&site_format)
        .to_string();
    let cfg_level = config.log.as_ref()
        .and_then(|l| l.level.as_deref())
        .unwrap_or(&site_level)
        .to_string();
    let level = log_level.as_deref().unwrap_or(&cfg_level).to_string();

    let _log_guard = match m6_core::log::init(&format, &level) {
        Ok(g) => g,
        Err(e) => {
            eprintln!("logging init error: {}", e);
            return 1;
        }
    };

    let mut routes: Vec<Route> = config.route.iter().map(Route::from_config).collect();
    route::sort_routes(&mut routes);

    for i in 0..routes.len() {
        for j in (i + 1)..routes.len() {
            if routes[i].specificity == routes[j].specificity {
                warn!(
                    route_a = %routes[i].raw_path,
                    route_b = %routes[j].raw_path,
                    "routes have equal specificity, first declaration wins"
                );
            }
        }
    }

    info!(
        route_count = routes.len(),
        site_dir = %site_dir.display(),
        "m6-file starting"
    );

    let socket_path = if let Ok(override_path) = std::env::var("M6_SOCKET_OVERRIDE") {
        PathBuf::from(override_path)
    } else {
        socket_path_from_config(&config_path)
    };

    if let Some(parent) = socket_path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            error!(error = %e, dir = %parent.display(), "failed to create socket directory");
            return 1;
        }
    }

    let _ = std::fs::remove_file(&socket_path);

    let listener = match std::os::unix::net::UnixListener::bind(&socket_path) {
        Ok(l) => l,
        Err(e) => {
            error!(error = %e, socket = %socket_path.display(), "failed to bind socket");
            return 1;
        }
    };

    if let Err(e) = std::fs::set_permissions(
        &socket_path,
        std::fs::Permissions::from_mode(0o666),
    ) {
        warn!(error = %e, "failed to set socket permissions");
    }

    if let Err(e) = listener.set_nonblocking(true) {
        error!(error = %e, "failed to set listener non-blocking");
        return 1;
    }

    info!(socket = %socket_path.display(), "listening on Unix socket");

    let shutdown = Arc::new(AtomicBool::new(false));
    let signal_count = Arc::new(AtomicUsize::new(0));

    setup_signal_handlers(
        Arc::clone(&shutdown),
        Arc::clone(&signal_count),
        socket_path.clone(),
    );

    let pool_size = config
        .thread_pool
        .as_ref()
        .and_then(|tp| tp.size)
        .unwrap_or_else(num_cpus);

    info!(threads = pool_size, "thread pool configured");

    // Wrap shared hot-reload state in an RwLock.
    let shared = Arc::new(RwLock::new(FileState {
        config: Arc::new(config),
        routes: Arc::new(routes),
    }));

    let site_dir = Arc::new(site_dir);

    // In-flight counter for graceful drain: incremented before handing off a
    // connection to a worker, decremented when the worker finishes.
    let in_flight = Arc::new(AtomicUsize::new(0));

    let (tx, rx) = mpsc::channel::<std::os::unix::net::UnixStream>();
    let rx = Arc::new(std::sync::Mutex::new(rx));

    for _ in 0..pool_size {
        let rx = Arc::clone(&rx);
        let shared = Arc::clone(&shared);
        let site_dir = Arc::clone(&site_dir);
        let in_flight = Arc::clone(&in_flight);

        std::thread::spawn(move || {
            loop {
                let stream = match rx.lock().unwrap().recv() {
                    Ok(s) => s,
                    Err(_) => break,
                };
                // Acquire read lock only long enough to clone the two inner Arcs.
                let (config, routes) = {
                    let guard = shared.read().unwrap();
                    (Arc::clone(&guard.config), Arc::clone(&guard.routes))
                };
                if let Err(e) = handle_connection(stream, &routes, &config, &site_dir) {
                    debug!("connection error: {:#}", e);
                }
                in_flight.fetch_sub(1, Ordering::SeqCst);
            }
        });
    }

    // ── Hot-reload setup ──────────────────────────────────────────────────
    let mut watcher = m6_core::ConfigWatcher::new(
        &[&config_path, &site_dir.join("site.toml")]
    ).ok();

    // Mtime fallback: used when watcher.raw_fd() is None.
    let mut last_config_mtime = std::fs::metadata(&config_path)
        .and_then(|m| m.modified())
        .ok();
    // Countdown so we only check mtime every ~10 poll timeouts (≈1 s).
    let mut reload_countdown: u32 = 10;

    let listener_fd = listener.as_raw_fd();

    // ── poll(2) accept + hot-reload loop ─────────────────────────────────
    loop {
        if shutdown.load(Ordering::Relaxed) {
            break;
        }

        let borrowed_listener =
            unsafe { std::os::fd::BorrowedFd::borrow_raw(listener_fd) };
        let mut pfd_listener = nix::poll::PollFd::new(
            &borrowed_listener,
            nix::poll::PollFlags::POLLIN,
        );

        let watcher_fd = watcher.as_ref().and_then(|w| w.raw_fd());

        let (poll_result, listener_ready, watcher_fired) = if let Some(wfd) = watcher_fd {
            let borrowed_w = unsafe { std::os::fd::BorrowedFd::borrow_raw(wfd) };
            let pfd_w = nix::poll::PollFd::new(
                &borrowed_w,
                nix::poll::PollFlags::POLLIN,
            );
            let mut fds = [pfd_listener, pfd_w];
            let r = nix::poll::poll(&mut fds, 100);
            let l = fds[0]
                .revents()
                .map_or(false, |f| f.contains(nix::poll::PollFlags::POLLIN));
            let w = fds[1]
                .revents()
                .map_or(false, |f| f.contains(nix::poll::PollFlags::POLLIN));
            (r, l, w)
        } else {
            let r = nix::poll::poll(std::slice::from_mut(&mut pfd_listener), 100);
            let l = pfd_listener
                .revents()
                .map_or(false, |f| f.contains(nix::poll::PollFlags::POLLIN));
            (r, l, false)
        };

        // ── Determine whether a reload is needed ─────────────────────────
        let mut should_reload = false;

        match poll_result {
            Ok(0) | Err(_) => {
                // Timeout or interrupted — check shutdown flag.
                if shutdown.load(Ordering::Relaxed) {
                    break;
                }
                // Mtime fallback: check every ~10 timeouts (≈1 s) when no watcher fd.
                if watcher_fd.is_none() {
                    reload_countdown = reload_countdown.saturating_sub(1);
                    if reload_countdown == 0 {
                        reload_countdown = 10;
                        let new_mtime = std::fs::metadata(&config_path)
                            .and_then(|m| m.modified())
                            .ok();
                        if new_mtime != last_config_mtime {
                            last_config_mtime = new_mtime;
                            should_reload = true;
                        }
                    }
                }
                if !should_reload {
                    continue;
                }
            }
            Ok(_) => {}
        }

        // Watcher fd fired — drain events and check for our config file.
        if watcher_fired {
            if let Some(ref mut w) = watcher {
                let config_filename = config_path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("");
                should_reload = w.read_events(&[config_filename, "site.toml"]);
            }
        }

        // ── Hot reload ────────────────────────────────────────────────────
        if should_reload {
            handle_reload(&config_path, &shared);
        }

        if !listener_ready {
            continue;
        }

        // Drain all ready connections (non-blocking accept loop).
        loop {
            match listener.accept() {
                Ok((stream, _)) => {
                    // On BSD/macOS, accepted sockets inherit O_NONBLOCK from the
                    // listener.  Reset to blocking mode; our handlers use blocking I/O.
                    if let Err(e) = stream.set_nonblocking(false) {
                        warn!(error = %e, "failed to set accepted socket to blocking mode");
                    }
                    in_flight.fetch_add(1, Ordering::SeqCst);
                    if tx.send(stream).is_err() {
                        in_flight.fetch_sub(1, Ordering::SeqCst);
                        break;
                    }
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) => {
                    if shutdown.load(Ordering::Relaxed) {
                        break;
                    }
                    error!(error = %e, "accept error");
                    break;
                }
            }
        }

        if shutdown.load(Ordering::Relaxed) {
            break;
        }
    }

    // Graceful drain: drop the sender so workers exit after finishing current
    // requests, then spin-wait until all in-flight requests complete.
    drop(tx);
    let drain_deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while in_flight.load(Ordering::SeqCst) > 0 {
        if std::time::Instant::now() >= drain_deadline {
            warn!("graceful drain timed out, {} requests still in flight", in_flight.load(Ordering::SeqCst));
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }

    let _ = std::fs::remove_file(&socket_path);
    info!("m6-file shutdown complete");
    0
}

// ---------------------------------------------------------------------------
// Hot-reload helper
// ---------------------------------------------------------------------------

fn handle_reload(config_path: &Path, shared: &RwLock<FileState>) {
    match Config::load(config_path) {
        Ok(new_config) => {
            let mut new_routes: Vec<Route> =
                new_config.route.iter().map(Route::from_config).collect();
            route::sort_routes(&mut new_routes);
            let mut w = shared.write().unwrap();
            w.config = Arc::new(new_config);
            w.routes = Arc::new(new_routes);
            info!("config reloaded");
        }
        Err(e) => warn!(error = %e, "config reload failed, keeping current config"),
    }
}

// ---------------------------------------------------------------------------
// Connection handler
// ---------------------------------------------------------------------------

fn handle_connection(
    mut stream: std::os::unix::net::UnixStream,
    routes: &[Route],
    config: &Config,
    site_dir: &Path,
) -> Result<()> {
    stream.set_read_timeout(Some(std::time::Duration::from_secs(30)))?;

    let stream_clone = stream.try_clone().context("cloning stream")?;
    let req = Request::read(stream_clone).context("reading request")?;

    let ctx = HandlerContext { routes, config, site_dir };

    let info = handle_request(&req, &ctx, &mut stream).context("handling request")?;

    debug!(
        path = req.path,
        method = req.method,
        status = info.status,
        bytes = info.bytes,
        latency_us = info.latency_us,
        "request complete"
    );

    Ok(())
}

// ---------------------------------------------------------------------------
// Signal handling
// ---------------------------------------------------------------------------

fn setup_signal_handlers(
    shutdown: Arc<AtomicBool>,
    signal_count: Arc<AtomicUsize>,
    socket_path: PathBuf,
) {
    use nix::sys::signal::{SigSet, Signal};

    let mut mask = SigSet::empty();
    mask.add(Signal::SIGTERM);
    mask.add(Signal::SIGINT);
    let _ = mask.thread_block();

    std::thread::spawn(move || {
        let mut sig_mask = SigSet::empty();
        sig_mask.add(Signal::SIGTERM);
        sig_mask.add(Signal::SIGINT);
        let _ = sig_mask.thread_block();

        loop {
            match sig_mask.wait() {
                Ok(_sig) => {
                    let count = signal_count.fetch_add(1, Ordering::SeqCst) + 1;
                    if count >= 2 {
                        let _ = std::fs::remove_file(&socket_path);
                        std::process::exit(0);
                    }
                    shutdown.store(true, Ordering::SeqCst);
                    let _ = std::os::unix::net::UnixStream::connect(&socket_path);
                }
                Err(_) => break,
            }
        }
    });
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn num_cpus() -> usize {
    match std::thread::available_parallelism() {
        Ok(n) => n.get(),
        Err(_) => 4,
    }
}
