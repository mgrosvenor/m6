/// Config loading: site.toml + system config merging and validation.
use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::{Deserialize, Serialize};
use tracing::warn;

/// Full merged configuration for m6-http.
#[derive(Debug, Clone, Serialize)]
pub struct Config {
    pub site: SiteConfig,
    pub server: ServerConfig,
    pub log: LogConfig,
    pub errors: ErrorsConfig,
    pub auth: Option<AuthConfig>,
    pub backends: Vec<BackendConfig>,
    pub routes: Vec<RouteConfig>,
    pub route_groups: Vec<RouteGroupConfig>,
    /// Directory where site.toml lives.
    pub site_dir: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SiteConfig {
    pub name: String,
    pub domain: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    pub bind: String,
    pub tls_cert: String,
    pub tls_key: String,
    /// Timeout in seconds for a backend call (connect + write + read). Default: 30.
    #[serde(default = "default_backend_timeout_secs")]
    pub backend_timeout_secs: u64,
    /// Optional H2C (HTTP/2 cleartext) listener address.
    /// Intended for use over WireGuard tunnels or trusted private networks.
    #[serde(default)]
    pub h2c_bind: Option<String>,
}

fn default_backend_timeout_secs() -> u64 {
    30
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogConfig {
    #[serde(default = "default_log_level")]
    pub level: String,
    #[serde(default = "default_log_format")]
    pub format: String,
}

fn default_log_level() -> String {
    "info".to_string()
}
fn default_log_format() -> String {
    "json".to_string()
}

impl Default for LogConfig {
    fn default() -> Self {
        LogConfig { level: default_log_level(), format: default_log_format() }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorsConfig {
    #[serde(default = "default_errors_mode")]
    pub mode: String,
    pub path: Option<String>,
    /// When true, internal fallback HTML includes descriptive detail and hints.
    /// Recommended for dev; leave false in production.
    #[serde(default)]
    pub verbose_fallback: bool,
}

fn default_errors_mode() -> String {
    "internal".to_string()
}

impl Default for ErrorsConfig {
    fn default() -> Self {
        ErrorsConfig { mode: default_errors_mode(), path: None, verbose_fallback: false }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthConfig {
    pub backend: String,
    pub public_key: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackendConfig {
    pub name: String,
    /// Unix socket glob, e.g. "/run/m6/m6-html-*.sock"
    pub sockets: Option<String>,
    /// URL upstream, e.g. "https://api.example.com"
    pub url: Option<String>,
    /// Skip TLS certificate verification for URL backends.
    /// For testing with self-signed certs only — do not use in production.
    #[serde(default)]
    pub tls_skip_verify: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteConfig {
    pub path: String,
    pub backend: String,
    /// Auth requirement e.g. "group:editors" or "role:admin"
    pub require: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteGroupConfig {
    /// Glob pattern relative to site dir
    pub glob: String,
    /// URL path template with {stem} placeholder
    pub path: String,
    pub backend: String,
    pub require: Option<String>,
}

// ── Raw serde types for parsing site.toml ──────────────────────────────────

#[derive(Debug, Deserialize)]
struct RawSiteToml {
    site: Option<RawSiteSection>,
    server: Option<RawServerSection>,
    log: Option<LogConfig>,
    errors: Option<ErrorsConfig>,
    auth: Option<AuthConfig>,
    #[serde(rename = "backend", default)]
    backends: Vec<BackendConfig>,
    #[serde(rename = "route", default)]
    routes: Vec<RouteConfig>,
    #[serde(rename = "route_group", default)]
    route_groups: Vec<RouteGroupConfig>,
}

#[derive(Debug, Deserialize)]
struct RawSiteSection {
    name: Option<String>,
    domain: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawServerSection {
    bind: Option<String>,
    tls_cert: Option<String>,
    tls_key: Option<String>,
    backend_timeout_secs: Option<u64>,
    h2c_bind: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawSystemToml {
    server: Option<RawServerSection>,
}

// ── Loading ─────────────────────────────────────────────────────────────────

/// Load and merge configs. Returns Config on success. Returns error with
/// message suitable for exit-2 on any validation failure.
pub fn load(site_dir: &Path, system_config_path: &Path) -> anyhow::Result<Config> {
    let site_toml_path = site_dir.join("site.toml");

    let site_raw = std::fs::read_to_string(&site_toml_path)
        .with_context(|| format!("reading {}", site_toml_path.display()))?;
    let site_parsed: RawSiteToml = toml::from_str(&site_raw)
        .with_context(|| format!("parsing {}", site_toml_path.display()))?;

    let system_raw = std::fs::read_to_string(system_config_path)
        .with_context(|| format!("reading {}", system_config_path.display()))?;
    let system_parsed: RawSystemToml = toml::from_str(&system_raw)
        .with_context(|| format!("parsing {}", system_config_path.display()))?;

    // Warn on unexpected sections in system config (we only check for unknown
    // top-level keys by seeing if extra keys exist; toml parsing uses deny_unknown_fields
    // is opt-in, but the spec says warn and ignore instead of error).
    // We rely on RawSystemToml only accepting `server`.

    // Build merged server config:
    // site.toml [server] provides base values, system config [server] overrides.
    let site_server = site_parsed.server.unwrap_or(RawServerSection {
        bind: None,
        tls_cert: None,
        tls_key: None,
        backend_timeout_secs: None,
        h2c_bind: None,
    });
    let sys_server = system_parsed.server.unwrap_or(RawServerSection {
        bind: None,
        tls_cert: None,
        tls_key: None,
        backend_timeout_secs: None,
        h2c_bind: None,
    });

    let bind = sys_server.bind.or(site_server.bind)
        .ok_or_else(|| anyhow::anyhow!("config error: [server].bind is required"))?;
    let tls_cert = sys_server.tls_cert.or(site_server.tls_cert)
        .ok_or_else(|| anyhow::anyhow!("config error: [server].tls_cert is required"))?;
    let tls_key = sys_server.tls_key.or(site_server.tls_key)
        .ok_or_else(|| anyhow::anyhow!("config error: [server].tls_key is required"))?;
    let backend_timeout_secs = sys_server.backend_timeout_secs
        .or(site_server.backend_timeout_secs)
        .unwrap_or(30);
    let h2c_bind = sys_server.h2c_bind.or(site_server.h2c_bind);

    // Resolve TLS cert/key paths relative to site_dir if not absolute.
    let tls_cert_path = resolve_path(site_dir, &tls_cert);
    let tls_key_path = resolve_path(site_dir, &tls_key);

    // Validate [site] required keys
    let raw_site = site_parsed.site.unwrap_or(RawSiteSection { name: None, domain: None });
    let site_name = raw_site.name
        .ok_or_else(|| anyhow::anyhow!("config error: [site].name is required"))?;
    let site_domain = raw_site.domain
        .ok_or_else(|| anyhow::anyhow!("config error: [site].domain is required"))?;

    let server = ServerConfig {
        bind,
        tls_cert: tls_cert_path.to_string_lossy().into_owned(),
        tls_key: tls_key_path.to_string_lossy().into_owned(),
        backend_timeout_secs,
        h2c_bind,
    };
    let log = site_parsed.log.unwrap_or_default();
    let errors = site_parsed.errors.unwrap_or_default();

    // Validate TLS files exist
    if !tls_cert_path.exists() {
        anyhow::bail!("config error: tls_cert file not found: {}", tls_cert_path.display());
    }
    if !tls_key_path.exists() {
        anyhow::bail!("config error: tls_key file not found: {}", tls_key_path.display());
    }

    // Validate backends
    let backends = site_parsed.backends;
    let backend_names: std::collections::HashSet<&str> =
        backends.iter().map(|b| b.name.as_str()).collect();

    // Validate routes
    let routes = site_parsed.routes;
    let mut seen_paths = std::collections::HashSet::new();
    for route in &routes {
        if route.path.is_empty() {
            anyhow::bail!("config error: route missing `path`");
        }
        if route.backend.is_empty() {
            anyhow::bail!("config error: route at {} missing `backend`", route.path);
        }
        if !backend_names.contains(route.backend.as_str()) {
            anyhow::bail!(
                "config error: route {} references unknown backend `{}`",
                route.path, route.backend
            );
        }
        if !seen_paths.insert(route.path.clone()) {
            anyhow::bail!("config error: duplicate route path `{}`", route.path);
        }
    }

    // Validate route_groups
    let route_groups = site_parsed.route_groups;
    for rg in &route_groups {
        if !backend_names.contains(rg.backend.as_str()) {
            anyhow::bail!(
                "config error: route_group {} references unknown backend `{}`",
                rg.path, rg.backend
            );
        }
    }

    // Validate auth
    let auth = site_parsed.auth;
    if let Some(ref a) = auth {
        let key_path = resolve_path(site_dir, &a.public_key);
        if !key_path.exists() {
            anyhow::bail!(
                "config error: [auth].public_key file not found: {}",
                key_path.display()
            );
        }
        if !backend_names.contains(a.backend.as_str()) {
            anyhow::bail!(
                "config error: [auth].backend `{}` not in [[backend]]",
                a.backend
            );
        }
    }

    // Validate require on routes needs [auth]
    for route in &routes {
        if route.require.is_some() && auth.is_none() {
            anyhow::bail!(
                "config error: route {} has `require` but no [auth] declared",
                route.path
            );
        }
    }
    for rg in &route_groups {
        if rg.require.is_some() && auth.is_none() {
            anyhow::bail!(
                "config error: route_group {} has `require` but no [auth] declared",
                rg.path
            );
        }
    }

    // Validate errors
    if errors.mode == "custom" && errors.path.is_none() {
        anyhow::bail!("config error: [errors] mode = \"custom\" requires `path`");
    }

    Ok(Config {
        site: SiteConfig { name: site_name, domain: site_domain },
        server,
        log,
        errors,
        auth,
        backends,
        routes,
        route_groups,
        site_dir: site_dir.to_path_buf(),
    })
}

/// Resolve a path: if absolute, return as-is; otherwise relative to base.
pub fn resolve_path(base: &Path, p: &str) -> PathBuf {
    let pb = Path::new(p);
    if pb.is_absolute() {
        pb.to_path_buf()
    } else {
        base.join(pb)
    }
}

/// Warn about any extra top-level keys in system config toml.
/// We do this via raw parsing.
pub fn warn_system_config_extra_keys(system_config_path: &Path) {
    if let Ok(raw) = std::fs::read_to_string(system_config_path) {
        if let Ok(val) = raw.parse::<toml::Value>() {
            if let toml::Value::Table(tbl) = val {
                for key in tbl.keys() {
                    if key != "server" {
                        warn!(
                            key = %key,
                            file = %system_config_path.display(),
                            "system config: ignoring non-[server] key"
                        );
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    fn make_test_dir() -> TempDir {
        tempfile::tempdir().unwrap()
    }

    fn write_file(dir: &Path, name: &str, content: &str) {
        std::fs::write(dir.join(name), content).unwrap();
    }

    fn minimal_site_toml() -> String {
        r#"
[site]
name   = "Test"
domain = "test.example.com"

[server]
bind     = "127.0.0.1:8443"
tls_cert = "cert.pem"
tls_key  = "key.pem"

[[backend]]
name    = "m6-html"
sockets = "/run/m6/m6-html-*.sock"

[[route]]
path    = "/"
backend = "m6-html"
"#.to_string()
    }

    fn minimal_system_toml() -> String {
        r#"
[server]
bind     = "127.0.0.1:8443"
tls_cert = "cert.pem"
tls_key  = "key.pem"
"#.to_string()
    }

    fn setup_minimal(dir: &Path) {
        write_file(dir, "site.toml", &minimal_site_toml());
        write_file(dir, "system.toml", &minimal_system_toml());
        // Create dummy TLS files
        write_file(dir, "cert.pem", "dummy");
        write_file(dir, "key.pem", "dummy");
    }

    #[test]
    fn test_valid_minimal_config() {
        let dir = make_test_dir();
        setup_minimal(dir.path());
        let cfg = load(dir.path(), &dir.path().join("system.toml")).unwrap();
        assert_eq!(cfg.site.name, "Test");
        assert_eq!(cfg.server.bind, "127.0.0.1:8443");
        assert_eq!(cfg.routes.len(), 1);
    }

    #[test]
    fn test_missing_site_name_fails() {
        let dir = make_test_dir();
        write_file(dir.path(), "cert.pem", "dummy");
        write_file(dir.path(), "key.pem", "dummy");
        write_file(dir.path(), "site.toml", r#"
[site]
domain = "test.example.com"
[server]
bind = "0.0.0.0:443"
tls_cert = "cert.pem"
tls_key  = "key.pem"
[[backend]]
name = "b"
sockets = "/run/m6/*.sock"
"#);
        write_file(dir.path(), "system.toml", "[server]\nbind = \"0.0.0.0:443\"\ntls_cert = \"cert.pem\"\ntls_key = \"key.pem\"\n");
        let result = load(dir.path(), &dir.path().join("system.toml"));
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("name"));
    }

    #[test]
    fn test_missing_tls_cert_fails() {
        let dir = make_test_dir();
        write_file(dir.path(), "key.pem", "dummy");
        write_file(dir.path(), "site.toml", r#"
[site]
name   = "Test"
domain = "test.example.com"
[server]
bind     = "0.0.0.0:443"
tls_cert = "missing-cert.pem"
tls_key  = "key.pem"
"#);
        write_file(dir.path(), "system.toml", "");
        let result = load(dir.path(), &dir.path().join("system.toml"));
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("tls_cert"));
    }

    #[test]
    fn test_unknown_backend_fails() {
        let dir = make_test_dir();
        write_file(dir.path(), "cert.pem", "dummy");
        write_file(dir.path(), "key.pem", "dummy");
        write_file(dir.path(), "site.toml", r#"
[site]
name   = "Test"
domain = "test.example.com"
[server]
bind     = "0.0.0.0:443"
tls_cert = "cert.pem"
tls_key  = "key.pem"
[[backend]]
name = "b"
sockets = "/run/m6/*.sock"
[[route]]
path    = "/"
backend = "unknown"
"#);
        write_file(dir.path(), "system.toml", "");
        let result = load(dir.path(), &dir.path().join("system.toml"));
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("unknown backend"));
    }

    #[test]
    fn test_duplicate_route_fails() {
        let dir = make_test_dir();
        write_file(dir.path(), "cert.pem", "dummy");
        write_file(dir.path(), "key.pem", "dummy");
        write_file(dir.path(), "site.toml", r#"
[site]
name   = "Test"
domain = "test.example.com"
[server]
bind     = "0.0.0.0:443"
tls_cert = "cert.pem"
tls_key  = "key.pem"
[[backend]]
name = "b"
sockets = "/run/m6/*.sock"
[[route]]
path    = "/"
backend = "b"
[[route]]
path    = "/"
backend = "b"
"#);
        write_file(dir.path(), "system.toml", "");
        let result = load(dir.path(), &dir.path().join("system.toml"));
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("duplicate"));
    }

    #[test]
    fn test_require_without_auth_fails() {
        let dir = make_test_dir();
        write_file(dir.path(), "cert.pem", "dummy");
        write_file(dir.path(), "key.pem", "dummy");
        write_file(dir.path(), "site.toml", r#"
[site]
name   = "Test"
domain = "test.example.com"
[server]
bind     = "0.0.0.0:443"
tls_cert = "cert.pem"
tls_key  = "key.pem"
[[backend]]
name = "b"
sockets = "/run/m6/*.sock"
[[route]]
path    = "/"
backend = "b"
require = "group:editors"
"#);
        write_file(dir.path(), "system.toml", "");
        let result = load(dir.path(), &dir.path().join("system.toml"));
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("require"));
    }

    #[test]
    fn test_custom_errors_without_path_fails() {
        let dir = make_test_dir();
        write_file(dir.path(), "cert.pem", "dummy");
        write_file(dir.path(), "key.pem", "dummy");
        write_file(dir.path(), "site.toml", r#"
[site]
name   = "Test"
domain = "test.example.com"
[server]
bind     = "0.0.0.0:443"
tls_cert = "cert.pem"
tls_key  = "key.pem"
[errors]
mode = "custom"
[[backend]]
name = "b"
sockets = "/run/m6/*.sock"
"#);
        write_file(dir.path(), "system.toml", "");
        let result = load(dir.path(), &dir.path().join("system.toml"));
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("custom"));
    }

    #[test]
    fn test_system_config_server_wins() {
        let dir = make_test_dir();
        write_file(dir.path(), "cert.pem", "dummy");
        write_file(dir.path(), "key.pem", "dummy");
        write_file(dir.path(), "site.toml", r#"
[site]
name   = "Test"
domain = "test.example.com"
[server]
bind     = "127.0.0.1:8443"
tls_cert = "cert.pem"
tls_key  = "key.pem"
[[backend]]
name = "b"
sockets = "/run/m6/*.sock"
"#);
        write_file(dir.path(), "system.toml", r#"
[server]
bind = "0.0.0.0:443"
tls_cert = "cert.pem"
tls_key = "key.pem"
"#);
        let cfg = load(dir.path(), &dir.path().join("system.toml")).unwrap();
        assert_eq!(cfg.server.bind, "0.0.0.0:443");
    }

    #[test]
    fn test_backend_timeout_default() {
        let dir = make_test_dir();
        setup_minimal(dir.path());
        let cfg = load(dir.path(), &dir.path().join("system.toml")).unwrap();
        assert_eq!(cfg.server.backend_timeout_secs, 30);
    }

    #[test]
    fn test_backend_timeout_from_site_toml() {
        let dir = make_test_dir();
        write_file(dir.path(), "cert.pem", "dummy");
        write_file(dir.path(), "key.pem", "dummy");
        write_file(dir.path(), "site.toml", r#"
[site]
name   = "Test"
domain = "test.example.com"
[server]
bind     = "127.0.0.1:8443"
tls_cert = "cert.pem"
tls_key  = "key.pem"
backend_timeout_secs = 60
[[backend]]
name = "b"
sockets = "/run/m6/*.sock"
[[route]]
path    = "/"
backend = "b"
"#);
        write_file(dir.path(), "system.toml", "[server]\nbind = \"127.0.0.1:8443\"\ntls_cert = \"cert.pem\"\ntls_key = \"key.pem\"\n");
        let cfg = load(dir.path(), &dir.path().join("system.toml")).unwrap();
        assert_eq!(cfg.server.backend_timeout_secs, 60);
    }

    #[test]
    fn test_backend_timeout_system_wins() {
        let dir = make_test_dir();
        write_file(dir.path(), "cert.pem", "dummy");
        write_file(dir.path(), "key.pem", "dummy");
        write_file(dir.path(), "site.toml", r#"
[site]
name   = "Test"
domain = "test.example.com"
[server]
bind     = "127.0.0.1:8443"
tls_cert = "cert.pem"
tls_key  = "key.pem"
backend_timeout_secs = 10
[[backend]]
name = "b"
sockets = "/run/m6/*.sock"
"#);
        write_file(dir.path(), "system.toml", r#"
[server]
bind = "127.0.0.1:8443"
tls_cert = "cert.pem"
tls_key = "key.pem"
backend_timeout_secs = 120
"#);
        let cfg = load(dir.path(), &dir.path().join("system.toml")).unwrap();
        assert_eq!(cfg.server.backend_timeout_secs, 120);
    }
}
