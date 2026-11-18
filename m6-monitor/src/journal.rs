//! The monitor's read of mon's journal.
//!
//! Every node streams its journal to mon with `systemd-journal-upload`, and mon
//! keeps it with `systemd-journal-remote` under a directory of per-peer files.
//! The metrics and traffic lines are in it, so the monitor reads them from
//! there instead of polling an endpoint: one `journalctl --follow` child at
//! start, its stdout one more descriptor in the existing loop, and never one
//! `journalctl` per poll.
//!
//! **A line is attributed by the `node` field inside `MESSAGE`.** That is the
//! name the node's own `site.toml` gives it, and every m6-http line carries it.
//! `_HOSTNAME` is not used at all, because a host that runs several nodes
//! writes one `_HOSTNAME` for all of them: three nodes on one host would
//! otherwise be one node, and a digest that silently merges three nodes looks
//! exactly like a digest of one. A line whose document has no node name is
//! counted and dropped rather than guessed at.
//!
//! # The start-up record
//!
//! `M6_TYPE=startup` and `M6_TYPE=shutdown` are read on the same child as the
//! two documents, so the monitor follows four types with one `journalctl`
//! process. A start-up record is several lines sharing one `run_id`, so this
//! module groups them and [`Startup`] holds the newest record of each node and
//! app. The node is the record's own `node` field when it carries one, and the
//! machine's hostname from its platform line otherwise. The app is the record's
//! own `app` field. `_HOSTNAME` is never read for either.
//!
//! A record whose first line the `--lines=200` tail cut cannot be judged, so it
//! is counted in [`Startup::cut`] and not shown. A record that started before
//! the monitor did is shown as the current run and is not judged for its
//! completeness or its restarts, because the tail can cut its last lines as
//! easily as its first.
//!
//! The child is restarted on a backoff when it stops, and it is killed at
//! stop, so no `journalctl --follow` outlives the monitor.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

use m6_core::limits::{Bounded, Limit};

/// The longest the monitor waits before starting the reader again.
const MAX_BACKOFF: Duration = Duration::from_secs(30);

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
    /// The `complete` line's count, when it arrived.
    pub declared: Option<u64>,
    /// The shutdown line, when the run wrote one.
    pub shutdown: Option<Shutdown>,
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
            declared: None,
            shutdown: None,
            first_seen,
            previous: None,
            judged,
        }
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

    /// Read one JSON log line, when it is a start-up or shutdown line.
    pub fn ingest(&mut self, line: &serde_json::Value) {
        match line["type"].as_str() {
            Some("startup") => self.startup_line(line),
            Some("shutdown") => self.shutdown_line(line),
            _ => {}
        }
    }

    fn startup_line(&mut self, line: &serde_json::Value) {
        let fields = &line["fields"];
        let Some(run_id) = fields["run_id"].as_str().filter(|r| !r.is_empty()) else {
            return;
        };
        let Some(part) = fields["part"].as_str() else {
            return;
        };
        let timestamp = line["timestamp"].as_str().unwrap_or_default();
        if !self.pending.contains_key(run_id) {
            // A finished record, or a replay of one, is not assembled again.
            if self.current.values().any(|r| r.run_id == run_id) {
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
        record.lines += 1;
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

    fn shutdown_line(&mut self, line: &serde_json::Value) {
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
            record.shutdown = Some(shutdown);
            self.push_failed(record);
            return;
        }
        if let Some(record) = self.current.values_mut().find(|r| r.run_id == run_id) {
            record.shutdown = Some(shutdown);
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

/// The newest document of each kind the journal has carried, by node name, and
/// the start-up records it carried.
///
/// The four types share one `journalctl --follow` child. The two documents are
/// found on `MESSAGE`'s `type`, and the two record types are handed to
/// [`Startup`].
#[derive(Debug, Default)]
pub struct Latest {
    perf: BTreeMap<String, serde_json::Value>,
    traffic: BTreeMap<String, serde_json::Value>,
    /// The start-up and shutdown records, by run id and then by node and app.
    pub startup: Startup,
    /// Lines whose `MESSAGE` had no node name, or was not one of the two
    /// documents. Counted rather than guessed at.
    pub unattributed: u64,
    pub malformed: u64,
    /// Why the reader is not running, when it is not.
    pub unavailable: Option<String>,
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
        let Some(message) = entry["MESSAGE"].as_str() else {
            self.malformed += 1;
            return;
        };
        let Ok(line) = serde_json::from_str::<serde_json::Value>(message) else {
            self.malformed += 1;
            return;
        };
        if matches!(line["type"].as_str(), Some("startup") | Some("shutdown")) {
            self.startup.ingest(&line);
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
        match line["type"].as_str() {
            Some("perf") => {
                self.perf.insert(node.to_string(), document.clone());
            }
            Some("traffic") => {
                self.traffic.insert(node.to_string(), document.clone());
            }
            _ => {}
        }
    }

    /// The newest metrics document a node wrote, if the journal carried one.
    ///
    /// It is the JSON the line carried, parsed into the monitor's type by the
    /// poller, which is where an unreadable document is reported against the
    /// node it names.
    pub fn perf(&self, node: &str) -> Option<&serde_json::Value> {
        self.perf.get(node)
    }

    /// The newest traffic summary a node wrote, if the journal carried one.
    pub fn traffic(&self, node: &str) -> Option<&serde_json::Value> {
        self.traffic.get(node)
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

/// The `journalctl --follow` child and the line being assembled.
pub struct Reader {
    dir: PathBuf,
    /// `--follow` for the service, off for `--check`, which reads once.
    follow: bool,
    child: Option<Child>,
    stdout: Option<ChildStdout>,
    /// The one line being assembled, bounded by a named limit.
    line: Bounded<u8>,
    /// A line past the limit is dropped, and its bytes are ignored until its
    /// newline arrives, so the next line starts clean.
    dropping: bool,
    /// When to start the child again, after it stopped.
    restart_at: Option<Instant>,
    backoff: Duration,
    /// Lines delivered, and lines dropped for their size.
    pub lines: u64,
    pub oversized: u64,
    /// How many times the child has been started, after the first.
    pub restarts: u64,
    /// Why the child is not running, when it is not.
    pub stopped: Option<String>,
}

impl Reader {
    /// Start reading `dir`. A child that cannot start is not fatal: the
    /// reason is kept, `unavailable` says it, and a backoff tries again.
    /// A reader for a journal directory that could not be opened: it starts
    /// nothing, and `stopped` says why.
    pub fn unavailable(reason: String, limit: &Limit) -> Reader {
        Reader {
            dir: PathBuf::new(),
            follow: true,
            child: None,
            stdout: None,
            line: Bounded::<u8>::new(limit),
            dropping: false,
            restart_at: None,
            backoff: Duration::from_secs(1),
            lines: 0,
            oversized: 0,
            restarts: 0,
            stopped: Some(reason),
        }
    }

    pub fn open(dir: &Path, limit: &Limit, now: Instant) -> Reader {
        let mut reader = Reader {
            dir: dir.to_path_buf(),
            follow: true,
            child: None,
            stdout: None,
            line: Bounded::<u8>::new(limit),
            dropping: false,
            restart_at: None,
            backoff: Duration::from_secs(1),
            lines: 0,
            oversized: 0,
            restarts: 0,
            stopped: None,
        };
        reader.start(now);
        reader
    }

    /// One run with no `--follow`, for `--check`: journalctl prints the recent
    /// tail and exits, and the pipe is read to its end, so a one-shot tool does
    /// not wait on a timer.
    pub fn once(dir: &Path, limit: &Limit) -> Reader {
        let mut reader = Reader {
            dir: dir.to_path_buf(),
            follow: false,
            child: None,
            stdout: None,
            line: Bounded::<u8>::new(limit),
            dropping: false,
            restart_at: None,
            backoff: Duration::from_secs(1),
            lines: 0,
            oversized: 0,
            restarts: 0,
            stopped: None,
        };
        reader.start(m6_core::platform::now());
        reader
    }

    /// Read a one-shot run to its end, handing every line to `each`.
    pub fn drain(&mut self, mut each: impl FnMut(&[u8])) {
        while self.stdout.is_some() {
            self.read(m6_core::platform::now(), &mut each);
        }
    }

    /// The descriptor to watch for readability, while there is one.
    pub fn fd(&self) -> Option<std::os::fd::RawFd> {
        use std::os::fd::AsRawFd;
        self.stdout.as_ref().map(AsRawFd::as_raw_fd)
    }

    /// Start the child again when its backoff has passed.
    pub fn restart_if_due(&mut self, now: Instant) {
        if self.child.is_none() && self.restart_at.is_some_and(|at| now >= at) {
            self.start(now);
        }
    }

    /// Read what the pipe holds, handing every complete line to `each`.
    ///
    /// [`Event::Exited`] means the child is gone and its descriptor must be
    /// removed from the reactor before [`Reader::retire`] closes it: a closed
    /// descriptor number can be given to the next socket, and deregistering it
    /// then would remove that socket instead.
    pub fn read(&mut self, now: Instant, mut each: impl FnMut(&[u8])) -> Event {
        let mut chunk = [0u8; 8 * 1024];
        loop {
            let Some(stdout) = self.stdout.as_mut() else {
                return Event::Idle;
            };
            let read = m6_core::platform::fd::read(stdout, &mut chunk);
            let count = match read {
                Ok(0) => {
                    self.stop("journalctl exited", now);
                    return Event::Exited;
                }
                Ok(count) => count,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return Event::Idle,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    self.stop(&format!("read error: {e}"), now);
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

    /// Close the dead child's descriptor, after the reactor has let it go.
    pub fn retire(&mut self) {
        self.stdout = None;
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
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

    /// The one `journalctl` command, with every type it follows on it.
    ///
    /// One child carries all four types. Two followers would mean two
    /// descriptors, two backoffs and two `--lines=200` tails to reconcile, so
    /// the two record types are two more matches on this command and nothing
    /// else.
    fn command(&self) -> Command {
        let mut command = Command::new("journalctl");
        if self.follow {
            command.arg("--follow");
        }
        command
            .arg("--lines=200")
            .arg("--output=json")
            .arg("--directory")
            .arg(&self.dir)
            .arg("M6_TYPE=perf")
            .arg("M6_TYPE=traffic")
            .arg("M6_TYPE=startup")
            .arg("M6_TYPE=shutdown")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        command
    }

    /// Start the child, and put it back on a longer backoff when it fails.
    fn start(&mut self, now: Instant) {
        let mut command = self.command();
        match m6_core::platform::process::spawn(&mut command) {
            Ok(mut child) => {
                let Some(stdout) = child.stdout.take() else {
                    let _ = child.kill();
                    let _ = child.wait();
                    self.stop("journalctl has no stdout", now);
                    return;
                };
                // The loop must never wait on the pipe, the same rule stdout
                // follows. A one-shot read wants the opposite: it reads to the
                // end and returns.
                let set = if self.follow {
                    m6_core::platform::fd::set_nonblocking(&stdout, true)
                } else {
                    Ok(false)
                };
                if let Err(e) = set {
                    let _ = child.kill();
                    let _ = child.wait();
                    self.stop(
                        &format!("the pipe could not be made non-blocking: {e}"),
                        now,
                    );
                    return;
                }
                self.stdout = Some(stdout);
                self.child = Some(child);
                self.restart_at = None;
                self.stopped = None;
            }
            Err(e) => self.stop(&format!("journalctl could not start: {e}"), now),
        }
    }

    /// Record why the reader stopped, kill what is left of the child, and
    /// schedule the next start on a doubling backoff.
    fn stop(&mut self, reason: &str, now: Instant) {
        self.stdout = None;
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        self.stopped = Some(reason.to_string());
        self.restart_at = Some(now + self.backoff);
        self.backoff = (self.backoff * 2).min(MAX_BACKOFF);
        self.restarts += 1;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn line(value: serde_json::Value) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "MESSAGE": value.to_string(),
            "M6_TYPE": value["type"],
            "_HOSTNAME": "build",
        }))
        .unwrap()
    }

    fn document(kind: &str, node: &str) -> serde_json::Value {
        match kind {
            "perf" => serde_json::json!({
                "type": "perf",
                "fields": {"perf": {"node": node, "uptime_s": 5}},
            }),
            _ => serde_json::json!({
                "type": "traffic",
                "fields": {"traffic": {"node": node, "window_minutes": 60}},
            }),
        }
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

    #[test]
    fn the_newest_document_wins() {
        let mut latest = Latest::new();
        let mut first = document("perf", "origin");
        first["fields"]["perf"]["uptime_s"] = serde_json::json!(5);
        let mut second = document("perf", "origin");
        second["fields"]["perf"]["uptime_s"] = serde_json::json!(9);
        latest.ingest(&line(first));
        latest.ingest(&line(second));
        assert_eq!(latest.perf("origin").expect("a document")["uptime_s"], 9);
        assert_eq!(latest.nodes_with_perf(), 1);
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

    /// The two record types are two more matches on the one child the two
    /// documents already use. A second follower would be a second process, a
    /// second tail and a second backoff, and this is the test that refuses it.
    #[test]
    fn the_one_follower_reads_all_four_line_types() {
        let reader = Reader::unavailable(
            "a test reader".into(),
            &Limit::builtin(&m6_core::limits::JOURNAL_LINE),
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
        assert_eq!(
            args.iter().filter(|a| *a == "--follow").count(),
            1,
            "one follower"
        );
        assert_eq!(
            args.iter().filter(|a| *a == "--directory").count(),
            1,
            "one journal directory"
        );
    }

    #[test]
    fn an_unreadable_entry_is_counted() {
        let mut latest = Latest::new();
        latest.ingest(b"{\"MESSAGE\":\"not json\"}");
        assert_eq!(latest.malformed, 1);
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
