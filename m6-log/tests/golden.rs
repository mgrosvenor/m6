//! The golden lines are the format specification: real lines captured from
//! example 01's running stack before m6-log existed.

use std::fs;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use m6_log::{line_at, Level, Sink, Type};
use serde_json::Value;

/// One file per captured line shape, in `tests/golden`.
///
/// `perf` is the periodic metrics line: the whole document the old `/perf`
/// endpoint answered, nested under `fields.perf`. Its nested object is what
/// proves `Line::json` writes a value and not a string.
const GOLDEN: [&str; 7] = [
    "request",
    "monitor",
    "rate_limited",
    "operational_info",
    "operational_warn",
    "operational_error",
    "perf",
];

#[test]
fn lines_mean_what_the_golden_lines_mean() {
    let dir = tempfile::tempdir().expect("a directory");
    let golden_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden");

    for name in GOLDEN {
        let text =
            fs::read_to_string(golden_dir.join(format!("{name}.jsonl"))).expect("the golden file");
        let golden: Value = serde_json::from_str(text.trim()).expect("golden JSON");
        let now = timestamp_of(golden["timestamp"].as_str().expect("a timestamp"));
        let level = level_of(golden["level"].as_str().expect("a level"));
        let target = golden.get("target").and_then(Value::as_str);
        let kind = type_of(golden["type"].as_str().expect("a type"));
        let fields = golden["fields"].as_object().expect("fields");
        let message = fields["message"].as_str().expect("a message");

        let path = dir.path().join(format!("{name}.jsonl"));
        let mut sink = Sink::file(&path).expect("the line file");
        let mut line = line_at(now, &mut sink, level, message).kind(kind);
        if let Some(target) = target {
            line = line.target(target);
        }
        for (key, value) in fields {
            if key == "message" {
                continue;
            }
            line = match value {
                Value::String(value) => line.str(key, value),
                Value::Bool(value) => line.bool(key, *value),
                Value::Number(value) => match value.as_u64() {
                    Some(value) => line.u64(key, value),
                    None => line.i64(key, value.as_i64().expect("a number")),
                },
                Value::Object(_) | Value::Array(_) => line.json(key, value),
                Value::Null => panic!("{name}: {key} is null, which m6-log never writes"),
            };
        }
        line.end();
        sink.flush();

        let written = fs::read_to_string(&path).expect("the written line");
        let ours: Value = serde_json::from_str(written.trim()).expect("written JSON");
        assert_eq!(
            ours, golden,
            "{name}: m6-log's line does not mean what the golden line means"
        );
    }
}

fn type_of(text: &str) -> Type {
    match text {
        "event" => Type::Event,
        "perf" => Type::Perf,
        "access" => Type::Access,
        other => panic!("unknown golden type {other}"),
    }
}

fn level_of(text: &str) -> Level {
    match text {
        "ERROR" => Level::Error,
        "WARN" => Level::Warn,
        "INFO" => Level::Info,
        "DEBUG" => Level::Debug,
        "TRACE" => Level::Trace,
        other => panic!("unknown golden level {other}"),
    }
}

/// An independent inverse of the line timestamp, `YYYY-MM-DDTHH:MM:SS.ffffffZ`.
fn timestamp_of(text: &str) -> SystemTime {
    let year: i64 = text[0..4].parse().expect("a year");
    let month: u32 = text[5..7].parse().expect("a month");
    let day: u32 = text[8..10].parse().expect("a day");
    let hour: u32 = text[11..13].parse().expect("an hour");
    let minute: u32 = text[14..16].parse().expect("a minute");
    let second: u32 = text[17..19].parse().expect("a second");
    let micros: u32 = text[20..26].parse().expect("microseconds");
    assert_eq!(&text[19..20], ".");
    assert_eq!(&text[26..27], "Z");
    let seconds = days_from_civil(year, month, day) * 86_400
        + i64::from(hour) * 3_600
        + i64::from(minute) * 60
        + i64::from(second);
    UNIX_EPOCH + Duration::new(seconds as u64, micros * 1_000)
}

/// Howard Hinnant's `days_from_civil`.
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let yoe = year - era * 400;
    let month = i64::from(month);
    let doy = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + i64::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}
