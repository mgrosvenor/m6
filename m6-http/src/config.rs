/// Config loading: site.toml + system config merging and validation.
use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::{Deserialize, Serialize};
use tracing::warn;

/// Full merged configuration for m6-http.
#[derive(Debug, Clone, Serialize)]
pub struct Config {
    pub site: SiteConfig,
    pub node: NodeConfig,
    pub server: ServerConfig,
    pub log: LogConfig,
    pub analytics: AnalyticsConfig,
    pub rate_limit: RateLimitConfig,
    pub errors: ErrorsConfig,
    pub security: SecurityConfig,
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

/// This deployment's node identity (e.g. "sydney", "london") — distinct from
/// `[site].name`, which is the site's own display name and is identical
/// across every node (they all serve the same site, from the same
/// byte-for-byte site.toml). Comes from the per-node *system* config
/// (`configs/cache-<city>.toml` / `configs/sydney.toml`) instead.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeConfig {
    pub name: String,
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

/// Per-request traffic logging (session cookie, referrer, UA, cache state,
/// node identity, ...) — separate from operational `[log]` above. Written to
/// its own file via `m6_core::log::init_with_analytics`, always JSON.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnalyticsConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Resolved relative to the process's current working directory (not
    /// `site_dir`, which lives under a generated/rendered tree that gets
    /// wiped on redeploy) unless absolute.
    #[serde(default = "default_analytics_log_path")]
    pub log_path: String,
}

fn default_true() -> bool {
    true
}
fn default_analytics_log_path() -> String {
    "logs/analytics.ndjson".to_string()
}

impl Default for AnalyticsConfig {
    fn default() -> Self {
        AnalyticsConfig { enabled: default_true(), log_path: default_analytics_log_path() }
    }
}

/// Per-IP request throttling at the edge, ahead of cache lookup and backend
/// work. Fixed-window counter, in-memory, no external dependency.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RateLimitConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Requests allowed per IP per rolling 60s window before a 429.
    #[serde(default = "default_requests_per_min")]
    pub requests_per_min: u32,
}

fn default_requests_per_min() -> u32 {
    // Generous default for a personal site: a single page load already
    // fires off a handful of asset requests from one IP in quick succession,
    // and browsing a few pages in a session adds up fast. Tune down once
    // real traffic patterns are visible in the dashboard.
    300
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        RateLimitConfig { enabled: default_true(), requests_per_min: default_requests_per_min() }
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

/// Security response headers added to every response by the edge.
///
/// Each field is the literal header value; setting one to `""` omits that
/// header entirely. A backend that sets its own value for a given header
/// always wins — these only fill in what is absent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecurityConfig {
    /// `Strict-Transport-Security`. Default is one year. Only meaningful over
    /// TLS, which m6-http always terminates.
    #[serde(default = "default_hsts")]
    pub hsts: String,
    /// `X-Content-Type-Options` — stops MIME sniffing.
    #[serde(default = "default_nosniff")]
    pub x_content_type_options: String,
    /// `X-Frame-Options` — clickjacking protection for older browsers.
    /// Modern equivalent is `frame-ancestors` in the CSP below.
    #[serde(default = "default_frame_options")]
    pub x_frame_options: String,
    /// `Referrer-Policy` — keeps paths and queries off cross-origin referers.
    #[serde(default = "default_referrer_policy")]
    pub referrer_policy: String,
    /// `Content-Security-Policy`.
    ///
    /// The default locks down script/object/frame sources but allows inline
    /// *styles* (`style-src 'self' 'unsafe-inline'`), which templated sites
    /// commonly emit via `style="..."` attributes — that one is low-risk
    /// since CSS can't execute arbitrary code. `script-src` deliberately has
    /// no such exception: fix inline scripts/handlers at the template level
    /// (external `.js` + `addEventListener`, or a nonce/hash if an inline
    /// `<script>` block is unavoidable) rather than widening this policy —
    /// `'unsafe-inline'` on `script-src` disables the one thing this header
    /// exists to stop. Override this string only for a genuinely
    /// site-specific *source* (e.g. a CDN the site loads scripts from), not
    /// to work around a template that hasn't been fixed yet. Set `""` to
    /// omit the header value entirely; see `csp_mode` to disable or
    /// log-only the header as a whole instead of rewriting the policy.
    #[serde(default = "default_csp")]
    pub content_security_policy: String,
    /// Controls whether `content_security_policy` is enforced, logged only,
    /// or not sent at all — independent of what the policy string says.
    /// Defaults to `enforce`. Use `report-only` to observe violations (via
    /// browser devtools, or a `report-uri`/`report-to` clause added to the
    /// policy string) before switching a new or tightened policy over to
    /// enforcing, and `off` to omit CSP entirely.
    #[serde(default)]
    pub csp_mode: CspMode,
    /// `Permissions-Policy` — disables browser features/APIs the site never
    /// uses (camera, microphone, geolocation, etc.) so an XSS or a compromised
    /// third-party script can't invoke them.
    #[serde(default = "default_permissions_policy")]
    pub permissions_policy: String,
    /// `Cross-Origin-Opener-Policy` — isolates this site's browsing context
    /// from cross-origin popups/openers, mitigating cross-window attacks
    /// (e.g. Spectre-style side channels, `window.opener` reverse tabnabbing).
    #[serde(default = "default_coop")]
    pub cross_origin_opener_policy: String,
    /// `Cross-Origin-Resource-Policy` — stops other origins from embedding
    /// this site's responses (images, scripts, etc.) in their own pages.
    #[serde(default = "default_corp")]
    pub cross_origin_resource_policy: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum CspMode {
    #[default]
    Enforce,
    ReportOnly,
    Off,
}

fn default_hsts() -> String {
    "max-age=31536000; includeSubDomains; preload".to_string()
}

fn default_nosniff() -> String {
    "nosniff".to_string()
}

fn default_frame_options() -> String {
    "DENY".to_string()
}

fn default_referrer_policy() -> String {
    "strict-origin-when-cross-origin".to_string()
}

fn default_csp() -> String {
    "default-src 'self'; img-src 'self' data:; style-src 'self' 'unsafe-inline'; \
     script-src 'self'; object-src 'none'; frame-ancestors 'none'; base-uri 'self'; \
     form-action 'self'"
        .to_string()
}

fn default_permissions_policy() -> String {
    "camera=(), microphone=(), geolocation=(), payment=(), usb=(), \
     interest-cohort=()"
        .to_string()
}

fn default_coop() -> String {
    "same-origin".to_string()
}

fn default_corp() -> String {
    "same-origin".to_string()
}

impl Default for SecurityConfig {
    fn default() -> Self {
        SecurityConfig {
            hsts: default_hsts(),
            x_content_type_options: default_nosniff(),
            x_frame_options: default_frame_options(),
            referrer_policy: default_referrer_policy(),
            content_security_policy: default_csp(),
            csp_mode: CspMode::default(),
            permissions_policy: default_permissions_policy(),
            cross_origin_opener_policy: default_coop(),
            cross_origin_resource_policy: default_corp(),
        }
    }
}

impl SecurityConfig {
    /// `(name, value)` pairs for every non-empty setting.
    ///
    /// The CSP pair's header *name* depends on `csp_mode`: `enforce` sends
    /// `content-security-policy` (blocking), `report-only` sends
    /// `content-security-policy-report-only` (same policy, browser reports
    /// violations but does not block), `off` omits the pair regardless of
    /// what `content_security_policy` contains.
    pub fn resolved_headers(&self) -> Vec<(String, String)> {
        let csp_header_name = match self.csp_mode {
            CspMode::Enforce => "content-security-policy",
            CspMode::ReportOnly => "content-security-policy-report-only",
            CspMode::Off => "",
        };
        [
            ("strict-transport-security", self.hsts.as_str()),
            ("x-content-type-options", self.x_content_type_options.as_str()),
            ("x-frame-options", self.x_frame_options.as_str()),
            ("referrer-policy", self.referrer_policy.as_str()),
            ("permissions-policy", self.permissions_policy.as_str()),
            ("cross-origin-opener-policy", self.cross_origin_opener_policy.as_str()),
            ("cross-origin-resource-policy", self.cross_origin_resource_policy.as_str()),
            (csp_header_name, self.content_security_policy.as_str()),
        ]
        .into_iter()
        .filter(|(k, v)| !k.is_empty() && !v.is_empty())
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
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
    analytics: Option<AnalyticsConfig>,
    rate_limit: Option<RateLimitConfig>,
    errors: Option<ErrorsConfig>,
    security: Option<SecurityConfig>,
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
    node:   Option<NodeConfig>,
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
    // We rely on RawSystemToml only accepting `server`/`node`.

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

    // Node identity comes from system config, not site.toml (see NodeConfig
    // doc comment) — falls back to [site].name with a warning so an older
    // system config without [node] still starts up instead of hard-failing.
    let node = system_parsed.node.unwrap_or_else(|| {
        warn!(
            file = %system_config_path.display(),
            "system config: no [node].name set, falling back to [site].name for node identity"
        );
        NodeConfig { name: site_name.clone() }
    });

    let server = ServerConfig {
        bind,
        tls_cert: tls_cert_path.to_string_lossy().into_owned(),
        tls_key: tls_key_path.to_string_lossy().into_owned(),
        backend_timeout_secs,
        h2c_bind,
    };
    let log = site_parsed.log.unwrap_or_default();
    let analytics = site_parsed.analytics.unwrap_or_default();
    let rate_limit = site_parsed.rate_limit.unwrap_or_default();
    let errors = site_parsed.errors.unwrap_or_default();
    let security = site_parsed.security.unwrap_or_default();

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
        node,
        server,
        log,
        analytics,
        rate_limit,
        errors,
        security,
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
                    if key != "server" && key != "node" {
                        warn!(
                            key = %key,
                            file = %system_config_path.display(),
                            "system config: ignoring non-[server]/[node] key"
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
    fn test_security_csp_mode_defaults_to_enforce() {
        let dir = make_test_dir();
        setup_minimal(dir.path());
        let cfg = load(dir.path(), &dir.path().join("system.toml")).unwrap();
        assert_eq!(cfg.security.csp_mode, CspMode::Enforce);
    }

    #[test]
    fn test_security_csp_mode_parses_report_only() {
        let dir = make_test_dir();
        let mut site = minimal_site_toml();
        site.push_str("\n[security]\ncsp_mode = \"report-only\"\n");
        write_file(dir.path(), "site.toml", &site);
        write_file(dir.path(), "system.toml", &minimal_system_toml());
        write_file(dir.path(), "cert.pem", "dummy");
        write_file(dir.path(), "key.pem", "dummy");
        let cfg = load(dir.path(), &dir.path().join("system.toml")).unwrap();
        assert_eq!(cfg.security.csp_mode, CspMode::ReportOnly);
    }

    #[test]
    fn test_security_csp_mode_parses_off() {
        let dir = make_test_dir();
        let mut site = minimal_site_toml();
        site.push_str("\n[security]\ncsp_mode = \"off\"\n");
        write_file(dir.path(), "site.toml", &site);
        write_file(dir.path(), "system.toml", &minimal_system_toml());
        write_file(dir.path(), "cert.pem", "dummy");
        write_file(dir.path(), "key.pem", "dummy");
        let cfg = load(dir.path(), &dir.path().join("system.toml")).unwrap();
        assert_eq!(cfg.security.csp_mode, CspMode::Off);
    }

    #[test]
    fn test_security_content_security_policy_string_still_overridable_alongside_mode() {
        let dir = make_test_dir();
        let mut site = minimal_site_toml();
        site.push_str(
            "\n[security]\ncsp_mode = \"report-only\"\ncontent_security_policy = \"default-src 'self'\"\n",
        );
        write_file(dir.path(), "site.toml", &site);
        write_file(dir.path(), "system.toml", &minimal_system_toml());
        write_file(dir.path(), "cert.pem", "dummy");
        write_file(dir.path(), "key.pem", "dummy");
        let cfg = load(dir.path(), &dir.path().join("system.toml")).unwrap();
        assert_eq!(cfg.security.csp_mode, CspMode::ReportOnly);
        assert_eq!(cfg.security.content_security_policy, "default-src 'self'");
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
