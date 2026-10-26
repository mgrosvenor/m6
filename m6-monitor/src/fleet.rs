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
    /// Point this at the backbone address, not the public one. `/perf` is
    /// token-gated because it faces the internet; between nodes on a private
    /// mesh the exposure is different and the round trip is shorter. Polling
    /// a node through its own public edge also measures the edge rather than
    /// the node.
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
    /// Optional. Without it the round trip is still measured and the handshake
    /// age is reported as not configured.
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
    /// Optional. Without it no handshake age is read. **Never use `wg show ...
    /// dump`**: it includes the interface private key.
    #[serde(default)]
    pub wg_handshakes_file: Option<String>,
    /// Seconds since a peer's last WireGuard handshake above which the monitor
    /// reports a fault. Optional, default 600.
    ///
    /// The time is the last SUCCESSFUL handshake, so it is the slow signal: it
    /// stops moving the moment a tunnel breaks and stays recent for minutes.
    /// The round trip to the tunnel address is the fast one.
    #[serde(default = "default_tunnel_handshake_max_age_s")]
    pub tunnel_handshake_max_age_s: u64,
}

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
tunnel_handshake_max_age_s = 90

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
        assert_eq!(fleet.tunnel_handshake_max_age_s, 90);
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
    fn a_node_without_a_tunnel_has_none() {
        let fleet =
            load("[[monitor.nodes]]\nname = \"origin\"\nurl = \"http://origin.example.com\"\n")
                .unwrap();
        assert!(fleet.nodes[0].tunnel.is_none());
        assert!(fleet.wg_handshakes_file.is_none());
        assert_eq!(fleet.tunnel_handshake_max_age_s, 600);
    }
}
