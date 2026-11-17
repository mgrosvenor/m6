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
//! The child is restarted on a backoff when it stops, and it is killed at
//! stop, so no `journalctl --follow` outlives the monitor.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

use m6_core::limits::{BoundedBuf, Limit};

/// The longest the monitor waits before starting the reader again.
const MAX_BACKOFF: Duration = Duration::from_secs(30);

/// The newest document of each kind the journal has carried, by node name.
#[derive(Debug, Default)]
pub struct Latest {
    perf: BTreeMap<String, serde_json::Value>,
    traffic: BTreeMap<String, serde_json::Value>,
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
    line: BoundedBuf,
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
            line: BoundedBuf::new(limit),
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
            line: BoundedBuf::new(limit),
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
            line: BoundedBuf::new(limit),
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

    /// Start the child, and put it back on a longer backoff when it fails.
    fn start(&mut self, now: Instant) {
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
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
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

    #[test]
    fn an_unreadable_entry_is_counted() {
        let mut latest = Latest::new();
        latest.ingest(b"{\"MESSAGE\":\"not json\"}");
        assert_eq!(latest.malformed, 1);
    }
}
