//! Which nodes there are, and how to reach them.
//!
//! A fleet is configuration, not code. example.com is three nodes named
//! origin, edge-a and edge-b; that is one deployment of m6 and nothing here knows it.

use std::path::Path;

use serde::{Deserialize, Serialize};

/// One node to poll.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Node {
    /// Short name for the report. Not necessarily the node's own `node.name`,
    /// which is whatever that node calls itself and is reported separately so
    /// a mismatch is visible.
    pub name: String,
    /// Base URL, scheme and authority.
    ///
    /// The node's public address. `/health` and the header checks are fetched
    /// from here, because what a visitor receives is what they assert on, and
    /// its host name is the name the node's certificate is checked against.
    /// `/perf` and `/traffic` come from here too unless `metrics_over_tunnel`
    /// moves them to the tunnel, so `/perf` no longer has to face the
    /// internet. Polling a node through its own public edge also measures the
    /// edge rather than the node.
    pub url: String,
    /// Role, for the report only. A cache node legitimately has no socket
    /// pools and a low hit rate relative to origin, and reporting it next to
    /// origin without saying which is which invites the wrong conclusion.
    #[serde(default)]
    pub role: String,
    /// This node's `/perf` bearer token, if it has its own.
    ///
    /// **The nodes do not share a token.** Measured 2026-09-11: origin, edge-a, edge-b
    /// and the build host each have a different `/etc/m6/perf-token`. The
    /// docs said otherwise and were wrong, and the failure is quiet: the
    /// monitor polls with one token and two of three nodes answer 401 with
    /// nothing to say why.
    ///
    /// Distinct tokens are the better property, so this follows reality
    /// rather than changing it: a leaked token exposes one node's `/perf`,
    /// not the fleet's.
    #[serde(default)]
    pub perf_token_file: Option<String>,
    /// The WireGuard tunnel from this host to the node, if there is one.
    ///
    /// Absent means "no tunnel configured", which the report says out loud and
    /// never counts as a fault.
    #[serde(default)]
    pub tunnel: Option<Tunnel>,
    /// Fetch `/perf` and `/traffic` through `tunnel.address` instead of the
    /// public address. Default false.
    ///
    /// `tunnel.address` must then be the node's HTTPS port on its tunnel
    /// address, because the request is TLS to that socket. The URL's host name
    /// still names the certificate, so the node's public certificate is checked
    /// as usual. The node answers these paths only to peers it lists
    /// (`[health] metrics_peers`) and only for an API token, so this is the
    /// supported way to keep them off the public interface.
    #[serde(default)]
    pub metrics_over_tunnel: bool,
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

impl Node {
    /// This node's token, falling back to the fleet-wide one.
    pub fn perf_token(&self, fallback: Option<&str>) -> Option<String> {
        match &self.perf_token_file {
            Some(path) => read_token(path),
            None => fallback.map(|s| s.to_string()),
        }
    }
}

fn read_token(path: &str) -> Option<String> {
    match std::fs::read_to_string(path) {
        Ok(s) => {
            let t = s.trim().to_string();
            if t.is_empty() {
                tracing::warn!(path = %path, "perf token file is empty");
                None
            } else {
                Some(t)
            }
        }
        Err(e) => {
            tracing::warn!(path = %path, error = %e, "perf token file unreadable");
            None
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Fleet {
    pub nodes: Vec<Node>,
    /// Fallback `/perf` bearer token, for nodes that do not name their own.
    ///
    /// Read from a file, never inlined: this config is in git and the token is
    /// not. Absent means `/perf` is not polled, and the report says so rather
    /// than showing an empty fleet.
    ///
    /// A fleet whose nodes each have a distinct token names one per node and
    /// leaves this unset. It is the convenience for a fleet that does share.
    #[serde(default)]
    pub perf_token_file: Option<String>,
    /// How long to wait for one node before giving up on it.
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
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
    /// Seconds since a node last reopened its log file above which the monitor
    /// reports a fault. Optional, with no default.
    ///
    /// A process reopens its log file when log rotation renames it and sends
    /// SIGUSR1, and each node reports the age of its last reopen on `/perf`.
    /// With this set, an age over the limit is a fault, and so is a node that
    /// has not reopened at all in more than this many seconds since it
    /// started, and so is a node too old to report the age.
    /// Without it the age is shown in the check and the digest and never
    /// judged.
    ///
    /// There is no default because analytics logging is on by default, and a
    /// deployment that does not rotate would otherwise be faulted after every
    /// start. A site that rotates every hour does well with twice the
    /// interval, which is 7200.
    #[serde(default)]
    pub log_reopen_max_age_s: Option<u64>,
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

impl Fleet {
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
        let fleet: Fleet = section.clone().try_into()?;
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
        for n in &fleet.nodes {
            if n.metrics_over_tunnel && n.tunnel.is_none() {
                anyhow::bail!(
                    "node {:?}: metrics_over_tunnel needs tunnel.address, the node's HTTPS port on its tunnel address",
                    n.name
                );
            }
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
        Ok(fleet)
    }

    /// The fleet-wide fallback token, if one is configured and readable.
    pub fn perf_token(&self) -> Option<String> {
        read_token(self.perf_token_file.as_ref()?)
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
        assert!(fleet.perf_token().is_none());
    }

    #[test]
    fn a_fleet_with_no_nodes_is_an_error_not_an_empty_report() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        write!(f, "[monitor]\ntimeout_ms = 1000\n").unwrap();
        assert!(Fleet::from_config(f.path()).is_err());
    }

    #[test]
    fn the_token_is_read_from_a_file_never_from_the_config() {
        let mut tok = tempfile::NamedTempFile::new().unwrap();
        writeln!(tok, "s3cret").unwrap();
        let mut f = tempfile::NamedTempFile::new().unwrap();
        write!(
            f,
            "[monitor]\nperf_token_file = {:?}\n\n[[monitor.nodes]]\nname=\"a\"\nurl=\"http://origin.example.com\"\n",
            tok.path()
        )
        .unwrap();
        let fleet = Fleet::from_config(f.path()).unwrap();
        assert_eq!(fleet.perf_token().as_deref(), Some("s3cret"));
        // And the token itself is not a field anyone could set inline.
        let raw = std::fs::read_to_string(f.path()).unwrap();
        assert!(!raw.contains("s3cret"));
    }

    /// The nodes do not share a token, so a per-node file must win over the
    /// fleet-wide one. Getting this wrong is a quiet failure: the monitor
    /// polls with the wrong token and the node answers 401.
    #[test]
    fn a_node_token_overrides_the_fleet_token() {
        let mut per_node = tempfile::NamedTempFile::new().unwrap();
        writeln!(per_node, "node-token").unwrap();

        let n = Node {
            name: "edge-a".into(),
            url: "https://edge-a.example.com".into(),
            role: "cache".into(),
            perf_token_file: Some(per_node.path().to_string_lossy().to_string()),
            tunnel: None,
            metrics_over_tunnel: false,
        };
        assert_eq!(
            n.perf_token(Some("fleet-token")).as_deref(),
            Some("node-token")
        );

        let bare = Node {
            name: "origin".into(),
            url: "https://origin.example.com".into(),
            role: "origin".into(),
            perf_token_file: None,
            tunnel: None,
            metrics_over_tunnel: false,
        };
        assert_eq!(
            bare.perf_token(Some("fleet-token")).as_deref(),
            Some("fleet-token")
        );
        assert_eq!(bare.perf_token(None), None);
    }

    #[test]
    fn a_missing_token_file_is_a_warning_not_a_crash() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        write!(
            f,
            "[monitor]\nperf_token_file = \"/no/such/file\"\n\n[[monitor.nodes]]\nname=\"a\"\nurl=\"http://origin.example.com\"\n"
        )
        .unwrap();
        let fleet = Fleet::from_config(f.path()).unwrap();
        assert!(fleet.perf_token().is_none());
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
        // The poller reads the top-level key, so a `[monitor]` one changes nothing:
        // the poll is 30 s and the default limit of 600 s is enough.
        load(&format!("[monitor]\npoll_interval_s = 600\n{node}")).unwrap();
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

    #[test]
    fn log_reopen_max_age_s_is_read_and_absent_means_none() {
        let node = "[[monitor.nodes]]\nname = \"origin\"\nurl = \"http://origin.example.com\"\n";
        let set = load(&format!("[monitor]\nlog_reopen_max_age_s = 7200\n{node}")).unwrap();
        assert_eq!(set.log_reopen_max_age_s, Some(7200));
        let unset = load(node).unwrap();
        assert_eq!(unset.log_reopen_max_age_s, None);
    }

    #[test]
    fn metrics_over_tunnel_defaults_off() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        write!(
            f,
            "[[monitor.nodes]]\nname = \"a\"\nurl = \"http://a.example.com\"\n\
             tunnel = {{ address = \"192.0.2.2:443\" }}\n"
        )
        .unwrap();
        let fleet = Fleet::from_config(f.path()).unwrap();
        assert!(!fleet.nodes[0].metrics_over_tunnel);
    }

    #[test]
    fn metrics_over_tunnel_needs_a_tunnel() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        write!(
            f,
            "[[monitor.nodes]]\nname = \"a\"\nurl = \"http://a.example.com\"\n\
             metrics_over_tunnel = true\n"
        )
        .unwrap();
        let e = Fleet::from_config(f.path()).unwrap_err().to_string();
        assert!(
            e.contains("metrics_over_tunnel needs tunnel.address"),
            "{e}"
        );
    }
}
