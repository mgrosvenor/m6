//! `m6-monitor`: one page that says how the fleet is.
//!
//! Runs at origin, polls every node's `/health` and `/perf` over the backbone,
//! and serves the digest. These routes:
//!
//!   GET /          the page
//!   GET /digest    the same thing as JSON, for anything that is not a person
//!   GET /check     the hourly health check, as text
//!   GET /history   the poll history as paged JSON Lines, when `history_file` is set
//!   GET /crawlers  the crawler sightings as paged JSON Lines, likewise
//!
//! Every route answers on the unix socket only. The monitor has no listener and
//! no token code. Access control is the `require` rule of the m6-http placed in
//! front of the socket. See `history.rs` for the history and a front example.
//!
//! # One process and one event loop
//!
//! The monitor's `main` is start, run and stop, the shape every service has
//! since EL-6b: `m6_core::event` supplies the signals, the deadlines and the
//! connection table, and `m6_core::platform` the reactor. The fleet poll is not a thread.
//! It is a table of steps in this loop (`poll.rs`): each node's `/health`,
//! `/perf` and `/traffic` requests, each tunnel connect and each crawler
//! lookup is a descriptor the loop waits on, so a node that hangs holds only
//! its own step and never the monitor.
//!
//! Nothing blocks while serving. The fleet, every node's host name, the
//! history file and the host's certificate store are read at start, and a
//! change to any of them needs a restart.
//!
//! It is an ordinary m6 service. It links `m6-core` and nothing else, gets its
//! config, routing and logging from there, and is configured the way every
//! other m6 service is. The interesting part is `digest.rs`, which is the only
//! module here allowed to hold an opinion about what a number means.
//!
//! # Why this is not a script over ssh
//!
//! `/health` and `/perf` are HTTP endpoints and the analytics stream is a file
//! m6 writes, so the data was always reachable without a shell. What was
//! missing was the host's own numbers, which are now on `/perf`. Polling over
//! the backbone rather than shelling in means the monitor needs no credentials
//! on the boxes, measures the path it actually cares about, and fails the way a
//! service fails rather than the way a script does.

#![deny(unsafe_code)]

mod check;
mod digest;
mod fleet;
mod history;
mod poll;
mod tunnel;

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use m6_core::event::{LoopTrace, Slots, Timeouts};
use m6_core::h1::{close_conn, head_slots, Flush, Read, RequestHead, ServerConn};
use m6_core::net::listen_unix;
use m6_core::platform::process::raise_fd_limit;
use m6_core::platform::Signals;
use m6_core::platform::{Events, Interest, Poll, Token};
use m6_core::prelude::*;
use m6_core::signal::{Action, Lifecycle};
use m6_core::site::RouteMethod;

#[cfg(feature = "count-allocations")]
#[global_allocator]
static ALLOC: m6_core::testkit::alloc_count::Counting = m6_core::testkit::alloc_count::Counting;

const LISTENER: Token = Token(usize::MAX);
const SIGNALS: Token = Token(usize::MAX - 1);
/// The accounting tick, and the longest the loop waits when nothing happens.
const TICK: Duration = Duration::from_secs(1);
const EVENTS: usize = 256;
/// Slots and buffers made at start, before any connection exists.
const INITIAL_CONNECTIONS: usize = 1024;
const READ_CAP: usize = 16 * 1024;
const WRITE_CAP: usize = 64 * 1024;

/// The five routes, in the order the service answers them. A code route's
/// index in this slice is what `Handled::Code` carries. The first three are
/// answered from the loop's own stored bytes; the last two read the history.
const ROUTES: [CodeRoute; 5] = [
    CodeRoute {
        pattern: "/",
        method: RouteMethod::Get,
    },
    CodeRoute {
        pattern: "/digest",
        method: RouteMethod::Get,
    },
    CodeRoute {
        pattern: "/check",
        method: RouteMethod::Get,
    },
    CodeRoute {
        pattern: "/history",
        method: RouteMethod::Get,
    },
    CodeRoute {
        pattern: "/crawlers",
        method: RouteMethod::Get,
    },
];

/// How often the poll refreshes the fleet view.
///
/// Every request used to poll the whole fleet itself, which meant load on the
/// nodes scaled with how often anyone LOOKED at the monitor, two requests
/// moments apart could disagree because they came from different polls, and a
/// plain `/digest` fetch took 2.9 to 3.6 seconds. A monitoring endpoint should
/// answer instantly from a recent reading.
const DEFAULT_POLL_SECS: u64 = 30;

fn main() -> Result<()> {
    // `m6-monitor --check <config>` prints the hourly health check and exits.
    // Same poll code as the page, so the two cannot disagree.
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--check") {
        let config = args
            .iter()
            .skip(1)
            .find(|a| !a.starts_with("--"))
            .ok_or_else(|| anyhow::anyhow!("usage: m6-monitor --check <config-path>"))?;
        return run_check(Path::new(config));
    }
    run_service()
}

/// A completed fleet poll and the three answers built from it.
///
/// The loop owns it: the poll writes it by plain assignment when it finishes
/// and every request reads it through a shared borrow. One owner, no lock and
/// no shared handle.
struct Snapshot {
    digest: digest::Digest,
    readings: Vec<poll::NodeReading>,
    verdicts: check::Verdicts,
    /// The page, rendered once per poll. Its figures never change between
    /// polls, so a request copies the bytes.
    page: Vec<u8>,
    page_status: u16,
    at: Instant,
    at_utc: String,
}

impl Snapshot {
    /// Age of the snapshot, and whether it is stale enough to distrust.
    ///
    /// Stale at three missed intervals. A monitor serving an old reading
    /// without saying so is worse than one that is plainly down, because it
    /// looks like current information.
    fn age(&self, interval: Duration) -> (f64, bool) {
        let age = self.at.elapsed().as_secs_f64();
        (age, age > interval.as_secs_f64() * 3.0)
    }
}

// ── start, run and stop ─────────────────────────────────────────────────────

fn run_service() -> Result<()> {
    // Block the signals before anything else: the mask is inherited only by
    // threads created after this point, and logging starts one.
    m6_core::signal::block();

    // ── start: blocking and allocation are fine ──────────────────────────
    let site = Site::load("m6-monitor", &ROUTES, &[], &NoTemplates);
    let read_timeout = site.read_timeout();
    let interval = Duration::from_secs(fleet::poll_interval_s(|k| {
        site.user_config().get(k).and_then(|v| v.as_u64())
    }));
    let fleet = fleet::Fleet::from_config(site.config_path())
        .unwrap_or_else(|e| fail("reading the fleet", format!("{e:#}")));
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let tls = Arc::new(
        rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap_or_else(|e| fail("building the TLS client config", e))
            .with_root_certificates(
                m6_core::client::system_roots()
                    .unwrap_or_else(|e| fail("reading the host certificate store", e)),
            )
            .with_no_client_auth(),
    );
    let nodes = fleet
        .resolve(&tls)
        .unwrap_or_else(|e| fail("resolving the fleet", format!("{e:#}")));
    let tokens = {
        let fallback = fleet.perf_token();
        fleet
            .nodes
            .iter()
            .map(|n| n.perf_token(fallback.as_deref()))
            .collect::<Vec<_>>()
    };
    let nameserver = m6_core::dns::nameserver()
        .unwrap_or_else(|e| fail("reading the resolver configuration", e));
    let history_config = history::config_from(site.user_config().get("monitor"), site.site_dir())
        .unwrap_or_else(|e| fail("reading the history config", format!("{e:#}")));
    let mut history = history::start(history_config)
        .unwrap_or_else(|e| fail("opening the history file", format!("{e:#}")));
    let thresholds = digest::Thresholds {
        tunnel_handshake_age_s: fleet.tunnel_handshake_max_age_s,
        log_reopen_age_s: fleet.log_reopen_max_age_s,
        ..Default::default()
    };
    let mut poller = poll::Poller::new(poll::Setup {
        nodes,
        timeout: Duration::from_millis(fleet.timeout_ms),
        header_checks: fleet.header_check.clone(),
        tokens,
        nameserver,
        wg_file: fleet.wg_handshakes_file.clone(),
        wg_max_age_s: fleet.tunnel_handshake_max_age_s,
        lookup_cap: check::MAX_LOOKUPS_PER_POLL,
    });

    let ceiling = raise_fd_limit().unwrap_or_else(|e| fail("raising the open file limit", e));
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
        .register_fd(signals.raw_fd(), SIGNALS, Interest::READABLE)
        .unwrap_or_else(|e| fail("registering the signal descriptor", e));
    let mut lifecycle = Lifecycle::new(site.name(), Some(site.socket_path().to_path_buf()));
    lifecycle.started();

    // The whole connection table, every buffer in it, the deadlines, the
    // outgoing slots and the events: made here, never grown except by `Slots`.
    let mut conns = Slots::new(initial, ceiling, || ServerConn::new(READ_CAP, WRITE_CAP));
    // A config with `read_timeout_s = 0` keeps the zero duration and never
    // touches a deadline.
    let mut deadlines = Timeouts::new(initial, read_timeout.unwrap_or(Duration::ZERO));
    let mut events = Events::with_capacity(EVENTS);
    let mut trace = LoopTrace::new(&["accept", "client", "node", "probe", "dns", "tick"]);
    let mut out = poller.out();
    let mut accepting = true;
    let mut snapshot: Option<Snapshot> = None;
    let mut poll_reported_late = false;
    let mut last = m6_core::platform::now();
    let mut last_report = last;
    #[cfg(feature = "count-allocations")]
    let mut client_allocs = 0u64;

    // The first poll starts with the service, so the page and the check have a
    // reading before the first interval passes.
    let first = poller
        .start(&poll, &mut out, m6_core::platform::now())
        .unwrap_or_else(|e| fail("starting the first poll", e));
    publish(first, &mut snapshot, &thresholds, &mut history);

    // ── run: nothing blocks ──────────────────────────────────────────────
    'run: loop {
        // The wait is the earlier of the two deadline tables and the tick. The
        // poll starts on a tick, not on its own timer: one interval is 30
        // ticks, and a second of lateness on the first request is the price of
        // not keeping a second clock.
        let now = m6_core::platform::now();
        let wait = deadlines.timeout(now, TICK).min(poller.timeout(now, TICK));
        match poll.poll(&mut events, Some(wait)) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
        }
        let now = m6_core::platform::now();
        trace.waited(now - last);
        // One sample per iteration, under the kind of its first event, as the
        // other services report the trace.
        let nodes = nodes_in(&out);
        let trigger = match events.iter().next().map(|ev| ev.token()) {
            Some(LISTENER) => 0,
            Some(SIGNALS) => 5,
            Some(Token(t)) if t < poll::CLIENT_BASE => 1,
            Some(Token(t)) => {
                let slot = t - poll::CLIENT_BASE;
                if slot < nodes {
                    2
                } else if slot < nodes + tunnel::PROBES_PER_POLL {
                    3
                } else {
                    4
                }
            }
            None => 5,
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
                                    eprintln!("m6-monitor: accept error: {e}");
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
                // One connection's turn: answer from the snapshot, or from
                // the history for the two paged routes.
                Token(k) if k < poll::CLIENT_BASE => {
                    #[cfg(feature = "count-allocations")]
                    let before = m6_core::testkit::alloc_count::now();
                    let conn = conns.get_mut(k);
                    let was = conn.interest();
                    let turn = turn(conn, &site, snapshot.as_ref(), &history, interval);
                    match turn {
                        Ok(true) => {
                            // Re-register only when the turn changed what the
                            // connection waits for.
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
                        _ => close_conn(&poll, &mut conns, &mut deadlines, k)?,
                    }
                    #[cfg(feature = "count-allocations")]
                    {
                        client_allocs += (m6_core::testkit::alloc_count::now() - before) as u64;
                    }
                }
                // One outgoing slot: a node's request, a tunnel connect or a
                // crawler lookup.
                Token(t) => {
                    let slot = t - poll::CLIENT_BASE;
                    let progress = poller.drive(&poll, &mut out, slot, m6_core::platform::now())?;
                    publish(progress, &mut snapshot, &thresholds, &mut history);
                }
            }
        }

        // Outgoing work whose deadline passed: a timed-out node step, a lost
        // probe or a failed lookup, each recorded and moved on.
        while let Some(progress) = poller.expire(&poll, &mut out, m6_core::platform::now())? {
            publish(progress, &mut snapshot, &thresholds, &mut history);
        }
        // Close every client whose read deadline passed, with no response.
        while let Some(Token(k)) = deadlines.expired(m6_core::platform::now()) {
            close_conn(&poll, &mut conns, &mut deadlines, k)?;
        }
        // A slot came free while the table was full: accept again.
        if !accepting && !conns.is_full() {
            poll.registry()
                .register(&mut listener, LISTENER, Interest::READABLE)?;
            accepting = true;
        }

        // The tick: start a poll when the interval has passed since the last
        // one started and none is running. A poll still running at its next
        // due time is not restarted; the tick says so once and waits.
        let now = m6_core::platform::now();
        let due = poller
            .last_start()
            .is_none_or(|start| now.duration_since(start) >= interval);
        if due && poller.is_running() {
            if !poll_reported_late {
                tracing::debug!("a poll is still running at its next due time; waiting");
                poll_reported_late = true;
            }
        } else if due {
            poll_reported_late = false;
            let progress = poller.start(&poll, &mut out, now)?;
            publish(progress, &mut snapshot, &thresholds, &mut history);
        }

        // The accounting tick: report and clear the loop's wait and work per
        // trigger. The monitor serves no metrics page, so the report is the
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
    // Outgoing work in flight is dropped. A batch held for a busy database is
    // tried once more; a failure is logged, never a wait.
    history.flush();
    #[cfg(feature = "count-allocations")]
    eprintln!("{client_allocs} allocation(s) in the client arm outside table growth");
    // Connections still open are dropped: a SIGTERM does not wait for a
    // request in flight. `complete` removes the socket and logs the line.
    lifecycle.complete();
    Ok(())
}

/// One connection's turn: write what is owed, read, answer and write until the
/// socket would block. Returns whether the connection stays open.
fn turn(
    conn: &mut ServerConn,
    site: &Site,
    snapshot: Option<&Snapshot>,
    history: &history::History,
    interval: Duration,
) -> std::io::Result<bool> {
    loop {
        // Write what is owed before reading anything. A streamed body is not
        // in the output buffer, so a read here would let the next request's
        // answer replace it mid-body.
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
        let (reply, idle) = match read {
            Read::Request(head) => (
                Some(answer(&head, site, snapshot, history, interval)),
                false,
            ),
            Read::More => (None, true),
            Read::Closed => return Ok(false),
        };
        if let Some(reply) = reply {
            match reply {
                Answer::Bytes {
                    status,
                    headers,
                    body,
                } => conn.respond(status, headers, body),
                Answer::Response(response) => conn.respond_response(response),
            }
        }
        match conn.flush()? {
            // The answer is out and the connection keeps alive. A pipelined
            // request may already be buffered, so read again before waiting.
            Flush::Done if !idle => {}
            Flush::Done | Flush::More => return Ok(true),
            Flush::Close => return Ok(false),
        }
    }
}

/// What one request is answered with: stored bytes that copy into the
/// connection's buffer, or a response the request built.
enum Answer<'a> {
    Bytes {
        status: u16,
        headers: &'static [(&'static str, &'static str)],
        body: &'a [u8],
    },
    Response(Response),
}

/// The answer to one parsed request.
///
/// `/`, `/digest` and `/check` are answered here from the loop's snapshot.
/// `/history` and `/crawlers` go through the site's dispatch, because they
/// parse a query and read the store, which allocate anyway. Every other path
/// is the site's 404.
fn answer<'a>(
    head: &RequestHead<'_>,
    site: &Site,
    snapshot: Option<&'a Snapshot>,
    history: &history::History,
    interval: Duration,
) -> Answer<'a> {
    // The stored answers are all GETs. Any other method falls through to the
    // site's dispatch, which answers the 404 a route without that method earns.
    if head.method == "GET" {
        match head.path {
            "/" => return page_answer(snapshot),
            "/digest" => return Answer::Response(digest_json(snapshot, interval)),
            "/check" => return Answer::Response(check_text(snapshot, interval)),
            _ => {}
        }
    }
    match site.handle(head) {
        Handled::Done(response) => Answer::Response(response),
        // `find_route` trims outer slashes, so `/digest/`, `//` and a
        // relative `digest` arrive here as the code routes the byte-equal arm
        // above answers. They are answered from the snapshot the same way,
        // because a slash is not a reason to abort the loop.
        Handled::Code(index, req) => match index {
            0 => page_answer(snapshot),
            1 => Answer::Response(digest_json(snapshot, interval)),
            2 => Answer::Response(check_text(snapshot, interval)),
            3 => Answer::Response(routed(history_ndjson(&req, history), &req, site)),
            4 => Answer::Response(routed(crawlers_ndjson(&req, history), &req, site)),
            // Five routes give indices nought to four. A sixth cannot exist;
            // a 404 is a cheaper answer than ending the process if one ever did.
            _ => Answer::Response(site.finish(&req, Response::not_found())),
        },
        Handled::Named(..) => unreachable!("m6-monitor registers no named handler"),
    }
}

/// Finish a paged route's answer, with an error turned into the site's own
/// error response.
fn routed(response: m6_core::Result<Response>, req: &Request, site: &Site) -> Response {
    match response {
        Ok(response) => site.finish(req, response),
        Err(e) => site.finish(req, m6_core::response::error_to_response(&e)),
    }
}

const HTML: &[(&str, &str)] = &[
    ("Content-Type", "text/html; charset=utf-8"),
    ("Cache-Control", "no-store"),
    ("X-Robots-Tag", "noindex, nofollow"),
];
const JSON: &[(&str, &str)] = &[
    ("Content-Type", "application/json"),
    ("Cache-Control", "no-store"),
];

/// The page, or "no poll yet" as JSON, from bytes the poll stored.
fn page_answer(snapshot: Option<&Snapshot>) -> Answer<'_> {
    match snapshot {
        Some(snap) => Answer::Bytes {
            status: snap.page_status,
            headers: HTML,
            body: &snap.page,
        },
        None => Answer::Bytes {
            status: 503,
            headers: JSON,
            body: b"{\"error\":\"no fleet poll has completed yet\"}",
        },
    }
}

/// A plain-text answer that no cache may keep.
fn plain(status: u16, text: &str) -> Response {
    Response::text(text)
        .with_status(status)
        .header("Cache-Control", "no-store")
        .header("X-Content-Type-Options", "nosniff")
}

/// The digest from the latest snapshot, as JSON.
///
/// The body is serialized per request because its age fields are the age of
/// the reading at this request: the plan's stored-bytes shape would freeze
/// them at the poll that made it, and a consumer could no longer tell a
/// 3-second-old digest from a 40-minute-old one.
fn digest_json(snapshot: Option<&Snapshot>, interval: Duration) -> Response {
    let Some(snap) = snapshot else {
        return Response::json_status(json!({"error": "no fleet poll has completed yet"}), 503)
            .header("Cache-Control", "no-store");
    };
    let (age, stale) = snap.age(interval);
    let mut v = serde_json::to_value(&snap.digest).unwrap_or(json!({}));
    // Age is part of the reading, not metadata.
    if let Some(obj) = v.as_object_mut() {
        obj.insert("snapshot_age_s".into(), json!((age * 10.0).round() / 10.0));
        obj.insert("snapshot_at".into(), json!(snap.at_utc));
        obj.insert("poll_interval_s".into(), json!(interval.as_secs()));
        obj.insert("stale".into(), json!(stale));
    }
    Response::json(v).header("Cache-Control", "no-store")
}

/// The whole A-to-H report, as text, from the latest snapshot.
///
/// Its first line is the age of the reading at this request, so the body is
/// built per request for the same reason `/digest`'s is.
fn check_text(snapshot: Option<&Snapshot>, interval: Duration) -> Response {
    let Some(snap) = snapshot else {
        return Response::text("no fleet poll has completed yet\n").with_status(503);
    };
    let (age, stale) = snap.age(interval);
    let mut out = String::new();
    // The age goes FIRST, before any figure, so it cannot be read past.
    if stale {
        out.push_str(&format!(
            "STALE: this reading is {age:.0}s old and the poll interval is {}s. \
             The poller is failing; the figures below are not current.\n\n",
            interval.as_secs()
        ));
    } else {
        out.push_str(&format!("polled {age:.1}s ago ({})\n", snap.at_utc));
    }
    out.push_str(&check::render(&snap.digest, &snap.readings, &snap.verdicts));
    Response::text(&out).header("Cache-Control", "no-store")
}

fn history_ndjson(req: &Request, history: &history::History) -> m6_core::Result<Response> {
    paged(req, history, history::health_page)
}

fn crawlers_ndjson(req: &Request, history: &history::History) -> m6_core::Result<Response> {
    paged(req, history, history::crawl_page)
}

/// The shared read path of `/history` and `/crawlers`.
///
/// One page reads inside one read transaction (see `history::fill_page`), in
/// chunks of 256 rows, and stops reading when the page is full.
fn paged<T: history::Keyed>(
    req: &Request,
    history: &history::History,
    read: history::PageFn<T>,
) -> m6_core::Result<Response> {
    use m6_core::ndjson::{Cursor, PageLimits};

    let Some(path) = history.file() else {
        return Ok(plain(
            503,
            "history is off: set [monitor] history_file to keep one\n",
        ));
    };
    let range = match m6_core::time_range::TimeRange::from_request(
        req,
        m6_core::time_range::DEFAULT_MAX_SPAN,
    ) {
        Ok(r) => r,
        Err(e) => return Ok(e.to_response()),
    };
    let after = match Cursor::from_query(req.query()) {
        Ok(None) => None,
        Ok(Some(c)) => match history::parse_cursor(c.as_str()) {
            Some(k) => Some(k),
            None => return Ok(m6_core::ndjson::CursorError::Malformed.to_response()),
        },
        Err(e) => return Ok(e.to_response()),
    };
    let node = match history::node_param(req.query()) {
        Ok(n) => n,
        Err(msg) => return Ok(Response::refusal(&msg)),
    };
    let unavailable =
        |e: &dyn std::fmt::Display| plain(503, &format!("history is unavailable: {e}\n"));
    let conn = match m6_core::sqlite::open_reader(path) {
        Ok(c) => c,
        Err(e) => return Ok(unavailable(&e)),
    };
    let node_id = match &node {
        None => None,
        Some(name) => match history::node_id(&conn, name) {
            Ok(Some(id)) => Some(id),
            Ok(None) => return Ok(Response::refusal(&"`node` names no node in the history")),
            Err(e) => return Ok(unavailable(&e)),
        },
    };
    let range = (range.from_unix_ms(), range.to_unix_ms());
    let (page, next) =
        match history::fill_page(&conn, range, node_id, after, PageLimits::default(), read) {
            Ok(p) => p,
            Err(history::PageError::Db(e)) => return Ok(unavailable(&e)),
            Err(history::PageError::Build(e)) => return Err(e.into()),
        };
    drop(conn);
    Ok(page.into_response(req.query(), next.as_ref()))
}

// ── the poll's result becomes the snapshot ──────────────────────────────────

/// Store a finished poll: the digest, the history batch, the page bytes and
/// the reading a check reads. A poll that did not finish changes nothing, so
/// the previous snapshot is kept and its age keeps growing.
fn publish(
    progress: poll::Progress,
    snapshot: &mut Option<Snapshot>,
    thresholds: &digest::Thresholds,
    history: &mut history::History,
) {
    let poll::Progress::Done(polled) = progress else {
        return;
    };
    let at_ms = unix_ms();
    let mut d = digest::build(&polled.readings, thresholds, now_iso8601());
    let h = history.digest();
    if let Some(f) = history::finding(&h) {
        d.add_finding(f);
    }
    d.history = Some(h);
    history.record(history::batch(at_ms, &polled.readings, &d));
    let page_status = if d.level == digest::Level::Fault {
        503
    } else {
        200
    };
    let page = render(&d).into_bytes();
    *snapshot = Some(Snapshot {
        digest: d,
        readings: polled.readings,
        verdicts: polled.verdicts,
        page,
        page_status,
        at: polled.at,
        at_utc: now_iso8601(),
    });
}

/// How many of the outgoing slots are nodes' requests.
fn nodes_in(out: &[poll::Out]) -> usize {
    out.len() - tunnel::PROBES_PER_POLL - check::MAX_LOOKUPS_PER_POLL
}

// ── --check: the same poll, once, in its own process ────────────────────────

/// Build the poll with its own reactor, drive it until it finishes, print the
/// check, and exit 1 on faults.
///
/// It shares every poll function with the service, so the page and the check
/// cannot disagree.
fn run_check(config_path: &Path) -> Result<()> {
    let fleet = fleet::Fleet::from_config(config_path)?;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let tls = Arc::new(
        rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()?
            .with_root_certificates(m6_core::client::system_roots()?)
            .with_no_client_auth(),
    );
    let thresholds = digest::Thresholds {
        tunnel_handshake_age_s: fleet.tunnel_handshake_max_age_s,
        log_reopen_age_s: fleet.log_reopen_max_age_s,
        ..Default::default()
    };
    // One run, one cache, with the wider one-shot cap. `--check` keeps no
    // history, so its digest says so.
    let mut poller = poll::one_shot_poller(
        &fleet,
        &tls,
        m6_core::dns::nameserver()?,
        check::MAX_LOOKUPS_PER_CHECK,
    )?;
    let mut out = poller.out();
    let polled = poll::drive_to_end(&mut poller, &mut out, None)?;
    let d = digest::build(&polled.readings, &thresholds, now_iso8601());
    print!("{}", check::render(&d, &polled.readings, &polled.verdicts));
    if d.level == digest::Level::Fault {
        std::process::exit(1);
    }
    Ok(())
}

fn unix_ms() -> i64 {
    m6_core::platform::wall()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

fn bytes(n: u64) -> String {
    const U: [&str; 4] = ["B", "K", "M", "G"];
    let mut v = n as f64;
    for (i, u) in U.iter().enumerate() {
        if v < 1024.0 || i == U.len() - 1 {
            return format!("{v:.0}{u}");
        }
        v /= 1024.0;
    }
    unreachable!()
}

fn opt<T: std::fmt::Display>(v: Option<T>) -> String {
    // An absent reading is a dash, never a zero. A zero is a measurement.
    v.map(|x| x.to_string()).unwrap_or_else(|| "-".to_string())
}

/// Rendered here rather than through a template on purpose.
///
/// This page has to work when the fleet does not, and a template is one more
/// thing that can be missing, mis-edited or fail to compile at exactly the
/// moment someone needs to read it. It is the same reasoning that keeps
/// `/health` free of a render step.
fn render(d: &digest::Digest) -> String {
    let mut h = String::with_capacity(8192);
    let (tone, word) = match d.level {
        digest::Level::Ok => ("#137547", "ALL CLEAR"),
        digest::Level::Warn => ("#b07d00", "WARNINGS"),
        digest::Level::Fault => ("#b3261e", "FAULTS"),
    };
    h.push_str(&format!(
        "<!doctype html><meta charset=utf-8><meta name=viewport content='width=device-width,initial-scale=1'>\
         <title>m6 fleet: {word}</title>\
         <style>\
         :root{{color-scheme:light dark}}\
         body{{font:14px/1.5 ui-monospace,SFMono-Regular,Menlo,monospace;margin:0;padding:24px;\
         background:Canvas;color:CanvasText}}\
         h1{{font-size:18px;margin:0 0 4px}}\
         .badge{{display:inline-block;padding:2px 10px;border-radius:3px;color:#fff;background:{tone};font-weight:700}}\
         .meta{{opacity:.7;margin:8px 0 20px}}\
         table{{border-collapse:collapse;width:100%;margin-bottom:24px}}\
         th,td{{text-align:right;padding:6px 10px;border-bottom:1px solid rgba(128,128,128,.3);white-space:nowrap}}\
         th:first-child,td:first-child{{text-align:left}}\
         th{{opacity:.7;font-weight:600}}\
         .wrap{{overflow-x:auto}}\
         .f{{padding:8px 12px;border-left:3px solid {tone};margin-bottom:6px;background:rgba(128,128,128,.08)}}\
         .f b{{font-family:inherit}}\
         .warn{{border-left-color:#b07d00}}\
         .dim{{opacity:.55}}\
         </style>\
         <h1>m6 fleet <span class=badge>{word}</span></h1>\
         <div class=meta>{} &middot; {} node(s)</div>",
        d.generated_at,
        d.nodes.len()
    ));

    if d.findings.is_empty() {
        h.push_str("<div class=f>Nothing to report.</div>");
    }
    for f in &d.findings {
        let cls = if f.level == digest::Level::Warn {
            "f warn"
        } else {
            "f"
        };
        h.push_str(&format!(
            "<div class='{cls}'><b>{}</b> &middot; {}</div>",
            m6_core::util::html_escape(&f.node),
            m6_core::util::html_escape(&f.text)
        ));
    }

    h.push_str(
        "<div class=wrap><table><tr>\
         <th>node</th><th>role</th><th>status</th><th>rtt</th>\
         <th>reqs</th><th>hit rate</th><th>p50</th><th>p99</th>\
         <th>load</th><th>mem</th><th>disk</th><th>temp</th>\
         <th>svc up</th><th>host up</th><th>tunnel</th></tr>",
    );
    for n in &d.nodes {
        let load = match (n.load_one, n.cpus) {
            (Some(l), Some(c)) => format!("{l:.2}/{c}"),
            _ => "-".to_string(),
        };
        let mem = match (n.memory_used, n.memory_total_bytes) {
            (Some(u), Some(t)) => format!("{:.0}% of {}", u * 100.0, bytes(t)),
            _ => "-".to_string(),
        };
        let disk = match (n.disk_used, n.disk_total_bytes) {
            (Some(u), Some(t)) => format!("{:.0}% of {}", u * 100.0, bytes(t)),
            _ => "-".to_string(),
        };
        h.push_str(&format!(
            "<tr><td>{}</td><td class=dim>{}</td><td>{}</td><td>{}</td>\
             <td>{}</td><td>{}</td><td>{}</td><td>{}</td>\
             <td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
            m6_core::util::html_escape(&n.name),
            m6_core::util::html_escape(&n.role),
            m6_core::util::html_escape(&n.status),
            n.rtt_ms
                .map(|v| format!("{v:.1}ms"))
                .unwrap_or_else(|| "-".into()),
            opt(n.requests_total),
            n.hit_rate
                .map(|v| format!("{v:.4}"))
                .unwrap_or_else(|| "-".into()),
            n.hit_p50_ns
                .map(|v| format!("{v}ns"))
                .unwrap_or_else(|| "-".into()),
            n.hit_p99_ns
                .map(|v| format!("{v}ns"))
                .unwrap_or_else(|| "-".into()),
            load,
            mem,
            disk,
            n.thermal_max_c
                .map(|c| format!("{c:.0}C"))
                .unwrap_or_else(|| "-".into()),
            n.uptime_s
                .map(digest::fmt_uptime)
                .unwrap_or_else(|| "-".into()),
            n.host_uptime_s
                .map(digest::fmt_uptime)
                .unwrap_or_else(|| "-".into()),
            // The same wording as /check section H.
            match &n.tunnel {
                Some(t) => m6_core::util::html_escape(&format!(
                    "{} handshake {}",
                    t.rtt_text(),
                    t.handshake_text()
                )),
                None => "<span class=dim>no tunnel</span>".to_string(),
            },
        ));
    }
    h.push_str("</table></div>");

    h.push_str("<div class=wrap><table><tr><th>node</th><th>backend pools</th></tr>");
    for n in &d.nodes {
        let pools = if n.pools.is_empty() {
            "<span class=dim>none (a cache node has no socket pools)</span>".to_string()
        } else {
            n.pools
                .iter()
                .map(|(name, a, t)| format!("{name} {a}/{t}"))
                .collect::<Vec<_>>()
                .join(" &middot; ")
        };
        h.push_str(&format!(
            "<tr><td>{}</td><td style='text-align:left'>{}</td></tr>",
            m6_core::util::html_escape(&n.name),
            pools
        ));
    }
    h.push_str("</table></div>");
    h
}

/// A failure at start is an error: print it and exit 2.
fn fail(what: &str, error: impl std::fmt::Display) -> ! {
    eprintln!("m6-monitor: {what}: {error}");
    std::process::exit(2)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node_digest(name: &str, tunnel: Option<digest::TunnelDigest>) -> digest::NodeDigest {
        let r = poll::NodeReading {
            name: name.into(),
            role: "cache".into(),
            url: "http://edge-a.example.com".into(),
            health: None,
            health_status: None,
            perf: None,
            perf_error: None,
            traffic: None,
            traffic_error: None,
            header_checks: Vec::new(),
            rtt: None,
            unreachable: None,
            tunnel: None,
        };
        let mut d = digest::build(&[r], &digest::Thresholds::default(), "now".into());
        let mut n = d.nodes.remove(0);
        n.tunnel = tunnel;
        n
    }

    /// The page carries the tunnel column in the wording of /check section H,
    /// and says "no tunnel" for a node without one.
    #[test]
    fn the_page_has_a_tunnel_column_and_says_no_tunnel_without_one() {
        let t = digest::TunnelDigest {
            address: "192.0.2.2:443".into(),
            rtt_ms: Some(0.4),
            rtt_samples_ms: vec![0.4, 0.5, 0.6],
            probe: digest::ProbeState::Connected,
            probe_error: None,
            handshake: digest::HandshakeState::Ok,
            handshake_age_s: Some(42),
            handshake_detail: None,
        };
        let mut d = digest::build(&[], &digest::Thresholds::default(), "now".into());
        d.nodes = vec![node_digest("edge-a", Some(t)), node_digest("edge-b", None)];
        let html = render(&d);
        assert!(html.contains("<th>tunnel</th>"), "{html}");
        assert!(
            html.contains("rtt 0.40ms (n 3, connected) handshake 42s ago"),
            "{html}"
        );
        assert!(html.contains("no tunnel"), "{html}");
    }

    /// The page's "svc up" column uses the same formatter as the check, so a
    /// node up 42 minutes reads "42m" on both and never "0h".
    #[test]
    fn the_page_uses_the_shared_uptime_formatter() {
        let mut d = digest::build(&[], &digest::Thresholds::default(), "now".into());
        let mut n = node_digest("edge-a", None);
        n.uptime_s = Some(42 * 60);
        d.nodes = vec![n];
        let html = render(&d);
        assert!(
            html.contains(&format!("<td>{}</td>", digest::fmt_uptime(42 * 60))),
            "{html}"
        );
        assert_eq!(digest::fmt_uptime(42 * 60), "42m");
    }

    /// Node name, role and status come from config and from the nodes, and
    /// reach the page as HTML.
    #[test]
    fn the_page_escapes_node_name_role_and_status() {
        let mut d = digest::build(&[], &digest::Thresholds::default(), "now".into());
        let mut n = node_digest("<i>edge</i>", None);
        n.role = "<b>cache</b>".into();
        n.status = "<u>ok</u>".into();
        d.nodes = vec![n];
        d.findings.push(digest::Finding {
            level: digest::Level::Fault,
            node: "<s>monitor</s>".into(),
            text: "file </p>/x is old".into(),
        });
        let html = render(&d);
        assert!(!html.contains("<s>monitor"), "{html}");
        assert!(html.contains("file &lt;/p&gt;/x is old"), "{html}");
        assert!(!html.contains("<i>edge"), "{html}");
        assert!(!html.contains("<b>cache"), "{html}");
        assert!(!html.contains("<u>ok"), "{html}");
        assert!(html.contains("&lt;i&gt;edge&lt;/i&gt;"), "{html}");
        assert!(html.contains("&lt;b&gt;cache&lt;/b&gt;"), "{html}");
        assert!(html.contains("&lt;u&gt;ok&lt;/u&gt;"), "{html}");
    }

    /// Every client config in this service names its provider.
    ///
    /// The automatic client-config builder asks rustls for the process
    /// default. In a workspace-wide test build rustls is compiled with both
    /// providers, because m6-http's dev-dependency `tokio-rustls` pulls
    /// `aws-lc-rs` in, so there is no default and the first call panics. A
    /// per-crate build compiles only ring and hides it, which is how a
    /// per-crate gate passed while the workspace build failed. Naming ring at
    /// each call site keeps the monitor starting whatever else is in the
    /// build. Nothing is installed process-wide: that is state a library
    /// must not set.
    #[test]
    fn every_client_config_names_its_provider() {
        let needle = ["ClientConfig::", "builder("].concat();
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = 0;
        let mut found = Vec::new();
        for entry in std::fs::read_dir(&dir).expect("read m6-monitor/src") {
            let path = entry.expect("directory entry").path();
            if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                continue;
            }
            let text = std::fs::read_to_string(&path).expect("read a source file");
            files += 1;
            for (line, body) in text.lines().enumerate() {
                if body.contains(&needle) {
                    found.push(format!("{}:{}: {}", path.display(), line + 1, body.trim()));
                }
            }
        }
        assert!(files > 0, "read no source files under {}", dir.display());
        assert!(
            found.is_empty(),
            "a client config without a named provider:\n{}",
            found.join("\n")
        );
    }
}
