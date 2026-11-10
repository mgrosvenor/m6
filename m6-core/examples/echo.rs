//! A TCP echo server on the loop parts every m6 service copies.
//!
//! Usage: echo [ADDR] (default 127.0.0.1:7000). ECHO_IDLE_MS sets the idle
//! timeout (default 10000). The connection table starts with ECHO_CONNS slots
//! (default 1024), doubles when full, never shrinks, and stops at ECHO_MAX_CONNS
//! (default: the open-file limit). Growing is the one place the run loop
//! allocates, and the counting allocator proves it makes no other allocation.
//!
//! This is the approved loop shape every service copies: `mio` for the reactor
//! and sockets, and `m6_core::event` for signals, timeouts and the slot table.

#![deny(unsafe_code)]

use std::io::{self, Read, Write};
use std::time::{Duration, Instant};

use m6_core::event::{raise_open_file_limit, Signal, Signals, Slots, Timeouts};
use m6_core::mio::event::Event;
use m6_core::mio::net::{TcpListener, TcpStream};
use m6_core::mio::unix::SourceFd;
use m6_core::mio::{Events, Interest, Poll, Token};

#[global_allocator]
static ALLOC: m6_core::testkit::alloc_count::Counting = m6_core::testkit::alloc_count::Counting;

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
    let initial = env_or("ECHO_CONNS", 1024) as usize;
    let ceiling = env_or("ECHO_MAX_CONNS", raise_open_file_limit()? as u64) as usize;

    let mut poll = Poll::new()?;
    let mut listener = TcpListener::bind(addr)?;
    let mut signals = Signals::new()?;
    poll.registry()
        .register(&mut listener, LISTENER, Interest::READABLE)?;
    poll.registry().register(
        &mut SourceFd(&signals.raw_fd()),
        SIGNALS,
        Interest::READABLE,
    )?;

    // The whole connection table, every buffer in it, the free list, the
    // timeouts and the events: allocated here, never grown except by `Slots`.
    let mut slots = Slots::new(initial, ceiling, Conn::empty);
    let mut idle_timeouts = Timeouts::new(initial, idle);
    let mut events = Events::with_capacity(EVENTS);
    let mut accepting = true;
    eprintln!(
        "listening on {}, {} connection slots",
        listener.local_addr()?,
        initial
    );
    let allocated_at_start = m6_core::testkit::alloc_count::now();
    let mut growth_allocations = 0;

    // ---- run: nothing blocks, nothing allocates ----
    'run: loop {
        // An interrupted wait (a debugger or tracer attaching, a stop and
        // continue) is not an error: wait again. mio passes EINTR up rather
        // than retrying.
        match poll.poll(
            &mut events,
            Some(idle_timeouts.timeout(Instant::now(), TICK)),
        ) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
        let now = Instant::now();
        for ev in &events {
            match ev.token() {
                // NEW CONNECTIONS: take a free slot for each, register it,
                // start its idle clock.
                LISTENER => {
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
                            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                                // No connection was pending after all: give the
                                // slot back rather than leaking it.
                                slots.free(taken.key);
                                break;
                            }
                            Err(e) => return Err(e),
                        };
                        stream.set_nodelay(true)?; // as the C server: no Nagle delay on 1-byte replies
                        poll.registry().register(
                            &mut stream,
                            Token(taken.key),
                            Interest::READABLE,
                        )?;
                        idle_timeouts.touch(Token(taken.key), now);
                        let c = slots.get_mut(taken.key);
                        (c.stream, c.start, c.end, c.eof, c.interest) =
                            (Some(stream), 0, 0, false, Interest::READABLE);
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
                SIGNALS => match signals.read() {
                    Some(Signal::Terminate) => break 'run,
                    Some(Signal::ReopenLogs) | None => {}
                },
                // SERVICE: echo what arrived, then wait for the right thing
                // next, or remove.
                Token(k) => {
                    let c = slots.get_mut(k);
                    match service(c, ev) {
                        Some(next) => {
                            if next != c.interest {
                                if let Some(s) = &mut c.stream {
                                    poll.registry().reregister(s, Token(k), next)?;
                                }
                                c.interest = next;
                            }
                            idle_timeouts.touch(Token(k), now);
                        }
                        None => close(&poll, &mut slots, &mut idle_timeouts, k)?,
                    }
                }
            }
        }
        // REMOVAL ON TIMEOUT: connections idle past their deadline.
        while let Some(Token(k)) = idle_timeouts.expired(Instant::now()) {
            close(&poll, &mut slots, &mut idle_timeouts, k)?;
        }
        // A slot came free while the table was full: accept again.
        if !accepting && !slots.is_full() {
            poll.registry()
                .register(&mut listener, LISTENER, Interest::READABLE)?;
            accepting = true;
        }
    }

    // ---- stop: blocking and allocation are fine ----
    let outside_growth =
        m6_core::testkit::alloc_count::now() - allocated_at_start - growth_allocations;
    let open = open_count(&slots);
    eprintln!(
        "stopping, {open} connection(s) open, {} slots, {outside_growth} allocation(s) during the run outside table growth",
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
    // mio is edge-triggered: the poll reports a socket once per change, not
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

/// The number of open connections, so the stopping line can report it.
fn open_count(slots: &Slots<Conn>) -> usize {
    (0..slots.len())
        .filter(|k| slots.get(*k).stream.is_some())
        .count()
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
