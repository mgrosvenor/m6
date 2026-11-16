#![allow(
    clippy::disallowed_methods,
    reason = "test code: a blocking client with deadlines and a writer thread drive the slot from outside"
)]

//! The page path's allocation baseline, for ALLOC-3 to reduce.
//!
//! A `Site` over a temp site with one template route and one params file is
//! driven through a `ServerConn` for a hundred requests. Every request after
//! the first must allocate the same number of times: nothing grows per request.
//! The number itself is printed and reported as the baseline, not asserted on.
//!
//! `harness = false` and its own `main`, so the counting allocator is installed
//! before any thread starts and the count is only this binary's own. The one
//! check speaks libtest's protocol through `libtest-mimic`, forced onto one
//! thread so it runs in this `main` thread.

use std::io::Read as _;
use std::io::Write;
use std::os::unix::net::UnixStream as StdUnixStream;
use std::time::Duration;

use libtest_mimic::{Arguments, Failed, Trial};
use m6_core::h1::{self, Flush, Read, ServerConn};
use m6_core::platform;
use m6_core::platform::{Events, Interest, Poll, Token};
use m6_core::site::Site;
use m6_core::testkit::alloc_count;

#[global_allocator]
static ALLOC: alloc_count::Counting = alloc_count::Counting;

const SERVER: Token = Token(0);
const DEADLINE: Duration = Duration::from_secs(10);
const REQUEST: &[u8] = b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n";

/// What one request on the page path may allocate.
///
/// 71 when ALLOC-RENDER attributed it, on a tree that predates the four RS-2
/// removals and does not carry CACHE-HDR's `ETag`; 67 on integration 6bb7a2f
/// plus RS-2. The rendered-page job (I-177) removed the owned request, the
/// compression candidate `Vec` and two of the `ETag`'s four, cached
/// `Last-Modified`, made the dictionary's built-in keys borrow their
/// `&'static str` and made the template name an `Arc<str>`: 50 here. The
/// target is zero, and the remaining 13 are listed in
/// `work-queue/out/ALLOC-RENDER-ZERO.md`; a change that raises this is
/// refused, and one that lowers it records the new number in that commit.
/// The borrowed response head took nine of the 13 and the page copy one more:
/// 41. The reusable page context took the last four: 36. A traced run shows all
/// 36 are the libraries': minify-html's 21 and Tera's 15, and none is ours. A
/// revert of the page context puts this back to 41, and of both changes to 50.
const PAGE_BUDGET: usize = 36;

fn main() {
    let mut args = Arguments::from_args();
    // One thread: `libtest-mimic` then runs the trial in this (the main)
    // thread, so the counting allocator reads only the trial's own
    // allocations and no other thread's.
    args.test_threads = Some(1);
    let tests = vec![Trial::test(
        "page_path_allocations_do_not_grow",
        page_path_allocations_do_not_grow,
    )];
    libtest_mimic::run(&args, tests).exit();
}

fn page_path_allocations_do_not_grow() -> Result<(), Failed> {
    let (mut site, _dir) = site();
    let mut conn = connection();

    let mut counted = Vec::new();
    for i in 0..100 {
        let before = alloc_count::now();
        serve_one(&mut site, &mut conn);
        let allocations = alloc_count::now() - before;
        println!("request {i}: {allocations} allocation(s)");
        counted.push(allocations);
    }

    let first = counted[1];
    if counted[1..].iter().any(|&n| n != first) {
        return Err(format!("allocations vary per request: {counted:?}").into());
    }
    println!("page path allocations per request after the first: {first} (budget {PAGE_BUDGET})");
    if first != PAGE_BUDGET {
        return Err(format!(
            "the page path allocates {first} times, budget {PAGE_BUDGET}: a per-request \
             allocation has come back, or a removal was not recorded"
        )
        .into());
    }
    Ok(())
}

/// A site over a temp directory: one template route and one params file.
fn site() -> (Site, tempfile::TempDir) {
    let dir = tempfile::TempDir::new().expect("a temp site");
    std::fs::create_dir_all(dir.path().join("templates")).expect("templates");
    std::fs::write(
        dir.path().join("templates/index.html"),
        "<html><body><h1>{{ title }}</h1></body></html>",
    )
    .expect("template");
    std::fs::write(dir.path().join("data.json"), "{\"title\": \"Hello\"}").expect("params");
    std::fs::write(
        dir.path().join("site.toml"),
        "[[backend]]\nname = \"m6-site\"\ncompresses = true\n",
    )
    .expect("site.toml");
    let config_path = dir.path().join("m6-site.toml");
    std::fs::write(
        &config_path,
        "[[route]]\npath = \"/\"\ntemplate = \"templates/index.html\"\n\
         params = [\"data.json\"]\n",
    )
    .expect("config");

    #[cfg(feature = "templates")]
    let renderer: &dyn m6_core::RendererFactory = &m6_core::TeraFactory;
    #[cfg(not(feature = "templates"))]
    let renderer: &dyn m6_core::RendererFactory = &m6_core::NoTemplates;

    let site = Site::load_for_tests(
        "m6-site",
        dir.path().to_path_buf(),
        config_path,
        &[],
        &[],
        renderer,
    );
    (site, dir)
}

struct Conn {
    poll: Poll,
    events: Events,
    client: StdUnixStream,
    conn: ServerConn,
}

fn connection() -> Conn {
    let (client, server) = StdUnixStream::pair().expect("a socket pair");
    server.set_nonblocking(true).expect("non-blocking");
    client
        .set_read_timeout(Some(DEADLINE))
        .expect("a read deadline");
    client
        .set_write_timeout(Some(DEADLINE))
        .expect("a write deadline");
    let mut conn = ServerConn::new(16 * 1024, 64 * 1024);
    conn.attach(platform::net::UnixStream::from_std(server));
    let poll = Poll::new().expect("a poll");
    poll.registry()
        .register(conn.stream().expect("attached"), SERVER, Interest::READABLE)
        .expect("register");
    Conn {
        poll,
        events: Events::with_capacity(8),
        client,
        conn,
    }
}

/// One request and its full answer.
fn serve_one(site: &mut Site, c: &mut Conn) {
    c.client.write_all(REQUEST).expect("write the request");
    loop {
        c.events.clear();
        c.poll
            .poll(&mut c.events, Some(DEADLINE))
            .expect("a poll with a deadline");
        if c.events
            .iter()
            .any(|e| e.token() == SERVER && e.is_readable())
        {
            break;
        }
    }

    let mut response = None;
    loop {
        let mut heads = h1::head_slots();
        let read = c.conn.read(&mut heads).expect("read");
        match read {
            Read::Request(head) => match site.handle(&head) {
                m6_core::site::Handled::Done(answer) => response = Some(answer),
                m6_core::site::Handled::Code(..) | m6_core::site::Handled::Named(..) => {
                    panic!("this site has no service routes")
                }
                m6_core::site::Handled::Reload(_) => panic!("this test sends no /genid"),
            },
            Read::More => break,
            Read::Closed => panic!("the client stayed open"),
        }
        let flush = c.conn.flush().expect("flush");
        assert!(
            matches!(flush, Flush::Done | Flush::Close),
            "the answer must go out"
        );
        if response.is_some() {
            break;
        }
    }
    let response = response.expect("the site answers its one route");
    c.conn.respond_response(response);
    loop {
        match c.conn.flush().expect("flush") {
            Flush::Done => break,
            Flush::More => {
                c.events.clear();
                c.poll
                    .registry()
                    .reregister(
                        c.conn.stream().expect("attached"),
                        SERVER,
                        Interest::WRITABLE,
                    )
                    .expect("reregister");
                loop {
                    c.events.clear();
                    c.poll
                        .poll(&mut c.events, Some(DEADLINE))
                        .expect("a poll with a deadline");
                    if c.events
                        .iter()
                        .any(|e| e.token() == SERVER && e.is_writable())
                    {
                        break;
                    }
                }
                c.poll
                    .registry()
                    .reregister(
                        c.conn.stream().expect("attached"),
                        SERVER,
                        Interest::READABLE,
                    )
                    .expect("reregister");
            }
            // The hundredth request on one connection closes it; its answer is
            // already written by then.
            Flush::Close => break,
        }
    }

    read_response(&mut c.client);
}

/// Read one complete response into a fixed buffer, then drop it. No allocation:
/// the count includes this call.
fn read_response(client: &mut StdUnixStream) {
    let mut buf = [0u8; 64 * 1024];
    let mut filled = 0;
    loop {
        if let Some(end) = buf[..filled].windows(4).position(|w| w == b"\r\n\r\n") {
            let head_end = end + 4;
            if let Some(length) = content_length(&buf[..end]) {
                if filled >= head_end + length {
                    assert!(
                        buf.starts_with(b"HTTP/1.1 200 OK\r\n"),
                        "the page must render: {:?}",
                        String::from_utf8_lossy(&buf[..head_end])
                    );
                    return;
                }
            }
        }
        let n = client.read(&mut buf[filled..]).expect("read the response");
        assert!(n > 0, "the peer closed before the answer was complete");
        filled += n;
    }
}

/// The `Content-Length` value of a response head, parsed without allocating.
fn content_length(head: &[u8]) -> Option<usize> {
    let text = std::str::from_utf8(head).ok()?;
    for line in text.split("\r\n") {
        if let Some(value) = line.strip_prefix("Content-Length: ") {
            return value.trim().parse().ok();
        }
    }
    None
}
