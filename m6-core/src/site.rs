//! The site's request handling, as plain functions on a value the service
//! owns.
//!
//! `Site` is what is left of `App`'s request path once the loop is gone: the
//! start half (the command line, the config, `site.toml`, templates, params,
//! the route table and the unknown-handler checks) and the per-request half
//! (routing, the dictionary, rendering, and the site-wide finish: headers,
//! CSRF, flash, minification and compression).
//!
//! A service's loop call [`Site::load`] once at start, then per request
//! [`Site::handle`], its own handler for the index `handle` returns, and
//! [`Site::finish`]. `Site` never calls back into the service: a route the
//! service answers itself comes back as [`Handled::Code`] or [`Handled::Named`]
//! with the index of its route.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Map, Value};
use tracing::{error, warn};

use crate::app::{
    check_routes, find_route, route_method_key, CompiledRoute, FrameworkState, RouteMethod,
};
use crate::dict::Dict;
use crate::h1::RequestHead;
use crate::http::RawRequest;
use crate::render::{RenderError, RendererFactory};
use crate::request::Request;
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
    /// The framework state the request path reads: the config, the compiled
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

    /// The start half of what was `run_app_with_shutdown`, for `App` and for a
    /// test: `config` is the config the caller has already loaded, so a
    /// service that needed it before this point does not read it twice.
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
    /// own files against (what `AppContext` carried).
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

    /// The number of routes the startup log reports.
    pub(crate) fn route_count(&self) -> usize {
        self.state.routes.len()
    }

    /// The worker pool the `App` runners read from the config.
    pub(crate) fn thread_pool(&self) -> (usize, usize) {
        (
            self.state.config.thread_pool.size,
            self.state.config.thread_pool.queue_size,
        )
    }

    /// Handle a borrowed request head. Builds the owned request the service's
    /// handler needs from the head, then dispatches.
    pub fn handle(&self, head: &RequestHead<'_>) -> Handled {
        self.handle_raw(raw_from_head(head))
    }

    /// `handle` for a caller that already owns a [`RawRequest`]: the one
    /// dispatch, shared by the borrowed path and by `App`'s blocking workers.
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
                let token = from_dict.unwrap_or_else(crate::app::generate_csrf_token);
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
        //
        // A cache keeps what it stores, so minification is paid once per fill
        // and its saving collected on every hit. A `no-store` response has no
        // second hit, so the cost lands on every request, where it is about
        // what rendering the page costs, for a few bytes saved after
        // compression. The default ETag is skipped by the same rule in
        // `response.rs`.
        if !resp.verbatim && !crate::response::cache_control_no_store(&resp.headers) {
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

/// The owned request a handler is given, built from the borrowed head: the one
/// copy `h1::owned_request` makes for both the borrowed path and the owned one.
fn raw_from_head(head: &RequestHead<'_>) -> RawRequest {
    crate::h1::owned_request(head, head.body().to_vec())
}

/// The checking half of the `App` start path's `site_toml_or_exit`.
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

/// What every `App` service is started with.
///
/// One parser rather than four. `run_app` and the three stateful runners each
/// had an identical copy of this, which is how `--dump-config` ended up
/// existing only in m6-http: there was no one place to add it to.
pub(crate) struct Invocation {
    pub(crate) site_dir: PathBuf,
    pub(crate) config_path: PathBuf,
    pub(crate) cli_log_level: Option<String>,
}

/// The binary's own name, from `argv[0]`, for the lifecycle lines and
/// `--version`.
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
/// It is not a formality. The `App` migrations changed m6-file's config format,
/// and the old binary against the new config and the new binary against the old
/// one both fail, in opposite directions and neither loudly. A validation step
/// that runs before the install is what turns that from an outage into a
/// refused deploy.
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
        "thread_pool size={} queue={}",
        config.thread_pool.size, config.thread_pool.queue_size
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
    // a route with no way to be served is the failure the App migrations made
    // possible, and it is invisible at runtime because it simply 404s.
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::h1::parse_head;

    /// A renderer that renders nothing: these tests route and finish, they do
    /// not render templates.
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
}

/// The finish path's minification rule: a response a cache keeps is minified
/// once per fill, and a response no cache keeps goes out as it rendered.
#[cfg(test)]
mod minify_tests {
    use super::*;
    use crate::h1::{parse_head, HeadResult};

    /// The `/login` template. The two-space runs and the blank line are what a
    /// minifier removes, so finding them in the finished body is the evidence
    /// that it did not run.
    const LOGIN_PAGE: &str = "<p>  a  </p>\n\n<p>  b  </p>";

    /// A site over a temp directory and the given config, with the templates
    /// already written: the renderer reads the directory at start, so a file
    /// written after would never be seen. Copied from `tests::site_from` with
    /// the `/login` page added.
    fn site_from(toml: &str, named: &[&str]) -> (Site, tempfile::TempDir) {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("templates")).unwrap();
        std::fs::write(dir.path().join("templates/index.html"), "<p>hello</p>").unwrap();
        std::fs::write(dir.path().join("templates/login.html"), LOGIN_PAGE).unwrap();
        let config_path = dir.path().join("m6-site.toml");
        std::fs::write(&config_path, toml).unwrap();
        let config = crate::config::load(&config_path, dir.path()).unwrap();
        let state =
            FrameworkState::build(config, dir.path().to_path_buf(), &[], test_renderer()).unwrap();
        let site = Site::from_state(state, named.iter().map(|n| (*n).to_string()).collect());
        (site, dir)
    }

    /// A renderer that renders the templates written by [`site_from`].
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

    /// One `GET` through `Site::handle`, with the client's `Accept-Encoding`.
    fn handle_get(site: &Site, path: &str, accept_encoding: &str) -> Handled {
        let request = format!(
            "GET {path} HTTP/1.1\r\nHost: localhost\r\nAccept-Encoding: {accept_encoding}\r\n\r\n"
        );
        let mut slots = [httparse::EMPTY_HEADER; 64];
        let head = match parse_head(request.as_bytes(), &mut slots) {
            HeadResult::Complete(head) => head,
            _ => panic!("the test's request must parse"),
        };
        site.handle(&head)
    }

    /// A cache keeps nothing of a `no-store` page, so minifying it is paid on
    /// every request and never repaid. The page must leave `handle` with the
    /// bytes the template rendered, spaces and blank line intact.
    #[test]
    fn a_no_store_page_is_sent_unminified() {
        let (site, _dir) = site_from(
            "[[route]]\npath = \"/login\"\ntemplate = \"templates/login.html\"\n\
             cache = \"no-store\"\n",
            &[],
        );
        match handle_get(&site, "/login", "identity") {
            Handled::Done(resp) => {
                let body = resp.body.as_bytes().expect("a rendered page is buffered");
                assert_eq!(
                    body,
                    LOGIN_PAGE.as_bytes(),
                    "a no-store page must carry the template's whitespace: {:?}",
                    String::from_utf8_lossy(body)
                );
            }
            _ => panic!("a template route comes back Done"),
        }
    }

    /// A cacheable page is minified once and served to many hits, which is
    /// where its saving is collected. Its whitespace must be gone.
    #[test]
    fn a_cacheable_page_is_still_minified() {
        let (site, _dir) = site_from(
            "[[route]]\npath = \"/login\"\ntemplate = \"templates/login.html\"\n",
            &[],
        );
        match handle_get(&site, "/login", "identity") {
            Handled::Done(resp) => {
                let body = resp.body.as_bytes().expect("a rendered page is buffered");
                assert!(
                    !body.windows(2).any(|pair| pair == b"  "),
                    "a public page is minified, so no two spaces remain: {:?}",
                    String::from_utf8_lossy(body)
                );
            }
            _ => panic!("a template route comes back Done"),
        }
    }

    /// A config route naming a handler defaults to `no-store`, so its answer
    /// is unminified for the same reason. JSON is a minified type by default,
    /// and this body is deliberately spaced.
    #[test]
    fn a_no_store_json_answer_is_sent_unminified() {
        let (site, _dir) = site_from(
            "[[route]]\npath = \"/api\"\nhandler = \"json\"\n",
            &["json"],
        );
        let req = match handle_get(&site, "/api", "identity") {
            Handled::Named(0, req) => req,
            Handled::Named(index, _) => panic!("`json` is index 0, got {index}"),
            _ => panic!("a config route naming a handler comes back Named"),
        };
        let answer = Response::status(200)
            .header("Content-Type", "application/json")
            .body(b"{ \"a\": 1 }".to_vec());
        let resp = site.finish(&req, answer);
        assert_eq!(
            resp.body
                .as_bytes()
                .expect("a handler's JSON answer is buffered"),
            b"{ \"a\": 1 }".as_slice(),
            "a no-store JSON answer must not be minified"
        );
    }

    /// A `no-store` response skips minification, not compression: a client
    /// that accepts brotli still gets the page compressed.
    #[test]
    fn a_no_store_page_is_still_compressed() {
        let (site, _dir) = site_from(
            "[[route]]\npath = \"/login\"\ntemplate = \"templates/login.html\"\n\
             cache = \"no-store\"\n",
            &[],
        );
        match handle_get(&site, "/login", "br") {
            Handled::Done(resp) => assert_eq!(
                crate::headers::get(&resp.headers[..], "content-encoding"),
                Some("br"),
                "a no-store page is still compressed: {:?}",
                resp.headers
            ),
            _ => panic!("a template route comes back Done"),
        }
    }
}
