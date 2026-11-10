//! One DNS question on its own non-blocking UDP socket, for checking a
//! crawler's claim.
//!
//! [`Confirm`] is the reverse-then-forward check that [`crate::resolve`]'s
//! worker thread makes: resolve the address to a name, then resolve the name
//! forward and require the address among the results. The monitor's own loop
//! (EL-9) will wait on descriptors with a deadline, so a lookup driven by that
//! loop needs no thread. [`Query`] owns one UDP socket connected to one
//! nameserver. Nothing in this module waits.
//!
//! # Three outcomes, and they are not two
//!
//! [`Ptr`] distinguishes a name that was found from a lookup that found nothing
//! and from a lookup that could not be completed. Collapsing the last two into
//! "not verified" would be a small lie in the expensive direction: an address
//! with no PTR record is unverified and may be perfectly honest, while a
//! resolver that timed out says nothing about the address at all.
//!
//! # A PTR record alone proves nothing
//!
//! Whoever controls an address's reverse zone can make it say anything,
//! including `crawl-1-2-3.example.com`. Every crawler operator that documents
//! the method documents the second half too: resolve the name forward and check
//! that the original address is among the results. [`Confirm`] returns
//! [`Ptr::Name`] only when that forward answer contains the address.
//!
//! # One query, one deadline
//!
//! There is no retry and no second nameserver: a question goes to the first
//! `nameserver` in /etc/resolv.conf and its answer is the answer. The caller
//! owns the deadline, and a query dropped at its deadline closes its socket,
//! which is what a timed-out lookup gives the thread version today. A truncated
//! reply is a failure: an A, AAAA or PTR answer for the names a monitor asks
//! about fits in UDP, and a TCP fallback would be a second mechanism.
//!
//! The messages are built and parsed by `hickory-proto`, which allocates. That
//! is at most `MAX_LOOKUPS_PER_POLL` lookups per monitor poll, off the request
//! path of every server.

use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};
use std::os::fd::{AsRawFd, RawFd};
use std::path::Path;

use nix::net::if_::if_nametoindex;

use hickory_proto::op::{Message, MessageType, OpCode, Query as Question, ResponseCode};
use hickory_proto::rr::{Name, RData, RecordType};
use hickory_proto::serialize::binary::{BinDecodable, BinEncodable};
use mio::net::UdpSocket;

/// The first `nameserver` in /etc/resolv.conf, port 53. Start only.
///
/// A file with no nameserver is an error, and so is one that cannot be read:
/// a service that cannot resolve the crawler addresses has nothing to fall back
/// on.
pub fn nameserver() -> io::Result<SocketAddr> {
    nameserver_from(Path::new("/etc/resolv.conf"))
}

/// [`nameserver`] from another file, for tests.
pub fn nameserver_from(path: &Path) -> io::Result<SocketAddr> {
    let config = resolv_conf::Config::parse(std::fs::read(path)?)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let nameserver = config.nameservers.first().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "the resolver configuration has no nameserver",
        )
    })?;
    Ok(match nameserver {
        resolv_conf::ScopedIp::V4(ip) => SocketAddr::V4(SocketAddrV4::new(*ip, 53)),
        // The scope id is part of the address: a link-local nameserver without
        // it cannot be reached, and connect would succeed before send failed.
        resolv_conf::ScopedIp::V6(ip, scope) => {
            SocketAddr::V6(SocketAddrV6::new(*ip, 53, 0, scope_id(scope.as_deref())?))
        }
    })
}

/// The scope id of a scoped IPv6 nameserver: a number, or an interface name
/// resolved once at start.
///
/// An interface name that does not resolve is an error, because a link-local
/// nameserver without its scope is unreachable.
fn scope_id(scope: Option<&str>) -> io::Result<u32> {
    let Some(scope) = scope else {
        return Ok(0);
    };
    if let Ok(id) = scope.parse::<u32>() {
        return Ok(id);
    }
    if_nametoindex(scope).map_err(io::Error::from)
}

/// The answer to one [`Query`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer {
    /// The addresses an A and AAAA question answered.
    Addrs(Vec<IpAddr>),
    /// The name a PTR question answered.
    Name(String),
    /// The name has no record of the kind asked for.
    NotFound,
    /// The resolver could not answer, answered with a failure, or truncated
    /// the reply.
    Failed,
}

/// The largest reply datagram read. A reply without EDNS is at most 512 bytes,
/// which fits the one answer these questions need, and a larger datagram says
/// more than this module asked for.
const MAX_REPLY: usize = 512;

/// One DNS question on its own UDP socket. Never waits.
pub struct Query {
    /// Connected to the nameserver, so the kernel drops datagrams from any
    /// other address.
    socket: UdpSocket,
    /// The reply buffer, allocated with the query and reused for every
    /// datagram it reads.
    buf: Vec<u8>,
    kind: Kind,
    /// Whether the answer has been returned, so a duplicate reply cannot
    /// produce a second one.
    done: bool,
}

/// The questions one query has outstanding, and what has answered so far.
enum Kind {
    /// One PTR question.
    Ptr { id: u16, name: Name },
    /// An A and an AAAA question, sent on the one socket with two ids.
    Addrs {
        a: u16,
        aaaa: u16,
        name: Name,
        a_answered: bool,
        aaaa_answered: bool,
        addrs: Vec<IpAddr>,
    },
}

impl Query {
    /// One PTR question for `ip`.
    pub fn ptr(ip: IpAddr, ns: SocketAddr) -> io::Result<Query> {
        let name = Name::from(ip);
        let id = rand::random();
        let query = Query::new(
            ns,
            Kind::Ptr {
                id,
                name: name.clone(),
            },
        )?;
        query.send(id, &name, RecordType::PTR)?;
        Ok(query)
    }

    /// An A and an AAAA question for `name`, two messages on the one socket.
    pub fn addrs(name: &str, ns: SocketAddr) -> io::Result<Query> {
        let mut name =
            Name::from_ascii(name).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        // The wire form always ends at the root; setting it here too makes the
        // stored name compare equal to the question a reply echoes back.
        name.set_fqdn(true);
        let a = rand::random();
        // Two ids, so a reply can be told apart from the other question's. A
        // collision would make the second reply look like a duplicate of the
        // first and hold the query until its deadline.
        let aaaa = loop {
            let id = rand::random();
            if id != a {
                break id;
            }
        };
        let query = Query::new(
            ns,
            Kind::Addrs {
                a,
                aaaa,
                name: name.clone(),
                a_answered: false,
                aaaa_answered: false,
                addrs: Vec::new(),
            },
        )?;
        query.send(a, &name, RecordType::A)?;
        query.send(aaaa, &name, RecordType::AAAA)?;
        Ok(query)
    }

    /// The descriptor to register READABLE with the caller's poll.
    pub fn fd(&self) -> RawFd {
        self.socket.as_raw_fd()
    }

    /// Read every datagram the socket has. `Some` when the answer is complete.
    ///
    /// An `Err` is a socket error, not an answer: the caller records
    /// [`Ptr::Failed`] and drops the query.
    pub fn drive(&mut self) -> io::Result<Option<Answer>> {
        if self.done {
            return Ok(None);
        }
        loop {
            match self.socket.recv(&mut self.buf) {
                Ok(n) => {
                    if let Some(answer) = self.received(n) {
                        self.done = true;
                        return Ok(Some(answer));
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(None),
                Err(e) => return Err(e),
            }
        }
    }

    /// A socket bound locally and connected to the nameserver.
    fn new(ns: SocketAddr, kind: Kind) -> io::Result<Query> {
        let local = match ns {
            SocketAddr::V4(_) => SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0),
            SocketAddr::V6(_) => SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), 0),
        };
        let socket = UdpSocket::bind(local)?;
        socket.connect(ns)?;
        Ok(Query {
            socket,
            buf: vec![0; MAX_REPLY],
            kind,
            done: false,
        })
    }

    /// One question datagram: recursion desired, one question, a fresh id.
    fn send(&self, id: u16, name: &Name, record_type: RecordType) -> io::Result<()> {
        let mut message = Message::new(id, MessageType::Query, OpCode::Query);
        message.metadata.recursion_desired = true;
        message.add_query(Question::query(name.clone(), record_type));
        let bytes = message
            .to_bytes()
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        self.socket.send(&bytes)?;
        Ok(())
    }

    /// The answer `n` bytes of a received datagram carry, or `None` to ignore
    /// it: a reply whose id or question does not match the outstanding
    /// question is not this query's.
    fn received(&mut self, n: usize) -> Option<Answer> {
        let Ok(message) = Message::from_bytes(&self.buf[..n]) else {
            return Some(Answer::Failed);
        };
        match &mut self.kind {
            Kind::Ptr { id, name } => {
                if message.metadata.id != *id || !asks_about(&message, name, RecordType::PTR) {
                    return None;
                }
                // A truncated reply would need TCP to complete, which is not a
                // second mechanism this lookup takes on. It is a failed lookup
                // instead.
                if message.metadata.truncation {
                    return Some(Answer::Failed);
                }
                Some(match message.metadata.response_code {
                    ResponseCode::NoError => message
                        .answers
                        .iter()
                        .find_map(|record| match &record.data {
                            RData::PTR(name) => Some(Answer::Name(name.to_utf8())),
                            _ => None,
                        })
                        .unwrap_or(Answer::NotFound),
                    ResponseCode::NXDomain => Answer::NotFound,
                    _ => Answer::Failed,
                })
            }
            Kind::Addrs {
                a,
                aaaa,
                name,
                a_answered,
                aaaa_answered,
                addrs,
            } => {
                let record_type = if message.metadata.id == *a {
                    RecordType::A
                } else if message.metadata.id == *aaaa {
                    RecordType::AAAA
                } else {
                    return None;
                };
                if !asks_about(&message, name, record_type) {
                    return None;
                }
                if message.metadata.truncation {
                    return Some(Answer::Failed);
                }
                match message.metadata.response_code {
                    ResponseCode::NoError => {
                        // A duplicate reply must not add the same address twice.
                        let answered = if record_type == RecordType::A {
                            &mut *a_answered
                        } else {
                            &mut *aaaa_answered
                        };
                        if *answered {
                            return None;
                        }
                        *answered = true;
                        for record in &message.answers {
                            match &record.data {
                                RData::A(address) => addrs.push(IpAddr::V4(address.0)),
                                RData::AAAA(address) => addrs.push(IpAddr::V6(address.0)),
                                _ => {}
                            }
                        }
                    }
                    ResponseCode::NXDomain => return Some(Answer::NotFound),
                    _ => return Some(Answer::Failed),
                }
                if *a_answered && *aaaa_answered {
                    Some(if addrs.is_empty() {
                        Answer::NotFound
                    } else {
                        Answer::Addrs(std::mem::take(addrs))
                    })
                } else {
                    None
                }
            }
        }
    }
}

/// Whether `message` echoes the one question asked: its name and record type.
fn asks_about(message: &Message, name: &Name, record_type: RecordType) -> bool {
    let questions = &message.queries;
    questions.len() == 1 && questions[0].name() == name && questions[0].query_type() == record_type
}

/// The PTR-then-forward check that `resolve::confirm` makes today, driven by
/// readiness.
///
/// The PTR step runs first. When it answers a name, the query is replaced by
/// the A and AAAA questions for that name, so [`Confirm::fd`] changes and the
/// caller registers the new descriptor. One descriptor is open at a time.
pub struct Confirm {
    ip: IpAddr,
    ns: SocketAddr,
    /// The step being driven now: the PTR query, then the forward query.
    query: Query,
    /// The PTR name, once the reverse step has answered it.
    name: Option<String>,
}

impl Confirm {
    /// Start the PTR step for `ip`.
    pub fn start(ip: IpAddr, ns: SocketAddr) -> io::Result<Confirm> {
        Ok(Confirm {
            ip,
            ns,
            query: Query::ptr(ip, ns)?,
            name: None,
        })
    }

    /// The descriptor to wait on now. It changes when the PTR step ends.
    pub fn fd(&self) -> RawFd {
        self.query.fd()
    }

    /// Move the current step as far as it goes without waiting. `Some` once,
    /// when the check is complete, then `None` on later calls.
    ///
    /// An `Err` is a socket error: the caller records [`Ptr::Failed`] and drops
    /// the confirmation.
    pub fn drive(&mut self) -> io::Result<Option<Ptr>> {
        let Some(answer) = self.query.drive()? else {
            return Ok(None);
        };
        let result = if let Some(name) = &self.name {
            match answer {
                Answer::Addrs(addrs) => {
                    let want = self.ip.to_canonical();
                    if addrs.iter().any(|addr| addr.to_canonical() == want) {
                        Ptr::Name(name.clone())
                    } else {
                        Ptr::Unconfirmed(name.clone())
                    }
                }
                Answer::NotFound => Ptr::Unconfirmed(name.clone()),
                Answer::Failed | Answer::Name(_) => Ptr::Failed,
            }
        } else {
            match answer {
                Answer::Name(name) => {
                    // A reverse zone is written by whoever holds the address. A
                    // name with characters no host name has is not looked up
                    // forward or reported, so it cannot forge or hide report
                    // lines on any platform.
                    if !is_host_name(&name) {
                        Ptr::Failed
                    } else {
                        self.query = Query::addrs(&name, self.ns)?;
                        self.name = Some(name);
                        return Ok(None);
                    }
                }
                Answer::NotFound => Ptr::None,
                // The PTR question can only answer a name, not found, or
                // failed; the other two arms keep the match total.
                Answer::Failed | Answer::Addrs(_) => Ptr::Failed,
            }
        };
        Ok(Some(result))
    }
}

/// The result of asking what name an address has.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ptr {
    /// A name was returned. From [`Confirm`] it is also forward-confirmed:
    /// resolving it returns the address.
    Name(String),
    /// A PTR name was found and the forward lookup of that name completed
    /// without returning the address. Whoever controls the reverse zone can
    /// publish any name, so this is a false claim.
    Unconfirmed(String),
    /// The lookup completed and the address has no PTR record. Unverified,
    /// which is not the same as false.
    None,
    /// The lookup did not complete in time, or the resolver refused. Says
    /// nothing about the address.
    Failed,
}

impl Ptr {
    /// The name, if there is one.
    pub fn name(&self) -> Option<&str> {
        match self {
            Ptr::Name(n) => Some(n),
            _ => None,
        }
    }

    /// Whether this name supports a claim to be `domain`.
    ///
    /// A suffix match on a dot boundary, so `bot.example.com` matches
    /// `example.com` and `example.com.attacker.net` does not. The trailing dot
    /// a resolver may return is ignored. Case-insensitive, because DNS is.
    ///
    /// This is the check every crawler operator documents, and the dot
    /// boundary is the whole of it: `notexample.com` ends with `example.com`
    /// as a string and is a different domain.
    pub fn verifies(&self, domain: &str) -> bool {
        let Ptr::Name(name) = self else {
            return false;
        };
        let name = name.trim_end_matches('.').to_ascii_lowercase();
        let domain = domain.trim_end_matches('.').to_ascii_lowercase();
        if domain.is_empty() {
            return false;
        }
        name == domain || name.ends_with(&format!(".{domain}"))
    }
}

/// Letters, digits, `-`, `_` and `.`, at most 253 characters once the trailing
/// dot a resolver may return is removed: the shape of a host name.
///
/// `pub(crate)` because `resolve.rs`'s worker thread applies the same check
/// until EL-9 removes that path.
pub(crate) fn is_host_name(name: &str) -> bool {
    let name = name.strip_suffix('.').unwrap_or(name);
    !name.is_empty()
        && name.len() <= 253
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "test code: threads and blocking sockets drive the query from outside"
)]
mod tests {
    use super::*;
    use hickory_proto::rr::rdata::{A, AAAA, PTR};
    use hickory_proto::rr::Record;
    use std::net::TcpListener;
    use std::net::UdpSocket as StdUdpSocket;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    /// A test may block; the query under test may not.
    const T: Duration = Duration::from_secs(5);

    /// An ASCII name a fake nameserver can put on the wire.
    fn name(text: &str) -> Name {
        Name::from_ascii(text).expect("name")
    }

    /// A response to `query` with `rcode` and `answers`, echoing its id and
    /// question.
    fn reply(query: &Message, rcode: ResponseCode, answers: Vec<RData>) -> Message {
        let asked = &query.queries[0];
        let mut message = Message::new(query.metadata.id, MessageType::Response, OpCode::Query);
        message.metadata.recursion_desired = true;
        message.metadata.response_code = rcode;
        for question in &query.queries {
            message.add_query(question.clone());
        }
        for rdata in answers {
            message.add_answer(Record::from_rdata(asked.name().clone(), 60, rdata));
        }
        message
    }

    /// A fake nameserver on `socket`: for each of `questions` question
    /// datagrams it sends the replies `answer` returns.
    fn nameserver_on(
        socket: StdUdpSocket,
        questions: usize,
        mut answer: impl FnMut(&Message) -> Vec<Message> + Send + 'static,
    ) -> (SocketAddr, std::thread::JoinHandle<()>) {
        let addr = socket.local_addr().expect("fake nameserver address");
        // A query that never sends its question must fail the test, not hang
        // it on a join.
        socket
            .set_read_timeout(Some(T))
            .expect("fake nameserver read timeout");
        let handle = std::thread::spawn(move || {
            let mut buf = [0u8; MAX_REPLY];
            for _ in 0..questions {
                let (n, peer) = socket.recv_from(&mut buf).expect("receive a question");
                let query = Message::from_bytes(&buf[..n]).expect("parse the question");
                for message in answer(&query) {
                    let bytes = message.to_bytes().expect("encode the reply");
                    socket.send_to(&bytes, peer).expect("send the reply");
                }
            }
        });
        (addr, handle)
    }

    /// A fake nameserver on a fresh loopback port.
    fn nameserver(
        questions: usize,
        answer: impl FnMut(&Message) -> Vec<Message> + Send + 'static,
    ) -> (SocketAddr, std::thread::JoinHandle<()>) {
        let socket = StdUdpSocket::bind("127.0.0.1:0").expect("bind fake nameserver");
        nameserver_on(socket, questions, answer)
    }

    /// One reply carrying the one PTR record `name`.
    fn ptr_reply(query: &Message, name: &str) -> Vec<Message> {
        let name = Name::from_ascii(name).expect("PTR name");
        vec![reply(
            query,
            ResponseCode::NoError,
            vec![RData::PTR(PTR(name))],
        )]
    }

    /// Drive `query` with a poll until it answers.
    fn driven(query: &mut Query) -> Answer {
        let mut poll = mio::Poll::new().expect("poll");
        let mut events = mio::Events::with_capacity(4);
        poll.registry()
            .register(
                &mut mio::unix::SourceFd(&query.fd()),
                mio::Token(0),
                mio::Interest::READABLE,
            )
            .expect("register the query's socket");
        let deadline = Instant::now() + T;
        loop {
            let now = Instant::now();
            assert!(
                now < deadline,
                "the query did not answer before its deadline"
            );
            poll.poll(&mut events, Some(deadline - now)).expect("poll");
            for event in events.iter() {
                if event.token() == mio::Token(0) {
                    if let Some(answer) = query.drive().expect("drive the query") {
                        return answer;
                    }
                }
            }
        }
    }

    /// Drive `confirm` with a poll until it answers, following the descriptor
    /// from the PTR step to the forward step.
    fn driven_confirm(confirm: &mut Confirm) -> Ptr {
        let mut poll = mio::Poll::new().expect("poll");
        let mut events = mio::Events::with_capacity(4);
        let mut registered = -1;
        let deadline = Instant::now() + T;
        loop {
            let fd = confirm.fd();
            if fd != registered {
                poll.registry()
                    .register(
                        &mut mio::unix::SourceFd(&fd),
                        mio::Token(0),
                        mio::Interest::READABLE,
                    )
                    .expect("register the confirmation's socket");
                registered = fd;
            }
            let now = Instant::now();
            assert!(
                now < deadline,
                "the confirmation did not finish before its deadline"
            );
            poll.poll(&mut events, Some(deadline - now)).expect("poll");
            for event in events.iter() {
                if event.token() == mio::Token(0) {
                    if let Some(ptr) = confirm.drive().expect("drive the confirmation") {
                        return ptr;
                    }
                }
            }
        }
    }

    #[test]
    fn a_ptr_answer_is_returned() {
        let (seen_tx, seen_rx) = mpsc::channel();
        let (addr, server) = nameserver(1, move |query| {
            seen_tx
                .send(query.queries[0].name().to_utf8())
                .expect("report the question");
            ptr_reply(query, "crawler.example.com.")
        });
        let ip = "198.51.100.7".parse().expect("address");
        let mut query = Query::ptr(ip, addr).expect("start the query");
        assert_eq!(
            driven(&mut query),
            Answer::Name("crawler.example.com.".to_string())
        );
        assert_eq!(
            seen_rx.recv().expect("the question the server saw"),
            "7.100.51.198.in-addr.arpa."
        );
        server.join().expect("fake nameserver");
    }

    #[test]
    fn an_a_and_aaaa_answer_is_returned() {
        let (seen_tx, seen_rx) = mpsc::channel();
        let (addr, server) = nameserver(2, move |query| {
            let question = &query.queries[0];
            seen_tx
                .send((question.name().to_utf8(), question.query_type()))
                .expect("report the question");
            let answers = match question.query_type() {
                RecordType::A => vec![RData::A(A("192.0.2.1".parse().expect("address")))],
                RecordType::AAAA => {
                    vec![RData::AAAA(AAAA("2001:db8::1".parse().expect("address")))]
                }
                other => panic!("unexpected question type {other:?}"),
            };
            vec![reply(query, ResponseCode::NoError, answers)]
        });
        let mut query = Query::addrs("crawler.example.com", addr).expect("start the query");
        let mut addrs = match driven(&mut query) {
            Answer::Addrs(addrs) => addrs,
            other => panic!("expected addresses, got {other:?}"),
        };
        addrs.sort();
        assert_eq!(
            addrs,
            vec![
                "192.0.2.1".parse::<IpAddr>().expect("address"),
                "2001:db8::1".parse().expect("address"),
            ]
        );
        let seen = [
            seen_rx.recv().expect("the first question"),
            seen_rx.recv().expect("the second question"),
        ];
        let name = "crawler.example.com.".to_string();
        assert!(seen.contains(&(name.clone(), RecordType::A)));
        assert!(seen.contains(&(name, RecordType::AAAA)));
        server.join().expect("fake nameserver");
    }

    #[test]
    fn a_reply_with_the_wrong_id_is_ignored() {
        let (addr, server) = nameserver(1, |query| {
            let mut wrong_id = reply(
                query,
                ResponseCode::NoError,
                vec![RData::PTR(PTR(
                    Name::from_ascii("wrong.example.com.").expect("name")
                ))],
            );
            wrong_id.metadata.id = query.metadata.id.wrapping_add(1);
            // The same id, with a question this query did not ask, must be
            // ignored by the question half of the check.
            let mut other = query.clone();
            other.queries[0] = Question::query(name("other.example.com."), RecordType::PTR);
            let wrong_question = reply(
                &other,
                ResponseCode::NoError,
                vec![RData::PTR(PTR(Name::from_ascii(
                    "wrong-question.example.com.",
                )
                .expect("name")))],
            );
            let mut replies = vec![wrong_id, wrong_question];
            replies.append(&mut ptr_reply(query, "crawler.example.com."));
            replies
        });
        let ip = "198.51.100.7".parse().expect("address");
        let mut query = Query::ptr(ip, addr).expect("start the query");
        assert_eq!(
            driven(&mut query),
            Answer::Name("crawler.example.com.".to_string())
        );
        server.join().expect("fake nameserver");
    }

    #[test]
    fn nxdomain_is_not_found_and_servfail_is_failed() {
        for (rcode, expected) in [
            (ResponseCode::NXDomain, Answer::NotFound),
            (ResponseCode::ServFail, Answer::Failed),
        ] {
            let (addr, server) = nameserver(1, move |query| vec![reply(query, rcode, Vec::new())]);
            let ip = "198.51.100.7".parse().expect("address");
            let mut query = Query::ptr(ip, addr).expect("start the query");
            assert_eq!(driven(&mut query), expected, "{rcode:?}");
            server.join().expect("fake nameserver");
        }
    }

    #[test]
    fn a_truncated_reply_is_failed() {
        // A TCP listener on the nameserver's port sees any fallback the query
        // would make. UDP and TCP ports are separate, so both can be bound.
        let (udp, tcp) = loop {
            let udp = StdUdpSocket::bind("127.0.0.1:0").expect("bind fake nameserver");
            let addr = udp.local_addr().expect("fake nameserver address");
            if let Ok(tcp) = TcpListener::bind(addr) {
                break (udp, tcp);
            }
        };
        let addr = udp.local_addr().expect("fake nameserver address");
        tcp.set_nonblocking(true).expect("non-blocking listener");
        let (_, server) = nameserver_on(udp, 1, |query| {
            let mut message = reply(query, ResponseCode::NoError, Vec::new());
            message.metadata.truncation = true;
            vec![message]
        });
        let ip = "198.51.100.7".parse().expect("address");
        let mut query = Query::ptr(ip, addr).expect("start the query");
        assert_eq!(driven(&mut query), Answer::Failed);
        assert_eq!(
            tcp.accept()
                .expect_err("no TCP connection is attempted")
                .kind(),
            io::ErrorKind::WouldBlock
        );
        server.join().expect("fake nameserver");
    }

    #[test]
    fn confirm_matches_the_old_rule() {
        let confirmed = confirm_case(
            Some(name("crawl.example.com.")),
            |kind| match kind {
                RecordType::A => (
                    ResponseCode::NoError,
                    vec![
                        RData::A(A("192.0.2.1".parse().expect("address"))),
                        RData::A(A("192.0.2.7".parse().expect("address"))),
                    ],
                ),
                _ => (ResponseCode::NoError, Vec::new()),
            },
            3,
        );
        assert_eq!(confirmed, Ptr::Name("crawl.example.com.".into()));

        let forged = confirm_case(
            Some(name("crawl.example.com.")),
            |kind| match kind {
                RecordType::A => (
                    ResponseCode::NoError,
                    vec![RData::A(A("198.51.100.1".parse().expect("address")))],
                ),
                _ => (ResponseCode::NoError, Vec::new()),
            },
            3,
        );
        assert_eq!(forged, Ptr::Unconfirmed("crawl.example.com.".into()));
        assert!(!forged.verifies("example.com"));

        let no_ptr = confirm_case(
            None,
            |_| panic!("an address with no PTR is not looked up"),
            1,
        );
        assert_eq!(no_ptr, Ptr::None);

        let forward_failed = confirm_case(
            Some(name("crawl.example.com.")),
            |_| (ResponseCode::ServFail, Vec::new()),
            3,
        );
        assert_eq!(forward_failed, Ptr::Failed);

        let bad_name = confirm_case(
            Some(
                Name::from_labels([&b"crawl.example.com\nVERIFIED"[..]])
                    .expect("a name with characters no host name has"),
            ),
            |_| panic!("a name with bad characters is not looked up forward"),
            1,
        );
        assert_eq!(bad_name, Ptr::Failed);

        let mapped = confirm_case(
            Some(name("crawl.example.com.")),
            |kind| match kind {
                RecordType::AAAA => (
                    ResponseCode::NoError,
                    vec![RData::AAAA(AAAA(
                        "::ffff:192.0.2.7".parse().expect("address"),
                    ))],
                ),
                _ => (ResponseCode::NoError, Vec::new()),
            },
            3,
        );
        assert_eq!(mapped, Ptr::Name("crawl.example.com.".into()));

        let no_address = confirm_case(
            Some(name("crawl.example.com.")),
            |_| (ResponseCode::NoError, Vec::new()),
            3,
        );
        assert_eq!(no_address, Ptr::Unconfirmed("crawl.example.com.".into()));
    }

    /// One confirmation against a fake nameserver: the PTR answer, then the
    /// forward answer `forward` builds for each A and AAAA question.
    fn confirm_case(
        ptr_name: Option<Name>,
        forward: impl Fn(RecordType) -> (ResponseCode, Vec<RData>) + Send + 'static,
        questions: usize,
    ) -> Ptr {
        let reverse = ptr_name;
        let (addr, server) = nameserver(questions, move |query| {
            let kind = query.queries[0].query_type();
            if kind == RecordType::PTR {
                let (rcode, answers) = match &reverse {
                    Some(name) => (ResponseCode::NoError, vec![RData::PTR(PTR(name.clone()))]),
                    None => (ResponseCode::NXDomain, Vec::new()),
                };
                vec![reply(query, rcode, answers)]
            } else {
                let (rcode, answers) = forward(kind);
                vec![reply(query, rcode, answers)]
            }
        });
        let ip = "192.0.2.7".parse().expect("address");
        let mut confirm = Confirm::start(ip, addr).expect("start the confirmation");
        let result = driven_confirm(&mut confirm);
        server.join().expect("fake nameserver");
        result
    }

    #[test]
    fn a_dropped_query_closes_its_socket() {
        let server = StdUdpSocket::bind("127.0.0.1:0").expect("bind fake nameserver");
        let addr = server.local_addr().expect("fake nameserver address");
        let (dropped_tx, dropped_rx) = mpsc::channel();
        let handle = std::thread::spawn(move || {
            let mut buf = [0u8; MAX_REPLY];
            let (n, client) = server.recv_from(&mut buf).expect("receive the question");
            let query = Message::from_bytes(&buf[..n]).expect("parse the question");
            let bytes = ptr_reply(&query, "crawler.example.com.")[0]
                .to_bytes()
                .expect("encode the reply");
            server.send_to(&bytes, client).expect("send the reply");
            dropped_rx.recv().expect("the test dropped the query");
            // The kernel reports ICMP port unreachable only to a connected
            // socket, so connect to the address the question came from.
            server
                .connect(client)
                .expect("connect to the query's address");
            server
                .set_read_timeout(Some(Duration::from_millis(100)))
                .expect("read timeout");
            let deadline = Instant::now() + Duration::from_secs(2);
            loop {
                assert!(
                    Instant::now() < deadline,
                    "the closed socket never reported as refused"
                );
                // Each send to the closed port provokes the ICMP error that
                // the next receive reports.
                let _ = server.send(&[]);
                if let Err(e) = server.recv(&mut buf) {
                    if e.kind() == io::ErrorKind::ConnectionRefused {
                        return;
                    }
                }
            }
        });
        let ip = "198.51.100.7".parse().expect("address");
        let mut query = Query::ptr(ip, addr).expect("start the query");
        assert_eq!(
            driven(&mut query),
            Answer::Name("crawler.example.com.".to_string())
        );
        drop(query);
        dropped_tx.send(()).expect("tell the server");
        handle.join().expect("fake nameserver");
    }

    #[test]
    fn a_resolv_conf_with_no_nameserver_is_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("resolv.conf");
        std::fs::write(&path, "search example.com\n").expect("write");
        assert!(nameserver_from(&path).is_err());
    }

    #[test]
    fn nameserver_from_reads_the_first_nameserver_at_port_53() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("resolv.conf");
        std::fs::write(
            &path,
            "search example.com\nnameserver 192.0.2.53\nnameserver 192.0.2.54\n",
        )
        .expect("write");
        assert_eq!(
            nameserver_from(&path).expect("the nameserver"),
            "192.0.2.53:53".parse::<SocketAddr>().expect("address")
        );
    }

    /// A link-local nameserver keeps its scope id: without it the socket
    /// connects and the first send fails, so every lookup reads Failed.
    #[test]
    fn nameserver_from_keeps_an_ipv6_scope_id() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("resolv.conf");
        std::fs::write(&path, "nameserver fe80::1%7\n").expect("write");
        assert_eq!(
            nameserver_from(&path).expect("the nameserver"),
            SocketAddr::V6(SocketAddrV6::new(
                "fe80::1".parse().expect("address"),
                53,
                0,
                7
            ))
        );
    }

    /// An interface name that does not resolve is an error at start.
    #[test]
    fn nameserver_from_rejects_an_unknown_interface_scope() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("resolv.conf");
        std::fs::write(&path, "nameserver fe80::1%m6nosuchif0\n").expect("write");
        assert!(nameserver_from(&path).is_err());
    }

    /// The dot boundary is the whole of the check, so it gets the most tests.
    /// A suffix match without it accepts `example.com.attacker.net`, which is
    /// a domain the attacker controls and which ends with the string being
    /// looked for.
    #[test]
    fn verification_matches_on_a_dot_boundary_only() {
        let n = |s: &str| Ptr::Name(s.to_string());
        assert!(n("crawl-1-2-3.example.com").verifies("example.com"));
        assert!(n("example.com").verifies("example.com"));
        // The trailing dot a resolver may return.
        assert!(n("crawl.example.com.").verifies("example.com"));
        // DNS is case-insensitive.
        assert!(n("Crawl.Example.COM").verifies("example.com"));

        // The attack this exists to refuse.
        assert!(!n("example.com.attacker.net").verifies("example.com"));
        // A different domain that happens to end with the same letters.
        assert!(!n("notexample.com").verifies("example.com"));
        // A subdomain match must not run the other way.
        assert!(!n("example.com").verifies("crawl.example.com"));
    }

    /// An absent name and a failed lookup both fail verification, and neither
    /// is reported as the other.
    #[test]
    fn an_absent_name_and_a_failed_lookup_are_different_and_neither_verifies() {
        assert!(!Ptr::None.verifies("example.com"));
        assert!(!Ptr::Failed.verifies("example.com"));
        assert_ne!(Ptr::None, Ptr::Failed);
        assert_eq!(Ptr::None.name(), None);
        assert_eq!(Ptr::Failed.name(), None);
        assert_eq!(
            Ptr::Name("a.example.com".into()).name(),
            Some("a.example.com")
        );
    }

    /// An empty domain must never verify. A sighting whose expected domain is
    /// unknown would otherwise be reported as confirmed by any name at all.
    #[test]
    fn an_empty_domain_never_verifies() {
        assert!(!Ptr::Name("anything.example.com".into()).verifies(""));
        assert!(!Ptr::Name("anything.example.com".into()).verifies("."));
    }

    /// The length limit is a host name without the trailing dot: 253 bytes
    /// pass and 254 do not, with or without the dot a resolver may append.
    #[test]
    fn a_host_name_is_at_most_253_characters_without_the_trailing_dot() {
        let longest = "a".repeat(253);
        assert!(is_host_name(&longest));
        assert!(is_host_name(&format!("{longest}.")));
        assert!(!is_host_name(&format!("{longest}b")));
        assert!(!is_host_name(&format!("{longest}b.")));
    }
}
