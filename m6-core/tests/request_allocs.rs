//! The page path's per-request allocation counts, so one that comes back is seen.
//!
//! Two checks on one thread. The first measures what the compression block
//! costs a response the client asked to keep `identity`: it may not copy the
//! whole body for a coding it never applies. The second holds one handler
//! request to a recorded allocation count.
//!
//! `harness = false` and its own `main`, so the counting allocator is installed
//! before any thread starts and the count is only this binary's own.

use m6_core::h1::{parse_head, HeadResult};
use m6_core::site::{Handled, Site};
use m6_core::testkit::alloc_count;
use m6_core::Response;

#[global_allocator]
static ALLOC: alloc_count::Counting = alloc_count::Counting;

/// The body both compression routes answer with.
const BODY_LEN: usize = 64 * 1024;

/// What the compression block cost a `text/plain` response requested with
/// `Accept-Encoding: identity`, over the same body answered as a type with no
/// compression level (so the block is skipped): the candidate list and the
/// `to_vec` of the whole body. With the copy gone the candidate list is all
/// that is left, so the difference must drop by one.
///
/// Measured on 2292e60 (the branch RS-1 left), before the copy was removed.
const BASE_IDENTITY_DIFFERENCE: usize = 2;

/// The allocations one `/ok` request makes with every removal in place. The
/// count is deterministic for a given dependency set: a dependency update that
/// moves it records the new count, with the reason, in that commit.
const HANDLER_BUDGET: usize = 29;

const BIG: &[u8] = b"GET /big HTTP/1.1\r\nHost: localhost\r\nAccept-Encoding: identity\r\n\r\n";
const RAW: &[u8] = b"GET /raw HTTP/1.1\r\nHost: localhost\r\nAccept-Encoding: identity\r\n\r\n";
const OK: &[u8] = b"GET /ok HTTP/1.1\r\nHost: localhost\r\nCookie: _csrf=abc; a=b\r\n\
                    Accept-Encoding: identity\r\n\r\n";

/// One counted request: the bytes, the handler index the config gives it, and
/// the answer the service builds for it.
struct Route {
    request: &'static [u8],
    index: usize,
    answer: fn() -> Response,
}

fn main() {
    let (site, _dir) = site();
    let routes = routes();

    // Every request is sent once before it is counted, so a first-use
    // allocation (a lazy table, a cache entry) is not in the count.
    for route in &routes {
        let warm = count_request(&site, route);
        assert!(warm > 0, "a request allocates something");
    }

    let big = count_request(&site, &routes[0]);
    let raw = count_request(&site, &routes[1]);
    let difference = big - raw;
    let ok = count_request(&site, &routes[2]);

    println!(
        "identity difference: {difference} (expected {})",
        BASE_IDENTITY_DIFFERENCE - 1
    );
    println!("handler request: {ok} allocation(s) (budget {HANDLER_BUDGET})");

    let mut failed = false;
    if difference != BASE_IDENTITY_DIFFERENCE - 1 {
        println!(
            "FAILED: an identity response costs {difference} more allocations than an \
             uncompressed type, expected {}: the body is copied for compression that is \
             not applied",
            BASE_IDENTITY_DIFFERENCE - 1
        );
        failed = true;
    }
    if ok != HANDLER_BUDGET {
        println!(
            "FAILED: a handler request allocates {ok} times, budget {HANDLER_BUDGET}: a \
             per-request allocation has come back"
        );
        failed = true;
    }
    if failed {
        std::process::exit(1);
    }
    println!("request allocations: within budget");
}

/// The three counted requests, in the order the checks run.
fn routes() -> [Route; 3] {
    [
        Route {
            request: BIG,
            index: 0,
            answer: big_response,
        },
        Route {
            request: RAW,
            index: 1,
            answer: raw_response,
        },
        Route {
            request: OK,
            index: 2,
            answer: ok_response,
        },
    ]
}

/// The same 64 KiB body as `/raw`, typed so the compression block runs.
fn big_response() -> Response {
    Response::status(200)
        .body(vec![b'a'; BODY_LEN])
        .header("Content-Type", "text/plain")
}

/// The same 64 KiB body, typed so the compression block is skipped.
fn raw_response() -> Response {
    Response::status(200)
        .body(vec![b'a'; BODY_LEN])
        .header("Content-Type", "application/octet-stream")
}

/// A short handler answer, for the budget check.
fn ok_response() -> Response {
    Response::text("ok")
}

/// One request's allocations: `handle`, then `finish` with an answer built
/// between the two windows, so the test's own response is outside the count.
fn count_request(site: &Site, route: &Route) -> usize {
    let mut slots = [httparse::EMPTY_HEADER; 64];
    let head = match parse_head(route.request, &mut slots) {
        HeadResult::Complete(head) => head,
        _ => panic!("the test's request must parse"),
    };

    let before_handle = alloc_count::now();
    let handled = site.handle(&head);
    let after_handle = alloc_count::now();

    let req = match handled {
        Handled::Named(index, req) => {
            assert_eq!(index, route.index, "a config route must name its handler");
            req
        }
        Handled::Code(..) => panic!("a config route naming a handler comes back Named"),
        Handled::Done(_) => panic!("a config route naming a handler is answered by it"),
    };
    let response = (route.answer)();

    let before_finish = alloc_count::now();
    let finished = site.finish(&req, response);
    let after_finish = alloc_count::now();

    assert_eq!(finished.status, 200, "the route must answer");
    (after_handle - before_handle) + (after_finish - before_finish)
}

/// A site over a temp directory: three config routes, one per counted request.
fn site() -> (Site, tempfile::TempDir) {
    let dir = tempfile::TempDir::new().expect("a temp site");
    std::fs::create_dir_all(dir.path().join("templates")).expect("templates");
    std::fs::write(
        dir.path().join("site.toml"),
        "[[backend]]\nname = \"m6-site\"\ncompresses = true\n",
    )
    .expect("site.toml");
    let config_path = dir.path().join("m6-site.toml");
    std::fs::write(
        &config_path,
        "[[route]]\npath = \"/big\"\nhandler = \"big\"\n\
         [[route]]\npath = \"/raw\"\nhandler = \"raw\"\n\
         [[route]]\npath = \"/ok\"\nhandler = \"ok\"\n",
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
        &["big", "raw", "ok"],
        renderer,
    );
    (site, dir)
}
