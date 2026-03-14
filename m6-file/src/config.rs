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

#[derive(Debug, Clone, Deserialize)]
pub struct RouteConfig {
    pub path: String,
    pub root: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ThreadPoolConfig {
    pub size: Option<usize>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct Config {
    #[serde(default)]
    pub compression: HashMap<String, CompressionSettings>,
    #[serde(default)]
    pub route: Vec<RouteConfig>,
    pub thread_pool: Option<ThreadPoolConfig>,
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
