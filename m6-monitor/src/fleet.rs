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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Fleet {
    pub nodes: Vec<Node>,
    /// Bearer token for `/perf` on every node.
    ///
    /// Read from a file, never inlined: this config is in git and the token is
    /// not. Absent means `/perf` is not polled, and the report says so rather
    /// than showing an empty fleet.
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

    /// The `/perf` bearer token, if one is configured and readable.
    pub fn perf_token(&self) -> Option<String> {
        let path = self.perf_token_file.as_ref()?;
        match std::fs::read_to_string(path) {
            Ok(s) => {
                let t = s.trim().to_string();
                if t.is_empty() {
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
