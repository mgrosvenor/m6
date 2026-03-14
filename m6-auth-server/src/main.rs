mod config;
mod jwt;
mod key_watch;
mod rate_limit;
mod handlers;

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};

use anyhow::Result;
use tracing::{error, info, warn};

use m6_auth::Db;
use m6_core::server::{socket_path_from_config, UnixServer};
use m6_core::signal::ShutdownHandle;

use config::AuthConfig;
use key_watch::{KeyMaterial, spawn_key_watcher};
use rate_limit::RateLimiter;

fn main() {
    let code = run();
    std::process::exit(code);
}

fn run() -> i32 {
    let args: Vec<String> = std::env::args().collect();

    // Parse CLI: m6-auth-server <site-dir> <config-path> [--log-level debug]
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
            eprintln!("Usage: m6-auth-server <site-dir> <config-path> [--log-level LEVEL]");
            return 2;
        }
    };
    let config_path_str = match config_path_str {
        Some(s) => s,
        None => {
            eprintln!("Usage: m6-auth-server <site-dir> <config-path> [--log-level LEVEL]");
            return 2;
        }
    };

    let site_dir  = PathBuf::from(&site_dir_str);
    let config_path = PathBuf::from(&config_path_str);

    // Load config (exit 2 on config error)
    let cfg = match AuthConfig::load(&site_dir, &config_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("failed to load config: {}", e);
            return 2;
        }
    };

    // Resolve log settings: site.toml base → per-app [log] override → CLI --log-level.
    let (site_level, site_format) = m6_core::log::read_site_log_config(&site_dir);
    let format = cfg.log.as_ref()
        .and_then(|l| l.format.as_deref())
        .unwrap_or(&site_format)
        .to_string();
    let cfg_level = cfg.log.as_ref()
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

    // Load key material (exit 2 if unreadable or invalid)
    let key_material = match KeyMaterial::load(&cfg.private_key_path, &cfg.public_key_path, cfg.issuer.clone()) {
        Ok(k) => k,
        Err(e) => {
            error!(error = %e, "failed to load key material");
            return 2;
        }
    };

    // Open database
    let db_path = site_dir.join(&cfg.db_path);
    if let Some(parent) = db_path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            error!(error = %e, "failed to create db directory");
            return 1;
        }
    }
    let db = match Db::open(&db_path) {
        Ok(d) => d,
        Err(e) => {
            error!(error = %e, "failed to open auth database");
            return 1;
        }
    };

    // Wrap key material in Arc<RwLock<>> for hot-swappable key rotation
    let keys = Arc::new(RwLock::new(key_material));

    // Spawn key file watcher — reloads keys on rotation without restart
    spawn_key_watcher(
        cfg.private_key_path.clone(),
        cfg.public_key_path.clone(),
        cfg.issuer.clone(),
        Arc::clone(&keys),
    );

    // Build shared state
    let state = Arc::new(handlers::AppState {
        db: Mutex::new(db),
        keys,
        access_ttl:  cfg.access_ttl,
        refresh_ttl: cfg.refresh_ttl,
        issuer: cfg.issuer.clone(),
        rate_limiter: Mutex::new(RateLimiter::new()),
    });

    // Derive socket path (can be overridden for tests)
    let socket_path = if let Ok(override_path) = std::env::var("M6_SOCKET_OVERRIDE") {
        PathBuf::from(override_path)
    } else {
        socket_path_from_config(&config_path)
    };

    // Ensure socket directory exists
    if let Some(parent) = socket_path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            error!(error = %e, dir = %parent.display(), "failed to create socket directory");
            return 1;
        }
    }

    // Bind Unix socket
    let server = match UnixServer::bind(socket_path.clone()) {
        Ok(s) => s,
        Err(e) => {
            error!(error = %e, socket = %socket_path.display(), "failed to bind socket");
            return 1;
        }
    };

    // Set socket permissions
    if let Err(e) = std::fs::set_permissions(
        server.path(),
        std::fs::Permissions::from_mode(0o666),
    ) {
        warn!(error = %e, "failed to set socket permissions");
    }

    info!(socket = %server.path().display(), issuer = %cfg.issuer, "m6-auth-server starting");

    // Signal handling
    let _shutdown = ShutdownHandle::install();
    let shutdown2 = _shutdown.clone();

    // Spawn a thread that unblocks the accept loop after shutdown
    let socket_path2 = socket_path.clone();
    std::thread::spawn(move || {
        shutdown2.wait();
        // Wake the accept loop
        let _ = std::os::unix::net::UnixStream::connect(&socket_path2);
    });

    // Accept loop
    loop {
        if _shutdown.is_shutdown() {
            break;
        }

        let (stream, _addr) = match server.listener().accept() {
            Ok(s) => s,
            Err(e) => {
                if _shutdown.is_shutdown() {
                    break;
                }
                error!(error = %e, "accept error");
                continue;
            }
        };

        if _shutdown.is_shutdown() {
            break;
        }

        let state2 = Arc::clone(&state);
        std::thread::spawn(move || {
            if let Err(e) = handle_connection(stream, &state2) {
                tracing::debug!(error = %e, "connection error");
            }
        });
    }

    info!("m6-auth-server shutdown complete");
    0
}

fn handle_connection(
    mut stream: std::os::unix::net::UnixStream,
    state: &Arc<handlers::AppState>,
) -> Result<()> {
    use std::io::Write;

    stream.set_read_timeout(Some(std::time::Duration::from_secs(30)))?;
    let req = m6_core::parse::parse_request(&mut stream)?;

    // Extract peer IP for rate limiting (use a placeholder since Unix sockets don't have IPs)
    let peer_ip = req.header("x-forwarded-for")
        .or_else(|| req.header("x-real-ip"))
        .unwrap_or("unix")
        .to_string();

    let resp = handlers::dispatch(&req, state, &peer_ip);
    stream.write_all(&resp.to_bytes())?;
    Ok(())
}
