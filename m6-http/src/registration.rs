//! m6-http's cache control channel: registration, the change counter, and
//! purge fan-out.
//!
//! A cache node holds one plain TCP connection open to its origin, the
//! control channel. Over it the cache announces itself and presents the change
//! counter it saved with its disk copy, the origin answers its own counter,
//! and the origin pushes each purge to every registered cache and reads back
//! whether the path was resident. TCP keepalive on the socket detects a dead
//! peer, so nothing polls and nothing runs on the request path.
//!
//! The module holds the state and the line protocol. The event loop owns the
//! sockets and drives [`ControlConn`], exactly as it drives every other
//! descriptor.

use std::io::{self, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpStream};
use std::os::fd::{AsRawFd, RawFd};
use std::time::{Duration, Instant};

use m6_core::platform::{Interest, Token};

use crate::forward::Waiter;

/// The HTTP path that reports the registered-cache set and the change counter,
/// replacing the source's `GET /genid`.
pub const STATUS_PATH: &str = "/m6/status";

/// The HTTP method that purges one path or all, the industry method Varnish,
/// Fastly and Squid all answer.
pub const PURGE_METHOD: &str = "PURGE";

/// The purge-all target, sent down the control channel as the purge's path.
pub const PURGE_ALL: &str = "*";

/// The longest control-channel line. A node name and a path are both far below
/// this, and a longer line is a malformed peer, not a request to serve.
pub const MAX_LINE: usize = 1024;

// ── The line protocol ────────────────────────────────────────────────────────

/// Parse one cache-to-origin line.
pub enum CacheLine<'a> {
    Register { name: &'a str, counter: u64 },
    Resident(bool),
    Unknown,
}

/// Parse one origin-to-cache line.
pub enum OriginLine<'a> {
    Counter(u64),
    Purge { target: &'a str, counter: u64 },
    Unknown,
}

pub fn parse_cache_line(line: &str) -> CacheLine<'_> {
    let mut parts = line.split_whitespace();
    let verb = parts.next().unwrap_or("");
    match verb {
        "REGISTER" => match (parts.next(), parts.next().and_then(|c| c.parse::<u64>().ok())) {
            (Some(name), Some(counter)) => CacheLine::Register { name, counter },
            _ => CacheLine::Unknown,
        },
        "RESIDENT" => match parts.next() {
            Some("true") => CacheLine::Resident(true),
            Some("false") => CacheLine::Resident(false),
            _ => CacheLine::Unknown,
        },
        _ => CacheLine::Unknown,
    }
}

pub fn parse_origin_line(line: &str) -> OriginLine<'_> {
    let mut parts = line.split_whitespace();
    let verb = parts.next().unwrap_or("");
    match verb {
        "COUNTER" => match parts.next().and_then(|c| c.parse::<u64>().ok()) {
            Some(counter) => OriginLine::Counter(counter),
            None => OriginLine::Unknown,
        },
        "PURGE" => match (parts.next(), parts.next().and_then(|c| c.parse::<u64>().ok())) {
            (Some(target), Some(counter)) => OriginLine::Purge { target, counter },
            _ => OriginLine::Unknown,
        },
        _ => OriginLine::Unknown,
    }
}

/// Encode the cache's registration line.
pub fn register_line(name: &str, counter: u64) -> String {
    format!("REGISTER {name} {counter}\n")
}

/// Encode the origin's counter answer.
pub fn counter_line(counter: u64) -> String {
    format!("COUNTER {counter}\n")
}

/// Encode a purge the origin pushes down the channel.
pub fn purge_line(target: &str, counter: u64) -> String {
    format!("PURGE {target} {counter}\n")
}

/// Encode the cache's residency answer.
pub fn resident_line(resident: bool) -> String {
    if resident {
        "RESIDENT true\n".to_string()
    } else {
        "RESIDENT false\n".to_string()
    }
}

// ── The socket ───────────────────────────────────────────────────────────────

/// Where a control connection is in its own start-up or end.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Phase {
    /// The non-blocking TCP connect is in progress.
    Connecting,
    /// The connection is open and lines flow.
    Open,
    /// The peer is gone, the connect failed, or a line overflowed.
    Done,
}

/// One control-channel connection, used by both sides.
///
/// The cache side connects and then writes `REGISTER`; the origin side is
/// accepted already open. `drive` reads complete lines into `lines`, writes
/// queued bytes, and reports when the peer is gone. No timer: keepalive is a
/// kernel mechanism on the socket.
pub struct ControlConn {
    stream: Option<TcpStream>,
    phase: Phase,
    read_buf: Vec<u8>,
    write_buf: Vec<u8>,
    /// Complete lines read by the last drive, drained by the owner.
    lines: Vec<String>,
    /// The connect finished and the owner has not yet been told.
    connected: bool,
}

impl ControlConn {
    /// Begin a non-blocking connect. The caller registers the descriptor and
    /// writes nothing until a drive reports the connect finished.
    pub fn connect(addr: SocketAddr) -> io::Result<Self> {
        let (stream, state) = m6_core::platform::net::connect_tcp(addr)?;
        set_keepalive(&stream)?;
        let connected = state == m6_core::platform::net::Connect::Connected;
        Ok(Self {
            stream: Some(stream),
            phase: if connected { Phase::Open } else { Phase::Connecting },
            read_buf: Vec::with_capacity(MAX_LINE),
            write_buf: Vec::with_capacity(MAX_LINE),
            lines: Vec::with_capacity(4),
            connected,
        })
    }

    /// An unassigned slot, for the token table's starting allocation. It is
    /// replaced by [`ControlConn::connect`] or [`ControlConn::accepted`]
    /// before it is ever driven.
    pub fn empty() -> Self {
        ControlConn {
            stream: None,
            phase: Phase::Done,
            read_buf: Vec::new(),
            write_buf: Vec::new(),
            lines: Vec::new(),
            connected: false,
        }
    }

    /// Wrap a connection the listener accepted, already open. The `connected`
    /// flag stays false: only a cache node's own outbound connect reports it,
    /// so an accepted connection is never mistaken for a just-connected cache.
    pub fn accepted(stream: TcpStream) -> io::Result<Self> {
        set_keepalive(&stream)?;
        Ok(Self {
            stream: Some(stream),
            phase: Phase::Open,
            read_buf: Vec::with_capacity(MAX_LINE),
            write_buf: Vec::with_capacity(MAX_LINE),
            lines: Vec::with_capacity(4),
            connected: false,
        })
    }

    /// Queue one line to write. Nothing is sent until the next drive.
    pub fn send_line(&mut self, line: &str) {
        self.write_buf.extend_from_slice(line.as_bytes());
    }

    /// Whether the connect finished since the last call. The cache side sends
    /// its register line once on the first true.
    pub fn take_connected(&mut self) -> bool {
        std::mem::take(&mut self.connected)
    }

    /// The lines the last drive read, in order.
    pub fn drain_lines(&mut self) -> Vec<String> {
        std::mem::take(&mut self.lines)
    }

    pub fn is_done(&self) -> bool {
        self.phase == Phase::Done
    }

    pub fn fd(&self) -> RawFd {
        self.stream.as_ref().map(AsRawFd::as_raw_fd).unwrap_or(-1)
    }

    /// The poll interest: writable while connecting or while bytes are queued,
    /// and readable once open.
    pub fn interest(&self) -> Interest {
        match self.phase {
            Phase::Connecting => Interest::WRITABLE,
            Phase::Open if !self.write_buf.is_empty() => Interest::READABLE.add(Interest::WRITABLE),
            Phase::Open => Interest::READABLE,
            Phase::Done => Interest::READABLE,
        }
    }

    /// Drive one turn: finish a connect, then read lines and write queued
    /// bytes. Returns true while the connection is still alive.
    pub fn drive(&mut self) -> bool {
        if self.phase == Phase::Done {
            return false;
        }
        if self.phase == Phase::Connecting {
            match self.stream.as_ref().and_then(|s| s.take_error().ok().flatten()) {
                Some(_) => {
                    self.close();
                    return false;
                }
                None => {
                    self.phase = Phase::Open;
                    self.connected = true;
                }
            }
        }
        self.read_lines();
        self.write_bytes();
        self.phase != Phase::Done
    }

    fn read_lines(&mut self) {
        let mut tmp = [0u8; 1024];
        loop {
            let Some(stream) = self.stream.as_mut() else {
                self.phase = Phase::Done;
                return;
            };
            match stream.read(&mut tmp) {
                Ok(0) => {
                    self.close();
                    return;
                }
                Ok(n) => {
                    self.read_buf.extend_from_slice(&tmp[..n]);
                    if self.read_buf.len() > MAX_LINE {
                        // A peer that sends no newline before the bound is
                        // malformed. Drop it rather than growing a buffer.
                        self.close();
                        return;
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(_) => {
                    self.close();
                    return;
                }
            }
        }
        // Split complete lines; keep the partial tail.
        let mut start = 0;
        while let Some(pos) = self.read_buf[start..].iter().position(|b| *b == b'\n') {
            let end = start + pos;
            let line = self.read_buf[start..end].to_owned();
            self.lines
                .push(String::from_utf8_lossy(&line).trim().to_string());
            start = end + 1;
        }
        if start > 0 {
            self.read_buf.drain(..start);
        }
    }

    fn write_bytes(&mut self) {
        if self.write_buf.is_empty() {
            return;
        }
        let Some(stream) = self.stream.as_mut() else {
            self.phase = Phase::Done;
            return;
        };
        let mut off = 0;
        while off < self.write_buf.len() {
            match stream.write(&self.write_buf[off..]) {
                Ok(0) => {
                    self.close();
                    return;
                }
                Ok(n) => off += n,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(_) => {
                    self.close();
                    return;
                }
            }
        }
        if off > 0 {
            self.write_buf.drain(..off);
        }
    }

    pub fn close(&mut self) {
        self.phase = Phase::Done;
        if let Some(stream) = self.stream.take() {
            let _ = stream.shutdown(Shutdown::Both);
        }
    }
}

/// Set TCP keepalive so a silent partition (the WireGuard path drops with no
/// FIN or RST) eventually closes the socket. Kernel mechanism, no loop timer.
fn set_keepalive(stream: &TcpStream) -> io::Result<()> {
    use socket2::SockRef;
    SockRef::from(stream).set_keepalive(true)
}

// ── The role and the cache's state ──────────────────────────────────────────

/// Which side of the control channel this node is.
#[allow(clippy::large_enum_variant)]
pub enum Role {
    /// The origin owns the change counter and the registered-cache set.
    Source(Source),
    /// A cache node follows one origin's counter.
    Cache(Follower),
}

impl Role {
    pub fn source(&self) -> Option<&Source> {
        match self {
            Role::Source(s) => Some(s),
            Role::Cache(_) => None,
        }
    }

    pub fn source_mut(&mut self) -> Option<&mut Source> {
        match self {
            Role::Source(s) => Some(s),
            Role::Cache(_) => None,
        }
    }

    pub fn cache(&self) -> Option<&Follower> {
        match self {
            Role::Cache(f) => Some(f),
            Role::Source(_) => None,
        }
    }

    pub fn cache_mut(&mut self) -> Option<&mut Follower> {
        match self {
            Role::Cache(f) => Some(f),
            Role::Source(_) => None,
        }
    }
}

/// A cache node's view of its origin: the counter it saved, and whether the
/// control connection is up. The socket itself is owned by the event loop.
pub struct Follower {
    counter: u64,
    up: bool,
}

impl Follower {
    pub fn new(counter: u64) -> Self {
        Follower { counter, up: false }
    }

    /// The counter this cache saved with its disk copy.
    pub fn counter(&self) -> u64 {
        self.counter
    }

    pub fn set_counter(&mut self, counter: u64) {
        self.counter = counter;
    }

    /// Whether the control connection to the origin is up, for `/health`.
    pub fn up(&self) -> bool {
        self.up
    }

    pub fn set_up(&mut self, up: bool) {
        self.up = up;
    }
}

// ── The origin's state ───────────────────────────────────────────────────────

/// One cache that has registered, by its control-channel connection.
pub struct Registered {
    pub name: String,
    pub conn: Token,
}

/// One cache the fan-out waits on, and its residency once it answers.
struct FanEntry {
    name: String,
    conn: Token,
    /// `None` until the cache answers `RESIDENT`.
    resident: Option<bool>,
    /// The cache's connection dropped before it answered: pending.
    gone: bool,
}

/// One purge the origin is fanned out, waiting on every registered cache's
/// residency answer.
pub struct FanOut {
    target: String,
    counter: u64,
    self_resident: bool,
    entries: Vec<FanEntry>,
    /// The `PURGE` callers this fan-out answers, in arrival order.
    waiters: Vec<Waiter>,
    deadline: Instant,
}

/// What one finished fan-out hands back, so the caller writes the reply.
pub struct Finished {
    pub self_resident: bool,
    /// Each cache and its answer: resident or not, or pending when it did not
    /// answer in time.
    pub nodes: Vec<(String, NodeAnswer)>,
    pub waiters: Vec<Waiter>,
    /// A purge arrived during this fan-out and is queued for the next one.
    pub again: bool,
}

/// One cache's answer in a finished purge.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NodeAnswer {
    Resident(bool),
    Pending,
}

/// One queued purge, waiting for the running fan-out to finish.
pub struct QueuedPurge {
    pub target: String,
    pub self_resident: bool,
    pub waiters: Vec<Waiter>,
}

/// The origin's control-channel state: the change counter, the registered
/// caches, and the purge being fanned out.
pub struct Source {
    counter: u64,
    registered: Vec<Registered>,
    fanout: Option<FanOut>,
    queued: Vec<QueuedPurge>,
}

impl Source {
    pub fn new(counter: u64) -> Self {
        Source {
            counter,
            registered: Vec::new(),
            fanout: None,
            queued: Vec::new(),
        }
    }

    /// The change counter. A purge raises it by one.
    pub fn counter(&self) -> u64 {
        self.counter
    }

    /// The registered caches, for `/m6/status` and for the fan-out.
    pub fn registered(&self) -> &[Registered] {
        &self.registered
    }

    /// Whether a fan-out is running.
    pub fn fanout_running(&self) -> bool {
        self.fanout.is_some()
    }

    /// A cache announced itself on `conn`. A re-registration on the same
    /// connection replaces the old name, which is the cache reconnecting.
    pub fn register(&mut self, name: String, conn: Token) {
        if let Some(entry) = self.registered.iter_mut().find(|r| r.conn == conn) {
            entry.name = name;
            return;
        }
        self.registered.push(Registered { name, conn });
    }

    /// A cache's control connection dropped. Returns the name it registered
    /// under, so the caller can log the loss.
    pub fn unregister(&mut self, conn: Token) -> Option<String> {
        let name = self
            .registered
            .iter()
            .find(|r| r.conn == conn)
            .map(|r| r.name.clone());
        self.registered.retain(|r| r.conn != conn);
        if let Some(fanout) = self.fanout.as_mut() {
            for entry in fanout.entries.iter_mut() {
                if entry.conn == conn && entry.resident.is_none() {
                    entry.gone = true;
                }
            }
        }
        name
    }

    /// A `PURGE` arrived from a caller. When no fan-out is running, this
    /// raises the counter, snapshots the registered caches and starts the
    /// fan-out, returning true so the caller writes `PURGE` to each cache.
    /// During a fan-out it queues the purge and returns false.
    pub fn post_purge(
        &mut self,
        target: String,
        self_resident: bool,
        waiter: Waiter,
        timeout: Duration,
    ) -> bool {
        if self.fanout.is_some() {
            if let Some(q) = self
                .queued
                .iter_mut()
                .find(|q| q.target == target && q.self_resident == self_resident)
            {
                q.waiters.push(waiter);
            } else {
                self.queued.push(QueuedPurge {
                    target,
                    self_resident,
                    waiters: vec![waiter],
                });
            }
            return false;
        }
        self.start_fanout(target, self_resident, vec![waiter], timeout);
        true
    }

    /// Begin a fan-out over the currently registered caches. Called only when
    /// none is running. Raises the counter by one, as every purge does.
    pub fn start_fanout(
        &mut self,
        target: String,
        self_resident: bool,
        waiters: Vec<Waiter>,
        timeout: Duration,
    ) {
        debug_assert!(self.fanout.is_none(), "one fan-out at a time");
        self.counter = self.counter.wrapping_add(1);
        let entries = self
            .registered
            .iter()
            .map(|r| FanEntry {
                name: r.name.clone(),
                conn: r.conn,
                resident: None,
                gone: false,
            })
            .collect();
        self.fanout = Some(FanOut {
            target,
            counter: self.counter,
            self_resident,
            entries,
            waiters,
            deadline: m6_core::platform::now() + timeout,
        });
    }

    /// The running fan-out's target and new counter, for the caller to write
    /// one `PURGE` line to each registered cache. `None` when no fan-out runs.
    pub fn fanout_purge(&self) -> Option<(&str, u64)> {
        self.fanout
            .as_ref()
            .map(|f| (f.target.as_str(), f.counter))
    }

    /// The cache connections the fan-out must reach, in order, so the caller
    /// writes one `PURGE` line to each.
    pub fn fanout_conns(&self) -> Vec<(String, Token)> {
        self.fanout
            .as_ref()
            .map(|f| {
                f.entries
                    .iter()
                    .map(|e| (e.name.clone(), e.conn))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// A cache answered `RESIDENT`. Returns true when the fan-out became
    /// complete.
    pub fn resident(&mut self, conn: Token, resident: bool) -> bool {
        let Some(fanout) = self.fanout.as_mut() else {
            return false;
        };
        for entry in fanout.entries.iter_mut() {
            if entry.conn == conn && entry.resident.is_none() {
                entry.resident = Some(resident);
            }
        }
        fanout.entries.iter().all(|e| e.resident.is_some() || e.gone)
    }

    /// Whether the fan-out's deadline has passed, so the caller can complete it
    /// with every unanswered cache pending.
    pub fn fanout_due(&self, now: Instant) -> bool {
        self.fanout.as_ref().is_some_and(|f| now >= f.deadline)
    }

    /// Whether the running fan-out has every cache answered or gone, which
    /// includes the empty case (no registered caches), so the caller can
    /// complete it without waiting for a `RESIDENT` event that never comes.
    pub fn fanout_complete(&self) -> bool {
        self.fanout
            .as_ref()
            .is_some_and(|f| f.entries.iter().all(|e| e.resident.is_some() || e.gone))
    }

    /// Complete the running fan-out and hand back what the reply needs. The
    /// caller then writes the reply and, when `again` is true, starts the next
    /// queued fan-out.
    pub fn finish_fanout(&mut self) -> Option<Finished> {
        let fanout = self.fanout.take()?;
        let nodes = fanout
            .entries
            .iter()
            .map(|e| {
                let answer = match e.resident {
                    Some(r) => NodeAnswer::Resident(r),
                    None => NodeAnswer::Pending,
                };
                (e.name.clone(), answer)
            })
            .collect();
        let again = !self.queued.is_empty();
        Some(Finished {
            self_resident: fanout.self_resident,
            nodes,
            waiters: fanout.waiters,
            again,
        })
    }

    /// The next queued purge, taken to start a fan-out after a finish.
    pub fn take_queued(&mut self) -> Option<QueuedPurge> {
        if self.queued.is_empty() {
            None
        } else {
            Some(self.queued.remove(0))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn waiter(n: usize) -> Waiter {
        Waiter::H1 {
            conn: Token(n),
            gen: n as u64,
        }
    }

    fn tok(n: usize) -> Token {
        Token(n)
    }

    fn timeout() -> Duration {
        Duration::from_secs(30)
    }

    #[test]
    fn register_line_parses_round_trip() {
        let line = register_line("lon", 7);
        let CacheLine::Register { name, counter } = parse_cache_line(line.trim()) else {
            panic!("a register line parses");
        };
        assert_eq!(name, "lon");
        assert_eq!(counter, 7);
    }

    #[test]
    fn counter_and_purge_and_resident_lines_parse() {
        assert!(matches!(
            parse_origin_line(counter_line(42).trim()),
            OriginLine::Counter(42)
        ));
        assert!(matches!(
            parse_origin_line(purge_line(PURGE_ALL, 9).trim()),
            OriginLine::Purge {
                target: "*",
                counter: 9
            }
        ));
        assert!(matches!(
            parse_cache_line(resident_line(true).trim()),
            CacheLine::Resident(true)
        ));
        assert!(matches!(
            parse_cache_line(resident_line(false).trim()),
            CacheLine::Resident(false)
        ));
    }

    #[test]
    fn malformed_lines_are_unknown() {
        assert!(matches!(parse_cache_line(""), CacheLine::Unknown));
        assert!(matches!(parse_cache_line("REGISTER"), CacheLine::Unknown));
        assert!(matches!(
            parse_cache_line("REGISTER lon nope"),
            CacheLine::Unknown
        ));
        assert!(matches!(parse_origin_line("PURGE /x"), OriginLine::Unknown));
        assert!(matches!(parse_origin_line("COUNTER"), OriginLine::Unknown));
    }

    #[test]
    fn a_purge_raises_the_counter_and_collects_residency() {
        let mut s = Source::new(0);
        s.register("lon".to_string(), tok(1));
        s.register("chi".to_string(), tok(2));
        assert_eq!(s.counter(), 0);

        assert!(s.post_purge("/r/cap7k9".to_string(), true, waiter(9), timeout()));
        assert_eq!(s.counter(), 1, "the purge raised the counter");
        let (target, counter) = s.fanout_purge().expect("a fan-out runs");
        assert_eq!(target, "/r/cap7k9");
        assert_eq!(counter, 1);

        assert!(!s.resident(tok(1), true), "one cache still owes an answer");
        assert!(s.resident(tok(2), false), "the last cache finishes it");
        let f = s.finish_fanout().expect("finished");
        assert!(f.self_resident);
        assert_eq!(
            f.nodes,
            vec![
                ("lon".to_string(), NodeAnswer::Resident(true)),
                ("chi".to_string(), NodeAnswer::Resident(false))
            ]
        );
        assert_eq!(f.waiters, vec![waiter(9)]);
        assert!(!f.again);
    }

    #[test]
    fn a_purge_during_a_fanout_is_queued_for_the_next() {
        let mut s = Source::new(0);
        s.register("lon".to_string(), tok(1));
        assert!(s.post_purge("/a".to_string(), true, waiter(1), timeout()));
        assert!(!s.post_purge("/b".to_string(), false, waiter(2), timeout()));
        assert!(s.resident(tok(1), true));
        let first = s.finish_fanout().expect("finished");
        assert_eq!(first.nodes, vec![("lon".to_string(), NodeAnswer::Resident(true))]);
        assert!(first.again, "a queued purge waits");
        let next = s.take_queued().expect("queued");
        assert_eq!(next.target, "/b");
        assert!(!next.self_resident);
    }

    #[test]
    fn a_cache_that_drops_before_answering_is_pending() {
        let mut s = Source::new(0);
        s.register("lon".to_string(), tok(1));
        assert!(s.post_purge("/a".to_string(), false, waiter(1), timeout()));
        assert_eq!(s.unregister(tok(1)), Some("lon".to_string()));
        let f = s.finish_fanout().expect("finished");
        assert_eq!(f.nodes, vec![("lon".to_string(), NodeAnswer::Pending)]);
    }

    #[test]
    fn a_re_registration_on_the_same_connection_replaces_the_name() {
        let mut s = Source::new(0);
        s.register("lon".to_string(), tok(1));
        s.register("lon-new".to_string(), tok(1));
        assert_eq!(s.registered().len(), 1);
        assert_eq!(s.registered()[0].name, "lon-new");
    }

    #[test]
    fn a_missing_counter_register_line_is_unknown() {
        assert!(matches!(
            parse_cache_line("REGISTER lon"),
            CacheLine::Unknown
        ));
    }
}
