//! The authentication service.
//!
//! An `App` service with two state layers: one database connection per worker
//! thread, opened on the thread's first request, and the signing keys, token lifetimes and rate limiter shared by
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

use std::sync::Mutex;

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
    let cfg = AuthConfig::load(ctx.config_path, ctx.site_toml)
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

    // Loaded once at start. A key rotation is a restart.
    let keys = key_material;

    tracing::info!(issuer = %cfg.issuer, "auth config loaded");

    Ok(AppState {
        keys,
        access_ttl: cfg.access_ttl,
        refresh_ttl: cfg.refresh_ttl,
        issuer: cfg.issuer.clone(),
        db_path: cfg.db_path.clone(),
        rate_limiter: Mutex::new(RateLimiter::with_limits(
            cfg.rate_limit_max_attempts,
            cfg.rate_limit_window_secs,
        )),
    })
}

/// One worker thread's database connection, opened on first use.
///
/// `App` builds this value while it handles the thread's first request, and
/// core panics when that build returns `Err`. The release profile aborts on a
/// panic, so a failed open there would end the whole service. The value is
/// therefore built empty and cannot fail. [`WorkerDb::get`] makes the real
/// open, and a failure becomes an ordinary 500 for that one request. The next
/// request on the thread tries the open again.
///
/// Every connection to one file must use the same journal mode, and
/// `m6_auth::Db::open` is the one place that sets it, so every connection goes
/// through it.
struct WorkerDb(Option<m6_auth::Db>);

impl WorkerDb {
    /// The connection, opening it first if this thread has none. `None` when
    /// the open fails.
    fn get(&mut self, path: &std::path::Path) -> Option<&m6_auth::Db> {
        if self.0.is_none() {
            match m6_auth::Db::open(path) {
                Ok(db) => self.0 = Some(db),
                Err(e) => {
                    tracing::error!(error = %e, "opening auth database for a worker");
                    return None;
                }
            }
        }
        self.0.as_ref()
    }
}

fn open_worker_db(_config: &Map<String, Value>, _state: &AppState) -> Result<WorkerDb> {
    Ok(WorkerDb(None))
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
/// The route receives the shared state and the worker's own [`WorkerDb`], in
/// that order, which is the shape `App::with_state` hands a handler. It opens
/// the connection on first use and answers 500 when that fails.
macro_rules! route {
    ($app:expr, $method:ident, $path:literal, $f:expr) => {
        $app.$method(
            $path,
            move |req: &Request, state: &AppState, slot: &mut WorkerDb| {
                let f: fn(&Request, &AppState, &m6_auth::Db, &str) -> RawResponse = $f;
                match slot.get(&state.db_path) {
                    Some(db) => Ok(f(req, state, db, &peer_ip(req)).into()),
                    None => Ok(handlers::internal_error().into()),
                }
            },
        )
    };
}

fn main() -> anyhow::Result<()> {
    let app = App::with_state(build_state, open_worker_db);
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
