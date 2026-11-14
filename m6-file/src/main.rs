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

#![deny(unsafe_code)]

mod compress;
mod handler;

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
use m6_core::{Handled, NoTemplates, Site};

const LISTENER: Token = Token(usize::MAX);
const SIGNALS: Token = Token(usize::MAX - 1);
/// The accounting tick, and the longest the loop waits when nothing happens.
const TICK: Duration = Duration::from_secs(1);
const EVENTS: usize = 256;
/// Slots and buffers made at start, before any connection exists.
const INITIAL_CONNECTIONS: usize = 1024;
const READ_CAP: usize = 16 * 1024;
const WRITE_CAP: usize = 64 * 1024;

/// The handler this service registers, by the name config routes with
/// `handler = "files"`.
const HANDLER: &str = "files";

fn main() -> Result<()> {
    // Block the signals before anything else: the mask is inherited only by
    // threads created after this point, and logging starts one.
    m6_core::signal::block();

    // ── start: blocking and allocation are fine ──────────────────────────
    let site = Site::load("m6-file", &[], &[HANDLER], &NoTemplates);
    let read_timeout = site.read_timeout();
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
    let initial = INITIAL_CONNECTIONS.min(ceiling);
    let mut poll = Poll::new().unwrap_or_else(|e| fail("creating the poll", e));
    let mut listener = listen_unix(site.socket_path(), site.socket_mode())
        .unwrap_or_else(|e| fail(&format!("binding {}", site.socket_path().display()), e));
    let mut signals = Signals::new().unwrap_or_else(|e| fail("opening the signal descriptor", e));
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
    let mut conns = Slots::new(initial, ceiling, || ServerConn::new(READ_CAP, WRITE_CAP));
    // A config with `read_timeout_s = 0` keeps the zero duration and never
    // touches a deadline.
    let mut deadlines = Timeouts::new(initial, read_timeout.unwrap_or(Duration::ZERO));
    let mut events = Events::with_capacity(EVENTS);
    let mut trace = LoopTrace::new(&["accept", "conn", "signal", "timeout"]);
    let mut accepting = true;
    // Set when an accept failed for want of a descriptor: the free-slot count
    // then, so the listener is armed again only once a connection has closed.
    let mut starved_at: Option<usize> = None;
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
                                // Out of descriptors, or a failure: say so,
                                // give the slot back and stop accepting until
                                // a connection closes, when the listener is
                                // armed again and the queued clients accepted.
                                // Re-arming at once would spin while nothing
                                // frees.
                                m6_core::platform::net::Accept::Retry => {
                                    conns.free(taken.key);
                                    eprintln!("m6-file: accept error: {e}");
                                    poll.registry().deregister(&mut listener)?;
                                    accepting = false;
                                    starved_at = Some(conns.free_slots());
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
                // One connection's turn, then wait for what it still needs,
                // touch its deadline, or close it.
                Token(k) => {
                    let conn = conns.get_mut(k);
                    let was = conn.interest();
                    match turn(conn, &site) {
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
        while let Some(Token(k)) = deadlines.expired(m6_core::platform::now()) {
            m6_core::h1::close_conn(&poll, &mut conns, &mut deadlines, k)?;
        }
        // A slot came free while the table was full: accept again.
        if !accepting && !conns.is_full() && starved_at.is_none_or(|n| conns.free_slots() > n) {
            poll.registry()
                .register(&mut listener, LISTENER, Interest::READABLE)?;
            accepting = true;
            starved_at = None;
        }

        // The accounting tick: report and clear the loop's wait and work per
        // trigger. These services serve no metrics page, so the report is the
        // tick's own accounting rather than a served number.
        let end = m6_core::platform::now();
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
fn turn(conn: &mut ServerConn, site: &Site) -> io::Result<bool> {
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
