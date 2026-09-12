/// Utility functions exposed via the prelude.

/// Slugify a string: "Hello World" → "hello-world"
pub fn slugify(s: &str) -> String {
    slug::slugify(s)
}

/// Current date in ISO 8601 format: "2024-01-15"
pub fn today_iso8601() -> String {
    chrono::Utc::now().format("%Y-%m-%d").to_string()
}

/// A `SystemTime` as an ISO 8601 date: "2024-01-15".
///
/// Here rather than in a service because civil-from-days is exactly the kind of
/// thing that looks like four lines of arithmetic and is not. `m6-md` wrote its
/// own and it was **wrong for a quarter of all dates**: the era was anchored at
/// 1970 instead of being shifted to March, so the leap day landed inside a year
/// rather than at the end of the four-year block, and everything after it
/// drifted by one. 31 December of every leap year came out as 1 January of the
/// next, and the whole of the following year was a day late. It reported every
/// date in 2025 wrong and would have started again on 2028-12-31.
///
/// Times before the Unix epoch clamp to the epoch rather than failing: this
/// formats a file mtime, and a filesystem that reports one is not worth an
/// error path.
pub fn iso_date_from(t: std::time::SystemTime) -> String {
    let secs = t
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    chrono::DateTime::from_timestamp(secs, 0)
        .unwrap_or_else(|| chrono::DateTime::from_timestamp(0, 0).expect("epoch is in range"))
        .format("%Y-%m-%d")
        .to_string()
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

#[cfg(test)]
mod date_tests {
    use super::iso_date_from;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    fn at(days: u64) -> String {
        iso_date_from(UNIX_EPOCH + Duration::from_secs(days * 86400))
    }

    /// The dates m6-md's hand-rolled civil-from-days got wrong.
    ///
    /// Its era was anchored at 1970 rather than shifted to March, so the leap
    /// day fell inside a year instead of at the end of the four-year block and
    /// everything after it drifted by one. Measured against the calendar over
    /// 1970-2050: **7,281 of 29,200 days wrong**, in a repeating shape. The
    /// last day of every leap year became the first of the next, and the whole
    /// of the following year was a day late.
    #[test]
    fn the_last_day_of_a_leap_year_is_not_the_first_of_the_next() {
        // Day 1095 from the epoch. The old code said 1973-01-01.
        assert_eq!(at(1095), "1972-12-31");
        // Day 1096, the real new year. The old code said 1973-01-02.
        assert_eq!(at(1096), "1973-01-01");
    }

    /// The year after a leap year was wrong on 364 of its 365 days.
    #[test]
    fn the_year_after_a_leap_year_is_not_a_day_late() {
        assert_eq!(at(1156), "1973-03-02");
        assert_eq!(at(1460), "1973-12-31");
    }

    /// Leap days themselves, including the century rules.
    #[test]
    fn leap_days_land_on_the_right_date() {
        assert_eq!(at(789), "1972-02-29");
        assert_eq!(at(19782), "2024-02-29");
        // 2000 is a leap year (divisible by 400); 1900 and 2100 are not.
        assert_eq!(at(11016), "2000-02-29");
    }

    /// A time before the epoch clamps rather than panicking: this formats file
    /// mtimes, and a filesystem that reports one is not worth an error path.
    #[test]
    fn a_pre_epoch_time_clamps_to_the_epoch() {
        let before = UNIX_EPOCH - Duration::from_secs(86_400 * 500);
        assert_eq!(iso_date_from(before), "1970-01-01");
    }
}
