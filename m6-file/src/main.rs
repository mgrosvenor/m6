//! The static file service: one process and one event loop.
//!
//! One handler, registered by name, and every route it answers comes from
//! config.
//!
//! ```toml
//! [[route]]
//! path = "/assets/{*relpath}"
//! handler = "files"
//! root = "assets/"
//! ```
//!
//! The loop accepts on the listener's readability, reads and answers each
//! connection until its socket would block, and waits for the next readiness
//! event. Each connection slot keeps its own buffers, made at start. A file
//! larger than a slot's output buffer is streamed: one `read(2)` of the file
//! per writable turn, so a slow client holds its own connection and no other.
//!
//! A stop closes the listener and answers what was already accepted, up to
//! `[server] drain_timeout_s`, then exits. See [`m6_core::drain`].

#![deny(unsafe_code)]

mod compress;
mod handler;
mod site_size;

use std::io;
use std::time::Duration;

use anyhow::Result;
use m6_core::drain::{Drain, Step};
use m6_core::event::{LoopTrace, Slots, Timeouts};
use m6_core::h1::{head_slots, Flush, Read, ServerConn};
use m6_core::net::listen_unix;
use m6_core::platform::process::raise_fd_limit;
use m6_core::platform::Signals;
use m6_core::platform::{Events, Interest, Poll, Token};
use m6_core::signal::{Action, Lifecycle};
use m6_core::{genid_failed, genid_ok, Handled, NoTemplates, Site};

const LISTENER: Token = Token(usize::MAX);
const SIGNALS: Token = Token(usize::MAX - 1);
/// The operational log's stdout, registered while it holds bytes a full
/// destination would not take.
const LOG: Token = Token(usize::MAX - 2);
/// The accounting tick, and the longest the loop waits when nothing happens.
const TICK: Duration = Duration::from_secs(1);
const EVENTS: usize = 256;

/// The handler this service registers, by the name config routes with
/// `handler = "files"`.
const HANDLER: &str = "files";

/// The global allocator: mimalloc, which also counts calls when the
/// `count-allocations` feature turns on m6-core's `alloc-count`. The counting
/// is not compiled in a plain build.
#[global_allocator]
static ALLOC: m6_core::alloc::M6Alloc = m6_core::alloc::M6Alloc;

fn main() -> Result<()> {
    // `--version` and `--version-full` answer from constants fixed at build
    // time, before a config path is required.
    m6_core::version_flags!(m6_core::build_info::linked());

    // Never hand freed memory back to the OS: the heap grows and does not shrink.
    m6_core::alloc::never_return();
    // Block the signals before anything else: the mask is inherited only by
    // threads created after this point, and logging starts one.
    m6_core::signal::block();

    // ── start: blocking and allocation are fine ──────────────────────────
    let mut site = Site::load("m6-file", &[], &[HANDLER], &NoTemplates);
    // The minified bodies a file version produces are this service's only
    // cache, and they are the hot data the heap reservation exists for. The
    // reservation is the cache plus headroom unless the config sets one, the
    // rule m6-http uses, so a default start reserves for the whole cache and
    // the bound the record prints is the bound the cache has. An explicit
    // reservation below the cache still bounds it, and says so once.
    let (reserve, cache_warning) = m6_core::limits::heap_reserve(site.limits());
    m6_core::alloc::reserve(reserve);

    // What the document roots carry is a start question, asked once. A leak
    // here is a file nobody meant to publish, and a route's root is the only
    // tree it can reach, so the answer cannot wait for a request.
    let site_dir = site.site_dir().to_path_buf();
    let roots = site_size::served_roots(&site_dir, &site.config().routes);
    let report = match site_size::inspect(&roots) {
        Ok(report) => report,
        Err(e) => fail(
            &format!("reading the document root {}", site_dir.display()),
            e,
        ),
    };
    if !report.refused.is_empty() {
        eprintln!("{}", site_size::refusal(&site_dir, &report.refused));
        std::process::exit(2);
    }
    if !report.warned.is_empty() {
        eprintln!("{}", site_size::warning(&site_dir, &report.warned));
    }
    let budget_mb = site
        .config()
        .site_size_budget_mb
        .unwrap_or(site_size::DEFAULT_BUDGET_MB);
    if report.bytes > site_size::budget_bytes(budget_mb) {
        eprintln!("{}", site_size::over_budget(&site_dir, &report, budget_mb));
    }

    let read_timeout = site.read_timeout();
    handler::set_tail_limit(site.limits().get(&m6_core::limits::TAIL_READ));
    // An explicit reservation below the cache is the one case where the cache
    // is smaller than the max the record prints. The line names both keys and
    // the value that would hold the whole cache.
    let cache = site.limits().get(&m6_core::limits::CACHE_BYTES);
    let cache_limit = cache.max().min(reserve);
    if let Some(warning) = cache_warning {
        eprintln!("{}: {warning}", site.name());
    }
    handler::set_minify_cache_limit(cache_limit);
    let descriptors = raise_fd_limit().unwrap_or_else(|e| fail("raising the open file limit", e));
    // A streamed file holds a second descriptor for as long as its body is
    // being written, so the table takes half of what the limit leaves: one
    // descriptor per connection and one per streamed body. A larger table
    // could never report itself full before the kernel runs out, because
    // `Slots::is_full` counts connections and not the files behind them.
    let ceiling = descriptors / 2;
    if ceiling == 0 {
        fail(
            "the open file limit",
            "leaves no connection table and no room for a streamed file",
        );
    }
    // The connection table follows the `connections` limit, and the open-file
    // limit can lower its maximum.
    let table = site.limits().get(&m6_core::limits::CONNECTIONS);
    let ceiling = ceiling.min(table.max());
    let initial = table.start().min(ceiling);
    let mut poll = Poll::new().unwrap_or_else(|e| fail("creating the poll", e));
    // The signal descriptor opens before the socket exists. Its existence is how
    // a caller knows the service is ready, so a caller that waits for the socket
    // and then sends SIGTERM must find the handler armed. A stop that arrives
    // while the mask is blocked and no descriptor is open is discarded on
    // macOS, and the service would then ignore the stop that followed its own
    // bind.
    let mut signals = Signals::new().unwrap_or_else(|e| fail("opening the signal descriptor", e));
    let mut listener = listen_unix(site.socket_path(), site.socket_mode())
        .unwrap_or_else(|e| fail(&format!("binding {}", site.socket_path().display()), e));
    // The reserve the accept loop spends to refuse a connection when the
    // process is out of descriptors. Opened here, before any pressure, and on
    // the listener's own socket family.
    let mut spare = m6_core::platform::net::SpareDescriptor::for_unix()
        .unwrap_or_else(|e| fail("opening the reserve descriptor", e));
    let mut refusals = m6_core::net::Refusals::new();
    poll.registry()
        .register(&mut listener, LISTENER, Interest::READABLE)
        .unwrap_or_else(|e| fail("registering the listener", e));
    poll.registry()
        .register_fd(signals.raw_fd(), SIGNALS, Interest::READABLE)
        .unwrap_or_else(|e| fail("registering the signal descriptor", e));

    let mut lifecycle = Lifecycle::new(site.name(), Some(site.socket_path().to_path_buf()));
    lifecycle.started();

    // The whole connection table, every buffer in it, the deadlines and the
    // events: made here, never grown except by `Slots`.
    let read_limit = site.limits().get(&m6_core::limits::REQUEST_BODY).clone();
    let write_limit = site.limits().get(&m6_core::limits::RESPONSE_BUFFER).clone();
    let mut conns = Slots::new(initial, ceiling, move || {
        ServerConn::with_limits(&read_limit, &write_limit)
    });
    // Accepted work and the shutdown drain. The count is taken at the accept,
    // so a request the listener took and no turn has read yet still holds
    // shutdown open.
    let drain_timeout = site.drain_timeout();
    let mut drain = Drain::new(drain_timeout, initial);
    // A config with `read_timeout_s = 0` keeps the zero duration and never
    // touches a deadline.
    let mut deadlines = Timeouts::new(initial, read_timeout.unwrap_or(Duration::ZERO));
    let mut events = Events::with_capacity(EVENTS);
    let mut trace = LoopTrace::new(&["accept", "conn", "signal", "timeout", "log"]);
    let mut accepting = true;
    let mut last = m6_core::platform::now();
    let mut last_report = last;

    // ── run: nothing blocks ──────────────────────────────────────────────
    'run: loop {
        // While draining, the wait is capped at the deadline: nothing else
        // would wake the loop to notice it.
        let mut wait = deadlines.timeout(m6_core::platform::now(), TICK);
        if let Some(left) = drain.poll_timeout(m6_core::platform::now()) {
            wait = wait.min(left);
        }
        // An interrupted wait is not an error: wait again. mio passes EINTR
        // up rather than retrying.
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
            Some(LISTENER) => 0,
            Some(SIGNALS) => 2,
            Some(LOG) => 4,
            Some(_) => 1,
            None => 3,
        };

        for ev in &events {
            match ev.token() {
                // New connections: take a free slot for each, register it,
                // start its read deadline.
                LISTENER => {
                    let mut served = false;
                    while let Some(taken) = conns.take() {
                        if let Some(new_len) = taken.grew_to {
                            deadlines.grow(new_len);
                            drain.grown(new_len);
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
                                // Out of descriptors: refuse the connection
                                // that is already queued by spending the
                                // reserve, so it is not left waiting for a
                                // wakeup that a full queue cannot deliver.
                                // Then keep draining.
                                m6_core::platform::net::Accept::Exhausted => {
                                    conns.free(taken.key);
                                    match spare.reject_one(|| {
                                        let (stream, _) = listener.accept()?;
                                        drop(stream);
                                        Ok(())
                                    }) {
                                        Ok(m6_core::platform::net::RejectOutcome::Refused) => {
                                            refusals.refused();
                                            continue;
                                        }
                                        Ok(m6_core::platform::net::RejectOutcome::Nothing) => {
                                            continue
                                        }
                                        Ok(m6_core::platform::net::RejectOutcome::Failed(e)) => {
                                            m6_core::log::warn!(error = %e, "accept failed while refusing a queued connection");
                                            poll.registry().reregister(
                                                &mut listener,
                                                LISTENER,
                                                Interest::READABLE,
                                            )?;
                                            break;
                                        }
                                        Err(e) => {
                                            // The reserve is gone, so the next
                                            // shortage cannot be handled.
                                            // Arm the listener and stop
                                            // draining rather than spinning.
                                            m6_core::log::warn!(
                                                error = %e,
                                                "the reserve descriptor could not be reopened"
                                            );
                                            poll.registry().reregister(
                                                &mut listener,
                                                LISTENER,
                                                Interest::READABLE,
                                            )?;
                                            break;
                                        }
                                    }
                                }
                                // Any other error: say so once and keep the
                                // listener armed, so a connection still
                                // queued is reported again rather than left
                                // edge-triggered and unread.
                                m6_core::platform::net::Accept::Stop => {
                                    conns.free(taken.key);
                                    m6_core::log::warn!(error = %e, "accept failed");
                                    poll.registry().reregister(
                                        &mut listener,
                                        LISTENER,
                                        Interest::READABLE,
                                    )?;
                                    break;
                                }
                            },
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
                        // Counted here, at the accept, not when a turn reads
                        // the request: the window between the two is what
                        // issue #338 measured.
                        drain.begin(taken.key);
                        served = true;
                    }
                    let counts = if served {
                        refusals.flush(now)
                    } else {
                        refusals.due(now)
                    };
                    if let Some(counts) = counts {
                        m6_core::net::warn_refused("m6-file", counts);
                    }
                    if conns.is_full() {
                        // At the ceiling: stop accepting. New clients wait in
                        // the kernel's backlog.
                        poll.registry().deregister(&mut listener)?;
                        accepting = false;
                    }
                }
                SIGNALS => {
                    while let Some(signal) = signals.read() {
                        if lifecycle.on_signal(signal) == Action::Stop {
                            // Stop accepting now, then answer what was already
                            // accepted, until the count is zero or the
                            // deadline passes. Nothing accepted means nothing
                            // to wait for.
                            stop_accepting(poll.registry(), &mut listener, &mut accepting)?;
                            // Do not leave here even when nothing is in
                            // flight: the zero-timeout turn the drain gives
                            // the loop is what reads a request already in a
                            // kept-alive socket. `step` decides, after it.
                            drain.start(now);
                        }
                    }
                }
                LOG => m6_core::log::flush(),
                // One connection's turn, then wait for what it still needs,
                // touch its deadline, or close it.
                Token(k) => {
                    // A turn on the connection is accepted work. An idle
                    // connection that sent another request is counted here,
                    // so a request that arrives during the drain is answered.
                    drain.begin(k);
                    let conn = conns.get_mut(k);
                    let was = conn.interest();
                    match turn(conn, &mut site) {
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
                            // Answered and holding nothing: idle, so it stops
                            // holding the drain.
                            if conn.served() > 0 && !conn.pending() {
                                drain.end(k);
                            }
                        }
                        _ => close(&poll, &mut conns, &mut deadlines, &mut drain, k)?,
                    }
                }
            }
        }

        // A full stdout holds its bytes in the Sink. Watch it for writability
        // only while it does, so the loop never waits on the log.
        if let Err(e) = m6_core::log::watch(poll.registry(), LOG) {
            m6_core::log::error!(error = %e, "log descriptor registration failed");
        }

        // Close every connection whose read deadline passed, with no
        // response: it did not send a request in time.
        while let Some(Token(k)) = deadlines.expired(m6_core::platform::now()) {
            close(&poll, &mut conns, &mut deadlines, &mut drain, k)?;
        }

        // The drain: leave the turn the last accepted request finishes, or at
        // the deadline, and say how many were left when it was the deadline.
        if drain.draining() {
            match drain.step(m6_core::platform::now()) {
                Step::Draining => {}
                Step::Finished => break 'run,
                Step::DeadlinePassed(unfinished) => {
                    m6_core::log::warn!(
                        unfinished,
                        timeout_s = drain_timeout.as_secs(),
                        "drain deadline passed with requests still in flight"
                    );
                    break 'run;
                }
            }
        }

        // A slot came free while the table was full: accept again. Never
        // while draining: the listener stays closed.
        if !accepting && !drain.draining() && !conns.is_full() {
            poll.registry()
                .register(&mut listener, LISTENER, Interest::READABLE)?;
            accepting = true;
        }

        // The accounting tick: report and clear the loop's wait and work per
        // trigger. These services serve no metrics page, so the report is the
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
    // Reaching here means nothing accepted is left, or the drain deadline
    // passed and the warning already named how many were. `complete` removes
    // the socket and logs the line.
    //
    // A refusal the rate limit was still holding back when the loop ended is
    // written here. There is no later turn to carry it, so the interval does
    // not apply: the last line reports every count the process took.
    m6_core::net::close_refusals("m6-file", &mut refusals);
    lifecycle.complete();
    m6_core::log::stop()?;
    Ok(())
}

/// Stop accepting, so new clients wait in the kernel's backlog.
fn stop_accepting(
    registry: &m6_core::platform::Registry,
    listener: &mut m6_core::platform::net::UnixListener,
    accepting: &mut bool,
) -> io::Result<()> {
    if *accepting {
        registry.deregister(listener)?;
        *accepting = false;
    }
    Ok(())
}

/// Close one connection and stop counting its accepted work.
fn close(
    poll: &Poll,
    conns: &mut Slots<ServerConn>,
    deadlines: &mut Timeouts,
    drain: &mut Drain,
    k: usize,
) -> io::Result<()> {
    drain.end(k);
    m6_core::h1::close_conn(poll, conns, deadlines, k)
}

/// One connection's turn: read, answer and write until the socket would
/// block. Returns whether the connection stays open.
fn turn(conn: &mut ServerConn, site: &mut Site) -> io::Result<bool> {
    loop {
        // Write what is owed before reading anything. A streamed body is not
        // in the output buffer, so a read here would let the next request's
        // answer replace it mid-body. Once the flush empties the buffer, the
        // read below serves any request the client pipelined behind it.
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
                    // The reserved path: run the start path again and assign
                    // the new Site in the loop's own binding, or keep serving
                    // the old one and answer the cause.
                    Handled::Reload(reload) => {
                        let response = match site.reload(&[], &[HANDLER], &NoTemplates) {
                            Ok(next) => {
                                *site = next;
                                genid_ok()
                            }
                            Err(e) => genid_failed(&e),
                        };
                        m6_core::site::finish_reload(&reload, response)
                    }
                    // The one handler this service registers.
                    Handled::Named(_, req) => {
                        let response = handler::serve(&req)
                            .unwrap_or_else(|e| m6_core::response::error_to_response(&e));
                        site.finish(&req, response)
                    }
                    Handled::Code(..) => unreachable!(
                        "m6-file passes no code routes, so every route names a handler"
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
    eprintln!("m6-file: {what}: {error}");
    std::process::exit(2)
}
