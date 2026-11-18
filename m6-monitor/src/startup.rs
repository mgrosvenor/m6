//! The monitor's checks on the start-up record.
//!
//! The journal carries one `type=startup` record per run, several lines sharing
//! a `run_id`, and one `type=shutdown` line on a clean exit. `journal.rs` groups
//! them. This module judges them, and it is the only place that holds an
//! opinion about what a record means.
//!
//! # What is checked, and against what
//!
//! Two kinds of question, and the answer comes from a different reference for
//! each. The split is the one `docs/m6-log.md` section 9 states:
//!
//! - the machine's facts (`host`) are compared PEER TO PEER. A node whose
//!   kernel, OS, CPU or memory differs from its peers is a warning.
//! - what the process chose (`decided`) is compared AGAINST THE DEPLOY'S
//!   RECORD. A different socket, bind or process index is a fault.
//!
//! The deploy's record is a JSON file the deploy writes and the monitor reads.
//! Its path is `[monitor] expected_deploy_file`, so no deployment path lives in
//! this crate (RULE ZERO). The shape is documented in `docs/m6-monitor.md`
//! section 5.1. A file that does not exist yet is not a fault: the check says
//! it is waiting for the deploy side.
//!
//! # Everything names a node, a run and the fact
//!
//! A finding is one line an operator acts on, so every one of them names the
//! node, the run id and the value. A finding that says "a config changed" and
//! not which run on which node is noise, and noise is the same as nothing.
//!
//! # Severities
//!
//! `Fault` for a version the rules refuse, a version or config sha that differs
//! from the deploy's record, a decided fact that differs from it, a record that
//! never closed, and a completed record whose own count disagrees. `Warn` for a
//! restart nobody expected, a host fact that differs from the peers, and a
//! deploy record the monitor cannot compare against.

use std::collections::BTreeMap;
use std::path::Path;

use semver::Version;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::digest::{Digest, Finding, Level};
use crate::journal::StartupView;

/// How many peers one drift finding lists.
const PEERS_LISTED: usize = 6;

/// How many host facts the peer comparison reads.
const HOST_FACT_COUNT: usize = 5;

/// The deploy's record of what it expects on each node.
///
/// The deploy writes this file. The monitor only reads it, and every value in
/// it is the deployment's own answer: a version string, a sha256 per config
/// file role, and the decided facts it meant to ship. `docs/m6-monitor.md`
/// section 5.1 gives the shape and an example.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Expectations {
    /// The environment this record was written for: `dev`, `staging` or
    /// `prod`. Used only when `[monitor] environment` is not set.
    #[serde(default)]
    pub environment: Option<String>,
    /// Whether a deploy is in progress right now. A restart is expected only
    /// when this is true and the previous run wrote a shutdown line.
    #[serde(default)]
    pub deploy_in_progress: bool,
    /// The expected values, keyed by the node the start-up record names.
    #[serde(default)]
    pub nodes: BTreeMap<String, NodeExpectation>,
}

/// What the deploy expects of one node.
///
/// The key is `apps`, so one node can run several m6 processes and each is
/// checked on its own.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NodeExpectation {
    #[serde(default)]
    pub apps: BTreeMap<String, AppExpectation>,
}

/// What the deploy expects of one app on one node.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AppExpectation {
    /// The release the deploy shipped, as the full SemVer string with its
    /// build metadata. A tag form has no commit, so it cannot say which build.
    pub version: String,
    /// Config file role to the sha256 of the rendered file.
    #[serde(default)]
    pub config: BTreeMap<String, String>,
    /// Decided fact to the value the deploy recorded. `socket_path`, `bind`,
    /// `redirect_bind`, `h2c_bind`, `process_index` and `lock_memory` are the
    /// facts the platform line's `decided` block carries today.
    #[serde(default)]
    pub decided: BTreeMap<String, Value>,
}

/// The state of the deploy's record, as the check reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExpectedState {
    /// `[monitor] expected_deploy_file` is not set.
    NotConfigured,
    /// The key is set and the file is not there. Waiting for the deploy side.
    Missing,
    /// The file is there and could not be read or decoded.
    Unreadable(String),
    /// The file is there and every expected value is loaded.
    Loaded,
}

impl ExpectedState {
    /// The word the check and the digest print for this state.
    pub fn as_str(&self) -> &'static str {
        match self {
            ExpectedState::NotConfigured => "not_configured",
            ExpectedState::Missing => "missing",
            ExpectedState::Unreadable(_) => "unreadable",
            ExpectedState::Loaded => "loaded",
        }
    }

    /// Why the file could not be used, when that is the state.
    pub fn error(&self) -> Option<&str> {
        match self {
            ExpectedState::Unreadable(e) => Some(e),
            _ => None,
        }
    }
}

/// Read the deploy's record, and say what state it is in.
///
/// A missing file is a state, never an error. It is the deploy side's to write
/// and the monitor starts before it exists on a fresh host.
pub fn load(path: Option<&Path>) -> (Expectations, ExpectedState) {
    let Some(path) = path else {
        return (Expectations::default(), ExpectedState::NotConfigured);
    };
    match std::fs::read_to_string(path) {
        Ok(text) => match serde_json::from_str::<Expectations>(&text) {
            Ok(expected) => (expected, ExpectedState::Loaded),
            Err(e) => (
                Expectations::default(),
                ExpectedState::Unreadable(format!("{e}")),
            ),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            (Expectations::default(), ExpectedState::Missing)
        }
        Err(e) => (
            Expectations::default(),
            ExpectedState::Unreadable(format!("{e}")),
        ),
    }
}

/// What the journal and the deploy recorded, as the checks read it.
#[derive(Debug, Clone)]
pub struct Facts {
    /// The records the journal carried, newest of each node and app, and the
    /// ones that never closed.
    pub runs: Vec<StartupView>,
    pub expectations: Expectations,
    pub expected_state: ExpectedState,
    /// The path the file was read from, for the report.
    pub expected_path: Option<String>,
    /// The environment `[monitor] environment` declares.
    pub environment: Option<String>,
    /// Records the journal tail cut, counted and not judged.
    pub cut: u64,
    /// Shutdown lines whose start-up lines were never seen.
    pub orphan_shutdowns: u64,
    /// Why the journal reader is not running, when it is not.
    pub journal_unavailable: Option<String>,
}

/// One node's and app's current run, as `/digest` carries it.
#[derive(Debug, Clone, Serialize)]
pub struct RunDigest {
    pub node: String,
    pub app: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub process_index: Option<u64>,
    pub run_id: String,
    /// The full SemVer string, build metadata included.
    pub version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
    /// The identity line's timestamp: when the run started, as the record
    /// states it.
    pub started_at: String,
    /// Config file role to the sha256 the record read.
    pub config: BTreeMap<String, String>,
    /// Config file role to the path the record read.
    pub config_paths: BTreeMap<String, String>,
    /// Start-up lines received.
    pub lines: u64,
    /// Every journal line the monitor ingested for this run's id: the
    /// start-up lines, any replay of them, and the shutdown line.
    pub ingested_lines: u64,
    /// The count the `complete` line declared.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub complete_lines: Option<u64>,
    /// `complete` when the count agrees and the record closed, `incomplete`
    /// otherwise.
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shutdown: Option<ShutdownDigest>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_run_id: Option<String>,
    /// How the previous run ended, when there is one: its shutdown line, or
    /// `null` when it wrote none. Absent when this is the first run seen.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_shutdown: Option<Option<ShutdownDigest>>,
    /// `info` for a restart the monitor expected, `warn` for one it did not.
    /// Absent when there is no previous run, or the run is from the journal
    /// tail and is not judged.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub restart: Option<&'static str>,
}

/// One clean exit, as `/digest` carries it.
#[derive(Debug, Clone, Serialize)]
pub struct ShutdownDigest {
    pub reason: String,
    pub uptime_ms: u64,
    pub at: String,
}

/// The start-up section `/digest` carries and `/check` prints.
#[derive(Debug, Clone, Serialize)]
pub struct StartupDigest {
    /// The environment the checks used, resolved from the monitor's config
    /// and then the deploy's record.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub environment: Option<String>,
    /// The deploy's record the monitor read, when one is configured.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected_file: Option<String>,
    /// `loaded`, `missing`, `not_configured` or `unreadable`.
    pub expected_state: String,
    /// Why the file could not be used, when that is the state.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected_error: Option<String>,
    /// What the deploy's record says about a deploy right now, when it was
    /// read.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deploy_in_progress: Option<bool>,
    /// Records the journal tail cut, so a reader knows the list is partial.
    pub cut_records: u64,
    /// Shutdown lines whose start-up lines were never seen.
    pub orphan_shutdowns: u64,
    /// Why the journal reader is not running, when it is not. A reader that
    /// has stopped explains an empty list, and saying so is the difference
    /// between "no record" and "cannot look".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub journal_unavailable: Option<String>,
    pub runs: Vec<RunDigest>,
}

impl Default for StartupDigest {
    fn default() -> StartupDigest {
        StartupDigest {
            environment: None,
            expected_file: None,
            expected_state: "not_configured".to_string(),
            expected_error: None,
            deploy_in_progress: None,
            cut_records: 0,
            orphan_shutdowns: 0,
            journal_unavailable: None,
            runs: Vec::new(),
        }
    }
}

/// Judge every record, put the section into the digest, and add the findings.
///
/// The section is filled even when every record passes, because a reader of
/// `/digest` asks what is running on each node and "nothing" must be said
/// rather than left out.
pub fn judge(d: &mut Digest, facts: &Facts) {
    let environment = facts
        .environment
        .clone()
        .or_else(|| facts.expectations.environment.clone());
    let mut digest = StartupDigest {
        environment: environment.clone(),
        expected_file: facts.expected_path.clone(),
        expected_state: facts.expected_state.as_str().to_string(),
        expected_error: facts.expected_state.error().map(str::to_string),
        deploy_in_progress: matches!(facts.expected_state, ExpectedState::Loaded)
            .then_some(facts.expectations.deploy_in_progress),
        cut_records: facts.cut,
        orphan_shutdowns: facts.orphan_shutdowns,
        journal_unavailable: facts.journal_unavailable.clone(),
        runs: Vec::new(),
    };

    let mut findings = expectation_findings(facts, environment.as_deref());
    for run in &facts.runs {
        digest.runs.push(run_digest(run, facts));
        findings.extend(run_findings(run, facts, environment.as_deref()));
    }
    findings.extend(peer_findings(&facts.runs));

    d.startup = digest;
    for finding in findings {
        d.add_finding(finding);
    }
}

/// A finding about the deploy's record or the environment, not about one run.
fn monitor_finding(level: Level, text: String) -> Finding {
    Finding {
        level,
        node: "monitor".to_string(),
        text,
    }
}

/// The findings that do not depend on one run.
fn expectation_findings(facts: &Facts, environment: Option<&str>) -> Vec<Finding> {
    let mut out = Vec::new();
    match &facts.expected_state {
        ExpectedState::NotConfigured => out.push(monitor_finding(
            Level::Warn,
            "no [monitor] expected_deploy_file is configured, so no run's version, config sha256 or decided facts are checked against the deploy's record: set it to the file the deploy writes".to_string(),
        )),
        ExpectedState::Missing => out.push(monitor_finding(
            Level::Warn,
            format!(
                "the deploy's expectation file {} does not exist yet, so no run's version, config sha256 or decided facts are checked: waiting for the deploy side",
                facts.expected_path.as_deref().unwrap_or("-")
            ),
        )),
        ExpectedState::Unreadable(e) => out.push(monitor_finding(
            Level::Fault,
            format!(
                "the deploy's expectation file {} could not be read: {e}",
                facts.expected_path.as_deref().unwrap_or("-")
            ),
        )),
        ExpectedState::Loaded => {}
    }
    if let (Some(configured), Some(recorded)) = (
        facts.environment.as_deref(),
        facts.expectations.environment.as_deref(),
    ) {
        if !configured.eq_ignore_ascii_case(recorded) {
            out.push(monitor_finding(
                Level::Warn,
                format!(
                    "this monitor is configured for environment {configured:?}, but the deploy's expectation file was written for {recorded:?}: the version checks are reading the wrong environment's record"
                ),
            ));
        }
    }
    if environment.is_none() {
        out.push(monitor_finding(
            Level::Warn,
            "no environment is set, by [monitor] environment or by the deploy's record, so a dirty build and a pre-release cannot be judged".to_string(),
        ));
    }
    out
}

fn shutdown_digest(shutdown: &crate::journal::Shutdown) -> ShutdownDigest {
    ShutdownDigest {
        reason: shutdown.reason.clone(),
        uptime_ms: shutdown.uptime_ms,
        at: shutdown.at.clone(),
    }
}

fn run_digest(run: &StartupView, facts: &Facts) -> RunDigest {
    RunDigest {
        node: run.node.clone(),
        app: run.app.clone(),
        process_index: run.process_index,
        run_id: run.run_id.clone(),
        version: run.version.clone(),
        tag: run.tag.clone(),
        started_at: run.started_at.clone(),
        config: run.config.clone(),
        config_paths: run.config_paths.clone(),
        lines: run.lines,
        ingested_lines: run.ingested,
        complete_lines: run.complete,
        state: if run.complete == Some(run.lines) {
            "complete".to_string()
        } else {
            "incomplete".to_string()
        },
        shutdown: run.shutdown.as_ref().map(shutdown_digest),
        previous_run_id: run.previous.as_ref().map(|p| p.run_id.clone()),
        previous_shutdown: run
            .previous
            .as_ref()
            .map(|p| p.shutdown.as_ref().map(shutdown_digest)),
        restart: restart_expected(run, facts)
            .map(|expected| if expected { "info" } else { "warn" }),
    }
}

/// Every finding one record produces.
fn run_findings(run: &StartupView, facts: &Facts, environment: Option<&str>) -> Vec<Finding> {
    let mut out = Vec::new();
    let who = format!("run {} ({})", run.run_id, run.app);
    {
        let mut push = |level: Level, fact: String| {
            out.push(Finding {
                level,
                node: run.node.clone(),
                text: format!("{who}: {fact}"),
            });
        };
        if run.hostname.is_none() && run.node == run.app {
            push(
                Level::Warn,
                format!(
                    "the record names no node and its platform line reports no hostname, so this run is attributed to app {:?} alone and a second host running the same app would be merged with it",
                    run.app
                ),
            );
        }
        let version = check_version(run, environment, &mut push);
        if matches!(facts.expected_state, ExpectedState::Loaded) {
            match expectation_for(&facts.expectations, run) {
                Lookup::NodeMissing => push(
                    Level::Warn,
                    format!(
                        "the deploy recorded no expectation for node {:?}, so this run's version and config cannot be checked",
                        run.node
                    ),
                ),
                Lookup::AppMissing => push(
                    Level::Warn,
                    format!(
                        "the deploy recorded no expectation for app {:?} on node {:?}, so this run's version and config cannot be checked",
                        run.app, run.node
                    ),
                ),
                Lookup::Found(expected) => {
                    check_expected(run, expected, version.as_ref(), &mut push);
                }
            }
        }
        check_completeness(run, &mut push);
        check_restart(run, facts, &mut push);
    }
    out
}

/// Parse the record's version and apply the two rules that depend on the
/// environment. Returns the parsed version for the comparison that follows.
fn check_version(
    run: &StartupView,
    environment: Option<&str>,
    push: &mut impl FnMut(Level, String),
) -> Option<Version> {
    let version = match Version::parse(&run.version) {
        Ok(version) => Some(version),
        Err(e) => {
            push(
                Level::Fault,
                format!(
                    "the record's version {:?} is not valid SemVer, so no rule about it can be applied: {e}",
                    run.version
                ),
            );
            None
        }
    };
    let version = version?;
    let is_dev = environment.is_some_and(|e| e.eq_ignore_ascii_case("dev"));
    let is_prod = environment.is_some_and(|e| e.eq_ignore_ascii_case("prod"));
    if !is_dev
        && version
            .build
            .as_str()
            .split('.')
            .any(|part| part.eq_ignore_ascii_case("dirty"))
    {
        push(
            Level::Fault,
            format!(
                "the build is dirty ({}), and a dirty build is refused anywhere but dev",
                run.version
            ),
        );
    }
    if is_prod && !version.pre.is_empty() {
        push(
            Level::Fault,
            format!("a pre-release ({}) is refused on prod", run.version),
        );
    }
    Some(version)
}

/// The comparison against the deploy's expected values for this app on this
/// node.
fn check_expected(
    run: &StartupView,
    expected: &AppExpectation,
    version: Option<&Version>,
    push: &mut impl FnMut(Level, String),
) {
    match Version::parse(&expected.version) {
        Ok(want) => {
            if let Some(got) = version {
                if got != &want {
                    push(
                        Level::Fault,
                        format!(
                            "version {} differs from the release the deploy recorded for node {:?}, {}",
                            run.version, run.node, expected.version
                        ),
                    );
                }
            }
        }
        Err(e) => push(
            Level::Fault,
            format!(
                "the deploy's recorded version {:?} is not valid SemVer, so the version cannot be compared: {e}",
                expected.version
            ),
        ),
    }
    for (role, want) in &expected.config {
        match run.config.get(role) {
            Some(got) if got.eq_ignore_ascii_case(want) => {}
            Some(got) => push(
                Level::Fault,
                format!(
                    "config file role {role:?} has sha256 {got}, but the deploy rendered {want}: the file on the node is hand-edited or stale"
                ),
            ),
            None => push(
                Level::Fault,
                format!(
                    "the deploy rendered sha256 {want} for config file role {role:?}, but the record has no config_file line for it"
                ),
            ),
        }
    }
    for role in run.config.keys() {
        if !expected.config.contains_key(role) {
            push(
                Level::Warn,
                format!(
                    "the record read config file role {role:?}, which the deploy recorded no sha256 for"
                ),
            );
        }
    }
    if let Some(decided) = &run.decided {
        for (key, want) in &expected.decided {
            match decided.get(key) {
                Some(got) if got == want => {}
                Some(got) => push(
                    Level::Fault,
                    format!(
                        "decided {key} is {got}, but the deploy recorded {want}"
                    ),
                ),
                None => push(
                    Level::Fault,
                    format!(
                        "the deploy recorded decided {key} = {want}, but the record carries no decided {key}"
                    ),
                ),
            }
        }
    }
}

/// The count against the lines received, and a record that never closed.
fn check_completeness(run: &StartupView, push: &mut impl FnMut(Level, String)) {
    match run.complete {
        Some(declared) if declared != run.lines => push(
            Level::Fault,
            format!(
                "the complete line counts {declared} start-up lines, but {} were received, so the record is not the one the line describes",
                run.lines
            ),
        ),
        Some(_) => {}
        None if !run.current => push(
            Level::Fault,
            format!(
                "the record never wrote its complete line ({} start-up line(s) received), so it is incomplete",
                run.lines
            ),
        ),
        // A current record without a complete line is not a possible state:
        // `journal::Startup::finish_record` is reached only by that line.
        None => {}
    }
}

/// Whether this run's restart was expected. `None` when there is no previous
/// run, or the run is from the journal tail and is not judged.
fn restart_expected(run: &StartupView, facts: &Facts) -> Option<bool> {
    let previous = run.previous.as_ref()?;
    if !run.judged {
        return None;
    }
    let deployed = matches!(facts.expected_state, ExpectedState::Loaded);
    Some(if deployed {
        facts.expectations.deploy_in_progress && previous.shutdown.is_some()
    } else {
        // Without the deploy's record a deploy in progress cannot be claimed
        // or denied, so a graceful restart is not called unexpected. The
        // missing record already has its own finding.
        previous.shutdown.is_some()
    })
}

/// The finding for a restart nobody expected. An expected one is `info` in
/// the run's digest and in `/check`'s previous line, and is not a finding.
fn check_restart(run: &StartupView, facts: &Facts, push: &mut impl FnMut(Level, String)) {
    let Some(previous) = &run.previous else {
        return;
    };
    if restart_expected(run, facts) != Some(false) {
        return;
    }
    let why = match &previous.shutdown {
        Some(shutdown) => format!(
            "the previous run {} exited with reason {:?} at {} after {}ms, and no deploy is in progress",
            previous.run_id, shutdown.reason, shutdown.at, shutdown.uptime_ms
        ),
        None => format!(
            "the previous run {} wrote no shutdown line, so it did not exit cleanly",
            previous.run_id
        ),
    };
    push(Level::Warn, format!("unexpected restart: {why}"));
}

/// What the deploy's record says about one run.
enum Lookup<'a> {
    NodeMissing,
    AppMissing,
    Found(&'a AppExpectation),
}

fn expectation_for<'a>(expectations: &'a Expectations, run: &StartupView) -> Lookup<'a> {
    let Some(node) = expectations.nodes.get(&run.node) else {
        return Lookup::NodeMissing;
    };
    // A node polled as several processes shares one deployment, so the plain
    // app name is the first key. `<app>/<index>` lets a deploy record process
    // facts that differ, and wins when it exists.
    if let Some(index) = run.process_index {
        if let Some(expected) = node.apps.get(&format!("{}/{}", run.app, index)) {
            return Lookup::Found(expected);
        }
    }
    match node.apps.get(&run.app) {
        Some(expected) => Lookup::Found(expected),
        None => Lookup::AppMissing,
    }
}

/// One node's host facts, for the peer comparison.
struct Peer<'a> {
    node: &'a str,
    app: &'a str,
    run_id: &'a str,
    host: &'a Value,
}

/// The host facts compared peer to peer, in the order the finding names them.
///
/// The list is the worker's split from `docs/m6-log.md`: the machine's kernel,
/// OS, CPU and memory. A limit that differs between two machines with one
/// kernel is a configuration difference, not drift, and is left out.
fn host_facts(host: &Value) -> [(&'static str, Option<String>); HOST_FACT_COUNT] {
    let os_name = host.pointer("/os/known/name").and_then(Value::as_str);
    let os_version = host
        .pointer("/os/known/version/known")
        .and_then(|v| match v {
            Value::String(s) => Some(s.clone()),
            Value::Number(n) => Some(n.to_string()),
            _ => None,
        });
    [
        (
            "kernel release",
            host.pointer("/kernel/known/release")
                .and_then(Value::as_str)
                .map(str::to_string),
        ),
        (
            "OS",
            match (os_name, os_version) {
                (Some(name), Some(version)) => Some(format!("{name} {version}")),
                (Some(name), None) => Some(name.to_string()),
                _ => None,
            },
        ),
        (
            "CPU model",
            host.pointer("/cpu/model/known")
                .and_then(Value::as_str)
                .map(str::to_string),
        ),
        (
            "CPU count",
            host.pointer("/cpu/count/known")
                .and_then(Value::as_u64)
                .map(|n| n.to_string()),
        ),
        (
            "memory total",
            host.pointer("/memory/total_bytes/known")
                .and_then(Value::as_u64)
                .map(|n| n.to_string()),
        ),
    ]
}

/// One host fact per node, and the findings where a node differs from its
/// peers.
fn peer_findings(runs: &[StartupView]) -> Vec<Finding> {
    let newest: BTreeMap<&str, &StartupView> = {
        let mut newest: BTreeMap<&str, &StartupView> = BTreeMap::new();
        for run in runs.iter().filter(|r| r.host.is_some()) {
            newest
                .entry(run.node.as_str())
                .and_modify(|current| {
                    if run.start_ms > current.start_ms {
                        *current = run;
                    }
                })
                .or_insert(run);
        }
        newest
    };
    let peers: Vec<Peer<'_>> = newest
        .values()
        .map(|run| Peer {
            node: &run.node,
            app: &run.app,
            run_id: &run.run_id,
            host: run.host.as_ref().expect("filtered on host"),
        })
        .collect();
    if peers.len() < 2 {
        return Vec::new();
    }
    let mut out = Vec::new();
    let facts: Vec<[(&'static str, Option<String>); HOST_FACT_COUNT]> =
        peers.iter().map(|p| host_facts(p.host)).collect();
    for field in 0..HOST_FACT_COUNT {
        let name = facts[0][field].0;
        let known: Vec<(&Peer<'_>, &str)> = peers
            .iter()
            .zip(&facts)
            .filter_map(|(peer, values)| values[field].1.as_deref().map(|value| (peer, value)))
            .collect();
        if known.is_empty() {
            continue;
        }
        let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
        for (_, value) in &known {
            *counts.entry(value).or_insert(0) += 1;
        }
        for (peer, values) in peers.iter().zip(&facts) {
            if values[field].1.is_none() {
                out.push(Finding {
                    level: Level::Warn,
                    node: peer.node.to_string(),
                    text: format!(
                        "run {} ({}): {} is unknown, so it cannot be compared with its {} peer(s)",
                        peer.run_id,
                        peer.app,
                        name,
                        peers.len() - 1
                    ),
                });
            }
        }
        if counts.len() < 2 {
            continue;
        }
        let most = counts.iter().max_by_key(|(_, count)| **count);
        match most {
            Some((reference, count)) if counts.values().filter(|c| *c == count).count() == 1 => {
                let count = *count;
                for (peer, values) in peers.iter().zip(&facts) {
                    let Some(value) = values[field].1.as_deref() else {
                        continue;
                    };
                    if value != *reference {
                        out.push(Finding {
                            level: Level::Warn,
                            node: peer.node.to_string(),
                            text: format!(
                                "run {} ({}): {} {value} differs from {reference}, which {count} of {} peers report",
                                peer.run_id,
                                peer.app,
                                name,
                                peers.len()
                            ),
                        });
                    }
                }
            }
            _ => {
                // No majority, which is a two-node fleet that disagrees or a
                // tie. Every peer is named against the whole set, so the
                // finding says what the disagreement is rather than picking a
                // side it cannot justify.
                let listed = peers
                    .iter()
                    .zip(&facts)
                    .filter_map(|(peer, values)| {
                        values[field]
                            .1
                            .as_deref()
                            .map(|value| format!("{} {value}", peer.node))
                    })
                    .take(PEERS_LISTED)
                    .collect::<Vec<_>>()
                    .join(", ");
                for (peer, values) in peers.iter().zip(&facts) {
                    let Some(value) = values[field].1.as_deref() else {
                        continue;
                    };
                    out.push(Finding {
                        level: Level::Warn,
                        node: peer.node.to_string(),
                        text: format!(
                            "run {} ({}): {} {value} differs from its peers, which report {listed}",
                            peer.run_id, peer.app, name
                        ),
                    });
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::journal::Latest;

    /// A run id from the monitor's own start, so a test's record is judged
    /// exactly as a live restart is.
    fn run_id(latest: &Latest, offset_ms: u64) -> String {
        format!("{}-{}", latest.startup.started_ms() + offset_ms, 4242)
    }

    fn entry(line: Value) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "MESSAGE": line.to_string(),
            "M6_TYPE": line["type"],
            "_HOSTNAME": "build",
        }))
        .unwrap()
    }

    fn startup_line(run_id: &str, seq: u64, part: &str, app: &str, version: &str) -> Value {
        serde_json::json!({
            "timestamp": "2026-11-18T02:16:20.858965Z",
            "level": "INFO",
            "fields": {
                "run_id": run_id,
                "seq": seq,
                "part": part,
                "app": app,
                "version": version,
                "message": format!("startup record: {part}"),
            },
            "type": "startup",
        })
    }

    fn with(mut line: Value, key: &str, value: Value) -> Value {
        line["fields"][key] = value;
        line
    }

    /// A whole record, in the order the writer writes it, with the host facts
    /// a test asks for.
    #[allow(clippy::too_many_arguments)]
    fn record(
        latest: &mut Latest,
        run_id: &str,
        app: &str,
        version: &str,
        config: &[(&str, &str)],
        host: Option<Value>,
        decided: Value,
        lines_before_complete: u64,
    ) {
        let identity = startup_line(run_id, 1, "identity", app, version);
        let identity = with(identity, "tag", serde_json::json!("v1.16.0"));
        latest.ingest(&entry(identity));
        for (i, (role, sha)) in config.iter().enumerate() {
            let line = startup_line(run_id, 2 + i as u64, "config_file", app, version);
            let line = with(line, "role", serde_json::json!(role));
            let line = with(line, "sha256", serde_json::json!(sha));
            latest.ingest(&entry(line));
        }
        let platform = startup_line(run_id, 2 + config.len() as u64, "platform", app, version);
        let platform = with(
            platform,
            "host",
            host.unwrap_or_else(|| serde_json::json!({})),
        );
        let platform = with(platform, "decided", decided);
        latest.ingest(&entry(platform));
        let complete = startup_line(run_id, 3 + config.len() as u64, "complete", app, version);
        let complete = with(complete, "lines", serde_json::json!(lines_before_complete));
        latest.ingest(&entry(complete));
    }

    fn shutdown(latest: &mut Latest, run_id: &str, app: &str, version: &str, reason: &str) {
        let line = serde_json::json!({
            "timestamp": "2026-11-18T02:17:20.858965Z",
            "level": "INFO",
            "fields": {
                "run_id": run_id,
                "app": app,
                "version": version,
                "uptime_ms": 60_000,
                "reason": reason,
                "message": "shutdown",
            },
            "type": "shutdown",
        });
        latest.ingest(&entry(line));
    }

    fn host(kernel: &str, memory: u64) -> Value {
        serde_json::json!({
            "hostname": {"known": "origin"},
            "fqdn": {"known": "origin.example.com"},
            "os": {"known": {"name": "Linux", "version": {"known": "13"}}},
            "kernel": {"known": {"release": kernel, "version": "#1"}},
            "cpu": {"model": {"known": "Example CPU"}, "count": {"known": 4}},
            "memory": {"total_bytes": {"known": memory}, "page_size": {"known": 4096}},
            "limits": {},
            "clock": {},
            "virtualization": {"known": "none"},
        })
    }

    fn expectations(app: &str, version: &str, sha: &str) -> Expectations {
        Expectations {
            environment: Some("prod".to_string()),
            deploy_in_progress: false,
            nodes: BTreeMap::from([(
                "origin".to_string(),
                NodeExpectation {
                    apps: BTreeMap::from([(
                        app.to_string(),
                        AppExpectation {
                            version: version.to_string(),
                            config: BTreeMap::from([("config".to_string(), sha.to_string())]),
                            decided: BTreeMap::new(),
                        },
                    )]),
                },
            )]),
        }
    }

    fn facts(latest: &mut Latest, expected: Expectations) -> Facts {
        Facts {
            runs: latest.startup.views(m6_core::platform::now()),
            expectations: expected,
            expected_state: ExpectedState::Loaded,
            expected_path: Some("/run/m6/expected.json".to_string()),
            environment: Some("prod".to_string()),
            cut: latest.startup.cut,
            orphan_shutdowns: latest.startup.orphan_shutdowns,
            journal_unavailable: None,
        }
    }

    fn empty_digest() -> Digest {
        Digest {
            generated_at: "2026-11-18T02:20:00Z".to_string(),
            nodes: Vec::new(),
            findings: Vec::new(),
            level: Level::Ok,
            thresholds: crate::digest::Thresholds::default(),
            handshake_file_age_s: None,
            history: None,
            process_groups: Vec::new(),
            startup: StartupDigest::default(),
        }
    }

    fn findings_of(facts: &Facts) -> Vec<Finding> {
        let mut digest = empty_digest();
        judge(&mut digest, facts);
        digest.findings
    }

    fn texts(findings: &[Finding]) -> String {
        findings
            .iter()
            .map(|f| format!("{:?} {} {}\n", f.level, f.node, f.text))
            .collect()
    }

    fn clean(latest: &mut Latest, app: &str, version: &str, sha: &str) {
        let id = run_id(latest, 1_000);
        record(
            latest,
            &id,
            app,
            version,
            &[("config", sha)],
            Some(host("6.11.0-1", 8_000_000_000)),
            serde_json::json!({"socket_path": "/run/m6/a.sock"}),
            4,
        );
    }

    #[test]
    fn a_clean_run_produces_no_findings() {
        let mut latest = Latest::new();
        clean(&mut latest, "m6-http", "1.16.0+abc123", "aa11");
        let facts = facts(
            &mut latest,
            expectations("m6-http", "1.16.0+abc123", "aa11"),
        );
        let found = findings_of(&facts);
        assert!(
            found.is_empty(),
            "a clean run must be silent, found:\n{}",
            texts(&found)
        );
    }

    #[test]
    fn a_dirty_build_outside_dev_is_a_fault() {
        let mut latest = Latest::new();
        clean(&mut latest, "m6-http", "1.16.0+abc123.dirty", "aa11");
        let facts = facts(
            &mut latest,
            expectations("m6-http", "1.16.0+abc123.dirty", "aa11"),
        );
        let found = findings_of(&facts);
        assert!(
            found.iter().any(|f| f.level == Level::Fault
                && f.text.contains("dirty")
                && f.text.contains("run ")
                && f.text.contains("m6-http")),
            "a dirty build on prod is refused:\n{}",
            texts(&found)
        );
    }

    #[test]
    fn a_dirty_build_is_allowed_on_dev() {
        let mut latest = Latest::new();
        clean(&mut latest, "m6-http", "1.16.0+abc123.dirty", "aa11");
        let mut facts = facts(
            &mut latest,
            expectations("m6-http", "1.16.0+abc123.dirty", "aa11"),
        );
        facts.environment = Some("dev".to_string());
        facts.expectations.environment = Some("dev".to_string());
        let found = findings_of(&facts);
        assert!(
            !found.iter().any(|f| f.text.contains("dirty")),
            "dev builds dirty:\n{}",
            texts(&found)
        );
    }

    #[test]
    fn a_pre_release_on_prod_is_a_fault() {
        let mut latest = Latest::new();
        clean(&mut latest, "m6-http", "1.16.0-rc.1+abc123", "aa11");
        let facts = facts(
            &mut latest,
            expectations("m6-http", "1.16.0-rc.1+abc123", "aa11"),
        );
        let found = findings_of(&facts);
        assert!(
            found
                .iter()
                .any(|f| f.level == Level::Fault && f.text.contains("pre-release")),
            "a pre-release on prod is refused:\n{}",
            texts(&found)
        );
    }

    #[test]
    fn a_version_that_differs_from_the_deploy_is_a_fault() {
        let mut latest = Latest::new();
        clean(&mut latest, "m6-http", "1.16.0+abc123", "aa11");
        let facts = facts(
            &mut latest,
            expectations("m6-http", "1.16.0+def456", "aa11"),
        );
        let found = findings_of(&facts);
        let f = found
            .iter()
            .find(|f| {
                f.text
                    .contains("differs from the release the deploy recorded")
            })
            .unwrap_or_else(|| panic!("a version mismatch is a fault:\n{}", texts(&found)));
        assert_eq!(f.level, Level::Fault);
        assert_eq!(f.node, "origin");
        assert!(f.text.contains("1.16.0+abc123") && f.text.contains("1.16.0+def456"));
        assert!(f.text.contains("run "), "the finding names the run id");
    }

    #[test]
    fn a_stale_config_sha_is_a_fault() {
        let mut latest = Latest::new();
        clean(&mut latest, "m6-http", "1.16.0+abc123", "aa11");
        let facts = facts(
            &mut latest,
            expectations("m6-http", "1.16.0+abc123", "bb22"),
        );
        let found = findings_of(&facts);
        let f = found
            .iter()
            .find(|f| f.text.contains("sha256"))
            .unwrap_or_else(|| panic!("a stale config sha is a fault:\n{}", texts(&found)));
        assert_eq!(f.level, Level::Fault);
        assert!(f.text.contains("aa11") && f.text.contains("bb22"));
    }

    #[test]
    fn a_decided_fact_that_differs_from_the_deploy_is_a_fault() {
        let mut latest = Latest::new();
        clean(&mut latest, "m6-http", "1.16.0+abc123", "aa11");
        let mut expected = expectations("m6-http", "1.16.0+abc123", "aa11");
        expected
            .nodes
            .get_mut("origin")
            .unwrap()
            .apps
            .get_mut("m6-http")
            .unwrap()
            .decided
            .insert(
                "socket_path".to_string(),
                serde_json::json!("/run/m6/other.sock"),
            );
        let facts = facts(&mut latest, expected);
        let found = findings_of(&facts);
        let f = found
            .iter()
            .find(|f| f.text.contains("decided socket_path"))
            .unwrap_or_else(|| panic!("decided drift is a fault:\n{}", texts(&found)));
        assert_eq!(f.level, Level::Fault);
    }

    #[test]
    fn a_complete_count_that_disagrees_is_a_fault() {
        let mut latest = Latest::new();
        let id = run_id(&latest, 1_000);
        // Four lines written, the complete line says five.
        record(
            &mut latest,
            &id,
            "m6-http",
            "1.16.0+abc123",
            &[("config", "aa11")],
            Some(host("6.11.0-1", 8_000_000_000)),
            serde_json::json!({}),
            5,
        );
        let facts = facts(
            &mut latest,
            expectations("m6-http", "1.16.0+abc123", "aa11"),
        );
        let found = findings_of(&facts);
        let f = found
            .iter()
            .find(|f| f.text.contains("complete line counts"))
            .unwrap_or_else(|| panic!("a count mismatch is a fault:\n{}", texts(&found)));
        assert_eq!(f.level, Level::Fault);
        assert!(f.text.contains("5") && f.text.contains("4"));
    }

    #[test]
    fn a_record_that_never_completes_is_a_fault() {
        let mut latest = Latest::new();
        let id = run_id(&latest, 1_000);
        // The record starts and then the process exits without its complete
        // line.
        latest.ingest(&entry(startup_line(
            &id,
            1,
            "identity",
            "m6-http",
            "1.16.0+abc123",
        )));
        shutdown(&mut latest, &id, "m6-http", "1.16.0+abc123", "signal");
        let facts = facts(
            &mut latest,
            expectations("m6-http", "1.16.0+abc123", "aa11"),
        );
        let found = findings_of(&facts);
        let f = found
            .iter()
            .find(|f| f.text.contains("never wrote its complete line"))
            .unwrap_or_else(|| panic!("an unfinished record is a fault:\n{}", texts(&found)));
        assert_eq!(f.level, Level::Fault);
        assert!(f.text.contains(&id));
    }

    #[test]
    fn an_unexpected_restart_quotes_the_shutdown_reason() {
        let mut latest = Latest::new();
        let first = run_id(&latest, 1_000);
        record(
            &mut latest,
            &first,
            "m6-http",
            "1.16.0+abc123",
            &[("config", "aa11")],
            Some(host("6.11.0-1", 8_000_000_000)),
            serde_json::json!({}),
            4,
        );
        shutdown(&mut latest, &first, "m6-http", "1.16.0+abc123", "signal");
        let second = run_id(&latest, 60_000);
        record(
            &mut latest,
            &second,
            "m6-http",
            "1.16.0+abc123",
            &[("config", "aa11")],
            Some(host("6.11.0-1", 8_000_000_000)),
            serde_json::json!({}),
            4,
        );
        let facts = facts(
            &mut latest,
            expectations("m6-http", "1.16.0+abc123", "aa11"),
        );
        let found = findings_of(&facts);
        let f = found
            .iter()
            .find(|f| f.text.contains("unexpected restart"))
            .unwrap_or_else(|| {
                panic!(
                    "a restart outside a deploy is unexpected:\n{}",
                    texts(&found)
                )
            });
        assert_eq!(f.level, Level::Warn);
        assert!(f.text.contains(&second), "the new run is named");
        assert!(f.text.contains(&first), "the previous run is named");
        assert!(
            f.text.contains("\"signal\""),
            "the shutdown reason is quoted"
        );
    }

    #[test]
    fn an_unexpected_restart_without_a_shutdown_says_why() {
        let mut latest = Latest::new();
        let first = run_id(&latest, 1_000);
        record(
            &mut latest,
            &first,
            "m6-http",
            "1.16.0+abc123",
            &[("config", "aa11")],
            Some(host("6.11.0-1", 8_000_000_000)),
            serde_json::json!({}),
            4,
        );
        // No shutdown line: the process died.
        let second = run_id(&latest, 60_000);
        record(
            &mut latest,
            &second,
            "m6-http",
            "1.16.0+abc123",
            &[("config", "aa11")],
            Some(host("6.11.0-1", 8_000_000_000)),
            serde_json::json!({}),
            4,
        );
        let facts = facts(
            &mut latest,
            expectations("m6-http", "1.16.0+abc123", "aa11"),
        );
        let found = findings_of(&facts);
        let f = found
            .iter()
            .find(|f| f.text.contains("unexpected restart"))
            .unwrap_or_else(|| {
                panic!(
                    "a restart with no shutdown is unexpected:\n{}",
                    texts(&found)
                )
            });
        assert!(f.text.contains("wrote no shutdown line"));
        assert!(f.text.contains(&first));
    }

    #[test]
    fn a_restart_during_a_deploy_is_expected() {
        let mut latest = Latest::new();
        let first = run_id(&latest, 1_000);
        record(
            &mut latest,
            &first,
            "m6-http",
            "1.16.0+abc123",
            &[("config", "aa11")],
            Some(host("6.11.0-1", 8_000_000_000)),
            serde_json::json!({}),
            4,
        );
        shutdown(&mut latest, &first, "m6-http", "1.16.0+abc123", "signal");
        let second = run_id(&latest, 60_000);
        record(
            &mut latest,
            &second,
            "m6-http",
            "1.16.0+abc123",
            &[("config", "aa11")],
            Some(host("6.11.0-1", 8_000_000_000)),
            serde_json::json!({}),
            4,
        );
        let mut expected = expectations("m6-http", "1.16.0+abc123", "aa11");
        expected.deploy_in_progress = true;
        let facts = facts(&mut latest, expected);
        let found = findings_of(&facts);
        assert!(
            !found.iter().any(|f| f.text.contains("unexpected restart")),
            "a deploy restarts nodes:\n{}",
            texts(&found)
        );
    }

    /// Every restart carries how the previous run ended in `/digest`, a
    /// graceful one at `info` and an unexpected one at `warn`, and each run
    /// counts the raw lines the monitor ingested for it.
    #[test]
    fn every_restart_carries_the_previous_shutdown_and_its_level() {
        let digest_of = |deploying: bool, shut_down: bool| {
            let mut latest = Latest::new();
            let first = run_id(&latest, 1_000);
            record(
                &mut latest,
                &first,
                "m6-http",
                "1.16.0+abc123",
                &[("config", "aa11")],
                Some(host("6.11.0-1", 8_000_000_000)),
                serde_json::json!({}),
                4,
            );
            if shut_down {
                shutdown(&mut latest, &first, "m6-http", "1.16.0+abc123", "signal");
            }
            let second = run_id(&latest, 60_000);
            record(
                &mut latest,
                &second,
                "m6-http",
                "1.16.0+abc123",
                &[("config", "aa11")],
                Some(host("6.11.0-1", 8_000_000_000)),
                serde_json::json!({}),
                4,
            );
            let mut expected = expectations("m6-http", "1.16.0+abc123", "aa11");
            expected.deploy_in_progress = deploying;
            let facts = facts(&mut latest, expected);
            let mut digest = empty_digest();
            judge(&mut digest, &facts);
            let run = digest
                .startup
                .runs
                .iter()
                .find(|r| r.run_id == second)
                .expect("the second run is current")
                .clone();
            (first, serde_json::to_value(&run).expect("a run serialises"))
        };

        let (first, graceful) = digest_of(true, true);
        assert_eq!(graceful["previous_run_id"], first.as_str());
        assert_eq!(graceful["previous_shutdown"]["reason"], "signal");
        assert_eq!(
            graceful["previous_shutdown"]["at"],
            "2026-11-18T02:17:20.858965Z"
        );
        assert_eq!(graceful["previous_shutdown"]["uptime_ms"], 60_000);
        assert_eq!(graceful["restart"], "info", "a restart during a deploy");

        let (_, outside) = digest_of(false, true);
        assert_eq!(outside["previous_shutdown"]["reason"], "signal");
        assert_eq!(outside["restart"], "warn", "a restart outside a deploy");

        let (_, died) = digest_of(true, false);
        assert!(
            died["previous_shutdown"].is_null() && died.get("previous_shutdown").is_some(),
            "a previous run with no shutdown line is null, not absent: {died}"
        );
        assert_eq!(died["restart"], "warn", "a run that died");

        assert_eq!(graceful["lines"], 4);
        assert_eq!(graceful["ingested_lines"], 4);
    }

    /// The raw count includes what the record count leaves out: a replayed
    /// start-up line and the shutdown line.
    #[test]
    fn the_ingested_count_includes_replays_and_the_shutdown_line() {
        let mut latest = Latest::new();
        let id = run_id(&latest, 1_000);
        record(
            &mut latest,
            &id,
            "m6-http",
            "1.16.0+abc123",
            &[("config", "aa11")],
            Some(host("6.11.0-1", 8_000_000_000)),
            serde_json::json!({}),
            4,
        );
        latest.ingest(&entry(startup_line(
            &id,
            1,
            "identity",
            "m6-http",
            "1.16.0+abc123",
        )));
        shutdown(&mut latest, &id, "m6-http", "1.16.0+abc123", "signal");
        let facts = facts(
            &mut latest,
            expectations("m6-http", "1.16.0+abc123", "aa11"),
        );
        let mut digest = empty_digest();
        judge(&mut digest, &facts);
        let run = &digest.startup.runs[0];
        assert_eq!((run.lines, run.ingested_lines), (4, 6));
        assert!(
            run.previous_shutdown.is_none(),
            "a first run has no previous"
        );
        assert!(run.restart.is_none());
    }

    #[test]
    fn a_peer_with_a_different_kernel_is_a_warning() {
        let mut latest = Latest::new();
        let id = run_id(&latest, 1_000);
        record(
            &mut latest,
            &id,
            "m6-http",
            "1.16.0+abc123",
            &[("config", "aa11")],
            Some(host("6.11.0-1", 8_000_000_000)),
            serde_json::json!({}),
            4,
        );
        let other = run_id(&latest, 2_000);
        record_other_node(&mut latest, &other, "edge-a", "6.8.0-2", 8_000_000_000);
        let mut expected = expectations("m6-http", "1.16.0+abc123", "aa11");
        expected.nodes.insert(
            "edge-a".to_string(),
            NodeExpectation {
                apps: BTreeMap::from([(
                    "m6-http".to_string(),
                    AppExpectation {
                        version: "1.16.0+abc123".to_string(),
                        config: BTreeMap::from([("config".to_string(), "aa11".to_string())]),
                        decided: BTreeMap::new(),
                    },
                )]),
            },
        );
        let found = findings_of(&facts(&mut latest, expected));
        let peer: Vec<&Finding> = found
            .iter()
            .filter(|f| f.text.contains("kernel release"))
            .collect();
        assert_eq!(
            peer.len(),
            2,
            "each node differs from the other:\n{}",
            texts(&found)
        );
        for f in peer {
            assert_eq!(f.level, Level::Warn);
            assert!(f.text.contains("run "));
            assert!(
                (f.node == "origin" && f.text.contains("6.11.0-1"))
                    || (f.node == "edge-a" && f.text.contains("6.8.0-2"))
            );
        }
    }

    /// A second node's record, written with the same helper but its own
    /// hostname and kernel.
    fn record_other_node(latest: &mut Latest, run_id: &str, node: &str, kernel: &str, memory: u64) {
        let app = "m6-http";
        let version = "1.16.0+abc123";
        let mut host = host(kernel, memory);
        host["hostname"] = serde_json::json!({"known": node});
        let identity = startup_line(run_id, 1, "identity", app, version);
        latest.ingest(&entry(identity));
        let file = startup_line(run_id, 2, "config_file", app, version);
        let file = with(file, "role", serde_json::json!("config"));
        let file = with(file, "sha256", serde_json::json!("aa11"));
        latest.ingest(&entry(file));
        let platform = startup_line(run_id, 3, "platform", app, version);
        let platform = with(platform, "host", host);
        let platform = with(platform, "decided", serde_json::json!({}));
        latest.ingest(&entry(platform));
        let complete = startup_line(run_id, 4, "complete", app, version);
        let complete = with(complete, "lines", serde_json::json!(4));
        latest.ingest(&entry(complete));
    }

    #[test]
    fn a_missing_expectation_file_is_a_warning_not_a_fault() {
        let mut latest = Latest::new();
        clean(&mut latest, "m6-http", "1.16.0+abc123", "aa11");
        let mut facts = facts(&mut latest, Expectations::default());
        facts.expected_state = ExpectedState::Missing;
        facts.expected_path = Some("/run/m6/expected.json".to_string());
        let found = findings_of(&facts);
        let f = found
            .iter()
            .find(|f| f.text.contains("waiting for the deploy side"))
            .unwrap_or_else(|| panic!("a missing expectation file is said:\n{}", texts(&found)));
        assert_eq!(f.level, Level::Warn);
        assert!(f.text.contains("/run/m6/expected.json"));
    }

    #[test]
    fn the_digest_carries_each_runs_version_start_and_config_sha() {
        let mut latest = Latest::new();
        clean(&mut latest, "m6-http", "1.16.0+abc123", "aa11");
        let mut digest = empty_digest();
        judge(
            &mut digest,
            &facts(
                &mut latest,
                expectations("m6-http", "1.16.0+abc123", "aa11"),
            ),
        );
        let run = digest.startup.runs.first().expect("one run");
        assert_eq!(run.version, "1.16.0+abc123");
        assert_eq!(run.started_at, "2026-11-18T02:16:20.858965Z");
        assert_eq!(run.config.get("config").map(String::as_str), Some("aa11"));
        assert_eq!(run.state, "complete");
        assert_eq!(digest.startup.expected_state, "loaded");
    }

    #[test]
    fn the_deploy_record_loads_or_says_why_it_cannot() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("expected.json");
        let (_, state) = load(Some(&path));
        assert_eq!(
            state,
            ExpectedState::Missing,
            "a file the deploy has not written yet"
        );

        std::fs::write(&path, "{ not json").unwrap();
        let (_, state) = load(Some(&path));
        assert!(matches!(state, ExpectedState::Unreadable(_)), "{state:?}");

        std::fs::write(&path, r#"{"nodes": {}, "surprise": 1}"#).unwrap();
        let (_, state) = load(Some(&path));
        assert!(
            matches!(state, ExpectedState::Unreadable(_)),
            "an unknown field is refused, not ignored: {state:?}"
        );

        std::fs::write(
            &path,
            r#"{"environment": "prod", "nodes": {"origin": {"apps": {"m6-http": {"version": "1.16.0+abc"}}}}}"#,
        )
        .unwrap();
        let (expected, state) = load(Some(&path));
        assert_eq!(state, ExpectedState::Loaded);
        assert_eq!(expected.environment.as_deref(), Some("prod"));
        assert_eq!(
            expected.nodes["origin"].apps["m6-http"].version,
            "1.16.0+abc"
        );

        let (_, state) = load(None);
        assert_eq!(state, ExpectedState::NotConfigured);
    }

    #[test]
    fn a_record_cut_by_the_journal_tail_is_not_judged() {
        let mut latest = Latest::new();
        let id = run_id(&latest, 1_000);
        // First line seen is seq 4: the tail cut the start.
        latest.ingest(&entry(startup_line(
            &id,
            4,
            "config_file",
            "m6-http",
            "1.16.0+abc123",
        )));
        assert_eq!(latest.startup.cut, 1);
        assert!(latest.startup.views(m6_core::platform::now()).is_empty());
    }
}
