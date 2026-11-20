//! The monitor's read of mon's journal.
//!
//! Every node streams its journal to mon with `systemd-journal-upload`, and mon
//! keeps it with `systemd-journal-remote` under a directory of per-peer files.
//! The metrics and traffic lines are in it, so the monitor reads them from
//! there instead of polling an endpoint: one `journalctl` read at start and one
//! each poll interval, its stdout one more descriptor in the existing loop, and
//! no new timer or thread.
//!
//! **A read does not follow.** `journalctl --follow` over a remote journal that
//! two hosts upload into returns only one host's entries, whichever it started
//! behind, and never the other's. The uploaders deliver their batches with
//! different delays, so the lagging host's new entries are older than the
//! follower's position and are never returned. The same command without
//! `--follow` prints both hosts' entries every time, so the service reads a
//! count of lines and the child exits, the way `--check` always has.
//!
//! **A read's window is a count of lines.** Each read passes `--lines=N`, with
//! `N = max(200, 14 x the number of [[monitor.nodes]] entries)`. A node writes
//! about 7 matching lines a minute, 6 metrics documents at 10 seconds and 1
//! traffic document at 60, so 14 lines for each node cover at least two minutes
//! of the whole fleet, which is more than two poll intervals at the default 30
//! seconds. Start-up lines share the window and shorten it a little, and 200 is
//! the floor so a small fleet still holds a node's start-up record. The monitor's
//! clock is not read: the reads overlap, and [`Latest`] and [`Startup`] take each
//! entry once.
//!
//! **Known limits, for this release.** The window is the newest N entries of
//! every node merged and ordered by each sender's own clock. It is neither a
//! time nor a share for each node. A node whose clock lags by more than the
//! window's span is not read. A node whose clock runs ahead, or that writes much
//! faster than the others, fills the window and hides the rest. A poll interval
//! over about two minutes on a fleet of more than 14 nodes is not covered. A
//! start-up record older than the window is not read, and its absence is not a
//! finding. A read per node is the planned fix.
//!
//! **A document is ordered by the journal's `__REALTIME_TIMESTAMP`.** For a
//! remote journal that is the uploading node's own clock, which no application
//! on the node can set but the node itself can. It orders one node's own
//! entries kept under one node name, which a node chooses in its own document. Nothing
//! inside `MESSAGE` orders anything. An entry without the field is counted as
//! malformed, and an older entry read later never replaces a newer one.
//!
//! # The start-up record
//!
//! `M6_TYPE=startup` and `M6_TYPE=shutdown` are read by the same child as the
//! two documents, so one `journalctl` process reads four types. A start-up
//! record is several lines sharing one `run_id`, so this
//! module groups them and [`Startup`] holds the newest record of each node and
//! app. The node is the record's own `node` field when it carries one, and the
//! machine's hostname from its platform line otherwise. The app is the record's
//! own `app` field. `_HOSTNAME` is never read for either.
//!
//! A record whose first line the read's window cut cannot be judged, so it is
//! counted in [`Startup::cut`] and not shown. A record that started
//! before the monitor did is shown as the current run and is not judged for
//! its completeness or its restarts, because the tail can cut its last lines
//! as easily as its first. A line seen again in the next, overlapping read is
//! counted once, by the journal's time for it and its `seq`.
//!
//! # The child
//!
//! At most one read starts per poll interval, and a read still in flight is not
//! started again. A read still in flight one interval after it started is killed
//! and reported in [`Reader::stopped`], and the next read starts normally. The
//! child is reaped without waiting for it. A reader made by
//! [`Reader::unavailable`] starts nothing and keeps its reason. The child is
//! killed at stop, so no `journalctl` outlives the monitor.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};
use std::time::{Duration, Instant};

use m6_core::platform::process::{Child, Io, Streams};

use m6_core::limits::{Bounded, Limit};

/// How long a start-up record may take from its first line to its `complete`
/// line before the monitor reports it unfinished.
///
/// A start-up record is written once, in one burst, after logging is up. Two
/// minutes is far past any start that would produce a usable record, and a
/// record that has not closed in that window never will.
pub const INCOMPLETE_AFTER: Duration = Duration::from_secs(120);

/// How many start-up records may be assembling at once. A burst from a whole
/// fleet writing at start is far below this.
const MAX_PENDING_RECORDS: usize = 64;

/// How many finished records are kept: one per node and app, plus the
/// restarts that have not aged out.
const MAX_KEPT_RECORDS: usize = 256;

/// How many unfinished records are kept for the report.
const MAX_FAILED_RECORDS: usize = 32;

/// How many distinct journal entries one record remembers, so a read that
/// delivers an entry again is told from a new one. A record is a few dozen
/// lines, so this is far past any real one.
const MAX_SEEN_LINES: usize = 1024;

/// One clean exit, as the `type=shutdown` line states it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shutdown {
    pub run_id: String,
    pub app: String,
    pub version: String,
    pub uptime_ms: u64,
    pub reason: String,
    /// The line's own timestamp.
    pub at: String,
}

/// The run a start-up record replaced, kept so a restart can quote it.
///
/// The previous run is always a record that wrote its `complete` line, because
/// only a complete record becomes the current one. A record that never closed
/// is reported as itself, not as the run that replaced it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreviousRun {
    pub run_id: String,
    /// The shutdown line, when the previous run wrote one.
    pub shutdown: Option<Shutdown>,
}

/// The identity of one start-up record in the report: the node and app it
/// belongs to, and the process index when the process declared one.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct RunKey {
    node: String,
    app: String,
    instance: Option<u64>,
}

/// One start-up record, as the lines that share its `run_id` built it.
#[derive(Debug, Clone)]
pub struct StartupRecord {
    pub run_id: String,
    /// The run id's first half, the process start in Unix milliseconds.
    start_ms: u64,
    pub app: String,
    pub version: String,
    pub tag: Option<String>,
    /// The record's own `node` field, when a line carries one.
    pub node: Option<String>,
    /// The identity line's timestamp.
    pub started_at: String,
    /// The hostname the platform line reports.
    pub hostname: Option<String>,
    /// The process index the decided facts report.
    pub process_index: Option<u64>,
    /// Config file role to the sha256 as read.
    pub config: BTreeMap<String, String>,
    /// Config file role to the path as read.
    pub config_paths: BTreeMap<String, String>,
    /// The platform line's `host`, the machine's facts.
    pub host: Option<serde_json::Value>,
    /// The platform line's `decided`, what this process chose.
    pub decided: Option<serde_json::Value>,
    /// Every start-up line seen for this run.
    pub lines: u64,
    /// Every journal line ingested for this run's id: its start-up lines, a
    /// replay of them, and its shutdown line. Raw, so a reader can compare it
    /// with `lines` and see a replay or a shutdown arrive.
    pub ingested: u64,
    /// The `complete` line's count, when it arrived.
    pub declared: Option<u64>,
    /// The shutdown line, when the run wrote one.
    pub shutdown: Option<Shutdown>,
    /// The journal entries this record has counted, as their recorded time and
    /// `seq`. Reads overlap, so every entry arrives again and counts once.
    seen: BTreeSet<(u64, u64)>,
    /// The monitor's clock at the first line of the run.
    pub first_seen: Instant,
    /// The run a record of this node and app replaced.
    pub previous: Option<PreviousRun>,
    /// Whether the run started after the monitor did. A run from the journal
    /// tail is shown and is not judged.
    judged: bool,
}

impl StartupRecord {
    fn new(
        run_id: &str,
        start_ms: u64,
        started_at: String,
        first_seen: Instant,
        judged: bool,
    ) -> StartupRecord {
        StartupRecord {
            run_id: run_id.to_string(),
            start_ms,
            app: String::new(),
            version: String::new(),
            tag: None,
            node: None,
            started_at,
            hostname: None,
            process_index: None,
            config: BTreeMap::new(),
            config_paths: BTreeMap::new(),
            host: None,
            decided: None,
            lines: 0,
            ingested: 0,
            declared: None,
            shutdown: None,
            seen: BTreeSet::new(),
            first_seen,
            previous: None,
            judged,
        }
    }

    /// Note one journal entry, and say whether it is new to this record.
    fn see(&mut self, entry: (u64, u64)) -> bool {
        self.seen.len() < MAX_SEEN_LINES && self.seen.insert(entry)
    }

    /// The node this record belongs to: its own `node` field, else the
    /// machine's hostname from its platform line, else the app as a last
    /// resort. The last case is reported as unattributable rather than guessed
    /// at.
    fn node_key(&self) -> String {
        self.node
            .clone()
            .or_else(|| self.hostname.clone())
            .unwrap_or_else(|| self.app.clone())
    }

    fn key(&self) -> RunKey {
        RunKey {
            node: self.node_key(),
            app: self.app.clone(),
            instance: self.process_index,
        }
    }
}

/// One run as the report reads it, whether it is the current record or one
/// that never finished.
#[derive(Debug, Clone)]
pub struct StartupView {
    pub node: String,
    pub app: String,
    /// The hostname the platform line reported. `None` when it reported none,
    /// which is when the node is the app's name alone.
    pub hostname: Option<String>,
    pub process_index: Option<u64>,
    pub run_id: String,
    /// The run id's first half, the process start in Unix milliseconds.
    pub start_ms: u64,
    pub version: String,
    pub tag: Option<String>,
    pub started_at: String,
    pub config: BTreeMap<String, String>,
    pub config_paths: BTreeMap<String, String>,
    pub host: Option<serde_json::Value>,
    pub decided: Option<serde_json::Value>,
    /// Start-up lines received for this run.
    pub lines: u64,
    /// Every journal line ingested for this run's id, replays and the shutdown
    /// line included.
    pub ingested: u64,
    /// The `complete` line's count, when it arrived.
    pub complete: Option<u64>,
    pub shutdown: Option<Shutdown>,
    pub previous: Option<PreviousRun>,
    /// Whether the run started after the monitor did.
    pub judged: bool,
    /// False for a record that never closed.
    pub current: bool,
}

impl StartupRecord {
    fn view(&self, current: bool) -> StartupView {
        StartupView {
            node: self.node_key(),
            app: self.app.clone(),
            hostname: self.hostname.clone(),
            process_index: self.process_index,
            run_id: self.run_id.clone(),
            start_ms: self.start_ms,
            version: self.version.clone(),
            tag: self.tag.clone(),
            started_at: self.started_at.clone(),
            config: self.config.clone(),
            config_paths: self.config_paths.clone(),
            host: self.host.clone(),
            decided: self.decided.clone(),
            lines: self.lines,
            ingested: self.ingested,
            complete: self.declared,
            shutdown: self.shutdown.clone(),
            previous: self.previous.clone(),
            judged: self.judged,
            current,
        }
    }
}

/// The start-up records the journal has carried, by node and app.
#[derive(Debug)]
pub struct Startup {
    /// When the monitor started, in Unix milliseconds. A record that started
    /// before this is from the journal tail and is not judged.
    started_ms: u64,
    /// Records still arriving, by run id.
    pending: BTreeMap<String, StartupRecord>,
    /// The newest finished record of each node and app.
    current: BTreeMap<RunKey, StartupRecord>,
    /// Records that ended without a `complete` line, kept for the report.
    failed: Vec<StartupRecord>,
    /// Records whose first line the journal tail cut. Counted, not shown.
    pub cut: u64,
    /// Shutdown lines whose start-up lines were never seen.
    pub orphan_shutdowns: u64,
}

impl Default for Startup {
    fn default() -> Startup {
        Startup::new()
    }
}

impl Startup {
    /// A reader for the records written from now on.
    pub fn new() -> Startup {
        Startup {
            started_ms: unix_ms(m6_core::platform::wall()),
            pending: BTreeMap::new(),
            current: BTreeMap::new(),
            failed: Vec::new(),
            cut: 0,
            orphan_shutdowns: 0,
        }
    }

    /// The monitor's start, in Unix milliseconds, for a test that writes a
    /// run id the monitor will judge.
    #[cfg(test)]
    pub fn started_ms(&self) -> u64 {
        self.started_ms
    }

    /// Read one JSON log line, when it is a start-up or shutdown line. `at` is
    /// the journal's recorded time for the entry that carried it, which tells
    /// the same entry read twice from a replay the node wrote.
    pub fn ingest(&mut self, line: &serde_json::Value, at: u64) {
        match line["type"].as_str() {
            Some("startup") => self.startup_line(line, at),
            Some("shutdown") => self.shutdown_line(line, at),
            _ => {}
        }
    }

    fn startup_line(&mut self, line: &serde_json::Value, at: u64) {
        let fields = &line["fields"];
        let Some(run_id) = fields["run_id"].as_str().filter(|r| !r.is_empty()) else {
            return;
        };
        let Some(part) = fields["part"].as_str() else {
            return;
        };
        let timestamp = line["timestamp"].as_str().unwrap_or_default();
        let entry = (at, fields["seq"].as_u64().unwrap_or(0));
        if !self.pending.contains_key(run_id) {
            // A finished record, or a replay of one, is not assembled again,
            // and a replayed line is still counted as ingested. An entry read
            // again by an overlapping read is not a replay.
            if let Some(record) = self.current.values_mut().find(|r| r.run_id == run_id) {
                if record.see(entry) {
                    record.ingested += 1;
                }
                return;
            }
            // A record that never closed was reported once, and is not
            // assembled again by the next read.
            if self.failed.iter().any(|r| r.run_id == run_id) {
                return;
            }
            if fields["seq"].as_u64() != Some(1) {
                // The tail cut the start of this record, so its own count and
                // start cannot be judged. Counted and not shown.
                self.cut += 1;
                return;
            }
            let start_ms = run_id
                .split_once('-')
                .and_then(|(ms, _)| ms.parse().ok())
                .unwrap_or(0);
            let started_at = timestamp.to_string();
            let first_seen = m6_core::platform::now();
            let record = StartupRecord::new(
                run_id,
                start_ms,
                started_at,
                first_seen,
                start_ms >= self.started_ms,
            );
            self.pending.insert(run_id.to_string(), record);
            self.bound_pending();
        }
        let Some(record) = self.pending.get_mut(run_id) else {
            return;
        };
        if !record.see(entry) {
            return;
        }
        record.lines += 1;
        record.ingested += 1;
        if let Some(app) = fields["app"].as_str().filter(|a| !a.is_empty()) {
            record.app = app.to_string();
        }
        if let Some(version) = fields["version"].as_str().filter(|v| !v.is_empty()) {
            record.version = version.to_string();
        }
        if let Some(node) = fields["node"].as_str().filter(|n| !n.is_empty()) {
            record.node = Some(node.to_string());
        }
        match part {
            "identity" => {
                record.tag = fields["tag"].as_str().map(str::to_string);
            }
            "config_file" => {
                if let (Some(role), Some(sha)) =
                    (fields["role"].as_str(), fields["sha256"].as_str())
                {
                    record.config.insert(role.to_string(), sha.to_string());
                    if let Some(path) = fields["path"].as_str() {
                        record
                            .config_paths
                            .insert(role.to_string(), path.to_string());
                    }
                }
            }
            "platform" => {
                record.host = Some(fields["host"].clone());
                record.decided = Some(fields["decided"].clone());
                record.hostname = known_str(&fields["host"], "hostname");
                record.process_index = fields["decided"]["process_index"].as_u64();
            }
            "complete" => {
                record.declared = fields["lines"].as_u64();
            }
            _ => {}
        }
        if part == "complete" {
            if let Some(record) = self.pending.remove(run_id) {
                self.finish_record(record);
            }
        }
    }

    fn shutdown_line(&mut self, line: &serde_json::Value, at: u64) {
        let fields = &line["fields"];
        let Some(run_id) = fields["run_id"].as_str().filter(|r| !r.is_empty()) else {
            return;
        };
        let shutdown = Shutdown {
            run_id: run_id.to_string(),
            app: fields["app"].as_str().unwrap_or_default().to_string(),
            version: fields["version"].as_str().unwrap_or_default().to_string(),
            uptime_ms: fields["uptime_ms"].as_u64().unwrap_or(0),
            reason: fields["reason"]
                .as_str()
                .unwrap_or("no reason given")
                .to_string(),
            at: line["timestamp"].as_str().unwrap_or_default().to_string(),
        };
        if let Some(mut record) = self.pending.remove(run_id) {
            // The run ended before its `complete` line: a record that never
            // closed, kept so the report can say so.
            record.ingested += 1;
            record.shutdown = Some(shutdown);
            self.push_failed(record);
            return;
        }
        if let Some(record) = self.current.values_mut().find(|r| r.run_id == run_id) {
            if record.see((at, u64::MAX)) {
                record.ingested += 1;
                record.shutdown = Some(shutdown);
            }
            return;
        }
        if self.failed.iter().any(|r| r.run_id == run_id) {
            return;
        }
        self.orphan_shutdowns += 1;
    }

    /// Move one closed record into the current table, keeping the run it
    /// replaced so a restart can be read from it.
    fn finish_record(&mut self, mut record: StartupRecord) {
        let key = record.key();
        if let Some(previous) = self.current.get(&key) {
            if record.start_ms <= previous.start_ms {
                // Older than the record already shown: a replay of the tail.
                return;
            }
            record.previous = Some(PreviousRun {
                run_id: previous.run_id.clone(),
                shutdown: previous.shutdown.clone(),
            });
        }
        self.current.insert(key, record);
        if self.current.len() > MAX_KEPT_RECORDS {
            if let Some(oldest) = self
                .current
                .values()
                .min_by_key(|r| r.start_ms)
                .map(|r| r.key())
            {
                self.current.remove(&oldest);
            }
        }
    }

    fn bound_pending(&mut self) {
        while self.pending.len() > MAX_PENDING_RECORDS {
            let Some(oldest) = self
                .pending
                .values()
                .min_by_key(|r| r.first_seen)
                .map(|r| r.run_id.clone())
            else {
                return;
            };
            self.pending.remove(&oldest);
        }
    }

    fn push_failed(&mut self, record: StartupRecord) {
        self.failed.push(record);
        if self.failed.len() > MAX_FAILED_RECORDS {
            self.failed.remove(0);
        }
    }

    /// Every record the report shows: the newest of each node and app, and the
    /// ones that never closed.
    ///
    /// A record still assembling past [`INCOMPLETE_AFTER`] is moved to the
    /// unfinished list, so a start-up that never finishes is reported once and
    /// not lost when the next poll starts.
    pub fn views(&mut self, now: Instant) -> Vec<StartupView> {
        let stale: Vec<String> = self
            .pending
            .values()
            .filter(|r| now.saturating_duration_since(r.first_seen) >= INCOMPLETE_AFTER)
            .map(|r| r.run_id.clone())
            .collect();
        for run_id in stale {
            if let Some(record) = self.pending.remove(&run_id) {
                if record.judged {
                    self.push_failed(record);
                } else {
                    // A run from before the monitor started never closed, and
                    // the tail can have cut its last lines as easily as its
                    // first. It is counted, not blamed.
                    self.cut += 1;
                }
            }
        }
        let mut out: Vec<StartupView> = self
            .current
            .values()
            .map(|record| record.view(true))
            .chain(self.failed.iter().map(|record| record.view(false)))
            .collect();
        out.sort_by(|a, b| {
            (&a.node, &a.app, a.process_index, &a.started_at).cmp(&(
                &b.node,
                &b.app,
                b.process_index,
                &b.started_at,
            ))
        });
        out
    }
}

/// The `known` value of a host fact, when the platform line reported one.
fn known_str(host: &serde_json::Value, key: &str) -> Option<String> {
    host.get(key)?.get("known")?.as_str().map(str::to_string)
}

/// A `SystemTime` in milliseconds since the Unix epoch.
fn unix_ms(time: std::time::SystemTime) -> u64 {
    time.duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// One document and the time the journal recorded for the line that carried it.
#[derive(Debug)]
struct Document {
    /// The entry's `__REALTIME_TIMESTAMP`, in microseconds since the Unix
    /// epoch. In a remote journal it is the uploading node's clock, so it
    /// orders one node's own entries and is never compared between nodes.
    at: u64,
    value: serde_json::Value,
}

/// The newest document of each kind the journal has carried, by node name, and
/// the start-up records it carried.
///
/// The four types share one `journalctl` read. The two documents are found on
/// `MESSAGE`'s `type`, and the two record types are handed to [`Startup`].
#[derive(Debug, Default)]
pub struct Latest {
    perf: BTreeMap<String, Document>,
    traffic: BTreeMap<String, Document>,
    /// The start-up and shutdown records, by run id and then by node and app.
    pub startup: Startup,
    /// Lines whose `MESSAGE` had no node name, or was not one of the two
    /// documents. Counted rather than guessed at.
    pub unattributed: u64,
    pub malformed: u64,
}

impl Latest {
    pub fn new() -> Latest {
        Latest::default()
    }

    /// Read one `journalctl --output=json` line and keep what it carries.
    pub fn ingest(&mut self, entry: &[u8]) {
        let Ok(entry) = serde_json::from_slice::<serde_json::Value>(entry) else {
            self.malformed += 1;
            return;
        };
        // The journal's own time for the entry, never a time inside MESSAGE,
        // which the node writes. An entry without it cannot be ordered.
        let Some(at) = entry["__REALTIME_TIMESTAMP"]
            .as_str()
            .and_then(|t| t.parse::<u64>().ok())
        else {
            self.malformed += 1;
            return;
        };
        let Some(message) = entry["MESSAGE"].as_str() else {
            self.malformed += 1;
            return;
        };
        let Ok(line) = serde_json::from_str::<serde_json::Value>(message) else {
            self.malformed += 1;
            return;
        };
        if matches!(line["type"].as_str(), Some("startup") | Some("shutdown")) {
            self.startup.ingest(&line, at);
            return;
        }
        let document = match line["type"].as_str() {
            Some("perf") => &line["fields"]["perf"],
            Some("traffic") => &line["fields"]["traffic"],
            _ => return,
        };
        let Some(node) = document["node"].as_str().filter(|n| !n.is_empty()) else {
            self.unattributed += 1;
            return;
        };
        // Keyed on the journal's time, not on the order this read delivered it
        // in. A later read can carry an older document, and it must not replace
        // the newer one already kept.
        match line["type"].as_str() {
            Some("perf") => keep_newest(&mut self.perf, node, at, document),
            Some("traffic") => keep_newest(&mut self.traffic, node, at, document),
            _ => {}
        }
    }

    /// The newest metrics document a node wrote, if the journal carried one.
    ///
    /// It is the JSON the line carried, parsed into the monitor's type by the
    /// poller, which is where an unreadable document is reported against the
    /// node it names.
    pub fn perf(&self, node: &str) -> Option<&serde_json::Value> {
        self.perf.get(node).map(|document| &document.value)
    }

    /// The newest traffic summary a node wrote, if the journal carried one.
    pub fn traffic(&self, node: &str) -> Option<&serde_json::Value> {
        self.traffic.get(node).map(|document| &document.value)
    }

    /// Nodes the journal has a metrics document for, in a test.
    #[cfg(test)]
    fn nodes_with_perf(&self) -> usize {
        self.perf.len()
    }

    /// Nodes the journal has a traffic summary for, in a test.
    #[cfg(test)]
    fn nodes_with_traffic(&self) -> usize {
        self.traffic.len()
    }
}

/// Keep `value` for `node` unless the kept document was recorded later.
///
/// An older entry read later leaves the newer one in place, which is what makes
/// reading an overlapping window on every poll safe: the same documents arrive
/// again in every read.
fn keep_newest(
    documents: &mut BTreeMap<String, Document>,
    node: &str,
    at: u64,
    value: &serde_json::Value,
) {
    if documents.get(node).is_none_or(|current| at >= current.at) {
        documents.insert(
            node.to_string(),
            Document {
                at,
                value: value.clone(),
            },
        );
    }
}

/// What one read pass found.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Event {
    /// The pipe has nothing more for now, or the child is gone and already
    /// retired.
    Idle,
    /// The child is gone. Deregister its descriptor, then call
    /// [`Reader::retire`].
    Exited,
}

/// The fewest lines a read asks for, so a small fleet still holds a start-up
/// record, which is 4 lines and the config files and sections.
const MIN_LINES: usize = 200;

/// Lines a read asks for per configured node: a node writes about 7 matching
/// lines a minute, so this is at least two minutes of the fleet.
const LINES_PER_NODE: usize = 14;

/// The `journalctl` child of one read, and the line being assembled.
///
/// A read starts a child that prints the last `--lines` entries and exits, and
/// its stdout is one more non-blocking descriptor in the loop. At most one read
/// starts per poll interval, a read still in flight is not started again, and
/// a read in flight past the interval is killed. [`Reader::start_if_idle`] and
/// [`Reader::expire`] are called on every pass of the loop and use the clock it
/// already has.
pub struct Reader {
    /// The journal directory. `None` for a reader that has none to read, which
    /// starts nothing.
    dir: Option<PathBuf>,
    /// The poll interval: the spacing of reads and the deadline of one.
    interval: Duration,
    child: Option<Child>,
    /// Whether `child` was killed, so its exit status is not the reason.
    killed: bool,
    stdout: Option<File>,
    /// When the last read was started, for the spacing and the deadline.
    last_started: Option<Instant>,
    /// The `--lines` count of each read.
    window: usize,
    /// The one line being assembled, bounded by a named limit.
    line: Bounded<u8>,
    /// A line past the limit is dropped, and its bytes are ignored until its
    /// newline arrives, so the next line starts clean.
    dropping: bool,
    /// Lines delivered, and lines dropped for their size.
    pub lines: u64,
    pub oversized: u64,
    /// Why the last read did not succeed, until one does.
    pub stopped: Option<String>,
}

impl Reader {
    /// A reader with nothing started, the one place its fields are set.
    fn new(
        dir: Option<PathBuf>,
        nodes: usize,
        interval: Duration,
        limit: &Limit,
        stopped: Option<String>,
    ) -> Reader {
        Reader {
            dir,
            interval,
            child: None,
            killed: false,
            stdout: None,
            last_started: None,
            window: MIN_LINES.max(LINES_PER_NODE * nodes),
            line: Bounded::<u8>::new(limit),
            dropping: false,
            lines: 0,
            oversized: 0,
            stopped,
        }
    }

    /// A reader with no directory, before the service or the check opens the
    /// journal.
    pub fn idle(limit: &Limit) -> Reader {
        Reader::new(None, 0, Duration::ZERO, limit, None)
    }

    /// A reader for a journal directory that could not be opened: it starts
    /// nothing on any tick, and `stopped` says why.
    pub fn unavailable(reason: String, limit: &Limit) -> Reader {
        Reader::new(None, 0, Duration::ZERO, limit, Some(reason))
    }

    /// Start the first read of `dir`. A child that cannot start is not fatal:
    /// the reason is kept in `stopped`, and the next interval tries again.
    ///
    /// `nodes` is the configured fleet size, which sets the `--lines` window.
    pub fn open(dir: &Path, nodes: usize, interval: Duration, limit: &Limit) -> Reader {
        let mut reader = Reader::new(Some(dir.to_path_buf()), nodes, interval, limit, None);
        reader.start(m6_core::platform::now());
        reader
    }

    /// One run for `--check`: the same read, and [`Reader::drain`] reads the
    /// pipe to its end.
    pub fn once(dir: &Path, nodes: usize, limit: &Limit) -> Reader {
        Reader::open(dir, nodes, Duration::ZERO, limit)
    }

    /// Start a read when none is in flight and an interval has passed since
    /// the last one started, and say whether it started.
    ///
    /// The loop calls this on every pass. A read in flight is not doubled, and
    /// a child that cannot start is tried again after the interval, so a
    /// missing `journalctl` is not run on every pass.
    pub fn start_if_idle(&mut self, now: Instant) -> bool {
        self.settle();
        if self.dir.is_none() || self.child.is_some() || self.stdout.is_some() {
            return false;
        }
        if let Some(last) = self.last_started {
            if now.saturating_duration_since(last) < self.interval {
                return false;
            }
        }
        self.start(now);
        self.stdout.is_some()
    }

    /// Kill a read that has run past one interval, and say whether it did. The
    /// caller then deregisters the descriptor and calls [`Reader::retire`].
    pub fn expire(&mut self, now: Instant) -> bool {
        let Some(last) = self.last_started else {
            return false;
        };
        let running = self.child.is_some() || self.stdout.is_some();
        if !running || now.saturating_duration_since(last) < self.interval {
            return false;
        }
        self.kill(format!(
            "journalctl was still reading after {} s and was killed",
            self.interval.as_secs()
        ));
        true
    }

    /// Read a one-shot run to its end, handing every line to `each`.
    pub fn drain(&mut self, mut each: impl FnMut(&[u8])) {
        // A one-shot tool may wait, so its pipe is blocking here. The service
        // leaves the pipe non-blocking for its loop.
        let set = self
            .stdout
            .as_ref()
            .map(|stdout| m6_core::platform::fd::set_nonblocking(stdout, false));
        if let Some(Err(e)) = set {
            self.kill(format!("the pipe could not be made blocking: {e}"));
            self.retire();
            return;
        }
        while self.stdout.is_some() {
            if self.read(&mut each) == Event::Exited {
                self.retire();
            }
        }
        if let Some(child) = self.child.as_mut() {
            let status = child.wait();
            self.conclude(status);
        }
    }

    /// The descriptor to watch for readability, while there is one.
    pub fn fd(&self) -> Option<std::os::fd::RawFd> {
        use std::os::fd::AsRawFd;
        self.stdout.as_ref().map(AsRawFd::as_raw_fd)
    }

    /// Read what the pipe holds, handing every complete line to `each`.
    ///
    /// [`Event::Exited`] means the pipe has ended and its descriptor must be
    /// removed from the reactor before [`Reader::retire`] closes it: a closed
    /// descriptor number can be given to the next socket, and deregistering it
    /// then would remove that socket instead.
    pub fn read(&mut self, mut each: impl FnMut(&[u8])) -> Event {
        let mut chunk = [0u8; 8 * 1024];
        loop {
            let Some(stdout) = self.stdout.as_mut() else {
                return Event::Idle;
            };
            let read = m6_core::platform::fd::read(stdout, &mut chunk);
            let count = match read {
                Ok(0) => return Event::Exited,
                Ok(count) => count,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return Event::Idle,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    self.kill(format!("read error: {e}"));
                    return Event::Exited;
                }
            };
            let mut start = 0;
            for i in 0..count {
                if chunk[i] != b'\n' {
                    continue;
                }
                self.push(&chunk[start..i]);
                if !self.dropping {
                    self.lines += 1;
                    each(self.line.as_bytes());
                    self.line.clear();
                }
                self.dropping = false;
                start = i + 1;
            }
            self.push(&chunk[start..count]);
        }
    }

    /// Close the pipe, after the reactor has let it go, and reap the child if
    /// it has exited. A child still exiting is reaped by a later
    /// [`Reader::start_if_idle`], so the loop never waits for it.
    pub fn retire(&mut self) {
        self.stdout = None;
        self.line.clear();
        self.dropping = false;
        self.settle();
    }

    /// Reap the child without waiting, and record how its read ended.
    fn settle(&mut self) {
        if self.stdout.is_some() {
            return;
        }
        let Some(child) = self.child.as_mut() else {
            return;
        };
        match child.try_wait() {
            Ok(Some(status)) => self.conclude(Ok(status)),
            Ok(None) => {}
            Err(e) => self.conclude(Err(e)),
        }
    }

    /// The child is gone. A read that was not killed succeeded when journalctl
    /// exited with success, and any other exit, such as a directory it could
    /// not read, is the reason.
    fn conclude(&mut self, status: std::io::Result<ExitStatus>) {
        self.child = None;
        if std::mem::take(&mut self.killed) {
            return;
        }
        self.stopped = match status {
            Ok(status) if status.success() => None,
            Ok(status) => Some(format!("journalctl {status}")),
            Err(e) => Some(format!("journalctl could not be waited for: {e}")),
        };
    }

    /// Append part of the line being assembled.
    ///
    /// Past the limit the line is dropped and counted, and its remaining bytes
    /// are ignored until its newline, so the next line starts clean.
    fn push(&mut self, bytes: &[u8]) {
        if self.dropping || bytes.is_empty() {
            return;
        }
        if self.line.write_all(bytes).is_err() {
            self.oversized += 1;
            self.dropping = true;
            self.line.clear();
        }
    }

    /// The one `journalctl` command: the last `window` entries of the four
    /// types it reads, all on one child.
    ///
    /// The count reads no clock of the monitor's, but the newest entries are
    /// chosen by the senders' clocks, so a node whose clock lags past the window
    /// is not read. It is sized in [`MIN_LINES`] and [`LINES_PER_NODE`], and
    /// covers about two minutes of the fleet when every node writes at the same
    /// rate. The read does not follow: a follower over a remote
    /// journal that two hosts upload into shows one host's entries and never the
    /// other's.
    fn command(&self) -> Command {
        let mut command = Command::new("journalctl");
        command
            .arg(format!("--lines={}", self.window))
            // JSON output prints a field null when its name and value reach 4096 bytes
            // together, which for MESSAGE is a value of 4088. A metrics document is about
            // 6.4 KB, so every one of them would arrive null. A null is no MESSAGE, which this
            // reader counts as malformed and drops, so the digest then reports
            // that the node has no metrics line at all.
            .arg("--all")
            .arg("--output=json")
            .arg("--directory")
            .arg(self.dir.as_deref().unwrap_or(Path::new("")))
            .arg("M6_TYPE=perf")
            .arg("M6_TYPE=traffic")
            .arg("M6_TYPE=startup")
            .arg("M6_TYPE=shutdown");
        command
    }

    /// Start the read's child.
    fn start(&mut self, now: Instant) {
        self.last_started = Some(now);
        let mut command = self.command();
        // Only stdout is read. Stderr goes nowhere, as a reader that stops
        // draining it would otherwise stall journalctl.
        let streams = Streams {
            stdin: Io::Null,
            stdout: Io::Piped,
            stderr: Io::Null,
        };
        match m6_core::platform::process::spawn(&mut command, streams) {
            Ok(mut child) => {
                let stdout = child.stdout.take();
                // The loop must never wait on the pipe.
                let ready =
                    stdout
                        .as_ref()
                        .map_or(Err("journalctl has no stdout".to_string()), |stdout| {
                            m6_core::platform::fd::set_nonblocking(stdout, true).map_err(|e| {
                                format!("the pipe could not be made non-blocking: {e}")
                            })
                        });
                self.child = Some(child);
                match ready {
                    Ok(_) => self.stdout = stdout,
                    Err(reason) => self.kill(reason),
                }
            }
            Err(e) => self.stopped = Some(format!("journalctl could not start: {e}")),
        }
    }

    /// Record why the read failed and kill its child. The pipe stays open: the
    /// loop deregisters its descriptor first and [`Reader::retire`] closes it,
    /// so the number cannot be reused by a socket that the deregistration
    /// would then remove. The dead child is reaped without waiting.
    fn kill(&mut self, reason: String) {
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
            self.killed = true;
        }
        self.stopped = Some(reason);
    }
}

impl Drop for Reader {
    fn drop(&mut self) {
        self.stdout = None;
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// A journal time for a test entry that does not care which, later than every
/// one handed out before it. Shared by the tests of this crate.
#[cfg(test)]
pub fn next_realtime() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 =
        std::sync::atomic::AtomicU64::new(1_763_000_000_000_000);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One entry as `journalctl --output=json` prints it, recorded at `at`.
    fn line_at(value: serde_json::Value, at: u64) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "MESSAGE": value.to_string(),
            "M6_TYPE": value["type"],
            "_HOSTNAME": "build",
            "__REALTIME_TIMESTAMP": at.to_string(),
        }))
        .unwrap()
    }

    fn line(value: serde_json::Value) -> Vec<u8> {
        line_at(value, next_realtime())
    }

    fn document_at(kind: &str, node: &str, at: &str) -> serde_json::Value {
        match kind {
            "perf" => serde_json::json!({
                "timestamp": at,
                "type": "perf",
                "fields": {"perf": {"node": node, "uptime_s": 5}},
            }),
            _ => serde_json::json!({
                "timestamp": at,
                "type": "traffic",
                "fields": {"traffic": {"node": node, "window_minutes": 60}},
            }),
        }
    }

    fn document(kind: &str, node: &str) -> serde_json::Value {
        document_at(kind, node, "2026-11-20T00:00:00.000000Z")
    }

    /// Three nodes on one host are three nodes, because the document names
    /// each one and `_HOSTNAME` names none of them.
    #[test]
    fn three_nodes_on_one_host_are_three_nodes() {
        let mut latest = Latest::new();
        for node in ["origin", "edge-a", "edge-b"] {
            latest.ingest(&line(document("perf", node)));
            latest.ingest(&line(document("traffic", node)));
        }
        assert_eq!(latest.nodes_with_perf(), 3);
        assert_eq!(latest.nodes_with_traffic(), 3);
        assert!(latest.perf("edge-b").is_some(), "each node has its own");
        assert!(latest.traffic("edge-a").is_some());
    }

    fn perf_with(node: &str, uptime_s: u64, message_time: &str) -> serde_json::Value {
        let mut value = document_at("perf", node, message_time);
        value["fields"]["perf"]["uptime_s"] = serde_json::json!(uptime_s);
        value
    }

    #[test]
    fn the_newest_document_wins() {
        let mut latest = Latest::new();
        latest.ingest(&line_at(perf_with("origin", 5, "t"), 1_000));
        latest.ingest(&line_at(perf_with("origin", 9, "t"), 2_000));
        assert_eq!(latest.perf("origin").expect("a document")["uptime_s"], 9);
        assert_eq!(latest.nodes_with_perf(), 1);
    }

    /// A later read can carry an older entry: the uploader delivered it out of
    /// order, or the window overlaps the last one. The newer document already
    /// kept must stay.
    #[test]
    fn an_older_entry_read_later_does_not_replace_a_newer_one() {
        let mut latest = Latest::new();
        latest.ingest(&line_at(perf_with("origin", 9, "t"), 9_000));
        latest.ingest(&line_at(perf_with("origin", 1, "t"), 5_000));
        assert_eq!(
            latest.perf("origin").expect("a document")["uptime_s"],
            9,
            "the older entry read later replaced the newer one"
        );
    }

    /// The order is the journal's time. A node writes the timestamp inside
    /// MESSAGE, so a forged far-future one must not win over a later genuine
    /// entry.
    #[test]
    fn a_timestamp_inside_message_does_not_order_entries() {
        let mut latest = Latest::new();
        let forged = perf_with("origin", 666, "9999-12-31T23:59:59.999999Z");
        latest.ingest(&line_at(forged, 5_000));
        latest.ingest(&line_at(
            perf_with("origin", 7, "2026-11-20T00:00:00.000000Z"),
            6_000,
        ));
        assert_eq!(latest.perf("origin").expect("a document")["uptime_s"], 7);
    }

    /// An entry with no journal time cannot be ordered, so it is counted and
    /// kept nowhere.
    #[test]
    fn an_entry_without_a_journal_time_is_malformed() {
        let mut latest = Latest::new();
        let message = perf_with("origin", 1, "t").to_string();
        latest.ingest(&serde_json::to_vec(&serde_json::json!({"MESSAGE": message})).unwrap());
        latest.ingest(
            &serde_json::to_vec(
                &serde_json::json!({"MESSAGE": message, "__REALTIME_TIMESTAMP": "soon"}),
            )
            .unwrap(),
        );
        assert_eq!(latest.malformed, 2);
        assert_eq!(latest.nodes_with_perf(), 0);
    }

    /// One read carries both nodes: their entries interleave and arrive out of
    /// order, and each node keeps its own newest.
    #[test]
    fn two_nodes_documents_interleaved_out_of_order_both_reach_latest() {
        let mut latest = Latest::new();
        latest.ingest(&line_at(perf_with("edge-a", 12, "t"), 4_000));
        latest.ingest(&line_at(perf_with("edge-b", 21, "t"), 1_000));
        latest.ingest(&line_at(perf_with("edge-b", 22, "t"), 5_000));
        latest.ingest(&line_at(perf_with("edge-a", 11, "t"), 2_000));
        latest.ingest(&line_at(document_at("traffic", "edge-b", "t"), 3_000));
        latest.ingest(&line_at(document_at("traffic", "edge-a", "t"), 3_500));

        assert_eq!(latest.nodes_with_perf(), 2, "both nodes' metrics");
        assert_eq!(latest.nodes_with_traffic(), 2, "both nodes' traffic");
        assert_eq!(latest.perf("edge-a").expect("a document")["uptime_s"], 12);
        assert_eq!(latest.perf("edge-b").expect("a document")["uptime_s"], 22);
    }

    #[test]
    fn a_line_with_no_node_is_counted_not_guessed() {
        let mut latest = Latest::new();
        let mut nameless = document("perf", "");
        nameless["fields"]["perf"]["node"] = serde_json::json!("");
        latest.ingest(&line(nameless));
        assert_eq!(latest.unattributed, 1);
        assert_eq!(latest.nodes_with_perf(), 0);
    }

    #[test]
    fn a_line_that_is_not_one_of_the_two_documents_is_ignored() {
        let mut latest = Latest::new();
        latest.ingest(&line(
            serde_json::json!({"type": "event", "fields": {"message": "hi"}}),
        ));
        latest.ingest(b"not json");
        assert_eq!(latest.nodes_with_perf(), 0);
        assert_eq!(latest.malformed, 1, "only the unparseable entry counts");
        assert_eq!(latest.unattributed, 0);
    }

    /// A read that exits leaves its pipe open until `retire`, so the loop can
    /// deregister the descriptor before the number is free for reuse.
    #[test]
    fn a_finished_read_keeps_its_descriptor_until_retire() {
        let mut reader = running("printf 'one\\n'; exit 3", Duration::from_secs(30));
        let fd = reader.fd().expect("the pipe");
        let mut lines = 0;
        while reader.read(|_| lines += 1) != Event::Exited {}
        assert_eq!(lines, 1);
        assert_eq!(reader.fd(), Some(fd), "the descriptor is still open");
        reader.retire();
        assert_eq!(reader.fd(), None, "retire closes it");
        reaped(&mut reader);
        let reason = reader.stopped.clone().expect("the reason is recorded");
        assert!(reason.contains('3'), "{reason}");
    }

    /// A reader with a read in flight, whose child runs `script` in place of
    /// journalctl.
    fn running(script: &str, interval: Duration) -> Reader {
        let mut reader = Reader::new(
            Some(PathBuf::from("/var/empty/journal")),
            1,
            interval,
            &Limit::builtin(&m6_core::limits::JOURNAL_LINE),
            None,
        );
        let mut child = m6_core::platform::process::spawn(
            Command::new("sh").args(["-c", script]),
            Streams {
                stdin: Io::Null,
                stdout: Io::Piped,
                stderr: Io::Null,
            },
        )
        .expect("a child");
        reader.stdout = child.stdout.take();
        reader.child = Some(child);
        reader
    }

    /// Reap the child: without waiting first, as the loop does, and then by
    /// waiting, as a test may.
    fn reaped(reader: &mut Reader) {
        reader.settle();
        if let Some(child) = reader.child.as_mut() {
            let status = child.wait();
            reader.conclude(status);
        }
        assert!(reader.child.is_none(), "the child was not reaped");
    }

    /// A read already in flight at the next tick is not started again.
    #[test]
    fn a_read_in_flight_is_not_started_again() {
        let mut reader = running("printf 'one\\n'", Duration::from_secs(30));
        let fd = reader.fd().expect("the pipe");
        let started = reader.last_started;
        assert!(
            !reader.start_if_idle(m6_core::platform::now() + Duration::from_secs(60)),
            "a child in flight is not doubled"
        );
        assert_eq!(reader.last_started, started, "no second child was started");
        assert_eq!(reader.fd(), Some(fd), "the same pipe");
        assert_eq!(reader.stopped, None);
        reader.retire();
    }

    /// `--check` reads the journal once. The child printing its tail and
    /// exiting is the normal end, and once read as "not running", so every
    /// `--check` reported the reader unavailable.
    #[test]
    fn a_one_shot_read_that_ends_normally_is_not_unavailable() {
        let mut reader = running("printf 'one\\ntwo\\n'", Duration::ZERO);
        let mut lines = 0;
        reader.drain(|_| lines += 1);
        assert_eq!(lines, 2);
        assert_eq!(reader.stopped, None);

        let mut failed = running("exit 3", Duration::ZERO);
        failed.drain(|_| {});
        let reason = failed.stopped.clone().expect("a failed run is the reason");
        assert!(reason.contains('3'), "{reason}");
    }

    /// The read does not follow, has no `--since`, and carries `--lines=N` as
    /// one argument: three nodes sit under the floor of 200.
    #[test]
    fn the_one_read_has_all_four_types_lines_and_does_not_follow() {
        let reader = Reader::new(
            Some(PathBuf::from("/var/empty/journal")),
            3,
            Duration::from_secs(30),
            &Limit::builtin(&m6_core::limits::JOURNAL_LINE),
            None,
        );
        let command = reader.command();
        let args: Vec<std::ffi::OsString> = command.get_args().map(|a| a.to_os_string()).collect();
        let types: Vec<String> = args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .filter(|a| a.starts_with("M6_TYPE="))
            .collect();
        assert_eq!(
            types,
            [
                "M6_TYPE=perf",
                "M6_TYPE=traffic",
                "M6_TYPE=startup",
                "M6_TYPE=shutdown",
            ],
            "four types on one child"
        );
        let text: Vec<String> = args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(
            text.iter().all(|a| a != "--follow" && !a.starts_with("-f")),
            "a read prints its window and exits: {text:?}"
        );
        assert!(
            text.iter().all(|a| !a.starts_with("--since")),
            "the window reads no clock: {text:?}"
        );
        assert_eq!(
            text.iter().filter(|a| *a == "--lines=200").count(),
            1,
            "one --lines, as one argument: {text:?}"
        );
        assert_eq!(
            args.iter().filter(|a| *a == "--directory").count(),
            1,
            "one journal directory"
        );
        // JSON output nulls a field whose name and value reach 4096 bytes together
        // without this, which for MESSAGE is 4088 bytes of value, and a metrics
        // document is about 6.4 KB.
        assert_eq!(
            args.iter().filter(|a| *a == "--all").count(),
            1,
            "one --all, so long fields are not nulled"
        );
    }

    /// A read killed at its deadline does not poison the next one: its child
    /// is reaped, its pipe is closed, and the next read starts at once.
    #[test]
    fn the_read_after_a_killed_one_starts_normally() {
        let mut reader = running("sleep 30", Duration::from_secs(30));
        let began = m6_core::platform::now();
        reader.last_started = Some(began);
        assert!(reader.expire(began + Duration::from_secs(30)));
        reader.retire();
        reaped(&mut reader);
        assert!(reader.child.is_none() && reader.stdout.is_none());
        let next = began + Duration::from_secs(31);
        reader.start_if_idle(next);
        assert_eq!(reader.last_started, Some(next), "the next read was started");
    }

    /// A reader with no journal directory runs nothing on any tick, and keeps
    /// the reason it was given.
    #[test]
    fn the_unavailable_reader_does_not_exec() {
        let reason = "create it, or point the key at the directory".to_string();
        let mut reader = Reader::unavailable(
            reason.clone(),
            &Limit::builtin(&m6_core::limits::JOURNAL_LINE),
        );
        let now = m6_core::platform::now();
        for tick in 0..5 {
            assert!(!reader.start_if_idle(now + Duration::from_secs(60 * tick)));
        }
        assert!(reader.child.is_none() && reader.last_started.is_none());
        assert_eq!(reader.stopped.as_deref(), Some(reason.as_str()));
    }

    /// At most one read starts per interval, so a failing `journalctl` is
    /// not run on every pass of the loop.
    #[test]
    fn a_read_starts_at_most_once_per_interval() {
        let mut reader = running("exit 0", Duration::from_secs(30));
        reader.drain(|_| {});
        let before = m6_core::platform::now();
        reader.last_started = Some(before);
        assert!(!reader.start_if_idle(before + Duration::from_secs(29)));
        assert_eq!(reader.last_started, Some(before), "nothing was started");
        assert_eq!(reader.stopped, None);
    }

    /// A read still running one interval after it started is killed and
    /// reported, its child is reaped without a blocking wait, and the pipe is
    /// closed.
    #[test]
    fn a_stuck_read_past_its_deadline_is_killed() {
        let mut reader = running("sleep 30", Duration::from_secs(30));
        let began = m6_core::platform::now();
        reader.last_started = Some(began);
        let pid = reader.child.as_ref().expect("a child").id();
        assert!(
            !reader.expire(began + Duration::from_secs(29)),
            "not due yet"
        );
        assert!(reader.expire(began + Duration::from_secs(30)), "overran");
        let reason = reader.stopped.clone().expect("the overrun is reported");
        assert!(reason.contains("killed"), "{reason}");
        reader.retire();
        assert_eq!(reader.fd(), None);
        reaped(&mut reader);
        assert!(
            !m6_core::platform::process::is_alive(pid),
            "the child is gone"
        );
        assert_eq!(reader.stopped.as_deref(), Some(reason.as_str()));
    }

    #[test]
    fn an_unreadable_entry_is_counted() {
        let mut latest = Latest::new();
        latest.ingest(b"{\"MESSAGE\":\"not json\"}");
        assert_eq!(latest.malformed, 1);
    }

    /// The reader side of the null: a metrics document of the size a real one is must
    /// arrive whole and be retained, and a null MESSAGE must be counted rather than read.
    /// The argument itself is covered by `the_one_read_has_all_four_types_lines_and_does_not_follow`,
    /// which fails if `--all` is removed; this test does not construct the command.
    #[test]
    fn a_metrics_document_over_4096_bytes_is_read_whole() {
        let fields = "x".repeat(6_400);
        let document = format!("{{\"node\":\"edge-a\",\"pad\":\"{fields}\"}}");
        let metrics = format!("{{\"type\":\"perf\",\"fields\":{{\"perf\":{document}}}}}");
        let line = format!(
            "{{\"MESSAGE\":{},\"__REALTIME_TIMESTAMP\":\"1763000000000000\"}}",
            serde_json::json!(metrics)
        );
        let mut latest = Latest::new();
        latest.ingest(line.as_bytes());
        assert_eq!(latest.malformed, 0, "a 6.4 KB document is not malformed");
        assert!(
            latest.perf.contains_key("edge-a"),
            "the document's node was not retained: {:?}",
            latest.perf.keys().collect::<Vec<_>>()
        );

        // The same entry with the field nulled, which is what a missing `--all`
        // delivers: counted, not silently dropped.
        let mut nulled = Latest::new();
        nulled.ingest(b"{\"MESSAGE\":null}");
        assert_eq!(nulled.malformed, 1);
    }

    /// One start-up line as the writer emits it, inside the journal's JSON.
    fn startup(run_id: &str, seq: u64, part: &str) -> serde_json::Value {
        serde_json::json!({
            "timestamp": "2026-11-18T02:16:20.858965Z",
            "level": "INFO",
            "fields": {
                "run_id": run_id,
                "seq": seq,
                "part": part,
                "app": "m6-http",
                "version": "1.16.0+abc123",
                "message": format!("startup record: {part}"),
            },
            "type": "startup",
        })
    }

    /// One record, grouped by its run id, attributed by its own platform line
    /// and not by `_HOSTNAME`, and closed by its own count.
    #[test]
    fn a_record_is_grouped_by_run_id_and_attributed_by_its_platform_line() {
        let mut latest = Latest::new();
        let run_id = format!("{}-81", latest.startup.started_ms() + 1_000);
        latest.ingest(&line(startup(&run_id, 1, "identity")));
        let config = startup(&run_id, 2, "config_file");
        let config = serde_json::json!({
            "timestamp": config["timestamp"],
            "level": "INFO",
            "fields": {
                "run_id": run_id,
                "seq": 2,
                "part": "config_file",
                "app": "m6-http",
                "version": "1.16.0+abc123",
                "role": "config",
                "path": "/etc/m6/m6-http.conf",
                "sha256": "aa11",
                "message": "startup record: config file",
            },
            "type": "startup",
        });
        latest.ingest(&line(config));
        let platform = serde_json::json!({
            "timestamp": "2026-11-18T02:16:20.859000Z",
            "level": "INFO",
            "fields": {
                "run_id": run_id,
                "seq": 3,
                "part": "platform",
                "app": "m6-http",
                "version": "1.16.0+abc123",
                "host": {
                    "hostname": {"known": "edge-a"},
                    "kernel": {"known": {"release": "6.11.0-1", "version": "#1"}},
                },
                "decided": {"socket_path": "/run/m6/m6-http.sock"},
                "message": "startup record: platform",
            },
            "type": "startup",
        });
        latest.ingest(&line(platform));
        let complete = startup(&run_id, 4, "complete");
        let complete = serde_json::json!({
            "timestamp": complete["timestamp"],
            "level": "INFO",
            "fields": {
                "run_id": run_id,
                "seq": 4,
                "part": "complete",
                "app": "m6-http",
                "version": "1.16.0+abc123",
                "lines": 4,
                "message": "startup record: complete",
            },
            "type": "startup",
        });
        latest.ingest(&line(complete));
        let views = latest.startup.views(m6_core::platform::now());
        assert_eq!(views.len(), 1, "one record, one run id");
        let run = &views[0];
        assert_eq!(run.node, "edge-a", "the platform's hostname names the node");
        assert_eq!(run.app, "m6-http");
        assert_eq!(run.complete, Some(4));
        assert_eq!(run.lines, 4);
        assert_eq!(run.config.get("config").map(String::as_str), Some("aa11"));

        let shutdown = serde_json::json!({
            "timestamp": "2026-11-18T02:17:20.858965Z",
            "level": "INFO",
            "fields": {
                "run_id": run_id,
                "app": "m6-http",
                "version": "1.16.0+abc123",
                "uptime_ms": 60_000,
                "reason": "signal",
                "message": "shutdown",
            },
            "type": "shutdown",
        });
        latest.ingest(&line(shutdown));
        let views = latest.startup.views(m6_core::platform::now());
        assert_eq!(
            views[0].shutdown.as_ref().map(|s| s.reason.as_str()),
            Some("signal"),
            "the shutdown attaches to the same run"
        );
    }

    /// Reads overlap, so a record whose lines span two reads arrives twice in
    /// part. Each entry counts once: counted twice, the record would hold six
    /// lines against the four its `complete` line declares, and the check would
    /// raise a fault on a healthy start.
    #[test]
    fn a_record_split_across_two_overlapping_reads_counts_once() {
        let mut latest = Latest::new();
        let run_id = format!("{}-83", latest.startup.started_ms() + 1_000);
        let at = 1_763_000_000_000_000u64;
        let entries: Vec<Vec<u8>> = [
            (1, "identity"),
            (2, "config_file"),
            (3, "platform"),
            (4, "complete"),
        ]
        .into_iter()
        .map(|(seq, part)| {
            let mut value = startup(&run_id, seq, part);
            value["fields"]["lines"] = serde_json::json!(4);
            line_at(value, at + seq)
        })
        .collect();
        // The first read ends partway through the record, and the next read's
        // window reaches back over what the first one carried.
        for entry in &entries[..2] {
            latest.ingest(entry);
        }
        for entry in &entries {
            latest.ingest(entry);
        }
        let views = latest.startup.views(m6_core::platform::now());
        assert_eq!(views.len(), 1);
        assert_eq!(views[0].lines, 4, "each line counted once");
        assert_eq!(views[0].complete, Some(4));
        assert_eq!(views[0].ingested, 4);

        // Another overlapping read of the finished record changes nothing.
        for entry in &entries {
            latest.ingest(entry);
        }
        let views = latest.startup.views(m6_core::platform::now());
        assert_eq!((views[0].lines, views[0].ingested), (4, 4));
        assert_eq!(latest.startup.cut, 0);
    }

    /// The record's own `node` field names the node when a line carries one.
    /// The platform line's hostname is the fallback for a record written
    /// before the field existed, and is kept for the peer comparison.
    #[test]
    fn a_node_field_the_record_carries_wins_over_the_platform_hostname() {
        let mut latest = Latest::new();
        let run_id = format!("{}-82", latest.startup.started_ms() + 1_000);
        let mut identity = startup(&run_id, 1, "identity");
        identity["fields"]["node"] = serde_json::json!("edge-a");
        latest.ingest(&line(identity));
        let mut platform = startup(&run_id, 2, "platform");
        platform["fields"]["node"] = serde_json::json!("edge-a");
        platform["fields"]["host"] = serde_json::json!({"hostname": {"known": "host-1"}});
        platform["fields"]["decided"] = serde_json::json!({});
        latest.ingest(&line(platform));
        let mut complete = startup(&run_id, 3, "complete");
        complete["fields"]["node"] = serde_json::json!("edge-a");
        complete["fields"]["lines"] = serde_json::json!(3);
        latest.ingest(&line(complete));

        let views = latest.startup.views(m6_core::platform::now());
        assert_eq!(views.len(), 1, "one record, one run id");
        assert_eq!(
            views[0].node, "edge-a",
            "the record's own node field wins over the platform hostname"
        );
        assert_eq!(
            views[0].hostname.as_deref(),
            Some("host-1"),
            "the platform line's hostname is still kept for the peer comparison"
        );
    }
}
