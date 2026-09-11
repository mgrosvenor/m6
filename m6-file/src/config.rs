use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;

#[derive(Debug, Clone, Deserialize)]
pub struct CompressionSettings {
    #[serde(default)]
    pub brotli: u32,
    #[serde(default)]
    pub gzip: u32,
}

/// Per-MIME-type minification enable flags, mirroring `m6-render`'s
/// `MinificationConfig` so both backends honour the same `[minification]`
/// TOML shape.
#[derive(Debug, Clone, Deserialize)]
pub struct MinificationConfig {
    #[serde(flatten, default = "default_minification_enabled")]
    pub enabled: HashMap<String, bool>,
    /// Whether to minify inline `<script>` blocks inside HTML files.
    /// Off by default — see `m6-render`'s `MinificationConfig` for why.
    #[serde(default)]
    pub inline_js: bool,
}

impl Default for MinificationConfig {
    fn default() -> Self {
        MinificationConfig { enabled: default_minification_enabled(), inline_js: false }
    }
}

impl MinificationConfig {
    /// Returns true if minification is enabled for `mime`.
    pub fn is_enabled(&self, mime: &str) -> bool {
        let base = mime.split(';').next().unwrap_or(mime).trim();
        *self.enabled.get(base).unwrap_or(&false)
    }
}

fn default_minification_enabled() -> HashMap<String, bool> {
    let mut m = HashMap::new();
    m.insert("text/html".to_string(), true);
    m.insert("text/css".to_string(), true);
    m.insert("application/json".to_string(), true);
    m.insert("application/javascript".to_string(), true);
    m.insert("text/javascript".to_string(), true);
    m
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct RouteConfig {
    pub path: String,
    pub root: String,
    /// If true, serves the file from an optional `?offset=N` byte offset and
    /// returns `Cache-Control: no-store`. Intended for log tailing.
    pub tail: Option<bool>,
    /// Extra response headers as `[[key, value]]` pairs.
    #[serde(default)]
    pub headers: Vec<[String; 2]>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ThreadPoolConfig {
    pub size: Option<usize>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct LogConfig {
    pub level: Option<String>,
    pub format: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct Config {
    #[serde(default)]
    pub compression: HashMap<String, CompressionSettings>,
    #[serde(default)]
    pub minification: MinificationConfig,
    #[serde(default)]
    pub route: Vec<RouteConfig>,
    pub thread_pool: Option<ThreadPoolConfig>,
    pub log: Option<LogConfig>,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("reading config file: {}", path.display()))?;
        let config: Config = toml::from_str(&content)
            .with_context(|| format!("parsing config file: {}", path.display()))?;
        Ok(config)
    }
}

// `socket_path_from_config` used to live here too, a fourth copy of a rule
// m6-core already owned, and m6-file called this one rather than core's. It
// differed only in its fallback stem (`m6-file` against core's `m6-default`),
// which is the kind of difference that is harmless until the day it is not.
// It is now `m6_core::server::socket_path_from_config`, which is also where
// the `M6_SOCKET_OVERRIDE` escape hatch lives, so this service no longer
// carries its own copy of that either.
