//! The authentication service.
//!
//! An `App` service with two state layers: one database connection per worker
//! thread, and the signing keys, token lifetimes and rate limiter shared by
//! all of them. Four routes, all literal paths.
//!
//! This file used to be 246 lines of CLI parsing, logging setup, socket bind
//! and permissions, an accept loop, and a `thread::spawn` per connection. All
//! of it had an equivalent in core. The one capability it needed that `App`
//! lacked was `chmod` on the socket, which became `[server] socket_mode` on
//! 2026-09-12.
//!
//! **The connection model changes.** This spawned an unbounded thread per
//! connection; `App` uses a bounded pool and answers 503 when the queue is
//! full. For a login endpoint that is the safer of the two by a wide margin:
//! a thread per connection is what turns a credential-stuffing burst into
//! memory exhaustion, and this service already rate-limits per address
//! precisely because it expects that traffic.

mod config;
mod handlers;
mod jwt;
mod keys;
mod rate_limit;

use std::sync::{Arc, Mutex, RwLock};

use m6_core::http::RawResponse;
use m6_core::prelude::*;

use config::AuthConfig;
use handlers::AppState;
use keys::KeyMaterial;
use rate_limit::RateLimiter;

/// Build the state every worker shares: config, keys, rate limiter.
///
/// Returns `Err` rather than exiting, so `App` reports it the same way it
/// reports every other startup failure.
fn build_state(ctx: &AppContext) -> Result<AppState> {
    let cfg = AuthConfig::load(ctx.site_dir, ctx.config_path, ctx.site_toml)
        .map_err(|e| Error::Other(e.context("loading auth config")))?;

    let key_material = KeyMaterial::load(
        &cfg.private_key_path,
        &cfg.public_key_path,
        cfg.issuer.clone(),
    )
    .map_err(|e| Error::Other(e.context("loading key material")))?;

    if let Some(parent) = cfg.db_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| Error::Other(anyhow::Error::new(e).context("creating db directory")))?;
    }
    // Opened once here, then dropped: a path that cannot be opened or a
    // schema that cannot be applied fails startup with a message. Each worker
    // opens its own connection below.
    m6_auth::Db::open(&cfg.db_path)
        .map_err(|e| Error::Other(anyhow::anyhow!("opening auth database: {e}")))?;

    // Swapped by `reload_state` on a rotation, so a rotation needs no restart.
    let keys = Arc::new(RwLock::new(key_material));

    tracing::info!(issuer = %cfg.issuer, "auth config loaded");

    Ok(AppState {
        keys,
        access_ttl: cfg.access_ttl,
        refresh_ttl: cfg.refresh_ttl,
        issuer: cfg.issuer.clone(),
        db_path: cfg.db_path.clone(),
        rate_limit_max_attempts: cfg.rate_limit_max_attempts,
        rate_limit_window_secs: cfg.rate_limit_window_secs,
        rate_limiter: Mutex::new(RateLimiter::with_limits(
            cfg.rate_limit_max_attempts,
            cfg.rate_limit_window_secs,
        )),
    })
}

/// One database connection for the worker thread that opens it.
///
/// `App` calls this while handling that thread's first request. Core panics
/// when this returns `Err`, and the release profile aborts, so `build_state`
/// opens the database once first: that applies the schema and switches the
/// file to WAL before any worker exists, and a worker's open then reopens a
/// file already known to work. Every connection to one file must use the same
/// journal mode, and `m6_auth::Db::open` is the one place that sets WAL, the
/// busy timeout and foreign keys, so every connection goes through it.
fn open_worker_db(_config: &Map<String, Value>, state: &AppState) -> Result<m6_auth::Db> {
    m6_auth::Db::open(&state.db_path)
        .map_err(|e| Error::Other(anyhow::anyhow!("opening auth database: {e}")))
}

/// Re-read the signing keys when the service's config is touched.
///
/// A rotation is: write the private key and the public key, then touch the
/// config. Both files are loaded before anything is swapped, so a bad key
/// returns `Err`, the running keys stay, and core tries again after a delay
/// and at once on the next write.
///
/// The database path, the token lifetimes, the issuer and the rate limit are
/// read once at start. A change to any of them is named in a warning that says
/// to restart.
fn reload_state(state: &AppState, ctx: &AppContext) -> Result<()> {
    let cfg = AuthConfig::load(ctx.site_dir, ctx.config_path, ctx.site_toml)
        .map_err(|e| Error::Other(e.context("loading auth config")))?;
    let km = KeyMaterial::load(
        &cfg.private_key_path,
        &cfg.public_key_path,
        state.issuer.clone(),
    )
    .map_err(|e| Error::Other(e.context("loading key material")))?;
    *state
        .keys
        .write()
        .map_err(|_| Error::Other(anyhow::anyhow!("the signing key lock is poisoned")))? = km;

    let changed = [
        ("storage.path", cfg.db_path != state.db_path),
        ("tokens.access_ttl", cfg.access_ttl != state.access_ttl),
        ("tokens.refresh_ttl", cfg.refresh_ttl != state.refresh_ttl),
        ("tokens.issuer", cfg.issuer != state.issuer),
        (
            "rate_limit.max_attempts",
            cfg.rate_limit_max_attempts != state.rate_limit_max_attempts,
        ),
        (
            "rate_limit.window_secs",
            cfg.rate_limit_window_secs != state.rate_limit_window_secs,
        ),
    ];
    for (key, differs) in changed {
        if differs {
            tracing::warn!("config reload: [{key}] changed; restart m6-auth-server to apply it");
        }
    }
    tracing::info!("signing keys reloaded");
    Ok(())
}

/// A unix socket has no peer address, so rate limiting keys off what the proxy
/// forwarded. `"unix"` when nothing did, which buckets every unattributed
/// request together rather than exempting them.
fn peer_ip(req: &Request) -> String {
    req.header("x-forwarded-for")
        .or_else(|| req.header("x-real-ip"))
        .unwrap_or("unix")
        .to_string()
}

/// Adapt one of `handlers`' functions to a route.
///
/// The handlers return `RawResponse` and are left alone: they are the
/// security-carrying part of this service (rate limiting, JWT minting, cookie
/// flags), and a migration is the wrong time to rewrite them. `RawResponse`
/// lifts into `Response` verbatim.
///
/// The route receives the shared state and the worker's own `Db`, in that
/// order, which is the shape `App::with_state` hands a handler.
macro_rules! route {
    ($app:expr, $method:ident, $path:literal, $f:expr) => {
        $app.$method(
            $path,
            move |req: &Request, state: &AppState, db: &mut m6_auth::Db| {
                let f: fn(&Request, &AppState, &m6_auth::Db, &str) -> RawResponse = $f;
                Ok(f(req, state, db, &peer_ip(req)).into())
            },
        )
    };
}

fn main() -> anyhow::Result<()> {
    let app = App::with_state(build_state, open_worker_db).on_reload(reload_state);
    let app = route!(app, route_post, "/auth/login", |req, state, db, ip| {
        handlers::dispatch_login(req.raw(), state, db, ip)
    });
    let app = route!(app, route_post, "/auth/refresh", |req, state, db, _ip| {
        handlers::dispatch_refresh(req.raw(), state, db)
    });
    let app = route!(app, route_post, "/auth/logout", |req, state, db, _ip| {
        handlers::dispatch_logout(req.raw(), state, db)
    });
    let app = route!(
        app,
        route_get,
        "/auth/public-key",
        |_req, state, _db, _ip| { handlers::dispatch_public_key(state) }
    );
    app.run()?;
    Ok(())
}
