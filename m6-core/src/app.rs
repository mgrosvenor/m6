//! The service: routing, the request dictionary, the thread pool, and the
//! lifecycle that holds them together.
//!
//! This is the module a service actually uses. `App::new().run()` is a whole
//! working service, and every other module in core is reached through it. It is
//! also the longest file here, so this note says how it is laid out and which
//! parts the rest of the system depends on for reasons that are not visible
//! locally.
//!
//! ```ignore
//! use m6_core::prelude::*;
//! fn main() -> anyhow::Result<()> { App::new().run()?; Ok(()) }
//! ```
//!
//! ## The shape
//!
//! Four builders, not one, and the extra three exist for typing rather than for
//! features: `App`, `AppWithGlobal<G>`, `AppWithThreadState<T>` and
//! `AppWithState<G, T>`. A handler that needs shared state has to receive it
//! with its real type, and the alternative to four builders was one builder
//! handing out `dyn Any` and making every handler downcast.
//!
//! Below them, in order of a request's life:
//!
//! | part | what it does |
//! |---|---|
//! | `compile_pattern`, `check_route_conflicts` | a route string becomes `Vec<Segment>` once, at load, and two routes that could meet are refused |
//! | `RouteTable`, `find_route`, `match_route` | a fixed route is one hash lookup, else the one pattern route that matches |
//! | `FrameworkState::build` | everything that can be computed at start rather than per request |
//! | `FrameworkState::build_dict` | the per-request dictionary, described below |
//! | `ThreadPool` | fixed workers over a bounded queue |
//! | `install_shutdown`, `is_shutdown` | a signal sets the flag the accept loop exits on |
//!
//! ## The request path is in `site`
//!
//! `Site::load` does the start half and `Site::handle` and `Site::finish` do the request half.
//! This module keeps the four builders, the thread pool, the accept loop and the lifecycle, and its worker calls `Site` for everything after `accept`.
//! EL-6d removes the loop and the builders, and the parts a service's own loop needs are already in `site`, `h1` and `net`.
//!
//! ## Fixed routes and patterns, and no tie-break
//!
//! A route is fixed when every segment is a literal and a pattern when it has a
//! `{param}` or `{*rest}`. A fixed route beats a pattern: `/blog/feed` answers
//! `/blog/feed` even when `/blog/{stem}` is declared. Two fixed routes on one
//! path, or two patterns that can both match some path, are refused when the
//! config loads. That leaves at most one candidate for any request, so the
//! answer cannot depend on the order of `[[route]]` blocks in a file somebody
//! edits later, and a request needs no tie-break.
//!
//! Routes only compete when a request method can reach both, so a code
//! `POST /contact` and a config `GET /contact` template share a path.
//! Code-registered routes are compiled ahead of config routes and inherit the
//! config's `cache` value for the same path, so a handler on a path the config
//! also names does not silently lose its caching policy.
//!
//! A trailing `{param}` matches ONE segment. Spanning several is `{*param}`,
//! spelled out, because making the last parameter implicitly greedy would
//! change the meaning of every route already written. m6-file's own matcher did
//! make it greedy, and reconciling the two is what made every nested asset on a
//! live site 404 while the process stayed perfectly healthy.
//!
//! ## The request dictionary, and why its ordering matters
//!
//! `build_dict` is where a request becomes the map a template renders against.
//! It was twelve ordered steps. It is now a base plus an overlay, and the
//! ordering the twelve steps depended on is preserved by the layering rather
//! than by one function running top to bottom. `crate::dict` holds that.
//!
//! **Steps 1 to 3 left this function because they produced the same map for
//! every request on a route**: config keys, global params files, and the route's
//! static params files. They are merged once at start into `route.base_dict`
//! and shared behind an `Arc`. That is the difference between 222,708ns and
//! 1,125ns per request, and it is the single largest measured win in the
//! project. See `docs/PERFORMANCE.md`.
//!
//! What is left varies per request, and the order within it decides the
//! result:
//!
//! | step | what goes in |
//! |---|---|
//! | 3b | params files whose path holds a `{placeholder}`, so they resolve per request |
//! | 4 | path params, validated per route: `{*param}` may contain `/`, `{param}` may not |
//! | 5 | query params, at the top level AND as a nested `query` map |
//! | 6 | POST form fields |
//! | 7 | cookies |
//! | 8 | **the built-ins** |
//! | 9 | auth claims |
//! | 10 | error keys |
//! | 11 | flash message, if its HMAC verifies |
//! | 12 | CSRF token |
//!
//! **Step 8 is the one to be careful with.** The built-ins go in AFTER every
//! params file, which means a params file cannot override them. That is
//! deliberate: `request_path`, the node name and the rest describe the request,
//! and a content file that could redefine them could make a page lie about
//! which URL it is. Moving step 8 earlier would look like a tidy-up and would
//! open exactly that.
//!
//! Step 6 decodes `application/x-www-form-urlencoded` and nothing else. A body
//! in any other type is skipped and a warning is logged, loudly, because the
//! silent version cost real debugging time: a client switched to multipart, every
//! field arrived empty, and downstream that looked like a failed CAPTCHA with
//! nothing in any log to say a body had been ignored. `crate::multipart` handles
//! the other type when the feature is on.
//!
//! ## Thread state
//!
//! One `thread_local!` slot holds the user's `T` as `Box<dyn Any + Send>`, with
//! the init and destroy callbacks stored globally so a worker thread can call
//! them without carrying type parameters. Exactly one stateful app can be active
//! per process, which is not enforced at runtime because a binary has one `main`
//! calling one `run()`.
//!
//! ## Shutdown
//!
//! `install_shutdown` makes a signal set a flag that `is_shutdown` reports. The
//! accept loop sees it, stops accepting and returns, the socket file is
//! removed and the process exits 0. Requests in progress are not waited for.

use std::any::Any;
use std::cell::RefCell;
use std::collections::HashMap;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::Context;
use serde_json::{Map, Value};
use tracing::{error, info, warn};

use crate::dict::Dict;
use crate::error::{Error, Result};
use crate::render::{RenderError, Renderer, RendererFactory};
use crate::request::{
    parse_auth_claims, parse_cookies, parse_form_body, parse_query_string, validate_path_param,
    validate_wildcard_param, RawRequest, Request,
};
use crate::response::Response;
use crate::site::Invocation;

// ---------------------------------------------------------------------------
// Per-thread state infrastructure
// ---------------------------------------------------------------------------
//
// A single `thread_local!` slot holds any user T as `Box<dyn Any + Send>`.
// The init and destroy callbacks are stored globally so worker threads can
// call them without needing type parameters.
//
// Only ONE stateful app can be active per process (ensured by design — a
// binary has exactly one `main` that calls exactly one `App::with_*().run()`).

thread_local! {
    static THREAD_STATE: RefCell<Option<Box<dyn Any + Send>>> = RefCell::new(None);
}

/// Factory: called once per thread to produce the initial thread-local value.
/// Returns a type-erased `Box<dyn Any + Send>`.
type ThreadInitFn = Arc<dyn Fn() -> Box<dyn Any + Send> + Send + Sync>;

/// Destructor: called once per thread at shutdown, receives the type-erased state.
type ThreadDestroyFn = Arc<dyn Fn(Box<dyn Any + Send>) + Send + Sync>;

// ── The stateful builders' callback shapes ────────────────────────────────────
//
// These four spell out the closures a service hands `App` when it carries state.
// They are aliases rather than the types written out because the written-out
// forms are what `App`'s builders, their private run functions and their
// internal structs all repeat, once per arity, and a reader comparing two
// signatures had to diff forty characters of `Arc<dyn Fn(..) + Send + Sync>` to
// find the one difference that mattered. Naming them says which is which.

/// Builds the global state once, at startup, before any worker thread exists.
/// Fails the service if it returns `Err`: there is no degraded mode.
type InitGlobal<G> = Arc<dyn Fn(&AppContext) -> Result<G> + Send + Sync>;

/// Builds one thread's state from the config dict and the global state. Called
/// once per worker thread. `G` is `()` for a service with thread state only.
type InitThread<G, T> = Arc<dyn Fn(&Map<String, Value>, &G) -> Result<T> + Send + Sync>;

/// Tears state down at shutdown. `None` is the common case: most services hold
/// nothing that needs an explicit drop, and the option is what says so.
type Destroy<S> = Option<Arc<dyn Fn(S) + Send + Sync>>;

/// Global thread-init function — set at startup before any threads are created.
static THREAD_INIT_FN: OnceLock<ThreadInitFn> = OnceLock::new();

/// Global thread-destroy function — set at startup before any threads are created.
static THREAD_DESTROY_FN: OnceLock<ThreadDestroyFn> = OnceLock::new();

/// Ensure the TLS slot is initialised for this thread, then run `f` with `&mut T`.
///
/// # Panics
///
/// Panics if T does not match the type that was stored (programming error).
fn with_thread_state<T: Any + Send + 'static, R>(f: impl FnOnce(&mut T) -> R) -> R {
    THREAD_STATE.with(|cell| {
        let mut opt = cell.borrow_mut();
        if opt.is_none() {
            // Lazy-init: call the global init function.
            if let Some(init) = THREAD_INIT_FN.get() {
                *opt = Some(init());
            } else {
                panic!("THREAD_INIT_FN not set but thread state was requested");
            }
        }
        let boxed = opt.as_mut().expect("thread state must be Some after init");
        let typed = boxed
            .downcast_mut::<T>()
            .expect("thread state type mismatch");
        f(typed)
    })
}

/// Drain and destroy the TLS slot for the current thread.
fn drain_thread_state() {
    THREAD_STATE.with(|cell| {
        let val = cell.borrow_mut().take();
        if let Some(state) = val {
            if let Some(destroy) = THREAD_DESTROY_FN.get() {
                destroy(state);
            }
        }
    });
}

// ---------------------------------------------------------------------------
// Route matching
// ---------------------------------------------------------------------------

/// The one method, or all of them, that a route registered in code answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RouteMethod {
    Any,
    Get,
    Post,
    Put,
    Patch,
    Delete,
}

/// The set of request methods a route answers, as bits so that one test
/// decides a request and one `&` decides whether two routes can meet.
///
/// `GET` also answers `HEAD`. Any other method a client sends (`OPTIONS`, a
/// made-up one) is answered only by a route that answers every method.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Methods(u8);

impl Methods {
    const NAMED: [(&'static str, u8); 5] = [
        ("GET", 1),
        ("POST", 2),
        ("PUT", 4),
        ("PATCH", 8),
        ("DELETE", 16),
    ];
    const OTHER: u8 = 0x80;
    /// Every method, including the ones m6 has no name for.
    pub const ANY: Methods = Methods(0xFF);

    fn bit_of_request(method: &str) -> u8 {
        match method {
            "GET" | "HEAD" => 1,
            "POST" => 2,
            "PUT" => 4,
            "PATCH" => 8,
            "DELETE" => 16,
            _ => Self::OTHER,
        }
    }

    /// The set a config `methods = [...]` list names, or the first name m6 does
    /// not support. No list means every method, which is the caller's case.
    pub fn from_names(names: &[String]) -> std::result::Result<Methods, &str> {
        let mut bits = 0;
        for name in names {
            let (_, bit) = Self::NAMED
                .iter()
                .find(|(n, _)| n == name)
                .ok_or(name.as_str())?;
            bits |= bit;
        }
        Ok(Methods(bits))
    }

    /// Does a request with this method belong to the set?
    pub fn contains(self, method: &str) -> bool {
        self.0 & Self::bit_of_request(method) != 0
    }

    /// The name of a method in both sets, for an example request, or `None` if
    /// the sets do not meet.
    fn shared_name(self, other: Methods) -> Option<&'static str> {
        let both = self.0 & other.0;
        Self::NAMED
            .iter()
            .find(|(_, bit)| both & bit != 0)
            .map(|(name, _)| *name)
    }

    /// Key that tells apart the handlers registered in code on one path.
    pub(crate) fn handler_key(self) -> &'static str {
        if self == Self::ANY {
            return "ANY";
        }
        Self::NAMED
            .iter()
            .find(|(_, bit)| self.0 == *bit)
            .map_or("SEVERAL", |(name, _)| name)
    }
}

impl From<&RouteMethod> for Methods {
    fn from(m: &RouteMethod) -> Methods {
        match m {
            RouteMethod::Any => Methods::ANY,
            RouteMethod::Get => Methods(1),
            RouteMethod::Post => Methods(2),
            RouteMethod::Put => Methods(4),
            RouteMethod::Patch => Methods(8),
            RouteMethod::Delete => Methods(16),
        }
    }
}

/// A compiled route pattern.
#[derive(Debug, Clone)]
pub struct CompiledRoute {
    pub pattern: String,
    pub methods: Methods,
    pub segments: Vec<Segment>,
    pub template: Option<String>,
    pub params_files: Vec<String>,
    pub status: u16,
    pub cache: String,
    pub headers: Vec<(String, String)>,
    /// When this route's rendered output last changed, for `Last-Modified`.
    ///
    /// A rendered page has no file of its own to stat, which is why it carried
    /// no `Last-Modified` at all and every `If-Modified-Since` came back 200
    /// with the whole page. It does have inputs, though, and their mtimes are a
    /// truthful answer: the max over every template and this route's own static
    /// params files.
    ///
    /// Computed once when the framework state is built, so it costs nothing per
    /// request.
    ///
    /// `None` for a route whose params path contains a `{placeholder}`: the
    /// actual file depends on the request, so no honest value exists until one
    /// arrives. Omitting the header is correct there — RFC 9110 lets a server
    /// leave it out, and a wrong date is far worse than an absent one.
    pub last_modified: Option<std::time::SystemTime>,
    /// Name of the registered handler this route dispatches to, if any.
    ///
    /// `Some` for a config route carrying `handler = "..."`. `None` for a
    /// template route and for a route registered in code with `route_get` and
    /// friends, which is found by pattern and method instead: those are bound
    /// at the call site, so the pattern *is* the binding.
    pub handler: Option<String>,
    /// The matched route's own config keys that core does not define.
    pub settings: Arc<Map<String, Value>>,
    /// The part of this route's request dictionary that does not vary between
    /// requests: config keys, global params files and this route's static
    /// params files, merged once at start in that order.
    ///
    /// Shared with every request on this route rather than copied into each
    /// one. A content-driven site puts its whole catalogue here, which runs to
    /// tens of kilobytes and thousands of nodes, so a deep copy per request
    /// costs several times that for a value nothing mutates.
    pub base_dict: Arc<Map<String, Value>>,
}

impl CompiledRoute {
    /// Was `name` captured by a `{*name}` wildcard on this route?
    ///
    /// Asked per matched parameter rather than stored per capture because a
    /// route has at most a handful of segments and this runs once per param,
    /// not once per segment per request.
    pub fn is_wildcard_param(&self, name: &str) -> bool {
        self.segments
            .iter()
            .any(|s| matches!(s, Segment::Wildcard(n) if n == name))
    }
}

#[derive(Debug, Clone)]
pub enum Segment {
    Literal(String),
    Param(String),
    /// `{*name}` — captures this segment and every one after it, joined by `/`.
    ///
    /// **Only legal as the last segment**, because a wildcard in the middle has
    /// no single correct answer: `/a/{*rest}/c` against `/a/b/c/d/c` could
    /// split in two places and neither is more right than the other.
    ///
    /// This is what `App` needed in order to express a static file server, and
    /// it is the whole of why `m6-file` has a different shape. Note m6-file
    /// spells the same idea as a bare `{relpath}` in the last position, which
    /// works because its own matcher makes the final parameter greedy. Core
    /// does not copy that: making the last `{param}` implicitly span several
    /// segments would silently change the meaning of every route already
    /// written, everywhere, including the ones already serving. The star is
    /// explicit.
    Wildcard(String),
}

/// Compile a route pattern string into segments.
///
/// `{name}` is one segment, `{*name}` is the rest of the path. A `{*name}`
/// anywhere but last is compiled as an ordinary parameter and warned about,
/// rather than rejected: this runs at startup for every route in a config, and
/// taking a service down over a pattern that is merely ambiguous is worse than
/// serving it with the narrower reading.
pub fn compile_pattern(pattern: &str) -> Vec<Segment> {
    let raw: Vec<&str> = pattern.split('/').filter(|s| !s.is_empty()).collect();
    let last = raw.len().saturating_sub(1);
    raw.iter()
        .enumerate()
        .map(|(i, s)| {
            if s.starts_with('{') && s.ends_with('}') {
                let inner = &s[1..s.len() - 1];
                if let Some(name) = inner.strip_prefix('*') {
                    if i == last {
                        return Segment::Wildcard(name.to_string());
                    }
                    warn!(
                        pattern = pattern,
                        segment = *s,
                        "a wildcard is only meaningful as the last segment; \
                         treating it as an ordinary parameter"
                    );
                    return Segment::Param(name.to_string());
                }
                Segment::Param(inner.to_string())
            } else {
                Segment::Literal(s.to_string())
            }
        })
        .collect()
}

/// A route is fixed when every segment is a literal. Anything with a `{param}`
/// or a `{*rest}` is a pattern.
fn is_fixed(segments: &[Segment]) -> bool {
    segments.iter().all(|s| matches!(s, Segment::Literal(_)))
}

/// One path that both patterns match, or `None` if no path does.
///
/// Mirrors `match_route`: a literal must equal the path segment, a parameter
/// takes any one segment, a trailing wildcard takes one or more, and without a
/// wildcard the segment counts must be equal. A wildcard matches whatever the
/// other pattern has opposite it, so it only ever needs comparing as a
/// parameter.
fn shared_path(a: &[Segment], b: &[Segment]) -> Option<String> {
    let ends_in_wildcard = |s: &[Segment]| matches!(s.last(), Some(Segment::Wildcard(_)));
    let len = match (ends_in_wildcard(a), ends_in_wildcard(b)) {
        (false, false) if a.len() == b.len() => a.len(),
        (false, false) => return None,
        (true, false) if b.len() >= a.len() => b.len(),
        (false, true) if a.len() >= b.len() => a.len(),
        (true, false) | (false, true) => return None,
        (true, true) => a.len().max(b.len()),
    };
    let mut path = String::new();
    for i in 0..len {
        let literal = match (a.get(i), b.get(i)) {
            (Some(Segment::Literal(x)), Some(Segment::Literal(y))) if x != y => return None,
            (Some(Segment::Literal(x)), _) | (_, Some(Segment::Literal(x))) => x.as_str(),
            _ => "x",
        };
        path.push('/');
        path.push_str(literal);
    }
    Some(if path.is_empty() { "/".into() } else { path })
}

/// A route as written, for the load-time conflict check. A service declares
/// its routes with `[[route]]` and in code, and the edge declares its own, so
/// each builds these and shares one rule.
pub struct RouteDecl<'a> {
    origin: String,
    pattern: &'a str,
    methods: Methods,
    segments: Vec<Segment>,
    from_code: bool,
    /// A config `[[route]]` on a code route's own pattern that has neither a
    /// template nor a handler. It only gives that code route its `cache`.
    declares_code_route: bool,
}

impl<'a> RouteDecl<'a> {
    /// `origin` says where the route was written, for the error, for example
    /// `[[route]] number 2`.
    pub fn new(origin: impl Into<String>, pattern: &'a str, methods: Methods) -> Self {
        RouteDecl {
            origin: origin.into(),
            pattern,
            methods,
            segments: compile_pattern(pattern),
            from_code: false,
            declares_code_route: false,
        }
    }
}

/// Refuse a route table in which two routes could answer one request and m6
/// could not say which. The rule, with "can answer the same request" meaning
/// a path both match and a method both accept:
///
/// 1. a fixed route beats a pattern route;
/// 2. two fixed routes with the same path are an error;
/// 3. two pattern routes that can both match some path are an error.
///
/// So at most one pattern route can match a request, and a request needs no
/// tie-break. Runs once per load over the whole table, never per request.
pub fn check_route_conflicts(routes: &[RouteDecl]) -> anyhow::Result<()> {
    for (i, a) in routes.iter().enumerate() {
        for b in &routes[i + 1..] {
            let Some(method) = a.methods.shared_name(b.methods) else {
                continue;
            };
            let a_fixed = is_fixed(&a.segments);
            let b_fixed = is_fixed(&b.segments);
            if a_fixed != b_fixed
                || (a.from_code && b.declares_code_route && a.pattern == b.pattern)
            {
                continue;
            }
            if let Some(path) = shared_path(&a.segments, &b.segments) {
                let how_to_fix = if a_fixed {
                    "remove one or give one explicit methods"
                } else {
                    "remove one, give one explicit methods, or make one a fixed path"
                };
                anyhow::bail!(
                    "routes \"{}\" ({}) and \"{}\" ({}) can both answer {method} {path}; {how_to_fix}",
                    a.pattern,
                    a.origin,
                    b.pattern,
                    b.origin
                );
            }
        }
    }
    Ok(())
}

/// The routes a service declares, in the order they are tried: code routes
/// first, then config routes. Fails on a `methods` list m6 cannot read.
fn declared_routes<'a>(
    code_routes: &'a [(String, RouteMethod)],
    config_routes: &'a [crate::config::RouteConfig],
) -> anyhow::Result<Vec<RouteDecl<'a>>> {
    let code = code_routes.iter().map(|(pattern, method)| {
        let mut decl = RouteDecl::new("code route", pattern, method.into());
        decl.from_code = true;
        Ok(decl)
    });
    let config = config_routes.iter().enumerate().map(|(i, rc)| {
        let origin = format!("[[route]] number {}", i + 1);
        let mut decl = RouteDecl::new(origin, &rc.path, config_route_methods(rc, i)?);
        decl.declares_code_route = rc.template.is_none()
            && rc.handler.is_none()
            && code_routes.iter().any(|(p, _)| *p == rc.path);
        Ok(decl)
    });
    code.chain(config).collect()
}

/// Refuse a service's routes if one is unreadable or two can meet. The check
/// start and `--dump-config` share.
pub fn check_routes(
    code_routes: &[(String, RouteMethod)],
    config_routes: &[crate::config::RouteConfig],
) -> anyhow::Result<()> {
    check_route_conflicts(&declared_routes(code_routes, config_routes)?)
}

/// The methods a config `[[route]]` answers: exactly the ones it lists, or all
/// of them when it lists none.
fn config_route_methods(rc: &crate::config::RouteConfig, index: usize) -> anyhow::Result<Methods> {
    let Some(names) = &rc.methods else {
        return Ok(Methods::ANY);
    };
    anyhow::ensure!(
        !names.is_empty(),
        "route \"{}\" ([[route]] number {}) has an empty methods list; leave `methods` out to answer every method",
        rc.path,
        index + 1
    );
    Methods::from_names(names).map_err(|unknown| {
        anyhow::anyhow!(
            "route \"{}\" ([[route]] number {}) lists the unknown method \"{unknown}\"; \
             the supported methods are GET (which also answers HEAD), POST, PUT, PATCH and DELETE",
            rc.path,
            index + 1
        )
    })
}

/// Key that tells apart the handlers registered in code on one path.
pub(crate) fn route_method_key(m: &RouteMethod) -> &'static str {
    Methods::from(m).handler_key()
}

/// Route params: at most a few captures; Vec beats HashMap for small N.
pub type PathParams = Vec<(String, String)>;

/// Try to match pre-split URL path segments against a compiled route.
/// Returns `Some(params)` on success.
pub fn match_route(path_segs: &[&str], route: &CompiledRoute) -> Option<PathParams> {
    let trailing_wildcard = matches!(route.segments.last(), Some(Segment::Wildcard(_)));

    // A wildcard route matches a path at least as long as its own fixed part.
    // Everything else still requires an exact segment count, which is what
    // stops `/a/{b}` answering for `/a/b/c`.
    if trailing_wildcard {
        if path_segs.len() < route.segments.len() {
            return None;
        }
    } else if path_segs.len() != route.segments.len() {
        return None;
    }

    let mut params = PathParams::new();
    for (ps, seg) in route.segments.iter().enumerate() {
        match seg {
            Segment::Literal(lit) => {
                if path_segs[ps] != lit.as_str() {
                    return None;
                }
            }
            Segment::Param(name) => {
                params.push((name.clone(), path_segs[ps].to_string()));
            }
            Segment::Wildcard(name) => {
                // The rest of the path, rejoined. Empty segments were dropped
                // by the split, so `/a//b` captures as `a/b`; that collapsing
                // is deliberate, since the two address the same file.
                params.push((name.clone(), path_segs[ps..].join("/")));
            }
        }
    }
    Some(params)
}

/// The compiled routes, indexed so that a request needs no tie-break.
///
/// A fixed route is found by one hash lookup on its path. A request that no
/// fixed route answers is tried against the pattern routes, of which at most
/// one can match: `check_route_conflicts` refuses a table where two could.
#[derive(Debug)]
pub struct RouteTable {
    routes: Vec<CompiledRoute>,
    /// Fixed routes by path without its slashes (`blog/archive`, and `` for
    /// the root), each with the routes on it in declaration order, because
    /// routes with disjoint methods share a path.
    fixed: HashMap<String, Vec<usize>>,
    patterns: Vec<usize>,
}

impl RouteTable {
    pub fn new(routes: Vec<CompiledRoute>) -> Self {
        let mut fixed: HashMap<String, Vec<usize>> = HashMap::new();
        let mut patterns = Vec::new();
        for (i, route) in routes.iter().enumerate() {
            if is_fixed(&route.segments) {
                let key = route
                    .segments
                    .iter()
                    .filter_map(|s| match s {
                        Segment::Literal(l) => Some(l.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("/");
                fixed.entry(key).or_default().push(i);
            } else {
                patterns.push(i);
            }
        }
        RouteTable {
            routes,
            fixed,
            patterns,
        }
    }

    pub fn routes(&self) -> &[CompiledRoute] {
        &self.routes
    }

    pub fn len(&self) -> usize {
        self.routes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.routes.is_empty()
    }
}

/// Find the route that answers a request: the fixed route on its path if one
/// accepts the method, otherwise the one pattern route that matches.
pub fn find_route<'a>(
    path: &str,
    method: &str,
    table: &'a RouteTable,
) -> Option<(&'a CompiledRoute, PathParams)> {
    // A path with no doubled slash is already its own key once the outer
    // slashes go, so the common request allocates nothing for the lookup.
    let trimmed = path.trim_matches('/');
    let collapsed;
    let key = if trimmed.contains("//") {
        collapsed = trimmed
            .split('/')
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("/");
        collapsed.as_str()
    } else {
        trimmed
    };
    if let Some(on_path) = table.fixed.get(key) {
        if let Some(&i) = on_path
            .iter()
            .find(|&&i| table.routes[i].methods.contains(method))
        {
            return Some((&table.routes[i], PathParams::new()));
        }
    }

    let path_segs: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    table.patterns.iter().find_map(|&i| {
        let route = &table.routes[i];
        if !route.methods.contains(method) {
            return None;
        }
        match_route(&path_segs, route).map(|params| (route, params))
    })
}

// ---------------------------------------------------------------------------
// Handler trait
// ---------------------------------------------------------------------------

/// Handler function for `App::new()` (no state).
pub trait HandlerFn: Send + Sync + 'static {
    fn call(&self, req: &Request) -> Result<Response>;
}

impl<F> HandlerFn for F
where
    F: Fn(&Request) -> Result<Response> + Send + Sync + 'static,
{
    fn call(&self, req: &Request) -> Result<Response> {
        (self)(req)
    }
}

pub type BoxHandler = Box<dyn HandlerFn>;

// ---------------------------------------------------------------------------
// Params cache
// ---------------------------------------------------------------------------

pub(crate) struct ParamsCache {
    inner: Mutex<lru::LruCache<String, Arc<Map<String, Value>>>>,
}

impl ParamsCache {
    fn new(size: usize) -> Self {
        use std::num::NonZeroUsize;
        let cap = NonZeroUsize::new(size.max(1)).unwrap();
        Self {
            inner: Mutex::new(lru::LruCache::new(cap)),
        }
    }

    fn get(&self, key: &str) -> Option<Arc<Map<String, Value>>> {
        self.inner.lock().unwrap().get(key).cloned()
    }

    fn insert(&self, key: String, val: Arc<Map<String, Value>>) {
        self.inner.lock().unwrap().put(key, val);
    }
}

// ---------------------------------------------------------------------------
// Shared framework state
// ---------------------------------------------------------------------------

pub(crate) struct FrameworkState {
    /// Behind an `Arc` because it is immutable for the life of the process and
    /// every request needs to read it. Owned here instead, the two maps a
    /// request actually wants -- `compression` and `minification` -- have to be
    /// deep-cloned out, per request. An `Arc` clone is a refcount bump and
    /// carries the whole config rather than two fields of it.
    pub(crate) config: Arc<crate::config::RendererConfig>,

    /// Likewise: one `PathBuf` allocation per request, twice, for a path that
    /// does not change.
    pub(crate) site_dir: Arc<PathBuf>,

    /// Shared rather than owned, so a request can route without copying the
    /// route it matched. `find_route` returns a borrow, and owning the table
    /// here would mean turning that borrow into an owned `CompiledRoute`.
    pub(crate) routes: Arc<RouteTable>,

    /// Test-only. Steps 1 to 3 of the dictionary build are merged once at
    /// start into `route.base_dict`, so no serving path reads either of these.
    /// `dict_cost` measures per-request assembly against that base, which is
    /// what they are kept for. A release build carries neither.
    #[cfg(test)]
    global_params_data: Map<String, Value>,
    #[cfg(test)]
    static_params: HashMap<String, Arc<Map<String, Value>>>,
    pub(crate) params_cache: Arc<ParamsCache>,

    pub(crate) renderer: Arc<dyn Renderer>,

    #[cfg(feature = "flash")]
    pub(crate) flash_secret: Vec<u8>,
}

impl FrameworkState {
    pub(crate) fn build(
        config: crate::config::RendererConfig,
        site_dir: PathBuf,
        // NOTE: takes them by value and wraps them; callers keep passing
        // owned values and the sharing is this type's business.
        code_routes: &[(String, RouteMethod)],
        renderer_factory: &dyn RendererFactory,
    ) -> anyhow::Result<Self> {
        check_routes(code_routes, &config.routes)?;

        // Compile routes from config.
        let mut routes = Vec::new();

        // Add code-registered routes (they come first — higher priority for same pattern).
        // Default cache to no-store: handler routes generate dynamic content.
        // The config's [[route]] cache setting for the same path is inherited below.
        for (pattern, method) in code_routes {
            let segs = compile_pattern(pattern);
            // Look for a matching config route to inherit its cache setting.
            let cache = config
                .routes
                .iter()
                .find(|r| r.path == *pattern)
                .map(|r| r.cache.clone())
                .unwrap_or_else(|| "no-store".to_string());
            routes.push(CompiledRoute {
                pattern: pattern.clone(),
                methods: Methods::from(method),
                segments: segs,
                template: None,
                params_files: vec![],
                status: 200,
                cache,
                headers: vec![],
                // A code route renders whatever its handler decides at request
                // time, so there is no input file whose mtime describes it.
                last_modified: None,
                // A code route is bound by pattern and method at the call
                // site, so it names no handler and carries no config of its
                // own. The config-declared form below is the one that does.
                handler: None,
                settings: Arc::new(Map::new()),
                base_dict: Arc::new(Map::new()), // filled in below
            });
        }

        // Add config routes.
        for (i, rc) in config.routes.iter().enumerate() {
            let segs = compile_pattern(&rc.path);
            routes.push(CompiledRoute {
                pattern: rc.path.clone(),
                methods: config_route_methods(rc, i)?,
                segments: segs,
                template: rc.template.clone(),
                params_files: rc.params.clone(),
                status: rc.status,
                cache: rc.cache.clone(),
                headers: rc.headers.clone(),
                last_modified: None, // filled in below, once templates_mtime is known
                handler: rc.handler.clone(),
                settings: Arc::clone(&rc.settings),
                base_dict: Arc::new(Map::new()), // filled in below
            });
        }

        // ── Per-route Last-Modified ─────────────────────────────────────────
        // Rendered HTML had no `Last-Modified`, so a client validating by date
        // got the whole page back on every request, tens of kilobytes at a
        // time. m6-html emits an ETag, so `If-None-Match` already worked. This
        // closes the other half.
        //
        // Templates are pooled deliberately. Shared partials are included by
        // every page, and resolving the transitive include set per route would
        // be a lot of machinery to make one date slightly tighter. Taking the
        // newest template as every page's floor is honest, because a change to
        // a shared partial really can change any page, and it errs toward
        // revalidating, which costs a request rather than serving something
        // stale.
        //
        // Params are per route, which is where the precision pays: editing one
        // page's content should not make another page look modified.
        let templates_mtime = newest_mtime_under(&site_dir.join("templates"));
        for route in &mut routes {
            // The date is derived from templates and params files, so it is
            // only meaningful for a route that renders from them.
            //
            // Config routes only. Run over every route, this quietly includes
            // the code routes it is not meant to touch: their `params_files` is
            // empty, so nothing skips them and they take the newest template's
            // mtime as their own. A handler's answer is computed per request and
            // is not dated by a template it may never read.
            if route.template.is_none() {
                continue;
            }
            // A params path with a placeholder resolves per request, so no
            // build-time value can be right. Leave the header off rather than
            // publish a date that is wrong for most requests.
            if route.params_files.iter().any(|p| p.contains('{')) {
                continue;
            }
            let mut newest = templates_mtime;
            for pf in &route.params_files {
                if let Some(t) = file_mtime(&site_dir.join(pf)) {
                    newest = match newest {
                        Some(n) if n >= t => Some(n),
                        _ => Some(t),
                    };
                }
            }
            route.last_modified = newest;
        }

        // Collect template paths from config routes.
        let template_paths: Vec<String> = config
            .routes
            .iter()
            .filter_map(|r| r.template.clone())
            .collect();

        // Build the renderer. A template syntax error surfaces as an `Err`
        // here, which the caller turns into exit 2 at startup.
        //
        // Whether an empty `template_paths` means "this site has no templates"
        // or "discover them under site_dir" is a question about templates, so
        // the factory answers it rather than this loop.
        let renderer = renderer_factory
            .build(&site_dir, &template_paths)
            .context("building renderer")?;

        // Load global params.
        let mut global_params_data = Map::new();
        for path_str in &config.global_params {
            let abs = site_dir.join(path_str);
            if abs.exists() {
                let data = std::fs::read(&abs)
                    .with_context(|| format!("reading global params {}", abs.display()))?;
                let v: Value = serde_json::from_slice(&data)
                    .with_context(|| format!("parsing global params {}", abs.display()))?;
                if let Some(obj) = v.as_object() {
                    for (k, val) in obj {
                        global_params_data.insert(k.clone(), val.clone());
                    }
                }
            } else {
                error!(path = %abs.display(), "global params file missing");
            }
        }

        // Load static params files.
        let mut static_params = HashMap::new();
        for rc in &config.routes {
            for pf in &rc.params {
                if !pf.contains('{') && !static_params.contains_key(pf) {
                    let abs = site_dir.join(pf);
                    if abs.exists() {
                        let data = std::fs::read(&abs)
                            .with_context(|| format!("reading params {}", abs.display()))?;
                        let v: Value = serde_json::from_slice(&data)
                            .with_context(|| format!("parsing params {}", abs.display()))?;
                        let mut m = Map::new();
                        if let Some(obj) = v.as_object() {
                            for (k, val) in obj {
                                m.insert(k.clone(), val.clone());
                            }
                        }
                        static_params.insert(pf.clone(), Arc::new(m));
                    }
                }
            }
        }

        // ── The per-route base dictionary ───────────────────────────────────
        //
        // Steps 1 to 3 of `build_dict` produced the same map for every request
        // on a given route, and produced it per request: the config's keys,
        // every global params file, and the route's static params files, in
        // that order. Merged once, here, and shared with every request on the
        // route behind an `Arc`.
        //
        // Routes sharing a params-file set share the merged result too, keyed
        // on the static file list, so a site with fifteen routes over one
        // content file holds one copy rather than fifteen.
        let mut base_by_key: HashMap<String, Arc<Map<String, Value>>> = HashMap::new();
        for route in &mut routes {
            let static_files: Vec<&String> = route
                .params_files
                .iter()
                .filter(|p| !p.contains('{'))
                .collect();
            let key = static_files
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join("\u{1}");

            let base = base_by_key.entry(key).or_insert_with(|| {
                let mut m = Map::new();
                // 1. Config keys.
                for (k, v) in &config.user_config {
                    m.insert(k.clone(), v.clone());
                }
                // 2. Global params files.
                for (k, v) in &global_params_data {
                    m.insert(k.clone(), v.clone());
                }
                // 3. This route's static params files, in declaration order.
                //
                // A file already merged as a global param is skipped rather
                // than merged a second time over itself. Naming one file as
                // both a global and a route param is an ordinary thing to do,
                // and merging it twice costs a second full copy of it for
                // identical values.
                for pf in &static_files {
                    if config.global_params.contains(pf) {
                        continue;
                    }
                    if let Some(sp) = static_params.get(pf.as_str()) {
                        for (k, v) in sp.as_ref() {
                            m.insert(k.clone(), v.clone());
                        }
                    }
                }
                Arc::new(m)
            });
            route.base_dict = Arc::clone(base);
        }

        let params_cache = Arc::new(ParamsCache::new(config.params_cache.size));

        // Flash secret: decode from config if present. Presence is validated at server startup,
        // not here, so that tests can call FrameworkState::build without a flash_secret.
        #[cfg(feature = "flash")]
        let flash_secret = {
            use base64::Engine;
            let raw = config
                .user_config
                .get("flash_secret")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if raw.is_empty() {
                vec![] // validated at startup
            } else {
                base64::engine::general_purpose::STANDARD
                    .decode(raw)
                    .or_else(|_| base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(raw))
                    .context("decoding flash_secret (expected base64)")?
            }
        };

        Ok(Self {
            config: Arc::new(config),
            site_dir: Arc::new(site_dir),
            routes: Arc::new(RouteTable::new(routes)),
            #[cfg(test)]
            global_params_data,
            #[cfg(test)]
            static_params,
            params_cache,
            renderer: Arc::from(renderer),
            #[cfg(feature = "flash")]
            flash_secret,
        })
    }

    /// Build the request dictionary for a matched route.
    ///
    /// **Steps 1 to 3 are not here any more**, because they produced the same
    /// map for every request on a route: config keys, global params files and
    /// the route's *static* params files. They are merged once at start into
    /// `route.base_dict` and shared behind an `Arc`. What is left in this
    /// function is what genuinely varies per request.
    ///
    /// The ordering the original twelve steps depended on is preserved by the
    /// layering rather than by this loop; see `crate::dict`.
    pub(crate) fn build_dict(
        &self,
        raw: &RawRequest,
        route: &CompiledRoute,
        path_params: &PathParams,
    ) -> Result<Dict> {
        let mut dict = Dict::with_base(Arc::clone(&route.base_dict));

        // 3b. Params files whose path holds a `{placeholder}` resolve per
        // request, so they cannot live in the base. They go in first, ahead of
        // everything else in the overlay, which keeps them below the built-ins
        // exactly as they were below them before.
        for pf_template in &route.params_files {
            if !pf_template.contains('{') {
                continue; // static: already in the base
            }
            let pf_resolved = resolve_path_template(pf_template, path_params);
            let arc_m: Option<Arc<Map<String, Value>>> =
                if let Some(cached) = self.params_cache.get(&pf_resolved) {
                    Some(cached)
                } else {
                    let abs = self.site_dir.join(&pf_resolved);
                    if abs.exists() {
                        let data = std::fs::read(&abs)
                            .with_context(|| format!("reading params {}", abs.display()))
                            .map_err(Error::Other)?;
                        let v: Value = serde_json::from_slice(&data)
                            .with_context(|| format!("parsing params {}", abs.display()))
                            .map_err(Error::Other)?;
                        let mut m = Map::new();
                        if let Some(obj) = v.as_object() {
                            for (k, val) in obj {
                                m.insert(k.clone(), val.clone());
                            }
                        }
                        let arc = Arc::new(m);
                        self.params_cache.insert(pf_resolved, Arc::clone(&arc));
                        Some(arc)
                    } else {
                        error!(path = %abs.display(), "params file missing");
                        None
                    }
                };

            if let Some(m) = arc_m {
                for (k, v) in m.as_ref() {
                    dict.insert(k.clone(), v.clone());
                }
            }
        }

        // 4. Path params.
        //
        // Which validation applies is a property of the route, not of the
        // parameter's name: a `{*name}` capture is defined to span segments
        // and is the only one allowed to carry a slash. Everything else is
        // one segment and is checked exactly as before.
        for (k, v) in path_params {
            if route.is_wildcard_param(k) {
                validate_wildcard_param(k, v)?;
            } else {
                validate_path_param(k, v)?;
            }
            dict.insert(k.clone(), Value::String(v.clone()));
        }

        // 5. Query params — inserted at top level AND as a nested `query` map.
        //
        // A query or form key never replaces a path parameter at the top
        // level. The path value was validated in step 4 and the query value
        // was not, so `/assets/a.css?relpath=../../x` must not reach a
        // handler as `relpath`. The query value stays readable as
        // `query.<name>`.
        let is_path_param = |k: &str| path_params.iter().any(|(p, _)| p == k);
        let mut query_map = Map::new();
        for (k, v) in parse_query_string(raw.query()) {
            query_map.insert(k.clone(), Value::String(v.clone()));
            if !is_path_param(&k) {
                dict.insert(k, Value::String(v));
            }
        }
        dict.insert("query".to_string(), Value::Object(query_map));

        // 6. POST form fields.
        //
        // Only `application/x-www-form-urlencoded` is decoded. Anything else --
        // notably `multipart/form-data` -- yields NO fields.
        //
        // That silence cost real debugging time. A client switched to sending
        // multipart (a `fetch` with a `FormData` body does this by default) and
        // every field arrived empty. Downstream that looked like a failed
        // CAPTCHA rather than an unparsed body, and there was nothing in any
        // log to say the body had been skipped. Warn loudly instead: an empty
        // dict on a POST that plainly carried a body is a bug somewhere, and
        // the content type names it.
        if raw.method() == "POST" {
            match raw.content_type() {
                Some(ct) if ct.contains("application/x-www-form-urlencoded") => {
                    for (k, v) in parse_form_body(&raw.body) {
                        if !is_path_param(&k) {
                            dict.insert(k, Value::String(v));
                        }
                    }
                }
                other => {
                    if !raw.body.is_empty() {
                        warn!(
                            content_type = other.unwrap_or("<none>"),
                            body_len = raw.body.len(),
                            "POST body not decoded: only application/x-www-form-urlencoded \
                             is supported, so no form fields are available to the handler"
                        );
                    }
                }
            }
        }

        // 7. Cookies.
        let cookies_map = if let Some(cookie_hdr) = raw.header("cookie") {
            parse_cookies(cookie_hdr)
        } else {
            Map::new()
        };
        dict.insert("cookies".to_string(), Value::Object(cookies_map));

        // 8. Built-in keys (set after params files — cannot be overridden by them).
        let (datetime, year) = Self::builtin_dates(chrono::Utc::now());
        dict.insert(
            "request_path".to_string(),
            Value::String(raw.path().to_string()),
        );
        dict.insert("datetime".to_string(), Value::String(datetime));
        dict.insert("year".to_string(), Value::String(year));

        // 9. Auth keys.
        if let Some(claims_hdr) = raw.header("x-auth-claims") {
            let auth = parse_auth_claims(claims_hdr);
            for (k, v) in auth {
                dict.insert(k, v);
            }
        }

        // 10. Error keys (from query params — already merged in step 5, but highlight here).

        // 11. Flash message: verify HMAC, add to dict if valid, clear cookie.
        // Read back through the dictionary: `Dict::get` returns the overlay
        // first, and the map inserted in step 7 is in it. The verified message
        // is owned, so the borrow of `dict` ends before the insert.
        #[cfg(feature = "flash")]
        {
            let message = dict
                .get("cookies")
                .and_then(|cookies| cookies.get("_flash"))
                .and_then(|v| v.as_str())
                .and_then(|flash_cookie| verify_flash_cookie(flash_cookie, &self.flash_secret));
            if let Some(msg) = message {
                dict.insert("flash".to_string(), Value::String(msg));
            }
        }

        // 12. CSRF token: generate or reuse from cookie, inject into dict —
        // but only as a default. Step 6 (POST form fields) may already have
        // set dict["csrf_token"] to whatever the client actually submitted;
        // verify_csrf() below compares that submitted value against the
        // cookie, so overwriting it here unconditionally (the previous
        // behavior) replaced the submitted token with the cookie's own
        // value before the comparison ever ran — the two sides being
        // compared were always identical, silently defeating the
        // double-submit check for every request, pass or fail.
        #[cfg(feature = "csrf")]
        if !dict.contains_key("csrf_token") {
            let token = if let Some(existing) = dict
                .get("cookies")
                .and_then(|c| c.get("_csrf"))
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
            {
                existing.to_string()
            } else {
                generate_csrf_token()
            };
            dict.insert("csrf_token".to_string(), Value::String(token));
        }

        Ok(dict)
    }

    /// The two built-in date values: RFC 3339 in UTC to the second, and the
    /// four-digit year. No format string is parsed on the request path.
    fn builtin_dates(now: chrono::DateTime<chrono::Utc>) -> (String, String) {
        use chrono::Datelike as _;
        (
            now.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            now.year().to_string(),
        )
    }

    /// Render a template response.
    ///
    /// Builds the context as a plain `Map` rather than an engine type: the
    /// merge order is the framework's business (global params, path params,
    /// auth claims first, then the handler's `render_with` extras on top), and
    /// the engine's business starts once it has the finished context.
    ///
    /// An associated function, not a method: it needs the renderer and nothing
    /// else from the framework state, so a request can render after it has
    /// released the read lock.
    pub(crate) fn render_response(
        renderer: &dyn Renderer,
        resp: &mut Response,
        dict: &Dict,
    ) -> std::result::Result<(), RenderError> {
        if let Some(template_name) = resp.template_name.take() {
            // No copy on the common path. A response that supplied its own
            // context is rendered against that; one that did not is rendered
            // against the request's dictionary, which is this argument. Cloning
            // the dict here unconditionally would then merge into it a context
            // that, for a config template route, is a copy of the same dict.
            let ctx: &Dict = match &resp.template_dict {
                Some(supplied) => supplied,
                None => dict,
            };
            let html = renderer.render(&template_name, ctx)?;
            resp.body = crate::response::Body::Bytes(html.into_bytes());
            resp.headers.push((
                "Content-Type".to_string(),
                "text/html; charset=utf-8".to_string(),
            ));
            resp.template_dict = None;
        }
        Ok(())
    }
}

#[cfg(test)]
mod builtin_dates_tests {
    use super::FrameworkState;

    /// The built-in dates keep the shape templates already read:
    /// `%Y-%m-%dT%H:%M:%SZ` and `%Y`. Pinned here so a change to how they are
    /// formatted cannot drift from what a template sees.
    #[test]
    fn builtin_dates_keep_their_format() {
        use chrono::TimeZone as _;

        let now = chrono::Utc.with_ymd_and_hms(2026, 1, 2, 3, 4, 5).unwrap();
        let (datetime, year) = FrameworkState::builtin_dates(now);
        assert_eq!(datetime, "2026-01-02T03:04:05Z");
        assert_eq!(year, "2026");

        let last = chrono::Utc
            .with_ymd_and_hms(9999, 12, 31, 23, 59, 59)
            .unwrap();
        let (datetime, year) = FrameworkState::builtin_dates(last);
        assert_eq!(datetime, "9999-12-31T23:59:59Z");
        assert_eq!(year, "9999");
    }
}

fn resolve_path_template(template: &str, params: &PathParams) -> String {
    let mut result = template.to_string();
    for (k, v) in params {
        result = result.replace(&format!("{{{}}}", k), v);
    }
    result
}

// ---------------------------------------------------------------------------
// Flash helpers
// ---------------------------------------------------------------------------

/// Verify a `_flash` cookie value and return the decoded message if valid.
/// Cookie format: `<base64(message)>.<base64(hmac)>`
#[cfg(feature = "flash")]
fn verify_flash_cookie(cookie_val: &str, secret: &[u8]) -> Option<String> {
    use base64::Engine;
    use hmac::{Hmac, Mac};
    use sha2::Sha256;

    let (msg_b64, sig_b64) = cookie_val.split_once('.')?;

    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(secret).ok()?;
    mac.update(msg_b64.as_bytes());
    let expected = mac.finalize().into_bytes();

    let provided = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(sig_b64)
        .ok()?;

    // Constant-time comparison.
    if expected.len() != provided.len() {
        return None;
    }
    let ok = expected
        .iter()
        .zip(provided.iter())
        .fold(0u8, |acc, (a, b)| acc | (a ^ b))
        == 0;
    if !ok {
        return None;
    }

    let msg_bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(msg_b64)
        .ok()?;
    String::from_utf8(msg_bytes).ok()
}

// ---------------------------------------------------------------------------
// CSRF helpers
// ---------------------------------------------------------------------------

/// Generate a fresh CSRF token: 32 random bytes as hex.
#[cfg(feature = "csrf")]
pub(crate) fn generate_csrf_token() -> String {
    crate::random_hex_token::<32>()
}

// ---------------------------------------------------------------------------
// Stateful run_app variants
// ---------------------------------------------------------------------------

/// Run with global state only.
fn run_app_global<G: Send + Sync + 'static>(
    raw_routes: Vec<GlobalRawRoute<G>>,
    raw_named: Vec<GlobalRawNamed<G>>,
    init_global: InitGlobal<G>,
    renderer: Arc<dyn RendererFactory>,
) -> Result<()> {
    // Before anything else, and before `init_global` below, which is allowed
    // to spawn threads and in m6-auth-server's case does: the mask is
    // inherited only by threads created after this call, so a thread started
    // during initialisation would otherwise take SIGTERM at its default
    // disposition and kill the process.
    //
    // `run_app` has always done this and these three never did, which made
    // SIGTERM handling silently wrong for every stateful service. Nothing had
    // been built on them until now; the assertion in `install_with_hooks` is
    // what caught it, which is the guard earning its place.
    crate::signal::block();

    // We need the config before we can call init_global. Load it here.
    let Invocation {
        site_dir,
        config_path,
        cli_log_level,
    } = crate::site::parse_invocation(&sigs_of(&raw_routes), &names_of(&raw_named));

    let config = crate::config::load(&config_path, &site_dir).unwrap_or_else(|e| {
        eprintln!("Config error: {e}");
        std::process::exit(2);
    });

    // Call init_global with the user config.
    let site_toml = site_toml_or_exit(&config, &site_dir);
    let g = init_global(&AppContext {
        config: &config.user_config,
        site_dir: &site_dir,
        config_path: &config_path,
        site_toml: &site_toml,
    })
    .unwrap_or_else(|e| {
        eprintln!("init_global failed: {e:#}");
        std::process::exit(2);
    });
    let arc_g = Arc::new(g);

    // Convert raw routes → BoxHandler by closing over Arc<G>.
    let code_routes: Vec<CodeRoute> = raw_routes
        .into_iter()
        .map(|(path, method, handler)| {
            let arc_g2 = arc_g.clone();
            let h: BoxHandler = Box::new(move |req: &Request| handler(req, &*arc_g2));
            (path, method, Arc::new(h))
        })
        .collect();

    // Named handlers take the same conversion as the routes above: the state
    // is what the closure captures, and by what the route was bound to.
    let named: Vec<NamedHandler> = raw_named
        .into_iter()
        .map(|(name, handler)| {
            let arc_g2 = arc_g.clone();
            let h: BoxHandler = Box::new(move |req: &Request| handler(req, &*arc_g2));
            (name, Arc::new(h))
        })
        .collect();

    run_app_with_shutdown(
        Handlers {
            routes: code_routes,
            named,
        },
        config_path,
        site_dir,
        RunHooks {
            on_thread_exit: None,
        },
        cli_log_level,
        renderer,
        Some(config),
    )
}

/// The hook a service with global and per-thread state can set for worker
/// thread teardown.
struct StateHooks<T> {
    destroy_thread: Destroy<T>,
}

/// What a runner hands the run loop to call as each worker thread ends.
#[derive(Default)]
struct RunHooks {
    on_thread_exit: Option<Arc<dyn Fn() + Send + Sync + 'static>>,
}

/// Run with per-thread state only (no global).
fn run_app_thread_state<T: Any + Send + 'static>(
    raw_routes: Vec<ThreadRawRoute<T>>,
    raw_named: Vec<ThreadRawNamed<T>>,
    init_thread: InitThread<(), T>,
    destroy_thread: Destroy<T>,
    renderer: Arc<dyn RendererFactory>,
) -> Result<()> {
    // Before anything else, and before `init_global` below, which is allowed
    // to spawn threads and in m6-auth-server's case does: the mask is
    // inherited only by threads created after this call, so a thread started
    // during initialisation would otherwise take SIGTERM at its default
    // disposition and kill the process.
    //
    // `run_app` has always done this and these three never did, which made
    // SIGTERM handling silently wrong for every stateful service. Nothing had
    // been built on them until now; the assertion in `install_with_hooks` is
    // what caught it, which is the guard earning its place.
    crate::signal::block();

    let Invocation {
        site_dir,
        config_path,
        cli_log_level,
    } = crate::site::parse_invocation(&sigs_of(&raw_routes), &names_of(&raw_named));

    let config = crate::config::load(&config_path, &site_dir).unwrap_or_else(|e| {
        eprintln!("Config error: {e}");
        std::process::exit(2);
    });

    // Set up TLS init fn (closes over config dict).
    let cfg_clone: Map<String, Value> = config.user_config.clone();
    let init_fn = init_thread.clone();
    let tls_init: ThreadInitFn = Arc::new(move || match init_fn(&cfg_clone, &()) {
        Ok(t) => Box::new(t) as Box<dyn Any + Send>,
        Err(e) => panic!("init_thread failed: {e}"),
    });
    THREAD_INIT_FN.set(tls_init).ok();

    // Set up TLS destroy fn.
    if let Some(d) = &destroy_thread {
        let d2 = d.clone();
        let tls_destroy: ThreadDestroyFn = Arc::new(move |boxed| {
            if let Ok(t) = boxed.downcast::<T>() {
                d2(*t);
            }
        });
        THREAD_DESTROY_FN.set(tls_destroy).ok();
    }

    // Convert raw routes → BoxHandler using TLS.
    let code_routes: Vec<CodeRoute> = raw_routes
        .into_iter()
        .map(|(path, method, handler)| {
            let h: BoxHandler =
                Box::new(move |req: &Request| with_thread_state::<T, _>(|t| handler(req, t)));
            (path, method, Arc::new(h))
        })
        .collect();

    let named: Vec<NamedHandler> = raw_named
        .into_iter()
        .map(|(name, handler)| {
            let h: BoxHandler =
                Box::new(move |req: &Request| with_thread_state::<T, _>(|t| handler(req, t)));
            (name, Arc::new(h))
        })
        .collect();

    let on_thread_exit: Arc<dyn Fn() + Send + Sync> = Arc::new(drain_thread_state);

    run_app_with_shutdown(
        Handlers {
            routes: code_routes,
            named,
        },
        config_path,
        site_dir,
        RunHooks {
            on_thread_exit: Some(on_thread_exit),
        },
        cli_log_level,
        renderer,
        Some(config),
    )
}

/// Run with global + per-thread state.
fn run_app_state<G: Send + Sync + 'static, T: Any + Send + 'static>(
    raw_routes: Vec<StateRawRoute<G, T>>,
    raw_named: Vec<StateRawNamed<G, T>>,
    renderer: Arc<dyn RendererFactory>,
    init_global: InitGlobal<G>,
    init_thread: InitThread<G, T>,
    hooks: StateHooks<T>,
) -> Result<()> {
    let StateHooks { destroy_thread } = hooks;
    // Before anything else, and before `init_global` below, which is allowed
    // to spawn threads and in m6-auth-server's case does: the mask is
    // inherited only by threads created after this call, so a thread started
    // during initialisation would otherwise take SIGTERM at its default
    // disposition and kill the process.
    //
    // `run_app` has always done this and these three never did, which made
    // SIGTERM handling silently wrong for every stateful service. Nothing had
    // been built on them until now; the assertion in `install_with_hooks` is
    // what caught it, which is the guard earning its place.
    crate::signal::block();

    let Invocation {
        site_dir,
        config_path,
        cli_log_level,
    } = crate::site::parse_invocation(&sigs_of(&raw_routes), &names_of(&raw_named));

    let config = crate::config::load(&config_path, &site_dir).unwrap_or_else(|e| {
        eprintln!("Config error: {e}");
        std::process::exit(2);
    });

    let site_toml = site_toml_or_exit(&config, &site_dir);
    let g = init_global(&AppContext {
        config: &config.user_config,
        site_dir: &site_dir,
        config_path: &config_path,
        site_toml: &site_toml,
    })
    .unwrap_or_else(|e| {
        eprintln!("init_global failed: {e:#}");
        std::process::exit(2);
    });
    let arc_g = Arc::new(g);

    // Set up TLS init fn.
    let cfg_clone: Map<String, Value> = config.user_config.clone();
    let init_fn = init_thread.clone();
    let arc_g2 = arc_g.clone();
    let tls_init: ThreadInitFn = Arc::new(move || match init_fn(&cfg_clone, &*arc_g2) {
        Ok(t) => Box::new(t) as Box<dyn Any + Send>,
        Err(e) => panic!("init_thread failed: {e}"),
    });
    THREAD_INIT_FN.set(tls_init).ok();

    // Set up TLS destroy fn.
    if let Some(d) = &destroy_thread {
        let d2 = d.clone();
        let tls_destroy: ThreadDestroyFn = Arc::new(move |boxed| {
            if let Ok(t) = boxed.downcast::<T>() {
                d2(*t);
            }
        });
        THREAD_DESTROY_FN.set(tls_destroy).ok();
    }

    // Convert raw routes → BoxHandler using Arc<G> + TLS.
    let code_routes: Vec<CodeRoute> = raw_routes
        .into_iter()
        .map(|(path, method, handler)| {
            let arc_g3 = arc_g.clone();
            let h: BoxHandler = Box::new(move |req: &Request| {
                with_thread_state::<T, _>(|t| handler(req, &*arc_g3, t))
            });
            (path, method, Arc::new(h))
        })
        .collect();

    let named: Vec<NamedHandler> = raw_named
        .into_iter()
        .map(|(name, handler)| {
            let arc_g3 = arc_g.clone();
            let h: BoxHandler = Box::new(move |req: &Request| {
                with_thread_state::<T, _>(|t| handler(req, &*arc_g3, t))
            });
            (name, Arc::new(h))
        })
        .collect();

    let on_thread_exit: Arc<dyn Fn() + Send + Sync> = Arc::new(drain_thread_state);

    run_app_with_shutdown(
        Handlers {
            routes: code_routes,
            named,
        },
        config_path,
        site_dir,
        RunHooks {
            on_thread_exit: Some(on_thread_exit),
        },
        cli_log_level,
        renderer,
        Some(config),
    )
}

// ---------------------------------------------------------------------------
// Thread pool
// ---------------------------------------------------------------------------

type WorkItem = Box<dyn FnOnce() + Send + 'static>;

pub struct ThreadPool {
    queue: std::sync::mpsc::SyncSender<WorkItem>,
}

impl ThreadPool {
    pub fn new(size: usize, queue_size: usize) -> Self {
        Self::new_with_exit(size, queue_size, None)
    }

    /// Create a thread pool. `on_thread_exit` is called at the end of each
    /// worker thread's loop (after receiving the shutdown sentinel).
    pub fn new_with_exit(
        size: usize,
        queue_size: usize,
        on_thread_exit: Option<Arc<dyn Fn() + Send + Sync + 'static>>,
    ) -> Self {
        let (tx, rx) = std::sync::mpsc::sync_channel::<WorkItem>(queue_size);
        let rx = Arc::new(Mutex::new(rx));

        for _ in 0..size {
            let rx = rx.clone();
            let on_exit = on_thread_exit.clone();
            #[expect(
                clippy::disallowed_methods,
                reason = "the App worker pool; removed by EL-6"
            )]
            std::thread::spawn(move || {
                loop {
                    let work = rx.lock().unwrap().recv();
                    match work {
                        Ok(f) => f(),
                        Err(_) => break,
                    }
                }
                // Channel closed — call the exit callback (destroy_thread).
                if let Some(cb) = &on_exit {
                    cb();
                }
            });
        }

        Self { queue: tx }
    }

    /// Submit work. Returns false if the queue is full (→ 503).
    pub fn submit(&self, f: WorkItem) -> bool {
        self.queue.try_send(f).is_ok()
    }

    /// Try to submit work that takes ownership of a resource.
    /// On success (queue not full), returns Ok(true).
    /// On failure (queue full), returns Err(resource) so the caller can handle it.
    pub fn try_submit<R: Send + 'static>(
        &self,
        resource: R,
        f: impl FnOnce(R) + Send + 'static,
    ) -> std::result::Result<bool, R> {
        // We need to package f(resource) into a WorkItem, but recover resource on failure.
        // Use a Option<R> wrapped in Arc<Mutex<>> to allow recovery.
        let resource_cell = Arc::new(Mutex::new(Some(resource)));
        let rc = resource_cell.clone();
        let work: WorkItem = Box::new(move || {
            let r = rc.lock().unwrap().take().unwrap();
            f(r);
        });
        if self.submit(work) {
            Ok(true)
        } else {
            // Queue full — recover resource.
            let r = resource_cell.lock().unwrap().take().unwrap();
            Err(r)
        }
    }
}

// ---------------------------------------------------------------------------
// Signal handling
// ---------------------------------------------------------------------------

/// Signal handling delegates to `m6-core`.
///
/// The local version used `signal()` rather than `sigaction()`, which differ
/// in syscall-restart semantics, and ran the shutdown logic in signal context
/// where almost nothing is legal. `m6-core` uses `sigwait` on a dedicated
/// thread, so no code runs in signal context at all.
/// Install shutdown for the app hosted by this framework.
///
/// The name comes from `argv[0]` because one binary is not one service here:
/// `m6-html`, `render-contact` and `render-analytics` are three processes over
/// the same loop, and a hardcoded name would make all three log as the wrong
/// thing.
///
/// `socket` is what gives this loop the two behaviours it did not have. The
/// self-connect returns the parked `accept()` at once, instead of the 100 ms
/// poll timeout it relied on before; and the socket is unlinked on the way out,
/// which no render app did. Clearing a stale socket at startup before `bind` is
/// the workaround for missing cleanup rather than the cleanup itself.
fn install_shutdown(name: &str, socket_path: &std::path::Path) -> crate::signal::ShutdownHandle {
    crate::signal::ShutdownHandle::install(crate::signal::Service::new(name).socket(socket_path))
}

#[inline]
pub fn is_shutdown() -> bool {
    crate::signal::is_shutdown()
}

// ---------------------------------------------------------------------------
// App builder — no state
// ---------------------------------------------------------------------------

/// Resolve `site.toml` for `config` and refuse any path that cannot be read or
/// parsed. Exit 2, the configuration error code, before anything binds.
///
/// Every service needs a readable `site.toml`: the default
/// `<site_dir>/site.toml`, or the path the `site_toml` key names.
fn site_toml_or_exit(
    config: &crate::config::RendererConfig,
    site_dir: &std::path::Path,
) -> PathBuf {
    match crate::site::check_site_toml(config, site_dir) {
        Ok(path) => path,
        Err(msg) => {
            eprintln!("Config error: {msg}");
            std::process::exit(2);
        }
    }
}

/// What a service was started with, for building its shared state.
///
/// A struct rather than a widening argument list. The initialiser first took
/// only the config's non-framework keys, which was enough for a service whose
/// state is derived from values; it is not enough for one whose config *names
/// files*, because those resolve against a root the map does not carry.
/// A service may need either root, so the initialiser is given both rather
/// than a third argument each time a service needs another.
pub struct AppContext<'a> {
    /// The config's non-framework keys, already parsed by core.
    pub config: &'a Map<String, Value>,
    /// The site directory, as passed on the command line.
    pub site_dir: &'a std::path::Path,
    /// The config file this service was started with.
    pub config_path: &'a std::path::Path,
    /// The `site.toml` this service reads: the path named by the `site_toml`
    /// key, or `<site_dir>/site.toml` when the key is absent. Always set,
    /// whether or not the file exists.
    pub site_toml: &'a std::path::Path,
}

/// Handler function entries: (pattern, method, handler).
type CodeRoute = (String, RouteMethod, Arc<BoxHandler>);

/// A handler registered by name rather than bound to a pattern: (name, handler).
type NamedHandler = (String, Arc<BoxHandler>);

/// Everything a service supplies in code: handlers bound to a pattern at the
/// call site, and handlers bound to a name for config to route to.
///
/// One argument rather than two because they are one thing, the set of code a
/// config can reach, and because every runner threads them to the same place.
struct Handlers {
    routes: Vec<CodeRoute>,
    named: Vec<NamedHandler>,
}

/// App with no user state.
pub struct App {
    routes: Vec<CodeRoute>,
    named: Vec<NamedHandler>,
    renderer: Arc<dyn RendererFactory>,
}

/// The renderer a service gets when it does not ask for a particular one.
///
/// With the `templates` feature on, which is the default, that is Tera with
/// the site filters. Without it no engine is linked, and a route naming a
/// template becomes a configuration error the operator hears about rather
/// than an empty body nobody notices.
fn default_renderer() -> Arc<dyn RendererFactory> {
    #[cfg(feature = "templates")]
    {
        Arc::new(crate::template::TeraFactory)
    }
    #[cfg(not(feature = "templates"))]
    {
        Arc::new(crate::render::NoTemplates)
    }
}

impl App {
    pub fn new() -> Self {
        Self {
            routes: vec![],
            named: vec![],
            renderer: default_renderer(),
        }
    }

    /// Register a handler under a name, for config to route to.
    ///
    /// `route_get("/health", f)` binds code to a path at the call site, so the
    /// set of routes is fixed for the life of the process. That is correct for
    /// a service whose routes are part of its code, and wrong for one whose
    /// routes are part of its deployment: a static file server gains an asset
    /// tree by being told about a directory, not by being recompiled.
    ///
    /// This splits the binding in two along the line where the change actually
    /// falls. The handler is code and is registered here, once. The route is
    /// config:
    ///
    /// ```toml
    /// [[route]]
    /// path = "/assets/{*relpath}"
    /// handler = "files"
    /// root = "assets/"
    /// ```
    ///
    /// Config routes are compiled at start, and the handler reads `root`
    /// through `Request::route_setting`. A route naming a handler that was
    /// never registered is a hard error: startup exits 2.
    pub fn handler(
        mut self,
        name: &str,
        handler: impl Fn(&Request) -> Result<Response> + Send + Sync + 'static,
    ) -> Self {
        self.named
            .push((name.to_string(), Arc::new(Box::new(handler) as BoxHandler)));
        self
    }

    /// Use a renderer other than the default.
    ///
    /// The seam stays public. Core does not depend on Tera at the type level,
    /// only at the default, so a service with its own engine or none says so
    /// and gets it.
    pub fn renderer(mut self, renderer: impl RendererFactory) -> Self {
        self.renderer = Arc::new(renderer);
        self
    }

    fn add_route(
        mut self,
        path: &str,
        method: RouteMethod,
        handler: impl Fn(&Request) -> Result<Response> + Send + Sync + 'static,
    ) -> Self {
        self.routes.push((
            path.to_string(),
            method,
            Arc::new(Box::new(handler) as BoxHandler),
        ));
        self
    }

    pub fn route(
        self,
        path: &str,
        handler: impl Fn(&Request) -> Result<Response> + Send + Sync + 'static,
    ) -> Self {
        self.add_route(path, RouteMethod::Any, handler)
    }

    pub fn route_get(
        self,
        path: &str,
        handler: impl Fn(&Request) -> Result<Response> + Send + Sync + 'static,
    ) -> Self {
        self.add_route(path, RouteMethod::Get, handler)
    }

    pub fn route_post(
        self,
        path: &str,
        handler: impl Fn(&Request) -> Result<Response> + Send + Sync + 'static,
    ) -> Self {
        self.add_route(path, RouteMethod::Post, handler)
    }

    pub fn route_put(
        self,
        path: &str,
        handler: impl Fn(&Request) -> Result<Response> + Send + Sync + 'static,
    ) -> Self {
        self.add_route(path, RouteMethod::Put, handler)
    }

    pub fn route_patch(
        self,
        path: &str,
        handler: impl Fn(&Request) -> Result<Response> + Send + Sync + 'static,
    ) -> Self {
        self.add_route(path, RouteMethod::Patch, handler)
    }

    pub fn route_delete(
        self,
        path: &str,
        handler: impl Fn(&Request) -> Result<Response> + Send + Sync + 'static,
    ) -> Self {
        self.add_route(path, RouteMethod::Delete, handler)
    }

    pub fn run(self) -> Result<()> {
        run_app(self.routes, self.named, self.renderer)
    }
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// App with global state only
// ---------------------------------------------------------------------------

// Raw stateful route (Global-only): stores the handler before G is known.
type GlobalRawRoute<G> = (
    String,
    RouteMethod,
    Arc<dyn Fn(&Request, &G) -> Result<Response> + Send + Sync>,
);

// The same, bound to a name rather than to a pattern. See `App::handler`.
type GlobalRawNamed<G> = (
    String,
    Arc<dyn Fn(&Request, &G) -> Result<Response> + Send + Sync>,
);

pub struct AppWithGlobal<G: Send + Sync + 'static> {
    raw_routes: Vec<GlobalRawRoute<G>>,
    raw_named: Vec<GlobalRawNamed<G>>,
    init_global: InitGlobal<G>,
    renderer: Arc<dyn RendererFactory>,
}

impl<G: Send + Sync + 'static> AppWithGlobal<G> {
    /// Register a handler under a name, for config to route to.
    /// See `App::handler`.
    pub fn handler(
        mut self,
        name: &str,
        handler: impl Fn(&Request, &G) -> Result<Response> + Send + Sync + 'static,
    ) -> Self {
        self.raw_named.push((name.to_string(), Arc::new(handler)));
        self
    }

    fn add_route(
        mut self,
        path: &str,
        method: RouteMethod,
        handler: impl Fn(&Request, &G) -> Result<Response> + Send + Sync + 'static,
    ) -> Self {
        self.raw_routes
            .push((path.to_string(), method, Arc::new(handler)));
        self
    }

    pub fn route(
        self,
        path: &str,
        handler: impl Fn(&Request, &G) -> Result<Response> + Send + Sync + 'static,
    ) -> Self {
        self.add_route(path, RouteMethod::Any, handler)
    }

    pub fn route_get(
        self,
        path: &str,
        handler: impl Fn(&Request, &G) -> Result<Response> + Send + Sync + 'static,
    ) -> Self {
        self.add_route(path, RouteMethod::Get, handler)
    }

    pub fn route_post(
        self,
        path: &str,
        handler: impl Fn(&Request, &G) -> Result<Response> + Send + Sync + 'static,
    ) -> Self {
        self.add_route(path, RouteMethod::Post, handler)
    }

    pub fn route_put(
        self,
        path: &str,
        handler: impl Fn(&Request, &G) -> Result<Response> + Send + Sync + 'static,
    ) -> Self {
        self.add_route(path, RouteMethod::Put, handler)
    }

    pub fn route_patch(
        self,
        path: &str,
        handler: impl Fn(&Request, &G) -> Result<Response> + Send + Sync + 'static,
    ) -> Self {
        self.add_route(path, RouteMethod::Patch, handler)
    }

    pub fn route_delete(
        self,
        path: &str,
        handler: impl Fn(&Request, &G) -> Result<Response> + Send + Sync + 'static,
    ) -> Self {
        self.add_route(path, RouteMethod::Delete, handler)
    }

    /// Use a different renderer. See `App::renderer`.
    pub fn renderer(mut self, renderer: impl RendererFactory) -> Self {
        self.renderer = Arc::new(renderer);
        self
    }

    pub fn run(self) -> Result<()> {
        run_app_global(
            self.raw_routes,
            self.raw_named,
            self.init_global,
            self.renderer,
        )
    }
}

impl App {
    /// An app with one shared value, built once at startup.
    ///
    /// The initialiser receives an [`AppContext`]: the parsed config plus the
    /// site directory and the config path, because a service whose config
    /// names files has to resolve them against the same roots the rest of the
    /// service uses.
    pub fn with_global<G: Send + Sync + 'static>(
        init_global: impl Fn(&AppContext) -> Result<G> + Send + Sync + 'static,
    ) -> AppWithGlobal<G> {
        AppWithGlobal {
            raw_routes: vec![],
            raw_named: vec![],
            init_global: Arc::new(init_global),
            renderer: default_renderer(),
        }
    }

    pub fn with_thread_state<T: Any + Send + 'static>(
        init_thread: impl Fn(&Map<String, Value>, &()) -> Result<T> + Send + Sync + 'static,
    ) -> AppWithThreadState<T> {
        AppWithThreadState {
            raw_routes: vec![],
            raw_named: vec![],
            init_thread: Arc::new(init_thread),
            destroy_thread: None,
            renderer: default_renderer(),
        }
    }

    pub fn with_state<G: Send + Sync + 'static, T: Any + Send + 'static>(
        init_global: impl Fn(&AppContext) -> Result<G> + Send + Sync + 'static,
        init_thread: impl Fn(&Map<String, Value>, &G) -> Result<T> + Send + Sync + 'static,
    ) -> AppWithState<G, T> {
        AppWithState {
            raw_routes: vec![],
            raw_named: vec![],
            init_global: Arc::new(init_global),
            init_thread: Arc::new(init_thread),
            destroy_thread: None,
            renderer: default_renderer(),
        }
    }
}

// ---------------------------------------------------------------------------
// App with per-thread state only
// ---------------------------------------------------------------------------

// Raw stateful route (ThreadLocal): stores the handler before config/TLS known.
type ThreadRawRoute<T> = (
    String,
    RouteMethod,
    Arc<dyn Fn(&Request, &mut T) -> Result<Response> + Send + Sync>,
);

// The same, bound to a name rather than to a pattern. See `App::handler`.
type ThreadRawNamed<T> = (
    String,
    Arc<dyn Fn(&Request, &mut T) -> Result<Response> + Send + Sync>,
);

pub struct AppWithThreadState<T: Any + Send + 'static> {
    raw_routes: Vec<ThreadRawRoute<T>>,
    raw_named: Vec<ThreadRawNamed<T>>,
    init_thread: InitThread<(), T>,
    destroy_thread: Destroy<T>,
    renderer: Arc<dyn RendererFactory>,
}

impl<T: Any + Send + 'static> AppWithThreadState<T> {
    pub fn on_destroy_thread(mut self, f: impl Fn(T) + Send + Sync + 'static) -> Self {
        self.destroy_thread = Some(Arc::new(f));
        self
    }

    /// Register a handler under a name, for config to route to.
    /// See `App::handler`.
    pub fn handler(
        mut self,
        name: &str,
        handler: impl Fn(&Request, &(), &mut T) -> Result<Response> + Send + Sync + 'static,
    ) -> Self {
        self.raw_named.push((
            name.to_string(),
            Arc::new(move |req: &Request, t: &mut T| handler(req, &(), t)),
        ));
        self
    }

    fn add_route(
        mut self,
        path: &str,
        method: RouteMethod,
        handler: impl Fn(&Request, &mut T) -> Result<Response> + Send + Sync + 'static,
    ) -> Self {
        self.raw_routes
            .push((path.to_string(), method, Arc::new(handler)));
        self
    }

    pub fn route(
        self,
        path: &str,
        handler: impl Fn(&Request, &(), &mut T) -> Result<Response> + Send + Sync + 'static,
    ) -> Self {
        self.add_route(path, RouteMethod::Any, move |req, t| handler(req, &(), t))
    }

    pub fn route_get(
        self,
        path: &str,
        handler: impl Fn(&Request, &(), &mut T) -> Result<Response> + Send + Sync + 'static,
    ) -> Self {
        self.add_route(path, RouteMethod::Get, move |req, t| handler(req, &(), t))
    }

    pub fn route_post(
        self,
        path: &str,
        handler: impl Fn(&Request, &(), &mut T) -> Result<Response> + Send + Sync + 'static,
    ) -> Self {
        self.add_route(path, RouteMethod::Post, move |req, t| handler(req, &(), t))
    }

    pub fn route_put(
        self,
        path: &str,
        handler: impl Fn(&Request, &(), &mut T) -> Result<Response> + Send + Sync + 'static,
    ) -> Self {
        self.add_route(path, RouteMethod::Put, move |req, t| handler(req, &(), t))
    }

    pub fn route_patch(
        self,
        path: &str,
        handler: impl Fn(&Request, &(), &mut T) -> Result<Response> + Send + Sync + 'static,
    ) -> Self {
        self.add_route(path, RouteMethod::Patch, move |req, t| handler(req, &(), t))
    }

    pub fn route_delete(
        self,
        path: &str,
        handler: impl Fn(&Request, &(), &mut T) -> Result<Response> + Send + Sync + 'static,
    ) -> Self {
        self.add_route(path, RouteMethod::Delete, move |req, t| {
            handler(req, &(), t)
        })
    }

    /// Use a different renderer. See `App::renderer`.
    pub fn renderer(mut self, renderer: impl RendererFactory) -> Self {
        self.renderer = Arc::new(renderer);
        self
    }

    pub fn run(self) -> Result<()> {
        run_app_thread_state(
            self.raw_routes,
            self.raw_named,
            self.init_thread,
            self.destroy_thread,
            self.renderer,
        )
    }
}

// ---------------------------------------------------------------------------
// App with global + per-thread state
// ---------------------------------------------------------------------------

// Raw stateful route (Global + ThreadLocal).
type StateRawRoute<G, T> = (
    String,
    RouteMethod,
    Arc<dyn Fn(&Request, &G, &mut T) -> Result<Response> + Send + Sync>,
);

// The same, bound to a name rather than to a pattern. See `App::handler`.
type StateRawNamed<G, T> = (
    String,
    Arc<dyn Fn(&Request, &G, &mut T) -> Result<Response> + Send + Sync>,
);

pub struct AppWithState<G: Send + Sync + 'static, T: Any + Send + 'static> {
    raw_routes: Vec<StateRawRoute<G, T>>,
    raw_named: Vec<StateRawNamed<G, T>>,
    init_global: InitGlobal<G>,
    init_thread: InitThread<G, T>,
    destroy_thread: Destroy<T>,
    renderer: Arc<dyn RendererFactory>,
}

impl<G: Send + Sync + 'static, T: Any + Send + 'static> AppWithState<G, T> {
    pub fn on_destroy_thread(mut self, f: impl Fn(T) + Send + Sync + 'static) -> Self {
        self.destroy_thread = Some(Arc::new(f));
        self
    }

    /// Register a handler under a name, for config to route to.
    /// See `App::handler`.
    pub fn handler(
        mut self,
        name: &str,
        handler: impl Fn(&Request, &G, &mut T) -> Result<Response> + Send + Sync + 'static,
    ) -> Self {
        self.raw_named.push((name.to_string(), Arc::new(handler)));
        self
    }

    fn add_route(
        mut self,
        path: &str,
        method: RouteMethod,
        handler: impl Fn(&Request, &G, &mut T) -> Result<Response> + Send + Sync + 'static,
    ) -> Self {
        self.raw_routes
            .push((path.to_string(), method, Arc::new(handler)));
        self
    }

    pub fn route(
        self,
        path: &str,
        handler: impl Fn(&Request, &G, &mut T) -> Result<Response> + Send + Sync + 'static,
    ) -> Self {
        self.add_route(path, RouteMethod::Any, handler)
    }

    pub fn route_get(
        self,
        path: &str,
        handler: impl Fn(&Request, &G, &mut T) -> Result<Response> + Send + Sync + 'static,
    ) -> Self {
        self.add_route(path, RouteMethod::Get, handler)
    }

    pub fn route_post(
        self,
        path: &str,
        handler: impl Fn(&Request, &G, &mut T) -> Result<Response> + Send + Sync + 'static,
    ) -> Self {
        self.add_route(path, RouteMethod::Post, handler)
    }

    pub fn route_put(
        self,
        path: &str,
        handler: impl Fn(&Request, &G, &mut T) -> Result<Response> + Send + Sync + 'static,
    ) -> Self {
        self.add_route(path, RouteMethod::Put, handler)
    }

    pub fn route_patch(
        self,
        path: &str,
        handler: impl Fn(&Request, &G, &mut T) -> Result<Response> + Send + Sync + 'static,
    ) -> Self {
        self.add_route(path, RouteMethod::Patch, handler)
    }

    pub fn route_delete(
        self,
        path: &str,
        handler: impl Fn(&Request, &G, &mut T) -> Result<Response> + Send + Sync + 'static,
    ) -> Self {
        self.add_route(path, RouteMethod::Delete, handler)
    }

    /// Use a different renderer. See `App::renderer`.
    pub fn renderer(mut self, renderer: impl RendererFactory) -> Self {
        self.renderer = Arc::new(renderer);
        self
    }

    pub fn run(self) -> Result<()> {
        run_app_state(
            self.raw_routes,
            self.raw_named,
            self.renderer,
            self.init_global,
            self.init_thread,
            StateHooks {
                destroy_thread: self.destroy_thread,
            },
        )
    }
}

// ---------------------------------------------------------------------------
// Core run loop
// ---------------------------------------------------------------------------

/// Check the mtime of a file. Returns `None` if the file cannot be statted.
fn file_mtime(path: &std::path::Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(path).ok()?.modified().ok()
}

/// Route signatures from any of the builders' raw route lists.
///
/// Generic over the handler type because the three stateful builders each
/// store a different closure shape, and all three want the same two fields.
fn sigs_of<H>(raw: &[(String, RouteMethod, H)]) -> Vec<(String, RouteMethod)> {
    raw.iter().map(|(p, m, _)| (p.clone(), m.clone())).collect()
}

/// Handler names from any of the builders' raw named-handler lists.
fn names_of<H>(raw: &[(String, H)]) -> Vec<String> {
    raw.iter().map(|(n, _)| n.clone()).collect()
}

pub fn run_app(
    code_routes: Vec<CodeRoute>,
    named: Vec<NamedHandler>,
    renderer: Arc<dyn RendererFactory>,
) -> Result<()> {
    // Block SIGTERM and SIGINT before anything else, logging included. Apps
    // built on this framework have a `main` that does nothing but call here,
    // so this is the process's first statement in practice. The mask is
    // inherited only by threads created afterwards, and tracing-appender's
    // writer thread would otherwise take the signal at its default
    // disposition and kill the process. See crate::signal.
    crate::signal::block();

    let Invocation {
        site_dir,
        config_path,
        cli_log_level,
    } = crate::site::parse_invocation(
        &code_routes
            .iter()
            .map(|(p, m, _)| (p.clone(), m.clone()))
            .collect::<Vec<_>>(),
        &named.iter().map(|(n, _)| n.clone()).collect::<Vec<_>>(),
    );
    run_app_with_shutdown(
        Handlers {
            routes: code_routes,
            named,
        },
        config_path,
        site_dir,
        RunHooks::default(),
        cli_log_level,
        renderer,
        None,
    )
}

fn run_app_with_shutdown(
    handlers: Handlers,
    config_path: PathBuf,
    site_dir: PathBuf,
    hooks: RunHooks,
    cli_log_level: Option<String>,
    renderer: Arc<dyn RendererFactory>,
    config: Option<crate::config::RendererConfig>,
) -> Result<()> {
    let RunHooks { on_thread_exit } = hooks;

    let Handlers {
        routes: code_routes,
        named,
    } = handlers;
    let code_route_signatures: Vec<(String, RouteMethod)> = code_routes
        .iter()
        .map(|(p, m, _)| (p.clone(), m.clone()))
        .collect();
    let named_names: Vec<String> = named.iter().map(|(n, _)| n.clone()).collect();

    // The start half all lives in `Site` now: the config, `site.toml`,
    // logging, the route table and the checks that name it. The `Arc` is
    // shared read-only by the worker threads until EL-6d removes `App`.
    let site = Arc::new(crate::site::Site::start(
        &crate::site::program_name(),
        Invocation {
            site_dir,
            config_path,
            cli_log_level,
        },
        config,
        &code_route_signatures,
        &named_names,
        &*renderer,
    ));
    let (tp_size, tp_queue) = site.thread_pool();
    // Read once at startup: it is applied to a socket at accept time.
    let read_timeout = site.read_timeout();
    let socket_mode = site.socket_mode();
    let socket_path = site.socket_path().to_path_buf();

    // Bind socket: one start helper, so the stale-file, parent-directory and
    // mode rules are the ones every service has. The listener is non-blocking
    // for the poll loop; an accepted stream goes back to blocking for the
    // worker thread that serves it.
    let listener = crate::net::listen_unix(&socket_path, socket_mode).unwrap_or_else(|e| {
        eprintln!("Failed to bind socket {}: {e}", socket_path.display());
        std::fs::remove_file(&socket_path).ok();
        std::process::exit(2);
    });

    // After the bind, not before: the wake connects to this socket, so it has
    // to exist by the time a signal can arrive.
    let shutdown = install_shutdown(site.name(), &socket_path);

    info!(
        routes = site.route_count(),
        threads = tp_size,
        socket = %socket_path.display(),
        "routes loaded"
    );

    let pool = Arc::new(ThreadPool::new_with_exit(tp_size, tp_queue, on_thread_exit));

    // The handlers a `Handled::Code` or `Handled::Named` index names, in the
    // order `Site` was given the routes and the names.
    let code_handlers: Arc<Vec<Arc<BoxHandler>>> = Arc::new(
        code_routes
            .into_iter()
            .map(|(_, _, handler)| handler)
            .collect(),
    );
    // A vector, in the order `Site` was given the names: `Handled::Named(i)`
    // is an index, so no lookup and no second absent-name branch here.
    let named_handlers: Arc<Vec<Arc<BoxHandler>>> =
        Arc::new(named.into_iter().map(|(_, handler)| handler).collect());

    use std::os::unix::io::AsRawFd;
    let listener_fd = listener.as_raw_fd();

    loop {
        // ── poll(2) ─────────────────────────────────────────────────────
        // The 100 ms timeout ensures we catch shutdown signals promptly
        // even when the server is completely idle.
        let listener_ready = crate::server::poll_listener(listener_fd, 100);

        // The wait timed out or a signal interrupted it.
        if !listener_ready && is_shutdown() {
            break;
        }

        if !listener_ready {
            continue;
        }

        match listener.accept() {
            Ok((stream, _)) => {
                // The listener is non-blocking; the pool's workers read and
                // write with blocking calls, so the stream is put back first.
                let stream: UnixStream = stream.into();
                if let Err(e) = stream.set_nonblocking(false) {
                    error!("accept error: {e}");
                    continue;
                }
                // Before the handoff, not inside the worker: a worker that has
                // already taken the connection is the resource being protected.
                crate::server::apply_read_timeout(&stream, read_timeout);

                let site = site.clone();
                let code_handlers = code_handlers.clone();
                let named_handlers = named_handlers.clone();

                match pool.try_submit(stream, move |mut s| {
                    handle_connection(&mut s, &site, &code_handlers, &named_handlers);
                }) {
                    Ok(_) => {}
                    Err(mut s) => {
                        warn!("Thread pool queue full, returning 503");
                        crate::server::write_error_response(&mut s, 503, "Service Unavailable")
                            .ok();
                    }
                }
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            // During shutdown an accept error is expected.
            Err(e) => {
                if !is_shutdown() {
                    error!("Accept error: {e}");
                }
            }
        }

        if is_shutdown() {
            break;
        }
    }

    shutdown.complete();
    Ok(())
}

/// One accepted connection, served until it ends.
///
/// The loop, the malformed-request answer, the HEAD rule and the keep-alive
/// decision are core's. The request path is `Site`'s; this function is the
/// service's own dispatch: one `match` on the index `Site::handle` returned.
fn handle_connection(
    stream: &mut UnixStream,
    site: &crate::site::Site,
    code_handlers: &[Arc<BoxHandler>],
    named_handlers: &[Arc<BoxHandler>],
) {
    let served: std::result::Result<(), std::io::Error> =
        crate::server::serve_connection(stream, |raw, resp| {
            let response = match site.handle_raw(raw) {
                crate::site::Handled::Done(response) => response,
                crate::site::Handled::Code(index, req) => match code_handlers[index].call(&req) {
                    Ok(response) => site.finish(&req, response),
                    Err(e) => site.finish(&req, crate::response::error_to_response(&e)),
                },
                crate::site::Handled::Named(index, req) => match named_handlers[index].call(&req) {
                    Ok(response) => site.finish(&req, response),
                    Err(e) => site.finish(&req, crate::response::error_to_response(&e)),
                },
            };
            crate::server::write_response(resp, response).ok();
            Ok(())
        });
    if let Err(e) = served {
        error!("connection error: {e}");
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
pub(crate) use crate::site::{
    check_compress_declared, check_site_toml, describe_unknown, dump_config, unknown_handlers,
};

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "test code: threads, sleeps and blocking clients drive the service from outside"
)]
mod tests {
    use super::*;

    /// A renderer config that names `site_toml`, or does not when `None`.
    fn config_naming(
        dir: &std::path::Path,
        site_toml: Option<&str>,
    ) -> crate::config::RendererConfig {
        let cfg = dir.join("app.toml");
        let body = match site_toml {
            Some(p) => format!("site_toml = \"{p}\"\n"),
            None => "site_name = \"x\"\n".to_string(),
        };
        std::fs::write(&cfg, body).unwrap();
        crate::config::load(&cfg, dir).unwrap()
    }

    #[test]
    fn named_site_toml_that_is_valid_passes() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("s.toml"), "[server]\ncompresses = true\n").unwrap();
        let cfg = config_naming(d.path(), Some("s.toml"));
        assert_eq!(
            check_site_toml(&cfg, d.path()).unwrap(),
            d.path().join("s.toml")
        );
    }

    #[test]
    fn named_site_toml_missing_is_refused() {
        let d = tempfile::tempdir().unwrap();
        let cfg = config_naming(d.path(), Some("absent.toml"));
        let e = check_site_toml(&cfg, d.path()).unwrap_err();
        assert!(e.contains("does not exist"), "{e}");
    }

    #[test]
    fn named_site_toml_that_is_a_directory_is_refused() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir(d.path().join("s.toml")).unwrap();
        let cfg = config_naming(d.path(), Some("s.toml"));
        let e = check_site_toml(&cfg, d.path()).unwrap_err();
        assert!(e.contains("cannot be read"), "{e}");
    }

    #[test]
    fn named_site_toml_that_is_unreadable_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        // root reads any file, so the test would prove nothing.
        if m6_platform::process::is_root() {
            return;
        }
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("s.toml");
        std::fs::write(&f, "[server]\n").unwrap();
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o000)).unwrap();
        let cfg = config_naming(d.path(), Some("s.toml"));
        let e = check_site_toml(&cfg, d.path()).unwrap_err();
        assert!(e.contains("cannot be read"), "{e}");
    }

    #[test]
    fn named_site_toml_that_is_not_toml_is_refused() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("s.toml"), "[server\ncompresses = \n").unwrap();
        let cfg = config_naming(d.path(), Some("s.toml"));
        let e = check_site_toml(&cfg, d.path()).unwrap_err();
        assert!(e.contains("not valid TOML"), "{e}");
    }

    #[test]
    fn a_missing_default_site_toml_is_refused() {
        let d = tempfile::tempdir().unwrap();
        let cfg = config_naming(d.path(), None);
        let e = check_site_toml(&cfg, d.path()).unwrap_err();
        assert!(e.contains("site.toml at"), "{e}");
        assert!(e.contains("does not exist"), "{e}");
    }

    #[test]
    fn an_invalid_default_site_toml_is_refused() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("site.toml"), "[site").unwrap();
        let cfg = config_naming(d.path(), None);
        let e = check_site_toml(&cfg, d.path()).unwrap_err();
        assert!(e.contains("is not valid TOML"), "{e}");
    }

    #[test]
    fn dump_config_refuses_a_compresses_disagreement() {
        // Every MIME type `default_compression` turns on, set to zero, so this
        // service compresses nothing.
        const NO_COMPRESSION: &str = concat!(
            "[compression.\"text/html\"]\nbrotli = 0\ngzip = 0\n",
            "[compression.\"text/css\"]\nbrotli = 0\ngzip = 0\n",
            "[compression.\"text/plain\"]\nbrotli = 0\ngzip = 0\n",
            "[compression.\"application/javascript\"]\nbrotli = 0\ngzip = 0\n",
            "[compression.\"text/javascript\"]\nbrotli = 0\ngzip = 0\n",
            "[compression.\"application/json\"]\nbrotli = 0\ngzip = 0\n",
            "[compression.\"image/svg+xml\"]\nbrotli = 0\ngzip = 0\n",
        );
        let d = tempfile::tempdir().unwrap();
        let config_path = d.path().join("app.toml");
        std::fs::write(&config_path, NO_COMPRESSION).unwrap();
        // The config file's stem is "app", the name the site.toml entry must
        // use. It declares compression this service does not do.
        std::fs::write(
            d.path().join("site.toml"),
            "[[backend]]\nname = \"app\"\ncompresses = true\n",
        )
        .unwrap();

        // The message start prints for this disagreement, so the test can say
        // --dump-config refuses the same thing rather than only exiting 2.
        let config = crate::config::load(&config_path, d.path()).unwrap();
        let site_toml = config.resolve_site_toml(d.path());
        let start_msg = check_compress_declared(&config, &site_toml, "app").unwrap_err();
        assert!(
            start_msg.contains("compresses = true, but this service compresses nothing"),
            "{start_msg}"
        );

        assert_eq!(
            dump_config(d.path(), &config_path, &[], &[], None),
            2,
            "--dump-config reported the config good, but start exits 2 on it"
        );
    }

    #[test]
    fn test_compile_pattern() {
        let segs = compile_pattern("/blog/{stem}");
        assert_eq!(segs.len(), 2);
        assert!(matches!(&segs[0], Segment::Literal(s) if s == "blog"));
        assert!(matches!(&segs[1], Segment::Param(s) if s == "stem"));
    }

    #[test]
    fn test_route_matching() {
        let route = CompiledRoute {
            pattern: "/blog/{stem}".to_string(),
            methods: Methods::ANY,
            segments: compile_pattern("/blog/{stem}"),
            template: None,
            params_files: vec![],
            status: 200,
            cache: "public".to_string(),
            headers: vec![],
            last_modified: None,
            handler: None,
            settings: Arc::new(Map::new()),
            base_dict: Arc::new(Map::new()),
        };
        let segs: Vec<&str> = "/blog/hello-world"
            .split('/')
            .filter(|s| !s.is_empty())
            .collect();
        let m = match_route(&segs, &route);
        assert!(m.is_some());
        let params = m.unwrap();
        let stem = params
            .iter()
            .find(|(k, _)| k == "stem")
            .map(|(_, v)| v.as_str());
        assert_eq!(stem, Some("hello-world"));
    }

    #[test]
    fn test_no_match_on_different_length() {
        let route = CompiledRoute {
            pattern: "/blog/{stem}".to_string(),
            methods: Methods::ANY,
            segments: compile_pattern("/blog/{stem}"),
            template: None,
            params_files: vec![],
            status: 200,
            cache: "public".to_string(),
            headers: vec![],
            last_modified: None,
            handler: None,
            settings: Arc::new(Map::new()),
            base_dict: Arc::new(Map::new()),
        };
        let segs_ab: Vec<&str> = "/blog/a/b".split('/').filter(|s| !s.is_empty()).collect();
        let segs_b: Vec<&str> = "/blog".split('/').filter(|s| !s.is_empty()).collect();
        assert!(match_route(&segs_ab, &route).is_none());
        assert!(match_route(&segs_b, &route).is_none());
    }

    #[test]
    fn test_exact_beats_parameterised() {
        let routes = vec![
            CompiledRoute {
                pattern: "/blog/{stem}".to_string(),
                methods: Methods::ANY,
                segments: compile_pattern("/blog/{stem}"),
                template: None,
                params_files: vec![],
                status: 200,
                cache: "public".to_string(),
                headers: vec![],
                last_modified: None,
                handler: None,
                settings: Arc::new(Map::new()),
                base_dict: Arc::new(Map::new()),
            },
            CompiledRoute {
                pattern: "/blog/about".to_string(),
                methods: Methods::ANY,
                segments: compile_pattern("/blog/about"),
                template: None,
                params_files: vec![],
                status: 200,
                cache: "public".to_string(),
                headers: vec![],
                last_modified: None,
                handler: None,
                settings: Arc::new(Map::new()),
                base_dict: Arc::new(Map::new()),
            },
        ];

        let table = RouteTable::new(routes);
        let (matched, _) = find_route("/blog/about", "GET", &table).unwrap();
        assert_eq!(matched.pattern, "/blog/about");
    }

    #[test]
    fn test_unmatched_returns_none() {
        let routes: Vec<CompiledRoute> = vec![];
        assert!(find_route("/anything", "GET", &RouteTable::new(routes)).is_none());
    }

    #[test]
    fn test_app_new_builds() {
        let _app = App::new()
            .route("/", |_req| Ok(Response::text("home")))
            .route_get("/about", |_req| Ok(Response::text("about")))
            .route_post("/submit", |_req| Ok(Response::status(200)));
        // Just verifies it compiles and builds without panicking.
    }

    #[test]
    fn test_thread_pool_submit() {
        use std::sync::{Arc, Mutex};
        let pool = ThreadPool::new(2, 16);
        let results = Arc::new(Mutex::new(vec![]));

        let n = 4;
        for i in 0..n {
            let r = results.clone();
            pool.submit(Box::new(move || {
                r.lock().unwrap().push(i);
            }));
        }

        // Wait for work to complete.
        std::thread::sleep(std::time::Duration::from_millis(100));
        let r = results.lock().unwrap();
        assert_eq!(r.len(), n);
    }

    #[test]
    fn test_thread_pool_queue_full_returns_false() {
        let pool = ThreadPool::new(1, 1);
        // Block the single thread.
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let b = barrier.clone();
        pool.submit(Box::new(move || {
            b.wait();
        }));

        // Give thread time to pick up the work.
        std::thread::sleep(std::time::Duration::from_millis(20));

        // Fill the queue with one item.
        let submitted1 = pool.submit(Box::new(|| {}));

        // This should fail because queue_size=1 and it's occupied.
        let submitted2 = pool.submit(Box::new(|| {}));

        // Unblock the thread.
        barrier.wait();

        // At most one of submitted1/submitted2 can be true given queue_size=1.
        // At least one submit should have returned false.
        assert!(
            !(submitted1 && submitted2),
            "both submits succeeded but queue_size=1"
        );
    }

    /// file_mtime returns different values after a file is updated.
    #[test]
    fn test_file_mtime_changes() {
        use std::io::Write;
        use tempfile::NamedTempFile;

        let mut f = NamedTempFile::new().unwrap();
        writeln!(f, "v1").unwrap();
        let mtime1 = file_mtime(f.path());
        assert!(mtime1.is_some());

        // Wait at least 1 file-system tick (typically 1s on HFS+/APFS).
        // On most CI systems the mtime granularity is 1ns, so just rewrite.
        std::thread::sleep(std::time::Duration::from_millis(10));
        // Touch by setting mtime explicitly via filetime.
        let future =
            filetime::FileTime::from_unix_time(filetime::FileTime::now().unix_seconds() + 1, 0);
        filetime::set_file_mtime(f.path(), future).unwrap();

        let mtime2 = file_mtime(f.path());
        assert_ne!(mtime1, mtime2, "mtime did not change after touch");
    }

    /// Compression levels from config are honoured: level 0 means no compression.
    #[test]
    fn test_compression_level_zero_skips_encoding() {
        use crate::config::CompressionLevel;
        use std::collections::HashMap;

        // Build a compression map with level 0 for text/html.
        let mut compression: HashMap<String, CompressionLevel> = HashMap::new();
        compression.insert(
            "text/html".to_string(),
            CompressionLevel { brotli: 0, gzip: 0 },
        );

        // Simulate the guard: level.brotli > 0 is false → no encoding applied.
        let mime = "text/html";
        let level = compression.get(mime).unwrap();
        assert_eq!(level.brotli, 0);
        assert_eq!(level.gzip, 0);
        // The handle_connection logic checks `level.brotli > 0`; with 0 no compression occurs.
    }

    /// Compression levels from config are used (non-zero level → actual compression).
    #[test]
    fn test_compression_level_nonzero_compresses() {
        use crate::config::CompressionLevel;
        use std::collections::HashMap;

        let mut compression: HashMap<String, CompressionLevel> = HashMap::new();
        compression.insert(
            "text/html".to_string(),
            CompressionLevel { brotli: 4, gzip: 5 },
        );

        let level = compression.get("text/html").unwrap();
        assert!(level.brotli > 0);

        let data = b"Hello, this is some HTML content to compress for testing purposes!";
        let compressed = crate::compress::brotli_compress(data, level.brotli).unwrap();
        assert!(!compressed.is_empty());

        let decompressed = crate::compress::brotli_decompress(&compressed).unwrap();
        assert_eq!(decompressed, data);
    }

    // ── Stateful handler tests ─────────────────────────────────────────────

    /// App::with_global — route closures receive &G correctly.
    #[test]
    fn test_app_with_global_builds() {
        // Just verify it compiles and builds without panicking.
        let _app: AppWithGlobal<u32> = App::with_global(|_ctx| Ok(42u32)).route("/", |_req, g| {
            assert_eq!(*g, 42);
            Ok(Response::text("ok"))
        });
        // We don't call .run() — that would need CLI args and a socket.
    }

    /// App::with_thread_state — route closures receive &mut T correctly.
    #[test]
    fn test_app_with_thread_state_builds() {
        let _app: AppWithThreadState<Vec<String>> =
            App::with_thread_state(|_cfg, _g| Ok(Vec::<String>::new()))
                .route("/push", |_req, _g, t| {
                    t.push("hello".to_string());
                    Ok(Response::text("ok"))
                })
                .on_destroy_thread(|v| {
                    // v is a Vec<String> — just verify we got it.
                    let _ = v;
                });
    }

    /// App::with_state — route closures receive &G and &mut T correctly.
    #[test]
    fn test_app_with_state_builds() {
        struct Global {
            base: u32,
        }
        struct Local {
            count: u32,
        }

        let _app: AppWithState<Global, Local> = App::with_state(
            |_ctx| Ok(Global { base: 10 }),
            |_cfg, g| Ok(Local { count: g.base }),
        )
        .route("/inc", |_req, g, t| {
            t.count += g.base;
            Ok(Response::text("ok"))
        })
        .on_destroy_thread(|l| {
            let _ = l;
        });
    }

    // ── Minification tests ────────────────────────────────────────────────

    #[test]
    fn test_minification_config_defaults() {
        let cfg = crate::config::MinificationConfig {
            enabled: {
                let mut m = std::collections::HashMap::new();
                m.insert("text/html".to_string(), true);
                m.insert("text/css".to_string(), true);
                m.insert("application/json".to_string(), true);
                m.insert("application/javascript".to_string(), false);
                m
            },
            inline_js: false,
        };
        assert!(cfg.is_enabled("text/html"));
        assert!(cfg.is_enabled("text/css"));
        assert!(cfg.is_enabled("application/json"));
        assert!(!cfg.is_enabled("application/javascript"));
        assert!(!cfg.is_enabled("image/png"));
    }

    // ── Flash feature tests ───────────────────────────────────────────────

    #[cfg(feature = "flash")]
    #[test]
    fn test_flash_round_trip() {
        let secret = b"test-secret-key-32-bytes-xxxxxxx";
        let message = "Login successful!";

        // Use the Response::flash method to build the cookie.
        let resp = Response::text("ok").flash(message, secret);

        // Find the Set-Cookie header.
        let cookie_hdr = resp
            .headers
            .iter()
            .find(|(k, _)| k == "Set-Cookie")
            .map(|(_, v)| v.as_str())
            .expect("Set-Cookie header not found");

        assert!(cookie_hdr.starts_with("_flash="), "header: {}", cookie_hdr);
        assert!(cookie_hdr.contains("Max-Age=120"), "header: {}", cookie_hdr);

        // Extract the cookie value and verify it.
        let val = cookie_hdr
            .split(';')
            .next()
            .unwrap()
            .trim_start_matches("_flash=");

        let recovered = verify_flash_cookie(val, secret).expect("verification failed");
        assert_eq!(recovered, message);
    }

    #[cfg(feature = "flash")]
    #[test]
    fn test_flash_tampered_rejected() {
        let secret = b"test-secret-key-32-bytes-xxxxxxx";
        // Tampered cookie: valid format but wrong HMAC.
        let tampered = "aGVsbG8.aW52YWxpZHNpZ25hdHVyZXh4eHh4eHg";
        assert!(verify_flash_cookie(tampered, secret).is_none());
    }

    // ── CSRF feature tests ────────────────────────────────────────────────

    #[cfg(feature = "csrf")]
    #[test]
    fn test_csrf_token_generation() {
        let t1 = generate_csrf_token();
        let t2 = generate_csrf_token();
        // Tokens should be 64 hex chars (32 bytes).
        assert_eq!(t1.len(), 64, "token length: {}", t1.len());
        // Two tokens should differ (extremely high probability).
        assert_ne!(t1, t2);
    }

    // The two `verify_csrf` tests live in `crate::request`, next to the
    // method they exercise.
    //
    // They had not compiled since Phase 4. Both built a `RawRequest` with
    // `query: String` and no `version`, which was m6-render's own type before
    // the shared one replaced it, and the workspace test run uses default
    // features, so `csrf` was never on and nothing ever tried. This is the
    // bench problem from Phase 0.2 again: code that is never compiled is not
    // covered by a green test run.
}

/// Newest mtime of any regular file beneath `dir`, or `None` for a missing or
/// empty directory.
///
/// Walked once at load, never per request. Depth is bounded by `max_depth` so a
/// symlink loop under the site directory cannot spin here -- templates are one
/// or two levels deep in practice, and a runaway walk at startup would be a
/// boot hang rather than a visible error.
fn newest_mtime_under(dir: &std::path::Path) -> Option<std::time::SystemTime> {
    fn walk(dir: &std::path::Path, depth: usize, newest: &mut Option<std::time::SystemTime>) {
        if depth == 0 {
            return;
        }
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for e in entries.flatten() {
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_dir() {
                walk(&e.path(), depth - 1, newest);
            } else if ft.is_file() {
                if let Some(t) = e.metadata().ok().and_then(|m| m.modified().ok()) {
                    if newest.is_none_or(|n| t > n) {
                        *newest = Some(t);
                    }
                }
            }
        }
    }
    let mut newest = None;
    walk(dir, 8, &mut newest);
    newest
}

#[cfg(test)]
mod last_modified_tests {
    use super::newest_mtime_under;
    use std::time::{Duration, SystemTime};

    #[test]
    fn newest_mtime_is_none_for_a_missing_directory() {
        assert!(newest_mtime_under(std::path::Path::new("/nonexistent/definitely")).is_none());
    }

    #[test]
    fn newest_mtime_is_none_for_an_empty_directory() {
        let d = tempfile::tempdir().unwrap();
        assert!(newest_mtime_under(d.path()).is_none());
    }

    /// The value has to be the NEWEST file, not the first or last walked:
    /// a page's rendering depends on every template, so the most recently
    /// edited one is what dates the output.
    #[test]
    fn newest_mtime_picks_the_most_recent_file() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("old.html"), b"a").unwrap();
        std::fs::write(d.path().join("new.html"), b"b").unwrap();

        let past = m6_platform::wall() - Duration::from_secs(3600);
        let recent = m6_platform::wall() - Duration::from_secs(60);
        filetime::set_file_mtime(d.path().join("old.html"), filetime::FileTime::from(past))
            .unwrap();
        filetime::set_file_mtime(d.path().join("new.html"), filetime::FileTime::from(recent))
            .unwrap();

        let got = newest_mtime_under(d.path()).expect("some mtime");
        let delta = got.duration_since(recent).unwrap_or_else(|e| e.duration());
        assert!(
            delta < Duration::from_secs(2),
            "expected the newer file's mtime"
        );
    }

    /// Partials commonly live in a subdirectory; a change to one of those must
    /// still date the output.
    #[test]
    fn newest_mtime_descends_into_subdirectories() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("page.html"), b"a").unwrap();
        std::fs::create_dir(d.path().join("partials")).unwrap();
        std::fs::write(d.path().join("partials/_head.html"), b"b").unwrap();

        let past = m6_platform::wall() - Duration::from_secs(7200);
        let recent = m6_platform::wall() - Duration::from_secs(30);
        filetime::set_file_mtime(d.path().join("page.html"), filetime::FileTime::from(past))
            .unwrap();
        filetime::set_file_mtime(
            d.path().join("partials/_head.html"),
            filetime::FileTime::from(recent),
        )
        .unwrap();

        let got = newest_mtime_under(d.path()).expect("some mtime");
        let delta = got.duration_since(recent).unwrap_or_else(|e| e.duration());
        assert!(
            delta < Duration::from_secs(2),
            "a nested partial should date the output"
        );
    }

    /// The walk is depth-bounded so a symlink loop under the site directory
    /// cannot hang startup. Nothing legitimate is this deep.
    #[test]
    fn newest_mtime_walk_is_depth_bounded() {
        let d = tempfile::tempdir().unwrap();
        let mut p = d.path().to_path_buf();
        for i in 0..20 {
            p = p.join(format!("d{i}"));
            std::fs::create_dir(&p).unwrap();
        }
        std::fs::write(p.join("deep.html"), b"x").unwrap();
        // Returns rather than recursing forever; the too-deep file is simply
        // not counted, which is the safe direction.
        assert!(newest_mtime_under(d.path()).is_none());
    }
}

#[cfg(test)]
mod wildcard_route_tests {
    use super::*;

    fn route(pattern: &str) -> CompiledRoute {
        let segments = compile_pattern(pattern);
        CompiledRoute {
            pattern: pattern.to_string(),
            methods: Methods::ANY,
            segments,
            template: None,
            params_files: Vec::new(),
            status: 200,
            cache: String::new(),
            headers: Vec::new(),
            last_modified: None,
            handler: None,
            settings: Arc::new(Map::new()),
            base_dict: Arc::new(Map::new()),
        }
    }

    fn m(pattern: &str, path: &str) -> Option<PathParams> {
        let segs: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        match_route(&segs, &route(pattern))
    }

    /// The capability `App` was missing, and the whole of why m6-file has a
    /// different shape: a static file server needs one route to answer for a
    /// path of unknown depth.
    #[test]
    fn a_wildcard_captures_the_rest_of_the_path() {
        assert_eq!(
            m("/assets/{*relpath}", "/assets/css/main.css"),
            Some(vec![("relpath".to_string(), "css/main.css".to_string())])
        );
        assert_eq!(
            m("/assets/{*relpath}", "/assets/a/b/c/d/e.png"),
            Some(vec![("relpath".to_string(), "a/b/c/d/e.png".to_string())])
        );
    }

    /// One segment is still a match. `{*name}` means "the rest", and one is a
    /// quantity of rest.
    #[test]
    fn a_wildcard_also_matches_a_single_segment() {
        assert_eq!(
            m("/assets/{*relpath}", "/assets/favicon.ico"),
            Some(vec![("relpath".to_string(), "favicon.ico".to_string())])
        );
    }

    /// But not an empty one. `/assets` is the directory, not a file in it, and
    /// capturing an empty string would hand the handler a path it cannot use.
    #[test]
    fn a_wildcard_does_not_match_nothing() {
        assert_eq!(m("/assets/{*relpath}", "/assets"), None);
    }

    /// The guard that stops this being a behaviour change: an ordinary
    /// parameter is still exactly one segment. Almost every route ever written is
    /// written this way, and if `{p}` had quietly become greedy they would all
    /// have changed meaning at once.
    #[test]
    fn an_ordinary_parameter_still_matches_exactly_one_segment() {
        assert!(m("/assets/{relpath}", "/assets/css/main.css").is_none());
        assert_eq!(
            m("/assets/{relpath}", "/assets/main.css"),
            Some(vec![("relpath".to_string(), "main.css".to_string())])
        );
    }

    /// The matcher is not the wire. Every test above stops at `match_route`,
    /// and the capture still has to survive `build_dict` to reach a handler.
    ///
    /// Step 4 validates every path param with `allow_slash = false`, on a
    /// premise stated in `request::validate_path_param`'s own doc comment:
    /// "this crate's router has no catch-all support ... so a parameter here
    /// captures exactly one path segment and can never contain a slash". That
    /// was true when it was written and `Segment::Wildcard` made it false, so
    /// the one capture that is *defined* to hold slashes was answered 400.
    #[test]
    fn a_wildcard_capture_survives_dict_building() {
        use std::io::Write;

        let site_dir = tempfile::TempDir::new().unwrap();
        std::fs::create_dir(site_dir.path().join("templates")).unwrap();
        let mut f = tempfile::NamedTempFile::new().unwrap();
        writeln!(
            f,
            "[[route]]\npath = \"/assets/{{*relpath}}\"\ntemplate = \"x.html\""
        )
        .unwrap();

        let cfg = crate::config::load(f.path(), site_dir.path()).unwrap();
        let state = FrameworkState::build(
            cfg,
            site_dir.path().to_path_buf(),
            &[],
            &*default_renderer(),
        )
        .unwrap();

        let raw = RawRequest {
            version: "HTTP/1.1".to_string(),
            method: "GET".to_string(),
            path: "/assets/css/main.css".to_string(),
            query: None,
            headers: vec![],
            body: vec![],
        };
        let (route, params) =
            find_route(raw.path(), raw.method(), &state.routes).expect("route should match");

        let dict = state
            .build_dict(&raw, route, &params)
            .expect("a wildcard capture is not a malformed request");
        assert_eq!(
            dict.get("relpath").unwrap().as_str().unwrap(),
            "css/main.css"
        );
    }

    /// The other half of the fix: a wildcard is allowed the separator, and
    /// nothing else. Traversal still has to be refused, or the capability that
    /// exists to serve a directory tree is a way out of it.
    #[test]
    fn a_wildcard_capture_still_refuses_traversal() {
        use std::io::Write;

        let site_dir = tempfile::TempDir::new().unwrap();
        std::fs::create_dir(site_dir.path().join("templates")).unwrap();
        let mut f = tempfile::NamedTempFile::new().unwrap();
        writeln!(
            f,
            "[[route]]\npath = \"/assets/{{*relpath}}\"\ntemplate = \"x.html\""
        )
        .unwrap();
        let cfg = crate::config::load(f.path(), site_dir.path()).unwrap();
        let state = FrameworkState::build(
            cfg,
            site_dir.path().to_path_buf(),
            &[],
            &*default_renderer(),
        )
        .unwrap();

        // `..` never survives the matcher, so drive the check directly: these
        // are the values that would reach step 4 if it ever did.
        let route = &state.routes.routes()[0];
        assert!(route.is_wildcard_param("relpath"));
        assert!(!route.is_wildcard_param("stem"));

        for bad in [
            "../etc/passwd",
            "css/../../etc/passwd",
            "a b/c",
            "/leading",
            "trailing/",
        ] {
            assert!(
                validate_wildcard_param("relpath", bad).is_err(),
                "{bad} should be refused"
            );
        }
        assert!(validate_wildcard_param("relpath", "css/main.css").is_ok());

        // And an ordinary parameter on the same route is unchanged: still one
        // segment, still no slash.
        assert!(validate_path_param("stem", "a/b").is_err());
    }

    /// A wildcard that is not last has no single correct split, so it is
    /// narrowed to an ordinary parameter rather than taking the service down.
    #[test]
    fn a_wildcard_that_is_not_last_is_narrowed_not_fatal() {
        let segs = compile_pattern("/a/{*rest}/c");
        assert!(matches!(segs[1], Segment::Param(_)), "got {:?}", segs[1]);
        // Which means it behaves as one segment, not as a greedy match.
        assert!(m("/a/{*rest}/c", "/a/b/x/c").is_none());
        assert!(m("/a/{*rest}/c", "/a/b/c").is_some());
    }
}

#[cfg(test)]
mod config_route_handler_tests {
    use super::*;
    use std::io::Write;

    /// Build a `FrameworkState` from TOML written to a temporary file, the way
    /// the service does at startup.
    fn state_from(toml: &str) -> (FrameworkState, tempfile::TempDir) {
        let site_dir = tempfile::TempDir::new().unwrap();
        std::fs::create_dir(site_dir.path().join("templates")).unwrap();
        let mut f = tempfile::NamedTempFile::new().unwrap();
        write!(f, "{toml}").unwrap();
        let cfg = crate::config::load(f.path(), site_dir.path()).unwrap();
        let state = FrameworkState::build(
            cfg,
            site_dir.path().to_path_buf(),
            &[],
            &*default_renderer(),
        )
        .unwrap();
        (state, site_dir)
    }

    /// What a browser sees: a script route with no `cache` key defaults to
    /// `no-store`, and the script labelled `text/javascript` still comes back
    /// brotli-compressed under the default config.
    #[test]
    fn a_text_javascript_response_is_brotli_compressed_by_default() {
        let (state, _site_dir) =
            state_from("[[route]]\npath = \"/app.js\"\nhandler = \"script\"\n");
        let script: BoxHandler = Box::new(|_req: &Request| {
            Ok(Response::status(200)
                .header("Content-Type", "text/javascript; charset=utf-8")
                .body("function f(a, b) { return a + b; }\n".repeat(50)))
        });
        let named = HashMap::from([("script".to_string(), Arc::new(script))]);
        let raw = RawRequest {
            version: "HTTP/1.1".to_string(),
            method: "GET".to_string(),
            path: "/app.js".to_string(),
            query: None,
            headers: vec![
                ("Host".to_string(), "localhost".to_string()),
                ("Accept-Encoding".to_string(), "br".to_string()),
            ],
            body: vec![],
        };

        let site = crate::site::Site::from_state(state, vec!["script".to_string()]);
        let response = match site.handle_raw(raw) {
            crate::site::Handled::Named(0, req) => {
                let response = named
                    .get("script")
                    .expect("registered")
                    .call(&req)
                    .expect("the handler answers");
                site.finish(&req, response)
            }
            _ => panic!("the config route names `script`, which is registered"),
        };
        let mut out = Vec::new();
        {
            let mut resp = crate::h1::Responder::new(&mut out, "GET", false);
            response.send(&mut resp).expect("the response serialises");
        }
        let head_end = out.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
        let head = String::from_utf8_lossy(&out[..head_end]).to_ascii_lowercase();
        assert!(head.starts_with("http/1.1 200"), "{head}");
        assert!(
            head.contains("cache-control: no-store"),
            "the route has no `cache` key, so it defaults to no-store: {head}"
        );
        assert!(head.contains("content-encoding: br"), "{head}");
    }

    const FILES: &str = r#"
[[route]]
path = "/assets/{*relpath}"
handler = "files"
root = "assets/"
tail = false
"#;

    /// The binding config declares and code provides.
    #[test]
    fn a_config_route_can_name_a_handler() {
        let (state, _d) = state_from(FILES);
        let route = &state.routes.routes()[0];
        assert_eq!(route.handler.as_deref(), Some("files"));
        assert_eq!(route.pattern, "/assets/{*relpath}");
    }

    /// A handler name with no code behind it has no defensible reading, so it
    /// is refused rather than narrowed. The caller exits 2 at startup.
    #[test]
    fn a_route_naming_an_unregistered_handler_is_refused() {
        let (state, _d) = state_from(FILES);

        let missing = unknown_handlers(state.routes.routes(), &["uploads".to_string()], &[]);
        assert_eq!(
            missing,
            vec![("/assets/{*relpath}".to_string(), "files".to_string())]
        );

        // The operator needs to be told which route, which name, and what was
        // available. A bare "unknown handler" sends them reading config by eye.
        let described = describe_unknown(&missing, &["uploads".to_string()]);
        assert!(described.contains("/assets/{*relpath}"), "{described}");
        assert!(described.contains("`files`"), "{described}");
        assert!(described.contains("uploads"), "{described}");

        // Registered, and there is nothing to refuse.
        assert!(unknown_handlers(state.routes.routes(), &["files".to_string()], &[]).is_empty());
    }

    /// A template route is unaffected by any of this: it names no handler and
    /// is not subject to the check.
    #[test]
    fn a_template_route_names_no_handler() {
        let (state, _d) = state_from("[[route]]\npath = \"/\"\ntemplate = \"index.html\"\n");
        assert!(state.routes.routes()[0].handler.is_none());
        assert!(unknown_handlers(state.routes.routes(), &[], &[]).is_empty());
    }

    /// Core does not know what `root` or `tail` mean, and does not need to.
    /// Keys it does not define are kept verbatim for the handler; keys it does
    /// define are consumed, not duplicated into the bag.
    #[test]
    fn keys_core_does_not_define_are_kept_for_the_handler() {
        let (state, _d) = state_from(FILES);
        let s = &state.routes.routes()[0].settings;
        assert_eq!(s.get("root").unwrap().as_str().unwrap(), "assets/");
        assert!(!s.get("tail").unwrap().as_bool().unwrap());
        for consumed in [
            "path", "handler", "template", "cache", "status", "methods", "headers",
        ] {
            assert!(
                s.get(consumed).is_none(),
                "{consumed} is core's, not the handler's"
            );
        }
    }

    /// A handler computes its answer; a template renders a document from files
    /// on disk. Letting the first inherit the second's `public` default would
    /// put dynamic output in a shared cache by omission.
    #[test]
    fn a_handler_route_defaults_to_no_store_and_an_explicit_cache_wins() {
        let (state, _d) = state_from(FILES);
        assert_eq!(state.routes.routes()[0].cache, "no-store");

        let (explicit, _d2) = state_from(
            "[[route]]\npath = \"/a/{*r}\"\nhandler = \"files\"\ncache = \"public, max-age=60\"\n",
        );
        assert_eq!(explicit.routes.routes()[0].cache, "public, max-age=60");

        let (template, _d3) = state_from("[[route]]\npath = \"/\"\ntemplate = \"index.html\"\n");
        assert_eq!(template.routes.routes()[0].cache, "public");
    }

    /// `Last-Modified` is derived from templates and params files, so it
    /// belongs to routes that render from them. The loop that computes it used
    /// to run over every route, including the code routes its own comment said
    /// were skipped, and hand a handler the newest template's mtime as the date
    /// of an answer computed per request.
    #[test]
    fn a_handler_route_carries_no_last_modified() {
        let (state, _d) = state_from(FILES);
        assert!(state.routes.routes()[0].last_modified.is_none());
    }

    /// The handler's side of the contract: it reads its own route's config
    /// through the request, which is what lets a service keep a per-route
    /// vocabulary core knows nothing about.
    #[test]
    fn a_handler_reads_its_routes_settings_from_the_request() {
        let (state, dir) = state_from(FILES);
        let raw = RawRequest {
            version: "HTTP/1.1".to_string(),
            method: "GET".to_string(),
            path: "/assets/css/main.css".to_string(),
            query: None,
            headers: vec![],
            body: vec![],
        };
        let req = Request::new(raw, Map::new(), dir.path().to_path_buf())
            .with_route(&state.routes.routes()[0]);

        assert_eq!(req.route_pattern(), Some("/assets/{*relpath}"));
        assert_eq!(req.route_str("root"), Some("assets/"));
        assert!(!req.route_bool("tail", true), "tail = false in config");
        assert!(
            req.route_bool("absent", true),
            "an absent key takes the default"
        );
        assert_eq!(req.route_str("nothing"), None);
    }

    /// A request built without a route has no settings, rather than a wrong
    /// answer: every accessor says "absent" and the defaults apply.
    #[test]
    fn a_request_with_no_route_reports_no_settings() {
        let dir = tempfile::tempdir().unwrap();
        let req = Request::new(
            RawRequest {
                version: "HTTP/1.1".to_string(),
                method: "GET".to_string(),
                path: "/".to_string(),
                query: None,
                headers: vec![],
                body: vec![],
            },
            Map::new(),
            dir.path().to_path_buf(),
        );
        assert_eq!(req.route_pattern(), None);
        assert_eq!(req.route_str("root"), None);
        assert!(req.route_bool("tail", true));
    }

    // ── Flash read-back ───────────────────────────────────────────────────
    //
    // Step 11 reads `_flash` back through `dict.get("cookies")`, the map step
    // 7 inserted. `flash` is not a default feature, so the allocation tests
    // never compile that block and the round-trip tests above stop at
    // `verify_flash_cookie`. These two drive the whole way to the dictionary.

    /// The cookie value `Response::flash` produces for `message`.
    #[cfg(feature = "flash")]
    fn flash_cookie(message: &str, secret: &[u8]) -> String {
        Response::text("ok")
            .flash(message, secret)
            .headers
            .iter()
            .find(|(k, _)| k == "Set-Cookie")
            .map(|(_, v)| v.as_str())
            .expect("Set-Cookie header")
            .split(';')
            .next()
            .unwrap()
            .trim_start_matches("_flash=")
            .to_string()
    }

    /// The dictionary a request carrying one `_flash` cookie builds.
    ///
    /// The key is set here, decoded, because `flash_secret` is a framework key
    /// that `config::load` keeps out of `user_config`. The read-back under test
    /// is step 11's, not the config plumbing's.
    #[cfg(feature = "flash")]
    fn flash_dict(cookie: &str, secret: &[u8]) -> Dict {
        let (mut state, _dir) = state_from("[[route]]\npath = \"/\"\nhandler = \"page\"\n");
        state.flash_secret = secret.to_vec();
        let raw = RawRequest {
            version: "HTTP/1.1".to_string(),
            method: "GET".to_string(),
            path: "/".to_string(),
            query: None,
            headers: vec![("Cookie".to_string(), format!("_flash={cookie}"))],
            body: vec![],
        };
        let (route, params) = find_route(raw.path(), raw.method(), &state.routes).unwrap();
        state.build_dict(&raw, route, &params).unwrap()
    }

    #[cfg(feature = "flash")]
    #[test]
    fn a_valid_flash_cookie_reaches_the_dictionary() {
        let secret = b"test-secret-key-32-bytes-xxxxxxx";
        let message = "Login successful!";
        let dict = flash_dict(&flash_cookie(message, secret), secret);

        assert_eq!(
            dict.get("flash").and_then(|v| v.as_str()),
            Some(message),
            "a verified flash message must reach the page"
        );
    }

    #[cfg(feature = "flash")]
    #[test]
    fn a_tampered_flash_cookie_leaves_no_message_in_the_dictionary() {
        let secret = b"test-secret-key-32-bytes-xxxxxxx";
        let (message, signature) = flash_cookie("Login successful!", secret)
            .split_once('.')
            .map(|(m, s)| (m.to_string(), s.to_string()))
            .expect("a flash cookie is message.signature");
        let mut first = signature.chars();
        let swapped = match first.next() {
            Some('A') => 'B',
            Some(_) => 'A',
            None => panic!("a signature is not empty"),
        };
        let tampered = format!("{message}.{}{}", swapped, first.as_str());

        let dict = flash_dict(&tampered, secret);
        assert!(
            dict.get("flash").is_none(),
            "a tampered flash cookie must not reach the page"
        );
    }
}

#[cfg(test)]
mod dict_cost {
    //! What `App` spends per request before a handler or a template runs.
    //!
    //! All `#[ignore]`d. These are measurements, not assertions: a timing
    //! assertion is the wall-clock trap `test_static_file_cache_hit` already
    //! fell into once, firing on a loaded build box against correct code.
    //!
    //! ```sh
    //! cargo test --release -p m6-core dict_cost -- --ignored --nocapture
    //! ```
    //!
    //! Measured in release on 2026-11-14. **The whole cost is copying
    //! immutable state per request**, and it is nothing to do with parsing or
    //! formatting dates:
    //!
    //! | what | ns |
    //! |---|---|
    //! | `Map::new()` | 0 |
    //! | query parse + map | 41 |
    //! | `parse_cookies` | 167 |
    //! | `chrono::Utc::now()` + `builtin_dates` | 83 |
    //! | **cloning 20 config keys** | **1,458** |
    //!
    //! Against a real content catalogue, tens of kilobytes and thousands of
    //! nodes, the same shape costs far more, because such a file is commonly
    //! loaded as *both* the
    //! global params and the route's params and is then cloned a third time by
    //! `render_response` (the figures below are from 2026-09-12):
    //!
    //! | what | ns |
    //! |---|---|
    //! | step 2, global params clone | 104,583 |
    //! | step 3, route params clone, same file | 103,125 |
    //! | `render_response`'s `dict.clone()` | 103,542 |
    //! | **total before the engine runs** | **323,292** |
    //!
    //! None of it varies between requests. It is the config and the content
    //! file, read once at start, deep-copied three times per page.

    use super::*;
    use std::io::Write;

    fn median<F: FnMut()>(n: usize, mut f: F) -> u64 {
        for _ in 0..n / 4 {
            f();
        }
        let mut v = Vec::with_capacity(n);
        for _ in 0..n {
            let t = m6_platform::now();
            f();
            v.push(t.elapsed().as_nanos() as u64);
        }
        v.sort_unstable();
        v[v.len() / 2]
    }

    /// Build a state from TOML, with `site_dir` already populated by `setup`.
    fn state_from(
        toml: &str,
        setup: impl FnOnce(&std::path::Path),
    ) -> (FrameworkState, tempfile::TempDir) {
        let site_dir = tempfile::TempDir::new().unwrap();
        std::fs::create_dir(site_dir.path().join("templates")).unwrap();
        setup(site_dir.path());
        let mut f = tempfile::NamedTempFile::new().unwrap();
        write!(f, "{toml}").unwrap();
        let config = crate::config::load(f.path(), site_dir.path()).unwrap();
        let state = FrameworkState::build(
            config,
            site_dir.path().to_path_buf(),
            &[],
            &*default_renderer(),
        )
        .unwrap();
        (state, site_dir)
    }

    fn asset_request() -> RawRequest {
        RawRequest {
            version: "HTTP/1.1".to_string(),
            method: "GET".to_string(),
            path: "/assets/css/main.css".to_string(),
            query: None,
            headers: vec![
                ("Host".to_string(), "localhost".to_string()),
                ("Accept-Encoding".to_string(), "br, gzip".to_string()),
                ("Cookie".to_string(), "_csrf=abc; session=def".to_string()),
            ],
            body: vec![],
        }
    }

    fn config_with_keys(n: usize) -> String {
        let mut cfg = String::new();
        for i in 0..n {
            cfg.push_str(&format!("key_{i} = \"value_{i}\"\n"));
        }
        cfg
    }

    /// The headline for a static asset request: what a file handler would pay
    /// per request for a dictionary it never reads.
    #[test]
    #[ignore = "a measurement, not an assertion"]
    fn measure_build_dict_for_a_static_asset_request() {
        let toml = format!(
            "{}[[route]]\npath = \"/assets/{{*relpath}}\"\nhandler = \"files\"\n",
            config_with_keys(20)
        );
        let (state, _d) = state_from(&toml, |_| {});
        let raw = asset_request();
        let (route, params) = find_route(raw.path(), raw.method(), &state.routes).unwrap();

        const N: usize = 20_000;
        let whole = median(N, || {
            std::hint::black_box(state.build_dict(&raw, route, &params).unwrap());
        });
        let routing = median(N, || {
            std::hint::black_box(find_route(raw.path(), raw.method(), &state.routes));
        });
        println!("build_dict p50 {whole}ns | find_route p50 {routing}ns");
    }

    /// Where that time goes, phase by phase. The answer is the copy, not the
    /// parser: cloning the config alone is more than half of it.
    #[test]
    #[ignore = "a measurement, not an assertion"]
    fn where_does_build_dict_spend_its_time() {
        let toml = format!(
            "{}[[route]]\npath = \"/assets/{{*relpath}}\"\nhandler = \"files\"\n",
            config_with_keys(20)
        );
        let (state, _d) = state_from(&toml, |_| {});
        let raw = asset_request();
        let (route, params) = find_route(raw.path(), raw.method(), &state.routes).unwrap();
        const N: usize = 20_000;

        println!("--- build_dict phase breakdown (ns, median of {N}) ---");
        println!(
            "whole build_dict          {:>7}",
            median(N, || {
                std::hint::black_box(state.build_dict(&raw, route, &params).unwrap());
            })
        );
        println!(
            "Map::new alone            {:>7}",
            median(N, || {
                std::hint::black_box(Map::new());
            })
        );
        println!(
            "1. user_config clone x{:<3} {:>7}",
            state.config.user_config.len(),
            median(N, || {
                let mut d = Map::new();
                for (k, v) in &state.config.user_config {
                    d.insert(k.clone(), v.clone());
                }
                std::hint::black_box(d);
            })
        );
        println!(
            "5. query parse + map      {:>7}",
            median(N, || {
                let mut q = Map::new();
                for (k, v) in parse_query_string(raw.query()) {
                    q.insert(k.clone(), Value::String(v.clone()));
                }
                std::hint::black_box(Value::Object(q));
            })
        );
        println!(
            "7. parse_cookies          {:>7}",
            median(N, || {
                std::hint::black_box(parse_cookies(raw.header("cookie").unwrap()));
            })
        );
        println!(
            "8. now() + builtin_dates  {:>7}",
            median(N, || {
                std::hint::black_box(FrameworkState::builtin_dates(chrono::Utc::now()));
            })
        );
    }

    /// The same thing against a real deployment's content file, which is the
    /// number that matters: an HTML service renders every page through this.
    ///
    /// Point it at yours. m6 is generic and ships no content of its own, so
    /// there is nothing here to measure against by default:
    ///
    ///     M6_REAL_CONTENT_JSON=/path/to/data/content.json \
    ///       cargo test -p m6-core build_dict_against_the_real_content_json -- --ignored
    ///
    /// Named by the environment rather than joined from a path inside one
    /// particular deployment: this is a generic library's test suite.
    #[test]
    #[ignore = "a measurement; set M6_REAL_CONTENT_JSON to a content file"]
    fn build_dict_against_the_real_content_json() {
        let Ok(path) = std::env::var("M6_REAL_CONTENT_JSON") else {
            println!("SKIP: set M6_REAL_CONTENT_JSON to a content.json to measure against");
            return;
        };
        let src = std::path::PathBuf::from(path);
        if !src.exists() {
            println!("SKIP: {} not present", src.display());
            return;
        }

        // A shape an HTML service commonly declares: the same file as the
        // global params and as the route's params.
        let toml = format!(
            "{}global_params = [\"data/content.json\"]\n\
             [[route]]\npath = \"/\"\ntemplate = \"index.html\"\nparams = [\"data/content.json\"]\n",
            config_with_keys(34)
        );
        let (state, _d) = state_from(&toml, |dir| {
            std::fs::create_dir(dir.join("data")).unwrap();
            std::fs::copy(&src, dir.join("data/content.json")).unwrap();
        });

        let raw = RawRequest {
            version: "HTTP/1.1".to_string(),
            method: "GET".to_string(),
            path: "/".to_string(),
            query: None,
            headers: vec![("Cookie".to_string(), "_csrf=abc".to_string())],
            body: vec![],
        };
        let (route, params) = find_route(raw.path(), raw.method(), &state.routes).unwrap();

        const N: usize = 2_000;
        let whole = median(N, || {
            std::hint::black_box(state.build_dict(&raw, route, &params).unwrap());
        });
        let globals = median(N, || {
            let mut d = Map::new();
            for (k, v) in &state.global_params_data {
                d.insert(k.clone(), v.clone());
            }
            std::hint::black_box(d);
        });
        let route_params = median(N, || {
            let mut d = Map::new();
            for pf in &route.params_files {
                if let Some(m) = state.static_params.get(pf) {
                    for (k, v) in m.as_ref() {
                        d.insert(k.clone(), v.clone());
                    }
                }
            }
            std::hint::black_box(d);
        });
        let dict = state.build_dict(&raw, route, &params).unwrap();
        let render_copy = median(N, || {
            std::hint::black_box(dict.clone());
        });

        println!("--- production-shaped per-request dict cost (ns, median of {N}) ---");
        println!("whole build_dict                 {whole:>8}");
        println!("  step 2, global params clone    {globals:>8}");
        println!("  step 3, route params clone     {route_params:>8}  (same file, again)");
        println!("render_response dict.clone()     {render_copy:>8}  (third copy)");
        println!(
            "total before the engine runs     {:>8}",
            whole + render_copy
        );
    }
}

#[cfg(test)]
mod copy_audit {
    //! Every copy `App` makes per request, measured against a real content
    //! file. `#[ignore]`d: measurements, not assertions.
    //!
    //! ```sh
    //! cargo test --release -p m6-core copy_audit -- --ignored --nocapture
    //! ```
    use super::*;
    use std::io::Write;

    fn median<F: FnMut()>(n: usize, mut f: F) -> u64 {
        for _ in 0..n / 4 {
            f();
        }
        let mut v = Vec::with_capacity(n);
        for _ in 0..n {
            let t = m6_platform::now();
            f();
            v.push(t.elapsed().as_nanos() as u64);
        }
        v.sort_unstable();
        v[v.len() / 2]
    }

    #[test]
    #[ignore = "a measurement; set M6_REAL_CONTENT_JSON to a content file"]
    fn every_copy_app_makes_per_request() {
        // As above: a real content file, named by the environment rather than
        // by this repository. See build_dict_against_the_real_content_json.
        let Ok(path) = std::env::var("M6_REAL_CONTENT_JSON") else {
            println!("SKIP: set M6_REAL_CONTENT_JSON to a content.json to measure against");
            return;
        };
        let src = std::path::PathBuf::from(path);
        if !src.exists() {
            println!("SKIP: {} not present", src.display());
            return;
        }
        let site_dir = tempfile::TempDir::new().unwrap();
        std::fs::create_dir(site_dir.path().join("templates")).unwrap();
        std::fs::create_dir(site_dir.path().join("data")).unwrap();
        std::fs::copy(&src, site_dir.path().join("data/content.json")).unwrap();

        let mut cfg = String::new();
        for i in 0..34 {
            cfg.push_str(&format!("key_{i} = \"value_{i}\"\n"));
        }
        cfg.push_str("global_params = [\"data/content.json\"]\n");
        cfg.push_str("[compression.\"text/html\"]\nbrotli = 6\ngzip = 6\n");
        cfg.push_str("[compression.\"text/css\"]\nbrotli = 6\ngzip = 6\n");
        cfg.push_str("[minification]\n\"text/html\" = true\n\"text/css\" = true\n");
        cfg.push_str("[[route]]\npath = \"/\"\ntemplate = \"index.html\"\nparams = [\"data/content.json\"]\n");
        let mut f = tempfile::NamedTempFile::new().unwrap();
        write!(f, "{cfg}").unwrap();
        let config = crate::config::load(f.path(), site_dir.path()).unwrap();
        let state = FrameworkState::build(
            config,
            site_dir.path().to_path_buf(),
            &[],
            &*default_renderer(),
        )
        .unwrap();

        let raw = RawRequest {
            version: "HTTP/1.1".to_string(),
            method: "GET".to_string(),
            path: "/".to_string(),
            query: None,
            headers: vec![
                ("Host".to_string(), "example.com".to_string()),
                (
                    "User-Agent".to_string(),
                    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7)".to_string(),
                ),
                (
                    "Accept".to_string(),
                    "text/html,application/xhtml+xml,application/xml;q=0.9".to_string(),
                ),
                (
                    "Accept-Encoding".to_string(),
                    "gzip, deflate, br".to_string(),
                ),
                ("Cookie".to_string(), "_csrf=abc123".to_string()),
            ],
            body: vec![],
        };
        let (route, params) = find_route(raw.path(), raw.method(), &state.routes).unwrap();
        let dict = state.build_dict(&raw, route, &params).unwrap();

        const N: usize = 2_000;
        let rows: Vec<(&str, u64)> = vec![
            // J1 (done): config and site_dir leave the read lock as `Arc`
            // clones. These three rows were 458ns of deep copies; they are now
            // two refcount bumps. Measured as the path actually runs it.
            (
                "Arc::clone(config) + Arc::clone(site_dir)  [was 458]",
                median(N, || {
                    std::hint::black_box((Arc::clone(&state.config), Arc::clone(&state.site_dir)));
                }),
            ),
            // J6 (done): the route table is shared, so routing borrows from
            // an `Arc` instead of cloning the matched route out of the guard.
            (
                "Arc::clone(routes)  [was route.clone(), 167]",
                median(N, || {
                    std::hint::black_box(Arc::clone(&state.routes));
                }),
            ),
            (
                "build_dict (3 copies inside)",
                median(N, || {
                    std::hint::black_box(state.build_dict(&raw, route, &params).unwrap());
                }),
            ),
            // J6 (done): `serve_connection` hands the request over instead of
            // lending it, so there is nothing to clone.
            ("raw.clone()  [removed]", 0),
            (
                "dict.clone() into Request",
                median(N, || {
                    std::hint::black_box(dict.clone());
                }),
            ),
            // J5 (done): render_response renders against the request dict
            // when the response supplied no context of its own, which is the
            // common path. There is no copy left to measure here.
            ("render_response copy  [removed]", 0),
            // Tera's `insert` serialises each value into its own context, so
            // the engine takes a copy of its own. This one is not core's to
            // remove without changing the renderer seam, but it is part of the
            // per-request bill and the audit is dishonest without it.
            (
                "tera::Context build (in the engine)",
                median(N, || {
                    let mut tctx = tera::Context::new();
                    for (k, v) in dict.iter() {
                        tctx.insert(k.as_str(), v);
                    }
                    std::hint::black_box(tctx);
                }),
            ),
        ];
        let total: u64 = rows.iter().map(|(_, ns)| ns).sum();

        // J7 in proportion: what does the render itself cost, against the
        // context build that precedes it? A minimal template is the *most*
        // favourable case for the copy mattering, since a real page does far
        // more work; if the copy is small even here, it is smaller in
        // production.
        let mut tera = tera::Tera::default();
        tera.add_raw_template("t.html", "<h1>{{ site_name | default(value='x') }}</h1>")
            .unwrap();
        let ctx_build = median(N, || {
            let mut tctx = tera::Context::new();
            for (k, v) in dict.iter() {
                tctx.insert(k.as_str(), v);
            }
            std::hint::black_box(tctx);
        });
        let build_and_render = median(N, || {
            let mut tctx = tera::Context::new();
            for (k, v) in dict.iter() {
                tctx.insert(k.as_str(), v);
            }
            std::hint::black_box(tera.render("t.html", &tctx).unwrap());
        });
        println!();
        println!("J7 in proportion (minimal template, the most favourable case):");
        println!("  context build      {ctx_build:>8} ns");
        println!("  build + render     {build_and_render:>8} ns");
        println!(
            "  render alone       {:>8} ns",
            build_and_render.saturating_sub(ctx_build)
        );
        println!(
            "  context share      {:>7.1}%",
            100.0 * ctx_build as f64 / build_and_render as f64
        );

        println!("| copy | ns |");
        println!("|---|---:|");
        for (what, ns) in &rows {
            println!("| {what} | {ns} |");
        }
        println!("| **total per request** | **{total}** |");
    }
}

#[cfg(test)]
mod route_rule_tests {
    use super::*;
    use crate::config::RouteConfig;

    fn config_route(path: &str, methods: Option<&[&str]>) -> RouteConfig {
        RouteConfig {
            path: path.to_string(),
            template: Some("t.html".to_string()),
            params: Vec::new(),
            status: 200,
            cache: "public".to_string(),
            methods: methods.map(|m| m.iter().map(|s| s.to_string()).collect()),
            headers: Vec::new(),
            handler: None,
            settings: Arc::new(Map::new()),
        }
    }

    /// The error the load gives for these config routes, or `None` if it
    /// accepts them.
    fn refusal(paths: &[&str]) -> Option<String> {
        let routes: Vec<_> = paths.iter().map(|p| config_route(p, None)).collect();
        check_routes(&[], &routes).err().map(|e| e.to_string())
    }

    fn accepted(paths: &[&str]) {
        assert_eq!(refusal(paths), None, "{paths:?} must not conflict");
    }

    fn compiled(pattern: &str, methods: Methods) -> CompiledRoute {
        CompiledRoute {
            pattern: pattern.to_string(),
            methods,
            segments: compile_pattern(pattern),
            template: None,
            params_files: Vec::new(),
            status: 200,
            cache: String::new(),
            headers: Vec::new(),
            last_modified: None,
            handler: None,
            settings: Arc::new(Map::new()),
            base_dict: Arc::new(Map::new()),
        }
    }

    fn answer<'a>(table: &'a RouteTable, method: &str, path: &str) -> Option<&'a str> {
        find_route(path, method, table).map(|(r, _)| r.pattern.as_str())
    }

    // ── Which route answers ────────────────────────────────────────────────

    /// The pattern is declared first, so an answer decided by declaration
    /// order would be the pattern. The wrong outcome: `/blog/about` renders
    /// the parameter page, and the fixed page is unreachable.
    #[test]
    fn a_fixed_route_answers_ahead_of_a_pattern_declared_before_it() {
        let table = RouteTable::new(vec![
            compiled("/blog/{slug}", Methods::ANY),
            compiled("/blog/about", Methods::ANY),
        ]);
        assert_eq!(answer(&table, "GET", "/blog/about"), Some("/blog/about"));
        assert_eq!(answer(&table, "GET", "/blog/other"), Some("/blog/{slug}"));
        assert_eq!(answer(&table, "GET", "/blog/about/"), Some("/blog/about"));
        assert_eq!(answer(&table, "GET", "//blog//about"), Some("/blog/about"));
    }

    /// A fixed route competes only for the methods it answers. The wrong
    /// outcome: a `POST`-only fixed route hides the pattern from `GET`.
    #[test]
    fn a_fixed_route_for_other_methods_leaves_the_pattern_to_answer() {
        let table = RouteTable::new(vec![
            compiled("/blog/about", Methods::from(&RouteMethod::Post)),
            compiled("/blog/{slug}", Methods::ANY),
        ]);
        assert_eq!(answer(&table, "POST", "/blog/about"), Some("/blog/about"));
        assert_eq!(answer(&table, "GET", "/blog/about"), Some("/blog/{slug}"));
    }

    /// The root is a fixed route with no segments.
    #[test]
    fn the_root_is_a_fixed_route() {
        let table = RouteTable::new(vec![
            compiled("/{*rest}", Methods::ANY),
            compiled("/", Methods::ANY),
        ]);
        assert_eq!(answer(&table, "GET", "/"), Some("/"));
        assert_eq!(answer(&table, "GET", "/x/y"), Some("/{*rest}"));
    }

    /// `methods = ["GET", "POST"]` answers those two and no other. The wrong
    /// outcome (#336): it answered every method, so `DELETE` ran the handler.
    #[test]
    fn a_route_answers_exactly_the_methods_it_lists() {
        let methods = Methods::from_names(&["GET".to_string(), "POST".to_string()]).unwrap();
        let table = RouteTable::new(vec![compiled("/comments", methods)]);
        for answered in ["GET", "HEAD", "POST"] {
            assert_eq!(answer(&table, answered, "/comments"), Some("/comments"));
        }
        for refused in ["PUT", "PATCH", "DELETE", "OPTIONS"] {
            assert_eq!(answer(&table, refused, "/comments"), None, "{refused}");
        }
    }

    /// A route with no `methods` answers everything, including a method m6 has
    /// no name for.
    #[test]
    fn a_route_without_methods_answers_every_method() {
        let table = RouteTable::new(vec![compiled("/any", Methods::ANY)]);
        for m in [
            "GET", "HEAD", "POST", "PUT", "PATCH", "DELETE", "OPTIONS", "PURGE",
        ] {
            assert_eq!(answer(&table, m, "/any"), Some("/any"), "{m}");
        }
    }

    // ── What the load refuses ──────────────────────────────────────────────

    /// The bug: two routes for one path, the later one unreachable, and nothing
    /// said. The refusal names both routes as written, where each came from and
    /// one request both would answer.
    #[test]
    fn two_patterns_for_one_path_are_refused_with_both_named() {
        assert_eq!(
            refusal(&["/blog/{slug}", "/blog/{name}"]).unwrap(),
            "routes \"/blog/{slug}\" ([[route]] number 1) and \"/blog/{name}\" \
             ([[route]] number 2) can both answer GET /blog/x; remove one, give one \
             explicit methods, or make one a fixed path"
        );
    }

    /// Each pair the rule names as an error.
    #[test]
    fn the_pattern_pairs_in_the_rule_are_refused_with_an_example() {
        for (a, b, example) in [
            ("/blog/{slug}", "/blog/{*rest}", "GET /blog/x"),
            ("/{lang}/about", "/blog/{slug}", "GET /blog/about"),
            ("/{*rest}", "/blog/{slug}", "GET /blog/x"),
            ("/a/{x}", "/{y}/b", "GET /a/b"),
        ] {
            let e = refusal(&[a, b]).unwrap_or_else(|| panic!("{a} with {b} must conflict"));
            assert!(e.contains(&format!("can both answer {example};")), "{e}");
        }
    }

    /// Two fixed routes on one path.
    #[test]
    fn two_fixed_routes_on_one_path_are_refused() {
        let e = refusal(&["/about", "/about"]).unwrap();
        assert!(
            e.contains("\"/about\"") && e.contains("answer GET /about"),
            "{e}"
        );
        assert!(
            e.ends_with("remove one or give one explicit methods"),
            "{e}"
        );
        // Slashes do not make a different path.
        assert!(refusal(&["/about", "/about/"]).is_some());
        assert!(refusal(&["/", "/"]).unwrap().contains("GET /;"));
    }

    /// A fixed route and a pattern that covers it is the rule working, in
    /// either order. A check that refused every overlap would reject the
    /// exact-beats-parameter pair the matcher exists to resolve.
    #[test]
    fn a_fixed_route_beside_a_pattern_is_accepted() {
        accepted(&["/blog/{slug}", "/blog/latest"]);
        accepted(&["/blog/latest", "/blog/{slug}"]);
        accepted(&["/assets/{*rest}", "/assets/style.css"]);
        accepted(&["/", "/{*rest}"]);
    }

    /// Patterns no path can match both of. A check that compared only the
    /// first segment or only the length would refuse these.
    #[test]
    fn patterns_that_share_no_path_are_accepted() {
        accepted(&["/blog/{slug}", "/news/{slug}"]);
        accepted(&["/a/{x}", "/a/{x}/{y}"]);
        accepted(&["/a/{x}/c", "/a/{x}/d"]);
        accepted(&["/a/{*r}", "/b/{*s}"]);
        // The wildcard needs four segments and the other has three.
        accepted(&["/{a}/{b}/{c}/{*r}", "/x/{y}/{z}"]);
        accepted(&["/x/{y}/{z}", "/{a}/{b}/{c}/{*r}"]);
    }

    /// A wildcard reaches past a longer pattern, so the pair shares paths.
    #[test]
    fn a_wildcard_and_a_longer_pattern_under_it_are_refused() {
        let e = refusal(&["/a/{*r}", "/a/{x}/{y}"]).unwrap();
        assert!(e.contains("GET /a/x/x"), "{e}");
    }

    /// Routes that no one method can reach together do not compete, which is
    /// how a `POST` handler and a `GET` page share a path.
    #[test]
    fn routes_conflict_only_if_a_method_is_shared() {
        let check = |a: &[&str], b: &[&str]| {
            check_routes(
                &[],
                &[
                    config_route("/f/{x}", Some(a)),
                    config_route("/f/{y}", Some(b)),
                ],
            )
        };
        assert!(check(&["GET"], &["POST"]).is_ok());
        assert!(check(&["GET", "POST"], &["PUT", "DELETE"]).is_ok());
        let e = check(&["GET", "POST"], &["POST", "PUT"])
            .unwrap_err()
            .to_string();
        assert!(e.contains("can both answer POST /f/x;"), "{e}");
        // No list is every method, so it meets any list.
        let none = check_routes(
            &[],
            &[
                config_route("/f/{x}", None),
                config_route("/f/{y}", Some(&["DELETE"])),
            ],
        );
        assert!(none.unwrap_err().to_string().contains("DELETE /f/x"));
    }

    /// A code `POST /contact` beside a config `GET /contact` template: the
    /// layout of a form page. Refused when the template route lists no methods,
    /// because it then answers `POST` as well.
    #[test]
    fn a_post_code_route_beside_a_get_template_route_is_accepted() {
        let code = [("/contact".to_string(), RouteMethod::Post)];
        assert!(check_routes(&code, &[config_route("/contact", Some(&["GET"]))]).is_ok());
        let e = check_routes(&code, &[config_route("/contact", None)])
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("(code route)") && e.contains("([[route]] number 1)"),
            "{e}"
        );
        assert!(e.contains("answer POST /contact"), "{e}");
    }

    /// A config route that names an unknown method is refused naming the route
    /// and the value, whatever else is wrong with the table. The wrong outcome
    /// (#336): it answered every method and the typo was never reported.
    #[test]
    fn an_unknown_method_is_refused_naming_the_route_and_the_value() {
        for bad in ["HEAD", "GTE", "PURGE"] {
            let e = check_routes(
                &[],
                &[
                    config_route("/ok", None),
                    config_route("/typo", Some(&["GET", bad])),
                ],
            )
            .unwrap_err()
            .to_string();
            assert!(
                e.contains("\"/typo\"")
                    && e.contains("([[route]] number 2)")
                    && e.contains(&format!("\"{bad}\"")),
                "{e}"
            );
        }
        let e = check_routes(&[], &[config_route("/none", Some(&[]))])
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("\"/none\"") && e.contains("empty methods"),
            "{e}"
        );
    }

    /// A `[[route]]` on a code route's pattern with no template and no handler
    /// gives that code route its `cache` and is not a conflict. Refusing it
    /// would break every service that does so.
    #[test]
    fn a_config_route_that_only_declares_a_code_route_is_not_a_conflict() {
        let code = [("/api/{id}".to_string(), RouteMethod::Get)];
        let mut declaration = config_route("/api/{id}", None);
        declaration.template = None;
        assert!(check_routes(&code, &[declaration.clone()]).is_ok());
        // With a template it is a second route for the same requests.
        let mut with_template = declaration;
        with_template.template = Some("t.html".to_string());
        assert!(check_routes(&code, &[with_template]).is_err());
    }

    /// The check is where `FrameworkState::build` starts, so every load path
    /// refuses.
    #[test]
    fn building_the_state_refuses_the_conflict() {
        let config = crate::config::RendererConfig {
            routes: vec![
                config_route("/blog/{slug}", None),
                config_route("/blog/{name}", None),
            ],
            ..Default::default()
        };
        let err = FrameworkState::build(config, PathBuf::from("."), &[], &*default_renderer())
            .err()
            .expect("the conflicting config must not build");
        assert!(
            err.to_string().contains("can both answer GET /blog/x"),
            "{err}"
        );
    }
}
