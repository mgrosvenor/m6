//! The A/B performance verdict: one tested place that decides the gate.
//!
//! tools/perfcheck.sh measures and `m6-perf-ab` records. This module is where a
//! decision is made, in Rust with unit tests, so the gate's verdict is not a
//! script anyone edits by hand. The statistic, the verdict, the printed table
//! and the history record all live here.

use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::Write;

/// The 1-based order-statistic indices `(k, rounds - k + 1)` of a
/// distribution-free 99% confidence interval for a median. `k` is the largest
/// integer with `P(Binomial(rounds, 1/2) <= k - 1) <= 0.005`.
///
/// The binomial probabilities are computed in `f64` by the recurrence
/// `p(0) = 0.5^n`, `p(j + 1) = p(j) * (n - j) / (j + 1)`, with no factorials.
/// For 20 rounds this is `(4, 17)`, for 30 rounds `(8, 23)`.
pub fn ci_indices(rounds: usize) -> (usize, usize) {
    let n = rounds as f64;
    let mut p = 0.5f64.powi(rounds as i32);
    let mut cumulative = 0.0;
    let mut k = 1;
    for j in 0..rounds {
        cumulative += p;
        if cumulative > 0.005 {
            break;
        }
        k = j + 1;
        if j + 1 < rounds {
            p *= (n - j as f64) / (j as f64 + 1.0);
        }
    }
    (k, rounds - k + 1)
}

/// What the gate decided for one target.
#[derive(Debug, PartialEq, Eq)]
pub enum Verdict {
    Slower,
    Faster,
    NoChange,
    NotMeasured(&'static str),
}

impl std::fmt::Display for Verdict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Verdict::Slower => write!(f, "slower"),
            Verdict::Faster => write!(f, "faster"),
            Verdict::NoChange => write!(f, "no change"),
            Verdict::NotMeasured(why) => write!(f, "not measured ({why})"),
        }
    }
}

/// The verdict for one target, with the numbers that produced it.
#[derive(Debug)]
pub struct Summary {
    pub median_ratio: f64,
    pub ci_low: f64,
    pub ci_high: f64,
    pub slower_rounds: usize,
    pub verdict: Verdict,
}

/// The median of a slice of ratios, or `NaN` when the slice is empty.
fn median_ratio(ratios: &[f64]) -> f64 {
    if ratios.is_empty() {
        return f64::NAN;
    }
    let mut sorted = ratios.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = sorted.len();
    if n % 2 == 1 {
        sorted[n / 2]
    } else {
        (sorted[n / 2 - 1] + sorted[n / 2]) / 2.0
    }
}

/// Decide one target's verdict from its per-round paired ratios.
///
/// `ratios[i]` is `candidate_round_median / base_round_median` for round `i`.
/// The decision order is:
///
/// 1. fewer than 10 rounds: `not measured` (too few rounds for a 99% interval),
/// 2. `ci_high - ci_low` above `max_ci_width_pct / 100`: `not measured` (too
///    noisy to decide),
/// 3. `ci_low > 1` and `median_ratio >= 1 + threshold_pct / 100`: `slower`,
/// 4. `ci_high < 1` and `median_ratio <= 1 - threshold_pct / 100`: `faster`,
/// 5. otherwise `no change`.
///
/// Rule 3 is the sign test: `ci_low > 1` at 30 rounds means at least 23 rounds
/// were slower.
pub fn verdict(ratios: &[f64], threshold_pct: f64, max_ci_width_pct: f64) -> Summary {
    let median = median_ratio(ratios);
    let slower_rounds = ratios.iter().filter(|&&r| r > 1.0).count();
    if ratios.len() < 10 {
        return Summary {
            median_ratio: median,
            ci_low: f64::NAN,
            ci_high: f64::NAN,
            slower_rounds,
            verdict: Verdict::NotMeasured("too few rounds for a 99% interval"),
        };
    }

    let mut sorted = ratios.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let (k, upper) = ci_indices(ratios.len());
    let ci_low = sorted[k - 1];
    let ci_high = sorted[upper - 1];

    if ci_high - ci_low > max_ci_width_pct / 100.0 {
        return Summary {
            median_ratio: median,
            ci_low,
            ci_high,
            slower_rounds,
            verdict: Verdict::NotMeasured("too noisy to decide"),
        };
    }

    let v = if ci_low > 1.0 && median >= 1.0 + threshold_pct / 100.0 {
        Verdict::Slower
    } else if ci_high < 1.0 && median <= 1.0 - threshold_pct / 100.0 {
        Verdict::Faster
    } else {
        Verdict::NoChange
    };
    Summary {
        median_ratio: median,
        ci_low,
        ci_high,
        slower_rounds,
        verdict: v,
    }
}

/// One arm's per-round readings, gathered from the rounds file.
struct Arm {
    readings: Vec<u64>,
    p90: Vec<u64>,
    bytes: Option<u64>,
}

/// The median of a list of nanosecond readings, as integer nanoseconds.
fn median_ns(readings: &[u64]) -> u64 {
    if readings.is_empty() {
        return 0;
    }
    let mut sorted = readings.to_vec();
    sorted.sort_unstable();
    let n = sorted.len();
    if n % 2 == 1 {
        sorted[n / 2]
    } else {
        (sorted[n / 2 - 1] + sorted[n / 2]) / 2
    }
}

/// Round a ratio to four decimal places, for the history record.
fn round4(x: f64) -> f64 {
    (x * 10_000.0).round() / 10_000.0
}

/// A UTC timestamp in ISO 8601 shape: `YYYY-MM-DDTHH:MM:SSZ`.
fn utc_timestamp() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let days = secs.div_euclid(86_400);
    let secs_of_day = secs.rem_euclid(86_400);
    let (hh, mm, ss) = (
        secs_of_day / 3600,
        (secs_of_day % 3600) / 60,
        secs_of_day % 60,
    );

    // civil_from_days (Howard Hinnant's algorithm), for days since 1970-01-01.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let mut y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    y += if m <= 2 { 1 } else { 0 };

    format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}Z")
}

/// Everything the `m6-perf-ab` binary does, with the environment passed in so
/// the tests can set it without touching the process environment.
///
/// Arguments: `<rounds-file> <history-file> <threshold-pct> <max-ci-width-pct>
/// <rounds> <requests> <load-before> <load-after> <pinned 0|1> <client-cpu>
/// <server-cpu>`.
pub fn run(args: &[String], env: &BTreeMap<String, String>) -> i32 {
    if args.len() != 11 {
        eprintln!("{}", usage());
        return 2;
    }
    let rounds_file = &args[0];
    let history_file = &args[1];
    let threshold_pct: f64 = match args[2].parse() {
        Ok(v) => v,
        Err(_) => {
            eprintln!("{}", usage());
            return 2;
        }
    };
    let max_ci_width_pct: f64 = match args[3].parse() {
        Ok(v) => v,
        Err(_) => {
            eprintln!("{}", usage());
            return 2;
        }
    };
    let rounds: usize = match args[4].parse() {
        Ok(v) if v <= 500 => v,
        _ => {
            eprintln!("{}", usage());
            return 2;
        }
    };
    let requests: usize = match args[5].parse() {
        Ok(v) => v,
        Err(_) => {
            eprintln!("{}", usage());
            return 2;
        }
    };
    let load_before = parse_load(&args[6]);
    let load_after = parse_load(&args[7]);
    let pinned = args[8] == "1";
    let client_cpu = nonempty(&args[9]);
    let server_cpu = nonempty(&args[10]);

    // Parse the rounds file: one line per measurement,
    // `<target> <round> <arm> <median_ns> <p90_ns> <bytes>`.
    let mut targets: BTreeMap<String, BTreeMap<usize, [Option<u64>; 6]>> = BTreeMap::new();
    for line in std::fs::read_to_string(rounds_file)
        .unwrap_or_default()
        .lines()
    {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() != 6 {
            continue;
        }
        let (Ok(round), Ok(median), Ok(p90), Ok(bytes)) = (
            parts[1].parse::<usize>(),
            parts[3].parse::<u64>(),
            parts[4].parse::<u64>(),
            parts[5].parse::<u64>(),
        ) else {
            continue;
        };
        let entry = targets.entry(parts[0].to_string()).or_default();
        let slot = entry.entry(round).or_insert([None; 6]);
        // [base median, base p90, base bytes, cand median, cand p90, cand bytes]
        if parts[2] == "base" {
            slot[0] = Some(median);
            slot[1] = Some(p90);
            slot[2] = Some(bytes);
        } else if parts[2] == "cand" {
            slot[3] = Some(median);
            slot[4] = Some(p90);
            slot[5] = Some(bytes);
        }
    }

    let accept_slower = env
        .get("PERFCHECK_ACCEPT_SLOWER")
        .and_then(|s| nonempty(s))
        .map(|s| s.to_string());

    let mut any_slower = false;
    let mut any_not_measured = false;
    let mut targets_json = serde_json::Map::new();

    for (name, rounds_map) in &targets {
        let mut base = Arm {
            readings: Vec::new(),
            p90: Vec::new(),
            bytes: None,
        };
        let mut cand = Arm {
            readings: Vec::new(),
            p90: Vec::new(),
            bytes: None,
        };
        let mut ratios: Vec<f64> = Vec::new();
        let mut missing_pair = false;
        for slot in rounds_map.values() {
            match (slot[0], slot[3]) {
                (Some(bm), Some(cm)) => {
                    base.readings.push(bm);
                    cand.readings.push(cm);
                    ratios.push(cm as f64 / bm as f64);
                }
                _ => missing_pair = true,
            }
            if let Some(bp) = slot[1] {
                base.p90.push(bp);
            }
            if let Some(cp) = slot[4] {
                cand.p90.push(cp);
            }
            if let Some(bb) = slot[2] {
                base.bytes = Some(bb);
            }
            if let Some(cb) = slot[5] {
                cand.bytes = Some(cb);
            }
        }

        let base_median = median_ns(&base.readings);
        let cand_median = median_ns(&cand.readings);
        let med_ratio = median_ratio(&ratios);
        let slower_rounds = ratios.iter().filter(|&&r| r > 1.0).count();
        let mut summary = verdict(&ratios, threshold_pct, max_ci_width_pct);
        if !pinned {
            summary.verdict = Verdict::NotMeasured("the run was not pinned to separate CPUs");
        } else if missing_pair || ratios.is_empty() {
            summary.verdict = Verdict::NotMeasured("a round has no base or cand reading");
        }

        let ratio_pct = if med_ratio.is_nan() {
            String::from("n/a")
        } else {
            format!("{:+.1}%", (med_ratio - 1.0) * 100.0)
        };
        let ci_pct = if summary.ci_low.is_nan() || summary.ci_high.is_nan() {
            String::from("n/a")
        } else {
            format!(
                "[{:+.1}%, {:+.1}%]",
                (summary.ci_low - 1.0) * 100.0,
                (summary.ci_high - 1.0) * 100.0
            )
        };
        let table_verdict = match &summary.verdict {
            Verdict::NotMeasured(why) => format!("NOT MEASURED ({why})"),
            other => other.to_string(),
        };
        println!(
            "{name}: base {base_median}ns cand {cand_median}ns ratio {ratio_pct} ci99 {ci_pct} {slower_rounds}/{rounds} slower {table_verdict}"
        );

        if summary.verdict == Verdict::Slower {
            if let Some(reason) = &accept_slower {
                println!("ACCEPTED: {reason}");
            } else {
                any_slower = true;
            }
        } else if matches!(summary.verdict, Verdict::NotMeasured(_)) {
            any_not_measured = true;
        }

        targets_json.insert(
            name.clone(),
            serde_json::json!({
                "base": {
                    "readings": base.readings,
                    "p90": base.p90,
                    "bytes": base.bytes,
                },
                "cand": {
                    "readings": cand.readings,
                    "p90": cand.p90,
                    "bytes": cand.bytes,
                },
                "ratios": ratios.iter().map(|r| round4(*r)).collect::<Vec<f64>>(),
                "median_ratio": if med_ratio.is_nan() { serde_json::Value::Null } else { serde_json::json!(round4(med_ratio)) },
                "ci99": if summary.ci_low.is_nan() { serde_json::json!([serde_json::Value::Null, serde_json::Value::Null]) } else { serde_json::json!([round4(summary.ci_low), round4(summary.ci_high)]) },
                "slower_rounds": slower_rounds,
                "verdict": summary.verdict.to_string(),
            }),
        );
    }

    let code = if any_slower {
        1
    } else if any_not_measured {
        2
    } else {
        0
    };

    match code {
        0 => println!("PASS: no consistent slowdown"),
        1 => println!("FAIL: a target is consistently slower"),
        _ => println!("FAIL: a target was not measured"),
    }

    let record = build_record(
        env,
        load_before,
        load_after,
        pinned,
        client_cpu,
        server_cpu,
        rounds,
        requests,
        threshold_pct,
        max_ci_width_pct,
        accept_slower.as_deref(),
        &targets_json,
    );
    let line = serde_json::to_string(&record).unwrap_or_default();
    match OpenOptions::new()
        .create(true)
        .append(true)
        .open(history_file)
        .and_then(|mut f| writeln!(f, "{line}"))
    {
        Ok(()) => {}
        Err(e) => eprintln!("cannot write the history record: {e}"),
    }

    code
}

fn usage() -> &'static str {
    "usage: m6-perf-ab <rounds-file> <history-file> <threshold-pct> <max-ci-width-pct> <rounds> <requests> <load-before> <load-after> <pinned 0|1> <client-cpu> <server-cpu>"
}

/// A load average is optional: `uptime` can fail, and the record then holds
/// `null` rather than a number it did not take.
fn parse_load(s: &str) -> Option<f64> {
    s.parse::<f64>().ok()
}

fn nonempty(s: &str) -> Option<&str> {
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// A string from the environment, `null` when unset or empty.
fn env_str(env: &BTreeMap<String, String>, key: &str) -> Option<String> {
    env.get(key)
        .and_then(|s| nonempty(s))
        .map(|s| s.to_string())
}

/// A boolean from the environment: `null` when unset, `false` for "0", "" or
/// "false", `true` otherwise.
fn env_bool(env: &BTreeMap<String, String>, key: &str) -> Option<bool> {
    env.get(key)
        .map(|s| !matches!(s.as_str(), "" | "0" | "false"))
}

/// An unsigned number from the environment, `null` when unset or unparsable.
fn env_num(env: &BTreeMap<String, String>, key: &str) -> Option<u64> {
    env.get(key).and_then(|s| s.parse::<u64>().ok())
}

// The record's fields map one to one onto these parameters, so the signature
// reads the record itself. Grouping them into a struct would add a type that
// nothing else uses.
#[allow(clippy::too_many_arguments)]
fn build_record(
    env: &BTreeMap<String, String>,
    load_before: Option<f64>,
    load_after: Option<f64>,
    pinned: bool,
    client_cpu: Option<&str>,
    server_cpu: Option<&str>,
    rounds: usize,
    requests: usize,
    threshold_pct: f64,
    max_ci_width_pct: f64,
    accept_slower: Option<&str>,
    targets: &serde_json::Map<String, serde_json::Value>,
) -> serde_json::Value {
    let mut machine = serde_json::Map::new();
    machine.insert(
        "cpu".to_string(),
        serde_json::Value::from(env_str(env, "M6_PERF_CPU")),
    );
    machine.insert(
        "physical_cores".to_string(),
        serde_json::Value::from(env_num(env, "M6_PERF_CORES")),
    );
    machine.insert(
        "os".to_string(),
        serde_json::Value::from(env_str(env, "M6_PERF_OS")),
    );
    machine.insert(
        "arch".to_string(),
        serde_json::Value::from(env_str(env, "M6_PERF_ARCH")),
    );
    machine.insert(
        "memory_kb".to_string(),
        serde_json::Value::from(env_num(env, "M6_PERF_MEM_KB")),
    );

    let mut record = serde_json::Map::new();
    record.insert("ts".to_string(), serde_json::Value::from(utc_timestamp()));
    record.insert(
        "commit".to_string(),
        serde_json::Value::from(env_str(env, "M6_PERF_COMMIT")),
    );
    record.insert(
        "dirty".to_string(),
        serde_json::Value::from(env_bool(env, "M6_PERF_DIRTY")),
    );
    record.insert(
        "toolchain".to_string(),
        serde_json::Value::from(env_str(env, "M6_PERF_TOOLCHAIN")),
    );
    record.insert("profile".to_string(), serde_json::Value::from("release"));
    record.insert("machine".to_string(), serde_json::Value::Object(machine));
    record.insert(
        "load_before".to_string(),
        serde_json::Value::from(load_before),
    );
    record.insert(
        "load_after".to_string(),
        serde_json::Value::from(load_after),
    );
    record.insert("format".to_string(), serde_json::Value::from(2));
    record.insert(
        "base_commit".to_string(),
        serde_json::Value::from(env_str(env, "M6_PERF_BASE_COMMIT")),
    );
    record.insert("pinned".to_string(), serde_json::Value::from(pinned));
    record.insert(
        "cpus".to_string(),
        serde_json::json!({
            "client": client_cpu,
            "server": server_cpu,
        }),
    );
    record.insert("rounds".to_string(), serde_json::Value::from(rounds));
    record.insert("requests".to_string(), serde_json::Value::from(requests));
    record.insert(
        "threshold_pct".to_string(),
        serde_json::Value::from(threshold_pct),
    );
    record.insert(
        "max_ci_width_pct".to_string(),
        serde_json::Value::from(max_ci_width_pct),
    );
    record.insert(
        "accepted_slower".to_string(),
        serde_json::Value::from(accept_slower),
    );
    record.insert(
        "targets".to_string(),
        serde_json::Value::Object(targets.clone()),
    );
    serde_json::Value::Object(record)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn ci_indices_match_the_binomial_table() {
        assert_eq!(ci_indices(20), (4, 17));
        assert_eq!(ci_indices(30), (8, 23));
    }

    #[test]
    fn symmetric_noise_is_no_change() {
        let ratios: Vec<f64> = (0..30)
            .map(|i| if i % 2 == 0 { 0.97 } else { 1.03 })
            .collect();
        let summary = verdict(&ratios, 3.0, 10.0);
        assert_eq!(summary.verdict, Verdict::NoChange);
    }

    #[test]
    fn a_consistent_slowdown_is_slower() {
        let mut ratios = vec![1.05; 25];
        ratios.extend_from_slice(&[0.99; 5]);
        let summary = verdict(&ratios, 3.0, 10.0);
        assert_eq!(summary.verdict, Verdict::Slower);
        assert_eq!(summary.slower_rounds, 25);
    }

    #[test]
    fn a_consistent_but_small_change_is_no_change() {
        let ratios = vec![1.01; 30];
        let summary = verdict(&ratios, 3.0, 10.0);
        assert_eq!(summary.verdict, Verdict::NoChange);
    }

    #[test]
    fn a_consistent_speedup_is_faster() {
        let ratios = vec![0.94; 30];
        let summary = verdict(&ratios, 3.0, 10.0);
        assert_eq!(summary.verdict, Verdict::Faster);
    }

    #[test]
    fn a_wide_interval_is_not_measured() {
        let mut ratios = vec![1.20; 15];
        ratios.extend_from_slice(&[0.95; 15]);
        let summary = verdict(&ratios, 3.0, 10.0);
        assert!(matches!(summary.verdict, Verdict::NotMeasured(_)));
    }

    #[test]
    fn too_few_rounds_is_not_measured() {
        let ratios = vec![1.10; 9];
        let summary = verdict(&ratios, 3.0, 10.0);
        assert!(matches!(summary.verdict, Verdict::NotMeasured(_)));
    }

    #[test]
    fn the_record_and_the_exit_codes() {
        let dir = tempfile::tempdir().unwrap();
        let rounds_path = dir.path().join("rounds.txt");
        let history_path = dir.path().join("history.jsonl");

        // Two targets, 30 rounds each. render:minimal is 5% slower on every
        // round, render:blog-index is unchanged on every round.
        let mut rounds = String::new();
        for r in 1..=30 {
            for (target, base, cand) in [
                ("render:minimal", 100_000u64, 105_000u64),
                ("render:blog-index", 1_000_000u64, 1_000_000u64),
            ] {
                rounds.push_str(&format!(
                    "{target} {r} base {base} {} {}\n",
                    base + 1000,
                    9000
                ));
                rounds.push_str(&format!(
                    "{target} {r} cand {cand} {} {}\n",
                    cand + 1000,
                    9000
                ));
            }
        }
        std::fs::write(&rounds_path, rounds).unwrap();

        let make_args = |history: &std::path::Path, pinned: bool| -> Vec<String> {
            vec![
                rounds_path.to_string_lossy().to_string(),
                history.to_string_lossy().to_string(),
                "3".to_string(),
                "10".to_string(),
                "30".to_string(),
                "500".to_string(),
                "0.1".to_string(),
                "0.2".to_string(),
                if pinned {
                    "1".to_string()
                } else {
                    "0".to_string()
                },
                "0".to_string(),
                "3".to_string(),
            ]
        };

        // A slower target with no accept: exit 1, and one JSON record.
        let env = BTreeMap::new();
        assert_eq!(run(&make_args(&history_path, true), &env), 1);

        let content = std::fs::read_to_string(&history_path).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 1);
        let record: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(record["format"], 2);
        assert_eq!(record["pinned"], true);
        let minimal = &record["targets"]["render:minimal"];
        assert_eq!(minimal["verdict"], "slower");
        assert_eq!(minimal["slower_rounds"], 30);
        assert_eq!(minimal["base"]["readings"].as_array().unwrap().len(), 30);
        assert_eq!(minimal["cand"]["readings"].as_array().unwrap().len(), 30);
        assert_eq!(
            record["targets"]["render:blog-index"]["verdict"],
            "no change"
        );

        // A slower target with an accept reason: exit 0.
        let mut env_accept = BTreeMap::new();
        env_accept.insert(
            "PERFCHECK_ACCEPT_SLOWER".to_string(),
            "security".to_string(),
        );
        assert_eq!(run(&make_args(&history_path, true), &env_accept), 0);

        // Unpinned: every verdict is not measured, exit 2.
        assert_eq!(run(&make_args(&history_path, false), &env), 2);
    }
}
