//! Firewall blocks, read from nftables' own JSON.
//!
//! The hourly check used to count `SRC=` out of the kernel ring buffer over
//! ssh. It did not need to: **nftables emits JSON natively**, schema version
//! 1, and it is already installed on every node. There is no format to invent
//! and no log to scrape.
//!
//! # Why a file rather than a call
//!
//! `nft list ruleset` needs `CAP_NET_ADMIN`. The process that answers public
//! requests must not have it to report a number, so a systemd timer runs
//!
//! ```text
//! nft -j list ruleset > /var/lib/m6/firewall.json
//! ```
//!
//! and m6 reads that file as ordinary data. The collector is one command with
//! no logic in it, which is the point: nothing to get wrong in a privileged
//! context, and the parsing lives here where it is tested.
//!
//! # What this reports, and what it deliberately does not
//!
//! Per-IP block rules and their counters. That is the actionable half: which
//! addresses are denied, and whether a denied address is still trying.
//!
//! It does **not** report "180 packets from 10 sources", which is what the
//! script printed every hour from the kernel log. That number is internet
//! background radiation: it never changed, never prompted an action, and the
//! addresses in it were not the ones that mattered. What matters is whether
//! the deliberate blocks are in place and whether they are still being hit,
//! and an address that reaches the application is m6's own business and is
//! already in [`crate::telemetry`].

use serde::{Deserialize, Serialize};

/// One deliberate per-address block.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Block {
    pub address: String,
    /// Packets this rule has dropped since it was installed.
    ///
    /// Cumulative, and reset when the rule is reinstalled, so it is a "has
    /// this been hit" signal rather than a rate. Zero on a long-standing rule
    /// means the operator gave up on that address, which is the outcome a
    /// block is for.
    pub packets: u64,
    pub bytes: u64,
    /// The `ufw ... comment` text, which is where the reason lives.
    pub comment: Option<String>,
    pub chain: String,
}

/// The firewall's blocking state.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FirewallState {
    pub blocks: Vec<Block>,
    /// Rules carrying a counter, of any kind. Context for the blocks count:
    /// a ruleset with hundreds of rules and no per-IP blocks is a different
    /// state from one with no ruleset at all.
    pub total_rules: usize,
}

impl FirewallState {
    pub fn blocked_addresses(&self) -> impl Iterator<Item = &str> {
        self.blocks.iter().map(|b| b.address.as_str())
    }

    /// Blocks that have dropped something. A block being hit means the
    /// operator is still trying; a block at zero has done its job.
    pub fn active(&self) -> impl Iterator<Item = &Block> {
        self.blocks.iter().filter(|b| b.packets > 0)
    }

    /// Parse `nft -j list ruleset` output.
    ///
    /// Walks the rule list looking for the shape a per-address block has: a
    /// source-address match, a counter, and a drop or reject verdict. Anything
    /// else in the ruleset is ignored rather than guessed at, so ufw's own
    /// scaffolding (dozens of chains, logging rules, protocol matches) does
    /// not turn into noise.
    pub fn from_nft_json(text: &str) -> anyhow::Result<Self> {
        let v: serde_json::Value = serde_json::from_str(text)?;
        let rules = v
            .get("nftables")
            .and_then(|n| n.as_array())
            .ok_or_else(|| anyhow::anyhow!("not an nft ruleset: no `nftables` array"))?;

        let mut blocks = Vec::new();
        let mut total_rules = 0usize;

        for entry in rules {
            let Some(rule) = entry.get("rule") else { continue };
            total_rules += 1;
            let Some(exprs) = rule.get("expr").and_then(|e| e.as_array()) else { continue };

            let mut address = None;
            let mut counter = None;
            let mut drops = false;

            for e in exprs {
                if let Some(m) = e.get("match") {
                    let is_saddr = m
                        .get("left")
                        .and_then(|l| l.get("payload"))
                        .and_then(|p| p.get("field"))
                        .and_then(|f| f.as_str())
                        == Some("saddr");
                    // Only a plain string right-hand side. A set or a prefix
                    // is a range rule, and this deployment does not use them
                    // (see BLOCKLIST.md: per-IP only, no CIDR), so reporting
                    // one as a single address would be a lie.
                    if is_saddr {
                        address = m.get("right").and_then(|r| r.as_str()).map(String::from);
                    }
                }
                if let Some(c) = e.get("counter") {
                    counter = Some((
                        c.get("packets").and_then(|p| p.as_u64()).unwrap_or(0),
                        c.get("bytes").and_then(|b| b.as_u64()).unwrap_or(0),
                    ));
                }
                if e.get("drop").is_some() || e.get("reject").is_some() {
                    drops = true;
                }
            }

            if let (Some(address), true) = (address, drops) {
                let (packets, bytes) = counter.unwrap_or((0, 0));
                blocks.push(Block {
                    address,
                    packets,
                    bytes,
                    comment: rule.get("comment").and_then(|c| c.as_str()).map(String::from),
                    chain: rule
                        .get("chain")
                        .and_then(|c| c.as_str())
                        .unwrap_or("")
                        .to_string(),
                });
            }
        }

        blocks.sort_by(|a, b| a.address.cmp(&b.address));
        Ok(FirewallState { blocks, total_rules })
    }

    /// Read the file the collector writes.
    ///
    /// A missing file is `Ok(None)`, not an error: a node with no collector
    /// installed is a node that reports no firewall state, which is different
    /// from a node whose firewall could not be read.
    pub fn from_file(path: &std::path::Path) -> anyhow::Result<Option<Self>> {
        match std::fs::read_to_string(path) {
            Ok(text) => Ok(Some(Self::from_nft_json(&text)?)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real rule from syd, 2026-09-11, byte for byte.
    const REAL_RULE: &str = r#"{"nftables":[
      {"metainfo":{"version":"1.1.6","release_name":"Commodore Bullmoose #7","json_schema_version":1}},
      {"rule":{"family":"ip","table":"filter","chain":"ufw-user-input","handle":1381,
        "comment":"spoofed Googlebot sweep 2026-09-09 see BLOCKLIST.md",
        "expr":[{"match":{"op":"==","left":{"payload":{"protocol":"ip","field":"saddr"}},"right":"94.154.46.249"}},
                {"counter":{"packets":17,"bytes":1020}},
                {"drop":null}]}}
    ]}"#;

    #[test]
    fn parses_a_real_per_ip_block() {
        let s = FirewallState::from_nft_json(REAL_RULE).unwrap();
        assert_eq!(s.blocks.len(), 1);
        let b = &s.blocks[0];
        assert_eq!(b.address, "94.154.46.249");
        assert_eq!(b.packets, 17);
        assert_eq!(b.bytes, 1020);
        assert_eq!(b.chain, "ufw-user-input");
        assert!(b.comment.as_deref().unwrap().contains("spoofed Googlebot"));
        assert_eq!(s.active().count(), 1);
    }

    /// ufw's own scaffolding is most of the ruleset: chains, logging rules,
    /// protocol and port matches. None of it is a per-address block, and
    /// guessing at it would turn a hundred rules into a hundred false blocks.
    #[test]
    fn ufw_scaffolding_is_not_mistaken_for_blocks() {
        let json = r#"{"nftables":[
          {"chain":{"family":"ip","table":"filter","name":"ufw-before-input","handle":1}},
          {"rule":{"family":"ip","table":"filter","chain":"ufw-before-input","handle":2,
            "expr":[{"counter":{"packets":2374532,"bytes":1715841412}},{"accept":null}]}},
          {"rule":{"family":"ip","table":"filter","chain":"ufw-user-input","handle":3,
            "expr":[{"match":{"op":"==","left":{"payload":{"protocol":"tcp","field":"dport"}},"right":443}},
                    {"counter":{"packets":9,"bytes":540}},{"accept":null}]}}
        ]}"#;
        let s = FirewallState::from_nft_json(json).unwrap();
        assert!(s.blocks.is_empty(), "got {:?}", s.blocks);
        assert_eq!(s.total_rules, 2, "counted as rules, just not as blocks");
    }

    /// An ACCEPT rule matching a source address is an allowlist entry, not a
    /// block. Reporting it as one would say an address is denied when it is
    /// specifically permitted, which is the worst direction to be wrong in.
    #[test]
    fn an_allowed_source_is_not_a_block() {
        let json = r#"{"nftables":[
          {"rule":{"family":"ip","table":"filter","chain":"ufw-user-input","handle":4,
            "expr":[{"match":{"op":"==","left":{"payload":{"protocol":"ip","field":"saddr"}},"right":"10.0.0.4"}},
                    {"counter":{"packets":5,"bytes":300}},{"accept":null}]}}
        ]}"#;
        let s = FirewallState::from_nft_json(json).unwrap();
        assert!(s.blocks.is_empty());
    }

    /// A range rule is not a single address. This deployment is per-IP only
    /// by policy (BLOCKLIST.md), and a `/24` reported as one address would
    /// hide 255 others.
    #[test]
    fn a_prefix_rule_is_not_reported_as_an_address() {
        let json = r#"{"nftables":[
          {"rule":{"family":"ip","table":"filter","chain":"ufw-user-input","handle":5,
            "expr":[{"match":{"op":"==","left":{"payload":{"protocol":"ip","field":"saddr"}},
                     "right":{"prefix":{"addr":"94.154.46.0","len":24}}}},
                    {"counter":{"packets":1,"bytes":60}},{"drop":null}]}}
        ]}"#;
        let s = FirewallState::from_nft_json(json).unwrap();
        assert!(s.blocks.is_empty(), "a prefix is not an address: {:?}", s.blocks);
    }

    /// A block at zero has done its job; one still counting has not.
    #[test]
    fn active_means_still_being_hit() {
        let json = r#"{"nftables":[
          {"rule":{"chain":"ufw-user-input",
            "expr":[{"match":{"op":"==","left":{"payload":{"protocol":"ip","field":"saddr"}},"right":"1.1.1.1"}},
                    {"counter":{"packets":0,"bytes":0}},{"drop":null}]}},
          {"rule":{"chain":"ufw-user-input",
            "expr":[{"match":{"op":"==","left":{"payload":{"protocol":"ip","field":"saddr"}},"right":"2.2.2.2"}},
                    {"counter":{"packets":99,"bytes":5940}},{"drop":null}]}}
        ]}"#;
        let s = FirewallState::from_nft_json(json).unwrap();
        assert_eq!(s.blocks.len(), 2);
        assert_eq!(s.active().map(|b| b.address.as_str()).collect::<Vec<_>>(), ["2.2.2.2"]);
        assert_eq!(
            s.blocked_addresses().collect::<Vec<_>>(),
            ["1.1.1.1", "2.2.2.2"],
            "sorted, so two nodes' states can be compared directly"
        );
    }

    #[test]
    fn a_missing_collector_file_is_not_an_error() {
        let got = FirewallState::from_file(std::path::Path::new("/no/such/firewall.json")).unwrap();
        assert!(got.is_none(), "no collector is not the same as a broken firewall");
    }

    #[test]
    fn garbage_is_an_error_not_an_empty_state() {
        assert!(FirewallState::from_nft_json("not json").is_err());
        assert!(FirewallState::from_nft_json(r#"{"something":"else"}"#).is_err());
    }
}
