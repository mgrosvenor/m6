//! The parts a service's own event loop is built from.
//!
//! The reactor and the signal descriptor are [`crate::platform`]'s. This
//! module adds [`Timeouts`] (deadlines that never allocate), [`Slots`] (a
//! connection table that doubles when full and never shrinks) and
//! [`LoopTrace`] (wait time against work time per trigger).
//!
//! The loop these parts serve follows six rules. Start and stop may block
//! freely. The loop never blocks. One thread runs the loop. The poll is
//! edge-triggered, so every turn reads and writes until `WouldBlock`. The loop
//! allocates nothing (table growth excepted). A service's loop uses no
//! `unsafe`.

use std::time::{Duration, Instant};

use crate::platform::Token;

// ── Timeouts ─────────────────────────────────────────────────────────────────

const NONE: usize = usize::MAX;

/// Deadlines for up to `capacity` tokens that all share one duration (an idle
/// timeout, an upstream timeout). Every slot is allocated at start. Touching a
/// token moves it to the back of a list kept in deadline order, so setting,
/// clearing and finding the next expiry are all O(1) and never allocate. A
/// service with two durations keeps two of these.
pub struct Timeouts {
    after: Duration,
    at: Vec<Option<Instant>>,
    prev: Vec<usize>,
    next: Vec<usize>,
    head: usize,
    tail: usize,
}

impl Timeouts {
    pub fn new(capacity: usize, after: Duration) -> Timeouts {
        Timeouts {
            after,
            at: vec![None; capacity],
            prev: vec![NONE; capacity],
            next: vec![NONE; capacity],
            head: NONE,
            tail: NONE,
        }
    }

    /// Room for `capacity` tokens. Existing deadlines are kept.
    pub fn grow(&mut self, capacity: usize) {
        self.at.resize(capacity, None);
        self.prev.resize(capacity, NONE);
        self.next.resize(capacity, NONE);
    }

    /// The token's deadline becomes `now + after`, replacing any earlier one.
    pub fn touch(&mut self, token: Token, now: Instant) {
        self.clear(token);
        let k = token.0;
        self.at[k] = Some(now + self.after);
        self.prev[k] = self.tail;
        self.next[k] = NONE;
        if self.tail == NONE {
            self.head = k;
        } else {
            self.next[self.tail] = k;
        }
        self.tail = k;
    }

    pub fn clear(&mut self, token: Token) {
        let k = token.0;
        if self.at[k].take().is_none() {
            return;
        }
        let (p, n) = (self.prev[k], self.next[k]);
        if p == NONE {
            self.head = n;
        } else {
            self.next[p] = n;
        }
        if n == NONE {
            self.tail = p;
        } else {
            self.prev[n] = p;
        }
    }

    /// Time until the earliest deadline, capped at `tick`.
    pub fn timeout(&self, now: Instant, tick: Duration) -> Duration {
        match self.head {
            NONE => tick,
            k => self.at[k].map_or(tick, |at| at.saturating_duration_since(now).min(tick)),
        }
    }

    /// One token whose deadline has passed, removed from the list, or `None`.
    pub fn expired(&mut self, now: Instant) -> Option<Token> {
        let k = self.head;
        if k == NONE || self.at[k].is_none_or(|at| at > now) {
            return None;
        }
        self.clear(Token(k));
        Some(Token(k))
    }
}

// ── Slots ────────────────────────────────────────────────────────────────────

/// A connection table of `T`. `initial` slots are made at once with `make`,
/// each with its own buffers. The table doubles when full, never shrinks, and
/// stops at `ceiling`. A slot is not dropped when freed: its buffers stay for
/// the next connection, and the caller resets its fields.
pub struct Slots<T> {
    items: Vec<T>,
    free: Vec<usize>,
    ceiling: usize,
    make: fn() -> T,
}

/// A slot handed out by [`Slots::take`], with whether the table grew.
#[derive(Debug, PartialEq, Eq)]
pub struct Taken {
    pub key: usize,
    /// The new length, only when the table doubled to make room.
    pub grew_to: Option<usize>,
}

impl<T> Slots<T> {
    /// `initial` slots made at once with `make`, each with its own buffers.
    pub fn new(initial: usize, ceiling: usize, make: fn() -> T) -> Slots<T> {
        Slots {
            items: (0..initial).map(|_| make()).collect(),
            free: (0..initial).rev().collect(),
            ceiling,
            make,
        }
    }

    /// A free slot. Doubles the table (up to `ceiling`) when none is free: the
    /// one allocation the loop may make. `None` at the ceiling.
    pub fn take(&mut self) -> Option<Taken> {
        if self.free.is_empty() {
            if self.items.len() >= self.ceiling {
                return None;
            }
            let old = self.items.len();
            let new = (old * 2).min(self.ceiling);
            self.items.extend((old..new).map(|_| (self.make)()));
            self.free.reserve_exact(new - self.free.len());
            self.free.extend((old..new).rev());
            return Some(Taken {
                key: self.free.pop()?,
                grew_to: Some(new),
            });
        }
        Some(Taken {
            key: self.free.pop()?,
            grew_to: None,
        })
    }

    pub fn free(&mut self, key: usize) {
        self.free.push(key);
    }

    pub fn get(&self, key: usize) -> &T {
        &self.items[key]
    }

    pub fn get_mut(&mut self, key: usize) -> &mut T {
        &mut self.items[key]
    }

    #[allow(clippy::len_without_is_empty)] // the table never shrinks, so it is never empty after construction
    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_full(&self) -> bool {
        self.free.is_empty() && self.items.len() >= self.ceiling
    }
}

// ── LoopTrace ────────────────────────────────────────────────────────────────

/// How many triggers a loop may name.
const MAX_TRIGGERS: usize = 16;
/// Samples kept per trigger.
const RING: usize = 256;

/// The summary of one trigger since the last [`LoopTrace::report`].
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct TriggerReport {
    pub name: &'static str,
    pub count: u64,
    pub wait_min_ns: u64,
    pub wait_max_ns: u64,
    pub wait_mean_ns: u64,
    pub work_min_ns: u64,
    pub work_max_ns: u64,
    pub work_mean_ns: u64,
}

/// Wait time against work time, per trigger, reported on the accounting tick.
///
/// `worked` takes a trigger index rather than a name, and `report` returns a
/// fixed array rather than a `String`, so the loop never allocates and never
/// compares strings.
pub struct LoopTrace {
    triggers: &'static [&'static str],
    wait_ring: [[u64; RING]; MAX_TRIGGERS],
    work_ring: [[u64; RING]; MAX_TRIGGERS],
    ring_idx: [usize; MAX_TRIGGERS],
    counts: [u64; MAX_TRIGGERS],
    last_waited_ns: u64,
    reports: [TriggerReport; MAX_TRIGGERS],
    report_len: usize,
}

impl LoopTrace {
    /// The service's trigger names, fixed at start (at most 16).
    pub fn new(triggers: &'static [&'static str]) -> LoopTrace {
        assert!(
            triggers.len() <= MAX_TRIGGERS,
            "at most {MAX_TRIGGERS} triggers"
        );
        LoopTrace {
            triggers,
            wait_ring: [[0; RING]; MAX_TRIGGERS],
            work_ring: [[0; RING]; MAX_TRIGGERS],
            ring_idx: [0; MAX_TRIGGERS],
            counts: [0; MAX_TRIGGERS],
            last_waited_ns: 0,
            reports: [TriggerReport {
                name: "",
                count: 0,
                wait_min_ns: 0,
                wait_max_ns: 0,
                wait_mean_ns: 0,
                work_min_ns: 0,
                work_max_ns: 0,
                work_mean_ns: 0,
            }; MAX_TRIGGERS],
            report_len: 0,
        }
    }

    /// How long the last `poll` waited.
    pub fn waited(&mut self, d: Duration) {
        self.last_waited_ns = d.as_nanos() as u64;
    }

    /// How long the work for one trigger took, `trigger` an index into the
    /// start-time list. Pairs its sample with the most recent `waited`.
    pub fn worked(&mut self, trigger: usize, d: Duration) {
        let pos = self.ring_idx[trigger];
        self.wait_ring[trigger][pos] = self.last_waited_ns;
        self.work_ring[trigger][pos] = d.as_nanos() as u64;
        self.ring_idx[trigger] = (pos + 1) % RING;
        self.counts[trigger] += 1;
    }

    /// Count, min, max and mean per trigger since the last call, then cleared.
    pub fn report(&mut self) -> &[TriggerReport] {
        for t in 0..self.triggers.len() {
            let count = self.counts[t];
            let occupied = count.min(RING as u64) as usize;
            let (mut wmin, mut wmax, mut wsum) = (u64::MAX, 0u64, 0u64);
            let (mut kmin, mut kmax, mut ksum) = (u64::MAX, 0u64, 0u64);
            for i in 0..occupied {
                let w = self.wait_ring[t][i];
                let k = self.work_ring[t][i];
                wmin = wmin.min(w);
                wmax = wmax.max(w);
                wsum += w;
                kmin = kmin.min(k);
                kmax = kmax.max(k);
                ksum += k;
            }
            if occupied == 0 {
                wmin = 0;
                wmax = 0;
                kmin = 0;
                kmax = 0;
            }
            self.reports[t] = TriggerReport {
                name: self.triggers[t],
                count,
                wait_min_ns: wmin,
                wait_max_ns: wmax,
                wait_mean_ns: if occupied == 0 {
                    0
                } else {
                    wsum / occupied as u64
                },
                work_min_ns: kmin,
                work_max_ns: kmax,
                work_mean_ns: if occupied == 0 {
                    0
                } else {
                    ksum / occupied as u64
                },
            };
            self.counts[t] = 0;
            self.ring_idx[t] = 0;
        }
        self.report_len = self.triggers.len();
        &self.reports[..self.report_len]
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // Timeouts.

    #[test]
    fn touch_then_expired_in_deadline_order() {
        let mut t = Timeouts::new(4, Duration::from_secs(10));
        let now = Instant::now();
        t.touch(Token(0), now);
        t.touch(Token(1), now);
        t.touch(Token(2), now);
        let later = now + Duration::from_secs(11);
        assert_eq!(t.expired(later), Some(Token(0)));
        assert_eq!(t.expired(later), Some(Token(1)));
        assert_eq!(t.expired(later), Some(Token(2)));
        assert_eq!(t.expired(later), None);
    }

    #[test]
    fn clear_removes_from_the_middle_head_and_tail() {
        let mut t = Timeouts::new(4, Duration::from_secs(10));
        let now = Instant::now();
        let later = now + Duration::from_secs(11);

        t.touch(Token(0), now);
        t.touch(Token(1), now);
        t.touch(Token(2), now);
        t.clear(Token(1)); // the middle
        assert_eq!(t.expired(later), Some(Token(0)));
        assert_eq!(t.expired(later), Some(Token(2)));
        assert_eq!(t.expired(later), None);

        let mut t = Timeouts::new(4, Duration::from_secs(10));
        t.touch(Token(0), now);
        t.touch(Token(1), now);
        t.touch(Token(2), now);
        t.clear(Token(0)); // the head
        assert_eq!(t.expired(later), Some(Token(1)));
        assert_eq!(t.expired(later), Some(Token(2)));
        assert_eq!(t.expired(later), None);

        let mut t = Timeouts::new(4, Duration::from_secs(10));
        t.touch(Token(0), now);
        t.touch(Token(1), now);
        t.touch(Token(2), now);
        t.clear(Token(2)); // the tail
        assert_eq!(t.expired(later), Some(Token(0)));
        assert_eq!(t.expired(later), Some(Token(1)));
        assert_eq!(t.expired(later), None);
    }

    #[test]
    fn timeout_is_capped_at_the_tick() {
        let mut t = Timeouts::new(4, Duration::from_secs(100));
        let now = Instant::now();
        t.touch(Token(0), now);
        let tick = Duration::from_secs(1);
        assert_eq!(t.timeout(now, tick), tick);
    }

    #[test]
    fn grow_keeps_existing_deadlines() {
        let mut t = Timeouts::new(2, Duration::from_secs(10));
        let now = Instant::now();
        t.touch(Token(0), now);
        t.touch(Token(1), now);
        t.grow(4);
        t.touch(Token(3), now);
        let later = now + Duration::from_secs(11);
        assert_eq!(t.expired(later), Some(Token(0)));
        assert_eq!(t.expired(later), Some(Token(1)));
        assert_eq!(t.expired(later), Some(Token(3)));
        assert_eq!(t.expired(later), None);
    }

    // Slots.

    #[test]
    fn take_hands_out_every_initial_slot_then_doubles() {
        let mut s = Slots::new(2, 8, || 0u8);
        assert_eq!(s.take().map(|t| t.key), Some(0));
        assert_eq!(s.take().map(|t| t.key), Some(1));
        let third = s.take().expect("grows");
        assert_eq!(third.key, 2);
        assert_eq!(third.grew_to, Some(4));
        assert_eq!(s.len(), 4);
    }

    #[test]
    fn the_ceiling_stops_growth_and_take_returns_none() {
        let mut s = Slots::new(2, 2, || 0u8);
        assert_eq!(s.take().map(|t| t.key), Some(0));
        assert_eq!(s.take().map(|t| t.key), Some(1));
        assert_eq!(s.take(), None);
    }

    #[test]
    fn a_freed_slot_is_reused_before_growing() {
        let mut s = Slots::new(2, 8, || 0u8);
        assert_eq!(s.take().map(|t| t.key), Some(0));
        assert_eq!(s.take().map(|t| t.key), Some(1));
        s.free(0);
        let reused = s.take().expect("reused");
        assert_eq!(reused.key, 0);
        assert_eq!(reused.grew_to, None);
        assert_eq!(s.len(), 2);
    }

    // LoopTrace.

    #[test]
    fn the_trace_summarises_and_clears() {
        let mut trace = LoopTrace::new(&["t"]);
        trace.waited(Duration::ZERO);
        trace.worked(0, Duration::from_millis(1));
        trace.worked(0, Duration::from_millis(2));
        trace.worked(0, Duration::from_millis(3));
        let r = trace.report();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].name, "t");
        assert_eq!(r[0].count, 3);
        assert_eq!(r[0].work_min_ns, Duration::from_millis(1).as_nanos() as u64);
        assert_eq!(r[0].work_max_ns, Duration::from_millis(3).as_nanos() as u64);
        assert_eq!(
            r[0].work_mean_ns,
            Duration::from_millis(2).as_nanos() as u64
        );
        let r = trace.report();
        assert_eq!(r[0].count, 0);
    }

    #[test]
    fn the_ring_keeps_the_newest_256() {
        let mut trace = LoopTrace::new(&["t"]);
        trace.waited(Duration::ZERO);
        for _ in 0..10 {
            trace.worked(0, Duration::from_millis(9));
        }
        for _ in 0..300 {
            trace.worked(0, Duration::from_millis(1));
        }
        let r = trace.report();
        assert_eq!(r[0].count, 310);
        assert_eq!(r[0].work_max_ns, Duration::from_millis(1).as_nanos() as u64);
        assert_eq!(
            r[0].work_mean_ns,
            Duration::from_millis(1).as_nanos() as u64
        );
    }
}
