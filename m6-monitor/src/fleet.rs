//! Which nodes there are, and how to reach them.
//!
//! A fleet is configuration, not code. The nodes, their names and roles all come
//! from the config file, and nothing here knows any particular deployment.
//!
//! # Read and resolved once, at start
//!
//! The fleet is read once and every node's host name is resolved once, both at
//! start with blocking calls. An address that changes later needs a restart, and
//! a name that does not resolve stops the start with exit 2: while serving, the
//! loop never looks a name up.

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;

use m6_core::client::TlsClient;
use serde::{Deserialize, Serialize};

/// One node to poll.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Node {
    /// Short name for the report. Not necessarily the node's own `node.name`,
    /// which is whatever that node calls itself and is reported separately so
    /// a mismatch is visible.
    pub name: String,
    /// Base URL, scheme and authority.
    ///
    /// The node's public address. A header check is fetched from here, because
    /// what a visitor receives is what the check asserts on, and the URL's host
    /// name is the name the node's certificate is checked against. `/health` is
    /// fetched from here for a node polled as one address, and from the
    /// process's own status port for a node polled as several.
    /// The metrics and traffic documents are read from the journal, so they
    /// never travel over the public interface. Polling a node through its own
    /// public edge also measures the edge rather than the node.
    pub url: String,
    /// Role, for the report only. A cache node legitimately has no socket
    /// pools and a low hit rate relative to origin, and reporting it next to
    /// origin without saying which is which invites the wrong conclusion.
    #[serde(default)]
    pub role: String,
    /// The WireGuard tunnel from this host to the node, if there is one.
    ///
    /// Absent means "no tunnel configured", which the report says out loud and
    /// never counts as a fault.
    #[serde(default)]
    pub tunnel: Option<Tunnel>,
    /// How many m6-http processes this node runs on its shared port, each with
    /// `[server] process_index` and `status_bind` set. Absent means the node is
    /// polled as one address, as always. Present, the node is polled as that
    /// many processes: process `i` at `tunnel.address` with its port raised by
    /// `i`, over the tunnel, and reported as `<name>/<i>`. `tunnel.address`
    /// is then the base, the status port of process 0. `1` is the same as
    /// absent.
    #[serde(default)]
    pub processes: Option<u32>,
    /// Set by `Fleet::from_config` on each entry a `processes` node expands to:
    /// the process index that entry polls. Never read from the file.
    #[serde(skip)]
    pub process: Option<u32>,
}

/// A WireGuard tunnel from the monitor host to one node.
///
/// ```toml
/// tunnel = { address = "192.0.2.2:443", public_key = "<peer public key>" }
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tunnel {
    /// An IP address and port on the tunnel, such as `192.0.2.2:443`. An IP
    /// literal, never a name, so a probe cannot depend on DNS.
    ///
    /// Point it at any TCP port the peer listens on over the tunnel. A closed
    /// port that answers with a reset still gives a round trip, and the report
    /// says `refused`. A port the peer's firewall drops gives "no answer".
    pub address: String,
    /// The peer's WireGuard public key, as `wg show` prints it.
    ///
    /// Without it the round trip is still measured, and the monitor reports a
    /// fault for this node because its handshake cannot be checked.
    #[serde(default)]
    pub public_key: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Fleet {
    pub nodes: Vec<Node>,
    /// The seconds between polls, read at load from the top level.
    ///
    /// Kept here so a default that depends on the cadence is computed from the
    /// same number the poller uses.
    #[serde(skip)]
    pub(crate) poll_interval_s: u64,
    /// How long to wait for one node before giving up on it.
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
    /// `[monitor] history_file`. Validated by `history`, and named here so
    /// the table's key list is complete: every other key under `[monitor]` is
    /// one of these fields, so an unknown one, a removed one included, stops
    /// the start.
    #[serde(default)]
    pub history_file: Option<String>,
    /// `[monitor] history_max_mb`, validated by `history` for the same reason.
    #[serde(default)]
    pub history_max_mb: Option<u64>,
    /// The directory mon's `systemd-journal-remote` writes each node's journal
    /// to. A relative path resolves against the monitor's site directory.
    ///
    /// The metrics and traffic documents are read from it, so this is the one
    /// source of a node's figures. The default is stock journal-remote's.
    #[serde(default = "default_journal_dir")]
    pub journal_dir: String,
    /// Whether this fleet is meant to serve one certificate. Default false.
    ///
    /// Read from `[monitor] one_certificate`. A fleet that runs one
    /// certificate per node is a supported shape, so the declaration is off
    /// unless the deployment makes it. With it on, a node whose leaf
    /// certificate fingerprint differs from the fleet's is a fault and not a
    /// judgement call, which is the only way the monitor can read a two-node
    /// fleet or two certificates that expire on the same day.
    ///
    /// `digest::Thresholds::one_certificate` carries the reasoning, including
    /// what a majority, the first-seen node and a quorum would each get wrong.
    #[serde(default)]
    pub one_certificate: bool,
    /// Response headers the fleet config expects on particular paths.
    ///
    /// **Declared by the deployment, not by m6.** A cache policy is a property of
    /// a site: `max-age=60, s-maxage=86400` is right for one page and wrong for
    /// another, and m6 has no business knowing which. Hardcoding the strings here
    /// would put one site's decisions inside a generic web system, which the
    /// `no_file_names_one_particular_deployment` test exists to prevent, and would
    /// drift from that site's config the first time it changed.
    ///
    /// Empty by default, so a fleet that declares none is checked for nothing and
    /// the report says so rather than silently passing.
    #[serde(default)]
    pub header_check: Vec<HeaderCheck>,
    /// A file holding the output of `wg show <interface> latest-handshakes`,
    /// written by a privileged timer, for example
    /// `wg show wg0 latest-handshakes > <file>.tmp && mv -f <file>.tmp <file>`.
    ///
    /// Required when any node has a `tunnel`, and the monitor reports a fault
    /// without it. **Never use `wg show ...
    /// dump`**: it includes the interface private key.
    #[serde(default)]
    pub wg_handshakes_file: Option<String>,
    /// Seconds since a peer's last WireGuard handshake above which the monitor
    /// reports a fault. Optional, default 600. With a tunnel configured it must be at
    /// least the top-level `poll_interval_s` plus `timeout_ms` rounded up to
    /// whole seconds plus 180, and loading the config fails otherwise.
    ///
    /// WireGuard rekeys about every 120 seconds while traffic flows, and the
    /// probe each poll is that traffic. The limit must also cover the wait for
    /// the poll that carries it, plus one rekey and a minute of slack.
    ///
    /// The time is the last SUCCESSFUL handshake, so it is the slow signal: it
    /// stops moving the moment a tunnel breaks and stays recent for minutes.
    /// The round trip to the tunnel address is the fast one.
    #[serde(default = "default_tunnel_handshake_max_age_s")]
    pub tunnel_handshake_max_age_s: u64,
    /// Seconds since a node's `/health` last answered 200 above which the
    /// check flags it. Optional, default three poll intervals.
    ///
    /// The age comes from the history, so the service shows it only when
    /// `history_file` is set. The check already faults a node that is
    /// degraded or unreachable, and the age says how long it has been so.
    /// A deployment that changes `poll_interval_s` keeps this limit in step
    /// without a second number to maintain.
    #[serde(default)]
    pub health_last_ok_max_age_s: Option<u64>,
    /// This monitor's environment: `dev`, `staging` or `prod`. Optional.
    ///
    /// It decides two rules from the start-up record's version, and nothing
    /// else: a dirty build is refused anywhere but `dev`, and a pre-release is
    /// refused on `prod`. The value is the deployment's declaration, like
    /// `journal_dir`, so no site name enters this crate.
    ///
    /// Absent, the monitor falls back to the `environment` in the deploy's
    /// expectation file. With neither, the two rules cannot be judged and the
    /// check says so once instead of guessing.
    #[serde(default)]
    pub environment: Option<String>,
    /// The deploy's record of what it expects on each node: a version and a
    /// config sha256 per app, plus whether a deploy is in progress. Optional.
    ///
    /// The deploy writes it and the monitor only reads it. The path comes from
    /// this key and is resolved against the site directory, so no deployment
    /// path is compiled into the crate.
    ///
    /// The shape is documented in `docs/m6-monitor.md` section 5: JSON, a
    /// `nodes` map, an `apps` map under each node, and a `version`, `config`
    /// and optional `decided` block under each app. A file that does not exist
    /// yet is a state the check reports as "waiting for the deploy side",
    /// never a refusal to start.
    #[serde(default)]
    pub expected_deploy_file: Option<String>,
}

/// The seconds between polls, read from the top level of the service config.
///
/// The poller and the handshake-limit check both call this, so they cannot
/// disagree. `get` looks a top-level key up and returns it when it is a
/// non-negative integer. The config reaches the two callers in different
/// forms, so they pass different lookups. Default 30.
pub fn poll_interval_s(get: impl Fn(&str) -> Option<u64>) -> u64 {
    get("poll_interval_s").unwrap_or(crate::DEFAULT_POLL_SECS)
}

/// How far `tunnel_handshake_max_age_s` must exceed `poll_interval_s` plus the
/// probe timeout rounded up to whole seconds, in seconds.
pub const MIN_HANDSHAKE_MARGIN_S: u64 = 180;

fn default_tunnel_handshake_max_age_s() -> u64 {
    crate::digest::DEFAULT_TUNNEL_HANDSHAKE_AGE_S
}

/// One "this path must answer with this header" assertion.
///
/// The failure these catch is a response that is SERVED CORRECTLY and cached
/// wrongly: a page that should be revalidated pinned for a day, or an immutable
/// asset that is not, so a content change never reaches anyone. Both look
/// perfectly healthy to a status-code check, which is why the fleet report needs
/// its own.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct HeaderCheck {
    /// Path to request, relative to the node's base URL.
    pub path: String,
    /// Header to read. Case-insensitive, as HTTP field names are.
    pub header: String,
    /// The value it must have, compared exactly after trimming.
    pub expect: String,
    /// Which roles to check this on. Empty means every node.
    ///
    /// An origin and a cache node do not always agree: the origin states the
    /// policy and the edge may add to it. A check that is only meaningful on one
    /// of them should say so rather than be written to pass on both.
    #[serde(default)]
    pub roles: Vec<String>,
    /// Append a unique query string, so a cached copy cannot answer for a
    /// backend that has stopped being able to produce one.
    ///
    /// On by default. `s-maxage=86400` means a stale entry keeps answering 200
    /// for a day after the thing behind it broke, and a check that accepts that
    /// answer is checking the cache rather than the site.
    #[serde(default = "default_true")]
    pub cache_bust: bool,
}

fn default_true() -> bool {
    true
}

fn default_timeout_ms() -> u64 {
    5_000
}

/// Replace each node that sets `processes = N` with N entries, one per
/// process, so the poller and every report treat a process as a node. Entry
/// `i` is named `<name>/<i>` and polls `tunnel.address` with its port raised by
/// `i`. A node without `processes`, or with `processes = 1`, passes through
/// as it is, which is today's single-address polling.
fn expand_processes(nodes: Vec<Node>) -> anyhow::Result<Vec<Node>> {
    let mut out = Vec::with_capacity(nodes.len());
    for n in nodes {
        let Some(count) = n.processes else {
            out.push(n);
            continue;
        };
        if count == 0 {
            anyhow::bail!("node {:?}: processes must be at least 1", n.name);
        }
        // One process is the node as it always was: the one address, no suffix.
        if count == 1 {
            out.push(Node {
                processes: None,
                ..n
            });
            continue;
        }
        if n.name.contains('/') {
            anyhow::bail!(
                "node {:?}: a node with processes cannot have '/' in its name, which separates the process index",
                n.name
            );
        }
        let Some(tunnel) = &n.tunnel else {
            anyhow::bail!(
                "node {:?}: processes needs tunnel.address, the status port of process 0 on the node's tunnel address",
                n.name
            );
        };
        let base: SocketAddr = tunnel.address.parse().map_err(|_| {
            anyhow::anyhow!(
                "node {:?}: tunnel.address {:?} is not an IP address and port, for example 192.0.2.2:9440",
                n.name,
                tunnel.address
            )
        })?;
        for i in 0..count {
            let port = u16::try_from(u32::from(base.port()) + i).map_err(|_| {
                anyhow::anyhow!(
                    "node {:?}: tunnel.address port {} plus process {i} is past 65535",
                    n.name,
                    base.port()
                )
            })?;
            let mut entry = n.clone();
            entry.name = format!("{}/{i}", n.name);
            entry.processes = None;
            entry.process = Some(i);
            if let Some(t) = entry.tunnel.as_mut() {
                t.address = SocketAddr::new(base.ip(), port).to_string();
            }
            out.push(entry);
        }
    }
    Ok(out)
}

/// Where `systemd-journal-remote` keeps per-node journals by default.
fn default_journal_dir() -> String {
    "/var/log/journal/remote".to_string()
}

impl Fleet {
    /// The journal directory, resolved against the monitor's site directory.
    ///
    /// A missing directory is refused at start: a monitor that silently reads
    /// an empty journal reports every node as having no metrics, which reads
    /// like a fleet-wide fault instead of a misconfigured path.
    pub fn journal_path(&self, site_dir: &Path) -> anyhow::Result<std::path::PathBuf> {
        let path = std::path::PathBuf::from(&self.journal_dir);
        let path = if path.is_absolute() {
            path
        } else {
            site_dir.join(path)
        };
        if !path.is_dir() {
            anyhow::bail!(
                "[monitor] journal_dir {} is not a directory: create it, or point the key at the directory systemd-journal-remote writes each node's journal to",
                path.display()
            );
        }
        Ok(path)
    }

    /// The deploy's expectation file, resolved against the site directory.
    ///
    /// `None` when `[monitor] expected_deploy_file` is not set. A path that
    /// does not exist is not a refusal: the file is the deploy side's to
    /// write, and the check reports it as waiting rather than stopping the
    /// monitor.
    pub fn expected_deploy_path(&self, site_dir: &Path) -> Option<std::path::PathBuf> {
        let configured = self.expected_deploy_file.as_deref()?;
        let path = std::path::PathBuf::from(configured);
        Some(if path.is_absolute() {
            path
        } else {
            site_dir.join(path)
        })
    }

    /// Load from the service's own config file.
    ///
    /// The fleet lives under `[monitor]` in the ordinary m6 config, so a
    /// monitor is configured the way every other m6 service is.
    pub fn from_config(config_path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(config_path)?;
        let value: toml::Value = toml::from_str(&text)?;
        let section = value
            .get("monitor")
            .ok_or_else(|| anyhow::anyhow!("no [monitor] section in {}", config_path.display()))?;
        let mut fleet: Fleet = section.clone().try_into()?;
        if fleet.nodes.is_empty() {
            anyhow::bail!("[monitor] lists no nodes");
        }
        // The top-level key, through the function the poller uses.
        let poll_interval_s = poll_interval_s(|k| {
            value
                .get(k)
                .and_then(|v| v.as_integer())
                .and_then(|i| u64::try_from(i).ok())
        });
        let timeout_s = fleet.timeout_ms.div_ceil(1000);
        let min = poll_interval_s
            .saturating_add(timeout_s)
            .saturating_add(MIN_HANDSHAKE_MARGIN_S);
        let has_tunnel = fleet.nodes.iter().any(|n| n.tunnel.is_some());
        if has_tunnel && fleet.tunnel_handshake_max_age_s < min {
            anyhow::bail!(
                "[monitor] tunnel_handshake_max_age_s = {} is too low for the top-level \
                 poll_interval_s = {poll_interval_s} and timeout_ms = {}: \
                 it must be at least poll_interval_s + ceil(timeout_ms / 1000) \
                 + {MIN_HANDSHAKE_MARGIN_S} = {min}, \
                 because WireGuard rekeys every 120 seconds only while traffic flows \
                 and the probe each poll is that traffic",
                fleet.tunnel_handshake_max_age_s,
                fleet.timeout_ms
            );
        }
        fleet.nodes = expand_processes(std::mem::take(&mut fleet.nodes))?;
        for n in &fleet.nodes {
            if let Some(t) = &n.tunnel {
                if t.address.parse::<std::net::SocketAddr>().is_err() {
                    anyhow::bail!(
                        "node {:?}: tunnel.address {:?} is not an IP address and port, for example 192.0.2.2:443",
                        n.name,
                        t.address
                    );
                }
            }
        }
        fleet.poll_interval_s = poll_interval_s;
        Ok(fleet)
    }

    /// Seconds a node's last `/health` 200 may be old before the check flags
    /// it: the configured limit, or three poll intervals.
    pub fn health_last_ok_limit_s(&self) -> u64 {
        self.health_last_ok_max_age_s.unwrap_or_else(|| {
            self.poll_interval_s
                .saturating_mul(crate::digest::HEALTH_LAST_OK_POLLS)
        })
    }
}

/// One node as the loop reaches it: its addresses resolved and its TLS client
/// built, all at start.
pub struct ResolvedNode {
    /// Short name for the report, as configured.
    pub name: String,
    pub role: String,
    /// The configured base URL, for error text and the `Host` header.
    pub url: String,
    /// Where `/health` and the header checks go: the node's public address.
    pub addr: SocketAddr,
    /// The tunnel, when one is configured.
    pub tunnel: Option<ResolvedTunnel>,
    /// The process index this entry polls, when the node was expanded from
    /// `processes`.
    pub process: Option<u32>,
    /// Where every step but a header check goes for a process entry: its
    /// status port. `None` for an ordinary node.
    pub status_addr: Option<SocketAddr>,
    /// The TLS client for an `https` URL. The name is the URL's host in both
    /// cases: the tunnel carries TLS for the same certificate.
    pub tls: Option<TlsClient>,
    /// The URL's path prefix, empty for an authority-only URL.
    pub base_path: String,
    /// The authority as the URL writes it, for the `Host` header.
    pub authority: String,
}

/// A node's tunnel address, parsed once at start.
pub struct ResolvedTunnel {
    pub addr: SocketAddr,
    /// As configured, for the report.
    pub address: String,
    pub public_key: Option<String>,
}

/// A node URL, split into what a connection needs.
#[derive(Debug)]
struct NodeUrl {
    https: bool,
    host: String,
    port: u16,
    authority: String,
    base_path: String,
}

impl Fleet {
    /// Resolve every node's host once and build its TLS client, blocking.
    ///
    /// `tls` is the one shared client config built at start from the host's
    /// certificate store. A name that does not resolve is an error naming the
    /// node and the host, so the service refuses to start rather than report
    /// the node unreachable at every poll.
    pub fn resolve(&self, tls: &Arc<rustls::ClientConfig>) -> anyhow::Result<Vec<ResolvedNode>> {
        let mut out = Vec::with_capacity(self.nodes.len());
        for n in &self.nodes {
            let u =
                parse_node_url(&n.url).map_err(|e| anyhow::anyhow!("node {:?}: {e}", n.name))?;
            let addr = m6_core::net::resolve(&u.host, u.port).map_err(|e| {
                anyhow::anyhow!("node {:?}: {} does not resolve: {e}", n.name, u.host)
            })?;
            let tunnel = match &n.tunnel {
                Some(t) => Some(ResolvedTunnel {
                    addr: t.address.parse::<SocketAddr>().map_err(|e| {
                        anyhow::anyhow!("node {:?}: tunnel.address {:?}: {e}", n.name, t.address)
                    })?,
                    address: t.address.clone(),
                    public_key: t.public_key.clone(),
                }),
                None => None,
            };
            let tls_client = if u.https {
                let name =
                    rustls::pki_types::ServerName::try_from(u.host.clone()).map_err(|e| {
                        anyhow::anyhow!("node {:?}: invalid host name {:?}: {e}", n.name, u.host)
                    })?;
                Some(TlsClient {
                    config: Arc::clone(tls),
                    name,
                })
            } else {
                None
            };
            out.push(ResolvedNode {
                name: n.name.clone(),
                role: n.role.clone(),
                url: n.url.clone(),
                addr,
                process: n.process,
                status_addr: n.process.and(tunnel.as_ref()).map(|t| t.addr),
                tunnel,
                tls: tls_client,
                base_path: u.base_path,
                authority: u.authority,
            });
        }
        Ok(out)
    }
}

/// Split `http://host[:port][/prefix]` or `https://...` into what a request
/// needs. A node URL names an authority and an optional path prefix, never a
/// query or a fragment.
fn parse_node_url(url: &str) -> anyhow::Result<NodeUrl> {
    let (scheme, rest) = url
        .split_once("://")
        .ok_or_else(|| anyhow::anyhow!("{url:?} is not an http:// or https:// URL"))?;
    let https = match scheme {
        "http" => false,
        "https" => true,
        _ => anyhow::bail!("{url:?} is not an http:// or https:// URL"),
    };
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    if authority.is_empty() || authority.contains(['@', '?', '#']) {
        anyhow::bail!("{url:?} has no plain host authority");
    }
    let (host, port) = split_host_port(authority, if https { 443 } else { 80 })?;
    if host.is_empty() {
        anyhow::bail!("{url:?} has no host");
    }
    Ok(NodeUrl {
        https,
        host,
        port,
        authority: authority.to_string(),
        base_path: path.trim_end_matches('/').to_string(),
    })
}

/// The host and port of an authority, with the bracket form for IPv6.
fn split_host_port(authority: &str, default_port: u16) -> anyhow::Result<(String, u16)> {
    let bad_port = |p: &str| anyhow::anyhow!("{authority:?} has an invalid port {p:?}");
    if let Some(rest) = authority.strip_prefix('[') {
        let (host, tail) = rest
            .split_once(']')
            .ok_or_else(|| anyhow::anyhow!("{authority:?} has an unclosed IPv6 address"))?;
        let port = match tail.strip_prefix(':') {
            Some(p) => p.parse::<u16>().map_err(|_| bad_port(p))?,
            None if tail.is_empty() => default_port,
            None => anyhow::bail!("{authority:?} has text after the IPv6 address"),
        };
        return Ok((host.to_string(), port));
    }
    match authority.rsplit_once(':') {
        Some((host, p)) => Ok((host.to_string(), p.parse::<u16>().map_err(|_| bad_port(p))?)),
        None => Ok((authority.to_string(), default_port)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn reads_a_fleet_from_an_ordinary_m6_config() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        write!(
            f,
            r#"
site_name = "irrelevant"

[monitor]
timeout_ms = 2000

[[monitor.nodes]]
name = "origin"
url  = "http://origin.example.com:8080"
role = "origin"

[[monitor.nodes]]
name = "edge-a"
url  = "http://edge-b.example.com:8080"
role = "cache"
"#
        )
        .unwrap();
        let fleet = Fleet::from_config(f.path()).unwrap();
        assert_eq!(fleet.nodes.len(), 2);
        assert_eq!(fleet.nodes[0].name, "origin");
        assert_eq!(fleet.nodes[0].role, "origin");
        assert_eq!(fleet.timeout_ms, 2000);
    }

    #[test]
    fn a_fleet_with_no_nodes_is_an_error_not_an_empty_report() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        write!(f, "[monitor]\ntimeout_ms = 1000\n").unwrap();
        assert!(Fleet::from_config(f.path()).is_err());
    }

    /// The journal directory is the one source of a node's figures, so its
    /// default is stock journal-remote's and a relative path resolves against
    /// the site directory.
    #[test]
    fn the_journal_directory_defaults_to_the_stock_remote_path() {
        let fleet =
            load("[[monitor.nodes]]\nname = \"a\"\nurl = \"http://origin.example.com\"\n").unwrap();
        assert_eq!(fleet.journal_dir, "/var/log/journal/remote");
        let relative = load(
            "[monitor]\njournal_dir = \"journal\"\n\n[[monitor.nodes]]\nname = \"a\"\nurl = \"http://origin.example.com\"\n",
        )
        .unwrap();
        assert_eq!(relative.journal_dir, "journal");
    }

    /// A directory that is not there is an error naming the key, which the
    /// service reports as a fault rather than refusing to start the monitor.
    #[test]
    fn a_missing_journal_directory_names_the_key() {
        let fleet = load(
            "[monitor]\njournal_dir = \"/no/such/journal\"\n\n[[monitor.nodes]]\nname = \"a\"\nurl = \"http://origin.example.com\"\n",
        )
        .unwrap();
        let error = fleet
            .journal_path(std::path::Path::new("/tmp"))
            .unwrap_err()
            .to_string();
        assert!(error.contains("[monitor] journal_dir"), "{error}");
    }

    fn load(text: &str) -> anyhow::Result<Fleet> {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        write!(f, "{text}").unwrap();
        Fleet::from_config(f.path())
    }

    #[test]
    fn a_node_with_a_tunnel_reads_its_address_and_key() {
        let fleet = load(
            r#"
[monitor]
wg_handshakes_file = "/var/lib/m6-monitor/wg-handshakes"
tunnel_handshake_max_age_s = 215

[[monitor.nodes]]
name = "edge-a"
url  = "https://edge-a.example.com"
tunnel = { address = "192.0.2.2:443", public_key = "KEY" }
"#,
        )
        .unwrap();
        let t = fleet.nodes[0].tunnel.as_ref().unwrap();
        assert_eq!(t.address, "192.0.2.2:443");
        assert_eq!(t.public_key.as_deref(), Some("KEY"));
        assert_eq!(
            fleet.wg_handshakes_file.as_deref(),
            Some("/var/lib/m6-monitor/wg-handshakes")
        );
        assert_eq!(fleet.tunnel_handshake_max_age_s, 215);
    }

    #[test]
    fn a_tunnel_address_that_is_not_ip_and_port_is_a_config_error() {
        for bad in ["edge-a.example.com:443", "192.0.2.2"] {
            let e = load(&format!(
                "[[monitor.nodes]]\nname = \"edge-a\"\nurl = \"http://x.example.com\"\n\
                 tunnel = {{ address = {bad:?} }}\n"
            ))
            .unwrap_err();
            assert!(e.to_string().contains("edge-a"), "{e}");
        }
    }

    #[test]
    fn a_handshake_limit_too_close_to_the_poll_interval_is_a_config_error() {
        let node = "[[monitor.nodes]]\nname = \"a\"\nurl = \"http://x.example.com\"\n\
                    tunnel = { address = \"192.0.2.2:443\", public_key = \"K\" }\n";
        for (poll, limit) in [(30, 90), (30, 214), (60, 244), (30, 0)] {
            let e = load(&format!(
                "poll_interval_s = {poll}\n[monitor]\ntunnel_handshake_max_age_s = {limit}\n{node}"
            ))
            .unwrap_err()
            .to_string();
            assert!(e.contains("tunnel_handshake_max_age_s"), "{e}");
            assert!(e.contains("poll_interval_s"), "{e}");
            assert!(e.contains(&format!("{}", poll + 185)), "{e}");
        }
        for (poll, limit) in [(30, 215), (60, 245), (30, 600)] {
            load(&format!(
                "poll_interval_s = {poll}\n[monitor]\ntunnel_handshake_max_age_s = {limit}\n{node}"
            ))
            .unwrap();
        }
        // The limit is about tunnels, so a fleet with none is not refused.
        load("poll_interval_s = 600\n[[monitor.nodes]]\nname = \"a\"\nurl = \"http://x.example.com\"\n")
            .unwrap();
        // A long poll interval with the default limit.
        let e = load(&format!("poll_interval_s = 600\n[monitor]\n{node}")).unwrap_err();
        assert!(e.to_string().contains("785"), "{e}");
    }

    #[test]
    fn the_handshake_floor_includes_the_probe_timeout() {
        let node = "[[monitor.nodes]]\nname = \"a\"\nurl = \"http://x.example.com\"\n\
                    tunnel = { address = \"192.0.2.2:443\", public_key = \"K\" }\n";
        let cfg = |poll: u64, timeout: u64, limit: u64| {
            load(&format!(
                "poll_interval_s = {poll}\n[monitor]\ntimeout_ms = {timeout}\n\
                 tunnel_handshake_max_age_s = {limit}\n{node}"
            ))
        };
        // 30 + ceil(2500 / 1000) + 180 = 213
        assert!(cfg(30, 2500, 212).is_err());
        cfg(30, 2500, 213).unwrap();
        // 60 + 20 + 180 = 260
        assert!(cfg(60, 20_000, 259).is_err());
        cfg(60, 20_000, 260).unwrap();
        let e = cfg(60, 20_000, 100).unwrap_err().to_string();
        for needle in ["poll_interval_s = 60", "timeout_ms = 20000", "= 260"] {
            assert!(e.contains(needle), "{needle}: {e}");
        }
    }

    #[test]
    fn the_handshake_check_reads_the_same_poll_interval_as_the_poller() {
        let node = "[[monitor.nodes]]\nname = \"a\"\nurl = \"http://x.example.com\"\n\
                    tunnel = { address = \"192.0.2.2:443\", public_key = \"K\" }\n";
        // The poller reads the top-level key, so a `[monitor]` one is refused
        // as an unknown key rather than silently ignored.
        let e = load(&format!("[monitor]\npoll_interval_s = 600\n{node}")).unwrap_err();
        assert!(e.to_string().contains("poll_interval_s"), "{e}");
        // The top-level key is the one the poller uses, so the check uses it too.
        let e = load(&format!("poll_interval_s = 600\n[monitor]\n{node}")).unwrap_err();
        assert!(e.to_string().contains("785"), "{e}");
        // One function reads both representations of the config.
        let toml_cfg: toml::Value =
            toml::from_str("poll_interval_s = 45\n[monitor]\npoll_interval_s = 9\n").unwrap();
        let json_cfg: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(r#"{"poll_interval_s": 45, "monitor": {"poll_interval_s": 9}}"#)
                .unwrap();
        assert_eq!(
            poll_interval_s(|k| toml_cfg
                .get(k)
                .and_then(|v| v.as_integer())
                .and_then(|i| u64::try_from(i).ok())),
            45
        );
        assert_eq!(
            poll_interval_s(|k| json_cfg.get(k).and_then(|v| v.as_u64())),
            45
        );
        assert_eq!(poll_interval_s(|_| None), crate::DEFAULT_POLL_SECS);
    }

    #[test]
    fn a_node_without_a_tunnel_has_none() {
        let fleet =
            load("[[monitor.nodes]]\nname = \"origin\"\nurl = \"http://origin.example.com\"\n")
                .unwrap();
        assert!(fleet.nodes[0].tunnel.is_none());
        assert!(fleet.wg_handshakes_file.is_none());
        assert_eq!(fleet.tunnel_handshake_max_age_s, 600);
    }

    /// The health limit is the one setting that has no literal default: it
    /// follows the poll cadence, so changing `poll_interval_s` keeps it true.
    #[test]
    fn the_health_limit_defaults_to_three_poll_intervals() {
        let node = "[[monitor.nodes]]\nname = \"origin\"\nurl = \"http://origin.example.com\"\n";
        let set = load(&format!(
            "poll_interval_s = 60\n[monitor]\nhealth_last_ok_max_age_s = 45\n{node}"
        ))
        .unwrap();
        assert_eq!(set.health_last_ok_max_age_s, Some(45));
        assert_eq!(set.health_last_ok_limit_s(), 45);
        let default = load(&format!("poll_interval_s = 60\n{node}")).unwrap();
        assert_eq!(default.health_last_ok_max_age_s, None);
        assert_eq!(default.health_last_ok_limit_s(), 180);
        let stock = load(node).unwrap();
        assert_eq!(stock.health_last_ok_limit_s(), 90);
    }

    #[test]
    fn a_node_url_splits_into_scheme_host_port_and_prefix() {
        let u = parse_node_url("https://edge-a.example.com").unwrap();
        assert!(u.https);
        assert_eq!((u.host.as_str(), u.port), ("edge-a.example.com", 443));
        assert_eq!(u.authority, "edge-a.example.com");
        assert_eq!(u.base_path, "");

        let u = parse_node_url("http://origin.example.com:8080/mon/").unwrap();
        assert!(!u.https);
        assert_eq!((u.host.as_str(), u.port), ("origin.example.com", 8080));
        assert_eq!(u.authority, "origin.example.com:8080");
        assert_eq!(u.base_path, "/mon");

        let u = parse_node_url("http://[::1]:8081").unwrap();
        assert_eq!((u.host.as_str(), u.port), ("::1", 8081));
        assert_eq!(u.authority, "[::1]:8081");
    }

    /// A URL that names nothing a socket can be opened to is a config error,
    /// and the message says which URL.
    #[test]
    fn a_url_that_is_not_http_or_has_no_host_is_refused() {
        for bad in [
            "edge-a.example.com",
            "ftp://edge-a.example.com",
            "https://",
            "https://edge-a.example.com:notaport",
            "https://[::1",
            "https://user@edge-a.example.com",
        ] {
            let e = parse_node_url(bad).unwrap_err().to_string();
            assert!(!e.is_empty(), "{bad}");
        }
    }

    /// Resolution happens at start: an IP literal needs no lookup, so a config
    /// that names addresses works with no resolver at all.
    #[test]
    fn resolving_an_ip_literal_node_needs_no_lookup() {
        let fleet =
            load("[[monitor.nodes]]\nname = \"a\"\nurl = \"http://127.0.0.1:9\"\n").unwrap();
        let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
        let config = Arc::new(
            rustls::ClientConfig::builder_with_provider(provider)
                .with_safe_default_protocol_versions()
                .expect("protocol versions")
                .with_root_certificates(rustls::RootCertStore::empty())
                .with_no_client_auth(),
        );
        let resolved = fleet.resolve(&config).unwrap();
        assert_eq!(resolved.len(), 1);
        let want: std::net::SocketAddr = "127.0.0.1:9".parse().unwrap();
        assert_eq!(resolved[0].addr, want);
        assert!(resolved[0].tls.is_none());
        assert_eq!(resolved[0].name, "a");
    }

    fn fleet_from(text: &str) -> anyhow::Result<Fleet> {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        write!(f, "{text}").unwrap();
        Fleet::from_config(f.path())
    }

    const THREE: &str =
        "[monitor]\n\n[[monitor.nodes]]\nname = \"edge-a\"\nurl = \"http://127.0.0.1:1\"\n\
         role = \"cache\"\nprocesses = 3\ntunnel = { address = \"192.0.2.2:9440\" }\n";

    #[test]
    fn a_node_with_processes_expands_to_one_entry_per_process() {
        let fleet = fleet_from(THREE).unwrap();
        let seen: Vec<(String, String, Option<u32>)> = fleet
            .nodes
            .iter()
            .map(|n| {
                (
                    n.name.clone(),
                    n.tunnel.as_ref().unwrap().address.clone(),
                    n.process,
                )
            })
            .collect();
        assert_eq!(
            seen,
            [
                (
                    "edge-a/0".to_string(),
                    "192.0.2.2:9440".to_string(),
                    Some(0)
                ),
                (
                    "edge-a/1".to_string(),
                    "192.0.2.2:9441".to_string(),
                    Some(1)
                ),
                (
                    "edge-a/2".to_string(),
                    "192.0.2.2:9442".to_string(),
                    Some(2)
                ),
            ]
        );
        // Every process entry polls its own status port for `/health`.
        assert!(fleet.nodes.iter().all(|n| n.process.is_some()));
    }

    /// One process is the node as it always was: same name, same address.
    #[test]
    fn processes_one_or_absent_leaves_the_node_as_it_is() {
        for line in ["processes = 1\n", ""] {
            let text = THREE.replace("processes = 3\n", line);
            let fleet = fleet_from(&text).unwrap();
            assert_eq!(fleet.nodes.len(), 1);
            let n = &fleet.nodes[0];
            assert_eq!(n.name, "edge-a");
            assert_eq!(n.tunnel.as_ref().unwrap().address, "192.0.2.2:9440");
            assert_eq!((n.processes, n.process), (None, None));
        }
    }

    #[test]
    fn processes_without_a_tunnel_a_slash_or_room_for_the_ports_is_refused() {
        let no_tunnel = THREE.replace("tunnel = { address = \"192.0.2.2:9440\" }\n", "");
        let slash = THREE.replace("edge-a", "edge/a");
        let overflow = THREE.replace("9440", "65535");
        let zero = THREE.replace("processes = 3", "processes = 0");
        for (text, needle) in [
            (no_tunnel, "needs tunnel.address"),
            (slash, "'/' in its name"),
            (overflow, "past 65535"),
            (zero, "at least 1"),
        ] {
            let err = fleet_from(&text).unwrap_err().to_string();
            assert!(err.contains(needle), "{needle}: {err}");
        }
    }
}
