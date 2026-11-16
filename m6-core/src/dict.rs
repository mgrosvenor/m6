//! The request dictionary: shared base, per-request overlay.
//!
//! Building a request's dictionary used to start from an empty map and copy
//! the whole of the service's static configuration into it, per request. On
//! a content-driven site that is the config file's keys plus its whole
//! catalogue, tens of kilobytes and thousands of nodes, deep-copied several
//! times to produce a map that was identical for every request.
//!
//! Nothing in that is per-request. So it is not copied per request: the base
//! is built once per route at start and shared behind an `Arc`, and only the
//! handful of entries that genuinely differ are allocated per request.
//!
//! **Precedence is why this is a type rather than two fields.** `build_dict`
//! merges twelve sources in a fixed order, and the order decides the result:
//! built-in keys go in *after* params files so that a params file cannot
//! override `year`, `datetime` or `request_path`. Splitting the sources across
//! two layers preserves that only if the split respects it, so it does:
//!
//! - **base**, built once at start: config keys, global params files, and
//!   the route's *static* params files, merged in that order.
//! - **overlay**, built per request: dynamic params files (those whose path
//!   holds a `{placeholder}` and so resolve per request), then path params,
//!   query, form fields, cookies, built-ins, auth claims, flash.
//!
//! The overlay always wins over the base, and within the overlay later
//! insertions win, so both halves of the original ordering survive: a params
//! file still loses to a built-in, whether it was static or dynamic.

use std::borrow::Cow;
use std::cell::RefCell;
use std::sync::{Arc, OnceLock};

use serde_json::{Map, Value};

use crate::limits::{Limit, REQUEST_DICT};

/// The built-in keys whose `String` values are kept from one request to the
/// next, so a request writes them into a buffer that already exists.
const KEPT: [&str; 3] = ["request_path", "datetime", "year"];

/// The longest kept built-in value, in bytes. A `request_path` is a request
/// line's target, so it is at most 8 KiB.
const KEPT_BYTES: usize = 8 * 1024;

/// How many overlays one thread keeps for the next request. One per request in
/// flight on the service's loop, which is one, and a few for a caller that holds
/// a request beyond its answer.
const SPARE_OVERLAYS: usize = 4;

static ENTRY_LIMIT: OnceLock<Limit> = OnceLock::new();

/// Set the `request_dict` limit every dictionary follows. Start only, once: a
/// second call changes nothing.
pub fn set_entry_limit(limit: &Limit) {
    let _ = ENTRY_LIMIT.set(limit.clone());
}

/// Make this thread's spare overlays. Start only.
pub fn warm_spare_overlays() {
    SPARE.with(|_| {});
}

/// The `request_dict` limit dictionaries follow.
pub fn entry_limit() -> &'static Limit {
    ENTRY_LIMIT.get_or_init(|| Limit::builtin(&REQUEST_DICT))
}

thread_local! {
    /// Overlays whose last request is over: cleared, with their capacity and
    /// their built-in `String`s kept. A service runs one loop on one thread, so
    /// this is the service's own reusable dictionary.
    ///
    /// Made with room for every spare it keeps, on the thread's first use, so
    /// putting an overlay back never allocates.
    static SPARE: RefCell<Vec<Overlay>> = RefCell::new(Vec::with_capacity(SPARE_OVERLAYS));
}

type Entry = (Cow<'static, str>, Value);

/// The per-request layer of a [`Dict`]: a list of entries in insertion order.
///
/// `kept` holds the built-ins of an earlier request, keyed and holding a
/// `String` whose buffer is reused. A later insert of the same built-in key
/// takes its entry from there and writes into its buffer, so a page that comes
/// after a page allocates nothing for them.
#[derive(Debug, Default)]
struct Overlay {
    entries: Vec<Entry>,
    kept: Vec<Entry>,
    /// An insert was refused past the `request_dict` maximum.
    overflowed: bool,
}

impl Overlay {
    /// An overlay from this thread's spare ones, or a new one at the limit's
    /// `start`.
    fn take() -> Self {
        SPARE
            .with(|s| s.borrow_mut().pop())
            .unwrap_or_else(|| Overlay {
                entries: Vec::with_capacity(entry_limit().start()),
                kept: Vec::with_capacity(KEPT.len()),
                overflowed: false,
            })
    }

    fn get(&self, key: &str) -> Option<&Value> {
        self.entries.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    /// Room for one more entry, by doubling and clamped at the limit's maximum.
    /// `false` past the maximum.
    fn make_room(&mut self) -> bool {
        let limit = entry_limit();
        if self.entries.len() >= limit.max() {
            self.overflowed = true;
            return false;
        }
        let cap = self.entries.capacity();
        if self.entries.len() == cap {
            if let Some(size) = limit.grown(cap, cap + 1) {
                self.entries.reserve_exact(size - self.entries.len());
                limit.log_growth(size);
            }
        }
        true
    }

    fn insert(&mut self, key: Cow<'static, str>, value: Value) {
        if let Some(slot) = self.entries.iter_mut().find(|(k, _)| *k == key) {
            slot.1 = value;
            return;
        }
        if self.make_room() {
            self.entries.push((key, value));
        }
    }

    /// Insert a string, writing into the buffer a kept built-in of the same key
    /// holds when there is one.
    fn insert_str(&mut self, key: &'static str, text: &str) {
        if let Some(slot) = self.entries.iter_mut().find(|(k, _)| *k == key) {
            slot.1 = Value::String(text.to_string());
            return;
        }
        if !self.make_room() {
            return;
        }
        match self.kept.iter().position(|(k, _)| *k == key) {
            Some(at) => {
                let mut entry = self.kept.swap_remove(at);
                if let Value::String(buf) = &mut entry.1 {
                    buf.clear();
                    buf.push_str(text);
                }
                self.entries.push(entry);
            }
            None => self
                .entries
                .push((Cow::Borrowed(key), Value::String(text.to_string()))),
        }
    }

    /// End of the request: keep the built-ins' buffers and drop the rest, so
    /// nothing of this request's content waits in a spare overlay.
    fn recycle(mut self) {
        self.kept.clear();
        for entry in self.entries.drain(..) {
            let keep = KEPT.contains(&entry.0.as_ref())
                && matches!(&entry.1, Value::String(s) if s.capacity() <= KEPT_BYTES);
            if keep {
                self.kept.push(entry);
            }
        }
        self.overflowed = false;
        SPARE.with(|s| {
            let mut spare = s.borrow_mut();
            if spare.len() < SPARE_OVERLAYS {
                spare.push(self);
            }
        });
    }
}

/// A request dictionary.
///
/// Cloning is cheap by construction: an `Arc` bump for the base and a copy of
/// the overlay, which holds a dozen short entries rather than the site's
/// content.
#[derive(Debug, Default)]
pub struct Dict {
    base: Option<Arc<Map<String, Value>>>,
    overlay: Option<Overlay>,
}

impl Clone for Dict {
    fn clone(&self) -> Self {
        Dict {
            base: self.base.clone(),
            overlay: self.overlay.as_ref().map(|o| Overlay {
                entries: o.entries.clone(),
                kept: Vec::new(),
                overflowed: o.overflowed,
            }),
        }
    }
}

impl Drop for Dict {
    fn drop(&mut self) {
        if let Some(overlay) = self.overlay.take() {
            overlay.recycle();
        }
    }
}

impl Dict {
    /// A dictionary layered over a shared base. Its overlay is this thread's
    /// spare one, so a request after the first allocates nothing for it.
    pub fn with_base(base: Arc<Map<String, Value>>) -> Self {
        Self {
            base: Some(base),
            overlay: Some(Overlay::take()),
        }
    }

    /// A dictionary with no base, for a caller that has no site state:
    /// tests, and handlers building a context by hand.
    pub fn new() -> Self {
        Self {
            base: None,
            overlay: Some(Overlay::take()),
        }
    }

    fn overlay(&self) -> &Overlay {
        self.overlay
            .as_ref()
            .expect("a dictionary has its overlay until it is dropped")
    }

    fn overlay_mut(&mut self) -> &mut Overlay {
        self.overlay
            .as_mut()
            .expect("a dictionary has its overlay until it is dropped")
    }

    /// Insert into the overlay, where it shadows any base entry of the same
    /// name. This is the only way to write, which is what makes the base
    /// genuinely immutable and shareable.
    ///
    /// Past the `request_dict` maximum the entry is not kept and
    /// [`Dict::overflowed`] answers true.
    pub fn insert(&mut self, key: impl Into<Cow<'static, str>>, value: Value) {
        self.overlay_mut().insert(key.into(), value);
    }

    /// Insert a built-in string. The text is written into the buffer the same
    /// key held for the last request, when there is one, so it allocates
    /// nothing once warm.
    pub fn insert_str(&mut self, key: &'static str, text: &str) {
        self.overlay_mut().insert_str(key, text);
    }

    /// Whether an insert was refused for passing the `request_dict` maximum.
    pub fn overflowed(&self) -> bool {
        self.overlay().overflowed
    }

    /// Overlay first, then base.
    pub fn get(&self, key: &str) -> Option<&Value> {
        match self.overlay().get(key) {
            Some(v) => Some(v),
            None => self.base.as_ref()?.get(key),
        }
    }

    pub fn contains_key(&self, key: &str) -> bool {
        self.get(key).is_some()
    }

    /// Distinct keys across both layers.
    pub fn len(&self) -> usize {
        let overlay = self.overlay();
        let base = match &self.base {
            Some(b) => b
                .keys()
                .filter(|k| overlay.get(k.as_str()).is_none())
                .count(),
            None => 0,
        };
        base + overlay.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Every visible entry, overlay first, then the base entries it does not
    /// shadow. Order between the two layers is not otherwise meaningful, and
    /// no consumer depends on it: the renderer builds a keyed context.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &Value)> {
        let overlay = self.overlay();
        let own = overlay.entries.iter().map(|(k, v)| (k.as_ref(), v));
        let base = self
            .base
            .iter()
            .flat_map(|b| b.iter())
            .filter(|(k, _)| overlay.get(k.as_str()).is_none())
            .map(|(k, v)| (k.as_str(), v));
        own.chain(base)
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
        for (k, v) in &self.overlay().entries {
            m.insert(k.to_string(), v.clone());
        }
        m
    }
}

impl From<Map<String, Value>> for Dict {
    /// An owned map becomes an overlay with no base, so existing callers that
    /// hand core a `Map` keep working unchanged.
    fn from(m: Map<String, Value>) -> Self {
        let mut dict = Dict::new();
        for (k, v) in m {
            dict.insert(Cow::Owned(k), v);
        }
        dict
    }
}

impl<'a> IntoIterator for &'a Dict {
    type Item = (&'a str, &'a Value);
    type IntoIter = Box<dyn Iterator<Item = (&'a str, &'a Value)> + 'a>;
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
        m.insert("site_name".into(), json!("example.com"));
        m.insert("year".into(), json!("1999"));
        Arc::new(m)
    }

    #[test]
    fn the_overlay_shadows_the_base() {
        let mut d = Dict::with_base(base());
        assert_eq!(d.get("year").unwrap(), &json!("1999"));
        d.insert("year", json!("2026"));
        assert_eq!(d.get("year").unwrap(), &json!("2026"));
        // The base is untouched and still shared.
        assert_eq!(base().get("year").unwrap(), &json!("1999"));
    }

    /// The precedence that step 8 of `build_dict` depends on: a params file
    /// must not be able to override a built-in. Params files are in the
    /// base, built-ins in the overlay, so the overlay winning *is* that rule.
    #[test]
    fn a_base_entry_can_never_override_an_overlay_one() {
        let mut m = Map::new();
        m.insert("year".into(), json!("a params file tried to set this"));
        m.insert("request_path".into(), json!("/wrong"));
        m.insert("datetime".into(), json!("never"));
        let mut d = Dict::with_base(Arc::new(m));

        d.insert("year", json!("2026"));
        d.insert("request_path", json!("/right"));
        d.insert("datetime", json!("2026-09-12T00:00:00Z"));

        assert_eq!(d.get("year").unwrap(), &json!("2026"));
        assert_eq!(d.get("request_path").unwrap(), &json!("/right"));
        assert_eq!(d.get("datetime").unwrap(), &json!("2026-09-12T00:00:00Z"));
    }

    #[test]
    fn iter_yields_every_key_once_with_the_overlay_winning() {
        let mut d = Dict::with_base(base());
        d.insert("year", json!("2026"));
        d.insert("extra", json!(1));

        let mut seen: Vec<(String, Value)> =
            d.iter().map(|(k, v)| (k.to_string(), v.clone())).collect();
        seen.sort_by(|a, b| a.0.cmp(&b.0));

        assert_eq!(
            seen,
            vec![
                ("extra".to_string(), json!(1)),
                ("site_name".to_string(), json!("example.com")),
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
        d.insert("year", json!("2026"));
        let m = d.to_map();
        assert_eq!(m.get("site_name").unwrap(), &json!("example.com"));
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
        assert_eq!(
            Arc::strong_count(&b),
            3,
            "the clone shares, it does not copy"
        );
        drop(d2);
        assert_eq!(Arc::strong_count(&b), 2);
        drop(d);
        assert_eq!(Arc::strong_count(&b), 1);
    }

    /// A built-in value is written into the buffer the last request left, and
    /// nothing else of the last request is visible.
    #[test]
    fn a_dictionary_reuses_the_last_ones_built_in_buffers_and_shows_nothing_else() {
        let mut first = Dict::with_base(base());
        first.insert_str("year", "2026");
        first.insert("secret", json!("from the first request"));
        let buffer = first.get("year").and_then(Value::as_str).unwrap().as_ptr();
        drop(first);

        let mut second = Dict::with_base(base());
        assert!(
            !second.contains_key("secret"),
            "a request sees none of the last one"
        );
        assert_eq!(
            second.get("year").unwrap(),
            &json!("1999"),
            "only the base shows"
        );
        second.insert_str("year", "2027");
        let reused = second.get("year").and_then(Value::as_str).unwrap().as_ptr();
        assert_eq!(buffer, reused, "the built-in's buffer is the last one's");
        assert_eq!(second.get("year").unwrap(), &json!("2027"));
    }

    /// Past the `request_dict` maximum an insert is refused and the dictionary
    /// says so, so a request cannot grow it without bound.
    #[test]
    fn an_insert_past_the_maximum_is_refused_and_reported() {
        let max = entry_limit().max();
        let mut d = Dict::new();
        for i in 0..max {
            d.insert(format!("k{i}"), json!(i));
        }
        assert!(!d.overflowed());
        d.insert("one too many", json!(0));
        assert!(d.overflowed());
        assert_eq!(d.len(), max, "the overlay never passed its maximum");
        assert!(!d.contains_key("one too many"));
    }

    /// A clone is its own dictionary: it keeps the entries it had, and the
    /// original's spare buffers go back when the original is dropped.
    #[test]
    fn a_clone_keeps_its_entries() {
        let mut d = Dict::with_base(base());
        d.insert("a", json!(1));
        let c = d.clone();
        drop(d);
        assert_eq!(c.get("a").unwrap(), &json!(1));
    }
}
