/// Config loading: TOML → serde_json::Map.
///
/// Load the renderer TOML config, optionally merge a secrets file,
/// and return the merged map plus the parsed route/framework settings.
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
}

/// Thread-pool configuration parsed from `[thread_pool]`.
#[derive(Debug, Clone)]
pub struct ThreadPoolConfig {
    pub size: usize,
    pub queue_size: usize,
}

/// LRU params-cache configuration.
#[derive(Debug, Clone)]
pub struct ParamsCacheConfig {
    pub size: usize,
}

/// Compression level for a MIME type.
#[derive(Debug, Clone)]
pub struct CompressionLevel {
    pub brotli: u32,
    pub gzip: u32,
}

/// Fully parsed renderer configuration.
#[derive(Debug, Clone)]
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
    /// Compression settings keyed by MIME type.
    pub compression: std::collections::HashMap<String, CompressionLevel>,
    /// Minification settings keyed by MIME type.
    pub minification: MinificationConfig,
}

/// Per-MIME-type minification enable flag.
#[derive(Debug, Clone)]
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
    // JS: on by default — parse-js engine handles modern ES syntax.
    // Falls back to original bytes on parse failure.
    m.insert("application/javascript".to_string(), true);
    m.insert("text/javascript".to_string(), true);
    MinificationConfig { enabled: m }
}

/// Framework-consumed top-level keys that must not appear in the request dict.
const FRAMEWORK_KEYS: &[&str] = &[
    "global_params",
    "route",
    "secrets_file",
    "thread_pool",
    "params_cache",
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
            merge_toml(&mut toml_val, secrets);
        }
        // Silently ignore if absent.
    }

    parse_config(toml_val, site_dir)
}

/// Deep-merge `src` into `dst`; `src` wins on conflict.
fn merge_toml(dst: &mut toml::Value, src: toml::Value) {
    match (dst, src) {
        (toml::Value::Table(d), toml::Value::Table(s)) => {
            for (k, v) in s {
                let entry = d.entry(k).or_insert(toml::Value::Table(toml::map::Map::new()));
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
    let cpus = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);

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
        for (mime, val) in tbl {
            let enabled = val.as_bool().unwrap_or(false);
            minification.enabled.insert(mime.clone(), enabled);
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

    Ok(RendererConfig {
        user_config,
        global_params,
        routes,
        thread_pool: ThreadPoolConfig { size: tp_size, queue_size: tp_queue },
        params_cache: ParamsCacheConfig { size: pc_size },
        compression,
        minification,
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

        let template = item.get("template").and_then(|v| v.as_str()).map(|s| s.to_string());

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

        let cache = item
            .get("cache")
            .and_then(|v| v.as_str())
            .unwrap_or("public")
            .to_string();

        let methods: Option<Vec<String>> = item.get("methods").and_then(|v| v.as_array()).map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_uppercase()))
                .collect()
        });

        routes.push(RouteConfig { path, template, params, status, cache, methods });
    }

    Ok(routes)
}

/// Convert a `toml::Value` to `serde_json::Value`.
pub fn toml_to_json(v: &toml::Value) -> Value {
    match v {
        toml::Value::String(s) => Value::String(s.clone()),
        toml::Value::Integer(i) => Value::Number((*i).into()),
        toml::Value::Float(f) => {
            serde_json::Number::from_f64(*f)
                .map(Value::Number)
                .unwrap_or(Value::Null)
        }
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

/// Derive socket path from config filename.
/// `configs/m6-html.conf` → `/run/m6/m6-html.sock`
pub fn socket_path_from_config(config_path: &Path) -> PathBuf {
    let stem = config_path
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy();
    PathBuf::from(format!("/run/m6/{}.sock", stem))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn test_socket_path() {
        let p = Path::new("configs/m6-html.conf");
        assert_eq!(socket_path_from_config(p), PathBuf::from("/run/m6/m6-html.sock"));
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

    #[test]
    fn test_secrets_override() {
        let mut secrets = NamedTempFile::new().unwrap();
        write!(secrets, "password = \"secret\"\n").unwrap();

        let mut cfg_file = NamedTempFile::new().unwrap();
        write!(
            cfg_file,
            "password = \"dev\"\nsecrets_file = {:?}\n",
            secrets.path()
        )
        .unwrap();

        let cfg = load(cfg_file.path(), Path::new("/tmp")).unwrap();
        assert_eq!(cfg.user_config.get("password").unwrap().as_str().unwrap(), "secret");
    }

    #[test]
    fn test_secrets_absent_ignored() {
        let mut cfg_file = NamedTempFile::new().unwrap();
        write!(cfg_file, "secrets_file = \"/nonexistent/path/file.toml\"\n").unwrap();
        // Should not error
        load(cfg_file.path(), Path::new("/tmp")).unwrap();
    }

    #[test]
    fn test_secrets_malformed_errors() {
        let mut secrets = NamedTempFile::new().unwrap();
        write!(secrets, "not valid toml {{{{").unwrap();

        let mut cfg_file = NamedTempFile::new().unwrap();
        write!(cfg_file, "secrets_file = {:?}\n", secrets.path()).unwrap();

        assert!(load(cfg_file.path(), Path::new("/tmp")).is_err());
    }
}
