//! The site generation ID, held by the source node and by nothing else.
//!
//! A generation is the content a set of backends is serving at one moment. The
//! source owns the number that names it, advanced only by a completed
//! `POST /genid`, and every response a backend returns is stamped with the
//! generation it was dispatched under. A backend never sees the ID: its only
//! part is `POST /genid` answering 204 after it has rebuilt its content.
//!
//! [`Genid`] is the state, with the change sequence's waiters and a fixed table
//! of the peers that read the ID. It does no I/O beyond [`read_state`] and
//! [`write_state`], which the caller times, so the event loop stays the only
//! owner of when a file is touched.
//!
//! The module is m6-http's, not m6-core's: a generation is a fact about a
//! deployment's content, which no other service needs to know.

use std::io;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::forward::Waiter;

/// The header that carries a response's generation to a client.
pub const GENID_HEADER: &str = "M6-Genid";

/// The same header, in the only spelling HTTP/2 and HTTP/3 may put on the wire.
///
/// RFC 9113 8.2.1 and RFC 9114 4.2 require field names in lower case, and a
/// strict client rejects the whole header block when one is not. `GENID_HEADER`
/// is the HTTP/1.1 spelling (case does not matter there, and it is what the
/// docs show). An h2 response written with it reached curl as a protocol error
/// and zero body bytes where HTTP/1.1 read 2671.
pub const GENID_HEADER_LOWER: &str = "m6-genid";

/// How often a cache node asks its upstream for the ID, unless configured.
pub const DEFAULT_HEARTBEAT_SECS: u64 = 10;

/// How soon a cache node with no ID retries a heartbeat that failed.
///
/// A node with no ID cannot serve a request: the store rule needs the ID a
/// reply was dispatched under. One missed answer under load would otherwise
/// keep it blind for a whole configured interval, which is an hour at
/// `heartbeat_secs = 3600`. The loop's own accounting tick is the smallest
/// unit it schedules, so this is one tick. Once the node holds an ID the
/// interval is the right wait and this constant is not used.
const NO_ID_RETRY: Duration = Duration::from_secs(1);

/// What this node's generation state is: the source that owns the ID, or a
/// cache node that follows another node's ID.
///
/// Held once in `ServerState` for the life of the process, so the source's
/// larger state costs one bigger field and no allocation. Boxing it would add
/// an indirection to every `/genid` and store-rule read for no memory saved.
#[allow(clippy::large_enum_variant)]
pub enum Role {
    Source(Genid),
    Cache(Follower),
}

impl Role {
    /// The source's state, when this node is the source.
    pub fn source(&self) -> Option<&Genid> {
        match self {
            Role::Source(g) => Some(g),
            Role::Cache(_) => None,
        }
    }

    pub fn source_mut(&mut self) -> Option<&mut Genid> {
        match self {
            Role::Source(g) => Some(g),
            Role::Cache(_) => None,
        }
    }

    /// The cache node's follower state, when this node is a cache node.
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

    /// The ID a response dispatched now is stamped with. A cache node with no
    /// ID yet answers 0, which no reply can match, so nothing is stored.
    pub fn dispatched(&self) -> u64 {
        match self {
            Role::Source(g) => g.current(),
            Role::Cache(f) => f.current().unwrap_or(0),
        }
    }

    /// The ID a completed response is stored under, or `None` when it must not
    /// be stored, given the reply's `M6-Genid` already parsed.
    ///
    /// The source keeps its dispatch-ID rule. A cache node stores only a reply
    /// whose `M6-Genid` equals the ID it holds: the ID it serves, not the ID it
    /// asked for.
    pub fn may_store_parsed(&self, dispatched: u64, id: Option<u64>) -> Option<u64> {
        match self {
            Role::Source(g) => (dispatched == g.current()).then_some(dispatched),
            Role::Cache(f) => f.may_store_parsed(id),
        }
    }
}

/// Parse a decimal `M6-Genid` value in place: a borrowed `&str`, no copy.
fn parse_id(header: &[u8]) -> Option<u64> {
    std::str::from_utf8(header).ok()?.trim().parse().ok()
}

/// A cache node's view of the source: the ID it last heard, whether the
/// upstream is down, and when the next heartbeat is due.
///
/// No I/O. The loop owns the one heartbeat upstream and calls [`Follower::sent`]
/// when it starts one and [`Follower::answered`] when it finishes, so every
/// state change happens on the loop's own thread.
pub struct Follower {
    /// The ID this node holds. `None` until the first answer, and a node with
    /// no ID stores nothing.
    current: Option<u64>,
    /// The last heartbeat or forwarded request failed to reach the upstream.
    down: bool,
    /// The one heartbeat upstream is in flight, so a tick cannot start a second.
    in_flight: bool,
    /// When the next heartbeat may start.
    due: Instant,
    /// When the upstream last answered with an ID.
    last_ok: Option<Instant>,
    /// The last successful heartbeat's round trip.
    rtt: Option<Duration>,
    /// How long between heartbeats.
    heartbeat: Duration,
    /// The node is serving a disk copy at start whose ID no heartbeat has
    /// confirmed. Cleared by the first successful answer, whichever ID it
    /// names: a `Same` answer confirms the copy, and a `Changed` one flushes
    /// it on the same turn.
    unverified: bool,
}

/// What one heartbeat or forwarded request changed, so the caller logs the
/// transition once and runs the flush on a new ID.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The first answer set the ID. No change: there was no old content.
    First(u64),
    /// The upstream still holds the ID this node holds.
    Same,
    /// The upstream advanced. The caller flushes and refills on demand.
    Changed { from: u64, to: u64 },
    /// The upstream stopped answering. The caller serves held content.
    Down(&'static str),
    /// It is still down. Nothing to log.
    StillDown,
}

/// What the first heartbeat answer decides about a disk copy read at start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Settle {
    /// The answer named the ID the files carried: install the loaded cache.
    Keep,
    /// The answer named another ID: drop the loaded cache.
    Discard,
    /// The upstream did not answer: install the loaded cache, unverified.
    Unverified,
}

/// The one decision the first heartbeat makes about a held disk load.
///
/// This is the same equality the store rule uses: content is only served
/// under the ID it was written under. The first answer is the first time the
/// node has an ID to compare, so it is the only place the held copy can be
/// installed or dropped.
pub fn settle(pending_id: u64, outcome: &Outcome) -> Settle {
    match outcome {
        Outcome::First(id) if *id == pending_id => Settle::Keep,
        Outcome::First(_) => Settle::Discard,
        Outcome::Down(_) => Settle::Unverified,
        // The rest cannot be the first answer: before it the follower holds no
        // ID, so `answered` returns only `First` or `Down`. If one ever
        // arrives, an unconfirmed load is the unsafe reading, so it goes.
        Outcome::Same | Outcome::Changed { .. } | Outcome::StillDown => Settle::Discard,
    }
}

impl Follower {
    /// A follower that holds no ID and owes a heartbeat at `now`.
    pub fn new(heartbeat: Duration, now: Instant) -> Follower {
        Follower {
            current: None,
            down: false,
            in_flight: false,
            due: now,
            last_ok: None,
            rtt: None,
            heartbeat,
            unverified: false,
        }
    }

    /// The ID this node holds, if it has one.
    pub fn current(&self) -> Option<u64> {
        self.current
    }

    pub fn down(&self) -> bool {
        self.down
    }

    pub fn rtt(&self) -> Option<Duration> {
        self.rtt
    }

    /// Whether the node is serving a disk copy its upstream has not confirmed.
    pub fn unverified(&self) -> bool {
        self.unverified
    }

    /// Hold `id` without an answer: the node serves the disk copy it read at
    /// start, and the ID it carries is the one the files were written under.
    ///
    /// The next successful answer compares against this ID as usual, so an
    /// unchanged site clears the flag and a changed one runs the same flush a
    /// confirmed change would.
    pub fn install_unverified(&mut self, id: u64) {
        self.current = Some(id);
        self.unverified = true;
    }

    /// Seconds since the last successful answer, for `/perf`.
    pub fn last_ok_s(&self, now: Instant) -> Option<u64> {
        self.last_ok
            .map(|at| now.saturating_duration_since(at).as_secs())
    }

    /// Whether a heartbeat may start now: at start, then once
    /// `heartbeat_secs` have passed since the last one finished. One at a time.
    pub fn heartbeat_due(&self, now: Instant) -> bool {
        !self.in_flight && now >= self.due
    }

    /// The heartbeat upstream is started. The next answer clears this.
    pub fn sent(&mut self) {
        self.in_flight = true;
    }

    /// One heartbeat finished: a parsed ID and the round trip, or a static
    /// reason it did not (`"connect failed"`, `"timed out"`, `"status"`,
    /// `"no M6-Genid"`). The reason is a static string so a failure allocates
    /// nothing.
    pub fn answered(
        &mut self,
        result: Result<(u64, Duration), &'static str>,
        now: Instant,
    ) -> Outcome {
        self.in_flight = false;
        match result {
            Ok((id, rtt)) => {
                self.due = now + self.heartbeat;
                self.rtt = Some(rtt);
                self.last_ok = Some(now);
                self.down = false;
                // A success confirms whatever copy the node is serving,
                // including one installed unverified at start.
                self.unverified = false;
                match self.current {
                    None => {
                        self.current = Some(id);
                        Outcome::First(id)
                    }
                    Some(current) if current == id => Outcome::Same,
                    Some(from) => {
                        self.current = Some(id);
                        Outcome::Changed { from, to: id }
                    }
                }
            }
            Err(reason) => {
                // A node with no ID cannot serve, so it retries a failed
                // heartbeat on the next tick rather than waiting out an
                // interval that may be configured very long.
                self.due = now
                    + if self.current.is_none() {
                        self.heartbeat.min(NO_ID_RETRY)
                    } else {
                        self.heartbeat
                    };
                let was_down = self.down;
                self.down = true;
                if was_down {
                    Outcome::StillDown
                } else {
                    Outcome::Down(reason)
                }
            }
        }
    }

    /// Make the next heartbeat due now. A reply under a different ID is the
    /// upstream saying the node is behind, and there is no reason to wait out
    /// the interval to ask.
    pub fn behind(&mut self, now: Instant) {
        self.due = now;
    }

    /// A forwarded client request could not reach the upstream. Mark it down
    /// and make the next heartbeat due now, which is the cheap way to learn
    /// whether the whole upstream is gone.
    ///
    /// The heartbeat in flight is left alone: its own answer clears that flag,
    /// and clearing it here would let the next turn start a second one.
    /// Returns true on the transition, so the caller logs it once.
    pub fn failed(&mut self, now: Instant) -> bool {
        let first = !self.down;
        self.down = true;
        self.due = now;
        first
    }

    /// The ID to store a reply under, or `None` when it must not be stored.
    ///
    /// A cache node stores only a response stamped with the ID it holds. A
    /// missing header, an unparsable one, another ID, or no ID yet all mean the
    /// response is sent to the client and not kept. The parse reads the header
    /// as a borrowed `&str`, so the rule allocates nothing.
    pub fn may_store(&self, header: Option<&[u8]>) -> Option<u64> {
        self.may_store_parsed(header.and_then(parse_id))
    }

    /// The same rule over an already parsed ID.
    pub fn may_store_parsed(&self, id: Option<u64>) -> Option<u64> {
        let current = self.current?;
        (id == Some(current)).then_some(current)
    }
}

/// The peers a source remembers. Only a loopback or an internal-listener
/// caller reaches the endpoint at all, so a site's monitoring set fits many
/// times over. A 33rd address replaces the oldest.
pub const PEER_SLOTS: usize = 32;

/// The source's generation state: the number, the sequence running now and the
/// POSTs waiting for one.
pub struct Genid {
    current: u64,
    /// `$StateDirectory/genid`, or `None` when this node has no `[genid]` and
    /// therefore reads and writes no file.
    state_path: Option<PathBuf>,
    running: Option<Sequence>,
    /// A POST arrived during the running sequence and needs the next one.
    again: bool,
    /// POSTs this sequence answers. One whose dispatch began the sequence is
    /// the first entry.
    covered: Vec<Waiter>,
    /// POSTs that arrived while a sequence ran. They move to `covered` when
    /// the next sequence starts.
    parked: Vec<Waiter>,
    peers: [Option<(IpAddr, Instant)>; PEER_SLOTS],
    /// A POST arrived with no sequence running, so the loop must start one.
    start_requested: bool,
    /// The socket paths the running sequence is refreshing, by the index its
    /// `Waiter::Genid` carries for the failure log line.
    members: Vec<PathBuf>,
}

/// One change sequence: how many members still owe an answer, how many failed,
/// and when it began.
struct Sequence {
    waiting: usize,
    failed: usize,
    started: Instant,
}

/// What [`Genid::finish`] hands back: the new ID, the POSTs it answers, and the
/// facts the one `genid advanced` line records.
pub struct Finished {
    pub id: u64,
    pub covered: Vec<Waiter>,
    pub again: bool,
    pub failed: usize,
    pub members: usize,
    pub elapsed_ms: u128,
}

impl Genid {
    /// State for a source whose ID is `current`, writing to `state_path` when
    /// an advance commits. `None` means this node holds the ID in memory and
    /// reads no file, which is the no-`[genid]` case.
    pub fn new(current: u64, state_path: Option<PathBuf>) -> Genid {
        Genid {
            current,
            state_path,
            running: None,
            again: false,
            covered: Vec::with_capacity(64),
            parked: Vec::with_capacity(64),
            peers: [None; PEER_SLOTS],
            start_requested: false,
            members: Vec::new(),
        }
    }

    /// The ID a response dispatched now carries.
    pub fn current(&self) -> u64 {
        self.current
    }

    /// Where an advance is written, or `None` when this source keeps its ID in
    /// memory.
    pub fn state_path(&self) -> Option<&Path> {
        self.state_path.as_deref()
    }

    /// Whether a change sequence is running. `/perf` reports it.
    pub fn sequence_running(&self) -> bool {
        self.running.is_some()
    }

    /// Whether the loop owes a sequence start. Cleared by the caller that
    /// starts it.
    pub fn take_start_requested(&mut self) -> bool {
        std::mem::replace(&mut self.start_requested, false)
    }

    /// Whether the running sequence has no member left to hear from, so the
    /// loop can finish it at once.
    pub fn finish_due(&self) -> bool {
        self.running.as_ref().is_some_and(|s| s.waiting == 0)
    }

    /// Park one POST.
    ///
    /// Returns true when no sequence ran, so the caller must start one now.
    /// Returns false when a sequence is running: the POST is covered by the
    /// next one, which keeps the promise that every POST is answered by a
    /// sequence that started after it arrived.
    pub fn post(&mut self, w: Waiter) -> bool {
        if self.running.is_some() {
            self.parked.push(w);
            self.again = true;
            false
        } else {
            self.covered.push(w);
            self.start_requested = true;
            true
        }
    }

    /// Begin the sequence over `members`. POSTs parked during the previous
    /// sequence become this one's covered set, so they are answered by a
    /// sequence that began after they arrived.
    pub fn start(&mut self, members: &[PathBuf]) {
        debug_assert!(self.running.is_none(), "one sequence at a time");
        self.covered.append(&mut self.parked);
        self.again = false;
        self.members = members.to_vec();
        self.running = Some(Sequence {
            waiting: self.members.len(),
            failed: 0,
            started: m6_core::platform::now(),
        });
    }

    /// One member answered. `ok` is false for a connect failure, a deadline and
    /// any status other than 204, 404 or 405. Returns true when this was the
    /// last member, so the caller runs the change step in the same turn.
    pub fn member_done(&mut self, ok: bool) -> bool {
        let Some(seq) = self.running.as_mut() else {
            return false;
        };
        seq.waiting = seq.waiting.saturating_sub(1);
        if !ok {
            seq.failed += 1;
        }
        seq.waiting == 0
    }

    /// The socket path of member `index`, for the failure log line.
    pub fn member_path(&self, index: u32) -> Option<&Path> {
        self.members.get(index as usize).map(PathBuf::as_path)
    }

    /// Complete the sequence: advance the ID by one and hand back the POSTs it
    /// answers.
    ///
    /// The caller rebuilds the content state first, then calls this, then
    /// writes the new ID and answers the returned waiters. Advancing here keeps
    /// one owner for the number.
    pub fn finish(&mut self) -> Finished {
        let seq = self.running.take().expect("finish only after every member");
        let covered = std::mem::take(&mut self.covered);
        let again = self.again;
        self.again = false;
        self.current = self.current.wrapping_add(1);
        Finished {
            id: self.current,
            covered,
            again,
            failed: seq.failed,
            members: self.members.len(),
            elapsed_ms: seq.started.elapsed().as_millis(),
        }
    }

    /// Record that `ip` read the ID at `now`, in its slot or the slot of the
    /// oldest peer. Fixed storage, no allocation.
    pub fn saw_peer(&mut self, ip: IpAddr, now: Instant) {
        let ip = ip.to_canonical();
        let mut oldest: Option<(usize, Instant)> = None;
        for (i, slot) in self.peers.iter_mut().enumerate() {
            match slot {
                Some((seen, at)) if *seen == ip => {
                    *at = now;
                    return;
                }
                Some((_, at)) => {
                    if oldest.is_none_or(|(_, o)| *at < o) {
                        oldest = Some((i, *at));
                    }
                }
                None => {
                    *slot = Some((ip, now));
                    return;
                }
            }
        }
        if let Some((i, _)) = oldest {
            self.peers[i] = Some((ip, now));
        }
    }

    /// The peers that read the ID and how long ago, sorted by address. Built
    /// only for an authorised `/perf`, which already allocates its report.
    pub fn peers(&self, now: Instant) -> Vec<(IpAddr, u64)> {
        let mut out: Vec<(IpAddr, u64)> = self
            .peers
            .iter()
            .flatten()
            .map(|(ip, at)| (*ip, now.saturating_duration_since(*at).as_secs()))
            .collect();
        out.sort_unstable_by_key(|(ip, _)| *ip);
        out
    }
}

/// Read `$StateDirectory/genid`: the decimal ID and a newline. An absent file
/// is 0, which is the first generation. Anything else is an error the start
/// prints.
pub fn read_state(path: &Path) -> anyhow::Result<u64> {
    match std::fs::read_to_string(path) {
        Ok(text) => text
            .trim()
            .parse::<u64>()
            .map_err(|e| anyhow::anyhow!("does not hold a u64: {e}")),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(0),
        Err(e) => Err(anyhow::anyhow!("cannot be read: {e}")),
    }
}

/// Write the ID to `<path>.tmp` and rename it over `path`, so a reader either
/// sees the previous generation or the new one, never a partial line.
pub fn write_state(path: &Path, id: u64) -> io::Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, format!("{id}\n"))?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use m6_core::platform::Token;

    fn waiter(n: usize) -> Waiter {
        Waiter::H1 {
            conn: Token(n),
            gen: n as u64,
        }
    }

    fn members(n: usize) -> Vec<PathBuf> {
        (0..n)
            .map(|i| PathBuf::from(format!("/tmp/m{i}.sock")))
            .collect()
    }

    #[test]
    fn a_post_when_idle_starts_a_sequence_and_is_answered_by_it() {
        let mut g = Genid::new(0, None);
        assert!(g.post(waiter(1)), "an idle POST starts a sequence");
        assert!(g.take_start_requested());
        g.start(&members(2));
        assert!(g.sequence_running());
        assert!(!g.member_done(true), "one member still owes an answer");
        assert!(g.member_done(true), "the last member finishes the sequence");
        let f = g.finish();
        assert_eq!(f.id, 1);
        assert_eq!(f.covered, vec![waiter(1)], "the POST names the ID it made");
        assert!(!f.again);
        assert_eq!(f.failed, 0);
    }

    #[test]
    fn a_post_during_a_sequence_is_answered_by_the_next_one() {
        let mut g = Genid::new(0, None);
        assert!(g.post(waiter(1)));
        g.start(&members(1));
        assert!(
            !g.post(waiter(2)),
            "a POST during a sequence is parked, not answered by it"
        );
        assert!(g.member_done(true));
        let first = g.finish();
        assert_eq!(first.covered, vec![waiter(1)]);
        assert!(first.again, "the parked POST asks for one more sequence");

        g.start(&members(1));
        assert!(g.member_done(true));
        let second = g.finish();
        assert_eq!(
            second.covered,
            vec![waiter(2)],
            "the parked POST is answered by the sequence that started after it"
        );
        assert!(!second.again);
    }

    #[test]
    fn finish_advances_by_one() {
        let mut g = Genid::new(41, None);
        g.start(&members(0));
        assert_eq!(g.finish().id, 42);
        g.start(&members(0));
        assert_eq!(g.finish().id, 43);
    }

    #[test]
    fn a_missing_state_file_is_zero() {
        let dir = tempfile::tempdir().expect("a temp dir");
        assert_eq!(
            read_state(&dir.path().join("genid")).expect("absent is 0"),
            0
        );
    }

    #[test]
    fn an_unparsable_state_file_is_an_error() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("genid");
        std::fs::write(&path, "not a number\n").expect("write");
        let e = read_state(&path).expect_err("a non-number is a start error");
        assert!(e.to_string().contains("u64"), "{e}");
    }

    #[test]
    fn write_then_read_round_trips() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("genid");
        write_state(&path, 7).expect("write");
        assert_eq!(read_state(&path).expect("read"), 7);
        assert!(
            !dir.path().join("genid.tmp").exists(),
            "the temporary file is renamed away, not left behind"
        );
    }

    #[test]
    fn peers_replace_the_oldest_and_sort_by_address() {
        let mut g = Genid::new(0, None);
        let now = m6_core::platform::now();
        for i in 0..=PEER_SLOTS {
            let ip: IpAddr = format!("127.0.0.{}", 1 + (i % 250)).parse().unwrap();
            g.saw_peer(ip, now);
        }
        let peers = g.peers(now);
        assert!(peers.len() <= PEER_SLOTS);
        assert!(
            peers.windows(2).all(|w| w[0].0 <= w[1].0),
            "the report is sorted by address"
        );
    }

    // ── The cache node's follower ────────────────────────────────────────────

    fn follower() -> (Follower, Instant) {
        let t0 = m6_core::platform::now();
        (Follower::new(Duration::from_secs(10), t0), t0)
    }

    /// The first heartbeat sets the ID and is not a change: the node held
    /// nothing the new ID makes old.
    #[test]
    fn the_first_answer_sets_the_id_without_a_change() {
        let (mut f, t0) = follower();
        assert!(
            f.heartbeat_due(t0),
            "a new follower owes a heartbeat at once"
        );
        f.sent();
        assert!(
            !f.heartbeat_due(t0),
            "one heartbeat at a time, so a second tick cannot start another"
        );
        assert_eq!(
            f.answered(Ok((7, Duration::from_millis(4))), t0),
            Outcome::First(7)
        );
        assert_eq!(f.current(), Some(7));
        assert!(!f.down());
        assert_eq!(f.rtt(), Some(Duration::from_millis(4)));
        assert_eq!(f.last_ok_s(t0), Some(0));
        let due = t0 + Duration::from_secs(10);
        assert!(!f.heartbeat_due(due - Duration::from_millis(1)));
        assert!(f.heartbeat_due(due));
    }

    /// A failed heartbeat with no ID is due again on the next tick, not after
    /// the configured interval.
    ///
    /// Break: always schedule the retry a heartbeat away, and a node that
    /// missed its first answer at `heartbeat_secs = 3600` cannot learn an ID
    /// for an hour.
    #[test]
    fn a_failed_heartbeat_with_no_id_retries_on_the_next_tick() {
        let t0 = m6_core::platform::now();
        let mut f = Follower::new(Duration::from_secs(3600), t0);
        assert!(f.heartbeat_due(t0));
        f.sent();
        assert_eq!(
            f.answered(Err("timed out"), t0 + Duration::from_secs(5)),
            Outcome::Down("timed out")
        );
        assert_eq!(f.current(), None);
        assert!(
            f.heartbeat_due(t0 + Duration::from_secs(6)),
            "a node with no ID must retry on the next tick"
        );
    }

    /// Once the node holds an ID, a failed heartbeat waits the interval: it can
    /// still serve what it holds, and there is no reason to retry sooner.
    #[test]
    fn a_failed_heartbeat_with_an_id_waits_the_interval() {
        let (mut f, t0) = follower();
        f.answered(Ok((7, Duration::ZERO)), t0);
        f.sent();
        let failed_at = t0 + Duration::from_secs(1);
        f.answered(Err("timed out"), failed_at);
        assert!(
            !f.heartbeat_due(failed_at + Duration::from_secs(9)),
            "the configured interval starts at the failure"
        );
        assert!(f.heartbeat_due(failed_at + Duration::from_secs(10)));
    }

    /// A different ID is the change, and the ID is the one just heard.
    #[test]
    fn a_new_id_is_a_change() {
        let (mut f, t0) = follower();
        assert_eq!(f.answered(Ok((7, Duration::ZERO)), t0), Outcome::First(7));
        assert_eq!(
            f.answered(Ok((7, Duration::ZERO)), t0),
            Outcome::Same,
            "the same ID is not a change"
        );
        assert_eq!(
            f.answered(Ok((8, Duration::ZERO)), t0),
            Outcome::Changed { from: 7, to: 8 }
        );
        assert_eq!(f.current(), Some(8));
    }

    /// A failure is logged once on the transition, then says nothing.
    #[test]
    fn a_failure_marks_down_once() {
        let (mut f, t0) = follower();
        assert_eq!(
            f.answered(Err("connect failed"), t0),
            Outcome::Down("connect failed")
        );
        assert!(f.down());
        assert_eq!(f.current(), None, "a failure never sets an ID");
        assert_eq!(
            f.answered(Err("timed out"), t0),
            Outcome::StillDown,
            "a second failure is the same fact, not a new one"
        );
        // A forwarded request failing is the same fact from another source,
        // and it makes the next heartbeat due without touching the one in
        // flight.
        assert!(!f.failed(t0), "already down, so it is not a transition");
        let mut g = Follower::new(Duration::from_secs(10), t0);
        g.answered(Ok((7, Duration::ZERO)), t0);
        g.sent();
        assert!(g.failed(t0 + Duration::from_secs(1)), "the first failure");
        assert!(
            !g.heartbeat_due(t0 + Duration::from_secs(1)),
            "the heartbeat already in flight still owns the slot"
        );
        assert!(g.down());
    }

    /// The first answer after an outage clears down, and the ID it already held
    /// is not a change.
    #[test]
    fn a_success_after_down_clears_it() {
        let (mut f, t0) = follower();
        f.answered(Ok((7, Duration::ZERO)), t0);
        f.answered(Err("status"), t0);
        assert!(f.down());
        assert_eq!(
            f.answered(Ok((7, Duration::ZERO)), t0),
            Outcome::Same,
            "a success after down is not a change when the ID is unchanged"
        );
        assert!(!f.down());
    }

    /// The store rule: the current ID or nothing. No ID yet, no header, another
    /// ID and an unparsable one all refuse.
    ///
    /// Break: accept an absent header, and the absent case comes back `Some`.
    #[test]
    fn may_store_needs_the_current_id() {
        let (mut f, t0) = follower();
        assert_eq!(
            f.may_store(Some(b"7")),
            None,
            "no current ID stores nothing"
        );
        f.answered(Ok((7, Duration::ZERO)), t0);
        assert_eq!(f.may_store(None), None, "an absent header stores nothing");
        assert_eq!(f.may_store(Some(b"8")), None, "another ID stores nothing");
        assert_eq!(
            f.may_store(Some(b"nope")),
            None,
            "a bad header stores nothing"
        );
        assert_eq!(f.may_store(Some(b"7")), Some(7));
        assert_eq!(
            f.may_store(Some(b" 7 ")),
            Some(7),
            "surrounding whitespace is not part of the number"
        );
    }

    /// A reply under another ID is the upstream saying the node is behind, so
    /// the next heartbeat is due now rather than at the end of the interval.
    #[test]
    fn behind_makes_the_heartbeat_due_now() {
        let (mut f, t0) = follower();
        f.answered(Ok((7, Duration::ZERO)), t0);
        let later = t0 + Duration::from_secs(1);
        assert!(!f.heartbeat_due(later));
        f.behind(later);
        assert!(f.heartbeat_due(later));
    }

    // ── The settle of a disk load held from start ────────────────────────────

    /// The first answer naming the ID the files carried installs the load.
    ///
    /// Break: return `Discard` for the matching ID, and the restarted node
    /// throws away a cache whose site has not changed.
    #[test]
    fn settle_keeps_the_same_id() {
        assert_eq!(settle(7, &Outcome::First(7)), Settle::Keep);
    }

    /// The first answer naming another ID drops the load: the site changed
    /// while the node was down.
    ///
    /// Break: return `Keep` for every `First`, and this test fails.
    #[test]
    fn settle_discards_another_id() {
        assert_eq!(settle(7, &Outcome::First(8)), Settle::Discard);
        assert_eq!(settle(7, &Outcome::First(0)), Settle::Discard);
    }

    /// No answer installs the load unverified: the site serves from disk until
    /// the source answers.
    ///
    /// Break: return `Discard` for `Down`, and a node whose source is down
    /// starts with no pages at all.
    #[test]
    fn settle_installs_an_unanswered_upstream() {
        assert_eq!(
            settle(7, &Outcome::Down("connect failed")),
            Settle::Unverified
        );
        assert_eq!(settle(7, &Outcome::Down("no M6-Genid")), Settle::Unverified);
    }

    /// An unverified install holds the file's ID, and the first success clears
    /// the flag. A `Same` answer confirms the copy without a flush, and a
    /// `Changed` one reports the change the caller flushes.
    ///
    /// Break: clear `unverified` in `install_unverified`, and the node claims
    /// an upstream has confirmed the copy before one answered.
    #[test]
    fn an_unverified_install_is_cleared_by_the_first_success() {
        let (mut f, t0) = follower();
        assert_eq!(f.current(), None, "nothing is installed before a load");
        f.install_unverified(7);
        assert_eq!(f.current(), Some(7));
        assert!(f.unverified());
        assert_eq!(
            f.answered(Ok((7, Duration::ZERO)), t0),
            Outcome::Same,
            "the disk ID is the ID, so an unchanged site is not a change"
        );
        assert!(!f.unverified(), "a success confirms the copy");
        assert!(!f.down());

        f.install_unverified(9);
        assert_eq!(
            f.answered(Ok((10, Duration::ZERO)), t0),
            Outcome::Changed { from: 9, to: 10 },
            "a changed site is the usual change, from the disk ID"
        );
        assert!(!f.unverified(), "a change clears the flag too");
    }
}
