//! Accepted work, and the shutdown that waits for it.
//!
//! A service's loop starts counting a connection the moment the listener
//! accepts it, not when the request in it is read. A shutdown that waits only
//! for work already being served leaves the request that was accepted and not
//! yet read unanswered, which is the defect issue #338 measured: 57 failures in
//! 150 loaded runs of the m6-html shutdown test.
//!
//! [`Drain`] is the count and the deadline in one place, so the four services
//! whose loops share [`crate::h1::ServerConn`] make the same decision with the
//! same code. A loop calls [`Drain::begin`] at the accept and [`Drain::end`]
//! when that connection's work is finished: after its last response is written
//! and it is idle, or when it closes. [`Drain::start`] takes the stop, and
//! [`Drain::step`] decides each turn whether the loop may leave.
//!
//! # The deadline, and why nothing waits on a condvar
//!
//! The drain this restores (aabd31e, merged in 50c580be, removed in 6310c3a9)
//! counted work handed to a thread pool, and the accept thread slept on a
//! condvar that the last worker signalled. A service is one event loop now
//! (2f4b831a), so the loop is the only thing that can finish a request. A wait
//! on a condvar would stop the work it was waiting for, so the loop runs on,
//! polls for at most the time left on the deadline, and asks [`Drain::step`]
//! after every turn. The count falling to zero ends the drain on that turn,
//! which is the promptness the condvar used to provide.
//!
//! # The request path
//!
//! One add on the accept path, one subtract when the connection is finished,
//! and one flag write per connection. No lock and no syscall, which is issue
//! #338's done-when for the request path.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// What a loop does after a drain turn.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Step {
    /// The drain is waiting for accepted work.
    Draining,
    /// Every accepted request has finished. The loop leaves now.
    Finished,
    /// The deadline passed with this many requests left. The loop leaves now
    /// and says so, so a deploy never overruns the supervisor's stop timeout.
    DeadlinePassed(usize),
}

/// Accepted requests, and the deadline a shutdown gives them.
///
/// One per loop, made at start with the table's first size. `begin` and `end`
/// are keyed by connection slot: the flag for a slot makes a release happen
/// once, and lets a request that arrives later on a kept-alive connection be
/// counted again.
pub struct Drain {
    /// How long shutdown waits, from `[server] drain_timeout_s`.
    timeout: Duration,
    /// When the wait ends, set when the stop arrives.
    until: Option<Instant>,
    in_flight: AtomicUsize,
    /// Whether each connection slot holds accepted work. One byte per slot.
    counted: Vec<bool>,
    /// Set when the stop arrived with nothing in flight. The loop gets one
    /// non-blocking turn before it may leave, so a request already waiting in
    /// a kept-alive connection's socket is read and answered rather than
    /// closed under the client.
    idle_turn: bool,
}

impl Drain {
    /// A drain that waits up to `timeout` for `slots` connections.
    pub fn new(timeout: Duration, slots: usize) -> Drain {
        Drain {
            timeout,
            until: None,
            in_flight: AtomicUsize::new(0),
            counted: vec![false; slots],
            idle_turn: false,
        }
    }

    /// Room for `slots` connections. Called when the connection table doubles,
    /// before the new slots are handed out.
    pub fn grown(&mut self, slots: usize) {
        self.counted.resize(slots, false);
    }

    /// A connection the listener accepted holds work from now on.
    ///
    /// Called at the accept, before a request is read, because a request that
    /// was accepted and not yet read is work the service owes an answer.
    /// Calling it twice for one slot adds once.
    pub fn begin(&mut self, key: usize) {
        if !self.counted[key] {
            self.counted[key] = true;
            self.in_flight.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// The connection's accepted work is finished.
    ///
    /// Called when its last response is written and it holds nothing else, and
    /// on every close: a connection whose peer went away is finished too.
    /// Calling it twice for one slot subtracts once.
    pub fn end(&mut self, key: usize) {
        if self.counted[key] {
            self.counted[key] = false;
            self.in_flight.fetch_sub(1, Ordering::SeqCst);
        }
    }

    /// Accepted work that has not finished.
    pub fn in_flight(&self) -> usize {
        self.in_flight.load(Ordering::SeqCst)
    }

    /// Whether the stop has arrived and the loop is waiting for accepted work.
    pub fn draining(&self) -> bool {
        self.until.is_some()
    }

    /// Take the stop. Returns true when nothing was in flight, in which case
    /// the loop still gets one non-blocking turn ([`Self::poll_timeout`] returns
    /// zero once) before [`Self::step`] lets it leave.
    ///
    /// The count is per accepted connection, and a kept-alive connection that
    /// has answered a request is released. A request the client sent after that
    /// and that is already in the kernel's receive buffer is accepted work the
    /// service still owes an answer, but no `begin` has run for it yet, so the
    /// count alone cannot see it. The turn is what gives its socket the chance
    /// to report.
    pub fn start(&mut self, now: Instant) -> bool {
        self.until = Some(now + self.timeout);
        let idle = self.in_flight() == 0;
        self.idle_turn = idle;
        idle
    }

    /// The longest the loop may wait while draining. A wait capped at the
    /// deadline is what makes the deadline hold when nothing else wakes the
    /// loop.
    ///
    /// The one turn after a stop that found nothing in flight waits zero, so
    /// the loop asks for readiness without blocking and then leaves if no
    /// socket reported.
    pub fn poll_timeout(&mut self, now: Instant) -> Option<Duration> {
        if self.idle_turn {
            self.idle_turn = false;
            return Some(Duration::ZERO);
        }
        self.until.map(|until| until.saturating_duration_since(now))
    }

    /// After a turn: whether the drain is over.
    pub fn step(&mut self, now: Instant) -> Step {
        let left = self.in_flight();
        if left == 0 {
            // One turn for a request already waiting in a socket. `poll_timeout`
            // consumes the flag when it gives that turn its zero wait, so this
            // branch runs once before the next `step` may finish.
            if self.idle_turn {
                return Step::Draining;
            }
            return Step::Finished;
        }
        self.idle_turn = false;
        match self.until {
            Some(until) if now >= until => Step::DeadlinePassed(left),
            _ => Step::Draining,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Long enough that a test only leaves the drain because the last request
    /// finished, never because the deadline passed.
    const DRAIN_LONG: Duration = Duration::from_secs(86_400);

    /// A connection the listener accepted is in flight at once, whether or not
    /// a request has been read from it. The wrong outcome: the count rises when
    /// the request is dispatched, so a shutdown that arrives between the accept
    /// and the read sees nothing in flight and exits under the request.
    #[test]
    fn accepted_work_is_in_flight_before_the_loop_serves_it() {
        let mut drain = Drain::new(DRAIN_LONG, 2);
        drain.begin(0);
        drain.begin(1);
        assert_eq!(drain.in_flight(), 2);

        let now = crate::platform::now();
        assert!(
            !drain.start(now),
            "two accepted connections must hold the drain open"
        );
        assert_eq!(drain.step(now), Step::Draining);

        drain.end(0);
        drain.end(1);
        assert_eq!(drain.in_flight(), 0);
        assert_eq!(drain.step(now), Step::Finished);
    }

    /// The drain ends on the turn the last request finishes, not on a tick and
    /// not at its deadline. The wrong outcome: a deploy waits out the deadline
    /// after the work is done. With a one-day deadline, finishing at all means
    /// the count ended it.
    #[test]
    fn drain_returns_when_the_last_request_finishes() {
        let mut drain = Drain::new(DRAIN_LONG, 1);
        drain.begin(0);
        let now = crate::platform::now();
        assert!(!drain.start(now));
        assert_eq!(drain.step(now + Duration::from_secs(1)), Step::Draining);

        drain.end(0);
        assert_eq!(
            drain.step(now + Duration::from_secs(1)),
            Step::Finished,
            "a day before the deadline, the finished request must end the drain"
        );
    }

    /// A request that never finishes cannot hold shutdown past the deadline.
    /// The wrong outcome: the drain waits for ever and the supervisor kills the
    /// service mid-response. The step reports how many were left behind.
    #[test]
    fn drain_gives_up_at_its_deadline_and_counts_what_is_left() {
        let mut drain = Drain::new(Duration::from_millis(20), 2);
        drain.begin(0);
        drain.begin(1);
        let now = crate::platform::now();
        assert!(!drain.start(now));
        assert_eq!(
            drain.step(now + Duration::from_millis(20)),
            Step::DeadlinePassed(2)
        );
    }

    /// Nothing accepted means shutdown is one non-blocking turn: the loop asks
    /// for readiness without waiting, then leaves. The turn is what sees a
    /// request already sitting in a kept-alive socket.
    #[test]
    fn nothing_in_flight_stops_after_one_nonblocking_turn() {
        let mut drain = Drain::new(DRAIN_LONG, 1);
        let now = crate::platform::now();
        assert!(drain.start(now));
        assert_eq!(
            drain.step(now),
            Step::Draining,
            "the zero-in-flight stop gets one turn"
        );
        assert_eq!(
            drain.poll_timeout(now),
            Some(Duration::ZERO),
            "that turn must not block"
        );
        assert_eq!(drain.step(now), Step::Finished);
    }

    /// A request that arrives on a kept-alive connection after the stop is
    /// counted in the turn and holds the drain open. The wrong outcome: the
    /// stop finds zero and the process closes the socket without reading the
    /// request the kernel already holds.
    #[test]
    fn a_request_waiting_at_the_stop_holds_the_drain_open() {
        let mut drain = Drain::new(DRAIN_LONG, 1);
        let now = crate::platform::now();
        assert!(drain.start(now));
        assert_eq!(drain.step(now), Step::Draining);
        // The non-blocking turn sees the ready socket and counts it.
        assert_eq!(drain.poll_timeout(now), Some(Duration::ZERO));
        drain.begin(0);
        assert_eq!(drain.step(now), Step::Draining);
        drain.end(0);
        assert_eq!(drain.step(now), Step::Finished);
    }

    /// A request that arrives on a kept-alive connection during the drain is
    /// work the service accepted, so it is counted again and answered. The
    /// wrong outcome: the connection was already released as idle, so the
    /// drain leaves before the second request is served.
    #[test]
    fn a_request_on_a_kept_alive_connection_is_counted_again() {
        let mut drain = Drain::new(DRAIN_LONG, 1);
        drain.begin(0);
        let now = crate::platform::now();
        assert!(!drain.start(now));
        drain.end(0);
        assert_eq!(drain.step(now), Step::Finished);

        // The same connection sends a second request: the loop counts it
        // again before serving it, and the drain waits for it too.
        drain.begin(0);
        assert_eq!(drain.step(now), Step::Draining);
        drain.end(0);
        assert_eq!(drain.step(now), Step::Finished);
    }
}
