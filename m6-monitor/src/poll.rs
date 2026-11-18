//! Fetching one node's view of itself, as steps an event loop drives.
//!
//! `/health` and each declared `[[monitor.header_check]]` are the requests. A
//! header check asks the node's public address, because it asserts what a
//! visitor receives. `/health` asks the status port of a process entry and the
//! public address otherwise.
//!
//! The metrics and traffic documents are steps too, and they read mon's
//! journal, where the line carries `perf` or `traffic` on `M6_TYPE` and names
//! its node in the document's own `node` field. Nothing is asked of a node for
//! them. The start-up and shutdown lines arrive on the same reader and are kept
//! beside the two documents, so a poll carries the current run of each node and
//! app as well.
//!
//! # A poll is a table of steps, not a thread per node
//!
//! [`Poller`] owns one [`Out`] slot per node plus the probe and lookup slots.
//! Starting a poll writes each node's first request into that node's
//! [`Upstream`] and registers its descriptor. A descriptor that fires moves
//! its node one step on and starts the next request. The nodes make progress
//! at once because the loop returns to each ready socket, and no thread and no
//! blocking call is involved.
//!
//! The request bytes are written into the slot's own buffer and the reply is
//! parsed in place, so a poll allocates only where parsing a report does. The
//! poll runs once per interval, behind every request the monitor serves.

use std::io::{self, Write as _};
use std::net::SocketAddr;
use std::os::fd::{AsRawFd, RawFd};
use std::time::{Duration, Instant};

use m6_core::client::{reply_slots, Addr, Reply, Upstream};
use m6_core::dns::{Confirm, Ptr};
use m6_core::event::Timeouts;
use m6_core::monitoring::{HealthReport, PerfReport, TrafficReport};
use m6_core::platform::net::TcpStream as MioTcpStream;
use m6_core::platform::{Events, Interest, Poll, Token};

use crate::check::{VerdictCache, Verdicts};
use crate::fleet::{HeaderCheck, ResolvedNode};
use crate::tunnel::{self, TunnelReading};

/// The first token reserved for outgoing work. Client connections use the
/// tokens below it, far above any table the open-file limit allows.
pub const CLIENT_BASE: usize = 1 << 20;

/// The lookup slots in the outgoing table. A poll due more addresses than this
/// starts them as the earlier ones finish.
const DNS_SLOTS: usize = crate::check::MAX_LOOKUPS_PER_POLL;

/// What one node answered, and what it cost to ask.
#[derive(Debug, Clone)]
pub struct NodeReading {
    pub name: String,
    pub role: String,
    pub url: String,
    /// `/health`: present unless the node could not be reached at all.
    pub health: Option<HealthReport>,
    /// HTTP status from `/health`. 503 is a degraded node answering honestly,
    /// which is different from no answer.
    pub health_status: Option<u16>,
    /// The metrics document: absent while the journal holds no line for this
    /// node, or when the newest line does not parse.
    pub perf: Option<PerfReport>,
    /// Why the metrics document is absent, when it is. An aggregator that
    /// silently shows nothing for a node is indistinguishable from a healthy
    /// quiet node.
    pub perf_error: Option<String>,
    /// Round trip for `/health`, measured from here, **including connect and
    /// TLS**.
    ///
    /// This is a network measurement, not the node's own latency. It is the
    /// cost of reaching the node from wherever the monitor runs, and over a
    /// long link the handshake dominates it. A near node and an
    /// intercontinental one can differ by a factor of thirty here with almost
    /// all of the difference being the handshake rather than anything either
    /// node did. Do not compare it with the loopback TTFB, which measures the
    /// opposite thing.
    pub rtt: Option<Duration>,
    /// Outcome of each `[[monitor.header_check]]` the fleet config declared.
    ///
    /// Empty when none are declared, which the report states rather than
    /// rendering as a pass: "nothing configured" and "everything correct" must
    /// not look the same.
    pub header_checks: Vec<HeaderCheckResult>,
    /// The traffic document: this node's summary of its own traffic and
    /// whether its logging is alive. Absent for the same reasons the metrics
    /// document can be.
    pub traffic: Option<TrafficReport>,
    pub traffic_error: Option<String>,
    /// Set when the node could not be reached at all.
    pub unreachable: Option<String>,
    /// The WireGuard tunnel to this node. Filled by the poller after the
    /// requests, and `None` when no tunnel is configured.
    pub tunnel: Option<crate::tunnel::TunnelReading>,
}

impl NodeReading {
    pub fn is_up(&self) -> bool {
        self.unreachable.is_none()
    }

    /// Whether the node says it can serve, and the one place that decides it.
    ///
    /// A node is "ok" only when `/health` answered 200 and its body says "ok".
    /// A non-200 code is "degraded" whatever the body says, a 200 carries the
    /// body's own status, a 200 with no body is "unknown", and no answer is
    /// "unreachable". The digest, the history row and the HEALTH section all
    /// read this, so they cannot disagree.
    pub fn status(&self) -> &str {
        if self.unreachable.is_some() {
            return "unreachable";
        }
        match (self.health_status, &self.health) {
            (Some(200), Some(h)) => h.status.as_str(),
            (Some(200), None) | (None, _) => "unknown",
            (Some(_), _) => "degraded",
        }
    }

    /// The reading with every polled field cleared, so the next poll resets it
    /// in place rather than building a new one.
    fn reset(&mut self) {
        self.health = None;
        self.health_status = None;
        self.perf = None;
        self.perf_error = None;
        self.traffic = None;
        self.traffic_error = None;
        self.header_checks.clear();
        self.rtt = None;
        self.unreachable = None;
        self.tunnel = None;
    }
}

/// What one declared header check found.
#[derive(Debug, Clone)]
pub struct HeaderCheckResult {
    pub path: String,
    pub header: String,
    pub expected: String,
    /// What came back, or None if the header was absent entirely. Absent and
    /// wrong are different failures and are reported differently.
    pub actual: Option<String>,
    pub status: Option<u16>,
    /// Transport failure, as opposed to a wrong answer.
    pub error: Option<String>,
}

impl HeaderCheckResult {
    pub fn passed(&self) -> bool {
        self.error.is_none() && self.actual.as_deref() == Some(self.expected.as_str())
    }
}

/// One node's place in the poll.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Step {
    Health,
    /// One declared header check, by its index in the fleet config.
    Header(usize),
    Done,
}

/// One entry of the outgoing table: what the loop is waiting on, and for whom.
///
/// The node variant is the large one, and it is deliberate: the table holds
/// one [`Upstream`] per node, made at start, and a box around it would put an
/// indirection on every event for no gain. Nothing here is created in the loop.
#[allow(clippy::large_enum_variant)]
pub enum Out {
    /// One node's request, on its own [`Upstream`] with its own buffers.
    Node {
        node: usize,
        step: Step,
        up: Upstream,
        /// The descriptor registered for this slot, while one is.
        fd: Option<RawFd>,
    },
    /// One non-blocking connect to a node's tunnel address.
    Probe {
        node: usize,
        /// Which of the node's `PROBES_PER_POLL` connects this is.
        n: usize,
        /// When this connect started, for its round trip.
        started: Instant,
        sock: Option<MioTcpStream>,
        fd: Option<RawFd>,
    },
    /// One reverse-then-forward lookup on its own UDP socket.
    Dns {
        addr: String,
        confirm: Confirm,
        fd: RawFd,
    },
    Free,
}

/// Why the loop came back: still working, or the poll finished.
// A poll's result is built once and held from `finish` to `publish`, and it
// carries every reading of the fleet. Boxing the large variant would put an
// indirection on every field read for no gain, the reason `Out` carries the
// same allowance below.
#[allow(clippy::large_enum_variant)]
pub enum Progress {
    Working,
    Done(Polled),
}

/// One completed poll: everything the snapshot and the check are built from.
pub struct Polled {
    pub readings: Vec<NodeReading>,
    pub verdicts: Verdicts,
    /// When the poll finished.
    pub at: Instant,
    /// The start-up records the journal carried and the deploy's expected
    /// values, read at the same poll as everything else.
    pub startup: crate::startup::Facts,
}

/// The poll in flight.
struct Running {
    active: bool,
    /// Set while [`Poller::start`] is starting the first step of every node:
    /// an empty fleet must not finish the poll before it began.
    starting: bool,
    /// Nodes that have not reached `Done`.
    outstanding: usize,
    /// When each node's current step started, for the `/health` round trip.
    step_started: Vec<Instant>,
    /// The probe tally being built for each node, reset every poll.
    probes: Vec<tunnel::Tally>,
    /// The next node whose tunnel to probe.
    probe_cursor: usize,
    probing: Option<Probing>,
    probes_done: bool,
    /// The addresses that need a lookup this poll, reused every poll.
    due: Vec<String>,
    dns_started: bool,
    /// The next due address to start.
    dns_next: usize,
    /// Lookups registered now.
    dns_active: usize,
    /// Lookups not yet finished.
    dns_left: usize,
    /// The allocation count when the poll began, under the test feature.
    #[cfg(feature = "count-allocations")]
    alloc_start: usize,
}

/// The node whose tunnel is being probed.
struct Probing {
    node: usize,
    /// When the node's one budget of `timeout` began.
    started: Instant,
}

/// What the poller is built from, all read at start.
pub struct Setup {
    pub nodes: Vec<ResolvedNode>,
    /// The fleet's timeout for one step.
    pub timeout: Duration,
    pub header_checks: Vec<HeaderCheck>,
    pub nameserver: SocketAddr,
    pub wg_file: Option<String>,
    pub wg_max_age_s: u64,
    /// The most lookups one poll makes. `--check` passes the wider cap.
    pub lookup_cap: usize,
    /// The `upstream_read` limit each node's request slot follows.
    pub upstream_read: m6_core::limits::Limit,
    /// The `upstream_request` limit each node's request slot follows.
    pub upstream_request: m6_core::limits::Limit,
    /// `[monitor] expected_deploy_file`, resolved. `None` when the key is not
    /// set, which the check reports as "no deploy record configured".
    pub expected_file: Option<std::path::PathBuf>,
    /// `[monitor] environment`, when the config declares one.
    pub environment: Option<String>,
}

/// Every node's own view, driven by a service's loop.
pub struct Poller {
    nodes: Vec<ResolvedNode>,
    readings: Vec<NodeReading>,
    /// The fleet's timeout for one step.
    timeout: Duration,
    header_checks: Vec<HeaderCheck>,
    /// The newest metrics and traffic document the journal carried, by node.
    latest: crate::journal::Latest,
    /// The one journal reader: the service's `--follow` child, or a check's
    /// one-shot read. Its `stopped` is the one record of why it is not
    /// running.
    pub journal: crate::journal::Reader,
    cache: VerdictCache,
    nameserver: SocketAddr,
    wg_file: Option<String>,
    wg_max_age_s: u64,
    upstream_read: m6_core::limits::Limit,
    upstream_request: m6_core::limits::Limit,
    /// The deploy's expected values, re-read at each poll so a deploy lands
    /// without a monitor restart.
    expectations: crate::startup::Expectations,
    expected_state: crate::startup::ExpectedState,
    expected_file: Option<std::path::PathBuf>,
    environment: Option<String>,
    running: Running,
    last_start: Option<Instant>,
    /// The deadlines for the outgoing slots, indexed by their table position.
    pub out_timeouts: Timeouts,
    /// Test seam: make the next tunnel probe's connect fail at once with this
    /// error. Set on the one poller the test drives, so no other test can see
    /// it, and never built outside a test.
    #[cfg(test)]
    probe_connect_failure: std::cell::Cell<Option<io::ErrorKind>>,
    /// Test seam: let this many step starts pass, then fail the next one at
    /// once with this error.
    #[cfg(test)]
    step_start_failure: std::cell::Cell<Option<(usize, io::ErrorKind)>>,
}

/// One document from the journal for one node, or why there is none.
fn journal_document<T: serde::de::DeserializeOwned>(
    document: Option<&serde_json::Value>,
    stopped: Option<&str>,
    what: &str,
) -> Result<T, String> {
    match (document, stopped) {
        (Some(document), _) => {
            T::deserialize(document).map_err(|e| format!("unreadable {what} line: {e}"))
        }
        (None, Some(reason)) => Err(format!(
            "no {what} line: the journal reader is not running: {reason}"
        )),
        (None, None) => Err(format!("no {what} line in the journal for this node")),
    }
}

impl Poller {
    /// Read what the journal pipe holds and keep every line it carried.
    pub fn read_journal(&mut self, now: Instant) -> crate::journal::Event {
        let Poller {
            journal, latest, ..
        } = self;
        journal.read(now, |line| latest.ingest(line))
    }

    /// Point the poller at the deploy's expectation file.
    ///
    /// A one-shot run knows the config's directory, so it resolves the key
    /// itself. The service passes the resolved path in its `Setup`.
    pub fn set_expected_file(&mut self, path: Option<std::path::PathBuf>) {
        self.expected_file = path;
    }

    /// Read the journal once for `--check`. `None` means the directory could
    /// not be opened, and the reason is reported against every node.
    pub fn read_journal_once(
        &mut self,
        dir: Result<std::path::PathBuf, String>,
        limit: &m6_core::limits::Limit,
    ) {
        self.journal = match dir {
            Ok(dir) => crate::journal::Reader::once(&dir, limit),
            Err(reason) => crate::journal::Reader::unavailable(reason, limit),
        };
        let Poller {
            journal, latest, ..
        } = self;
        journal.drain(|line| latest.ingest(line));
    }

    /// At start: the reading table, the lookup cache and every deadline, all
    /// sized once. The fleet is already read and resolved.
    pub fn new(setup: Setup) -> Poller {
        let Setup {
            nodes,
            timeout,
            header_checks,
            nameserver,
            wg_file,
            wg_max_age_s,
            lookup_cap,
            upstream_read,
            upstream_request,
            expected_file,
            environment,
        } = setup;
        let (expectations, expected_state) = crate::startup::load(expected_file.as_deref());
        let readings = nodes
            .iter()
            .map(|n| NodeReading {
                name: n.name.clone(),
                role: n.role.clone(),
                url: n.url.clone(),
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
            })
            .collect::<Vec<_>>();
        let n = nodes.len();
        let slots = n + tunnel::PROBES_PER_POLL + DNS_SLOTS;
        let running = Running {
            active: false,
            starting: false,
            outstanding: 0,
            step_started: vec![m6_core::platform::now(); n],
            probes: (0..n).map(|_| tunnel::Tally::new()).collect(),
            probe_cursor: 0,
            probing: None,
            probes_done: false,
            due: Vec::with_capacity(lookup_cap),
            dns_started: false,
            dns_next: 0,
            dns_active: 0,
            dns_left: 0,
            #[cfg(feature = "count-allocations")]
            alloc_start: 0,
        };
        Poller {
            nodes,
            readings,
            timeout,
            header_checks,
            latest: crate::journal::Latest::new(),
            journal: crate::journal::Reader::idle(&m6_core::limits::Limit::builtin(
                &m6_core::limits::JOURNAL_LINE,
            )),
            cache: VerdictCache::with_cap(lookup_cap),
            nameserver,
            wg_file,
            wg_max_age_s,
            upstream_read,
            upstream_request,
            expectations,
            expected_state,
            expected_file,
            environment,
            running,
            last_start: None,
            out_timeouts: Timeouts::new(slots, timeout),
            #[cfg(test)]
            probe_connect_failure: std::cell::Cell::new(None),
            #[cfg(test)]
            step_start_failure: std::cell::Cell::new(None),
        }
    }

    /// Make the next tunnel probe's connect fail at once with `kind`.
    ///
    /// A TCP connect is not synchronous on every platform: the same refused
    /// address can answer at once here and at the readiness event there. A
    /// test that must reach the tally's failure path uses this seam instead of
    /// guessing the platform.
    #[cfg(test)]
    pub fn fail_next_probe_connect(&self, kind: io::ErrorKind) {
        self.probe_connect_failure.set(Some(kind));
    }

    /// Let `starts` step starts pass, then make the next one fail at once
    /// with `kind`, as a connect that cannot even open a socket does.
    #[cfg(test)]
    pub fn fail_step_start_after(&self, starts: usize, kind: io::ErrorKind) {
        self.step_start_failure.set(Some((starts, kind)));
    }

    /// The outgoing table: one request slot per node, then the probe slots,
    /// then the lookup slots. Made at start, never grown.
    pub fn out(&self) -> Vec<Out> {
        let n = self.nodes.len();
        let mut out = Vec::with_capacity(n + tunnel::PROBES_PER_POLL + DNS_SLOTS);
        for i in 0..n {
            out.push(Out::Node {
                node: i,
                step: Step::Done,
                up: Upstream::with_limits(&self.upstream_read, &self.upstream_request),
                fd: None,
            });
        }
        for _ in 0..tunnel::PROBES_PER_POLL + DNS_SLOTS {
            out.push(Out::Free);
        }
        out
    }

    pub fn is_running(&self) -> bool {
        self.running.active
    }

    /// When the last poll started, for the interval.
    pub fn last_start(&self) -> Option<Instant> {
        self.last_start
    }

    /// The wait timeout: the nearest outgoing deadline, capped at `tick`.
    pub fn timeout(&self, now: Instant, tick: Duration) -> Duration {
        self.out_timeouts.timeout(now, tick)
    }

    /// Begin a poll: reset every reading in place and start each node's
    /// `/health` request.
    pub fn start(&mut self, poll: &Poll, out: &mut [Out], now: Instant) -> io::Result<Progress> {
        debug_assert!(!self.running.active, "a poll is already running");
        self.running.active = true;
        self.running.starting = true;
        self.running.outstanding = self.nodes.len();
        self.running.probe_cursor = 0;
        self.running.probing = None;
        self.running.probes_done = false;
        self.running.dns_started = false;
        self.running.dns_left = 0;
        self.last_start = Some(now);
        // The deploy rewrites its record around a deploy, so the monitor reads
        // it once per poll rather than once per process. A file that vanished
        // or arrived is a state the check reports, never a poll failure.
        let (expectations, expected_state) = crate::startup::load(self.expected_file.as_deref());
        self.expectations = expectations;
        self.expected_state = expected_state;
        #[cfg(feature = "count-allocations")]
        {
            self.running.alloc_start = m6_core::testkit::alloc_count::now();
        }
        for i in 0..self.nodes.len() {
            self.readings[i].reset();
            self.running.probes[i].reset();
        }
        for i in 0..self.nodes.len() {
            self.run_step(poll, out, i, Step::Health, now)?;
        }
        self.running.starting = false;
        self.advance(poll, out, now)
    }

    /// Move the slot a readiness event named one step on.
    pub fn drive(
        &mut self,
        poll: &Poll,
        out: &mut [Out],
        slot: usize,
        now: Instant,
    ) -> io::Result<Progress> {
        match &out[slot] {
            Out::Node { .. } => self.drive_node(poll, out, slot, now),
            Out::Probe { .. } => self.drive_probe(poll, out, slot, now),
            Out::Dns { .. } => self.drive_dns(poll, out, slot, now),
            Out::Free => Ok(Progress::Working),
        }
    }

    /// One outgoing deadline passed. `None` when none has.
    pub fn expire(
        &mut self,
        poll: &Poll,
        out: &mut [Out],
        now: Instant,
    ) -> io::Result<Option<Progress>> {
        let Some(Token(slot)) = self.out_timeouts.expired(now) else {
            return Ok(None);
        };
        let progress = match &out[slot] {
            Out::Node { node, step, .. } => {
                let (node, step) = (*node, *step);
                // A node that finished keeps its slot, and its last step's
                // deadline may still be in the table: it passed after the node
                // was done. There is nothing to fail.
                if step == Step::Done {
                    return Ok(Some(Progress::Working));
                }
                self.close_slot(poll, out, slot);
                let text = format!(
                    "{}: Network Error: timed out reading response",
                    self.step_url(node, step)
                );
                let next = self.record_error(node, step, text);
                self.after(node, next, poll, out, now)?
            }
            Out::Probe { .. } => {
                let Some(probing) = self.running.probing.as_ref() else {
                    return Ok(Some(Progress::Working));
                };
                let node = probing.node;
                self.out_timeouts.clear(Token(slot));
                self.close_slot(poll, out, slot);
                self.running.probes[node].lost();
                self.end_probe_sequence(poll, out, node, now)?
            }
            Out::Dns { addr, .. } => {
                let addr = addr.clone();
                self.cache.record(&addr, Ptr::Failed, now);
                self.dns_done(poll, out, slot, now)?
            }
            Out::Free => Progress::Working,
        };
        Ok(Some(progress))
    }

    // ── One node's request steps ────────────────────────────────────────────

    fn drive_node(
        &mut self,
        poll: &Poll,
        out: &mut [Out],
        slot: usize,
        now: Instant,
    ) -> io::Result<Progress> {
        let (node, step) = match &out[slot] {
            Out::Node { node, step, .. } => (*node, *step),
            _ => unreachable!("a node token names a node slot"),
        };
        // A node that has finished keeps its slot for the next poll. A second
        // readiness event for it in the same batch is stale: driving it again
        // would record the answer twice and run the node's count below zero.
        if step == Step::Done {
            return Ok(Progress::Working);
        }
        {
            let Out::Node { up, .. } = &mut out[slot] else {
                unreachable!("a node token names a node slot");
            };
            match up.drive() {
                Ok(false) => {
                    // The step changed what it waits for (write then read).
                    // Edge-triggered readiness is armed for one interest only,
                    // so re-arm it, as the other services do after a turn.
                    let interest = up.interest();
                    let fd = up.fd();
                    poll.registry()
                        .reregister_fd(fd, Token(CLIENT_BASE + slot), interest)?;
                    return Ok(Progress::Working);
                }
                Ok(true) => {
                    let mut slots = reply_slots();
                    match up.reply(&mut slots) {
                        Some(reply) => self.record_reply(node, step, &reply),
                        None => {
                            let text = format!(
                                "{}: Network Error: the response head did not parse",
                                self.step_url(node, step)
                            );
                            self.record_error(node, step, text);
                        }
                    }
                }
                Err(e) => {
                    let text = format!("{}: Network Error: {e}", self.step_url(node, step));
                    self.record_error(node, step, text);
                }
            }
        }
        let next = self.next_step(node, step);
        self.after(node, next, poll, out, now)
    }

    /// Start one step for one node, or record its transport failure.
    fn run_step(
        &mut self,
        poll: &Poll,
        out: &mut [Out],
        node: usize,
        step: Step,
        now: Instant,
    ) -> io::Result<Progress> {
        let mut step = step;
        loop {
            match self.begin_step(poll, out, node, step, now) {
                Ok(()) => return Ok(Progress::Working),
                Err(e) => {
                    let text = format!("{}: Network Error: {e}", self.step_url(node, step));
                    let next = self.record_error(node, step, text);
                    if next == Step::Done {
                        // The node finished on a step that failed at start.
                        // `after` is the one place that finishes a node, so a
                        // deadline an earlier step armed cannot fire on it.
                        return self.after(node, next, poll, out, now);
                    }
                    step = next;
                }
            }
        }
    }

    /// Fill both journal documents for one node, once its health answered.
    ///
    /// A node with no line says so instead of showing nothing: a monitor that
    /// quietly reports a healthy quiet node when the journal is empty is the
    /// failure this exists to avoid.
    fn record_journal(&mut self, node: usize) {
        let name = &self.nodes[node].name;
        let stopped = self.journal.stopped.as_deref();
        let r = &mut self.readings[node];
        match journal_document(self.latest.perf(name), stopped, "metrics") {
            Ok(report) => r.perf = Some(report),
            Err(e) => r.perf_error = Some(e),
        }
        match journal_document(self.latest.traffic(name), stopped, "traffic") {
            Ok(report) => r.traffic = Some(report),
            Err(e) => r.traffic_error = Some(e),
        }
    }

    /// What follows `step` for this node, recorded or not.
    fn next_step(&mut self, node: usize, step: Step) -> Step {
        match step {
            Step::Health => {
                if self.readings[node].unreachable.is_some() {
                    Step::Done
                } else {
                    // The two documents come from the journal, not from a
                    // request.
                    self.record_journal(node);
                    self.next_header(node, 0)
                }
            }
            Step::Header(i) => self.next_header(node, i + 1),
            Step::Done => Step::Done,
        }
    }

    /// The next header check that applies to this node, from `from` on.
    fn next_header(&self, node: usize, from: usize) -> Step {
        // The public face is one thing per node, so only process 0 asks it.
        if self.nodes[node].process.is_some_and(|p| p != 0) {
            return Step::Done;
        }
        let role = &self.readings[node].role;
        for i in from..self.header_checks.len() {
            let hc = &self.header_checks[i];
            if hc.roles.is_empty() || hc.roles.iter().any(|r| r == role) {
                return Step::Header(i);
            }
        }
        Step::Done
    }

    /// After a step is recorded: start the next one, or finish the node.
    fn after(
        &mut self,
        node: usize,
        next: Step,
        poll: &Poll,
        out: &mut [Out],
        now: Instant,
    ) -> io::Result<Progress> {
        if next == Step::Done {
            self.close_slot(poll, out, node);
            self.out_timeouts.clear(Token(node));
            if let Out::Node { step, .. } = &mut out[node] {
                *step = Step::Done;
            }
            self.running.outstanding -= 1;
            if self.running.outstanding == 0 {
                self.advance(poll, out, now)
            } else {
                Ok(Progress::Working)
            }
        } else {
            self.run_step(poll, out, node, next, now)
        }
    }

    /// The address, request path and authority of one step.
    fn step_target(&self, node: usize, step: Step) -> (Addr, String, String) {
        let n = &self.nodes[node];
        let path = match step {
            Step::Health => format!("{}/health", n.base_path),
            Step::Header(i) => {
                let hc = &self.header_checks[i];
                let sep = if hc.path.contains('?') { '&' } else { '?' };
                let path = if hc.cache_bust {
                    format!(
                        "{}{}{}hc={}",
                        n.base_path,
                        hc.path,
                        sep,
                        m6_core::platform::wall()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_nanos())
                            .unwrap_or(0)
                    )
                } else {
                    format!("{}{}", n.base_path, hc.path)
                };
                path
            }
            Step::Done => unreachable!("a done step starts no request"),
        };
        let addr = match (step, n.status_addr) {
            // A header check asks the public face, whatever the process.
            (Step::Header(_), _) => Addr::Tcp(n.addr),
            (_, Some(status)) => Addr::Tcp(status),
            _ => Addr::Tcp(n.addr),
        };
        (addr, path, n.authority.clone())
    }

    /// The URL a step is asked for, for an error line. The cache-bust value of
    /// a header check is its own, so the text names the path and the node.
    fn step_url(&self, node: usize, step: Step) -> String {
        let n = &self.nodes[node];
        let path = match step {
            Step::Health => "/health".to_string(),
            Step::Header(i) => self.header_checks[i].path.clone(),
            Step::Done => String::new(),
        };
        format!("{}{}", n.url.trim_end_matches('/'), path)
    }

    /// Write one step's request into the node's slot and start it.
    fn begin_step(
        &mut self,
        poll: &Poll,
        out: &mut [Out],
        node: usize,
        step: Step,
        now: Instant,
    ) -> io::Result<()> {
        let (addr, path, authority) = self.step_target(node, step);
        let tls = self.nodes[node].tls.clone();
        self.close_slot(poll, out, node);
        let Out::Node {
            up,
            step: slot_step,
            ..
        } = &mut out[node]
        else {
            unreachable!("a node slot holds its node's Upstream");
        };
        *slot_step = step;
        write_request(up, &authority, &path);
        #[cfg(test)]
        if let Some((starts, kind)) = self.step_start_failure.get() {
            if starts == 0 {
                self.step_start_failure.set(None);
                return Err(io::Error::from(kind));
            }
            self.step_start_failure.set(Some((starts - 1, kind)));
        }
        up.start(&addr, tls.as_ref(), false, now + self.timeout)?;
        let fd = up.fd();
        let interest = up.interest();
        poll.registry()
            .register_fd(fd, Token(CLIENT_BASE + node), interest)?;
        if let Out::Node { fd: slot_fd, .. } = &mut out[node] {
            *slot_fd = Some(fd);
        }
        self.running.step_started[node] = now;
        self.out_timeouts.touch(Token(node), now);
        Ok(())
    }

    /// Deregister and forget whatever the slot was waiting on.
    fn close_slot(&mut self, poll: &Poll, out: &mut [Out], slot: usize) {
        let fd = match &mut out[slot] {
            Out::Node { fd, .. } | Out::Probe { fd, .. } => fd.take(),
            Out::Dns { fd, .. } => Some(*fd),
            Out::Free => None,
        };
        if let Some(fd) = fd {
            let _ = poll.registry().deregister_fd(fd);
        }
        match &mut out[slot] {
            Out::Node { .. } => {}
            _ => out[slot] = Out::Free,
        }
    }

    fn record_reply(&mut self, node: usize, step: Step, reply: &Reply<'_>) {
        let r = &mut self.readings[node];
        match step {
            Step::Health => {
                r.rtt = Some(self.running.step_started[node].elapsed());
                r.health_status = Some(reply.status);
                match serde_json::from_slice::<HealthReport>(reply.body) {
                    Ok(h) => {
                        // A status port answers as the process it was bound
                        // for: another index means two processes were given
                        // one index, or the config names the wrong base.
                        if let Some(expected) = self.nodes[node].process {
                            if h.process_index != Some(expected) {
                                r.unreachable = Some(format!(
                                    "the status port of process {expected} answered as process {}",
                                    h.process_index
                                        .map_or_else(|| "none".to_string(), |i| i.to_string())
                                ));
                            }
                        }
                        r.health = Some(h);
                    }
                    Err(e) => {
                        r.unreachable = Some(format!(
                            "/health answered {} but did not parse: {e}",
                            reply.status
                        ))
                    }
                }
            }
            Step::Header(i) => {
                let hc = &self.header_checks[i];
                let actual = reply
                    .headers
                    .iter()
                    .find(|h| h.name.eq_ignore_ascii_case(&hc.header))
                    .and_then(|h| std::str::from_utf8(h.value).ok())
                    .map(|v| v.trim().to_string());
                r.header_checks.push(HeaderCheckResult {
                    path: hc.path.clone(),
                    header: hc.header.clone(),
                    expected: hc.expect.clone(),
                    actual,
                    status: Some(reply.status),
                    error: None,
                });
            }
            Step::Done => {}
        }
    }

    /// Record a transport failure on one step and answer what follows it.
    fn record_error(&mut self, node: usize, step: Step, text: String) -> Step {
        let r = &mut self.readings[node];
        match step {
            Step::Health => r.unreachable = Some(text),
            Step::Header(i) => {
                let hc = &self.header_checks[i];
                r.header_checks.push(HeaderCheckResult {
                    path: hc.path.clone(),
                    header: hc.header.clone(),
                    expected: hc.expect.clone(),
                    actual: None,
                    status: None,
                    error: Some(text),
                });
            }
            Step::Done => {}
        }
        self.next_step(node, step)
    }

    // ── Tunnel probes ──────────────────────────────────────────────────────

    /// Start one non-blocking connect for the node's probe sequence.
    ///
    /// `Ok(false)` means the connect or its registration failed at once: the
    /// failure is in the node's tally and the caller ends the sequence. A
    /// down interface is a reading, never the end of the service.
    fn start_probe(
        &mut self,
        poll: &Poll,
        out: &mut [Out],
        node: usize,
        n: usize,
        now: Instant,
    ) -> io::Result<bool> {
        let Some(addr) = self.nodes[node].tunnel.as_ref().map(|t| t.addr) else {
            return Ok(false);
        };
        let slot = self.probe_slot();
        self.close_slot(poll, out, slot);
        // The connect's own result. A test can stand in for it with its one-shot
        // seam, so the match below is still the one place a failed connect is
        // handled, in the test build as in the release one.
        #[cfg(test)]
        let connect = match self.probe_connect_failure.take() {
            Some(kind) => Err(io::Error::from(kind)),
            None => MioTcpStream::connect(addr),
        };
        #[cfg(not(test))]
        let connect = MioTcpStream::connect(addr);
        let sock = match connect {
            Ok(sock) => sock,
            Err(e) => {
                self.running.probes[node].failed(&e);
                return Ok(false);
            }
        };
        let fd = sock.as_raw_fd();
        if let Err(e) =
            poll.registry()
                .register_fd(fd, Token(CLIENT_BASE + slot), Interest::WRITABLE)
        {
            self.running.probes[node].failed(&e);
            return Ok(false);
        }
        let started = now;
        out[slot] = Out::Probe {
            node,
            n,
            started,
            sock: Some(sock),
            fd: Some(fd),
        };
        // One deadline covers the whole sequence, so a connect that gets
        // through leaves less of the budget for the next one.
        if n == 0 {
            self.out_timeouts.touch(Token(slot), now);
        }
        Ok(true)
    }

    fn probe_slot(&self) -> usize {
        self.nodes.len()
    }

    fn drive_probe(
        &mut self,
        poll: &Poll,
        out: &mut [Out],
        slot: usize,
        now: Instant,
    ) -> io::Result<Progress> {
        let (node, n) = match &out[slot] {
            Out::Probe { node, n, .. } => (*node, *n),
            _ => unreachable!("a probe token names a probe slot"),
        };
        let sock = match &mut out[slot] {
            Out::Probe {
                sock, fd, started, ..
            } => {
                if let Some(fd) = fd.take() {
                    let _ = poll.registry().deregister_fd(fd);
                }
                let started = *started;
                sock.take().map(|s| (s, started))
            }
            _ => None,
        };
        let Some((sock, started)) = sock else {
            return Ok(Progress::Working);
        };
        let rtt = started.elapsed();
        match sock.take_error() {
            Ok(Some(e)) if e.kind() == io::ErrorKind::ConnectionRefused => {
                self.running.probes[node].refused(rtt)
            }
            Ok(Some(e)) => self.running.probes[node].failed(&e),
            Ok(None) => self.running.probes[node].connected(rtt),
            Err(e) => self.running.probes[node].failed(&e),
        }
        // The stream drops here, closing the socket; nothing is sent on it.
        drop(sock);
        out[slot] = Out::Free;
        self.after_probe(poll, out, node, n, now)
    }

    fn after_probe(
        &mut self,
        poll: &Poll,
        out: &mut [Out],
        node: usize,
        n: usize,
        now: Instant,
    ) -> io::Result<Progress> {
        let Some(probing) = self.running.probing.as_ref() else {
            return Ok(Progress::Working);
        };
        let budget_left = (probing.started + self.timeout).saturating_duration_since(now);
        let next = n + 1;
        if next < tunnel::PROBES_PER_POLL && !budget_left.is_zero() {
            if !self.start_probe(poll, out, node, next, now)? {
                return self.end_probe_sequence(poll, out, node, now);
            }
            return Ok(Progress::Working);
        }
        self.end_probe_sequence(poll, out, node, now)
    }

    /// End the node's probe sequence and move on: the connects that can be
    /// made are, so the reading is built from what the tally holds.
    fn end_probe_sequence(
        &mut self,
        poll: &Poll,
        out: &mut [Out],
        node: usize,
        now: Instant,
    ) -> io::Result<Progress> {
        self.running.probing = None;
        self.running.probe_cursor = node + 1;
        self.out_timeouts.clear(Token(self.probe_slot()));
        self.advance(poll, out, now)
    }

    // ── Crawler lookups ────────────────────────────────────────────────────

    fn dns_slot(&self, i: usize) -> usize {
        self.nodes.len() + tunnel::PROBES_PER_POLL + i
    }

    /// Fill every free lookup slot from the due list.
    fn start_lookups(&mut self, poll: &Poll, out: &mut [Out], now: Instant) -> io::Result<()> {
        while self.running.dns_active < DNS_SLOTS && self.running.dns_next < self.running.due.len()
        {
            let Some(i) = (0..DNS_SLOTS).find(|&i| matches!(out[self.dns_slot(i)], Out::Free))
            else {
                return Ok(());
            };
            let addr = self.running.due[self.running.dns_next].clone();
            self.running.dns_next += 1;
            let started = addr
                .parse::<std::net::IpAddr>()
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
                .and_then(|ip| Confirm::start(ip, self.nameserver));
            match started {
                Ok(confirm) => {
                    let slot = self.dns_slot(i);
                    let fd = confirm.fd();
                    poll.registry().register_fd(
                        fd,
                        Token(CLIENT_BASE + slot),
                        Interest::READABLE,
                    )?;
                    self.out_timeouts.touch(Token(slot), now);
                    out[slot] = Out::Dns { addr, confirm, fd };
                    self.running.dns_active += 1;
                }
                Err(_) => {
                    self.cache.record(&addr, Ptr::Failed, now);
                    self.running.dns_left -= 1;
                }
            }
        }
        Ok(())
    }

    fn drive_dns(
        &mut self,
        poll: &Poll,
        out: &mut [Out],
        slot: usize,
        now: Instant,
    ) -> io::Result<Progress> {
        let (addr, result, before, after) = {
            let Out::Dns { addr, confirm, fd } = &mut out[slot] else {
                unreachable!("a lookup token names a lookup slot");
            };
            let before = *fd;
            let result = confirm.drive();
            let after = confirm.fd();
            (addr.clone(), result, before, after)
        };
        match result {
            Ok(None) => {
                // The PTR step ended, so the descriptor to wait on is the
                // forward query's: the old socket is closed, and closing a
                // descriptor removes its registration. Register the new one
                // under the same token. This is an add, never a modify: on
                // epoll a re-register is EPOLL_CTL_MOD and fails for a
                // descriptor the set does not hold.
                if before != after {
                    poll.registry().register_fd(
                        after,
                        Token(CLIENT_BASE + slot),
                        Interest::READABLE,
                    )?;
                    if let Out::Dns { fd, .. } = &mut out[slot] {
                        *fd = after;
                    }
                }
                Ok(Progress::Working)
            }
            Ok(Some(ptr)) => {
                self.cache.record(&addr, ptr, now);
                self.dns_done(poll, out, slot, now)
            }
            Err(_) => {
                self.cache.record(&addr, Ptr::Failed, now);
                self.dns_done(poll, out, slot, now)
            }
        }
    }

    fn dns_done(
        &mut self,
        poll: &Poll,
        out: &mut [Out],
        slot: usize,
        now: Instant,
    ) -> io::Result<Progress> {
        self.out_timeouts.clear(Token(slot));
        self.close_slot(poll, out, slot);
        self.running.dns_active -= 1;
        self.running.dns_left -= 1;
        if self.running.dns_left == 0 {
            return self.finish(now);
        }
        self.start_lookups(poll, out, now)?;
        if self.running.dns_left == 0 {
            self.finish(now)
        } else {
            Ok(Progress::Working)
        }
    }

    // ── Stages ─────────────────────────────────────────────────────────────

    /// Move on from whichever stage just finished.
    fn advance(&mut self, poll: &Poll, out: &mut [Out], now: Instant) -> io::Result<Progress> {
        if self.running.starting || self.running.outstanding > 0 {
            return Ok(Progress::Working);
        }
        if !self.running.probes_done {
            let cursor = self.running.probe_cursor;
            match (cursor..self.nodes.len()).find(|&i| self.nodes[i].tunnel.is_some()) {
                None => self.running.probes_done = true,
                Some(node) => {
                    self.running.probe_cursor = node;
                    self.running.probing = Some(Probing { node, started: now });
                    self.running.probes[node].reset();
                    if !self.start_probe(poll, out, node, 0, now)? {
                        return self.end_probe_sequence(poll, out, node, now);
                    }
                    return Ok(Progress::Working);
                }
            }
        }
        if !self.running.dns_started {
            self.running.dns_started = true;
            self.cache.due(&self.readings, now, &mut self.running.due);
            self.running.dns_left = self.running.due.len();
            self.running.dns_next = 0;
            self.running.dns_active = 0;
            if self.running.dns_left == 0 {
                return self.finish(now);
            }
            self.start_lookups(poll, out, now)?;
            if self.running.dns_left == 0 {
                return self.finish(now);
            }
            return Ok(Progress::Working);
        }
        if self.running.dns_left > 0 {
            return Ok(Progress::Working);
        }
        self.finish(now)
    }

    /// The poll is complete: the tunnel readings, then the result.
    fn finish(&mut self, now: Instant) -> io::Result<Progress> {
        let src = tunnel::load(self.wg_file.as_deref(), self.wg_max_age_s);
        let file_age_s = match &src {
            Ok((_, age)) => *age,
            Err(tunnel::Handshake::FileStale { age_s, .. }) => Some(*age_s),
            Err(_) => None,
        };
        let now_unix = m6_core::platform::wall()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        for i in 0..self.nodes.len() {
            let Some(t) = &self.nodes[i].tunnel else {
                continue;
            };
            let probe = self.running.probes[i].result(self.timeout);
            self.readings[i].tunnel = Some(TunnelReading {
                address: t.address.clone(),
                key_prefix: tunnel::key_prefix(t.public_key.as_deref()),
                probe,
                handshake: tunnel::handshake_for(&src, t.public_key.as_deref(), now_unix),
                file_age_s,
            });
        }
        if let Some(start) = self.last_start {
            // The one number an operator watches on a monitor: how long the
            // fleet took to answer. Debug, because a poll is once per interval.
            m6_core::log::debug!(
                poll_ms = now.duration_since(start).as_millis() as u64,
                "fleet poll completed"
            );
        }
        // The poll is where this service allocates: parsing reports and
        // building the digest, the page and the history batch. Counted under
        // the test feature, so the number is measured rather than assumed.
        #[cfg(feature = "count-allocations")]
        m6_core::log::debug!(
            poll_allocs = m6_core::testkit::alloc_count::now() - self.running.alloc_start,
            "fleet poll allocations"
        );
        let verdicts = self.cache.verdicts(&self.readings);
        let startup = crate::startup::Facts {
            runs: self.latest.startup.views(now),
            expectations: self.expectations.clone(),
            expected_state: self.expected_state.clone(),
            expected_path: self
                .expected_file
                .as_ref()
                .map(|path| path.display().to_string()),
            environment: self.environment.clone(),
            cut: self.latest.startup.cut,
            orphan_shutdowns: self.latest.startup.orphan_shutdowns,
            journal_unavailable: self.journal.stopped.clone(),
        };
        self.running.active = false;
        Ok(Progress::Done(Polled {
            readings: self.readings.clone(),
            verdicts,
            at: now,
            startup,
        }))
    }
}

/// The poller a one-shot run builds: the fleet resolved, its timeout, header
/// checks and lookup cap. `--check` and the poll's own tests
/// both use it, so the two cannot drift.
pub fn one_shot_poller(
    fleet: &crate::fleet::Fleet,
    tls: &std::sync::Arc<rustls::ClientConfig>,
    nameserver: SocketAddr,
    lookup_cap: usize,
) -> anyhow::Result<Poller> {
    let nodes = fleet.resolve(tls)?;
    Ok(Poller::new(Setup {
        nodes,
        timeout: Duration::from_millis(fleet.timeout_ms),
        header_checks: fleet.header_check.clone(),
        nameserver,
        wg_file: fleet.wg_handshakes_file.clone(),
        wg_max_age_s: fleet.tunnel_handshake_max_age_s,
        lookup_cap,
        upstream_read: m6_core::limits::Limit::builtin(&m6_core::limits::UPSTREAM_READ),
        upstream_request: m6_core::limits::Limit::builtin(&m6_core::limits::UPSTREAM_REQUEST),
        expected_file: None,
        environment: fleet.environment.clone(),
    }))
}

/// Drive a poller from its first request to the end of its poll, with a
/// reactor of its own.
///
/// A service's own loop makes the same `start`, `drive` and `expire` calls
/// while it serves clients. A one-shot run has no clients, so this is the
/// loop it needs.
pub fn drive_to_end(
    poller: &mut Poller,
    out: &mut [Out],
    deadline: Option<Instant>,
) -> io::Result<Polled> {
    const TICK: Duration = Duration::from_secs(1);
    let mut poll = Poll::new()?;
    let mut events = Events::with_capacity(64);
    let mut progress = poller.start(&poll, out, m6_core::platform::now())?;
    loop {
        if let Progress::Done(polled) = progress {
            return Ok(polled);
        }
        let now = m6_core::platform::now();
        if deadline.is_some_and(|d| now >= d) {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "the poll did not finish",
            ));
        }
        match poll.poll(&mut events, Some(poller.timeout(now, TICK))) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
        let mut finished = false;
        for ev in &events {
            let Some(slot) = ev.token().0.checked_sub(CLIENT_BASE) else {
                continue;
            };
            progress = poller.drive(&poll, out, slot, m6_core::platform::now())?;
            if matches!(progress, Progress::Done(_)) {
                finished = true;
                break;
            }
        }
        if finished {
            continue;
        }
        while let Some(next) = poller.expire(&poll, out, m6_core::platform::now())? {
            progress = next;
            if matches!(progress, Progress::Done(_)) {
                break;
            }
        }
    }
}

/// Write one plain HTTP/1.1 request into the slot's own buffer.
///
/// `Connection: close` because the reply's framing is the server's choice and
/// the slot waits for the connection to end when the body has no
/// `Content-Length`. No `Accept-Encoding` is sent, so the answer is the bytes
/// the node would serve a client that takes the identity encoding: this client
/// decodes nothing.
fn write_request(up: &mut Upstream, authority: &str, path: &str) {
    let w = up.request_buf();
    let _ = write!(
        w,
        "GET {path} HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\n"
    );
    w.extend_from_slice(b"\r\n");
}

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "test code: threads, sleeps and blocking clients drive the service from outside"
)]
mod tests {
    use super::*;
    use crate::fleet::Node;

    fn reading(unreachable: Option<&str>, status: Option<&str>) -> NodeReading {
        NodeReading {
            name: "origin".into(),
            role: "origin".into(),
            url: "http://origin.example.com".into(),
            health: status.map(|s| HealthReport {
                process_index: None,
                status: s.to_string(),
                node: "origin".into(),
            }),
            health_status: status.map(|_| 200),
            perf: None,
            perf_error: None,
            traffic: None,
            traffic_error: None,
            header_checks: Vec::new(),
            rtt: None,
            unreachable: unreachable.map(|s| s.to_string()),
            tunnel: None,
        }
    }

    /// A node that cannot be reached is not healthy and is not degraded.
    /// Collapsing "no answer" into either one is how a dead node gets reported
    /// as fine, or a working fleet gets reported as broken.
    #[test]
    fn unreachable_is_its_own_state() {
        assert_eq!(
            reading(Some("connection refused"), None).status(),
            "unreachable"
        );
        assert!(!reading(Some("timeout"), None).is_up());
        assert_eq!(reading(None, Some("ok")).status(), "ok");
        assert_eq!(reading(None, Some("degraded")).status(), "degraded");
        assert_eq!(reading(None, None).status(), "unknown");
    }

    /// "ok" needs the 200 as well as the body. A 503 that says "ok" is a node
    /// that is not serving, and a 200 with any other status is not healthy.
    #[test]
    fn ok_needs_both_the_200_and_the_body() {
        let mut r = reading(None, Some("ok"));
        r.health_status = Some(503);
        assert_eq!(r.status(), "degraded");
        r.health_status = Some(500);
        assert_eq!(r.status(), "degraded");
        r.health_status = Some(200);
        assert_eq!(r.status(), "ok");
        let r = reading(None, Some("starting"));
        assert_eq!(r.status(), "starting");
    }

    // ── The poll engine, driven against loopback listeners ────────────────

    /// One shared TLS config for http-only nodes. Nothing in these tests
    /// connects with it.
    fn tls() -> std::sync::Arc<rustls::ClientConfig> {
        let provider = std::sync::Arc::new(rustls::crypto::aws_lc_rs::default_provider());
        std::sync::Arc::new(
            rustls::ClientConfig::builder_with_provider(provider)
                .with_safe_default_protocol_versions()
                .expect("protocol versions")
                .with_root_certificates(rustls::RootCertStore::empty())
                .with_no_client_auth(),
        )
    }

    fn fleet_of(node: Node) -> crate::fleet::Fleet {
        crate::fleet::Fleet {
            nodes: vec![node],
            poll_interval_s: crate::DEFAULT_POLL_SECS,
            history_file: None,
            history_max_mb: None,
            journal_dir: "/var/log/journal/remote".to_string(),
            timeout_ms: 5_000,
            one_certificate: false,
            header_check: Vec::new(),
            wg_handshakes_file: None,
            tunnel_handshake_max_age_s: 600,
            health_last_ok_max_age_s: None,
            environment: None,
            expected_deploy_file: None,
        }
    }

    /// Run the poll to completion with its own reactor, as `--check` does.
    fn run(fleet: &crate::fleet::Fleet, token: Option<&str>) -> Polled {
        run_with(fleet, token, "127.0.0.1:53".parse().unwrap())
    }

    /// Run the poll as `--check` does, with a chosen nameserver so the lookup
    /// path can be scripted.
    fn run_with(
        fleet: &crate::fleet::Fleet,
        token: Option<&str>,
        nameserver: std::net::SocketAddr,
    ) -> Polled {
        run_with_journal(fleet, token, nameserver, &[])
    }

    /// The same, with whole journal lines fed to the poller as the journal
    /// would.
    fn run_with_journal(
        fleet: &crate::fleet::Fleet,
        token: Option<&str>,
        nameserver: std::net::SocketAddr,
        lines: &[serde_json::Value],
    ) -> Polled {
        let tls = tls();
        let fleet = fleet.clone();
        let mut poller =
            one_shot_poller(&fleet, &tls, nameserver, crate::check::MAX_LOOKUPS_PER_POLL).unwrap();
        for line in lines {
            poller.latest.ingest(&journal_line(line.clone()));
        }
        let mut out = poller.out();
        let polled = drive_to_end(
            &mut poller,
            &mut out,
            Some(m6_core::platform::now() + Duration::from_secs(20)),
        )
        .unwrap();
        let _ = token;
        polled
    }

    /// A one-connection-per-request HTTP/1.1 server on a loopback port. It
    /// answers `n` requests with `reply(path)` and returns what it saw as
    /// (request line, authorization header or empty).
    fn serve(
        n: usize,
        reply: fn(&str) -> (u16, &'static str),
    ) -> (u16, std::thread::JoinHandle<Vec<(String, String)>>) {
        use std::io::{BufRead, BufReader, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = std::thread::spawn(move || {
            let mut seen = Vec::new();
            for stream in listener.incoming().take(n) {
                let mut stream = stream.unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let mut auth = String::new();
                loop {
                    let mut h = String::new();
                    reader.read_line(&mut h).unwrap();
                    if h == "\r\n" || h.is_empty() {
                        break;
                    }
                    if h.to_ascii_lowercase().starts_with("authorization:") {
                        auth = h["authorization:".len()..].trim().to_string();
                    }
                }
                let path = line.split_whitespace().nth(1).unwrap_or("").to_string();
                let (code, body) = reply(&path);
                write!(
                    stream,
                    "HTTP/1.1 {code} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .unwrap();
                seen.push((line.trim().to_string(), auth));
            }
            seen
        });
        (port, handle)
    }

    fn process_node(status_port: u16, index: u32) -> Node {
        Node {
            name: format!("edge/{index}"),
            // Nothing listens here: a request that went to the public face
            // instead of the status port would read the node as unreachable.
            url: "http://127.0.0.1:1".into(),
            role: "cache".into(),
            tunnel: Some(crate::fleet::Tunnel {
                address: format!("127.0.0.1:{status_port}"),
                public_key: None,
            }),
            processes: None,
            process: Some(index),
        }
    }

    /// A process entry asks its status port for `/health`, and nothing else:
    /// no header check, no public address. The two documents come from the
    /// journal and ask it nothing. Break: `/health` sent to the public address,
    /// which reads unreachable here.
    #[test]
    fn a_process_entry_asks_its_status_port_for_health() {
        let (port, seen) = serve(1, |path| match path {
            "/health" => (200, r#"{"status":"ok","node":"edge","process_index":1}"#),
            _ => (404, r#"{"error":"not found"}"#),
        });
        let polled = run(&fleet_of(process_node(port, 1)), None);
        let r = &polled.readings[0];
        assert_eq!(r.unreachable, None);
        assert_eq!(r.health.as_ref().map(|h| h.status.as_str()), Some("ok"));
        let lines: Vec<String> = seen.join().unwrap().into_iter().map(|(l, _)| l).collect();
        assert_eq!(lines, ["GET /health HTTP/1.1"]);
    }

    /// A status port that answers as another process is a fault that names
    /// both. Break: the index is not compared, and two processes given one
    /// index read as healthy.
    #[test]
    fn a_status_port_that_answers_as_another_process_is_a_fault() {
        let (port, seen) = serve(1, |path| match path {
            "/health" => (200, r#"{"status":"ok","node":"edge","process_index":0}"#),
            _ => (404, r#"{"error":"not found"}"#),
        });
        let polled = run(&fleet_of(process_node(port, 2)), Some("tok"));
        seen.join().unwrap();
        let why = polled.readings[0]
            .unreachable
            .as_deref()
            .unwrap_or_default();
        assert!(
            why.contains("process 2") && why.contains("answered as process 0"),
            "{why}"
        );
        assert_eq!(polled.readings[0].status(), "unreachable");
    }

    /// A process that is down refuses its own port, and is reported as that
    /// index. This is what the shared port hides.
    /// A traffic document wrapped as the line the journal carries it in.
    fn traffic_line(document: &str) -> serde_json::Value {
        serde_json::json!({
            "type": "traffic",
            "fields": {"traffic": serde_json::from_str::<serde_json::Value>(document)
                .expect("a test document")},
        })
    }

    /// One line as `journalctl --output=json` prints it: the line's own JSON in
    /// `MESSAGE`.
    fn journal_line(document: serde_json::Value) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "MESSAGE": document.to_string(),
            "M6_TYPE": document["type"],
            "_HOSTNAME": "build",
        }))
        .unwrap()
    }

    fn perf_document(node: &str, uptime_s: u64) -> serde_json::Value {
        serde_json::json!({
            "type": "perf",
            "fields": {"perf": {"node": node, "uptime_s": uptime_s,
                "build": {"name": "m6-http", "version": "1.0.0", "hash": "h"},
                "host": {"cpus": 1, "load": {"one": 0.0, "five": 0.0, "fifteen": 0.0, "running": 0, "total": 0},
                    "memory": {"available_bytes": 1, "total_bytes": 2, "source": "Host"},
                    "disk": {"available_bytes": 1, "total_bytes": 2}, "disks": [], "fds": {"open": 1, "soft_limit": 2, "hard_limit": 3},
                    "net": [], "tcp": null, "thermal": [], "cpu_pressure": null, "io_pressure": null, "memory_pressure": null,
                    "uptime_s": 1},
                "metrics": {"requests_total": 0, "cache_hits_total": 0, "cache_misses_total": 0,
                    "cache_hits_valid_total": 0, "cache_hits_invalid_total": 0,
                    "cache_misses_fetched_total": 0, "cache_misses_answered_locally_total": 0,
                    "backend_errors_total": 0, "backend_errors_by_name": {}, "monitor_requests_total": 0,
                    "rps_peak": 0, "hit_samples": 0, "hit_p0_ns": 0, "hit_p50_ns": 0, "hit_p99_ns": 0, "hit_max_ns": 0,
                    "miss_samples": 0, "miss_p0_ns": 0, "miss_p50_ns": 0, "miss_p99_ns": 0, "miss_max_ns": 0,
                    "monitor_samples": 0, "monitor_p50_ns": 0, "monitor_p99_ns": 0,
                    "status_counts": {}, "channels": [], "event_loop": []},
                "pools": [], "url_backends": [], "tls": [], "tls_unreadable": 0, "logging": null}},
        })
    }

    fn traffic_document(node: &str) -> serde_json::Value {
        serde_json::json!({
            "type": "traffic",
            "fields": {"traffic": {"node": node, "window_minutes": 60, "events_dropped": 0,
                "total_requests": 0, "status": {}, "notable": [], "heavy_hitters": [],
                "crawlers": [], "forged_bot_requests": 0, "forgers": [], "probe_noise": [],
                "logging": {"events_total": 10, "seconds_since_last": 1}, "firewall": null}},
        })
    }

    /// The two documents come from the journal, and the only request a node
    /// sees is `/health`: no monitor poll asks it for metrics.
    #[test]
    fn the_documents_come_from_the_journal_and_health_from_the_url() {
        let (public, public_seen) = serve(1, |_| (200, r#"{"status":"ok","node":"origin"}"#));
        let n = Node {
            name: "origin".into(),
            url: format!("http://127.0.0.1:{public}"),
            role: "origin".into(),
            tunnel: None,
            processes: None,
            process: None,
        };
        let fleet = fleet_of(n);
        let mut poller = one_shot_poller(
            &fleet,
            &tls(),
            "127.0.0.1:53".parse().unwrap(),
            crate::check::MAX_LOOKUPS_PER_POLL,
        )
        .unwrap();
        poller
            .latest
            .ingest(&journal_line(perf_document("origin", 5)));
        poller
            .latest
            .ingest(&journal_line(traffic_document("origin")));
        let mut out = poller.out();
        let polled = drive_to_end(
            &mut poller,
            &mut out,
            Some(m6_core::platform::now() + Duration::from_secs(20)),
        )
        .unwrap();
        let r = &polled.readings[0];
        assert_eq!(r.perf.as_ref().map(|p| p.uptime_s), Some(5), "{r:?}");
        assert_eq!(
            r.traffic.as_ref().map(|t| t.window_minutes),
            Some(60),
            "{r:?}"
        );
        assert_eq!(r.perf_error, None);
        let seen = public_seen.join().unwrap();
        assert_eq!(seen.len(), 1, "one request: {seen:?}");
        assert_eq!(seen[0].0, "GET /health HTTP/1.1");
    }

    /// A node the journal has no line for says so, and the reason names the
    /// journal rather than leaving the figures blank.
    #[test]
    fn a_node_with_no_journal_line_says_so() {
        let (public, _seen) = serve(1, |_| (200, r#"{"status":"ok","node":"origin"}"#));
        let n = Node {
            name: "origin".into(),
            url: format!("http://127.0.0.1:{public}"),
            role: "origin".into(),
            tunnel: None,
            processes: None,
            process: None,
        };
        let polled = run(&fleet_of(n), None);
        let r = &polled.readings[0];
        assert_eq!(
            r.perf_error.as_deref(),
            Some("no metrics line in the journal for this node")
        );
        assert_eq!(
            r.traffic_error.as_deref(),
            Some("no traffic line in the journal for this node")
        );
    }

    /// A reader that is not running is named, not reported as a node with no
    /// figures.
    #[test]
    fn a_stopped_reader_is_named() {
        let (public, _seen) = serve(1, |_| (200, r#"{"status":"ok","node":"origin"}"#));
        let n = Node {
            name: "origin".into(),
            url: format!("http://127.0.0.1:{public}"),
            role: "origin".into(),
            tunnel: None,
            processes: None,
            process: None,
        };
        let fleet = fleet_of(n);
        let mut poller = one_shot_poller(
            &fleet,
            &tls(),
            "127.0.0.1:53".parse().unwrap(),
            crate::check::MAX_LOOKUPS_PER_POLL,
        )
        .unwrap();
        poller.journal.stopped = Some("journalctl could not start".to_string());
        let mut out = poller.out();
        let polled = drive_to_end(
            &mut poller,
            &mut out,
            Some(m6_core::platform::now() + Duration::from_secs(20)),
        )
        .unwrap();
        let r = &polled.readings[0];
        assert!(
            r.perf_error
                .as_deref()
                .is_some_and(|e| e.contains("the journal reader is not running")),
            "{r:?}"
        );
        assert!(
            r.perf_error
                .as_deref()
                .is_some_and(|e| e.contains("journalctl could not start")),
            "{r:?}"
        );
    }

    #[test]
    fn a_process_that_is_down_is_unreachable_under_its_own_name() {
        let port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let polled = run(&fleet_of(process_node(port, 1)), Some("tok"));
        assert_eq!(polled.readings[0].name, "edge/1");
        assert!(polled.readings[0].unreachable.is_some());
    }

    /// A hung node's step fails at the deadline and reads unreachable, with the
    /// error text a blocking client gave.
    #[test]
    fn a_hung_node_reads_unreachable_at_its_deadline() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let held = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            // Hold the request open until the poll has failed it, never for a
            // guessed time.
            let _ = release_rx.recv();
            drop(stream);
        });
        let n = Node {
            name: "origin".into(),
            url: format!("http://127.0.0.1:{port}"),
            role: "origin".into(),
            tunnel: None,
            processes: None,
            process: None,
        };
        let mut fleet = fleet_of(n);
        fleet.timeout_ms = 100;
        let started = m6_core::platform::now();
        let polled = run(&fleet, None);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the poll waited past its deadline"
        );
        let why = polled.readings[0].unreachable.as_deref().unwrap_or("");
        assert!(
            why.ends_with("Network Error: timed out reading response"),
            "{why}"
        );
        drop(release_tx);
        held.join().unwrap();
    }

    /// A node whose `url` ends in a slash gets header checks against the same
    /// base as every other request: `GET /path`, never `GET //path`.
    #[test]
    fn a_trailing_slash_url_builds_header_check_urls_without_a_double_slash() {
        let (port, seen) = serve(2, |path| {
            if path == "/health" {
                (200, r#"{"status":"ok","node":"origin"}"#)
            } else {
                (404, r#"{"error":"not found"}"#)
            }
        });
        let n = Node {
            name: "origin".into(),
            url: format!("http://127.0.0.1:{port}/"),
            role: "origin".into(),
            tunnel: None,
            processes: None,
            process: None,
        };
        let checks = vec![crate::fleet::HeaderCheck {
            path: "/capabilities".into(),
            header: "cache-control".into(),
            expect: "public, max-age=60".into(),
            roles: vec![],
            cache_bust: false,
        }];
        let mut fleet = fleet_of(n);
        fleet.header_check = checks;
        let polled = run(&fleet, Some("tok"));
        let seen = seen.join().unwrap();
        assert_eq!(seen.len(), 2, "{seen:?}");
        assert_eq!(seen[0].0, "GET /health HTTP/1.1");
        assert_eq!(seen[1].0, "GET /capabilities HTTP/1.1");
        assert!(
            seen.iter().all(|(line, _)| !line.contains("//")),
            "a request line carries a double slash: {seen:?}"
        );
        assert_eq!(polled.readings[0].header_checks.len(), 1);
    }

    // ── The steps that can fail at start, and the lookup path ─────────────

    const HEALTH: &str = r#"{"status":"ok","node":"origin"}"#;
    const TRAFFIC: &str = r#"{"node":"origin","window_minutes":1,"total_requests":10,"status":{},"notable":[],"heavy_hitters":[],"crawlers":[{"user_agent":"ExampleBot/1.0","requests":3,"client_ips":["192.0.2.7"],"paths":["/"]}],"forged_bot_requests":0,"forgers":[],"probe_noise":[],"logging":{"events_total":1,"seconds_since_last":1}}"#;

    /// The reply for a poll step that is still a request. The two documents
    /// come from the journal, so no test asks this server for them.
    fn health_reply(path: &str) -> (u16, &'static str) {
        match path {
            "/health" => (200, HEALTH),
            _ => (404, r#"{"error":"not found"}"#),
        }
    }

    /// A UDP nameserver on loopback that answers one PTR question with
    /// `crawl.example.com.` and the forward A and AAAA questions for it: A
    /// answers `192.0.2.7`, AAAA answers nothing. Three datagrams, then it
    /// stops.
    ///
    /// A DNS reply is the header, the question copied back and one answer, so
    /// this needs no crate: it echoes the query's id, copies its question, and
    /// appends a resource record that points at the question's name.
    fn scripted_nameserver() -> (std::net::SocketAddr, std::thread::JoinHandle<()>) {
        let sock = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let addr = sock.local_addr().unwrap();
        let handle = std::thread::spawn(move || {
            let mut buf = [0u8; 512];
            for _ in 0..3 {
                let (n, peer) = sock.recv_from(&mut buf).unwrap();
                let query = &buf[..n];
                let zero = query[12..].iter().position(|b| *b == 0).unwrap();
                let qend = 12 + zero + 5; // qname, its root zero, qtype, qclass
                let qtype = u16::from_be_bytes([query[qend - 4], query[qend - 3]]);
                let mut reply = Vec::with_capacity(64);
                reply.extend_from_slice(&query[..2]); // the id
                reply.extend_from_slice(&[0x81, 0x80]); // response, recursion
                reply.extend_from_slice(&[0x00, 0x01]); // one question
                reply.extend_from_slice(&(u16::from(qtype != 28)).to_be_bytes()); // answers
                reply.extend_from_slice(&[0, 0, 0, 0]);
                reply.extend_from_slice(&query[12..qend]);
                if qtype != 28 {
                    reply.extend_from_slice(&[
                        0xC0,
                        0x0C, // the question's name
                        0x00,
                        qtype as u8,
                        0x00,
                        0x01,
                        0x00,
                        0x00,
                        0x00,
                        0x3C,
                    ]);
                    let rdata: &[u8] = if qtype == 12 {
                        b"\x05crawl\x07example\x03com\x00"
                    } else {
                        &[192, 0, 2, 7]
                    };
                    reply.extend_from_slice(&(rdata.len() as u16).to_be_bytes());
                    reply.extend_from_slice(rdata);
                }
                sock.send_to(&reply, peer).unwrap();
            }
        });
        (addr, handle)
    }

    /// The DNS stage drives a real `Confirm` through the poller: the PTR
    /// answer becomes a name only after the forward A answer carries the
    /// address, and the lookup's second socket is registered while the first
    /// is gone.
    #[test]
    fn the_dns_stage_drives_a_real_confirm() {
        let (public, seen) = serve(1, health_reply);
        let n = Node {
            name: "origin".into(),
            url: format!("http://127.0.0.1:{public}"),
            role: "origin".into(),
            tunnel: None,
            processes: None,
            process: None,
        };
        let (nameserver, server) = scripted_nameserver();
        let polled = run_with_journal(&fleet_of(n), None, nameserver, &[traffic_line(TRAFFIC)]);
        server.join().unwrap();
        seen.join().unwrap();
        assert_eq!(
            polled.verdicts.get("192.0.2.7"),
            // The resolver's own answer keeps the root label's dot.
            Some(&m6_core::resolve::Ptr::Name("crawl.example.com.".into())),
            "{:?}",
            polled.verdicts
        );
    }

    /// A later step whose connect fails at once finishes the node as `after`
    /// does: its deadline is dropped and its slot marked done, so the finished
    /// node cannot fail a second time.
    ///
    /// The steps that follow `/health` are the journal's, so the one network
    /// step left after it is a header check, and that is the one made to fail.
    /// The failure is injected: a TCP connect that refuses at once on one
    /// platform reaches its readiness event on another, so the platform is not
    /// a sound trigger for this path.
    #[test]
    fn a_later_step_that_cannot_connect_finishes_the_node_once() {
        let (public, seen) = serve(1, health_reply);
        let n = Node {
            name: "origin".into(),
            url: format!("http://127.0.0.1:{public}"),
            role: "origin".into(),
            tunnel: None,
            processes: None,
            process: None,
        };
        let mut fleet = fleet_of(n);
        fleet.timeout_ms = 100;
        fleet.header_check = vec![HeaderCheck {
            path: "/".into(),
            header: "x-test".into(),
            expect: "yes".into(),
            roles: Vec::new(),
            cache_bust: false,
        }];
        let tls = tls();
        let mut poller = one_shot_poller(
            &fleet,
            &tls,
            "127.0.0.1:53".parse().unwrap(),
            crate::check::MAX_LOOKUPS_PER_POLL,
        )
        .unwrap();
        // `/health` starts; the header check after it fails at start, as a
        // connect that cannot open a socket fails.
        poller.fail_step_start_after(1, io::ErrorKind::AddrNotAvailable);
        let mut out = poller.out();
        let polled = drive_to_end(
            &mut poller,
            &mut out,
            Some(m6_core::platform::now() + Duration::from_secs(20)),
        )
        .unwrap();
        seen.join().unwrap();
        let r = &polled.readings[0];
        assert_eq!(r.health_status, Some(200), "{r:?}");
        assert_eq!(r.header_checks.len(), 1, "{r:?}");
        assert!(r.header_checks[0].error.is_some(), "{r:?}");
        assert!(r.perf_error.is_some(), "no line was fed to this poller");
        assert_eq!(r.unreachable, None, "{r:?}");

        // The failed step's deadline is still armed in the old code. It fires
        // now, on a finished node, and must find nothing.
        let poll = Poll::new().unwrap();
        let later = m6_core::platform::now() + Duration::from_secs(1);
        let mut expired = 0;
        while poller.expire(&poll, &mut out, later).unwrap().is_some() {
            expired += 1;
        }
        assert_eq!(
            expired, 0,
            "a finished node's stale deadline fired {expired} time(s)"
        );
    }

    /// A tunnel connect that fails at once is a reading, not the end of the
    /// service: the probe reports the failure and the poll finishes.
    ///
    /// The failure is injected for the same reason as the later-step test's:
    /// whether a refused connect answers synchronously is the platform's to
    /// decide, and this test is about the tally.
    #[test]
    fn a_tunnel_address_that_cannot_connect_is_reported_not_fatal() {
        let (public, seen) = serve(1, health_reply);
        let n = Node {
            name: "origin".into(),
            url: format!("http://127.0.0.1:{public}"),
            role: "origin".into(),
            tunnel: Some(crate::fleet::Tunnel {
                address: "192.0.2.1:9".into(),
                public_key: None,
            }),
            processes: None,
            process: None,
        };
        let fleet = fleet_of(n);
        let mut poller = one_shot_poller(
            &fleet,
            &tls(),
            "127.0.0.1:53".parse().unwrap(),
            crate::check::MAX_LOOKUPS_PER_POLL,
        )
        .unwrap();
        poller.fail_next_probe_connect(io::ErrorKind::AddrNotAvailable);
        let mut out = poller.out();
        let polled = drive_to_end(
            &mut poller,
            &mut out,
            Some(m6_core::platform::now() + Duration::from_secs(20)),
        )
        .unwrap();
        seen.join().unwrap();
        let t = polled.readings[0].tunnel.as_ref().unwrap();
        assert!(
            matches!(&t.probe.outcome, tunnel::Probe::Failed(why) if !why.is_empty()),
            "{:?}",
            t.probe.outcome
        );
    }

    /// A closed tunnel port answers with a reset: the probe reads `Refused`
    /// with a round trip, which is the branch a dead-but-reachable peer takes.
    #[test]
    fn a_closed_tunnel_port_reads_as_refused() {
        let (public, seen) = serve(1, health_reply);
        let closed = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap()
        };
        let n = Node {
            name: "origin".into(),
            url: format!("http://127.0.0.1:{public}"),
            role: "origin".into(),
            tunnel: Some(crate::fleet::Tunnel {
                address: closed.to_string(),
                public_key: None,
            }),
            processes: None,
            process: None,
        };
        let polled = run(&fleet_of(n), None);
        seen.join().unwrap();
        let t = polled.readings[0].tunnel.as_ref().unwrap();
        assert_eq!(
            t.probe.outcome,
            tunnel::Probe::Refused,
            "{:?}",
            t.probe.outcome
        );
        assert!(!t.probe.samples.is_empty(), "{t:?}");
    }
}
