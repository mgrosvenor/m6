//! Do m6's two route matchers agree?
//!
//! Route matching is implemented twice. The edge maps a request path to a
//! backend with the `matchit` crate (`m6_http_lib::router`), and `m6-core` maps
//! a request path to a template or a handler with its own compiled segments
//! and fixed-then-pattern lookup (`m6_core::app`).
//!
//! A site author writes the same pattern vocabulary in both places: `site.toml`
//! names `/blog/{stem}` and a backend's own config names `/blog/{stem}` too. So
//! the two have to reach the same answer about which pattern wins, and nothing
//! checked that until this file.
//!
//! **What this file asserts is the CURRENT state, including where the two
//! disagree.** That is deliberate, and it is the same shape as
//! `m6_core::path`'s reconciliation test: pinning a known divergence means any
//! change to either matcher shows up here rather than in production. The
//! divergences are tracked as issue #177, and when core adopts `matchit` the
//! three pinned cases below become agreements and this file says so instead.

use std::sync::Arc;

use m6_core::app::{
    check_route_conflicts, compile_pattern, CompiledRoute, Methods, RouteDecl,
    RouteTable as CoreTable,
};
use m6_http_lib::router::RouteTable;
use serde_json::Map;

/// A core route table from patterns alone, which is what the edge's table is.
fn core_routes(patterns: &[&str]) -> CoreTable {
    CoreTable::new(
        patterns
            .iter()
            .map(|p| {
                let segments = compile_pattern(p);
                CompiledRoute {
                    pattern: (*p).to_string(),
                    methods: Methods::ANY,
                    segments,
                    template: Some("t.html".to_string()),
                    params_files: vec![],
                    status: 200,
                    cache: "public".to_string(),
                    headers: vec![],
                    last_modified: None,
                    handler: None,
                    settings: Arc::new(Map::new()),
                    base_dict: Arc::new(Map::new()),
                }
            })
            .collect(),
    )
}

fn core_winner<'a>(routes: &'a CoreTable, path: &str) -> Option<&'a str> {
    m6_core::app::find_route(path, "GET", routes).map(|(r, _)| r.pattern.as_str())
}

fn edge_winner<'a>(table: &'a RouteTable, path: &str) -> Option<&'a str> {
    table.at(path).map(|e| e.path.as_str())
}

fn edge_table(patterns: &[&str]) -> RouteTable {
    RouteTable::for_bench(
        &patterns
            .iter()
            .map(|p| (*p, None))
            .collect::<Vec<(&str, Option<&str>)>>(),
    )
}

/// Every path where the two matchers pick differently.
///
/// Returned rather than asserted so one run reports the whole set at once,
/// which is what makes the list actionable instead of a bisect.
fn divergences(patterns: &[&str], paths: &[&str]) -> Vec<(String, Option<String>, Option<String>)> {
    let routes = core_routes(patterns);
    let table = edge_table(patterns);
    paths
        .iter()
        .filter_map(|path| {
            let core = core_winner(&routes, path).map(str::to_string);
            let edge = edge_winner(&table, path).map(str::to_string);
            if core == edge {
                None
            } else {
                Some(((*path).to_string(), core, edge))
            }
        })
        .collect()
}

/// The vocabulary a real site writes, where the two must agree and do.
///
/// This is the half that matters in production: every pattern in the example
/// configs, against the paths a visitor actually sends.
#[test]
fn the_two_matchers_agree_on_what_sites_actually_write() {
    let patterns = &[
        "/",
        "/about",
        "/blog",
        "/blog/{stem}",
        "/members/{page}",
        "/api/admin/backends/{name}/sample",
        "/assets/{*relpath}",
    ];
    let paths = &[
        "/",
        "/about",
        "/blog",
        "/blog/hello-world",
        "/members/2",
        "/api/admin/backends/m6-html/sample",
        "/assets/css/site.css",
        "/assets/img/deep/nested/photo.jpg",
        "/nothing-here",
        "/blog/hello/extra",
    ];
    let found = divergences(patterns, paths);
    assert!(
        found.is_empty(),
        "the edge and core must agree on real patterns, and disagreed on: {found:?}"
    );
}

/// Precedence, where the two use different mechanisms and reach the same answer.
///
/// The edge uses `matchit`'s radix priority. Core looks a fixed route up by its
/// path and tries the patterns only after that. A literal has to beat a
/// parameter in both, and it does.
#[test]
fn the_two_matchers_agree_that_a_literal_beats_a_parameter() {
    let patterns = &[
        "/blog/feed",
        "/blog/{stem}",
        "/assets/style.css",
        "/assets/{name}",
    ];
    let paths = &[
        "/blog/feed",
        "/blog/anything",
        "/assets/style.css",
        "/assets/other.css",
    ];
    let found = divergences(patterns, paths);
    assert!(
        found.is_empty(),
        "the edge and core must agree on precedence, and disagreed on: {found:?}"
    );
}

/// **Pinned divergence, issue #177: path normalisation.**
///
/// Core splits on `/` and discards empty segments, so a trailing slash and a
/// doubled slash both collapse. The edge does not normalise, so it sees a
/// different path and finds no route.
///
/// No production request reaches this. The edge answers first, and it answers
/// `/blog/` with a 301 to `/blog` through `trailing_slash_redirect` rather than
/// a 404. The edge's reading is also the more correct one: RFC 3986 makes
/// `/blog//hello` a different URI from `/blog/hello`, and canonicalising with a
/// redirect keeps one cache entry per resource.
///
/// When #177 lands, core stops collapsing and every row here becomes `None`
/// on both sides.
#[test]
fn core_collapses_empty_path_segments_and_the_edge_does_not() {
    let patterns = &["/", "/blog", "/blog/{stem}", "/assets/{*relpath}"];

    // (path, what core matches today, what the edge matches today)
    let pinned: &[(&str, Option<&str>, Option<&str>)] = &[
        ("/blog/", Some("/blog"), None),
        ("/blog//hello", Some("/blog/{stem}"), None),
        ("/blog/hello/", Some("/blog/{stem}"), None),
        ("//", Some("/"), None),
    ];

    let routes = core_routes(patterns);
    let table = edge_table(patterns);
    for (path, want_core, want_edge) in pinned {
        assert_eq!(
            core_winner(&routes, path),
            *want_core,
            "core's answer for {path:?} changed"
        );
        assert_eq!(
            edge_winner(&table, path),
            *want_edge,
            "the edge's answer for {path:?} changed"
        );
    }
}

/// Both matchers refuse a parameter beside a catch-all in the same position.
///
/// `matchit` refuses to build the table, so a `site.toml` naming both fails at
/// startup. Core's load check refuses the same pair, because both patterns
/// match `/assets/one.css`. The wrong outcome: one side starts and the other
/// does not, so a pattern pair valid for a service is invalid at the edge.
#[test]
fn both_matchers_refuse_a_parameter_beside_a_catch_all() {
    let patterns = &["/assets/{name}", "/assets/{*rest}"];

    let decls: Vec<RouteDecl> = patterns
        .iter()
        .map(|p| RouteDecl::new("test", p, Methods::ANY))
        .collect();
    let refusal = check_route_conflicts(&decls).expect_err("core must refuse the pair");
    assert!(refusal.to_string().contains("GET /assets/x"), "{refusal}");

    // `for_bench` panics on the conflict, so the refusal is observed rather
    // than described. The hook is silenced because a deliberate panic should
    // not print a backtrace into a passing run.
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let built = std::panic::catch_unwind(|| edge_table(patterns));
    std::panic::set_hook(previous);
    assert!(
        built.is_err(),
        "the edge must refuse a parameter beside a catch-all in the same position"
    );
}
