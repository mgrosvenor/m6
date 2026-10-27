/// Config loading: site.toml + system config merging and validation.
use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

/// Full merged configuration for m6-http.
#[derive(Debug, Clone, Serialize)]
pub struct Config {
    pub site: SiteConfig,
    pub node: NodeConfig,
    pub server: ServerConfig,
    pub log: LogConfig,
    pub analytics: AnalyticsConfig,
    pub health: HealthConfig,
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
    /// 301 `www.<domain>` to the bare `<domain>`. Default true.
    ///
    /// Without it the two hostnames serve byte-identical pages, which is
    /// duplicate content: search engines pick a canonical themselves and split
    /// ranking signals across both until they do.
    ///
    /// Only the `www.` alias is redirected, never an arbitrary unrecognised
    /// Host. Node hostnames (`node-a.example.com`) must keep serving directly,
    /// because per-node verification depends on reaching a specific node by
    /// name rather than through the GeoDNS-routed apex.
    #[serde(default = "default_true")]
    pub redirect_www: bool,
    /// Path to a machine-readable description of the site (conventionally
    /// `/llms.txt`). When set, every **HTML** response carries
    /// `Link: <path>; rel="describedby"`.
    ///
    /// The point of the header over the equivalent `<link rel="describedby">`
    /// in `<head>` is that it arrives before the page is parsed, so anything
    /// inspecting response headers on its first request discovers the site's
    /// machine-facing layer without reading any markup. Both are worth having;
    /// the markup one is the fallback for readers that only see the document.
    ///
    /// Empty (the default) omits the header, so a deployment without such a
    /// file does not advertise one.
    #[serde(default)]
    pub describedby: String,
}

/// This deployment's node identity, for example "origin" or "edge-a".
///
/// Distinct from `[site].name`, which is the site's own display name and is
/// identical across every node: they all serve the same site, from the same
/// byte-for-byte site.toml. The node identity comes from the per-node *system*
/// config instead, one file per node.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeConfig {
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    pub bind: String,
    /// TLS certificate/key. `None` only in redirect mode (see `redirect_bind`),
    /// which terminates no TLS — every serving path requires both, and `load()`
    /// rejects a config that omits them outside that mode. Typed as `Option`
    /// rather than defaulted to a dummy path so "no certificate here" is a
    /// state the compiler knows about, not a convention a future reader has to
    /// infer from an empty string.
    pub tls_cert: Option<String>,
    pub tls_key: Option<String>,
    /// Timeout in seconds for a backend call (connect + write + read). Default: 30.
    #[serde(default = "default_backend_timeout_secs")]
    pub backend_timeout_secs: u64,
    /// Optional H2C (HTTP/2 cleartext) listener address.
    /// Intended for use over a private backbone between nodes.
    #[serde(default)]
    pub h2c_bind: Option<String>,
    /// Listeners this server treats as internal, by address. See
    /// docs/m6-site-toml.md. Validated by `load`: each entry is an IP and port
    /// in a local range (`m6_core::net::is_private_ip`) equal to `bind` or
    /// `h2c_bind`. Read at startup; a change needs a restart.
    #[serde(default)]
    pub internal_binds: Vec<String>,
    /// Run this process as a plain-HTTP redirector on this address instead of
    /// a normal server: every request is answered 301 to `https://<Host><path>`.
    ///
    /// Meant for a **second m6-http process** alongside the real one, not for
    /// the `:443` instance to also listen here. Different port, different
    /// failure domain — a slow client on :80 cannot stall TLS serving because
    /// it is not sharing that process's event loop. In this mode nothing else
    /// in the config is read: no TLS, no QUIC, no backends, no routes, no
    /// cache, so `tls_cert`/`tls_key` may point nowhere.
    #[serde(default)]
    pub redirect_bind: Option<String>,
    /// Warm this node's own cache when it starts.
    ///
    /// The server knows its route table, so it can fill its own cache instead of
    /// something outside it doing so. On a cache node that means one fetch per
    /// warmable route per encoding from the origin at startup, after the listeners
    /// are up, so the first real visitor to each page gets a hit rather than paying
    /// for the miss.
    ///
    /// This replaced a shell script that a systemd unit ran on each node
    /// (`m6-warm-local`), which parsed the node's own site.toml **with a regex** to
    /// find the warmable routes. The server has the parsed config, so it selects
    /// them properly: concrete paths only, skipping the configured error path and
    /// anything a route marks `no-store`.
    ///
    /// Off by default. A server should not do network I/O at boot unless asked: on a
    /// cache node this reaches across the backbone to the origin, and a fleet
    /// restarting together would all reach at once.
    ///
    /// Warming rides the existing background-fetch queue, which drains one entry per
    /// event-loop iteration. So it cannot delay startup, cannot block serving, and
    /// cannot stampede: it is the same mechanism that refreshes stale entries.
    #[serde(default)]
    pub warm_on_start: bool,
    /// Request methods this server will serve. Anything else is answered 405
    /// with an `Allow` header, before routing, cache lookup or backend
    /// dispatch.
    ///
    /// Default is GET, HEAD, POST, which is everything a site with forms and
    /// no write API does. POST is never cached (see
    /// `cache::method_may_write_cache`), only forwarded.
    ///
    /// Nothing validated the method before this existed, so every verb --
    /// PUT, DELETE, TRACE, and entirely invented ones like FOO -- was answered
    /// 200 with the cached page. TRACE especially should never be served: it
    /// is a cross-site tracing vector and a content site has no use for it.
    ///
    /// Configurable rather than hard-coded because the CMS routes (currently
    /// disabled) need PUT and DELETE when they come back.
    #[serde(default = "default_allowed_methods")]
    pub allowed_methods: Vec<String>,
}

impl ServerConfig {
    /// True when `listener` (a bind string from this config) is named in
    /// `internal_binds`. Compared as socket addresses, so `[::1]:80` equals
    /// `[0:0:0:0:0:0:0:1]:80`. A listener given as a host name never matches.
    pub fn is_internal(&self, listener: &str) -> bool {
        let Ok(l) = listener.parse::<std::net::SocketAddr>() else {
            return false;
        };
        self.internal_binds
            .iter()
            .any(|e| e.parse::<std::net::SocketAddr>().is_ok_and(|a| a == l))
    }
}

fn default_allowed_methods() -> Vec<String> {
    vec!["GET".to_string(), "HEAD".to_string(), "POST".to_string()]
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
        LogConfig {
            level: default_log_level(),
            format: default_log_format(),
        }
    }
}

/// Per-request traffic logging (session cookie, referrer, UA, cache state,
/// node identity, ...) — separate from operational `[log]` above. Written to
/// its own file via `m6_core::log::init_with_analytics`, always JSON.
///
/// Rotate by renaming the file and sending SIGUSR1: the process reopens the path.
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
        AnalyticsConfig {
            enabled: default_true(),
            log_path: default_analytics_log_path(),
        }
    }
}

/// The per-node health endpoint (see `crate::health`).
///
/// Answered inside m6-http ahead of routing, the cache and every backend, so
/// it costs a JSON serialisation rather than a page render.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Configurable rather than hardcoded so a site that legitimately wants
    /// to serve its own `/health` page can move ours out of the way instead
    /// of being silently shadowed by it.
    #[serde(default = "default_health_path")]
    pub path: String,
    /// Bearer token gating the metrics block (counters + latency
    /// percentiles). `None` -- the default -- means metrics are never served,
    /// so forgetting to configure it fails closed.
    ///
    /// Path to a file whose contents are the bearer token.
    ///
    /// The *path* is configuration and belongs in git; the *contents* are a
    /// secret and must not. site.toml is rendered from a committed template
    /// and shipped byte-identically to every node, so a token written inline
    /// there would be a secret in the repository. The same split applies to
    /// every other credential a node needs: the file is mode 0600, owned by
    /// root, and exists only on the box.
    #[serde(default)]
    pub metrics_token_file: Option<String>,
    /// The resolved token, read from `metrics_token_file` at load.
    ///
    /// `serde(skip)` in both directions, deliberately. It must never be
    /// printed by `--dump-config`, which serialises the whole Config, and it
    /// must not be settable inline from TOML -- otherwise the file
    /// indirection above is advisory rather than enforced.
    #[serde(skip)]
    pub metrics_token: Option<String>,
    /// Path for the metrics endpoint. Separate from `path` on purpose: the
    /// health check must stay constant-cost, and percentiles are not.
    #[serde(default = "default_perf_path")]
    pub perf_path: String,
    /// Where the traffic summary is served. Same token as `/perf`.
    ///
    /// A third path rather than more fields on `/perf`, for the reason that
    /// split `/health` from `/perf` in the first place: the cost profiles
    /// differ. `/perf` sorts a reservoir in memory; this reads the tail of a
    /// 47MB file. Putting the expensive one behind a conditional on a cheaper
    /// path leaves the expensive branch one misconfigured header away from
    /// being taken on every request, so each answer's cost stays a property of
    /// its path.
    #[serde(default = "default_traffic_path")]
    pub traffic_path: String,
    /// How long a traffic summary is reused before the analytics log is read
    /// again.
    ///
    /// Without this a monitor polling every 30 seconds makes every node
    /// re-read its analytics tail every 30 seconds, forever, to produce an
    /// answer that changes slowly. The check that consumes this runs hourly.
    #[serde(default = "default_traffic_cache_s")]
    pub traffic_cache_s: u64,
    /// `role:<name>` or `group:<name>`. When set, `/perf` and `/traffic` need
    /// an API token that carries this claim, verified with `[auth] public_key`
    /// exactly as a route `require` is: 401 for no or a bad token, 403 for the
    /// wrong claim. It wins over `metrics_token_file`.
    #[serde(default)]
    pub metrics_require: Option<String>,
    /// The IP addresses allowed to read `/perf` and `/traffic`, as written.
    /// Any other peer gets 404. Unset means no address rule.
    ///
    /// Each entry must be an IP address in a local range
    /// (`m6_core::net::is_private_ip`). Names and ranges are refused. The
    /// peer address is the one a completed TCP or QUIC handshake proved, and
    /// the endpoints are refused on an internal h2c listener, where the
    /// address is a forwarded claim.
    #[serde(default)]
    pub metrics_peers: Option<Vec<String>>,
    /// `metrics_peers`, parsed and canonical (an IPv4-mapped IPv6 address is
    /// stored as IPv4), filled by `load`. The gate reads only this: `None`
    /// means no address rule. Compared on the request path with no allocation.
    #[serde(skip)]
    pub metrics_peer_ips: Option<Vec<std::net::IpAddr>>,
}

fn default_perf_path() -> String {
    "/perf".to_string()
}

fn default_traffic_path() -> String {
    "/traffic".to_string()
}

fn default_traffic_cache_s() -> u64 {
    60
}

fn default_health_path() -> String {
    "/health".to_string()
}

impl Default for HealthConfig {
    fn default() -> Self {
        HealthConfig {
            enabled: default_true(),
            path: default_health_path(),
            metrics_token_file: None,
            metrics_token: None,
            perf_path: default_perf_path(),
            traffic_path: default_traffic_path(),
            traffic_cache_s: default_traffic_cache_s(),
            metrics_require: None,
            metrics_peers: None,
            metrics_peer_ips: None,
        }
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
        RateLimitConfig {
            enabled: default_true(),
            requests_per_min: default_requests_per_min(),
        }
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
        ErrorsConfig {
            mode: default_errors_mode(),
            path: None,
            verbose_fallback: false,
        }
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
            (
                "x-content-type-options",
                self.x_content_type_options.as_str(),
            ),
            ("x-frame-options", self.x_frame_options.as_str()),
            ("referrer-policy", self.referrer_policy.as_str()),
            ("permissions-policy", self.permissions_policy.as_str()),
            (
                "cross-origin-opener-policy",
                self.cross_origin_opener_policy.as_str(),
            ),
            (
                "cross-origin-resource-policy",
                self.cross_origin_resource_policy.as_str(),
            ),
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
    /// Optional. When given it must name a `[[backend]]`. Nothing reads it
    /// beyond that check: verification needs only `public_key`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend: Option<String>,
    pub public_key: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackendConfig {
    pub name: String,
    /// Unix socket glob, e.g. "/run/m6/m6-html-*.sock"
    pub sockets: Option<String>,
    /// URL upstream, e.g. `https://api.example.com`
    pub url: Option<String>,
    /// Skip TLS certificate verification for URL backends.
    /// For testing with self-signed certs only — do not use in production.
    #[serde(default)]
    pub tls_skip_verify: bool,
    /// Whether this backend compresses its own responses.
    ///
    /// **m6-http is a cache, not a transformer.** It does not compress: it
    /// negotiates between representations a backend produced and caches each
    /// one. `brotli` and `flate2` are in m6-core, on the backend side, and
    /// nothing under m6-http/src calls a compressor.
    ///
    /// That contradicted two normative documents, which told backends not to
    /// compress on the grounds that the proxy would. A C, Go or Python backend
    /// written from that advice had its bytes served uncompressed forever, and
    /// it was invisible for the Rust services only because m6-core compresses
    /// for them. See docs/m6-backend-examples.md §10.5.
    ///
    /// So the two sides agree in config instead. The edge reads this to decide
    /// whether to promise variants by varying on `Accept-Encoding`; the backend
    /// reads the SAME key and refuses to start if it disagrees with what it can
    /// actually do (`m6_core::compress::check_declared_support`).
    ///
    /// DEFAULT TRUE, deliberately. A backend built on m6-core compresses by
    /// default, and that is the ordinary case, so true matches what is already
    /// running. A backend that does not compress says so, and then the edge
    /// stops advertising an encoding dimension that has exactly one value.
    #[serde(default = "default_compresses")]
    pub compresses: bool,
}

/// See [`BackendConfig::compresses`]. True because that is what the fleet does.
fn default_compresses() -> bool {
    true
}

impl Config {
    /// Whether the named backend compresses its own responses.
    ///
    /// An unknown name answers `true`, which is the conservative direction: it
    /// keeps `Vary: Accept-Encoding` on a response whose origin we cannot
    /// identify. Answering `false` there would drop the header from a response
    /// that really does have several encodings, and a shared cache would then
    /// hand one client's brotli body to a client that asked for identity. A
    /// redundant `Vary` costs cache efficiency; a missing one is a correctness
    /// bug.
    pub fn backend_compresses(&self, name: &str) -> bool {
        self.backends
            .iter()
            .find(|b| b.name == name)
            .map(|b| b.compresses)
            .unwrap_or(true)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteConfig {
    pub path: String,
    pub backend: String,
    /// Auth requirement e.g. "group:editors" or "role:admin"
    pub require: Option<String>,
    /// Captured only so `load()` can warn that m6-http ignores it — see
    /// `warn_ignored_cache_key`. Deliberately not wired to anything.
    pub cache: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteGroupConfig {
    /// Glob pattern relative to site dir
    pub glob: String,
    /// URL path template with {stem} placeholder
    pub path: String,
    pub backend: String,
    pub require: Option<String>,
    /// See `RouteConfig::cache` — captured to warn, not to act on.
    pub cache: Option<String>,
}

// ── Raw serde types for parsing site.toml ──────────────────────────────────

#[derive(Debug, Deserialize)]
struct RawSiteToml {
    site: Option<RawSiteSection>,
    server: Option<RawServerSection>,
    log: Option<LogConfig>,
    analytics: Option<AnalyticsConfig>,
    health: Option<HealthConfig>,
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
    redirect_www: Option<bool>,
    describedby: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawServerSection {
    bind: Option<String>,
    warm_on_start: Option<bool>,
    tls_cert: Option<String>,
    tls_key: Option<String>,
    backend_timeout_secs: Option<u64>,
    h2c_bind: Option<String>,
    internal_binds: Option<Vec<String>>,
    redirect_bind: Option<String>,
    allowed_methods: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
struct RawSystemToml {
    server: Option<RawServerSection>,
    node: Option<NodeConfig>,
    /// `[auth]` may live here instead of site.toml (never both), so the
    /// verify key's path is owned by the same file as the node's other
    /// credentials.
    auth: Option<AuthConfig>,
    /// Only the access keys: a node's own credentials and peers belong to the
    /// node, not to the site that is shipped to every node.
    /// Kept as a raw value so a site.toml passed as its own system config
    /// (one file for both, as a development script does) is not rejected for
    /// the `[health]` keys only site.toml may carry. It is parsed strictly
    /// into `RawSystemHealth` when the two files differ.
    health: Option<toml::Value>,
}

/// The `[health]` keys the system config may carry. Anything else fails to
/// parse, so a misplaced key is loud instead of ignored.
/// `NODE_HEALTH_KEYS` lists the same keys for `misplaced_keys`; change both together.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSystemHealth {
    metrics_require: Option<String>,
    metrics_peers: Option<Vec<String>>,
    /// A credential path, so it belongs to the node. Absolute only, as
    /// `[auth].public_key` is in this file.
    metrics_token_file: Option<String>,
}

/// Parse `[health] metrics_peers` into canonical addresses.
///
/// Only an IP literal in a local range is accepted. A name is never resolved,
/// a range is not a peer, and a public or wildcard address could never be a
/// private monitor, so each is a load error.
fn validate_metrics_peers(
    peers: Option<&[String]>,
) -> anyhow::Result<Option<Vec<std::net::IpAddr>>> {
    let Some(peers) = peers else {
        return Ok(None);
    };
    if peers.is_empty() {
        anyhow::bail!(
            "config error: [health].metrics_peers is empty; list the peers allowed to read /perf and /traffic, or remove the key"
        );
    }
    let mut ips = Vec::with_capacity(peers.len());
    for e in peers {
        let ip: std::net::IpAddr = e.parse().map_err(|_| {
            anyhow::anyhow!(
                "config error: [health].metrics_peers entry \"{e}\" is not an IP address such as \"10.0.0.5\" or \"fd00::5\"; names are never resolved and ranges are not accepted"
            )
        })?;
        if !m6_core::net::is_private_ip(ip) {
            anyhow::bail!(
                "config error: [health].metrics_peers entry \"{e}\" is not in a local range; a metrics peer must be a loopback, private (RFC 1918), link-local, shared (100.64.0.0/10) or IPv6 unique-local address"
            );
        }
        ips.push(ip.to_canonical());
    }
    Ok(Some(ips))
}

// ── Loading ─────────────────────────────────────────────────────────────────

/// Check `[server] internal_binds` against the listeners and the local ranges.
///
/// Order: the `h2c_bind` migration guard, then each entry in list order for an
/// IP and port, not the TLS bind, a local range and the h2c listener. Host names are never resolved.
fn validate_internal_binds(
    internal_binds: Option<&[String]>,
    bind: &str,
    h2c_bind: Option<&str>,
) -> anyhow::Result<()> {
    if let (Some(h2c), None) = (h2c_bind, internal_binds) {
        let what = "m6-http no longer decides from an address whether a listener is internal";
        let public = "internal_binds = [] to treat it as public";
        match h2c.parse::<std::net::SocketAddr>() {
            Err(_) => anyhow::bail!(
                "config error: [server].h2c_bind = \"{h2c}\" is set but [server].internal_binds is not. {what}. An internal h2c listener needs an IP address and port such as \"10.0.0.1:8080\", and \"{h2c}\" is not one. Bind h2c_bind to an IP address and name it in internal_binds, or add {public}"
            ),
            Ok(sa) if !m6_core::net::is_private_ip(sa.ip()) => anyhow::bail!(
                "config error: [server].h2c_bind = \"{h2c}\" is set but [server].internal_binds is not. {what}. To be internal the listener must be bound to a local address (loopback, private, link-local, shared or unique-local), and \"{h2c}\" is not. Bind it to a local address and name it in internal_binds, or add {public}"
            ),
            Ok(_) => anyhow::bail!(
                "config error: [server].h2c_bind = \"{h2c}\" is set but [server].internal_binds is not. {what}. Add internal_binds = [\"{h2c}\"] to keep trusting it as the private backbone (forwarded client addresses believed, the custom error path allowed, statistics labelled internal), or {public}"
            ),
        }
    }
    let listener = |s: &str| s.parse::<std::net::SocketAddr>().ok();
    for e in internal_binds.unwrap_or_default() {
        let Ok(sa) = e.parse::<std::net::SocketAddr>() else {
            anyhow::bail!(
                "config error: [server].internal_binds entry \"{e}\" is not an IP address and port such as \"10.0.0.1:8080\" or \"[fd00::1]:8080\"; names are never resolved"
            );
        };
        if listener(bind) == Some(sa) && h2c_bind.and_then(listener) != Some(sa) {
            anyhow::bail!(
                "config error: [server].internal_binds entry \"{e}\" is [server].bind, the TLS and QUIC listener. Only the h2c_bind listener can be internal, because the TLS and QUIC listener faces clients"
            );
        }
        if !m6_core::net::is_private_ip(sa.ip()) {
            anyhow::bail!(
                "config error: [server].internal_binds entry \"{e}\" is not in a local range; an internal binding must be a loopback, private (RFC 1918), link-local, shared (100.64.0.0/10) or IPv6 unique-local address"
            );
        }
        if h2c_bind.and_then(listener) != Some(sa) {
            anyhow::bail!(
                "config error: [server].internal_binds entry \"{e}\" is not this server's h2c listener; it must equal [server].h2c_bind"
            );
        }
    }
    Ok(())
}

/// Whether `site.toml` and the system config are one file (one-file mode, as a
/// development script runs). A path that cannot be resolved is not one file.
fn is_one_file(site_toml: &Path, system_config: &Path) -> bool {
    match (
        std::fs::canonicalize(site_toml),
        std::fs::canonicalize(system_config),
    ) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

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
    let mut system_parsed: RawSystemToml = toml::from_str(&system_raw)
        .with_context(|| format!("parsing {}", system_config_path.display()))?;
    // When both arguments name the same file, its [auth] and [health] belong
    // to the site config alone, not to a second declaration.
    if is_one_file(&site_toml_path, system_config_path) {
        system_parsed.auth = None;
        system_parsed.health = None;
    }
    let system_health: Option<RawSystemHealth> = system_parsed
        .health
        .take()
        .map(|v| v.try_into())
        .transpose()
        .with_context(|| format!("parsing [health] in {}", system_config_path.display()))?;

    // Warn on unexpected sections in system config (we only check for unknown
    // top-level keys by seeing if extra keys exist; toml parsing uses deny_unknown_fields
    // is opt-in, but the spec says warn and ignore instead of error).
    // We rely on RawSystemToml only accepting `server`/`node`/`auth`/`health`.

    // Build merged server config:
    // site.toml [server] provides base values, system config [server] overrides.
    let site_server = site_parsed.server.unwrap_or(RawServerSection {
        bind: None,
        warm_on_start: None,
        tls_cert: None,
        tls_key: None,
        backend_timeout_secs: None,
        h2c_bind: None,
        internal_binds: None,
        redirect_bind: None,
        allowed_methods: None,
    });
    let sys_server = system_parsed.server.unwrap_or(RawServerSection {
        bind: None,
        warm_on_start: None,
        tls_cert: None,
        tls_key: None,
        backend_timeout_secs: None,
        h2c_bind: None,
        internal_binds: None,
        redirect_bind: None,
        allowed_methods: None,
    });

    // Resolved before `bind`, because in redirect mode it supplies the default.
    let redirect_bind = sys_server.redirect_bind.or(site_server.redirect_bind);
    // Same precedence as every other server field: the system config wins, because
    // warming is a property of the node, not of the site it serves. A cache node
    // wants it; a laptop running the site locally does not.
    let warm_on_start = sys_server
        .warm_on_start
        .or(site_server.warm_on_start)
        .unwrap_or(false);

    // A redirector binds `redirect_bind` and nothing else, so requiring a
    // separate `bind` there would mean every redirect config carried the same
    // address twice, or carried a second one that was never listened on.
    let bind = match sys_server.bind.or(site_server.bind) {
        Some(b) => b,
        None => redirect_bind
            .clone()
            .ok_or_else(|| anyhow::anyhow!("config error: [server].bind is required"))?,
    };
    let tls_cert = sys_server.tls_cert.or(site_server.tls_cert);
    let tls_key = sys_server.tls_key.or(site_server.tls_key);
    if redirect_bind.is_none() {
        if tls_cert.is_none() {
            anyhow::bail!("config error: [server].tls_cert is required");
        }
        if tls_key.is_none() {
            anyhow::bail!("config error: [server].tls_key is required");
        }
    }
    let backend_timeout_secs = sys_server
        .backend_timeout_secs
        .or(site_server.backend_timeout_secs)
        .unwrap_or(30);
    let h2c_bind = sys_server.h2c_bind.or(site_server.h2c_bind);
    // Nothing is inferred from an address: a listener is internal only when
    // this list names it. A redirector binds nothing the list could name, so a
    // site.toml shared with one must not break it.
    let internal_binds = sys_server.internal_binds.or(site_server.internal_binds);
    if redirect_bind.is_none() {
        validate_internal_binds(internal_binds.as_deref(), &bind, h2c_bind.as_deref())?;
    }
    let internal_binds = internal_binds.unwrap_or_default();

    // Resolve TLS cert/key paths relative to site_dir if not absolute.
    let tls_cert_path = tls_cert.as_deref().map(|c| resolve_path(site_dir, c));
    let tls_key_path = tls_key.as_deref().map(|k| resolve_path(site_dir, k));

    // Validate [site] required keys
    let raw_site = site_parsed.site.unwrap_or(RawSiteSection {
        name: None,
        domain: None,
        redirect_www: None,
        describedby: None,
    });
    let site_name = raw_site
        .name
        .ok_or_else(|| anyhow::anyhow!("config error: [site].name is required"))?;
    let site_domain = raw_site
        .domain
        .ok_or_else(|| anyhow::anyhow!("config error: [site].domain is required"))?;

    // Node identity comes from system config, not site.toml (see NodeConfig
    // doc comment) — falls back to [site].name, with a warning from main, so an older
    // system config without [node] still starts up instead of hard-failing.
    // The fallback is reported by `warn_node_fallback` from main, because
    // warn! inside load() goes to a subscriber that does not exist yet.
    let node = system_parsed.node.unwrap_or_else(|| NodeConfig {
        name: site_name.clone(),
    });

    let server = ServerConfig {
        bind,
        tls_cert: tls_cert_path
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned()),
        tls_key: tls_key_path
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned()),
        backend_timeout_secs,
        h2c_bind,
        internal_binds,
        warm_on_start,
        redirect_bind: redirect_bind.clone(),
        // Uppercased once at load so the hot-path check is a plain comparison
        // rather than a case-insensitive one per request.
        allowed_methods: sys_server
            .allowed_methods
            .or(site_server.allowed_methods)
            .unwrap_or_else(default_allowed_methods)
            .iter()
            .map(|m| m.trim().to_ascii_uppercase())
            .filter(|m| !m.is_empty())
            .collect(),
    };
    let log = site_parsed.log.unwrap_or_default();
    let analytics = site_parsed.analytics.unwrap_or_default();
    let mut health = site_parsed.health.unwrap_or_default();
    // The system config wins key by key, as [server] keys do.
    if let Some(sys) = system_health {
        health.metrics_require = sys.metrics_require.or(health.metrics_require);
        health.metrics_peers = sys.metrics_peers.or(health.metrics_peers);
        if let Some(p) = sys.metrics_token_file {
            // A relative path resolves against the site directory, as
            // `tls_cert` does, so the service does not depend on the working
            // directory it was started from.
            let p = resolve_path(site_dir, &p).to_string_lossy().into_owned();
            health.metrics_token_file = Some(p);
        }
    }
    // Read the token here so the running server never touches the file on the
    // request path. It is read after the merge so that a path from either file
    // is the one read. Failure is deliberately not fatal: a missing token file
    // means /perf answers 404 (fails closed), and refusing to boot the whole
    // server over a metrics credential would be disproportionate -- the same
    // call made for the inert `cache =` route key. It is reported by
    // `warn_health_token` from main, because warn! inside load() goes to a
    // subscriber that does not exist yet (load supplies the log level).
    if let Some(ref path) = health.metrics_token_file {
        health.metrics_token = std::fs::read_to_string(path)
            .ok()
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty());
    }
    health.metrics_peer_ips = validate_metrics_peers(health.metrics_peers.as_deref())?;
    let rate_limit = site_parsed.rate_limit.unwrap_or_default();
    let errors = site_parsed.errors.unwrap_or_default();
    let security = site_parsed.security.unwrap_or_default();

    // Validate TLS files exist.
    //
    // Skipped entirely in redirect mode: that process terminates no TLS, so
    // requiring a certificate would mean every :80 redirector needed a copy of
    // one -- and would fail to start on a node whose cert had expired, taking
    // out the plain-HTTP redirect for a reason that has nothing to do with it.
    if redirect_bind.is_none() {
        for (label, path) in [("tls_cert", &tls_cert_path), ("tls_key", &tls_key_path)] {
            // Both are Some here: the required-key check above rejects a
            // non-redirect config that omits either.
            if let Some(path) = path {
                if !path.exists() {
                    anyhow::bail!("config error: {} file not found: {}", label, path.display());
                }
            }
        }
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
                route.path,
                route.backend
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
                rg.path,
                rg.backend
            );
        }
    }

    // Validate auth. One owner: declaring [auth] in both files would make a
    // whole-section override silently change which key verifies every route.
    let auth = match (site_parsed.auth, system_parsed.auth) {
        (Some(_), Some(_)) => anyhow::bail!(
            "config error: [auth] is declared in both site.toml and the system config; declare it in one"
        ),
        (None, Some(a)) => {
            // A relative `public_key` resolves against the site directory below,
            // as `tls_cert` does.
            Some(a)
        }
        (site, None) => site,
    };
    if let Some(ref a) = auth {
        let key_path = resolve_path(site_dir, &a.public_key);
        if !key_path.exists() {
            anyhow::bail!(
                "config error: [auth].public_key file not found: {}",
                key_path.display()
            );
        }
        if let Some(ref b) = a.backend {
            if !backend_names.contains(b.as_str()) {
                anyhow::bail!("config error: [auth].backend `{}` not in [[backend]]", b);
            }
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

    if let Some(ref r) = health.metrics_require {
        if auth.is_none() {
            anyhow::bail!("config error: [health].metrics_require needs [auth] with a public_key");
        }
        let named = matches!(
            crate::auth::Require::parse(r),
            Some(crate::auth::Require::Role(n) | crate::auth::Require::Group(n)) if !n.is_empty()
        );
        if !named {
            anyhow::bail!(
                "config error: [health].metrics_require = \"{r}\" must be \"role:<name>\" or \"group:<name>\""
            );
        }
    }

    // Validate errors
    if errors.mode == "custom" && errors.path.is_none() {
        anyhow::bail!("config error: [errors] mode = \"custom\" requires `path`");
    }

    Ok(Config {
        site: SiteConfig {
            name: site_name,
            domain: site_domain,
            redirect_www: raw_site.redirect_www.unwrap_or(true),
            describedby: raw_site.describedby.unwrap_or_default(),
        },
        node,
        server,
        log,
        analytics,
        health,
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

/// Warn about `cache = "..."` on routes, which m6-http ignores.
///
/// It is a real, honoured directive in m6-render's route tables, which makes
/// it very natural to write here too — but m6-http takes its caching decision
/// from the *response's* `Cache-Control`, never from the route table, so the
/// line does exactly nothing. Silently dropping it is the trap: the config
/// reads as though `no-store` is enforced at the edge when nothing enforces
/// it.
///
/// Warns rather than bailing, so a config carrying a line that has always
/// been inert can never be turned into a boot failure by this check.
///
/// Called from `main` *after* logging is initialised, not from `load()`:
/// `load()` runs first (it supplies the log level), so a `warn!` inside it
/// goes to a subscriber that does not exist yet and is silently dropped —
/// which is exactly the failure mode this function exists to complain about.
pub fn warn_ignored_route_cache_keys(config: &Config) {
    for route in &config.routes {
        if let Some(cache) = &route.cache {
            warn!(
                route = %route.path, value = %cache,
                "site config: `cache` on a [[route]] is ignored by m6-http — set Cache-Control on the backend response instead"
            );
        }
    }
    for rg in &config.route_groups {
        if let Some(cache) = &rg.cache {
            warn!(
                route_group = %rg.path, value = %cache,
                "site config: `cache` on a [[route_group]] is ignored by m6-http — set Cache-Control on the backend response instead"
            );
        }
    }
}

/// Warn about any extra top-level keys in system config toml.
/// We do this via raw parsing.
/// Report a configured-but-unusable metrics token file.
///
/// Called from `main` after logging is initialised. Warning from inside
/// `load()` would print nothing at all: `load()` runs before the subscriber
/// exists, because it is what supplies the log level.
pub fn warn_health_token(health: &HealthConfig) {
    if let Some(ref path) = health.metrics_token_file {
        if health.metrics_require.is_some() {
            info!(
                file = %path,
                "health: metrics_require is set, so metrics_token_file is ignored"
            );
        } else {
            info!(
                file = %path,
                "health: metrics_token_file is the legacy gate; metrics_require checks an API token instead"
            );
            if health.metrics_token.is_none() {
                warn!(
                    file = %path,
                    "health: metrics token file missing or empty -- /perf will answer 404"
                );
            }
        }
    }
}

/// Keys that are this node's own state. They belong in the system config.
const NODE_SERVER_KEYS: &[&str] = &[
    "bind",
    "tls_cert",
    "tls_key",
    "h2c_bind",
    "internal_binds",
    "redirect_bind",
    "warm_on_start",
];
/// The fields of `RawSystemHealth`; change both together.
const NODE_HEALTH_KEYS: &[&str] = &["metrics_token_file", "metrics_require", "metrics_peers"];
/// Whole sections that are node state. They belong in the system config.
///
/// `RawSiteToml` has no `[node]` field, so a `[node]` in `site.toml` is
/// dropped without a word unless it is named here.
const NODE_SECTIONS: &[&str] = &["auth", "node"];
/// Keys that describe how the site's routes are served. They belong in
/// `site.toml`.
const SITE_SERVER_KEYS: &[&str] = &["backend_timeout_secs", "allowed_methods"];

/// Name every key that sits in the file it does not belong to.
///
/// Both files accept most of these keys and the system config wins, so a
/// misplaced key works and says nothing. Each message names the key and the
/// file it belongs in. The rule: `site.toml` holds state every node of the site
/// shares, and the system config holds this node's own state (binds,
/// certificates, credential paths, per-node security).
///
/// Returns nothing when both paths name one file (one-file mode, as a
/// development script runs) or when either file cannot be read or parsed, since
/// `load` reports those.
pub fn misplaced_keys(site_dir: &Path, system_config_path: &Path) -> Vec<String> {
    let site_toml_path = site_dir.join("site.toml");
    if is_one_file(&site_toml_path, system_config_path) {
        return Vec::new();
    }
    let read = |p: &Path| -> Option<toml::Value> {
        std::fs::read_to_string(p).ok()?.parse::<toml::Value>().ok()
    };
    let (Some(site), Some(system)) = (read(&site_toml_path), read(system_config_path)) else {
        return Vec::new();
    };

    let mut out = Vec::new();
    let in_site = |section: &str, key: &str| site.get(section).and_then(|t| t.get(key)).is_some();
    for key in NODE_SERVER_KEYS {
        if in_site("server", key) {
            out.push(node_state_message("server", Some(key), system_config_path));
        }
    }
    for section in NODE_SECTIONS {
        if site.get(section).is_some() {
            out.push(node_state_message(section, None, system_config_path));
        }
    }
    for key in NODE_HEALTH_KEYS {
        if in_site("health", key) {
            out.push(node_state_message("health", Some(key), system_config_path));
        }
    }
    for key in SITE_SERVER_KEYS {
        if system.get("server").and_then(|t| t.get(key)).is_some() {
            out.push(format!(
                "the system config sets [server].{key}, which is site-wide and belongs in site.toml"
            ));
        }
    }
    out
}

fn node_state_message(section: &str, key: Option<&str>, system_config_path: &Path) -> String {
    let name = match key {
        Some(k) => format!("[{section}].{k}"),
        None => format!("[{section}]"),
    };
    format!(
        "site.toml sets {name}, which is this node's own state and belongs in the system config {}",
        system_config_path.display()
    )
}

/// True when the system config names no `[node]`, so the node identity falls
/// back to `[site].name`.
pub fn node_falls_back(system_config_path: &Path) -> bool {
    std::fs::read_to_string(system_config_path)
        .ok()
        .and_then(|raw| raw.parse::<toml::Table>().ok())
        .is_some_and(|t| !t.contains_key("node"))
}

/// Warn that the node identity fell back to `[site].name`.
///
/// Called from `main` after logging is initialised, for the reason given on
/// [`warn_ignored_route_cache_keys`].
pub fn warn_node_fallback(system_config_path: &Path) {
    if node_falls_back(system_config_path) {
        warn!(
            file = %system_config_path.display(),
            "system config: no [node].name set, falling back to [site].name for node identity"
        );
    }
}

pub fn warn_system_config_extra_keys(system_config_path: &Path) {
    if let Ok(raw) = std::fs::read_to_string(system_config_path) {
        if let Ok(toml::Value::Table(tbl)) = raw.parse::<toml::Value>() {
            for key in tbl.keys() {
                if !matches!(key.as_str(), "server" | "node" | "auth" | "health") {
                    warn!(
                        key = %key,
                        file = %system_config_path.display(),
                        "system config: ignoring key other than [server], [node], [auth], [health]"
                    );
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

    /// `[[backend]] compresses` and what the edge does with it.
    ///
    /// Issue #8: the proxy is a cache, not a transformer, so it must not promise
    /// encodings a backend cannot produce. Every test written for that issue was
    /// in m6-core, on the backend's side of the contract; the edge's own reading
    /// of the flag had none, which is the half a visitor sees.
    ///
    /// Through the real `load`, so the key's parsing and its default are covered
    /// too rather than a hand-built struct that cannot drift from the TOML.
    #[test]
    fn compresses_is_read_from_the_config_and_defaults_to_true() {
        let dir = make_test_dir();
        write_file(
            dir.path(),
            "site.toml",
            r#"
[site]
name   = "Test"
domain = "test.example.com"

[server]
bind     = "127.0.0.1:8443"
tls_cert = "cert.pem"
tls_key  = "key.pem"

[[backend]]
name    = "says-yes"
sockets = "/run/m6/a-*.sock"
compresses = true

[[backend]]
name    = "says-no"
sockets = "/run/m6/b-*.sock"
compresses = false

# No `compresses` key at all: the default has to be true, because a missing
# Vary is a correctness bug and a redundant one only costs cache efficiency.
[[backend]]
name    = "says-nothing"
sockets = "/run/m6/c-*.sock"

[[route]]
path    = "/"
backend = "says-yes"
"#,
        );
        write_file(dir.path(), "cert.pem", "");
        write_file(dir.path(), "key.pem", "");
        write_file(
            dir.path(),
            "system.toml",
            "[server]\nbind = \"127.0.0.1:8443\"\ntls_cert = \"cert.pem\"\ntls_key = \"key.pem\"\n",
        );

        let cfg = load(dir.path(), &dir.path().join("system.toml")).expect("load");

        assert!(cfg.backend_compresses("says-yes"));
        assert!(!cfg.backend_compresses("says-no"));
        assert!(
            cfg.backend_compresses("says-nothing"),
            "a backend that does not mention `compresses` must be assumed to compress"
        );
        assert!(
            cfg.backend_compresses("never-configured"),
            "an unknown backend name must be assumed to compress: a missing Vary is a \
             correctness bug, a redundant one only costs cache efficiency"
        );
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
"#
        .to_string()
    }

    fn minimal_system_toml() -> String {
        r#"
[server]
bind     = "127.0.0.1:8443"
tls_cert = "cert.pem"
tls_key  = "key.pem"
"#
        .to_string()
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
        write_file(
            dir.path(),
            "site.toml",
            r#"
[site]
domain = "test.example.com"
[server]
bind = "0.0.0.0:443"
tls_cert = "cert.pem"
tls_key  = "key.pem"
[[backend]]
name = "b"
sockets = "/run/m6/*.sock"
"#,
        );
        write_file(
            dir.path(),
            "system.toml",
            "[server]\nbind = \"0.0.0.0:443\"\ntls_cert = \"cert.pem\"\ntls_key = \"key.pem\"\n",
        );
        let result = load(dir.path(), &dir.path().join("system.toml"));
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("name"));
    }

    #[test]
    fn test_missing_tls_cert_fails() {
        let dir = make_test_dir();
        write_file(dir.path(), "key.pem", "dummy");
        write_file(
            dir.path(),
            "site.toml",
            r#"
[site]
name   = "Test"
domain = "test.example.com"
[server]
bind     = "0.0.0.0:443"
tls_cert = "missing-cert.pem"
tls_key  = "key.pem"
"#,
        );
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
        write_file(
            dir.path(),
            "site.toml",
            r#"
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
"#,
        );
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
        write_file(
            dir.path(),
            "site.toml",
            r#"
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
"#,
        );
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
        write_file(
            dir.path(),
            "site.toml",
            r#"
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
"#,
        );
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
        write_file(
            dir.path(),
            "site.toml",
            r#"
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
"#,
        );
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
        write_file(
            dir.path(),
            "site.toml",
            r#"
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
"#,
        );
        write_file(
            dir.path(),
            "system.toml",
            r#"
[server]
bind = "0.0.0.0:443"
tls_cert = "cert.pem"
tls_key = "key.pem"
"#,
        );
        let cfg = load(dir.path(), &dir.path().join("system.toml")).unwrap();
        assert_eq!(cfg.server.bind, "0.0.0.0:443");
    }

    /// A :80 redirector terminates no TLS, so it must load with no certificate
    /// named at all — not merely with one that is allowed to point nowhere.
    /// This is what lets every node's redirect config be a two-line file, and
    /// what stops an expired cert from taking the plain-HTTP redirect down
    /// with it.
    #[test]
    fn test_redirect_mode_needs_no_tls() {
        let dir = make_test_dir();
        write_file(
            dir.path(),
            "site.toml",
            r#"
[site]
name   = "Test"
domain = "test.example.com"
[server]
redirect_bind = "0.0.0.0:80"
"#,
        );
        write_file(dir.path(), "system.toml", "");
        let cfg = load(dir.path(), &dir.path().join("system.toml")).unwrap();
        assert_eq!(cfg.server.redirect_bind.as_deref(), Some("0.0.0.0:80"));
        assert!(cfg.server.tls_cert.is_none());
        assert!(cfg.server.tls_key.is_none());
        // `bind` falls back to the redirect address rather than being a second
        // copy of it in the file.
        assert_eq!(cfg.server.bind, "0.0.0.0:80");
    }

    /// The other half of the contract: outside redirect mode the certificate
    /// is still mandatory, so making it optional cannot let a serving node
    /// boot without TLS.
    #[test]
    fn test_tls_still_required_when_not_redirecting() {
        let dir = make_test_dir();
        write_file(
            dir.path(),
            "site.toml",
            r#"
[site]
name   = "Test"
domain = "test.example.com"
[server]
bind = "0.0.0.0:443"
[[backend]]
name = "b"
sockets = "/run/m6/*.sock"
"#,
        );
        write_file(dir.path(), "system.toml", "");
        let err = load(dir.path(), &dir.path().join("system.toml"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("tls_cert"), "unexpected error: {err}");
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
        write_file(
            dir.path(),
            "site.toml",
            r#"
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
"#,
        );
        write_file(
            dir.path(),
            "system.toml",
            "[server]\nbind = \"127.0.0.1:8443\"\ntls_cert = \"cert.pem\"\ntls_key = \"key.pem\"\n",
        );
        let cfg = load(dir.path(), &dir.path().join("system.toml")).unwrap();
        assert_eq!(cfg.server.backend_timeout_secs, 60);
    }

    #[test]
    fn test_backend_timeout_system_wins() {
        let dir = make_test_dir();
        write_file(dir.path(), "cert.pem", "dummy");
        write_file(dir.path(), "key.pem", "dummy");
        write_file(
            dir.path(),
            "site.toml",
            r#"
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
"#,
        );
        write_file(
            dir.path(),
            "system.toml",
            r#"
[server]
bind = "127.0.0.1:8443"
tls_cert = "cert.pem"
tls_key = "key.pem"
backend_timeout_secs = 120
"#,
        );
        let cfg = load(dir.path(), &dir.path().join("system.toml")).unwrap();
        assert_eq!(cfg.server.backend_timeout_secs, 120);
    }

    // ── [server] internal_binds ─────────────────────────────────────────────

    /// Load the minimal site with `extra` appended to the system config's
    /// `[server]` table.
    fn load_with_system_server(extra: &str) -> anyhow::Result<Config> {
        let dir = make_test_dir();
        setup_minimal(dir.path());
        let mut sys = minimal_system_toml();
        sys.push_str(extra);
        write_file(dir.path(), "system.toml", &sys);
        load(dir.path(), &dir.path().join("system.toml"))
    }

    fn load_error(extra: &str) -> String {
        load_with_system_server(extra)
            .expect_err("load must refuse")
            .to_string()
    }

    #[test]
    fn no_internal_binds_without_h2c_loads_and_nothing_is_internal() {
        let cfg = load_with_system_server("").unwrap();
        assert!(cfg.server.internal_binds.is_empty());
        // A loopback bind is no longer internal by address.
        assert!(!cfg.server.is_internal("127.0.0.1:8443"));
    }

    #[test]
    fn an_h2c_bind_without_internal_binds_refuses_to_start() {
        let e = load_error("h2c_bind = \"127.0.0.1:18080\"\n");
        assert!(e.starts_with("config error:"), "{e}");
        assert!(e.contains("internal_binds = [\"127.0.0.1:18080\"]"), "{e}");
        assert!(e.contains("internal_binds = []"), "{e}");
    }

    #[test]
    fn the_migration_error_for_a_host_name_does_not_suggest_that_name() {
        let e = load_error("h2c_bind = \"localhost:8080\"\n");
        assert!(!e.contains("internal_binds = [\"localhost:8080\"]"), "{e}");
        assert!(e.contains("needs an IP address and port"), "{e}");
        assert!(e.contains("internal_binds = []"), "{e}");
    }

    #[test]
    fn the_migration_error_for_a_non_local_address_does_not_suggest_it() {
        for h2c in ["0.0.0.0:8080", "203.0.113.7:8080"] {
            let e = load_error(&format!("h2c_bind = \"{h2c}\"\n"));
            assert!(
                !e.contains(&format!("internal_binds = [\"{h2c}\"]")),
                "{h2c}: {e}"
            );
            assert!(e.contains("must be bound to a local address"), "{h2c}: {e}");
            assert!(e.contains("internal_binds = []"), "{h2c}: {e}");
        }
    }

    #[test]
    fn an_empty_internal_binds_accepts_an_undeclared_h2c() {
        let cfg = load_with_system_server("h2c_bind = \"127.0.0.1:18080\"\ninternal_binds = []\n")
            .unwrap();
        assert!(cfg.server.internal_binds.is_empty());
        assert!(!cfg.server.is_internal("127.0.0.1:18080"));
    }

    #[test]
    fn a_declared_h2c_bind_is_internal_and_the_tls_bind_is_not() {
        let cfg = load_with_system_server(
            "h2c_bind = \"127.0.0.1:18080\"\ninternal_binds = [\"127.0.0.1:18080\"]\n",
        )
        .unwrap();
        assert!(cfg.server.is_internal("127.0.0.1:18080"));
        assert!(!cfg.server.is_internal(&cfg.server.bind));
    }

    #[test]
    fn an_internal_binding_that_names_the_tls_bind_is_refused() {
        let e = load_error("internal_binds = [\"127.0.0.1:8443\"]\n");
        assert!(e.starts_with("config error:"), "{e}");
        assert!(
            e.contains("Only the h2c_bind listener can be internal"),
            "{e}"
        );
        assert!(e.contains("\"127.0.0.1:8443\""), "{e}");
    }

    #[test]
    fn an_internal_binding_must_be_an_ip_and_port() {
        for entry in ["localhost:8080", "10.0.0.1", "fdexample.com:80"] {
            let e = load_error(&format!("internal_binds = [\"{entry}\"]\n"));
            assert!(e.contains("is not an IP address and port"), "{entry}: {e}");
            assert!(e.contains(&format!("\"{entry}\"")), "{entry}: {e}");
        }
    }

    #[test]
    fn an_internal_binding_must_be_in_a_local_range() {
        // Same address as h2c_bind, so only the range check can refuse it.
        for entry in ["203.0.113.7:8080", "0.0.0.0:8080"] {
            let e = load_error(&format!(
                "h2c_bind = \"{entry}\"\ninternal_binds = [\"{entry}\"]\n"
            ));
            assert!(e.contains("is not in a local range"), "{entry}: {e}");
        }
    }

    #[test]
    fn an_internal_binding_must_name_the_h2c_listener() {
        let e = load_error("h2c_bind = \"10.0.0.1:8080\"\ninternal_binds = [\"10.0.0.9:8080\"]\n");
        assert!(e.contains("is not this server's h2c listener"), "{e}");
        assert!(e.contains("it must equal [server].h2c_bind"), "{e}");
    }

    #[test]
    fn ipv6_internal_bindings_match_as_socket_addresses() {
        let cfg = load_with_system_server(
            "h2c_bind = \"[::1]:8080\"\ninternal_binds = [\"[0:0:0:0:0:0:0:1]:8080\"]\n",
        )
        .unwrap();
        assert!(cfg.server.is_internal("[::1]:8080"));
        let cfg = load_with_system_server(
            "h2c_bind = \"[fd00::1]:80\"\ninternal_binds = [\"[fd00::1]:80\"]\n",
        )
        .unwrap();
        assert!(cfg.server.is_internal("[fd00::1]:80"));
    }

    #[test]
    fn system_internal_binds_replace_site_internal_binds() {
        let dir = make_test_dir();
        setup_minimal(dir.path());
        let mut site = minimal_site_toml();
        site = site.replace(
            "tls_key  = \"key.pem\"\n",
            "tls_key  = \"key.pem\"\ninternal_binds = []\n",
        );
        write_file(dir.path(), "site.toml", &site);
        let mut sys = minimal_system_toml();
        sys.push_str("h2c_bind = \"127.0.0.1:18080\"\ninternal_binds = [\"127.0.0.1:18080\"]\n");
        write_file(dir.path(), "system.toml", &sys);
        let cfg = load(dir.path(), &dir.path().join("system.toml")).unwrap();
        assert!(cfg.server.is_internal("127.0.0.1:18080"));
    }

    #[test]
    fn redirect_mode_ignores_internal_binds() {
        let dir = make_test_dir();
        write_file(
            dir.path(),
            "site.toml",
            r#"
[site]
name   = "Test"
domain = "test.example.com"
[server]
redirect_bind = "0.0.0.0:80"
internal_binds = ["203.0.113.7:80"]
"#,
        );
        write_file(dir.path(), "system.toml", "");
        let cfg = load(dir.path(), &dir.path().join("system.toml")).unwrap();
        assert_eq!(cfg.server.redirect_bind.as_deref(), Some("0.0.0.0:80"));
    }

    /// Load with `site_extra` appended to site.toml and `sys_extra` appended
    /// to system.toml. A verify key exists as `auth.pem` in the test directory.
    fn load_with(site_extra: &str, sys_extra: &str) -> (TempDir, anyhow::Result<Config>) {
        let dir = make_test_dir();
        setup_minimal(dir.path());
        write_file(dir.path(), "auth.pem", "dummy");
        let mut site = minimal_site_toml();
        site.push_str(&site_extra.replace("{DIR}", &dir.path().display().to_string()));
        write_file(dir.path(), "site.toml", &site);
        let mut sys = minimal_system_toml();
        sys.push_str(&sys_extra.replace("{DIR}", &dir.path().display().to_string()));
        write_file(dir.path(), "system.toml", &sys);
        let result = load(dir.path(), &dir.path().join("system.toml"));
        (dir, result)
    }

    #[test]
    fn auth_without_backend_loads() {
        let (_d, cfg) = load_with("\n[auth]\npublic_key = \"auth.pem\"\n", "");
        let cfg = cfg.unwrap();
        let auth = cfg.auth.expect("auth present");
        assert_eq!(auth.backend, None);
        assert_eq!(auth.public_key, "auth.pem");
    }

    #[test]
    fn auth_backend_when_given_must_be_declared() {
        let (_d, r) = load_with(
            "\n[auth]\nbackend = \"nope\"\npublic_key = \"auth.pem\"\n",
            "",
        );
        assert!(r
            .unwrap_err()
            .to_string()
            .contains("[auth].backend `nope` not in [[backend]]"));
        let (_d, ok) = load_with(
            "\n[auth]\nbackend = \"m6-html\"\npublic_key = \"auth.pem\"\n",
            "",
        );
        assert!(ok.is_ok());
    }

    #[test]
    fn auth_from_the_system_config_is_used() {
        let (_d, r) = load_with("", "\n[auth]\npublic_key = \"{DIR}/auth.pem\"\n");
        let auth = r.unwrap().auth.expect("auth present");
        assert!(auth.public_key.ends_with("/auth.pem"));
        assert_eq!(auth.backend, None);
    }

    #[test]
    fn auth_in_both_files_is_refused() {
        let (_d, r) = load_with(
            "\n[auth]\npublic_key = \"auth.pem\"\n",
            "\n[auth]\npublic_key = \"{DIR}/auth.pem\"\n",
        );
        assert_eq!(
            r.unwrap_err().to_string(),
            "config error: [auth] is declared in both site.toml and the system config; declare it in one"
        );
    }

    #[test]
    fn a_relative_public_key_in_the_system_config_resolves_against_the_site_dir() {
        let (_d, r) = load_with("", "\n[auth]\npublic_key = \"auth.pem\"\n");
        // load_with writes auth.pem into the site directory.
        let auth = r.unwrap().auth.expect("auth present");
        assert_eq!(auth.public_key, "auth.pem");
    }

    #[test]
    fn a_relative_public_key_in_the_system_config_must_exist_in_the_site_dir() {
        let (_d, r) = load_with("", "\n[auth]\npublic_key = \"missing.pem\"\n");
        let e = r.unwrap_err().to_string();
        assert!(
            e.contains("[auth].public_key file not found") && e.ends_with("/missing.pem"),
            "{e}"
        );
    }

    fn peers_error(entries: &str) -> String {
        let (_d, r) = load_with(&format!("\n[health]\nmetrics_peers = [{entries}]\n"), "");
        r.expect_err("must refuse").to_string()
    }

    #[test]
    fn metrics_peers_accepts_local_addresses() {
        let (_d, r) = load_with(
            "\n[health]\nmetrics_peers = [\"10.0.0.9\", \"127.0.0.1\", \"fd00::5\", \"100.64.0.1\"]\n",
            "",
        );
        let h = r.unwrap().health;
        let ips = h.metrics_peer_ips.expect("peers set");
        assert_eq!(ips.len(), 4);
        assert!(ips.contains(&"fd00::5".parse().unwrap()));
    }

    #[test]
    fn metrics_peers_are_stored_canonical() {
        let (_d, r) = load_with("\n[health]\nmetrics_peers = [\"::ffff:10.0.0.9\"]\n", "");
        let h = r.unwrap().health;
        assert_eq!(
            h.metrics_peer_ips,
            Some(vec!["10.0.0.9".parse::<std::net::IpAddr>().unwrap()])
        );
    }

    #[test]
    fn metrics_peers_refuses_names_ranges_and_ports() {
        for entry in ["localhost", "10.0.0.0/24", "10.0.0.9:443"] {
            let e = peers_error(&format!("\"{entry}\""));
            assert!(
                e.contains(&format!(
                    "metrics_peers entry \"{entry}\" is not an IP address"
                )),
                "{entry}: {e}"
            );
        }
    }

    #[test]
    fn metrics_peers_refuses_public_and_wildcard() {
        for entry in ["203.0.113.7", "0.0.0.0", "::"] {
            let e = peers_error(&format!("\"{entry}\""));
            assert!(e.contains("is not in a local range"), "{entry}: {e}");
        }
    }

    #[test]
    fn an_empty_metrics_peers_is_refused() {
        assert!(peers_error("").contains("metrics_peers is empty"));
    }

    #[test]
    fn metrics_require_needs_auth() {
        let (_d, r) = load_with("\n[health]\nmetrics_require = \"role:monitor\"\n", "");
        assert_eq!(
            r.unwrap_err().to_string(),
            "config error: [health].metrics_require needs [auth] with a public_key"
        );
    }

    #[test]
    fn metrics_require_must_name_a_role_or_group() {
        for bad in ["admin", "role:", "group:", "user:x"] {
            let (_d, r) = load_with(
                &format!(
                    "\n[auth]\npublic_key = \"auth.pem\"\n\n[health]\nmetrics_require = \"{bad}\"\n"
                ),
                "",
            );
            let e = r.unwrap_err().to_string();
            assert!(
                e.contains("must be \"role:<name>\" or \"group:<name>\""),
                "{bad}: {e}"
            );
        }
        let (_d, ok) = load_with(
            "\n[auth]\npublic_key = \"auth.pem\"\n\n[health]\nmetrics_require = \"group:ops\"\n",
            "",
        );
        assert!(ok.is_ok());
    }

    #[test]
    fn system_health_keys_win_over_site_health_keys() {
        let (_d, r) = load_with(
            "\n[auth]\npublic_key = \"auth.pem\"\n\n[health]\nmetrics_require = \"role:site\"\nmetrics_peers = [\"10.0.0.1\"]\n",
            "\n[health]\nmetrics_require = \"role:system\"\nmetrics_peers = [\"10.0.0.2\"]\n",
        );
        let h = r.unwrap().health;
        assert_eq!(h.metrics_require.as_deref(), Some("role:system"));
        assert_eq!(
            h.metrics_peer_ips,
            Some(vec!["10.0.0.2".parse::<std::net::IpAddr>().unwrap()])
        );
    }

    #[test]
    fn a_system_health_key_alone_keeps_the_other_from_the_site() {
        let (_d, r) = load_with(
            "\n[auth]\npublic_key = \"auth.pem\"\n\n[health]\nmetrics_require = \"role:site\"\n",
            "\n[health]\nmetrics_peers = [\"10.0.0.2\"]\n",
        );
        let h = r.unwrap().health;
        assert_eq!(h.metrics_require.as_deref(), Some("role:site"));
        assert_eq!(h.metrics_peer_ips.map(|p| p.len()), Some(1));
    }

    #[test]
    fn an_unknown_health_key_in_the_system_config_fails_to_parse() {
        let (_d, r) = load_with("", "\n[health]\nperf_path = \"/p\"\n");
        let e = format!("{:#}", r.unwrap_err());
        assert!(e.contains("perf_path"), "{e}");
    }

    #[test]
    fn health_defaults_have_no_require_and_no_peers() {
        let (_d, r) = load_with("", "");
        let h = r.unwrap().health;
        assert_eq!(h.metrics_require, None);
        assert_eq!(h.metrics_peers, None);
        assert_eq!(h.metrics_peer_ips, None);
    }

    #[test]
    fn one_file_given_as_both_is_read_once() {
        let dir = make_test_dir();
        setup_minimal(dir.path());
        write_file(dir.path(), "auth.pem", "dummy");
        let mut site = minimal_site_toml();
        site.push_str("\n[auth]\npublic_key = \"auth.pem\"\n\n[health]\nperf_path = \"/p\"\nmetrics_peers = [\"10.0.0.5\"]\n");
        write_file(dir.path(), "site.toml", &site);
        let same = dir.path().join("site.toml");
        let cfg = load(dir.path(), &same).expect("one file for both loads");
        assert_eq!(cfg.auth.expect("auth").public_key, "auth.pem");
        assert_eq!(cfg.health.perf_path, "/p");
        assert_eq!(cfg.health.metrics_peer_ips.map(|p| p.len()), Some(1));
    }

    // --- misplaced_keys and metrics_token_file placement ---

    #[test]
    fn metrics_token_file_in_the_system_config_wins() {
        let dir = make_test_dir();
        setup_minimal(dir.path());
        let site_token = dir.path().join("site-token");
        let sys_token = dir.path().join("sys-token");
        std::fs::write(&site_token, "from-site\n").unwrap();
        std::fs::write(&sys_token, "from-system\n").unwrap();
        let mut site = minimal_site_toml();
        site.push_str(&format!(
            "\n[health]\nmetrics_token_file = \"{}\"\n",
            site_token.display()
        ));
        write_file(dir.path(), "site.toml", &site);
        let mut sys = minimal_system_toml();
        sys.push_str(&format!(
            "\n[health]\nmetrics_token_file = \"{}\"\n",
            sys_token.display()
        ));
        write_file(dir.path(), "system.toml", &sys);
        let h = load(dir.path(), &dir.path().join("system.toml"))
            .unwrap()
            .health;
        assert_eq!(h.metrics_token_file.as_deref(), sys_token.to_str());
        assert_eq!(h.metrics_token.as_deref(), Some("from-system"));

        // Alone in the system config it is read too.
        write_file(dir.path(), "site.toml", &minimal_site_toml());
        let h = load(dir.path(), &dir.path().join("system.toml"))
            .unwrap()
            .health;
        assert_eq!(h.metrics_token.as_deref(), Some("from-system"));
    }

    #[test]
    fn a_relative_metrics_token_file_in_the_system_config_resolves_against_the_site_dir() {
        let dir = make_test_dir();
        setup_minimal(dir.path());
        std::fs::write(dir.path().join("token"), "from-site-dir\n").unwrap();
        write_file(dir.path(), "site.toml", &minimal_site_toml());
        let mut sys = minimal_system_toml();
        sys.push_str("\n[health]\nmetrics_token_file = \"token\"\n");
        write_file(dir.path(), "system.toml", &sys);
        let h = load(dir.path(), &dir.path().join("system.toml"))
            .unwrap()
            .health;
        assert_eq!(
            h.metrics_token_file.as_deref(),
            dir.path().join("token").to_str()
        );
        assert_eq!(h.metrics_token.as_deref(), Some("from-site-dir"));
    }

    /// Write `site` and `system` into a fresh directory and name the misplaced keys.
    fn misplaced(site: &str, system: &str) -> Vec<String> {
        let dir = make_test_dir();
        write_file(dir.path(), "site.toml", site);
        write_file(dir.path(), "system.toml", system);
        misplaced_keys(dir.path(), &dir.path().join("system.toml"))
    }

    #[test]
    fn misplaced_keys_names_a_bind_in_site_toml() {
        let m = misplaced(
            "[server]\nbind = \"0.0.0.0:443\"\n\n[auth]\npublic_key = \"k\"\n\n[health]\nmetrics_peers = [\"10.0.0.1\"]\n",
            "[server]\nbind = \"0.0.0.0:443\"\n",
        );
        assert_eq!(m.len(), 3, "{m:?}");
        assert!(m[0].starts_with("site.toml sets [server].bind, which is this node's own state"));
        assert!(m[0].contains("belongs in the system config "), "{}", m[0]);
        assert!(m.iter().any(|x| x.contains("sets [auth],")));
        assert!(m.iter().any(|x| x.contains("sets [health].metrics_peers,")));
    }

    #[test]
    fn misplaced_keys_names_allowed_methods_in_the_system_config() {
        let m = misplaced(
            "",
            "[server]\nallowed_methods = [\"GET\"]\nbackend_timeout_secs = 5\nbind = \"0.0.0.0:443\"\n",
        );
        assert_eq!(m.len(), 2, "{m:?}");
        assert!(m
            .iter()
            .all(|x| x.contains("is site-wide and belongs in site.toml")));
        assert!(m.iter().any(|x| x.contains("[server].allowed_methods")));
        assert!(m
            .iter()
            .any(|x| x.contains("[server].backend_timeout_secs")));
    }

    #[test]
    fn misplaced_keys_is_silent_in_one_file_mode() {
        let dir = make_test_dir();
        write_file(
            dir.path(),
            "site.toml",
            "[server]\nbind = \"0.0.0.0:443\"\nallowed_methods = [\"GET\"]\n\n[auth]\npublic_key = \"k\"\n",
        );
        let same = dir.path().join("site.toml");
        assert!(misplaced_keys(dir.path(), &same).is_empty());
    }

    #[test]
    fn misplaced_keys_is_silent_for_a_clean_pair() {
        let m = misplaced(
            "[server]\nallowed_methods = [\"GET\"]\nbackend_timeout_secs = 5\n\n[health]\nperf_path = \"/p\"\n",
            "[server]\nbind = \"0.0.0.0:443\"\n\n[auth]\npublic_key = \"/k\"\n\n[health]\nmetrics_token_file = \"/t\"\n",
        );
        assert!(m.is_empty(), "{m:?}");
    }

    #[test]
    fn misplaced_keys_names_a_node_section_in_site_toml() {
        let m = misplaced("[node]\nname = \"edge-a\"\n", "[node]\nname = \"edge-a\"\n");
        assert_eq!(m.len(), 1, "{m:?}");
        assert!(m[0].starts_with("site.toml sets [node],"), "{}", m[0]);
    }

    #[test]
    fn the_node_fallback_is_detected_for_the_caller_to_warn_about() {
        let dir = make_test_dir();
        let with = dir.path().join("with.toml");
        let without = dir.path().join("without.toml");
        write_file(dir.path(), "with.toml", "[node]\nname = \"edge-a\"\n");
        write_file(dir.path(), "without.toml", "[server]\nbind = \"x\"\n");
        assert!(!node_falls_back(&with));
        assert!(node_falls_back(&without));
    }
}
