//! `m6-redirect`: a generic static redirect engine, one process and one event
//! loop.
//!
//! The service is a backend like any other: it listens on a Unix socket and
//! answers HTTP/1.1 over it. It registers no routes. The whole of its
//! behaviour is three answers, decided in the loop:
//!
//! - `POST /m6-redirect/rules` replaces the list: validate the Cloudflare
//!   list-items JSON body, write it verbatim to the rules file, and swap the
//!   table in one assignment. Success is 204, a malformed body 400.
//! - `GET` and `HEAD` exact-match the request's host and path against the
//!   table. A match answers the rule's status and target as `Location`, with
//!   `Cache-Control` carrying the shared freshness lifetime and
//!   stale-while-revalidate. No match answers 404.
//! - Everything else goes through `Site::handle`, so `POST /genid` runs the
//!   start path again (config and rules file) and an unmatched path is the
//!   house 404.
//!
//! The query is ignored. The list is static, the way Cloudflare Bulk Redirects
//! are: exact match, no regex, no templating.

#![deny(unsafe_code)]

use std::io;
use std::time::Duration;

use anyhow::Result;
use m6_core::drain::{Drain, Step};
use m6_core::event::{LoopTrace, Slots, Timeouts};
use m6_core::h1::{head_slots, Flush, Read, RequestHead, ServerConn};
use m6_core::net::listen_unix;
use m6_core::platform::process::raise_fd_limit;
use m6_core::platform::Signals;
use m6_core::platform::{Events, Interest, Poll, Token};
use m6_core::signal::{Action, Lifecycle};
use m6_core::{genid_failed, genid_ok, Handled, NoTemplates, Response, Site};
use m6_redirect::config::Config;
use m6_redirect::rules::Rules;

const LISTENER: Token = Token(usize::MAX);
const SIGNALS: Token = Token(usize::MAX - 1);
/// The operational log's stdout, registered while it holds bytes a full
/// destination would not take.
const LOG: Token = Token(usize::MAX - 2);
/// The accounting tick, and the longest the loop waits when nothing happens.
const TICK: Duration = Duration::from_secs(1);
const EVENTS: usize = 256;

/// The one path that replaces the list. Everything under it is this service's
/// own, and the site routes it over the internal listener only.
const RULES_PATH: &str = "/m6-redirect/rules";

/// The global allocator: mimalloc, which also counts calls when the
/// `count-allocations` feature turns on m6-core's `alloc-count`. The counting
/// is not compiled in a plain build.
#[global_allocator]
static ALLOC: m6_core::alloc::M6Alloc = m6_core::alloc::M6Alloc;

/// The state one loop owns: the parsed `[redirect]` settings and the compiled
/// table. One owner, no lock and no shared handle. A reload replaces both in
/// one assignment.
struct State {
    config: Config,
    rules: Rules,
}

impl State {
    /// Read the config and the rules file, the way the start does.
    fn load(site: &Site) -> Result<State, String> {
        let config = Config::read(site.user_config().get("redirect"), site.site_dir())?;
        let rules = Rules::load(&config.rules_file)?;
        Ok(State { config, rules })
    }

    /// The start path again, for `POST /genid`: read the config and the rules
    /// file, and only then replace both, so a failure changes nothing.
    fn reload(&mut self, site: &Site) -> Result<(), String> {
        let config = Config::read(site.user_config().get("redirect"), site.site_dir())?;
        let rules = Rules::load(&config.rules_file)?;
        self.config = config;
        self.rules = rules;
        Ok(())
    }

    /// `POST /m6-redirect/rules`: validate, write verbatim, swap.
    fn replace_list(&mut self, body: &[u8]) -> Response {
        let rules = match Rules::parse(body) {
            Ok(rules) => rules,
            Err(e) => return Response::refusal(&format!("malformed redirect list: {e}")),
        };
        if let Err(e) = Rules::write_atomic(&self.config.rules_file, body) {
            return Response::status(500)
                .header("Cache-Control", "no-store")
                .body(format!("writing the redirect list failed: {e}\n"));
        }
        self.rules = rules;
        Response::status(204).header("Cache-Control", "no-store")
    }

    /// A redirect answer for `GET` and `HEAD`, or the 404 for an unknown code.
    fn redirect_or_not_found(&self, head: &RequestHead<'_>) -> Response {
        let host = header(head, "host").unwrap_or("");
        match self.rules.lookup(host, head.path) {
            Some(redirect) => Response::status(redirect.status)
                .header("Location", &redirect.target)
                .header("Cache-Control", &self.config.cache_control()),
            None => Response::not_found(),
        }
    }
}

/// Look up one header value on the borrowed head, case-insensitively. A value
/// that is not UTF-8 reads as absent, which then cannot match any rule.
fn header<'a>(head: &'a RequestHead<'_>, name: &str) -> Option<&'a str> {
    head.headers
        .iter()
        .find(|h| h.name.eq_ignore_ascii_case(name))
        .and_then(|h| std::str::from_utf8(h.value).ok())
}

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
    // The redirect engine reads its own `[redirect]` table and rules file, so
    // the dump is deferred until those reads have run: a config the start
    // refuses must fail the dump the same way, not print `ok`.
    let mut site = Site::load_deferring_dump("m6-redirect", &[], &[], &NoTemplates);
    let mut state = State::load(&site).unwrap_or_else(|e| fail("loading the redirect state", e));
    if site.dump_requested() {
        std::process::exit(site.dump(&[], &[]));
    }

    let read_timeout = site.read_timeout();
    let descriptors = raise_fd_limit().unwrap_or_else(|e| fail("raising the open file limit", e));
    // A connection table larger than the descriptors the limit allows could
    // never report itself full: `Slots::is_full` would stay false past the
    // kernel's limit. The connection table follows the `connections` limit,
    // and the open-file limit can lower its maximum.
    let table = site.limits().get(&m6_core::limits::CONNECTIONS);
    let ceiling = table.max().min(descriptors);
    if ceiling == 0 {
        fail("the open file limit", "leaves no connection table");
    }
    let initial = table.start().min(ceiling);
    let mut poll = Poll::new().unwrap_or_else(|e| fail("creating the poll", e));
    // The signal descriptor opens before the socket exists. Its existence is how
    // a caller knows the service is ready, so a caller that waits for the socket
    // and then sends SIGTERM must find the handler armed.
    let mut signals = Signals::new().unwrap_or_else(|e| fail("opening the signal descriptor", e));
    let mut listener = listen_unix(site.socket_path(), site.socket_mode())
        .unwrap_or_else(|e| fail(&format!("binding {}", site.socket_path().display()), e));
    // The reserve the accept loop spends to refuse a connection when the
    // process is out of descriptors.
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
    let drain_timeout = site.drain_timeout();
    let mut drain = Drain::new(drain_timeout, initial);
    let mut deadlines = Timeouts::new(initial, read_timeout.unwrap_or(Duration::ZERO));
    let mut events = Events::with_capacity(EVENTS);
    let mut trace = LoopTrace::new(&["accept", "conn", "signal", "timeout", "log"]);
    let mut accepting = true;
    let mut last = m6_core::platform::now();
    let mut last_report = last;

    // ── run: nothing blocks ──────────────────────────────────────────────
    'run: loop {
        let mut wait = deadlines.timeout(m6_core::platform::now(), TICK);
        if let Some(left) = drain.poll_timeout(m6_core::platform::now()) {
            wait = wait.min(left);
        }
        match poll.poll(&mut events, Some(wait)) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
        }
        let now = m6_core::platform::now();
        trace.waited(now - last);
        let trigger = match events.iter().next().map(|ev| ev.token()) {
            Some(LISTENER) => 0,
            Some(SIGNALS) => 2,
            Some(LOG) => 4,
            Some(_) => 1,
            None => 3,
        };

        for ev in &events {
            match ev.token() {
                LISTENER => {
                    let mut served = false;
                    while let Some(taken) = conns.take() {
                        if let Some(new_len) = taken.grew_to {
                            deadlines.grow(new_len);
                            drain.grown(new_len);
                        }
                        let mut stream = match listener.accept() {
                            Ok((stream, _)) => stream,
                            Err(e) => match m6_core::platform::net::accept_error(&e) {
                                m6_core::platform::net::Accept::Drained => {
                                    conns.free(taken.key);
                                    break;
                                }
                                m6_core::platform::net::Accept::SkipOne => {
                                    conns.free(taken.key);
                                    continue;
                                }
                                m6_core::platform::net::Accept::Exhausted => {
                                    conns.free(taken.key);
                                    let drained = spare.drain(|| {
                                        let (stream, _) = listener.accept()?;
                                        drop(stream);
                                        Ok(())
                                    });
                                    for _ in 0..drained.refused {
                                        refusals.refused();
                                    }
                                    if let Some(e) = &drained.stopped {
                                        m6_core::log::warn!(error = %e, "accept failed while draining the queued connections");
                                    }
                                    if let Some(e) = &drained.lost {
                                        m6_core::log::warn!(error = %e, "the reserve descriptor could not be reopened");
                                    }
                                    if drained.needs_rearm() {
                                        poll.registry().reregister(
                                            &mut listener,
                                            LISTENER,
                                            Interest::READABLE,
                                        )?;
                                    }
                                    break;
                                }
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
                        drain.begin(taken.key);
                        served = true;
                    }
                    let counts = if served {
                        refusals.flush(now)
                    } else {
                        refusals.due(now)
                    };
                    if let Some(counts) = counts {
                        m6_core::net::warn_refused("m6-redirect", counts);
                    }
                    if conns.is_full() {
                        poll.registry().deregister(&mut listener)?;
                        accepting = false;
                    }
                }
                SIGNALS => {
                    while let Some(signal) = signals.read() {
                        if lifecycle.on_signal(signal) == Action::Stop {
                            stop_accepting(poll.registry(), &mut listener, &mut accepting)?;
                            drain.start(now);
                        }
                    }
                }
                LOG => m6_core::log::flush(),
                Token(k) => {
                    drain.begin(k);
                    let conn = conns.get_mut(k);
                    let was = conn.interest();
                    match turn(conn, &mut site, &mut state) {
                        Ok(true) => {
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
                            if conn.served() > 0 && !conn.pending() {
                                drain.end(k);
                            }
                        }
                        _ => close(&poll, &mut conns, &mut deadlines, &mut drain, k)?,
                    }
                }
            }
        }

        if let Err(e) = m6_core::log::watch(poll.registry(), LOG) {
            m6_core::log::error!(error = %e, "log descriptor registration failed");
        }

        while let Some(Token(k)) = deadlines.expired(m6_core::platform::now()) {
            close(&poll, &mut conns, &mut deadlines, &mut drain, k)?;
        }

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

        if !accepting && !drain.draining() && !conns.is_full() {
            poll.registry()
                .register(&mut listener, LISTENER, Interest::READABLE)?;
            accepting = true;
        }

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
    m6_core::net::close_refusals("m6-redirect", &mut refusals);
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
fn turn(conn: &mut ServerConn, site: &mut Site, state: &mut State) -> io::Result<bool> {
    loop {
        if conn.interest() == Interest::WRITABLE {
            match conn.flush()? {
                Flush::Done => {}
                Flush::More => return Ok(true),
                Flush::Close => return Ok(false),
            }
        }
        let mut heads = head_slots();
        let read = conn.read(&mut heads)?;
        let (response, idle) = match read {
            Read::Request(head) => (Some(answer(&head, site, state)), false),
            Read::More => (None, true),
            Read::Closed => return Ok(false),
        };
        if let Some(response) = response {
            conn.respond_response(response);
        }
        match conn.flush()? {
            Flush::Done if !idle => {}
            Flush::Done | Flush::More => return Ok(true),
            Flush::Close => return Ok(false),
        }
    }
}

/// One request's answer: the list-replace, the redirect lookup, or the site's
/// dispatch (the reload and the house 404).
fn answer(head: &RequestHead<'_>, site: &mut Site, state: &mut State) -> Response {
    if head.method == "POST" && head.path == RULES_PATH {
        return state.replace_list(head.body());
    }
    if head.method == "GET" || head.method == "HEAD" {
        return state.redirect_or_not_found(head);
    }
    match site.handle(head) {
        Handled::Done(response) => response,
        Handled::Reload(reload) => {
            let response = match site.reload(&[], &[], &NoTemplates) {
                Ok(next) => match state.reload(&next) {
                    Ok(()) => {
                        *site = next;
                        genid_ok()
                    }
                    Err(e) => genid_failed(&e),
                },
                Err(e) => genid_failed(&e),
            };
            m6_core::site::finish_reload(&reload, response)
        }
        Handled::Code(..) | Handled::Named(..) => {
            unreachable!("m6-redirect registers no routes")
        }
    }
}

/// A failure at start is an error: print it and exit 2.
fn fail(what: &str, error: impl std::fmt::Display) -> ! {
    eprintln!("m6-redirect: {what}: {error}");
    std::process::exit(2)
}
