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
use std::time::Instant;

use crate::forward::Waiter;

/// The header that carries a response's generation to a client.
pub const GENID_HEADER: &str = "M6-Genid";

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
}
