use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

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

/// Derive the socket path from the config path.
/// e.g. `configs/m6-file.conf` → `/run/m6/m6-file.sock`
pub fn socket_path_from_config(config_path: &Path) -> PathBuf {
    let stem = config_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("m6-file");
    PathBuf::from(format!("/run/m6/{}.sock", stem))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_socket_path_derivation() {
        let p = Path::new("configs/m6-file.conf");
        assert_eq!(socket_path_from_config(p), PathBuf::from("/run/m6/m6-file.sock"));
    }
}
