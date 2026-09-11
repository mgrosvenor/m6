//! Which nodes there are, and how to reach them.
//!
//! A fleet is configuration, not code. mgrosvenor.com is three nodes named
//! syd, lon and chi; that is one deployment of m6 and nothing here knows it.

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
    /// **The nodes do not share a token.** Measured 2026-09-11: syd, lon, chi
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
    /// In this deployment every node has a distinct token, so each one names
    /// its own and this is mostly a convenience for a fleet that does share.
    #[serde(default)]
    pub perf_token_file: Option<String>,
    /// How long to wait for one node before giving up on it.
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
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
name = "syd"
url  = "http://10.0.0.1:8080"
role = "origin"

[[monitor.nodes]]
name = "lon"
url  = "http://10.0.0.4:8080"
role = "cache"
"#
        )
        .unwrap();
        let fleet = Fleet::from_config(f.path()).unwrap();
        assert_eq!(fleet.nodes.len(), 2);
        assert_eq!(fleet.nodes[0].name, "syd");
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
        write!(tok, "s3cret\n").unwrap();
        let mut f = tempfile::NamedTempFile::new().unwrap();
        write!(
            f,
            "[monitor]\nperf_token_file = {:?}\n\n[[monitor.nodes]]\nname=\"a\"\nurl=\"http://x\"\n",
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
        write!(per_node, "node-token\n").unwrap();

        let n = Node {
            name: "lon".into(),
            url: "https://lon".into(),
            role: "cache".into(),
            perf_token_file: Some(per_node.path().to_string_lossy().to_string()),
        };
        assert_eq!(n.perf_token(Some("fleet-token")).as_deref(), Some("node-token"));

        let bare = Node {
            name: "syd".into(),
            url: "https://syd".into(),
            role: "origin".into(),
            perf_token_file: None,
        };
        assert_eq!(bare.perf_token(Some("fleet-token")).as_deref(), Some("fleet-token"));
        assert_eq!(bare.perf_token(None), None);
    }

    #[test]
    fn a_missing_token_file_is_a_warning_not_a_crash() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        write!(
            f,
            "[monitor]\nperf_token_file = \"/no/such/file\"\n\n[[monitor.nodes]]\nname=\"a\"\nurl=\"http://x\"\n"
        )
        .unwrap();
        let fleet = Fleet::from_config(f.path()).unwrap();
        assert!(fleet.perf_token().is_none());
    }
}
