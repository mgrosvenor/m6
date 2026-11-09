//! SIGUSR1 and SIGTERM arrive on the descriptor, and the process stays alive.
//!
//! This binary has its own `main` (`harness = false`) so `signal::block()` runs
//! before any thread, as `log_reopen` does. The check proves the kqueue arm on
//! macOS and the `signalfd` arm on Linux deliver both signals through
//! [`Signals`] and leave the process running past their default dispositions.

use std::time::Duration;

use m6_core::event::{Signal, Signals};
use m6_core::mio::unix::SourceFd;
use m6_core::mio::{Events, Interest, Poll, Token};

fn main() {
    sigusr1_and_sigterm_arrive_on_the_descriptor();
    println!("sigusr1_and_sigterm_arrive_on_the_descriptor ... ok");
}

fn sigusr1_and_sigterm_arrive_on_the_descriptor() {
    m6_core::signal::block();
    let mut signals = Signals::new().expect("Signals::new");
    let mut poll = Poll::new().expect("Poll::new");
    const TOKEN: Token = Token(0);
    poll.registry()
        .register(&mut SourceFd(&signals.raw_fd()), TOKEN, Interest::READABLE)
        .expect("register the signal descriptor");

    // SAFETY: `kill` takes a pid and a signal number and touches no memory.
    unsafe { libc::kill(libc::getpid(), libc::SIGUSR1) };
    unsafe { libc::kill(libc::getpid(), libc::SIGTERM) };

    let mut events = Events::with_capacity(8);
    let mut got = Vec::new();
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while std::time::Instant::now() < deadline {
        poll.poll(&mut events, Some(Duration::from_millis(50)))
            .expect("poll");
        let reported = events.iter().any(|ev| ev.token() == TOKEN);
        if reported {
            while let Some(s) = signals.read() {
                got.push(s);
            }
            if got.len() >= 2 {
                break;
            }
        }
    }

    // The order is not asserted: signalfd orders by signal number and kqueue by
    // delivery, so a strict order would flake on one platform.
    assert!(
        got.contains(&Signal::ReopenLogs),
        "SIGUSR1 must arrive: {got:?}"
    );
    assert!(
        got.contains(&Signal::Terminate),
        "SIGTERM must arrive: {got:?}"
    );
    assert!(
        signals.read().is_none(),
        "the descriptor drains to None, not a spin: {got:?}"
    );
    // Reaching here, past the default dispositions of both signals, proves the
    // process is still alive.
}
