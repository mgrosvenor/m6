//! The request dictionary: shared base, per-request overlay.
//!
//! Building a request's dictionary used to start from an empty map and copy
//! the whole of the service's static configuration into it, per request. On
//! the real site that is the config file's keys plus `data/content.json`,
//! 68KB and 1,364 nodes, deep-copied several times to produce a map that was
//! identical for every request until the next config reload.
//!
//! Nothing in that is per-request. So it is not copied per request: the base
//! is built once per route per reload and shared behind an `Arc`, and only the
//! handful of entries that genuinely differ are allocated per request.
//!
//! **Precedence is why this is a type rather than two fields.** `build_dict`
//! merges twelve sources in a fixed order, and the order is load-bearing:
//! built-in keys go in *after* params files so that a params file cannot
//! override `year`, `datetime` or `request_path`. Splitting the sources across
//! two layers preserves that only if the split respects it, so it does:
//!
//! - **base**, built once per reload: config keys, global params files, and
//!   the route's *static* params files, merged in that order.
//! - **overlay**, built per request: dynamic params files (those whose path
//!   holds a `{placeholder}` and so resolve per request), then path params,
//!   query, form fields, cookies, built-ins, auth claims, flash.
//!
//! The overlay always wins over the base, and within the overlay later
//! insertions win, so both halves of the original ordering survive: a params
//! file still loses to a built-in, whether it was static or dynamic.

use std::sync::Arc;

use serde_json::{Map, Value};

/// A request dictionary.
///
/// Cloning is cheap by construction: an `Arc` bump for the base and a copy of
/// the overlay, which holds a dozen short entries rather than the site's
/// content.
#[derive(Clone, Debug, Default)]
pub struct Dict {
    base: Option<Arc<Map<String, Value>>>,
    overlay: Map<String, Value>,
}

impl Dict {
    /// A dictionary layered over a shared base.
    pub fn with_base(base: Arc<Map<String, Value>>) -> Self {
        Self { base: Some(base), overlay: Map::new() }
    }

    /// A dictionary with no base, for a caller that has no framework state:
    /// tests, and handlers building a context by hand.
    pub fn new() -> Self {
        Self { base: None, overlay: Map::new() }
    }

    /// Insert into the overlay, where it shadows any base entry of the same
    /// name. This is the only way to write, which is what makes the base
    /// genuinely immutable and shareable.
    pub fn insert(&mut self, key: String, value: Value) {
        self.overlay.insert(key, value);
    }

    /// Overlay first, then base.
    pub fn get(&self, key: &str) -> Option<&Value> {
        match self.overlay.get(key) {
            Some(v) => Some(v),
            None => self.base.as_ref()?.get(key),
        }
    }

    pub fn contains_key(&self, key: &str) -> bool {
        self.get(key).is_some()
    }

    /// Distinct keys across both layers.
    pub fn len(&self) -> usize {
        let base = match &self.base {
            Some(b) => b.keys().filter(|k| !self.overlay.contains_key(*k)).count(),
            None => 0,
        };
        base + self.overlay.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Every visible entry, overlay first, then the base entries it does not
    /// shadow. Order between the two layers is not otherwise meaningful, and
    /// no consumer depends on it: the renderer builds a keyed context.
    pub fn iter(&self) -> impl Iterator<Item = (&String, &Value)> {
        self.overlay.iter().chain(
            self.base
                .iter()
                .flat_map(|b| b.iter())
                .filter(|(k, _)| !self.overlay.contains_key(*k)),
        )
    }

    /// Flatten into an owned map.
    ///
    /// This is the copy the rest of the type exists to avoid, so it is named
    /// rather than implicit and has exactly one caller class: somewhere that
    /// genuinely needs an owned `Map`, such as a handler API that predates
    /// this type.
    pub fn to_map(&self) -> Map<String, Value> {
        let mut m = match &self.base {
            Some(b) => b.as_ref().clone(),
            None => Map::new(),
        };
        for (k, v) in &self.overlay {
            m.insert(k.clone(), v.clone());
        }
        m
    }
}

impl From<Map<String, Value>> for Dict {
    /// An owned map becomes an overlay with no base, so existing callers that
    /// hand core a `Map` keep working unchanged.
    fn from(m: Map<String, Value>) -> Self {
        Self { base: None, overlay: m }
    }
}

impl<'a> IntoIterator for &'a Dict {
    type Item = (&'a String, &'a Value);
    type IntoIter = Box<dyn Iterator<Item = (&'a String, &'a Value)> + 'a>;
    fn into_iter(self) -> Self::IntoIter {
        Box::new(self.iter())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn base() -> Arc<Map<String, Value>> {
        let mut m = Map::new();
        m.insert("site_name".into(), json!("mgrosvenor.com"));
        m.insert("year".into(), json!("1999"));
        Arc::new(m)
    }

    #[test]
    fn the_overlay_shadows_the_base() {
        let mut d = Dict::with_base(base());
        assert_eq!(d.get("year").unwrap(), &json!("1999"));
        d.insert("year".into(), json!("2026"));
        assert_eq!(d.get("year").unwrap(), &json!("2026"));
        // The base is untouched and still shared.
        assert_eq!(base().get("year").unwrap(), &json!("1999"));
    }

    /// The precedence that step 8 of `build_dict` calls load-bearing: a params
    /// file must not be able to override a built-in. Params files are in the
    /// base, built-ins in the overlay, so the overlay winning *is* that rule.
    #[test]
    fn a_base_entry_can_never_override_an_overlay_one() {
        let mut m = Map::new();
        m.insert("year".into(), json!("a params file tried to set this"));
        m.insert("request_path".into(), json!("/wrong"));
        m.insert("datetime".into(), json!("never"));
        let mut d = Dict::with_base(Arc::new(m));

        d.insert("year".into(), json!("2026"));
        d.insert("request_path".into(), json!("/right"));
        d.insert("datetime".into(), json!("2026-09-12T00:00:00Z"));

        assert_eq!(d.get("year").unwrap(), &json!("2026"));
        assert_eq!(d.get("request_path").unwrap(), &json!("/right"));
        assert_eq!(d.get("datetime").unwrap(), &json!("2026-09-12T00:00:00Z"));
    }

    #[test]
    fn iter_yields_every_key_once_with_the_overlay_winning() {
        let mut d = Dict::with_base(base());
        d.insert("year".into(), json!("2026"));
        d.insert("extra".into(), json!(1));

        let mut seen: Vec<(String, Value)> =
            d.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        seen.sort_by(|a, b| a.0.cmp(&b.0));

        assert_eq!(
            seen,
            vec![
                ("extra".to_string(), json!(1)),
                ("site_name".to_string(), json!("mgrosvenor.com")),
                ("year".to_string(), json!("2026")),
            ]
        );
        assert_eq!(d.len(), 3, "a shadowed key is one key, not two");
    }

    #[test]
    fn a_dict_with_no_base_behaves_like_the_map_it_replaced() {
        let mut m = Map::new();
        m.insert("a".into(), json!(1));
        let d: Dict = m.into();
        assert_eq!(d.get("a").unwrap(), &json!(1));
        assert_eq!(d.get("missing"), None);
        assert_eq!(d.len(), 1);
        assert!(!d.is_empty());
        assert!(Dict::new().is_empty());
    }

    #[test]
    fn to_map_flattens_both_layers() {
        let mut d = Dict::with_base(base());
        d.insert("year".into(), json!("2026"));
        let m = d.to_map();
        assert_eq!(m.get("site_name").unwrap(), &json!("mgrosvenor.com"));
        assert_eq!(m.get("year").unwrap(), &json!("2026"));
        assert_eq!(m.len(), 2);
    }

    /// The whole point: cloning a request dictionary must not copy the site's
    /// content. A clone shares the base rather than duplicating it.
    #[test]
    fn cloning_shares_the_base_rather_than_copying_it() {
        let b = base();
        let d = Dict::with_base(Arc::clone(&b));
        assert_eq!(Arc::strong_count(&b), 2);
        let d2 = d.clone();
        assert_eq!(Arc::strong_count(&b), 3, "the clone shares, it does not copy");
        drop(d2);
        assert_eq!(Arc::strong_count(&b), 2);
        drop(d);
        assert_eq!(Arc::strong_count(&b), 1);
    }
}
