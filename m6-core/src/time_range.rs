//! A `from` and `to` time range read from a query string.
//!
//! A website that serves rows for a period (a log page, a report, a feed) needs
//! the same few decisions every time: which format the times come in, whether
//! the end is included, how long a range may be, and what to say when the
//! client gets it wrong. This module makes them once.
//!
//! # The rules
//!
//! - Both `from` and `to` are required and each may appear once.
//! - A value is an RFC 3339 date-time in UTC: `2026-01-01T00:00:00Z`, with an
//!   uppercase `T` and a final `Z`. Fractional seconds are accepted. An offset
//!   such as `+10:00` is refused, so a range means one thing everywhere.
//! - The range is half open, `[from, to)`: a row at exactly `from` is in, a row
//!   at exactly `to` is out. Two adjacent ranges never share a row.
//! - `from` is at or after 1970-01-01T00:00:00Z, `to` is later than `from`, and
//!   the span is at most `max_span` ([`DEFAULT_MAX_SPAN`] is 31 days).
//! - Other query keys are ignored, so a cursor or a filter passes through.
//!
//! # Refusals
//!
//! [`TimeRangeError::to_response`] is a 400 whose body says what to fix and
//! never repeats what the client sent, so a hostile value cannot be reflected
//! into a page.
//!
//! # Integer bounds
//!
//! [`TimeRange::from_unix_ms`], [`TimeRange::to_unix_ms`] and the `_s` pair
//! round **up** to the next whole unit. For integer timestamps,
//! `from_ms <= t < to_ms` is then exactly `[from, to)`. For example a `from` of
//! 10:00:00.0004 gives 10:00:00.001, the first whole millisecond at or after
//! it, and a `to` of 10:00:00.0004 gives 10:00:00.001, the first whole
//! millisecond that is not before it, so that millisecond is out.
//!
//! [`rfc3339_utc_ms`] formats a millisecond timestamp the same way the parser
//! reads one.

use std::fmt;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::request::{single_query_value, url_decode, RepeatedKey, Request};
use crate::response::Response;

/// The longest range accepted unless a service asks for another: 31 days.
pub const DEFAULT_MAX_SPAN: Duration = Duration::from_secs(31 * 86_400);

/// A half-open time range `[from, to)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimeRange {
    pub from: SystemTime,
    pub to: SystemTime,
}

/// Why a range was refused. The `&'static str` is the parameter name, `from`
/// or `to`. Nothing here holds the client's value.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TimeRangeError {
    #[error("missing `{0}`: give from and to as RFC 3339 UTC times, for example ?from=2026-01-01T00:00:00Z&to=2026-01-02T00:00:00Z")]
    Missing(&'static str),
    #[error("`{0}` given more than once")]
    Repeated(&'static str),
    #[error("`{0}` is empty")]
    Empty(&'static str),
    #[error("`{0}` is not an RFC 3339 date-time such as 2026-01-01T00:00:00Z")]
    Malformed(&'static str),
    #[error("`{0}` must be UTC and end in Z")]
    NotUtc(&'static str),
    #[error("`{0}` is before 1970-01-01T00:00:00Z")]
    BeforeEpoch(&'static str),
    #[error("`to` must be later than `from`: the range is [from, to)")]
    NotAfter,
    #[error("the range is longer than the maximum of {} days", .max.as_secs() / 86_400)]
    TooLong { max: Duration },
}

impl TimeRangeError {
    /// A 400 with a plain-text explanation that never echoes the input.
    pub fn to_response(&self) -> Response {
        Response::refusal(self)
    }
}

/// The value of the one query key `key`, url-decoded.
fn single(query: &str, key: &'static str) -> Result<String, TimeRangeError> {
    single_query_value(query, key)
        .map_err(|RepeatedKey| TimeRangeError::Repeated(key))?
        .map(url_decode)
        .ok_or(TimeRangeError::Missing(key))
}

fn parse_utc(value: &str, name: &'static str) -> Result<SystemTime, TimeRangeError> {
    if value.is_empty() {
        return Err(TimeRangeError::Empty(name));
    }
    let parsed = chrono::DateTime::parse_from_rfc3339(value);
    if !value.ends_with('Z') {
        return Err(if parsed.is_ok() {
            TimeRangeError::NotUtc(name)
        } else {
            TimeRangeError::Malformed(name)
        });
    }
    let dt = parsed.map_err(|_| TimeRangeError::Malformed(name))?;
    // chrono accepts a space or a lowercase `t` between date and time, and
    // reports a leap second as a nanosecond count of 1e9 or more.
    if value.as_bytes().get(10) != Some(&b'T') || dt.timestamp_subsec_nanos() >= 1_000_000_000 {
        return Err(TimeRangeError::Malformed(name));
    }
    let secs = dt.timestamp();
    if secs < 0 {
        return Err(TimeRangeError::BeforeEpoch(name));
    }
    Ok(UNIX_EPOCH + Duration::new(secs as u64, dt.timestamp_subsec_nanos()))
}

fn nanos(t: SystemTime) -> u128 {
    t.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos())
}

fn ceil_div(n: u128, d: u128) -> i64 {
    n.div_ceil(d).min(i64::MAX as u128) as i64
}

impl TimeRange {
    /// Read `from` and `to` out of a raw query string (no leading `?`).
    pub fn from_query(query: &str, max_span: Duration) -> Result<Self, TimeRangeError> {
        let from_raw = single(query, "from")?;
        let to_raw = single(query, "to")?;
        let from = parse_utc(&from_raw, "from")?;
        let to = parse_utc(&to_raw, "to")?;
        let span = to
            .duration_since(from)
            .map_err(|_| TimeRangeError::NotAfter)?;
        if span.is_zero() {
            return Err(TimeRangeError::NotAfter);
        }
        if span > max_span {
            return Err(TimeRangeError::TooLong { max: max_span });
        }
        Ok(Self { from, to })
    }

    /// [`from_query`](Self::from_query) on the request's query string.
    pub fn from_request(req: &Request, max_span: Duration) -> Result<Self, TimeRangeError> {
        Self::from_query(req.query(), max_span)
    }

    /// True when `from <= t < to`.
    pub fn contains(&self, t: SystemTime) -> bool {
        self.from <= t && t < self.to
    }

    /// `to - from`. Always positive.
    pub fn span(&self) -> Duration {
        self.to.duration_since(self.from).unwrap_or_default()
    }

    /// `from` in milliseconds since the epoch, rounded up.
    pub fn from_unix_ms(&self) -> i64 {
        ceil_div(nanos(self.from), 1_000_000)
    }

    /// `to` in milliseconds since the epoch, rounded up.
    pub fn to_unix_ms(&self) -> i64 {
        ceil_div(nanos(self.to), 1_000_000)
    }

    /// `from` in seconds since the epoch, rounded up.
    pub fn from_unix_s(&self) -> i64 {
        ceil_div(nanos(self.from), 1_000_000_000)
    }

    /// `to` in seconds since the epoch, rounded up.
    pub fn to_unix_s(&self) -> i64 {
        ceil_div(nanos(self.to), 1_000_000_000)
    }
}

/// A millisecond Unix timestamp as `2026-01-01T00:00:00.123Z`.
///
/// Lazy: it returns a value that formats when displayed, so a service can
/// serialise it into a row with `collect_str` and allocate no `String` per
/// line. A negative or out-of-range input gives the epoch rather than failing.
pub fn rfc3339_utc_ms(unix_ms: i64) -> impl fmt::Display {
    let dt = if unix_ms < 0 {
        None
    } else {
        chrono::DateTime::from_timestamp_millis(unix_ms)
    }
    .unwrap_or(chrono::DateTime::UNIX_EPOCH);
    dt.format("%Y-%m-%dT%H:%M:%S%.3fZ")
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: Duration = Duration::from_secs(86_400);

    fn parse(q: &str) -> Result<TimeRange, TimeRangeError> {
        TimeRange::from_query(q, DEFAULT_MAX_SPAN)
    }

    fn at(secs: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(secs)
    }

    #[test]
    fn a_valid_range_parses() {
        let r = parse("from=2026-01-01T00:00:00Z&to=2026-01-02T00:00:00Z").unwrap();
        assert_eq!(r.from_unix_s(), 1_767_225_600);
        assert_eq!(r.to_unix_s(), 1_767_225_600 + 86_400);
        assert_eq!(r.span(), DAY);
    }

    #[test]
    fn percent_encoded_colons_parse() {
        let r = parse("from=2026-01-01T00%3A00%3A00Z&to=2026-01-01T06%3a00%3a00Z").unwrap();
        assert_eq!(r.span(), Duration::from_secs(6 * 3600));
    }

    #[test]
    fn fractional_seconds_parse() {
        let r = parse("from=2026-01-01T00:00:00.123Z&to=2026-01-01T00:00:01.5Z").unwrap();
        assert_eq!(r.span(), Duration::from_millis(1377));
    }

    #[test]
    fn missing_from_or_to_is_400() {
        assert_eq!(
            parse("to=2026-01-02T00:00:00Z"),
            Err(TimeRangeError::Missing("from"))
        );
        assert_eq!(
            parse("from=2026-01-01T00:00:00Z"),
            Err(TimeRangeError::Missing("to"))
        );
        assert_eq!(parse(""), Err(TimeRangeError::Missing("from")));
        let resp = TimeRangeError::Missing("to").to_response();
        assert_eq!(resp.status, 400);
    }

    #[test]
    fn a_repeated_param_is_400() {
        assert_eq!(
            parse("from=2026-01-01T00:00:00Z&from=2026-01-01T00:00:00Z&to=2026-01-02T00:00:00Z"),
            Err(TimeRangeError::Repeated("from"))
        );
        assert_eq!(
            parse("from=2026-01-01T00:00:00Z&to=2026-01-02T00:00:00Z&to=2026-01-03T00:00:00Z"),
            Err(TimeRangeError::Repeated("to"))
        );
    }

    #[test]
    fn an_empty_value_is_400() {
        assert_eq!(
            parse("from=&to=2026-01-02T00:00:00Z"),
            Err(TimeRangeError::Empty("from"))
        );
        assert_eq!(
            parse("from=2026-01-01T00:00:00Z&to"),
            Err(TimeRangeError::Empty("to"))
        );
    }

    #[test]
    fn a_date_without_time_is_400() {
        assert_eq!(
            parse("from=2026-01-01&to=2026-01-02T00:00:00Z"),
            Err(TimeRangeError::Malformed("from"))
        );
        assert_eq!(
            parse("from=2026-01-01T00:00:00Z&to=2026-01-02Z"),
            Err(TimeRangeError::Malformed("to"))
        );
    }

    #[test]
    fn an_offset_other_than_z_is_400() {
        for off in ["%2B10:00", "%2B00:00", "-05:00"] {
            let q = format!("from=2026-01-01T00:00:00{off}&to=2026-01-02T00:00:00Z");
            assert_eq!(parse(&q), Err(TimeRangeError::NotUtc("from")), "{off}");
        }
        // An unencoded `+` decodes to a space, which is not a date-time.
        assert!(parse("from=2026-01-01T00:00:00+10:00&to=2026-01-02T00:00:00Z").is_err());
    }

    #[test]
    fn lowercase_t_or_z_is_400() {
        assert!(parse("from=2026-01-01t00:00:00Z&to=2026-01-02T00:00:00Z").is_err());
        assert!(parse("from=2026-01-01T00:00:00z&to=2026-01-02T00:00:00Z").is_err());
        assert!(parse("from=2026-01-01%2000:00:00Z&to=2026-01-02T00:00:00Z").is_err());
    }

    #[test]
    fn a_leap_second_is_400() {
        assert_eq!(
            parse("from=2016-12-31T23:59:60Z&to=2017-01-01T12:00:00Z"),
            Err(TimeRangeError::Malformed("from"))
        );
    }

    #[test]
    fn before_1970_is_400() {
        assert_eq!(
            parse("from=1969-12-31T23:59:59Z&to=2026-01-02T00:00:00Z"),
            Err(TimeRangeError::BeforeEpoch("from"))
        );
        assert_eq!(
            parse("from=1969-12-31T23:59:59.5Z&to=1970-01-01T00:00:00Z"),
            Err(TimeRangeError::BeforeEpoch("from"))
        );
        assert!(parse("from=1970-01-01T00:00:00Z&to=1970-01-02T00:00:00Z").is_ok());
    }

    #[test]
    fn to_equal_to_from_is_400() {
        assert_eq!(
            parse("from=2026-01-01T00:00:00Z&to=2026-01-01T00:00:00Z"),
            Err(TimeRangeError::NotAfter)
        );
    }

    #[test]
    fn to_before_from_is_400() {
        assert_eq!(
            parse("from=2026-01-02T00:00:00Z&to=2026-01-01T00:00:00Z"),
            Err(TimeRangeError::NotAfter)
        );
    }

    #[test]
    fn a_span_of_exactly_the_max_is_accepted_and_one_second_more_is_400() {
        let max = Duration::from_secs(2 * 86_400);
        assert!(
            TimeRange::from_query("from=2026-01-01T00:00:00Z&to=2026-01-03T00:00:00Z", max).is_ok()
        );
        assert_eq!(
            TimeRange::from_query("from=2026-01-01T00:00:00Z&to=2026-01-03T00:00:01Z", max),
            Err(TimeRangeError::TooLong { max })
        );
    }

    #[test]
    fn other_params_are_ignored() {
        let r = parse("cursor=abc&from=2026-01-01T00:00:00Z&node=edge-a&to=2026-01-02T00:00:00Z&x")
            .unwrap();
        assert_eq!(r.span(), DAY);
    }

    #[test]
    fn bounds_round_up_so_the_range_is_half_open() {
        let r = TimeRange {
            from: UNIX_EPOCH + Duration::new(10, 400_000),
            to: UNIX_EPOCH + Duration::new(20, 1_000_000),
        };
        assert_eq!(r.from_unix_ms(), 10_001);
        assert_eq!(r.to_unix_ms(), 20_001);
        assert_eq!(r.from_unix_s(), 11);
        assert_eq!(r.to_unix_s(), 21);
        // An exact boundary is not moved.
        let e = TimeRange {
            from: at(10),
            to: at(20),
        };
        assert_eq!((e.from_unix_ms(), e.to_unix_ms()), (10_000, 20_000));
        assert_eq!((e.from_unix_s(), e.to_unix_s()), (10, 20));
        // A stored integer t is in the range exactly when the SystemTime is.
        for t_ms in [10_000_i64, 10_001, 19_999, 20_000, 20_001] {
            let t = UNIX_EPOCH + Duration::from_millis(t_ms as u64);
            assert_eq!(
                r.contains(t),
                r.from_unix_ms() <= t_ms && t_ms < r.to_unix_ms(),
                "{t_ms}"
            );
        }
    }

    #[test]
    fn contains_is_half_open() {
        let r = TimeRange {
            from: at(10),
            to: at(20),
        };
        assert!(!r.contains(at(9)));
        assert!(r.contains(at(10)));
        assert!(r.contains(at(19)));
        assert!(!r.contains(at(20)));
    }

    #[test]
    fn the_400_body_never_echoes_the_value() {
        let marker = "ZQXMARKER7731";
        let queries = [
            format!("from={marker}&to=2026-01-02T00:00:00Z"),
            format!("from=2026-01-01T00:00:00Z&to={marker}"),
            format!("from=2026-01-01T00:00:00%2B{marker}&to=2026-01-02T00:00:00Z"),
            format!("from={marker}&from={marker}&to=2026-01-02T00:00:00Z"),
            format!("{marker}=1"),
        ];
        for q in queries {
            let err = parse(&q).unwrap_err();
            let resp = err.to_response();
            assert_eq!(resp.status, 400);
            let body = String::from_utf8(resp.body.as_bytes().unwrap().to_vec()).unwrap();
            assert!(!body.contains(marker), "{q}: {body}");
            assert!(body.ends_with('\n'));
            let h = |n: &str| {
                resp.headers
                    .iter()
                    .find(|(k, _)| k == n)
                    .map(|(_, v)| v.as_str())
            };
            assert_eq!(h("Content-Type"), Some("text/plain; charset=utf-8"));
            assert_eq!(h("Cache-Control"), Some("no-store"));
            assert_eq!(h("X-Content-Type-Options"), Some("nosniff"));
        }
    }

    #[test]
    fn rfc3339_utc_ms_formats_milliseconds_and_z() {
        assert_eq!(rfc3339_utc_ms(0).to_string(), "1970-01-01T00:00:00.000Z");
        assert_eq!(
            rfc3339_utc_ms(1_767_225_600_123).to_string(),
            "2026-01-01T00:00:00.123Z"
        );
    }

    #[test]
    fn rfc3339_utc_ms_clamps_out_of_range_to_the_epoch() {
        assert_eq!(rfc3339_utc_ms(-1).to_string(), "1970-01-01T00:00:00.000Z");
        assert_eq!(
            rfc3339_utc_ms(i64::MAX).to_string(),
            "1970-01-01T00:00:00.000Z"
        );
    }
}
