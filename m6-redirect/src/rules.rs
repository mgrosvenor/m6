//! The redirect list: parse, validate, index and match.
//!
//! The list is the Cloudflare Bulk Redirects list-items JSON shape. m6-redirect
//! is deliberately static, the way Cloudflare Bulk Redirects are: exact match
//! by host and path in a hash map, no regular expressions, no string
//! replacement, no templating. A rule's `subpath_matching` must be false,
//! because subpath matching is the one feature of the Cloudflare shape this
//! engine does not implement.

use std::collections::HashMap;
use std::io::Write;
use std::path::Path;

use serde::Deserialize;

/// The redirect one rule performs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Redirect {
    pub target: String,
    pub status: u16,
}

/// The compiled table of rules.
///
/// One entry per rule, keyed by the lowercased host concatenated with the
/// path. The host is compared case-insensitively, the path exactly. The host
/// carries its port when one is present, so `example.com` and
/// `example.com:8443` are different hosts and can answer differently.
pub struct Rules {
    by_key: HashMap<String, Redirect>,
}

/// The list-items JSON wrapper Cloudflare uses.
#[derive(Deserialize)]
struct List {
    items: Vec<Item>,
}

/// One list item. The fields the engine reads are `source_url`, `target_url`,
/// `status_code` and `subpath_matching`. The rest of the Cloudflare shape is
/// ignored, because this engine ignores the query and matches exactly.
#[derive(Deserialize)]
struct Item {
    source_url: String,
    target_url: String,
    #[serde(default = "default_status")]
    status_code: u16,
    #[serde(default)]
    subpath_matching: bool,
}

fn default_status() -> u16 {
    301
}

impl Rules {
    /// Parse and index a list body.
    ///
    /// The returned table shares nothing with the body, so a caller may keep
    /// the bytes and write them verbatim while swapping the table in.
    pub fn parse(body: &[u8]) -> Result<Rules, String> {
        let list: List = serde_json::from_slice(body).map_err(|e| format!("invalid JSON: {e}"))?;
        let mut by_key = HashMap::with_capacity(list.items.len());
        for (i, item) in list.items.into_iter().enumerate() {
            validate_item(&item, i)?;
            let key = source_key(&item.source_url, i)?;
            let redirect = Redirect {
                target: item.target_url,
                status: item.status_code,
            };
            if by_key.insert(key, redirect).is_some() {
                return Err(format!("items[{i}]: duplicate source_url"));
            }
        }
        Ok(Rules { by_key })
    }

    /// Read the list file. A missing file is an empty table, so a fresh
    /// install serves no redirect until the first list is posted. Any other
    /// read error, and any malformed body, is an error the caller maps to a
    /// start refusal or a failed reload.
    pub fn load(path: &Path) -> Result<Rules, String> {
        match std::fs::read(path) {
            Ok(bytes) => Rules::parse(&bytes),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Rules {
                by_key: HashMap::new(),
            }),
            Err(e) => Err(format!("reading {}: {e}", path.display())),
        }
    }

    /// The redirect for one request, or `None` for a 404.
    ///
    /// The host is lowercased for the comparison. The path is used exactly as
    /// written, so `CAP7K9` and `cap7k9/` are different paths from `cap7k9`.
    pub fn lookup(&self, host: &str, path: &str) -> Option<&Redirect> {
        let mut key = host.to_ascii_lowercase();
        key.push_str(if path.is_empty() { "/" } else { path });
        self.by_key.get(&key)
    }

    /// How many rules the table holds.
    pub fn len(&self) -> usize {
        self.by_key.len()
    }

    /// True when the table holds no rules.
    pub fn is_empty(&self) -> bool {
        self.by_key.is_empty()
    }

    /// Write a validated list body to `path` atomically: a temp file in the
    /// same directory, synced, then renamed over the target. The directory is
    /// created when it is missing, so a fresh StateDirectory still accepts its
    /// first list.
    pub fn write_atomic(path: &Path, body: &[u8]) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("creating {}: {e}", parent.display()))?;
        }
        let tmp = path.with_extension("tmp");
        {
            let mut file = std::fs::File::create(&tmp)
                .map_err(|e| format!("creating {}: {e}", tmp.display()))?;
            file.write_all(body)
                .map_err(|e| format!("writing {}: {e}", tmp.display()))?;
            file.sync_all()
                .map_err(|e| format!("syncing {}: {e}", tmp.display()))?;
        }
        std::fs::rename(&tmp, path)
            .map_err(|e| format!("renaming {} to {}: {e}", tmp.display(), path.display()))
    }
}

/// Reject one item the engine cannot honour, or that would write an unsafe
/// header. The message names the item index and the field, never the value, so
/// it is safe to put in a refusal body.
fn validate_item(item: &Item, i: usize) -> Result<(), String> {
    if item.source_url.is_empty() {
        return Err(format!("items[{i}].source_url must be a non-empty string"));
    }
    if item.target_url.is_empty() {
        return Err(format!("items[{i}].target_url must be a non-empty string"));
    }
    if header_unsafe(&item.source_url) {
        return Err(format!(
            "items[{i}].source_url contains a control character"
        ));
    }
    if header_unsafe(&item.target_url) {
        return Err(format!(
            "items[{i}].target_url contains a control character"
        ));
    }
    if !matches!(item.status_code, 301 | 302 | 307 | 308) {
        return Err(format!(
            "items[{i}].status_code must be 301, 302, 307 or 308"
        ));
    }
    if item.subpath_matching {
        return Err(format!(
            "items[{i}].subpath_matching is not supported: m6-redirect matches exactly"
        ));
    }
    Ok(())
}

/// A `\r` or `\n` in either URL would escape the `Location` header this engine
/// writes, so the two bytes that matter are refused. A control character in a
/// list is a broken list, not something to sanitise silently.
fn header_unsafe(s: &str) -> bool {
    s.chars().any(|c| c == '\r' || c == '\n')
}

/// The lookup key of one `source_url`: lowercased host plus the path, with the
/// optional scheme stripped. The shape is the Cloudflare one, which is a full
/// URL, plus the bare `host/path` form the interface note's example uses.
fn source_key(source_url: &str, i: usize) -> Result<String, String> {
    let rest = source_url
        .strip_prefix("https://")
        .or_else(|| source_url.strip_prefix("http://"))
        .unwrap_or(source_url);
    let (host, path) = match rest.find('/') {
        Some(slash) => (&rest[..slash], &rest[slash..]),
        None => (rest, "/"),
    };
    if host.is_empty() {
        return Err(format!("items[{i}].source_url has no host"));
    }
    let mut key = host.to_ascii_lowercase();
    key.push_str(path);
    Ok(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules(body: &str) -> Rules {
        Rules::parse(body.as_bytes()).expect("a valid list")
    }

    const LIST: &str = r#"{
        "items": [
            {"source_url": "mgrosvenor.com/r/cap7k9",
             "target_url": "https://mgrosvenor.com/capabilities?utm_source=robert&utm_campaign=recruitmentremal",
             "status_code": 302},
            {"source_url": "https://mgrosvenor.com/r/old",
             "target_url": "https://mgrosvenor.com/new",
             "status_code": 301},
            {"source_url": "example.com/r/see-other",
             "target_url": "https://example.com/other", "status_code": 308}
        ]
    }"#;

    #[test]
    fn a_valid_list_indexes_every_rule() {
        let r = rules(LIST);
        assert_eq!(r.len(), 3);
    }

    #[test]
    fn lookup_matches_host_and_path_exactly() {
        let r = rules(LIST);
        let redirect = r.lookup("mgrosvenor.com", "/r/cap7k9").unwrap();
        assert_eq!(redirect.status, 302);
        assert_eq!(
            redirect.target,
            "https://mgrosvenor.com/capabilities?utm_source=robert&utm_campaign=recruitmentremal"
        );
        assert!(r.lookup("mgrosvenor.com", "/r/other").is_none());
        assert!(r.lookup("other.com", "/r/cap7k9").is_none());
    }

    #[test]
    fn the_host_is_case_insensitive_and_the_path_is_not() {
        let r = rules(LIST);
        assert!(r.lookup("MGROSVENOR.COM", "/r/cap7k9").is_some());
        assert!(r.lookup("mgrosvenor.com", "/R/CAP7K9").is_none());
    }

    #[test]
    fn a_source_url_with_a_scheme_matches_the_bare_form() {
        let r = rules(LIST);
        assert!(r.lookup("mgrosvenor.com", "/r/old").is_some());
    }

    #[test]
    fn a_missing_host_is_refused() {
        let body = r#"{"items": [{"source_url": "/r/x", "target_url": "https://e/", "status_code": 301}]}"#;
        assert!(Rules::parse(body.as_bytes()).is_err());
    }

    #[test]
    fn an_invalid_status_code_is_refused() {
        let body = r#"{"items": [{"source_url": "e.com/r/x", "target_url": "https://e/", "status_code": 200}]}"#;
        assert!(Rules::parse(body.as_bytes()).is_err());
    }

    #[test]
    fn subpath_matching_is_refused() {
        let body = r#"{"items": [{"source_url": "e.com/r/", "target_url": "https://e/", "subpath_matching": true}]}"#;
        assert!(Rules::parse(body.as_bytes()).is_err());
    }

    #[test]
    fn a_duplicate_source_url_is_refused() {
        let body = r#"{"items": [
            {"source_url": "e.com/r/x", "target_url": "https://e/a"},
            {"source_url": "E.COM/r/x", "target_url": "https://e/b"}
        ]}"#;
        assert!(Rules::parse(body.as_bytes()).is_err());
    }

    #[test]
    fn a_crlf_in_a_target_url_is_refused() {
        let body =
            r#"{"items": [{"source_url": "e.com/r/x", "target_url": "https://e/\r\nX-Evil: 1"}]}"#;
        assert!(Rules::parse(body.as_bytes()).is_err());
    }

    #[test]
    fn a_status_code_defaults_to_301() {
        let body = r#"{"items": [{"source_url": "e.com/r/x", "target_url": "https://e/"}]}"#;
        let r = rules(body);
        assert_eq!(r.lookup("e.com", "/r/x").unwrap().status, 301);
    }

    #[test]
    fn an_empty_items_array_is_a_valid_empty_table() {
        let r = rules(r#"{"items": []}"#);
        assert!(r.is_empty());
    }

    #[test]
    fn malformed_json_is_refused() {
        assert!(Rules::parse(b"not json").is_err());
        assert!(Rules::parse(b"{}").is_err());
        assert!(Rules::parse(br#"{"items": "x"}"#).is_err());
    }

    #[test]
    fn a_missing_file_is_an_empty_table() {
        let dir = tempfile::tempdir().unwrap();
        let r = Rules::load(&dir.path().join("absent.json")).unwrap();
        assert!(r.is_empty());
    }

    #[test]
    fn a_malformed_file_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rules.json");
        std::fs::write(&path, "not json").unwrap();
        assert!(Rules::load(&path).is_err());
    }

    #[test]
    fn write_atomic_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rules.json");
        let body = r#"{"items": []}"#.as_bytes();
        Rules::write_atomic(&path, body).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), body);
        assert!(
            !path.with_extension("tmp").exists(),
            "the temp file is gone"
        );
    }
}
