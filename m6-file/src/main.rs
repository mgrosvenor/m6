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
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};
use tracing::{debug, error, info, warn};

fn main() {
    let code = run();
    std::process::exit(code);
}

fn run() -> i32 {
    let args: Vec<String> = std::env::args().collect();

    // Parse CLI: m6-file <site-dir> <config-path> [--log-level debug]
    let mut site_dir_str: Option<String> = None;
    let mut config_path_str: Option<String> = None;
    let mut log_level = "info".to_string();

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--log-level" => {
                i += 1;
                if i < args.len() {
                    log_level = args[i].clone();
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

    let level_filter = match log_level.as_str() {
        "trace" => tracing::Level::TRACE,
        "debug" => tracing::Level::DEBUG,
        "info"  => tracing::Level::INFO,
        "warn"  => tracing::Level::WARN,
        "error" => tracing::Level::ERROR,
        _       => tracing::Level::INFO,
    };

    tracing_subscriber::fmt()
        .json()
        .with_max_level(level_filter)
        .init();

    let site_dir = PathBuf::from(&site_dir_str);
    let config_path = PathBuf::from(&config_path_str);

    let config = match Config::load(&config_path) {
        Ok(c) => c,
        Err(e) => {
            error!(error = %e, "failed to load config");
            return 2;
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

    let routes = Arc::new(routes);
    let config = Arc::new(config);
    let site_dir = Arc::new(site_dir);

    // In-flight counter for graceful drain: incremented before handing off a
    // connection to a worker, decremented when the worker finishes.
    let in_flight = Arc::new(AtomicUsize::new(0));

    let (tx, rx) = mpsc::channel::<std::os::unix::net::UnixStream>();
    let rx = Arc::new(std::sync::Mutex::new(rx));

    for _ in 0..pool_size {
        let rx = Arc::clone(&rx);
        let routes = Arc::clone(&routes);
        let config = Arc::clone(&config);
        let site_dir = Arc::clone(&site_dir);
        let in_flight = Arc::clone(&in_flight);

        std::thread::spawn(move || {
            loop {
                let stream = match rx.lock().unwrap().recv() {
                    Ok(s) => s,
                    Err(_) => break,
                };
                if let Err(e) = handle_connection(stream, &routes, &config, &site_dir) {
                    debug!(error = %e, "connection error");
                }
                in_flight.fetch_sub(1, Ordering::SeqCst);
            }
        });
    }

    for stream in listener.incoming() {
        if shutdown.load(Ordering::Relaxed) {
            break;
        }
        let stream = match stream {
            Ok(s) => s,
            Err(e) => {
                if shutdown.load(Ordering::Relaxed) { break; }
                error!(error = %e, "accept error");
                continue;
            }
        };
        in_flight.fetch_add(1, Ordering::SeqCst);
        if tx.send(stream).is_err() {
            in_flight.fetch_sub(1, Ordering::SeqCst);
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

    info!(
        path = req.path,
        method = req.method,
        status = info.status,
        bytes = info.bytes,
        latency_us = info.latency_us,
        "request complete"
    );

    Ok(())
}

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

fn num_cpus() -> usize {
    match std::thread::available_parallelism() {
        Ok(n) => n.get(),
        Err(_) => 4,
    }
}
