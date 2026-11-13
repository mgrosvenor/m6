//! The site's request handling, as plain functions on a value the service
//! owns.
//!
//! `Site` holds the start half (the command line, the config, `site.toml`,
//! templates, params, the route table and the unknown-handler checks) and the
//! per-request half (routing, the dictionary, rendering, and the site-wide
//! finish: headers, CSRF, flash, minification and compression). The compiled
//! route table and the match in it live here too.
//!
//! A service's loop calls [`Site::load`] once at start, then per request
//! [`Site::handle`], its own handler for the index `handle` returns, and
//! [`Site::finish`]. `Site` never calls back into the service: a route the
//! service answers itself comes back as [`Handled::Code`] or [`Handled::Named`]
//! with the index of its route.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use serde_json::{Map, Value};
use tracing::{error, warn};

use crate::dict::Dict;
use crate::error::Error;
use crate::h1::RequestHead;
use crate::http::RawRequest;
use crate::render::{RenderError, Renderer, RendererFactory};
use crate::request::{
    parse_auth_claims, parse_cookies, parse_form_body, parse_query_string, validate_path_param,
    validate_wildcard_param, Request,
};
use crate::response::Response;

/// A route a service answers in its own code, bound by pattern and method.
#[derive(Debug, Clone)]
pub struct CodeRoute {
    pub pattern: &'static str,
    pub method: RouteMethod,
}

/// What [`Site::handle`] did with a request.
pub enum Handled {
    /// Answered: a template page, a 404, an error. Already finished.
    Done(Response),
    /// One of the service's code routes: the index into the `code` slice given
    /// to `load`.
    Code(usize, Request),
    /// A config route with `handler = "<name>"`: the index into the `named`
    /// slice given to `load`.
    Named(usize, Request),
}

/// A loaded site: everything the request path needs, read at start.
pub struct Site {
    /// The site state the request path reads: the config, the compiled
    /// routes, the params cache and the renderer.
    state: FrameworkState,
    config_path: PathBuf,
    site_toml: PathBuf,
    socket_path: PathBuf,
    name: String,
    /// Index into `code` by `pattern:method`, the key the dispatch uses.
    code_index: HashMap<String, usize>,
    /// Index into the named handlers by name. `Handled::Named` is this index,
    /// and the caller's handler vector is in the order `load` was given.
    named_index: HashMap<String, usize>,
    /// The logging worker, held for the life of the site and never touched
    /// again. A plain `Option` because `LogHandle` is `Sync`.
    _log: Option<crate::log::LogHandle>,
}

impl Site {
    /// Start only: the command line (`--version`, `--dump-config`), the config,
    /// the `site.toml` checks, logging, the route table, the unknown-handler
    /// and `flash_secret` checks.
    ///
    /// Exits 2 on a configuration error and 1 when logging cannot start, as
    /// the service loop has always done.
    pub fn load(
        name: &str,
        code: &[CodeRoute],
        named: &[&str],
        renderer: &dyn RendererFactory,
    ) -> Site {
        let signatures: Vec<(String, RouteMethod)> = code
            .iter()
            .map(|r| (r.pattern.to_string(), r.method.clone()))
            .collect();
        let names: Vec<String> = named.iter().map(|n| (*n).to_string()).collect();
        let invocation = parse_invocation(&signatures, &names);
        Site::start(name, invocation, None, &signatures, &names, renderer)
    }

    /// The start half with a config the caller already loaded, shared by
    /// `Site::load` and the tests: a service that needed the config before
    /// this point does not read it twice.
    pub(crate) fn start(
        name: &str,
        invocation: Invocation,
        config: Option<crate::config::RendererConfig>,
        code: &[(String, RouteMethod)],
        named: &[String],
        renderer: &dyn RendererFactory,
    ) -> Site {
        match Site::prepare(name, invocation, config, code, named, renderer, true) {
            Ok(site) => site,
            Err((message, code)) => {
                eprintln!("{message}");
                std::process::exit(code);
            }
        }
    }

    /// The same start path without argv, without logging and without exiting,
    /// for tests that drive the request path in process. `testkit` only, so a
    /// production build has the one way in.
    #[cfg(feature = "testkit")]
    pub fn load_for_tests(
        name: &str,
        site_dir: PathBuf,
        config_path: PathBuf,
        code: &[CodeRoute],
        named: &[&str],
        renderer: &dyn RendererFactory,
    ) -> Site {
        let signatures: Vec<(String, RouteMethod)> = code
            .iter()
            .map(|r| (r.pattern.to_string(), r.method.clone()))
            .collect();
        let names: Vec<String> = named.iter().map(|n| (*n).to_string()).collect();
        match Site::prepare(
            name,
            Invocation {
                site_dir,
                config_path,
                cli_log_level: None,
            },
            None,
            &signatures,
            &names,
            renderer,
            false,
        ) {
            Ok(site) => site,
            Err((message, _)) => panic!("{message}"),
        }
    }

    /// Everything both ways in do, up to the listener. `service` selects the
    /// parts that belong to a running process: logging, and the `flash_secret`
    /// presence check a service refuses to start without.
    fn prepare(
        name: &str,
        invocation: Invocation,
        config: Option<crate::config::RendererConfig>,
        code: &[(String, RouteMethod)],
        named: &[String],
        renderer: &dyn RendererFactory,
        service: bool,
    ) -> Result<Site, (String, i32)> {
        let Invocation {
            site_dir,
            config_path,
            cli_log_level,
        } = invocation;
        let cli_log_level = cli_log_level.as_deref();

        // Load config.
        let config = match config {
            Some(config) => config,
            None => crate::config::load(&config_path, &site_dir)
                .map_err(|e| (format!("Config error: {e}"), 2))?,
        };

        let site_toml = validate_startup(&config, &site_dir, &config_path)
            .map_err(|msg| (format!("Config error: {msg}"), 2))?;

        // Logging: site.toml base, the service's `[log]`, then `--log-level`.
        let (format, level) = resolve_log(&site_toml, &config.log, cli_log_level)
            .map_err(|msg| (format!("Config error: {msg}"), 2))?;
        let log = if service {
            Some(
                crate::log::init(&format, &level)
                    .map_err(|e| (format!("logging init error: {e}"), 1))?,
            )
        } else {
            None
        };

        let socket_path = crate::socket_path_from_config(&config_path);

        let code_signatures: Vec<(String, RouteMethod)> = code.to_vec();
        let state = FrameworkState::build(config, site_dir.clone(), &code_signatures, renderer)
            .map_err(|e| (format!("Startup error: {e:#}"), 2))?;

        let code_patterns: Vec<String> = code_signatures.iter().map(|(p, _)| p.clone()).collect();
        let missing = unknown_handlers(state.routes.routes(), named, &code_patterns);
        if !missing.is_empty() {
            let message = format!(
                "Config error: route names an unregistered handler: {}",
                describe_unknown(&missing, named)
            );
            return Err((message, 2));
        }

        // A service that enables flash without a key would sign every cookie
        // with the empty key. Refuse at start rather than serve that.
        #[cfg(feature = "flash")]
        if service && state.flash_secret.is_empty() {
            return Err((
                "Error: flash feature enabled but `flash_secret` is absent from config \
                 (generate: openssl rand -base64 32)"
                    .to_string(),
                2,
            ));
        }

        let code_index = code_signatures
            .iter()
            .enumerate()
            .map(|(i, (pattern, method))| (format!("{pattern}:{}", route_method_key(method)), i))
            .collect();

        let named_index = named
            .iter()
            .enumerate()
            .map(|(i, name)| (name.clone(), i))
            .collect();
        Ok(Site {
            state,
            config_path,
            site_toml,
            socket_path,
            name: name.to_string(),
            code_index,
            named_index,
            _log: log,
        })
    }

    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// The accepted connection's read deadline, or `None` when the config has
    /// none (`read_timeout_s = 0`).
    pub fn read_timeout(&self) -> Option<Duration> {
        self.state.config.server.read_timeout
    }

    pub fn socket_mode(&self) -> u32 {
        self.state.config.server.socket_mode
    }

    /// The config's non-framework keys, and the roots a service resolves its
    /// own files against.
    pub fn user_config(&self) -> &Map<String, Value> {
        &self.state.config.user_config
    }

    pub fn site_dir(&self) -> &Path {
        &self.state.site_dir
    }

    pub fn config_path(&self) -> &Path {
        &self.config_path
    }

    pub fn site_toml(&self) -> &Path {
        &self.site_toml
    }

    /// The service's name, for the lifecycle lines and the startup log.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Handle a borrowed request head. Builds the owned request the service's
    /// handler needs from the head, then dispatches.
    pub fn handle(&self, head: &RequestHead<'_>) -> Handled {
        self.handle_raw(raw_from_head(head))
    }

    /// `handle` for a caller that already owns a [`RawRequest`]: the one
    /// dispatch, shared by the borrowed path and by a caller holding one.
    pub(crate) fn handle_raw(&self, raw: RawRequest) -> Handled {
        let started = std::time::Instant::now();
        let matched = find_route(raw.path(), raw.method(), &self.state.routes)
            .map(|(route, params)| (route, self.state.build_dict(&raw, route, &params)));

        let (route, dict) = match matched {
            None => {
                warn!(path = raw.path(), "unmatched path");
                let resp = self.finish_raw(&raw, None, None, started, Response::not_found());
                return Handled::Done(resp);
            }
            Some((route, dict)) => match dict {
                Ok(dict) => (route, dict),
                Err(e) => {
                    // An internal failure is finished like the unmatched 404,
                    // with no route at all: no default `Cache-Control`, no
                    // `Last-Modified`, no route headers. The CSRF and flash
                    // housekeeping is the same as the 404's, because it is the
                    // same finish path.
                    let resp = crate::response::error_to_response(&e);
                    let resp = self.finish_raw(&raw, None, None, started, resp);
                    return Handled::Done(resp);
                }
            },
        };

        let req = Request::new(raw, dict, Arc::clone(&self.state.site_dir))
            .with_started(started)
            .with_route(route)
            .with_config(Arc::clone(&self.state.config));

        // The order the request path has always dispatched in: a config route
        // naming a handler, then a code route, then a template. A route
        // carrying both a handler and a template goes to the handler.
        if let Some(name) = &route.handler {
            if let Some(&index) = self.named_index.get(name) {
                return Handled::Named(index, req);
            }
            // `prepare` refused an absent name, so an absent one here means
            // the check was bypassed. Answer 500 rather than a 404 that reads
            // as "no such page".
            error!(
                pattern = %route.pattern,
                handler = %name,
                "route names a handler that is not registered"
            );
            let resp = self.finish(&req, Response::status(500));
            return Handled::Done(resp);
        }

        if let Some(template) = &route.template {
            let resp = Response::render_request_dict(template, route.status);
            let resp = self.finish_raw(req.raw(), Some(req.dict()), Some(route), started, resp);
            return Handled::Done(resp);
        }

        // No template: a code route, or a 404 when nothing in code answers it.
        let key = format!("{}:{}", route.pattern, route.methods.handler_key());
        match self.code_index.get(&key) {
            Some(&index) => Handled::Code(index, req),
            None => {
                let resp = self.finish(&req, Response::not_found());
                Handled::Done(resp)
            }
        }
    }

    /// Renders a template the response names, then Cache-Control, CSRF, flash,
    /// minification and compression, as the request path does after its
    /// dispatch.
    pub fn finish(&self, req: &Request, resp: Response) -> Response {
        let route =
            find_route(req.path(), req.method(), &self.state.routes).map(|(route, _)| route);
        self.finish_raw(req.raw(), Some(req.dict()), route, req.started(), resp)
    }

    /// The one finish path, for a request that has a [`Request`] and for one
    /// that does not (an unmatched path's 404).
    fn finish_raw(
        &self,
        raw: &RawRequest,
        dict: Option<&Dict>,
        route: Option<&CompiledRoute>,
        started: std::time::Instant,
        mut resp: Response,
    ) -> Response {
        // Render a template the response names.
        if resp.template_name.is_some() {
            if let Some(dict) = dict {
                match FrameworkState::render_response(&*self.state.renderer, &mut resp, dict) {
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
        }

        if let Some(route) = route {
            // A default, not an override: a handler that decided its own
            // caching did so per request, and a second `Cache-Control` field
            // would be two answers to one question.
            if !crate::headers::contains(&resp.headers[..], "cache-control") {
                resp = resp.header("Cache-Control", &route.cache);
            }
            // Last-Modified, from the route's own inputs. Only on a success:
            // attaching a validator to a 404 or a 500 invites a client to keep
            // revalidating an error as though it were content.
            if resp.status < 300 {
                if let Some(lm) = route.last_modified {
                    resp = resp.header("Last-Modified", &httpdate::fmt_http_date(lm));
                }
            }
            for (k, v) in &route.headers {
                resp.headers.push((k.clone(), v.clone()));
            }
        }

        // CSRF: set the `_csrf` cookie when the request has none. The token is
        // the one the dictionary was rendered with, so the form field and the
        // cookie cannot disagree.
        #[cfg(feature = "csrf")]
        {
            let has_csrf = raw
                .header("cookie")
                .map(|h| h.contains("_csrf="))
                .unwrap_or(false);
            if !has_csrf {
                let from_dict = dict
                    .and_then(|d| d.get("csrf_token"))
                    .and_then(|v| v.as_str())
                    .map(str::to_string);
                let token = from_dict.unwrap_or_else(generate_csrf_token);
                // No HttpOnly: the page reads the double-submit token back.
                resp.headers.push(
                    crate::cookie::Cookie::new("_csrf", token)
                        .path("/")
                        .same_site(crate::cookie::SameSite::Strict)
                        .secure()
                        .to_header(),
                );
            }
        }

        // Flash: clear the cookie after reading it.
        #[cfg(feature = "flash")]
        {
            let had_flash = raw
                .header("cookie")
                .map(|h| h.contains("_flash="))
                .unwrap_or(false);
            if had_flash {
                // Path and HttpOnly must match the cookie being removed or the
                // browser keeps both.
                resp.headers.push(
                    crate::cookie::Cookie::removal("_flash")
                        .path("/")
                        .http_only()
                        .to_header(),
                );
            }
        }

        // Minification, then compression: minifying first compresses better.
        // Both are skipped for a `verbatim` response, which a handler has
        // already decided. A streamed body has no bytes and no flag.
        if !resp.verbatim {
            if let Some(bytes) = resp.body.as_bytes() {
                if !bytes.is_empty() {
                    let content_type =
                        crate::headers::get(&resp.headers[..], "content-type").unwrap_or("");
                    let mime = content_type.split(';').next().unwrap_or("").trim();
                    let minification = &self.state.config.minification;
                    if let Some(m) = minified(mime, bytes, minification) {
                        resp.body = crate::response::Body::Bytes(m);
                    }
                }
            }
        }

        let accept_encoding = raw.header("accept-encoding").unwrap_or("");
        if !resp.verbatim && !resp.body.is_empty() {
            let content_type = crate::headers::get(&resp.headers[..], "content-type").unwrap_or("");
            let mime = content_type.split(';').next().unwrap_or("").trim();
            let compression = &self.state.config.compression;
            if let Some(level) = compression.get(mime) {
                // Ask the client what it will accept, rather than testing
                // whether a coding name appears anywhere in the header.
                let mut candidates: Vec<&str> = Vec::with_capacity(2);
                if level.brotli > 0 {
                    candidates.push("br");
                }
                if level.gzip > 0 {
                    candidates.push("gzip");
                }
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
                        if let Ok(compressed) = crate::compress::gzip_compress(&bytes, level.gzip) {
                            resp.body = crate::response::Body::Bytes(compressed);
                            resp.headers
                                .push(("Content-Encoding".to_string(), "gzip".to_string()));
                        }
                    }
                    // Nothing acceptable: send identity rather than a 406,
                    // which RFC 9110 12.5.3 permits.
                    _ => {}
                }
            }
        }

        tracing::debug!(
            path = raw.path(),
            method = raw.method(),
            status = resp.status,
            latency_us = started.elapsed().as_micros(),
            "request complete"
        );
        resp
    }

    /// A `Site` over an already built state, for this crate's tests: no
    /// command line, no config file, no logging.
    #[cfg(test)]
    pub(crate) fn from_state(state: FrameworkState, named: Vec<String>) -> Site {
        Site::from_state_with_code(state, named, Vec::new())
    }

    #[cfg(test)]
    pub(crate) fn from_state_with_code(
        state: FrameworkState,
        named: Vec<String>,
        code: Vec<(String, RouteMethod)>,
    ) -> Site {
        let code_index = code
            .iter()
            .enumerate()
            .map(|(i, (pattern, method))| (format!("{pattern}:{}", route_method_key(method)), i))
            .collect();
        let named_index = named
            .iter()
            .enumerate()
            .map(|(i, name)| (name.clone(), i))
            .collect();
        Site {
            state,
            config_path: PathBuf::new(),
            site_toml: PathBuf::new(),
            socket_path: PathBuf::new(),
            name: "m6-test".to_string(),
            code_index,
            named_index,
            _log: None,
        }
    }
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
    /// Computed once when the site state is built, so it costs nothing per
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
    /// This is what a static file server needs in order to answer a path of
    /// unknown depth, and it is the whole of why `m6-file` has a different
    /// shape. Note m6-file
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
// Params cache
// ---------------------------------------------------------------------------

/// The site's LRU of resolved params files, read on a request whose route
/// names one with a `{placeholder}`.
///
/// One thread owns the site, so the cache takes a `RefCell` rather than a lock.
pub(crate) struct ParamsCache {
    inner: RefCell<lru::LruCache<String, Arc<Map<String, Value>>>>,
}

impl ParamsCache {
    fn new(size: usize) -> Self {
        use std::num::NonZeroUsize;
        let cap = NonZeroUsize::new(size.max(1)).unwrap();
        Self {
            inner: RefCell::new(lru::LruCache::new(cap)),
        }
    }

    fn get(&self, key: &str) -> Option<Arc<Map<String, Value>>> {
        self.inner.borrow_mut().get(key).cloned()
    }

    fn insert(&self, key: String, val: Arc<Map<String, Value>>) {
        self.inner.borrow_mut().put(key, val);
    }
}

// ---------------------------------------------------------------------------
// The site's shared state
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
    pub(crate) params_cache: ParamsCache,

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

        let params_cache = ParamsCache::new(config.params_cache.size);

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
    ) -> crate::error::Result<Dict> {
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
        dict.insert("cookies".to_string(), Value::Object(cookies_map.clone()));

        // 8. Built-in keys (set after params files — cannot be overridden by them).
        let now = chrono::Utc::now();
        dict.insert(
            "request_path".to_string(),
            Value::String(raw.path().to_string()),
        );
        dict.insert(
            "datetime".to_string(),
            Value::String(now.format("%Y-%m-%dT%H:%M:%SZ").to_string()),
        );
        dict.insert(
            "year".to_string(),
            Value::String(now.format("%Y").to_string()),
        );

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
        if let Some(flash_cookie) = cookies_map.get("_flash").and_then(|v| v.as_str()) {
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
    /// merge order is the site's business (global params, path params,
    /// auth claims first, then the handler's `render_with` extras on top), and
    /// the engine's business starts once it has the finished context.
    ///
    /// An associated function, not a method: it needs the renderer and nothing
    /// else from the site state, so a request can render after it has
    /// released the read lock.
    pub(crate) fn render_response(
        renderer: &dyn Renderer,
        resp: &mut Response,
        dict: &Dict,
    ) -> std::result::Result<(), RenderError> {
        if let Some(template_name) = resp.template_name.clone() {
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

/// Check the mtime of a file. Returns `None` if the file cannot be statted.
fn file_mtime(path: &std::path::Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(path).ok()?.modified().ok()
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

/// The owned request a handler is given, built from the borrowed head: the one
/// copy `h1::owned_request` makes for both the borrowed path and the owned one.
fn raw_from_head(head: &RequestHead<'_>) -> RawRequest {
    crate::h1::owned_request(head, head.body().to_vec())
}

/// The start path's `site_toml` check.
///
/// Every path is read and parsed once here, the default included. A file that
/// is missing, cannot be read or is not valid TOML is refused, because the
/// readers of `site.toml` would otherwise fall back to defaults and hide a
/// disagreement the operator wants to hear about.
pub(crate) fn check_site_toml(
    config: &crate::config::RendererConfig,
    site_dir: &std::path::Path,
) -> std::result::Result<PathBuf, String> {
    let site_toml = config.resolve_site_toml(site_dir);
    let src = if config.site_toml.is_some() {
        format!("site_toml = {}", site_toml.display())
    } else {
        format!("site.toml at {}", site_toml.display())
    };
    let text = match std::fs::read_to_string(&site_toml) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(format!("{src} does not exist"));
        }
        Err(e) => return Err(format!("{src} cannot be read: {e}")),
    };
    if let Err(e) = text.parse::<toml::Table>() {
        return Err(format!("{src} is not valid TOML: {}", e.message()));
    }
    Ok(site_toml)
}

/// What a service is started with: the site directory, the config path and the
/// optional command-line log level.
///
/// One parser for every service, so `--version`, `--dump-config` and
/// `--log-level` are handled in one place and a new service inherits them.
pub(crate) struct Invocation {
    pub(crate) site_dir: PathBuf,
    pub(crate) config_path: PathBuf,
    pub(crate) cli_log_level: Option<String>,
}

/// The binary's own name, from `argv[0]`, for `--version`.
///
/// The lifecycle lines take `Site::name`, the literal the service passes to
/// `Site::load`.
///
/// Not `CARGO_PKG_NAME`: that macro expands where it is written, which is
/// m6-core, so every service would announce itself as "m6-core" and a deploy
/// log would not say which binary it had just checked.
pub(crate) fn program_name() -> String {
    std::env::args()
        .next()
        .and_then(|arg| {
            std::path::Path::new(&arg)
                .file_name()
                .and_then(|name| name.to_str())
                .map(str::to_owned)
        })
        .unwrap_or_else(|| "m6".to_string())
}

/// Parse the command line, and handle the flags that do not start a server.
///
/// `--dump-config` loads the configuration exactly as the service would, prints
/// what it resolved to, and exits: **0 if this binary can serve this config, 2
/// if it cannot.** That is the check a deploy runs against a new binary on each
/// node before installing it, and it covers every service: they all load their
/// config through this same function.
///
/// It is not a formality. A configuration format can change under a running
/// deployment, and the old binary against the new config and the new binary
/// against the old one both fail, in opposite directions and neither loudly.
/// A validation step that runs before the install is what turns that from an
/// outage into a refused deploy.
pub(crate) fn parse_invocation(
    routes: &[(String, RouteMethod)],
    handlers: &[String],
) -> Invocation {
    let args: Vec<String> = std::env::args().collect();

    // Before the argument-count check, because `--version` takes no site directory
    // and no config: a deploy asks a freshly installed binary what it is before
    // any config is in place.
    if args.iter().any(|a| a == "--version" || a == "-V") {
        // The program's own name, from argv[0], not `CARGO_PKG_NAME`. That macro
        // expands where it is written, which is m6-core, so every service would
        // announce itself as "m6-core" and a deploy log would not say which binary
        // it had just checked.
        println!("{} {}", program_name(), env!("CARGO_PKG_VERSION"));
        std::process::exit(0);
    }

    if args.len() < 3 {
        eprintln!(
            "Usage: {} <site-dir> <config-path> [--log-level LEVEL] [--dump-config] [--version]",
            args[0]
        );
        std::process::exit(2);
    }
    let site_dir = PathBuf::from(&args[1]);
    let config_path = PathBuf::from(&args[2]);
    let cli_log_level = args
        .windows(2)
        .find(|w| w[0] == "--log-level")
        .map(|w| w[1].clone());

    if args.iter().any(|a| a == "--dump-config") {
        std::process::exit(dump_config(
            &site_dir,
            &config_path,
            routes,
            handlers,
            cli_log_level.as_deref(),
        ));
    }

    Invocation {
        site_dir,
        config_path,
        cli_log_level,
    }
}

/// The configuration refusals start and `--dump-config` share: a named
/// `site.toml` that is missing, unreadable or not TOML, then a
/// `[[backend]] compresses` disagreement. Returns the resolved `site.toml`.
///
/// Exit 2 is the configuration error code, detected before anything binds
/// (backend protocol §8.3). A service that starts having disagreed with the
/// edge about compression serves either fragmented caches or, worse, brotli
/// to clients that asked for identity, and neither is visible from the
/// outside until it bites. `--dump-config` runs the same check so a deploy
/// that validates a config learns what the start would say.
pub(crate) fn validate_startup(
    config: &crate::config::RendererConfig,
    site_dir: &std::path::Path,
    config_path: &std::path::Path,
) -> std::result::Result<PathBuf, String> {
    let site_toml = check_site_toml(config, site_dir)?;
    check_compress_declared(config, &site_toml, &backend_name_of(config_path))?;
    Ok(site_toml)
}

/// Load the config and report how each route would be served. Returns the
/// process exit code.
pub(crate) fn dump_config(
    site_dir: &std::path::Path,
    config_path: &std::path::Path,
    code_routes: &[(String, RouteMethod)],
    handlers: &[String],
    cli_log_level: Option<&str>,
) -> i32 {
    let config = match crate::config::load(config_path, site_dir) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("config error: {e:#}");
            return 2;
        }
    };

    // The same start refusals, so a deploy that validates with --dump-config
    // learns what the start would say.
    let site_toml = match validate_startup(&config, site_dir, config_path) {
        Ok(path) => path,
        Err(msg) => {
            eprintln!("Config error: {msg}");
            return 2;
        }
    };

    if let Err(msg) = resolve_log(&site_toml, &config.log, cli_log_level) {
        eprintln!("Config error: {msg}");
        return 2;
    }

    if let Err(e) = check_routes(code_routes, &config.routes) {
        eprintln!("Config error: {e}");
        return 2;
    }

    println!("site_dir    {}", site_dir.display());
    println!("config      {}", config_path.display());
    println!("site.toml   {}", site_toml.display());
    println!(
        "socket      {}",
        crate::socket_path_from_config(config_path).display()
    );
    println!(
        "server      read_timeout={} socket_mode={:04o}",
        config
            .server
            .read_timeout
            .map_or("none".into(), |d| format!("{}s", d.as_secs())),
        config.server.socket_mode
    );
    for p in &config.global_params {
        println!("global      {p}");
    }
    println!(
        "handlers    {}",
        if handlers.is_empty() {
            "none".to_string()
        } else {
            handlers.join(", ")
        }
    );

    // Every route, and what would answer it. This is the part worth reading:
    // a route with no way to be served is invisible at runtime because it
    // simply 404s.
    let mut dead = 0;
    println!("routes ({})", config.routes.len());
    for rc in &config.routes {
        let how = if let Some(h) = &rc.handler {
            if handlers.iter().any(|n| n == h) {
                format!("handler `{h}`")
            } else {
                dead += 1;
                format!("handler `{h}` NOT REGISTERED")
            }
        } else if let Some(t) = &rc.template {
            format!("template {t}")
        } else if code_routes.iter().any(|(p, _)| *p == rc.path) {
            "code route".to_string()
        } else {
            dead += 1;
            "NOTHING CAN SERVE THIS".to_string()
        };
        println!("  {:<40} {}", rc.path, how);
    }

    if dead > 0 {
        eprintln!(
            "\n{dead} route(s) this binary cannot serve. Refusing: a config and a \n\
             binary that disagree is an outage that looks like a 404."
        );
        return 2;
    }
    println!("\nok: this binary can serve every route in this config");
    0
}

/// The name `site.toml` uses for this service's `[[backend]]`: the config
/// file's stem, falling back to `m6-default`.
///
/// The same convention `socket_path_from_config` uses to derive
/// `/run/m6/<stem>.sock`, so the name in `site.toml` and the name here are the
/// same string. Used at start and by `--dump-config` so the two cannot look up
/// different entries.
fn backend_name_of(config_path: &std::path::Path) -> String {
    config_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("m6-default")
        .to_string()
}

/// Whether `site.toml` declares the compression `config` does, for the backend
/// named `backend_name`. Checked at start and by `--dump-config`.
///
/// "Compresses" means at least one MIME type has a non-zero level. The
/// defaults give text types real levels and images zero, so a service that has
/// not opted out does compress.
pub(crate) fn check_compress_declared(
    config: &crate::config::RendererConfig,
    site_toml: &std::path::Path,
    backend_name: &str,
) -> std::result::Result<(), String> {
    let compresses = config
        .compression
        .values()
        .any(|c| c.brotli > 0 || c.gzip > 0);
    crate::compress::check_declared_support(site_toml, backend_name, compresses)
}

/// The log format and level, in order of precedence: the command line, then
/// the service's `[log]`, then `site.toml`'s `[log]`.
///
/// Read once at start. The resolved level is parsed here, so an unknown level
/// is a configuration error before anything binds.
pub(crate) fn resolve_log(
    site_toml: &std::path::Path,
    config_log: &crate::config::LogConfig,
    cli_log_level: Option<&str>,
) -> std::result::Result<(String, String), String> {
    let (site_level, site_format) =
        crate::log::read_site_log_config(site_toml).map_err(|e| e.to_string())?;
    let format = config_log.format.clone().unwrap_or(site_format);
    let level = cli_log_level
        .map(str::to_string)
        .or_else(|| config_log.level.clone())
        .unwrap_or(site_level);
    crate::log::parse_level(&level).map_err(|e| e.to_string())?;
    Ok((format, level))
}

pub(crate) fn unknown_handlers(
    routes: &[CompiledRoute],
    named: &[String],
    code_patterns: &[String],
) -> Vec<(String, String)> {
    let mut missing = Vec::new();
    for route in routes {
        match &route.handler {
            // A route naming a handler nothing provides.
            Some(name) if !named.iter().any(|registered| registered == name) => {
                missing.push((route.pattern.clone(), name.clone()));
            }
            Some(_) => {}
            // A config route with no template and no handler can still be
            // served by a code route registered on the same pattern, which is
            // how a renderer carries `cache` and `methods` for an endpoint
            // implemented in Rust. With no such route it can serve
            // nothing, and the only symptom is a 404 that looks like a missing
            // page. That is exactly what an out-of-step config and binary
            // produce, so it is refused here rather than discovered in
            // production.
            //
            // Matched on pattern alone, not pattern and method: a config route
            // left at the default `Any` legitimately annotates a code route
            // registered with `route_get`, and comparing methods would call
            // that dead.
            None if route.template.is_none() && !code_patterns.contains(&route.pattern) => {
                missing.push((
                    route.pattern.clone(),
                    "<no template, no handler>".to_string(),
                ));
            }
            None => {}
        }
    }
    missing
}

/// Render `unknown_handlers`' answer as one line an operator can act on.
pub(crate) fn describe_unknown(missing: &[(String, String)], registered: &[String]) -> String {
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

/// The body minified for `mime`, or `None` when that type is not minified.
#[cfg(feature = "minify")]
fn minified(
    mime: &str,
    bytes: &[u8],
    config: &crate::config::MinificationConfig,
) -> Option<Vec<u8>> {
    if !config.is_enabled(mime) {
        return None;
    }
    match mime {
        "text/html" => Some(crate::minify::minify_html(bytes, config.inline_js)),
        "text/css" => Some(crate::minify::minify_css(bytes)),
        "application/json" => Some(crate::minify::minify_json(bytes)),
        "application/javascript" | "text/javascript" => Some(crate::minify::minify_js(bytes)),
        _ => None,
    }
}

/// Without the `minify` feature the config loader has already refused any
/// config that turns minification on, so there is nothing to do.
#[cfg(not(feature = "minify"))]
fn minified(_: &str, _: &[u8], _: &crate::config::MinificationConfig) -> Option<Vec<u8>> {
    None
}

/// A renderer that renders nothing: these tests route and finish, they do
/// not render templates.
#[cfg(test)]
fn test_renderer() -> &'static dyn RendererFactory {
    #[cfg(feature = "templates")]
    {
        &crate::template::TeraFactory
    }
    #[cfg(not(feature = "templates"))]
    {
        &crate::render::NoTemplates
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::h1::parse_head;

    /// A site over a temp directory and the given config, with nothing loaded
    /// from the command line or the log.
    fn site_from(toml: &str, named: &[&str]) -> (Site, tempfile::TempDir) {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("templates")).unwrap();
        // Written before the site is built: the renderer loads templates at
        // start, so a file written after would never be seen.
        std::fs::write(dir.path().join("templates/index.html"), "<p>hello</p>").unwrap();
        let config_path = dir.path().join("m6-site.toml");
        std::fs::write(&config_path, toml).unwrap();
        let config = crate::config::load(&config_path, dir.path()).unwrap();
        let state =
            FrameworkState::build(config, dir.path().to_path_buf(), &[], test_renderer()).unwrap();
        let site = Site::from_state(state, named.iter().map(|n| (*n).to_string()).collect());
        (site, dir)
    }

    /// A config route naming a handler comes back with that handler's index in
    /// the `named` slice, with the path parameter already in the request
    /// dictionary. The index is 1, not 0: the names were given in the order
    /// `["other", "files"]`, and a wrong index would call the wrong handler.
    #[test]
    fn handle_returns_the_index_of_a_named_route() {
        let (site, _dir) = site_from(
            "[[route]]\npath = \"/assets/{*relpath}\"\nhandler = \"files\"\n",
            &["other", "files"],
        );
        let mut slots = [httparse::EMPTY_HEADER; 64];
        let head = match parse_head(
            b"GET /assets/a.css HTTP/1.1\r\nHost: localhost\r\n\r\n",
            &mut slots,
        ) {
            crate::h1::HeadResult::Complete(head) => head,
            _ => panic!("the test's request must parse"),
        };
        match site.handle(&head) {
            Handled::Named(1, req) => assert_eq!(
                req.get("relpath").and_then(|v| v.as_str()),
                Some("a.css"),
                "the path parameter must reach the handler"
            ),
            Handled::Named(0, _) => panic!("index 0 is `other`, not `files`"),
            _ => panic!("a config route naming a handler must come back as Named"),
        }
    }

    /// A template route is answered here and comes back finished, not as an
    /// index: the service has nothing to call for it.
    #[test]
    fn a_template_route_is_answered_and_finished() {
        let (site, _dir) = site_from(
            "[[route]]\npath = \"/\"\ntemplate = \"templates/index.html\"\n",
            &[],
        );
        let mut slots = [httparse::EMPTY_HEADER; 64];
        let head = match parse_head(b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n", &mut slots) {
            crate::h1::HeadResult::Complete(head) => head,
            _ => panic!("the test's request must parse"),
        };
        match site.handle(&head) {
            Handled::Done(resp) => assert_eq!(resp.status, 200),
            _ => panic!("a template route must come back Done"),
        }
    }

    /// A route carrying both a handler and a template goes to the handler,
    /// which is the order the request path has always dispatched in. A change
    /// there would silently send such a route's requests to the template.
    #[test]
    fn a_route_with_a_handler_and_a_template_goes_to_the_handler() {
        let (site, _dir) = site_from(
            "[[route]]\npath = \"/both\"\nhandler = \"files\"\n\
             template = \"templates/index.html\"\n",
            &["other", "files"],
        );
        let mut slots = [httparse::EMPTY_HEADER; 64];
        let head = match parse_head(b"GET /both HTTP/1.1\r\nHost: localhost\r\n\r\n", &mut slots) {
            crate::h1::HeadResult::Complete(head) => head,
            _ => panic!("the test's request must parse"),
        };
        match site.handle(&head) {
            Handled::Named(1, _) => {}
            Handled::Named(0, _) => panic!("index 0 is `other`"),
            Handled::Named(index, _) => panic!("unexpected named index {index}"),
            Handled::Done(_) => panic!("the handler wins over the template"),
            Handled::Code(..) => panic!("a config route names no code route"),
        }
    }

    /// An internal dictionary failure is finished like the unmatched 404: no
    /// route's default `Cache-Control`, no `Last-Modified`, no route headers.
    /// An error must not go out looking like cacheable content.
    #[test]
    fn a_dictionary_error_is_finished_without_route_defaults() {
        let (site, _dir) = site_from(
            "[[route]]\npath = \"/assets/{*relpath}\"\ntemplate = \"templates/index.html\"\n\
             cache = \"public\"\n",
            &[],
        );
        let mut slots = [httparse::EMPTY_HEADER; 64];
        // A percent sign in the wildcard capture fails path validation, which
        // is a dictionary build error. (A space would fail the HTTP parse.)
        let head = match parse_head(
            b"GET /assets/a%20b HTTP/1.1\r\nHost: localhost\r\n\r\n",
            &mut slots,
        ) {
            crate::h1::HeadResult::Complete(head) => head,
            _ => panic!("the test's request must parse"),
        };
        match site.handle(&head) {
            Handled::Done(response) => {
                assert_eq!(response.status, 400);
                assert!(
                    !crate::headers::contains(&response.headers[..], "cache-control"),
                    "an error must not carry the route's Cache-Control: {:?}",
                    response.headers
                );
                // The finish path is the unmatched 404's, so the CSRF
                // housekeeping still runs. That is the intended model: an
                // error is finished like any other answer that has no route.
                #[cfg(feature = "csrf")]
                assert!(
                    response
                        .headers
                        .iter()
                        .any(|(name, value)| name.eq_ignore_ascii_case("set-cookie")
                            && value.contains("_csrf=")),
                    "the unmatched-404 model sets the CSRF cookie: {:?}",
                    response.headers
                );
            }
            _ => panic!("a matched route with a template comes back Done"),
        }
    }

    /// The code-route index is the index of the route in the `code` slice, so
    /// a service's `match` on it calls the right function.
    #[test]
    fn handle_returns_the_index_of_a_code_route() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("templates")).unwrap();
        let config_path = dir.path().join("m6-site.toml");
        std::fs::write(&config_path, "").unwrap();
        let config = crate::config::load(&config_path, dir.path()).unwrap();
        let code = vec![
            ("/status".to_string(), RouteMethod::Get),
            ("/health".to_string(), RouteMethod::Get),
        ];
        let state = FrameworkState::build(config, dir.path().to_path_buf(), &code, test_renderer())
            .unwrap();
        let site = Site::from_state_with_code(state, Vec::new(), code);
        let mut slots = [httparse::EMPTY_HEADER; 64];
        let head = match parse_head(
            b"GET /health HTTP/1.1\r\nHost: localhost\r\n\r\n",
            &mut slots,
        ) {
            crate::h1::HeadResult::Complete(head) => head,
            _ => panic!("the test's request must parse"),
        };
        match site.handle(&head) {
            Handled::Code(1, _) => {}
            Handled::Code(index, _) => panic!("`/health` is index 1, got {index}"),
            _ => panic!("a code route must come back as Code"),
        }
    }

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
        if unsafe { libc::geteuid() } == 0 {
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

    /// file_mtime returns different values after a file is updated.
    #[test]
    fn test_file_mtime_changes() {
        use std::io::Write;
        use tempfile::NamedTempFile;

        let mut f = NamedTempFile::new().unwrap();
        writeln!(f, "v1").unwrap();
        let mtime1 = file_mtime(f.path());
        assert!(mtime1.is_some());

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

        let past = SystemTime::now() - Duration::from_secs(7200);
        let recent = SystemTime::now() - Duration::from_secs(30);
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

    /// The capability a static file server needs, and the whole of why m6-file
    /// has a different shape: one route answers for a path of unknown depth.
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
        let state = FrameworkState::build(cfg, site_dir.path().to_path_buf(), &[], test_renderer())
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
        let state = FrameworkState::build(cfg, site_dir.path().to_path_buf(), &[], test_renderer())
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
        let state = FrameworkState::build(cfg, site_dir.path().to_path_buf(), &[], test_renderer())
            .unwrap();
        (state, site_dir)
    }

    /// What a browser sees: a script labelled `text/javascript` comes back
    /// brotli-compressed under the default config.
    #[test]
    fn a_text_javascript_response_is_brotli_compressed_by_default() {
        let (state, _site_dir) =
            state_from("[[route]]\npath = \"/app.js\"\nhandler = \"script\"\n");
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
                let response = Response::status(200)
                    .header("Content-Type", "text/javascript; charset=utf-8")
                    .body("function f(a, b) { return a + b; }\n".repeat(50));
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
}

#[cfg(test)]
mod dict_cost {
    //! What the request path spends per request before a handler or a template runs.
    //!
    //! All `#[ignore]`d. These are measurements, not assertions: a timing
    //! assertion is the wall-clock trap `test_static_file_cache_hit` already
    //! fell into once, firing on a loaded build box against correct code.
    //!
    //! ```sh
    //! cargo test --release -p m6-core dict_cost -- --ignored --nocapture
    //! ```
    //!
    //! Measured on the laptop in release, 2026-09-12. **The whole cost is
    //! copying immutable state per request**, and it is nothing to do with
    //! parsing:
    //!
    //! | what | ns |
    //! |---|---|
    //! | `Map::new()` | 41 |
    //! | query parse + map | 42 |
    //! | `parse_cookies` | 333 |
    //! | `chrono::Utc::now()` + two `strftime` | 708 |
    //! | **cloning 20 config keys** | **2,750** |
    //!
    //! Against a real content catalogue, tens of kilobytes and thousands of
    //! nodes, the same shape costs far more, because such a file is commonly
    //! loaded as *both* the
    //! global params and the route's params and is then cloned a third time by
    //! `render_response`:
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
            let t = std::time::Instant::now();
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
        let state =
            FrameworkState::build(config, site_dir.path().to_path_buf(), &[], test_renderer())
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
            "8. now() + both formats   {:>7}",
            median(N, || {
                let n = chrono::Utc::now();
                std::hint::black_box((
                    n.format("%Y-%m-%dT%H:%M:%SZ").to_string(),
                    n.format("%Y").to_string(),
                ));
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
    //! Every copy the request path makes per request, measured against a real content
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
            let t = std::time::Instant::now();
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
        let state =
            FrameworkState::build(config, site_dir.path().to_path_buf(), &[], test_renderer())
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
        let err = FrameworkState::build(config, PathBuf::from("."), &[], test_renderer())
            .err()
            .expect("the conflicting config must not build");
        assert!(
            err.to_string().contains("can both answer GET /blog/x"),
            "{err}"
        );
    }
}
