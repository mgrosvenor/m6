//! The page service: one process and one event loop.
//!
//! The loop accepts on the listener's readability, reads and answers each
//! connection until its socket would block, and waits for the next readiness
//! event. Each connection slot keeps its own buffers, made at start, so
//! serving a request allocates only where the page path does.
//!
//! Routes come from config. m6-html passes no code routes and no handler
//! names, so every route it serves is a template.

#![deny(unsafe_code)]

use std::io;
use std::time::Duration;

use anyhow::Result;
use m6_core::event::{LoopTrace, Slots, Timeouts};
use m6_core::h1::{head_slots, Flush, Read, ServerConn};
use m6_core::net::listen_unix;
use m6_core::platform::process::raise_fd_limit;
use m6_core::platform::Signals;
use m6_core::platform::{Events, Interest, Poll, Token};
use m6_core::signal::{Action, Lifecycle};
use m6_core::{genid_failed, genid_ok, Handled, Site, TeraFactory};

const LISTENER: Token = Token(usize::MAX);
const SIGNALS: Token = Token(usize::MAX - 1);
/// The operational log's stdout, registered while it holds bytes a full
/// destination would not take.
const LOG: Token = Token(usize::MAX - 2);
/// The accounting tick, and the longest the loop waits when nothing happens.
const TICK: Duration = Duration::from_secs(1);
const EVENTS: usize = 256;

/// The global allocator: mimalloc, which also counts calls when the
/// `count-allocations` feature turns on m6-core's `alloc-count`. The counting
/// is not compiled in a plain build.
#[global_allocator]
static ALLOC: m6_core::alloc::M6Alloc = m6_core::alloc::M6Alloc;

fn main() -> Result<()> {
    // `--version` and `--version-full` answer from constants fixed at build
    // time, before a config path is required.
    m6_core::version_flags!(m6_core::build_info::linked());
    // The start-up record's identity line: the binary first, then each m6
    // crate it links. The record itself is written once logging is up.
    m6_core::startup_identity!(m6_core::build_info::linked());

    // Never hand freed memory back to the OS: the heap grows and does not shrink.
    m6_core::alloc::never_return();
    // Block the signals before anything else: the mask is inherited only by
    // threads created after this point, and logging starts one.
    m6_core::signal::block();

    // ── start: blocking and allocation are fine ──────────────────────────
    let mut site = Site::load("m6-html", &[], &[], &TeraFactory);
    m6_core::alloc::reserve(site.limits().get(&m6_core::limits::HEAP_RESERVE).start());
    let read_timeout = site.read_timeout();
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
    // The signal descriptor opens before the socket exists. Its existence is how
    // a caller knows the service is ready, so a caller that waits for the socket
    // and then sends SIGTERM must find the handler armed. A stop that arrives
    // while the mask is blocked and no descriptor is open is discarded on
    // macOS, and the service would then ignore the stop that followed its own
    // bind.
    let mut signals = Signals::new().unwrap_or_else(|e| fail("opening the signal descriptor", e));
    let mut listener = listen_unix(site.socket_path(), site.socket_mode())
        .unwrap_or_else(|e| fail(&format!("binding {}", site.socket_path().display()), e));
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
        // An interrupted wait is not an error: wait again. mio passes EINTR
        // up rather than retrying.
        match poll.poll(
            &mut events,
            Some(deadlines.timeout(m6_core::platform::now(), TICK)),
        ) {
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
                    while let Some(taken) = conns.take() {
                        if let Some(new_len) = taken.grew_to {
                            deadlines.grow(new_len);
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
                                    eprintln!("m6-html: accept error: {e}");
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
                LOG => m6_core::log::flush(),
                // One connection's turn, then wait for what it still needs,
                // touch its deadline, or close it.
                Token(k) => {
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
                        }
                        _ => m6_core::h1::close_conn(&poll, &mut conns, &mut deadlines, k)?,
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
            m6_core::h1::close_conn(&poll, &mut conns, &mut deadlines, k)?;
        }
        // A slot came free while the table was full: accept again.
        if !accepting && !conns.is_full() {
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
    // Connections still open are dropped: a SIGTERM does not wait for a
    // request in flight. `complete` removes the socket and logs the line.
    lifecycle.complete();
    m6_core::log::stop()?;
    Ok(())
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
                        let response = match site.reload(&[], &[], &TeraFactory) {
                            Ok(next) => {
                                *site = next;
                                genid_ok()
                            }
                            Err(e) => genid_failed(&e),
                        };
                        m6_core::site::finish_reload(&reload, response)
                    }
                    Handled::Code(..) | Handled::Named(..) => unreachable!(
                        "m6-html passes no code routes and no handler names, so every route \
                         is answered by its template"
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
    eprintln!("m6-html: {what}: {error}");
    std::process::exit(2)
}
