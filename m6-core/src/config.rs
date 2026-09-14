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

/// A single route entry from `[[route]]` in the config file.
#[derive(Debug, Clone)]
pub struct RouteConfig {
    pub path: String,
    pub template: Option<String>,
    pub params: Vec<String>,
    pub status: u16,
    /// "public" | "no-store"
    pub cache: String,
    pub methods: Option<Vec<String>>,
    /// Extra response headers as `[[key, value]]` pairs.
    pub headers: Vec<(String, String)>,
    /// Name of a handler registered in code with `App::handler`.
    ///
    /// This is what makes a route dynamic. A handler is code and cannot appear
    /// while the process runs; a *route* is config and must be able to. Naming
    /// the binding in config puts the half that changes on the side that
    /// reloads, so adding an asset tree is a config edit rather than a
    /// restart.
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

/// Thread-pool configuration parsed from `[thread_pool]`.
#[derive(Debug, Clone, Default)]
pub struct ThreadPoolConfig {
    pub size: usize,
    pub queue_size: usize,
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
    /// `None` means no timeout, which is what every `App` service did before
    /// this key existed: a peer that connects and then says nothing holds a
    /// pool worker until it disconnects, and a handful of such peers take the
    /// whole pool. Configure `read_timeout_s = 0` to ask for that old
    /// behaviour back deliberately.
    pub read_timeout: Option<std::time::Duration>,
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

/// Log configuration from `[log]` in the renderer config file.
#[derive(Debug, Clone, Default)]
pub struct LogConfig {
    pub level: Option<String>,
    pub format: Option<String>,
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
    /// Thread-pool settings.
    pub thread_pool: ThreadPoolConfig,
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
}

/// Per-MIME-type minification enable flag.
#[derive(Debug, Clone, Default)]
pub struct MinificationConfig {
    /// Which MIME types have minification enabled.
    pub enabled: std::collections::HashMap<String, bool>,
    /// Whether to minify inline `<script>` blocks inside HTML responses.
    ///
    /// Defaults to `false`. The `minify-html` JS engine parses scripts as
    /// ES modules (`TopLevelMode::Module`), which has different scoping
    /// semantics from classic scripts and can silently corrupt valid inline
    /// JS (e.g. rewriting string `'\n'` as template literal `'\\n'`,
    /// making `var`-declared functions inaccessible from `onclick` attributes,
    /// etc.). Enable only if all inline scripts in your templates are written
    /// as self-contained ES modules.
    ///
    /// Set in config with `[minification] inline_js = true`.
    pub inline_js: bool,
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
    // JS files: on by default — parse-js engine handles modern ES syntax
    // and falls back to original bytes on parse failure.
    m.insert("application/javascript".to_string(), true);
    m.insert("text/javascript".to_string(), true);
    MinificationConfig {
        enabled: m,
        inline_js: false,
    }
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
    "errors",
    "multipart",
    "flash_secret",
];

/// Load and merge config + optional secrets file.
///
/// Returns `(RendererConfig, socket_path)`.
pub fn load(config_path: &Path, site_dir: &Path) -> anyhow::Result<RendererConfig> {
    let raw = std::fs::read_to_string(config_path)
        .with_context(|| format!("reading config {}", config_path.display()))?;

    let mut toml_val: toml::Value = toml::from_str(&raw)
        .with_context(|| format!("parsing config {}", config_path.display()))?;

    // Optionally merge secrets file.
    if let Some(secrets_path) = toml_val.get("secrets_file").and_then(|v| v.as_str()) {
        let sp = PathBuf::from(secrets_path);
        if sp.exists() {
            let srw = std::fs::read_to_string(&sp)
                .with_context(|| format!("reading secrets file {}", sp.display()))?;
            let secrets: toml::Value = toml::from_str(&srw)
                .with_context(|| format!("parsing secrets file {}", sp.display()))?;

            // ONE KEY, ONE OWNER. A key set in both files is refused here.
            //
            // This used to be a plain merge with "src wins", which meant a config
            // file could state a value that was not the one in use, silently, with
            // nothing logged and neither file mentioning the other.
            //
            // It cost a wrong conclusion about a live system. The origin's
            // render-contact.conf read `from = "noreply@example.com"`,
            // `to = "matthew@..."`, `host = "localhost"`, `port = 1025` -- and
            // every one of those was inert, because the secrets file set all four.
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

    parse_config(toml_val, site_dir)
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

fn parse_config(tv: toml::Value, _site_dir: &Path) -> anyhow::Result<RendererConfig> {
    let cpus = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);

    // --- thread_pool ---
    let tp_size = tv
        .get("thread_pool")
        .and_then(|t| t.get("size"))
        .and_then(|v| v.as_integer())
        .map(|v| v as usize)
        .unwrap_or(cpus);
    let tp_queue = tv
        .get("thread_pool")
        .and_then(|t| t.get("queue_size"))
        .and_then(|v| v.as_integer())
        .map(|v| v as usize)
        .unwrap_or(tp_size * 8);

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
    // by hand for the same reason, so it is the value this fleet already runs.
    let read_timeout_s = match tv.get("server").and_then(|t| t.get("read_timeout_s")) {
        None => crate::server::DEFAULT_READ_TIMEOUT_SECS as i64,
        Some(v) => {
            let n = v.as_integer().ok_or_else(|| {
                anyhow::anyhow!("[server] read_timeout_s must be an integer, got {v}")
            })?;
            if n < 0 {
                anyhow::bail!("[server] read_timeout_s must not be negative, got {n}");
            }
            n
        }
    };
    let read_timeout = if read_timeout_s == 0 {
        None
    } else {
        Some(std::time::Duration::from_secs(read_timeout_s as u64))
    };

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
    let mut minification = default_minification();
    if let Some(toml::Value::Table(tbl)) = tv.get("minification") {
        for (key, val) in tbl {
            if key == "inline_js" {
                minification.inline_js = val.as_bool().unwrap_or(false);
            } else {
                minification
                    .enabled
                    .insert(key.clone(), val.as_bool().unwrap_or(false));
            }
        }
    }

    // --- user config (strip framework keys) ---
    let mut user_config = Map::new();
    if let toml::Value::Table(tbl) = &tv {
        for (k, v) in tbl {
            if !FRAMEWORK_KEYS.contains(&k.as_str()) {
                user_config.insert(k.clone(), toml_to_json(v));
            }
        }
    }

    // --- log ---
    let log = LogConfig {
        level: tv
            .get("log")
            .and_then(|l| l.get("level"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        format: tv
            .get("log")
            .and_then(|l| l.get("format"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
    };

    Ok(RendererConfig {
        user_config,
        global_params,
        routes,
        thread_pool: ThreadPoolConfig {
            size: tp_size,
            queue_size: tp_queue,
        },
        params_cache: ParamsCacheConfig { size: pc_size },
        server: ServerConfig {
            read_timeout,
            socket_mode,
        },
        compression,
        minification,
        log,
    })
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
        // `public`, and an explicit `cache` wins over both.
        //
        // The default has to depend on the kind of route because the two are
        // not the same kind of thing: a template renders a document from files
        // on disk, a handler computes an answer. Letting a handler route
        // inherit `public` would put dynamic output in a shared cache by
        // omission, which is the shape of defect that is found by someone else
        // seeing another user's page. Code routes registered with
        // `App::route_get` already defaulted to `no-store` for this reason;
        // this is the same rule reaching the config-declared form.
        let cache = item
            .get("cache")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| {
                if handler.is_some() {
                    "no-store".to_string()
                } else {
                    "public".to_string()
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

[thread_pool]
size = 4
queue_size = 32
"#
        )
        .unwrap();

        let cfg = load(f.path(), Path::new("/tmp")).unwrap();
        assert_eq!(cfg.global_params, vec!["data/site.json"]);
        assert_eq!(cfg.routes.len(), 1);
        assert_eq!(cfg.thread_pool.size, 4);
        assert_eq!(cfg.thread_pool.queue_size, 32);
        assert_eq!(cfg.user_config.get("site_name").unwrap(), "Test");
    }

    /// Parse a `[server]` body, or return the refusal message.
    fn server_cfg(body: &str) -> anyhow::Result<ServerConfig> {
        let mut f = NamedTempFile::new().unwrap();
        write!(f, "[server]\n{body}\n").unwrap();
        load(f.path(), Path::new("/tmp")).map(|c| c.server)
    }

    /// The defaults are the contract for every config that says nothing, which
    /// is every config in production today.
    #[test]
    fn server_defaults_are_thirty_seconds_and_0660() {
        let mut f = NamedTempFile::new().unwrap();
        writeln!(f, "site_name = \"Test\"").unwrap();
        let cfg = load(f.path(), Path::new("/tmp")).unwrap();
        assert_eq!(
            cfg.server.read_timeout,
            Some(std::time::Duration::from_secs(30))
        );
        assert_eq!(cfg.server.socket_mode, 0o660);
    }

    #[test]
    fn server_keys_are_read() {
        let s = server_cfg("read_timeout_s = 5\nsocket_mode = \"0600\"").unwrap();
        assert_eq!(s.read_timeout, Some(std::time::Duration::from_secs(5)));
        assert_eq!(s.socket_mode, 0o600);
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
}

/// A key belongs to exactly one file.
///
/// ## The defect these cover
///
/// `load` merged the secrets file over the config with "src wins on conflict",
/// silently. A config file could therefore state a value that was not the one in
/// use, with nothing logged and neither file mentioning the other.
///
/// It cost a wrong conclusion about a live system. The production origin's
/// `render-contact.conf` read `from = "noreply@example.com"`,
/// `to = "matthew@..."`, `host = "localhost"` and `port = 1025`, and all four
/// were inert because the secrets file set them. The form was relaying through a
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
}
