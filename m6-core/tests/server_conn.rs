#![allow(
    clippy::disallowed_methods,
    reason = "test code: a blocking client with deadlines and a writer thread drive the slot from outside"
)]

//! `h1::ServerConn`'s proof.
//!
//! A thousand keep-alive requests allocate nothing, a pipelined pair is served
//! from one readiness event, a request larger than the slot's buffer is taken
//! whole and the buffer is cut back, a streamed body is pumped one chunk per
//! writable turn, keep-alive and the request cap behave, and a refusal answers
//! as the blocking path does today.
//!
//! `harness = false`: its own `main`, and the counting allocator is installed
//! before any thread starts, so the count is only this binary's own.

use std::io::Read as _;
use std::io::Write;
use std::os::unix::net::UnixStream as StdUnixStream;
use std::time::Duration;

use m6_core::h1::{self, Flush, Read, ServerConn};
use m6_core::mio;
use m6_core::mio::{Events, Interest, Poll, Token};
use m6_core::testkit::alloc_count;

#[global_allocator]
static ALLOC: alloc_count::Counting = alloc_count::Counting;

const SERVER: Token = Token(0);
const READ_CAP: usize = 16 * 1024;
const WRITE_CAP: usize = 64 * 1024;
const DEADLINE: Duration = Duration::from_secs(10);
const REQUEST: &[u8] = b"GET /x HTTP/1.1\r\nHost: localhost\r\n\r\n";
/// The exact bytes `respond(200, &[("Content-Type", "text/plain")], b"ok")`
/// writes when the connection keeps alive.
const RESPONSE_LEN: usize = 90;
/// The same answer with `Connection: close`, five bytes shorter.
const RESPONSE_LEN_CLOSE: usize = RESPONSE_LEN - 5;

fn main() {
    let mut failed = false;
    failed |= check(
        "a_conn_serves_keep_alive_requests_without_allocating",
        a_conn_serves_keep_alive_requests_without_allocating,
    );
    failed |= check(
        "a_pipelined_pair_is_served_from_one_read",
        a_pipelined_pair_is_served_from_one_read,
    );
    failed |= check(
        "a_large_upload_grows_and_returns_the_buffer",
        a_large_upload_grows_and_returns_the_buffer,
    );
    failed |= check(
        "a_stream_body_is_pumped_per_writable_turn",
        a_stream_body_is_pumped_per_writable_turn,
    );
    failed |= check(
        "keep_alive_and_the_request_cap",
        keep_alive_and_the_request_cap,
    );
    failed |= check("parse_errors_answer_as_today", parse_errors_answer_as_today);
    failed |= check(
        "a_chunked_body_split_across_reads_is_one_request",
        a_chunked_body_split_across_reads_is_one_request,
    );
    failed |= check(
        "a_declared_body_does_not_allocate_before_it_arrives",
        a_declared_body_does_not_allocate_before_it_arrives,
    );
    failed |= check("a_response_allocates_nothing", a_response_allocates_nothing);
    std::process::exit(if failed { 1 } else { 0 });
}

fn check(name: &str, run: fn() -> bool) -> bool {
    println!("running {name}");
    std::io::stdout().flush().expect("flush");
    let ok = run();
    println!("{name}: {}", if ok { "ok" } else { "FAILED" });
    !ok
}

/// A slot with an attached server end, the client end of the same pair, and
/// the poll they share.
struct Harness {
    poll: Poll,
    events: Events,
    client: StdUnixStream,
    conn: ServerConn,
}

fn harness() -> Harness {
    let (client, server) = StdUnixStream::pair().expect("a socket pair");
    // mio's `from_std` takes a socket that is already non-blocking; a pair made
    // by std is blocking, and a blocking read is what a loop must never do.
    server
        .set_nonblocking(true)
        .expect("a non-blocking server end");
    client
        .set_read_timeout(Some(DEADLINE))
        .expect("a read deadline");
    client
        .set_write_timeout(Some(DEADLINE))
        .expect("a write deadline");
    harness_with((client, mio::net::UnixStream::from_std(server)))
}

fn harness_with(pair: (StdUnixStream, mio::net::UnixStream)) -> Harness {
    let (client, server) = pair;
    let poll = Poll::new().expect("a poll");
    let mut conn = ServerConn::new(READ_CAP, WRITE_CAP);
    conn.attach(server);
    poll.registry()
        .register(conn.stream().expect("attached"), SERVER, Interest::READABLE)
        .expect("register");
    Harness {
        poll,
        events: Events::with_capacity(8),
        client,
        conn,
    }
}

impl Harness {
    /// Wait for one readable event on the server end, with a deadline.
    fn wait_readable(&mut self) {
        let started = std::time::Instant::now();
        loop {
            self.events.clear();
            self.poll
                .poll(&mut self.events, Some(DEADLINE))
                .expect("a poll with a deadline");
            if self
                .events
                .iter()
                .any(|e| e.token() == SERVER && e.is_readable())
            {
                return;
            }
            assert!(
                started.elapsed() < DEADLINE,
                "no readable event within {DEADLINE:?}"
            );
        }
    }

    /// Wait for one writable event on the server end, with a deadline.
    fn wait_writable(&mut self) {
        self.poll
            .registry()
            .reregister(
                self.conn.stream().expect("attached"),
                SERVER,
                Interest::WRITABLE,
            )
            .expect("reregister writable");
        let started = std::time::Instant::now();
        loop {
            self.events.clear();
            self.poll
                .poll(&mut self.events, Some(DEADLINE))
                .expect("a poll with a deadline");
            if self
                .events
                .iter()
                .any(|e| e.token() == SERVER && e.is_writable())
            {
                return;
            }
            assert!(
                started.elapsed() < DEADLINE,
                "no writable event within {DEADLINE:?}"
            );
        }
    }

    /// Swap in the next connection's ends: deregister the old, attach the new.
    fn next_connection(&mut self, client: StdUnixStream, server: mio::net::UnixStream) {
        if let Some(mut old) = self.conn.detach() {
            self.poll
                .registry()
                .deregister(&mut old)
                .expect("deregister");
        }
        self.conn.attach(server);
        self.poll
            .registry()
            .register(
                self.conn.stream().expect("attached"),
                SERVER,
                Interest::READABLE,
            )
            .expect("register");
        self.client = client;
    }

    fn readable_again(&mut self) {
        self.poll
            .registry()
            .reregister(
                self.conn.stream().expect("attached"),
                SERVER,
                Interest::READABLE,
            )
            .expect("reregister readable");
    }

    /// Read and answer until the turn has nothing more to read. Returns the
    /// last flush outcome.
    fn turn(&mut self) -> Flush {
        loop {
            // The slots are made per call and cost nothing: `head_slots` is
            // uninitialised, and the head borrows them only for this turn.
            let mut heads = h1::head_slots();
            let read = self.conn.read(&mut heads).expect("read");
            let answered = match read {
                // The head is dropped here; the slot can be used again below.
                Read::Request(_head) => true,
                Read::More => false,
                Read::Closed => return Flush::Close,
            };
            if answered {
                self.conn
                    .respond(200, &[("Content-Type", "text/plain")], b"ok");
            }
            match self.conn.flush().expect("flush") {
                Flush::Done if answered => {}
                Flush::Done => return Flush::Done,
                other => return other,
            }
        }
    }

    /// Serve exactly one request and read its response, expecting the
    /// connection to keep alive (`more`) or to close after this answer.
    fn serve_one_expecting(&mut self, more: bool) {
        self.client.write_all(REQUEST).expect("write a request");
        self.wait_readable();
        let flush = self.turn();
        assert_eq!(
            flush,
            if more { Flush::Done } else { Flush::Close },
            "the flush outcome after request {}",
            if more { "below the cap" } else { "at the cap" }
        );
        let len = if more {
            RESPONSE_LEN
        } else {
            RESPONSE_LEN_CLOSE
        };
        let mut response = [0u8; RESPONSE_LEN];
        self.client
            .read_exact(&mut response[..len])
            .expect("read the response");
        assert!(
            response.starts_with(b"HTTP/1.1 200 OK\r\n"),
            "got {:?}",
            String::from_utf8_lossy(&response)
        );
    }
}

fn a_conn_serves_keep_alive_requests_without_allocating() -> bool {
    // One connection serves `MAX_REQUESTS_PER_CONN` requests, so a thousand
    // requests need ten connections. Every socket pair is made before the
    // count starts: attaching one allocates nothing.
    const CONNECTIONS: usize = 10;
    const PER_CONNECTION: usize = 100;
    let mut pairs: Vec<(StdUnixStream, mio::net::UnixStream)> = (0..CONNECTIONS)
        .map(|_| {
            let (client, server) = StdUnixStream::pair().expect("a socket pair");
            server
                .set_nonblocking(true)
                .expect("a non-blocking server end");
            client
                .set_read_timeout(Some(DEADLINE))
                .expect("a read deadline");
            client
                .set_write_timeout(Some(DEADLINE))
                .expect("a write deadline");
            (client, mio::net::UnixStream::from_std(server))
        })
        .collect();

    let mut h = harness_with(pairs.remove(0));
    h.serve_one_expecting(true);
    let start = alloc_count::now();
    // Finish the first connection, then the nine made above.
    let mut served = 1usize;
    for i in 1..PER_CONNECTION {
        h.serve_one_expecting(i + 1 < PER_CONNECTION);
        served += 1;
    }
    for (client, server) in pairs {
        h.next_connection(client, server);
        for i in 0..PER_CONNECTION {
            h.serve_one_expecting(i + 1 < PER_CONNECTION);
            served += 1;
        }
    }
    let allocations = alloc_count::now() - start;
    println!(
        "  keep-alive requests: {allocations} allocation(s) over {}",
        served - 1
    );
    allocations == 0
}

fn a_pipelined_pair_is_served_from_one_read() -> bool {
    let mut h = harness();
    let pair = [REQUEST, REQUEST].concat();
    h.client.write_all(&pair).expect("write both requests");

    // One readability event must serve both answers, the second request coming
    // from the buffer without a new event.
    h.wait_readable();
    let mut served = 0;
    loop {
        let mut heads = h1::head_slots();
        let read = h.conn.read(&mut heads).expect("read");
        let answered = match read {
            Read::Request(_head) => true,
            Read::More => false,
            Read::Closed => false,
        };
        if !answered {
            if served == 2 {
                // A third read must now say More, still without polling.
                let third = {
                    let mut heads = h1::head_slots();
                    matches!(h.conn.read(&mut heads).expect("read"), Read::More)
                };
                assert!(third, "a served pipeline leaves nothing to read");
            }
            break;
        }
        h.conn
            .respond(200, &[("Content-Type", "text/plain")], b"ok");
        served += 1;
        let flush = h.conn.flush().expect("flush");
        assert_eq!(flush, Flush::Done, "both responses fit the socket");
    }
    if served != 2 {
        println!("  served {served} requests, expected 2");
        return false;
    }
    let mut response = [0u8; RESPONSE_LEN * 2];
    h.client
        .read_exact(&mut response)
        .expect("read both answers");
    response.starts_with(b"HTTP/1.1 200 OK\r\n")
        && response[RESPONSE_LEN..].starts_with(b"HTTP/1.1 200 OK\r\n")
}

fn a_large_upload_grows_and_returns_the_buffer() -> bool {
    const BODY: usize = 1024 * 1024;
    let mut h = harness();

    // The client drives a 1 MiB upload and a following small request on the
    // same connection while this thread serves it.
    let mut client = h.client.try_clone().expect("a client clone");
    let uploader = std::thread::spawn(move || {
        let head = format!("POST /x HTTP/1.1\r\nHost: localhost\r\nContent-Length: {BODY}\r\n\r\n")
            .into_bytes();
        let mut request = head.clone();
        request.resize(head.len() + BODY, b'x');
        client.write_all(&request).expect("write the upload");
        let mut response = [0u8; RESPONSE_LEN];
        client.read_exact(&mut response).expect("read the answer");
        client.write_all(REQUEST).expect("write the small request");
        client
            .read_exact(&mut response)
            .expect("read the small answer");
    });

    // Serve the upload and the small request, then stop: the client thread has
    // read both answers even though the test still holds a client copy.
    let mut requests = 0u32;
    let mut small_allocations = None;
    loop {
        h.wait_readable();
        // The small request is the second one; its whole server turn is
        // counted, including the answer and the flush.
        let counted = if requests == 1 {
            Some(alloc_count::now())
        } else {
            None
        };
        let mut heads = h1::head_slots();
        let read = h.conn.read(&mut heads).expect("read");
        let answered = match read {
            Read::Request(_head) => true,
            Read::More => false,
            Read::Closed => break,
        };
        if answered {
            h.conn
                .respond(200, &[("Content-Type", "text/plain")], b"ok");
            requests += 1;
        }
        let flush = h.conn.flush().expect("flush");
        if let (Some(start), true) = (counted, answered) {
            small_allocations = Some(alloc_count::now() - start);
        }
        match flush {
            Flush::Done if requests >= 2 => break,
            Flush::Close => break,
            _ => {}
        }
    }
    assert_eq!(requests, 2, "both requests must be served");
    uploader.join().expect("the uploading client");

    // The grown input buffer was cut back after the large request, and the
    // small request that followed allocated nothing, as the plan's row says.
    let (read_cap, write_cap) = h.conn.buffer_caps();
    println!(
        "  slot buffers after the upload: read {read_cap}, write {write_cap}; \
         small request: {small_allocations:?} allocation(s)"
    );
    read_cap == READ_CAP && write_cap == WRITE_CAP && small_allocations == Some(0)
}

/// A `Response` answered through `respond_response` allocates nothing: the
/// head is written into the slot and the default `ETag` is formatted there.
/// This is the claim `site_allocs` cannot make on its own, because it only
/// checks that its count does not vary.
fn a_response_allocates_nothing() -> bool {
    use m6_core::response::{Body, Response};

    let mut h = harness();
    h.client.write_all(REQUEST).expect("write a request");
    h.wait_readable();
    {
        let mut heads = h1::head_slots();
        let read = h.conn.read(&mut heads).expect("read");
        if !matches!(read, Read::Request(_head)) {
            println!("  expected one request");
            return false;
        }
    }
    // Built before the count: the response and its bytes are the caller's.
    let response = Response {
        status: 200,
        headers: vec![("Content-Type".to_string(), "text/plain".to_string())],
        body: Body::Bytes(b"ok".to_vec()),
        verbatim: false,
        template_name: None,
        template_dict: None,
    };
    let start = alloc_count::now();
    h.conn.respond_response(response);
    let flush = h.conn.flush().expect("flush");
    let allocations = alloc_count::now() - start;
    println!("  respond_response + flush: {allocations} allocation(s)");
    flush == Flush::Done && allocations == 0
}

fn a_stream_body_is_pumped_per_writable_turn() -> bool {
    use m6_core::response::{Body, Response};

    let mut h = harness();
    const BODY: usize = 4 * 1024 * 1024;
    let mut file = tempfile::NamedTempFile::new().expect("a temp file");
    let bytes: Vec<u8> = (0..BODY).map(|i| (i % 251) as u8).collect();
    file.write_all(&bytes).expect("write the body");
    file.flush().expect("flush the body");
    let file = file.reopen().expect("reopen the body");

    // The client sends a request, then reads nothing until the server has said
    // it would block: the body must not be buffered ahead of the socket.
    h.client.write_all(REQUEST).expect("write a request");
    h.wait_readable();
    let framed = {
        let mut heads = h1::head_slots();
        matches!(h.conn.read(&mut heads).expect("read"), Read::Request(_head))
    };
    if !framed {
        return false;
    }
    let response = Response {
        status: 200,
        headers: vec![("Content-Type".to_string(), "text/plain".to_string())],
        body: Body::Stream {
            len: BODY as u64,
            reader: Box::new(file),
        },
        verbatim: false,
        template_name: None,
        template_dict: None,
    };
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {BODY}\r\n\
         Connection: keep-alive\r\n\r\n"
    );
    let head_len = head.len();
    h.conn.respond_response(response);

    // The peer reads nothing, so the first turn fills the socket and says the
    // body is owed. One chunk was read, not the whole file.
    let flush = h.conn.flush().expect("flush");
    if flush != Flush::More {
        println!("  first flush said {flush:?}, expected More");
        return false;
    }
    let (_, write_cap) = h.conn.buffer_caps();
    let one_chunk = 64 * 1024;
    println!("  slot write buffer after one turn: {write_cap}");
    if write_cap != one_chunk {
        return false;
    }
    assert_eq!(h.conn.interest(), Interest::WRITABLE);

    // Now the client reads everything while the server pumps on writability.
    let mut reader_client = h.client.try_clone().expect("a client clone");
    let reader = std::thread::spawn(move || {
        let client = &mut reader_client;
        client.set_read_timeout(Some(DEADLINE)).expect("a deadline");
        let mut all = Vec::new();
        let mut buf = [0u8; 8192];
        loop {
            match client.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => all.extend_from_slice(&buf[..n]),
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    break
                }
                Err(_) => break,
            }
            if all.len() >= head_len + BODY {
                break;
            }
        }
        all
    });

    loop {
        match h.conn.flush().expect("flush") {
            Flush::Done | Flush::Close => break,
            Flush::More => {
                h.wait_writable();
                h.readable_again();
            }
        }
    }

    let all = reader.join().expect("the reader");
    all.len() == head_len + BODY && all.starts_with(head.as_bytes()) && all[head_len..] == bytes[..]
}

fn keep_alive_and_the_request_cap() -> bool {
    let mut h = harness();

    // `Connection: close` ends the connection after one answer.
    let close = b"GET /x HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
    h.client.write_all(close).expect("write");
    h.wait_readable();
    let flush = h.turn();
    let mut response = [0u8; RESPONSE_LEN_CLOSE];
    h.client.read_exact(&mut response).expect("read");
    if flush != Flush::Close {
        println!("  a close request gave {flush:?}");
        return false;
    }
    let text = String::from_utf8_lossy(&response);
    if !text.contains("Connection: close") {
        return false;
    }

    // A fresh connection: the 100th request is the one that closes.
    let mut h = harness();
    for i in 1..=100u32 {
        h.client.write_all(REQUEST).expect("write");
        h.wait_readable();
        let flush = h.turn();
        let len = if i < 100 {
            RESPONSE_LEN
        } else {
            RESPONSE_LEN_CLOSE
        };
        let mut response = [0u8; RESPONSE_LEN];
        h.client.read_exact(&mut response[..len]).expect("read");
        let text = String::from_utf8_lossy(&response[..len]);
        if i < 100 {
            if !text.contains("Connection: keep-alive") {
                println!("  request {i} did not keep alive");
                return false;
            }
        } else {
            if !text.contains("Connection: close") || flush != Flush::Close {
                println!("  request 100 kept the connection open");
                return false;
            }
        }
    }
    true
}

fn parse_errors_answer_as_today() -> bool {
    // Header too large: 413, as `parse.rs` answers a head over its cap.
    {
        let mut h = harness();
        // A complete head over the cap is accepted today (the cap is on an
        // unfinished head), so this one never sends the blank line.
        let mut request = b"GET /x HTTP/1.1\r\nHost: localhost\r\n".to_vec();
        // Over the 8 KiB head cap and under the 64 header slots, so the
        // refusal is the size cap rather than the header count.
        for i in 0..12 {
            request.extend_from_slice(format!("X-Pad-{i}: {}\r\n", "a".repeat(800)).as_bytes());
        }
        let mut writer = h.client.try_clone().expect("a client clone");
        // The head is larger than the socket's buffer: the writer blocks until
        // the server reads, and the server is this thread.
        let writing = std::thread::spawn(move || {
            let _ = writer.write_all(&request);
        });
        h.wait_readable();
        let flush = h.turn();
        let mut response = [0u8; 4096];
        let n = h.client.read(&mut response).expect("read");
        // Close the server end so the writer cannot stay blocked on a full
        // socket after the refusal.
        drop(h.conn.detach());
        let _ = writing.join();
        let text = String::from_utf8_lossy(&response[..n]).to_string();
        if flush != Flush::Close || !text.starts_with("HTTP/1.1 413 Payload Too Large\r\n") {
            println!("  header too large: {flush:?} {text:?}");
            return false;
        }
    }

    // Body too large: 413.
    {
        let mut h = harness();
        let request = b"POST /x HTTP/1.1\r\nHost: localhost\r\nContent-Length: 20000000\r\n\r\n";
        h.client.write_all(request).expect("write");
        h.wait_readable();
        let flush = h.turn();
        let mut response = [0u8; 4096];
        let n = h.client.read(&mut response).expect("read");
        let text = String::from_utf8_lossy(&response[..n]).to_string();
        if flush != Flush::Close || !text.starts_with("HTTP/1.1 413 Payload Too Large\r\n") {
            println!("  body too large: {flush:?} {text:?}");
            return false;
        }
    }

    // Malformed: 400.
    {
        let mut h = harness();
        h.client
            .write_all(b"GET /x HTTP/1.1\nHost: localhost\n\n")
            .expect("write");
        h.wait_readable();
        let flush = h.turn();
        let mut response = [0u8; 4096];
        let n = h.client.read(&mut response).expect("read");
        let text = String::from_utf8_lossy(&response[..n]).to_string();
        if flush != Flush::Close || !text.starts_with("HTTP/1.1 400 Bad Request\r\n") {
            println!("  malformed: {flush:?} {text:?}");
            return false;
        }
    }

    // `Expect: 100-continue`: the interim answer goes out before the body is
    // asked for, then the request is served normally.
    {
        let mut h = harness();
        h.client
            .write_all(
                b"POST /x HTTP/1.1\r\nHost: localhost\r\nContent-Length: 2\r\n\
                  Expect: 100-continue\r\n\r\n",
            )
            .expect("write the head");
        h.wait_readable();
        let more = {
            let mut heads = h1::head_slots();
            matches!(h.conn.read(&mut heads).expect("read"), Read::More)
        };
        if !more {
            return false;
        }
        assert_eq!(h.conn.interest(), Interest::WRITABLE);
        let flush = h.conn.flush().expect("flush");
        if flush != Flush::Done {
            println!("  100-continue flush: {flush:?}");
            return false;
        }
        let mut interim = [0u8; 25];
        h.client.read_exact(&mut interim).expect("read 100");
        if &interim[..] != b"HTTP/1.1 100 Continue\r\n\r\n" {
            println!("  interim: {:?}", String::from_utf8_lossy(&interim));
            return false;
        }
        h.client.write_all(b"ok").expect("write the body");
        h.wait_readable();
        let flush = h.turn();
        let mut response = [0u8; RESPONSE_LEN];
        h.client.read_exact(&mut response).expect("read");
        if flush != Flush::Done || !response.starts_with(b"HTTP/1.1 200 OK\r\n") {
            return false;
        }
    }

    // An expectation this server does not understand: 417.
    {
        let mut h = harness();
        // A body is owed, so the expectation is answered before the read waits
        // for it. A bodyless request is served today and is not a refusal.
        h.client
            .write_all(
                b"POST /x HTTP/1.1\r\nHost: localhost\r\nContent-Length: 2\r\n\
                  Expect: something\r\n\r\n",
            )
            .expect("write");
        h.wait_readable();
        let flush = h.turn();
        let mut response = [0u8; 4096];
        let n = h.client.read(&mut response).expect("read");
        let text = String::from_utf8_lossy(&response[..n]).to_string();
        if flush != Flush::Close || !text.starts_with("HTTP/1.1 417 Expectation Failed\r\n") {
            println!("  417: {flush:?} {text:?}");
            return false;
        }
    }

    // A 20-digit Content-Length is refused by the body cap. It must not
    // overflow the head's body slice on the way.
    {
        let mut h = harness();
        h.client
            .write_all(
                b"POST /x HTTP/1.1\r\nHost: localhost\r\n\
                  Content-Length: 18446744073709551615\r\n\r\n",
            )
            .expect("write");
        h.wait_readable();
        let flush = h.turn();
        let mut response = [0u8; 4096];
        let n = h.client.read(&mut response).expect("read");
        let text = String::from_utf8_lossy(&response[..n]).to_string();
        if flush != Flush::Close || !text.starts_with("HTTP/1.1 413 Payload Too Large\r\n") {
            println!("  20-digit length: {flush:?} {text:?}");
            return false;
        }
    }

    // A 16-hex-digit chunk size is `usize::MAX`: malformed framing, 400.
    {
        let mut h = harness();
        h.client
            .write_all(
                b"POST /x HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked\r\n\r\n\
                  1\r\na\r\nffffffffffffffff\r\n",
            )
            .expect("write");
        h.wait_readable();
        let flush = h.turn();
        let mut response = [0u8; 4096];
        let n = h.client.read(&mut response).expect("read");
        let text = String::from_utf8_lossy(&response[..n]).to_string();
        if flush != Flush::Close || !text.starts_with("HTTP/1.1 400 Bad Request\r\n") {
            println!("  huge chunk size: {flush:?} {text:?}");
            return false;
        }
    }

    true
}

/// A chunked body split across reads is one request with the whole body, never
/// a 400 and never a second request parsed out of the chunk framing. The
/// in-place decoder may copy only once the whole body is buffered.
fn a_chunked_body_split_across_reads_is_one_request() -> bool {
    const HEAD: &[u8] =
        b"POST /x HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked\r\n\r\n";
    const BODY: &[u8] = b"5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n";

    // One byte at a time.
    {
        let mut h = harness();
        h.client.write_all(HEAD).expect("write the head");
        h.wait_readable();
        {
            let mut heads = h1::head_slots();
            if !matches!(h.conn.read(&mut heads).expect("read"), Read::More) {
                println!("  the head alone must be More");
                return false;
            }
        }
        let mut body = None;
        for &byte in BODY {
            h.client.write_all(&[byte]).expect("write a body byte");
            h.wait_readable();
            let mut heads = h1::head_slots();
            match h.conn.read(&mut heads).expect("read") {
                Read::Request(head) => {
                    body = Some(head.body().to_vec());
                    break;
                }
                Read::More => {}
                Read::Closed => return false,
            }
        }
        if body.as_deref() != Some(&b"hello world"[..]) {
            println!("  one-byte body gave {body:?}");
            return false;
        }
        // The chunk framing must not be parsed again as requests.
        {
            let mut heads = h1::head_slots();
            if !matches!(h.conn.read(&mut heads).expect("read"), Read::More) {
                println!("  the body was parsed a second time");
                return false;
            }
        }
        h.conn
            .respond(200, &[("Content-Type", "text/plain")], b"ok");
        if h.conn.flush().expect("flush") != Flush::Done {
            return false;
        }
        let mut response = [0u8; RESPONSE_LEN];
        if h.client.read_exact(&mut response).is_err() {
            return false;
        }
        if !response.starts_with(b"HTTP/1.1 200 OK\r\n") {
            return false;
        }
    }

    // In two writes.
    {
        let mut h = harness();
        h.client.write_all(HEAD).expect("write the head");
        h.wait_readable();
        let half = BODY.len() / 2;
        h.client.write_all(&BODY[..half]).expect("write half");
        h.wait_readable();
        {
            let mut heads = h1::head_slots();
            if !matches!(h.conn.read(&mut heads).expect("read"), Read::More) {
                println!("  half a body must be More");
                return false;
            }
        }
        h.client.write_all(&BODY[half..]).expect("write the rest");
        h.wait_readable();
        let mut heads = h1::head_slots();
        match h.conn.read(&mut heads).expect("read") {
            Read::Request(head) => {
                if head.body() != b"hello world" {
                    println!("  two-write body gave {:?}", head.body());
                    return false;
                }
            }
            _ => return false,
        }
    }

    true
}

/// A declared body allocates only as its bytes arrive: a short request naming
/// a 16 MiB `Content-Length` must not grow the slot to 16 MiB before the body
/// is sent.
fn a_declared_body_does_not_allocate_before_it_arrives() -> bool {
    let mut h = harness();
    h.client
        .write_all(b"POST /x HTTP/1.1\r\nHost: localhost\r\nContent-Length: 16777216\r\n\r\n")
        .expect("write");
    h.wait_readable();
    let mut heads = h1::head_slots();
    let more = matches!(h.conn.read(&mut heads).expect("read"), Read::More);
    let (read_cap, _) = h.conn.buffer_caps();
    println!("  declared 16 MiB body, head only: read buffer {read_cap}");
    more && read_cap <= 2 * READ_CAP
}
