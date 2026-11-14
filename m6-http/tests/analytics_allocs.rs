//! The per-request analytics line allocates nothing.
//!
//! `harness = false` means no libtest thread shares this binary, so the
//! counting allocator reads only what `record` allocated. The analytics Sink is
//! opened before the count, as it is at start, because `record` builds its line
//! into that Sink: with no Sink the line is dropped and a zero would prove
//! nothing.
//!
//! The first loop measures a request that already carries a session cookie, a
//! referer and a user agent, which is the returning visitor's path. The second
//! measures a request with no cookie, which mints a new session, so the random
//! id and its 32-byte stack buffer are counted too. The CSPRNG's thread-local is
//! created on first use, so both paths run once before the count starts. The
//! number of lines in the file is checked afterwards, so the zero cannot come
//! from a line that was never built.

use m6_core::log;
use m6_core::testkit::alloc_count;
use m6_http_lib::analytics::{self, Event, SessionId};
use m6_http_lib::health::TrafficWindow;

#[global_allocator]
static ALLOC: alloc_count::Counting = alloc_count::Counting;

const REQUESTS: usize = 1_000;

fn main() {
    let dir = tempfile::tempdir().expect("a temp directory");
    let path = dir.path().join("analytics.jsonl");
    log::init(log::Level::Info, Some(path.as_path())).expect("the analytics sink");

    let with_cookie = vec![
        (
            "Cookie".to_string(),
            "_m6sid=0f1e2d3c4b5a69788796a5b4c3d2e1f0".to_string(),
        ),
        (
            "Referer".to_string(),
            "https://example.com/some/page?q=1".to_string(),
        ),
        (
            "User-Agent".to_string(),
            "Mozilla/5.0 (compatible) Test".to_string(),
        ),
    ];
    let no_cookie = vec![
        (
            "Referer".to_string(),
            "https://example.com/some/page?q=1".to_string(),
        ),
        (
            "User-Agent".to_string(),
            "Mozilla/5.0 (compatible) Test".to_string(),
        ),
    ];
    let ev = Event {
        node: "dev",
        path: "/some/page",
        status: 200,
        cache_state: "HIT",
        client_ip: "192.0.2.1",
        latency_ns: Some(1_234),
    };
    let mut window = TrafficWindow::new();

    // Warm: the CSPRNG's thread-local, the line clock and the Sink's buffers are
    // created on first use, and that first use is not the path being measured.
    let _: Option<SessionId> = analytics::record(true, &with_cookie, &ev, &mut window);
    let _: Option<SessionId> = analytics::record(true, &no_cookie, &ev, &mut window);

    let start = alloc_count::now();
    for _ in 0..REQUESTS {
        let _: Option<SessionId> = analytics::record(true, &with_cookie, &ev, &mut window);
    }
    let returning = alloc_count::now() - start;

    let start = alloc_count::now();
    for _ in 0..REQUESTS {
        let _: Option<SessionId> = analytics::record(true, &no_cookie, &ev, &mut window);
    }
    let new_session = alloc_count::now() - start;

    log::stop().expect("the analytics sink closes");

    // A line was built for every call, so a zero below is the line path's and
    // not an empty Sink's.
    let written = std::fs::read_to_string(&path).expect("the analytics file");
    let lines = written.lines().count();
    let expected = 2 + 2 * REQUESTS;
    let bytes_each = written.len() / lines;
    drop(dir);

    let allocations = returning + new_session;
    let lines_ok = lines == expected;
    println!(
        "recording_a_request_allocates_nothing: {returning} allocation(s) in {REQUESTS} \
         returning requests, {new_session} in {REQUESTS} new-session requests, \
         {lines} line(s) written (expected {expected}), {bytes_each} bytes a line"
    );
    std::process::exit(if allocations == 0 && lines_ok { 0 } else { 1 });
}
