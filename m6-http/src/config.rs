/// Config loading: site.toml + system config merging and validation.
use std::path::{Path, PathBuf};

use anyhow::Context;
use m6_core::config::secret_path;
use m6_core::site::Methods;
use serde::{Deserialize, Serialize};

/// The one `[log]` type, owned by m6-core. Re-exported so this crate's `Config`
/// uses the same type every other service uses, and `[log]` in site.toml is
/// parsed by the same function as `[log]` in a service's own config.
pub use m6_core::config::LogConfig;

/// Full merged configuration for m6-http.
#[derive(Debug, Clone, Serialize)]
pub struct Config {
    pub site: SiteConfig,
    pub node: NodeConfig,
    pub server: ServerConfig,
    pub log: LogConfig,
    pub analytics: AnalyticsConfig,
    /// How often the periodic stats line is written. Site state: the line is
    /// the site's record of what its node did.
    pub stats: StatsConfig,
    pub health: HealthConfig,
    pub rate_limit: RateLimitConfig,
    pub errors: ErrorsConfig,
    pub security: SecurityConfig,
    pub auth: Option<AuthConfig>,
    /// Where a source node's generation ID comes from, and who may read or
    /// advance it. `None` means this node has no `/genid`.
    pub genid: Option<GenidConfig>,
    /// A cache node's disk cache directory. `None` means the node keeps its
    /// cache in memory only.
    pub cache_dir: Option<PathBuf>,
    /// The `[limits]` table: the start and maximum of each buffer and queue.
    /// Node state, so it is read from the system config in two-file mode.
    #[serde(skip)]
    pub limits: m6_core::limits::Limits,
    pub backends: Vec<BackendConfig>,
    pub routes: Vec<RouteConfig>,
    pub route_groups: Vec<RouteGroupConfig>,
    /// Directory where site.toml lives.
    pub site_dir: PathBuf,
}

/// The node's `[genid]` table.
///
/// Two shapes, one table. A node with `upstream` is a cache node that follows
/// another node's ID. A node without one is the source that owns the ID, and
/// the claims say who may read or advance it.
///
/// On the source both claims are set or neither: one alone would leave one half
/// of the endpoint unguarded. Both absent leaves `/genid` answering 404 to
/// everyone, which is how a node says it does not serve a generation ID.
#[derive(Debug, Clone, Serialize)]
pub struct GenidConfig {
    /// `role:<name>` or `group:<name>` a `GET /genid` needs. Source only.
    pub read_require: Option<String>,
    /// The same for `POST /genid`, which advances the generation. Source only.
    pub write_require: Option<String>,
    /// The `[[backend]]` a cache node follows. Present makes this node a cache
    /// node, and the backend must have a `url`.
    pub upstream: Option<String>,
    /// The bearer token a heartbeat sends, read from `token_file` at load.
    ///
    /// `serde(skip)` in both directions: it must never be printed by
    /// `--dump-config`, and it must not be settable inline from TOML.
    #[serde(skip)]
    pub token: Option<String>,
    /// How long between heartbeats. Cache node only, default 10 s.
    #[serde(rename = "heartbeat_secs", serialize_with = "serialize_secs")]
    pub heartbeat: std::time::Duration,
}

/// A `Duration` as the whole seconds the config key spells.
fn serialize_secs<S>(d: &std::time::Duration, serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    serializer.serialize_u64(d.as_secs())
}

impl Default for GenidConfig {
    fn default() -> Self {
        GenidConfig {
            read_require: None,
            write_require: None,
            upstream: None,
            token: None,
            heartbeat: std::time::Duration::from_secs(crate::genid::DEFAULT_HEARTBEAT_SECS),
        }
    }
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
#[serde(deny_unknown_fields)]
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
    /// Most requests served on one HTTP/1.1 keep-alive connection before it is
    /// closed. Default: 100, `DEFAULT_MAX_REQUESTS_PER_CONN`. 0 is rejected by
    /// `load`. Read at startup; a change needs a restart.
    ///
    /// Legal, and expensive: closing a connection makes the client pay a new
    /// TCP and TLS handshake. m6-http talks HTTP/1.1 to its own backends, so
    /// every cache miss pays that cost on a fresh connection. A cap rather
    /// than unlimited so a connection cannot be held forever. At 100 requests
    /// a browser's whole page load fits in one connection and a long-running
    /// client still cycles. A client that makes many requests on one
    /// connection, such as a benchmark, wants a higher value.
    #[serde(default = "default_max_requests_per_connection")]
    pub max_requests_per_connection: u32,
    /// Let another m6-http process bind the same address, by setting
    /// `SO_REUSEPORT` before `bind` on the TLS listener and the QUIC UDP socket
    /// of `bind`. Default false, so an accidental second instance still fails
    /// at bind instead of quietly sharing traffic.
    ///
    /// True routes both sockets through `m6_platform::net::listen_shared`, so
    /// they are bound in one call and always share the public 443 port. No
    /// other socket is shared: the redirect listener on port 80, the internal
    /// h2c listener and every backend socket keep their own binding. The
    /// health endpoints are served on that shared listener, so a probe reaches
    /// one process. Linux spreads a new connection across the sockets by a
    /// hash of its 4-tuple and macOS does not, so the split is not even and no
    /// process is guaranteed traffic. A QUIC client that changes address may
    /// land on another process. Read at start, and a change needs a restart.
    #[serde(default)]
    pub reuse_port: bool,
    /// How many m6-http processes share the port, with `reuse_port`. On Linux
    /// it makes each client address always reach the same process: the kernel
    /// is given a socket filter that sends (a hash of the client's source
    /// address) mod this number to one socket, for TCP and for QUIC, so the
    /// per-client rate limit is exact. Every process on the port must set the
    /// same number. Unset, Linux chooses by a hash of the whole connection and
    /// one client can reach several processes. While a process is stopped,
    /// some clients move to another until it returns. macOS has no such
    /// filter, ignores the number, and always gives the last-bound process
    /// every client. Read at start, and a change needs a restart.
    #[serde(default)]
    pub reuse_port_processes: Option<std::num::NonZeroU32>,
    /// This process's number among the m6-http processes of one node, 0 to N-1
    /// (N is `reuse_port_processes` when set). `/health` and `/perf` report it
    /// as `process_index`, so a monitor can tell which process answered. With
    /// `status_bind` it also picks this process's status port. Read at start,
    /// and a change needs a restart.
    #[serde(default)]
    pub process_index: Option<u32>,
    /// `"<address>:<base port>"`: where this process answers `/health`,
    /// `/perf` and `/traffic` for a monitor, as a TLS listener of its own on
    /// the base port plus `process_index`. It answers those three paths and
    /// 404s every other, under the same `[health]` gate as the shared port.
    /// Needs `process_index`. It is not part of the `reuse_port` group, so
    /// nothing steers to it. Bind it to the node's tunnel address, never a
    /// public one. Unset means no status listener. Read at start, and a change
    /// needs a restart.
    #[serde(default)]
    pub status_bind: Option<String>,
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
    /// The peers the internal h2c listener accepts, canonical. Filled by
    /// `load` from `[server] internal_peers`; empty when no listener is
    /// internal. Read at startup; a change needs a restart.
    #[serde(default)]
    pub internal_peers: Vec<std::net::IpAddr>,
    /// Run this process as a plain-HTTP redirector on this address instead of
    /// a normal server: every request is answered 301 to `https://<Host><path>`.
    ///
    /// Meant for a **second m6-http process** alongside the real one, not for
    /// the `:443` instance to also listen here. Different port, different
    /// failure domain — a slow client on :80 cannot stall TLS serving because
    /// it is not sharing that process's event loop. In this mode nothing else
    /// in the config is read: no TLS, no QUIC, no backends, no routes, no
    /// cache, so `tls_cert`/`tls_key` may point nowhere. The unmatched-host
    /// policy is the one exception, because a redirector has to know which
    /// names it serves and where the rest go.
    #[serde(default)]
    pub redirect_bind: Option<String>,
    /// The host names this node serves. Empty by default.
    ///
    /// Matched against a request's `Host` (or HTTP/2 and HTTP/3 `:authority`)
    /// with any port stripped and ASCII case ignored. The list is node state:
    /// a node of a fleet can serve a different name from its neighbour, and a
    /// site's `[site] domain` is shared by every node and is not enough.
    ///
    /// The list alone changes nothing. It takes effect with
    /// `redirect_unmatched_to`.
    #[serde(default)]
    pub served_hosts: Vec<String>,
    /// Where a request goes when its Host is not in `served_hosts`: a bare
    /// host name, for example `example.com`. Unset by default, which turns the
    /// policy off.
    ///
    /// When set, a request outside `served_hosts`, including one with no Host,
    /// is answered 301 to `https://<redirect_unmatched_to><path>`, with the
    /// query kept, before the cache lookup and before routing. That position
    /// matters: the cache key has no host component, so a warm entry for a
    /// served name would otherwise answer a request for a name the node does
    /// not own. The internal h2c listener is excluded.
    ///
    /// Refused at load when `served_hosts` is empty. The combination would
    /// redirect every request, including the node's own name, which would take
    /// the deploy's checks and the fleet monitor off the node.
    #[serde(default)]
    pub redirect_unmatched_to: Option<String>,
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
    /// Pin every page of the process in RAM as it is faulted, after the heap is
    /// reserved and before serving (`mlockall` with `MCL_ONFAULT`), so a hot
    /// page is never a page fault and the kernel never swaps the cache out.
    /// Default true. Memory the process has reserved and not touched is not
    /// resident and not pinned: the pin follows the pages the load touches, so
    /// a warm process holds what its cache holds rather than every mapping
    /// mimalloc has reserved. It needs `RLIMIT_MEMLOCK` unlimited, which an
    /// ordinary account's 8 MiB is not: production sets
    /// `LimitMEMLOCK=infinity` in the systemd unit, and a developer on Linux
    /// raises the limit for the account (`docs/m6-testing.md`). When the limit
    /// or the kernel refuses, m6-http stops at start with exit 2 and an error
    /// that names the limit and the fix; it never carries on unpinned when
    /// pinning was asked for. Set false to run unpinned. A no-op on macOS. Read
    /// at start, and a change needs a restart.
    pub lock_memory: bool,
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
    /// is a cross-site tracking vector and a content site has no use for it.
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

    /// True when this node serves `host`, comparing with any port stripped and
    /// ASCII case ignored. An empty Host is never served.
    pub fn serves_host(&self, host: &str) -> bool {
        let host = host.split(':').next().unwrap_or(host);
        !host.is_empty()
            && self
                .served_hosts
                .iter()
                .any(|s| s.eq_ignore_ascii_case(host))
    }

    /// The 301 target for a request outside `served_hosts`, or `None` when the
    /// policy is off, the Host is served, or the path or query cannot be
    /// written into a `Location`.
    ///
    /// The target is built from the configured name, never from the request's
    /// Host. Echoing a request's own bytes back into `Location` is how open
    /// redirects and header injection get built.
    pub fn unmatched_host_redirect(
        &self,
        host: Option<&str>,
        path: &str,
        query: Option<&str>,
    ) -> Option<String> {
        let target = self.redirect_unmatched_to.as_deref()?;
        if host.is_some_and(|h| self.serves_host(h)) {
            return None;
        }
        let unsafe_byte = |s: &str| s.bytes().any(|b| b == b'\r' || b == b'\n' || b == 0);
        if unsafe_byte(path) {
            return None;
        }
        match query {
            Some(q) if unsafe_byte(q) => None,
            Some(q) => Some(format!("https://{target}{path}?{q}")),
            None => Some(format!("https://{target}{path}")),
        }
    }
}

fn default_allowed_methods() -> Vec<String> {
    vec!["GET".to_string(), "HEAD".to_string(), "POST".to_string()]
}

fn default_backend_timeout_secs() -> u64 {
    30
}

/// A string a `Host` header can plausibly carry as a bare name.
///
/// It is deliberately a syntax check, not a DNS check: a name is compared with
/// a request's Host, which never resolves here. The characters refused are the
/// ones that cannot match a Host or cannot be written into a `Location`.
fn is_host_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 253
        && !s
            .bytes()
            .any(|b| b.is_ascii_whitespace() || b.is_ascii_control())
        && !s.contains(['/', '\\', ':', '*', '@'])
}

/// `[server] served_hosts`: lowercase every entry once, so the stored list
/// is canonical, and refuse anything that could not match a Host.
fn validate_served_hosts(hosts: Vec<String>) -> anyhow::Result<Vec<String>> {
    let mut out = Vec::with_capacity(hosts.len());
    for host in hosts {
        let host = host.trim().to_ascii_lowercase();
        if !is_host_name(&host) {
            anyhow::bail!(
                "config error: [server].served_hosts entry {host:?} is not a bare host name. Drop any scheme, port, path, whitespace, control byte or wildcard"
            );
        }
        out.push(host);
    }
    Ok(out)
}

/// `[server] redirect_unmatched_to`: a bare host name, lowercased. An address
/// is refused because the target is the second hop of the redirect, and no
/// public certificate authority issues for an address.
fn validate_redirect_unmatched_to(target: Option<String>) -> anyhow::Result<Option<String>> {
    let Some(target) = target else {
        return Ok(None);
    };
    let target = target.trim().to_ascii_lowercase();
    if target.is_empty() {
        anyhow::bail!(
            "config error: [server].redirect_unmatched_to is empty. Give the canonical host name, for example example.com"
        );
    }
    if !is_host_name(&target) {
        anyhow::bail!(
            "config error: [server].redirect_unmatched_to {target:?} is not a bare host name. It is written into a Location header, so it may not carry a scheme, a port, a path, whitespace, a control byte or a wildcard"
        );
    }
    if target.parse::<std::net::IpAddr>().is_ok() {
        anyhow::bail!(
            "config error: [server].redirect_unmatched_to {target:?} is an IP address. No public certificate authority issues for an address, so the canonical target is a host name"
        );
    }
    Ok(Some(target))
}

/// The address a process's status listener binds: `status_bind`'s address with
/// its port raised by `process_index`. Checked by `load`, so a bad value is
/// exit 2 and never reaches the bind.
pub fn status_addr(status_bind: &str, process_index: u32) -> anyhow::Result<std::net::SocketAddr> {
    let mut addr = crate::http11::resolve_bind(status_bind)?;
    let port = u32::from(addr.port()) + process_index;
    let port = u16::try_from(port).map_err(|_| {
        anyhow::anyhow!(
            "config error: [server].status_bind {status_bind:?} plus process_index {process_index} is port {port}, past 65535"
        )
    })?;
    addr.set_port(port);
    Ok(addr)
}

/// `[server]` `process_index` and `status_bind`: a status listener needs an
/// index, and the index must be one of the `reuse_port_processes` when that is
/// set.
fn validate_status(
    process_index: Option<u32>,
    status_bind: Option<String>,
    processes: Option<std::num::NonZeroU32>,
) -> anyhow::Result<(Option<u32>, Option<String>)> {
    if let (Some(i), Some(n)) = (process_index, processes) {
        if i >= n.get() {
            anyhow::bail!(
                "config error: [server].process_index {i} is not below [server].reuse_port_processes {n}: the indexes run 0 to {}",
                n.get() - 1
            );
        }
    }
    if let Some(bind) = &status_bind {
        let Some(i) = process_index else {
            anyhow::bail!(
                "config error: [server].status_bind needs [server].process_index, which picks this process's port"
            );
        };
        status_addr(bind, i)?;
    }
    Ok((process_index, status_bind))
}

fn default_max_requests_per_connection() -> u32 {
    crate::http11::DEFAULT_MAX_REQUESTS_PER_CONN
}

/// Per-request traffic logging (session cookie, referrer, UA, cache state,
/// node identity, ...) — separate from operational `[log]` above. Written to
/// its own file through `m6_core::log`, always JSON.
///
/// Rotate by renaming the file and sending SIGUSR1: the process reopens the path.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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

/// The `[stats]` table: how often the periodic metrics line is written.
///
/// One key, because the line carries the whole `/perf` document at one
/// interval. A node that logs it more often writes more bytes, and the line's
/// size is bounded by `[limits.log_buffer]`, not by this.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StatsConfig {
    /// Seconds between two metrics lines. Ten matches the window the line's
    /// own per-window figures describe, and the default it has always had.
    #[serde(default = "default_stats_interval_s")]
    pub interval_s: u64,
}

fn default_stats_interval_s() -> u64 {
    DEFAULT_STATS_INTERVAL_S
}

/// Ten seconds: the interval m6-http emitted its stats line at before the key
/// existed, and the window those per-window figures describe.
pub const DEFAULT_STATS_INTERVAL_S: u64 = 10;

impl Default for StatsConfig {
    fn default() -> Self {
        StatsConfig {
            interval_s: default_stats_interval_s(),
        }
    }
}

/// The per-node health endpoint (see `crate::health`).
///
/// Answered inside m6-http ahead of routing, the cache and every backend, so
/// it costs a JSON serialisation rather than a page render.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HealthConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Configurable rather than hardcoded so a site that legitimately wants
    /// to serve its own `/health` page can move ours out of the way instead
    /// of being silently shadowed by it.
    #[serde(default = "default_health_path")]
    pub path: String,
    /// Path for the metrics endpoint. Separate from `path` on purpose: the
    /// health check must stay constant-cost, and percentiles are not.
    #[serde(default = "default_perf_path")]
    pub perf_path: String,
    /// Where the traffic summary is served. Same token as `/perf`.
    ///
    /// A third path rather than more fields on `/perf`, for the reason that
    /// split `/health` from `/perf` in the first place: the cost profiles
    /// differ. `/perf` sorts a reservoir in memory; this builds the report
    /// from the window this process keeps of its own logging, which allocates
    /// a record per retained event on a cache miss. Putting the expensive one
    /// behind a conditional on a cheaper path leaves the expensive branch one
    /// misconfigured header away from being taken on every request, so each
    /// answer's cost stays a property of its path. `docs/m6-http.md` describes
    /// the window.
    #[serde(default = "default_traffic_path")]
    pub traffic_path: String,
    /// How long a traffic summary is reused before it is rebuilt from the
    /// window.
    ///
    /// Without this a monitor polling every 30 seconds makes every node
    /// rebuild its report every 30 seconds, forever, to produce an answer
    /// that changes slowly. The check that consumes this runs hourly.
    #[serde(default = "default_traffic_cache_s")]
    pub traffic_cache_s: u64,
    /// `role:<name>` or `group:<name>`. When set, `/perf` and `/traffic` need
    /// a credential for an identity that carries this claim, checked by
    /// `POST /check` exactly as a route `require` is: 401 for no or a refused
    /// credential, 403 for the wrong claim, and 503 for a checker that did
    /// not answer.
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
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
pub struct AuthConfig {
    /// The `[[backend]]` whose members answer `POST /check`, with `sockets`.
    /// Its members are probed once at start.
    pub backend: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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

/// See [`BackendConfig::compresses`]. True because a backend built on m6-core compresses by default.
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
#[serde(deny_unknown_fields)]
pub struct RouteConfig {
    pub path: String,
    pub backend: String,
    /// Auth requirement e.g. "group:editors" or "role:admin"
    pub require: Option<String>,
    /// A `Cache-Control` value that replaces the one the backend set, on every
    /// reply this route gets. It is the route's word over the handler's default,
    /// so a handler that builds its own header, such as m6-file, can still be
    /// told `no-store` for one path. Absent leaves the backend's header alone.
    pub cache: Option<String>,
    /// The methods this route answers, or every method when absent.
    ///
    /// A route with a list answers exactly those methods: a request with
    /// another gets 405 with `Allow`, before the cache or a backend is
    /// touched (RFC 9110 15.5.6). `GET` also answers `HEAD`.
    ///
    /// Validated when the route table is built, so an unknown name stops the
    /// start with exit 2 rather than becoming a request-time 405.
    pub methods: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteGroupConfig {
    /// Glob pattern relative to site dir
    pub glob: String,
    /// URL path template with {stem} placeholder
    pub path: String,
    pub backend: String,
    pub require: Option<String>,
    /// See `RouteConfig::cache`.
    pub cache: Option<String>,
    /// See `RouteConfig::methods`.
    pub methods: Option<Vec<String>>,
}

/// A route's `cache` becomes a response header, so it must be a value a header
/// can carry. An empty one would send a `Cache-Control` that says nothing, and a
/// control character would end the header early.
fn check_route_cache(cache: Option<&str>, what: &str) -> anyhow::Result<()> {
    let Some(value) = cache else { return Ok(()) };
    if value.trim().is_empty() || value.bytes().any(|b| b.is_ascii_control()) {
        anyhow::bail!(
            "config error: {what}: cache must be a Cache-Control value such as \"no-store\", with no control characters"
        );
    }
    Ok(())
}

/// The methods a `[[route]]` or `[[route_group]]` answers, or every method
/// when it lists none.
///
/// Names are trimmed and upper-cased once at load, matching
/// `[server] allowed_methods`, so the request-time comparison is a fixed set
/// and a config typo is an exit 2 rather than a 405 on every request.
pub(crate) fn route_methods(names: Option<&[String]>, origin: &str) -> anyhow::Result<Methods> {
    let Some(names) = names else {
        return Ok(Methods::ANY);
    };
    anyhow::ensure!(
        !names.is_empty(),
        "config error: {origin} has an empty methods list; leave `methods` out to answer every method"
    );
    let upper: Vec<String> = names
        .iter()
        .map(|name| name.trim().to_ascii_uppercase())
        .collect();
    Methods::from_names(&upper).map_err(|unknown| {
        anyhow::anyhow!(
            "config error: {origin} lists the unknown method \"{unknown}\"; \
             the supported methods are GET (which also answers HEAD), POST, PUT, PATCH and DELETE"
        )
    })
}

// ── Raw serde types for parsing site.toml ──────────────────────────────────

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSiteToml {
    site: Option<RawSiteSection>,
    server: Option<RawServerSection>,
    node: Option<NodeConfig>,
    log: Option<toml::Value>,
    analytics: Option<AnalyticsConfig>,
    stats: Option<StatsConfig>,
    health: Option<HealthConfig>,
    rate_limit: Option<RateLimitConfig>,
    errors: Option<ErrorsConfig>,
    security: Option<SecurityConfig>,
    auth: Option<AuthConfig>,
    genid: Option<RawGenid>,
    /// The disk cache directory. Node state, so the table is refused in
    /// `site.toml` in two-file mode.
    cache: Option<RawCache>,
    /// The named limits. Node state, so the table is refused in `site.toml` in
    /// two-file mode.
    limits: Option<toml::Value>,
    #[serde(rename = "backend", default)]
    backends: Vec<BackendConfig>,
    #[serde(rename = "route", default)]
    routes: Vec<RouteConfig>,
    #[serde(rename = "route_group", default)]
    route_groups: Vec<RouteGroupConfig>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSiteSection {
    name: Option<String>,
    domain: Option<String>,
    redirect_www: Option<bool>,
    describedby: Option<String>,
}

#[derive(Debug, Deserialize, Default, Clone)]
#[serde(deny_unknown_fields)]
struct RawServerSection {
    bind: Option<String>,
    warm_on_start: Option<bool>,
    lock_memory: Option<bool>,
    tls_cert: Option<String>,
    tls_key: Option<String>,
    backend_timeout_secs: Option<u64>,
    max_requests_per_connection: Option<u32>,
    reuse_port: Option<bool>,
    reuse_port_processes: Option<std::num::NonZeroU32>,
    process_index: Option<u32>,
    status_bind: Option<String>,
    h2c_bind: Option<String>,
    internal_binds: Option<Vec<String>>,
    internal_peers: Option<Vec<String>>,
    redirect_bind: Option<String>,
    served_hosts: Option<Vec<String>>,
    redirect_unmatched_to: Option<String>,
    allowed_methods: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSystemToml {
    server: Option<RawServerSection>,
    node: Option<NodeConfig>,
    /// `[auth]` lives here, so the verify key's path is owned by the same
    /// file as the node's other credentials.
    auth: Option<AuthConfig>,
    /// Only the access keys: a node's own credentials and peers belong to the
    /// node, not to the site that is shipped to every node.
    health: Option<RawSystemHealth>,
    /// The generation-ID endpoint. Node state: a site's content ships to every
    /// node, its publish claims do not.
    genid: Option<RawGenid>,
    /// The disk cache directory. Node state, so it belongs beside the node's
    /// credentials rather than in the site that ships to every node.
    cache: Option<RawCache>,
    /// The named limits: each buffer's start and maximum.
    limits: Option<toml::Value>,
}

/// The `[cache]` keys. A cache node may point `dir` at a directory it owns.
#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawCache {
    dir: Option<String>,
}

/// The `[genid]` keys. A source sets the two claims. A cache node sets
/// `upstream` and `token_file`, and may set `heartbeat_secs`.
#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawGenid {
    read_require: Option<String>,
    write_require: Option<String>,
    upstream: Option<String>,
    token_file: Option<String>,
    heartbeat_secs: Option<u64>,
}

/// The `[health]` keys the system config may carry. Anything else fails to
/// parse, so a misplaced key is loud instead of ignored.
/// `NODE_HEALTH_KEYS` lists the same keys for `misplaced_keys`; change both together.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSystemHealth {
    metrics_require: Option<String>,
    metrics_peers: Option<Vec<String>>,
}

/// Parse a list of peer addresses into canonical form.
///
/// Only an IP literal in a local range is accepted. A name is never resolved,
/// a range is not a peer, and a public or wildcard address could never be a
/// private peer, so each is a load error. `key` names the setting in the
/// messages, `role` says what the entry is, and `empty_hint` says what to do
/// about an empty list.
fn parse_local_peers(
    key: &str,
    role: &str,
    empty_hint: &str,
    peers: &[String],
) -> anyhow::Result<Vec<std::net::IpAddr>> {
    if peers.is_empty() {
        anyhow::bail!("config error: {key} is empty; {empty_hint}");
    }
    let mut ips = Vec::with_capacity(peers.len());
    for e in peers {
        let ip: std::net::IpAddr = e.parse().map_err(|_| {
            anyhow::anyhow!(
                "config error: {key} entry \"{e}\" is not an IP address such as \"10.0.0.5\" or \"fd00::5\"; names are never resolved and ranges are not accepted"
            )
        })?;
        if !m6_core::net::is_private_ip(ip) {
            anyhow::bail!(
                "config error: {key} entry \"{e}\" is not in a local range; {role} must be a loopback, private (RFC 1918), link-local, shared (100.64.0.0/10) or IPv6 unique-local address"
            );
        }
        ips.push(ip.to_canonical());
    }
    Ok(ips)
}

/// Parse `[health] metrics_peers` into canonical addresses.
fn validate_metrics_peers(
    peers: Option<&[String]>,
) -> anyhow::Result<Option<Vec<std::net::IpAddr>>> {
    let Some(peers) = peers else {
        return Ok(None);
    };
    parse_local_peers(
        "[health].metrics_peers",
        "a metrics peer",
        "list the peers allowed to read /perf and /traffic, or remove the key",
        peers,
    )
    .map(Some)
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

/// Check `[server] internal_peers` against the listeners that are internal.
///
/// A listener that is internal believes the client address its peer forwards,
/// so it must say which peers it accepts. The key is required then, and
/// refused when no listener is internal because it would do nothing.
/// `internal_binds` is already validated, so a non-empty list names `h2c_bind`.
fn validate_internal_peers(
    internal_peers: Option<&[String]>,
    internal_binds: &[String],
) -> anyhow::Result<Vec<std::net::IpAddr>> {
    match (internal_binds.first(), internal_peers) {
        (None, None) => Ok(Vec::new()),
        (None, Some(_)) => anyhow::bail!(
            "config error: [server].internal_peers is set but no listener is internal; remove it, or name the h2c listener in [server].internal_binds"
        ),
        (Some(b), None) => anyhow::bail!(
            "config error: [server].internal_binds names \"{b}\" but [server].internal_peers is not set. An internal listener believes the client address a peer forwards, so it accepts only the peers it names. Add internal_peers = [\"<peer IP>\", ...] with the address each cache node connects from; any other peer is refused"
        ),
        (Some(b), Some(peers)) => {
            let parsed = parse_local_peers(
                "[server].internal_peers",
                "an internal peer",
                "list the peers allowed to connect to the internal h2c listener, or set internal_binds = [] to make the listener public",
                peers,
            )?;
            // A connection arrives from the bind's own IP family, and an
            // IPv4-mapped bind is IPv4. A list with no peer of that family
            // would refuse every connection.
            let bind_ip = b.parse::<std::net::SocketAddr>()?.ip().to_canonical();
            if !parsed.iter().any(|p| p.is_ipv4() == bind_ip.is_ipv4()) {
                let (bind_family, peer_family) = if bind_ip.is_ipv4() {
                    ("IPv4", "IPv6")
                } else {
                    ("IPv6", "IPv4")
                };
                anyhow::bail!(
                    "config error: [server].internal_binds \"{b}\" is {bind_family} but every [server].internal_peers entry is {peer_family}, so every connection would be refused. Add an internal_peers entry in the {bind_family} address each cache node connects from"
                );
            }
            Ok(parsed)
        }
    }
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
    let site_table: toml::Table = toml::from_str(&site_raw)
        .map_err(|e| anyhow::anyhow!("parsing {}: {e}", site_toml_path.display()))?;

    let system_raw = std::fs::read_to_string(system_config_path)
        .with_context(|| format!("reading {}", system_config_path.display()))?;
    let system_table: toml::Table = toml::from_str(&system_raw)
        .map_err(|e| anyhow::anyhow!("parsing {}: {e}", system_config_path.display()))?;

    let one_file = is_one_file(&site_toml_path, system_config_path);

    // One source per setting: a key in the file it does not belong to stops the
    // start. In one-file mode every key is read from the one file, so the check
    // does not run.
    if !one_file {
        let misplaced = misplaced_keys(&site_table, &system_table, system_config_path);
        if !misplaced.is_empty() {
            anyhow::bail!("config error: {}", misplaced.join("\n"));
        }
    }

    let site_parsed: RawSiteToml = toml::from_str(&site_raw)
        .map_err(|e| anyhow::anyhow!("parsing {}: {e}", site_toml_path.display()))?;

    // The node's own values come from one place: the system config in two-file
    // mode, the one file in one-file mode. The system config is parsed strictly
    // only when it is a second file, so a one-file config is not refused for the
    // site-only keys it necessarily carries.
    let (node_server, node, auth, node_health, raw_genid, raw_cache, raw_limits) = if one_file {
        (
            site_parsed.server.clone().unwrap_or_default(),
            site_parsed.node,
            site_parsed.auth,
            None,
            site_parsed.genid,
            site_parsed.cache,
            site_parsed.limits,
        )
    } else {
        let system_parsed: RawSystemToml = toml::from_str(&system_raw)
            .map_err(|e| anyhow::anyhow!("parsing {}: {e}", system_config_path.display()))?;
        (
            system_parsed.server.clone().unwrap_or_default(),
            system_parsed.node,
            system_parsed.auth,
            system_parsed.health,
            system_parsed.genid,
            system_parsed.cache,
            system_parsed.limits,
        )
    };
    let limits_file = if one_file {
        &site_toml_path
    } else {
        &system_config_path.to_path_buf()
    };
    let limits = m6_core::limits::Limits::from_toml(
        raw_limits.as_ref(),
        limits_file,
        m6_core::platform::process::page_size(),
    )
    .map_err(|e| anyhow::anyhow!("config error: {e}"))?;
    let stats = site_parsed.stats.unwrap_or_default();
    if stats.interval_s == 0 {
        anyhow::bail!(
            "config error: [stats].interval_s must be at least 1, because 0 would write the stats line on every pass of the event loop"
        );
    }
    let site_server = site_parsed.server.unwrap_or_default();

    // Resolved before `bind`, because in redirect mode it supplies the default.
    let redirect_bind = node_server.redirect_bind;
    // The unmatched-host policy is node state: a node serves its own names, so
    // both keys are read from the system config even when site.toml carries a
    // [server] table.
    let served_hosts = validate_served_hosts(node_server.served_hosts.unwrap_or_default())?;
    let redirect_unmatched_to = validate_redirect_unmatched_to(node_server.redirect_unmatched_to)?;
    if redirect_unmatched_to.is_some() && served_hosts.is_empty() {
        anyhow::bail!(
            "config error: [server].redirect_unmatched_to is set but [server].served_hosts is empty. Every request would be redirected, including this node's own name, so the deploy's checks and the monitor could not reach it. List the host names this node serves"
        );
    }
    // Warming is a property of the node, not of the site it serves.
    let warm_on_start = node_server.warm_on_start.unwrap_or(false);
    let lock_memory = node_server.lock_memory.unwrap_or(true);

    // A redirector binds `redirect_bind` and nothing else, so requiring a
    // separate `bind` there would mean every redirect config carried the same
    // address twice, or carried a second one that was never listened on.
    let bind = match node_server.bind {
        Some(b) => b,
        None => redirect_bind
            .clone()
            .ok_or_else(|| anyhow::anyhow!("config error: [server].bind is required"))?,
    };
    let tls_cert = node_server.tls_cert;
    let tls_key = node_server.tls_key;
    if redirect_bind.is_none() {
        if tls_cert.is_none() {
            anyhow::bail!("config error: [server].tls_cert is required");
        }
        if tls_key.is_none() {
            anyhow::bail!("config error: [server].tls_key is required");
        }
    }
    let backend_timeout_secs = site_server.backend_timeout_secs.unwrap_or(30);
    let max_requests_per_connection = node_server
        .max_requests_per_connection
        .unwrap_or(crate::http11::DEFAULT_MAX_REQUESTS_PER_CONN);
    if max_requests_per_connection == 0 {
        anyhow::bail!(
            "config error: [server].max_requests_per_connection must be at least 1, because 0 would close every connection before it served a request"
        );
    }
    let (process_index, status_bind) = validate_status(
        node_server.process_index,
        node_server.status_bind,
        node_server.reuse_port_processes,
    )?;
    let h2c_bind = node_server.h2c_bind;
    // Nothing is inferred from an address: a listener is internal only when
    // this list names it. A redirector binds nothing the list could name, so a
    // site.toml shared with one must not break it.
    let internal_binds = node_server.internal_binds;
    let internal_peers = node_server.internal_peers;
    let internal_peers = if redirect_bind.is_none() {
        validate_internal_binds(internal_binds.as_deref(), &bind, h2c_bind.as_deref())?;
        validate_internal_peers(
            internal_peers.as_deref(),
            internal_binds.as_deref().unwrap_or_default(),
        )?
    } else {
        Vec::new()
    };
    let internal_binds = internal_binds.unwrap_or_default();

    let tls_cert_path = tls_cert
        .as_deref()
        .map(|c| secret_path("[server] tls_cert", c))
        .transpose()?;
    let tls_key_path = tls_key
        .as_deref()
        .map(|k| secret_path("[server] tls_key", k))
        .transpose()?;

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

    // Node identity comes from the system config, which must name it.
    let node = node.ok_or_else(|| {
        let file = if one_file {
            site_toml_path.display()
        } else {
            system_config_path.display()
        };
        anyhow::anyhow!(
            "config error: {file} has no [node]. Add [node] with name = \"<this node's name>\", for example name = \"edge-a\""
        )
    })?;

    let server = ServerConfig {
        bind,
        tls_cert: tls_cert_path
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned()),
        tls_key: tls_key_path
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned()),
        backend_timeout_secs,
        max_requests_per_connection,
        reuse_port: node_server.reuse_port.unwrap_or(false),
        reuse_port_processes: node_server.reuse_port_processes,
        process_index,
        status_bind,
        h2c_bind,
        internal_binds,
        internal_peers,
        warm_on_start,
        lock_memory,
        redirect_bind: redirect_bind.clone(),
        served_hosts,
        redirect_unmatched_to,
        // Uppercased once at load so the hot-path check is a plain comparison
        // rather than a case-insensitive one per request.
        allowed_methods: site_server
            .allowed_methods
            .unwrap_or_else(default_allowed_methods)
            .iter()
            .map(|m| m.trim().to_ascii_uppercase())
            .filter(|m| !m.is_empty())
            .collect(),
    };
    // The one `[log]` parse, shared with every other service. site.toml is the
    // bottom of the precedence chain, so an absent level here is `info`.
    let raw = LogConfig::from_toml(site_parsed.log.as_ref())
        .map_err(|e| anyhow::anyhow!("parsing {}: {e}", site_toml_path.display()))?;
    let level = raw.level.unwrap_or_else(|| "info".to_string());
    m6_core::log::parse_level(&level).map_err(|e| anyhow::anyhow!("config error: [log] {e}"))?;
    let log = LogConfig { level: Some(level) };
    let analytics = site_parsed.analytics.unwrap_or_default();
    let mut health = site_parsed.health.unwrap_or_default();
    // The node's own health keys come from the system config only.
    if let Some(sys) = node_health {
        health.metrics_require = sys.metrics_require;
        health.metrics_peers = sys.metrics_peers;
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
        check_route_cache(route.cache.as_deref(), &format!("[[route]] {}", route.path))?;
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
        check_route_cache(rg.cache.as_deref(), &format!("[[route_group]] {}", rg.path))?;
    }

    // Validate auth. The misplaced check above has already refused an [auth] in
    // site.toml in two-file mode, so `auth` is the one owner's value.
    if let Some(ref a) = auth {
        let Some(backend) = backends.iter().find(|backend| backend.name == a.backend) else {
            anyhow::bail!(
                "config error: [auth].backend `{}` not in [[backend]]",
                a.backend
            );
        };
        // The check path is the socket-backend dispatch, so a URL backend
        // cannot serve it: that would be a different transport and a
        // different round trip. Refuse it by name at load, where
        // --dump-config sees it too.
        if backend.sockets.is_none() {
            anyhow::bail!(
                "config error: [auth].backend `{}` is a URL backend: the check path is a Unix socket on this host, so [auth].backend must name a backend with `sockets`",
                a.backend
            );
        }
    }

    // Validate require on routes needs [auth], and [auth] needs the check
    // backend that answers POST /check.
    let auth_backend = auth.as_ref().map(|auth| auth.backend.as_str());
    for route in &routes {
        if route.require.is_some() {
            if auth.is_none() {
                anyhow::bail!(
                    "config error: route {} has `require` but no [auth] declared",
                    route.path
                );
            }
            if auth_backend.is_none() {
                anyhow::bail!(
                    "config error: route {} has `require` but [auth] has no `backend`; the edge asks POST /check, so [auth] backend must name a [[backend]] with `sockets`",
                    route.path
                );
            }
        }
    }
    for rg in &route_groups {
        if rg.require.is_some() {
            if auth.is_none() {
                anyhow::bail!(
                    "config error: route_group {} has `require` but no [auth] declared",
                    rg.path
                );
            }
            if auth_backend.is_none() {
                anyhow::bail!(
                    "config error: route_group {} has `require` but [auth] has no `backend`; the edge asks POST /check, so [auth] backend must name a [[backend]] with `sockets`",
                    rg.path
                );
            }
        }
    }

    if let Some(ref r) = health.metrics_require {
        if auth.is_none() {
            anyhow::bail!("config error: [health].metrics_require needs [auth] with a backend");
        }
        let named = matches!(
            m6_core::auth::Require::parse(r),
            Some(m6_core::auth::Require::Role(n) | m6_core::auth::Require::Group(n)) if !n.is_empty()
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

    // The generation endpoint. Both claims or neither: one alone would leave
    // one half of the endpoint unguarded, and a value that names no role or
    // group is a typo that would otherwise refuse every caller at run time.
    // The generation state. `upstream` makes this a cache node that follows
    // another node; without it the node is the source that owns the ID.
    let genid = match raw_genid {
        None => None,
        Some(raw) => {
            // The keys of one shape applied to the other are typos that would
            // otherwise be ignored, so each is refused by name.
            if raw.upstream.is_some() {
                for (key, value) in [
                    ("read_require", raw.read_require.as_deref()),
                    ("write_require", raw.write_require.as_deref()),
                ] {
                    if value.is_some() {
                        anyhow::bail!(
                            "config error: [genid] {key} applies only to the source, which has no upstream"
                        );
                    }
                }
            } else {
                for (key, present) in [
                    ("token_file", raw.token_file.is_some()),
                    ("heartbeat_secs", raw.heartbeat_secs.is_some()),
                ] {
                    if present {
                        anyhow::bail!(
                            "config error: [genid] {key} applies only to a cache node with upstream"
                        );
                    }
                }
            }

            let (upstream, token, heartbeat) = match raw.upstream.as_deref() {
                None => (
                    None,
                    None,
                    std::time::Duration::from_secs(crate::genid::DEFAULT_HEARTBEAT_SECS),
                ),
                Some(name) => {
                    let named = backends.iter().any(|b| b.name == name && b.url.is_some());
                    if !named {
                        anyhow::bail!(
                            "config error: [genid].upstream = \"{name}\" must name a [[backend]] with a url"
                        );
                    }
                    let Some(token_file) = raw.token_file.as_deref() else {
                        anyhow::bail!(
                            "config error: [genid].upstream = \"{name}\" needs token_file"
                        );
                    };
                    let path = secret_path("[genid] token_file", token_file)?;
                    let token = match std::fs::read_to_string(&path) {
                        Ok(text) => text.trim().to_string(),
                        Err(e) => anyhow::bail!(
                            "config error: [genid] token_file {} cannot be read: {e}",
                            path.display()
                        ),
                    };
                    if token.is_empty() {
                        anyhow::bail!(
                            "config error: [genid] token_file {} is empty",
                            path.display()
                        );
                    }
                    let secs = raw
                        .heartbeat_secs
                        .unwrap_or(crate::genid::DEFAULT_HEARTBEAT_SECS);
                    if secs == 0 {
                        anyhow::bail!(
                            "config error: [genid].heartbeat_secs must be at least 1, because 0 would send a heartbeat every loop turn"
                        );
                    }
                    (
                        Some(name.to_string()),
                        Some(token),
                        std::time::Duration::from_secs(secs),
                    )
                }
            };

            // The source's two claims, both or neither.
            let read = raw.read_require.is_some();
            let write = raw.write_require.is_some();
            if read != write {
                anyhow::bail!(
                    "config error: [genid] needs both read_require and write_require, or neither"
                );
            }
            for (key, value) in [
                ("read_require", raw.read_require.as_deref()),
                ("write_require", raw.write_require.as_deref()),
            ] {
                let Some(value) = value else { continue };
                if auth.is_none() {
                    anyhow::bail!(
                        "config error: [genid] read_require and write_require need [auth] with a backend"
                    );
                }
                let named = matches!(
                    m6_core::auth::Require::parse(value),
                    Some(m6_core::auth::Require::Role(n) | m6_core::auth::Require::Group(n)) if !n.is_empty()
                );
                if !named {
                    anyhow::bail!(
                        "config error: [genid].{key} = \"{value}\" must be \"role:<name>\" or \"group:<name>\""
                    );
                }
            }
            Some(GenidConfig {
                read_require: raw.read_require,
                write_require: raw.write_require,
                upstream,
                token,
                heartbeat,
            })
        }
    };

    // The disk cache. A cache node only, because a source sits beside its
    // backends and a cold source cache costs one local render per page, while
    // a cache node's cold miss crosses the network. The directory must be
    // absolute and writable by this process: a probe file is created and
    // removed at start, so an unwritable mount fails the start rather than
    // every later write.
    let cache_dir = match raw_cache {
        None => None,
        Some(raw) => {
            let dir = raw.dir.ok_or_else(|| {
                anyhow::anyhow!("config error: [cache] needs dir = \"<absolute path>\"")
            })?;
            let is_cache_node = genid.as_ref().is_some_and(|g| g.upstream.is_some());
            if !is_cache_node {
                anyhow::bail!("config error: [cache].dir is for a cache node ([genid] upstream)");
            }
            let path = PathBuf::from(&dir);
            if !path.is_absolute() {
                anyhow::bail!("config error: [cache].dir must be an absolute path");
            }
            let probe = path.join(".m6-probe");
            match std::fs::write(&probe, b"") {
                Ok(()) => {
                    let _ = std::fs::remove_file(&probe);
                }
                Err(e) => anyhow::bail!("config error: [cache].dir {}: {e}", path.display()),
            }
            Some(path)
        }
    };

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
        stats,
        health,
        rate_limit,
        errors,
        security,
        auth,
        genid,
        cache_dir,
        limits,
        backends,
        routes,
        route_groups,
        site_dir: site_dir.to_path_buf(),
    })
}

/// Keys that are this node's own state. They belong in the system config.
const NODE_SERVER_KEYS: &[&str] = &[
    "bind",
    "tls_cert",
    "tls_key",
    "h2c_bind",
    "internal_binds",
    "internal_peers",
    "redirect_bind",
    "warm_on_start",
    "lock_memory",
    "max_requests_per_connection",
    "reuse_port",
    "reuse_port_processes",
    "process_index",
    "status_bind",
    "served_hosts",
    "redirect_unmatched_to",
];
/// The fields of `RawSystemHealth`; change both together.
const NODE_HEALTH_KEYS: &[&str] = &["metrics_require", "metrics_peers"];
/// Whole sections that are node state. They belong in the system config.
///
/// `RawSiteToml` carries `[node]` only for one-file mode. In two-file mode a
/// `[node]` in `site.toml` is named here so `load` refuses it.
const NODE_SECTIONS: &[&str] = &["auth", "cache", "genid", "limits", "node"];
/// Keys that describe how the site's routes are served. They belong in
/// `site.toml`.
const SITE_SERVER_KEYS: &[&str] = &["backend_timeout_secs", "allowed_methods"];

/// Name every key that sits in the file it does not belong to.
///
/// Each message names the key and the file it belongs in, and `load` refuses
/// the start with exit 2 for any of them. The rule: `site.toml` holds state
/// every node of the site shares, and the system config holds this node's own
/// state (binds, certificates, credential paths, per-node security).
///
/// `site` and `system` are the two files already parsed as tables. The caller
/// runs this only in two-file mode: in one-file mode every key belongs to the
/// one file.
pub fn misplaced_keys(
    site: &toml::Table,
    system: &toml::Table,
    system_config_path: &Path,
) -> Vec<String> {
    let mut out = Vec::new();
    let in_site = |section: &str, key: &str| site.get(section).and_then(|t| t.get(key)).is_some();
    for &key in NODE_SERVER_KEYS {
        if in_site("server", key) {
            out.push(node_state_message("server", Some(key), system_config_path));
        }
    }
    for &section in NODE_SECTIONS {
        if site.get(section).is_some() {
            out.push(node_state_message(section, None, system_config_path));
        }
    }
    for &key in NODE_HEALTH_KEYS {
        if in_site("health", key) {
            out.push(node_state_message("health", Some(key), system_config_path));
        }
    }
    for &key in SITE_SERVER_KEYS {
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

#[cfg(test)]
mod tests {

    use super::*;
    use tempfile::TempDir;

    fn make_test_dir() -> TempDir {
        tempfile::tempdir().unwrap()
    }

    fn write_file(dir: &Path, name: &str, content: &str) {
        let replaced = content.replace("{DIR}", &dir.display().to_string());
        std::fs::write(dir.join(name), replaced).unwrap();
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
            "[node]\nname = \"test\"\n\n[server]\nbind = \"127.0.0.1:8443\"\ntls_cert = \"{DIR}/cert.pem\"\ntls_key = \"{DIR}/key.pem\"\n",
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
[node]
name = "test"

[server]
bind     = "127.0.0.1:8443"
tls_cert = "{DIR}/cert.pem"
tls_key  = "{DIR}/key.pem"
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

    /// The stats line's interval: ten seconds when no `[stats]` table is
    /// present, and whatever `interval_s` names when one is.
    #[test]
    fn the_stats_interval_is_read_from_the_site_table() {
        let dir = make_test_dir();
        setup_minimal(dir.path());
        let cfg = load(dir.path(), &dir.path().join("system.toml")).unwrap();
        assert_eq!(cfg.stats.interval_s, DEFAULT_STATS_INTERVAL_S);
        assert_eq!(cfg.stats.interval_s, 10);

        let with_stats = format!("{}\n[stats]\ninterval_s = 2\n", minimal_site_toml());
        write_file(dir.path(), "site.toml", &with_stats);
        let cfg = load(dir.path(), &dir.path().join("system.toml")).unwrap();
        assert_eq!(cfg.stats.interval_s, 2);
    }

    /// Zero would write the line on every pass of the event loop: refused at
    /// start, and the error names the key.
    #[test]
    fn a_zero_stats_interval_stops_the_start() {
        let dir = make_test_dir();
        setup_minimal(dir.path());
        let with_stats = format!("{}\n[stats]\ninterval_s = 0\n", minimal_site_toml());
        write_file(dir.path(), "site.toml", &with_stats);
        let error = load(dir.path(), &dir.path().join("system.toml")).unwrap_err();
        let text = format!("{error}");
        assert!(
            text.contains("[stats].interval_s"),
            "the error did not name the key: {text}"
        );
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
[[backend]]
name = "b"
sockets = "/run/m6/*.sock"
"#,
        );
        write_file(
            dir.path(),
            "system.toml",
            "[node]\nname = \"test\"\n\n[server]\nbind = \"0.0.0.0:443\"\ntls_cert = \"{DIR}/cert.pem\"\ntls_key = \"{DIR}/key.pem\"\n",
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
tls_cert = "{DIR}/missing-cert.pem"
tls_key  = "{DIR}/key.pem"
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
[[backend]]
name = "b"
sockets = "/run/m6/*.sock"
[[route]]
path    = "/"
backend = "unknown"
"#,
        );
        write_file(dir.path(), "system.toml", "[node]\nname = \"test\"\n\n[server]\nbind = \"0.0.0.0:443\"\ntls_cert = \"{DIR}/cert.pem\"\ntls_key = \"{DIR}/key.pem\"\n");
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
        write_file(dir.path(), "system.toml", "[node]\nname = \"test\"\n\n[server]\nbind = \"0.0.0.0:443\"\ntls_cert = \"{DIR}/cert.pem\"\ntls_key = \"{DIR}/key.pem\"\n");
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
[[backend]]
name = "b"
sockets = "/run/m6/*.sock"
[[route]]
path    = "/"
backend = "b"
require = "group:editors"
"#,
        );
        write_file(dir.path(), "system.toml", "[node]\nname = \"test\"\n\n[server]\nbind = \"0.0.0.0:443\"\ntls_cert = \"{DIR}/cert.pem\"\ntls_key = \"{DIR}/key.pem\"\n");
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
[errors]
mode = "custom"
[[backend]]
name = "b"
sockets = "/run/m6/*.sock"
"#,
        );
        write_file(dir.path(), "system.toml", "[node]\nname = \"test\"\n\n[server]\nbind = \"0.0.0.0:443\"\ntls_cert = \"{DIR}/cert.pem\"\ntls_key = \"{DIR}/key.pem\"\n");
        let result = load(dir.path(), &dir.path().join("system.toml"));
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("custom"));
    }

    #[test]
    fn bind_in_site_toml_is_refused() {
        let dir = make_test_dir();
        write_file(dir.path(), "cert.pem", "dummy");
        write_file(dir.path(), "key.pem", "dummy");
        write_file(
            dir.path(),
            "site.toml",
            "[site]\nname = \"Test\"\ndomain = \"test.example.com\"\n\n[server]\nbind = \"127.0.0.1:8443\"\ntls_cert = \"{DIR}/cert.pem\"\ntls_key = \"{DIR}/key.pem\"\n\n[[backend]]\nname = \"b\"\nsockets = \"/run/m6/*.sock\"\n",
        );
        write_file(
            dir.path(),
            "system.toml",
            "[node]\nname = \"test\"\n\n[server]\nbind = \"0.0.0.0:443\"\ntls_cert = \"{DIR}/cert.pem\"\ntls_key = \"{DIR}/key.pem\"\n",
        );
        let e = load(dir.path(), &dir.path().join("system.toml"))
            .unwrap_err()
            .to_string();
        assert!(e.contains("site.toml sets [server].bind"), "{e}");
        assert!(e.contains("belongs in the system config"), "{e}");
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
"#,
        );
        write_file(
            dir.path(),
            "system.toml",
            "[node]\nname = \"test\"\n\n[server]\nredirect_bind = \"0.0.0.0:80\"\n",
        );
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
[[backend]]
name = "b"
sockets = "/run/m6/*.sock"
"#,
        );
        write_file(
            dir.path(),
            "system.toml",
            "[node]\nname = \"test\"\n\n[server]\nbind = \"0.0.0.0:443\"\n",
        );
        let err = load(dir.path(), &dir.path().join("system.toml"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("tls_cert"), "unexpected error: {err}");
    }

    // ── [server] served_hosts and redirect_unmatched_to ─────────────────────

    /// The combination that would lock an operator out of their own node: a
    /// target with no served names answers every request with a redirect,
    /// including the node's own name. `load` must refuse it, naming the empty
    /// list, so a typo cannot take the deploy's checks and the monitor off the
    /// node.
    #[test]
    fn a_canonical_target_with_no_served_hosts_refuses_to_start() {
        let e = load_error("redirect_unmatched_to = \"example.com\"\n");
        assert!(e.starts_with("config error:"), "{e}");
        assert!(e.contains("[server].served_hosts is empty"), "{e}");
        assert!(e.contains("Every request would be redirected"), "{e}");
        assert!(e.contains("this node's own name"), "{e}");
    }

    #[test]
    fn a_served_host_is_matched_without_its_port_or_case() {
        let cfg = load_with_system_server(
            "served_hosts = [\"Edge-A.Example.COM\"]\nredirect_unmatched_to = \"Example.COM\"\n",
        )
        .unwrap();
        assert_eq!(
            cfg.server.served_hosts,
            vec!["edge-a.example.com".to_string()]
        );
        assert_eq!(
            cfg.server.redirect_unmatched_to.as_deref(),
            Some("example.com")
        );
        assert!(cfg
            .server
            .unmatched_host_redirect(Some("edge-a.example.com:8443"), "/x", None)
            .is_none());
        assert_eq!(
            cfg.server
                .unmatched_host_redirect(Some("other.example.com"), "/x", Some("a=1")),
            Some("https://example.com/x?a=1".to_string())
        );
        // An absent Host is outside the list, so a redirector sends it to the
        // canonical name instead of answering 400.
        assert_eq!(
            cfg.server.unmatched_host_redirect(None, "/", None),
            Some("https://example.com/".to_string())
        );
    }

    #[test]
    fn a_served_list_without_a_target_changes_nothing() {
        let cfg = load_with_system_server("served_hosts = [\"edge-a.example.com\"]\n").unwrap();
        assert!(cfg
            .server
            .unmatched_host_redirect(Some("other.example.com"), "/", None)
            .is_none());
    }

    #[test]
    fn a_target_that_is_an_address_or_carries_a_path_is_refused() {
        let e = load_error(
            "served_hosts = [\"edge-a.example.com\"]\nredirect_unmatched_to = \"203.0.113.7\"\n",
        );
        assert!(e.contains("IP address"), "{e}");
        let e = load_error(
            "served_hosts = [\"edge-a.example.com\"]\nredirect_unmatched_to = \"example.com/x\"\n",
        );
        assert!(e.contains("not a bare host name"), "{e}");
        let e =
            load_error("served_hosts = [\"edge-a.example.com\"]\nredirect_unmatched_to = \"\"\n");
        assert!(e.contains("is empty"), "{e}");
    }

    #[test]
    fn a_served_entry_with_a_port_or_wildcard_is_refused() {
        let e = load_error(
            "served_hosts = [\"edge-a.example.com:8443\"]\nredirect_unmatched_to = \"example.com\"\n",
        );
        assert!(e.contains("[server].served_hosts entry"), "{e}");
        let e = load_error(
            "served_hosts = [\"*.example.com\"]\nredirect_unmatched_to = \"example.com\"\n",
        );
        assert!(e.contains("[server].served_hosts entry"), "{e}");
    }

    /// The policy is node state, so site.toml must not carry it. `misplaced_keys`
    /// is one list, and this is the pair of keys that joined it.
    #[test]
    fn misplaced_keys_names_the_host_policy_in_site_toml() {
        let m = misplaced(
            "[server]\nserved_hosts = [\"edge-a.example.com\"]\nredirect_unmatched_to = \"example.com\"\n",
            "",
        );
        assert_eq!(m.len(), 2, "{m:?}");
        assert!(m.iter().any(|x| x.contains("[server].served_hosts")));
        assert!(m
            .iter()
            .any(|x| x.contains("[server].redirect_unmatched_to")));
    }

    #[test]
    fn test_backend_timeout_default() {
        let dir = make_test_dir();
        setup_minimal(dir.path());
        let cfg = load(dir.path(), &dir.path().join("system.toml")).unwrap();
        assert_eq!(cfg.server.backend_timeout_secs, 30);
    }

    #[test]
    fn test_max_requests_per_connection_default() {
        let dir = make_test_dir();
        setup_minimal(dir.path());
        let cfg = load(dir.path(), &dir.path().join("system.toml")).unwrap();
        assert_eq!(cfg.server.max_requests_per_connection, 100);
    }

    fn load_with_system_server_line(line: &str) -> anyhow::Result<Config> {
        let dir = make_test_dir();
        setup_minimal(dir.path());
        write_file(
            dir.path(),
            "system.toml",
            &format!(
                "[node]\nname = \"test\"\n\n[server]\nbind = \"127.0.0.1:8443\"\ntls_cert = \"{{DIR}}/cert.pem\"\ntls_key = \"{{DIR}}/key.pem\"\n{line}\n"
            ),
        );
        load(dir.path(), &dir.path().join("system.toml"))
    }

    #[test]
    fn test_max_requests_per_connection_is_read() {
        let cfg = load_with_system_server_line("max_requests_per_connection = 10000").unwrap();
        assert_eq!(cfg.server.max_requests_per_connection, 10_000);
    }

    #[test]
    fn test_max_requests_per_connection_zero_is_rejected() {
        let err = load_with_system_server_line("max_requests_per_connection = 0")
            .unwrap_err()
            .to_string();
        assert!(err.contains("max_requests_per_connection"), "error: {err}");
    }

    /// The default is what keeps an accidental second instance from binding a
    /// live port, so it is false unless the node asks for sharing.
    #[test]
    fn reuse_port_defaults_off() {
        let cfg = load_with_system_server_line("").unwrap();
        assert!(!cfg.server.reuse_port);
    }

    #[test]
    fn reuse_port_is_read_from_the_system_config() {
        let cfg = load_with_system_server_line("reuse_port = true").unwrap();
        assert!(cfg.server.reuse_port);
    }

    #[test]
    fn reuse_port_processes_is_unset_by_default_and_read_when_set() {
        let cfg = load_with_system_server_line("reuse_port = true").unwrap();
        assert_eq!(cfg.server.reuse_port_processes, None);
        let cfg =
            load_with_system_server_line("reuse_port = true\nreuse_port_processes = 3").unwrap();
        assert_eq!(cfg.server.reuse_port_processes.map(|n| n.get()), Some(3));
    }

    /// A zero would be a modulus of zero in the kernel filter.
    #[test]
    fn status_listener_keys_are_read_and_default_off() {
        let cfg = load_with_system_server_line("").unwrap();
        assert_eq!(
            (cfg.server.process_index, cfg.server.status_bind),
            (None, None)
        );
        let cfg = load_with_system_server_line(
            "reuse_port_processes = 3\nprocess_index = 2\nstatus_bind = \"127.0.0.1:9440\"",
        )
        .unwrap();
        assert_eq!(cfg.server.process_index, Some(2));
        assert_eq!(
            status_addr(cfg.server.status_bind.as_deref().unwrap(), 2)
                .unwrap()
                .port(),
            9442
        );
    }

    #[test]
    fn status_bind_without_an_index_is_rejected() {
        let err = load_with_system_server_line("status_bind = \"127.0.0.1:9440\"")
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("status_bind") && err.contains("process_index"),
            "error: {err}"
        );
    }

    #[test]
    fn an_index_past_the_process_count_is_rejected() {
        let err = load_with_system_server_line("reuse_port_processes = 2\nprocess_index = 2")
            .unwrap_err()
            .to_string();
        assert!(err.contains("process_index 2"), "error: {err}");
    }

    #[test]
    fn a_status_port_past_65535_is_rejected() {
        let err =
            load_with_system_server_line("process_index = 5\nstatus_bind = \"127.0.0.1:65533\"")
                .unwrap_err()
                .to_string();
        assert!(err.contains("past 65535"), "error: {err}");
    }

    #[test]
    fn reuse_port_processes_zero_is_rejected() {
        let err = load_with_system_server_line("reuse_port = true\nreuse_port_processes = 0")
            .unwrap_err()
            .to_string();
        assert!(err.contains("reuse_port_processes"), "error: {err}");
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
            "[node]\nname = \"test\"\n\n[server]\nbind = \"127.0.0.1:8443\"\ntls_cert = \"{DIR}/cert.pem\"\ntls_key = \"{DIR}/key.pem\"\n",
        );
        let cfg = load(dir.path(), &dir.path().join("system.toml")).unwrap();
        assert_eq!(cfg.server.backend_timeout_secs, 60);
    }

    #[test]
    fn backend_timeout_in_system_config_is_refused() {
        let dir = make_test_dir();
        write_file(dir.path(), "cert.pem", "dummy");
        write_file(dir.path(), "key.pem", "dummy");
        write_file(
            dir.path(),
            "site.toml",
            "[site]\nname = \"Test\"\ndomain = \"test.example.com\"\n\n[server]\nbackend_timeout_secs = 10\n\n[[backend]]\nname = \"b\"\nsockets = \"/run/m6/*.sock\"\n",
        );
        write_file(
            dir.path(),
            "system.toml",
            "[node]\nname = \"test\"\n\n[server]\nbind = \"127.0.0.1:8443\"\ntls_cert = \"{DIR}/cert.pem\"\ntls_key = \"{DIR}/key.pem\"\nbackend_timeout_secs = 120\n",
        );
        let e = load(dir.path(), &dir.path().join("system.toml"))
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("the system config sets [server].backend_timeout_secs"),
            "{e}"
        );
        assert!(e.contains("belongs in site.toml"), "{e}");
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
            "h2c_bind = \"127.0.0.1:18080\"\ninternal_binds = [\"127.0.0.1:18080\"]\ninternal_peers = [\"127.0.0.1\"]\n",
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
            "h2c_bind = \"[::1]:8080\"\ninternal_binds = [\"[0:0:0:0:0:0:0:1]:8080\"]\ninternal_peers = [\"::1\"]\n",
        )
        .unwrap();
        assert!(cfg.server.is_internal("[::1]:8080"));
        let cfg = load_with_system_server(
            "h2c_bind = \"[fd00::1]:80\"\ninternal_binds = [\"[fd00::1]:80\"]\ninternal_peers = [\"fd00::2\"]\n",
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
            "tls_key  = \"{DIR}/key.pem\"\n",
            "tls_key  = \"{DIR}/key.pem\"\ninternal_binds = []\n",
        );
        write_file(dir.path(), "site.toml", &site);
        let mut sys = minimal_system_toml();
        sys.push_str("h2c_bind = \"127.0.0.1:18080\"\ninternal_binds = [\"127.0.0.1:18080\"]\ninternal_peers = [\"127.0.0.1\"]\n");
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
"#,
        );
        write_file(dir.path(), "system.toml", "[node]\nname = \"test\"\n\n[server]\nredirect_bind = \"0.0.0.0:80\"\ninternal_binds = [\"203.0.113.7:80\"]\n");
        let cfg = load(dir.path(), &dir.path().join("system.toml")).unwrap();
        assert_eq!(cfg.server.redirect_bind.as_deref(), Some("0.0.0.0:80"));
    }

    // ── [server] internal_peers ─────────────────────────────────────────────

    /// An internal h2c listener, so `internal_peers` is in play.
    const INTERNAL_H2C: &str =
        "h2c_bind = \"127.0.0.1:18080\"\ninternal_binds = [\"127.0.0.1:18080\"]\n";

    #[test]
    fn internal_peers_are_required_when_a_listener_is_internal() {
        let e = load_error(INTERNAL_H2C);
        assert!(e.starts_with("config error:"), "{e}");
        assert!(e.contains("[server].internal_peers is not set"), "{e}");
        assert!(e.contains("127.0.0.1:18080"), "{e}");
    }

    #[test]
    fn internal_peers_without_an_internal_listener_is_refused() {
        let with_public_h2c =
            "h2c_bind = \"127.0.0.1:18080\"\ninternal_binds = []\ninternal_peers = [\"127.0.0.1\"]\n";
        for extra in [with_public_h2c, "internal_peers = [\"127.0.0.1\"]\n"] {
            let e = load_error(extra);
            assert!(
                e.contains("[server].internal_peers is set but no listener is internal"),
                "{extra}: {e}"
            );
        }
    }

    #[test]
    fn peers_of_the_other_ip_family_than_the_internal_bind_are_refused() {
        let v6 = "h2c_bind = \"[fd00::1]:18080\"\ninternal_binds = [\"[fd00::1]:18080\"]\n";
        let e = load_error(&format!("{v6}internal_peers = [\"10.0.0.2\"]\n"));
        assert!(e.contains("[server].internal_peers"), "{e}");
        assert!(e.contains("IPv6") && e.contains("fd00::1"), "{e}");
        let v4 = "h2c_bind = \"10.0.0.1:18080\"\ninternal_binds = [\"10.0.0.1:18080\"]\n";
        let e = load_error(&format!("{v4}internal_peers = [\"fd00::2\"]\n"));
        assert!(e.contains("IPv4") && e.contains("10.0.0.1"), "{e}");
        // An IPv4-mapped bind is IPv4, and a mapped peer is stored as IPv4.
        let mapped = "h2c_bind = \"[::ffff:10.0.0.1]:18080\"\ninternal_binds = [\"[::ffff:10.0.0.1]:18080\"]\n";
        let e = load_error(&format!("{mapped}internal_peers = [\"fd00::2\"]\n"));
        assert!(e.contains("IPv4"), "{e}");
    }

    #[test]
    fn one_peer_of_the_bind_family_is_enough() {
        let v6 = "h2c_bind = \"[fd00::1]:18080\"\ninternal_binds = [\"[fd00::1]:18080\"]\n";
        let cfg = load_with_system_server(&format!(
            "{v6}internal_peers = [\"10.0.0.2\", \"fd00::2\"]\n"
        ))
        .unwrap();
        assert_eq!(cfg.server.internal_peers.len(), 2);
    }

    #[test]
    fn internal_peers_must_not_be_empty() {
        let e = load_error(&format!("{INTERNAL_H2C}internal_peers = []\n"));
        assert!(e.contains("[server].internal_peers is empty"), "{e}");
        assert!(e.contains("internal_binds = []"), "{e}");
    }

    #[test]
    fn an_internal_peer_must_be_an_ip() {
        for entry in ["localhost", "10.0.0.0/24", "10.0.0.2:80"] {
            let e = load_error(&format!("{INTERNAL_H2C}internal_peers = [\"{entry}\"]\n"));
            assert!(
                e.contains(&format!(
                    "[server].internal_peers entry \"{entry}\" is not an IP address"
                )),
                "{entry}: {e}"
            );
        }
    }

    #[test]
    fn an_internal_peer_must_be_in_a_local_range() {
        for entry in ["203.0.113.7", "0.0.0.0", "::"] {
            let e = load_error(&format!("{INTERNAL_H2C}internal_peers = [\"{entry}\"]\n"));
            assert!(
                e.contains(&format!(
                    "[server].internal_peers entry \"{entry}\" is not in a local range"
                )),
                "{entry}: {e}"
            );
            assert!(
                e.contains("an internal peer must be a loopback"),
                "{entry}: {e}"
            );
        }
    }

    #[test]
    fn internal_peers_are_stored_canonical() {
        let cfg = load_with_system_server(&format!(
            "{INTERNAL_H2C}internal_peers = [\"::ffff:10.0.0.2\", \"fd00::5\"]\n"
        ))
        .unwrap();
        let want: Vec<std::net::IpAddr> =
            vec!["10.0.0.2".parse().unwrap(), "fd00::5".parse().unwrap()];
        assert_eq!(cfg.server.internal_peers, want);
    }

    #[test]
    fn no_internal_listener_means_no_internal_peers() {
        let cfg = load_with_system_server("").unwrap();
        assert!(cfg.server.internal_peers.is_empty());
    }

    #[test]
    fn system_internal_peers_replace_site_internal_peers() {
        let dir = make_test_dir();
        setup_minimal(dir.path());
        let mut site = minimal_site_toml();
        site = site.replace(
            "tls_key  = \"{DIR}/key.pem\"\n",
            "tls_key  = \"{DIR}/key.pem\"\ninternal_peers = [\"10.0.0.9\"]\n",
        );
        write_file(dir.path(), "site.toml", &site);
        let mut sys = minimal_system_toml();
        sys.push_str(INTERNAL_H2C);
        sys.push_str("internal_peers = [\"127.0.0.1\"]\n");
        write_file(dir.path(), "system.toml", &sys);
        let cfg = load(dir.path(), &dir.path().join("system.toml")).unwrap();
        let want: Vec<std::net::IpAddr> = vec!["127.0.0.1".parse().unwrap()];
        assert_eq!(cfg.server.internal_peers, want);
    }

    #[test]
    fn redirect_mode_ignores_internal_peers() {
        let dir = make_test_dir();
        write_file(
            dir.path(),
            "site.toml",
            r#"
[site]
name   = "Test"
domain = "test.example.com"
"#,
        );
        write_file(dir.path(), "system.toml", "[node]\nname = \"test\"\n\n[server]\nredirect_bind = \"0.0.0.0:80\"\ninternal_peers = [\"203.0.113.7\"]\n");
        let cfg = load(dir.path(), &dir.path().join("system.toml")).unwrap();
        assert!(cfg.server.internal_peers.is_empty());
    }

    /// Load with `site_extra` appended to site.toml and `sys_extra` appended
    /// to system.toml. A verify key exists as `auth.pem` in the test directory.
    fn load_with(site_extra: &str, sys_extra: &str) -> (TempDir, anyhow::Result<Config>) {
        let dir = make_test_dir();
        setup_minimal(dir.path());
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
    fn auth_needs_a_backend() {
        let (_d, r) = load_with("", "\n[auth]\n");
        let e = r.unwrap_err().to_string();
        assert!(e.contains("backend"), "{e}");
    }

    #[test]
    fn auth_backend_when_given_must_be_declared() {
        let (_d, r) = load_with("", "\n[auth]\nbackend = \"nope\"\n");
        assert!(r
            .unwrap_err()
            .to_string()
            .contains("[auth].backend `nope` not in [[backend]]"));
        let (_d, ok) = load_with("", "\n[auth]\nbackend = \"m6-html\"\n");
        assert!(ok.is_ok());
    }

    #[test]
    fn auth_from_the_system_config_is_used() {
        let (_d, r) = load_with("", "\n[auth]\nbackend = \"m6-html\"\n");
        let auth = r.unwrap().auth.expect("auth present");
        assert_eq!(auth.backend, "m6-html");
    }

    #[test]
    fn auth_in_both_files_is_refused() {
        let (_d, r) = load_with(
            "\n[auth]\nbackend = \"m6-html\"\n",
            "\n[auth]\nbackend = \"m6-html\"\n",
        );
        let e = r.unwrap_err().to_string();
        assert!(e.contains("site.toml sets [auth],"), "{e}");
        assert!(e.contains("belongs in the system config"), "{e}");
    }

    #[test]
    fn a_relative_tls_cert_or_key_is_refused_and_named() {
        for (relative, absolute, expected) in [
            (
                "tls_cert = \"cert.pem\"",
                "tls_key = \"{DIR}/key.pem\"",
                "[server] tls_cert",
            ),
            (
                "tls_key = \"key.pem\"",
                "tls_cert = \"{DIR}/cert.pem\"",
                "[server] tls_key",
            ),
        ] {
            let dir = make_test_dir();
            write_file(dir.path(), "cert.pem", "dummy");
            write_file(dir.path(), "key.pem", "dummy");
            write_file(dir.path(), "site.toml", &minimal_site_toml());
            let sys = format!(
                "[node]\nname = \"test\"\n\n[server]\nbind = \"127.0.0.1:8443\"\n{relative}\n{absolute}\n"
            );
            write_file(dir.path(), "system.toml", &sys);
            let e = load(dir.path(), &dir.path().join("system.toml"))
                .unwrap_err()
                .to_string();
            assert!(e.contains(expected), "{e}");
        }
    }

    #[test]
    fn the_removed_metrics_token_file_is_refused_and_named() {
        // The legacy bearer gate is gone; an operator still carrying it gets a
        // start failure that names the key rather than a silently ignored row.
        let (_d, r) = load_with("", "\n[health]\nmetrics_token_file = \"token\"\n");
        let e = r.unwrap_err().to_string();
        assert!(e.contains("metrics_token_file"), "{e}");
    }

    fn peers_error(entries: &str) -> String {
        let (_d, r) = load_with("", &format!("\n[health]\nmetrics_peers = [{entries}]\n"));
        r.expect_err("must refuse").to_string()
    }

    #[test]
    fn metrics_peers_accepts_local_addresses() {
        let (_d, r) = load_with(
            "",
            "\n[health]\nmetrics_peers = [\"10.0.0.9\", \"127.0.0.1\", \"fd00::5\", \"100.64.0.1\"]\n",
        );
        let h = r.unwrap().health;
        let ips = h.metrics_peer_ips.expect("peers set");
        assert_eq!(ips.len(), 4);
        assert!(ips.contains(&"fd00::5".parse().unwrap()));
    }

    #[test]
    fn metrics_peers_are_stored_canonical() {
        let (_d, r) = load_with("", "\n[health]\nmetrics_peers = [\"::ffff:10.0.0.9\"]\n");
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
        let (_d, r) = load_with("", "\n[health]\nmetrics_require = \"role:monitor\"\n");
        assert_eq!(
            r.unwrap_err().to_string(),
            "config error: [health].metrics_require needs [auth] with a backend"
        );
    }

    #[test]
    fn metrics_require_must_name_a_role_or_group() {
        for bad in ["admin", "role:", "group:", "user:x"] {
            let (_d, r) = load_with(
                "",
                &format!(
                    "\n[auth]\nbackend = \"m6-html\"\n\n[health]\nmetrics_require = \"{bad}\"\n"
                ),
            );
            let e = r.unwrap_err().to_string();
            assert!(
                e.contains("must be \"role:<name>\" or \"group:<name>\""),
                "{bad}: {e}"
            );
        }
        let (_d, ok) = load_with(
            "",
            "\n[auth]\nbackend = \"m6-html\"\n\n[health]\nmetrics_require = \"group:ops\"\n",
        );
        assert!(ok.is_ok());
    }

    #[test]
    fn genid_needs_both_requires_or_neither() {
        let auth = "\n[auth]\nbackend = \"m6-html\"\n";
        for only in [
            "\n[genid]\nread_require = \"role:monitor\"\n",
            "\n[genid]\nwrite_require = \"role:editor\"\n",
        ] {
            let (_d, r) = load_with("", &format!("{auth}{only}"));
            let e = r.unwrap_err().to_string();
            assert_eq!(
                e,
                "config error: [genid] needs both read_require and write_require, or neither"
            );
        }
        let (_d, ok) = load_with(
            "",
            &format!(
                "{auth}\n[genid]\nread_require = \"role:monitor\"\nwrite_require = \"role:editor\"\n"
            ),
        );
        let cfg = ok.expect("both claims load");
        let g = cfg.genid.expect("[genid] present");
        assert_eq!(g.read_require.as_deref(), Some("role:monitor"));
        assert_eq!(g.write_require.as_deref(), Some("role:editor"));
    }

    #[test]
    fn genid_require_needs_auth() {
        let (_d, r) = load_with(
            "",
            "\n[genid]\nread_require = \"role:monitor\"\nwrite_require = \"role:editor\"\n",
        );
        assert_eq!(
            r.unwrap_err().to_string(),
            "config error: [genid] read_require and write_require need [auth] with a backend"
        );
    }

    #[test]
    fn genid_require_must_name_a_role_or_group() {
        for (key, bad) in [
            ("read_require", "admin"),
            ("write_require", "role:"),
            ("write_require", "user:x"),
        ] {
            let body = match key {
                "read_require" => {
                    format!("read_require = \"{bad}\"\nwrite_require = \"role:editor\"\n")
                }
                _ => format!("read_require = \"role:monitor\"\nwrite_require = \"{bad}\"\n"),
            };
            let (_d, r) = load_with(
                "",
                &format!("\n[auth]\nbackend = \"m6-html\"\n\n[genid]\n{body}"),
            );
            let e = r.unwrap_err().to_string();
            assert!(
                e.contains(&format!("[genid].{key}"))
                    && e.contains("must be \"role:<name>\" or \"group:<name>\""),
                "{bad}: {e}"
            );
        }
    }

    #[test]
    fn genid_in_the_system_config_loads() {
        let (_d, r) = load_with(
            "",
            "\n[auth]\nbackend = \"m6-html\"\n\n[genid]\nread_require = \"group:ops\"\nwrite_require = \"role:editor\"\n",
        );
        let cfg = r.expect("a source loads");
        assert_eq!(
            cfg.genid.expect("present").read_require.as_deref(),
            Some("group:ops")
        );
    }

    /// `[limits]` is read from the system config, names the system config in
    /// its refusals, and is refused in `site.toml`.
    #[test]
    fn limits_in_the_system_config_load_and_in_site_toml_are_refused() {
        use m6_core::limits::EDGE_REQUEST;
        let (_d, r) = load_with("", "\n[limits.edge_request]\nmax = \"8MiB\"\n");
        let cfg = r.expect("limits load");
        assert_eq!(cfg.limits.get(&EDGE_REQUEST).max(), 8 << 20);

        let (_d, r) = load_with("", "\n[limits.edge_request]\nmax = \"1KiB\"\n");
        let e = r.unwrap_err().to_string();
        assert!(
            e.contains("[limits.edge_request] max = 1 KiB is below start = 4 KiB"),
            "{e}"
        );
        assert!(e.contains("system"), "{e}");

        let (_d, r) = load_with("\n[limits.edge_request]\nmax = \"8MiB\"\n", "");
        let e = r.unwrap_err().to_string();
        assert!(e.contains("site.toml sets [limits]"), "{e}");
    }

    #[test]
    fn genid_in_site_toml_is_refused() {
        let (_d, r) = load_with(
            "\n[genid]\nread_require = \"role:monitor\"\nwrite_require = \"role:editor\"\n",
            "\n[auth]\nbackend = \"m6-html\"\n",
        );
        let e = r.unwrap_err().to_string();
        assert!(e.contains("site.toml sets [genid]"), "{e}");
        assert!(e.contains("belongs in the system config"), "{e}");
    }

    /// A cache node's site: one URL backend named `origin`, then whatever the
    /// test puts in `[genid]`. The token file is written with `token`, so a
    /// positive case can read it.
    fn load_cache_with(
        sys_extra: &str,
        token: &str,
    ) -> (tempfile::TempDir, anyhow::Result<Config>) {
        let dir = make_test_dir();
        setup_minimal(dir.path());
        write_file(dir.path(), "genid-token", token);
        let mut site = minimal_site_toml();
        site.push_str("\n[[backend]]\nname = \"origin\"\nurl = \"https://127.0.0.1:9\"\n");
        write_file(dir.path(), "site.toml", &site);
        let mut sys = minimal_system_toml();
        sys.push_str(&sys_extra.replace("{DIR}", &dir.path().display().to_string()));
        write_file(dir.path(), "system.toml", &sys);
        let result = load(dir.path(), &dir.path().join("system.toml"));
        (dir, result)
    }

    #[test]
    fn genid_upstream_must_name_a_backend_with_a_url() {
        for name in ["nope", "m6-html"] {
            let (_d, r) = load_cache_with(
                &format!(
                    "\n[genid]\nupstream = \"{name}\"\ntoken_file = \"{{DIR}}/genid-token\"\n"
                ),
                "read-token",
            );
            assert_eq!(
                r.unwrap_err().to_string(),
                format!(
                    "config error: [genid].upstream = \"{name}\" must name a [[backend]] with a url"
                )
            );
        }
    }

    #[test]
    fn genid_upstream_needs_a_token_file() {
        let (_d, r) = load_cache_with("\n[genid]\nupstream = \"origin\"\n", "read-token");
        assert_eq!(
            r.unwrap_err().to_string(),
            "config error: [genid].upstream = \"origin\" needs token_file"
        );
    }

    #[test]
    fn genid_token_file_is_a_secret_path() {
        let (_d, r) = load_cache_with(
            "\n[genid]\nupstream = \"origin\"\ntoken_file = \"genid-token\"\n",
            "read-token",
        );
        let e = r.unwrap_err().to_string();
        assert!(
            e.contains("[genid] token_file") && e.contains("relative path"),
            "{e}"
        );
    }

    #[test]
    fn genid_token_file_must_be_readable_and_not_empty() {
        let (_d, r) = load_cache_with(
            "\n[genid]\nupstream = \"origin\"\ntoken_file = \"{DIR}/missing\"\n",
            "read-token",
        );
        let e = r.unwrap_err().to_string();
        assert!(
            e.contains("[genid] token_file") && e.contains("cannot be read"),
            "{e}"
        );

        let (_d, r) = load_cache_with(
            "\n[genid]\nupstream = \"origin\"\ntoken_file = \"{DIR}/genid-token\"\n",
            "  \n",
        );
        let e = r.unwrap_err().to_string();
        assert!(
            e.contains("[genid] token_file") && e.contains("is empty"),
            "{e}"
        );
    }

    #[test]
    fn genid_heartbeat_secs_must_be_positive() {
        let (_d, r) = load_cache_with(
            "\n[genid]\nupstream = \"origin\"\ntoken_file = \"{DIR}/genid-token\"\nheartbeat_secs = 0\n",
            "read-token",
        );
        assert_eq!(
            r.unwrap_err().to_string(),
            "config error: [genid].heartbeat_secs must be at least 1, because 0 would send a heartbeat every loop turn"
        );
    }

    #[test]
    fn genid_cache_keys_need_an_upstream() {
        let auth = "\n[auth]\nbackend = \"m6-html\"\n";
        for key in ["token_file = \"{DIR}/genid-token\"", "heartbeat_secs = 5"] {
            let (_d, r) = load_with("", &format!("{auth}\n[genid]\n{key}\n"));
            assert_eq!(
                r.unwrap_err().to_string(),
                format!(
                    "config error: [genid] {} applies only to a cache node with upstream",
                    key.split(' ').next().unwrap()
                )
            );
        }
    }

    #[test]
    fn genid_source_claims_do_not_apply_to_a_cache_node() {
        let auth = "\n[auth]\nbackend = \"m6-html\"\n";
        let (_d, r) = load_cache_with(
            &format!(
                "{auth}\n[genid]\nupstream = \"origin\"\ntoken_file = \"{{DIR}}/genid-token\"\nread_require = \"role:monitor\"\nwrite_require = \"role:editor\"\n"
            ),
            "read-token",
        );
        assert_eq!(
            r.unwrap_err().to_string(),
            "config error: [genid] read_require applies only to the source, which has no upstream"
        );
    }

    #[test]
    fn genid_cache_node_loads_with_the_default_heartbeat() {
        let (_d, r) = load_cache_with(
            "\n[genid]\nupstream = \"origin\"\ntoken_file = \"{DIR}/genid-token\"\n",
            "  read-token\n",
        );
        let cfg = r.expect("a cache node loads");
        let g = cfg.genid.as_ref().expect("[genid] present");
        assert_eq!(g.upstream.as_deref(), Some("origin"));
        assert_eq!(
            g.token.as_deref(),
            Some("read-token"),
            "the file is trimmed"
        );
        assert_eq!(
            g.heartbeat,
            std::time::Duration::from_secs(crate::genid::DEFAULT_HEARTBEAT_SECS)
        );
        assert_eq!(g.read_require, None);
        assert_eq!(g.write_require, None);

        // The token is a credential: `--dump-config` must never print it.
        let dumped = serde_json::to_string(&cfg).expect("serialise");
        assert!(!dumped.contains("read-token"), "{dumped}");
    }

    #[test]
    fn genid_cache_node_reads_heartbeat_secs() {
        let (_d, r) = load_cache_with(
            "\n[genid]\nupstream = \"origin\"\ntoken_file = \"{DIR}/genid-token\"\nheartbeat_secs = 3\n",
            "read-token",
        );
        let g = r
            .expect("a cache node loads")
            .genid
            .expect("[genid] present");
        assert_eq!(g.heartbeat, std::time::Duration::from_secs(3));
        assert_eq!(g.upstream.as_deref(), Some("origin"));
    }

    #[test]
    fn health_keys_in_site_toml_are_refused() {
        let (_d, r) = load_with(
            "\n[auth]\nbackend = \"m6-html\"\n\n[health]\nmetrics_require = \"role:site\"\nmetrics_peers = [\"10.0.0.1\"]\n",
            "\n[health]\nmetrics_require = \"role:system\"\nmetrics_peers = [\"10.0.0.2\"]\n",
        );
        let e = r.unwrap_err().to_string();
        assert!(e.contains("site.toml sets [health].metrics_require"), "{e}");
        assert!(e.contains("site.toml sets [health].metrics_peers"), "{e}");
        assert!(e.contains("belongs in the system config"), "{e}");
    }

    #[test]
    fn a_system_health_key_alone_keeps_the_other_from_the_site() {
        let (_d, r) = load_with(
            "\n[auth]\nbackend = \"m6-html\"\n\n[health]\nmetrics_require = \"role:site\"\n",
            "\n[health]\nmetrics_peers = [\"10.0.0.2\"]\n",
        );
        let e = r.unwrap_err().to_string();
        assert!(e.contains("site.toml sets [health].metrics_require"), "{e}");
        assert!(e.contains("belongs in the system config"), "{e}");
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
        let mut site = minimal_site_toml();
        site.push_str("\n[node]\nname = \"test\"\n\n[server]\nbind = \"127.0.0.1:8443\"\ntls_cert = \"{DIR}/cert.pem\"\ntls_key = \"{DIR}/key.pem\"\n\n[auth]\nbackend = \"m6-html\"\n\n[health]\nperf_path = \"/p\"\nmetrics_peers = [\"10.0.0.5\"]\n");
        write_file(dir.path(), "site.toml", &site);
        let same = dir.path().join("site.toml");
        let cfg = load(dir.path(), &same).expect("one file for both loads");
        assert_eq!(cfg.auth.expect("auth").backend, "m6-html");
        assert_eq!(cfg.health.perf_path, "/p");
        assert_eq!(cfg.health.metrics_peer_ips.map(|p| p.len()), Some(1));
    }

    // --- misplaced_keys and the removed legacy metrics key ---

    #[test]
    fn metrics_token_file_in_site_toml_is_refused() {
        let dir = make_test_dir();
        setup_minimal(dir.path());
        let site_token = dir.path().join("site-token");
        std::fs::write(&site_token, "from-site\n").unwrap();
        let mut site = minimal_site_toml();
        site.push_str(&format!(
            "\n[health]\nmetrics_token_file = \"{}\"\n",
            site_token.display()
        ));
        write_file(dir.path(), "site.toml", &site);
        let e = load(dir.path(), &dir.path().join("system.toml"))
            .unwrap_err()
            .to_string();
        // The key is removed outright: neither file accepts it any more, and
        // the error names it so an operator can find the line.
        assert!(e.contains("metrics_token_file"), "{e}");
    }

    /// Write `site` and `system` into a fresh directory and name the misplaced keys.
    fn misplaced(site: &str, system: &str) -> Vec<String> {
        let site_table: toml::Table = toml::from_str(site).unwrap_or_default();
        let system_table: toml::Table = toml::from_str(system).unwrap_or_default();
        misplaced_keys(&site_table, &system_table, Path::new("system.toml"))
    }

    #[test]
    fn misplaced_keys_names_a_bind_in_site_toml() {
        let m = misplaced(
            "[server]\nbind = \"0.0.0.0:443\"\n\n[health]\nmetrics_peers = [\"10.0.0.1\"]\n",
            "[server]\nbind = \"0.0.0.0:443\"\n",
        );
        assert_eq!(m.len(), 2, "{m:?}");
        assert!(m[0].starts_with("site.toml sets [server].bind, which is this node's own state"));
        assert!(m[0].contains("belongs in the system config "), "{}", m[0]);
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
    fn one_file_mode_reads_node_keys_from_the_one_file() {
        let dir = make_test_dir();
        setup_minimal(dir.path());
        let one = "[site]\nname = \"Test\"\ndomain = \"test.example.com\"\n\n[server]\nbind = \"127.0.0.1:8443\"\ntls_cert = \"{DIR}/cert.pem\"\ntls_key = \"{DIR}/key.pem\"\nallowed_methods = [\"GET\"]\n\n[node]\nname = \"test\"\n\n[[backend]]\nname = \"m6-html\"\nsockets = \"/run/m6/*.sock\"\n\n[auth]\nbackend = \"m6-html\"\n";
        write_file(dir.path(), "site.toml", one);
        std::fs::write(dir.path().join("cert.pem"), "").unwrap();
        std::fs::write(dir.path().join("key.pem"), "").unwrap();
        let cfg = load(dir.path(), &dir.path().join("site.toml")).expect("one-file load");
        assert_eq!(cfg.node.name, "test");
        assert_eq!(cfg.server.bind, "127.0.0.1:8443");
        assert!(cfg.auth.is_some());
    }

    #[test]
    fn misplaced_keys_is_silent_for_a_clean_pair() {
        let m = misplaced(
            "[server]\nallowed_methods = [\"GET\"]\nbackend_timeout_secs = 5\n\n[health]\nperf_path = \"/p\"\n",
            "[server]\nbind = \"0.0.0.0:443\"\n\n[auth]\nbackend = \"m6-html\"\n\n[health]\nmetrics_require = \"role:monitor\"\n",
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
    fn a_system_config_without_node_is_refused() {
        let dir = make_test_dir();
        setup_minimal(dir.path());
        write_file(
            dir.path(),
            "system.toml",
            "[server]\nbind = \"127.0.0.1:8443\"\ntls_cert = \"{DIR}/cert.pem\"\ntls_key = \"{DIR}/key.pem\"\n",
        );
        let err = load(dir.path(), &dir.path().join("system.toml"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("has no [node]"), "{err}");
    }

    #[test]
    fn one_file_mode_without_node_is_refused() {
        let dir = make_test_dir();
        let one = "[site]\nname = \"Test\"\ndomain = \"test.example.com\"\n\n[server]\nbind = \"127.0.0.1:8443\"\ntls_cert = \"{DIR}/cert.pem\"\ntls_key = \"{DIR}/key.pem\"\n";
        write_file(dir.path(), "site.toml", one);
        write_file(dir.path(), "cert.pem", "");
        write_file(dir.path(), "key.pem", "");
        let err = load(dir.path(), &dir.path().join("site.toml"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("has no [node]"), "{err}");
    }

    /// Every key `misplaced_keys` knows, as (section, key, belongs in system config).
    /// A whole-section entry has no key.
    fn every_placed_key() -> Vec<(&'static str, Option<&'static str>, bool)> {
        let mut all = Vec::new();
        all.extend(NODE_SERVER_KEYS.iter().map(|k| ("server", Some(*k), true)));
        all.extend(NODE_HEALTH_KEYS.iter().map(|k| ("health", Some(*k), true)));
        all.extend(SITE_SERVER_KEYS.iter().map(|k| ("server", Some(*k), false)));
        all.extend(NODE_SECTIONS.iter().map(|s| (*s, None, true)));
        all
    }

    #[test]
    fn every_misplaced_key_is_reported_in_the_wrong_file_and_not_in_the_right_one() {
        let all = every_placed_key();
        // Every field of `RawServerSection` and `RawSystemHealth` is placed
        // in one list or another. Dropping a key from a list fails here.
        let mut server: Vec<&str> = NODE_SERVER_KEYS
            .iter()
            .chain(SITE_SERVER_KEYS)
            .copied()
            .collect();
        server.sort_unstable();
        assert_eq!(
            server,
            [
                "allowed_methods",
                "backend_timeout_secs",
                "bind",
                "h2c_bind",
                "internal_binds",
                "internal_peers",
                "lock_memory",
                "max_requests_per_connection",
                "process_index",
                "redirect_bind",
                "redirect_unmatched_to",
                "reuse_port",
                "reuse_port_processes",
                "served_hosts",
                "status_bind",
                "tls_cert",
                "tls_key",
                "warm_on_start",
            ]
        );
        let mut health = NODE_HEALTH_KEYS.to_vec();
        health.sort_unstable();
        assert_eq!(health, ["metrics_peers", "metrics_require"]);
        assert_eq!(NODE_SECTIONS, ["auth", "cache", "genid", "limits", "node"]);
        // 13 base [server] keys + lock_memory (the mimalloc pin) + served_hosts
        // and redirect_unmatched_to (the unmatched-host policy) = 16, plus the
        // two site-wide [server] keys, the two [health] keys and the five
        // node sections.
        assert_eq!(all.len(), 18 + 2 + 5);
        for (section, key, belongs_in_system) in all {
            let body = match key {
                Some(k) => format!("[{section}]\n{k} = 1\n"),
                None => format!("[{section}]\nname = 1\n"),
            };
            let name = match key {
                Some(k) => format!("[{section}].{k}"),
                None => format!("[{section}]"),
            };
            let (wrong_site, wrong_system, right_site, right_system) = if belongs_in_system {
                (body.as_str(), "", "", body.as_str())
            } else {
                ("", body.as_str(), body.as_str(), "")
            };
            let wrong = misplaced(wrong_site, wrong_system);
            assert_eq!(wrong.len(), 1, "{name} in the wrong file: {wrong:?}");
            assert!(
                wrong[0].contains(&format!("sets {name},"))
                    || wrong[0].contains(&format!("sets {name} ")),
                "{name}: {}",
                wrong[0]
            );
            let right = misplaced(right_site, right_system);
            assert!(right.is_empty(), "{name} in the right file: {right:?}");
        }
    }

    #[test]
    fn an_unknown_key_is_refused_and_named() {
        let sys = "[node]\nname = \"test\"\n\n[server]\nbind = \"127.0.0.1:8443\"\ntls_cert = \"{DIR}/cert.pem\"\ntls_key = \"{DIR}/key.pem\"\n";
        for (site, system, key) in [
            ("[site]\nname = \"Test\"\ndomain = \"test.example.com\"\n", format!("{sys}bnid = \"x\"\n"), "bnid"),
            ("[site]\nname = \"Test\"\ndomain = \"test.example.com\"\ntitel = \"x\"\n", sys.to_string(), "titel"),
            ("[site]\nname = \"Test\"\ndomain = \"test.example.com\"\n\n[[backend]]\nname = \"b\"\nsockets = \"/run/m6/*.sock\"\n\n[[route]]\npath = \"/\"\nbackend = \"b\"\nbakend = \"x\"\n", sys.to_string(), "bakend"),
            ("[site]\nname = \"Test\"\ndomain = \"test.example.com\"\n", format!("{sys}\n[logs]\nlevel = \"info\"\n"), "logs"),
            ("[site]\nname = \"Test\"\ndomain = \"test.example.com\"\n\n[log]\nformat = \"text\"\n", sys.to_string(), "format"),
        ] {
            let dir = make_test_dir();
            write_file(dir.path(), "cert.pem", "");
            write_file(dir.path(), "key.pem", "");
            write_file(dir.path(), "site.toml", site);
            write_file(dir.path(), "system.toml", &system);
            let e = format!("{:#}", load(dir.path(), &dir.path().join("system.toml")).unwrap_err());
            assert!(e.contains(key), "expected {key:?} in: {e}");
        }
    }

    #[test]
    fn allowed_methods_in_the_system_config_is_refused() {
        let e = load_error("allowed_methods = [\"GET\"]\n");
        assert!(
            e.contains("the system config sets [server].allowed_methods"),
            "{e}"
        );
    }

    #[test]
    fn an_unknown_log_level_is_refused() {
        let (_d, r) = load_with("[log]\nlevel = \"verbose\"\n", "");
        let e = r.unwrap_err().to_string();
        assert!(e.contains("unknown log level"), "{e}");
    }

    /// site.toml's `[log]` is read into m6-core's `LogConfig`, the same type a
    /// service's own config uses, and that value is what main hands the logger.
    /// A second declaration in this crate used to hold the same table.
    #[test]
    fn a_site_toml_log_level_reaches_the_one_log_config_type() {
        let (_d, r) = load_with("[log]\nlevel = \"warn\"\n", "");
        let cfg = r.unwrap();
        let level = cfg.log.level.as_deref().expect("site.toml set a level");
        assert_eq!(level, "warn");
        assert_eq!(
            m6_core::log::parse_level(level).unwrap(),
            m6_core::log::Level::Warn
        );
    }

    /// site.toml is the bottom of the precedence chain, so an absent level
    /// there is `info`, not "unset". `--dump-config` printed this value before
    /// the type moved and still prints it now.
    #[test]
    fn an_absent_site_toml_log_level_is_info() {
        let (_d, r) = load_with("", "");
        assert_eq!(r.unwrap().log.level.as_deref(), Some("info"));
    }

    #[test]
    fn the_removed_metrics_token_file_is_refused_in_the_system_config_too() {
        let dir = make_test_dir();
        setup_minimal(dir.path());
        let mut sys = minimal_system_toml();
        sys.push_str("\n[health]\nmetrics_token_file = \"{DIR}/missing\"\n");
        write_file(dir.path(), "system.toml", &sys);
        let e = load(dir.path(), &dir.path().join("system.toml"))
            .unwrap_err()
            .to_string();
        // Removed, not read: the key is unknown in both files now.
        assert!(e.contains("metrics_token_file"), "{e}");
    }

    /// A route's `cache` is a `Cache-Control` value the edge sends in place of
    /// the backend's, so it loads on a `[[route]]` and a `[[route_group]]`.
    #[test]
    fn a_route_cache_key_loads_on_a_route_and_a_route_group() {
        let (_d, r) = load_with(
            "[[route]]\npath = \"/x\"\nbackend = \"m6-html\"\ncache = \"no-store\"\n\n\
             [[route_group]]\nglob = \"content/*.md\"\npath = \"/{stem}\"\nbackend = \"m6-html\"\ncache = \"no-cache\"\n",
            "",
        );
        let config = r.unwrap();
        let route = config.routes.iter().find(|r| r.path == "/x").unwrap();
        assert_eq!(route.cache.as_deref(), Some("no-store"));
        let group = config.route_groups.iter().find(|g| g.path == "/{stem}");
        assert_eq!(group.unwrap().cache.as_deref(), Some("no-cache"));
    }

    /// A value a header cannot carry is refused at load, naming the route.
    #[test]
    fn an_unusable_route_cache_is_refused_and_named() {
        for bad in ["\"\"", "\"no-store\\r\\nX: y\""] {
            let (_d, r) = load_with(
                &format!("[[route]]\npath = \"/x\"\nbackend = \"m6-html\"\ncache = {bad}\n"),
                "",
            );
            let e = r.unwrap_err().to_string();
            assert!(e.contains("[[route]] /x"), "{e}");
            assert!(e.contains("Cache-Control"), "{e}");
        }
    }

    // ── [cache] dir ────────────────────────────────────────────────────────

    /// The site and system config of a cache node, ready for a `[cache]`
    /// table to be appended to the system file. The token file exists, and the
    /// origin is a URL backend.
    fn setup_cache_node(dir: &Path) {
        write_file(
            dir,
            "site.toml",
            "[site]\nname = \"Test\"\ndomain = \"test.example.com\"\n\n\
             [[backend]]\nname = \"origin\"\nurl = \"https://127.0.0.1:9443\"\ntls_skip_verify = true\n\n\
             [[route]]\npath = \"/\"\nbackend = \"origin\"\n",
        );
        write_file(
            dir,
            "system.toml",
            "[node]\nname = \"edge-a\"\n\n\
             [server]\nbind = \"127.0.0.1:8443\"\ntls_cert = \"{DIR}/cert.pem\"\ntls_key = \"{DIR}/key.pem\"\n\n\
             [genid]\nupstream = \"origin\"\ntoken_file = \"{DIR}/genid-token\"\n",
        );
        write_file(dir, "cert.pem", "dummy");
        write_file(dir, "key.pem", "dummy");
        write_file(dir, "genid-token", "read-token\n");
    }

    fn append_to_system(dir: &Path, extra: &str) {
        let path = dir.join("system.toml");
        let mut text = std::fs::read_to_string(&path).unwrap();
        text.push_str(extra);
        std::fs::write(&path, text).unwrap();
    }

    /// On a cache node `[cache] dir` is read into `Config::cache_dir`, and the
    /// start-time probe leaves no file behind.
    ///
    /// Break: skip the probe removal, and `.m6-probe` stays in the directory.
    #[test]
    fn a_cache_node_reads_its_disk_directory() {
        let dir = make_test_dir();
        setup_cache_node(dir.path());
        let cache = dir.path().join("cache");
        std::fs::create_dir_all(&cache).unwrap();
        append_to_system(
            dir.path(),
            &format!("\n[cache]\ndir = \"{}\"\n", cache.display()),
        );
        let cfg = load(dir.path(), &dir.path().join("system.toml")).unwrap();
        assert_eq!(cfg.cache_dir.as_deref(), Some(cache.as_path()));
        assert!(
            !cache.join(".m6-probe").exists(),
            "the write probe file must be removed at start"
        );
    }

    /// A relative directory is refused by name.
    ///
    /// Break: accept it, and the writer resolves it against the process's
    /// working directory instead of the deployment's.
    #[test]
    fn a_relative_cache_dir_is_refused() {
        let dir = make_test_dir();
        setup_cache_node(dir.path());
        append_to_system(dir.path(), "\n[cache]\ndir = \"cache\"\n");
        let e = load(dir.path(), &dir.path().join("system.toml"))
            .unwrap_err()
            .to_string();
        assert_eq!(e, "config error: [cache].dir must be an absolute path");
    }

    /// A source may not have one. Its cache sits beside its backends, so a
    /// warm restart is worth least there.
    ///
    /// Break: accept [cache] dir on a source, and `[cache] dir` is documented
    /// for a node that never writes one.
    #[test]
    fn a_cache_dir_on_a_source_is_refused() {
        let dir = make_test_dir();
        setup_minimal(dir.path());
        let cache = dir.path().join("cache");
        append_to_system(
            dir.path(),
            &format!("\n[cache]\ndir = \"{}\"\n", cache.display()),
        );
        let e = load(dir.path(), &dir.path().join("system.toml"))
            .unwrap_err()
            .to_string();
        assert_eq!(
            e,
            "config error: [cache].dir is for a cache node ([genid] upstream)"
        );
    }

    /// The directory must exist and be writable by this process. A path that
    /// is a file, and a path that is missing, both fail the start with the io
    /// error and the path.
    ///
    /// Break: create the directory instead of probing it, and a typo in the
    /// path starts a node that writes its cache somewhere nobody looks.
    #[test]
    fn an_unusable_cache_dir_is_refused_with_the_io_error_and_path() {
        let dir = make_test_dir();
        setup_cache_node(dir.path());
        let file = dir.path().join("not-a-dir");
        std::fs::write(&file, "x").unwrap();
        append_to_system(
            dir.path(),
            &format!("\n[cache]\ndir = \"{}\"\n", file.display()),
        );
        let e = load(dir.path(), &dir.path().join("system.toml"))
            .unwrap_err()
            .to_string();
        assert!(
            e.starts_with(&format!("config error: [cache].dir {}: ", file.display())),
            "{e}"
        );

        let missing = dir.path().join("missing/deeper");
        let mut sys = std::fs::read_to_string(dir.path().join("system.toml")).unwrap();
        sys = sys.replace(
            &format!("dir = \"{}\"", file.display()),
            &format!("dir = \"{}\"", missing.display()),
        );
        std::fs::write(dir.path().join("system.toml"), sys).unwrap();
        let e = load(dir.path(), &dir.path().join("system.toml"))
            .unwrap_err()
            .to_string();
        assert!(
            e.starts_with(&format!(
                "config error: [cache].dir {}: ",
                missing.display()
            )),
            "{e}"
        );
    }

    /// A `[cache]` table with no `dir` is a mistake, not a node with the
    /// default memory-only cache.
    ///
    /// Break: treat it as `None`, and the operator's misspelt key silently
    /// does nothing.
    #[test]
    fn a_cache_table_without_dir_is_refused() {
        let dir = make_test_dir();
        setup_cache_node(dir.path());
        append_to_system(dir.path(), "\n[cache]\n");
        let e = load(dir.path(), &dir.path().join("system.toml"))
            .unwrap_err()
            .to_string();
        assert_eq!(e, "config error: [cache] needs dir = \"<absolute path>\"");
    }

    /// `[cache]` belongs to the system config, because it is this node's own
    /// state.
    ///
    /// Break: accept it in site.toml, and a directory shared by every node is
    /// written by every node.
    #[test]
    fn a_cache_section_in_site_toml_is_refused() {
        let dir = make_test_dir();
        setup_cache_node(dir.path());
        let cache = dir.path().join("cache");
        std::fs::create_dir_all(&cache).unwrap();
        let mut site = std::fs::read_to_string(dir.path().join("site.toml")).unwrap();
        site.push_str(&format!("\n[cache]\ndir = \"{}\"\n", cache.display()));
        std::fs::write(dir.path().join("site.toml"), site).unwrap();
        let e = load(dir.path(), &dir.path().join("system.toml"))
            .unwrap_err()
            .to_string();
        assert!(e.contains("[cache]"), "{e}");
        assert!(e.contains("this node's own state"), "{e}");
        assert!(e.contains("belongs in the system config"), "{e}");
    }
}
