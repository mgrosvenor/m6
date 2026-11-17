//! The TLS version of the echo server: the same loop, with rustls (the ring
//! provider, as m6-http uses) between the socket and the buffer.
//!
//! Usage: echo_tls ADDR CERT KEY. ECHO_IDLE_MS, ECHO_DRAIN_MS and ECHO_CONNS
//! as echo.
//!
//! The reference loop of `echo.rs`, with TLS in the slot's own buffers, built
//! on `m6_core::platform` alone.

#![deny(unsafe_code)]

use std::io::{self, Read, Write};
use std::sync::Arc;
use std::time::{Duration, Instant};

use m6_core::event::{Slots, Timeouts};
use m6_core::platform::net::{self, listen_tcp, TcpStream};
use m6_core::platform::process::raise_fd_limit;
use m6_core::platform::{self, Event, Events, Interest, Poll, Signal, Signals, Token};
use rustls::ServerConnection;
use rustls_pki_types::pem::PemObject;
use rustls_pki_types::{CertificateDer, PrivateKeyDer};

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
    tls: Option<ServerConnection>,
    buf: Box<[u8]>, // BUF bytes, allocated once at start
    start: usize,   // buf[start..end] is owed to the client
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
    let cert = std::env::args()
        .nth(2)
        .expect("usage: echo_tls ADDR CERT KEY");
    let key = std::env::args()
        .nth(3)
        .expect("usage: echo_tls ADDR CERT KEY");
    let invalid = |e: rustls_pki_types::pem::Error| io::Error::new(io::ErrorKind::InvalidData, e);
    let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_file_iter(&cert)
        .map_err(invalid)?
        .collect::<Result<_, _>>()
        .map_err(invalid)?;
    let key = PrivateKeyDer::from_pem_file(&key).map_err(invalid)?;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut config = rustls::ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .and_then(|b| b.with_no_client_auth().with_single_cert(certs, key))
        .map_err(io::Error::other)?;
    config.send_tls13_tickets = 0; // as the C server: no session tickets
    let config = Arc::new(config);
    let idle = Duration::from_millis(env_or("ECHO_IDLE_MS", 10_000));
    let drain = Duration::from_millis(env_or("ECHO_DRAIN_MS", 1_000));
    let initial = env_or("ECHO_CONNS", 1024) as usize;
    let ceiling = env_or("ECHO_MAX_CONNS", raise_fd_limit()? as u64) as usize;

    let mut poll = Poll::new()?;
    let mut listener = listen_tcp(addr)?;
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
        match poll.poll(&mut events, Some(timeout)) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
        now = platform::now();
        turns += 1;
        for ev in &events {
            match ev.token() {
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
                                // Any other error: say so once, give the slot back and stop
                                // accepting for this turn.
                                net::Accept::Stop => {
                                    slots.free(taken.key);
                                    eprintln!("echo: accept error: {e}");
                                    break;
                                }
                            },
                        };
                        stream.set_nodelay(true)?;
                        poll.registry().register(
                            &mut stream,
                            Token(taken.key),
                            Interest::READABLE,
                        )?;
                        idle_timeouts.touch(Token(taken.key), now);
                        open += 1;
                        let c = slots.get_mut(taken.key);
                        c.tls =
                            Some(ServerConnection::new(config.clone()).map_err(io::Error::other)?);
                        (c.stream, c.start, c.end, c.eof, c.interest) =
                            (Some(stream), 0, 0, false, Interest::READABLE);
                    }
                    if slots.is_full() {
                        poll.registry().deregister(&mut listener)?;
                        accepting = false;
                        eprintln!(
                            "connection table full at {} slots: not accepting",
                            slots.len()
                        );
                    }
                }
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
                            Signal::ReopenLogs => {
                                io::stderr().write_all(b"log reopen requested\n")?;
                            }
                        }
                    }
                }
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
                        None => {
                            close(&poll, &mut slots, &mut idle_timeouts, k)?;
                            open -= 1;
                        }
                    }
                }
            }
        }
        while let Some(Token(k)) = idle_timeouts.expired(now) {
            close(&poll, &mut slots, &mut idle_timeouts, k)?;
            open -= 1;
        }
        if let Some(until) = drain_until {
            if open == 0 || now >= until {
                break 'run;
            }
        }
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

/// One connection's turn: decrypt what has arrived into the buffer, encrypt
/// it back and send what the socket will take. Edge-triggered, so it keeps
/// going until the socket would block. Returns what to wait for next, or None.
fn service(c: &mut Conn, ev: &Event) -> Option<Interest> {
    if ev.is_error() {
        return None;
    }
    let (Some(stream), Some(tls)) = (c.stream.as_mut(), c.tls.as_mut()) else {
        return None;
    };
    loop {
        // Socket to rustls.
        let mut read_blocked = c.eof;
        while !c.eof && tls.wants_read() {
            match tls.read_tls(stream) {
                Ok(0) => c.eof = true,
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    read_blocked = true;
                    break;
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => return None,
            }
            if tls.process_new_packets().is_err() {
                let _ = tls.write_tls(stream); // send the alert
                return None;
            }
        }
        // rustls plaintext to the buffer.
        while c.end < BUF {
            match tls.reader().read(&mut c.buf[c.end..]) {
                Ok(0) => {
                    c.eof = true; // close_notify
                    break;
                }
                Ok(n) => c.end += n,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
                    c.eof = true;
                    break;
                }
                Err(_) => return None,
            }
        }
        // The buffer back into rustls, encrypted.
        if c.start < c.end {
            match tls.writer().write(&c.buf[c.start..c.end]) {
                Ok(n) => c.start += n,
                Err(_) => return None,
            }
        }
        if c.start == c.end {
            (c.start, c.end) = (0, 0);
        }
        // rustls to the socket.
        let mut write_blocked = false;
        while tls.wants_write() {
            match tls.write_tls(stream) {
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    write_blocked = true;
                    break;
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => return None,
            }
        }
        if read_blocked || write_blocked || c.eof {
            break;
        }
    }
    if c.eof && c.start == c.end && !tls.wants_write() {
        None // all echoed and the client is done
    } else if tls.wants_write() {
        Some(Interest::READABLE | Interest::WRITABLE)
    } else {
        Some(Interest::READABLE)
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
    slots.get_mut(k).tls = None;
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
            tls: None,
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
