//! A TCP echo server: the reference loop every m6 service follows.
//!
//! Usage: `echo [ADDR]` (default 127.0.0.1:7000). ECHO_IDLE_MS sets the idle
//! timeout (default 10000) and ECHO_DRAIN_MS how long a shutdown waits for
//! open connections to finish (default 1000). The connection table starts
//! with ECHO_CONNS slots (default 1024), doubles when full, never shrinks, and
//! stops at ECHO_MAX_CONNS (default: the open-file limit). Growing is the one
//! place the run loop allocates, and the counting allocator proves it makes no
//! other allocation.
//!
//! Built on `m6_core::platform` alone, which is the only code that knows the
//! operating system: `Poll` for readiness, `Signals` for a shutdown or reload
//! request, `net` for the listener and `now` for the clock. The shape:
//!
//! 1. Start, where blocking is fine: block the signals, raise the open-file
//!    limit, make the listener, the poll, the signal descriptor, the slot
//!    table, the deadlines and the event buffer.
//! 2. Run, where nothing blocks and nothing allocates: wait, read the clock
//!    once, then one `match` on each event's token. Accept until `WouldBlock`.
//!    On a connection, read and write until `WouldBlock`, then re-read what it
//!    waits for next and reregister only on a change. A shutdown request stops
//!    accepting and drains, a reload request is logged.
//! 3. Stop, where blocking is fine: report and exit 0.

#![deny(unsafe_code)]

use std::io::{self, Read, Write};
use std::time::{Duration, Instant};

use m6_core::event::{Slots, Timeouts};
use m6_core::platform::net::{self, listen_tcp, TcpStream};
use m6_core::platform::process::raise_fd_limit;
use m6_core::platform::{self, Event, Events, Interest, Poll, Signal, Signals, Token};

#[global_allocator]
static ALLOC: m6_core::alloc::M6Alloc = m6_core::alloc::M6Alloc;

const LISTENER: Token = Token(usize::MAX);
const SIGNALS: Token = Token(usize::MAX - 1);
const TICK: Duration = Duration::from_secs(1);
const EVENTS: usize = 256;
/// Each connection's buffer: bytes read and not yet written back. When it is
/// full the connection stops reading until the client takes some back.
const BUF: usize = 16 * 1024;

struct Conn {
    stream: Option<TcpStream>, // None while the slot is free
    buf: Box<[u8]>,            // BUF bytes, allocated once at start
    start: usize,              // buf[start..end] is owed to the client
    end: usize,
    eof: bool,          // the client has finished sending
    interest: Interest, // what the poll waits for on this connection
}

fn main() -> io::Result<()> {
    // Block the signals before anything else, as every m6 service does: the
    // mask is inherited only by threads created after this point.
    m6_core::signal::block();

    // ---- start: blocking and allocation are fine ----
    let addr = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "127.0.0.1:7000".into());
    let addr = addr
        .parse()
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    let idle = Duration::from_millis(env_or("ECHO_IDLE_MS", 10_000));
    let drain = Duration::from_millis(env_or("ECHO_DRAIN_MS", 1_000));
    let initial = env_or("ECHO_CONNS", 1024) as usize;
    let ceiling = env_or("ECHO_MAX_CONNS", raise_fd_limit()? as u64) as usize;

    let mut poll = Poll::new()?;
    let mut listener = listen_tcp(addr)?;
    // The reserve the accept loop spends to refuse a connection when the
    // process is out of descriptors. Opened here, before any pressure, and on
    // the listener's own socket family.
    let mut spare = m6_core::platform::net::SpareDescriptor::for_tcp(listener.local_addr()?)?;
    let mut refusals = m6_core::net::Refusals::new();
    let mut signals = Signals::new()?;
    poll.registry()
        .register(&mut listener, LISTENER, Interest::READABLE)?;
    poll.registry()
        .register_fd(signals.raw_fd(), SIGNALS, Interest::READABLE)?;

    // The whole connection table, every buffer in it, the free list, the
    // timeouts and the events: allocated here, never grown except by `Slots`.
    let mut slots = Slots::new(initial, ceiling, Conn::empty);
    let mut idle_timeouts = Timeouts::new(initial, idle);
    let mut events = Events::with_capacity(EVENTS);
    let mut accepting = true;
    let mut turns: u64 = 0;
    let mut open = 0usize;
    // Set by a shutdown request: the time to give up on open connections.
    let mut drain_until: Option<Instant> = None;
    eprintln!(
        "listening on {}, {} connection slots",
        listener.local_addr()?,
        initial
    );
    let allocated_at_start = m6_core::testkit::alloc_count::now();
    let mut growth_allocations = 0;
    let mut now = platform::now();

    // ---- run: nothing blocks, nothing allocates ----
    'run: loop {
        let mut timeout = idle_timeouts.timeout(now, TICK);
        if let Some(until) = drain_until {
            timeout = timeout.min(until.saturating_duration_since(now));
        }
        // An interrupted wait (a debugger or tracer attaching, a stop and
        // continue) is not an error: wait again.
        match poll.poll(&mut events, Some(timeout)) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
        // The turn's one time: every deadline below sees the same clock.
        now = platform::now();
        turns += 1;
        for ev in &events {
            match ev.token() {
                // NEW CONNECTIONS: take a free slot for each, register it,
                // start its idle clock.
                LISTENER => {
                    let mut served = false;
                    loop {
                        let before = m6_core::testkit::alloc_count::now();
                        let old_len = slots.len();
                        let Some(taken) = slots.take() else { break };
                        if let Some(new_len) = taken.grew_to {
                            idle_timeouts.grow(new_len);
                            eprintln!("connection table grew from {old_len} to {new_len} slots");
                            growth_allocations += m6_core::testkit::alloc_count::now() - before;
                        }
                        let mut stream = match listener.accept() {
                            Ok((stream, _)) => stream,
                            // No connection was pending after all: give the
                            // slot back rather than leaking it.
                            Err(e) => match net::accept_error(&e) {
                                net::Accept::Drained => {
                                    slots.free(taken.key);
                                    break;
                                }
                                net::Accept::SkipOne => {
                                    slots.free(taken.key);
                                    continue;
                                }
                                // Out of descriptors: refuse the connection
                                // that is already queued by spending the
                                // reserve, so it is not left waiting for a
                                // wakeup that a full queue cannot deliver.
                                // Then keep draining.
                                net::Accept::Exhausted => {
                                    slots.free(taken.key);
                                    match spare.reject_one(|| {
                                        let (stream, _) = listener.accept()?;
                                        drop(stream);
                                        Ok(())
                                    }) {
                                        Ok(()) => {
                                            refusals.refused();
                                            continue;
                                        }
                                        Err(e) => {
                                            // The reserve is gone, so the next
                                            // shortage cannot be handled. Arm
                                            // the listener and stop draining.
                                            eprintln!(
                                                "the reserve descriptor could not be reopened: {e}"
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
                                net::Accept::Stop => {
                                    slots.free(taken.key);
                                    eprintln!("echo: accept error: {e}");
                                    poll.registry().reregister(
                                        &mut listener,
                                        LISTENER,
                                        Interest::READABLE,
                                    )?;
                                    break;
                                }
                            },
                        };
                        stream.set_nodelay(true)?; // no Nagle delay on 1-byte replies
                        poll.registry().register(
                            &mut stream,
                            Token(taken.key),
                            Interest::READABLE,
                        )?;
                        idle_timeouts.touch(Token(taken.key), now);
                        open += 1;
                        served = true;
                        let c = slots.get_mut(taken.key);
                        (c.stream, c.start, c.end, c.eof, c.interest) =
                            (Some(stream), 0, 0, false, Interest::READABLE);
                    }
                    let counts = if served {
                        refusals.flush(now)
                    } else {
                        refusals.due(now)
                    };
                    if let Some(counts) = counts {
                        eprintln!(
                            "echo: {} (rejected {}, total {})",
                            m6_core::net::REFUSED_MESSAGE,
                            counts.since_last,
                            counts.total
                        );
                    }
                    if slots.is_full() {
                        // At the ceiling: stop accepting. New clients wait in
                        // the kernel's backlog.
                        poll.registry().deregister(&mut listener)?;
                        accepting = false;
                        eprintln!(
                            "connection table full at {} slots: not accepting",
                            slots.len()
                        );
                    }
                }
                // A SHUTDOWN REQUEST stops accepting and drains what is open,
                // for at most the drain time.
                SIGNALS => {
                    while let Some(request) = signals.read() {
                        match request {
                            Signal::Shutdown if drain_until.is_none() => {
                                if accepting {
                                    poll.registry().deregister(&mut listener)?;
                                    accepting = false;
                                }
                                drain_until = Some(now + drain);
                            }
                            Signal::Shutdown => {}
                        }
                    }
                }
                // SERVICE: echo what arrived, then wait for the right thing
                // next, or remove.
                Token(k) => {
                    let c = slots.get_mut(k);
                    match service(c, ev) {
                        Some(next) => {
                            // Interest is re-read AFTER the handler moved the
                            // connection on, and the kernel is told only on a
                            // change.
                            if next != c.interest {
                                if let Some(s) = &mut c.stream {
                                    poll.registry().reregister(s, Token(k), next)?;
                                }
                                c.interest = next;
                            }
                            idle_timeouts.touch(Token(k), now);
                        }
                        None => {
                            close(&poll, &mut slots, &mut idle_timeouts, k)?;
                            open -= 1;
                        }
                    }
                }
            }
        }
        // REMOVAL ON TIMEOUT: connections idle past their deadline.
        while let Some(Token(k)) = idle_timeouts.expired(now) {
            close(&poll, &mut slots, &mut idle_timeouts, k)?;
            open -= 1;
        }
        // DRAIN: a shutdown ends once everything open has finished, or at the
        // drain deadline.
        if let Some(until) = drain_until {
            if open == 0 || now >= until {
                break 'run;
            }
        }
        // A slot came free while the table was full: accept again.
        if !accepting && drain_until.is_none() && !slots.is_full() {
            poll.registry()
                .register(&mut listener, LISTENER, Interest::READABLE)?;
            accepting = true;
        }
    }

    // ---- stop: blocking and allocation are fine ----
    let outside_growth =
        m6_core::testkit::alloc_count::now() - allocated_at_start - growth_allocations;
    eprintln!(
        "stopping, {open} connection(s) open, {} slots, {turns} turn(s), {outside_growth} allocation(s) during the run outside table growth",
        slots.len()
    );
    Ok(())
}

/// One connection's turn: read what has arrived into its buffer, write back
/// what the socket will take. Returns what to wait for next, or None to close.
fn service(c: &mut Conn, ev: &Event) -> Option<Interest> {
    if ev.is_error() {
        return None;
    }
    let stream = c.stream.as_mut()?;
    // The poll is edge-triggered: it reports a socket once per change, not
    // while data waits. So keep reading and writing until one side would
    // block, or the client is done. Stopping early, with data still in the
    // socket, would never be reported again.
    loop {
        let mut read_blocked = c.eof;
        while !c.eof && c.end < BUF {
            match stream.read(&mut c.buf[c.end..]) {
                Ok(0) => c.eof = true,
                Ok(n) => c.end += n,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    read_blocked = true;
                    break;
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => return None,
            }
        }
        let mut write_blocked = false;
        while c.start < c.end {
            match stream.write(&c.buf[c.start..c.end]) {
                Ok(n) => c.start += n,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    write_blocked = true;
                    break;
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => return None,
            }
        }
        if c.start == c.end {
            (c.start, c.end) = (0, 0); // all sent: the whole buffer is free again
        }
        if read_blocked || write_blocked || c.eof {
            break; // the next change will be reported
        }
    }

    let owed = c.start < c.end;
    if !owed {
        if c.eof {
            None
        } else {
            Some(Interest::READABLE) // all echoed: close, or wait for input
        }
    } else if c.eof || c.end == BUF {
        Some(Interest::WRITABLE) // only drain what is owed
    } else {
        Some(Interest::READABLE | Interest::WRITABLE)
    }
}

/// REMOVAL: deregister, forget the deadline, close the socket, free the slot.
fn close(
    poll: &Poll,
    slots: &mut Slots<Conn>,
    timeouts: &mut Timeouts,
    k: usize,
) -> io::Result<()> {
    timeouts.clear(Token(k));
    if let Some(mut stream) = slots.get_mut(k).stream.take() {
        poll.registry().deregister(&mut stream)?;
        slots.free(k);
    } // `stream` drops here, which closes the socket
    Ok(())
}

impl Conn {
    fn empty() -> Conn {
        Conn {
            stream: None,
            buf: vec![0; BUF].into_boxed_slice(),
            start: 0,
            end: 0,
            eof: false,
            interest: Interest::READABLE,
        }
    }
}

fn env_or(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}
