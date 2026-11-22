//! Where one measured request's allocations come from, grouped by call site.
//!
//! This is diagnostic, not a gate: it turns on `alloc_count`'s backtrace
//! recording around one `/ok` request and one `/page` request and prints each
//! allocation grouped by the first frame in `m6-core` or `m6-http`. Run it on
//! two trees and diff the printed lists to see which site is new. It builds
//! only with `--features testkit,alloc-trace`; the `alloc-trace` feature is
//! off by default, so no release build and no ordinary test build carries the
//! recording code.
//!
//! `harness = false` and its own `main`, as `request_allocs`'s, so the counting
//! allocator is installed before any thread starts and the trace is only this
//! binary's own.

use m6_core::h1::{parse_head, HeadResult};
use m6_core::site::{Handled, Site};
use m6_core::testkit::alloc_count;

#[global_allocator]
static ALLOC: m6_core::alloc::M6Alloc = m6_core::alloc::M6Alloc;

const OK: &[u8] = b"GET /ok HTTP/1.1\r\nHost: localhost\r\nCookie: _csrf=abc; a=b\r\n\
                    Accept-Encoding: identity\r\n\r\n";
const PAGE: &[u8] = b"GET /page HTTP/1.1\r\nHost: localhost\r\nAccept-Encoding: identity\r\n\r\n";

fn main() {
    let (site, _dir) = site();

    // Warm every request first, as `request_allocs` does, so a first-use
    // allocation (a lazy table, a cache entry) is not in a measured window.
    measure_handler(&site);
    measure_template(&site);

    report_handler(&site);
    report_template(&site);
}

/// A handler route is answered in two windows: `handle`, then the test builds
/// its response, then `finish`. The response is built outside the trace, so
/// this is the same per-request window `request_allocs` counts.
fn report_handler(site: &Site) {
    let mut slots = [httparse::EMPTY_HEADER; 64];
    let head = match parse_head(OK, &mut slots) {
        HeadResult::Complete(head) => head,
        _ => panic!("the test's request must parse"),
    };
    alloc_count::trace::start();
    let handled = site.handle(&head);
    let handle_sites = alloc_count::trace::stop();

    let req = match handled {
        Handled::Named(index, req) => {
            assert_eq!(index, 0, "the config names /ok's handler");
            req
        }
        Handled::Code(..) | Handled::Done(_) => panic!("/ok is a named handler route"),
        Handled::Reload(_) => panic!("/ok is not a reloaded route"),
    };
    let response = m6_core::Response::text("ok");
    alloc_count::trace::start();
    let finished = site.finish(&req, response);
    let finish_sites = alloc_count::trace::stop();
    assert_eq!(finished.status, 200, "the route must answer");

    report("handler /ok, Site::handle", &handle_sites);
    report("handler /ok, Site::finish", &finish_sites);
}

/// A template route is rendered and finished by `Site::handle`, so its one
/// window contains the render, as `request_allocs`'s template window does.
fn report_template(site: &Site) {
    let mut slots = [httparse::EMPTY_HEADER; 64];
    let head = match parse_head(PAGE, &mut slots) {
        HeadResult::Complete(head) => head,
        _ => panic!("the test's request must parse"),
    };
    alloc_count::trace::start();
    let handled = site.handle(&head);
    let sites = alloc_count::trace::stop();
    match handled {
        Handled::Done(resp) => assert_eq!(resp.status, 200, "the template route must answer"),
        Handled::Named(..) | Handled::Code(..) => {
            panic!("a template route is answered by the site")
        }
        Handled::Reload(_) => panic!("a template route is not reloaded"),
    }
    report("template /page, Site::handle", &sites);
}

/// One measured window: the same request the budget test counts, but with the
/// allocator recording where each allocation came from.
fn measure_handler(site: &Site) {
    let mut slots = [httparse::EMPTY_HEADER; 64];
    let head = match parse_head(OK, &mut slots) {
        HeadResult::Complete(head) => head,
        _ => panic!("the test's request must parse"),
    };
    let handled = site.handle(&head);
    let req = match handled {
        Handled::Named(_, req) => req,
        Handled::Code(..) | Handled::Done(_) => panic!("/ok is a named handler route"),
        Handled::Reload(_) => panic!("/ok is not a reloaded route"),
    };
    let response = m6_core::Response::text("ok");
    let finished = site.finish(&req, response);
    assert_eq!(finished.status, 200, "the route must answer");
}

fn measure_template(site: &Site) {
    let mut slots = [httparse::EMPTY_HEADER; 64];
    let head = match parse_head(PAGE, &mut slots) {
        HeadResult::Complete(head) => head,
        _ => panic!("the test's request must parse"),
    };
    match site.handle(&head) {
        Handled::Done(resp) => assert_eq!(resp.status, 200, "the template route must answer"),
        Handled::Named(..) | Handled::Code(..) => {
            panic!("a template route is answered by the site")
        }
        Handled::Reload(_) => panic!("a template route is not reloaded"),
    }
}

/// Print one window's groups, one line per call site, with the total. The
/// first line is a stable header a diff can key on.
fn report(label: &str, sites: &[(String, usize)]) {
    let total: usize = sites.iter().map(|(_, count)| count).sum();
    let mut grouped: std::collections::BTreeMap<&str, usize> = std::collections::BTreeMap::new();
    for (site, count) in sites {
        *grouped.entry(site.as_str()).or_default() += count;
    }
    let mut grouped: Vec<(&str, usize)> = grouped.into_iter().collect();
    grouped.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
    println!("\n== {label}: {total} allocation(s)");
    for (site, count) in grouped {
        println!("{count:4}  {site}");
    }
}

/// A site over a temp directory: the same two config routes the budget test
/// counts, set up the same way.
fn site() -> (Site, tempfile::TempDir) {
    let dir = tempfile::TempDir::new().expect("a temp site");
    std::fs::create_dir_all(dir.path().join("templates")).expect("templates");
    std::fs::write(dir.path().join("templates/page.html.tmpl"), "<h1>page</h1>").expect("template");
    std::fs::write(
        dir.path().join("site.toml"),
        "[[backend]]\nname = \"m6-site\"\ncompresses = true\n",
    )
    .expect("site.toml");
    let config_path = dir.path().join("m6-site.toml");
    std::fs::write(
        &config_path,
        "[[route]]\npath = \"/ok\"\nhandler = \"ok\"\n\
         [[route]]\npath = \"/page\"\ntemplate = \"templates/page.html.tmpl\"\n",
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
        &["ok"],
        renderer,
    );
    (site, dir)
}
