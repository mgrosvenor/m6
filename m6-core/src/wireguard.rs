//! WireGuard peer handshakes, read from `wg`'s own output.
//!
//! # Why a file rather than a call
//!
//! `wg show` needs `CAP_NET_ADMIN`. The process that serves a socket must not
//! have it to report a number, the rule `firewall` follows. A systemd timer
//! runs
//!
//! ```text
//! wg show wg0 latest-handshakes > /var/lib/m6-monitor/wg-handshakes.tmp && mv -f /var/lib/m6-monitor/wg-handshakes.tmp /var/lib/m6-monitor/wg-handshakes
//! ```
//!
//! and m6 reads the result as ordinary data. The collector has no logic, and
//! the parsing lives here where it is tested.
//!
//! # What the file must hold
//!
//! The output of `latest-handshakes` only. It prints `<public-key>\t<unix-seconds>`
//! per peer, or `<interface>\t<public-key>\t<unix-seconds>` for `wg show all`.
//! A time of 0 means the peer has never completed a handshake.
//!
//! **Never use `wg show <if> dump`.** It includes the interface private key.
//!
//! # What the time means
//!
//! `latest-handshakes` reports the last SUCCESSFUL handshake. After a key or
//! an endpoint changes underneath a tunnel, the time stops moving, and for the
//! next minutes it still looks recent. An age is therefore the slow signal. A
//! live probe over the tunnel is the fast one.

use serde::{Deserialize, Serialize};

/// One peer's latest handshake.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerHandshake {
    /// The interface name, present only when the file came from `wg show all`.
    pub interface: Option<String>,
    /// The peer's public key, as `wg` prints it.
    pub public_key: String,
    /// Unix seconds of the latest handshake. `None` means never (`wg` prints 0).
    pub latest_unix: Option<u64>,
}

/// Every peer in a handshake file.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Handshakes {
    pub peers: Vec<PeerHandshake>,
}

impl Handshakes {
    /// Parse the output of `wg show <if> latest-handshakes` or `wg show all latest-handshakes`.
    ///
    /// Any line of another shape is an error naming its 1-based line number.
    /// A file in the wrong format means the wrong command was configured, and
    /// a check that cannot measure must fail.
    pub fn parse_latest_handshakes(text: &str) -> anyhow::Result<Self> {
        let mut peers = Vec::new();
        for (i, line) in text.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let fields: Vec<&str> = line.split_whitespace().collect();
            let (interface, key, secs) = match fields.as_slice() {
                [key, secs] => (None, *key, *secs),
                [iface, key, secs] => (Some((*iface).to_string()), *key, *secs),
                _ => anyhow::bail!(
                    "line {}: expected 2 or 3 columns from `wg show latest-handshakes`, found {}",
                    i + 1,
                    fields.len()
                ),
            };
            let secs: u64 = secs.parse().map_err(|_| {
                anyhow::anyhow!("line {}: {:?} is not a number of seconds", i + 1, secs)
            })?;
            peers.push(PeerHandshake {
                interface,
                public_key: key.to_string(),
                latest_unix: if secs == 0 { None } else { Some(secs) },
            });
        }
        Ok(Handshakes { peers })
    }

    /// Read the file the collector writes.
    ///
    /// A missing file is `Ok(None)`, not an error: a host with no collector
    /// installed is different from a host whose handshakes could not be read.
    pub fn from_file(path: &std::path::Path) -> anyhow::Result<Option<Self>> {
        match std::fs::read_to_string(path) {
            Ok(text) => Ok(Some(Self::parse_latest_handshakes(&text)?)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// The peer with exactly this public key (whitespace trimmed).
    pub fn find(&self, public_key: &str) -> Option<&PeerHandshake> {
        let key = public_key.trim();
        self.peers.iter().find(|p| p.public_key == key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY_A: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";
    const KEY_B: &str = "BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB=";

    #[test]
    fn parses_wg_show_latest_handshakes_output() {
        let text = format!("{KEY_A}\t1700000000\n{KEY_B}\t1700000042\n");
        let h = Handshakes::parse_latest_handshakes(&text).unwrap();
        assert_eq!(h.peers.len(), 2);
        assert_eq!(h.peers[0].interface, None);
        assert_eq!(h.peers[0].public_key, KEY_A);
        assert_eq!(h.peers[1].latest_unix, Some(1_700_000_042));
    }

    #[test]
    fn parses_wg_show_all_latest_handshakes_output() {
        let text = format!("wg0\t{KEY_A}\t1700000000\nwg1\t{KEY_B}\t5\n");
        let h = Handshakes::parse_latest_handshakes(&text).unwrap();
        assert_eq!(h.peers[0].interface.as_deref(), Some("wg0"));
        assert_eq!(h.peers[1].interface.as_deref(), Some("wg1"));
        assert_eq!(h.peers[1].public_key, KEY_B);
    }

    #[test]
    fn a_zero_timestamp_is_never_not_the_epoch() {
        let h = Handshakes::parse_latest_handshakes(&format!("{KEY_A}\t0\n")).unwrap();
        assert_eq!(h.peers[0].latest_unix, None);
    }

    #[test]
    fn a_malformed_line_is_an_error_naming_the_line() {
        let e = Handshakes::parse_latest_handshakes(&format!("{KEY_A}\t1\njunk\n")).unwrap_err();
        assert!(e.to_string().contains("line 2"), "{e}");
        let e = Handshakes::parse_latest_handshakes(&format!("{KEY_A}\t1\n{KEY_B}\tsoon\n"))
            .unwrap_err();
        assert!(e.to_string().contains("line 2"), "{e}");
    }

    #[test]
    fn an_empty_file_is_no_peers() {
        assert!(Handshakes::parse_latest_handshakes("")
            .unwrap()
            .peers
            .is_empty());
        assert!(Handshakes::parse_latest_handshakes("\n")
            .unwrap()
            .peers
            .is_empty());
    }

    #[test]
    fn a_missing_file_is_none_not_an_error() {
        let r = Handshakes::from_file(std::path::Path::new("/no/such/file")).unwrap();
        assert!(r.is_none());
    }

    #[test]
    fn find_matches_the_public_key_exactly() {
        let h = Handshakes::parse_latest_handshakes(&format!("{KEY_A}\t7\n")).unwrap();
        assert!(h.find(KEY_A).is_some());
        assert!(h.find(&format!(" {KEY_A}\n")).is_some());
        assert!(h.find(&KEY_A[..8]).is_none());
        assert!(h.find(KEY_B).is_none());
    }
}
