//! The authentication service: one process, one thread and one event loop.
//!
//! The loop serves four routes, all literal paths, over one SQLite connection
//! opened at start and kept for the life of the process. The signing keys, the
//! token lifetimes and the rate limiter are plain fields the loop owns. A login
//! holds the process for its password verify, which is CPU work with no
//! descriptor to wait on.
//!
//! **The connection model.** Connections beyond what the process can serve wait
//! in the kernel's listen queue. The login rate limit is per process and in
//! memory, keyed on the address the proxy forwarded.

#![deny(unsafe_code)]

mod config;
mod handlers;
mod jwt;
mod keys;
mod rate_limit;

use std::cell::RefCell;
use std::io;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use m6_core::app::RouteMethod;
use m6_core::event::{raise_open_file_limit, LoopTrace, Signals, Slots, Timeouts};
use m6_core::h1::{head_slots, Flush, Read, ServerConn};
use m6_core::mio::unix::SourceFd;
use m6_core::mio::{Events, Interest, Poll, Token};
use m6_core::net::listen_unix;
use m6_core::prelude::*;
use m6_core::signal::{Action, Lifecycle};

use config::AuthConfig;
use handlers::AppState;
use keys::KeyMaterial;
use rate_limit::RateLimiter;

const LISTENER: Token = Token(usize::MAX);
const SIGNALS: Token = Token(usize::MAX - 1);
/// The accounting tick, and the longest the loop waits when nothing happens.
const TICK: Duration = Duration::from_secs(1);
const EVENTS: usize = 256;
/// Slots and buffers made at start, before any connection exists.
const INITIAL_CONNECTIONS: usize = 1024;
const READ_CAP: usize = 16 * 1024;
const WRITE_CAP: usize = 64 * 1024;

/// The four routes, in the order `Handled::Code` indexes them.
const ROUTES: [CodeRoute; 4] = [
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
];

/// Build what the loop owns: the config, the keys, the rate limiter and the
/// one database connection.
///
/// A failure here, like a failure to open the database, is a start failure: it
/// exits 2 with the path that failed.
fn build_state(site: &Site) -> Result<(AppState, m6_auth::Db)> {
    let cfg =
        AuthConfig::load(site.config_path(), site.site_toml()).context("loading auth config")?;

    let keys = KeyMaterial::load(
        &cfg.private_key_path,
        &cfg.public_key_path,
        cfg.issuer.clone(),
    )
    .context("loading key material")?;

    if let Some(parent) = cfg.db_path.parent() {
        std::fs::create_dir_all(parent).context("creating db directory")?;
    }
    let db = m6_auth::Db::open(&cfg.db_path)
        .map_err(|e| anyhow::anyhow!("opening the database {}: {e}", cfg.db_path.display()))?;

    tracing::info!(issuer = %cfg.issuer, "auth config loaded");

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

fn main() -> Result<()> {
    // Block the signals before anything else: the mask is inherited only by
    // threads created after this point, and logging starts one.
    m6_core::signal::block();

    // ── start: blocking and allocation are fine ──────────────────────────
    let site = Site::load("m6-auth-server", &ROUTES, &[], &NoTemplates);
    let read_timeout = site.read_timeout();
    let (state, db) = build_state(&site).unwrap_or_else(|e| fail("building the auth state", e));
    let ceiling =
        raise_open_file_limit().unwrap_or_else(|e| fail("raising the open file limit", e));
    if ceiling == 0 {
        fail("the open file limit", "leaves no connection table");
    }
    // A table larger than the descriptors the limit allows could never report
    // itself full: `Slots::is_full` would stay false past the kernel's limit.
    let initial = INITIAL_CONNECTIONS.min(ceiling);
    let mut poll = Poll::new().unwrap_or_else(|e| fail("creating the poll", e));
    let mut listener = listen_unix(site.socket_path(), site.socket_mode())
        .unwrap_or_else(|e| fail(&format!("binding {}", site.socket_path().display()), e));
    let mut signals = Signals::new().unwrap_or_else(|e| fail("opening the signal descriptor", e));
    poll.registry()
        .register(&mut listener, LISTENER, Interest::READABLE)
        .unwrap_or_else(|e| fail("registering the listener", e));
    poll.registry()
        .register(
            &mut SourceFd(&signals.raw_fd()),
            SIGNALS,
            Interest::READABLE,
        )
        .unwrap_or_else(|e| fail("registering the signal descriptor", e));

    let mut lifecycle = Lifecycle::new(site.name(), Some(site.socket_path().to_path_buf()));
    lifecycle.started();

    // The whole connection table, every buffer in it, the deadlines and the
    // events: made here, never grown except by `Slots`.
    let mut conns = Slots::new(initial, ceiling, || ServerConn::new(READ_CAP, WRITE_CAP));
    // The deadline is only read when a connection is touched, and a config
    // without `read_timeout_s` touches none.
    let mut deadlines = Timeouts::new(initial, read_timeout.unwrap_or(Duration::ZERO));
    let mut events = Events::with_capacity(EVENTS);
    let mut trace = LoopTrace::new(&["accept", "conn", "signal", "timeout"]);
    let mut accepting = true;
    let mut last = Instant::now();
    let mut last_report = last;

    // ── run: nothing blocks ──────────────────────────────────────────────
    'run: loop {
        // An interrupted wait is not an error: wait again. mio passes EINTR
        // up rather than retrying.
        match poll.poll(&mut events, Some(deadlines.timeout(Instant::now(), TICK))) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
        }
        let now = Instant::now();
        trace.waited(now - last);
        // One sample per iteration, under the kind of its first event, as
        // m6-http reports the trace.
        let trigger = match events.iter().next().map(|ev| ev.token()) {
            Some(LISTENER) => 0,
            Some(SIGNALS) => 2,
            Some(_) => 1,
            None => 3,
        };

        for ev in &events {
            match ev.token() {
                // New connections: take a free slot for each, register it,
                // start its read deadline.
                LISTENER => {
                    while let Some(taken) = conns.take() {
                        if let Some(new_len) = taken.grew_to {
                            deadlines.grow(new_len);
                        }
                        let mut stream = match listener.accept() {
                            Ok((stream, _)) => stream,
                            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                                // No connection was pending after all: give
                                // the slot back rather than leaking it.
                                conns.free(taken.key);
                                break;
                            }
                            Err(e) => {
                                // A failure here sheds this accept pass and
                                // not the service: give the slot back, say so
                                // once, and wait for the next listener event.
                                // The budget leaves room for every connection,
                                // so this is a transient error outside the
                                // table.
                                conns.free(taken.key);
                                eprintln!("m6-auth-server: accept error: {e}");
                                break;
                            }
                        };
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
                        // At the ceiling: stop accepting. New clients wait in
                        // the kernel's backlog.
                        poll.registry().deregister(&mut listener)?;
                        accepting = false;
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
                // One connection's turn, then wait for what it still needs,
                // touch its deadline, or close it.
                Token(k) => {
                    let conn = conns.get_mut(k);
                    let was = conn.interest();
                    match turn(conn, &site, &state, &db) {
                        Ok(true) => {
                            // Re-register only when the turn changed what the
                            // connection waits for: a kept-alive request that
                            // read and wrote everything leaves READABLE alone.
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
                        }
                        _ => m6_core::h1::close_conn(&poll, &mut conns, &mut deadlines, k)?,
                    }
                }
            }
        }

        // Close every connection whose read deadline passed, with no
        // response: it did not send a request in time.
        while let Some(Token(k)) = deadlines.expired(Instant::now()) {
            m6_core::h1::close_conn(&poll, &mut conns, &mut deadlines, k)?;
        }
        // A slot came free while the table was full: accept again.
        if !accepting && !conns.is_full() {
            poll.registry()
                .register(&mut listener, LISTENER, Interest::READABLE)?;
            accepting = true;
        }

        // The accounting tick: report and clear the loop's wait and work per
        // trigger. This service serves no metrics page, so the report is the
        // tick's own accounting rather than a served number.
        let end = Instant::now();
        trace.worked(trigger, end - now);
        last = end;
        if end - last_report >= TICK {
            let _ = trace.report();
            last_report = end;
        }
    }

    // ── stop: blocking and allocation are fine ───────────────────────────
    // Connections still open are dropped: a SIGTERM does not wait for a
    // request in flight. `complete` removes the socket and logs the line.
    lifecycle.complete();
    Ok(())
}

/// One connection's turn: read, answer and write until the socket would
/// block. Returns whether the connection stays open.
fn turn(
    conn: &mut ServerConn,
    site: &Site,
    state: &AppState,
    db: &m6_auth::Db,
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
                let response = match site.handle(&head) {
                    Handled::Done(response) => response,
                    // This service's four routes, by the index they were given
                    // to `Site::load` in.
                    Handled::Code(index, req) => {
                        let response = match index {
                            0 => handlers::dispatch_login(req.raw(), state, db, &peer_ip(&req)),
                            1 => handlers::dispatch_refresh(req.raw(), state, db),
                            2 => handlers::dispatch_logout(req.raw(), state, db),
                            3 => handlers::dispatch_public_key(state),
                            _ => unreachable!(
                                "ROUTES has four routes and Handled::Code carries one of \
                                 their indexes"
                            ),
                        };
                        site.finish(&req, response.into())
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

/// A failure at start is an error: print it and exit 2.
fn fail(what: &str, error: impl std::fmt::Display) -> ! {
    eprintln!("m6-auth-server: {what}: {error:#}");
    std::process::exit(2)
}
