use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct RawConfig {
    storage: StorageConfig,
    tokens: Option<TokensConfig>,
    keys: KeysConfig,
    rate_limit: Option<RateLimitConfig>,
}

#[derive(Debug, Deserialize)]
struct StorageConfig {
    path: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TokensConfig {
    access_ttl: Option<u64>,
    refresh_ttl: Option<u64>,
    issuer: Option<String>,
    audience: Option<String>,
}

#[derive(Debug, Deserialize)]
struct KeysConfig {
    /// Required only by a process that serves the login routes, which sign.
    /// The check side reads the public key and never this one. A path, typed
    /// as one, because the key's bytes are never in the config.
    private_key: Option<PathBuf>,
    public_key: String,
}

/// `[rate_limit]` — the login throttle, in the config rather than hard-coded.
///
/// It was two `const`s in `rate_limit.rs`, which meant a deployment that needed
/// a different budget had to be recompiled, and a test suite that logs in
/// repeatedly had no way to ask for room. See `rate_limit::RateLimiter`.
#[derive(Debug, Deserialize)]
struct RateLimitConfig {
    max_attempts: Option<u32>,
    window_secs: Option<u64>,
}

/// Parsed and validated auth server configuration.
/// `[log]` is deliberately absent: `m6_core::site` resolves it for every
/// service, from the same file, with the same precedence (site.toml, then the
/// service config's `[log]`, then `--log-level`). Two parsers for one section
/// is how they drift.
pub struct AuthConfig {
    /// Absolute: see m6_core::config::secret_path.
    pub db_path: PathBuf,
    pub access_ttl: u64,  // seconds
    pub refresh_ttl: u64, // seconds
    pub issuer: String,
    /// The deployment the tokens are minted for, and the `aud` every token
    /// carries. The issuer when the config names no audience.
    pub audience: String,
    /// Absolute, as db_path is. `None` when the config names no private key,
    /// which only the check side may do.
    pub private_key_path: Option<PathBuf>,
    pub public_key_path: PathBuf,
    /// Failed logins allowed per IP per `rate_limit_window_secs`.
    pub rate_limit_max_attempts: u32,
    pub rate_limit_window_secs: u64,
}

impl AuthConfig {
    /// Every path in the file is absolute.
    pub fn load(config_path: &Path, site_toml: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(config_path)
            .with_context(|| format!("reading config: {}", config_path.display()))?;

        let raw: RawConfig = toml::from_str(&text)
            .with_context(|| format!("parsing TOML: {}", config_path.display()))?;

        let db_path_str = raw
            .storage
            .path
            .ok_or_else(|| anyhow!("[storage] path is required"))?;
        if db_path_str.is_empty() {
            return Err(anyhow!("[storage] path must not be empty"));
        }
        let db_path = m6_core::config::secret_path("[storage] path", &db_path_str)?;

        let tokens = raw.tokens.unwrap_or(TokensConfig {
            access_ttl: None,
            refresh_ttl: None,
            issuer: None,
            audience: None,
        });

        let access_ttl = tokens.access_ttl.unwrap_or(900);
        let refresh_ttl = tokens.refresh_ttl.unwrap_or(2_592_000);

        // The issuer is the deployment's name, taken from `[tokens] issuer`
        // and then from `[site] domain` in the site's `site.toml`. A config
        // that names neither stops the start: a token minted for one deployment
        // must not verify at another, and a default would name every deployment
        // the same thing.
        let issuer = match tokens.issuer {
            Some(issuer) => issuer,
            None => try_issuer_from_site_toml(site_toml).ok_or_else(|| {
                anyhow!(
                    "[tokens] issuer: no issuer is set and {} has no [site] domain: set [tokens] issuer to the deployment's name, or set [site] domain in that file",
                    site_toml.display()
                )
            })?,
        };
        // The audience is the deployment's own name, and the issuer is that name
        // when the config names no separate one.
        let audience = tokens.audience.unwrap_or_else(|| issuer.clone());

        let private_key_path = raw
            .keys
            .private_key
            .as_deref()
            .map(|path| m6_core::config::secret_path("[keys] private_key", &path.to_string_lossy()))
            .transpose()?;
        let public_key_path =
            m6_core::config::secret_path("[keys] public_key", &raw.keys.public_key)?;

        let rl = raw.rate_limit.unwrap_or(RateLimitConfig {
            max_attempts: None,
            window_secs: None,
        });
        let rate_limit_max_attempts = rl
            .max_attempts
            .unwrap_or(crate::rate_limit::DEFAULT_MAX_ATTEMPTS);
        let rate_limit_window_secs = rl
            .window_secs
            .unwrap_or(crate::rate_limit::DEFAULT_WINDOW_SECS);

        Ok(AuthConfig {
            db_path,
            access_ttl,
            refresh_ttl,
            issuer,
            audience,
            private_key_path,
            public_key_path,
            rate_limit_max_attempts,
            rate_limit_window_secs,
        })
    }
}

fn try_issuer_from_site_toml(site_toml: &Path) -> Option<String> {
    let text = std::fs::read_to_string(site_toml).ok()?;
    let val: toml::Value = text.parse().ok()?;
    val.get("site")?
        .get("domain")?
        .as_str()
        .map(|s| s.to_string())
}
