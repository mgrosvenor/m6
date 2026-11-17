//! The authentication service: one process and one event loop.
//!
//! The loop serves five routes, all literal paths, over one SQLite connection
//! opened at start and kept for the life of the process. The signing keys, the
//! token lifetimes and the rate limiter are plain fields the loop owns. A login
//! holds the process for its password verify, which is CPU work with no
//! descriptor to wait on.
//!
//! **The connection model.** Connections beyond what the process can serve wait
//! in the kernel's listen queue. The login rate limit is per process and in
//! memory, keyed on the address the proxy forwarded.
//!
//! **Two listeners do not make two loops.** `/check` gets its own Unix socket
//! and the other four routes keep the primary one, but both sockets feed this
//! one loop and this one connection table. A `/check` that arrives during a
//! login waits for that login's bcrypt verify, so the second listener alone
//! isolates nothing. Only a second process removes that tail: `--login-only`
//! and `--check-only` let a deployment run the login side and the check side
//! as two units, with two loops and two connection tables. The check loop then
//! runs no bcrypt, because `/auth/login` is not on its route set.

#![deny(unsafe_code)]

mod config;
mod handlers;
mod jwt;
mod keys;
mod rate_limit;

use std::cell::RefCell;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use m6_core::event::{LoopTrace, Slots, Timeouts};
use m6_core::h1::{head_slots, Flush, Read, ServerConn};
use m6_core::net::listen_unix;
use m6_core::platform::process::raise_fd_limit;
use m6_core::platform::Signals;
use m6_core::platform::{Events, Interest, Poll, Token};
use m6_core::prelude::*;
use m6_core::signal::{Action, Lifecycle};
use m6_core::site::{RouteMethod, GENID_PATH};
use m6_core::{genid_failed, genid_ok};

use config::AuthConfig;
use handlers::{AppState, Busy};
use keys::KeyMaterial;
use rate_limit::RateLimiter;

/// The login listener, on the primary socket. `/auth/login`,
/// `/auth/refresh`, `/auth/logout` and `/auth/public-key` answer here.
const LOGIN_LISTENER: Token = Token(usize::MAX);
const SIGNALS: Token = Token(usize::MAX - 1);
/// The operational log's stdout, registered while it holds bytes a full
/// destination would not take.
const LOG: Token = Token(usize::MAX - 2);
/// The check listener, on the check socket. Only `/check` answers here.
const CHECK_LISTENER: Token = Token(usize::MAX - 3);
/// The accounting tick, and the longest the loop waits when nothing happens.
const TICK: Duration = Duration::from_secs(1);
const EVENTS: usize = 256;
/// How soon the loop tries a request whose handler met a database lock. A
/// retry costs little and the lock usually clears in milliseconds, so the
/// waiting client barely notices while the loop serves the other connections
/// between attempts.
const BUSY_RETRY: Duration = Duration::from_millis(5);
/// How long a request waits for a lock before the loop answers 503. After a
/// second the client needs an answer it can act on, and the lock may be held
/// for a long time.
const BUSY_GIVE_UP: Duration = Duration::from_secs(1);

/// The five routes, in the order `Handled::Code` indexes them.
const ROUTES: [CodeRoute; 5] = [
    CodeRoute {
        pattern: "/auth/login",
        method: RouteMethod::Post,
    },
    CodeRoute {
        pattern: "/auth/refresh",
        method: RouteMethod::Post,
    },
    CodeRoute {
        pattern: "/auth/logout",
        method: RouteMethod::Post,
    },
    CodeRoute {
        pattern: "/auth/public-key",
        method: RouteMethod::Get,
    },
    CodeRoute {
        pattern: "/check",
        method: RouteMethod::Post,
    },
];

/// The listener a route is served on.
///
/// The split exists so a deployment can run the check route in a process that
/// never runs a login's bcrypt. A route on the other listener answers 404
/// rather than running, so a shared route table cannot put bcrypt back on the
/// check loop.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Side {
    Login,
    Check,
}

impl Side {
    const BOTH: [Side; 2] = [Side::Login, Side::Check];

    fn index(self) -> usize {
        match self {
            Side::Login => 0,
            Side::Check => 1,
        }
    }

    fn token(self) -> Token {
        match self {
            Side::Login => LOGIN_LISTENER,
            Side::Check => CHECK_LISTENER,
        }
    }
}

/// The listener each index in `ROUTES` belongs to.
const ROUTE_SIDE: [Side; 5] = [
    Side::Login,
    Side::Login,
    Side::Login,
    Side::Login,
    Side::Check,
];

/// Which listeners this process binds.
#[derive(Clone, Copy)]
struct Bind {
    login: bool,
    check: bool,
}

/// Read `--login-only` and `--check-only`, which select the listeners.
///
/// Neither flag binds both, which is the upgrade path: the primary socket and
/// the four routes it serves are unchanged. The flags are mutually exclusive
/// because a process with neither socket has nothing to serve.
fn bind_from_args() -> Bind {
    let args: Vec<String> = std::env::args().collect();
    let check_only = args.iter().any(|arg| arg == "--check-only");
    let login_only = args.iter().any(|arg| arg == "--login-only");
    // `--version` answers without a config, as it does in the shared parser.
    let version = args.iter().any(|arg| arg == "--version" || arg == "-V");
    if check_only && login_only && !version {
        eprintln!(
            "m6-auth-server: --check-only and --login-only are mutually exclusive: pass one or neither"
        );
        std::process::exit(2);
    }
    Bind {
        login: !check_only,
        check: !login_only,
    }
}

/// The check listener's socket: `<parent>/check-<file name>` beside the primary.
///
/// `/run/m6/m6-auth.sock` gives `/run/m6/check-m6-auth.sock`. The prefix keeps
/// the socket outside a deployment's existing login glob
/// `/run/m6/m6-auth*.sock`, so the upgrade adds a socket and moves none that
/// already dials the primary. `M6_CHECK_SOCKET_OVERRIDE` wins outright, as
/// `M6_SOCKET_OVERRIDE` does for the primary.
fn check_socket_path(primary: &Path) -> PathBuf {
    if let Ok(path) = std::env::var("M6_CHECK_SOCKET_OVERRIDE") {
        return PathBuf::from(path);
    }
    let name = primary
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("m6-auth-server.sock");
    primary.with_file_name(format!("check-{name}"))
}

/// A request whose handler met a database lock: the route and the request are
/// kept until the loop tries again.
struct Retry {
    /// The index into `ROUTES` the request matched.
    route: usize,
    req: Request,
    /// When the request first met the lock, for the one-second limit.
    first: Instant,
    /// The user a login verified before its refresh-token write met the lock,
    /// so the retry skips bcrypt.
    verified: Option<m6_auth::User>,
}

/// Build what the loop owns: the config, the keys, the rate limiter and the
/// one database connection.
///
/// The side decides how much is loaded. A process that serves the login routes
/// signs tokens and writes users, so it loads the private key, opens the
/// database read-write and applies the schema. A check-only process decodes
/// tokens and reads users, so it loads only the public key, opens the database
/// without the schema and cannot write it. The private key file is not read and
/// may be absent.
///
/// Returns `Err` for `main` to print and exit 2. The database branches name the
/// path they failed on: the directory that is missing, the database that
/// cannot be opened and the connection that cannot stop waiting for a lock. The
/// config and key branches name the setting or key that failed.
fn build_state(site: &Site, bind: Bind) -> Result<(AppState, m6_auth::Db)> {
    let cfg =
        AuthConfig::load(site.config_path(), site.site_toml()).context("loading auth config")?;

    let keys = if bind.login {
        let private_key_path = cfg.private_key_path.as_deref().ok_or_else(|| {
            anyhow::anyhow!(
                "[keys] private_key is required because this process serves the login routes, which sign tokens: add it to the config, or run with --check-only"
            )
        })?;
        KeyMaterial::load(private_key_path, &cfg.public_key_path, cfg.issuer.clone())
    } else {
        KeyMaterial::load_verify_only(&cfg.public_key_path, cfg.issuer.clone())
    }
    .context("loading key material")?;

    if let Some(parent) = cfg.db_path.parent() {
        if !parent.is_dir() {
            anyhow::bail!(
                "[storage] path: the directory {} does not exist: create it, owned by {}, or fix [storage] path",
                parent.display(),
                m6_core::platform::process::user()
            );
        }
    }
    let db = if bind.login {
        m6_auth::Db::open(&cfg.db_path)
    } else {
        m6_auth::Db::open_read_only(&cfg.db_path)
    }
    .map_err(|e| anyhow::anyhow!("[storage] path: {e}"))?;
    if !bind.login {
        db.schema_present().map_err(|e| {
            anyhow::anyhow!(
                "[storage] path: the database {} has no auth schema, and this process does not create it: start the login side once, or run m6-auth-cli, before --check-only: {e}",
                cfg.db_path.display()
            )
        })?;
    }
    // From here the loop owns the connection: a statement that meets another
    // process's lock must fail at once and be deferred, never wait in the loop.
    db.no_busy_wait().map_err(|e| {
        anyhow::anyhow!(
            "setting the busy timeout to zero on {}: {e}",
            cfg.db_path.display()
        )
    })?;

    m6_core::log::info!(issuer = %cfg.issuer, "auth config loaded");

    Ok((
        AppState {
            keys,
            access_ttl: cfg.access_ttl,
            refresh_ttl: cfg.refresh_ttl,
            issuer: cfg.issuer.clone(),
            rate_limiter: RefCell::new(RateLimiter::with_limits(
                cfg.rate_limit_max_attempts,
                cfg.rate_limit_window_secs,
            )),
        },
        db,
    ))
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

/// The global allocator: mimalloc, which also counts calls when the
/// `count-allocations` feature turns on m6-core's `alloc-count`. The counting
/// is not compiled in a plain build.
#[global_allocator]
static ALLOC: m6_core::alloc::M6Alloc = m6_core::alloc::M6Alloc;

fn main() -> Result<()> {
    // Never hand freed memory back to the OS: the heap grows and does not shrink.
    m6_core::alloc::never_return();
    // Block the signals before anything else: the mask is inherited only by
    // threads created after this point, and logging starts one.
    m6_core::signal::block();

    // ── start: blocking and allocation are fine ──────────────────────────
    // The flags are read before any state is built: `--check-only` selects the
    // verify-only key load and the read-only database open, so a check-only
    // process starts without the private key it never uses.
    let bind = bind_from_args();
    let mut site = Site::load("m6-auth-server", &ROUTES, &[], &NoTemplates);
    m6_core::alloc::reserve(site.limits().get(&m6_core::limits::HEAP_RESERVE).start());
    let read_timeout = site.read_timeout();
    let (state, db) =
        build_state(&site, bind).unwrap_or_else(|e| fail("building the auth state", e));
    let ceiling = raise_fd_limit().unwrap_or_else(|e| fail("raising the open file limit", e));
    if ceiling == 0 {
        fail("the open file limit", "leaves no connection table");
    }
    // A table larger than the descriptors the limit allows could never report
    // itself full: `Slots::is_full` would stay false past the kernel's limit.
    // The connection table follows the `connections` limit, and the open-file
    // limit can lower its maximum.
    let table = site.limits().get(&m6_core::limits::CONNECTIONS);
    let ceiling = ceiling.min(table.max());
    let initial = table.start().min(ceiling);
    let mut poll = Poll::new().unwrap_or_else(|e| fail("creating the poll", e));
    let primary_socket = site.socket_path().to_path_buf();
    let check_socket = check_socket_path(&primary_socket);
    let mut login_listener = bind.login.then(|| {
        listen_unix(&primary_socket, site.socket_mode())
            .unwrap_or_else(|e| fail(&format!("binding {}", primary_socket.display()), e))
    });
    let mut check_listener = bind.check.then(|| {
        listen_unix(&check_socket, site.socket_mode())
            .unwrap_or_else(|e| fail(&format!("binding {}", check_socket.display()), e))
    });
    let mut signals = Signals::new().unwrap_or_else(|e| fail("opening the signal descriptor", e));
    if let Some(listener) = login_listener.as_mut() {
        poll.registry()
            .register(listener, LOGIN_LISTENER, Interest::READABLE)
            .unwrap_or_else(|e| fail("registering the login listener", e));
    }
    if let Some(listener) = check_listener.as_mut() {
        poll.registry()
            .register(listener, CHECK_LISTENER, Interest::READABLE)
            .unwrap_or_else(|e| fail("registering the check listener", e));
    }
    poll.registry()
        .register_fd(signals.raw_fd(), SIGNALS, Interest::READABLE)
        .unwrap_or_else(|e| fail("registering the signal descriptor", e));

    // The started line and the shutdown cleanup name the socket this process
    // binds when it binds only one. A process that binds both names the
    // primary, and removes the check socket after `complete` below.
    let lifecycle_socket = if bind.login {
        primary_socket.clone()
    } else {
        check_socket.clone()
    };
    let mut lifecycle = Lifecycle::new(site.name(), Some(lifecycle_socket));
    lifecycle.started();

    // The whole connection table, every buffer in it, the deadlines and the
    // events: made here, never grown except by `Slots`.
    let read_limit = site.limits().get(&m6_core::limits::REQUEST_BODY).clone();
    let write_limit = site.limits().get(&m6_core::limits::RESPONSE_BUFFER).clone();
    let mut conns = Slots::new(initial, ceiling, move || {
        ServerConn::with_limits(&read_limit, &write_limit)
    });
    // A config with `read_timeout_s = 0` keeps the zero duration and never
    // touches a deadline.
    let mut deadlines = Timeouts::new(initial, read_timeout.unwrap_or(Duration::ZERO));
    // One retry deadline per slot, for a request whose handler met a database
    // lock.
    let mut busy_timeouts = Timeouts::new(initial, BUSY_RETRY);
    let mut retries: Vec<Option<Retry>> = (0..initial).map(|_| None).collect();
    // The listener each connection arrived on, parallel to `retries`.
    let mut conn_sides: Vec<Side> = (0..initial).map(|_| Side::Login).collect();
    let mut events = Events::with_capacity(EVENTS);
    let mut trace = LoopTrace::new(&["accept", "conn", "signal", "timeout", "log"]);
    let mut accepting = [bind.login, bind.check];
    let mut last = m6_core::platform::now();
    let mut last_report = last;

    // ── run: nothing blocks ──────────────────────────────────────────────
    'run: loop {
        // The nearer of the read deadlines, a deferred request's retry and the
        // tick. An interrupted wait is not an error: wait again. mio passes
        // EINTR up rather than retrying.
        let wait = deadlines
            .timeout(m6_core::platform::now(), TICK)
            .min(busy_timeouts.timeout(m6_core::platform::now(), TICK));
        match poll.poll(&mut events, Some(wait)) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
        }
        let now = m6_core::platform::now();
        trace.waited(now - last);
        // One sample per iteration, under the kind of its first event, as
        // m6-http reports the trace.
        let trigger = match events.iter().next().map(|ev| ev.token()) {
            Some(LOGIN_LISTENER) | Some(CHECK_LISTENER) => 0,
            Some(SIGNALS) => 2,
            Some(LOG) => 4,
            Some(_) => 1,
            None => 3,
        };

        for ev in &events {
            match ev.token() {
                // New connections: take a free slot for each, register it,
                // start its read deadline. The side the connection arrived on
                // decides which route set its requests may reach.
                token if token == LOGIN_LISTENER || token == CHECK_LISTENER => {
                    let side = if token == LOGIN_LISTENER {
                        Side::Login
                    } else {
                        Side::Check
                    };
                    let listener = match side {
                        Side::Login => login_listener.as_mut(),
                        Side::Check => check_listener.as_mut(),
                    }
                    .expect("a listener with a registered token is bound");
                    while let Some(taken) = conns.take() {
                        if let Some(new_len) = taken.grew_to {
                            deadlines.grow(new_len);
                            busy_timeouts.grow(new_len);
                            retries.resize_with(new_len, || None);
                            conn_sides.resize_with(new_len, || Side::Login);
                        }
                        let mut stream = match listener.accept() {
                            Ok((stream, _)) => stream,
                            // No connection was pending after all: give
                            // the slot back rather than leaking it.
                            Err(e) => match m6_core::platform::net::accept_error(&e) {
                                m6_core::platform::net::Accept::Drained => {
                                    conns.free(taken.key);
                                    break;
                                }
                                m6_core::platform::net::Accept::SkipOne => {
                                    conns.free(taken.key);
                                    continue;
                                }
                                // Any other error: say so once, give the slot back and stop
                                // accepting for this turn.
                                m6_core::platform::net::Accept::Stop => {
                                    conns.free(taken.key);
                                    eprintln!("m6-auth-server: accept error: {e}");
                                    break;
                                }
                            },
                        };
                        conn_sides[taken.key] = side;
                        poll.registry().register(
                            &mut stream,
                            Token(taken.key),
                            Interest::READABLE,
                        )?;
                        if read_timeout.is_some() {
                            deadlines.touch(Token(taken.key), now);
                        }
                        conns.get_mut(taken.key).attach(stream);
                    }
                    if conns.is_full() {
                        // At the ceiling: stop accepting on both listeners. New
                        // clients wait in the kernel's backlog.
                        for full in Side::BOTH {
                            if !accepting[full.index()] {
                                continue;
                            }
                            let listener = match full {
                                Side::Login => login_listener.as_mut(),
                                Side::Check => check_listener.as_mut(),
                            };
                            if let Some(listener) = listener {
                                poll.registry().deregister(listener)?;
                            }
                            accepting[full.index()] = false;
                        }
                    }
                }
                SIGNALS => {
                    let mut stop = false;
                    while let Some(signal) = signals.read() {
                        if lifecycle.on_signal(signal) == Action::Stop {
                            stop = true;
                        }
                    }
                    if stop {
                        break 'run;
                    }
                }
                LOG => m6_core::log::flush(),
                // One connection's turn, then wait for what it still needs,
                // touch its deadline, or close it. A connection holding a
                // deferred request is not read: its client is waiting for that
                // answer.
                Token(k) => {
                    if retries[k].is_some() {
                        continue;
                    }
                    let conn = conns.get_mut(k);
                    let was = conn.interest();
                    match turn(conn, &mut retries[k], &mut site, &state, &db, conn_sides[k]) {
                        Ok(true) => {
                            touch_retry(&mut busy_timeouts, &retries, k);
                            after_turn(
                                &poll,
                                &mut conns,
                                &mut deadlines,
                                read_timeout,
                                k,
                                was,
                                now,
                            )?;
                        }
                        _ => close(
                            &poll,
                            &mut conns,
                            &mut deadlines,
                            &mut busy_timeouts,
                            &mut retries,
                            k,
                        )?,
                    }
                }
            }
        }

        // A full stdout holds its bytes in the Sink. Watch it for writability
        // only while it does, so the loop never waits on the log.
        if let Err(e) = m6_core::log::watch(poll.registry(), LOG) {
            m6_core::log::error!(error = %e, "log descriptor registration failed");
        }

        // A deferred request: try its handler again when the retry deadline
        // passes, and answer 503 once the database has been locked for a
        // second. The retry keeps the request, so it is not read again.
        while let Some(Token(k)) = busy_timeouts.expired(m6_core::platform::now()) {
            let Some(mut retry) = retries[k].take() else {
                continue;
            };
            let now = m6_core::platform::now();
            let response = match run_route(
                retry.route,
                &retry.req,
                Some(&retry),
                &site,
                &state,
                &db,
                conn_sides[k],
            ) {
                Ok(response) => response,
                Err(busy) if now.duration_since(retry.first) < BUSY_GIVE_UP => {
                    retry.verified = retry.verified.or(busy.verified);
                    retries[k] = Some(retry);
                    // Armed from the end of this attempt, so the loop
                    // polls and serves the other connections before the
                    // next one, even when the attempt itself took longer
                    // than the retry delay.
                    busy_timeouts.touch(Token(k), m6_core::platform::now());
                    continue;
                }
                Err(_) => {
                    m6_core::log::warn!(
                        route = ROUTES[retry.route].pattern,
                        "database still locked after a second"
                    );
                    busy_response()
                }
            };
            let conn = conns.get_mut(k);
            // Capture what the connection was registered for before the answer
            // makes it writable: a partial flush must re-register the interest.
            let was = conn.interest();
            conn.respond_response(response);
            match turn(conn, &mut retries[k], &mut site, &state, &db, conn_sides[k]) {
                Ok(true) => {
                    touch_retry(&mut busy_timeouts, &retries, k);
                    after_turn(&poll, &mut conns, &mut deadlines, read_timeout, k, was, now)?;
                }
                _ => close(
                    &poll,
                    &mut conns,
                    &mut deadlines,
                    &mut busy_timeouts,
                    &mut retries,
                    k,
                )?,
            }
        }

        // Close every connection whose read deadline passed, with no
        // response: it did not send a request in time.
        while let Some(Token(k)) = deadlines.expired(m6_core::platform::now()) {
            close(
                &poll,
                &mut conns,
                &mut deadlines,
                &mut busy_timeouts,
                &mut retries,
                k,
            )?;
        }
        // A slot came free while the table was full: accept again on every
        // listener that was deregistered.
        if !conns.is_full() {
            for side in Side::BOTH {
                if accepting[side.index()] {
                    continue;
                }
                let listener = match side {
                    Side::Login => login_listener.as_mut(),
                    Side::Check => check_listener.as_mut(),
                };
                if let Some(listener) = listener {
                    poll.registry()
                        .register(listener, side.token(), Interest::READABLE)?;
                    accepting[side.index()] = true;
                }
            }
        }

        // The accounting tick: report and clear the loop's wait and work per
        // trigger. This service serves no metrics page, so the report is the
        // tick's own accounting rather than a served number.
        let end = m6_core::platform::now();
        trace.worked(trigger, end - now);
        last = end;
        if end - last_report >= TICK {
            m6_core::log::tick(m6_core::platform::wall());
            m6_core::alloc::note_growth();
            let _ = trace.report();
            last_report = end;
        }
    }

    // ── stop: blocking and allocation are fine ───────────────────────────
    // Connections still open are dropped: a SIGTERM does not wait for a
    // request in flight. `complete` removes the socket it was given and logs
    // the line. The check socket is removed here when the primary was that
    // socket, which is the case in both-listener and login-listener modes.
    lifecycle.complete();
    if bind.check {
        let _ = std::fs::remove_file(&check_socket);
    }
    m6_core::log::stop()?;
    Ok(())
}

/// One connection's turn: read, answer and write until the socket would
/// block. Returns whether the connection stays open.
///
/// A handler that meets a database lock stores its request in `retry` and
/// returns `Ok(true)` with no answer written: the loop tries it again when the
/// retry deadline passes. `side` is the listener the connection arrived on.
fn turn(
    conn: &mut ServerConn,
    retry: &mut Option<Retry>,
    site: &mut Site,
    state: &AppState,
    db: &m6_auth::Db,
    side: Side,
) -> io::Result<bool> {
    loop {
        // Write what is owed before reading anything. A read here would let
        // the next request's answer join output that is still being written.
        // Once the flush empties the buffer, the read below serves any request
        // the client pipelined behind it.
        if conn.interest() == Interest::WRITABLE {
            match conn.flush()? {
                Flush::Done => {}
                Flush::More => return Ok(true),
                Flush::Close => return Ok(false),
            }
        }
        // Made where it is used: the head borrows the slots, and a mutable
        // reference to a long-lived array cannot be reborrowed per turn.
        let mut heads = head_slots();
        let read = conn.read(&mut heads)?;
        // The head borrows the slot, so its answer is built here and written
        // after the match, when the borrow has ended.
        let (response, idle) = match read {
            Read::Request(head) => {
                // The reserved path is answered by `Site` before routing, so
                // the side test in `run_route` never sees it. It belongs to
                // the login side like the four login routes, and a check-only
                // process serves `/check` and nothing else: without this, a
                // peer of the check socket can post `/genid` and make the
                // process run its start path again.
                let handled = if side == Side::Check && head.path == GENID_PATH {
                    Handled::Done(site.not_found(&head))
                } else {
                    site.handle(&head)
                };
                let response = match handled {
                    Handled::Done(response) => response,
                    // This service's four routes, by the index they were given
                    // to `Site::load` in.
                    Handled::Code(index, req) => {
                        match run_route(index, &req, None, site, state, db, side) {
                            Ok(response) => response,
                            Err(busy) => {
                                *retry = Some(Retry {
                                    route: index,
                                    req,
                                    // The one-second limit starts when the
                                    // database first refused the statement.
                                    first: m6_core::platform::now(),
                                    verified: busy.verified,
                                });
                                return Ok(true);
                            }
                        }
                    }
                    // The reserved path: run the start path again and assign
                    // the new Site in the loop's own binding, or keep serving
                    // the old one and answer the cause.
                    Handled::Reload(reload) => {
                        let response = match site.reload(&ROUTES, &[], &NoTemplates) {
                            Ok(next) => {
                                *site = next;
                                genid_ok()
                            }
                            Err(e) => genid_failed(&e),
                        };
                        m6_core::site::finish_reload(&reload, response)
                    }
                    Handled::Named(..) => unreachable!(
                        "m6-auth-server registers no named handlers, so every route is in code"
                    ),
                };
                (Some(response), false)
            }
            // Nothing complete yet; a refusal may have been queued for a
            // malformed request, and is written below.
            Read::More => (None, true),
            Read::Closed => return Ok(false),
        };
        if let Some(response) = response {
            conn.respond_response(response);
        }
        match conn.flush()? {
            // The answer is out and the connection keeps alive. A pipelined
            // request may already be buffered, so read again before waiting.
            Flush::Done if !idle => {}
            // Nothing more to read this turn, or the socket would block with
            // output still queued: the caller re-registers for what is left.
            Flush::Done | Flush::More => return Ok(true),
            // Everything is written and this connection closes.
            Flush::Close => return Ok(false),
        }
    }
}

/// Run the handler for route `index` and finish its answer, or report the
/// database busy so the loop can defer the request.
///
/// A route that belongs to the other listener answers the house 404 before its
/// handler runs. That is what keeps a login's bcrypt off a check-only process
/// even though both route sets are compiled into every process.
fn run_route(
    index: usize,
    req: &Request,
    retry: Option<&Retry>,
    site: &Site,
    state: &AppState,
    db: &m6_auth::Db,
    side: Side,
) -> std::result::Result<Response, Busy> {
    if ROUTE_SIDE[index] != side {
        // Finished with no route, so the answer carries no route
        // `Cache-Control` and no route headers. `site.finish` would find the
        // route again from the path and make this 404 distinguishable from an
        // unknown path, which says the path exists on the other socket.
        return Ok(site.not_found_owned(req));
    }
    let response = match index {
        0 => handlers::dispatch_login(
            req.raw(),
            state,
            db,
            &peer_ip(req),
            retry.and_then(|r| r.verified.as_ref()),
        ),
        1 => handlers::dispatch_refresh(req.raw(), state, db),
        2 => handlers::dispatch_logout(req.raw(), state, db),
        3 => Ok(handlers::dispatch_public_key(state)),
        4 => Ok(handlers::dispatch_check(req.raw(), state, db)),
        _ => unreachable!("ROUTES has five routes and Handled::Code carries one of their indexes"),
    }?;
    Ok(site.finish(req, response.into()))
}

/// What a finished turn leaves behind: re-register a connection whose interest
/// changed, and touch the read deadline of one that stays open.
fn after_turn(
    poll: &Poll,
    conns: &mut Slots<ServerConn>,
    deadlines: &mut Timeouts,
    read_timeout: Option<Duration>,
    k: usize,
    was: Interest,
    now: Instant,
) -> io::Result<()> {
    let conn = conns.get_mut(k);
    if conn.interest() != was {
        let interest = conn.interest();
        let stream = conn
            .stream()
            .expect("every registered token has an attached connection");
        poll.registry().reregister(stream, Token(k), interest)?;
    }
    if read_timeout.is_some() {
        deadlines.touch(Token(k), now);
    }
    Ok(())
}

/// Wake the loop at the retry deadline when the turn deferred this
/// connection's request.
fn touch_retry(busy_timeouts: &mut Timeouts, retries: &[Option<Retry>], k: usize) {
    if retries[k].is_some() {
        busy_timeouts.touch(Token(k), m6_core::platform::now());
    }
}

/// Close a connection: forget any deferred request and its retry deadline, then
/// detach and free the slot.
fn close(
    poll: &Poll,
    conns: &mut Slots<ServerConn>,
    deadlines: &mut Timeouts,
    busy_timeouts: &mut Timeouts,
    retries: &mut [Option<Retry>],
    k: usize,
) -> io::Result<()> {
    busy_timeouts.clear(Token(k));
    retries[k] = None;
    m6_core::h1::close_conn(poll, conns, deadlines, k)
}

/// The answer once a deferred request has waited a second for the database.
fn busy_response() -> Response {
    let retry_after = BUSY_GIVE_UP.as_secs().to_string();
    Response::status(503)
        .header("Retry-After", &retry_after)
        .body("Service Unavailable")
}

/// A failure at start is an error: print it and exit 2.
fn fail(what: &str, error: impl std::fmt::Display) -> ! {
    eprintln!("m6-auth-server: {what}: {error:#}");
    std::process::exit(2)
}
