//! App builder, thread pool, lifecycle management.
#![allow(dead_code)]

use std::any::Any;
use std::cell::RefCell;
use std::collections::HashMap;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex, OnceLock,
};

use anyhow::Context;
use serde_json::{Map, Value};
use tracing::{error, info, warn};

use crate::error::{Error, Result};
use crate::request::{
    parse_auth_claims, parse_cookies, parse_form_body, parse_query_string, validate_path_param,
    validate_wildcard_param, RawRequest, Request,
};
use crate::response::{error_to_response, Response};
use crate::render::{RenderError, Renderer, RendererFactory};

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RouteMethod {
    Any,
    Get,
    Post,
    Put,
    Patch,
    Delete,
}

impl RouteMethod {
    fn matches(&self, method: &str) -> bool {
        match self {
            RouteMethod::Any => true,
            RouteMethod::Get => method == "GET" || method == "HEAD",
            RouteMethod::Post => method == "POST",
            RouteMethod::Put => method == "PUT",
            RouteMethod::Patch => method == "PATCH",
            RouteMethod::Delete => method == "DELETE",
        }
    }
}

/// A compiled route pattern.
#[derive(Debug, Clone)]
pub struct CompiledRoute {
    pub pattern: String,
    pub method: RouteMethod,
    pub segments: Vec<Segment>,
    pub template: Option<String>,
    pub params_files: Vec<String>,
    pub status: u16,
    pub cache: String,
    pub headers: Vec<(String, String)>,
    /// Specificity score: exact > parameterised, longer > shorter.
    pub specificity: i32,
    /// When this route's rendered output last changed, for `Last-Modified`.
    ///
    /// A rendered page has no file of its own to stat, which is why it carried
    /// no `Last-Modified` at all and every `If-Modified-Since` came back 200
    /// with the whole page. It does have inputs, though, and their mtimes are a
    /// truthful answer: the max over every template (partials are shared, so a
    /// change to `_banner.html` genuinely can change any page) and this route's
    /// own static params files.
    ///
    /// Computed once when the framework state is built, so it costs nothing per
    /// request, and recomputed on reload.
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
    /// written, including every one in production. The star is explicit.
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

/// How specific a route is. Higher wins when several match.
///
/// A wildcard scores *below* a parameter in the same position, so
/// `/assets/{name}` beats `/assets/{*rest}` for a one-segment tail and
/// `/assets/style.css` beats both. Without that, adding a catch-all to a config
/// would quietly capture traffic from the exact routes beside it.
pub fn route_specificity(segments: &[Segment]) -> i32 {
    let mut score = (segments.len() as i32) * 2;
    for seg in segments {
        match seg {
            Segment::Literal(_) => score += 1,
            Segment::Param(_) => {}
            Segment::Wildcard(_) => score -= 1,
        }
    }
    score
}

/// Stable string key for a `RouteMethod` — used to disambiguate handlers
/// registered on the same path with different HTTP methods.
fn route_method_key(m: &RouteMethod) -> &'static str {
    match m {
        RouteMethod::Any    => "ANY",
        RouteMethod::Get    => "GET",
        RouteMethod::Post   => "POST",
        RouteMethod::Put    => "PUT",
        RouteMethod::Patch  => "PATCH",
        RouteMethod::Delete => "DELETE",
    }
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

/// Find the best matching route for a request.
/// Path is split once here and shared across all route checks.
pub fn find_route<'a>(
    path: &str,
    method: &str,
    routes: &'a [CompiledRoute],
) -> Option<(&'a CompiledRoute, PathParams)> {
    // Split path once — reused for every route check.
    let path_segs: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let mut best: Option<(&CompiledRoute, PathParams)> = None;

    for route in routes {
        if !route.method.matches(method) {
            continue;
        }
        if let Some(params) = match_route(&path_segs, route) {
            match &best {
                Some((best_route, _)) if route.specificity <= best_route.specificity => {}
                _ => { best = Some((route, params)); }
            }
        }
    }
    best
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

struct ParamsCache {
    inner: Mutex<lru::LruCache<String, Arc<Map<String, Value>>>>,
}

impl ParamsCache {
    fn new(size: usize) -> Self {
        use std::num::NonZeroUsize;
        let cap = NonZeroUsize::new(size.max(1)).unwrap();
        Self { inner: Mutex::new(lru::LruCache::new(cap)) }
    }

    fn get(&self, key: &str) -> Option<Arc<Map<String, Value>>> {
        self.inner.lock().unwrap().get(key).cloned()
    }

    fn insert(&self, key: String, val: Arc<Map<String, Value>>) {
        self.inner.lock().unwrap().put(key, val);
    }

    fn clear(&self) {
        self.inner.lock().unwrap().clear();
    }
}

// ---------------------------------------------------------------------------
// Shared framework state
// ---------------------------------------------------------------------------

struct FrameworkState {
    config: crate::config::RendererConfig,
    site_dir: PathBuf,
    routes: Vec<CompiledRoute>,
    global_params_data: Map<String, Value>,
    static_params: HashMap<String, Arc<Map<String, Value>>>,
    params_cache: Arc<ParamsCache>,
    renderer: Box<dyn Renderer>,
    #[cfg(feature = "flash")]
    flash_secret: Vec<u8>,
}

impl FrameworkState {
    fn build(
        config: crate::config::RendererConfig,
        site_dir: PathBuf,
        code_routes: &[(String, RouteMethod)],
        renderer_factory: &dyn RendererFactory,
    ) -> anyhow::Result<Self> {
        // Compile routes from config.
        let mut routes = Vec::new();

        // Add code-registered routes (they come first — higher priority for same pattern).
        // Default cache to no-store: handler routes generate dynamic content.
        // The config's [[route]] cache setting for the same path is inherited below.
        for (pattern, method) in code_routes {
            let segs = compile_pattern(pattern);
            let spec = route_specificity(&segs);
            // Look for a matching config route to inherit its cache setting.
            let cache = config.routes.iter()
                .find(|r| r.path == *pattern)
                .map(|r| r.cache.clone())
                .unwrap_or_else(|| "no-store".to_string());
            routes.push(CompiledRoute {
                pattern: pattern.clone(),
                method: method.clone(),
                segments: segs,
                template: None,
                params_files: vec![],
                status: 200,
                cache,
                headers: vec![],
                specificity: spec,
                // A code route renders whatever its handler decides at request
                // time, so there is no input file whose mtime describes it.
                last_modified: None,
                // A code route is bound by pattern and method at the call
                // site, so it names no handler and carries no config of its
                // own. The config-declared form below is the one that does.
                handler: None,
                settings: Arc::new(Map::new()),
            });
        }

        // Add config routes.
        for rc in &config.routes {
            let segs = compile_pattern(&rc.path);
            let spec = route_specificity(&segs);
            let method = if let Some(methods) = &rc.methods {
                if methods.len() == 1 {
                    match methods[0].as_str() {
                        "GET" => RouteMethod::Get,
                        "POST" => RouteMethod::Post,
                        "PUT" => RouteMethod::Put,
                        "PATCH" => RouteMethod::Patch,
                        "DELETE" => RouteMethod::Delete,
                        _ => RouteMethod::Any,
                    }
                } else {
                    RouteMethod::Any
                }
            } else {
                RouteMethod::Any
            };
            routes.push(CompiledRoute {
                pattern: rc.path.clone(),
                method,
                segments: segs,
                template: rc.template.clone(),
                params_files: rc.params.clone(),
                status: rc.status,
                cache: rc.cache.clone(),
                headers: rc.headers.clone(),
                specificity: spec,
                last_modified: None, // filled in below, once templates_mtime is known
                handler: rc.handler.clone(),
                settings: Arc::clone(&rc.settings),
            });
        }

        // ── Per-route Last-Modified ─────────────────────────────────────────
        // Rendered HTML had no `Last-Modified`, so a client validating by date
        // got the entire page back every time -- 52 KB for `/`. m6-html emits an
        // ETag, so `If-None-Match` already worked; this closes the other half.
        //
        // Templates are pooled deliberately. They include each other
        // (`_head.html`, `_banner.html`, `_footer.html` are on every page), and
        // resolving the transitive include set per route would be a lot of
        // machinery to make one date slightly tighter. Taking the newest
        // template as every page's floor is honest -- a change to a shared
        // partial really can change any page -- and errs toward revalidating,
        // which costs a request rather than serving something stale.
        //
        // Params are per route, which is where the precision actually pays:
        // editing a publication should not make /capabilities look modified.
        let templates_mtime = newest_mtime_under(&site_dir.join("templates"));
        for route in &mut routes {
            // The date is derived from templates and params files, so it is
            // only meaningful for a route that renders from them.
            //
            // This loop used to run over every route, which quietly included
            // the code routes it was never meant to touch: their
            // `params_files` is empty, so nothing skipped them and they took
            // the newest template's mtime as their own. The comment at the
            // emit site already said "skipped when the route has no honest
            // date -- a code route", and that was true of the intent and not
            // of the code. A handler's answer is computed per request and is
            // not dated by a template it may never read.
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
        // here, which the caller turns into exit 2 at startup and into a kept
        // previous state on a reload.
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

        let params_cache = Arc::new(ParamsCache::new(config.params_cache.size));

        // Flash secret: decode from config if present. Presence is validated at server startup,
        // not here, so that tests can call FrameworkState::build without a flash_secret.
        #[cfg(feature = "flash")]
        let flash_secret = {
            use base64::Engine;
            let raw = config.user_config
                .get("flash_secret")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if raw.is_empty() {
                vec![] // validated at startup
            } else {
                base64::engine::general_purpose::STANDARD
                    .decode(raw)
                    .or_else(|_| {
                        base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(raw)
                    })
                    .context("decoding flash_secret (expected base64)")?
            }
        };

        Ok(Self {
            config,
            site_dir,
            routes,
            global_params_data,
            static_params,
            params_cache,
            renderer,
            #[cfg(feature = "flash")]
            flash_secret,
        })
    }

    /// Build the request dictionary for a matched route.
    fn build_dict(
        &self,
        raw: &RawRequest,
        route: &CompiledRoute,
        path_params: &PathParams,
    ) -> Result<Map<String, Value>> {
        let mut dict = Map::new();

        // 1. Config keys.
        for (k, v) in &self.config.user_config {
            dict.insert(k.clone(), v.clone());
        }

        // 2. Global params files.
        for (k, v) in &self.global_params_data {
            dict.insert(k.clone(), v.clone());
        }

        // 3. Route params files.
        for pf_template in &route.params_files {
            let arc_m: Option<Arc<Map<String, Value>>> = if pf_template.contains('{') {
                let pf_resolved = resolve_path_template(pf_template, path_params);
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
                }
            } else {
                self.static_params.get(pf_template).cloned()
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
        let mut query_map = Map::new();
        for (k, v) in parse_query_string(raw.query()) {
            query_map.insert(k.clone(), Value::String(v.clone()));
            dict.insert(k, Value::String(v));
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
                        dict.insert(k, Value::String(v));
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
        dict.insert("cookies".to_string(), Value::Object(cookies_map.clone()));

        // 8. Built-in keys (set after params files — cannot be overridden by them).
        let now = chrono::Utc::now();
        dict.insert("request_path".to_string(), Value::String(raw.path().to_string()));
        dict.insert("datetime".to_string(), Value::String(now.format("%Y-%m-%dT%H:%M:%SZ").to_string()));
        dict.insert("year".to_string(), Value::String(now.format("%Y").to_string()));

        // 9. Auth keys.
        if let Some(claims_hdr) = raw.header("x-auth-claims") {
            let auth = parse_auth_claims(claims_hdr);
            for (k, v) in auth {
                dict.insert(k, v);
            }
        }

        // 10. Error keys (from query params — already merged in step 5, but highlight here).

        // 11. Flash message: verify HMAC, add to dict if valid, clear cookie.
        #[cfg(feature = "flash")]
        if let Some(flash_cookie) = cookies_map
            .get("_flash")
            .and_then(|v| v.as_str())
        {
            if let Some(msg) = verify_flash_cookie(flash_cookie, &self.flash_secret) {
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

    /// Render a template response.
    ///
    /// Builds the context as a plain `Map` rather than an engine type: the
    /// merge order is the framework's business (global params, path params,
    /// auth claims first, then the handler's `render_with` extras on top), and
    /// the engine's business starts once it has the finished context.
    fn render_response(
        &self,
        resp: &mut Response,
        dict: &Map<String, Value>,
    ) -> std::result::Result<(), RenderError> {
        if let Some(template_name) = resp.template_name.clone() {
            let mut ctx = dict.clone();
            if let Some(handler_dict) = &resp.template_dict {
                for (k, v) in handler_dict {
                    ctx.insert(k.clone(), v.clone());
                }
            }
            let html = self.renderer.render(&template_name, &ctx)?;
            resp.body = crate::response::Body::Bytes(html.into_bytes());
            resp.headers
                .push(("Content-Type".to_string(), "text/html; charset=utf-8".to_string()));
            resp.template_name = None;
            resp.template_dict = None;
        }
        Ok(())
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
    let ok = expected.iter().zip(provided.iter()).fold(0u8, |acc, (a, b)| acc | (a ^ b)) == 0;
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
fn generate_csrf_token() -> String {
    crate::random_hex_token::<32>()
}

// ---------------------------------------------------------------------------
// Stateful run_app variants
// ---------------------------------------------------------------------------

/// Helper: downcast TLS and call the typed destroy.
fn drain_thread_state_typed<T: Any + Send + 'static>(
    destroy: &Arc<dyn Fn(T) + Send + Sync>,
) {
    THREAD_STATE.with(|cell| {
        if let Some(boxed) = cell.borrow_mut().take() {
            if let Ok(t) = boxed.downcast::<T>() {
                destroy(*t);
            }
        }
    });
}

/// Run with global state only.
fn run_app_global<G: Send + Sync + 'static>(
    raw_routes: Vec<GlobalRawRoute<G>>,
    raw_named: Vec<GlobalRawNamed<G>>,
    init_global: Arc<dyn Fn(&Map<String, Value>) -> Result<G> + Send + Sync>,
    destroy_global: Option<Arc<dyn Fn(G) + Send + Sync>>,
    renderer: Arc<dyn RendererFactory>,
) -> Result<()> {
    // We need the config before we can call init_global. Load it here.
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("Usage: {} <site-dir> <config-path>", args[0]);
        std::process::exit(2);
    }
    let site_dir = PathBuf::from(&args[1]);
    let config_path = PathBuf::from(&args[2]);
    let cli_log_level = args
        .windows(2)
        .find(|w| w[0] == "--log-level")
        .map(|w| w[1].clone());

    let config = crate::config::load(&config_path, &site_dir).unwrap_or_else(|e| {
        eprintln!("Config error: {e}");
        std::process::exit(2);
    });

    // Call init_global with the user config.
    let g = init_global(&config.user_config).unwrap_or_else(|e| {
        eprintln!("init_global failed: {e}");
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

    // Build a type-erased on_shutdown callback that calls destroy_global.
    let arc_g_destroy = arc_g.clone();
    let on_shutdown: Option<Box<dyn FnOnce() + Send>> = destroy_global.map(|dg| {
        // We need to unwrap the Arc<G>. Use Arc::try_unwrap; if other Arcs exist,
        // fall back to a no-op (shouldn't happen at shutdown — all requests done).
        let ag = arc_g_destroy;
        let b: Box<dyn FnOnce() + Send> = Box::new(move || {
            if let Ok(g) = Arc::try_unwrap(ag) {
                dg(g);
            }
        });
        b
    });

    run_app_with_shutdown(
        Handlers { routes: code_routes, named },
        config_path,
        site_dir,
        on_shutdown,
        None,
        cli_log_level,
        renderer,
    )
}

/// Run with per-thread state only (no global).
fn run_app_thread_state<T: Any + Send + 'static>(
    raw_routes: Vec<ThreadRawRoute<T>>,
    raw_named: Vec<ThreadRawNamed<T>>,
    init_thread: Arc<dyn Fn(&Map<String, Value>, &()) -> Result<T> + Send + Sync>,
    destroy_thread: Option<Arc<dyn Fn(T) + Send + Sync>>,
    renderer: Arc<dyn RendererFactory>,
) -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("Usage: {} <site-dir> <config-path>", args[0]);
        std::process::exit(2);
    }
    let site_dir = PathBuf::from(&args[1]);
    let config_path = PathBuf::from(&args[2]);
    let cli_log_level = args
        .windows(2)
        .find(|w| w[0] == "--log-level")
        .map(|w| w[1].clone());

    let config = crate::config::load(&config_path, &site_dir).unwrap_or_else(|e| {
        eprintln!("Config error: {e}");
        std::process::exit(2);
    });

    // Set up TLS init fn (closes over config dict).
    let cfg_clone: Map<String, Value> = config.user_config.clone();
    let init_fn = init_thread.clone();
    let tls_init: ThreadInitFn = Arc::new(move || {
        match init_fn(&cfg_clone, &()) {
            Ok(t) => Box::new(t) as Box<dyn Any + Send>,
            Err(e) => panic!("init_thread failed: {e}"),
        }
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
            let h: BoxHandler = Box::new(move |req: &Request| {
                with_thread_state::<T, _>(|t| handler(req, t))
            });
            (path, method, Arc::new(h))
        })
        .collect();

    let named: Vec<NamedHandler> = raw_named
        .into_iter()
        .map(|(name, handler)| {
            let h: BoxHandler = Box::new(move |req: &Request| {
                with_thread_state::<T, _>(|t| handler(req, t))
            });
            (name, Arc::new(h))
        })
        .collect();

    let on_thread_exit: Arc<dyn Fn() + Send + Sync> = Arc::new(drain_thread_state);

    run_app_with_shutdown(
        Handlers { routes: code_routes, named },
        config_path,
        site_dir,
        None,
        Some(on_thread_exit),
        cli_log_level,
        renderer,
    )
}

/// Run with global + per-thread state.
fn run_app_state<G: Send + Sync + 'static, T: Any + Send + 'static>(
    raw_routes: Vec<StateRawRoute<G, T>>,
    raw_named: Vec<StateRawNamed<G, T>>,
    renderer: Arc<dyn RendererFactory>,
    init_global: Arc<dyn Fn(&Map<String, Value>) -> Result<G> + Send + Sync>,
    init_thread: Arc<dyn Fn(&Map<String, Value>, &G) -> Result<T> + Send + Sync>,
    destroy_thread: Option<Arc<dyn Fn(T) + Send + Sync>>,
    destroy_global: Option<Arc<dyn Fn(G) + Send + Sync>>,
) -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("Usage: {} <site-dir> <config-path>", args[0]);
        std::process::exit(2);
    }
    let site_dir = PathBuf::from(&args[1]);
    let config_path = PathBuf::from(&args[2]);
    let cli_log_level = args
        .windows(2)
        .find(|w| w[0] == "--log-level")
        .map(|w| w[1].clone());

    let config = crate::config::load(&config_path, &site_dir).unwrap_or_else(|e| {
        eprintln!("Config error: {e}");
        std::process::exit(2);
    });

    let g = init_global(&config.user_config).unwrap_or_else(|e| {
        eprintln!("init_global failed: {e}");
        std::process::exit(2);
    });
    let arc_g = Arc::new(g);

    // Set up TLS init fn.
    let cfg_clone: Map<String, Value> = config.user_config.clone();
    let init_fn = init_thread.clone();
    let arc_g2 = arc_g.clone();
    let tls_init: ThreadInitFn = Arc::new(move || {
        match init_fn(&cfg_clone, &*arc_g2) {
            Ok(t) => Box::new(t) as Box<dyn Any + Send>,
            Err(e) => panic!("init_thread failed: {e}"),
        }
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

    let arc_g_destroy = arc_g.clone();
    let on_shutdown: Option<Box<dyn FnOnce() + Send>> = destroy_global.map(|dg| {
        let ag = arc_g_destroy;
        let b: Box<dyn FnOnce() + Send> = Box::new(move || {
            if let Ok(g) = Arc::try_unwrap(ag) {
                dg(g);
            }
        });
        b
    });

    let on_thread_exit: Arc<dyn Fn() + Send + Sync> = Arc::new(drain_thread_state);

    run_app_with_shutdown(
        Handlers { routes: code_routes, named },
        config_path,
        site_dir,
        on_shutdown,
        Some(on_thread_exit),
        cli_log_level,
        renderer,
    )
}

// ---------------------------------------------------------------------------
// Thread pool
// ---------------------------------------------------------------------------

type WorkItem = Box<dyn FnOnce() + Send + 'static>;

pub struct ThreadPool {
    queue: std::sync::mpsc::SyncSender<WorkItem>,
    in_flight: Arc<AtomicUsize>,
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
        let in_flight = Arc::new(AtomicUsize::new(0));

        for _ in 0..size {
            let rx = rx.clone();
            let in_flight2 = in_flight.clone();
            let on_exit = on_thread_exit.clone();
            std::thread::spawn(move || {
                loop {
                    let work = rx.lock().unwrap().recv();
                    match work {
                        Ok(f) => {
                            in_flight2.fetch_add(1, Ordering::SeqCst);
                            f();
                            in_flight2.fetch_sub(1, Ordering::SeqCst);
                        }
                        Err(_) => break,
                    }
                }
                // Channel closed — call the exit callback (destroy_thread).
                if let Some(cb) = &on_exit {
                    cb();
                }
            });
        }

        Self { queue: tx, in_flight }
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
        match self.queue.try_send(work) {
            Ok(()) => Ok(true),
            Err(_) => {
                // Queue full — recover resource.
                let r = resource_cell.lock().unwrap().take().unwrap();
                Err(r)
            }
        }
    }

    /// Wait for all in-flight work to complete.
    pub fn drain(&self) {
        while self.in_flight.load(Ordering::SeqCst) > 0 {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    pub fn in_flight(&self) -> usize {
        self.in_flight.load(Ordering::SeqCst)
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
/// which no render app did. The only `remove_file` in this file used to be at
/// startup, clearing a stale socket before `bind`, which is the workaround for
/// the missing cleanup rather than the cleanup.
fn install_shutdown(socket_path: &std::path::Path) -> crate::signal::ShutdownHandle {
    let name = std::env::args()
        .next()
        .and_then(|a| {
            std::path::Path::new(&a)
                .file_name()
                .and_then(|s| s.to_str())
                .map(str::to_owned)
        })
        .unwrap_or_else(|| "m6-render".to_string());
    crate::signal::ShutdownHandle::install(
        crate::signal::Service::new(name).socket(socket_path),
    )
}

#[inline]
pub fn is_shutdown() -> bool {
    crate::signal::is_shutdown()
}

// ---------------------------------------------------------------------------
// App builder — no state
// ---------------------------------------------------------------------------

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
    { Arc::new(crate::template::TeraFactory) }
    #[cfg(not(feature = "templates"))]
    { Arc::new(crate::render::NoTemplates) }
}

impl App {
    pub fn new() -> Self {
        Self { routes: vec![], named: vec![], renderer: default_renderer() }
    }

    /// Register a handler under a name, for config to route to.
    ///
    /// `route_get("/health", f)` binds code to a path at the call site, so the
    /// set of routes is fixed for the life of the process and a config reload
    /// cannot add one. That is correct for a service whose routes are part of
    /// its code, and wrong for one whose routes are part of its deployment: a
    /// static file server gains an asset tree by being told about a directory,
    /// not by being recompiled.
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
    /// Config routes are recompiled on every reload, so adding, changing or
    /// removing one of these takes effect without a restart, and the handler
    /// reads `root` through `Request::route_setting`. A route naming a handler
    /// that was never registered is a hard error: startup exits 2 and a reload
    /// is refused with the previous routes left serving.
    pub fn handler(
        mut self,
        name: &str,
        handler: impl Fn(&Request) -> Result<Response> + Send + Sync + 'static,
    ) -> Self {
        self.named.push((
            name.to_string(),
            Arc::new(Box::new(handler) as BoxHandler),
        ));
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
type GlobalRawRoute<G> = (String, RouteMethod, Arc<dyn Fn(&Request, &G) -> Result<Response> + Send + Sync>);

// The same, bound to a name rather than to a pattern. See `App::handler`.
type GlobalRawNamed<G> = (String, Arc<dyn Fn(&Request, &G) -> Result<Response> + Send + Sync>);

pub struct AppWithGlobal<G: Send + Sync + 'static> {
    raw_routes: Vec<GlobalRawRoute<G>>,
    raw_named: Vec<GlobalRawNamed<G>>,
    init_global: Arc<dyn Fn(&Map<String, Value>) -> Result<G> + Send + Sync>,
    destroy_global: Option<Arc<dyn Fn(G) + Send + Sync>>,
    renderer: Arc<dyn RendererFactory>,
}

impl<G: Send + Sync + 'static> AppWithGlobal<G> {
    pub fn on_destroy(mut self, f: impl Fn(G) + Send + Sync + 'static) -> Self {
        self.destroy_global = Some(Arc::new(f));
        self
    }

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
        self.raw_routes.push((
            path.to_string(),
            method,
            Arc::new(handler),
        ));
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
            self.destroy_global,
            self.renderer,
        )
    }
}

impl App {
    pub fn with_global<G: Send + Sync + 'static>(
        init_global: impl Fn(&Map<String, Value>) -> Result<G> + Send + Sync + 'static,
    ) -> AppWithGlobal<G> {
        AppWithGlobal {
            raw_routes: vec![],
            raw_named: vec![],
            init_global: Arc::new(init_global),
            destroy_global: None,
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
        init_global: impl Fn(&Map<String, Value>) -> Result<G> + Send + Sync + 'static,
        init_thread: impl Fn(&Map<String, Value>, &G) -> Result<T> + Send + Sync + 'static,
    ) -> AppWithState<G, T> {
        AppWithState {
            raw_routes: vec![],
            raw_named: vec![],
            init_global: Arc::new(init_global),
            init_thread: Arc::new(init_thread),
            destroy_thread: None,
            destroy_global: None,
            renderer: default_renderer(),
        }
    }
}

// ---------------------------------------------------------------------------
// App with per-thread state only
// ---------------------------------------------------------------------------

// Raw stateful route (ThreadLocal): stores the handler before config/TLS known.
type ThreadRawRoute<T> = (String, RouteMethod, Arc<dyn Fn(&Request, &mut T) -> Result<Response> + Send + Sync>);

// The same, bound to a name rather than to a pattern. See `App::handler`.
type ThreadRawNamed<T> = (String, Arc<dyn Fn(&Request, &mut T) -> Result<Response> + Send + Sync>);

pub struct AppWithThreadState<T: Any + Send + 'static> {
    raw_routes: Vec<ThreadRawRoute<T>>,
    raw_named: Vec<ThreadRawNamed<T>>,
    init_thread: Arc<dyn Fn(&Map<String, Value>, &()) -> Result<T> + Send + Sync>,
    destroy_thread: Option<Arc<dyn Fn(T) + Send + Sync>>,
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
        self.raw_routes.push((path.to_string(), method, Arc::new(handler)));
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
        self.add_route(path, RouteMethod::Delete, move |req, t| handler(req, &(), t))
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
    init_global: Arc<dyn Fn(&Map<String, Value>) -> Result<G> + Send + Sync>,
    init_thread: Arc<dyn Fn(&Map<String, Value>, &G) -> Result<T> + Send + Sync>,
    destroy_thread: Option<Arc<dyn Fn(T) + Send + Sync>>,
    destroy_global: Option<Arc<dyn Fn(G) + Send + Sync>>,
    renderer: Arc<dyn RendererFactory>,
}

impl<G: Send + Sync + 'static, T: Any + Send + 'static> AppWithState<G, T> {
    pub fn on_destroy_thread(mut self, f: impl Fn(T) + Send + Sync + 'static) -> Self {
        self.destroy_thread = Some(Arc::new(f));
        self
    }

    pub fn on_destroy(mut self, f: impl Fn(G) + Send + Sync + 'static) -> Self {
        self.destroy_global = Some(Arc::new(f));
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
        self.raw_routes.push((path.to_string(), method, Arc::new(handler)));
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
            self.destroy_thread,
            self.destroy_global,
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

/// Config routes naming a handler that was never registered.
///
/// Returned rather than logged so the two callers can answer differently in
/// the way each of them should: startup exits 2, and a reload refuses the new
/// state and keeps serving the old one.
///
/// This is deliberately fatal rather than a per-route warning. A misplaced
/// wildcard is narrowed and warned about because the narrower reading is still
/// a defensible route; a handler name with no code behind it has no reading at
/// all, and the alternatives are a route that 404s or one that 500s while the
/// config plainly says it should serve. Refusing the whole config keeps the
/// previous one serving, which is the outcome an operator can recover from.
fn unknown_handlers(
    routes: &[CompiledRoute],
    named: &HashMap<String, Arc<BoxHandler>>,
) -> Vec<(String, String)> {
    let mut missing = Vec::new();
    for route in routes {
        if let Some(name) = &route.handler {
            if !named.contains_key(name) {
                missing.push((route.pattern.clone(), name.clone()));
            }
        }
    }
    missing
}

/// Render `unknown_handlers`' answer as one line an operator can act on.
fn describe_unknown(missing: &[(String, String)], registered: &[String]) -> String {
    let named: Vec<String> = missing
        .iter()
        .map(|(pattern, handler)| format!("{pattern} -> `{handler}`"))
        .collect();
    let known = if registered.is_empty() {
        "none are registered".to_string()
    } else {
        format!("registered: {}", registered.join(", "))
    };
    format!("{} ({known})", named.join("; "))
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

    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("Usage: {} <site-dir> <config-path>", args[0]);
        std::process::exit(2);
    }
    let site_dir = PathBuf::from(&args[1]);
    let config_path = PathBuf::from(&args[2]);
    let cli_log_level = args
        .windows(2)
        .find(|w| w[0] == "--log-level")
        .map(|w| w[1].clone());
    run_app_with_shutdown(
        Handlers { routes: code_routes, named },
        config_path,
        site_dir,
        None,
        None,
        cli_log_level,
        renderer,
    )
}

fn run_app_with_shutdown(
    handlers: Handlers,
    config_path: PathBuf,
    site_dir: PathBuf,
    on_shutdown: Option<Box<dyn FnOnce() + Send>>,
    on_thread_exit: Option<Arc<dyn Fn() + Send + Sync + 'static>>,
    cli_log_level: Option<String>,
    renderer: Arc<dyn RendererFactory>,
) -> Result<()> {
    // Load config.
    let config = crate::config::load(&config_path, &site_dir).unwrap_or_else(|e| {
        eprintln!("Config error: {e}");
        std::process::exit(2);
    });

    // Init logging: site.toml base → renderer config [log] → CLI --log-level.
    let (site_level, site_format) = crate::log::read_site_log_config(&site_dir);
    let format = config.log.format.as_deref().unwrap_or(&site_format).to_string();
    let cfg_level = config.log.level.as_deref().unwrap_or(&site_level).to_string();
    let level = cli_log_level.as_deref().unwrap_or(&cfg_level).to_string();
    let _log_guard = crate::log::init(&format, &level).unwrap_or_else(|e| {
        eprintln!("logging init error: {e}");
        std::process::exit(1);
    });

    let socket_path = crate::socket_path_from_config(&config_path);

    // Build framework state.
    let Handlers { routes: code_routes, named } = handlers;
    let code_route_signatures: Vec<(String, RouteMethod)> = code_routes
        .iter()
        .map(|(p, m, _)| (p.clone(), m.clone()))
        .collect();

    // Handlers registered by name, for config-declared routes to dispatch to.
    // Built before the state so a route naming a handler nothing provides is
    // caught at startup rather than by the first request that matches it.
    let named_handlers: Arc<HashMap<String, Arc<BoxHandler>>> =
        Arc::new(named.into_iter().collect());
    let mut registered_names: Vec<String> = named_handlers.keys().cloned().collect();
    registered_names.sort();

    let framework_state =
        FrameworkState::build(config, site_dir.clone(), &code_route_signatures, &*renderer)
            .unwrap_or_else(|e| {
                eprintln!("Startup error: {e:#}");
                std::process::exit(2);
            });

    let missing = unknown_handlers(&framework_state.routes, &named_handlers);
    if !missing.is_empty() {
        eprintln!(
            "Config error: route names an unregistered handler: {}",
            describe_unknown(&missing, &registered_names)
        );
        std::process::exit(2);
    }

    // Validate flash_secret presence at startup (exit 2 if feature enabled but key absent).
    #[cfg(feature = "flash")]
    {
        if framework_state.flash_secret.is_empty() {
            eprintln!(
                "Error: flash feature enabled but `flash_secret` is absent from config \
                 (generate: openssl rand -base64 32)"
            );
            std::process::exit(2);
        }
    }

    let tp_size = framework_state.config.thread_pool.size;
    let tp_queue = framework_state.config.thread_pool.queue_size;
    // Read once at startup, like the pool dimensions above and for the same
    // reason: it is applied to a socket at accept time, so a reload cannot
    // retune it without reopening connections that are already being served.
    let read_timeout = framework_state.config.server.read_timeout;
    let socket_mode = framework_state.config.server.socket_mode;

    // Wrap state in RwLock so hot reload can atomically swap it while
    // in-flight requests continue reading the old state via their cloned Arc.
    let fs: Arc<std::sync::RwLock<FrameworkState>> =
        Arc::new(std::sync::RwLock::new(framework_state));

    // Build handler lookup map (pattern:METHOD → handler).
    // Include the method in the key so GET and POST handlers registered on the
    // same path are stored and looked up independently.
    let code_handlers: Arc<HashMap<String, Arc<BoxHandler>>> = Arc::new(
        code_routes
            .into_iter()
            .map(|(p, m, h)| (format!("{}:{}", p, route_method_key(&m)), h))
            .collect(),
    );

    // Wrap on_shutdown for call at most once (accept loop is single-threaded).
    let mut on_shutdown_cell = on_shutdown;

    // Bind socket.
    if socket_path.exists() {
        std::fs::remove_file(&socket_path).ok();
    }
    if let Some(parent) = socket_path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let listener = UnixListener::bind(&socket_path).unwrap_or_else(|e| {
        eprintln!("Failed to bind socket {}: {e}", socket_path.display());
        std::process::exit(2);
    });

    // Before the shutdown handle below, which self-connects to this socket:
    // the mode has to be right by the time anything can reach it.
    crate::server::apply_socket_mode(&socket_path, socket_mode);

    // After the bind, not before: the wake connects to this socket, so it has
    // to exist by the time a signal can arrive.
    let shutdown = install_shutdown(&socket_path);

    {
        let fs_r = fs.read().unwrap();
        info!(
            routes = fs_r.routes.len(),
            threads = tp_size,
            socket = %socket_path.display(),
            "routes loaded"
        );
    }

    let pool = Arc::new(ThreadPool::new_with_exit(tp_size, tp_queue, on_thread_exit));

    // ── Hot-reload setup ──────────────────────────────────────────────────
    // ConfigWatcher watches the directories containing config_path and
    // site.toml; its fd is added to the poll(2) call so reloads happen
    // within milliseconds of a file write when supported.
    // When raw_fd() returns None (fallback platform or init failure): fall
    // back to mtime polling at ~1-second intervals via the poll(2) timeout
    // countdown.
    let site_toml_path = site_dir.join("site.toml");
    let mut watcher = crate::ConfigWatcher::new(&[&config_path, &site_toml_path]).ok();

    // Mtime fallback state — only meaningful when watcher.raw_fd() is None.
    let mut config_mtime    = file_mtime(&config_path);
    let mut site_toml_mtime = file_mtime(&site_toml_path);
    let mut reload_countdown: u8 = 10;

    // Filename (not path) used to match watcher events.
    let config_filename = config_path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_string();

    use std::os::unix::io::AsRawFd;
    let listener_fd = listener.as_raw_fd();

    loop {
        // ── poll(2) ─────────────────────────────────────────────────────
        // When inotify is available we poll two fds (listener + inotify).
        // The 100 ms timeout ensures we catch shutdown signals promptly
        // even when the server is completely idle.
        let watcher_fd = watcher.as_ref().and_then(|w| w.raw_fd());
        let ready = crate::server::poll_listener_and_watcher(listener_fd, watcher_fd, 100);
        let (listener_ready, inotify_fired) = (ready.listener, ready.watcher);

        // ── Determine whether a reload is needed ─────────────────────────
        let mut should_reload = false;

        // Neither fd fired: a timeout, or a signal interrupted the wait.
        if ready.idle {
            if is_shutdown() {
                info!("Shutdown signal received, draining...");
                pool.drain();
                if let Some(f) = on_shutdown_cell.take() {
                    f();
                }
                info!("Clean shutdown");
                break;
            }
            // Mtime fallback: check every ~10 timeouts (≈1 s).
            if watcher_fd.is_none() {
                reload_countdown = reload_countdown.saturating_sub(1);
                if reload_countdown == 0 {
                    reload_countdown = 10;
                    let nm = file_mtime(&config_path);
                    let ns = file_mtime(&site_toml_path);
                    if nm != config_mtime || ns != site_toml_mtime {
                        config_mtime = nm;
                        site_toml_mtime = ns;
                        should_reload = true;
                    }
                }
            }
            if !should_reload {
                continue;
            }
        }

        // Watcher fired — drain events and check for our watched files.
        if inotify_fired {
            should_reload = watcher.as_mut().map_or(false, |w| w.read_events(&[&config_filename, "site.toml"]));
        }

        // ── Hot reload ───────────────────────────────────────────────────
        if should_reload {
            info!("Config change detected, reloading...");
            let reload_start = std::time::Instant::now();
            match crate::config::load(&config_path, &site_dir) {
                Err(e) => {
                    error!("Reload failed (config parse error): {e}");
                }
                Ok(new_config) => {
                    match FrameworkState::build(
                        new_config,
                        site_dir.clone(),
                        &code_route_signatures,
                        &*renderer,
                    ) {
                        Err(e) => {
                            error!("Reload failed (template/state error): {e}");
                        }
                        Ok(new_state) => {
                            // The new config may have added a route naming a
                            // handler this binary does not have. Refuse the
                            // whole state rather than swap in one that cannot
                            // serve a route it advertises: the previous routes
                            // keep serving, which is what makes a typo in a
                            // live config recoverable.
                            let missing =
                                unknown_handlers(&new_state.routes, &named_handlers);
                            if !missing.is_empty() {
                                error!(
                                    "Reload refused, route names an unregistered handler: {}",
                                    describe_unknown(&missing, &registered_names)
                                );
                            } else {
                                let routes = new_state.routes.len();
                                *fs.write().unwrap() = new_state;
                                let elapsed = reload_start.elapsed().as_millis();
                                info!(
                                    elapsed_ms = elapsed,
                                    routes = routes,
                                    "Reload complete"
                                );
                            }
                        }
                    }
                }
            }
            // Don't skip accept — the listener may also be ready.
        }

        if !listener_ready {
            continue;
        }

        match listener.accept() {
            Ok((stream, _)) => {
                // Before the handoff, not inside the worker: a worker that has
                // already taken the connection is the resource being protected.
                crate::server::apply_read_timeout(&stream, read_timeout);

                let fs = fs.clone();
                let code_handlers = code_handlers.clone();
                let named_handlers = named_handlers.clone();

                match pool.try_submit(stream, move |mut s| {
                    handle_connection(&mut s, &*fs, &code_handlers, &named_handlers);
                }) {
                    Ok(_) => {}
                    Err(mut s) => {
                        warn!("Thread pool queue full, returning 503");
                        crate::server::write_error_response(&mut s, 503, "Service Unavailable").ok();
                    }
                }
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(e) => {
                if is_shutdown() {
                    break;
                }
                error!("Accept error: {e}");
            }
        }

        if is_shutdown() {
            pool.drain();
            if let Some(f) = on_shutdown_cell.take() {
                f();
            }
            break;
        }
    }

    shutdown.complete();
    Ok(())
}

/// One accepted connection, served until it ends.
///
/// The loop, the malformed-request answer, the HEAD rule and the keep-alive
/// decision are core's. This file had its own of each, and answered exactly
/// one request per connection.
fn handle_connection(
    stream: &mut UnixStream,
    fs: &std::sync::RwLock<FrameworkState>,
    code_handlers: &HashMap<String, Arc<BoxHandler>>,
    named_handlers: &HashMap<String, Arc<BoxHandler>>,
) {
    let served: std::result::Result<(), std::io::Error> =
        crate::server::serve_connection(stream, |raw, resp| {
            handle_request(raw, resp, fs, code_handlers, named_handlers);
            Ok(())
        });
    if let Err(e) = served {
        error!("connection error: {e}");
    }
}

fn handle_request<W: std::io::Write>(
    raw: &RawRequest,
    stream: &mut crate::h1::Responder<'_, W>,
    fs: &std::sync::RwLock<FrameworkState>,
    code_handlers: &HashMap<String, Arc<BoxHandler>>,
    named_handlers: &HashMap<String, Arc<BoxHandler>>,
) {
    let start = std::time::Instant::now();

    // Take a read lock once per request — released after we have everything
    // we need so that a concurrent hot reload can proceed promptly.
    let (route_match, site_dir, compression, minification) = {
        let fs_r = fs.read().unwrap();
        let route_match = find_route(raw.path(), raw.method(), &fs_r.routes)
            .map(|(route, params)| (route.clone(), params));
        let site_dir = fs_r.site_dir.clone();
        let compression = fs_r.config.compression.clone();
        let minification = fs_r.config.minification.clone();
        (route_match, site_dir, compression, minification)
    };

    // Populated from `dict["csrf_token"]` inside the matched-route arm below
    // (when the csrf feature is on) so the cookie set further down uses the
    // exact same token the response body was rendered with — see the
    // comment at that Set-Cookie site for why a second, independently
    // generated token there was a real bug.
    #[cfg(feature = "csrf")]
    let mut csrf_token_for_cookie: Option<String> = None;

    let mut resp = match route_match {
        None => {
            warn!(path = raw.path(), "unmatched path");
            Response::not_found()
        }
        Some((route, path_params)) => {
            // Re-acquire read lock for dict building and template rendering.
            let fs_r = fs.read().unwrap();

            // Build request dict.
            let dict = match fs_r.build_dict(&raw, &route, &path_params) {
                Ok(d) => d,
                Err(e) => {
                    let r = error_to_response(&e);
                    crate::server::write_response(stream, r).ok();
                    return;
                }
            };

            #[cfg(feature = "csrf")]
            {
                csrf_token_for_cookie = dict.get("csrf_token").and_then(|v| v.as_str()).map(str::to_string);
            }

            let req = Request::new(raw.clone(), dict.clone(), site_dir.clone())
                .with_route(&route);

            // Dispatch to code handler or template render.
            // A code handler is only used when the matched route is a code route
            // (template is None). If the matched route has a template (config route),
            // use the template even if a code handler exists for the same pattern
            // — this ensures GET routes handled by config are not shadowed by POST
            // code handlers registered on the same path.
            let handler_key = format!("{}:{}", route.pattern, route_method_key(&route.method));
            let mut resp = if let Some(name) = &route.handler {
                // A config-declared route naming a registered handler. The
                // lookup cannot miss: `unknown_handlers` refuses a state whose
                // routes name anything absent, at startup and at every reload,
                // so an absent handler here means that check was bypassed
                // rather than that config is wrong. Answer 500 and say so,
                // rather than 404, which would read as "no such page".
                match named_handlers.get(name) {
                    Some(handler) => match handler.call(&req) {
                        Ok(r) => r,
                        Err(e) => error_to_response(&e),
                    },
                    None => {
                        error!(
                            pattern = %route.pattern,
                            handler = %name,
                            "route names a handler that is not registered"
                        );
                        Response::status(500)
                    }
                }
            } else if route.template.is_none() {
                if let Some(handler) = code_handlers.get(&handler_key) {
                    match handler.call(&req) {
                        Ok(r) => r,
                        Err(e) => error_to_response(&e),
                    }
                } else {
                    Response::not_found()
                }
            } else if let Some(template) = &route.template {
                match Response::render_dict(template, &dict, route.status) {
                    Ok(r) => r,
                    Err(e) => error_to_response(&e),
                }
            } else {
                Response::not_found()
            };

            // Render template if needed.
            if resp.template_name.is_some() {
                match fs_r.render_response(&mut resp, &dict) {
                    Ok(()) => {}
                    Err(RenderError::NotFound) => {
                        warn!(path = raw.path(), "template signalled not_found");
                        resp = Response::not_found();
                    }
                    Err(RenderError::Failed(e)) => {
                        error!("Template render error: {e:#}");
                        resp = Response::status(500);
                    }
                }
            }

            // Add Cache-Control header. route.cache is a free-form string
            // (config.rs parses it as-is, defaulting to "public") — this
            // used to collapse anything that wasn't literally "no-store"
            // down to a bare "public", silently discarding any max-age or
            // other directive a site actually configured (e.g.
            // "public, max-age=60, must-revalidate"). Pass it through as
            // configured instead.
            resp = resp.header("Cache-Control", &route.cache);

            // Last-Modified, from the route's own inputs (see CompiledRoute).
            // Only on a success: attaching a validator to a 404 or a 500 would
            // invite a client to keep revalidating an error as though it were
            // content. Skipped when the route has no honest date -- a code
            // route, or one whose params path is resolved per request.
            //
            // m6-http already honours If-Modified-Since against a cached
            // entry's Last-Modified (cache::is_not_modified); it simply never
            // had one to compare with for rendered HTML, so every date-based
            // revalidation returned the whole page. Emitting the header here is
            // the whole fix.
            if resp.status < 300 {
                if let Some(lm) = route.last_modified {
                    resp = resp.header("Last-Modified", &httpdate::fmt_http_date(lm));
                }
            }

            // Add any extra per-route headers (e.g. COOP/COEP for cross-origin isolation).
            for (k, v) in &route.headers {
                resp.headers.push((k.clone(), v.clone()));
            }

            resp
        }
    };

    // ── CSRF: set _csrf cookie if not already present in the request.
    #[cfg(feature = "csrf")]
    {
        let has_csrf = raw.header("cookie")
            .map(|h| h.contains("_csrf="))
            .unwrap_or(false);
        if !has_csrf {
            // Reuse the exact token build_dict already put in dict["csrf_token"]
            // (captured above as csrf_token_for_cookie) rather than generating
            // a second, independent one here. Those used to be two unrelated
            // random values: the page's hidden csrf_token field carried
            // whatever build_dict generated, while the cookie actually sent
            // to the browser carried a *different* token generated
            // independently right here — so for any visitor without an
            // existing _csrf cookie (i.e. every first-time visitor), the
            // submitted field could never match the cookie and verify_csrf()
            // would reject every legitimate submission. Falls back to a
            // fresh token only for routes with no dict (e.g. an unmatched
            // path's 404), where there's no rendered form to have carried one.
            let token = csrf_token_for_cookie.unwrap_or_else(generate_csrf_token);
            // No HttpOnly, deliberately: this is the double-submit token and
            // the page has to be able to read it back.
            resp.headers.push(
                crate::cookie::Cookie::new("_csrf", token)
                    .path("/")
                    .same_site(crate::cookie::SameSite::Strict)
                    .secure()
                    .to_header(),
            );
        }
    }

    // ── Flash: clear the _flash cookie after reading it.
    #[cfg(feature = "flash")]
    {
        let had_flash = raw.header("cookie")
            .map(|h| h.contains("_flash="))
            .unwrap_or(false);
        if had_flash {
            // Path and HttpOnly must match the cookie being removed or the
            // browser treats this as a different cookie and keeps both.
            resp.headers.push(
                crate::cookie::Cookie::removal("_flash")
                    .path("/")
                    .http_only()
                    .to_header(),
            );
        }
    }

    // ── Minification: applied BEFORE compression for better ratios.
    //
    // Both transforms below are skipped for a `verbatim` response, which is a
    // handler saying it has already produced the exact representation. m6-file
    // negotiates the coding itself and builds an ETag naming it, so
    // re-compressing here would put brotli bytes on the wire under a tag
    // asserting identity.
    //
    // A streamed body needs no such flag: `as_bytes` gives `None` and there is
    // nothing to transform. That is deliberate rather than incidental -- the
    // alternative shape, a `Vec<u8>` plus an optional reader, would have left
    // all four transforms free to run on the empty bytes beside the stream and
    // produce a correct-looking response with the wrong body.
    if !resp.verbatim {
        if let Some(bytes) = resp.body.as_bytes() {
            if !bytes.is_empty() {
                let content_type =
                    crate::headers::get(&resp.headers[..], "content-type").unwrap_or("");
                let mime = content_type.split(';').next().unwrap_or("").trim();

                if minification.is_enabled(mime) {
                    let minified = match mime {
                        "text/html" => Some(crate::minify::minify_html(bytes, minification.inline_js)),
                        "text/css" => Some(crate::minify::minify_css(bytes)),
                        "application/json" => Some(crate::minify::minify_json(bytes)),
                        "application/javascript" | "text/javascript" => {
                            Some(crate::minify::minify_js(bytes))
                        }
                        _ => None,
                    };
                    if let Some(m) = minified {
                        resp.body = crate::response::Body::Bytes(m);
                    }
                }
            }
        }
    }

    // ── Compression: applied AFTER minification.
    let accept_encoding = raw.header("accept-encoding").unwrap_or("");
    if !resp.verbatim && !resp.body.is_empty() {
        let content_type =
            crate::headers::get(&resp.headers[..], "content-type").unwrap_or("");
        let mime = content_type.split(';').next().unwrap_or("").trim();

        if let Some(level) = compression.get(mime) {
            // Ask the client what it will actually accept, rather than testing
            // whether a coding name appears anywhere in the header.
            //
            // This used to be `ae_contains`, a raw substring match, and it was
            // wrong three ways at once. Measured against production on
            // 2026-09-10: `gzip, br;q=0` was served **br**, so a client that
            // had explicitly refused brotli got brotli (RFC 9110 12.4.2 makes
            // `q=0` "not acceptable", not "least preferred"); `notbr` was
            // served **br**, matching the substring inside an unrelated token;
            // and `gzip;q=1.0, br;q=0.1` was served **br**, because candidates
            // were tested in our order and the first hit won, ignoring the
            // client's stated preference entirely.
            //
            // m6-file had already found and fixed this for static assets and
            // written the parser. m6-render never got it, which is why the fix
            // now lives in m6-core and both call it: this is the second time
            // the same rules diverged between two crates.
            //
            // Only codings the config actually permits are offered, so a level
            // of 0 removes a candidate rather than producing an empty body.
            let mut candidates: Vec<&str> = Vec::with_capacity(2);
            if level.brotli > 0 {
                candidates.push("br");
            }
            if level.gzip > 0 {
                candidates.push("gzip");
            }
            // `as_bytes` is `None` for a stream, so a streamed body simply
            // does not reach either compressor.
            let plain = resp.body.as_bytes().map(|b| b.to_vec());
            match (crate::preferred_coding(accept_encoding, &candidates), plain) {
                (Some("br"), Some(bytes)) => {
                    if let Ok(compressed) =
                        crate::compress::brotli_compress(&bytes, level.brotli)
                    {
                        resp.body = crate::response::Body::Bytes(compressed);
                        resp.headers
                            .push(("Content-Encoding".to_string(), "br".to_string()));
                    }
                }
                (Some("gzip"), Some(bytes)) => {
                    if let Ok(compressed) =
                        crate::compress::gzip_compress(&bytes, level.gzip)
                    {
                        resp.body = crate::response::Body::Bytes(compressed);
                        resp.headers
                            .push(("Content-Encoding".to_string(), "gzip".to_string()));
                    }
                }
                // Nothing acceptable: send identity rather than a 406. RFC 9110
                // 12.5.3 permits that, and an uncompressed body is a better
                // outcome than refusing to serve the resource.
                _ => {}
            }
        }
    }

    let latency = start.elapsed().as_micros();
    tracing::debug!(
        path = raw.path(),
        method = raw.method(),
        status = resp.status,
        latency_us = latency,
        "request complete"
    );

    crate::server::write_response(stream, resp).ok();
}



// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

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
            method: RouteMethod::Any,
            segments: compile_pattern("/blog/{stem}"),
            template: None,
            params_files: vec![],
            status: 200,
            cache: "public".to_string(),
            headers: vec![],
            specificity: 3,
            last_modified: None,
            handler: None,
            settings: Arc::new(Map::new()),
        };
        let segs: Vec<&str> = "/blog/hello-world".split('/').filter(|s| !s.is_empty()).collect();
        let m = match_route(&segs, &route);
        assert!(m.is_some());
        let params = m.unwrap();
        let stem = params.iter().find(|(k, _)| k == "stem").map(|(_, v)| v.as_str());
        assert_eq!(stem, Some("hello-world"));
    }

    #[test]
    fn test_no_match_on_different_length() {
        let route = CompiledRoute {
            pattern: "/blog/{stem}".to_string(),
            method: RouteMethod::Any,
            segments: compile_pattern("/blog/{stem}"),
            template: None,
            params_files: vec![],
            status: 200,
            cache: "public".to_string(),
            headers: vec![],
            specificity: 3,
            last_modified: None,
            handler: None,
            settings: Arc::new(Map::new()),
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
                method: RouteMethod::Any,
                segments: compile_pattern("/blog/{stem}"),
                template: None,
                params_files: vec![],
                status: 200,
                cache: "public".to_string(),
                headers: vec![],
                specificity: route_specificity(&compile_pattern("/blog/{stem}")),
                last_modified: None,
                handler: None,
                settings: Arc::new(Map::new()),
            },
            CompiledRoute {
                pattern: "/blog/about".to_string(),
                method: RouteMethod::Any,
                segments: compile_pattern("/blog/about"),
                template: None,
                params_files: vec![],
                status: 200,
                cache: "public".to_string(),
                headers: vec![],
                specificity: route_specificity(&compile_pattern("/blog/about")),
                last_modified: None,
                handler: None,
                settings: Arc::new(Map::new()),
            },
        ];

        let (matched, _) = find_route("/blog/about", "GET", &routes).unwrap();
        assert_eq!(matched.pattern, "/blog/about");
    }

    #[test]
    fn test_unmatched_returns_none() {
        let routes: Vec<CompiledRoute> = vec![];
        assert!(find_route("/anything", "GET", &routes).is_none());
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
        assert!(!(submitted1 && submitted2), "both submits succeeded but queue_size=1");
    }

    /// Hot-reload: FrameworkState::build succeeds with a fresh config, and the
    /// RwLock swap is visible to subsequent readers.
    #[test]
    fn test_hot_reload_state_swap() {
        use std::io::Write;
        use tempfile::NamedTempFile;
        use std::sync::RwLock;

        let site_dir = tempfile::TempDir::new().unwrap();
        // Create a minimal templates directory so Tera doesn't scan an
        // unpredictable system directory (e.g. /tmp with stale root-owned dirs).
        std::fs::create_dir(site_dir.path().join("templates")).unwrap();

        let mut f = NamedTempFile::new().unwrap();
        write!(f, "site_name = \"v1\"\n").unwrap();

        let cfg1 = crate::config::load(f.path(), site_dir.path()).unwrap();
        assert_eq!(cfg1.user_config["site_name"].as_str().unwrap(), "v1");

        let state1 = FrameworkState::build(cfg1, site_dir.path().to_path_buf(), &[], &*default_renderer()).unwrap();
        let fs = Arc::new(RwLock::new(state1));

        // Verify initial state.
        assert_eq!(fs.read().unwrap().config.user_config["site_name"].as_str().unwrap(), "v1");

        // Write a new config.
        let mut f2 = NamedTempFile::new().unwrap();
        write!(f2, "site_name = \"v2\"\n").unwrap();

        let cfg2 = crate::config::load(f2.path(), site_dir.path()).unwrap();
        let state2 = FrameworkState::build(cfg2, site_dir.path().to_path_buf(), &[], &*default_renderer()).unwrap();

        // Atomic swap.
        *fs.write().unwrap() = state2;

        // New state is visible.
        assert_eq!(fs.read().unwrap().config.user_config["site_name"].as_str().unwrap(), "v2");
    }

    /// file_mtime returns different values after a file is updated.
    #[test]
    fn test_file_mtime_changes() {
        use std::io::Write;
        use tempfile::NamedTempFile;

        let mut f = NamedTempFile::new().unwrap();
        write!(f, "v1\n").unwrap();
        let mtime1 = file_mtime(f.path());
        assert!(mtime1.is_some());

        // Wait at least 1 file-system tick (typically 1s on HFS+/APFS).
        // On most CI systems the mtime granularity is 1ns, so just rewrite.
        std::thread::sleep(std::time::Duration::from_millis(10));
        // Touch by setting mtime explicitly via filetime.
        let future = filetime::FileTime::from_unix_time(
            filetime::FileTime::now().unix_seconds() + 1, 0
        );
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
        compression.insert("text/html".to_string(), CompressionLevel { brotli: 0, gzip: 0 });

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
        compression.insert("text/html".to_string(), CompressionLevel { brotli: 4, gzip: 5 });

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
        let _app: AppWithGlobal<u32> = App::with_global(|_cfg| Ok(42u32))
            .route("/", |_req, g| {
                assert_eq!(*g, 42);
                Ok(Response::text("ok"))
            })
            .on_destroy(|v| assert_eq!(v, 42));
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
            |_cfg| Ok(Global { base: 10 }),
            |_cfg, g| Ok(Local { count: g.base }),
        )
        .route("/inc", |_req, g, t| {
            t.count += g.base;
            Ok(Response::text("ok"))
        })
        .on_destroy_thread(|l| { let _ = l; })
        .on_destroy(|g| { let _ = g; });
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
        let cookie_hdr = resp.headers.iter()
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

    // The two `verify_csrf` tests that used to live here have moved to
    // `crate::request`, next to the method they exercise.
    //
    // They had not compiled since Phase 4. Both built a `RawRequest` with
    // `query: String` and no `version`, which was m6-render's own type before
    // the shared one replaced it, and `run-tests.sh` runs `cargo test
    // --workspace` with default features, so `csrf` was never on and nothing
    // ever tried. This is the bench problem from Phase 0.2 again: code that is
    // never compiled is not covered by a green test run.
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
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for e in entries.flatten() {
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_dir() {
                walk(&e.path(), depth - 1, newest);
            } else if ft.is_file() {
                if let Some(t) = e.metadata().ok().and_then(|m| m.modified().ok()) {
                    if newest.map_or(true, |n| t > n) {
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

        let past = SystemTime::now() - Duration::from_secs(3600);
        let recent = SystemTime::now() - Duration::from_secs(60);
        filetime::set_file_mtime(d.path().join("old.html"), filetime::FileTime::from(past)).unwrap();
        filetime::set_file_mtime(d.path().join("new.html"), filetime::FileTime::from(recent)).unwrap();

        let got = newest_mtime_under(d.path()).expect("some mtime");
        let delta = got.duration_since(recent).unwrap_or_else(|e| e.duration());
        assert!(delta < Duration::from_secs(2), "expected the newer file's mtime");
    }

    /// Partials commonly live in a subdirectory; a change to one of those must
    /// still date the output.
    #[test]
    fn newest_mtime_descends_into_subdirectories() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("page.html"), b"a").unwrap();
        std::fs::create_dir(d.path().join("partials")).unwrap();
        std::fs::write(d.path().join("partials/_head.html"), b"b").unwrap();

        let past = SystemTime::now() - Duration::from_secs(7200);
        let recent = SystemTime::now() - Duration::from_secs(30);
        filetime::set_file_mtime(d.path().join("page.html"), filetime::FileTime::from(past)).unwrap();
        filetime::set_file_mtime(d.path().join("partials/_head.html"), filetime::FileTime::from(recent)).unwrap();

        let got = newest_mtime_under(d.path()).expect("some mtime");
        let delta = got.duration_since(recent).unwrap_or_else(|e| e.duration());
        assert!(delta < Duration::from_secs(2), "a nested partial should date the output");
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
            method: RouteMethod::Any,
            specificity: route_specificity(&segments),
            segments,
            template: None,
            params_files: Vec::new(),
            status: 200,
            cache: String::new(),
            headers: Vec::new(),
            last_modified: None,
            handler: None,
            settings: Arc::new(Map::new()),
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
    /// parameter is still exactly one segment. Every route in production is
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

    /// A literal beats a parameter beats a wildcard for the same path, so
    /// adding a catch-all to a config does not quietly capture the traffic of
    /// the exact routes sitting beside it.
    #[test]
    fn specificity_orders_literal_above_param_above_wildcard() {
        let lit = route("/assets/style.css").specificity;
        let par = route("/assets/{name}").specificity;
        let wild = route("/assets/{*rest}").specificity;
        assert!(lit > par, "literal {lit} should beat param {par}");
        assert!(par > wild, "param {par} should beat wildcard {wild}");
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
        writeln!(f, "[[route]]\npath = \"/assets/{{*relpath}}\"\ntemplate = \"x.html\"").unwrap();

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
        assert_eq!(dict["relpath"].as_str().unwrap(), "css/main.css");
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
        writeln!(f, "[[route]]\npath = \"/assets/{{*relpath}}\"\ntemplate = \"x.html\"").unwrap();
        let cfg = crate::config::load(f.path(), site_dir.path()).unwrap();
        let state =
            FrameworkState::build(cfg, site_dir.path().to_path_buf(), &[], &*default_renderer())
                .unwrap();

        // `..` never survives the matcher, so drive the check directly: these
        // are the values that would reach step 4 if it ever did.
        let route = &state.routes[0];
        assert!(route.is_wildcard_param("relpath"));
        assert!(!route.is_wildcard_param("stem"));

        for bad in ["../etc/passwd", "css/../../etc/passwd", "a b/c", "/leading", "trailing/"] {
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
    /// the service does at startup and again at every reload.
    fn state_from(toml: &str) -> (FrameworkState, tempfile::TempDir) {
        let site_dir = tempfile::TempDir::new().unwrap();
        std::fs::create_dir(site_dir.path().join("templates")).unwrap();
        let mut f = tempfile::NamedTempFile::new().unwrap();
        write!(f, "{toml}").unwrap();
        let cfg = crate::config::load(f.path(), site_dir.path()).unwrap();
        let state =
            FrameworkState::build(cfg, site_dir.path().to_path_buf(), &[], &*default_renderer())
                .unwrap();
        (state, site_dir)
    }

    fn registry(names: &[&str]) -> HashMap<String, Arc<BoxHandler>> {
        names
            .iter()
            .map(|n| {
                let h: BoxHandler = Box::new(|_req: &Request| Ok(Response::text("ok")));
                (n.to_string(), Arc::new(h))
            })
            .collect()
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
        let route = &state.routes[0];
        assert_eq!(route.handler.as_deref(), Some("files"));
        assert_eq!(route.pattern, "/assets/{*relpath}");
    }

    /// The point of the whole change, at the level the state owns it: the route
    /// table is rebuilt from config, so a config that gained a route produces a
    /// state that serves it, with the same handlers and no restart.
    ///
    /// `App` registers code routes once at startup. That is correct for a route
    /// that is part of the program and wrong for one that is part of the
    /// deployment, and it was the last thing standing between `m6-file` and
    /// `App`: its `handle_reload` rebuilds its route table today, so migrating
    /// as things stood would have quietly removed the ability to add an asset
    /// tree without restarting the service.
    #[test]
    fn a_reload_adds_a_route_that_the_previous_state_did_not_have() {
        use std::sync::RwLock;

        let (state1, _d) = state_from(FILES);
        let fs = Arc::new(RwLock::new(state1));

        // Before: nothing answers under /downloads.
        assert!(find_route("/downloads/report.pdf", "GET", &fs.read().unwrap().routes).is_none());

        let (state2, _d2) = state_from(&format!(
            "{FILES}\n[[route]]\npath = \"/downloads/{{*relpath}}\"\nhandler = \"files\"\nroot = \"files/\"\n"
        ));
        *fs.write().unwrap() = state2;

        // After: it does, and it is bound to the same handler the binary
        // already had.
        let guard = fs.read().unwrap();
        let (route, params) = find_route("/downloads/report.pdf", "GET", &guard.routes)
            .expect("the reloaded state should serve the new route");
        assert_eq!(route.handler.as_deref(), Some("files"));
        assert_eq!(route.settings.get("root").unwrap().as_str().unwrap(), "files/");
        assert_eq!(params[0].1, "report.pdf");

        // And the route that was already there is untouched.
        assert!(find_route("/assets/css/main.css", "GET", &guard.routes).is_some());
    }

    /// A handler name with no code behind it has no defensible reading, so it
    /// is refused rather than narrowed. The caller decides what that means:
    /// exit 2 at startup, and at reload a refusal that leaves the previous
    /// routes serving.
    #[test]
    fn a_route_naming_an_unregistered_handler_is_refused() {
        let (state, _d) = state_from(FILES);

        let missing = unknown_handlers(&state.routes, &registry(&["uploads"]));
        assert_eq!(missing, vec![("/assets/{*relpath}".to_string(), "files".to_string())]);

        // The operator needs to be told which route, which name, and what was
        // available. A bare "unknown handler" sends them reading config by eye.
        let described = describe_unknown(&missing, &["uploads".to_string()]);
        assert!(described.contains("/assets/{*relpath}"), "{described}");
        assert!(described.contains("`files`"), "{described}");
        assert!(described.contains("uploads"), "{described}");

        // Registered, and there is nothing to refuse.
        assert!(unknown_handlers(&state.routes, &registry(&["files"])).is_empty());
    }

    /// A template route is unaffected by any of this: it names no handler and
    /// is not subject to the check.
    #[test]
    fn a_template_route_names_no_handler() {
        let (state, _d) = state_from("[[route]]\npath = \"/\"\ntemplate = \"index.html\"\n");
        assert!(state.routes[0].handler.is_none());
        assert!(unknown_handlers(&state.routes, &HashMap::new()).is_empty());
    }

    /// Core does not know what `root` or `tail` mean, and does not need to.
    /// Keys it does not define are kept verbatim for the handler; keys it does
    /// define are consumed, not duplicated into the bag.
    #[test]
    fn keys_core_does_not_define_are_kept_for_the_handler() {
        let (state, _d) = state_from(FILES);
        let s = &state.routes[0].settings;
        assert_eq!(s.get("root").unwrap().as_str().unwrap(), "assets/");
        assert!(!s.get("tail").unwrap().as_bool().unwrap());
        for consumed in ["path", "handler", "template", "cache", "status", "methods", "headers"] {
            assert!(s.get(consumed).is_none(), "{consumed} is core's, not the handler's");
        }
    }

    /// A handler computes its answer; a template renders a document from files
    /// on disk. Letting the first inherit the second's `public` default would
    /// put dynamic output in a shared cache by omission.
    #[test]
    fn a_handler_route_defaults_to_no_store_and_an_explicit_cache_wins() {
        let (state, _d) = state_from(FILES);
        assert_eq!(state.routes[0].cache, "no-store");

        let (explicit, _d2) = state_from(
            "[[route]]\npath = \"/a/{*r}\"\nhandler = \"files\"\ncache = \"public, max-age=60\"\n",
        );
        assert_eq!(explicit.routes[0].cache, "public, max-age=60");

        let (template, _d3) = state_from("[[route]]\npath = \"/\"\ntemplate = \"index.html\"\n");
        assert_eq!(template.routes[0].cache, "public");
    }

    /// `Last-Modified` is derived from templates and params files, so it
    /// belongs to routes that render from them. The loop that computes it used
    /// to run over every route, including the code routes its own comment said
    /// were skipped, and hand a handler the newest template's mtime as the date
    /// of an answer computed per request.
    #[test]
    fn a_handler_route_carries_no_last_modified() {
        let (state, _d) = state_from(FILES);
        assert!(state.routes[0].last_modified.is_none());
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
            .with_route(&state.routes[0]);

        assert_eq!(req.route_pattern(), Some("/assets/{*relpath}"));
        assert_eq!(req.route_str("root"), Some("assets/"));
        assert!(!req.route_bool("tail", true), "tail = false in config");
        assert!(req.route_bool("absent", true), "an absent key takes the default");
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
}

#[cfg(test)]
mod dict_cost_probe {
    use super::*;
    use std::io::Write;

    /// What `App` spends per request before a handler is reached.
    ///
    /// Not an assertion, a measurement, printed with `--nocapture`. m6-file
    /// does none of this today: it matches a route and goes straight to the
    /// filesystem. If it becomes an `App` service, every asset request pays
    /// whatever this costs, and latency is the stated key metric.
    ///
    /// Measured 2026-09-12 on the laptop, release:
    /// **build_dict p50 3.08us, p99 5.25us; find_route p50 167ns.** syd is a
    /// 1-core VM and would be worse. The tracked production cache-hit p50 is
    /// 3.9us, so this is not a rounding error beside it.
    ///
    /// `#[ignore]`d deliberately. A timing assertion is the wall-clock trap
    /// that `test_static_file_cache_hit` already fell into once: it fires on a
    /// loaded build box against correct code. Run it when the question is
    /// asked:
    ///
    /// ```sh
    /// cargo test --release -p m6-core measure_build_dict -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore = "a measurement, not an assertion; see the doc comment"]
    fn measure_build_dict_for_a_static_asset_request() {
        let site_dir = tempfile::TempDir::new().unwrap();
        std::fs::create_dir(site_dir.path().join("templates")).unwrap();

        // A config shaped like the real one: a handful of site-wide keys that
        // every dict copies, plus the asset route.
        let mut cfg = String::new();
        for i in 0..20 {
            cfg.push_str(&format!("key_{i} = \"value_{i}\"\n"));
        }
        cfg.push_str("[[route]]\npath = \"/assets/{*relpath}\"\nhandler = \"files\"\nroot = \"assets/\"\n");
        let mut f = tempfile::NamedTempFile::new().unwrap();
        write!(f, "{cfg}").unwrap();

        let config = crate::config::load(f.path(), site_dir.path()).unwrap();
        let state =
            FrameworkState::build(config, site_dir.path().to_path_buf(), &[], &*default_renderer())
                .unwrap();

        let raw = RawRequest {
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
        };
        let (route, params) = find_route(raw.path(), raw.method(), &state.routes).unwrap();

        // Warm, then take the median of a decent run.
        for _ in 0..1000 {
            let _ = state.build_dict(&raw, route, &params).unwrap();
        }
        let mut samples = Vec::with_capacity(2000);
        for _ in 0..2000 {
            let t = std::time::Instant::now();
            let d = state.build_dict(&raw, route, &params).unwrap();
            samples.push(t.elapsed().as_nanos() as u64);
            std::hint::black_box(d);
        }
        samples.sort_unstable();
        let p50 = samples[samples.len() / 2];
        let p99 = samples[samples.len() * 99 / 100];

        // And the routing it replaces, for scale.
        let mut rsamples = Vec::with_capacity(2000);
        for _ in 0..2000 {
            let t = std::time::Instant::now();
            let m = find_route(raw.path(), raw.method(), &state.routes);
            rsamples.push(t.elapsed().as_nanos() as u64);
            std::hint::black_box(m);
        }
        rsamples.sort_unstable();

        println!(
            "build_dict per request: p50 {p50}ns p99 {p99}ns | find_route p50 {}ns",
            rsamples[rsamples.len() / 2]
        );
    }
}
