//! Reading and writing a header list, once.
//!
//! Core already had [`crate::http::header`] for "find the first one,
//! case-insensitively", and it was not used: eight or more sites across the
//! workspace wrote `.find(|(k, _)| k.eq_ignore_ascii_case(name))` inline
//! instead, including several inside core. That is a cheap kind of
//! duplication, but it is the kind that hides the expensive questions, and
//! there are two of those.
//!
//! # A field can appear more than once
//!
//! `find` returns the first. For `Host` that is all there is; for
//! `Set-Cookie`, `Via`, `Warning` or a request with two `Accept-Encoding`
//! lines, the first is one of several and silently using it drops the rest.
//! [`get_all`] is the honest read when a field may repeat.
//!
//! # Combining repeated fields is conditional, not automatic
//!
//! RFC 9110 5.3 permits a recipient to combine multiple field lines of the
//! same name into one comma-separated value **only when the field's value is
//! defined as a comma-separated list**. For anything else, combining changes
//! the meaning.
//!
//! `Set-Cookie` is the case everybody gets wrong. RFC 6265 3 is explicit that
//! it must not be folded: cookie values may themselves contain commas, so
//! joining two `Set-Cookie` lines produces one malformed cookie and silently
//! loses the other. [`combine`] refuses it rather than trusting the caller to
//! remember, and [`NEVER_COMBINED`] is the list.

use crate::http::HeaderSource;

/// Fields that must never be folded into one comma-separated line.
///
/// `Set-Cookie` is the one that matters and the one that is routinely got
/// wrong. Its value can contain commas (in an `Expires` date, for one), so a
/// combined pair is not merely ugly, it is unparseable.
pub const NEVER_COMBINED: &[&str] = &["set-cookie"];

/// Whether a field may be folded into a comma-separated line.
pub fn may_combine(name: &str) -> bool {
    !NEVER_COMBINED.iter().any(|n| name.eq_ignore_ascii_case(n))
}

/// The first value for `name`, case-insensitively.
///
/// The same thing [`crate::http::header`] does; here so that a caller reaching
/// for this module finds it without having to know the other name.
pub fn get<'a>(headers: &'a (impl HeaderSource + ?Sized), name: &str) -> Option<&'a str> {
    headers.find(name)
}

/// Every value for `name`, in the order sent.
pub fn get_all<'a>(
    headers: &'a [(String, String)],
    name: &'a str,
) -> impl Iterator<Item = &'a str> + 'a {
    headers
        .iter()
        .filter(move |(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

pub fn contains(headers: &(impl HeaderSource + ?Sized), name: &str) -> bool {
    headers.find(name).is_some()
}

/// All values for `name`, folded into one comma-separated string.
///
/// `None` when the field is absent **or** when it is one that must not be
/// combined; the two are distinguished by [`contains`]. Returning `None` for
/// `Set-Cookie` rather than a joined string is the point: a caller that wanted
/// the values gets nothing and goes looking, instead of getting something
/// plausible and wrong.
pub fn combine(headers: &[(String, String)], name: &str) -> Option<String> {
    if !may_combine(name) {
        return None;
    }
    let mut out: Option<String> = None;
    for v in get_all(headers, name) {
        match &mut out {
            None => out = Some(v.to_string()),
            Some(acc) => {
                acc.push_str(", ");
                acc.push_str(v);
            }
        }
    }
    out
}

/// Replace every occurrence of `name` with one line carrying `value`.
///
/// The replacement goes where the first one was, so header order is stable
/// across a `set`. Appending after a remove would move the field to the end
/// and make two otherwise identical responses differ byte for byte.
pub fn set(headers: &mut Vec<(String, String)>, name: &str, value: impl Into<String>) {
    let first = headers.iter().position(|(k, _)| k.eq_ignore_ascii_case(name));
    match first {
        Some(i) => {
            headers[i] = (name.to_string(), value.into());
            let mut j = i + 1;
            while j < headers.len() {
                if headers[j].0.eq_ignore_ascii_case(name) {
                    headers.remove(j);
                } else {
                    j += 1;
                }
            }
        }
        None => headers.push((name.to_string(), value.into())),
    }
}

/// Add a line without touching any existing one.
///
/// What `Set-Cookie` needs, and what `set` must not be used for.
pub fn append(headers: &mut Vec<(String, String)>, name: &str, value: impl Into<String>) {
    headers.push((name.to_string(), value.into()));
}

/// Set `name` only if it is not already present. Returns whether it was added.
///
/// The shape of "supply a default the handler did not": `Content-Type` and
/// `ETag` in `Response::send` both do this by hand today.
pub fn set_if_absent(
    headers: &mut Vec<(String, String)>,
    name: &str,
    value: impl Into<String>,
) -> bool {
    if headers.iter().any(|(k, _)| k.eq_ignore_ascii_case(name)) {
        return false;
    }
    headers.push((name.to_string(), value.into()));
    true
}

/// Remove every occurrence of `name`. Returns how many went.
pub fn remove(headers: &mut Vec<(String, String)>, name: &str) -> usize {
    let before = headers.len();
    headers.retain(|(k, _)| !k.eq_ignore_ascii_case(name));
    before - headers.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h() -> Vec<(String, String)> {
        vec![
            ("Host".into(), "example.com".into()),
            ("Set-Cookie".into(), "a=1; Path=/".into()),
            ("set-cookie".into(), "b=2; Path=/".into()),
            ("Accept-Encoding".into(), "gzip".into()),
            ("accept-encoding".into(), "br".into()),
        ]
    }

    #[test]
    fn get_is_case_insensitive_and_takes_the_first() {
        assert_eq!(get(&h()[..], "host"), Some("example.com"));
        assert_eq!(get(&h()[..], "HOST"), Some("example.com"));
        assert_eq!(get(&h()[..], "set-cookie"), Some("a=1; Path=/"));
        assert_eq!(get(&h()[..], "absent"), None);
    }

    /// The reason `get` alone is not enough: taking the first silently drops
    /// the rest, and for a repeated field that is data loss, not a shortcut.
    #[test]
    fn get_all_returns_every_occurrence() {
        let hs = h();
        let cookies: Vec<_> = get_all(&hs, "Set-Cookie").collect();
        assert_eq!(cookies, vec!["a=1; Path=/", "b=2; Path=/"]);
        let enc: Vec<_> = get_all(&hs, "ACCEPT-ENCODING").collect();
        assert_eq!(enc, vec!["gzip", "br"]);
    }

    /// RFC 9110 5.3 allows folding for list-valued fields.
    #[test]
    fn a_list_valued_field_combines() {
        assert_eq!(combine(&h(), "accept-encoding"), Some("gzip, br".to_string()));
    }

    /// RFC 6265 3: Set-Cookie must not be folded. A cookie value can contain a
    /// comma, so a combined pair is one malformed cookie and one lost cookie,
    /// and it looks fine until something tries to parse it.
    #[test]
    fn set_cookie_is_never_combined() {
        assert_eq!(combine(&h(), "Set-Cookie"), None);
        assert!(!may_combine("set-cookie"));
        assert!(!may_combine("Set-Cookie"));
        // And absent is distinguishable from refused.
        assert!(contains(&h()[..], "Set-Cookie"));
        assert!(!contains(&h()[..], "nonesuch"));
        assert_eq!(combine(&h(), "nonesuch"), None);
    }

    /// `set` replaces every duplicate, not just the first. Leaving a second
    /// line behind is how a response ends up asserting two content types.
    #[test]
    fn set_replaces_all_duplicates() {
        let mut hs = h();
        set(&mut hs, "Accept-Encoding", "identity");
        let enc: Vec<_> = get_all(&hs, "accept-encoding").collect();
        assert_eq!(enc, vec!["identity"]);
    }

    /// Position is preserved, so two responses built the same way are
    /// byte-identical rather than differing by field order.
    #[test]
    fn set_keeps_the_original_position() {
        let mut hs = h();
        set(&mut hs, "host", "other.example");
        assert_eq!(hs[0].0, "host");
        assert_eq!(hs[0].1, "other.example");
    }

    #[test]
    fn set_on_an_absent_field_appends() {
        let mut hs = vec![("A".to_string(), "1".to_string())];
        set(&mut hs, "B", "2");
        assert_eq!(hs.len(), 2);
        assert_eq!(get(&hs[..], "b"), Some("2"));
    }

    #[test]
    fn append_does_not_disturb_what_is_there() {
        let mut hs = h();
        append(&mut hs, "Set-Cookie", "c=3");
        let cookies: Vec<_> = get_all(&hs, "set-cookie").collect();
        assert_eq!(cookies, vec!["a=1; Path=/", "b=2; Path=/", "c=3"]);
    }

    #[test]
    fn set_if_absent_defers_to_the_caller() {
        let mut hs = vec![("Content-Type".to_string(), "text/plain".to_string())];
        assert!(!set_if_absent(&mut hs, "content-type", "text/html"));
        assert_eq!(get(&hs[..], "Content-Type"), Some("text/plain"));
        assert!(set_if_absent(&mut hs, "ETag", "\"abc\""));
        assert_eq!(get(&hs[..], "etag"), Some("\"abc\""));
    }

    #[test]
    fn remove_takes_every_occurrence_and_counts() {
        let mut hs = h();
        assert_eq!(remove(&mut hs, "SET-COOKIE"), 2);
        assert!(!contains(&hs[..], "set-cookie"));
        assert_eq!(remove(&mut hs, "set-cookie"), 0);
    }
}
