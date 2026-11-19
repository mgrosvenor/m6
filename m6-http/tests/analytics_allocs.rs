//! The per-request analytics line allocates nothing in `record`.
//!
//! What this measures is the line path: the borrowed features, the referer
//! slices, the session scan and the Sink write. A new session's `Set-Cookie`
//! value is built later, where the response headers are written, and is not in
//! this count: it is one `String` per new session, once per visitor rather than
//! once per request, and it is the only allocation this work left in the
//! analytics path.
//!
//! `harness = false` means no libtest thread shares this binary, so the
//! counting allocator reads only what `record` allocated. A custom harness must
//! still speak libtest's protocol (`--list --format terse`, `--exact <name>`),
//! which `libtest-mimic` does, with the check forced onto the main thread. The
//! one Sink is opened before the count, as it is at start, with stdout pointed
//! at a file so the lines can be counted: with no Sink the line is dropped and
//! a zero would prove nothing.
//!
//! The first loop measures a request that already carries a session cookie, a
//! referer and a user agent, which is the returning visitor's path. The second
//! measures a request with no cookie, which mints a new session, so the random
//! id and its 32-byte stack buffer are counted too. The CSPRNG's thread-local is
//! created on first use, so both paths run once before the count starts. The
//! number of lines in the file is checked afterwards, so the zero cannot come
//! from a line that was never built.

use libtest_mimic::{Arguments, Failed, Trial};
use m6_core::log;
use m6_core::testkit::alloc_count;
use m6_http_lib::analytics::{self, Event, SessionId};
use m6_http_lib::health::TrafficWindow;

#[global_allocator]
static ALLOC: m6_core::alloc::M6Alloc = m6_core::alloc::M6Alloc;

const REQUESTS: usize = 1_000;

fn main() {
    let mut args = Arguments::from_args();
    // One thread: `libtest-mimic` then runs the check in this (the main)
    // thread, so the counting allocator reads only the check's own allocations.
    args.test_threads = Some(1);
    let tests = vec![Trial::test(
        "recording_a_request_allocates_nothing",
        recording_a_request_allocates_nothing,
    )];
    libtest_mimic::run(&args, tests).exit();
}

fn recording_a_request_allocates_nothing() -> Result<(), Failed> {
    let dir = tempfile::tempdir().expect("a temp directory");
    let path = dir.path().join("analytics.jsonl");
    let out = std::fs::File::options()
        .create(true)
        .append(true)
        .open(&path)
        .expect("the stdout file");
    let saved = m6_core::platform::fd::dup(&std::io::stdout()).expect("a copy of stdout");
    m6_core::platform::fd::dup2_stdout(&out).expect("stdout is the file");
    log::init(log::Level::Info).expect("the one sink");

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
        let id = analytics::record(true, &with_cookie, &ev, &mut window);
        assert!(id.is_none(), "a request carrying the cookie adopts it");
    }
    let returning = alloc_count::now() - start;

    let start = alloc_count::now();
    for _ in 0..REQUESTS {
        let id = analytics::record(true, &no_cookie, &ev, &mut window);
        assert!(
            id.is_some(),
            "a request with no cookie gets a new session id"
        );
    }
    let new_session = alloc_count::now() - start;

    log::stop().expect("the sink closes");

    // A line was built for every call, so a zero below is the line path's and
    // not an empty Sink's.
    let written = std::fs::read_to_string(&path).expect("the written lines");
    let lines = written.lines().count();
    let expected = 2 + 2 * REQUESTS;
    let bytes_each = written.len() / lines;
    m6_core::platform::fd::dup2_stdout(&saved).expect("stdout is back");
    drop(dir);

    let allocations = returning + new_session;
    let lines_ok = lines == expected;
    println!(
        "recording_a_request_allocates_nothing: {returning} allocation(s) in {REQUESTS} \
         returning requests, {new_session} in {REQUESTS} new-session requests, \
         {lines} line(s) written (expected {expected}), {bytes_each} bytes a line"
    );
    if allocations == 0 && lines_ok {
        Ok(())
    } else {
        Err(format!("{allocations} allocation(s), {lines} of {expected} line(s) written").into())
    }
}
