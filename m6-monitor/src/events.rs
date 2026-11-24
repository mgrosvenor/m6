//! Counting one service's journal events, and faulting on the ones that are ours.
//!
//! # The problem this solves
//!
//! A service can refuse every request and look exactly like a service under
//! attack. That happens when the refusal is caused by the service's own
//! configuration -- a credential that does not verify, for instance -- because
//! the service then answers every caller the same way it answers a bad one. The
//! only trace is a log line, and a run of those reads as traffic from outside.
//!
//! **The digest is the one place an operator looks without being told to.** So
//! the distinction belongs here, on the page, rather than in a command somebody
//! has to remember to run.
//!
//! # What a series is
//!
//! A `[[monitor.event_series]]` table names one service by its journal
//! identifier, the message it writes when it ACCEPTED work, the message it
//! writes when it REFUSED work, and -- optionally -- the field on the refusal
//! line that names a cause, plus the causes that mean **the service's own
//! configuration is wrong**:
//!
//! ```toml
//! [[monitor.event_series]]
//! identifier  = "example-app"
//! accepted    = "job accepted"
//! rejected    = "job rejected"
//! split_by    = "credential_present"
//! fault_field = "error_codes"
//! fault_codes = ["invalid-credential", "missing-credential"]
//! ```
//!
//! The rules those fields buy:
//!
//! - a refusal whose `split_by` field is true and whose `fault_field` carries
//!   one of `fault_codes` is a **Fault at once**, with no window and no
//!   averaging, because a caller was turned away for a reason that is the
//!   operator's;
//! - a refusal carrying any other code is **not** a Fault, because the caller's
//!   credential was bad, stale or reused and the service behaved correctly;
//! - a refusal whose `split_by` field is false is **counted and never a
//!   Fault**, because that is traffic with no credential at all. A Fault that
//!   fires on that traffic gets ignored, and then it cannot fire on the night it
//!   matters.
//!
//! # De-duplication across overlapping reads
//!
//! The journal is read as the newest N entries and the next read overlaps the
//! last, so an entry arrives more than once. Each series keeps the newest
//! journal time it has counted, and an entry counts only when its
//! `__REALTIME_TIMESTAMP` is past that. **Two readers delivering the same entry,
//! or one reader delivering it in two consecutive reads, therefore counts it
//! once.** The order is the journal's own time for the entry, never the time
//! inside the message, which the service writes about itself.
//!
//! # What the counts are
//!
//! They are **since this monitor started reading**, which is one poll interval
//! after it started. A restart begins at zero, and the journal itself is the
//! record of the events. A series that has seen nothing says so rather than
//! showing a zero that cannot be told from a reader that is not working.

use serde::{Deserialize, Serialize};

/// One configured service, and what its two messages mean.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct SeriesConfig {
    /// The `SYSLOG_IDENTIFIER` of the service, which m6-log sets from the
    /// program's name. The journal field, never a string inside the message.
    pub identifier: String,
    /// The message written when the service ACCEPTED work.
    pub accepted: String,
    /// The message written when the service REFUSED work.
    pub rejected: String,
    /// The field on the refusal line that says whether the caller presented a
    /// credential at all, which is what the report splits the refusals by.
    /// Absent means the service does not say, so every refusal is judged by its
    /// code alone and the split stays at zero rather than being guessed.
    #[serde(default)]
    pub split_by: Option<String>,
    /// The field on the refusal line that names the cause, when the service
    /// names one.
    #[serde(default)]
    pub fault_field: Option<String>,
    /// Values of `fault_field` that mean the service's own configuration is
    /// wrong. A refusal carrying one is a Fault immediately.
    #[serde(default)]
    pub fault_codes: Vec<String>,
}

/// Refuse a series that could not do what it says.
///
/// Every problem is an error rather than a default, because a series that
/// silently matches nothing looks exactly like a service nobody calls. The
/// message names the table and the key, and `Fleet::from_config` refuses the
/// start with it.
pub fn validate(series: &[SeriesConfig]) -> Result<(), String> {
    for (i, s) in series.iter().enumerate() {
        if s.identifier.trim().is_empty() {
            return Err(format!("monitor.event_series[{i}] has no `identifier`"));
        }
        if s.accepted.trim().is_empty() {
            return Err(format!(
                "monitor.event_series[{i}] (`{}`) has no `accepted` message",
                s.identifier
            ));
        }
        if s.rejected.trim().is_empty() {
            return Err(format!(
                "monitor.event_series[{i}] (`{}`) has no `rejected` message",
                s.identifier
            ));
        }
        // A series that names one message for both counts every event as
        // accepted and refused at once, and reports a refusal rate it invented.
        if s.accepted == s.rejected {
            return Err(format!(
                "monitor.event_series[{i}] (`{}`) has the same message for `accepted` and \
                 `rejected`, so a line cannot say which it is",
                s.identifier
            ));
        }
        // One without the other is a rule that never fires, or one that fires
        // on every refusal. Both are silent in the page and wrong in the level.
        match (&s.fault_field, s.fault_codes.is_empty()) {
            (None, false) => {
                return Err(format!(
                    "monitor.event_series[{i}] (`{}`) lists `fault_codes` but no `fault_field`, \
                     so nothing is ever compared with them",
                    s.identifier
                ))
            }
            (Some(_), true) => {
                return Err(format!(
                    "monitor.event_series[{i}] (`{}`) names `fault_field` but no `fault_codes`, \
                     so no refusal could ever be a fault",
                    s.identifier
                ))
            }
            _ => {}
        }
        for code in &s.fault_codes {
            if code.trim().is_empty() {
                return Err(format!(
                    "monitor.event_series[{i}] (`{}`) has an empty entry in `fault_codes`",
                    s.identifier
                ));
            }
        }
        if series[..i].iter().any(|o| o.identifier == s.identifier) {
            return Err(format!(
                "monitor.event_series names `{}` more than once, so its lines would be counted \
                 twice",
                s.identifier
            ));
        }
    }
    Ok(())
}

/// A journal time and the line's own timestamp, for ordering and for display.
#[derive(Debug, Clone)]
struct Moment {
    at: u64,
    at_text: String,
    /// The code that made this one the service's fault, when it did.
    code: Option<String>,
}

/// One series' counters and the newest facts its lines carried.
#[derive(Debug, Clone)]
struct Series {
    config: SeriesConfig,
    accepted: u64,
    refused: u64,
    /// Refusals that carried a credential, which are the only ones a cause can
    /// belong to.
    refused_with_token: u64,
    /// Refusals that carried none. Traffic and the deployment's own checks.
    refused_without_token: u64,
    /// Refusals the service did not split by `split_by`, counted as neither
    /// of the two above and judged by their code alone.
    last_accepted: Option<Moment>,
    last_refused: Option<Moment>,
    /// The newest refusal whose code means the service's own configuration is
    /// wrong. A Fault stands while this is newer than `last_accepted`.
    last_fault: Option<Moment>,
    /// The newest journal time counted, so an overlapping read counts an entry
    /// once. `None` until the first entry.
    high_water: Option<u64>,
}

/// Every configured series, with what their lines have said.
#[derive(Debug, Clone, Default)]
pub struct Counters {
    series: Vec<Series>,
}

impl Counters {
    pub fn new(configs: Vec<SeriesConfig>) -> Counters {
        Counters {
            series: configs
                .into_iter()
                .map(|config| Series {
                    config,
                    accepted: 0,
                    refused: 0,
                    refused_with_token: 0,
                    refused_without_token: 0,
                    last_accepted: None,
                    last_refused: None,
                    last_fault: None,
                    high_water: None,
                })
                .collect(),
        }
    }

    /// The `SYSLOG_IDENTIFIER`s the journal read must select.
    pub fn identifiers(&self) -> Vec<String> {
        self.series
            .iter()
            .map(|s| s.config.identifier.clone())
            .collect()
    }

    pub fn is_empty(&self) -> bool {
        self.series.is_empty()
    }

    /// One journal entry: its decoded `journalctl --output=json` object and the
    /// line the service wrote inside `MESSAGE`.
    ///
    /// `at` is the journal's own time for the entry, in microseconds since the
    /// Unix epoch. It orders and de-duplicates here exactly as it does for the
    /// metrics documents, and for the same reason: every read overlaps the last
    /// one, and two readers overlap each other.
    pub fn ingest(&mut self, entry: &serde_json::Value, line: &serde_json::Value, at: u64) {
        // The identifier is the journal's field, never anything inside the
        // message a service writes about itself.
        let Some(identifier) = entry["SYSLOG_IDENTIFIER"].as_str() else {
            return;
        };
        let Some(series) = self
            .series
            .iter_mut()
            .find(|s| s.config.identifier == identifier)
        else {
            return;
        };
        // THE OVERLAP IS HERE. An entry at or below the newest time already
        // counted arrived before, from this reader's previous read or from the
        // other reader, and counts once.
        if series.high_water.is_some_and(|seen| at <= seen) {
            return;
        }
        let Some(message) = line["fields"]["message"].as_str() else {
            return;
        };
        let at_text = line["timestamp"].as_str().unwrap_or("-").to_string();
        if message == series.config.accepted {
            series.high_water = Some(at);
            series.accepted += 1;
            series.last_accepted = Some(Moment {
                at,
                at_text,
                code: None,
            });
            return;
        }
        if message != series.config.rejected {
            return;
        }
        series.high_water = Some(at);
        series.refused += 1;
        // No `split_by` configured means the service's refusals are judged
        // by their code alone, and the split is reported as zero of each
        // rather than guessed at.
        let token = series
            .config
            .split_by
            .as_ref()
            .map(|field| line["fields"][field].as_bool().unwrap_or(false));
        match token {
            Some(false) => {
                series.refused_without_token += 1;
                series.last_refused = Some(Moment {
                    at,
                    at_text,
                    code: None,
                });
                return;
            }
            Some(true) => series.refused_with_token += 1,
            None => {}
        }
        // What the service said about the cause, exactly as it sent it. This
        // is reported for every refusal, fault or not, so a reader can see a
        // run of caller-side refusals and know it is caller-side.
        let reported = series
            .config
            .fault_field
            .as_ref()
            .and_then(|field| line["fields"][field].as_str())
            .map(|value| value.to_string());
        // The field is a comma-joined list in the service's own line, so one
        // refusal can name more than one cause and only one of them needs to be
        // the operator's.
        let fault = reported.as_deref().and_then(|value| {
            value
                .split(',')
                .map(str::trim)
                .find(|code| series.config.fault_codes.iter().any(|c| c == code))
                .map(|code| code.to_string())
        });
        if let Some(code) = fault {
            series.last_fault = Some(Moment {
                at,
                at_text: at_text.clone(),
                code: Some(code),
            });
        }
        series.last_refused = Some(Moment {
            at,
            at_text,
            code: reported,
        });
    }

    /// Whether any series is standing on a fault, for a test.
    #[cfg(test)]
    fn faulting(&self) -> usize {
        self.digests(true)
            .iter()
            .filter(|d| d.fault_code.is_some())
            .count()
    }

    /// The digest's view: one entry per configured series, in config order.
    ///
    /// `reads` is whether the event-series journal read is running. A series
    /// whose reader is stopped is shown as `unread`, because its zeroes are not
    /// evidence about the service.
    pub fn digests(&self, reads: bool) -> Vec<EventSeries> {
        self.series.iter().map(|s| s.digest(reads)).collect()
    }
}

impl Series {
    fn digest(&self, reads: bool) -> EventSeries {
        // THE FAULT CLEARS ON THE FIRST ACCEPTANCE AFTER IT. A fault that
        // stands once it has ever been true teaches a reader to ignore the
        // page, and the acceptance is the evidence that the cause is gone.
        let cleared = match (&self.last_fault, &self.last_accepted) {
            (Some(f), Some(a)) => a.at > f.at,
            (Some(_), None) => false,
            (None, _) => true,
        };
        EventSeries {
            identifier: self.config.identifier.clone(),
            accepted: self.accepted,
            refused: self.refused,
            refused_with_token: self.refused_with_token,
            refused_without_token: self.refused_without_token,
            fault_code: self
                .last_fault
                .as_ref()
                .filter(|_| !cleared)
                .and_then(|f| f.code.clone()),
            fault_at: self
                .last_fault
                .as_ref()
                .filter(|_| !cleared)
                .map(|f| f.at_text.clone()),
            last_accepted_at: self.last_accepted.as_ref().map(|m| m.at_text.clone()),
            last_refused_at: self.last_refused.as_ref().map(|m| m.at_text.clone()),
            last_refused_code: self.last_refused.as_ref().and_then(|m| m.code.clone()),
            reads,
        }
    }
}

/// One series as the digest reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EventSeries {
    pub identifier: String,
    pub accepted: u64,
    pub refused: u64,
    /// Refusals that carried a credential, so a cause can be attributed.
    pub refused_with_token: u64,
    /// Refusals with no credential. Counted, and never a Fault.
    pub refused_without_token: u64,
    /// The code that means the service's own configuration is wrong, while the
    /// fault still stands.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fault_code: Option<String>,
    /// When that refusal happened.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fault_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_accepted_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_refused_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_refused_code: Option<String>,
    /// Whether the journal is being read at all. `false` is a series whose
    /// counts are not evidence of anything, and the page says so.
    pub reads: bool,
}

/// Whether a series is standing on a fault, and what to report.
///
/// A `Fault` finding names the service, the code and when it was refused, and
/// says the counts, so a reader can tell one refusal from a run of them without
/// opening the journal.
pub fn findings(series: &[EventSeries]) -> Vec<crate::digest::Finding> {
    series
        .iter()
        .filter_map(|s| {
            let code = s.fault_code.as_ref()?;
            Some(crate::digest::Finding {
                level: crate::digest::Level::Fault,
                node: "monitor".to_string(),
                text: format!(
                    "{} refused with `{}` at {}: every caller presenting a credential is being \
                     refused by this service's own configuration. {} accepted and {} refused \
                     since the monitor started",
                    s.identifier,
                    code,
                    s.fault_at.as_deref().unwrap_or("-"),
                    s.accepted,
                    s.refused
                ),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A made-up service. The engine is generic, so its tests are too.
    fn one() -> Vec<SeriesConfig> {
        vec![SeriesConfig {
            identifier: "example-app".to_string(),
            accepted: "job accepted".to_string(),
            rejected: "job rejected".to_string(),
            split_by: Some("credential_present".to_string()),
            fault_field: Some("error_codes".to_string()),
            fault_codes: vec![
                "invalid-credential".to_string(),
                "missing-credential".to_string(),
            ],
        }]
    }

    fn entry(identifier: &str) -> serde_json::Value {
        serde_json::json!({ "SYSLOG_IDENTIFIER": identifier })
    }

    fn accepted_line() -> serde_json::Value {
        serde_json::json!({
            "timestamp": "2026-11-23T01:00:00Z",
            "fields": { "message": "job accepted" }
        })
    }

    fn rejected_line(token: bool, codes: &str) -> serde_json::Value {
        serde_json::json!({
            "timestamp": "2026-11-23T01:00:01Z",
            "fields": {
                "message": "job rejected",
                "credential_present": token,
                "error_codes": codes
            }
        })
    }

    #[test]
    fn an_accepted_and_a_refused_line_are_counted() {
        let mut c = Counters::new(one());
        c.ingest(&entry("example-app"), &accepted_line(), 10);
        c.ingest(&entry("example-app"), &rejected_line(true, ""), 11);
        let d = &c.digests(true)[0];
        assert_eq!(d.accepted, 1);
        assert_eq!(d.refused, 1);
        assert_eq!(d.refused_with_token, 1);
        assert_eq!(d.refused_without_token, 0);
        assert_eq!(d.last_accepted_at.as_deref(), Some("2026-11-23T01:00:00Z"));
        assert_eq!(d.last_refused_at.as_deref(), Some("2026-11-23T01:00:01Z"));
    }

    /// **The rule the whole module exists for.** A refusal with a credential
    /// whose code names the service's own configuration is a Fault at once. A
    /// refusal with a credential and a caller-side code is not.
    #[test]
    fn only_the_codes_that_are_the_services_own_are_a_fault() {
        let mut c = Counters::new(one());
        c.ingest(
            &entry("example-app"),
            &rejected_line(true, "invalid-credential"),
            10,
        );
        let d = &c.digests(true)[0];
        assert_eq!(d.fault_code.as_deref(), Some("invalid-credential"));
        assert_eq!(d.fault_at.as_deref(), Some("2026-11-23T01:00:01Z"));
        assert_eq!(c.faulting(), 1);

        let mut c = Counters::new(one());
        c.ingest(
            &entry("example-app"),
            &rejected_line(true, "invalid-token"),
            10,
        );
        let d = &c.digests(true)[0];
        assert_eq!(d.fault_code, None, "a caller's bad token is not our fault");
        assert_eq!(
            d.last_refused_code.as_deref(),
            Some("invalid-token"),
            "the code is still reported, just not as a Fault"
        );
        assert_eq!(c.faulting(), 0);
    }

    /// A refusal with no credential can never be a Fault, however many arrive.
    #[test]
    fn a_refusal_without_a_credential_is_a_count_and_never_a_fault() {
        let mut c = Counters::new(one());
        for at in 0..31 {
            c.ingest(&entry("example-app"), &rejected_line(false, ""), at);
        }
        let d = &c.digests(true)[0];
        assert_eq!(d.refused, 31);
        assert_eq!(d.refused_without_token, 31);
        assert_eq!(d.fault_code, None);
        assert_eq!(c.faulting(), 0);
    }

    /// **The clearing, tested and not only the firing.** An acceptance after
    /// the fault is the evidence that the cause is gone.
    #[test]
    fn an_acceptance_after_the_fault_clears_it() {
        let mut c = Counters::new(one());
        c.ingest(
            &entry("example-app"),
            &rejected_line(true, "invalid-credential"),
            10,
        );
        assert!(c.digests(true)[0].fault_code.is_some());
        c.ingest(&entry("example-app"), &accepted_line(), 11);
        let d = &c.digests(true)[0];
        assert_eq!(d.fault_code, None, "the fix is the evidence");
        assert_eq!(d.accepted, 1);
        assert_eq!(c.faulting(), 0);
    }

    /// An acceptance BEFORE the fault does not clear it: only an acceptance
    /// after the refusal is evidence about it.
    #[test]
    fn an_acceptance_before_the_fault_does_not_clear_it() {
        let mut c = Counters::new(one());
        c.ingest(&entry("example-app"), &accepted_line(), 9);
        c.ingest(
            &entry("example-app"),
            &rejected_line(true, "invalid-credential"),
            10,
        );
        assert_eq!(
            c.digests(true)[0].fault_code.as_deref(),
            Some("invalid-credential")
        );
    }

    /// **The overlap.** Two readers, or one reader twice, deliver the same
    /// entry. It counts once, and the newest time counted moves forward.
    #[test]
    fn an_entry_delivered_twice_counts_once() {
        let mut c = Counters::new(one());
        c.ingest(&entry("example-app"), &accepted_line(), 10);
        c.ingest(&entry("example-app"), &accepted_line(), 10);
        assert_eq!(c.digests(true)[0].accepted, 1);
        // A later entry is new, and an older one delivered after it is not.
        c.ingest(&entry("example-app"), &rejected_line(false, ""), 12);
        c.ingest(&entry("example-app"), &accepted_line(), 11);
        let d = &c.digests(true)[0];
        assert_eq!(d.accepted, 1);
        assert_eq!(d.refused, 1);
        // The older one did not move the high-water mark back, so a repeat of
        // the newest is still refused.
        c.ingest(&entry("example-app"), &rejected_line(false, ""), 12);
        assert_eq!(c.digests(true)[0].refused, 1);
    }

    /// Another service's line is not counted, even when it says the same words.
    #[test]
    fn another_services_line_is_ignored() {
        let mut c = Counters::new(one());
        c.ingest(&entry("other-app"), &accepted_line(), 10);
        assert_eq!(c.digests(true)[0].accepted, 0);
    }

    /// Several codes on one line: only one has to be the service's own.
    #[test]
    fn a_comma_joined_field_finds_the_code_that_is_ours() {
        let mut c = Counters::new(one());
        c.ingest(
            &entry("example-app"),
            &rejected_line(true, "invalid-token,invalid-credential"),
            10,
        );
        assert_eq!(
            c.digests(true)[0].fault_code.as_deref(),
            Some("invalid-credential")
        );
    }

    /// A refusal with no `split_by` configured is judged by its code, and
    /// the split stays at zero rather than being guessed.
    #[test]
    fn a_series_without_a_split_by_judges_the_code_alone() {
        let mut configs = one();
        configs[0].split_by = None;
        let mut c = Counters::new(configs);
        c.ingest(
            &entry("example-app"),
            &rejected_line(false, "invalid-credential"),
            10,
        );
        let d = &c.digests(true)[0];
        assert_eq!(d.refused, 1);
        assert_eq!(d.refused_with_token, 0);
        assert_eq!(d.refused_without_token, 0);
        assert_eq!(d.fault_code.as_deref(), Some("invalid-credential"));
    }

    #[test]
    fn the_config_refuses_the_mistakes_that_would_be_silent() {
        let good = one();
        validate(&good).expect("a complete series");
        validate(&[]).expect("no series is not an error");

        for (why, mutate) in [
            (
                "no identifier",
                (|s: &mut SeriesConfig| s.identifier.clear()) as fn(&mut SeriesConfig),
            ),
            ("no accepted", |s: &mut SeriesConfig| s.accepted.clear()),
            ("no rejected", |s: &mut SeriesConfig| s.rejected.clear()),
            ("both the same", |s: &mut SeriesConfig| {
                s.rejected = s.accepted.clone()
            }),
            ("codes with no field", |s: &mut SeriesConfig| {
                s.fault_field = None
            }),
            ("a field with no codes", |s: &mut SeriesConfig| {
                s.fault_codes.clear()
            }),
            ("an empty code", |s: &mut SeriesConfig| {
                s.fault_codes = vec![String::new()]
            }),
        ] {
            let mut series = good.clone();
            mutate(&mut series[0]);
            let err = validate(&series).expect_err(why);
            assert!(
                err.contains("event_series[0]"),
                "the refusal must name the table: {err}"
            );
        }

        let mut twice = good.clone();
        twice.push(good[0].clone());
        let err = validate(&twice).expect_err("the same identifier twice");
        assert!(err.contains("more than once"), "{err}");
    }

    /// The finding names the service, the code, the time and both counts, so a
    /// reader can tell one refusal from a run without opening the journal.
    #[test]
    fn the_finding_names_the_code_the_time_and_the_counts() {
        let mut c = Counters::new(one());
        c.ingest(
            &entry("example-app"),
            &rejected_line(true, "invalid-credential"),
            10,
        );
        let f = findings(&c.digests(true));
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].level, crate::digest::Level::Fault);
        assert!(f[0].text.contains("example-app"), "{}", f[0].text);
        assert!(f[0].text.contains("invalid-credential"), "{}", f[0].text);
        assert!(f[0].text.contains("0 accepted"), "{}", f[0].text);
        // A fault-free series produces nothing at all.
        assert!(findings(&[]).is_empty());
    }
}
