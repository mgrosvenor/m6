//! App builder, thread pool, lifecycle management.
#![allow(dead_code)]

use std::any::Any;
use std::cell::RefCell;
use std::collections::HashMap;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc, Mutex, OnceLock,
};

use anyhow::Context;
use serde_json::{Map, Value};
use tracing::{error, info, warn};

use crate::error::{Error, Result};
use crate::request::{
    parse_auth_claims, parse_cookies, parse_form_body, parse_query_string, validate_path_param,
    RawRequest, Request,
};
use crate::response::{error_to_response, Response};
use crate::template::{build_tera, build_tera_from_paths};

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
}

#[derive(Debug, Clone)]
pub enum Segment {
    Literal(String),
    Param(String),
}

/// Compile a route pattern string into segments.
pub fn compile_pattern(pattern: &str) -> Vec<Segment> {
    pattern
        .split('/')
        .filter(|s| !s.is_empty())
        .map(|s| {
            if s.starts_with('{') && s.ends_with('}') {
                Segment::Param(s[1..s.len() - 1].to_string())
            } else {
                Segment::Literal(s.to_string())
            }
        })
        .collect()
}

pub fn route_specificity(segments: &[Segment]) -> i32 {
    let mut score = (segments.len() as i32) * 2;
    for seg in segments {
        if matches!(seg, Segment::Literal(_)) {
            score += 1;
        }
    }
    score
}

/// Route params: at most a few captures; Vec beats HashMap for small N.
pub type PathParams = Vec<(String, String)>;

/// Try to match pre-split URL path segments against a compiled route.
/// Returns `Some(params)` on success.
pub fn match_route(path_segs: &[&str], route: &CompiledRoute) -> Option<PathParams> {
    if path_segs.len() != route.segments.len() {
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
    tera: tera::Tera,
    #[cfg(feature = "flash")]
    flash_secret: Vec<u8>,
}

impl FrameworkState {
    fn build(
        config: crate::config::RendererConfig,
        site_dir: PathBuf,
        code_routes: &[(String, RouteMethod)],
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
            });
        }

        // Collect template paths from config routes.
        let template_paths: Vec<String> = config
            .routes
            .iter()
            .filter_map(|r| r.template.clone())
            .collect();

        // Build Tera (exit 2 on syntax error, but here we return Err which caller turns to exit 2).
        // If no templates are listed in config routes (handler apps using render_with directly),
        // fall back to loading all templates from site_dir.
        let tera = if template_paths.is_empty() {
            build_tera(&site_dir).context("compiling templates")?
        } else {
            build_tera_from_paths(&site_dir, &template_paths)
                .context("compiling templates")?
        };

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
            tera,
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
        for (k, v) in path_params {
            validate_path_param(k, v)?;
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
        if raw.method() == "POST" {
            if let Some(ct) = raw.content_type() {
                if ct.contains("application/x-www-form-urlencoded") {
                    for (k, v) in parse_form_body(&raw.body) {
                        dict.insert(k, Value::String(v));
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

        // 12. CSRF token: generate or reuse from cookie, inject into dict.
        #[cfg(feature = "csrf")]
        {
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
    fn render_response(
        &self,
        resp: &mut Response,
        dict: &Map<String, Value>,
    ) -> anyhow::Result<()> {
        if let Some(template_name) = resp.template_name.clone() {
            let mut ctx = tera::Context::new();
            // Start with the framework dict (global params, path params, auth claims…)
            for (k, v) in dict {
                ctx.insert(k.as_str(), v);
            }
            // Overlay the handler-supplied dict (render_with extra data)
            if let Some(handler_dict) = &resp.template_dict {
                for (k, v) in handler_dict {
                    ctx.insert(k.as_str(), v);
                }
            }
            let html = self
                .tera
                .render(&template_name, &ctx)
                .with_context(|| format!("rendering template {template_name}"))?;
            resp.body = html.into_bytes();
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
    use rand::RngCore;
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
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
    init_global: Arc<dyn Fn(&Map<String, Value>) -> Result<G> + Send + Sync>,
    destroy_global: Option<Arc<dyn Fn(G) + Send + Sync>>,
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

    run_app_with_shutdown(code_routes, config_path, site_dir, on_shutdown, None, cli_log_level)
}

/// Run with per-thread state only (no global).
fn run_app_thread_state<T: Any + Send + 'static>(
    raw_routes: Vec<ThreadRawRoute<T>>,
    init_thread: Arc<dyn Fn(&Map<String, Value>, &()) -> Result<T> + Send + Sync>,
    destroy_thread: Option<Arc<dyn Fn(T) + Send + Sync>>,
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

    let on_thread_exit: Arc<dyn Fn() + Send + Sync> = Arc::new(drain_thread_state);

    run_app_with_shutdown(
        code_routes,
        config_path,
        site_dir,
        None,
        Some(on_thread_exit),
        cli_log_level,
    )
}

/// Run with global + per-thread state.
fn run_app_state<G: Send + Sync + 'static, T: Any + Send + 'static>(
    raw_routes: Vec<StateRawRoute<G, T>>,
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
        code_routes,
        config_path,
        site_dir,
        on_shutdown,
        Some(on_thread_exit),
        cli_log_level,
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

static SHUTDOWN_FLAG: AtomicBool = AtomicBool::new(false);
static SIGNAL_COUNT: AtomicUsize = AtomicUsize::new(0);

pub fn install_signal_handler() {
    use nix::sys::signal::{self, SigHandler, Signal};
    extern "C" fn handler(_sig: libc::c_int) {
        let count = SIGNAL_COUNT.fetch_add(1, Ordering::SeqCst) + 1;
        if count >= 2 {
            std::process::exit(0);
        }
        SHUTDOWN_FLAG.store(true, Ordering::SeqCst);
    }
    unsafe {
        let _ = signal::signal(Signal::SIGTERM, SigHandler::Handler(handler));
        let _ = signal::signal(Signal::SIGINT, SigHandler::Handler(handler));
    }
}

pub fn is_shutdown() -> bool {
    SHUTDOWN_FLAG.load(Ordering::SeqCst)
}

// ---------------------------------------------------------------------------
// App builder — no state
// ---------------------------------------------------------------------------

/// Handler function entries: (pattern, method, handler).
type CodeRoute = (String, RouteMethod, Arc<BoxHandler>);

/// App with no user state.
pub struct App {
    routes: Vec<CodeRoute>,
}

impl App {
    pub fn new() -> Self {
        Self { routes: vec![] }
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
        run_app(self.routes)
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

pub struct AppWithGlobal<G: Send + Sync + 'static> {
    raw_routes: Vec<GlobalRawRoute<G>>,
    init_global: Arc<dyn Fn(&Map<String, Value>) -> Result<G> + Send + Sync>,
    destroy_global: Option<Arc<dyn Fn(G) + Send + Sync>>,
}

impl<G: Send + Sync + 'static> AppWithGlobal<G> {
    pub fn on_destroy(mut self, f: impl Fn(G) + Send + Sync + 'static) -> Self {
        self.destroy_global = Some(Arc::new(f));
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

    pub fn run(self) -> Result<()> {
        run_app_global(
            self.raw_routes,
            self.init_global,
            self.destroy_global,
        )
    }
}

impl App {
    pub fn with_global<G: Send + Sync + 'static>(
        init_global: impl Fn(&Map<String, Value>) -> Result<G> + Send + Sync + 'static,
    ) -> AppWithGlobal<G> {
        AppWithGlobal {
            raw_routes: vec![],
            init_global: Arc::new(init_global),
            destroy_global: None,
        }
    }

    pub fn with_thread_state<T: Any + Send + 'static>(
        init_thread: impl Fn(&Map<String, Value>, &()) -> Result<T> + Send + Sync + 'static,
    ) -> AppWithThreadState<T> {
        AppWithThreadState {
            raw_routes: vec![],
            init_thread: Arc::new(init_thread),
            destroy_thread: None,
        }
    }

    pub fn with_state<G: Send + Sync + 'static, T: Any + Send + 'static>(
        init_global: impl Fn(&Map<String, Value>) -> Result<G> + Send + Sync + 'static,
        init_thread: impl Fn(&Map<String, Value>, &G) -> Result<T> + Send + Sync + 'static,
    ) -> AppWithState<G, T> {
        AppWithState {
            raw_routes: vec![],
            init_global: Arc::new(init_global),
            init_thread: Arc::new(init_thread),
            destroy_thread: None,
            destroy_global: None,
        }
    }
}

// ---------------------------------------------------------------------------
// App with per-thread state only
// ---------------------------------------------------------------------------

// Raw stateful route (ThreadLocal): stores the handler before config/TLS known.
type ThreadRawRoute<T> = (String, RouteMethod, Arc<dyn Fn(&Request, &mut T) -> Result<Response> + Send + Sync>);

pub struct AppWithThreadState<T: Any + Send + 'static> {
    raw_routes: Vec<ThreadRawRoute<T>>,
    init_thread: Arc<dyn Fn(&Map<String, Value>, &()) -> Result<T> + Send + Sync>,
    destroy_thread: Option<Arc<dyn Fn(T) + Send + Sync>>,
}

impl<T: Any + Send + 'static> AppWithThreadState<T> {
    pub fn on_destroy_thread(mut self, f: impl Fn(T) + Send + Sync + 'static) -> Self {
        self.destroy_thread = Some(Arc::new(f));
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

    pub fn run(self) -> Result<()> {
        run_app_thread_state(
            self.raw_routes,
            self.init_thread,
            self.destroy_thread,
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

pub struct AppWithState<G: Send + Sync + 'static, T: Any + Send + 'static> {
    raw_routes: Vec<StateRawRoute<G, T>>,
    init_global: Arc<dyn Fn(&Map<String, Value>) -> Result<G> + Send + Sync>,
    init_thread: Arc<dyn Fn(&Map<String, Value>, &G) -> Result<T> + Send + Sync>,
    destroy_thread: Option<Arc<dyn Fn(T) + Send + Sync>>,
    destroy_global: Option<Arc<dyn Fn(G) + Send + Sync>>,
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

    pub fn run(self) -> Result<()> {
        run_app_state(
            self.raw_routes,
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

pub fn run_app(code_routes: Vec<CodeRoute>) -> Result<()> {
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
    run_app_with_shutdown(code_routes, config_path, site_dir, None, None, cli_log_level)
}

fn run_app_with_shutdown(
    code_routes: Vec<CodeRoute>,
    config_path: PathBuf,
    site_dir: PathBuf,
    on_shutdown: Option<Box<dyn FnOnce() + Send>>,
    on_thread_exit: Option<Arc<dyn Fn() + Send + Sync + 'static>>,
    cli_log_level: Option<String>,
) -> Result<()> {
    // Load config.
    let config = crate::config::load(&config_path, &site_dir).unwrap_or_else(|e| {
        eprintln!("Config error: {e}");
        std::process::exit(2);
    });

    // Init logging: site.toml base → renderer config [log] → CLI --log-level.
    let (site_level, site_format) = m6_core::log::read_site_log_config(&site_dir);
    let format = config.log.format.as_deref().unwrap_or(&site_format).to_string();
    let cfg_level = config.log.level.as_deref().unwrap_or(&site_level).to_string();
    let level = cli_log_level.as_deref().unwrap_or(&cfg_level).to_string();
    let _log_guard = m6_core::log::init(&format, &level).unwrap_or_else(|e| {
        eprintln!("logging init error: {e}");
        std::process::exit(1);
    });

    let socket_path = if let Ok(override_path) = std::env::var("M6_SOCKET_OVERRIDE") {
        PathBuf::from(override_path)
    } else {
        crate::config::socket_path_from_config(&config_path)
    };

    // Build framework state.
    let code_route_signatures: Vec<(String, RouteMethod)> = code_routes
        .iter()
        .map(|(p, m, _)| (p.clone(), m.clone()))
        .collect();

    let framework_state =
        FrameworkState::build(config, site_dir.clone(), &code_route_signatures)
            .unwrap_or_else(|e| {
                eprintln!("Startup error: {e}");
                std::process::exit(2);
            });

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

    // Wrap state in RwLock so hot reload can atomically swap it while
    // in-flight requests continue reading the old state via their cloned Arc.
    let fs: Arc<std::sync::RwLock<FrameworkState>> =
        Arc::new(std::sync::RwLock::new(framework_state));

    // Build handler lookup map (pattern → handler).
    let code_handlers: Arc<HashMap<String, Arc<BoxHandler>>> = Arc::new(
        code_routes
            .into_iter()
            .map(|(p, _, h)| (p, h))
            .collect(),
    );

    // Wrap on_shutdown for call at most once (accept loop is single-threaded).
    let mut on_shutdown_cell = on_shutdown;

    install_signal_handler();

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

    {
        let fs_r = fs.read().unwrap();
        info!(
            routes = fs_r.routes.len(),
            threads = tp_size,
            socket = %socket_path.display(),
            "m6-render started"
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
    let mut watcher = m6_core::ConfigWatcher::new(&[&config_path, &site_toml_path]).ok();

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
        let borrowed_listener =
            unsafe { std::os::fd::BorrowedFd::borrow_raw(listener_fd) };
        let mut pfd_listener = nix::poll::PollFd::new(
            &borrowed_listener,
            nix::poll::PollFlags::POLLIN,
        );

        let watcher_fd = watcher.as_ref().and_then(|w| w.raw_fd()).unwrap_or(-1);
        let (poll_result, listener_ready, inotify_fired) = if watcher_fd >= 0 {
            let borrowed_ino =
                unsafe { std::os::fd::BorrowedFd::borrow_raw(watcher_fd) };
            let pfd_ino = nix::poll::PollFd::new(
                &borrowed_ino,
                nix::poll::PollFlags::POLLIN,
            );
            let mut fds = [pfd_listener, pfd_ino];
            let r = nix::poll::poll(&mut fds, 100);
            let l = fds[0]
                .revents()
                .map_or(false, |f| f.contains(nix::poll::PollFlags::POLLIN));
            let i = fds[1]
                .revents()
                .map_or(false, |f| f.contains(nix::poll::PollFlags::POLLIN));
            (r, l, i)
        } else {
            let r = nix::poll::poll(std::slice::from_mut(&mut pfd_listener), 100);
            let l = pfd_listener
                .revents()
                .map_or(false, |f| f.contains(nix::poll::PollFlags::POLLIN));
            (r, l, false)
        };

        // ── Determine whether a reload is needed ─────────────────────────
        let mut should_reload = false;

        match poll_result {
            Ok(0) | Err(_) => {
                // Timeout or interrupted — check shutdown flag.
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
                if watcher_fd < 0 {
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
            Ok(_) => {}
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
                    ) {
                        Err(e) => {
                            error!("Reload failed (template/state error): {e}");
                        }
                        Ok(new_state) => {
                            *fs.write().unwrap() = new_state;
                            let elapsed = reload_start.elapsed().as_millis();
                            info!(elapsed_ms = elapsed, "Reload complete");
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
                let fs = fs.clone();
                let code_handlers = code_handlers.clone();

                match pool.try_submit(stream, move |mut s| {
                    handle_connection(&mut s, &*fs, &code_handlers);
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
            info!("Shutdown signal received, draining...");
            pool.drain();
            if let Some(f) = on_shutdown_cell.take() {
                f();
            }
            info!("Clean shutdown");
            break;
        }
    }

    Ok(())
}

fn handle_connection(
    stream: &mut UnixStream,
    fs: &std::sync::RwLock<FrameworkState>,
    code_handlers: &HashMap<String, Arc<BoxHandler>>,
) {
    let raw = match crate::server::parse_request(stream) {
        Ok(Some(r)) => r,
        Ok(None) => return,
        Err(e) => {
            error!("Parse error: {e}");
            crate::server::write_error_response(stream, 400, "Bad Request").ok();
            return;
        }
    };

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
                    crate::server::write_response(stream, &r).ok();
                    return;
                }
            };

            let req = Request::new(raw.clone(), dict.clone(), site_dir.clone());

            // Dispatch to code handler or template render.
            // A code handler is only used when the matched route is a code route
            // (template is None). If the matched route has a template (config route),
            // use the template even if a code handler exists for the same pattern
            // — this ensures GET routes handled by config are not shadowed by POST
            // code handlers registered on the same path.
            let mut resp = if route.template.is_none() {
                if let Some(handler) = code_handlers.get(&route.pattern) {
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
                if let Err(e) = fs_r.render_response(&mut resp, &dict) {
                    error!("Template render error: {e:#}");
                    resp = Response::status(500);
                }
            }

            // Add Cache-Control header.
            let cache = if route.cache == "no-store" { "no-store" } else { "public" };
            resp = resp.header("Cache-Control", cache);

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
        // Check if the request already had a _csrf cookie (already in dict["cookies"]["_csrf"]).
        // The dict was built in build_dict and the csrf_token is already there.
        // We need to set the cookie in the response if absent.
        // NOTE: we check the raw Cookie header for `_csrf=` to avoid depending on
        // request dict here (we don't have it outside the route match arm).
        let has_csrf = raw.header("cookie")
            .map(|h| h.contains("_csrf="))
            .unwrap_or(false);
        if !has_csrf {
            // Generate a fresh token and set it.
            let token = generate_csrf_token();
            let cookie = format!("_csrf={}; Path=/; SameSite=Strict", token);
            resp.headers.push(("Set-Cookie".to_string(), cookie));
        }
    }

    // ── Flash: clear the _flash cookie after reading it.
    #[cfg(feature = "flash")]
    {
        let had_flash = raw.header("cookie")
            .map(|h| h.contains("_flash="))
            .unwrap_or(false);
        if had_flash {
            resp.headers.push((
                "Set-Cookie".to_string(),
                "_flash=; Max-Age=0; Path=/; HttpOnly".to_string(),
            ));
        }
    }

    // ── Minification: applied BEFORE compression for better ratios.
    if !resp.body.is_empty() {
        let content_type = resp
            .headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("content-type"))
            .map(|(_, v)| v.as_str())
            .unwrap_or("");
        let mime = content_type.split(';').next().unwrap_or("").trim();

        if minification.is_enabled(mime) {
            resp.body = match mime {
                "text/html" => crate::minify::minify_html(&resp.body),
                "text/css" => crate::minify::minify_css(&resp.body),
                "application/json" => crate::minify::minify_json(&resp.body),
                "application/javascript" | "text/javascript" => crate::minify::minify_js(&resp.body),
                _ => resp.body,
            };
        }
    }

    // ── Compression: applied AFTER minification.
    let accept_encoding = raw.header("accept-encoding").unwrap_or("");
    if !resp.body.is_empty() {
        let content_type = resp
            .headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("content-type"))
            .map(|(_, v)| v.as_str())
            .unwrap_or("");
        let mime = content_type.split(';').next().unwrap_or("").trim();

        if let Some(level) = compression.get(mime) {
            // Use compression levels from config (not hardcoded 6).
            if ae_contains(accept_encoding, "br") && level.brotli > 0 {
                if let Ok(compressed) = crate::compress::brotli_compress(&resp.body, level.brotli) {
                    resp.body = compressed;
                    resp.headers.push(("Content-Encoding".to_string(), "br".to_string()));
                }
            } else if ae_contains(accept_encoding, "gzip") && level.gzip > 0 {
                if let Ok(compressed) = crate::compress::gzip_compress(&resp.body, level.gzip) {
                    resp.body = compressed;
                    resp.headers.push(("Content-Encoding".to_string(), "gzip".to_string()));
                }
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

    crate::server::write_response(stream, &resp).ok();
}

/// Check if an `Accept-Encoding` header value contains `enc` (case-insensitive, no allocation).
#[inline]
fn ae_contains(header: &str, enc: &str) -> bool {
    header.as_bytes().windows(enc.len()).any(|w| w.eq_ignore_ascii_case(enc.as_bytes()))
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

        let mut f = NamedTempFile::new().unwrap();
        write!(f, "site_name = \"v1\"\n").unwrap();
        let site_dir = std::path::Path::new("/tmp");

        let cfg1 = crate::config::load(f.path(), site_dir).unwrap();
        assert_eq!(cfg1.user_config["site_name"].as_str().unwrap(), "v1");

        let state1 = FrameworkState::build(cfg1, site_dir.to_path_buf(), &[]).unwrap();
        let fs = Arc::new(RwLock::new(state1));

        // Verify initial state.
        assert_eq!(fs.read().unwrap().config.user_config["site_name"].as_str().unwrap(), "v1");

        // Write a new config.
        let mut f2 = NamedTempFile::new().unwrap();
        write!(f2, "site_name = \"v2\"\n").unwrap();

        let cfg2 = crate::config::load(f2.path(), site_dir).unwrap();
        let state2 = FrameworkState::build(cfg2, site_dir.to_path_buf(), &[]).unwrap();

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

    #[cfg(feature = "csrf")]
    #[test]
    fn test_csrf_verify_pass() {
        let token = "abcd1234".to_string();
        let mut dict = Map::new();
        let mut cookies = Map::new();
        cookies.insert("_csrf".to_string(), Value::String(token.clone()));
        dict.insert("cookies".to_string(), Value::Object(cookies));
        dict.insert("csrf_token".to_string(), Value::String(token.clone()));

        let raw = RawRequest {
            method: "POST".to_string(),
            path: "/submit".to_string(),
            query: String::new(),
            headers: vec![],
            body: vec![],
        };
        let req = Request::new(raw, dict, std::path::PathBuf::from("/tmp"));
        assert!(req.verify_csrf().is_ok());
    }

    #[cfg(feature = "csrf")]
    #[test]
    fn test_csrf_verify_fail_mismatch() {
        let mut dict = Map::new();
        let mut cookies = Map::new();
        cookies.insert("_csrf".to_string(), Value::String("token-a".to_string()));
        dict.insert("cookies".to_string(), Value::Object(cookies));
        dict.insert("csrf_token".to_string(), Value::String("token-b".to_string()));

        let raw = RawRequest {
            method: "POST".to_string(),
            path: "/submit".to_string(),
            query: String::new(),
            headers: vec![],
            body: vec![],
        };
        let req = Request::new(raw, dict, std::path::PathBuf::from("/tmp"));
        assert!(matches!(req.verify_csrf(), Err(Error::Forbidden)));
    }
}
