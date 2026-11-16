//! `email::Smtp`'s allocation proof, on the main thread so the count is this
//! binary's own.
//!
//! `harness = false` and one `libtest-mimic` trial: the counting allocator is
//! installed before any thread. The scripted SMTP peer is a non-blocking
//! `m6_core::platform::net::TcpStream` accepted on the same `Poll` as the
//! slot, and every reply it sends was made before counting. The counts are
//! read just before and after each `start` and `drive` call, so the peer's own
//! work is outside them.
//!
//! `Message::formatted()`, `Response::from_str` and the owned `Address` clones
//! that `Mail::new` and `Rcpt::new` take are counted here. The slot's own
//! buffers, steps and command bytes must allocate nothing. The check prints
//! each number and fails when the slot's count is not zero.

#![allow(
    clippy::disallowed_methods,
    reason = "test code: the peer is a non-blocking socket driven by the same poll"
)]

use std::io::{Read, Write};
use std::time::Duration;

use libtest_mimic::{Arguments, Trial};
use m6_core::email::{Relay, RelayConfig, Smtp};
use m6_core::platform::net::TcpStream;
use m6_core::platform::{Events, Interest, Poll, Token};
use m6_core::testkit::alloc_count;

#[global_allocator]
static ALLOC: alloc_count::Counting = alloc_count::Counting;

const LISTENER: Token = Token(2);
const PEER: Token = Token(1);
const SMTP: Token = Token(0);

/// The replies the peer sends, one per expected client step.
const EHLO_REPLY: &[u8] = b"250 test\r\n";
const MAIL_REPLY: &[u8] = b"250 ok\r\n";
const RCPT_REPLY: &[u8] = b"250 ok\r\n";
const DATA_REPLY: &[u8] = b"354 go\r\n";
const END_REPLY: &[u8] = b"250 queued\r\n";

fn main() {
    let mut args = Arguments::from_args();
    args.test_threads = Some(1);
    let tests = vec![Trial::test(
        "a_plain_delivery_allocates_nothing_but_lettre",
        || {
            let (slot, formatted, replies, clones, start, drive) =
                a_plain_delivery_allocates_nothing_but_lettre();
            println!(
                "allocations: slot {slot}, lettre formatted() {formatted}, lettre address clones {clones}, lettre reply parses {replies} (start {start}, drive {drive})"
            );
            if slot == 0 {
                Ok(())
            } else {
                Err(format!("the slot allocated {slot} time(s)").into())
            }
        },
    )];
    libtest_mimic::run(&args, tests).exit();
}

/// The peer's per-connection state, all stacks and fixed buffers.
struct Peer {
    sock: Option<TcpStream>,
    buf: [u8; 8192],
    len: usize,
    step: usize,
}

impl Peer {
    fn new() -> Peer {
        Peer {
            sock: None,
            buf: [0; 8192],
            len: 0,
            step: 0,
        }
    }

    /// Read what arrived and answer the step the bytes complete. Never
    /// allocates: the reply bytes are `'static` and the buffer is fixed.
    fn turn(&mut self, poll: &Poll) {
        let Some(sock) = self.sock.as_mut() else {
            return;
        };
        loop {
            if self.len == self.buf.len() {
                return;
            }
            match sock.read(&mut self.buf[self.len..]) {
                Ok(0) => {
                    self.close(poll);
                    return;
                }
                Ok(n) => self.len += n,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(_) => {
                    self.close(poll);
                    return;
                }
            }
        }
        loop {
            if self.step == 4 {
                let Some(end) = self.buf[..self.len]
                    .windows(5)
                    .position(|w| w == b"\r\n.\r\n")
                else {
                    return;
                };
                let _ = end;
                self.reply(END_REPLY, poll);
                self.step = 5;
                return;
            }
            let Some(end) = self.buf[..self.len].windows(2).position(|w| w == b"\r\n") else {
                return;
            };
            let line = &self.buf[..end];
            let reply = match self.step {
                0 if line.starts_with(b"EHLO") => EHLO_REPLY,
                1 if line.starts_with(b"MAIL") => MAIL_REPLY,
                2 if line.starts_with(b"RCPT") => RCPT_REPLY,
                3 if line.starts_with(b"DATA") => DATA_REPLY,
                _ => return,
            };
            self.buf.copy_within(end + 2..self.len, 0);
            self.len -= end + 2;
            self.step += 1;
            self.reply(reply, poll);
        }
    }

    fn reply(&mut self, bytes: &'static [u8], poll: &Poll) {
        let Some(sock) = self.sock.as_mut() else {
            return;
        };
        match sock.write(bytes) {
            Ok(n) if n == bytes.len() => {}
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(_) => self.close(poll),
        }
    }

    fn close(&mut self, poll: &Poll) {
        if let Some(mut sock) = self.sock.take() {
            let _ = poll.registry().deregister(&mut sock);
        }
    }
}

fn a_plain_delivery_allocates_nothing_but_lettre() -> (usize, usize, usize, usize, usize, usize) {
    let mut listener =
        m6_core::net::listen_tcp("127.0.0.1:0".parse().expect("addr")).expect("listen");
    let addr = listener.local_addr().expect("addr");
    let mut poll = Poll::new().expect("poll");
    poll.registry()
        .register(&mut listener, LISTENER, Interest::READABLE)
        .expect("register listener");
    let mut peer = Peer::new();
    let relay = Relay::new(&RelayConfig {
        host: "127.0.0.1".into(),
        port: addr.port(),
        starttls: None,
        credentials: None,
        timeout: Duration::from_secs(10),
    })
    .expect("relay");
    let msg = message("hello\r\n");

    // One warm-up delivery on the same slot: the address Strings take their
    // capacity here, out of the count.
    let mut smtp = Smtp::new(16 * 1024, 16 * 1024);
    let _ = deliver(
        &relay,
        &msg,
        &mut smtp,
        &mut poll,
        &mut listener,
        &mut peer,
        false,
    );

    // lettre's own allocations, measured once each.
    let before = alloc_count::now();
    let bytes = msg.formatted();
    let formatted = alloc_count::now() - before;
    drop(bytes);
    // `Mail::new` and `Rcpt::new` take owned addresses. Cloning each addressed
    // envelope value is the allocation the owner accepted, and the check
    // measures it here.
    let before = alloc_count::now();
    let _ = msg.envelope().from().cloned();
    for to in msg.envelope().to() {
        let _ = to.clone();
    }
    let clones = alloc_count::now() - before;
    let before = alloc_count::now();
    for reply in [
        "220 test\r\n",
        "250 test\r\n",
        "250 ok\r\n",
        "250 ok\r\n",
        "354 go\r\n",
        "250 queued\r\n",
    ] {
        let _ = reply.parse::<lettre::transport::smtp::response::Response>();
    }
    let replies = alloc_count::now() - before;

    let msg = message("hello\r\n");
    let (start, drive) = deliver(
        &relay,
        &msg,
        &mut smtp,
        &mut poll,
        &mut listener,
        &mut peer,
        true,
    );
    let slot = start.saturating_sub(formatted + clones) + drive.saturating_sub(replies);
    (slot, formatted, replies, clones, start, drive)
}

/// Start the message and drive it to its end. With `count`, the `start` and
/// every `drive` call are measured; the peer's turns in between are not.
fn deliver(
    relay: &Relay,
    msg: &lettre::Message,
    smtp: &mut Smtp,
    poll: &mut Poll,
    listener: &mut m6_core::platform::net::TcpListener,
    peer: &mut Peer,
    count: bool,
) -> (usize, usize) {
    let before = alloc_count::now();
    smtp.start(relay, msg).expect("start");
    let mut start = alloc_count::now() - before;
    if !count {
        start = 0;
    }
    poll.registry()
        .register_fd(
            smtp.fd().expect("a started delivery has a socket"),
            SMTP,
            smtp.interest(),
        )
        .expect("register slot");
    let mut interest = smtp.interest();
    let mut drive = 0usize;
    let mut events = Events::with_capacity(16);
    let deadline = m6_core::platform::now() + Duration::from_secs(30);
    loop {
        poll.poll(&mut events, Some(Duration::from_millis(100)))
            .expect("poll");
        assert!(
            m6_core::platform::now() < deadline,
            "the delivery did not finish"
        );
        for ev in &events {
            match ev.token() {
                LISTENER => {
                    let (stream, _) = listener.accept().expect("accept");
                    stream.set_nodelay(true).expect("nodelay");
                    peer.sock = Some(stream);
                    peer.len = 0;
                    peer.step = 0;
                    peer.reply(b"220 test\r\n", poll);
                    let sock = peer.sock.as_mut().expect("peer");
                    poll.registry()
                        .register(sock, PEER, Interest::READABLE)
                        .expect("register peer");
                }
                PEER => peer.turn(poll),
                SMTP => {
                    // A slot that is no longer sending ignores its token.
                    if smtp.fd().is_none() {
                        continue;
                    }
                    let before = alloc_count::now();
                    let done = smtp.drive().expect("drive");
                    if count {
                        drive += alloc_count::now() - before;
                    }
                    if done.is_some() {
                        poll.registry()
                            .deregister_fd(smtp.fd().expect("a started delivery has a socket"))
                            .expect("deregister slot");
                        smtp.close();
                        return (start, drive);
                    }
                    if smtp.interest() != interest {
                        interest = smtp.interest();
                        poll.registry()
                            .reregister_fd(
                                smtp.fd().expect("a started delivery has a socket"),
                                SMTP,
                                interest,
                            )
                            .expect("reregister slot");
                    }
                }
                _ => {}
            }
        }
    }
}

fn message(body: &str) -> m6_core::email::lettre::Message {
    m6_core::email::lettre::Message::builder()
        .from("Sender <from@example.com>".parse().expect("from"))
        .to("To <to@example.com>".parse().expect("to"))
        .subject("a subject")
        .body(body.to_owned())
        .expect("message")
}
