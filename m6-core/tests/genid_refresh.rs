//! `POST /genid` runs the start path again and swaps the `Site`.
//!
//! The page path, the reserved route and a failed load are driven in process
//! through a `ServerConn` pair, the shape `site_allocs.rs` uses. Nothing here
//! starts a release binary: the site is built with `Site::load_for_tests`, so
//! every wait is one read or one poll with a deadline.
//!
//! Four claims, each the one its test is named for:
//!
//! - a reload reads the files again and serves the new content,
//! - an answer the old site already serialised into the connection is written
//!   whole after the swap,
//! - a load that fails keeps the old site serving and names the cause,
//! - the reserved route answers only `POST /genid`, with no query and no
//!   trailing slash, and the socket keeps its default mode as the only ACL.

#![allow(
    clippy::disallowed_methods,
    reason = "test code: a blocking client with deadlines drives the slot from outside"
)]

use std::io::{Read as _, Write as _};
use std::os::unix::net::UnixStream as StdUnixStream;
use std::path::Path;
use std::time::Duration;

use m6_core::h1::{self, Flush, Read, ServerConn};
use m6_core::platform::net::UnixStream;
use m6_core::platform::{Events, Interest, Poll, Token};
use m6_core::{genid_failed, Handled, Response, Site, TeraFactory};

const SERVER: Token = Token(0);
const DEADLINE: Duration = Duration::from_secs(10);
const GET: &[u8] = b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n";

/// One site over the temp directory, and the directory it reads from.
fn site(dir: &Path) -> Site {
    #[cfg(feature = "templates")]
    let renderer: &dyn m6_core::RendererFactory = &TeraFactory;
    #[cfg(not(feature = "templates"))]
    let renderer: &dyn m6_core::RendererFactory = &m6_core::NoTemplates;
    Site::load_for_tests(
        "m6-site",
        dir.to_path_buf(),
        dir.join("m6-site.toml"),
        &[],
        &[],
        renderer,
    )
}

/// A site with one template route on `/` and a params file the page reads.
fn page_site() -> (Site, tempfile::TempDir) {
    let dir = tempfile::TempDir::new().expect("a temp site");
    std::fs::create_dir_all(dir.path().join("templates")).expect("templates");
    std::fs::create_dir_all(dir.path().join("data")).expect("data");
    std::fs::write(
        dir.path().join("templates/index.html"),
        "first: {{ title }}",
    )
    .expect("template");
    std::fs::write(dir.path().join("data/page.json"), "{\"title\": \"one\"}").expect("params");
    std::fs::write(
        dir.path().join("site.toml"),
        "[[backend]]\nname = \"m6-site\"\ncompresses = true\n",
    )
    .expect("site.toml");
    std::fs::write(
        dir.path().join("m6-site.toml"),
        "[[route]]\npath = \"/\"\ntemplate = \"templates/index.html\"\n\
         params = [\"data/page.json\"]\n",
    )
    .expect("config");
    let site = site(dir.path());
    (site, dir)
}

/// A site with no routes, to test the reserved path itself.
fn empty_site() -> (Site, tempfile::TempDir) {
    let dir = tempfile::TempDir::new().expect("a temp site");
    std::fs::create_dir_all(dir.path().join("templates")).expect("templates");
    std::fs::write(
        dir.path().join("site.toml"),
        "[[backend]]\nname = \"m6-site\"\ncompresses = true\n",
    )
    .expect("site.toml");
    std::fs::write(dir.path().join("m6-site.toml"), "").expect("config");
    let site = site(dir.path());
    (site, dir)
}

struct Pair {
    poll: Poll,
    events: Events,
    client: StdUnixStream,
    conn: ServerConn,
}

fn connect() -> Pair {
    let (client, server) = StdUnixStream::pair().expect("a socket pair");
    server.set_nonblocking(true).expect("non-blocking");
    client
        .set_read_timeout(Some(DEADLINE))
        .expect("a read deadline");
    client
        .set_write_timeout(Some(DEADLINE))
        .expect("a write deadline");
    let mut conn = ServerConn::new(16 * 1024, 64 * 1024);
    conn.attach(UnixStream::from_std(server));
    let poll = Poll::new().expect("a poll");
    poll.registry()
        .register(conn.stream().expect("attached"), SERVER, Interest::READABLE)
        .expect("register");
    Pair {
        poll,
        events: Events::with_capacity(8),
        client,
        conn,
    }
}

fn wait_readable(p: &mut Pair) {
    loop {
        p.events.clear();
        p.poll
            .poll(&mut p.events, Some(DEADLINE))
            .expect("a poll with a deadline");
        if p.events
            .iter()
            .any(|e| e.token() == SERVER && e.is_readable())
        {
            return;
        }
    }
}

fn wait_writable(p: &mut Pair) {
    p.poll
        .registry()
        .reregister(
            p.conn.stream().expect("attached"),
            SERVER,
            Interest::WRITABLE,
        )
        .expect("reregister");
    loop {
        p.events.clear();
        p.poll
            .poll(&mut p.events, Some(DEADLINE))
            .expect("a poll with a deadline");
        if p.events
            .iter()
            .any(|e| e.token() == SERVER && e.is_writable())
        {
            return;
        }
    }
}

/// Write one request, read its head and dispatch it. The answer is queued by
/// the caller, so a test that reloads between the two keeps the queued bytes.
fn dispatch(site: &Site, p: &mut Pair, request: &[u8]) -> Handled {
    p.client.write_all(request).expect("write the request");
    loop {
        wait_readable(p);
        let mut heads = h1::head_slots();
        match p.conn.read(&mut heads).expect("read the request") {
            Read::Request(head) => return site.handle(&head),
            Read::More => continue,
            Read::Closed => panic!("the client stayed open"),
        }
    }
}

/// Write every queued byte, then read the answer the peer received.
fn finish_write(p: &mut Pair) -> Vec<u8> {
    loop {
        match p.conn.flush().expect("flush") {
            Flush::Done | Flush::Close => break,
            Flush::More => wait_writable(p),
        }
    }
    p.poll
        .registry()
        .reregister(
            p.conn.stream().expect("attached"),
            SERVER,
            Interest::READABLE,
        )
        .expect("reregister");
    read_wire(p)
}

/// One request, answered and written whole.
fn exchange(site: &Site, p: &mut Pair, request: &[u8]) -> Vec<u8> {
    match dispatch(site, p, request) {
        Handled::Done(response) => {
            p.conn.respond_response(response);
            finish_write(p)
        }
        _ => panic!("the page route must be answered by the site"),
    }
}

fn read_wire(p: &mut Pair) -> Vec<u8> {
    let mut out = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        if let Some(head_end) = find_head_end(&out) {
            if let Some(length) = content_length(&out[..head_end - 4]) {
                if out.len() >= head_end + length {
                    return out;
                }
            }
        }
        let n = p.client.read(&mut buf).expect("read the answer");
        assert!(n > 0, "the peer closed before the answer was complete");
        out.extend_from_slice(&buf[..n]);
    }
}

fn find_head_end(bytes: &[u8]) -> Option<usize> {
    bytes
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|i| i + 4)
}

fn content_length(head: &[u8]) -> Option<usize> {
    let text = std::str::from_utf8(head).ok()?;
    for line in text.split("\r\n") {
        if let Some(value) = line.strip_prefix("Content-Length: ") {
            return value.trim().parse().ok();
        }
    }
    None
}

fn body_of(wire: &[u8]) -> &[u8] {
    &wire[find_head_end(wire).expect("a complete head")..]
}

fn status_of(response: &Response) -> u16 {
    response.status
}

fn header<'a>(response: &'a Response, name: &str) -> Option<&'a str> {
    response
        .headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

fn rewrite(dir: &Path) {
    std::fs::write(dir.join("templates/index.html"), "second: {{ title }}").expect("new template");
    std::fs::write(dir.join("data/page.json"), "{\"title\": \"two\"}").expect("new params");
}

/// The reload is the start path again: the config, templates and params are
/// read from disk and the new Site serves them. The old Site keeps its own.
#[test]
fn a_reload_serves_new_content() {
    let (mut site, dir) = page_site();
    let mut p = connect();

    let first = exchange(&site, &mut p, GET);
    assert_eq!(
        body_of(&first),
        b"first: one",
        "the page starts with what it loaded"
    );

    rewrite(dir.path());
    let still_first = exchange(&site, &mut p, GET);
    assert_eq!(
        body_of(&still_first),
        b"first: one",
        "the old Site serves what it loaded until it is replaced"
    );

    let next = site
        .reload(&[], &[], &TeraFactory)
        .expect("the start path reads the new files");
    site = next;

    let second = exchange(&site, &mut p, GET);
    assert_eq!(
        body_of(&second),
        b"second: two",
        "the new Site serves the content the reload read"
    );
}

/// A response the old Site already serialised is bytes the connection owns.
/// The swap fills the loop's `site` binding and touches no connection, so the
/// queued answer goes out exactly as the old Site built it, and the next
/// request on that connection sees the new Site.
#[test]
fn an_answer_queued_before_the_swap_is_written_whole() {
    let (mut site, dir) = page_site();
    let mut p = connect();

    let control = exchange(&site, &mut p, GET);
    assert_eq!(body_of(&control), b"first: one");

    let queued = match dispatch(&site, &mut p, GET) {
        Handled::Done(response) => response,
        _ => panic!("the page route must be answered by the site"),
    };
    p.conn.respond_response(queued);
    assert!(
        p.conn.buffer_caps().1 > 0,
        "the answer must be queued in the connection, not flushed"
    );

    rewrite(dir.path());
    let next = site
        .reload(&[], &[], &TeraFactory)
        .expect("the start path reads the new files");
    site = next;

    let written = finish_write(&mut p);
    assert_eq!(
        written, control,
        "the queued answer must equal what the old Site built, byte for byte"
    );

    let after = exchange(&site, &mut p, GET);
    assert_eq!(
        body_of(&after),
        b"second: two",
        "the same connection's next request is served by the new Site"
    );
}

/// A load that fails assigns nothing. The old Site keeps serving, and the
/// answer names the cause a start would have printed.
#[test]
fn a_failed_load_keeps_the_old_site_serving_and_is_reported() {
    let (site, dir) = page_site();
    let mut p = connect();

    let first = exchange(&site, &mut p, GET);
    assert_eq!(body_of(&first), b"first: one");

    std::fs::write(dir.path().join("templates/index.html"), "{% if %}").expect("broken template");

    let error = match site.reload(&[], &[], &TeraFactory) {
        Ok(_) => panic!("a template that does not parse fails the load"),
        Err(error) => error,
    };
    assert!(
        error.contains("compiling templates"),
        "the error must name the failed step: {error}"
    );

    let still_first = exchange(&site, &mut p, GET);
    assert_eq!(
        body_of(&still_first),
        b"first: one",
        "nothing was swapped: the old Site still serves"
    );

    let answer = genid_failed(&error);
    assert_eq!(status_of(&answer), 500);
    assert_eq!(header(&answer, "cache-control"), Some("no-store"));
    assert_eq!(
        answer.body.as_bytes(),
        Some(format!("content refresh failed: {error}").as_bytes()),
        "the body names the cause"
    );
}

/// Only `POST /genid` is the refresh. Another method is a 405 with the one
/// allowed method, and a query or a trailing slash is routed as usual, so it
/// is the ordinary 404. With no `[server]` table the socket mode is the
/// default, which is the whole ACL the protocol gives this request.
#[test]
fn the_route_answers_only_the_exact_post() {
    let (site, _dir) = empty_site();
    let mut p = connect();

    match dispatch(
        &site,
        &mut p,
        b"POST /genid HTTP/1.1\r\nHost: localhost\r\n\r\n",
    ) {
        Handled::Reload => {}
        _ => panic!("POST /genid must ask the loop to reload"),
    }

    for request in [
        &b"GET /genid HTTP/1.1\r\nHost: localhost\r\n\r\n"[..],
        b"PUT /genid HTTP/1.1\r\nHost: localhost\r\n\r\n",
    ] {
        match dispatch(&site, &mut p, request) {
            Handled::Done(response) => {
                assert_eq!(status_of(&response), 405, "the method is refused");
                assert_eq!(header(&response, "allow"), Some("POST"));
                assert_eq!(header(&response, "cache-control"), Some("no-store"));
            }
            _ => panic!("a non-POST on /genid is answered by the site"),
        }
    }

    for request in [
        &b"POST /genid?x=1 HTTP/1.1\r\nHost: localhost\r\n\r\n"[..],
        b"POST /genid/ HTTP/1.1\r\nHost: localhost\r\n\r\n",
    ] {
        match dispatch(&site, &mut p, request) {
            Handled::Done(response) => assert_eq!(
                status_of(&response),
                404,
                "the reserved path is exactly /genid with no query"
            ),
            _ => panic!("a near miss falls through to routing, where no route answers it"),
        }
    }

    assert_eq!(
        site.socket_mode(),
        0o660,
        "the socket's directory permissions are the only authentication"
    );
}
