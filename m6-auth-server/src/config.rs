use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct LogConfig {
    pub level:  Option<String>,
    pub format: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawConfig {
    storage: StorageConfig,
    tokens:  Option<TokensConfig>,
    keys:    KeysConfig,
    log:     Option<LogConfig>,
}

#[derive(Debug, Deserialize)]
struct StorageConfig {
    path: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TokensConfig {
    access_ttl:  Option<u64>,
    refresh_ttl: Option<u64>,
    issuer:      Option<String>,
}

#[derive(Debug, Deserialize)]
struct KeysConfig {
    private_key: String,
    public_key:  String,
}

/// Parsed and validated auth server configuration.
pub struct AuthConfig {
    pub db_path:         PathBuf,   // relative to site_dir
    pub access_ttl:      u64,       // seconds
    pub refresh_ttl:     u64,       // seconds
    pub issuer:          String,
    pub private_key_path: PathBuf,
    pub public_key_path:  PathBuf,
    pub log:             Option<LogConfig>,
}

impl AuthConfig {
    pub fn load(site_dir: &Path, config_path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(config_path)
            .with_context(|| format!("reading config: {}", config_path.display()))?;

        let raw: RawConfig = toml::from_str(&text)
            .with_context(|| format!("parsing TOML: {}", config_path.display()))?;

        // Validate db path — resolve relative to the config file's directory
        // (matching m6-auth-cli behaviour).
        let db_path_str = raw.storage.path
            .ok_or_else(|| anyhow!("[storage] path is required"))?;
        if db_path_str.is_empty() {
            return Err(anyhow!("[storage] path must not be empty"));
        }
        let db_path = {
            let p = PathBuf::from(&db_path_str);
            if p.is_absolute() {
                p
            } else {
                let config_dir = config_path.parent().unwrap_or(Path::new("."));
                config_dir.join(&p)
            }
        };

        let tokens = raw.tokens.unwrap_or(TokensConfig {
            access_ttl:  None,
            refresh_ttl: None,
            issuer:      None,
        });

        let access_ttl  = tokens.access_ttl.unwrap_or(900);
        let refresh_ttl = tokens.refresh_ttl.unwrap_or(2_592_000);

        // Issuer: from config, or try to derive from site.toml
        let issuer = tokens.issuer
            .unwrap_or_else(|| {
                // Try reading site.toml from the site_dir
                try_issuer_from_site_toml(site_dir).unwrap_or_else(|| "localhost".to_string())
            });

        let private_key_path = site_dir.join(&raw.keys.private_key);
        let public_key_path  = site_dir.join(&raw.keys.public_key);

        Ok(AuthConfig {
            db_path,
            access_ttl,
            refresh_ttl,
            issuer,
            private_key_path,
            public_key_path,
            log: raw.log,
        })
    }
}

fn try_issuer_from_site_toml(site_dir: &Path) -> Option<String> {
    let site_toml = site_dir.join("site.toml");
    let text = std::fs::read_to_string(site_toml).ok()?;
    let val: toml::Value = text.parse().ok()?;
    val.get("site")?.get("domain")?.as_str().map(|s| s.to_string())
}
