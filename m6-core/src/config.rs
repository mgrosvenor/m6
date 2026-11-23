//! Configuration: TOML in, `RendererConfig` out.
//!
//! **One parser for every service.** Each of them used to carry its own, and
//! they drifted: m6-file and m6-auth-server had their own `[log]` handling,
//! m6-file had a second `[[route]]` shape, and each resolved paths against a
//! slightly different root.
//!
//! Keys core does not define are kept rather than dropped. A `[[route]]` may
//! carry anything a service's handler needs -- `root` and `tail` for
//! m6-file -- and core passes them through untouched via
//! `Request::route_setting`. Discarding them silently was indistinguishable
//! from a typo being honoured.
//!
//! `--dump-config` on any service prints what this produced and how each route
//! would be served, and exits non-zero if the binary cannot serve the config.
//! That is what the deploy validates against the new binary before installing
//! it.

use std::path::{Path, PathBuf};

use anyhow::Context;
use serde_json::{Map, Value};

use crate::startup::{self, ConfigRole, ConfigTrace, Section};

/// The `Cache-Control` a page gets unless its route says otherwise.
///
/// This is the browser-caching pattern: the browser revalidates on every
/// request, the shared cache keeps the response for a day.
///
/// - `max-age=0` makes the browser's copy stale immediately, and
///   `must-revalidate` forbids serving it once stale (RFC 9111 5.2.2.1 and
///   5.2.2.2), so a deploy is visible on the next request and an unchanged
///   page costs one conditional request and a 304.
/// - `s-maxage=86400` is honoured ONLY by shared caches and overrides
///   `max-age` for them (RFC 9111 5.2.2.10), so the edge keeps the page for a
///   day and answers `If-None-Match` itself.
/// - `public` makes the response storable by a shared cache.
///
/// `no-cache` is absent on purpose: it has the same browser behaviour but also
/// forces the edge to revalidate (RFC 9111 5.2.2.4), which is the round trip
/// `s-maxage` removes. `stale-while-revalidate` is absent because it would let
/// the browser serve stale while revalidating.
pub const PAGE_CACHE_CONTROL: &str = "public, max-age=0, must-revalidate, s-maxage=86400";

/// A single route entry from `[[route]]` in the config file.
#[derive(Debug, Clone)]
pub struct RouteConfig {
    pub path: String,
    pub template: Option<String>,
    pub params: Vec<String>,
    pub status: u16,
    /// A `Cache-Control` value, or empty when the route sets none.
    pub cache: String,
    /// The config named `cache`, so the value replaces a `Cache-Control` the
    /// handler set. False when `cache` is only the default for the route's kind.
    pub cache_explicit: bool,
    pub methods: Option<Vec<String>>,
    /// Extra response headers as `[[key, value]]` pairs.
    pub headers: Vec<(String, String)>,
    /// Name of a handler the service registers in code.
    ///
    /// A handler is code and cannot appear while the process runs; a *route*
    /// is config. Naming the binding in config puts the half that changes on
    /// the side that is read at start, so adding an asset tree is a config
    /// edit followed by a restart.
    pub handler: Option<String>,
    /// Keys on this `[[route]]` that core does not define, verbatim.
    ///
    /// Core deliberately does not know what `root` or `tail` mean; m6-file
    /// does. A handler reads them through `Request::route_setting`, which is
    /// what lets a service keep its own per-route vocabulary without core
    /// growing a field per consumer.
    ///
    /// `Arc` because the matched route is cloned once per request and a map
    /// cloned per request is a dynamic allocation on the hot path.
    pub settings: std::sync::Arc<Map<String, Value>>,
}

/// LRU params-cache configuration.
#[derive(Debug, Clone, Default)]
pub struct ParamsCacheConfig {
    pub size: usize,
}

/// Connection-level settings parsed from `[server]`.
#[derive(Debug, Clone, Default)]
pub struct ServerConfig {
    /// Read timeout applied to every accepted connection.
    ///
    /// `None` means no timeout: a peer that connects and then says nothing
    /// holds its connection slot until it disconnects. Configure
    /// `read_timeout_s = 0` to ask for that deliberately.
    pub read_timeout: Option<std::time::Duration>,
    /// How long shutdown waits for accepted requests to finish, from
    /// `drain_timeout_s`.
    ///
    /// When it passes, the service warns with how many requests were still
    /// running and exits anyway, so a deploy finishes inside the supervisor's
    /// own stop timeout instead of being killed mid-response. Zero waits for
    /// nothing.
    pub drain_timeout: std::time::Duration,
    /// Mode applied to the unix socket after bind, from `socket_mode`.
    ///
    /// Written the way systemd writes it, as an octal string:
    /// `socket_mode = "0660"`. TOML has no octal literal, and `660` as a
    /// decimal integer is `0o1224`, which is the kind of thing that is only
    /// noticed a month after it stops mattering.
    pub socket_mode: u32,
}

/// Compression level for a MIME type.
#[derive(Debug, Clone)]
pub struct CompressionLevel {
    pub brotli: u32,
    pub gzip: u32,
}

/// Log configuration from a `[log]` table.
///
/// One type and one parse for every file that can set a level: `site.toml`
/// sets the site default and a service's own config overrides it. `None` means
/// the file did not set one, so the next source in the order decides. The
/// level itself is parsed by [`crate::log::parse_level`].
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct LogConfig {
    pub level: Option<String>,
}

impl LogConfig {
    /// Parse the `[log]` table of a config file.
    ///
    /// An absent table is [`LogConfig::default`], which sets no level. Every
    /// service reads the table through this function, so the same file gets
    /// the same answer from every process: a `[log]` that is not a table, a
    /// key other than `level`, and a `level` that is not a string are all
    /// refused here.
    pub fn from_toml(value: Option<&toml::Value>) -> anyhow::Result<Self> {
        let Some(value) = value else {
            return Ok(Self::default());
        };
        let table = value
            .as_table()
            .ok_or_else(|| anyhow::anyhow!("[log] must be a table"))?;
        if let Some(key) = table.keys().find(|k| k.as_str() != "level") {
            anyhow::bail!("unknown key [log] {key}: known keys are level");
        }
        let level = match table.get("level") {
            Some(v) => Some(
                v.as_str()
                    .ok_or_else(|| anyhow::anyhow!("[log] level must be a string, got {v}"))?
                    .to_string(),
            ),
            None => None,
        };
        Ok(Self { level })
    }
}

/// Fully parsed renderer configuration.
#[derive(Debug, Clone, Default)]
pub struct RendererConfig {
    /// Non-framework keys merged into every request dictionary.
    pub user_config: Map<String, Value>,
    /// Paths to global params JSON files.
    pub global_params: Vec<String>,
    /// Route entries.
    pub routes: Vec<RouteConfig>,
    /// Params-cache settings.
    pub params_cache: ParamsCacheConfig,
    /// Connection-level settings.
    pub server: ServerConfig,
    /// Compression settings keyed by MIME type.
    pub compression: std::collections::HashMap<String, CompressionLevel>,
    /// Minification settings keyed by MIME type.
    pub minification: MinificationConfig,
    /// Logging settings from `[log]` in the config file.
    pub log: LogConfig,
    /// The named size limits from `[limits]`: each bounded buffer's start and
    /// maximum. See [`crate::limits`].
    pub limits: crate::limits::Limits,
    /// Where `site.toml` is, from the reserved key `site_toml`.
    ///
    /// A relative path resolves against `site_dir`. `None` means
    /// `<site_dir>/site.toml`. See [`RendererConfig::resolve_site_toml`].
    pub site_toml: Option<PathBuf>,
    /// The document root's size budget in mebibytes, from the top-level
    /// `site_size_budget_mb` key. m6-file reads it. `None` uses
    /// `m6_file_lib::site_size::DEFAULT_BUDGET_MB`.
    ///
    /// The key is parsed here, rather than in m6-file, so a value that is not a
    /// positive integer is refused before any service binds a socket. A service
    /// that serves nothing from disk accepts the key and never reads it.
    pub site_size_budget_mb: Option<u64>,
}

impl RendererConfig {
    /// The path of `site.toml` for a process started on `site_dir`.
    ///
    /// No `site_toml` key gives `<site_dir>/site.toml`. An absolute value is
    /// used as given and a relative one resolves against `site_dir`, the way
    /// every other relative path in a process config does.
    pub fn resolve_site_toml(&self, site_dir: &Path) -> PathBuf {
        match &self.site_toml {
            None => site_dir.join("site.toml"),
            Some(p) if p.is_absolute() => p.clone(),
            Some(p) => site_dir.join(p),
        }
    }
}

/// A config value that must never be written to a log.
///
/// **This is the mechanism, and it is a type.** A field declared `Secret<String>`
/// serialises as [`startup::REDACTED`] through every `Serialize` path, so the
/// start-up record cannot print it even by mistake. The value is read through
/// [`Secret::expose`] by the code that needs it.
///
/// A config type that marks its secrets this way needs no name list anywhere,
/// and a new secret is a compile-time change with no separate registration to
/// forget.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Secret<T>(T);

impl<T> Secret<T> {
    /// Wrap a value that must not be logged.
    pub const fn new(value: T) -> Secret<T> {
        Secret(value)
    }

    /// The value, for the code that acts on it.
    pub fn expose(&self) -> &T {
        &self.0
    }

    /// Unwrap the value.
    pub fn into_inner(self) -> T {
        self.0
    }
}

impl<T> std::fmt::Debug for Secret<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(startup::REDACTED)
    }
}

impl<T> serde::Serialize for Secret<T> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(startup::REDACTED)
    }
}

impl RendererConfig {
    /// The full effective configuration as JSON, defaults included.
    ///
    /// This is the value the start-up record splits into `config_section`
    /// lines. Every field of [`RendererConfig`] is here, including a section
    /// the file did not contain, because a reader recovering the run needs the
    /// value the process actually used.
    ///
    /// A value whose key was supplied by the secrets file is redacted by
    /// [`startup::sections_from_value`], using the trace's provenance. A value
    /// declared [`Secret`] is already [`startup::REDACTED`] here.
    pub fn effective_json(&self, site_dir: &Path) -> Value {
        let mut root = Map::new();
        // The service's own top-level keys. They are merged at the root, not
        // under a `user_config` key, so their paths match the config file's.
        for (key, value) in &self.user_config {
            root.insert(key.clone(), value.clone());
        }

        let server = serde_json::json!({
            "read_timeout_s": self.server.read_timeout.map(|d| d.as_secs()),
            "socket_mode": format!("{:04o}", self.server.socket_mode),
        });
        root.insert("server".to_string(), server);
        root.insert(
            "params_cache".to_string(),
            serde_json::json!({"size": self.params_cache.size}),
        );
        root.insert(
            "log".to_string(),
            serde_json::json!({"level": self.log.level}),
        );

        // The minification table is flat in the file: `[minification]` holds
        // one `<mime> = true` key per type, so the record and the rebuilt
        // file use the same shape.
        let mut minification = Map::new();
        for (mime, on) in &self.minification.enabled {
            minification.insert(mime.clone(), Value::Bool(*on));
        }
        root.insert("minification".to_string(), Value::Object(minification));

        let mut compression = Map::new();
        for (mime, level) in &self.compression {
            compression.insert(
                mime.clone(),
                serde_json::json!({"brotli": level.brotli, "gzip": level.gzip}),
            );
        }
        root.insert("compression".to_string(), Value::Object(compression));

        let routes: Vec<Value> = self
            .routes
            .iter()
            .map(|route| {
                let mut table = Map::new();
                table.insert("path".to_string(), Value::String(route.path.clone()));
                table.insert("template".to_string(), json_option(&route.template));
                table.insert(
                    "params".to_string(),
                    Value::Array(
                        route
                            .params
                            .iter()
                            .map(|p| Value::String(p.clone()))
                            .collect(),
                    ),
                );
                table.insert("status".to_string(), Value::from(route.status));
                table.insert("cache".to_string(), Value::String(route.cache.clone()));
                table.insert(
                    "cache_explicit".to_string(),
                    Value::Bool(route.cache_explicit),
                );
                table.insert(
                    "methods".to_string(),
                    match &route.methods {
                        Some(methods) => {
                            Value::Array(methods.iter().map(|m| Value::String(m.clone())).collect())
                        }
                        None => Value::Null,
                    },
                );
                table.insert(
                    "headers".to_string(),
                    Value::Array(
                        route
                            .headers
                            .iter()
                            .map(|(k, v)| {
                                Value::Array(vec![
                                    Value::String(k.clone()),
                                    Value::String(v.clone()),
                                ])
                            })
                            .collect(),
                    ),
                );
                table.insert("handler".to_string(), json_option(&route.handler));
                for (key, value) in route.settings.iter() {
                    table.insert(key.clone(), value.clone());
                }
                Value::Object(table)
            })
            .collect();
        root.insert("route".to_string(), Value::Array(routes));
        root.insert(
            "global_params".to_string(),
            Value::Array(
                self.global_params
                    .iter()
                    .map(|p| Value::String(p.clone()))
                    .collect(),
            ),
        );
        root.insert(
            "site_toml".to_string(),
            Value::String(self.resolve_site_toml(site_dir).display().to_string()),
        );
        root.insert(
            "site_size_budget_mb".to_string(),
            match self.site_size_budget_mb {
                Some(mb) => Value::from(mb),
                None => Value::Null,
            },
        );
        Value::Object(root)
    }

    /// Every effective setting, split into bounded sections for the start-up
    /// record, with each value's source from `trace`.
    pub fn startup_sections(&self, site_dir: &Path, trace: &ConfigTrace) -> Vec<Section> {
        startup::sections_from_value("", &self.effective_json(site_dir), trace)
    }
}

fn json_option(value: &Option<String>) -> Value {
    match value {
        Some(value) => Value::String(value.clone()),
        None => Value::Null,
    }
}

/// Per-MIME-type minification enable flag.
#[derive(Debug, Clone, Default)]
pub struct MinificationConfig {
    /// Which MIME types have minification enabled.
    pub enabled: std::collections::HashMap<String, bool>,
}

impl MinificationConfig {
    /// Returns true if minification is enabled for `mime`.
    pub fn is_enabled(&self, mime: &str) -> bool {
        *self.enabled.get(mime).unwrap_or(&false)
    }
}

fn default_minification() -> MinificationConfig {
    let mut m = std::collections::HashMap::new();
    m.insert("text/html".to_string(), true);
    m.insert("text/css".to_string(), true);
    m.insert("application/json".to_string(), true);
    MinificationConfig { enabled: m }
}

/// Framework-consumed top-level keys that must not appear in the request dict.
const FRAMEWORK_KEYS: &[&str] = &[
    "global_params",
    "route",
    "secrets_file",
    "thread_pool",
    "params_cache",
    "server",
    "compression",
    "minification",
    "log",
    "limits",
    "errors",
    "flash_secret",
    "site_toml",
    "site_size_budget_mb",
];

/// Load and merge config + optional secrets file.
///
/// Returns the effective config and the trace that says which file, flag or
/// environment variable set each key. The trace is what the start-up record
/// needs to label a default as a default.
pub fn load_traced(
    config_path: &Path,
    site_dir: &Path,
) -> anyhow::Result<(RendererConfig, ConfigTrace)> {
    let raw = std::fs::read_to_string(config_path)
        .with_context(|| format!("reading config {}", config_path.display()))?;

    let mut toml_val: toml::Value = toml::from_str(&raw)
        .with_context(|| format!("parsing config {}", config_path.display()))?;

    let mut trace = ConfigTrace::new();
    trace.add_config_file(config_path, ConfigRole::Config, &raw, &toml_val);

    // Optionally merge secrets file.
    if let Some(secrets_path) = toml_val.get("secrets_file").and_then(|v| v.as_str()) {
        let sp = secret_path("secrets_file", secrets_path)?;
        if sp.exists() {
            let srw = std::fs::read_to_string(&sp)
                .with_context(|| format!("reading secrets file {}", sp.display()))?;
            let secrets: toml::Value = toml::from_str(&srw)
                .with_context(|| format!("parsing secrets file {}", sp.display()))?;
            trace.add_config_file(&sp, ConfigRole::Secrets, &srw, &secrets);

            // ONE KEY, ONE OWNER. A key set in both files is refused here.
            //
            // This used to be a plain merge with "src wins", which meant a config
            // file could state a value that was not the one in use, silently, with
            // nothing logged and neither file mentioning the other.
            //
            // It cost a wrong conclusion about a live system. A deployed renderer
            // config read `from = "noreply@example.com"`,
            // `to = "someone@example.net"`, `host = "localhost"`, `port = 1025`
            // -- and all four were inert, because the secrets file set them.
            // The form was relaying through a real provider and sending as a
            // different domain entirely. An audit read the deployed config and
            // believed it. A file that is overridden is indistinguishable from a
            // file that is correct, when you are looking at one file.
            //
            // The deployment's stated reason for the overlap was that a missing
            // secrets file should fail loudly rather than quietly succeed against
            // the wrong host, so the base config pointed at a dead relay on
            // purpose. That intent is right and the mechanism inverted it: the way
            // to fail loudly for a missing required value is for the value to be
            // ABSENT. Present-and-deliberately-wrong is what made the config lie.
            let mut clashes = Vec::new();
            find_key_clashes(&toml_val, &secrets, "", &mut clashes);
            if !clashes.is_empty() {
                return Err(anyhow::anyhow!(
                    "{} key(s) are set in both the config and its secrets file, so \
                     which value applies cannot be read from either file:\n{}\n\n\
                     config:  {}\n  secrets: {}\n\n\
                     Each key must have exactly one owner. Put a secret only in the \
                     secrets file and remove it from the config; a value that is not \
                     secret belongs only in the config. If the config held a \
                     deliberately broken placeholder so that a missing secrets file \
                     would fail loudly, delete the placeholder -- an absent required \
                     key fails loudly by itself, and says which key is missing.",
                    clashes.len(),
                    clashes
                        .iter()
                        .map(|k| format!("  {k}"))
                        .collect::<Vec<_>>()
                        .join("\n"),
                    config_path.display(),
                    sp.display(),
                ));
            }

            merge_toml(&mut toml_val, secrets);
        }
        // Silently ignore if absent.
    }

    let limits = crate::limits::Limits::from_toml(
        toml_val.get("limits"),
        config_path,
        crate::platform::process::page_size(),
    )?;
    let mut config = parse_config(toml_val, site_dir)?;
    config.limits = limits;
    Ok((config, trace))
}

/// Load the effective config without the source trace.
pub fn load(config_path: &Path, site_dir: &Path) -> anyhow::Result<RendererConfig> {
    load_traced(config_path, site_dir).map(|(config, _)| config)
}

/// The path a setting that names a secret, key or certificate holds, which
/// must be absolute.
///
/// A relative path resolves against a base the reader of a config has to
/// know, and for a secret that base decides which file is read. systemd's
/// credential directory and /etc/m6 are both absolute, so no deployment
/// needs a relative one. Every crate calls this, so the rule and its message
/// are the same everywhere.
pub fn secret_path(setting: &str, value: &str) -> anyhow::Result<PathBuf> {
    let path = PathBuf::from(value);
    if path.is_absolute() {
        Ok(path)
    } else {
        anyhow::bail!(
            "{setting} = {value:?} is a relative path. A secret, key or certificate \
             is named by an absolute path, for example /etc/m6/<file> or \
             /run/credentials/<unit>/<name>"
        )
    }
}

/// Every leaf key present in BOTH trees, as dotted paths.
///
/// Tables are descended into rather than reported: two files each contributing
/// different keys to the same `[smtp]` section is the whole point of having a
/// secrets file, and is not a clash. A clash is one VALUE claimed twice.
///
/// An empty string is a value like any other. `username = ""` in the config
/// beside a real username in the secrets file is exactly the case that made the
/// config unreadable, so it is reported rather than treated as "unset".
fn find_key_clashes(
    base: &toml::Value,
    secrets: &toml::Value,
    prefix: &str,
    out: &mut Vec<String>,
) {
    let (Some(b), Some(s)) = (base.as_table(), secrets.as_table()) else {
        return;
    };
    for (k, sv) in s {
        let Some(bv) = b.get(k) else { continue };
        let path = if prefix.is_empty() {
            k.clone()
        } else {
            format!("{prefix}.{k}")
        };
        match (bv.is_table(), sv.is_table()) {
            // Both tables: not a clash in itself, look inside.
            (true, true) => find_key_clashes(bv, sv, &path, out),
            // One is a table and the other is not: the shapes disagree, which is
            // a clash and a confusing one, so it is named too.
            _ => out.push(path),
        }
    }
}

/// Deep-merge `src` into `dst`; `src` wins on conflict.
///
/// Reachable only after `find_key_clashes` has confirmed there are no leaf
/// conflicts, so in practice `src` never replaces a value -- it only adds keys.
/// The conflict arm is kept because this function is a general merge and a future
/// caller may not have that guarantee.
fn merge_toml(dst: &mut toml::Value, src: toml::Value) {
    match (dst, src) {
        (toml::Value::Table(d), toml::Value::Table(s)) => {
            for (k, v) in s {
                let entry = d
                    .entry(k)
                    .or_insert(toml::Value::Table(toml::map::Map::new()));
                merge_toml(entry, v);
            }
        }
        (dst, src) => *dst = src,
    }
}

fn default_compression() -> std::collections::HashMap<String, CompressionLevel> {
    let mut m = std::collections::HashMap::new();
    let text_types = [
        "text/html",
        "text/css",
        "text/plain",
        "application/javascript",
        "text/javascript",
        "application/json",
        "image/svg+xml",
    ];
    for t in &text_types {
        m.insert(t.to_string(), CompressionLevel { brotli: 6, gzip: 6 });
    }
    // Images and binary types default to no compression (brotli=0,gzip=0),
    // but we only need to store explicit overrides; absence means 0.
    m
}

/// Refuse an unknown key inside one of the core config tables `parse_config`
/// reads, so a typo in a key core acts on cannot pass silently as a default.
///
/// Out of scope: top-level keys (a service's own config, `user_config`), the
/// keys of a `[[route]]` beyond core's (kept as the route's `settings` by
/// design, `parse_routes`), `[errors]` (m6-http's, reserved here so it does not
/// reach the dictionary) and `[multipart]` (an ordinary service key).
fn check_keys(tv: &toml::Value, table: &str, known: &[&str]) -> anyhow::Result<()> {
    let Some(val) = tv.get(table) else {
        return Ok(());
    };
    let tbl = val
        .as_table()
        .ok_or_else(|| anyhow::anyhow!("[{table}] must be a table"))?;
    if let Some(key) = tbl.keys().find(|k| !known.contains(&k.as_str())) {
        anyhow::bail!(
            "unknown key [{table}] {key}: known keys are {}",
            known.join(", ")
        );
    }
    Ok(())
}

fn parse_config(tv: toml::Value, _site_dir: &Path) -> anyhow::Result<RendererConfig> {
    if tv.get("thread_pool").is_some() {
        anyhow::bail!(
            "[thread_pool] is removed: a service runs one event loop on one core; \
             run more instances of the service to use more cores"
        );
    }
    check_keys(&tv, "params_cache", &["size"])?;
    check_keys(
        &tv,
        "server",
        &["read_timeout_s", "drain_timeout_s", "socket_mode"],
    )?;
    // The one `[log]` parse, kept here so a bad log table is refused before the
    // rest of the file is read.
    let log = LogConfig::from_toml(tv.get("log"))?;
    if let Some(toml::Value::Table(tbl)) = tv.get("compression") {
        for (mime, val) in tbl {
            let table = format!("compression.\"{mime}\"");
            let inner = val
                .as_table()
                .ok_or_else(|| anyhow::anyhow!("[{table}] must be a table"))?;
            if let Some(key) = inner.keys().find(|k| *k != "brotli" && *k != "gzip") {
                anyhow::bail!("unknown key [{table}] {key}: known keys are brotli, gzip");
            }
        }
    }

    // --- params_cache ---
    let pc_size = tv
        .get("params_cache")
        .and_then(|t| t.get("size"))
        .and_then(|v| v.as_integer())
        .map(|v| v as usize)
        .unwrap_or(256);

    // --- server ---
    // Both keys refuse a value they cannot make sense of rather than quietly
    // substituting a default. A service that will not start says so on the
    // first line of its journal; a service that started with a socket mode or
    // a deadline nobody chose looks healthy, which is lesson 12 and the reason
    // the `[server]` section is the one place it would hurt most.
    //
    // 30 seconds is not a new number: m6-file and m6-auth-server each picked it
    // by hand for the same reason, so it is the value already in service.
    let server_secs = |key: &str, default: u64| -> anyhow::Result<u64> {
        match tv.get("server").and_then(|t| t.get(key)) {
            None => Ok(default),
            Some(v) => {
                let n = v
                    .as_integer()
                    .ok_or_else(|| anyhow::anyhow!("[server] {key} must be an integer, got {v}"))?;
                u64::try_from(n)
                    .map_err(|_| anyhow::anyhow!("[server] {key} must not be negative, got {n}"))
            }
        }
    };
    let read_timeout =
        match server_secs("read_timeout_s", crate::server::DEFAULT_READ_TIMEOUT_SECS)? {
            0 => None,
            secs => Some(std::time::Duration::from_secs(secs)),
        };
    let drain_timeout = std::time::Duration::from_secs(server_secs(
        "drain_timeout_s",
        crate::server::DEFAULT_DRAIN_TIMEOUT_SECS,
    )?);

    // An octal string, as systemd writes it. See `ServerConfig::socket_mode`
    // for why this is not an integer.
    let socket_mode = match tv.get("server").and_then(|t| t.get("socket_mode")) {
        None => crate::server::DEFAULT_SOCKET_MODE,
        Some(v) => {
            let s = v.as_str().ok_or_else(|| {
                anyhow::anyhow!(
                    "[server] socket_mode must be an octal string such as \"0660\", got {v}"
                )
            })?;
            let mode = u32::from_str_radix(s.trim_start_matches("0o"), 8).map_err(|_| {
                anyhow::anyhow!("[server] socket_mode {s:?} is not an octal number")
            })?;
            if mode > 0o777 {
                anyhow::bail!("[server] socket_mode {s:?} sets bits above 0777");
            }
            mode
        }
    };

    // --- global_params ---
    let global_params: Vec<String> = tv
        .get("global_params")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();

    // --- routes ---
    let routes = parse_routes(tv.get("route"))?;

    // --- compression ---
    let mut compression = default_compression();
    if let Some(toml::Value::Table(tbl)) = tv.get("compression") {
        for (mime, val) in tbl {
            let brotli = val.get("brotli").and_then(|v| v.as_integer()).unwrap_or(0) as u32;
            let gzip = val.get("gzip").and_then(|v| v.as_integer()).unwrap_or(0) as u32;
            compression.insert(mime.clone(), CompressionLevel { brotli, gzip });
        }
    }

    // --- minification ---
    let minification = parse_minification(tv.get("minification"), cfg!(feature = "minify"))?;

    // --- user config (strip framework keys) ---
    let mut user_config = Map::new();
    if let toml::Value::Table(tbl) = &tv {
        for (k, v) in tbl {
            if !FRAMEWORK_KEYS.contains(&k.as_str()) {
                user_config.insert(k.clone(), toml_to_json(v));
            }
        }
    }

    // --- site_size_budget_mb ---
    //
    // A budget of zero would warn on every root, which reads as a broken
    // setting rather than a choice. Refuse it here, before a service starts.
    let site_size_budget_mb = match tv.get("site_size_budget_mb") {
        None => None,
        Some(v) => {
            let n = v.as_integer().ok_or_else(|| {
                anyhow::anyhow!("site_size_budget_mb must be an integer, got {v}")
            })?;
            let n = u64::try_from(n).map_err(|_| {
                anyhow::anyhow!("site_size_budget_mb must not be negative, got {n}")
            })?;
            if n == 0 {
                anyhow::bail!("site_size_budget_mb must be at least 1, got 0");
            }
            Some(n)
        }
    };

    // --- site_toml ---
    let site_toml = match tv.get("site_toml") {
        None => None,
        Some(toml::Value::String(s)) if s.is_empty() => {
            anyhow::bail!("site_toml must not be empty")
        }
        Some(toml::Value::String(s)) => Some(PathBuf::from(s)),
        Some(v) => anyhow::bail!("site_toml must be a path string, got {v}"),
    };

    Ok(RendererConfig {
        user_config,
        global_params,
        routes,
        params_cache: ParamsCacheConfig { size: pc_size },
        server: ServerConfig {
            read_timeout,
            drain_timeout,
            socket_mode,
        },
        compression,
        minification,
        log,
        limits: crate::limits::Limits::default(),
        site_toml,
        site_size_budget_mb,
    })
}

/// Reads `[minification]`. The minifiers are only linked with the `minify`
/// feature, so without them nothing is on by default and a config that turns
/// minification on is refused, rather than quietly serving unminified bodies.
fn parse_minification(
    table: Option<&toml::Value>,
    minifiers_linked: bool,
) -> anyhow::Result<MinificationConfig> {
    let mut minification = if minifiers_linked {
        default_minification()
    } else {
        MinificationConfig::default()
    };
    if let Some(toml::Value::Table(tbl)) = table {
        for (key, val) in tbl {
            let on = val.as_bool().ok_or_else(|| {
                anyhow::anyhow!("[minification] {key} must be true or false, got {val}")
            })?;
            minification.enabled.insert(key.clone(), on);
        }
    }
    let asked = minification.enabled.values().any(|&on| on);
    if asked && !minifiers_linked {
        anyhow::bail!(
            "[minification] turns minification on, but this build of m6-core has no minifiers: \
             build it with the `minify` feature"
        );
    }
    Ok(minification)
}

fn parse_routes(val: Option<&toml::Value>) -> anyhow::Result<Vec<RouteConfig>> {
    let arr = match val {
        Some(toml::Value::Array(a)) => a,
        None => return Ok(vec![]),
        _ => anyhow::bail!("[[route]] must be an array"),
    };

    let mut routes = Vec::new();
    for item in arr {
        let path = item
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("route missing `path`"))?
            .to_string();

        let template = item
            .get("template")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        let params: Vec<String> = item
            .get("params")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();

        let status = item
            .get("status")
            .and_then(|v| v.as_integer())
            .unwrap_or(200) as u16;

        let handler = item
            .get("handler")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        // A handler route defaults to `no-store`, a template route to
        // `PAGE_CACHE_CONTROL`, and an explicit `cache` wins over both.
        //
        // The default depends on the kind of route because the two are not the
        // same kind of thing: a template renders a document from files on disk,
        // a handler computes an answer. Letting a handler route inherit the
        // page default would put dynamic output in a shared cache by omission,
        // which is the shape of defect that is found by someone else seeing
        // another user's page. A code route defaults to `no-store` for the same
        // reason; this is that rule reaching the config-declared form.
        //
        // A page is still cacheable, and the header says how: the browser
        // revalidates every time, and the shared cache keeps it for a day. The
        // ETag that makes the 304 possible is added by the finish path from
        // the final body bytes.
        let cache_explicit = item.get("cache").is_some();
        let cache = item
            .get("cache")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| {
                if handler.is_some() {
                    "no-store".to_string()
                } else {
                    PAGE_CACHE_CONTROL.to_string()
                }
            });

        let methods: Option<Vec<String>> =
            item.get("methods").and_then(|v| v.as_array()).map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_uppercase()))
                    .collect()
            });

        let headers: Vec<(String, String)> = item
            .get("headers")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|pair| {
                        let a = pair.as_array()?;
                        let k = a.first()?.as_str()?.to_string();
                        let v = a.get(1)?.as_str()?.to_string();
                        Some((k, v))
                    })
                    .collect()
            })
            .unwrap_or_default();

        // Everything core does not define is kept rather than dropped, so a
        // service can carry its own per-route vocabulary. Unknown keys were
        // previously discarded in silence, which is indistinguishable from a
        // typo being honoured.
        const KNOWN: [&str; 8] = [
            "path", "template", "params", "status", "cache", "methods", "headers", "handler",
        ];
        let mut settings = Map::new();
        if let Some(table) = item.as_table() {
            for (k, v) in table {
                if !KNOWN.contains(&k.as_str()) {
                    settings.insert(k.clone(), toml_to_json(v));
                }
            }
        }

        routes.push(RouteConfig {
            path,
            template,
            params,
            status,
            cache,
            cache_explicit,
            methods,
            headers,
            handler,
            settings: std::sync::Arc::new(settings),
        });
    }

    Ok(routes)
}

/// Convert a `toml::Value` to `serde_json::Value`.
pub fn toml_to_json(v: &toml::Value) -> Value {
    match v {
        toml::Value::String(s) => Value::String(s.clone()),
        toml::Value::Integer(i) => Value::Number((*i).into()),
        toml::Value::Float(f) => serde_json::Number::from_f64(*f)
            .map(Value::Number)
            .unwrap_or(Value::Null),
        toml::Value::Boolean(b) => Value::Bool(*b),
        toml::Value::Array(a) => Value::Array(a.iter().map(toml_to_json).collect()),
        toml::Value::Table(t) => {
            let mut m = Map::new();
            for (k, val) in t {
                m.insert(k.clone(), toml_to_json(val));
            }
            Value::Object(m)
        }
        toml::Value::Datetime(dt) => Value::String(dt.to_string()),
    }
}

// `socket_path_from_config` used to live here as well as in `server.rs`, and
// the two did not agree: this one took `file_stem().unwrap_or_default()`, so a
// path with no stem produced `/run/m6/.sock`, a hidden file in the socket
// directory. `server.rs` falls back to `m6-default`. That one is the survivor.

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    /// Compressed by default and deliberately not minified.
    const NOT_MINIFIED: &[&str] = &[
        "image/svg+xml",
        "text/plain",
        "application/javascript",
        "text/javascript",
    ];

    #[test]
    fn every_minified_type_is_also_compressed_by_default() {
        let compression = default_compression();
        let minification = default_minification();
        let mut missing: Vec<&String> = minification
            .enabled
            .keys()
            .filter(|k| !compression.contains_key(*k))
            .collect();
        missing.sort();
        assert!(
            missing.is_empty(),
            "minified but not compressed: {missing:?}"
        );
        let mut extra: Vec<&String> = compression
            .keys()
            .filter(|k| !minification.enabled.contains_key(*k))
            .filter(|k| !NOT_MINIFIED.contains(&k.as_str()))
            .collect();
        extra.sort();
        assert!(extra.is_empty(), "compressed but not minified: {extra:?}");
    }

    #[test]
    fn minification_asked_for_without_the_minify_feature_is_refused_by_name() {
        for body in ["\"text/html\" = true", "\"text/css\" = true"] {
            let table: toml::Value = toml::from_str(&format!("[minification]\n{body}"))
                .map(|v: toml::Value| v["minification"].clone())
                .unwrap();
            let err = parse_minification(Some(&table), false)
                .unwrap_err()
                .to_string();
            assert!(err.contains("`minify` feature"), "{body}: {err}");
        }
    }

    #[test]
    fn a_build_without_the_minify_feature_loads_a_config_that_leaves_minification_off() {
        let none = parse_minification(None, false).unwrap();
        assert!(!none.is_enabled("text/html"));
        let off: toml::Value = toml::from_str("\"text/html\" = false").unwrap();
        assert!(!parse_minification(Some(&off), false)
            .unwrap()
            .is_enabled("text/html"));
    }

    #[test]
    fn a_build_with_the_minify_feature_minifies_by_default() {
        assert!(parse_minification(None, true)
            .unwrap()
            .is_enabled("text/html"));
    }

    #[test]
    fn test_basic_config() {
        let mut f = NamedTempFile::new().unwrap();
        write!(
            f,
            r#"
global_params = ["data/site.json"]
site_name = "Test"

[[route]]
path = "/"
template = "templates/home.html"
"#
        )
        .unwrap();

        let cfg = load(f.path(), Path::new("/tmp")).unwrap();
        assert_eq!(cfg.global_params, vec!["data/site.json"]);
        assert_eq!(cfg.routes.len(), 1);
        assert_eq!(cfg.user_config.get("site_name").unwrap(), "Test");
    }

    /// Load a config whose whole text is `body`.
    fn load_text(body: &str) -> anyhow::Result<RendererConfig> {
        let mut f = NamedTempFile::new().unwrap();
        write!(f, "{body}").unwrap();
        load(f.path(), Path::new("/tmp"))
    }

    /// `[limits]` is read by `load` with the file's own path, kept out of the
    /// request dictionary, and a value that cannot work is refused naming the
    /// file.
    #[test]
    fn limits_are_read_from_the_file_and_kept_out_of_the_dictionary() {
        let cfg = load_text("[limits.render_page]\nstart = \"1MiB\"\nmax = \"8MiB\"\n").unwrap();
        let page = cfg.limits.get(&crate::limits::RENDER_PAGE);
        assert_eq!((page.start(), page.max()), (1 << 20, 8 << 20));
        assert!(cfg.user_config.get("limits").is_none());

        let mut f = NamedTempFile::new().unwrap();
        write!(f, "[limits.render_page]\nmax = \"4KiB\"\n").unwrap();
        let err = load(f.path(), Path::new("/tmp")).unwrap_err().to_string();
        assert!(err.contains("is below start = 512 KiB"), "{err}");
        assert!(err.contains(&f.path().display().to_string()), "{err}");
    }

    #[test]
    fn site_toml_absent_resolves_to_the_site_dir() {
        let cfg = load_text("site_name = \"x\"\n").unwrap();
        assert_eq!(cfg.site_toml, None);
        assert_eq!(
            cfg.resolve_site_toml(Path::new("/srv/site")),
            PathBuf::from("/srv/site/site.toml")
        );
    }

    #[test]
    fn site_size_budget_is_absent_by_default() {
        let cfg = load_text("site_name = \"x\"\n").unwrap();
        assert_eq!(cfg.site_size_budget_mb, None);
        assert!(!cfg.user_config.contains_key("site_size_budget_mb"));
    }

    #[test]
    fn site_size_budget_is_read_and_reserved() {
        let cfg = load_text("site_size_budget_mb = 256\n").unwrap();
        assert_eq!(cfg.site_size_budget_mb, Some(256));
        // The key is a start setting, not a template variable.
        assert!(!cfg.user_config.contains_key("site_size_budget_mb"));
    }

    #[test]
    fn site_size_budget_zero_is_refused() {
        let err = load_text("site_size_budget_mb = 0\n").unwrap_err();
        assert!(err.to_string().contains("site_size_budget_mb"), "{err}");
        assert!(err.to_string().contains("at least 1"), "{err}");
    }

    #[test]
    fn site_size_budget_that_is_not_an_integer_is_refused() {
        let err = load_text("site_size_budget_mb = \"128\"\n").unwrap_err();
        assert!(err.to_string().contains("site_size_budget_mb"), "{err}");
    }

    #[test]
    fn site_toml_relative_resolves_against_the_site_dir() {
        let cfg = load_text("site_toml = \"../../site.toml\"\n").unwrap();
        assert_eq!(
            cfg.resolve_site_toml(Path::new("/srv/site/apps/file")),
            PathBuf::from("/srv/site/apps/file/../../site.toml")
        );
        // The key is reserved, so it is not a template variable.
        assert!(!cfg.user_config.contains_key("site_toml"));
    }

    #[test]
    fn a_multipart_table_is_an_ordinary_service_key() {
        let cfg = load_text("site_name = \"x\"\n\n[multipart]\nmax_size_mb = 10\n").unwrap();
        let multipart = cfg
            .user_config
            .get("multipart")
            .expect("a [multipart] table reaches user_config");
        assert_eq!(multipart["max_size_mb"].as_i64(), Some(10));
        assert_eq!(cfg.user_config.get("site_name").unwrap(), "x");
    }

    #[test]
    fn site_toml_absolute_is_used_as_given() {
        let cfg = load_text("site_toml = \"/etc/m6/site.toml\"\n").unwrap();
        assert_eq!(
            cfg.resolve_site_toml(Path::new("/srv/site")),
            PathBuf::from("/etc/m6/site.toml")
        );
    }

    #[test]
    fn a_non_string_site_toml_is_refused() {
        let err = load_text("site_toml = 3\n").unwrap_err().to_string();
        assert!(err.contains("site_toml must be a path string"), "{err}");
        let err = load_text("site_toml = \"\"\n").unwrap_err().to_string();
        assert!(err.contains("site_toml must not be empty"), "{err}");
    }

    /// Parse a `[server]` body, or return the refusal message.
    fn server_cfg(body: &str) -> anyhow::Result<ServerConfig> {
        let mut f = NamedTempFile::new().unwrap();
        write!(f, "[server]\n{body}\n").unwrap();
        load(f.path(), Path::new("/tmp")).map(|c| c.server)
    }

    /// The defaults are the contract for every config that says nothing, which
    /// is most of them: the [server] section is optional in full.
    #[test]
    fn server_defaults_are_thirty_seconds_and_0660() {
        let mut f = NamedTempFile::new().unwrap();
        writeln!(f, "site_name = \"Test\"").unwrap();
        let cfg = load(f.path(), Path::new("/tmp")).unwrap();
        assert_eq!(
            cfg.server.read_timeout,
            Some(std::time::Duration::from_secs(30))
        );
        assert_eq!(cfg.server.drain_timeout, std::time::Duration::from_secs(30));
        assert_eq!(cfg.server.socket_mode, 0o660);
    }

    #[test]
    fn server_keys_are_read() {
        let s =
            server_cfg("read_timeout_s = 5\ndrain_timeout_s = 7\nsocket_mode = \"0600\"").unwrap();
        assert_eq!(s.read_timeout, Some(std::time::Duration::from_secs(5)));
        assert_eq!(s.drain_timeout, std::time::Duration::from_secs(7));
        assert_eq!(s.socket_mode, 0o600);
    }

    /// `drain_timeout_s = 0` waits for nothing, which is how a service asks
    /// for the stop-at-once behaviour that every service had while the drain
    /// was removed.
    #[test]
    fn a_zero_drain_timeout_waits_for_nothing() {
        assert_eq!(
            server_cfg("drain_timeout_s = 0").unwrap().drain_timeout,
            std::time::Duration::ZERO
        );
    }

    /// Zero is the documented way to ask for the old no-timeout behaviour, so
    /// it has to survive as `None` rather than becoming a zero-length deadline,
    /// which would time out every read instantly.
    #[test]
    fn a_zero_read_timeout_means_no_timeout() {
        assert_eq!(server_cfg("read_timeout_s = 0").unwrap().read_timeout, None);
    }

    /// `socket_mode` is octal wherever it is written, so `"0660"` and `"660"`
    /// are the same mode and neither is decimal 660.
    #[test]
    fn socket_mode_is_octal_with_or_without_the_leading_zero() {
        assert_eq!(
            server_cfg("socket_mode = \"0660\"").unwrap().socket_mode,
            0o660
        );
        assert_eq!(
            server_cfg("socket_mode = \"660\"").unwrap().socket_mode,
            0o660
        );
        assert_eq!(
            server_cfg("socket_mode = \"0o660\"").unwrap().socket_mode,
            0o660
        );
    }

    /// A value that cannot be understood stops the service rather than being
    /// silently replaced by a default. A socket running at a mode nobody chose
    /// looks exactly like one running at the right mode.
    #[test]
    fn a_nonsense_server_value_is_refused_not_defaulted() {
        for body in [
            "read_timeout_s = -1",
            "read_timeout_s = \"30\"",
            "drain_timeout_s = -1",
            "drain_timeout_s = \"30\"",
            "socket_mode = 660",
            "socket_mode = \"rw-rw----\"",
            "socket_mode = \"0999\"",
            "socket_mode = \"7777\"",
        ] {
            assert!(
                server_cfg(body).is_err(),
                "{body:?} should have been refused, got {:?}",
                server_cfg(body).map(|s| (s.read_timeout, s.socket_mode))
            );
        }
    }

    /// A secrets file SUPPLIES a value the config does not have.
    ///
    /// This test was `test_secrets_override` and asserted the opposite: that a
    /// `password` in the config was silently replaced by the one in the secrets
    /// file. That behaviour is gone, and the test is inverted rather than deleted,
    /// because the old assertion is the clearest statement of what changed.
    ///
    /// The override let a config file state a value that was not in use. On the
    /// production origin four keys in `render-contact.conf` were inert that way,
    /// and an audit of where the site sends mail from read the deployed config and
    /// drew the wrong conclusion. See `secrets_overlay_tests`.
    #[test]
    fn test_secrets_supply_a_value_the_config_does_not_set() {
        let mut secrets = NamedTempFile::new().unwrap();
        writeln!(secrets, "password = \"secret\"").unwrap();

        let mut cfg_file = NamedTempFile::new().unwrap();
        // No `password` here. That is the point: one owner per key.
        writeln!(cfg_file, "secrets_file = {:?}", secrets.path()).unwrap();

        let cfg = load(cfg_file.path(), Path::new("/tmp")).unwrap();
        assert_eq!(
            cfg.user_config.get("password").unwrap().as_str().unwrap(),
            "secret"
        );
    }

    /// And setting it in both is refused, which is what used to be an override.
    #[test]
    fn test_secrets_no_longer_override_the_config() {
        let mut secrets = NamedTempFile::new().unwrap();
        writeln!(secrets, "password = \"secret\"").unwrap();

        let mut cfg_file = NamedTempFile::new().unwrap();
        write!(
            cfg_file,
            "password = \"dev\"\nsecrets_file = {:?}\n",
            secrets.path()
        )
        .unwrap();

        let err = load(cfg_file.path(), Path::new("/tmp"))
            .expect_err("a key in both files must be refused, not silently resolved");
        assert!(format!("{err}").contains("password"), "{err}");
    }

    #[test]
    fn test_secrets_absent_ignored() {
        let mut cfg_file = NamedTempFile::new().unwrap();
        writeln!(cfg_file, "secrets_file = \"/nonexistent/path/file.toml\"").unwrap();
        // Should not error
        load(cfg_file.path(), Path::new("/tmp")).unwrap();
    }

    #[test]
    fn test_secrets_malformed_errors() {
        let mut secrets = NamedTempFile::new().unwrap();
        write!(secrets, "not valid toml {{{{").unwrap();

        let mut cfg_file = NamedTempFile::new().unwrap();
        writeln!(cfg_file, "secrets_file = {:?}", secrets.path()).unwrap();

        assert!(load(cfg_file.path(), Path::new("/tmp")).is_err());
    }

    #[test]
    fn thread_pool_is_refused() {
        let err = load_text("[thread_pool]\nsize = 4\n")
            .unwrap_err()
            .to_string();
        assert!(
            err.contains(
                "[thread_pool] is removed: a service runs one event loop on one core; \
                 run more instances of the service to use more cores"
            ),
            "{err}"
        );
    }

    #[test]
    fn an_unknown_key_in_a_core_table_is_refused_and_named() {
        for (body, table, key) in [
            ("[server]\nread_timeout = 5\n", "server", "read_timeout"),
            ("[log]\nlevle = \"info\"\n", "log", "levle"),
            ("[log]\nformat = \"text\"\n", "log", "format"),
            (
                "[compression.\"text/html\"]\nbrottli = 5\n",
                "compression.\"text/html\"",
                "brottli",
            ),
        ] {
            let err = load_text(body).unwrap_err().to_string();
            assert!(err.contains(table), "error should name {table:?}: {err}");
            assert!(err.contains(key), "error should name {key:?}: {err}");
        }
    }

    #[test]
    fn a_non_boolean_minification_value_is_refused() {
        let err = load_text("[minification]\n\"text/html\" = \"yes\"\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("must be true or false"), "{err}");
    }

    /// The one `[log]` parse. A service config and site.toml reach
    /// `LogConfig::from_toml` with the same table, so this covers the refusals
    /// every process gives, and the level a file did set.
    #[test]
    fn the_one_log_parse_reads_a_level_and_refuses_the_rest() {
        let v: toml::Value = toml::from_str("[log]\nlevel = \"warn\"\n").unwrap();
        let log = LogConfig::from_toml(v.get("log")).unwrap();
        assert_eq!(log.level.as_deref(), Some("warn"));

        for (body, needle) in [
            ("log = 3\n", "[log] must be a table"),
            ("[log]\nformat = \"text\"\n", "unknown key [log] format"),
            ("[log]\nlevel = 3\n", "[log] level must be a string"),
        ] {
            let v: toml::Value = toml::from_str(body).unwrap();
            let err = LogConfig::from_toml(v.get("log")).unwrap_err().to_string();
            assert!(err.contains(needle), "{body:?}: {err}");
        }

        let absent: toml::Value = toml::from_str("").unwrap();
        assert_eq!(LogConfig::from_toml(absent.get("log")).unwrap().level, None);
    }

    #[test]
    fn a_service_key_outside_the_core_tables_still_loads() {
        let cfg = load_text("[contact]\nname = \"Contact\"\n").unwrap();
        assert!(cfg.user_config.contains_key("contact"));
    }
}

/// A key belongs to exactly one file.
///
/// ## The defect these cover
///
/// `load` merged the secrets file over the config with "src wins on conflict",
/// silently. A config file could therefore state a value that was not the one in
/// use, with nothing logged and neither file mentioning the other.
///
/// It cost a wrong conclusion about a live system. A deployed renderer config
/// read `from = "noreply@example.com"`, `to = "someone@example.net"`,
/// `host = "localhost"` and `port = 1025`, and all four were inert because the
/// secrets file set them. The form was relaying through a
/// real provider and sending as an entirely different domain. An audit read the
/// deployed config and believed it, because a file that is overridden looks
/// exactly like a file that is correct when you are holding one file.
///
/// Nothing caught it because every test wrote keys into one file or the other and
/// never the same key into both, which is the one case where the old code was
/// unambiguous.
#[cfg(test)]
mod secrets_overlay_tests {
    use super::*;
    use std::io::Write;
    use tempfile::{NamedTempFile, TempDir};

    /// Write a config that points at a secrets file, and the secrets file.
    /// Returns the result of loading it.
    fn load_pair(config_body: &str, secrets_body: &str) -> anyhow::Result<RendererConfig> {
        let dir = TempDir::new().unwrap();
        let secrets_path = dir.path().join("secrets.toml");
        std::fs::write(&secrets_path, secrets_body).unwrap();

        let mut cfg = NamedTempFile::new().unwrap();
        writeln!(cfg, "secrets_file = \"{}\"", secrets_path.display()).unwrap();
        write!(cfg, "{config_body}").unwrap();
        cfg.flush().unwrap();

        load(cfg.path(), dir.path())
    }

    /// The regression test. This is the exact shape the production config had.
    #[test]
    fn a_key_set_in_both_files_is_refused() {
        let r = load_pair(
            r#"
[smtp]
host = "localhost"
port = 1025
from = "noreply@example.com"
"#,
            r#"
[smtp]
host = "smtp.provider.example"
port = 587
from = "real@example.com"
username = "someone"
"#,
        );
        let err = match r {
            Ok(_) => panic!(
                "a config that sets smtp.host/port/from AND a secrets file that sets \
                 the same three was accepted. Which value applies cannot be read \
                 from either file: this is the defect."
            ),
            Err(e) => format!("{e}"),
        };
        // Every clashing key is named, because fixing one at a time with a
        // restart between is how a five-key overlap takes five deploys.
        for key in ["smtp.host", "smtp.port", "smtp.from"] {
            assert!(err.contains(key), "the error should name {key}:\n{err}");
        }
        // And the key that is only in the secrets file must NOT be reported.
        assert!(
            !err.contains("smtp.username"),
            "smtp.username is set in one file only and is not a clash:\n{err}"
        );
    }

    /// The whole point of a secrets file: it contributes keys the config does not
    /// have, including into a section the config also uses.
    #[test]
    fn a_secrets_file_may_add_keys_to_a_section_the_config_also_uses() {
        let cfg = load_pair(
            r#"
[smtp]
host = "smtp.provider.example"
port = 587
"#,
            r#"
[smtp]
username = "someone"
password = "hunter2"
"#,
        )
        .expect("adding new keys to a shared section is not a clash");
        let smtp = cfg.user_config.get("smtp").expect("smtp section survives");
        // All four present: two from each file.
        for k in ["host", "port", "username", "password"] {
            assert!(smtp.get(k).is_some(), "{k} missing from the merged config");
        }
    }

    /// An empty string is a value, not "unset".
    ///
    /// `username = ""` beside a real username is precisely how the production
    /// config came to read as though it held the credentials. Treating `""` as
    /// absent would keep that case silent, so it is a clash.
    #[test]
    fn an_empty_placeholder_is_still_a_clash() {
        let r = load_pair(
            "[smtp]\nusername = \"\"\npassword = \"\"\n",
            "[smtp]\nusername = \"someone\"\npassword = \"hunter2\"\n",
        );
        let err = match r {
            Ok(_) => panic!("empty-string placeholders were treated as unset"),
            Err(e) => format!("{e}"),
        };
        assert!(err.contains("smtp.username"), "{err}");
        assert!(err.contains("smtp.password"), "{err}");
    }

    /// The error has to be actionable: both paths, so the reader knows which two
    /// files to look at without guessing.
    #[test]
    fn the_error_names_both_files_and_says_what_to_do() {
        let err = format!(
            "{}",
            load_pair("[smtp]\nhost = \"a\"\n", "[smtp]\nhost = \"b\"\n").unwrap_err()
        );
        assert!(
            err.contains("secrets.toml"),
            "names the secrets file:\n{err}"
        );
        assert!(
            err.contains("exactly one owner"),
            "says what the rule is:\n{err}"
        );
        assert!(
            err.contains("fails loudly"),
            "addresses the deliberate-placeholder case, which is why the overlap \
             existed in the first place:\n{err}"
        );
    }

    /// A top-level key, not only one inside a section.
    #[test]
    fn a_top_level_key_clash_is_caught_too() {
        let err = format!(
            "{}",
            load_pair("site_name = \"A\"\n", "site_name = \"B\"\n").unwrap_err()
        );
        assert!(err.contains("site_name"), "{err}");
    }

    /// A section in one file and a scalar of the same name in the other is a
    /// clash, and a confusing one, so it is named rather than silently merged.
    #[test]
    fn a_table_against_a_scalar_is_a_clash() {
        let err = format!(
            "{}",
            load_pair("smtp = \"nonsense\"\n", "[smtp]\nhost = \"a\"\n").unwrap_err()
        );
        assert!(err.contains("smtp"), "{err}");
    }

    /// No secrets file at all is not an error. A service that needs a value which
    /// nothing provides fails on the missing value, which names what is missing.
    #[test]
    fn an_absent_secrets_file_is_not_an_error() {
        let dir = TempDir::new().unwrap();
        let mut cfg = NamedTempFile::new().unwrap();
        writeln!(
            cfg,
            "secrets_file = \"{}\"",
            dir.path().join("does-not-exist.toml").display()
        )
        .unwrap();
        writeln!(cfg, "site_name = \"A\"").unwrap();
        cfg.flush().unwrap();
        load(cfg.path(), dir.path()).expect("an absent secrets file is tolerated");
    }

    /// A relative path for a secret, key or certificate is refused, and the
    /// error names the setting and the value.
    #[test]
    fn secret_path_refuses_a_relative_path_and_names_the_setting() {
        for value in ["keys/x.pem", "./x.pem", ""] {
            let err = secret_path("secrets_file", value).unwrap_err().to_string();
            assert!(err.contains("secrets_file"), "{err}");
            assert!(err.contains(&format!("{value:?}")), "{err}");
        }
    }

    #[test]
    fn secret_path_returns_an_absolute_path_unchanged() {
        assert_eq!(
            secret_path("secrets_file", "/etc/m6/x.pem").unwrap(),
            PathBuf::from("/etc/m6/x.pem")
        );
    }

    /// A relative secrets_file is refused at load, where the old code ignored
    /// an absent file rather than checking its path.
    #[test]
    fn a_relative_secrets_file_is_refused_at_load() {
        let mut cfg = NamedTempFile::new().unwrap();
        writeln!(cfg, "secrets_file = \"secrets.toml\"").unwrap();
        writeln!(cfg, "site_name = \"A\"").unwrap();
        cfg.flush().unwrap();
        let err = load(cfg.path(), Path::new("/tmp")).unwrap_err().to_string();
        assert!(err.contains("secrets_file"), "{err}");
    }
}
