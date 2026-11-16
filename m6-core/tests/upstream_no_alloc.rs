//! `client::Upstream`'s no-allocation proof, on the main thread so the count is
//! this binary's own.
//!
//! `harness = false` means no libtest thread runs here, so
//! [`alloc_count::now`] reads only what these checks allocated. The plain
//! request check is a guard: a thousand requests must allocate nothing. The TLS
//! check is a recorded measurement for ALLOC-1 and runs only when the binary is
//! started with `--tls` (a rustls server thread allocates in the same process
//! while it counts, so it asserts nothing). The test speaks libtest's protocol
//! through `libtest-mimic`; the checks are forced onto one thread so the count
//! is this `main` thread's own.

#![allow(
    clippy::disallowed_methods,
    reason = "test code: the TLS measurement runs a rustls server thread"
)]

use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::Arc;
use std::time::{Duration, Instant};

use libtest_mimic::{Arguments, Trial};
use m6_core::client::{Addr, Upstream};
use m6_core::testkit::alloc_count;

#[global_allocator]
static ALLOC: alloc_count::Counting = alloc_count::Counting;

const REQUEST: &[u8] = b"GET / HTTP/1.1\r\nHost: x\r\n\r\n";

fn main() {
    // The ALLOC-1 measurement, not a test: started by hand, never listed.
    if std::env::args().nth(1).as_deref() == Some("--tls") {
        tls_requests_report_their_allocations();
        return;
    }

    let mut args = Arguments::from_args();
    // One thread: `libtest-mimic` then runs the trial in this (the main)
    // thread, so the counting allocator reads only the trial's own
    // allocations and no other thread's.
    args.test_threads = Some(1);
    let tests = vec![Trial::test(
        "a_thousand_plain_requests_allocate_nothing",
        || {
            let allocated = a_thousand_plain_requests_allocate_nothing();
            println!("a_thousand_plain_requests_allocate_nothing: {allocated} allocation(s)");
            if allocated == 0 {
                Ok(())
            } else {
                Err(format!("{allocated} allocation(s) in 1000 requests").into())
            }
        },
    )];
    libtest_mimic::run(&args, tests).exit();
}

fn a_thousand_plain_requests_allocate_nothing() -> usize {
    let dir = tempfile::tempdir_in(m6_core::testkit::socket_scratch_root()).expect("tempdir");
    let path = dir.path().join("up.sock");
    let listener = UnixListener::bind(&path).expect("bind");
    let addr = Addr::Unix(path);
    let deadline = Instant::now() + Duration::from_secs(30);

    let mut up = Upstream::new(64 * 1024, 16 * 1024);
    let mut response = b"HTTP/1.1 200 OK\r\nContent-Length: 4096\r\n\r\n".to_vec();
    response.extend_from_slice(&vec![b'x'; 4096]);

    // One warm-up request, so the connect's allocations are out of count and
    // the connection is open for reuse. The connect is the first `start`, and
    // the listener accepts it here.
    up.request_buf().extend_from_slice(REQUEST);
    up.start(&addr, None, false, deadline).expect("start");
    let mut server = listener.accept().expect("accept").0;
    answer_one(&mut up, &mut server, &response);

    let start = alloc_count::now();
    for _ in 0..1_000 {
        up.request_buf().extend_from_slice(REQUEST);
        up.start(&addr, None, false, deadline).expect("start");
        answer_one(&mut up, &mut server, &response);
    }
    let keep_alive = alloc_count::now() - start;

    // A reconnect must also allocate nothing: the slot's address and buffers
    // are reused. Close the connection, then connect again to the same path.
    up.close();
    let start = alloc_count::now();
    up.request_buf().extend_from_slice(REQUEST);
    up.start(&addr, None, false, deadline).expect("start");
    let mut server2 = listener.accept().expect("accept").0;
    answer_one(&mut up, &mut server2, &response);
    let reconnect = alloc_count::now() - start;
    assert_eq!(reconnect, 0, "a reconnect must allocate nothing");

    keep_alive
}

/// One request on the open connection, on one thread: the client writes, the
/// far end reads and answers, then the client reads the reply.
fn answer_one(up: &mut Upstream, server: &mut UnixStream, response: &[u8]) {
    assert!(!up.drive().expect("drive"));
    let mut got = [0u8; 512];
    let mut n = 0;
    while !got[..n].windows(4).any(|w| w == b"\r\n\r\n") {
        let r = server.read(&mut got[n..]).expect("read request");
        assert!(r > 0, "the request must reach the far end");
        n += r;
    }
    server.write_all(response).expect("write reply");
    assert!(up.drive().expect("drive"), "the reply must complete");
    let mut slots: [std::mem::MaybeUninit<httparse::Header<'_>>; m6_core::client::MAX_HEADERS] =
        [const { std::mem::MaybeUninit::uninit() }; m6_core::client::MAX_HEADERS];
    let reply = up.reply(&mut slots).expect("reply");
    assert_eq!((reply.status, reply.body.len()), (200, 4096));
}

fn tls_requests_report_their_allocations() {
    let ck = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).expect("cert");
    let cert = ck.cert.der().clone();
    let key = rustls::pki_types::PrivateKeyDer::try_from(ck.key_pair.serialize_der()).expect("key");
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let server = Arc::new(
        rustls::ServerConfig::builder_with_provider(provider.clone())
            .with_safe_default_protocol_versions()
            .expect("protocol versions")
            .with_no_client_auth()
            .with_single_cert(vec![cert.clone()], key)
            .expect("server config"),
    );
    let mut roots = rustls::RootCertStore::empty();
    roots.add(cert).expect("trust the run's own cert");
    let client = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("protocol versions")
        .with_root_certificates(roots)
        .with_no_client_auth();

    const N: usize = 100;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let peer = std::thread::spawn(move || {
        let (tcp, _) = listener.accept().expect("accept");
        let conn = rustls::ServerConnection::new(server).expect("server conn");
        let mut stream = rustls::StreamOwned::new(conn, tcp);
        // One warm-up request plus the N counted ones.
        for _ in 0..=N {
            let mut got = Vec::new();
            let mut buf = [0u8; 4096];
            while !got.windows(4).any(|w| w == b"\r\n\r\n") {
                match stream.read(&mut buf) {
                    Ok(0) | Err(_) => return,
                    Ok(n) => got.extend_from_slice(&buf[..n]),
                }
            }
            let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok");
            let _ = stream.flush();
        }
    });

    let name = rustls::pki_types::ServerName::try_from("localhost".to_string()).expect("name");
    let tls = m6_core::client::TlsClient {
        config: Arc::new(client),
        name,
    };
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut up = Upstream::new(64 * 1024, 16 * 1024);
    let mut poll = m6_core::platform::Poll::new().expect("poll");
    let mut events = m6_core::platform::Events::with_capacity(8);
    let token = m6_core::platform::Token(0);
    let mut registered: Option<m6_core::platform::Interest> = None;

    // One warm-up request, so the connect and handshake are out of count.
    tls_request(
        &mut up,
        &mut poll,
        &mut events,
        token,
        &mut registered,
        &tls,
        addr,
        deadline,
    );

    let start = alloc_count::now();
    for _ in 0..N {
        tls_request(
            &mut up,
            &mut poll,
            &mut events,
            token,
            &mut registered,
            &tls,
            addr,
            deadline,
        );
    }
    let allocated = alloc_count::now() - start;
    println!("tls_requests_report_their_allocations: {allocated} allocation(s) over {N} requests");
    peer.join().expect("peer");
}

/// One request over TLS, driven with a poll until the reply completes.
#[allow(clippy::too_many_arguments)]
fn tls_request(
    up: &mut Upstream,
    poll: &mut m6_core::platform::Poll,
    events: &mut m6_core::platform::Events,
    token: m6_core::platform::Token,
    registered: &mut Option<m6_core::platform::Interest>,
    tls: &m6_core::client::TlsClient,
    addr: std::net::SocketAddr,
    deadline: Instant,
) {
    up.request_buf()
        .extend_from_slice(b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n");
    up.start(&Addr::Tcp(addr), Some(tls), false, deadline)
        .expect("start");
    let mut interest = up.interest();
    if *registered != Some(interest) {
        if registered.is_some() {
            poll.registry()
                .reregister_fd(up.fd(), token, interest)
                .expect("reregister");
        } else {
            poll.registry()
                .register_fd(up.fd(), token, interest)
                .expect("register");
        }
        *registered = Some(interest);
    }
    loop {
        poll.poll(events, Some(Duration::from_millis(100)))
            .expect("poll");
        if events.iter().any(|ev| ev.token() == token) {
            if up.drive().expect("drive") {
                let mut slots: [std::mem::MaybeUninit<httparse::Header<'_>>;
                    m6_core::client::MAX_HEADERS] =
                    [const { std::mem::MaybeUninit::uninit() }; m6_core::client::MAX_HEADERS];
                let reply = up.reply(&mut slots).expect("reply");
                assert_eq!(reply.status, 200);
                return;
            }
            // Drive first: under epoll edge-triggering, interest registered
            // before the drive waits for an edge that never comes.
            let next = up.interest();
            if next != interest {
                poll.registry()
                    .reregister_fd(up.fd(), token, next)
                    .expect("reregister");
                interest = next;
                *registered = Some(next);
            }
        }
        if Instant::now() >= up.deadline() {
            panic!("backend timed out");
        }
    }
}
