//! `m6-monitor`: one page that says how the fleet is.
//!
//! Runs at origin, polls every node's `/health` over the backbone, reads every
//! node's metrics and traffic documents and every start-up record from mon's
//! own journal, and serves the digest. These routes:
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
//! since the event-loop refactor: `m6_core::event` supplies the signals, the deadlines and the
//! connection table, and `m6_core::platform` the reactor. The fleet poll is not a thread.
//! It is a table of steps in this loop (`poll.rs`): each node's `/health`
//! request, each tunnel connect and each crawler lookup is a descriptor the
//! loop waits on, and so is the `journalctl` pipe `journal.rs` reads the two
//! documents from, so a node that hangs holds only its own step and never the
//! monitor.
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
//! `/health` is an HTTP endpoint and each node ships its journal to mon over
//! the tunnel, so the data is reachable without a shell. What was missing was
//! the host's own numbers, which the metrics line now carries. Polling and
//! reading a journal rather than shelling in means the monitor needs no
//! credentials on the boxes, measures the path it actually cares about, and
//! fails the way a service fails rather than the way a script does.

#![deny(unsafe_code)]

mod check;
mod digest;
mod events;
mod fleet;
mod history;
mod journal;
mod poll;
mod startup;
mod tunnel;

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use m6_core::drain::{Drain, Step};
use m6_core::event::{LoopTrace, Slots, Timeouts};
use m6_core::h1::{close_conn, head_slots, Flush, Read, RequestHead, ServerConn};
use m6_core::net::listen_unix;
use m6_core::platform::process::raise_fd_limit;
use m6_core::platform::Signals;
use m6_core::platform::{Events, Interest, Poll, Token};
use m6_core::prelude::*;
use m6_core::signal::{Action, Lifecycle};
use m6_core::site::RouteMethod;
use m6_core::{genid_failed, genid_ok};

/// The global allocator: mimalloc, which also counts calls when the
/// `count-allocations` feature turns on m6-core's `alloc-count`. The counting
/// is not compiled in a plain build.
#[global_allocator]
static ALLOC: m6_core::alloc::M6Alloc = m6_core::alloc::M6Alloc;

const LISTENER: Token = Token(usize::MAX);
const SIGNALS: Token = Token(usize::MAX - 1);
/// The operational log's stdout, registered while it holds bytes a full
/// destination would not take.
const LOG: Token = Token(usize::MAX - 2);
/// The journal reader's pipe: the `journalctl` child of the read in flight.
const JOURNAL: Token = Token(usize::MAX - 3);
/// The event series' own journal read. A second descriptor, so the two
/// readers cannot take each other's lines.
const EVENTS_JOURNAL: Token = Token(usize::MAX - 4);
/// The accounting tick, and the longest the loop waits when nothing happens.
const TICK: Duration = Duration::from_secs(1);
const EVENTS: usize = 256;
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
    // `--version` and `--version-full` answer from constants fixed at build
    // time, before a config path is required. `--check` is read below.
    m6_core::version_flags!(m6_core::build_info::linked());

    // Never hand freed memory back to the OS: the heap grows and does not shrink.
    m6_core::alloc::never_return();
    // `m6-monitor --check <config>` prints the hourly health check and exits.
    // Same poll code as the page, so the two cannot disagree.
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--check") {
        let config = args
            .iter()
            .skip(1)
            .find(|a| !a.starts_with("--"))
            .ok_or_else(|| anyhow::anyhow!("usage: m6-monitor --check <config-path>"))?;
        // A one-shot tool: no loop, so the log starts here and stops before
        // the process exits.
        m6_core::log::init(m6_core::log::Level::Info)?;
        let result = run_check(Path::new(config));
        m6_core::log::stop()?;
        return result;
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
    let mut site = Site::load_deferring_dump("m6-monitor", &ROUTES, &[], &NoTemplates);
    let read_timeout = site.read_timeout();
    let interval = Duration::from_secs(fleet::poll_interval_s(|k| {
        site.user_config().get(k).and_then(|v| v.as_u64())
    }));
    let fleet = fleet::Fleet::from_config(site.config_path())
        .unwrap_or_else(|e| fail("reading the fleet", format!("{e:#}")));
    let history_config = history::config_from(site.user_config().get("monitor"), site.site_dir())
        .unwrap_or_else(|e| fail("reading the history config", format!("{e:#}")));

    // `--dump-config` stops here, after the same fleet and history parse the
    // start runs and before anything resolves a name, opens a file or binds a
    // socket. One sequence, so the dump cannot accept a config the start
    // refuses.
    if site.dump_requested() {
        std::process::exit(site.dump(&ROUTES, &[]));
    }

    m6_core::alloc::reserve(site.limits().get(&m6_core::limits::HEAP_RESERVE).start());
    let tls = Arc::new(
        rustls::ClientConfig::builder()
            .with_root_certificates(
                m6_core::client::system_roots()
                    .unwrap_or_else(|e| fail("reading the host certificate store", e)),
            )
            .with_no_client_auth(),
    );
    let nodes = fleet
        .resolve(&tls)
        .unwrap_or_else(|e| fail("resolving the fleet", format!("{e:#}")));
    // The journal read's `--lines` window follows the fleet count, so it is
    // taken before the fleet moves into the poller.
    let node_count = nodes.len();
    m6_core::ndjson::set_page_limit(site.limits().get(&m6_core::limits::NDJSON_PAGE));
    let nameserver = m6_core::dns::nameserver()
        .unwrap_or_else(|e| fail("reading the resolver configuration", e));
    let mut history = history::start(
        history_config,
        site.limits().get(&m6_core::limits::HISTORY_QUEUE),
    )
    .unwrap_or_else(|e| fail("opening the history file", format!("{e:#}")));
    let thresholds = digest::Thresholds {
        tunnel_handshake_age_s: fleet.tunnel_handshake_max_age_s,
        health_last_ok_age_s: fleet.health_last_ok_limit_s(),
        one_certificate: fleet.one_certificate,
        ..Default::default()
    };
    let mut poller = poll::Poller::new(poll::Setup {
        nodes,
        timeout: Duration::from_millis(fleet.timeout_ms),
        header_checks: fleet.header_check.clone(),
        nameserver,
        wg_file: fleet.wg_handshakes_file.clone(),
        wg_max_age_s: fleet.tunnel_handshake_max_age_s,
        lookup_cap: check::MAX_LOOKUPS_PER_POLL,
        upstream_read: site.limits().get(&m6_core::limits::UPSTREAM_READ).clone(),
        upstream_request: site
            .limits()
            .get(&m6_core::limits::UPSTREAM_REQUEST)
            .clone(),
        expected_file: fleet.expected_deploy_path(site.site_dir()),
        environment: fleet.environment.clone(),
        event_series: fleet.event_series.clone(),
    });

    // The journal is where a node's metrics and traffic lines are read from.
    // A directory that is not there is said once and reported as a fault,
    // never a refusal to start: the monitor's own page and check still run.
    let journal_limit = site.limits().get(&m6_core::limits::JOURNAL_LINE).clone();
    // One answer for both readers: the documents read and the event-series
    // read are given the same directory, and the same reason when it is not
    // there, so the page cannot report one of them working and the other not.
    let journal_dir = fleet
        .journal_path(site.site_dir())
        .map_err(|e| format!("{e:#}"));
    poller.journal = match &journal_dir {
        Ok(dir) => journal::Reader::open(dir, node_count, interval, &journal_limit),
        Err(reason) => {
            m6_core::log::error!(error = %reason, "the journal cannot be read");
            journal::Reader::unavailable(reason.clone(), &journal_limit)
        }
    };
    poller.open_event_read(journal_dir, &journal_limit, interval);

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

    // The whole connection table, every buffer in it, the deadlines, the
    // outgoing slots and the events: made here, never grown except by `Slots`.
    let read_limit = site.limits().get(&m6_core::limits::REQUEST_BODY).clone();
    let write_limit = site.limits().get(&m6_core::limits::RESPONSE_BUFFER).clone();
    let mut conns = Slots::new(initial, ceiling, move || {
        ServerConn::with_limits(&read_limit, &write_limit)
    });
    // Accepted work and the shutdown drain, for client connections only: an
    // outgoing node request is the monitor's own work and no client is waiting
    // on it.
    let drain_timeout = site.drain_timeout();
    let mut drain = Drain::new(drain_timeout, initial);
    // A config with `read_timeout_s = 0` keeps the zero duration and never
    // touches a deadline.
    let mut deadlines = Timeouts::new(initial, read_timeout.unwrap_or(Duration::ZERO));
    let mut events = Events::with_capacity(EVENTS);
    let mut trace = LoopTrace::new(&["accept", "client", "node", "probe", "dns", "tick", "log"]);
    let mut out = poller.out();
    let mut accepting = true;
    let mut snapshot: Option<Snapshot> = None;
    let mut poll_reported_late = false;
    let mut last = m6_core::platform::now();
    let mut last_report = last;
    #[cfg(feature = "count-allocations")]
    let mut client_allocs = 0u64;

    // The journal's pipe joins the loop as one more descriptor, when there is
    // one. It leaves again when the child stops, after the reactor has let the
    // descriptor go.
    let mut journal_fd = None;
    if let Some(fd) = poller.journal.fd() {
        poll.registry()
            .register_fd(fd, JOURNAL, Interest::READABLE)
            .unwrap_or_else(|e| fail("registering the journal", e));
        journal_fd = Some(fd);
    }
    // The event-series read is its own child and its own pipe, so it cannot
    // take the documents read's turn.
    let mut events_fd = None;
    if let Some(fd) = poller.events_journal.fd() {
        poll.registry()
            .register_fd(fd, EVENTS_JOURNAL, Interest::READABLE)
            .unwrap_or_else(|e| fail("registering the event-series journal", e));
        events_fd = Some(fd);
    }

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
        let mut wait = deadlines.timeout(now, TICK).min(poller.timeout(now, TICK));
        // While draining, the wait is capped at the deadline: nothing else
        // would wake the loop to notice it.
        if let Some(left) = drain.poll_timeout(now) {
            wait = wait.min(left);
        }
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
            Some(LOG) => 6,
            Some(JOURNAL) | Some(EVENTS_JOURNAL) => 6,
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
                        m6_core::net::warn_refused("m6-monitor", counts);
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
                JOURNAL => {
                    if poller.read_journal() == journal::Event::Exited {
                        if let Some(fd) = journal_fd.take() {
                            let _ = poll.registry().deregister_fd(fd);
                        }
                        poller.journal.retire();
                    }
                }
                EVENTS_JOURNAL => {
                    if poller.read_events_journal() == journal::Event::Exited {
                        if let Some(fd) = events_fd.take() {
                            let _ = poll.registry().deregister_fd(fd);
                        }
                        poller.events_journal.retire();
                    }
                }
                // One connection's turn: answer from the snapshot, or from
                // the history for the two paged routes.
                Token(k) if k < poll::CLIENT_BASE => {
                    // A turn on the connection is accepted work. An idle
                    // connection that sent another request is counted here,
                    // so a request that arrives during the drain is answered.
                    drain.begin(k);
                    #[cfg(feature = "count-allocations")]
                    let before = m6_core::testkit::alloc_count::now();
                    let conn = conns.get_mut(k);
                    let was = conn.interest();
                    let turn = turn(conn, &mut site, snapshot.as_ref(), &history, interval);
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
                            // Answered and holding nothing: idle, so it stops
                            // holding the drain.
                            if conn.served() > 0 && !conn.pending() {
                                drain.end(k);
                            }
                        }
                        _ => close(&poll, &mut conns, &mut deadlines, &mut drain, k)?,
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

        // A full stdout holds its bytes in the Sink. Watch it for writability
        // only while it does, so the loop never waits on the log.
        if let Err(e) = m6_core::log::watch(poll.registry(), LOG) {
            m6_core::log::error!(error = %e, "log descriptor registration failed");
        }

        // Outgoing work whose deadline passed: a timed-out node step, a lost
        // probe or a failed lookup, each recorded and moved on.
        while let Some(progress) = poller.expire(&poll, &mut out, m6_core::platform::now())? {
            publish(progress, &mut snapshot, &thresholds, &mut history);
        }
        // Close every client whose read deadline passed, with no response.
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

        // The tick: start a poll when the interval has passed since the last
        // one started and none is running. A poll still running at its next
        // due time is not restarted; the tick says so once and waits.
        let now = m6_core::platform::now();
        let due = poller
            .last_start()
            .is_none_or(|start| now.duration_since(start) >= interval);
        if due && poller.is_running() {
            if !poll_reported_late {
                m6_core::log::debug!("a poll is still running at its next due time; waiting");
                poll_reported_late = true;
            }
        } else if due {
            poll_reported_late = false;
            let progress = poller.start(&poll, &mut out, now)?;
            publish(progress, &mut snapshot, &thresholds, &mut history);
        }

        // The journal read has the poll's interval and the loop's own tick, and
        // no timer of its own. A read in flight past one interval is killed and
        // reported. At most one read starts per interval, so a poll that
        // overruns does not turn this into a read on every pass, and a read
        // still in flight is not doubled. A read that starts gets its pipe
        // registered here.
        if poller.journal.expire(now) {
            if let Some(fd) = journal_fd.take() {
                let _ = poll.registry().deregister_fd(fd);
            }
            poller.journal.retire();
            m6_core::log::error!(
                error = poller.journal.stopped.as_deref().unwrap_or_default(),
                "the journal read overran"
            );
        }
        if poller.journal.start_if_idle(now) {
            if let Some(fd) = poller.journal.fd() {
                poll.registry()
                    .register_fd(fd, JOURNAL, Interest::READABLE)?;
                journal_fd = Some(fd);
            }
        }
        if poller.events_journal.expire(now) {
            if let Some(fd) = events_fd.take() {
                let _ = poll.registry().deregister_fd(fd);
            }
            poller.events_journal.retire();
            m6_core::log::error!(
                error = poller.events_journal.stopped.as_deref().unwrap_or_default(),
                "the event-series journal read overran"
            );
        }
        if poller.events_journal.start_if_idle(now) {
            if let Some(fd) = poller.events_journal.fd() {
                poll.registry()
                    .register_fd(fd, EVENTS_JOURNAL, Interest::READABLE)?;
                events_fd = Some(fd);
            }
        }

        // The accounting tick: report and clear the loop's wait and work per
        // trigger. The monitor serves no metrics page, so the report is the
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
    // Outgoing work in flight is dropped. A batch held for a busy database is
    // tried once more; a failure is logged, never a wait.
    history.flush();
    #[cfg(feature = "count-allocations")]
    eprintln!("{client_allocs} allocation(s) in the client arm outside table growth");
    // The journal's child is killed here, so no `journalctl` outlives the
    // monitor.
    poller.journal.retire();
    // Reaching here means nothing accepted is left, or the drain deadline
    // passed and the warning already named how many were. `complete` removes
    // the socket and logs the line.
    lifecycle.complete();
    m6_core::log::stop()?;
    Ok(())
}

/// Stop accepting, so new clients wait in the kernel's backlog.
fn stop_accepting(
    registry: &m6_core::platform::Registry,
    listener: &mut m6_core::platform::net::UnixListener,
    accepting: &mut bool,
) -> std::io::Result<()> {
    if *accepting {
        registry.deregister(listener)?;
        *accepting = false;
    }
    Ok(())
}

/// Close one client connection and stop counting its accepted work.
fn close(
    poll: &Poll,
    conns: &mut Slots<ServerConn>,
    deadlines: &mut Timeouts,
    drain: &mut Drain,
    k: usize,
) -> std::io::Result<()> {
    drain.end(k);
    close_conn(poll, conns, deadlines, k)
}

/// One connection's turn: write what is owed, read, answer and write until the
/// socket would block. Returns whether the connection stays open.
fn turn(
    conn: &mut ServerConn,
    site: &mut Site,
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
            let reply = match reply {
                // The reserved path: run the start path again and assign the
                // new Site in the loop's own binding, or keep serving the old
                // one and answer the cause.
                Answer::Reload(reload) => {
                    let response = match site.reload(&ROUTES, &[], &NoTemplates) {
                        Ok(next) => {
                            *site = next;
                            genid_ok()
                        }
                        Err(e) => genid_failed(&e),
                    };
                    Answer::Response(m6_core::site::finish_reload(&reload, response))
                }
                answer => answer,
            };
            match reply {
                Answer::Bytes {
                    status,
                    headers,
                    body,
                } => conn.respond(status, headers, body),
                Answer::Response(response) => conn.respond_response(response),
                Answer::Reload(_) => unreachable!("the reload arm above replaced it"),
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
    /// `POST /genid`: the turn runs the start path again and assigns the new
    /// Site, which only the loop can do, then writes the reload's access line.
    Reload(m6_core::site::Reload),
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
        Handled::Reload(reload) => Answer::Reload(reload),
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
    let at_ms = history::now_ms();
    let mut d = digest::build(&polled.readings, thresholds, now_iso8601());
    // The start-up record's checks: the deploy's expectation file and the
    // journal's own records, judged beside every other reading.
    startup::judge(&mut d, &polled.startup);
    // The event series: the counts on the page, and a Fault for any series
    // whose refusal names the service's own configuration.
    d.events = polled.events;
    for f in events::findings(&d.events) {
        d.add_finding(f);
    }
    let h = history.digest();
    if let Some(f) = history::finding(&h) {
        d.add_finding(f);
    }
    d.history = Some(h);
    // The HEALTH section's own judgement reaches the verdict through the same
    // list as every other fault, so a line that reads FAULT cannot sit above
    // an ALL CLEAR. The digest already raises the unreachable and degraded
    // ones, and this adds the age of the last 200, which only the history
    // knows.
    for r in &polled.readings {
        if let Some(f) =
            check::stale_last_ok_finding(r, d.history.as_ref(), thresholds.health_last_ok_age_s)
        {
            d.add_finding(f);
        }
    }
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
    let tls = Arc::new(
        rustls::ClientConfig::builder()
            .with_root_certificates(m6_core::client::system_roots()?)
            .with_no_client_auth(),
    );
    let thresholds = digest::Thresholds {
        tunnel_handshake_age_s: fleet.tunnel_handshake_max_age_s,
        health_last_ok_age_s: fleet.health_last_ok_limit_s(),
        one_certificate: fleet.one_certificate,
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
    // The deploy's expectation file, resolved the same way the journal
    // directory is: against the config file's own directory.
    let site_dir = config_path.parent().unwrap_or(Path::new("."));
    poller.set_expected_file(fleet.expected_deploy_path(site_dir));
    // The metrics and traffic documents are read from the journal before the
    // poll, until every node has both or the settle bound passes. A one-shot
    // tool may wait here, so this is a bounded read rather than a loop turn.
    let limit = m6_core::limits::Limit::builtin(&m6_core::limits::JOURNAL_LINE);
    poller.read_journal_once(
        fleet
            .journal_path(config_path.parent().unwrap_or(Path::new(".")))
            .map_err(|e| format!("{e:#}")),
        &limit,
    );
    let mut out = poller.out();
    let polled = poll::drive_to_end(&mut poller, &mut out, None)?;
    let mut d = digest::build(&polled.readings, &thresholds, now_iso8601());
    startup::judge(&mut d, &polled.startup);
    d.events = polled.events;
    for f in events::findings(&d.events) {
        d.add_finding(f);
    }
    // The report goes through `print!`, so stop the log first: it flushes and
    // puts stdout back to the blocking flag the process started with.
    m6_core::log::stop()?;
    print!("{}", check::render(&d, &polled.readings, &polled.verdicts));
    if d.level == digest::Level::Fault {
        std::process::exit(1);
    }
    Ok(())
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
                .map(|(name, count)| format!("{name} {count}"))
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

    /// The automatic client-config builder resolves the one provider.
    ///
    /// This replaces the source scan that required every
    /// `ClientConfig::builder()` call to name its provider. That rule existed
    /// because a workspace-wide test build compiled rustls with both providers:
    /// m6-http's dev-dependency `tokio-rustls` pulled `aws-lc-rs` in while the
    /// direct dependency named ring, so the automatic builder had no default
    /// and panicked. A per-crate build compiled only ring and hid it, which is
    /// how a per-crate gate passed while the workspace build failed.
    ///
    /// m6-monitor, m6-core and m6-http now all name `aws_lc_rs` and none names
    /// `ring`, so there is one provider in every build and the automatic
    /// builder is unambiguous. This test holds that invariant by behaviour: a
    /// second provider pulled into any workspace build panics the builder, and
    /// a workspace that resolved a different single provider fails the
    /// key-exchange check. X25519MLKEM768 is what tells aws-lc-rs's default
    /// apart from ring's.
    #[test]
    fn the_automatic_client_config_builder_resolves_the_one_provider() {
        let _config = rustls::ClientConfig::builder()
            .with_root_certificates(rustls::RootCertStore::empty())
            .with_no_client_auth();
        let provider = rustls::crypto::CryptoProvider::get_default()
            .expect("the automatic builder installs the provider from the crate features");
        assert!(
            provider
                .kx_groups
                .iter()
                .any(|g| matches!(g.name(), rustls::NamedGroup::X25519MLKEM768)),
            "the automatic builder resolved a provider other than aws-lc-rs"
        );
    }
}
