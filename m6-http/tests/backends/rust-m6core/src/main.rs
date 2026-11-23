//! An m6 backend in Rust **on `m6-core`**, written from
//! `docs/m6-backend-protocol.md`.
//!
//! Usage: `m6-example-rust-m6core <site-dir> <config-path>`
//!
//! Note the invocation: it is not the `<socket-path> <status-json-path>` of the
//! other five. A core service is configured rather than argument-driven, which
//! is `parse_invocation`'s contract, and the socket path comes from the config
//! file or `M6_SOCKET_OVERRIDE`. That difference is itself part of what this
//! example shows.
//!
//! # Read this beside `rust-plain/src/main.rs`
//!
//! That file and this one answer the same five routes with the same bytes. The
//! difference between them is the whole point of the pair, and it is the
//! measurement the phase exists for: same language, same compiler, same payload,
//! so the delta between the two binaries is precisely what linking `m6-core`
//! costs. Per `docs/m6-backend-examples.md` §5.3 that is the genuinely
//! informative comparison in the set, and if it is not close to zero then core
//! has a problem worth knowing about.
//!
//! What the pair file writes by hand and this one does not write at all:
//!
//! - the binding sequence of spec §1.2, including the socket `chmod`, which is
//!   the single most common cause of a backend that starts and is never contacted
//! - `signal(2)` handling, the shutdown flag, and the unlink on the way out
//! - parsing a request line and header section by hand, and draining exactly
//!   `Content-Length` bytes
//! - writing a status line, `Content-Type` and an accurate `Content-Length`
//! - suppressing the body on HEAD, and on 1xx, 204 and 304
//!
//! Both files write their own event loop. This one's is a little under 200 lines
//! of `main`: start, the poll loop and the per-connection turn, with
//! `m6_core::mio` for the reactor and `m6_core::event` for signals, deadlines
//! and the connection table. That makes the pair's delta core's request path
//! and start checks, which is the number the pair exists to show, rather than
//! the cost of a loop core supplied.
//!
//! What is left is the part that is actually this service: five routes and their
//! bodies. That is the shape the architectural rule asks for, and the reason
//! the rule is phrased as "a service being nearly a no-op on top of core is the
//! goal, not a smell".

#![deny(unsafe_code)]

use std::io;
use std::time::Duration;

use m6_core::event::{LoopTrace, Slots, Timeouts};
use m6_core::h1::{head_slots, Flush, Read, ServerConn};
use m6_core::net::listen_unix;
use m6_core::platform::process::raise_fd_limit;
use m6_core::platform::Signals;
use m6_core::platform::{Events, Interest, Poll, Token};
use m6_core::prelude::*;
use m6_core::signal::{Action, Lifecycle};
use m6_core::site::RouteMethod;
use m6_core::{genid_failed, genid_ok, CodeRoute};

const LANGUAGE: &str = "Rust (m6-core)";
const NAME: &str = "m6-example-rust-m6core";

const LISTENER: Token = Token(usize::MAX);
const SIGNALS: Token = Token(usize::MAX - 1);
/// The operational log's stdout, registered while it holds bytes a full
/// destination would not take.
const LOG: Token = Token(usize::MAX - 2);
/// The accounting tick, and the longest the loop waits when nothing happens.
const TICK: Duration = Duration::from_secs(1);
const EVENTS: usize = 256;
/// Slots and buffers made at start, before any connection exists.
const INITIAL_CONNECTIONS: usize = 1024;
const READ_CAP: usize = 16 * 1024;
const WRITE_CAP: usize = 64 * 1024;

/// The five routes, in the order the service answers them. A code route's
/// index in this slice is what `Handled::Code` carries.
const ROUTES: [CodeRoute; 5] = [
    CodeRoute {
        pattern: "/",
        method: RouteMethod::Get,
    },
    CodeRoute {
        pattern: "/status",
        method: RouteMethod::Get,
    },
    CodeRoute {
        pattern: "/health",
        method: RouteMethod::Get,
    },
    CodeRoute {
        pattern: "/boom",
        method: RouteMethod::Get,
    },
    // A catch-all wildcard gives the same 404 body as the other five, and it
    // is registered last so the four specific routes win. `{*any}` captures a
    // segment and every one after it.
    CodeRoute {
        pattern: "/{*any}",
        method: RouteMethod::Get,
    },
];

fn main() -> std::io::Result<()> {
    // Block the signals before anything else: the mask is inherited only by
    // threads created after this point, and logging starts one.
    m6_core::signal::block();

    // The payload comes from the one copy in the repository, the same file the
    // other five examples read, so byte-identity across all six is structural
    // rather than asserted. The site directory is where a core service's own
    // files live, and `parse_invocation` has already established it as argv[1].
    let site_dir = std::env::args().nth(1).unwrap_or_default();
    let status_path = std::path::Path::new(&site_dir).join("status.json");
    let status_body = match std::fs::read(&status_path) {
        Ok(b) => b,
        Err(e) => {
            // Exit 2 means "failed before binding", so a supervisor can tell a
            // misconfiguration from a crash (spec §8.3). Core exits 2 for a bad
            // invocation; this is the same contract for a missing payload.
            eprintln!("cannot read status payload {}: {e}", status_path.display());
            std::process::exit(2);
        }
    };

    // ── start: blocking and allocation are fine ──────────────────────────
    let mut site = Site::load(NAME, &ROUTES, &[], &NoTemplates);
    let read_timeout = site.read_timeout();
    let ceiling = raise_fd_limit().unwrap_or_else(|e| fail("raising the open file limit", e));
    if ceiling == 0 {
        fail("the open file limit", "leaves no connection table");
    }
    // A table larger than the descriptors the limit allows could never report
    // itself full: `Slots::is_full` would stay false past the kernel's limit.
    let initial = INITIAL_CONNECTIONS.min(ceiling);
    let mut poll = Poll::new().unwrap_or_else(|e| fail("creating the poll", e));
    // The signal descriptor opens BEFORE the socket exists, so a caller that
    // waits for the socket and then sends SIGTERM finds the handler ready. The
    // `block()` above already keeps SIGTERM from killing the process, and
    // opening the descriptor first makes the readiness rule hold in the order
    // it is written rather than as a consequence of the mask.
    let mut signals = Signals::new().unwrap_or_else(|e| fail("opening the signal descriptor", e));
    let mut listener = listen_unix(site.socket_path(), site.socket_mode())
        .unwrap_or_else(|e| fail(&format!("binding {}", site.socket_path().display()), e));
    // The reserve the accept loop spends to refuse a connection when the
    // process is out of descriptors. Opened here, before any pressure, and on
    // the listener's own socket family.
    let mut spare = m6_core::platform::net::SpareDescriptor::for_unix()?;
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
    let mut conns = Slots::new(initial, ceiling, || ServerConn::new(READ_CAP, WRITE_CAP));
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
            Err(e) => return Err(e),
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
                                            // shortage cannot be handled. Arm
                                            // the listener and stop draining.
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
                        served = true;
                    }
                    let counts = if served {
                        refusals.flush(now)
                    } else {
                        refusals.due(now)
                    };
                    if let Some(counts) = counts {
                        m6_core::net::warn_refused(NAME, counts);
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
                    match turn(conn, &mut site, &status_body) {
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
        //
        // These logger calls are live: `Site::load` above initialises the
        // operational Sink, and `lifecycle.started()` has written to it.
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
fn turn(conn: &mut ServerConn, site: &mut Site, status_body: &[u8]) -> io::Result<bool> {
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
                        let response = match site.reload(&ROUTES, &[], &NoTemplates) {
                            Ok(next) => {
                                *site = next;
                                genid_ok()
                            }
                            Err(e) => genid_failed(&e),
                        };
                        m6_core::site::finish_reload(&reload, response)
                    }
                    // This service's five routes, by the index they were given
                    // to `Site::load` in.
                    Handled::Code(index, req) => {
                        let response = route(index, status_body);
                        site.finish(&req, response)
                    }
                    Handled::Named(..) => unreachable!(
                        "the backend registers no named handlers, so every route is in code"
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

/// The body of one route, by its index in [`ROUTES`].
fn route(index: usize, status_body: &[u8]) -> Response {
    match index {
        0 => Response::html(html_page(
            &format!("m6 backend example: {LANGUAGE}"),
            &format!("A minimal m6 backend written in {LANGUAGE}."),
        )),
        1 => {
            // `.verbatim()` rather than letting the pipeline have it, and not
            // `Response::json`, for two separate reasons:
            //
            // - `Response::json` would re-serialise a parsed value, and the
            //   whole point of this route is that all six examples emit the
            //   same bytes. Serde would be free to order or space them
            //   differently.
            // - verbatim stops core compressing the body, and minifying it in a
            //   build that has the `minify` feature. A workspace build does,
            //   through m6-html, even though this crate does not ask for it. Spec
            //   §3.6 says a backend SHOULD NOT compress, because the proxy
            //   negotiates and caches each representation itself. It also keeps
            //   the measurement honest: compressing here and not in the pair
            //   would make the delta a story about brotli rather than about
            //   core.
            Response::status(200)
                .body(status_body.to_vec())
                .header("content-type", "application/json; charset=utf-8")
                .verbatim()
        }
        2 => Response::text("ok"),
        3 => {
            // Fails on purpose. The proxy counts 5xx as a backend error and may
            // replace it with a styled error page (spec §4).
            // `.verbatim()` here too. Without it core would compress the body,
            // and minify the HTML in a build with the `minify` feature, which
            // would make this example's body differ from the other five's 178
            // bytes (167 when minified). The examples are meant to be
            // readable side by side, so the one that is allowed to differ is
            // /status, and that one differs in nothing at all.
            Response::status(500)
                .body(html_page(
                    "500 Internal Server Error",
                    "This endpoint fails on purpose.",
                ))
                .header("content-type", "text/html; charset=utf-8")
                .verbatim()
        }
        4 => Response::status(404)
            .body(html_page(
                "404 Not Found",
                &format!("This {LANGUAGE} backend does not serve that path."),
            ))
            .header("content-type", "text/html; charset=utf-8")
            .verbatim(),
        _ => unreachable!("the five code routes are indexes 0 to 4"),
    }
}

fn html_page(title: &str, detail: &str) -> String {
    format!(
        "<!doctype html>\n<html lang=\"en\"><meta charset=\"utf-8\">\n<title>{title}</title>\n<h1>{title}</h1>\n<p>{detail}</p>\n</html>\n"
    )
}

/// A failure at start is an error: print it and exit 2.
fn fail(what: &str, error: impl std::fmt::Display) -> ! {
    eprintln!("{NAME}: {what}: {error}");
    std::process::exit(2)
}
