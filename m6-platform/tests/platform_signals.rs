//! The platform layer's signal contract: a shutdown or reload request arrives
//! once through the descriptor, a burst coalesces and loses nothing, a request
//! sent before the first poll is delivered, a spawned child still dies on
//! SIGTERM, and the process survives every signal it handles.
//!
//! Its own `main` (`harness = false`), because the signal mask must be set
//! before any thread exists and the libtest harness starts one first. The
//! same checks run unchanged on every supported target. `OS-1-TESTS.md`
//! section 2 names each case and the wrong implementation that must fail it.

use std::process::{Command, Stdio};
use std::time::Duration;

use m6_platform::{self as platform, process, Events, Interest, Poll, Signal, Signals, Token};

const SIGNALS: Token = Token(7);

fn main() {
    m6_platform::block();
    let mut signals = Signals::new().expect("Signals::new");
    let mut poll = Poll::new().expect("Poll::new");
    poll.registry()
        .register_fd(signals.raw_fd(), SIGNALS, Interest::READABLE)
        .expect("register the signal descriptor");
    let mut events = Events::with_capacity(8);

    sigterm_is_one_shutdown_request(&mut poll, &mut events, &mut signals);
    sigint_is_a_shutdown_request(&mut poll, &mut events, &mut signals);
    sigusr1_is_a_reload_request(&mut poll, &mut events, &mut signals);
    a_burst_coalesces_and_none_is_lost(&mut poll, &mut events, &mut signals);
    a_signal_sent_before_the_poll_is_delivered(&mut signals);
    a_spawned_child_still_dies_on_sigterm();
    the_process_survives_every_signal_it_handles(&mut poll, &mut events, &mut signals);
    signals_new_refuses_an_unblocked_mask();
    println!("platform_signals: 8 checks ... ok");
}

/// Everything pending on the descriptor after one wake, with a deadline.
fn drain(
    poll: &mut Poll,
    events: &mut Events,
    signals: &mut Signals,
    wait: Duration,
) -> Vec<Signal> {
    let mut got = Vec::new();
    poll.poll(events, Some(wait)).expect("poll");
    if events.iter().any(|ev| ev.token() == SIGNALS) {
        while let Some(s) = signals.read() {
            got.push(s);
        }
    }
    got
}

fn me() -> u32 {
    std::process::id()
}

/// `kill -INT`, which `platform::send` does not offer because no service sends
/// an interrupt. A named binary, present on every supported target.
fn kill_int(pid: u32) {
    let status = Command::new("kill")
        .args(["-INT", &pid.to_string()])
        .status()
        .expect("kill runs");
    assert!(status.success(), "kill -INT {pid}: {status}");
}

// S1. Break: no event, or the request twice.
fn sigterm_is_one_shutdown_request(poll: &mut Poll, events: &mut Events, signals: &mut Signals) {
    platform::send(me(), Signal::Shutdown).expect("send");
    let got = drain(poll, events, signals, Duration::from_secs(2));
    assert_eq!(got, [Signal::Shutdown], "S1: one shutdown request");
    println!("S1 sigterm_is_one_shutdown_request ... ok");
}

// S2. Break: SIGINT not in the mask, which ends the process here.
fn sigint_is_a_shutdown_request(poll: &mut Poll, events: &mut Events, signals: &mut Signals) {
    kill_int(me());
    let got = drain(poll, events, signals, Duration::from_secs(2));
    assert_eq!(got, [Signal::Shutdown], "S2: SIGINT is a shutdown request");
    println!("S2 sigint_is_a_shutdown_request ... ok");
}

// S3. Break: SIGUSR1 mapped to Shutdown.
fn sigusr1_is_a_reload_request(poll: &mut Poll, events: &mut Events, signals: &mut Signals) {
    platform::send(me(), Signal::ReopenLogs).expect("send");
    let got = drain(poll, events, signals, Duration::from_secs(2));
    assert_eq!(got, [Signal::ReopenLogs], "S3: SIGUSR1 is a reload request");
    println!("S3 sigusr1_is_a_reload_request ... ok");
}

// S4. Break: none delivered.
fn a_burst_coalesces_and_none_is_lost(poll: &mut Poll, events: &mut Events, signals: &mut Signals) {
    for _ in 0..3 {
        platform::send(me(), Signal::ReopenLogs).expect("send");
    }
    let got = drain(poll, events, signals, Duration::from_secs(2));
    assert!(
        (1..=3).contains(&got.len()) && got.iter().all(|s| *s == Signal::ReopenLogs),
        "S4: a burst of three reloads coalesces to between one and three: {got:?}"
    );
    println!("S4 a_burst_coalesces_and_none_is_lost ... ok");
}

// S5. Break: a signal posted before the first poll is lost. A fresh poll, so
// the descriptor was never waited on before the signal was sent.
fn a_signal_sent_before_the_poll_is_delivered(signals: &mut Signals) {
    platform::send(me(), Signal::Shutdown).expect("send");
    let mut poll = Poll::new().expect("Poll::new");
    poll.registry()
        .register_fd(signals.raw_fd(), SIGNALS, Interest::READABLE)
        .expect("register");
    let mut events = Events::with_capacity(8);
    let got = drain(&mut poll, &mut events, signals, Duration::from_secs(2));
    assert_eq!(got, [Signal::Shutdown], "S5: delivered on the first poll");
    println!("S5 a_signal_sent_before_the_poll_is_delivered ... ok");
}

// S6. Break: `process::spawn` replaced by `Command::spawn`, so the child
// inherits the blocked mask across exec, ignores the SIGTERM and sleeps out
// its 30 s before exiting 0.
fn a_spawned_child_still_dies_on_sigterm() {
    use std::os::unix::process::ExitStatusExt;
    let mut child = process::spawn(
        Command::new("bash")
            .args(["-c", "echo up; exec sleep 30"])
            .stdout(Stdio::piped()),
    )
    .expect("spawn a child");
    // The child has exec'd sleep once its one line has been written.
    let mut line = String::new();
    std::io::BufRead::read_line(
        &mut std::io::BufReader::new(child.stdout.take().expect("stdout")),
        &mut line,
    )
    .expect("read the child's line");
    assert_eq!(line, "up\n");
    platform::send(child.id(), Signal::Shutdown).expect("send to the child");
    let status = child.wait().expect("wait");
    assert_eq!(
        status.signal(),
        Some(15),
        "S6: the child must die of SIGTERM, not inherit an ignored disposition: {status}"
    );
    println!("S6 a_spawned_child_still_dies_on_sigterm ... ok");
}

// S7. Break: a default disposition left in place, which would have ended the
// process in S1 to S5.
fn the_process_survives_every_signal_it_handles(
    poll: &mut Poll,
    events: &mut Events,
    signals: &mut Signals,
) {
    let got = drain(poll, events, signals, Duration::from_millis(100));
    assert!(got.is_empty(), "S7: nothing left pending: {got:?}");
    assert!(
        signals.read().is_none(),
        "S7: the descriptor drains to None"
    );
    println!("S7 the_process_survives_every_signal_it_handles ... ok");
}

// S8. Break: the check removed. The mask is unblocked first so the assertion
// is the thing that fails, then restored.
fn signals_new_refuses_an_unblocked_mask() {
    platform::unblock_for_test();
    let r = std::panic::catch_unwind(|| {
        let _ = Signals::new();
    });
    m6_platform::block();
    let payload = r.expect_err("S8: Signals::new must refuse an unblocked mask");
    let msg = payload
        .downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_default();
    assert!(
        msg.contains("must be the first statement of main"),
        "S8: the refusal names signal::block: {msg}"
    );
    println!("S8 signals_new_refuses_an_unblocked_mask ... ok");
}
