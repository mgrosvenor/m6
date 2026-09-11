/// Utility functions exposed via the prelude.

/// Slugify a string: "Hello World" → "hello-world"
pub fn slugify(s: &str) -> String {
    slug::slugify(s)
}

/// Current date in ISO 8601 format: "2024-01-15"
pub fn today_iso8601() -> String {
    chrono::Utc::now().format("%Y-%m-%d").to_string()
}

/// Current datetime in ISO 8601 format: "2024-01-15T10:30:00Z"
pub fn now_iso8601() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// A UTC timestamp `minutes` ago, as an ISO 8601 prefix.
///
/// For selecting a window out of a log whose timestamps are fixed-width UTC:
/// there, a string compare is a time compare, so a cutoff string is all a
/// reader needs and no date library has to reach the reader at all.
///
/// No trailing `Z`, deliberately. It is compared with `<` against timestamps
/// that carry sub-second precision (`...T08:00:00.123456Z`), and `Z` sorts
/// after `.`, so `"...T08:00:00Z" < "...T08:00:00.123456Z"` is false and the
/// first fraction of a second of the window would be dropped.
pub fn iso8601_minutes_ago(minutes: u64) -> String {
    let t = chrono::Utc::now() - chrono::Duration::minutes(minutes as i64);
    t.format("%Y-%m-%dT%H:%M:%S").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_slugify() {
        assert_eq!(slugify("Hello World"), "hello-world");
        assert_eq!(slugify("Rust is great!"), "rust-is-great");
    }

    /// The missing `Z` is load-bearing. A log timestamp carries sub-second
    /// precision, and `Z` sorts after `.` in ASCII, so a cutoff ending in `Z`
    /// would compare as later than any timestamp in the same second and drop
    /// the first fraction of a second of the window.
    #[test]
    fn the_cutoff_has_no_trailing_z() {
        let cutoff = iso8601_minutes_ago(60);
        assert!(!cutoff.ends_with('Z'), "got {cutoff}");
        assert_eq!(cutoff.len(), 19);

        let same_second_event = format!("{cutoff}.123456Z");
        assert!(
            same_second_event.as_str() >= cutoff.as_str(),
            "an event in the cutoff second must be inside the window"
        );
        let with_z = format!("{cutoff}Z");
        assert!(
            same_second_event.as_str() < with_z.as_str(),
            "and this is why the Z is not there"
        );
    }

    #[test]
    fn minutes_ago_is_in_the_past() {
        assert!(iso8601_minutes_ago(60) < now_iso8601());
        assert!(iso8601_minutes_ago(0) <= now_iso8601());
    }

    #[test]
    fn test_dates_not_empty() {
        assert!(!today_iso8601().is_empty());
        assert!(!now_iso8601().is_empty());
    }
}
